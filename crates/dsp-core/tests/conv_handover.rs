//! [`Chain::inherit_filter_state`]: a chain built from the same config
//! that takes over a running chain's EQ and convolution state plays on bit
//! for bit as that chain does (per-channel IRs or a filter matrix, whole
//! or on channel workers); another filter is not taken over.
//! [`Chain::take_over_upstream`]: an edit before an unchanged convolution,
//! taken over in place with a fade at its input, as long as the IR's fade
//! bound allows.

use audivane_dsp_core::chain::{Chain, ChainSpec};
use audivane_dsp_core::gain::StereoGainRamp;
use audivane_dsp_core::matrix::{ConvIrs, FilterMatrix};
use audivane_dsp_core::workers::LanePool;
use audivane_protocol::{
    Band, ChainConfig, ConvolutionConfig, DitherKind, EqProfile, FilterType, OutputConfig,
};
use std::sync::Arc;
use std::time::Duration;

const RATE: u32 = 48_000;
const BLOCK: usize = 512;
const TAPS: usize = 20_000;

fn lcg(seed: &mut u64) -> f64 {
    *seed = seed
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    ((*seed >> 11) as f64) / ((1u64 << 53) as f64) * 2.0 - 1.0
}

fn ir(seed: u64) -> Vec<f64> {
    let mut s = seed;
    (0..TAPS)
        .map(|i| lcg(&mut s) * (-(i as f64) / 6_000.0).exp() * 0.05)
        .collect()
}

/// A room-calibration-like filter: a peak 30 ms in, ringing around it
/// (low-passed noise, 3 ms before, 6 ms after with a faint 40 ms floor),
/// about as rough as a full-phase correction the calibration makes.
fn cal_ir(seed: u64, direct: f64, ring: f64) -> Vec<f64> {
    const PEAK: i64 = 1_440;
    let mut s = seed;
    let mut lp = 0.0;
    (0..TAPS as i64)
        .map(|i| {
            lp += 0.02 * (lcg(&mut s) - lp);
            let d = i - PEAK;
            let env = if d > 0 {
                (-(d as f64) / 300.0).exp() + 0.01 * (-(d as f64) / 2_000.0).exp()
            } else {
                (d as f64 / 150.0).exp()
            };
            ring * lp * env + if d == 0 { direct } else { 0.0 }
        })
        .collect()
}

fn config(gain_db: f64) -> ChainConfig {
    ChainConfig {
        eq: Some(EqProfile {
            name: "eq".into(),
            preamp_db: -6.0,
            bands: vec![Band {
                kind: FilterType::Peaking,
                freq_hz: 60.0,
                gain_db,
                q: 2.0,
                enabled: true,
            }],
            ..Default::default()
        }),
        convolution: vec![ConvolutionConfig {
            name: None,
            enabled: true,
            ir_path: "unused.wav".into(),
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
    }
}

fn chain(gain_db: f64, irs: &ConvIrs, workers: Option<&Arc<LanePool>>) -> Chain {
    chain_with(config(gain_db), irs, workers, BLOCK)
}

fn chain_with(
    config: ChainConfig,
    irs: &ConvIrs,
    workers: Option<&Arc<LanePool>>,
    block: usize,
) -> Chain {
    let mut c = Chain::new(ChainSpec {
        config,
        sample_rate: RATE,
        channels: 2,
        max_block: block,
        irs: Some(irs.clone()),
    });
    if let Some(pool) = workers {
        c.attach_workers(pool);
    }
    c
}

fn pool() -> Arc<LanePool> {
    let p = LanePool::spawn(1, |_| {}).unwrap();
    assert!(p.wait_started(Duration::from_secs(10)));
    p.set_min_wait(Duration::from_secs(60));
    p
}

fn input(block: usize, len: usize) -> Vec<f32> {
    let mut s = 1_000 + block as u64;
    (0..len * 2).map(|_| (lcg(&mut s) * 0.4) as f32).collect()
}

fn play(c: &mut Chain, block: usize) -> Vec<i32> {
    play_len(c, block, BLOCK)
}

fn play_len(c: &mut Chain, block: usize, len: usize) -> Vec<i32> {
    let mut out = vec![0i32; len * 2];
    let mut gain = StereoGainRamp::unity(RATE);
    c.process_f32_with_gain(&input(block, len), &mut out, &mut gain);
    out
}

fn per_channel() -> ConvIrs {
    ConvIrs::Channels(vec![ir(1), ir(2)])
}

fn matrix() -> ConvIrs {
    let mut m = FilterMatrix::new(2);
    m.set(0, 0, ir(1));
    m.set(1, 1, ir(2));
    m.set(0, 1, ir(3));
    ConvIrs::Matrix(m)
}

fn cal_matrix() -> ConvIrs {
    let mut m = FilterMatrix::new(2);
    m.set(0, 0, cal_ir(1, 0.7, 0.15));
    m.set(1, 1, cal_ir(2, 0.7, 0.15));
    m.set(0, 1, cal_ir(3, 0.0, 0.05));
    ConvIrs::Matrix(m)
}

/// Run `a` for longer than its IR (and a partial block on the grid), hand
/// it to `b`, then both play the same input.
fn continues(what: &str, mut a: Chain, mut b: Chain) {
    for k in 0..50 {
        play(&mut a, k);
    }
    assert!(
        b.inherit_filter_state(&mut a, usize::MAX),
        "{what}: handed over"
    );
    for k in 50..100 {
        assert_eq!(play(&mut a, k), play(&mut b, k), "{what}: block {k}");
    }
}

#[test]
fn a_chain_that_takes_over_plays_on_bit_for_bit() {
    let workers = pool();
    for (what, irs) in [("per channel", per_channel()), ("matrix", matrix())] {
        continues(what, chain(3.0, &irs, None), chain(3.0, &irs, None));
        continues(
            &format!("{what}, on workers"),
            chain(3.0, &irs, Some(&workers)),
            chain(3.0, &irs, Some(&workers)),
        );
    }
}

/// Two taps of +-1.9 20 ms apart (a null at 1 kHz), the preamp from 0 to
/// -12 dB. A 10 ms fade at the convolution's input would pass the null's
/// (zero) output by about a tap's height times the change (1.28 FS for a
/// 0.9 FS tone): the bound takes it over only with its longest fade
/// (500 ms), and refuses a mute (the edit warms a new chain up). A small
/// edit on the same IR takes the shortest fade; these edits on a
/// calibration-like one take over within 500 ms.
#[test]
fn the_fade_bound_decides_the_takeover() {
    let preamp = |db: f64| {
        let mut c = config(3.0);
        c.eq.as_mut().unwrap().preamp_db = db;
        c.eq.as_mut().unwrap().bands.clear();
        c
    };
    let mut far = vec![0.0; 2_000];
    far[0] = 1.9;
    far[960] = -1.9;
    let far = ConvIrs::Channels(vec![far]);
    let run = |irs: &ConvIrs, a: ChainConfig, b: ChainConfig| {
        let mut old = chain_with(a, irs, None, BLOCK);
        for k in 0..10 {
            play(&mut old, k);
        }
        let mut new = chain_with(b, irs, None, BLOCK);
        new.take_over_upstream(&mut old)
            .then(|| new.upstream_fade())
    };
    let big = run(&far, preamp(0.0), preamp(-12.0)).expect("taken over");
    eprintln!("far taps, preamp -12 dB: fade {big:?}");
    assert_eq!(big.map(|f| f.0), Some(24_000), "the longest fade");
    assert_eq!(
        run(&far, preamp(0.0), preamp(-200.0)),
        None,
        "a mute refused"
    );
    let small = run(&far, preamp(0.0), preamp(-0.2)).expect("a small edit takes over");
    eprintln!("far taps, preamp -0.2 dB: fade {small:?}");
    assert_eq!(small.map(|f| f.0), Some(480));
    let ms = |frames: usize| frames as f64 * 1000.0 / f64::from(RATE);
    for (what, a, b) in [
        ("band step 3 to 9 dB", config(3.0), config(9.0)),
        ("drag step 3 to 3.5 dB", config(3.0), config(3.5)),
        ("preamp 0 to -12 dB", preamp(0.0), preamp(-12.0)),
    ] {
        let (len, bound) = run(&cal_matrix(), a, b)
            .expect("taken over")
            .expect("a fade");
        eprintln!(
            "calibration, {what}: fade {:.1} ms, bound {bound:.3} FS",
            ms(len)
        );
        assert!(bound <= 0.125);
        assert!(ms(len) <= 500.0);
    }
    // A drag step's fade is the edited band's own (five of its time
    // constants), as on a delay without any bound: the IR adds nothing.
    let mut delay = vec![0.0; TAPS];
    delay[1_440] = 0.7;
    let delay = ConvIrs::Channels(vec![delay]);
    let fade = |irs: &ConvIrs| run(irs, config(3.0), config(3.5)).unwrap().unwrap().0;
    assert_eq!(fade(&cal_matrix()), fade(&delay));
}

/// A reset while a channel worker is late (its lane still out, the
/// pipeline stalled): that lane's convolver was not cleared, so the
/// pipeline is not marked clean, and the reset when the lane is back
/// clears it. Silence in after it plays silence out on both channels (the
/// late block's 0.75 FS does not come back).
#[test]
fn a_reset_while_a_worker_is_late_clears_its_lane_when_back() {
    const LONG: usize = 2_048;
    let mut delta = vec![0.0; 4_096];
    delta[0] = 1.0;
    let irs = ConvIrs::Channels(vec![delta]);
    let workers = LanePool::spawn(1, |_| {}).unwrap();
    assert!(workers.wait_started(Duration::from_secs(10)));
    let mut c = chain_with(config(0.0), &irs, Some(&workers), LONG);
    let mut out = vec![0i32; 2 * LONG];
    let mut gain = StereoGainRamp::unity(RATE);
    workers.stall_next(0, Duration::from_millis(200));
    c.process_f32_with_gain(&vec![0.75; 2 * LONG], &mut out, &mut gain);
    assert_eq!(c.worker_late_blocks(), 1, "the worker missed the block");
    assert!(!workers.is_idle(0));
    c.reset();
    let t0 = std::time::Instant::now();
    while !workers.is_idle(0) {
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "the worker never came back"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let mut peak = [0i32; 2];
    for _ in 0..4 {
        c.process_f32_with_gain(&vec![0.0; 2 * LONG], &mut out, &mut gain);
        for (i, &x) in out.iter().enumerate() {
            peak[i % 2] = peak[i % 2].max(x.abs());
        }
    }
    assert_eq!(peak, [0, 0], "what the late lane had came back");
}

/// A unity IR, a full-scale 20 Hz tone and a Q 30 bell at 20 Hz edited
/// from -12 to -13 dB. The edited section starts from empty state and rings
/// for seconds (its poles' time constant 0.23 s: a cut's poles sit at about
/// `Q A`, `A` its amplitude, 0.47): faded in at once it took the output to
/// 0.511 FS against settled 0.251 and 0.224. The takeover holds the old
/// stages (the fade's weight exactly 0) for the section's settling time
/// (6.9 time constants, to -60 dB: 1.56 s), then ramps: the output never
/// passes both settings' by more than the bound, and is the old chain's,
/// bit for bit, through the hold.
#[test]
fn a_cold_section_settles_before_it_is_faded_in() {
    const LONG: usize = 2_048;
    let hz = 20.0;
    let irs = ConvIrs::Channels(vec![vec![1.0]]);
    let bell = |gain_db: f64| {
        let mut c = config(0.0);
        let eq = c.eq.as_mut().unwrap();
        eq.preamp_db = 0.0;
        eq.bands = vec![Band {
            kind: FilterType::Peaking,
            freq_hz: hz,
            gain_db,
            q: 30.0,
            enabled: true,
        }];
        chain_with(c, &irs, None, LONG)
    };
    let tone = |k: usize| -> Vec<f32> {
        (k * LONG..(k + 1) * LONG)
            .flat_map(|n| {
                [(std::f64::consts::TAU * hz * n as f64 / f64::from(RATE)).cos() as f32; 2]
            })
            .collect()
    };
    let render = |c: &mut Chain, k: usize| {
        let mut out = vec![0i32; 2 * LONG];
        let mut gain = StereoGainRamp::unity(RATE);
        c.process_f32_with_gain(&tone(k), &mut out, &mut gain);
        out
    };
    let peak = |o: &[i32]| {
        o.iter().fold(0.0f64, |m, &x| {
            m.max((f64::from(x) / 2_147_483_648.0).abs())
        })
    };
    let (mut old, mut twin, mut settled) = (bell(-12.0), bell(-12.0), bell(-13.0));
    // 160 blocks, 6.8 s: both settled.
    for k in 0..160 {
        render(&mut old, k);
        render(&mut twin, k);
        render(&mut settled, k);
    }
    let mut new = bell(-13.0);
    assert!(new.take_over_upstream(&mut old));
    let hold = new.upstream_hold();
    let (len, bound) = new.upstream_fade().unwrap();
    let s = |f: usize| f as f64 / f64::from(RATE);
    assert!((1.5..1.65).contains(&s(hold)), "hold {} s", s(hold));
    let (mut got, mut refs) = (0.0f64, 0.0f64);
    for k in 160..160 + (hold + len).div_ceil(LONG) + 40 {
        let (x, a, b) = (
            render(&mut new, k),
            render(&mut twin, k),
            render(&mut settled, k),
        );
        if (k + 1 - 160) * LONG <= hold {
            assert_eq!(x, a, "block {k}: the hold plays the old chain");
        }
        got = got.max(peak(&x));
        refs = refs.max(peak(&a)).max(peak(&b));
    }
    eprintln!(
        "cold 20 Hz Q 30 section: hold {:.2} s, ramp {:.0} ms, bound {bound:.4}, peak {got:.4} FS \
         against the settings' {refs:.4}",
        s(hold),
        1000.0 * s(len)
    );
    assert!(got <= refs + bound + 0.01, "{got} over {refs} + {bound}");
}
