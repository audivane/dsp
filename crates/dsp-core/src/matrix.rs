//! FIR matrix convolution: `out_j = Σ_i F_ji * in_i`, `i` over the chain's
//! input channels and `j` over its output channels, any entry absent (zero).
//!
//! - [`combine_conv`] composes the enabled convolution items in series into
//!   one filter set, offline: the per-channel items alone take the existing
//!   path ([`crate::conv::combine_irs`], bit for bit), and any item with a
//!   cross term makes the result the matrix product `M_k · … · M_1` (each
//!   per-channel item the diagonal matrix of its IRs). A product that stays
//!   diagonal is given back per channel.
//! - [`MatrixConvolver`] runs a matrix with one single-channel
//!   [`Convolver`] per present entry, summing into each output; an output
//!   with no entry is silent. Every entry runs at the same base block, so
//!   all share one latency.

use crate::Stage;
use crate::chain::ChainError;
use crate::conv::{Convolver, MAX_COMBINED_IR_TAPS, combine_irs};
use crate::db_to_lin;
use crate::fft::{RealFftPair, SplitSpectrum, complex_mac_split};

/// A square filter matrix over `channels` channels, entries `[out][in]`.
#[derive(Clone, Debug, PartialEq)]
pub struct FilterMatrix {
    channels: usize,
    entries: Vec<Option<Vec<f64>>>,
}

impl FilterMatrix {
    /// All entries absent.
    pub fn new(channels: usize) -> Self {
        Self {
            channels,
            entries: vec![None; channels * channels],
        }
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// The filter from input `input` to output `out` (`None`: absent).
    pub fn get(&self, out: usize, input: usize) -> Option<&[f64]> {
        self.entries[out * self.channels + input].as_deref()
    }

    /// Set the filter from input `input` to output `out`.
    pub fn set(&mut self, out: usize, input: usize, ir: Vec<f64>) {
        self.entries[out * self.channels + input] = Some(ir);
    }

    /// The present entries as `(out, in, ir)`, row by row.
    pub fn entries(&self) -> impl Iterator<Item = (usize, usize, &[f64])> + '_ {
        let c = self.channels;
        self.entries
            .iter()
            .enumerate()
            .filter_map(move |(k, e)| e.as_deref().map(|ir| (k / c, k % c, ir)))
    }

    /// No present entry off the diagonal.
    pub fn is_diagonal(&self) -> bool {
        self.entries().all(|(o, i, _)| o == i)
    }

    /// The longest entry, taps.
    pub fn max_len(&self) -> usize {
        self.entries().map(|(_, _, ir)| ir.len()).max().unwrap_or(0)
    }

    /// The diagonal as one IR per channel (an absent entry is one zero tap).
    fn diagonal(&self) -> Vec<Vec<f64>> {
        (0..self.channels)
            .map(|c| self.get(c, c).map_or_else(|| vec![0.0], <[f64]>::to_vec))
            .collect()
    }
}

/// The combined convolution of a chain, already at its rate and scaled by
/// every gain.
#[derive(Clone, Debug, PartialEq)]
pub enum ConvIrs {
    /// One IR (every channel) or one per channel: the plain L→L, R→R form.
    Channels(Vec<Vec<f64>>),
    /// A filter matrix with at least one cross term.
    Matrix(FilterMatrix),
}

impl From<Vec<Vec<f64>>> for ConvIrs {
    fn from(irs: Vec<Vec<f64>>) -> Self {
        Self::Channels(irs)
    }
}

impl ConvIrs {
    /// The longest IR or entry, taps.
    pub fn max_len(&self) -> usize {
        match self {
            Self::Channels(irs) => irs.iter().map(Vec::len).max().unwrap_or(0),
            Self::Matrix(m) => m.max_len(),
        }
    }

    /// The bulk delay, taps: where the largest IR or entry peaks (the
    /// index of its largest magnitude; the latest over all of them). 0 for
    /// a minimum-phase IR, about half the length for a linear-phase one.
    pub fn bulk_delay(&self) -> usize {
        match self {
            Self::Channels(irs) => irs.iter().map(|ir| peak_index(ir)).max().unwrap_or(0),
            Self::Matrix(m) => m
                .entries()
                .map(|(_, _, ir)| peak_index(ir))
                .max()
                .unwrap_or(0),
        }
    }
}

/// The index of `ir`'s largest magnitude (the first of equals; 0 when
/// empty).
fn peak_index(ir: &[f64]) -> usize {
    let mut best = (0, 0.0);
    for (i, x) in ir.iter().enumerate() {
        if x.abs() > best.1 {
            best = (i, x.abs());
        }
    }
    best.0
}

/// One entry of a matrix item: the filter from input `input` to output
/// `output`, scaled by `gain_db`.
#[derive(Clone, Copy, Debug)]
pub struct MatrixTap<'a> {
    pub input: usize,
    pub output: usize,
    pub ir: &'a [f64],
    pub gain_db: f64,
}

/// One enabled convolution item, its IRs at the chain's rate.
#[derive(Clone, Debug)]
pub enum ConvItem<'a> {
    /// One IR (every channel) or one per channel, as
    /// [`crate::conv::combine_irs`] takes them.
    Channels { irs: &'a [Vec<f64>], gain_db: f64 },
    /// A filter matrix; the item's `gain_db` scales every entry.
    Matrix {
        taps: Vec<MatrixTap<'a>>,
        gain_db: f64,
    },
}

impl ConvItem<'_> {
    fn gain_db(&self) -> f64 {
        match self {
            Self::Channels { gain_db, .. } | Self::Matrix { gain_db, .. } => *gain_db,
        }
    }

    /// The entry from `input` to `out` and its linear gain (a per-channel
    /// item is diagonal; the item's own gain is not included).
    fn entry(&self, out: usize, input: usize) -> Option<(&[f64], f64)> {
        match self {
            Self::Channels { irs, .. } => (out == input).then(|| {
                let ir = if irs.len() == 1 { &irs[0] } else { &irs[out] };
                (ir.as_slice(), 1.0)
            }),
            Self::Matrix { taps, .. } => taps
                .iter()
                .find(|t| t.input == input && t.output == out)
                .map(|t| (t.ir, db_to_lin(t.gain_db))),
        }
    }

    /// Entry lengths `[out][in]` (`None`: absent).
    fn shape(&self, c: usize) -> Vec<Option<usize>> {
        (0..c * c)
            .map(|k| self.entry(k / c, k % c).map(|(ir, _)| ir.len()))
            .collect()
    }

    fn validate(&self, channels: usize) -> Result<(), ChainError> {
        match self {
            Self::Channels { irs, .. } => {
                if irs.is_empty() || irs.iter().any(Vec::is_empty) {
                    return Err(ChainError::EmptyIr);
                }
                if irs.len() != 1 && irs.len() != channels {
                    return Err(ChainError::IrCount {
                        irs: irs.len(),
                        channels,
                    });
                }
            }
            Self::Matrix { taps, .. } => {
                if taps.is_empty() {
                    return Err(ChainError::Matrix("a filter matrix has no entries".into()));
                }
                for (k, t) in taps.iter().enumerate() {
                    if t.input >= channels || t.output >= channels {
                        return Err(ChainError::Matrix(format!(
                            "matrix entry {} -> {} is out of range for {channels} channels",
                            t.input, t.output
                        )));
                    }
                    if t.ir.is_empty() {
                        return Err(ChainError::EmptyIr);
                    }
                    if !t.gain_db.is_finite() {
                        return Err(ChainError::Matrix(format!(
                            "matrix entry {} -> {} gain is not finite",
                            t.input, t.output
                        )));
                    }
                    if taps[..k]
                        .iter()
                        .any(|u| u.input == t.input && u.output == t.output)
                    {
                        return Err(ChainError::Matrix(format!(
                            "matrix entry {} -> {} is given twice",
                            t.input, t.output
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    fn is_diagonal(&self) -> bool {
        match self {
            Self::Channels { .. } => true,
            Self::Matrix { taps, .. } => taps.iter().all(|t| t.input == t.output),
        }
    }
}

/// Compose the enabled convolution items in series (`items[0]` first) into
/// one filter set at `channels` channels, enforcing
/// [`MAX_COMBINED_IR_TAPS`] on every entry. Per-channel items alone (and
/// matrix items with no cross term) take [`combine_irs`]; otherwise the
/// result is the matrix product, diagonal results given back per channel.
pub fn combine_conv(items: &[ConvItem<'_>], channels: usize) -> Result<ConvIrs, ChainError> {
    if channels == 0 {
        return Err(ChainError::ZeroChannels);
    }
    if items.is_empty() {
        return Err(ChainError::MissingIr);
    }
    for item in items {
        item.validate(channels)?;
    }

    if items.iter().all(ConvItem::is_diagonal) {
        // A channel some item has no entry for is silent: its filters in
        // every item become one zero tap, so a long filter on a dead route
        // is not combined (the same pruning as the matrix path; per-channel
        // items alone keep every channel and go through untouched).
        let dead: Vec<bool> = (0..channels)
            .map(|c| items.iter().any(|item| item.entry(c, c).is_none()))
            .collect();
        let any_dead = dead.iter().any(|&d| d);
        // A diagonal matrix item (and, with a dead channel, every item) as
        // per-channel IRs, its entry gains folded in.
        let diag: Vec<Option<Vec<Vec<f64>>>> = items
            .iter()
            .map(|item| match item {
                ConvItem::Channels { .. } if !any_dead => None,
                _ => Some(
                    (0..channels)
                        .map(|c| match item.entry(c, c) {
                            Some((ir, g)) if !dead[c] => ir.iter().map(|&s| s * g).collect(),
                            _ => vec![0.0],
                        })
                        .collect(),
                ),
            })
            .collect();
        let pairs: Vec<(&[Vec<f64>], f64)> = items
            .iter()
            .zip(&diag)
            .map(|(item, d)| match (item, d) {
                (_, Some(d)) => (d.as_slice(), item.gain_db()),
                (ConvItem::Channels { irs, gain_db }, None) => (*irs, *gain_db),
                (ConvItem::Matrix { .. }, None) => unreachable!("built above"),
            })
            .collect();
        return combine_irs(&pairs, channels).map(ConvIrs::Channels);
    }

    let total_gain: f64 = items.iter().map(|i| db_to_lin(i.gain_db())).product();

    // Entry lengths of the product, checked before any FFT, and the
    // entries on a route from an input to an output: the rest never
    // reaches the result (and may be longer than its transform).
    let c = channels;
    let shapes: Vec<Vec<Option<usize>>> = items.iter().map(|it| it.shape(c)).collect();
    let lens = product_shape(&shapes, c);
    let live = live_routes(&shapes, c);
    let entry = |k: usize, out: usize, input: usize| {
        live[k][out * c + input]
            .then(|| items[k].entry(out, input))
            .flatten()
    };
    let max_len = lens.iter().flatten().copied().max().unwrap_or(0);
    if max_len > MAX_COMBINED_IR_TAPS {
        return Err(ChainError::IrTooLong {
            taps: max_len,
            max: MAX_COMBINED_IR_TAPS,
        });
    }

    let mut out = FilterMatrix::new(c);
    if items.len() == 1 {
        for j in 0..c {
            for i in 0..c {
                if let Some((ir, g)) = items[0].entry(j, i) {
                    let g = g * total_gain;
                    out.set(j, i, ir.iter().map(|&s| s * g).collect());
                }
            }
        }
    } else {
        // One input column at a time: v = M_k · … · M_1 e_i, as spectra.
        let fft_len = max_len.max(2).next_power_of_two();
        let mut fft = RealFftPair::new(fft_len);
        let spec_len = fft.spec_len();
        let mut tmp = SplitSpectrum::new(spec_len);
        let mut time = vec![0.0; fft_len];
        let scale = total_gain / fft_len as f64;
        for i in 0..c {
            let mut v: Vec<Option<SplitSpectrum>> = (0..c)
                .map(|m| {
                    entry(0, m, i).map(|(ir, g)| {
                        let mut s = SplitSpectrum::new(spec_len);
                        fft.forward_padded_into_split(ir, g, &mut s);
                        s
                    })
                })
                .collect();
            for k in 1..items.len() {
                let mut next: Vec<Option<SplitSpectrum>> = vec![None; c];
                for (j, slot) in next.iter_mut().enumerate() {
                    for (m, vm) in v.iter().enumerate() {
                        let (Some((b, g)), Some(vm)) = (entry(k, j, m), vm) else {
                            continue;
                        };
                        fft.forward_padded_into_split(b, g, &mut tmp);
                        let acc = slot.get_or_insert_with(|| SplitSpectrum::new(spec_len));
                        complex_mac_split(acc, &tmp, vm);
                    }
                }
                v = next;
            }
            for (j, s) in v.iter().enumerate() {
                if let (Some(s), Some(len)) = (s, lens[j * c + i]) {
                    fft.inverse_from_split(s, &mut time);
                    out.set(j, i, time[..len].iter().map(|&x| x * scale).collect());
                }
            }
        }
    }

    Ok(if out.is_diagonal() {
        ConvIrs::Channels(out.diagonal())
    } else {
        ConvIrs::Matrix(out)
    })
}

/// The entry lengths of the product of `shapes` (each item's entry
/// lengths `[out][in]`, `None`: absent; `shapes[0]` first), as
/// [`combine_conv`] combines them.
pub fn product_shape(shapes: &[Vec<Option<usize>>], c: usize) -> Vec<Option<usize>> {
    let Some((first, rest)) = shapes.split_first() else {
        return vec![None; c * c];
    };
    let mut lens = first.clone();
    for item in rest {
        let mut next = vec![None; c * c];
        for j in 0..c {
            for i in 0..c {
                for m in 0..c {
                    if let (Some(b), Some(a)) = (item[j * c + m], lens[m * c + i]) {
                        let len = b.saturating_add(a) - 1;
                        let slot: &mut Option<usize> = &mut next[j * c + i];
                        *slot = Some(slot.map_or(len, |l| l.max(len)));
                    }
                }
            }
        }
        lens = next;
    }
    lens
}

/// Which entries of each item lie on a route from an input through every
/// item to an output (`[item][out * c + in]`): an entry whose input no
/// earlier route reaches, or whose output no later entry reads, never
/// reaches the product.
fn live_routes(shapes: &[Vec<Option<usize>>], c: usize) -> Vec<Vec<bool>> {
    let n = shapes.len();
    // reach[k]: channels a route reaches at item k's input.
    let mut reach = vec![vec![true; c]; n + 1];
    for k in 0..n {
        for j in 0..c {
            reach[k + 1][j] = (0..c).any(|m| reach[k][m] && shapes[k][j * c + m].is_some());
        }
    }
    // read[k]: channels at item k's input that a route carries to an output.
    let mut read = vec![vec![true; c]; n + 1];
    for k in (0..n).rev() {
        for m in 0..c {
            read[k][m] = (0..c).any(|j| read[k + 1][j] && shapes[k][j * c + m].is_some());
        }
    }
    (0..n)
        .map(|k| {
            (0..c * c)
                .map(|e| {
                    let (j, m) = (e / c, e % c);
                    shapes[k][e].is_some() && reach[k][m] && read[k + 1][j]
                })
                .collect()
        })
        .collect()
}

/// What [`combine_conv`] checks, without combining: the items' shapes and
/// [`MAX_COMBINED_IR_TAPS`] on every combined entry. Returns the longest
/// combined entry, taps.
pub fn check_conv(items: &[ConvItem<'_>], channels: usize) -> Result<usize, ChainError> {
    if channels == 0 {
        return Err(ChainError::ZeroChannels);
    }
    if items.is_empty() {
        return Err(ChainError::MissingIr);
    }
    for item in items {
        item.validate(channels)?;
    }
    let shapes: Vec<Vec<Option<usize>>> = items.iter().map(|it| it.shape(channels)).collect();
    let max_len = product_shape(&shapes, channels)
        .into_iter()
        .flatten()
        .max()
        .unwrap_or(0);
    if max_len > MAX_COMBINED_IR_TAPS {
        return Err(ChainError::IrTooLong {
            taps: max_len,
            max: MAX_COMBINED_IR_TAPS,
        });
    }
    Ok(max_len)
}

// ---------------------------------------------------------------- loading

/// A matrix entry with its IR loaded.
#[derive(Clone, Debug, PartialEq)]
pub struct LoadedTap {
    pub input: usize,
    pub output: usize,
    pub ir: Vec<f64>,
    pub gain_db: f64,
}

/// An enabled convolution item with its IRs loaded at the chain's rate
/// ([`load_conv`]); [`Self::as_item`] hands it to [`combine_conv`].
#[derive(Clone, Debug, PartialEq)]
pub enum LoadedConv {
    Channels { irs: Vec<Vec<f64>>, gain_db: f64 },
    Matrix { taps: Vec<LoadedTap>, gain_db: f64 },
}

impl LoadedConv {
    pub fn as_item(&self) -> ConvItem<'_> {
        match self {
            Self::Channels { irs, gain_db } => ConvItem::Channels {
                irs,
                gain_db: *gain_db,
            },
            Self::Matrix { taps, gain_db } => ConvItem::Matrix {
                taps: taps
                    .iter()
                    .map(|t| MatrixTap {
                        input: t.input,
                        output: t.output,
                        ir: &t.ir,
                        gain_db: t.gain_db,
                    })
                    .collect(),
                gain_db: *gain_db,
            },
        }
    }
}

/// The shape rules of a convolution item, the daemon's config check and
/// [`load_conv`] alike: a `matrix` item has no `ir_path` beside it, at
/// least one entry, `in`/`out` below `channels`, each pair once and finite
/// gains. The error goes after the item's field name (`" has both …"`,
/// `".matrix[1]: …"`).
pub fn check_item_shape(
    conv: &audivane_protocol::ConvolutionConfig,
    channels: usize,
) -> Result<(), String> {
    let Some(matrix) = &conv.matrix else {
        return Ok(());
    };
    if !conv.ir_path.as_os_str().is_empty() {
        return Err(" has both ir_path and matrix; give one".into());
    }
    if matrix.is_empty() {
        return Err(".matrix must have at least one entry".into());
    }
    for (k, e) in matrix.iter().enumerate() {
        if e.input >= channels || e.output >= channels {
            return Err(format!(
                ".matrix[{k}]: in {} / out {} must be below {channels} (the chain's channels)",
                e.input, e.output
            ));
        }
        if matrix[..k]
            .iter()
            .any(|d| d.input == e.input && d.output == e.output)
        {
            return Err(format!(
                ".matrix[{k}]: in {} -> out {} is given twice",
                e.input, e.output
            ));
        }
        if !e.gain_db.is_finite() {
            return Err(format!(".matrix[{k}].gain_db must be finite"));
        }
    }
    Ok(())
}

/// Reads one WAV for [`load_conv`]: its channels at the chain's rate, or a
/// message.
pub type IrLoader<'a> = dyn FnMut(&std::path::Path) -> Result<Vec<Vec<f64>>, String> + 'a;

/// Load the enabled items of `convs` for a chain of `channels` channels,
/// `load` reading one WAV (path → its channels at the chain's rate; the
/// caller does the I/O, resampling and caching). Every enabled item's
/// shape is checked first ([`check_item_shape`]). An `ir_path` item needs
/// 1 or `channels` channels; a matrix entry takes its `channel` of the
/// file, or a mono file when it names none.
pub fn load_conv(
    convs: &[audivane_protocol::ConvolutionConfig],
    channels: usize,
    load: &mut IrLoader<'_>,
) -> Result<Vec<LoadedConv>, String> {
    for (i, conv) in convs.iter().enumerate().filter(|(_, c)| c.enabled) {
        check_item_shape(conv, channels).map_err(|e| format!("convolution[{i}]{e}"))?;
    }
    let mut out = Vec::new();
    for conv in convs.iter().filter(|c| c.enabled) {
        match &conv.matrix {
            None => {
                let irs = load(&conv.ir_path)?;
                if irs.len() != 1 && irs.len() != channels {
                    return Err(format!(
                        "IR '{}' has {} channels; expected 1 (mono) or {channels}",
                        conv.ir_path.display(),
                        irs.len(),
                    ));
                }
                out.push(LoadedConv::Channels {
                    irs,
                    gain_db: conv.gain_db,
                });
            }
            Some(entries) => {
                let mut taps = Vec::with_capacity(entries.len());
                for e in entries {
                    let mut irs = load(&e.ir_path)?;
                    let ch = match e.channel {
                        Some(ch) if ch < irs.len() => ch,
                        Some(ch) => {
                            return Err(format!(
                                "IR '{}' has {} channels; matrix entry {} -> {} asks for channel {ch}",
                                e.ir_path.display(),
                                irs.len(),
                                e.input,
                                e.output,
                            ));
                        }
                        None if irs.len() == 1 => 0,
                        None => {
                            return Err(format!(
                                "IR '{}' has {} channels; matrix entry {} -> {} needs a mono file or a `channel`",
                                e.ir_path.display(),
                                irs.len(),
                                e.input,
                                e.output,
                            ));
                        }
                    };
                    taps.push(LoadedTap {
                        input: e.input,
                        output: e.output,
                        ir: irs.swap_remove(ch),
                        gain_db: e.gain_db,
                    });
                }
                out.push(LoadedConv::Matrix {
                    taps,
                    gain_db: conv.gain_db,
                });
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------- convolver

struct Path {
    input: usize,
    output: usize,
    conv: Convolver,
}

/// Runs a [`FilterMatrix`]: one single-channel [`Convolver`] per present
/// entry, its outputs summed into each output channel. Real-time safe
/// (scratch allocated at build).
pub struct MatrixConvolver {
    channels: usize,
    max_block: usize,
    paths: Vec<Path>,
    /// The input copy each path convolves in place.
    scratch: Vec<f64>,
    /// One sum per output channel.
    acc: Vec<Vec<f64>>,
    latency: usize,
    ir_len: usize,
}

impl MatrixConvolver {
    /// `max_block`: the largest frame count `process` will receive.
    pub fn new(matrix: &FilterMatrix, max_block: usize) -> Self {
        let paths: Vec<Path> = matrix
            .entries()
            .map(|(output, input, ir)| Path {
                input,
                output,
                conv: Convolver::new(&[ir.to_vec()], 1, max_block),
            })
            .collect();
        // The base block depends on `max_block` alone: every path has the
        // same latency.
        let latency = paths.first().map_or(0, |p| p.conv.latency_frames());
        debug_assert!(paths.iter().all(|p| p.conv.latency_frames() == latency));
        let block = max_block.max(1);
        Self {
            channels: matrix.channels(),
            max_block: block,
            paths,
            scratch: vec![0.0; block],
            acc: vec![vec![0.0; block]; matrix.channels()],
            latency,
            ir_len: matrix.max_len(),
        }
    }

    /// Frames the output still rings after its delay once the input ends
    /// (the longest entry less one).
    pub fn ir_tail_frames(&self) -> usize {
        self.ir_len.saturating_sub(1)
    }

    /// Bytes [`Self::inherit_state`] copies (every entry's convolver).
    pub fn state_bytes(&self) -> usize {
        self.paths.iter().map(|p| p.conv.state_bytes()).sum()
    }

    /// Whether `other` runs the same matrix with the same buffers (every
    /// entry's filter in the same place: [`Convolver::can_inherit`]).
    pub fn same_filter(&self, other: &MatrixConvolver) -> bool {
        self.channels == other.channels
            && self.max_block == other.max_block
            && self.paths.len() == other.paths.len()
            && self.paths.iter().zip(&other.paths).all(|(d, s)| {
                d.input == s.input && d.output == s.output && d.conv.can_inherit(&s.conv)
            })
    }

    /// Continue from `from`'s state when it runs the same matrix (every
    /// entry's filter, in the same places: [`Convolver::can_inherit`]);
    /// otherwise nothing changes and this returns `false`. See
    /// [`Convolver::inherit_state`]. Real-time safe.
    pub fn inherit_state(&mut self, from: &MatrixConvolver) -> bool {
        if !self.same_filter(from) {
            return false;
        }
        for (d, s) in self.paths.iter_mut().zip(&from.paths) {
            d.conv.inherit_state(&s.conv);
        }
        true
    }
}

impl Stage for MatrixConvolver {
    fn process(&mut self, chans: &mut [&mut [f64]]) {
        assert_eq!(chans.len(), self.channels, "channel count mismatch");
        let n = chans.first().map_or(0, |c| c.len());
        assert!(
            n <= self.max_block,
            "frame count {n} exceeds max_block {}",
            self.max_block
        );
        if n == 0 {
            return;
        }
        for acc in &mut self.acc {
            acc[..n].fill(0.0);
        }
        let x = &mut self.scratch[..n];
        for p in &mut self.paths {
            x.copy_from_slice(&chans[p.input][..n]);
            p.conv.process(&mut [&mut *x]);
            for (a, &v) in self.acc[p.output][..n].iter_mut().zip(x.iter()) {
                *a += v;
            }
        }
        for (ch, acc) in chans.iter_mut().zip(&self.acc) {
            ch[..n].copy_from_slice(&acc[..n]);
        }
    }

    fn latency_frames(&self) -> usize {
        self.latency
    }

    fn reset(&mut self) {
        for p in &mut self.paths {
            p.conv.reset();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lcg(seed: &mut u64) -> f64 {
        *seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((*seed >> 11) as f64) / ((1u64 << 53) as f64) * 2.0 - 1.0
    }

    fn noise(n: usize, seed: u64) -> Vec<f64> {
        let mut s = seed;
        (0..n).map(|_| lcg(&mut s)).collect()
    }

    /// Full linear convolution, direct.
    fn convolve(a: &[f64], b: &[f64]) -> Vec<f64> {
        let mut y = vec![0.0; a.len() + b.len() - 1];
        for (i, &x) in a.iter().enumerate() {
            for (k, &h) in b.iter().enumerate() {
                y[i + k] += x * h;
            }
        }
        y
    }

    fn add_into(acc: &mut Vec<f64>, x: &[f64]) {
        if acc.len() < x.len() {
            acc.resize(x.len(), 0.0);
        }
        for (a, &v) in acc.iter_mut().zip(x) {
            *a += v;
        }
    }

    /// Run `stage` over `inputs` in uneven blocks (at most `max_block`).
    fn run(stage: &mut dyn Stage, inputs: &[Vec<f64>], max_block: usize) -> Vec<Vec<f64>> {
        let n = inputs[0].len();
        let mut out: Vec<Vec<f64>> = inputs.to_vec();
        let mut pos = 0;
        let mut k = 0;
        while pos < n {
            let len = [max_block, 1, 37, max_block / 2, 0][k % 5].min(n - pos);
            k += 1;
            let mut chans: Vec<&mut [f64]> =
                out.iter_mut().map(|c| &mut c[pos..pos + len]).collect();
            stage.process(&mut chans);
            pos += len;
        }
        out
    }

    fn assert_close(a: &[f64], b: &[f64], tol: f64, what: &str) {
        assert_eq!(a.len(), b.len(), "{what}: lengths");
        let err = a
            .iter()
            .zip(b)
            .map(|(x, y)| (x - y).abs())
            .fold(0.0, f64::max);
        assert!(err < tol, "{what}: max error {err:e}");
    }

    fn tap(input: usize, output: usize, ir: &[f64], gain_db: f64) -> MatrixTap<'_> {
        MatrixTap {
            input,
            output,
            ir,
            gain_db,
        }
    }

    #[test]
    fn diagonal_matrix_is_bit_identical_to_a_stereo_ir() {
        let l = noise(3000, 1);
        let r = noise(2500, 2);
        let stereo = vec![l.clone(), r.clone()];
        let per_channel = combine_conv(
            &[ConvItem::Channels {
                irs: &stereo,
                gain_db: -2.0,
            }],
            2,
        )
        .unwrap();
        let matrix = combine_conv(
            &[ConvItem::Matrix {
                taps: vec![
                    MatrixTap {
                        input: 0,
                        output: 0,
                        ir: &l,
                        gain_db: 0.0,
                    },
                    MatrixTap {
                        input: 1,
                        output: 1,
                        ir: &r,
                        gain_db: 0.0,
                    },
                ],
                gain_db: -2.0,
            }],
            2,
        )
        .unwrap();
        assert_eq!(per_channel, matrix);
        assert!(matches!(matrix, ConvIrs::Channels(_)));
        // In series with another item too.
        let mono = vec![noise(400, 3)];
        let a = combine_conv(
            &[
                ConvItem::Channels {
                    irs: &stereo,
                    gain_db: 0.0,
                },
                ConvItem::Channels {
                    irs: &mono,
                    gain_db: 1.0,
                },
            ],
            2,
        )
        .unwrap();
        let b = combine_conv(
            &[
                ConvItem::Matrix {
                    taps: vec![
                        MatrixTap {
                            input: 1,
                            output: 1,
                            ir: &r,
                            gain_db: 0.0,
                        },
                        MatrixTap {
                            input: 0,
                            output: 0,
                            ir: &l,
                            gain_db: 0.0,
                        },
                    ],
                    gain_db: 0.0,
                },
                ConvItem::Channels {
                    irs: &mono,
                    gain_db: 1.0,
                },
            ],
            2,
        )
        .unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn sum_to_one_output_and_full_matrix_match_direct_reference() {
        let max_block = 256;
        let x = [noise(20_000, 10), noise(20_000, 11)];
        let f: Vec<Vec<f64>> = (0..4)
            .map(|k| noise(1500 + 700 * k, 20 + k as u64))
            .collect();

        // 2 -> 1: out 0 = F00*L + F01*R, out 1 silent.
        let taps = vec![
            MatrixTap {
                input: 0,
                output: 0,
                ir: &f[0],
                gain_db: 0.0,
            },
            MatrixTap {
                input: 1,
                output: 0,
                ir: &f[1],
                gain_db: -6.0,
            },
        ];
        let ConvIrs::Matrix(m) =
            combine_conv(&[ConvItem::Matrix { taps, gain_db: 0.0 }], 2).unwrap()
        else {
            panic!("a cross term makes a matrix");
        };
        let mut conv = MatrixConvolver::new(&m, max_block);
        let y = run(&mut conv, &x, max_block);
        let lat = conv.latency_frames();
        let g = db_to_lin(-6.0);
        let mut want = convolve(&x[0], &f[0]);
        let r: Vec<f64> = convolve(&x[1], &f[1]).iter().map(|v| v * g).collect();
        add_into(&mut want, &r);
        assert_close(&y[0][lat..], &want[..x[0].len() - lat], 1e-9, "2->1 output");
        assert!(y[1].iter().all(|&v| v == 0.0), "the absent row is silent");

        // Full 2x2 with cross terms.
        let taps: Vec<MatrixTap> = (0..4)
            .map(|k| MatrixTap {
                input: k % 2,
                output: k / 2,
                ir: &f[k],
                gain_db: 0.0,
            })
            .collect();
        let ConvIrs::Matrix(m) =
            combine_conv(&[ConvItem::Matrix { taps, gain_db: 3.0 }], 2).unwrap()
        else {
            panic!("a full matrix");
        };
        let mut conv = MatrixConvolver::new(&m, max_block);
        let y = run(&mut conv, &x, max_block);
        let g = db_to_lin(3.0);
        for j in 0..2 {
            let mut want = convolve(&x[0], &f[2 * j]);
            add_into(&mut want, &convolve(&x[1], &f[2 * j + 1]));
            let want: Vec<f64> = want.iter().map(|v| v * g).collect();
            assert_close(
                &y[j][lat..],
                &want[..x[0].len() - lat],
                1e-9,
                &format!("2x2 output {j}"),
            );
        }
    }

    #[test]
    fn two_matrix_items_in_series_equal_their_product() {
        let a: Vec<Vec<f64>> = (0..4).map(|k| noise(200 + 50 * k, 40 + k as u64)).collect();
        let b: Vec<Vec<f64>> = (0..3).map(|k| noise(300 + 90 * k, 50 + k as u64)).collect();
        let first = ConvItem::Matrix {
            taps: vec![
                tap(0, 0, &a[0], 0.0),
                tap(1, 0, &a[1], -3.0),
                tap(0, 1, &a[2], 0.0),
                tap(1, 1, &a[3], 1.0),
            ],
            gain_db: -1.0,
        };
        // Second: out 0 = B0*m0 + B1*m1, out 1 = B2*m1.
        let second = ConvItem::Matrix {
            taps: vec![
                tap(0, 0, &b[0], 0.0),
                tap(1, 0, &b[1], 0.0),
                tap(1, 1, &b[2], 2.0),
            ],
            gain_db: 0.5,
        };
        let ConvIrs::Matrix(p) = combine_conv(&[first, second], 2).unwrap() else {
            panic!("cross terms");
        };
        let lin = db_to_lin;
        let g = lin(-1.0) * lin(0.5);
        let a_e = |o: usize, i: usize, gain: f64| -> Vec<f64> {
            a[o * 2 + i].iter().map(|v| v * lin(gain)).collect()
        };
        // a entries indexed as [out*2+in]: a[0]=0->0, a[1]=1->0, a[2]=0->1, a[3]=1->1.
        let a00 = a_e(0, 0, 0.0);
        let a01 = a_e(0, 1, -3.0);
        let a10 = a_e(1, 0, 0.0);
        let a11 = a_e(1, 1, 1.0);
        let b11: Vec<f64> = b[2].iter().map(|v| v * lin(2.0)).collect();
        let scale = |v: Vec<f64>| -> Vec<f64> { v.iter().map(|x| x * g).collect() };
        let mut p00 = convolve(&b[0], &a00);
        add_into(&mut p00, &convolve(&b[1], &a10));
        let mut p01 = convolve(&b[0], &a01);
        add_into(&mut p01, &convolve(&b[1], &a11));
        let p10 = convolve(&b11, &a10);
        let p11 = convolve(&b11, &a11);
        for ((o, i), want) in [
            ((0, 0), scale(p00)),
            ((0, 1), scale(p01)),
            ((1, 0), scale(p10)),
            ((1, 1), scale(p11)),
        ] {
            let got = p.get(o, i).expect("present");
            assert_close(got, &want, 1e-12, &format!("entry {i}->{o}"));
        }

        // A per-channel item in series is the diagonal matrix of its IRs.
        let stereo = vec![noise(100, 60), noise(120, 61)];
        let ConvIrs::Matrix(q) = combine_conv(
            &[
                ConvItem::Channels {
                    irs: &stereo,
                    gain_db: 0.0,
                },
                ConvItem::Matrix {
                    taps: vec![tap(1, 0, &b[1], 0.0)],
                    gain_db: 0.0,
                },
            ],
            2,
        )
        .unwrap() else {
            panic!("cross term");
        };
        assert_eq!(q.entries().count(), 1);
        assert_close(
            q.get(0, 1).unwrap(),
            &convolve(&b[1], &stereo[1]),
            1e-12,
            "diag then cross",
        );
    }
}
