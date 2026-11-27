//! Short plain-word lines for the tray menu (mirrors app/src/lib/passthrough.ts).

use audivane_protocol::ipc::{OverloadAction, PassthroughBreak, Status};

pub fn passthrough_line(status: &Status) -> String {
    let p = &status.passthrough;
    let rate = format_rate(status.output_rate.max(status.graph_rate));
    if p.intact || p.broken_because.is_empty() {
        return format!("Bit-perfect · {rate}");
    }
    let chosen = p.broken_because.iter().any(|b| {
        matches!(
            b,
            PassthroughBreak::DspActive | PassthroughBreak::Upsampling { .. }
        )
    });
    if chosen {
        return match fallback_now(status) {
            Some(what) => format!("DSP active · {rate} · plays {what}"),
            None => format!("DSP active · {rate}"),
        };
    }
    let only_idle = p
        .broken_because
        .iter()
        .all(|b| matches!(b, PassthroughBreak::NotPlaying));
    if only_idle {
        return "Nothing playing".to_string();
    }
    format!("Not bit-perfect · {rate}")
}

/// What the rate playing now plays in place of the chosen filter
/// (`status.upsampler_fallback_now`), in a word or two.
fn fallback_now(status: &Status) -> Option<&'static str> {
    let f = status.upsampler_fallback_now.as_ref()?.fallback.as_ref()?;
    Some(match f.action {
        OverloadAction::Recommended => "Recommended",
        OverloadAction::Reverted => "the last filter that ran",
        _ => "a lighter filter",
    })
}

fn format_rate(r: u32) -> String {
    if r.is_multiple_of(1000) {
        format!("{} kHz", r / 1000)
    } else {
        format!("{:.1} kHz", f64::from(r) / 1000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Built from JSON so new optional Status fields never break this test.
    fn status(breaks: Vec<PassthroughBreak>) -> Status {
        serde_json::from_value(serde_json::json!({
            "version": "t", "state": "playing", "device": "hw:0,0", "error": null,
            "format": null, "graph_rate": 44100, "output_rate": 44100, "ratio": 1,
            "quantum": 1024, "latency_ms": 1.0, "xruns": 0, "dsp_load": 0.0,
            "dsp_enabled": true, "active_sink": null, "ab": null,
            "passthrough": { "intact": breaks.is_empty(), "broken_because": breaks },
        }))
        .unwrap()
    }

    #[test]
    fn lines() {
        assert_eq!(passthrough_line(&status(vec![])), "Bit-perfect · 44.1 kHz");
        assert_eq!(
            passthrough_line(&status(vec![PassthroughBreak::DspActive])),
            "DSP active · 44.1 kHz"
        );
        assert_eq!(
            passthrough_line(&status(vec![PassthroughBreak::Mixing { streams: 2 }])),
            "Not bit-perfect · 44.1 kHz"
        );
        assert_eq!(
            passthrough_line(&status(vec![PassthroughBreak::NotPlaying])),
            "Nothing playing"
        );
        let mut s = status(vec![PassthroughBreak::DspActive]);
        s.upsampler_fallback_now = serde_json::from_value(serde_json::json!({
            "source_rate": 44100, "output_rate": 352800, "load": 0.1,
            "fallback": {"action": "recommended", "cause": "design", "load": 0.1,
                         "reason": "too short here"},
        }))
        .unwrap();
        assert_eq!(
            passthrough_line(&s),
            "DSP active · 44.1 kHz · plays Recommended"
        );
    }
}
