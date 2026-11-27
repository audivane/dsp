//! Kernel lengths: taps per input sample from a design's length rule, and
//! the decimation length.

use super::{Decimation, DesignError, Grid, KernelDesign, Length, OddRule, Shape, Span};

/// The output-span ladder: the next power of two of a ladder above the
/// output rate (about 1.5 s at any rate).
pub fn constant_time_length(output_rate: u64) -> usize {
    const LADDER: [(u64, usize); 6] = [
        (50_000, 1 << 16),
        (100_000, 1 << 17),
        (250_000, 1 << 18),
        (500_000, 1 << 19),
        (1_000_000, 1 << 20),
        (2_000_000, 1 << 21),
    ];
    LADDER
        .iter()
        .find(|&&(limit, _)| output_rate < limit)
        .map_or(1 << 22, |&(_, n)| n)
}

/// Taps per input sample (per polyphase branch) of `k` from `source_rate`
/// on a grid of `up` branches, before any decimation rule.
pub fn per_input(k: &KernelDesign, up: usize, source_rate: u32) -> Result<usize, DesignError> {
    let bad = |what: &str| Err(DesignError::Unsupported(what.to_string()));
    match &k.shape {
        Shape::Lanczos { lobes } => Ok(2 * lobes + 1),
        Shape::WindowedSinc { length, .. } | Shape::Optimal { length, .. } => match length {
            Length::Fixed(_) | Length::Band | Length::Auto(_) => {
                bad("this length rule counts frames or is searched: the standard path only")
            }
            Length::PerInput(t) => match t.at(source_rate) {
                Some(t) => Ok(t),
                None => bad("the stepped length has no step for this source rate"),
            },
            Length::OutputSpan(span) => {
                if up == 0 {
                    return bad("an output-span length needs a grid");
                }
                // T = N / L - 1, so the kernel has N - L taps when L
                // divides N.
                let n = match span {
                    Span::Ladder => constant_time_length(u64::from(source_rate) * up as u64),
                    Span::Taps(n) => *n,
                };
                match (n / up).checked_sub(1) {
                    Some(t) => Ok(t),
                    None => bad("the output span is shorter than one tap per input sample"),
                }
            }
        },
    }
}

/// The length of `taps` per input sample at a decimating ratio:
/// `ceil(base * M / L)` made odd by the rule, or `taps` itself for
/// [`Decimation::Keep`].
pub fn decimated(taps: usize, rule: &Decimation, grid: &Grid) -> usize {
    let Decimation::Scale { odd, base_taps } = *rule else {
        return taps;
    };
    let base = base_taps.unwrap_or(taps);
    let mut t = (base as u64 * grid.down as u64).div_ceil(grid.up as u64) as usize;
    if t.is_multiple_of(2) {
        t = match odd {
            OddRule::Down => t - 1,
            OddRule::Up => t + 1,
        };
    }
    t
}
