//! Pure, hardware-free lifecycle state machines for ratio resolution, sink
//! transition checks, and `ReserveDevice1` + ALSA device handoff.

use crate::engine_driver::ReleaseKind;
use audivane_protocol::SampleFormat;
use audivane_protocol::ipc::{DeviceCapabilities, EngineState, SinkChoice};
use audivane_protocol::{MAX_OUTPUT_RATE, RateRatio, RateTarget, resolve_rate_ratio};

/// Largest `up` or `down` of a rational ratio the engine opens a device
/// for (44.1k -> 768k is 2560/147).
pub const MAX_RATIONAL_TERM: u32 = 4096;

/// Every output rate a ratio ladder can offer: the multiples ×1..=16 of
/// [`audivane_protocol::ALLOWED_RATES`] up to 768 kHz, then
/// [`audivane_protocol::HIGH_OUTPUT_RATES`] (×32 and ×64 of 44.1 and
/// 48 kHz, up to [`audivane_protocol::MAX_OUTPUT_RATE`]): the rates the
/// driver probes a device for (continuous-range devices and the loopback
/// report them all). Other multiples above 768 kHz (44.1 kHz ×24) are no
/// DAC rate and never offered: a ratio that lands there falls back down
/// its ladder.
pub const MASK_OUTPUT_RATES: [u32; 36] = [
    44_100, 48_000, 88_200, 96_000, 132_300, 144_000, 176_400, 192_000, 220_500, 240_000, 264_600,
    288_000, 308_700, 336_000, 352_800, 384_000, 396_900, 432_000, 441_000, 480_000, 485_100,
    528_000, 529_200, 573_300, 576_000, 617_400, 624_000, 661_500, 672_000, 705_600, 720_000,
    768_000, 1_411_200, 1_536_000, 2_822_400, 3_072_000,
];

const _: () = assert!(MASK_OUTPUT_RATES.len() <= u64::BITS as usize);
const _: () =
    assert!(MASK_OUTPUT_RATES[MASK_OUTPUT_RATES.len() - 1] == audivane_protocol::MAX_OUTPUT_RATE);

/// Which `(input rate, output rate)` pairs the configured DSP chain can be
/// built for: inputs from [`audivane_protocol::ALLOWED_RATES`] (the graph
/// rates), outputs from [`MASK_OUTPUT_RATES`], one bit per pair. The control
/// side computes it from the upsampler designs (preset ratio rules, the
/// rational cost limit, both A/B sides), so a ratio ladder never offers a
/// ratio whose chain would fail to build. ×1 always counts as buildable; any
/// other pair outside the two lists does not, except under [`Self::ALL`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RatePairMask {
    rows: [u64; 10],
    unlisted: bool,
}

const _: () = assert!(audivane_protocol::ALLOWED_RATES.len() == 10);

impl RatePairMask {
    /// Every pair buildable (no restriction).
    pub const ALL: Self = Self {
        rows: [u64::MAX; 10],
        unlisted: true,
    };

    /// No pair buildable but x1.
    pub const NONE: Self = Self {
        rows: [0; 10],
        unlisted: false,
    };

    fn bit(input: u32, output: u32) -> Option<(usize, u64)> {
        let i = audivane_protocol::ALLOWED_RATES
            .iter()
            .position(|&r| r == input)?;
        let o = MASK_OUTPUT_RATES.iter().position(|&r| r == output)?;
        Some((i, 1 << o))
    }

    /// The mask with exactly the pairs for which `ok(input, output)` holds.
    pub fn from_fn(mut ok: impl FnMut(u32, u32) -> bool) -> Self {
        let mut mask = Self {
            rows: [0; 10],
            unlisted: false,
        };
        for &i in audivane_protocol::ALLOWED_RATES {
            for o in MASK_OUTPUT_RATES {
                if i == o || ok(i, o) {
                    let (row, bit) = Self::bit(i, o).expect("listed rates");
                    mask.rows[row] |= bit;
                }
            }
        }
        mask
    }

    /// The pairs both masks allow (A/B: both sides must build).
    pub fn intersect(self, other: Self) -> Self {
        let mut rows = self.rows;
        for (r, o) in rows.iter_mut().zip(other.rows) {
            *r &= o;
        }
        Self {
            rows,
            unlisted: self.unlisted && other.unlisted,
        }
    }

    /// Whether `input -> output` can be built.
    pub fn allows(self, input: u32, output: u32) -> bool {
        if input == output {
            return true;
        }
        match Self::bit(input, output) {
            Some((row, bit)) => self.rows[row] & bit != 0,
            None => self.unlisted,
        }
    }

    /// Whether the allowed pairs from `input` differ between the masks.
    pub fn differs_from(self, other: Self, input: u32) -> bool {
        match audivane_protocol::ALLOWED_RATES
            .iter()
            .position(|&r| r == input)
        {
            Some(row) => self.rows[row] != other.rows[row],
            None => self.unlisted != other.unlisted,
        }
    }

    /// Whether `ratio` from `input` can be built (×1 always can).
    pub fn allows_ratio(self, input: u32, ratio: RateRatio) -> bool {
        ratio.is_unity()
            || ratio
                .output_rate(input)
                .is_some_and(|out| self.allows(input, out))
    }

    /// The mask without the pair `input -> output` (a listed pair; others
    /// are left as they are).
    pub fn without(mut self, input: u32, output: u32) -> Self {
        if input != output
            && let Some((row, bit)) = Self::bit(input, output)
        {
            self.rows[row] &= !bit;
        }
        self
    }

    /// The mask with the pair `input -> output` (a listed pair; others are
    /// left as they are).
    pub fn with(mut self, input: u32, output: u32) -> Self {
        if let Some((row, bit)) = Self::bit(input, output) {
            self.rows[row] |= bit;
        }
        self
    }
}

/// An open whose engine bypass resampler chain is not there
/// ([`open_with_bypass_fallback`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BypassMiss {
    pub input: u32,
    pub output: u32,
    /// Its design is still running: the pair comes back when it is done
    /// ([`BypassHeld::designed`]). Else it failed, for good on this device.
    pub pending: bool,
    /// The engine's design epoch the negotiation ran in (bumped by an edit
    /// of what the designs depend on: device, output format, quantum,
    /// bypass mode). A hold from an older epoch is retired
    /// ([`BypassHeld::retire`]).
    pub epoch: u64,
}

/// What the engine tells the driver about its bypass designs
/// ([`crate::BypassNotifier`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BypassNote {
    /// The design of `input -> output` the driver waits for is done (`ok`:
    /// it builds).
    Designed { input: u32, output: u32, ok: bool },
    /// The designs now belong to `epoch`: what was asked for (or failed)
    /// before it no longer speaks for the pairs.
    Epoch(u64),
}

/// The engine bypass resampler's pairs the driver holds back on the open
/// device: the ones whose chain failed to build, and the ones whose design
/// is still running. Both fall back to PipeWire (the graph moves to a
/// native rate) until a design is done, or until the engine's design epoch
/// moves on (an edit made the designs obsolete: [`Self::retire`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BypassHeld {
    failed: RatePairMask,
    designing: RatePairMask,
    /// The design epoch of the holds ([`BypassMiss::epoch`]).
    epoch: u64,
}

impl BypassHeld {
    /// Nothing held (a new device).
    pub const NONE: Self = Self {
        failed: RatePairMask::ALL,
        designing: RatePairMask::ALL,
        epoch: 0,
    };

    /// The pairs the bypass mask keeps ([`DesiredDspConfig::keep_bypass`]).
    pub fn keep(self) -> RatePairMask {
        self.failed.intersect(self.designing)
    }

    /// Hold back the pair of `miss`. A miss from a newer epoch retires the
    /// older holds first; one from an older epoch is ignored.
    pub fn record(&mut self, miss: BypassMiss) {
        if miss.epoch < self.epoch {
            return;
        }
        self.retire(miss.epoch);
        if miss.pending {
            self.designing = self.designing.without(miss.input, miss.output);
        } else {
            self.failed = self.failed.without(miss.input, miss.output);
        }
    }

    /// The engine's designs moved on to `epoch`: holds from before it are
    /// void (their designs were dropped, or are for another output). Whether
    /// that released a pair.
    pub fn retire(&mut self, epoch: u64) -> bool {
        if epoch <= self.epoch {
            return false;
        }
        let held = self.keep() != Self::NONE.keep();
        *self = Self {
            epoch,
            ..Self::NONE
        };
        held
    }

    /// The design of `input -> output` is done (`ok`: it builds). Whether
    /// that changed what is held.
    pub fn designed(&mut self, input: u32, output: u32, ok: bool) -> bool {
        let before = *self;
        self.designing = self.designing.with(input, output);
        if !ok {
            self.failed = self.failed.without(input, output);
        }
        *self != before
    }
}

/// One entry point's open and negotiation, for
/// [`open_with_bypass_fallback`] (start-up, reacquire and reopen).
pub trait BypassOpen {
    type Output;
    type Negotiated;
    /// Open for `desired` (closing `previous`, the output of an attempt
    /// that fell back, first).
    fn open(
        &mut self,
        previous: Option<Self::Output>,
        desired: DesiredDspConfig,
    ) -> std::result::Result<Self::Output, String>;
    /// Build the chain for the open: what the engine says, and the pair
    /// when the engine bypass resampler's chain is not there.
    fn negotiate(
        &mut self,
        out: &Self::Output,
        desired: DesiredDspConfig,
    ) -> (Self::Negotiated, Option<BypassMiss>);
    /// Publish the rate policies for `desired` (the pair held back).
    fn republish(&mut self, desired: DesiredDspConfig);
}

/// Open and negotiate; when the engine bypass resampler's chain for the
/// open is not there (it failed to build, or its design is still
/// running), hold the pair back in `held`, republish the policies without
/// it, and open again: the open then plans a move to a native rate
/// (PipeWire resamples, the fallback). Nothing resumes at the missing pair.
/// `base` is the desired config for a mask of kept pairs. Returns the open,
/// its negotiation and the desired config it ran with.
pub fn open_with_bypass_fallback<T: BypassOpen>(
    t: &mut T,
    mut desired: DesiredDspConfig,
    held: &mut BypassHeld,
    base: impl Fn(RatePairMask) -> DesiredDspConfig,
) -> std::result::Result<(T::Output, T::Negotiated, DesiredDspConfig), String> {
    let mut previous = None;
    // A fallback opens at a native rate (x1, no chain to miss); the bound
    // only guards against a backend that keeps landing on held pairs.
    for _ in 0..4 {
        let out = t.open(previous.take(), desired)?;
        let (negotiated, miss) = t.negotiate(&out, desired);
        let Some(miss) = miss else {
            return Ok((out, negotiated, desired));
        };
        held.record(miss);
        desired = desired.next(base(held.keep()), true);
        t.republish(desired);
        previous = Some(out);
    }
    let out = t.open(previous.take(), desired)?;
    let (negotiated, _) = t.negotiate(&out, desired);
    Ok((out, negotiated, desired))
}

/// [`open_with_bypass_fallback`] for the control thread: `desired` (the
/// config it keeps and sends the RT thread) becomes the one the open ran
/// with, before anything resumes. Every entry point (start-up, reacquire,
/// reopen) goes through it: a fallback's config carries a newer
/// generation, and a control loop that kept the older one would publish
/// policies with the held pair in them and send the RT thread updates it
/// refuses ([`rt_takes`]), so the pair would reopen and fall back again.
pub fn open_and_keep<T: BypassOpen>(
    t: &mut T,
    desired: &mut DesiredDspConfig,
    held: &mut BypassHeld,
    base: impl Fn(RatePairMask) -> DesiredDspConfig,
) -> std::result::Result<(T::Output, T::Negotiated), String> {
    let (out, negotiated, ran) = open_with_bypass_fallback(t, *desired, held, base)?;
    *desired = ran;
    Ok((out, negotiated))
}

/// The control thread hears from the engine ([`BypassNote`]): a design it
/// waits for is done, or the design epoch moved on (an edit dropped the
/// designs a held pair waited for: the pair is released, so the policies
/// offer it again and the next open asks for its new design). `desired`
/// follows what is held now. Whether it changed (the RT thread and the
/// rate policies need the update).
pub fn bypass_note(
    desired: &mut DesiredDspConfig,
    held: &mut BypassHeld,
    note: BypassNote,
    base: impl Fn(RatePairMask) -> DesiredDspConfig,
) -> bool {
    let changed = match note {
        BypassNote::Designed { input, output, ok } => held.designed(input, output, ok),
        BypassNote::Epoch(epoch) => held.retire(epoch),
    };
    if !changed {
        return false;
    }
    *desired = desired.next(base(held.keep()), false);
    true
}

/// The RT thread takes a desired config `cfg` (an update, or the one a
/// resume carries) over the one it runs, `rt`, unless `cfg` is older.
pub fn rt_takes(rt: &DesiredDspConfig, cfg: &DesiredDspConfig) -> bool {
    cfg.generation >= rt.generation
}

/// The engine leg of the bypass resampler (`rate.bypass_resampler`): what
/// Direct and DSP off need to convert a graph rate the device does not
/// play natively themselves ([`DesiredDspConfig::bypass_landing`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BypassLeg {
    /// `rate.bypass_resampler: engine`.
    pub engine: bool,
    /// The pairs the resample-only chain can be built for (the bypass
    /// mask): one pipeline of the fixed design.
    pub buildable: RatePairMask,
    /// The configured allowed graph rates, one bit per
    /// [`audivane_protocol::ALLOWED_RATES`] entry ([`Self::allowed_bits`]).
    pub allowed: u16,
}

impl BypassLeg {
    /// No engine leg: Direct and DSP off play x1 only (PipeWire converts).
    pub const OFF: Self = Self {
        engine: false,
        buildable: RatePairMask::NONE,
        allowed: 0,
    };

    /// The [`Self::allowed`] bits of `rates`.
    pub fn allowed_bits(rates: &[u32]) -> u16 {
        audivane_protocol::ALLOWED_RATES
            .iter()
            .enumerate()
            .filter(|(_, r)| rates.contains(r))
            .fold(0, |bits, (i, _)| bits | (1 << i))
    }

    fn allows_rate(self, rate: u32) -> bool {
        audivane_protocol::ALLOWED_RATES
            .iter()
            .position(|&r| r == rate)
            .is_some_and(|i| self.allowed & (1 << i) != 0)
    }
}

impl Default for BypassLeg {
    fn default() -> Self {
        Self::OFF
    }
}

impl Default for RatePairMask {
    fn default() -> Self {
        Self::ALL
    }
}

/// Desired DSP rate-target snapshot propagated from control to RT with a
/// monotonic generation counter so Direct <-> DSP transitions never consult
/// stale startup configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DesiredDspConfig {
    pub dsp_enabled: bool,
    pub rate_target: RateTarget,
    pub max_ratio: u32,
    pub generation: u64,
    /// The rate pairs the chain can be built for ([`RatePairMask`]).
    pub buildable: RatePairMask,
    /// The engine bypass resampler for Direct and DSP off ([`BypassLeg`]).
    pub bypass: BypassLeg,
}

impl Default for DesiredDspConfig {
    fn default() -> Self {
        Self {
            dsp_enabled: true,
            rate_target: RateTarget::Off,
            max_ratio: audivane_protocol::MAX_INTEGER_RATIO,
            generation: 0,
            buildable: RatePairMask::ALL,
            bypass: BypassLeg::OFF,
        }
    }
}

impl DesiredDspConfig {
    /// The config that follows `self` with `new`'s settings: the
    /// generation advances only when what steers negotiation changed (DSP
    /// on or off, the target, the ratio limit, the buildable pairs) or
    /// `force` (an edit that reopens anyway), so a volume or EQ edit never
    /// retries what a blind open found busy.
    pub fn next(self, new: Self, force: bool) -> Self {
        let same = Self {
            generation: self.generation,
            ..new
        } == self;
        Self {
            generation: if same && !force {
                self.generation
            } else {
                self.generation.wrapping_add(1)
            },
            ..new
        }
    }

    /// The config with the engine bypass resampler kept to the pairs of
    /// `keep` (the driver drops a pair whose chain failed to build).
    pub fn keep_bypass(mut self, keep: RatePairMask) -> Self {
        self.bypass.buildable = self.bypass.buildable.intersect(keep);
        self
    }

    /// The config without the engine bypass resampler: what plays natively
    /// (or through the DSP chain). The rate policies start from it and add
    /// the rates the engine converts on top
    /// ([`crate::rate_caps::RatePolicies`]).
    pub fn native_only(mut self) -> Self {
        self.bypass = BypassLeg::OFF;
        self
    }

    /// Whether `sink` plays without the DSP chain: Direct, or DSP off.
    pub fn is_bypass(self, sink: SinkChoice) -> bool {
        sink == SinkChoice::Direct || !self.dsp_enabled
    }

    /// Where the engine bypass resampler takes `graph_rate` for Direct and
    /// DSP off, on a device that plays `rates` (`None`: no probe): the
    /// native rate PipeWire's move would pick (the configured allowed rate
    /// the device plays, the rate itself or the next higher one, else the
    /// closest; any native engine rate when no configured one is native,
    /// as the effective policy widens), when the resample-only chain can be
    /// built for that pair ([`BypassLeg::buildable`]). `None` when the
    /// device plays `graph_rate` natively (x1, bit-perfect), without the
    /// engine leg, without a probe, or for a pair it cannot build (PipeWire
    /// converts then). The rate arbiter, the effective policies and the
    /// open ladder all decide through this one function, so they agree.
    /// Allocation-free.
    pub fn bypass_landing(self, graph_rate: u32, rates: Option<DeviceRates>) -> Option<u32> {
        let leg = self.bypass;
        if !leg.engine {
            return None;
        }
        let rates = rates?;
        if rates.contains(graph_rate) {
            return None;
        }
        let pick = |configured_only: bool| {
            let mut closest: Option<(u32, u32)> = None;
            for &r in audivane_protocol::ALLOWED_RATES {
                if !rates.contains(r) || (configured_only && !leg.allows_rate(r)) {
                    continue;
                }
                if r >= graph_rate {
                    // Ascending: the first one at or above is the next higher.
                    return Some(r);
                }
                let d = r.abs_diff(graph_rate);
                if closest.is_none_or(|(_, best)| d < best) {
                    closest = Some((r, d));
                }
            }
            closest.map(|(r, _)| r)
        };
        let out = pick(true).or_else(|| pick(false))?;
        let ratio = RateRatio::from_rates(graph_rate, out);
        (rational_ok(ratio, graph_rate) && leg.buildable.allows(graph_rate, out)).then_some(out)
    }

    /// Resolve the output/input ratio for `(sink, graph_rate)`, without
    /// knowing the device. Integer ratios are clamped to `max_ratio`; a
    /// rational ratio is kept when it gives a whole output rate within the
    /// engine's limits, else 1. A rational ratio the chain cannot be built
    /// for ([`Self::buildable`]) also resolves to 1. Direct and DSP off
    /// resolve to 1: their engine leg depends on what the device plays
    /// natively, so only [`Self::ladder`] (with the device's rates) offers
    /// it.
    pub fn resolve_ratio_for(self, sink: SinkChoice, graph_rate: u32) -> RateRatio {
        if self.is_bypass(sink) {
            return RateRatio::ONE;
        }
        let graph_rate = graph_rate.max(8_000);
        let r = resolve_rate_ratio(self.rate_target, graph_rate);
        if r.is_integer() {
            RateRatio::integer(r.up.clamp(1, self.max_ratio.max(1)))
        } else if rational_ok(r, graph_rate) && self.buildable.allows_ratio(graph_rate, r) {
            r
        } else {
            RateRatio::ONE
        }
    }

    /// The ratios to try, best first, when opening a device for
    /// `(sink, graph_rate)` ([`Self::ladder`] on the rates the device plays
    /// in one of `formats`, [`DeviceRates::playable`]).
    pub fn ratio_ladder_for(
        self,
        sink: SinkChoice,
        graph_rate: u32,
        caps: Option<&DeviceCapabilities>,
        formats: &[SampleFormat],
    ) -> Vec<RateRatio> {
        self.ladder(sink, graph_rate, DeviceRates::playable(caps, formats))
            .as_slice()
            .to_vec()
    }

    /// The ratio ladder for `(sink, graph_rate)` on a device that plays
    /// `rates` (`None`: no probe), without allocating: the resolved ratio
    /// and its fallbacks ([`audivane_io_alsa::ratio_ladder`]), keeping only
    /// ratios the chain can be built for ([`Self::buildable`]; ×1 always
    /// stays). With a probe, [`RateTarget::MaxRateAny`] follows
    /// [`max_rate_any_ladder`]. Direct and DSP off get x1, and before it,
    /// at a rate the device does not play natively, the engine bypass
    /// resampler's ratio ([`Self::bypass_landing`]).
    pub fn ladder(self, sink: SinkChoice, graph_rate: u32, rates: Option<DeviceRates>) -> Rungs {
        if self.is_bypass(sink) {
            let mut rungs = Rungs::EMPTY;
            let g = graph_rate.max(8_000);
            if let Some(out) = self.bypass_landing(g, rates) {
                rungs.push(RateRatio::from_rates(g, out));
            }
            rungs.push(RateRatio::ONE);
            return rungs;
        }
        let desired = self.resolve_ratio_for(sink, graph_rate);
        let graph_rate = graph_rate.max(8_000);
        if let (RateTarget::MaxRateAny(cap), Some(rates), SinkChoice::Dsp, true) =
            (self.rate_target, rates, sink, self.dsp_enabled)
        {
            return max_rate_any_rungs(
                graph_rate,
                cap,
                rates.iter_desc(),
                self.max_ratio,
                self.buildable,
            );
        }
        // `audivane_io_alsa::ratio_ladder`, kept to buildable ratios.
        let mut rungs = Rungs::EMPTY;
        let mut keep = |q: RateRatio| {
            if self.buildable.allows_ratio(graph_rate, q) {
                rungs.push(q);
            }
        };
        if desired.is_integer() {
            let mut r = desired.up.max(1);
            while r > 1 {
                keep(RateRatio::integer(r));
                r = if r.is_power_of_two() { r / 2 } else { r - 1 };
            }
        } else {
            keep(desired);
        }
        rungs.push(RateRatio::ONE);
        rungs
    }

    /// Where a fresh open for `(sink, graph_rate)` lands: the first entry of
    /// [`Self::ladder`] the device plays (`rates`, as ALSA's open plan
    /// checks it: rate and format) that is not in `refused` (what an open
    /// without a probe found the device refuses). `None` when no entry is
    /// left (the open would fail). Allocation-free.
    pub fn open_landing(
        self,
        sink: SinkChoice,
        graph_rate: u32,
        rates: Option<DeviceRates>,
        refused: &Rungs,
    ) -> Option<RateRatio> {
        let g = graph_rate.max(8_000);
        self.ladder(sink, graph_rate, rates)
            .as_slice()
            .iter()
            .copied()
            .find(|&q| {
                let plays = match (rates, q.output_rate(g)) {
                    (Some(rates), Some(out)) => rates.contains(out),
                    (None, Some(_)) => true,
                    (_, None) => false,
                };
                plays && !refused.contains(q)
            })
    }
}

/// Most entries a ratio ladder has: one per probed rate and ×1
/// ([`max_rate_any_ladder`]), or the integer fallbacks.
pub const MAX_RUNGS: usize = 64;

/// A ratio ladder, or a set of its entries, without allocating.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rungs {
    items: [RateRatio; MAX_RUNGS],
    len: usize,
}

impl Rungs {
    pub const EMPTY: Self = Self {
        items: [RateRatio::ONE; MAX_RUNGS],
        len: 0,
    };

    /// Appends `q` (ignored when full; ladders are shorter).
    pub fn push(&mut self, q: RateRatio) {
        debug_assert!(
            self.len < MAX_RUNGS,
            "ratio ladder over {MAX_RUNGS} entries"
        );
        if self.len < MAX_RUNGS {
            self.items[self.len] = q;
            self.len += 1;
        }
    }

    pub fn as_slice(&self) -> &[RateRatio] {
        &self.items[..self.len]
    }

    pub fn contains(&self, q: RateRatio) -> bool {
        self.as_slice().contains(&q)
    }
}

/// The rates a device plays, as a set over [`MASK_OUTPUT_RATES`] (the rates
/// the driver probes a device for; other rates are left out). `Copy`, so the
/// RT thread can work out what an open would pick without allocating.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceRates(u64);

impl DeviceRates {
    /// The listed rates among `rates`; `None` when there are none.
    pub fn from_rates(rates: impl IntoIterator<Item = u32>) -> Option<Self> {
        let mut bits = 0u64;
        for rate in rates {
            if let Some(i) = MASK_OUTPUT_RATES.iter().position(|&r| r == rate) {
                bits |= 1 << i;
            }
        }
        (bits != 0).then_some(Self(bits))
    }

    /// The rates of a device probe; `None` without a probe or rates.
    pub fn from_caps(caps: Option<&DeviceCapabilities>) -> Option<Self> {
        Self::from_rates(caps?.rates.iter().map(|r| r.rate))
    }

    /// The probed rates the device plays in one of `formats` (empty: the
    /// driver's format preference), the check ALSA's open plan makes
    /// ([`audivane_io_alsa::plan_open`]). `None` without a probe or such a
    /// rate.
    pub fn playable(caps: Option<&DeviceCapabilities>, formats: &[SampleFormat]) -> Option<Self> {
        let caps = caps?;
        let formats: &[SampleFormat] = if formats.is_empty() {
            &audivane_io_alsa::FORMAT_PREFERENCE
        } else {
            formats
        };
        Self::from_rates(
            caps.rates
                .iter()
                .map(|r| r.rate)
                .filter(|&rate| formats.iter().any(|&f| caps.supports(rate, f))),
        )
    }

    pub fn contains(self, rate: u32) -> bool {
        MASK_OUTPUT_RATES
            .iter()
            .position(|&r| r == rate)
            .is_some_and(|i| self.0 & (1 << i) != 0)
    }

    /// The rates, ascending.
    pub fn iter(self) -> impl Iterator<Item = u32> + Clone {
        MASK_OUTPUT_RATES
            .into_iter()
            .enumerate()
            .filter(move |(i, _)| self.0 & (1 << i) != 0)
            .map(|(_, r)| r)
    }

    /// The rates, descending.
    pub fn iter_desc(self) -> impl Iterator<Item = u32> + Clone {
        MASK_OUTPUT_RATES
            .into_iter()
            .enumerate()
            .rev()
            .filter(move |(i, _)| self.0 & (1 << i) != 0)
            .map(|(_, r)| r)
    }
}

/// The ratio ladder of [`RateTarget::MaxRateAny`] from `graph_rate` on a
/// device that plays `device_rates`. Candidates are the device rates at or
/// below `cap` (and the engine's highest output rate) that the chain can be
/// built for. The source's own family comes first: its integer multiples
/// above ×1 (up to `max_ratio`), highest first. The other family (rates that
/// are not a multiple of the source rate, reached by a fractional ratio)
/// comes first only when no integer multiple above ×1 is playable, or when
/// its best rate is at least twice the source family's best. Then ×1, then
/// rates below the source rate (decimation), for a device that cannot play
/// the source rate itself.
pub fn max_rate_any_ladder(
    graph_rate: u32,
    cap: u32,
    device_rates: &[u32],
    max_ratio: u32,
    buildable: RatePairMask,
) -> Vec<RateRatio> {
    let mut rates: Vec<u32> = device_rates.to_vec();
    rates.sort_unstable_by(|a, b| b.cmp(a));
    rates.dedup();
    max_rate_any_rungs(graph_rate, cap, rates.into_iter(), max_ratio, buildable)
        .as_slice()
        .to_vec()
}

/// [`max_rate_any_ladder`] over `rates_desc` (descending, no repeats),
/// without allocating.
fn max_rate_any_rungs(
    graph_rate: u32,
    cap: u32,
    rates_desc: impl Iterator<Item = u32> + Clone,
    max_ratio: u32,
    buildable: RatePairMask,
) -> Rungs {
    let candidates = rates_desc
        .filter(move |&r| r <= cap.min(MAX_OUTPUT_RATE) && r != graph_rate)
        .filter(move |&r| buildable.allows(graph_rate, r));
    let ratio = move |r: u32| RateRatio::from_rates(graph_rate, r);
    let same = candidates
        .clone()
        .map(ratio)
        .filter(move |q| q.is_integer() && q.up > 1 && q.up <= max_ratio.max(1));
    let other = candidates
        .clone()
        .filter(move |&r| r > graph_rate)
        .map(ratio)
        .filter(move |q| !q.is_integer() && rational_ok(*q, graph_rate));
    let below = candidates
        .filter(move |&r| r < graph_rate)
        .map(ratio)
        .filter(move |q| rational_ok(*q, graph_rate));
    let rate_of = |q: RateRatio| u64::from(q.output_rate(graph_rate).unwrap_or(0));
    let cross = match (same.clone().next(), other.clone().next()) {
        (None, Some(_)) => true,
        (Some(s), Some(o)) => rate_of(o) >= 2 * rate_of(s),
        _ => false,
    };
    let mut rungs = Rungs::EMPTY;
    if cross {
        other.clone().for_each(|q| rungs.push(q));
        same.for_each(|q| rungs.push(q));
    } else {
        same.for_each(|q| rungs.push(q));
        other.for_each(|q| rungs.push(q));
    }
    rungs.push(RateRatio::ONE);
    below.for_each(|q| rungs.push(q));
    rungs
}

/// A rational ratio the engine can open a device for from `graph_rate`.
fn rational_ok(r: RateRatio, graph_rate: u32) -> bool {
    r.up <= MAX_RATIONAL_TERM
        && r.down <= MAX_RATIONAL_TERM
        && r.output_rate(graph_rate)
            .is_some_and(|out| (8_000..=MAX_OUTPUT_RATE).contains(&out))
}

/// What the last ALSA open found: the device rates it planned with (as
/// [`DeviceRates::playable`] gives them; `None` without a probe) and, for
/// an open without a probe, the ladder entries the device refused before
/// the one it took: `refused` for a capability error (it cannot play
/// that), `transient` for a failure a later attempt may get past (busy,
/// gone during a hot-plug, a timeout). A fresh open would skip the refused
/// entries, so [`DesiredDspConfig::open_landing`] predicts it exactly.
/// Transient failures only hold for the negotiation the open was made for
/// (`generation`, which only an edit that changes negotiation advances,
/// [`DesiredDspConfig::next`]): such an edit tries them again, while the
/// RT thread's own checks and other edits (volume) do not reopen into them
/// over and over. Both only hold within the playback session the open was
/// made in (`session`, [`PlaybackSessions`]): a fresh stream start on
/// either sink tries every entry again, for the RT thread and the daemon
/// alike ([`Self::in_session`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NegotiatedKey {
    pub graph_rate: u32,
    pub rates: Option<DeviceRates>,
    pub refused: Rungs,
    pub transient: Rungs,
    pub generation: u64,
    pub session: u64,
}

impl NegotiatedKey {
    /// The key of an open made in playback session `session`.
    pub fn with_session(self, session: u64) -> Self {
        Self { session, ..self }
    }

    /// The key as it holds in playback session `session`: an open from an
    /// earlier session keeps what it probed but no refusals (every entry
    /// gets another try).
    pub fn in_session(self, session: u64) -> Self {
        if self.session == session {
            self
        } else {
            Self {
                refused: Rungs::EMPTY,
                transient: Rungs::EMPTY,
                ..self
            }
        }
    }

    /// The entries a fresh open under `cfg` at `graph_rate` would skip.
    pub fn skipped(&self, cfg: DesiredDspConfig, graph_rate: u32) -> Rungs {
        if self.graph_rate != graph_rate {
            return Rungs::EMPTY;
        }
        let mut skipped = self.refused;
        if cfg.generation == self.generation {
            self.transient
                .as_slice()
                .iter()
                .for_each(|&q| skipped.push(q));
        }
        skipped
    }
}

/// The key of an open of `sink` at `graph_rate` under `cfg` on a device
/// that plays `rates` (`None`: no probe), from what the open took and saw
/// fail ([`audivane_io_alsa::RatioOpen`]).
pub fn negotiated_key(
    cfg: DesiredDspConfig,
    _sink: SinkChoice,
    graph_rate: u32,
    rates: Option<DeviceRates>,
    open: &audivane_io_alsa::RatioOpen,
) -> NegotiatedKey {
    let rungs = |list: &[RateRatio]| {
        let mut r = Rungs::EMPTY;
        list.iter().for_each(|&q| r.push(q));
        r
    };
    NegotiatedKey {
        graph_rate,
        rates,
        refused: rungs(&open.refused),
        transient: rungs(&open.transient),
        generation: cfg.generation,
        session: 0,
    }
}

/// Playback sessions: a new one starts whenever a sink starts streaming
/// (per sink, so a stream starting on one sink while the other streams
/// counts too). [`NegotiatedKey::in_session`] drops refusals from an
/// earlier one. The RT thread observes the streams and publishes the
/// session; the daemon reads it, so both predict the same open.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PlaybackSessions {
    dsp: bool,
    direct: bool,
    session: u64,
}

impl PlaybackSessions {
    /// The session after seeing the sinks' streaming flags.
    pub fn observe(&mut self, dsp: bool, direct: bool) -> u64 {
        if (dsp && !self.dsp) || (direct && !self.direct) {
            self.session = self.session.wrapping_add(1);
        }
        self.dsp = dsp;
        self.direct = direct;
        self.session
    }

    pub fn current(&self) -> u64 {
        self.session
    }
}

/// Pure decision function called by the RT thread (and the daemon): whether a
/// graph rate change, sink transition or DSP config update reopens ALSA, and
/// `Some(ratio)` for where a fresh open would land. The prediction follows
/// the real open plan ([`DesiredDspConfig::open_landing`] with the last
/// open's [`NegotiatedKey`]), so a fresh open and any sequence of edits, DSP
/// toggles and sink switches end at the same ratio, and an edit that would
/// land where the device already runs (a device that fell back, an entry only
/// in another format) never reopens. Where nothing lands (Direct or DSP off
/// at a rate the device does not play, without the engine leg) a device away
/// from ×1 is handed to control, which moves the graph to a native rate or
/// reports the error.
///
/// Allocation-free. The rules are in `docs/architecture.md` ("Sample rate
/// and clock").
#[inline]
pub fn decide_rt_reopen(
    rt_cfg: DesiredDspConfig,
    last_negotiated_key: Option<NegotiatedKey>,
    active_sink: SinkChoice,
    target_sink: SinkChoice,
    active_graph_rate: u32,
    cur_rate: u32,
    active_ratio: RateRatio,
) -> Option<RateRatio> {
    let rates = last_negotiated_key.and_then(|k| k.rates);
    if cur_rate != active_graph_rate {
        let landing = rt_cfg.open_landing(target_sink, cur_rate, rates, &Rungs::EMPTY);
        return Some(landing.unwrap_or(RateRatio::ONE));
    }
    let skipped = last_negotiated_key.map_or(Rungs::EMPTY, |k| k.skipped(rt_cfg, cur_rate));
    match rt_cfg.open_landing(target_sink, cur_rate, rates, &skipped) {
        Some(q) => (q != active_ratio).then_some(q),
        // Landing unavailable (not unchanged): bypass without the engine
        // leg cannot play away from x1, so hand the device to control.
        None => (!active_ratio.is_unity() || target_sink != active_sink).then_some(RateRatio::ONE),
    }
}

/// Pure reopen coordination gate preventing duplicate `SuspendForReopen` /
/// `NeedReopen` cycles while still permitting recovery from `EngineState::Error`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReopenGate {
    pub awaiting_reopen_ack: bool,
    pub rt_reopen_in_flight: bool,
}

impl ReopenGate {
    pub const fn new() -> Self {
        Self {
            awaiting_reopen_ack: false,
            rt_reopen_in_flight: false,
        }
    }

    /// Control thread: returns `true` when `SuspendForReopen` should be sent.
    pub fn ctrl_request_reopen(
        &mut self,
        force_reopen: bool,
        released_or_teardown: bool,
        state: EngineState,
    ) -> bool {
        if force_reopen
            && !released_or_teardown
            && state != EngineState::Idle
            && !self.awaiting_reopen_ack
        {
            self.awaiting_reopen_ack = true;
            true
        } else {
            false
        }
    }

    /// Control thread: consumes `awaiting_reopen_ack` when handling `NeedReopen`.
    pub fn ctrl_take_awaiting_cfg(&mut self) -> bool {
        std::mem::take(&mut self.awaiting_reopen_ack)
    }

    /// Control thread: clears pending reopen state when entering release teardown.
    pub fn ctrl_clear_for_release(&mut self) {
        self.awaiting_reopen_ack = false;
    }

    /// RT thread: returns `true` if a `NeedReopen` event should be emitted.
    pub fn rt_try_begin_reopen(&mut self) -> bool {
        if self.rt_reopen_in_flight {
            false
        } else {
            self.rt_reopen_in_flight = true;
            true
        }
    }

    /// RT thread: clears `rt_reopen_in_flight` on `Resume`, `ReopenAborted`, or
    /// `SuspendForRelease`.
    pub fn rt_finish_reopen(&mut self) {
        self.rt_reopen_in_flight = false;
    }
}

/// Abstract backend for reservation + PCM open/close so device handoff and
/// error-state invariants can be tested exhaustively without ALSA/D-Bus.
pub trait DeviceBackend {
    type Reservation;
    type Output;

    fn card_index(&mut self, device: &str) -> std::result::Result<u32, String>;
    fn reserve(
        &mut self,
        device: &str,
        card: u32,
        priority: i32,
    ) -> std::result::Result<Self::Reservation, String>;
    fn is_release_requested(&self, res: &Self::Reservation) -> bool;
    fn release_reservation(&mut self, res: Self::Reservation);
    fn open_and_prepare(
        &mut self,
        device: &str,
        graph_rate: u32,
        ladder: &[RateRatio],
        quantum: u32,
    ) -> std::result::Result<OpenedDevice<Self::Output>, String>;
    fn close_output(&mut self, out: Self::Output);

    /// The ratios `open_and_prepare` should try for `(sink, graph_rate)`
    /// ([`DesiredDspConfig::ratio_ladder_for`]); a backend with a device
    /// probe passes it in.
    fn ratio_ladder(
        &self,
        desired_dsp: DesiredDspConfig,
        target_sink: SinkChoice,
        graph_rate: u32,
    ) -> Vec<RateRatio> {
        desired_dsp.ratio_ladder_for(target_sink, graph_rate, None, &[])
    }

    /// The playback session ([`PlaybackSessions`]) as the RT thread
    /// publishes it now.
    fn playback_session(&self) -> u64 {
        0
    }

    /// Called once the reservation is held, right before `open_and_prepare`.
    /// The backend may move the graph rate (and its quantum) to one the
    /// device can play; [`DeviceLifecycle::handoff_and_open`] then opens at
    /// the returned rate and reports it in [`OpenedDevice::graph_rate`].
    fn resolve_graph_rate(
        &mut self,
        _device: &str,
        graph_rate: u32,
        quantum: u32,
        _target_sink: SinkChoice,
        _desired_dsp: DesiredDspConfig,
    ) -> std::result::Result<(u32, u32), String> {
        Ok((graph_rate, quantum))
    }
}

/// Result returned by [`DeviceBackend::open_and_prepare`].
#[derive(Debug)]
pub struct OpenedDevice<O> {
    pub output: O,
    pub actual_ratio: RateRatio,
    pub output_rate: u32,
    /// Graph rate the device was opened for (filled in by
    /// [`DeviceLifecycle::handoff_and_open`]; may differ from the requested
    /// one after [`DeviceBackend::resolve_graph_rate`]).
    pub graph_rate: u32,
    /// Quantum used for that graph rate.
    pub quantum: u32,
    /// What the open took and saw fail on the way (`plan.ratio` is
    /// `actual_ratio`).
    pub plan: audivane_io_alsa::RatioOpen,
    /// The playback session the open ran in (filled in by
    /// [`DeviceLifecycle::handoff_and_open`]): what it saw refused belongs
    /// to that session ([`NegotiatedKey::with_session`]).
    pub session: u64,
}

/// Lifecycle manager enforcing full close-old -> release-old -> reserve-new ->
/// open-new handoff semantics and explicit `Error` state on any failure.
pub struct DeviceLifecycle<B: DeviceBackend> {
    pub state: EngineState,
    pub last_error: Option<String>,
    pub reservation: Option<(u32, B::Reservation)>,
    pub active_device: Option<String>,
    pub active_graph_rate: u32,
    pub active_ratio: RateRatio,
    pub active_sink: SinkChoice,
    pub output_held: bool,
}

impl<B: DeviceBackend> Default for DeviceLifecycle<B> {
    fn default() -> Self {
        Self::new()
    }
}

impl<B: DeviceBackend> DeviceLifecycle<B> {
    pub fn new() -> Self {
        Self {
            state: EngineState::Idle,
            last_error: None,
            reservation: None,
            active_device: None,
            active_graph_rate: 48_000,
            active_ratio: RateRatio::ONE,
            active_sink: SinkChoice::Dsp,
            output_held: false,
        }
    }

    pub fn release_reservation(&mut self, backend: &mut B) {
        if let Some((_, res)) = self.reservation.take() {
            backend.release_reservation(res);
        }
    }

    pub fn reservation_release_requested(&self, backend: &B) -> bool {
        self.reservation
            .as_ref()
            .is_some_and(|(_, r)| backend.is_release_requested(r))
    }

    /// Release any held reservation cleanly when transitioning to `Idle`.
    pub fn finish_release_to_idle(&mut self, backend: &mut B, old_out: Option<B::Output>) {
        if let Some(out) = old_out {
            backend.close_output(out);
        }
        self.output_held = false;
        self.release_reservation(backend);
        self.active_device = None;
        self.state = EngineState::Idle;
    }

    /// Execute a full device handoff / reopen:
    /// 1. Close `old_out` (if provided).
    /// 2. Ensure reservation matches `target_device`'s card (reusing if same
    ///    card and still valid, otherwise releasing old card's reservation BEFORE
    ///    acquiring the new card's reservation).
    /// 3. Resolve desired ratio at `graph_rate` and open `target_device`.
    /// 4. On ANY failure, leave no active output and release any held
    ///    reservation, setting `state = EngineState::Error` and `last_error` so
    ///    device A is never silently kept.
    ///
    /// `on_release` is the engine's release hook: it runs with
    /// [`ReleaseKind::HandOver`] and the card's device while a card is
    /// still ours and about to go (device A before a switch to another
    /// device, before A's reservation is released, and the target before
    /// the reservation is dropped after a failed rate resolve or open).
    /// Card-index and reserve failures leave it to the caller
    /// ([`ReleaseKind::Lost`]).
    #[allow(clippy::too_many_arguments)]
    pub fn handoff_and_open(
        &mut self,
        backend: &mut B,
        old_out: Option<B::Output>,
        target_device: &str,
        reserve_device: bool,
        reserve_priority: i32,
        graph_rate: u32,
        quantum: u32,
        target_sink: SinkChoice,
        desired_dsp: DesiredDspConfig,
        on_release: &dyn Fn(ReleaseKind, &str),
    ) -> std::result::Result<OpenedDevice<B::Output>, String> {
        // Before anything opens: a stream starting meanwhile starts a new
        // session, which retries what this open sees refused.
        let session = backend.playback_session();
        let switching_device = self
            .active_device
            .as_deref()
            .is_some_and(|d| d != target_device);
        if switching_device
            && (old_out.is_some() || self.reservation.is_some())
            && let Some(old) = self.active_device.as_deref()
        {
            on_release(ReleaseKind::HandOver, old);
        }
        if let Some(out) = old_out {
            backend.close_output(out);
        }
        self.output_held = false;

        if reserve_device {
            let target_card = match backend.card_index(target_device) {
                Ok(c) => c,
                Err(e) => {
                    self.release_reservation(backend);
                    self.active_device = None;
                    let msg = format!("Invalid device '{target_device}': {e}");
                    self.last_error = Some(msg.clone());
                    self.state = EngineState::Error;
                    return Err(msg);
                }
            };

            let keep_existing = self.reservation.as_ref().is_some_and(|(held_card, res)| {
                *held_card == target_card && !backend.is_release_requested(res)
            });

            if !keep_existing {
                // Release old card reservation BEFORE reserving new card.
                if self.reservation.is_some() {
                    on_release(
                        ReleaseKind::HandOver,
                        self.active_device.as_deref().unwrap_or(target_device),
                    );
                }
                self.release_reservation(backend);
                match backend.reserve(target_device, target_card, reserve_priority) {
                    Ok(res) => {
                        self.reservation = Some((target_card, res));
                    }
                    Err(e) => {
                        self.active_device = None;
                        let msg = format!("ReserveDevice1 failed for {target_device}: {e}");
                        self.last_error = Some(msg.clone());
                        self.state = EngineState::Error;
                        return Err(msg);
                    }
                }
            }
        } else {
            self.release_reservation(backend);
        }

        let (graph_rate, quantum) = match backend.resolve_graph_rate(
            target_device,
            graph_rate,
            quantum,
            target_sink,
            desired_dsp,
        ) {
            Ok(v) => v,
            Err(e) => {
                on_release(ReleaseKind::HandOver, target_device);
                self.release_reservation(backend);
                self.active_device = None;
                let msg = format!("ALSA open/prepare failed for {target_device}: {e}");
                self.last_error = Some(msg.clone());
                self.state = EngineState::Error;
                return Err(msg);
            }
        };
        let ladder = backend.ratio_ladder(desired_dsp, target_sink, graph_rate);
        match backend.open_and_prepare(target_device, graph_rate, &ladder, quantum) {
            Ok(mut opened) => {
                opened.graph_rate = graph_rate;
                opened.quantum = quantum;
                opened.session = session;
                self.active_device = Some(target_device.to_string());
                self.active_graph_rate = graph_rate;
                self.active_ratio = opened.actual_ratio;
                self.active_sink = target_sink;
                self.output_held = true;
                self.last_error = None;
                self.state = EngineState::Ready;
                Ok(opened)
            }
            Err(e) => {
                // Do not sit on a reservation or claim old device A is still open.
                on_release(ReleaseKind::HandOver, target_device);
                self.release_reservation(backend);
                self.active_device = None;
                let msg = format!("ALSA open/prepare failed for {target_device}: {e}");
                self.last_error = Some(msg.clone());
                self.state = EngineState::Error;
                Err(msg)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// One entry point (start-up, reacquire, reopen) on a 48 kHz-only DAC
    /// for [`open_with_bypass_fallback`]: an open plans like the driver's
    /// (a rate with no landing moves to 48 kHz), and the engine misses the
    /// chain for `missing`.
    struct FakeEntry {
        rates: DeviceRates,
        graph_rate: u32,
        /// The output the RT thread handed back (reopen only).
        old: Option<String>,
        missing: Option<BypassMiss>,
        journal: Vec<String>,
        republished: Vec<DesiredDspConfig>,
    }

    impl BypassOpen for FakeEntry {
        type Output = (String, u32, RateRatio);
        type Negotiated = u32;

        fn open(
            &mut self,
            previous: Option<Self::Output>,
            desired: DesiredDspConfig,
        ) -> std::result::Result<Self::Output, String> {
            if let Some(out) = previous.map(|p| p.0).or_else(|| self.old.take()) {
                self.journal.push(format!("close {out}"));
            }
            let g = self.graph_rate;
            let (rate, ratio) = if self.rates.contains(g) {
                (g, RateRatio::ONE)
            } else if let Some(out) = desired.bypass_landing(g, Some(self.rates)) {
                (g, RateRatio::from_rates(g, out))
            } else {
                (48_000, RateRatio::ONE)
            };
            let name = format!("open {rate} {ratio}");
            self.journal.push(name.clone());
            Ok((name, rate, ratio))
        }

        fn negotiate(
            &mut self,
            out: &Self::Output,
            _: DesiredDspConfig,
        ) -> (u32, Option<BypassMiss>) {
            self.journal.push(format!("negotiate {}", out.1));
            let output = out.2.output_rate(out.1).unwrap_or(out.1);
            let miss = self
                .missing
                .filter(|m| m.input == out.1 && m.output == output && !out.2.is_unity());
            (out.1, miss)
        }

        fn republish(&mut self, desired: DesiredDspConfig) {
            self.journal.push("republish".into());
            self.republished.push(desired);
        }
    }

    #[test]
    fn a_missing_bypass_chain_falls_back_before_anything_resumes() {
        let caps = audivane_protocol::ipc::DeviceCapabilities {
            device: "hw:CARD=USB,DEV=0".into(),
            channels: 2,
            channels_min: 2,
            channels_max: 2,
            rates: vec![audivane_protocol::ipc::RateCapability {
                rate: 48_000,
                formats: vec![audivane_protocol::ipc::FormatCapability {
                    format: audivane_protocol::SampleFormat::S32,
                    sbits: Some(32),
                }],
            }],
            rate_min: 48_000,
            rate_max: 48_000,
            formats: vec![audivane_protocol::SampleFormat::S32],
        };
        let rates = DeviceRates::playable(Some(&caps), &[]).unwrap();
        let base = |keep: RatePairMask| {
            DesiredDspConfig {
                dsp_enabled: false,
                bypass: BypassLeg {
                    engine: true,
                    buildable: RatePairMask::ALL,
                    allowed: BypassLeg::allowed_bits(&[48_000]),
                },
                ..DesiredDspConfig::default()
            }
            .keep_bypass(keep)
        };
        // Start-up and reacquire open from nothing; reopen first closes the
        // output the RT thread handed back.
        for (entry, old) in [
            ("start-up", None),
            ("reacquire", None),
            ("reopen", Some("rt")),
        ] {
            for pending in [false, true] {
                let what = format!("{entry}, pending {pending}");
                let miss = BypassMiss {
                    input: 44_100,
                    output: 48_000,
                    pending,
                    epoch: 0,
                };
                let mut fake = FakeEntry {
                    rates,
                    graph_rate: 44_100,
                    old: old.map(str::to_string),
                    missing: Some(miss),
                    journal: Vec::new(),
                    republished: Vec::new(),
                };
                let mut held = BypassHeld::NONE;
                let desired = base(held.keep());
                assert_eq!(desired.bypass_landing(44_100, Some(rates)), Some(48_000));
                let (out, negotiated, ran) =
                    open_with_bypass_fallback(&mut fake, desired, &mut held, base).unwrap();
                let mut expected: Vec<String> = old.iter().map(|o| format!("close {o}")).collect();
                expected.extend(
                    [
                        "open 44100 160/147",
                        "negotiate 44100",
                        // Recorded and republished, then opened again
                        // (closing the missed open) at the native rate.
                        "republish",
                        "close open 44100 160/147",
                        "open 48000 x1",
                        "negotiate 48000",
                    ]
                    .map(String::from),
                );
                assert_eq!(fake.journal, expected, "{what}");
                assert_eq!((out.1, out.2, negotiated), (48_000, RateRatio::ONE, 48_000));
                // The pair is held back in what resumes and in the policies.
                assert!(!held.keep().allows(44_100, 48_000), "{what}");
                assert_eq!(ran.bypass_landing(44_100, Some(rates)), None, "{what}");
                assert_eq!(fake.republished, vec![ran], "{what}");
                assert!(ran.generation > desired.generation);
                // A design that finishes brings a pending pair back; a
                // failed pair stays out.
                assert!(held.designed(44_100, 48_000, true) == pending, "{what}");
                assert_eq!(held.keep().allows(44_100, 48_000), pending, "{what}");
                assert!(!held.designed(44_100, 48_000, true));
            }
        }
        // Nothing missing: one open, no republish.
        let mut fake = FakeEntry {
            rates,
            graph_rate: 44_100,
            old: None,
            missing: None,
            journal: Vec::new(),
            republished: Vec::new(),
        };
        let mut held = BypassHeld::NONE;
        let (out, _, _) =
            open_with_bypass_fallback(&mut fake, base(held.keep()), &mut held, base).unwrap();
        assert_eq!(out.2, RateRatio::from_rates(44_100, 48_000));
        assert!(fake.republished.is_empty());
        assert_eq!(held, BypassHeld::NONE);
    }

    #[test]
    fn a_fallback_open_is_kept_so_the_pair_comes_back_once_designed() {
        // A 48 kHz-only DAC, 44.1 kHz playing with DSP off, the pair's
        // design cold. Through what the driver's entry points and RT thread
        // call: the control thread keeps the fallback's config, the RT
        // thread takes it and does not ask to reopen, and the design's end
        // brings the pair back on both sides.
        let caps = audivane_protocol::ipc::DeviceCapabilities {
            device: "hw:CARD=USB,DEV=0".into(),
            channels: 2,
            channels_min: 2,
            channels_max: 2,
            rates: vec![audivane_protocol::ipc::RateCapability {
                rate: 48_000,
                formats: vec![audivane_protocol::ipc::FormatCapability {
                    format: audivane_protocol::SampleFormat::S32,
                    sbits: Some(32),
                }],
            }],
            rate_min: 48_000,
            rate_max: 48_000,
            formats: vec![audivane_protocol::SampleFormat::S32],
        };
        let rates = DeviceRates::playable(Some(&caps), &[]).unwrap();
        let base = |keep: RatePairMask| {
            DesiredDspConfig {
                dsp_enabled: false,
                bypass: BypassLeg {
                    engine: true,
                    buildable: RatePairMask::ALL,
                    allowed: BypassLeg::allowed_bits(&[44_100, 48_000]),
                },
                ..DesiredDspConfig::default()
            }
            .keep_bypass(keep)
        };
        let r = RateRatio::from_rates(44_100, 48_000);
        let designed_ok = BypassNote::Designed {
            input: 44_100,
            output: 48_000,
            ok: true,
        };
        for (entry, old) in [
            ("start-up", None),
            ("reacquire", None),
            ("reopen", Some("rt")),
        ] {
            let mut held = BypassHeld::NONE;
            // What the control thread holds and last sent the RT thread.
            let mut desired = base(held.keep()).next(base(held.keep()), true);
            let mut rt = desired;
            let mut fake = FakeEntry {
                rates,
                graph_rate: 44_100,
                old: old.map(str::to_string),
                missing: Some(BypassMiss {
                    input: 44_100,
                    output: 48_000,
                    pending: true,
                    epoch: 0,
                }),
                journal: Vec::new(),
                republished: Vec::new(),
            };
            let (out, _) = open_and_keep(&mut fake, &mut desired, &mut held, base).unwrap();
            assert_eq!((out.1, out.2), (48_000, RateRatio::ONE), "{entry}");
            // Kept: the policies the control thread publishes leave the
            // pair out, and the resume's config is the one it holds.
            assert_eq!(desired.bypass_landing(44_100, Some(rates)), None, "{entry}");
            assert_eq!(fake.republished, vec![desired], "{entry}");
            assert!(rt_takes(&rt, &desired), "{entry}");
            rt = desired;
            let open_key = NegotiatedKey {
                rates: Some(rates),
                ..key(48_000, &[])
            };
            assert_eq!(
                decide_rt_reopen(
                    rt,
                    Some(open_key),
                    SinkChoice::Dsp,
                    SinkChoice::Dsp,
                    48_000,
                    48_000,
                    RateRatio::ONE
                ),
                None,
                "{entry}: the fallback asks to reopen"
            );
            // The design is done: an update the RT thread takes, with the
            // pair back.
            assert!(bypass_note(&mut desired, &mut held, designed_ok, base));
            assert!(rt_takes(&rt, &desired), "{entry}: the RT thread refuses");
            rt = desired;
            assert_eq!(
                rt.bypass_landing(44_100, Some(rates)),
                Some(48_000),
                "{entry}"
            );
            // Still at 48 kHz: nothing to reopen until a client moves the
            // graph; then the pair opens at 160/147.
            assert_eq!(
                decide_rt_reopen(
                    rt,
                    Some(open_key),
                    SinkChoice::Dsp,
                    SinkChoice::Dsp,
                    48_000,
                    48_000,
                    RateRatio::ONE
                ),
                None,
                "{entry}"
            );
            assert_eq!(
                decide_rt_reopen(
                    rt,
                    Some(open_key),
                    SinkChoice::Dsp,
                    SinkChoice::Dsp,
                    48_000,
                    44_100,
                    RateRatio::ONE
                ),
                Some(r),
                "{entry}"
            );
            // A repeat of the notification changes nothing.
            assert!(!bypass_note(&mut desired, &mut held, designed_ok, base));
        }
    }

    #[test]
    fn holds_from_a_superseded_design_epoch_are_released() {
        // The engine's designs move on (an output, quantum or bypass mode
        // edit dropped the one a held pair waited for): the pair is offered
        // again, so the next open asks for its new design.
        let base = |keep: RatePairMask| {
            DesiredDspConfig {
                dsp_enabled: false,
                bypass: BypassLeg {
                    engine: true,
                    buildable: RatePairMask::ALL,
                    allowed: BypassLeg::allowed_bits(&[44_100, 48_000]),
                },
                ..DesiredDspConfig::default()
            }
            .keep_bypass(keep)
        };
        let miss = |pending, epoch| BypassMiss {
            input: 44_100,
            output: 48_000,
            pending,
            epoch,
        };
        for pending in [true, false] {
            let mut held = BypassHeld::NONE;
            let mut desired = base(held.keep());
            held.record(miss(pending, 3));
            desired = desired.next(base(held.keep()), true);
            assert!(!desired.bypass.buildable.allows(44_100, 48_000));
            // The epoch it was held in, or an older one: still held.
            for e in [2, 3] {
                assert!(!bypass_note(
                    &mut desired,
                    &mut held,
                    BypassNote::Epoch(e),
                    base
                ));
            }
            let before = desired.generation;
            assert!(bypass_note(
                &mut desired,
                &mut held,
                BypassNote::Epoch(4),
                base
            ));
            assert!(desired.bypass.buildable.allows(44_100, 48_000));
            assert!(desired.generation > before);
            // A late miss from the old epoch holds nothing; a new one does.
            held.record(miss(pending, 3));
            assert_eq!(held.keep(), BypassHeld::NONE.keep());
            held.record(miss(pending, 4));
            assert!(!held.keep().allows(44_100, 48_000));
            // A miss from a newer epoch drops the older holds first.
            held.record(BypassMiss {
                input: 88_200,
                ..miss(pending, 5)
            });
            assert!(held.keep().allows(44_100, 48_000));
            assert!(!held.keep().allows(88_200, 48_000));
        }
        // Nothing held: a new epoch changes nothing to send.
        let mut held = BypassHeld::NONE;
        let mut desired = base(held.keep());
        assert!(!bypass_note(
            &mut desired,
            &mut held,
            BypassNote::Epoch(1),
            base
        ));
    }

    /// The key of an open without a device probe that saw `refused`.
    fn key(graph_rate: u32, refused: &[RateRatio]) -> NegotiatedKey {
        let mut r = Rungs::EMPTY;
        refused.iter().for_each(|&q| r.push(q));
        NegotiatedKey {
            graph_rate,
            rates: None,
            refused: r,
            transient: Rungs::EMPTY,
            generation: 0,
            session: 0,
        }
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Op {
        Reserve(u32, String),
        Release(u32),
        Open(String, u32, u32),
        Close(String),
    }

    struct MockBackend {
        ops: Vec<Op>,
        fail_reserve_cards: HashSet<u32>,
        fail_open_devices: HashSet<String>,
        max_rate_for_device: Option<u32>,
        /// Device plays only this graph rate (like a 48k-only USB DAC).
        only_rate: Option<u32>,
        /// Closes, releases and (from the test's hook) hand-overs, in order.
        journal: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
        fail_resolve: bool,
        session: u64,
        session_steps_per_open: u64,
    }

    impl MockBackend {
        fn new() -> Self {
            Self {
                ops: Vec::new(),
                fail_reserve_cards: HashSet::new(),
                fail_open_devices: HashSet::new(),
                max_rate_for_device: None,
                only_rate: None,
                journal: Default::default(),
                fail_resolve: false,
                session: 0,
                session_steps_per_open: 0,
            }
        }
    }

    impl DeviceBackend for MockBackend {
        type Reservation = u32;
        type Output = String;

        fn playback_session(&self) -> u64 {
            self.session
        }

        fn card_index(&mut self, device: &str) -> std::result::Result<u32, String> {
            let rest = device
                .strip_prefix("hw:")
                .ok_or_else(|| format!("not hw: {device}"))?;
            let card = rest.split(',').next().unwrap_or("");
            if card == "Loopback" {
                return Ok(10);
            }
            if card == "CardA" {
                return Ok(1);
            }
            if card == "CardB" {
                return Ok(2);
            }
            card.parse::<u32>()
                .map_err(|_| format!("bad card name: {device}"))
        }

        fn reserve(
            &mut self,
            device: &str,
            card: u32,
            _priority: i32,
        ) -> std::result::Result<Self::Reservation, String> {
            if self.fail_reserve_cards.contains(&card) {
                return Err(format!("busy card {card}"));
            }
            self.ops.push(Op::Reserve(card, device.to_string()));
            Ok(card)
        }

        fn is_release_requested(&self, _res: &Self::Reservation) -> bool {
            false
        }

        fn release_reservation(&mut self, res: Self::Reservation) {
            self.journal.borrow_mut().push(format!("release {res}"));
            self.ops.push(Op::Release(res));
        }

        fn open_and_prepare(
            &mut self,
            device: &str,
            graph_rate: u32,
            ladder: &[RateRatio],
            _quantum: u32,
        ) -> std::result::Result<OpenedDevice<Self::Output>, String> {
            if self.fail_open_devices.contains(device) {
                return Err(format!("cannot open {device}"));
            }
            let mut r = ladder.first().map_or(1, |d| d.up).max(1);
            if let Some(max_r) = self.max_rate_for_device {
                while r > 1 && graph_rate * r > max_r {
                    r /= 2;
                }
            }
            let out_rate = graph_rate * r;
            self.ops.push(Op::Open(device.to_string(), graph_rate, r));
            // A stream may start while the device opens.
            self.session += self.session_steps_per_open;
            Ok(OpenedDevice {
                output: device.to_string(),
                actual_ratio: RateRatio::integer(r),
                output_rate: out_rate,
                graph_rate,
                quantum: _quantum,
                plan: audivane_io_alsa::RatioOpen::at(RateRatio::integer(r)),
                session: 0,
            })
        }

        fn close_output(&mut self, out: Self::Output) {
            self.journal.borrow_mut().push(format!("close {out}"));
            self.ops.push(Op::Close(out));
        }

        fn resolve_graph_rate(
            &mut self,
            _device: &str,
            graph_rate: u32,
            quantum: u32,
            _target_sink: SinkChoice,
            _desired_dsp: DesiredDspConfig,
        ) -> std::result::Result<(u32, u32), String> {
            if self.fail_resolve {
                return Err("no usable rate".into());
            }
            Ok(match self.only_rate {
                Some(r) if r != graph_rate => (r, quantum * r / graph_rate),
                _ => (graph_rate, quantum),
            })
        }
    }

    /// The engine's hand-over hook runs for card A before A's output is
    /// closed and its reservation released on a device switch, and before
    /// the reservation is dropped when resolving or opening fails (the card
    /// was still reserved then).
    #[test]
    fn device_switch_and_open_failures_hand_over_first() {
        let dsp = DesiredDspConfig::default();
        let open = |lc: &mut DeviceLifecycle<MockBackend>,
                    backend: &mut MockBackend,
                    old: Option<String>,
                    device: &str| {
            let journal = std::rc::Rc::clone(&backend.journal);
            let hook = move |kind: ReleaseKind, device: &str| {
                assert_eq!(kind, ReleaseKind::HandOver);
                journal.borrow_mut().push(format!("hook {device}"));
            };
            lc.handoff_and_open(
                backend,
                old,
                device,
                true,
                10,
                48_000,
                1024,
                SinkChoice::Dsp,
                dsp,
                &hook,
            )
        };
        let mut backend = MockBackend::new();
        let mut lc = DeviceLifecycle::new();
        let a = open(&mut lc, &mut backend, None, "hw:CardA,0").unwrap();
        assert!(
            backend.journal.borrow().is_empty(),
            "first open: nothing to hand over"
        );
        let _b = open(&mut lc, &mut backend, Some(a.output), "hw:CardB,0").unwrap();
        let j = backend.journal.borrow().clone();
        // Card A's hand-over names card A, never the card being opened.
        assert_eq!(
            j.first().map(String::as_str),
            Some("hook hw:CardA,0"),
            "{j:?}"
        );
        assert!(j.contains(&"release 1".to_string()), "{j:?}");
        // A same-device reopen (a rate switch) is no hand-over.
        backend.journal.borrow_mut().clear();
        let b2 = open(
            &mut lc,
            &mut backend,
            Some("hw:CardB,0".into()),
            "hw:CardB,0",
        )
        .unwrap();
        assert!(
            !backend
                .journal
                .borrow()
                .iter()
                .any(|e| e.starts_with("hook"))
        );
        // Resolving fails with the card reserved: hook first, then release.
        backend.journal.borrow_mut().clear();
        backend.fail_resolve = true;
        assert!(open(&mut lc, &mut backend, Some(b2.output), "hw:CardB,0").is_err());
        let j = backend.journal.borrow().clone();
        let hook = j
            .iter()
            .position(|e| e == "hook hw:CardB,0")
            .expect("no hook");
        let release = j.iter().position(|e| e == "release 2").expect("no release");
        assert!(hook < release, "{j:?}");
        // Opening fails: the same.
        backend.fail_resolve = false;
        backend.fail_open_devices.insert("hw:CardA,0".into());
        backend.journal.borrow_mut().clear();
        assert!(open(&mut lc, &mut backend, None, "hw:CardA,0").is_err());
        let j = backend.journal.borrow().clone();
        let hook = j
            .iter()
            .position(|e| e == "hook hw:CardA,0")
            .expect("no hook");
        let release = j.iter().position(|e| e == "release 1").expect("no release");
        assert!(hook < release, "{j:?}");
    }

    #[test]
    fn an_open_reports_the_session_it_started_in() {
        // A stream starts while the device opens (and, in the driver, while
        // the chain is built): what this open saw belongs to the session
        // before, so the new one retries it.
        let mut backend = MockBackend::new();
        backend.session = 4;
        backend.session_steps_per_open = 1;
        let mut lc = DeviceLifecycle::new();
        let opened = lc
            .handoff_and_open(
                &mut backend,
                None,
                "hw:CardA,0",
                true,
                10,
                48_000,
                1024,
                SinkChoice::Dsp,
                DesiredDspConfig::default(),
                &|_, _| {},
            )
            .unwrap();
        assert_eq!(backend.session, 5);
        assert_eq!(opened.session, 4);
        let k = key(48_000, &[RateRatio::integer(4)]).with_session(opened.session);
        assert_eq!(k.in_session(backend.session).refused, Rungs::EMPTY);
    }

    #[test]
    fn resolved_graph_rate_is_opened_and_reported() {
        let mut backend = MockBackend::new();
        backend.only_rate = Some(48_000);
        let mut lc = DeviceLifecycle::new();
        let dsp = DesiredDspConfig::default();
        let opened = lc
            .handoff_and_open(
                &mut backend,
                None,
                "hw:CardA,0",
                true,
                10,
                44_100,
                882,
                SinkChoice::Dsp,
                dsp,
                &|_, _| {},
            )
            .unwrap();
        assert_eq!(opened.graph_rate, 48_000);
        assert_eq!(opened.quantum, 960);
        assert_eq!(lc.active_graph_rate, 48_000);
        assert!(
            backend
                .ops
                .contains(&Op::Open("hw:CardA,0".into(), 48_000, 1))
        );
    }

    #[test]
    fn switching_between_different_cards_releases_old_reservation_before_reserving_new() {
        let mut backend = MockBackend::new();
        let mut lc = DeviceLifecycle::new();
        let dsp = DesiredDspConfig {
            dsp_enabled: true,
            rate_target: RateTarget::Ratio(4),
            max_ratio: 16,
            generation: 1,
            buildable: RatePairMask::ALL,
            bypass: crate::lifecycle::BypassLeg::OFF,
        };

        let out_a = lc
            .handoff_and_open(
                &mut backend,
                None,
                "hw:CardA,0,0",
                true,
                1000,
                48_000,
                1024,
                SinkChoice::Dsp,
                dsp,
                &|_, _| {},
            )
            .unwrap()
            .output;
        assert_eq!(lc.state, EngineState::Ready);
        assert_eq!(lc.reservation, Some((1, 1)));

        let out_b = lc
            .handoff_and_open(
                &mut backend,
                Some(out_a),
                "hw:CardB,0,0",
                true,
                1000,
                48_000,
                1024,
                SinkChoice::Dsp,
                dsp,
                &|_, _| {},
            )
            .unwrap()
            .output;
        assert_eq!(out_b, "hw:CardB,0,0");
        assert_eq!(lc.state, EngineState::Ready);
        assert_eq!(lc.reservation, Some((2, 2)));
        assert_eq!(lc.active_device.as_deref(), Some("hw:CardB,0,0"));
        assert_eq!(
            backend.ops,
            vec![
                Op::Reserve(1, "hw:CardA,0,0".into()),
                Op::Open("hw:CardA,0,0".into(), 48_000, 4),
                Op::Close("hw:CardA,0,0".into()),
                Op::Release(1),
                Op::Reserve(2, "hw:CardB,0,0".into()),
                Op::Open("hw:CardB,0,0".into(), 48_000, 4),
            ]
        );
    }

    #[test]
    fn switching_substreams_on_same_card_keeps_reservation_without_dropping() {
        let mut backend = MockBackend::new();
        let mut lc = DeviceLifecycle::new();
        let dsp = DesiredDspConfig::default();

        let out0 = lc
            .handoff_and_open(
                &mut backend,
                None,
                "hw:Loopback,0,0",
                true,
                1000,
                48_000,
                1024,
                SinkChoice::Dsp,
                dsp,
                &|_, _| {},
            )
            .unwrap()
            .output;
        let _out1 = lc
            .handoff_and_open(
                &mut backend,
                Some(out0),
                "hw:Loopback,0,1",
                true,
                1000,
                48_000,
                1024,
                SinkChoice::Dsp,
                dsp,
                &|_, _| {},
            )
            .unwrap()
            .output;

        assert_eq!(lc.reservation, Some((10, 10)));
        assert_eq!(lc.active_device.as_deref(), Some("hw:Loopback,0,1"));
        assert_eq!(
            backend.ops,
            vec![
                Op::Reserve(10, "hw:Loopback,0,0".into()),
                Op::Open("hw:Loopback,0,0".into(), 48_000, 1),
                Op::Close("hw:Loopback,0,0".into()),
                Op::Open("hw:Loopback,0,1".into(), 48_000, 1),
            ]
        );
    }

    #[test]
    fn device_change_failure_enters_error_and_does_not_silently_keep_old_device() {
        let mut backend = MockBackend::new();
        backend.fail_open_devices.insert("hw:CardB,0,0".into());
        let mut lc = DeviceLifecycle::new();
        let dsp = DesiredDspConfig::default();

        let out_a = lc
            .handoff_and_open(
                &mut backend,
                None,
                "hw:CardA,0,0",
                true,
                1000,
                48_000,
                1024,
                SinkChoice::Dsp,
                dsp,
                &|_, _| {},
            )
            .unwrap()
            .output;

        let err = lc
            .handoff_and_open(
                &mut backend,
                Some(out_a),
                "hw:CardB,0,0",
                true,
                1000,
                48_000,
                1024,
                SinkChoice::Dsp,
                dsp,
                &|_, _| {},
            )
            .unwrap_err();
        assert!(err.contains("hw:CardB,0,0"), "got: {err}");
        assert_eq!(lc.state, EngineState::Error);
        assert!(lc.last_error.is_some());
        assert_eq!(lc.reservation, None, "failed switch must release card B");
        assert_eq!(lc.active_device, None, "old device A must not stay active");
        assert!(!lc.output_held);

        // Switching to a working device afterwards recovers cleanly out of Error.
        let _out_c = lc
            .handoff_and_open(
                &mut backend,
                None,
                "hw:Loopback,0,0",
                true,
                1000,
                48_000,
                1024,
                SinkChoice::Dsp,
                dsp,
                &|_, _| {},
            )
            .unwrap()
            .output;
        assert_eq!(lc.state, EngineState::Ready);
        assert!(lc.last_error.is_none());
        assert_eq!(lc.active_device.as_deref(), Some("hw:Loopback,0,0"));
    }

    #[test]
    fn rate_target_max_rate_resolves_against_actual_graph_rate_and_direct_dsp_transitions() {
        let cfg1 = DesiredDspConfig {
            dsp_enabled: true,
            rate_target: RateTarget::Off,
            max_ratio: 16,
            generation: 1,
            buildable: RatePairMask::ALL,
            bypass: crate::lifecycle::BypassLeg::OFF,
        };
        // Scenario 1: start ratio 1 -> update config to x4 (gen 2) -> Direct (x1) -> DSP (must be x4!).
        let cfg2 = DesiredDspConfig {
            dsp_enabled: true,
            rate_target: RateTarget::Ratio(4),
            max_ratio: 16,
            generation: 2,
            buildable: RatePairMask::ALL,
            bypass: crate::lifecycle::BypassLeg::OFF,
        };
        assert_eq!(
            cfg1.resolve_ratio_for(SinkChoice::Dsp, 48_000),
            RateRatio::integer(1)
        );
        assert_eq!(
            decide_rt_reopen(
                cfg2,
                Some(key(48_000, &[])),
                SinkChoice::Dsp,
                SinkChoice::Dsp,
                48_000,
                48_000,
                RateRatio::integer(1),
            ),
            Some(RateRatio::integer(4))
        );
        // On Direct, must reopen at x1:
        assert_eq!(
            decide_rt_reopen(
                cfg2,
                Some(key(48_000, &[])),
                SinkChoice::Direct,
                SinkChoice::Direct,
                48_000,
                48_000,
                RateRatio::integer(4),
            ),
            Some(RateRatio::integer(1))
        );
        // Back from Direct -> DSP using updated cfg2 (gen 2) must reopen at x4:
        assert_eq!(
            decide_rt_reopen(
                cfg2,
                Some(key(48_000, &[])),
                SinkChoice::Dsp,
                SinkChoice::Dsp,
                48_000,
                48_000,
                RateRatio::integer(1),
            ),
            Some(RateRatio::integer(4))
        );

        // Scenario 1b: Hardware capped at 96k (ratio 2 instead of 4: the
        // open saw x4 refused) must NOT loop reopening!
        assert_eq!(
            decide_rt_reopen(
                cfg2,
                Some(key(48_000, &[RateRatio::integer(4)])),
                SinkChoice::Dsp,
                SinkChoice::Dsp,
                48_000,
                48_000,
                RateRatio::integer(2),
            ),
            None
        );

        // Scenario 2: MaxRate(192000) resolves 44.1k->x4(176.4k), 48k->x4(192k), 96k->x2(192k).
        let max192 = DesiredDspConfig {
            dsp_enabled: true,
            rate_target: RateTarget::MaxRate(192_000),
            max_ratio: 16,
            generation: 3,
            buildable: RatePairMask::ALL,
            bypass: crate::lifecycle::BypassLeg::OFF,
        };
        assert_eq!(
            max192.resolve_ratio_for(SinkChoice::Dsp, 44_100),
            RateRatio::integer(4)
        );
        assert_eq!(
            max192.resolve_ratio_for(SinkChoice::Dsp, 48_000),
            RateRatio::integer(4)
        );
        assert_eq!(
            max192.resolve_ratio_for(SinkChoice::Dsp, 96_000),
            RateRatio::integer(2)
        );
        assert_eq!(
            max192.resolve_ratio_for(SinkChoice::Dsp, 192_000),
            RateRatio::integer(1)
        );

        // Switching source 48k -> 96k while on DSP triggers reopen with ratio 2 (192k), NOT ratio 4 (384k).
        assert_eq!(
            decide_rt_reopen(
                max192,
                Some(key(48_000, &[])),
                SinkChoice::Dsp,
                SinkChoice::Dsp,
                48_000,
                96_000,
                RateRatio::integer(4),
            ),
            Some(RateRatio::integer(2))
        );
    }

    #[test]
    fn config_edit_on_ratio_capped_device_does_not_reopen() {
        // 48k-only device: x4 wanted, x1 granted. A later config change (new
        // generation, same target) must not reopen; a new target must.
        let gen1 = DesiredDspConfig {
            dsp_enabled: true,
            rate_target: RateTarget::Ratio(4),
            max_ratio: 16,
            generation: 1,
            buildable: RatePairMask::ALL,
            bypass: crate::lifecycle::BypassLeg::OFF,
        };
        let open = audivane_io_alsa::RatioOpen {
            ratio: RateRatio::ONE,
            refused: vec![RateRatio::integer(4), RateRatio::integer(2)],
            transient: vec![],
        };
        let key = negotiated_key(gen1, SinkChoice::Dsp, 48_000, None, &open);
        assert_eq!(
            key.refused.as_slice(),
            [RateRatio::integer(4), RateRatio::integer(2)]
        );
        let gen2 = DesiredDspConfig {
            generation: 2,
            ..gen1
        };
        let r = |cfg| {
            decide_rt_reopen(
                cfg,
                Some(key),
                SinkChoice::Dsp,
                SinkChoice::Dsp,
                48_000,
                48_000,
                RateRatio::ONE,
            )
        };
        assert_eq!(r(gen2), None);
        // x2 was refused on the way down too: a fresh open lands on x1.
        let gen3 = DesiredDspConfig {
            rate_target: RateTarget::Ratio(2),
            generation: 3,
            ..gen1
        };
        assert_eq!(r(gen3), None);
        // x3 was never tried: reopen.
        let gen4 = DesiredDspConfig {
            rate_target: RateTarget::Ratio(3),
            generation: 4,
            ..gen1
        };
        assert_eq!(r(gen4), Some(RateRatio::integer(3)));
    }

    #[test]
    fn reopen_gate_suppresses_duplicate_reopens_and_allows_error_recovery() {
        let mut gate = ReopenGate::new();

        // Rapid double force_reopen on control thread emits only one SuspendForReopen.
        assert!(gate.ctrl_request_reopen(true, false, EngineState::Ready));
        assert!(!gate.ctrl_request_reopen(true, false, EngineState::Switching));

        // RT thread accepts SuspendForReopen and suppresses concurrent NeedReopen.
        assert!(gate.rt_try_begin_reopen());
        assert!(!gate.rt_try_begin_reopen());

        // Control thread processes NeedReopen, then fails -> sends ReopenAborted.
        assert!(gate.ctrl_take_awaiting_cfg());
        assert!(!gate.ctrl_take_awaiting_cfg());
        gate.rt_finish_reopen();

        // Recovery apply from EngineState::Error MUST trigger a fresh reopen.
        assert!(gate.ctrl_request_reopen(true, false, EngineState::Error));
        assert!(gate.rt_try_begin_reopen());
        assert!(gate.ctrl_take_awaiting_cfg());
        gate.rt_finish_reopen();

        // Released / Idle state suppresses reopen requests.
        assert!(!gate.ctrl_request_reopen(true, true, EngineState::Ready));
        assert!(!gate.ctrl_request_reopen(true, false, EngineState::Idle));
    }

    #[test]
    fn rational_targets_resolve_and_build_ladders() {
        use audivane_protocol::ipc::{FormatCapability, RateCapability};
        let cfg = |target| DesiredDspConfig {
            rate_target: target,
            ..DesiredDspConfig::default()
        };
        let r = cfg(RateTarget::Rate(48_000));
        assert_eq!(
            r.resolve_ratio_for(SinkChoice::Dsp, 44_100),
            RateRatio::new(160, 147)
        );
        assert_eq!(
            r.resolve_ratio_for(SinkChoice::Dsp, 96_000),
            RateRatio::new(1, 2)
        );
        assert_eq!(r.resolve_ratio_for(SinkChoice::Dsp, 48_000), RateRatio::ONE);
        assert_eq!(
            r.resolve_ratio_for(SinkChoice::Direct, 44_100),
            RateRatio::ONE
        );
        assert_eq!(
            r.ratio_ladder_for(SinkChoice::Dsp, 44_100, None, &[]),
            vec![RateRatio::new(160, 147), RateRatio::ONE]
        );

        // Without a probe the any-family cap resolves like the integer one.
        let any = cfg(RateTarget::MaxRateAny(192_000));
        assert_eq!(
            any.resolve_ratio_for(SinkChoice::Dsp, 44_100),
            RateRatio::integer(4)
        );
        // With one it tries every device rate under the cap, highest first.
        let rate = |rate| RateCapability {
            rate,
            formats: vec![FormatCapability {
                format: audivane_protocol::SampleFormat::S32,
                sbits: Some(32),
            }],
        };
        let caps = DeviceCapabilities {
            device: "hw:Test".into(),
            channels: 2,
            channels_min: 2,
            channels_max: 2,
            rates: [44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 384_000]
                .into_iter()
                .map(rate)
                .collect(),
            rate_min: 44_100,
            rate_max: 384_000,
            formats: vec![audivane_protocol::SampleFormat::S32],
        };
        // The source family first: x4 (176.4 kHz) beats 192 kHz, which is
        // not twice as high.
        let ladder = any.ratio_ladder_for(SinkChoice::Dsp, 44_100, Some(&caps), &[]);
        assert_eq!(
            ladder,
            vec![
                RateRatio::integer(4),
                RateRatio::integer(2),
                RateRatio::from_rates(44_100, 192_000),
                RateRatio::from_rates(44_100, 96_000),
                RateRatio::from_rates(44_100, 48_000),
                RateRatio::ONE,
            ]
        );
        // Direct and DSP off only ever open x1.
        let rates = DeviceRates::playable(Some(&caps), &[]);
        assert_eq!(
            any.ladder(SinkChoice::Direct, 44_100, rates).as_slice(),
            [RateRatio::ONE]
        );
        assert_eq!(
            any.ratio_ladder_for(SinkChoice::Direct, 44_100, Some(&caps), &[]),
            [RateRatio::ONE]
        );
        assert_eq!(
            any.open_landing(SinkChoice::Direct, 44_100, rates, &Rungs::EMPTY),
            Some(RateRatio::ONE)
        );
    }

    #[test]
    fn the_mask_covers_every_probed_rate() {
        let probed = crate::engine_driver::engine_probe_rates(audivane_protocol::MAX_INTEGER_RATIO);
        assert_eq!(probed, MASK_OUTPUT_RATES.to_vec());
        // Up to 768 kHz: every multiple x1..=16 of a graph rate.
        let up_to_768k =
            audivane_io_alsa::caps::probe_rates(audivane_protocol::ALLOWED_RATES, 16, 768_000);
        assert_eq!(probed[..up_to_768k.len()], up_to_768k[..]);
        assert_eq!(
            probed[up_to_768k.len()..],
            audivane_protocol::HIGH_OUTPUT_RATES[..]
        );
        // A x16 limit probes what it always did up to 768 kHz (and the
        // high rates the higher graph rates reach: 192 kHz x8 = 1.536 MHz).
        let x16 = crate::engine_driver::engine_probe_rates(16);
        assert_eq!(x16[..up_to_768k.len()], up_to_768k[..]);
        // A design that only runs at integer ratios, on a device that
        // reports every probed rate (a continuous-range DAC, the loopback):
        // 176.4k -> 288k (80/49) and the other unlisted-before rates are
        // refused like the listed ones.
        let integer_only = RatePairMask::from_fn(|i, o| o % i == 0);
        let ladder = max_rate_any_ladder(176_400, 300_000, &probed, 16, integer_only);
        assert_eq!(ladder, vec![RateRatio::ONE]);
        let ladder = max_rate_any_ladder(44_100, 300_000, &probed, 16, integer_only);
        assert_eq!(ladder[0], RateRatio::integer(6));
        assert!(ladder.iter().all(|q| q.is_integer()));
        // Intersection: a pair is buildable only when both masks allow it.
        let up_only = RatePairMask::from_fn(|i, o| o > i);
        let both = integer_only.intersect(up_only);
        assert!(both.allows(44_100, 88_200));
        assert!(!both.allows(44_100, 48_000));
        assert!(!both.allows(88_200, 44_100));
        assert!(both.differs_from(up_only, 88_200));
        assert!(!both.differs_from(integer_only, 44_100));
        assert_eq!(RatePairMask::ALL.intersect(both), both);
    }

    /// Where a fresh open lands for `cfg` on a device that plays `caps`.
    fn landing(cfg: DesiredDspConfig, graph: u32, caps: &DeviceCapabilities) -> RateRatio {
        cfg.open_landing(
            SinkChoice::Dsp,
            graph,
            DeviceRates::playable(Some(caps), &[]),
            &Rungs::EMPTY,
        )
        .unwrap()
    }

    fn target(rate_target: RateTarget) -> DesiredDspConfig {
        DesiredDspConfig {
            rate_target,
            ..DesiredDspConfig::default()
        }
    }

    #[test]
    fn a_device_up_to_3072k_opens_x32_and_x64() {
        // A continuous-range DAC to 3.072 MHz reports every probed rate.
        let dac = caps_of(&crate::engine_driver::engine_probe_rates(64));
        assert_eq!(dac.rate_max, 3_072_000);
        let top = target(RateTarget::MaxRate(MAX_OUTPUT_RATE));
        for (graph, ratio) in [(44_100, 64), (48_000, 64), (88_200, 32), (96_000, 32)] {
            assert_eq!(landing(top, graph, &dac), RateRatio::integer(ratio));
        }
        assert_eq!(landing(top, 768_000, &dac), RateRatio::integer(4));
        let rates = DeviceRates::playable(Some(&dac), &[]);
        assert_eq!(
            top.ladder(SinkChoice::Dsp, 44_100, rates).as_slice(),
            [64, 32, 16, 8, 4, 2, 1].map(RateRatio::integer)
        );
        // Either family, highest first.
        let any = target(RateTarget::MaxRateAny(MAX_OUTPUT_RATE));
        assert_eq!(landing(any, 44_100, &dac), RateRatio::integer(64));
        assert_eq!(landing(any, 48_000, &dac), RateRatio::integer(64));
        let any_1536 = target(RateTarget::MaxRateAny(1_536_000));
        assert_eq!(landing(any_1536, 44_100, &dac), RateRatio::integer(32));
        // A ratio that lands on no DAC rate (44.1 kHz x48 = 2.1168 MHz)
        // falls back to the next one that does (x32 = 1.4112 MHz).
        assert_eq!(
            landing(target(RateTarget::Ratio(48)), 44_100, &dac),
            RateRatio::integer(32)
        );
        assert_eq!(
            landing(target(RateTarget::Ratio(64)), 48_000, &dac),
            RateRatio::integer(64)
        );
        // A fixed output rate: integer from its own family, rational within
        // the term limit from the other.
        let fixed = target(RateTarget::Rate(2_822_400));
        assert_eq!(landing(fixed, 44_100, &dac), RateRatio::integer(64));
        assert_eq!(landing(fixed, 48_000, &dac), RateRatio::new(294, 5));
        // 44.1 kHz -> 3.072 MHz is 10240/147, past the term limit: never
        // opened; the source rate plays instead.
        let fixed = target(RateTarget::Rate(3_072_000));
        assert_eq!(landing(fixed, 44_100, &dac), RateRatio::ONE);
        // A 48 kHz-family-only DAC to 3.072 MHz with a 44.1 kHz source
        // crosses over at the highest rate within the term limit.
        let family48: Vec<u32> = crate::engine_driver::engine_probe_rates(64)
            .into_iter()
            .filter(|r| r % 48_000 == 0)
            .collect();
        let dac48 = caps_of(&family48);
        assert_eq!(
            landing(any, 44_100, &dac48),
            RateRatio::from_rates(44_100, 768_000)
        );
        // The engine's own ratio limit still applies.
        let x16 = DesiredDspConfig {
            max_ratio: 16,
            ..top
        };
        assert_eq!(landing(x16, 44_100, &dac), RateRatio::integer(16));
    }

    #[test]
    fn a_device_that_stops_at_768k_is_never_offered_more() {
        let rates: Vec<u32> = crate::engine_driver::engine_probe_rates(64)
            .into_iter()
            .filter(|&r| r <= 768_000)
            .collect();
        let dac = caps_of(&rates);
        for rate_target in [
            RateTarget::MaxRate(MAX_OUTPUT_RATE),
            RateTarget::MaxRateAny(MAX_OUTPUT_RATE),
            RateTarget::Ratio(64),
            RateTarget::Ratio(48),
            RateTarget::Rate(1_411_200),
        ] {
            let cfg = target(rate_target);
            for &graph in audivane_protocol::ALLOWED_RATES {
                let landed = landing(cfg, graph, &dac);
                let out = landed.output_rate(graph).unwrap();
                assert!(out <= 768_000, "{rate_target:?} at {graph}: {landed}");
                let rates = DeviceRates::playable(Some(&dac), &[]);
                assert!(
                    rates.is_some_and(|r| r.iter().all(|r| r <= 768_000)),
                    "the device rates stay as probed"
                );
            }
        }
        // Exactly what a x16 engine picks there.
        let top = target(RateTarget::MaxRate(MAX_OUTPUT_RATE));
        assert_eq!(landing(top, 44_100, &dac), RateRatio::integer(16));
        assert_eq!(landing(top, 48_000, &dac), RateRatio::integer(16));
        assert_eq!(landing(top, 96_000, &dac), RateRatio::integer(8));
        assert_eq!(landing(top, 768_000, &dac), RateRatio::ONE);
        let any = target(RateTarget::MaxRateAny(MAX_OUTPUT_RATE));
        assert_eq!(landing(any, 44_100, &dac), RateRatio::integer(16));
        // A fixed high rate falls down its integer ladder like any other.
        let fixed = target(RateTarget::Rate(1_411_200));
        assert_eq!(landing(fixed, 44_100, &dac), RateRatio::integer(16));
    }

    #[test]
    fn max_rate_any_stays_in_the_source_family_unless_the_other_is_far_better() {
        let r = RateRatio::from_rates;
        let all = RatePairMask::ALL;
        // A 192k-family-only DAC above x1: no integer multiple, so cross.
        assert_eq!(
            max_rate_any_ladder(44_100, 768_000, &[44_100, 48_000, 96_000, 192_000], 16, all),
            vec![
                r(44_100, 192_000),
                r(44_100, 96_000),
                r(44_100, 48_000),
                RateRatio::ONE
            ]
        );
        // Same family up to 88.2k, other family to 384k (more than twice).
        assert_eq!(
            max_rate_any_ladder(
                44_100,
                768_000,
                &[44_100, 88_200, 192_000, 384_000],
                16,
                all
            ),
            vec![
                r(44_100, 384_000),
                r(44_100, 192_000),
                RateRatio::integer(2),
                RateRatio::ONE
            ]
        );
        // 44.1k -> 768k is not taken over x16 (705.6k), nor over x8.
        let full = audivane_protocol::ALLOWED_RATES;
        assert_eq!(
            max_rate_any_ladder(44_100, 768_000, full, 16, all)[0],
            RateRatio::integer(16)
        );
        assert_eq!(
            max_rate_any_ladder(44_100, 400_000, full, 16, all)[0],
            RateRatio::integer(8)
        );
        // A device that cannot play the source rate: x1 first, then
        // decimation.
        assert_eq!(
            max_rate_any_ladder(96_000, 768_000, &[44_100, 48_000], 16, all),
            vec![RateRatio::ONE, r(96_000, 48_000), r(96_000, 44_100)]
        );
        // Unbuildable pairs are never offered.
        let no_cross = RatePairMask::from_fn(|i, o| o % i == 0 || i % o == 0);
        assert_eq!(
            max_rate_any_ladder(
                44_100,
                768_000,
                &[44_100, 48_000, 96_000, 192_000],
                16,
                no_cross
            ),
            vec![RateRatio::ONE]
        );
        assert!(no_cross.allows(48_000, 96_000));
        assert!(!no_cross.allows(44_100, 48_000));
        assert!(no_cross.allows(44_100, 44_100));
        // Pairs outside the lists are refused, except under ALL.
        assert!(!no_cross.allows(44_100, 12_345));
        assert!(RatePairMask::ALL.allows(44_100, 12_345));
        assert!(!no_cross.allows(12_345, 48_000));
        // The plain ladders and the resolution respect the mask too.
        let cfg = DesiredDspConfig {
            rate_target: RateTarget::Rate(48_000),
            buildable: no_cross,
            ..DesiredDspConfig::default()
        };
        assert_eq!(
            cfg.resolve_ratio_for(SinkChoice::Dsp, 44_100),
            RateRatio::ONE
        );
        assert_eq!(
            cfg.ratio_ladder_for(SinkChoice::Dsp, 44_100, None, &[]),
            vec![RateRatio::ONE]
        );
        assert_eq!(
            cfg.ratio_ladder_for(SinkChoice::Dsp, 96_000, None, &[]),
            vec![r(96_000, 48_000), RateRatio::ONE]
        );
    }

    fn caps_of(rates: &[u32]) -> DeviceCapabilities {
        use audivane_protocol::ipc::{FormatCapability, RateCapability};
        DeviceCapabilities {
            device: "hw:Test".into(),
            channels: 2,
            channels_min: 2,
            channels_max: 2,
            rates: rates
                .iter()
                .map(|&rate| RateCapability {
                    rate,
                    formats: vec![FormatCapability {
                        format: audivane_protocol::SampleFormat::S32,
                        sbits: Some(32),
                    }],
                })
                .collect(),
            rate_min: rates.iter().copied().min().unwrap_or(0),
            rate_max: rates.iter().copied().max().unwrap_or(0),
            formats: vec![audivane_protocol::SampleFormat::S32],
        }
    }

    fn any_family(cap: u32) -> DesiredDspConfig {
        DesiredDspConfig {
            rate_target: RateTarget::MaxRateAny(cap),
            ..DesiredDspConfig::default()
        }
    }

    /// A device that plays `rates`, each in its listed formats.
    fn caps_with(rates: &[(u32, &[SampleFormat])]) -> DeviceCapabilities {
        use audivane_protocol::ipc::{FormatCapability, RateCapability};
        let mut caps = caps_of(&rates.iter().map(|r| r.0).collect::<Vec<_>>());
        caps.rates = rates
            .iter()
            .map(|&(rate, formats)| RateCapability {
                rate,
                formats: formats
                    .iter()
                    .map(|&format| FormatCapability {
                        format,
                        sbits: None,
                    })
                    .collect(),
            })
            .collect();
        caps
    }

    /// An open without a probe, like the driver's: the first ladder entry
    /// the device takes, and its key.
    fn open_blind(
        cfg: DesiredDspConfig,
        sink: SinkChoice,
        graph: u32,
        takes: &[u32],
    ) -> (RateRatio, NegotiatedKey) {
        let ladder = cfg.ratio_ladder_for(sink, graph, None, &[]);
        let landed = *ladder
            .iter()
            .find(|q| q.output_rate(graph).is_some_and(|o| takes.contains(&o)))
            .unwrap();
        // Every entry before it was refused for good.
        let open = audivane_io_alsa::RatioOpen {
            ratio: landed,
            refused: ladder
                .iter()
                .copied()
                .take_while(|&q| q != landed)
                .collect(),
            transient: vec![],
        };
        (landed, negotiated_key(cfg, sink, graph, None, &open))
    }

    #[test]
    fn the_landing_is_the_open_plan() {
        use SampleFormat::{S16, S32};
        let integer_only = RatePairMask::from_fn(|i, o| o % i == 0);
        let no_44k_rational = RatePairMask::from_fn(|i, o| o % i == 0 || i % 11_025 != 0);
        let targets = [
            RateTarget::Off,
            RateTarget::Ratio(4),
            RateTarget::Ratio(6),
            RateTarget::MaxRate(192_000),
            RateTarget::Rate(48_000),
            RateTarget::Rate(768_000),
            RateTarget::Rate(44_100),
            RateTarget::MaxRateAny(48_000),
            RateTarget::MaxRateAny(192_000),
            RateTarget::MaxRateAny(768_000),
        ];
        let both: &[SampleFormat] = &[S16, S32];
        let s16: &[SampleFormat] = &[S16];
        let devices: Vec<Vec<(u32, &[SampleFormat])>> = vec![
            vec![(44_100, both), (48_000, both)],
            vec![(176_400, both), (192_000, s16)],
            vec![
                (48_000, both),
                (96_000, both),
                (192_000, both),
                (288_000, both),
            ],
            vec![
                (44_100, both),
                (88_200, s16),
                (176_400, both),
                (768_000, s16),
            ],
            MASK_OUTPUT_RATES
                .iter()
                .map(|&r| (r, if r > 192_000 { s16 } else { both }))
                .collect(),
        ];
        let mut checked = 0;
        for rate_target in targets {
            for buildable in [RatePairMask::ALL, integer_only, no_44k_rational] {
                for dsp_enabled in [true, false] {
                    let cfg = DesiredDspConfig {
                        dsp_enabled,
                        rate_target,
                        buildable,
                        ..DesiredDspConfig::default()
                    };
                    for sink in [SinkChoice::Dsp, SinkChoice::Direct] {
                        for &graph in audivane_protocol::ALLOWED_RATES {
                            for dev in &devices {
                                let caps = caps_with(dev);
                                for formats in [&[S32][..], &[S16], &[S32, S16], &[]] {
                                    // With a probe: exactly ALSA's plan.
                                    let ladder =
                                        cfg.ratio_ladder_for(sink, graph, Some(&caps), formats);
                                    let fmts: &[SampleFormat] = if formats.is_empty() {
                                        &audivane_io_alsa::FORMAT_PREFERENCE
                                    } else {
                                        formats
                                    };
                                    let plan =
                                        audivane_io_alsa::plan_open(&caps, graph, &ladder, fmts)
                                            .ok()
                                            .map(|(q, _)| q);
                                    let rates = DeviceRates::playable(Some(&caps), formats);
                                    let landing =
                                        cfg.open_landing(sink, graph, rates, &Rungs::EMPTY);
                                    assert_eq!(
                                        landing, plan,
                                        "{cfg:?} {sink:?} {graph} {dev:?} {formats:?}"
                                    );
                                    checked += 1;
                                }
                                // Without a probe: the open's own key
                                // predicts it.
                                let takes: Vec<u32> = dev.iter().map(|r| r.0).collect();
                                if cfg
                                    .ratio_ladder_for(sink, graph, None, &[])
                                    .iter()
                                    .any(|q| {
                                        q.output_rate(graph).is_some_and(|o| takes.contains(&o))
                                    })
                                {
                                    let (landed, key) = open_blind(cfg, sink, graph, &takes);
                                    assert_eq!(
                                        cfg.open_landing(sink, graph, None, &key.refused),
                                        Some(landed)
                                    );
                                    assert_eq!(
                                        decide_rt_reopen(
                                            cfg,
                                            Some(key),
                                            sink,
                                            sink,
                                            graph,
                                            graph,
                                            landed
                                        ),
                                        None
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
        assert_eq!(checked, 10 * 3 * 2 * 2 * 10 * 5 * 4);
        // The integer ladder is still `audivane_io_alsa::ratio_ladder`.
        let cfg = DesiredDspConfig {
            rate_target: RateTarget::Ratio(15),
            ..DesiredDspConfig::default()
        };
        assert_eq!(
            cfg.ratio_ladder_for(SinkChoice::Dsp, 48_000, None, &[]),
            audivane_io_alsa::ratio_ladder(RateRatio::integer(15))
        );
    }

    #[test]
    fn a_blind_open_reopens_when_a_fresh_one_would_land_elsewhere() {
        // No probe; the device takes x4 and x5 at 48k but not x6. An x6
        // design that cannot run x5 opened x4 (x6 refused); a design that
        // can lands on x5, so the switch reopens although both ladders
        // start at x6.
        let takes = [48_000, 192_000, 240_000];
        let no_x5 = RatePairMask::from_fn(|i, o| o % i == 0 && o / i != 5);
        let old = DesiredDspConfig {
            rate_target: RateTarget::Ratio(6),
            buildable: no_x5,
            ..DesiredDspConfig::default()
        };
        let new = DesiredDspConfig {
            buildable: RatePairMask::ALL,
            generation: 1,
            ..old
        };
        let (running, key) = open_blind(old, SinkChoice::Dsp, 48_000, &takes);
        assert_eq!(running, RateRatio::integer(4));
        assert_eq!(key.refused.as_slice(), [RateRatio::integer(6)]);
        let fresh = open_blind(new, SinkChoice::Dsp, 48_000, &takes).0;
        assert_eq!(fresh, RateRatio::integer(5));
        let rt = decide_rt_reopen(
            new,
            Some(key),
            SinkChoice::Dsp,
            SinkChoice::Dsp,
            48_000,
            48_000,
            running,
        );
        assert_eq!(rt, Some(fresh));
        // Back to the design without x5: x6 was refused, so it lands on x4.
        let key5 = open_blind(new, SinkChoice::Dsp, 48_000, &takes).1;
        let back = DesiredDspConfig {
            generation: 2,
            ..old
        };
        let rt = decide_rt_reopen(
            back,
            Some(key5),
            SinkChoice::Dsp,
            SinkChoice::Dsp,
            48_000,
            48_000,
            fresh,
        );
        assert_eq!(rt, Some(RateRatio::integer(4)));
    }

    #[test]
    fn transient_failures_do_not_pin_the_fallback() {
        // x6 busy once on a blind open (x5 not supported), so it fell
        // back to x4.
        let cfg = DesiredDspConfig {
            rate_target: RateTarget::Ratio(6),
            generation: 3,
            ..DesiredDspConfig::default()
        };
        let x = RateRatio::integer;
        let busy = audivane_io_alsa::RatioOpen {
            ratio: x(4),
            refused: vec![x(5)],
            transient: vec![x(6)],
        };
        let key = negotiated_key(cfg, SinkChoice::Dsp, 48_000, None, &busy);
        let rt = |cfg, key| {
            decide_rt_reopen(
                cfg,
                Some(key),
                SinkChoice::Dsp,
                SinkChoice::Dsp,
                48_000,
                48_000,
                x(4),
            )
        };
        // The RT thread's own checks do not reopen into it again...
        assert_eq!(rt(cfg, key), None);
        // ...nor does an edit that leaves negotiation alone (volume, EQ):
        // it keeps the generation, so dragging the volume never retries x6
        // and interrupts playback.
        let volume_edit = cfg.next(cfg, false);
        assert_eq!(volume_edit.generation, cfg.generation);
        assert_eq!(rt(volume_edit, key), None);
        // ...but an edit that steers negotiation tries x6 again: a fresh
        // open would take it.
        let edit = cfg.next(
            DesiredDspConfig {
                max_ratio: 12,
                ..cfg
            },
            false,
        );
        assert_eq!(edit.generation, cfg.generation + 1);
        assert_eq!(rt(edit, key), Some(x(6)));
        // So does any edit that reopens anyway.
        assert_eq!(cfg.next(cfg, true).generation, cfg.generation + 1);
        // A capability refusal holds across edits, until a fresh stream
        // start (a new playback session) tries it once more.
        let unsupported = audivane_io_alsa::RatioOpen {
            ratio: x(4),
            refused: vec![x(6), x(5)],
            transient: vec![],
        };
        let key = negotiated_key(cfg, SinkChoice::Dsp, 48_000, None, &unsupported).with_session(7);
        assert_eq!(rt(edit, key.in_session(7)), None);
        assert_eq!(rt(cfg, key.in_session(8)), Some(x(6)));
        assert_eq!(key.in_session(8).rates, None);
    }

    #[test]
    fn every_stream_start_on_either_sink_starts_a_session() {
        let mut s = PlaybackSessions::default();
        assert_eq!(s.observe(false, false), 0);
        assert_eq!(s.observe(true, false), 1);
        assert_eq!(s.observe(true, false), 1);
        // Direct starts while the DSP sink keeps streaming: a new session
        // (the OR of both flags never changed).
        assert_eq!(s.observe(true, true), 2);
        assert_eq!(s.observe(false, true), 2);
        assert_eq!(s.observe(false, false), 2);
        assert_eq!(s.observe(false, true), 3);
        assert_eq!(s.current(), 3);
        // The probe outlives a session; refusals do not.
        let caps = caps_of(&[48_000]);
        let rates = DeviceRates::playable(Some(&caps), &[]);
        let open = audivane_io_alsa::RatioOpen {
            ratio: RateRatio::ONE,
            refused: vec![RateRatio::integer(2)],
            transient: vec![RateRatio::integer(3)],
        };
        let cfg = DesiredDspConfig::default();
        let key = negotiated_key(cfg, SinkChoice::Dsp, 48_000, rates, &open).with_session(3);
        assert_eq!(key.in_session(3), key);
        let later = key.in_session(4);
        assert_eq!(later.rates, rates);
        assert!(later.skipped(cfg, 48_000).as_slice().is_empty());
        assert!(!key.skipped(cfg, 48_000).as_slice().is_empty());
    }

    #[test]
    fn a_rung_only_in_another_format_does_not_reopen() {
        // 768k only in S16 on an S32 config: turning A/B off makes 2560/147
        // buildable, but the open plan still lands on x1.
        use SampleFormat::{S16, S32};
        let caps = caps_with(&[(44_100, &[S32]), (768_000, &[S16])]);
        let no_rational = RatePairMask::from_fn(|i, o| o % i == 0);
        let ab = DesiredDspConfig {
            buildable: no_rational,
            ..any_family(768_000)
        };
        let single = DesiredDspConfig {
            buildable: RatePairMask::ALL,
            generation: 1,
            ..ab
        };
        let rates = DeviceRates::playable(Some(&caps), &[S32]);
        assert_eq!(rates.unwrap().iter().collect::<Vec<_>>(), [44_100]);
        let key = negotiated_key(
            ab,
            SinkChoice::Dsp,
            44_100,
            rates,
            &audivane_io_alsa::RatioOpen::at(RateRatio::ONE),
        );
        let ladder = single.ratio_ladder_for(SinkChoice::Dsp, 44_100, Some(&caps), &[S32]);
        let plan = audivane_io_alsa::plan_open(&caps, 44_100, &ladder, &[S32]).unwrap();
        assert_eq!(plan.0, RateRatio::ONE);
        let rt = decide_rt_reopen(
            single,
            Some(key),
            SinkChoice::Dsp,
            SinkChoice::Dsp,
            44_100,
            44_100,
            RateRatio::ONE,
        );
        assert_eq!(rt, None);
        // With S16 configured the same edit does reopen, at 768k.
        let rates16 = DeviceRates::playable(Some(&caps), &[S16, S32]);
        let key16 = negotiated_key(
            ab,
            SinkChoice::Dsp,
            44_100,
            rates16,
            &audivane_io_alsa::RatioOpen::at(RateRatio::ONE),
        );
        let rt = decide_rt_reopen(
            single,
            Some(key16),
            SinkChoice::Dsp,
            SinkChoice::Dsp,
            44_100,
            44_100,
            RateRatio::ONE,
        );
        assert_eq!(rt, Some(RateRatio::new(2560, 147)));
    }

    /// Opens like the driver with a probe (S32): ALSA's plan, and its key.
    fn open(
        cfg: DesiredDspConfig,
        sink: SinkChoice,
        graph: u32,
        caps: &DeviceCapabilities,
    ) -> (RateRatio, NegotiatedKey) {
        let formats = [SampleFormat::S32];
        let ladder = cfg.ratio_ladder_for(sink, graph, Some(caps), &formats);
        let actual = audivane_io_alsa::plan_open(caps, graph, &ladder, &formats)
            .unwrap()
            .0;
        let rates = DeviceRates::playable(Some(caps), &formats);
        let open = audivane_io_alsa::RatioOpen::at(actual);
        (actual, negotiated_key(cfg, sink, graph, rates, &open))
    }

    /// Where the RT thread ends after `cfg` (on `sink`) replaces what was
    /// opened as `(active_sink, active, key)`: reopens until it stays.
    fn settle(
        cfg: DesiredDspConfig,
        sink: SinkChoice,
        graph: u32,
        caps: &DeviceCapabilities,
        (mut active_sink, mut active, mut key): (SinkChoice, RateRatio, NegotiatedKey),
    ) -> (SinkChoice, RateRatio, NegotiatedKey) {
        for _ in 0..4 {
            let rt = decide_rt_reopen(cfg, Some(key), active_sink, sink, graph, graph, active);
            if rt.is_none() {
                return (sink, active, key);
            }
            (active, key) = open(cfg, sink, graph, caps);
            active_sink = sink;
        }
        panic!("reopens forever: {cfg:?} {sink:?} {graph}");
    }

    #[test]
    fn dsp_off_at_a_cross_family_rate_reopens_at_unity() {
        // Source 44.1k, a {44.1k, 48k} DAC, any family up to 48k: the open
        // picks 48k (160/147), which bypass cannot play.
        for (dev, graph, cap, picked) in [
            (
                &[44_100, 48_000][..],
                44_100,
                48_000,
                RateRatio::new(160, 147),
            ),
            (
                &[176_400, 192_000][..],
                176_400,
                192_000,
                RateRatio::new(160, 147),
            ),
        ] {
            let caps = caps_of(dev);
            let on = any_family(cap);
            let off = DesiredDspConfig {
                dsp_enabled: false,
                generation: 2,
                ..on
            };
            let (active, key) = open(on, SinkChoice::Dsp, graph, &caps);
            assert_eq!(active, picked);
            // DSP off: reopen at x1.
            assert_eq!(
                decide_rt_reopen(
                    off,
                    Some(key),
                    SinkChoice::Dsp,
                    SinkChoice::Dsp,
                    graph,
                    graph,
                    active
                ),
                Some(RateRatio::ONE)
            );
            let state = settle(
                off,
                SinkChoice::Dsp,
                graph,
                &caps,
                (SinkChoice::Dsp, active, key),
            );
            assert_eq!(state.1, RateRatio::ONE);
            // And back on: the fresh-open pick again.
            let on2 = DesiredDspConfig {
                generation: 3,
                ..on
            };
            assert_eq!(settle(on2, SinkChoice::Dsp, graph, &caps, state).1, picked);
            // The Direct sink never stays at a fractional ratio either.
            let state = settle(
                on,
                SinkChoice::Direct,
                graph,
                &caps,
                (SinkChoice::Dsp, active, key),
            );
            assert_eq!(state.1, RateRatio::ONE);
        }
    }

    #[test]
    fn every_history_ends_at_the_fresh_open_pick() {
        let caps = caps_of(&[44_100, 48_000]);
        let on = any_family(48_000);
        let fresh = open(on, SinkChoice::Dsp, 44_100, &caps).0;
        assert_eq!(fresh, RateRatio::new(160, 147));
        // Opened for Direct, then the app moves to DSP.
        let direct = open(on, SinkChoice::Direct, 44_100, &caps);
        assert_eq!(direct.0, RateRatio::ONE);
        let state = settle(
            on,
            SinkChoice::Dsp,
            44_100,
            &caps,
            (SinkChoice::Direct, direct.0, direct.1),
        );
        assert_eq!(state.1, fresh);
        // Started with DSP off, then turned on.
        let off = DesiredDspConfig {
            dsp_enabled: false,
            ..on
        };
        let started = open(off, SinkChoice::Dsp, 44_100, &caps);
        assert_eq!(started.0, RateRatio::ONE);
        let on2 = DesiredDspConfig {
            generation: 1,
            ..on
        };
        let state = settle(
            on2,
            SinkChoice::Dsp,
            44_100,
            &caps,
            (SinkChoice::Dsp, started.0, started.1),
        );
        assert_eq!(state.1, fresh);
        // A device without the first entry stays where the plan put it: a
        // 48k-only DAC asked for x4 plays x1, and an edit does not reopen.
        let only48 = caps_of(&[48_000]);
        let x4 = DesiredDspConfig {
            rate_target: RateTarget::Ratio(4),
            ..DesiredDspConfig::default()
        };
        let (active, key) = open(x4, SinkChoice::Dsp, 48_000, &only48);
        assert_eq!(active, RateRatio::ONE);
        let edit = DesiredDspConfig {
            generation: 5,
            ..x4
        };
        assert_eq!(
            decide_rt_reopen(
                edit,
                Some(key),
                SinkChoice::Dsp,
                SinkChoice::Dsp,
                48_000,
                48_000,
                active
            ),
            None
        );
    }
}
