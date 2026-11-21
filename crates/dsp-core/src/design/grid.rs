//! The polyphase grid of a ratio: the only place that knows the ratio.
//!
//! With `L/M` = output rate / input rate (reduced), a kernel lives on the
//! grid at `input * L` and has L polyphase branches. An integer ratio is
//! `M = 1`.

use audivane_protocol::RateRatio;

/// The grid of `ratio` from one source rate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Grid {
    pub ratio: RateRatio,
    /// L, the polyphase branch count.
    pub up: usize,
    /// M.
    pub down: usize,
    /// `R = max(L, M)`.
    pub design_ratio: usize,
    /// The output rate is below the source rate.
    pub decimating: bool,
    /// One of the two rates is an integer multiple of the other.
    pub multiple: bool,
}

impl Grid {
    pub fn new(source_rate: u32, ratio: RateRatio) -> Self {
        let up = ratio.up as usize;
        let down = ratio.down as usize;
        let src = u64::from(source_rate);
        let out = src * u64::from(ratio.up) / u64::from(ratio.down);
        let (hi, lo) = (src.max(out), src.min(out));
        Self {
            ratio,
            up,
            down,
            design_ratio: up.max(down),
            decimating: ratio.is_decimating(),
            multiple: lo > 0 && hi.is_multiple_of(lo),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grids() {
        let g = Grid::new(44_100, RateRatio::integer(8));
        assert_eq!((g.up, g.down, g.design_ratio), (8, 1, 8));
        assert!(g.multiple && !g.decimating);
        let g = Grid::new(44_100, RateRatio::from_rates(44_100, 48_000));
        assert_eq!((g.up, g.down, g.design_ratio), (160, 147, 160));
        assert!(!g.multiple && !g.decimating);
        let g = Grid::new(96_000, RateRatio::from_rates(96_000, 48_000));
        assert_eq!((g.up, g.down, g.design_ratio), (1, 2, 2));
        assert!(g.multiple && g.decimating);
        let g = Grid::new(48_000, RateRatio::from_rates(48_000, 44_100));
        assert!(!g.multiple && g.decimating);
    }
}
