//! The tee rings, the master's position at the DAC, the command log, the
//! alignment delay and the per-device slots, offline, and the processor around
//! them (the master bit-identical and no slower with no follower).

use super::*;
use crate::engine::processor::{ActiveChain, DaemonProcessor, MeterBank, PendingSwap, RtDspCmd};
use crate::engine::varispeed::{FOLLOWED_MAX_SPEED, VarispeedRtCmd};
use audivane_dsp_core::chain::{Chain, ChainSpec};
use audivane_dsp_core::varispeed::{Varispeed, VarispeedSpec};
use audivane_io_pw::EngineAudioProcessor;
use audivane_protocol::ipc::SinkChoice;
use audivane_protocol::{ChainConfig, DitherKind, EqProfile, OutputConfig, RateRatio, RateTarget};
use std::sync::atomic::AtomicU8;
use std::time::Instant;

const RATE: u32 = 48_000;

fn eq_chain() -> ActiveChain {
    ActiveChain::Single(Box::new(Chain::new(ChainSpec {
        config: ChainConfig {
            eq: Some(EqProfile {
                name: "eq".into(),
                preamp_db: -1.0,
                ..Default::default()
            }),
            output: OutputConfig {
                bits: 32,
                dither: DitherKind::None,
                headroom_db: 0.0,
            },
            ..ChainConfig::neutral()
        },
        sample_rate: RATE,
        channels: 2,
        max_block: 8192,
        irs: None,
    })))
}

/// A ×2 upsampler (a chain with latency, at another output rate).
fn x2_chain() -> ActiveChain {
    ActiveChain::Single(Box::new(Chain::new(ChainSpec {
        config: ChainConfig {
            upsampler: audivane_dsp_core::resample::ManualConfig {
                target: RateTarget::Ratio(2),
                taps: 256,
                ..Default::default()
            }
            .into_upsampler_config()
            .unwrap(),
            output: OutputConfig {
                bits: 32,
                dither: DitherKind::None,
                headroom_db: 0.0,
            },
            ..ChainConfig::neutral()
        },
        sample_rate: RATE,
        channels: 2,
        max_block: 8192,
        irs: None,
    })))
}

fn stage() -> Box<Varispeed> {
    Box::new(Varispeed::new(spec()).unwrap())
}

fn spec() -> VarispeedSpec {
    VarispeedSpec {
        sample_rate: RATE,
        channels: 2,
        max_frames: 8192,
        speed: 1.0,
    }
}

fn tone(i: u64) -> f32 {
    (0.4 * (2.0 * std::f64::consts::PI * 997.0 * i as f64 / f64::from(RATE)).sin()) as f32
}

/// The processor, driven as the driver does (`varispeed()` → `plan` →
/// the planned source frames → `process_block`, then the commands).
struct Rig {
    proc: DaemonProcessor,
    vs_tx: rtrb::Producer<VarispeedRtCmd>,
    host: Option<TeeHost>,
    ratio: RateRatio,
    /// Source frames handed in.
    src: u64,
    pos: u64,
    /// Device frames output.
    out_frames: u64,
    speed: f64,
    ramp_ms: f64,
    limit: f64,
    /// The last block's `(n, n_src)` (n_src = n without a stage).
    last: (usize, usize),
    out: Vec<i32>,
    _keep: (
        rtrb::Producer<RtDspCmd>,
        rtrb::Producer<PendingSwap>,
        rtrb::Consumer<ActiveChain>,
        rtrb::Consumer<Box<Varispeed>>,
    ),
}

impl Rig {
    fn new(chain: ActiveChain, ratio: RateRatio, tee: bool) -> Self {
        let (cmd_tx, cmd_rx) = rtrb::RingBuffer::new(8);
        let (swap_tx, swap_rx) = rtrb::RingBuffer::<PendingSwap>::new(8);
        let (recycle_tx, recycle) = rtrb::RingBuffer::new(8);
        let (vs_tx, vs_rx) = rtrb::RingBuffer::new(8);
        let (vs_recycle_tx, vs_recycle) = rtrb::RingBuffer::new(8);
        let mut proc = DaemonProcessor::new(
            chain,
            true,
            2,
            8192,
            16,
            RATE,
            Arc::new(AtomicU8::new(0)),
            MeterBank::new(2),
            cmd_rx,
            swap_rx,
            recycle_tx,
        )
        .with_varispeed(vs_rx, vs_recycle_tx, 2);
        let host = if tee {
            let (host, rt) = TeeHost::pair(2);
            proc = proc.with_tee(rt);
            Some(host)
        } else {
            None
        };
        Self {
            proc,
            vs_tx,
            host,
            ratio,
            src: 0,
            pos: 0,
            out_frames: 0,
            speed: 1.0,
            ramp_ms: 0.0,
            limit: 2.0,
            last: (0, 0),
            out: Vec::new(),
            _keep: (cmd_tx, swap_tx, recycle, vs_recycle),
        }
    }

    fn host(&mut self) -> &mut TeeHost {
        self.host.as_mut().unwrap()
    }

    fn install_stage(&mut self) {
        assert!(self.vs_tx.push(VarispeedRtCmd::Install(stage())).is_ok());
    }

    /// One block of `n` nominal frames of `src(i)` on `sink`.
    fn block_on(&mut self, sink: SinkChoice, n: usize, src: &dyn Fn(u64) -> f32) -> &[i32] {
        let ratio = self.ratio;
        self.proc.poll_updates(sink, RATE, ratio, true);
        let mut n_src = n;
        if let Some(port) = self.proc.varispeed(sink, ratio, RATE, 2) {
            n_src = port.plan(n);
            port.set_fill(n_src, false);
            port.set_speed_limit(self.limit);
            port.set_speed(self.speed, self.ramp_ms);
            let _ = port.peak_until_settled();
            port.pulled();
        }
        let input: Vec<f32> = (0..n_src as u64)
            .flat_map(|i| {
                let x = src(self.src + i);
                [x, -x]
            })
            .collect();
        let n_out = if ratio.is_unity() {
            n
        } else {
            ratio.frames_out(self.pos, n)
        };
        self.out = vec![0_i32; n_out * 2];
        self.proc.process_block(
            sink,
            &input,
            &input,
            &mut self.out,
            2,
            ratio,
            RATE,
            self.pos,
        );
        self.src += n_src as u64;
        self.pos += n as u64;
        self.out_frames += n_out as u64;
        self.last = (n, n_src);
        &self.out
    }

    fn block(&mut self, n: usize, src: &dyn Fn(u64) -> f32) -> &[i32] {
        self.block_on(SinkChoice::Dsp, n, src)
    }

    fn reader(&mut self, device: &str) -> TeeReader {
        self.host()
            .slots_mut()
            .get_mut(device)
            .unwrap()
            .reader
            .take()
            .unwrap()
    }
}

fn master_cfg() -> EngineConfig {
    EngineConfig::default()
}

const SIZES: [usize; 7] = [256, 1024, 64, 511, 2048, 1, 300];

// --- the ring -------------------------------------------------------------

// --- the position slot and the log ----------------------------------------

#[test]
fn the_position_slot_is_never_torn() {
    let slot = Arc::new(MasterPositionSlot::default());
    let w = Arc::clone(&slot);
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let s2 = Arc::clone(&stop);
    let writer = std::thread::spawn(move || {
        let mut k: i64 = 0;
        while !s2.load(Ordering::Relaxed) {
            // Every field follows from k.
            w.publish(MasterPoint {
                pos: i128::from(k) * FRAME + i128::from(k),
                time_ns: k,
                speed: k as f64,
                ramping: k % 2 == 1,
                epoch: k as u64,
                shift: k,
            });
            k += 1;
        }
    });
    // Until enough reads have settled, or a few seconds (a loaded machine
    // may not run the writer for a while).
    let mut seen = 0;
    let t = Instant::now();
    while seen < 20_000 && t.elapsed().as_secs_f64() < 5.0 {
        if let Some(p) = slot.read() {
            let k = p.time_ns;
            assert_eq!(p.pos, i128::from(k) * FRAME + i128::from(k));
            assert_eq!(p.speed, k as f64);
            assert_eq!(p.ramping, k % 2 == 1);
            assert_eq!(p.epoch, k as u64);
            assert_eq!(p.shift, k);
            seen += 1;
        }
    }
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();
    assert!(seen > 0);
}

// --- the delay line -------------------------------------------------------

fn interleave(f: impl Fn(u64) -> f32, from: u64, n: usize) -> Vec<f32> {
    (0..n as u64)
        .flat_map(|i| [f(from + i), 0.5 * f(from + i)])
        .collect()
}

#[test]
fn a_restarted_history_plays_silence_then_fades_in_without_stale_audio() {
    let mut d = AlignDelay::new(2, 1000);
    d.set_delay(300, 50);
    let dc = |_: u64| 0.5f32;
    for k in 0..10 {
        let x = interleave(dc, k * 100, 100);
        d.process(&x, 2, true);
    }
    // Direct for a while: unchanged blocks, then the history restarts.
    let x = interleave(|_| 0.9, 0, 64);
    assert_eq!(d.process(&x, 2, false), &x[..]);
    let mut out = Vec::new();
    for k in 0..6 {
        let x = interleave(|_| 0.25, k * 100, 100);
        out.extend(d.process(&x, 2, true).iter().step_by(2).copied());
    }
    assert!(out[..300].iter().all(|&y| y == 0.0), "stale audio");
    // Then a fade-in, monotonic, to the input.
    assert!(out[300..350].windows(2).all(|w| w[1] >= w[0]));
    assert!(out[350..].iter().all(|&y| y == 0.25));
}

// --- the per-device slots -------------------------------------------------

// --- the processor --------------------------------------------------------

/// Run `blocks` blocks on a rig with the stage (speed changes on the way)
/// or without, returning every output sample.
fn run_master(rig: &mut Rig, stage: bool, blocks: usize) -> Vec<i32> {
    if stage {
        rig.install_stage();
    }
    let mut all = Vec::new();
    for b in 0..blocks {
        match b {
            30 => {
                rig.speed = 1.3;
                rig.ramp_ms = 80.0;
            }
            70 => {
                rig.speed = 0.7;
                rig.ramp_ms = 0.0;
            }
            100 => rig.limit = 1.2,
            _ => {}
        }
        let n = SIZES[b % SIZES.len()];
        all.extend_from_slice(rig.block(n, &tone));
    }
    all
}

#[test]
fn with_no_follower_the_master_is_bit_identical() {
    type Make = fn() -> ActiveChain;
    let cases: [(Make, RateRatio, bool); 3] = [
        (eq_chain, RateRatio::ONE, true),
        (eq_chain, RateRatio::ONE, false),
        (x2_chain, RateRatio::new(2, 1), false),
    ];
    for (make, ratio, stage) in cases {
        let reference = run_master(&mut Rig::new(make(), ratio, false), stage, 140);
        // The tee slot, no follower.
        let idle = run_master(&mut Rig::new(make(), ratio, true), stage, 140);
        assert!(idle == reference, "no follower, stage {stage}");
        // A follower with no alignment: the tee only reads the source.
        let mut rig = Rig::new(make(), ratio, true);
        rig.host()
            .add_follower("hw:CARD=USB,DEV=0", &master_cfg())
            .unwrap();
        let mut reader = rig.reader("hw:CARD=USB,DEV=0");
        let teed = run_master(&mut rig, stage, 140);
        assert!(
            teed == reference,
            "a follower at zero alignment, stage {stage}"
        );
        assert!(reader.available() > 0);
        let (mut a, mut b) = (vec![0.0; 1 << 17], vec![0.0; 1 << 17]);
        assert!(reader.read(&mut [&mut a, &mut b]).is_some());
    }
}

/// Feed an impulse at source frame `at`, find its peak in the output, and
/// read the DAC there: the published position is the impulse's.
fn position_error(rig: &mut Rig, at: u64, n: usize) -> (f64, MasterPoint) {
    let impulse = move |i: u64| if i == at { 0.8 } else { 0.0 };
    // The impulse's energy centroid in device frames (a linear-phase
    // response: its group delay), over the blocks that carry it.
    let (mut e, mut m) = (0.0f64, 0.0f64);
    let mut quiet = 0;
    while quiet < 2 {
        rig.block(n, &impulse);
        let start = rig.out_frames - (rig.out.len() / 2) as u64;
        let mut energy = 0.0;
        for (f, s) in rig.out.as_chunks::<2>().0.iter().enumerate() {
            let v = f64::from(s[0]).powi(2);
            energy += v;
            m += v * (start + f as u64) as f64;
        }
        e += energy;
        if e > 0.0 && energy < 1e-12 * e {
            quiet += 1;
        }
    }
    let centroid = m / e;
    let frame = centroid.floor();
    let delay = rig.out_frames as i64 - frame as i64;
    rig.proc.dac_read(delay, 1_000_000);
    let slot = &rig.host.as_ref().unwrap().shared().position;
    let p = slot.read().expect("a position");
    assert_eq!(p.time_ns, 1_000_000);
    // Between device frames the map moves at the speed (per output frame
    // at the chain's ratio).
    let per_frame = p.speed / rig.ratio.as_f64();
    let pos = p.pos as f64 / FRAME as f64 + (centroid - frame) * per_frame;
    (pos - at as f64, p)
}

#[test]
fn the_published_position_is_the_source_frame_at_the_dac() {
    // (chain, ratio, stage speed or none, alignment ms, tolerance frames)
    type Make = fn() -> ActiveChain;
    // The ×2 chain reports its delay in whole output frames: its half
    // frame (a quarter source frame) stays, a constant like a DAC's own.
    let cases: [(Make, RateRatio, Option<f64>, f64, f64); 8] = [
        (eq_chain, RateRatio::ONE, None, 0.0, 1e-9),
        (x2_chain, RateRatio::new(2, 1), None, 0.0, 0.26),
        (eq_chain, RateRatio::ONE, None, 10.0, 1e-9),
        (eq_chain, RateRatio::ONE, Some(1.0), 0.0, 0.01),
        (eq_chain, RateRatio::ONE, Some(1.5), 7.0, 0.01),
        (eq_chain, RateRatio::ONE, Some(0.5), 0.0, 0.01),
        (eq_chain, RateRatio::ONE, Some(2.0), 0.0, 0.01),
        (x2_chain, RateRatio::new(2, 1), Some(1.25), 3.0, 0.32),
    ];
    for (make, ratio, speed, align_ms, tol) in cases {
        let mut rig = Rig::new(make(), ratio, true);
        rig.host()
            .add_follower("hw:CARD=USB,DEV=0", &master_cfg())
            .unwrap();
        rig.host().set_alignment_ms(align_ms).unwrap();
        if let Some(s) = speed {
            rig.install_stage();
            rig.speed = s;
        }
        // Settle: the stage's start and ramp, the alignment's fade.
        for k in 0..240 {
            rig.block(SIZES[k % SIZES.len()], &|_| 0.0);
        }
        let at = rig.src + 1500;
        let (err, p) = position_error(&mut rig, at, 333);
        assert!(
            err.abs() <= tol,
            "{speed:?} at {align_ms} ms, ratio {ratio:?}: {err:.3} frames off"
        );
        // Followed, the master's speed is capped at 1.996.
        let want = speed.unwrap_or(1.0).min(FOLLOWED_MAX_SPEED);
        assert_eq!(p.speed, want);
        assert!(!p.ramping);
        // The latency report carries the alignment.
        let lat = rig.proc.latency_out_frames(SinkChoice::Dsp) as f64;
        let align = ms_frames(align_ms, RATE) as f64 * ratio.as_f64() / want;
        assert!(lat >= align.round(), "{lat} < {align}");
    }
}

#[test]
fn the_alignment_delays_the_master_and_never_plays_stale_audio() {
    let mut reference = Rig::new(eq_chain(), RateRatio::ONE, false);
    let mut rig = Rig::new(eq_chain(), RateRatio::ONE, true);
    rig.host()
        .add_follower("hw:CARD=USB,DEV=0", &master_cfg())
        .unwrap();
    let mut refs = Vec::new();
    let mut outs = Vec::new();
    for k in 0..60 {
        if k == 10 {
            assert_eq!(rig.host().set_alignment_ms(150.0), Ok(MAX_ALIGN_MS));
        }
        refs.extend(reference.block(480, &tone).iter().step_by(2).copied());
        outs.extend(rig.block(480, &tone).iter().step_by(2).copied());
    }
    // The cap (120 ms = 5760 frames), after the fade.
    let d = max_align_frames(RATE);
    // In the total latency, not in the filter delay.
    let total = rig.proc.latency_out_frames(SinkChoice::Dsp);
    let align = rig.proc.align_out_frames(SinkChoice::Dsp);
    assert_eq!(align, d);
    assert_eq!(
        total - align,
        reference.proc.latency_out_frames(SinkChoice::Dsp)
    );
    let from = 10 * 480 + 240;
    for i in from + d..outs.len() {
        assert!(
            (outs[i] - refs[i - d]).abs() <= 1,
            "frame {i}: {} against {}",
            outs[i],
            refs[i - d]
        );
    }
    let loud = refs.iter().map(|x| x.abs()).max().unwrap();
    assert!(outs.iter().all(|x| x.abs() <= loud + 1));
    // Off the DSP path and back: silence, then a fade-in, never stale.
    for _ in 0..4 {
        rig.block_on(SinkChoice::Direct, 480, &|_| 0.9);
    }
    let mut back = Vec::new();
    for _ in 0..16 {
        back.extend(rig.block(480, &tone).iter().step_by(2).copied());
    }
    assert!(back[..d].iter().all(|&x| x == 0));
    assert!(back[d + 240..].iter().any(|&x| x != 0));
    // The last follower gone: the delay fades out and retires.
    rig.host().remove_follower("hw:CARD=USB,DEV=0").unwrap();
    let mut reaped = 0;
    for _ in 0..10 {
        rig.block(480, &tone);
        reaped += rig.host().reap();
    }
    assert_eq!(rig.proc.tee().delay_frames(), 0);
    assert!(reaped >= 1);
    assert_eq!(rig.proc.tee().stash_len(), 0);
}
