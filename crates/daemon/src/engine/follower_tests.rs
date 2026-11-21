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
    _keep: (
        rtrb::Producer<RtDspCmd>,
        rtrb::Producer<PendingSwap>,
        rtrb::Consumer<ActiveChain>,
        rtrb::Consumer<Box<Varispeed>>,
    ),
    /// Where its retired stretches go.
    _stretch_recycle: rtrb::Consumer<Box<audivane_dsp_core::tempo::TempoStretch>>,
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
        .with_tee(rt);
        Self {
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
            for i in 0..n_src as u64 {
                let x = tone(self.src + i, rate);
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
        self.block();
        self.k += 1;
        self.next_wake = self
            .dev
            .wake_after((self.k * self.dev.period as u64) as f64, &mut self.rng)
            .max(t);
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
    restart: bool,
    gone: Option<(f64, f64)>,
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
            restart: false,
            gone: None,
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
        while self.master.next_wake <= t {
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
        if let Some(at) = self.xrun
            && t >= at
        {
            // An underrun the driver recovered: the buffer is dropped and
            // the device waits for a prime.
            self.xrun = None;
            self.restart = true;
            self.base = self.written;
            self.running = false;
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
            max_step: 0.0,
            max_step_at: 0.0,
            audible_changes: Vec::new(),
            last_sample: 0.0,
            audio_base: 0,
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
                let p = p as f64 / FRAME as f64;
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
            if self.align_auto {
                let slack = self.status.snapshot().slack_ms;
                if let Some(ms) = self.align.tick(self.pcm.now, slack) {
                    self.set_alignment(ms);
                }
                if self.align.settled_at_cap(self.pcm.now)
                    && let Some(ms) =
                        self.lag
                            .tick(self.pcm.now, &self.status.snapshot(), &self.controls)
                {
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
        prime_device(&mut self.pcm, self.rt.as_mut().unwrap()).unwrap();
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
        println!("{name}: back in sync after {sync:.1} s, heard after {heard:.1} s");
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
    let mut host = FollowerHost::new(
        build,
        Arc::new(|_, _| RateRatio::ONE),
        Arc::new(|_, _| 1024),
    );
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
fn the_protocol_view_names_the_state() {
    use audivane_protocol::ipc::{FollowerConfig, FollowerState as W};
    let f = FollowerConfig::new("hw:CARD=USB,DEV=0");
    let status = FollowerStatus::default();
    let snap = |state, audible| {
        status.set_state(state);
        status.audible.store(audible, Ordering::Relaxed);
        status.snapshot()
    };
    let state = |s: Option<&FollowerSnapshot>, lag| view(&f, s, lag, None).state;
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
    let late = view(&f, Some(&snap(FollowerState::Locked, true)), 6.0, None);
    assert_eq!((late.state, late.offset_ms), (W::Unaligned, 6.0));
    // Not started: why, and no NaN on the wire.
    let off = view(&f, None, 0.0, Some("no thread"));
    assert_eq!(off.error.as_deref(), Some("no thread"));
    let fresh = view(&f, Some(&FollowerStatus::default().snapshot()), 0.0, None);
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
    let v = view(&f, Some(&snap), lag, None);
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
/// alignment and its lateness grow, and it is heard again (never a
/// wedge, never playing what it does not have).
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
    assert!(starved > 0.5, "never shut for want of source: {starved} s");
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
