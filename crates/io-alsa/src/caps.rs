//! Device capability probing and the pure decisions built on it.
//!
//! The probe runs on an opened but not yet configured PCM handle (the one the
//! engine is about to configure), so it works while the engine holds the
//! device and never needs a second handle. It only uses `HwParams::any` plus
//! refine/test calls on a private parameter set: nothing is installed with
//! `snd_pcm_hw_params`, so the device is left unconfigured.

use crate::error::{AlsaError, Result};
use alsa::PCM;
use alsa::pcm::{Access, Format, HwParams};
use audivane_protocol::SampleFormat;
use audivane_protocol::ipc::{DeviceCapabilities, FormatCapability, RateCapability};

/// Formats the engine can pack, in the order the probe reports them.
pub const PROBE_FORMATS: [SampleFormat; 4] = [
    SampleFormat::S16,
    SampleFormat::S24,
    SampleFormat::S24_3,
    SampleFormat::S32,
];

/// Ratios tried when the device rejects `source_rate * desired`: halve powers
/// of two, otherwise step down by one, ending at 1.
pub fn ratio_ladder(desired: u32) -> Vec<u32> {
    let mut ratios = Vec::new();
    let mut r = desired.max(1);
    loop {
        ratios.push(r);
        if r == 1 {
            break;
        }
        if r.is_power_of_two() {
            r /= 2;
        } else {
            r -= 1;
        }
    }
    ratios
}

/// Every DAC rate the engine could ask for: each graph rate times every
/// integer ratio up to `max_ratio`, capped at `max_rate`. Sorted, unique.
pub fn probe_rates(graph_rates: &[u32], max_ratio: u32, max_rate: u32) -> Vec<u32> {
    let mut out: Vec<u32> = graph_rates
        .iter()
        .flat_map(|&r| (1..=max_ratio.max(1)).map(move |k| r.saturating_mul(k)))
        .filter(|&r| r <= max_rate)
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// Pick the DAC ratio and format for `source_rate`: the first ratio on
/// [`ratio_ladder`] whose rate `source_rate * ratio` the device supports with
/// one of `formats` (in preference order). Never a "nearest" rate: the graph
/// rate times the ratio must be the device rate exactly, or the pitch is off.
pub fn plan_open(
    caps: &DeviceCapabilities,
    source_rate: u32,
    desired_ratio: u32,
    formats: &[SampleFormat],
) -> Result<(u32, SampleFormat)> {
    let ladder = ratio_ladder(desired_ratio);
    for &ratio in &ladder {
        let rate = source_rate.saturating_mul(ratio);
        if let Some(&f) = formats.iter().find(|&&f| caps.supports(rate, f)) {
            return Ok((ratio, f));
        }
    }
    let tried: Vec<String> = ladder
        .iter()
        .map(|&k| format!("{} Hz", source_rate.saturating_mul(k)))
        .collect();
    let rejected = if ladder.len() == 1 {
        format!("rate {source_rate} Hz")
    } else {
        format!(
            "graph rate {source_rate} Hz at any upsampling ratio from x{desired_ratio} down to x1 ({})",
            tried.join(", ")
        )
    };
    Err(AlsaError::Unsupported {
        device: caps.device.clone(),
        rejected,
        supported: caps.summary(),
    })
}

/// Probe `pcm` (opened, not configured) for every rate in `rates` with
/// `channels` channels and RW-interleaved access, per format.
pub fn probe_pcm(
    pcm: &PCM,
    device: &str,
    channels: u32,
    rates: &[u32],
) -> Result<DeviceCapabilities> {
    let base = HwParams::any(pcm)?;
    let _ = base.set_rate_resample(false);
    // Unfiltered support, before the channel count and rate list narrow it:
    // error messages say what the device does take.
    let channels_min = base.get_channels_min().unwrap_or(0);
    let channels_max = base.get_channels_max().unwrap_or(0);
    let (rate_min, rate_max) = rate_bounds(&base);
    let native_formats: Vec<SampleFormat> = PROBE_FORMATS
        .into_iter()
        .filter(|f| base.test_format(to_alsa_format(*f)).is_ok())
        .collect();
    if base.set_access(Access::RWInterleaved).is_err() {
        return Err(AlsaError::Unsupported {
            device: device.to_string(),
            rejected: "interleaved read/write access".into(),
            supported: "only mmap or non-interleaved access".into(),
        });
    }

    let mut per_rate: Vec<RateCapability> = rates
        .iter()
        .map(|&rate| RateCapability {
            rate,
            formats: Vec::new(),
        })
        .collect();

    for fmt in PROBE_FORMATS {
        let afmt = to_alsa_format(fmt);
        if base.test_format(afmt).is_err() {
            continue;
        }
        let with_fmt = base.clone();
        if with_fmt.set_format(afmt).is_err() || with_fmt.set_channels(channels).is_err() {
            continue;
        }
        for cap in &mut per_rate {
            if with_fmt.test_rate(cap.rate).is_err() {
                continue;
            }
            let exact = with_fmt.clone();
            if exact.set_rate(cap.rate, alsa::ValueOr::Nearest).is_err()
                || exact.get_rate().ok() != Some(cap.rate)
            {
                continue;
            }
            cap.formats.push(FormatCapability {
                format: fmt,
                sbits: hw_params_sbits(pcm, &exact),
            });
        }
    }
    per_rate.retain(|r| !r.formats.is_empty());
    if let Some(bits) = crate::reserve::card_index_from_device(device)
        .ok()
        .and_then(hda_codec_bits)
    {
        cap_sbits(&mut per_rate, bits);
    }

    Ok(DeviceCapabilities {
        device: device.to_string(),
        channels,
        channels_min,
        channels_max,
        rates: per_rate,
        rate_min,
        rate_max,
        formats: native_formats,
    })
}

/// Bits the converter of an HD-Audio card resolves, from the codecs' own
/// report (`/proc/asound/cardN/codec#M`, "Default PCM" bits). Recent kernels
/// report HD-Audio S32 as using all 32 bits (subformat `MSBITS_MAX`) even
/// when the codec lists 16, 20 and 24 only. `None` for other cards.
fn hda_codec_bits(card: u32) -> Option<u32> {
    let dir = std::fs::read_dir(format!("/proc/asound/card{card}")).ok()?;
    dir.filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("codec#"))
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .filter_map(|text| codec_default_pcm_bits(&text))
        .max()
}

/// Highest sample width in the "Default PCM" block of a codec proc file:
/// `bits [0xe]: 16 20 24` gives 24.
fn codec_default_pcm_bits(text: &str) -> Option<u32> {
    let mut lines = text.lines().skip_while(|l| l.trim() != "Default PCM:");
    lines.next()?;
    lines
        .take_while(|l| l.starts_with(char::is_whitespace))
        .find_map(|l| l.trim().strip_prefix("bits ["))
        .and_then(|rest| rest.split_once(':'))
        .and_then(|(_, list)| list.split_whitespace().filter_map(|b| b.parse().ok()).max())
}

/// Cap every format's significant bits at `bits` (what the converter
/// resolves), keeping narrower formats as they are.
fn cap_sbits(per_rate: &mut [RateCapability], bits: u32) {
    for f in per_rate.iter_mut().flat_map(|r| r.formats.iter_mut()) {
        let width = f.sbits.unwrap_or_else(|| f.format.bits());
        f.sbits = Some(width.min(bits));
    }
}

/// Closed rate range of `hwp`. The `alsa` crate drops the open/closed
/// direction ALSA reports, so a fixed 48000 Hz device can read as
/// `48000..48001`: step an end that does not test as supported inwards.
fn rate_bounds(hwp: &HwParams<'_>) -> (u32, u32) {
    let mut min = hwp.get_rate_min().unwrap_or(0);
    let mut max = hwp.get_rate_max().unwrap_or(0);
    if max > min && hwp.test_rate(max).is_err() {
        max -= 1;
    }
    if max > min && hwp.test_rate(min).is_err() {
        min += 1;
    }
    (min, max)
}

/// What the device accepts overall, for an error message when no probe
/// result is at hand: rate range, formats, channel range.
pub(crate) fn describe_pcm(pcm: &PCM) -> String {
    let Ok(hwp) = HwParams::any(pcm) else {
        return "unknown (hw_params query failed)".into();
    };
    let _ = hwp.set_rate_resample(false);
    let (rmin, rmax) = rate_bounds(&hwp);
    let cmin = hwp.get_channels_min().unwrap_or(0);
    let cmax = hwp.get_channels_max().unwrap_or(0);
    let formats: Vec<&str> = PROBE_FORMATS
        .iter()
        .filter(|f| hwp.test_format(to_alsa_format(**f)).is_ok())
        .map(|f| f.name())
        .collect();
    let rates = if rmin == rmax {
        format!("rate {rmin} Hz")
    } else {
        format!("rates {rmin}-{rmax} Hz")
    };
    let formats = if formats.is_empty() {
        "none of S16/S24/S24_3/S32".to_string()
    } else {
        formats.join(", ")
    };
    let channels = if cmin == cmax {
        format!("{cmin}")
    } else {
        format!("{cmin}-{cmax}")
    };
    format!("{rates}; formats {formats}; channels {channels}")
}

pub(crate) fn to_alsa_format(fmt: SampleFormat) -> Format {
    match fmt {
        SampleFormat::S16 => Format::S16LE,
        SampleFormat::S24 => Format::S24LE,
        SampleFormat::S24_3 => Format::S243LE,
        SampleFormat::S32 => Format::S32LE,
    }
}

/// `snd_pcm_hw_params_get_sbits` for a parameter set with a single format.
///
/// The `alsa` crate does not wrap it and keeps the raw pointer private.
/// `HwParams` is two pointer-sized words: the `snd_pcm_hw_params_t *` and the
/// `&PCM` it was made from. We know the second (it is `pcm`'s address), so
/// the other word is the parameter pointer, whatever the field order. If the
/// layout ever stops matching, we report no sbits instead of guessing.
fn hw_params_sbits(pcm: &PCM, hwp: &HwParams<'_>) -> Option<u32> {
    const WORDS: usize = 2;
    if std::mem::size_of::<HwParams<'_>>() != WORDS * std::mem::size_of::<usize>() {
        return None;
    }
    // Safety: copies the bytes of a live value out as plain integers.
    let words: [usize; WORDS] = unsafe { std::mem::transmute_copy(hwp) };
    let pcm_addr = pcm as *const PCM as usize;
    let params = match (words[0] == pcm_addr, words[1] == pcm_addr) {
        (true, false) => words[1],
        (false, true) => words[0],
        _ => return None,
    };
    if params == 0 {
        return None;
    }
    // Safety: `params` is the live `snd_pcm_hw_params_t` owned by `hwp`;
    // get_sbits only reads it.
    let v = unsafe {
        alsa_sys::snd_pcm_hw_params_get_sbits(params as *const alsa_sys::snd_pcm_hw_params_t)
    };
    (v > 0).then_some(v as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hda_codec_bits_come_from_the_default_pcm_block() {
        let codec = "Codec: Realtek ALC245\nAddress: 0\nDefault PCM:\n    rates [0x560]: 44100 48000 96000 192000\n    bits [0xe]: 16 20 24\n    formats [0x1]: PCM\nDefault Amp-In caps: N/A\nNode 0x02 [Audio Output] wcaps 0x41d: Stereo Amp-Out\n  PCM:\n    rates [0x40]: 48000\n    bits [0x1e]: 16 20 24 32\n";
        assert_eq!(codec_default_pcm_bits(codec), Some(24));
        assert_eq!(
            codec_default_pcm_bits("Codec: HDMI\nDefault PCM:\n    N/A\n"),
            None
        );
        assert_eq!(codec_default_pcm_bits(""), None);

        let mut rates = vec![RateCapability {
            rate: 48000,
            formats: vec![
                FormatCapability {
                    format: SampleFormat::S16,
                    sbits: Some(16),
                },
                FormatCapability {
                    format: SampleFormat::S32,
                    sbits: Some(32),
                },
            ],
        }];
        cap_sbits(&mut rates, 24);
        let sbits: Vec<_> = rates[0].formats.iter().map(|f| f.sbits).collect();
        assert_eq!(sbits, [Some(16), Some(24)]);
    }

    /// The real device behind the bug report: Creative Pebble V3, USB full
    /// speed, one playback altset: S16_LE, 2 channels, 48000 Hz only.
    pub(crate) fn pebble_v3() -> DeviceCapabilities {
        DeviceCapabilities {
            device: "hw:CARD=V3,DEV=0".into(),
            channels: 2,
            channels_min: 2,
            channels_max: 2,
            rate_min: 48_000,
            rate_max: 48_000,
            formats: vec![SampleFormat::S16],
            rates: vec![RateCapability {
                rate: 48_000,
                formats: vec![FormatCapability {
                    format: SampleFormat::S16,
                    sbits: Some(16),
                }],
            }],
        }
    }

    const PREF: [SampleFormat; 4] = [
        SampleFormat::S32,
        SampleFormat::S24,
        SampleFormat::S24_3,
        SampleFormat::S16,
    ];

    #[test]
    fn ladder_halves_powers_of_two_and_steps_others() {
        assert_eq!(ratio_ladder(1), vec![1]);
        assert_eq!(ratio_ladder(8), vec![8, 4, 2, 1]);
        assert_eq!(ratio_ladder(6), vec![6, 5, 4, 2, 1]);
        assert_eq!(ratio_ladder(0), vec![1]);
    }

    #[test]
    fn probe_rates_cover_ratios_and_cap() {
        let r = probe_rates(&[44_100, 48_000], 4, 192_000);
        assert_eq!(
            r,
            vec![
                44_100, 48_000, 88_200, 96_000, 132_300, 144_000, 176_400, 192_000
            ]
        );
        let all = probe_rates(audivane_protocol::ALLOWED_RATES, 16, 768_000);
        assert!(all.contains(&768_000) && all.contains(&705_600));
        assert!(all.iter().all(|&r| r <= 768_000));
    }

    #[test]
    fn pebble_v3_plays_48k_s16_at_ratio_one() {
        let caps = pebble_v3();
        // S32 preferred, upsampling x4 wanted: falls back to x1 and S16.
        assert_eq!(
            plan_open(&caps, 48_000, 4, &PREF).unwrap(),
            (1, SampleFormat::S16)
        );
        // 44.1k can never be opened: no nearest-rate fallback.
        let err = plan_open(&caps, 44_100, 1, &PREF).unwrap_err().to_string();
        assert!(err.contains("rate 44100 Hz"), "{err}");
        assert!(
            err.contains("rates 48000 Hz; formats S16; channels 2"),
            "{err}"
        );
        let err = plan_open(&caps, 44_100, 4, &PREF).unwrap_err().to_string();
        assert!(err.contains("176400 Hz, 88200 Hz, 44100 Hz"), "{err}");
    }

    #[test]
    fn plan_open_respects_rate_dependent_formats() {
        // Typical USB: 24-bit only up to 96k, 16-bit up to 192k.
        let mut rates = Vec::new();
        for rate in [44_100, 48_000, 88_200, 96_000, 176_400, 192_000] {
            let mut formats = vec![FormatCapability {
                format: SampleFormat::S16,
                sbits: Some(16),
            }];
            if rate <= 96_000 {
                formats.push(FormatCapability {
                    format: SampleFormat::S32,
                    sbits: Some(24),
                });
            }
            rates.push(RateCapability { rate, formats });
        }
        let caps = DeviceCapabilities {
            device: "hw:X".into(),
            channels: 2,
            channels_min: 2,
            channels_max: 2,
            rate_min: 44_100,
            rate_max: 192_000,
            formats: vec![SampleFormat::S16, SampleFormat::S32],
            rates,
        };
        assert_eq!(
            plan_open(&caps, 48_000, 4, &PREF).unwrap(),
            (4, SampleFormat::S16)
        );
        assert_eq!(
            plan_open(&caps, 48_000, 2, &PREF).unwrap(),
            (2, SampleFormat::S32)
        );
        // Strict list without S16 skips 192k and drops to x2.
        assert_eq!(
            plan_open(&caps, 48_000, 4, &[SampleFormat::S32]).unwrap(),
            (2, SampleFormat::S32)
        );
    }
}

/// Rig check (needs `snd-aloop` as card "Loopback"): the probe sees the
/// loopback's full matrix, including sbits, and leaves the PCM configurable.
#[cfg(test)]
mod rig_tests {
    use super::*;

    #[test]
    #[ignore = "needs the snd-aloop Loopback card"]
    fn probe_loopback_card() {
        let open = crate::OpenPcm::open("hw:Loopback,0,3", true).expect("open loopback");
        let rates = probe_rates(audivane_protocol::ALLOWED_RATES, 16, 768_000);
        let caps = open.probe(2, &rates).expect("probe");
        eprintln!("{caps:#?}");
        assert!(caps.supports(44_100, SampleFormat::S32));
        assert!(caps.supports(768_000, SampleFormat::S16));
        assert_eq!(caps.formats_at(48_000).len(), 4);
        // Not configured by the probe: a normal open still works afterwards.
        let cfg = crate::AlsaConfig {
            device: "hw:Loopback,0,3".into(),
            rate: 96_000,
            ..Default::default()
        };
        let (out, ratio) = open
            .configure_with_ratio_fallback(&cfg, 48_000, 2, 1024, &[], Some(&caps))
            .expect("configure");
        assert_eq!(ratio, 2);
        assert_eq!(out.params().rate, 96_000);
    }
}
