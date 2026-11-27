use audivane_dsp_core::{
    Stage, db_to_lin,
    limiter::{Limiter, MAX_HEADROOM_DB},
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
