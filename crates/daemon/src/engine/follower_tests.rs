//! The follower against simulated devices. The master is the real processor
//! with its tee, driven on a simulated DAC clock; the follower is the real
//! [`FollowerRt`] in the real [`step_device`] loop on a simulated DAC
//! ([`SimPcm`]). Both devices are `drift::sim::DeviceModel`s (the measured USB
//! model: microframe pointers, outliers, wakeup latency, timestamp
//! jitter).
//!
//! The truth: at each master wakeup, the master's exact position at its DAC
//! (its own published position at the exact frame) and the follower's (its
//! map at the exact frame it plays then).

use super::*;
use crate::engine::processor::{DaemonProcessor, MeterBank, PendingSwap, RtDspCmd};
use crate::engine::varispeed::VarispeedRtCmd;
use audivane_dsp_core::chain::{Chain, ChainSpec};
use audivane_dsp_core::drift::sim::{DeviceModel, Rng};
use audivane_io_pw::EngineAudioProcessor;
use audivane_protocol::ipc::SinkChoice;
use audivane_protocol::{ChainConfig, DitherKind, EqProfile, OutputConfig};
use std::sync::atomic::AtomicU8;

const CH: usize = 2;

fn eq_chain(rate: u32) -> ActiveChain {
    eq_chain_with(rate, -1.0, false)
}

/// The EQ chain at `preamp_db`, with a limiter (5 ms look-ahead: the
/// chain's latency) or without.
fn eq_chain_with(rate: u32, preamp_db: f64, limiter: bool) -> ActiveChain {
    use audivane_protocol::{LimiterConfig, LimiterMode};
    ActiveChain::Single(Box::new(Chain::new(ChainSpec {
        config: ChainConfig {
            eq: Some(EqProfile {
                name: "eq".into(),
                preamp_db,
                ..Default::default()
            }),
            limiter: LimiterConfig {
                mode: if limiter {
                    LimiterMode::Limit
                } else {
                    LimiterMode::Off
                },
                ..Default::default()
            },
            output: OutputConfig {
                bits: 32,
                dither: DitherKind::None,
                headroom_db: 0.0,
            },
            ..ChainConfig::neutral()
        },
        sample_rate: rate,
        channels: CH,
        max_block: 8192,
        irs: None,
    })))
}

/// The source: a 997 Hz tone at -6 dBFS (the right channel inverted).
fn tone(i: u64, rate: u32) -> f32 {
    (0.5 * (2.0 * PI * 997.0 * i as f64 / f64::from(rate)).sin()) as f32
}

/// The master: the processor with its tee on a simulated DAC.
struct Master {
    proc: DaemonProcessor,
    host: TeeHost,
    vs_tx: rtrb::Producer<VarispeedRtCmd>,
    dev: DeviceModel,
    rate: u32,
    rng: Rng,
    sink: SinkChoice,
    speed: f64,
    ramp_ms: f64,
    /// The driver's cap (the engine caps it while followed).
    limit: f64,
    /// Stage calls run (more than one a period where one is cut).
    calls: u64,
    written: u64,
    k: u64,
    next_wake: f64,
    src: u64,
    pos: u64,
    out: Vec<i32>,
    input: Vec<f32>,
    /// Every output sample (to compare runs), when kept.
    keep_output: bool,
    output: Vec<i32>,
    /// The source is silent over these spans of the master's wakeups
    /// (seconds), and the wakeup now.
    silent: Vec<(f64, f64)>,
    now: f64,
    /// One late wakeup: the first at or after `.0` comes `.1` seconds late
    /// (a stall of the master's thread; its queue covers it).
    stall: Option<(f64, f64)>,
    _keep: (
        rtrb::Producer<RtDspCmd>,
        rtrb::Producer<PendingSwap>,
        rtrb::Consumer<ActiveChain>,
        rtrb::Consumer<Box<Varispeed>>,
    ),
    /// Where its retired stretches go.
    _stretch_recycle: rtrb::Consumer<Box<audivane_dsp_core::tempo::TempoStretch>>,
    /// Its measurement slot.
    measure: Arc<crate::engine::measure::MeasureSlot>,
}

impl Master {
    fn new(dev: DeviceModel, seed: u64) -> Self {
        let rate = dev.rate as u32;
        let (cmd_tx, cmd_rx) = rtrb::RingBuffer::new(8);
        let (swap_tx, swap_rx) = rtrb::RingBuffer::<PendingSwap>::new(8);
        let (recycle_tx, recycle) = rtrb::RingBuffer::new(8);
        let (vs_tx, vs_rx) = rtrb::RingBuffer::new(8);
        let (vs_recycle_tx, vs_recycle) = rtrb::RingBuffer::new(8);
        let (stretch_recycle_tx, stretch_recycle) = rtrb::RingBuffer::new(8);
        let (host, rt) = TeeHost::pair(CH);
        let measure = Arc::new(crate::engine::measure::MeasureSlot::default());
        let proc = DaemonProcessor::new(
            eq_chain(rate),
            true,
            CH,
            8192,
            16,
            rate,
            Arc::new(AtomicU8::new(0)),
            MeterBank::new(CH),
            cmd_rx,
            swap_rx,
            recycle_tx,
        )
        .with_varispeed(vs_rx, vs_recycle_tx, CH)
        .with_stretch_recycle(stretch_recycle_tx)
        .with_tee(rt)
        .with_measure(Arc::clone(&measure), CH);
        Self {
            measure,
            proc,
            host,
            vs_tx,
            dev,
            rate,
            rng: Rng::new(seed),
            sink: SinkChoice::Dsp,
            speed: 1.0,
            ramp_ms: 200.0,
            limit: 2.0,
            calls: 0,
            written: 0,
            k: 0,
            next_wake: 0.0,
            src: 0,
            pos: 0,
            out: Vec::new(),
            input: Vec::new(),
            keep_output: false,
            output: Vec::new(),
            silent: Vec::new(),
            now: 0.0,
            stall: None,
            _keep: (cmd_tx, swap_tx, recycle, vs_recycle),
            _stretch_recycle: stretch_recycle,
        }
    }

    /// One period block, as the driver runs it (through the stage when one
    /// runs).
    /// The calls end where the stage asks (a fade's end, a scheduled
    /// command), as the driver's `run_period`; the commands go in after the
    /// period's last plan.
    fn block(&mut self) {
        let n = self.dev.period;
        let (sink, rate) = (self.sink, self.rate);
        self.proc.poll_updates(sink, rate, RateRatio::ONE, true);
        let mut off = 0;
        while off < n {
            let left = n - off;
            let mut k = left;
            let mut n_src = k;
            if let Some(port) = self.proc.varispeed(sink, RateRatio::ONE, rate, CH) {
                k = port
                    .frames_until_stop()
                    .map_or(left, |u| left.min(u.max(1)));
                n_src = port.plan(k);
                port.set_fill(n_src, false);
                if off + k >= n {
                    port.set_speed_limit(self.limit);
                    port.set_speed(self.speed, self.ramp_ms);
                    let _ = port.peak_until_settled();
                    port.pulled();
                }
            }
            self.input.clear();
            let now = self.now;
            let quiet = self.silent.iter().any(|&(a, b)| now >= a && now < b);
            for i in 0..n_src as u64 {
                let x = if quiet { 0.0 } else { tone(self.src + i, rate) };
                self.input.extend([x, -x]);
            }
            self.out.clear();
            self.out.resize(k * CH, 0);
            self.proc.process_block(
                sink,
                &self.input,
                &self.input,
                &mut self.out,
                CH,
                RateRatio::ONE,
                rate,
                self.pos,
            );
            if self.keep_output {
                self.output.extend_from_slice(&self.out);
            }
            self.src += n_src as u64;
            self.pos += k as u64;
            off += k;
            self.calls += 1;
        }
        self.written += n as u64;
    }

    fn start(&mut self, t: f64) {
        for _ in 0..self.dev.buffer_periods {
            self.block();
        }
        self.dev.start_s = t;
        self.k = 1;
        self.next_wake = self.dev.wake_after(self.dev.period as f64, &mut self.rng);
    }

    /// A pause, as the driver takes one: the stream stops (nothing
    /// streams: the tee's map restarts), the DSP state resets.
    fn pause(&mut self) {
        let (sink, rate) = (self.sink, self.rate);
        self.proc.poll_updates(sink, rate, RateRatio::ONE, false);
        self.proc.reset_dsp_state();
        self.next_wake = f64::INFINITY;
    }

    /// Playback resumes at `t`: a new stream, primed and started.
    fn resume(&mut self, t: f64) {
        self.proc.stream_prepared();
        self.written = 0;
        self.start(t);
    }

    /// The wakeup at `next_wake`: the exact position (the truth), then the
    /// reading followers see, then a block.
    fn wake(&mut self) -> Option<(f64, f64)> {
        let t = self.next_wake;
        let exact = self.dev.frame_at(t);
        let f = exact.floor();
        let slot = Arc::clone(&self.host.shared().position);
        self.proc
            .dac_read(self.written as i64 - f as i64, ns(t) as u64);
        let truth = slot
            .read()
            .map(|p| (t, p.pos as f64 / FRAME as f64 + (exact - f) * p.speed));
        let ptr = self.dev.pointer_at(t);
        let delay = self.written as i64 - ptr as i64 + self.dev.outlier(&mut self.rng);
        let jitter = self.dev.timestamp_jitter_s * self.rng.gauss();
        self.proc.dac_read(delay, ns(t + jitter) as u64);
        self.now = t;
        self.block();
        self.k += 1;
        self.next_wake = self
            .dev
            .wake_after((self.k * self.dev.period as u64) as f64, &mut self.rng)
            .max(t);
        if let Some((at, late)) = self.stall
            && self.next_wake >= at
        {
            self.stall = None;
            self.next_wake += late;
        }
        truth
    }
}

/// Seconds to the readings' nanoseconds (offset, so they stay positive).
fn ns(t: f64) -> i64 {
    ((t + 10.0) * 1e9).round() as i64
}

/// The follower's simulated DAC, and the world it advances: the master's
/// wakeups that come before each of its own.
struct SimPcm {
    master: Master,
    dev: DeviceModel,
    rng: Rng,
    period: usize,
    buffer: usize,
    /// Frames written, and the device frame the current run started at.
    written: u64,
    base: u64,
    running: bool,
    k: u64,
    next_wake: f64,
    now: f64,
    end_s: f64,
    /// The master's truth at its wakeups since the last step: `(t, master
    /// position, the follower's exact frame then)`.
    samples: Vec<(f64, f64, f64)>,
    /// Events: playback jumps by frames (lost frames, unannounced), a
    /// recovered xrun, the device gone over a window.
    jump: Option<(f64, i64)>,
    xrun: Option<f64>,
    /// The xrun is recovered inside the room query (the device prepared,
    /// with room), not at the wakeup.
    xrun_in_avail: bool,
    restart: bool,
    gone: Option<(f64, f64)>,
    /// The master's pauses `(from, to)`, in order, and whether it is in one.
    pauses: Vec<(f64, f64)>,
    paused: bool,
    /// What was written: (first device frame, samples), when kept.
    keep: bool,
    written_audio: Vec<i32>,
}

impl SimPcm {
    fn new(master: Master, dev: DeviceModel, seed: u64, end_s: f64) -> Self {
        Self {
            master,
            period: dev.period,
            buffer: dev.period * dev.buffer_periods,
            dev,
            rng: Rng::new(seed),
            written: 0,
            base: 0,
            running: false,
            k: 0,
            next_wake: 0.0,
            now: 0.0,
            end_s,
            samples: Vec::new(),
            jump: None,
            xrun: None,
            xrun_in_avail: false,
            restart: false,
            gone: None,
            pauses: Vec::new(),
            paused: false,
            keep: false,
            written_audio: Vec::new(),
        }
    }

    fn played(&self, t: f64) -> u64 {
        if !self.running {
            return self.base;
        }
        self.base + self.dev.frame_at(t).max(0.0).floor() as u64
    }

    fn is_gone(&self) -> bool {
        self.gone
            .is_some_and(|(a, b)| self.now >= a && self.now < b)
    }

    /// Run the master's wakeups up to `t`.
    fn advance_to(&mut self, t: f64) {
        loop {
            if let Some(&(a, b)) = self.pauses.first() {
                if !self.paused && self.master.next_wake >= a && a <= t {
                    self.master.pause();
                    self.paused = true;
                }
                if self.paused && b <= t {
                    self.master.resume(b);
                    self.paused = false;
                    self.pauses.remove(0);
                }
            }
            if self.master.next_wake > t {
                break;
            }
            let tm = self.master.next_wake;
            if let Some((tt, pm)) = self.master.wake() {
                let ff = if self.running && tm >= self.dev.start_s {
                    self.base as f64 + self.dev.frame_at(tm)
                } else {
                    f64::NAN
                };
                self.samples.push((tt, pm, ff));
            }
        }
        self.now = t;
    }

    /// The xrun, when it is due: an underrun the driver recovered (the
    /// buffer dropped, the device prepared, waiting for a prime).
    fn take_xrun(&mut self) {
        if let Some(at) = self.xrun
            && self.now >= at
        {
            self.xrun = None;
            self.restart = true;
            self.base = self.written;
            self.running = false;
        }
    }

    /// A fresh device (a reopen): nothing written, stopped.
    fn reopen(&mut self) {
        self.written = 0;
        self.base = 0;
        self.running = false;
    }
}

impl FollowerPcm for SimPcm {
    fn status(&mut self) -> Result<(i64, i64), PcmFault> {
        if self.is_gone() {
            return Err(PcmFault::Lost(libc::ENODEV));
        }
        let ptr = self.base + self.dev.pointer_at(self.now);
        let delay = self.written as i64 - ptr as i64 + self.dev.outlier(&mut self.rng);
        let jitter = self.dev.timestamp_jitter_s * self.rng.gauss();
        Ok((delay, ns(self.now + jitter)))
    }

    fn avail(&mut self) -> Result<usize, PcmFault> {
        if self.is_gone() {
            return Err(PcmFault::Lost(libc::ENODEV));
        }
        if self.xrun_in_avail {
            self.take_xrun();
        }
        let queued = self.written.saturating_sub(self.played(self.now)) as usize;
        Ok(self.buffer.saturating_sub(queued))
    }

    fn write(&mut self, samples: &[i32]) -> Result<usize, PcmFault> {
        if self.is_gone() {
            return Err(PcmFault::Lost(libc::ENODEV));
        }
        let n = (samples.len() / CH).min(self.avail()?);
        if self.keep {
            self.written_audio.extend_from_slice(&samples[..n * CH]);
        }
        self.written += n as u64;
        Ok(n)
    }

    fn start(&mut self) -> Result<(), PcmFault> {
        self.restart = false;
        self.running = true;
        self.dev.start_s = self.now;
        self.k = 1;
        self.next_wake = self.dev.wake_after(self.period as f64, &mut self.rng);
        Ok(())
    }

    fn wait(&mut self, _timeout_ms: u32) -> Result<bool, PcmFault> {
        let t = self.next_wake;
        self.advance_to(t);
        if self.is_gone() {
            return Err(PcmFault::Lost(libc::ENODEV));
        }
        if let Some((at, frames)) = self.jump
            && t >= at
        {
            // The device lost frames: it plays further on, unannounced.
            self.jump = None;
            self.dev.start_s -= frames as f64 / self.dev.rate;
        }
        if !self.xrun_in_avail {
            self.take_xrun();
        }
        self.k += 1;
        self.next_wake = self
            .dev
            .wake_after((self.k * self.period as u64) as f64, &mut self.rng)
            .max(t);
        Ok(true)
    }

    fn needs_restart(&self) -> bool {
        self.restart
    }

    fn discard(&mut self) -> Result<(), PcmFault> {
        // What was written and not played goes.
        self.base = self.written;
        self.running = false;
        Ok(())
    }

    fn interrupted(&self) -> bool {
        self.now >= self.end_s
    }
}

/// A follower rig: the world, the follower and its controls.
struct Rig {
    pcm: SimPcm,
    rt: Option<FollowerRt>,
    /// The control side's ends of the follower's chain queues.
    swaps: ChainSwapHost,
    /// The chain's gain the output check expects, dB.
    gain_db: f64,
    controls: Arc<FollowerControls>,
    status: Arc<FollowerStatus>,
    /// `(t, error frames, audible)` at each master wakeup the follower
    /// could be placed at.
    errors: Vec<(f64, f64, bool)>,
    /// The master's alignment, and whether its controller runs.
    align: AlignControl,
    align_auto: bool,
    /// The follower's lateness at the alignment cap (with `align_auto`).
    lag: LagControl,
    next_trace: f64,
    /// Its lateness (what `lag` set), ms.
    lag_ms: f64,
    /// The output against the map (−1 dB of EQ preamp on the tone): frames
    /// checked so far, and the error and signal energy from `audio_from_s`.
    audio_checked: usize,
    audio_from_s: f64,
    audio_err: f64,
    audio_sig: f64,
    /// How late a convolution's peak puts the tone, frames (0 without).
    audio_delay: f64,
    /// The largest step between two output samples (a click's mark), and
    /// the last sample.
    max_step: f64,
    max_step_at: f64,
    /// When the follower went silent or audible.
    audible_changes: Vec<(f64, bool)>,
    last_sample: f64,
    /// Device frames written before the current device run (a reopen
    /// restarts the count).
    audio_base: usize,
    /// The master's chain compensation, run as the engine's tick does
    /// (with `controls.compensate()`).
    comp: CompControl,
    /// The alignment's shrinks that landed: `(t, ms)`.
    shrinks: Vec<(f64, f64)>,
    /// With `pcm.keep`: `(t, error frames, heard)` at each master wakeup,
    /// heard from what its DAC plays then (above −60 dBFS).
    at_dac: Vec<(f64, f64, bool)>,
}

fn params_for(dev: &DeviceModel, graph_rate: u32) -> FollowerParams {
    FollowerParams {
        graph_rate,
        channels: CH,
        device_rate: dev.rate as u32,
        period: dev.period,
        buffer: dev.period * dev.buffer_periods,
        ratio: RateRatio::ONE,
    }
}

impl Rig {
    fn new(master_dev: DeviceModel, follower_dev: DeviceModel, end_s: f64) -> Self {
        Self::build(master_dev, follower_dev, end_s, true)
    }

    /// `master_first`: the master starts at 0 and the follower 0.137 s
    /// later; otherwise the master waits for [`Master::start`].
    fn build(
        master_dev: DeviceModel,
        follower_dev: DeviceModel,
        end_s: f64,
        master_first: bool,
    ) -> Self {
        let mut master = Master::new(master_dev, 11);
        let rate = master.rate;
        master
            .host
            .add_follower("hw:CARD=USB,DEV=0", &EngineConfig::default())
            .unwrap();
        let reader = master
            .host
            .slots_mut()
            .get_mut("hw:CARD=USB,DEV=0")
            .unwrap()
            .reader
            .take()
            .unwrap();
        let shared = master.host.shared().clone();
        // The tee's commands land at the first block; the master starts.
        if master_first {
            master.start(0.0);
        } else {
            master.next_wake = f64::INFINITY;
        }
        let status = Arc::new(FollowerStatus::default());
        let controls = Arc::new(FollowerControls::new(0.0, 0.0));
        let p = params_for(&follower_dev, rate);
        let mut rt = FollowerRt::new(
            p,
            reader,
            shared,
            eq_chain(rate),
            Arc::clone(&status),
            Arc::clone(&controls),
        )
        .unwrap();
        let (swaps, rt_swaps) = chain_swaps();
        rt.set_swaps(Some(rt_swaps));
        let mut pcm = SimPcm::new(master, follower_dev, 23, end_s);
        let rt = Some(rt);
        // The follower starts a little after the master.
        if master_first {
            pcm.advance_to(0.137);
        }
        Self {
            pcm,
            rt,
            swaps,
            gain_db: -1.0,
            controls,
            status,
            errors: Vec::new(),
            align: AlignControl::default(),
            align_auto: true,
            lag: LagControl::default(),
            next_trace: 1.0,
            lag_ms: 0.0,
            audio_checked: 0,
            audio_from_s: f64::INFINITY,
            audio_err: 0.0,
            audio_sig: 0.0,
            audio_delay: 0.0,
            max_step: 0.0,
            max_step_at: 0.0,
            audible_changes: Vec::new(),
            last_sample: 0.0,
            audio_base: 0,
            comp: CompControl::default(),
            shrinks: Vec::new(),
            at_dac: Vec::new(),
        }
    }

    fn rt(&self) -> &FollowerRt {
        self.rt.as_ref().unwrap()
    }

    fn set_alignment(&mut self, ms: f64) {
        self.align.ms = self.pcm.master.host.set_alignment_ms(ms).unwrap();
    }

    /// The follower's exact position at device frame `f` (fractional).
    fn follower_at(&self, f: f64) -> Option<f64> {
        let fl = f.floor();
        let a = self.rt().source_at(fl as u64)?;
        let b = self.rt().source_at(fl as u64 + 1)?;
        Some((a as f64 + (b - a) as f64 * (f - fl)) / FRAME as f64)
    }

    fn check(&mut self) {
        let samples = std::mem::take(&mut self.pcm.samples);
        let audible = self.status.audible.load(Ordering::Relaxed);
        if self.audible_changes.last().map(|c| c.1) != Some(audible) {
            self.audible_changes.push((self.pcm.now, audible));
        }
        // The lateness is playback time: stream frames at the speed the
        // master publishes (1 under Keep pitch), as the follower counts it.
        let speed = self
            .pcm
            .master
            .host
            .shared()
            .position
            .read()
            .map_or(1.0, |p| p.speed);
        // The user's trim and the unaligned lag, both playback time.
        let late_ms = self.lag_ms + self.controls.trim_ms();
        let trim = late_ms * 1e-3 * f64::from(self.pcm.master.rate) * speed;
        for (t, pm, ff) in samples {
            if ff.is_nan() {
                continue;
            }
            if let Some(pf) = self.follower_at(ff) {
                self.errors.push((t, pm - pf - trim, audible));
                if self.pcm.keep {
                    // Heard at its DAC then: what was written for the
                    // frames playing (the flag is the render's).
                    let f = ff.floor() as usize + self.audio_base;
                    let a = &self.pcm.written_audio;
                    let lo = f.saturating_sub(2) * CH;
                    let hi = ((f + 3) * CH).min(a.len());
                    let heard = lo < hi
                        && a[lo..hi]
                            .iter()
                            .any(|&x| f64::from(x).abs() > 1e-3 * 2_147_483_648.0);
                    self.at_dac.push((t, pm - pf - trim, heard));
                }
            }
        }
        if self.pcm.keep {
            let frames = self.pcm.written_audio.len() / CH - self.audio_base;
            let g = 10f64.powf(self.gain_db / 20.0);
            let rate = f64::from(self.pcm.master.rate);
            for f in self.audio_checked..frames {
                let x =
                    f64::from(self.pcm.written_audio[(self.audio_base + f) * CH]) / 2_147_483_648.0;
                if self.last_sample.is_finite() && (x - self.last_sample).abs() > self.max_step {
                    self.max_step = (x - self.last_sample).abs();
                    self.max_step_at = self.pcm.now;
                }
                self.last_sample = x;
                let Some(p) = self.rt().source_at(f as u64) else {
                    continue;
                };
                if self.pcm.now < self.audio_from_s || !audible {
                    continue;
                }
                let p = p as f64 / FRAME as f64 - self.audio_delay;
                let want = g * 0.5 * (2.0 * PI * 997.0 * p / rate).sin();
                let got = x;
                self.audio_err += (got - want).powi(2);
                self.audio_sig += want * want;
            }
            self.audio_checked = frames;
        }
    }

    /// Run until `end_s` (or the run ends): returns how it ended.
    fn run_until(&mut self, end_s: f64) -> RunExit {
        self.pcm.end_s = end_s;
        loop {
            let rt = self.rt.as_mut().unwrap();
            if let Some(x) = step_device(&mut self.pcm, rt, &self.controls) {
                self.check();
                return x;
            }
            self.check();
            if self.pcm.now >= self.next_trace {
                self.next_trace += 1.0;
                self.trace();
            }
            if let Some(ms) = lead_needed_ms(&self.status.snapshot()) {
                self.pcm.master.host.set_command_lead_ms(ms);
            }
            if self.controls.compensate() {
                let shared = self.pcm.master.host.shared().clone();
                let master = shared.master_chain_graph.get();
                let sn = self.status.snapshot();
                let need = comp_need_of([(sn.state, sn.chain_graph)], master);
                let cap = crate::engine::tee::max_comp_frames(self.pcm.master.rate);
                if let Some(frames) = self.comp.tick(self.pcm.now, need, cap) {
                    self.pcm.master.host.set_comp_frames(frames).unwrap();
                }
            }
            if self.align_auto {
                let now = self.pcm.now;
                let snap = self.status.snapshot();
                self.floor_alignment();
                if let Some(ms) = self.align.tick(now, snap.slack_ms) {
                    self.set_alignment(ms);
                }
                // The engine's shrink, as `tick_followers` runs it.
                let applied = self.pcm.master.host.applied_alignment_ms();
                if self.align.landed(now, applied) {
                    self.shrinks.push((now, applied));
                }
                self.align.note_need(now, need_of([snap], self.align.ms));
                match self.align.shrink(now, self.controls.lag_ms() == 0.0) {
                    Some((ms, true)) => self.pcm.master.host.shrink_alignment_ms(ms).unwrap(),
                    Some((ms, false)) => self.set_alignment(ms),
                    None => {}
                }
                let at_cap = self.align.settled_at_cap(self.pcm.now);
                if let Some(ms) = self.lag.tick(
                    self.pcm.now,
                    &self.status.snapshot(),
                    &self.controls,
                    at_cap,
                ) {
                    self.lag_ms = ms;
                }
            }
        }
    }

    /// `FOLLOWER_TRACE=1`: the state once a second.
    fn trace(&self) {
        if std::env::var_os("FOLLOWER_TRACE").is_none() {
            return;
        }
        let sn = self.status.snapshot();
        let last = self.errors.last().map_or(f64::NAN, |e| e.1);
        println!(
            "t {:.1} truth {last:.2} filtered {:.2} bw {:.3} ppm {:.2} skips {} audible {} {:?} \
             align {} ms lag {} ms slack {:.1} ms underfeeds {} latency {:.1} ms",
            self.pcm.now,
            sn.phase_error_frames,
            sn.bandwidth_hz,
            sn.ppm,
            sn.skips,
            sn.audible,
            sn.state,
            self.align.ms,
            self.lag_ms,
            sn.slack_ms,
            sn.underfeeds,
            sn.latency_ms,
        );
    }

    fn start(&mut self) {
        if self.align_auto {
            self.floor_alignment();
        }
        prime_device(&mut self.pcm, self.rt.as_mut().unwrap()).unwrap();
    }

    /// The engine's floor: what the pair is expected to need, until it is
    /// measured (the sim's master writes at its wakeup: its lead is its
    /// queue less a period).
    fn floor_alignment(&mut self) {
        let (m, f) = (self.pcm.master.dev, self.pcm.dev);
        let lead = (m.buffer_periods as f64 - 1.0) * m.period as f64 * 1e3 / m.rate;
        let period_ms = f.period as f64 * 1e3 / f.rate;
        let expected = expected_need_ms(period_ms, f.buffer_periods as u32, lead);
        if unplaced(&self.status.snapshot())
            && let Some(lag) = floor_lag_ms(expected)
            && lag > self.controls.lag_ms()
        {
            self.controls.set_lag_ms(lag);
            self.lag_ms = lag;
        }
        if let Some(ms) = self.align.floor(self.pcm.now, Some(expected)) {
            self.set_alignment(ms);
        }
    }

    /// The thread's way back from a loss: the world runs on until
    /// `back_s`, then the device reopens and a fresh follower (the same
    /// ring) primes it.
    fn reopen_at(&mut self, back_s: f64) {
        while self.pcm.now < back_s {
            let t = (self.pcm.now + 0.01).min(back_s);
            self.pcm.running = false;
            self.pcm.advance_to(t);
            self.pcm.samples.clear();
        }
        self.audio_base = self.pcm.written_audio.len() / CH;
        self.audio_checked = 0;
        // A new stream: no step across the reopen.
        self.last_sample = f64::NAN;
        self.pcm.reopen();
        let old = self.rt.take().unwrap();
        let p = old.params();
        let shared = old.shared.clone();
        let rate = p.graph_rate;
        let (reader, swaps) = old.into_parts();
        let mut rt = FollowerRt::new(
            p,
            reader,
            shared,
            eq_chain(rate),
            Arc::clone(&self.status),
            Arc::clone(&self.controls),
        )
        .unwrap();
        rt.set_swaps(swaps);
        self.rt = Some(rt);
        // A new stream starts aligned.
        self.lag_ms = self.controls.lag_ms();
        self.start();
    }
}

/// What a run measured.
#[derive(Debug)]
struct Outcome {
    /// From here on the error stays within 1 frame (s).
    sync_s: f64,
    /// The follower became audible (s; infinity: never).
    audible_s: f64,
    steady_peak: f64,
    steady_rms: f64,
    bias: f64,
    skips: u64,
}

fn outcome(rig: &Rig, from_s: f64, steady_s: f64) -> Outcome {
    let e: Vec<&(f64, f64, bool)> = rig.errors.iter().filter(|x| x.0 >= from_s).collect();
    let sync_s = e
        .iter()
        .rposition(|x| x.1.abs() >= 1.0)
        .map_or(from_s, |i| e.get(i + 1).map_or(f64::INFINITY, |x| x.0));
    let audible_s = e.iter().find(|x| x.2).map_or(f64::INFINITY, |x| x.0);
    let steady: Vec<f64> = e.iter().filter(|x| x.0 >= steady_s).map(|x| x.1).collect();
    let n = steady.len().max(1) as f64;
    let bias = steady.iter().sum::<f64>() / n;
    let steady_peak = steady.iter().fold(0.0f64, |a, x| a.max(x.abs()));
    let steady_rms = (steady.iter().map(|x| x * x).sum::<f64>() / n).sqrt();
    Outcome {
        sync_s,
        audible_s,
        steady_peak,
        steady_rms,
        bias,
        skips: rig.status.snapshot().skips,
    }
}

fn follower_usb(ppm: f64, period: usize, buffer_periods: usize) -> DeviceModel {
    let mut d = DeviceModel::usb(ppm);
    d.period = period;
    d.buffer_periods = buffer_periods;
    d.bus_phase = 0.71;
    d
}

/// The base scenario: two USB DACs at 48 kHz (the measured model), the
/// master at 1024-frame periods with two queued, the follower +60 ppm at
/// 512-frame periods with three queued; the master's alignment set by the
/// controller from the follower's slack. The follower locks, fades in and
/// holds the master within a frame, and its output is the source at the
/// positions its map gives (the audio is where the loop thinks it is).
#[test]
fn a_follower_locks_fades_in_and_holds_the_master() {
    let secs = 90.0;
    let mut rig = Rig::new(DeviceModel::usb(0.0), follower_usb(60.0, 512, 3), secs);
    rig.pcm.keep = true;
    rig.audio_from_s = 60.0;
    rig.start();
    assert_eq!(rig.run_until(secs), RunExit::Stopped);
    let o = outcome(&rig, 0.0, 45.0);
    let snap = rig.status.snapshot();
    println!(
        "48k usb pair: {o:?}, alignment {} ms, ppm {:.2}, gated {}, underfeeds {}, latency {:.1} ms, slack {:.1} ms",
        rig.align.ms, snap.ppm, snap.gated, snap.underfeeds, snap.latency_ms, snap.slack_ms
    );
    assert!(o.sync_s < 30.0, "{o:?}");
    assert!(o.audible_s < 30.0, "{o:?}");
    assert!(o.steady_peak < 1.0 && o.steady_rms < 0.5, "{o:?}");
    assert!(o.bias.abs() < 0.5 && o.skips <= 3, "{o:?}");
    assert!(
        snap.state == FollowerState::Locked && snap.audible,
        "{snap:?}"
    );
    assert!((snap.ppm - 60.0).abs() < 2.0, "{}", snap.ppm);
    assert!(rig.align.ms > 0.0 && rig.align.ms <= super::super::tee::MAX_ALIGN_MS);
    let (err, sig) = (rig.audio_err, rig.audio_sig);
    let rel = (err / sig.max(1e-30)).sqrt();
    println!("output against the map: {:.1} dB", 20.0 * rel.log10());
    assert!(sig > 0.0, "no samples checked");
    assert!(rel < 1e-3, "output off its map: {rel:e}");
}

/// The follower starts before the master has played anything, as the
/// engine starts them: its stage runs on silence until the tee's first
/// frames. Its map still places its output, and its path latency is the
/// buffer and the stage (on the rig the map once sat 6 periods ahead).
#[test]
fn a_follower_started_before_the_master_plays_where_its_map_says() {
    let secs = 60.0;
    let mut rig = Rig::build(
        DeviceModel::usb(0.0),
        follower_usb(60.0, 512, 3),
        secs,
        false,
    );
    rig.pcm.keep = true;
    rig.audio_from_s = 40.0;
    // The engine has opened the master (its rate is known), which has
    // played nothing yet.
    let rate = rig.pcm.master.rate;
    rig.pcm
        .master
        .host
        .shared()
        .graph_rate
        .store(rate, Ordering::Relaxed);
    rig.start();
    while rig.pcm.now < 0.2 {
        let rt = rig.rt.as_mut().unwrap();
        assert_eq!(step_device(&mut rig.pcm, rt, &rig.controls), None);
    }
    let now = rig.pcm.now;
    rig.pcm.master.start(now);
    assert_eq!(rig.run_until(secs), RunExit::Stopped);
    let snap = rig.status.snapshot();
    let o = outcome(&rig, 0.0, 30.0);
    let rel = (rig.audio_err / rig.audio_sig.max(1e-30)).sqrt();
    println!(
        "follower first: {o:?}, latency {:.1} ms, output against the map {:.1} dB",
        snap.latency_ms,
        20.0 * rel.log10()
    );
    assert!(
        snap.state == FollowerState::Locked && snap.audible,
        "{snap:?}"
    );
    let buffer_ms = 3.0 * 512.0 / 48.0;
    assert!(
        snap.latency_ms > buffer_ms * 0.5 && snap.latency_ms < buffer_ms + 20.0,
        "{}",
        snap.latency_ms
    );
    assert!(rig.audio_sig > 0.0, "no samples checked");
    assert!(rel < 1e-3, "output off its map: {rel:e}");
}

/// Back in sync (within a frame for the rest of the window) after `from_s`,
/// seconds after it.
fn resync_after(rig: &Rig, from_s: f64, to_s: f64) -> f64 {
    let e: Vec<&(f64, f64, bool)> = rig
        .errors
        .iter()
        .filter(|x| x.0 >= from_s && x.0 < to_s)
        .collect();
    match e.iter().rposition(|x| x.1.abs() >= 1.0) {
        None => 0.0,
        Some(i) => e.get(i + 1).map_or(f64::INFINITY, |x| x.0 - from_s),
    }
}

/// The longest the follower was silent after `from_s`, seconds (infinity:
/// it never came back). A known step mutes it for its fade only: out, the
/// skip, back in ([`FADE_MS`] each way, seen at its wakeups).
fn longest_mute(rig: &Rig, from_s: f64) -> f64 {
    let c = &rig.audible_changes;
    let mut longest = 0.0f64;
    for (i, x) in c.iter().enumerate() {
        if x.0 < from_s || x.1 {
            continue;
        }
        let back = c[i + 1..]
            .iter()
            .find(|y| y.1)
            .map_or(f64::INFINITY, |y| y.0);
        longest = longest.max(back - x.0);
    }
    longest
}

/// The most a known step may mute a 512-frame follower (its fade, at its
/// wakeups).
const STEP_MUTE_S: f64 = 0.025;

/// Heard again after `from_s` (silent at some point after it, then
/// audible), seconds after it; 0 when it never went silent.
fn audible_after(rig: &Rig, from_s: f64) -> f64 {
    let c = &rig.audible_changes;
    let Some(q) = c.iter().position(|x| x.0 >= from_s && !x.1) else {
        return 0.0;
    };
    c[q..]
        .iter()
        .find(|x| x.1)
        .map_or(f64::INFINITY, |x| x.0 - from_s)
}

/// The master's output with a follower that loses frames (an unannounced
/// jump), underruns (a recovered xrun) and is unplugged for three seconds,
/// against the same run without the events: bit for bit the same (the
/// master never waits on a follower, its ring overflows meanwhile). The
/// follower is back in sync and heard after each, and never clicks.
#[test]
fn a_follower_xrun_or_unplug_never_disturbs_the_master() {
    let secs = 110.0;
    let run = |events: bool| {
        let mut rig = Rig::new(DeviceModel::usb(0.0), follower_usb(-40.0, 512, 3), secs);
        rig.pcm.master.keep_output = true;
        rig.pcm.keep = true;
        rig.align_auto = false;
        rig.set_alignment(30.0);
        if events {
            rig.pcm.jump = Some((30.0, 1024));
            rig.pcm.xrun = Some((50.0, 0.0).0);
            rig.pcm.gone = Some((70.0, 73.0));
        }
        rig.start();
        loop {
            match rig.run_until(secs) {
                RunExit::Stopped => break,
                RunExit::Lost(_) => {
                    let back = rig.pcm.gone.map_or(rig.pcm.now, |g| g.1);
                    rig.reopen_at(back);
                }
                RunExit::Rebuild => panic!("rebuild"),
            }
        }
        rig
    };
    let clean = run(false);
    let hit = run(true);
    assert_eq!(clean.pcm.master.output.len(), hit.pcm.master.output.len());
    assert!(
        clean.pcm.master.output == hit.pcm.master.output,
        "the master's output changed"
    );
    let dropped = hit
        .pcm
        .master
        .host
        .slots()
        .get("hw:CARD=USB,DEV=0")
        .map(|_| ());
    assert!(dropped.is_some());
    let snap = hit.status.snapshot();
    for (name, at, next) in [
        ("jump", 30.0, 50.0),
        ("xrun", 50.0, 70.0),
        ("unplug", 70.0, secs),
    ] {
        let sync = resync_after(&hit, at, next);
        let heard = audible_after(&hit, at);
        // An unannounced jump plays out of place until the loop's next
        // readings see it; a signalled xrun or a loss shuts the follower at
        // once, so nothing out of place is heard from the event itself.
        let from = if name == "jump" { at + 0.1 } else { at };
        let peak = audible_peak(&hit, from, next);
        println!(
            "{name}: back in sync after {sync:.1} s, heard after {heard:.1} s, heard at most {peak:.2} frames off"
        );
        assert!(peak < 0.05e-3 * 48_000.0, "{name}: heard {peak} frames off");
        assert!(sync < 15.0, "{name}: {sync}");
        assert!(heard < 20.0, "{name}: {heard}");
    }
    println!(
        "events: skips {}, xruns {}, max step {:.3} at {:.2} s (clean {:.3})",
        snap.skips, snap.xruns, hit.max_step, hit.max_step_at, clean.max_step
    );
    assert_eq!(snap.xruns, 1);
    assert!(
        snap.state == FollowerState::Locked && snap.audible,
        "{snap:?}"
    );
    // A 997 Hz tone at −7 dB moves at most 0.056 a sample: nothing cut.
    assert!(hit.max_step < 0.07, "a click: {}", hit.max_step);
}

/// Direct takes the DAC for four seconds: the follower fades out (its
/// output goes to digital silence, no click) and fades back in once the
/// master plays the DSP path again.
#[test]
fn a_follower_mutes_while_the_master_plays_direct() {
    let secs = 70.0;
    let mut rig = Rig::new(DeviceModel::usb(0.0), follower_usb(25.0, 512, 3), secs);
    rig.pcm.keep = true;
    rig.align_auto = false;
    rig.set_alignment(30.0);
    rig.start();
    assert_eq!(rig.run_until(30.0), RunExit::Stopped);
    assert!(rig.status.snapshot().audible);
    rig.pcm.master.sink = SinkChoice::Direct;
    let quiet_from = rig.pcm.written_audio.len() / CH;
    assert_eq!(rig.run_until(34.0), RunExit::Stopped);
    let quiet_to = rig.pcm.written_audio.len() / CH;
    assert!(!rig.status.snapshot().audible);
    rig.pcm.master.sink = SinkChoice::Dsp;
    assert_eq!(rig.run_until(secs), RunExit::Stopped);
    // Silent from 0.2 s into Direct to its end.
    let a = quiet_from + 9600;
    let peak = rig.pcm.written_audio[a * CH..quiet_to * CH]
        .iter()
        .fold(0i32, |m, &x| m.max(x.saturating_abs()));
    assert_eq!(peak, 0, "heard during Direct");
    let back = audible_after(&rig, 30.0) - 4.0;
    println!(
        "direct: heard again {back:.2} s after, max step {:.3}",
        rig.max_step
    );
    assert!(back < 15.0, "{back}");
    assert!(rig.status.snapshot().audible);
    assert!(rig.max_step < 0.07, "a click: {}", rig.max_step);
}

/// A stage for the master at 48 kHz.
fn master_stage() -> VarispeedRtCmd {
    VarispeedRtCmd::Install(Box::new(
        Varispeed::new(VarispeedSpec {
            sample_rate: 48_000,
            channels: CH,
            max_frames: 8192,
            speed: 1.0,
        })
        .unwrap(),
    ))
}

/// The master plays varispeed at 1.25 (the follower joins from the
/// master's state) and stays in sync there.
#[test]
fn a_follower_follows_the_master_speed() {
    let secs = 60.0;
    let mut rig = Rig::new(DeviceModel::usb(0.0), follower_usb(30.0, 512, 3), secs);
    rig.pcm.master.speed = 1.25;
    rig.pcm.master.vs_tx.push(master_stage()).ok().unwrap();
    rig.align_auto = false;
    rig.set_alignment(30.0);
    rig.start();
    assert_eq!(rig.run_until(secs), RunExit::Stopped);
    let o = outcome(&rig, 0.0, 40.0);
    let snap = rig.status.snapshot();
    println!("at 1.25: {o:?}, lead {:.1} ms", snap.lead_ms);
    assert!(o.sync_s < 30.0 && o.steady_peak < 1.0, "{o:?}");
    assert!(
        snap.audible && snap.state == FollowerState::Locked,
        "{snap:?}"
    );
}

/// A locked follower through master ramps: 1.0 to 1.25 over 200 ms, back
/// to 1.0 over 500 ms, then up again at once. The master holds each
/// command back by the follower's lead and logs it ahead, so the follower
/// starts every ramp where the master does: no late command, no skip, no
/// mute, and the error stays within a frame.
#[test]
fn a_locked_follower_rides_master_ramps() {
    let secs = 80.0;
    let mut rig = Rig::new(DeviceModel::usb(0.0), follower_usb(30.0, 512, 3), secs);
    rig.pcm.master.vs_tx.push(master_stage()).ok().unwrap();
    // The alignment held (at its cap: room for the speed-ups), so nothing
    // but the ramps happens; its growth is its own test.
    rig.align_auto = false;
    rig.set_alignment(super::super::tee::MAX_ALIGN_MS);
    rig.pcm.keep = true;
    rig.start();
    assert_eq!(rig.run_until(40.0), RunExit::Stopped);
    let locked = rig.status.snapshot();
    let calls0 = rig.pcm.master.calls;
    assert!(
        locked.audible && locked.state == FollowerState::Locked,
        "{locked:?}"
    );
    let heard_from = rig.audible_changes.len();
    let cmds = [(40.0, 1.25, 200.0), (52.0, 1.0, 500.0), (64.0, 1.25, 0.0)];
    for (t, speed, ramp) in cmds {
        assert_eq!(rig.run_until(t), RunExit::Stopped);
        rig.pcm.master.speed = speed;
        rig.pcm.master.ramp_ms = ramp;
    }
    assert_eq!(rig.run_until(secs), RunExit::Stopped);
    let snap = rig.status.snapshot();
    let lead = f64::from_bits(
        rig.pcm
            .master
            .host
            .shared()
            .command_lead_ms
            .load(Ordering::Relaxed),
    );
    // Within half a second of a command the truth itself is approximate
    // (both maps interpolate across a call while the speed bends).
    let near = |t: f64| cmds.iter().any(|c| t >= c.0 && t < c.0 + 0.5);
    let (mut held, mut ramping) = (0.0f64, 0.0f64);
    for e in rig.errors.iter().filter(|e| e.0 >= 40.0) {
        if near(e.0) {
            ramping = ramping.max(e.1.abs());
        } else {
            held = held.max(e.1.abs());
        }
    }
    println!(
        "ramps: peak {held:.3} frames held, {ramping:.2} within the ramps; lead {lead} ms \
         (measured {:.1} ms), late commands {}, skips {} -> {}, max step {:.3}, master calls \
         {} more than periods",
        snap.lead_ms,
        snap.late_commands,
        locked.skips,
        snap.skips,
        rig.max_step,
        rig.pcm.master.calls - calls0 - ((secs - 40.0) * 48_000.0 / 1024.0).round() as u64,
    );
    // (Counted from lock: the ramps' commands.)
    assert_eq!(snap.late_commands, locked.late_commands, "{snap:?}");
    assert_eq!(snap.skips, locked.skips, "a skip");
    assert_eq!(
        rig.audible_changes.len(),
        heard_from,
        "muted: {:?}",
        &rig.audible_changes[heard_from - 1..]
    );
    assert_eq!(snap.known_steps, locked.known_steps, "a step");
    assert!(held < 1.0, "{held}");
    assert!(ramping < 4.0, "{ramping}");
    assert!(lead > 0.0 && lead <= 60.0, "{lead}");
    // The tone's own largest step is 0.058 at 1.0, so 0.073 at 1.25.
    assert!(
        rig.max_step < 0.058 * 1.25 * 1.05,
        "a click: {}",
        rig.max_step
    );
}

/// A trimmed follower (3 ms late) through master ramps 1 → 1.25 → 0.9 →
/// 1: the trim is time, so its offset in source frames follows the speed
/// (the follower replays the commands at the master's source positions, so
/// its lateness in frames follows by itself: no step). No skip, no step and
/// no mute; the error held between the ramps stays
/// within a frame (it reached 2.7 while the offset kept the old speed).
#[test]
fn a_trimmed_follower_rides_master_ramps() {
    let secs = 70.0;
    let mut rig = Rig::new(DeviceModel::usb(0.0), follower_usb(30.0, 512, 3), secs);
    rig.controls.set_trim_ms(3.0);
    rig.pcm.master.vs_tx.push(master_stage()).ok().unwrap();
    // The alignment held (at its cap: room for the speed-ups), so nothing
    // but the ramps happens; its growth is its own test.
    rig.align_auto = false;
    rig.set_alignment(super::super::tee::MAX_ALIGN_MS);
    rig.start();
    assert_eq!(rig.run_until(40.0), RunExit::Stopped);
    let locked = rig.status.snapshot();
    assert!(
        locked.audible && locked.state == FollowerState::Locked,
        "{locked:?}"
    );
    let heard_from = rig.audible_changes.len();
    let cmds = [(40.0, 1.25, 200.0), (50.0, 0.9, 200.0), (60.0, 1.0, 200.0)];
    for (t, speed, ramp) in cmds {
        assert_eq!(rig.run_until(t), RunExit::Stopped);
        rig.pcm.master.speed = speed;
        rig.pcm.master.ramp_ms = ramp;
    }
    assert_eq!(rig.run_until(secs), RunExit::Stopped);
    let snap = rig.status.snapshot();
    let near = |t: f64| cmds.iter().any(|c| t >= c.0 && t < c.0 + 0.5);
    let held = rig
        .errors
        .iter()
        .filter(|e| e.0 >= 40.0 && !near(e.0))
        .fold(0.0f64, |a, e| a.max(e.1.abs()));
    let mute = longest_mute(&rig, 40.0);
    println!(
        "trimmed ramps: peak {held:.3} frames held, skips {} -> {}, known steps {} -> {}, \
         longest mute {mute:.3} s",
        locked.skips, snap.skips, locked.known_steps, snap.known_steps
    );
    assert_eq!(snap.skips, locked.skips, "a skip");
    assert_eq!(
        rig.audible_changes.len(),
        heard_from,
        "muted: {:?}",
        &rig.audible_changes[heard_from - 1..]
    );
    assert_eq!(snap.known_steps, locked.known_steps, "a step");
    assert!(held < 1.0, "{held}");
}

/// A room calibration on the follower's chain: `taps` long, its peak
/// (gain 1) `peak` frames in, a faint last tap. The EQ as
/// [`eq_chain_with`], without a limiter.
fn conv_chain_with(rate: u32, preamp_db: f64, peak: usize, taps: usize) -> ActiveChain {
    let mut ir = vec![0.0; taps];
    ir[peak] = 1.0;
    ir[taps - 1] = 1e-6;
    let mut m = audivane_dsp_core::matrix::FilterMatrix::new(CH);
    m.set(0, 0, ir.clone());
    m.set(1, 1, ir.clone());
    // A faint cross term: a matrix, as a calibration bundle's.
    let mut cross = vec![0.0; taps];
    cross[taps - 1] = 1e-6;
    m.set(0, 1, cross);
    let ActiveChain::Single(base) = eq_chain_with(rate, preamp_db, false) else {
        unreachable!()
    };
    let mut config = base.config().clone();
    config.convolution = vec![audivane_protocol::ConvolutionConfig {
        name: None,
        enabled: true,
        ir_path: Default::default(),
        gain_db: 0.0,
        matrix: Some(Vec::new()),
        calibration: true,
    }];
    ActiveChain::Single(Box::new(Chain::new(ChainSpec {
        config,
        sample_rate: rate,
        channels: CH,
        max_block: 8192,
        irs: Some(audivane_dsp_core::matrix::ConvIrs::Matrix(m)),
    })))
}

/// A follower's incoming chain on a rational grid (44.1 kHz into a 48 kHz
/// device, a 20 001-tap IR with a late reflection): readied at a nonzero
/// phase of the grid, it is aligned before it takes over the playing
/// chain's state, so the next call's align leaves it be and it plays on
/// bit for bit, the reflection included. (Taken over first, the align
/// cleared it.)
#[test]
fn a_rational_follower_chain_keeps_the_handed_over_tail() {
    use audivane_protocol::RateTarget;
    const TAPS: usize = 20_001;
    let mut ir = vec![0.0; TAPS];
    ir[100] = 0.5;
    ir[TAPS - 1] = 0.4;
    let chain = || {
        ActiveChain::Single(Box::new(Chain::new(ChainSpec {
            config: ChainConfig {
                upsampler: audivane_dsp_core::resample::ManualConfig {
                    target: RateTarget::Rate(48_000),
                    taps: 2048,
                    ..Default::default()
                }
                .into_upsampler_config()
                .unwrap(),
                convolution: vec![audivane_protocol::ConvolutionConfig {
                    name: None,
                    enabled: true,
                    ir_path: Default::default(),
                    gain_db: 0.0,
                    matrix: None,
                    calibration: true,
                }],
                output: OutputConfig {
                    bits: 32,
                    dither: DitherKind::None,
                    headroom_db: 0.0,
                },
                ..ChainConfig::neutral()
            },
            sample_rate: 44_100,
            channels: CH,
            max_block: 8192,
            irs: Some(audivane_dsp_core::matrix::ConvIrs::Channels(vec![
                ir.clone(),
                ir.clone(),
            ])),
        })))
    };
    let m = 300;
    let input = |k: usize| -> Vec<Vec<f64>> {
        let mut rng = Rng::new(k as u64 + 1);
        (0..CH)
            .map(|_| (0..m).map(|_| rng.uniform(-0.3, 0.3)).collect())
            .collect()
    };
    let call = |c: &mut ActiveChain, k: usize, grid: u64| -> Vec<i32> {
        c.align_to(grid);
        let n = c.output_frames(m);
        let mut out = vec![0i32; n * CH];
        let mut g = StereoGainRamp::unity(48_000);
        let bufs = input(k);
        let _ = c.process_in_with_gain(
            DspIn::planar(&bufs, m),
            &mut out,
            CH,
            STAGE_FRAMES,
            &mut g,
            None,
        );
        out
    };
    let mut playing = chain();
    let calls = 2 * TAPS / m;
    for k in 0..calls {
        call(&mut playing, k, (k * m) as u64);
    }
    let next_pos = (calls * m) as u64;
    assert_ne!(next_pos % 147, 0, "a nonzero phase of the 160/147 grid");

    // A handover made before the align is cleared by it.
    let mut taken_first = chain();
    assert!(taken_first.inherit_filter_state(&mut playing, usize::MAX));
    assert!(
        taken_first.align_to(next_pos),
        "the align cleared the handover"
    );

    let mut next = chain();
    let lat = next.latency_out_frames();
    let target = ready_incoming(
        &mut next,
        &mut playing,
        next_pos,
        usize::MAX,
        usize::MAX,
        48_000,
    );
    assert_eq!(
        target,
        lat + SWAP_WARM_MARGIN,
        "handed over: the short warm-up"
    );
    // A chain that started cold at the same point: what the tail is worth.
    let mut cold = chain();
    cold.align_to(next_pos);
    let (mut warmed, mut compared, mut cold_off) = (0usize, 0usize, 0usize);
    for k in calls..calls + 2 * TAPS / m {
        let grid = (k * m) as u64;
        assert!(
            !next.align_to(grid),
            "call {k}: no align after the handover"
        );
        let (want, got, c) = (
            call(&mut playing, k, grid),
            call(&mut next, k, grid),
            call(&mut cold, k, grid),
        );
        warmed += want.len() / CH;
        // Once the stages after the convolution (the upsampler) have
        // settled: bit for bit, the reflection of what came before the
        // handover included; the cold chain misses it.
        if warmed > target {
            assert!(want == got, "call {k}: the handed-over chain strays");
            compared += 1;
            cold_off += usize::from(want != c && (k - calls) * m < TAPS);
        }
    }
    assert!(
        compared > 0 && cold_off > 0,
        "{compared} compared, cold off {cold_off}"
    );
}

/// Quick EQ edits: four built at once, faster than the follower drains
/// its queue (it holds two), then two more while it swaps. The control
/// side keeps the newest that did not fit pending (an older one dropped
/// there) and queues it on its next reap: the follower ends on the last.
#[test]
fn a_follower_ends_on_the_last_of_quick_edits() {
    const PEAK: usize = 1_440;
    const TAPS: usize = 32_768;
    let mut rig = Rig::new(DeviceModel::usb(0.0), follower_usb(60.0, 512, 3), 45.0);
    rig.start();
    assert_eq!(rig.run_until(35.0), RunExit::Stopped);
    for db in [-7.0, -8.0, -9.0, -10.0] {
        rig.swaps.offer(conv_chain_with(48_000, db, PEAK, TAPS));
    }
    assert!(
        rig.swaps.incoming.is_full(),
        "two queued, the newest pending"
    );
    let mut t = 35.0;
    for k in 0..80 {
        if k == 2 {
            rig.swaps.offer(conv_chain_with(48_000, -11.0, PEAK, TAPS));
            rig.swaps.offer(conv_chain_with(48_000, -12.0, PEAK, TAPS));
        }
        t += 0.05;
        assert_eq!(rig.run_until(t), RunExit::Stopped);
        rig.swaps.reap();
    }
    let ActiveChain::Single(c) = &rig.rt().chain else {
        panic!("a single chain")
    };
    let preamp = c.config().eq.as_ref().unwrap().preamp_db;
    println!(
        "quick edits: swaps {}, playing preamp {preamp} dB",
        rig.rt().swaps_done
    );
    assert_eq!(preamp, -12.0, "the last edit plays");
}

/// EQ edits on a locked follower whose chain carries a room calibration
/// (32768 taps, its peak 30 ms in): the calibration goes in (warming up
/// for its whole ring), then ten EQ edits, each taking over the playing
/// convolver. No gap, no click, the output is the tone at the edit's gain
/// throughout.
#[test]
fn a_follower_edits_its_eq_with_a_long_convolution() {
    const PEAK: usize = 1_440;
    const TAPS: usize = 32_768;
    let secs = 52.0;
    let mut rig = Rig::new(DeviceModel::usb(0.0), follower_usb(60.0, 512, 3), secs);
    rig.pcm.keep = true;
    rig.start();
    assert_eq!(rig.run_until(35.0), RunExit::Stopped);
    let locked = rig.status.snapshot();
    assert!(locked.state == FollowerState::Locked, "{locked:?}");
    let master_swap = |rig: &mut Rig, chain: ActiveChain, seq: u64| {
        rig.pcm
            .master
            ._keep
            .1
            .push(PendingSwap {
                chain,
                crossfade: true,
                generation: seq,
                swap_seq: seq,
            })
            .ok()
            .unwrap();
    };
    let mut t = 35.0;
    let swap_both = |rig: &mut Rig, t: &mut f64, db: f64, seq: u64, settle: f64| {
        master_swap(rig, conv_chain_with(48_000, db, PEAK, TAPS), seq);
        *t += 0.2;
        assert_eq!(rig.run_until(*t), RunExit::Stopped);
        // The control side drops the chains the follower retired.
        while rig.swaps.retired.pop().is_ok() {}
        rig.swaps
            .incoming
            .push(conv_chain_with(48_000, db, PEAK, TAPS))
            .ok()
            .unwrap();
        // An EQ edit lands at once (the next period, a 512-frame one; then
        // its fade): within 30 ms the follower plays it.
        *t += 0.03;
        assert_eq!(rig.run_until(*t), RunExit::Stopped);
        let landed = match &rig.rt().chain {
            ActiveChain::Single(c) => c.config().eq.as_ref().map(|e| e.preamp_db) == Some(db),
            _ => false,
        };
        *t += settle;
        assert_eq!(rig.run_until(*t), RunExit::Stopped);
        landed
    };
    // The calibration goes in: a latency change, a fade through silence.
    // (A new convolution: it warms up for its ring, so not within 30 ms.)
    assert!(!swap_both(&mut rig, &mut t, -7.0, 1, 3.0));
    let lat = rig.rt().chain.latency_out_frames();
    // The tone, delayed by the peak: first a second without an edit (the
    // map's own error against it), then the edits.
    rig.gain_db = -7.0;
    rig.audio_delay = PEAK as f64;
    rig.audio_err = 0.0;
    rig.audio_sig = 0.0;
    rig.audio_from_s = t;
    t += 1.0;
    assert_eq!(rig.run_until(t), RunExit::Stopped);
    let steady = (rig.audio_err / rig.audio_sig.max(1e-30)).sqrt();
    rig.gain_db = -7.025;
    rig.audio_err = 0.0;
    rig.audio_sig = 0.0;
    let from = rig.pcm.written_audio.len() / CH;
    rig.max_step = 0.0;
    for k in 0..10u64 {
        let db = if k % 2 == 0 { -7.05 } else { -7.0 };
        assert!(
            swap_both(&mut rig, &mut t, db, 2 + k, 0.3),
            "edit {k}: not at once"
        );
    }
    let rel = (rig.audio_err / rig.audio_sig.max(1e-30)).sqrt();
    let snap = rig.status.snapshot();
    let (mut run, mut gap) = (0usize, 0usize);
    for f in from..rig.pcm.written_audio.len() / CH {
        if f64::from(rig.pcm.written_audio[f * CH]).abs() < 1e-3 * 2_147_483_648.0 {
            run += 1;
            gap = gap.max(run);
        } else {
            run = 0;
        }
    }
    println!(
        "conv edits: swaps {}, output against the tone {:.1} dB (steady {:.1} dB), longest \
         near-silence {gap} frames, max step {:.4}, chain latency {lat}",
        rig.rt().swaps_done,
        20.0 * rel.log10(),
        20.0 * steady.log10(),
        rig.max_step,
    );
    assert_eq!(rig.rt().swaps_done, 11);
    assert_eq!(
        rig.rt().chain.latency_out_frames(),
        lat,
        "an EQ edit keeps it"
    );
    assert!(gap < 48, "a {gap}-frame gap");
    // A 997 Hz tone at 0.5 x -7 dB steps at most 0.029 a frame.
    assert!(rig.max_step < 0.035, "a click: step {:.4}", rig.max_step);
    // The two gains are 0.05 dB apart: 0.3 % either side of the middle.
    assert!(
        rel < steady + 3e-3,
        "the output strays from the tone: {rel:e} (steady {steady:e})"
    );
    assert_eq!(snap.state, FollowerState::Locked);
    assert_eq!(snap.skips, locked.skips, "a skip");
}

/// A config edit reaches a locked follower without a reopen: an EQ edit
/// (the same latency: a crossfade), then a limiter (5 ms more latency: a
/// fade through silence). The master swaps first and the follower a moment
/// later (its chain is built after the master's); both positions step by
/// the new latency, and the follower's loop holds its readings over the
/// gap. No skip, no relock, the output is the new chain's, no click.
#[test]
fn a_follower_swaps_its_chain_on_a_config_edit() {
    let secs = 75.0;
    let mut rig = Rig::new(DeviceModel::usb(0.0), follower_usb(60.0, 512, 3), secs);
    rig.pcm.keep = true;
    rig.start();
    assert_eq!(rig.run_until(35.0), RunExit::Stopped);
    let locked = rig.status.snapshot();
    assert!(
        locked.audible && locked.state == FollowerState::Locked,
        "{locked:?}"
    );
    let heard_from = rig.audible_changes.len();
    let master_swap = |rig: &mut Rig, chain: ActiveChain, seq: u64| {
        rig.pcm
            .master
            ._keep
            .1
            .push(PendingSwap {
                chain,
                crossfade: true,
                generation: seq,
                swap_seq: seq,
            })
            .ok()
            .unwrap();
    };
    // The EQ edit.
    master_swap(&mut rig, eq_chain_with(48_000, -7.0, false), 1);
    assert_eq!(rig.run_until(35.2), RunExit::Stopped);
    rig.swaps
        .incoming
        .push(eq_chain_with(48_000, -7.0, false))
        .ok()
        .unwrap();
    assert_eq!(rig.run_until(40.0), RunExit::Stopped);
    // The output is the new chain's.
    rig.gain_db = -7.0;
    rig.audio_err = 0.0;
    rig.audio_sig = 0.0;
    rig.audio_from_s = 40.0;
    assert_eq!(rig.run_until(50.0), RunExit::Stopped);
    let eq_rel = (rig.audio_err / rig.audio_sig.max(1e-30)).sqrt();
    let eq_swaps = rig.rt().swaps_done;
    rig.audio_from_s = f64::INFINITY;
    // The limiter: 240 frames more latency on both.
    master_swap(&mut rig, eq_chain_with(48_000, -7.0, true), 2);
    assert_eq!(rig.run_until(50.5), RunExit::Stopped);
    rig.swaps
        .incoming
        .push(eq_chain_with(48_000, -7.0, true))
        .ok()
        .unwrap();
    assert_eq!(rig.run_until(secs), RunExit::Stopped);
    let snap = rig.status.snapshot();
    // The truth steps at the master's swap and back at the follower's.
    let gap = |t: f64| (35.0..35.4).contains(&t) || (50.0..50.8).contains(&t);
    let held = rig
        .errors
        .iter()
        .filter(|e| e.0 >= 35.0 && !gap(e.0))
        .fold(0.0f64, |a, e| a.max(e.1.abs()));
    let last = outcome(&rig, 60.0, 60.0);
    println!(
        "chain swaps: {} (eq {eq_swaps}), eq output against the map {:.1} dB, held peak {held:.3}, \
         after {:?}, skips {} -> {}, max step {:.3}, chain latency {}",
        rig.rt().swaps_done,
        20.0 * eq_rel.log10(),
        last,
        locked.skips,
        snap.skips,
        rig.max_step,
        rig.rt().chain.latency_out_frames(),
    );
    // The fade through silence is silent only around its zero: the new
    // chain fades in within the call the old one reached zero in.
    let from = 30 * 48_000;
    let (mut run, mut gap) = (0usize, 0usize);
    for f in from..rig.pcm.written_audio.len() / CH {
        if f64::from(rig.pcm.written_audio[f * CH]).abs() < 1e-3 * 2_147_483_648.0 {
            run += 1;
            gap = gap.max(run);
        } else {
            run = 0;
        }
    }
    println!("longest near-silence after lock: {gap} frames");
    assert!(gap < 48, "a {gap}-frame gap");
    assert_eq!(eq_swaps, 1);
    assert_eq!(rig.rt().swaps_done, 2);
    assert_eq!(rig.rt().chain.latency_out_frames(), 240);
    assert!(eq_rel < 1e-3, "the EQ edit did not apply: {eq_rel:e}");
    assert_eq!(snap.skips, locked.skips, "a skip");
    assert_eq!(snap.state, FollowerState::Locked);
    assert_eq!(
        rig.audible_changes.len(),
        heard_from,
        "muted: {:?}",
        &rig.audible_changes[heard_from - 1..]
    );
    assert!(held < 1.0, "{held}");
    assert!(last.steady_peak < 1.0, "{last:?}");
    // The tone's own largest step (at -1 dB, before the edit) is 0.058.
    assert!(rig.max_step < 0.07, "a click: {}", rig.max_step);
}

/// The master asked for 2.0 (the driver's cap 2.0) while a follower that
/// needs a positive ε plays: the engine caps the master at 1.996 while
/// followed, so `v·(1 + ε)` stays within the stage's range. No plan is
/// refused, no skip, no mute, and back at 1.0 it still holds.
#[test]
fn a_followed_master_is_capped_below_2() {
    let secs = 70.0;
    let mut rig = Rig::new(DeviceModel::usb(0.0), follower_usb(-60.0, 512, 3), secs);
    rig.pcm.master.vs_tx.push(master_stage()).ok().unwrap();
    // The alignment held (at its cap: room for the speed-ups), so nothing
    // but the ramps happens; its growth is its own test.
    rig.align_auto = false;
    rig.set_alignment(super::super::tee::MAX_ALIGN_MS);
    rig.start();
    assert_eq!(rig.run_until(40.0), RunExit::Stopped);
    let locked = rig.status.snapshot();
    assert!(locked.audible && locked.eps > 0.0, "{locked:?}");
    let heard_from = rig.audible_changes.len();
    rig.pcm.master.speed = 2.0;
    rig.pcm.master.ramp_ms = 500.0;
    assert_eq!(rig.run_until(55.0), RunExit::Stopped);
    let top = rig.pcm.master.proc.varispeed_view().unwrap();
    rig.pcm.master.speed = 1.0;
    assert_eq!(rig.run_until(secs), RunExit::Stopped);
    let snap = rig.status.snapshot();
    let held = rig
        .errors
        .iter()
        .filter(|e| e.0 >= 40.0 && !(40.0..40.7).contains(&e.0) && !(55.0..55.7).contains(&e.0))
        .fold(0.0f64, |a, e| a.max(e.1.abs()));
    println!(
        "to 2.0: the master at {} (cap {}), refused plans {}, skips {} -> {}, held peak {held:.3}",
        top.speed, top.limit, snap.refused_plans, locked.skips, snap.skips
    );
    assert!(top.speed <= 1.996 && top.limit == 1.996, "{top:?}");
    assert_eq!(snap.refused_plans, 0);
    assert_eq!(snap.skips, locked.skips);
    assert_eq!(
        rig.audible_changes.len(),
        heard_from,
        "muted: {:?}",
        &rig.audible_changes[heard_from - 1..]
    );
    assert_eq!(snap.known_steps, locked.known_steps, "a step");
    // A ramp across the whole range starts on the follower within half an
    // output frame of the master's: up to about a frame, which the loop
    // takes out over the next seconds.
    assert!(held < 2.0, "{held}");
}

/// A slowdown with the alignment at its cap (1.0 to 0.9 over 200 ms, and
/// back): the delay holds in source frames, so the master's position never
/// steps and the follower rides it: no skip, no mute.
#[test]
fn a_slowdown_at_the_alignment_cap_keeps_the_follower_locked() {
    let secs = 75.0;
    let mut rig = Rig::new(DeviceModel::usb(0.0), follower_usb(30.0, 512, 3), secs);
    rig.pcm.master.vs_tx.push(master_stage()).ok().unwrap();
    rig.align_auto = false;
    rig.set_alignment(super::super::tee::MAX_ALIGN_MS);
    rig.start();
    assert_eq!(rig.run_until(40.0), RunExit::Stopped);
    let locked = rig.status.snapshot();
    assert!(locked.audible, "{locked:?}");
    let heard_from = rig.audible_changes.len();
    let seq = |rig: &Rig| {
        rig.pcm
            .master
            .host
            .shared()
            .align_seq
            .load(Ordering::Relaxed)
    };
    let seq0 = seq(&rig);
    rig.pcm.master.speed = 0.9;
    assert_eq!(rig.run_until(55.0), RunExit::Stopped);
    rig.pcm.master.speed = 1.0;
    assert_eq!(rig.run_until(secs), RunExit::Stopped);
    let snap = rig.status.snapshot();
    let held = rig
        .errors
        .iter()
        .filter(|e| e.0 >= 40.0 && !(40.0..40.5).contains(&e.0) && !(55.0..55.5).contains(&e.0))
        .fold(0.0f64, |a, e| a.max(e.1.abs()));
    println!(
        "slowdown to 0.9: align steps {}, skips {} -> {}, held peak {held:.3}",
        seq(&rig) - seq0,
        locked.skips,
        snap.skips
    );
    assert_eq!(seq(&rig), seq0, "the alignment stepped");
    assert_eq!(snap.skips, locked.skips, "a skip");
    assert_eq!(rig.audible_changes.len(), heard_from, "muted");
    assert!(held < 1.0, "{held}");
}

/// The master at 0.5 (the bottom of the range) with a faster follower
/// (ε < 0): the follower's stage plays down to 0.499, so it follows.
#[test]
fn a_faster_follower_follows_the_master_at_half_speed() {
    let secs = 60.0;
    let mut rig = Rig::new(DeviceModel::usb(0.0), follower_usb(60.0, 512, 3), secs);
    rig.pcm.master.speed = 0.5;
    rig.pcm.master.vs_tx.push(master_stage()).ok().unwrap();
    rig.start();
    assert_eq!(rig.run_until(secs), RunExit::Stopped);
    let o = outcome(&rig, 0.0, 40.0);
    let snap = rig.status.snapshot();
    println!(
        "at 0.5: {o:?}, eps {:.2e}, refused {}",
        snap.eps, snap.refused_plans
    );
    assert!(snap.eps < 0.0, "{snap:?}");
    assert_eq!(snap.refused_plans, 0);
    assert!(o.sync_s < 30.0 && o.steady_peak < 1.0, "{o:?}");
    assert!(
        snap.audible && snap.state == FollowerState::Locked,
        "{snap:?}"
    );
}

/// 192 kHz (two USB DACs, the master at 4096-frame periods, the follower
/// at 2048 with three queued): what the loop gets with its thresholds in
/// time. Test 19's 1-frame target is known to be out of reach here (the
/// pointer moves 24 frames at a time); the time is what matters.
#[test]
fn a_follower_at_192k() {
    let secs = 90.0;
    let mut m = DeviceModel::usb(0.0);
    m.rate = 192_000.0;
    m.period = 4096;
    let mut f = follower_usb(60.0, 2048, 3);
    f.rate = 192_000.0;
    let mut rig = Rig::new(m, f, secs);
    rig.start();
    assert_eq!(rig.run_until(secs), RunExit::Stopped);
    let o = outcome(&rig, 0.0, 45.0);
    let snap = rig.status.snapshot();
    println!(
        "192k usb pair: {o:?} ({:.1} us peak), alignment {} ms, ppm {:.2}, gated {}",
        o.steady_peak / 192.0 * 1e3,
        rig.align.ms,
        snap.ppm,
        snap.gated
    );
    assert!(o.audible_s < 45.0, "{o:?}");
    assert!(o.steady_peak < 8.0, "{o:?}");
    assert!(snap.audible, "{snap:?}");
}

/// The host refuses the master's device, a trim out of range and a fourth
/// follower; a follower whose device does not exist is reported lost, and
/// removing it stops its thread at once.
#[test]
fn the_host_refuses_bad_followers_and_stops_a_lost_one() {
    let build: ChainBuilder = Arc::new(|_, rate, _, _| Ok(eq_chain(rate)));
    let mut host = FollowerHost::new(build, Arc::new(|_, _| RateRatio::ONE));
    let mut tee = TeeHost::detached();
    tee.shared().graph_rate.store(48_000, Ordering::Relaxed);
    let cfg = EngineConfig::default();
    let master = "hw:CARD=Master,DEV=0";
    let opts = FollowerOptions::default();
    assert!(host.add(&mut tee, master, master, &cfg, opts).is_err());
    let far = FollowerOptions {
        trim_ms: 60.0,
        ..opts
    };
    assert!(
        host.add(&mut tee, "hw:CARD=Nope0,DEV=0", master, &cfg, far)
            .is_err()
    );
    let devices = [
        "hw:CARD=Nope1,DEV=0",
        "hw:CARD=Nope2,DEV=0",
        "hw:CARD=Nope3,DEV=0",
    ];
    for d in devices {
        host.add(&mut tee, d, master, &cfg, opts).unwrap();
    }
    assert!(
        host.add(&mut tee, "hw:CARD=Nope4,DEV=0", master, &cfg, opts)
            .is_err()
    );
    assert!(host.add(&mut tee, devices[0], master, &cfg, opts).is_err());
    let t0 = std::time::Instant::now();
    while host
        .status()
        .iter()
        .any(|(_, s)| s.state != FollowerState::Lost)
        && t0.elapsed() < Duration::from_secs(5)
    {
        std::thread::sleep(Duration::from_millis(20));
    }
    for (d, s) in host.status() {
        assert_eq!(s.state, FollowerState::Lost, "{d}: {s:?}");
        assert!(s.error.is_some(), "{d}");
    }
    let t = std::time::Instant::now();
    host.remove(&mut tee, devices[1]).unwrap();
    assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
    assert_eq!(host.len(), 2);
    assert_eq!(tee.slots().len(), 2);
    host.stop_all(&mut tee);
    assert!(host.is_empty() && tee.slots().is_empty());
}

/// A patched follower reopens only when the ratio it asks for changes, not
/// on each edit of its chain: one that asks for 4× and whose device gave 2×
/// keeps its 2× through an inherited EQ edit; clearing the patch (1×)
/// reopens it.
#[test]
fn a_patched_follower_reopens_only_when_its_asked_ratio_changes() {
    use audivane_protocol::ipc::FollowerConfig;
    let build: ChainBuilder = Arc::new(|_, rate, _, _| Ok(eq_chain(rate)));
    // A stand-in for its upsampler target: the limiter asks for 4×.
    let asks: RatioFn = Arc::new(|cfg, _| {
        if cfg.chain.limiter.mode == audivane_protocol::LimiterMode::Limit {
            RateRatio::integer(4)
        } else {
            RateRatio::ONE
        }
    });
    let mut host = FollowerHost::new(build, asks);
    let mut tee = TeeHost::detached();
    tee.shared().graph_rate.store(48_000, Ordering::Relaxed);
    let dev = "hw:CARD=NoSuchPatched,DEV=0";
    let mut cfg = EngineConfig::default();
    cfg.outputs.followers = vec![FollowerConfig {
        chain_patch: Some(serde_json::json!({"limiter": {"mode": "limit"}})),
        ..FollowerConfig::new(dev)
    }];
    host.add(
        &mut tee,
        dev,
        "hw:CARD=Master,DEV=0",
        &cfg,
        FollowerOptions::default(),
    )
    .unwrap();
    // Its device opened, stepped down to 2×.
    let h = &host.by_device[dev];
    *h.opened.lock().unwrap() = Some((48_000, RateRatio::integer(2), SampleFormat::S32));
    let reopens = |host: &FollowerHost| host.by_device[dev].controls.take_reopen();
    for preamp in [-2.0, -3.0] {
        cfg.chain.eq = Some(EqProfile {
            name: "eq".into(),
            preamp_db: preamp,
            ..Default::default()
        });
        host.config_changed(&cfg);
        assert!(!reopens(&host), "an inherited EQ edit reopened it");
    }
    cfg.outputs.followers[0].chain_patch = None;
    host.config_changed(&cfg);
    assert!(reopens(&host), "a cleared patch (1×) did not reopen it");
    host.stop_all(&mut tee);
}

/// A lost follower does not hold the master's compensation: it settles to
/// what the followers that play need, 0 with none; one that is opening
/// holds it.
#[test]
fn a_lost_follower_lets_the_master_s_compensation_settle() {
    use FollowerState::*;
    let master = 100.0;
    // One plays a chain 300 frames longer: the master waits 300.
    let mut comp = CompControl::default();
    let need = comp_need_of([(Locked, 400.0)], master);
    assert_eq!(need, Some(300.0));
    comp.tick(0.0, need, 48_000);
    assert_eq!(comp.tick(1.0, need, 48_000), Some(300));
    // Opening (a reopen): held.
    assert_eq!(comp_need_of([(Opening, f64::NAN)], master), None);
    assert_eq!(comp.tick(2.0, None, 48_000), None);
    assert_eq!(comp.tick(3.0, None, 48_000), None);
    assert_eq!(comp.frames, 300);
    // Lost (or stopped): it needs nothing, the master settles to 0.
    for lost in [Lost, Off] {
        assert_eq!(comp_need_of([(lost, 400.0)], master), Some(0.0));
    }
    let need = comp_need_of([(Lost, 400.0)], master);
    comp.tick(4.0, need, 48_000);
    assert_eq!(comp.tick(5.0, need, 48_000), Some(0));
    // Another still plays a 50-frame longer chain: that much.
    assert_eq!(
        comp_need_of([(Lost, 400.0), (Acquiring, 150.0)], master),
        Some(50.0)
    );
    // A shorter one: none.
    assert_eq!(comp_need_of([(Locked, 20.0)], master), Some(0.0));
    // The master not mapped: held.
    assert_eq!(comp_need_of([(Locked, 400.0)], f64::NAN), None);
}

/// Keep pitch for the master at 48 kHz: the stretch in varispeed's place.
fn master_stretch() -> VarispeedRtCmd {
    VarispeedRtCmd::InstallStretch(Box::new(
        audivane_dsp_core::tempo::TempoStretch::new(audivane_dsp_core::tempo::TempoSpec {
            sample_rate: 48_000,
            channels: CH,
            max_frames: 8192,
            speed: 1.0,
        })
        .unwrap(),
    ))
}

/// The largest error outside the windows `skip` (start, length) after
/// `from_s`.
fn held_peak(rig: &Rig, from_s: f64, skip: &[(f64, f64)]) -> f64 {
    rig.errors
        .iter()
        .filter(|e| e.0 >= from_s && !skip.iter().any(|w| (w.0..w.0 + w.1).contains(&e.0)))
        .fold(0.0f64, |a, e| a.max(e.1.abs()))
}

/// Keep pitch on the master: the stretch runs before the tee,
/// so the follower reads the stretched stream at nominal speed and its own
/// stage only corrects its clock. At 1.25 it locks and holds as at 1.0,
/// and the master's own ramp (to 0.8 over 500 ms, back to 1.25 at once)
/// moves nothing it follows: no skip, no mute, no late command.
#[test]
fn a_follower_follows_a_stretched_master_through_ramps() {
    let secs = 75.0;
    let mut rig = Rig::new(DeviceModel::usb(0.0), follower_usb(30.0, 512, 3), secs);
    rig.pcm.master.speed = 1.25;
    rig.pcm.master.vs_tx.push(master_stretch()).ok().unwrap();
    rig.pcm.keep = true;
    rig.start();
    assert_eq!(rig.run_until(40.0), RunExit::Stopped);
    let locked = rig.status.snapshot();
    let o = outcome(&rig, 0.0, 30.0);
    println!("stretched 1.25: {o:?}, eps {:.1} ppm", locked.eps * 1e6);
    assert!(
        locked.audible && locked.state == FollowerState::Locked,
        "{locked:?}"
    );
    assert!(o.sync_s < 30.0 && o.steady_peak < 1.0, "{o:?}");
    let a = rig.pcm.master.proc.varispeed_applied().snapshot();
    assert!(a.stretch && a.speed == 1.25, "{a:?}");
    let heard_from = rig.audible_changes.len();
    rig.pcm.master.speed = 0.8;
    rig.pcm.master.ramp_ms = 500.0;
    assert_eq!(rig.run_until(55.0), RunExit::Stopped);
    rig.pcm.master.speed = 1.25;
    rig.pcm.master.ramp_ms = 0.0;
    assert_eq!(rig.run_until(secs), RunExit::Stopped);
    let snap = rig.status.snapshot();
    let held = held_peak(&rig, 40.0, &[]);
    println!(
        "stretched ramps: peak {held:.3} frames (ramps included), skips {} -> {}, late {}, max \
         step {:.3}",
        locked.skips, snap.skips, snap.late_commands, rig.max_step
    );
    assert_eq!(snap.skips, locked.skips, "a skip");
    assert_eq!(snap.late_commands, 0);
    assert_eq!(rig.audible_changes.len(), heard_from, "muted");
    assert!(held < 1.0, "{held}");
    assert!(rig.max_step < 0.07, "a click: {}", rig.max_step);
}

/// Keep pitch toggled on and off under a locked follower (the master at
/// 1.1): each toggle bumps the tee's epoch, and the follower re-syncs
/// once, then holds; the master never waits on it.
#[test]
fn a_follower_resyncs_once_per_keep_pitch_toggle() {
    let secs = 110.0;
    let mut rig = Rig::new(DeviceModel::usb(0.0), follower_usb(30.0, 512, 3), secs);
    rig.pcm.master.speed = 1.1;
    rig.pcm.master.vs_tx.push(master_stage()).ok().unwrap();
    rig.pcm.keep = true;
    rig.start();
    assert_eq!(rig.run_until(40.0), RunExit::Stopped);
    let locked = rig.status.snapshot();
    assert!(locked.audible, "{locked:?}");
    let epoch = |rig: &Rig| rig.pcm.master.host.shared().position.epoch();
    let e0 = epoch(&rig);
    rig.pcm.master.vs_tx.push(master_stretch()).ok().unwrap();
    assert_eq!(rig.run_until(41.0), RunExit::Stopped);
    let e1 = epoch(&rig);
    assert_eq!(rig.run_until(75.0), RunExit::Stopped);
    let a = rig.pcm.master.proc.varispeed_applied().snapshot();
    assert!(a.stretch && a.speed == 1.1, "{a:?}");
    let on = rig.status.snapshot();
    let sync_on = resync_after(&rig, 40.0, 75.0);
    let heard_on = audible_after(&rig, 40.0);
    rig.pcm.master.vs_tx.push(master_stage()).ok().unwrap();
    assert_eq!(rig.run_until(secs), RunExit::Stopped);
    let a = rig.pcm.master.proc.varispeed_applied().snapshot();
    assert!(!a.stretch && a.speed == 1.1, "{a:?}");
    let off = rig.status.snapshot();
    let sync_off = resync_after(&rig, 75.0, secs);
    let heard_off = audible_after(&rig, 75.0);
    println!(
        "keep pitch on: epoch +{}, in sync after {sync_on:.1} s, heard after {heard_on:.1} s; \
         off: in sync after {sync_off:.1} s, heard after {heard_off:.1} s; skips {} -> {} -> {}, \
         max step {:.3}",
        e1 - e0,
        locked.skips,
        on.skips,
        off.skips,
        rig.max_step
    );
    assert!(e1 > e0, "the toggle bumps the epoch");
    assert!(sync_on < 15.0 && heard_on < 20.0, "{sync_on} {heard_on}");
    assert!(
        sync_off < 15.0 && heard_off < 20.0,
        "{sync_off} {heard_off}"
    );
    assert!(off.audible && off.state == FollowerState::Locked, "{off:?}");
    let held = held_peak(&rig, 40.0, &[(40.0, 20.0), (75.0, 20.0)]);
    assert!(held < 1.0, "{held}");
    // Heard far off only briefly: the master's stage fades out, but the
    // follower plays the source on to the domain change (where the log's
    // `Off` mutes it for its reset), about the master stage's delay; and
    // the master's position jumps by that delay there, which the follower
    // re-acquires (a skip), as at a varispeed disable.
    for at in [40.0, 75.0] {
        let off: Vec<f64> = rig
            .errors
            .iter()
            .filter(|e| e.2 && e.1.abs() > 8.0 && (at..at + 20.0).contains(&e.0))
            .map(|e| e.0)
            .collect();
        let span = off.last().map_or(0.0, |l| l - off[0]);
        println!("toggle at {at}: heard off by more than 8 frames for {span:.3} s");
        assert!(span < 0.1, "{span}");
    }
    assert!(rig.max_step < 0.07, "a click: {}", rig.max_step);
    // At the alignment's cap, a toggle's re-sync is not a lack of source.
    assert_eq!(rig.controls.lag_ms(), 0.0, "the lag grew at a toggle");
}

/// With followers attached and 12 ms of alignment, the master never clicks
/// at a Keep pitch toggle (either way), a varispeed disable (of either
/// stage) or an enable. After the stretch the alignment delay holds its
/// output, so a stop of the stretch plays that out before the delay's
/// history restarts, which would cut a tone at full level.
#[test]
fn the_master_never_clicks_at_stage_changes_with_alignment() {
    let mut m = Master::new(DeviceModel::usb(0.0), 3);
    m.host
        .add_follower("hw:CARD=USB,DEV=0", &EngineConfig::default())
        .unwrap();
    m.host.set_alignment_ms(12.0).unwrap();
    m.keep_output = true;
    m.speed = 1.1;
    m.vs_tx.push(master_stage()).ok().unwrap();
    m.start(0.0);
    let disable = || VarispeedRtCmd::Disable;
    type MakeCmd = fn() -> VarispeedRtCmd;
    let steps: [(&str, MakeCmd); 7] = [
        ("Keep pitch on", master_stretch),
        ("Keep pitch off", master_stage),
        ("Keep pitch on again", master_stretch),
        ("varispeed off (the stretch)", disable),
        ("varispeed on (the stretch)", master_stretch),
        ("Keep pitch off again", master_stage),
        ("varispeed off", disable),
    ];
    let left = |m: &Master, from: usize| -> Vec<f64> {
        m.output[from * CH..]
            .iter()
            .step_by(CH)
            .map(|&x| f64::from(x) / 2_147_483_648.0)
            .collect()
    };
    for _ in 0..60 {
        m.block();
    }
    assert!(m.proc.tee().delay_frames() > 500, "the alignment runs");
    // A 997 Hz tone at -7 dB moves at most 0.058 a sample, 0.064 at 1.1.
    let bound = 0.07;
    for (what, cmd) in steps {
        let from = m.output.len() / CH - 64;
        m.vs_tx.push(cmd()).ok().unwrap();
        for _ in 0..60 {
            m.block();
        }
        let x = left(&m, from);
        let (step, at) = x
            .windows(2)
            .enumerate()
            .map(|(i, w)| ((w[1] - w[0]).abs(), i))
            .fold((0.0, 0), |a, b| if b.0 > a.0 { b } else { a });
        let a = m.proc.varispeed_applied().snapshot();
        println!(
            "{what}: largest step {step:.4} at {at}, stretch {}",
            a.stretch
        );
        assert!(step < bound, "{what}: a click of {step} at {at}");
        assert!(
            x[x.len() - 4800..].iter().any(|v| v.abs() > 0.3),
            "{what}: silent after"
        );
    }
}

#[test]
fn a_follower_plays_late_by_what_it_lacks_once_the_alignment_is_at_its_cap() {
    // Plenty, or nothing measured yet: unchanged.
    assert_eq!(next_lag_ms(0.0, 3.0), None);
    assert_eq!(next_lag_ms(0.0, f64::NAN), None);
    // 4.2 ms short: enough for 2 ms of slack.
    assert_eq!(next_lag_ms(0.0, -4.2), Some(7.0));
    assert_eq!(next_lag_ms(7.0, 0.5), Some(9.0));
    // Capped, and never back.
    assert_eq!(next_lag_ms(MAX_LAG_MS - 2.0, -10.0), Some(MAX_LAG_MS));
    assert_eq!(next_lag_ms(MAX_LAG_MS, -10.0), None);
    let c = FollowerControls::new(0.0, 0.0);
    c.set_lag_ms(1e9);
    assert_eq!(c.lag_ms(), MAX_LAG_MS);
    c.set_lag_ms(f64::NAN);
    assert_eq!(c.lag_ms(), 0.0);
}

#[test]
fn a_late_follower_steps_back_by_what_it_can_spare() {
    // Aligned, nothing measured, or nothing to spare: unchanged.
    assert_eq!(shrunk_lag_ms(0.0, 50.0), None);
    assert_eq!(shrunk_lag_ms(5.0, f64::NAN), None);
    assert_eq!(shrunk_lag_ms(5.0, 2.5), None);
    // 29.5 ms ahead at the worst phase: 27 ms to spare, not worth a step
    // (less than the start's estimate can be over); 32.5: 30 ms back,
    // keeping 2.5.
    assert_eq!(shrunk_lag_ms(46.0, 29.5), None);
    assert_eq!(shrunk_lag_ms(46.0, 32.5), Some(16.0));
    // All of it: aligned again, however little.
    assert_eq!(shrunk_lag_ms(3.0, 5.0), Some(0.0));
    // More than it plays late: aligned again.
    assert_eq!(shrunk_lag_ms(26.0, 80.0), Some(0.0));
    // A later trim takes the lateness back first, then moves it.
    let c = FollowerControls::new(0.0, 0.0);
    c.set_lag_ms(30.0);
    c.retrim(-5.0);
    assert_eq!(c.lag_ms(), 35.0);
    c.retrim(5.0);
    assert_eq!((c.trim_ms(), c.lag_ms()), (5.0, 25.0));
    c.retrim(40.0);
    assert_eq!((c.trim_ms(), c.lag_ms()), (40.0, 0.0));
}

/// The slow follower plays late (unaligned); a trim 5 ms later than its
/// lateness takes it back exactly: aligned again (no lateness), 5 ms later
/// than before in one known step, no skip of the loop's, and heard within
/// a frame at once.
/// Then a reopen (as at a rate change) starts aligned too.
#[test]
fn a_late_follower_is_aligned_again_by_a_later_trim_and_a_reopen() {
    let secs = 150.0;
    let mut rig = Rig::new(DeviceModel::usb(0.0), follower_usb(40.0, 2048, 3), secs);
    rig.start();
    assert_eq!(rig.run_until(60.0), RunExit::Stopped);
    let late = rig.status.snapshot();
    let lag = rig.controls.lag_ms();
    assert!(lag > 0.0 && late.audible, "{lag} {late:?}");
    rig.controls.retrim(lag + 5.0);
    assert_eq!(rig.controls.lag_ms(), 0.0);
    rig.lag_ms = 0.0;
    assert_eq!(rig.run_until(90.0), RunExit::Stopped);
    let snap = rig.status.snapshot();
    // (Its path is about 90 ms: the step reaches the DAC after it.)
    let held = held_after(&rig, 60.0, &[60.0], 0.3);
    let back = resync_after(&rig, 60.0, 90.0);
    println!(
        "late {lag} ms, trim +{lag} +5 ms: lateness {} ms, known steps {} -> {}, skips {} -> {}, \
         back within a frame after {back:.3} s, held {held:.3}",
        rig.controls.lag_ms(),
        late.known_steps,
        snap.known_steps,
        late.skips,
        snap.skips
    );
    assert_eq!(rig.controls.lag_ms(), 0.0, "late again");
    assert_eq!(snap.skips, late.skips, "the loop skipped");
    assert_eq!(snap.known_steps, late.known_steps + 1);
    assert!(back < 0.3 && held < 1.0, "{back} {held}");
    assert!(
        snap.audible && snap.state == FollowerState::Locked,
        "{snap:?}"
    );
    // Back to trim 0 (35 ms earlier, from aligned: taken, then late again
    // by what it lacks); a reopen starts from aligned and needs it again:
    // what it lacks at the new stream's phase of the two clocks' wakeups
    // (within one follower period of the last, never the seconds a stale
    // slack would ask).
    rig.controls.retrim(0.0);
    assert_eq!(rig.run_until(95.0), RunExit::Stopped);
    assert!(rig.controls.lag_ms() > 0.0);
    rig.reopen_at(96.0);
    assert_eq!(rig.controls.lag_ms(), 0.0);
    assert_eq!(rig.run_until(secs), RunExit::Stopped);
    let again = rig.controls.lag_ms();
    let snap = rig.status.snapshot();
    let period_ms = 2048.0 / 48.0;
    assert!(
        again > 0.0 && again <= lag + period_ms + 2.0,
        "{again} against {lag}"
    );
    assert!(
        snap.audible && snap.state == FollowerState::Locked,
        "{snap:?}"
    );
}

#[test]
fn the_protocol_view_names_the_state() {
    use audivane_protocol::ipc::{FollowerConfig, FollowerState as W};
    let f = FollowerConfig::new("hw:CARD=USB,DEV=0");
    let status = FollowerStatus::default();
    let snap = |state, audible| {
        status.set_state(state);
        status.audible.store(audible, Ordering::Relaxed);
        status.snapshot()
    };
    let state = |s: Option<&FollowerSnapshot>, lag| view(&f, s, lag, None, None).state;
    assert_eq!(state(None, 0.0), W::Off);
    assert_eq!(
        state(Some(&snap(FollowerState::Opening, false)), 0.0),
        W::Opening
    );
    assert_eq!(
        state(Some(&snap(FollowerState::Acquiring, false)), 0.0),
        W::Locking
    );
    assert_eq!(state(Some(&snap(FollowerState::Lost, false)), 0.0), W::Lost);
    assert_eq!(
        state(Some(&snap(FollowerState::Locked, true)), 0.0),
        W::Locked
    );
    assert_eq!(
        state(Some(&snap(FollowerState::Locked, false)), 0.0),
        W::Muted
    );
    let late = view(
        &f,
        Some(&snap(FollowerState::Locked, true)),
        6.0,
        None,
        None,
    );
    assert_eq!((late.state, late.offset_ms), (W::Unaligned, 6.0));
    // Not started: why, and no NaN on the wire.
    let off = view(&f, None, 0.0, Some("no thread"), None);
    assert_eq!(off.error.as_deref(), Some("no thread"));
    let fresh = view(
        &f,
        Some(&FollowerStatus::default().snapshot()),
        0.0,
        None,
        None,
    );
    assert!(fresh.latency_ms.is_finite() && fresh.ppm.is_finite());
}

/// A follower whose path needs more than the master's 30 ms (2048-frame
/// periods, three queued) plays late by the rest, locked and heard, and the
/// status says so.
#[test]
fn a_slow_follower_plays_unaligned() {
    let secs = 120.0;
    let mut rig = Rig::new(DeviceModel::usb(0.0), follower_usb(40.0, 2048, 3), secs);
    rig.start();
    assert_eq!(rig.run_until(secs), RunExit::Stopped);
    let snap = rig.status.snapshot();
    let lag = rig.controls.lag_ms();
    let o = outcome(&rig, 0.0, 90.0);
    println!(
        "unaligned: {o:?}, alignment {} ms, lag {lag} ms, slack {:.1} ms, underfeeds {}",
        rig.align.ms, snap.slack_ms, snap.underfeeds
    );
    assert_eq!(rig.align.ms, super::super::tee::MAX_ALIGN_MS);
    assert!(lag > 0.0 && lag < MAX_LAG_MS, "{lag}");
    assert!(
        snap.state == FollowerState::Locked && snap.audible,
        "{snap:?}"
    );
    assert!(o.audible_s < 90.0, "{o:?}");
    assert!(o.steady_peak < 1.0, "{o:?}");
    let u = rig.status.snapshot().underfeeds;
    let f = audivane_protocol::ipc::FollowerConfig::new("hw:CARD=USB,DEV=0");
    let v = view(&f, Some(&snap), lag, None, None);
    assert_eq!(v.state, audivane_protocol::ipc::FollowerState::Unaligned);
    assert_eq!(v.offset_ms, lag);
    assert!(
        snap.slack_ms > 0.0,
        "still short: {} ms, {u} underfeeds",
        snap.slack_ms
    );
}

/// The largest error after each event's first `settle_s` (the step on its
/// way through both paths), from `from_s` on.
fn held_after(rig: &Rig, from_s: f64, events: &[f64], settle_s: f64) -> f64 {
    rig.errors
        .iter()
        .filter(|e| e.0 >= from_s && !events.iter().any(|&t| e.0 >= t && e.0 < t + settle_s))
        .fold(0.0f64, |a, e| a.max(e.1.abs()))
}

/// A live trim edit (+1.5 ms, 72 frames) and a lateness step (+2 ms, as at
/// the alignment cap) are steps the follower knows exactly. Each is one skip
/// of its size under the fade: no skip of the loop's, no mute beyond the
/// fade, and the error back within a frame at once. A trim back while it
/// plays late goes into its lateness (it cannot play earlier): no step.
#[test]
fn trim_and_lateness_changes_are_exact_steps() {
    let secs = 75.0;
    let mut rig = Rig::new(DeviceModel::usb(0.0), follower_usb(30.0, 512, 3), secs);
    rig.pcm.keep = true;
    rig.start();
    assert_eq!(rig.run_until(40.0), RunExit::Stopped);
    let locked = rig.status.snapshot();
    assert!(
        locked.audible && locked.state == FollowerState::Locked,
        "{locked:?}"
    );
    let heard_from = rig.audible_changes.len();
    // The alignment settled; the lateness is set by hand from here (its
    // controller would take a lateness nothing needs back).
    rig.align_auto = false;
    rig.controls.retrim(1.5);
    assert_eq!(rig.run_until(50.0), RunExit::Stopped);
    let trimmed = rig.status.snapshot();
    rig.controls.set_lag_ms(2.0);
    rig.lag_ms = 2.0;
    assert_eq!(rig.run_until(60.0), RunExit::Stopped);
    let lagged = rig.status.snapshot();
    rig.controls.retrim(0.0);
    assert_eq!(rig.controls.lag_ms(), 3.5);
    rig.lag_ms = 3.5;
    assert_eq!(rig.run_until(secs), RunExit::Stopped);
    let snap = rig.status.snapshot();
    let events = [40.0, 50.0, 60.0];
    let held = held_after(&rig, 40.0, &events, 0.15);
    let mute = longest_mute(&rig, 40.0);
    let back = [
        resync_after(&rig, 40.0, 50.0),
        resync_after(&rig, 50.0, 60.0),
    ];
    println!(
        "trim +1.5 ms, lateness +2 ms, trim back: known steps {} -> {} -> {} -> {}, skips {} -> {}, \
         longest mute {:.1} ms, within a frame after {back:.3?} s, held peak {held:.3} frames, \
         max step {:.3}, alignment {} ms",
        locked.known_steps,
        trimmed.known_steps,
        lagged.known_steps,
        snap.known_steps,
        locked.skips,
        snap.skips,
        mute * 1e3,
        rig.max_step,
        rig.align.ms,
    );
    assert!(back.iter().all(|&b| b < 0.15), "{back:?}");
    assert!(rig.max_step < 0.07, "a click: {}", rig.max_step);
    assert_eq!(trimmed.known_steps, locked.known_steps + 1);
    assert_eq!(lagged.known_steps, trimmed.known_steps + 1);
    assert_eq!(
        snap.known_steps, lagged.known_steps,
        "a step for a trim taken up"
    );
    assert_eq!(snap.skips, locked.skips, "the loop skipped");
    assert!(
        mute < STEP_MUTE_S,
        "muted {mute} s: {:?}",
        &rig.audible_changes[heard_from - 1..]
    );
    assert!(held < 1.0, "{held}");
    assert!(snap.audible && snap.state == FollowerState::Locked);
}

/// A follower 5 ms late (as when unaligned) through the rig's ramps
/// 1 → 1.25 → 0.9 → 1. The lateness is time; the follower's stage replays
/// each command at the master's source position, so its lateness in frames
/// follows the speed by itself and only the detector's offset moves (84
/// frames from 1.25 to 0.9): no step, no skip, no mute, the error within a
/// frame between the ramps.
#[test]
fn a_late_follower_rides_master_ramps() {
    let secs = 70.0;
    let mut rig = Rig::new(DeviceModel::usb(0.0), follower_usb(30.0, 512, 3), secs);
    rig.controls.set_lag_ms(5.0);
    rig.lag_ms = 5.0;
    rig.pcm.master.vs_tx.push(master_stage()).ok().unwrap();
    // The alignment held (at its cap: room for the speed-ups), so nothing
    // but the ramps happens; its growth is its own test.
    rig.align_auto = false;
    rig.set_alignment(super::super::tee::MAX_ALIGN_MS);
    rig.start();
    assert_eq!(rig.run_until(40.0), RunExit::Stopped);
    let locked = rig.status.snapshot();
    assert!(
        locked.audible && locked.state == FollowerState::Locked,
        "{locked:?}"
    );
    let heard_from = rig.audible_changes.len();
    let cmds = [(40.0, 1.25, 200.0), (50.0, 0.9, 200.0), (60.0, 1.0, 200.0)];
    for (t, speed, ramp) in cmds {
        assert_eq!(rig.run_until(t), RunExit::Stopped);
        rig.pcm.master.speed = speed;
        rig.pcm.master.ramp_ms = ramp;
    }
    assert_eq!(rig.run_until(secs), RunExit::Stopped);
    let snap = rig.status.snapshot();
    let held = held_after(&rig, 40.0, &cmds.map(|c| c.0), 0.5);
    let mute = longest_mute(&rig, 40.0);
    println!(
        "5 ms late through ramps: held peak {held:.3} frames, skips {} -> {}, known steps {} -> {}, \
         longest mute {:.1} ms, late commands {}",
        locked.skips,
        snap.skips,
        locked.known_steps,
        snap.known_steps,
        mute * 1e3,
        snap.late_commands - locked.late_commands,
    );
    assert_eq!(snap.skips, locked.skips, "the loop skipped");
    assert_eq!(
        rig.audible_changes.len(),
        heard_from,
        "muted: {:?}",
        &rig.audible_changes[heard_from - 1..]
    );
    assert_eq!(snap.known_steps, locked.known_steps, "a step");
    assert!(held < 1.0, "{held}");
}

/// Switching varispeed on (at 1.0) delays the master's output by the stage
/// (its filters and look-ahead, about 500 frames): a known step of the
/// master's, which the tee publishes. The follower takes the same step at
/// once: one skip of its size, the loop untouched, the fade only.
#[test]
fn varispeed_switched_on_is_an_exact_step() {
    let secs = 60.0;
    let mut rig = Rig::new(DeviceModel::usb(0.0), follower_usb(30.0, 512, 3), secs);
    rig.pcm.keep = true;
    rig.start();
    assert_eq!(rig.run_until(40.0), RunExit::Stopped);
    let locked = rig.status.snapshot();
    assert!(
        locked.audible && locked.state == FollowerState::Locked,
        "{locked:?}"
    );
    let heard_from = rig.audible_changes.len();
    let steps =
        |rig: &Rig| step_frames(rig.pcm.master.host.shared().steps.load(Ordering::Relaxed) as i64);
    let before = steps(&rig);
    rig.pcm.master.vs_tx.push(master_stage()).ok().unwrap();
    assert_eq!(rig.run_until(secs), RunExit::Stopped);
    let snap = rig.status.snapshot();
    let step = steps(&rig) - before;
    let held = held_after(&rig, 40.0, &[40.0], 0.2);
    let mute = longest_mute(&rig, 40.0);
    let back = resync_after(&rig, 40.0, secs);
    println!(
        "varispeed on at 1.0: the master stepped {step:.2} frames; known steps {} -> {}, skips {} \
         -> {}, longest mute {:.1} ms, within a frame after {back:.3} s, held peak {held:.3} \
         frames, max step {:.3}",
        locked.known_steps,
        snap.known_steps,
        locked.skips,
        snap.skips,
        mute * 1e3,
        rig.max_step,
    );
    assert!(back < 0.2, "{back}");
    assert!(rig.max_step < 0.07, "a click: {}", rig.max_step);
    assert!(step < -100.0, "{step}");
    assert!(snap.known_steps > locked.known_steps);
    assert_eq!(snap.skips, locked.skips, "the loop skipped");
    assert!(
        mute < STEP_MUTE_S,
        "muted {mute} s: {:?}",
        &rig.audible_changes[heard_from - 1..]
    );
    assert!(held < 1.0, "{held}");
    assert!(snap.audible && snap.state == FollowerState::Locked);
}

/// At 60 ms, at the default quantum (1024, three periods queued on both sides,
/// as on the rig): a follower's path needs more than 30 ms, and it lines up
/// within the 60 ms cap instead of playing late.
#[test]
fn a_follower_at_the_default_quantum_lines_up() {
    let secs = 60.0;
    let mut master = DeviceModel::usb(0.0);
    master.buffer_periods = 3;
    let mut rig = Rig::new(master, follower_usb(50.0, 1024, 3), secs);
    rig.start();
    assert_eq!(rig.run_until(secs), RunExit::Stopped);
    let snap = rig.status.snapshot();
    let o = outcome(&rig, 0.0, 40.0);
    println!(
        "1024 x 3 pair: {o:?}, alignment {} ms, lag {} ms, slack {:.1} ms, latency {:.1} ms",
        rig.align.ms,
        rig.controls.lag_ms(),
        snap.slack_ms,
        snap.latency_ms
    );
    assert!(rig.align.ms > 30.0 && rig.align.ms <= super::super::tee::MAX_ALIGN_MS);
    assert_eq!(rig.controls.lag_ms(), 0.0, "unaligned");
    assert!(
        snap.state == FollowerState::Locked && snap.audible,
        "{snap:?}"
    );
    assert!(o.steady_peak < 1.0, "{o:?}");
}

/// Over a long session the phase of the two clocks' wakeups drifts (here
/// 40 ppm: through a whole 1024-frame period in about 9 minutes), and the
/// slack's least value over a window with it. The alignment the first
/// windows settle on holds: it never steps again (each step is a fade on
/// the master), and the follower never runs short.
#[test]
fn the_alignment_holds_as_the_clocks_phase_drifts() {
    let secs = 900.0;
    let mut master = DeviceModel::usb(0.0);
    master.buffer_periods = 3;
    let mut rig = Rig::new(master, follower_usb(40.0, 1024, 3), secs);
    rig.start();
    assert_eq!(rig.run_until(60.0), RunExit::Stopped);
    let (align0, under0) = (rig.align.ms, rig.status.snapshot().underfeeds);
    assert_eq!(rig.run_until(secs), RunExit::Stopped);
    let snap = rig.status.snapshot();
    println!(
        "phase drift: alignment {align0} -> {} ms, lag {} ms, slack {:.1} ms, underfeeds {under0} -> {}",
        rig.align.ms,
        rig.controls.lag_ms(),
        snap.slack_ms,
        snap.underfeeds
    );
    assert_eq!(rig.align.ms, align0, "the alignment stepped");
    assert_eq!(snap.underfeeds, under0, "ran short");
    assert!(
        snap.audible && snap.state == FollowerState::Locked,
        "{snap:?}"
    );
}

/// A trim edit at `trim_at_s` on a follower locked `from_ms` late (the
/// alignment held), while the master ramps 1 → 1.25 from 40 s: the step,
/// the loop's skips, late commands, the longest mute and the error held
/// from 41 s.
fn trim_across_a_ramp(from_ms: f64, to_ms: f64, trim_at_s: f64) -> (u64, u64, u64, f64, f64) {
    let secs = 50.0;
    let mut rig = Rig::new(DeviceModel::usb(0.0), follower_usb(30.0, 512, 3), secs);
    rig.align_auto = false;
    rig.set_alignment(super::super::tee::MAX_ALIGN_MS);
    rig.controls.retrim(from_ms);
    rig.pcm.master.vs_tx.push(master_stage()).ok().unwrap();
    rig.start();
    assert_eq!(rig.run_until(40.0), RunExit::Stopped);
    let s0 = rig.status.snapshot();
    assert!(s0.audible, "{s0:?}");
    rig.pcm.master.speed = 1.25;
    assert_eq!(rig.run_until(trim_at_s), RunExit::Stopped);
    rig.controls.retrim(to_ms);
    assert_eq!(rig.run_until(secs), RunExit::Stopped);
    let s1 = rig.status.snapshot();
    (
        s1.known_steps - s0.known_steps,
        s1.skips - s0.skips,
        s1.late_commands - s0.late_commands,
        longest_mute(&rig, 40.0),
        held_after(&rig, 40.0, &[40.0], 1.0),
    )
}

/// A known step inside a master ramp, or across a command the follower is
/// about to apply or has just applied, would play part of the source at
/// the other speed (the stage's envelope runs on output frames, not on the
/// source): the step waits for a clear stretch (the follower plays on,
/// heard) and is sized at the stage's speed then. Trim edits 5 ms → 0 (a
/// skip forward) and 0 → 5 ms (a repeat) at three moments of the ramp: the
/// command still ahead, just reached, and the ramp running. Each takes its
/// step exactly: no late command, no loop skip, the fade only, the error
/// within a frame.
#[test]
fn a_trim_step_waits_out_a_master_ramp() {
    for (from, to) in [(5.0, 0.0), (0.0, 5.0)] {
        for at in [40.025, 40.045, 40.055] {
            let (steps, skips, late, mute, held) = trim_across_a_ramp(from, to, at);
            println!(
                "trim {from} -> {to} ms at {at} s: steps {steps}, loop skips {skips}, late {late}, \
                 longest mute {:.1} ms, held peak {held:.3} frames",
                mute * 1e3
            );
            assert_eq!((steps, skips, late), (1, 0, 0), "{from} -> {to} at {at}");
            assert!(mute < STEP_MUTE_S, "{from} -> {to} at {at}: muted {mute} s");
            assert!(held < 1.0, "{from} -> {to} at {at}: {held}");
        }
    }
}

/// The master's alignment growing under a locked follower (as the
/// controller grows it when a follower runs short): a known step of the
/// master's, taken exactly, the fade only.
#[test]
fn an_alignment_change_is_an_exact_step() {
    let secs = 55.0;
    let mut rig = Rig::new(DeviceModel::usb(0.0), follower_usb(30.0, 512, 3), secs);
    rig.align_auto = false;
    rig.set_alignment(30.0);
    rig.start();
    assert_eq!(rig.run_until(40.0), RunExit::Stopped);
    let locked = rig.status.snapshot();
    assert!(locked.audible, "{locked:?}");
    let heard_from = rig.audible_changes.len();
    rig.set_alignment(33.0);
    assert_eq!(rig.run_until(secs), RunExit::Stopped);
    let snap = rig.status.snapshot();
    let mute = longest_mute(&rig, 40.0);
    let held = held_after(&rig, 40.0, &[40.0], 0.2);
    let back = resync_after(&rig, 40.0, secs);
    println!(
        "alignment 30 -> 33 ms: known steps {} -> {}, skips {} -> {}, longest mute {:.1} ms, \
         within a frame after {back:.3} s, held peak {held:.3}",
        locked.known_steps,
        snap.known_steps,
        locked.skips,
        snap.skips,
        mute * 1e3
    );
    assert_eq!(snap.known_steps, locked.known_steps + 1);
    assert_eq!(snap.skips, locked.skips, "the loop skipped");
    assert!(
        mute < STEP_MUTE_S,
        "muted {mute} s: {:?}",
        &rig.audible_changes[heard_from - 1..]
    );
    assert!(back < 0.2 && held < 1.0, "{back} {held}");
}

/// Slider edits: six +0.1 ms trims at 48 kHz ask for 28.8 frames in all.
/// The whole-frame skips carry their remainder (29 frames, not 6 × 5 =
/// 30), so the error after them stays within half a frame.
#[test]
fn small_trim_edits_add_up_exactly() {
    let secs = 50.0;
    let mut rig = Rig::new(DeviceModel::usb(0.0), follower_usb(30.0, 512, 3), secs);
    rig.align_auto = false;
    rig.set_alignment(30.0);
    rig.start();
    assert_eq!(rig.run_until(40.0), RunExit::Stopped);
    let locked = rig.status.snapshot();
    let mut t = 40.0;
    for k in 1..=6 {
        rig.controls.retrim(0.1 * f64::from(k));
        t += 0.5;
        assert_eq!(rig.run_until(t), RunExit::Stopped);
    }
    assert_eq!(rig.run_until(secs), RunExit::Stopped);
    let snap = rig.status.snapshot();
    let after: Vec<f64> = rig
        .errors
        .iter()
        .filter(|e| e.0 >= t && e.0 < t + 5.0)
        .map(|e| e.1)
        .collect();
    let mean = after.iter().sum::<f64>() / after.len().max(1) as f64;
    println!(
        "six +0.1 ms edits: known steps {} -> {}, skips {} -> {}, mean error after {mean:.3} frames",
        locked.known_steps, snap.known_steps, locked.skips, snap.skips
    );
    assert_eq!(snap.known_steps, locked.known_steps + 6);
    assert_eq!(snap.skips, locked.skips, "the loop skipped");
    assert!(mean.abs() < 0.5, "{mean}");
}

/// A whole trim swing at 192 kHz: +50 ms repeats 9600 frames, which the
/// history (sized for the trim's range) holds: the fade only, no loop
/// skip. Then −50 ms (100 ms earlier) jumps past what the master has
/// written: the follower stays shut, starved, its slack makes the
/// alignment grow (the cap has room for the whole trim range), and it is
/// heard again, aligned (never a wedge, never playing what it does not
/// have).
#[test]
fn a_full_trim_swing_finds_its_source_or_waits() {
    let secs = 75.0;
    let mut m = DeviceModel::usb(0.0);
    m.rate = 192_000.0;
    m.period = 4096;
    let mut f = follower_usb(60.0, 2048, 3);
    f.rate = 192_000.0;
    let mut rig = Rig::new(m, f, secs);
    rig.pcm.keep = true;
    rig.start();
    assert_eq!(rig.run_until(45.0), RunExit::Stopped);
    let locked = rig.status.snapshot();
    assert!(locked.audible, "{locked:?}");
    rig.controls.retrim(50.0);
    assert_eq!(rig.run_until(55.0), RunExit::Stopped);
    let back = rig.status.snapshot();
    let mute = longest_mute(&rig, 45.0);
    let held = held_after(&rig, 45.0, &[45.0], 0.2);
    rig.controls.retrim(-50.0);
    assert_eq!(rig.run_until(secs), RunExit::Stopped);
    let snap = rig.status.snapshot();
    let heard = audible_after(&rig, 55.0);
    // It did starve: shut for longer than a step's fade.
    let starved = longest_mute(&rig, 55.0);
    println!(
        "192k trim +50 ms: skips {} -> {}, longest mute {:.1} ms, held peak {held:.3}; \
         then -50 ms: shut {starved:.1} s, heard again after {heard:.1} s, lateness {} ms, \
         alignment {} ms, max step {:.3}",
        locked.skips,
        back.skips,
        mute * 1e3,
        rig.controls.lag_ms(),
        rig.align.ms,
        rig.max_step,
    );
    assert_eq!(back.skips, locked.skips, "the loop skipped");
    assert!(mute < STEP_MUTE_S, "muted {mute} s");
    assert!(held < 8.0, "{held}");
    // (Longer than a step's mute: until its source is there, 50 ms or
    // so as the alignment grows.)
    assert!(
        starved > 2.0 * STEP_MUTE_S,
        "never shut for want of source: {starved} s"
    );
    // In place once heard again (past the edit's own queued audio).
    let after = audible_peak(&rig, 55.0 + 0.1, secs);
    assert!(after < 1.0, "heard {after} frames off");
    // The cap has room for the whole trim range: aligned, not late.
    assert_eq!(rig.controls.lag_ms(), 0.0);
    assert!(starved.is_finite() && heard < 15.0, "{starved} {heard}");
    assert!(
        snap.audible && snap.state == FollowerState::Locked,
        "{snap:?}"
    );
    assert!(rig.max_step < 0.07, "a click: {}", rig.max_step);
}

/// A known step inside a ramp longer than its 1 s deadline (a master
/// ramp 1 -> 1.25 over 3 s, a +1 ms trim 0.3 s into it): it goes at the
/// deadline, under one fade, without waiting for the ramp's end or
/// fading again. Taken inside the ramp it would leave the follower
/// `k · (v_end − v) / v` frames off after it (8.4 here: the stage's
/// envelope keeps its timing while the ramp it should play moves by the
/// skip); the skip takes that in, so after the ramp the follower is
/// locked, heard and within a frame at once.
#[test]
fn a_known_step_goes_at_its_deadline_inside_a_long_ramp() {
    let secs = 70.0;
    let mut rig = Rig::new(DeviceModel::usb(0.0), follower_usb(30.0, 512, 3), secs);
    rig.align_auto = false;
    rig.set_alignment(super::super::tee::MAX_ALIGN_MS);
    rig.pcm.master.vs_tx.push(master_stage()).ok().unwrap();
    rig.start();
    assert_eq!(rig.run_until(40.0), RunExit::Stopped);
    let locked = rig.status.snapshot();
    assert!(locked.audible, "{locked:?}");
    let heard_from = rig.audible_changes.len();
    rig.pcm.master.speed = 1.25;
    rig.pcm.master.ramp_ms = 3000.0;
    assert_eq!(rig.run_until(40.3), RunExit::Stopped);
    rig.controls.retrim(1.0);
    assert_eq!(rig.run_until(43.0), RunExit::Stopped);
    let mid = rig.status.snapshot();
    let fades = rig.audible_changes[heard_from..]
        .iter()
        .filter(|c| !c.1)
        .count();
    let mute = longest_mute(&rig, 40.0);
    // (Applied: the error, against the new trim, back near zero.)
    let applied = rig
        .errors
        .iter()
        .rev()
        .take(20)
        .map(|e| e.1.abs())
        .fold(0.0f64, f64::max);
    assert_eq!(rig.run_until(secs), RunExit::Stopped);
    let snap = rig.status.snapshot();
    let back = resync_after(&rig, 43.5, secs);
    println!(
        "a step inside a 3 s ramp: known steps {} -> {}, fades {fades}, longest mute {:.1} ms, \
         error at 43 s {applied:.2} frames, loop skips {} -> {}, back within a frame {back:.1} s \
         after 43.5 s, {:?}",
        locked.known_steps,
        mid.known_steps,
        mute * 1e3,
        locked.skips,
        snap.skips,
        snap.state,
    );
    assert_eq!(mid.known_steps, locked.known_steps + 1);
    assert_eq!(fades, 1, "{:?}", &rig.audible_changes[heard_from - 1..]);
    assert!(mute < STEP_MUTE_S, "muted {mute} s");
    assert!(applied < 10.0, "not taken: {applied}");
    assert_eq!(snap.skips, locked.skips, "the loop skipped");
    assert!(back < 0.5, "{back}");
    assert!(
        snap.audible && snap.state == FollowerState::Locked,
        "{snap:?}"
    );
}

/// A `bypass` job for the rig's follower (its channel 0), a second of
/// lead-in, posted to the follower and the master.
fn follower_job(rig: &Rig, file: Vec<f64>) -> Arc<crate::engine::measure::MeasureJob> {
    use crate::engine::measure::{MeasureDevice, MeasureJob, NO_DEVICE};
    use audivane_protocol::ipc::MeasureChain;
    let rate = rig.pcm.master.rate;
    let p = rig.rt().params();
    let job = Arc::new(MeasureJob::new(
        MeasureChain::Bypass,
        true,
        0.0,
        u64::from(rate),
        p.graph_rate,
        vec![MeasureDevice::new(
            Some("hw:CARD=USB,DEV=0".into()),
            rate,
            p.device_rate,
            vec![(0, file)],
            u64::from(rate),
        )],
    ));
    rig.controls
        .measure_slot()
        .post(Some((Arc::clone(&job), 0)));
    rig.pcm
        .master
        .measure
        .post(Some((Arc::clone(&job), NO_DEVICE)));
    job
}

/// A short chirp on a follower: it reports its start, the tap holds until
/// the stage has passed its last frame (so its resampler tail never goes
/// through the chain), and `done` waits for the DAC.
#[test]
fn a_short_chirp_on_a_follower_starts_and_drains() {
    let secs = 45.0;
    let mut rig = Rig::new(DeviceModel::usb(0.0), follower_usb(60.0, 512, 3), secs);
    rig.start();
    rig.run_until(35.0);
    let rate = f64::from(rig.pcm.master.rate);
    // 20 ms, 1 to 20 kHz.
    let n = (0.02 * rate) as usize;
    let file: Vec<f64> = (0..n)
        .map(|k| {
            let t = k as f64 / rate;
            0.5 * (2.0 * PI * (1000.0 * t + 0.5 * 19_000.0 / 0.02 * t * t)).sin()
        })
        .collect();
    let job = follower_job(&rig, file);
    let d = &job.devices[0];
    let mut fed_at = None;
    while rig.pcm.now < 40.0 && !d.done.load(Ordering::Acquire) {
        rig.run_until(rig.pcm.now + 0.005);
        if fed_at.is_none() && d.fed.load(Ordering::Acquire) {
            fed_at = Some(rig.pcm.now);
            assert!(!d.done.load(Ordering::Acquire), "done before it drained");
        }
    }
    assert_eq!(job.any_fault(), crate::engine::measure::MeasureFault::None);
    assert!(d.done.load(Ordering::Acquire), "drained");
    let start = d.start_frame().expect("a start frame");
    let last = d.last_frame().expect("a last frame");
    // The last frame: the file, the crossfades and the stage's tail.
    assert!(last > start + n as f64, "{start} .. {last}");
    assert!(d.start_time_ns.load(Ordering::Acquire) > 0);
}

/// A step close to full scale into the follower's drift resampler: what
/// reaches its DAC stays at the measurement's ceiling.
#[test]
fn a_follower_step_stays_under_the_ceiling() {
    use crate::engine::measure::CEILING;
    let secs = 42.0;
    let mut rig = Rig::new(DeviceModel::usb(0.0), follower_usb(60.0, 512, 3), secs);
    rig.pcm.keep = true;
    rig.start();
    rig.run_until(35.0);
    let rate = rig.pcm.master.rate as usize;
    // Steps between +0.99 and -0.99 every 10 ms.
    let file: Vec<f64> = (0..rate)
        .map(|k| {
            if (k / (rate / 100)).is_multiple_of(2) {
                0.99
            } else {
                -0.99
            }
        })
        .collect();
    let from = rig.pcm.written_audio.len() / CH;
    let job = follower_job(&rig, file);
    rig.run_until(39.0);
    let d = &job.devices[0];
    assert!(d.fed.load(Ordering::Acquire));
    let ceiling = (CEILING * 2_147_483_648.0).round() as i32 + 1;
    let peak = rig.pcm.written_audio[from * CH..]
        .iter()
        .map(|x| x.abs())
        .max()
        .unwrap();
    assert!(peak <= ceiling, "{peak} over {ceiling}");
    assert!(
        d.clamped.load(Ordering::Relaxed) > 0,
        "the resampler overshoots"
    );
}

/// A measurement on a locked follower (`bypass`, its channel 0): the file
/// plays at its stage input by source index, so on its DAC it sits where
/// its drift-corrected map puts the source (the file frame `k` at source
/// position `s0 + k`), at the follower's volume; its other channel is muted
/// with the source.
#[test]
fn a_measurement_on_a_follower_plays_where_its_map_says() {
    let secs = 50.0;
    let mut rig = Rig::new(DeviceModel::usb(0.0), follower_usb(60.0, 512, 3), secs);
    rig.pcm.keep = true;
    rig.start();
    rig.run_until(35.0);
    let snap = rig.status.snapshot();
    assert!(
        snap.state == FollowerState::Locked && snap.audible,
        "{snap:?}"
    );
    let rate = rig.pcm.master.rate;
    let n = 2 * rate as usize;
    let freq = 500.0;
    let file: Vec<f64> = (0..n)
        .map(|k| 0.25 * (2.0 * PI * freq * k as f64 / f64::from(rate)).sin())
        .collect();
    let job = follower_job(&rig, file);
    // Checked as it plays (the map keeps recent frames only).
    let s0 = loop {
        rig.run_until(rig.pcm.now + 0.01);
        if let Some(s0) = job.s0() {
            break s0 as f64;
        }
    };
    let (mut err, mut sig, mut checked, mut other) = (0.0, 0.0, 0, 0.0f64);
    let mut first = None;
    let mut done_to = rig.pcm.written_audio.len() / CH;
    while rig.pcm.now < 41.0 {
        rig.run_until(rig.pcm.now + 0.05);
        let frames = rig.pcm.written_audio.len() / CH;
        for f in done_to..frames {
            let Some(p) = rig.rt().source_at(f as u64) else {
                continue;
            };
            let k = p as f64 / FRAME as f64 - s0;
            if k >= 0.0 && first.is_none() {
                first = Some(f as f64 - k);
            }
            if k < 200.0 || k > (n - 200) as f64 {
                continue;
            }
            let want = 0.25 * (2.0 * PI * freq * k / f64::from(rate)).sin();
            let got = f64::from(rig.pcm.written_audio[f * CH]) / 2_147_483_648.0;
            err += (got - want).powi(2);
            sig += want * want;
            checked += 1;
            other =
                other.max((f64::from(rig.pcm.written_audio[f * CH + 1]) / 2_147_483_648.0).abs());
        }
        done_to = frames;
    }
    let d = &job.devices[0];
    assert!(d.done.load(Ordering::Acquire), "the follower played it all");
    assert_eq!(job.any_fault(), crate::engine::measure::MeasureFault::None);
    let rel = (err / sig.max(1e-30)).sqrt();
    println!(
        "measurement on the follower: {checked} frames, {:.1} dB off the map, start frame {:?} (map {first:?})",
        20.0 * rel.log10(),
        d.start_frame()
    );
    assert!(checked > n / 2, "{checked} frames checked");
    assert!(rel < 1e-3, "the file is off the map: {rel:e}");
    assert!(other < 1e-6, "the other channel is muted: {other}");
    let start = d.start_frame().unwrap();
    assert!((start - first.unwrap()).abs() < 1.0, "{start} vs {first:?}");
    assert!(d.start_time_ns.load(Ordering::Acquire) > 0);
    // Taken back, the source plays again.
    job.release.store(true, Ordering::Release);
    rig.run_until(43.0);
    assert!(job.released.load(Ordering::Acquire));
}

/// A chain swap on the master (the processor's queue).
fn swap_master(rig: &mut Rig, chain: ActiveChain, seq: u64) {
    rig.pcm
        .master
        ._keep
        .1
        .push(PendingSwap {
            chain,
            crossfade: true,
            generation: seq,
            swap_seq: seq,
        })
        .ok()
        .unwrap();
}

/// Compensation on, without a chain of its own: the pad stays at zero and
/// the follower's output is bit for bit what it is without compensation.
#[test]
fn a_follower_without_its_own_chain_plays_bit_for_bit_as_before() {
    let run = |compensate: bool| {
        let mut rig = Rig::new(DeviceModel::usb(0.0), follower_usb(60.0, 512, 3), 40.0);
        rig.controls.set_compensate(compensate);
        rig.pcm.keep = true;
        rig.start();
        assert_eq!(rig.run_until(40.0), RunExit::Stopped);
        let sn = rig.status.snapshot();
        (rig.pcm.written_audio.clone(), sn, rig.comp.frames)
    };
    let (plain, a, _) = run(false);
    let (padded, b, comp) = run(true);
    assert_eq!(b.state, FollowerState::Locked);
    assert_eq!((b.pad_frames, comp), (0, 0));
    assert_eq!(a.skips, b.skips);
    assert_eq!(plain.len(), padded.len());
    assert!(plain == padded, "the output differs");
}

/// The master loads a chain with latency (a 5 ms limiter) the follower's
/// own chain does not have: the follower pads its path by that much (a
/// muted step), the loop never sees it, and it plays in place, heard.
#[test]
fn a_longer_master_chain_pads_the_follower() {
    let secs = 60.0;
    let mut rig = Rig::new(DeviceModel::usb(0.0), follower_usb(60.0, 512, 3), secs);
    rig.controls.set_compensate(true);
    rig.start();
    assert_eq!(rig.run_until(30.0), RunExit::Stopped);
    let before = rig.status.snapshot();
    assert!(before.audible, "{before:?}");
    swap_master(&mut rig, eq_chain_with(48_000, -1.0, true), 1);
    assert_eq!(rig.run_until(secs), RunExit::Stopped);
    let sn = rig.status.snapshot();
    println!("{sn:?} comp {}", rig.comp.frames);
    assert_eq!(sn.pad_frames, 240, "the pad: the master's limiter");
    assert_eq!(rig.comp.frames, 0);
    assert_eq!(sn.skips, before.skips, "the loop re-placed it");
    assert!(sn.audible && sn.state == FollowerState::Locked, "{sn:?}");
    let after = outcome(&rig, 40.0, 40.0);
    assert!(after.steady_peak < 1.0, "{after:?}");
    // Muted for the step only (the pad's settle and its fades).
    let muted: f64 = rig
        .audible_changes
        .windows(2)
        .filter(|w| w[0].0 >= 30.0 && !w[0].1)
        .map(|w| w[1].0 - w[0].0)
        .sum();
    assert!(muted < 1.0, "muted {muted} s: {:?}", rig.audible_changes);
}

/// The follower's own chain gets longer (a limiter only it has): the
/// master's compensation delays the master by that much, the follower is
/// shut until it applies, and the two stay together without a skip.
#[test]
fn a_longer_follower_chain_delays_the_master() {
    let secs = 60.0;
    let mut rig = Rig::new(DeviceModel::usb(0.0), follower_usb(60.0, 512, 3), secs);
    rig.controls.set_compensate(true);
    rig.start();
    assert_eq!(rig.run_until(30.0), RunExit::Stopped);
    let before = rig.status.snapshot();
    assert!(before.audible, "{before:?}");
    rig.swaps
        .incoming
        .push(eq_chain_with(48_000, -1.0, true))
        .ok()
        .unwrap();
    assert_eq!(rig.run_until(secs), RunExit::Stopped);
    let sn = rig.status.snapshot();
    println!("{sn:?} comp {}", rig.comp.frames);
    assert_eq!(rig.comp.frames, 240, "the master waits for the limiter");
    assert_eq!(
        rig.pcm
            .master
            .host
            .shared()
            .comp_frames
            .load(Ordering::Relaxed),
        240
    );
    assert_eq!(sn.pad_frames, 0);
    assert_eq!(sn.skips, before.skips, "the loop re-placed it");
    assert!(sn.audible && sn.state == FollowerState::Locked, "{sn:?}");
    let after = outcome(&rig, 40.0, 40.0);
    assert!(after.steady_peak < 1.0, "{after:?}");
    // And back: the limiter goes, so does the compensation.
    let mut rig2 = rig;
    rig2.swaps.incoming.push(eq_chain(48_000)).ok().unwrap();
    let end = secs + 20.0;
    assert_eq!(rig2.run_until(end), RunExit::Stopped);
    let sn = rig2.status.snapshot();
    assert_eq!((rig2.comp.frames, sn.pad_frames), (0, 0));
    assert_eq!(sn.skips, before.skips);
    assert!(sn.audible, "{sn:?}");
}

/// The same edit on both sides (no chain of its own): the master's swap and
/// the follower's land a little apart, the target comes back by itself, and
/// nothing moves: no pad, no compensation, no skip.
#[test]
fn an_edit_on_both_sides_moves_no_compensation() {
    let secs = 50.0;
    let mut rig = Rig::new(DeviceModel::usb(0.0), follower_usb(60.0, 512, 3), secs);
    rig.controls.set_compensate(true);
    rig.start();
    assert_eq!(rig.run_until(30.0), RunExit::Stopped);
    let before = rig.status.snapshot();
    swap_master(&mut rig, eq_chain_with(48_000, -1.0, true), 1);
    assert_eq!(rig.run_until(30.1), RunExit::Stopped);
    rig.swaps
        .incoming
        .push(eq_chain_with(48_000, -1.0, true))
        .ok()
        .unwrap();
    assert_eq!(rig.run_until(secs), RunExit::Stopped);
    let sn = rig.status.snapshot();
    assert_eq!((rig.comp.frames, sn.pad_frames), (0, 0));
    assert_eq!(sn.skips, before.skips);
    assert!(sn.audible && sn.state == FollowerState::Locked, "{sn:?}");
}

/// A need beyond the pad's most: the target is the most (so the pad opens
/// once there, never shut waiting for a length it cannot reach), and the
/// rest is reported; a longer own chain the master's compensation cannot
/// cover is late, one it still covers shuts it meanwhile.
#[test]
fn a_pad_need_beyond_its_most_is_clamped_and_reported() {
    let p = pad_plan(1500.0, 1000, true);
    assert_eq!((p.target, p.short, p.late_frames), (1000, false, -500.0));
    let mut pad = OutPad::new(2, 1000, 8);
    pad.set(p.target, p.short);
    let mut x = vec![1; 2 * 2000];
    pad.process(&mut x);
    assert!(pad.open(), "shut at its most");
    assert_eq!(
        pad_plan(400.4, 1000, true),
        PadPlan {
            target: 400,
            short: false,
            late_frames: 0.0
        }
    );
    assert_eq!(
        pad_plan(-300.0, 1000, true),
        PadPlan {
            target: 0,
            short: true,
            late_frames: 0.0
        }
    );
    assert_eq!(
        pad_plan(-300.0, 1000, false),
        PadPlan {
            target: 0,
            short: false,
            late_frames: 300.0
        }
    );
    // Reported as unaligned, by that much.
    let f = FollowerConfig::new("hw:CARD=USB,DEV=0");
    let status = FollowerStatus::default();
    status.set_state(FollowerState::Locked);
    status.audible.store(true, Ordering::Relaxed);
    status.comp_late_ms.store(bits(6.25), Ordering::Relaxed);
    let v = view(&f, Some(&status.snapshot()), 0.0, None, None);
    assert_eq!(
        (v.state, v.offset_ms),
        (ipc::FollowerState::Unaligned, 6.25)
    );
}

#[test]
fn the_pad_delays_exactly_and_fades_around_a_change() {
    let mut pad = OutPad::new(2, 100, 8);
    let input: Vec<i32> = (0..400).map(|i| (i / 2 + 1) * 1000).collect();
    // Open at 0: unchanged.
    let mut x = input[..40].to_vec();
    assert!(!pad.process(&mut x));
    assert_eq!(x, input[..40]);
    // To 10 frames: fades out, switches at silence, fades back in, then
    // plays the input 10 frames late.
    pad.set(10, false);
    let mut y = input[40..400].to_vec();
    assert!(pad.process(&mut y));
    assert_eq!(pad.delay(), 10);
    assert!(pad.open());
    let tail = &y[y.len() - 40..];
    let want = &input[400 - 40 - 20..400 - 20];
    assert_eq!(tail, want);
    // Muted, it shuts and stays shut.
    pad.set(10, true);
    let mut z = vec![5000; 40];
    pad.process(&mut z);
    assert!(z[z.len() - 4..].iter().all(|&v| v == 0), "{z:?}");
}

/// An `active` measurement on the master and a follower together, with the
/// master's compensation non-zero (the follower's own chain is a limiter
/// longer): on the master, file frame 0 plays where its music plays source
/// frame `s0` (after the alignment and the compensation), so the two
/// devices start it at the same source frame.
#[test]
fn a_joint_active_measurement_starts_at_s0_on_the_compensated_master() {
    use crate::engine::measure::{MeasureDevice, MeasureJob};
    use audivane_protocol::ipc::MeasureChain;
    let secs = 50.0;
    let mut rig = Rig::new(DeviceModel::usb(0.0), follower_usb(60.0, 512, 3), secs);
    rig.controls.set_compensate(true);
    rig.pcm.master.keep_output = true;
    rig.start();
    rig.swaps
        .incoming
        .push(eq_chain_with(48_000, -1.0, true))
        .ok()
        .unwrap();
    assert_eq!(rig.run_until(35.0), RunExit::Stopped);
    let delay = rig.pcm.master.proc.tee().delay_frames();
    assert_eq!(rig.comp.frames, 240);
    assert!(delay >= 240, "{delay}");
    let rate = rig.pcm.master.rate;
    let n = rate as usize / 2;
    let file: Vec<f64> = (0..n)
        .map(|k| 0.25 * (2.0 * PI * 500.0 * k as f64 / f64::from(rate)).sin())
        .collect();
    let p = rig.rt().params();
    let job = Arc::new(MeasureJob::new(
        MeasureChain::Active,
        false,
        0.0,
        u64::from(rate),
        p.graph_rate,
        vec![
            MeasureDevice::new(None, rate, rate, vec![(0, file.clone())], u64::from(rate)),
            MeasureDevice::new(
                Some("hw:CARD=USB,DEV=0".into()),
                rate,
                p.device_rate,
                vec![(0, file)],
                u64::from(rate),
            ),
        ],
    ));
    rig.pcm.master.measure.post(Some((Arc::clone(&job), 0)));
    rig.controls
        .measure_slot()
        .post(Some((Arc::clone(&job), 1)));
    let from = rig.pcm.master.output.len() / CH;
    let s0 = loop {
        assert!(rig.pcm.now < 45.0, "the measurement never started");
        rig.run_until(rig.pcm.now + 0.05);
        if let Some(s0) = job.s0()
            && job.devices[0].start_frame().is_some()
            && rig.pcm.master.output.len() / CH > from + 3 * rate as usize
        {
            break s0;
        }
    };
    assert_eq!(job.any_fault(), crate::engine::measure::MeasureFault::None);
    let out = &rig.pcm.master.output;
    // Where the file starts on channel 0: after the silent lead-in.
    let mut run = 0;
    let mut start = None;
    for f in from..out.len() / CH {
        if out[f * CH] == 0 {
            run += 1;
        } else {
            if run > rate as usize / 2 {
                start = Some(f);
                break;
            }
            run = 0;
        }
    }
    // (File frame 0 is sin 0: the first frame that is not zero is frame 1.)
    let start = start.expect("no file after the lead-in") - 1;
    // The music on the other channel (chain A: −1 dB, the tone inverted)
    // at that frame is source frame s0, not s0 + the delay.
    let g = 10f64.powf(-1.0 / 20.0);
    let err_at = |shift: i64| {
        let mut e = 0.0;
        for j in 0..2000usize {
            let got = f64::from(out[(start + j) * CH + 1]) / 2_147_483_648.0;
            let want = -g * f64::from(tone((s0 as i64 + shift + j as i64) as u64, rate));
            e += (got - want).powi(2);
        }
        (e / 2000.0).sqrt()
    };
    let here = err_at(0);
    let early = err_at(delay as i64);
    println!("delay {delay}, error at s0 {here:e}, at s0 + delay {early:e}");
    assert!(
        here < 1e-4,
        "file frame 0 is not at s0: {here:e} (at s0 + delay: {early:e})"
    );
    // And the file is there on channel 0.
    let x = f64::from(out[(start + 120) * CH]) / 2_147_483_648.0;
    let want = g * 0.25 * (2.0 * PI * 500.0 * 120.0 / f64::from(rate)).sin();
    assert!((x - want).abs() < 1e-3, "{x} vs {want}");
}

/// What a measurement waits for: the alignment holds 3 s after a change
/// and while a follower is short of source; the compensation while what it
/// would take differs from what it has.
#[test]
fn the_alignment_and_compensation_report_when_they_settle() {
    let mut a = AlignControl::default();
    // No alignment, plenty of source: settled.
    assert_eq!(a.settling(10.0, 5.0), None);
    assert_eq!(a.settling(10.0, f64::NAN), None);
    // Short of source: it is about to grow.
    assert!(a.settling(10.0, 0.5).unwrap().contains("grows"));
    assert_eq!(a.tick(10.0, 0.5), Some(2.0));
    assert!(a.settling(11.0, 5.0).unwrap().contains("changed"));
    assert!(a.settling(12.9, 5.0).is_some());
    assert_eq!(a.settling(13.0, 5.0), None);
    let mut c = CompControl::default();
    assert!(!c.settling(Some(0.0), 48_000));
    assert!(c.settling(Some(240.0), 48_000));
    c.tick(0.0, Some(240.0), 48_000);
    assert!(c.settling(Some(240.0), 48_000), "not applied yet");
    assert_eq!(c.tick(1.0, Some(240.0), 48_000), Some(240));
    assert!(!c.settling(Some(240.0), 48_000));
    // (Capped: what it can take.)
    assert!(!c.settling(Some(1e9), 240));
}

/// An `active` measurement on a follower whose chain gets longer while it
/// plays: the file would cross from one chain to the other, and its pad
/// then shuts until the master's compensation follows; the run faults at
/// the swap (`ChainSwap`) instead of ending clean with a broken capture.
#[test]
fn a_follower_shut_during_an_active_measurement_faults_it() {
    use crate::engine::measure::{MeasureDevice, MeasureFault, MeasureJob, NO_DEVICE};
    use audivane_protocol::ipc::MeasureChain;
    let secs = 50.0;
    let mut rig = Rig::new(DeviceModel::usb(0.0), follower_usb(60.0, 512, 3), secs);
    rig.controls.set_compensate(true);
    rig.start();
    assert_eq!(rig.run_until(30.0), RunExit::Stopped);
    assert!(rig.status.snapshot().audible);
    let rate = rig.pcm.master.rate;
    let p = rig.rt().params();
    let n = 3 * rate as usize;
    let file: Vec<f64> = (0..n)
        .map(|k| 0.25 * (2.0 * PI * 500.0 * k as f64 / f64::from(rate)).sin())
        .collect();
    let job = Arc::new(MeasureJob::new(
        MeasureChain::Active,
        true,
        0.0,
        u64::from(rate),
        p.graph_rate,
        vec![MeasureDevice::new(
            Some("hw:CARD=USB,DEV=0".into()),
            rate,
            p.device_rate,
            vec![(0, file)],
            u64::from(rate),
        )],
    ));
    rig.controls
        .measure_slot()
        .post(Some((Arc::clone(&job), 0)));
    rig.pcm
        .master
        .measure
        .post(Some((Arc::clone(&job), NO_DEVICE)));
    // Into the file, then a longer chain on the follower only.
    while job.devices[0].pos.load(Ordering::Relaxed) < i64::from(rate) / 2 {
        assert!(rig.pcm.now < 40.0, "the measurement never started");
        rig.run_until(rig.pcm.now + 0.01);
    }
    assert_eq!(job.any_fault(), MeasureFault::None);
    rig.swaps
        .incoming
        .push(eq_chain_with(48_000, -1.0, true))
        .ok()
        .unwrap();
    rig.run_until(40.0);
    // (The master's compensation then moves too: its own fault comes
    // first in the run's error.)
    assert_eq!(job.devices[0].fault(), MeasureFault::ChainSwap);
}

/// Heard at the DAC after `from_s` (`pcm.keep`), seconds after it
/// (infinity: never).
fn heard_after(rig: &Rig, from_s: f64) -> f64 {
    rig.at_dac
        .iter()
        .find(|x| x.0 >= from_s && x.2)
        .map_or(f64::INFINITY, |x| x.0 - from_s)
}

/// The largest error (frames) heard at the DAC over `[from_s, to_s)`.
fn audible_peak(rig: &Rig, from_s: f64, to_s: f64) -> f64 {
    rig.at_dac
        .iter()
        .filter(|x| x.2 && x.0 >= from_s && x.0 < to_s)
        .fold(0.0f64, |a, x| a.max(x.1.abs()))
}

/// A locked follower through the master's pauses (its stream stops, its
/// DSP state resets, a new stream starts): heard again within a second of
/// each resume, in place from its first audible frame (within the
/// placement floor, 0.05 ms), and in sync after.
#[test]
fn a_locked_follower_is_heard_at_once_after_a_pause() {
    let secs = 100.0;
    let mut rig = Rig::new(DeviceModel::usb(0.0), follower_usb(60.0, 512, 3), secs);
    rig.pcm.keep = true;
    let pauses = [(40.0, 45.0), (55.0, 55.7), (65.0, 85.0)];
    rig.pcm.pauses = pauses.to_vec();
    rig.start();
    assert_eq!(rig.run_until(secs), RunExit::Stopped);
    let floor = 0.05e-3 * 48_000.0;
    for (i, &(_, back)) in pauses.iter().enumerate() {
        let next = pauses.get(i + 1).map_or(secs, |p| p.0);
        let heard = heard_after(&rig, back);
        let first = audible_peak(&rig, back, back + heard + 1.0);
        let all = audible_peak(&rig, back, next);
        println!(
            "resume at {back}: heard after {heard:.3} s, peak {first:.2} frames over its first second, {all:.2} to {next}"
        );
        assert!(heard < 1.0, "resume at {back}: heard after {heard}");
        assert!(first < floor, "resume at {back}: {first} frames off");
        assert!(all < floor, "resume at {back}: {all} frames off");
        let mutes = dac_changes(&rig, back + heard)
            .iter()
            .filter(|c| c.0 < next && !c.1)
            .count();
        assert_eq!(mutes, 0, "resume at {back}: muted again");
    }
    let snap = rig.status.snapshot();
    println!("{snap:?}");
    assert!(snap.state == FollowerState::Locked && snap.audible);
    assert!(rig.max_step < 0.07, "a click: {}", rig.max_step);
}

/// A reopen of a follower that locked before starts from its remembered
/// rate: across a rate change (the master's stream stops, the follower is
/// rebuilt, the master's new stream starts) and an unplug while the master
/// plays, it is heard within a second of the master's (or its own) return,
/// in place from its first audible frame, without an acquisition.
#[test]
fn a_reopened_follower_is_placed_from_its_remembered_rate() {
    let secs = 80.0;
    let mut rig = Rig::new(DeviceModel::usb(0.0), follower_usb(-35.0, 512, 3), secs);
    rig.pcm.keep = true;
    // The rate change: the master stops at 40 s, the follower reopens at
    // 40.3 s, the master plays again at 40.6 s.
    rig.pcm.pauses = vec![(40.0, 40.6)];
    rig.pcm.gone = Some((60.0, 61.0));
    rig.start();
    assert_eq!(rig.run_until(40.3), RunExit::Stopped);
    let before = rig.status.snapshot();
    assert!(before.state == FollowerState::Locked, "{before:?}");
    assert!(rig.controls.remembered_rate(48_000).is_some());
    rig.reopen_at(40.3);
    loop {
        match rig.run_until(secs) {
            RunExit::Stopped => break,
            RunExit::Lost(_) => rig.reopen_at(61.0),
            RunExit::Rebuild => panic!("rebuild"),
        }
    }
    let floor = 0.05e-3 * 48_000.0;
    for (name, back, next) in [("rate change", 40.6, 60.0), ("unplug", 61.0, secs)] {
        let heard = heard_after(&rig, back);
        let first = audible_peak(&rig, back, back + heard + 1.0);
        let all = audible_peak(&rig, back, next);
        println!(
            "{name}: heard after {heard:.3} s, peak {first:.2} frames over its first second, {all:.2} to {next}"
        );
        assert!(heard < 1.0, "{name}: heard after {heard}");
        assert!(all < floor, "{name}: {all} frames off");
    }
    let snap = rig.status.snapshot();
    assert!(
        snap.state == FollowerState::Locked && snap.audible,
        "{snap:?}"
    );
    assert!((snap.ppm + 35.0).abs() < 2.0, "{}", snap.ppm);
    assert!(rig.max_step < 0.07, "a click: {}", rig.max_step);
}

/// The remembered rate is kept per rate family: a DAC may have a crystal
/// for 48 kHz and another for 44.1 kHz.
#[test]
fn the_remembered_rate_is_per_rate_family() {
    let c = FollowerControls::new(0.0, 0.0);
    assert_eq!(c.remembered_rate(48_000), None);
    c.remember_rate(96_000, 3e-5);
    assert_eq!(c.remembered_rate(48_000), Some(3e-5));
    assert_eq!(c.remembered_rate(192_000), Some(3e-5));
    assert_eq!(c.remembered_rate(44_100), None);
    c.remember_rate(88_200, -1e-5);
    assert_eq!(c.remembered_rate(44_100), Some(-1e-5));
    assert_eq!(c.remembered_rate(384_000), Some(3e-5));
}

/// Muted time after it was first heard, from `from_s` (seconds), and the
/// mutes.
fn muted_after_heard(rig: &Rig, from_s: f64) -> (f64, usize) {
    let c = dac_changes(rig, from_s);
    let (mut total, mutes) = (0.0, c.iter().filter(|x| !x.1).count());
    for (i, x) in c.iter().enumerate() {
        if !x.1 {
            let back = c[i + 1..].iter().find(|y| y.1).map_or(rig.pcm.now, |y| y.0);
            total += back - x.0;
        }
    }
    (total, mutes)
}

/// Where what its DAC plays turns heard or silent ([`Rig::at_dac`]),
/// from the first heard at or after `from_s`.
fn dac_changes(rig: &Rig, from_s: f64) -> Vec<(f64, bool)> {
    let mut c: Vec<(f64, bool)> = Vec::new();
    for x in rig.at_dac.iter().filter(|x| x.0 >= from_s) {
        if c.last().map_or(x.2, |l| l.1 != x.2) {
            c.push((x.0, x.2));
        }
    }
    c
}

const FLOOR_48K: f64 = 0.05e-3 * 48_000.0;

/// A pair never seen before (a fresh install): the follower is placed at
/// the nominal rate from its first readings and heard within the first
/// second whatever its clock, inside the first start's wider floor
/// (0.25 ms) while its loop learns the rate; once the readings' slope is
/// known its rate is taken and it is within the placement floor from then
/// on.
#[test]
fn a_fresh_pair_is_heard_from_the_start_and_stays_in_place() {
    const FIRST: f64 = 0.25e-3 * 48_000.0;
    for (ppm, period) in [
        (50.0, 128),
        (100.0, 256),
        (-100.0, 256),
        (50.0, 512),
        (-50.0, 512),
        (100.0, 512),
        (-100.0, 512),
        (100.0, 1024),
        (-100.0, 1024),
        (100.0, 2048),
        (-100.0, 2048),
    ] {
        let secs = 40.0;
        let mut rig = Rig::new(DeviceModel::usb(0.0), follower_usb(ppm, period, 3), secs);
        rig.pcm.keep = true;
        // (No alignment preset: it starts at what the pair is expected to
        // need, a 2048-frame follower late by the rest.)
        rig.start();
        assert_eq!(rig.run_until(secs), RunExit::Stopped);
        let heard = heard_after(&rig, 0.0);
        let peak = audible_peak(&rig, 0.0, secs);
        // From when on it is heard within the floor.
        let settled = rig
            .at_dac
            .iter()
            .rev()
            .find(|x| x.2 && x.1.abs() >= FLOOR_48K)
            .map_or(heard, |x| x.0);
        let (muted, mutes) = muted_after_heard(&rig, 0.0);
        let snap = rig.status.snapshot();
        println!(
            "fresh {ppm:+} ppm, {period}-frame periods: heard after {heard:.3} s, peak {peak:.2} \
             frames, within the floor from {settled:.2} s, muted {muted:.3} s in {mutes} mutes, \
             ppm {:.2}",
            snap.ppm
        );
        assert!(heard < 1.0, "{ppm} {period}: heard after {heard}");
        assert!(peak < FIRST, "{ppm} {period}: {peak} frames off");
        assert!(
            settled < 6.0,
            "{ppm} {period}: outside the floor at {settled} s"
        );
        assert!(
            muted < 0.5 && mutes <= 2,
            "{ppm} {period}: muted {muted} s in {mutes}"
        );
        assert!(
            snap.state == FollowerState::Locked && snap.audible,
            "{snap:?}"
        );
        assert!((snap.ppm - ppm).abs() < 2.0, "{}", snap.ppm);
        assert!(rig.max_step < 0.07, "a click: {}", rig.max_step);
    }
}

/// A daemon restart: what the last run's loop learnt, through the disk,
/// places the follower of a new process at once, from the master's first
/// readings, at the remembered rate (no acquisition).
#[test]
fn a_restart_places_the_follower_from_the_persisted_rate() {
    let mut first = Rig::new(DeviceModel::usb(0.0), follower_usb(-35.0, 512, 3), 70.0);
    first.start();
    assert_eq!(first.run_until(70.0), RunExit::Stopped);
    let learnt = [first.controls.memory_at(0), first.controls.memory_at(1)];
    assert!(learnt[0].eps.is_some(), "{learnt:?}");
    let dir = std::env::temp_dir().join(format!("audivane-restart-{}", std::process::id()));
    let path = dir.join("follower-clocks.json");
    let mut store = super::super::clocks::ClockStore::open(path.clone());
    store.note("hw:CARD=Master,DEV=0", "hw:CARD=USB,DEV=0", learnt);
    store.note_alignment("hw:CARD=Master,DEV=0", first.align.ms);
    store.save(true);
    let reopened = super::super::clocks::ClockStore::open(path);
    let back = reopened.get("hw:CARD=Master,DEV=0", "hw:CARD=USB,DEV=0");
    let align = reopened.alignment("hw:CARD=Master,DEV=0").unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(back[0].eps.is_some());
    let secs = 30.0;
    let mut rig = Rig::new(DeviceModel::usb(0.0), follower_usb(-35.0, 512, 3), secs);
    rig.pcm.keep = true;
    rig.controls.install_memory(back);
    // The engine starts the master at the remembered alignment.
    rig.align.start_at(0.0, align);
    rig.set_alignment(align);
    rig.start();
    assert_eq!(rig.run_until(secs), RunExit::Stopped);
    let heard = heard_after(&rig, 0.0);
    let peak = audible_peak(&rig, 0.0, secs);
    let (muted, mutes) = muted_after_heard(&rig, 0.0);
    let snap = rig.status.snapshot();
    println!(
        "restart: heard after {heard:.3} s, peak {peak:.2} frames, muted {muted:.3} s in {mutes}, ppm {:.2}",
        snap.ppm
    );
    // (At its DAC: the render's flag is a queue earlier.)
    assert!(heard < 0.6, "heard after {heard}");
    assert!(peak < FLOOR_48K, "{peak} frames off");
    assert_eq!(mutes, 0);
    assert!((snap.ppm + 35.0).abs() < 1.0, "{}", snap.ppm);
}

/// The master moves to another DAC (its clock 45 ppm from the last one's
/// against the follower): the follower does not carry the last pair's
/// rate, it starts from the new pair's (here never seen: the nominal
/// rate), heard within a second of the new master's start, in place.
#[test]
fn a_master_change_starts_from_the_new_pairs_clock() {
    let secs = 80.0;
    let mut rig = Rig::new(DeviceModel::usb(0.0), follower_usb(60.0, 512, 3), secs);
    rig.pcm.keep = true;
    rig.pcm.pauses = vec![(40.0, 40.5)];
    rig.start();
    assert_eq!(rig.run_until(40.2), RunExit::Stopped);
    assert!(rig.status.snapshot().state == FollowerState::Locked);
    // The new master: another clock, and the host's memory for the new
    // pair (none).
    rig.pcm.master.dev.clock = audivane_dsp_core::drift::sim::ClockModel::fixed(15.0);
    rig.controls.install_memory([ClockMemory::default(); 2]);
    assert_eq!(rig.run_until(secs), RunExit::Stopped);
    let heard = heard_after(&rig, 40.5);
    let peak = audible_peak(&rig, 40.5, secs);
    let (muted, mutes) = muted_after_heard(&rig, 40.5);
    let snap = rig.status.snapshot();
    println!(
        "master change: heard after {heard:.3} s, peak {peak:.2} frames, muted {muted:.3} s in {mutes}, ppm {:.2}",
        snap.ppm
    );
    assert!(heard < 1.0, "heard after {heard}");
    assert!(peak < FLOOR_48K, "{peak} frames off");
    // (Placed at the nominal rate, 45 ppm off: the drift may be caught
    // before it leaves the floor, and the follower placed again.)
    assert!(muted < 1.0 && mutes <= 2, "muted {muted} s in {mutes}");
    assert!((snap.ppm - 45.0).abs() < 2.0, "{}", snap.ppm);
}

/// The host keeps each pair's clocks: a follower added starts from its
/// pair's, a master change gives each follower the new pair's (bumping
/// its generation), and the old pair's comes back with the old master.
#[test]
fn the_host_keeps_each_pairs_clocks() {
    let build: ChainBuilder = Arc::new(|_, rate, _, _| Ok(eq_chain(rate)));
    let mut host = FollowerHost::new(build, Arc::new(|_, _| RateRatio::ONE));
    let mut tee = TeeHost::detached();
    tee.shared().graph_rate.store(48_000, Ordering::Relaxed);
    let mut cfg = EngineConfig::default();
    let (a, b, f) = ("hw:CARD=A,DEV=0", "hw:CARD=B,DEV=0", "hw:CARD=Nope9,DEV=0");
    let known = ClockMemory {
        eps: Some(-4e-5),
        master_bias_s: 1e-6,
        follower_bias_s: 2e-6,
    };
    host.clocks.note(a, f, [known, ClockMemory::default()]);
    cfg.outputs.followers =
        vec![serde_json::from_value(serde_json::json!({ "device": f })).unwrap()];
    host.sync(&mut tee, a, &cfg);
    let c = host.controls(f).unwrap();
    assert_eq!(c.memory_at(0), known);
    let before = c.pair_gen.load(Ordering::Relaxed);
    host.sync(&mut tee, b, &cfg);
    let c = host.controls(f).unwrap();
    assert_eq!(c.memory_at(0), ClockMemory::default());
    assert!(c.pair_gen.load(Ordering::Relaxed) > before);
    host.sync(&mut tee, a, &cfg);
    assert_eq!(host.controls(f).unwrap().memory_at(0), known);
    host.stop_all(&mut tee);
}

/// A follower whose readings step on 1 ms packets at a phase that sweeps
/// against its wakeups (a timer-driven reader: a bias of 0 to 48 frames
/// moving over seconds or minutes). Its long-run mean (half a packet) is a
/// constant no loop can see, the trim's. The follower is heard at once, as a
/// fresh pair, up to about a packet from that mean until its phase has swept
/// a whole packet, then within the floor of it; after a pause, within a
/// second, in place.
fn sweeping_grid(name: &str, bus_ppm: f64, secs: f64, settled_s: f64, pause_s: f64) {
    let mut dev = follower_usb(60.0, 480, 3);
    dev.pointer_s = 1e-3;
    dev.free_wake = true;
    dev.bus_ppm = bus_ppm;
    let mut rig = Rig::new(DeviceModel::usb(0.0), dev, secs);
    rig.pcm.keep = true;
    rig.pcm.pauses = vec![(pause_s, pause_s + 2.0)];
    rig.start();
    assert_eq!(rig.run_until(secs), RunExit::Stopped);
    let heard = heard_after(&rig, 0.0);
    let resume = heard_after(&rig, pause_s + 2.0);
    let o = outcome(&rig, pause_s + 20.0, pause_s + 20.0);
    // From the long-run mean (the steady bias).
    let off = |from: f64, to: f64| {
        rig.at_dac
            .iter()
            .filter(|x| x.2 && x.0 >= from && x.0 < to)
            .fold(0.0f64, |a, x| a.max((x.1 - o.bias).abs()))
    };
    let before = off(0.0, settled_s);
    let after = off(settled_s, secs);
    let resume_peak = off(pause_s, secs);
    let (muted, mutes) = muted_after_heard(&rig, 0.0);
    println!(
        "{name}: long-run mean {:.2} frames; heard after {heard:.2} s, at most {before:.2} frames \
         from it until {settled_s} s, {after:.2} after; muted {muted:.2} s in {mutes}; after the \
         pause heard after {resume:.3} s, at most {resume_peak:.2}",
        o.bias
    );
    let packet = 48.0;
    assert!(heard < 1.0, "{name}: first heard after {heard}");
    assert!(
        before < packet + FLOOR_48K,
        "{name}: {before} frames from the long-run mean"
    );
    assert!(
        after < FLOOR_48K,
        "{name}: {after} frames from the long-run mean"
    );
    assert!(resume < 1.0, "{name}: heard after {resume} s");
    // (About the long-run mean.)
    let spread = (o.steady_rms.powi(2) - o.bias.powi(2)).max(0.0).sqrt();
    assert!(spread < 1.0, "{name}: {spread}: {o:?}");
}

/// [`sweeping_grid`], the phase sweeping a packet in about 5 s.
#[test]
fn a_sweeping_coarse_grid_is_heard_at_once_and_settles_in_place() {
    sweeping_grid("5 s sweep", -140.0, 150.0, 30.0, 100.0);
}

/// [`sweeping_grid`], the phase sweeping a packet in about 100 s (the
/// follower's clock 10 ppm from its bus's): a fit to a part of the sweep
/// would take the sawtooth for a clock offset.
#[test]
fn a_slowly_sweeping_coarse_grid_stays_within_a_packet_until_it_has_wrapped() {
    sweeping_grid("100 s sweep", 50.0, 360.0, 230.0, 300.0);
}

/// The master's level as the engine publishes it (`publish_volume`).
fn set_master_level(rig: &Rig, volume_db: f64, balance_db: f64, muted: bool) {
    rig.pcm
        .master
        .host
        .shared()
        .master_level
        .store(VolumeTarget {
            volume_db,
            balance_db,
            muted,
            hardware_volume: false,
            hardware_mute: false,
        });
}

/// The follower's left channel against the source at `gain_db` (`None`:
/// silence) over `[from, to)` s: its relative error (absolute, for
/// silence).
fn level_phase(rig: &mut Rig, from: f64, to: f64, gain_db: Option<f64>) -> f64 {
    rig.audio_err = 0.0;
    rig.audio_sig = 0.0;
    rig.audio_from_s = from;
    rig.gain_db = gain_db.unwrap_or(f64::NEG_INFINITY);
    assert_eq!(rig.run_until(to), RunExit::Stopped);
    match gain_db {
        Some(_) => {
            assert!(rig.audio_sig > 0.0, "no samples checked");
            (rig.audio_err / rig.audio_sig).sqrt()
        }
        None => rig.audio_err.sqrt(),
    }
}

/// A follower that follows the master moves by the master's changes from
/// the level it was set at (its offset to the master is kept, never its
/// absolute level forced to the master's), takes the master's balance and
/// mute, and stops at 0 dB; ramped like the master's (no step: a −20 dB
/// change on a −4 dB tone would jump by up to 0.4). Not following, it
/// keeps its own level.
#[test]
fn a_follower_follows_the_master_level_and_opts_out() {
    let secs = 62.0;
    let mut rig = Rig::new(DeviceModel::usb(0.0), follower_usb(25.0, 512, 3), secs);
    rig.pcm.keep = true;
    rig.align_auto = false;
    rig.set_alignment(30.0);
    // Its own −3 dB set while the master was at −20 dB.
    rig.controls.set_level(-3.0, Some(-3.0 - -20.0));
    set_master_level(&rig, -20.0, 0.0, false);
    rig.start();
    assert_eq!(rig.run_until(30.0), RunExit::Stopped);
    assert!(rig.status.snapshot().audible);
    // The EQ's −1 dB on top of its level.
    let settle = 0.1;
    let check = |rig: &mut Rig, at: f64, want: Option<f64>, what: &str| {
        let e = level_phase(rig, at + settle, at + 4.0, want);
        println!("{what}: {e:e}");
        assert!(e < 1e-3, "{what}: {e:e}");
    };
    check(
        &mut rig,
        30.0,
        Some(-4.0),
        "its own level, master at -20 dB",
    );
    set_master_level(&rig, -40.0, 0.0, false);
    check(&mut rig, 34.0, Some(-24.0), "master down 20 dB");
    set_master_level(&rig, -26.0, 0.0, false);
    check(&mut rig, 38.0, Some(-10.0), "master up 14 dB");
    // Balance to the right: the left input down by 6 dB.
    set_master_level(&rig, -26.0, 6.0, false);
    check(&mut rig, 42.0, Some(-16.0), "balance +6 dB");
    set_master_level(&rig, -26.0, 0.0, true);
    check(&mut rig, 46.0, None, "muted");
    // Raised past where it would go above 0 dB: it stops there.
    set_master_level(&rig, -5.0, 0.0, false);
    check(&mut rig, 50.0, Some(-1.0), "at 0 dB");
    // Not following: its own level, whatever the master does.
    rig.controls.set_level(-3.0, None);
    set_master_level(&rig, -30.0, 12.0, true);
    check(&mut rig, 54.0, Some(-4.0), "own level");
    rig.controls.set_level(-3.0, Some(-3.0 - -30.0));
    check(&mut rig, 58.0, None, "following again (muted)");
    assert!(rig.status.snapshot().level_db.is_finite());
    println!("max step {:.3} at {:.2} s", rig.max_step, rig.max_step_at);
    assert!(rig.max_step < 0.07, "a step: {}", rig.max_step);
}

/// Unticking while the master sits 30 dB below where the follower's level
/// was set: the control side publishes the level it plays at (−36 dB) and
/// "not following" together, so no read on the audio side ever sees the
/// old own level (−6 dB) without the offset. The writer flips between the
/// two states as fast as it can; every read plays −36 dB.
#[test]
fn an_untick_never_plays_the_old_level_for_a_block() {
    let controls = Arc::new(FollowerControls::new(0.0, -6.0));
    let master = VolumeTarget {
        volume_db: -50.0,
        ..VolumeTarget::default()
    };
    // −6 dB set at master −20: following, it plays −36 dB now.
    let following = (-6.0, Some(-6.0 - -20.0));
    let unticked = (-36.0, None);
    controls.set_level(unticked.0, unticked.1);
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let (c, stop) = (Arc::clone(&controls), Arc::clone(&stop));
        std::thread::spawn(move || {
            let mut n = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let (own, off) = if n.is_multiple_of(2) {
                    unticked
                } else {
                    following
                };
                c.set_level(own, off);
                n += 1;
            }
            n
        })
    };
    let mut loudest = f64::NEG_INFINITY;
    for _ in 0..2_000_000 {
        let (own, offset) = controls.level();
        loudest = loudest.max(heard_level(own, offset, &master).0);
    }
    stop.store(true, Ordering::Relaxed);
    let flips = writer.join().unwrap();
    println!("{flips} flips, loudest {loudest} dB");
    assert!(flips > 1000, "{flips}");
    assert!((loudest - -36.0).abs() < 1e-6, "{loudest}");
}

/// An xrun recovered inside the room query (the device left prepared, with
/// room, before the follower has written): nothing rendered before it
/// plays after the restart. Heard again within a second, in place from
/// the event on.
#[test]
fn an_xrun_recovered_in_the_room_query_plays_nothing_stale() {
    let secs = 60.0;
    let mut rig = Rig::new(DeviceModel::usb(0.0), follower_usb(-40.0, 512, 3), secs);
    rig.pcm.keep = true;
    rig.pcm.xrun = Some(40.0);
    rig.pcm.xrun_in_avail = true;
    rig.start();
    assert_eq!(rig.run_until(secs), RunExit::Stopped);
    let snap = rig.status.snapshot();
    assert_eq!(snap.xruns, 1);
    let heard = heard_after(&rig, 40.0);
    let peak = audible_peak(&rig, 40.0, secs);
    let first = audible_peak(&rig, 40.0, 40.1);
    println!(
        "xrun in the room query: heard after {heard:.3} s, peak {peak:.2} frames, {first:.2} over the first 100 ms"
    );
    assert!(peak < FLOOR_48K, "heard {peak} frames off");
    assert!(heard < 1.0, "heard after {heard}");
    // (The underrun itself cuts the sound where the device ran dry: no
    // click check here.)
}

/// A follower chain convolving with a 32768-tap filter, a direct tap and a
/// late one (frame 30000): after a signalled xrun nothing from before it
/// comes out of the filter's tail. The output stays silent until the tail
/// of what the chain heard before the xrun has passed, then fades in: no
/// step where the silence ends bigger than the signal's own from one
/// sample to the next.
#[test]
fn an_xrun_never_plays_the_convolution_tail_from_before_it() {
    const TAPS: usize = 32_768;
    const LATE: usize = 30_000;
    let secs = 45.0;
    let mut rig = Rig::new(DeviceModel::usb(0.0), follower_usb(-40.0, 512, 3), secs);
    rig.pcm.keep = true;
    rig.pcm.xrun = Some(40.0);
    rig.start();
    assert_eq!(rig.run_until(5.0), RunExit::Stopped);
    let mut ir = vec![0.0; TAPS];
    ir[0] = 0.7;
    ir[LATE] = 0.3;
    let conv = ActiveChain::Single(Box::new(Chain::new(ChainSpec {
        config: ChainConfig {
            convolution: vec![audivane_protocol::ConvolutionConfig {
                name: None,
                enabled: true,
                ir_path: "unused".into(),
                gain_db: 0.0,
                matrix: None,
                calibration: false,
            }],
            output: OutputConfig {
                bits: 32,
                dither: DitherKind::None,
                headroom_db: 0.0,
            },
            ..ChainConfig::neutral()
        },
        sample_rate: 48_000,
        channels: CH,
        max_block: 8192,
        irs: Some(vec![ir].into()),
    })));
    assert!(conv.tail_out_frames() >= LATE, "{}", conv.tail_out_frames());
    rig.swaps.incoming.push(conv).ok().unwrap();
    assert_eq!(rig.run_until(39.9), RunExit::Stopped);
    assert!(rig.status.snapshot().audible);
    assert_eq!(rig.run_until(secs), RunExit::Stopped);
    assert_eq!(rig.status.snapshot().xruns, 1);
    // The first frame written after the restart, and the frames after it
    // the tail could reach with what came before.
    let from = (rig.audio_base + rig.pcm.base as usize) * CH;
    let span = &rig.pcm.written_audio[from..from + LATE * CH];
    let first = span.iter().position(|&x| x != 0).map(|i| i / CH);
    println!("after the restart: first sound at frame {first:?} of {LATE}");
    assert_eq!(first, None, "the tail from before the xrun played");
    // The largest step between two samples (the left channel): the
    // signal's own over seconds before the xrun, and from the restart on.
    let step = |a: usize, b: usize| {
        let w = &rig.pcm.written_audio[a * CH..b * CH];
        w.as_chunks::<CH>()
            .0
            .iter()
            .map(|f| f64::from(f[0]) / 2_147_483_648.0)
            .collect::<Vec<_>>()
            .windows(2)
            .fold(0.0f64, |m, x| m.max((x[1] - x[0]).abs()))
    };
    let restart = from / CH;
    let own = step(restart - 4 * 48_000, restart - 48_000);
    let at_release = step(restart, restart + 2 * 48_000);
    println!("largest step: the signal's own {own:.4}, from the restart on {at_release:.4}");
    assert!(own > 0.0);
    assert!(
        at_release <= own * 1.05,
        "a click: {at_release} (the signal's own {own})"
    );
    let heard = heard_after(&rig, 40.0);
    assert!(heard < 2.0, "heard after {heard}");
}

/// `active` on a follower with the keepalive: it plays after the chain at
/// its level under the (silent) file, fades out once the stimulus is over,
/// and nothing of it is left from the measurement's last frame on: the
/// follower works out that frame before it mixes the keepalive, so a call
/// the end falls in carries exact zeros after it (the source is muted).
#[test]
fn a_follower_keepalive_ends_before_the_measurements_last_frame() {
    use crate::engine::measure::{MeasureDevice, MeasureJob, NO_DEVICE};
    use audivane_protocol::ipc::MeasureChain;
    let secs = 45.0;
    let mut rig = Rig::new(DeviceModel::usb(0.0), follower_usb(60.0, 512, 3), secs);
    rig.pcm.keep = true;
    rig.start();
    rig.run_until(35.0);
    assert!(rig.status.snapshot().audible);
    let rate = rig.pcm.master.rate;
    let p = rig.rt().params();
    let ka = 1e-5;
    let n = rate as usize / 2;
    let job = Arc::new(MeasureJob::new(
        MeasureChain::Active,
        true,
        ka,
        u64::from(rate),
        p.graph_rate,
        vec![MeasureDevice::new(
            Some("hw:CARD=USB,DEV=0".into()),
            rate,
            p.device_rate,
            vec![(0, vec![0.0; n])],
            u64::from(rate),
        )],
    ));
    rig.controls
        .measure_slot()
        .post(Some((Arc::clone(&job), 0)));
    rig.pcm
        .master
        .measure
        .post(Some((Arc::clone(&job), NO_DEVICE)));
    let d = &job.devices[0];
    while !d.done.load(Ordering::Acquire) {
        assert!(rig.pcm.now < 42.0, "never drained");
        rig.run_until(rig.pcm.now + 0.01);
    }
    rig.run_until(rig.pcm.now + 0.2);
    assert_eq!(job.any_fault(), crate::engine::measure::MeasureFault::None);
    let x = |f: usize| f64::from(rig.pcm.written_audio[f * CH]) / 2_147_483_648.0;
    let start = d.start_frame().unwrap() as usize;
    let last = d.last_frame().unwrap().ceil() as usize;
    let rms =
        |a: usize, b: usize| ((a..b).map(|f| x(f).powi(2)).sum::<f64>() / (b - a) as f64).sqrt();
    let under = rms(start, start + n / 2);
    assert!(
        (20.0 * (under / ka).log10()).abs() < 1.0,
        "under the file {under}"
    );
    let after: Vec<i32> = (last..last + 4_000)
        .map(|f| rig.pcm.written_audio[f * CH])
        .collect();
    assert!(
        after.iter().all(|v| *v == 0),
        "after the last frame: {:?}",
        &after[..16]
    );
    // Faded, not cut: the last 2 ms before the end carry almost nothing.
    let tail = rms(last - 96, last);
    assert!(tail < ka * 0.1, "just before the end {tail}");
}

/// An `active` measurement through a follower whose chain carries a
/// convolution, and an EQ edit on that follower while the file plays: the
/// edit takes the playing chain over in place (no warm-up), and the run
/// faults at it (`ChainSwap`), as for a swap by warm-up: the file would
/// cross the edit's fade.
#[test]
fn an_in_place_takeover_on_a_follower_faults_an_active_measurement() {
    use crate::engine::measure::{MeasureDevice, MeasureFault, MeasureJob, NO_DEVICE};
    use audivane_protocol::ipc::MeasureChain;
    const PEAK: usize = 1_440;
    const TAPS: usize = 32_768;
    let mut rig = Rig::new(DeviceModel::usb(0.0), follower_usb(60.0, 512, 3), 50.0);
    rig.pcm.keep = true;
    rig.start();
    assert_eq!(rig.run_until(30.0), RunExit::Stopped);
    // The calibration on both (a new convolution: it warms up), settled.
    rig.pcm
        .master
        ._keep
        .1
        .push(PendingSwap {
            chain: conv_chain_with(48_000, -7.0, PEAK, TAPS),
            crossfade: true,
            generation: 1,
            swap_seq: 1,
        })
        .ok()
        .unwrap();
    rig.swaps.offer(conv_chain_with(48_000, -7.0, PEAK, TAPS));
    assert_eq!(rig.run_until(34.0), RunExit::Stopped);
    rig.swaps.reap();
    let rate = rig.pcm.master.rate;
    let p = rig.rt().params();
    let n = 3 * rate as usize;
    let file: Vec<f64> = (0..n)
        .map(|k| 0.25 * (2.0 * PI * 500.0 * k as f64 / f64::from(rate)).sin())
        .collect();
    let job = Arc::new(MeasureJob::new(
        MeasureChain::Active,
        true,
        0.0,
        u64::from(rate),
        p.graph_rate,
        vec![MeasureDevice::new(
            Some("hw:CARD=USB,DEV=0".into()),
            rate,
            p.device_rate,
            vec![(0, file)],
            u64::from(rate),
        )],
    ));
    rig.controls
        .measure_slot()
        .post(Some((Arc::clone(&job), 0)));
    rig.pcm
        .master
        .measure
        .post(Some((Arc::clone(&job), NO_DEVICE)));
    while job.devices[0].pos.load(Ordering::Relaxed) < i64::from(rate) / 2 {
        assert!(rig.pcm.now < 44.0, "the measurement never started");
        rig.run_until(rig.pcm.now + 0.01);
    }
    assert_eq!(job.any_fault(), MeasureFault::None);
    // An EQ edit on the follower: taken over at once.
    let before = rig.rt().swaps_done;
    rig.swaps.offer(conv_chain_with(48_000, -8.0, PEAK, TAPS));
    rig.run_until(rig.pcm.now + 0.03);
    assert_eq!(rig.rt().swaps_done, before + 1, "taken over in place");
    assert!(matches!(rig.rt().swap, Swap::Idle), "no warm-up");
    assert_eq!(job.devices[0].fault(), MeasureFault::ChainSwap);
}

/// A two-DAC setup in time: two USB DACs at 768 kHz from a 48 kHz
/// graph (×16), the master's period the 1024-frame quantum (16384 frames
/// at 768 kHz, 21.3 ms) with three queued, the follower at
/// `period` graph frames (16 times that at its DAC) with three. The rig
/// runs the graph rate at the DACs (ratio 1): the timing is the same.
fn two_dac_rig(period: usize, ppm: f64, bus_phase: f64, secs: f64) -> Rig {
    let mut master = DeviceModel::usb(0.0);
    master.buffer_periods = 3;
    let mut follower = follower_usb(ppm, period, 3);
    follower.bus_phase = bus_phase;
    Rig::new(master, follower, secs)
}

/// The alignment comes back down: from the 82 ms high-water mark it
/// holds through music while the need is measured, a shrink to
/// the need plus 3 ms waits, and it lands in the first gap long enough
/// (the history and the margin), never in play. The follower takes the
/// master's step exactly inside the gap (muted only there, no skip), and
/// after it plays in place with its source to spare: no growth after.
#[test]
fn the_alignment_shrinks_to_the_need_in_silence_never_in_play() {
    let secs = 75.0;
    let mut rig = two_dac_rig(1024, 50.0, 0.71, secs);
    rig.pcm.master.silent = vec![(45.0, 47.0)];
    rig.set_alignment(82.0);
    rig.start();
    assert_eq!(rig.run_until(45.0), RunExit::Stopped);
    assert!(rig.align.measured_s >= SHRINK_MEASURED_S, "{:?}", rig.align);
    let before = rig.status.snapshot();
    let (need, target) = (rig.align.need_ms, rig.align.pending);
    println!(
        "82 ms in play: need {need:.1} ms, shrink waiting {target:?}, slack {:.1} (worst {:.1}) ms",
        before.slack_ms, before.slack_low_ms
    );
    assert_eq!(rig.align.ms, 82.0, "moved in play");
    assert!(rig.shrinks.is_empty(), "{:?}", rig.shrinks);
    let target = target.expect("a shrink waits");
    assert_eq!(target, (need + SHRINK_MARGIN_MS).ceil().max(0.0));
    assert!(target < 82.0 - 10.0, "{target}");
    assert!(before.audible && before.state == FollowerState::Locked);
    let heard = rig.audible_changes.len();
    assert_eq!(rig.run_until(secs), RunExit::Stopped);
    let snap = rig.status.snapshot();
    println!(
        "shrinks {:?}, then slack {:.1} (worst {:.1}) ms, known steps {} -> {}, skips {} -> {}, \
         underfeeds {} -> {}, audible changes {:?}",
        rig.shrinks,
        snap.slack_ms,
        snap.slack_low_ms,
        before.known_steps,
        snap.known_steps,
        before.skips,
        snap.skips,
        before.underfeeds,
        snap.underfeeds,
        &rig.audible_changes[heard..]
    );
    assert_eq!(rig.shrinks.len(), 1, "{:?}", rig.shrinks);
    let (t, ms) = rig.shrinks[0];
    assert_eq!(ms, target);
    assert!((45.2..47.0).contains(&t), "landed at {t} s");
    assert_eq!(rig.align.ms, target, "grew back");
    assert_eq!(snap.skips, before.skips, "the loop skipped");
    assert_eq!(snap.known_steps, before.known_steps + 1);
    assert_eq!(snap.underfeeds, before.underfeeds, "ran short");
    assert_eq!(snap.xruns, 0);
    // Muted only inside the gap (the follower's path behind it included).
    assert!(
        rig.audible_changes[heard..]
            .iter()
            .all(|c| (45.0..47.5).contains(&c.0)),
        "{:?}",
        &rig.audible_changes[heard..]
    );
    assert!(snap.audible && snap.state == FollowerState::Locked);
    // Heard, it is in place: within a frame from the step on.
    let worst = rig
        .errors
        .iter()
        .filter(|e| e.0 >= 47.5 && e.2)
        .fold(0.0f64, |a, e| a.max(e.1.abs()));
    assert!(worst < 1.0, "{worst} frames out of place");
    assert!(snap.slack_low_ms >= SHRINK_MARGIN_MS - 1.0, "{snap:?}");
}

/// A pause (the master stops: no position, no slack) keeps what the
/// follower needed, and the next stream starts at the shrunk alignment:
/// the shrink lands while nothing plays.
#[test]
fn a_shrink_lands_at_a_pause_and_the_next_stream_starts_with_it() {
    let mut a = AlignControl {
        ms: 82.0,
        ..AlignControl::default()
    };
    let snap = |state, low: f64| FollowerSnapshot {
        state,
        slack_low_ms: low,
        ..FollowerStatus::default().snapshot()
    };
    // Measured in play (after the hold): 82 − 50 = 32 ms needed; no
    // shrink before 30 s of it.
    a.note_need(1.0, need_of([snap(FollowerState::Locked, 50.0)], 82.0));
    assert!(a.need_ms.is_nan(), "within the hold");
    for t in 4..=20 {
        a.note_need(
            f64::from(t),
            need_of([snap(FollowerState::Locked, 50.0)], 82.0),
        );
    }
    assert_eq!(a.need_ms, 32.0);
    assert_eq!(a.shrink(20.0, true), None, "measured over 16 s only");
    // A pause (nothing measured) does not count as play.
    a.note_need(21.0, need_of([snap(FollowerState::Locked, f64::NAN)], 82.0));
    for t in 60..=74 {
        a.note_need(
            f64::from(t),
            need_of([snap(FollowerState::Locked, 50.0)], 82.0),
        );
    }
    assert_eq!(a.measured_s, 30.0);
    assert_eq!(a.shrink(74.0, true), Some((35.0, true)));
    assert_eq!(a.shrink(75.0, true), None, "sent once");
    // Paused: nothing measured, the need and the shrink hold.
    a.note_need(76.0, need_of([snap(FollowerState::Locked, f64::NAN)], 82.0));
    assert_eq!((a.need_ms, a.pending), (32.0, Some(35.0)));
    // The RT side applied it: landed; the need (the alignment less the
    // slack, both moved by the step) holds.
    assert!(!a.landed(77.0, 82.0));
    assert!(a.landed(77.0, 35.0));
    assert_eq!((a.ms, a.pending, a.need_ms), (35.0, None, 32.0));
    assert_eq!(a.shrink(90.0, true), None);
    // A window that measures less (a follower's worst slack restarted)
    // never lowers it; a path change forgets it.
    a.note_need(91.0, Some(10.0));
    assert_eq!(a.need_ms, 32.0);
    a.forget_need();
    assert!(a.need_ms.is_nan() && a.measured_s == 0.0);
    // A need that grows past the shrink waiting drops it (at once).
    let mut b = AlignControl {
        ms: 60.0,
        ..AlignControl::default()
    };
    for t in 5..=35 {
        b.note_need(f64::from(t), Some(20.0));
    }
    assert_eq!(b.shrink(35.0, true), Some((23.0, true)));
    b.note_need(36.0, Some(58.0));
    assert_eq!(b.shrink(36.0, true), Some((60.0, false)));
    assert_eq!(b.pending, None);
    // Not while a follower plays late; a follower opening or unmeasured
    // gives no need.
    let mut c = AlignControl {
        ms: 60.0,
        ..AlignControl::default()
    };
    for t in 5..=40 {
        c.note_need(f64::from(t), Some(10.0));
    }
    assert_eq!(c.shrink(40.0, false), None);
    assert_eq!(
        need_of(
            [
                snap(FollowerState::Locked, 10.0),
                snap(FollowerState::Opening, 10.0)
            ],
            60.0
        ),
        None
    );
    assert_eq!(
        need_of(
            [
                snap(FollowerState::Locked, 10.0),
                snap(FollowerState::Acquiring, 30.0),
                snap(FollowerState::Lost, f64::NAN)
            ],
            60.0
        ),
        Some(50.0)
    );
    // A follower that needs none: down to the margin.
    assert_eq!(shrunk_alignment_ms(-20.0), Some(0.0));
    assert_eq!(shrunk_alignment_ms(-2.5), Some(1.0));
}

/// At the default short period (5.3 ms: 256 graph frames, 4096 at
/// 768 kHz) against the master, the follower locks and stays
/// locked through every phase of the two clocks' wakeups (its clock 100
/// ppm off sweeps the phase through its period about twice): no skip after
/// the acquisition, no xrun, never short of source, within a frame, and no
/// alignment needed.
fn short_period_holds(ppm: f64, bus_phase: f64) {
    let secs = 120.0;
    let mut rig = two_dac_rig(256, ppm, bus_phase, secs);
    rig.start();
    assert_eq!(rig.run_until(20.0), RunExit::Stopped);
    let locked = rig.status.snapshot();
    assert!(locked.audible, "{locked:?}");
    assert_eq!(rig.run_until(secs), RunExit::Stopped);
    let snap = rig.status.snapshot();
    let o = outcome(&rig, 0.0, 20.0);
    println!(
        "256 x 3 at {ppm:+} ppm: {o:?}, alignment {} ms, slack {:.1} (worst {:.1}) ms, \
         latency {:.1} ms, underfeeds {} -> {}",
        rig.align.ms,
        snap.slack_ms,
        snap.slack_low_ms,
        snap.latency_ms,
        locked.underfeeds,
        snap.underfeeds
    );
    assert_eq!(snap.skips, locked.skips, "skipped after the lock");
    assert!(locked.skips <= 1, "{locked:?}");
    assert_eq!(snap.xruns, 0);
    assert_eq!(snap.underfeeds, locked.underfeeds, "ran short");
    assert!(o.steady_peak < 1.0, "{o:?}");
    assert!(
        snap.audible && snap.state == FollowerState::Locked,
        "{snap:?}"
    );
    assert_eq!(rig.align.ms, 0.0, "the master waits for it");
    assert!(snap.slack_low_ms > SHRINK_MARGIN_MS, "{snap:?}");
}

#[test]
fn a_follower_at_a_short_period_holds_at_every_phase_faster() {
    short_period_holds(100.0, 0.71);
}

#[test]
fn a_follower_at_a_short_period_holds_at_every_phase_slower() {
    short_period_holds(-80.0, 0.2);
}

/// The sim's figures: what a follower at each period
/// needs of the master's alignment (measured with the alignment held at
/// 60 ms, the worst slack at any phase), and the total latency of the
/// master's path before (the follower at the quantum's period, the
/// alignment grown from 0 by the controller) and after (the default 5.3 ms
/// period): chain (53 ms, the same both ways) + alignment +
/// the master's queue (2.5 periods of 21.3 ms on average).
#[test]
fn two_dac_latency_before_and_after() {
    let chain_ms = 53.0;
    let queue_ms = 2.5 * 1024.0 / 48.0;
    let runs: Vec<(usize, Option<f64>, f64, f64)> = std::thread::scope(|s| {
        let jobs: Vec<_> = [
            (1024, Some(60.0)),
            (512, Some(60.0)),
            (256, Some(60.0)),
            (128, Some(60.0)),
            (1024, None),
            (256, None),
        ]
        .into_iter()
        .map(|(period, held)| {
            s.spawn(move || {
                let secs = 40.0;
                let mut rig = two_dac_rig(period, 50.0, 0.71, secs);
                if let Some(ms) = held {
                    rig.set_alignment(ms);
                }
                rig.start();
                assert_eq!(rig.run_until(secs), RunExit::Stopped);
                let snap = rig.status.snapshot();
                assert!(snap.audible, "{period}: {snap:?}");
                (period, held, rig.align.ms - snap.slack_low_ms, rig.align.ms)
            })
        })
        .collect();
        jobs.into_iter().map(|j| j.join().unwrap()).collect()
    });
    for &(period, held, need, _) in runs.iter().filter(|r| r.1.is_some()) {
        println!(
            "follower period {period} graph frames ({} at 768 kHz, {:.1} ms): needs {need:.1} ms \
             (alignment {} ms)",
            period * 16,
            period as f64 / 48.0,
            held.unwrap()
        );
    }
    let grown = |p: usize| runs.iter().find(|r| r.0 == p && r.1.is_none()).unwrap().3;
    let (before, after) = (grown(1024), grown(256));
    let total = |a: f64| chain_ms + a + queue_ms;
    println!(
        "total: before {:.0} ms (alignment {before} ms; {:.0} ms at the 82 ms high-water), \
         after {:.0} ms (alignment {after} ms)",
        total(before),
        total(82.0),
        total(after)
    );
    let need = |p: usize| runs.iter().find(|r| r.0 == p && r.1.is_some()).unwrap().2;
    assert!(need(256) < need(1024) - 20.0, "{runs:?}");
    assert!(need(256) + SHRINK_MARGIN_MS <= 0.0 || after <= need(256) + SHRINK_MARGIN_MS + 1.0);
    assert!(total(after) + 30.0 < total(before), "{runs:?}");
}

/// A follower's period: the power of two of device frames nearest the
/// asked time at the rate it opens at, in graph frames.
#[test]
fn a_follower_period_is_the_nearest_power_of_two_at_its_rate() {
    let x16 = RateRatio::integer(16);
    // 5 ms: 4096 frames at 768 kHz (256 graph frames at 48 kHz), 256 at
    // 48 kHz, 4096 at 705.6 kHz.
    assert_eq!(period_graph_for(5.0, 48_000, x16), 256);
    assert_eq!(x16.scale_to_output(256), 4096);
    assert_eq!(period_graph_for(5.0, 48_000, RateRatio::ONE), 256);
    assert_eq!(period_graph_for(5.0, 44_100, x16), 256);
    assert_eq!(period_graph_for(5.0, 96_000, RateRatio::integer(4)), 512);
    // The quantum's 21.3 ms is 16384 at 768 kHz.
    assert_eq!(period_graph_for(21.3, 48_000, x16), 1024);
    // A rational landing (48 → 44.1 kHz) keeps the device period a power
    // of two.
    let down = RateRatio::new(147, 160);
    let g = period_graph_for(5.0, 48_000, down);
    assert_eq!(down.scale_to_output(g), 256);
    // Unset (NaN) is the default; never below 32 graph frames.
    assert_eq!(period_graph_for(f64::NAN, 48_000, x16), 256);
    assert_eq!(period_graph_for(0.01, 48_000, RateRatio::ONE), 32);
}

/// The drift loop's placement floor at 48 kHz (0.05 ms).
const STEP_FLOOR_48K: f64 = 0.05e-3 * 48_000.0;

/// An alignment change while the follower is heard, judged at its DAC
/// (what it plays when the master plays a frame): never heard beyond the
/// floor, whether it queues more than the master (2048 × 3, 128 ms against
/// 43 ms: the master announces the change a command lead ahead, the
/// follower shuts before it), less (the default 5 ms against 64 ms: it
/// waits for the master's jump) or as much. Growth and decrease, each an
/// exact step (no skip), heard again soon after.
fn step_heard_in_place(master_periods: usize, period: usize, from_ms: f64, by_ms: f64) {
    step_heard_in_place_with(master_periods, period, from_ms, by_ms, false);
}

/// [`step_heard_in_place`]; `comp`: the follower plays a limiter the
/// master does not (5 ms look-ahead), so the master's delay holds 240
/// frames of chain compensation besides the alignment.
fn step_heard_in_place_with(
    master_periods: usize,
    period: usize,
    from_ms: f64,
    by_ms: f64,
    comp: bool,
) {
    step_case(StepCase {
        master_periods,
        period,
        from_ms,
        by_ms,
        comp,
        ..StepCase::default()
    });
}

/// One alignment step while heard ([`step_heard_in_place`]).
#[derive(Clone, Copy)]
struct StepCase {
    master_period: usize,
    master_periods: usize,
    period: usize,
    from_ms: f64,
    by_ms: f64,
    /// A chain compensation of 240 frames (the follower's limiter).
    comp: bool,
    /// The follower's trim, ms.
    trim_ms: f64,
    /// The master's varispeed speed (1: no stage).
    speed: f64,
}

impl Default for StepCase {
    fn default() -> Self {
        Self {
            master_period: 1024,
            master_periods: 3,
            period: 256,
            from_ms: 60.0,
            by_ms: 5.0,
            comp: false,
            trim_ms: 0.0,
            speed: 1.0,
        }
    }
}

fn step_case(c: StepCase) {
    let StepCase {
        master_period,
        master_periods,
        period,
        from_ms,
        by_ms,
        comp,
        trim_ms,
        speed,
    } = c;
    let secs = 50.0;
    let mut master = DeviceModel::usb(0.0);
    master.period = master_period;
    master.buffer_periods = master_periods;
    let mut rig = Rig::new(master, follower_usb(30.0, period, 3), secs);
    rig.pcm.keep = true;
    rig.align_auto = false;
    rig.set_alignment(from_ms);
    if trim_ms != 0.0 {
        rig.controls.set_trim_ms(trim_ms);
    }
    if speed != 1.0 {
        rig.pcm.master.speed = speed;
        rig.pcm.master.vs_tx.push(master_stage()).ok().unwrap();
    }
    if comp {
        rig.controls.set_compensate(true);
    }
    rig.start();
    if comp {
        assert_eq!(rig.run_until(10.0), RunExit::Stopped);
        rig.swaps
            .incoming
            .push(eq_chain_with(48_000, -1.0, true))
            .ok()
            .unwrap();
    }
    assert_eq!(rig.run_until(40.0), RunExit::Stopped);
    if comp {
        assert_eq!(rig.comp.frames, 240, "no compensation");
    }
    let before = rig.status.snapshot();
    assert!(before.audible, "{before:?}");
    let heard_before = rig.at_dac.iter().filter(|e| e.0 >= 39.0 && e.2).count();
    assert!(heard_before > 0, "nothing heard before the step");
    rig.set_alignment(from_ms + by_ms);
    assert_eq!(rig.run_until(secs), RunExit::Stopped);
    let snap = rig.status.snapshot();
    let after: Vec<_> = rig.at_dac.iter().filter(|e| e.0 >= 39.0).collect();
    let worst = after
        .iter()
        .filter(|e| e.2)
        .fold(0.0f64, |a, e| a.max(e.1.abs()));
    // The longest the DAC was silent after the change.
    let mut silent = 0.0f64;
    let mut since: Option<f64> = None;
    for e in &after {
        match (e.2, since) {
            (false, None) => since = Some(e.0),
            (true, Some(t)) => {
                silent = silent.max(e.0 - t);
                since = None;
            }
            _ => {}
        }
    }
    println!(
        "master {master_period} x{master_periods}, follower {period} x 3, trim {trim_ms} ms, \
         speed {speed}, alignment {from_ms} -> {} ms: heard within {worst:.2} frames, silent at its DAC {:.0} ms at most, known steps {} -> {}, \
         skips {} -> {}",
        from_ms + by_ms,
        silent * 1e3,
        before.known_steps,
        snap.known_steps,
        before.skips,
        snap.skips
    );
    assert!(worst < STEP_FLOOR_48K, "heard {worst} frames off");
    assert_eq!(since, None, "silent at the end");
    assert!(silent < 0.25, "silent {silent} s");
    assert_eq!(snap.known_steps, before.known_steps + 1);
    assert_eq!(snap.skips, before.skips, "the loop skipped");
    assert!(snap.audible && snap.state == FollowerState::Locked);
}

#[test]
fn an_alignment_step_is_never_heard_out_of_place_on_a_longer_queue() {
    step_heard_in_place(2, 2048, 114.0, 5.0);
}

#[test]
fn an_alignment_step_is_never_heard_out_of_place_on_a_shorter_queue() {
    step_heard_in_place(3, 256, 60.0, 5.0);
    step_heard_in_place(3, 256, 60.0, -5.0);
}

#[test]
fn an_alignment_step_is_never_heard_out_of_place_with_compensation() {
    step_heard_in_place_with(2, 2048, 114.0, 5.0, true);
    step_heard_in_place_with(3, 256, 60.0, 5.0, true);
    step_heard_in_place_with(3, 1024, 60.0, -5.0, true);
}

#[test]
fn an_alignment_step_is_never_heard_out_of_place_on_an_equal_queue() {
    step_heard_in_place(3, 1024, 60.0, 5.0);
    step_heard_in_place(2, 1024, 80.0, -3.0);
}

/// The need is a high-water mark: a stall of the master's thread early on
/// (one wakeup 12 ms late, its queue covers it) is what the follower needed
/// at its worst. A pause (the master plays Direct: no position, the
/// follower's worst slack starts again) and the play after it measure
/// less, but the shrink in the next gap never goes below the stall's need.
/// And none goes before the need has been measured over 30 s of play.
#[test]
fn a_shrink_never_undercuts_a_stall_seen_before_a_pause() {
    let secs = 110.0;
    let mut rig = two_dac_rig(1024, 50.0, 0.71, secs);
    rig.pcm.master.stall = Some((25.0, 0.012));
    rig.pcm.master.silent = vec![(100.0, 102.0)];
    rig.set_alignment(82.0);
    rig.start();
    assert_eq!(rig.run_until(22.0), RunExit::Stopped);
    let calm = rig.align.need_ms;
    assert_eq!(rig.align.pending, None, "a shrink before 30 s of play");
    assert_eq!(rig.run_until(35.0), RunExit::Stopped);
    let stalled = rig.align.need_ms;
    let worst = rig.status.snapshot().slack_low_ms;
    println!("need {calm:.1} ms, {stalled:.1} after the stall (worst slack {worst:.1} ms)");
    assert!(
        stalled > calm + 5.0,
        "the stall did not show: {calm} {stalled}"
    );
    // The pause, and play again.
    rig.pcm.master.sink = SinkChoice::Direct;
    assert_eq!(rig.run_until(40.0), RunExit::Stopped);
    rig.pcm.master.sink = SinkChoice::Dsp;
    assert_eq!(rig.run_until(99.0), RunExit::Stopped);
    let snap = rig.status.snapshot();
    let fresh = rig.align.ms - snap.slack_low_ms;
    println!(
        "after the pause: the follower alone needs {fresh:.1} ms, kept {:.1}, shrink {:?}",
        rig.align.need_ms, rig.align.pending
    );
    assert!(snap.audible, "{snap:?}");
    assert!(
        fresh < stalled - 5.0,
        "the pause did not restart it: {fresh}"
    );
    assert_eq!(rig.align.need_ms, stalled, "the high-water mark moved");
    assert_eq!(rig.run_until(secs), RunExit::Stopped);
    assert_eq!(rig.shrinks.len(), 1, "{:?}", rig.shrinks);
    let (_, ms) = rig.shrinks[0];
    assert!(
        ms >= stalled + SHRINK_MARGIN_MS,
        "shrunk to {ms} under {stalled}"
    );
    assert!(ms < 82.0);
}

/// A trimmed follower plays the master's position that much later (or
/// earlier): it jumps at the master's jump less its trim, so it is heard
/// at its trim across the step, not on the old mapping for the trim's
/// length.
#[test]
fn an_alignment_step_is_never_heard_out_of_place_with_a_trim() {
    for (trim_ms, by_ms) in [(50.0, 1.0), (50.0, -3.0), (-30.0, 1.0), (-30.0, -3.0)] {
        step_case(StepCase {
            from_ms: 90.0,
            by_ms,
            trim_ms,
            ..StepCase::default()
        });
    }
    // A queue longer than the master's, trimmed late.
    step_case(StepCase {
        master_periods: 2,
        period: 1024,
        from_ms: 80.0,
        by_ms: 2.0,
        trim_ms: 20.0,
        ..StepCase::default()
    });
}

/// Under varispeed the lead is playback time: at 1.9× (a 256-frame
/// quantum, a follower whose stage runs well ahead) the step is scheduled
/// far enough in source for the follower to hear of it in time, and at
/// 0.5× it is not needlessly far. Off 1× the master's stage pulls source
/// in blocks of varying size (just above 1×, 258 and 259): the step lands
/// within a block of where it was announced.
#[test]
fn an_alignment_step_is_never_heard_out_of_place_under_varispeed() {
    for (speed, from_ms) in [(1.9, 90.0), (0.5, 40.0), (1.01, 60.0)] {
        step_case(StepCase {
            master_period: 256,
            period: 512,
            from_ms,
            by_ms: 2.0,
            speed,
            ..StepCase::default()
        });
    }
}

/// Slack measured at half speed is playback time of half as much source:
/// it gives no need (a shrink to it would starve the follower on a resume
/// at 1×). At 1× and faster the need is in the alignment's unit (source
/// ms): its slack times the speed.
#[test]
fn a_need_measured_slower_than_1x_never_shrinks_the_alignment() {
    let snap = |low: f64, speed: f64| FollowerSnapshot {
        state: FollowerState::Locked,
        slack_low_ms: low,
        slack_speed: speed,
        ..FollowerStatus::default().snapshot()
    };
    // 50 ms of alignment at 0.5× (100 ms of playback), 65 ms
    // of worst slack: no need, no shrink.
    assert_eq!(need_of([snap(65.0, 0.5)], 50.0), None);
    assert_eq!(need_of([snap(10.0, 1.0)], 50.0), Some(40.0));
    // At 1.9×: 10 ms of playback is 19 ms of source.
    assert_eq!(need_of([snap(10.0, 1.9)], 50.0), Some(31.0));
    // One follower at half speed hides the rest: nothing is known.
    assert_eq!(need_of([snap(10.0, 1.0), snap(65.0, 0.5)], 50.0), None);
    // The sim: the master at 0.5× throughout; the need is never taken, no
    // shrink is sent, and the alignment holds through a silent gap.
    let secs = 80.0;
    let mut rig = two_dac_rig(1024, 50.0, 0.71, secs);
    rig.pcm.master.speed = 0.5;
    rig.pcm.master.vs_tx.push(master_stage()).ok().unwrap();
    rig.pcm.master.silent = vec![(70.0, 72.0)];
    rig.set_alignment(50.0);
    rig.start();
    // (Raised to what the pair is expected to need: it is never measured.)
    let start_ms = rig.align.ms;
    assert!(start_ms >= 50.0);
    assert_eq!(rig.run_until(secs), RunExit::Stopped);
    let snap = rig.status.snapshot();
    println!(
        "at 0.5x: slack {:.1} (worst {:.1}) ms at speed {}, need {:?}, shrinks {:?}",
        snap.slack_ms, snap.slack_low_ms, snap.slack_speed, rig.align.need_ms, rig.shrinks
    );
    assert!(snap.audible && snap.slack_low_ms.is_finite(), "{snap:?}");
    assert!((snap.slack_speed - 0.5).abs() < 0.01, "{snap:?}");
    assert!(rig.align.need_ms.is_nan(), "{:?}", rig.align);
    assert!(rig.shrinks.is_empty(), "{:?}", rig.shrinks);
    assert_eq!(rig.align.ms, start_ms);
}

/// A pair never seen before, with no alignment saved: the alignment (and a
/// follower's lateness past its cap) starts at what the pair is expected to
/// need before the follower is placed, so it is heard within the first second
/// and never runs short; neither grows, and once heard it is not muted. The
/// default 5 ms period (256 frames) and a 2048-frame follower (more than the
/// cap), ±100 ppm, against the default master two periods deep.
///
/// (At 2048 frames and +100 ppm the loop's own placement skip mutes it for
/// about 40 ms at 2.9 s.)
#[test]
fn a_fresh_pair_starts_at_its_expected_need_and_never_grows() {
    let cases = [(256, 100.0), (256, -100.0), (2048, 100.0), (2048, -100.0)];
    std::thread::scope(|sc| {
        for (period, ppm) in cases {
            sc.spawn(move || fresh_start_case(period, ppm));
        }
    });
}

fn fresh_start_case(period: usize, ppm: f64) {
    let secs = 45.0;
    let mut rig = Rig::new(DeviceModel::usb(0.0), follower_usb(ppm, period, 3), secs);
    rig.pcm.keep = true;
    rig.start();
    let (align0, lag0) = (rig.align.ms, rig.controls.lag_ms());
    let (mut aligns, mut lags) = (0, 0);
    let (mut a_last, mut l_last) = (align0, lag0);
    while rig.pcm.now < secs {
        let t = rig.pcm.now + 0.05;
        if rig.run_until(t) != RunExit::Stopped {
            break;
        }
        aligns += usize::from(rig.align.ms != a_last);
        lags += usize::from(rig.controls.lag_ms() != l_last);
        (a_last, l_last) = (rig.align.ms, rig.controls.lag_ms());
    }
    let snap = rig.status.snapshot();
    let heard = heard_after(&rig, 0.0);
    let (muted, mutes) = muted_after_heard(&rig, 0.0);
    println!(
        "fresh {period} {ppm:+}: alignment {align0} ms, lateness {lag0} ms, heard after {heard:.3} s, \
         muted {muted:.3} s in {mutes}, need {:?}, underfeeds {}",
        need_of([snap.clone()], rig.align.ms),
        snap.underfeeds
    );
    // Placed at the estimate (256 frames: 4 periods and the fixed 13 ms,
    // less the master's 21.3 ms, 13 ms; 2048: 162 ms, the cap and 46 ms
    // late) plus the margin, before the follower is placed.
    let (want_align, want_lag) = if period == 256 {
        (16.0, 0.0)
    } else {
        (120.0, 46.0)
    };
    assert_eq!((align0, lag0), (want_align, want_lag));
    assert_eq!((aligns, lags), (0, 0), "{period} {ppm}: grew");
    assert!(heard < 1.0, "{period} {ppm}: heard after {heard}");
    if period == 2048 && ppm > 0.0 {
        assert!(
            mutes <= 1 && muted < 0.05,
            "{period} {ppm}: muted {muted} s in {mutes}"
        );
    } else {
        assert_eq!(mutes, 0, "{period} {ppm}: muted {muted} s");
    }
    assert_eq!(snap.underfeeds, 0);
    assert!(
        snap.audible && snap.state == FollowerState::Locked,
        "{snap:?}"
    );
    if period == 256 {
        // The estimate is conservative against what the sim measures.
        let need = need_of([snap], rig.align.ms).unwrap();
        assert!(need <= 13.0 - 2.0, "{need}");
    }
}

/// A reopen starts a late follower late by its expected need again (a new
/// stream plays from aligned): placed there, not grown into it while
/// heard. A 2048-frame follower, −100 ppm, through a rate change at 40 s
/// (the master stops, the follower reopens, the master plays again at
/// 40.6 s; its rate saved by then: placed from it). (Heard about 2.9 s
/// after the resume at this period, with the alignment preset too: the
/// loop's placement, not the lateness.)
#[test]
fn a_reopened_late_follower_starts_at_its_expected_lateness() {
    let secs = 70.0;
    let mut rig = Rig::new(DeviceModel::usb(0.0), follower_usb(-100.0, 2048, 3), secs);
    rig.pcm.keep = true;
    rig.pcm.pauses = vec![(40.0, 40.6)];
    rig.start();
    assert_eq!(rig.run_until(40.3), RunExit::Stopped);
    assert_eq!(rig.controls.lag_ms(), 46.0);
    assert!(rig.controls.remembered_rate(48_000).is_some());
    rig.reopen_at(40.3);
    let mut lags = vec![(rig.pcm.now, rig.controls.lag_ms())];
    while rig.pcm.now < secs {
        let t = rig.pcm.now + 0.05;
        if rig.run_until(t) != RunExit::Stopped {
            break;
        }
        if rig.controls.lag_ms() != lags.last().unwrap().1 {
            lags.push((rig.pcm.now, rig.controls.lag_ms()));
        }
    }
    let heard = heard_after(&rig, 40.6);
    let (muted, mutes) = muted_after_heard(&rig, 40.6 + heard);
    println!(
        "reopened: lateness {lags:?}, heard after {heard:.3} s, muted {muted:.3} s in {mutes}"
    );
    // Reset to aligned by the new stream, back to 46 ms before it is
    // heard, and no more.
    assert_eq!(lags.last().unwrap().1, 46.0, "{lags:?}");
    assert!(lags.iter().all(|l| l.0 < 40.6 + heard), "{lags:?} {heard}");
    assert!(heard.is_finite(), "never heard again");
    assert_eq!(mutes, 0, "muted {muted} s");
}

/// [`expected_need_ms`] at the engine's master (three periods of a
/// 1024-frame quantum, half a quantum for a late cycle) and the default
/// follower period: 2.3 ms, the alignment starts at 6 ms; a 1024-frame
/// follower 66 ms.
#[test]
fn the_expected_need_at_the_engine_defaults() {
    let q = 1024.0 / 48.0;
    let lead = master_lead_ms(q, 3);
    assert!((lead - 1.5 * q).abs() < 1e-9);
    let at = |frames: f64| expected_need_ms(frames / 48.0, 3, lead);
    assert!((at(256.0) - 2.33).abs() < 0.01, "{}", at(256.0));
    assert_eq!(shrunk_alignment_ms(at(256.0)), Some(6.0));
    assert!((at(1024.0) - 66.33).abs() < 0.01, "{}", at(1024.0));
    // Only up, and not once the need is measured.
    let mut a = AlignControl::default();
    assert_eq!(a.floor(0.0, Some(at(256.0))), Some(6.0));
    assert_eq!(a.floor(0.5, Some(at(1024.0))), Some(70.0));
    assert_eq!(a.floor(1.0, Some(at(256.0))), None);
    a.need_ms = 10.0;
    assert_eq!(a.floor(2.0, Some(100.0)), None);
}
