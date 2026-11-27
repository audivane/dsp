//! Which graph rates the engine can actually offer PipeWire, given what the
//! output device accepts.
//!
//! Pure logic over a probed [`DeviceCapabilities`] so it can be tested with
//! fake devices. The driver publishes the result (not the configured
//! `rate.allowed_rates`) to `clock.allowed-rates`, the sinks'
//! `audio.allowed-rates` and their format pods, so PipeWire resamples streams
//! at rates the device cannot play instead of switching the graph to them.

use crate::lifecycle::DesiredDspConfig;
use audivane_io_alsa::FORMAT_PREFERENCE;
use audivane_protocol::SampleFormat;
use audivane_protocol::ipc::{DeviceCapabilities, RatePolicy, SinkChoice};

/// What an open does with the graph rate ([`plan_graph_rate`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphRate {
    /// Open for the graph rate as it is.
    Keep,
    /// Ask PipeWire to move the graph to this rate, and open for it.
    Move(u32),
    /// Direct cannot play on the device at all (no native rate).
    DirectCannotPlay,
}

/// Where an open of `sink` at `graph_rate` runs the graph under the
/// device's [`RatePolicies`]. Direct runs at its own rates (native x1, or
/// converted by the engine bypass resampler), so another rate moves the
/// graph to one of them (PipeWire resamples the stream into it); for the DSP
/// sink, a rate it cannot play at any ratio moves it too. A move goes to
/// the sink's native base ([`crate::rate_arbiter::sink_rate`]): a rate the
/// engine converts is never a move target, so nothing is converted twice.
pub fn plan_graph_rate(p: &RatePolicies, graph_rate: u32, sink: SinkChoice) -> GraphRate {
    use crate::rate_arbiter::sink_rate;
    if sink == SinkChoice::Direct {
        if p.direct.allowed_rates.is_empty() {
            return GraphRate::DirectCannotPlay;
        }
        if !p.direct.allowed_rates.contains(&graph_rate) {
            return GraphRate::Move(sink_rate(graph_rate, &p.direct, Some(&p.direct_native)));
        }
        // Direct's rates are its own: with the engine bypass resampler they
        // include rates the DSP sink cannot play.
        return GraphRate::Keep;
    }
    let eff = &p.dsp;
    if eff.allowed_rates.is_empty() || eff.allowed_rates.contains(&graph_rate) {
        return GraphRate::Keep;
    }
    GraphRate::Move(if eff.follow_source {
        sink_rate(graph_rate, eff, Some(&p.dsp_native))
    } else {
        eff.default_rate
    })
}

/// A graph that is not moving: opens keep asking PipeWire to move it off
/// `from` (to `to`) and it stays (another driver or client fixes the
/// rate). Each such open reopens the device for nothing, so one deadline
/// runs per unresolved move: [`MOVE_GIVE_UP_NS`] after its first attempt
/// the open fails with an error instead (the DSP path keeps working). Only
/// an observed landing (the RT thread saw the graph at the rate it opened
/// for: `landings` moved on) or another request starts a new one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MoveAttempts {
    pub from: u32,
    pub to: u32,
    first_ns: u64,
    landings: u64,
}

/// How long an unresolved move may go on before the open gives up.
pub const MOVE_GIVE_UP_NS: u64 = 3_000_000_000;

impl MoveAttempts {
    /// Note an attempt to move the graph from `from` to `to` at `now_ns`,
    /// `landings` being the RT thread's count of observed landings; `true`
    /// when this move has stayed unresolved for [`MOVE_GIVE_UP_NS`].
    pub fn note(slot: &mut Option<Self>, from: u32, to: u32, now_ns: u64, landings: u64) -> bool {
        match slot {
            Some(a) if a.from == from && a.to == to && a.landings == landings => {
                now_ns.saturating_sub(a.first_ns) >= MOVE_GIVE_UP_NS
            }
            _ => {
                *slot = Some(Self {
                    from,
                    to,
                    first_ns: now_ns,
                    landings,
                });
                false
            }
        }
    }
}

/// The rate policy the driver runs with, plus what was dropped and why.
#[derive(Clone, Debug, PartialEq)]
pub struct EffectiveRates {
    /// `allowed_rates` and `default_rate` restricted to the device; the other
    /// fields as configured.
    pub policy: RatePolicy,
    /// Configured rates the device cannot play at any ratio.
    pub unsupported: Vec<u32>,
    /// The configured set had no playable rate, so `policy.allowed_rates`
    /// came from the engine's full rate list instead.
    pub widened: bool,
}

/// Can the graph run at `rate` on this device: does it accept
/// `rate * k` for some ratio `k` on the fallback ladder from the desired
/// ratio down to 1 ([`DesiredDspConfig::ratio_ladder_for`], rational
/// ratios and decimation included), with one of `formats`?
pub fn graph_rate_playable(
    caps: &DeviceCapabilities,
    rate: u32,
    desired: DesiredDspConfig,
    formats: &[SampleFormat],
) -> bool {
    let formats: &[SampleFormat] = if formats.is_empty() {
        &FORMAT_PREFERENCE
    } else {
        formats
    };
    // DSP off opens x1 only: a rate the device lacks natively is not
    // playable then, so the graph moves to a native one.
    desired
        .ladder(
            SinkChoice::Dsp,
            rate,
            crate::lifecycle::DeviceRates::playable(Some(caps), formats),
        )
        .as_slice()
        .iter()
        .filter_map(|k| k.output_rate(rate))
        .any(|out| formats.iter().any(|&f| caps.supports(out, f)))
}

fn family(rate: u32) -> u32 {
    if rate.is_multiple_of(11_025) {
        44_100
    } else if rate.is_multiple_of(8_000) {
        48_000
    } else {
        0
    }
}

/// Nearest of `candidates` to `want`: same rate family first (44.1k vs
/// 48k multiples), then smallest distance, then 48 kHz on a tie.
pub fn nearest_rate(want: u32, candidates: &[u32]) -> Option<u32> {
    candidates
        .iter()
        .copied()
        .min_by_key(|&r| (family(r) != family(want), r.abs_diff(want), r != 48_000))
}

/// Restrict `configured` to what the device can play.
///
/// Allowed = configured ∩ playable without the engine bypass resampler
/// (natively, or through the DSP chain). If that is empty, every such rate
/// in `engine_rates` (the engine's full list); if still empty, an error
/// that says what the device supports. The default rate is kept when
/// playable, else the [`nearest_rate`]. With DSP off, the configured rates
/// the engine bypass resampler converts ([`DesiredDspConfig::bypass_landing`])
/// are added on top of that native base, never in its place: a native rate
/// the base widens to stays, so it is never converted.
pub fn effective_rate_policy(
    configured: &RatePolicy,
    caps: &DeviceCapabilities,
    desired: DesiredDspConfig,
    formats: &[SampleFormat],
    engine_rates: &[u32],
) -> Result<EffectiveRates, String> {
    // Nothing usable at all: say which parameter the device rejects (channel
    // count, rates outside the engine's list, formats) and what it takes.
    if let Some(reason) = caps.unusable_reason() {
        return Err(reason);
    }
    let native = desired.native_only();
    let playable = |r: u32| graph_rate_playable(caps, r, native, formats);
    let (mut allowed, mut unsupported): (Vec<u32>, Vec<u32>) = configured
        .allowed_rates
        .iter()
        .copied()
        .partition(|&r| playable(r));
    let mut widened = false;
    if allowed.is_empty() {
        allowed = engine_rates
            .iter()
            .copied()
            .filter(|&r| playable(r))
            .collect();
        widened = true;
    }
    if allowed.is_empty() {
        let with = if formats.is_empty() {
            String::new()
        } else {
            let names: Vec<&str> = formats.iter().map(|f| f.name()).collect();
            format!(" in the configured formats ({})", names.join(", "))
        };
        return Err(format!(
            "device '{}' supports none of the engine's sample rates{with} (device supports {})",
            caps.device,
            caps.summary()
        ));
    }
    allowed.sort_unstable();
    allowed.dedup();
    let default_rate = if allowed.contains(&configured.default_rate) {
        configured.default_rate
    } else {
        nearest_rate(configured.default_rate, &allowed).unwrap_or(allowed[0])
    };
    if desired.is_bypass(SinkChoice::Dsp) {
        let rates = crate::lifecycle::DeviceRates::playable(Some(caps), formats);
        unsupported.retain(|&r| {
            let converted = desired.bypass_landing(r, rates).is_some();
            if converted && !allowed.contains(&r) {
                allowed.push(r);
            }
            !converted
        });
        allowed.sort_unstable();
    }
    Ok(EffectiveRates {
        policy: RatePolicy {
            allowed_rates: allowed,
            default_rate,
            ..configured.clone()
        },
        unsupported,
        widened,
    })
}

/// The rate policy of the Direct (bit-perfect) sink. Direct never runs the
/// DSP chain: it runs at effective rates the device plays natively (x1)
/// with one of `formats`, and, with the engine bypass resampler, at the
/// configured or effective rates the engine converts to a native one
/// ([`DesiredDspConfig::bypass_landing`]). The default is the effective
/// default when Direct plays it natively, else the [`nearest_rate`] native
/// one. Without a probe the effective policy is used as is.
/// `allowed_rates` is empty when Direct runs at no effective rate: it
/// cannot play on this device then.
pub fn direct_rate_policy(
    effective: &RatePolicy,
    caps: Option<&DeviceCapabilities>,
    formats: &[SampleFormat],
    desired: DesiredDspConfig,
) -> RatePolicy {
    split_direct_policy(effective, caps, formats, desired).0
}

/// Direct's policy and its native base (the rates it plays at x1).
fn split_direct_policy(
    effective: &RatePolicy,
    caps: Option<&DeviceCapabilities>,
    formats: &[SampleFormat],
    desired: DesiredDspConfig,
) -> (RatePolicy, RatePolicy) {
    let Some(caps) = caps else {
        return (effective.clone(), effective.clone());
    };
    let formats: &[SampleFormat] = if formats.is_empty() {
        &FORMAT_PREFERENCE
    } else {
        formats
    };
    let native_at = |r: u32| formats.iter().any(|&f| caps.supports(r, f));
    let mut native: Vec<u32> = effective
        .allowed_rates
        .iter()
        .copied()
        .filter(|&r| native_at(r))
        .collect();
    if native.is_empty() && desired.bypass.engine {
        // No effective rate is native (the DSP chain upsamples them all):
        // Direct still has a native base, so a source the engine does not
        // convert lands at a native rate (once), never at a converted one.
        native = audivane_protocol::ALLOWED_RATES
            .iter()
            .copied()
            .filter(|&r| native_at(r))
            .collect();
    }
    let mut allowed = native.clone();
    let rates = crate::lifecycle::DeviceRates::playable(Some(caps), formats);
    let configured = audivane_protocol::ALLOWED_RATES
        .iter()
        .enumerate()
        .filter(|(i, _)| desired.bypass.allowed & (1 << i) != 0)
        .map(|(_, &r)| r);
    for r in configured.chain(effective.allowed_rates.iter().copied()) {
        if !allowed.contains(&r) && desired.bypass_landing(r, rates).is_some() {
            allowed.push(r);
        }
    }
    allowed.sort_unstable();
    let default_rate = if native.contains(&effective.default_rate) {
        effective.default_rate
    } else {
        nearest_rate(effective.default_rate, &native)
            .or_else(|| nearest_rate(effective.default_rate, &allowed))
            .unwrap_or(effective.default_rate)
    };
    (
        RatePolicy {
            allowed_rates: allowed,
            default_rate,
            ..effective.clone()
        },
        RatePolicy {
            allowed_rates: native,
            default_rate,
            ..effective.clone()
        },
    )
}

/// The rate policies the driver runs with on one device.
///
/// Each sink has its rates and a native base: the rates it plays without
/// the engine bypass resampler. The engine's converted rates are added on
/// top of the base, never in its place, and a client is only run at a
/// converted rate when it plays at exactly that rate; any other rate maps
/// through the base ([`crate::rate_arbiter::sink_rate`]). So a native rate
/// is never converted, and no source is converted twice (PipeWire, then
/// the engine).
#[derive(Clone, Debug, PartialEq)]
pub struct RatePolicies {
    /// What PipeWire is offered (`clock.allowed-rates`, the sinks' rates)
    /// and status shows: the DSP sink's rates and Direct's together. The
    /// default is the DSP sink's.
    pub published: RatePolicy,
    /// The rates the DSP sink plays ([`effective_rate_policy`]).
    pub dsp: RatePolicy,
    /// Direct's rates ([`direct_rate_policy`]).
    pub direct: RatePolicy,
    /// The DSP sink's native base: `dsp` without the converted rates.
    pub dsp_native: RatePolicy,
    /// Direct's native base: the rates it plays at x1.
    pub direct_native: RatePolicy,
    /// Every engine rate ([`audivane_protocol::ALLOWED_RATES`]) the device
    /// plays with the current settings, configured or not
    /// ([`playable_rates`]). Empty without a probe.
    pub playable: Vec<u32>,
}

/// The [`RatePolicies`] for the DSP sink's effective policy `dsp` on a
/// device with `caps`. The published rates only differ from `dsp` with the
/// engine bypass resampler while DSP is on: Direct then runs at rates the
/// DSP sink cannot play (the rate arbiter keeps the DSP sink's clients on
/// `dsp`, [`crate::rate_arbiter::RateInputs::dsp_policy`]).
pub fn rate_policies(
    dsp: RatePolicy,
    caps: Option<&DeviceCapabilities>,
    formats: &[SampleFormat],
    desired: DesiredDspConfig,
) -> RatePolicies {
    let playable = playable_rates(&dsp, caps, formats, desired);
    RatePolicies {
        playable,
        ..sink_policies(dsp, caps, formats, desired)
    }
}

/// Every engine rate the device plays with `desired`, whether
/// `rate.allowed_rates` has it or not: the rates the driver would publish
/// ([`RatePolicies::published`]) if every engine rate were allowed, the
/// engine bypass resampler's rates included. `base` gives the other policy
/// fields. Empty without a probe or when the device plays none.
pub fn playable_rates(
    base: &RatePolicy,
    caps: Option<&DeviceCapabilities>,
    formats: &[SampleFormat],
    desired: DesiredDspConfig,
) -> Vec<u32> {
    use audivane_protocol::ALLOWED_RATES;
    let Some(caps) = caps else {
        return Vec::new();
    };
    let all = RatePolicy {
        allowed_rates: ALLOWED_RATES.to_vec(),
        ..base.clone()
    };
    match effective_rate_policy(&all, caps, desired, formats, ALLOWED_RATES) {
        Ok(eff) => {
            sink_policies(eff.policy, Some(caps), formats, desired)
                .published
                .allowed_rates
        }
        Err(_) => Vec::new(),
    }
}

/// [`rate_policies`] without [`RatePolicies::playable`].
fn sink_policies(
    dsp: RatePolicy,
    caps: Option<&DeviceCapabilities>,
    formats: &[SampleFormat],
    desired: DesiredDspConfig,
) -> RatePolicies {
    let dsp_native = match caps {
        Some(caps) => {
            let native = desired.native_only();
            let base: Vec<u32> = dsp
                .allowed_rates
                .iter()
                .copied()
                .filter(|&r| graph_rate_playable(caps, r, native, formats))
                .collect();
            if base.is_empty() {
                dsp.clone()
            } else {
                RatePolicy {
                    allowed_rates: base,
                    ..dsp.clone()
                }
            }
        }
        None => dsp.clone(),
    };
    // Direct's native base is the native part of `dsp` (a converted rate
    // is never native); it converts the configured and effective rates.
    let (direct, direct_native) = split_direct_policy(&dsp, caps, formats, desired);
    let mut allowed = dsp.allowed_rates.clone();
    allowed.extend(
        direct
            .allowed_rates
            .iter()
            .filter(|r| !dsp.allowed_rates.contains(r)),
    );
    allowed.sort_unstable();
    RatePolicies {
        published: RatePolicy {
            allowed_rates: allowed,
            ..dsp.clone()
        },
        dsp,
        direct,
        dsp_native,
        direct_native,
        playable: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::RatePairMask;
    use audivane_protocol::ipc::{FormatCapability, RateCapability};
    use audivane_protocol::{ALLOWED_RATES, RateTarget};

    fn caps(rows: &[(u32, &[SampleFormat])]) -> DeviceCapabilities {
        DeviceCapabilities {
            device: "hw:Test".into(),
            channels: 2,
            channels_min: 2,
            channels_max: 2,
            rates: rows
                .iter()
                .map(|&(rate, fmts)| RateCapability {
                    rate,
                    formats: fmts
                        .iter()
                        .map(|&format| FormatCapability {
                            format,
                            sbits: Some(format.bits()),
                        })
                        .collect(),
                })
                .collect(),
            rate_min: rows.iter().map(|r| r.0).min().unwrap_or(0),
            rate_max: rows.iter().map(|r| r.0).max().unwrap_or(0),
            formats: FORMAT_PREFERENCE
                .into_iter()
                .rev()
                .filter(|f| rows.iter().any(|r| r.1.contains(f)))
                .collect(),
            modes: Vec::new(),
        }
    }

    fn dsp(target: RateTarget) -> DesiredDspConfig {
        DesiredDspConfig {
            dsp_enabled: true,
            rate_target: target,
            max_ratio: 16,
            generation: 0,
            buildable: RatePairMask::ALL,
            bypass: crate::lifecycle::BypassLeg::OFF,
        }
    }

    const ALL: [SampleFormat; 4] = FORMAT_PREFERENCE;

    #[test]
    fn empty_intersection_widens_then_errors() {
        let c = caps(&[(384_000, &[SampleFormat::S32])]);
        let configured = RatePolicy {
            allowed_rates: vec![44_100, 48_000],
            default_rate: 48_000,
            ..RatePolicy::default()
        };
        let eff = effective_rate_policy(&configured, &c, dsp(RateTarget::Off), &ALL, ALLOWED_RATES)
            .unwrap();
        assert!(eff.widened);
        assert_eq!(eff.policy.allowed_rates, vec![384_000]);
        assert_eq!(eff.policy.default_rate, 384_000);

        let none = caps(&[(22_050, &[SampleFormat::S16])]);
        let err = effective_rate_policy(
            &configured,
            &none,
            dsp(RateTarget::Off),
            &ALL,
            ALLOWED_RATES,
        )
        .unwrap_err();
        assert!(err.contains("rates 22050 Hz"), "{err}");
    }

    /// 96 kHz-only device with DSP x2: the graph may run at 48k (DSP
    /// upsamples), but Direct only at 96k.
    #[test]
    fn direct_policy_keeps_native_rates_only() {
        let c = caps(&[(96_000, &[SampleFormat::S24_3])]);
        let configured = RatePolicy {
            default_rate: 48_000,
            ..RatePolicy::default()
        };
        let eff = effective_rate_policy(
            &configured,
            &c,
            dsp(RateTarget::Ratio(2)),
            &ALL,
            ALLOWED_RATES,
        )
        .unwrap();
        assert_eq!(eff.policy.allowed_rates, vec![48_000, 96_000]);
        assert_eq!(eff.policy.default_rate, 48_000);
        let direct = direct_rate_policy(&eff.policy, Some(&c), &ALL, DesiredDspConfig::default());
        assert_eq!(direct.allowed_rates, vec![96_000]);
        assert_eq!(direct.default_rate, 96_000);
        // A 48k Direct client maps onto the native 96k graph.
        assert_eq!(
            crate::rate_arbiter::choose_allowed_rate(48_000, &direct),
            96_000
        );
        // No native rate among the effective ones: Direct cannot play.
        let only48 = RatePolicy {
            allowed_rates: vec![48_000],
            default_rate: 48_000,
            ..RatePolicy::default()
        };
        assert!(
            direct_rate_policy(&only48, Some(&c), &ALL, DesiredDspConfig::default())
                .allowed_rates
                .is_empty()
        );
        // Without a probe, Direct takes the effective policy.
        assert_eq!(
            direct_rate_policy(&only48, None, &ALL, DesiredDspConfig::default()),
            only48
        );
    }

    #[test]
    fn strict_format_list_limits_rates() {
        let c = caps(&[
            (48_000, &[SampleFormat::S16, SampleFormat::S32]),
            (96_000, &[SampleFormat::S16]),
        ]);
        let eff = effective_rate_policy(
            &RatePolicy::default(),
            &c,
            dsp(RateTarget::Off),
            &[SampleFormat::S32],
            ALLOWED_RATES,
        )
        .unwrap();
        assert_eq!(eff.policy.allowed_rates, vec![48_000]);
    }
}
