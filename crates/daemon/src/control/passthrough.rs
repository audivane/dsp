//! "Is what reaches the DAC bit-identical to the source?"
//!
//! This module is pure: it turns an engine snapshot plus the PipeWire facts
//! about the streams feeding the active sink into a `Passthrough`. Anything
//! that changes the samples on the way to the DAC becomes one
//! [`PassthroughBreak`]; an empty list means the path is intact.

use crate::api::EngineSnapshot;
use audivane_protocol::ipc::{
    BypassResampler, BypassResamplerStatus, EngineConfig, EngineState, Passthrough,
    PassthroughBreak, PassthroughCaveat, SinkChoice,
};

/// What the graph watcher knows about one playing stream.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StreamView {
    /// PipeWire node id (used in `Volume` breaks).
    pub node: u32,
    /// Native rate in Hz, when negotiated.
    pub rate: Option<u32>,
    /// Negotiated channel count.
    pub channels: Option<u32>,
    /// Negotiated PipeWire format, e.g. `S32LE` or `F32LE`.
    pub format: Option<String>,
    /// Linear software volume, 1.0 = unity.
    pub volume: Option<f64>,
    pub muted: bool,
}

/// What the graph watcher knows about the sink feeding the DAC.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SinkView {
    pub node: u32,
    pub volume: Option<f64>,
    pub muted: bool,
    pub channels: Option<u32>,
}

/// Significant bits a PipeWire format carries.
///
/// Floats are reported as 32 bits: an `f32` bus holds 24-bit integers exactly,
/// but a float source itself can carry more precision than that, so it is not
/// a bit-perfect carrier in the sense used here.
pub fn format_bits(format: &str) -> Option<u32> {
    let name = format.trim().to_ascii_uppercase();
    let name = name
        .strip_suffix("LE")
        .or_else(|| name.strip_suffix("BE"))
        .unwrap_or(&name);
    let mut chars = name.chars();
    let kind = chars.next()?;
    let digits: String = chars.take_while(char::is_ascii_digit).collect();
    let bits: u32 = digits.parse().ok()?;
    match kind {
        // `F32`, `F64`: floating point, not bit-exact for >24-bit sources.
        'F' => Some(bits),
        // `S16`, `S24`, `S24_3`, `S24_32`, `S32`, `U8`: the first number is the
        // significant bit depth (the rest describes the container).
        'S' | 'U' => Some(bits),
        _ => None,
    }
}

/// True when a PipeWire format is an integer format of at most 24 bits, i.e.
/// exactly representable in the f32 graph bus.
pub fn is_int_le_24(format: &str) -> bool {
    format.trim().to_ascii_uppercase().starts_with('S')
        && format_bits(format).is_some_and(|bits| bits <= 24)
}

/// True when the format name is an integer sample format (`S16LE`, `U8`, ...).
fn is_int_format(format: &str) -> bool {
    matches!(
        format.trim().to_ascii_uppercase().chars().next(),
        Some('S' | 'U')
    )
}

/// The source precision we can actually prove for one stream, if any.
///
/// A negotiated *container* is not the source precision: `pw-play` opens a
/// 24-bit WAV as `S32LE`, and a float bus says nothing about the file behind it.
/// Only two things count as knowledge:
///
/// * `hint`: the player's own metadata (MPRIS `native_bits`), passed in only
///   when one stream fills the sink and is therefore the file that player plays;
/// * an integer format of at most 24 bits, whose first number *is* the
///   significant depth (`S16LE`, `S24_3LE`, ...).
fn known_source_bits(stream: &StreamView, hint: Option<u32>) -> Option<u32> {
    if let Some(bits) = hint.filter(|bits| *bits > 0) {
        return Some(bits);
    }
    let format = stream.format.as_deref()?;
    let bits = format_bits(format)?;
    (is_int_format(format) && bits <= 24).then_some(bits)
}

/// Compute the passthrough status.
///
/// `streams` must be the streams currently feeding the active sink (not every
/// stream in the graph). `sink` is that sink's node when known. `source_bits`
/// is the playing file's native bit depth from the player (MPRIS), used only
/// when it can be attributed to the single stream feeding the sink.
pub fn compute(
    snap: &EngineSnapshot,
    streams: &[StreamView],
    sink: Option<&SinkView>,
    source_bits: Option<u32>,
) -> Passthrough {
    let mut broken = Vec::new();
    let mut caveats = Vec::new();

    if streams.is_empty() {
        broken.push(PassthroughBreak::NotPlaying);
    } else if snap.format.is_none() || snap.state != EngineState::Playing {
        // Streams are linked, but the output is not writing them (no
        // format negotiated, a reopen in flight, an error): nothing reaches
        // the DAC, so nothing is bit-perfect.
        broken.push(PassthroughBreak::NoOutput);
    }
    let varispeed_active = snap.varispeed.as_ref().filter(|v| v.active);
    if snap.dsp_active || varispeed_active.is_some() {
        broken.push(PassthroughBreak::DspActive);
    }
    match snap.ratio.as_integer() {
        Some(1) => {}
        Some(ratio) => broken.push(PassthroughBreak::Upsampling { ratio }),
        None => broken.push(PassthroughBreak::RateConversion {
            graph_rate: snap.graph_rate,
            output_rate: snap.output_rate,
        }),
    }
    if let Some(vs) = varispeed_active {
        broken.push(PassthroughBreak::Varispeed { speed: vs.speed });
    }

    if !streams.is_empty() {
        if let Some(sink) = sink
            && (sink.muted || sink.volume.is_some_and(|v| v != 1.0))
        {
            broken.push(PassthroughBreak::Volume {
                node: sink.node,
                volume: effective_volume(sink.muted, sink.volume),
            });
        }
        let mut ordered: Vec<&StreamView> = streams.iter().collect();
        ordered.sort_by_key(|s| s.node);
        for stream in &ordered {
            if stream.muted || stream.volume.is_some_and(|v| v != 1.0) {
                broken.push(PassthroughBreak::Volume {
                    node: stream.node,
                    volume: effective_volume(stream.muted, stream.volume),
                });
            }
        }

        if streams.len() > 1 {
            broken.push(PassthroughBreak::Mixing {
                streams: u32::try_from(streams.len()).unwrap_or(u32::MAX),
            });
        }

        // A player's native bit depth is only the *source* precision when it
        // is clear which stream it belongs to.
        let hint = if ordered.len() == 1 {
            source_bits
        } else {
            None
        };

        let mut known: Vec<(&StreamView, Option<u32>)> = Vec::with_capacity(ordered.len());
        for stream in &ordered {
            let bits = known_source_bits(stream, hint);
            if bits.is_none()
                && let Some(format) = stream.format.as_deref()
                && !caveats.iter().any(|caveat| {
                    matches!(
                        caveat,
                        PassthroughCaveat::UnknownSourcePrecision { format: seen }
                            if seen == format
                    )
                })
            {
                caveats.push(PassthroughCaveat::UnknownSourcePrecision {
                    format: format.to_string(),
                });
            }
            known.push((stream, bits));
        }

        for (stream, source_bits) in &known {
            if let (Some(rate), true) = (stream.rate, snap.graph_rate > 0)
                && rate != snap.graph_rate
            {
                broken.push(PassthroughBreak::Resampled {
                    stream_rate: rate,
                    graph_rate: snap.graph_rate,
                });
            }
            if let (Some(stream_channels), Some(sink_channels)) =
                (stream.channels, sink.and_then(|s| s.channels))
                && stream_channels != sink_channels
            {
                broken.push(PassthroughBreak::ChannelMix {
                    stream_channels,
                    sink_channels,
                });
            }
            if source_bits.is_some_and(|bits| bits > 24) {
                broken.push(PassthroughBreak::SourceOver24Bit);
            }
        }

        // The DAC only receives `output_bits` significant bits; compare with
        // the widest *known* source so a 24-bit stream into S16 is reported
        // while an unlabelled S32LE container is not.
        let output_bits = snap.format.map(|f| f.bits());
        let widest_source = known.iter().filter_map(|(_, bits)| *bits).max();
        if let (Some(source_bits), Some(output_bits)) = (widest_source, output_bits)
            && output_bits < source_bits
        {
            broken.push(PassthroughBreak::Truncated {
                source_bits,
                output_bits,
            });
        }
    }

    Passthrough {
        intact: broken.is_empty(),
        broken_because: broken,
        caveats,
    }
}

/// The engine bypass resampler's status (`engine`, from the engine
/// snapshot) with the reason filled in when it is on but PipeWire still
/// resamples a stream feeding Direct or DSP off (passthrough reports it as
/// `resampled`): rate following off, a rate outside `rate.allowed_rates`,
/// another stream deciding the graph rate, or a rate the engine cannot
/// convert to one the DAC plays (`landing`: where the engine would take a
/// stream rate, [`audivane_io_pw::DesiredDspConfig::bypass_landing`]).
pub fn bypass_resampler_status(
    engine: Option<BypassResamplerStatus>,
    cfg: &EngineConfig,
    snap: &EngineSnapshot,
    streams: &[StreamView],
    active_sink: Option<SinkChoice>,
    landing: impl Fn(u32) -> Option<u32>,
) -> Option<BypassResamplerStatus> {
    let mut status = engine?;
    let bypass_path = active_sink == Some(SinkChoice::Direct) || !cfg.dsp_enabled;
    if status.mode != BypassResampler::Engine
        || status.active
        || status.reason.is_some()
        || !bypass_path
        || snap.graph_rate == 0
    {
        return Some(status);
    }
    let Some(rate) = streams
        .iter()
        .filter_map(|s| s.rate)
        .find(|&r| r != snap.graph_rate)
    else {
        return Some(status);
    };
    let why = if !cfg.rate.follow_source {
        "rate following is off".to_string()
    } else if !cfg.rate.allowed_rates.contains(&rate) {
        format!("{rate} Hz is not in the allowed rates")
    } else if streams.iter().any(|s| s.rate == Some(snap.graph_rate)) {
        format!("another stream plays at {} Hz", snap.graph_rate)
    } else if landing(rate).is_none() {
        format!("the engine cannot convert {rate} Hz to a rate the DAC plays")
    } else {
        "the graph has not moved yet".to_string()
    };
    status.reason = Some(format!("PipeWire resamples the {rate} Hz stream: {why}"));
    Some(status)
}

/// The volume a break reports: 0 when muted (what is heard, and what
/// clients show as "muted"), else the linear slider volume.
fn effective_volume(muted: bool, volume: Option<f64>) -> f64 {
    if muted { 0.0 } else { volume.unwrap_or(0.0) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use audivane_protocol::SampleFormat;
    use audivane_protocol::ipc::EngineState;

    fn snapshot() -> EngineSnapshot {
        EngineSnapshot {
            state: EngineState::Playing,
            device: "hw:0,0".into(),
            error: None,
            format: Some(SampleFormat::S24),
            graph_rate: 44_100,
            output_rate: 44_100,
            ratio: audivane_protocol::RateRatio::ONE,
            quantum: 1024,
            latency_ms: 5.0,
            filter_latency_ms: 0.0,
            xruns: 0,
            dsp_load: 0.0,
            dsp_active: false,
            active_sink: None,
            ab: None,
            volume: None,
            dsp_node: None,
            direct_node: None,
            frames_out: 0,
            chain_generation: 0,
            gaps: Default::default(),
            device_caps: None,
            effective_rates: Vec::new(),
            direct_rates: Vec::new(),
            effective_default_rate: None,
            dither_bits: None,
            bypass_resampler: None,
            rt_load_max: 0.0,
            varispeed: None,
            waiting_for_device: false,
        }
    }

    fn stream() -> StreamView {
        StreamView {
            node: 10,
            rate: Some(44_100),
            channels: Some(2),
            format: Some("S24LE".into()),
            volume: Some(1.0),
            muted: false,
        }
    }

    fn sink() -> SinkView {
        SinkView {
            node: 5,
            volume: Some(1.0),
            muted: false,
            channels: Some(2),
        }
    }

    #[test]
    fn clean_path_is_intact() {
        let status = compute(&snapshot(), &[stream()], Some(&sink()), None);
        assert!(status.intact, "{:?}", status.broken_because);
        assert!(status.broken_because.is_empty());
    }

    #[test]
    fn not_playing() {
        let status = compute(&snapshot(), &[], Some(&sink()), None);
        assert!(!status.intact);
        assert_eq!(status.broken_because, vec![PassthroughBreak::NotPlaying]);
    }

    #[test]
    fn dsp_and_upsampling() {
        let mut snap = snapshot();
        snap.dsp_active = true;
        snap.ratio = audivane_protocol::RateRatio::integer(4);
        snap.output_rate = 176_400;
        let status = compute(&snap, &[stream()], Some(&sink()), None);
        assert_eq!(
            status.broken_because,
            vec![
                PassthroughBreak::DspActive,
                PassthroughBreak::Upsampling { ratio: 4 }
            ]
        );
    }

    #[test]
    fn rate_conversion_is_reported() {
        let mut snap = snapshot();
        snap.ratio = audivane_protocol::RateRatio::new(160, 147);
        snap.output_rate = 48_000;
        let status = compute(&snap, &[stream()], Some(&sink()), None);
        assert_eq!(
            status.broken_because,
            vec![PassthroughBreak::RateConversion {
                graph_rate: 44_100,
                output_rate: 48_000,
            }]
        );
    }

    #[test]
    fn nothing_open_is_never_intact() {
        // A reopen from x1 failed: the stream is still linked, but no
        // format is negotiated and nothing reaches the DAC.
        let mut snap = snapshot();
        snap.format = None;
        let status = compute(&snap, &[stream()], Some(&sink()), None);
        assert!(!status.intact);
        assert_eq!(status.broken_because, vec![PassthroughBreak::NoOutput]);
        // The format is still known, but the output is not writing: a
        // reopen in flight, waiting for a chain, or an error.
        let mut snap = snapshot();
        for state in [
            EngineState::Switching,
            EngineState::Ready,
            EngineState::Error,
        ] {
            snap.state = state;
            let status = compute(&snap, &[stream()], Some(&sink()), None);
            assert!(!status.intact, "{state:?}");
            assert_eq!(
                status.broken_because,
                vec![PassthroughBreak::NoOutput],
                "{state:?}"
            );
        }
        // Reported before the DSP break, as documented.
        snap.dsp_active = true;
        assert_eq!(
            compute(&snap, &[stream()], Some(&sink()), None).broken_because[..2],
            [PassthroughBreak::NoOutput, PassthroughBreak::DspActive]
        );
        // Nothing playing: that, not NoOutput.
        let status = compute(&snap, &[], Some(&sink()), None);
        assert_eq!(status.broken_because[0], PassthroughBreak::NotPlaying);
        assert!(!status.broken_because.contains(&PassthroughBreak::NoOutput));
    }

    #[test]
    fn sink_volume_and_mute() {
        let mut snap = snapshot();
        snap.dsp_active = false;
        let mut sink = sink();
        sink.volume = Some(0.5);
        let status = compute(&snap, &[stream()], Some(&sink), None);
        assert_eq!(
            status.broken_because,
            vec![PassthroughBreak::Volume {
                node: 5,
                volume: 0.5
            }]
        );

        // Muted at full slider volume: reported as 0 (what is heard).
        sink.volume = Some(1.0);
        sink.muted = true;
        let status = compute(&snap, &[stream()], Some(&sink), None);
        assert_eq!(
            status.broken_because,
            vec![PassthroughBreak::Volume {
                node: 5,
                volume: 0.0
            }]
        );
    }

    #[test]
    fn stream_volume_reported_per_node_in_id_order() {
        let mut a = stream();
        a.node = 30;
        a.volume = Some(0.25);
        let mut b = stream();
        b.node = 20;
        b.volume = Some(0.5);
        let status = compute(&snapshot(), &[a, b], Some(&sink()), None);
        assert_eq!(
            status.broken_because,
            vec![
                PassthroughBreak::Volume {
                    node: 20,
                    volume: 0.5
                },
                PassthroughBreak::Volume {
                    node: 30,
                    volume: 0.25
                },
                PassthroughBreak::Mixing { streams: 2 },
            ]
        );
    }

    #[test]
    fn mixing_and_resampling() {
        let mut a = stream();
        a.rate = Some(48_000);
        let status = compute(&snapshot(), &[stream(), a], Some(&sink()), None);
        assert_eq!(
            status.broken_because,
            vec![
                PassthroughBreak::Mixing { streams: 2 },
                PassthroughBreak::Resampled {
                    stream_rate: 48_000,
                    graph_rate: 44_100
                },
            ]
        );
    }

    #[test]
    fn channel_mix_and_unknown_container_precision() {
        let mut stream = stream();
        stream.format = Some("S32LE".into());
        stream.channels = Some(1);
        let status = compute(&snapshot(), &[stream], Some(&sink()), None);
        // The channel remix is a real break; the 32-bit *container* only
        // means the source precision is unknown.
        assert!(!status.intact);
        assert_eq!(
            status.broken_because,
            vec![PassthroughBreak::ChannelMix {
                stream_channels: 1,
                sink_channels: 2
            }]
        );
        assert_eq!(
            status.caveats,
            vec![PassthroughCaveat::UnknownSourcePrecision {
                format: "S32LE".into()
            }]
        );
    }

    #[test]
    fn float_and_wide_containers_are_caveats_not_breaks() {
        // pw-play opens a 24-bit WAV as S32LE, and a float bus says nothing
        // about the file: neither may claim "more than 24 bits".
        for format in ["S32LE", "F32LE", "F64LE"] {
            let mut stream = stream();
            stream.format = Some(format.into());
            let status = compute(&snapshot(), &[stream], Some(&sink()), None);
            assert!(status.intact, "{format}: {:?}", status.broken_because);
            assert_eq!(
                status.caveats,
                vec![PassthroughCaveat::UnknownSourcePrecision {
                    format: format.into()
                }],
                "{format}"
            );
        }

        // Same container, 16-bit output: an unlabelled container cannot prove
        // the output truncates the source either.
        let mut snap = snapshot();
        snap.format = Some(SampleFormat::S16);
        let mut stream = stream();
        stream.format = Some("S32LE".into());
        let status = compute(&snap, &[stream], Some(&sink()), None);
        assert!(status.intact, "{:?}", status.broken_because);
        assert!(status.broken_because.is_empty());
    }

    #[test]
    fn over_24_bit_source_only_when_the_player_reports_it() {
        // MPRIS says the file is 32-bit: that *is* known, so it is a break.
        let mut snap = snapshot();
        snap.format = Some(SampleFormat::S32);
        let status = compute(&snap, &[stream()], Some(&sink()), Some(32));
        assert_eq!(
            status.broken_because,
            vec![PassthroughBreak::SourceOver24Bit]
        );
        assert!(status.caveats.is_empty());

        // The player reports 24 bits: nothing to report.
        let status = compute(&snap, &[stream()], Some(&sink()), Some(24));
        assert!(status.intact, "{:?}", status.broken_because);
        assert!(status.caveats.is_empty());

        // 32-bit file into a 16-bit DAC: both the graph precision and the
        // output word length are reported.
        snap.format = Some(SampleFormat::S16);
        let status = compute(&snap, &[stream()], Some(&sink()), Some(32));
        assert_eq!(
            status.broken_because,
            vec![
                PassthroughBreak::SourceOver24Bit,
                PassthroughBreak::Truncated {
                    source_bits: 32,
                    output_bits: 16
                },
            ]
        );
    }

    #[test]
    fn player_bits_are_only_used_for_a_single_stream() {
        let mut snap = snapshot();
        snap.format = Some(SampleFormat::S32);
        let mut other = stream();
        other.node = 11;
        let status = compute(&snap, &[stream(), other], Some(&sink()), Some(32));
        // Two streams: the player's metadata cannot be attributed, and a
        // mixed signal is not a bit-perfect path anyway.
        assert_eq!(
            status.broken_because,
            vec![PassthroughBreak::Mixing { streams: 2 }]
        );
        assert!(status.caveats.is_empty());
    }

    #[test]
    fn truncated_output() {
        let mut snap = snapshot();
        snap.format = Some(SampleFormat::S16);
        let status = compute(&snap, &[stream()], Some(&sink()), None);
        assert_eq!(
            status.broken_because,
            vec![PassthroughBreak::Truncated {
                source_bits: 24,
                output_bits: 16
            }]
        );
    }

    #[test]
    fn unknown_pieces_do_not_crash() {
        // Unknown stream details and graph rate (the output is open; with
        // no output at all see `nothing_open_is_never_intact`).
        let mut snap = snapshot();
        snap.graph_rate = 0;
        let mut stream = stream();
        stream.format = None;
        stream.rate = None;
        stream.channels = None;
        let status = compute(&snap, &[stream], None, None);
        assert!(status.intact, "{:?}", status.broken_because);
        assert!(status.caveats.is_empty());
    }

    #[test]
    fn format_bits_parsing() {
        assert_eq!(format_bits("S16LE"), Some(16));
        assert_eq!(format_bits("S24LE"), Some(24));
        assert_eq!(format_bits("S24_3LE"), Some(24));
        assert_eq!(format_bits("S24_32LE"), Some(24));
        assert_eq!(format_bits("S32LE"), Some(32));
        assert_eq!(format_bits("S32BE"), Some(32));
        assert_eq!(format_bits("F32LE"), Some(32));
        assert_eq!(format_bits("F64LE"), Some(64));
        assert_eq!(format_bits("U8"), Some(8));
        assert_eq!(format_bits("garbage"), None);
        assert_eq!(format_bits(""), None);
        assert!(is_int_le_24("S24LE"));
        assert!(!is_int_le_24("S32LE"));
        assert!(!is_int_le_24("F32LE"));
    }

    /// Test 18 (status): the engine leg converting reports the existing
    /// rate breaks; a stream PipeWire still resamples keeps `Resampled`,
    /// and the bypass resampler's status says why.
    #[test]
    fn the_engine_leg_reports_its_conversion_and_why_pipewire_resamples() {
        // Direct, 44.1 kHz converted to 48 kHz by the engine.
        let mut snap = snapshot();
        snap.active_sink = Some(SinkChoice::Direct);
        snap.ratio = audivane_protocol::RateRatio::from_rates(44_100, 48_000);
        snap.output_rate = 48_000;
        let p = compute(&snap, &[stream()], Some(&sink()), None);
        assert!(!p.intact);
        assert_eq!(
            p.broken_because,
            vec![PassthroughBreak::RateConversion {
                graph_rate: 44_100,
                output_rate: 48_000
            }]
        );
        // A second client at another rate: still `Resampled` for it.
        let mut other = stream();
        other.node = 11;
        other.rate = Some(96_000);
        let p = compute(&snap, &[stream(), other.clone()], Some(&sink()), None);
        assert!(p.broken_because.contains(&PassthroughBreak::Resampled {
            stream_rate: 96_000,
            graph_rate: 44_100
        }));

        let cfg = EngineConfig::default();
        let idle = BypassResamplerStatus {
            mode: BypassResampler::Engine,
            active: false,
            reason: None,
        };
        let direct = Some(SinkChoice::Direct);
        let lands = |_| Some(48_000);
        // Converting: nothing to explain.
        let active = BypassResamplerStatus {
            active: true,
            ..idle.clone()
        };
        let st = bypass_resampler_status(Some(active), &cfg, &snap, &[stream()], direct, lands);
        assert!(st.unwrap().reason.is_none());
        // A native graph rate with every stream at it: nothing either.
        let mut native = snapshot();
        native.active_sink = direct;
        let st = bypass_resampler_status(
            Some(idle.clone()),
            &cfg,
            &native,
            &[stream()],
            direct,
            lands,
        );
        assert!(st.unwrap().reason.is_none());
        // Another stream decides the graph rate.
        let st = bypass_resampler_status(
            Some(idle.clone()),
            &cfg,
            &native,
            &[stream(), other.clone()],
            direct,
            lands,
        );
        assert!(
            st.unwrap()
                .reason
                .unwrap()
                .contains("another stream plays at 44100 Hz")
        );
        // The engine cannot convert the stream's rate.
        let st = bypass_resampler_status(
            Some(idle.clone()),
            &cfg,
            &native,
            &[other.clone()],
            direct,
            |_| None,
        );
        let reason = st.unwrap().reason.unwrap();
        assert!(
            reason.starts_with("PipeWire resamples the 96000 Hz stream"),
            "{reason}"
        );
        assert!(reason.contains("cannot convert"), "{reason}");
        // A rate outside the allowed rates.
        let mut narrow = cfg.clone();
        narrow.rate.allowed_rates = vec![44_100, 48_000];
        let st = bypass_resampler_status(
            Some(idle.clone()),
            &narrow,
            &native,
            &[other.clone()],
            direct,
            lands,
        );
        assert!(
            st.unwrap()
                .reason
                .unwrap()
                .contains("not in the allowed rates")
        );
        // DSP on, DSP sink: not the bypass path, no reason.
        let st = bypass_resampler_status(
            Some(idle.clone()),
            &cfg,
            &native,
            &[other.clone()],
            Some(SinkChoice::Dsp),
            |_| None,
        );
        assert!(st.unwrap().reason.is_none());
        // PipeWire mode: no reason.
        let pw = BypassResamplerStatus {
            mode: BypassResampler::Pipewire,
            ..idle
        };
        let st = bypass_resampler_status(Some(pw), &cfg, &native, &[other], direct, |_| None);
        assert!(st.unwrap().reason.is_none());
        assert!(bypass_resampler_status(None, &cfg, &native, &[], direct, lands).is_none());
    }

    #[test]
    fn active_varispeed_breaks_passthrough_even_at_unity_and_inactive_stays_intact() {
        use audivane_protocol::ipc::{VarispeedConfig, VarispeedStatus};

        // Enabled in config, not running (Direct / DSP off / idle): bit-perfect.
        let mut snap = snapshot();
        snap.varispeed = Some(VarispeedStatus::from_config(&VarispeedConfig {
            enabled: true,
            speed: 1.05,
            ramp_ms: 200.0,
            keep_pitch: false,
        }));
        let p = compute(&snap, &[stream()], Some(&sink()), None);
        assert!(p.intact, "{:?}", p.broken_because);

        // Active at unity with a flat chain: DspActive + Varispeed { speed: 1.0 }.
        let mut active_unity = VarispeedStatus::from_config(&VarispeedConfig {
            enabled: true,
            speed: 1.0,
            ramp_ms: 200.0,
            keep_pitch: false,
        });
        active_unity.active = true;
        active_unity.speed = 1.0;
        snap.varispeed = Some(active_unity);
        let p = compute(&snap, &[stream()], Some(&sink()), None);
        assert!(!p.intact);
        assert_eq!(
            p.broken_because,
            vec![
                PassthroughBreak::DspActive,
                PassthroughBreak::Varispeed { speed: 1.0 },
            ]
        );

        // Active during a ramp: reports applied speed, never target/requested.
        let mut ramping = VarispeedStatus::from_config(&VarispeedConfig {
            enabled: true,
            speed: 1.5,
            ramp_ms: 500.0,
            keep_pitch: false,
        });
        ramping.active = true;
        ramping.speed = 1.08;
        ramping.target = 1.5;
        ramping.ramping = true;
        snap.dsp_active = true;
        snap.varispeed = Some(ramping);
        let p = compute(&snap, &[stream()], Some(&sink()), None);
        assert_eq!(
            p.broken_because,
            vec![
                PassthroughBreak::DspActive,
                PassthroughBreak::Varispeed { speed: 1.08 },
            ]
        );
    }
}
