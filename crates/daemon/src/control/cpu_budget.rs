//! CPU admission for the upsampler: estimate what a design costs the audio
//! thread at the output rate it would run at, and refuse a new one this
//! machine cannot run in real time.
//!
//! The estimate is the design's own callback work
//! ([`audivane_dsp_core::design::budget::Callback`]) priced with this machine's
//! rates for that kind of work, measured once at daemon start
//! ([`audivane_dsp_core::cpu_bench`]), times the channels and the pipelines
//! running at once. This module keeps what is machine-specific: the
//! benchmark, the sustained rate, the admission policy and its refusal.
//!
//! Spectral and dot rates depend on how much filter data each sweep touches
//! (in cache or streamed), so the benchmark measures them at several sizes
//! and the estimate picks the one for the design. A design is admitted up to
//! [`RT_BUDGET`]: the audio thread runs the rest of the chain too, and a swap
//! briefly runs two chains; what overloads anyway goes to the fallback
//! ([`super::overload`]). Figures in multiply-adds per second are equivalents
//! at the in-cache dot rate, so needed and sustained compare directly.
//!
//! An A/B output compare (B's own bits or dither) runs a second quantizer on
//! the audio thread; its cost, at the measured rate of B's dither kind
//! ([`KernelRates::quantizer_rate`]), counts with side B's upsampler.
//!
//! The design workspace cap ([`init_design_workspace`]) lives here too: a
//! quarter of the total RAM within
//! [`design::budget::DESIGN_WORKSPACE_FLOOR`] ..=
//! [`design::budget::DESIGN_WORKSPACE_CEILING`], read once at start-up.
//!
//! Only client changes are admitted (`set_upsampler`, `set_config`, `set_ab`,
//! presets): the saved config at start is never refused.

use audivane_dsp_core::cpu_bench::KernelRates;
use audivane_dsp_core::design::{self, BuildContext, FilterDesign};
use audivane_protocol::ipc::{DeviceCapabilities, EngineConfig};
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

/// The worst case of a config: the rate and the load.
#[derive(Clone, Debug, PartialEq)]
pub struct Worst {
    pub source_rate: u32,
    pub output_rate: u32,
    /// Fraction of one core, every pipeline included.
    pub load: f64,
    /// A/B's side B share of [`Self::load`] (`None` without A/B): its
    /// upsampler and, in an output compare, its own quantizer.
    pub load_b: Option<f64>,
}

/// The heaviest source rate of `cfg` (its upsamplers, chain A and B
/// together) where the engine would land it: the graph rates the driver
/// would offer and the ratio a fresh open picks at each
/// ([`crate::engine::predicted_landings`], the driver's own rate policy and
/// ladder, widened rates included).
pub fn worst_case(
    rates: &KernelRates,
    cfg: &EngineConfig,
    caps: Option<&DeviceCapabilities>,
) -> Result<Option<Worst>, String> {
    if !cfg.dsp_enabled {
        return Ok(None);
    }
    // B's filter as built: with A's bits and dither.
    let chains = super::ab_side::designed_chains(cfg);
    let mut worst: Option<Worst> = None;
    for (rate, r) in crate::engine::predicted_landings(cfg, caps)? {
        let max_block = audivane_io_pw::scaled_quantum(cfg.quantum, rate).clamp(512, 8192) as usize;
        let mut load = 0.0;
        let mut load_b = None;
        let output_rate = r.output_rate(rate).unwrap_or(rate);
        for (i, chain) in chains.iter().enumerate() {
            // Every budget the engine can resolve to: the heaviest. Chain B
            // runs at chain A's landing.
            let mut heaviest = 0.0f64;
            for up in super::config::upsampler_variants(chain) {
                heaviest = heaviest.max(upsampler_load(rates, &up, rate, r, max_block)?);
            }
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
        if worst.as_ref().is_none_or(|w| load > w.load) {
            worst = Some(Worst {
                source_rate: rate,
                output_rate,
                load,
                load_b,
            });
        }
    }
    Ok(worst)
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
    let needed = worst.load * CHANNELS as f64 * rates.reference_rate();
    if let Some(b) = worst.load_b
        && b > 0.0
        && worst.load - b <= RT_BUDGET
    {
        return format!(
            "Too heavy for this CPU at {} with A/B: side B's upsampler (and its own \
             quantizer, when its bits or dither differ) needs about {:.1} G MAC/s ({:.0}% of \
             one core) on top of chain A's, {:.1} G MAC/s in all, and this machine sustains \
             about {:.1} G MAC/s in real time. Pick a lighter filter for side B, or turn A/B \
             off.",
            khz(worst.output_rate),
            b * CHANNELS as f64 * rates.reference_rate() / 1e9,
            b * 100.0,
            needed / 1e9,
            sustained_macs(rates) / 1e9
        );
    }
    format!(
        "Too heavy for this CPU at {}: about {:.1} G MAC/s needed ({:.0}% of one core), this \
         machine sustains about {:.1} G MAC/s in real time. Use fewer taps or a lower output \
         rate.",
        khz(worst.output_rate),
        needed / 1e9,
        worst.load * 100.0,
        sustained_macs(rates) / 1e9
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
    )
}

/// Refuse `next` when it brings a new upsampler (or a rate, device or A/B
/// change that makes the current one run elsewhere) this machine cannot
/// run in real time ([`RT_BUDGET`]). A change elsewhere in the chain is
/// never refused, so a client can still edit the EQ of a config that
/// overloads.
pub fn admit(
    rates: &KernelRates,
    old: &EngineConfig,
    next: &EngineConfig,
    caps: Option<&DeviceCapabilities>,
) -> Result<(), String> {
    if upsampler_part(old) == upsampler_part(next) {
        return Ok(());
    }
    match worst_case(rates, next, caps)? {
        Some(w) if w.load > RT_BUDGET => {
            log::info!(
                "refused an upsampler change: {:.0}% of a core at {} (budget {:.0}%)",
                w.load * 100.0,
                khz(w.output_rate),
                RT_BUDGET * 100.0
            );
            Err(refusal(rates, &w))
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use audivane_dsp_core::presets::{FilterPreset, UpsamplerRuntime};
    use std::time::Instant;

    fn manual(taps: usize, ratio: u32) -> UpsamplerConfig {
        audivane_dsp_core::resample::ManualConfig {
            target: RateTarget::Ratio(ratio),
            taps,
            ..Default::default()
        }
        .into_upsampler_config()
        .unwrap()
    }

    /// Measured seconds per second of audio of `rt` (stereo, `block`
    /// input frames per call), best of three runs of `seconds` of audio.
    fn measure(rt: &mut dyn UpsamplerRuntime, rate: u32, ratio: RateRatio, block: usize) -> f64 {
        let seconds = 0.4;
        let x: Vec<f64> = (0..block).map(|i| (i as f64 * 0.01).sin() * 0.5).collect();
        let max_out = ratio.max_frames_out(block);
        let mut l = vec![0.0; max_out];
        let mut r = vec![0.0; max_out];
        let calls = ((f64::from(rate) * seconds) / block as f64).ceil() as usize;
        let mut best = f64::INFINITY;
        for _ in 0..3 {
            let t0 = Instant::now();
            for _ in 0..calls {
                let n = rt.output_frames(block).min(max_out);
                rt.process(&[&x, &x], &mut [&mut l[..n], &mut r[..n]]);
            }
            let audio = (calls * block) as f64 / f64::from(rate);
            best = best.min(t0.elapsed().as_secs_f64() / audio);
        }
        std::hint::black_box(&l);
        best
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
    /// every path: never more than 30 % under.
    #[test]
    fn the_estimate_does_not_under_estimate_measured_callbacks() {
        let rates = audivane_dsp_core::cpu_bench::measure();
        let block = 1024;
        let preset = |p: FilterPreset| UpsamplerConfig {
            target: RateTarget::Ratio(8),
            preset: Some(p.name().to_string()),
            ..UpsamplerConfig::default()
        };
        let cases: Vec<(&str, UpsamplerConfig, u32, RateRatio)> = vec![
            (
                "manual 65536 x16",
                manual(65_536, 16),
                48_000,
                RateRatio::integer(16),
            ),
            (
                "manual 1048576 x16",
                manual(1 << 20, 16),
                48_000,
                RateRatio::integer(16),
            ),
            (
                "manual 4095 x4",
                manual(4095, 4),
                44_100,
                RateRatio::integer(4),
            ),
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
            ),
            (
                "cascade_fir x8",
                preset(FilterPreset::CascadeFir),
                48_000,
                RateRatio::integer(8),
            ),
            (
                "cascade_elliptic x8",
                preset(FilterPreset::CascadeElliptic),
                48_000,
                RateRatio::integer(8),
            ),
            (
                "table_sinc x8",
                preset(FilterPreset::TableSinc),
                48_000,
                RateRatio::integer(8),
            ),
            (
                "cosine x8",
                preset(FilterPreset::Cosine),
                48_000,
                RateRatio::integer(8),
            ),
            (
                "block_fft x8",
                preset(FilterPreset::BlockFft),
                48_000,
                RateRatio::integer(8),
            ),
        ];
        let mut report = Vec::new();
        for (name, up, rate, ratio) in cases {
            let mut rt = build(&up, rate, ratio, block);
            let estimate = upsampler_load(&rates, &up, rate, ratio, block).unwrap();
            // Machine load only ever makes a callback slower: when a case
            // looks under-estimated, measure again and keep the fastest.
            let mut measured = measure(rt.as_mut(), rate, ratio, block);
            for _ in 0..3 {
                if estimate / measured >= 0.7 {
                    break;
                }
                measured = measured.min(measure(rt.as_mut(), rate, ratio, block));
            }
            report.push(format!(
                "{name}: estimate {:.4} measured {:.4} (x{:.2})",
                estimate,
                measured,
                estimate / measured
            ));
        }
        eprintln!("{}", report.join("\n"));
        for line in &report {
            let x: f64 = line
                .rsplit("(x")
                .next()
                .unwrap()
                .trim_end_matches(')')
                .parse()
                .unwrap();
            assert!(x >= 0.7, "under-estimated: {line}");
        }
    }

    fn slow_machine() -> KernelRates {
        KernelRates {
            dot: vec![(256 << 10, 1.0e9), (8 << 20, 0.8e9)],
            spectral: vec![(256 << 10, 0.25e9), (8 << 20, 0.2e9), (64 << 20, 0.12e9)],
            fft: vec![(2048, 1.0e-9), (32768, 1.0e-9)],
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

    #[test]
    fn a_heavy_design_is_refused_with_a_plain_message() {
        let rates = slow_machine();
        let old = base();
        let err = admit(&rates, &old, &heavy(), None).unwrap_err();
        assert!(
            err.starts_with("Too heavy for this CPU at 768 kHz: about "),
            "{err}"
        );
        assert!(err.contains("G MAC/s needed"), "{err}");
        assert!(
            err.contains("this machine sustains about 1.8 G MAC/s in real time"),
            "{err}"
        );
        assert!(
            err.ends_with("Use fewer taps or a lower output rate."),
            "{err}"
        );
        // A light design passes, and an edit that leaves the upsampler
        // alone passes even when the current one is too heavy.
        let mut light = base();
        light.chain.upsampler = manual(4095, 4);
        admit(&rates, &old, &light, None).unwrap();
        let mut eq_edit = heavy();
        eq_edit.chain.eq = None;
        eq_edit.dsp_enabled = true;
        admit(&rates, &heavy(), &eq_edit, None).unwrap();
        // DSP off runs nothing.
        let mut off = heavy();
        off.dsp_enabled = false;
        admit(&rates, &old, &off, None).unwrap();
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
        let err = admit(&rates, &light, &next, None).unwrap_err();
        assert!(err.contains("with A/B: side B's upsampler"), "{err}");
        assert!(
            err.ends_with("Pick a lighter filter for side B, or turn A/B off."),
            "{err}"
        );
        // Chain A too heavy on its own: the plain refusal.
        let mut both = heavy();
        both.ab = Some(AbConfig::new(AbB::Patch(serde_json::json!({}))));
        assert!(super::super::config::normalize_config(&mut both).is_none());
        let err = admit(&rates, &base(), &both, None).unwrap_err();
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
        for taps in (1..=64).map(|k| k << 16) {
            let (cfg, l) = load(taps);
            if l > 0.75 && l <= RT_BUDGET && admitted.is_none() {
                admitted = Some((cfg, l));
            } else if l > RT_BUDGET && refused.is_none() {
                refused = Some((cfg, l));
            }
        }
        let (cfg, l) = admitted.expect("a design between three quarters and 90 % of a core");
        admit(&rates, &base(), &cfg, None).unwrap_or_else(|e| panic!("{l}: {e}"));
        let (cfg, l) = refused.expect("a design above 90 % of a core");
        assert!(admit(&rates, &base(), &cfg, None).is_err(), "{l}");
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
        }
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
        admit(&r, &base(), &cfg, None).unwrap();
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

        fn fft_polyphase_secs(
            rates: &KernelRates,
            taps: usize,
            r: usize,
            b: usize,
            fs: f64,
        ) -> f64 {
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

    /// The admission's estimate is the design budget's now; it is the same
    /// number, bit for bit, as the estimator it replaced, over every preset
    /// at every source rate, integer ratio and ladder output rate, and a
    /// few Custom designs (fixed, per-input, minimum, blended intermediate,
    /// band, automatic), on two machines and several chain blocks.
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
        let (mut same, mut errors) = (0usize, 0usize);
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
        eprintln!("{same} estimates bit-equal, {errors} refusals equal");
        assert!(same > 25_000, "{same}");
    }
}
