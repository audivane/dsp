use audivane_dsp_core::{
    Stage, db_to_lin,
    limiter::{Limiter, MAX_HEADROOM_DB, detection_factor},
};
use audivane_protocol::{LimiterConfig, LimiterMode};
use std::f64::consts::PI;

fn config(mode: LimiterMode) -> LimiterConfig {
    LimiterConfig {
        mode,
        ..Default::default()
    }
}

fn run(
    input: &[f64],
    ratio: usize,
    chunks: &[usize],
    mode: LimiterMode,
) -> (Vec<f64>, audivane_protocol::LimiterStatus) {
    run_at(input, 48_000 * ratio as u32, chunks, &config(mode))
}

fn run_at(
    input: &[f64],
    rate: u32,
    chunks: &[usize],
    config: &LimiterConfig,
) -> (Vec<f64>, audivane_protocol::LimiterStatus) {
    let mut limiter = Limiter::new(config, rate, 2).unwrap();
    let mut left = vec![0.0; input.len() + limiter.latency_frames() + 256];
    left[..input.len()].copy_from_slice(input);
    let mut right: Vec<_> = left.iter().map(|x| x * -0.25).collect();
    let mut pos = 0;
    let mut index = 0;
    let mut status = audivane_protocol::LimiterStatus::default();
    while pos < left.len() {
        let n = chunks[index % chunks.len()].min(left.len() - pos);
        limiter.process(&mut [&mut left[pos..pos + n], &mut right[pos..pos + n]]);
        let s = limiter.take_status();
        status.events += s.events;
        status.gain_reduction_db = status.gain_reduction_db.max(s.gain_reduction_db);
        status.headroom_db = s.headroom_db;
        status.active = s.active;
        index += 1;
        pos += n;
    }
    for (&l, &r) in left.iter().zip(&right) {
        assert_eq!(l * -0.25, r);
    }
    (left, status)
}

fn bessel_i0(x: f64) -> f64 {
    let mut term = 1.0_f64;
    let mut sum = 1.0_f64;
    for k in 1..200 {
        term *= x / 2.0 / k as f64;
        sum += term * term;
    }
    sum
}

/// Independent reference reconstruction: 4097-tap Kaiser (beta 10)
/// windowed sinc per phase, flat to about 0.499 cycles per sample, with
/// zero padding on both sides. Shares no code or design with the detector.
fn reconstructed_peak(input: &[f64], factor: usize) -> f64 {
    const HALF: i64 = 2048;
    let mut peak = input.iter().fold(0.0_f64, |p, x| p.max(x.abs()));
    let n = input.len() as i64;
    // padded[p] = input[p - 2 HALF]; zeros beyond both ends.
    let mut padded = vec![0.0; input.len() + 4 * HALF as usize + 2];
    padded[2 * HALF as usize..2 * HALF as usize + input.len()].copy_from_slice(input);
    for phase in 1..factor {
        let frac = phase as f64 / factor as f64;
        // y(i + frac) = sum_m x[i + m] h(m - frac), m = -HALF..=HALF.
        let mut taps: Vec<_> = (-HALF..=HALF)
            .map(|m| {
                let x = m as f64 - frac;
                let t = x / (HALF + 1) as f64;
                (PI * x).sin() / (PI * x) * bessel_i0(10.0 * (1.0 - t * t).max(0.0).sqrt())
            })
            .collect();
        let sum: f64 = taps.iter().sum();
        taps.iter_mut().for_each(|t| *t /= sum);
        for i in -HALF..n + HALF {
            let start = (i + HALF) as usize;
            let window = &padded[start..start + taps.len()];
            let mut acc = [0.0; 4];
            for (t, x) in taps.chunks(4).zip(window.chunks(4)) {
                for k in 0..t.len() {
                    acc[k] += t[k] * x[k];
                }
            }
            peak = peak.max((acc[0] + acc[1] + acc[2] + acc[3]).abs());
        }
    }
    peak
}

fn dbfs(x: f64) -> f64 {
    20.0 * x.log10()
}

/// Signs chosen to reinforce the sinc lobes around one half-sample point.
fn aligned_signs(len: usize, count: usize) -> Vec<f64> {
    let centre = len / 2;
    let half = (count / 2) as i64;
    (0..len)
        .map(|i| {
            let k = i as i64 - centre as i64;
            if (-half..=half).contains(&k) {
                let x = k as f64 - 0.5;
                ((PI * x).sin() / (PI * x)).signum()
            } else {
                0.0
            }
        })
        .collect()
}

#[test]
fn unity_is_exact_delay_and_reset_restores_it() {
    let input: Vec<_> = (0..4800)
        .map(|i| db_to_lin(-1.0) * (2.0 * PI * i as f64 / 48.0).sin())
        .collect();
    for ratio in [1, 2, 4, 8, 16, 32] {
        let (output, status) = run(&input, ratio, &[0, 1, 17, 257], LimiterMode::Limit);
        let delay = 240 * ratio;
        for (i, (&actual, &expected)) in output[delay..delay + input.len()]
            .iter()
            .zip(&input)
            .enumerate()
        {
            assert_eq!(actual, expected, "ratio {ratio}, frame {i}");
        }
        assert_eq!(status.headroom_db, 0.0);
        assert_eq!(status.gain_reduction_db, 0.0);
    }
    let mut limiter = Limiter::new(&config(LimiterMode::AutoHeadroom), 48_000, 1).unwrap();
    let mut loud = vec![4.0; 1000];
    limiter.process(&mut [&mut loud]);
    assert!(limiter.take_status().headroom_db > 0.0);
    limiter.reset();
    assert_eq!(limiter.take_status().headroom_db, 0.0);
    let mut impulse = vec![0.0; 500];
    impulse[0] = 0.25;
    limiter.process(&mut [&mut impulse]);
    assert_eq!(impulse[240], 0.25);
    assert_eq!(impulse.iter().filter(|&&x| x != 0.0).count(), 1);
}

fn faded_sine(len: usize, frequency: f64, amplitude: f64) -> Vec<f64> {
    let fade = 2400.0;
    (0..len)
        .map(|i| {
            let e = (i as f64 / fade).min((len - i) as f64 / fade).min(1.0);
            let w = 0.5 - 0.5 * (PI * e).cos();
            w * amplitude * (2.0 * PI * frequency * i as f64 + 0.3).sin()
        })
        .collect()
}

/// Run `input` (at 48 kHz) through the real upsampler first when `ratio > 1`.
fn upsampled(input: &[f64], ratio: usize) -> Vec<f64> {
    use audivane_dsp_core::resample::{ManualConfig, Upsampler, UpsamplerDesign};
    if ratio == 1 {
        return input.to_vec();
    }
    let design = UpsamplerDesign::try_from_config(&ManualConfig::default(), ratio).unwrap();
    let mut up = Upsampler::new(&design, 1, 256);
    let latency = up.latency_out_frames();
    let mut padded = input.to_vec();
    // Flush the whole kernel, then drop the leading delay.
    padded.resize(
        (input.len() + 2 * latency.div_ceil(ratio) + 512).div_ceil(256) * 256,
        0.0,
    );
    let mut out = vec![0.0; padded.len() * ratio];
    for (src, dst) in padded.chunks(256).zip(out.chunks_mut(256 * ratio)) {
        up.process(&[src], &mut [dst]);
    }
    out.drain(..latency);
    let peak = out.iter().fold(0.0_f64, |m, x| m.max(x.abs()));
    assert!(peak > 0.5, "upsampled signal missing");
    out
}

#[test]
fn detection_density_follows_the_rate_family() {
    for (rate, factor) in [
        (44_100, 8),
        (48_000, 8),
        (88_200, 4),
        (96_000, 4),
        (176_400, 2),
        (192_000, 2),
        (352_800, 1),
        (384_000, 1),
        (705_600, 1),
        (768_000, 1),
        (1_536_000, 1),
        (2_822_400, 1),
        (3_072_000, 1),
    ] {
        assert_eq!(detection_factor(rate), factor, "{rate} Hz");
    }
}

#[test]
fn the_limiter_runs_up_to_3072k_with_its_lookahead_in_time() {
    let config = LimiterConfig {
        mode: LimiterMode::Limit,
        ..Default::default()
    };
    for rate in [1_411_200u32, 1_536_000, 2_822_400, 3_072_000] {
        let mut limiter = Limiter::new(&config, rate, 2).unwrap();
        let expected = (f64::from(rate) * config.lookahead_ms / 1000.0).ceil() as usize;
        assert_eq!(limiter.latency_frames(), expected, "{rate} Hz");
        // A loud block comes out under the ceiling once the look-ahead
        // has passed.
        let n = expected * 4;
        let mut left = vec![1.5; n];
        let mut right = vec![-1.5; n];
        limiter.process(&mut [&mut left, &mut right]);
        let ceiling = 10f64.powf(config.ceiling_dbfs / 20.0);
        assert!(left[expected..].iter().all(|v| v.abs() <= ceiling + 1e-9));
    }
    let err = Limiter::new(&config, 3_072_001, 2).err().unwrap();
    assert!(err.contains("3072 kHz"), "{err}");
}

#[test]
fn adversarial_signals_respect_sample_and_reconstructed_ceiling() {
    let signals = [
        (
            "square",
            (0..4096)
                .map(|i| if i % 48 < 24 { 1.0 } else { -1.0 })
                .collect::<Vec<_>>(),
        ),
        (
            "fs/4 at 45 degrees",
            (0..4096)
                .map(|i| (PI * i as f64 / 2.0 + PI / 4.0).sin() * 2.0_f64.sqrt())
                .collect(),
        ),
        (
            "clipped sine",
            (0..4096)
                .map(|i| (2.0 * (2.0 * PI * i as f64 / 43.0).sin()).clamp(-1.0, 1.0))
                .collect(),
        ),
        (
            "impulse",
            (0..4096)
                .map(|i| if i == 1200 { 8.0 } else { 0.0 })
                .collect(),
        ),
        ("0.45 fs sine", faded_sine(8192, 0.45, 1.2)),
        ("0.49 fs sine", faded_sine(8192, 0.49, 1.2)),
        ("0.495 fs sine", faded_sine(8192, 0.495, 1.2)),
    ];
    let ceiling = db_to_lin(-0.5);
    for (name, input) in &signals {
        for mode in [LimiterMode::Limit, LimiterMode::AutoHeadroom] {
            let (output, _) = run(input, 1, &[0, 1, 17, 257], mode);
            assert!(output.iter().all(|x| x.abs() <= ceiling));
            let peak = reconstructed_peak(&output, 16);
            assert!(peak <= ceiling, "{name}, {mode:?}: {} dBFS", dbfs(peak));
        }
    }
}

/// The detector sees 192 taps, so sign sequences built to reinforce a much
/// longer sinc tail beat it at the source rate. Through an upsampler the
/// tail is already in the output samples and the ceiling holds.
#[test]
fn aligned_sign_stress_is_a_documented_source_rate_limit() {
    let ceiling = db_to_lin(-0.5);
    for count in [193, 1025] {
        let input = aligned_signs(4 * count + 4096, count);
        let (native, _) = run(&input, 1, &[256], LimiterMode::Limit);
        assert!(native.iter().all(|x| x.abs() <= ceiling));
        // Known overshoot at x1; bounded so a regression still shows.
        assert!(dbfs(reconstructed_peak(&native, 16)) < 3.5, "{count} signs");
        for ratio in [2, 4] {
            let (output, _) = run(&upsampled(&input, ratio), ratio, &[256], LimiterMode::Limit);
            let peak = reconstructed_peak(&output, 16 / ratio);
            assert!(
                peak <= ceiling,
                "{count} signs x{ratio}: {} dBFS",
                dbfs(peak)
            );
        }
    }
}

#[test]
fn block_geometry_is_irrelevant_and_headroom_holds() {
    let mut seed = 17u64;
    let input: Vec<_> = (0..5000)
        .map(|_| {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            ((seed >> 11) as f64 / (1u64 << 53) as f64 * 4.0 - 2.0).clamp(-1.0, 1.0)
        })
        .collect();
    for ratio in [1, 2, 4, 8, 16, 32] {
        for mode in [LimiterMode::Limit, LimiterMode::AutoHeadroom] {
            let (a, sa) = run(&input, ratio, &[1], mode);
            let (b, sb) = run(&input, ratio, &[0, 257, 17, 1, 63], mode);
            assert_eq!(a, b);
            assert_eq!(sa, sb);
            if mode == LimiterMode::AutoHeadroom {
                assert!(sa.headroom_db >= 1.0);
                assert!(sa.events > 0);
                assert_eq!(sa.headroom_db % 0.5, 0.0);
            }
        }
    }
}

#[test]
fn invalid_designs_return_errors_and_off_is_exact() {
    for bad in [f64::NAN, f64::INFINITY, -6.1, 0.1] {
        let mut c = config(LimiterMode::Limit);
        c.ceiling_dbfs = bad;
        assert!(Limiter::new(&c, 48_000, 2).is_err());
    }
    assert!(Limiter::new(&config(LimiterMode::Limit), 0, 2).is_err());
    assert!(Limiter::new(&config(LimiterMode::Limit), 48_000, 0).is_err());
    let mut limiter = Limiter::new(&config(LimiterMode::Off), 48_000, 1).unwrap();
    let mut data = [f64::MAX, -1.0, 0.0, 1.1];
    let original = data;
    limiter.process(&mut [&mut data]);
    assert_eq!(data, original);
    assert_eq!(limiter.latency_frames(), 0);
}

#[test]
fn chain_limits_after_volume_and_ab_level_match() {
    use audivane_dsp_core::{
        chain::{AbChain, Chain, ChainSpec, Side},
        gain::StereoGainRamp,
    };
    use audivane_protocol::{ChainConfig, DitherKind};
    let mut c = ChainConfig {
        limiter: config(LimiterMode::AutoHeadroom),
        ..ChainConfig::neutral()
    };
    c.output.dither = DitherKind::None;
    let spec = ChainSpec {
        config: c,
        sample_rate: 48_000,
        channels: 2,
        max_block: 1024,
        irs: None,
    };
    let input = vec![1.25f32; 2048];
    let mut out = vec![0; 2048];
    let mut tap = vec![0.0; 2048];
    let mut gain = StereoGainRamp::unity(48_000);
    gain.set_targets([db_to_lin(-10.0); 3], false);
    let mut chain = Chain::try_new(spec.clone()).unwrap();
    assert!(!chain.is_bit_transparent());
    assert_eq!(chain.latency_out_frames(), 240);
    let stats = chain.process_f32_with_gain_tapped(&input, &mut out, &mut gain, Some(&mut tap));
    assert_eq!(stats.limiter.headroom_db, 0.0);
    assert_eq!(stats.limiter.events, 0);
    assert_eq!(stats.clipped, 0);
    assert!((tap[1000] as f64 - 1.25 * db_to_lin(-10.0)).abs() < 1e-7);
    // Ramped, as the daemon does while streaming.
    gain.set_targets([1.0; 3], true);
    chain.process_f32_with_gain_tapped(&input, &mut out, &mut gain, Some(&mut tap));
    let stats = chain.process_f32_with_gain_tapped(&input, &mut out, &mut gain, Some(&mut tap));
    // The material's need (1.25 + 0.7 dB reserve: 3.0 dB) was learnt under
    // the volume; unity applies it at once. Only decisions that pull the
    // level down count as events, and -10 dB covered this one.
    assert_eq!(stats.limiter.headroom_db, 3.0);
    assert_eq!(stats.limiter.events, 0);
    assert_eq!(stats.clipped, 0);
    assert!(
        tap.iter()
            .all(|&x| f64::from(x.abs()) <= db_to_lin(-0.5) + 1e-7)
    );
    // The chain hands the volume to the limiter: -6 dB (ramped) covers the
    // 3 dB at once after the ramp block, and unity needs it again with no
    // new decision.
    let held = stats.limiter.headroom_db;
    gain.set_targets([db_to_lin(-6.0); 3], true);
    chain.process_f32_with_gain(&input, &mut out, &mut gain);
    let stats = chain.process_f32_with_gain(&input, &mut out, &mut gain);
    assert_eq!((stats.limiter.headroom_db, stats.limiter.events), (0.0, 0));
    gain.set_targets([1.0; 3], true);
    let ramp = chain.process_f32_with_gain(&input, &mut out, &mut gain);
    let stats = chain.process_f32_with_gain(&input, &mut out, &mut gain);
    assert_eq!(ramp.limiter.events, 0);
    assert_eq!((stats.limiter.headroom_db, stats.limiter.events), (held, 0));
    // A/B never boosts a side, so B's +12 dB comes from its own EQ preamp,
    // left unmatched (offset 0, no shared headroom).
    let mut spec_b = spec.clone();
    spec_b.config.eq = Some(audivane_protocol::EqProfile {
        preamp_db: 12.0,
        ..Default::default()
    });
    let mut ab = AbChain::new(
        spec,
        spec_b,
        audivane_dsp_core::ab_level::AbLevel::fixed(0.0),
    )
    .unwrap();
    ab.select(Side::B, 0);
    assert_eq!(ab.latency_out_frames(), 240);
    let stats = ab.process_f32_with_gain(&input, &mut out, &mut gain);
    // +12 dB on 1.25 would need 15 dB; held headroom stops at the cap.
    assert_eq!(stats.limiter.headroom_db, MAX_HEADROOM_DB);
    assert!(ab.limiter().is_some() && chain.limiter().is_some());
    assert_eq!(stats.clipped, 0);
}

fn sine(seconds: f64, amplitude: f64) -> Vec<f64> {
    (0..(seconds * 48_000.0) as usize)
        .map(|i| amplitude * (2.0 * PI * 1000.0 * i as f64 / 48_000.0).sin())
        .collect()
}

fn auto() -> LimiterConfig {
    config(LimiterMode::AutoHeadroom)
}

#[test]
fn one_huge_sample_holds_at_most_the_cap() {
    let mut input = vec![0.0; 1000];
    input[500] = 1e6;
    input.extend(sine(10.0, 0.5));
    let (output, status) = run_at(&input, 48_000, &[4800], &auto());
    // Exactly the capped held gain for the rest of the stream, not 120 dB.
    for seconds in [3, 9] {
        let i = seconds * 48_000;
        assert_eq!(output[i + 240], input[i] * db_to_lin(-MAX_HEADROOM_DB));
    }
    assert_eq!(status.headroom_db, MAX_HEADROOM_DB);
    assert_eq!(status.events, 1);
}

#[test]
fn volume_changes_move_the_applied_headroom_at_once_both_ways() {
    let mut limiter = Limiter::new(&auto(), 48_000, 1).unwrap();
    let mut events = 0;
    // `volume` is the digital gain ahead of the stage; feed what it outputs.
    let mut feed = |limiter: &mut Limiter, seconds, volume: f64| {
        limiter.set_volume_gain(volume);
        let input: Vec<_> = sine(seconds, 1.2).iter().map(|x| x * volume).collect();
        let mut output = input.clone();
        for block in output.chunks_mut(512) {
            limiter.process(&mut [block]);
            events += limiter.take_status().events;
        }
        (input, output)
    };
    // Material at 1.2 needs 2.5 dB at unity volume.
    feed(&mut limiter, 1.0, 1.0);
    assert_eq!(limiter.headroom_db(), 2.5);
    // -3 dB of volume covers it: no held attenuation, immediately, and
    // exact pass-through once the release has finished.
    limiter.set_volume_gain(db_to_lin(-3.0));
    assert_eq!(limiter.headroom_db(), 0.0);
    let (input, output) = feed(&mut limiter, 4.0, db_to_lin(-3.0));
    let tail = input.len() - 24_000;
    assert_eq!(&output[tail..], &input[tail - 240..input.len() - 240]);
    // -1 dB covers 1 dB of it; back to unity needs all 2.5 dB again.
    limiter.set_volume_gain(db_to_lin(-1.0));
    assert!((limiter.headroom_db() - 1.5).abs() < 1e-12);
    let (_, output) = feed(&mut limiter, 1.0, db_to_lin(-1.0));
    assert!(output.iter().all(|x| x.abs() <= db_to_lin(-0.5)));
    limiter.set_volume_gain(1.0);
    assert_eq!(limiter.headroom_db(), 2.5);
    // Mute: nothing to protect, and silence decides nothing.
    limiter.set_volume_gain(0.0);
    assert_eq!(limiter.headroom_db(), 0.0);
    feed(&mut limiter, 1.0, 0.0);
    limiter.set_volume_gain(1.0);
    assert_eq!(limiter.headroom_db(), 2.5);
    // One material decision in all of that.
    assert_eq!(events, 1);
}

/// Feed `seconds` of a 1 kHz sine per channel (`material[c]` before the
/// gain) through the limiter at per-channel gains `gains`; returns events.
fn feed_stereo(limiter: &mut Limiter, seconds: f64, material: [f64; 2], gains: [f64; 2]) -> u64 {
    limiter.set_channel_gains(|c| gains[c]);
    let unit = sine(seconds, 1.0);
    let mut left: Vec<_> = unit.iter().map(|x| x * material[0] * gains[0]).collect();
    let mut right: Vec<_> = unit.iter().map(|x| x * material[1] * gains[1]).collect();
    let mut events = 0;
    for (l, r) in left.chunks_mut(512).zip(right.chunks_mut(512)) {
        limiter.process(&mut [l, r]);
        events += limiter.take_status().events;
    }
    events
}

#[test]
fn the_cap_applies_to_the_applied_headroom_not_the_material() {
    // Material at 5.0 needs more than 12 dB at unity. At -6 dB of volume it
    // still needs more than 6 dB after the volume: held, not left to the
    // limiter for the rest of the stream.
    let mut limiter = Limiter::new(&auto(), 48_000, 2).unwrap();
    let low = db_to_lin(-6.0);
    let events = feed_stereo(&mut limiter, 1.0, [5.0, 5.0], [low, low]);
    // Default ceiling -0.5 dBFS, detector 0.2 dB below it, 0.5 dB steps.
    let unity_need = (20.0 * (5.0 / db_to_lin(-0.7)).log10() / 0.5).ceil() * 0.5;
    assert!(unity_need > MAX_HEADROOM_DB + 1.0, "{unity_need}");
    assert!(limiter.headroom_db() > MAX_HEADROOM_DB - 6.0 + 1.0);
    assert!(limiter.headroom_db() <= MAX_HEADROOM_DB);
    assert_eq!(events, 1);
    // Back at unity: exactly the cap.
    limiter.set_volume_gain(1.0);
    assert_eq!(limiter.headroom_db(), MAX_HEADROOM_DB);
}

#[test]
fn balance_changes_move_the_applied_headroom_at_once() {
    // Loud material on the right only, turned down 6 dB by the balance:
    // nothing to hold. Moving the balance to the right then holds the
    // right channel's 2.5 dB at once, without a new decision.
    let mut limiter = Limiter::new(&auto(), 48_000, 2).unwrap();
    let low = db_to_lin(-6.0);
    let mut events = feed_stereo(&mut limiter, 1.0, [0.1, 1.2], [1.0, low]);
    assert_eq!(limiter.headroom_db(), 0.0);
    limiter.set_channel_gains(|c| [low, 1.0][c]);
    assert_eq!(limiter.headroom_db(), 2.5);
    events += feed_stereo(&mut limiter, 1.0, [0.1, 1.2], [low, 1.0]);
    assert_eq!(limiter.headroom_db(), 2.5);
    assert_eq!(events, 0);
}

#[test]
fn a_need_learnt_at_unity_is_given_back_by_a_lower_volume() {
    // 5.0 needs 15 dB at unity: 12 dB applied (the cap), but 9 dB once the
    // volume is 6 dB lower, not 6.
    let mut limiter = Limiter::new(&auto(), 48_000, 2).unwrap();
    feed_stereo(&mut limiter, 1.0, [5.0, 5.0], [1.0, 1.0]);
    assert_eq!(limiter.headroom_db(), MAX_HEADROOM_DB);
    let low = db_to_lin(-6.0);
    limiter.set_volume_gain(low);
    assert!((limiter.headroom_db() - (15.0 + 20.0 * low.log10())).abs() < 1e-9);
}

#[test]
fn a_boosting_gain_does_not_panic_and_needs_more() {
    // A digital boost ahead of the stage (the daemon never does this, the
    // API allows it): no panic, and the need is measured after the boost.
    let mut limiter = Limiter::new(&auto(), 48_000, 2).unwrap();
    feed_stereo(&mut limiter, 0.5, [1.0, 1.0], [5.0, 5.0]);
    assert_eq!(limiter.headroom_db(), MAX_HEADROOM_DB);
    // Measured after the boost: 0.5 x 1.5 = 0.75 needs nothing, 0.8 x 1.5
    // = 1.2 needs 2.5 dB.
    let mut limiter = Limiter::new(&auto(), 48_000, 2).unwrap();
    feed_stereo(&mut limiter, 0.5, [0.5, 0.5], [1.5, 1.5]);
    assert_eq!(limiter.headroom_db(), 0.0);
    let mut limiter = Limiter::new(&auto(), 48_000, 2).unwrap();
    feed_stereo(&mut limiter, 0.5, [0.8, 0.8], [1.5, 1.5]);
    assert_eq!(limiter.headroom_db(), 2.5);
}

#[test]
fn what_a_ramp_learns_does_not_depend_on_block_size() {
    // A 50 ms fade to silence at 384 kHz with a peak halfway: the same held
    // headroom whether the ramp comes in one block or frame by frame.
    let rate = 384_000;
    let n = rate as usize / 20;
    let input: Vec<f64> = (0..n)
        .map(|i| {
            if i == n / 2 {
                3.0
            } else {
                0.1 * (i as f64 * 0.01).sin()
            }
        })
        .collect();
    let gain = |i: usize| 1.0 - i as f64 / n as f64;
    let run = |block: usize| {
        let mut limiter = Limiter::new(&auto(), rate, 1).unwrap();
        let mut pos = 0;
        while pos < n {
            let m = block.min(n - pos);
            let (before, after) = (gain(pos), gain(pos + m));
            limiter.set_channel_gains(|_| before.max(after));
            limiter.set_ramping(before != after);
            let mut x: Vec<f64> = (pos..pos + m).map(|i| input[i] * gain(i)).collect();
            limiter.process(&mut [&mut x]);
            pos += m;
        }
        limiter.set_volume_gain(1.0);
        limiter.headroom_db()
    };
    let whole = run(n);
    assert_eq!(whole, run(1));
    assert_eq!(whole, run(257));
}

#[test]
fn held_headroom_does_not_move_within_a_stream() {
    // 120 s at 44.1 kHz: 20 s verses peaking at -4 dBFS, 10 s choruses at
    // +1.5 dBTP. Held headroom is decided by the first chorus and then
    // stays: no release in the verses, no re-attack in each chorus.
    let rate = 44_100;
    let mut limiter = Limiter::new(&auto(), rate, 1).unwrap();
    let mut phase = 0.0_f64;
    let mut events = 0;
    let mut held_after_first_chorus = None;
    for second in 0..120 {
        let amplitude = if second % 30 >= 20 {
            db_to_lin(1.5)
        } else {
            db_to_lin(-4.0)
        };
        let mut block: Vec<_> = (0..rate)
            .map(|_| {
                phase += 2.0 * PI * 1000.0 / f64::from(rate);
                amplitude * phase.sin()
            })
            .collect();
        for chunk in block.chunks_mut(441) {
            limiter.process(&mut [chunk]);
            events += limiter.take_status().events;
            if let Some(held) = held_after_first_chorus {
                assert_eq!(limiter.headroom_db(), held, "second {second}");
            }
        }
        if second < 20 {
            assert_eq!(limiter.headroom_db(), 0.0);
        } else if held_after_first_chorus.is_none() {
            // 1.5 + 0.7 dB reserve, in 0.5 dB steps.
            assert_eq!(limiter.headroom_db(), 2.5);
            held_after_first_chorus = Some(2.5);
        }
    }
    assert_eq!(events, 1);
}

#[test]
fn release_snaps_to_unity_and_telemetry_reports_onsets_and_maximum() {
    let mut input = sine(9.0, 0.5);
    for (start, ms) in [(24_000, 10), (28_800, 10), (240_000, 10)] {
        for x in &mut input[start..start + ms * 48] {
            *x *= 3.0;
        }
    }
    let mut limiter = Limiter::new(&config(LimiterMode::Limit), 48_000, 1).unwrap();
    let mut output = input.clone();
    output.resize(input.len() + 240, 0.0);
    // First second in one block: the drained reduction is the burst's, not
    // the nearly released end-of-block value.
    limiter.process(&mut [&mut output[..48_000]]);
    let first = limiter.take_status();
    let needed = dbfs(1.5) + 0.7;
    assert!(first.gain_reduction_db > needed - 0.01, "{first:?}");
    // Two bursts 100 ms apart are one onset.
    assert_eq!(first.events, 1);
    limiter.process(&mut [&mut output[48_000..]]);
    let rest = limiter.take_status();
    assert_eq!(rest.events, 1);
    // Fully released between bursts and at the end: bit-exact again.
    for range in [180_000..230_000, input.len() - 30_000..input.len()] {
        for i in range {
            assert_eq!(output[i + 240], input[i], "frame {i}");
        }
    }
    assert_eq!(limiter.take_status().gain_reduction_db, 0.0);
}

#[test]
fn limit_onsets_re_arm_below_a_hundredth_of_a_db() {
    // Bursts 1 s apart: with a 100 ms release the reduction is far below
    // 0.01 dB by then (exact unity would take about 2.7 s), so two onsets.
    // With a 2000 ms release it is still about 2 dB down: one onset.
    for (release_ms, onsets) in [(100.0, 2), (2000.0, 1)] {
        let mut input = sine(3.0, 0.5);
        for start in [24_000, 72_000] {
            for x in &mut input[start..start + 480] {
                *x *= 3.0;
            }
        }
        let mut c = config(LimiterMode::Limit);
        c.release_ms = release_ms;
        let (_, status) = run_at(&input, 48_000, &[512], &c);
        assert_eq!(status.events, onsets, "release {release_ms} ms");
    }
}

#[test]
fn held_headroom_carries_over_only_with_the_same_config_and_rate() {
    let mut old = Limiter::new(&auto(), 48_000, 1).unwrap();
    let mut loud = sine(0.5, 1.2);
    old.process(&mut [&mut loud]);
    assert_eq!(old.headroom_db(), 2.5);
    let mut same = Limiter::new(&auto(), 48_000, 1).unwrap();
    same.set_volume_gain(db_to_lin(-1.0));
    assert!(same.inherit_headroom(&old));
    // The material's need carries over; this chain's own volume applies.
    assert!((same.take_status().headroom_db - 1.5).abs() < 1e-12);
    // A rate change is usually a new track: start fresh.
    let mut faster = Limiter::new(&auto(), 96_000, 1).unwrap();
    assert!(!faster.inherit_headroom(&old));
    assert_eq!(faster.headroom_db(), 0.0);
    let mut other = auto();
    other.ceiling_dbfs = -1.0;
    let mut different = Limiter::new(&other, 48_000, 1).unwrap();
    assert!(!different.inherit_headroom(&old));
    assert_eq!(different.headroom_db(), 0.0);
    let mut limit = Limiter::new(&config(LimiterMode::Limit), 48_000, 1).unwrap();
    assert!(!limit.inherit_headroom(&old));
}

#[test]
#[ignore = "measurement report; run explicitly with --nocapture"]
fn limiter_measurements() {
    use std::time::Instant;
    let len = 8192;
    let mut seed = 41u64;
    let music: Vec<_> = (0..len)
        .map(|i| {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let noise = (seed >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0;
            let t = i as f64 / 48_000.0;
            (1.2 * (2.0 * PI * 997.0 * t).sin()
                + 0.6 * (2.0 * PI * 3091.0 * t).sin()
                + 0.3 * (2.0 * PI * 7919.0 * t).sin()
                + 0.35 * noise)
                .clamp(-1.0, 1.0)
        })
        .collect();
    let square: Vec<_> = (0..len)
        .map(|i| if i % 48 < 24 { 1.0 } else { -1.0 })
        .collect();
    let abrupt: Vec<_> = (0..len)
        .map(|i| 1.2 * (2.0 * PI * 0.49 * i as f64 + 0.3).sin())
        .collect();
    let cases = [
        ("loud mixture", music),
        ("square", square),
        ("0.49 fs sine, faded", faded_sine(len, 0.49, 1.2)),
        ("0.49 fs sine, abrupt edges", abrupt),
        ("aligned signs, 193", aligned_signs(len, 193)),
        ("aligned signs, 1025", aligned_signs(len, 1025)),
        ("aligned signs, 4097", aligned_signs(16_384, 4097)),
    ];
    println!(
        "| Signal | Ratio | Input peak | Limit: peak | Limit: energy | Auto: peak | Auto: held / events |"
    );
    for (name, input) in &cases {
        for ratio in [1, 2, 8] {
            let signal = upsampled(input, ratio);
            // Reference density: 16 points per 48 kHz sample.
            let reference = (16 / ratio).max(2);
            let input_peak = reconstructed_peak(&signal, reference);
            let input_energy: f64 = signal.iter().map(|x| x * x).sum();
            let (limited, _) = run(&signal, ratio, &[256], LimiterMode::Limit);
            let (auto, status) = run(&signal, ratio, &[256], LimiterMode::AutoHeadroom);
            let energy: f64 = limited.iter().map(|x| x * x).sum();
            println!(
                "| {name} | x{ratio} | {:+.2} | {:+.2} | {:+.2} | {:+.2} | {:.1} dB / {} |",
                dbfs(input_peak),
                dbfs(reconstructed_peak(&limited, reference)),
                10.0 * (energy / input_energy).log10(),
                dbfs(reconstructed_peak(&auto, reference)),
                status.headroom_db,
                status.events
            );
        }
    }
    // CPU: limiter alone, stereo, 256-frame calls on a loud signal that
    // keeps it limiting; median of five one-second runs.
    let rates = [
        44_100u32, 48_000, 88_200, 96_000, 176_400, 192_000, 352_800, 384_000, 705_600, 768_000,
    ];
    println!("| Source | Ratio | Output | Detector phases | Limit | Auto |");
    for &source in &rates {
        for ratio in [1u32, 2, 4, 8] {
            let rate = source * ratio;
            if rate > 768_000 {
                continue;
            }
            let mut row = Vec::new();
            for mode in [LimiterMode::Limit, LimiterMode::AutoHeadroom] {
                let mut limiter = Limiter::new(&config(mode), rate, 2).unwrap();
                let source_block: Vec<_> = (0..256)
                    .map(|i| 1.5 * (2.0 * PI * 997.0 * i as f64 / rate as f64).sin())
                    .collect();
                let (mut left, mut right) = (vec![0.0; 256], vec![0.0; 256]);
                let calls = (rate as usize).div_ceil(256);
                let mut runs: Vec<f64> = (0..5)
                    .map(|_| {
                        let start = Instant::now();
                        for _ in 0..calls {
                            left.copy_from_slice(&source_block);
                            right.copy_from_slice(&source_block);
                            limiter.process(&mut [&mut left, &mut right]);
                            std::hint::black_box(&left);
                        }
                        100.0 * start.elapsed().as_secs_f64() / (calls as f64 * 256.0 / rate as f64)
                    })
                    .collect();
                runs.sort_by(f64::total_cmp);
                row.push(runs[2]);
            }
            println!(
                "| {} | x{ratio} | {} | {} | {:.2}% | {:.2}% |",
                source,
                rate,
                detection_factor(rate) - 1,
                row[0],
                row[1]
            );
        }
    }
}
