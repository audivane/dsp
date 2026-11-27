//! The engine bypass resampler's rate policy (Direct and DSP off at a rate
//! the DAC does not play natively), on the driver's pure pieces: the
//! landing function, the effective policies, the rate arbiter, the
//! control's graph-rate plan, the open ladder and the RT thread's reopen
//! check. They all decide through `DesiredDspConfig::bypass_landing`, so
//! they agree; a rate the DAC plays natively stays x1 (bit-perfect).

use audivane_io_pw::lifecycle::{
    BypassLeg, DesiredDspConfig, DeviceRates, NegotiatedKey, RatePairMask, Rungs, decide_rt_reopen,
    negotiated_key,
};
use audivane_io_pw::rate_arbiter::{RateClient, RateInputs, choose_rate};
use audivane_io_pw::rate_caps::{
    GraphRate, RatePolicies, effective_rate_policy, plan_graph_rate, rate_policies,
};
use audivane_protocol::ipc::{
    DeviceCapabilities, FormatCapability, RateCapability, RatePolicy, SinkChoice,
};
use audivane_protocol::{ALLOWED_RATES, RateRatio, RateTarget, SampleFormat};

const FORMATS: [SampleFormat; 1] = [SampleFormat::S32];

fn dac(rates: &[u32]) -> DeviceCapabilities {
    DeviceCapabilities {
        device: "hw:CARD=USB,DEV=0".into(),
        channels: 2,
        channels_min: 2,
        channels_max: 2,
        rates: rates
            .iter()
            .map(|&rate| RateCapability {
                rate,
                formats: vec![FormatCapability {
                    format: SampleFormat::S32,
                    sbits: Some(32),
                }],
            })
            .collect(),
        rate_min: rates.iter().copied().min().unwrap_or(0),
        rate_max: rates.iter().copied().max().unwrap_or(0),
        formats: vec![SampleFormat::S32],
        modes: Vec::new(),
    }
}

/// DSP on (`dsp`) or off, the upsampler at `target`, the engine leg on
/// (`engine`) with `mask`, configured rates `allowed`.
fn desired(
    dsp: bool,
    target: RateTarget,
    engine: bool,
    mask: RatePairMask,
    allowed: &[u32],
) -> DesiredDspConfig {
    DesiredDspConfig {
        dsp_enabled: dsp,
        rate_target: target,
        bypass: BypassLeg {
            engine,
            buildable: mask,
            allowed: BypassLeg::allowed_bits(allowed),
        },
        ..DesiredDspConfig::default()
    }
}

fn policies(
    caps: &DeviceCapabilities,
    cfg: DesiredDspConfig,
    configured: &RatePolicy,
) -> RatePolicies {
    let eff = effective_rate_policy(configured, caps, cfg, &FORMATS, ALLOWED_RATES)
        .unwrap()
        .policy;
    rate_policies(eff, Some(caps), &FORMATS, cfg)
}

fn client(id: u32, native_rate: u32, since: u64) -> RateClient {
    RateClient {
        id,
        name: format!("app{id}"),
        native_rate,
        since,
    }
}

/// The rate the arbiter runs the graph at for `direct` clients on Direct
/// (`direct_active`) or `dsp` clients on the DSP sink.
fn arbitrate(
    p: &RatePolicies,
    current: u32,
    direct_active: bool,
    direct: &[RateClient],
    dsp: &[RateClient],
) -> u32 {
    choose_rate(&RateInputs {
        policy: &p.published,
        current,
        direct_active,
        direct,
        dsp,
        direct_policy: Some(&p.direct),
        dsp_policy: Some(&p.dsp),
        direct_native: Some(&p.direct_native),
        dsp_native: Some(&p.dsp_native),
    })
    .unwrap()
    .rate
}

fn landing(
    cfg: DesiredDspConfig,
    sink: SinkChoice,
    g: u32,
    caps: &DeviceCapabilities,
) -> RateRatio {
    let rates = DeviceRates::playable(Some(caps), &FORMATS);
    cfg.open_landing(sink, g, rates, &Rungs::EMPTY).unwrap()
}

fn key(
    cfg: DesiredDspConfig,
    sink: SinkChoice,
    g: u32,
    caps: &DeviceCapabilities,
    q: RateRatio,
) -> NegotiatedKey {
    let rates = DeviceRates::playable(Some(caps), &FORMATS);
    negotiated_key(cfg, sink, g, rates, &audivane_io_alsa::RatioOpen::at(q))
}

const A48: u32 = 48_000;

#[test]
fn a_native_rate_stays_x1_in_every_mode() {
    // The DAC plays the source rate: Direct and DSP off stay x1, whatever
    // the engine leg's setting.
    let caps = dac(&[44_100, 48_000, 88_200, 96_000]);
    let configured = RatePolicy::default();
    for engine in [true, false] {
        for dsp in [true, false] {
            let cfg = desired(
                dsp,
                RateTarget::Off,
                engine,
                RatePairMask::ALL,
                &configured.allowed_rates,
            );
            let p = policies(&caps, cfg, &configured);
            let pipewire = policies(
                &caps,
                desired(
                    dsp,
                    RateTarget::Off,
                    false,
                    RatePairMask::NONE,
                    &configured.allowed_rates,
                ),
                &configured,
            );
            let rates = DeviceRates::playable(Some(&caps), &FORMATS);
            for g in [44_100, 48_000, 88_200, 96_000] {
                assert_eq!(cfg.bypass_landing(g, rates), None, "{g}");
                assert_eq!(
                    cfg.ladder(SinkChoice::Direct, g, rates).as_slice(),
                    &[RateRatio::ONE]
                );
                assert_eq!(landing(cfg, SinkChoice::Direct, g, &caps), RateRatio::ONE);
                if !dsp {
                    assert_eq!(landing(cfg, SinkChoice::Dsp, g, &caps), RateRatio::ONE);
                }
                assert_eq!(arbitrate(&p, A48, true, &[client(1, g, 0)], &[]), g);
                assert_eq!(plan_graph_rate(&p, g, SinkChoice::Direct), GraphRate::Keep);
                // Running at x1: nothing reopens.
                let k = key(cfg, SinkChoice::Direct, g, &caps, RateRatio::ONE);
                assert_eq!(
                    decide_rt_reopen(
                        cfg,
                        Some(k),
                        SinkChoice::Direct,
                        SinkChoice::Direct,
                        g,
                        g,
                        RateRatio::ONE
                    ),
                    None
                );
            }
            // Only native rates on this DAC among the defaults: the engine
            // leg adds none, so Direct's rates are PipeWire mode's.
            let mut native: Vec<u32> = p.direct.allowed_rates.clone();
            native.retain(|r| [44_100, 48_000, 88_200, 96_000].contains(r));
            assert_eq!(native, pipewire.direct.allowed_rates);
        }
    }
}

/// A small deterministic generator (xorshift64*).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn subset(&mut self, of: &[u32]) -> Vec<u32> {
        let bits = self.next();
        of.iter()
            .enumerate()
            .filter(|(i, _)| bits & (1 << i) != 0)
            .map(|(_, &r)| r)
            .collect()
    }
}

#[test]
fn arbiter_planner_and_ladder_agree_on_every_landing() {
    // Random DACs (native rate sets), configured rates, bypass masks and
    // DSP on/off: whatever Direct is admitted at, the planner keeps and
    // the open lands on (x1 when native, else the landing's ratio to a
    // native rate); and the landing is exactly where PipeWire's move would
    // have taken the graph without the engine leg.
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut checked = 0usize;
    for _ in 0..3000 {
        let device = rng.subset(ALLOWED_RATES);
        let allowed = rng.subset(ALLOWED_RATES);
        if device.is_empty() || allowed.is_empty() {
            continue;
        }
        let caps = dac(&device);
        let dsp = rng.next() & 1 == 0;
        let seed = rng.next();
        let mask = RatePairMask::from_fn(|i, o| {
            !(u64::from(i ^ o.rotate_left(7)) ^ seed).is_multiple_of(5)
        });
        let configured = RatePolicy {
            allowed_rates: allowed.clone(),
            default_rate: allowed[0],
            ..RatePolicy::default()
        };
        let cfg = desired(dsp, RateTarget::Off, true, mask, &allowed);
        let pw_cfg = desired(dsp, RateTarget::Off, false, RatePairMask::NONE, &allowed);
        let Ok(eff) = effective_rate_policy(&configured, &caps, cfg, &FORMATS, ALLOWED_RATES)
        else {
            continue;
        };
        let p = rate_policies(eff.policy, Some(&caps), &FORMATS, cfg);
        let pw = policies(&caps, pw_cfg, &configured);
        let rates = DeviceRates::playable(Some(&caps), &FORMATS);
        for &g in ALLOWED_RATES {
            let native = device.contains(&g);
            let land = cfg.bypass_landing(g, rates);
            if native {
                assert_eq!(land, None);
            }
            // Where PipeWire would have moved Direct without the engine leg.
            let pw_move = match plan_graph_rate(&pw, g, SinkChoice::Direct) {
                GraphRate::Move(r) => Some(r),
                _ => None,
            };
            if let Some(out) = land {
                assert!(device.contains(&out), "{g} -> {out} on {device:?}");
                assert!(mask.allows(g, out));
                if allowed.iter().any(|r| device.contains(r)) {
                    assert_eq!(Some(out), pw_move, "{g} on {device:?}, allowed {allowed:?}");
                }
            }
            for sink in [SinkChoice::Direct, SinkChoice::Dsp] {
                if sink == SinkChoice::Dsp && dsp {
                    continue;
                }
                let admitted = if sink == SinkChoice::Direct {
                    p.direct.allowed_rates.contains(&g)
                } else {
                    p.dsp.allowed_rates.contains(&g)
                };
                if !admitted {
                    continue;
                }
                checked += 1;
                // Admitted: the arbiter runs a lone client there...
                let (d, s) = if sink == SinkChoice::Direct {
                    (vec![client(1, g, 0)], vec![])
                } else {
                    (vec![], vec![client(1, g, 0)])
                };
                assert_eq!(arbitrate(&p, g, sink == SinkChoice::Direct, &d, &s), g);
                // ...the planner keeps it...
                assert_eq!(plan_graph_rate(&p, g, sink), GraphRate::Keep);
                // ...and the open lands where the landing says.
                let q = landing(cfg, sink, g, &caps);
                match land {
                    Some(out) => assert_eq!(q, RateRatio::from_rates(g, out)),
                    None => {
                        assert!(native, "{g} admitted on {device:?} without a landing");
                        assert_eq!(q, RateRatio::ONE);
                    }
                }
                // And the RT thread, running there, does not reopen.
                let k = key(cfg, sink, g, &caps, q);
                assert_eq!(decide_rt_reopen(cfg, Some(k), sink, sink, g, g, q), None);
            }
        }
    }
    assert!(checked > 1000, "{checked}");
}

/// How many times a lone client at `s` is converted on `sink`: PipeWire
/// resamples it when the arbiter runs the graph at another rate, and the
/// engine bypass resampler converts the graph rate when it lands
/// elsewhere (Direct, or the DSP sink with DSP off). Returns the graph
/// rate too.
fn conversions(
    p: &RatePolicies,
    cfg: DesiredDspConfig,
    caps: &DeviceCapabilities,
    sink: SinkChoice,
    s: u32,
) -> (u32, usize) {
    let direct = sink == SinkChoice::Direct;
    let (d, x) = if direct {
        (vec![client(1, s, 0)], vec![])
    } else {
        (vec![], vec![client(1, s, 0)])
    };
    let g = arbitrate(p, s, direct, &d, &x);
    let engine = cfg.is_bypass(sink)
        && cfg
            .bypass_landing(g, DeviceRates::playable(Some(caps), &FORMATS))
            .is_some();
    (g, usize::from(g != s) + usize::from(engine))
}

#[test]
fn no_native_rate_is_converted_and_no_source_twice() {
    // Random DACs, configured rates, masks and DSP on/off, every source
    // rate on Direct and (with DSP off) the DSP sink: a source that plays
    // untouched without the engine leg still plays untouched, and nothing is
    // converted more than once.
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let mut checked = 0usize;
    for _ in 0..3000 {
        let device = rng.subset(ALLOWED_RATES);
        let allowed = rng.subset(ALLOWED_RATES);
        if device.is_empty() || allowed.is_empty() {
            continue;
        }
        let caps = dac(&device);
        let dsp = rng.next() & 1 == 0;
        let seed = rng.next();
        let mask = RatePairMask::from_fn(|i, o| {
            !(u64::from(i ^ o.rotate_left(7)) ^ seed).is_multiple_of(5)
        });
        let configured = RatePolicy {
            allowed_rates: allowed.clone(),
            default_rate: allowed[0],
            ..RatePolicy::default()
        };
        let cfg = desired(dsp, RateTarget::Off, true, mask, &allowed);
        let pw_cfg = desired(dsp, RateTarget::Off, false, RatePairMask::NONE, &allowed);
        let (Ok(eff), Ok(pw_eff)) = (
            effective_rate_policy(&configured, &caps, cfg, &FORMATS, ALLOWED_RATES),
            effective_rate_policy(&configured, &caps, pw_cfg, &FORMATS, ALLOWED_RATES),
        ) else {
            continue;
        };
        let p = rate_policies(eff.policy, Some(&caps), &FORMATS, cfg);
        let pw = rate_policies(pw_eff.policy, Some(&caps), &FORMATS, pw_cfg);
        for sink in [SinkChoice::Direct, SinkChoice::Dsp] {
            if sink == SinkChoice::Dsp && dsp {
                continue;
            }
            if sink == SinkChoice::Direct && pw.direct.allowed_rates.is_empty() {
                // Direct could not play here before at all.
                continue;
            }
            for &s in ALLOWED_RATES {
                let (g, n) = conversions(&p, cfg, &caps, sink, s);
                let (pw_g, pw_n) = conversions(&pw, pw_cfg, &caps, sink, s);
                let ctx =
                    format!("{s} on {sink:?}, DAC {device:?}, allowed {allowed:?}, dsp {dsp}");
                assert!(n <= 1, "{ctx}: converted {n} times (graph {g})");
                assert!(n <= pw_n, "{ctx}: {n} conversions, {pw_n} before");
                if pw_n == 0 {
                    assert!(device.contains(&s) && pw_g == s, "{ctx}");
                    assert_eq!((g, n), (s, 0), "{ctx}: a native rate is converted");
                }
                checked += 1;
            }
        }
    }
    assert!(checked > 10_000, "{checked}");
}
