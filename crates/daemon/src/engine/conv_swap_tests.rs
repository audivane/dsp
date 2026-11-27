//! Edits with a long convolution loaded (a room calibration's 2x2 matrix
//! of 32768 taps). One before the convolution (an EQ band, a commit that
//! changes nothing) takes the playing chain over in place: it lands in the
//! next period and fades in at the convolution's input. One after it that
//! keeps its input copies the convolver state, within a copy budget per
//! period; a new IR warms the new chain up for its ring. No missing direct
//! sound or room tail, no transient through it, no click.

use super::*;
use audivane_dsp_core::chain::ChainSpec;
use audivane_dsp_core::matrix::{ConvIrs, FilterMatrix};
use audivane_protocol::{Band, ChainConfig, ConvolutionConfig, EqProfile, FilterType};
use std::time::{Duration, Instant};

const RATE: u32 = 48_000;
const BLOCK: usize = 1024;
const TAPS: usize = 32_768;
/// The calibration's delay: its peak, 30 ms in.
const PEAK: usize = 1_440;
const FS: f64 = 2_147_483_648.0;

fn lcg(seed: &mut u64) -> f64 {
    *seed = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    ((*seed >> 11) as f64) / ((1u64 << 53) as f64) * 2.0 - 1.0
}

/// A room-correction-like filter: a peak at [`PEAK`] and ringing around
/// it (low-passed noise, as a correction rings longest in the bass: 3 ms
/// before, 6 ms after, a faint 40 ms floor), about as rough as a
/// full-phase correction the room calibration makes.
fn room_ir(direct: f64, ring: f64, seed: u64) -> Vec<f64> {
    let mut s = seed;
    let mut lp = 0.0;
    (0..TAPS)
        .map(|i| {
            lp += 0.02 * (lcg(&mut s) - lp);
            let d = i as f64 - PEAK as f64;
            let env = if d > 0.0 {
                (-d / 300.0).exp() + 0.01 * (-d / 2_000.0).exp()
            } else {
                (d / 150.0).exp()
            };
            ring * lp * env + if i == PEAK { direct } else { 0.0 }
        })
        .collect()
}

fn room_matrix() -> FilterMatrix {
    let mut m = FilterMatrix::new(2);
    m.set(0, 0, room_ir(0.7, 0.15, 1));
    m.set(1, 1, room_ir(0.7, 0.15, 2));
    m.set(0, 1, room_ir(0.0, 0.05, 3));
    m.set(1, 0, room_ir(0.0, 0.05, 4));
    m
}

fn calibration() -> ConvolutionConfig {
    ConvolutionConfig {
        name: None,
        enabled: true,
        ir_path: Default::default(),
        gain_db: 0.0,
        matrix: Some(Vec::new()),
        calibration: true,
    }
}

/// Chain A: one EQ band at 80 Hz of `gain_db`, then the calibration.
pub(super) fn conv_chain(gain_db: f64, m: &FilterMatrix) -> ActiveChain {
    ActiveChain::Single(Box::new(Chain::new(ChainSpec {
        config: ChainConfig {
            eq: Some(EqProfile {
                name: "room".into(),
                preamp_db: -3.0,
                bands: vec![Band {
                    kind: FilterType::Peaking,
                    freq_hz: 80.0,
                    gain_db,
                    q: 1.4,
                    enabled: true,
                }],
                ..Default::default()
            }),
            convolution: vec![calibration()],
            ..ChainConfig::neutral()
        },
        sample_rate: RATE,
        channels: 2,
        max_block: BLOCK,
        irs: Some(ConvIrs::Matrix(m.clone())),
    })))
}

fn processor(chain: ActiveChain) -> (DaemonProcessor, Rig) {
    let (_cmd_tx, cmd_rx) = rtrb::RingBuffer::new(8);
    let (swap_tx, swap_rx) = rtrb::RingBuffer::new(8);
    let (recycle_tx, recycle_rx) = rtrb::RingBuffer::new(8);
    let proc = DaemonProcessor::new(
        chain,
        true,
        2,
        BLOCK,
        16,
        RATE,
        Arc::new(AtomicU8::new(0)),
        MeterBank::new(2),
        cmd_rx,
        swap_rx,
        recycle_tx,
    );
    (
        proc,
        Rig {
            swap_tx,
            recycle_rx,
            _cmd_tx,
        },
    )
}

struct Rig {
    swap_tx: rtrb::Producer<PendingSwap>,
    recycle_rx: rtrb::Consumer<ActiveChain>,
    _cmd_tx: rtrb::Producer<RtDspCmd>,
}

/// Music-like input: bass, mids and treble on both channels.
fn input(block: usize) -> Vec<f32> {
    let mut v = Vec::with_capacity(BLOCK * 2);
    for i in 0..BLOCK {
        let t = (block * BLOCK + i) as f64 / f64::from(RATE);
        let tone = |f: f64, ph: f64| (2.0 * PI * f * t + ph).sin();
        let l = 0.2 * tone(41.0, 0.0) + 0.15 * tone(97.0, 1.0) + 0.1 * tone(1_250.0, 2.0);
        let r = 0.2 * tone(43.0, 0.5) + 0.15 * tone(103.0, 1.5) + 0.1 * tone(3_100.0, 2.5);
        v.push(l as f32);
        v.push(r as f32);
    }
    v
}

/// One period: the updates (a swap starts there) and the block, timed
/// together (the audio thread runs both).
fn run(proc: &mut DaemonProcessor, block: usize, out: &mut [i32]) -> Duration {
    let inp = input(block);
    let t0 = Instant::now();
    proc.poll_updates(SinkChoice::Dsp, RATE, RateRatio::ONE, true);
    proc.process_block(
        SinkChoice::Dsp,
        &inp,
        &inp,
        out,
        2,
        RateRatio::ONE,
        RATE,
        (block * BLOCK) as u64,
    );
    t0.elapsed()
}

fn max_diff(a: &[i32], b: &[i32]) -> f64 {
    a.iter()
        .zip(b)
        .map(|(&x, &y)| (f64::from(x) - f64::from(y)).abs() / FS)
        .fold(0.0, f64::max)
}

/// The largest sample and the largest step between two samples (a click's
/// mark) of a stereo stream, block after block.
#[derive(Default)]
struct Track {
    last: Option<[f64; 2]>,
    step: f64,
    peak: f64,
}

impl Track {
    fn feed(&mut self, out: &[i32]) {
        for &[l, r] in out.as_chunks::<2>().0 {
            let x = [f64::from(l) / FS, f64::from(r) / FS];
            if let Some(l) = self.last {
                self.step = self.step.max((x[0] - l[0]).abs()).max((x[1] - l[1]).abs());
            }
            self.peak = self.peak.max(x[0].abs()).max(x[1].abs());
            self.last = Some(x);
        }
    }
}

struct Outcome {
    /// The swapped processor against one that plays `a` throughout, FS.
    off_a: f64,
    /// `a` against `b`, both played throughout: what the edit itself
    /// changes, FS.
    a_to_b: f64,
    swaps: usize,
    /// The most periods an edit took to land, and its fade to end.
    land: usize,
    fade: usize,
    /// The swapped stream, and the two settings' (the larger of each).
    out: Track,
    refs: Track,
    /// The slowest block with a swap in flight, and without.
    slowest_swap: Duration,
    slowest_idle: Duration,
}

/// Play `a`, then edit to `b` and back `edits` times (a drag across the
/// two values, or commits of the same one when they are equal), each edit
/// once the last has landed and faded in, then a period more (a drag
/// step's pace); compare with `a` and `b` played throughout.
fn edits(a_db: f64, b_db: f64, edits: usize) -> Outcome {
    let m = room_matrix();
    edits_with(&|db| conv_chain(db, &m), a_db, b_db, edits)
}

fn edits_with(mk: &dyn Fn(f64) -> ActiveChain, a_db: f64, b_db: f64, edits: usize) -> Outcome {
    let (mut proc, mut rig) = processor(mk(a_db));
    let (mut ref_a, _ra) = processor(mk(a_db));
    let (mut ref_b, _rb) = processor(mk(b_db));
    let mut out = vec![0i32; BLOCK * 2];
    let mut out_a = vec![0i32; BLOCK * 2];
    let mut out_b = vec![0i32; BLOCK * 2];
    // Past the IR's length: every chain plays its steady state.
    let warm = TAPS.div_ceil(BLOCK) + 4;
    let mut block = 0;
    for _ in 0..warm {
        run(&mut proc, block, &mut out);
        run(&mut ref_a, block, &mut out_a);
        run(&mut ref_b, block, &mut out_b);
        block += 1;
    }
    assert_eq!(max_diff(&out, &out_a), 0.0, "the same chain plays the same");

    let mut o = Outcome {
        off_a: 0.0,
        a_to_b: 0.0,
        swaps: 0,
        land: 0,
        fade: 0,
        out: Track::default(),
        refs: Track::default(),
        slowest_swap: Duration::ZERO,
        slowest_idle: Duration::ZERO,
    };
    let (mut track_a, mut track_b) = (Track::default(), Track::default());
    for k in 0..edits {
        let generation = k as u64 + 1;
        let db = if k % 2 == 0 { b_db } else { a_db };
        rig.swap_tx
            .push(PendingSwap {
                chain: mk(db),
                crossfade: true,
                generation,
                swap_seq: generation,
            })
            .unwrap();
        let (mut land, mut fade, mut settle) = (0, 0, 0);
        loop {
            let landed = proc.active_generation() == generation;
            let fading = proc.active_chain.upstream_fading();
            let busy = !landed || fading || !matches!(proc.swap_phase, SwapPhase::Idle);
            if landed && !busy && settle == 1 {
                break;
            }
            let dt = run(&mut proc, block, &mut out);
            if !landed {
                land += 1;
            } else if fading {
                fade += 1;
            }
            if busy {
                o.slowest_swap = o.slowest_swap.max(dt);
            } else {
                o.slowest_idle = o.slowest_idle.max(dt);
                settle += 1;
            }
            run(&mut ref_a, block, &mut out_a);
            run(&mut ref_b, block, &mut out_b);
            o.out.feed(&out);
            track_a.feed(&out_a);
            track_b.feed(&out_b);
            o.off_a = o.off_a.max(max_diff(&out, &out_a));
            o.a_to_b = o.a_to_b.max(max_diff(&out_a, &out_b));
            while rig.recycle_rx.pop().is_ok() {}
            block += 1;
            assert!(block < 10_000, "the swap never lands");
        }
        o.land = o.land.max(land);
        o.fade = o.fade.max(fade);
        o.swaps += 1;
    }
    o.refs.step = track_a.step.max(track_b.step);
    o.refs.peak = track_a.peak.max(track_b.peak);
    o
}

/// A gain step of the band, 0 to +9 dB (the band starts fresh: a section
/// the old EQ did not have): a smooth transition, never louder than the
/// louder setting, no step between samples past the settings' own.
#[test]
fn a_gain_step_fades_in_smoothly() {
    let o = edits(0.0, 9.0, 2);
    eprintln!(
        "gain step: off a {:.3e} FS, a to b {:.3e} FS, land {} fade {} periods, peak {:.4} \
         (settings {:.4}), step {:.4} (settings {:.4})",
        o.off_a, o.a_to_b, o.land, o.fade, o.out.peak, o.refs.peak, o.out.step, o.refs.step
    );
    assert_eq!(o.land, 1);
    assert!(
        o.out.peak <= 1.01 * o.refs.peak,
        "louder than either setting"
    );
    assert!(o.out.step <= 1.01 * o.refs.step, "a click");
    assert!(o.off_a <= 1.05 * o.a_to_b);
}

/// Once an edit's fade has ended and the IR's ring has passed, the chain
/// that took over plays the new setting bit for bit as a chain of the new
/// setting started at the edit (its band starts fresh at the same frame,
/// as the edited band does; against one that played the new setting all
/// along the two bands' rounding keeps them a 32-bit LSB apart). No
/// dither: the same output on both. Nothing of the outgoing stages or the
/// mix is left.
#[test]
fn after_the_fade_the_output_is_the_new_settings_bit_for_bit() {
    let m = room_matrix();
    let mk = |db: f64| {
        let ActiveChain::Single(c) = conv_chain(db, &m) else {
            unreachable!()
        };
        let mut config = c.config().clone();
        config.output.dither = audivane_protocol::DitherKind::None;
        config.output.bits = 32;
        ActiveChain::Single(Box::new(Chain::new(ChainSpec {
            config,
            sample_rate: RATE,
            channels: 2,
            max_block: BLOCK,
            irs: Some(ConvIrs::Matrix(m.clone())),
        })))
    };
    let (mut proc, mut rig) = processor(mk(4.0));
    let (mut out, mut ob) = (vec![0i32; BLOCK * 2], vec![0i32; BLOCK * 2]);
    let ring = TAPS.div_ceil(BLOCK) + 4;
    let mut block = 0;
    for _ in 0..ring {
        run(&mut proc, block, &mut out);
        block += 1;
    }
    let (mut ref_b, _rb) = processor(mk(6.0));
    rig.swap_tx
        .push(PendingSwap {
            chain: mk(6.0),
            crossfade: true,
            generation: 1,
            swap_seq: 1,
        })
        .unwrap();
    let mut first_equal = None;
    for k in 0..4 * ring {
        run(&mut proc, block, &mut out);
        run(&mut ref_b, block, &mut ob);
        while rig.recycle_rx.pop().is_ok() {}
        block += 1;
        if out == ob {
            first_equal.get_or_insert(k);
        } else {
            assert!(first_equal.is_none(), "block {k}: equal, then not");
        }
    }
    eprintln!("bit for bit from {first_equal:?} periods after the edit");
    let at = first_equal.expect("never bit for bit");
    // The IR's ring (32 periods) and the band's fade.
    assert!(at <= ring, "bit for bit only {at} periods on");
    assert!(!proc.active_chain.upstream_fading());
}

/// A +6 dB, Q 1 peak moved from 1 kHz to 25 Hz under a 0.35 FS tone, with
/// an unchanged IR that has a reflection 100 ms after its peak. A fade at
/// the convolution's input would pass both settings' through the
/// reflection: the fade bound refuses the takeover, and the new chain
/// warms up for the ring (the moved band from cleared state) and fades in
/// after the convolution. The output never passes what either setting
/// plays (the edit's state copied into the moved band rang to several
/// times full scale, and through the reflection it came out after a
/// post-chain crossfade).
#[test]
fn a_moved_band_with_a_long_reflection_stays_bounded() {
    let mut ir = vec![0.0; TAPS];
    ir[PEAK] = 0.6;
    ir[PEAK + 4_800] = 0.4;
    let irs = ConvIrs::Channels(vec![ir.clone(), ir]);
    let chain = |hz: f64| {
        ActiveChain::Single(Box::new(Chain::new(ChainSpec {
            config: ChainConfig {
                eq: Some(EqProfile {
                    name: "move".into(),
                    bands: vec![Band {
                        kind: FilterType::Peaking,
                        freq_hz: hz,
                        gain_db: 6.0,
                        q: 1.0,
                        enabled: true,
                    }],
                    ..Default::default()
                }),
                convolution: vec![calibration()],
                ..ChainConfig::neutral()
            },
            sample_rate: RATE,
            channels: 2,
            max_block: BLOCK,
            irs: Some(irs.clone()),
        })))
    };
    let tone = |block: usize| -> Vec<f32> {
        (0..BLOCK)
            .flat_map(|i| {
                let t = (block * BLOCK + i) as f64 / f64::from(RATE);
                let x = (0.35 * (2.0 * PI * 1000.0 * t).sin()) as f32;
                [x, x]
            })
            .collect()
    };
    let play = |p: &mut DaemonProcessor, block: usize, out: &mut [i32]| {
        p.poll_updates(SinkChoice::Dsp, RATE, RateRatio::ONE, true);
        p.process_block(
            SinkChoice::Dsp,
            &tone(block),
            &tone(block),
            out,
            2,
            RateRatio::ONE,
            RATE,
            (block * BLOCK) as u64,
        );
    };
    let peak = |o: &[i32]| {
        o.iter()
            .fold(0.0f64, |m, &x| m.max(f64::from(x).abs() / FS))
    };
    let (mut proc, mut rig) = processor(chain(1000.0));
    let (mut ref_a, _ra) = processor(chain(1000.0));
    let (mut ref_b, _rb) = processor(chain(25.0));
    let (mut out, mut oa, mut ob) = (vec![0; BLOCK * 2], vec![0; BLOCK * 2], vec![0; BLOCK * 2]);
    let warm = TAPS.div_ceil(BLOCK) + 4;
    for k in 0..warm {
        play(&mut proc, k, &mut out);
        play(&mut ref_a, k, &mut oa);
        play(&mut ref_b, k, &mut ob);
    }
    rig.swap_tx
        .push(PendingSwap {
            chain: chain(25.0),
            crossfade: true,
            generation: 1,
            swap_seq: 1,
        })
        .unwrap();
    let (mut got, mut bound) = (0.0f64, 0.0f64);
    let mut landed = None;
    for k in warm..warm + 3 * warm {
        play(&mut proc, k, &mut out);
        if proc.active_generation() == 1 {
            landed.get_or_insert(k + 1 - warm);
        }
        play(&mut ref_a, k, &mut oa);
        play(&mut ref_b, k, &mut ob);
        got = got.max(peak(&out));
        bound = bound.max(peak(&oa)).max(peak(&ob));
        while rig.recycle_rx.pop().is_ok() {}
    }
    eprintln!("moved band: peak {got:.4} FS, the settings' {bound:.4} FS, landed after {landed:?}");
    let ring = (TAPS + 1_024 + 1_024).div_ceil(BLOCK) + 1;
    assert!(
        landed.is_some_and(|p| p > 1 && p <= ring),
        "the edit warms up for the ring and lands: {landed:?}"
    );
    assert!(got <= bound + 1e-6, "peak {got} over the settings' {bound}");
}

// ---------------------------------------------------------------------------
// The fade bound against random IRs and edits.

/// A small seeded generator.
struct Rng(u64);

impl Rng {
    fn unit(&mut self) -> f64 {
        (lcg(&mut self.0) + 1.0) / 2.0
    }
    fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.unit()
    }
    fn log(&mut self, lo: f64, hi: f64) -> f64 {
        self.range(lo.ln(), hi.ln()).exp()
    }
    fn below(&mut self, n: usize) -> usize {
        ((self.unit() * n as f64) as usize).min(n - 1)
    }
    fn chance(&mut self, p: f64) -> bool {
        self.unit() < p
    }
    fn sign(&mut self) -> f64 {
        if self.chance(0.5) { 1.0 } else { -1.0 }
    }
}

/// The longest random IR: 150 ms.
const ADV_TAPS: usize = 7_201;

/// A sparse IR (one to four taps, random delays up to 150 ms, signs and
/// gains) or a dense one (a direct tap and decaying noise, random delay
/// and decay).
fn random_ir(r: &mut Rng, scale: f64) -> Vec<f64> {
    let mut ir = vec![0.0; ADV_TAPS];
    if r.chance(0.5) {
        for _ in 0..1 + r.below(4) {
            ir[r.below(ADV_TAPS)] += r.sign() * r.range(0.1, 2.0) * scale;
        }
    } else {
        let peak = r.below(ADV_TAPS / 2);
        let tau = r.log(24.0, 2_400.0);
        let ring = r.range(0.02, 0.4) * scale;
        let mut s = r.0;
        let lp_k = r.range(0.01, 1.0);
        let mut lp = 0.0;
        for (i, h) in ir.iter_mut().enumerate().skip(peak) {
            lp += lp_k * (lcg(&mut s) - lp);
            *h = ring * lp * (-((i - peak) as f64) / tau).exp();
        }
        ir[peak] += r.sign() * r.range(0.2, 1.0) * scale;
    }
    ir
}

fn random_irs(r: &mut Rng) -> ConvIrs {
    if r.chance(0.3) {
        return ConvIrs::Channels(vec![random_ir(r, 1.0), random_ir(r, 1.0)]);
    }
    let mut m = FilterMatrix::new(2);
    m.set(0, 0, random_ir(r, 1.0));
    m.set(1, 1, random_ir(r, 1.0));
    for (o, i) in [(0, 1), (1, 0)] {
        if r.chance(0.5) {
            m.set(o, i, random_ir(r, 0.5));
        }
    }
    ConvIrs::Matrix(m)
}

fn random_band(r: &mut Rng) -> Band {
    let kind = match r.below(5) {
        0 | 1 => FilterType::Peaking,
        2 => FilterType::LowShelf,
        3 => FilterType::HighShelf,
        _ => FilterType::HighPass,
    };
    let q = match kind {
        FilterType::Peaking => r.log(0.5, 30.0),
        FilterType::HighPass => r.range(0.5, 1.4),
        _ => 0.707,
    };
    let freq_hz = match kind {
        FilterType::HighPass => r.log(20.0, 300.0),
        _ => r.log(20.0, 16_000.0),
    };
    Band {
        kind,
        freq_hz,
        gain_db: r.range(-12.0, 12.0),
        q,
        enabled: true,
    }
}

/// A low, sharp bell (15-60 Hz, Q 5-40): its sections ring for seconds,
/// so a fresh one starts far from its settled state.
fn random_low_band(r: &mut Rng) -> Band {
    Band {
        kind: FilterType::Peaking,
        freq_hz: r.log(15.0, 60.0),
        gain_db: r.sign() * r.range(1.0, 12.0),
        q: r.log(5.0, 40.0),
        enabled: true,
    }
}

/// A low-band edit of `p`: a low bell's gain changed (when `p` has one),
/// or one added; then sometimes another random edit. The low band's
/// frequency.
fn random_low_edit(r: &mut Rng, p: &mut EqProfile) -> f64 {
    let lows: Vec<usize> = (0..p.bands.len())
        .filter(|&k| p.bands[k].freq_hz <= 60.0 && p.bands[k].q >= 5.0)
        .collect();
    let at = if !lows.is_empty() && r.chance(0.6) {
        let k = lows[r.below(lows.len())];
        p.bands[k].gain_db += r.sign() * r.range(0.5, 6.0);
        p.bands[k].freq_hz
    } else {
        let b = random_low_band(r);
        let at = b.freq_hz;
        p.bands.push(b);
        at
    };
    if r.chance(0.3) {
        random_edit(r, p);
    }
    at
}

/// An edit of `p`, on the linked bands or the left channel's: a band's
/// gain, a band enabled or disabled, a band added (a high-Q peak, a shelf,
/// a steep high-pass: two sections), the preamp, or two of these. The
/// frequency it acts at (a tone there is the hardest test).
fn random_edit(r: &mut Rng, p: &mut EqProfile) -> f64 {
    let mut at = r.log(20.0, 16_000.0);
    for _ in 0..if r.chance(0.3) { 2 } else { 1 } {
        let left = r.chance(0.3);
        let bands = if left {
            &mut p
                .left
                .get_or_insert_with(audivane_protocol::ChannelEq::default)
                .bands
        } else {
            &mut p.bands
        };
        match r.below(5) {
            0 if !bands.is_empty() => {
                let k = r.below(bands.len());
                bands[k].gain_db = r.range(-12.0, 12.0);
                at = bands[k].freq_hz;
            }
            1 if !bands.is_empty() => {
                let k = r.below(bands.len());
                bands[k].enabled = !bands[k].enabled;
                at = bands[k].freq_hz;
            }
            2 => {
                let b = random_band(r);
                at = b.freq_hz;
                if b.kind == FilterType::HighPass {
                    bands.push(Band {
                        q: 0.541,
                        ..b.clone()
                    });
                    bands.push(Band { q: 1.307, ..b });
                } else {
                    bands.push(b);
                }
            }
            _ => {
                if left {
                    let c = p.left.as_mut().unwrap();
                    c.preamp_db += r.range(-12.0, 6.0);
                } else {
                    p.preamp_db += r.range(-12.0, 6.0);
                }
            }
        }
    }
    at
}

/// One random case: the edit's takeover (the bound it took) or its
/// warm-up, and how far the output passed both settings', per unit of
/// tone (a tone of `AMP` at `hz` on both channels).
struct AdvCase {
    bound: Option<f64>,
    over: f64,
    landed: usize,
}

const AMP: f64 = 0.05;

/// `low`: the edit is at a low, sharp bell (a tone at its frequency or
/// within 2% of it), and the case plays long enough for the bells to
/// settle before it (the reference chains too) and for the hold or the
/// warm-up (up to 5 s) and the fade after it.
fn adversarial_case(r: &mut Rng, low: bool) -> AdvCase {
    let irs = random_irs(r);
    let mut old = EqProfile {
        name: "adv".into(),
        bands: (0..r.below(3)).map(|_| random_band(r)).collect(),
        ..Default::default()
    };
    if low && r.chance(0.6) {
        old.bands.push(random_low_band(r));
    }
    if r.chance(0.3) {
        old.preamp_db = r.range(-6.0, 0.0);
    }
    let mut new = old.clone();
    let hz = if low {
        let at = random_low_edit(r, &mut new);
        if r.chance(0.5) {
            at
        } else {
            at * r.range(0.98, 1.02)
        }
    } else {
        let at = random_edit(r, &mut new);
        if r.chance(0.6) {
            at * r.range(0.97, 1.03)
        } else {
            r.log(20.0, 20_000.0)
        }
    };
    let chain = |eq: &EqProfile| {
        ActiveChain::Single(Box::new(Chain::new(ChainSpec {
            config: ChainConfig {
                eq: Some(eq.clone()),
                convolution: vec![calibration()],
                output: audivane_protocol::OutputConfig {
                    bits: 32,
                    dither: audivane_protocol::DitherKind::None,
                    headroom_db: 0.0,
                },
                ..ChainConfig::neutral()
            },
            sample_rate: RATE,
            channels: 2,
            max_block: BLOCK,
            irs: Some(irs.clone()),
        })))
    };
    let tone = |block: usize| -> Vec<f32> {
        (0..BLOCK)
            .flat_map(|i| {
                let t = (block * BLOCK + i) as f64 / f64::from(RATE);
                let w = 2.0 * PI * hz * t;
                [(AMP * w.sin()) as f32, (AMP * (w + 1.0).sin()) as f32]
            })
            .collect()
    };
    let play = |p: &mut DaemonProcessor, block: usize, out: &mut [i32]| {
        p.poll_updates(SinkChoice::Dsp, RATE, RateRatio::ONE, true);
        p.process_block(
            SinkChoice::Dsp,
            &tone(block),
            &tone(block),
            out,
            2,
            RateRatio::ONE,
            RATE,
            (block * BLOCK) as u64,
        );
    };
    let peak = |o: &[i32]| {
        o.iter()
            .fold(0.0f64, |m, &x| m.max(f64::from(x).abs() / FS))
    };
    let (mut proc, mut rig) = processor(chain(&old));
    let (mut ref_a, _ra) = processor(chain(&old));
    let (mut ref_b, _rb) = processor(chain(&new));
    let (mut out, mut oa, mut ob) = (vec![0; BLOCK * 2], vec![0; BLOCK * 2], vec![0; BLOCK * 2]);
    // Past the IR and the EQs' own start: steady. (A low bell rings for
    // seconds: Q 40 at 15 Hz settles to -60 dB in about 4 s.)
    let (warm, len) = if low { (240, 300) } else { (40, 90) };
    for k in 0..warm {
        play(&mut proc, k, &mut out);
        play(&mut ref_a, k, &mut oa);
        play(&mut ref_b, k, &mut ob);
    }
    rig.swap_tx
        .push(PendingSwap {
            chain: chain(&new),
            crossfade: true,
            generation: 1,
            swap_seq: 1,
        })
        .unwrap();
    let (mut got, mut settled) = (0.0f64, 0.0f64);
    let mut landed = None;
    let mut bound = None;
    for k in warm..warm + len {
        play(&mut proc, k, &mut out);
        if k == warm
            && proc.active_generation() == 1
            && let ActiveChain::Single(c) = &proc.active_chain
        {
            bound = Some(c.upstream_fade().map_or(0.0, |f| f.1));
        }
        if proc.active_generation() == 1 && matches!(proc.swap_phase, SwapPhase::Idle) {
            landed.get_or_insert(k + 1 - warm);
        }
        play(&mut ref_a, k, &mut oa);
        play(&mut ref_b, k, &mut ob);
        got = got.max(peak(&out));
        settled = settled.max(peak(&oa)).max(peak(&ob));
        while rig.recycle_rx.pop().is_ok() {}
    }
    AdvCase {
        bound,
        over: (got - settled) / AMP,
        landed: landed.expect("the edit lands"),
    }
}

/// Random IRs (sparse taps or dense decays, delays up to 150 ms, cross
/// routes) under random edits (high-Q bands of +-12 dB, a band enabled or
/// disabled, shelves, a steep high-pass, the preamp, on one input or
/// both), a tone at the edit's frequency or anywhere. Every takeover the
/// bound accepts passes both settings' by no more than the bound (plus a
/// margin of 1% of the tone); every edit it refuses lands through the
/// warm-up (its crossfade after the convolution passes them by nothing).
fn adversarial(seed: u64, cases: usize, low: bool) {
    let mut r = Rng(seed);
    let (mut taken, mut worst, mut tight) = (0, f64::NEG_INFINITY, 0.0f64);
    for case in 0..cases {
        let c = adversarial_case(&mut r, low);
        match c.bound {
            Some(b) => {
                taken += 1;
                worst = worst.max(c.over - b);
                if b > 0.0 {
                    tight = tight.max(c.over / b);
                }
                assert!(
                    c.over <= b + 0.01,
                    "seed {seed} case {case}: passed both settings' by {:.4} per unit, the bound \
                     {b:.4}",
                    c.over
                );
            }
            None => {
                assert!(
                    c.over <= 0.01,
                    "seed {seed} case {case}: the warm-up passed the settings' by {:.4}",
                    c.over
                );
                assert!(c.landed > 1, "seed {seed} case {case}: neither path");
            }
        }
    }
    eprintln!(
        "fade bound, seed {seed}, low {low}: {taken} of {cases} taken over, the most over the bound \
         {worst:+.4} per unit, the most of the bound reached {:.0}%",
        100.0 * tight
    );
}

#[test]
fn the_fade_bound_holds_for_random_edits_1() {
    adversarial(1, 80, false);
}
