//! Equalizer APO / AutoEQ `ParametricEQ.txt` import and export.
//!
//! Supported lines (case-insensitive, extra whitespace ok, `#` comments):
//! `Preamp: -6.2 dB`
//! `Filter 1: ON PK Fc 105 Hz Gain 3.4 dB Q 0.70`
//! Types: PK/PEQ, LS/LSC, HS/HSC (with optional Q, default 0.7071), LP/LPQ,
//! HP/HPQ, BP, NO, AP. `OFF` filters import as disabled bands.
//! `Channel: L` / `Channel: R` put the filters (and a `Preamp`) that follow
//! into that channel's own section (`EqProfile::left` / `right`);
//! `Channel: all` goes back to the linked one, and any other channel list
//! is read as all, with a warning. Unknown directives (Device:,
//! Include: …) are skipped with a warning.
//!
//! The tone knobs (`EqProfile::tone`) export as two shelves at the end of
//! the linked filters; a file whose linked filters end with exactly those
//! shelves (same type, corner and Q, a 0.5 dB step within ±6 dB) imports
//! them back as the tone.
//!
//! Beyond the documented subset, `BW <octaves>` and `S <slope>` are accepted
//! in place of `Q`, because Equalizer APO itself writes those.  They are
//! converted to the equivalent Q at import time; export always writes Q.

use audivane_protocol::{
    Band, ChannelEq, EqProfile, FilterType, TONE_BASS_HZ, TONE_BASS_Q, TONE_MAX_DB, TONE_TREBLE_HZ,
    TONE_TREBLE_Q, ToneConfig,
};
use std::fmt::Write as _;

#[derive(Debug, Clone, PartialEq)]
pub struct ParseError {
    pub line: usize,
    pub message: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for ParseError {}

#[derive(Debug, Clone, PartialEq)]
pub struct Parsed {
    pub profile: EqProfile,
    /// Human-readable notes about skipped lines.
    pub warnings: Vec<String>,
}

fn err(line: usize, message: impl Into<String>) -> ParseError {
    ParseError {
        line,
        message: message.into(),
    }
}

/// Parse an Equalizer APO / AutoEQ parametric filter file.
pub fn parse(text: &str) -> Result<Parsed, ParseError> {
    let mut profile = EqProfile::default();
    let mut warnings = Vec::new();
    // 0: linked, 1: left, 2: right.
    let mut sides: [(f64, Vec<Band>); 3] = Default::default();
    let mut ch = 0usize;

    for (index, raw) in text.lines().enumerate() {
        let line_no = index + 1;
        // Everything after `#` is a comment.
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }

        let Some((key, rest)) = line.split_once(':') else {
            let first = line
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_ascii_lowercase();
            if first == "preamp" || first == "filter" {
                return Err(err(line_no, "expected `:` after the directive name"));
            }
            warnings.push(format!(
                "line {line_no}: ignored unknown directive `{first}`"
            ));
            continue;
        };

        let key = key.trim();
        let key_lower = key.to_ascii_lowercase();
        let is_filter = key_lower == "filter"
            || key_lower
                .strip_prefix("filter")
                .is_some_and(|suffix| suffix.trim().parse::<u32>().is_ok());

        if key_lower == "preamp" {
            sides[ch].0 = parse_preamp(rest, line_no)?;
        } else if is_filter {
            match parse_filter(rest, line_no)? {
                Some(band) => sides[ch].1.push(band),
                None => warnings.push(format!("line {line_no}: skipped `None` filter")),
            }
        } else if key_lower == "channel" {
            let v = rest.trim();
            ch = if v.eq_ignore_ascii_case("l") {
                1
            } else if v.eq_ignore_ascii_case("r") {
                2
            } else {
                if !v.eq_ignore_ascii_case("all") {
                    warnings.push(format!(
                        "line {line_no}: channel `{v}` read as all channels"
                    ));
                }
                0
            };
        } else {
            warnings.push(format!("line {line_no}: ignored unknown directive `{key}`"));
        }
    }

    let [(preamp_db, bands), left, right] = sides;
    profile.preamp_db = preamp_db;
    profile.bands = bands;
    profile.tone = take_tone(&mut profile.bands);
    let side = |(preamp_db, bands): (f64, Vec<Band>)| ChannelEq {
        enabled: true,
        preamp_db,
        bands,
    };
    if !left.1.is_empty() || left.0 != 0.0 || !right.1.is_empty() || right.0 != 0.0 {
        profile.left = Some(side(left));
        profile.right = Some(side(right));
    }
    Ok(Parsed { profile, warnings })
}

/// Whether `band` is the tone shelf of `kind` (corner and Q exact, gain a
/// non-zero 0.5 dB step within the knob's range).
fn is_tone_shelf(band: &Band, kind: FilterType, hz: f64, q: f64) -> bool {
    band.enabled
        && band.kind == kind
        && band.freq_hz == hz
        && band.q == q
        && band.gain_db != 0.0
        && band.gain_db.abs() <= TONE_MAX_DB
        && (band.gain_db * 2.0).fract() == 0.0
}

/// Pull the tone shelves (bass then treble, either one alone) off the end of
/// the linked filters, as [`to_apo_string`] writes them.
fn take_tone(bands: &mut Vec<Band>) -> Option<ToneConfig> {
    let mut tone = ToneConfig::default();
    if let Some(b) = bands.last()
        && is_tone_shelf(b, FilterType::HighShelf, TONE_TREBLE_HZ, TONE_TREBLE_Q)
    {
        tone.treble_db = b.gain_db;
        bands.pop();
    }
    if let Some(b) = bands.last()
        && is_tone_shelf(b, FilterType::LowShelf, TONE_BASS_HZ, TONE_BASS_Q)
    {
        tone.bass_db = b.gain_db;
        bands.pop();
    }
    tone.is_active().then_some(tone)
}

fn parse_preamp(rest: &str, line: usize) -> Result<f64, ParseError> {
    let mut tokens = rest.split_whitespace();
    let Some(value) = tokens.next() else {
        return Err(err(line, "`Preamp` is missing a value"));
    };
    let db = parse_number(Some(value), line, "Preamp")?;
    if let Some(unit) = tokens.next()
        && !unit.eq_ignore_ascii_case("db")
    {
        return Err(err(
            line,
            format!("unexpected token `{unit}` after the Preamp value"),
        ));
    }
    Ok(db)
}

fn parse_filter(spec: &str, line: usize) -> Result<Option<Band>, ParseError> {
    let mut tokens = spec.split_whitespace().peekable();

    let Some(mut token) = tokens.next() else {
        return Err(err(line, "empty filter definition"));
    };
    let mut enabled = true;
    if token.eq_ignore_ascii_case("on") || token.eq_ignore_ascii_case("off") {
        enabled = token.eq_ignore_ascii_case("on");
        let Some(next) = tokens.next() else {
            return Err(err(line, "missing filter type"));
        };
        token = next;
    }

    let kind = match token.to_ascii_lowercase().as_str() {
        "pk" | "peq" => FilterType::Peaking,
        "ls" | "lsc" => FilterType::LowShelf,
        "hs" | "hsc" => FilterType::HighShelf,
        "lp" | "lpq" => FilterType::LowPass,
        "hp" | "hpq" => FilterType::HighPass,
        "bp" => FilterType::BandPass,
        "no" => FilterType::Notch,
        "ap" => FilterType::AllPass,
        "none" => return Ok(None),
        other => return Err(err(line, format!("unknown filter type `{other}`"))),
    };

    let mut freq_hz: Option<f64> = None;
    let mut gain_db: Option<f64> = None;
    let mut q: Option<f64> = None;
    let mut bandwidth: Option<f64> = None;
    let mut slope: Option<f64> = None;

    while let Some(token) = tokens.next() {
        match token.to_ascii_lowercase().as_str() {
            "fc" => {
                freq_hz = Some(parse_number(tokens.next(), line, "Fc")?);
                skip_unit(&mut tokens, "hz");
            }
            "gain" => {
                gain_db = Some(parse_number(tokens.next(), line, "Gain")?);
                skip_unit(&mut tokens, "db");
            }
            "q" => q = Some(parse_number(tokens.next(), line, "Q")?),
            "bw" => {
                // Equalizer APO writes `BW Oct 1`: the unit comes first.
                if tokens.peek().is_some_and(|t| t.eq_ignore_ascii_case("oct")) {
                    tokens.next();
                }
                bandwidth = Some(parse_number(tokens.next(), line, "BW")?);
                skip_unit(&mut tokens, "oct");
            }
            "s" => slope = Some(parse_number(tokens.next(), line, "S")?),
            other => return Err(err(line, format!("unexpected filter parameter `{other}`"))),
        }
    }

    let freq_hz = freq_hz.ok_or_else(|| err(line, "filter is missing `Fc`"))?;
    let gain_db = gain_db.unwrap_or(0.0);

    let q = if let Some(q) = q {
        q
    } else if let Some(bw) = bandwidth {
        if !bw.is_finite() || bw <= 0.0 {
            return Err(err(line, "`BW` must be positive"));
        }
        let w = 2f64.powf(bw);
        w.sqrt() / (w - 1.0)
    } else if let Some(s) = slope {
        // Cookbook shelf slope S -> Q, using the band's gain.
        if !s.is_finite() || s <= 0.0 {
            return Err(err(line, "`S` must be positive"));
        }
        let a = 10f64.powf(gain_db / 40.0);
        let inner = (a + 1.0 / a) * (1.0 / s - 1.0) + 2.0;
        if !inner.is_finite() || inner <= 0.0 {
            return Err(err(line, "shelf slope `S` is out of range for this gain"));
        }
        1.0 / inner.sqrt()
    } else {
        std::f64::consts::FRAC_1_SQRT_2
    };

    if !q.is_finite() || q <= 0.0 {
        return Err(err(line, "`Q` must be a positive number"));
    }

    Ok(Some(Band {
        kind,
        freq_hz,
        gain_db,
        q,
        enabled,
    }))
}

fn skip_unit(tokens: &mut std::iter::Peekable<std::str::SplitWhitespace<'_>>, unit: &str) {
    if let Some(next) = tokens.peek()
        && next.eq_ignore_ascii_case(unit)
    {
        tokens.next();
    }
}

fn parse_number(token: Option<&str>, line: usize, what: &str) -> Result<f64, ParseError> {
    let Some(token) = token else {
        return Err(err(line, format!("`{what}` is missing a value")));
    };
    match token.parse::<f64>() {
        Ok(v) if v.is_finite() => Ok(v),
        _ => Err(err(
            line,
            format!("`{what}` has an invalid number `{token}`"),
        )),
    }
}

fn type_name(kind: FilterType) -> &'static str {
    match kind {
        FilterType::Peaking => "PK",
        FilterType::LowShelf => "LS",
        FilterType::HighShelf => "HS",
        FilterType::LowPass => "LP",
        FilterType::HighPass => "HP",
        FilterType::BandPass => "BP",
        FilterType::Notch => "NO",
        FilterType::AllPass => "AP",
    }
}

/// Serialize back to Equalizer APO syntax (round-trips through `parse`).
///
/// `EqProfile::name` is not part of the Equalizer APO format and is not
/// written; every other field round-trips exactly (numbers are rendered with
/// Rust's shortest round-trip `f64` formatting).
pub fn to_apo_string(profile: &EqProfile) -> String {
    let mut out = String::new();
    // Writing to a String cannot fail.
    let _ = writeln!(out, "Preamp: {} dB", profile.preamp_db);
    write_bands(&mut out, &profile.bands, 1);
    let tone = profile.tone_bands();
    if !tone.is_empty() {
        let _ = writeln!(out, "# Tone (bass and treble knobs)");
        write_bands(&mut out, &tone, profile.bands.len() + 1);
    }
    let mut per_channel = false;
    for (name, c) in [("L", &profile.left), ("R", &profile.right)] {
        let Some(c) = c else { continue };
        if !c.enabled || (c.bands.is_empty() && c.preamp_db == 0.0) {
            continue;
        }
        per_channel = true;
        let _ = writeln!(out, "Channel: {name}");
        if c.preamp_db != 0.0 {
            let _ = writeln!(out, "Preamp: {} dB", c.preamp_db);
        }
        write_bands(&mut out, &c.bands, 1);
    }
    if per_channel {
        let _ = writeln!(out, "Channel: all");
    }
    out
}

fn write_bands(out: &mut String, bands: &[Band], first: usize) {
    for (i, band) in bands.iter().enumerate() {
        let _ = write!(
            out,
            "Filter {}: {} {} Fc {} Hz",
            i + first,
            if band.enabled { "ON" } else { "OFF" },
            type_name(band.kind),
            band.freq_hz
        );
        // Peaking and shelf filters always carry a Gain in Equalizer APO.
        let uses_gain = matches!(
            band.kind,
            FilterType::Peaking | FilterType::LowShelf | FilterType::HighShelf
        );
        if uses_gain || band.gain_db != 0.0 {
            let _ = write!(out, " Gain {} dB", band.gain_db);
        }
        let _ = writeln!(out, " Q {}", band.q);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AUTOEQ: &str = "\
# AutoEQ-style profile for a headphone
Preamp: -6.2 dB
Filter 1: ON PK Fc 105 Hz Gain 3.4 dB Q 0.70
Filter 2: ON PK Fc 3200 Hz Gain -2.1 dB Q 1.41
Filter 3: OFF PK Fc 1000 Hz Gain 5.0 dB Q 0.50
Filter 4: ON LSC Fc 105 Hz Gain 5.5 dB Q 0.71
Filter 5: ON HSC Fc 10000 Hz Gain -4.0 dB Q 0.71
Filter 6: ON PK Fc 200 Hz Gain -1.2 dB Q 0.9
Filter 7: ON PK Fc 4000 Hz Gain 2.2 dB Q 2.0
Device: Speakers
Channel: all
";

    #[test]
    fn parses_autoeq_file() {
        let parsed = parse(AUTOEQ).unwrap();
        assert!((parsed.profile.preamp_db + 6.2).abs() < 1e-12);
        assert_eq!(parsed.profile.bands.len(), 7);
        assert_eq!(parsed.profile.bands[0].kind, FilterType::Peaking);
        assert!((parsed.profile.bands[0].freq_hz - 105.0).abs() < 1e-12);
        assert!((parsed.profile.bands[0].gain_db - 3.4).abs() < 1e-12);
        assert!((parsed.profile.bands[0].q - 0.70).abs() < 1e-12);
        assert!(parsed.profile.bands[0].enabled);
        assert_eq!(parsed.profile.bands[3].kind, FilterType::LowShelf);
        assert_eq!(parsed.profile.bands[4].kind, FilterType::HighShelf);
        assert!(!parsed.profile.bands[2].enabled);

        // Device is unknown; Channel: all is the linked section.
        assert_eq!(parsed.warnings.len(), 1, "{:?}", parsed.warnings);
        assert!(parsed.warnings[0].contains("Device"));
        assert_eq!(parsed.profile.left, None);
    }

    #[test]
    fn round_trips_every_type_and_odd_values() {
        let profile = EqProfile {
            name: "not serialized".into(),
            preamp_db: -3.25,
            bands: vec![
                Band {
                    kind: FilterType::Peaking,
                    freq_hz: 1234.5,
                    gain_db: -7.125,
                    q: 0.3,
                    enabled: true,
                },
                Band {
                    kind: FilterType::LowShelf,
                    freq_hz: 82.0,
                    gain_db: 4.5,
                    q: 1.1,
                    enabled: true,
                },
                Band {
                    kind: FilterType::HighShelf,
                    freq_hz: 11_000.0,
                    gain_db: -2.0,
                    q: 0.65,
                    enabled: false,
                },
                Band {
                    kind: FilterType::LowPass,
                    freq_hz: 20.0,
                    gain_db: 0.0,
                    q: std::f64::consts::FRAC_1_SQRT_2,
                    enabled: true,
                },
                Band {
                    kind: FilterType::HighPass,
                    freq_hz: 19_000.0,
                    gain_db: 0.0,
                    q: 3.0,
                    enabled: true,
                },
                Band {
                    kind: FilterType::BandPass,
                    freq_hz: 440.0,
                    gain_db: 0.0,
                    q: 8.0,
                    enabled: true,
                },
                Band {
                    kind: FilterType::Notch,
                    freq_hz: 60.0,
                    gain_db: 0.0,
                    q: 12.0,
                    enabled: true,
                },
                Band {
                    kind: FilterType::AllPass,
                    freq_hz: 5000.0,
                    gain_db: 0.0,
                    q: 0.5,
                    enabled: true,
                },
            ],
            ..Default::default()
        };
        let text = to_apo_string(&profile);
        let again = parse(&text).unwrap();
        assert_eq!(again.profile.preamp_db, profile.preamp_db);
        assert_eq!(again.profile.bands, profile.bands);
    }
}
