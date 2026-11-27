//! Filter fingerprints: the bits of every upsampling filter the engine
//! builds, so that a refactor of the filter builders can prove it changed
//! none of them.
//!
//! Each case is one design (a preset, a custom design, or either built
//! through the chain) at one source rate and one ratio
//! (`fingerprint_common/matrix.rs`), recorded as one line of
//! `tests/fixtures/filter-fingerprints.tsv`:
//!
//! ```text
//! name source_rate up/down kind status paths taps_len taps_hash kernel_delay latency_frames latency_exact runtime_hash
//! ```
//!
//! - `paths` is what production returns, never short-circuited: the daemon's
//!   preset validation (`design::feasibility`, as its `check_rational_at`
//!   calls it) with 2 and with 4 pipelines, then the build itself
//!   (`design::upsampler`, or `Chain::try_new`), which always runs. Each is
//!   `ok` or its refusal (`ratio`, `too_long`, `cost`; another build refusal
//!   is `error`; a preset refused before its rational kernel is designed
//!   reads as a design failure and is classed by its message). Custom and
//!   chain designs have no preset checks (`-`); the daemon's own validator is
//!   pinned by `crates/daemon/tests/filter_validation.rs`. `status` is `ok`,
//!   `refused:<class>` from the checks (`ratio`, `too_long`, `cost2`,
//!   `cost4`), or `refused:build` when only the build refuses.
//! - `taps_hash`, `taps_len`, `kernel_delay`: what the runtimes were
//!   constructed from, recorded inside their constructors
//!   (`audivane_dsp_core::test_support`): the kernel and its delay as
//!   `Upsampler` or `RationalResampler` received them (`kernel_delay` an
//!   integer, or the f64 bits on a rational grid), the cascade stage kernels
//!   and recursive tables, the interpolator phase tables, the block FFT
//!   geometry, window and twiddles. Nothing is designed a second time;
//!   `taps_len` is the kernel length or the number of recorded words.
//! - `latency_frames`, `latency_exact` (`=` when it equals the frames, else
//!   the f64 bits), `runtime_hash`: from the built runtime, hashing its
//!   two-channel response, at most [`MAX_BLOCK`] input frames per call, to
//!   impulses (channel 1's half of channel 0's). Integer ratios: 1 at input
//!   frame 0 (block FFT: channel 1 at [`BLOCK_FFT_SECOND_POS`], a second
//!   block position), over the whole `impulse_span_out_frames` (every
//!   partition of a long kernel). Rational L/M: at M consecutive input
//!   frames, one on every residue modulo M, after one span of silence and
//!   followed by one, with distinct amplitudes `1 + j/4096` (channel 1:
//!   half). An impulse at input frame i reaches the grid taps
//!   `k = m M - i L`, so every tap the runtime holds is in exactly one term
//!   of one output frame of the hash (`rational_probe_reaches_every_tap`).
//! - Chain cases (`chain:<design>`) build through `Chain::try_new` with
//!   [`CHAIN_HEADROOM_DB`] of output headroom (folded into the kernel). At a
//!   unity ratio the chain must build and have no upsampler; otherwise its
//!   upsampler's response must equal, bit for bit, the same runtime built
//!   alone and scaled by `db_to_lin(-headroom)`.
//!
//! Tier A runs by default; tier B (`--ignored`) covers every preset at every
//! integer ratio and every rational pair, and runs the post-build safety gate
//! over every distinct construction (`taps_hash`), compared against the
//! committed
//! `tests/fixtures/filter-safety-exemptions.tsv` and the library's
//! exemptions (`design::safety::EXEMPTIONS`). The numeric report and the
//! memory and time tables go to `AVDSP_FINGERPRINT_ARTIFACTS` (default
//! `filter-fingerprints/` in Cargo's test temporary directory). The ladder
//! tier (`tier_ladder_builds`, with tier B) covers the rational pairs only a
//! rate ladder asks for (every input to the ladder-only output rates: 132.3
//! kHz, 144 kHz, 480 kHz, ...), for every single-kernel preset, without
//! fingerprints. A preset case that feasibility accepts (the rate mask, two
//! pipelines; with four, a subset) must build: the build adds only the
//! kernel and the safety gate, which do not depend on the pipelines.
//!
//! Modes (environment): default compares and prints every differing case and
//! column, and fails when the fixture's `# env` lines (compiler, target and
//! FFT crate versions, the C library and CPU features) do not match;
//! `AVDSP_FINGERPRINT_WRITE=1` writes the rows run into the fixture (other
//! rows stay), under an exclusive lock; `AVDSP_FINGERPRINT_RSS=1` also runs
//! cases estimated above 256 MiB alone, for their peak resident set;
//! `AVDSP_FINGERPRINT_CHUNK=k/n`, `AVDSP_FINGERPRINT_ONLY=name,name`,
//! `AVDSP_FINGERPRINT_THREADS=n` (workers: at most 16; the default is the
//! machine's cores, up to 16), `AVDSP_FINGERPRINT_DIR=path` (trial runs) and
//! `AVDSP_FINGERPRINT_PANIC=<key>` (a pool self-test) narrow or perturb a
//! run. Capture in the default test profile:
//!
//! ```text
//! AVDSP_FINGERPRINT_WRITE=1 cargo test -p audivane-dsp-core --test filter_fingerprints -- --include-ignored --nocapture
//! ```
//!
//! Concurrency is bounded by memory: a case is admitted while the summed
//! estimates of the running cases stay under half the available memory, and
//! a case estimated above 1 GiB runs alone. Workers are the machine's cores,
//! at most 16. A case whose construction (`taps_hash`, when the runtime was
//! built from a recorded kernel or block FFT) another case is already
//! probing shares that runtime hash: the fixture has every pair of such
//! non-chain rows with one `taps_hash` on one hash. Chain cases and the
//! runtime kinds always probe their own (a chain folds its headroom into the
//! response; a runtime kind's response is its safety gate's input).

#[path = "fingerprint_common/matrix.rs"]
mod matrix;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Instant;

use audivane_dsp_core::bypass::hash_u64;
use audivane_dsp_core::chain::{Chain, ChainSpec};
use audivane_dsp_core::db_to_lin;
use audivane_dsp_core::design::upsampler::{Stage, preset_error};
use audivane_dsp_core::design::{
    self, BuildContext, DesignError, FilterDesign, Named, Refusal, catalogue,
};
use audivane_dsp_core::presets::{FilterPreset, PresetError, UpsamplerRuntime};
use audivane_dsp_core::test_support::{Record, capture};
use audivane_protocol::{ChainConfig, FilterPhase, RateRatio, RateTarget, UpsamplerConfig};
use matrix::{CHAIN_HEADROOM_DB, COST4_PICKS, Case, Design};
use realfft::RealFftPlanner;

// ------------------------------------------------------------ heap counter

/// Live heap bytes and their high-water mark, per thread. The design code
/// spawns short helper threads for some windows and root sweeps; their own
/// allocations are not counted (they fill buffers the case thread owns).
struct Counting;

thread_local! {
    static LIVE: Cell<i64> = const { Cell::new(0) };
    static PEAK: Cell<i64> = const { Cell::new(0) };
}

fn account(delta: i64) {
    let _ = LIVE.try_with(|live| {
        let v = live.get() + delta;
        live.set(v);
        let _ = PEAK.try_with(|p| {
            if v > p.get() {
                p.set(v);
            }
        });
    });
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            account(layout.size() as i64);
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            account(layout.size() as i64);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        account(-(layout.size() as i64));
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            account(new_size as i64 - layout.size() as i64);
        }
        p
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Run `f` and return its result with the peak of live heap bytes above
/// the level at the start, on this thread. Nests: an enclosing measure
/// still sees this peak.
fn measure<T>(f: impl FnOnce() -> T) -> (T, u64) {
    let base = LIVE.with(Cell::get);
    let outer = PEAK.with(Cell::get);
    PEAK.with(|p| p.set(base));
    let out = f();
    let peak = PEAK.with(Cell::get);
    PEAK.with(|p| p.set(outer.max(peak)));
    (out, (peak - base).max(0) as u64)
}

// ------------------------------------------------------------ constants

/// Input frames per `process` call (the engine's largest block).
const MAX_BLOCK: usize = 1024;
/// Channels of every built runtime.
const CHANNELS: usize = 2;
/// Input frame of channel 1's impulse for the block FFT runtimes (inside
/// the first hop of every geometry, which is at least 4096 frames).
const BLOCK_FFT_SECOND_POS: usize = 1001;
/// Rational pipelines without and with A/B (the daemon's
/// `rational_pipelines`).
const PIPELINES: [usize; 2] = [2, 4];
/// Cases estimated above this run alone.
const SOLO_BYTES: u64 = 1 << 30;
/// With `AVDSP_FINGERPRINT_RSS=1`, cases whose construction is estimated
/// above this run alone too, so their peak resident set can be measured.
const RSS_BYTES: u64 = 256 << 20;

// ------------------------------------------------------------ one case

#[derive(Clone, Debug, Default)]
struct Row {
    kind: &'static str,
    status: String,
    paths: String,
    taps_len: Option<usize>,
    taps_hash: Option<u64>,
    kernel_delay: Option<String>,
    latency_frames: Option<usize>,
    latency_exact: Option<f64>,
    runtime_hash: Option<u64>,
}

impl Row {
    fn columns(&self) -> String {
        fn opt<T>(v: &Option<T>, f: impl Fn(&T) -> String) -> String {
            v.as_ref().map_or_else(|| "-".to_string(), f)
        }
        let exact = match (self.latency_exact, self.latency_frames) {
            (Some(e), Some(f)) if e == f as f64 => "=".to_string(),
            (e, _) => opt(&e, |v| format!("{:016x}", v.to_bits())),
        };
        [
            self.kind.to_string(),
            self.status.clone(),
            self.paths.clone(),
            opt(&self.taps_len, |v| v.to_string()),
            opt(&self.taps_hash, |v| format!("{v:016x}")),
            opt(&self.kernel_delay, Clone::clone),
            opt(&self.latency_frames, |v| v.to_string()),
            exact,
            opt(&self.runtime_hash, |v| format!("{v:016x}")),
        ]
        .join("\t")
    }
}

/// What the safety gate is run on.
enum GateInput {
    /// A kernel on a grid of `branches` polyphase branches.
    Kernel {
        h: Vec<f64>,
        branches: usize,
        delay: f64,
        linear: bool,
    },
    /// A runtime's impulse response at the output rate (`branches` output
    /// phases), with its reported latency.
    Response {
        h: Vec<f64>,
        branches: usize,
        latency: usize,
    },
    /// The block FFT runtime: impulses at many block positions.
    BlockFft {
        preset: FilterPreset,
        ratio: usize,
        b: usize,
        hop: usize,
    },
}

#[derive(Clone, Debug, Default)]
struct Memory {
    /// Peak heap of the construction (design included), the impulse run,
    /// the safety gate, and the whole case.
    build: u64,
    run: u64,
    gate: u64,
    total: u64,
    /// Peak resident set above the start, for a case that ran alone.
    rss: Option<u64>,
    /// The pool's estimate.
    estimate: u64,
    ms: u128,
    run_ms: u128,
    gate_ms: u128,
}

/// The gates of one run: each distinct construction is gated once, by the
/// first case (in matrix order) that reaches it.
#[derive(Default)]
struct Gates {
    first: Mutex<HashMap<u64, usize>>,
    results: Mutex<BTreeMap<u64, Gate>>,
}

/// One case's context: its matrix index, the gates when tier B runs them,
/// and the constructions cases share (`None` for chain cases, which must
/// probe their own headroom-scaled response).
struct Ctx<'a> {
    index: usize,
    gates: Option<&'a Gates>,
    share: Option<&'a ProbeShare>,
}

struct Outcome {
    row: Row,
    memory: Memory,
    /// Checks of the case that failed (the chain's headroom and unity
    /// rules, the recorded constructions); they fail the test.
    failures: Vec<String>,
}

fn build_class(e: &PresetError) -> &'static str {
    match e {
        PresetError::Ratio { .. } => "ratio",
        PresetError::TooLong { .. } => "too_long",
        PresetError::NotImplemented(_) => "error",
        PresetError::Design(msg) => message_class(msg),
    }
}

/// The class of a refusal message: `design::budget::check_rational`'s two
/// refusals (the multiply-add budget, the grid length), told apart by
/// their text, or `error`.
fn message_class(msg: &str) -> &'static str {
    if msg.contains("multiply-adds") {
        "cost"
    } else if msg.contains(" MB)") {
        "too_long"
    } else {
        "error"
    }
}

/// A build context of the matrix, at the smallest design workspace cap
/// (the one a test process has, so the chain cases build at it too): the
/// fingerprints do not depend on the machine.
fn context(rate: u32, ratio: RateRatio, pipelines: usize, channels: usize) -> BuildContext {
    BuildContext {
        source_rate: rate,
        ratio,
        channels,
        max_block: MAX_BLOCK,
        pipelines,
        output: Default::default(),
        workspace: design::budget::DESIGN_WORKSPACE_FLOOR,
    }
}

/// The daemon's validation of preset `p` at `ratio` from `rate` with
/// `pipelines` rational pipelines (the design's feasibility: the ratio
/// rules, then at a rational ratio the cost budget): `ok` or the refusal's
/// class.
fn preset_check(p: FilterPreset, rate: u32, ratio: RateRatio, pipelines: usize) -> &'static str {
    match design::feasibility(
        &catalogue::design(p),
        &context(rate, ratio, pipelines, CHANNELS),
    ) {
        Ok(_) => "ok",
        Err(e) => build_class(&preset_error(p, e)),
    }
}

/// The class of a preset build's refusal: by kind, except a refusal
/// before a rational kernel is designed, which reads as a design failure
/// (its message).
fn refusal_class(p: FilterPreset, ratio: RateRatio, r: &Refusal) -> &'static str {
    if !ratio.is_integer() && r.stage == Stage::Feasibility {
        message_class(&r.to_string())
    } else {
        build_class(&preset_error(p, r.error.clone()))
    }
}

/// Build `named` at `ratio` from `rate` as the chain does (two pipelines),
/// without headroom.
fn build_alone(
    named: &Named,
    rate: u32,
    ratio: RateRatio,
    channels: usize,
) -> Result<Box<dyn UpsamplerRuntime>, Refusal> {
    let cx = context(rate, ratio, PIPELINES[0], channels);
    design::upsampler(named, &cx, 0.0).map(|rt| rt.expect("not a unity ratio"))
}

/// The status from the two checks: the first refusal, `cost4` for a case
/// only the A/B pipeline count refuses.
fn status_of(c2: &str, c4: &str) -> String {
    match (c2, c4) {
        ("ok", "ok") => "ok".into(),
        ("ok", "cost") => "refused:cost4".into(),
        ("ok", other) => format!("refused:{other}4"),
        ("cost", _) => "refused:cost2".into(),
        (other, _) => format!("refused:{other}"),
    }
}

/// What the constructors recorded, summarised.
struct Built {
    taps_len: usize,
    taps_hash: u64,
    kernel_delay: Option<String>,
    /// The kernel the runtime received: taps, branches, delay.
    kernel: Option<(Vec<f64>, usize, f64)>,
    /// Block FFT: (B, hop).
    block: Option<(usize, usize)>,
}

fn summarise(records: &[Record], kernel_kind: bool) -> Result<Built, String> {
    let tag = |t: &str| hash_u64(t.bytes().map(u64::from));
    let taps_hash = hash_u64(records.iter().flat_map(|r| {
        [tag(r.tag), r.words.len() as u64]
            .into_iter()
            .chain(r.words.iter().copied())
    }));
    let mut built = Built {
        taps_len: records.iter().map(|r| r.words.len()).sum(),
        taps_hash,
        kernel_delay: None,
        kernel: None,
        block: None,
    };
    if records.is_empty() {
        return Err("no construction recorded".into());
    }
    if kernel_kind {
        let kernels: Vec<&Record> = records
            .iter()
            .filter(|r| r.tag == "upsampler" || r.tag == "rational")
            .collect();
        let [r] = kernels[..] else {
            return Err(format!("{} kernel runtimes constructed", kernels.len()));
        };
        // upsampler: [ratio, delay, block_in, len, taps..];
        // rational: [L, M, delay bits, len, taps..].
        let w = &r.words;
        let taps: Vec<f64> = w[4..].iter().map(|&b| f64::from_bits(b)).collect();
        built.taps_len = w[3] as usize;
        let delay = if r.tag == "upsampler" {
            built.kernel_delay = Some(w[1].to_string());
            w[1] as f64
        } else {
            built.kernel_delay = Some(format!("{:016x}", w[2]));
            f64::from_bits(w[2])
        };
        built.kernel = Some((taps, w[0] as usize, delay));
    }
    if let Some(r) = records.iter().find(|r| r.tag == "block_fft") {
        // [ratio, B, history, hop, ..]
        built.block = Some((r.words[1] as usize, r.words[3] as usize));
    }
    Ok(built)
}

/// Run `rt` from its start for `frames` input frames, at most `MAX_BLOCK`
/// per call, on impulses (`(channel, input frame, value)`, sorted by
/// frame); the output of every channel.
fn run_impulses(
    rt: &mut dyn UpsamplerRuntime,
    channels: usize,
    impulses: &[(usize, usize, f64)],
    frames: usize,
) -> Vec<Vec<f64>> {
    let r = rt.ratio();
    let expect = (frames as u64 * u64::from(r.up)).div_ceil(u64::from(r.down)) as usize;
    let mut out: Vec<Vec<f64>> = (0..channels)
        .map(|_| Vec::with_capacity(expect + 2))
        .collect();
    let mut input = vec![vec![0.0; MAX_BLOCK]; channels];
    let mut buf = vec![Vec::new(); channels];
    let mut pos = 0usize;
    let mut next = 0usize;
    while pos < frames {
        let n = MAX_BLOCK.min(frames - pos);
        for inp in &mut input {
            inp[..n].fill(0.0);
        }
        while next < impulses.len() && impulses[next].1 < pos + n {
            let (ch, at, v) = impulses[next];
            input[ch][at - pos] = v;
            next += 1;
        }
        let n_out = rt.output_frames(n);
        for b in &mut buf {
            b.clear();
            b.resize(n_out, 0.0);
        }
        {
            let ins: Vec<&[f64]> = input.iter().map(|v| &v[..n]).collect();
            let mut outs: Vec<&mut [f64]> = buf.iter_mut().map(Vec::as_mut_slice).collect();
            rt.process(&ins, &mut outs);
        }
        for (o, b) in out.iter_mut().zip(&buf) {
            o.extend_from_slice(b);
        }
        pos += n;
    }
    out
}

/// The probe impulses of the runtime hash (see the module docs) and the
/// input frames to run.
fn probe_impulses(rt: &dyn UpsamplerRuntime, block_fft: bool) -> (Vec<(usize, usize, f64)>, usize) {
    let r = rt.ratio();
    let (l, m) = (r.up as usize, r.down as usize);
    let span_in = (rt.impulse_span_out_frames() * m).div_ceil(l) + 1;
    if m == 1 {
        let second = if block_fft { BLOCK_FFT_SECOND_POS } else { 0 };
        let mut v = vec![(0, 0, 1.0), (1, second, 0.5)];
        v.sort_by_key(|i| i.1);
        (v, span_in + second)
    } else {
        // After one span of silence, so that taps the runtime reads before
        // its first output frame from an impulse at frame 0 are reached too.
        // Distinct amplitudes, so that a change to any one tap changes its
        // output frame whatever the other taps summed into it.
        let v = (0..m)
            .flat_map(|j| {
                let a = 1.0 + j as f64 / 4096.0;
                [(0, span_in + j, a), (1, span_in + j, a / 2.0)]
            })
            .collect();
        (v, 2 * span_in + m)
    }
}

/// The runtime hash, and channel 0's response to the impulse at frame 0
/// over the span (integer ratios: the gate's input for runtime kinds).
fn probe(rt: &mut dyn UpsamplerRuntime, block_fft: bool) -> (u64, Vec<f64>) {
    let (impulses, frames) = probe_impulses(rt, block_fft);
    let mut resp = run_impulses(rt, CHANNELS, &impulses, frames);
    let hash = hash_u64(resp.iter().flatten().map(|v| v.to_bits()));
    let mut first = resp.swap_remove(0);
    first.truncate(rt.impulse_span_out_frames());
    (hash, first)
}

fn kind_of(p: FilterPreset) -> &'static str {
    catalogue::design(p).kind_name()
}

fn run_case(case: &Case, ctx: &Ctx) -> Outcome {
    if std::env::var("AVDSP_FINGERPRINT_PANIC").is_ok_and(|k| k == case.key().replace('\t', " ")) {
        panic!("injected panic (AVDSP_FINGERPRINT_PANIC)");
    }
    match &case.design {
        Design::Preset(p) => run_preset(*p, case.source_rate, case.ratio, ctx),
        Design::Custom(_, cfg) => run_custom(cfg, case.source_rate, case.ratio, ctx),
        Design::Chain(inner) => run_chain(inner, case.source_rate, case.ratio),
    }
}

/// How a built runtime is fingerprinted and gated.
struct Fill<'a> {
    kernel_kind: bool,
    linear: bool,
    preset: Option<FilterPreset>,
    ctx: &'a Ctx<'a>,
}

/// The probe results of the constructions cases share (see [`Ctx::share`]).
/// The fixture has one runtime hash for every pair of non-chain rows with
/// one `taps_hash`, and only the rows built from a recorded kernel or a
/// preset's block FFT are shared: the other kinds' response is their safety
/// gate's input, and a chain folds its headroom into the response.
#[derive(Default)]
struct ProbeShare {
    state: Mutex<HashMap<u64, Shared>>,
    cv: Condvar,
}

enum Shared {
    /// One case is probing this construction (a [`Flight`]).
    Probing,
    /// Its runtime hash.
    Done(u64),
}

/// A construction this case is probing. Its `Drop` runs on unwind too: the
/// construction then reads as not probed and the waiters race for it, so a
/// panicking case cannot wedge the pool.
struct Flight<'a> {
    share: &'a ProbeShare,
    taps_hash: u64,
}

impl Drop for Flight<'_> {
    fn drop(&mut self) {
        let mut s = self.share.state.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(s.get(&self.taps_hash), Some(Shared::Probing)) {
            s.remove(&self.taps_hash);
        }
        self.share.cv.notify_all();
    }
}

impl ProbeShare {
    /// The runtime hash of `taps_hash` if a case already probed it, else a
    /// flight to probe it (waiting while another case does).
    fn enter(&self, taps_hash: u64) -> Result<u64, Flight<'_>> {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            match s.get(&taps_hash) {
                Some(Shared::Done(hash)) => return Ok(*hash),
                Some(Shared::Probing) => s = self.cv.wait(s).unwrap_or_else(|e| e.into_inner()),
                None => {
                    s.insert(taps_hash, Shared::Probing);
                    return Err(Flight {
                        share: self,
                        taps_hash,
                    });
                }
            }
        }
    }

    /// Publish the runtime hash `flight`'s construction probed. Dropping the
    /// flight then only wakes the waiters, which find it done.
    fn publish(&self, flight: Flight<'_>, hash: u64) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(flight.taps_hash, Shared::Done(hash));
    }
}

/// Fill the row's runtime columns from a built runtime and its records,
/// and run the gate when this case is the first to reach its hash.
fn fill_built(
    row: &mut Row,
    memory: &mut Memory,
    rt: &mut dyn UpsamplerRuntime,
    records: &[Record],
    how: Fill,
) -> Result<(), String> {
    let built = summarise(records, how.kernel_kind)?;
    row.taps_len = Some(built.taps_len);
    row.taps_hash = Some(built.taps_hash);
    row.kernel_delay = built.kernel_delay.clone();
    row.latency_frames = Some(rt.latency_out_frames());
    row.latency_exact = Some(rt.latency_out_exact());
    let block_fft = built.block.is_some();
    // Only where the gate below reads the kernel or block, never `resp`.
    let share = how
        .ctx
        .share
        .filter(|_| built.kernel.is_some() || (built.block.is_some() && how.preset.is_some()));
    let t = Instant::now();
    let mut resp = Vec::new();
    let mut probed = false;
    let mut run_probe = || {
        let ((hash, r), bytes) = measure(|| probe(rt, block_fft));
        memory.run = bytes;
        resp = r;
        probed = true;
        hash
    };
    let hash = match share {
        Some(share) => match share.enter(built.taps_hash) {
            Ok(hash) => hash,
            Err(flight) => {
                let hash = run_probe();
                share.publish(flight, hash);
                hash
            }
        },
        None => run_probe(),
    };
    if probed {
        memory.run_ms = t.elapsed().as_millis();
    }
    row.runtime_hash = Some(hash);
    let Some(gates) = how.ctx.gates else {
        return Ok(());
    };
    let index = how.ctx.index;
    if *gates
        .first
        .lock()
        .unwrap()
        .entry(built.taps_hash)
        .or_insert(index)
        != index
    {
        return Ok(());
    }
    let ratio = rt.ratio();
    let input = match (built.kernel, built.block, how.preset) {
        (Some((h, branches, delay)), _, _) => GateInput::Kernel {
            h,
            branches,
            delay,
            linear: how.linear,
        },
        (None, Some((b, hop)), Some(preset)) => GateInput::BlockFft {
            preset,
            ratio: ratio.up as usize,
            b,
            hop,
        },
        _ if ratio.is_integer() => GateInput::Response {
            h: resp,
            branches: ratio.up as usize,
            latency: rt.latency_out_frames(),
        },
        _ => return Err("no gate for a rational runtime kind".into()),
    };
    let t = Instant::now();
    let (gate, bytes) = measure(|| run_gate(input));
    memory.gate = bytes;
    memory.gate_ms = t.elapsed().as_millis();
    gates.results.lock().unwrap().insert(built.taps_hash, gate);
    Ok(())
}

fn run_preset(p: FilterPreset, rate: u32, ratio: RateRatio, ctx: &Ctx) -> Outcome {
    let kind = kind_of(p);
    let c2 = preset_check(p, rate, ratio, PIPELINES[0]);
    let c4 = preset_check(p, rate, ratio, PIPELINES[1]);
    let named = Named::preset(p);
    let ((built, records), bytes) =
        measure(|| capture(|| build_alone(&named, rate, ratio, CHANNELS)));
    let mut memory = Memory {
        build: bytes,
        ..Memory::default()
    };
    let b = match &built {
        Ok(_) => "ok",
        Err(e) => refusal_class(p, ratio, e),
    };
    let mut row = Row {
        kind,
        status: status_of(c2, c4),
        paths: format!("{c2},{c4},{b}"),
        ..Row::default()
    };
    let mut failures = Vec::new();
    match built {
        Ok(mut rt) => {
            if rt.ratio() != ratio {
                failures.push(format!("built ratio {} for {ratio}", rt.ratio()));
            }
            let how = Fill {
                kernel_kind: kind == "kernel",
                linear: named.design.filter_phase() == FilterPhase::Linear,
                preset: Some(p),
                ctx,
            };
            if let Err(e) = fill_built(&mut row, &mut memory, rt.as_mut(), &records, how) {
                failures.push(e);
            }
        }
        Err(e) => {
            // What the daemon's rate mask accepts (feasibility with two
            // pipelines; four with A/B, a subset) must build: the build's
            // only other steps, the kernel and the safety gate, do not
            // depend on the pipelines.
            if c2 == "ok" {
                failures.push(format!(
                    "feasibility (the rate mask) accepts it, the build refuses: {e}"
                ));
            }
            if row.status == "ok" {
                row.status = "refused:build".into();
            }
        }
    }
    Outcome {
        row,
        memory,
        failures,
    }
}

fn run_custom(cfg: &UpsamplerConfig, rate: u32, ratio: RateRatio, ctx: &Ctx) -> Outcome {
    let ((built, records), bytes) = measure(|| {
        capture(|| {
            let named = Named::of(cfg).map_err(|e| e.to_string())?;
            build_alone(&named, rate, ratio, CHANNELS).map_err(|e| e.to_string())
        })
    });
    let mut memory = Memory {
        build: bytes,
        ..Memory::default()
    };
    let mut row = Row {
        kind: "kernel",
        status: "ok".into(),
        paths: format!(
            "-,-,{}",
            built.as_ref().map_or_else(|e| message_class(e), |_| "ok")
        ),
        ..Row::default()
    };
    let mut failures = Vec::new();
    match built {
        Ok(mut rt) => {
            let named = Named::of(cfg).unwrap();
            let how = Fill {
                kernel_kind: true,
                linear: named.design.filter_phase() == FilterPhase::Linear,
                preset: None,
                ctx,
            };
            if let Err(e) = fill_built(&mut row, &mut memory, rt.as_mut(), &records, how) {
                failures.push(e);
            }
        }
        Err(_) => row.status = "refused:build".into(),
    }
    Outcome {
        row,
        memory,
        failures,
    }
}

/// The chain config for `inner` at `ratio` from `rate`, with headroom.
fn chain_config(inner: &Design, rate: u32, ratio: RateRatio) -> ChainConfig {
    let mut upsampler = inner.config();
    upsampler.target = match ratio.as_integer() {
        Some(k) => RateTarget::Ratio(k),
        None => RateTarget::Rate(ratio.output_rate(rate).expect("an output rate")),
    };
    let mut config = ChainConfig {
        upsampler,
        ..ChainConfig::neutral()
    };
    config.output.headroom_db = CHAIN_HEADROOM_DB;
    config
}

fn run_chain(inner: &Design, rate: u32, ratio: RateRatio) -> Outcome {
    let config = chain_config(inner, rate, ratio);
    let spec = ChainSpec {
        config: config.clone(),
        sample_rate: rate,
        channels: CHANNELS,
        max_block: MAX_BLOCK,
        irs: None,
    };
    let ((built, records), bytes) = measure(|| capture(|| Chain::try_new(spec)));
    let mut memory = Memory {
        build: bytes,
        ..Memory::default()
    };
    let mut row = Row {
        kind: "chain",
        status: "ok".into(),
        paths: format!("-,-,{}", if built.is_ok() { "ok" } else { "error" }),
        ..Row::default()
    };
    let mut failures = Vec::new();
    let mut chain = match built {
        Ok(chain) => chain,
        Err(e) => {
            row.status = "refused:build".into();
            failures.push(format!("the chain refused it: {e}"));
            return Outcome {
                row,
                memory,
                failures,
            };
        }
    };
    if ratio.is_unity() {
        if chain.upsampler_runtime_mut().is_some() || !records.is_empty() {
            failures.push("a unity ratio built an upsampler".into());
        }
        row.latency_frames = Some(chain.latency_out_frames());
        return Outcome {
            row,
            memory,
            failures,
        };
    }
    let Some(up) = chain.upsampler_runtime_mut() else {
        failures.push("no upsampler".into());
        return Outcome {
            row,
            memory,
            failures,
        };
    };
    let no_gate = Ctx {
        index: 0,
        gates: None,
        share: None,
    };
    let how = Fill {
        kernel_kind: match inner {
            Design::Preset(p) => matrix::is_kernel(*p),
            _ => true,
        },
        linear: false,
        preset: None,
        ctx: &no_gate,
    };
    if let Err(e) = fill_built(&mut row, &mut memory, up, &records, how) {
        failures.push(e);
    }
    // The same runtime built alone, scaled by the expected headroom gain.
    let alone = match inner {
        Design::Preset(p) => {
            build_alone(&Named::preset(*p), rate, ratio, CHANNELS).map_err(|e| e.to_string())
        }
        _ => Named::of(&config.upsampler.with_output(&config.output))
            .map_err(|e: DesignError| e.to_string())
            .and_then(|n| build_alone(&n, rate, ratio, CHANNELS).map_err(|e| e.to_string())),
    };
    match alone {
        Ok(mut rt) => {
            rt.scale_output(db_to_lin(-CHAIN_HEADROOM_DB));
            let block_fft = row.kind == "chain"
                && matches!(inner, Design::Preset(p)
                    if matches!(catalogue::design(*p), FilterDesign::BlockFft(_)));
            let (want, _) = probe(rt.as_mut(), block_fft);
            let got = row.runtime_hash.unwrap_or_default();
            if want != got {
                failures.push(format!(
                    "headroom: the chain's upsampler response ({got:016x}) is not the runtime \
                     scaled by db_to_lin(-{CHAIN_HEADROOM_DB}) ({want:016x})"
                ));
            }
        }
        Err(e) => failures.push(format!("the runtime alone does not build: {e}")),
    }
    Outcome {
        row,
        memory,
        failures,
    }
}

// ------------------------------------------------------------ safety gate

/// The post-build safety gate, report only. Gains are against unity before headroom.
#[derive(Clone, Debug, Default)]
struct Gate {
    branches: usize,
    /// Block positions measured (block FFT), else 1.
    positions: usize,
    finite: bool,
    nonzero: bool,
    /// Largest |DC gain| deviation of a branch from 0 dB.
    dc_dev_db: f64,
    /// Largest sampled branch magnitude, dB.
    boost_sampled_db: f64,
    /// Its guaranteed bound.
    boost_bound_db: f64,
    /// Largest FFT used.
    nfft: usize,
    /// Bounded-input peak: the largest branch L1 norm, dB.
    l1_db: f64,
    /// The kernel's (or response's) first largest |value|.
    peak_at: usize,
    /// The delay the kernel kinds should report: the centre for a linear
    /// kernel (`len / 2`, the integer and rational paths' convention; an
    /// even-length kernel may also report the exact `(len - 1) / 2`), the
    /// peak otherwise (none for runtime kinds).
    expected_delay: Option<f64>,
    /// The other centre an even-length linear kernel may report.
    expected_delay_alt: Option<f64>,
    /// The delay (or latency) the runtime reports.
    delay: f64,
    /// DC tolerance used (0.01 dB, or 0.1 dB for a phase-transformed
    /// kernel).
    dc_tol_db: f64,
}

const MAX_BOOST: f64 = 1.122_018_454_301_963_3; // 10^(1/20), +1 dB
const MAX_GATE_FFT: usize = 1 << 23;

fn db(v: f64) -> f64 {
    20.0 * v.abs().log10()
}

thread_local! {
    static PLANNER: std::cell::RefCell<RealFftPlanner<f64>> =
        std::cell::RefCell::new(RealFftPlanner::new());
}

/// Largest |H(w)| of `h` sampled on `nfft` points.
fn sampled_max(h: &[f64], nfft: usize) -> f64 {
    let fft = PLANNER.with(|p| p.borrow_mut().plan_fft_forward(nfft));
    let mut x = vec![0.0; nfft];
    x[..h.len()].copy_from_slice(h);
    let mut spec = fft.make_output_vec();
    fft.process(&mut x, &mut spec).unwrap();
    spec.iter().map(|c| c.norm()).fold(0.0, f64::max)
}

/// Fold one branch (or response phase) into `g`.
fn gate_branch(g: &mut Gate, h: &[f64]) {
    let dc: f64 = h.iter().sum();
    g.dc_dev_db = g.dc_dev_db.max(db(dc).abs());
    let l1: f64 = h.iter().map(|v| v.abs()).sum();
    g.l1_db = g.l1_db.max(db(l1));
    // Trim zeros at both ends: they do not change |H|.
    let Some(first) = h.iter().position(|&v| v != 0.0) else {
        return;
    };
    let last = h.iter().rposition(|&v| v != 0.0).unwrap();
    let h = &h[first..=last];
    // Around the weighted median (any centre is valid; this one makes the
    // first moment smallest).
    let half = l1 / 2.0;
    let mut acc = 0.0;
    let c = h
        .iter()
        .position(|v| {
            acc += v.abs();
            acc >= half
        })
        .unwrap_or(0) as f64;
    let (m1, m2) = h.iter().enumerate().fold((0.0, 0.0), |(m1, m2), (j, v)| {
        let d = (j as f64 - c).abs();
        (m1 + d * v.abs(), m2 + d * d * v.abs())
    });
    let mut nfft = (8 * h.len()).max(1 << 16).next_power_of_two();
    loop {
        let max = sampled_max(h, nfft);
        // Between two bins Δω = 2π/nfft apart, |H| exceeds the larger
        // sampled value by at most (Δω/2) Σ|j - c||h_j| (first derivative)
        // and by at most (Δω²/8) Σ(j - c)²|h_j| (the error of the complex
        // linear interpolation, whose magnitude stays below the larger end).
        // Both are guaranteed; the second one shrinks faster with nfft.
        let dw = 2.0 * std::f64::consts::PI / nfft as f64;
        let bound = max + (dw / 2.0 * m1).min(dw * dw / 8.0 * m2);
        if bound <= MAX_BOOST || max > MAX_BOOST || nfft >= MAX_GATE_FFT {
            g.boost_sampled_db = g.boost_sampled_db.max(db(max));
            g.boost_bound_db = g.boost_bound_db.max(db(bound));
            g.nfft = g.nfft.max(nfft);
            return;
        }
        nfft *= 2;
    }
}

fn new_gate(branches: usize) -> Gate {
    Gate {
        branches,
        positions: 1,
        finite: true,
        nonzero: false,
        dc_dev_db: 0.0,
        boost_sampled_db: f64::NEG_INFINITY,
        boost_bound_db: f64::NEG_INFINITY,
        nfft: 0,
        l1_db: f64::NEG_INFINITY,
        peak_at: 0,
        expected_delay: None,
        expected_delay_alt: None,
        delay: 0.0,
        dc_tol_db: 0.01,
    }
}

/// Index of the first largest |value|.
fn peak_index(h: &[f64]) -> usize {
    let mut best = 0;
    for (i, v) in h.iter().enumerate() {
        if v.abs() > h[best].abs() {
            best = i;
        }
    }
    best
}

/// The gate over the `branches` polyphase branches of `h`.
fn gate_branches(g: &mut Gate, h: &[f64], branches: usize) {
    g.finite &= h.iter().all(|v| v.is_finite());
    g.nonzero |= h.iter().any(|&v| v != 0.0);
    if !g.finite {
        return;
    }
    let mut branch = Vec::with_capacity(h.len() / branches + 1);
    for b in 0..branches {
        branch.clear();
        branch.extend(h.iter().skip(b).step_by(branches));
        gate_branch(g, &branch);
    }
}

fn run_gate(input: GateInput) -> Gate {
    match input {
        GateInput::Kernel {
            h,
            branches,
            delay,
            linear,
        } => {
            let mut g = new_gate(branches);
            g.dc_tol_db = if linear { 0.01 } else { 0.1 };
            gate_branches(&mut g, &h, branches);
            g.peak_at = peak_index(&h);
            g.expected_delay = Some(if linear {
                (h.len() / 2) as f64
            } else {
                g.peak_at as f64
            });
            if linear && h.len() % 2 == 0 {
                g.expected_delay_alt = Some((h.len() as f64 - 1.0) / 2.0);
            }
            g.delay = delay;
            g
        }
        GateInput::Response {
            h,
            branches,
            latency,
        } => {
            let mut g = new_gate(branches);
            gate_branches(&mut g, &h, branches);
            g.peak_at = peak_index(&h);
            g.delay = latency as f64;
            g
        }
        GateInput::BlockFft {
            preset,
            ratio,
            b,
            hop,
        } => {
            let r = RateRatio::integer(ratio as u32);
            let mut rt = build_alone(&Named::preset(preset), 44_100, r, 1).unwrap();
            let span = rt.impulse_span_out_frames();
            // Every position of one hop, or for B > 2^12 256 evenly spaced
            // ones plus both hop edges.
            let positions: Vec<usize> = if b <= 1 << 12 {
                (0..hop).collect()
            } else {
                let mut v: Vec<usize> = (0..256).map(|i| i * hop / 256).collect();
                v.push(hop - 1);
                v.dedup();
                v
            };
            let mut g = new_gate(ratio);
            g.positions = positions.len();
            let mut peak0 = 0;
            for (i, &at) in positions.iter().enumerate() {
                rt.reset();
                let frames = at + span.div_ceil(ratio) + 1;
                let out = run_impulses(rt.as_mut(), 1, &[(0, at, 1.0)], frames)
                    .pop()
                    .unwrap();
                // The response from the impulse on, over the span.
                let start = at * ratio;
                let h = &out[start..(start + span).min(out.len())];
                gate_branches(&mut g, h, ratio);
                if i == 0 {
                    peak0 = peak_index(h);
                }
            }
            g.peak_at = peak0;
            g.delay = rt.latency_out_frames() as f64;
            g
        }
    }
}

impl Gate {
    fn dc_ok(&self) -> bool {
        self.dc_dev_db <= self.dc_tol_db
    }

    fn boost_ok(&self) -> bool {
        self.boost_bound_db <= db(MAX_BOOST)
    }

    fn delay_ok(&self) -> bool {
        self.expected_delay.is_none_or(|e| e == self.delay)
            || self.expected_delay_alt == Some(self.delay)
    }

    fn columns(&self) -> Vec<String> {
        let f = |v: f64| format!("{v:e}");
        vec![
            self.branches.to_string(),
            self.positions.to_string(),
            u8::from(self.finite).to_string(),
            u8::from(self.nonzero).to_string(),
            f(self.dc_dev_db),
            f(self.dc_tol_db),
            u8::from(self.dc_ok()).to_string(),
            f(self.boost_sampled_db),
            f(self.boost_bound_db),
            u8::from(self.boost_ok()).to_string(),
            self.nfft.to_string(),
            f(self.l1_db),
            self.peak_at.to_string(),
            self.expected_delay
                .map_or_else(|| "-".to_string(), |v| format!("{v}")),
            format!("{}", self.delay),
            u8::from(self.delay_ok()).to_string(),
        ]
    }

    /// The checks this construction fails, with the measured value (its
    /// legacy exemptions).
    fn failed(&self) -> Vec<(&'static str, String)> {
        let mut v = Vec::new();
        if !self.finite {
            v.push(("finite", "0".into()));
        }
        if !self.nonzero {
            v.push(("nonzero", "0".into()));
        }
        if self.finite && !self.dc_ok() {
            v.push(("dc", format!("{:e}", self.dc_dev_db)));
        }
        if self.finite && !self.boost_ok() {
            v.push(("boost", format!("{:e}", self.boost_bound_db)));
        }
        if !self.delay_ok() {
            v.push((
                "delay",
                format!("{} {}", self.delay, self.expected_delay.unwrap_or_default()),
            ));
        }
        v
    }
}

const GATE_COLUMNS: &str = "taps_hash\tbranches\tpositions\tfinite\tnonzero\tdc_dev_db\tdc_tol_db\t\
dc_ok\tboost_sampled_db\tboost_bound_db\tboost_ok\tnfft\tl1_db\tpeak_at\texpected_delay\tdelay\t\
delay_ok\tfirst_case";

// ------------------------------------------------------------ environment

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn libc_version() -> String {
    unsafe extern "C" {
        fn gnu_get_libc_version() -> *const std::ffi::c_char;
    }
    // SAFETY: glibc returns a static NUL-terminated string.
    let v = unsafe { std::ffi::CStr::from_ptr(gnu_get_libc_version()) };
    format!("glibc {}", v.to_string_lossy())
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn libc_version() -> String {
    format!("{} {}", std::env::consts::OS, std::env::consts::ARCH)
}

/// CPU features detected at run time (the FFT and dot-product kernels
/// pick their code paths from them).
fn cpu_features() -> String {
    #[cfg(target_arch = "x86_64")]
    {
        let mut v = Vec::new();
        macro_rules! feat {
            ($($f:tt),*) => {$(
                if std::arch::is_x86_feature_detected!($f) {
                    v.push($f);
                }
            )*};
        }
        feat!("sse4.1", "avx", "avx2", "fma", "avx512f");
        v.join(" ")
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        std::env::consts::ARCH.to_string()
    }
}

/// The lines that must match between a fixture and this run: everything
/// the bits can depend on besides the code. The compiler, target and
/// crate versions were recorded when dsp-core was built (its build
/// script), so they do not depend on the runner's tools or PATH.
fn environment() -> Vec<String> {
    vec![
        format!("rustc: {}", env!("AVDSP_BUILD_RUSTC")),
        format!("rustc commit: {}", env!("AVDSP_BUILD_RUSTC_COMMIT")),
        format!("llvm: {}", env!("AVDSP_BUILD_LLVM")),
        format!("target: {}", env!("AVDSP_BUILD_TARGET")),
        format!("target features: {}", env!("AVDSP_BUILD_TARGET_FEATURES")),
        format!("rustfft: {}", env!("AVDSP_BUILD_RUSTFFT")),
        format!("realfft: {}", env!("AVDSP_BUILD_REALFFT")),
        format!("libc: {}", libc_version()),
        format!("cpu: {}", cpu_features()),
    ]
}

/// Informational lines: how the fixture was captured.
fn provenance() -> Vec<String> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    let commit = git(&["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let dirty = git(&[
        "status",
        "--porcelain",
        "--untracked-files=no",
        "--",
        ".",
        ":(exclude)crates/dsp-core/tests/fixtures",
    ])
    .is_some_and(|s| !s.is_empty());
    vec![
        format!("commit: {commit}{}", if dirty { " (modified)" } else { "" }),
        format!(
            "profile: {} (opt-level {}, debug assertions {})",
            env!("AVDSP_BUILD_PROFILE"),
            env!("AVDSP_BUILD_OPT_LEVEL"),
            if cfg!(debug_assertions) { "on" } else { "off" }
        ),
        format!("runtime: {CHANNELS} channels, at most {MAX_BLOCK} frames per call"),
    ]
}

/// `tests/fixtures`, or `AVDSP_FINGERPRINT_DIR` (for trial runs).
fn fixtures_dir() -> PathBuf {
    std::env::var_os("AVDSP_FINGERPRINT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures"))
}

/// Where the reports that are not committed go.
fn artifacts_dir() -> PathBuf {
    std::env::var_os("AVDSP_FINGERPRINT_ARTIFACTS")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("filter-fingerprints"))
}

// ------------------------------------------------------------ pool

fn mem_available() -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("MemAvailable:"))
                .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
        })
        .map_or(8 << 30, |kib| kib * 1024)
}

fn proc_status_kib(key: &str) -> Option<u64> {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix(key))
                .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
        })
}

/// Peak resident set of `f` above the start (resets the process peak;
/// meaningful only when nothing else runs).
fn measure_rss<T>(f: impl FnOnce() -> T) -> (T, Option<u64>) {
    let reset = std::fs::write("/proc/self/clear_refs", "5").is_ok();
    let before = proc_status_kib("VmRSS:");
    let out = f();
    let peak = proc_status_kib("VmHWM:");
    let rss = match (reset, before, peak) {
        (true, Some(b), Some(p)) => Some(p.saturating_sub(b) * 1024),
        _ => None,
    };
    (out, rss)
}

/// The FFT size the gate starts with for a branch of `len` taps.
fn gate_fft(len: u64) -> u64 {
    (8 * len)
        .max(1 << 16)
        .next_power_of_two()
        .min(MAX_GATE_FFT as u64)
}

/// Upper estimate of a case's peak memory, calibrated on the memory
/// tables (measured peaks are 0.3 to 0.8 of it): construction (the
/// cepstral transform about 40 bytes per FFT point, a linear design and
/// its runtime about 48 bytes per tap), and the gate.
fn estimate(case: &Case, gate: bool) -> u64 {
    match &case.design {
        Design::Custom(..) => 64 << 20,
        Design::Chain(_) if case.ratio.is_unity() => 1 << 20,
        Design::Chain(inner) => match &**inner {
            Design::Preset(p) => {
                2 * estimate(&Case::preset(*p, case.source_rate, case.ratio), false)
            }
            _ => 128 << 20,
        },
        Design::Preset(p) => match catalogue::design(*p) {
            FilterDesign::Kernel(_) => {
                let cx = context(case.source_rate, case.ratio, PIPELINES[0], CHANNELS);
                let Ok(plan) = design::feasibility(&catalogue::design(*p), &cx) else {
                    return 1 << 20;
                };
                let n = plan.grid_taps;
                // Refused before anything is designed.
                if preset_check(*p, case.source_rate, case.ratio, PIPELINES[0]) != "ok" {
                    return 1 << 20;
                }
                let n = n as u64;
                let mut est = 64 * n + (16 << 20);
                match catalogue::design(*p).filter_phase() {
                    FilterPhase::Linear => {}
                    phase => {
                        let design = if phase == FilterPhase::Intermediate {
                            n / 4
                        } else {
                            n
                        };
                        est += 44 * design::phase::cepstral::fft_len(design as usize) as u64;
                    }
                }
                if gate {
                    est += 8 * n + 24 * gate_fft(n / u64::from(case.ratio.up));
                }
                est
            }
            FilterDesign::BlockFft(_) => {
                // The gate's FFTs of the branches of one block position.
                let b = if *p == FilterPreset::BlockFftLarge {
                    (1u64 << 20) / u64::from(case.ratio.up)
                } else {
                    1 << 14
                };
                (160 << 20) + if gate { 64 * gate_fft(b) } else { 0 }
            }
            _ => 64 << 20,
        },
    }
}

struct Admission {
    budget: u64,
    state: Mutex<(u64, usize, bool)>, // (bytes in flight, cases running, solo running)
    cv: Condvar,
}

/// A place in the pool, given back when dropped (also when the case
/// panics).
struct Admitted<'a> {
    admission: &'a Admission,
    bytes: u64,
}

impl Admission {
    fn enter(&self, bytes: u64, solo: bool) -> Admitted<'_> {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            let (used, running, solo_running) = *s;
            let fits = if solo {
                running == 0
            } else {
                !solo_running && (running == 0 || used + bytes <= self.budget)
            };
            if fits {
                *s = (used + bytes, running + 1, solo);
                return Admitted {
                    admission: self,
                    bytes,
                };
            }
            s = self.cv.wait(s).unwrap_or_else(|e| e.into_inner());
        }
    }
}

impl Drop for Admitted<'_> {
    fn drop(&mut self) {
        let a = self.admission;
        let mut s = a.state.lock().unwrap_or_else(|e| e.into_inner());
        *s = (s.0 - self.bytes, s.1 - 1, false);
        a.cv.notify_all();
    }
}

struct Ran {
    row: Row,
    memory: Memory,
}

fn panic_message(e: &(dyn std::any::Any + Send)) -> String {
    e.downcast_ref::<String>()
        .cloned()
        .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_else(|| "panic".into())
}

/// The results of a pool run.
struct PoolRun {
    ran: Vec<Option<Ran>>,
    /// Gate results with the first case that reached them.
    gates: BTreeMap<u64, (Gate, String)>,
    /// A panicking case, or a case's own failed checks, named by case.
    failures: Vec<String>,
}

/// Run `cases` on a memory-bounded pool.
fn run_all(cases: &[Case], gate: bool) -> PoolRun {
    // The machine's cores, at most 16: more only wait on the memory admission.
    let default_threads = std::thread::available_parallelism().map_or(8, |n| n.get().min(16));
    let threads = std::env::var("AVDSP_FINGERPRINT_THREADS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default_threads)
        .clamp(1, 16);
    let budget = mem_available() / 2;
    let estimates: Vec<u64> = cases.iter().map(|c| estimate(c, gate)).collect();
    // Alone: above 1 GiB; with AVDSP_FINGERPRINT_RSS=1, also a
    // construction above 256 MiB (the gate runs once per construction, so
    // its share does not count).
    let rss_mode = std::env::var("AVDSP_FINGERPRINT_RSS").is_ok_and(|v| v == "1");
    let solo: Vec<bool> = cases
        .iter()
        .zip(&estimates)
        .map(|(c, &e)| e > SOLO_BYTES || (rss_mode && estimate(c, false) > RSS_BYTES))
        .collect();
    // The shared cases first, then the solo ones, so a solo case never
    // holds the pool back while it has other work.
    let mut order: Vec<usize> = (0..cases.len()).collect();
    order.sort_by_key(|&i| (solo[i], i));
    eprintln!(
        "{} cases, {threads} threads, memory budget {} MiB, {} run alone",
        cases.len(),
        budget >> 20,
        solo.iter().filter(|&&s| s).count()
    );
    let admission = Admission {
        budget,
        state: Mutex::new((0, 0, false)),
        cv: Condvar::new(),
    };
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let results: Vec<Mutex<Option<Ran>>> = cases.iter().map(|_| Mutex::new(None)).collect();
    let failures: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let gates = Gates::default();
    let share = ProbeShare::default();
    let start = Instant::now();
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                while let Some(&i) = order.get(next.fetch_add(1, Ordering::SeqCst)) {
                    let case = &cases[i];
                    let key = case.key().replace('\t', " ");
                    let solo = solo[i];
                    let _admitted = admission.enter(estimates[i], solo);
                    let ctx = Ctx {
                        index: i,
                        gates: gate.then_some(&gates),
                        share: Some(&share),
                    };
                    let t0 = Instant::now();
                    let run = || {
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            measure(|| run_case(case, &ctx))
                        }))
                    };
                    let (outcome, rss) = if solo {
                        measure_rss(run)
                    } else {
                        (run(), None)
                    };
                    let mut failed = failures.lock().unwrap_or_else(|e| e.into_inner());
                    match outcome {
                        Ok((out, total)) => {
                            let mut memory = out.memory;
                            memory.total = total;
                            memory.rss = rss;
                            memory.ms = t0.elapsed().as_millis();
                            memory.estimate = estimates[i];
                            failed.extend(out.failures.into_iter().map(|f| format!("{key}: {f}")));
                            *results[i].lock().unwrap() = Some(Ran {
                                row: out.row,
                                memory,
                            });
                        }
                        Err(e) => failed.push(format!("{key}: panicked: {}", panic_message(&*e))),
                    }
                    drop(failed);
                    let n = done.fetch_add(1, Ordering::SeqCst) + 1;
                    if n.is_multiple_of(1000) || n == cases.len() {
                        eprintln!(
                            "  {n}/{} cases, {:.0} s",
                            cases.len(),
                            start.elapsed().as_secs_f64()
                        );
                    }
                }
            });
        }
    });
    let first = gates.first.into_inner().unwrap_or_else(|e| e.into_inner());
    PoolRun {
        ran: results
            .into_iter()
            .map(|m| m.into_inner().unwrap_or_else(|e| e.into_inner()))
            .collect(),
        gates: gates
            .results
            .into_inner()
            .unwrap_or_else(|e| e.into_inner())
            .into_iter()
            .map(|(h, g)| (h, (g, cases[first[&h]].key().replace('\t', " "))))
            .collect(),
        failures: failures.into_inner().unwrap_or_else(|e| e.into_inner()),
    }
}

// ------------------------------------------------------------ fixtures

const COLUMNS: &str = "name\tsource_rate\tratio\tkind\tstatus\tpaths\ttaps_len\ttaps_hash\t\
kernel_delay\tlatency_frames\tlatency_exact\truntime_hash";

const FIXTURE: &str = "filter-fingerprints.tsv";
const EXEMPTIONS: &str = "filter-safety-exemptions.tsv";

struct Table {
    env: Vec<String>,
    /// key (the first `key_columns` columns) -> the rest of the line.
    rows: BTreeMap<String, String>,
}

/// Read a table whose keys are the first `key_columns` tab-separated
/// columns.
fn read_table(path: &Path, key_columns: usize) -> Option<Table> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut t = Table {
        env: Vec::new(),
        rows: BTreeMap::new(),
    };
    for line in text.lines() {
        if let Some(e) = line.strip_prefix("# env ") {
            t.env.push(e.to_string());
        } else if !line.starts_with('#') && !line.is_empty() {
            let parts: Vec<&str> = line.splitn(key_columns + 1, '\t').collect();
            let key = parts[..key_columns.min(parts.len())].join("\t");
            let rest = parts.get(key_columns).copied().unwrap_or("").to_string();
            t.rows.insert(key, rest);
        }
    }
    Some(t)
}

fn write_mode() -> bool {
    std::env::var("AVDSP_FINGERPRINT_WRITE").is_ok_and(|v| v == "1")
}

/// The selected subset of `cases` (chunk and name filters), and whether a
/// filter is active.
fn select(cases: Vec<Case>) -> (Vec<Case>, bool) {
    let chunk = std::env::var("AVDSP_FINGERPRINT_CHUNK").ok().map(|v| {
        let (k, n) = v.split_once('/').expect("AVDSP_FINGERPRINT_CHUNK=k/n");
        (
            k.parse::<usize>().expect("chunk k"),
            n.parse::<usize>().expect("chunk n"),
        )
    });
    let only: Option<Vec<String>> = std::env::var("AVDSP_FINGERPRINT_ONLY")
        .ok()
        .map(|v| v.split(',').map(str::to_string).collect());
    let filtered = chunk.is_some() || only.is_some();
    let cases = cases
        .into_iter()
        .enumerate()
        .filter(|(i, _)| chunk.is_none_or(|(k, n)| i % n == k))
        .filter(|(_, c)| only.as_ref().is_none_or(|o| o.contains(&c.name())))
        .map(|(_, c)| c)
        .collect();
    (cases, filtered)
}

/// Every key of both tiers, in fixture order (tier B's, then tier A's
/// own).
fn all_keys() -> Vec<String> {
    let mut seen = HashSet::new();
    matrix::tier_b()
        .iter()
        .chain(matrix::tier_a().iter())
        .map(Case::key)
        .filter(|k| seen.insert(k.clone()))
        .collect()
}

/// Write `rows` into the table at `path`: rows for the same keys are
/// replaced, earlier rows whose keys `drop` returns true are removed, the
/// others stay. Rows are written in `order`, then any others sorted.
fn merge_table(
    path: &Path,
    header: &[String],
    key_columns: usize,
    rows: BTreeMap<String, String>,
    drop: impl Fn(&str) -> bool,
    order: Option<&[String]>,
) {
    let mut merged: BTreeMap<String, String> = read_table(path, key_columns)
        .map(|t| t.rows)
        .unwrap_or_default();
    merged.retain(|k, _| !drop(k));
    merged.extend(rows);
    let mut text = String::new();
    for h in header {
        let _ = writeln!(text, "{h}");
    }
    for key in order.unwrap_or_default() {
        if let Some(rest) = merged.remove(key) {
            let _ = writeln!(text, "{key}\t{rest}");
        }
    }
    for (key, rest) in merged {
        let _ = writeln!(text, "{key}\t{rest}");
    }
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// An exclusive lock on the fixture file while a run reads, merges and
/// writes it and the exemptions (rewritten in place, so the lock stays on
/// the same file); fails when another writer holds it.
fn lock_fixtures(path: &Path) -> std::fs::File {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    if f.try_lock().is_err() {
        panic!(
            "another fingerprint writer holds {} (writes are serialised)",
            path.display()
        );
    }
    f
}

/// The differences between the fixture's env lines and this run's.
fn env_diff(fixture: &[String], env: &[String]) -> Option<String> {
    if fixture == env {
        return None;
    }
    let mut msg = String::new();
    for i in 0..fixture.len().max(env.len()) {
        let (a, b) = (fixture.get(i), env.get(i));
        if a != b {
            let _ = writeln!(
                msg,
                "  fixture:  {}\n  this run: {}",
                a.map_or("(none)", |s| s.as_str()),
                b.map_or("(none)", |s| s.as_str())
            );
        }
    }
    Some(msg)
}

fn run_tier(tier: &str, all: Vec<Case>, gate: bool) {
    let (cases, filtered) = select(all);
    let env = environment();
    let fixture_path = fixtures_dir().join(FIXTURE);
    let write = write_mode();
    let _lock = write.then(|| lock_fixtures(&fixture_path));
    let existing = read_table(&fixture_path, 3);
    match &existing {
        None if !write => panic!(
            "no fixture at {} (capture one with AVDSP_FINGERPRINT_WRITE=1)",
            fixture_path.display()
        ),
        Some(fx) if !fx.rows.is_empty() => {
            if let Some(msg) = env_diff(&fx.env, &env) {
                if write {
                    panic!(
                        "refusing to merge into a fixture captured on a different environment \
                         (capture into an empty AVDSP_FINGERPRINT_DIR, then replace it):\n{msg}"
                    );
                }
                panic!("fixture captured on a different environment:\n{msg}");
            }
        }
        _ => {}
    }

    let start = Instant::now();
    let PoolRun {
        ran,
        gates,
        mut failures,
    } = run_all(&cases, gate);
    let secs = start.elapsed().as_secs_f64();

    let mut rows = BTreeMap::new();
    let mut statuses: BTreeMap<String, usize> = BTreeMap::new();
    for (c, r) in cases.iter().zip(&ran) {
        if let Some(r) = r {
            rows.insert(c.key(), r.row.columns());
            *statuses.entry(r.row.status.clone()).or_default() += 1;
        }
    }
    println!(
        "tier {tier}: {} cases in {secs:.1} s; status counts: {statuses:?}",
        cases.len()
    );
    if tier == "a" {
        for &(name, input, output) in COST4_PICKS {
            let r = RateRatio::from_rates(input, output);
            let key = format!("{name}\t{input}\t{}/{}", r.up, r.down);
            if let Some(rest) = rows.get(&key)
                && !rest.contains("refused:cost4")
            {
                failures.push(format!("{key}: picked as a cost4 case but got {rest}"));
            }
        }
    }
    report_memory(tier, &cases, &ran, filtered);
    if gate {
        write_gate_report(tier, &gates, filtered);
    }
    if !failures.is_empty() {
        for f in &failures {
            println!("FAILED {f}");
        }
        panic!(
            "{} cases failed in tier {tier} (listed above); nothing written or compared",
            failures.len()
        );
    }

    if write {
        let mut header = vec![
            "# Audivane filter fingerprints (tiers A and B). See tests/filter_fingerprints.rs."
                .to_string(),
            "# `# env` lines must match the build that compares; `# info` lines say how this \
             file was captured."
                .to_string(),
        ];
        header.extend(env.iter().map(|e| format!("# env {e}")));
        header.extend(provenance().iter().map(|i| format!("# info {i}")));
        header.push(format!("# {COLUMNS}"));
        let keys = all_keys();
        let known: HashSet<&str> = keys.iter().map(String::as_str).collect();
        merge_table(
            &fixture_path,
            &header,
            3,
            rows,
            |k| !known.contains(k),
            Some(&keys),
        );
        println!("wrote {}", fixture_path.display());
        if gate {
            write_exemptions(&gates, &env);
        }
        return;
    }

    // Compare.
    let fx = existing.unwrap();
    let mut diffs = Vec::new();
    let names: Vec<&str> = COLUMNS.split('\t').collect();
    for c in &cases {
        let key = c.key();
        let Some(got) = rows.get(&key) else { continue };
        let shown = key.replace('\t', " ");
        match fx.rows.get(&key) {
            None => diffs.push(format!("{shown}: not in the fixture")),
            Some(want) if want != got => {
                let w: Vec<&str> = want.split('\t').collect();
                let g: Vec<&str> = got.split('\t').collect();
                for (i, name) in names[3..].iter().enumerate() {
                    let (a, b) = (w.get(i).unwrap_or(&""), g.get(i).unwrap_or(&""));
                    if a != b {
                        diffs.push(format!("{shown}: {name} {a} -> {b}"));
                    }
                }
            }
            Some(_) => {}
        }
    }
    if !filtered {
        let keys: HashSet<String> = all_keys().into_iter().collect();
        for key in fx.rows.keys() {
            if !keys.contains(key) {
                diffs.push(format!(
                    "{}: in the fixture, not in the matrix",
                    key.replace('\t', " ")
                ));
            }
        }
    }
    if gate {
        diffs.extend(compare_exemptions(&gates, &env));
        diffs.extend(uncovered(&gates));
    }
    if !diffs.is_empty() {
        for d in &diffs {
            println!("DIFF {d}");
        }
        panic!(
            "{} fingerprint differences in tier {tier} (listed above)",
            diffs.len()
        );
    }
    println!("tier {tier}: {} cases match the fixture", cases.len());
}

/// The per-case memory and time table (an artifact), and the largest
/// cases.
fn report_memory(tier: &str, cases: &[Case], ran: &[Option<Ran>], filtered: bool) {
    let mut top: Vec<(&Case, &Memory)> = cases
        .iter()
        .zip(ran)
        .filter_map(|(c, r)| r.as_ref().map(|r| (c, &r.memory)))
        .collect();
    top.sort_by_key(|(_, m)| std::cmp::Reverse(m.total.max(m.rss.unwrap_or(0))));
    for (c, m) in top.iter().take(5) {
        println!(
            "  peak heap {:>5} MiB (build {} / run {} / gate {} MiB), rss {}, {:.1} s: {}",
            m.total >> 20,
            m.build >> 20,
            m.run >> 20,
            m.gate >> 20,
            m.rss
                .map_or_else(|| "-".to_string(), |v| format!("{} MiB", v >> 20)),
            m.ms as f64 / 1e3,
            c.key().replace('\t', " ")
        );
    }
    let rows = cases
        .iter()
        .zip(ran)
        .filter_map(|(c, r)| {
            let m = &r.as_ref()?.memory;
            Some((
                c.key(),
                format!(
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                    m.estimate,
                    m.build,
                    m.run,
                    m.gate,
                    m.total,
                    m.rss.map_or_else(|| "-".to_string(), |v| v.to_string()),
                    m.ms,
                    m.run_ms,
                    m.gate_ms
                ),
            ))
        })
        .collect();
    let header = vec![
        format!(
            "# Peak heap per case, tier {} (bytes above the case's start on its worker \
             thread): the pool's estimate; construction, impulse run, safety gate, whole case; \
             peak resident set of a case that ran alone; wall time of the case, its impulse \
             run and its gate. Measured, not compared.",
            tier.to_uppercase()
        ),
        "# name\tsource_rate\tratio\testimate\tbuild\trun\tgate\ttotal\trss\tms\trun_ms\tgate_ms"
            .to_string(),
    ];
    let path = artifacts_dir().join(format!("filter-memory-{tier}.tsv"));
    merge_table(&path, &header, 3, rows, |_| !filtered, None);
    println!("memory table: {}", path.display());
}

fn write_gate_report(tier: &str, gates: &BTreeMap<u64, (Gate, String)>, filtered: bool) {
    let path = artifacts_dir().join(format!("filter-safety-{tier}.tsv"));
    let rows: BTreeMap<String, String> = gates
        .iter()
        .map(|(h, (g, first))| {
            (
                format!("{h:016x}"),
                format!("{}\t{first}", g.columns().join("\t")),
            )
        })
        .collect();
    let header = vec![
        format!(
            "# The post-build safety gate over every distinct construction of tier \
             {}, report only: DC per polyphase branch (0.01 dB, 0.1 dB for phase-transformed \
             kernels), the largest branch magnitude with its guaranteed bound (+1 dB limit), \
             the bounded-input peak (L1), and the reported delay against the kernel's centre \
             (linear) or peak. Keyed by taps_hash.",
            tier.to_uppercase()
        ),
        format!("# {GATE_COLUMNS}"),
    ];
    merge_table(&path, &header, 1, rows, |_| !filtered, None);
    println!("safety report: {}", path.display());
}

/// The exemption rows of `gates`: "hash\tcheck" -> (value, first case).
fn exemption_rows(gates: &BTreeMap<u64, (Gate, String)>) -> BTreeMap<String, (String, String)> {
    let mut rows = BTreeMap::new();
    for (h, (g, first)) in gates {
        for (check, value) in g.failed() {
            rows.insert(format!("{h:016x}\t{check}"), (value, first.clone()));
        }
    }
    rows
}

/// Whether an exemption key ("hash\tcheck") belongs to a hash gated in
/// this run.
fn gated_key(gates: &BTreeMap<u64, (Gate, String)>, key: &str) -> bool {
    key.split('\t')
        .next()
        .and_then(|h| u64::from_str_radix(h, 16).ok())
        .is_some_and(|h| gates.contains_key(&h))
}

fn write_exemptions(gates: &BTreeMap<u64, (Gate, String)>, env: &[String]) {
    let path = fixtures_dir().join(EXEMPTIONS);
    let rows = exemption_rows(gates)
        .into_iter()
        .map(|(k, (v, first))| (k, format!("{v}\t{first}")))
        .collect();
    let mut header = vec![
        "# Legacy exemptions of the post-build safety gate: every check a tier B \
         construction fails today, keyed by taps_hash, with the measured value and the \
         first case that builds it. Compared by tier B (hash, check and value); the full \
         report is an artifact."
            .to_string(),
    ];
    header.extend(env.iter().map(|e| format!("# env {e}")));
    header.push("# taps_hash\tcheck\tvalue\tfirst_case".to_string());
    // The rows of every hash gated in this run are replaced, also when it
    // passes now.
    merge_table(&path, &header, 2, rows, |k| gated_key(gates, k), None);
    println!("wrote {}", path.display());
}

/// The exemptions of the hashes gated in this run must be the committed
/// ones (the example case is not compared).
fn compare_exemptions(gates: &BTreeMap<u64, (Gate, String)>, env: &[String]) -> Vec<String> {
    let Some(file) = read_table(&fixtures_dir().join(EXEMPTIONS), 2) else {
        return vec!["no safety exemptions file".into()];
    };
    if let Some(msg) = env_diff(&file.env, env) {
        return vec![format!(
            "exemptions captured on a different environment:\n{msg}"
        )];
    }
    let want: BTreeMap<&String, &str> = file
        .rows
        .iter()
        .filter(|(k, _)| gated_key(gates, k))
        .map(|(k, v)| (k, v.split('\t').next().unwrap_or("")))
        .collect();
    let got = exemption_rows(gates);
    let mut diffs = Vec::new();
    for (k, (v, first)) in &got {
        let shown = k.replace('\t', " ");
        match want.get(k) {
            None => diffs.push(format!(
                "safety {shown} ({first}): fails, not exempted: {v}"
            )),
            Some(w) if w != v => diffs.push(format!("safety {shown} ({first}): {w} -> {v}")),
            Some(_) => {}
        }
    }
    for k in want.keys() {
        if !got.contains_key(*k) {
            diffs.push(format!("safety {}: passes now", k.replace('\t', " ")));
        }
    }
    diffs
}

/// The checks the gated constructions fail that the library's legacy
/// exemptions (`design::safety::EXEMPTIONS`: catalogue preset, kind of
/// ratio, check, bound) do not cover. A Custom design's DC, boost and
/// delay are measured only.
fn uncovered(gates: &BTreeMap<u64, (Gate, String)>) -> Vec<String> {
    let mut out = Vec::new();
    for (h, (g, first)) in gates {
        let mut words = first.split(' ');
        let name = words.next().unwrap_or("");
        let ratio = words.nth(1).and_then(|r| {
            let (up, down) = r.split_once('/')?;
            Some(RateRatio::new(up.parse().ok()?, down.parse().ok()?))
        });
        for (check, value) in g.failed() {
            let custom = name.starts_with("custom:") && !matches!(check, "finite" | "nonzero");
            let covered = FilterPreset::from_name(name)
                .zip(ratio)
                .zip(value.parse::<f64>().ok())
                .is_some_and(|((p, r), v)| {
                    design::safety::EXEMPTIONS.iter().any(|e| {
                        e.preset == p
                            && e.integer == r.is_integer()
                            && e.check.name() == check
                            && v <= e.max_db
                    })
                });
            if !custom && !covered {
                out.push(format!(
                    "safety {h:016x} {check} ({first}): {value}, no legacy exemption covers it"
                ));
            }
        }
    }
    out
}

// ------------------------------------------------------------ tests

/// The rational probe reaches every tap: changing any one tap of a
/// rational kernel changes the runtime hash.
#[test]
fn rational_probe_reaches_every_tap() {
    use audivane_dsp_core::rational::RationalResampler;
    for (l, m) in [(5u32, 3u32), (3, 8), (160, 147)] {
        let ratio = RateRatio::new(l, m);
        let n = 4 * l.max(m) as usize + 3;
        let kernel: Vec<f64> = (0..n).map(|i| 1.0 / (1.0 + i as f64)).collect();
        let hash = |k: &[f64]| {
            let mut rt =
                RationalResampler::from_kernel(k, ratio, (n / 2) as f64, CHANNELS, MAX_BLOCK);
            probe(&mut rt, false).0
        };
        let base = hash(&kernel);
        let mut seen = HashSet::from([base]);
        for i in 0..n {
            let mut k = kernel.clone();
            k[i] += 0.25;
            assert!(seen.insert(hash(&k)), "{l}/{m}: tap {i} is not in the hash");
        }
    }
}

/// The pool gives a place back when its case panics.
#[test]
fn admission_released_on_unwind() {
    let a = Admission {
        budget: 10,
        state: Mutex::new((0, 0, false)),
        cv: Condvar::new(),
    };
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _p = a.enter(5, true);
        panic!("case");
    }));
    assert!(r.is_err());
    assert_eq!(*a.state.lock().unwrap(), (0, 0, false));
}

#[test]
fn tier_a_fingerprints() {
    run_tier("a", matrix::tier_a(), false);
}

#[test]
#[ignore = "tier B: about half an hour; run before and after every filter refactor step"]
fn tier_b_fingerprints() {
    run_tier("b", matrix::tier_b(), true);
}
