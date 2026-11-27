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
