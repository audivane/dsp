//! Design limits.
//!
//! Three separate budgets, all checked before anything is designed
//! ([`super::feasibility`]), the measured ones again once the kernel
//! exists ([`super::prepare`]):
//!
//! - **Construction workspace**: what designing the kernel allocates for
//!   a moment (prototype, phase-transform buffers, the automatic search's
//!   candidates and measurements, the safety gate's FFTs), at most the
//!   context's cap ([`super::BuildContext::workspace`]): a quarter of the
//!   machine's RAM within [`DESIGN_WORKSPACE_FLOOR`] ..=
//!   [`DESIGN_WORKSPACE_CEILING`] ([`design_workspace`]). The formulas are
//!   upper bounds calibrated on
//!   measured peaks; the cepstral transform is also capped by its FFT
//!   ([`MAX_CEPSTRAL_FFT_LEN`]).
//! - **Resident memory**: what the runtime keeps (kernel spectra or
//!   coefficients, FIFOs, block buffers), reported in the [`Plan`]. The
//!   tap limits bound it; it has no limit of its own.
//! - **Callback work**: at a rational ratio the direct sum may cost at
//!   most [`MAX_RATIONAL_MACS_PER_SECOND`] per channel over every pipeline
//!   that can run at once, with at most [`MAX_RATIONAL_GRID_TAPS`] taps; at
//!   an integer ratio a kernel has at most [`MAX_KERNEL_TAPS`] taps
//!   (intermediate phase a quarter of that). [`Plan::callback`] is the
//!   work the admission check prices on this machine.
//!
//! Every refusal is a [`DesignError::Budget`] carrying a [`Budget`], whose
//! message is the one each limit has always had.

use crate::design::{DesignError, Grid};
use audivane_protocol::RateRatio;

/// Longest kernel at an integer ratio, in output-rate taps: 8 Mi. The
/// longest preset kernel is `lanczos65535` at x64 (8 388 544 taps). A
/// design within it fits the largest workspace cap
/// ([`DESIGN_WORKSPACE_CEILING`]) as a linear kernel and as a reflected
/// minimum phase; a smaller cap refuses the long reflected ones.
pub const MAX_KERNEL_TAPS: usize = 1 << 23;

/// Shortest kernel.
pub const MIN_TAPS: usize = 1;

/// Longest intermediate-phase design blended between the two phases (a
/// mix strictly between 0 and 1): its reflected kernel can grow
/// [`crate::minphase::INTERMEDIATE_MAX_GROWTH`] times, so a quarter of
/// [`MAX_KERNEL_TAPS`].
pub fn max_intermediate_taps() -> usize {
    (MAX_KERNEL_TAPS as f64 / crate::minphase::INTERMEDIATE_MAX_GROWTH) as usize
}

/// Largest integer ratio.
pub const MAX_RATIO: usize = audivane_protocol::MAX_INTEGER_RATIO as usize;

/// Largest FFT of the cepstral phase transform, `2^(bitlen(N) + 5)` for an
/// N-tap prototype (N below 2^21): about 3.1 GB charged, within
/// [`DESIGN_WORKSPACE_CEILING`]. The largest preset case needs half of it
/// (`kaiser_steep_mp` at 1280/147: 2^25 points); the cap matters to the
/// offline export, which has no playback budget.
pub const MAX_CEPSTRAL_FFT_LEN: usize = 1 << 26;

/// The smallest design workspace cap (the most memory one design may
/// allocate while it is built: prototype, transform buffers, search
/// candidates, safety FFTs): 1.5 GiB, on any machine. The largest preset
/// construction peaks at 1.35 GB (the cepstral transform of
/// `kaiser_steep_mp` at 1280/147), so every preset builds everywhere.
pub const DESIGN_WORKSPACE_FLOOR: u64 = 3 << 29;

/// The largest design workspace cap: 4 GiB.
pub const DESIGN_WORKSPACE_CEILING: u64 = 4 << 30;

/// The machine's design workspace cap for `total_ram` bytes of RAM (its
/// total, not what is free, so the cap and the rate masks stay the same for
/// a daemon's lifetime): a quarter of it, within [`DESIGN_WORKSPACE_FLOOR`]
/// ..= [`DESIGN_WORKSPACE_CEILING`]; the floor when the RAM is unknown.
pub fn design_workspace_for_ram(total_ram: Option<u64>) -> u64 {
    total_ram.map_or(DESIGN_WORKSPACE_FLOOR, |ram| {
        (ram / 4).clamp(DESIGN_WORKSPACE_FLOOR, DESIGN_WORKSPACE_CEILING)
    })
}

static MACHINE_WORKSPACE: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

/// Set this process's design workspace cap ([`design_workspace`]), once,
/// before anything is designed (the daemon does it at start-up, from
/// [`design_workspace_for_ram`]). Returns the cap in effect: `bytes`, or
/// the one set before.
pub fn set_design_workspace(bytes: u64) -> u64 {
    *MACHINE_WORKSPACE.get_or_init(|| bytes)
}

/// This process's design workspace cap: what [`set_design_workspace`] set,
/// else [`DESIGN_WORKSPACE_FLOOR`] (tests, tools). Production contexts
/// carry it ([`super::BuildContext::workspace`]); the automatic search,
/// cached per process, holds its candidates to it.
pub fn design_workspace() -> u64 {
    MACHINE_WORKSPACE
        .get()
        .copied()
        .unwrap_or(DESIGN_WORKSPACE_FLOOR)
}

/// The most multiply-adds per second and channel the rational runtimes of
/// one configuration may cost together, counting every pipeline that can
/// run at once ([`RATIONAL_SWAP_PIPELINES`], twice that with A/B). With
/// kernels within [`MAX_RATIONAL_KERNEL_BYTES`] the runtime does 7 to 9 G
/// multiply-adds per second on one core, so this is about a fifth of a core
/// for stereo at the worst moment (an A/B swap).
pub const MAX_RATIONAL_MACS_PER_SECOND: f64 = 800.0e6;

/// Rational pipelines that run at once for one chain: during a swap or
/// crossfade the old and the new chain both run. An A/B pair doubles it.
pub const RATIONAL_SWAP_PIPELINES: usize = 2;

/// Largest rational kernel, in bytes of f64 coefficients (1 Mi taps). Past
/// the caches the runtime is bound by memory bandwidth, so the limit keeps
/// every kernel's sweep cache-friendly.
pub const MAX_RATIONAL_KERNEL_BYTES: usize = 8 << 20;

/// Longest kernel on the grid a rational runtime may use
/// ([`MAX_RATIONAL_KERNEL_BYTES`] of f64 taps).
pub const MAX_RATIONAL_GRID_TAPS: usize = MAX_RATIONAL_KERNEL_BYTES / size_of::<f64>();

/// Which limit a design is over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Limit {
    /// A rational kernel longer than [`MAX_RATIONAL_GRID_TAPS`].
    RationalTaps,
    /// A rational kernel over [`MAX_RATIONAL_MACS_PER_SECOND`].
    RationalCost,
    /// A kernel at an integer ratio outside [`MIN_TAPS`] ..=
    /// [`MAX_KERNEL_TAPS`] (a quarter of it for intermediate phase).
    KernelTaps,
    /// A cepstral transform over [`MAX_CEPSTRAL_FFT_LEN`].
    CepstralFft,
    /// A construction over the design workspace cap
    /// ([`super::BuildContext::workspace`]).
    Workspace,
}

/// A refusal by one of the limits, with its message.
#[derive(Clone, Debug, PartialEq)]
pub struct Budget {
    pub limit: Limit,
    pub message: String,
}

impl std::fmt::Display for Budget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

fn refuse(limit: Limit, message: String) -> DesignError {
    DesignError::Budget(Budget { limit, message })
}

/// Multiply-adds per second and channel of a direct polyphase rational
/// runtime with a `grid_taps`-tap kernel at `ratio` from `source_rate`:
/// output rate times taps per branch.
pub fn rational_macs_per_second(grid_taps: usize, ratio: RateRatio, source_rate: u32) -> f64 {
    let out_rate = f64::from(source_rate) * ratio.as_f64();
    out_rate * grid_taps.div_ceil(ratio.up.max(1) as usize) as f64
}

/// Refuse a rational kernel that is too long (more than `max_grid_taps` or
/// [`MAX_RATIONAL_GRID_TAPS`]) or too expensive when `pipelines` copies of
/// it can run at once ([`MAX_RATIONAL_MACS_PER_SECOND`]).
pub fn check_rational(
    grid_taps: usize,
    ratio: RateRatio,
    source_rate: u32,
    max_grid_taps: usize,
    pipelines: usize,
) -> Result<(), DesignError> {
    let max_taps = max_grid_taps.min(MAX_RATIONAL_GRID_TAPS);
    if grid_taps > max_taps {
        return Err(refuse(
            Limit::RationalTaps,
            format!(
                "the {ratio} kernel needs {grid_taps} taps ({:.1} MB), more than {max_taps} \
                 ({} MB)",
                (grid_taps * size_of::<f64>()) as f64 / 1e6,
                MAX_RATIONAL_KERNEL_BYTES >> 20
            ),
        ));
    }
    let one = rational_macs_per_second(grid_taps, ratio, source_rate);
    let pipelines = pipelines.max(1);
    let macs = one * pipelines as f64;
    if macs > MAX_RATIONAL_MACS_PER_SECOND {
        return Err(refuse(
            Limit::RationalCost,
            format!(
                "the {ratio} kernel ({} taps per output frame at {:.0} Hz) needs {:.0} M \
                 multiply-adds per second per channel, {:.0} M with the {pipelines} pipelines \
                 that can run at once (a swap or crossfade runs two, A/B doubles it), more than \
                 the {:.0} M budget",
                grid_taps.div_ceil(ratio.up.max(1) as usize),
                f64::from(source_rate) * ratio.as_f64(),
                one / 1e6,
                macs / 1e6,
                MAX_RATIONAL_MACS_PER_SECOND / 1e6
            ),
        ));
    }
    Ok(())
}

/// Refuse an integer-ratio kernel of `taps` design taps (`intermediate`: a
/// reflected intermediate phase, which can grow
/// [`crate::minphase::INTERMEDIATE_MAX_GROWTH`] times).
pub fn check_kernel_taps(
    taps: usize,
    ratio: RateRatio,
    intermediate: bool,
) -> Result<(), DesignError> {
    let max = if intermediate {
        max_intermediate_taps()
    } else {
        MAX_KERNEL_TAPS
    };
    if (MIN_TAPS..=max).contains(&taps) {
        return Ok(());
    }
    Err(refuse(
        Limit::KernelTaps,
        if intermediate {
            format!(
                "the {ratio} kernel needs {taps} taps, outside {MIN_TAPS} to {max} for \
                 intermediate phase (it can grow to {} times the design length)",
                crate::minphase::INTERMEDIATE_MAX_GROWTH
            )
        } else {
            format!("the {ratio} kernel needs {taps} taps, outside {MIN_TAPS} to {max}")
        },
    ))
}

/// Refuse a cepstral transform of an `n`-tap prototype over
/// [`MAX_CEPSTRAL_FFT_LEN`].
pub fn check_cepstral(n: usize, ratio: RateRatio) -> Result<(), DesignError> {
    let m = crate::design::phase::cepstral::fft_len(n);
    if m <= MAX_CEPSTRAL_FFT_LEN {
        return Ok(());
    }
    Err(refuse(
        Limit::CepstralFft,
        format!(
            "the {ratio} kernel's phase transform needs a {m}-point FFT ({n} taps), more than \
             {MAX_CEPSTRAL_FFT_LEN}: use a lower ratio or a shorter design"
        ),
    ))
}

/// Refuse a construction that needs more than `limit` bytes (the design
/// workspace cap, [`super::BuildContext::workspace`]).
pub fn check_workspace(bytes: u64, limit: u64, what: &str) -> Result<(), DesignError> {
    if bytes <= limit {
        return Ok(());
    }
    Err(refuse(
        Limit::Workspace,
        format!(
            "designing {what} needs about {:.2} GB of memory, more than the {:.2} GB design \
             limit: use a shorter filter or a lower ratio",
            bytes as f64 / 1e9,
            limit as f64 / 1e9
        ),
    ))
}

// ------------------------------------------------------ workspace formulas

/// Plans, twiddles and small buffers beside the per-tap terms.
const SLACK: u64 = 16 << 20;

/// A linear kernel of `n` taps on any path: window, prototype, kernel and
/// the normalisation's copies (measured 16 to 48 bytes per tap).
pub fn linear_workspace(n: usize) -> u64 {
    64 * n as u64 + SLACK
}

/// The cepstral transform of an `n`-tap prototype: double-double
/// magnitudes, f64 spectra and plans over [`MAX_CEPSTRAL_FFT_LEN`]-bounded
/// `fft_len(n)` points (measured at most 40.2 bytes per point), plus the
/// prototype and the up to `4n`-tap result.
pub fn cepstral_workspace(n: usize) -> u64 {
    44 * crate::design::phase::cepstral::fft_len(n) as u64 + linear_workspace(n)
}

/// A reflected (checked zero reflection) minimum or intermediate phase of
/// an `n`-tap kernel: the cepstral grids of [`crate::minphase`] over
/// `fft_len_for(n)` points (measured 47 bytes per point for minimum phase,
/// 51.4 for intermediate), plus the kernel copies.
pub fn reflect_workspace(n: usize) -> u64 {
    52 * crate::minphase::fft_len_for(n) as u64 + linear_workspace(n)
}

/// Bytes per point of a real FFT that holds its time and two spectrum
/// buffers and one plan (measured: 16 for the input and spectrum, 12 for
/// the plan).
const FFT_POINT: u64 = 40;

/// The measurement of the automatic search on a `len`-tap kernel (the
/// converted one, for a minimum or intermediate phase): the kernel and one
/// real FFT of `32 len` points (at most
/// [`crate::design::auto::AUTO_GRID_MAX_FFT`] when `capped`, the real-grid
/// check of a rational design, which measures a longer kernel in several
/// shifted FFTs of that size). The search keeps one FFT size's plan at a
/// time.
pub fn measurement_workspace(len: usize, capped: bool) -> u64 {
    let points = len.max(2).saturating_mul(32).next_power_of_two();
    let points = if capped {
        points.min(crate::design::auto::AUTO_GRID_MAX_FFT)
    } else {
        points
    };
    8 * len as u64 + FFT_POINT * points as u64
}

/// The design of one candidate of the automatic search (`reflect`: with
/// the phase conversion). Its measurement, once the kernel is known, is
/// [`measurement_workspace`]: the design's grids are gone by then.
pub fn candidate_workspace(n: usize, reflect: bool) -> u64 {
    if reflect {
        reflect_workspace(n)
    } else {
        linear_workspace(n)
    }
}

/// Bytes per point of a real FFT plan (twiddles and scratch; measured 12).
const PLAN_POINT: u64 = 16;

/// What the plans of one `points`-point real FFT keep.
pub fn plan_workspace(points: usize) -> u64 {
    PLAN_POINT * points as u64
}

/// [`check_workspace`] for designing one candidate of the automatic
/// search while it holds `retained` bytes (the last measurement's plans),
/// as the search's own error; noted as the search's workspace. The search
/// is cached per process, so it holds its candidates to the process's cap
/// ([`design_workspace`]).
pub(crate) fn check_candidate(n: usize, reflect: bool, retained: u64) -> Result<(), String> {
    let bytes = candidate_workspace(n, reflect) + retained;
    crate::design::auto::note_search_workspace(bytes);
    check_workspace(bytes, design_workspace(), &format!("a {n}-tap candidate"))
        .map_err(|e| e.to_string())
}

/// [`check_workspace`] for measuring a `len`-tap candidate kernel
/// ([`measurement_workspace`]), as the search's own error.
pub(crate) fn check_measurement(len: usize, capped: bool) -> Result<(), String> {
    let bytes = measurement_workspace(len, capped);
    crate::design::auto::note_search_workspace(bytes);
    check_workspace(
        bytes,
        design_workspace(),
        &format!("a {len}-tap candidate's measurement"),
    )
    .map_err(|e| e.to_string())
}

/// Largest FFT of the safety gate (per polyphase branch).
pub const MAX_GATE_FFT: usize = 1 << 23;

/// The FFT the safety gate starts with for a branch of `len` taps: 8 points
/// per tap and at least 2^16, at most [`MAX_GATE_FFT`] (and never shorter
/// than the branch).
pub fn gate_fft(len: usize) -> usize {
    (8 * len)
        .max(1 << 16)
        .next_power_of_two()
        .min(MAX_GATE_FFT)
        .max(len.next_power_of_two())
}

/// The budget refusal an automatic search returned as its own error
/// ([`check_candidate`]), if `message` is one.
pub(crate) fn candidate_refusal(message: &str) -> Option<Budget> {
    (message.starts_with("designing a ") && message.contains("GB design limit")).then(|| Budget {
        limit: Limit::Workspace,
        message: message.to_string(),
    })
}

/// The safety gate on an `n`-tap kernel (or response) of `branches`
/// branches: the kernel, one branch, and its FFT, the buffers of one size
/// and the plans of every size it reaches. With `doubling` (a check the
/// gate enforces: it doubles the FFT while only the analytic bound is
/// over +1 dB) up to [`MAX_GATE_FFT`] points, else at [`gate_fft`]'s.
pub fn gate_workspace(n: usize, branches: usize, doubling: bool) -> u64 {
    let branch = n.div_ceil(branches.max(1));
    let start = gate_fft(branch);
    let top = if doubling {
        start.max(MAX_GATE_FFT)
    } else {
        start
    };
    8 * (n + branch) as u64 + FFT_POINT * top as u64
}

/// Output frames of a block FFT runtime's response to an impulse in its
/// first hop (`impulse_span_out_frames`): the leading delay and every
/// block the impulse reaches.
fn block_fft_span(b: &super::BlockFftDesign, ratio: usize) -> usize {
    let r = ratio.max(1);
    let block = match b.block {
        super::BlockSize::Input(n) => n,
        super::BlockSize::Output(p) => p / r,
    };
    let normal = b.overlap == super::Overlap::QuarterAverage;
    let hist = if normal { 3 * block / 4 } else { block / 2 };
    let hop = (block - hist).max(1);
    let pre = r * (2 * hop).saturating_sub(1);
    pre + (hist / hop + usize::from(normal) + 1) * r * hop
}

/// The safety gate on a runtime kind in `cx`
/// (`super::safety::check_runtime`): its one-channel instance, the
/// impulse response (at most `max_block` input frames per call, two
/// blocks of slack) and the gate over it. The response of a cascade or
/// an interpolator is at most 2^16 output frames per unit of ratio.
pub fn runtime_gate_workspace(d: &super::FilterDesign, cx: &super::BuildContext) -> u64 {
    let r = cx.ratio.up.max(1) as usize;
    let span = match d {
        super::FilterDesign::BlockFft(b) => block_fft_span(b, r),
        _ => r << 16,
    };
    let one = super::BuildContext { channels: 1, ..*cx };
    let response = span + 2 * cx.max_block.max(1) * r;
    runtime_resident(d, &one) + 8 * response as u64 + gate_workspace(span, r, true)
}

// ---------------------------------------------------------- resident memory

/// The FFT polyphase engine's input block for a chain block.
pub fn engine_block(max_block: usize) -> usize {
    if max_block == 0 {
        64
    } else {
        max_block.next_power_of_two().clamp(64, 1024)
    }
}

/// What an `n`-tap kernel's runtime keeps in `cx` (an estimate): the FFT
/// polyphase engine's partition spectra and per-channel delay lines and
/// FIFOs, or the rational runtime's coefficient rows and histories.
pub fn kernel_resident(n: usize, cx: &super::BuildContext) -> u64 {
    let l = cx.ratio.up.max(1) as usize;
    let per_branch = n.div_ceil(l).max(1);
    let ch = cx.channels as u64;
    if cx.ratio.is_integer() {
        let b = engine_block(cx.max_block);
        let parts = per_branch.div_ceil(b) as u64;
        let spectrum = 16 * (b as u64 + 1);
        let fifos = 8 * 4 * (b * l) as u64;
        parts * l as u64 * spectrum + ch * (parts * spectrum + fifos)
    } else {
        8 * (l * per_branch) as u64 + ch * 8 * (per_branch + cx.max_block.max(1)) as u64
    }
}

/// What `a` runtime with fixed internal filters keeps (an estimate).
pub fn runtime_resident(d: &super::FilterDesign, cx: &super::BuildContext) -> u64 {
    let r = cx.ratio.up.max(1) as u64;
    let ch = cx.channels.max(1) as u64;
    match d {
        super::FilterDesign::BlockFft(b) => {
            let block = match b.block {
                super::BlockSize::Input(n) => n as u64,
                super::BlockSize::Output(p) => p as u64 / r,
            };
            // History, block and spectra per channel, at both rates.
            ch * 8 * 4 * block * (r + 1)
        }
        _ => ch * 64 * 1024 * r,
    }
}

// ----------------------------------------------------------- callback work

/// The work the runtime does on the audio thread, per channel, as the
/// admission check prices it ([`Callback::seconds`] with this machine's
/// [`KernelRates`]):
///
/// | Runtime | Work per second and channel |
/// |---|---|
/// | integer kernel: FFT polyphase engine, block `b` | `fs/b` blocks of `parts * r * (b+1)` spectral bins and `1 + r` transforms of `2b` points |
/// | rational kernel: direct polyphase | `out_rate * ceil(grid_taps / L)` dot multiply-adds |
/// | cascade, FIR stages | per stage the same block model with its short kernel |
/// | cascade, recursive stages | per stage output sample, `order` serial multiply-adds |
/// | short interpolators | a few multiply-adds (polynomial) or 127 scalar `mul_add` (table sinc) per output sample |
/// | block FFT | per hop, the block's forward transform, `r` inverse ones and `r B / 2` bins |
///
/// A kernel is priced at the length the admission check has always used:
/// the exact length, but twice the design length for a cepstral
/// intermediate phase (whose kernel is four times it) and the expected
/// growth from its beta for a reflected one ([`intermediate_growth`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Callback {
    /// The FFT polyphase engine: `taps` at the output rate, ratio `ratio`.
    Polyphase { taps: usize, ratio: usize },
    /// The direct polyphase rational runtime.
    Rational { grid_taps: usize, ratio: RateRatio },
    /// The x2/x3 stage cascade at the integer `ratio`.
    Cascade {
        stage: super::CascadeStage,
        ratio: usize,
    },
    /// A short interpolator at the integer `ratio`.
    Interpolator {
        method: super::InterpMethod,
        ratio: usize,
    },
    /// The block FFT at the integer `ratio`.
    BlockFft {
        design: super::BlockFftDesign,
        ratio: usize,
    },
    /// Nothing runs (a unity ratio).
    None,
}

/// Per output sample and channel, in in-cache dot multiply-adds: FIFO
/// pushes and pops, interleaving, the output copy.
const OUT_OVERHEAD: f64 = 2.0;
/// Per recursive-stage output sample and channel and per filter order, in
/// in-cache dot multiply-adds: the recursion is a serial chain (one fused
/// multiply-add latency per term), several times slower than a dot.
const IIR_PER_ORDER: f64 = 4.0;
/// Per output sample and channel of the cosine and Catmull-Rom
/// interpolators.
const POLY_PER_OUTPUT: f64 = 8.0;
/// Scalar `mul_add` terms per output sample and channel of the table sinc
/// (127 taps, plus the history copy).
const TABLE_SINC_PER_OUTPUT: f64 = 127.0 * 1.2;
/// The FFT polyphase engine's work beside its bins and transforms: the
/// delay line, the branch interleave, FIFO copies.
const FFT_ENGINE_OVERHEAD: f64 = 1.15;
/// The block FFT's work beside its transforms and bins: windowing, the
/// split forward transform's twiddle pass, the phase interleave.
const BLOCK_FFT_OVERHEAD: f64 = 1.8;

use crate::cpu_bench::KernelRates;

/// Kernel length growth of a reflected intermediate-phase blend, from its
/// Kaiser beta: about 1.45x at beta 14, 3.1x at beta 2 (upsampler docs).
pub fn intermediate_growth(beta: f64) -> f64 {
    (1.45 + (14.0 - beta).max(0.0) * (3.1 - 1.45) / 12.0).clamp(1.45, 3.1)
}

/// Seconds per second of audio, per channel, of a uniformly partitioned
/// polyphase FFT convolution: `taps` at the output rate, ratio `r`, input
/// block `b`, source rate `fs`.
fn fft_polyphase_secs(rates: &KernelRates, taps: usize, r: usize, b: usize, fs: f64) -> f64 {
    let blocks = fs / b as f64;
    let parts = taps.div_ceil(r).div_ceil(b).max(1);
    let bins = (b + 1) as f64;
    let ir_bytes = parts * r * (b + 1) * 16;
    let spectral = blocks * (parts * r) as f64 * bins / rates.spectral_rate(ir_bytes);
    let ffts = blocks * (1 + r) as f64 * rates.fft_seconds(2 * b);
    (spectral + ffts) * FFT_ENGINE_OVERHEAD + out_overhead(rates, fs * r as f64)
}

fn out_overhead(rates: &KernelRates, out_rate: f64) -> f64 {
    out_rate * OUT_OVERHEAD / rates.reference_rate()
}

/// Seconds per second and channel of a direct polyphase rational kernel.
fn rational_secs(rates: &KernelRates, grid_taps: usize, ratio: RateRatio, fs: f64) -> f64 {
    let out_rate = fs * ratio.as_f64();
    let per_out = grid_taps.div_ceil(ratio.up.max(1) as usize) as f64;
    out_rate * per_out / rates.dot_rate(grid_taps * 8) + out_overhead(rates, out_rate)
}

/// Orders of the recursive cascade stages (`presets::cascade::iir`).
fn iir_order(stage: super::CascadeStage, f: usize) -> usize {
    match (stage, f) {
        (super::CascadeStage::Elliptic, 2) => 16,
        (super::CascadeStage::Elliptic, _) => 17,
        (_, 2) => 30,
        _ => 24,
    }
}

impl Callback {
    /// Seconds of CPU per second of audio and channel on a machine with
    /// `rates`, from `source_rate` with chain blocks of `max_block` input
    /// frames.
    pub fn seconds(
        &self,
        rates: &KernelRates,
        source_rate: u32,
        max_block: usize,
    ) -> Result<f64, DesignError> {
        let fs = f64::from(source_rate);
        let reference = rates.reference_rate();
        Ok(match *self {
            Self::None => 0.0,
            Self::Polyphase { taps, ratio } => {
                fft_polyphase_secs(rates, taps, ratio, engine_block(max_block), fs)
            }
            Self::Rational { grid_taps, ratio } => rational_secs(rates, grid_taps, ratio, fs),
            Self::Cascade { stage, ratio } => {
                use super::CascadeStage::{Elliptic, InverseChebyshev};
                let mut secs = 0.0;
                let mut rate = fs;
                for f in crate::presets::cascade::stage_factors(ratio) {
                    if matches!(stage, Elliptic | InverseChebyshev) {
                        secs += rate * f as f64 * iir_order(stage, f) as f64 * IIR_PER_ORDER
                            / reference;
                    } else {
                        let Some(p) =
                            super::catalogue::runtime_preset(&super::FilterDesign::Cascade(stage))
                        else {
                            return Err(DesignError::NotAKernel);
                        };
                        let taps = crate::presets::cascade::stage_kernel(p, f)
                            .map_err(|e| DesignError::Failed(e.to_string()))?
                            .len();
                        let b = taps.div_ceil(f).next_power_of_two().max(8);
                        secs += fft_polyphase_secs(rates, taps, f, b, rate);
                    }
                    rate *= f as f64;
                }
                secs
            }
            Self::Interpolator { method, ratio } => {
                let out = fs * ratio as f64;
                if method == super::InterpMethod::TableSinc {
                    out * TABLE_SINC_PER_OUTPUT / rates.mul_add
                } else {
                    out * POLY_PER_OUTPUT / reference
                }
            }
            Self::BlockFft { design, ratio: r } => {
                let out = fs * r as f64;
                let b = match design.block {
                    super::BlockSize::Input(b) => b,
                    super::BlockSize::Output(p) => p / r,
                };
                let hop = match design.overlap {
                    super::Overlap::QuarterAverage => b / 4,
                    super::Overlap::Half => b / 2,
                };
                let l = b.min(1 << 15);
                let q = r * b / l;
                let blocks = fs / hop as f64;
                let per_block = rates.fft_seconds(b)
                    + q as f64 * rates.fft_seconds(l)
                    + (r * b / 2) as f64 / rates.spectral_rate(b * 8);
                blocks * per_block * BLOCK_FFT_OVERHEAD + out_overhead(rates, out)
            }
        })
    }
}

/// The callback of a kernel of `taps` priced taps in `cx`.
pub(crate) fn kernel_callback(taps: usize, ratio: RateRatio) -> Callback {
    match ratio.as_integer() {
        Some(r) => Callback::Polyphase {
            taps,
            ratio: r as usize,
        },
        None => Callback::Rational {
            grid_taps: taps,
            ratio,
        },
    }
}

/// The callback of a runtime kind at `ratio` (integer only).
pub(crate) fn runtime_callback(
    d: &super::FilterDesign,
    ratio: RateRatio,
) -> Result<Callback, DesignError> {
    let Some(r) = ratio.as_integer() else {
        return Err(DesignError::Ratio {
            ratio,
            need: "an integer upsampling ratio",
        });
    };
    let ratio = r as usize;
    Ok(match *d {
        super::FilterDesign::Cascade(stage) => Callback::Cascade { stage, ratio },
        super::FilterDesign::Interpolator(method) => Callback::Interpolator { method, ratio },
        super::FilterDesign::BlockFft(design) => Callback::BlockFft { design, ratio },
        super::FilterDesign::Kernel(_) => return Err(DesignError::NotAKernel),
    })
}

/// The callback work of `d` in `cx`, as the admission check prices it:
/// the length rule evaluated (an automatic length searched, cached), no
/// budget checked. [`Callback::None`] at a unity ratio.
pub fn callback(
    d: &super::FilterDesign,
    cx: &super::BuildContext,
) -> Result<Callback, DesignError> {
    if cx.ratio.is_unity() {
        return Ok(Callback::None);
    }
    let d = &d.with_output(&cx.output);
    let super::FilterDesign::Kernel(k) = d else {
        return runtime_callback(d, cx.ratio);
    };
    if super::novel(k) {
        let (_, priced) = super::novel_taps(k, cx.source_rate, cx.ratio)?;
        return Ok(kernel_callback(priced, cx.ratio));
    }
    if k.numerics.path == super::ArithmeticPath::Standard {
        let r = super::paths::standard::recipe(k, cx.source_rate, cx.ratio)?;
        return Ok(kernel_callback(standard_priced_taps(k, &r), cx.ratio));
    }
    let grid = cx.grid();
    let mut t = super::length::per_input(k, grid.up, cx.source_rate)?;
    if grid.decimating {
        t = super::length::decimated(t, &k.decimation, &grid);
    }
    Ok(kernel_callback(
        preset_priced_taps(k, t * grid.up),
        cx.ratio,
    ))
}

/// The [`Plan`] of `d` in `cx` with its length resolved (an automatic length
/// searched, cached) and the ratio rules applied, but no budget checked:
/// what a static budget prices a design at (the varispeed budget sums
/// [`Plan::macs_per_second`] with the stage's own). [`super::feasibility`]
/// is the cheap, checked counterpart (an automatic length at its
/// estimate). Run it with `pipelines` 1 for one pipeline's cost.
pub fn plan(d: &super::FilterDesign, cx: &super::BuildContext) -> Result<Plan, DesignError> {
    let d = &d.with_output(&cx.output);
    let super::FilterDesign::Kernel(k) = d else {
        super::rates::check(d, cx.ratio)?;
        return Ok(Plan::runtime(d, cx));
    };
    if super::novel(k) {
        super::rates::check(d, cx.ratio)?;
        return super::novel_plan(k, cx);
    }
    if k.numerics.path == super::ArithmeticPath::Standard {
        let r = super::paths::standard::recipe(k, cx.source_rate, cx.ratio)?;
        super::rates::check(d, cx.ratio)?;
        return Ok(super::paths::standard::plan(
            k,
            r.taps,
            standard_priced_taps(k, &r),
            cx,
        ));
    }
    let n = super::grid_taps(d, cx.source_rate, cx.ratio)?;
    Ok(super::kernel_plan(k, n, cx))
}

/// The priced length of a preset-path kernel of `n` design taps on the
/// grid: twice it for intermediate phase.
pub(crate) fn preset_priced_taps(k: &super::KernelDesign, n: usize) -> usize {
    match k.phase {
        super::Phase::Intermediate { .. } => 2 * n,
        _ => n,
    }
}

/// The priced length of the `standard` path's resolved design `r`: its
/// length, grown by [`intermediate_growth`] for a blended intermediate
/// phase.
pub(crate) fn standard_priced_taps(
    k: &super::KernelDesign,
    r: &super::paths::standard::UpsamplerDesign,
) -> usize {
    let mixed = matches!(k.phase, super::Phase::Intermediate { mix, .. } if mix > 0.0 && mix < 1.0);
    if mixed {
        (r.taps as f64 * intermediate_growth(r.kaiser_beta)).ceil() as usize
    } else {
        r.taps
    }
}

/// What a design needs in one [`super::BuildContext`], known before it is
/// designed ([`super::feasibility`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Plan {
    pub grid: Grid,
    /// Taps of the kernel on the grid (0 for the runtime kinds): exact, or
    /// an upper bound (`grid_taps_exact` false) where the length is only
    /// known once designed (a reflected intermediate phase grows up to
    /// four times the design length), or the window-formula estimate of an
    /// automatic length before its search.
    pub grid_taps: usize,
    pub grid_taps_exact: bool,
    /// Peak construction memory (an upper bound), from the formulas above;
    /// an automatic length is held to the limit per candidate instead.
    pub workspace_bytes: u64,
    /// What the runtime keeps, all channels (an estimate).
    pub resident_bytes: u64,
    /// Multiply-adds per second and channel of the direct polyphase sum
    /// over `grid_taps` (output rate times taps per branch), times the
    /// context's pipelines at a rational ratio: the figure the rational
    /// budget holds to [`MAX_RATIONAL_MACS_PER_SECOND`]. 0 for the runtime
    /// kinds.
    pub macs_per_second: f64,
    /// The work on the audio thread, as the admission check prices it
    /// ([`Callback::seconds`]); for an automatic length before its search,
    /// at the estimate ([`callback`] searches).
    pub callback: Callback,
}

impl Plan {
    /// The plan of a kernel of `grid_taps` taps in `cx`, built with
    /// `workspace` bytes (the gate's added here).
    pub(crate) fn kernel(
        cx: &super::BuildContext,
        grid_taps: usize,
        exact: bool,
        workspace: u64,
        priced_taps: usize,
        gate_doubling: bool,
    ) -> Self {
        let pipelines = if cx.ratio.is_integer() {
            1
        } else {
            cx.pipelines.max(1)
        };
        let grid = cx.grid();
        Self {
            grid,
            grid_taps,
            grid_taps_exact: exact,
            // The design's grids are freed before the gate runs on the
            // kernel.
            workspace_bytes: workspace.max(gate_workspace(grid_taps, grid.up, gate_doubling)),
            resident_bytes: kernel_resident(grid_taps, cx),
            macs_per_second: rational_macs_per_second(grid_taps, cx.ratio, cx.source_rate)
                * pipelines as f64,
            callback: if cx.ratio.is_unity() {
                Callback::None
            } else {
                kernel_callback(priced_taps, cx.ratio)
            },
        }
    }

    /// The plan of a runtime kind (fixed internal filters) in `cx`.
    pub(crate) fn runtime(d: &super::FilterDesign, cx: &super::BuildContext) -> Self {
        Self {
            grid: cx.grid(),
            grid_taps: 0,
            grid_taps_exact: true,
            // The gate's instance and measurement (prepare), then the
            // runtime (build).
            workspace_bytes: SLACK + runtime_resident(d, cx).max(runtime_gate_workspace(d, cx)),
            resident_bytes: runtime_resident(d, cx),
            macs_per_second: 0.0,
            callback: runtime_callback(d, cx.ratio).unwrap_or(Callback::None),
        }
    }

    /// The plan on the wire, with the CPU share of one pipeline (both
    /// channels) when priced, and the bounded-input peak once designed.
    pub fn wire(
        &self,
        cpu_load: Option<f64>,
        bounded_peak_db: Option<f64>,
    ) -> audivane_protocol::design::DesignPlan {
        audivane_protocol::design::DesignPlan {
            grid_up: self.grid.up as u32,
            grid_down: self.grid.down as u32,
            grid_taps: self.grid_taps,
            grid_taps_exact: self.grid_taps_exact,
            workspace_bytes: self.workspace_bytes,
            resident_bytes: self.resident_bytes,
            cpu_load: cpu_load.filter(|l| l.is_finite()),
            bounded_peak_db: bounded_peak_db.filter(|l| l.is_finite()),
        }
    }

    /// Refuse the plan when its workspace is over `cx`'s cap.
    pub(crate) fn check_workspace(self, cx: &super::BuildContext) -> Result<Self, DesignError> {
        check_workspace(
            self.workspace_bytes,
            cx.workspace,
            &format!("the {}-tap {} kernel", self.grid_taps, self.grid.ratio),
        )?;
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::design::paths::standard::design_of;
    use crate::design::{BuildContext, FilterDesign, Phase, catalogue, feasibility, length, rates};
    use audivane_protocol::{
        ALLOWED_RATES, FilterPhase, FilterPreset, HIGH_OUTPUT_RATES, OutputConfig, RateRatio,
        TapsUnit,
    };

    fn cx(source_rate: u32, ratio: RateRatio, pipelines: usize) -> BuildContext {
        BuildContext {
            source_rate,
            ratio,
            channels: 2,
            max_block: 256,
            pipelines,
            output: OutputConfig::default(),
            workspace: DESIGN_WORKSPACE_FLOOR,
        }
    }

    #[test]
    fn workspace_cap_is_a_quarter_of_the_ram_within_its_bounds() {
        const GIB: u64 = 1 << 30;
        assert_eq!(DESIGN_WORKSPACE_FLOOR, 1536 << 20);
        assert_eq!(DESIGN_WORKSPACE_CEILING, 4 * GIB);
        // A read failure: the floor.
        assert_eq!(design_workspace_for_ram(None), DESIGN_WORKSPACE_FLOOR);
        // Below the floor (4 GiB and 6 GiB machines, and nonsense).
        for ram in [0, 1, 4 * GIB, 6 * GIB] {
            assert_eq!(
                design_workspace_for_ram(Some(ram)),
                DESIGN_WORKSPACE_FLOOR,
                "{ram}"
            );
        }
        // In range: a quarter.
        assert_eq!(design_workspace_for_ram(Some(6 * GIB + 4)), 6 * GIB / 4 + 1);
        assert_eq!(design_workspace_for_ram(Some(8 * GIB)), 2 * GIB);
        assert_eq!(design_workspace_for_ram(Some(12 * GIB)), 3 * GIB);
        assert_eq!(design_workspace_for_ram(Some(16 * GIB)), 4 * GIB);
        // Above the ceiling.
        for ram in [16 * GIB + 4, 32 * GIB, 121 * GIB, u64::MAX] {
            assert_eq!(
                design_workspace_for_ram(Some(ram)),
                DESIGN_WORKSPACE_CEILING,
                "{ram}"
            );
        }
        // Nothing sets the cap in a test process: the floor.
        assert_eq!(design_workspace(), DESIGN_WORKSPACE_FLOOR);
    }

    /// The refusal names the context's cap, not a fixed one.
    #[test]
    fn workspace_refusal_uses_the_context_cap() {
        let x2 = RateRatio::integer(2);
        let design = custom(1 << 22, FilterPhase::Minimum);
        let mut c = cx(44_100, x2, 2);
        let need = feasibility(
            &design,
            &BuildContext {
                workspace: DESIGN_WORKSPACE_CEILING,
                ..c
            },
        )
        .unwrap()
        .workspace_bytes;
        c.workspace = need - 1;
        let e = feasibility(&design, &c).unwrap_err().to_string();
        assert!(
            e.contains(&format!(
                "more than the {:.2} GB design limit",
                (need - 1) as f64 / 1e9
            )),
            "{e}"
        );
        c.workspace = need;
        assert!(feasibility(&design, &c).is_ok());
    }

    /// Every output rate a ratio ladder can offer (the daemon's
    /// `MASK_OUTPUT_RATES`): the multiples x1..=16 of the graph rates up to
    /// 768 kHz, then the high rates. The daemon checks the two lists agree.
    pub(crate) fn mask_output_rates() -> Vec<u32> {
        let mut v: Vec<u32> = ALLOWED_RATES
            .iter()
            .flat_map(|&r| (1..=16).map(move |m| r * m))
            .filter(|&r| r <= 768_000)
            .chain(HIGH_OUTPUT_RATES.iter().copied())
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    /// Every (source rate, ratio) a preset can be asked for: each graph
    /// rate at every integer ratio up to the highest output rate, and to
    /// every other output rate of a ladder with terms up to 4096.
    fn every_case() -> Vec<(u32, RateRatio)> {
        let max_output = u64::from(audivane_protocol::MAX_OUTPUT_RATE);
        let mut cases = Vec::new();
        for &rate in ALLOWED_RATES {
            for r in 2..=MAX_RATIO as u32 {
                if u64::from(rate) * u64::from(r) <= max_output {
                    cases.push((rate, RateRatio::integer(r)));
                }
            }
            for out in mask_output_rates() {
                let r = RateRatio::from_rates(rate, out);
                if !r.is_integer() && r.up <= 4096 && r.down <= 4096 {
                    cases.push((rate, r));
                }
            }
        }
        cases
    }

    /// The legacy checks of a preset case: the ratio rules, then at a
    /// rational ratio the rational budget (at two pipelines).
    fn legacy_ok(d: &FilterDesign, rate: u32, ratio: RateRatio) -> bool {
        if rates::check(d, ratio).is_err() {
            return false;
        }
        let FilterDesign::Kernel(k) = d else {
            return true;
        };
        if ratio.is_integer() {
            return true;
        }
        let grid = crate::design::Grid::new(rate, ratio);
        let mut t = length::per_input(k, grid.up, rate).unwrap();
        if grid.decimating {
            t = length::decimated(t, &k.decimation, &grid);
        }
        let n = t * grid.up;
        let n = match k.phase {
            Phase::Intermediate { .. } => 4 * n,
            _ => n,
        };
        check_rational(
            n,
            ratio,
            rate,
            MAX_RATIONAL_GRID_TAPS,
            RATIONAL_SWAP_PIPELINES,
        )
        .is_ok()
    }

    /// The new limits refuse no preset case the legacy checks pass, over
    /// every source rate and every output rate a ladder offers (all of
    /// `MASK_OUTPUT_RATES`, for example 48 kHz -> 132.3 kHz, 441/160), and
    /// the largest cepstral transform among them needs 2^25 points
    /// (`kaiser_steep_mp` at 1280/147), half of [`MAX_CEPSTRAL_FFT_LEN`].
    #[test]
    fn caps_refuse_no_legacy_preset_case() {
        assert_eq!(mask_output_rates().len(), 36);
        let mut largest = (0, String::new());
        let mut workspace = 0u64;
        for &p in FilterPreset::ALL {
            let d = catalogue::design(p);
            for &(rate, ratio) in &every_case() {
                let legacy = legacy_ok(&d, rate, ratio);
                let plan = feasibility(&d, &cx(rate, ratio, RATIONAL_SWAP_PIPELINES));
                assert_eq!(plan.is_ok(), legacy, "{p:?} {rate} {ratio}: {plan:?}");
                let Ok(plan) = plan else { continue };
                // The plan's callback is the one the admission check prices.
                let c = cx(rate, ratio, RATIONAL_SWAP_PIPELINES);
                assert_eq!(Ok(plan.callback), callback(&d, &c), "{p:?} {rate} {ratio}");
                workspace = workspace.max(plan.workspace_bytes);
                let FilterDesign::Kernel(k) = &d else {
                    continue;
                };
                let design = match k.phase {
                    Phase::Linear => continue,
                    Phase::Minimum(_) => plan.grid_taps,
                    Phase::Intermediate { .. } => plan.grid_taps / 4,
                };
                let m = crate::design::phase::cepstral::fft_len(design);
                if m > largest.0 {
                    largest = (m, format!("{p:?} {rate} {ratio}"));
                }
            }
        }
        assert_eq!(largest.0, 1 << 25, "{}", largest.1);
        assert_eq!(MAX_CEPSTRAL_FFT_LEN, 2 * largest.0);
        // Every preset case fits the floor: the masks of the presets do
        // not depend on the machine.
        assert!(workspace <= DESIGN_WORKSPACE_FLOOR, "{workspace}");
        // The cap itself: one tap more than the largest prototype it admits.
        let x2 = RateRatio::integer(2);
        assert!(check_cepstral((1 << 21) - 1, x2).is_ok());
        let e = check_cepstral(1 << 21, x2).unwrap_err();
        assert!(
            matches!(&e, DesignError::Budget(b) if b.limit == Limit::CepstralFft),
            "{e:?}"
        );
    }

    fn custom(taps: usize, phase: FilterPhase) -> FilterDesign {
        use crate::design::paths::standard::ManualConfig;
        FilterDesign::Kernel(
            design_of(&ManualConfig {
                preset: None,
                taps,
                taps_unit: TapsUnit::Output,
                phase,
                phase_mix: 0.5,
                ..ManualConfig::default()
            })
            .unwrap(),
        )
    }

    fn limit_of(r: Result<Plan, DesignError>) -> Option<Limit> {
        match r {
            Err(DesignError::Budget(b)) => Some(b.limit),
            Ok(_) => None,
            Err(e) => panic!("{e:?}"),
        }
    }

    /// The library holds integer Custom lengths to the kernel limit (the
    /// daemon refused them first, and still does with its own messages).
    #[test]
    fn integer_custom_lengths_are_limited() {
        let x4 = cx(44_100, RateRatio::integer(4), 2);
        let f = |taps, phase| limit_of(feasibility(&custom(taps, phase), &x4));
        assert_eq!(f(MAX_KERNEL_TAPS, FilterPhase::Linear), None);
        assert_eq!(
            f(MAX_KERNEL_TAPS + 1, FilterPhase::Linear),
            Some(Limit::KernelTaps)
        );
        assert_eq!(f(MAX_KERNEL_TAPS / 4, FilterPhase::Intermediate), None);
        assert_eq!(
            f(MAX_KERNEL_TAPS / 4 + 1, FilterPhase::Intermediate),
            Some(Limit::KernelTaps)
        );
    }

    /// At the smallest cap ([`DESIGN_WORKSPACE_FLOOR`]) a reflected
    /// minimum phase above 2^21 taps needs a 2^25-point cepstral grid
    /// (about 1.7 GB): over the workspace limit. 2^21 taps fit.
    #[test]
    fn workspace_limit_refuses_the_longest_reflected_designs() {
        let x2 = cx(44_100, RateRatio::integer(2), 2);
        let f = |taps| limit_of(feasibility(&custom(taps, FilterPhase::Minimum), &x2));
        assert_eq!(f(1 << 21), None);
        assert_eq!(f((1 << 21) + 1), Some(Limit::Workspace));
        assert_eq!(f(MAX_KERNEL_TAPS), Some(Limit::Workspace));
        // Linear designs of any allowed length fit.
        let plan = feasibility(&custom(MAX_KERNEL_TAPS, FilterPhase::Linear), &x2).unwrap();
        assert!(plan.workspace_bytes < DESIGN_WORKSPACE_FLOOR / 2);
        // The search's own refusal reads back as the budget's.
        let e = check_candidate(1 << 22, true, 0).unwrap_err();
        assert_eq!(candidate_refusal(&e).unwrap().limit, Limit::Workspace);
        assert!(check_candidate(1 << 20, true, 0).is_ok());
    }

    /// At the largest cap ([`DESIGN_WORKSPACE_CEILING`]) every Custom
    /// length the tap limits allow fits: linear and reflected minimum phase
    /// up to [`MAX_KERNEL_TAPS`] (about 4.04 GB charged at 2^23 taps,
    /// measured at 3.15 GB of heap), intermediate up to a quarter of it.
    #[test]
    fn the_largest_cap_fits_every_allowed_length() {
        let x2 = BuildContext {
            workspace: DESIGN_WORKSPACE_CEILING,
            ..cx(44_100, RateRatio::integer(2), 2)
        };
        let f = |taps, phase| feasibility(&custom(taps, phase), &x2);
        let min = f(MAX_KERNEL_TAPS, FilterPhase::Minimum).unwrap();
        assert!(min.workspace_bytes <= DESIGN_WORKSPACE_CEILING);
        assert!(
            min.workspace_bytes > 4_000_000_000,
            "{}",
            min.workspace_bytes
        );
        f(MAX_KERNEL_TAPS, FilterPhase::Linear).unwrap();
        f(MAX_KERNEL_TAPS / 4, FilterPhase::Intermediate).unwrap();
        assert_eq!(
            limit_of(f(MAX_KERNEL_TAPS + 1, FilterPhase::Minimum)),
            Some(Limit::KernelTaps)
        );
        // The largest cepstral transform of a preset-path design fits too
        // (about 3.1 GB charged at 2^26 points, measured at 2.70 GB).
        assert!(cepstral_workspace((1 << 21) - 1) <= DESIGN_WORKSPACE_CEILING);
        assert_eq!(
            crate::design::phase::cepstral::fft_len((1 << 21) - 1),
            MAX_CEPSTRAL_FFT_LEN
        );
    }
}
