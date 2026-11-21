//! The daemon's validation of tier A of the filter fingerprint matrix
//! (`crates/dsp-core/tests/filter_fingerprints.rs`), with 2 and 4 rational
//! pipelines (`rational_pipelines(false)` and `(true)`), recorded in
//! `tests/fixtures/filter-validation.tsv`:
//!
//! ```text
//! name source_rate up/down validate2 validate4 rational2 rational4
//! ```
//!
//! - `validate<n>`: `validate_upsampler_at` on the design with its target
//!   (`Ratio(k)` for an integer ratio, `Rate(output)` otherwise) and
//!   `rates = [source_rate]`, as a config or the measured-response call
//!   would be validated.
//! - `rational<n>`: `check_rational_at` at the case's ratio (rational
//!   ratios only, else `-`).
//!
//! Each is `ok`, or the refusal's class and the FNV-1a hash of its
//! message (`cost:1a2b3c4d`). The classes are `cost` (the multiply-add
//! budget), `too_long` (the grid length limit), `ratio` (the preset does
//! not run at this ratio) and `error`.
//!
//! For preset cases at a rational ratio, `check_rational_at`'s class must
//! also equal the preset checks the dsp-core fixture recorded (its
//! `paths` column), so the two tests agree on which cases production
//! refuses.
//!
//! `AVDSP_FINGERPRINT_WRITE=1` writes the fixture; the default compares.

#[path = "../../dsp-core/tests/fingerprint_common/matrix.rs"]
mod matrix;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;

use audivane_dsp_core::bypass::hash_u64;
use audivane_dsp_daemon::control::config::{
    check_rational_at, rational_pipelines, validate_upsampler_at,
};
use audivane_protocol::{RateTarget, UpsamplerConfig};
use matrix::{Case, Design};

const FIXTURE: &str = "filter-validation.tsv";
const COLUMNS: &str = "name\tsource_rate\tratio\tvalidate2\tvalidate4\trational2\trational4";

fn class(r: Result<(), String>) -> String {
    match r {
        Ok(()) => "ok".into(),
        Err(msg) => {
            let class = if msg.contains("multiply-adds") {
                "cost"
            } else if msg.contains(" MB)") {
                "too_long"
            } else if msg.contains(".preset:") && msg.contains("needs") {
                "ratio"
            } else {
                "error"
            };
            let h = hash_u64(msg.bytes().map(u64::from));
            format!("{class}:{:08x}", h as u32)
        }
    }
}

/// The upsampler config the case validates.
fn config(case: &Case) -> UpsamplerConfig {
    let mut up = case.design.config();
    up.target = match case.ratio.as_integer() {
        Some(k) => RateTarget::Ratio(k),
        None => RateTarget::Rate(case.output_rate()),
    };
    up
}

fn row(case: &Case) -> String {
    let up = config(case);
    let field = "upsampler";
    let mut cols = Vec::new();
    for ab in [false, true] {
        cols.push(class(validate_upsampler_at(
            field,
            &up,
            &[case.source_rate],
            rational_pipelines(ab),
        )));
    }
    for ab in [false, true] {
        cols.push(if case.ratio.is_integer() {
            "-".into()
        } else {
            class(check_rational_at(
                field,
                &up,
                case.source_rate,
                case.ratio,
                rational_pipelines(ab),
            ))
        });
    }
    cols.join("\t")
}

fn fixture_path() -> PathBuf {
    std::env::var_os("AVDSP_FINGERPRINT_DIR")
        .map(|d| PathBuf::from(d).join("daemon"))
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures"))
        .join(FIXTURE)
}

fn read_rows(text: &str, key_columns: usize) -> BTreeMap<String, String> {
    text.lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
        .map(|l| {
            let parts: Vec<&str> = l.splitn(key_columns + 1, '\t').collect();
            (
                parts[..key_columns].join("\t"),
                parts.get(key_columns).copied().unwrap_or("").to_string(),
            )
        })
        .collect()
}

/// The dsp-core fixture's preset checks (`c2,c4,build`) per key.
fn core_paths() -> Option<BTreeMap<String, (String, String)>> {
    let dir = std::env::var_os("AVDSP_FINGERPRINT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../dsp-core/tests/fixtures")
        });
    let text = std::fs::read_to_string(dir.join("filter-fingerprints.tsv")).ok()?;
    Some(
        read_rows(&text, 3)
            .into_iter()
            .filter_map(|(k, rest)| {
                // kind status paths ...
                let paths = rest.split('\t').nth(2)?;
                let mut p = paths.split(',');
                Some((k, (p.next()?.to_string(), p.next()?.to_string())))
            })
            .collect(),
    )
}

#[test]
fn filter_validation_tier_a() {
    let cases = matrix::tier_a();
    let rows: Vec<(String, String)> = cases.iter().map(|c| (c.key(), row(c))).collect();

    // check_rational_at and the dsp-core harness's preset checks agree.
    let mut failures = Vec::new();
    match core_paths() {
        Some(core) => {
            for (c, (key, r)) in cases.iter().zip(&rows) {
                if !matches!(c.design, Design::Preset(_)) || c.ratio.is_integer() {
                    continue;
                }
                let Some((c2, c4)) = core.get(key) else {
                    failures.push(format!("{key}: not in the dsp-core fixture"));
                    continue;
                };
                let cols: Vec<&str> = r.split('\t').collect();
                let short = |s: &str| s.split(':').next().unwrap_or("").to_string();
                if short(cols[2]) != *c2 || short(cols[3]) != *c4 {
                    failures.push(format!(
                        "{}: check_rational_at gives {},{} but the dsp-core fixture {c2},{c4}",
                        key.replace('\t', " "),
                        short(cols[2]),
                        short(cols[3])
                    ));
                }
            }
        }
        None => failures.push("no dsp-core fixture to cross-check".into()),
    }

    let path = fixture_path();
    if std::env::var("AVDSP_FINGERPRINT_WRITE").is_ok_and(|v| v == "1") {
        assert!(failures.is_empty(), "{}", failures.join("\n"));
        let mut text = String::new();
        let _ = writeln!(
            text,
            "# The daemon's validation of filter fingerprint tier A. See \
             crates/daemon/tests/filter_validation.rs."
        );
        let _ = writeln!(text, "# {COLUMNS}");
        for (k, r) in &rows {
            let _ = writeln!(text, "{k}\t{r}");
        }
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        println!("wrote {} ({} rows)", path.display(), rows.len());
        return;
    }
    let want = read_rows(
        &std::fs::read_to_string(&path).unwrap_or_else(|_| {
            panic!(
                "no fixture at {} (AVDSP_FINGERPRINT_WRITE=1 writes it)",
                path.display()
            )
        }),
        3,
    );
    let names: Vec<&str> = COLUMNS.split('\t').collect();
    for (k, got) in &rows {
        let shown = k.replace('\t', " ");
        match want.get(k) {
            None => failures.push(format!("{shown}: not in the fixture")),
            Some(w) if w != got => {
                for ((name, a), b) in names[3..].iter().zip(w.split('\t')).zip(got.split('\t')) {
                    if a != b {
                        failures.push(format!("{shown}: {name} {a} -> {b}"));
                    }
                }
            }
            Some(_) => {}
        }
    }
    if want.len() != rows.len() {
        failures.push(format!(
            "the fixture has {} rows, the matrix {}",
            want.len(),
            rows.len()
        ));
    }
    if !failures.is_empty() {
        for f in &failures {
            println!("DIFF {f}");
        }
        panic!("{} validation differences (listed above)", failures.len());
    }
    println!("{} cases match the fixture", rows.len());
}
