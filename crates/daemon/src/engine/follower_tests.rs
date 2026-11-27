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

/// A room calibration switched off (`bundle_enable`) mutes its followers
/// however their own `enabled` is set; a follower outside it and a
/// calibration that is on are left alone.
#[test]
fn a_calibration_switched_off_mutes_its_followers() {
    use audivane_protocol::ipc::{CalibrationConfig, CalibrationFollower, CalibrationRestore};
    let sub = "hw:CARD=USB_1,DEV=0";
    let mut cfg = EngineConfig {
        device: "hw:CARD=USB,DEV=0".into(),
        ..EngineConfig::default()
    };
    cfg.outputs.followers = vec![
        FollowerConfig::new(sub),
        FollowerConfig::new("hw:CARD=X,DEV=0"),
    ];
    assert!(!muted_by_calibration(&cfg, sub));
    cfg.calibration = Some(CalibrationConfig {
        path: "/tmp/bundle.json".into(),
        label: "desk".into(),
        design_rate_hz: 48_000,
        enabled: true,
        notes: None,
        master: None,
        master_manifest_gain_db: None,
        followers: vec![CalibrationFollower {
            device: sub.into(),
            item: audivane_protocol::ConvolutionConfig {
                name: None,
                enabled: true,
                ir_path: "/tmp/sub.wav".into(),
                gain_db: 0.0,
                matrix: None,
                calibration: true,
            },
            trim_ms: 0.0,
            manifest_gain_db: None,
            restore: CalibrationRestore::default(),
        }],
    });
    assert!(!muted_by_calibration(&cfg, sub));
    cfg.calibration.as_mut().unwrap().enabled = false;
    assert!(muted_by_calibration(&cfg, sub));
    assert!(!muted_by_calibration(&cfg, "hw:CARD=X,DEV=0"));
    assert!(!muted_by_calibration(&cfg, "hw:CARD=USB,DEV=0"));
}
