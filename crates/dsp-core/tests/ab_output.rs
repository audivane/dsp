//! A/B output compare: B may differ from A in its dither, noise shaper and
//! effective bits. Two quantizers then run on the shared mix and the output
//! cuts from one quantized stream to the other at the middle of the
//! crossfade ([`AbChain::select`]).

use audivane_dsp_core::ab_level::AbLevel;
use audivane_dsp_core::chain::{AbChain, ChainSpec, Side};
use audivane_protocol::{
    Band, ChainConfig, DitherKind, EqProfile, FilterType, LimiterConfig, LimiterMode, OutputConfig,
    RateTarget,
};

const FS: u32 = 44_100;
const MAX_BLOCK: usize = 1024;

fn output(bits: u32, dither: DitherKind) -> OutputConfig {
    OutputConfig {
        bits,
        dither,
        headroom_db: 0.0,
    }
}

/// A with a small EQ boost at x2; B without the EQ. Both with the
/// limiter in `limiter` mode.
fn configs(
    out_a: OutputConfig,
    out_b: OutputConfig,
    limiter: LimiterMode,
) -> (ChainConfig, ChainConfig) {
    let base = ChainConfig {
        limiter: LimiterConfig {
            mode: limiter,
            ..LimiterConfig::default()
        },
        upsampler: audivane_protocol::UpsamplerConfig {
            target: RateTarget::Ratio(2),
            ..Default::default()
        },
        ..ChainConfig::neutral()
    };
    let a = ChainConfig {
        eq: Some(EqProfile {
            name: "test".into(),
            preamp_db: -1.0,
            bands: vec![Band {
                kind: FilterType::Peaking,
                freq_hz: 1_000.0,
                gain_db: 3.0,
                q: 1.0,
                enabled: true,
            }],
            ..Default::default()
        }),
        output: out_a,
        ..base.clone()
    };
    let b = ChainConfig {
        output: out_b,
        ..base
    };
    (a, b)
}

fn spec(config: ChainConfig) -> ChainSpec {
    ChainSpec {
        config,
        sample_rate: FS,
        channels: 2,
        max_block: MAX_BLOCK,
        irs: None,
    }
}

fn pair(out_a: OutputConfig, out_b: OutputConfig, limiter: LimiterMode) -> AbChain {
    let (a, b) = configs(out_a, out_b, limiter);
    AbChain::new(spec(a), spec(b), AbLevel::fixed(0.0)).unwrap()
}

/// [`pair`] with a fixed 256-tap upsampler: the default design's length
/// follows the output bits, and the references below need the same
/// filters whatever the output.
fn fixed_pair(out_a: OutputConfig, out_b: OutputConfig, limiter: LimiterMode) -> AbChain {
    let (mut a, mut b) = configs(out_a, out_b, limiter);
    let up = audivane_dsp_core::resample::ManualConfig {
        target: RateTarget::Ratio(2),
        taps: 256,
        ..Default::default()
    }
    .into_upsampler_config()
    .unwrap();
    a.upsampler = up.clone();
    b.upsampler = up;
    AbChain::new(spec(a), spec(b), AbLevel::fixed(0.0)).unwrap()
}

/// Interleaved stereo: two tones and a little noise, near full scale so the
/// limiter and the quantizer's clipping are reached now and then.
fn signal(frames: usize) -> Vec<f32> {
    let mut s = 0x1234_5678_u64;
    let mut out = Vec::with_capacity(frames * 2);
    for i in 0..frames {
        s = s
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let noise = ((s >> 11) as f64 / (1u64 << 53) as f64 - 0.5) * 0.02;
        let t = i as f64 / f64::from(FS);
        let l = 0.85 * (2.0 * std::f64::consts::PI * 997.0 * t).sin() + noise;
        let r = 0.85 * (2.0 * std::f64::consts::PI * 3_001.0 * t).sin() - noise;
        out.push(l as f32);
        out.push(r as f32);
    }
    out
}

/// Run `ab` over `input` in blocks of `sizes` (cycled), calling `select`
/// with the input frame where each block starts. Returns the output and
/// the over counts at the end.
fn run(
    ab: &mut AbChain,
    input: &[f32],
    sizes: &[usize],
    mut select: impl FnMut(&mut AbChain, usize),
) -> Vec<i32> {
    let frames = input.len() / 2;
    let mut out = Vec::new();
    let mut buf = vec![0i32; ab.max_output_frames() * 2];
    let mut pos = 0;
    let mut k = 0;
    while pos < frames {
        let n = sizes[k % sizes.len()].min(frames - pos);
        k += 1;
        select(ab, pos);
        let m = ab.output_frames(n);
        ab.process_f32(&input[pos * 2..(pos + n) * 2], &mut buf);
        out.extend_from_slice(&buf[..m * 2]);
        pos += n;
    }
    out
}

fn fnv(samples: &[i32]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325_u64;
    for s in samples {
        for b in s.to_le_bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    }
    h
}

/// Switches at block starts: B at 3000 input frames with a 10 ms fade, A
/// at 9000 with a hard cut, B at 15000 with a 5 ms fade.
fn switches(ab: &mut AbChain, pos: usize, last: &mut usize) {
    for (at, side, fade) in [
        (3000, Side::B, 882),
        (9000, Side::A, 0),
        (15000, Side::B, 441),
    ] {
        if *last < at && pos >= at {
            ab.select(side, fade);
        }
    }
    *last = pos;
}

/// With the same output on both sides, the pair keeps its one quantizer and
/// its output keeps the bits the hash pins (taken on the engine with one
/// quantizer).
#[test]
fn same_output_settings_are_bit_identical_to_one_quantizer() {
    let input = signal(20_000);
    let odd: &[usize] = &[7, 1000, 1, 513];
    for (out, limiter, sizes, hash) in [
        (
            output(16, DitherKind::Shaped),
            LimiterMode::Limit,
            &[1024][..],
            0x6232_d6d3_003e_44df,
        ),
        (
            output(24, DitherKind::Tpdf),
            LimiterMode::Off,
            &[1024],
            0xb0da_3963_d72e_70b4,
        ),
        (
            output(16, DitherKind::Resonant5),
            LimiterMode::AutoHeadroom,
            &[1024],
            0xcebe_32d7_8ac8_c19e,
        ),
        (
            output(20, DitherKind::None),
            LimiterMode::Off,
            &[1024],
            0xcfac_f144_ece9_cb87,
        ),
        (
            output(16, DitherKind::Shaped),
            LimiterMode::Off,
            odd,
            0x4e94_4611_c5ed_13e0,
        ),
    ] {
        let mut ab = pair(out, out, limiter);
        let mut last = 0;
        let mut got = run(&mut ab, &input, sizes, |ab, pos| {
            switches(ab, pos, &mut last)
        });
        // The overs since the last switch, as fixed point.
        got.extend(ab.overs().map(|o| (o * 1e6).round() as i32));
        assert_eq!(fnv(&got), hash, "{out:?} {limiter:?} {sizes:?}");
    }
}

/// A switch at an exact input frame: `(frame, side, fade in output frames)`.
type Switch = (usize, Side, usize);

const SWITCHES: [Switch; 4] = [
    (3_000, Side::B, 882),
    (9_000, Side::A, 0),
    (15_000, Side::B, 441),
    (21_000, Side::A, 300),
];

/// Run `ab` over `input` in blocks of `sizes` (cycled), split so that each
/// of `switches` is selected exactly at its input frame.
fn run_at(ab: &mut AbChain, input: &[f32], sizes: &[usize], switches: &[Switch]) -> Vec<i32> {
    let mut next = 0;
    let mut out = Vec::new();
    let frames = input.len() / 2;
    let mut buf = vec![0i32; ab.max_output_frames() * 2];
    let (mut pos, mut k) = (0, 0);
    while pos < frames {
        while next < switches.len() && switches[next].0 == pos {
            ab.select(switches[next].1, switches[next].2);
            next += 1;
        }
        let mut n = sizes[k % sizes.len()].min(frames - pos);
        if let Some(&(at, ..)) = switches.get(next) {
            n = n.min(at - pos);
        }
        k += 1;
        let m = ab.output_frames(n);
        ab.process_f32(&input[pos * 2..(pos + n) * 2], &mut buf);
        out.extend_from_slice(&buf[..m * 2]);
        pos += n;
    }
    out
}

/// The output frame where B's (or A's) quantizer takes over after a switch
/// at input `frame` with a fade of `fade` output frames: the first frame
/// whose B weight is at least one half (under one half for a switch to
/// A), delayed by the limiter. The weight is the pair's raised cosine,
/// evaluated the same way (position 1 is the first frame after `select`).
fn cut_frame(ratio: usize, limiter: usize, (frame, side, fade): Switch) -> usize {
    let (from, to) = match side {
        Side::B => (0.0, 1.0),
        Side::A => (1.0, 0.0),
    };
    let weight = |pos: usize| {
        if pos >= fade {
            return to;
        }
        let t = pos as f64 / fade as f64;
        let shape = 0.5 - 0.5 * (std::f64::consts::PI * t).cos();
        from + (to - from) * shape
    };
    let at = (1..)
        .find(|&pos| (weight(pos) >= 0.5) == (side == Side::B))
        .unwrap();
    frame * ratio + at - 1 + limiter
}

fn limiter_latency(limiter: LimiterMode) -> usize {
    use audivane_dsp_core::Stage;
    let (a, _) = configs(
        output(24, DitherKind::Tpdf),
        output(24, DitherKind::Tpdf),
        limiter,
    );
    audivane_dsp_core::limiter::Limiter::new(&a.limiter, FS * 2, 2)
        .map_or(0, |l| l.latency_frames())
}

/// Two outputs that differ: every frame comes from one of the two
/// quantizers run alone (a pair with that output on both sides) and the
/// cut lands where documented, for blocks of 1, 7, 1024 and variable
/// sizes. The step at the cut is a few LSB of the coarser quantizer.
#[test]
fn two_quantizers_cut_at_the_middle_of_the_fade() {
    let input = signal(24_000);
    let odd: &[usize] = &[7, 1000, 1, 513, 64];
    for (out_a, out_b, max_step_lsb) in [
        // Shaped noise at 16 bits reaches a few LSB (4.2 measured).
        (
            output(24, DitherKind::Tpdf),
            output(16, DitherKind::Shaped),
            8,
        ),
        (
            output(16, DitherKind::Tpdf),
            output(24, DitherKind::None),
            2,
        ),
        (
            output(20, DitherKind::Resonant5),
            output(16, DitherKind::Tpdf),
            2,
        ),
    ] {
        for limiter in [LimiterMode::Limit, LimiterMode::Off] {
            let lat = limiter_latency(limiter);
            let alone_a = run_at(
                &mut fixed_pair(out_a, out_a, limiter),
                &input,
                &[1024],
                &SWITCHES,
            );
            let alone_b = run_at(
                &mut fixed_pair(out_b, out_b, limiter),
                &input,
                &[1024],
                &SWITCHES,
            );
            let cuts: Vec<usize> = SWITCHES.iter().map(|&s| cut_frame(2, lat, s)).collect();
            let mut reference = None;
            for sizes in [&[1][..], &[7], &[1024], odd] {
                let mut ab = fixed_pair(out_a, out_b, limiter);
                assert!(ab.compares_output());
                let got = run_at(&mut ab, &input, sizes, &SWITCHES);
                assert_eq!(got.len(), alone_a.len());
                let what = format!("{out_a:?} -> {out_b:?}, {limiter:?}, blocks {sizes:?}");
                // Which side's quantizer each frame should come from.
                let mut side = Side::A;
                let mut next = 0;
                for f in 0..got.len() / 2 {
                    if next < cuts.len() && f == cuts[next] {
                        side = SWITCHES[next].1;
                        next += 1;
                    }
                    let want = match side {
                        Side::A => &alone_a,
                        Side::B => &alone_b,
                    };
                    assert_eq!(
                        &got[f * 2..f * 2 + 2],
                        &want[f * 2..f * 2 + 2],
                        "{what}: frame {f} should be {side}'s quantizer"
                    );
                }
                match &reference {
                    None => reference = Some(got),
                    Some(r) => assert!(r == &got, "{what}: depends on the block size"),
                }
            }
            // The step the cut adds: the two streams' difference there.
            let lsb = f64::from(1u32 << (32 - out_a.bits.min(out_b.bits)));
            for &c in &cuts {
                for ch in 0..2 {
                    let i = c * 2 + ch;
                    let step = (f64::from(alone_b[i]) - f64::from(alone_a[i])).abs() / lsb;
                    assert!(
                        step <= f64::from(max_step_lsb),
                        "{out_a:?} -> {out_b:?}: {step} LSB at frame {c}"
                    );
                }
            }
        }
    }
}

/// With two quantizers a clip counts whole to the side whose quantizer
/// produced the frame: A clips (its EQ boost), B does not.
#[test]
fn quantizer_clips_count_to_the_producing_side() {
    let input = signal(12_000);
    let (a, b) = (output(16, DitherKind::Tpdf), output(24, DitherKind::Tpdf));
    let mut ab = fixed_pair(a, b, LimiterMode::Off);
    run_at(&mut ab, &input, &[512], &[]);
    let [on_a, on_b] = ab.overs();
    assert!(on_a > 10.0 && on_a.fract() == 0.0, "{on_a}");
    assert_eq!(on_b, 0.0);
    // Switch to B over 10 ms: A's frames before the cut still clip and
    // count whole to A; B's frames never do.
    run_at(&mut ab, &input, &[512], &[(0, Side::B, 882)]);
    let [on_a, on_b] = ab.overs();
    assert_eq!(on_b, 0.0);
    assert!(on_a > 0.0 && on_a.fract() == 0.0, "{on_a}");
    // With one quantizer the same clips are split by the fade weight.
    let mut one = fixed_pair(a, a, LimiterMode::Off);
    run_at(&mut one, &input, &[512], &[]);
    run_at(&mut one, &input, &[512], &[(0, Side::B, 882)]);
    let [one_a, one_b] = one.overs();
    assert!(one_b > 0.0 && one_a.fract() != 0.0, "{one_a} {one_b}");
}

/// A B that differs only in bits and dither is chain A before the
/// quantizers, with the Recommended filter, whose automatic budget follows
/// the output: B's settings change its quantization only.
#[test]
fn a_bits_and_dither_compare_is_chain_a_before_the_quantizers() {
    use audivane_dsp_core::gain::StereoGainRamp;
    const RATE: u32 = 48_000;
    let side = |output| ChainConfig {
        upsampler: audivane_protocol::UpsamplerConfig {
            target: RateTarget::Ratio(2),
            preset: Some(audivane_protocol::RECOMMENDED_PRESET.into()),
            design: None,
        },
        output,
        ..ChainConfig::neutral()
    };
    let spec = |config| ChainSpec {
        config,
        sample_rate: RATE,
        channels: 2,
        max_block: MAX_BLOCK,
        irs: None,
    };
    let a = output(24, DitherKind::Tpdf);
    let input: Vec<f32> = (0..MAX_BLOCK * 2)
        .map(|i| {
            (0.5 * (2.0 * std::f64::consts::PI * 5_000.0 * (i / 2) as f64 / f64::from(RATE)).sin())
                as f32
        })
        .collect();
    // The pre-quantizer mix of a pair playing B from the start.
    let tap_of = |b| {
        let mut ab = AbChain::new(spec(side(a)), spec(side(b)), AbLevel::fixed(0.0)).unwrap();
        ab.select(Side::B, 0);
        let mut out = vec![0i32; ab.max_output_frames() * 2];
        let mut tap = vec![0f32; ab.max_output_frames() * 2];
        let mut all = Vec::new();
        for _ in 0..8 {
            let mut gain = StereoGainRamp::unity(RATE * 2);
            ab.process_f32_with_gain_tapped(&input, &mut out, &mut gain, Some(&mut tap));
            all.extend_from_slice(&tap);
        }
        (ab.compares_output(), all)
    };
    let (one, same) = tap_of(a);
    assert!(!one);
    for b in [
        output(16, DitherKind::Tpdf),
        output(16, DitherKind::Shaped),
        output(32, DitherKind::None),
        output(24, DitherKind::Integrator15),
    ] {
        let (two, got) = tap_of(b);
        assert!(two, "{b:?}");
        assert!(got.iter().any(|&x| x.abs() > 0.1), "silent");
        assert!(got == same, "{b:?}: B's stages differ from A's");
    }
}
