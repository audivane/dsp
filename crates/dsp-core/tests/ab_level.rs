//! A/B level matching and shared headroom on the built pair: the analytic
//! K-weighted offset against an offline LUFS measurement, the IR and
//! upsampler terms, the auto headroom, per-side over attribution and
//! block-size independence.

use audivane_dsp_core::Stage;
use audivane_dsp_core::ab_level::{AbLevel, AbOffset};
use audivane_dsp_core::biquad::Eq;
use audivane_dsp_core::chain::{AbChain, Chain, ChainSpec, Side};
use audivane_dsp_core::db_to_lin;
use audivane_dsp_core::loudness::{LufsMeter, Weighting};
use audivane_protocol::ipc::AbHeadroom;
use audivane_protocol::{
    Band, ChainConfig, ChannelEq, ConvolutionConfig, CrossfeedConfig, DitherKind, EqProfile,
    FilterType, LimiterConfig, LimiterMode, OutputConfig, RateTarget, TiltConfig,
};
use realfft::RealFftPlanner;
use rustfft::num_complex::Complex64;
use std::f64::consts::PI;

const FS: u32 = 48_000;
const BLOCK: usize = 1024;

fn output() -> OutputConfig {
    OutputConfig {
        bits: 32,
        dither: DitherKind::None,
        headroom_db: 0.0,
    }
}

fn config() -> ChainConfig {
    ChainConfig {
        output: output(),
        ..ChainConfig::neutral()
    }
}

fn spec(config: ChainConfig, irs: Option<Vec<Vec<f64>>>) -> ChainSpec {
    ChainSpec {
        config,
        sample_rate: FS,
        channels: 2,
        max_block: BLOCK,
        irs: irs.map(Into::into),
    }
}

fn band(kind: FilterType, freq_hz: f64, gain_db: f64, q: f64) -> Band {
    Band {
        kind,
        freq_hz,
        gain_db,
        q,
        enabled: true,
    }
}

fn eq(preamp_db: f64, bands: Vec<Band>) -> ChainConfig {
    ChainConfig {
        eq: Some(EqProfile {
            name: "test".into(),
            preamp_db,
            bands,
            ..Default::default()
        }),
        ..config()
    }
}

fn with_conv(mut c: ChainConfig) -> ChainConfig {
    c.convolution = vec![ConvolutionConfig {
        name: None,
        enabled: true,
        ir_path: "test.wav".into(),
        gain_db: 0.0,
        matrix: None,
        calibration: false,
    }];
    c
}

fn analytic(weighting: Weighting) -> AbLevel {
    AbLevel {
        offset: AbOffset::Analytic(weighting),
        headroom: AbHeadroom::Fixed { db: 0.0 },
    }
}

fn offset(a: ChainSpec, b: ChainSpec) -> f64 {
    AbChain::new(a, b, analytic(Weighting::K))
        .unwrap()
        .gains()
        .offset_db
}

struct Noise(u64);

impl Noise {
    fn next(&mut self) -> f64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        let r = self.0 ^ (self.0 >> 29);
        ((r >> 11) as f64 + 1.0) / (1u64 << 53) as f64
    }

    fn gauss(&mut self) -> f64 {
        let (u, v) = (self.next(), self.next());
        (-2.0 * u.ln()).sqrt() * (2.0 * PI * v).cos()
    }
}

/// Pink noise built in the frequency domain, power 1/f over the analytic
/// reference band (20 Hz to 20 kHz, zero outside), RMS 0.1.
fn pink(seed: u64, frames: usize, fs: f64) -> Vec<f64> {
    let n = frames.next_power_of_two();
    let ifft = RealFftPlanner::<f64>::new().plan_fft_inverse(n);
    let mut spec = ifft.make_input_vec();
    let mut noise = Noise(seed);
    let bins = spec.len();
    for (k, bin) in spec.iter_mut().enumerate() {
        let f = k as f64 * fs / n as f64;
        *bin = if !(20.0..=20_000.0).contains(&f) || k == bins - 1 {
            Complex64::new(0.0, 0.0)
        } else {
            Complex64::new(noise.gauss(), noise.gauss()) / f.sqrt()
        };
    }
    let mut out = ifft.make_output_vec();
    ifft.process(&mut spec, &mut out).unwrap();
    out.truncate(frames);
    let rms = (out.iter().map(|x| x * x).sum::<f64>() / frames as f64).sqrt();
    out.iter().map(|x| 0.1 * x / rms).collect()
}

/// Interleaved stereo pink noise (the analytic reference) with
/// inter-channel correlation `rho`.
fn stereo_pink(rho: f64, seconds: f64) -> Vec<f32> {
    let frames = (seconds * f64::from(FS)) as usize;
    let fs = f64::from(FS);
    let (c, u1, u2) = (
        pink(1, frames, fs),
        pink(2, frames, fs),
        pink(3, frames, fs),
    );
    let (a, b) = (rho.sqrt(), (1.0 - rho).sqrt());
    (0..frames)
        .flat_map(|i| [(a * c[i] + b * u1[i]) as f32, (a * c[i] + b * u2[i]) as f32])
        .collect()
}

/// Integrated loudness of `chain`'s output for `input`, skipping 1 s.
fn chain_lufs(spec: ChainSpec, input: &[f32]) -> f64 {
    let mut chain = Chain::new(spec);
    let rate = f64::from(chain.output_rate());
    let mut l = Vec::new();
    let mut r = Vec::new();
    let mut out = vec![0i32; chain.max_output_frames() * 2];
    for block in input.chunks(BLOCK * 2) {
        let n = chain.output_frames(block.len() / 2);
        chain.process_f32(block, &mut out);
        for f in out[..n * 2].as_chunks::<2>().0 {
            l.push(f64::from(f[0]) / 2_147_483_648.0);
            r.push(f64::from(f[1]) / 2_147_483_648.0);
        }
    }
    let skip = rate as usize;
    let mut meter = LufsMeter::new(rate, 2);
    meter.push(&[&l[skip..], &r[skip..]]);
    meter.integrated_lufs()
}

/// Test 3: the analytic K-weighted offset against an offline LUFS
/// measurement of 60 s of pink noise (correlation 0.5) through both chains.
#[test]
fn analytic_k_offset_matches_offline_lufs() {
    let input = stereo_pink(0.5, 60.0);
    let a = config();
    let reference = chain_lufs(spec(a.clone(), None), &input);
    let headphone = eq(
        -6.5,
        vec![
            band(FilterType::LowShelf, 105.0, 6.5, 0.71),
            band(FilterType::Peaking, 60.0, -1.5, 0.6),
            band(FilterType::Peaking, 180.0, -2.8, 0.9),
            band(FilterType::Peaking, 650.0, 1.6, 1.4),
            band(FilterType::Peaking, 1_400.0, -1.9, 2.0),
            band(FilterType::Peaking, 2_800.0, 3.4, 2.2),
            band(FilterType::Peaking, 4_200.0, -2.6, 3.0),
            band(FilterType::Peaking, 6_300.0, 4.1, 3.5),
            band(FilterType::Peaking, 8_900.0, -3.2, 4.0),
            band(FilterType::HighShelf, 10_000.0, 2.0, 0.71),
        ],
    );
    let mut tilt = config();
    tilt.eq = Some(EqProfile {
        name: "tilt".into(),
        tilt: Some(TiltConfig {
            enabled: true,
            slope_db_per_oct: None,
            total_db: Some(-3.0),
            ..TiltConfig::default()
        }),
        ..Default::default()
    });
    let crossfeed = ChainConfig {
        crossfeed: Some(CrossfeedConfig {
            width: 1.5,
            ..CrossfeedConfig::default()
        }),
        ..config()
    };
    let rows = [
        (
            "bass shelf +6 dB @105 Hz",
            eq(0.0, vec![band(FilterType::LowShelf, 105.0, 6.0, 0.71)]),
        ),
        (
            "treble shelf -4 dB @8 kHz",
            eq(0.0, vec![band(FilterType::HighShelf, 8_000.0, -4.0, 0.71)]),
        ),
        (
            "presence +4 dB @3 kHz",
            eq(0.0, vec![band(FilterType::Peaking, 3_000.0, 4.0, 1.0)]),
        ),
        ("tilt -3 dB total", tilt),
        ("10-band headphone EQ", headphone),
        ("crossfeed bs2b, width 1.5", crossfeed),
    ];
    let mut worst: f64 = 0.0;
    for (name, b) in rows {
        let analytic = offset(spec(a.clone(), None), spec(b.clone(), None));
        let measured = reference - chain_lufs(spec(b, None), &input);
        let err = analytic - measured;
        eprintln!("{name}: analytic {analytic:+.3} dB, LUFS {measured:+.3} dB, error {err:+.3} dB");
        worst = worst.max(err.abs());
        assert!(err.abs() < 0.1, "{name}: {analytic:+.3} vs {measured:+.3}");
    }
    eprintln!("worst error {worst:.3} dB");
}

/// Test 4: the IR magnitude enters the analytic offset exactly.
#[test]
fn ir_magnitude_enters_the_offset() {
    // One tap of 0.5: B is exactly 6.02 dB quieter.
    let half = offset(
        spec(config(), None),
        spec(with_conv(config()), Some(vec![vec![0.5]])),
    );
    let want = 20.0 * 2.0_f64.log10();
    assert!((half - want).abs() < 1e-9, "{half} vs {want}");

    // A shelf as an IR matches the same shelf as a biquad, offset and peak.
    let shelf = eq(0.0, vec![band(FilterType::LowShelf, 105.0, 6.0, 0.71)]);
    let mut biquad = Eq::new(shelf.eq.as_ref().unwrap(), f64::from(FS), 1).unwrap();
    let mut ir = vec![0.0; 1 << 16];
    ir[0] = 1.0;
    biquad.process(&mut [ir.as_mut_slice()]);
    let level = AbLevel {
        offset: AbOffset::Analytic(Weighting::K),
        headroom: AbHeadroom::Auto,
    };
    let as_eq = AbChain::new(spec(config(), None), spec(shelf, None), level)
        .unwrap()
        .gains();
    let as_ir = AbChain::new(
        spec(config(), None),
        spec(with_conv(config()), Some(vec![ir])),
        level,
    )
    .unwrap()
    .gains();
    assert!(
        (as_eq.offset_db - as_ir.offset_db).abs() < 0.01,
        "{as_eq:?} vs {as_ir:?}"
    );
    assert!(
        (as_eq.headroom_db - as_ir.headroom_db).abs() < 0.01,
        "{as_eq:?} vs {as_ir:?}"
    );
    assert!(as_eq.headroom_db > 5.0, "{as_eq:?}");
}

/// Per-channel EQ and IR combine per channel: a half-gain EQ on R and a
/// half-gain IR on L make B 6.02 dB quieter on both channels.
#[test]
fn stereo_eq_and_stereo_ir_combine_per_channel() {
    let half_db = 20.0 * 0.5_f64.log10();
    let mut b = with_conv(config());
    b.eq = Some(EqProfile {
        name: "right half".into(),
        right: Some(ChannelEq {
            enabled: true,
            preamp_db: half_db,
            bands: Vec::new(),
        }),
        ..Default::default()
    });
    let got = offset(
        spec(config(), None),
        spec(b, Some(vec![vec![0.5], vec![1.0]])),
    );
    assert!((got + half_db).abs() < 1e-9, "{got}");
}

/// The peak prediction keeps the EQ's attenuation: -6 dB channel preamps
/// and a +6 dB IR peak at 0 dB, so `auto` needs no headroom.
#[test]
fn attenuating_eq_offsets_an_ir_boost_in_the_peak() {
    let gain_db = 20.0 * 2.0_f64.log10();
    let cut = ChannelEq {
        enabled: true,
        preamp_db: -gain_db,
        bands: vec![band(FilterType::Peaking, 1_000.0, -3.0, 1.0)],
    };
    let mut b = with_conv(config());
    b.eq = Some(EqProfile {
        name: "cut".into(),
        left: Some(cut.clone()),
        right: Some(cut),
        ..Default::default()
    });
    let level = AbLevel {
        offset: AbOffset::Fixed(0.0),
        headroom: AbHeadroom::Auto,
    };
    let g = AbChain::new(spec(config(), None), spec(b, Some(vec![vec![2.0]])), level)
        .unwrap()
        .gains();
    assert!(g.headroom_db.abs() < 1e-6, "{g:?}");
}

/// The upsampler's magnitude enters the offset when the two designs differ,
/// and cancels when they match.
#[test]
fn upsampler_magnitude_enters_the_offset() {
    let up = |taps: usize, cutoff: f64| ChainConfig {
        upsampler: audivane_dsp_core::resample::ManualConfig {
            target: RateTarget::Ratio(2),
            taps,
            cutoff,
            ..Default::default()
        }
        .into_upsampler_config()
        .unwrap(),
        ..config()
    };
    let a = up(
        4096,
        audivane_dsp_core::resample::ManualConfig::default().cutoff,
    );
    let b = up(32, 0.5);
    assert_eq!(offset(spec(a.clone(), None), spec(a.clone(), None)), 0.0);
    let analytic = offset(spec(a.clone(), None), spec(b.clone(), None));
    let input = stereo_pink(0.5, 20.0);
    let measured = chain_lufs(spec(a, None), &input) - chain_lufs(spec(b, None), &input);
    eprintln!("upsampler: analytic {analytic:+.3} dB, LUFS {measured:+.3} dB");
    assert!(
        analytic > 0.2,
        "the slow filter should read quieter: {analytic}"
    );
    assert!(
        (analytic - measured).abs() < 0.15,
        "{analytic} vs {measured}"
    );
}

/// Test 2: with `auto` headroom a full-scale sine at a +6 dB shelf's peak
/// leaves the mix at or below 0 dBFS, with no clips and no overs.
#[test]
fn auto_headroom_keeps_a_shelf_boost_below_full_scale() {
    let level = AbLevel {
        offset: AbOffset::Analytic(Weighting::K),
        headroom: AbHeadroom::Auto,
    };
    let shelf = eq(0.0, vec![band(FilterType::LowShelf, 105.0, 6.0, 0.71)]);
    let mut ab = AbChain::new(spec(config(), None), spec(shelf, None), level).unwrap();
    let g = ab.gains();
    assert!(g.headroom_db > 5.0 && g.headroom_db < 6.0, "{g:?}");
    assert!(g.gain_a_db <= 0.0 && g.gain_b_db <= 0.0);
    ab.select(Side::B, 0);
    // 20 Hz, on the shelf's plateau, faded in over 0.5 s, then 2 s steady.
    let fs = f64::from(FS);
    let frames = (2.5 * fs) as usize;
    let input: Vec<f32> = (0..frames)
        .flat_map(|i| {
            let t = i as f64 / fs;
            let env = if t < 0.5 {
                0.5 - 0.5 * (PI * t / 0.5).cos()
            } else {
                1.0
            };
            let x = (env * (2.0 * PI * 20.0 * t).sin()) as f32;
            [x, x]
        })
        .collect();
    let mut out = vec![0i32; BLOCK * 2];
    let mut peak: f64 = 0.0;
    for block in input.chunks(BLOCK * 2) {
        let stats = ab.process_f32(block, &mut out);
        assert_eq!(stats.clipped, 0);
        peak = peak.max(stats.peak);
    }
    assert!(peak <= 1.0, "peak {peak}");
    assert!(
        peak > db_to_lin(-0.5),
        "the headroom is not excessive: {peak}"
    );
    assert_eq!(ab.overs(), [0.0, 0.0]);
}

/// Test 6: overs on B only count to B, and split by the fade weight.
#[test]
fn overs_are_attributed_to_the_side_that_made_them() {
    let gain = db_to_lin(6.0);
    let b = eq(6.0, Vec::new());
    let mut ab = AbChain::new(spec(config(), None), spec(b, None), AbLevel::fixed(0.0)).unwrap();
    // A pulse of 0.9 every 100 frames: over on B (x2), not on A.
    let pulses = |frames: usize| -> Vec<f32> {
        (0..frames)
            .flat_map(|i| {
                let x = if i % 100 == 0 { 0.9 } else { 0.0 };
                [x, x]
            })
            .collect()
    };
    let input = pulses(10_000);
    let mut out = vec![0i32; BLOCK * 2];
    let run = |ab: &mut AbChain, out: &mut [i32]| {
        for block in input.chunks(1000 * 2) {
            ab.process_f32(block, out);
        }
    };
    run(&mut ab, &mut out);
    assert_eq!(ab.overs(), [0.0, 0.0], "A alone makes no overs");
    ab.select(Side::B, 0);
    run(&mut ab, &mut out);
    assert_eq!(ab.overs(), [0.0, 100.0], "B's overs count to B");

    // Back to A over 2000 frames: the pulses inside the fade split by the
    // B weight there, while it is still enough to go over.
    let len = 2000;
    ab.select(Side::A, len);
    run(&mut ab, &mut out);
    let mut want = [0.0, 0.0];
    for k in (1..=10_000).step_by(100) {
        let w = if k >= len {
            0.0
        } else {
            1.0 - (0.5 - 0.5 * (PI * k as f64 / len as f64).cos())
        };
        if 0.9 * (1.0 - w) + 0.9 * gain * w > 1.0 {
            want[0] += 1.0 - w;
            want[1] += w;
        }
    }
    let got = ab.overs();
    assert!(want[0] > 0.5 && want[1] > 5.0, "{want:?}");
    assert!(
        (got[0] - want[0]).abs() < 1e-9 && (got[1] - want[1]).abs() < 1e-9,
        "{got:?} vs {want:?}"
    );
}

fn limited(mut c: ChainConfig, mode: LimiterMode) -> ChainConfig {
    c.limiter = LimiterConfig {
        mode,
        ..LimiterConfig::default()
    };
    c
}

/// Mono-in-stereo pulses of `amp` at `period` frames, from frame 100.
fn pulse_train(frames: usize, period: usize, amp: f32) -> Vec<f32> {
    (0..frames)
        .flat_map(|i| {
            let x = if i % period == 100 { amp } else { 0.0 };
            [x, x]
        })
        .collect()
}

fn feed(ab: &mut AbChain, input: &[f32]) {
    let mut out = vec![0i32; BLOCK * 2];
    for block in input.chunks(BLOCK * 2) {
        ab.process_f32(block, &mut out);
    }
}

/// Limiting onsets count to the side whose frames they limit, through the
/// limiter's delay: an onset for a pulse that entered on B still counts to
/// B after a hard cut to A.
#[test]
fn limiter_onsets_count_to_the_side_they_limit() {
    let a = limited(config(), LimiterMode::Limit);
    let b = limited(eq(6.0, Vec::new()), LimiterMode::Limit);
    let mut ab = AbChain::new(spec(a, None), spec(b, None), AbLevel::fixed(0.0)).unwrap();
    // 0.5 stays under the ceiling on A; B doubles it.
    let period = 24_000;
    let input = pulse_train(2 * period, period, 0.5);
    feed(&mut ab, &input);
    assert_eq!(ab.overs(), [0.0, 0.0], "A alone is not limited");
    ab.select(Side::B, 0);
    feed(&mut ab, &input);
    assert_eq!(ab.overs(), [0.0, 2.0], "each of B's pulses is one onset");
    // The pulse enters on B; the cut to A comes before it leaves.
    feed(&mut ab, &input[..101 * 2]);
    ab.select(Side::A, 0);
    feed(&mut ab, &input[101 * 2..period * 2]);
    assert_eq!(ab.overs(), [0.0, 1.0], "the delayed onset belongs to B");
}

/// In `auto_headroom` the held headroom absorbs repeats: one step, one over.
#[test]
fn held_headroom_counts_one_over_not_every_peak() {
    let a = limited(config(), LimiterMode::AutoHeadroom);
    let b = limited(eq(6.0, Vec::new()), LimiterMode::AutoHeadroom);
    let mut ab = AbChain::new(spec(a, None), spec(b, None), AbLevel::fixed(0.0)).unwrap();
    ab.select(Side::B, 0);
    feed(&mut ab, &pulse_train(48_000, 2_400, 0.5));
    assert_eq!(ab.overs(), [0.0, 1.0]);
}

/// A sample at exactly full scale clips the quantizer and counts.
#[test]
fn a_clip_at_exact_full_scale_counts() {
    let mut ab = AbChain::new(
        spec(config(), None),
        spec(config(), None),
        AbLevel::fixed(0.0),
    )
    .unwrap();
    feed(&mut ab, &pulse_train(1_000, 1_000, 1.0));
    assert_eq!(ab.overs(), [1.0, 0.0]);
}

/// Test 8: the pair's output (gains, ramps, fades, overs) does not depend
/// on the block size.
#[test]
fn ab_output_is_block_size_independent() {
    let mut a = limited(
        eq(-2.0, vec![band(FilterType::LowShelf, 105.0, 6.0, 0.71)]),
        LimiterMode::Limit,
    );
    let mut b = limited(
        eq(0.0, vec![band(FilterType::Peaking, 3_000.0, 4.0, 1.0)]),
        LimiterMode::Limit,
    );
    b.crossfeed = Some(CrossfeedConfig::default());
    // A fast release re-arms the onsets between the pulses.
    a.limiter.release_ms = 10.0;
    b.limiter.release_ms = 10.0;
    let level = AbLevel {
        offset: AbOffset::Analytic(Weighting::K),
        headroom: AbHeadroom::Fixed { db: 0.0 },
    };
    let frames = 20_000;
    // Pink noise with a loud pulse every 4000 frames.
    let pulses = pulse_train(frames, 4_000, 1.5);
    let input: Vec<f32> = stereo_pink(0.5, 1.0)[..frames * 2]
        .iter()
        .zip(&pulses)
        .map(|(x, p)| x + p)
        .collect();
    // The output and the over counts seen before each event and at the end.
    let run = |pattern: &[usize]| -> (Vec<i32>, Vec<[f64; 2]>) {
        let mut ab = AbChain::new(spec(a.clone(), None), spec(b.clone(), None), level).unwrap();
        let events = [5_000, 9_000, 12_000];
        let mut out = vec![0i32; BLOCK * 2];
        let mut all = Vec::with_capacity(frames * 2);
        let (mut pos, mut i) = (0, 0);
        let mut overs = Vec::new();
        while pos < frames {
            if events.contains(&pos) {
                overs.push(ab.overs());
            }
            if pos == events[0] {
                ab.select(Side::B, 960);
            } else if pos == events[1] {
                ab.set_level_offset_db(-2.0);
            } else if pos == events[2] {
                ab.select(Side::A, 0);
            }
            let next = events.iter().copied().find(|&e| e > pos).unwrap_or(frames);
            let n = pattern[i % pattern.len()].min(next - pos);
            i += 1;
            ab.process_f32(&input[pos * 2..(pos + n) * 2], &mut out);
            all.extend_from_slice(&out[..n * 2]);
            pos += n;
        }
        overs.push(ab.overs());
        (all, overs)
    };
    let reference = run(&[1024]);
    let total: f64 = reference.1.iter().map(|o| o[0] + o[1]).sum();
    assert!(total >= 2.0, "the run makes overs: {:?}", reference.1);
    for pattern in [&[1][..], &[7], &[1, 300, 17, 1024, 5, 999]] {
        let got = run(pattern);
        assert!(got.0 == reference.0, "{pattern:?}: output differs");
        assert_eq!(got.1, reference.1, "{pattern:?}");
    }
}
