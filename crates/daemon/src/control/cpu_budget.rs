//! CPU admission for the upsampler: estimate what a design costs the audio
//! thread at the output rate it would run at, and refuse a new one this
//! machine cannot run in real time.
//!
//! The estimate is the design's own callback work
//! ([`audivane_dsp_core::design::budget::Callback`]) priced with this machine's
//! measured rates for that kind of work ([`audivane_dsp_core::cpu_bench`]),
//! times the channels and the pipelines running at once. Spectral and dot
//! rates depend on how much filter data each sweep touches (in cache or
//! streamed), so the benchmark measures several sizes and the estimate picks
//! the one for the design; loads are multiply-adds per second at the in-cache
//! dot rate. This module keeps what is machine-specific: the benchmark, the
//! sustained rate, the admission policy and its refusal.
//!
//! A design is admitted up to [`RT_BUDGET`]: the audio thread runs the rest
//! of the chain too, and a swap briefly runs two chains; what overloads
//! anyway goes to the fallback ([`super::overload`]).
//!
//! With channel workers on (`channel_workers`, the default), each channel's
//! upsampler runs on its own thread, so the load priced is the busiest
//! thread's ([`lanes`]); an A/B output compare's second quantizer stays on
//! the audio thread. Priced single-threaded when the workers could not
//! start, and while a follower is enabled (each follower runs chain A whole
//! on its own thread). [`thread_load`] is that figure for one filter, as
//! `check_design` and `upsampler_response` report it.
//!
//! A convolution filter matrix with a cross term runs whole on the audio
//! thread with the EQ before it (the lanes start at the upsampler): one
//! partition per entry of the combined product
//! ([`design::budget::convolution_secs`]), plus the EQ's sections on every
//! channel ([`design::budget::biquad_secs`]), on top of that thread's share of
//! the upsamplers ([`matrix_load`]). Per-channel convolution runs in the lanes.
//!
//! The design workspace cap ([`init_design_workspace`]) is a quarter of the
//! total RAM within [`design::budget::DESIGN_WORKSPACE_FLOOR`] ..=
//! [`design::budget::DESIGN_WORKSPACE_CEILING`], read at start-up. Only
//! client changes are admitted (`set_upsampler`, `set_config`, `set_ab`,
//! `set_convolution`, presets); the saved config at start is never refused.

use audivane_dsp_core::cpu_bench::KernelRates;
use audivane_dsp_core::design::{self, BuildContext, FilterDesign};
use audivane_protocol::ipc::{
    DeviceCapabilities, EngineConfig, OverloadAction, RateAdmission, RateFallback,
    RateFallbackCause,
};
use audivane_protocol::{RateRatio, RateTarget, SampleFormat, UpsamplerConfig};
use std::sync::OnceLock;

/// The share of one core the upsampler may take: 90 %.
pub const RT_BUDGET: f64 = 0.9;

/// Channels the engine runs.
const CHANNELS: usize = 2;

static MACHINE: OnceLock<KernelRates> = OnceLock::new();

/// This machine's kernel rates, measured on first use (about 0.2 s, off
/// the audio thread) and kept for the daemon's lifetime.
pub fn machine() -> &'static KernelRates {
    MACHINE.get_or_init(|| {
        let (rates, floored) = settle_rates(audivane_dsp_core::cpu_bench::measure(), || {
            std::thread::sleep(REMEASURE_AFTER);
            audivane_dsp_core::cpu_bench::measure()
        });
        log::info!(
            "CPU benchmark: {:.1} G multiply-adds/s in cache, {:.1} G streamed, FFT \
             2048 points {:.1} us ({:.0} ms); upsampler budget {:.1} G multiply-adds/s; \
             design workspace {:.2} GiB{}",
            2.0 * rates.reference_rate() / 1e9,
            2.0 * rates.dot_rate(8 << 20) / 1e9,
            rates.fft_seconds(2048) * 1e6,
            rates.elapsed.as_secs_f64() * 1e3,
            sustained_macs(&rates) / 1e9,
            design::budget::design_workspace() as f64 / GIB,
            if floored {
                " (measured under the floor even when measured again: the machine was busy \
                 or throttled; using the floor, the overload fallback covers the rest)"
            } else {
                ""
            }
        );
        rates
    })
}

/// Measurements after the first one that came out under
/// [`KernelRates::floor`] (a busy boot, a CPU still at its lowest clock).
const REMEASURES: usize = 2;
/// Wait before measuring again.
const REMEASURE_AFTER: std::time::Duration = std::time::Duration::from_millis(250);

/// The rates the admission check uses from a first measurement: when it
/// is under the floor anywhere, measured again ([`REMEASURES`] times at
/// most, `again`) and the best of each kept; what is still under the floor
/// is raised to it, so a busy start never makes the check refuse ordinary
/// designs (a machine that really is that slow overloads, and the
/// fallback handles it). Returns the rates and whether the floor was used.
fn settle_rates(first: KernelRates, mut again: impl FnMut() -> KernelRates) -> (KernelRates, bool) {
    let floor = KernelRates::floor();
    let mut rates = first;
    for _ in 0..REMEASURES {
        if !rates.below(&floor) {
            return (rates, false);
        }
        rates = rates.best_of(&again());
    }
    if rates.below(&floor) {
        return (rates.at_least(&floor), true);
    }
    (rates, false)
}

const GIB: f64 = (1u64 << 30) as f64;

/// The machine's total RAM in bytes (`sysinfo`), `None` when it cannot be
/// read.
pub fn total_ram() -> Option<u64> {
    // SAFETY: `sysinfo` only writes the struct it is given.
    let mut info: libc::sysinfo = unsafe { std::mem::zeroed() };
    if unsafe { libc::sysinfo(&mut info) } != 0 {
        return None;
    }
    let unit = u64::from(info.mem_unit.max(1));
    (info.totalram as u64)
        .checked_mul(unit)
        .filter(|&bytes| bytes > 0)
}

/// Set the design workspace cap from the machine's total RAM
/// ([`design::budget::design_workspace_for_ram`]: total, not free memory,
/// so the cap and the rate masks stay the same for the daemon's lifetime),
/// once, before any design is validated. Returns the cap.
pub fn init_design_workspace() -> u64 {
    let ram = total_ram();
    let cap = design::budget::set_design_workspace(design::budget::design_workspace_for_ram(ram));
    match ram {
        Some(ram) => log::info!(
            "design workspace {:.2} GiB (a quarter of {:.1} GiB of RAM, within {:.1} to {:.0} \
             GiB)",
            cap as f64 / GIB,
            ram as f64 / GIB,
            design::budget::DESIGN_WORKSPACE_FLOOR as f64 / GIB,
            design::budget::DESIGN_WORKSPACE_CEILING as f64 / GIB
        ),
        None => log::warn!(
            "cannot read the total RAM: design workspace {:.2} GiB, the floor",
            cap as f64 / GIB
        ),
    }
    cap
}

/// The limits published in `status.limits`: the design library's tap
/// limits, this process's design workspace cap and [`RT_BUDGET`].
pub fn engine_limits() -> audivane_protocol::ipc::EngineLimits {
    audivane_protocol::ipc::EngineLimits {
        max_kernel_taps: design::budget::MAX_KERNEL_TAPS as u64,
        intermediate_max_taps: design::budget::max_intermediate_taps() as u64,
        design_workspace_bytes: design::budget::design_workspace(),
        cpu_share: RT_BUDGET,
    }
}

/// Measure the machine now on a background thread, so the first client
/// change does not wait for it.
pub fn warm() {
    let _ = std::thread::Builder::new()
        .name("avdsp-cpu-bench".into())
        .spawn(|| {
            machine();
        });
}

/// Equivalent multiply-adds per second (both channels) the upsampler may
/// use in real time.
pub fn sustained_macs(rates: &KernelRates) -> f64 {
    RT_BUDGET * CHANNELS as f64 * rates.reference_rate()
}

/// Seconds of CPU per second of audio for one upsampler pipeline (all
/// channels) running `up` at `ratio` from `source_rate`, with chain blocks
/// of `max_block` input frames: the design's callback work
/// ([`design::budget::callback`]) priced with `rates`. Zero at unity. `up`
/// has its automatic budget resolved ([`super::config::upsampler_variants`]).
pub fn upsampler_load(
    rates: &KernelRates,
    up: &UpsamplerConfig,
    source_rate: u32,
    ratio: RateRatio,
    max_block: usize,
) -> Result<f64, String> {
    if ratio.is_unity() || up.target == RateTarget::Off {
        return Ok(0.0);
    }
    let named = design::Named::of(up).map_err(|e| e.to_string())?;
    let d = &named.design;
    let cx = BuildContext {
        source_rate,
        ratio,
        channels: CHANNELS,
        max_block,
        pipelines: 1,
        output: audivane_protocol::OutputConfig::default(),
        workspace: design::budget::design_workspace(),
    };
    let work = design::budget::callback(d, &cx).map_err(|e| match (named.preset, d) {
        (Some(p), FilterDesign::Cascade(_) | FilterDesign::Interpolator(_))
        | (Some(p), FilterDesign::BlockFft(_)) => {
            format!("{} runs at integer ratios only", p.name())
        }
        _ => e.to_string(),
    })?;
    let per_channel = work
        .seconds(rates, source_rate, max_block)
        .map_err(|e| e.to_string())?;
    Ok(per_channel * CHANNELS as f64)
}

/// Seconds of CPU per second of audio for A/B's second quantizer at
/// `output_rate` (all channels) when the device opens in `format`: zero
/// unless B's bits or dither still differ from A's once both bits are
/// capped to the container, as the engine caps them before it builds the
/// pair ([`audivane_dsp_core::chain::AbChain::compares_output`]).
pub fn second_quantizer_load(
    rates: &KernelRates,
    a: &audivane_protocol::OutputConfig,
    b: &audivane_protocol::OutputConfig,
    output_rate: u32,
    format: Option<SampleFormat>,
) -> f64 {
    let cap = |bits| audivane_protocol::effective_dither_bits(bits, format);
    if cap(a.bits) == cap(b.bits) && a.dither == b.dither {
        return 0.0;
    }
    CHANNELS as f64 * f64::from(output_rate) / rates.quantizer_rate(b.dither)
}

/// The formats the device may open in at `output_rate`, as the driver
/// picks one (the first of the config's preference list the device takes
/// there): that one with the device's capabilities, every format of the
/// list without them (or when it takes none there).
fn candidate_formats(
    cfg: &EngineConfig,
    caps: Option<&DeviceCapabilities>,
    output_rate: u32,
) -> Vec<SampleFormat> {
    let formats: &[SampleFormat] = if cfg.formats.is_empty() {
        &audivane_io_alsa::FORMAT_PREFERENCE
    } else {
        &cfg.formats
    };
    caps.filter(|c| c.device == cfg.device)
        .and_then(|c| formats.iter().find(|&&f| c.supports(output_rate, f)))
        .map_or_else(|| formats.to_vec(), |&f| vec![f])
}

/// Threads the upsamplers of `cfg` run on, as admission prices them: one
/// per channel with channel workers on (as far as the machine has cores
/// and the pool's threads started), else one. One as well while any
/// follower is enabled: a follower runs chain A whole on its own thread,
/// so that thread is the busiest.
pub fn lanes(cfg: &EngineConfig) -> usize {
    if !cfg.channel_workers || followers_enabled(cfg) {
        1
    } else {
        crate::engine::workers::available_lanes()
    }
}

fn followers_enabled(cfg: &EngineConfig) -> bool {
    cfg.outputs.followers.iter().any(|f| f.enabled)
}

/// The fraction of one core the busiest thread spends on a filter that
/// costs `per_channel_secs` per second of audio on one channel, under
/// `cfg`: what admission prices ([`lanes`]), and what `cpu_load` reports.
pub fn thread_load(per_channel_secs: f64, cfg: &EngineConfig) -> f64 {
    per_channel_secs * CHANNELS as f64 / lanes(cfg) as f64
}

/// The worst case of a config: the rate and the load.
#[derive(Clone, Debug, PartialEq)]
pub struct Worst {
    pub source_rate: u32,
    pub output_rate: u32,
    /// Fraction of one core on the busiest thread, every pipeline included:
    /// with [`Self::lanes`] threads, each runs its share of the upsamplers.
    pub load: f64,
    /// Threads the upsamplers' channels are spread over ([`lanes`]).
    pub lanes: usize,
    /// A/B's side B share of [`Self::load`] (`None` without A/B): its
    /// upsampler, in an output compare its own quantizer, and its filter
    /// matrix.
    pub load_b: Option<f64>,
    /// The filter matrices' share of [`Self::load`] (both sides, with the
    /// EQ before them, [`matrix_load`]); 0 without one.
    pub load_conv: f64,
}

/// Taps of the WAV at `path` once resampled to `rate`, from its header
/// (`None`: unreadable; the config check reports that).
fn wav_taps(path: &std::path::Path, rate: u32) -> Option<usize> {
    let reader = hound::WavReader::open(path).ok()?;
    let native = reader.spec().sample_rate;
    let frames = u64::from(reader.duration());
    if native == 0 || frames == 0 {
        return None;
    }
    Some((frames * u64::from(rate)).div_ceil(u64::from(native)) as usize)
}

/// Seconds of CPU per second of audio that `chain`'s convolution filter
/// matrix adds to the audio thread at `rate` with blocks of `max_block`
/// frames: one convolver per entry of the combined product of the enabled
/// items ([`audivane_dsp_core::matrix::product_shape`], file lengths from
/// their headers), plus the EQ before it on every channel. 0 without a
/// cross term (per-channel convolution runs in the lanes), or when a file
/// cannot be read or an entry is out of range (the config check refuses
/// those).
pub fn matrix_load(
    rates: &KernelRates,
    chain: &audivane_protocol::ChainConfig,
    rate: u32,
    max_block: usize,
) -> f64 {
    let enabled: Vec<_> = chain.convolution.iter().filter(|c| c.enabled).collect();
    if !enabled.iter().any(|c| c.matrix.is_some()) {
        return 0.0;
    }
    let c = CHANNELS;
    let mut shapes = Vec::with_capacity(enabled.len());
    for conv in enabled {
        let mut shape = vec![None; c * c];
        match &conv.matrix {
            None => {
                let Some(taps) = wav_taps(&conv.ir_path, rate) else {
                    return 0.0;
                };
                for ch in 0..c {
                    shape[ch * c + ch] = Some(taps);
                }
            }
            Some(entries) => {
                for e in entries {
                    if e.input >= c || e.output >= c {
                        return 0.0;
                    }
                    let Some(taps) = wav_taps(&e.ir_path, rate) else {
                        return 0.0;
                    };
                    shape[e.output * c + e.input] = Some(taps);
                }
            }
        }
        shapes.push(shape);
    }
    let lens = audivane_dsp_core::matrix::product_shape(&shapes, c);
    let cross = lens
        .iter()
        .enumerate()
        .any(|(k, l)| l.is_some() && k / c != k % c);
    if !cross {
        return 0.0;
    }
    let conv: f64 = lens
        .iter()
        .flatten()
        .map(|&taps| design::budget::convolution_secs(rates, taps, rate, max_block))
        .sum();
    let eq = chain
        .eq
        .as_ref()
        .and_then(|p| audivane_dsp_core::biquad::Eq::new(p, f64::from(rate), c).ok())
        .map_or(0.0, |eq| {
            c as f64 * design::budget::biquad_secs(rates, eq.sections(), rate)
        });
    conv + eq
}

/// Every source rate of `cfg` where the engine would land it (its
/// upsamplers, chain A and B together, priced at each): the graph rates the
/// driver would offer and the ratio a fresh open picks at each
/// ([`crate::engine::predicted_landings`], the driver's own rate policy and
/// ladder, widened rates included). Empty with DSP off.
pub fn landing_costs(
    rates: &KernelRates,
    cfg: &EngineConfig,
    caps: Option<&DeviceCapabilities>,
) -> Result<Vec<(RateRatio, Worst)>, String> {
    if !cfg.dsp_enabled {
        return Ok(Vec::new());
    }
    crate::engine::predicted_landings(cfg, caps)?
        .into_iter()
        .map(|(rate, r)| {
            let c = played_at(Some(rates), cfg, caps, rate, r);
            Ok((r, landing_cost(rates, &c, caps, rate, r)?))
        })
        .collect()
}

/// `cfg` as the engine builds it at the landing `(rate, r)` as far as the
/// design goes: chain A's custom design where it builds, else its
/// substitute there ([`design_substitute`], priced with `rates`). The one
/// selection admission, the per-rate plan, the varispeed budget, the design
/// check and the followers all use.
pub fn played_at<'a>(
    rates: Option<&KernelRates>,
    cfg: &'a EngineConfig,
    caps: Option<&DeviceCapabilities>,
    rate: u32,
    r: RateRatio,
) -> std::borrow::Cow<'a, EngineConfig> {
    if super::config::chain_a_builds_at(cfg, rate, r).is_ok() {
        return std::borrow::Cow::Borrowed(cfg);
    }
    match design_substitute(rates, cfg, caps, rate, r) {
        Some(s) => {
            let mut c = cfg.clone();
            c.chain.upsampler = s.up;
            std::borrow::Cow::Owned(c)
        }
        None => std::borrow::Cow::Borrowed(cfg),
    }
}

/// What runs at a landing in place of the requested upsampler.
#[derive(Clone, Debug, PartialEq)]
pub struct Substitute {
    /// [`OverloadAction::Recommended`] for Recommended itself,
    /// [`OverloadAction::LightUpsampler`] for a lighter one.
    pub action: OverloadAction,
    pub up: UpsamplerConfig,
    /// Its estimated load there, when the machine is measured.
    pub load: Option<f64>,
}

/// The automatic budgets of the lighter forms of Recommended
/// ([`fallback_upsamplers`]), dB.
const LIGHT_BUDGETS_DB: [f64; 3] = [120.0, 96.0, 72.0];

/// The upsamplers a rate falls back to, heaviest first, all at `wanted`'s
/// target (so the device lands where it would): Recommended, then
/// Recommended with a shallower stopband and automatic budget
/// (`LIGHT_BUDGETS_DB`), each shorter and cheaper than the one before.
pub fn fallback_upsamplers(wanted: &UpsamplerConfig) -> Vec<(OverloadAction, UpsamplerConfig)> {
    let rec = super::config::recommended_upsampler(wanted);
    let mut out = vec![(OverloadAction::Recommended, rec)];
    for db in LIGHT_BUDGETS_DB {
        let mut d = design::catalogue::recommended();
        if let FilterDesign::Kernel(k) = &mut d
            && let design::Shape::WindowedSinc { length, cutoff, .. } = &mut k.shape
        {
            *length = design::Length::Auto(audivane_protocol::FilterErrorBudget::Db(db));
            if let design::Cutoff::Band(b) = cutoff {
                b.attenuation_db = db;
            }
        }
        out.push((
            OverloadAction::LightUpsampler,
            UpsamplerConfig {
                target: wanted.target,
                preset: None,
                design: Some(audivane_protocol::StoredDesign::based_on(
                    design::catalogue::RECOMMENDED,
                    d,
                )),
            },
        ));
    }
    out
}

/// The first of `candidates` that builds at the landing `(rate, r)` in
/// `cfg`'s place ([`super::config::chain_builds_at`]) and fits
/// [`RT_BUDGET`] there by the estimate; else the cheapest that builds;
/// without `rates`, the first that builds. `None` when none builds.
fn pick_at(
    rates: Option<&KernelRates>,
    cfg: &EngineConfig,
    caps: Option<&DeviceCapabilities>,
    rate: u32,
    r: RateRatio,
    candidates: &[(OverloadAction, UpsamplerConfig)],
) -> Option<Substitute> {
    let mut cheapest: Option<Substitute> = None;
    for (action, up) in candidates {
        let mut c = cfg.clone();
        c.chain.upsampler = up.clone();
        if super::config::chain_builds_at(&c, rate, r).is_err() {
            continue;
        }
        let Some(k) = rates else {
            return Some(Substitute {
                action: *action,
                up: up.clone(),
                load: None,
            });
        };
        let Ok(w) = landing_cost(k, &c, caps, rate, r) else {
            continue;
        };
        let s = Substitute {
            action: *action,
            up: up.clone(),
            load: Some(w.load),
        };
        if w.load <= RT_BUDGET {
            return Some(s);
        }
        if cheapest.as_ref().is_none_or(|c| c.load > s.load) {
            cheapest = Some(s);
        }
    }
    cheapest
}

/// What runs at the landing `(rate, r)` where chain A's custom design does
/// not build there: the first of [`fallback_upsamplers`] that builds there
/// and fits the CPU budget (`pick_at`). Depends only on the config, the
/// landing and the machine, so every caller makes the same choice.
pub fn design_substitute(
    rates: Option<&KernelRates>,
    cfg: &EngineConfig,
    caps: Option<&DeviceCapabilities>,
    rate: u32,
    r: RateRatio,
) -> Option<Substitute> {
    pick_at(
        rates,
        cfg,
        caps,
        rate,
        r,
        &fallback_upsamplers(&cfg.chain.upsampler),
    )
}

/// This machine's kernel rates once measured ([`machine`]), without
/// measuring: what the config validation prices a fallback with.
pub fn measured() -> Option<&'static KernelRates> {
    MACHINE.get()
}

/// Where chain A's custom design does not build among `landings`: each
/// such rate with a few plain words why ([`design::explain`], over every
/// landing where it upsamples). Empty for a preset.
pub fn design_fallbacks(cfg: &EngineConfig, landings: &[(u32, RateRatio)]) -> Vec<(u32, String)> {
    let up = &cfg.chain.upsampler;
    if !cfg.dsp_enabled || !super::config::falls_back_per_rate(up) {
        return Vec::new();
    }
    let Some(d) = up.design.as_ref() else {
        return Vec::new();
    };
    let pipelines = super::config::rational_pipelines(cfg.ab.is_some());
    let cases: Vec<design::explain::Case> = landings
        .iter()
        .filter(|(_, r)| !r.is_unity())
        .map(|&(rate, r)| design::explain::Case {
            cx: BuildContext {
                source_rate: rate,
                ratio: r,
                channels: CHANNELS,
                max_block: 1024,
                pipelines,
                output: cfg.chain.output,
                workspace: design::budget::design_workspace(),
            },
            error: super::config::chain_a_builds_at(cfg, rate, r)
                .err()
                .map(super::config::ChainAFailure::into_design_error),
        })
        .collect();
    if cases.iter().all(|c| c.error.is_none()) {
        return Vec::new();
    }
    design::explain::explain(d, &cases, true)
        .rates
        .into_iter()
        .map(|r| (r.source_rate, r.short))
        .collect()
}

/// What `cfg` costs at the landing `(rate, r)`: its upsamplers (every budget
/// the engine can resolve to: the heaviest), chain B at chain A's landing,
/// and an output compare's second quantizer.
fn landing_cost(
    rates: &KernelRates,
    cfg: &EngineConfig,
    caps: Option<&DeviceCapabilities>,
    rate: u32,
    r: RateRatio,
) -> Result<Worst, String> {
    // B's filter as built: with A's bits and dither.
    let chains = super::ab_side::designed_chains(cfg);
    let lanes = lanes(cfg);
    let max_block = audivane_io_pw::scaled_quantum(cfg.quantum, rate).clamp(512, 8192) as usize;
    let mut load = 0.0;
    let mut load_b = None;
    let mut load_conv = 0.0;
    let output_rate = r.output_rate(rate).unwrap_or(rate);
    for (i, chain) in chains.iter().enumerate() {
        let mut heaviest = 0.0f64;
        for up in super::config::upsampler_variants(chain) {
            heaviest = heaviest.max(upsampler_load(rates, &up, rate, r, max_block)?);
        }
        heaviest /= lanes as f64;
        // A filter matrix (and the EQ before it) runs whole on the audio
        // thread, beside that thread's share of the upsamplers.
        let conv = matrix_load(rates, chain, rate, max_block);
        heaviest += conv;
        load_conv += conv;
        if let (1, Some(ab)) = (i, &cfg.ab) {
            // An output compare's second quantizer runs with B, when
            // the format the device opens in leaves the two apart.
            heaviest += candidate_formats(cfg, caps, output_rate)
                .into_iter()
                .map(|f| {
                    second_quantizer_load(
                        rates,
                        &cfg.chain.output,
                        &ab.chain_b.output,
                        output_rate,
                        Some(f),
                    )
                })
                .fold(0.0, f64::max);
            load_b = Some(heaviest);
        }
        load += heaviest;
    }
    Ok(Worst {
        source_rate: rate,
        output_rate,
        load,
        lanes,
        load_b,
        load_conv,
    })
}

/// The heaviest landing of `cfg` ([`landing_costs`]).
pub fn worst_case(
    rates: &KernelRates,
    cfg: &EngineConfig,
    caps: Option<&DeviceCapabilities>,
) -> Result<Option<Worst>, String> {
    Ok(landing_costs(rates, cfg, caps)?
        .into_iter()
        .map(|(_, w)| w)
        .reduce(|a, b| if b.load > a.load { b } else { a }))
}

/// The landing admission prices `cfg` at: where `playing` (the source rate
/// playing now, else the last one played) lands under `cfg`, or the worst
/// case when there is none or `cfg` would not offer it.
pub fn admission_case(
    rates: &KernelRates,
    cfg: &EngineConfig,
    caps: Option<&DeviceCapabilities>,
    playing: Option<u32>,
) -> Result<Option<Worst>, String> {
    let costs = landing_costs(rates, cfg, caps)?;
    if let Some(w) = playing.and_then(|p| costs.iter().find(|(_, w)| w.source_rate == p)) {
        return Ok(Some(w.1.clone()));
    }
    Ok(costs
        .into_iter()
        .map(|(_, w)| w)
        .reduce(|a, b| if b.load > a.load { b } else { a }))
}

/// The per-rate plan of `requested` ([`crate::api::RatePlan`]) and the
/// status rows it makes ([`RateAdmission`]), at every landing:
///
/// - where chain A's custom design does not build ([`design_fallbacks`]),
///   its substitute there ([`design_substitute`]: Recommended, or a lighter
///   form of it when Recommended is over [`RT_BUDGET`];
///   [`RateFallbackCause::Design`]), as admission prices it;
/// - else, where the requested chain A upsampler is over [`RT_BUDGET`] by
///   the estimate, or overloaded there in this run (`tripped`), the first
///   that builds there and fits of `last_good` (the last upsampler setting
///   that ran without overload) and [`fallback_upsamplers`], all at the
///   requested target so the device lands as it would; the cheapest that
///   builds when none fits by the estimate (the reactive fallback is the
///   net).
///
/// Without `rates` (the CPU not measured yet) only the design's fallbacks
/// are planned, the first that builds, their rows priced at 0. Empty with
/// DSP or the upsampler off.
pub fn plan_rates(
    rates: Option<&KernelRates>,
    requested: &EngineConfig,
    caps: Option<&DeviceCapabilities>,
    last_good: Option<&UpsamplerConfig>,
    tripped: &[u32],
) -> (crate::api::RatePlan, Vec<RateAdmission>) {
    let mut plan = crate::api::RatePlan::default();
    let mut rows = Vec::new();
    let wanted = &requested.chain.upsampler;
    if !requested.dsp_enabled || wanted.target == RateTarget::Off {
        return (plan, rows);
    }
    let landings = match crate::engine::predicted_landings(requested, caps) {
        Ok(l) => l,
        Err(e) => {
            log::debug!("per-rate plan: no landings for the request: {e}");
            return (plan, rows);
        }
    };
    let mut candidates = Vec::new();
    if let Some(good) = last_good {
        let good = UpsamplerConfig {
            target: wanted.target,
            ..good.clone()
        };
        if good != *wanted {
            candidates.push((OverloadAction::Reverted, good));
        }
    }
    candidates.extend(
        fallback_upsamplers(wanted)
            .into_iter()
            .filter(|(_, up)| up != wanted),
    );
    let unbuilt = design_fallbacks(requested, &landings);
    for (rate, r) in landings {
        if r.is_unity() {
            continue;
        }
        if let Some((_, reason)) = unbuilt.iter().find(|u| u.0 == rate) {
            let Some(sub) = design_substitute(rates, requested, caps, rate, r) else {
                log::warn!(
                    "per-rate plan: neither the design nor a fallback builds at {rate} Hz ({r})"
                );
                continue;
            };
            let load = sub.load.unwrap_or(0.0);
            plan.rates.push((rate, sub.up));
            rows.push(RateAdmission {
                source_rate: rate,
                output_rate: r.output_rate(rate).unwrap_or(rate),
                load,
                fallback: Some(RateFallback {
                    action: sub.action,
                    cause: RateFallbackCause::Design,
                    load,
                    reason: Some(reason.clone()),
                }),
            });
            continue;
        }
        let Some(k) = rates else {
            continue;
        };
        let w = match landing_cost(k, requested, caps, rate, r) {
            Ok(w) => w,
            Err(e) => {
                log::debug!("per-rate plan: the request could not be priced at {rate} Hz: {e}");
                continue;
            }
        };
        let cause = if tripped.contains(&w.source_rate) {
            Some(RateFallbackCause::Overload)
        } else if w.load > RT_BUDGET {
            Some(RateFallbackCause::Estimate)
        } else {
            None
        };
        let fallback = cause.and_then(|cause| {
            let sub = pick_at(rates, requested, caps, rate, r, &candidates)?;
            let load = sub.load.unwrap_or(0.0);
            plan.rates.push((w.source_rate, sub.up));
            Some(RateFallback {
                action: sub.action,
                cause,
                load,
                reason: None,
            })
        });
        rows.push(RateAdmission {
            source_rate: w.source_rate,
            output_rate: w.output_rate,
            load: w.load,
            fallback,
        });
    }
    if !plan.rates.is_empty() {
        plan.made_for = Some((wanted.clone(), requested.chain.output));
    }
    (plan, rows)
}

/// "768 kHz", "705.6 kHz", "3072 kHz".
pub fn khz(rate: u32) -> String {
    let k = f64::from(rate) / 1000.0;
    if (k - k.round()).abs() < 1e-9 {
        format!("{k:.0} kHz")
    } else {
        format!("{} kHz", format!("{k:.3}").trim_end_matches('0'))
    }
}

/// The refusal for `worst` (over [`RT_BUDGET`]). When chain A alone fits
/// and A/B's side B is what tips it over, it names side B.
pub fn refusal(rates: &KernelRates, worst: &Worst) -> String {
    // In all, over the threads; the share is the busiest thread's.
    let lanes = worst.lanes.max(1) as f64;
    let needed = worst.load * lanes * CHANNELS as f64 * rates.reference_rate();
    let sustained = sustained_macs(rates) * lanes;
    let core = if worst.lanes > 1 {
        "of a core on each channel's thread"
    } else {
        "of one core"
    };
    if worst.load_conv > 0.0 && worst.load - worst.load_conv <= RT_BUDGET {
        return format!(
            "Too heavy for this CPU at {} from a {} source: the convolution filter matrix \
             (one convolver per filter, with the EQ before it, on the audio thread) needs about \
             {:.0}% of a core, {:.0}% in all with the rest of the chain. Use shorter filters or \
             fewer matrix entries.",
            khz(worst.output_rate),
            khz(worst.source_rate),
            worst.load_conv * 100.0,
            worst.load * 100.0
        );
    }
    if let Some(b) = worst.load_b
        && b > 0.0
        && worst.load - b <= RT_BUDGET
    {
        return format!(
            "Too heavy for this CPU at {} from a {} source with A/B: side B's upsampler (and its own \
             quantizer, when its bits or dither differ) needs about {:.1} G MAC/s ({:.0}% \
             {core}) on top of chain A's, {:.1} G MAC/s in all, and this machine sustains \
             about {:.1} G MAC/s in real time. Pick a lighter filter for side B, or turn A/B \
             off.",
            khz(worst.output_rate),
            khz(worst.source_rate),
            b * lanes * CHANNELS as f64 * rates.reference_rate() / 1e9,
            b * 100.0,
            needed / 1e9,
            sustained / 1e9
        );
    }
    format!(
        "Too heavy for this CPU at {} from a {} source: about {:.1} G MAC/s needed ({:.0}% \
         {core}), this machine sustains about {:.1} G MAC/s in real time. Use fewer taps or a \
         lower output rate.",
        khz(worst.output_rate),
        khz(worst.source_rate),
        needed / 1e9,
        worst.load * 100.0,
        sustained / 1e9
    )
}

/// The parts of a config the admission check looks at.
fn upsampler_part(cfg: &EngineConfig) -> impl PartialEq + '_ {
    (
        cfg.dsp_enabled,
        &cfg.chain.upsampler,
        &cfg.chain.output,
        cfg.ab
            .as_ref()
            .map(|ab| (&ab.chain_b.upsampler, &ab.chain_b.output)),
        &cfg.rate.allowed_rates,
        cfg.quantum,
        &cfg.device,
        &cfg.formats,
        // Turning the workers off, or a follower on (which runs chain A on
        // one thread), doubles the busiest thread's share.
        cfg.channel_workers,
        followers_enabled(cfg),
    )
}

/// The convolution lists (both sides): a change there is admitted too,
/// for what a filter matrix costs ([`matrix_load`]).
fn convolution_part(cfg: &EngineConfig) -> impl PartialEq + '_ {
    (
        &cfg.chain.convolution,
        cfg.ab.as_ref().map(|ab| &ab.chain_b.convolution),
    )
}

/// Refuse `next` when it brings a new upsampler (or a rate, device or A/B
/// change that makes the current one run elsewhere) this machine cannot
/// run in real time ([`RT_BUDGET`]) at the source rate playing now
/// (`playing`: the one playing, else the last one played; the worst case
/// without one, [`admission_case`]). Other source rates over budget fall
/// back per rate ([`plan_rates`]). Where a filter matrix counts in `old` or
/// `next`, a change that costs no more than `old` there is never refused,
/// whatever it touches (removing or shortening a matrix, turning A/B off).
/// A change elsewhere in the chain is
/// never refused, so a client can still edit the EQ of a config that
/// overloads.
pub fn admit(
    rates: &KernelRates,
    old: &EngineConfig,
    next: &EngineConfig,
    caps: Option<&DeviceCapabilities>,
    playing: Option<u32>,
) -> Result<(), String> {
    if upsampler_part(old) == upsampler_part(next)
        && convolution_part(old) == convolution_part(next)
    {
        return Ok(());
    }
    match admission_case(rates, next, caps, playing)? {
        Some(w) if w.load > RT_BUDGET => {
            // Where a filter matrix counts (in either config), never refuse
            // what costs no more than the config it replaces, whatever else
            // the change touches (a preset that shortens a matrix and moves
            // the headroom, A/B off). An upsampler alone over budget is
            // still refused, lighter or not.
            let before = admission_case(rates, old, caps, playing).ok().flatten();
            if before.is_some_and(|b| (w.load_conv > 0.0 || b.load_conv > 0.0) && w.load <= b.load)
            {
                return Ok(());
            }
            log::info!(
                "refused a change: {:.0}% of a core at {} from {} (budget {:.0}%)",
                w.load * 100.0,
                khz(w.output_rate),
                khz(w.source_rate),
                RT_BUDGET * 100.0
            );
            Err(refusal(rates, &w))
        }
        _ => Ok(()),
    }
}

/// [`admit`] for follower `f`'s own chain (its chain patch over `next`'s
/// chain A; `old` holds its chain before the edit, chain A alone for a new
/// patch). It runs at every allowed rate on its own thread, without the
/// master's per-rate substitutes, so it is priced at its worst landing,
/// not where the master plays: a refusal names that rate.
pub fn admit_follower(
    rates: &KernelRates,
    old: &EngineConfig,
    next: &EngineConfig,
    f: &audivane_protocol::ipc::FollowerConfig,
) -> Result<(), String> {
    let before = old
        .outputs
        .followers
        .iter()
        .find(|g| g.device == f.device)
        .cloned()
        .unwrap_or_else(|| {
            let mut plain = f.clone();
            plain.chain_patch = None;
            plain
        });
    let mut old_view = super::follower_chain::config(old, &before);
    old_view.outputs = next.outputs.clone();
    let next_view = super::follower_chain::config(next, f);
    admit(rates, &old_view, &next_view, None, None)
        .map_err(|e| format!("follower {}: {e}", f.device))
}

#[cfg(test)]
mod tests {
    use super::*;
    use audivane_dsp_core::presets::{FilterPreset, UpsamplerRuntime};

    fn manual(taps: usize, ratio: u32) -> UpsamplerConfig {
        audivane_dsp_core::resample::ManualConfig {
            target: RateTarget::Ratio(ratio),
            taps,
            ..Default::default()
        }
        .into_upsampler_config()
        .unwrap()
    }

    /// Measured cost of `rt` (stereo on this thread, `block` input frames
    /// per call): seconds per second of audio over `seconds` of audio
    /// (best of three runs), and the 99th percentile of one call's time
    /// over its duration (the slowest run's, as admission must hold it).
    fn measure(
        rt: &mut dyn UpsamplerRuntime,
        rate: u32,
        ratio: RateRatio,
        block: usize,
        seconds: f64,
    ) -> (f64, f64) {
        let x: Vec<f64> = (0..block).map(|i| (i as f64 * 0.01).sin() * 0.5).collect();
        let max_out = ratio.max_frames_out(block);
        let mut l = vec![0.0; max_out];
        let mut r = vec![0.0; max_out];
        let calls = ((f64::from(rate) * seconds) / block as f64).ceil() as usize;
        let period = block as f64 / f64::from(rate);
        fn thread_cpu_secs() -> f64 {
            let mut ts = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            // SAFETY: valid pointer to initialized timespec, Linux CLOCK_THREAD_CPUTIME_ID.
            let rc = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
            assert_eq!(rc, 0, "clock_gettime(CLOCK_THREAD_CPUTIME_ID) failed");
            ts.tv_sec as f64 + ts.tv_nsec as f64 * 1e-9
        }

        let (mut best, mut p99) = (f64::INFINITY, f64::INFINITY);
        let mut times = Vec::with_capacity(calls);
        for _ in 0..3 {
            times.clear();
            let t0 = thread_cpu_secs();
            for _ in 0..calls {
                let t = thread_cpu_secs();
                let n = rt.output_frames(block).min(max_out);
                rt.process(&[&x, &x], &mut [&mut l[..n], &mut r[..n]]);
                times.push((thread_cpu_secs() - t).max(0.0) / period);
            }
            let audio = (calls * block) as f64 / f64::from(rate);
            best = best.min((thread_cpu_secs() - t0).max(0.0) / audio);
            times.sort_by(f64::total_cmp);
            p99 = p99.min(times[times.len() * 99 / 100]);
        }
        std::hint::black_box(&l);
        (best, p99)
    }

    fn build(
        up: &UpsamplerConfig,
        rate: u32,
        ratio: RateRatio,
        block: usize,
    ) -> Box<dyn UpsamplerRuntime> {
        let cx = BuildContext {
            source_rate: rate,
            ratio,
            channels: CHANNELS,
            max_block: block,
            pipelines: design::budget::RATIONAL_SWAP_PIPELINES,
            output: audivane_protocol::OutputConfig::default(),
            workspace: design::budget::design_workspace(),
        };
        design::upsampler(&design::Named::of(up).unwrap(), &cx, 0.0)
            .unwrap()
            .unwrap()
    }

    /// The estimate against the measured callback time for designs on
    /// every path: never more than 30 % under the mean. On non-uniform
    /// partitions (the long kernels) the estimate is the heaviest callback,
    /// so it must cover the measured 99th percentile of a callback too (an
    /// idle machine measured 1.4 to 1.6x the mean, 1.15 to 1.4x the 99th
    /// percentile; not asserted from above: under a parallel test run the
    /// benchmark itself runs slow). Long
    /// kernels run over two periods of their largest stage, and at 44.1 kHz
    /// on the 512-frame block the engine gets there (q1024).
    #[test]
    fn the_estimate_does_not_under_estimate_measured_callbacks() {
        let mut rates = audivane_dsp_core::cpu_bench::measure();
        let preset = |p: FilterPreset| UpsamplerConfig {
            target: RateTarget::Ratio(8),
            preset: Some(p.name().to_string()),
            ..UpsamplerConfig::default()
        };
        let int = RateRatio::integer;
        let cases: Vec<(&str, UpsamplerConfig, u32, RateRatio, usize)> = vec![
            (
                "manual 65536 x16",
                manual(65_536, 16),
                48_000,
                int(16),
                1024,
            ),
            (
                "manual 1048576 x16",
                manual(1 << 20, 16),
                48_000,
                int(16),
                1024,
            ),
            (
                "manual 1048576 x16 44.1k",
                manual(1 << 20, 16),
                44_100,
                int(16),
                512,
            ),
            (
                "manual 4194304 x16 44.1k",
                manual(1 << 22, 16),
                44_100,
                int(16),
                512,
            ),
            (
                "manual 8388608 x16 48k",
                manual(1 << 23, 16),
                48_000,
                int(16),
                1024,
            ),
            (
                "manual 8388608 x4 192k",
                manual(1 << 23, 4),
                192_000,
                int(4),
                4096,
            ),
            ("manual 4095 x4", manual(4095, 4), 44_100, int(4), 1024),
            (
                "rational 44.1>48 511/branch",
                audivane_dsp_core::resample::ManualConfig {
                    target: RateTarget::Rate(48_000),
                    taps: 511,
                    taps_unit: audivane_protocol::TapsUnit::Input,
                    ..Default::default()
                }
                .into_upsampler_config()
                .unwrap(),
                44_100,
                RateRatio::from_rates(44_100, 48_000),
                1024,
            ),
            (
                "cascade_fir x8",
                preset(FilterPreset::CascadeFir),
                48_000,
                int(8),
                1024,
            ),
            (
                "cascade_elliptic x8",
                preset(FilterPreset::CascadeElliptic),
                48_000,
                int(8),
                1024,
            ),
            (
                "table_sinc x8",
                preset(FilterPreset::TableSinc),
                48_000,
                int(8),
                1024,
            ),
            (
                "cosine x8",
                preset(FilterPreset::Cosine),
                48_000,
                int(8),
                1024,
            ),
            (
                "block_fft x8",
                preset(FilterPreset::BlockFft),
                48_000,
                int(8),
                1024,
            ),
        ];
        let mut report = Vec::new();
        let mut failures = Vec::new();
        for (name, up, rate, ratio, block) in cases {
            let mut rt = build(&up, rate, ratio, block);
            let mut estimate = upsampler_load(&rates, &up, rate, ratio, block).unwrap();
            // On non-uniform partitions: the FFT polyphase engine with tail
            // stages, as the budget prices it.
            let cx = BuildContext {
                source_rate: rate,
                ratio,
                channels: CHANNELS,
                max_block: block,
                pipelines: 1,
                output: audivane_protocol::OutputConfig::default(),
                workspace: design::budget::design_workspace(),
            };
            let named = design::Named::of(&up).unwrap();
            let long = match design::budget::callback(&named.design, &cx).unwrap() {
                design::budget::Callback::Polyphase { taps, ratio } => {
                    design::budget::polyphase_stages(taps, ratio, block) > 1
                }
                _ => false,
            };
            let seconds = if long {
                (2.0 * 32_768.0 / f64::from(rate)).max(0.4)
            } else {
                0.4
            };
            // Machine load (cache/memory contention) only ever makes a callback
            // slower: when a case looks under-estimated, re-sync the benchmark
            // rates under the same load, measure again and keep the fastest.
            let (mut mean, mut p99) = measure(rt.as_mut(), rate, ratio, block, seconds);
            for _ in 0..if long { 5 } else { 3 } {
                if estimate / mean >= 0.7 && (!long || estimate / p99 >= 1.0) {
                    break;
                }
                rates = audivane_dsp_core::cpu_bench::measure();
                estimate = upsampler_load(&rates, &up, rate, ratio, block).unwrap();
                let (m, p) = measure(rt.as_mut(), rate, ratio, block, seconds);
                (mean, p99) = (mean.min(m), p99.min(p));
            }
            let line = format!(
                "{name}: estimate {estimate:.4} measured {mean:.4} (x{:.2}), p99 call {p99:.4} (x{:.2})",
                estimate / mean,
                estimate / p99,
            );
            if estimate / mean < 0.7 || (long && estimate / p99 < 1.0) {
                failures.push(line.clone());
            }
            report.push(line);
        }
        eprintln!("{}", report.join("\n"));
        assert!(failures.is_empty(), "out of bounds: {failures:#?}");
    }

    fn slow_machine() -> KernelRates {
        KernelRates {
            dot: vec![(256 << 10, 1.0e9), (8 << 20, 0.8e9)],
            // Slow spectra and transforms: the FFT polyphase engine runs a
            // 4M-tap kernel in a fraction of a core on a desktop, so the
            // refusals below need a slow machine.
            spectral: vec![(256 << 10, 0.06e9), (8 << 20, 0.05e9), (64 << 20, 0.03e9)],
            fft: vec![(2048, 4.0e-9), (32768, 4.0e-9)],
            mul_add: 0.3e9,
            quantizer: KernelRates::floor().quantizer,
            elapsed: std::time::Duration::ZERO,
        }
    }

    /// A config whose graph runs at 48 kHz only.
    fn base() -> EngineConfig {
        let mut cfg = EngineConfig::default();
        cfg.rate.allowed_rates = vec![48_000];
        cfg
    }

    fn heavy() -> EngineConfig {
        let mut cfg = base();
        cfg.chain.upsampler = manual(4 << 20, 16);
        cfg
    }

    /// A follower runs chain A whole on its own thread: with one enabled,
    /// admission prices the chain on one thread, and enabling one is
    /// checked again.
    #[test]
    fn an_enabled_follower_prices_the_chain_on_one_thread() {
        let rates = slow_machine();
        let mut follow = heavy();
        follow
            .outputs
            .followers
            .push(audivane_protocol::ipc::FollowerConfig::new(
                "hw:CARD=USB,DEV=0",
            ));
        assert_eq!(lanes(&follow), 1);
        let mut single = heavy();
        single.channel_workers = false;
        assert_eq!(
            worst_case(&rates, &follow, None).unwrap(),
            worst_case(&rates, &single, None).unwrap()
        );
        let err = admit(&rates, &heavy(), &follow, None, None).unwrap_err();
        assert!(err.contains("% of one core)"), "{err}");
        assert_eq!(thread_load(0.25, &follow), 0.5);
        // A disabled follower plays nothing.
        follow.outputs.followers[0].enabled = false;
        assert_eq!(lanes(&follow), lanes(&heavy()));
        assert_eq!(thread_load(0.25, &follow), 0.5 / lanes(&heavy()) as f64);
    }

    /// Mono float WAVs of `taps` taps at 48 kHz in a fresh directory.
    fn matrix_irs(tag: &str, taps: usize, names: &[&str]) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("audivane_budget_{tag}_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48_000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        for name in names {
            let mut w = hound::WavWriter::create(dir.join(name), spec).unwrap();
            for i in 0..taps {
                w.write_sample(if i == 0 { 0.5_f32 } else { 0.0 }).unwrap();
            }
            w.finalize().unwrap();
        }
        dir
    }

    fn matrix_item(
        dir: &std::path::Path,
        routes: &[(usize, usize, &str)],
    ) -> audivane_protocol::ConvolutionConfig {
        audivane_protocol::ConvolutionConfig {
            name: None,
            enabled: true,
            ir_path: Default::default(),
            gain_db: 0.0,
            matrix: Some(
                routes
                    .iter()
                    .map(|&(input, output, f)| audivane_protocol::MatrixEntry {
                        input,
                        output,
                        ir_path: dir.join(f),
                        channel: None,
                        gain_db: 0.0,
                    })
                    .collect(),
            ),
            calibration: false,
        }
    }

    /// A filter matrix with a cross term runs on the audio thread: one
    /// convolver per entry of the combined product, priced on its own
    /// schedule, and the EQ before it on every channel, on top of that
    /// thread's share of the upsamplers. A diagonal one runs per channel in
    /// the lanes, as plain convolution does, and adds nothing here.
    #[test]
    fn a_filter_matrix_is_priced_per_entry_on_the_audio_thread() {
        let rates = slow_machine();
        let dir = matrix_irs("price", 65_536, &["a.wav", "b.wav"]);
        let one = design::budget::convolution_secs(&rates, 65_536, 48_000, 1024);
        assert!(one > 0.0);
        assert!(design::budget::convolution_secs(&rates, 262_144, 48_000, 1024) > one);
        let mut sum = base();
        sum.chain.convolution = vec![matrix_item(&dir, &[(0, 0, "a.wav"), (1, 0, "b.wav")])];
        let mut full = base();
        full.chain.convolution = vec![matrix_item(
            &dir,
            &[
                (0, 0, "a.wav"),
                (1, 0, "b.wav"),
                (0, 1, "b.wav"),
                (1, 1, "a.wav"),
            ],
        )];
        let close = |a: f64, b: f64| (a - b).abs() <= 1e-12 * b.max(1e-30);
        assert!(close(
            matrix_load(&rates, &sum.chain, 48_000, 1024),
            2.0 * one
        ));
        assert!(close(
            matrix_load(&rates, &full.chain, 48_000, 1024),
            4.0 * one
        ));
        let mut diag = base();
        diag.chain.convolution = vec![matrix_item(&dir, &[(0, 0, "a.wav"), (1, 1, "b.wav")])];
        assert_eq!(matrix_load(&rates, &diag.chain, 48_000, 1024), 0.0);
        // The EQ moves onto the audio thread with the matrix.
        let mut with_eq = full.clone();
        with_eq.chain.eq = Some(audivane_protocol::EqProfile {
            bands: (0..10)
                .map(|k| audivane_protocol::Band {
                    kind: audivane_protocol::FilterType::Peaking,
                    freq_hz: 100.0 * (k + 1) as f64,
                    gain_db: -3.0,
                    q: 1.0,
                    enabled: true,
                })
                .collect(),
            ..Default::default()
        });
        let eq = matrix_load(&rates, &with_eq.chain, 48_000, 1024) - 4.0 * one;
        assert!(
            close(eq, 2.0 * design::budget::biquad_secs(&rates, 10, 48_000)),
            "{eq}"
        );
        // Admission counts it, on top of the upsamplers' share.
        let w0 = worst_case(&rates, &base(), None).unwrap().unwrap();
        let w = worst_case(&rates, &full, None).unwrap().unwrap();
        assert_eq!(w0.load_conv, 0.0);
        assert!(w.load_conv > 0.0);
        assert!(close(w.load - w0.load, w.load_conv), "{w:?} {w0:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A filter matrix over budget is refused with its own message (also
    /// through `set_convolution`, a change that leaves the upsampler
    /// alone); removing or shortening it passes even while still over.
    #[test]
    fn a_heavy_filter_matrix_is_refused_and_can_always_be_removed() {
        let dir = matrix_irs("refuse", 65_536, &["a.wav", "b.wav"]);
        let mut full = base();
        full.chain.convolution = vec![matrix_item(
            &dir,
            &[
                (0, 0, "a.wav"),
                (1, 0, "b.wav"),
                (0, 1, "b.wav"),
                (1, 1, "a.wav"),
            ],
        )];
        // A machine on which the four convolvers alone take 2.4 cores (two:
        // 1.2, still over).
        let slow = slow_machine();
        let w = worst_case(&slow, &full, None).unwrap().unwrap();
        let rates = scaled(&slow, w.load_conv / 2.4);
        let err = admit(&rates, &base(), &full, None, None).unwrap_err();
        assert!(err.contains("the convolution filter matrix"), "{err}");
        assert!(
            err.ends_with("Use shorter filters or fewer matrix entries."),
            "{err}"
        );
        // Lighter, still over: passes; none: passes.
        let mut sum = base();
        sum.chain.convolution = vec![matrix_item(&dir, &[(0, 0, "a.wav"), (1, 0, "b.wav")])];
        let w_sum = worst_case(&rates, &sum, None).unwrap().unwrap();
        assert!(w_sum.load > RT_BUDGET, "{w_sum:?}");
        admit(&rates, &full, &sum, None, None).unwrap();
        admit(&rates, &full, &base(), None, None).unwrap();
        // Disabling it is a convolution change too, and lighter.
        let mut off = full.clone();
        off.chain.convolution[0].enabled = false;
        admit(&rates, &full, &off, None, None).unwrap();
        // Lighter with another field changed too (a preset that also moves
        // the output headroom): still passes, as a heavier one is refused.
        let mut sum_headroom = sum.clone();
        sum_headroom.chain.output.headroom_db += 1.0;
        admit(&rates, &full, &sum_headroom, None, None).unwrap();
        let mut full_headroom = full.clone();
        full_headroom.chain.output.headroom_db += 1.0;
        assert!(admit(&rates, &sum, &full_headroom, None, None).is_err());
        // A/B off with the matrix on both sides halves the load: passes.
        let mut ab = full.clone();
        ab.ab = Some(audivane_protocol::ipc::AbConfig::new(
            audivane_protocol::ipc::AbB::Patch(serde_json::json!({})),
        ));
        assert!(super::super::config::normalize_config(&mut ab).is_none());
        let w_full = worst_case(&rates, &full, None).unwrap().unwrap();
        let w_ab = worst_case(&rates, &ab, None).unwrap().unwrap();
        assert!(w_ab.load > 1.9 * w_full.load_conv, "{w_ab:?}");
        admit(&rates, &ab, &full, None, None).unwrap();
        assert!(admit(&rates, &full, &ab, None, None).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A follower's own upsampler is priced at every allowed rate, not
    /// only where the master plays: one that fits from 48 kHz but not from
    /// 192 kHz is refused, naming 192 kHz.
    #[test]
    fn a_follower_patch_is_priced_at_every_allowed_rate() {
        let rates = slow_machine();
        let mut old = EngineConfig::default();
        old.rate.allowed_rates = vec![48_000, 192_000];
        old.channel_workers = false;
        let dev = "hw:CARD=USB,DEV=0";
        old.outputs
            .followers
            .push(audivane_protocol::ipc::FollowerConfig::new(dev));
        let mut next = old.clone();
        let up = serde_json::to_value(manual(1 << 18, 4)).unwrap();
        next.outputs.followers[0].chain_patch = Some(serde_json::json!({ "upsampler": up }));
        let view = super::super::follower_chain::config(&next, &next.outputs.followers[0]);
        let plain = super::super::follower_chain::config(&old, &old.outputs.followers[0]);
        // Where the master plays (48 kHz) it fits.
        admit(&rates, &plain, &view, None, Some(48_000)).unwrap();
        let err = admit_follower(&rates, &old, &next, &next.outputs.followers[0]).unwrap_err();
        assert!(
            err.starts_with(&format!(
                "follower {dev}: Too heavy for this CPU at 768 kHz from a 192 kHz source"
            )),
            "{err}"
        );
        // Lighter, it fits at both.
        let up = serde_json::to_value(manual(1 << 14, 4)).unwrap();
        next.outputs.followers[0].chain_patch = Some(serde_json::json!({ "upsampler": up }));
        admit_follower(&rates, &old, &next, &next.outputs.followers[0]).unwrap();
    }

    #[test]
    fn a_heavy_design_is_refused_with_a_plain_message() {
        let rates = slow_machine();
        let mut old = base();
        old.channel_workers = false;
        let mut single = heavy();
        single.channel_workers = false;
        let err = admit(&rates, &old, &single, None, None).unwrap_err();
        assert!(
            err.starts_with("Too heavy for this CPU at 768 kHz from a "),
            "{err}"
        );
        assert!(err.contains("G MAC/s needed"), "{err}");
        assert!(err.contains("% of one core)"), "{err}");
        assert!(
            err.contains("this machine sustains about 1.8 G MAC/s in real time"),
            "{err}"
        );
        // With channel workers (the default) each channel's thread takes
        // half the load and the machine sustains twice as much.
        if lanes(&heavy()) == 2 {
            let w1 = worst_case(&rates, &single, None).unwrap().unwrap();
            let w2 = worst_case(&rates, &heavy(), None).unwrap().unwrap();
            assert!((w2.load - w1.load / 2.0).abs() < 1e-12, "{w1:?} {w2:?}");
            let err = admit(&rates, &base(), &heavy(), None, None).unwrap_err();
            assert!(
                err.contains("% of a core on each channel's thread)"),
                "{err}"
            );
            assert!(err.contains("sustains about 3.6 G MAC/s"), "{err}");
        }
        let old = base();
        assert!(
            err.ends_with("Use fewer taps or a lower output rate."),
            "{err}"
        );
        // A light design passes, and an edit that leaves the upsampler
        // alone passes even when the current one is too heavy.
        let mut light = base();
        light.chain.upsampler = manual(4095, 4);
        admit(&rates, &old, &light, None, None).unwrap();
        let mut eq_edit = heavy();
        eq_edit.chain.eq = None;
        eq_edit.dsp_enabled = true;
        admit(&rates, &heavy(), &eq_edit, None, None).unwrap();
        // DSP off runs nothing.
        let mut off = heavy();
        off.dsp_enabled = false;
        admit(&rates, &old, &off, None, None).unwrap();
    }

    #[test]
    fn a_refusal_names_side_b_when_b_tips_it_over() {
        use audivane_protocol::ipc::{AbB, AbConfig};
        let rates = slow_machine();
        let mut light = base();
        light.chain.upsampler = manual(4095, 16);
        let b_up = heavy().chain.upsampler;
        let mut next = light.clone();
        next.ab = Some(AbConfig::new(AbB::Patch(
            serde_json::json!({"upsampler": {"preset": null, "design": b_up.design}}),
        )));
        assert!(super::super::config::normalize_config(&mut next).is_none());
        let err = admit(&rates, &light, &next, None, None).unwrap_err();
        assert!(err.contains("with A/B: side B's upsampler"), "{err}");
        assert!(
            err.ends_with("Pick a lighter filter for side B, or turn A/B off."),
            "{err}"
        );
        // Chain A too heavy on its own: the plain refusal.
        let mut both = heavy();
        both.ab = Some(AbConfig::new(AbB::Patch(serde_json::json!({}))));
        assert!(super::super::config::normalize_config(&mut both).is_none());
        let err = admit(&rates, &base(), &both, None, None).unwrap_err();
        assert!(
            err.ends_with("Use fewer taps or a lower output rate."),
            "{err}"
        );
    }

    /// An output compare counts B's own quantizer with side B, at the
    /// output rate and the rate of B's dither kind; the same bits and
    /// dither count nothing.
    #[test]
    fn an_output_compare_counts_the_second_quantizer() {
        use audivane_protocol::ipc::{AbB, AbConfig};
        let rates = slow_machine();
        let mut cfg = base();
        cfg.chain.upsampler = manual(4095, 16);
        let with = |patch| {
            let mut next = cfg.clone();
            next.ab = Some(AbConfig::new(AbB::Patch(patch)));
            assert!(super::super::config::normalize_config(&mut next).is_none());
            worst_case(&rates, &next, None).unwrap().unwrap()
        };
        let same = with(serde_json::json!({"eq": null}));
        let shaped = with(serde_json::json!({"output": {"bits": 16, "dither": "shaped"}}));
        assert_eq!(shaped.output_rate, 768_000);
        // The floor's 4 M samples per second: 2 x 768 k / 4 M of a core.
        let q = 2.0 * 768_000.0 / 4.0e6;
        assert!((shaped.load - same.load - q).abs() < 1e-12, "{shaped:?}");
        assert!((shaped.load_b.unwrap() - same.load_b.unwrap() - q).abs() < 1e-12);
    }

    /// The format cap comes first, as in the engine: B at 32 bits against
    /// A at 24 shares A's quantizer on a device that opens in S24, and has
    /// its own on one that opens in S32.
    #[test]
    fn the_second_quantizer_is_counted_after_the_format_cap() {
        use audivane_protocol::ipc::{AbB, AbConfig};
        let rates = slow_machine();
        let mut cfg = base();
        cfg.chain.upsampler = manual(4095, 16);
        cfg.chain.output.bits = 24;
        let caps = |format| {
            let mut caps = s32_caps(&[48_000, 768_000]);
            for r in &mut caps.rates {
                r.formats[0].format = format;
            }
            caps.formats = vec![format];
            caps
        };
        let load = |patch, format| {
            let mut next = cfg.clone();
            next.ab = Some(AbConfig::new(AbB::Patch(patch)));
            assert!(super::super::config::normalize_config(&mut next).is_none());
            worst_case(&rates, &next, Some(&caps(format)))
                .unwrap()
                .unwrap()
                .load
        };
        let wide = serde_json::json!({"output": {"bits": 32}});
        let same = serde_json::json!({"eq": null});
        assert_eq!(
            load(wide.clone(), SampleFormat::S24),
            load(same.clone(), SampleFormat::S24)
        );
        let q = 2.0 * 768_000.0 / 4.0e6;
        let diff = load(wide, SampleFormat::S32) - load(same, SampleFormat::S32);
        assert!((diff - q).abs() < 1e-12, "{diff}");
    }

    /// The share is 90 % of a core: a design between three quarters and
    /// 90 % is admitted, one above is refused.
    #[test]
    fn the_admission_share_is_90_percent_of_a_core() {
        assert_eq!(RT_BUDGET, 0.9);
        let rates = slow_machine();
        assert_eq!(sustained_macs(&rates), 1.8e9);
        let load = |taps| {
            let mut cfg = base();
            cfg.chain.upsampler = manual(taps, 16);
            let w = worst_case(&rates, &cfg, None).unwrap().unwrap();
            (cfg, w.load)
        };
        let (mut admitted, mut refused) = (None, None);
        for taps in (1..=128).map(|k| k << 16) {
            let (cfg, l) = load(taps);
            if l > 0.75 && l <= RT_BUDGET && admitted.is_none() {
                admitted = Some((cfg, l));
            } else if l > RT_BUDGET && refused.is_none() {
                refused = Some((cfg, l));
            }
        }
        let (cfg, l) = admitted.expect("a design between three quarters and 90 % of a core");
        admit(&rates, &base(), &cfg, None, None).unwrap_or_else(|e| panic!("{l}: {e}"));
        let (cfg, l) = refused.expect("a design above 90 % of a core");
        assert!(admit(&rates, &base(), &cfg, None, None).is_err(), "{l}");
    }

    /// A design to 768 kHz that fits at 48 kHz (x16) and not at 192 kHz
    /// (x4: the FFT engine's cost grows with the source rate), with its
    /// config over both rates.
    fn fits_at_48k_only(rates: &KernelRates) -> (EngineConfig, f64, f64) {
        for taps in (1..=256).map(|k| k << 16) {
            let mut cfg = base();
            cfg.rate.allowed_rates = vec![48_000, 192_000];
            cfg.chain.upsampler = UpsamplerConfig {
                target: RateTarget::MaxRate(768_000),
                ..manual(taps, 16)
            };
            let costs = landing_costs(rates, &cfg, None).unwrap();
            let at = |r| {
                costs
                    .iter()
                    .find(|(_, w)| w.source_rate == r)
                    .unwrap()
                    .1
                    .load
            };
            let (l48, l192) = (at(48_000), at(192_000));
            if l48 <= RT_BUDGET && l192 > RT_BUDGET {
                return (cfg, l48, l192);
            }
        }
        panic!("no design fits at 48 kHz only");
    }

    #[test]
    fn admission_prices_the_rate_playing_now() {
        let rates = slow_machine();
        let (cfg, l48, l192) = fits_at_48k_only(&rates);
        assert!(l48 < l192, "{l48} {l192}");
        // Playing 48 kHz (or last played it): admitted.
        admit(&rates, &base(), &cfg, None, Some(48_000)).unwrap();
        // Playing 192 kHz: refused, naming the source rate.
        let err = admit(&rates, &base(), &cfg, None, Some(192_000)).unwrap_err();
        assert!(
            err.starts_with("Too heavy for this CPU at 768 kHz from a 192 kHz source: about "),
            "{err}"
        );
        // Nothing played yet: the worst case, as before.
        let err = admit(&rates, &base(), &cfg, None, None).unwrap_err();
        assert!(err.contains("from a 192 kHz source"), "{err}");
        // A rate the config does not offer: the worst case too.
        assert!(admit(&rates, &base(), &cfg, None, Some(96_000)).is_err());
    }

    /// `k` `f` times faster (slower below 1).
    fn scaled(k: &KernelRates, f: f64) -> KernelRates {
        let t = |v: &[(usize, f64)]| v.iter().map(|&(n, r)| (n, r * f)).collect();
        KernelRates {
            dot: t(&k.dot),
            spectral: t(&k.spectral),
            fft: k.fft.iter().map(|&(n, s)| (n, s / f)).collect(),
            mul_add: k.mul_add * f,
            quantizer: k.quantizer.iter().map(|&(d, r)| (d, r * f)).collect(),
            elapsed: k.elapsed,
        }
    }

    /// Where a design does not build and Recommended is over budget, a
    /// genuinely lighter Recommended plays, checked at that landing; the
    /// plan, its row and the price admission and the cost table charge
    /// agree on it. At a rational ratio, where the cost follows the grid
    /// taps (at integer ratios the FFT polyphase engine costs the same for
    /// every length near Recommended's, so Recommended stays).
    #[test]
    fn a_light_fallback_is_lighter_and_fits() {
        let mut cfg = base();
        cfg.rate.allowed_rates = vec![44_100, 48_000];
        cfg.rate.default_rate = 48_000;
        cfg.chain.upsampler =
            crate::control::config::tests::equiripple_1279(RateTarget::Rate(48_000));
        let frac = RateRatio::from_rates(44_100, 48_000);
        assert!(crate::control::config::chain_a_builds_at(&cfg, 44_100, frac).is_err());
        let price = |k: &KernelRates, up: &UpsamplerConfig| {
            let mut c = cfg.clone();
            c.chain.upsampler = up.clone();
            landing_cost(k, &c, None, 44_100, frac).unwrap().load
        };
        let all = fallback_upsamplers(&cfg.chain.upsampler);
        let slow = slow_machine();
        let (rec, lightest) = (price(&slow, &all[0].1), price(&slow, &all[3].1));
        assert!(lightest < rec * 0.8, "{rec} {lightest}");
        // Recommended at 110 % of the budget.
        let k = scaled(&slow, rec / (RT_BUDGET * 1.1));
        assert!(price(&k, &all[0].1) > RT_BUDGET);
        let sub = design_substitute(Some(&k), &cfg, None, 44_100, frac).unwrap();
        assert_eq!(sub.action, OverloadAction::LightUpsampler);
        assert_ne!(sub.up, all[0].1);
        let load = sub.load.unwrap();
        assert!(load <= RT_BUDGET, "{load}");
        assert_eq!(load, price(&k, &sub.up));
        let mut c = cfg.clone();
        c.chain.upsampler = sub.up.clone();
        crate::control::config::chain_builds_at(&c, 44_100, frac).unwrap();

        let (plan, rows) = plan_rates(Some(&k), &cfg, None, None, &[]);
        let row = rows.iter().find(|w| w.source_rate == 44_100).unwrap();
        let f = row.fallback.clone().unwrap();
        assert_eq!(
            (f.action, f.cause),
            (OverloadAction::LightUpsampler, RateFallbackCause::Design)
        );
        assert_eq!((f.load, row.load), (load, load));
        assert_eq!(plan.substitute(&cfg, 44_100), Some(&sub.up));
        let costs = landing_costs(&k, &cfg, None).unwrap();
        let (r, w) = costs.iter().find(|(_, w)| w.source_rate == 44_100).unwrap();
        assert_eq!((*r, w.load), (frac, load));
        assert_eq!(
            admission_case(&k, &cfg, None, Some(44_100))
                .unwrap()
                .unwrap()
                .load,
            load
        );
    }

    #[test]
    fn rates_over_budget_fall_back_per_rate() {
        let rates = slow_machine();
        let (cfg, l48, l192) = fits_at_48k_only(&rates);
        let (plan, rows) = plan_rates(Some(&rates), &cfg, None, None, &[]);
        assert_eq!(rows.len(), 2, "{rows:?}");
        let row = |r: u32| rows.iter().find(|w| w.source_rate == r).unwrap();
        assert_eq!(row(48_000).fallback, None);
        assert_eq!(row(48_000).load, l48);
        assert_eq!(row(192_000).output_rate, 768_000);
        assert_eq!(row(192_000).load, l192);
        let f = row(192_000).fallback.clone().unwrap();
        // Recommended fits here, so Recommended it is (not "light").
        assert_eq!(f.action, OverloadAction::Recommended);
        assert_eq!(f.cause, RateFallbackCause::Estimate);
        assert!(f.load <= RT_BUDGET, "{f:?}");
        // The engine builds Recommended at 192 kHz only, at the requested
        // target (so the device lands where it would).
        let up = plan.substitute(&cfg, 192_000).unwrap();
        assert_eq!(up.target, cfg.chain.upsampler.target);
        assert_eq!(
            *up,
            UpsamplerConfig {
                target: cfg.chain.upsampler.target,
                ..super::super::overload::cheapest_upsampler(&cfg.chain.upsampler)
            }
        );
        assert_eq!(plan.substitute(&cfg, 48_000), None);
        assert_eq!(plan.apply(&cfg, 48_000).as_ref(), &cfg);
        // Only for the request it was made for, with DSP on.
        let mut other = cfg.clone();
        other.chain.upsampler = manual(4095, 16);
        assert_eq!(plan.substitute(&other, 192_000), None);
        let mut off = cfg.clone();
        off.dsp_enabled = false;
        assert_eq!(plan.substitute(&off, 192_000), None);

        // The last setting that ran comes first where it fits.
        let good = manual(1 << 16, 16);
        let (plan, rows) = plan_rates(Some(&rates), &cfg, None, Some(&good), &[]);
        let f = rows
            .iter()
            .find(|w| w.source_rate == 192_000)
            .unwrap()
            .fallback
            .clone()
            .unwrap();
        assert_eq!(f.action, OverloadAction::Reverted);
        assert_eq!(
            plan.substitute(&cfg, 192_000),
            Some(&UpsamplerConfig {
                target: cfg.chain.upsampler.target,
                ..good
            })
        );

        // A rate where the request overloaded falls back too.
        let (plan, rows) = plan_rates(Some(&rates), &cfg, None, None, &[48_000]);
        let f = rows
            .iter()
            .find(|w| w.source_rate == 48_000)
            .unwrap()
            .fallback
            .clone()
            .unwrap();
        assert_eq!(f.cause, RateFallbackCause::Overload);
        assert!(plan.substitute(&cfg, 48_000).is_some());

        // A design that fits everywhere: no plan.
        let mut light = cfg.clone();
        light.chain.upsampler.target = RateTarget::MaxRate(768_000);
        light.chain.upsampler = UpsamplerConfig {
            target: RateTarget::MaxRate(768_000),
            ..manual(4095, 16)
        };
        let (plan, rows) = plan_rates(Some(&rates), &light, None, None, &[]);
        assert_eq!(plan, crate::api::RatePlan::default());
        assert!(rows.iter().all(|r| r.fallback.is_none()), "{rows:?}");
        // DSP off: nothing.
        let (_, rows) = plan_rates(Some(&rates), &off, None, None, &[]);
        assert!(rows.is_empty());
    }

    #[test]
    fn the_device_caps_set_the_rate_it_is_judged_at() {
        let rates = slow_machine();
        let caps = |rates: &[u32]| DeviceCapabilities {
            device: EngineConfig::default().device,
            rates: rates
                .iter()
                .map(|&rate| audivane_protocol::ipc::RateCapability {
                    rate,
                    formats: vec![audivane_protocol::ipc::FormatCapability {
                        format: audivane_protocol::SampleFormat::S32,
                        sbits: None,
                    }],
                })
                .collect(),
            channels: 2,
            channels_min: 2,
            channels_max: 2,
            rate_min: 0,
            rate_max: 0,
            formats: vec![audivane_protocol::SampleFormat::S32],
            modes: Vec::new(),
        };
        let mut cfg = base();
        // Taps per input sample: the kernel lasts as long at every ratio,
        // so x4 has a quarter of the taps of x16.
        cfg.chain.upsampler = audivane_dsp_core::resample::ManualConfig {
            target: RateTarget::Ratio(16),
            taps: 65_536,
            taps_unit: audivane_protocol::TapsUnit::Input,
            ..Default::default()
        }
        .into_upsampler_config()
        .unwrap();
        let all = worst_case(&rates, &cfg, None).unwrap().unwrap();
        assert_eq!(all.output_rate, 768_000);
        // A DAC that stops at 192 kHz runs it at x4: about a quarter.
        let low = worst_case(
            &rates,
            &cfg,
            Some(&caps(&[44_100, 48_000, 88_200, 96_000, 176_400, 192_000])),
        )
        .unwrap()
        .unwrap();
        assert_eq!(low.output_rate, 192_000);
        assert!(low.load < all.load / 2.0, "{low:?} vs {all:?}");
    }

    fn s32_caps(rates: &[u32]) -> DeviceCapabilities {
        DeviceCapabilities {
            device: EngineConfig::default().device,
            rates: rates
                .iter()
                .map(|&rate| audivane_protocol::ipc::RateCapability {
                    rate,
                    formats: vec![audivane_protocol::ipc::FormatCapability {
                        format: audivane_protocol::SampleFormat::S32,
                        sbits: None,
                    }],
                })
                .collect(),
            channels: 2,
            channels_min: 2,
            channels_max: 2,
            rate_min: 0,
            rate_max: 0,
            formats: vec![audivane_protocol::SampleFormat::S32],
            modes: Vec::new(),
        }
    }

    /// The Upsampling preview on a 48 kHz-only DAC with "Highest, same
    /// family, up to 768 kHz": 48 kHz lands 1:1, 44.1 and 96 kHz are
    /// converted by PipeWire, and "Best for DAC" would play them in the
    /// engine. The cost table follows the same landings.
    #[test]
    fn previews_resolve_where_the_device_lands() {
        let mut cfg = EngineConfig::default();
        cfg.rate.allowed_rates = vec![44_100, 48_000, 96_000];
        let up = UpsamplerConfig {
            target: RateTarget::MaxRate(768_000),
            preset: Some("recommended".into()),
            design: None,
        };
        let caps = s32_caps(&[48_000]);
        let l = crate::engine::source_landings(&cfg, &up, Some(&caps), &[44_100, 48_000, 96_000]);
        let at = |rate: u32| l.iter().find(|x| x.source_rate == rate).unwrap();
        assert_eq!(at(44_100).converted_to, Some(48_000));
        assert_eq!(at(44_100).requested, RateRatio::integer(16));
        let best = at(44_100).best_for_dac.unwrap();
        assert_eq!(best.target, RateTarget::MaxRateAny(768_000));
        assert_eq!(best.ratio, RateRatio::new(160, 147));
        assert_eq!(at(48_000).converted_to, None);
        assert_eq!(at(48_000).ratio, RateRatio::ONE);
        assert_eq!(at(48_000).best_for_dac, None);
        assert_eq!(at(96_000).converted_to, Some(48_000));
        assert_eq!(
            at(96_000).best_for_dac.map(|b| b.ratio),
            Some(RateRatio::new(1, 2))
        );
        // Best for DAC itself: 44.1 kHz plays at 160/147, no hint.
        let any = UpsamplerConfig {
            target: RateTarget::MaxRateAny(768_000),
            ..up.clone()
        };
        let l = crate::engine::source_landings(&cfg, &any, Some(&caps), &[44_100]);
        assert_eq!(l[0].ratio, RateRatio::new(160, 147));
        assert_eq!((l[0].converted_to, l[0].best_for_dac), (None, None));
        // No probe: nothing device-aware.
        assert!(crate::engine::source_landings(&cfg, &up, None, &[44_100]).is_empty());
        assert!(crate::engine::preview_landings(&cfg, &up, None).is_none());

        // The cost table: no x16 row anywhere, since nothing upsamples here.
        let design = audivane_protocol::StoredDesign::based_on(
            "recommended",
            audivane_dsp_core::design::catalogue::by_name("recommended").unwrap(),
        );
        let output = cfg.chain.output;
        let check = |landings: Option<&[(u32, RateRatio)]>| {
            super::super::config::check_design(
                &design,
                Some(up.target),
                None,
                &cfg,
                &output,
                &slow_machine(),
                landings,
            )
        };
        let landings = crate::engine::preview_landings(&cfg, &up, Some(&caps)).unwrap();
        assert_eq!(landings, vec![(48_000, RateRatio::ONE)]);
        let device = check(Some(&landings));
        assert!(device.errors.is_empty(), "{:?}", device.errors);
        assert!(device.plans.is_empty(), "{:?}", device.plans);
        // Without a probe: every allowed rate at the target's own ratio.
        let plain = check(None);
        assert_eq!(plain.plans.len(), 3, "{:?}", plain.plans);
    }

    #[test]
    fn it_is_judged_where_the_driver_lands() {
        let rates = slow_machine();
        // A 44.1 kHz source, "highest rate up to 192 kHz in either family",
        // on a DAC with only 44.1 and 192 kHz: the driver lands at 192 kHz
        // (a rational ratio), not at the x1 the same-family ladder would
        // reach.
        let mut cfg = base();
        cfg.rate.allowed_rates = vec![44_100];
        cfg.rate.default_rate = 44_100;
        cfg.chain.upsampler = audivane_dsp_core::resample::ManualConfig {
            target: RateTarget::MaxRateAny(192_000),
            taps: 256,
            taps_unit: audivane_protocol::TapsUnit::Input,
            ..Default::default()
        }
        .into_upsampler_config()
        .unwrap();
        let caps = s32_caps(&[44_100, 192_000]);
        let landings = crate::engine::predicted_landings(&cfg, Some(&caps)).unwrap();
        assert_eq!(landings, vec![(44_100, RateRatio::new(640, 147))]);
        let w = worst_case(&rates, &cfg, Some(&caps)).unwrap().unwrap();
        assert_eq!((w.source_rate, w.output_rate), (44_100, 192_000));
        assert!(w.load > 0.0, "{w:?}");
        // None of the configured rates plays (48 kHz, x4 on a DAC with
        // only 44.1 and 176.4 kHz): the driver widens to the engine's rates
        // the device plays, and so does the estimate.
        cfg.rate.allowed_rates = vec![48_000];
        cfg.rate.default_rate = 48_000;
        cfg.chain.upsampler = manual(65_536, 4);
        let caps = s32_caps(&[44_100, 176_400]);
        let landings = crate::engine::predicted_landings(&cfg, Some(&caps)).unwrap();
        assert!(
            landings.contains(&(44_100, RateRatio::integer(4))),
            "{landings:?}"
        );
        assert!(!landings.iter().any(|&(rate, _)| rate == 48_000));
        // Priced at the widened rates (44.1 x4 or 88.2 x2), never at 48 kHz.
        let w = worst_case(&rates, &cfg, Some(&caps)).unwrap().unwrap();
        assert_eq!(w.output_rate, 176_400, "{w:?}");
        assert!([44_100, 88_200].contains(&w.source_rate), "{w:?}");
    }

    #[test]
    fn a_throttled_benchmark_is_measured_again_and_floored() {
        let floor = KernelRates::floor();
        let scaled = |k: f64| KernelRates {
            dot: floor.dot.iter().map(|&(b, r)| (b, r * k)).collect(),
            spectral: floor.spectral.iter().map(|&(b, r)| (b, r * k)).collect(),
            fft: floor.fft.iter().map(|&(n, t)| (n, t / k)).collect(),
            mul_add: floor.mul_add * k,
            quantizer: floor.quantizer.iter().map(|&(q, r)| (q, r * k)).collect(),
            elapsed: std::time::Duration::from_millis(200),
        };
        // A normal machine: measured once.
        let mut calls = 0;
        let (r, floored) = settle_rates(scaled(4.0), || {
            calls += 1;
            scaled(4.0)
        });
        assert_eq!((calls, floored), (0, false));
        assert_eq!(r.reference_rate(), floor.reference_rate() * 4.0);
        // A busy start: the second measurement is the real one.
        let mut calls = 0;
        let (r, floored) = settle_rates(scaled(0.2), || {
            calls += 1;
            scaled(3.0)
        });
        assert_eq!((calls, floored), (1, false));
        assert_eq!(r.reference_rate(), floor.reference_rate() * 3.0);
        // Slow every time: the floor, so ordinary presets stay admitted.
        let mut calls = 0;
        let (r, floored) = settle_rates(scaled(0.2), || {
            calls += 1;
            scaled(0.3)
        });
        assert_eq!((calls, floored), (REMEASURES, true));
        assert_eq!(r.reference_rate(), floor.reference_rate());
        assert!(!r.below(&floor));
        let mut cfg = base();
        cfg.chain.upsampler = UpsamplerConfig {
            target: RateTarget::Ratio(8),
            preset: Some(FilterPreset::GaussianLong.name().to_string()),
            ..UpsamplerConfig::default()
        };
        admit(&r, &base(), &cfg, None, None).unwrap();
    }

    #[test]
    fn rates_are_written_in_khz() {
        assert_eq!(khz(768_000), "768 kHz");
        assert_eq!(khz(705_600), "705.6 kHz");
        assert_eq!(khz(44_100), "44.1 kHz");
        assert_eq!(khz(3_072_000), "3072 kHz");
    }

    /// The estimate before the cost model moved into the design budget,
    /// kept verbatim for [`the_cost_model_moved_unchanged`].
    mod legacy {
        use super::super::CHANNELS;
        use audivane_dsp_core::cpu_bench::KernelRates;
        use audivane_dsp_core::design::{self, FilterDesign, Grid, catalogue};
        use audivane_dsp_core::presets::{self, FilterPreset};

        /// The runtime kind of a preset (the old `presets::kind`).
        #[derive(PartialEq)]
        enum Kind {
            Fir,
            Cascade,
            Interpolator,
            BlockFft,
        }

        fn kind(p: FilterPreset) -> Kind {
            match catalogue::design(p) {
                FilterDesign::Kernel(_) => Kind::Fir,
                FilterDesign::Cascade(_) => Kind::Cascade,
                FilterDesign::Interpolator(_) => Kind::Interpolator,
                FilterDesign::BlockFft(_) => Kind::BlockFft,
            }
        }
        use audivane_protocol::{FilterPhase, RateRatio, RateTarget};

        const OUT_OVERHEAD: f64 = 2.0;
        const IIR_PER_ORDER: f64 = 4.0;
        const POLY_PER_OUTPUT: f64 = 8.0;
        const TABLE_SINC_PER_OUTPUT: f64 = 127.0 * 1.2;
        const FFT_ENGINE_OVERHEAD: f64 = 1.15;
        const BLOCK_FFT_OVERHEAD: f64 = 1.8;

        fn engine_block(max_block: usize) -> usize {
            max_block.next_power_of_two().clamp(64, 1024)
        }

        /// NaN where the engine now runs more than one stage (its estimate
        /// moved on purpose: non-uniform partitions).
        fn fft_polyphase_secs(
            rates: &KernelRates,
            taps: usize,
            r: usize,
            b: usize,
            fs: f64,
        ) -> f64 {
            if design::budget::polyphase_stages(taps, r, b) > 1 {
                return f64::NAN;
            }
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

        fn rational_secs(rates: &KernelRates, grid_taps: usize, ratio: RateRatio, fs: f64) -> f64 {
            let out_rate = fs * ratio.as_f64();
            let per_out = grid_taps.div_ceil(ratio.up.max(1) as usize) as f64;
            out_rate * per_out / rates.dot_rate(grid_taps * 8) + out_overhead(rates, out_rate)
        }

        fn intermediate_growth(beta: f64) -> f64 {
            (1.45 + (14.0 - beta).max(0.0) * (3.1 - 1.45) / 12.0).clamp(1.45, 3.1)
        }

        fn preset_kernel_taps(
            p: FilterPreset,
            rate: u32,
            ratio: RateRatio,
        ) -> Result<usize, String> {
            let d = catalogue::design(p);
            let FilterDesign::Kernel(k) = &d else {
                return Err(format!("{} is not a single-kernel preset", p.name()));
            };
            let grid = Grid::new(rate, ratio);
            let mut t = design::length::per_input(k, grid.up, rate).map_err(|e| e.to_string())?;
            if grid.decimating {
                t = design::length::decimated(t, &k.decimation, &grid);
            }
            let n = t * grid.up;
            Ok(match k.phase {
                design::Phase::Intermediate { .. } => 2 * n,
                _ => n,
            })
        }

        fn iir_order(p: FilterPreset, f: usize) -> usize {
            match (p, f) {
                (FilterPreset::CascadeElliptic, 2) => 16,
                (FilterPreset::CascadeElliptic, _) => 17,
                (_, 2) => 30,
                _ => 24,
            }
        }

        fn fixed_runtime_secs(
            rates: &KernelRates,
            p: FilterPreset,
            r: usize,
            fs: f64,
        ) -> Result<f64, String> {
            let out = fs * r as f64;
            let reference = rates.reference_rate();
            Ok(match kind(p) {
                Kind::Cascade => {
                    let mut secs = 0.0;
                    let mut rate = fs;
                    for f in presets::cascade::stage_factors(r) {
                        if matches!(
                            p,
                            FilterPreset::CascadeElliptic | FilterPreset::CascadeInverseChebyshev
                        ) {
                            secs += rate * f as f64 * iir_order(p, f) as f64 * IIR_PER_ORDER
                                / reference;
                        } else {
                            let taps = presets::cascade::stage_kernel(p, f)
                                .map_err(|e| e.to_string())?
                                .len();
                            let b = taps.div_ceil(f).next_power_of_two().max(8);
                            secs += fft_polyphase_secs(rates, taps, f, b, rate);
                        }
                        rate *= f as f64;
                    }
                    secs
                }
                Kind::Interpolator => {
                    if p == FilterPreset::TableSinc {
                        out * TABLE_SINC_PER_OUTPUT / rates.mul_add
                    } else {
                        out * POLY_PER_OUTPUT / reference
                    }
                }
                Kind::BlockFft => {
                    let (b, normal) = match p {
                        FilterPreset::BlockFftFast => (16_384usize, false),
                        FilterPreset::BlockFftLarge => ((1usize << 20) / r, true),
                        _ => (16_384, true),
                    };
                    let hop = if normal { b / 4 } else { b / 2 };
                    let l = b.min(1 << 15);
                    let q = r * b / l;
                    let blocks = fs / hop as f64;
                    let per_block = rates.fft_seconds(b)
                        + q as f64 * rates.fft_seconds(l)
                        + (r * b / 2) as f64 / rates.spectral_rate(b * 8);
                    blocks * per_block * BLOCK_FFT_OVERHEAD + out_overhead(rates, out)
                }
                Kind::Fir => unreachable!("single-kernel presets run the polyphase engines"),
            })
        }

        pub fn upsampler_load(
            rates: &KernelRates,
            up: &audivane_dsp_core::resample::ManualConfig,
            source_rate: u32,
            ratio: RateRatio,
            max_block: usize,
        ) -> Result<f64, String> {
            if ratio.is_unity() || up.target == RateTarget::Off {
                return Ok(0.0);
            }
            let fs = f64::from(source_rate);
            let per_channel = match up.preset {
                Some(p) if kind(p) != Kind::Fir => {
                    let r = ratio
                        .as_integer()
                        .ok_or_else(|| format!("{} runs at integer ratios only", p.name()))?;
                    fixed_runtime_secs(rates, p, r as usize, fs)?
                }
                Some(p) => {
                    let taps = preset_kernel_taps(p, source_rate, ratio)?;
                    match ratio.as_integer() {
                        Some(r) => {
                            fft_polyphase_secs(rates, taps, r as usize, engine_block(max_block), fs)
                        }
                        None => rational_secs(rates, taps, ratio, fs),
                    }
                }
                None => {
                    let d = audivane_dsp_core::resample::UpsamplerDesign::try_for_ratio(up, ratio)
                        .map_err(|e| e.to_string())?;
                    let mixed = up.phase == FilterPhase::Intermediate
                        && up.phase_mix > 0.0
                        && up.phase_mix < 1.0;
                    let taps = if mixed {
                        (d.taps as f64 * intermediate_growth(d.kaiser_beta)).ceil() as usize
                    } else {
                        d.taps
                    };
                    match ratio.as_integer() {
                        Some(r) => {
                            fft_polyphase_secs(rates, taps, r as usize, engine_block(max_block), fs)
                        }
                        None => rational_secs(rates, taps, ratio, fs),
                    }
                }
            };
            Ok(per_channel * CHANNELS as f64)
        }
    }

    /// Pins the admission's estimate to the legacy estimator, bit for bit,
    /// except where the FFT polyphase engine runs one stage (its non-uniform
    /// partitions are priced differently on purpose): over every preset at
    /// every source rate, integer ratio and ladder output rate, and a few
    /// Custom designs (fixed, per-input, minimum, blended intermediate, band,
    /// automatic), on two machines and several chain blocks.
    #[test]
    #[ignore = "refactor pin against removed code; run with tier B"]
    fn the_cost_model_moved_unchanged() {
        use audivane_dsp_core::resample::ManualConfig;
        use audivane_protocol::{
            ALLOWED_RATES, BandSpec, CutoffMode, FilterErrorBudget, FilterPhase, FilterPreset,
            TapsUnit,
        };
        let mut ups: Vec<ManualConfig> = FilterPreset::ALL
            .iter()
            .map(|&p| ManualConfig {
                preset: Some(p),
                target: RateTarget::MaxRate(768_000),
                ..ManualConfig::default()
            })
            .collect();
        let custom = ManualConfig {
            preset: None,
            target: RateTarget::MaxRate(768_000),
            ..ManualConfig::default()
        };
        let band = BandSpec {
            passband_end: 0.9,
            passband_end_hz: None,
            stopband_start: 1.0,
            attenuation_db: 120.0,
        };
        ups.extend([
            ManualConfig {
                taps: 65_536,
                ..custom.clone()
            },
            ManualConfig {
                taps: 255,
                taps_unit: TapsUnit::Input,
                ..custom.clone()
            },
            ManualConfig {
                taps: 8192,
                phase: FilterPhase::Minimum,
                ..custom.clone()
            },
            ManualConfig {
                taps: 8192,
                phase: FilterPhase::Intermediate,
                phase_mix: 0.5,
                kaiser_beta: 9.0,
                ..custom.clone()
            },
            ManualConfig {
                cutoff_mode: CutoffMode::Band,
                band,
                ..custom.clone()
            },
            ManualConfig {
                cutoff_mode: CutoffMode::Band,
                band,
                auto_taps: Some(FilterErrorBudget::Db(120.0)),
                phase: FilterPhase::Minimum,
                ..custom.clone()
            },
        ]);
        let mut cases = Vec::new();
        for &rate in ALLOWED_RATES {
            for r in 1..=64u32 {
                if u64::from(rate) * u64::from(r) <= u64::from(audivane_protocol::MAX_OUTPUT_RATE) {
                    cases.push((rate, RateRatio::integer(r)));
                }
            }
            for out in audivane_io_pw::lifecycle::MASK_OUTPUT_RATES {
                let r = RateRatio::from_rates(rate, out);
                if !r.is_integer() && r.up <= 4096 && r.down <= 4096 {
                    cases.push((rate, r));
                }
            }
        }
        let machines = [KernelRates::floor(), slow_machine()];
        let (mut same, mut errors, mut moved) = (0usize, 0usize, 0usize);
        for up in &ups {
            let wire = up.clone().into_upsampler_config().unwrap();
            let auto = up.auto_taps.is_some();
            for (i, &(rate, ratio)) in cases.iter().enumerate() {
                // The automatic search is slow in a debug build: a few ratios;
                // the other Custom designs every third case.
                if auto && !matches!(ratio.up, 1 | 2 | 8 | 160) {
                    continue;
                }
                if up.preset.is_none() && i % 3 != 0 {
                    continue;
                }
                // The phase-transformed cascade stages are designed on every
                // call (about 30 ms): every 13th case.
                let slow = matches!(
                    up.preset,
                    Some(
                        FilterPreset::CascadeFirMp
                            | FilterPreset::CascadeFirAsym
                            | FilterPreset::CascadeFft
                    )
                );
                if slow && i % 13 != 0 {
                    continue;
                }
                // Custom designs: the cases the daemon validates (no
                // rational kernel over the budget is ever priced).
                if up.preset.is_none()
                    && !ratio.is_integer()
                    && super::super::config::check_rational_at("u", &wire, rate, ratio, 2).is_err()
                {
                    continue;
                }
                // Every case on one machine and block; every 13th on both
                // machines and three blocks (the debug build is slow).
                let (machines, blocks): (&[KernelRates], &[usize]) = if i % 13 == 0 {
                    (&machines, &[256, 1024, 4096])
                } else {
                    (&machines[..1], &[1024])
                };
                for rates in machines {
                    for &block in blocks {
                        let old = legacy::upsampler_load(rates, up, rate, ratio, block);
                        let new = upsampler_load(rates, &wire, rate, ratio, block);
                        match (&old, &new) {
                            (Ok(a), Ok(_)) if a.is_nan() => moved += 1,
                            (Ok(a), Ok(b)) => {
                                assert_eq!(
                                    a.to_bits(),
                                    b.to_bits(),
                                    "{:?} {rate} {ratio}: {a} vs {b}",
                                    up.preset
                                );
                                same += 1;
                            }
                            (Err(a), Err(b)) => {
                                assert_eq!(a, b, "{:?} {rate} {ratio}", up.preset);
                                errors += 1;
                            }
                            _ => panic!("{:?} {rate} {ratio}: {old:?} vs {new:?}", up.preset),
                        }
                    }
                }
            }
        }
        eprintln!(
            "{same} estimates bit-equal, {errors} refusals equal, {moved} on non-uniform \
             partitions (not compared)"
        );
        assert!(same > 25_000, "{same}");
    }
}
