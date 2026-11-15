//! Which graph rates the engine can actually offer PipeWire, given what the
//! output device accepts.
//!
//! Pure logic over a probed [`DeviceCapabilities`] so it can be tested with
//! fake devices. The driver publishes the result (not the configured
//! `rate.allowed_rates`) to `clock.allowed-rates`, the sinks'
//! `audio.allowed-rates` and their format pods, so PipeWire resamples streams
//! at rates the device cannot play instead of switching the graph to them.

use crate::lifecycle::DesiredDspConfig;
use audivane_io_alsa::{FORMAT_PREFERENCE, ratio_ladder};
use audivane_protocol::SampleFormat;
use audivane_protocol::ipc::{DeviceCapabilities, RatePolicy, SinkChoice};

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
/// upsampling ratio down to 1, with one of `formats`?
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
    let desired_ratio = desired.resolve_ratio_for(SinkChoice::Dsp, rate);
    ratio_ladder(desired_ratio).into_iter().any(|k| {
        let out = rate.saturating_mul(k);
        formats.iter().any(|&f| caps.supports(out, f))
    })
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
/// Allowed = configured ∩ playable. If that is empty, every playable rate in
/// `engine_rates` (the engine's full list); if still empty, an error that
/// says what the device supports. The default rate is kept when playable,
/// else the [`nearest_rate`].
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
    let playable = |r: u32| graph_rate_playable(caps, r, desired, formats);
    let (mut allowed, unsupported): (Vec<u32>, Vec<u32>) = configured
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

/// The rate policy of the Direct (bit-perfect) sink: Direct never
/// upsamples, so it only runs at effective rates the device plays natively
/// (x1) with one of `formats`. The default is the effective default when it
/// is native, else the [`nearest_rate`] native one. Without a probe the
/// effective policy is used as is. `allowed_rates` is empty when no effective
/// rate is native: Direct cannot play on this device then.
pub fn direct_rate_policy(
    effective: &RatePolicy,
    caps: Option<&DeviceCapabilities>,
    formats: &[SampleFormat],
) -> RatePolicy {
    let Some(caps) = caps else {
        return effective.clone();
    };
    let formats: &[SampleFormat] = if formats.is_empty() {
        &FORMAT_PREFERENCE
    } else {
        formats
    };
    let allowed: Vec<u32> = effective
        .allowed_rates
        .iter()
        .copied()
        .filter(|&r| formats.iter().any(|&f| caps.supports(r, f)))
        .collect();
    let default_rate = if allowed.contains(&effective.default_rate) {
        effective.default_rate
    } else {
        nearest_rate(effective.default_rate, &allowed).unwrap_or(effective.default_rate)
    };
    RatePolicy {
        allowed_rates: allowed,
        default_rate,
        ..effective.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        }
    }

    /// Creative Pebble V3 (the bug report): S16_LE, 2 ch, 48000 Hz only.
    fn pebble_v3() -> DeviceCapabilities {
        DeviceCapabilities {
            device: "hw:CARD=V3,DEV=0".into(),
            ..caps(&[(48_000, &[SampleFormat::S16])])
        }
    }

    fn dsp(target: RateTarget) -> DesiredDspConfig {
        DesiredDspConfig {
            dsp_enabled: true,
            rate_target: target,
            max_ratio: 16,
            generation: 0,
        }
    }

    const ALL: [SampleFormat; 4] = FORMAT_PREFERENCE;

    #[test]
    fn pebble_v3_offers_48k_only() {
        let eff = effective_rate_policy(
            &RatePolicy::default(),
            &pebble_v3(),
            dsp(RateTarget::Off),
            &ALL,
            ALLOWED_RATES,
        )
        .unwrap();
        assert_eq!(eff.policy.allowed_rates, vec![48_000]);
        assert_eq!(eff.policy.default_rate, 48_000);
        assert_eq!(
            eff.unsupported,
            vec![44_100, 88_200, 96_000, 176_400, 192_000]
        );
        assert!(!eff.widened);
        // A 44.1k app maps onto the 48k graph (PipeWire resamples it).
        assert_eq!(
            crate::rate_arbiter::choose_allowed_rate(44_100, &eff.policy),
            48_000
        );
    }

    #[test]
    fn pebble_v3_with_upsampling_still_48k_and_ratio_one() {
        for target in [
            RateTarget::Ratio(2),
            RateTarget::Ratio(4),
            RateTarget::MaxRate(192_000),
        ] {
            let eff = effective_rate_policy(
                &RatePolicy::default(),
                &pebble_v3(),
                dsp(target),
                &ALL,
                ALLOWED_RATES,
            )
            .unwrap();
            assert_eq!(eff.policy.allowed_rates, vec![48_000], "{target:?}");
            let ratio = dsp(target).resolve_ratio_for(SinkChoice::Dsp, 48_000);
            let (k, f) = audivane_io_alsa::plan_open(&pebble_v3(), 48_000, ratio, &ALL).unwrap();
            assert_eq!((k, f), (1, SampleFormat::S16), "{target:?}");
        }
    }

    #[test]
    fn upsampled_only_rates_count_as_playable() {
        // A device that only runs at 96k/192k still takes a 48k graph x2/x4.
        let c = caps(&[
            (96_000, &[SampleFormat::S32]),
            (192_000, &[SampleFormat::S32]),
        ]);
        let eff = effective_rate_policy(
            &RatePolicy::default(),
            &c,
            dsp(RateTarget::Ratio(4)),
            &ALL,
            ALLOWED_RATES,
        )
        .unwrap();
        assert_eq!(eff.policy.allowed_rates, vec![48_000, 96_000, 192_000]);
        // Without upsampling only the native rates remain.
        let eff = effective_rate_policy(
            &RatePolicy::default(),
            &c,
            dsp(RateTarget::Off),
            &ALL,
            ALLOWED_RATES,
        )
        .unwrap();
        assert_eq!(eff.policy.allowed_rates, vec![96_000, 192_000]);
        assert_eq!(eff.policy.default_rate, 96_000);
    }

    #[test]
    fn default_rate_prefers_same_family_then_48k() {
        assert_eq!(nearest_rate(44_100, &[48_000, 88_200]), Some(88_200));
        assert_eq!(nearest_rate(44_100, &[48_000, 96_000]), Some(48_000));
        assert_eq!(nearest_rate(192_000, &[48_000, 96_000]), Some(96_000));
        assert_eq!(nearest_rate(44_100, &[32_000, 48_000]), Some(48_000));
    }

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

    /// Mono-only device (tester's `hw:Loopback,0,2,48000,S16_LE,1`): the
    /// stereo probe finds nothing, and the error blames the channel count
    /// and keeps the rate and format the device does take.
    #[test]
    fn mono_device_reports_channel_error() {
        let c = DeviceCapabilities {
            device: "hw:Mono".into(),
            channels: 2,
            channels_min: 1,
            channels_max: 1,
            rates: vec![],
            rate_min: 48_000,
            rate_max: 48_000,
            formats: vec![SampleFormat::S16],
        };
        let err = effective_rate_policy(
            &RatePolicy::default(),
            &c,
            dsp(RateTarget::Off),
            &ALL,
            ALLOWED_RATES,
        )
        .unwrap_err();
        assert!(err.contains("needs 2 channels; device supports 1"), "{err}");
        assert!(
            err.contains("channels 2") && err.contains("channels 1"),
            "{err}"
        );
        assert!(err.contains("rate 48000 Hz; formats S16"), "{err}");
        assert!(!err.contains("sample rates"), "{err}");
    }

    /// 32 kHz-only device: its rate is outside the engine's list.
    #[test]
    fn rate_outside_engine_list_is_named() {
        let c = DeviceCapabilities {
            device: "hw:32k".into(),
            channels: 2,
            channels_min: 2,
            channels_max: 2,
            rates: vec![],
            rate_min: 32_000,
            rate_max: 32_000,
            formats: vec![SampleFormat::S16],
        };
        let err = effective_rate_policy(
            &RatePolicy::default(),
            &c,
            dsp(RateTarget::Off),
            &ALL,
            ALLOWED_RATES,
        )
        .unwrap_err();
        assert!(
            err.contains("supports 32000 Hz only, which the engine doesn't run at"),
            "{err}"
        );
        let tail = err.split("(device supports ").last().unwrap();
        assert!(tail.contains("32000") && tail.contains("S16"), "{err}");
        // No engine format at all.
        let float_only = DeviceCapabilities {
            formats: vec![],
            ..c
        };
        let err = float_only.unusable_reason().unwrap();
        assert!(err.contains("none of the engine's sample formats"), "{err}");
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
        let direct = direct_rate_policy(&eff.policy, Some(&c), &ALL);
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
            direct_rate_policy(&only48, Some(&c), &ALL)
                .allowed_rates
                .is_empty()
        );
        // Without a probe, Direct takes the effective policy.
        assert_eq!(direct_rate_policy(&only48, None, &ALL), only48);
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
