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
    DeviceCapabilities, FormatCapability, RateCapability, RateConflict, RatePolicy, SinkChoice,
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

const A441: u32 = 44_100;
const A48: u32 = 48_000;

#[test]
fn engine_mode_admits_the_source_rate_before_the_open() {
    // A DAC without the 44.1 kHz family; DSP on without upsampling.
    let caps = dac(&[48_000, 96_000, 192_000]);
    let configured = RatePolicy::default();
    let cfg = desired(
        true,
        RateTarget::Off,
        true,
        RatePairMask::ALL,
        &configured.allowed_rates,
    );
    let p = policies(&caps, cfg, &configured);
    // The DSP sink cannot play 44.1 kHz (x1 only): its policy keeps to its
    // playable rates.
    assert!(!p.dsp.allowed_rates.contains(&A441));
    // Direct runs at it through the engine; PipeWire is offered it.
    assert!(p.direct.allowed_rates.contains(&A441));
    assert!(p.published.allowed_rates.contains(&A441));
    assert_eq!(p.published.default_rate, p.dsp.default_rate);

    // Arbiter: a 44.1 kHz app into Direct keeps the graph at 44.1 kHz...
    assert_eq!(arbitrate(&p, A48, true, &[client(1, A441, 0)], &[]), A441);
    // ...the same app into the DSP sink still moves it to 48 kHz.
    assert_eq!(arbitrate(&p, A48, false, &[], &[client(1, A441, 0)]), A48);
    // Planner: the open keeps the graph, and lands on 160/147.
    assert_eq!(
        plan_graph_rate(&p, A441, SinkChoice::Direct),
        GraphRate::Keep
    );
    assert_eq!(
        landing(cfg, SinkChoice::Direct, A441, &caps),
        RateRatio::from_rates(A441, A48)
    );
    // The DSP sink at a 44.1 kHz graph still moves it (DSP on: no engine leg).
    assert_eq!(
        plan_graph_rate(&p, A441, SinkChoice::Dsp),
        GraphRate::Move(A48)
    );

    // DSP off: the DSP sink is a bypass path too.
    let off = desired(
        false,
        RateTarget::Off,
        true,
        RatePairMask::ALL,
        &configured.allowed_rates,
    );
    let p = policies(&caps, off, &configured);
    assert!(p.dsp.allowed_rates.contains(&A441));
    assert_eq!(arbitrate(&p, A48, false, &[], &[client(1, A441, 0)]), A441);
    assert_eq!(plan_graph_rate(&p, A441, SinkChoice::Dsp), GraphRate::Keep);
    assert_eq!(
        landing(off, SinkChoice::Dsp, A441, &caps),
        RateRatio::from_rates(A441, A48)
    );
    // Decimation: 352.8 kHz on this DAC lands on the closest, 192 kHz.
    assert_eq!(
        landing(off, SinkChoice::Dsp, 352_800, &caps),
        RateRatio::from_rates(352_800, 192_000)
    );
}

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

#[test]
fn pipewire_mode_and_unbuildable_pairs_move_the_graph_as_before() {
    let caps = dac(&[48_000, 96_000]);
    let configured = RatePolicy::default();
    let no_441 = RatePairMask::ALL.without(A441, A48);
    for cfg in [
        desired(
            true,
            RateTarget::Off,
            false,
            RatePairMask::ALL,
            &configured.allowed_rates,
        ),
        desired(
            true,
            RateTarget::Off,
            true,
            no_441,
            &configured.allowed_rates,
        ),
    ] {
        let p = policies(&caps, cfg, &configured);
        assert!(!p.direct.allowed_rates.contains(&A441));
        assert_eq!(arbitrate(&p, A48, true, &[client(1, A441, 0)], &[]), A48);
        // The graph moves to the native rate PipeWire picks.
        assert_eq!(
            plan_graph_rate(&p, A441, SinkChoice::Direct),
            GraphRate::Move(A48)
        );
        let rates = DeviceRates::playable(Some(&caps), &FORMATS);
        assert_eq!(
            cfg.ladder(SinkChoice::Direct, A441, rates).as_slice(),
            &[RateRatio::ONE]
        );
        // A device still away from x1 is handed to control.
        let r = RateRatio::from_rates(A441, A48);
        let k = key(cfg, SinkChoice::Direct, A441, &caps, r);
        assert_eq!(
            decide_rt_reopen(
                cfg,
                Some(k),
                SinkChoice::Direct,
                SinkChoice::Direct,
                A441,
                A441,
                r
            ),
            Some(RateRatio::ONE)
        );
    }
    // Without a probe nothing is known to be native: PipeWire converts.
    let cfg = desired(
        true,
        RateTarget::Off,
        true,
        RatePairMask::ALL,
        &configured.allowed_rates,
    );
    assert_eq!(cfg.bypass_landing(A441, None), None);
}

#[test]
fn conflicts_keep_their_rules() {
    let caps = dac(&[48_000, 96_000]);
    let configured = RatePolicy {
        conflict: RateConflict::Highest,
        ..RatePolicy::default()
    };
    let cfg = desired(
        true,
        RateTarget::Off,
        true,
        RatePairMask::ALL,
        &configured.allowed_rates,
    );
    let p = policies(&caps, cfg, &configured);
    // Direct: sticky, first come first served, the engine leg admitting
    // 44.1 kHz; a later 48 kHz app is resampled by PipeWire.
    let two = [client(1, A441, 0), client(2, A48, 1)];
    assert_eq!(arbitrate(&p, A441, true, &two, &[]), A441);
    assert_eq!(arbitrate(&p, A48, true, &two, &[]), A48);
    // Native first: 96 kHz plays natively when it started first.
    let native_first = [client(3, 96_000, 0), client(1, A441, 1)];
    assert_eq!(arbitrate(&p, 192_000, true, &native_first, &[]), 96_000);
    // DSP sink with DSP on: 44.1 kHz maps to 48 kHz, so both apps agree on
    // 48 kHz.
    assert_eq!(arbitrate(&p, A441, false, &[], &two), A48);
}

#[test]
fn reopen_decisions_follow_the_landing() {
    let caps = dac(&[48_000, 96_000]);
    let allowed = RatePolicy::default().allowed_rates;
    let engine = |dsp: bool, target: RateTarget, mask: RatePairMask| {
        desired(dsp, target, true, mask, &allowed)
    };
    let r48 = RateRatio::from_rates(A441, A48);
    let r96 = RateRatio::from_rates(A441, 96_000);
    let on = engine(true, RateTarget::Rate(96_000), RatePairMask::ALL);
    let off = engine(false, RateTarget::Rate(96_000), RatePairMask::ALL);
    // DSP on plays 44.1 -> 96 kHz; DSP off lands on 48 kHz: it reopens.
    let k = key(on, SinkChoice::Dsp, A441, &caps, r96);
    assert_eq!(
        decide_rt_reopen(
            off,
            Some(k),
            SinkChoice::Dsp,
            SinkChoice::Dsp,
            A441,
            A441,
            r96
        ),
        Some(r48)
    );
    // DSP on again: back to 96 kHz.
    let k = key(off, SinkChoice::Dsp, A441, &caps, r48);
    assert_eq!(
        decide_rt_reopen(
            on,
            Some(k),
            SinkChoice::Dsp,
            SinkChoice::Dsp,
            A441,
            A441,
            r48
        ),
        Some(r96)
    );
    // A DSP on chain at the landing's ratio: DSP off keeps the ratio (the
    // chains swap, no reopen).
    let on48 = engine(true, RateTarget::Rate(48_000), RatePairMask::ALL);
    let off48 = engine(false, RateTarget::Rate(48_000), RatePairMask::ALL);
    let k = key(on48, SinkChoice::Dsp, A441, &caps, r48);
    assert_eq!(
        decide_rt_reopen(
            off48,
            Some(k),
            SinkChoice::Dsp,
            SinkChoice::Dsp,
            A441,
            A441,
            r48
        ),
        None
    );
    // Direct takeover from the DSP sink at 96 kHz: Direct lands on 48 kHz.
    let k = key(on, SinkChoice::Dsp, A441, &caps, r96);
    assert_eq!(
        decide_rt_reopen(
            on,
            Some(k),
            SinkChoice::Dsp,
            SinkChoice::Direct,
            A441,
            A441,
            r96
        ),
        Some(r48)
    );
    // Direct running the engine leg: nothing to do.
    let k = key(on, SinkChoice::Direct, A441, &caps, r48);
    assert_eq!(
        decide_rt_reopen(
            on,
            Some(k),
            SinkChoice::Direct,
            SinkChoice::Direct,
            A441,
            A441,
            r48
        ),
        None
    );
    // A mask edit (the pair no longer builds): handed to control, which
    // moves the graph (the PipeWire path).
    let dropped = on.keep_bypass(RatePairMask::ALL.without(A441, A48));
    assert_eq!(
        decide_rt_reopen(
            dropped,
            Some(k),
            SinkChoice::Direct,
            SinkChoice::Direct,
            A441,
            A441,
            r48
        ),
        Some(RateRatio::ONE)
    );
    // The engine leg turned off: the same.
    let pipewire = desired(
        true,
        RateTarget::Rate(96_000),
        false,
        RatePairMask::ALL,
        &allowed,
    );
    assert_eq!(
        decide_rt_reopen(
            pipewire,
            Some(k),
            SinkChoice::Direct,
            SinkChoice::Direct,
            A441,
            A441,
            r48
        ),
        Some(RateRatio::ONE)
    );
    // A graph rate change to a native rate: x1.
    assert_eq!(
        decide_rt_reopen(
            on,
            Some(k),
            SinkChoice::Direct,
            SinkChoice::Direct,
            A441,
            A48,
            r48
        ),
        Some(RateRatio::ONE)
    );
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
fn converted_rates_add_to_the_native_base_never_replace_it() {
    // A 48 kHz-only DAC, allowed rates 44.1 and 88.2 kHz, DSP off. Without
    // the engine leg the policy widens to 48 kHz; with it, both configured
    // rates convert, and 48 kHz must stay.
    let caps = dac(&[A48]);
    let allowed = [A441, 88_200];
    let configured = RatePolicy {
        allowed_rates: allowed.to_vec(),
        default_rate: A441,
        ..RatePolicy::default()
    };
    let cfg = desired(false, RateTarget::Off, true, RatePairMask::ALL, &allowed);
    let p = policies(&caps, cfg, &configured);
    assert_eq!(p.dsp.allowed_rates, vec![A441, A48, 88_200]);
    assert_eq!(p.dsp_native.allowed_rates, vec![A48]);
    assert_eq!(p.dsp.default_rate, A48);
    assert_eq!(p.direct_native.allowed_rates, vec![A48]);
    for sink in [SinkChoice::Dsp, SinkChoice::Direct] {
        // A native 48 kHz source plays x1, untouched.
        assert_eq!(conversions(&p, cfg, &caps, sink, A48), (A48, 0), "{sink:?}");
        // 44.1 and 88.2 kHz: the engine converts them, once.
        assert_eq!(
            conversions(&p, cfg, &caps, sink, A441),
            (A441, 1),
            "{sink:?}"
        );
        assert_eq!(conversions(&p, cfg, &caps, sink, 88_200), (88_200, 1));
        // Any other rate: PipeWire resamples it into 48 kHz, once (never
        // into 88.2 kHz for the engine to convert again).
        for s in [96_000, 64_000, 192_000, 32_000] {
            assert_eq!(
                conversions(&p, cfg, &caps, sink, s),
                (A48, 1),
                "{s} {sink:?}"
            );
        }
        assert_eq!(
            plan_graph_rate(&p, 96_000, sink),
            GraphRate::Move(A48),
            "{sink:?}"
        );
    }
    // A pair the engine cannot build maps through the native base too.
    let no_441 = RatePairMask::ALL.without(A441, A48);
    let cfg = desired(false, RateTarget::Off, true, no_441, &allowed);
    let p = policies(&caps, cfg, &configured);
    assert_eq!(p.dsp.allowed_rates, vec![A48, 88_200]);
    assert_eq!(conversions(&p, cfg, &caps, SinkChoice::Dsp, A441), (A48, 1));
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
