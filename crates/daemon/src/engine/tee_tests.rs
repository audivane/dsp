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

#[test]
fn the_ring_carries_frames_with_their_index_and_flags_a_dropped_block() {
    let (mut w, mut r) = tee_ring(2, 100);
    let block = |from: u64, n: usize| -> Vec<f32> {
        (0..n as u64)
            .flat_map(|i| [(from + i) as f32, -((from + i) as f32)])
            .collect()
    };
    let mut a = vec![0.0; 100];
    let mut b = vec![0.0; 100];
    assert!(r.read(&mut [&mut a, &mut b]).is_none());
    assert!(w.write(1000, &block(1000, 40)));
    assert!(w.write(1040, &block(1040, 40)));
    // No room for 40 more: dropped, and counted.
    assert!(!w.write(1080, &block(1080, 40)));
    assert_eq!(r.stats().dropped_frames(), 40);
    // Reads stop at a short output, and continue.
    let c = r.read(&mut [&mut a[..50], &mut b[..50]]).unwrap();
    assert_eq!(
        c,
        TeeChunk {
            index: 1000,
            frames: 50,
            gap: false
        }
    );
    assert_eq!((a[0], b[0], a[49]), (1000.0, -1000.0, 1049.0));
    // Room again: the next block follows the gap.
    assert!(w.write(1120, &block(1120, 30)));
    let c = r.read(&mut [&mut a, &mut b]).unwrap();
    assert_eq!(
        c,
        TeeChunk {
            index: 1050,
            frames: 30,
            gap: false
        }
    );
    let c = r.read(&mut [&mut a, &mut b]).unwrap();
    assert_eq!(
        c,
        TeeChunk {
            index: 1120,
            frames: 30,
            gap: true
        }
    );
    assert_eq!((a[0], b[29]), (1120.0, -1149.0));
    assert!(r.read(&mut [&mut a, &mut b]).is_none());
    assert_eq!(r.available(), 0);
}

// --- the position slot and the log ----------------------------------------

#[test]
fn the_position_slot_publishes_and_invalidates() {
    let slot = MasterPositionSlot::default();
    assert_eq!(slot.read(), None);
    let p = MasterPoint {
        pos: -(5 * FRAME) - 3,
        time_ns: 123_456_789,
        speed: 1.25,
        ramping: true,
        epoch: 7,
        shift: -3,
    };
    slot.publish(p);
    assert_eq!(slot.read(), Some(p));
    slot.invalidate(8);
    assert_eq!(slot.read(), None);
    assert_eq!(slot.epoch(), 8);
}

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

#[test]
fn the_log_keeps_its_entries_until_overwritten() {
    let log = CommandLog::default();
    assert_eq!(log.get(0), LogRead::NotYet);
    let cmds = [
        VsCommand::Reset {
            speed: 1.0,
            limit: 2.0,
            ramp_ms: 50.0,
        },
        VsCommand::Speed {
            speed: 1.5,
            ramp_ms: 200.0,
        },
        VsCommand::Limit(1.25),
        VsCommand::Off,
        VsCommand::Lost,
    ];
    for (i, c) in cmds.iter().enumerate() {
        log.append(-(i as i128) * FRAME, *c);
    }
    assert_eq!(log.head(), 5);
    for (i, c) in cmds.iter().enumerate() {
        assert_eq!(
            log.get(i as u64),
            LogRead::Entry(LoggedCommand {
                seq: i as u64,
                at: -(i as i128) * FRAME,
                cmd: *c
            })
        );
    }
    assert_eq!(log.get(5), LogRead::NotYet);
    for i in 5..(LOG_LEN as u64 + 2) {
        log.append(i as i128, VsCommand::Limit(1.0));
    }
    assert_eq!(log.get(0), LogRead::Lost);
    assert_eq!(log.get(1), LogRead::Lost);
    assert!(matches!(log.get(2), LogRead::Entry(_)));
}

// --- the delay line -------------------------------------------------------

fn interleave(f: impl Fn(u64) -> f32, from: u64, n: usize) -> Vec<f32> {
    (0..n as u64)
        .flat_map(|i| [f(from + i), 0.5 * f(from + i)])
        .collect()
}

#[test]
fn the_delay_passes_unchanged_at_zero_and_delays_exactly_after_its_fade() {
    let mut d = AlignDelay::new(2, 1000);
    let mut t = 0u64;
    for &n in &SIZES {
        let x = interleave(tone, t, n);
        assert_eq!(d.process(&x, 2, true), &x[..], "bit for bit at zero");
        t += n as u64;
    }
    d.set_delay(480, 240);
    let mut out = Vec::new();
    let start = t;
    for _ in 0..20 {
        let x = interleave(tone, t, 100);
        out.extend_from_slice(d.process(&x, 2, true));
        t += 100;
    }
    // After the fade: the input 480 frames back, exactly.
    for i in 240..2000 {
        let src = start + i as u64 - 480;
        assert_eq!(out[2 * i], tone(src), "frame {i}");
        assert_eq!(out[2 * i + 1], 0.5 * tone(src));
    }
    // Never louder than the input during the fade.
    assert!(out.iter().all(|x| x.abs() <= 0.4 + 1e-6));
    // A cap above the line's length.
    d.set_delay(5000, 10);
    assert_eq!(d.delay(), 1000);
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

#[test]
fn follower_slots_are_keyed_by_device_and_follow_the_master_config() {
    let mut h = TeeHost::detached();
    let mut cfg = master_cfg();
    h.add_follower("hw:CARD=USB,DEV=0", &cfg).unwrap();
    h.add_follower("hw:CARD=B,DEV=0", &cfg).unwrap();
    assert!(h.add_follower("hw:CARD=USB,DEV=0", &cfg).is_err());
    h.add_follower("hw:CARD=C,DEV=0", &cfg).unwrap();
    assert!(
        h.add_follower("hw:CARD=D,DEV=0", &cfg).is_err(),
        "at most 3"
    );
    let slots: Vec<usize> = ["hw:CARD=USB,DEV=0", "hw:CARD=B,DEV=0", "hw:CARD=C,DEV=0"]
        .iter()
        .map(|d| h.slots().get(d).unwrap().slot)
        .collect();
    assert_eq!(slots, [0, 1, 2]);
    // A chain edit reaches every slot.
    cfg.chain.eq = Some(EqProfile {
        name: "edit".into(),
        preamp_db: -4.0,
        ..Default::default()
    });
    h.master_config_changed(&cfg);
    for d in h.slots().devices() {
        assert_eq!(h.slots().get(d).unwrap().chain_config, cfg);
    }
    // A removed follower's ring index is reused.
    h.remove_follower("hw:CARD=B,DEV=0").unwrap();
    h.add_follower("hw:CARD=D,DEV=0", &cfg).unwrap();
    assert_eq!(h.slots().get("hw:CARD=D,DEV=0").unwrap().slot, 1);
    assert_eq!(h.set_alignment_ms(75.0), Ok(MAX_ALIGN_MS));
    assert_eq!(h.set_alignment_ms(-1.0), Ok(0.0));
}

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

#[test]
fn with_no_follower_the_master_costs_what_the_base_processor_costs() {
    // The base: the processor without its multi-device hooks (as before
    // them; the varispeed slot's command record stays, a few stores per
    // command). Against it: the processor as built, its tee slot attached,
    // no follower. Interleaved, the best of several runs each (scheduling
    // noise only ever adds).
    let time = |base: bool| {
        let mut rig = Rig::new(eq_chain(), RateRatio::ONE, !base);
        rig.proc.set_base_path(base);
        rig.install_stage();
        rig.speed = 1.25;
        for _ in 0..8 {
            rig.block(1024, &tone);
        }
        let t = Instant::now();
        for _ in 0..60 {
            rig.block(1024, &tone);
        }
        t.elapsed().as_secs_f64()
    };
    // Under heavy machine load a round can still lose; a real cost shows in
    // every round, so retry a few before failing.
    let (mut base, mut idle) = (f64::MAX, f64::MAX);
    for _round in 0..4 {
        for _ in 0..7 {
            base = base.min(time(true));
            idle = idle.min(time(false));
        }
        if idle <= base * 1.10 + 2e-4 {
            break;
        }
    }
    assert!(
        idle <= base * 1.10 + 2e-4,
        "no follower: {idle:.6} s against the base's {base:.6} s"
    );
}

#[test]
fn the_tee_carries_the_source_before_the_stage_and_drops_when_stalled() {
    let mut rig = Rig::new(eq_chain(), RateRatio::ONE, true);
    rig.host()
        .add_follower("hw:CARD=USB,DEV=0", &master_cfg())
        .unwrap();
    let mut reader = rig.reader("hw:CARD=USB,DEV=0");
    rig.install_stage();
    rig.speed = 1.5;
    let (mut a, mut b) = (vec![0.0; 1 << 15], vec![0.0; 1 << 15]);
    let mut next = None;
    let mut pulled = 0u64;
    for k in 0..200 {
        rig.block(SIZES[k % SIZES.len()], &tone);
        pulled += rig.last.1 as u64;
        while let Some(c) = reader.read(&mut [&mut a, &mut b]) {
            assert!(!c.gap);
            if let Some(n) = next {
                assert_eq!(c.index, n);
            }
            for i in 0..c.frames {
                let x = f64::from(tone(c.index + i as u64));
                assert_eq!((a[i], b[i]), (x, -x));
            }
            next = Some(c.index + c.frames as u64);
        }
    }
    // Every source frame handed in, at speed 1.5 (more than played).
    assert_eq!(next, Some(rig.src));
    assert_eq!(rig.src, pulled);
    // A stalled follower: the master drops its blocks, and the reader
    // finds the gap (more than the ring holds, at speed 1.5).
    let stall = ring_frames().div_ceil(1024) as f64 / 1.5;
    for _ in 0..stall.ceil() as usize + 8 {
        rig.block(1024, &tone);
    }
    assert!(reader.stats().dropped_frames() > 0);
    // What was written before the stall is intact; the gap shows with the
    // first block written after it.
    let mut gap = false;
    for _ in 0..2 {
        while let Some(c) = reader.read(&mut [&mut a, &mut b]) {
            gap |= c.gap;
            let x = f64::from(tone(c.index));
            assert_eq!(a[0], x);
            next = Some(c.index + c.frames as u64);
        }
        rig.block(1024, &tone);
    }
    assert!(gap);
    let c = reader.read(&mut [&mut a, &mut b]).unwrap();
    assert_eq!(Some(c.index + c.frames as u64), Some(rig.src));
    assert!(next.is_some());
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
fn the_position_goes_invalid_off_the_dsp_path_and_on_a_reset() {
    let mut rig = Rig::new(eq_chain(), RateRatio::ONE, true);
    rig.host()
        .add_follower("hw:CARD=USB,DEV=0", &master_cfg())
        .unwrap();
    for _ in 0..8 {
        rig.block(512, &tone);
    }
    rig.proc.dac_read(1024, 5);
    let slot = Arc::clone(&rig.host.as_ref().unwrap().shared().position);
    let p = slot.read().unwrap();
    // Frame `out − 1024` carries source frame `src − 1024` (no delay).
    assert_eq!(p.pos, (rig.src as i128 - 1024) * FRAME);
    rig.block_on(SinkChoice::Direct, 512, &tone);
    assert_eq!(slot.read(), None);
    rig.proc.dac_read(1024, 6);
    assert_eq!(slot.read(), None, "nothing to follow in Direct");
    rig.block(512, &tone);
    rig.block(512, &tone);
    rig.proc.dac_read(256, 7);
    let q = slot.read().unwrap();
    assert!(q.epoch > p.epoch);
    rig.proc.reset_dsp_state();
    assert_eq!(slot.read(), None);
    // A reading of frames written before the reset maps to nothing.
    rig.proc.dac_read(0, 8);
    assert_eq!(slot.read(), None);
}

/// A follower's stage replays the log at the logged source positions, on
/// the same source behind the same look-ahead: it runs exactly as the
/// master's stage (core position and step, call after call), through a
/// restart, ramps, a jump and a cap drop.
#[test]
fn a_follower_stage_replaying_the_log_runs_as_the_master_stage() {
    let mut rig = Rig::new(eq_chain(), RateRatio::ONE, true);
    rig.host()
        .add_follower("hw:CARD=USB,DEV=0", &master_cfg())
        .unwrap();
    let mut reader = rig.reader("hw:CARD=USB,DEV=0");
    let log = Arc::clone(&rig.host.as_ref().unwrap().shared().log);
    rig.install_stage();
    let mut f = Varispeed::new(spec()).unwrap();
    // The follower's source behind its look-ahead (per channel), and the
    // source position of its input frame 0.
    let mut stream: [Vec<f64>; 2] = [Vec::new(), Vec::new()];
    let mut taken = 0usize;
    let mut origin: Option<i128> = None;
    let mut next_entry = 0u64;
    let mut tee: Vec<(u64, f64, f64)> = Vec::new();
    let (mut a, mut b) = (vec![0.0; 1 << 15], vec![0.0; 1 << 15]);
    let mut applied = 0;
    for k in 0..260usize {
        match k {
            40 => {
                rig.speed = 1.6;
                rig.ramp_ms = 120.0;
            }
            90 => {
                rig.speed = 0.6;
                rig.ramp_ms = 0.0;
            }
            150 => {
                rig.speed = 1.9;
                rig.ramp_ms = 30.0;
            }
            170 => rig.limit = 1.1,
            210 => rig.limit = 2.0,
            _ => {}
        }
        let n = [480, 1024, 333, 64, 2000][k % 5];
        rig.block(n, &tone);
        let master = rig.proc.varispeed_view().expect("the master's stage");
        while let Some(c) = reader.read(&mut [&mut a, &mut b]) {
            for i in 0..c.frames {
                tee.push((c.index + i as u64, a[i], b[i]));
            }
        }
        // The commands for this call, at its start.
        loop {
            let here = origin.map(|o| o + (f.core().accumulator().position() >> 1) as i128);
            let LogRead::Entry(e) = log.get(next_entry) else {
                break;
            };
            match e.cmd {
                VsCommand::Reset {
                    speed,
                    limit,
                    ramp_ms,
                } => {
                    f.set_speed_limit(limit);
                    f.set_speed(speed, ramp_ms).unwrap();
                    f.reset();
                    origin = Some(e.at);
                    let hold = master.hold;
                    let first = (e.at / FRAME + hold as i128) as u64;
                    for s in &mut stream {
                        s.clear();
                        s.resize(hold, 0.0);
                    }
                    taken = 0;
                    tee.retain(|t| t.0 >= first);
                }
                _ if Some(e.at) != here => {
                    assert!(
                        here.is_some_and(|h| e.at > h),
                        "entry {e:?} late at {here:?}"
                    );
                    break;
                }
                VsCommand::Speed { speed, ramp_ms } => f.set_speed(speed, ramp_ms).unwrap(),
                VsCommand::Limit(max) => f.set_speed_limit(max),
                VsCommand::Lost | VsCommand::Off => panic!("{e:?}"),
            }
            applied += 1;
            next_entry += 1;
        }
        for &(_, l, r) in &tee {
            stream[0].push(l);
            stream[1].push(r);
        }
        tee.clear();
        // The follower's call: the master's size.
        let n_src = f.plan(n).unwrap();
        assert_eq!(n_src, rig.last.1, "block {k}");
        let (mut o0, mut o1) = (vec![0.0; n], vec![0.0; n]);
        f.process(
            &[
                &stream[0][taken..taken + n_src],
                &stream[1][taken..taken + n_src],
            ],
            &mut [&mut o0, &mut o1],
        );
        taken += n_src;
        assert_eq!(f.core().accumulator().position(), master.acc, "block {k}");
        assert_eq!(f.speed(), master.speed, "block {k}");
    }
    assert!(applied >= 6, "{applied} commands");
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
            assert_eq!(rig.host().set_alignment_ms(75.0), Ok(MAX_ALIGN_MS));
        }
        refs.extend(reference.block(480, &tone).iter().step_by(2).copied());
        outs.extend(rig.block(480, &tone).iter().step_by(2).copied());
    }
    // 60 ms = 2880 frames, after the fade.
    let d = 2880;
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
    for _ in 0..10 {
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

/// The playback time the alignment adds to the frames read at block `k`:
/// the output frames over which the stage consumed the `d[k]` source
/// frames before them (blocks of `(n, n_src)`).
fn align_playback_frames(d: &[usize], blocks: &[(usize, usize)], k: usize) -> f64 {
    let mut left = d[k] as f64;
    let mut out = 0.0;
    for &(n, n_src) in blocks[..k].iter().rev() {
        if left <= 0.0 {
            break;
        }
        let take = left.min(n_src as f64);
        out += n as f64 * take / n_src as f64;
        left -= take;
    }
    out
}

/// Through speed changes the alignment holds in source frames (a change
/// would step the master's position under the followers): slowed, it adds
/// up to 60 ms / speed of playback. A new stream fits it to the speed then
/// (here a cap at 0.6), and settled back at 1.0 it is refitted.
#[test]
fn the_alignment_holds_in_source_frames_through_speed_changes() {
    let mut rig = Rig::new(eq_chain(), RateRatio::ONE, true);
    rig.host()
        .add_follower("hw:CARD=USB,DEV=0", &master_cfg())
        .unwrap();
    rig.host().set_alignment_ms(MAX_ALIGN_MS).unwrap();
    rig.install_stage();
    let cap = max_align_frames(RATE);
    let seq = |rig: &Rig| rig.proc.tee().shared().align_seq.load(Ordering::Relaxed);
    let (mut d, mut blocks) = (Vec::new(), Vec::new());
    let mut seq_at_unity = 0;
    for k in 0..400 {
        match k {
            60 => {
                rig.speed = 0.5;
                rig.ramp_ms = 200.0;
            }
            200 => {
                rig.speed = 1.0;
                rig.ramp_ms = 200.0;
            }
            300 => {
                // A cap drop under the speed.
                rig.limit = 0.6;
            }
            _ => {}
        }
        rig.block(256, &tone);
        d.push(rig.proc.tee().delay_frames());
        blocks.push(rig.last);
        if k == 59 {
            assert_eq!(d[k], cap, "the request at unity");
            seq_at_unity = seq(&rig);
        }
        if k == 199 {
            assert_eq!(d[k], cap, "held at 0.5");
            assert_eq!(rig.proc.tee().max_delay_frames(), cap, "the drain's budget");
        }
    }
    assert_eq!(*d.last().unwrap(), cap, "held under the cap drop");
    assert_eq!(
        seq(&rig),
        seq_at_unity,
        "the master's position never stepped"
    );
    for k in 0..d.len() {
        let t = align_playback_frames(&d, &blocks, k);
        assert!(
            t <= cap as f64 / 0.5 + 1.0,
            "block {k}: {t:.1} frames of playback"
        );
    }
    // A new stream at 0.6: fitted to it.
    rig.proc.stream_prepared();
    for _ in 0..20 {
        rig.block(256, &tone);
    }
    assert_eq!(
        rig.proc.tee().delay_frames(),
        (cap as f64 * 0.6).floor() as usize
    );
    // The report counts it in playback time.
    let lat = rig.proc.latency_out_frames(SinkChoice::Dsp);
    let stage = rig.proc.varispeed_view().unwrap();
    let chain_and_stage = (stage.delay_src + 0.5 * stage.speed) / stage.speed;
    assert!((lat as f64 - chain_and_stage - cap as f64 * 0.6 / 0.6).abs() < 2.0 + 240.0 / 0.6);
    // The cap lifted and settled at 1.0: refitted.
    rig.limit = 1.996;
    for _ in 0..400 {
        rig.block(256, &tone);
    }
    assert_eq!(rig.proc.tee().delay_frames(), cap, "refitted at 1.0");
    // A stop at 1.0, then a resume at 0.5: the prime resets the stage at
    // 0.5 after the stream starts, and the fit takes that speed.
    rig.proc.stream_prepared();
    rig.speed = 0.5;
    rig.ramp_ms = 0.0;
    let port = rig
        .proc
        .varispeed(SinkChoice::Dsp, RateRatio::ONE, RATE, 2)
        .unwrap();
    port.set_speed(0.5, 0.0);
    port.reset();
    for _ in 0..20 {
        rig.block(256, &tone);
    }
    assert_eq!(
        rig.proc.tee().delay_frames(),
        (cap as f64 * 0.5).floor() as usize,
        "fitted to the resumed speed"
    );
}

#[test]
fn a_reading_takes_speed_and_ramp_state_from_the_frame_at_the_dac() {
    let mut rig = Rig::new(eq_chain(), RateRatio::ONE, true);
    rig.host()
        .add_follower("hw:CARD=USB,DEV=0", &master_cfg())
        .unwrap();
    rig.install_stage();
    for _ in 0..200 {
        rig.block(256, &tone);
    }
    let slot = Arc::clone(&rig.host.as_ref().unwrap().shared().position);
    // A ramp starts: the newest block ramps, the frame at the DAC (a
    // buffer of 8 blocks back) does not yet.
    rig.speed = 1.5;
    rig.ramp_ms = 100.0;
    rig.block(256, &tone);
    rig.block(256, &tone);
    rig.proc.dac_read(8 * 256, 1);
    let p = slot.read().unwrap();
    assert!(!p.ramping);
    assert_eq!(p.speed, 1.0);
    rig.proc.dac_read(10, 2);
    assert!(slot.read().unwrap().ramping);
    // The ramp ends: the newest block is settled, the frame at the DAC was
    // rendered ramping.
    let mut settled = 0;
    while settled < 3 {
        rig.block(256, &tone);
        if rig.proc.varispeed_view().unwrap().ramping {
            settled = 0;
        } else {
            settled += 1;
        }
    }
    rig.proc.dac_read(6 * 256, 3);
    let p = slot.read().unwrap();
    assert!(p.ramping, "{p:?}");
    assert!(p.speed > 1.0 && p.speed <= 1.5);
    rig.proc.dac_read(10, 4);
    let p = slot.read().unwrap();
    assert!(!p.ramping);
    assert_eq!(p.speed, 1.5);
}

/// A follower stage that starts from the snapshot (`PlaybackState`) and
/// replays the log after its cursor, at positions relative to the
/// snapshot's: it runs as the master's stage (step, and position from the
/// snapshot, call for call).
struct Joiner {
    f: Varispeed,
    /// The master's core position at the snapshot (its domain), and the
    /// snapshot's source position.
    base_at: i128,
    next: u64,
    on: bool,
}

impl Joiner {
    fn join(state: PlaybackState) -> Self {
        let mut j = Self {
            f: Varispeed::new(spec()).unwrap(),
            base_at: state.at,
            next: state.cursor,
            on: false,
        };
        j.apply(state.at, state.cmd);
        j
    }

    fn apply(&mut self, at: i128, cmd: VsCommand) {
        match cmd {
            VsCommand::Reset {
                speed,
                limit,
                ramp_ms,
            } => {
                self.f.set_speed_limit(limit);
                self.f.set_speed(speed, ramp_ms).unwrap();
                self.f.reset();
                self.base_at = at;
                self.on = true;
            }
            VsCommand::Off => self.on = false,
            VsCommand::Speed { speed, ramp_ms } => self.f.set_speed(speed, ramp_ms).unwrap(),
            VsCommand::Limit(max) => self.f.set_speed_limit(max),
            VsCommand::Lost => panic!("lost"),
        }
    }

    /// Its position (the log's domain) at its next call.
    fn here(&self) -> i128 {
        self.base_at + (self.f.core().accumulator().position() >> 1) as i128
    }

    /// The entries due at this call, then the call itself (`n` outputs).
    fn call(&mut self, log: &CommandLog, n: usize) -> Result<(), u64> {
        loop {
            match log.get(self.next) {
                LogRead::Entry(e) => {
                    let here = self.here();
                    let due = matches!(e.cmd, VsCommand::Reset { .. } | VsCommand::Off)
                        || e.at <= here + 1;
                    if !due {
                        break;
                    }
                    // Exactly at its call, or (the first call after the
                    // snapshot) up to half a step before.
                    let half_step = (self.f.step().bits() >> 1) as i128;
                    assert!(e.at >= here - half_step - 1, "{e:?} late at {here}");
                    self.apply(e.at, e.cmd);
                    self.next += 1;
                }
                LogRead::NotYet => break,
                LogRead::Lost => return Err(self.next),
            }
        }
        if self.on {
            let n_src = self.f.plan(n).unwrap();
            let zeros = vec![0.0; n_src];
            let (mut o0, mut o1) = (vec![0.0; n], vec![0.0; n]);
            self.f.process(&[&zeros, &zeros], &mut [&mut o0, &mut o1]);
        }
        Ok(())
    }
}

#[test]
fn a_follower_joins_or_resyncs_from_the_snapshot_and_follows_the_stage_off() {
    let mut rig = Rig::new(eq_chain(), RateRatio::ONE, true);
    rig.host()
        .add_follower("hw:CARD=USB,DEV=0", &master_cfg())
        .unwrap();
    let log = Arc::clone(&rig.host.as_ref().unwrap().shared().log);
    let state = Arc::clone(&rig.host.as_ref().unwrap().shared().state);
    rig.install_stage();
    rig.speed = 1.5;
    let sizes = [480, 1024, 333, 64, 2000];
    // Steady at 1.5, then more cap changes than the log keeps.
    for k in 0..130usize {
        if k >= 40 {
            rig.limit = if k % 2 == 0 { 1.9 } else { 2.0 };
        }
        rig.block(sizes[k % 5], &tone);
    }
    assert_eq!(log.get(0), LogRead::Lost, "the restart is gone");
    rig.limit = 2.0;
    rig.block(480, &tone);
    // Joining: the snapshot and the log after it.
    let snap = state.read().expect("a snapshot");
    assert!(log.head() - snap.cursor <= 1, "{snap:?}");
    assert_eq!(
        snap.cmd,
        VsCommand::Reset {
            speed: 1.5,
            // The cap while followed.
            limit: FOLLOWED_MAX_SPEED,
            ramp_ms: 0.0
        }
    );
    let mut j = Joiner::join(snap);
    // The master's core position at the snapshot, and its step: the
    // joiner's output 0 sits a step after it.
    let base = rig.proc.varispeed_view().unwrap();
    let check = |j: &Joiner, rig: &Rig, base: &StageView, k: usize| {
        let m = rig.proc.varispeed_view().unwrap();
        assert_eq!(j.f.speed(), m.speed, "block {k}");
        let rel = (m.acc - base.acc) as i128;
        let fr = (j.f.core().accumulator().position() + base.step) as i128;
        assert!((rel - fr).abs() <= 2, "block {k}: {rel} against {fr}");
    };
    for k in 0..120usize {
        match k {
            20 => {
                rig.speed = 0.8;
                rig.ramp_ms = 150.0;
            }
            70 => rig.limit = 0.7,
            _ => {}
        }
        let n = sizes[k % 5];
        rig.block(n, &tone);
        j.call(&log, n).unwrap();
        check(&j, &rig, &base, k);
    }
    // A follower that fell behind: Lost, then the snapshot again.
    let stale = log.head();
    for k in 0..70usize {
        rig.limit = if k % 2 == 0 { 0.75 } else { 0.7 };
        rig.block(sizes[k % 5], &tone);
    }
    // While the cap keeps moving the stage never settles: the snapshot
    // stays where it last did (consistent, older). Settled again, it is
    // the newest.
    rig.limit = 0.7;
    rig.block(480, &tone);
    while rig.proc.varispeed_view().unwrap().ramping {
        rig.block(480, &tone);
    }
    assert_eq!(log.get(stale), LogRead::Lost);
    let snap = state.read().unwrap();
    assert!(snap.cursor > stale);
    assert_eq!(snap.cursor, log.head());
    let mut j = Joiner::join(snap);
    let base = rig.proc.varispeed_view().unwrap();
    for k in 0..30usize {
        let n = sizes[k % 5];
        rig.block(n, &tone);
        j.call(&log, n).unwrap();
        check(&j, &rig, &base, k);
    }
    // The stage retires: the log and the snapshot say unity.
    assert!(rig.vs_tx.push(VarispeedRtCmd::Disable).is_ok());
    for _ in 0..20 {
        rig.block(480, &tone);
        j.call(&log, 480).unwrap();
    }
    assert!(rig.proc.varispeed_view().is_none());
    assert!(!j.on, "the follower saw Off");
    let snap = state.read().unwrap();
    assert_eq!(snap.cmd, VsCommand::Off);
    assert_eq!(snap.cursor, log.head());
}

#[test]
fn the_driver_hooks_count_and_restart_the_alignment() {
    let mut rig = Rig::new(eq_chain(), RateRatio::ONE, true);
    rig.host()
        .add_follower("hw:CARD=USB,DEV=0", &master_cfg())
        .unwrap();
    rig.host().set_alignment_ms(10.0).unwrap();
    for _ in 0..20 {
        rig.block(480, &tone);
    }
    // The clock delay's and the drain's term: the path that plays only.
    assert_eq!(rig.proc.alignment_frames(SinkChoice::Dsp), 480);
    assert_eq!(rig.proc.alignment_frames(SinkChoice::Direct), 0);
    assert_eq!(rig.proc.latency_out_frames(SinkChoice::Dsp), 480);
    // The stream ends: the drain feeds that many frames of zeros, and the
    // last real frame comes out within them.
    let end = rig.src;
    let mut out = Vec::new();
    for _ in 0..2 {
        out.extend(
            rig.block(480, &|i| if i < end { 0.4 } else { 0.0 })
                .iter()
                .step_by(2)
                .copied(),
        );
    }
    let last = out.iter().rposition(|&x| x != 0).unwrap();
    assert_eq!(last + 1, 480, "the line holds exactly its delay");
    // A new stream (a rate reopen's prepare): nothing old plays, and the
    // position restarts.
    for _ in 0..4 {
        rig.block(480, &|_| 0.4);
    }
    rig.proc.dac_read(100, 1);
    let slot = Arc::clone(&rig.host.as_ref().unwrap().shared().position);
    assert!(slot.read().is_some());
    rig.proc.stream_prepared();
    assert_eq!(slot.read(), None);
    let fresh: Vec<i32> = rig
        .block(480, &|_| 0.2)
        .iter()
        .step_by(2)
        .copied()
        .collect();
    assert!(fresh.iter().all(|&x| x == 0), "old-stream audio replayed");
    let next: Vec<i32> = rig
        .block(480, &|_| 0.2)
        .iter()
        .step_by(2)
        .copied()
        .collect();
    assert!(next[240..].iter().all(|&x| x != 0));
}
