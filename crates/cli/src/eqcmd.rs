//! `avdsp eq`: import, export and evaluate EQ profiles.

use crate::{ChannelArg, EqCommand};
use anyhow::{Context, Result, bail};
use audivane_dsp_core::biquad::Eq;
use audivane_protocol::EqProfile;
use serde_json::json;
use std::fs;

pub fn run(command: EqCommand) -> Result<i32> {
    match command {
        EqCommand::Import { input, json: out } => import(&input, out.as_deref()),
        EqCommand::Export { input, channel } => export(&input, channel),
        EqCommand::Response {
            input,
            rate,
            points,
            freqs,
            channel,
            json: as_json,
        } => response(&input, rate, points, freqs.as_deref(), channel, as_json),
    }
}

fn import(input: &std::path::Path, out: Option<&std::path::Path>) -> Result<i32> {
    let text =
        fs::read_to_string(input).with_context(|| format!("cannot read {}", input.display()))?;
    let parsed = audivane_dsp_core::eq_apo::parse(&text)
        .map_err(|e| anyhow::anyhow!("{}: {e}", input.display()))?;
    for warning in &parsed.warnings {
        eprintln!("{}: {warning}", input.display());
    }
    match out {
        Some(path) => {
            crate::profile::save_json(path, &parsed.profile)?;
            println!(
                "imported {} bands into {} (preamp {:.2} dB)",
                parsed.profile.bands.len(),
                path.display(),
                parsed.profile.preamp_db
            );
        }
        None => {
            println!(
                "{}",
                serde_json::to_string_pretty(&parsed.profile)
                    .context("cannot serialize profile")?
            );
        }
    }
    Ok(0)
}

fn channel_profile(profile: EqProfile, channel: ChannelArg) -> EqProfile {
    let sec = match channel {
        ChannelArg::Left => profile.left.as_ref().filter(|c| c.enabled),
        ChannelArg::Right => profile.right.as_ref().filter(|c| c.enabled),
    };
    let mut bands = if profile.linked_enabled {
        profile.bands
    } else {
        Vec::new()
    };
    let mut preamp_db = profile.preamp_db;
    if let Some(sec) = sec {
        preamp_db += sec.preamp_db;
        bands.extend(sec.bands.iter().cloned());
    }
    EqProfile {
        name: profile.name,
        preamp_db,
        bands,
        ..Default::default()
    }
}

fn export(input: &std::path::Path, channel: ChannelArg) -> Result<i32> {
    let profile = channel_profile(crate::profile::load(input)?, channel);
    print!("{}", audivane_dsp_core::eq_apo::to_apo_string(&profile));
    Ok(0)
}

fn response(
    input: &std::path::Path,
    rate: u32,
    points: usize,
    freqs: Option<&str>,
    channel: ChannelArg,
    as_json: bool,
) -> Result<i32> {
    if rate == 0 {
        bail!("--rate must be greater than zero");
    }
    if points < 2 {
        bail!("--points must be at least 2");
    }
    let profile = crate::profile::load(input)?;
    let ch_idx = match channel {
        ChannelArg::Left => 0,
        ChannelArg::Right => 1,
    };
    let nyquist = f64::from(rate) / 2.0;
    let frequencies: Vec<f64> = match freqs {
        Some(list) => {
            let mut out = Vec::new();
            for item in list.split(',') {
                let f: f64 = item
                    .trim()
                    .parse()
                    .with_context(|| format!("bad frequency {item:?}"))?;
                if !(f.is_finite() && f > 0.0 && f < nyquist) {
                    bail!("frequency {f} Hz is outside (0, {nyquist}) Hz");
                }
                out.push(f);
            }
            if out.is_empty() {
                bail!("--freqs needs at least one frequency");
            }
            out
        }
        None => {
            let start: f64 = 20.0;
            let end = 20_000.0f64.min(nyquist * 0.999);
            if end <= start {
                bail!("sample rate {rate} Hz is too low to plot a response");
            }
            (0..points)
                .map(|i| {
                    let t = i as f64 / (points - 1) as f64;
                    start * (end / start).powf(t)
                })
                .collect()
        }
    };

    let eq = Eq::new(&profile, f64::from(rate), 2).map_err(anyhow::Error::msg)?;
    if let Some(clamped) = eq.tilt_clamped_end_hz() {
        eprintln!("note: tilt end_hz clamped to {clamped:.1} Hz at {rate} Hz sample rate");
    }
    let rows: Vec<(f64, f64, f64)> = frequencies
        .iter()
        .map(|&f| {
            let h = eq.response_channel(ch_idx, f);
            let mag_db = 20.0 * h.norm().max(1e-300).log10();
            let phase_deg = h.arg().to_degrees();
            (f, mag_db, phase_deg)
        })
        .collect();

    if as_json {
        let points: Vec<_> = rows
            .iter()
            .map(|&(f, mag, phase)| json!({"freq_hz": f, "magnitude_db": mag, "phase_deg": phase}))
            .collect();
        println!("{}", json!({"rate": rate, "points": points}));
    } else {
        println!("freq_hz,magnitude_db,phase_deg");
        for (f, mag, phase) in rows {
            println!("{f:.9},{mag:.12},{phase:.12}");
        }
    }
    Ok(0)
}
