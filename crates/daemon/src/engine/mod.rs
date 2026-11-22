//! Real-time side: owns the DAC (ALSA hw), the two PipeWire driver sinks
//! ("Audivane DSP", "Audivane Direct") and the DSP chains.

pub mod bypass_design;
pub mod failsafe;
pub mod follower;
pub mod ir;
pub mod processor;
pub mod spectrum;
pub mod tee;
pub mod varispeed;
pub mod volume;
pub mod waiting;

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

const DEFAULT_CHANNELS: usize = 2;
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
    align_t0: Instant,
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
        if !self.dac_held || self.driver_error {
            self.pending_hw = None;
            let applied = volume::resolve(&vol, &plan, known);
            self.publish_volume(applied, known.map(|o| o.volume_db), mirror, waker);
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
            self.publish_volume(applied, known.map(|o| o.volume_db), mirror, waker);
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

    #[cfg(test)]
    fn shared_volume_target(&self) -> VolumeTarget {
        let mut generation = 0;
        self.shared_volume
            .read_if_changed(&mut generation)
            .unwrap_or_default()
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
        validate_engine_config(cfg, &mut self.ir_cache, current_rate)?;

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
        let volume_only = !force_reopen && (old_without_volume == *cfg || resample_kept);
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
            Some(build_active_chain(
                &cap_dither_bits(cfg, self.out_format),
                &mut self.ir_cache,
                current_rate,
                target_ratio,
                next_side,
            )?)
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
        let cfg_clone = cap_dither_bits(&self.cfg, Some(format));
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
        }
    }

    /// The followers the config lists run, with its controls.
    fn sync_followers(&mut self) {
        let master = self.cfg.device.clone();
        self.followers.sync(&mut self.tee, &master, &self.cfg);
        if self.followers.is_empty() {
            self.align.reset();
        }
    }

    /// The reaper's tick: the master's alignment grows with what the
    /// followers need (when one ran out of source, capped at 60 ms); at
    /// the cap, a follower that needs more plays late by the rest.
    fn tick_followers(&mut self) {
        if self.followers.is_empty() {
            return;
        }
        self.followers.reap();
        self.tee
            .set_command_lead_ms(self.followers.command_lead_ms());
        let now = (self.clock)().saturating_duration_since(self.align_t0);
        let least = self.followers.least_slack_ms();
        if let Some(ms) = self.align.tick(now.as_secs_f64(), least)
            && let Err(e) = self.tee.set_alignment_ms(ms)
        {
            log::warn!("followers: alignment {ms} ms not applied: {e}");
        }
        let at_cap = self.align.settled_at_cap(now.as_secs_f64());
        self.followers.tick_lag(now.as_secs_f64(), at_cap);
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
                follower::view(
                    f,
                    snap,
                    lag.unwrap_or(0.0),
                    self.followers.failure(&f.device),
                )
            })
            .collect();
        OutputsStatus {
            alignment_ms: if self.followers.is_empty() {
                0.0
            } else {
                self.align.ms
            },
            followers,
        }
    }
}

/// The followers' host: their chains are built as the master's, for the
/// follower's device (its ratio and format), chain A only.
fn follower_host() -> follower::FollowerHost {
    let build: follower::ChainBuilder = Arc::new(|cfg, graph_rate, ratio, format| {
        let mut ir_cache = IrCache::new();
        let mut cfg = cap_dither_bits(cfg, Some(format));
        cfg.ab = None;
        build_active_chain(&cfg, &mut ir_cache, graph_rate, ratio, AbSide::A)
            .map(|(chain, _, _)| chain)
            .map_err(|e| e.message)
    });
    follower::FollowerHost::new(
        build,
        Arc::new(desired_ratio_for),
        Arc::new(|cfg: &EngineConfig, rate| scaled_quantum(cfg.quantum.max(MIN_QUANTUM), rate)),
    )
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
    chain_generation: Arc<AtomicU64>,
    driver: EngineDriverHandle,
    device_name: Mutex<String>,
    reaper_stop: Arc<AtomicBool>,
    reaper_paused: Arc<AtomicBool>,
    rt_drain_paused: Arc<AtomicBool>,
    reaper_thread: Mutex<Option<JoinHandle<()>>>,
    /// What the varispeed stage applies (the RT thread publishes it).
    varispeed_applied: Arc<varispeed::VarispeedApplied>,
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
        validate_engine_config(cfg, &mut ir_cache, init_rate)?;

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
        let meters = MeterBank::new(DEFAULT_CHANNELS);
        let (spectrum_rt, spectrum_worker) = spectrum::SpectrumRt::create(DEFAULT_CHANNELS)
            .map_err(|e| {
                EngineError::new(
                    RpcError::INTERNAL,
                    format!("cannot start spectrum analyser: {e}"),
                )
            })?;

        let ab_side_atomic = Arc::new(AtomicU8::new(0));
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
        .with_initial_volume(initial_volume_target(cfg))
        .with_varispeed(vs_rx, vs_recycle_tx, DEFAULT_CHANNELS)
        .with_stretch_recycle(stretch_recycle_tx);
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
            followers: follower_host(),
            align: follower::AlignControl::default(),
            align_t0: Instant::now(),
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
            chain_generation,
            driver,
            device_name: Mutex::new(cfg.device.clone()),
            reaper_stop,
            reaper_paused,
            rt_drain_paused,
            varispeed_applied,
            reaper_thread: Mutex::new(Some(reaper_thread)),
        }))
    }
}

impl Engine for RealEngine {
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

        let (latency_ms, filter_latency_ms) =
            if drv.state != EngineState::Idle && drv.output_rate > 0 {
                let ms_per_frame = 1000.0 / (drv.output_rate as f64);
                let filter = drv.dsp_latency_out_frames as f64;
                let total_out_frames = (drv.alsa_delay_frames.max(0) as f64) + filter;
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

/// The engine bypass resampler's chain ([`ActiveChain::Resample`]) for
/// `graph_rate` at `ratio`: the fixed design alone, then the digital
/// volume and dither to `cfg`'s output section (capped at the format by the
/// caller), [`crate::control::config::bypass_chain_config`].
#[cfg(test)]
fn build_resample_chain(
    cfg: &EngineConfig,
    graph_rate: u32,
    ratio: RateRatio,
) -> Result<ActiveChain, EngineError> {
    build_resample_chain_for(cfg.chain.output, cfg.quantum, graph_rate, ratio)
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
    if old.device != new.device || old.formats != new.formats || old.quantum != new.quantum {
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
    let best = match up.target {
        RateTarget::Off | RateTarget::Rate(_) | RateTarget::MaxRateAny(_) => None,
        RateTarget::MaxRate(cap) => Some(RateTarget::MaxRateAny(cap)),
        RateTarget::Ratio(_) => Some(RateTarget::MaxRateAny(BEST_FOR_DAC_CAP)),
    }
    .map(|target| {
        let mut alt = preview.clone();
        alt.chain.upsampler.target = target;
        (target, to_driver_config(&alt))
    });
    let mut out = Vec::new();
    for &rate in rates {
        let Ok(landing) = audivane_io_pw::predicted_dsp_landing(&driver, Some(caps), rate) else {
            return Vec::new();
        };
        let (ratio, converted_to) = match landing {
            DspLanding::Opens(r) => (r, None),
            DspLanding::Converted(to) => (RateRatio::ONE, Some(to)),
        };
        let best_for_dac = best
            .as_ref()
            .filter(|_| ratio.is_unity())
            .and_then(|(target, alt)| {
                match audivane_io_pw::predicted_dsp_landing(alt, Some(caps), rate) {
                    Ok(DspLanding::Opens(r)) if !r.is_unity() => Some(BestForDac {
                        target: *target,
                        ratio: r,
                    }),
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

fn validate_chain_irs(
    convs: &[audivane_protocol::ConvolutionConfig],
    ir_cache: &mut IrCache,
    rate: u32,
) -> Result<(), EngineError> {
    let enabled: Vec<_> = convs.iter().filter(|c| c.enabled).collect();
    if enabled.is_empty() {
        return Ok(());
    }
    let mut lengths = Vec::with_capacity(enabled.len());
    for conv in enabled {
        let ir = ir_cache.load_for_rate(&conv.ir_path, rate)?;
        if ir.len() != 1 && ir.len() != DEFAULT_CHANNELS {
            return Err(EngineError::new(
                RpcError::INVALID_PARAMS,
                format!(
                    "IR '{}' has {} channels; expected 1 (mono) or {} (stereo)",
                    conv.ir_path.display(),
                    ir.len(),
                    DEFAULT_CHANNELS
                ),
            ));
        }
        lengths.push(ir.iter().map(Vec::len).max().unwrap_or(0));
    }
    audivane_dsp_core::conv::combined_ir_length(&lengths)
        .map_err(|e| EngineError::new(RpcError::INVALID_PARAMS, e.to_string()))?;
    Ok(())
}

fn load_and_combine_irs(
    convs: &[audivane_protocol::ConvolutionConfig],
    ir_cache: &mut IrCache,
    rate: u32,
) -> Result<Option<Vec<Vec<f64>>>, EngineError> {
    let enabled: Vec<_> = convs.iter().filter(|c| c.enabled).collect();
    if enabled.is_empty() {
        return Ok(None);
    }
    let mut loaded = Vec::with_capacity(enabled.len());
    for conv in enabled {
        let ir = ir_cache.load_for_rate(&conv.ir_path, rate)?;
        loaded.push((ir, conv.gain_db));
    }
    let pairs: Vec<(&[Vec<f64>], f64)> = loaded
        .iter()
        .map(|(ir, gain)| (ir.as_slice(), *gain))
        .collect();
    let combined = audivane_dsp_core::conv::combine_irs(&pairs, DEFAULT_CHANNELS)
        .map_err(|e| EngineError::new(RpcError::INVALID_PARAMS, e.to_string()))?;
    Ok(Some(combined))
}

fn validate_engine_config(
    cfg: &EngineConfig,
    ir_cache: &mut IrCache,
    rate: u32,
) -> Result<(), EngineError> {
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
        validate_chain_irs(&ab.chain_b.convolution, ir_cache, rate)?;
    }

    validate_chain_irs(&cfg.chain.convolution, ir_cache, rate)?;

    Ok(())
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
        current: side,
        offset_db: gains.offset_db,
        gain_a_db: gains.gain_a_db,
        gain_b_db: gains.gain_b_db,
        headroom_db: gains.headroom_db,
        overs: Default::default(),
        // The control plane fills in the level and the measurement.
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

    if let Some(ab) = &cfg.ab {
        let b_cfg = with_actual_ratio(ab.chain_b.clone(), graph_rate, actual_ratio);
        let irs_b = load_and_combine_irs(&b_cfg.convolution, ir_cache, graph_rate)?;
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
    use audivane_protocol::{
        Band, ConvolutionConfig, EqProfile, FilterType, OutputConfig, UpsamplerConfig,
    };

    fn test_cfg() -> EngineConfig {
        EngineConfig {
            device: "hw:Loopback,0,0".into(),
            chain: ChainConfig::neutral(),
            ..Default::default()
        }
    }

    /// The sinks' 100 % follows the ceiling; the level stays in dB.
    #[test]
    fn sink_view_follows_the_ceiling_and_keeps_the_level() {
        use audivane_protocol::ipc::VolumeConfig;
        let mut v = VolumeConfig {
            volume_db: -6.0,
            max_volume_db: Some(-3.0),
            ..VolumeConfig::default()
        };
        let view_of = |v: &VolumeConfig| sink_view(v, volume::target_db(v));
        let view = view_of(&v);
        assert_eq!((view.volume_db, view.ceiling_db), (-6.0, -3.0));
        // Lowering the ceiling below the level lowers the level: 100 %.
        v.max_volume_db = Some(-10.0);
        crate::control::config::normalize_volume(&mut v);
        let view = view_of(&v);
        assert_eq!((view.volume_db, view.ceiling_db), (-10.0, -10.0));
        assert_eq!(
            audivane_io_pw::sink_volume::channel_volumes_for(&view, 2),
            vec![1.0, 1.0]
        );
        // Raising it again never gets louder: the slider moves down instead.
        v.max_volume_db = Some(-3.0);
        crate::control::config::normalize_volume(&mut v);
        let view = view_of(&v);
        assert_eq!((view.volume_db, view.ceiling_db), (-10.0, -3.0));
        let ch = audivane_io_pw::sink_volume::channel_volumes_for(&view, 2);
        assert!((f64::from(ch[0]) - 10f64.powf(-7.0 / 20.0)).abs() < 1e-6);
        // No ceiling: 100 % is 0 dB.
        v.max_volume_db = None;
        assert_eq!(view_of(&v).ceiling_db, 0.0);
    }

    #[test]
    fn fallback_does_not_repeat_a_failing_automatic_design() {
        let mut cfg = test_cfg();
        cfg.chain.eq = Some(eq_profile(-3.0, 0.0));
        cfg.chain.upsampler = audivane_dsp_core::resample::ManualConfig {
            target: RateTarget::Ratio(32),
            auto_taps: Some(audivane_protocol::FilterErrorBudget::Db(98.0)),
            band: audivane_protocol::BandSpec {
                passband_end: 0.999,
                stopband_start: 1.0,
                ..Default::default()
            },
            ..Default::default()
        }
        .into_upsampler_config()
        .unwrap();
        let mut cache = IrCache::new();
        let started = std::time::Instant::now();
        let err =
            match build_fallback_chain(&cfg, &mut cache, 44_100, RateRatio::integer(32), AbSide::A)
            {
                Err(e) => e.to_string(),
                Ok(_) => panic!("an infeasible design must not build"),
            };
        assert!(err.contains("upsampler design failed"), "{err}");
        assert!(err.contains("automatic limit"), "{err}");
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn tilt_validation_runs_the_design_at_the_stream_rate() {
        use audivane_protocol::{TiltConfig, TiltPivot};
        let chain = |tilt: TiltConfig| ChainConfig {
            eq: Some(EqProfile {
                tilt: Some(tilt),
                ..EqProfile::default()
            }),
            ..ChainConfig::neutral()
        };
        let ok = TiltConfig {
            enabled: true,
            slope_db_per_oct: Some(-0.7),
            pivot: TiltPivot::Hz(1000.0),
            ..TiltConfig::default()
        };
        assert!(validate_tilt_at_rate(&chain(ok), 44_100).is_ok());
        // The band alone is fine; only the design rejects this.
        let too_steep = TiltConfig {
            slope_db_per_oct: None,
            total_db: Some(30.0),
            ..ok
        };
        assert!(audivane_dsp_core::tilt::effective_band(&too_steep, 48_000.0).is_ok());
        let err = validate_tilt_at_rate(&chain(too_steep), 48_000).unwrap_err();
        assert!(err.message.contains("within +/-24 dB"), "{}", err.message);
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
    fn chain_params_accept_automatic_and_band_designs() {
        let mut chain = ChainConfig {
            upsampler: audivane_dsp_core::resample::ManualConfig {
                target: RateTarget::Ratio(2),
                auto_taps: Some(audivane_protocol::FilterErrorBudget::Db(98.0)),
                ..Default::default()
            }
            .into_upsampler_config()
            .unwrap(),
            ..ChainConfig::neutral()
        };
        assert!(validate_chain_params(&chain).is_ok());
        chain.upsampler = audivane_dsp_core::resample::ManualConfig {
            target: RateTarget::Ratio(2),
            cutoff_mode: audivane_protocol::CutoffMode::Band,
            ..Default::default()
        }
        .into_upsampler_config()
        .unwrap();
        assert!(validate_chain_params(&chain).is_ok());
    }

    #[test]
    fn a_software_mute_is_not_bit_perfect() {
        let mut cfg = test_cfg();
        cfg.volume.volume_db = 0.0;
        cfg.volume.muted = true;
        let mut h = BuilderProcHarness::new(cfg);
        h.negotiate(48_000, RateRatio::ONE, SinkChoice::Dsp, SampleFormat::S32);
        assert!(!h.mirror.volume_status().hardware);
        assert!(h.mirror.digital_gain.load(Ordering::Acquire));
    }

    #[test]
    fn diff_reopen_triggers_only_when_alsa_params_change() {
        let base = test_cfg();

        // EQ change does NOT reopen ALSA.
        let mut eq_change = base.clone();
        eq_change.chain.eq = Some(EqProfile {
            name: "eq".into(),
            preamp_db: -2.0,
            bands: vec![Band {
                kind: FilterType::Peaking,
                freq_hz: 1000.0,
                gain_db: 2.0,
                q: 1.0,
                enabled: true,
            }],
            ..Default::default()
        });
        assert!(!diff_requires_reopen(
            &base,
            &eq_change,
            48_000,
            None,
            RateRatio::ONE,
            None
        ));

        // Adding A/B with same ratio does NOT reopen ALSA.
        let mut ab_change = base.clone();
        ab_change.ab = Some(AbConfig {
            level: LevelMatch::default(),
            headroom: Default::default(),
            ..AbConfig::chain(eq_change.chain.clone())
        });
        assert!(!diff_requires_reopen(
            &base,
            &ab_change,
            48_000,
            None,
            RateRatio::ONE,
            None
        ));

        // Enabling 4x upsampling DOES reopen ALSA.
        let mut up_change = base.clone();
        up_change.chain.upsampler = UpsamplerConfig {
            target: RateTarget::Ratio(4),
            ..Default::default()
        };
        assert!(diff_requires_reopen(
            &base,
            &up_change,
            48_000,
            None,
            RateRatio::ONE,
            None
        ));

        // Toggling dsp_enabled off when upsampling was active (the device
        // at x4) DOES reopen ALSA back to 1x.
        let mut dsp_off = up_change.clone();
        dsp_off.dsp_enabled = false;
        assert!(diff_requires_reopen(
            &up_change,
            &dsp_off,
            48_000,
            None,
            RateRatio::integer(4),
            None
        ));

        // Changing quantum or device DOES reopen ALSA.
        let mut q_change = base.clone();
        q_change.quantum = 512;
        assert!(diff_requires_reopen(
            &base,
            &q_change,
            48_000,
            None,
            RateRatio::ONE,
            None
        ));

        let mut dev_change = base.clone();
        dev_change.device = "hw:CARD=DAC,DEV=0".into();
        assert!(diff_requires_reopen(
            &base,
            &dev_change,
            48_000,
            None,
            RateRatio::ONE,
            None
        ));
    }

    #[test]
    fn validates_ab_output_and_ratio_parity() {
        let mut cache = IrCache::new();
        let mut bad_out = test_cfg();
        bad_out.ab = Some(AbConfig {
            level: LevelMatch::Off,
            headroom: Default::default(),
            ..AbConfig::chain(ChainConfig {
                output: OutputConfig {
                    bits: 16,
                    ..Default::default()
                },
                ..ChainConfig::neutral()
            })
        });
        assert!(validate_engine_config(&bad_out, &mut cache, 48_000).is_err());

        let mut bad_ratio = test_cfg();
        bad_ratio.ab = Some(AbConfig {
            level: LevelMatch::Off,
            headroom: Default::default(),
            ..AbConfig::chain(ChainConfig {
                upsampler: UpsamplerConfig {
                    target: RateTarget::Ratio(4),
                    ..Default::default()
                },
                ..ChainConfig::neutral()
            })
        });
        assert!(validate_engine_config(&bad_ratio, &mut cache, 48_000).is_err());
    }

    #[test]
    fn ab_status_waits_for_a_built_pair_and_attenuation_is_an_effect() {
        let mut cfg = test_cfg();
        cfg.ab = Some(AbConfig {
            level: LevelMatch::Off,
            headroom: Default::default(),
            ..AbConfig::chain(ChainConfig::neutral())
        });
        // Before a pair is published the mirror has no gains, so status
        // falls back to the config estimate instead of reporting 0 dB.
        let mirror = SharedMirror::new(&cfg);
        assert_eq!(mirror.ab_status(), None);
        let gains = AbGains::new(-3.0, Default::default(), 0.0, 0.0);
        mirror.set_ab(true, AbSide::A, Some(gains));
        assert_eq!(mirror.ab_status().unwrap().gain_b_db, -3.0);

        // Two passthrough sides: only a manual offset or a fixed headroom
        // make the pair attenuate, and then the DSP is not bit-perfect.
        cfg.chain = ChainConfig::neutral();
        assert!(cfg.chain.is_passthrough());
        assert!(!chain_has_effect(&cfg));
        cfg.ab.as_mut().unwrap().level = LevelMatch::Manual { offset_db: -3.0 };
        assert!(chain_has_effect(&cfg));
        cfg.ab.as_mut().unwrap().level = LevelMatch::default();
        assert!(!chain_has_effect(&cfg));
        cfg.ab.as_mut().unwrap().headroom = audivane_protocol::ipc::AbHeadroom::Fixed { db: 2.0 };
        assert!(chain_has_effect(&cfg));
        // An output compare quantizes B its own way.
        cfg.ab.as_mut().unwrap().headroom = Default::default();
        assert!(!chain_has_effect(&cfg));
        cfg.ab.as_mut().unwrap().chain_b.output.bits = 16;
        assert!(chain_has_effect(&cfg));
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
    fn a_filter_patch_builds_and_the_pair_latency_is_the_max() {
        use audivane_dsp_core::resample::ManualConfig;
        let design = |taps| {
            ManualConfig {
                target: RateTarget::Ratio(2),
                taps,
                ..Default::default()
            }
            .into_upsampler_config()
            .unwrap()
        };
        let mut cache = IrCache::new();
        let mut cfg = test_cfg();
        cfg.chain.upsampler = design(512);
        let b_up = design(4096);
        cfg.ab = Some(AbConfig::new(audivane_protocol::ipc::AbB::Patch(
            serde_json::json!({"upsampler": {"preset": null, "design": b_up.design}}),
        )));
        assert!(crate::control::config::normalize_config(&mut cfg).is_none());
        crate::control::config::validate(&cfg).unwrap();
        let chain_b = cfg.ab.as_ref().unwrap().chain_b.clone();
        assert_eq!(chain_b.upsampler, b_up);

        let latency = |cfg: &EngineConfig| {
            build_active_chain(
                cfg,
                &mut IrCache::new(),
                48_000,
                RateRatio::integer(2),
                AbSide::A,
            )
            .unwrap()
            .0
            .latency_out_frames()
        };
        let mut only_a = cfg.clone();
        only_a.ab = None;
        let mut only_b = only_a.clone();
        only_b.chain = chain_b;
        let (a, b) = (latency(&only_a), latency(&only_b));
        assert!(b > a, "{a} vs {b}");
        let (pair, _, _) =
            build_active_chain(&cfg, &mut cache, 48_000, RateRatio::integer(2), AbSide::A).unwrap();
        assert!(matches!(pair, ActiveChain::Ab(_)));
        assert_eq!(pair.latency_out_frames(), a.max(b));
    }

    /// Step 7: a dither patch builds a pair with B's own quantizer; the
    /// same output on both sides keeps one.
    #[test]
    fn a_dither_patch_builds_a_pair_with_two_quantizers() {
        let mut cfg = test_cfg();
        cfg.chain.eq = Some(EqProfile {
            name: "flat".into(),
            preamp_db: -1.0,
            ..Default::default()
        });
        let pair = |patch| {
            let mut cfg = cfg.clone();
            cfg.ab = Some(AbConfig::new(audivane_protocol::ipc::AbB::Patch(patch)));
            assert!(crate::control::config::normalize_config(&mut cfg).is_none());
            crate::control::config::validate(&cfg).unwrap();
            let capped = cap_dither_bits(&cfg, Some(SampleFormat::S32));
            match build_active_chain(
                &capped,
                &mut IrCache::new(),
                48_000,
                RateRatio::integer(1),
                AbSide::A,
            )
            .unwrap()
            .0
            {
                ActiveChain::Ab(ab) => ab.compares_output(),
                _ => panic!("expected ActiveChain::Ab"),
            }
        };
        assert!(pair(
            serde_json::json!({"output": {"dither": "shaped", "bits": 16}})
        ));
        assert!(pair(serde_json::json!({"output": {"bits": 20}})));
        assert!(!pair(serde_json::json!({"eq": null})));
    }

    #[test]
    fn max_rate_target_resolves_against_actual_source_rates() {
        let mut cfg = test_cfg();
        cfg.chain.upsampler = UpsamplerConfig {
            target: RateTarget::MaxRate(192_000),
            ..Default::default()
        };
        assert_eq!(desired_ratio_for(&cfg, 44_100), RateRatio::integer(4)); // 176.4 kHz
        assert_eq!(desired_ratio_for(&cfg, 48_000), RateRatio::integer(4)); // 192 kHz
        assert_eq!(desired_ratio_for(&cfg, 96_000), RateRatio::integer(2)); // 192 kHz (NOT 4x = 384 kHz)
        assert_eq!(desired_ratio_for(&cfg, 192_000), RateRatio::integer(1)); // 192 kHz
    }

    struct BuilderProcHarness {
        state: BuilderState,
        mirror: SharedMirror,
        ab_atomic: Arc<AtomicU8>,
        proc: DaemonProcessor,
        recycle_rx: rtrb::Consumer<ActiveChain>,
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
                followers: follower_host(),
                align: follower::AlignControl::default(),
                align_t0: Instant::now(),
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

        fn active_gen(&self) -> u64 {
            self.gen_handle.load(Ordering::Acquire)
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
    fn the_master_taking_a_follower_s_device_stops_that_follower_first() {
        let cfg = test_cfg();
        let mut h = BuilderProcHarness::new(cfg.clone());
        let dev = "hw:CARD=USB,DEV=0";
        let st = &mut h.state;
        let master = st.cfg.device.clone();
        st.followers
            .add(
                &mut st.tee,
                dev,
                &master,
                &cfg,
                follower::FollowerOptions::default(),
            )
            .unwrap();
        assert!(st.followers.contains(dev) && st.tee.slots().get(dev).is_some());
        let mut moved = cfg.clone();
        moved.device = dev.into();
        // Refused: the follower keeps playing.
        let mut bad = moved.clone();
        bad.chain.limiter.lookahead_ms = 1_000.0;
        assert!(
            h.state
                .apply_config(&bad, &h.mirror, EngineState::Idle, 48_000, None, None)
                .is_err()
        );
        assert!(h.state.followers.contains(dev), "a refused edit stopped it");
        h.state
            .apply_config(&moved, &h.mirror, EngineState::Idle, 48_000, None, None)
            .unwrap();
        assert!(!h.state.followers.contains(dev));
        assert!(h.state.tee.slots().get(dev).is_none());
        assert!(h.state.followers.is_empty());
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
        // A later trim is taken as it is.
        let mut later = edited.clone();
        later.outputs.followers[1].trim_ms = 1.0;
        apply(&mut h, &later);
        let c = h.state.followers.controls(b).unwrap();
        assert_eq!((c.trim_ms(), c.lag_ms()), (1.0, 7.5));
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
    fn a_same_rate_edit_is_held_to_varispeed_s_budget() {
        // 768 → 705.6 kHz, varispeed on with a 16-tap design: an edit to 64
        // taps hot-swaps at the same rate, and must not run past the static
        // budget just because no negotiation follows.
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
        let mut h = BuilderProcHarness::new(cfg.clone());
        h.negotiate(768_000, r, SinkChoice::Dsp, SampleFormat::S32);
        h.state
            .varispeed
            .set_enabled(true, 768_000, Ok(()))
            .unwrap();
        let mut edited = cfg.clone();
        edited.chain.upsampler = audivane_dsp_core::resample::ManualConfig {
            target: RateTarget::Rate(705_600),
            taps: 64,
            taps_unit: audivane_protocol::TapsUnit::Input,
            cutoff: 0.95,
            ..Default::default()
        }
        .into_upsampler_config()
        .unwrap();
        let apply = |h: &mut BuilderProcHarness, c: &EngineConfig| {
            h.state.apply_config(
                c,
                &h.mirror,
                EngineState::Playing,
                768_000,
                Some(SinkChoice::Dsp),
                None,
            )
        };
        let e = apply(&mut h, &edited).unwrap_err();
        assert!(
            e.message.contains("varispeed at 768000 Hz"),
            "{}",
            e.message
        );
        assert_eq!(
            h.state.cfg.chain.upsampler, cfg.chain.upsampler,
            "nothing committed"
        );
        // Without varispeed the same edit is fine.
        h.state
            .varispeed
            .set_enabled(false, 768_000, Ok(()))
            .unwrap();
        apply(&mut h, &edited).unwrap();
    }

    #[test]
    fn a_stage_the_budget_kept_off_comes_back_after_an_edit_that_fits() {
        // 768 → 705.6 kHz with a 64-tap design: the negotiation keeps the
        // stage off (over the static budget). An edit to 16 taps at the same
        // rate fits again: the stage is installed again, with no
        // negotiation.
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
        h.state
            .varispeed
            .set_enabled(true, 768_000, Ok(()))
            .unwrap();
        h.negotiate(768_000, r, SinkChoice::Dsp, SampleFormat::S32);
        assert_eq!(h.state.varispeed.built_rate(), None, "over the budget");
        assert!(h.state.varispeed.requested());
        // A stage that is off does not refuse the edit.
        let mut edited = cfg.clone();
        edited.chain.upsampler = audivane_dsp_core::resample::ManualConfig {
            target: RateTarget::Rate(705_600),
            taps: 16,
            taps_unit: audivane_protocol::TapsUnit::Input,
            cutoff: 0.95,
            ..Default::default()
        }
        .into_upsampler_config()
        .unwrap();
        h.state
            .apply_config(
                &edited,
                &h.mirror,
                EngineState::Playing,
                768_000,
                Some(SinkChoice::Dsp),
                None,
            )
            .unwrap();
        assert_eq!(h.state.varispeed.built_rate(), Some(768_000));
    }

    #[test]
    fn an_off_hold_is_acknowledged_once_the_stage_is_gone() {
        use crate::api::VarispeedHold;
        // The audio thread read the hold (sequence 3), the stage still fades
        // out: not acknowledged yet.
        assert_eq!(varispeed_hold_ack(VarispeedHold::Off, 3, 3, false), 2);
        assert_eq!(varispeed_hold_ack(VarispeedHold::Off, 3, 3, true), 3);
        // Not read yet: what the audio thread applies.
        assert_eq!(varispeed_hold_ack(VarispeedHold::Off, 3, 2, true), 2);
        // The milder holds count when the audio thread reads them.
        assert_eq!(varispeed_hold_ack(VarispeedHold::Unity, 2, 2, false), 2);
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
    fn build_varispeed_status_maps_applied_and_driver_fields() {
        use audivane_io_pw::varispeed_rt::{VarispeedDriverSnapshot, VarispeedHold};
        use audivane_io_pw::varispeed_sched::{CapReason, HeldReason};
        use audivane_protocol::ipc::{
            VarispeedCapReason, VarispeedHeldReason, VarispeedOverloadHold,
        };

        // Inactive default snapshots never produce zero speed or zero cap,
        // and preserve the accepted host request separately from RT speed/target.
        let idle = build_varispeed_status(
            varispeed::VarispeedAppliedSnapshot::default(),
            VarispeedDriverSnapshot::default(),
            false,
            1.0,
            None,
        );
        assert!(!idle.enabled);
        assert!(!idle.active);
        assert_eq!(idle.speed, 1.0);
        assert_eq!(idle.target, 1.0);
        assert_eq!(idle.requested, 1.0);
        assert_eq!(idle.speed_limit, 2.0);
        assert_eq!(idle.latency_frames, 0.0);
        assert!(idle.latency_exact);

        // Active state preserves applied speed/target, host request, and driver telemetry.
        let applied = varispeed::VarispeedAppliedSnapshot {
            enabled: true,
            active: true,
            speed: 1.12,
            target: 1.0,
            requested: 1.0, // RT saw 1.0 under hold; host request must still win
            speed_limit: 1.25,
            ramping: true,
            fading: true,
            latency_frames: 480.5,
            latency_exact: false,
            gap_fades: 2,
            ..Default::default()
        };
        let driver = VarispeedDriverSnapshot {
            active: true,
            cap: 1.25,
            cap_reason: Some(CapReason::PeriodQuantum),
            held: Some(HeldReason::Underfeed),
            overload_hold: VarispeedHold::Unity,
            underfeed_events: 4,
            underfeed_frames: 320,
            short_cycles: 3,
            ..Default::default()
        };
        let st = build_varispeed_status(applied, driver, true, 1.5, Some("over budget".into()));
        assert!(st.enabled && st.active);
        assert_eq!(st.speed, 1.12);
        assert_eq!(st.target, 1.0);
        assert_eq!(st.requested, 1.5);
        assert_eq!(st.speed_limit, 1.25);
        assert_eq!(st.cap_reason, Some(VarispeedCapReason::PeriodQuantum));
        assert!(st.ramping && st.fading);
        assert_eq!(st.latency_frames, 480.5);
        assert!(!st.latency_exact);
        assert_eq!(st.held, Some(VarispeedHeldReason::Underfeed));
        assert_eq!(st.overload_hold, VarispeedOverloadHold::Unity);
        assert_eq!(st.underfeed_events, 4);
        assert_eq!(st.underfeed_frames, 320);
        assert_eq!(st.short_cycles, 3);
        assert_eq!(st.gap_fades, 2);
        assert_eq!(st.error.as_deref(), Some("over budget"));
    }

    #[test]
    fn full_config_can_disable_varispeed_and_install_heavier_filter_together() {
        use audivane_io_pw::EngineAudioProcessor;
        let r = RateRatio::new(147, 160);
        let mut cfg = test_cfg();
        cfg.varispeed.enabled = true;
        cfg.varispeed.speed = 1.05;
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
        h.state
            .set_varispeed(true, 1.05, 200.0, &h.mirror, None)
            .unwrap();
        assert_eq!(h.state.varispeed.built_rate(), Some(768_000));
        // Poll the RT processor so the stage is installed and active.
        h.proc.poll_updates(SinkChoice::Dsp, 768_000, r, false);
        assert!(h.varispeed_applied.snapshot().enabled);

        // One edit disables varispeed and upgrades to 64 taps at 768 → 705.6 kHz:
        // must succeed without charging the incoming chain for varispeed and must
        // retire the stage before/with the swap.
        let mut edited = cfg.clone();
        edited.varispeed.enabled = false;
        edited.chain.upsampler = audivane_dsp_core::resample::ManualConfig {
            target: RateTarget::Rate(705_600),
            taps: 64,
            taps_unit: audivane_protocol::TapsUnit::Input,
            cutoff: 0.95,
            ..Default::default()
        }
        .into_upsampler_config()
        .unwrap();
        h.state
            .apply_config(
                &edited,
                &h.mirror,
                EngineState::Playing,
                768_000,
                Some(SinkChoice::Dsp),
                None,
            )
            .unwrap();
        assert!(!h.state.cfg.varispeed.enabled);
        assert_eq!(h.state.cfg.chain.upsampler, edited.chain.upsampler);
        assert_eq!(h.state.varispeed.built_rate(), None);
        // The over-budget chain stays held until RT retires the stage.
        assert!(h.state.pending_publication.is_some());
        assert!(!h.state.flush_pending_swap(&h.mirror, None));
        // Idle poll retires the stage at once, then the reaper flushes the swap.
        h.proc.poll_updates(SinkChoice::Dsp, 768_000, r, false);
        assert!(!h.varispeed_applied.snapshot().enabled);
        assert!(h.state.flush_pending_swap(&h.mirror, None));
        h.proc.poll_updates(SinkChoice::Dsp, 768_000, r, false);
        assert_eq!(h.active_gen(), 1);
    }

    #[test]
    fn disabling_varispeed_while_streaming_waits_for_stage_retirement_before_over_budget_swap() {
        use audivane_io_pw::EngineAudioProcessor;
        use audivane_io_pw::varispeed_rt::{
            VarispeedCommandSlot, VarispeedDriver, VarispeedDriverStatus,
        };
        use audivane_io_pw::varispeed_sched::{PullKind, Timing};

        let r = RateRatio::new(147, 160);
        let mut cfg = test_cfg();
        cfg.varispeed.enabled = true;
        cfg.varispeed.speed = 1.05;
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
        h.state
            .set_varispeed(true, 1.05, 0.0, &h.mirror, None)
            .unwrap();

        let slot = VarispeedCommandSlot::default();
        slot.send(1.05, 0.0);
        let drv_status = VarispeedDriverStatus::default();
        let mut drv = VarispeedDriver::new();

        h.proc.poll_updates(SinkChoice::Dsp, 768_000, r, false);
        let timing = Timing::new(768_000, 1024, 1024);
        {
            let port = h.proc.varispeed(SinkChoice::Dsp, r, 768_000, 2).unwrap();
            drv.start_priming(timing, port, &slot);
            drv.finish_priming();
            drv.publish(&drv_status);
        }

        let mut out = vec![0i32; 2048 * 2];
        let mut step_stream = |h: &mut BuilderProcHarness, drv: &mut VarispeedDriver, step: u64| {
            h.proc.poll_updates(SinkChoice::Dsp, 768_000, r, true);
            let now_ns = step * 1_333_333;
            let (take, pad) = if let Some(port) = h.proc.varispeed(SinkChoice::Dsp, r, 768_000, 2) {
                let p = drv.pull(port, now_ns, 1024, 8192, PullKind::Play);
                drv.after_pull(port, &slot);
                drv.publish(&drv_status);
                (p.take, p.pad)
            } else {
                (1024, 0)
            };
            let inp = vec![0.0f32; (take + pad) * 2];
            let out_frames = (1024usize * r.up as usize).div_ceil(r.down as usize);
            h.proc.process_block(
                SinkChoice::Dsp,
                &inp,
                &[],
                &mut out[..out_frames * 2],
                2,
                r,
                768_000,
                step * 1024,
            );
            while h.recycle_rx.pop().is_ok() {}
            if let Some(rx) = h.vs_recycle_rx.as_mut() {
                while rx.pop().is_ok() {}
            }
        };

        // Run a streaming block so the varispeed stage is actively running (`ran = true`).
        step_stream(&mut h, &mut drv, 1);
        assert!(h.varispeed_applied.snapshot().active);
        assert_eq!(h.varispeed_applied.snapshot().stage_rate, 768_000);

        // One edit disables varispeed and installs a 512-tap chain (well over
        // 800 M MAC/s if overlapped with varispeed at 768 → 705.6 kHz).
        let mut heavy = cfg;
        heavy.varispeed.enabled = false;
        heavy.chain.upsampler = audivane_dsp_core::resample::ManualConfig {
            target: RateTarget::Rate(705_600),
            taps: 512,
            taps_unit: audivane_protocol::TapsUnit::Input,
            cutoff: 0.95,
            ..Default::default()
        }
        .into_upsampler_config()
        .unwrap();
        h.state
            .apply_config(
                &heavy,
                &h.mirror,
                EngineState::Playing,
                768_000,
                Some(SinkChoice::Dsp),
                None,
            )
            .unwrap();
        assert!(
            h.state.pending_publication.is_some(),
            "512-tap chain must be held while streaming varispeed fades out"
        );

        // Step streaming blocks until the ~5 ms retirement fade completes. At
        // every step while varispeed is still alive on RT, the 512-tap swap stays
        // held, never warming or crossfading.
        let mut saw_fading_hold = false;
        let mut published = false;
        for step in 2..=32u64 {
            step_stream(&mut h, &mut drv, step);
            let vs = h.varispeed_applied.snapshot();
            let flushed = h.state.flush_pending_swap(&h.mirror, None);
            let word = h.state.swap_state.load(Ordering::Acquire);
            let rt_in_flight = (word & 1) != 0;
            if vs.phase != 0 || vs.stage_rate != 0 {
                saw_fading_hold = true;
                assert!(
                    !flushed,
                    "over-budget 512-tap chain published while varispeed was still live (step {step}, phase={}, rate={})",
                    vs.phase, vs.stage_rate
                );
                assert!(
                    !rt_in_flight && h.active_gen() == 0,
                    "512-tap chain started warming/running alongside varispeed (step {step})"
                );
            } else if flushed {
                published = true;
                break;
            }
        }
        assert!(
            saw_fading_hold,
            "test must observe at least one streaming block during the varispeed fade-out"
        );
        assert!(
            published,
            "512-tap chain must publish once varispeed has retired on RT"
        );
        // After publication, the 512-tap chain starts warming and runs with
        // no varispeed stage present (never overlapping the retired stage).
        step_stream(&mut h, &mut drv, 33);
        assert!(h.proc.varispeed(SinkChoice::Dsp, r, 768_000, 2).is_none());
        assert!(!h.varispeed_applied.snapshot().active);
        h.proc.poll_updates(SinkChoice::Dsp, 768_000, r, false);
        assert_eq!(h.active_gen(), 1);
    }

    #[test]
    fn keep_pitch_from_the_call_and_the_config() {
        use audivane_io_pw::EngineAudioProcessor;
        use audivane_protocol::ipc::VarispeedConfig;
        let one = RateRatio::ONE;
        let vs = |enabled, keep_pitch| VarispeedConfig {
            enabled,
            speed: 1.1,
            ramp_ms: 200.0,
            keep_pitch,
        };
        let mut h = BuilderProcHarness::with_varispeed(test_cfg(), 8);
        h.negotiate(48_000, one, SinkChoice::Dsp, SampleFormat::S32);

        // On with Keep pitch: the stretch is built, status says so.
        h.state
            .set_varispeed_cfg(vs(true, true), &h.mirror, None)
            .unwrap();
        h.proc.poll_updates(SinkChoice::Dsp, 48_000, one, false);
        assert!(h.state.varispeed.keep_pitch());
        assert_eq!(h.state.varispeed.built_rate(), Some(48_000));
        assert!(h.state.cfg.varispeed.keep_pitch);
        assert!(h.mirror.varispeed_keep_pitch.load(Ordering::Acquire));
        // The speed-only call keeps it.
        h.state
            .set_varispeed(true, 1.2, 200.0, &h.mirror, None)
            .unwrap();
        assert!(h.state.cfg.varispeed.keep_pitch && h.state.varispeed.keep_pitch());
        // Off and back to varispeed in one call: nothing is built.
        h.state
            .set_varispeed_cfg(vs(false, false), &h.mirror, None)
            .unwrap();
        assert!(!h.state.varispeed.keep_pitch());
        assert_eq!(h.state.varispeed.built_rate(), None);
        assert!(!h.mirror.varispeed_keep_pitch.load(Ordering::Acquire));

        // Through a whole config, too.
        let mut cfg = test_cfg();
        cfg.varispeed = vs(true, true);
        h.state
            .apply_config(
                &cfg,
                &h.mirror,
                EngineState::Playing,
                48_000,
                Some(SinkChoice::Dsp),
                None,
            )
            .unwrap();
        assert!(h.state.varispeed.keep_pitch());
        assert_eq!(h.state.varispeed.built_rate(), Some(48_000));
        assert!(h.mirror.varispeed_keep_pitch.load(Ordering::Acquire));

        // At 192 kHz the stretch does not fit: refused, nothing changes.
        let mut h = BuilderProcHarness::with_varispeed(test_cfg(), 8);
        h.negotiate(192_000, one, SinkChoice::Dsp, SampleFormat::S32);
        h.state
            .set_varispeed_cfg(vs(true, false), &h.mirror, None)
            .unwrap();
        let e = h
            .state
            .set_varispeed_cfg(vs(true, true), &h.mirror, None)
            .unwrap_err();
        assert_eq!(e.code, RpcError::INVALID_PARAMS);
        assert!(
            e.message
                .contains("Keep pitch is not available at 192000 Hz"),
            "{e}"
        );
        assert!(!h.state.varispeed.keep_pitch() && !h.state.cfg.varispeed.keep_pitch);
        assert_eq!(h.state.varispeed.built_rate(), Some(192_000));
        // Off, Keep pitch may be set (it is checked when varispeed starts).
        h.state
            .set_varispeed_cfg(vs(false, true), &h.mirror, None)
            .unwrap();
        assert!(h.state.varispeed.keep_pitch());
        assert!(
            h.state
                .set_varispeed_cfg(vs(true, true), &h.mirror, None)
                .is_err()
        );
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
    fn swap_tracking_keeps_heavy_middle_chain_between_two_light_edits() {
        use audivane_io_pw::EngineAudioProcessor;
        let r = RateRatio::new(147, 160);
        let make = |taps: usize| {
            let mut c = test_cfg();
            c.chain.upsampler = audivane_dsp_core::resample::ManualConfig {
                target: RateTarget::Rate(705_600),
                taps,
                taps_unit: audivane_protocol::TapsUnit::Input,
                cutoff: 0.95,
                ..Default::default()
            }
            .into_upsampler_config()
            .unwrap();
            c
        };
        let light1 = make(16);
        let heavy = make(80);
        let mut light2 = make(16);
        light2.chain.eq = Some(eq_profile(-1.0, 0.0));

        let mut h = BuilderProcHarness::with_varispeed(light1, 8);
        h.negotiate(768_000, r, SinkChoice::Dsp, SampleFormat::S32);
        h.proc.poll_updates(SinkChoice::Dsp, 768_000, r, false);

        // Gen 1: swap from light (16) to heavy (80) and start warming/crossfading on RT.
        h.state
            .apply_config(
                &heavy,
                &h.mirror,
                EngineState::Playing,
                768_000,
                Some(SinkChoice::Dsp),
                None,
            )
            .unwrap();
        h.proc.poll_updates(SinkChoice::Dsp, 768_000, r, true);
        let word = h.state.swap_state.load(Ordering::Acquire);
        assert_eq!(word & 1, 1, "heavy swap must be in flight");

        // Gen 2: while heavy (80) is still warming/crossfading, edit back to light (16).
        h.state
            .apply_config(
                &light2,
                &h.mirror,
                EngineState::Playing,
                768_000,
                Some(SinkChoice::Dsp),
                None,
            )
            .unwrap();
        // A live varispeed enable still prices the heavy middle chain, though both
        // Gen 0 and Gen 2 are 16 taps.
        let err = h
            .state
            .set_varispeed(true, 1.05, 200.0, &h.mirror, None)
            .unwrap_err();
        assert!(err.message.contains("varispeed at 768000 Hz"), "{err}");

        // Once Gen 2 settles on RT (idle poll clears in-flight), the heavy
        // middle chain is pruned and live varispeed enable succeeds.
        h.proc.poll_updates(SinkChoice::Dsp, 768_000, r, false);
        while h.recycle_rx.pop().is_ok() {}
        let word = h.state.swap_state.load(Ordering::Acquire);
        assert_eq!(word >> 1, 2);
        assert_eq!(word & 1, 0);
        h.state
            .set_varispeed(true, 1.05, 200.0, &h.mirror, None)
            .unwrap();
        assert!(h.state.cfg.varispeed.enabled);
    }

    #[test]
    fn superseded_unpublished_swaps_leave_no_tracking_record() {
        // The RT thread takes nothing (no poll): the swap queue (32) fills,
        // and every later edit supersedes a chain that never reached it.
        // Those chains can never run, so the tracking holds one record per
        // published swap, not one per edit.
        let r = RateRatio::new(147, 160);
        let make = |gain: f64| {
            let mut c = test_cfg();
            c.chain.upsampler = audivane_dsp_core::resample::ManualConfig {
                target: RateTarget::Rate(705_600),
                taps: 16,
                taps_unit: audivane_protocol::TapsUnit::Input,
                cutoff: 0.95,
                ..Default::default()
            }
            .into_upsampler_config()
            .unwrap();
            c.chain.eq = Some(eq_profile(gain, 0.0));
            c
        };
        let mut h = BuilderProcHarness::with_varispeed(make(0.0), 8);
        h.negotiate(768_000, r, SinkChoice::Dsp, SampleFormat::S32);
        for i in 1..=48 {
            h.state
                .apply_config(
                    &make(-0.1 * f64::from(i)),
                    &h.mirror,
                    EngineState::Playing,
                    768_000,
                    Some(SinkChoice::Dsp),
                    None,
                )
                .unwrap();
        }
        assert!(h.state.pending_publication.is_some(), "the queue filled");
        assert!(
            h.state.in_flight_cfgs.len() <= 33,
            "{} records",
            h.state.in_flight_cfgs.len()
        );
    }

    #[test]
    fn live_varispeed_enable_prices_outgoing_chain_until_swap_completes() {
        use audivane_io_pw::EngineAudioProcessor;
        let r = RateRatio::new(147, 160);
        // Start with an 80-tap rational chain (fits without varispeed, blows the
        // 800 M budget alongside varispeed even while swapping down to 16 taps).
        let mut heavy = test_cfg();
        heavy.chain.upsampler = audivane_dsp_core::resample::ManualConfig {
            target: RateTarget::Rate(705_600),
            taps: 80,
            taps_unit: audivane_protocol::TapsUnit::Input,
            cutoff: 0.95,
            ..Default::default()
        }
        .into_upsampler_config()
        .unwrap();
        let mut h = BuilderProcHarness::with_varispeed(heavy.clone(), 8);
        h.negotiate(768_000, r, SinkChoice::Dsp, SampleFormat::S32);
        h.proc.poll_updates(SinkChoice::Dsp, 768_000, r, false);
        assert_eq!(h.active_gen(), 0);

        // Swap to 16 taps while streaming: the 80-tap outgoing chain is still
        // playing/warming on the RT thread until the swap completes.
        let mut light = heavy.clone();
        light.chain.upsampler = audivane_dsp_core::resample::ManualConfig {
            target: RateTarget::Rate(705_600),
            taps: 16,
            taps_unit: audivane_protocol::TapsUnit::Input,
            cutoff: 0.95,
            ..Default::default()
        }
        .into_upsampler_config()
        .unwrap();
        h.state
            .apply_config(
                &light,
                &h.mirror,
                EngineState::Playing,
                768_000,
                Some(SinkChoice::Dsp),
                None,
            )
            .unwrap();
        // Before RT finishes the swap, live enable must price the 80-tap outgoing chain.
        let err = h
            .state
            .set_varispeed(true, 1.05, 200.0, &h.mirror, None)
            .unwrap_err();
        assert!(err.message.contains("varispeed at 768000 Hz"), "{err}");
        assert!(!h.state.cfg.varispeed.enabled);

        // Even while RT is actively warming/crossfading (streaming = true),
        // live enable stays refused until the swap finishes.
        h.proc.poll_updates(SinkChoice::Dsp, 768_000, r, true);
        let err = h
            .state
            .set_varispeed(true, 1.05, 200.0, &h.mirror, None)
            .unwrap_err();
        assert!(err.message.contains("varispeed at 768000 Hz"), "{err}");

        // Once the swap settles (idle promotion or finished crossfade), the
        // outgoing chain is retired and live enable succeeds.
        h.proc.poll_updates(SinkChoice::Dsp, 768_000, r, false);
        while h.recycle_rx.pop().is_ok() {}
        assert_eq!(h.active_gen(), 1);
        h.state
            .set_varispeed(true, 1.05, 200.0, &h.mirror, None)
            .unwrap();
        assert!(h.state.cfg.varispeed.enabled);
        assert_eq!(h.state.varispeed.built_rate(), Some(768_000));
    }

    #[test]
    fn real_engine_status_keeps_accepted_request_across_inactive_and_underfeed_hold() {
        use audivane_io_pw::EngineAudioProcessor;
        use audivane_io_pw::varispeed_rt::{
            VarispeedCommandSlot, VarispeedDriver, VarispeedDriverStatus,
        };
        use audivane_io_pw::varispeed_sched::{PullKind, Timing};
        use audivane_protocol::ipc::VarispeedHeldReason;

        let mut cfg = test_cfg();
        cfg.dsp_enabled = true;
        let mut h = BuilderProcHarness::with_varispeed(cfg, 8);
        h.negotiate(48_000, RateRatio::ONE, SinkChoice::Dsp, SampleFormat::S32);

        let slot = VarispeedCommandSlot::default();
        let drv_status = VarispeedDriverStatus::default();
        let mut drv = VarispeedDriver::new();

        let status = |h: &BuilderProcHarness, drv_status: &VarispeedDriverStatus| {
            let (en, req) = h.mirror.varispeed_request();
            build_varispeed_status(
                h.varispeed_applied.snapshot(),
                drv_status.snapshot(),
                en,
                req,
                h.mirror.varispeed_error(),
            )
        };

        // 1. Enable at 1.05 and run one DSP block so RT applies 1.05.
        h.state
            .set_varispeed(true, 1.05, 0.0, &h.mirror, None)
            .unwrap();
        slot.send(1.05, 0.0);
        h.proc
            .poll_updates(SinkChoice::Dsp, 48_000, RateRatio::ONE, false);
        let timing = Timing::new(48_000, 1024, 1024);
        {
            let port = h
                .proc
                .varispeed(SinkChoice::Dsp, RateRatio::ONE, 48_000, 2)
                .unwrap();
            drv.start_priming(timing, port, &slot);
            drv.finish_priming();
            drv.publish(&drv_status);
        }
        let mut out = vec![0i32; 1024 * 2];
        let mut run_dsp_block = |h: &mut BuilderProcHarness,
                                 drv: &mut VarispeedDriver,
                                 now_ns: u64,
                                 ring_fill: usize| {
            h.proc
                .poll_updates(SinkChoice::Dsp, 48_000, RateRatio::ONE, true);
            let pull = {
                let port = h
                    .proc
                    .varispeed(SinkChoice::Dsp, RateRatio::ONE, 48_000, 2)
                    .unwrap();
                let p = drv.pull(port, now_ns, 1024, ring_fill, PullKind::Play);
                drv.after_pull(port, &slot);
                drv.publish(&drv_status);
                p
            };
            let inp = vec![0.0f32; (pull.take + pull.pad) * 2];
            h.proc.process_block(
                SinkChoice::Dsp,
                &inp,
                &[],
                &mut out,
                2,
                RateRatio::ONE,
                48_000,
                0,
            );
        };
        run_dsp_block(&mut h, &mut drv, 0, 4096);
        let st_active = status(&h, &drv_status);
        assert!(st_active.enabled && st_active.active);
        assert_eq!(st_active.requested, 1.05);
        assert_eq!(st_active.target, 1.05);

        // 2. Switch to Direct (inactive), then request 1.25: status must show
        // requested = 1.25 right away and never leak the old 1.05 target.
        let direct_in = vec![0.0f32; 1024 * 2];
        let mut direct_out = vec![0i32; 1024 * 2];
        h.proc.process_block(
            SinkChoice::Direct,
            &[],
            &direct_in,
            &mut direct_out,
            2,
            RateRatio::ONE,
            48_000,
            0,
        );
        h.state
            .set_varispeed(true, 1.25, 0.0, &h.mirror, None)
            .unwrap();
        slot.send(1.25, 0.0);
        let st_direct = status(&h, &drv_status);
        assert!(st_direct.enabled);
        assert!(!st_direct.active);
        assert_eq!(st_direct.requested, 1.25);
        assert_eq!(st_direct.speed, 1.0);
        assert_eq!(st_direct.target, 1.0);

        // 3. Return to DSP playback and trigger 3 underfeeds within 1 s so the
        // scheduler holds the stage at unity: `target` becomes 1.0 and `held`
        // is `Underfeed`, while `requested` still reports 1.25 (not 1.0).
        run_dsp_block(&mut h, &mut drv, 100_000_000, 4096);
        for i in 1..=4u64 {
            run_dsp_block(&mut h, &mut drv, 100_000_000 + i * 25_000_000, 0);
        }
        let st_held = status(&h, &drv_status);
        assert!(st_held.enabled && st_held.active);
        assert_eq!(st_held.held, Some(VarispeedHeldReason::Underfeed));
        assert_eq!(st_held.target, 1.0);
        assert_eq!(
            st_held.requested, 1.25,
            "underfeed hold must not clobber the accepted request"
        );
    }

    #[test]
    fn rapid_varispeed_toggles_on_full_rt_queue_accept_and_reconcile_while_budget_errors_reject() {
        use audivane_io_pw::EngineAudioProcessor;
        // 1-slot RT queue: the second toggle hits backpressure and must be
        // accepted & reconciled rather than returning INVALID_PARAMS and
        // secretly taking effect later.
        let mut h = BuilderProcHarness::with_varispeed(test_cfg(), 1);
        h.negotiate(48_000, RateRatio::ONE, SinkChoice::Dsp, SampleFormat::S32);

        h.state
            .set_varispeed(true, 1.1, 200.0, &h.mirror, None)
            .unwrap();
        assert!(h.state.cfg.varispeed.enabled);
        assert_eq!(h.mirror.varispeed_request(), (true, 1.1));
        assert!(!h.state.varispeed.pending());

        // Queue is full (Install hasn't been drained by RT yet): disable must
        // be accepted, persisted in cfg/mirror, and reconciled once RT drains.
        h.state
            .set_varispeed(false, 1.1, 200.0, &h.mirror, None)
            .unwrap();
        assert!(!h.state.cfg.varispeed.enabled);
        assert_eq!(h.mirror.varispeed_request(), (false, 1.1));
        assert!(h.state.varispeed.pending());

        // RT drains Install; reaper reconciles Disable; RT drains Disable.
        h.proc
            .poll_updates(SinkChoice::Dsp, 48_000, RateRatio::ONE, false);
        assert!(h.varispeed_applied.snapshot().enabled);
        h.state.reconcile_varispeed(&h.mirror);
        assert!(!h.state.varispeed.pending());
        h.proc
            .poll_updates(SinkChoice::Dsp, 48_000, RateRatio::ONE, false);
        assert!(!h.varispeed_applied.snapshot().enabled);

        // By contrast, a static budget rejection must return INVALID_PARAMS,
        // leave cfg/mirror untouched, and leave nothing pending for retry.
        let r = RateRatio::new(147, 160);
        let mut heavy = test_cfg();
        heavy.chain.upsampler = audivane_dsp_core::resample::ManualConfig {
            target: RateTarget::Rate(705_600),
            taps: 64,
            taps_unit: audivane_protocol::TapsUnit::Input,
            cutoff: 0.95,
            ..Default::default()
        }
        .into_upsampler_config()
        .unwrap();
        let mut h2 = BuilderProcHarness::with_varispeed(heavy, 1);
        h2.negotiate(768_000, r, SinkChoice::Dsp, SampleFormat::S32);
        let err = h2
            .state
            .set_varispeed(true, 1.05, 200.0, &h2.mirror, None)
            .unwrap_err();
        assert_eq!(err.code, RpcError::INVALID_PARAMS);
        assert!(!h2.state.cfg.varispeed.enabled);
        assert_eq!(h2.mirror.varispeed_request(), (false, 1.0));
        assert!(!h2.state.varispeed.pending());
    }

    #[test]
    fn engine_accepts_varispeed_down_to_quantum_32_and_rejects_16() {
        let mut ir_cache = IrCache::new();
        let mut cfg = test_cfg();
        cfg.varispeed.enabled = true;
        cfg.varispeed.speed = 1.05;

        for q in [32, 64, 128, 8192] {
            cfg.quantum = q;
            validate_engine_config(&cfg, &mut ir_cache, 48_000).unwrap();
        }
        for bad in [16, 48, 16384] {
            cfg.quantum = bad;
            let err = validate_engine_config(&cfg, &mut ir_cache, 48_000).unwrap_err();
            assert_eq!(err.code, RpcError::INVALID_PARAMS);
            assert!(
                err.message.contains("between 32 and 8192"),
                "quantum {bad}: {err}"
            );
        }
    }

    #[test]
    fn an_edit_during_a_rate_change_builds_for_the_negotiated_rate() {
        // Playing 44.1 kHz; the graph moves to 96 kHz and the reopen has not
        // landed yet when the EQ changes. The device still runs 44.1 kHz,
        // so the new chain must be built for it: a 12 dB cut at 1 kHz on a
        // -6 dBFS 1 kHz tone gives -18 dBFS, not a filter tuned for 96 kHz
        // (whose 1 kHz cut lands near 460 Hz here).
        let mut cfg = test_cfg();
        cfg.chain.eq = Some(eq_profile(0.0, 0.0));
        let mut h = BuilderProcHarness::new(cfg.clone());
        h.negotiate(44_100, RateRatio::ONE, SinkChoice::Dsp, SampleFormat::S32);
        let (db, silent) = tone_peak_44k(&mut h, SinkChoice::Dsp, RateRatio::ONE);
        assert!(!silent && (db + 6.0).abs() < 0.1, "{db:.2} dBFS");

        let mut edited = cfg;
        edited.chain.eq = Some(eq_profile(0.0, -12.0));
        for state in [EngineState::Playing, EngineState::Switching] {
            h.state
                .apply_config(
                    &edited,
                    &h.mirror,
                    state,
                    96_000,
                    Some(SinkChoice::Dsp),
                    None,
                )
                .unwrap();
            let (db, silent) = tone_peak_44k(&mut h, SinkChoice::Dsp, RateRatio::ONE);
            assert!(!silent, "{state:?}");
            assert!((db + 18.0).abs() < 0.3, "{state:?}: {db:.2} dBFS");
        }
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

    /// Test 18 (engine bypass resampler): Direct and DSP off away from x1
    /// play only the resample-only chain, gated like a DSP chain; the DSP
    /// sink never plays it, and it never plays a DSP chain.
    #[test]
    fn direct_and_dsp_off_play_the_engine_resampler_away_from_x1() {
        let mut cfg = test_cfg();
        cfg.chain.eq = Some(eq_profile(0.0, -6.0));
        let r = RateRatio::from_rates(44_100, 48_000);
        let mut h = BuilderProcHarness::new(cfg.clone());
        // Direct opened 44.1 kHz on a DAC without it, at 160/147.
        let chain = h.negotiate(44_100, r, SinkChoice::Direct, SampleFormat::S32);
        let seq = chain.required_chain_seq.expect("the resume waits for it");
        assert!(!chain.bypass_unbuildable && chain.error.is_none());
        // Not ready (and silent, reported) before the swap is taken.
        assert!(!audivane_io_pw::EngineAudioProcessor::is_chain_ready(
            &h.proc,
            SinkChoice::Direct,
            r,
            Some(seq)
        ));
        audivane_io_pw::EngineAudioProcessor::poll_updates(
            &mut h.proc,
            SinkChoice::Direct,
            44_100,
            r,
            false,
        );
        assert!(audivane_io_pw::EngineAudioProcessor::is_chain_ready(
            &h.proc,
            SinkChoice::Direct,
            r,
            Some(seq)
        ));
        assert_eq!(
            h.proc.mode(SinkChoice::Direct, r),
            processor::PlayMode::ResampleOnly
        );
        // The tone plays at its level: no EQ (DSP stages never run there).
        let t = tone_at(&mut h, 44_100, SinkChoice::Direct, r);
        assert!(!t.silent);
        assert!(
            (t.peak_db + 6.0).abs() < 0.05,
            "Direct: {:.2} dBFS",
            t.peak_db
        );
        assert!(!h.proc.meters().bypass_muted());
        let st = h
            .mirror
            .bypass_status(
                EngineState::Playing,
                true,
                Some(SinkChoice::Direct),
                r,
                false,
            )
            .unwrap();
        assert!(st.active && st.reason.is_none());
        assert_eq!(st.mode, BypassResampler::Engine);
        // The DSP sink (DSP on) at the same ratio has no chain: the
        // resample-only one is not it.
        assert!(!audivane_io_pw::EngineAudioProcessor::has_chain_for(
            &h.proc,
            SinkChoice::Dsp,
            r
        ));
        let t = tone_at(&mut h, 44_100, SinkChoice::Dsp, r);
        assert!(t.silent);

        // DSP off on the DSP sink: the same leg.
        let mut off = cfg.clone();
        off.dsp_enabled = false;
        let mut h = BuilderProcHarness::new(off);
        let chain = h.negotiate(44_100, r, SinkChoice::Dsp, SampleFormat::S24);
        assert!(chain.required_chain_seq.is_some());
        let t = tone_at(&mut h, 44_100, SinkChoice::Dsp, r);
        assert!(!t.silent);
        assert!(
            (t.peak_db + 6.0).abs() < 0.05,
            "DSP off: {:.2} dBFS",
            t.peak_db
        );
        // While waiting (away from x1 without the chain) status says so.
        let st = h
            .mirror
            .bypass_status(EngineState::Switching, true, Some(SinkChoice::Dsp), r, true)
            .unwrap();
        assert!(!st.active);
        assert!(st.reason.unwrap().contains("waiting"));
    }

    /// Test 6: the resample-only chain equals the DSP path's upsampler
    /// stage alone (the same design and ratio), bit for bit.
    #[test]
    fn the_resample_only_chain_equals_the_dsp_upsampler_alone() {
        for (g, o, format) in [
            (44_100, 48_000, SampleFormat::S32),
            (48_000, 44_100, SampleFormat::S24),
            (176_400, 96_000, SampleFormat::S16),
            (44_100, 88_200, SampleFormat::S32),
        ] {
            let r = RateRatio::from_rates(g, o);
            let mut cfg = test_cfg();
            // The DSP path with only the recommended upsampler.
            cfg.chain.upsampler = crate::control::config::bypass_upsampler();
            cfg.chain.upsampler.target = RateTarget::Rate(o);
            let capped = cap_dither_bits(&cfg, Some(format));
            let mut ir = IrCache::new();
            let (dsp, _, _) = build_active_chain(&capped, &mut ir, g, r, AbSide::A).unwrap();
            let resample = build_resample_chain(&capped, g, r).unwrap();
            assert!(resample.is_resample() && !dsp.is_resample());
            assert_eq!(dsp.latency_out_frames(), resample.latency_out_frames());
            let (mut a, mut b) = (dsp, resample);
            let mut gain_a = audivane_dsp_core::gain::StereoGainRamp::unity(o);
            let mut gain_b = audivane_dsp_core::gain::StereoGainRamp::unity(o);
            let mut pos = 0usize;
            for block in [1usize, 441, 1024, 7, 2048] {
                let inp: Vec<f32> = (0..block * 2)
                    .map(|i| {
                        let t = (pos * 2 + i) as f64;
                        (0.4 * (t * 0.0123).sin() + 0.1 * (t * 0.31).cos()) as f32
                    })
                    .collect();
                let n = a.output_frames(block) * 2;
                assert_eq!(n, b.output_frames(block) * 2);
                let (mut oa, mut ob) = (vec![0_i32; n], vec![1_i32; n]);
                a.process_f32_with_gain(&inp, &mut oa, 2, 8192, &mut gain_a, None);
                b.process_f32_with_gain(&inp, &mut ob, 2, 8192, &mut gain_b, None);
                assert_eq!(oa, ob, "{g} -> {o} at block {block}");
                pos += block;
            }
        }
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

    /// DSP on <-> off at the same ratio (a DSP chain at the landing's
    /// ratio) swaps the chains through the edit, no reopen; an edit that
    /// leaves the resample-only chain's inputs alone keeps it.
    #[test]
    fn dsp_on_off_at_the_landing_ratio_swaps_chains() {
        let r = RateRatio::from_rates(44_100, 48_000);
        let mut on = test_cfg();
        on.chain.upsampler = audivane_dsp_core::resample::ManualConfig {
            target: RateTarget::Rate(48_000),
            taps: 64,
            taps_unit: audivane_protocol::TapsUnit::Input,
            ..Default::default()
        }
        .into_upsampler_config()
        .unwrap();
        on.chain.eq = Some(eq_profile(0.0, -6.0));
        let mut off = on.clone();
        off.dsp_enabled = false;
        let mut h = BuilderProcHarness::new(on.clone());
        // What the open found: a DAC without 44.1 kHz.
        let mut key = probed(&dac(&[48_000, 96_000]));
        key.graph_rate = 44_100;
        h.state.negotiated_key = Some(key);
        h.negotiate(44_100, r, SinkChoice::Dsp, SampleFormat::S32);
        let t = tone_at(&mut h, 44_100, SinkChoice::Dsp, r);
        assert!((t.peak_db + 12.0).abs() < 0.5, "DSP: {:.2} dBFS", t.peak_db);
        // DSP off: no reopen (the landing is the running ratio), the
        // resample-only chain is built and swapped in.
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
        // The design may still run (off the builder lock): the previous
        // chain plays until it is done and the resample-only chain swaps in.
        h.wait_bypass_designs();
        assert!(h.state.last_built_resample);
        h.proc.set_dsp_enabled_for_test(false);
        let t = tone_at(&mut h, 44_100, SinkChoice::Dsp, r);
        assert!(!t.silent);
        assert!(
            (t.peak_db + 6.0).abs() < 0.05,
            "DSP off: {:.2} dBFS",
            t.peak_db
        );
        // An EQ edit with DSP off leaves it in place.
        let mut eq_edit = off.clone();
        eq_edit.chain.eq = Some(eq_profile(0.0, -3.0));
        let seq = h.state.next_swap_seq;
        h.state
            .apply_config(
                &eq_edit,
                &h.mirror,
                EngineState::Playing,
                44_100,
                Some(SinkChoice::Dsp),
                None,
            )
            .unwrap();
        assert_eq!(h.state.next_swap_seq, seq);
        // DSP on again: the DSP chain comes back.
        let reopen = h
            .state
            .apply_config(
                &on,
                &h.mirror,
                EngineState::Playing,
                44_100,
                Some(SinkChoice::Dsp),
                None,
            )
            .unwrap();
        assert!(!reopen);
        assert!(!h.state.last_built_resample);
        h.proc.set_dsp_enabled_for_test(true);
        let t = tone_at(&mut h, 44_100, SinkChoice::Dsp, r);
        assert!(
            (t.peak_db + 12.0).abs() < 0.5,
            "DSP on: {:.2} dBFS",
            t.peak_db
        );
    }

    #[test]
    fn engine_conversion_counts_as_active_dsp_at_unity_volume() {
        use EngineState::{Idle, Playing};
        let direct = Some(SinkChoice::Direct);
        let dsp = Some(SinkChoice::Dsp);
        // Unity volume, no DSP effect: only the engine's conversion marks it.
        assert!(!dsp_active(Playing, false, false, direct, false));
        assert!(dsp_active(Playing, false, true, direct, false));
        assert!(dsp_active(Playing, false, true, dsp, false));
        // As before: a digital part anywhere, the DSP chain on the DSP sink.
        assert!(dsp_active(Playing, true, false, direct, false));
        assert!(dsp_active(Playing, false, false, dsp, true));
        assert!(!dsp_active(Playing, false, false, direct, true));
        assert!(!dsp_active(Idle, true, true, dsp, true));
    }

    #[test]
    fn a_volume_change_during_a_slow_bypass_design_completes_promptly() {
        // DSP off opens 44.1 kHz at 160/147 on a 48 kHz-only DAC while the
        // pair's design is cold and slow (a fake that blocks until told).
        // The negotiation must not wait for it (the driver falls back to
        // PipeWire meanwhile), and neither must a volume change, which
        // needs the same builder lock.
        let mut cfg = test_cfg();
        cfg.dsp_enabled = false;
        let r = RateRatio::from_rates(44_100, 48_000);
        let mut h = BuilderProcHarness::new(cfg.clone());
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
        let BuilderProcHarness {
            state,
            mirror,
            ab_atomic,
            ..
        } = h;
        let builder = Arc::new(Mutex::new(state));
        let mirror = Arc::new(mirror);

        let (b, m, a) = (
            Arc::clone(&builder),
            Arc::clone(&mirror),
            Arc::clone(&ab_atomic),
        );
        let (n, took) = thread::spawn(move || {
            let mut st = b.lock().unwrap();
            let t = Instant::now();
            let n =
                st.apply_negotiated(44_100, r, SinkChoice::Dsp, SampleFormat::S32, &m, &a, None);
            (n, t.elapsed())
        })
        .join()
        .unwrap();
        assert!(n.bypass_pending && n.required_chain_seq.is_none(), "{n:?}");
        // A negotiation that waited would never return (the design is gated
        // until later); the bound only has to sit well above a loaded run.
        assert!(took < Duration::from_secs(2), "negotiation took {took:?}");
        // Status says why PipeWire converts meanwhile (the driver's
        // fallback opened at 48 kHz, x1).
        let reason = |m: &SharedMirror| {
            m.bypass_status(
                EngineState::Playing,
                true,
                Some(SinkChoice::Dsp),
                RateRatio::ONE,
                false,
            )
            .and_then(|s| s.reason)
        };
        assert!(
            reason(&mirror).is_some_and(|r| r.contains("still building its 44100 Hz to 48000 Hz"))
        );

        let mut louder = cfg.clone();
        louder.volume.volume_db = -10.0;
        let (b, m) = (Arc::clone(&builder), Arc::clone(&mirror));
        let t = Instant::now();
        let applied = thread::spawn(move || {
            b.lock().unwrap().apply_config(
                &louder,
                &m,
                EngineState::Playing,
                44_100,
                Some(SinkChoice::Dsp),
                None,
            )
        })
        .join()
        .unwrap();
        let took = t.elapsed();
        assert!(!applied.unwrap());
        assert!(
            took < Duration::from_secs(2),
            "the volume change took {took:?}"
        );
        assert!((mirror.volume_status().volume_db + 10.0).abs() < 1e-9);
        // Still designing: nothing was waited for.
        let pending = bypass_design::DesignKey {
            output: cap_dither_bits(&cfg, Some(SampleFormat::S32)).chain.output,
            quantum: cfg.quantum,
            input: 44_100,
            ratio: r,
        };
        assert_eq!(
            builder.lock().unwrap().designer.peek(&pending),
            bypass_design::Design::Pending
        );

        // The design finishes: the next negotiation (the driver comes back
        // to the pair) builds the chain at once.
        {
            let (open, cv) = &*gate;
            *open.lock().unwrap() = true;
            cv.notify_all();
        }
        builder.lock().unwrap().designer.wait_idle();
        let mut st = builder.lock().unwrap();
        let n = st.apply_negotiated(
            44_100,
            r,
            SinkChoice::Dsp,
            SampleFormat::S32,
            &mirror,
            &ab_atomic,
            None,
        );
        assert!(!n.bypass_pending && n.required_chain_seq.is_some(), "{n:?}");
        assert!(st.last_built_resample);
        assert_eq!(reason(&mirror), None);
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
    fn a_volume_change_keeps_a_dsp_toggle_waiting_for_its_design() {
        // DSP off while the resample-only design runs, then a volume change
        // before it is done: the request carries over, the DSP chain keeps
        // playing, and the resample-only chain still swaps in.
        let (designer, gate) = gated_designer(|_| false);
        let (mut h, on, r) = dsp_on_at_160_147(designer);
        let mut off = on.clone();
        off.dsp_enabled = false;
        let apply = |h: &mut BuilderProcHarness, cfg: &EngineConfig| {
            h.state
                .apply_config(
                    cfg,
                    &h.mirror,
                    EngineState::Playing,
                    44_100,
                    Some(SinkChoice::Dsp),
                    None,
                )
                .unwrap()
        };
        assert!(!apply(&mut h, &off));
        let mut quieter = off.clone();
        quieter.volume.volume_db = -10.0;
        assert!(!apply(&mut h, &quieter));
        let p = h.state.pending_resample.clone().expect("still waiting");
        assert_eq!(p.generation, h.state.config_generation);
        let t = tone_at(&mut h, 44_100, SinkChoice::Dsp, r);
        assert!(!t.silent && !h.proc.meters().bypass_muted());
        open_design_gate(&gate);
        h.wait_bypass_designs();
        assert!(h.state.last_built_resample && h.state.pending_resample.is_none());
        assert!(!tone_at(&mut h, 44_100, SinkChoice::Dsp, r).silent);
        assert!(h.proc.chain_fits(SinkChoice::Dsp, r));
    }

    #[test]
    fn an_older_dsp_swap_installing_keeps_a_dsp_toggles_hold() {
        // An EQ edit at 160/147 starts its swap (warming), then DSP off
        // while the resample-only design is cold: the EQ chain installing
        // is not the chain the toggle waits for, so the DSP chain keeps
        // playing until the resample-only one takes over.
        let (designer, gate) = gated_designer(|_| false);
        let (mut h, on, r) = dsp_on_at_160_147(designer);
        let mut eq = on.clone();
        eq.chain.eq = Some(eq_profile(0.0, -1.0));
        let mut off = eq.clone();
        off.dsp_enabled = false;
        for cfg in [&eq, &off] {
            h.state
                .apply_config(
                    cfg,
                    &h.mirror,
                    EngineState::Playing,
                    44_100,
                    Some(SinkChoice::Dsp),
                    None,
                )
                .unwrap();
        }
        assert!(h.state.pending_resample.is_some());
        let before = h.proc.active_generation();
        for _ in 0..3 {
            let t = tone_at(&mut h, 44_100, SinkChoice::Dsp, r);
            assert!(!t.silent && !h.proc.meters().bypass_muted());
            while h.recycle_rx.pop().is_ok() {}
        }
        assert!(
            h.proc.active_generation() > before,
            "the EQ swap never installed"
        );
        assert!(!h.proc.chain_fits(SinkChoice::Dsp, r));
        open_design_gate(&gate);
        h.wait_bypass_designs();
        assert!(!tone_at(&mut h, 44_100, SinkChoice::Dsp, r).silent);
        assert!(h.proc.chain_fits(SinkChoice::Dsp, r));
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

    #[test]
    fn an_edits_late_design_never_overwrites_a_newer_negotiation() {
        // DSP off waits for the resample-only design; it finishes, and
        // before its callback runs a sink switch negotiates again at the
        // same rate and ratio and takes the chain. The late callback must
        // not publish the edit's chain over the negotiation's.
        let (designer, gate) = gated_designer(|_| false);
        let (mut h, on, r) = dsp_on_at_160_147(designer);
        let mut off = on.clone();
        off.dsp_enabled = false;
        h.state
            .apply_config(
                &off,
                &h.mirror,
                EngineState::Playing,
                44_100,
                Some(SinkChoice::Dsp),
                None,
            )
            .unwrap();
        assert!(h.state.pending_resample.is_some());
        open_design_gate(&gate);
        h.state.designer.wait_idle();
        let n = h.state.apply_negotiated(
            44_100,
            r,
            SinkChoice::Dsp,
            SampleFormat::S32,
            &h.mirror,
            &h.ab_atomic,
            None,
        );
        n.required_chain_seq
            .expect("the negotiation builds its chain");
        assert!(!n.bypass_pending && h.state.last_built_resample);
        // The spare is built again; then the late callback.
        h.state.designer.wait_idle();
        let swaps = h.state.next_swap_seq;
        h.state.commit_designed(&h.mirror, None);
        assert_eq!(h.state.next_swap_seq, swaps, "a second chain was published");
    }

    /// The driver's side of the bypass pairs, as its control loop keeps
    /// them (the held pairs, and the desired config the policies and the
    /// RT thread follow), with what the engine told it.
    struct DriverSide {
        cfg: EngineDriverConfig,
        held: audivane_io_pw::BypassHeld,
        desired: audivane_io_pw::DesiredDspConfig,
        notes: Arc<Mutex<Vec<audivane_io_pw::BypassNote>>>,
    }

    impl DriverSide {
        fn new(h: &mut BuilderProcHarness, cfg: &EngineConfig) -> Self {
            let cfg = to_driver_config(cfg);
            let held = audivane_io_pw::BypassHeld::NONE;
            let base = cfg.desired_dsp(0).keep_bypass(held.keep());
            let notes: Arc<Mutex<Vec<audivane_io_pw::BypassNote>>> = Arc::default();
            let n = Arc::clone(&notes);
            h.state.bypass_notify = Some(Arc::new(move |note| n.lock().unwrap().push(note)));
            Self {
                cfg,
                held,
                desired: base.next(base, true),
                notes,
            }
        }

        /// The policies offer 44.1 kHz converted to 48 kHz by the engine.
        fn offers_the_pair(&self) -> bool {
            self.desired.bypass.engine && self.desired.bypass.buildable.allows(44_100, 48_000)
        }

        /// `CtrlReq::UpdateConfig`, as far as the bypass pairs go.
        fn update(&mut self, cfg: &EngineConfig) {
            if self.cfg.device != cfg.device {
                self.held = audivane_io_pw::BypassHeld::NONE;
            }
            self.cfg = to_driver_config(cfg);
            let base = self.cfg.desired_dsp(0).keep_bypass(self.held.keep());
            self.desired = self.desired.next(base, false);
        }

        /// `CtrlReq::Bypass`, for each note the engine sent.
        fn hear(&mut self) {
            let notes: Vec<_> = self.notes.lock().unwrap().drain(..).collect();
            for note in notes {
                let cfg = self.cfg.clone();
                audivane_io_pw::lifecycle::bypass_note(
                    &mut self.desired,
                    &mut self.held,
                    note,
                    |keep| cfg.desired_dsp(0).keep_bypass(keep),
                );
            }
        }

        /// An open with a 44.1 kHz client on a 48 kHz-only DAC, through
        /// the driver's fallback: the graph stays at 44.1 kHz (160/147)
        /// when the policies offer the pair, else it moves to 48 kHz.
        fn open(&mut self, h: &mut BuilderProcHarness, format: SampleFormat) -> (u32, RateRatio) {
            struct Open<'a> {
                h: &'a mut BuilderProcHarness,
                format: SampleFormat,
            }
            impl audivane_io_pw::lifecycle::BypassOpen for Open<'_> {
                type Output = (u32, RateRatio);
                type Negotiated = NegotiatedChain;
                fn open(
                    &mut self,
                    _: Option<(u32, RateRatio)>,
                    desired: audivane_io_pw::DesiredDspConfig,
                ) -> Result<(u32, RateRatio), String> {
                    Ok(
                        if desired.bypass.engine && desired.bypass.buildable.allows(44_100, 48_000)
                        {
                            (44_100, RateRatio::from_rates(44_100, 48_000))
                        } else {
                            (48_000, RateRatio::ONE)
                        },
                    )
                }
                fn negotiate(
                    &mut self,
                    out: &(u32, RateRatio),
                    _: audivane_io_pw::DesiredDspConfig,
                ) -> (NegotiatedChain, Option<audivane_io_pw::BypassMiss>) {
                    let h = &mut *self.h;
                    let n = h.state.apply_negotiated(
                        out.0,
                        out.1,
                        SinkChoice::Dsp,
                        self.format,
                        &h.mirror,
                        &h.ab_atomic,
                        None,
                    );
                    let miss = n.bypass_miss(out.0, out.1.output_rate(out.0).unwrap());
                    (n, miss)
                }
                fn republish(&mut self, _: audivane_io_pw::DesiredDspConfig) {}
            }
            let cfg = self.cfg.clone();
            let (out, _) = audivane_io_pw::lifecycle::open_and_keep(
                &mut Open { h, format },
                &mut self.desired,
                &mut self.held,
                |keep| cfg.desired_dsp(0).keep_bypass(keep),
            )
            .unwrap();
            out
        }
    }

    /// DSP off at 44.1 kHz on a 48 kHz-only DAC, the pair's design cold
    /// (held by the gate): the driver falls back to 48 kHz.
    fn cold_pair(
        cfg: &EngineConfig,
        format: SampleFormat,
    ) -> (
        BuilderProcHarness,
        DriverSide,
        DesignGate,
        bypass_design::DesignKey,
    ) {
        let (designer, gate) = gated_designer(|_| false);
        let mut h = BuilderProcHarness::new(cfg.clone());
        h.state.designer = designer;
        let mut key = probed(&dac(&[48_000]));
        key.graph_rate = 44_100;
        h.state.negotiated_key = Some(key);
        let mut d = DriverSide::new(&mut h, cfg);
        assert!(d.offers_the_pair());
        assert_eq!(d.open(&mut h, format), (48_000, RateRatio::ONE));
        assert!(!d.offers_the_pair());
        let old = h.state.designing.clone().expect("designing");
        (h, d, gate, old)
    }

    #[test]
    fn an_edit_that_drops_a_pending_design_lets_the_driver_ask_for_the_new_one() {
        // The pair is held while its design runs; then an edit of what the
        // design depends on (the output section, the quantum, the bypass
        // mode off and on) drops it. The driver must release the pair, so
        // the policies offer it again and the next open asks for its new
        // design; that design's end brings the pair back. The old design
        // never answers for it.
        let mut off = test_cfg();
        off.dsp_enabled = false;
        off.chain.output.bits = 24;
        let r = RateRatio::from_rates(44_100, 48_000);
        let mut sixteen = off.clone();
        sixteen.chain.output.bits = 16;
        let mut quantum = off.clone();
        quantum.quantum = 2048;
        let mut pipewire = off.clone();
        pipewire.rate.bypass_resampler = BypassResampler::Pipewire;
        let edits: [(&str, Vec<&EngineConfig>); 3] = [
            ("output", vec![&sixteen]),
            ("quantum", vec![&quantum]),
            ("bypass mode", vec![&pipewire, &off]),
        ];
        for (what, steps) in edits {
            let (mut h, mut d, gate, old) = cold_pair(&off, SampleFormat::S32);
            for cfg in steps {
                h.state
                    .apply_config(
                        cfg,
                        &h.mirror,
                        EngineState::Playing,
                        48_000,
                        Some(SinkChoice::Dsp),
                        None,
                    )
                    .unwrap();
                d.update(cfg);
            }
            d.hear();
            assert!(d.offers_the_pair(), "{what}: the pair stays held");
            // A client at 44.1 kHz moves the graph there again: the open
            // asks for the new design, and falls back while it runs.
            assert_eq!(
                d.open(&mut h, SampleFormat::S32),
                (48_000, RateRatio::ONE),
                "{what}"
            );
            let new = h.state.designing.clone().expect("designing");
            open_design_gate(&gate);
            h.state.designer.wait_idle();
            // The callbacks: the old design (when it is another) says
            // nothing; the new one brings the pair back.
            if old != new {
                h.state.designed(&old, true, &h.mirror, None);
                d.hear();
                assert!(!d.offers_the_pair(), "{what}: the old design answered");
            }
            h.state.designed(&new, true, &h.mirror, None);
            d.hear();
            assert!(d.offers_the_pair(), "{what}");
            assert_eq!(d.open(&mut h, SampleFormat::S32), (44_100, r), "{what}");
            assert!(h.state.last_built_resample, "{what}");
        }
    }

    #[test]
    fn an_output_edit_the_format_caps_away_keeps_the_pending_design() {
        // On a 16-bit device, dither to 24 or to 20 bits both run at 16:
        // the edit changes no design, so nothing is dropped and the design
        // in flight still brings the pair back.
        let mut off = test_cfg();
        off.dsp_enabled = false;
        off.chain.output.bits = 24;
        let (mut h, mut d, gate, old) = cold_pair(&off, SampleFormat::S16);
        let epoch = h.state.design_epoch;
        let mut twenty = off.clone();
        twenty.chain.output.bits = 20;
        h.state
            .apply_config(
                &twenty,
                &h.mirror,
                EngineState::Playing,
                48_000,
                Some(SinkChoice::Dsp),
                None,
            )
            .unwrap();
        d.update(&twenty);
        assert_eq!(h.state.design_epoch, epoch);
        d.hear();
        assert!(!d.offers_the_pair());
        open_design_gate(&gate);
        h.state.designer.wait_idle();
        h.state.designed(&old, true, &h.mirror, None);
        d.hear();
        assert!(d.offers_the_pair());
        assert_eq!(
            d.open(&mut h, SampleFormat::S16),
            (44_100, RateRatio::from_rates(44_100, 48_000))
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
    fn dither_cap_follows_the_format() {
        let mut cfg = test_cfg();
        cfg.chain.output.bits = 24;
        assert_eq!(
            cap_dither_bits(&cfg, Some(SampleFormat::S16))
                .chain
                .output
                .bits,
            16
        );
        assert_eq!(
            cap_dither_bits(&cfg, Some(SampleFormat::S24_3))
                .chain
                .output
                .bits,
            24
        );
        assert_eq!(
            cap_dither_bits(&cfg, Some(SampleFormat::S32))
                .chain
                .output
                .bits,
            24
        );
        assert_eq!(cap_dither_bits(&cfg, None).chain.output.bits, 24);
    }

    #[test]
    fn builder_eq_then_ratio_change_in_same_cycle_preserves_eq_at_4x() {
        let mut h = BuilderProcHarness::new(test_cfg());

        // Apply 1: EQ while idle (gen 1, no reopen)
        let mut eq_cfg = test_cfg();
        eq_cfg.chain.eq = Some(eq_profile(-3.0, -2.0));
        let reopen1 = h
            .state
            .apply_config(&eq_cfg, &h.mirror, EngineState::Ready, 48_000, None, None)
            .unwrap();
        assert!(!reopen1);

        // Apply 2: immediate 4x upsample before RT polls or after RT polls (gen 2, reopen)
        let mut up_cfg = eq_cfg.clone();
        up_cfg.chain.upsampler = audivane_dsp_core::resample::ManualConfig {
            target: RateTarget::Ratio(4),
            taps: 8192,
            ..Default::default()
        }
        .into_upsampler_config()
        .unwrap();
        let reopen2 = h
            .state
            .apply_config(&up_cfg, &h.mirror, EngineState::Ready, 48_000, None, None)
            .unwrap();
        assert!(reopen2);

        // Simulate both RT polling between apply 1 and 2, AND skipping straight to on_negotiated
        h.negotiate(
            48_000,
            RateRatio::integer(4),
            SinkChoice::Dsp,
            SampleFormat::S32,
        );
        audivane_io_pw::EngineAudioProcessor::poll_updates(
            &mut h.proc,
            SinkChoice::Dsp,
            48_000,
            RateRatio::integer(4),
            false,
        );

        assert_eq!(h.active_gen(), 2);
        let db = h.measure_1khz_peak_db(4);
        assert!(
            (db - (-11.0)).abs() < 0.2,
            "expected -11.0 dBFS after eq->4x, got {db:.3} dBFS"
        );
    }

    #[test]
    fn builder_ratio_change_then_eq_both_before_and_after_negotiated() {
        // Case A: ratio 4x apply -> EQ apply arrives BEFORE on_negotiated completes.
        let mut h_before = BuilderProcHarness::new(test_cfg());
        let mut up_cfg = test_cfg();
        up_cfg.chain.upsampler = audivane_dsp_core::resample::ManualConfig {
            target: RateTarget::Ratio(4),
            taps: 8192,
            ..Default::default()
        }
        .into_upsampler_config()
        .unwrap();
        assert!(
            h_before
                .state
                .apply_config(
                    &up_cfg,
                    &h_before.mirror,
                    EngineState::Playing,
                    48_000,
                    Some(SinkChoice::Dsp),
                    None
                )
                .unwrap()
        );

        let mut eq_up_cfg = up_cfg.clone();
        eq_up_cfg.chain.eq = Some(eq_profile(-3.0, -2.0));
        assert!(
            !h_before
                .state
                .apply_config(
                    &eq_up_cfg,
                    &h_before.mirror,
                    EngineState::Switching,
                    48_000,
                    None,
                    None
                )
                .unwrap()
        );

        h_before.state.apply_negotiated(
            48_000,
            RateRatio::integer(4),
            SinkChoice::Dsp,
            SampleFormat::S32,
            &h_before.mirror,
            &h_before.ab_atomic,
            None,
        );
        audivane_io_pw::EngineAudioProcessor::poll_updates(
            &mut h_before.proc,
            SinkChoice::Dsp,
            48_000,
            RateRatio::integer(4),
            false,
        );
        assert_eq!(h_before.active_gen(), 2);
        let db_a = h_before.measure_1khz_peak_db(4);
        assert!(
            (db_a - (-11.0)).abs() < 0.2,
            "Case A expected -11.0 dBFS, got {db_a:.3}"
        );

        // Case B: ratio 4x apply -> on_negotiated completes -> EQ apply arrives.
        let mut h_after = BuilderProcHarness::new(test_cfg());
        assert!(
            h_after
                .state
                .apply_config(
                    &up_cfg,
                    &h_after.mirror,
                    EngineState::Ready,
                    48_000,
                    None,
                    None
                )
                .unwrap()
        );
        h_after.state.apply_negotiated(
            48_000,
            RateRatio::integer(4),
            SinkChoice::Dsp,
            SampleFormat::S32,
            &h_after.mirror,
            &h_after.ab_atomic,
            None,
        );
        audivane_io_pw::EngineAudioProcessor::poll_updates(
            &mut h_after.proc,
            SinkChoice::Dsp,
            48_000,
            RateRatio::integer(4),
            false,
        );
        assert_eq!(h_after.active_gen(), 1);

        assert!(
            !h_after
                .state
                .apply_config(
                    &eq_up_cfg,
                    &h_after.mirror,
                    EngineState::Ready,
                    48_000,
                    Some(SinkChoice::Dsp),
                    None
                )
                .unwrap()
        );
        audivane_io_pw::EngineAudioProcessor::poll_updates(
            &mut h_after.proc,
            SinkChoice::Dsp,
            48_000,
            RateRatio::integer(4),
            false,
        );
        assert_eq!(h_after.active_gen(), 2);
        let db_b = h_after.measure_1khz_peak_db(4);
        assert!(
            (db_b - (-11.0)).abs() < 0.2,
            "Case B expected -11.0 dBFS, got {db_b:.3}"
        );
    }

    #[test]
    fn builder_three_rapid_applies_and_direct_reopen_skip_fix() {
        let mut h = BuilderProcHarness::new(test_cfg());

        // Direct mode active: apply 4x (gen 1), then revert to 1x + EQ (gen 2),
        // then apply deeper EQ (gen 3). Even though on_negotiated fires on Direct
        // at the same rate 48 kHz, `cfg_stale` must force rebuilding the offline DSP chain!
        // (A quantum change reopens; a target change alone does not on
        // Direct, which stays at x1.)
        let mut c1 = test_cfg();
        c1.quantum = 512;
        c1.chain.upsampler = audivane_dsp_core::resample::ManualConfig {
            target: RateTarget::Ratio(4),
            taps: 8192,
            ..Default::default()
        }
        .into_upsampler_config()
        .unwrap();
        assert!(
            h.state
                .apply_config(
                    &c1,
                    &h.mirror,
                    EngineState::Playing,
                    48_000,
                    Some(SinkChoice::Direct),
                    None
                )
                .unwrap()
        );

        let mut c2 = test_cfg();
        c2.quantum = 256;
        c2.chain.eq = Some(eq_profile(-1.0, -1.0));
        assert!(
            h.state
                .apply_config(
                    &c2,
                    &h.mirror,
                    EngineState::Playing,
                    48_000,
                    Some(SinkChoice::Direct),
                    None
                )
                .unwrap()
        );
        // `c2` triggered reopen while Direct was active, so `built_generation` is stale (0 != 2).
        // `apply_negotiated` at the same 48 kHz rate on Direct rebuilds gen 2
        // instead of skipping.
        h.negotiate(
            48_000,
            RateRatio::integer(1),
            SinkChoice::Direct,
            SampleFormat::S32,
        );
        audivane_io_pw::EngineAudioProcessor::poll_updates(
            &mut h.proc,
            SinkChoice::Direct,
            48_000,
            RateRatio::integer(1),
            true,
        );
        assert_eq!(h.state.built_generation, 2);
        assert_eq!(h.active_gen(), 2);

        let mut c3 = c2.clone();
        c3.chain.eq = Some(eq_profile(-3.0, -2.0));
        assert!(
            !h.state
                .apply_config(
                    &c3,
                    &h.mirror,
                    EngineState::Playing,
                    48_000,
                    Some(SinkChoice::Direct),
                    None
                )
                .unwrap()
        );

        audivane_io_pw::EngineAudioProcessor::poll_updates(
            &mut h.proc,
            SinkChoice::Direct,
            48_000,
            RateRatio::integer(1),
            true,
        );
        assert_eq!(h.active_gen(), 3);

        // Failed validation must not bump config_generation.
        let mut invalid = c3.clone();
        invalid.quantum = 8;
        assert!(
            h.state
                .apply_config(&invalid, &h.mirror, EngineState::Ready, 48_000, None, None)
                .is_err()
        );
        assert_eq!(h.state.config_generation, 3);
        assert_eq!(h.state.built_generation, 3);

        // Now measure via DSP without reopening (Direct -> Dsp stays 1x).
        let db = h.measure_1khz_peak_db(1);
        assert!(
            (db - (-11.0)).abs() < 0.2,
            "expected gen 3 EQ (-11.0 dBFS), got {db:.3}"
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

    #[test]
    fn builder_deferred_publication_retries_across_full_queue_and_mutes_mismatch() {
        let mut h = BuilderProcHarness::new(test_cfg());
        // Fill the 32-slot swap queue with 1x swaps, the RT drain paused.
        h.proc.set_swap_drain_paused(true);
        for i in 1..=32 {
            let mut filler = test_cfg();
            filler.chain.eq = Some(eq_profile(-(i as f64) * 0.1, 0.0));
            assert!(
                !h.state
                    .apply_config(&filler, &h.mirror, EngineState::Ready, 48_000, None, None)
                    .unwrap()
            );
        }
        assert_eq!(h.state.built_generation, 32);

        // Apply a 4x ratio change with EQ (-3 preamp, -2 @ 1 kHz) -> reopen needed.
        let mut cfg_4x = test_cfg();
        cfg_4x.chain.eq = Some(eq_profile(-1.0, -1.0));
        cfg_4x.chain.upsampler = audivane_dsp_core::resample::ManualConfig {
            target: RateTarget::Ratio(4),
            taps: 8192,
            ..Default::default()
        }
        .into_upsampler_config()
        .unwrap();
        assert!(
            h.state
                .apply_config(
                    &cfg_4x,
                    &h.mirror,
                    EngineState::Playing,
                    48_000,
                    Some(SinkChoice::Dsp),
                    None,
                )
                .unwrap()
        );
        let gen_first_4x = h.state.config_generation;
        assert_eq!(h.state.built_generation, 32);

        // Negotiate 4x while the swap queue is still completely full.
        let neg1 = h.negotiate(
            48_000,
            RateRatio::integer(4),
            SinkChoice::Dsp,
            SampleFormat::S32,
        );
        let seq1 = neg1.required_chain_seq;
        assert!(seq1.is_some());
        assert!(neg1.error.is_none());
        // A failed push leaves built_generation at the last chain that reached
        // the queue.
        assert_eq!(h.state.built_generation, 32);
        assert!(h.state.pending_publication.is_some());
        assert!(h.state.has_pending_swap.load(Ordering::Acquire));
        assert!(!h.state.flush_pending_swap(&h.mirror, None));

        // With the 4x chain not installed, the resume is not ready and a block
        // at 4x plays silence, never mismatched passthrough.
        audivane_io_pw::EngineAudioProcessor::poll_updates(
            &mut h.proc,
            SinkChoice::Dsp,
            48_000,
            RateRatio::integer(4),
            false,
        );
        assert!(!audivane_io_pw::EngineAudioProcessor::is_chain_ready(
            &h.proc,
            SinkChoice::Dsp,
            RateRatio::integer(4),
            seq1,
        ));
        let inp = vec![0.4f32; 512 * 2];
        let mut out = vec![777_i32; 512 * 4 * 2];
        audivane_io_pw::EngineAudioProcessor::process_block(
            &mut h.proc,
            SinkChoice::Dsp,
            &inp,
            &inp,
            &mut out,
            2,
            RateRatio::integer(4),
            48_000,
            0,
        );
        assert!(
            out.iter().all(|&s| s == 0),
            "expected muted output while 4x chain is not installed"
        );

        // A newer EQ config arrives and supersedes the pending 4x publication while queue stays full.
        let mut cfg_4x_latest = cfg_4x.clone();
        cfg_4x_latest.chain.eq = Some(eq_profile(-3.0, -2.0));
        assert!(
            !h.state
                .apply_config(
                    &cfg_4x_latest,
                    &h.mirror,
                    EngineState::Switching,
                    48_000,
                    Some(SinkChoice::Dsp),
                    None,
                )
                .unwrap()
        );
        let gen_latest = h.state.config_generation;
        assert!(gen_latest > gen_first_4x);
        let neg_latest = h.negotiate(
            48_000,
            RateRatio::integer(4),
            SinkChoice::Dsp,
            SampleFormat::S32,
        );
        let seq_latest = neg_latest.required_chain_seq;
        assert!(seq_latest.unwrap() > seq1.unwrap());
        assert_eq!(
            h.state
                .pending_publication
                .as_ref()
                .map(|p| p.built_generation),
            Some(gen_latest)
        );
        assert_eq!(h.state.built_generation, 32);

        // Unpause RT drain + simulate reaper draining recycle_rx and flushing pending_publication.
        h.proc.set_swap_drain_paused(false);
        for _ in 0..8 {
            audivane_io_pw::EngineAudioProcessor::poll_updates(
                &mut h.proc,
                SinkChoice::Dsp,
                48_000,
                RateRatio::integer(4),
                false,
            );
            while h.recycle_rx.pop().is_ok() {}
            h.state.flush_pending_swap(&h.mirror, None);
        }
        audivane_io_pw::EngineAudioProcessor::poll_updates(
            &mut h.proc,
            SinkChoice::Dsp,
            48_000,
            RateRatio::integer(4),
            false,
        );
        while h.recycle_rx.pop().is_ok() {}

        assert_eq!(h.state.built_generation, gen_latest);
        assert!(h.state.pending_publication.is_none());
        assert!(!h.state.has_pending_swap.load(Ordering::Acquire));
        assert_eq!(h.active_gen(), gen_latest);
        assert!(audivane_io_pw::EngineAudioProcessor::is_chain_ready(
            &h.proc,
            SinkChoice::Dsp,
            RateRatio::integer(4),
            seq_latest,
        ));

        // Latest EQ (-3 preamp, -2 @ 1 kHz on -6 dBFS sine -> -11 dBFS) must be active!
        let db = h.measure_1khz_peak_db(4);
        assert!(
            (db - (-11.0)).abs() < 0.2,
            "expected latest gen EQ (-11.0 dBFS) after deferred retry, got {db:.3}"
        );
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
        }
    }

    fn expected_fallback_db() -> f64 {
        // -6 dBFS test sine through the -3 dB preamp, convolution disabled.
        -9.0
    }

    /// A ratio the configured upsampler cannot run at (an integer-only
    /// preset offered a fractional ratio) replaces only the upsampler:
    /// EQ and convolution stay.
    #[test]
    fn rational_fallback_replaces_only_the_upsampler() {
        let ir_path = temp_ir_path("rational_fallback");
        write_test_ir(&ir_path, 44_100);
        let mut cfg = test_cfg();
        cfg.chain.eq = Some(eq_profile(-3.0, 0.0));
        cfg.chain.convolution = vec![conv_cfg(&ir_path)];
        cfg.chain.upsampler = audivane_dsp_core::resample::ManualConfig {
            target: RateTarget::MaxRateAny(768_000),
            preset: Some(audivane_protocol::FilterPreset::CascadeFir),
            ..Default::default()
        }
        .into_upsampler_config()
        .unwrap();
        let ratio = RateRatio::from_rates(44_100, 192_000);
        let mut cache = IrCache::new();
        assert!(build_active_chain(&cfg, &mut cache, 44_100, ratio, AbSide::A).is_err());
        let (chain, _, _, given_up) =
            build_fallback_chain(&cfg, &mut cache, 44_100, ratio, AbSide::A).unwrap();
        assert!(given_up.contains("upsampler replaced"), "{given_up}");
        let ActiveChain::Single(chain) = chain else {
            panic!("expected a single chain");
        };
        assert_eq!(chain.ratio(), ratio);
        let c = chain.config();
        assert!(c.eq.is_some(), "EQ dropped");
        assert_eq!(c.convolution.len(), 1, "convolution dropped");
        assert_eq!(c.upsampler.preset, None);
        assert!(c.upsampler.design.is_some());

        // The driver is never offered that ratio in the first place.
        let mask = crate::control::config::buildable_rate_pairs(&cfg.chain.upsampler, 2);
        assert!(!mask.allows(44_100, 192_000));
        assert!(mask.allows(44_100, 176_400));
        let _ = std::fs::remove_dir_all(ir_path.parent().unwrap());
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
        }
    }

    #[test]
    fn direct_keeps_the_dsp_chain_warm_where_a_switch_lands() {
        // A 48 kHz-only DAC asked for x4: the DSP sink runs x1 there, and a
        // switch from Direct needs no reopen (the landing is x1 too). The
        // chain an edit builds meanwhile must be the x1 one, or the switch
        // waits for a chain only a negotiation builds.
        let mut cfg = test_cfg();
        cfg.chain.upsampler.target = RateTarget::Ratio(4);
        let only48 = probed(&dac(&[48_000]));
        assert_eq!(warm_dsp_ratio(&cfg, 48_000, Some(&only48)), RateRatio::ONE);
        let wide = probed(&dac(&[48_000, 192_000]));
        assert_eq!(
            warm_dsp_ratio(&cfg, 48_000, Some(&wide)),
            RateRatio::integer(4)
        );
        // Nothing known about the device: the configured ratio.
        assert_eq!(warm_dsp_ratio(&cfg, 48_000, None), RateRatio::integer(4));
    }

    #[test]
    fn a_direct_negotiation_warms_where_the_dsp_sink_lands() {
        // A 48 kHz-only DAC with max_rate_any 96 kHz: the DSP sink can only
        // land at x1 there. Direct's negotiation keeps the x1 chain warm,
        // as an edit would, not a x2 one only a reopen could use.
        let mut cfg = test_cfg();
        cfg.chain.upsampler.target = RateTarget::MaxRateAny(96_000);
        let mut h = BuilderProcHarness::new(cfg.clone());
        let mut key = probed(&dac(&[48_000]));
        key.graph_rate = 48_000;
        h.state.negotiated_key = Some(key);
        h.negotiate(
            48_000,
            RateRatio::ONE,
            SinkChoice::Direct,
            SampleFormat::S32,
        );
        assert_eq!(h.state.last_dsp_ratio, RateRatio::ONE);
        assert_eq!(
            h.state.last_dsp_ratio,
            warm_dsp_ratio(&cfg, 48_000, Some(&key))
        );
        // Nothing known about the device: the configured ratio.
        let mut h = BuilderProcHarness::new(cfg);
        h.negotiate(
            48_000,
            RateRatio::ONE,
            SinkChoice::Direct,
            SampleFormat::S32,
        );
        assert_eq!(h.state.last_dsp_ratio, RateRatio::integer(2));
    }

    #[test]
    fn target_edits_reopen_only_when_the_landing_moves() {
        let dsp = Some(SinkChoice::Dsp);
        let one = RateRatio::ONE;
        let with = |target| {
            let mut c = test_cfg();
            c.chain.upsampler.target = target;
            c
        };
        // A 48 kHz-only DAC at 48 kHz: x4 and x6 both land on x1.
        let only48 = probed(&dac(&[48_000]));
        let (x4, x6) = (with(RateTarget::Ratio(4)), with(RateTarget::Ratio(6)));
        assert!(!diff_requires_reopen(
            &x4,
            &x6,
            48_000,
            Some(&only48),
            one,
            dsp
        ));
        // On a DAC that plays 288 kHz, x6 moves the landing.
        let wide = probed(&dac(&[48_000, 192_000, 288_000]));
        assert!(diff_requires_reopen(
            &x4,
            &x6,
            48_000,
            Some(&wide),
            RateRatio::integer(4),
            dsp
        ));
        // max_rate_any cap edits with the same landing (192 kHz either way).
        let (cap192, cap200) = (
            with(RateTarget::MaxRateAny(192_000)),
            with(RateTarget::MaxRateAny(200_000)),
        );
        let x4r = RateRatio::integer(4);
        assert!(!diff_requires_reopen(
            &cap192,
            &cap200,
            48_000,
            Some(&wide),
            x4r,
            dsp
        ));
        // A cap that does move it.
        let cap300 = with(RateTarget::MaxRateAny(300_000));
        assert!(diff_requires_reopen(
            &cap192,
            &cap300,
            48_000,
            Some(&wide),
            x4r,
            dsp
        ));
        // Real ALSA parameters still always reopen.
        let mut other = x6.clone();
        other.formats = vec![SampleFormat::S16];
        assert!(diff_requires_reopen(
            &x6,
            &other,
            48_000,
            Some(&only48),
            one,
            dsp
        ));
        let mut device = x6.clone();
        device.device = "hw:Other".into();
        assert!(diff_requires_reopen(
            &x6,
            &device,
            48_000,
            Some(&only48),
            one,
            dsp
        ));
    }

    #[test]
    fn reopen_decisions_follow_the_open_plan() {
        use audivane_io_pw::lifecycle::negotiated_key;
        // Without a probe: a 48k device takes x4 and x5 but not x6.
        // cascade_fir at x6 (no x5) opened x4 after x6 was refused; a
        // manual x6 design lands on x5, although both ladders start at x6.
        let mut cascade = test_cfg();
        cascade.chain.upsampler = audivane_dsp_core::resample::ManualConfig {
            target: RateTarget::Ratio(6),
            preset: Some(audivane_protocol::FilterPreset::CascadeFir),
            ..Default::default()
        }
        .into_upsampler_config()
        .unwrap();
        let mut manual = cascade.clone();
        manual.chain.upsampler.preset = None;
        let takes = [48_000, 192_000, 240_000];
        let desired = to_driver_config(&cascade).desired_dsp(0);
        let ladder = desired.ratio_ladder_for(SinkChoice::Dsp, 48_000, None, &[]);
        let running = *ladder
            .iter()
            .find(|q| takes.contains(&q.output_rate(48_000).unwrap()))
            .unwrap();
        assert_eq!(running, RateRatio::integer(4));
        let open = audivane_io_alsa::RatioOpen {
            ratio: running,
            refused: ladder
                .iter()
                .copied()
                .take_while(|&q| q != running)
                .collect(),
            transient: vec![],
        };
        let key = negotiated_key(desired, SinkChoice::Dsp, 48_000, None, &open);
        let dsp = Some(SinkChoice::Dsp);
        assert_eq!(
            open_landing(&to_driver_config(&manual), 48_000, Some(&key), None),
            Some(RateRatio::integer(5))
        );
        assert!(diff_requires_reopen(
            &cascade,
            &manual,
            48_000,
            Some(&key),
            running,
            dsp
        ));

        // With a probe: 768k only in S16 on an S32 config. Turning A/B off
        // changes the buildable pairs, but the plan stays at x1.
        use audivane_protocol::ipc::FormatCapability;
        let mut caps = dac(&[44_100, 768_000]);
        caps.rates[1].formats = vec![FormatCapability {
            format: SampleFormat::S16,
            sbits: Some(16),
        }];
        let mut single = test_cfg();
        single.formats = vec![SampleFormat::S32];
        single.chain.upsampler = audivane_dsp_core::resample::ManualConfig {
            target: RateTarget::MaxRateAny(768_000),
            taps: 400,
            taps_unit: audivane_protocol::TapsUnit::Input,
            ..Default::default()
        }
        .into_upsampler_config()
        .unwrap();
        let mut ab = single.clone();
        ab.ab = Some(audivane_protocol::ipc::AbConfig {
            level: audivane_protocol::ipc::LevelMatch::Off,
            headroom: Default::default(),
            ..AbConfig::chain(single.chain.clone())
        });
        let drv_ab = to_driver_config(&ab);
        assert!(
            drv_ab
                .dsp_buildable
                .differs_from(to_driver_config(&single).dsp_buildable, 44_100)
        );
        let rates = audivane_io_pw::lifecycle::DeviceRates::playable(Some(&caps), &ab.formats);
        let key = negotiated_key(
            drv_ab.desired_dsp(0),
            SinkChoice::Dsp,
            44_100,
            rates,
            &audivane_io_alsa::RatioOpen::at(RateRatio::ONE),
        );
        let one = RateRatio::ONE;
        assert!(!diff_requires_reopen(
            &ab,
            &single,
            44_100,
            Some(&key),
            one,
            dsp
        ));
        // Playable in S32 too: the same edit reopens at 768k.
        caps.rates[1].formats.push(FormatCapability {
            format: SampleFormat::S32,
            sbits: Some(32),
        });
        let rates = audivane_io_pw::lifecycle::DeviceRates::playable(Some(&caps), &ab.formats);
        let key = negotiated_key(
            drv_ab.desired_dsp(0),
            SinkChoice::Dsp,
            44_100,
            rates,
            &audivane_io_alsa::RatioOpen::at(RateRatio::ONE),
        );
        assert!(diff_requires_reopen(
            &ab,
            &single,
            44_100,
            Some(&key),
            one,
            dsp
        ));
    }

    #[test]
    fn dsp_on_off_reopens_when_the_running_ratio_is_not_the_fresh_pick() {
        // A {44.1k, 48k} DAC, source 44.1k, any family up to 48k: DSP on
        // opens 48k (160/147); the plain resolution says x1 either way.
        let caps = dac(&[44_100, 48_000]);
        let cross = RateRatio::new(160, 147);
        let mut on = test_cfg();
        on.chain.upsampler = audivane_dsp_core::resample::ManualConfig {
            target: RateTarget::MaxRateAny(48_000),
            preset: Some(audivane_protocol::FilterPreset::GaussianLong),
            ..Default::default()
        }
        .into_upsampler_config()
        .unwrap();
        let mut off = on.clone();
        off.dsp_enabled = false;
        assert_eq!(desired_ratio_for(&on, 44_100), RateRatio::ONE);
        assert_eq!(
            open_landing(&to_driver_config(&on), 44_100, Some(&probed(&caps)), None),
            Some(cross)
        );
        let dsp = Some(SinkChoice::Dsp);
        // Off at 160/147: bypass cannot play it.
        assert!(diff_requires_reopen(
            &on,
            &off,
            44_100,
            Some(&probed(&caps)),
            cross,
            dsp
        ));
        assert!(diff_requires_reopen(
            &on,
            &off,
            44_100,
            Some(&probed(&caps)),
            cross,
            None
        ));
        // On at x1: back to the device-list pick.
        let one = RateRatio::ONE;
        assert!(diff_requires_reopen(
            &off,
            &on,
            44_100,
            Some(&probed(&caps)),
            one,
            dsp
        ));
        // Already where a fresh open would be: no gap.
        assert!(!diff_requires_reopen(
            &on,
            &off,
            44_100,
            Some(&probed(&caps)),
            one,
            dsp
        ));
        assert!(!diff_requires_reopen(
            &off,
            &on,
            44_100,
            Some(&probed(&caps)),
            cross,
            dsp
        ));
        // The Direct sink plays x1 with DSP on or off.
        let direct = Some(SinkChoice::Direct);
        assert!(!diff_requires_reopen(
            &on,
            &off,
            44_100,
            Some(&probed(&caps)),
            one,
            direct
        ));
        assert!(!diff_requires_reopen(
            &off,
            &on,
            44_100,
            Some(&probed(&caps)),
            one,
            direct
        ));
        // 176.4k on a {176.4k, 192k} DAC with a 192k cap: the same.
        let caps = dac(&[176_400, 192_000]);
        on.chain.upsampler.target = RateTarget::MaxRateAny(192_000);
        off.chain.upsampler.target = RateTarget::MaxRateAny(192_000);
        assert_eq!(
            open_landing(&to_driver_config(&on), 176_400, Some(&probed(&caps)), None),
            Some(cross)
        );
        assert!(diff_requires_reopen(
            &on,
            &off,
            176_400,
            Some(&probed(&caps)),
            cross,
            dsp
        ));
        assert!(diff_requires_reopen(
            &off,
            &on,
            176_400,
            Some(&probed(&caps)),
            one,
            dsp
        ));
    }

    #[test]
    fn buildable_pair_changes_reopen_when_they_change_the_pick() {
        use audivane_protocol::FilterPreset as P;
        let caps = dac(&[44_100, 48_000, 96_000, 192_000]);
        let x4 = RateRatio::from_rates(44_100, 192_000);
        let mut gauss = test_cfg();
        gauss.chain.upsampler = UpsamplerConfig {
            target: RateTarget::MaxRateAny(768_000),
            preset: Some(P::GaussianLong.name().to_string()),
            ..Default::default()
        };
        let mut cascade = gauss.clone();
        cascade.chain.upsampler.preset = Some(P::CascadeFir.name().to_string());
        let pick = |cfg: &EngineConfig| {
            open_landing(&to_driver_config(cfg), 44_100, Some(&probed(&caps)), None)
        };
        assert_eq!(pick(&gauss), Some(x4));
        assert_eq!(pick(&cascade), Some(RateRatio::ONE));
        // Playing 640/147 with gaussian_long, a switch to cascade_fir (no
        // fractional ratios) reopens, with or without a probe.
        assert!(diff_requires_reopen(
            &gauss,
            &cascade,
            44_100,
            Some(&probed(&caps)),
            x4,
            None
        ));
        assert!(diff_requires_reopen(
            &gauss, &cascade, 44_100, None, x4, None
        ));
        // And back: the better cross-family rate is picked up again.
        assert!(diff_requires_reopen(
            &cascade,
            &gauss,
            44_100,
            Some(&probed(&caps)),
            RateRatio::ONE,
            None
        ));
        // A change that leaves the buildable pairs alone does not.
        let mut eq = gauss.clone();
        eq.chain.eq = Some(eq_profile(-3.0, 0.0));
        assert!(!diff_requires_reopen(
            &gauss,
            &eq,
            44_100,
            Some(&probed(&caps)),
            x4,
            None
        ));
        // Nor one whose new pairs still pick the running ratio.
        let mut short = gauss.clone();
        short.chain.upsampler.preset = Some(P::GaussianShort.name().to_string());
        assert_eq!(pick(&short), Some(x4));
        assert!(!diff_requires_reopen(
            &gauss,
            &short,
            44_100,
            Some(&probed(&caps)),
            x4,
            None
        ));

        // A/B on doubles the pipelines: a manual design that fits one
        // chain but not two reopens to a ratio it fits.
        let mut manual = gauss.clone();
        manual.chain.upsampler = audivane_dsp_core::resample::ManualConfig {
            target: RateTarget::MaxRateAny(768_000),
            taps: 1500,
            taps_unit: audivane_protocol::TapsUnit::Input,
            ..Default::default()
        }
        .into_upsampler_config()
        .unwrap();
        let mut with_ab = manual.clone();
        with_ab.ab = Some(AbConfig {
            level: LevelMatch::Off,
            headroom: Default::default(),
            ..AbConfig::chain(manual.chain.clone())
        });
        assert_eq!(pick(&manual), Some(x4));
        assert_ne!(pick(&with_ab), Some(x4));
        assert!(diff_requires_reopen(
            &manual,
            &with_ab,
            44_100,
            Some(&probed(&caps)),
            x4,
            None
        ));
        // Without the reopen the build refuses what the 4 pipelines cannot run.
        let mut cache = IrCache::new();
        assert!(build_active_chain(&with_ab, &mut cache, 44_100, x4, AbSide::A).is_err());
        assert!(build_active_chain(&manual, &mut cache, 44_100, x4, AbSide::A).is_ok());
    }

    /// The engine's own path for x32/x64: the driver config, a probe of a
    /// DAC that reports up to 3.072 MHz, where an open lands, and the chain
    /// the engine builds there. A DAC that stops at 768 kHz lands where it
    /// always did.
    #[test]
    fn x64_targets_land_and_build_on_a_3072k_dac() {
        let full = audivane_io_pw::engine_driver::engine_probe_rates(MAX_DSP_RATIO as u32);
        let dac_3m = dac(&full);
        let up_to_768k: Vec<u32> = full.iter().copied().filter(|&r| r <= 768_000).collect();
        let dac_768k = dac(&up_to_768k);
        let land = |cfg: &EngineConfig, graph: u32, caps| {
            let drv = to_driver_config(cfg);
            assert_eq!(drv.max_dsp_ratio, 64);
            let rates = audivane_io_pw::lifecycle::DeviceRates::playable(Some(caps), &cfg.formats);
            let key = audivane_io_pw::lifecycle::negotiated_key(
                drv.desired_dsp(0),
                SinkChoice::Dsp,
                graph,
                rates,
                &audivane_io_alsa::RatioOpen::at(RateRatio::ONE),
            );
            open_landing(&drv, graph, Some(&key), Some(SinkChoice::Dsp)).unwrap()
        };
        let mut cfg = test_cfg();
        cfg.chain.upsampler = audivane_dsp_core::resample::ManualConfig {
            target: RateTarget::MaxRate(audivane_protocol::MAX_OUTPUT_RATE),
            taps: 8_192,
            ..Default::default()
        }
        .into_upsampler_config()
        .unwrap();
        crate::control::config::validate(&cfg).unwrap();
        let mut cache = IrCache::new();
        for (graph, ratio) in [(44_100, 64), (48_000, 64), (96_000, 32), (192_000, 16)] {
            let landed = land(&cfg, graph, &dac_3m);
            assert_eq!(landed, RateRatio::integer(ratio), "{graph}");
            let (chain, _, _) = build_active_chain(&cfg, &mut cache, graph, landed, AbSide::A)
                .unwrap_or_else(|e| panic!("{graph} x{ratio}: {e}"));
            assert_eq!(chain.ratio(), landed);
        }
        for (graph, ratio) in [(44_100, 16), (48_000, 16), (96_000, 8), (768_000, 1)] {
            assert_eq!(
                land(&cfg, graph, &dac_768k),
                RateRatio::integer(ratio),
                "{graph} on a 768 kHz DAC"
            );
        }
        // A preset at x64 and the either-family target.
        cfg.chain.upsampler = audivane_dsp_core::resample::ManualConfig {
            target: RateTarget::MaxRateAny(audivane_protocol::MAX_OUTPUT_RATE),
            preset: Some(audivane_protocol::FilterPreset::CascadeFir),
            ..Default::default()
        }
        .into_upsampler_config()
        .unwrap();
        crate::control::config::validate(&cfg).unwrap();
        let landed = land(&cfg, 44_100, &dac_3m);
        assert_eq!(landed, RateRatio::integer(64));
        let (chain, _, _) =
            build_active_chain(&cfg, &mut cache, 44_100, landed, AbSide::A).unwrap();
        assert_eq!(chain.ratio(), landed);
        assert_eq!(land(&cfg, 44_100, &dac_768k), RateRatio::integer(16));
    }

    #[test]
    fn ab_mask_and_fallback_cover_both_sides() {
        use audivane_protocol::FilterPreset as P;
        let ir_path = temp_ir_path("ab_both_sides");
        write_test_ir(&ir_path, 44_100);
        let mut cfg = test_cfg();
        cfg.chain.eq = Some(eq_profile(-3.0, 0.0));
        cfg.chain.convolution = vec![conv_cfg(&ir_path)];
        cfg.chain.upsampler = UpsamplerConfig {
            target: RateTarget::MaxRateAny(768_000),
            preset: Some(P::GaussianLong.name().to_string()),
            ..Default::default()
        };
        let mut b = cfg.chain.clone();
        b.upsampler.preset = Some(P::CascadeFir.name().to_string());
        cfg.ab = Some(AbConfig {
            level: Default::default(),
            headroom: Default::default(),
            ..AbConfig::chain(b)
        });
        crate::control::config::validate(&cfg).unwrap();

        // Chain A alone could run 640/147; B (integer-only) cannot, so the
        // ladder on a 48k-family DAC never offers it.
        let mask = crate::control::config::engine_buildable_rate_pairs(&cfg);
        let a_only = crate::control::config::buildable_rate_pairs(&cfg.chain.upsampler, 4);
        assert!(a_only.allows(44_100, 192_000));
        assert!(!mask.allows(44_100, 192_000));
        assert_eq!(to_driver_config(&cfg).dsp_buildable, mask);
        let dac = [44_100, 48_000, 96_000, 192_000];
        let ladder =
            audivane_io_pw::lifecycle::max_rate_any_ladder(44_100, 768_000, &dac, 16, mask);
        assert_eq!(ladder, vec![RateRatio::ONE]);

        // If the device is opened there anyway, the build refuses it and
        // the fallback replaces the upsampler on both sides, keeping EQ,
        // convolution and A/B.
        let ratio = RateRatio::from_rates(44_100, 192_000);
        let mut cache = IrCache::new();
        let err = build_active_chain(&cfg, &mut cache, 44_100, ratio, AbSide::A)
            .err()
            .unwrap();
        assert!(err.to_string().contains("ab.chain_b"), "{err}");
        let (chain, _, _, given_up) =
            build_fallback_chain(&cfg, &mut cache, 44_100, ratio, AbSide::A).unwrap();
        assert_eq!(
            given_up,
            "the upsampler replaced by a short safe design (EQ, convolution and A/B kept)"
        );
        let ActiveChain::Ab(ab) = chain else {
            panic!("expected an A/B chain");
        };
        assert_eq!(ab.ratio(), ratio);
        // Cost: a manual B design that fits two pipelines but not the four
        // A/B runs is kept off the ladder, and refused by the build.
        let mut heavy = cfg.clone();
        heavy.ab.as_mut().unwrap().chain_b.upsampler = audivane_dsp_core::resample::ManualConfig {
            target: RateTarget::MaxRateAny(768_000),
            taps: 1500,
            taps_unit: audivane_protocol::TapsUnit::Input,
            ..Default::default()
        }
        .into_upsampler_config()
        .unwrap();
        crate::control::config::validate(&heavy).unwrap();
        let b_up = &heavy.ab.as_ref().unwrap().chain_b.upsampler;
        assert!(crate::control::config::check_rational_at("b", b_up, 44_100, ratio, 2).is_ok());
        let mask = crate::control::config::engine_buildable_rate_pairs(&heavy);
        assert!(!mask.allows(44_100, 192_000));
        assert!(mask.allows(44_100, 48_000));
        let err = build_active_chain(&heavy, &mut cache, 44_100, ratio, AbSide::A)
            .err()
            .unwrap();
        assert!(err.to_string().contains("4 pipelines"), "{err}");
        let _ = std::fs::remove_dir_all(ir_path.parent().unwrap());
    }

    #[test]
    fn match_output_budgets_build_at_rational_ratios_on_both_sides() {
        use audivane_protocol::{DitherKind, FilterErrorBudget};
        for (rate, out) in [(44_100, 48_000), (48_000, 44_100)] {
            let ratio = RateRatio::from_rates(rate, out);
            let mut cfg = test_cfg();
            cfg.rate.allowed_rates = vec![44_100, 48_000];
            cfg.rate.default_rate = rate;
            cfg.chain.output = OutputConfig {
                bits: 24,
                dither: DitherKind::Tpdf,
                headroom_db: 0.0,
            };
            cfg.chain.upsampler = audivane_dsp_core::resample::ManualConfig {
                target: RateTarget::Rate(out),
                auto_taps: Some(FilterErrorBudget::MatchOutput),
                ..Default::default()
            }
            .into_upsampler_config()
            .unwrap();
            let mut b = cfg.chain.clone();
            b.upsampler = audivane_dsp_core::resample::ManualConfig {
                target: RateTarget::Rate(out),
                window: audivane_protocol::WindowKind::Gaussian,
                auto_taps: Some(FilterErrorBudget::MatchOutput),
                ..Default::default()
            }
            .into_upsampler_config()
            .unwrap();
            cfg.ab = Some(AbConfig {
                level: LevelMatch::Off,
                headroom: Default::default(),
                ..AbConfig::chain(b)
            });
            crate::control::config::validate(&cfg).unwrap();
            // The mask and the build agree: both sides resolve the budget
            // against their own (S24-capped) output.
            let capped = cap_dither_bits(&cfg, Some(SampleFormat::S24));
            assert!(crate::control::config::engine_buildable_rate_pairs(&cfg).allows(rate, out));
            rational_upsamplers_fit(&capped, rate, ratio).unwrap();
            let mut cache = IrCache::new();
            assert!(matches!(
                build_active_chain(&capped, &mut cache, rate, ratio, AbSide::A),
                Ok((ActiveChain::Ab(_), _, _))
            ));

            // An edit from a dB budget to match_output while the ratio
            // plays is applied without a fallback or an error.
            let mut h = BuilderProcHarness::new(test_cfg());
            let mut db = cfg.clone();
            db.chain.upsampler = audivane_dsp_core::resample::ManualConfig {
                target: RateTarget::Rate(out),
                auto_taps: Some(FilterErrorBudget::Db(120.0)),
                ..Default::default()
            }
            .into_upsampler_config()
            .unwrap();
            db.ab.as_mut().unwrap().chain_b.upsampler = audivane_dsp_core::resample::ManualConfig {
                target: RateTarget::Rate(out),
                window: audivane_protocol::WindowKind::Gaussian,
                auto_taps: Some(FilterErrorBudget::Db(120.0)),
                ..Default::default()
            }
            .into_upsampler_config()
            .unwrap();
            h.state
                .apply_config(&db, &h.mirror, EngineState::Ready, rate, None, None)
                .unwrap();
            h.state.last_dsp_ratio = ratio;
            h.state
                .apply_config(&cfg, &h.mirror, EngineState::Ready, rate, None, None)
                .unwrap();
            let neg = h.negotiate(rate, ratio, SinkChoice::Dsp, SampleFormat::S24);
            assert!(neg.error.is_none(), "{rate} -> {out}: {:?}", neg.error);
        }
    }

    #[test]
    fn last_resort_fallback_says_what_it_dropped() {
        use audivane_protocol::FilterPreset as P;
        // An A/B pair that cannot be built together whatever the upsampler
        // (different output headroom; validation keeps this from a real
        // config, the fallback must still cope): step 3 plays chain A only.
        let mut cfg = test_cfg();
        cfg.chain.eq = Some(eq_profile(-3.0, 0.0));
        cfg.chain.upsampler = UpsamplerConfig {
            target: RateTarget::MaxRateAny(768_000),
            preset: Some(P::CascadeFir.name().to_string()),
            ..Default::default()
        };
        // The limiter (a safety stage) and crossfeed stay in the last resort.
        cfg.chain.limiter = audivane_protocol::LimiterConfig {
            mode: audivane_protocol::LimiterMode::Limit,
            ..Default::default()
        };
        cfg.chain.crossfeed = Some(audivane_protocol::CrossfeedConfig {
            enabled: true,
            ..Default::default()
        });
        let mut b = cfg.chain.clone();
        b.output.headroom_db += 1.0;
        cfg.ab = Some(AbConfig {
            level: Default::default(),
            headroom: Default::default(),
            ..AbConfig::chain(b)
        });
        let mut cache = IrCache::new();
        let ratio = RateRatio::from_rates(44_100, 192_000);
        let (chain, _, _, given_up) =
            build_fallback_chain(&cfg, &mut cache, 44_100, ratio, AbSide::A).unwrap();
        assert!(matches!(chain, ActiveChain::Single(_)));
        assert!(given_up.contains("A/B off"), "{given_up}");
        assert!(given_up.contains("upsampler replaced"), "{given_up}");
        // At an integer ratio the upsampler is kept and the message says
        // only what was dropped.
        let (chain, _, _, given_up) =
            build_fallback_chain(&cfg, &mut cache, 44_100, RateRatio::integer(4), AbSide::A)
                .unwrap();
        let ActiveChain::Single(chain) = chain else {
            panic!("expected a single chain");
        };
        assert_eq!(
            chain.config().upsampler.preset.as_deref(),
            Some(P::CascadeFir.name())
        );
        assert!(chain.limiter().is_some());
        assert!(chain.config().crossfeed.is_some());
        assert_eq!(
            given_up,
            "EQ and convolution disabled, A/B off (chain A only)"
        );
    }

    #[test]
    fn rational_fallback_is_flat_and_fits_at_every_pair() {
        use crate::control::config::{check_rational_at, rational_pipelines};
        // It builds wherever the device can be opened, A/B included.
        for &i in audivane_protocol::ALLOWED_RATES {
            for o in audivane_io_pw::lifecycle::MASK_OUTPUT_RATES {
                let r = RateRatio::from_rates(i, o);
                if r.is_integer() || r.up > 4096 || r.down > 4096 {
                    continue;
                }
                for ab in [false, true] {
                    let p = rational_pipelines(ab);
                    let up = rational_fallback_upsampler(i, r, p);
                    check_rational_at("fallback", &up, i, r, p)
                        .unwrap_or_else(|e| panic!("{i} -> {o}: {e}"));
                }
            }
        }
        // Flat to 0.9 of the lower Nyquist, decimating or not, and
        // stopped there.
        for (i, o) in [(44_100, 48_000), (96_000, 48_000), (192_000, 44_100)] {
            let r = RateRatio::from_rates(i, o);
            let up = rational_fallback_upsampler(i, r, rational_pipelines(true));
            let named = audivane_dsp_core::design::Named::of(&up).unwrap();
            let h = audivane_dsp_core::design::offline_kernel(&named.design, i, r)
                .unwrap()
                .taps;
            let fs = f64::from(i) * f64::from(r.up);
            let db = |f: f64| {
                let (mut re, mut im) = (0.0, 0.0);
                for (n, &v) in h.iter().enumerate() {
                    let w = std::f64::consts::TAU * f * n as f64 / fs;
                    re += v * w.cos();
                    im -= v * w.sin();
                }
                20.0 * (re.hypot(im) / f64::from(r.up)).log10()
            };
            let band = f64::from(i.min(o)) / 2.0;
            for frac in [0.1, 0.5, 0.8, 0.9] {
                let g = db(frac * band);
                assert!(g.abs() < 0.1, "{i} -> {o}: {g:.3} dB at {frac} of the band");
            }
            assert!(
                db(band) < -120.0,
                "{i} -> {o}: {:.1} dB at the edge",
                db(band)
            );
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
    fn valid_config_without_reopen_clears_fallback_chain_error() {
        let mut h = BuilderProcHarness::new(test_cfg());
        h.mirror
            .set_chain_error("failed to rebuild DSP chain; playing fallback chain");

        let mut cfg = test_cfg();
        cfg.chain.eq = Some(eq_profile(-3.0, 0.0));
        let reopen = h
            .state
            .apply_config(&cfg, &h.mirror, EngineState::Ready, 48_000, None, None)
            .unwrap();

        assert!(!reopen, "a chain-only change must not reopen the device");
        assert!(
            h.mirror.chain_error().is_none(),
            "a successful replacement chain must clear the fallback error"
        );
        assert_eq!(h.state.config_generation, 1);
    }

    #[test]
    fn failed_same_ratio_rebuild_plays_fallback_chain_and_reports_error() {
        let ir_path = temp_ir_path("same_ratio_fallback");
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
        assert!(
            h.state
                .apply_config(&cfg, &h.mirror, EngineState::Ready, 48_000, None, None)
                .unwrap()
        );
        let first = h.negotiate(
            48_000,
            RateRatio::integer(4),
            SinkChoice::Dsp,
            SampleFormat::S32,
        );
        assert!(first.error.is_none());
        audivane_io_pw::EngineAudioProcessor::poll_updates(
            &mut h.proc,
            SinkChoice::Dsp,
            48_000,
            RateRatio::integer(4),
            false,
        );
        assert_eq!(h.active_gen(), h.state.config_generation);
        while h.recycle_rx.pop().is_ok() {}

        // A same-rate device re-acquire reopens at 48 kHz x4 after the IR vanished.
        std::fs::remove_file(&ir_path).unwrap();
        let neg = h.negotiate(
            48_000,
            RateRatio::integer(4),
            SinkChoice::Dsp,
            SampleFormat::S32,
        );
        assert!(!neg.fatal);
        let err = neg
            .error
            .clone()
            .expect("same-ratio rebuild failure must be surfaced in status");
        assert!(err.contains("IR"), "unexpected error: {err}");
        let seq = neg
            .required_chain_seq
            .expect("fallback chain must be published for the resume gate");

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
        let db = h.measure_1khz_peak_db(4);
        assert!(
            (db - expected_fallback_db()).abs() < 0.2,
            "expected {:.1} dBFS fallback, got {db:.3}",
            expected_fallback_db()
        );

        let _ = std::fs::remove_dir_all(ir_path.parent().unwrap());
    }

    #[test]
    fn deferred_publication_keeps_later_ab_selection() {
        let mut ab_cfg = test_cfg();
        ab_cfg.chain.eq = Some(eq_profile(-1.0, 0.0));
        ab_cfg.ab = Some(AbConfig {
            level: LevelMatch::Off,
            headroom: Default::default(),
            ..AbConfig::chain(ChainConfig {
                eq: Some(eq_profile(-9.0, 0.0)),
                ..ChainConfig::neutral()
            })
        });
        let mut h = BuilderProcHarness::new(ab_cfg.clone());

        // Pause RT swap drain and saturate the 32-slot swap ring with side-A chains.
        h.proc.set_swap_drain_paused(true);
        for i in 1..=32 {
            let mut filler = ab_cfg.clone();
            filler.chain.eq = Some(eq_profile(-1.0, -((i as f64) * 0.01)));
            assert!(
                !h.state
                    .apply_config(&filler, &h.mirror, EngineState::Ready, 48_000, None, None)
                    .unwrap()
            );
        }
        assert_eq!(h.state.built_generation, 32);

        // A newer config arrives while the ring is full: its publication is deferred.
        let mut latest = ab_cfg.clone();
        latest.chain.eq = Some(eq_profile(-2.0, 0.0));
        assert!(
            !h.state
                .apply_config(&latest, &h.mirror, EngineState::Ready, 48_000, None, None)
                .unwrap()
        );
        assert!(h.state.pending_publication.is_some());

        // The user selects B while the deferred publication is still pending.
        h.state.ab_side = AbSide::B;
        h.mirror.set_ab(true, AbSide::B, None);
        h.state
            .push_cmd(
                RtDspCmd::AbSelect {
                    side: AbSide::B,
                    crossfade_ms: 0.0,
                },
                None,
            )
            .unwrap();

        // Reaper retries the deferred publication; the chain must carry B and
        // the published status must not restore the captured A selection.
        h.proc.set_swap_drain_paused(false);
        for _ in 0..16 {
            audivane_io_pw::EngineAudioProcessor::poll_updates(
                &mut h.proc,
                SinkChoice::Dsp,
                48_000,
                RateRatio::integer(1),
                false,
            );
            while h.recycle_rx.pop().is_ok() {}
            h.state.flush_pending_swap(&h.mirror, None);
        }
        audivane_io_pw::EngineAudioProcessor::poll_updates(
            &mut h.proc,
            SinkChoice::Dsp,
            48_000,
            RateRatio::integer(1),
            false,
        );
        while h.recycle_rx.pop().is_ok() {}

        assert!(h.state.pending_publication.is_none());
        assert_eq!(h.state.built_generation, h.state.config_generation);
        assert_eq!(
            h.mirror.ab_status().expect("A/B enabled").current,
            AbSide::B,
            "deferred publication must not undo the later B selection"
        );

        // Installed chain must be side B: -9 dB preamp instead of A's -2 dB.
        let db = h.measure_1khz_peak_db(1);
        assert!(
            (db - (-15.0)).abs() < 0.3,
            "expected side B (-15.0 dBFS), got {db:.3}"
        );
    }

    #[test]
    fn multi_ir_convolution_combines_enabled_irs_and_applies_gain() {
        let ir1 = temp_ir_path("multi_ir_1");
        let ir2 = temp_ir_path("multi_ir_2");
        let ir_disabled = temp_ir_path("multi_ir_off");
        write_test_ir(&ir1, 48_000);
        write_test_ir(&ir2, 48_000);
        write_test_ir(&ir_disabled, 48_000);

        let mut h = BuilderProcHarness::new(test_cfg());
        let mut cfg = test_cfg();
        cfg.chain.convolution = vec![
            ConvolutionConfig {
                name: Some("First".into()),
                enabled: true,
                ir_path: ir1.clone(),
                gain_db: -3.0,
            },
            ConvolutionConfig {
                name: Some("Disabled".into()),
                enabled: false,
                ir_path: ir_disabled.clone(),
                gain_db: -20.0,
            },
            ConvolutionConfig {
                name: Some("Second".into()),
                enabled: true,
                ir_path: ir2.clone(),
                gain_db: -2.0,
            },
        ];
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

        // -6 dBFS sine through two unit impulses with -3 dB and -2 dB gains -> -11.0 dBFS.
        let db = h.measure_1khz_peak_db(1);
        assert!(
            (db - (-11.0)).abs() < 0.2,
            "expected -11.0 dBFS from combined enabled IRs, got {db:.3}"
        );

        let _ = std::fs::remove_dir_all(ir1.parent().unwrap());
        let _ = std::fs::remove_dir_all(ir2.parent().unwrap());
        let _ = std::fs::remove_dir_all(ir_disabled.parent().unwrap());
    }

    #[test]
    fn combined_convolution_length_cap_is_enforced() {
        use hound::{SampleFormat as WavFormat, WavSpec, WavWriter};
        let ir_path = temp_ir_path("ir_cap");
        let spec = WavSpec {
            channels: 1,
            sample_rate: 48_000,
            bits_per_sample: 16,
            sample_format: WavFormat::Int,
        };
        let mut w = WavWriter::create(&ir_path, spec).unwrap();
        for _ in 0..1_000_001 {
            w.write_sample(0_i16).unwrap();
        }
        w.finalize().unwrap();

        let mut h = BuilderProcHarness::new(test_cfg());
        let mut cfg = test_cfg();
        // 4 x 1,000,001 -> combined length = 4,000,004 - 3 = 4,000,001 > 4,000,000.
        cfg.chain.convolution = vec![conv_cfg(&ir_path); 4];
        let err = h
            .state
            .apply_config(&cfg, &h.mirror, EngineState::Ready, 48_000, None, None)
            .unwrap_err();
        assert_eq!(err.code, RpcError::INVALID_PARAMS);
        assert!(
            err.message.contains("4000000") || err.message.contains("too long"),
            "unexpected error: {}",
            err.message
        );

        let _ = std::fs::remove_dir_all(ir_path.parent().unwrap());
    }

    /// A measured lock (a level-only edit to a fixed offset) reaches the
    /// running pair as a 50 ms ramp, not a rebuild whose cold stages would
    /// be heard.
    #[test]
    fn ab_level_only_edit_ramps_the_running_pair_without_a_rebuild() {
        use audivane_protocol::ipc::{AbConfig, AbHeadroom, LevelMatch};
        let mut cfg = test_cfg();
        cfg.ab = Some(AbConfig {
            level: LevelMatch::default(),
            headroom: AbHeadroom::Fixed { db: 0.0 },
            ..AbConfig::chain(ChainConfig {
                eq: Some(EqProfile {
                    name: "quiet".into(),
                    preamp_db: -6.0,
                    ..Default::default()
                }),
                ..cfg.chain.clone()
            })
        });
        let mut h = BuilderProcHarness::new(cfg.clone());
        h.negotiate(48_000, RateRatio::ONE, SinkChoice::Dsp, SampleFormat::S32);
        // Analytic: B is 6 dB quieter, so A (audible) is cut by 6 dB.
        let before = h.measure_1khz_peak_db(1);
        assert!((before + 12.0).abs() < 0.3, "{before}");
        let seq = h.state.next_swap_seq;

        let mut locked = cfg.clone();
        locked.ab.as_mut().unwrap().level = LevelMatch::Measured {
            offset_db: 2.0,
            seconds: 25.0,
        };
        let reopen = h
            .state
            .apply_config(
                &locked,
                &h.mirror,
                EngineState::Playing,
                48_000,
                Some(SinkChoice::Dsp),
                None,
            )
            .unwrap();
        assert!(!reopen);
        assert_eq!(h.state.next_swap_seq, seq, "no rebuild");
        assert_eq!(h.state.built_generation, h.state.config_generation);
        assert_eq!(h.state.cfg, locked, "the level is in the config");
        let gains = h.state.ab_gains.unwrap();
        assert_eq!((gains.offset_db, gains.gain_a_db), (2.0, -2.0));
        assert_eq!(h.mirror.ab_status().unwrap().offset_db, 2.0);
        let after = h.measure_1khz_peak_db(1);
        assert!((after + 8.0).abs() < 0.3, "{after}");

        // An analytic level again needs the pair's terms: a rebuild.
        h.state
            .apply_config(
                &cfg,
                &h.mirror,
                EngineState::Playing,
                48_000,
                Some(SinkChoice::Dsp),
                None,
            )
            .unwrap();
        assert_ne!(h.state.next_swap_seq, seq);
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
    }

    /// The applied level's generation moves whenever what `Status.volume`
    /// reports changes, also without a config change (a staged hardware
    /// raise landing), and stays put when an edit leaves it as it was: the
    /// control plane's level revision counts it.
    #[test]
    fn a_staged_raise_landing_moves_the_applied_generation() {
        let mut cfg = test_cfg();
        cfg.device = "hw:CARD=DAC,DEV=0".into();
        cfg.volume.mode = VolumeMode::Auto;
        cfg.volume.volume_db = -30.0;
        let mut sim = DacSim::new(&cfg, Duration::from_millis(700));
        cfg.volume.mode = VolumeMode::Hybrid;
        cfg.volume.hardware_db = Some(0.0);
        sim.apply(&cfg);
        assert!(sim.h.state.pending_hw.is_some());
        let (interim, staged) = sim.h.mirror.volume_state();
        assert_eq!(interim.hardware_db, Some(-30.0));
        // An unrelated edit inside the window: the report is unchanged.
        cfg.dsp_enabled = !cfg.dsp_enabled;
        sim.apply(&cfg);
        assert_eq!(sim.h.mirror.volume_state(), (interim.clone(), staged));
        sim.advance(Duration::from_secs(2));
        assert!(sim.h.state.pending_hw.is_none());
        let (landed, generation) = sim.h.mirror.volume_state();
        assert_eq!(landed.hardware_db, Some(0.0));
        assert!(generation > staged, "{generation} <= {staged}");
    }

    /// A second change inside a staged raise's window (a volume nudge,
    /// mute, a non-volume edit, another mode change) never writes the
    /// hardware while louder digital audio is still buffered.
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
        let mut settled = std::collections::HashMap::new();
        let mut settled_total = |cfg: &EngineConfig| -> f64 {
            let mut c = cfg.clone();
            c.volume.muted = false;
            c.chain = base.chain.clone();
            let key = format!("{:?}", c.volume);
            *settled
                .entry(key)
                .or_insert_with(|| DacSim::new(&c, latency).total())
        };
        let mut checked = 0;
        for &a in &cases {
            for &b in &cases {
                let mut cfg_a = base.clone();
                set(&mut cfg_a, a);
                let mut cfg_b = cfg_a.clone();
                set(&mut cfg_b, b);
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
                    for dt in [100u64, 400] {
                        let mut sim = DacSim::new(&cfg_a, latency);
                        let total_a = sim.total();
                        let t0 = sim.t();
                        sim.apply(&cfg_b);
                        sim.advance(Duration::from_millis(dt));
                        sim.apply(cfg_c);
                        sim.advance(Duration::from_secs(3));
                        let bound =
                            total_a.max(settled_total(&cfg_b)).max(settled_total(cfg_c)) + 1e-9;
                        let (worst, at) = sim.loudest_since(t0);
                        assert!(
                            worst <= bound,
                            "{a:?} -> {b:?} -> {what} after {dt} ms: {worst} dB at +{:?}, bound {bound}",
                            at - t0
                        );
                        checked += 1;
                    }
                }
            }
        }
        assert_eq!(checked, cases.len() * cases.len() * 5 * 2);
    }

    /// A superseded raise never becomes due sooner, even when the output
    /// latency shrinks in between (say a shorter filter): the audio
    /// rendered at the old gain is still in the buffer.
    #[test]
    fn a_superseded_raise_never_becomes_due_sooner() {
        let mut cfg = test_cfg();
        cfg.device = "hw:CARD=DAC,DEV=0".into();
        cfg.volume.mode = VolumeMode::Auto;
        cfg.volume.volume_db = -30.0;
        let mut sim = DacSim::new(&cfg, Duration::from_millis(700));
        let t0 = sim.t();
        cfg.volume.mode = VolumeMode::Hybrid;
        cfg.volume.hardware_db = Some(0.0);
        sim.apply(&cfg);
        let first_due = sim.h.state.pending_hw.unwrap().due;
        assert!(first_due >= t0 + Duration::from_millis(700));
        sim.advance(Duration::from_millis(100));
        sim.h.state.output_latency = Arc::new(|| Duration::from_millis(50));
        cfg.volume.volume_db = -29.0;
        sim.apply(&cfg);
        assert!(sim.h.state.pending_hw.unwrap().due >= first_due);
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

    /// `auto`/`hardware` below the card's floor: the card sits at its
    /// minimum and the digital part takes the rest. Handing the DAC over
    /// mutes the switch (the next owner would hear the floor, not the
    /// volume), and the next negotiation turns it back on. Above the floor
    /// the card already carries the volume: nothing is written.
    #[test]
    fn handover_below_the_floor_mutes_the_switch_in_hardware_modes() {
        for mode in [VolumeMode::Auto, VolumeMode::Hardware] {
            let mut cfg = test_cfg();
            cfg.device = "hw:CARD=DAC,DEV=0".into();
            cfg.volume.mode = mode;
            cfg.volume.volume_db = -90.0;
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
            h.state.failsafe_mutes = failsafe::MuteStore::default();
            let negotiate = |h: &mut BuilderProcHarness| {
                h.negotiate(48_000, RateRatio::ONE, SinkChoice::Dsp, SampleFormat::S32);
            };
            negotiate(&mut h);
            assert_eq!(h.state.software_db, -26.0, "{mode:?}");
            writes.lock().unwrap().clear();
            h.state.on_release(ReleaseKind::HandOver);
            let w = writes.lock().unwrap().clone();
            assert_eq!(w.last().map(|w| w.1), Some(Some(true)), "{mode:?}: {w:?}");
            assert!(
                w.iter().all(|&(db, _)| db <= -64.0),
                "{mode:?}: raised {w:?}"
            );
            assert_eq!(h.state.failsafe_mutes.entries().len(), 1, "{mode:?}");
            negotiate(&mut h);
            assert_eq!(writes.lock().unwrap().last().unwrap().1, Some(false));
            assert!(h.state.failsafe_mutes.entries().is_empty(), "{mode:?}");
            // Above the floor: the card is at the volume, nothing to do.
            cfg.volume.volume_db = -30.0;
            h.state
                .apply_config(&cfg, &h.mirror, EngineState::Ready, 48_000, None, None)
                .unwrap();
            writes.lock().unwrap().clear();
            h.state.on_release(ReleaseKind::HandOver);
            assert!(writes.lock().unwrap().is_empty(), "{mode:?}");
            assert!(h.state.failsafe_mutes.entries().is_empty(), "{mode:?}");
        }
    }

    /// The failsafe records its mute before muting the switch (a crash in
    /// between leaves a record to undo, never an unrecorded mute), and
    /// forgets it when the mute did not take.
    #[test]
    fn the_failsafe_mute_is_recorded_before_the_switch_is_muted() {
        let dir = std::env::temp_dir().join(format!(
            "audivane-mute-order-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let path = dir.join("failsafe-muted");
        let _ = std::fs::remove_dir_all(&dir);
        for takes in [true, false] {
            let mut cfg = test_cfg();
            cfg.device = "hw:CARD=DAC,DEV=0".into();
            cfg.volume.mode = VolumeMode::Software;
            cfg.volume.hardware_db = Some(0.0);
            cfg.volume.volume_db = -80.0;
            let seen = Arc::new(Mutex::new(None::<bool>));
            let (seen_cb, path_cb) = (Arc::clone(&seen), path.clone());
            let mixer: MixerFn = Arc::new(move |_dev, db, mute: Option<bool>| {
                if mute == Some(true) {
                    let recorded = !failsafe::MuteStore::load(Some(path_cb.clone()))
                        .entries()
                        .is_empty();
                    *seen_cb.lock().unwrap() = Some(recorded);
                }
                MixerApplyOutcome {
                    hardware_volume: true,
                    hardware_mute: mute.is_some() && takes,
                    volume_db: db.clamp(-64.0, 0.0),
                    min_db: -64.0,
                    max_db: 0.0,
                }
            });
            let mut h = BuilderProcHarness::with_mixer(cfg.clone(), mixer);
            h.state.failsafe_mutes = failsafe::MuteStore::load(Some(path.clone()));
            h.negotiate(48_000, RateRatio::ONE, SinkChoice::Dsp, SampleFormat::S32);
            h.state.leave_hardware_safe();
            assert_eq!(
                *seen.lock().unwrap(),
                Some(true),
                "not recorded before the mute"
            );
            let left = failsafe::MuteStore::load(Some(path.clone()));
            assert_eq!(left.entries().len(), usize::from(takes), "takes={takes}");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// A failed unmute (the mixer did not set the switch) keeps the
    /// failsafe's record, so the next open tries again.
    #[test]
    fn a_failed_unmute_keeps_the_record() {
        let mut cfg = test_cfg();
        cfg.device = "hw:CARD=DAC,DEV=0".into();
        cfg.volume.mode = VolumeMode::Software;
        cfg.volume.hardware_db = Some(0.0);
        cfg.volume.volume_db = -30.0;
        let works = Arc::new(AtomicBool::new(false));
        // The card as a hand-over left it: at its floor, switch off.
        let card = Arc::new(Mutex::new((-64.0_f64, false)));
        let (works_cb, card_cb) = (Arc::clone(&works), Arc::clone(&card));
        let mixer: MixerFn = Arc::new(move |_dev, db, mute: Option<bool>| {
            let takes = mute.is_some() && works_cb.load(Ordering::Relaxed);
            let mut c = card_cb.lock().unwrap();
            c.0 = db.clamp(-64.0, 0.0);
            if takes {
                c.1 = mute == Some(false);
            }
            MixerApplyOutcome {
                hardware_volume: true,
                hardware_mute: takes,
                volume_db: c.0,
                min_db: -64.0,
                max_db: 0.0,
            }
        });
        let mut h = BuilderProcHarness::with_mixer(cfg.clone(), mixer);
        let (sw, lv) = (Arc::clone(&card), Arc::clone(&card));
        h.state.switch_state = Arc::new(move |_: &str| Some(sw.lock().unwrap().1));
        h.state.card_state = Arc::new(move |_: &str| {
            let (level_db, on) = *lv.lock().unwrap();
            CardRead::Channels(vec![PlaybackChannel {
                level_db,
                switch_on: Some(on),
            }])
        });
        h.state.failsafe_mutes.record(
            &cfg.device,
            -80.0,
            Some(vec![PlaybackChannel {
                level_db: -64.0,
                switch_on: Some(false),
            }]),
        );
        let negotiate = |h: &mut BuilderProcHarness| {
            h.negotiate(48_000, RateRatio::ONE, SinkChoice::Dsp, SampleFormat::S32);
        };
        negotiate(&mut h);
        assert_eq!(
            h.state.failsafe_mutes.entries().len(),
            1,
            "dropped after a failed unmute"
        );
        works.store(true, Ordering::Relaxed);
        negotiate(&mut h);
        assert!(h.state.failsafe_mutes.entries().is_empty());
        // The same in `software` without a fixed level.
        cfg.volume.hardware_db = None;
        h.state.cfg = cfg.clone();
        h.state.failsafe_mutes.record(
            &cfg.device,
            -80.0,
            Some(vec![PlaybackChannel {
                level_db: -64.0,
                switch_on: Some(false),
            }]),
        );
        *card.lock().unwrap() = (-64.0, false);
        works.store(false, Ordering::Relaxed);
        negotiate(&mut h);
        assert_eq!(h.state.failsafe_mutes.entries().len(), 1);
        works.store(true, Ordering::Relaxed);
        negotiate(&mut h);
        assert!(h.state.failsafe_mutes.entries().is_empty());
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

    /// Once the DAC was let go (ReserveDevice1), stopping the engine never
    /// writes the card, even after an edit made while released: another
    /// program holds it.
    #[test]
    fn stopping_after_a_release_never_writes_the_card() {
        let mut cfg = test_cfg();
        cfg.device = "hw:CARD=DAC,DEV=0".into();
        cfg.volume.mode = VolumeMode::Hybrid;
        cfg.volume.hardware_db = Some(0.0);
        cfg.volume.volume_db = -29.0;
        let mut sim = DacSim::new(&cfg, Duration::from_millis(700));
        sim.h.state.on_release(ReleaseKind::HandOver);
        assert_eq!(sim.hw_now(), -29.0);
        let writes = sim.hw_log.lock().unwrap().len();
        cfg.volume.volume_db = -10.0;
        sim.h
            .state
            .apply_config(&cfg, &sim.h.mirror, EngineState::Idle, 48_000, None, None)
            .unwrap();
        // The driver stopping: its hook, then the engine's own shutdown call.
        sim.h.state.on_release(ReleaseKind::HandOver);
        sim.h.state.leave_hardware_safe();
        assert_eq!(
            sim.hw_log.lock().unwrap().len(),
            writes,
            "card written after release"
        );
    }

    /// Letting go of the DAC is recorded for `--volume-failsafe` (which then
    /// leaves the card alone after a crash); taking it back clears it.
    #[test]
    fn the_release_is_recorded_for_the_crash_failsafe() {
        let path = std::env::temp_dir().join(format!(
            "audivane-released-{}-{:?}/dac-released",
            std::process::id(),
            std::thread::current().id()
        ));
        let mut h = BuilderProcHarness::new(test_cfg());
        h.state.released_marker = Some(path.clone());
        for kind in [ReleaseKind::HandOver, ReleaseKind::Lost] {
            h.state.on_release(kind);
            assert!(failsafe::was_released(Some(&path)), "{kind:?}");
            h.negotiate(48_000, RateRatio::ONE, SinkChoice::Dsp, SampleFormat::S32);
            assert!(!failsafe::was_released(Some(&path)));
        }
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
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

    /// A rate reopen of card A that finishes after a config change moved
    /// the engine to card B is superseded: no mixer write (B is not
    /// reserved yet, and A must not get B's gain split), the DAC is not
    /// taken, and the driver is told not to resume. A late release of A is
    /// not taken as B's. B's own open then writes B.
    #[test]
    fn a_superseded_open_of_the_old_card_writes_no_mixer() {
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
        let (a, b) = ("hw:CARD=CardA,DEV=0", "hw:CARD=CardB,DEV=0");
        let mut cfg = test_cfg();
        cfg.device = a.into();
        cfg.volume.mode = VolumeMode::Hybrid;
        cfg.volume.hardware_db = Some(0.0);
        cfg.volume.volume_db = -29.0;
        let mut h = BuilderProcHarness::with_mixer(cfg.clone(), mixer);
        h.state.failsafe_mutes = failsafe::MuteStore::default();
        let open = |h: &mut BuilderProcHarness, device: &str| {
            h.state.negotiated_on(
                device,
                None,
                48_000,
                RateRatio::ONE,
                SinkChoice::Dsp,
                SampleFormat::S32,
                &h.mirror,
                &h.ab_atomic,
                None,
            )
        };
        assert!(!open(&mut h, a).superseded);
        cfg.device = b.into();
        h.state
            .apply_config(&cfg, &h.mirror, EngineState::Switching, 48_000, None, None)
            .unwrap();
        writes.lock().unwrap().clear();
        let before = h.mirror.volume_status();
        // A's rate reopen, begun before the change, finishes now.
        let n = open(&mut h, a);
        assert!(n.superseded);
        assert_eq!(n.required_chain_seq, None);
        assert!(
            writes.lock().unwrap().is_empty(),
            "{:?}",
            writes.lock().unwrap()
        );
        assert!(!h.state.dac_held);
        assert_eq!(h.mirror.volume_status(), before);
        // The driver closes A and hands it over: still nothing written.
        h.state.on_release_of(ReleaseKind::HandOver, a);
        assert!(writes.lock().unwrap().is_empty());
        // B's open.
        assert!(!open(&mut h, b).superseded);
        let w = writes.lock().unwrap().clone();
        assert!(w.iter().all(|(dev, ..)| dev == b), "{w:?}");
        assert_eq!(w.last().unwrap().1, 0.0);
        // A release of A reported late leaves B alone (and held).
        writes.lock().unwrap().clear();
        h.state.on_release_of(ReleaseKind::HandOver, a);
        h.state.on_release_of(ReleaseKind::Lost, a);
        assert!(writes.lock().unwrap().is_empty());
        assert!(h.state.dac_held);
        // B's own release is B's.
        h.state.on_release_of(ReleaseKind::HandOver, b);
        assert_eq!(
            writes.lock().unwrap().last().map(|w| (w.0.as_str(), w.1)),
            Some((b, -29.0))
        );
    }

    /// A staged raise is not carried out once the driver reports an error
    /// (an RT failure, before any config apply could notice).
    #[test]
    fn a_staged_raise_is_dropped_after_a_driver_error() {
        let mut cfg = test_cfg();
        cfg.device = "hw:CARD=DAC,DEV=0".into();
        cfg.volume.mode = VolumeMode::Auto;
        cfg.volume.volume_db = -30.0;
        let mut sim = DacSim::new(&cfg, Duration::from_millis(700));
        cfg.volume.mode = VolumeMode::Hybrid;
        cfg.volume.hardware_db = Some(0.0);
        sim.apply(&cfg);
        assert!(sim.h.state.pending_hw.is_some());
        let writes = sim.hw_log.lock().unwrap().len();
        sim.h.state.driver_state = Arc::new(|| EngineState::Error);
        sim.advance(Duration::from_secs(2));
        assert_eq!(
            sim.hw_log.lock().unwrap().len(),
            writes,
            "raised after an error"
        );
        assert!(sim.h.state.pending_hw.is_none());
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

    /// While the driver reports an error (it may have let go of the card,
    /// say a failed reconnect), edits do not write the mixer; the next
    /// negotiation does.
    #[test]
    fn no_mixer_writes_while_the_driver_is_in_error() {
        let mut cfg = test_cfg();
        cfg.device = "hw:CARD=DAC,DEV=0".into();
        cfg.volume.mode = VolumeMode::Auto;
        cfg.volume.volume_db = -30.0;
        let mut sim = DacSim::new(&cfg, Duration::from_millis(700));
        let writes = sim.hw_log.lock().unwrap().len();
        cfg.volume.volume_db = -10.0;
        sim.h
            .state
            .apply_config(&cfg, &sim.h.mirror, EngineState::Error, 48_000, None, None)
            .unwrap();
        sim.advance(Duration::from_secs(2));
        assert_eq!(sim.hw_log.lock().unwrap().len(), writes);
        sim.h.state.apply_negotiated(
            48_000,
            RateRatio::ONE,
            SinkChoice::Dsp,
            SampleFormat::S32,
            &sim.h.mirror,
            &sim.h.ab_atomic,
            None,
        );
        assert_eq!(sim.hw_now(), -10.0);
    }

    /// While the DAC is let go, edits never touch the card (another program
    /// may own it); the next negotiation writes the level.
    #[test]
    fn no_mixer_writes_while_the_dac_is_released() {
        let mut cfg = test_cfg();
        cfg.device = "hw:CARD=DAC,DEV=0".into();
        cfg.volume.mode = VolumeMode::Hybrid;
        cfg.volume.hardware_db = Some(0.0);
        cfg.volume.volume_db = -29.0;
        let mut sim = DacSim::new(&cfg, Duration::from_millis(700));
        assert_eq!(sim.hw_now(), 0.0);
        sim.h.state.leave_hardware_safe();
        assert_eq!(sim.hw_now(), -29.0);
        let writes = sim.hw_log.lock().unwrap().len();
        for edit in [-31.0, -10.0, 0.0] {
            cfg.volume.volume_db = edit;
            sim.h
                .state
                .apply_config(&cfg, &sim.h.mirror, EngineState::Idle, 48_000, None, None)
                .unwrap();
            sim.advance(Duration::from_secs(2));
        }
        cfg.volume.mode = VolumeMode::Auto;
        sim.apply(&cfg);
        sim.advance(Duration::from_secs(2));
        assert_eq!(
            sim.hw_log.lock().unwrap().len(),
            writes,
            "card written while released"
        );
        assert!(sim.h.state.pending_hw.is_none());
        // Taking the DAC back writes it.
        cfg.volume.mode = VolumeMode::Hybrid;
        sim.apply(&cfg);
        sim.h.state.apply_negotiated(
            48_000,
            RateRatio::ONE,
            SinkChoice::Dsp,
            SampleFormat::S32,
            &sim.h.mirror,
            &sim.h.ab_atomic,
            None,
        );
        assert_eq!(sim.hw_now(), 0.0);
    }

    /// Letting go of the DAC in hybrid leaves the card at the heard level,
    /// not at its fixed one; taking it back restores the fixed level.
    #[test]
    fn releasing_the_dac_leaves_the_card_at_the_heard_level() {
        let writes = Arc::new(Mutex::new(Vec::<f64>::new()));
        let cb = Arc::clone(&writes);
        let mixer: MixerFn = Arc::new(move |_dev, db, mute: Option<bool>| {
            let applied = db.clamp(-64.0, 0.0);
            cb.lock().unwrap().push(applied);
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
        cfg.volume.mode = VolumeMode::Hybrid;
        cfg.volume.hardware_db = Some(0.0);
        cfg.volume.volume_db = -30.0;
        let mut h = BuilderProcHarness::with_mixer(cfg.clone(), mixer);
        let far = Instant::now() + Duration::from_secs(60);
        h.state
            .apply_config(&cfg, &h.mirror, EngineState::Ready, 48_000, None, None)
            .unwrap();
        h.state.run_pending_hw(far, &h.mirror, None);
        assert_eq!(writes.lock().unwrap().last(), Some(&0.0));
        h.state.leave_hardware_safe();
        assert_eq!(writes.lock().unwrap().last(), Some(&-30.0));
        // Once only (shutdown after a release, Drop).
        let n = writes.lock().unwrap().len();
        h.state.leave_hardware_safe();
        assert_eq!(writes.lock().unwrap().len(), n);
        // Taking the DAC back (a new negotiation) puts the fixed level back.
        h.negotiate(48_000, RateRatio::ONE, SinkChoice::Dsp, SampleFormat::S32);
        assert_eq!(writes.lock().unwrap().last(), Some(&0.0));
        // Auto already carries the volume on the card: nothing to do.
        cfg.volume.mode = VolumeMode::Auto;
        h.state
            .apply_config(&cfg, &h.mirror, EngineState::Ready, 48_000, None, None)
            .unwrap();
        let n = writes.lock().unwrap().len();
        h.state.leave_hardware_safe();
        assert_eq!(writes.lock().unwrap().len(), n);
    }

    /// The software fixed level is written again after another mode moved
    /// the hardware.
    #[test]
    fn software_fixed_level_is_rewritten_after_another_mode() {
        let hw_level = Arc::new(Mutex::new(0.0_f64));
        let hw_cb = Arc::clone(&hw_level);
        let mixer: MixerFn = Arc::new(move |_dev, db, mute: Option<bool>| {
            let applied = db.clamp(-64.0, 0.0);
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
        cfg.volume.mode = VolumeMode::Software;
        cfg.volume.hardware_db = Some(-20.0);
        cfg.volume.volume_db = -30.0;
        let mut h = BuilderProcHarness::with_mixer(cfg.clone(), mixer);
        let far = Instant::now() + Duration::from_secs(60);
        let apply = |h: &mut BuilderProcHarness, cfg: &EngineConfig| {
            h.state
                .apply_config(cfg, &h.mirror, EngineState::Ready, 48_000, None, None)
                .unwrap();
            h.state.run_pending_hw(far, &h.mirror, None);
        };
        apply(&mut h, &cfg);
        assert_eq!(*hw_level.lock().unwrap(), -20.0);
        cfg.volume.mode = VolumeMode::Hybrid;
        cfg.volume.hardware_db = Some(0.0);
        apply(&mut h, &cfg);
        assert_eq!(*hw_level.lock().unwrap(), 0.0);
        cfg.volume.mode = VolumeMode::Software;
        cfg.volume.hardware_db = Some(-20.0);
        apply(&mut h, &cfg);
        assert_eq!(*hw_level.lock().unwrap(), -20.0, "stale fixed level");
        assert_eq!(h.mirror.volume_status().hardware_db, Some(-20.0));

        // Software without a fixed level leaves the hardware where it is and
        // says where that is.
        cfg.volume.mode = VolumeMode::Auto;
        apply(&mut h, &cfg);
        cfg.volume.mode = VolumeMode::Software;
        cfg.volume.hardware_db = None;
        apply(&mut h, &cfg);
        assert_eq!(*hw_level.lock().unwrap(), -30.0);
        let status = h.mirror.volume_status();
        assert_eq!(status.hardware_db, Some(-30.0));
        assert!(!status.hardware);
        assert_eq!(status.software_db, -30.0);
    }

    #[test]
    fn software_mode_uses_digital_volume_on_a_card_with_a_mixer() {
        let calls: MixerLog<(f64, Option<bool>)> = Arc::new(Mutex::new(Vec::new()));
        let calls_cb = Arc::clone(&calls);
        let mixer: MixerFn = Arc::new(move |_dev, vol_db, muted: Option<bool>| {
            calls_cb.lock().unwrap().push((vol_db, muted));
            MixerApplyOutcome {
                hardware_volume: true,
                hardware_mute: muted.is_some(),
                volume_db: vol_db.clamp(-64.0, 0.0),
                min_db: -64.0,
                max_db: 0.0,
            }
        });
        let mut cfg = test_cfg();
        cfg.device = "hw:CARD=DAC,DEV=0".into();
        cfg.volume.volume_db = -14.0;
        cfg.volume.mode = VolumeMode::Software;
        let mut h = BuilderProcHarness::with_mixer(cfg.clone(), mixer);
        let negotiate = |h: &mut BuilderProcHarness| {
            h.negotiate(48_000, RateRatio::ONE, SinkChoice::Dsp, SampleFormat::S32)
        };
        negotiate(&mut h);
        let status = h.mirror.volume_status();
        assert!(!status.hardware);
        assert_eq!(status.software_db, -14.0);
        assert_eq!(status.hardware_db, None);
        assert!(calls.lock().unwrap().is_empty(), "mixer untouched");
        // Digital volume changes samples: DSP is active, not bit-perfect.
        assert!(h.mirror.dsp_has_effect.load(Ordering::Acquire));
        // ...on Direct and bypass too (they get the digital part).
        assert!(h.mirror.digital_gain.load(Ordering::Acquire));
        assert_eq!(h.state.shared_volume_target().volume_db, -14.0);

        // A fixed hardware level is written once per open, not per change.
        let mut fixed = cfg.clone();
        fixed.volume.hardware_db = Some(-6.0);
        h.state
            .apply_config(&fixed, &h.mirror, EngineState::Ready, 48_000, None, None)
            .unwrap();
        fixed.volume.volume_db = -20.0;
        h.state
            .apply_config(&fixed, &h.mirror, EngineState::Ready, 48_000, None, None)
            .unwrap();
        assert_eq!(calls.lock().unwrap().as_slice(), &[(-6.0, None)]);
        assert_eq!(h.mirror.volume_status().hardware_db, Some(-6.0));
        negotiate(&mut h);
        assert_eq!(
            calls.lock().unwrap().len(),
            2,
            "written again after a reopen"
        );

        // The ceiling holds whatever the config asks for.
        fixed.volume.max_volume_db = Some(-30.0);
        h.state
            .apply_config(&fixed, &h.mirror, EngineState::Ready, 48_000, None, None)
            .unwrap();
        let status = h.mirror.volume_status();
        assert_eq!(status.volume_db, -30.0);
        assert_eq!(status.ceiling_db, Some(-30.0));
        assert_eq!(h.state.shared_volume_target().volume_db, -30.0);
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

    /// A start from a stored -7 dB on a card without a mixer (`auto` falls
    /// back to digital): reported and played at -7 dB, from the first block
    /// on, before and after the first negotiation.
    #[test]
    fn start_up_reports_and_plays_the_stored_volume() {
        let mut cfg = test_cfg();
        cfg.volume.mode = VolumeMode::Auto;
        cfg.volume.volume_db = -7.0;
        let mut h = BuilderProcHarness::new(cfg.clone());
        let before = h.mirror.volume_status();
        assert_eq!(before.volume_db, -7.0);
        // Audio already streaming before the builder has published anything.
        let first = one_block_peak_db(&mut h);
        assert!(
            first <= -13.0 + 0.05,
            "first block at {first:.2} dBFS, louder than -7 dB"
        );

        open(&mut h);
        let status = h.mirror.volume_status();
        assert_eq!(status.volume_db, -7.0);
        assert_eq!(status.software_db, -7.0);
        assert!(!status.hardware);
        assert!(!status.hardware_unavailable);
        assert_eq!(status.mode, VolumeMode::Auto);
        let first = one_block_peak_db(&mut h);
        assert!(first <= -13.0 + 0.05, "after open at {first:.2} dBFS");
        let settled = h.measure_1khz_peak_db(1);
        assert!((settled + 13.0).abs() < 0.1, "settled at {settled:.2} dBFS");
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

    /// A two-channel card with an unlinked `-64..=0` dB control (0.5 dB
    /// steps) and a switch per channel. The engine's writes set both.
    struct StereoCard {
        ch: Mutex<[(f64, bool); 2]>,
    }

    impl StereoCard {
        fn new(level_db: f64, switch_on: bool) -> Arc<Self> {
            Arc::new(Self {
                ch: Mutex::new([(level_db, switch_on); 2]),
            })
        }
        fn get(&self) -> [(f64, bool); 2] {
            *self.ch.lock().unwrap()
        }
        fn set(&self, ch: [(f64, bool); 2]) {
            *self.ch.lock().unwrap() = ch;
        }
        fn wired(card: &Arc<Self>, cfg: EngineConfig) -> BuilderProcHarness {
            let w = Arc::clone(card);
            let mixer: MixerFn = Arc::new(move |_dev, db, mute: Option<bool>| {
                let mut ch = w.ch.lock().unwrap();
                for c in ch.iter_mut() {
                    c.0 = SimCard::step(db);
                    if let Some(m) = mute {
                        c.1 = !m;
                    }
                }
                MixerApplyOutcome {
                    hardware_volume: true,
                    hardware_mute: mute.is_some(),
                    volume_db: ch[0].0,
                    min_db: -64.0,
                    max_db: 0.0,
                }
            });
            let mut h = BuilderProcHarness::with_mixer(cfg, mixer);
            let (s, l) = (Arc::clone(card), Arc::clone(card));
            h.state.switch_state = Arc::new(move |_: &str| Some(s.get()[0].1));
            h.state.card_state = Arc::new(move |_: &str| {
                CardRead::Channels(
                    l.get()
                        .iter()
                        .map(|&(level_db, on)| PlaybackChannel {
                            level_db,
                            switch_on: Some(on),
                        })
                        .collect(),
                )
            });
            h.state.failsafe_mutes = failsafe::MuteStore::default();
            h
        }
    }

    /// A hand-over mute recorded below an `auto` card's floor is undone by
    /// `software` (which does not own the switch) only while every channel is
    /// exactly as the hand-over left it (switch off, level as left). Touched
    /// since, the switches are the user's: the record is dropped and the card
    /// stays as it is. Neither the switches nor the quietest level alone tells
    /// (another player unmuted both and raised only the right one to -40 dB;
    /// the user muted both again).
    #[test]
    fn a_handover_mute_is_undone_only_while_the_card_is_as_left() {
        const OFF: bool = false;
        const ON: bool = true;
        // (card when Audivane comes back, switches afterwards)
        let cases = [
            ([(-64.0, OFF), (-64.0, OFF)], [ON, ON]),
            ([(-64.0, OFF), (-40.0, OFF)], [OFF, OFF]),
            ([(-40.0, OFF), (-40.0, OFF)], [OFF, OFF]),
            ([(-64.0, OFF), (-64.0, ON)], [OFF, ON]),
            ([(-64.0, ON), (-64.0, ON)], [ON, ON]),
            ([(-20.0, ON), (-20.0, ON)], [ON, ON]),
        ];
        for (fixed_level, hardware_db) in [(false, None), (true, Some(-64.0))] {
            for (back, want) in cases {
                let card = StereoCard::new(-30.0, true);
                let mut cfg = hardware_cfg(-90.0);
                cfg.volume.mode = VolumeMode::Auto;
                let mut h = StereoCard::wired(&card, cfg.clone());
                open(&mut h);
                h.state.on_release(ReleaseKind::HandOver);
                assert_eq!(card.get(), [(-64.0, OFF); 2]);
                assert_eq!(h.state.failsafe_mutes.entries().len(), 1);
                // Outside Audivane's hands now.
                card.set(back);
                cfg.volume.mode = VolumeMode::Software;
                cfg.volume.hardware_db = hardware_db;
                h.state
                    .apply_config(&cfg, &h.mirror, EngineState::Idle, 48_000, None, None)
                    .unwrap();
                open(&mut h);
                let case = format!("fixed {fixed_level}: back {back:?}");
                let now = card.get();
                assert_eq!([now[0].1, now[1].1], want, "{case}");
                assert!(h.state.failsafe_mutes.entries().is_empty(), "{case}");
                if !fixed_level {
                    assert_eq!(
                        [now[0].0, now[1].0],
                        [back[0].0, back[1].0],
                        "{case}: level written"
                    );
                }
            }
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

    #[test]
    fn hardware_mixer_reapplies_on_reopen_acquire_and_device_switch() {
        let calls: MixerLog<(String, f64, Option<bool>)> = Arc::new(Mutex::new(Vec::new()));
        let calls_cb = Arc::clone(&calls);
        let mixer: MixerFn = Arc::new(move |dev, vol_db, muted: Option<bool>| {
            calls_cb
                .lock()
                .unwrap()
                .push((dev.to_string(), vol_db, muted));
            if dev.contains("DAC") {
                MixerApplyOutcome {
                    hardware_volume: true,
                    hardware_mute: true,
                    volume_db: vol_db.clamp(-64.0, 0.0),
                    min_db: -64.0,
                    max_db: 0.0,
                }
            } else {
                MixerApplyOutcome::digital(vol_db)
            }
        });

        let mut cfg = test_cfg();
        cfg.device = "hw:CARD=DAC,DEV=0".into();
        cfg.volume.volume_db = -14.0;
        cfg.volume.muted = true;

        let mut h = BuilderProcHarness::with_mixer(cfg.clone(), mixer);
        // Startup negotiation opens the DAC -> applies hardware mixer.
        h.negotiate(
            48_000,
            RateRatio::integer(1),
            SinkChoice::Dsp,
            SampleFormat::S32,
        );
        let status = h.mirror.volume_status();
        assert!(status.hardware);
        // Digital attenuation continues below the card's -64 dB floor.
        assert_eq!(status.min_db, VOLUME_MIN_DB);
        assert_eq!(status.max_db, 0.0);
        assert_eq!(status.volume_db, -14.0);
        assert!(status.muted);
        // Hardware volume + 0 balance keeps dsp_has_effect = false (bit-perfect passthrough).
        assert!(!h.mirror.dsp_has_effect.load(Ordering::Acquire));
        assert!(!h.mirror.digital_gain.load(Ordering::Acquire));

        // Adding non-zero digital balance activates DSP effect even with hardware volume.
        let mut bal_cfg = cfg.clone();
        bal_cfg.volume.balance_db = 3.0;
        assert!(
            !h.state
                .apply_config(&bal_cfg, &h.mirror, EngineState::Ready, 48_000, None, None)
                .unwrap()
        );
        assert!(h.mirror.dsp_has_effect.load(Ordering::Acquire));

        // Rate switch / re-acquire triggers apply_negotiated -> re-applies hardware mixer.
        h.negotiate(
            96_000,
            RateRatio::integer(1),
            SinkChoice::Direct,
            SampleFormat::S32,
        );

        // Switching to a card without mixer (Loopback) falls back to digital.
        let mut loopback_cfg = bal_cfg;
        loopback_cfg.device = "hw:Loopback,0,0".into();
        assert!(
            h.state
                .apply_config(
                    &loopback_cfg,
                    &h.mirror,
                    EngineState::Ready,
                    96_000,
                    None,
                    None
                )
                .unwrap()
        );
        h.negotiate(
            96_000,
            RateRatio::integer(1),
            SinkChoice::Dsp,
            SampleFormat::S32,
        );
        let fallback_status = h.mirror.volume_status();
        assert!(!fallback_status.hardware);
        assert_eq!(fallback_status.min_db, VOLUME_MIN_DB);
        assert_eq!(fallback_status.max_db, VOLUME_MAX_DB);

        let recorded = calls.lock().unwrap();
        assert!(
            recorded
                .iter()
                .any(|(d, v, m)| d == "hw:CARD=DAC,DEV=0" && *v == -14.0 && *m == Some(true))
        );
        assert!(recorded.iter().any(|(d, _, _)| d == "hw:Loopback,0,0"));
        assert!(recorded.len() >= 4);
    }

    #[test]
    fn dc_block_passthrough_reporting() {
        let mut cfg = test_cfg();
        assert!(!chain_has_effect(&cfg));
        assert!(!dsp_has_effect(&cfg, 0.0));

        cfg.chain.dc_block = Some(audivane_protocol::DcBlockConfig {
            enabled: false,
            cutoff_hz: 5.0,
        });
        assert!(!chain_has_effect(&cfg));
        assert!(!dsp_has_effect(&cfg, 0.0));

        cfg.chain.dc_block = Some(audivane_protocol::DcBlockConfig {
            enabled: true,
            cutoff_hz: 5.0,
        });
        assert!(chain_has_effect(&cfg));
        assert!(dsp_has_effect(&cfg, 0.0));

        cfg.dsp_enabled = false;
        assert!(!chain_has_effect(&cfg));
        assert!(!dsp_has_effect(&cfg, 0.0));
    }

    /// Captures `log::warn!` messages emitted on the current thread during `f`.
    fn capture_warns<T>(f: impl FnOnce() -> T) -> (T, Vec<String>) {
        use std::cell::RefCell;
        use std::sync::Once;

        thread_local! {
            static WARNS: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
        }

        struct CaptureLogger;
        impl log::Log for CaptureLogger {
            fn enabled(&self, meta: &log::Metadata<'_>) -> bool {
                meta.level() <= log::Level::Warn
            }
            fn log(&self, record: &log::Record<'_>) {
                if record.level() == log::Level::Warn {
                    WARNS.with(|slot| {
                        if let Some(buf) = slot.borrow_mut().as_mut() {
                            buf.push(record.args().to_string());
                        }
                    });
                }
            }
            fn flush(&self) {}
        }

        static LOGGER: CaptureLogger = CaptureLogger;
        static INIT: Once = Once::new();

        INIT.call_once(|| {
            let _ = log::set_logger(&LOGGER);
            log::set_max_level(log::LevelFilter::Warn);
        });
        WARNS.with(|slot| *slot.borrow_mut() = Some(Vec::new()));
        let out = f();
        let warns = WARNS.with(|slot| slot.borrow_mut().take().unwrap_or_default());
        (out, warns)
    }

    /// On a card with no ALSA simple mixer volume control, releasing /
    /// stopping the engine neither writes the mixer nor logs an unknown-level
    /// warning.
    #[test]
    fn mixerless_card_shuts_down_quietly_without_warning_or_write() {
        for (mode, hardware_db) in [
            (VolumeMode::Auto, None),
            (VolumeMode::Hardware, None),
            (VolumeMode::Hybrid, Some(0.0)),
            (VolumeMode::Software, Some(0.0)),
        ] {
            let mut cfg = test_cfg();
            cfg.device = "hw:CARD=USB,DEV=0".into();
            cfg.volume.mode = mode;
            cfg.volume.hardware_db = hardware_db;
            cfg.volume.volume_db = -30.0;
            let writes: MixerLog<(f64, Option<bool>)> = Arc::default();
            let cb = Arc::clone(&writes);
            let mixer: MixerFn = Arc::new(move |_dev, db, mute: Option<bool>| {
                cb.lock().unwrap().push((db, mute));
                MixerApplyOutcome::digital(db)
            });
            let mut h = BuilderProcHarness::with_mixer(cfg, mixer);
            h.state.card_state = Arc::new(|_: &str| CardRead::NoControl);
            h.state.switch_state = Arc::new(|_: &str| None);
            open(&mut h);
            writes.lock().unwrap().clear();

            let ((), warns) = capture_warns(|| {
                h.state.leave_hardware_safe();
            });
            assert!(
                writes.lock().unwrap().is_empty(),
                "{mode:?}: unexpected mixer write on shutdown"
            );
            assert!(
                warns.is_empty(),
                "{mode:?}: unexpected shutdown warning on mixerless card: {warns:?}"
            );
        }
    }

    /// When a card's volume control cannot be read (and no hardware readback
    /// is cached), releasing / stopping the engine writes nothing and keeps
    /// the unknown-level warning.
    #[test]
    fn unreadable_control_warns_and_writes_nothing_on_shutdown() {
        for (mode, hardware_db) in [
            (VolumeMode::Auto, None),
            (VolumeMode::Hardware, None),
            (VolumeMode::Hybrid, Some(0.0)),
            (VolumeMode::Software, Some(0.0)),
        ] {
            let mut cfg = test_cfg();
            cfg.device = "hw:CARD=USB,DEV=0".into();
            cfg.volume.mode = mode;
            cfg.volume.hardware_db = hardware_db;
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
            let mut h = BuilderProcHarness::with_mixer(cfg, mixer);
            h.state.card_state = Arc::new(|_: &str| CardRead::Error);
            open(&mut h);
            writes.lock().unwrap().clear();
            h.state.hw_known = None;

            let ((), warns) = capture_warns(|| {
                h.state.leave_hardware_safe();
            });
            assert!(
                writes.lock().unwrap().is_empty(),
                "{mode:?}: unexpected mixer write with unreadable level"
            );
            assert!(
                warns
                    .iter()
                    .any(|w| w.contains("cannot read the level of hw:CARD=USB,DEV=0")),
                "{mode:?}: expected unknown-level warning, got {warns:?}"
            );
        }
    }
}
