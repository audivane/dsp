//! Real-time side: owns the DAC (ALSA hw), the two PipeWire driver sinks
//! ("Audivane DSP", "Audivane Direct") and the DSP chains.

pub mod bypass_design;
pub mod clocks;
pub mod failsafe;
pub mod follower;
pub mod harmonics_live;
pub mod ir;
pub mod measure;
pub mod processor;
pub mod spectrum;
pub mod tee;
pub mod varispeed;
pub mod volume;
pub mod waiting;
pub(crate) mod workers;

use crate::api::{Engine, EngineError, EngineSnapshot};
use audivane_dsp_core::ab_level::{AbGains, AbLevel};
use audivane_dsp_core::bypass::{hash_i32, i32_to_f32};
use audivane_dsp_core::chain::{AbChain, Chain, ChainSpec, Side};
use audivane_io_alsa::mixer::{playback_channels, playback_level, playback_switch};
use audivane_io_alsa::{
    DEFAULT_RESERVE_PRIORITY, MixerApplyOutcome, PlaybackChannel, apply_playback_volume,
};
use audivane_io_pw::lifecycle::{NegotiatedKey, Rungs};
use audivane_io_pw::{
    DriverWaker, EngineDriverConfig, EngineDriverHandle, NegotiatedChain, RatioNegotiator,
    ReleaseKind, SinkVolumePort, SinkVolumeRequest, SinkVolumeView, scaled_quantum,
};
use audivane_protocol::ipc::{
    AbSide, AbStatus, BALANCE_MAX_DB, BypassResampler, BypassResamplerStatus, EngineConfig,
    EngineState, MAX_QUANTUM, MIN_QUANTUM, Meters, OutputsStatus, RpcError, SelfTestResult,
    SinkChoice, SinkVolumePolicy, Spectrum, VOLUME_MAX_DB, VOLUME_MIN_DB, VolumeMode, VolumeStatus,
};
use audivane_protocol::{
    ChainConfig, RateRatio, RateTarget, SampleFormat, UpsamplerConfig, effective_dither_bits,
    resolve_rate_ratio,
};
use ir::IrCache;
use processor::{
    ActiveChain, DaemonProcessor, MeterBank, PendingSwap, RtDspCmd, SelfTestJob, SharedVolume,
    VolumeTarget,
};
use spectrum::SpectrumWorker;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub const DEFAULT_CHANNELS: usize = 2;
const DEFAULT_BUFFER_PERIODS: u32 = 3;
const DEFAULT_RT_PRIORITY: u32 = 20;
/// Largest upsampling ratio the engine builds. [`engine_ratio`] is the one
/// place a target becomes the ratio the engine asks for; config validation
/// uses it too, so a change of limit (or rational ratios) lands here.
pub const MAX_DSP_RATIO: usize = audivane_protocol::MAX_INTEGER_RATIO as usize;

/// The ratio the engine asks the output for: the target resolved at
/// `rate`, an integer ratio clamped to [`MAX_DSP_RATIO`] (a rational one
/// as resolved). The device may then open a lower ratio from
/// `audivane_io_alsa::ratio_ladder` of this one.
pub fn engine_ratio(target: RateTarget, rate: u32) -> RateRatio {
    let r = audivane_protocol::resolve_rate_ratio(target, rate.max(8_000));
    match r.as_integer() {
        Some(k) => RateRatio::integer(k.clamp(1, MAX_DSP_RATIO as u32)),
        None => r,
    }
}

type MixerFn = Arc<volume::MixerFn>;

/// `audivane-dsp --volume-failsafe`: with the engine not running, bring
/// the card down to the heard level of `cfg` (see
/// [`volume::failsafe_write`]). Returns the level left, if it wrote one.
/// Nothing is written when the engine had let go of the DAC (another
/// program may hold it; the card was lowered when it was let go).
pub fn leave_mixer_safe(cfg: &EngineConfig) -> Result<Option<f64>, String> {
    if let Err(why) = failsafe::crash_failsafe_may_write(failsafe::released_path().as_deref()) {
        log::info!("volume failsafe: nothing to do: {why}");
        return Ok(None);
    }
    let error = std::cell::RefCell::new(None);
    let mixer = |device: &str, db: f64, mute: Option<bool>| {
        apply_playback_volume(device, db, mute).unwrap_or_else(|e| {
            *error.borrow_mut() = Some(format!("ALSA mixer on {device:?}: {e}"));
            MixerApplyOutcome::digital(db)
        })
    };
    let switch_on = playback_switch(&cfg.device).ok().flatten();
    // The failsafe only ever lowers the card, so it needs the card's level
    // (its quietest channel): without it nothing is written. A card with no
    // usable volume control (`Ok(None)`) has nothing to protect and does not
    // warn.
    let card_level = match playback_level(&cfg.device) {
        Ok(Some(db)) => volume::CardLevel::Known(db),
        Ok(None) => volume::CardLevel::NoControl,
        Err(_) => volume::CardLevel::Unknown,
    };
    if volume::failsafe_blocked_by_unknown_level(&cfg.volume, card_level) {
        log::warn!(
            "volume failsafe: cannot read the level of {}; left as it is",
            cfg.device
        );
    }
    let mut store = failsafe::MuteStore::load(failsafe::mutes_path());
    let left = volume::leave_safe(
        &cfg.volume,
        &cfg.device,
        &mixer,
        card_level.db(),
        switch_on,
        &mut |mute| match mute {
            Some(db) => store.record(&cfg.device, db, None),
            None => store.forget(&cfg.device),
        },
    );
    // The mute took: record how every channel was left, so a later open
    // can tell an untouched card from one changed since.
    if let Some((db, _, true)) = left {
        store.record(
            &cfg.device,
            db,
            playback_channels(&cfg.device).ok().flatten(),
        );
    }
    if let Some(e) = error.into_inner() {
        return Err(e);
    }
    Ok(left.map(|(_, outcome, _)| outcome.volume_db))
}

/// The processor's volume before the first negotiation: the whole stored
/// level as digital gain (the card's mixer is not known yet). The quietest
/// safe assumption: once a hardware mixer takes the level, the digital part
/// only goes up while the card goes down to it.
fn initial_volume_target(cfg: &EngineConfig) -> VolumeTarget {
    VolumeTarget {
        volume_db: volume::target_db(&cfg.volume),
        balance_db: cfg.volume.balance_db,
        muted: cfg.volume.muted,
        hardware_volume: false,
        hardware_mute: false,
    }
}

fn apply_alsa_mixer(device: &str, volume_db: f64, muted: Option<bool>) -> MixerApplyOutcome {
    match apply_playback_volume(device, volume_db, muted) {
        Ok(outcome) => outcome,
        Err(e) => {
            log::warn!("ALSA mixer apply failed on {device:?}: {e}; using digital volume fallback");
            MixerApplyOutcome::digital(volume_db)
        }
    }
}

struct PendingPublication {
    swap: PendingSwap,
    built_generation: u64,
    ab_gains: Option<AbGains>,
    has_effect: bool,
    ab_enabled: bool,
    /// Hold pushing `swap` to the RT thread until the varispeed stage has
    /// finished fading out and retired, because this chain only fits the
    /// static budget with varispeed off; at most until this deadline (the
    /// fade takes milliseconds), so a retirement that never completes
    /// cannot keep an accepted config from playing.
    wait_varispeed_retire: Option<Instant>,
}

/// The longest a chain waits for varispeed's retirement before it is
/// published anyway (the overload fallback covers a brief overlap).
const VARISPEED_RETIRE_WAIT: Duration = Duration::from_secs(1);

/// Extra wait after the output latency before a staged hardware raise.
const HW_RAISE_MARGIN: Duration = Duration::from_millis(50);
/// Output latency assumed while the driver cannot say.
const DEFAULT_OUTPUT_LATENCY: Duration = Duration::from_secs(1);

type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;
/// Reads a card's playback switch (see [`playback_switch`]).
type SwitchStateFn = Arc<dyn Fn(&str) -> Option<bool> + Send + Sync>;
/// Result of reading a card's playback channels (see [`playback_channels`]).
#[derive(Clone, Debug, PartialEq)]
enum CardRead {
    /// The card has an eligible playback volume control; its channels.
    Channels(Vec<PlaybackChannel>),
    /// The card has no usable playback volume control.
    NoControl,
    /// Opening the mixer or reading the control failed.
    Error,
}

impl CardRead {
    fn from_alsa(res: audivane_io_alsa::Result<Option<Vec<PlaybackChannel>>>) -> Self {
        match res {
            Ok(Some(ch)) => Self::Channels(ch),
            Ok(None) => Self::NoControl,
            Err(_) => Self::Error,
        }
    }

    fn channels(self) -> Option<Vec<PlaybackChannel>> {
        match self {
            Self::Channels(ch) => Some(ch),
            Self::NoControl | Self::Error => None,
        }
    }
}

/// Reads every playback channel of a card (see [`playback_channels`]).
type CardStateFn = Arc<dyn Fn(&str) -> CardRead + Send + Sync>;

#[derive(Clone, Copy, Debug)]
struct KnownHardware {
    requested: f64,
    mute: Option<bool>,
    outcome: MixerApplyOutcome,
}

#[derive(Clone, Copy, Debug)]
struct PendingHardware {
    write: volume::HardwareWrite,
    due: Instant,
}

struct BuilderState {
    cfg: EngineConfig,
    ir_cache: IrCache,
    ab_side: AbSide,
    ab_gains: Option<AbGains>,
    last_rate: u32,
    last_ratio: RateRatio,
    /// The ratio of the latest DSP chain built (published or still
    /// deferred), at `last_rate`: what edits build for, so an edit that
    /// supersedes a chain a resume waits for fits the same gate.
    last_dsp_ratio: RateRatio,
    /// The latest chain built is the engine bypass resampler's
    /// ([`ActiveChain::Resample`]), not a DSP chain.
    last_built_resample: bool,
    /// Format ALSA negotiated on the last open: caps the dither depth.
    out_format: Option<SampleFormat>,
    /// What the last open found (the driver's key for it): where a switch
    /// to the DSP sink would land, for the chain Direct keeps warm.
    negotiated_key: Option<NegotiatedKey>,
    config_generation: u64,
    built_generation: u64,
    next_swap_seq: u64,
    /// Packed `(active_generation << 1) | u64::from(in_flight)` from the RT
    /// processor: lets us observe promoted generation and swap-phase state
    /// atomically in one load.
    swap_state: Arc<AtomicU64>,
    /// Configs of outgoing/intermediate chains paired with the generation that
    /// replaced them, kept until `swap_state` proves they are retired on RT.
    in_flight_cfgs: Vec<(u64, EngineConfig)>,
    /// Shared handle to the RT varispeed stage's applied state (used to gate
    /// over-budget chain swaps on stage retirement).
    varispeed_applied: Arc<varispeed::VarispeedApplied>,
    pending_publication: Option<PendingPublication>,
    has_pending_swap: Arc<AtomicBool>,
    shared_volume: Arc<SharedVolume>,
    mixer: MixerFn,
    /// Digital master gain currently applied on the DSP path, dB.
    software_db: f64,
    /// The last hardware mixer write (any mode) since the device opened,
    /// and the device it went to.
    hw_known: Option<KnownHardware>,
    hw_known_device: Option<String>,
    /// A hardware raise waiting for the audio rendered at the old digital
    /// gain to play out (see [`BuilderState::sync_volume`]).
    pending_hw: Option<PendingHardware>,
    /// Audio already rendered but not yet heard: ALSA delay plus filter
    /// delay.
    output_latency: Arc<dyn Fn() -> Duration + Send + Sync>,
    /// The loudest digital gain (dB) of audio that may still be buffered,
    /// and until when: set whenever the digital part goes down.
    digital_peak: Option<(f64, Instant)>,
    /// Switches the failsafe muted, per card (persisted; see
    /// [`failsafe::mutes_path`]).
    failsafe_mutes: failsafe::MuteStore,
    /// The playback switch before the failsafe writes (`None`: unknown or
    /// none), so only a mute we cause is recorded.
    switch_state: SwitchStateFn,
    /// Every playback channel of the card (level and switch): the level the
    /// failsafe lowers from, and whether a recorded failsafe mute is still
    /// as it was left.
    card_state: CardStateFn,
    /// The recorded failsafe mute was found as it was left, in this open
    /// (see [`BuilderState::failsafe_mute_is_ours`]).
    failsafe_mute_verified: bool,
    /// Where "the DAC is not held" is recorded for `--volume-failsafe`
    /// (see [`failsafe::released_path`]); `None` in tests.
    released_marker: Option<std::path::PathBuf>,
    /// The driver's state now (an RT failure shows here before the next
    /// apply): a staged raise is dropped while it says `Error`.
    driver_state: Arc<dyn Fn() -> EngineState + Send + Sync>,
    /// The driver reported `Error` at the last apply: it may have let go of
    /// the card without telling us (a failed reconnect), so the mixer is
    /// not written until the next negotiation.
    driver_error: bool,
    /// The DAC format's bit depth, for the processor's Direct / bypass
    /// dither.
    dac_bits: Arc<AtomicU32>,
    /// The engine holds the DAC (set on every negotiation, cleared when it
    /// lets go). No mixer writes while it does not.
    dac_held: bool,
    clock: Clock,
    /// Shows the effective volume on our sinks (`sink_volume: follow`); set
    /// once the driver runs.
    sink_port: Option<SinkVolumePort>,
    cmd_tx: rtrb::Producer<RtDspCmd>,
    swap_tx: rtrb::Producer<PendingSwap>,
    /// The engine bypass resampler's designs, checked off this lock
    /// ([`bypass_design`]).
    designer: bypass_design::BypassDesigner<ActiveChain>,
    /// An edit asked for the resample-only chain while its design ran
    /// (the previous chain keeps playing): built when the design is done
    /// ([`Self::commit_designed`]), if nothing moved on meanwhile.
    pending_resample: Option<PendingResample>,
    /// The pair a negotiation fell back to PipeWire for while its chain is
    /// built (status says so until it is done).
    designing: Option<bypass_design::DesignKey>,
    /// Tells the driver about the designs ([`audivane_io_pw::BypassNote`]):
    /// a pair's design is done (a pair that fell back to PipeWire comes
    /// back, or a failed one stays out), or the design epoch moved on (the
    /// pairs it holds back are released). Set once the driver runs.
    bypass_notify: Option<BypassNotify>,
    /// Counts negotiations: a request an edit made is only served in the
    /// negotiation it was made in.
    negotiation_seq: u64,
    /// Bumped by an edit of what the designs depend on (device, the output
    /// section as the negotiated format caps it, quantum, bypass mode): a
    /// design requested before it no longer speaks for the driver's pairs,
    /// and the driver releases the pairs it held for one
    /// ([`audivane_io_pw::BypassHeld::retire`]).
    design_epoch: u64,
    /// The designs the driver waits for (a negotiation fell back for them),
    /// with the epoch they were asked for in.
    design_requests: Vec<(bypass_design::DesignKey, u64)>,
    /// Builds and retires the varispeed stage for the running graph rate.
    varispeed: varispeed::VarispeedHost,
    /// The last requested speed, and requests so far (the overload
    /// fallback ends a varispeed step on a new one).
    varispeed_speed: f64,
    varispeed_requests: u64,
    /// The overload fallback's hold on varispeed, and the sequence the
    /// driver gave it.
    varispeed_hold: audivane_io_pw::varispeed_rt::VarispeedHold,
    varispeed_hold_seq: u64,
    /// Multi-device: the followers' per-device slots (chain config, tee
    /// ring) and the processor's tee slot they drive.
    tee: tee::TeeHost,
    /// The followers' threads (added through [`RealEngine::add_follower`], not
    /// in the protocol yet), and the master's alignment applied for them, ms.
    followers: follower::FollowerHost,
    align: follower::AlignControl,
    /// The master's chain compensation (`TeeShared::comp_frames`).
    comp: follower::CompControl,
    align_t0: Instant,
    /// The per-rate upsampler plan ([`crate::api::RatePlan`]), and the
    /// upsampler it put in the latest DSP chain built (`None`: the config's
    /// own): an edit rebuilds when that moves.
    /// Per-rate upsampler substitutes from CPU admission; shared with the
    /// followers' chain builder so followers respect the same plan.
    rate_plan: Arc<Mutex<crate::api::RatePlan>>,
    built_substitute: Option<UpsamplerConfig>,
}

/// [`BuilderState::bypass_notify`].
type BypassNotify = Arc<dyn Fn(audivane_io_pw::BypassNote) + Send + Sync>;

/// The resample-only chain an edit waits for ([`BuilderState::pending_resample`]).
#[derive(Clone, Debug)]
struct PendingResample {
    key: bypass_design::DesignKey,
    /// The config generation it serves (carried over edits that leave the
    /// key alone, such as a volume change).
    generation: u64,
    /// The negotiation it was made in ([`BuilderState::negotiation_seq`]).
    negotiation: u64,
}

/// Whether something processes the samples ([`EngineSnapshot::dsp_active`]):
/// a digital volume part on any path, the engine bypass resampler while it
/// converts (`converting`; its chain resamples and dithers, at unity volume
/// too), or the DSP chain's effect on the DSP sink.
fn dsp_active(
    state: EngineState,
    digital: bool,
    converting: bool,
    sink: Option<SinkChoice>,
    dsp_has_effect: bool,
) -> bool {
    state != EngineState::Idle
        && (digital || converting || (sink != Some(SinkChoice::Direct) && dsp_has_effect))
}

/// What the engine bypass resampler's designer builds for a pair: the
/// resample-only chain (its exact design check first).
fn bypass_chain_builder() -> bypass_design::DesignBuild<ActiveChain> {
    Arc::new(|k: &bypass_design::DesignKey| {
        build_resample_chain_for(k.output, k.quantum, k.input, k.ratio).map_err(|e| e.message)
    })
}

impl BuilderState {
    /// Apply the master volume: hardware mixer and digital gain.
    ///
    /// Digital gain reaches samples rendered from now on, but up to one
    /// output latency of audio (ALSA buffer plus filter delay) was rendered
    /// at the old one; a hardware change is heard at once. So a hardware
    /// raise while the digital part goes down applies the digital part first
    /// and waits for that audio to play out ([`Self::run_pending_hw`]); any
    /// other order is never louder than the louder of the two settings.
    ///
    /// "The old gain" is the loudest still in the buffer
    /// ([`Self::digital_in_flight`]), not the last one set, so a second
    /// change inside the window cannot write the hardware early either.
    ///
    /// While the DAC is not held the mixer is not touched; the next
    /// negotiation writes it.
    fn sync_volume(&mut self, mirror: &SharedMirror, waker: Option<&DriverWaker>) {
        let previous = self.pending_hw.take();
        let vol = self.cfg.volume;
        let plan = volume::plan(&vol);
        let known = self
            .hw_known
            .filter(|_| self.hw_known_device.as_deref() == Some(self.cfg.device.as_str()))
            .map(|k| k.outcome);
        // Where an earlier mode left the hardware, for `software` without a
        // fixed level: only a card that has a control is at a level.
        let left_at = known.filter(|o| o.hardware_volume).map(|o| o.volume_db);
        if !self.dac_held || self.driver_error {
            self.pending_hw = None;
            let applied = volume::resolve(&vol, &plan, known);
            self.publish_volume(applied, left_at, mirror, waker);
            return;
        }
        let Some(write) = plan.hardware else {
            // `software` without a fixed level never writes the card, but a
            // switch our failsafe muted is turned back on while it is still
            // as the hand-over left it (at that level: no level change).
            if self.failsafe_mute_is_ours()
                && let Some(m) = self.failsafe_mutes.get(&self.cfg.device).cloned()
            {
                // The quietest channel as left (all of them are, after our
                // write): never a raise.
                let level = m
                    .left
                    .iter()
                    .flatten()
                    .map(|c| c.level_db)
                    .reduce(f64::min)
                    .unwrap_or(m.level_db);
                if (self.mixer)(&self.cfg.device, level, Some(false)).hardware_mute {
                    self.failsafe_mutes.forget(&self.cfg.device);
                }
            }
            let applied = volume::resolve(&vol, &plan, None);
            self.publish_volume(applied, left_at, mirror, waker);
            return;
        };
        // `software` sets its fixed level once per open, not on every volume
        // change (the user may trim it with alsamixer); any other write in
        // between (another mode) makes it write again.
        if vol.mode == VolumeMode::Software
            && let Some(k) = self.hw_known.filter(|_| known.is_some())
            && k.requested == write.db
            && k.mute == write.mute
        {
            let applied = volume::resolve(&vol, &plan, Some(k.outcome));
            self.publish_volume(applied, None, mirror, waker);
            return;
        }
        if let Some(current) = known
            && write.db > current.volume_db + volume::EPS_DB
        {
            // What the hardware will be at (the mixer rounds down), and the
            // switch not yet carrying the mute.
            let predicted = MixerApplyOutcome {
                volume_db: write.db.clamp(current.min_db, current.max_db),
                hardware_mute: false,
                ..current
            };
            let staged = volume::resolve(&vol, &plan, Some(predicted));
            let now = (self.clock)();
            if staged.software_db < self.digital_in_flight(now) - volume::EPS_DB {
                let mut due = now + (self.output_latency)() + HW_RAISE_MARGIN;
                if let Some(p) = previous {
                    due = due.max(p.due);
                }
                if let Some((_, until)) = self.digital_peak {
                    due = due.max(until);
                }
                log::debug!(
                    "volume: digital {:.2} dB now, hardware {:.2} -> {:.2} dB after the buffered audio",
                    staged.software_db,
                    current.volume_db,
                    predicted.volume_db
                );
                // Until the write, the hardware is still at its old level.
                let mut now = staged;
                now.volume_db = (current.volume_db + staged.software_db).min(staged.volume_db);
                now.hardware_db = Some(current.volume_db);
                self.publish_volume(now, None, mirror, waker);
                self.pending_hw = Some(PendingHardware { write, due });
                return;
            }
        }
        self.write_hardware(write, mirror, waker);
    }

    /// Whether the failsafe mute recorded for this card may be undone by a
    /// write that does not drive the switch (`software`). Only while every
    /// playback channel is exactly as it was left is the mute still ours;
    /// otherwise another player or the user changed it, or the state is
    /// unknown, and the switches are theirs: the record is dropped and the
    /// card left as it is, with a warning. Checked once per open, since our
    /// own writes in it change the level; a card changed and put back to the
    /// very same state cannot be told apart from an untouched one.
    fn failsafe_mute_is_ours(&mut self) -> bool {
        let device = self.cfg.device.clone();
        let Some(m) = self.failsafe_mutes.get(&device).cloned() else {
            return false;
        };
        if self.failsafe_mute_verified {
            return true;
        }
        let now = (self.card_state)(&device).channels();
        let as_left = matches!((&m.left, &now),
            (Some(left), Some(now)) if failsafe::same_channels(left, now));
        if as_left {
            self.failsafe_mute_verified = true;
            return true;
        }
        log::warn!(
            "volume: {device} was changed since its switch was muted at the hand-over \
             (now {now:?}, left {:?}); leaving the switch as it is",
            m.left
        );
        self.failsafe_mutes.forget(&device);
        false
    }

    /// The loudest digital gain that may still be in the buffer at `now`.
    fn digital_in_flight(&self, now: Instant) -> f64 {
        match self.digital_peak {
            Some((db, until)) if now < until => db.max(self.software_db),
            _ => self.software_db,
        }
    }

    /// Write the hardware now and publish what is in force.
    fn write_hardware(
        &mut self,
        write: volume::HardwareWrite,
        mirror: &SharedMirror,
        waker: Option<&DriverWaker>,
    ) {
        let vol = self.cfg.volume;
        let plan = volume::plan(&vol);
        let mut write = write;
        // A write that leaves the switch alone (`software`) undoes our
        // mute only while it is still ours; one that drives the switch
        // (the mute in `auto`/`hardware`/`hybrid`) sets it anyway.
        let ours = if write.mute.is_none() {
            self.failsafe_mute_is_ours()
        } else {
            self.failsafe_mutes.get(&self.cfg.device).is_some()
        };
        if ours && write.mute.is_none() {
            write.mute = Some(false);
        }
        let outcome = (self.mixer)(&self.cfg.device, write.db, write.mute);
        // Our mute is undone only by a write that actually set the switch
        // on; otherwise it stays recorded for the next try, at the level
        // this write left (the card is still as we left it).
        if ours && write.mute == Some(false) {
            if outcome.hardware_mute {
                self.failsafe_mutes.forget(&self.cfg.device);
            } else if let Some(m) = self.failsafe_mutes.get(&self.cfg.device).cloned() {
                let left = (self.card_state)(&self.cfg.device).channels();
                self.failsafe_mutes
                    .record(&self.cfg.device, m.level_db, left);
            }
        }
        self.hw_known_device = Some(self.cfg.device.clone());
        self.hw_known = Some(KnownHardware {
            requested: write.db,
            mute: write.mute,
            outcome,
        });
        let applied = volume::resolve(&vol, &plan, Some(outcome));
        self.publish_volume(applied, None, mirror, waker);
    }

    /// The driver let go of `device`. Only the card the engine holds is its
    /// business: a late release of another one (an old card whose reopen was
    /// superseded by a device change) must neither lower nor forget the card
    /// configured now.
    fn on_release_of(&mut self, kind: ReleaseKind, device: &str) {
        if device != self.cfg.device {
            log::debug!(
                "volume: release of {device} ignored (the engine is on {})",
                self.cfg.device
            );
            return;
        }
        self.on_release(kind);
    }

    /// The driver let go of the DAC (see [`ReleaseKind`]): bring the card
    /// down first when it is still ours, and stop writing it.
    fn on_release(&mut self, kind: ReleaseKind) {
        match kind {
            ReleaseKind::HandOver => self.leave_hardware_safe(),
            ReleaseKind::Lost => {
                self.pending_hw = None;
                self.dac_held = false;
            }
        }
        failsafe::note_released(self.released_marker.as_deref(), true);
    }

    /// Bring the card down to the heard level before letting go of the DAC
    /// (see [`volume::failsafe_write`]). Only while the DAC is held: once
    /// let go another program may own it, and the card was lowered then.
    fn leave_hardware_safe(&mut self) {
        self.pending_hw = None;
        if !std::mem::replace(&mut self.dac_held, false) {
            return;
        }
        // The card's level, on its quietest channel (auto/hardware: at its
        // floor above the volume?): read now, else our last write's
        // readback. Unknown, nothing is written (it could be a raise); when
        // the card has no usable volume control, there is nothing to protect
        // and no warning to emit.
        let known = self
            .hw_known
            .filter(|_| self.hw_known_device.as_deref() == Some(self.cfg.device.as_str()));
        let fallback = || {
            known
                .filter(|k| k.outcome.hardware_volume)
                .map_or(volume::CardLevel::Unknown, |k| {
                    volume::CardLevel::Known(k.outcome.volume_db)
                })
        };
        let card_level = match (self.card_state)(&self.cfg.device) {
            CardRead::Channels(ch) => match ch.iter().map(|c| c.level_db).reduce(f64::min) {
                Some(db) => volume::CardLevel::Known(db),
                None => fallback(),
            },
            CardRead::NoControl => volume::CardLevel::NoControl,
            CardRead::Error => fallback(),
        };
        if volume::failsafe_blocked_by_unknown_level(&self.cfg.volume, card_level) {
            log::warn!(
                "volume: cannot read the level of {}; left as it is for whoever opens it next",
                self.cfg.device
            );
        }
        let card_db = card_level.db();
        let Some(write) = volume::failsafe_write(&self.cfg.volume, card_db) else {
            return;
        };
        // Shutdown after a release (or twice, from Drop): already there.
        if !volume::plan(&self.cfg.volume).hardware_is_volume
            && known.is_some_and(|k| k.requested == write.db)
        {
            return;
        }
        let mixer = Arc::clone(&self.mixer);
        let switch_on = (self.switch_state)(&self.cfg.device);
        let store = &mut self.failsafe_mutes;
        let device = self.cfg.device.as_str();
        let Some((db, outcome, muted)) = volume::leave_safe(
            &self.cfg.volume,
            device,
            &*mixer,
            card_db,
            switch_on,
            &mut |mute| match mute {
                Some(db) => store.record(device, db, None),
                None => store.forget(device),
            },
        ) else {
            return;
        };
        // The mute took: record how every channel was left (see
        // [`Self::failsafe_mute_is_ours`]).
        if muted {
            let left = (self.card_state)(device).channels();
            self.failsafe_mutes.record(device, db, left);
        }
        log::info!(
            "volume: hardware left at {:.2} dB{} for whoever opens {} next",
            outcome.volume_db,
            if muted {
                ", switch muted (below the card's range)"
            } else {
                ""
            },
            self.cfg.device
        );
        self.hw_known_device = Some(self.cfg.device.clone());
        self.hw_known = Some(KnownHardware {
            requested: db,
            mute: muted.then_some(true),
            outcome,
        });
    }

    /// Perform a staged hardware raise once it is due. Returns whether one
    /// is still pending.
    fn run_pending_hw(
        &mut self,
        now: Instant,
        mirror: &SharedMirror,
        waker: Option<&DriverWaker>,
    ) -> bool {
        let Some(pending) = self.pending_hw else {
            return false;
        };
        if !self.dac_held || self.driver_error || (self.driver_state)() == EngineState::Error {
            self.pending_hw = None;
            return false;
        }
        if now < pending.due {
            return true;
        }
        self.pending_hw = None;
        self.write_hardware(pending.write, mirror, waker);
        false
    }

    /// `left_at_db`: in `software` without a fixed level, the level an
    /// earlier mode left the hardware at (it is not touched any more).
    fn publish_volume(
        &mut self,
        mut applied: volume::Applied,
        left_at_db: Option<f64>,
        mirror: &SharedMirror,
        waker: Option<&DriverWaker>,
    ) {
        let vol = self.cfg.volume;
        if applied.hardware_unavailable {
            log::warn!(
                "volume mode {:?}: {} has no usable hardware volume control; using digital volume",
                vol.mode,
                self.cfg.device
            );
        }
        if applied.hardware_db.is_none() {
            applied.hardware_db = left_at_db;
        }
        if applied.software_db < self.software_db - volume::EPS_DB {
            // The louder audio already rendered plays out for one output
            // latency yet.
            let now = (self.clock)();
            let peak = self.digital_in_flight(now);
            let mut until = now + (self.output_latency)() + HW_RAISE_MARGIN;
            if let Some((_, old)) = self.digital_peak.filter(|&(_, u)| now < u) {
                until = until.max(old);
            }
            self.digital_peak = Some((peak, until));
        }
        self.software_db = applied.software_db;
        self.shared_volume.store(VolumeTarget {
            volume_db: applied.software_db,
            balance_db: vol.balance_db,
            muted: vol.muted,
            hardware_volume: false,
            hardware_mute: applied.hardware_mute,
        });
        // The followers follow what the master's DAC plays: its effective
        // volume (a hardware part becomes their digital gain), its balance
        // and its mute (digital on them, whatever carries it here).
        self.tee.shared().master_level.store(VolumeTarget {
            volume_db: applied.volume_db,
            balance_db: vol.balance_db,
            muted: vol.muted,
            hardware_volume: false,
            hardware_mute: false,
        });
        mirror.set_volume(VolumeStatus {
            volume_db: applied.volume_db,
            hardware: applied.hardware_volume,
            min_db: applied.min_db,
            max_db: applied.max_db,
            hardware_db: applied.hardware_db,
            software_db: applied.software_db,
            hardware_unavailable: applied.hardware_unavailable,
            ..VolumeStatus::from_config(&vol)
        });
        mirror.dsp_has_effect.store(
            dsp_has_effect(&self.cfg, self.software_db),
            Ordering::Release,
        );
        // A software mute zeros the samples as surely as a gain does.
        mirror.digital_gain.store(
            self.software_db != 0.0
                || vol.balance_db != 0.0
                || (vol.muted && !applied.hardware_mute),
            Ordering::Release,
        );
        if let Some(w) = waker {
            w.wake_rt();
        }
        self.reflect_sink_volume(applied.volume_db);
    }

    /// Show the effective volume on our sinks, or release them.
    fn reflect_sink_volume(&self, volume_db: f64) {
        if let Some(port) = &self.sink_port {
            port.set(
                self.cfg.volume.sink_volume == SinkVolumePolicy::Follow,
                sink_view(&self.cfg.volume, volume_db),
            );
        }
    }

    fn alloc_swap_seq(&mut self) -> u64 {
        self.next_swap_seq = self.next_swap_seq.saturating_add(1);
        self.next_swap_seq
    }

    fn push_cmd(
        &mut self,
        cmd: RtDspCmd,
        driver: Option<&EngineDriverHandle>,
    ) -> Result<(), EngineError> {
        let mut pending = Some(cmd);
        let deadline = Instant::now() + Duration::from_millis(200);
        loop {
            let Some(c) = pending.take() else {
                return Ok(());
            };
            match self.cmd_tx.push(c) {
                Ok(()) => {
                    if let Some(d) = driver {
                        d.wake_rt();
                    }
                    return Ok(());
                }
                Err(rtrb::PushError::Full(returned)) => {
                    if Instant::now() >= deadline {
                        return Err(EngineError::new(
                            RpcError::INTERNAL,
                            "DSP command queue full",
                        ));
                    }
                    pending = Some(returned);
                    if let Some(d) = driver {
                        d.wake_rt();
                    }
                    thread::sleep(Duration::from_millis(1));
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn commit_published(
        &mut self,
        built_generation: u64,
        ab_gains: Option<AbGains>,
        has_effect: bool,
        ab_enabled: bool,
        mirror: &SharedMirror,
    ) {
        self.built_generation = built_generation;
        self.ab_gains = ab_gains;
        mirror.dsp_has_effect.store(has_effect, Ordering::Release);
        // A/B selection is independent engine state (`self.ab_side`), never
        // publication metadata: a deferred publication must not restore the
        // side that happened to be selected when the chain was built.
        mirror.set_ab(ab_enabled, self.ab_side, ab_gains);
    }

    /// Push `pub_item` into `swap_tx`: `built_generation` and mirror
    /// metadata advance only once the queue accepts the chain, else the swap
    /// waits in `pending_publication` for the reaper.
    ///
    /// The latest A/B selection is applied just before the push (and on each
    /// retry), so a selection made while the swap waits is not undone when it
    /// is installed.
    fn publish_or_defer(
        &mut self,
        mut pub_item: PendingPublication,
        mirror: &SharedMirror,
        waker: Option<&DriverWaker>,
    ) -> bool {
        if let Some(deadline) = pub_item.wait_varispeed_retire
            && !self.varispeed_retired()
        {
            if Instant::now() < deadline {
                self.pending_publication = Some(pub_item);
                self.has_pending_swap.store(true, Ordering::Release);
                if let Some(w) = waker {
                    w.wake_rt();
                }
                return false;
            }
            log::warn!(
                "varispeed: the stage did not retire within {} ms; the new chain plays anyway",
                VARISPEED_RETIRE_WAIT.as_millis()
            );
        }
        // Retired (or waited out): a retry on a full queue does not wait again.
        pub_item.wait_varispeed_retire = None;
        pub_item.swap.chain.apply_ab_side(self.ab_side);
        match self.swap_tx.push(pub_item.swap) {
            Ok(()) => {
                self.commit_published(
                    pub_item.built_generation,
                    pub_item.ab_gains,
                    pub_item.has_effect,
                    pub_item.ab_enabled,
                    mirror,
                );
                self.pending_publication = None;
                self.has_pending_swap.store(false, Ordering::Release);
                if let Some(w) = waker {
                    w.wake_rt();
                }
                true
            }
            Err(rtrb::PushError::Full(returned)) => {
                self.pending_publication = Some(PendingPublication {
                    swap: returned,
                    ..pub_item
                });
                self.has_pending_swap.store(true, Ordering::Release);
                if let Some(w) = waker {
                    w.wake_rt();
                }
                false
            }
        }
    }

    /// Queue a freshly built chain for the RT thread, superseding any older
    /// deferred publication first. Returns the swap sequence allocated for it.
    #[allow(clippy::too_many_arguments)]
    fn publish_built(
        &mut self,
        cfg: &EngineConfig,
        chain: ActiveChain,
        build_ratio: RateRatio,
        ab_gains: Option<AbGains>,
        has_effect: bool,
        crossfade: bool,
        generation: u64,
        mirror: &SharedMirror,
        waker: Option<&DriverWaker>,
    ) -> u64 {
        self.publish_built_gated(
            cfg,
            chain,
            build_ratio,
            ab_gains,
            has_effect,
            crossfade,
            generation,
            false,
            mirror,
            waker,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn publish_built_gated(
        &mut self,
        cfg: &EngineConfig,
        chain: ActiveChain,
        build_ratio: RateRatio,
        ab_gains: Option<AbGains>,
        has_effect: bool,
        crossfade: bool,
        generation: u64,
        wait_varispeed_retire: bool,
        mirror: &SharedMirror,
        waker: Option<&DriverWaker>,
    ) -> u64 {
        // Every chain of the main output reaches the audio thread here.
        let mut chain = chain;
        workers::attach(&mut chain, cfg);
        self.pending_publication = None;
        self.has_pending_swap.store(false, Ordering::Release);
        // Recorded now, not when the RT queue accepts it: a deferred chain
        // is still the one the next edit must match.
        self.last_dsp_ratio = build_ratio;
        self.last_built_resample = chain.is_resample();
        let swap_seq = self.alloc_swap_seq();
        let _ = self.publish_or_defer(
            PendingPublication {
                swap: PendingSwap {
                    chain,
                    crossfade,
                    generation,
                    swap_seq,
                },
                built_generation: generation,
                ab_gains,
                has_effect,
                ab_enabled: cfg.ab.is_some(),
                wait_varispeed_retire: wait_varispeed_retire
                    .then(|| Instant::now() + VARISPEED_RETIRE_WAIT),
            },
            mirror,
            waker,
        );
        swap_seq
    }

    /// Retry pushing any deferred chain swap into `swap_tx`.
    fn flush_pending_swap(&mut self, mirror: &SharedMirror, waker: Option<&DriverWaker>) -> bool {
        let Some(pub_item) = self.pending_publication.take() else {
            self.has_pending_swap.store(false, Ordering::Release);
            return false;
        };
        self.publish_or_defer(pub_item, mirror, waker)
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_config(
        &mut self,
        cfg: &EngineConfig,
        mirror: &SharedMirror,
        drv_state: EngineState,
        drv_rate: u32,
        drv_sink: Option<SinkChoice>,
        driver: Option<&EngineDriverHandle>,
    ) -> Result<bool, EngineError> {
        // Build for the input rate the device was opened (negotiated) for,
        // not the graph rate observed now: during a rate change they differ
        // until the reopen, which rebuilds for the new rate anyway.
        let negotiated = !matches!(drv_state, EngineState::Idle | EngineState::Error)
            && self.out_format.is_some();
        let current_rate = if negotiated { self.last_rate } else { drv_rate }.max(8_000);
        let conv_warnings = validate_engine_config(cfg, &mut self.ir_cache, current_rate)?;

        let old_cfg = self.cfg.clone();
        let snapshot = driver.map(|d| d.snapshot());
        // What the last open found, as it holds in this playback session
        // (the RT thread predicts from the same).
        // (Without a driver, what the builder recorded of the last open.)
        let open_key = match &snapshot {
            Some(d) => d.open_key.map(|k| k.in_session(d.playback_session)),
            None => self.negotiated_key,
        };
        // What the device runs at (the DSP chain's ratio without a driver).
        let running = match &snapshot {
            Some(d) if d.state != EngineState::Idle => d.ratio,
            _ => self.last_dsp_ratio,
        };
        let force_reopen = drv_state == EngineState::Error
            || diff_requires_reopen(
                &old_cfg,
                cfg,
                current_rate,
                open_key.as_ref(),
                running,
                drv_sink,
            );
        let next_side = if cfg.ab.is_none() {
            AbSide::A
        } else {
            self.ab_side
        };
        let prev_gen = self.config_generation;
        let next_gen = prev_gen.saturating_add(1);

        // Direct or DSP off away from x1 plays the engine bypass
        // resampler's chain: an edit rebuilds it only when what it depends
        // on changed (the output section, DSP on or off).
        let resample_only = !force_reopen
            && snapshot
                .as_ref()
                .is_none_or(|d| d.state != EngineState::Idle)
            && plays_resample_only(cfg, drv_sink, running);
        let resample_kept = resample_only
            && self.last_built_resample
            && crate::control::config::bypass_output(old_cfg.chain.output)
                == crate::control::config::bypass_output(cfg.chain.output)
            && old_cfg.dsp_enabled == cfg.dsp_enabled;
        let target_ratio = if force_reopen {
            desired_ratio_for(cfg, current_rate)
        } else if resample_only {
            running
        } else if drv_sink == Some(SinkChoice::Direct) {
            warm_dsp_ratio(cfg, current_rate, open_key.as_ref())
        } else {
            self.last_dsp_ratio
        };

        let varispeed_changed = old_cfg.varispeed != cfg.varispeed;
        let mut old_without_volume = old_cfg.clone();
        old_without_volume.volume = cfg.volume;
        old_without_volume.varispeed = cfg.varispeed;
        // The followers are their own threads: the master is untouched.
        old_without_volume.outputs = cfg.outputs.clone();
        // The per-rate plan's upsampler at this rate: a chain built with
        // another one is rebuilt.
        let substitute = self
            .rate_plan
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .substitute(cfg, current_rate)
            .cloned();
        let plan_moved = !resample_only && substitute != self.built_substitute;
        let volume_only =
            !force_reopen && !plan_moved && (old_without_volume == *cfg || resample_kept);
        // Only A/B's level changes, to a fixed offset: the running pair takes it
        // through a 50 ms ramp instead of a rebuild, whose cold stages would be
        // heard. The audio thread applies it to every pair it holds; a chain not
        // handed to it yet (still queued or deferred) would miss it, so that case
        // rebuilds.
        let level_offset = (!force_reopen && !volume_only)
            .then(|| ab_level_only_offset(&old_without_volume, cfg))
            .flatten()
            .filter(|_| {
                self.pending_publication.is_none()
                    && self.swap_tx.slots() == self.swap_tx.buffer().capacity()
            });
        let volume_only = volume_only || level_offset.is_some();

        // The resample-only chain an edit needs is built now when its
        // design is ready; otherwise the design runs off this lock and the
        // previous chain keeps playing until it is done
        // ([`Self::commit_designed`]).
        let wanted_key = resample_only.then(|| bypass_design::DesignKey {
            output: crate::control::config::bypass_output(
                cap_dither_bits(cfg, self.out_format).chain.output,
            ),
            quantum: cfg.quantum,
            input: current_rate,
            ratio: target_ratio,
        });
        let resample_key = if volume_only {
            None
        } else {
            wanted_key.clone()
        };
        let resample_design = resample_key.as_ref().map(|k| self.designer.status_now(k));
        // Varispeed's static budget for the chains this edit builds (as the
        // negotiated format builds them), with every outgoing or in-flight chain
        // that may still run on RT: refused before anything is committed,
        // while a stage stays built or is enabled by this edit. An
        // edit that disables varispeed does not charge the incoming chain for
        // it; if the incoming chain only fits without varispeed, its publish
        // is held until RT finishes retiring the stage.
        let disabling_varispeed = varispeed_changed && !cfg.varispeed.enabled;
        let check_vs_for_chain = !disabling_varispeed
            && self.varispeed.enabled()
            && self.varispeed.built_rate().is_some()
            && !volume_only;
        let check_vs_for_enable = varispeed_changed && cfg.varispeed.enabled;
        if check_vs_for_chain || check_vs_for_enable {
            let extra = (!volume_only).then_some(&old_cfg);
            self.check_stage_budget_in_flight(
                cfg,
                extra,
                current_rate,
                target_ratio,
                cfg.varispeed.keep_pitch,
            )
            .map_err(|e| EngineError::new(RpcError::INVALID_PARAMS, e))?;
        }
        // Pre-build the chain off-RT before committing config/mixer state so a
        // failing config leaves everything untouched.
        let built = if let Some(key) = &resample_key {
            // Validated here; a resample-only chain runs no pair.
            estimate_ab_gains(cfg, current_rate)?;
            let chain = match &resample_design {
                Some(bypass_design::Design::Ready) => self.designer.take(key),
                _ => None,
            };
            chain.map(|c| (c, None, chain_has_effect(cfg)))
        } else if !volume_only && !force_reopen {
            let built = build_active_chain(
                &cap_dither_bits(
                    &self
                        .rate_plan
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .apply(cfg, current_rate),
                    self.out_format,
                ),
                &mut self.ir_cache,
                current_rate,
                target_ratio,
                next_side,
            )?;
            self.built_substitute = substitute;
            Some(built)
        } else {
            None
        };
        // Likewise the A/B offset of a reopen: computed before anything is
        // committed, so a failure never leaves the new volume (or anything
        // else) applied under a config that was not stored.
        if !volume_only && built.is_none() {
            estimate_ab_gains(cfg, current_rate)?;
        }

        // Always update `dsp_enabled` first so RT processor reflects toggle promptly.
        if old_cfg.dsp_enabled != cfg.dsp_enabled {
            // At the same ratio (away from x1) the chain for the new mode is
            // on its way (the DSP chain, or the resample-only one unless its
            // design failed): the outgoing chain keeps playing until it
            // takes over.
            let incoming = if cfg.dsp_enabled {
                built.is_some()
            } else {
                resample_design
                    .as_ref()
                    .is_some_and(|d| !matches!(d, bypass_design::Design::Failed(_)))
            };
            let cmd = if incoming && !running.is_unity() {
                RtDspCmd::SetDspEnabledHolding(cfg.dsp_enabled)
            } else {
                RtDspCmd::SetDspEnabled(cfg.dsp_enabled)
            };
            self.push_cmd(cmd, driver)?;
        }

        if let Some(offset_db) = level_offset {
            self.push_cmd(RtDspCmd::AbLevel { offset_db }, driver)?;
        }

        // A new device: card A is still reserved (the reopen happens later
        // on the control thread), so it is handed over now, at the heard
        // level. That also stops mixer writes, so nothing touches card B
        // (or its switch) before the reopen has reserved and negotiated it.
        if old_cfg.device != cfg.device && self.dac_held {
            self.on_release(ReleaseKind::HandOver);
        }
        // The edit is accepted (every check that refuses it is above): a
        // follower playing the new device stops before the reopen.
        if old_cfg.device != cfg.device {
            self.release_follower_on(&cfg.device);
        }
        // A request still waiting carries over an edit that leaves what it
        // builds alone (a volume change); an edit that makes it obsolete ends
        // it, and with it the outgoing chain's hold unless something takes
        // over (below).
        let mut hold_ended = false;
        if let Some(p) = self.pending_resample.as_mut() {
            if wanted_key.as_ref() == Some(&p.key) {
                p.generation = next_gen;
            } else {
                self.pending_resample = None;
                hold_ended = true;
            }
        }
        // An edit of what the designs depend on: the driver's requests made
        // before it are void (it releases what it held for them, so the
        // next open asks for the new design), and queued builds for it are
        // dropped. An output edit the negotiated format caps to the same
        // section changes nothing.
        let output = crate::control::config::bypass_output(
            cap_dither_bits(cfg, self.out_format).chain.output,
        );
        let elsewhere = old_cfg.device != cfg.device
            || old_cfg.rate.bypass_resampler != cfg.rate.bypass_resampler;
        if elsewhere
            || crate::control::config::bypass_output(
                cap_dither_bits(&old_cfg, self.out_format).chain.output,
            ) != output
            || old_cfg.quantum != cfg.quantum
        {
            self.design_epoch += 1;
            self.design_requests.clear();
            let quantum = cfg.quantum;
            let fits = |k: &bypass_design::DesignKey| k.output == output && k.quantum == quantum;
            // A device or bypass mode change leaves nothing queued but what
            // this edit waits for: the next open asks again, and the new
            // device's builds ahead of time are queued anew.
            self.designer.invalidate(
                |k| (fits(k) && !elsewhere) || resample_key.as_ref() == Some(k),
                fits,
            );
            if let Some(n) = &self.bypass_notify {
                n(audivane_io_pw::BypassNote::Epoch(self.design_epoch));
            }
        }
        self.config_generation = next_gen;
        mirror.config_generation.store(next_gen, Ordering::Release);
        if follower_paths_differ(&self.cfg, cfg) {
            // What the followers needed of the alignment was for the old
            // paths.
            self.align.forget_need();
        }
        self.cfg = cfg.clone();
        self.tee.master_config_changed(&self.cfg);
        self.followers.config_changed(&self.cfg);
        self.sync_followers();
        mirror
            .output_bits
            .store(cfg.chain.output.bits, Ordering::Release);
        mirror.set_bypass_config(cfg);
        self.ab_side = next_side;

        let waker = driver.map(EngineDriverHandle::waker);
        self.driver_error = drv_state == EngineState::Error;
        self.sync_volume(mirror, waker.as_ref());
        if varispeed_changed {
            // Budget was already checked before committing when enabling.
            if let Err(e) = self.apply_varispeed_host(
                cfg.varispeed.enabled,
                cfg.varispeed.keep_pitch,
                current_rate,
            ) {
                log::warn!("varispeed: {e}");
            }
            mirror.set_varispeed_error(self.varispeed.last_error.clone());
            mirror.set_varispeed_request(cfg.varispeed.enabled, cfg.varispeed.speed);
            mirror.set_varispeed_keep_pitch(self.varispeed.keep_pitch());
            self.varispeed_speed = cfg.varispeed.speed;
            self.varispeed_requests += 1;
            if let Some(d) = driver {
                d.set_varispeed_speed(cfg.varispeed.speed, cfg.varispeed.ramp_ms);
            }
        }

        if volume_only {
            if let (Some(offset_db), Some(ab)) = (level_offset, &cfg.ab) {
                self.ab_gains = self.ab_gains.map(|g| g.with_offset(offset_db, ab.headroom));
                mirror.set_ab(true, next_side, self.ab_gains);
                // A non-zero fixed offset cuts even a passthrough pair.
                let had = mirror.dsp_has_effect.load(Ordering::Acquire);
                mirror.dsp_has_effect.store(
                    had || dsp_has_effect(cfg, self.software_db),
                    Ordering::Release,
                );
            }
            if resample_kept {
                // The resample-only chain stays; what status says of the DSP
                // chain follows the config.
                mirror
                    .dsp_has_effect
                    .store(dsp_has_effect(cfg, self.software_db), Ordering::Release);
                mirror.set_ab(cfg.ab.is_some(), next_side, self.ab_gains);
            }
            if self.built_generation == prev_gen && self.pending_publication.is_none() {
                self.built_generation = next_gen;
            }
            if hold_ended && old_cfg.dsp_enabled == cfg.dsp_enabled {
                self.end_bypass_hold(waker.as_ref());
            }
            *mirror
                .conv_warnings
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = conv_warnings;
            return Ok(false);
        }

        if let Some((chain, ab_gains, chain_effect)) = built {
            let wait_vs_retire = !cfg.varispeed.enabled
                && !self.varispeed_retired()
                && self
                    .check_varispeed_budget_in_flight(
                        cfg,
                        Some(&old_cfg),
                        current_rate,
                        target_ratio,
                    )
                    .is_err();
            self.record_in_flight(next_gen, old_cfg.clone());
            mirror.clear_chain_error();
            let has_effect = chain_effect || dsp_has_effect(cfg, self.software_db);
            let _ = self.publish_built_gated(
                cfg,
                chain,
                target_ratio,
                ab_gains,
                has_effect,
                true,
                next_gen,
                wait_vs_retire,
                mirror,
                waker.as_ref(),
            );
            // The fresh chain supersedes whatever made the previous
            // negotiation fall back; drop the driver's stale warning without
            // touching a warning recorded by a newer generation.
            if let Some(d) = driver {
                d.clear_chain_warning_through(next_gen);
            }
        } else {
            // Drop any stale pre-reopen pending chain; `on_negotiated` will build
            // and publish the exact chain once ALSA reopens.
            self.pending_publication = None;
            self.has_pending_swap.store(false, Ordering::Release);
            // The pair is built on the reopen; until then status estimates
            // its gains from the config.
            let ab_gains = None;
            self.ab_gains = ab_gains;
            self.last_dsp_ratio = target_ratio;
            mirror
                .dsp_has_effect
                .store(dsp_has_effect(cfg, self.software_db), Ordering::Release);
            mirror.set_ab(cfg.ab.is_some(), next_side, ab_gains);
            if let (Some(key), Some(design)) = (resample_key, resample_design) {
                match design {
                    bypass_design::Design::Failed(e) => {
                        self.bypass_failed(&key, &e, mirror);
                        hold_ended = true;
                    }
                    _ => {
                        self.pending_resample = Some(PendingResample {
                            key,
                            generation: next_gen,
                            negotiation: self.negotiation_seq,
                        });
                    }
                }
            }
            // Nothing takes over from the outgoing chain: silence until the
            // reopen, as a toggle with no chain on its way (a toggle in this
            // edit already set the hold right).
            if hold_ended
                && self.pending_resample.is_none()
                && old_cfg.dsp_enabled == cfg.dsp_enabled
            {
                self.end_bypass_hold(waker.as_ref());
            }
        }

        // A stage the budget kept off at this rate may fit the edited
        // chains (the outgoing one counted while the swap runs both): it
        // is installed again. A reopen builds it at the negotiation.
        if negotiated
            && !force_reopen
            && self.varispeed.enabled()
            && self.varispeed.built_rate().is_none()
        {
            let budget = crate::control::config::check_stage_budget_for(
                cfg,
                Some(&old_cfg),
                self.out_format,
                current_rate,
                target_ratio,
                self.varispeed.keep_pitch(),
            );
            if let Err(e) = self.varispeed.on_rate(current_rate, budget) {
                log::debug!("varispeed: still off at {current_rate} Hz: {e}");
            }
            mirror.set_varispeed_error(self.varispeed.last_error.clone());
        }

        // Status reports the guards' warnings of the config applied.
        *mirror
            .conv_warnings
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = conv_warnings;
        Ok(force_reopen)
    }

    /// True when the RT thread has finished retiring any varispeed stage and
    /// no host install/retirement is still waiting for queue space.
    fn varispeed_retired(&self) -> bool {
        let applied = self.varispeed_applied.snapshot();
        applied.phase == 0 && applied.stage_rate == 0 && !self.varispeed.pending()
    }

    /// Drop in-flight chain records that the RT processor's atomic swap state
    /// proves are completely promoted past and no longer crossfading.
    fn prune_in_flight(&mut self) {
        let word = self.swap_state.load(Ordering::Acquire);
        let active_gen = word >> 1;
        let rt_in_flight = (word & 1) != 0;
        let any_pending = rt_in_flight || self.pending_publication.is_some();
        self.in_flight_cfgs.retain(|(replaced_by, _)| {
            if any_pending {
                // While a swap is warming/crossfading/held at `active_gen`,
                // the chain replaced BY `active_gen` may still be `fading_old`
                // and chains replaced by `> active_gen` may still be active or
                // warming.
                *replaced_by >= active_gen
            } else {
                *replaced_by > active_gen
            }
        });
    }

    fn record_in_flight(&mut self, swap_gen: u64, prev_cfg: EngineConfig) {
        self.prune_in_flight();
        // The previous edit's chain never reached the RT thread: this one
        // supersedes it, so it can never run and needs no record. The chains
        // it was replacing still run until this swap completes.
        if let Some(p) = &self.pending_publication {
            let superseded = p.built_generation;
            for (replaced_by, _) in &mut self.in_flight_cfgs {
                if *replaced_by == superseded {
                    *replaced_by = swap_gen;
                }
            }
            return;
        }
        self.in_flight_cfgs.push((swap_gen, prev_cfg));
    }

    /// Check varispeed's static budget for `cfg` alongside `extra_outgoing`
    /// and every chain still running or crossfading on the RT thread.
    fn check_varispeed_budget_in_flight(
        &mut self,
        cfg: &EngineConfig,
        extra_outgoing: Option<&EngineConfig>,
        graph_rate: u32,
        ratio: RateRatio,
    ) -> Result<(), String> {
        let keep_pitch = self.varispeed.keep_pitch();
        self.check_stage_budget_in_flight(cfg, extra_outgoing, graph_rate, ratio, keep_pitch)
    }

    /// [`Self::check_varispeed_budget_in_flight`] for the stage kind
    /// `keep_pitch` asks for.
    fn check_stage_budget_in_flight(
        &mut self,
        cfg: &EngineConfig,
        extra_outgoing: Option<&EngineConfig>,
        graph_rate: u32,
        ratio: RateRatio,
        keep_pitch: bool,
    ) -> Result<(), String> {
        self.prune_in_flight();
        crate::control::config::check_stage_budget_for(
            cfg,
            extra_outgoing,
            self.out_format,
            graph_rate,
            ratio,
            keep_pitch,
        )?;
        for (_, prev) in &self.in_flight_cfgs {
            crate::control::config::check_stage_budget_for(
                cfg,
                Some(prev),
                self.out_format,
                graph_rate,
                ratio,
                keep_pitch,
            )?;
        }
        Ok(())
    }

    /// Hand the host varispeed's request and its stage kind: Keep pitch
    /// first when enabling (a running stage is replaced by the other kind
    /// under a fade, prime and fade), last when disabling (nothing is
    /// built for a stage on its way out). The budget is the caller's.
    fn apply_varispeed_host(
        &mut self,
        enabled: bool,
        keep_pitch: bool,
        rate: u32,
    ) -> Result<(), String> {
        let vs = &mut self.varispeed;
        let (first, second) = if enabled {
            let kp = vs.set_keep_pitch(keep_pitch, rate, Ok(()));
            (kp, vs.set_enabled(true, rate, Ok(())))
        } else {
            let en = vs.set_enabled(false, rate, Ok(()));
            (en, vs.set_keep_pitch(keep_pitch, rate, Ok(())))
        };
        // A full queue is kept pending for the reaper: the other error wins.
        match (first, second) {
            (Err(e), _) if e != varispeed::VarispeedHost::QUEUE_FULL_ERR => Err(e),
            (_, Err(e)) => Err(e),
            (r, Ok(())) => r,
        }
    }

    /// Live varispeed control without rebuilding the DSP chains. Bounds and the
    /// static budget (including every chain still playing or crossfading) are
    /// checked before the host or RT stage is touched, so a rejected request
    /// never retires an accepted stage. RT queue backpressure is deferred to the
    /// reaper (`reconcile_varispeed`), as in `apply_config`.
    #[cfg(test)]
    fn set_varispeed(
        &mut self,
        enabled: bool,
        speed: f64,
        ramp_ms: f64,
        mirror: &SharedMirror,
        waker: Option<&DriverWaker>,
    ) -> Result<(), EngineError> {
        let keep_pitch = self.varispeed.keep_pitch();
        self.set_varispeed_cfg(
            audivane_protocol::ipc::VarispeedConfig {
                enabled,
                speed,
                ramp_ms,
                keep_pitch,
            },
            mirror,
            waker,
        )
    }

    /// [`Self::set_varispeed`] with Keep pitch: the stage kind
    /// `vs_cfg.keep_pitch` asks for must fit the static budget at the
    /// running rate while varispeed is on (the stretch does not at 176.4
    /// kHz and above), or nothing changes.
    fn set_varispeed_cfg(
        &mut self,
        vs_cfg: audivane_protocol::ipc::VarispeedConfig,
        mirror: &SharedMirror,
        waker: Option<&DriverWaker>,
    ) -> Result<(), EngineError> {
        let audivane_protocol::ipc::VarispeedConfig {
            enabled,
            speed,
            keep_pitch,
            ..
        } = vs_cfg;
        crate::control::config::validate_varispeed(&vs_cfg)
            .map_err(|e| EngineError::new(RpcError::INVALID_PARAMS, e))?;
        let (rate, ratio) = (self.last_rate, self.last_ratio);
        if enabled {
            let cfg_now = self.cfg.clone();
            self.check_stage_budget_in_flight(&cfg_now, None, rate, ratio, keep_pitch)
                .map_err(|e| EngineError::new(RpcError::INVALID_PARAMS, e))?;
        }
        let res = self.apply_varispeed_host(enabled, keep_pitch, rate);
        mirror.set_varispeed_error(self.varispeed.last_error.clone());
        mirror.set_varispeed_keep_pitch(self.varispeed.keep_pitch());
        match res {
            Ok(()) => {}
            Err(e) if e == varispeed::VarispeedHost::QUEUE_FULL_ERR => {
                // `VarispeedHost` kept the install/retirement pending for the
                // reaper; accept the request so config/status stay consistent
                // with what reconciliation will deliver.
                log::debug!("varispeed: RT queue full, deferred to reaper: {e}");
                if let Some(w) = waker {
                    w.wake_rt();
                }
            }
            Err(e) => return Err(EngineError::new(RpcError::INVALID_PARAMS, e)),
        }
        self.cfg.varispeed = vs_cfg;
        self.varispeed_speed = speed;
        self.varispeed_requests += 1;
        mirror.set_varispeed_request(enabled, speed);
        Ok(())
    }

    /// The reaper's tick: varispeed work the RT queue refused (a full
    /// queue) goes out again until it is taken.
    fn reconcile_varispeed(&mut self, mirror: &SharedMirror) {
        if !self.varispeed.pending() {
            return;
        }
        let cfg_now = self.cfg.clone();
        let (rate, ratio) = (self.last_rate, self.last_ratio);
        let budget = self.check_varispeed_budget_in_flight(&cfg_now, None, rate, ratio);
        if let Err(e) = self.varispeed.reconcile(rate, budget) {
            log::debug!("varispeed: queued work not taken yet: {e}");
        }
        mirror.set_varispeed_error(self.varispeed.last_error.clone());
    }

    /// The driver opened `device`. A rate reopen (or an acquire) of card A
    /// can finish after a config change moved the engine to card B (that
    /// change waits behind it in the driver's queue): writing the mixer now
    /// would touch B before it is reserved, and give A the digital gain
    /// planned for B. Such an open is superseded: nothing is written,
    /// built or taken, and the driver closes it without resuming.
    #[allow(clippy::too_many_arguments)]
    fn negotiated_on(
        &mut self,
        device: &str,
        key: Option<NegotiatedKey>,
        graph_rate: u32,
        actual_ratio: RateRatio,
        active_sink: SinkChoice,
        format: SampleFormat,
        mirror: &SharedMirror,
        ab_atomic: &AtomicU8,
        waker: Option<&DriverWaker>,
    ) -> NegotiatedChain {
        if device != self.cfg.device {
            log::info!(
                "engine: the open of {device} is superseded (the config is on {}); not taking it",
                self.cfg.device
            );
            return NegotiatedChain {
                generation: self.config_generation,
                superseded: true,
                ..NegotiatedChain::default()
            };
        }
        // Only an open the engine takes describes what is open now.
        if let Some(key) = key {
            self.negotiated_key = Some(key);
        }
        self.apply_negotiated(
            graph_rate,
            actual_ratio,
            active_sink,
            format,
            mirror,
            ab_atomic,
            waker,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_negotiated(
        &mut self,
        graph_rate: u32,
        actual_ratio: RateRatio,
        active_sink: SinkChoice,
        format: SampleFormat,
        mirror: &SharedMirror,
        ab_atomic: &AtomicU8,
        waker: Option<&DriverWaker>,
    ) -> NegotiatedChain {
        // A new negotiation builds its own chain: an edit's request from
        // before it is void, and no crossfade carries an outgoing chain over
        // a reopen.
        self.negotiation_seq += 1;
        self.pending_resample = None;
        self.in_flight_cfgs.clear();
        let rate_changed = self.last_rate != graph_rate;
        let format_changed = self.out_format != Some(format);
        self.out_format = Some(format);
        self.dac_bits.store(format.bits(), Ordering::Relaxed);
        let cfg_stale =
            self.built_generation != self.config_generation || self.pending_publication.is_some();
        self.last_rate = graph_rate;
        self.last_ratio = actual_ratio;
        // Varispeed: a stage for the rate the graph runs at now, built here
        // (off the RT thread) and queued before the driver resumes.
        let budget = crate::control::config::check_stage_budget_for(
            &self.cfg,
            None,
            Some(format),
            graph_rate,
            actual_ratio,
            self.varispeed.keep_pitch(),
        );
        if let Err(e) = self.varispeed.on_rate(graph_rate, budget) {
            log::warn!("varispeed: off at {graph_rate} Hz: {e}");
        }
        mirror.set_varispeed_error(self.varispeed.last_error.clone());
        // The chain is built for the format just negotiated: dither never
        // targets more bits than the DAC format carries.
        // The per-rate plan's upsampler at this rate, if any: an over-budget
        // rate never builds the requested one.
        let (substitute, planned) = {
            let plan = self.rate_plan.lock().unwrap_or_else(|e| e.into_inner());
            (
                plan.substitute(&self.cfg, graph_rate).cloned(),
                plan.apply(&self.cfg, graph_rate),
            )
        };
        if substitute.is_some() {
            log::info!(
                "{graph_rate} Hz: building the per-rate fallback upsampler in place of the \
                 requested one (over the CPU budget at this rate)"
            );
        }
        let cfg_clone = cap_dither_bits(&planned, Some(format));
        self.built_substitute = substitute;
        let cur_side = self.ab_side;
        let cfg_gen = self.config_generation;
        ab_atomic.store(side_to_u8(cur_side), Ordering::Release);

        // Re-apply hardware/digital volume on every open/reopen/acquire and
        // device change before early-returning or publishing the chain.
        // The reopen dropped the buffered audio, so nothing needs to be
        // staged, and the card may have been reset: write again.
        self.hw_known = None;
        self.hw_known_device = None;
        self.failsafe_mute_verified = false;
        self.dac_held = true;
        self.driver_error = false;
        failsafe::note_released(self.released_marker.as_deref(), false);
        self.digital_peak = None;
        self.pending_hw = None;
        self.sync_volume(mirror, waker);
        self.digital_peak = None;
        let software_db = self.software_db;

        self.prewarm_bypass_designs(&cfg_clone);
        if plays_resample_only(&cfg_clone, Some(active_sink), actual_ratio) {
            return self.negotiated_resample_only(
                &cfg_clone,
                graph_rate,
                actual_ratio,
                mirror,
                waker,
            );
        }

        let build_ratio = if active_sink == SinkChoice::Direct {
            if !rate_changed && !format_changed && !cfg_stale && !self.last_built_resample {
                // Direct only temporarily holds ALSA at 1x and no config or rate
                // change occurred; keep the warm DSP chain untouched.
                return NegotiatedChain {
                    generation: cfg_gen,
                    ..NegotiatedChain::default()
                };
            }
            // Where a switch to the DSP sink would land, from what this open
            // found, as for an edit while Direct plays.
            warm_dsp_ratio(&cfg_clone, graph_rate, self.negotiated_key.as_ref())
        } else {
            actual_ratio
        };

        log::debug!(
            "engine on_negotiated: gen={cfg_gen} rate={graph_rate} actual_ratio={actual_ratio} build_ratio={build_ratio} sink={active_sink:?} cfg_stale={cfg_stale}"
        );

        let gate_seq = |swap_seq: u64| (active_sink != SinkChoice::Direct).then_some(swap_seq);

        match build_active_chain(
            &cfg_clone,
            &mut self.ir_cache,
            graph_rate,
            build_ratio,
            cur_side,
        ) {
            Ok((chain, ab_gains, chain_effect)) => {
                mirror.clear_chain_error();
                let has_effect = chain_effect || dsp_has_effect(&cfg_clone, software_db);
                let swap_seq = self.publish_built(
                    &cfg_clone,
                    chain,
                    build_ratio,
                    ab_gains,
                    has_effect,
                    false,
                    cfg_gen,
                    mirror,
                    waker,
                );
                NegotiatedChain {
                    required_chain_seq: gate_seq(swap_seq),
                    error: None,
                    fatal: false,
                    generation: cfg_gen,
                    superseded: false,
                    bypass_unbuildable: false,
                    bypass_pending: false,
                    bypass_epoch: 0,
                }
            }
            Err(e) => {
                let primary =
                    format!("failed to rebuild DSP chain for ({graph_rate} Hz {build_ratio}): {e}");
                log::error!("{primary}");
                // A chain the resume gate can never see must not strand
                // playback in `Switching` and must not silently let a stale
                // chain play. Rebuild the requested chain with the failing
                // stage disabled so the negotiated ratio is still covered;
                // the failure is reported in status and to acquire waiters.
                match build_fallback_chain(
                    &cfg_clone,
                    &mut self.ir_cache,
                    graph_rate,
                    build_ratio,
                    cur_side,
                ) {
                    Ok((chain, ab_gains, chain_effect, given_up)) => {
                        let has_effect = chain_effect || dsp_has_effect(&cfg_clone, software_db);
                        let swap_seq = self.publish_built(
                            &cfg_clone,
                            chain,
                            build_ratio,
                            ab_gains,
                            has_effect,
                            false,
                            cfg_gen,
                            mirror,
                            waker,
                        );
                        let msg =
                            format!("{primary}; playing a fallback chain instead: {given_up}");
                        log::warn!("{msg}");
                        mirror.set_chain_error(msg.clone());
                        NegotiatedChain {
                            required_chain_seq: gate_seq(swap_seq),
                            error: Some(msg),
                            fatal: false,
                            generation: cfg_gen,
                            superseded: false,
                            bypass_unbuildable: false,
                            bypass_pending: false,
                            bypass_epoch: 0,
                        }
                    }
                    Err(fallback) => {
                        let msg = format!(
                            "{primary}; fallback chain also failed ({fallback}); playback stays in Error"
                        );
                        log::error!("{msg}");
                        mirror.set_chain_error(msg.clone());
                        NegotiatedChain {
                            required_chain_seq: None,
                            error: Some(msg),
                            fatal: true,
                            generation: cfg_gen,
                            superseded: false,
                            bypass_unbuildable: false,
                            bypass_pending: false,
                            bypass_epoch: 0,
                        }
                    }
                }
            }
        }
    }
}

impl BuilderState {
    /// A negotiation for Direct or DSP off away from x1: build the engine
    /// bypass resampler's chain and gate the resume on it. When it cannot
    /// be built, say so: the driver drops the pair from the bypass mask and
    /// reopens into the PipeWire path ([`NegotiatedChain::bypass_unbuildable`]),
    /// so this never plays silence.
    fn negotiated_resample_only(
        &mut self,
        cfg: &EngineConfig,
        graph_rate: u32,
        ratio: RateRatio,
        mirror: &SharedMirror,
        waker: Option<&DriverWaker>,
    ) -> NegotiatedChain {
        let generation = self.config_generation;
        let key = bypass_design::DesignKey {
            output: crate::control::config::bypass_output(cfg.chain.output),
            quantum: cfg.quantum,
            input: graph_rate,
            ratio,
        };
        let out = key.output_rate();
        // The design runs off this lock: until it is done the driver falls
        // back to PipeWire, and comes back when told
        // (`BypassNotifier::designed`).
        let ready = match self.designer.status_now(&key) {
            bypass_design::Design::Ready => self.designer.take(&key),
            _ => None,
        };
        let built = match (self.designer.peek(&key), ready) {
            (bypass_design::Design::Ready, Some(chain)) => Ok(chain),
            (bypass_design::Design::Failed(e), _) => Err(e),
            _ => {
                log::info!(
                    "engine: designing the {graph_rate} Hz to {out} Hz resampler; PipeWire converts meanwhile"
                );
                mirror.set_bypass_designing(Some(format!(
                    "the engine is still building its {graph_rate} Hz to {out} Hz resampler; PipeWire converts meanwhile"
                )));
                // The driver waits for this design (in this epoch).
                let epoch = self.design_epoch;
                if !self
                    .design_requests
                    .iter()
                    .any(|(k, e)| *k == key && *e == epoch)
                {
                    self.design_requests.push((key.clone(), epoch));
                }
                self.designing = Some(key);
                return NegotiatedChain {
                    bypass_pending: true,
                    bypass_epoch: epoch,
                    generation,
                    ..NegotiatedChain::default()
                };
            }
        };
        self.designing = None;
        mirror.set_bypass_designing(None);
        match built {
            Ok(chain) => {
                mirror.clear_chain_error();
                mirror.set_bypass_failure(None);
                let has_effect = dsp_has_effect(cfg, self.software_db);
                let ab_gains = self.ab_gains;
                let swap_seq = self.publish_built(
                    cfg, chain, ratio, ab_gains, has_effect, false, generation, mirror, waker,
                );
                NegotiatedChain {
                    required_chain_seq: Some(swap_seq),
                    generation,
                    ..NegotiatedChain::default()
                }
            }
            Err(e) => {
                let msg = format!(
                    "the engine could not build its {graph_rate} Hz to {out} Hz resampler ({e}); PipeWire converts instead"
                );
                log::warn!("{msg}");
                mirror.set_bypass_failure(Some(msg));
                NegotiatedChain {
                    bypass_unbuildable: true,
                    bypass_pending: false,
                    bypass_epoch: self.design_epoch,
                    generation,
                    ..NegotiatedChain::default()
                }
            }
        }
    }

    /// Queue the designs of the pairs this device converts with the
    /// engine bypass resampler (the configured rates it lacks, to where
    /// they land), so a stream at one of them finds its design ready.
    fn prewarm_bypass_designs(&self, cfg: &EngineConfig) {
        let Some(key) = self.negotiated_key else {
            return;
        };
        let desired = to_driver_config(cfg).desired_dsp(0);
        if !desired.bypass.engine {
            return;
        }
        for &g in &cfg.rate.allowed_rates {
            if let Some(out) = desired.bypass_landing(g, key.rates) {
                let _ = self.designer.status(&bypass_design::DesignKey {
                    output: crate::control::config::bypass_output(cfg.chain.output),
                    quantum: cfg.quantum,
                    input: g,
                    ratio: RateRatio::from_rates(g, out),
                });
            }
        }
    }

    /// A design an edit waited for is done ([`Self::pending_resample`]):
    /// build and swap in the resample-only chain when nothing moved on
    /// meanwhile (the same config, graph rate and ratio); tell the driver
    /// when it cannot be built (it falls back to PipeWire). Called by the
    /// designer's callback.
    fn commit_designed(&mut self, mirror: &SharedMirror, waker: Option<&DriverWaker>) {
        if let Some(k) = &self.designing
            && self.designer.peek(k) != bypass_design::Design::Pending
        {
            self.designing = None;
            mirror.set_bypass_designing(None);
        }
        let Some(p) = self.pending_resample.clone() else {
            return;
        };
        let current = p.generation == self.config_generation
            && p.negotiation == self.negotiation_seq
            && p.key.input == self.last_rate
            && p.key.ratio == self.last_ratio;
        if !current {
            self.pending_resample = None;
            self.end_bypass_hold(waker);
            return;
        }
        match self.designer.peek(&p.key) {
            bypass_design::Design::Pending => {}
            bypass_design::Design::Failed(e) => {
                self.pending_resample = None;
                self.bypass_failed(&p.key, &e, mirror);
                self.end_bypass_hold(waker);
            }
            bypass_design::Design::Ready => {
                // Still building its chain (one was handed over before):
                // the next report brings it.
                let Some(chain) = self.designer.take(&p.key) else {
                    return;
                };
                self.pending_resample = None;
                let cfg = cap_dither_bits(&self.cfg, self.out_format);
                mirror.clear_chain_error();
                mirror.set_bypass_failure(None);
                let has_effect = dsp_has_effect(&cfg, self.software_db);
                let ab_gains = self.ab_gains;
                let generation = self.config_generation;
                let _ = self.publish_built(
                    &cfg,
                    chain,
                    p.key.ratio,
                    ab_gains,
                    has_effect,
                    true,
                    generation,
                    mirror,
                    waker,
                );
            }
        }
    }

    /// A design finished (the designer's callback): tell the driver when it
    /// waits for this one, asked for under the current device and output
    /// (a design from before such an edit says nothing of the pairs now),
    /// then serve what an edit waited for.
    fn designed(
        &mut self,
        key: &bypass_design::DesignKey,
        ok: bool,
        mirror: &SharedMirror,
        waker: Option<&DriverWaker>,
    ) {
        let epoch = self.design_epoch;
        let wanted = self
            .design_requests
            .iter()
            .any(|(k, e)| k == key && *e == epoch);
        self.design_requests
            .retain(|(k, e)| *e == epoch && k != key);
        if wanted && let Some(n) = &self.bypass_notify {
            n(audivane_io_pw::BypassNote::Designed {
                input: key.input,
                output: key.output_rate(),
                ok,
            });
        }
        self.commit_designed(mirror, waker);
    }

    /// End a DSP toggle's hold on the outgoing chain (its incoming chain
    /// is not coming): silence until the reopen, as for a toggle with no
    /// chain on its way.
    fn end_bypass_hold(&mut self, waker: Option<&DriverWaker>) {
        let dsp_enabled = self.cfg.dsp_enabled;
        if self
            .push_cmd(RtDspCmd::SetDspEnabled(dsp_enabled), None)
            .is_ok()
            && let Some(w) = waker
        {
            w.wake_rt();
        }
    }

    /// The resample-only chain for `key` cannot be built: say so, and let
    /// the driver fall back to PipeWire for the pair.
    fn bypass_failed(&self, key: &bypass_design::DesignKey, e: &str, mirror: &SharedMirror) {
        let msg = format!(
            "the engine could not build its {} Hz to {} Hz resampler ({e}); PipeWire converts instead",
            key.input,
            key.output_rate()
        );
        log::warn!("{msg}");
        mirror.set_bypass_failure(Some(msg));
        if let Some(n) = &self.bypass_notify {
            n(audivane_io_pw::BypassNote::Designed {
                input: key.input,
                output: key.output_rate(),
                ok: false,
            });
        }
    }
}

/// Shared non-RT chain-building + status atomics accessed both by [`RealEngine`]
/// and by the [`RatioNegotiator`] callback on ALSA reopen.
impl BuilderState {
    /// The master moves to a device a follower plays: that follower stops
    /// first (its thread joins, closing its PCM), so the master's reopen
    /// finds the device free.
    fn release_follower_on(&mut self, device: &str) {
        if !self.followers.contains(device) {
            return;
        }
        log::info!("followers: {device} becomes the master; its follower stops");
        if let Err(e) = self.followers.remove(&mut self.tee, device) {
            log::warn!("followers: {device} not removed: {e}");
        }
        if self.followers.is_empty() {
            self.align.reset();
            self.comp.reset();
        }
    }

    /// The followers the config lists run, with its controls.
    fn sync_followers(&mut self) {
        let master = self.cfg.device.clone();
        let before = self.followers.devices();
        self.followers.sync(&mut self.tee, &master, &self.cfg);
        if self.followers.devices() != before {
            // Another set of followers: what the last one needed is not
            // what this one needs.
            self.align.forget_need();
        }
        // (Before a new follower opens: it is placed with what it needs.)
        let now = (self.clock)().saturating_duration_since(self.align_t0);
        self.floor_alignment(now.as_secs_f64());
        if self.followers.is_empty() {
            self.align.reset();
            self.comp.reset();
        }
    }

    /// Until the followers' need is measured, the alignment starts at what
    /// they are expected to need (`AlignControl::floor`): a fresh follower
    /// is placed with the source it needs, not grown into it while heard.
    fn floor_alignment(&mut self, now_s: f64) {
        if self.followers.is_empty() {
            return;
        }
        let rate = self.tee.shared().graph_rate.load(Ordering::Relaxed);
        let rate = if rate > 0 { rate } else { 48_000 };
        let quantum_ms = f64::from(self.cfg.quantum.max(MIN_QUANTUM)) * 1e3 / f64::from(rate);
        let lead = follower::master_lead_ms(quantum_ms, DEFAULT_BUFFER_PERIODS);
        let expected = self.followers.expected_need_ms(&self.cfg, rate, lead);
        self.followers.floor_lag(&self.cfg, rate, lead);
        if let Some(ms) = self.align.floor(now_s, expected) {
            log::info!("followers: alignment {ms} ms, what they are expected to need");
            if let Err(e) = self.tee.set_alignment_ms(ms) {
                log::warn!("followers: alignment {ms} ms not applied: {e}");
            }
        }
    }

    /// The reaper's tick: the master's alignment grows with what the
    /// followers need (when one ran out of source, capped at 120 ms); at
    /// the cap, a follower that needs more plays late by the rest.
    fn tick_followers(&mut self) {
        if self.followers.is_empty() {
            return;
        }
        self.followers.reap();
        self.tee
            .set_command_lead_ms(self.followers.command_lead_ms());
        let now = (self.clock)().saturating_duration_since(self.align_t0);
        self.floor_alignment(now.as_secs_f64());
        let least = self.followers.least_slack_ms();
        // A master that played with these followers before starts at the
        // alignment it had (no second of starved followers after a
        // restart); then it grows as usual.
        let restored = self
            .followers
            .remembered_alignment()
            .and_then(|ms| self.align.start_at(now.as_secs_f64(), ms));
        if let Some(ms) = restored.or_else(|| self.align.tick(now.as_secs_f64(), least)) {
            if let Err(e) = self.tee.set_alignment_ms(ms) {
                log::warn!("followers: alignment {ms} ms not applied: {e}");
            }
            self.followers.note_alignment(ms);
        }
        // A shrink that waited for silence landed; what the followers need
        // now; a shrink to it (applied by the RT side at the next silence,
        // a pause or a new stream: never heard).
        let now_s = now.as_secs_f64();
        if self.align.landed(now_s, self.tee.applied_alignment_ms()) {
            log::info!(
                "followers: alignment down to {} ms (in silence)",
                self.align.ms
            );
        }
        self.align
            .note_need(now_s, self.followers.alignment_need_ms(self.align.ms));
        match self.align.shrink(now_s, !self.followers.any_late()) {
            Some((ms, true)) => {
                log::info!(
                    "followers: need {:.1} ms; the alignment shrinks from {} to {ms} ms at the next silence",
                    self.align.need_ms,
                    self.align.ms
                );
                if let Err(e) = self.tee.shrink_alignment_ms(ms) {
                    log::warn!("followers: alignment shrink to {ms} ms not sent: {e}");
                }
            }
            Some((ms, false)) => {
                if let Err(e) = self.tee.set_alignment_ms(ms) {
                    log::warn!("followers: alignment {ms} ms not applied: {e}");
                }
            }
            None => {}
        }
        let at_cap = self.align.settled_at_cap(now.as_secs_f64());
        self.followers.tick_lag(now.as_secs_f64(), at_cap);
        self.followers.tick_clocks();
        // The chain compensation: the master waits for the longest
        // follower's chain (the followers pad to the master's).
        let shared = self.tee.shared();
        let master = shared.master_chain_graph.get();
        let rate = shared.graph_rate.load(Ordering::Relaxed).max(8_000);
        let need = self.followers.comp_need(master);
        if let Some(frames) = self
            .comp
            .tick(now.as_secs_f64(), need, tee::max_comp_frames(rate))
        {
            log::info!("followers: chain compensation {frames} frames at {rate} Hz");
            if let Err(e) = self.tee.set_comp_frames(frames) {
                log::warn!("followers: compensation {frames} frames not applied: {e}");
            }
        }
    }

    /// The multi-device status, in config order.
    fn outputs_status(&self) -> OutputsStatus {
        let running = self.followers.status();
        let followers = self
            .cfg
            .outputs
            .followers
            .iter()
            .map(|f| {
                let lag = self.followers.controls(&f.device).map(|c| c.lag_ms());
                let snap = running.iter().find(|(d, _)| *d == f.device).map(|(_, s)| s);
                let own = f.chain_patch.as_ref().map(|_| {
                    let own = crate::control::follower_chain::config(&self.cfg, f);
                    (
                        crate::control::ab_side::differing_fields(&self.cfg.chain, &own.chain),
                        own.chain,
                    )
                });
                let mut v = follower::view(
                    f,
                    snap,
                    lag.unwrap_or(0.0),
                    self.followers.failure(&f.device),
                    own,
                );
                v.need_ms = snap
                    .and_then(|s| follower::need_ms_of(s, self.align.ms))
                    .filter(|n| n.is_finite())
                    .map(|n| (n * 10.0).round() / 10.0);
                v
            })
            .collect();
        let none = self.followers.is_empty();
        OutputsStatus {
            alignment_ms: if none { 0.0 } else { self.align.ms },
            alignment_need_ms: (!none && self.align.need_ms.is_finite())
                .then(|| (self.align.need_ms * 10.0).round() / 10.0),
            alignment_shrink_ms: self.align.pending.filter(|_| !none),
            master: self.master_latency(),
            followers,
            settling: self.settling(),
        }
    }

    /// Why the multi-device alignment may still move (`None`: settled, or
    /// no follower runs): a follower opening or locking, the master's
    /// alignment or compensation moving.
    fn settling(&self) -> Option<String> {
        if self.followers.is_empty() {
            return None;
        }
        if let Some((device, what)) = self.followers.first_unsettled() {
            return Some(format!("follower {device} is {what}"));
        }
        let now = (self.clock)().saturating_duration_since(self.align_t0);
        if let Some(why) = self
            .align
            .settling(now.as_secs_f64(), self.followers.least_slack_ms())
        {
            return Some(why);
        }
        let shared = self.tee.shared();
        let rate = shared.graph_rate.load(Ordering::Relaxed).max(8_000);
        let need = self.followers.comp_need(shared.master_chain_graph.get());
        self.comp
            .settling(need, tee::max_comp_frames(rate))
            .then(|| "the master's chain compensation is moving".to_string())
    }

    /// The master's chain latency, compensation and alignment, as its
    /// blocks are mapped (zeros without followers).
    fn master_latency(&self) -> audivane_protocol::ipc::PathLatency {
        let shared = self.tee.shared();
        let chain = shared.master_chain_graph.get();
        if self.followers.is_empty() || !chain.is_finite() {
            return audivane_protocol::ipc::PathLatency::default();
        }
        let graph = shared.graph_rate.load(Ordering::Relaxed);
        let ratio = self.last_dsp_ratio;
        let out = |g: f64| g * ratio.as_f64();
        let comp = shared.comp_frames.load(Ordering::Relaxed) as f64;
        let align = (self.align.ms * 1e-3 * f64::from(graph)).round();
        audivane_protocol::ipc::PathLatency::from_frames(
            ratio.output_rate(graph).unwrap_or(graph),
            out(chain).round(),
            out(comp),
            out(align),
        )
    }
}

/// A follower's chain for its device (its ratio and format), chain A only:
/// the master's per-rate plan at `graph_rate`, and where chain A's custom
/// design does not build at the ratio this device opened at, its
/// substitute there ([`crate::control::cpu_budget::played_at`], the same
/// choice made at this ratio), so a follower never retries a build that
/// cannot succeed.
fn follower_chain(
    rate_plan: &Mutex<crate::api::RatePlan>,
    cfg: &EngineConfig,
    graph_rate: u32,
    ratio: RateRatio,
    format: SampleFormat,
) -> Result<ActiveChain, String> {
    let mut ir_cache = IrCache::new();
    let planned = {
        let plan = rate_plan.lock().unwrap_or_else(|e| e.into_inner());
        plan.apply(cfg, graph_rate).into_owned()
    };
    let planned = crate::control::cpu_budget::played_at(
        crate::control::cpu_budget::measured(),
        &planned,
        None,
        graph_rate,
        ratio,
    )
    .into_owned();
    let mut cfg = cap_dither_bits(&planned, Some(format));
    cfg.ab = None;
    // Its IRs are loaded fresh for each build (a reopen, a rate change):
    // the guards run on them again.
    build_active_chain_guarded(&cfg, &mut ir_cache, graph_rate, ratio, AbSide::A, true)
        .map(|(chain, _, _)| chain)
        .map_err(|e| e.message)
}

/// Whether an edit from `a` to `b` changes what the followers need of the
/// master's alignment: either path (the chains, the quantum, the master's
/// device) or a follower's own (its device, period, chain, output pair).
fn follower_paths_differ(a: &EngineConfig, b: &EngineConfig) -> bool {
    let own = |c: &EngineConfig| {
        c.outputs
            .followers
            .iter()
            .map(|f| {
                (
                    f.device.clone(),
                    f.period_ms.map(f64::to_bits),
                    f.chain_patch.clone(),
                    f.output_channels,
                )
            })
            .collect::<Vec<_>>()
    };
    a.chain != b.chain || a.quantum != b.quantum || a.device != b.device || own(a) != own(b)
}

/// The followers' host: their chains are built as the master's, for the
/// follower's device (its ratio and format), chain A only, respecting the
/// shared per-rate `RatePlan`.
fn follower_host(rate_plan: Arc<Mutex<crate::api::RatePlan>>) -> follower::FollowerHost {
    let build: follower::ChainBuilder = Arc::new(move |cfg, graph_rate, ratio, format| {
        follower_chain(&rate_plan, cfg, graph_rate, ratio, format)
    });
    let host = follower::FollowerHost::new(build, Arc::new(follower_ratio_for));
    // (Unit tests keep the pairs' clocks in memory, off the user's state.)
    if cfg!(test) {
        return host;
    }
    match clocks::default_path() {
        Some(path) => host.with_clock_store(clocks::ClockStore::open(path)),
        None => host,
    }
}

/// Seed the initial per-rate plan before the first chain is built at start-up.
/// Skips the CPU benchmark when DSP or the upsampler is off.
fn seed_rate_plan(cfg: &EngineConfig) -> crate::api::RatePlan {
    if !cfg.dsp_enabled
        || matches!(
            cfg.chain.upsampler.target,
            audivane_protocol::RateTarget::Off
        )
    {
        return crate::api::RatePlan::default();
    }
    crate::control::cpu_budget::plan_rates(
        Some(crate::control::cpu_budget::machine()),
        cfg,
        None,
        None,
        &[],
    )
    .0
}

struct SharedMirror {
    /// Generation of the last config `apply` accepted (the chain the
    /// audio thread runs reaches it once it is built and swapped in).
    config_generation: AtomicU64,
    dsp_has_effect: AtomicBool,
    /// The master volume has a digital part (gain or balance), which every
    /// path applies, Direct and bypass included.
    digital_gain: AtomicBool,
    /// Configured `chain.output.bits` ("Dither to"), before the format cap.
    output_bits: AtomicU32,
    ab_enabled: AtomicBool,
    ab_side: AtomicU8,
    /// The running pair's gains; `None` until one is published (status
    /// then estimates them from the config).
    ab_gains: Mutex<Option<AbGains>>,
    /// What `Status.volume` reports, and a generation bumped whenever it
    /// changes (see [`Engine::volume_state`]).
    volume_status: Mutex<(VolumeStatus, u64)>,
    chain_error: Mutex<Option<String>>,
    /// `rate.bypass_resampler: engine` and `dsp_enabled` of the applied
    /// config, for the bypass resampler's status.
    bypass_engine: AtomicBool,
    dsp_enabled: AtomicBool,
    /// Why the engine bypass resampler last fell back to PipeWire at run
    /// time (its chain failed to build); cleared by a build or an edit.
    bypass_failure: Mutex<Option<String>>,
    /// The pair whose chain is still being built while PipeWire converts
    /// (a negotiation fell back for it), as status says it.
    bypass_designing: Mutex<Option<String>>,
    /// Why the last varispeed stage build was refused (budget, design).
    varispeed_error: Mutex<Option<String>>,
    /// The convolution guards' warnings of the config validated last.
    conv_warnings: Mutex<Vec<audivane_protocol::ipc::ConvWarning>>,
    /// Accepted host-side varispeed request `(enabled, speed)` stored together
    /// under one lock so status never observes a torn pair across concurrent
    /// requests (independent of RT-applied state while idle, on Direct, with
    /// DSP off, or under an underfeed/overload hold).
    varispeed_request: Mutex<(bool, f64)>,
    /// Keep pitch as the host accepted it (status' `varispeed.keep_pitch`).
    varispeed_keep_pitch: AtomicBool,
}

impl SharedMirror {
    fn new(cfg: &EngineConfig) -> Self {
        let has_effect = dsp_has_effect(cfg, volume::target_db(&cfg.volume));
        Self {
            config_generation: AtomicU64::new(0),
            dsp_has_effect: AtomicBool::new(has_effect),
            digital_gain: AtomicBool::new(
                volume::target_db(&cfg.volume) != 0.0
                    || cfg.volume.balance_db != 0.0
                    || cfg.volume.muted,
            ),
            output_bits: AtomicU32::new(cfg.chain.output.bits),
            ab_enabled: AtomicBool::new(cfg.ab.is_some()),
            ab_side: AtomicU8::new(0),
            ab_gains: Mutex::new(None),
            volume_status: Mutex::new((VolumeStatus::from_config(&cfg.volume), 0)),
            chain_error: Mutex::new(None),
            bypass_engine: AtomicBool::new(cfg.rate.bypass_resampler == BypassResampler::Engine),
            dsp_enabled: AtomicBool::new(cfg.dsp_enabled),
            bypass_failure: Mutex::new(None),
            bypass_designing: Mutex::new(None),
            varispeed_error: Mutex::new(None),
            conv_warnings: Mutex::new(Vec::new()),
            varispeed_request: Mutex::new((cfg.varispeed.enabled, cfg.varispeed.speed)),
            varispeed_keep_pitch: AtomicBool::new(cfg.varispeed.keep_pitch),
        }
    }

    fn set_varispeed_request(&self, enabled: bool, speed: f64) {
        if let Ok(mut g) = self.varispeed_request.lock() {
            *g = (enabled, speed);
        }
    }

    fn set_varispeed_keep_pitch(&self, on: bool) {
        self.varispeed_keep_pitch.store(on, Ordering::Release);
    }

    fn varispeed_request(&self) -> (bool, f64) {
        self.varispeed_request
            .lock()
            .map(|g| *g)
            .unwrap_or((false, 1.0))
    }

    fn set_varispeed_error(&self, err: Option<String>) {
        if let Ok(mut g) = self.varispeed_error.lock() {
            *g = err;
        }
    }

    fn varispeed_error(&self) -> Option<String> {
        self.varispeed_error.lock().ok().and_then(|g| g.clone())
    }

    fn set_bypass_config(&self, cfg: &EngineConfig) {
        let engine = cfg.rate.bypass_resampler == BypassResampler::Engine;
        let was_engine = self.bypass_engine.swap(engine, Ordering::AcqRel);
        let was_on = self.dsp_enabled.swap(cfg.dsp_enabled, Ordering::AcqRel);
        if was_engine != engine || was_on != cfg.dsp_enabled {
            self.set_bypass_failure(None);
        }
    }

    fn set_bypass_failure(&self, msg: Option<String>) {
        if let Ok(mut g) = self.bypass_failure.lock() {
            *g = msg;
        }
    }

    fn set_bypass_designing(&self, msg: Option<String>) {
        if let Ok(mut g) = self.bypass_designing.lock() {
            *g = msg;
        }
    }

    /// The engine bypass resampler in status, from what the driver runs
    /// (`state`, whether a format is open, the sink and the device ratio)
    /// and whether its chain plays (`muted`: the resample-only mode is
    /// silent, waiting for it). `None` while no device is open.
    fn bypass_status(
        &self,
        state: EngineState,
        open: bool,
        active_sink: Option<SinkChoice>,
        ratio: RateRatio,
        muted: bool,
    ) -> Option<BypassResamplerStatus> {
        if state == EngineState::Idle || !open {
            return None;
        }
        let mode = if self.bypass_engine.load(Ordering::Acquire) {
            BypassResampler::Engine
        } else {
            BypassResampler::Pipewire
        };
        let bypass_path =
            active_sink == Some(SinkChoice::Direct) || !self.dsp_enabled.load(Ordering::Acquire);
        let away = bypass_path && !ratio.is_unity();
        let active = away && state == EngineState::Playing && !muted;
        let reason = if active || mode == BypassResampler::Pipewire || !bypass_path {
            None
        } else if away {
            Some("waiting for the engine's resampler chain".to_string())
        } else {
            self.bypass_designing
                .lock()
                .ok()
                .and_then(|g| g.clone())
                .or_else(|| self.bypass_failure.lock().ok().and_then(|g| g.clone()))
        };
        Some(BypassResamplerStatus {
            mode,
            active,
            reason,
        })
    }

    fn set_volume(&self, status: VolumeStatus) {
        if let Ok(mut g) = self.volume_status.lock()
            && g.0 != status
        {
            g.0 = status;
            g.1 += 1;
        }
    }

    fn volume_status(&self) -> VolumeStatus {
        self.volume_state().0
    }

    fn volume_state(&self) -> (VolumeStatus, u64) {
        self.volume_status
            .lock()
            .map(|g| g.clone())
            .unwrap_or_else(|_| (VolumeStatus::from_config(&Default::default()), 0))
    }

    fn set_ab(&self, enabled: bool, side: AbSide, gains: Option<AbGains>) {
        self.ab_enabled.store(enabled, Ordering::Release);
        self.ab_side.store(side_to_u8(side), Ordering::Release);
        *self.ab_gains.lock().unwrap_or_else(|e| e.into_inner()) = gains;
    }

    /// Record a chain rebuild failure that kept audio alive on a fallback
    /// chain (or in `Error`). Surfaced through the engine snapshot error.
    fn set_chain_error(&self, msg: impl Into<String>) {
        if let Ok(mut g) = self.chain_error.lock() {
            *g = Some(msg.into());
        }
    }

    fn clear_chain_error(&self) {
        if let Ok(mut g) = self.chain_error.lock() {
            *g = None;
        }
    }

    fn chain_error(&self) -> Option<String> {
        self.chain_error.lock().ok().and_then(|g| g.clone())
    }

    fn ab_status(&self) -> Option<AbStatus> {
        if !self.ab_enabled.load(Ordering::Acquire) {
            return None;
        }
        let gains = (*self.ab_gains.lock().unwrap_or_else(|e| e.into_inner()))?;
        Some(ab_status(
            u8_to_side(self.ab_side.load(Ordering::Acquire)),
            gains,
        ))
    }
}

pub struct RealEngine {
    builder: Arc<Mutex<BuilderState>>,
    /// Handed to the control plane once ([`Engine::take_sink_volume_requests`]).
    sink_requests: Mutex<Option<std::sync::mpsc::Receiver<SinkVolumeRequest>>>,
    mirror: Arc<SharedMirror>,
    meters: Arc<MeterBank>,
    spectrum: Mutex<Option<SpectrumWorker>>,
    harmonics_live: Mutex<Option<harmonics_live::HarmonicsLiveWorker>>,
    chain_generation: Arc<AtomicU64>,
    driver: EngineDriverHandle,
    device_name: Mutex<String>,
    reaper_stop: Arc<AtomicBool>,
    reaper_paused: Arc<AtomicBool>,
    rt_drain_paused: Arc<AtomicBool>,
    reaper_thread: Mutex<Option<JoinHandle<()>>>,
    /// What the varispeed stage applies (the RT thread publishes it).
    varispeed_applied: Arc<varispeed::VarispeedApplied>,
    /// The master's measurement slot (`measure_play`).
    measure_slot: Arc<measure::MeasureSlot>,
    /// Old measurement jobs an audio thread had not handed back when their
    /// run ended: the reaper drops each once it has.
    measure_retired: Arc<Mutex<Vec<Arc<measure::MeasureJob>>>>,
}

/// Varispeed status for the control plane: the applied state, the driver's
/// scheduling, `rt_load_max`, and why the last stage build was refused.
#[derive(Clone, Debug, PartialEq)]
pub struct VarispeedStatus {
    pub applied: varispeed::VarispeedAppliedSnapshot,
    pub driver: audivane_io_pw::varispeed_rt::VarispeedDriverSnapshot,
    pub rt_load_max: f64,
    pub error: Option<String>,
}

/// Turn the RT applied state, the driver's scheduler snapshot and the
/// host-accepted request into the protocol's
/// [`audivane_protocol::ipc::VarispeedStatus`].
///
/// `requested` and `enabled` come from the host-accepted request, so idle,
/// Direct, DSP-off and underfeed/overload holds never overwrite what the caller
/// asked for; `active`, `speed` and `target` come strictly from the RT-applied
/// state (`1.0` while inactive).
fn build_varispeed_status(
    applied: varispeed::VarispeedAppliedSnapshot,
    driver: audivane_io_pw::varispeed_rt::VarispeedDriverSnapshot,
    host_enabled: bool,
    host_requested: f64,
    error: Option<String>,
) -> audivane_protocol::ipc::VarispeedStatus {
    use audivane_io_pw::varispeed_rt::VarispeedHold;
    use audivane_io_pw::varispeed_sched::{CapReason, HeldReason};
    use audivane_protocol::ipc::{
        VARISPEED_MAX_SPEED, VarispeedCapReason, VarispeedHeldReason, VarispeedOverloadHold,
        VarispeedStatus as WireVarispeedStatus,
    };

    let active = applied.active;
    let speed = if active { applied.speed } else { 1.0 };
    let target = if active { applied.target } else { 1.0 };
    let requested = if host_requested.is_finite() && host_requested > 0.0 {
        host_requested
    } else {
        1.0
    };
    let speed_limit = if active && applied.speed_limit > 0.0 {
        applied.speed_limit
    } else if driver.cap > 0.0 {
        driver.cap
    } else if applied.speed_limit > 0.0 {
        applied.speed_limit
    } else {
        VARISPEED_MAX_SPEED
    };
    let cap_reason = driver.cap_reason.map(|r| match r {
        CapReason::PeriodQuantum => VarispeedCapReason::PeriodQuantum,
        CapReason::QuantumTooSmall => VarispeedCapReason::QuantumTooSmall,
        CapReason::RingSize => VarispeedCapReason::RingSize,
    });
    let held = driver.held.map(|r| match r {
        HeldReason::Underfeed => VarispeedHeldReason::Underfeed,
    });
    let overload_hold = match driver.overload_hold {
        VarispeedHold::None => VarispeedOverloadHold::None,
        VarispeedHold::Budget => VarispeedOverloadHold::Budget,
        VarispeedHold::Unity => VarispeedOverloadHold::Unity,
        VarispeedHold::Off => VarispeedOverloadHold::Off,
    };
    WireVarispeedStatus {
        enabled: host_enabled,
        keep_pitch: false,
        active,
        speed,
        target,
        requested,
        speed_limit,
        cap_reason,
        ramping: active && applied.ramping,
        fading: active && applied.fading,
        latency_frames: if active { applied.latency_frames } else { 0.0 },
        latency_exact: !active || applied.latency_exact,
        held,
        overload_hold,
        underfeed_events: driver.underfeed_events,
        underfeed_frames: driver.underfeed_frames,
        short_cycles: driver.short_cycles,
        gap_fades: applied.gap_fades,
        error,
    }
}

impl RealEngine {
    /// Enable or disable the varispeed stage at `speed` (0.5 to 2), ramping
    /// over at least `ramp_ms` (0 to 10 000; the envelope sets the shortest).
    /// Enabling builds a stage for the running graph rate (later rates get
    /// theirs at each negotiation) and is refused when the static budget does
    /// not allow it. The speed applies at the next DAC wakeup, within the cap
    /// the negotiated period and quantum sustain. Varispeed runs on the DSP
    /// sink with DSP on only; Direct and DSP off stay bit-perfect.
    ///
    /// With `keep_pitch` the stage is the key-locked stretch (tempo only):
    /// toggling while varispeed plays fades the stage out, starts the other
    /// kind and fades in (one gap, the new stage's delay), and followers
    /// re-sync. Refused where the stretch does not fit the static budget
    /// (176.4 kHz graph rates and above).
    #[doc(hidden)]
    pub fn set_varispeed(
        &self,
        cfg: audivane_protocol::ipc::VarispeedConfig,
    ) -> Result<(), EngineError> {
        {
            let mut st = self.builder.lock().map_err(|_| {
                EngineError::new(RpcError::INTERNAL, "engine builder lock poisoned")
            })?;
            let waker = self.driver.waker();
            st.set_varispeed_cfg(cfg, &self.mirror, Some(&waker))?;
        }
        self.driver.set_varispeed_speed(cfg.speed, cfg.ramp_ms);
        self.driver.waker().wake_rt();
        Ok(())
    }

    /// Varispeed's status ([`VarispeedStatus`]).
    #[doc(hidden)]
    pub fn varispeed_status(&self) -> VarispeedStatus {
        let snap = self.driver.snapshot();
        VarispeedStatus {
            applied: self.varispeed_applied.snapshot(),
            driver: snap.varispeed,
            rt_load_max: snap.rt_load_max,
            error: self
                .builder
                .lock()
                .ok()
                .and_then(|st| st.varispeed.last_error.clone()),
        }
    }

    /// Multi-device (internal; not in the protocol yet): play the master's
    /// source on `device` too, in sync, with its own chain built from the
    /// master's config. At most three, never the master's device.
    #[doc(hidden)]
    pub fn add_follower(
        &self,
        device: &str,
        opts: follower::FollowerOptions,
    ) -> Result<(), EngineError> {
        let mut st = self
            .builder
            .lock()
            .map_err(|_| EngineError::new(RpcError::INTERNAL, "engine builder lock poisoned"))?;
        let st = &mut *st;
        let master = st.cfg.device.clone();
        st.followers
            .add(&mut st.tee, device, &master, &st.cfg, opts)
            .map_err(|e| EngineError::new(RpcError::INVALID_PARAMS, e))
    }

    /// Stop the follower on `device` (and the master's alignment with the
    /// last one).
    #[doc(hidden)]
    pub fn remove_follower(&self, device: &str) -> Result<(), EngineError> {
        let mut st = self
            .builder
            .lock()
            .map_err(|_| EngineError::new(RpcError::INTERNAL, "engine builder lock poisoned"))?;
        let st = &mut *st;
        st.followers
            .remove(&mut st.tee, device)
            .map_err(|e| EngineError::new(RpcError::INTERNAL, e))?;
        if st.followers.is_empty() {
            st.align.reset();
        }
        Ok(())
    }

    /// A follower's trim (ms, ±50), volume (dB) and whether it may be heard.
    #[doc(hidden)]
    pub fn set_follower(
        &self,
        device: &str,
        trim_ms: Option<f64>,
        volume_db: Option<f64>,
        enabled: Option<bool>,
    ) -> Result<(), EngineError> {
        let st = self
            .builder
            .lock()
            .map_err(|_| EngineError::new(RpcError::INTERNAL, "engine builder lock poisoned"))?;
        let c = st.followers.controls(device).ok_or_else(|| {
            EngineError::new(
                RpcError::INVALID_PARAMS,
                format!("{device}: not a follower"),
            )
        })?;
        if let Some(t) = trim_ms {
            if !t.is_finite() || t.abs() > follower::MAX_TRIM_MS {
                return Err(EngineError::new(
                    RpcError::INVALID_PARAMS,
                    format!("trim {t} ms outside ±{}", follower::MAX_TRIM_MS),
                ));
            }
            c.set_trim_ms(t);
        }
        if let Some(v) = volume_db {
            c.set_volume_db(v);
        }
        if let Some(e) = enabled {
            c.set_enabled(e);
        }
        Ok(())
    }

    /// Every follower's status, by device, and the master's alignment (ms).
    #[doc(hidden)]
    pub fn follower_status(&self) -> (Vec<(String, follower::FollowerSnapshot)>, f64) {
        match self.builder.lock() {
            Ok(st) => (st.followers.status(), st.align.ms),
            Err(_) => (Vec::new(), 0.0),
        }
    }

    fn leave_hardware_safe(&self) {
        match self.builder.lock() {
            Ok(mut st) => st.leave_hardware_safe(),
            Err(_) => log::warn!("volume: builder lock poisoned, hardware level not lowered"),
        }
    }

    /// Test hook: pause or resume dropping recycled chains off-RT.
    #[doc(hidden)]
    pub fn set_reaper_paused(&self, paused: bool) {
        self.reaper_paused.store(paused, Ordering::Release);
    }

    /// Test hook: pause or resume draining `swap_rx` on the ALSA RT thread.
    #[doc(hidden)]
    pub fn set_rt_drain_paused(&self, paused: bool) {
        self.rt_drain_paused.store(paused, Ordering::Release);
        if !paused {
            self.driver.wake_rt();
        }
    }

    /// Start the engine with `cfg`: validate config/IRs, reserve and open the
    /// ALSA device, register the PipeWire sinks, and start the RT threads.
    /// Returns once both PipeWire sinks are registered.
    pub fn start(cfg: &EngineConfig) -> Result<Arc<RealEngine>, EngineError> {
        // Measure the CPU for the upsampler admission check meanwhile.
        crate::control::cpu_budget::warm();
        let mut ir_cache = IrCache::new();
        let init_rate = cfg.rate.default_rate.max(8_000);
        let conv_warnings = validate_engine_config(cfg, &mut ir_cache, init_rate)?;

        let desired_ratio = desired_ratio_for(cfg, init_rate);
        let init_max_block = scaled_quantum(cfg.quantum, init_rate).clamp(512, 8192) as usize;

        let (cmd_tx, cmd_rx) = rtrb::RingBuffer::<RtDspCmd>::new(64);
        let (swap_tx, swap_rx) = rtrb::RingBuffer::<PendingSwap>::new(64);
        let (recycle_tx, mut recycle_rx) = rtrb::RingBuffer::<ActiveChain>::new(64);
        let (vs_tx, vs_rx) = rtrb::RingBuffer::<varispeed::VarispeedRtCmd>::new(8);
        let (vs_recycle_tx, mut vs_recycle_rx) =
            rtrb::RingBuffer::<Box<audivane_dsp_core::varispeed::Varispeed>>::new(8);
        let (stretch_recycle_tx, mut stretch_recycle_rx) =
            rtrb::RingBuffer::<Box<audivane_dsp_core::tempo::TempoStretch>>::new(8);

        let mirror = Arc::new(SharedMirror::new(cfg));
        *mirror
            .conv_warnings
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = conv_warnings;
        let meters = MeterBank::new(DEFAULT_CHANNELS);
        let (spectrum_rt, spectrum_worker) = spectrum::SpectrumRt::create(DEFAULT_CHANNELS)
            .map_err(|e| {
                EngineError::new(
                    RpcError::INTERNAL,
                    format!("cannot start spectrum analyser: {e}"),
                )
            })?;

        let (live_rt, live_worker) = harmonics_live::create().map_err(|e| {
            EngineError::new(
                RpcError::INTERNAL,
                format!("cannot start the harmonics live view: {e}"),
            )
        })?;
        live_worker.set_config(
            cfg.chain.harmonics.clone(),
            cfg.ab.as_ref().and_then(|ab| ab.chain_b.harmonics.clone()),
        );

        let ab_side_atomic = Arc::new(AtomicU8::new(0));
        let measure_slot = Arc::new(measure::MeasureSlot::default());
        let processor = DaemonProcessor::new(
            ActiveChain::Bypass,
            cfg.dsp_enabled,
            DEFAULT_CHANNELS,
            init_max_block,
            MAX_DSP_RATIO.max(desired_ratio.up.div_ceil(desired_ratio.down) as usize),
            init_rate,
            Arc::clone(&ab_side_atomic),
            Arc::clone(&meters),
            cmd_rx,
            swap_rx,
            recycle_tx,
        )
        .with_spectrum(spectrum_rt)
        .with_harmonics_live(live_rt)
        .with_initial_volume(initial_volume_target(cfg))
        .with_varispeed(vs_rx, vs_recycle_tx, DEFAULT_CHANNELS)
        .with_stretch_recycle(stretch_recycle_tx)
        .with_measure(Arc::clone(&measure_slot), DEFAULT_CHANNELS);
        let varispeed_applied = processor.varispeed_applied();
        if std::env::var_os("AUDIVANE_TEST_PAUSE_RT_DRAIN").is_some_and(|v| v == "1" || v == "true")
        {
            processor.set_swap_drain_paused(true);
        }
        let shared_volume = processor.volume_handle();
        let dac_bits = processor.dac_bits_handle();
        let chain_generation = processor.active_generation_handle();
        let swap_state = processor.swap_state_handle();
        let rt_drain_paused = processor.swap_drain_paused_handle();
        let has_pending_swap = Arc::new(AtomicBool::new(false));

        let mut vs_host = varispeed::VarispeedHost::new(vs_tx, DEFAULT_CHANNELS);
        let (tee_host, tee_rt) = tee::TeeHost::pair(DEFAULT_CHANNELS);
        // The followers' level until the first apply: the master's planned one.
        tee_host
            .shared()
            .master_level
            .store(initial_volume_target(cfg));
        let processor = processor.with_tee(tee_rt);
        // Not enabled yet: only the kind is set, nothing is built.
        let _ = vs_host.set_keep_pitch(cfg.varispeed.keep_pitch, init_rate, Ok(()));
        if cfg.varispeed.enabled {
            let budget = crate::control::config::check_stage_budget_for(
                cfg,
                None,
                None,
                init_rate,
                desired_ratio,
                cfg.varispeed.keep_pitch,
            );
            if let Err(e) = vs_host.set_enabled(true, init_rate, budget) {
                log::warn!("varispeed: off at {init_rate} Hz: {e}");
            }
            mirror.set_varispeed_error(vs_host.last_error.clone());
        }

        // Not holding the DAC until the first negotiation (the driver's
        // start below): a crash before it leaves the card alone.
        failsafe::engine_starting(failsafe::released_path().as_deref());
        let rate_plan = Arc::new(Mutex::new(seed_rate_plan(cfg)));
        let builder = Arc::new(Mutex::new(BuilderState {
            cfg: cfg.clone(),
            ir_cache,
            ab_side: AbSide::A,
            ab_gains: None,
            last_rate: init_rate,
            last_ratio: desired_ratio,
            last_dsp_ratio: desired_ratio,
            last_built_resample: false,
            out_format: None,
            negotiated_key: None,
            config_generation: 0,
            built_generation: 0,
            next_swap_seq: 0,
            swap_state,
            in_flight_cfgs: Vec::new(),
            varispeed_applied: Arc::clone(&varispeed_applied),
            pending_publication: None,
            has_pending_swap: Arc::clone(&has_pending_swap),
            shared_volume,
            mixer: Arc::new(apply_alsa_mixer),
            software_db: initial_volume_target(cfg).volume_db,
            hw_known: None,
            hw_known_device: None,
            pending_hw: None,
            output_latency: Arc::new(|| DEFAULT_OUTPUT_LATENCY),
            digital_peak: None,
            driver_error: false,
            driver_state: Arc::new(|| EngineState::Ready),
            failsafe_mutes: failsafe::MuteStore::load(failsafe::mutes_path()),
            switch_state: Arc::new(|device: &str| playback_switch(device).ok().flatten()),
            card_state: Arc::new(|device: &str| CardRead::from_alsa(playback_channels(device))),
            failsafe_mute_verified: false,
            released_marker: failsafe::released_path(),
            dac_bits,
            dac_held: false,
            clock: Arc::new(Instant::now),
            sink_port: None,
            cmd_tx,
            swap_tx,
            designer: bypass_design::BypassDesigner::new(bypass_chain_builder()),
            pending_resample: None,
            designing: None,
            bypass_notify: None,
            negotiation_seq: 0,
            design_epoch: 0,
            design_requests: Vec::new(),
            varispeed: vs_host,
            varispeed_speed: cfg.varispeed.speed,
            varispeed_requests: 0,
            varispeed_hold: Default::default(),
            varispeed_hold_seq: 0,
            tee: tee_host,
            followers: follower_host(Arc::clone(&rate_plan)),
            align: follower::AlignControl::default(),
            comp: follower::CompControl::default(),
            align_t0: Instant::now(),
            rate_plan,
            built_substitute: None,
        }));

        let builder_cb = Arc::clone(&builder);
        let mirror_cb = Arc::clone(&mirror);
        let ab_atomic_cb = Arc::clone(&ab_side_atomic);
        let waker_slot: Arc<OnceLock<DriverWaker>> = Arc::new(OnceLock::new());
        let waker_cb = Arc::clone(&waker_slot);
        // A finished design: the driver hears of it (a pair that fell back
        // to PipeWire while it ran comes back), and an edit waiting for it
        // gets its chain.
        if let Ok(st) = builder.lock() {
            let weak = Arc::downgrade(&builder);
            let mirror_d = Arc::clone(&mirror);
            let waker_d = Arc::clone(&waker_slot);
            st.designer
                .set_on_done(Arc::new(move |key: &bypass_design::DesignKey, ok| {
                    if let Some(b) = weak.upgrade()
                        && let Ok(mut st) = b.lock()
                    {
                        st.designed(key, ok, &mirror_d, waker_d.get());
                    }
                }));
        }
        let on_negotiated: RatioNegotiator = Arc::new(
            move |device, graph_rate, actual_ratio, active_sink, format, key| {
                let Ok(mut st) = builder_cb.lock() else {
                    return NegotiatedChain {
                        required_chain_seq: None,
                        error: Some("engine builder lock poisoned".into()),
                        fatal: true,
                        generation: 0,
                        superseded: false,
                        bypass_unbuildable: false,
                        bypass_pending: false,
                        bypass_epoch: 0,
                    };
                };
                st.negotiated_on(
                    device,
                    Some(*key),
                    graph_rate,
                    actual_ratio,
                    active_sink,
                    format,
                    &mirror_cb,
                    &ab_atomic_cb,
                    waker_cb.get(),
                )
            },
        );

        // Installed before the first negotiation (inside `spawn`), which may
        // write the mixer: a start-up failing after it still hands the card
        // over.
        let builder_rel = Arc::clone(&builder);
        let before_release: audivane_io_pw::ReleaseHook =
            Arc::new(move |kind, device| match builder_rel.lock() {
                Ok(mut st) => st.on_release_of(kind, device),
                Err(_) => log::warn!("volume: builder lock poisoned, hardware level not lowered"),
            });
        let drv_cfg = to_driver_config(cfg);
        let driver = EngineDriverHandle::spawn(
            drv_cfg,
            processor,
            Some(on_negotiated),
            Some(before_release),
        )
        .map_err(|e| EngineError::new(RpcError::IO, format!("driver start failed: {e}")))?;
        driver.set_varispeed_speed(cfg.varispeed.speed, cfg.varispeed.ramp_ms);
        let _ = waker_slot.set(driver.waker());
        let sink_requests = driver.sink_volume_requests();
        if let Ok(mut st) = builder.lock() {
            let probe = driver.output_latency_probe();
            st.output_latency = Arc::new(move || probe().unwrap_or(DEFAULT_OUTPUT_LATENCY));
            st.driver_state = Arc::new(driver.state_probe());
            st.sink_port = Some(driver.sink_volume_port());
            let notifier = driver.bypass_notifier();
            st.bypass_notify = Some(Arc::new(move |note| notifier.send(note)));
            let volume_db = mirror.volume_status().volume_db;
            st.reflect_sink_volume(volume_db);
            // The configured followers start with the master.
            st.sync_followers();
        }

        let reaper_stop = Arc::new(AtomicBool::new(false));
        let reaper_paused = Arc::new(AtomicBool::new(
            std::env::var_os("AUDIVANE_TEST_PAUSE_REAPER").is_some_and(|v| v == "1" || v == "true"),
        ));
        let reaper_stop_flag = Arc::clone(&reaper_stop);
        let reaper_paused_flag = Arc::clone(&reaper_paused);
        let reaper_waker = driver.waker();
        let builder_reaper = Arc::clone(&builder);
        let mirror_reaper = Arc::clone(&mirror);
        let measure_retired: Arc<Mutex<Vec<Arc<measure::MeasureJob>>>> = Arc::default();
        let retired_reaper = Arc::clone(&measure_retired);
        let measure_slot_reaper = Arc::clone(&measure_slot);
        let reaper_thread = thread::Builder::new()
            .name("audivane-chain-reaper".to_string())
            .spawn(move || {
                while !reaper_stop_flag.load(Ordering::Acquire) {
                    let mut woke_rt = false;
                    if !reaper_paused_flag.load(Ordering::Acquire) {
                        let mut reaped = false;
                        while let Ok(old_chain) = recycle_rx.pop() {
                            drop(old_chain);
                            reaped = true;
                        }
                        while let Ok(old_stage) = vs_recycle_rx.pop() {
                            drop(old_stage);
                            reaped = true;
                        }
                        while let Ok(old_stage) = stretch_recycle_rx.pop() {
                            drop(old_stage);
                            reaped = true;
                        }
                        if reaped {
                            reaper_waker.wake_rt();
                            woke_rt = true;
                        }
                    }
                    if let Ok(mut st) = builder_reaper.try_lock() {
                        st.run_pending_hw(Instant::now(), &mirror_reaper, Some(&reaper_waker));
                        st.reconcile_varispeed(&mirror_reaper);
                        st.tee.reap();
                        st.tick_followers();
                        if let Ok(mut r) = retired_reaper.try_lock()
                            && !r.is_empty()
                        {
                            measure_slot_reaper.reclaim();
                            st.followers.reclaim_measure();
                            r.retain(|j| Arc::strong_count(j) > 1);
                        }
                    }
                    let pending_now = has_pending_swap.load(Ordering::Acquire);
                    if pending_now {
                        if let Ok(mut st) = builder_reaper.try_lock()
                            && st.flush_pending_swap(&mirror_reaper, Some(&reaper_waker))
                        {
                            woke_rt = true;
                        }
                        if !woke_rt {
                            reaper_waker.wake_rt();
                        }
                    }
                    thread::sleep(Duration::from_millis(if pending_now { 5 } else { 25 }));
                }
                while let Ok(old_chain) = recycle_rx.pop() {
                    drop(old_chain);
                }
                while let Ok(old_stage) = vs_recycle_rx.pop() {
                    drop(old_stage);
                }
                while let Ok(old_stage) = stretch_recycle_rx.pop() {
                    drop(old_stage);
                }
            })
            .map_err(|e| {
                driver.stop();
                EngineError::new(
                    RpcError::INTERNAL,
                    format!("spawn reaper thread failed: {e}"),
                )
            })?;

        Ok(Arc::new(Self {
            sink_requests: Mutex::new(Some(sink_requests)),
            builder,
            mirror,
            meters,
            spectrum: Mutex::new(Some(spectrum_worker)),
            harmonics_live: Mutex::new(Some(live_worker)),
            chain_generation,
            driver,
            device_name: Mutex::new(cfg.device.clone()),
            reaper_stop,
            reaper_paused,
            rt_drain_paused,
            varispeed_applied,
            reaper_thread: Mutex::new(Some(reaper_thread)),
            measure_slot,
            measure_retired,
        }))
    }
}

impl Engine for RealEngine {
    fn load_conv(
        &self,
        convs: &[audivane_protocol::ConvolutionConfig],
        rate: u32,
    ) -> Result<Vec<audivane_dsp_core::matrix::LoadedConv>, EngineError> {
        let mut st = self
            .builder
            .lock()
            .map_err(|_| EngineError::new(RpcError::INTERNAL, "engine builder lock poisoned"))?;
        load_conv_items(convs, &mut st.ir_cache, rate)
    }

    fn conv_warnings(&self) -> Vec<audivane_protocol::ipc::ConvWarning> {
        self.mirror
            .conv_warnings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn take_sink_volume_requests(&self) -> Option<std::sync::mpsc::Receiver<SinkVolumeRequest>> {
        self.sink_requests.lock().ok()?.take()
    }

    fn volume_state(&self) -> (Option<VolumeStatus>, u64) {
        let (status, generation) = self.mirror.volume_state();
        (Some(status), generation)
    }

    fn refresh_sink_volume(&self) {
        let volume_db = self.mirror.volume_status().volume_db;
        if let Ok(st) = self.builder.lock() {
            st.reflect_sink_volume(volume_db);
        }
    }

    fn apply(&self, cfg: &EngineConfig) -> Result<(), EngineError> {
        let mut st = self
            .builder
            .lock()
            .map_err(|_| EngineError::new(RpcError::INTERNAL, "engine builder lock poisoned"))?;
        let drv_snap = self.driver.snapshot();
        let ab_changed = st.cfg.ab != cfg.ab;
        let force_reopen = st.apply_config(
            cfg,
            &self.mirror,
            drv_snap.state,
            drv_snap.graph_rate,
            drv_snap.active_sink,
            Some(&self.driver),
        )?;

        log::debug!(
            "engine apply: gen={} built_gen={} force_reopen={force_reopen} rate={} dsp_enabled={} target={:?}",
            st.config_generation,
            st.built_generation,
            drv_snap.graph_rate,
            cfg.dsp_enabled,
            cfg.chain.upsampler.target
        );

        if let Ok(slot) = self.harmonics_live.lock()
            && let Some(live) = slot.as_ref()
        {
            live.set_config(
                cfg.chain.harmonics.clone(),
                cfg.ab.as_ref().and_then(|ab| ab.chain_b.harmonics.clone()),
            );
        }
        if ab_changed {
            // A new comparison counts its overs from zero.
            self.meters.reset_ab_overs();
        }
        if let Ok(mut dev) = self.device_name.lock() {
            *dev = cfg.device.clone();
        }
        let drv_cfg = to_driver_config(cfg);
        self.driver.update_config(drv_cfg, force_reopen);
        drop(st);
        Ok(())
    }

    fn set_ab_measure(&self, epoch: u32) {
        self.meters.set_ab_measure_epoch(epoch);
    }

    fn take_ab_hops(&self, out: &mut Vec<audivane_dsp_core::ab_measure::AbHop>) {
        self.meters.take_ab_hops(out);
    }

    fn ab_select(&self, side: AbSide, crossfade_ms: f64) -> Result<AbStatus, EngineError> {
        if !crossfade_ms.is_finite() || crossfade_ms < 0.0 {
            return Err(EngineError::new(
                RpcError::INVALID_PARAMS,
                "crossfade_ms must be finite and >= 0",
            ));
        }

        let mut st = self
            .builder
            .lock()
            .map_err(|_| EngineError::new(RpcError::INTERNAL, "engine builder lock poisoned"))?;
        if st.cfg.ab.is_none() {
            return Err(EngineError::new(
                RpcError::BAD_STATE,
                "A/B is not configured",
            ));
        }
        st.ab_side = side;
        let gains = st.ab_gains;
        self.mirror.set_ab(true, side, gains);
        // The pair restarts its own counts on the select.
        self.meters.reset_ab_overs();
        st.push_cmd(
            RtDspCmd::AbSelect { side, crossfade_ms },
            Some(&self.driver),
        )?;

        Ok(ab_status(side, gains.unwrap_or_default()))
    }

    fn snapshot(&self) -> EngineSnapshot {
        let drv = self.driver.snapshot();
        let device = self
            .device_name
            .lock()
            .map(|d| d.clone())
            .unwrap_or_default();

        // The convolution runs at the graph rate: its delay counts there.
        let conv_latency_ms = if drv.state != EngineState::Idle && drv.graph_rate > 0 {
            drv.dsp_conv_graph_frames as f64 * 1000.0 / f64::from(drv.graph_rate)
        } else {
            0.0
        };
        let (latency_ms, filter_latency_ms) =
            if drv.state != EngineState::Idle && drv.output_rate > 0 {
                let ms_per_frame = 1000.0 / (drv.output_rate as f64);
                // The multi-device alignment is in the total, not filter
                // delay.
                let dsp = drv.dsp_latency_out_frames as f64;
                let filter = drv
                    .dsp_latency_out_frames
                    .saturating_sub(drv.dsp_align_out_frames) as f64;
                let total_out_frames = (drv.alsa_delay_frames.max(0) as f64) + dsp;
                (total_out_frames * ms_per_frame, filter * ms_per_frame)
            } else {
                (0.0, 0.0)
            };

        // The digital volume part changes samples on every path (Direct and
        // bypass included), so it breaks bit-perfect output everywhere.
        let digital = self.mirror.digital_gain.load(Ordering::Acquire);
        let bypass_resampler = self.mirror.bypass_status(
            drv.state,
            drv.format.is_some(),
            drv.active_sink,
            drv.ratio,
            self.meters.bypass_muted(),
        );
        let vs_applied = self.varispeed_applied.snapshot();
        let dsp_active = dsp_active(
            drv.state,
            digital,
            bypass_resampler.as_ref().is_some_and(|b| b.active),
            drv.active_sink,
            self.mirror.dsp_has_effect.load(Ordering::Acquire) || vs_applied.active,
        );
        let (vs_enabled, vs_requested) = self.mirror.varispeed_request();
        let mut varispeed = build_varispeed_status(
            vs_applied,
            drv.varispeed,
            vs_enabled,
            vs_requested,
            self.mirror.varispeed_error(),
        );
        varispeed.keep_pitch = self.mirror.varispeed_keep_pitch.load(Ordering::Acquire);
        let varispeed = Some(varispeed);
        EngineSnapshot {
            state: drv.state,
            device,
            error: drv
                .error
                .clone()
                .or_else(|| self.mirror.chain_error())
                .or_else(|| bypass_muted_error(&self.meters, &drv)),
            format: drv.format,
            graph_rate: drv.graph_rate,
            output_rate: drv.output_rate,
            ratio: drv.ratio,
            quantum: drv.quantum,
            latency_ms,
            filter_latency_ms,
            conv_latency_ms,
            xruns: drv.xruns,
            dsp_load: drv.dsp_load,
            dsp_active,
            active_sink: drv.active_sink,
            ab: self.mirror.ab_status().map(|mut ab| {
                let [a, b] = self.meters.ab_overs();
                ab.overs = audivane_protocol::ipc::AbOvers { a, b };
                ab
            }),
            volume: Some(self.mirror.volume_status()),
            dsp_node: drv.dsp_node,
            direct_node: drv.direct_node,
            frames_out: drv.frames_out,
            chain_generation: self.chain_generation.load(Ordering::Acquire),
            gaps: drv.gaps,
            dither_bits: drv.format.map(|f| {
                effective_dither_bits(self.mirror.output_bits.load(Ordering::Acquire), Some(f))
            }),
            effective_rates: drv
                .effective_policy
                .as_ref()
                .map(|p| p.allowed_rates.clone())
                .unwrap_or_default(),
            effective_default_rate: drv.effective_policy.as_ref().map(|p| p.default_rate),
            direct_rates: drv.direct_rates,
            playable_rates: drv.playable_rates,
            device_caps: drv.device_caps,
            bypass_resampler,
            rt_load_max: drv.rt_load_max,
            varispeed,
            waiting_for_device: drv.waiting_for_device,
        }
    }

    fn meters(&self) -> Meters {
        let drv = self.driver.snapshot();
        self.meters.read_and_reset(drv.xruns, drv.dsp_load)
    }

    fn limiter_held_db(&self) -> f64 {
        self.meters.limiter_held_db()
    }

    fn spectrum(&self) -> Option<Spectrum> {
        self.spectrum
            .lock()
            .ok()
            .and_then(|slot| slot.as_ref().and_then(SpectrumWorker::latest))
    }

    fn spectrum_generation(&self) -> u64 {
        self.spectrum
            .lock()
            .ok()
            .and_then(|slot| slot.as_ref().map(SpectrumWorker::generation))
            .unwrap_or(0)
    }

    fn harmonics_live(&self) -> Option<audivane_protocol::ipc::HarmonicsLive> {
        self.harmonics_live
            .lock()
            .ok()
            .and_then(|slot| slot.as_ref().map(harmonics_live::HarmonicsLiveWorker::poll))
    }

    fn harmonics_live_suspend(&self) {
        if let Ok(slot) = self.harmonics_live.lock()
            && let Some(worker) = slot.as_ref()
        {
            worker.suspend();
        }
    }

    fn set_spectrum_enabled(&self, enabled: bool) {
        if let Ok(slot) = self.spectrum.lock()
            && let Some(spectrum) = slot.as_ref()
        {
            spectrum.set_enabled(enabled);
        }
    }

    fn selftest(&self) -> Result<SelfTestResult, EngineError> {
        let drv = self.driver.snapshot();
        if matches!(drv.state, EngineState::Idle | EngineState::Error) {
            return Err(EngineError::new(
                RpcError::BAD_STATE,
                format!("cannot run self-test while engine is {:?}", drv.state),
            ));
        }

        // Build deterministic 24-bit stereo test vector (1024 frames = 2048 samples).
        const TEST_FRAMES: usize = 1024;
        let total_samples = TEST_FRAMES * DEFAULT_CHANNELS;
        let mut expected_i32 = Vec::with_capacity(total_samples);
        let mask = if drv.format == Some(SampleFormat::S16) {
            !0xffff_i32
        } else {
            !0_i32
        };

        let mut rng: u64 = 0x517c_c1b7_2722_0a95;
        for _ in 0..total_samples {
            rng = rng
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let s24 = ((rng >> 40) as i32) - (1 << 23);
            let left = s24 << 8;
            expected_i32.push(left);
        }

        let mut input_f32 = vec![0.0_f32; total_samples];
        i32_to_f32(&expected_i32, &mut input_f32);
        for s in &mut expected_i32 {
            *s &= mask;
        }
        let expected_u64 = hash_i32(&expected_i32);

        let job = SelfTestJob::new(input_f32, drv.format);
        {
            let mut st = self.builder.lock().map_err(|_| {
                EngineError::new(RpcError::INTERNAL, "engine builder lock poisoned")
            })?;
            st.push_cmd(RtDspCmd::RunSelfTest(Arc::clone(&job)), Some(&self.driver))?;
        }

        let deadline = Instant::now() + Duration::from_secs(2);
        while !job.done.load(Ordering::Acquire) {
            if Instant::now() >= deadline {
                return Err(EngineError::new(
                    RpcError::IO,
                    "self-test timed out waiting for RT thread",
                ));
            }
            self.driver.wake_rt();
            thread::sleep(Duration::from_millis(2));
        }

        let actual_u64 = job.actual_hash.load(Ordering::Acquire);
        let frames = job.actual_frames.load(Ordering::Acquire);
        let passed = actual_u64 == expected_u64 && frames == TEST_FRAMES as u64;
        let expected_hash = format!("{expected_u64:016x}");
        let actual_hash = format!("{actual_u64:016x}");
        let detail = if passed {
            format!(
                "{frames} stereo frames verified bit-exact at {} Hz ({:?})",
                drv.graph_rate, drv.format
            )
        } else {
            format!(
                "hash mismatch (expected {expected_hash}, got {actual_hash}) for {frames} frames"
            )
        };

        Ok(SelfTestResult {
            passed,
            frames,
            rate: drv.graph_rate,
            format: drv.format,
            expected_hash,
            actual_hash,
            detail,
        })
    }

    fn release_device(&self) -> Result<(), EngineError> {
        self.driver
            .release_device()
            .map_err(|e| EngineError::new(RpcError::IO, format!("release_device failed: {e}")))
        // The driver's release hook lowered the hardware before handing
        // the device over.
    }

    fn acquire_device(&self) -> Result<(), EngineError> {
        self.driver
            .acquire_device()
            .map_err(|e| EngineError::new(RpcError::IO, format!("acquire_device failed: {e}")))
    }

    fn cpu_rates(&self) -> Option<&'static audivane_dsp_core::cpu_bench::KernelRates> {
        Some(crate::control::cpu_budget::machine())
    }

    fn applied_generation(&self) -> u64 {
        self.mirror.config_generation.load(Ordering::Acquire)
    }

    fn set_rate_plan(&self, plan: crate::api::RatePlan) {
        if let Ok(mut st) = self.builder.lock() {
            let old = {
                let mut guard = st.rate_plan.lock().unwrap_or_else(|e| e.into_inner());
                std::mem::replace(&mut *guard, plan.clone())
            };
            let cfg = st.cfg.clone();
            st.followers.rebuild_for_rate_plan(&cfg, &old, &plan);
        }
    }

    fn set_varispeed_hold(&self, hold: crate::api::VarispeedHold) -> u64 {
        let Ok(mut st) = self.builder.lock() else {
            log::warn!("varispeed: builder lock poisoned, overload hold not applied");
            return 0;
        };
        // The same hold again: done, unless its install or retirement is
        // still waiting for room in the RT queue (sent again here, and by
        // the reaper).
        if st.varispeed_hold == hold && !st.varispeed.pending() {
            return st.varispeed_hold_seq;
        }
        st.varispeed_hold = hold;
        st.varispeed_hold_seq = self.driver.set_varispeed_hold(hold);
        let (rate, ratio) = (st.last_rate, st.last_ratio);
        let cfg_now = st.cfg.clone();
        let budget = st.check_varispeed_budget_in_flight(&cfg_now, None, rate, ratio);
        let off = hold == crate::api::VarispeedHold::Off;
        if let Err(e) = st.varispeed.set_held_off(off, rate, budget) {
            log::warn!("varispeed: overload hold {}: {e}", hold.as_str());
        }
        self.mirror
            .set_varispeed_error(st.varispeed.last_error.clone());
        st.varispeed_hold_seq
    }

    fn set_varispeed(
        &self,
        cfg: audivane_protocol::ipc::VarispeedConfig,
    ) -> Result<(), EngineError> {
        RealEngine::set_varispeed(self, cfg)
    }

    fn varispeed_request(&self) -> crate::api::VarispeedRequest {
        let driver = self.driver.snapshot().varispeed;
        let applied = self.varispeed_applied.snapshot();
        let Ok(st) = self.builder.lock() else {
            return crate::api::VarispeedRequest::default();
        };
        let retired = applied.phase == 0 && applied.stage_rate == 0 && !st.varispeed.pending();
        let hold_ack = varispeed_hold_ack(
            st.varispeed_hold,
            st.varispeed_hold_seq,
            driver.hold_ack,
            retired,
        );
        crate::api::VarispeedRequest {
            wanted: st.varispeed.requested(),
            running: applied.active,
            speed: st.varispeed_speed,
            budget: driver.proc_budget,
            requests: st.varispeed_requests,
            hold_ack,
        }
    }

    fn needs_restart(&self) -> bool {
        self.driver.audio_thread_ended()
    }

    fn outputs(&self) -> Option<OutputsStatus> {
        self.builder.lock().ok().map(|st| st.outputs_status())
    }

    fn measure_post(&self, job: Option<Arc<measure::MeasureJob>>) -> Result<(), EngineError> {
        let st = self
            .builder
            .lock()
            .map_err(|_| EngineError::new(RpcError::INTERNAL, "engine builder lock poisoned"))?;
        match job {
            Some(job) => {
                if let Some(want) = &job.master_device {
                    let now = self
                        .device_name
                        .lock()
                        .map(|d| d.clone())
                        .unwrap_or_default();
                    if *want != now {
                        return Err(EngineError::new(
                            RpcError::BAD_STATE,
                            format!("the main device changed ({want} to {now})"),
                        ));
                    }
                }
                for d in &job.devices {
                    if let Some(f) = &d.follower
                        && st.followers.controls(f).is_none()
                    {
                        return Err(EngineError::new(
                            RpcError::BAD_STATE,
                            format!("{f} is not a running follower"),
                        ));
                    }
                }
                let master = job
                    .devices
                    .iter()
                    .position(|d| d.follower.is_none())
                    .unwrap_or(measure::NO_DEVICE);
                for (i, d) in job.devices.iter().enumerate() {
                    if let Some(f) = &d.follower
                        && let Some(c) = st.followers.controls(f)
                    {
                        c.measure_slot().post(Some((Arc::clone(&job), i)));
                    }
                }
                // The master last: it arms and picks the start, which the
                // followers then find.
                self.measure_slot.post(Some((job, master)));
            }
            None => {
                self.measure_slot.post(None);
                for (device, _) in st.followers.status() {
                    if let Some(c) = st.followers.controls(&device) {
                        c.measure_slot().post(None);
                    }
                }
            }
        }
        drop(st);
        self.driver.wake_rt();
        Ok(())
    }

    fn measure_reclaim(&self) {
        self.measure_slot.reclaim();
        if let Ok(st) = self.builder.lock() {
            st.followers.reclaim_measure();
        }
    }

    fn measure_retire(&self, job: Arc<measure::MeasureJob>) {
        self.measure_retired
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(job);
    }

    fn shutdown(&self) {
        if let Ok(mut st) = self.builder.lock() {
            let st = &mut *st;
            st.followers.stop_all(&mut st.tee);
        }
        self.driver.stop();
        self.leave_hardware_safe();
        self.reaper_stop.store(true, Ordering::Release);
        if let Ok(mut slot) = self.reaper_thread.lock()
            && let Some(h) = slot.take()
        {
            let _ = h.join();
        }
        if let Ok(mut slot) = self.spectrum.lock()
            && let Some(mut spectrum) = slot.take()
        {
            spectrum.stop();
        }
        if let Ok(mut slot) = self.harmonics_live.lock()
            && let Some(mut live) = slot.take()
        {
            live.stop();
        }
    }
}

impl Drop for RealEngine {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ---------------------------------------------------------------- Helpers

fn side_to_u8(side: AbSide) -> u8 {
    match side {
        AbSide::A => 0,
        AbSide::B => 1,
    }
}

fn u8_to_side(v: u8) -> AbSide {
    match v {
        1 => AbSide::B,
        _ => AbSide::A,
    }
}

/// Whether `sink` (the DSP sink when unknown) plays the engine bypass
/// resampler's chain at `ratio`: Direct or DSP off away from x1 (the
/// processor's [`processor::PlayMode::ResampleOnly`]).
fn plays_resample_only(cfg: &EngineConfig, sink: Option<SinkChoice>, ratio: RateRatio) -> bool {
    (sink == Some(SinkChoice::Direct) || !cfg.dsp_enabled) && !ratio.is_unity()
}

/// [`build_resample_chain`] for an output section and quantum.
fn build_resample_chain_for(
    output: audivane_protocol::OutputConfig,
    quantum: u32,
    graph_rate: u32,
    ratio: RateRatio,
) -> Result<ActiveChain, EngineError> {
    crate::control::config::check_bypass_design(&output, graph_rate, ratio)
        .map_err(|e| EngineError::new(RpcError::INVALID_PARAMS, e))?;
    let config = crate::control::config::bypass_chain_config(output, graph_rate, ratio);
    let max_block = scaled_quantum(quantum, graph_rate).clamp(512, 8192) as usize;
    let chain = Chain::try_new(ChainSpec {
        config,
        sample_rate: graph_rate,
        channels: DEFAULT_CHANNELS,
        max_block,
        irs: None,
    })
    .map_err(|e| {
        EngineError::new(
            RpcError::INVALID_PARAMS,
            format!("invalid resample-only chain: {e}"),
        )
    })?;
    Ok(ActiveChain::Resample(Box::new(chain)))
}

fn chain_has_effect(cfg: &EngineConfig) -> bool {
    if !cfg.dsp_enabled {
        return false;
    }
    let a_active = !cfg.chain.is_passthrough();
    let b_active = cfg
        .ab
        .as_ref()
        .is_some_and(|ab| !ab.chain_b.is_passthrough());
    // B's own bits or dither quantize differently from A even when
    // neither side has a stage (an output compare).
    let b_quantizes = cfg.ab.as_ref().is_some_and(|ab| {
        let (a, b) = (&cfg.chain.output, &ab.chain_b.output);
        a.bits != b.bits || a.dither != b.dither
    });
    a_active || b_active || b_quantizes || cfg.ab.as_ref().is_some_and(ab_cuts)
}

/// B's new fixed offset when `next` differs from `old` only in A/B's level
/// and that level is a fixed offset (`measured`, `manual` or `off`).
fn ab_level_only_offset(old: &EngineConfig, next: &EngineConfig) -> Option<f64> {
    use audivane_protocol::ipc::LevelMatch;
    let (Some(old_ab), Some(next_ab)) = (&old.ab, &next.ab) else {
        return None;
    };
    let offset_db = match next_ab.level {
        LevelMatch::Measured { offset_db, .. } | LevelMatch::Manual { offset_db } => offset_db,
        LevelMatch::Off => 0.0,
        LevelMatch::Analytic { .. } => return None,
    };
    let mut same = old.clone();
    if let Some(ab) = &mut same.ab {
        ab.level = next_ab.level;
    }
    (old_ab.level != next_ab.level && same == *next).then_some(offset_db)
}

/// Whether an A/B pair attenuates even when both sides are passthrough: an
/// analytic offset and an `auto` headroom are 0 dB there, a manual offset
/// or a fixed headroom are not.
fn ab_cuts(ab: &audivane_protocol::ipc::AbConfig) -> bool {
    use audivane_protocol::ipc::{AbHeadroom, LevelMatch};
    let offset = matches!(
        ab.level,
        LevelMatch::Manual { offset_db } | LevelMatch::Measured { offset_db, .. } if offset_db != 0.0
    );
    let headroom = matches!(ab.headroom, AbHeadroom::Fixed { db } if db > 0.0);
    offset || headroom
}

/// Bypass away from x1 (Direct after a switch from the DSP sink, or DSP
/// off, until the x1 reopen) writes silence: say so while playing instead
/// of playing nothing without a word.
fn bypass_muted_error(
    meters: &MeterBank,
    drv: &audivane_io_pw::EngineDriverSnapshot,
) -> Option<String> {
    (drv.state == EngineState::Playing && meters.bypass_muted()).then(|| {
        format!(
            "Output muted: bypass cannot play {} Hz to {} Hz unchanged; waiting for the device to reopen at {} Hz",
            drv.graph_rate, drv.output_rate, drv.graph_rate
        )
    })
}

/// `software_db`: the digital master gain actually applied on the DSP path.
fn dsp_has_effect(cfg: &EngineConfig, software_db: f64) -> bool {
    if !cfg.dsp_enabled {
        return false;
    }
    if chain_has_effect(cfg) {
        return true;
    }
    let digital_vol = software_db != 0.0;
    let digital_bal = cfg.volume.balance_db != 0.0;
    digital_vol || digital_bal
}

fn desired_ratio_for(cfg: &EngineConfig, rate: u32) -> RateRatio {
    to_driver_config(cfg)
        .desired_dsp(0)
        .resolve_ratio_for(SinkChoice::Dsp, rate)
}

/// The ratio a follower asks its device for at `rate`: the top of the
/// master's ladder without a probe (the ratios chain A's upsamplers build
/// at, [`crate::control::config::engine_buildable_rate_pairs`]), so a
/// follower does not open a rung the master skips; the device then steps
/// down from it as it can.
fn follower_ratio_for(cfg: &EngineConfig, rate: u32) -> RateRatio {
    let drv = to_driver_config(cfg);
    drv.desired_dsp(0)
        .ladder(SinkChoice::Dsp, rate, None)
        .as_slice()
        .first()
        .copied()
        .unwrap_or_else(|| desired_ratio_for(cfg, rate))
}

/// The ratio the DSP chain is kept warm for while Direct plays: where a
/// switch to the DSP sink would land ([`open_landing`], as the RT thread
/// predicts it), so a switch that needs no reopen finds its chain.
fn warm_dsp_ratio(cfg: &EngineConfig, rate: u32, key: Option<&NegotiatedKey>) -> RateRatio {
    open_landing(&to_driver_config(cfg), rate, key, Some(SinkChoice::Dsp))
        .unwrap_or_else(|| desired_ratio_for(cfg, rate))
}

/// Where a fresh open of `sink` (the DSP sink when unknown) for `cfg` at
/// `rate` would land, as the driver predicts it from what the last open
/// found (`key`): the first ladder entry the device plays in a configured
/// format, or, without a probe, the first one the last open did not see
/// refused ([`audivane_io_pw::lifecycle::DesiredDspConfig::open_landing`]).
fn open_landing(
    drv: &EngineDriverConfig,
    rate: u32,
    key: Option<&NegotiatedKey>,
    sink: Option<SinkChoice>,
) -> Option<RateRatio> {
    let rates = key.and_then(|k| k.rates);
    let refused = key
        .filter(|k| k.graph_rate == rate)
        .map_or(Rungs::EMPTY, |k| k.refused);
    drv.desired_dsp(0)
        .open_landing(sink.unwrap_or(SinkChoice::Dsp), rate, rates, &refused)
}

/// Whether going from `old` to `new` needs the device reopened, while it
/// runs at `running` from `current_rate` (`key`: what the open of the
/// device found, when there is one; `sink`: the sink the driver plays).
///
/// A new device, format list or quantum always reopens (real ALSA
/// parameters). Edits that steer the ratio ladder (the upsampler target,
/// DSP on or off, a change of the rate pairs the chains can be built for:
/// a preset change, A/B on or off) reopen only when a fresh open would
/// land elsewhere than `running`, so the rate never depends on whether a
/// reopen happened since an edit, and an edit that lands where the device
/// already runs (a 48 kHz-only DAC asked for x4 or x6) never interrupts
/// playback. A running pair the chain can no longer be built for always
/// reopens.
fn diff_requires_reopen(
    old: &EngineConfig,
    new: &EngineConfig,
    current_rate: u32,
    key: Option<&NegotiatedKey>,
    running: RateRatio,
    sink: Option<SinkChoice>,
) -> bool {
    if old.device != new.device
        || old.output_channels != new.output_channels
        || old.formats != new.formats
        || old.quantum != new.quantum
    {
        return true;
    }
    let (drv_old, drv_new) = (to_driver_config(old), to_driver_config(new));
    let flipped = old.dsp_enabled != new.dsp_enabled;
    let target_changed = old.chain.upsampler.target != new.chain.upsampler.target
        || desired_ratio_for(old, current_rate) != desired_ratio_for(new, current_rate);
    let mask_changed = drv_old
        .dsp_buildable
        .differs_from(drv_new.dsp_buildable, current_rate);
    // The engine bypass resampler on or off, or the pairs it builds.
    let bypass_changed = old.rate.bypass_resampler != new.rate.bypass_resampler
        || drv_old
            .bypass_buildable
            .differs_from(drv_new.bypass_buildable, current_rate);
    let dsp_path = sink != Some(SinkChoice::Direct) && new.dsp_enabled;
    if dsp_path && mask_changed && !drv_new.dsp_buildable.allows_ratio(current_rate, running) {
        return true;
    }
    if (flipped || target_changed || mask_changed || bypass_changed)
        && open_landing(&drv_new, current_rate, key, sink).is_some_and(|q| q != running)
    {
        return true;
    }
    // A fixed graph rate moved: PipeWire runs the graph at the new rate.
    if old.rate != new.rate && !new.rate.follow_source && new.rate.default_rate != current_rate {
        return true;
    }
    false
}

/// What our sinks show for an effective master volume: the desktop's 100 %
/// is the ceiling.
fn sink_view(v: &audivane_protocol::ipc::VolumeConfig, volume_db: f64) -> SinkVolumeView {
    SinkVolumeView {
        volume_db,
        balance_db: v.balance_db,
        muted: v.muted,
        ceiling_db: v
            .max_volume_db
            .map_or(0.0, |c| c.clamp(VOLUME_MIN_DB, VOLUME_MAX_DB)),
    }
}

/// Where the engine would land the DSP sink for `cfg` on a device with
/// `caps`: `(graph rate, ratio)` for every graph rate the driver would
/// offer, from the driver's own rate policy and ratio ladder
/// ([`audivane_io_pw::predicted_dsp_landings`]). The CPU admission check
/// prices designs at these landings; the overload fallback only takes a
/// step whose config has one. Empty: nothing would play.
pub fn predicted_landings(
    cfg: &EngineConfig,
    caps: Option<&audivane_protocol::ipc::DeviceCapabilities>,
) -> Result<Vec<(u32, RateRatio)>, String> {
    let caps = caps.filter(|c| c.device == cfg.device);
    audivane_io_pw::predicted_dsp_landings(&to_driver_config(cfg), caps)
}

/// `cfg` previewing upsampler `up` with DSP on, and the probe of its
/// device; `None` without a usable probe of the configured device (the
/// previews then resolve the target as `resolve_rate_ratio` does).
fn preview_config<'a>(
    cfg: &EngineConfig,
    up: &UpsamplerConfig,
    caps: Option<&'a audivane_protocol::ipc::DeviceCapabilities>,
) -> Option<(EngineConfig, &'a audivane_protocol::ipc::DeviceCapabilities)> {
    let caps = caps.filter(|c| c.device == cfg.device)?;
    audivane_io_pw::lifecycle::DeviceRates::playable(Some(caps), &cfg.formats)?;
    let mut cfg = cfg.clone();
    cfg.dsp_enabled = true;
    cfg.chain.upsampler = up.clone();
    Some((cfg, caps))
}

/// [`predicted_landings`] for upsampler `up` in place of chain A's, DSP on:
/// where the engine would land each offered source rate on the probed
/// device. `None` without a probe (or when nothing would play).
pub fn preview_landings(
    cfg: &EngineConfig,
    up: &UpsamplerConfig,
    caps: Option<&audivane_protocol::ipc::DeviceCapabilities>,
) -> Option<Vec<(u32, RateRatio)>> {
    let (cfg, caps) = preview_config(cfg, up, caps)?;
    predicted_landings(&cfg, Some(caps)).ok()
}

/// Cap of the "Best for DAC" alternative for a `ratio` target.
const BEST_FOR_DAC_CAP: u32 = 768_000;

/// Where the engine plays a stream at each of `rates` with upsampler `up`
/// (DSP on) on the probed device ([`audivane_io_pw::predicted_dsp_landing`]),
/// with a "Best for DAC" alternative where `up`'s target lands 1:1 or is
/// converted and that one would upsample. Empty without a probe.
pub fn source_landings(
    cfg: &EngineConfig,
    up: &UpsamplerConfig,
    caps: Option<&audivane_protocol::ipc::DeviceCapabilities>,
    rates: &[u32],
) -> Vec<audivane_protocol::ipc::SourceLanding> {
    use audivane_io_pw::DspLanding;
    use audivane_protocol::ipc::{BestForDac, SourceLanding};
    let Some((preview, caps)) = preview_config(cfg, up, caps) else {
        return Vec::new();
    };
    let driver = to_driver_config(&preview);
    let best_target = match up.target {
        RateTarget::Off | RateTarget::Rate(_) | RateTarget::MaxRateAny(_) => None,
        RateTarget::MaxRate(cap) => Some(RateTarget::MaxRateAny(cap)),
        RateTarget::Ratio(_) => Some(RateTarget::MaxRateAny(BEST_FOR_DAC_CAP)),
    };
    // Made at the first rate that lands 1:1, if any: its driver config
    // checks every pair the other family could reach, a search per pair for
    // an automatic design (tens of seconds for a long least-squares one).
    let mut best = None;
    let mut best_for = |preview: &EngineConfig| {
        best.get_or_insert_with(|| {
            best_target.map(|target| {
                let mut alt = preview.clone();
                alt.chain.upsampler.target = target;
                (target, to_driver_config(&alt))
            })
        })
        .clone()
    };
    let mut out = Vec::new();
    for &rate in rates {
        let Ok(landing) = audivane_io_pw::predicted_dsp_landing(&driver, Some(caps), rate) else {
            return Vec::new();
        };
        let (ratio, converted_to) = match landing {
            DspLanding::Opens(r) => (r, None),
            DspLanding::Converted(to) => (RateRatio::ONE, Some(to)),
        };
        let best_for_dac = ratio
            .is_unity()
            .then(|| best_for(&preview))
            .flatten()
            .and_then(|(target, alt)| {
                match audivane_io_pw::predicted_dsp_landing(&alt, Some(caps), rate) {
                    Ok(DspLanding::Opens(r)) if !r.is_unity() => {
                        Some(BestForDac { target, ratio: r })
                    }
                    _ => None,
                }
            });
        out.push(SourceLanding {
            source_rate: rate,
            ratio,
            requested: resolve_rate_ratio(up.target, rate),
            converted_to,
            best_for_dac,
        });
    }
    out
}

fn to_driver_config(cfg: &EngineConfig) -> EngineDriverConfig {
    EngineDriverConfig {
        device: cfg.device.clone(),
        formats: cfg.formats.clone(),
        channels: DEFAULT_CHANNELS as u32,
        output_channels: cfg.output_channels,
        quantum: cfg.quantum.max(MIN_QUANTUM),
        buffer_periods: DEFAULT_BUFFER_PERIODS,
        dsp_enabled: cfg.dsp_enabled,
        dsp_rate_target: cfg.chain.upsampler.target,
        max_dsp_ratio: MAX_DSP_RATIO as u32,
        dsp_buildable: crate::control::config::engine_buildable_rate_pairs(cfg),
        bypass_buildable: crate::control::config::bypass_buildable_rate_pairs(cfg),
        rate_policy: cfg.rate.clone(),
        reserve_device: true,
        reserve_priority: DEFAULT_RESERVE_PRIORITY,
        rt_priority: DEFAULT_RT_PRIORITY,
        sink_volume: (cfg.volume.sink_volume == SinkVolumePolicy::Follow)
            .then(|| sink_view(&cfg.volume, volume::target_db(&cfg.volume))),
    }
}

/// The enabled convolution items of `convs`, loaded at `rate`.
pub(crate) fn load_conv_items(
    convs: &[audivane_protocol::ConvolutionConfig],
    ir_cache: &mut IrCache,
    rate: u32,
) -> Result<Vec<audivane_dsp_core::matrix::LoadedConv>, EngineError> {
    audivane_dsp_core::matrix::load_conv(convs, DEFAULT_CHANNELS, &mut |path| {
        ir_cache.load_for_rate(path, rate).map_err(|e| e.message)
    })
    .map_err(|e| EngineError::new(RpcError::INVALID_PARAMS, e))
}

/// Load, check and combine one chain's convolution at `rate` (`None`
/// without an enabled item).
fn validate_chain_irs(
    convs: &[audivane_protocol::ConvolutionConfig],
    ir_cache: &mut IrCache,
    rate: u32,
) -> Result<Option<audivane_dsp_core::matrix::ConvIrs>, EngineError> {
    let loaded = load_conv_items(convs, ir_cache, rate)?;
    if loaded.is_empty() {
        return Ok(None);
    }
    let items: Vec<_> = loaded.iter().map(|l| l.as_item()).collect();
    audivane_dsp_core::matrix::check_conv(&items, DEFAULT_CHANNELS)
        .map_err(|e| EngineError::new(RpcError::INVALID_PARAMS, e.to_string()))?;
    let combined = audivane_dsp_core::matrix::combine_conv(&items, DEFAULT_CHANNELS)
        .map_err(|e| EngineError::new(RpcError::INVALID_PARAMS, e.to_string()))?;
    Ok(Some(combined))
}

/// A row's gain above a limit by less than this is taken as at it (the
/// certified bound's own margin, float noise on a flat filter), dB.
const CONV_GUARD_SLACK_DB: f64 = 0.01;
/// The subsonic warning needs more than this much gain below the limit, dB:
/// a flat filter (0 dB) never warns.
const CONV_SUBSONIC_MIN_DB: f64 = 0.1;

fn conv_row_name(output: usize) -> String {
    match output {
        0 => "L".into(),
        1 => "R".into(),
        n => format!("output {n}"),
    }
}

/// Hold one chain's combined convolution to the guards
/// (`convolution_safety`): a row over the boost cap is refused (the
/// message names the row and its gain); energy below the subsonic limit,
/// and a boost the output headroom does not cover with the limiter off,
/// are warnings. Off the audio thread; reusable per chain.
pub(crate) fn conv_guard(
    chain: &ChainConfig,
    side: AbSide,
    safety: &audivane_protocol::ipc::ConvolutionSafety,
    gains: &[audivane_dsp_core::conv_safety::RowGain],
) -> Result<Vec<audivane_protocol::ipc::ConvWarning>, EngineError> {
    use audivane_protocol::ipc::{ConvWarning, ConvWarningKind};
    let prefix = match side {
        AbSide::A => "",
        AbSide::B => "A/B side B: ",
    };
    // The loudest row over the cap (the first of equals).
    if let Some(over) = gains
        .iter()
        .filter(|g| g.peak_db > safety.max_boost_db + CONV_GUARD_SLACK_DB)
        .fold(
            None,
            |best: Option<&audivane_dsp_core::conv_safety::RowGain>, g| match best {
                Some(b) if b.peak_db >= g.peak_db => Some(b),
                _ => Some(g),
            },
        )
    {
        return Err(EngineError::new(
            RpcError::INVALID_PARAMS,
            format!(
                "{prefix}the convolution boosts up to {:+.1} dB on {}, over the {:+.1} dB cap \
                 (convolution_safety.max_boost_db)",
                over.peak_db,
                conv_row_name(over.output),
                safety.max_boost_db
            ),
        ));
    }
    let side_tag = match side {
        AbSide::A => "",
        AbSide::B => "B: ",
    };
    let mut warnings = Vec::new();
    for g in gains {
        if g.low_db > CONV_SUBSONIC_MIN_DB {
            warnings.push(ConvWarning {
                side,
                device: None,
                output: g.output as u32,
                kind: ConvWarningKind::Subsonic,
                gain_db: g.low_db,
                freq_hz: Some(g.low_hz),
                limit_hz: Some(safety.subsonic_hz),
                headroom_db: None,
                message: format!(
                    "{side_tag}Below {} Hz: {:+.1} dB on {} (largest at {:.1} Hz)",
                    safety.subsonic_hz,
                    g.low_db,
                    conv_row_name(g.output),
                    g.low_hz
                ),
            });
        }
    }
    let headroom = chain.output.headroom_db;
    if chain.limiter.mode == audivane_protocol::LimiterMode::Off {
        for g in gains {
            if g.peak_db > headroom + CONV_GUARD_SLACK_DB {
                warnings.push(ConvWarning {
                    side,
                    device: None,
                    output: g.output as u32,
                    kind: ConvWarningKind::ClipRisk,
                    gain_db: g.peak_db,
                    freq_hz: None,
                    limit_hz: None,
                    headroom_db: Some(headroom),
                    message: format!(
                        "{side_tag}Limiter off: the convolution peaks at {:+.1} dB on {}, more \
                         than the {headroom} dB output headroom; loud passages can clip",
                        g.peak_db,
                        conv_row_name(g.output)
                    ),
                });
            }
        }
    }
    Ok(warnings)
}

/// Validate one chain's convolution and hold it to the guards
/// ([`conv_guard`]).
fn check_chain_conv(
    chain: &ChainConfig,
    side: AbSide,
    safety: &audivane_protocol::ipc::ConvolutionSafety,
    ir_cache: &mut IrCache,
    rate: u32,
) -> Result<Vec<audivane_protocol::ipc::ConvWarning>, EngineError> {
    let Some(irs) = validate_chain_irs(&chain.convolution, ir_cache, rate)? else {
        return Ok(Vec::new());
    };
    let gains = ir_cache.row_gains(&irs, rate, DEFAULT_CHANNELS, safety.subsonic_hz);
    conv_guard(chain, side, safety, &gains)
}

fn load_and_combine_irs(
    convs: &[audivane_protocol::ConvolutionConfig],
    ir_cache: &mut IrCache,
    rate: u32,
) -> Result<Option<audivane_dsp_core::matrix::ConvIrs>, EngineError> {
    let loaded = load_conv_items(convs, ir_cache, rate)?;
    if loaded.is_empty() {
        return Ok(None);
    }
    let items: Vec<_> = loaded.iter().map(|l| l.as_item()).collect();
    let combined = audivane_dsp_core::matrix::combine_conv(&items, DEFAULT_CHANNELS)
        .map_err(|e| EngineError::new(RpcError::INVALID_PARAMS, e.to_string()))?;
    Ok(Some(combined))
}

/// Validate `cfg` for the engine at `rate`; the convolution guards'
/// warnings of every chain on success.
pub(crate) fn validate_engine_config(
    cfg: &EngineConfig,
    ir_cache: &mut IrCache,
    rate: u32,
) -> Result<Vec<audivane_protocol::ipc::ConvWarning>, EngineError> {
    if cfg.device.trim().is_empty() {
        return Err(EngineError::new(
            RpcError::INVALID_PARAMS,
            "device must not be empty",
        ));
    }
    if !(MIN_QUANTUM..=MAX_QUANTUM).contains(&cfg.quantum) || !cfg.quantum.is_power_of_two() {
        return Err(EngineError::new(
            RpcError::INVALID_PARAMS,
            format!(
                "quantum must be a power of two between {MIN_QUANTUM} and {MAX_QUANTUM} (got {})",
                cfg.quantum
            ),
        ));
    }
    if cfg.rate.default_rate < 8_000 {
        return Err(EngineError::new(
            RpcError::INVALID_PARAMS,
            "rate.default_rate must be >= 8000",
        ));
    }
    if !cfg.volume.volume_db.is_finite()
        || !(VOLUME_MIN_DB..=VOLUME_MAX_DB).contains(&cfg.volume.volume_db)
    {
        return Err(EngineError::new(
            RpcError::INVALID_PARAMS,
            format!("volume_db must be in {VOLUME_MIN_DB}..={VOLUME_MAX_DB}"),
        ));
    }
    if !cfg.volume.balance_db.is_finite()
        || !(-BALANCE_MAX_DB..=BALANCE_MAX_DB).contains(&cfg.volume.balance_db)
    {
        return Err(EngineError::new(
            RpcError::INVALID_PARAMS,
            format!("balance_db must be in -{BALANCE_MAX_DB}..={BALANCE_MAX_DB}"),
        ));
    }
    validate_chain_params(&cfg.chain)?;
    validate_tilt_at_rate(&cfg.chain, rate)?;

    if let Some(ab) = &cfg.ab {
        validate_chain_params(&ab.chain_b)?;
        validate_tilt_at_rate(&ab.chain_b, rate)?;
        // B's bits and dither may differ (its own quantizer); the output
        // headroom is taken before the mix and must match.
        if cfg.chain.output.headroom_db != ab.chain_b.output.headroom_db {
            return Err(EngineError::new(
                RpcError::INVALID_PARAMS,
                "A/B output headroom must match",
            ));
        }
        let ratio_a = resolve_rate_ratio(cfg.chain.upsampler.target, rate);
        let ratio_b = resolve_rate_ratio(ab.chain_b.upsampler.target, rate);
        if ratio_a != ratio_b || cfg.chain.upsampler.target != ab.chain_b.upsampler.target {
            return Err(EngineError::new(
                RpcError::INVALID_PARAMS,
                format!("A/B upsampling ratios differ: {ratio_a} vs {ratio_b}"),
            ));
        }
    }
    cfg.convolution_safety
        .validate()
        .map_err(|e| EngineError::new(RpcError::INVALID_PARAMS, e))?;
    let safety = &cfg.convolution_safety;
    let mut warnings = check_chain_conv(&cfg.chain, AbSide::A, safety, ir_cache, rate)?;
    if let Some(ab) = &cfg.ab {
        warnings.extend(check_chain_conv(
            &ab.chain_b,
            AbSide::B,
            safety,
            ir_cache,
            rate,
        )?);
    }
    // Each follower's own chain, where its convolution differs from A's
    // (chain A's is checked above).
    for f in &cfg.outputs.followers {
        if f.chain_patch.is_none() {
            continue;
        }
        let own = crate::control::follower_chain::config(cfg, f);
        validate_chain_params(&own.chain)?;
        validate_tilt_at_rate(&own.chain, rate)?;
        if own.chain.convolution == cfg.chain.convolution
            && own.chain.limiter == cfg.chain.limiter
            && own.chain.output.headroom_db == cfg.chain.output.headroom_db
        {
            continue;
        }
        let tag = |m: String| format!("follower {}: {m}", f.device);
        let found = check_chain_conv(&own.chain, AbSide::A, safety, ir_cache, rate)
            .map_err(|e| EngineError::new(e.code, tag(e.message)))?;
        warnings.extend(found.into_iter().map(|mut w| {
            w.device = Some(f.device.clone());
            w.message = tag(std::mem::take(&mut w.message));
            w
        }));
    }
    Ok(warnings)
}

/// The overload hold's acknowledgement ([`crate::api::VarispeedRequest::hold_ack`]):
/// the sequence the audio thread applies (`rt_ack`), except that a
/// retirement (`Off`, given as `seq`) counts only once the stage is gone
/// (`retired`: none on the audio thread, nothing left queued).
fn varispeed_hold_ack(
    hold: crate::api::VarispeedHold,
    seq: u64,
    rt_ack: u64,
    retired: bool,
) -> u64 {
    if hold == crate::api::VarispeedHold::Off && rt_ack >= seq && !retired {
        seq.saturating_sub(1)
    } else {
        rt_ack
    }
}

/// Design the tilt at the stream rate, so a config whose filter cannot be
/// built (band, pivot, or a design that misses its accuracy check) is
/// rejected before it is applied, not only when the chain is rebuilt.
fn validate_tilt_at_rate(c: &ChainConfig, rate: u32) -> Result<(), EngineError> {
    if let Some(eq) = c.eq.as_ref()
        && (eq.tilt.is_some_and(|t| t.enabled) || eq.tilt_high.is_some_and(|t| t.enabled))
    {
        audivane_dsp_core::tilt::TiltFilter::design_stage(
            eq.tilt.as_ref(),
            eq.tilt_high.as_ref(),
            f64::from(rate),
        )
        .map_err(|e| EngineError::new(RpcError::INVALID_PARAMS, e))?;
    }
    Ok(())
}

fn validate_chain_params(c: &ChainConfig) -> Result<(), EngineError> {
    c.limiter
        .validate()
        .map_err(|e| EngineError::new(RpcError::INVALID_PARAMS, e))?;
    if !(1..=32).contains(&c.output.bits) {
        return Err(EngineError::new(
            RpcError::INVALID_PARAMS,
            format!("invalid output.bits: {}", c.output.bits),
        ));
    }
    if !matches!(c.upsampler.target, RateTarget::Off) {
        let named = audivane_dsp_core::design::Named::of(&c.upsampler)
            .map_err(|e| EngineError::new(RpcError::INVALID_PARAMS, format!("upsampler: {e}")))?;
        if named.preset.is_none()
            && let Err(e) = audivane_dsp_core::design::check::check(&named.design)
        {
            return Err(EngineError::new(
                RpcError::INVALID_PARAMS,
                format!("upsampler.{e}"),
            ));
        }
    }
    Ok(())
}

/// The A/B gains from the configs alone, for the paths that build no pair
/// (a built pair reports its own, with the IR and upsampler terms).
fn estimate_ab_gains(cfg: &EngineConfig, rate: u32) -> Result<AbGains, EngineError> {
    let Some(ab) = &cfg.ab else {
        return Ok(AbGains::default());
    };
    AbChain::estimate_gains(
        &cfg.chain,
        &ab.chain_b,
        rate,
        AbLevel::from_config(&ab.level, ab.headroom),
    )
    .map_err(|e| EngineError::new(RpcError::INVALID_PARAMS, e))
}

/// Engine A/B status for `side` and the pair's gains (overs are filled in
/// from the meters).
fn ab_status(side: AbSide, gains: AbGains) -> AbStatus {
    AbStatus {
        enabled: true,
        current: Some(side),
        offset_db: gains.offset_db,
        gain_a_db: gains.gain_a_db,
        gain_b_db: gains.gain_b_db,
        headroom_db: gains.headroom_db,
        overs: Default::default(),
        // The control plane fills in the level, the measurement and the
        // blind mask.
        blind: false,
        level: Default::default(),
        measuring: None,
        measure_error: None,
        error: None,
    }
}

/// `cfg` with `output.bits` (chain A and B) capped at the container width of
/// the negotiated format: "Dither to 24" on an S16 device dithers to 16
/// instead of leaving the S16 packer to truncate the low 8 bits.
fn cap_dither_bits(cfg: &EngineConfig, format: Option<SampleFormat>) -> EngineConfig {
    let mut out = cfg.clone();
    out.chain.output.bits = effective_dither_bits(cfg.chain.output.bits, format);
    if let Some(ab) = &mut out.ab {
        ab.chain_b.output.bits = effective_dither_bits(ab.chain_b.output.bits, format);
    }
    out
}

/// `cfg` with the upsampler target replaced by the ratio the device was
/// actually opened at from `graph_rate`.
fn with_actual_ratio(
    mut cfg: ChainConfig,
    graph_rate: u32,
    actual_ratio: RateRatio,
) -> ChainConfig {
    cfg.upsampler.target = if actual_ratio.is_unity() {
        RateTarget::Off
    } else if let Some(r) = actual_ratio.as_integer() {
        RateTarget::Ratio(r)
    } else {
        RateTarget::Rate(actual_ratio.output_rate(graph_rate).unwrap_or(graph_rate))
    };
    cfg
}

fn build_active_chain(
    cfg: &EngineConfig,
    ir_cache: &mut IrCache,
    graph_rate: u32,
    actual_ratio: RateRatio,
    current_ab_side: AbSide,
) -> Result<(ActiveChain, Option<AbGains>, bool), EngineError> {
    build_active_chain_guarded(
        cfg,
        ir_cache,
        graph_rate,
        actual_ratio,
        current_ab_side,
        true,
    )
}

/// [`build_active_chain`]; with `guard`, each side's combined IRs (chain A
/// and, with A/B, chain B), exactly as loaded for the build at this graph
/// rate, are held to the convolution guards first ([`conv_guard`]): a rate
/// change, or a file replaced at the same path since the config was
/// checked, cannot play a filter over the cap (the master's rebuild then
/// falls back to a chain without convolution, reported in status).
fn build_active_chain_guarded(
    cfg: &EngineConfig,
    ir_cache: &mut IrCache,
    graph_rate: u32,
    actual_ratio: RateRatio,
    current_ab_side: AbSide,
    guard: bool,
) -> Result<(ActiveChain, Option<AbGains>, bool), EngineError> {
    let has_effect = chain_has_effect(cfg);
    if !cfg.dsp_enabled {
        // DSP off only ever opens x1 (until that reopen lands, nothing
        // plays).
        // No pair runs: status estimates the gains from the config.
        estimate_ab_gains(cfg, graph_rate)?;
        return Ok((ActiveChain::Bypass, None, false));
    }

    rational_upsamplers_fit(cfg, graph_rate, actual_ratio)
        .map_err(|e| EngineError::new(RpcError::INVALID_PARAMS, format!("invalid chain: {e}")))?;
    let max_block = scaled_quantum(cfg.quantum, graph_rate).clamp(512, 8192) as usize;
    let a_cfg = with_actual_ratio(cfg.chain.clone(), graph_rate, actual_ratio);

    let irs_a = load_and_combine_irs(&a_cfg.convolution, ir_cache, graph_rate)?;
    if guard && let Some(irs) = &irs_a {
        let safety = &cfg.convolution_safety;
        let gains = ir_cache.row_gains(irs, graph_rate, DEFAULT_CHANNELS, safety.subsonic_hz);
        conv_guard(&cfg.chain, AbSide::A, safety, &gains)?;
    }

    if let Some(ab) = &cfg.ab {
        let b_cfg = with_actual_ratio(ab.chain_b.clone(), graph_rate, actual_ratio);
        let irs_b = load_and_combine_irs(&b_cfg.convolution, ir_cache, graph_rate)?;
        if guard && let Some(irs) = &irs_b {
            let safety = &cfg.convolution_safety;
            let gains = ir_cache.row_gains(irs, graph_rate, DEFAULT_CHANNELS, safety.subsonic_hz);
            conv_guard(&ab.chain_b, AbSide::B, safety, &gains)?;
        }
        let level = AbLevel::from_config(&ab.level, ab.headroom);
        let spec_a = ChainSpec {
            config: a_cfg,
            sample_rate: graph_rate,
            channels: DEFAULT_CHANNELS,
            max_block,
            irs: irs_a,
        };
        let spec_b = ChainSpec {
            config: b_cfg,
            sample_rate: graph_rate,
            channels: DEFAULT_CHANNELS,
            max_block,
            irs: irs_b,
        };
        let mut ab_chain = AbChain::new(spec_a, spec_b, level).map_err(|e| {
            EngineError::new(RpcError::INVALID_PARAMS, format!("invalid A/B chain: {e}"))
        })?;
        if current_ab_side == AbSide::B {
            ab_chain.select(Side::B, 0);
        }
        let gains = ab_chain.gains();
        return Ok((ActiveChain::Ab(Box::new(ab_chain)), Some(gains), has_effect));
    }

    let spec = ChainSpec {
        config: a_cfg,
        sample_rate: graph_rate,
        channels: DEFAULT_CHANNELS,
        max_block,
        irs: irs_a,
    };
    let chain = Chain::try_new(spec)
        .map_err(|e| EngineError::new(RpcError::INVALID_PARAMS, format!("invalid chain: {e}")))?;
    Ok((ActiveChain::Single(Box::new(chain)), None, has_effect))
}

/// Check every upsampler `cfg` runs (chain A and, with A/B, chain B) at
/// the non-integer `ratio` from `graph_rate`, counting the pipelines the
/// config runs at once ([`crate::control::config::rational_pipelines`]).
/// The chain build itself only knows its own copy; this is what keeps a
/// config from running a design the budget refuses for it. Integer ratios
/// always pass (their limits are validated with the config).
fn rational_upsamplers_fit(
    cfg: &EngineConfig,
    graph_rate: u32,
    ratio: RateRatio,
) -> Result<(), String> {
    if ratio.is_integer() || !cfg.dsp_enabled {
        return Ok(());
    }
    let pipelines = crate::control::config::rational_pipelines(cfg.ab.is_some());
    side_fits("chain.upsampler", &cfg.chain, graph_rate, ratio, pipelines)?;
    if let Some(ab) = &cfg.ab {
        side_fits(
            "ab.chain_b.upsampler",
            &crate::control::ab_side::design_view(&ab.chain_b, &cfg.chain),
            graph_rate,
            ratio,
            pipelines,
        )?;
    }
    Ok(())
}

/// [`rational_upsamplers_fit`] for one side.
fn side_fits(
    field: &str,
    chain: &ChainConfig,
    graph_rate: u32,
    ratio: RateRatio,
    pipelines: usize,
) -> Result<(), String> {
    // Resolved against the chain's own (format-capped) output, as the chain
    // build does: a `match_output` budget is unresolved before.
    let up = crate::control::config::resolve_upsampler_for_output(
        &with_actual_ratio(chain.clone(), graph_rate, ratio).upsampler,
        &chain.output,
    );
    crate::control::config::check_rational_at(field, &up, graph_rate, ratio, pipelines)
}

/// Build a reduced chain that is still valid for `actual_ratio` after
/// [`build_active_chain`] failed for the freshly negotiated format.
///
/// Policy: keep as much of the requested processing as possible, because the
/// alternative is silent playback on a stale chain or being stuck in
/// `Switching`/`Error`.
///
/// 1. At a fractional or decimating ratio an upsampler itself may not run there
///    (a preset without a rule for it, or a design over the cost limit), on
///    either A/B side ([`rational_upsamplers_fit`]): replace it on both sides by
///    a short safe design ([`rational_fallback_upsampler`]) and keep EQ,
///    convolution, A/B and the rest.
/// 2. Disable convolution on both A/B sides and rebuild (with the safe upsampler
///    when step 1 replaced it). A missing or invalid IR at the new rate is the
///    common failure (the per-rate cache key changes when the file or its mtime
///    disappears), and EQ/upsampling stay intact.
/// 3. If that still fails, build chain A's upsampler (or the safe design) with
///    its limiter, and pitch, crossfeed and harmonics when they build: no EQ, no
///    convolution, no A/B, at the same ratio so the resume gate's ratio check
///    matches.
///
/// Returns the chain with what was given up, for status.
fn build_fallback_chain(
    cfg: &EngineConfig,
    ir_cache: &mut IrCache,
    graph_rate: u32,
    actual_ratio: RateRatio,
    current_ab_side: AbSide,
) -> Result<(ActiveChain, Option<AbGains>, bool, String), EngineError> {
    const REPLACED: &str = "the upsampler replaced by a short safe design";
    let mut base = cfg.clone();
    let mut replaced = false;
    if rational_upsamplers_fit(cfg, graph_rate, actual_ratio).is_err() {
        let pipelines = crate::control::config::rational_pipelines(cfg.ab.is_some());
        let safe = rational_fallback_upsampler(graph_rate, actual_ratio, pipelines);
        let replace = |u: &mut UpsamplerConfig| {
            *u = UpsamplerConfig {
                target: u.target,
                ..safe.clone()
            };
        };
        let kept = if cfg.ab.is_some() {
            "EQ, convolution and A/B kept"
        } else {
            "EQ and convolution kept"
        };
        replace(&mut base.chain.upsampler);
        if let Some(ab) = &mut base.ab {
            replace(&mut ab.chain_b.upsampler);
        }
        replaced = true;
        if let Ok((chain, offset, effect)) =
            build_active_chain(&base, ir_cache, graph_rate, actual_ratio, current_ab_side)
        {
            return Ok((chain, offset, effect, format!("{REPLACED} ({kept})")));
        }
    }

    // Every later step keeps chain A's upsampler (or the safe design).
    // When that design is what failed, a retry would only repeat it (an
    // automatic-taps search, say): report it at once. The automatic design
    // is cached, so this is cheap. (At a rational ratio the check above
    // already replaced a design that fails.)
    if !replaced && !actual_ratio.is_unity() && cfg.dsp_enabled {
        let resolved = crate::control::config::resolve_upsampler_for_output(
            &base.chain.upsampler,
            &base.chain.output,
        );
        let cx = audivane_dsp_core::design::BuildContext {
            source_rate: graph_rate,
            ratio: actual_ratio,
            channels: 2,
            max_block: 1024,
            pipelines: crate::control::config::rational_pipelines(base.ab.is_some()),
            output: base.chain.output,
            workspace: audivane_dsp_core::design::budget::design_workspace(),
        };
        if let Ok(named) = audivane_dsp_core::design::Named::of(&resolved)
            && named.preset.is_none()
            && let Err(e) = audivane_dsp_core::design::searched_feasibility(&named.design, &cx)
        {
            return Err(EngineError::new(
                RpcError::INVALID_PARAMS,
                format!("upsampler design failed: {e}"),
            ));
        }
    }

    let mut stripped = base.clone();
    stripped.chain.convolution.clear();
    if let Some(ab) = &mut stripped.ab {
        ab.chain_b.convolution.clear();
    }
    // Without convolution to drop, this would rebuild the chain that
    // already failed.
    if stripped != base
        && let Ok((chain, offset, effect)) = build_active_chain(
            &stripped,
            ir_cache,
            graph_rate,
            actual_ratio,
            current_ab_side,
        )
    {
        let msg = if replaced {
            format!("{REPLACED} and convolution disabled")
        } else {
            "convolution disabled".to_string()
        };
        return Ok((chain, offset, effect, msg));
    }

    // Last resort: chain A's upsampler and limiter (a safety stage, never
    // dropped), with pitch, crossfeed and harmonics when they build, at
    // exactly the negotiated ratio.
    let full = with_actual_ratio(
        ChainConfig {
            upsampler: base.chain.upsampler.clone(),
            limiter: cfg.chain.limiter.clone(),
            output: cfg.chain.output,
            pitch: cfg.chain.pitch.clone(),
            crossfeed: cfg.chain.crossfeed.clone(),
            harmonics: cfg.chain.harmonics.clone(),
            ..ChainConfig::neutral()
        },
        graph_rate,
        actual_ratio,
    );
    let bare = ChainConfig {
        pitch: Default::default(),
        crossfeed: None,
        harmonics: None,
        ..full.clone()
    };
    let given_up = |replaced: bool, extras_dropped: bool| {
        let mut dropped = vec!["EQ", "convolution"];
        if extras_dropped {
            if cfg.chain.pitch.is_active() {
                dropped.push("pitch");
            }
            if cfg.chain.crossfeed.as_ref().is_some_and(|c| c.is_active()) {
                dropped.push("crossfeed");
            }
            if cfg.chain.harmonics.as_ref().is_some_and(|h| h.is_active()) {
                dropped.push("harmonics");
            }
        }
        let last = dropped.pop().unwrap_or_default();
        let mut msg = if dropped.is_empty() {
            format!("{last} disabled")
        } else {
            format!("{} and {last} disabled", dropped.join(", "))
        };
        if cfg.ab.is_some() {
            msg.push_str(", A/B off (chain A only)");
        }
        if replaced {
            msg.push_str(&format!(", {REPLACED}"));
        }
        msg
    };
    if full.is_passthrough() {
        return Ok((ActiveChain::Bypass, None, false, given_up(replaced, false)));
    }
    let max_block = scaled_quantum(cfg.quantum, graph_rate).clamp(512, 8192) as usize;
    let build = |config: &ChainConfig| {
        Chain::try_new(ChainSpec {
            config: config.clone(),
            sample_rate: graph_rate,
            channels: DEFAULT_CHANNELS,
            max_block,
            irs: None,
        })
    };
    let mut attempts = vec![(full.clone(), replaced, false)];
    if bare != full {
        attempts.push((bare.clone(), replaced, true));
    }
    // A rational ratio the configured filter cannot run at (too long, or a
    // preset without a rule for it): a short safe design instead, so the
    // ratio the device was opened at still plays.
    if !actual_ratio.is_integer() && !replaced {
        let safe = |mut c: ChainConfig| {
            c.upsampler = UpsamplerConfig {
                target: c.upsampler.target,
                ..rational_fallback_upsampler(
                    graph_rate,
                    actual_ratio,
                    crate::control::config::rational_pipelines(false),
                )
            };
            c
        };
        attempts.push((safe(full.clone()), true, false));
        if bare != full {
            attempts.push((safe(bare), true, true));
        }
    }
    let mut last_err = None;
    for (config, replaced, extras_dropped) in attempts {
        match build(&config) {
            Ok(chain) => {
                return Ok((
                    ActiveChain::Single(Box::new(chain)),
                    None,
                    true,
                    given_up(replaced, extras_dropped),
                ));
            }
            Err(e) => last_err = Some(e),
        }
    }
    Err(EngineError::new(
        RpcError::INVALID_PARAMS,
        format!(
            "invalid fallback chain: {}",
            last_err.map_or_else(String::new, |e| e.to_string())
        ),
    ))
}

/// Taps per sample at the lower of the two rates of the rational fallback
/// design ([`rational_fallback_upsampler`]).
const RATIONAL_FALLBACK_TAPS: u32 = 256;

/// The design a rational fallback chain runs from `graph_rate` at `ratio`
/// with `pipelines` copies at once: Kaiser beta 14 with the stopband edge at
/// the lower Nyquist (about 140 dB), [`RATIONAL_FALLBACK_TAPS`] taps per
/// sample at the lower of the two rates (so a decimating ratio gets a kernel
/// as sharp, relative to its output band, as an upsampling one), cut to what
/// the rational cost limit allows. Flat (within 0.1 dB) to about 0.9 of the
/// lower Nyquist.
fn rational_fallback_upsampler(
    graph_rate: u32,
    ratio: RateRatio,
    pipelines: usize,
) -> UpsamplerConfig {
    // Taps per input sample: scaled by in/out when decimating.
    let per_input = (RATIONAL_FALLBACK_TAPS as u64 * u64::from(ratio.down))
        .div_ceil(u64::from(ratio.up))
        .max(u64::from(RATIONAL_FALLBACK_TAPS)) as usize;
    let fits =
        crate::control::config::max_rational_taps_per_input(graph_rate, ratio, pipelines, false);
    audivane_dsp_core::resample::ManualConfig {
        taps: per_input.min(fits).max(16),
        taps_unit: audivane_protocol::TapsUnit::Input,
        kaiser_beta: 14.0,
        cutoff_mode: audivane_protocol::CutoffMode::NyquistStop,
        ..audivane_dsp_core::resample::ManualConfig::default()
    }
    .into_upsampler_config()
    .expect("valid fallback design")
}

#[cfg(test)]
mod tests {
    use super::*;
    use audivane_protocol::ipc::AbConfig;
    use audivane_protocol::ipc::LevelMatch;
    use audivane_protocol::{Band, ConvolutionConfig, EqProfile, FilterType, UpsamplerConfig};

    fn test_cfg() -> EngineConfig {
        EngineConfig {
            device: "hw:Loopback,0,0".into(),
            chain: ChainConfig::neutral(),
            ..Default::default()
        }
    }

    #[test]
    fn chain_params_validate_presets_and_designs() {
        let mut chain = ChainConfig {
            upsampler: audivane_dsp_core::resample::ManualConfig {
                target: RateTarget::Ratio(4),
                taps: 0,
                cutoff: 5.0,
                kaiser_beta: -1.0,
                ..Default::default()
            }
            .into_upsampler_config()
            .unwrap(),
            ..ChainConfig::neutral()
        };
        assert!(validate_chain_params(&chain).is_err());
        chain.upsampler = UpsamplerConfig {
            target: RateTarget::Ratio(4),
            preset: Some(
                audivane_protocol::FilterPreset::GaussianShort
                    .name()
                    .to_string(),
            ),
            ..Default::default()
        };
        assert!(validate_chain_params(&chain).is_ok());
    }

    #[test]
    fn builds_ab_chain_with_analytic_offset_and_preserves_b_side() {
        let mut cache = IrCache::new();
        let mut cfg = test_cfg();
        cfg.chain.eq = Some(EqProfile {
            name: "flat".into(),
            preamp_db: 0.0,
            ..Default::default()
        });
        cfg.ab = Some(AbConfig {
            level: LevelMatch::default(),
            headroom: Default::default(),
            ..AbConfig::chain(ChainConfig {
                eq: Some(EqProfile {
                    name: "hot".into(),
                    preamp_db: 3.0,
                    ..Default::default()
                }),
                ..ChainConfig::neutral()
            })
        });

        let (chain, offset_db, has_effect) =
            build_active_chain(&cfg, &mut cache, 48_000, RateRatio::integer(1), AbSide::B).unwrap();
        let offset_db = offset_db.expect("a built pair reports its gains");
        assert!(has_effect);
        assert!(
            (offset_db.offset_db + 3.0).abs() < 1e-6,
            "got {offset_db:?}"
        );
        // Never boost: B is cut, A is not; B's +3 dB preamp is matched away,
        // so the auto headroom has nothing left to cover.
        assert_eq!(offset_db.gain_a_db, 0.0);
        assert!((offset_db.gain_b_db + 3.0).abs() < 1e-6, "{offset_db:?}");
        assert!(offset_db.headroom_db.abs() < 1e-6, "{offset_db:?}");
        match chain {
            ActiveChain::Ab(ab) => assert_eq!(ab.current(), Side::B),
            _ => panic!("expected ActiveChain::Ab"),
        }
    }

    #[test]
    fn the_rate_plan_builds_its_upsampler_at_the_rates_it_names() {
        // A request with a per-rate plan for 96 kHz: an open there builds
        // the plan's upsampler (the request stays the config), an open at
        // 48 kHz the request's; an edit after the plan moved rebuilds.
        let x2 = RateRatio::integer(2);
        let mut cfg = test_cfg();
        cfg.chain.upsampler = audivane_dsp_core::resample::ManualConfig {
            target: RateTarget::Ratio(2),
            taps: 4095,
            ..Default::default()
        }
        .into_upsampler_config()
        .unwrap();
        let light = UpsamplerConfig {
            target: cfg.chain.upsampler.target,
            ..crate::control::config::bypass_upsampler()
        };
        let mut h = BuilderProcHarness::new(cfg.clone());
        *h.state.rate_plan.lock().unwrap() = crate::api::RatePlan {
            made_for: Some((cfg.chain.upsampler.clone(), cfg.chain.output)),
            rates: vec![(96_000, light.clone())],
        };
        let n = h.negotiate(48_000, x2, SinkChoice::Dsp, SampleFormat::S32);
        assert!(n.error.is_none(), "{n:?}");
        assert_eq!(h.state.built_substitute, None);
        let n = h.negotiate(96_000, x2, SinkChoice::Dsp, SampleFormat::S32);
        assert!(n.error.is_none(), "{n:?}");
        assert_eq!(h.state.built_substitute.as_ref(), Some(&light));
        assert_eq!(h.state.cfg, cfg, "the request stays the config");
        // The plan no longer names 96 kHz: the same config rebuilds.
        *h.state.rate_plan.lock().unwrap() = crate::api::RatePlan::default();
        h.state
            .apply_config(
                &cfg,
                &h.mirror,
                EngineState::Playing,
                96_000,
                Some(SinkChoice::Dsp),
                None,
            )
            .unwrap();
        assert_eq!(h.state.built_substitute, None);
    }

    struct BuilderProcHarness {
        state: BuilderState,
        mirror: SharedMirror,
        ab_atomic: Arc<AtomicU8>,
        proc: DaemonProcessor,
        recycle_rx: rtrb::Consumer<ActiveChain>,
        #[allow(dead_code)] // held so the other end stays connected
        gen_handle: Arc<AtomicU64>,
        varispeed_applied: Arc<varispeed::VarispeedApplied>,
        vs_recycle_rx: Option<rtrb::Consumer<Box<audivane_dsp_core::varispeed::Varispeed>>>,
    }

    impl BuilderProcHarness {
        /// Negotiate an open as the driver does: when the engine bypass
        /// resampler's design is still running, the driver falls back and
        /// comes back once it is done, negotiating again.
        fn negotiate(
            &mut self,
            graph_rate: u32,
            ratio: RateRatio,
            sink: SinkChoice,
            format: SampleFormat,
        ) -> NegotiatedChain {
            let n = self.state.apply_negotiated(
                graph_rate,
                ratio,
                sink,
                format,
                &self.mirror,
                &self.ab_atomic,
                None,
            );
            if !n.bypass_pending {
                return n;
            }
            self.wait_bypass_designs();
            self.state.apply_negotiated(
                graph_rate,
                ratio,
                sink,
                format,
                &self.mirror,
                &self.ab_atomic,
                None,
            )
        }

        /// Wait for the engine bypass resampler's designs in flight, and
        /// commit what an edit waited for (the engine's designer callback
        /// does that).
        fn wait_bypass_designs(&mut self) {
            self.state.designer.wait_idle();
            self.state.commit_designed(&self.mirror, None);
        }

        fn new(init_cfg: EngineConfig) -> Self {
            Self::with_mixer(
                init_cfg,
                Arc::new(|_dev, vol_db, _muted: Option<bool>| MixerApplyOutcome::digital(vol_db)),
            )
        }

        fn with_mixer(init_cfg: EngineConfig, mixer: MixerFn) -> Self {
            let (cmd_tx, cmd_rx) = rtrb::RingBuffer::new(32);
            let (swap_tx, swap_rx) = rtrb::RingBuffer::new(32);
            let (recycle_tx, recycle_rx) = rtrb::RingBuffer::new(32);
            let mirror = SharedMirror::new(&init_cfg);
            let ab_atomic = Arc::new(AtomicU8::new(0));
            let meters = MeterBank::new(2);
            let proc = DaemonProcessor::new(
                ActiveChain::Bypass,
                init_cfg.dsp_enabled,
                2,
                1024,
                16,
                48_000,
                Arc::clone(&ab_atomic),
                meters,
                cmd_rx,
                swap_rx,
                recycle_tx,
            )
            .with_initial_volume(initial_volume_target(&init_cfg));
            let shared_volume = proc.volume_handle();
            let gen_handle = proc.active_generation_handle();
            let swap_state = proc.swap_state_handle();
            let varispeed_applied = proc.varispeed_applied();
            let initial_software_db = initial_volume_target(&init_cfg).volume_db;
            let rate_plan = Arc::new(Mutex::new(crate::api::RatePlan::default()));
            let state = BuilderState {
                cfg: init_cfg,
                ir_cache: IrCache::new(),
                ab_side: AbSide::A,
                ab_gains: None,
                last_rate: 48_000,
                last_ratio: RateRatio::ONE,
                last_dsp_ratio: RateRatio::ONE,
                last_built_resample: false,
                out_format: None,
                negotiated_key: None,
                config_generation: 0,
                built_generation: 0,
                next_swap_seq: 0,
                swap_state,
                in_flight_cfgs: Vec::new(),
                varispeed_applied: Arc::clone(&varispeed_applied),
                pending_publication: None,
                has_pending_swap: Arc::new(AtomicBool::new(false)),
                shared_volume,
                mixer,
                software_db: initial_software_db,
                hw_known: None,
                hw_known_device: None,
                pending_hw: None,
                output_latency: Arc::new(|| DEFAULT_OUTPUT_LATENCY),
                digital_peak: None,
                driver_error: false,
                driver_state: Arc::new(|| EngineState::Ready),
                failsafe_mutes: failsafe::MuteStore::default(),
                switch_state: Arc::new(|_: &str| Some(true)),
                card_state: Arc::new(|_: &str| CardRead::Error),
                failsafe_mute_verified: false,
                released_marker: None,
                dac_bits: proc.dac_bits_handle(),
                dac_held: true,
                clock: Arc::new(Instant::now),
                sink_port: None,
                cmd_tx,
                swap_tx,
                designer: bypass_design::BypassDesigner::new(bypass_chain_builder()),
                pending_resample: None,
                designing: None,
                bypass_notify: None,
                negotiation_seq: 0,
                design_epoch: 0,
                design_requests: Vec::new(),
                varispeed: varispeed::VarispeedHost::detached(),
                varispeed_speed: 1.0,
                varispeed_requests: 0,
                varispeed_hold: Default::default(),
                varispeed_hold_seq: 0,
                tee: tee::TeeHost::detached(),
                followers: follower_host(Arc::clone(&rate_plan)),
                align: follower::AlignControl::default(),
                comp: follower::CompControl::default(),
                align_t0: Instant::now(),
                rate_plan,
                built_substitute: None,
            };
            Self {
                state,
                mirror,
                ab_atomic,
                proc,
                recycle_rx,
                gen_handle,
                varispeed_applied,
                vs_recycle_rx: None,
            }
        }

        /// Harness with a real `VarispeedHost` / `VarispeedRt` pair connected
        /// through a command ring of `queue_cap` slots.
        fn with_varispeed(init_cfg: EngineConfig, queue_cap: usize) -> Self {
            let mut h = Self::new(init_cfg);
            let (vs_tx, vs_rx) = rtrb::RingBuffer::new(queue_cap);
            let (vs_recycle_tx, vs_recycle_rx) = rtrb::RingBuffer::new(16);
            h.proc = h
                .proc
                .with_varispeed(vs_rx, vs_recycle_tx, DEFAULT_CHANNELS);
            h.varispeed_applied = h.proc.varispeed_applied();
            h.state.varispeed_applied = Arc::clone(&h.varispeed_applied);
            h.state.varispeed = varispeed::VarispeedHost::new(vs_tx, DEFAULT_CHANNELS);
            h.vs_recycle_rx = Some(vs_recycle_rx);
            h
        }

        fn measure_1khz_peak_db(&mut self, ratio: u32) -> f64 {
            use std::f64::consts::PI;
            let block_in = 1024usize;
            let amp = 10.0_f64.powf(-6.0 / 20.0);
            let step = 2.0 * PI * 1000.0 / 48_000.0;
            let mut phase = 0.0_f64;
            let mut out = vec![0_i32; block_in * (ratio as usize) * 2];
            let mut last_peak = 0.0_f64;
            for b in 0..40 {
                let mut inp = Vec::with_capacity(block_in * 2);
                for _ in 0..block_in {
                    let s = (phase.sin() * amp) as f32;
                    inp.push(s);
                    inp.push(s);
                    phase += step;
                }
                audivane_io_pw::EngineAudioProcessor::poll_updates(
                    &mut self.proc,
                    SinkChoice::Dsp,
                    48_000,
                    RateRatio::integer(ratio),
                    true,
                );
                audivane_io_pw::EngineAudioProcessor::process_block(
                    &mut self.proc,
                    SinkChoice::Dsp,
                    &inp,
                    &inp,
                    &mut out,
                    2,
                    RateRatio::integer(ratio),
                    48_000,
                    0,
                );
                if b == 39 {
                    for &s in out.iter().step_by(2) {
                        last_peak = last_peak.max((s as f64).abs() / 2_147_483_648.0);
                    }
                }
            }
            while self.recycle_rx.pop().is_ok() {}
            20.0 * last_peak.log10()
        }
    }

    fn eq_profile(preamp_db: f64, gain_db: f64) -> EqProfile {
        EqProfile {
            name: "test".into(),
            preamp_db,
            bands: vec![Band {
                kind: FilterType::Peaking,
                freq_hz: 1000.0,
                gain_db,
                q: 2.0,
                enabled: true,
            }],
            ..Default::default()
        }
    }

    /// Run a quiet 1 kHz tone through the published chain and return the output.
    fn render_quiet_tone(h: &mut BuilderProcHarness) -> Vec<i32> {
        let block_in = 1024usize;
        let amp = 10.0_f64.powf(-70.0 / 20.0);
        let step = 2.0 * std::f64::consts::PI * 1000.0 / 48_000.0;
        let mut phase = 0.0_f64;
        let mut all = Vec::new();
        let mut out = vec![0_i32; block_in * 2];
        for _ in 0..8 {
            let mut inp = Vec::with_capacity(block_in * 2);
            for _ in 0..block_in {
                let s = (phase.sin() * amp) as f32;
                inp.push(s);
                inp.push(s);
                phase += step;
            }
            audivane_io_pw::EngineAudioProcessor::poll_updates(
                &mut h.proc,
                SinkChoice::Dsp,
                48_000,
                RateRatio::integer(1),
                true,
            );
            audivane_io_pw::EngineAudioProcessor::process_block(
                &mut h.proc,
                SinkChoice::Dsp,
                &inp,
                &inp,
                &mut out,
                2,
                RateRatio::integer(1),
                48_000,
                0,
            );
            all.extend_from_slice(&out);
        }
        while h.recycle_rx.pop().is_ok() {}
        all
    }

    /// What a -6 dBFS 1 kHz tone at 44.1 kHz played to `sink` at `ratio`
    /// gives ([`tone_at`]): peak (dBFS) of the last block, whether a block
    /// was silent.
    fn tone_peak_44k(
        h: &mut BuilderProcHarness,
        sink: SinkChoice,
        ratio: RateRatio,
    ) -> (f64, bool) {
        let t = tone_at(h, 44_100, sink, ratio);
        (t.peak_db, t.silent)
    }

    struct Tone {
        peak_db: f64,
        silent: bool,
        /// The last block repeats every frame `ratio` times (sample and
        /// hold instead of a resampler).
        repeated: bool,
    }

    /// A -6 dBFS 1 kHz tone at `rate` played to `sink` at `ratio`.
    fn tone_at(h: &mut BuilderProcHarness, rate: u32, sink: SinkChoice, ratio: RateRatio) -> Tone {
        let block_in = 1024usize;
        let amp = 10.0_f64.powf(-6.0 / 20.0);
        let step = 2.0 * std::f64::consts::PI * 1000.0 / f64::from(rate);
        let mut phase = 0.0_f64;
        let mut pos = 0u64;
        let mut peak = 0.0_f64;
        let mut silent = false;
        let mut repeated = false;
        for b in 0..40 {
            let mut inp = Vec::with_capacity(block_in * 2);
            for _ in 0..block_in {
                let s = (phase.sin() * amp) as f32;
                inp.push(s);
                inp.push(s);
                phase += step;
            }
            let mut out = vec![0_i32; ratio.frames_out(pos, block_in) * 2];
            audivane_io_pw::EngineAudioProcessor::poll_updates(
                &mut h.proc,
                sink,
                rate,
                ratio,
                true,
            );
            audivane_io_pw::EngineAudioProcessor::process_block(
                &mut h.proc,
                sink,
                &inp,
                &inp,
                &mut out,
                2,
                ratio,
                rate,
                pos,
            );
            pos += block_in as u64;
            // The filter delay fills the first blocks with its ramp-in.
            if b >= 4 {
                silent |= out.iter().all(|&v| v == 0);
            }
            if b == 39 {
                for &v in out.iter().step_by(2) {
                    peak = peak.max((v as f64).abs() / 2_147_483_648.0);
                }
                let k = ratio.up as usize;
                repeated = ratio.down == 1
                    && k > 1
                    && out
                        .chunks(2 * k)
                        .all(|f| f.chunks(2).all(|frame| frame == &f[..2]));
            }
        }
        while h.recycle_rx.pop().is_ok() {}
        Tone {
            peak_db: 20.0 * peak.log10(),
            silent,
            repeated,
        }
    }

    #[test]
    fn a_newer_swap_never_strands_a_resume() {
        // A reopen at 160/147 gates its resume on the chain it negotiated;
        // edits land before the RT thread takes it (latest wins). The
        // resume completes on the newest chain. (A switch to Direct in
        // between is the RT thread's: it reopens when the DSP sink's chain
        // would not fit.)
        let cross = RateRatio::new(160, 147);
        let mut cfg = test_cfg();
        cfg.chain.upsampler = audivane_dsp_core::resample::ManualConfig {
            target: RateTarget::MaxRateAny(48_000),
            preset: Some(audivane_protocol::FilterPreset::GaussianLong),
            ..Default::default()
        }
        .into_upsampler_config()
        .unwrap();
        cfg.chain.eq = Some(eq_profile(0.0, 0.0));
        let mut h = BuilderProcHarness::new(cfg.clone());
        let chain = h.negotiate(44_100, cross, SinkChoice::Dsp, SampleFormat::S32);
        let gate = chain.required_chain_seq.expect("gated");
        let mut edited = cfg.clone();
        for gain in [-3.0, -6.0, -12.0] {
            edited.chain.eq = Some(eq_profile(0.0, gain));
            h.state
                .apply_config(
                    &edited,
                    &h.mirror,
                    EngineState::Switching,
                    44_100,
                    Some(SinkChoice::Dsp),
                    None,
                )
                .unwrap();
        }
        let (db, silent) = tone_peak_44k(&mut h, SinkChoice::Dsp, cross);
        assert!(audivane_io_pw::EngineAudioProcessor::is_chain_ready(
            &h.proc,
            SinkChoice::Dsp,
            cross,
            Some(gate)
        ));
        assert!(!silent);
        assert!((db + 18.0).abs() < 0.5, "{db:.2} dBFS");
    }

    #[test]
    fn the_config_s_followers_run_and_an_outputs_edit_leaves_the_master_alone() {
        use audivane_protocol::ipc::{FollowerConfig, FollowerState as W};
        let (a, b) = ("hw:CARD=NoSuchA,DEV=0", "hw:CARD=NoSuchB,DEV=0");
        let cfg = test_cfg();
        let mut h = BuilderProcHarness::new(cfg.clone());
        h.negotiate(48_000, RateRatio::ONE, SinkChoice::Dsp, SampleFormat::S32);
        let swap_seq = h.state.next_swap_seq;
        let mut with = cfg.clone();
        with.outputs.followers = vec![
            FollowerConfig::new(a),
            FollowerConfig {
                enabled: false,
                trim_ms: 1.5,
                volume_db: -6.0,
                ..FollowerConfig::new(b)
            },
        ];
        let apply = |h: &mut BuilderProcHarness, c: &EngineConfig| {
            h.state
                .apply_config(
                    c,
                    &h.mirror,
                    EngineState::Playing,
                    48_000,
                    Some(SinkChoice::Dsp),
                    None,
                )
                .unwrap()
        };
        assert!(!apply(&mut h, &with), "an outputs edit reopens nothing");
        assert_eq!(h.state.next_swap_seq, swap_seq, "nor rebuilds the master");
        assert!(h.state.followers.contains(a) && h.state.followers.contains(b));
        let c = h.state.followers.controls(b).unwrap();
        assert_eq!(
            (c.trim_ms(), c.volume_db(), c.enabled()),
            (1.5, -6.0, false)
        );
        let st = h.state.outputs_status();
        assert_eq!(st.followers.len(), 2);
        assert_eq!(st.followers[0].device, a);
        assert_eq!(st.followers[1].trim_ms, 1.5);
        assert!(st.followers.iter().all(|f| f.state != W::Locked));

        // An edit sets the controls; a follower playing late cannot play
        // earlier, so a trim 3.5 ms earlier goes into its lateness.
        c.set_lag_ms(4.0);
        let mut edited = with.clone();
        edited.outputs.followers[1].enabled = true;
        edited.outputs.followers[1].trim_ms = -2.0;
        apply(&mut h, &edited);
        let c = h.state.followers.controls(b).unwrap();
        assert_eq!((c.trim_ms(), c.lag_ms(), c.enabled()), (-2.0, 7.5, true));
        // A later trim takes the lateness back first: 3 ms later, 4.5 ms
        // late; 10 ms later still, aligned again (the rest moves it).
        let mut later = edited.clone();
        later.outputs.followers[1].trim_ms = 1.0;
        apply(&mut h, &later);
        let c = h.state.followers.controls(b).unwrap();
        assert_eq!((c.trim_ms(), c.lag_ms()), (1.0, 4.5));
        later.outputs.followers[1].trim_ms = 11.0;
        apply(&mut h, &later);
        let c = h.state.followers.controls(b).unwrap();
        assert_eq!((c.trim_ms(), c.lag_ms()), (11.0, 0.0));
        let edited = later;

        // Removed from the list: stopped; none left: no alignment.
        let mut one = edited.clone();
        one.outputs.followers.remove(0);
        apply(&mut h, &one);
        assert!(!h.state.followers.contains(a) && h.state.tee.slots().get(a).is_none());
        h.state.align.ms = 7.0;
        apply(&mut h, &cfg);
        assert!(h.state.followers.is_empty());
        assert_eq!(h.state.align.ms, 0.0);
        assert_eq!(h.state.outputs_status(), OutputsStatus::default());
        assert_eq!(h.state.next_swap_seq, swap_seq);
    }

    #[test]
    fn apply_config_varispeed_only_change_skips_chain_rebuild_and_checks_budget() {
        let r = RateRatio::new(147, 160);
        let mut cfg = test_cfg();
        cfg.chain.upsampler = audivane_dsp_core::resample::ManualConfig {
            target: RateTarget::Rate(705_600),
            taps: 64,
            taps_unit: audivane_protocol::TapsUnit::Input,
            cutoff: 0.95,
            ..Default::default()
        }
        .into_upsampler_config()
        .unwrap();
        let mut h = BuilderProcHarness::new(cfg.clone());
        h.negotiate(768_000, r, SinkChoice::Dsp, SampleFormat::S32);
        let swap_seq_before = h.state.next_swap_seq;

        // Enabling varispeed at 768 -> 705.6 kHz with 64 taps breaks the
        // static budget and is refused without committing anything.
        let mut over = cfg.clone();
        over.varispeed.enabled = true;
        over.varispeed.speed = 1.05;
        let err = h
            .state
            .apply_config(
                &over,
                &h.mirror,
                EngineState::Playing,
                768_000,
                Some(SinkChoice::Dsp),
                None,
            )
            .unwrap_err();
        assert!(err.message.contains("varispeed at 768000 Hz"), "{err}");
        assert!(!h.state.cfg.varispeed.enabled);
        assert_eq!(h.state.varispeed_requests, 0);
        assert_eq!(h.state.next_swap_seq, swap_seq_before);

        // At 48 kHz x1, enabling varispeed via set_config skips the chain
        // rebuild and bumps varispeed_requests.
        let cfg48 = test_cfg();
        let mut h48 = BuilderProcHarness::new(cfg48.clone());
        h48.negotiate(48_000, RateRatio::ONE, SinkChoice::Dsp, SampleFormat::S32);
        let seq48 = h48.state.next_swap_seq;
        let mut vs_on = cfg48;
        vs_on.varispeed.enabled = true;
        vs_on.varispeed.speed = 1.1;
        let reopen = h48
            .state
            .apply_config(
                &vs_on,
                &h48.mirror,
                EngineState::Playing,
                48_000,
                Some(SinkChoice::Dsp),
                None,
            )
            .unwrap();
        assert!(!reopen);
        assert_eq!(
            h48.state.next_swap_seq, seq48,
            "varispeed-only edit must not rebuild the chain"
        );
        assert!(h48.state.varispeed.requested());
        assert_eq!(h48.state.varispeed_speed, 1.1);
        assert_eq!(h48.state.varispeed_requests, 1);
    }

    #[test]
    fn rejected_live_varispeed_request_never_retires_accepted_stage() {
        use audivane_io_pw::EngineAudioProcessor;
        let r = RateRatio::new(147, 160);
        let mut cfg = test_cfg();
        cfg.chain.upsampler = audivane_dsp_core::resample::ManualConfig {
            target: RateTarget::Rate(705_600),
            taps: 16,
            taps_unit: audivane_protocol::TapsUnit::Input,
            cutoff: 0.95,
            ..Default::default()
        }
        .into_upsampler_config()
        .unwrap();
        let mut h = BuilderProcHarness::with_varispeed(cfg.clone(), 8);
        h.negotiate(768_000, r, SinkChoice::Dsp, SampleFormat::S32);

        // Enable varispeed on the accepted 16-tap chain and install it on RT.
        h.state
            .set_varispeed(true, 1.05, 200.0, &h.mirror, None)
            .unwrap();
        h.proc.poll_updates(SinkChoice::Dsp, 768_000, r, false);
        assert_eq!(h.state.varispeed.built_rate(), Some(768_000));
        assert!(h.varispeed_applied.snapshot().enabled);

        // An expensive 80-tap outgoing chain is still in flight while the live
        // config is 16 taps: a live set_varispeed(true, ...) fails the budget and
        // leaves the accepted stage and its state alone.
        let mut heavy = cfg;
        heavy.chain.upsampler = audivane_dsp_core::resample::ManualConfig {
            target: RateTarget::Rate(705_600),
            taps: 80,
            taps_unit: audivane_protocol::TapsUnit::Input,
            cutoff: 0.95,
            ..Default::default()
        }
        .into_upsampler_config()
        .unwrap();
        h.state.in_flight_cfgs.push((99, heavy));
        let err = h
            .state
            .set_varispeed(true, 1.15, 200.0, &h.mirror, None)
            .unwrap_err();
        assert_eq!(err.code, RpcError::INVALID_PARAMS);
        assert!(err.message.contains("varispeed at 768000 Hz"), "{err}");
        assert_eq!(h.state.varispeed.built_rate(), Some(768_000));
        assert!(h.state.varispeed.requested());
        assert_eq!(h.state.varispeed.last_error, None);
        assert_eq!(h.state.cfg.varispeed.speed, 1.05);
        assert_eq!(h.mirror.varispeed_request(), (true, 1.05));
        // RT never received a Disable command.
        h.proc.poll_updates(SinkChoice::Dsp, 768_000, r, false);
        assert!(h.varispeed_applied.snapshot().enabled);
    }

    #[test]
    fn direct_and_dsp_off_play_bypass_at_x1_only() {
        // The EQ cuts the tone by 6 dB; only DSP on applies it. Direct and
        // DSP off open x1 only and play the samples unchanged; Direct away
        // from x1 (between a sink switch and its x1 reopen) is silent and
        // says so, never a chain.
        let mut on = test_cfg();
        on.chain.upsampler.target = RateTarget::Ratio(2);
        on.chain.eq = Some(eq_profile(0.0, -6.0));
        let mut off = on.clone();
        off.dsp_enabled = false;
        let x2 = RateRatio::integer(2);
        let negotiated = |h: &mut BuilderProcHarness, sink, ratio| {
            h.negotiate(48_000, ratio, sink, SampleFormat::S32)
        };
        let mut h = BuilderProcHarness::new(on.clone());
        negotiated(&mut h, SinkChoice::Dsp, x2);
        let t = tone_at(&mut h, 48_000, SinkChoice::Dsp, x2);
        assert!(!t.silent && !t.repeated);
        assert!((t.peak_db + 12.0).abs() < 0.5, "DSP: {:.2} dBFS", t.peak_db);

        // Still at x2 when the switch to Direct lands: nothing plays.
        let t = tone_at(&mut h, 48_000, SinkChoice::Direct, x2);
        assert!(t.peak_db.is_infinite() && t.silent);
        assert!(h.proc.meters().bypass_muted());
        // Direct's x1 reopen: the samples unchanged, no chain to wait for.
        let chain = negotiated(&mut h, SinkChoice::Direct, RateRatio::ONE);
        assert!(chain.required_chain_seq.is_none());
        let t = tone_at(&mut h, 48_000, SinkChoice::Direct, RateRatio::ONE);
        assert!(!t.silent);
        assert!(
            (t.peak_db + 6.0).abs() < 0.05,
            "Direct: {:.2} dBFS",
            t.peak_db
        );
        assert!(!h.proc.meters().bypass_muted());

        // DSP off on the DSP sink: x1, bypass.
        let mut h = BuilderProcHarness::new(off);
        negotiated(&mut h, SinkChoice::Dsp, RateRatio::ONE);
        let (db, silent) = tone_peak_44k(&mut h, SinkChoice::Dsp, RateRatio::ONE);
        assert!(!silent);
        assert!((db + 6.0).abs() < 0.05, "DSP off: {db:.2} dBFS");
        assert!(!h.mirror.dsp_has_effect.load(Ordering::Acquire));
    }

    /// The engine leg never touches a native rate: Direct and DSP off at
    /// x1 stay an exact copy and wait for no chain, in engine mode.
    #[test]
    fn native_rates_stay_bit_perfect_with_the_engine_resampler() {
        let mut cfg = test_cfg();
        assert_eq!(cfg.rate.bypass_resampler, BypassResampler::Engine);
        cfg.chain.eq = Some(eq_profile(0.0, -6.0));
        cfg.chain.upsampler.target = RateTarget::Ratio(4);
        let mut off = cfg.clone();
        off.dsp_enabled = false;
        for (c, sink) in [(cfg, SinkChoice::Direct), (off, SinkChoice::Dsp)] {
            let mut h = BuilderProcHarness::new(c);
            let chain = h.negotiate(44_100, RateRatio::ONE, sink, SampleFormat::S32);
            // Direct waits for no chain (DSP off gates its x1 bypass chain).
            if sink == SinkChoice::Direct {
                assert!(chain.required_chain_seq.is_none());
            }
            audivane_io_pw::EngineAudioProcessor::poll_updates(
                &mut h.proc,
                sink,
                44_100,
                RateRatio::ONE,
                true,
            );
            assert_eq!(
                h.proc.mode(sink, RateRatio::ONE),
                processor::PlayMode::Bypass
            );
            assert!(audivane_io_pw::EngineAudioProcessor::is_chain_ready(
                &h.proc,
                sink,
                RateRatio::ONE,
                None
            ));
            let inp: Vec<f32> = (0..2048)
                .map(|i| ((i as f32) * 0.37).sin() * 0.7 + (i % 3) as f32 * 1e-7)
                .collect();
            let mut want = vec![0_i32; inp.len()];
            audivane_dsp_core::bypass::f32_to_i32(&inp, &mut want);
            let mut out = vec![0_i32; inp.len()];
            audivane_io_pw::EngineAudioProcessor::process_block(
                &mut h.proc,
                sink,
                &inp,
                &inp,
                &mut out,
                2,
                RateRatio::ONE,
                44_100,
                0,
            );
            assert_eq!(out, want, "{sink:?} is not an exact copy");
            assert_eq!(
                audivane_io_pw::EngineAudioProcessor::latency_out_frames(&h.proc, sink),
                0
            );
            let st = h
                .mirror
                .bypass_status(
                    EngineState::Playing,
                    true,
                    Some(sink),
                    RateRatio::ONE,
                    false,
                )
                .unwrap();
            assert!(!st.active && st.reason.is_none());
        }
    }

    /// A resample-only chain that fails to build at run time tells the
    /// driver to fall back to PipeWire (no chain, no gate), and status says
    /// why once the graph is back at a native rate.
    #[test]
    fn an_unbuildable_resample_chain_falls_back_to_pipewire() {
        let mut h = BuilderProcHarness::new(test_cfg());
        // No engine rate converts to 3.072 MHz (a term above 4096).
        let r = RateRatio::from_rates(44_100, 3_072_000);
        let chain = h.negotiate(44_100, r, SinkChoice::Direct, SampleFormat::S32);
        assert!(chain.bypass_unbuildable);
        assert!(chain.required_chain_seq.is_none() && !chain.fatal);
        let st = h
            .mirror
            .bypass_status(
                EngineState::Playing,
                true,
                Some(SinkChoice::Direct),
                RateRatio::ONE,
                false,
            )
            .unwrap();
        assert!(!st.active);
        assert!(st.reason.unwrap().contains("PipeWire converts instead"));
        // A successful build clears it.
        let ok = RateRatio::from_rates(44_100, 48_000);
        let chain = h.negotiate(44_100, ok, SinkChoice::Direct, SampleFormat::S32);
        assert!(!chain.bypass_unbuildable);
        let st = h
            .mirror
            .bypass_status(
                EngineState::Playing,
                true,
                Some(SinkChoice::Direct),
                RateRatio::ONE,
                false,
            )
            .unwrap();
        assert!(st.reason.is_none());
        // PipeWire mode: no engine leg in status reasons.
        let mut pw = test_cfg();
        pw.rate.bypass_resampler = BypassResampler::Pipewire;
        let h = BuilderProcHarness::new(pw);
        let st = h
            .mirror
            .bypass_status(
                EngineState::Playing,
                true,
                Some(SinkChoice::Direct),
                ok,
                true,
            )
            .unwrap();
        assert_eq!(st.mode, BypassResampler::Pipewire);
        assert!(!st.active && st.reason.is_none());
    }

    #[test]
    fn an_edit_waiting_for_a_design_keeps_the_previous_chain_then_swaps() {
        // DSP on at 160/147, then DSP off while the resample-only design is
        // still running: the DSP chain keeps playing (no silence), and the
        // resample-only chain swaps in once the design is done.
        let r = RateRatio::from_rates(44_100, 48_000);
        let mut on = test_cfg();
        on.chain.upsampler = UpsamplerConfig {
            target: RateTarget::Rate(48_000),
            ..crate::control::config::bypass_upsampler()
        };
        let mut off = on.clone();
        off.dsp_enabled = false;
        let mut h = BuilderProcHarness::new(on.clone());
        let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let g = Arc::clone(&gate);
        let real = bypass_chain_builder();
        h.state.designer = bypass_design::BypassDesigner::new(Arc::new(move |k| {
            let (open, cv) = &*g;
            let mut open = open.lock().unwrap();
            while !*open {
                open = cv.wait(open).unwrap();
            }
            real(k)
        }));
        let mut key = probed(&dac(&[48_000]));
        key.graph_rate = 44_100;
        h.state.negotiated_key = Some(key);
        let n = h.state.apply_negotiated(
            44_100,
            r,
            SinkChoice::Dsp,
            SampleFormat::S32,
            &h.mirror,
            &h.ab_atomic,
            None,
        );
        assert!(n.required_chain_seq.is_some() && !n.bypass_pending);
        assert!(!tone_at(&mut h, 44_100, SinkChoice::Dsp, r).silent);
        let reopen = h
            .state
            .apply_config(
                &off,
                &h.mirror,
                EngineState::Playing,
                44_100,
                Some(SinkChoice::Dsp),
                None,
            )
            .unwrap();
        assert!(!reopen);
        assert!(!h.state.last_built_resample);
        assert!(h.state.pending_resample.is_some());
        // DSP is off, its chain still plays until the design is done.
        let t = tone_at(&mut h, 44_100, SinkChoice::Dsp, r);
        assert!(!t.silent && !h.proc.meters().bypass_muted());
        assert!(!h.proc.chain_fits(SinkChoice::Dsp, r));
        {
            let (open, cv) = &*gate;
            *open.lock().unwrap() = true;
            cv.notify_all();
        }
        h.wait_bypass_designs();
        assert!(h.state.last_built_resample);
        let t = tone_at(&mut h, 44_100, SinkChoice::Dsp, r);
        assert!(!t.silent);
        assert!(h.proc.chain_fits(SinkChoice::Dsp, r));
    }

    type DesignGate = Arc<(Mutex<bool>, std::sync::Condvar)>;

    /// A designer whose builds wait until [`open_design_gate`], then build
    /// the real chain, or fail for keys `fails` picks.
    fn gated_designer(
        fails: impl Fn(&bypass_design::DesignKey) -> bool + Send + Sync + 'static,
    ) -> (bypass_design::BypassDesigner<ActiveChain>, DesignGate) {
        let gate: DesignGate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let g = Arc::clone(&gate);
        let real = bypass_chain_builder();
        let designer = bypass_design::BypassDesigner::new(Arc::new(move |k| {
            let (open, cv) = &*g;
            let mut open = open.lock().unwrap();
            while !*open {
                open = cv.wait(open).unwrap();
            }
            drop(open);
            if fails(k) {
                Err("no design for this test".into())
            } else {
                real(k)
            }
        }));
        (designer, gate)
    }

    fn open_design_gate(gate: &DesignGate) {
        let (open, cv) = &**gate;
        *open.lock().unwrap() = true;
        cv.notify_all();
    }

    /// DSP on at 160/147 (44.1 kHz into a 48 kHz-only DAC), negotiated,
    /// with `designer` for the resample-only chain.
    fn dsp_on_at_160_147(
        designer: bypass_design::BypassDesigner<ActiveChain>,
    ) -> (BuilderProcHarness, EngineConfig, RateRatio) {
        let r = RateRatio::from_rates(44_100, 48_000);
        let mut on = test_cfg();
        on.chain.upsampler = UpsamplerConfig {
            target: RateTarget::Rate(48_000),
            ..crate::control::config::bypass_upsampler()
        };
        let mut h = BuilderProcHarness::new(on.clone());
        h.state.designer = designer;
        let mut key = probed(&dac(&[48_000]));
        key.graph_rate = 44_100;
        h.state.negotiated_key = Some(key);
        let n = h.state.apply_negotiated(
            44_100,
            r,
            SinkChoice::Dsp,
            SampleFormat::S32,
            &h.mirror,
            &h.ab_atomic,
            None,
        );
        assert!(n.required_chain_seq.is_some() && !n.bypass_pending);
        assert!(!tone_at(&mut h, 44_100, SinkChoice::Dsp, r).silent);
        (h, on, r)
    }

    #[test]
    fn a_failed_design_ends_a_dsp_toggles_hold() {
        // As above, but the design fails: the DSP chain stops playing (DSP
        // is off) and the pair falls silent, as a toggle with no chain on
        // its way does, with the reason in status.
        let (designer, gate) = gated_designer(|_| true);
        let (mut h, on, r) = dsp_on_at_160_147(designer);
        let mut off = on.clone();
        off.dsp_enabled = false;
        for cfg in [off.clone(), {
            let mut q = off.clone();
            q.volume.volume_db = -10.0;
            q
        }] {
            h.state
                .apply_config(
                    &cfg,
                    &h.mirror,
                    EngineState::Playing,
                    44_100,
                    Some(SinkChoice::Dsp),
                    None,
                )
                .unwrap();
        }
        assert!(!tone_at(&mut h, 44_100, SinkChoice::Dsp, r).silent);
        open_design_gate(&gate);
        h.wait_bypass_designs();
        assert!(h.state.pending_resample.is_none() && !h.state.last_built_resample);
        let t = tone_at(&mut h, 44_100, SinkChoice::Dsp, r);
        assert!(t.silent && h.proc.meters().bypass_muted());
        // (Status once the driver's fallback opened at 48 kHz, x1.)
        let reason = h
            .mirror
            .bypass_status(
                EngineState::Playing,
                true,
                Some(SinkChoice::Dsp),
                RateRatio::ONE,
                false,
            )
            .and_then(|s| s.reason);
        assert!(
            reason
                .as_deref()
                .is_some_and(|r| r.contains("no design for this test")),
            "{reason:?}"
        );
    }

    /// Streams a steady half-scale 50 Hz tone through the DSP sink at 44.1
    /// kHz into a 48 kHz-only DAC (160/147), with `from` negotiated, and
    /// applies `to` live (a DSP toggle at the same ratio: no reopen) after
    /// 20 blocks. Returns the left channel, in full scale.
    fn stream_across_dsp_toggle(from: &EngineConfig, to: &EngineConfig) -> Vec<f64> {
        let r = RateRatio::from_rates(44_100, 48_000);
        let mut h = BuilderProcHarness::new(from.clone());
        let mut key = probed(&dac(&[48_000]));
        key.graph_rate = 44_100;
        h.state.negotiated_key = Some(key);
        let n = h.negotiate(44_100, r, SinkChoice::Dsp, SampleFormat::S32);
        assert!(!n.bypass_unbuildable && n.error.is_none());
        h.wait_bypass_designs();
        let block_in = 1024usize;
        let step = 2.0 * std::f64::consts::PI * 50.0 / 44_100.0;
        let mut phase = 0.0_f64;
        let mut pos = 0u64;
        let mut left = Vec::new();
        for b in 0..80 {
            if b == 20 {
                let reopen = h
                    .state
                    .apply_config(
                        to,
                        &h.mirror,
                        EngineState::Playing,
                        44_100,
                        Some(SinkChoice::Dsp),
                        None,
                    )
                    .unwrap();
                assert!(!reopen);
            }
            if b > 20 {
                h.wait_bypass_designs();
            }
            let mut inp = Vec::with_capacity(block_in * 2);
            for _ in 0..block_in {
                let v = (phase.sin() * 0.5) as f32;
                inp.extend([v, v]);
                phase += step;
            }
            let mut out = vec![0_i32; r.frames_out(pos, block_in) * 2];
            audivane_io_pw::EngineAudioProcessor::poll_updates(
                &mut h.proc,
                SinkChoice::Dsp,
                44_100,
                r,
                true,
            );
            audivane_io_pw::EngineAudioProcessor::process_block(
                &mut h.proc,
                SinkChoice::Dsp,
                &inp,
                &inp,
                &mut out,
                2,
                r,
                44_100,
                pos,
            );
            pos += block_in as u64;
            left.extend(
                out.iter()
                    .step_by(2)
                    .map(|&v| f64::from(v) / 2_147_483_648.0),
            );
            while h.recycle_rx.pop().is_ok() {}
        }
        left
    }

    /// The largest step between adjacent samples after the toggle, against
    /// the tone's own steady step before it.
    fn toggle_steps(left: &[f64]) -> (f64, f64) {
        let per_block = left.len() / 80;
        let steady = left[10 * per_block..20 * per_block]
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0, f64::max);
        let after = left[19 * per_block..]
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0, f64::max);
        (steady, after)
    }

    #[test]
    fn a_same_ratio_dsp_toggle_never_steps() {
        // DSP on with the bypass resampler's own design (equal latency: a
        // crossfade), and with a linear-phase design and an EQ (other
        // latency: a fade through silence), both directions. No adjacent
        // samples step more than the tone itself does, plus a small bound.
        let mut same = test_cfg();
        same.chain.upsampler = UpsamplerConfig {
            target: RateTarget::Rate(48_000),
            ..crate::control::config::bypass_upsampler()
        };
        let mut other = test_cfg();
        other.chain.upsampler = audivane_dsp_core::resample::ManualConfig {
            target: RateTarget::Rate(48_000),
            taps: 64,
            taps_unit: audivane_protocol::TapsUnit::Input,
            ..Default::default()
        }
        .into_upsampler_config()
        .unwrap();
        other.chain.eq = Some(eq_profile(0.0, -1.0));
        for on in [same, other] {
            let mut off = on.clone();
            off.dsp_enabled = false;
            for (from, to, what) in [(&on, &off, "on -> off"), (&off, &on, "off -> on")] {
                let left = stream_across_dsp_toggle(from, to);
                let (steady, after) = toggle_steps(&left);
                assert!(steady > 0.001, "{what}: no steady tone ({steady})");
                assert!(
                    after <= steady + 0.005,
                    "{what}: a step of {after:.6} against the tone's {steady:.6}"
                );
                // And it keeps playing: no block falls silent.
                let per_block = left.len() / 80;
                for (b, chunk) in left.chunks(per_block).enumerate().skip(5) {
                    let peak = chunk.iter().fold(0.0_f64, |m, v| m.max(v.abs()));
                    assert!(peak > 0.05, "{what}: block {b} is silent ({peak})");
                }
            }
        }
    }

    #[test]
    fn s16_device_dithers_at_16_bits_even_when_configured_for_24() {
        // "Dither to 24" (the default) with an EQ so the chain is active.
        let mut cfg = test_cfg();
        cfg.chain.eq = Some(eq_profile(-1.0, -1.0));
        cfg.chain.output.bits = 24;
        cfg.chain.output.dither = audivane_protocol::DitherKind::Tpdf;

        // Device negotiated S16 (e.g. a 48 kHz-only USB speaker): every sample must
        // already be a 16-bit value with dither noise in it, so the S16
        // packer drops nothing.
        let mut h = BuilderProcHarness::new(cfg.clone());
        h.negotiate(
            48_000,
            RateRatio::integer(1),
            SinkChoice::Dsp,
            SampleFormat::S16,
        );
        let out = render_quiet_tone(&mut h);
        let tail = &out[out.len() / 2..];
        assert!(
            tail.iter().all(|s| s & 0xFFFF == 0),
            "not quantized to 16 bits"
        );
        let distinct: std::collections::HashSet<i32> = tail.iter().copied().collect();
        assert!(distinct.len() >= 3, "no dither at 16 bits: {distinct:?}");

        // Same config on an S32 device keeps dithering at 24 bits: the low
        // 16 bits carry signal.
        let mut h = BuilderProcHarness::new(cfg);
        h.negotiate(
            48_000,
            RateRatio::integer(1),
            SinkChoice::Dsp,
            SampleFormat::S32,
        );
        let out = render_quiet_tone(&mut h);
        let tail = &out[out.len() / 2..];
        assert!(tail.iter().all(|s| s & 0xFF == 0));
        assert!(
            tail.iter().any(|s| s & 0xFFFF != 0),
            "24-bit dither expected"
        );
    }

    #[test]
    fn an_edit_never_discards_the_chain_a_reopen_waits_for() {
        // 48 kHz x1 with a 48 kHz target; the swap queue is full of edits.
        // The graph moves to 44.1 kHz: the negotiated 160/147 chain is
        // deferred, and an EQ edit lands before it is published. The edit
        // must build for 44.1 kHz at 160/147 too, so the resume completes.
        let cross = RateRatio::new(160, 147);
        let mut cfg = test_cfg();
        cfg.chain.upsampler = audivane_dsp_core::resample::ManualConfig {
            target: RateTarget::Rate(48_000),
            preset: Some(audivane_protocol::FilterPreset::GaussianLong),
            ..Default::default()
        }
        .into_upsampler_config()
        .unwrap();
        let mut h = BuilderProcHarness::new(cfg.clone());
        h.negotiate(48_000, RateRatio::ONE, SinkChoice::Dsp, SampleFormat::S32);
        h.proc.set_swap_drain_paused(true);
        let mut i = 0;
        while h.state.pending_publication.is_none() {
            i += 1;
            let mut filler = cfg.clone();
            filler.chain.eq = Some(eq_profile(-(f64::from(i)) * 0.1, 0.0));
            h.state
                .apply_config(
                    &filler,
                    &h.mirror,
                    EngineState::Playing,
                    48_000,
                    Some(SinkChoice::Dsp),
                    None,
                )
                .unwrap();
            cfg = filler;
        }
        let gate = h
            .state
            .apply_negotiated(
                44_100,
                cross,
                SinkChoice::Dsp,
                SampleFormat::S32,
                &h.mirror,
                &h.ab_atomic,
                None,
            )
            .required_chain_seq
            .expect("gated");
        assert!(h.state.pending_publication.is_some());
        let mut edited = cfg.clone();
        edited.chain.eq = Some(eq_profile(0.0, -6.0));
        h.state
            .apply_config(
                &edited,
                &h.mirror,
                EngineState::Switching,
                44_100,
                Some(SinkChoice::Dsp),
                None,
            )
            .unwrap();
        h.proc.set_swap_drain_paused(false);
        for _ in 0..8 {
            audivane_io_pw::EngineAudioProcessor::poll_updates(
                &mut h.proc,
                SinkChoice::Dsp,
                44_100,
                cross,
                false,
            );
            while h.recycle_rx.pop().is_ok() {}
            h.state.flush_pending_swap(&h.mirror, None);
        }
        assert!(h.state.pending_publication.is_none());
        assert!(
            audivane_io_pw::EngineAudioProcessor::is_chain_ready(
                &h.proc,
                SinkChoice::Dsp,
                cross,
                Some(gate)
            ),
            "the resume waits for a 160/147 chain nobody publishes"
        );
        let (db, silent) = tone_peak_44k(&mut h, SinkChoice::Dsp, cross);
        assert!(!silent && (db + 12.0).abs() < 0.5, "{db:.2} dBFS");
    }

    fn temp_ir_path(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "audivane_engine_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("ir.wav")
    }

    fn write_test_ir(path: &std::path::Path, rate: u32) {
        use hound::{SampleFormat as WavFormat, WavSpec, WavWriter};
        let spec = WavSpec {
            channels: 1,
            sample_rate: rate,
            bits_per_sample: 32,
            sample_format: WavFormat::Float,
        };
        let mut w = WavWriter::create(path, spec).unwrap();
        for i in 0..64 {
            w.write_sample(if i == 0 { 1.0_f32 } else { 0.0 }).unwrap();
        }
        w.finalize().unwrap();
    }

    fn conv_cfg(path: &std::path::Path) -> ConvolutionConfig {
        ConvolutionConfig {
            name: None,
            enabled: true,
            ir_path: path.to_path_buf(),
            gain_db: 0.0,
            matrix: None,
            calibration: false,
        }
    }

    fn guarded_cfg(convs: Vec<ConvolutionConfig>) -> EngineConfig {
        let mut cfg = test_cfg();
        cfg.chain.limiter.mode = audivane_protocol::LimiterMode::AutoHeadroom;
        cfg.chain.convolution = convs;
        cfg
    }

    fn gained(path: &std::path::Path, gain_db: f64) -> ConvolutionConfig {
        ConvolutionConfig {
            gain_db,
            ..conv_cfg(path)
        }
    }

    /// Over the boost cap is refused (chain A, A/B side B, a matrix row
    /// that sums its entries); under it passes.
    #[test]
    fn convolution_over_the_boost_cap_is_refused() {
        let ir = temp_ir_path("conv_cap");
        write_test_ir(&ir, 48_000);
        let mut cache = IrCache::new();

        validate_engine_config(&guarded_cfg(vec![gained(&ir, 11.0)]), &mut cache, 48_000).unwrap();
        let err = validate_engine_config(&guarded_cfg(vec![gained(&ir, 13.0)]), &mut cache, 48_000)
            .unwrap_err();
        assert_eq!(err.code, RpcError::INVALID_PARAMS);
        assert!(
            err.message.contains("+13.0 dB on L") && err.message.contains("+12.0 dB cap"),
            "{err}"
        );
        // Two items in series add up.
        assert!(
            validate_engine_config(
                &guarded_cfg(vec![gained(&ir, 7.0), gained(&ir, 7.0)]),
                &mut cache,
                48_000
            )
            .is_err()
        );
        // The cap is a setting.
        let mut raised = guarded_cfg(vec![gained(&ir, 13.0)]);
        raised.convolution_safety.max_boost_db = 14.0;
        validate_engine_config(&raised, &mut cache, 48_000).unwrap();
        raised.convolution_safety.max_boost_db = 30.0;
        assert!(validate_engine_config(&raised, &mut cache, 48_000).is_err());

        // A/B side B over the cap: refused, naming side B.
        let mut ab = guarded_cfg(Vec::new());
        ab.ab = Some(AbConfig {
            level: LevelMatch::Off,
            headroom: Default::default(),
            ..AbConfig::chain(ChainConfig {
                convolution: vec![gained(&ir, 13.0)],
                ..ab.chain.clone()
            })
        });
        let err = validate_engine_config(&ab, &mut cache, 48_000).unwrap_err();
        assert!(err.message.starts_with("A/B side B:"), "{err}");
        if let Some(b) = ab.ab.as_mut() {
            b.chain_b.convolution = vec![gained(&ir, 6.0)];
        }
        validate_engine_config(&ab, &mut cache, 48_000).unwrap();

        // A matrix row sums its entries: +7 dB from each input into L is
        // +13 dB there, each entry alone under the cap.
        let entry = |input, output, gain_db| audivane_protocol::MatrixEntry {
            input,
            output,
            ir_path: ir.clone(),
            channel: None,
            gain_db,
        };
        let matrix = |entries| ConvolutionConfig {
            name: None,
            enabled: true,
            ir_path: Default::default(),
            gain_db: 0.0,
            matrix: Some(entries),
            calibration: false,
        };
        let over = matrix(vec![entry(0, 0, 7.0), entry(1, 0, 7.0), entry(1, 1, 0.0)]);
        let err = validate_engine_config(&guarded_cfg(vec![over]), &mut cache, 48_000).unwrap_err();
        assert!(err.message.contains("+13.0 dB on L"), "{err}");
        let under = matrix(vec![entry(0, 0, 5.0), entry(1, 0, 5.0), entry(1, 1, 0.0)]);
        validate_engine_config(&guarded_cfg(vec![under]), &mut cache, 48_000).unwrap();
    }

    fn expected_fallback_db() -> f64 {
        // -6 dBFS test sine through the -3 dB preamp, convolution disabled.
        -9.0
    }

    /// The key of an open on the probed device `caps` (any format).
    fn probed(caps: &audivane_protocol::ipc::DeviceCapabilities) -> NegotiatedKey {
        NegotiatedKey {
            graph_rate: 0,
            rates: audivane_io_pw::lifecycle::DeviceRates::playable(Some(caps), &[]),
            refused: Rungs::EMPTY,
            transient: Rungs::EMPTY,
            generation: 0,
            session: 0,
        }
    }

    fn dac(rates: &[u32]) -> audivane_protocol::ipc::DeviceCapabilities {
        use audivane_protocol::ipc::{FormatCapability, RateCapability};
        audivane_protocol::ipc::DeviceCapabilities {
            device: "hw:Loopback,0,0".into(),
            channels: 2,
            channels_min: 2,
            channels_max: 2,
            rates: rates
                .iter()
                .map(|&rate| RateCapability {
                    rate,
                    formats: vec![FormatCapability {
                        format: SampleFormat::S32,
                        sbits: Some(32),
                    }],
                })
                .collect(),
            rate_min: rates.iter().copied().min().unwrap_or(0),
            rate_max: rates.iter().copied().max().unwrap_or(0),
            formats: vec![SampleFormat::S32],
            modes: Vec::new(),
        }
    }

    #[test]
    fn failed_ratio_changing_rebuild_plays_fallback_chain_and_reports_error() {
        let ir_path = temp_ir_path("ratio_fallback");
        write_test_ir(&ir_path, 48_000);

        let mut h = BuilderProcHarness::new(test_cfg());
        let mut cfg = test_cfg();
        cfg.chain.eq = Some(eq_profile(-3.0, 0.0));
        cfg.chain.convolution = vec![conv_cfg(&ir_path)];
        cfg.chain.upsampler = audivane_dsp_core::resample::ManualConfig {
            target: RateTarget::Ratio(4),
            taps: 1024,
            ..Default::default()
        }
        .into_upsampler_config()
        .unwrap();
        // Apply at 44.1 kHz: the IR is validated and cached for 44.1 kHz.
        assert!(
            h.state
                .apply_config(&cfg, &h.mirror, EngineState::Ready, 44_100, None, None)
                .unwrap()
        );

        // The IR becomes unavailable before the ratio-changing reopen to 48 kHz x4.
        std::fs::remove_file(&ir_path).unwrap();

        let neg = h.negotiate(
            48_000,
            RateRatio::integer(4),
            SinkChoice::Dsp,
            SampleFormat::S32,
        );
        assert!(
            !neg.fatal,
            "a convolution failure must fall back to a ratio-valid chain"
        );
        let err = neg
            .error
            .clone()
            .expect("rebuild failure must be surfaced in status");
        assert!(err.contains("IR"), "unexpected error: {err}");
        assert!(
            h.mirror
                .chain_error()
                .is_some_and(|e| e.contains("fallback")),
            "status must report the degraded fallback chain"
        );

        // The fallback chain still satisfies the resume gate's ratio check, so
        // playback resumes instead of staying in `Switching` forever.
        let seq = neg
            .required_chain_seq
            .expect("fallback chain must be published for the resume gate");
        assert!(!audivane_io_pw::EngineAudioProcessor::is_chain_ready(
            &h.proc,
            SinkChoice::Dsp,
            RateRatio::integer(4),
            Some(seq)
        ));
        audivane_io_pw::EngineAudioProcessor::poll_updates(
            &mut h.proc,
            SinkChoice::Dsp,
            48_000,
            RateRatio::integer(4),
            false,
        );
        assert!(audivane_io_pw::EngineAudioProcessor::is_chain_ready(
            &h.proc,
            SinkChoice::Dsp,
            RateRatio::integer(4),
            Some(seq)
        ));

        // EQ survived the fallback; only convolution was disabled.
        let db = h.measure_1khz_peak_db(4);
        assert!(
            (db - expected_fallback_db()).abs() < 0.2,
            "expected {:.1} dBFS fallback, got {db:.3}",
            expected_fallback_db()
        );

        let _ = std::fs::remove_dir_all(ir_path.parent().unwrap());
    }

    #[test]
    fn matrix_convolution_sums_inputs_into_one_output() {
        let ir1 = temp_ir_path("matrix_ir_1");
        let ir2 = temp_ir_path("matrix_ir_2");
        write_test_ir(&ir1, 48_000);
        write_test_ir(&ir2, 48_000);

        let mut h = BuilderProcHarness::new(test_cfg());
        let mut cfg = test_cfg();
        let entry = |input, path: &std::path::Path| audivane_protocol::MatrixEntry {
            input,
            output: 0,
            ir_path: path.to_path_buf(),
            channel: None,
            gain_db: -6.0,
        };
        cfg.chain.convolution = vec![ConvolutionConfig {
            name: Some("Sub".into()),
            enabled: true,
            ir_path: Default::default(),
            gain_db: 0.0,
            matrix: Some(vec![entry(0, &ir1), entry(1, &ir2)]),
            calibration: false,
        }];
        assert!(
            !h.state
                .apply_config(&cfg, &h.mirror, EngineState::Ready, 48_000, None, None)
                .unwrap()
        );
        audivane_io_pw::EngineAudioProcessor::poll_updates(
            &mut h.proc,
            SinkChoice::Dsp,
            48_000,
            RateRatio::integer(1),
            false,
        );
        // The same -6 dBFS sine on L and R, each through -6 dB, summed into
        // the left output: -6 dBFS (one input alone would read -12).
        let db = h.measure_1khz_peak_db(1);
        assert!(
            (db - (-6.0)).abs() < 0.2,
            "expected -6.0 dBFS from the matrix sum, got {db:.3}"
        );

        // An entry out of range is refused with the chain's channels named.
        let mut bad = cfg.clone();
        bad.chain.convolution[0].matrix.as_mut().unwrap()[1].input = 2;
        let err = h
            .state
            .apply_config(&bad, &h.mirror, EngineState::Ready, 48_000, None, None)
            .unwrap_err();
        assert!(err.message.contains("must be below 2"), "{}", err.message);

        let _ = std::fs::remove_dir_all(ir1.parent().unwrap());
        let _ = std::fs::remove_dir_all(ir2.parent().unwrap());
    }

    #[test]
    fn volume_only_fast_path_skips_chain_swap_and_sets_dsp_has_effect() {
        let mut h = BuilderProcHarness::new(test_cfg());
        h.negotiate(
            48_000,
            RateRatio::integer(1),
            SinkChoice::Dsp,
            SampleFormat::S32,
        );
        audivane_io_pw::EngineAudioProcessor::poll_updates(
            &mut h.proc,
            SinkChoice::Dsp,
            48_000,
            RateRatio::integer(1),
            false,
        );
        let initial_seq = h.state.next_swap_seq;
        assert!(!h.mirror.dsp_has_effect.load(Ordering::Acquire));

        // A digital -10 dB volume-only update queues no Chain swap, marks
        // dsp_has_effect = true, and keeps built_generation current.
        let mut vol_cfg = test_cfg();
        vol_cfg.volume.volume_db = -10.0;
        vol_cfg.volume.balance_db = -4.0;
        let reopen = h
            .state
            .apply_config(
                &vol_cfg,
                &h.mirror,
                EngineState::Playing,
                48_000,
                Some(SinkChoice::Dsp),
                None,
            )
            .unwrap();
        assert!(!reopen);
        assert_eq!(h.state.next_swap_seq, initial_seq);
        assert_eq!(h.state.built_generation, h.state.config_generation);
        assert!(h.mirror.dsp_has_effect.load(Ordering::Acquire));

        audivane_io_pw::EngineAudioProcessor::poll_updates(
            &mut h.proc,
            SinkChoice::Dsp,
            48_000,
            RateRatio::integer(1),
            false,
        );
        // -6 dBFS test sine attenuated by -10 dB digital volume on L -> -16 dBFS.
        let db = h.measure_1khz_peak_db(1);
        assert!(
            (db - (-16.0)).abs() < 0.25,
            "expected -16.0 dBFS with digital volume, got {db:.3}"
        );

        // Without a hardware control the Direct path gets the digital volume
        // too (-10 dB, balance -4 dB on the right): never louder than asked.
        let mut expected_i32 = Vec::with_capacity(512 * 2);
        for f in 0..512 {
            let l = (((f * 17_389 - 300_000) & 0x007f_ffff) - (1 << 22)) << 8;
            let r = (((-f * 19_001 + 400_000) & 0x007f_ffff) - (1 << 22)) << 8;
            expected_i32.push(l);
            expected_i32.push(r);
        }
        let mut direct_f32 = vec![0.0_f32; 512 * 2];
        i32_to_f32(&expected_i32, &mut direct_f32);
        let mut direct_out = vec![0_i32; 512 * 2];
        audivane_io_pw::EngineAudioProcessor::process_block(
            &mut h.proc,
            SinkChoice::Direct,
            &direct_f32,
            &direct_f32,
            &mut direct_out,
            2,
            RateRatio::integer(1),
            48_000,
            0,
        );
        let (gl, gr) = (
            audivane_dsp_core::db_to_lin(-10.0),
            audivane_dsp_core::db_to_lin(-14.0),
        );
        for (f, pair) in direct_out.chunks(2).enumerate().skip(480) {
            assert!((f64::from(pair[0]) - f64::from(expected_i32[f * 2]) * gl).abs() <= 1.0);
            assert!((f64::from(pair[1]) - f64::from(expected_i32[f * 2 + 1]) * gr).abs() <= 1.0);
        }
    }

    type MixerLog<T> = Arc<Mutex<Vec<T>>>;

    /// No mode/level transition is ever louder at the DAC than
    /// the louder of the two settings. The hardware is heard at once; the
    /// digital gain of the audio already rendered (one output latency) is
    /// the old one.
    #[test]
    fn no_volume_transition_overshoots_at_the_dac() {
        let mut settings: Vec<(VolumeMode, Option<f64>)> = vec![
            (VolumeMode::Auto, None),
            (VolumeMode::Hardware, None),
            (VolumeMode::Software, None),
        ];
        for hw in [-20.0, 0.0] {
            settings.push((VolumeMode::Software, Some(hw)));
        }
        for hw in [-10.0, 0.0] {
            settings.push((VolumeMode::Hybrid, Some(hw)));
        }
        let levels = [-60.0, -30.0, -10.0, 0.0];
        let mut cases = Vec::new();
        for &(mode, hw) in &settings {
            for &v in &levels {
                cases.push((mode, hw, v));
            }
        }
        let latency = Duration::from_millis(700);
        for &a in &cases {
            for &b in &cases {
                let hw_level = Arc::new(Mutex::new(0.0_f64));
                let hw_cb = Arc::clone(&hw_level);
                let mixer: MixerFn = Arc::new(move |_dev, db, mute: Option<bool>| {
                    let applied = (db.clamp(-64.0, 0.0) * 2.0).floor() / 2.0;
                    *hw_cb.lock().unwrap() = applied;
                    MixerApplyOutcome {
                        hardware_volume: true,
                        hardware_mute: mute.is_some(),
                        volume_db: applied,
                        min_db: -64.0,
                        max_db: 0.0,
                    }
                });
                let mut cfg = test_cfg();
                cfg.device = "hw:CARD=DAC,DEV=0".into();
                let set =
                    |cfg: &mut EngineConfig, (mode, hw, v): (VolumeMode, Option<f64>, f64)| {
                        cfg.volume.mode = mode;
                        cfg.volume.hardware_db = hw;
                        cfg.volume.volume_db = v;
                    };
                set(&mut cfg, a);
                let mut h = BuilderProcHarness::with_mixer(cfg.clone(), mixer);
                h.state.output_latency = Arc::new(move || latency);
                h.negotiate(48_000, RateRatio::ONE, SinkChoice::Dsp, SampleFormat::S32);
                let far = Instant::now() + Duration::from_secs(60);
                h.state.run_pending_hw(far, &h.mirror, None);
                let hw = || *hw_level.lock().unwrap();
                let (hw_a, sw_a) = (hw(), h.state.software_db);
                let total_a = hw_a + sw_a;

                set(&mut cfg, b);
                let t0 = Instant::now();
                h.state
                    .apply_config(&cfg, &h.mirror, EngineState::Ready, 48_000, None, None)
                    .unwrap();
                let (hw_1, sw_1) = (hw(), h.state.software_db);
                if let Some(p) = h.state.pending_hw {
                    assert!(p.due >= t0 + latency, "{a:?} -> {b:?}: raise not delayed");
                }
                h.state.run_pending_hw(far, &h.mirror, None);
                let (hw_2, sw_2) = (hw(), h.state.software_db);
                let total_b = hw_2 + sw_2;
                assert!(
                    total_b <= volume::target_db(&cfg.volume) + 1e-9 || b.0 == VolumeMode::Software,
                    "{a:?} -> {b:?}: settles at {total_b}"
                );
                let bound = total_a.max(total_b) + 1e-9;
                // Old digital audio still playing while the new hardware is
                // heard, then the new digital, then the staged write.
                for (stage, level) in [
                    ("hardware first", hw_1 + sw_a),
                    ("digital", hw_1 + sw_1),
                    ("staged write", hw_2 + sw_1),
                ] {
                    assert!(
                        level <= bound,
                        "{a:?} -> {b:?}: {stage} at {level} dB, bound {bound} dB"
                    );
                }
            }
        }
    }

    /// A second change inside a staged raise's window (a volume nudge,
    /// mute, a non-volume edit, another mode change) never writes the
    /// hardware while louder digital audio is still buffered. Every pair
    /// of settings; only the pairs that stage a
    /// raise are checked further (the others have no window).
    #[test]
    fn changes_inside_a_staged_raise_window_never_overshoot() {
        type Setting = (VolumeMode, Option<f64>, f64);
        let settings: Vec<(VolumeMode, Option<f64>)> = vec![
            (VolumeMode::Auto, None),
            (VolumeMode::Software, None),
            (VolumeMode::Software, Some(-20.0)),
            (VolumeMode::Hybrid, Some(-10.0)),
            (VolumeMode::Hybrid, Some(0.0)),
        ];
        let mut cases: Vec<Setting> = Vec::new();
        for &(mode, hw) in &settings {
            for v in [-30.0, -10.0, 0.0] {
                cases.push((mode, hw, v));
            }
        }
        let latency = Duration::from_millis(700);
        let set = |cfg: &mut EngineConfig, (mode, hw, v): Setting| {
            cfg.volume.mode = mode;
            cfg.volume.hardware_db = hw;
            cfg.volume.volume_db = v;
        };
        let mut base = test_cfg();
        base.device = "hw:CARD=DAC,DEV=0".into();
        // Settled DAC level of a setting (fresh card at 0 dB: an upper bound
        // for `software` without a fixed level).
        let settled = Mutex::new(std::collections::HashMap::new());
        let settled_total = |cfg: &EngineConfig| -> f64 {
            let mut c = cfg.clone();
            c.volume.muted = false;
            c.chain = base.chain.clone();
            let key = format!("{:?}", c.volume);
            if let Some(&t) = settled.lock().unwrap().get(&key) {
                return t;
            }
            let t = DacSim::new(&c, latency).total();
            settled.lock().unwrap().insert(key, t);
            t
        };
        // One thread per starting setting: each pair runs its own simulations.
        let checked: usize = std::thread::scope(|scope| {
            let runs: Vec<_> = cases
                .iter()
                .map(|&a| {
                    let (cases, base, settled_total) = (&cases, &base, &settled_total);
                    scope.spawn(move || {
                        let mut checked = 0;
                        for &b in cases {
                            let mut cfg_a = base.clone();
                            set(&mut cfg_a, a);
                            let mut cfg_b = cfg_a.clone();
                            set(&mut cfg_b, b);
                            // The first run doubles as the probe: no staged
                            // raise, no window to change things in.
                            let mut first = DacSim::new(&cfg_a, latency);
                            let first_t0 = first.t();
                            first.apply(&cfg_b);
                            if first.h.state.pending_hw.is_none() {
                                continue;
                            }
                            let mut first = Some((first, first_t0));
                            let nudge = if b.2 <= -1.0 { b.2 + 1.0 } else { b.2 - 1.0 };
                            let mut c_variants: Vec<(&str, EngineConfig)> = Vec::new();
                            let mut c = cfg_b.clone();
                            c.volume.volume_db = nudge;
                            c_variants.push(("nudge", c));
                            let mut c = cfg_b.clone();
                            c.volume.muted = true;
                            c_variants.push(("mute", c));
                            let mut c = cfg_b.clone();
                            c.chain.eq = Some(eq_profile(-3.0, -2.0));
                            c_variants.push(("eq edit", c));
                            c_variants.push(("back to A", cfg_a.clone()));
                            let mut c = cfg_b.clone();
                            set(&mut c, (VolumeMode::Hybrid, Some(0.0), b.2));
                            c_variants.push(("hybrid 0", c));
                            for (what, cfg_c) in &c_variants {
                                // Inside the window: it ends one latency
                                // plus a margin after B.
                                let dt = 100u64;
                                let (mut sim, t0) = first.take().unwrap_or_else(|| {
                                    let mut sim = DacSim::new(&cfg_a, latency);
                                    let t0 = sim.t();
                                    sim.apply(&cfg_b);
                                    (sim, t0)
                                });
                                let total_a = settled_total(&cfg_a);
                                sim.advance(Duration::from_millis(dt));
                                sim.apply(cfg_c);
                                sim.advance(Duration::from_secs(3));
                                let bound = total_a
                                    .max(settled_total(&cfg_b))
                                    .max(settled_total(cfg_c))
                                    + 1e-9;
                                let (worst, at) = sim.loudest_since(t0);
                                assert!(
                                    worst <= bound,
                                    "{a:?} -> {b:?} -> {what} after {dt} ms: {worst} dB at +{:?}, bound {bound}",
                                    at - t0
                                );
                                checked += 1;
                            }
                        }
                        checked
                    })
                })
                .collect();
            runs.into_iter().map(|r| r.join().unwrap()).sum()
        });
        assert!(checked >= 60, "only {checked} staged raises checked");
    }

    /// A fake clock and a DAC timeline: the hardware level is heard at
    /// once, the digital gain one output latency after it was set.
    struct DacSim {
        h: BuilderProcHarness,
        now: Arc<Mutex<Instant>>,
        hw_log: MixerLog<(Instant, f64)>,
        sw_log: Vec<(Instant, f64)>,
        latency: Duration,
    }

    impl DacSim {
        fn new(cfg: &EngineConfig, latency: Duration) -> Self {
            let start = Instant::now();
            let now = Arc::new(Mutex::new(start));
            let hw_log: MixerLog<(Instant, f64)> = Arc::default();
            let (now_cb, log_cb) = (Arc::clone(&now), Arc::clone(&hw_log));
            let mixer: MixerFn = Arc::new(move |_dev, db, mute: Option<bool>| {
                let applied = (db.clamp(-64.0, 0.0) * 2.0).floor() / 2.0;
                log_cb
                    .lock()
                    .unwrap()
                    .push((*now_cb.lock().unwrap(), applied));
                MixerApplyOutcome {
                    hardware_volume: true,
                    hardware_mute: mute.is_some(),
                    volume_db: applied,
                    min_db: -64.0,
                    max_db: 0.0,
                }
            });
            let mut h = BuilderProcHarness::with_mixer(cfg.clone(), mixer);
            h.state.output_latency = Arc::new(move || latency);
            let now_clock = Arc::clone(&now);
            h.state.clock = Arc::new(move || *now_clock.lock().unwrap());
            h.negotiate(48_000, RateRatio::ONE, SinkChoice::Dsp, SampleFormat::S32);
            let mut sim = Self {
                h,
                now,
                hw_log,
                sw_log: Vec::new(),
                latency,
            };
            // Settle, then start the timeline long after.
            sim.advance(Duration::from_secs(10));
            sim.hw_log.lock().unwrap().retain(|_| false);
            let t = sim.t();
            sim.hw_log.lock().unwrap().push((t, sim.hw_now()));
            sim.sw_log = vec![(t - Duration::from_secs(5), sim.h.state.software_db)];
            sim
        }

        fn t(&self) -> Instant {
            *self.now.lock().unwrap()
        }

        fn total(&self) -> f64 {
            self.hw_now() + self.h.state.software_db
        }

        /// The loudest the DAC played from `from` on.
        fn loudest_since(&self, from: Instant) -> (f64, Instant) {
            let at = |log: &[(Instant, f64)], t: Instant| {
                log.iter()
                    .take_while(|(lt, _)| *lt <= t)
                    .last()
                    .map(|&(_, v)| v)
            };
            let hw_log = self.hw_log.lock().unwrap().clone();
            let mut worst = (f64::NEG_INFINITY, from);
            let mut t = from;
            while t <= self.t() {
                let hw = at(&hw_log, t).unwrap();
                let sw = at(&self.sw_log, t.checked_sub(self.latency).unwrap()).unwrap();
                if hw + sw > worst.0 {
                    worst = (hw + sw, t);
                }
                t += Duration::from_millis(10);
            }
            worst
        }

        fn hw_now(&self) -> f64 {
            self.h.state.hw_known.map_or(0.0, |k| k.outcome.volume_db)
        }

        fn note_sw(&mut self) {
            let t = self.t();
            self.sw_log.push((t, self.h.state.software_db));
        }

        fn apply(&mut self, cfg: &EngineConfig) {
            self.h
                .state
                .apply_config(cfg, &self.h.mirror, EngineState::Ready, 48_000, None, None)
                .unwrap();
            self.note_sw();
        }

        fn advance(&mut self, d: Duration) {
            let end = self.t() + d;
            while self.t() < end {
                let next = self.t() + Duration::from_millis(10);
                *self.now.lock().unwrap() = next;
                self.h.state.run_pending_hw(next, &self.h.mirror, None);
                self.note_sw();
            }
        }
    }

    /// Below the card's range the failsafe mutes the switch (the card's
    /// minimum is still louder than what was heard) and the next
    /// negotiation unmutes it, also across runs (and reboots) through the
    /// persistent store.
    #[test]
    fn failsafe_mutes_below_the_card_range_and_unmutes_on_acquire() {
        let mut cfg = test_cfg();
        cfg.device = "hw:CARD=DAC,DEV=0".into();
        // `software` with a fixed level leaves the switch alone (hybrid
        // drives it with the mute anyway).
        cfg.volume.mode = VolumeMode::Software;
        cfg.volume.hardware_db = Some(0.0);
        cfg.volume.volume_db = -80.0; // the card stops at -64
        let mutes: MixerLog<Option<bool>> = Arc::default();
        // The card: (level, switch on).
        let card = Arc::new(Mutex::new((0.0_f64, true)));
        let sim = |mutes: &MixerLog<Option<bool>>| -> MixerFn {
            let (mutes_cb, card_cb) = (Arc::clone(mutes), Arc::clone(&card));
            Arc::new(move |_dev, db, mute: Option<bool>| {
                mutes_cb.lock().unwrap().push(mute);
                let mut c = card_cb.lock().unwrap();
                c.0 = db.clamp(-64.0, 0.0);
                if let Some(m) = mute {
                    c.1 = !m;
                }
                MixerApplyOutcome {
                    hardware_volume: true,
                    hardware_mute: mute.is_some(),
                    volume_db: c.0,
                    min_db: -64.0,
                    max_db: 0.0,
                }
            })
        };
        let wire = |h: &mut BuilderProcHarness| {
            let (s, l) = (Arc::clone(&card), Arc::clone(&card));
            h.state.switch_state = Arc::new(move |_: &str| Some(s.lock().unwrap().1));
            h.state.card_state = Arc::new(move |_: &str| {
                let (level_db, on) = *l.lock().unwrap();
                CardRead::Channels(vec![PlaybackChannel {
                    level_db,
                    switch_on: Some(on),
                }])
            });
        };
        let mixer = sim(&mutes);
        let dir = std::env::temp_dir().join(format!(
            "audivane-failsafe-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let marker = dir.join("failsafe-muted");
        let _ = std::fs::remove_file(&marker);
        let store = || failsafe::MuteStore::load(Some(marker.clone()));
        let mut h = BuilderProcHarness::with_mixer(cfg.clone(), mixer);
        wire(&mut h);
        h.state.failsafe_mutes = store();
        let negotiate = |h: &mut BuilderProcHarness| {
            h.negotiate(48_000, RateRatio::ONE, SinkChoice::Dsp, SampleFormat::S32);
        };
        negotiate(&mut h);
        h.state.leave_hardware_safe();
        assert_eq!(mutes.lock().unwrap().last(), Some(&Some(true)));
        let m = store().entries()[0].clone();
        assert_eq!(
            (m.card.as_str(), m.device.as_str()),
            ("DAC", cfg.device.as_str())
        );
        assert_eq!(m.level_db, -80.0);
        // Same run: unmuted on acquire, marker gone.
        negotiate(&mut h);
        assert_eq!(mutes.lock().unwrap().last(), Some(&Some(false)));
        assert!(!marker.exists());
        // Later writes do not touch the switch again.
        let n = mutes.lock().unwrap().len();
        cfg.volume.hardware_db = Some(-6.0);
        h.state
            .apply_config(&cfg, &h.mirror, EngineState::Ready, 48_000, None, None)
            .unwrap();
        assert!(mutes.lock().unwrap().len() > n);
        assert!(mutes.lock().unwrap()[n..].iter().all(Option::is_none));
        // Above the card's minimum: no mute.
        cfg.volume.volume_db = -30.0;
        h.state
            .apply_config(&cfg, &h.mirror, EngineState::Ready, 48_000, None, None)
            .unwrap();
        let n = mutes.lock().unwrap().len();
        h.state.leave_hardware_safe();
        assert_eq!(mutes.lock().unwrap()[n..], [None]);
        assert!(!marker.exists());
        // Another card's entry survives an open of this one.
        let mut other = store();
        other.record("hw:CARD=Other,DEV=0", -90.0, None);
        // Another run (or `--volume-failsafe`) recorded a mute and left the
        // card at -64 dB, switch off: the next engine unmutes it even in
        // `software` without a fixed level.
        let mut s = store();
        s.record(
            &cfg.device,
            -80.0,
            Some(vec![PlaybackChannel {
                level_db: -64.0,
                switch_on: Some(false),
            }]),
        );
        *card.lock().unwrap() = (-64.0, false);
        cfg.volume.mode = VolumeMode::Software;
        cfg.volume.hardware_db = None;
        let mut h2 = BuilderProcHarness::with_mixer(cfg.clone(), sim(&mutes));
        wire(&mut h2);
        h2.state.failsafe_mutes = store();
        negotiate(&mut h2);
        assert_eq!(mutes.lock().unwrap().last(), Some(&Some(false)));
        let left = store();
        assert_eq!(left.entries().len(), 1);
        assert_eq!(left.entries()[0].card, "Other");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A switch the user had muted is neither muted by the failsafe nor
    /// recorded, so the next open never unmutes it.
    #[test]
    fn failsafe_never_takes_over_a_switch_the_user_muted() {
        let mut cfg = test_cfg();
        cfg.device = "hw:CARD=DAC,DEV=0".into();
        cfg.volume.mode = VolumeMode::Software;
        cfg.volume.hardware_db = Some(0.0);
        cfg.volume.volume_db = -80.0;
        let mutes: MixerLog<Option<bool>> = Arc::default();
        let mutes_cb = Arc::clone(&mutes);
        let mixer: MixerFn = Arc::new(move |_dev, db, mute: Option<bool>| {
            mutes_cb.lock().unwrap().push(mute);
            MixerApplyOutcome {
                hardware_volume: true,
                hardware_mute: mute.is_some(),
                volume_db: db.clamp(-64.0, 0.0),
                min_db: -64.0,
                max_db: 0.0,
            }
        });
        let mut h = BuilderProcHarness::with_mixer(cfg.clone(), mixer);
        h.state.switch_state = Arc::new(|_: &str| Some(false));
        let negotiate = |h: &mut BuilderProcHarness| {
            h.negotiate(48_000, RateRatio::ONE, SinkChoice::Dsp, SampleFormat::S32);
        };
        negotiate(&mut h);
        h.state.leave_hardware_safe();
        assert!(h.state.failsafe_mutes.entries().is_empty());
        negotiate(&mut h);
        assert!(
            mutes.lock().unwrap().iter().all(Option::is_none),
            "switch touched: {:?}",
            mutes.lock().unwrap()
        );
    }

    /// Switching devices hands card A over at its heard level before the
    /// config moves on, and never writes card B (or unmutes its switch)
    /// before the reopen negotiated it.
    #[test]
    fn a_device_switch_hands_card_a_over_and_leaves_b_alone() {
        let writes: MixerLog<(String, f64, Option<bool>)> = Arc::default();
        let cb = Arc::clone(&writes);
        let mixer: MixerFn = Arc::new(move |dev: &str, db, mute: Option<bool>| {
            let applied = db.clamp(-64.0, 0.0);
            cb.lock().unwrap().push((dev.to_string(), applied, mute));
            MixerApplyOutcome {
                hardware_volume: true,
                hardware_mute: mute.is_some(),
                volume_db: applied,
                min_db: -64.0,
                max_db: 0.0,
            }
        });
        let mut cfg = test_cfg();
        cfg.device = "hw:CARD=CardA,DEV=0".into();
        cfg.volume.mode = VolumeMode::Hybrid;
        cfg.volume.hardware_db = Some(0.0);
        cfg.volume.volume_db = -29.0;
        let mut h = BuilderProcHarness::with_mixer(cfg.clone(), mixer);
        h.state.failsafe_mutes = failsafe::MuteStore::default();
        h.state
            .failsafe_mutes
            .record("hw:CARD=CardB,DEV=0", -80.0, None);
        let negotiate = |h: &mut BuilderProcHarness| {
            h.negotiate(48_000, RateRatio::ONE, SinkChoice::Dsp, SampleFormat::S32);
        };
        negotiate(&mut h);
        assert_eq!(writes.lock().unwrap().last().unwrap().1, 0.0);
        writes.lock().unwrap().clear();
        cfg.device = "hw:CARD=CardB,DEV=0".into();
        h.state
            .apply_config(&cfg, &h.mirror, EngineState::Ready, 48_000, None, None)
            .unwrap();
        let w = writes.lock().unwrap().clone();
        assert_eq!(w.len(), 1, "{w:?}");
        assert_eq!((w[0].0.as_str(), w[0].1), ("hw:CARD=CardA,DEV=0", -29.0));
        assert_eq!(
            h.state.failsafe_mutes.entries().len(),
            1,
            "B's mute taken early"
        );
        // The reopen negotiated B: now B gets its level (and its switch back).
        negotiate(&mut h);
        let w = writes.lock().unwrap().clone();
        let last = w.last().unwrap();
        assert_eq!((last.0.as_str(), last.1), ("hw:CARD=CardB,DEV=0", 0.0));
        assert!(h.state.failsafe_mutes.entries().is_empty());
    }

    /// A card lost without a hand-over (a failed reopen) is never written:
    /// no failsafe write, no edits, until the next negotiation.
    #[test]
    fn a_lost_card_is_never_written() {
        let mut cfg = test_cfg();
        cfg.device = "hw:CARD=DAC,DEV=0".into();
        cfg.volume.mode = VolumeMode::Hybrid;
        cfg.volume.hardware_db = Some(0.0);
        cfg.volume.volume_db = -29.0;
        let mut sim = DacSim::new(&cfg, Duration::from_millis(700));
        let writes = sim.hw_log.lock().unwrap().len();
        sim.h.state.on_release(ReleaseKind::Lost);
        cfg.volume.volume_db = -5.0;
        sim.apply(&cfg);
        sim.h.state.leave_hardware_safe();
        assert_eq!(sim.hw_log.lock().unwrap().len(), writes);
    }

    /// Peak (dBFS) of one streaming block of a -6 dBFS 1 kHz tone through
    /// the processor as it is now, ramps included.
    fn one_block_peak_db(h: &mut BuilderProcHarness) -> f64 {
        let n = 1024usize;
        let amp = 10.0_f64.powf(-6.0 / 20.0);
        let step = 2.0 * std::f64::consts::PI * 1000.0 / 48_000.0;
        let inp: Vec<f32> = (0..n)
            .flat_map(|i| {
                let s = ((i as f64 * step).sin() * amp) as f32;
                [s, s]
            })
            .collect();
        let mut out = vec![0_i32; n * 2];
        audivane_io_pw::EngineAudioProcessor::poll_updates(
            &mut h.proc,
            SinkChoice::Dsp,
            48_000,
            RateRatio::ONE,
            true,
        );
        audivane_io_pw::EngineAudioProcessor::process_block(
            &mut h.proc,
            SinkChoice::Dsp,
            &inp,
            &inp,
            &mut out,
            2,
            RateRatio::ONE,
            48_000,
            0,
        );
        while h.recycle_rx.pop().is_ok() {}
        let peak = out
            .iter()
            .map(|&s| (s as f64).abs() / 2_147_483_648.0)
            .fold(0.0, f64::max);
        20.0 * peak.log10()
    }

    /// With a hardware mixer the start-up never plays louder either: the
    /// digital stand-in only goes up once the card has come down.
    #[test]
    fn start_up_with_a_mixer_is_never_louder_than_the_stored_volume() {
        let cfg = hardware_cfg(-7.0);
        let card = SimCard::new(0.0, true);
        let mut h = SimCard::harness(&card, cfg);
        let first = one_block_peak_db(&mut h);
        assert!(
            first + card.level() <= -13.0 + 0.05,
            "{first:.2} dBFS at the card's {}",
            card.level()
        );
        open(&mut h);
        assert_eq!(card.level(), -7.0);
        let status = h.mirror.volume_status();
        assert_eq!(status.volume_db, -7.0);
        assert!(status.hardware);
        for _ in 0..20 {
            let p = one_block_peak_db(&mut h);
            assert!(
                p + card.level() <= -13.0 + 0.05,
                "{p:.2} dBFS at the card's {}",
                card.level()
            );
        }
    }

    /// A card with a `-64..=0` dB control in 0.5 dB steps and a switch.
    struct SimCard {
        level: Mutex<(f64, bool)>,
    }

    impl SimCard {
        fn new(level_db: f64, switch_on: bool) -> Arc<Self> {
            Arc::new(Self {
                level: Mutex::new((level_db, switch_on)),
            })
        }
        fn step(db: f64) -> f64 {
            (db.clamp(-64.0, 0.0) * 2.0).floor() / 2.0
        }
        fn level(&self) -> f64 {
            self.level.lock().unwrap().0
        }
        fn harness(card: &Arc<Self>, cfg: EngineConfig) -> BuilderProcHarness {
            let w = Arc::clone(card);
            let mixer: MixerFn = Arc::new(move |_dev, db, mute: Option<bool>| {
                let mut l = w.level.lock().unwrap();
                l.0 = SimCard::step(db);
                if let Some(m) = mute {
                    l.1 = !m;
                }
                MixerApplyOutcome {
                    hardware_volume: true,
                    hardware_mute: mute.is_some(),
                    volume_db: l.0,
                    min_db: -64.0,
                    max_db: 0.0,
                }
            });
            BuilderProcHarness::with_mixer(cfg, mixer)
        }
    }

    /// Letting go with the card's level unknown (no readback of our last
    /// write, and reading the card fails) writes nothing in `software` or
    /// `hybrid`: the card may already be below the heard level.
    #[test]
    fn an_unknown_card_level_is_never_written_at_the_hand_over() {
        for mode in [VolumeMode::Software, VolumeMode::Hybrid] {
            let mut cfg = test_cfg();
            cfg.device = "hw:CARD=DAC,DEV=0".into();
            cfg.volume.mode = mode;
            cfg.volume.hardware_db = Some(0.0);
            cfg.volume.volume_db = -30.0;
            let writes: MixerLog<(f64, Option<bool>)> = Arc::default();
            let cb = Arc::clone(&writes);
            let mixer: MixerFn = Arc::new(move |_dev, db, mute: Option<bool>| {
                cb.lock().unwrap().push((db, mute));
                MixerApplyOutcome {
                    hardware_volume: true,
                    hardware_mute: mute.is_some(),
                    volume_db: db.clamp(-64.0, 0.0),
                    min_db: -64.0,
                    max_db: 0.0,
                }
            });
            let mut h = BuilderProcHarness::with_mixer(cfg.clone(), mixer);
            open(&mut h);
            writes.lock().unwrap().clear();
            h.state.hw_known = None;
            h.state.on_release(ReleaseKind::HandOver);
            assert!(writes.lock().unwrap().is_empty(), "{mode:?}");
        }
    }

    fn open(h: &mut BuilderProcHarness) {
        h.negotiate(48_000, RateRatio::ONE, SinkChoice::Dsp, SampleFormat::S32);
    }

    fn hardware_cfg(volume_db: f64) -> EngineConfig {
        let mut cfg = test_cfg();
        cfg.device = "hw:CARD=DAC,DEV=0".into();
        cfg.volume.mode = VolumeMode::Hardware;
        cfg.volume.volume_db = volume_db;
        cfg
    }
}
