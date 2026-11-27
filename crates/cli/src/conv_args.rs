//! Convolution items from command-line arguments (`avdsp process --conv`,
//! `avdsp ctl conv set/add`): a WAV path is one item, and a run of matrix
//! entries `IN:OUT[:CH]=PATH` is one filter-matrix item (`CH`: the WAV's
//! channel, 0-based; left out, the file must be mono).

use anyhow::{Result, bail};
use audivane_protocol::{ConvolutionConfig, MatrixEntry};
use std::path::{Path, PathBuf};

/// `IN:OUT[:CH]=PATH` as a matrix entry; `None` when `arg` is not one
/// (a plain path).
pub fn parse_matrix_entry(arg: &str) -> Option<MatrixEntry> {
    let (spec, path) = arg.split_once('=')?;
    let nums: Vec<usize> = spec
        .split(':')
        .map(|n| n.parse().ok())
        .collect::<Option<_>>()?;
    let (input, output, channel) = match nums[..] {
        [i, o] => (i, o, None),
        [i, o, ch] => (i, o, Some(ch)),
        _ => return None,
    };
    if path.is_empty() {
        return None;
    }
    Some(MatrixEntry {
        input,
        output,
        ir_path: PathBuf::from(path),
        channel,
        gain_db: 0.0,
    })
}

/// The items `args` give, in order: each plain path one item, each run of
/// matrix entries one matrix item. Every item gets `gain_db` and `name`.
/// Files must exist.
pub fn conv_items(
    args: &[PathBuf],
    gain_db: f64,
    name: Option<&str>,
) -> Result<Vec<ConvolutionConfig>> {
    let item = |ir_path: PathBuf, matrix: Option<Vec<MatrixEntry>>| ConvolutionConfig {
        name: name.map(str::to_string),
        enabled: true,
        ir_path,
        gain_db,
        matrix,
        calibration: false,
    };
    let check = |p: &Path| -> Result<()> {
        if !p.is_file() {
            bail!("{} does not exist", p.display());
        }
        Ok(())
    };
    let mut out = Vec::new();
    let mut entries: Vec<MatrixEntry> = Vec::new();
    for arg in args {
        match arg.to_str().and_then(parse_matrix_entry) {
            Some(e) => {
                check(&e.ir_path)?;
                entries.push(e);
            }
            None => {
                check(arg)?;
                if !entries.is_empty() {
                    out.push(item(PathBuf::new(), Some(std::mem::take(&mut entries))));
                }
                out.push(item(arg.clone(), None));
            }
        }
    }
    if !entries.is_empty() {
        out.push(item(PathBuf::new(), Some(entries)));
    }
    Ok(out)
}

/// A short label for an item: its name, else its file or "matrix (N)".
pub fn conv_label(c: &ConvolutionConfig) -> String {
    if let Some(n) = &c.name {
        return n.clone();
    }
    match &c.matrix {
        Some(m) => format!("matrix ({} filters)", m.len()),
        None => c
            .ir_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("?")
            .to_string(),
    }
}

/// The item's files: its `ir_path`, or each matrix entry as
/// `IN:OUT[:CH]=PATH`.
pub fn conv_files(c: &ConvolutionConfig) -> String {
    match &c.matrix {
        Some(m) => m
            .iter()
            .map(|e| {
                let ch = e.channel.map_or_else(String::new, |ch| format!(":{ch}"));
                format!("{}:{}{ch}={}", e.input, e.output, e.ir_path.display())
            })
            .collect::<Vec<_>>()
            .join(" "),
        None => c.ir_path.display().to_string(),
    }
}
