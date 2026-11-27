//! The IPC call dispatcher.
//!
//! [`Service`] is the only place where a control-plane request turns into
//! engine and graph actions. It owns the authoritative [`EngineConfig`], keeps
//! it in sync with the persisted file, and combines engine snapshots with
//! PipeWire facts into protocol replies.
//!
//! Every method here runs on a connection thread (never on an audio thread)
//! and may block briefly on the engine.

use crate::api::{Engine, EngineError};
use crate::control::ab_measure::{Measurement, Outcome};
use crate::control::config::{self, ConfigStore};
use crate::control::device_identity;
use crate::control::graph::{Facts, GraphWatcher};
use crate::control::mpris::MprisWatcher;
use crate::control::passthrough::{self, SinkView, StreamView};
use crate::control::persist::Persister;
use crate::control::presets::{self, PresetError, PresetStore};
use crate::control::volume_guard::{self, EPS_DB, LevelGuard, VolumeEventLog};
use audivane_io_alsa::identity::{CardFacts, Roots};
use audivane_io_pw::SinkVolumeRequest;
use audivane_protocol::ipc::{
    AB_MEASURE_DEFAULT_SECONDS, AB_MEASURE_SECONDS, AbConfig, AbSide, AbStatus, BlockReason, Call,
    DeviceAssignment, DeviceInfo, EngineConfig, EngineState, FollowerConfig, GuardMode, LevelMatch,
    Meters, NowPlaying, OutputsStatus, PartPresetState, Passthrough, Preset, PresetInfo,
    PresetPart, PresetState, ResponseWindowRequest, RoutingMode, RpcError, SinkChoice,
    SinkVolumePolicy, Spectrum, Status, StreamInfo, TiltStatus, Topic, UnresolvedDevice,
    VarispeedConfig, VarispeedStatus, VolumeConfig, VolumeSource, VolumeStatus,
};
use audivane_protocol::{
    ChainConfig, HarmonicsConfig, RateRatio, TiltConfig, UpsamplerConfig, resolve_rate_ratio,
};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

/// Rate-specific bounds of an enabled tilt (cheap, no design). Recompute from
/// the live graph rate so a rate change cannot leave clients with a stale
/// clamp note. [`Service::tilt_status`] adds design failures (cached).
/// One status covers the whole tilt stage (`tilt` and `tilt_high`): the
/// outer edges of its enabled sections, each section's clamp note.
fn tilt_status(chain: &ChainConfig, rate: u32) -> Option<TiltStatus> {
    let eq = chain.eq.as_ref()?;
    let fs = f64::from(rate);
    let on: Vec<(&str, &TiltConfig)> = [
        ("tilt", eq.tilt.as_ref()),
        ("tilt_high", eq.tilt_high.as_ref()),
    ]
    .into_iter()
    .filter_map(|(name, t)| Some((name, t.filter(|t| t.enabled)?)))
    .collect();
    let (first, last) = (on.first()?.1, on.last()?.1);
    let notes: Vec<String> = on
        .iter()
        .filter(|(_, t)| t.end_hz > 0.45 * fs)
        .map(|(name, t)| {
            format!(
                "{name} end_hz clamped from {:.3} to {:.3} Hz at {rate} Hz (0.45 * fs)",
                t.end_hz,
                0.45 * fs
            )
        })
        .collect();
    let pair_error =
        audivane_protocol::check_tilt_sections(eq.tilt.as_ref(), eq.tilt_high.as_ref()).err();
    let error = pair_error.or_else(|| {
        on.iter().find_map(|(name, t)| {
            audivane_dsp_core::tilt::effective_band(t, fs)
                .err()
                .map(|e| e.replacen("tilt:", &format!("{name}:"), 1))
        })
    });
    Some(TiltStatus {
        sample_rate: rate,
        effective_start_hz: first.start_hz,
        effective_end_hz: last.end_hz.min(0.45 * fs),
        clamp_note: (!notes.is_empty()).then(|| notes.join("; ")),
        error,
    })
}

/// A tilt design cache key: both sections and the rate.
type TiltDesignKey = (Option<TiltConfig>, Option<TiltConfig>, u32);

/// What the IPC layer must do with a dispatched call.
#[derive(Debug)]
pub enum Dispatch {
    /// Reply with this JSON value.
    Value(Value),
    /// Register the connection for these event topics (and reply `null`).
    Subscribe(Vec<Topic>),
    /// Reply `null`, then stop the daemon.
    Shutdown,
}

/// Shared daemon shutdown latch.
#[derive(Debug, Default)]
pub struct ShutdownFlag {
    state: Mutex<bool>,
    cv: Condvar,
}

impl ShutdownFlag {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn request(&self) {
        if let Ok(mut state) = self.state.lock() {
            *state = true;
            self.cv.notify_all();
        }
    }

    pub fn is_requested(&self) -> bool {
        self.state.lock().map(|s| *s).unwrap_or(false)
    }

    /// Block until a shutdown is requested.
    pub fn wait(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        while !*state {
            state = self.cv.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Wait for shutdown, or return after `timeout`. Returns true when a
    /// shutdown was requested.
    pub fn wait_timeout(&self, timeout: std::time::Duration) -> bool {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let (state, _) = self
            .cv
            .wait_timeout_while(state, timeout, |s| !*s)
            .unwrap_or_else(|e| e.into_inner());
        *state
    }
}

#[derive(Default)]
struct DispatchGate {
    closing: bool,
    active: usize,
}

struct DispatchGuard<'a> {
    service: &'a Service,
}

impl Drop for DispatchGuard<'_> {
    fn drop(&mut self) {
        let mut gate = self.service.gate.lock().unwrap_or_else(|e| e.into_inner());
        gate.active = gate.active.saturating_sub(1);
        if gate.active == 0 {
            self.service.gate_cv.notify_all();
        }
    }
}

pub struct Service {
    engine: Arc<dyn Engine>,
    /// [`VolumeStatus::instance`]: this daemon process.
    instance: String,
    /// The level state and its [`VolumeStatus::revision`]. Only changed
    /// while the config lock is held (lock order: config, then this).
    levels: Mutex<LevelRevision>,
    /// Serializes all config, preset, routing, and A/B mutations so engine
    /// apply, routing updates, persistence queuing, and preset files stay in
    /// commit order.
    mutation: Mutex<()>,
    config: Mutex<EngineConfig>,
    store: Option<Arc<ConfigStore>>,
    /// Coalesced writer for `config.json` (None when there is no store).
    persister: Option<Arc<Persister>>,
    /// The active presets (global and per part), as last loaded/saved.
    /// `Status.preset.modified` and `Status.part_presets` compare chain A
    /// with them, so `Status` never has to read the preset dir.
    preset_cache: Mutex<PresetCache>,
    graph: Option<Arc<GraphWatcher>>,
    mpris: Option<Arc<MprisWatcher>>,
    shutdown: Arc<ShutdownFlag>,
    gate: Mutex<DispatchGate>,
    gate_cv: Condvar,
    /// Current A/B side, as last commanded. The engine snapshot is preferred
    /// when it reports one.
    ab_current: Mutex<AbSide>,
    /// A blind test runs (`ab_blind`): status hides the side. Not persisted.
    ab_blind: AtomicBool,
    /// Bumped on every config change (any client or file load); the event
    /// pump uses it to push `Event::Config`.
    config_gen: AtomicU64,
    /// Bumped whenever the preset directory changes through the daemon.
    presets_gen: AtomicU64,
    /// Measurement playback (`measure_play`).
    measurer: super::measure::Measurer,
    /// Recently measured upsampler responses, keyed by design and source rate.
    /// Reopening the view is instant; measuring a long kernel is expensive.
    /// Each entry keeps the measured impulse, so zoom windows are evaluated
    /// from it without measuring again.
    upsampler_cache: Mutex<Vec<UpsamplerCacheEntry>>,
    /// Recent convolution response curves (`conv_response`), per loaded IRs
    /// and rate.
    conv_response_cache: super::conv_response::ResponseCache,
    /// Last analytic A/B offset computed from the config: chain A, chain B,
    /// rate and the result.
    ab_offset_cache: Mutex<Option<AbOffsetCacheEntry>>,
    /// Recent tilt design results for status: config, rate, design error.
    tilt_design_cache: Mutex<Vec<(TiltDesignKey, Option<String>)>>,
    /// Volume changes the guard or the ceiling blocked or reduced.
    volume_log: Arc<VolumeEventLog>,
    /// Guard state of the sink volume (`sink_volume: follow`).
    sink_guard: Mutex<LevelGuard>,
    /// DSP overload fallback: maps the requested config to the one the
    /// engine runs ([`super::overload`]).
    overload: Mutex<super::overload::Supervisor>,
    /// Bumped on every fallback step, for `Event::Overload`.
    overload_gen: AtomicU64,
    /// The per-rate upsampler plan the engine holds, what it was made from,
    /// and its status rows ([`super::cpu_budget::plan_rates`]).
    rate_plan: Mutex<RatePlanState>,
    /// The source rate last seen playing on the DSP chain (0: none yet):
    /// admission prices that landing while nothing plays.
    last_source_rate: AtomicU32,
    /// The running measured level match (`ab_measure`), if any.
    ab_measure: Mutex<Option<MeasureRun>>,
    /// The last measurement epoch handed to the engine.
    ab_measure_epoch: AtomicU32,
    /// Why the last measurement ended without a lock ([`AbStatus::measure_error`]).
    ab_measure_error: Mutex<Option<String>>,
    /// The cards as last looked at, and the outputs held closed then
    /// ([`super::device_identity`]).
    identity: Mutex<IdentityState>,
}

/// What [`Service::identity_tick`] saw last.
#[derive(Default)]
struct IdentityState {
    roots: Roots,
    /// The cards at the last look (`None`: look again).
    cards: Option<Vec<CardFacts>>,
    unresolved: Vec<UnresolvedDevice>,
}

/// A running `ab_measure`: the gating state and the pair it measures
/// (chain A and the A/B config, its level left out).
struct MeasureRun {
    measurement: Measurement,
    pair: (ChainConfig, AbConfig),
    /// The track playing when it started (or when one was first known).
    track: Option<TrackId>,
    /// The `(graph, output)` rates it measures at, once the engine runs.
    rates: Option<(u32, u32)>,
}

/// A now-playing identity: the player and its `mpris:trackid` when it
/// gives one, else the player and the title, artist and album.
#[derive(Clone, Debug, PartialEq)]
enum TrackId {
    Id(String, String),
    Tags(String, Option<String>, Option<String>, Option<String>),
}

impl From<NowPlaying> for TrackId {
    fn from(n: NowPlaying) -> Self {
        match n.track_id {
            Some(id) => Self::Id(n.player, id),
            None => Self::Tags(n.player, n.title, n.artist, n.album),
        }
    }
}

impl MeasureRun {
    /// Why the run is no longer about what plays: another track or another
    /// rate. An unknown track or a stopped engine is not a change; the
    /// first known value becomes the reference.
    fn changed(
        &mut self,
        track: Option<TrackId>,
        rates: Option<(u32, u32)>,
    ) -> Option<&'static str> {
        match (&self.track, track) {
            (Some(was), Some(now)) if *was != now => return Some("the track changed"),
            (None, now @ Some(_)) => self.track = now,
            _ => {}
        }
        match (self.rates, rates) {
            (Some(was), Some(now)) if was != now => return Some("the sample rate changed"),
            (None, now @ Some(_)) => self.rates = now,
            _ => {}
        }
        None
    }
}

/// The pair a measurement is valid for: chain A and the A/B config apart
/// from its level (the measurement taps both sides before the gains) and
/// its label.
fn measured_pair(cfg: &EngineConfig) -> Option<(ChainConfig, AbConfig)> {
    cfg.ab.as_ref().map(|ab| {
        (
            cfg.chain.clone(),
            AbConfig {
                level: LevelMatch::Off,
                label: None,
                ..ab.clone()
            },
        )
    })
}

type AbOffsetCacheEntry = (
    ChainConfig,
    ChainConfig,
    u32,
    AbLevel,
    Result<AbGains, String>,
);

use audivane_dsp_core::ab_level::{AbGains, AbLevel};
use audivane_dsp_core::resample::MeasuredUpsampler;

/// Keyed by design, source rate and the engine's largest input block there
/// (the latency it reports counts that block).
type UpsamplerCacheKey = (UpsamplerConfig, u32, usize);
type UpsamplerCacheEntry = (UpsamplerCacheKey, Arc<MeasuredUpsampler>);

/// How many measured responses to keep. Reopening a view usually replays one
/// of a handful of source rates.
const UPSAMPLER_CACHE_LEN: usize = 4;
/// Impulse samples kept across the cache beyond the most recent entry
/// (16 Mi f64 = 128 MiB). A 4M-tap kernel is about 4 Mi samples.
const UPSAMPLER_CACHE_SAMPLES: usize = 16 << 20;

impl Service {
    pub fn new(
        engine: Arc<dyn Engine>,
        config: EngineConfig,
        store: Option<Arc<ConfigStore>>,
        graph: Option<Arc<GraphWatcher>>,
        mpris: Option<Arc<MprisWatcher>>,
        shutdown: Arc<ShutdownFlag>,
    ) -> Arc<Self> {
        let persister = store
            .as_ref()
            .map(|store| Persister::new(Arc::clone(store)));
        let preset_cache = PresetCache::default().refreshed(store.as_ref(), &config);
        // The engine runs the startup config already: judged like an
        // accepted edit, so it can count as the last one that ran fine.
        let mut overload = super::overload::Supervisor::default();
        overload.seed(&config, engine.applied_generation());
        let seed_upsampler = config.dsp_enabled
            && !matches!(
                config.chain.upsampler.target,
                audivane_protocol::RateTarget::Off
            );
        let startup_cfg = seed_upsampler.then(|| config.clone());
        let service = Arc::new(Self {
            engine,
            instance: new_instance_id(),
            levels: Mutex::new(LevelRevision {
                levels: levels_of(&config),
                applied_generation: 0,
                revision: 1,
            }),
            mutation: Mutex::new(()),
            config: Mutex::new(config),
            store,
            persister,
            preset_cache: Mutex::new(preset_cache),
            graph,
            mpris,
            shutdown,
            gate: Mutex::new(DispatchGate::default()),
            gate_cv: Condvar::new(),
            ab_current: Mutex::new(AbSide::A),
            ab_blind: AtomicBool::new(false),
            config_gen: AtomicU64::new(0),
            presets_gen: AtomicU64::new(0),
            measurer: super::measure::Measurer::default(),
            upsampler_cache: Mutex::new(Vec::new()),
            conv_response_cache: Default::default(),
            ab_offset_cache: Mutex::new(None),
            tilt_design_cache: Mutex::new(Vec::new()),
            volume_log: Arc::new(VolumeEventLog::default()),
            sink_guard: Mutex::new(LevelGuard::default()),
            overload: Mutex::new(overload),
            overload_gen: AtomicU64::new(0),
            rate_plan: Mutex::new(RatePlanState::default()),
            last_source_rate: AtomicU32::new(0),
            ab_measure: Mutex::new(None),
            ab_measure_epoch: AtomicU32::new(0),
            ab_measure_error: Mutex::new(None),
            identity: Mutex::new(IdentityState::default()),
        });
        if let Some(cfg) = startup_cfg {
            service.sync_rate_plan(&cfg);
        }
        service
    }

    pub fn engine(&self) -> &Arc<dyn Engine> {
        &self.engine
    }

    pub fn shutdown_flag(&self) -> Arc<ShutdownFlag> {
        Arc::clone(&self.shutdown)
    }

    fn lock_mutation(&self) -> MutexGuard<'_, ()> {
        self.mutation.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lock_config(&self) -> MutexGuard<'_, EngineConfig> {
        self.config.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Look up a measured response and move it to the front (most recent).
    fn cached_upsampler_response(&self, key: &UpsamplerCacheKey) -> Option<Arc<MeasuredUpsampler>> {
        let mut cache = self
            .upsampler_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let position = cache.iter().position(|(cached, _)| cached == key)?;
        let entry = cache.remove(position);
        let measured = Arc::clone(&entry.1);
        cache.insert(0, entry);
        Some(measured)
    }

    fn store_upsampler_response(&self, key: UpsamplerCacheKey, measured: Arc<MeasuredUpsampler>) {
        let mut cache = self
            .upsampler_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if cache.iter().any(|(cached, _)| cached == &key) {
            return;
        }
        cache.insert(0, (key, measured));
        // The newest entry always stays; older ones only while the kept
        // impulses fit the sample budget.
        let mut samples = 0usize;
        let mut keep = 0usize;
        for (index, (_, entry)) in cache.iter().enumerate() {
            samples = samples.saturating_add(entry.impulse_len());
            if index > 0 && (index >= UPSAMPLER_CACHE_LEN || samples > UPSAMPLER_CACHE_SAMPLES) {
                break;
            }
            keep = index + 1;
        }
        cache.truncate(keep);
    }

    /// Chain A's output section with the dither bits capped at the
    /// negotiated format: what `match_output` resolves against. A/B configs
    /// share chain A's upsampler and output, so it is the only one.
    fn effective_output(&self, cfg: &EngineConfig) -> audivane_protocol::OutputConfig {
        let mut output = cfg.chain.output;
        output.bits =
            audivane_protocol::effective_dither_bits(output.bits, self.engine.snapshot().format);
        output
    }

    pub fn config(&self) -> EngineConfig {
        self.lock_config().clone()
    }

    pub fn graph(&self) -> Option<&Arc<GraphWatcher>> {
        self.graph.as_ref()
    }

    fn facts(&self) -> Facts {
        self.graph.as_ref().map(|g| g.facts()).unwrap_or_default()
    }

    /// Apply the routing half of `cfg` to the graph watcher.
    ///
    /// Callers must hold `self.mutation` so routing updates are serialized in
    /// commit order.
    fn apply_routing_locked(&self, cfg: &EngineConfig) {
        let Some(graph) = &self.graph else { return };
        graph.set_volume_guard(cfg.volume.guard, Arc::clone(&self.volume_log));
        match &cfg.routing {
            RoutingMode::System => {
                graph.set_rules(Vec::new());
                // While the device is released our sinks do not exist: a
                // default pointing at them would leave the session silent.
                // `acquire_device` re-applies this.
                let released = self.engine.snapshot().state == EngineState::Idle;
                graph.set_system_default(!released);
            }
            RoutingMode::Apps { rules } => {
                graph.set_system_default(false);
                graph.set_rules(rules.clone());
            }
        }
    }

    fn enter_dispatch(&self) -> Result<DispatchGuard<'_>, RpcError> {
        let mut gate = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        if gate.closing {
            return Err(RpcError {
                code: RpcError::BAD_STATE,
                message: "daemon is shutting down".into(),
            });
        }
        gate.active += 1;
        Ok(DispatchGuard { service: self })
    }

    /// Stop admitting new dispatches/mutations.
    pub fn begin_shutdown(&self) {
        let mut gate = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        gate.closing = true;
        self.gate_cv.notify_all();
    }

    /// Stop admitting new dispatches and wait until every active dispatch has
    /// finished.
    pub fn drain_dispatches(&self) {
        let mut gate = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        gate.closing = true;
        while gate.active > 0 {
            gate = self.gate_cv.wait(gate).unwrap_or_else(|e| e.into_inner());
        }
    }

    // ------------------------------------------------------------ dispatch

    pub fn dispatch(&self, call: Call) -> Result<Dispatch, RpcError> {
        let _active = self.enter_dispatch()?;
        self.dispatch_inner(call)
    }

    fn dispatch_inner(&self, call: Call) -> Result<Dispatch, RpcError> {
        let value = match call {
            Call::Status => to_value(&self.status()),
            Call::GetConfig => to_value(&self.config()),
            Call::SetConfig(cfg) => {
                let mut cfg = cfg;
                if cfg.device.trim().is_empty()
                    && let Some(device) = config::default_playback_device()
                {
                    cfg.device = device;
                }
                // Normalized and validated in `apply_config`, after the live
                // level has replaced the incoming one.
                self.apply_config(cfg)?;
                self.conv_notes_reply()
            }
            Call::SetDcBlock(dc_block) => {
                self.edit(|cfg| cfg.chain.dc_block = dc_block)?;
                Ok(Value::Null)
            }
            Call::SetCrossfeed(crossfeed) => {
                self.edit(|cfg| cfg.chain.crossfeed = crossfeed)?;
                Ok(Value::Null)
            }
            Call::SetEq(eq) => {
                self.edit(|cfg| cfg.chain.eq = eq)?;
                Ok(Value::Null)
            }
            Call::SetConvolution(conv) => {
                self.edit(|cfg| cfg.chain.convolution = conv)?;
                self.conv_notes_reply()
            }
            Call::SetConvolutionSafety(safety) => {
                self.edit(|cfg| cfg.convolution_safety = safety)?;
                self.conv_notes_reply()
            }
            Call::SetUpsampler(upsampler) => {
                self.edit(|cfg| cfg.chain.upsampler = upsampler)?;
                Ok(Value::Null)
            }
            Call::SetPitch(pitch) => {
                self.edit(|cfg| cfg.chain.pitch = pitch)?;
                Ok(Value::Null)
            }
            Call::SetHarmonics(harmonics) => {
                self.edit(|cfg| cfg.chain.harmonics = harmonics)?;
                Ok(Value::Null)
            }
            Call::HarmonicsPresets => to_value(&audivane_protocol::harmonics_presets()),
            Call::HarmonicsSystems => to_value(&audivane_protocol::harmonics_systems()),
            // A blind test: nothing that could give the side away (the
            // engine is not even asked, so its analysis stops).
            Call::HarmonicsLive if self.ab_blind_active(&self.config()) => {
                to_value(&crate::engine::harmonics_live::empty_frame(
                    audivane_protocol::ipc::HarmonicsLiveState::Hidden,
                    0,
                    0,
                ))
            }
            Call::HarmonicsLive => match self.engine.harmonics_live() {
                Some(frame) => to_value(&frame),
                None => Err(RpcError {
                    code: RpcError::BAD_STATE,
                    message: "the harmonics live view needs the audio engine".into(),
                }),
            },
            Call::HarmonicsReadout {
                config,
                sweep,
                sweep_freq_hz,
                sweep_level_dbfs,
            } => {
                // Runs without the mutation lock: it only reads the config
                // and simulates the stage's slow layers (milliseconds when
                // cached; a few hundred milliseconds for a tape layer not
                // seen yet, on up to four threads).
                let config = match config {
                    Some(c) => {
                        config::validate_harmonics("config", &c).map_err(invalid_params)?;
                        c
                    }
                    None => self.config().chain.harmonics.unwrap_or(HarmonicsConfig {
                        enabled: false,
                        layers: Vec::new(),
                        ..Default::default()
                    }),
                };
                let snap = self.engine.snapshot();
                let rate = if snap.output_rate > 0 {
                    f64::from(snap.output_rate)
                } else {
                    96_000.0
                };
                let readout = if sweep {
                    audivane_dsp_core::harmonics::readout_sweeps(
                        &config,
                        rate,
                        audivane_dsp_core::harmonics::ReadoutSweeps {
                            level_sweep_hz: sweep_freq_hz,
                            freq_sweep_dbfs: sweep_level_dbfs,
                        },
                    )
                } else {
                    audivane_dsp_core::harmonics::readout(&config, rate, false)
                }
                .map_err(|e| invalid_params(e.to_string()))?;
                to_value(&readout)
            }
            Call::HarmonicsSpectrum {
                config,
                freq_hz,
                level_dbfs,
                traces,
            } => {
                // As the readout: without the mutation lock, read-only
                // (milliseconds cached or without tape layers; up to a few
                // tenths of a second for tape layers at 768 kHz).
                let cfg = self.config();
                let config = match config {
                    Some(c) => {
                        config::validate_harmonics("config", &c).map_err(invalid_params)?;
                        c
                    }
                    None => cfg.chain.harmonics.clone().unwrap_or(HarmonicsConfig {
                        enabled: false,
                        layers: Vec::new(),
                        ..Default::default()
                    }),
                };
                let snap = self.engine.snapshot();
                let rate = if snap.output_rate > 0 {
                    f64::from(snap.output_rate)
                } else {
                    96_000.0
                };
                let output = &cfg.chain.output;
                let word = audivane_protocol::ipc::HarmonicsSpectrumWord {
                    bits: snap.dither_bits.unwrap_or(output.bits),
                    dither: output.dither,
                    headroom_db: output.headroom_db,
                };
                let spectrum = audivane_dsp_core::harmonics::spectrum(
                    &config, rate, freq_hz, level_dbfs, &traces, word,
                )
                .map_err(|e| invalid_params(e.to_string()))?;
                to_value(&spectrum)
            }
            Call::SetLimiter(limiter) => {
                self.edit(|cfg| cfg.chain.limiter = limiter)?;
                Ok(Value::Null)
            }
            Call::SetDspEnabled { enabled } => {
                self.edit(|cfg| cfg.dsp_enabled = enabled)?;
                Ok(Value::Null)
            }
            // Targeted edits for what a client would otherwise send a whole
            // (possibly stale) config for: only their own field changes.
            Call::SetOutput(output) => {
                self.edit(|cfg| cfg.chain.output = output)?;
                Ok(Value::Null)
            }
            Call::SetFormats(formats) => {
                self.edit(|cfg| cfg.formats = formats)?;
                Ok(Value::Null)
            }
            Call::SetRate(rate) => {
                self.edit(|cfg| cfg.rate = rate)?;
                Ok(Value::Null)
            }
            Call::SetDevice {
                device,
                output_channels,
            } => {
                let device = if device.trim().is_empty() {
                    config::default_playback_device().unwrap_or(device)
                } else {
                    device
                };
                if let Some(pair) = output_channels {
                    if let Some(e) = pair.error() {
                        return Err(invalid_params(e));
                    }
                    // Known for the device that is open (or was probed).
                    if let Some(caps) = self.engine.snapshot().device_caps
                        && caps.device == device
                        && let Some(e) = pair.fit_error(&device, caps.channels_max)
                    {
                        return Err(invalid_params(e));
                    }
                }
                self.edit(|cfg| {
                    // The pair belongs to the device: another device starts
                    // from its first two channels unless one is given.
                    if output_channels.is_some() || cfg.device != device {
                        cfg.output_channels = output_channels;
                    }
                    cfg.device = device;
                })?;
                Ok(Value::Null)
            }
            Call::SetAb(ab) => {
                let (status, notes) = self.set_ab(ab)?;
                with_notes(to_value(&status)?, notes)
            }
            Call::AbSelect { side, crossfade_ms } => {
                let status = self.ab_select(side, crossfade_ms)?;
                to_value(&status)
            }
            Call::AbBlind { enabled } => to_value(&self.set_ab_blind(enabled)?),
            Call::AbMeasure {
                max_seconds,
                cancel,
                accept,
            } => to_value(&self.ab_measure(max_seconds, cancel, accept)?),
            Call::SetVolume {
                volume_db,
                balance_db,
                muted,
            } => self.set_volume(Some(volume_db), Some(balance_db), Some(muted), None),
            Call::SetVolumeGuarded {
                volume_db,
                balance_db,
                muted,
                if_instance,
                if_revision,
            } => self.set_volume(
                volume_db,
                balance_db,
                muted,
                Some((if_instance, if_revision)),
            ),
            Call::SetVarispeed {
                enabled,
                speed,
                ramp_ms,
                keep_pitch,
            } => self.set_varispeed(enabled, speed, ramp_ms, keep_pitch),
            Call::SetFollower {
                device,
                remove,
                enabled,
                trim_ms,
                volume_db,
                output_channels,
                follow_volume,
                chain_patch,
                period_ms,
            } => self.set_follower(
                device,
                remove,
                FollowerEdit {
                    enabled,
                    trim_ms,
                    volume_db,
                    output_channels,
                    follow_volume,
                    chain_patch,
                    period_ms,
                },
            ),
            Call::ListStreams => to_value(&self.streams()),
            Call::ListPresets => to_value(&self.list_presets()?),
            Call::GetPreset { name, part } => to_value(&self.get_preset(&name, part)?),
            Call::SavePreset {
                name,
                overwrite,
                part,
                parts,
            } => to_value(&self.save_preset(&name, overwrite, part, parts)?),
            Call::LoadPreset { name, part } => {
                self.load_preset(&name, part)?;
                self.conv_notes_reply()
            }
            Call::BlankPreset { part } => {
                self.blank_preset(part)?;
                self.conv_notes_reply()
            }
            Call::DeletePreset { name, part } => {
                self.delete_preset(&name, part)?;
                Ok(Value::Null)
            }
            Call::RenamePreset { from, to, part } => {
                to_value(&self.rename_preset(&from, &to, part)?)
            }
            Call::ImportApo { name, text } => to_value(&self.import_apo(&name, &text)?),
            Call::ListDevices => to_value(&self.devices()),
            Call::AssignDevices { assignments } => {
                self.assign_devices(assignments)?;
                Ok(Value::Null)
            }
            Call::Route { target, sink } => {
                self.route(target, sink)?;
                Ok(Value::Null)
            }
            Call::SetStreamVolume {
                node,
                volume_db,
                muted,
            } => to_value(&self.set_stream_volume(node, volume_db, muted)?),
            Call::SetRoutingMode(mode) => {
                self.edit(|cfg| cfg.routing = mode)?;
                Ok(Value::Null)
            }
            Call::NowPlaying => to_value(&self.now_playing()),
            Call::SelfTest => {
                let result = self.engine.selftest().map_err(engine_error)?;
                to_value(&result)
            }
            Call::MeasurePlay(req) => {
                let status = self
                    .measurer
                    .play(Arc::clone(&self.engine), self.config(), req)
                    .map_err(engine_error)?;
                to_value(&status)
            }
            Call::MeasureStop => to_value(&self.measurer.stop()),
            Call::MeasureStatus => to_value(&self.measurer.status()),
            Call::BundleLoad { path } => self.bundle_load(&path),
            Call::BundleUnload => {
                let notes = self.bundle_unload()?;
                if notes.is_empty() {
                    Ok(Value::Null)
                } else {
                    with_notes(Value::Object(serde_json::Map::new()), notes)
                }
            }
            Call::BundleShow => to_value(&self.calibration_status(&self.config())),
            Call::BundleAb { path } => {
                let (status, notes) = self.bundle_ab(path.as_deref())?;
                with_notes(to_value(&status)?, notes)
            }
            Call::BundleGain { device, gain_db } => self.bundle_gain(device.as_deref(), gain_db),
            Call::ConvResponse { side, device } => {
                // Without the mutation lock: it only reads the config, and
                // the engine's IR cache holds the files at the graph rate.
                let cfg = self.config();
                let (chain, side, device) =
                    super::conv_response::chain_for(&cfg, side, device.as_deref())
                        .map_err(invalid_params)?;
                let snap = self.engine.snapshot();
                let rate = if snap.graph_rate > 0 {
                    snap.graph_rate
                } else {
                    cfg.rate.default_rate.max(8_000)
                };
                let loaded = self
                    .engine
                    .load_conv(&chain.convolution, rate)
                    .map_err(engine_error)?;
                let computed = self
                    .conv_response_cache
                    .get(loaded, rate, crate::engine::DEFAULT_CHANNELS)
                    .map_err(invalid_params)?;
                to_value(&super::conv_response::reply(
                    &chain, &computed, rate, side, device,
                ))
            }
            Call::UpsamplerResponse {
                config,
                source_rate,
                window,
            } => {
                // Deliberately runs without the mutation lock: building and
                // flushing a long kernel can take a while, and this only reads
                // the current configuration.
                let active = self.config();
                let config = config.unwrap_or_else(|| active.chain.upsampler.clone());
                let output = self.effective_output(&active);
                let config = config::resolve_upsampler_for_output(&config, &output);
                let snap = self.engine.snapshot();
                let source_rate = source_rate.unwrap_or(if snap.graph_rate > 0 {
                    snap.graph_rate
                } else {
                    44_100
                });
                let top = audivane_protocol::MAX_OUTPUT_RATE;
                if !(8_000..=top).contains(&source_rate) {
                    return Err(invalid_params(format!(
                        "source_rate must be between 8000 and {top} Hz (got {source_rate})"
                    )));
                }
                // An explicit config must obey the same kernel limits as the
                // active chain (taps, cutoff, beta, target), not just the
                // resolved ratio.
                config::validate_upsampler_at(
                    "upsampler",
                    &config,
                    &[source_rate],
                    config::rational_pipelines(active.ab.is_some()),
                )
                .map_err(invalid_params)?;
                // On a probed device, measure where the engine would open it
                // for this source rate (1:1 when PipeWire converts it first).
                let mut rates = active.rate.allowed_rates.clone();
                rates.push(source_rate);
                rates.sort_unstable();
                rates.dedup();
                let device_landings = crate::engine::source_landings(
                    &active,
                    &config,
                    snap.device_caps.as_ref(),
                    &rates,
                );
                let config = match device_landings
                    .iter()
                    .find(|l| l.source_rate == source_rate)
                {
                    Some(l) => UpsamplerConfig {
                        target: landing_target(l.ratio, source_rate),
                        ..config
                    },
                    None => config,
                };
                let ratio = resolve_rate_ratio(config.target, source_rate);
                let integer_ok = ratio
                    .as_integer()
                    .is_none_or(|r| (1..=audivane_protocol::MAX_INTEGER_RATIO).contains(&r));
                if !integer_ok || ratio.output_rate(source_rate).is_none() {
                    return Err(invalid_params(format!(
                        "this design resolves to an unsupported ratio {ratio} at {source_rate} Hz"
                    )));
                }
                if let Some(window) = &window {
                    // The response is plotted up to the grid's Nyquist for
                    // rational ratios, the output Nyquist otherwise.
                    let nyquist_out = f64::from(source_rate) * f64::from(ratio.up) / 2.0;
                    validate_response_window(window, nyquist_out).map_err(invalid_params)?;
                }
                // The block the engine builds its chain with at this rate:
                // the measured latency counts it, as the live one does.
                let max_block = audivane_io_pw::scaled_quantum(active.quantum, source_rate)
                    .clamp(512, 8192) as usize;
                let key = (config.clone(), source_rate, max_block);
                let measured = match self.cached_upsampler_response(&key) {
                    Some(measured) => measured,
                    None => {
                        let measured = Arc::new(
                            audivane_dsp_core::resample::measure_upsampler_at(
                                &config,
                                source_rate,
                                max_block,
                            )
                            .map_err(invalid_params)?,
                        );
                        self.store_upsampler_response(key, Arc::clone(&measured));
                        measured
                    }
                };
                let mut response = measured.with_window(window);
                if let (Some(plan), Some(wire)) = (measured.plan(), response.plan.as_mut()) {
                    wire.cpu_load = plan
                        .callback
                        .seconds(super::cpu_budget::machine(), source_rate, max_block)
                        .ok()
                        .map(|secs| super::cpu_budget::thread_load(secs, &active))
                        .filter(|l| l.is_finite());
                }
                response.device_landings = device_landings;
                to_value(&response)
            }
            Call::ResolveDesign { preset } => {
                let design = audivane_dsp_core::design::catalogue::by_name(&preset)
                    .ok_or_else(|| invalid_params(format!("unknown filter preset {preset:?}")))?;
                to_value(&audivane_protocol::StoredDesign::based_on(preset, design))
            }
            Call::CheckDesign {
                design,
                target,
                source_rate,
            } => {
                // Runs without the mutation lock. The check itself designs
                // no kernel; where the device lands each rate does (the
                // driver's ladder keeps only the ratios the design builds
                // at, an automatic length searched at each), so a design
                // the check refuses is answered without it: its refusals
                // do not depend on the landings.
                let active = self.config();
                let output = self.effective_output(&active);
                let check = |landings: Option<&[(u32, RateRatio)]>| {
                    config::check_design(
                        &design,
                        target,
                        source_rate,
                        &active,
                        &output,
                        super::cpu_budget::machine(),
                        landings,
                    )
                };
                let caps = self.engine.snapshot().device_caps;
                let unplaced = check(None);
                if !unplaced.errors.is_empty() || caps.is_none() {
                    return to_value(&unplaced).map(Dispatch::Value);
                }
                // On a probed device, plan each rate where the engine would
                // open it, not at the target's own ratio.
                let up = UpsamplerConfig {
                    target: target.unwrap_or(active.chain.upsampler.target),
                    preset: None,
                    design: Some(design.clone()),
                };
                let landings = crate::engine::preview_landings(&active, &up, caps.as_ref());
                to_value(&check(landings.as_deref()))
            }
            Call::Meters => to_value(&self.meters()),
            Call::ReleaseDevice => {
                let _mutation = self.lock_mutation();
                self.engine.release_device().map_err(engine_error)?;
                // Our sinks disappear with the device: a session default
                // pointing at them would leave the machine silent until the
                // device is acquired again. Give the default back now.
                let cfg = self.config();
                self.apply_routing_locked(&cfg);
                Ok(Value::Null)
            }
            Call::AcquireDevice => {
                let _mutation = self.lock_mutation();
                self.engine.acquire_device().map_err(engine_error)?;
                let cfg = self.config();
                self.apply_routing_locked(&cfg);
                Ok(Value::Null)
            }
            Call::Subscribe { topics } => return Ok(Dispatch::Subscribe(topics)),
            Call::Shutdown => {
                log::info!("shutdown requested over IPC");
                self.begin_shutdown();
                self.shutdown.request();
                return Ok(Dispatch::Shutdown);
            }
        }?;
        Ok(Dispatch::Value(value))
    }

    /// `set_volume` (`guard: None`) and `set_volume_guarded` (`guard:
    /// Some((if_instance, if_revision))`, compare-and-set).
    /// Absent level fields keep their current value.
    fn set_volume(
        &self,
        volume_db: Option<f64>,
        balance_db: Option<f64>,
        muted: Option<bool>,
        guard: Option<(String, u64)>,
    ) -> Result<Value, RpcError> {
        // Every level change holds the mutation lock: the check and
        // the write below see the same revision.
        let _mutation = self.lock_mutation();
        if let Some((want, _)) = &guard
            && *want != self.instance
        {
            return Err(RpcError {
                code: RpcError::STALE_INSTANCE,
                message: format!(
                    "this volume change was made for engine instance {want:?}, which has \
                             since restarted; read the current volume and try again"
                ),
            });
        }
        if let Some(&(_, want)) = guard.as_ref() {
            let now = self.level_state().1;
            if want != now {
                return Err(RpcError {
                    code: RpcError::STALE_REVISION,
                    message: format!(
                        "the volume changed since revision {want} (now {now}); read \
                                 the current volume and try again"
                    ),
                });
            }
        }
        // Nothing to change (a guarded write without level fields): the
        // current level, with no side effects (a desktop ramp goes on, the
        // engine and the config generation are not touched).
        if volume_db.is_none() && balance_db.is_none() && muted.is_none() {
            let cfg = self.config();
            let (applied, revision) = self.level_state();
            return to_value(&self.volume_status(&cfg, applied, revision));
        }
        // Our own control is never slowed down by the guard, but it
        // never exceeds the ceiling either.
        self.lock_sink_guard().cancel();
        let ceiling = volume_guard::ceiling_db(&self.config().volume);
        // Out of the protocol range stays an error (validation).
        let volume_db = volume_db.map(|volume_db| {
            if volume_db > ceiling + EPS_DB && volume_db <= audivane_protocol::ipc::VOLUME_MAX_DB {
                self.volume_log.record(
                    VolumeSource::Control,
                    volume_db,
                    ceiling,
                    BlockReason::Capped,
                );
                ceiling
            } else {
                volume_db
            }
        });
        let cfg = self.edit_locked(
            |cfg| {
                cfg.volume = VolumeConfig {
                    volume_db: volume_db.unwrap_or(cfg.volume.volume_db),
                    balance_db: balance_db.unwrap_or(cfg.volume.balance_db),
                    muted: muted.unwrap_or(cfg.volume.muted),
                    ..cfg.volume
                };
            },
            None,
        )?;
        let (applied, revision) = self.level_state();
        to_value(&self.volume_status(&cfg, applied, revision))
    }

    /// Live varispeed control without rebuilding the DSP chain. Omitted
    /// fields keep their current configured value; even an empty `{}` follows
    /// the full request path (re-sending to the engine and ending any active
    /// varispeed overload fallback step). Persisted through the deferred-write
    /// path (like volume).
    fn set_varispeed(
        &self,
        enabled: Option<bool>,
        speed: Option<f64>,
        ramp_ms: Option<f64>,
        keep_pitch: Option<bool>,
    ) -> Result<Value, RpcError> {
        let _mutation = self.lock_mutation();
        let mut next = self.config();
        next.varispeed = VarispeedConfig {
            enabled: enabled.unwrap_or(next.varispeed.enabled),
            speed: speed.unwrap_or(next.varispeed.speed),
            ramp_ms: ramp_ms.unwrap_or(next.varispeed.ramp_ms),
            keep_pitch: keep_pitch.unwrap_or(next.varispeed.keep_pitch),
        };
        config::validate_varispeed(&next.varispeed).map_err(invalid_params)?;
        config::validate_varispeed_budget(&next).map_err(invalid_params)?;
        config::validate_outputs(&next).map_err(invalid_params)?;
        self.engine
            .set_varispeed(next.varispeed)
            .map_err(engine_error)?;
        {
            let mut guard = self.lock_config();
            guard.varispeed = next.varispeed;
        }
        // A new varispeed request ends any active varispeed overload step
        // and releases its hold at once (chain fallback steps stay in place).
        if self.lock_overload().end_varispeed_fallback() {
            log::info!("overload fallback ended: a new varispeed setting replaces it");
            self.overload_gen.fetch_add(1, Ordering::Relaxed);
        }
        self.sync_varispeed_hold();
        self.note_config_change();
        self.persist(&next)?;
        to_value(&self.varispeed_status(&next))
    }

    /// Multi-device: add (a device not listed yet), edit or remove one
    /// follower, through the ordinary config edit (validated, applied,
    /// persisted). The master is not rebuilt.
    fn set_follower(
        &self,
        device: String,
        remove: bool,
        edit: FollowerEdit,
    ) -> Result<Value, RpcError> {
        let FollowerEdit {
            enabled,
            trim_ms,
            volume_db,
            output_channels,
            follow_volume,
            chain_patch,
            period_ms,
        } = edit;
        if let Some(pair) = output_channels {
            if let Some(e) = pair.error() {
                return Err(invalid_params(e));
            }
            // Known once the follower has opened its device.
            let max = self.engine.outputs().and_then(|o| {
                o.followers
                    .iter()
                    .find(|f| f.device == device)
                    .map(|f| f.device_channels_max)
            });
            if let Some(e) = max.and_then(|m| pair.fit_error(&device, m)) {
                return Err(invalid_params(e));
            }
        }
        let _mutation = self.lock_mutation();
        let current = self.config();
        let master_db = self.master_level_db(&current);
        let mut outputs = current.outputs;
        let at = outputs.followers.iter().position(|f| f.device == device);
        match (at, remove) {
            (None, true) => {
                return Err(invalid_params(format!("{device} is not a follower")));
            }
            (Some(i), true) => {
                outputs.followers.remove(i);
            }
            (at, false) => {
                let i = at.unwrap_or_else(|| {
                    outputs.followers.push(FollowerConfig::new(device));
                    outputs.followers.len() - 1
                });
                let f = &mut outputs.followers[i];
                if let Some(e) = enabled {
                    f.enabled = e;
                }
                if let Some(t) = trim_ms {
                    f.trim_ms = t;
                }
                // Following, its level is relative to the master's: set (or
                // turned on) now, against the master's level now; turned
                // off, it keeps the level it plays at.
                let was = f.follow_volume;
                let follow = follow_volume.unwrap_or(was);
                if was && !follow && volume_db.is_none() {
                    f.volume_db = crate::engine::follower::followed_level_db(f, master_db);
                }
                if let Some(v) = volume_db {
                    f.volume_db = v;
                }
                if let Some(p) = output_channels {
                    f.output_channels = Some(p);
                }
                f.follow_volume = follow;
                f.follow_ref_db = match (follow, was && volume_db.is_none()) {
                    (false, _) => None,
                    (true, true) => f.follow_ref_db,
                    (true, false) => Some(master_db),
                };
                if chain_patch.is_some() {
                    f.chain_patch = super::follower_chain::normalise(chain_patch);
                }
                // 0: the default period again.
                if let Some(p) = period_ms {
                    f.period_ms = (p != 0.0).then_some(p);
                }
            }
        }
        let cfg = self.edit_locked(|cfg| cfg.outputs = outputs, None)?;
        to_value(&self.outputs_status(&cfg))
    }

    /// The master's level as its DAC plays it (the effective volume, after
    /// the hardware's quantization: what a following follower tracks), or
    /// its configured level before the engine reports one.
    fn master_level_db(&self, cfg: &EngineConfig) -> f64 {
        self.engine
            .volume_state()
            .0
            .map(|v| v.volume_db)
            .filter(|v| v.is_finite())
            .unwrap_or_else(|| crate::engine::volume::target_db(&cfg.volume))
    }

    fn outputs_status(&self, cfg: &EngineConfig) -> OutputsStatus {
        self.engine
            .outputs()
            .unwrap_or_else(|| OutputsStatus::from_config(&cfg.outputs))
    }

    fn varispeed_status(&self, cfg: &EngineConfig) -> VarispeedStatus {
        self.engine
            .snapshot()
            .varispeed
            .unwrap_or_else(|| VarispeedStatus::from_config(&cfg.varispeed))
    }

    // ------------------------------------------------------------ config

    /// Validate, apply, remember and queue a whole config.
    ///
    /// If the engine rejects it nothing changes. The disk write is coalesced
    /// (see [`Persister`]): a queued write failure is logged and reported in
    /// `Status.error`, never by failing this call (the reply may already be on
    /// the wire).
    fn apply_config(&self, cfg: EngineConfig) -> Result<(), RpcError> {
        let _mutation = self.lock_mutation();
        let mut cfg = cfg;
        let current = self.config();
        // The level (volume, balance, mute) is set by `set_volume` only: a
        // whole config is usually a client's copy edited for something else,
        // and its level may be one that has changed since (or another
        // daemon's). The volume settings (mode, levels of the card, ceiling,
        // sink policy, guard) still apply.
        cfg.volume.volume_db = current.volume.volume_db;
        cfg.volume.balance_db = current.volume.balance_db;
        cfg.volume.muted = current.volume.muted;
        // The room calibration is the daemon's (`bundle_load`,
        // `bundle_unload`): a whole config keeps it, its record following
        // what the config does to the calibrated followers.
        cfg.calibration = current.calibration.clone();
        super::calibration::follow_edits(&mut cfg);
        // So are the outputs' identities: a device the client kept keeps
        // its identity, one it changed takes the new card's.
        let cards = self.cards_if(device_identity::devices_changed(&current, &cfg));
        device_identity::carry(&current, &mut cfg, &cards);
        let identity = self.reconcile_edit(&current, &mut cfg, cards);
        // Only then normalize and validate: the ignored fields are not
        // checked, the ones that apply are. A side B the client sets must
        // apply to its chain A (normalizing would turn A/B off instead).
        if let Some(ab) = &cfg.ab {
            super::ab_side::materialize(&cfg.chain, &ab.b).map_err(invalid_params)?;
        }
        if let Some(note) = config::normalize_config(&mut cfg) {
            log::info!("set_config: {note}");
        }
        config::validate(&cfg).map_err(invalid_params)?;
        self.validate_live(&current, &cfg)?;
        self.adopt_sink_levels(&current, &mut cfg);
        // Chain B is normalized by the caller; keep this a hard invariant.
        config::normalize_config(&mut cfg);
        self.engine_apply_identified(&cfg, identity)?;
        // Presets the new config names but the cache lacks are read now,
        // outside the config lock.
        let cache = self
            .lock_presets()
            .clone()
            .refreshed(self.store.as_ref(), &cfg);
        {
            let mut guard = self.lock_config();
            *guard = cfg.clone();
            self.note_levels(&guard);
            *self.lock_presets() = cache;
        }
        *self.ab_current.lock().unwrap_or_else(|e| e.into_inner()) = AbSide::A;
        self.apply_routing_locked(&cfg);
        self.note_config_change();
        self.persist(&cfg)
    }

    /// Edit a clone of the config, validate it, apply it, queue it for disk,
    /// and make it the current config. A validation or engine failure leaves
    /// everything as it was.
    fn edit<F>(&self, edit: F) -> Result<EngineConfig, RpcError>
    where
        F: FnOnce(&mut EngineConfig),
    {
        let _mutation = self.lock_mutation();
        self.edit_locked(edit, None)
    }

    fn edit_locked<F>(
        &self,
        edit: F,
        preset_cache: Option<Preset>,
    ) -> Result<EngineConfig, RpcError>
    where
        F: FnOnce(&mut EngineConfig),
    {
        let old = self.config();
        let mut next = old.clone();
        edit(&mut next);
        super::calibration::follow_edits(&mut next);
        let identity = if device_identity::outputs_changed(&old, &next) {
            let cards = self.cards_if(true);
            device_identity::fill(&old, &mut next, &cards);
            self.reconcile_edit(&old, &mut next, cards)
        } else {
            None
        };
        self.adopt_sink_levels(&old, &mut next);
        // Normalize before validating: chain B must follow A, never make a
        // chain A edit fail because chain B still has the old upsampler.
        if let Some(note) = config::normalize_config(&mut next) {
            log::info!("{note}");
        }
        if let (Some(ab), Some(_)) = (&next.ab, &old.ab) {
            for note in super::ab_side::edit_notes(&ab.b, &old.chain, &next.chain) {
                log::info!("{note}");
            }
        }
        config::validate(&next).map_err(invalid_params)?;
        self.validate_live(&old, &next)?;
        self.engine_apply_identified(&next, identity)?;
        {
            let mut guard = self.lock_config();
            *guard = next.clone();
            self.note_levels(&guard);
            if let Some(preset) = preset_cache {
                self.lock_presets().set(preset.part, Some(preset));
            }
        }
        // A level-only edit (a slew step, the desktop's volume keys) leaves
        // routing and the guard as they are: no graph round trip per step.
        if !level_only_change(&old, &next) {
            self.apply_routing_locked(&next);
        }
        self.note_config_change();
        self.persist(&next)?;
        Ok(next)
    }

    // ------------------------------------------------------------ device identity

    /// Read the cards from somewhere else than `/proc/asound` and `/sys`
    /// (tests).
    pub fn set_identity_roots(&self, roots: Roots) {
        let mut state = self.lock_identity();
        state.roots = roots;
        state.cards = None;
    }

    fn lock_identity(&self) -> MutexGuard<'_, IdentityState> {
        self.identity.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn scan_cards(&self) -> Vec<CardFacts> {
        let roots = self.lock_identity().roots.clone();
        audivane_io_alsa::identity::scan_cards(&roots)
    }

    /// The cards now when `needed`, else none (nothing to look up).
    fn cards_if(&self, needed: bool) -> Vec<CardFacts> {
        if needed {
            self.scan_cards()
        } else {
            Vec::new()
        }
    }

    /// After an edit that changed the outputs' devices: find them among
    /// the cards (renaming none normally, since each just took its card's
    /// identity) and say what to pin. `None` when nothing was looked at.
    fn reconcile_edit(
        &self,
        old: &EngineConfig,
        next: &mut EngineConfig,
        cards: Vec<CardFacts>,
    ) -> Option<(device_identity::Outcome, Vec<CardFacts>)> {
        if !device_identity::outputs_changed(old, next) {
            return None;
        }
        let outcome = device_identity::reconcile(next, &cards);
        for note in &outcome.notes {
            log::info!("{note}");
        }
        Some((outcome, cards))
    }

    /// [`Self::engine_apply`], with the pins of a reconcile in place first
    /// (the engine checks them when it opens a device); a failure leaves
    /// the pins to the next look at the cards.
    fn engine_apply_identified(
        &self,
        cfg: &EngineConfig,
        identity: Option<(device_identity::Outcome, Vec<CardFacts>)>,
    ) -> Result<(), RpcError> {
        let Some((outcome, cards)) = identity else {
            return self.engine_apply(cfg);
        };
        audivane_io_alsa::identity::set_pins(outcome.pins);
        let applied = self.engine_apply(cfg);
        let mut state = self.lock_identity();
        if applied.is_ok() {
            state.cards = Some(cards);
            state.unresolved = outcome.unresolved;
        } else {
            state.cards = None;
        }
        applied
    }

    /// One look at the cards (the daemon's device thread, twice a second):
    /// when they changed, find every output's device again and rename the
    /// outputs to the cards that are theirs.
    pub fn identity_tick(&self) {
        let cards = self.scan_cards();
        if self.lock_identity().cards.as_ref() == Some(&cards) {
            return;
        }
        let _mutation = self.lock_mutation();
        let current = self.config();
        let mut next = current.clone();
        let outcome = device_identity::reconcile(&mut next, &cards);
        for note in &outcome.notes {
            log::info!("{note}");
        }
        if next == current {
            audivane_io_alsa::identity::set_pins(outcome.pins);
            let mut state = self.lock_identity();
            state.cards = Some(cards);
            state.unresolved = outcome.unresolved;
            return;
        }
        if let Err(e) = config::validate(&next) {
            log::warn!("cannot follow the output devices to their cards: {e}");
            audivane_io_alsa::identity::set_pins(outcome.pins);
            let mut state = self.lock_identity();
            state.cards = Some(cards);
            state.unresolved = outcome.unresolved;
            return;
        }
        if let Err(e) = self.engine_apply_identified(&next, Some((outcome, cards))) {
            log::warn!(
                "cannot follow the output devices to their cards: {}",
                e.message
            );
            return;
        }
        {
            let mut guard = self.lock_config();
            *guard = next.clone();
        }
        self.note_config_change();
        let _ = self.persist(&next);
    }

    /// Outputs held closed until the user says which card each is.
    pub fn unresolved_devices(&self) -> Vec<UnresolvedDevice> {
        self.lock_identity().unresolved.clone()
    }

    /// `assign_devices`: set every named output's device at once, each
    /// taking the identity of the card it names now.
    fn assign_devices(&self, assignments: Vec<DeviceAssignment>) -> Result<(), RpcError> {
        let _mutation = self.lock_mutation();
        let cards = self.scan_cards();
        let current = self.config();
        let mut next = current.clone();
        let mut moved: Vec<(String, String)> = Vec::new();
        for a in &assignments {
            let device = a.device.trim().to_string();
            let Some(identity) = device_identity::identity_of(&cards, &device) else {
                return Err(invalid_params(format!(
                    "{device} is not a connected USB card"
                )));
            };
            if a.output == device_identity::MASTER {
                next.device = device;
                next.device_identity = Some(identity);
            } else {
                let at = current
                    .outputs
                    .followers
                    .iter()
                    .position(|f| f.device == a.output)
                    .ok_or_else(|| invalid_params(format!("{} is not a follower", a.output)))?;
                let f = &mut next.outputs.followers[at];
                moved.push((a.output.clone(), device.clone()));
                f.device = device;
                f.identity = Some(identity);
            }
        }
        // A calibration's parts stay with their followers.
        if let Some(cal) = next.calibration.as_mut() {
            for part in &mut cal.followers {
                if let Some((_, new)) = moved.iter().find(|(old, _)| *old == part.device) {
                    part.device = new.clone();
                }
            }
        }
        // The other outputs of those models are looked for again too.
        self.edit_locked(|cfg| *cfg = next, None)?;
        Ok(())
    }

    /// The checks that need the running engine: the device's rates, and
    /// the CPU admission of a new upsampler ([`super::cpu_budget`]). Client
    /// changes only; the config the daemon starts with is never refused.
    fn validate_live(&self, old: &EngineConfig, next: &EngineConfig) -> Result<(), RpcError> {
        // The probe as `next`'s output pair sees it.
        let caps = self.caps_for(next);
        config::validate_against_device(next, caps.as_ref()).map_err(invalid_params)?;
        if let Some(rates) = self.engine.cpu_rates() {
            super::cpu_budget::admit(rates, old, next, caps.as_ref(), self.playing_rate())
                .map_err(invalid_params)?;
            // Each follower's own chain, on its own thread, at every rate.
            for f in next
                .outputs
                .followers
                .iter()
                .filter(|f| f.chain_patch.is_some())
            {
                super::cpu_budget::admit_follower(rates, old, next, f).map_err(invalid_params)?;
            }
        }
        Ok(())
    }

    /// The device probe viewed for `cfg`'s output pair and formats.
    fn caps_for(&self, cfg: &EngineConfig) -> Option<audivane_protocol::ipc::DeviceCapabilities> {
        self.engine
            .snapshot()
            .device_caps
            .map(|c| config::caps_view(cfg, &c))
    }

    fn lock_overload(&self) -> MutexGuard<'_, super::overload::Supervisor> {
        self.overload.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lock_rate_plan(&self) -> MutexGuard<'_, RatePlanState> {
        self.rate_plan.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The source rate admission prices a change at: the one playing on
    /// the DSP chain, else the last one that played (`None`: the worst
    /// case).
    fn playing_rate(&self) -> Option<u32> {
        let snap = self.engine.snapshot();
        if snap.state == EngineState::Playing
            && snap.active_sink != Some(SinkChoice::Direct)
            && snap.graph_rate != 0
        {
            return Some(snap.graph_rate);
        }
        Some(self.last_source_rate.load(Ordering::Relaxed)).filter(|&r| r != 0)
    }

    /// Make the per-rate plan for the request `requested` and give it to
    /// the engine when it changed (from the request, the device probe, the
    /// last upsampler that ran or the rates that overloaded). Returns
    /// whether it did. The engine builds with it from its next chain on.
    fn sync_rate_plan(&self, requested: &EngineConfig) -> bool {
        // Before the CPU is measured, the design's own fallbacks only, made
        // again once it is (the key is kept only then).
        let rates = self.engine.cpu_rates();
        let caps = self.caps_for(requested);
        let (last_good, tripped) = {
            let sup = self.lock_overload();
            (
                sup.last_good_upsampler().cloned(),
                sup.tripped_rates(requested).to_vec(),
            )
        };
        let key = RatePlanKey::of(requested, caps.clone(), last_good.clone(), tripped.clone());
        if rates.is_some() && self.lock_rate_plan().key.as_ref() == Some(&key) {
            return false;
        }
        let (plan, rows) = super::cpu_budget::plan_rates(
            rates,
            requested,
            caps.as_ref(),
            last_good.as_ref(),
            &tripped,
        );
        let mut st = self.lock_rate_plan();
        let changed = st.plan != plan;
        if changed {
            self.engine.set_rate_plan(plan.clone());
        }
        let rows_changed = st.rows != rows;
        if rows_changed {
            for row in &rows {
                if let Some(f) = &row.fallback {
                    log::info!(
                        "per-rate plan: {} falls back ({:?}, {:?}{}): the request takes {:.0}% of \
                         a core there, the fallback {:.0}%",
                        super::cpu_budget::khz(row.source_rate),
                        f.action,
                        f.cause,
                        f.reason
                            .as_ref()
                            .map_or(String::new(), |r| format!(": {r}")),
                        row.load * 100.0,
                        f.load * 100.0
                    );
                }
            }
        }
        *st = RatePlanState {
            key: rates.is_some().then_some(key),
            plan,
            rows,
        };
        drop(st);
        if rows_changed {
            // Status carries the rows.
            self.overload_gen.fetch_add(1, Ordering::Relaxed);
        }
        changed
    }

    /// The tick's part of the per-rate plan: made again when what it
    /// depends on moved; when that changes the upsampler of the rate
    /// playing, the request is applied again so the engine rebuilds it
    /// (through the chain swap).
    fn rate_plan_step(&self) {
        let requested = self.config();
        let before = self.lock_rate_plan().plan.clone();
        if !self.sync_rate_plan(&requested) {
            return;
        }
        let snap = self.engine.snapshot();
        let effective = self.lock_overload().plan(&requested);
        let after = self.lock_rate_plan().plan.clone();
        let rate = snap.graph_rate;
        if snap.state != EngineState::Idle
            && before.substitute(&effective, rate) != after.substitute(&effective, rate)
        {
            log::info!(
                "per-rate plan: the upsampler at {} changed; rebuilding the chain",
                super::cpu_budget::khz(rate)
            );
            if let Err(e) = self.engine_apply(&requested) {
                log::warn!(
                    "per-rate plan: the engine refused the rebuild: {}",
                    e.message
                );
            }
        }
    }

    /// Apply the requested `cfg` to the engine, as the overload fallback
    /// maps it (the request itself unless a fallback is in effect). The
    /// fallback state changes only once the engine accepted the config, so
    /// a refused edit leaves a fallback and its notice in place. Called
    /// under the mutation lock.
    fn engine_apply(&self, cfg: &EngineConfig) -> Result<(), RpcError> {
        // A measurement is of one pair: a change to either side ends it
        // before the engine rebuilds (a level change does not).
        {
            let run = self.lock_ab_measure();
            if run
                .as_ref()
                .is_some_and(|r| Some(&r.pair) != measured_pair(cfg).as_ref())
            {
                drop(run);
                self.ab_measure_end(Some("cancelled: the compared chains changed".into()));
            }
        }
        let effective = self.lock_overload().plan(cfg);
        let kept = self.lock_rate_plan().clone();
        self.sync_rate_plan(cfg);
        if let Err(e) = self.engine.apply(&effective) {
            // The engine keeps the config it runs: and the plan made for it.
            self.engine.set_rate_plan(kept.plan.clone());
            *self.lock_rate_plan() = kept;
            return Err(engine_error(e));
        }
        let generation = self.engine.applied_generation();
        if self.lock_overload().commit_request(cfg, generation) {
            log::info!("overload fallback ended: the engine runs the requested setting again");
            self.overload_gen.fetch_add(1, Ordering::Relaxed);
        }
        self.sync_varispeed_hold();
        Ok(())
    }

    /// Give the engine the fallback's hold on varispeed (idempotent); the
    /// supervisor judges nothing before the audio thread applies it.
    fn sync_varispeed_hold(&self) {
        let hold = self.lock_overload().varispeed_hold();
        let seq = self.engine.set_varispeed_hold(hold);
        self.lock_overload().note_hold_seq(seq);
    }

    /// The overload fallback in effect, if any.
    pub fn overload_report(&self) -> Option<audivane_protocol::ipc::OverloadReport> {
        self.lock_overload().report().cloned()
    }

    /// Bumped on every overload fallback change (a step, its landing, its
    /// end): the event pump's cue.
    pub fn overload_generation(&self) -> u64 {
        self.overload_gen.load(Ordering::Relaxed)
    }

    /// One overload check ([`super::overload::TICK`]): sample the engine and
    /// apply a fallback step when the chain runs over real time. Serialized
    /// with config edits (the mutation lock), so a decision sees the request
    /// the engine runs; a step is committed only once the engine accepted its
    /// config, and a refused one moves on to the next the device can play.
    pub fn overload_tick(&self) {
        let _mutation = self.lock_mutation();
        self.overload_step();
        self.rate_plan_step();
        self.sync_varispeed_hold();
        self.ab_measure_step();
    }

    /// [`Self::overload_tick`]'s sample and step, under the mutation lock.
    fn overload_step(&self) {
        let snap = self.engine.snapshot();
        let requested = self.config();
        // The probed device the request plays on, as a fingerprint.
        let device = snap
            .device_caps
            .as_ref()
            .filter(|c| c.device == requested.device)
            .map_or(0, |c| {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                format!("{c:?}").hash(&mut h);
                h.finish().max(1)
            });
        let sample = super::overload::Sample {
            running: snap.state == EngineState::Playing
                && snap.active_sink != Some(SinkChoice::Direct),
            error: snap.state == EngineState::Error,
            switching: snap.state == EngineState::Switching,
            wanted: snap.gaps.dsp_wanted != 0,
            load: snap.dsp_load,
            xrun_events: snap.gaps.xrun_events,
            periods: snap.gaps.processed_periods,
            rt_overloads: snap.gaps.rt_overloads,
            rt_signals: snap.gaps.rt_signals,
            rt_failed_demotions: snap.gaps.rt_failed_demotions,
            output_rate: snap.output_rate,
            ratio: snap.ratio,
            rt_overload_generation: snap.gaps.rt_overload_generation,
            rt_overload_hold_seq: snap.gaps.rt_overload_hold_seq,
            chain_generation: snap.chain_generation,
            device,
            probes: snap.gaps.probes,
            varispeed: self.engine.varispeed_request(),
            substituted: false,
            source_rate: snap.graph_rate,
        };
        let mut sample = sample;
        let effective = self.lock_overload().plan(&requested);
        sample.substituted = self
            .lock_rate_plan()
            .plan
            .substitute(&effective, sample.source_rate)
            .is_some();
        if sample.running && sample.source_rate != 0 {
            self.last_source_rate
                .store(sample.source_rate, Ordering::Relaxed);
        }
        let caps = snap.device_caps;
        let plays = |c: &EngineConfig| {
            super::overload::Landing::of(c, &crate::engine::predicted_landings(c, caps.as_ref()))
        };
        let mut step = self.lock_overload().observe(sample, &requested, &plays);
        loop {
            match step {
                super::overload::Step::Nothing => return,
                super::overload::Step::Exhausted { log } => {
                    log::warn!("{log}");
                    return;
                }
                super::overload::Step::Confirmed => {
                    log::info!("overload fallback runs on the audio thread now");
                    self.overload_gen.fetch_add(1, Ordering::Relaxed);
                    return;
                }
                super::overload::Step::Ended => {
                    log::info!("overload fallback ended: a new varispeed setting replaces it");
                    self.overload_gen.fetch_add(1, Ordering::Relaxed);
                    return;
                }
                super::overload::Step::End | super::overload::Step::RateLeft { .. } => {
                    // The fallback no longer plays on the device, or
                    // playback left the rate it was made for: the request,
                    // if the engine takes it (with the per-rate plan, which
                    // now counts a rate left as over budget).
                    let left = matches!(step, super::overload::Step::RateLeft { .. });
                    if left {
                        self.sync_rate_plan(&requested);
                    }
                    match self.engine.apply(&requested) {
                        Ok(()) => {
                            let generation = self.engine.applied_generation();
                            self.lock_overload().end(&requested, generation);
                            if let super::overload::Step::RateLeft { from, to } = step {
                                log::info!(
                                    "overload fallback ended: playback moved from {} to {}, \
                                     where the request runs",
                                    super::cpu_budget::khz(from),
                                    super::cpu_budget::khz(to)
                                );
                            } else {
                                log::info!(
                                    "overload fallback ended: it no longer plays on the device"
                                );
                            }
                            self.overload_gen.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(err) => {
                            // Asked again later, logged once in a row.
                            if self.lock_overload().end_refused() {
                                log::error!(
                                    "the requested setting was refused by the engine ({}); \
                                     keeping the fallback and trying again",
                                    err.message
                                );
                            } else {
                                log::debug!("the requested setting refused again: {}", err.message);
                            }
                        }
                    }
                    return;
                }
                super::overload::Step::Try(t) if t.varispeed_hold().is_some() => {
                    // A varispeed step leaves the config alone: the hold
                    // goes to the engine, and the step's attempt is the
                    // chain that runs (its generation) once the audio
                    // thread applies the hold (its sequence).
                    let seq = self
                        .engine
                        .set_varispeed_hold(t.varispeed_hold().unwrap_or_default());
                    let mut sup = self.lock_overload();
                    let generation = sup.expected_generation();
                    let (log, _) = sup.commit_varispeed(t, &requested, generation, seq);
                    drop(sup);
                    log::warn!("{log}");
                    self.overload_gen.fetch_add(1, Ordering::Relaxed);
                    return;
                }
                super::overload::Step::Try(t) => {
                    // The supervisor is not held while the engine builds.
                    let effective = self.lock_overload().config_for(&t, &requested);
                    match self.engine.apply(&effective) {
                        Ok(()) => {
                            let generation = self.engine.applied_generation();
                            let (log, _) = self.lock_overload().commit(t, &requested, generation);
                            log::warn!("{log}");
                            if effective.ab.is_none() && self.lock_ab_measure().is_some() {
                                self.ab_measure_end(Some(
                                    "cancelled: A/B turned off by overload fallback".into(),
                                ));
                            }
                            self.overload_gen.fetch_add(1, Ordering::Relaxed);
                            return;
                        }
                        Err(err) => {
                            log::error!(
                                "overload fallback step refused by the engine ({}); trying the \
                                 next one",
                                err.message
                            );
                            step = self.lock_overload().rejected(t, &requested, &plays);
                        }
                    }
                }
            }
        }
    }

    fn note_config_change(&self) {
        self.config_gen.fetch_add(1, Ordering::Relaxed);
    }

    /// Generation counters for the IPC event pump (config/preset events).
    pub fn config_generation(&self) -> u64 {
        self.config_gen.load(Ordering::Relaxed)
    }

    pub fn presets_generation(&self) -> u64 {
        self.presets_gen.load(Ordering::Relaxed)
    }

    /// Queue `cfg` for the coalesced writer. Always `Ok`: persistence errors
    /// are asynchronous now and surface through [`Service::persist_error`].
    ///
    /// Callers must hold `self.mutation` so queued snapshots match commit order.
    fn persist(&self, cfg: &EngineConfig) -> Result<(), RpcError> {
        if let Some(persister) = &self.persister {
            persister.mark(cfg);
        }
        Ok(())
    }

    /// Write any queued config change now (shutdown path).
    pub fn flush_persist(&self) -> Result<(), RpcError> {
        match &self.persister {
            Some(persister) => persister.flush().map_err(|err| RpcError {
                code: RpcError::IO,
                message: err.to_string(),
            }),
            None => Ok(()),
        }
    }

    /// Stop and join the config writer thread (after [`Service::flush_persist`]).
    pub fn stop_persist(&self) {
        if let Some(persister) = &self.persister {
            persister.stop();
        }
    }

    /// Successful `config.json` writes so far (diagnostics and tests).
    pub fn persist_writes(&self) -> u64 {
        self.persister.as_ref().map_or(0, |p| p.writes())
    }

    /// Last failed config write, if any.
    pub fn persist_error(&self) -> Option<String> {
        self.persister.as_ref().and_then(|p| p.last_error())
    }

    // ------------------------------------------------------------ presets

    /// Presets live next to `config.json`; without a store (tests, `--config`
    /// in memory only) the calls answer `BAD_STATE`.
    fn preset_store(&self) -> Result<PresetStore, RpcError> {
        self.store
            .as_ref()
            .map(|store| PresetStore::alongside(store))
            .ok_or_else(|| RpcError {
                code: RpcError::BAD_STATE,
                message: "preset storage is disabled (the daemon has no config file)".into(),
            })
    }

    pub fn list_presets(&self) -> Result<Vec<PresetInfo>, RpcError> {
        Ok(self.preset_store()?.list_all())
    }

    /// Preset list for events/snapshots; never fails (an unreadable directory
    /// is logged and reads as empty).
    pub fn presets_info(&self) -> Vec<PresetInfo> {
        self.preset_store()
            .map(|store| store.list_all())
            .unwrap_or_default()
    }

    fn lock_presets(&self) -> MutexGuard<'_, PresetCache> {
        self.preset_cache.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn get_preset(&self, name: &str, part: Option<PresetPart>) -> Result<Preset, RpcError> {
        self.preset_store()?
            .scoped(part)
            .load(name)
            .map_err(preset_error)
    }

    /// Point `scope`'s active name at `name` (and its cache at `preset`),
    /// persisting the config when the name changed.
    fn set_active_preset(
        &self,
        scope: Option<PresetPart>,
        name: Option<String>,
        preset: Option<Preset>,
    ) -> Result<(), RpcError> {
        let changed_cfg = {
            let mut guard = self.lock_config();
            self.lock_presets().set(scope, preset);
            if active_name(&guard, scope) == name.as_ref() {
                None
            } else {
                set_active_name(&mut guard, scope, name);
                Some(guard.clone())
            }
        };
        if let Some(cfg) = changed_cfg {
            self.note_config_change();
            self.persist(&cfg)?;
        }
        Ok(())
    }

    fn save_preset(
        &self,
        name: &str,
        overwrite: bool,
        part: Option<PresetPart>,
        parts: Option<Vec<PresetPart>>,
    ) -> Result<PresetInfo, RpcError> {
        let _mutation = self.lock_mutation();
        let store = self.preset_store()?.scoped(part);
        let name = presets::normalize_name(name).map_err(invalid_params)?;
        let mut chain = self.config().chain;
        // The room calibration belongs to the room: a preset never holds it.
        chain.convolution.retain(|c| !c.calibration);
        let exists = store.load(&name).is_ok();
        if exists && !overwrite {
            return Err(RpcError {
                code: RpcError::BAD_STATE,
                message: format!(
                    "a preset named {name:?} already exists; pass overwrite to replace it"
                ),
            });
        }
        let preset = match part {
            Some(part) => Preset::for_part(name.clone(), part, chain),
            None => {
                let mut preset = Preset::new(name.clone(), chain);
                // Every part ticked stores no list: the file is today's.
                preset.parts =
                    parts.filter(|parts| !PresetPart::ALL.iter().all(|part| parts.contains(part)));
                preset
            }
        };
        presets::validate(&preset).map_err(invalid_params)?;
        let info = store.save(&preset).map_err(preset_error)?;
        self.set_active_preset(part, Some(name), Some(preset))?;
        self.presets_gen.fetch_add(1, Ordering::Relaxed);
        Ok(info)
    }

    fn load_preset(&self, name: &str, part: Option<PresetPart>) -> Result<(), RpcError> {
        let _mutation = self.lock_mutation();
        let store = self.preset_store()?.scoped(part);
        let name = presets::normalize_name(name).map_err(invalid_params)?;
        let preset = store.load(&name).map_err(preset_error)?;
        presets::validate(&preset).map_err(invalid_params)?;
        let preset_name = preset.name.clone();
        let source = preset.clone();
        // A global preset replaces the parts it holds: their pages' presets
        // are no longer what plays, so their names are cleared.
        let replaced = if part.is_none() {
            preset.covers()
        } else {
            Vec::new()
        };
        self.edit_locked(
            |cfg| {
                cfg.chain = source.apply_to(&cfg.chain);
                set_active_name(cfg, part, Some(preset_name));
                for covered in &replaced {
                    set_active_name(cfg, Some(*covered), None);
                }
            },
            Some(preset),
        )?;
        let mut cache = self.lock_presets();
        for covered in replaced {
            cache.set(Some(covered), None);
        }
        Ok(())
    }

    /// [`Call::BlankPreset`]: `part` back to its defaults, unnamed.
    fn blank_preset(&self, part: PresetPart) -> Result<(), RpcError> {
        let _mutation = self.lock_mutation();
        // A blank EQ is an empty one that is on (no EQ reads as switched off).
        let chain = ChainConfig {
            eq: Some(audivane_protocol::EqProfile::default()),
            ..ChainConfig::default()
        };
        let blank = Preset::for_part(String::new(), part, chain);
        self.edit_locked(
            |cfg| {
                cfg.chain = blank.apply_to(&cfg.chain);
                set_active_name(cfg, Some(part), None);
            },
            None,
        )?;
        self.lock_presets().set(Some(part), None);
        Ok(())
    }

    fn delete_preset(&self, name: &str, part: Option<PresetPart>) -> Result<(), RpcError> {
        let _mutation = self.lock_mutation();
        let store = self.preset_store()?.scoped(part);
        let name = presets::normalize_name(name).map_err(invalid_params)?;
        let was_active = active_name(&self.config(), part) == Some(&name);
        store.delete(&name).map_err(preset_error)?;
        if was_active {
            self.set_active_preset(part, None, None)?;
        }
        self.presets_gen.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn rename_preset(
        &self,
        from: &str,
        to: &str,
        part: Option<PresetPart>,
    ) -> Result<PresetInfo, RpcError> {
        let _mutation = self.lock_mutation();
        let store = self.preset_store()?.scoped(part);
        let from = presets::normalize_name(from).map_err(invalid_params)?;
        let to = presets::normalize_name(to).map_err(invalid_params)?;
        let was_active = active_name(&self.config(), part) == Some(&from);
        let info = store.rename(&from, &to).map_err(preset_error)?;
        if was_active {
            let cached = self.lock_presets().get(part).cloned().map(|mut preset| {
                preset.name = info.name.clone();
                preset
            });
            self.set_active_preset(part, Some(info.name.clone()), cached)?;
        }
        self.presets_gen.fetch_add(1, Ordering::Relaxed);
        Ok(info)
    }

    /// Equalizer APO / AutoEQ text → an EQ preset (`presets/eq/`). It does
    /// not load it and never touches the other parts of chain A.
    fn import_apo(&self, name: &str, text: &str) -> Result<PresetInfo, RpcError> {
        let _mutation = self.lock_mutation();
        let store = self.preset_store()?.scoped(Some(PresetPart::Eq));
        let name = presets::normalize_name(name).map_err(invalid_params)?;
        let parsed = audivane_dsp_core::eq_apo::parse(text)
            .map_err(|err| invalid_params(format!("cannot parse the EQ text: {err}")))?;
        for warning in &parsed.warnings {
            log::warn!("import_apo: {warning}");
        }
        let mut profile = parsed.profile;
        if profile.name.is_empty() {
            profile.name = name.clone();
        }
        let chain = ChainConfig {
            eq: Some(profile),
            ..ChainConfig::default()
        };
        let preset = Preset::for_part(name.clone(), PresetPart::Eq, chain);
        presets::validate(&preset).map_err(invalid_params)?;
        let info = store.save(&preset).map_err(preset_error)?;
        {
            let guard = self.lock_config();
            if active_name(&guard, Some(PresetPart::Eq)) == Some(&name) {
                self.lock_presets().set(Some(PresetPart::Eq), Some(preset));
            }
        }
        self.presets_gen.fetch_add(1, Ordering::Relaxed);
        Ok(info)
    }

    // ------------------------------------------------------------ devices

    /// ALSA playback devices from `/proc/asound` (nothing is opened).
    pub fn devices(&self) -> Vec<DeviceInfo> {
        let cfg = self.config();
        let mut devices = config::playback_devices();
        config::mark_active(&mut devices, &cfg.device);
        device_identity::annotate(&mut devices, &self.scan_cards());
        devices
    }

    // ------------------------------------------------------------ A/B

    /// Configure or clear side B. A patch that does not apply to chain A, or
    /// touches a field both sides share, is refused with the field named. A
    /// full chain's upsampler target, limiter and output are realigned with
    /// chain A (the reply says so) rather than rejected: a stored preset
    /// carries its own, and a rejection would make `ab set` fail after any
    /// chain A change.
    fn set_ab(&self, ab: Option<AbConfig>) -> Result<(AbStatus, Vec<String>), RpcError> {
        let _mutation = self.lock_mutation();
        let mut notes = Vec::new();
        let mut next = self.config();
        if let Some(ab) = &ab {
            super::ab_side::materialize(&next.chain, &ab.b).map_err(invalid_params)?;
            notes.extend(super::ab_side::set_notes(&ab.b));
        }
        next.ab = ab;
        if let Some(note) = config::normalize_config(&mut next) {
            notes.push(note);
        }
        config::validate(&next).map_err(invalid_params)?;
        self.validate_live(&self.config(), &next)?;
        self.engine_apply(&next)?;
        {
            let mut guard = self.lock_config();
            *guard = next.clone();
            self.note_levels(&guard);
        }
        *self.ab_current.lock().unwrap_or_else(|e| e.into_inner()) = AbSide::A;
        self.ab_blind.store(false, Ordering::Relaxed);
        self.note_config_change();
        self.persist(&next)?;
        notes.extend(self.engine.conv_warnings().into_iter().map(|w| w.message));
        Ok((self.ab_status(), notes))
    }

    fn ab_select(&self, side: AbSide, crossfade_ms: f64) -> Result<AbStatus, RpcError> {
        if !crossfade_ms.is_finite() || !(0.0..=5000.0).contains(&crossfade_ms) {
            return Err(invalid_params(format!(
                "crossfade_ms must be between 0 and 5000 ms (got {crossfade_ms})"
            )));
        }
        let _mutation = self.lock_mutation();
        if self.lock_config().ab.is_none() {
            return Err(RpcError {
                code: RpcError::INVALID_PARAMS,
                message: "A/B is not configured".into(),
            });
        }
        let status = self
            .engine
            .ab_select(side, crossfade_ms)
            .map_err(engine_error)?;
        if let Some(current) = status.current {
            *self.ab_current.lock().unwrap_or_else(|e| e.into_inner()) = current;
        }
        // The engine's gains once a pair runs, the config estimate before.
        Ok(self.ab_status())
    }

    /// `ab_blind`: set or clear the blind test mask ([`AbStatus::blind`]).
    fn set_ab_blind(&self, enabled: bool) -> Result<AbStatus, RpcError> {
        let _mutation = self.lock_mutation();
        if enabled {
            if self.lock_config().ab.is_none() {
                return Err(RpcError {
                    code: RpcError::INVALID_PARAMS,
                    message: "A/B is not configured".into(),
                });
            }
            // The level stays where it is for the whole run.
            if self.lock_ab_measure().is_some() {
                self.ab_measure_end(None);
            }
        }
        self.ab_blind.store(enabled, Ordering::Relaxed);
        // The live view keeps nothing across either edge: no frame, count
        // or history from the blind run (it would tell the sides apart).
        self.engine.harmonics_live_suspend();
        Ok(self.ab_status())
    }

    /// Whether the blind mask is in force: set and A/B still configured.
    /// A/B turning off by any route (a config load, `set_config`) ends it.
    fn ab_blind_active(&self, cfg: &EngineConfig) -> bool {
        if cfg.ab.is_none() {
            self.ab_blind.store(false, Ordering::Relaxed);
            return false;
        }
        self.ab_blind.load(Ordering::Relaxed)
    }

    fn lock_ab_measure(&self) -> MutexGuard<'_, Option<MeasureRun>> {
        self.ab_measure.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `ab_measure`: start (or restart) a measurement, lock the running
    /// value now (`accept`), or stop it and keep the level (`cancel`).
    fn ab_measure(
        &self,
        max_seconds: Option<f64>,
        cancel: bool,
        accept: bool,
    ) -> Result<AbStatus, RpcError> {
        let _mutation = self.lock_mutation();
        let cfg = self.config();
        if cfg.ab.is_none() {
            return Err(RpcError {
                code: RpcError::INVALID_PARAMS,
                message: "A/B is not configured".into(),
            });
        }
        if cancel && accept {
            return Err(invalid_params(
                "ab_measure: cancel and accept exclude each other",
            ));
        }
        if cancel {
            if self.lock_ab_measure().is_some() {
                self.ab_measure_end(None);
            }
            return Ok(self.ab_status());
        }
        if accept {
            // What the audio thread measured so far, unless it is about
            // another track or rate by now.
            if let Some(reason) = self.ab_measure_take_hops() {
                return Err(RpcError {
                    code: RpcError::BAD_STATE,
                    message: format!("A/B level measurement {reason}"),
                });
            }
            let (offset_db, seconds) = {
                let run = self.lock_ab_measure();
                let Some(run) = run.as_ref() else {
                    return Err(RpcError {
                        code: RpcError::BAD_STATE,
                        message: "no A/B level measurement is running".into(),
                    });
                };
                let Some(offset_db) = run.measurement.offset_db() else {
                    return Err(RpcError {
                        code: RpcError::BAD_STATE,
                        message: "nothing measured yet: play some music first".into(),
                    });
                };
                (offset_db, run.measurement.seconds())
            };
            self.ab_measure_lock(offset_db, seconds)?;
            return Ok(self.ab_status());
        }
        if self.ab_blind_active(&cfg) {
            return Err(RpcError {
                code: RpcError::BAD_STATE,
                message: "a blind test is running: the level is locked".into(),
            });
        }
        let max_seconds = max_seconds.unwrap_or(AB_MEASURE_DEFAULT_SECONDS);
        if !max_seconds.is_finite() || !AB_MEASURE_SECONDS.contains(&max_seconds) {
            return Err(invalid_params(format!(
                "max_seconds must be between {} and {} s (got {max_seconds})",
                AB_MEASURE_SECONDS.start(),
                AB_MEASURE_SECONDS.end()
            )));
        }
        let mut epoch = 0;
        while epoch == 0 {
            epoch = self
                .ab_measure_epoch
                .fetch_add(1, Ordering::Relaxed)
                .wrapping_add(1);
        }
        // Whatever an earlier measurement left in the ring is dropped.
        self.engine.take_ab_hops(&mut Vec::new());
        let pair = measured_pair(&cfg).expect("A/B is configured");
        let (track, rates) = self.ab_measure_context();
        *self.lock_ab_measure() = Some(MeasureRun {
            measurement: Measurement::new(epoch, max_seconds),
            pair,
            track,
            rates,
        });
        *self.lock_ab_measure_error() = None;
        self.engine.set_ab_measure(epoch);
        Ok(self.ab_status())
    }

    fn lock_ab_measure_error(&self) -> MutexGuard<'_, Option<String>> {
        self.ab_measure_error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// The track playing and the engine's `(graph, output)` rates, when
    /// known.
    fn ab_measure_context(&self) -> (Option<TrackId>, Option<(u32, u32)>) {
        let track = self.now_playing().map(TrackId::from);
        let snap = self.engine.snapshot();
        let rates = (snap.graph_rate > 0 && snap.output_rate > 0)
            .then_some((snap.graph_rate, snap.output_rate));
        (track, rates)
    }

    /// Stop metering, drop the queued hops and the run; `error` says why
    /// it ended without a lock (`None`: a cancel).
    fn ab_measure_end(&self, error: Option<String>) {
        self.engine.set_ab_measure(0);
        self.engine.take_ab_hops(&mut Vec::new());
        self.lock_ab_measure().take();
        if let Some(e) = &error {
            log::info!("A/B level measurement ended: {e}");
        }
        *self.lock_ab_measure_error() = error;
    }

    /// Feed the running measurement the audio thread's hops, unless the
    /// track or the rate changed since it started: then it ends (its hops
    /// dropped) and the reason is returned.
    fn ab_measure_take_hops(&self) -> Option<String> {
        let (track, rates) = self.ab_measure_context();
        let mut run = self.lock_ab_measure();
        let r = run.as_mut()?;
        if let Some(reason) = r.changed(track, rates) {
            drop(run);
            let reason = format!("cancelled: {reason}");
            self.ab_measure_end(Some(reason.clone()));
            return Some(reason);
        }
        let mut hops = Vec::new();
        self.engine.take_ab_hops(&mut hops);
        for hop in hops {
            r.measurement.push(hop);
        }
        None
    }

    /// The control-plane tick of a running measurement: take the audio
    /// thread's hops and lock (or give up) when the rule says so. Under the
    /// mutation lock.
    fn ab_measure_step(&self) {
        if self.ab_measure_take_hops().is_some() {
            return;
        }
        let outcome = {
            let run = self.lock_ab_measure();
            let Some(r) = run.as_ref() else {
                return;
            };
            r.measurement
                .outcome()
                .map(|o| (o, r.measurement.seconds()))
        };
        match outcome {
            Some((Outcome::Lock(offset_db), seconds)) => {
                if let Err(e) = self.ab_measure_lock(offset_db, seconds) {
                    log::warn!("A/B level measurement could not lock: {}", e.message);
                }
            }
            Some((Outcome::NoMatch, seconds)) => self.ab_measure_end(Some(format!(
                "no match: B stayed silent over {seconds:.0} s of A"
            ))),
            None => {}
        }
    }

    /// Lock a measured offset: `level: measured` in the config, and the
    /// measurement ends. The engine takes a level-only edit without a
    /// rebuild: the running pair ramps to the new gains over 50 ms. Under
    /// the mutation lock.
    fn ab_measure_lock(&self, offset_db: f64, seconds: f64) -> Result<(), RpcError> {
        // Rounded as the UI shows it, so the config file stays readable.
        let offset_db = (offset_db.clamp(-60.0, 60.0) * 100.0).round() / 100.0;
        let seconds = (seconds * 10.0).round() / 10.0;
        self.ab_measure_end(None);
        log::info!("A/B level measurement locked: B {offset_db:+.2} dB after {seconds:.1} s");
        self.edit_locked(
            |cfg| {
                if let Some(ab) = &mut cfg.ab {
                    ab.level = LevelMatch::Measured { offset_db, seconds };
                }
            },
            None,
        )
        .map(|_| ())
    }

    /// The level in force and the running measurement, for an A/B status.
    fn decorate_ab(&self, mut status: AbStatus, cfg: &EngineConfig) -> AbStatus {
        if let Some(ab) = &cfg.ab {
            status.level = ab.level;
        }
        status.measuring = self
            .lock_ab_measure()
            .as_ref()
            .map(|r| r.measurement.status());
        status.measure_error = self.lock_ab_measure_error().clone();
        status.blind = self.ab_blind_active(cfg);
        if status.blind {
            // Overs differ between the sides, so they would give it away.
            status.current = None;
            status.overs = Default::default();
        }
        status
    }

    /// A/B status. The engine's gains (computed when the pair was built,
    /// with the IR and upsampler terms) win; before the engine has a pair,
    /// they are estimated from the configs and cached by chain pair, level
    /// and rate, so status polling never designs filters. A failed analytic
    /// offset is reported, not hidden as 0 dB.
    fn ab_status(&self) -> AbStatus {
        let cfg = self.config();
        let current = *self.ab_current.lock().unwrap_or_else(|e| e.into_inner());
        let snap = self.engine.snapshot();
        let Some(ab) = &cfg.ab else {
            self.ab_blind_active(&cfg);
            return AbStatus {
                enabled: false,
                current: Some(current),
                ..Default::default()
            };
        };
        if self.lock_overload().plan(&cfg).ab.is_none() {
            let status = AbStatus {
                enabled: false,
                current: Some(current),
                ..Default::default()
            };
            return self.decorate_ab(status, &cfg);
        }
        if let Some(engine_ab) = snap.ab.filter(|s| s.enabled) {
            return self.decorate_ab(engine_ab, &cfg);
        }
        let rate = if snap.graph_rate > 0 {
            snap.graph_rate
        } else {
            cfg.rate.default_rate.max(1)
        };
        let level = AbLevel::from_config(&ab.level, ab.headroom);
        let (gains, error) = match self.estimated_ab_gains(&cfg.chain, &ab.chain_b, rate, level) {
            Ok(gains) => (gains, None),
            Err(e) => (AbGains::default(), Some(e)),
        };
        let status = AbStatus {
            enabled: true,
            current: Some(current),
            offset_db: gains.offset_db,
            gain_a_db: gains.gain_a_db,
            gain_b_db: gains.gain_b_db,
            headroom_db: gains.headroom_db,
            error,
            ..Default::default()
        };
        self.decorate_ab(status, &cfg)
    }

    /// [`tilt_status`] plus a design failure at `rate` in `error`: a band can
    /// be usable and the design still miss its check. The design runs once
    /// per tilt config and rate (cached), not on every status poll.
    fn tilt_status(&self, chain: &ChainConfig, rate: u32) -> Option<TiltStatus> {
        let mut status = tilt_status(chain, rate)?;
        if status.error.is_none() {
            let eq = chain.eq.as_ref()?;
            let key = (eq.tilt, eq.tilt_high, rate);
            let mut cache = self
                .tilt_design_cache
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let hit = cache
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, e)| e.clone());
            status.error = match hit {
                Some(error) => error,
                None => {
                    let error = audivane_dsp_core::tilt::TiltFilter::design_stage(
                        eq.tilt.as_ref(),
                        eq.tilt_high.as_ref(),
                        f64::from(rate),
                    )
                    .err();
                    // Chains A and B, a couple of rates: keep a few entries.
                    if cache.len() >= 4 {
                        cache.remove(0);
                    }
                    cache.push((key, error.clone()));
                    error
                }
            };
        }
        Some(status)
    }

    /// Estimated A/B gains for this chain pair, level and `rate`, memoized
    /// (one entry: the config changes rarely, status is polled often).
    fn estimated_ab_gains(
        &self,
        a: &ChainConfig,
        b: &ChainConfig,
        rate: u32,
        level: AbLevel,
    ) -> Result<AbGains, String> {
        let mut cache = self
            .ab_offset_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some((ca, cb, r, l, result)) = cache.as_ref()
            && ca == a
            && cb == b
            && *r == rate
            && *l == level
        {
            return result.clone();
        }
        let result = audivane_dsp_core::chain::AbChain::estimate_gains(a, b, rate, level);
        *cache = Some((a.clone(), b.clone(), rate, level, result.clone()));
        result
    }

    // ------------------------------------------------------------ status

    pub fn status(&self) -> Status {
        let (cfg, (preset, part_presets), (applied, revision)) = {
            let guard = self.lock_config();
            let preset = self.preset_state(&guard);
            let levels = self.level_state_locked(&guard);
            (guard.clone(), preset, levels)
        };
        let snap = self.engine.snapshot();
        let facts = self.facts();

        let graph_rate = if snap.graph_rate > 0 {
            snap.graph_rate
        } else {
            cfg.rate.default_rate
        };
        let rate_ratio = if snap.graph_rate > 0 {
            snap.ratio
        } else if cfg.dsp_enabled {
            resolve_rate_ratio(cfg.chain.upsampler.target, graph_rate)
        } else {
            RateRatio::ONE
        };
        let ratio = rate_ratio.as_integer().unwrap_or(0);
        let output_rate = if snap.output_rate > 0 {
            snap.output_rate
        } else {
            rate_ratio.output_rate(graph_rate).unwrap_or(graph_rate)
        };
        let quantum = if snap.quantum > 0 {
            snap.quantum
        } else {
            cfg.quantum
        };
        let device = if snap.device.is_empty() {
            cfg.device.clone()
        } else {
            snap.device.clone()
        };

        let active_sink = snap.active_sink.or_else(|| {
            [SinkChoice::Dsp, SinkChoice::Direct]
                .into_iter()
                .find(|choice| !facts.streams_on(*choice).is_empty())
        });

        let streams: Vec<StreamView> = match active_sink {
            Some(choice) => facts
                .streams_on(choice)
                .into_iter()
                .map(|stream| StreamView {
                    node: stream.node,
                    rate: stream.rate,
                    channels: stream.channels,
                    format: stream.format.clone(),
                    volume: stream.volume,
                    muted: stream.muted,
                })
                .collect(),
            None => Vec::new(),
        };
        let sink_view = active_sink
            .and_then(|choice| facts.sink(choice))
            .map(|sink| SinkView {
                node: sink.node,
                volume: sink.volume,
                muted: sink.muted,
                channels: sink.channels,
            });
        // MPRIS sometimes knows the real source precision; the negotiated
        // PipeWire format only describes the container (S32LE/F32LE). Only a
        // player that is actually playing describes what is on the sink: a
        // paused "current player" may be a different app entirely.
        let native_bits = self
            .now_playing()
            .filter(|now| now.status == "Playing")
            .and_then(|now| now.native_bits);
        let passthrough: Passthrough =
            passthrough::compute(&snap, &streams, sink_view.as_ref(), native_bits);

        let overload_ab_off = cfg.ab.is_some() && self.lock_overload().plan(&cfg).ab.is_none();
        let ab = if overload_ab_off {
            Some(self.ab_status())
        } else {
            snap.ab
                .clone()
                .map(|status| self.decorate_ab(status, &cfg))
                .or_else(|| {
                    cfg.ab
                        .as_ref()
                        .map(|_| self.ab_status())
                        .filter(|status| status.enabled)
                })
        };
        let volume = Some(self.volume_status(&cfg, applied, revision));

        // One lock: a second one in the literal below would deadlock (the
        // first guard lives to the end of the statement).
        let upsampler_rates = self.lock_rate_plan().rows.clone();
        Status {
            version: env!("CARGO_PKG_VERSION").to_string(),
            state: snap.state,
            device,
            // A queued config write that failed is a recent recoverable error:
            // report it when the engine has nothing to say.
            error: snap.error.clone().or_else(|| self.persist_error()),
            format: snap.format,
            graph_rate,
            output_rate,
            ratio,
            rate_ratio,
            quantum,
            latency_ms: snap.latency_ms,
            filter_latency_ms: snap.filter_latency_ms,
            conv_latency_ms: snap.conv_latency_ms,
            xruns: snap.xruns,
            dsp_load: snap.dsp_load,
            dsp_enabled: cfg.dsp_enabled,
            eq_tilt: self.tilt_status(&cfg.chain, graph_rate),
            eq_tilt_b: cfg
                .ab
                .as_ref()
                .and_then(|ab| self.tilt_status(&ab.chain_b, graph_rate)),
            active_sink,
            passthrough,
            ab,
            preset,
            part_presets,
            volume,
            device_caps: snap.device_caps.clone(),
            effective_rates: snap.effective_rates.clone(),
            direct_rates: snap.direct_rates.clone(),
            playable_rates: snap.playable_rates.clone(),
            bypass_resampler: passthrough::bypass_resampler_status(
                snap.bypass_resampler.clone(),
                &cfg,
                &snap,
                &streams,
                active_sink,
                |rate| {
                    let cfg = &cfg;
                    let rates = audivane_io_pw::lifecycle::DeviceRates::playable(
                        snap.device_caps.as_ref(),
                        &cfg.formats,
                    );
                    audivane_io_pw::DesiredDspConfig {
                        dsp_enabled: cfg.dsp_enabled,
                        bypass: audivane_io_pw::BypassLeg {
                            engine: true,
                            buildable: crate::control::config::bypass_buildable_rate_pairs(cfg),
                            allowed: audivane_io_pw::BypassLeg::allowed_bits(
                                &cfg.rate.allowed_rates,
                            ),
                        },
                        ..audivane_io_pw::DesiredDspConfig::default()
                    }
                    .bypass_landing(rate, rates)
                },
            ),
            effective_default_rate: snap.effective_default_rate,
            dither_bits: snap.dither_bits,
            // Only while it can be held: playing, DSP on, the limiter on (the
            // bank keeps the last block's value through a pause).
            limiter_held_db: if snap.state == EngineState::Playing
                && cfg.dsp_enabled
                && cfg.chain.limiter.mode != audivane_protocol::LimiterMode::Off
            {
                self.engine.limiter_held_db()
            } else {
                0.0
            },
            normal_output: facts.normal_output.clone(),
            overload: self.overload_report(),
            upsampler_fallback_now: (snap.state == EngineState::Playing
                && snap.active_sink != Some(SinkChoice::Direct)
                && cfg.dsp_enabled)
                .then(|| {
                    upsampler_rates
                        .iter()
                        .find(|r| r.source_rate == snap.graph_rate && r.fallback.is_some())
                        .cloned()
                })
                .flatten(),
            upsampler_rates,
            rt_load_max: snap.rt_load_max,
            varispeed: Some(
                snap.varispeed
                    .unwrap_or_else(|| VarispeedStatus::from_config(&cfg.varispeed)),
            ),
            limits: Some(super::cpu_budget::engine_limits()),
            waiting_for_device: snap.waiting_for_device,
            outputs: self.outputs_status(&cfg),
            conv_warnings: self.engine.conv_warnings(),
            calibration: self.calibration_status(&cfg),
            unresolved_devices: self.unresolved_devices(),
        }
    }

    /// The loaded room calibration as it plays now.
    fn calibration_status(
        &self,
        cfg: &EngineConfig,
    ) -> Option<audivane_protocol::ipc::CalibrationStatus> {
        cfg.calibration.as_ref()?;
        let snap = self.engine.snapshot();
        let playing = snap.state == EngineState::Playing;
        super::calibration::status(
            cfg,
            if playing { snap.graph_rate } else { 0 },
            if playing { snap.output_rate } else { 0 },
            &self.outputs_status(cfg),
        )
    }

    /// `bundle_load`: read and check the manifest, then install it as one
    /// config edit (validated, CPU-admitted, applied, persisted); on any
    /// error nothing changes. The reply is the calibration's status, the
    /// load's own warnings added to `warnings`.
    fn bundle_load(&self, path: &str) -> Result<Value, RpcError> {
        let _mutation = self.lock_mutation();
        let current = self.config();
        let loaded = super::calibration::read(path, &current, crate::engine::DEFAULT_CHANNELS)
            .map_err(|e| invalid_params(format!("bundle_load: {e}")))?;
        let cal = loaded.calibration.clone();
        let master_db = self.master_level_db(&current);
        let cfg = self
            .edit_locked(|cfg| super::calibration::install(cfg, cal, master_db), None)
            .map_err(|e| RpcError {
                code: e.code,
                message: format!("bundle_load: nothing changed: {}", e.message),
            })?;
        log::info!(
            "calibration: loaded {:?} from {}",
            loaded.calibration.label,
            loaded.calibration.path.display()
        );
        // The reply is the status, with what the load found (the manifest's
        // notes, the convolution guards' warnings) after the rate warnings
        // in `warnings`: `notes` is the manifest's own text.
        let mut status = self.calibration_status(&cfg);
        if let Some(s) = &mut status {
            s.warnings.extend(loaded.notes);
            s.warnings
                .extend(self.engine.conv_warnings().into_iter().map(|w| w.message));
        }
        to_value(&status)
    }

    /// `bundle_unload`: remove exactly what the loaded calibration
    /// installed; what is no longer valid to put back is skipped, in the
    /// returned warnings. Without one, `BAD_STATE`.
    fn bundle_unload(&self) -> Result<Vec<String>, RpcError> {
        let _mutation = self.lock_mutation();
        if self.config().calibration.is_none() {
            return Err(RpcError {
                code: RpcError::BAD_STATE,
                message: "no room calibration is loaded".into(),
            });
        }
        let mut warnings = Vec::new();
        self.edit_locked(
            |cfg| {
                warnings = super::calibration::uninstall(cfg).unwrap_or_default();
            },
            None,
        )?;
        for w in &warnings {
            log::warn!("calibration: {w}");
        }
        log::info!("calibration: unloaded");
        Ok(warnings)
    }

    /// `bundle_gain`: one calibration part's level (its item's gain; `None`:
    /// the manifest's), as one config edit: the boost cap and the rest of
    /// the guards hold it, and on an error nothing changes. The reply is the
    /// calibration's status, the guards' warnings in `notes`.
    fn bundle_gain(&self, device: Option<&str>, gain_db: Option<f64>) -> Result<Value, RpcError> {
        let _mutation = self.lock_mutation();
        let mut current = self.config();
        if current.calibration.is_none() {
            return Err(RpcError {
                code: RpcError::BAD_STATE,
                message: "no room calibration is loaded".into(),
            });
        }
        super::calibration::set_gain(&mut current, device, gain_db)
            .map_err(|e| invalid_params(format!("bundle_gain: {e}")))?;
        let cal = current.calibration.clone();
        let cfg = self
            .edit_locked(
                |cfg| {
                    cfg.calibration = cal;
                    super::calibration::normalize(cfg);
                },
                None,
            )
            .map_err(|e| RpcError {
                code: e.code,
                message: format!("bundle_gain: nothing changed: {}", e.message),
            })?;
        let notes = self
            .engine
            .conv_warnings()
            .into_iter()
            .map(|w| w.message)
            .collect();
        with_notes(to_value(&self.calibration_status(&cfg))?, notes)
    }

    /// `bundle_ab`: A/B of calibrations on the master. Side B is chain A
    /// with the bundle at `path` in place of the loaded calibration
    /// (`None`: without one), the followers as they are (they keep the
    /// loaded calibration on both sides).
    fn bundle_ab(&self, path: Option<&str>) -> Result<(AbStatus, Vec<String>), RpcError> {
        let loaded = match path {
            Some(p) => Some(
                super::calibration::read(p, &self.config(), crate::engine::DEFAULT_CHANNELS)
                    .map_err(|e| invalid_params(format!("bundle_ab: {e}")))?,
            ),
            None => None,
        };
        let side = super::calibration::ab_side(loaded.as_ref());
        let label = match &side.label {
            Some(l) => format!("Calibration: {l}"),
            None => "No calibration".to_string(),
        };
        let mut ab = AbConfig::new(audivane_protocol::ipc::AbB::Patch(Value::Object(
            serde_json::Map::new(),
        )));
        ab.label = Some(label);
        ab.calibration_b = Some(side);
        let (status, mut notes) = self.set_ab(Some(ab))?;
        if let Some(l) = &loaded {
            notes.extend(l.notes.iter().cloned());
            if !l.calibration.followers.is_empty() {
                notes.push(
                    "A/B switches the master only: the followers play the loaded calibration                      on both sides"
                        .into(),
                );
            }
        }
        Ok((status, notes))
    }

    /// A mutation's reply: `null`, or `{"notes": [..]}` with the
    /// convolution guards' warnings of the config it applied.
    fn conv_notes_reply(&self) -> Result<Value, RpcError> {
        let notes: Vec<Value> = self
            .engine
            .conv_warnings()
            .into_iter()
            .map(|w| Value::String(w.message))
            .collect();
        if notes.is_empty() {
            return Ok(Value::Null);
        }
        let mut object = serde_json::Map::new();
        object.insert("notes".into(), Value::Array(notes));
        Ok(Value::Object(object))
    }

    /// The active presets (global, then each part's) and whether chain A
    /// still matches them on the parts they hold.
    fn preset_state(&self, cfg: &EngineConfig) -> (Option<PresetState>, Vec<PartPresetState>) {
        let cache = self.lock_presets();
        let global = cfg.active_preset.as_ref().and_then(|name| {
            let preset = cache.get(None).filter(|preset| &preset.name == name)?;
            Some(PresetState {
                name: name.clone(),
                modified: !preset.matches(&cfg.chain),
            })
        });
        let parts = cfg
            .active_part_presets
            .iter()
            .filter_map(|(part, name)| {
                let preset = cache
                    .get(Some(*part))
                    .filter(|preset| &preset.name == name)?;
                Some(PartPresetState {
                    part: *part,
                    name: name.clone(),
                    modified: !preset.matches(&cfg.chain),
                })
            })
            .collect();
        (global, parts)
    }

    /// `applied`: the engine's volume, read together with `revision` (see
    /// [`Service::level_state`]).
    fn volume_status(
        &self,
        cfg: &EngineConfig,
        applied: Option<VolumeStatus>,
        revision: u64,
    ) -> VolumeStatus {
        let mut status = applied.unwrap_or_else(|| VolumeStatus::from_config(&cfg.volume));
        (status.blocked, status.last_blocked) = self.volume_log.snapshot();
        status.instance = Some(self.instance.clone());
        status.revision = Some(revision);
        status
    }

    /// Bump the level revision when `cfg`'s level state differs from the
    /// last one noted. Called with the config lock held, right after a
    /// commit.
    fn note_levels(&self, cfg: &EngineConfig) {
        let mut l = self.levels.lock().unwrap_or_else(|e| e.into_inner());
        let now = levels_of(cfg);
        if now != l.levels {
            l.levels = now;
            l.revision += 1;
        }
    }

    /// The current [`VolumeStatus::revision`].
    #[cfg(test)]
    fn level_revision(&self) -> u64 {
        self.level_state().1
    }

    /// The engine's applied volume and the [`VolumeStatus::revision`] that
    /// goes with it. See [`Service::level_state_locked`].
    fn level_state(&self) -> (Option<VolumeStatus>, u64) {
        let config = self.lock_config();
        self.level_state_locked(&config)
    }

    /// The revision counts changes of the configured level (bumped at each
    /// commit, [`Service::note_levels`]) and of the level the engine reports as
    /// applied, which can move on its own (a staged hardware raise landing, a
    /// mixer readback on start-up or a reopen). The applied level is read here
    /// under the same locks the guarded write checks with, and a change of it
    /// bumps the revision first: one revision always goes with one applied
    /// level, so a write computed from an interim status is refused. Called
    /// with the config lock held.
    fn level_state_locked(&self, _config: &EngineConfig) -> (Option<VolumeStatus>, u64) {
        let mut l = self.levels.lock().unwrap_or_else(|e| e.into_inner());
        let (applied, generation) = self.engine.volume_state();
        if generation != l.applied_generation {
            l.applied_generation = generation;
            l.revision += 1;
        }
        (applied, l.revision)
    }

    pub fn streams(&self) -> Vec<StreamInfo> {
        self.facts()
            .streams
            .into_iter()
            .map(|stream| StreamInfo {
                node: stream.node,
                app_name: stream.app_name,
                binary: stream.binary,
                media_name: stream.media_name,
                rate: stream.rate,
                channels: stream.channels,
                format: stream.format,
                sink: stream.sink,
                volume: stream.volume,
                muted: stream.muted,
                volume_db: stream.volume_db,
            })
            .collect()
    }

    /// `set_stream_volume`: one app stream's level and/or mute, through the
    /// graph watcher (which applies the stream cap and the guard).
    fn set_stream_volume(
        &self,
        node: u32,
        volume_db: Option<f64>,
        muted: Option<bool>,
    ) -> Result<audivane_protocol::ipc::StreamVolume, RpcError> {
        if let Some(db) = volume_db {
            validate_stream_volume_db(db)
                .map_err(invalid_params)
                .inspect_err(|err| log::info!("set_stream_volume refused: {}", err.message))?;
        }
        let Some(graph) = &self.graph else {
            return Err(RpcError {
                code: RpcError::BAD_STATE,
                message: "the PipeWire graph is not available; cannot set stream volumes".into(),
            });
        };
        graph
            .set_stream_volume(node, volume_db, muted)
            .inspect_err(|err| {
                log::info!(
                    "set_stream_volume for stream {node} refused: {}",
                    err.message
                )
            })
    }

    pub fn now_playing(&self) -> Option<NowPlaying> {
        self.mpris.as_ref().and_then(|m| m.now_playing())
    }

    pub fn meters(&self) -> Meters {
        self.engine.meters()
    }

    /// Latest live spectrum frame, if the engine has published one.
    pub fn spectrum(&self) -> Option<Spectrum> {
        self.engine.spectrum()
    }

    pub fn spectrum_generation(&self) -> u64 {
        self.engine.spectrum_generation()
    }

    /// Ask the engine to copy spectrum taps (true while a client subscribes).
    pub fn set_spectrum_enabled(&self, enabled: bool) {
        self.engine.set_spectrum_enabled(enabled);
    }

    // ------------------------------------------------------------ routing

    fn route(
        &self,
        target: audivane_protocol::ipc::RouteTarget,
        sink: SinkChoice,
    ) -> Result<(), RpcError> {
        use audivane_protocol::ipc::RouteTarget;
        let Some(graph) = &self.graph else {
            return Err(RpcError {
                code: RpcError::BAD_STATE,
                message: "the PipeWire graph is not available; cannot route streams".into(),
            });
        };
        // A watcher whose PipeWire loop has stopped would accept the command
        // and drop it; report that instead of claiming the stream moved.
        if !graph.is_running() {
            return Err(RpcError {
                code: RpcError::BAD_STATE,
                message: "the PipeWire graph watcher has stopped; cannot route streams".into(),
            });
        }
        if !graph.has_metadata() {
            return Err(RpcError {
                code: RpcError::BAD_STATE,
                message: "the PipeWire `default` metadata is not available (is WirePlumber \
                          running?); cannot route streams"
                    .into(),
            });
        }
        let facts = graph.facts();
        if sink != SinkChoice::Default && facts.sink(sink).is_none() {
            return Err(RpcError {
                code: RpcError::BAD_STATE,
                message: format!(
                    "the {} sink is not registered (is the device released?)",
                    match sink {
                        SinkChoice::Dsp => "Audivane DSP",
                        SinkChoice::Direct => "Audivane Direct",
                        SinkChoice::Default => unreachable!(),
                    }
                ),
            });
        }
        match target {
            RouteTarget::Node(node) => {
                if !facts.streams.iter().any(|s| s.node == node) {
                    return Err(invalid_params(format!(
                        "no stream with node id {node} (see `avdsp ctl streams`)"
                    )));
                }
                graph.route_node(node, sink);
            }
            RouteTarget::App(app) => {
                let nodes: Vec<u32> = facts
                    .streams
                    .iter()
                    .filter(|stream| {
                        [stream.app_name.as_deref(), stream.binary.as_deref()]
                            .into_iter()
                            .flatten()
                            .any(|value| value.eq_ignore_ascii_case(app.trim()))
                    })
                    .map(|stream| stream.node)
                    .collect();
                if nodes.is_empty() {
                    return Err(invalid_params(format!(
                        "no playing streams for app {app:?} (see `avdsp ctl streams`)"
                    )));
                }
                for node in nodes {
                    graph.route_node(node, sink);
                }
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------ sink volume

    /// Turning `sink_volume: follow` on hands the sink volume to the engine,
    /// and PipeWire stops attenuating: the engine takes the quieter of its
    /// own level and what the sinks were set to, so the switch never gets
    /// louder.
    fn adopt_sink_levels(&self, old: &EngineConfig, next: &mut EngineConfig) {
        if old.volume.sink_volume == SinkVolumePolicy::Follow
            || next.volume.sink_volume != SinkVolumePolicy::Follow
        {
            return;
        }
        let facts = self.facts();
        let sinks = listened_sink_level(&facts, self.engine.snapshot().active_sink);
        if let Some(note) = adopt_levels(&mut next.volume, &sinks) {
            log::info!("sink volume follow: {note}");
        }
    }

    fn lock_sink_guard(&self) -> MutexGuard<'_, LevelGuard> {
        self.sink_guard.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The log the graph watcher's stream guard records into as well.
    pub fn volume_log(&self) -> Arc<VolumeEventLog> {
        Arc::clone(&self.volume_log)
    }

    /// A volume change made on one of our sinks from outside (desktop, media
    /// keys, `wpctl`) while `sink_volume` is `follow`: through the ceiling
    /// and the guard into the config, which the engine then shows on the
    /// sink again.
    pub fn on_sink_volume(&self, request: SinkVolumeRequest) {
        let Ok(_active) = self.enter_dispatch() else {
            return;
        };
        let _mutation = self.lock_mutation();
        let cfg = self.config();
        if cfg.volume.sink_volume != SinkVolumePolicy::Follow {
            return;
        }
        let now = std::time::Instant::now();
        let mut next = cfg.volume;
        // The sink must show what is in force when that is not the request.
        let mut differs = false;
        if let Some(muted) = request.muted {
            next.muted = muted;
        }
        let source = VolumeSource::Sink { sink: request.sink };
        match request.sink {
            SinkChoice::Direct => {
                if let Some(v) = request.volume_db
                    && v.abs() > EPS_DB
                {
                    // The driver already put it back to unity.
                    self.volume_log
                        .record(source, v, 0.0, BlockReason::DirectBitPerfect);
                }
            }
            _ => {
                let locked = cfg.volume.guard.mode == GuardMode::Lock;
                if let Some(b) = request.balance_db
                    && (b - cfg.volume.balance_db).abs() > EPS_DB
                {
                    if locked {
                        differs = true;
                    } else {
                        next.balance_db = b;
                    }
                }
                if let Some(v) = request.volume_db {
                    let decision = self.lock_sink_guard().decide(
                        &cfg.volume.guard,
                        volume_guard::ceiling_db(&cfg.volume),
                        cfg.volume.volume_db,
                        v,
                        now,
                    );
                    next.volume_db = decision.apply_db;
                    if let Some(reason) = decision.blocked {
                        self.volume_log.record(source, v, decision.apply_db, reason);
                        differs = true;
                    }
                }
            }
        }
        if next != cfg.volume {
            if let Err(err) = self.edit_locked(|c| c.volume = next, None) {
                log::warn!(
                    "sink volume request {request:?} not applied: {}",
                    err.message
                );
                self.engine.refresh_sink_volume();
            }
        } else if differs {
            self.engine.refresh_sink_volume();
        }
    }

    /// One step of a guarded rise. Returns whether one is still pending.
    pub fn volume_tick(&self) -> bool {
        let Ok(_active) = self.enter_dispatch() else {
            return false;
        };
        let _mutation = self.lock_mutation();
        let cfg = self.config();
        let mut guard = self.lock_sink_guard();
        if cfg.volume.sink_volume != SinkVolumePolicy::Follow
            || cfg.volume.guard.mode != GuardMode::Limit
        {
            guard.cancel();
            return false;
        }
        let hardware = self.engine.snapshot().volume.is_some_and(|v| v.hardware);
        let Some(next_db) = guard.tick(
            &cfg.volume.guard,
            cfg.volume.volume_db,
            volume_guard::slew_step_db(cfg.volume.mode, hardware),
            std::time::Instant::now(),
        ) else {
            return guard.is_slewing();
        };
        let pending = guard.is_slewing();
        drop(guard);
        if let Err(err) = self.edit_locked(|c| c.volume.volume_db = next_db, None) {
            log::warn!("guarded volume step not applied: {}", err.message);
            self.lock_sink_guard().cancel();
            return false;
        }
        pending
    }

    pub fn volume_slewing(&self) -> bool {
        self.lock_sink_guard().is_slewing()
    }

    /// Convenience for tests and the daemon start-up path.
    pub fn refresh(&self) {
        let _mutation = self.lock_mutation();
        let cfg = self.config();
        self.apply_routing_locked(&cfg);
    }
}

/// `value` with `notes` added (additive: clients that only want the plain
/// reply ignore it); unchanged without notes or when not an object.
/// The fields a [`Call::SetFollower`] edit sets (`None`: kept).
struct FollowerEdit {
    enabled: Option<bool>,
    trim_ms: Option<f64>,
    volume_db: Option<f64>,
    output_channels: Option<audivane_protocol::ipc::OutputChannels>,
    follow_volume: Option<bool>,
    chain_patch: Option<Value>,
    period_ms: Option<f64>,
}

fn with_notes(mut value: Value, notes: Vec<String>) -> Result<Value, RpcError> {
    if !notes.is_empty()
        && let Some(object) = value.as_object_mut()
    {
        object.insert(
            "notes".into(),
            Value::Array(notes.into_iter().map(Value::String).collect()),
        );
    }
    Ok(value)
}

/// True when `next` differs from `old` only in the volume level, balance or
/// mute.
fn level_only_change(old: &EngineConfig, next: &EngineConfig) -> bool {
    let mut levels = old.clone();
    levels.volume.volume_db = next.volume.volume_db;
    levels.volume.balance_db = next.volume.balance_db;
    levels.volume.muted = next.volume.muted;
    levels == *next
}

/// The level of the sink being listened through (the one feeding the DAC,
/// else the DSP sink), for [`adopt_levels`]: the other sink's slider (a
/// muted Direct sink, say) is not what the user hears.
fn listened_sink_level(
    facts: &crate::control::graph::Facts,
    active: Option<SinkChoice>,
) -> Vec<(Option<f64>, bool)> {
    let choice = active.unwrap_or(SinkChoice::Dsp);
    facts
        .sink(choice)
        .map(|s| vec![(s.volume, s.muted)])
        .unwrap_or_default()
}

/// Lower `volume` to the quietest of `sinks` (`(loudest channel linear,
/// muted)`), and mute it when one of them is muted.
fn adopt_levels(volume: &mut VolumeConfig, sinks: &[(Option<f64>, bool)]) -> Option<String> {
    let mut notes = Vec::new();
    for &(level, muted) in sinks {
        if let Some(v) = level {
            let db = if v > 0.0 {
                ((20.0 * v.log10()) * 1000.0).round() / 1000.0
            } else {
                audivane_protocol::ipc::VOLUME_MIN_DB
            }
            .max(audivane_protocol::ipc::VOLUME_MIN_DB);
            if db < volume.volume_db - EPS_DB {
                notes.push(format!(
                    "volume {:.2} -> {db:.2} dB (the sink's level)",
                    volume.volume_db
                ));
                volume.volume_db = db;
            }
        }
        if muted && !volume.muted {
            notes.push("muted (the sink was muted)".into());
            volume.muted = true;
        }
    }
    (!notes.is_empty()).then(|| notes.join(", "))
}

/// The level state a [`VolumeStatus::revision`] counts changes of: the
/// effective volume (after the range and the ceiling), balance and mute.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Levels {
    volume_db: f64,
    balance_db: f64,
    muted: bool,
}

struct LevelRevision {
    levels: Levels,
    /// [`Engine::volume_state`]'s generation the revision last counted.
    applied_generation: u64,
    revision: u64,
}

fn levels_of(cfg: &EngineConfig) -> Levels {
    Levels {
        volume_db: crate::engine::volume::target_db(&cfg.volume),
        balance_db: cfg.volume.balance_db,
        muted: cfg.volume.muted,
    }
}

/// A [`VolumeStatus::instance`] id: 128 bits from the OS's random source,
/// in hex. Unique per daemon process (and per [`Service`] within one).
fn new_instance_id() -> String {
    use std::io::Read;
    let mut bytes = [0u8; 16];
    if let Err(err) = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes))
    {
        // Never expected on Linux. std's hasher keys come from the OS's
        // random source too (getrandom), freshly per RandomState.
        log::warn!("cannot read /dev/urandom ({err}); using the hasher's random keys");
        use std::hash::{BuildHasher, Hasher};
        for half in bytes.chunks_mut(8) {
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_u8(0);
            half.copy_from_slice(&h.finish().to_le_bytes());
        }
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn to_value<T: serde::Serialize>(value: &T) -> Result<Value, RpcError> {
    serde_json::to_value(value).map_err(|err| RpcError {
        code: RpcError::INTERNAL,
        message: format!("cannot serialize reply: {err}"),
    })
}

/// The target that resolves to exactly `ratio` at `source_rate`: off at
/// 1:1, a multiple for an integer ratio, else the output rate.
fn landing_target(ratio: RateRatio, source_rate: u32) -> audivane_protocol::RateTarget {
    use audivane_protocol::RateTarget;
    if ratio.is_unity() {
        RateTarget::Off
    } else if let Some(k) = ratio.as_integer() {
        RateTarget::Ratio(k)
    } else {
        ratio
            .output_rate(source_rate)
            .map_or(RateTarget::Off, RateTarget::Rate)
    }
}

/// A zoom window must be finite, inside 0 .. output Nyquist and non-empty.
fn validate_response_window(
    window: &ResponseWindowRequest,
    nyquist_out: f64,
) -> Result<(), String> {
    let (lo, hi) = (window.f_lo_hz, window.f_hi_hz);
    if !lo.is_finite() || !hi.is_finite() {
        return Err("window frequencies must be finite".into());
    }
    if lo < 0.0 || hi > nyquist_out {
        return Err(format!(
            "window must lie within 0 .. {nyquist_out} Hz (got {lo} .. {hi} Hz)"
        ));
    }
    if lo >= hi {
        return Err(format!(
            "window needs f_lo_hz < f_hi_hz (got {lo} .. {hi} Hz)"
        ));
    }
    Ok(())
}

/// A `set_stream_volume` level: `-120..=0` dB (a stream is never set
/// above 100 %).
fn validate_stream_volume_db(db: f64) -> Result<(), String> {
    use audivane_protocol::ipc::{VOLUME_MAX_DB, VOLUME_MIN_DB};
    if db.is_finite() && (VOLUME_MIN_DB..=VOLUME_MAX_DB).contains(&db) {
        Ok(())
    } else {
        Err(format!(
            "volume_db must be between {VOLUME_MIN_DB} and {VOLUME_MAX_DB} dB (got {db})"
        ))
    }
}

fn invalid_params(message: impl Into<String>) -> RpcError {
    RpcError {
        code: RpcError::INVALID_PARAMS,
        message: message.into(),
    }
}

fn engine_error(err: EngineError) -> RpcError {
    RpcError {
        code: err.code,
        message: err.message,
    }
}

fn preset_error(err: PresetError) -> RpcError {
    match err {
        PresetError::NotFound(name) => RpcError {
            code: RpcError::INVALID_PARAMS,
            message: format!("no preset named {name:?} (see `avdsp ctl preset list`)"),
        },
        PresetError::Exists(name) => RpcError {
            code: RpcError::BAD_STATE,
            message: format!(
                "a preset named {name:?} already exists; pass overwrite to replace it"
            ),
        },
        PresetError::InvalidName(message) => invalid_params(message),
        PresetError::Io(err) => RpcError {
            code: RpcError::IO,
            message: format!("preset file: {err}"),
        },
    }
}

/// The active presets' contents, keyed by scope (`None` = global).
#[derive(Clone, Debug, Default)]
struct PresetCache {
    global: Option<Preset>,
    parts: std::collections::BTreeMap<PresetPart, Preset>,
}

impl PresetCache {
    fn get(&self, scope: Option<PresetPart>) -> Option<&Preset> {
        match scope {
            None => self.global.as_ref(),
            Some(part) => self.parts.get(&part),
        }
    }

    fn set(&mut self, scope: Option<PresetPart>, preset: Option<Preset>) {
        match (scope, preset) {
            (None, preset) => self.global = preset,
            (Some(part), Some(preset)) => {
                self.parts.insert(part, preset);
            }
            (Some(part), None) => {
                self.parts.remove(&part);
            }
        }
    }

    /// This cache matched to the names `cfg` makes active: entries whose
    /// name still matches are kept, the others read from disk (a missing or
    /// unreadable one is logged and left out).
    fn refreshed(self, store: Option<&Arc<ConfigStore>>, cfg: &EngineConfig) -> Self {
        let root = store.map(|store| PresetStore::alongside(store));
        let mut out = Self::default();
        let scopes = std::iter::once(None).chain(PresetPart::ALL.into_iter().map(Some));
        for scope in scopes {
            let Some(name) = active_name(cfg, scope) else {
                continue;
            };
            let kept = self
                .get(scope)
                .filter(|preset| &preset.name == name)
                .cloned();
            let preset = kept.or_else(|| {
                let root = root.as_ref()?;
                match root.scoped(scope).load(name) {
                    Ok(preset) => Some(preset),
                    Err(err) => {
                        log::warn!("active preset {name:?}: {err}");
                        None
                    }
                }
            });
            out.set(scope, preset);
        }
        out
    }
}

/// The active preset name of `scope` (`None` = global) in `cfg`.
fn active_name(cfg: &EngineConfig, scope: Option<PresetPart>) -> Option<&String> {
    match scope {
        None => cfg.active_preset.as_ref(),
        Some(part) => cfg.active_part_presets.get(&part),
    }
}

fn set_active_name(cfg: &mut EngineConfig, scope: Option<PresetPart>, name: Option<String>) {
    match (scope, name) {
        (None, name) => cfg.active_preset = name,
        (Some(part), Some(name)) => {
            cfg.active_part_presets.insert(part, name);
        }
        (Some(part), None) => {
            cfg.active_part_presets.remove(&part);
        }
    }
}

/// [`Service::rate_plan`]: the plan the engine holds and its status rows.
#[derive(Clone, Debug, Default)]
struct RatePlanState {
    key: Option<RatePlanKey>,
    plan: crate::api::RatePlan,
    rows: Vec<audivane_protocol::ipc::RateAdmission>,
}

/// What the per-rate plan is made from: the parts of the request it prices,
/// the device probe, the last upsampler that ran and the rates that
/// overloaded.
#[derive(Clone, Debug, PartialEq)]
struct RatePlanKey {
    dsp_enabled: bool,
    upsampler: UpsamplerConfig,
    output: audivane_protocol::OutputConfig,
    ab: Option<ChainConfig>,
    /// Chain A's convolution and, with a filter matrix enabled (it runs on
    /// the audio thread with the EQ before it,
    /// [`super::cpu_budget::matrix_load`]), its EQ: both price into the
    /// plan.
    convolution: Vec<audivane_protocol::ConvolutionConfig>,
    matrix_eq: Option<audivane_protocol::EqProfile>,
    rate: audivane_protocol::ipc::RatePolicy,
    quantum: u32,
    device: String,
    formats: Vec<audivane_protocol::SampleFormat>,
    lanes: usize,
    caps: Option<audivane_protocol::ipc::DeviceCapabilities>,
    last_good: Option<UpsamplerConfig>,
    tripped: Vec<u32>,
}

impl RatePlanKey {
    fn of(
        cfg: &EngineConfig,
        caps: Option<audivane_protocol::ipc::DeviceCapabilities>,
        last_good: Option<UpsamplerConfig>,
        tripped: Vec<u32>,
    ) -> Self {
        Self {
            dsp_enabled: cfg.dsp_enabled,
            upsampler: cfg.chain.upsampler.clone(),
            output: cfg.chain.output,
            ab: cfg.ab.as_ref().map(|ab| ab.chain_b.clone()),
            convolution: cfg.chain.convolution.clone(),
            matrix_eq: cfg
                .chain
                .convolution
                .iter()
                .any(|c| c.enabled && c.matrix.is_some())
                .then(|| cfg.chain.eq.clone())
                .flatten(),
            rate: cfg.rate.clone(),
            quantum: cfg.quantum,
            device: cfg.device.clone(),
            formats: cfg.formats.clone(),
            lanes: super::cpu_budget::lanes(cfg),
            caps,
            last_good,
            tripped,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::EngineSnapshot;
    use audivane_protocol::ipc::LevelMatch;
    use audivane_protocol::ipc::{AbSide, EngineState, Meters};
    use audivane_protocol::{ChainConfig, EqProfile, UpsamplerConfig};
    use std::sync::atomic::{AtomicU64, Ordering};

    struct MockEngine {
        config: Mutex<EngineConfig>,
        applies: AtomicU64,
        refreshes: AtomicU64,
        /// The volume as applied (`None`: not reported) and its generation.
        applied: Mutex<(Option<VolumeStatus>, u64)>,
        /// The A/B measurement epoch asked for, and hops waiting to be taken.
        measure: Mutex<(u32, Vec<audivane_dsp_core::ab_measure::AbHop>)>,
        /// The graph rate `snapshot` reports instead of the config's.
        graph_rate: Mutex<Option<u32>>,
        /// `harmonics_live_suspend` calls.
        live_suspends: AtomicU64,
        /// The probe `snapshot` reports.
        caps: Mutex<Option<audivane_protocol::ipc::DeviceCapabilities>>,
    }

    impl MockEngine {
        /// The applied level changes on its own (a staged hardware raise
        /// landing, a mixer readback on a reopen).
        fn set_applied(&self, status: VolumeStatus) {
            let mut a = self.applied.lock().unwrap();
            a.0 = Some(status);
            a.1 += 1;
        }
    }

    impl Engine for MockEngine {
        fn harmonics_live_suspend(&self) {
            self.live_suspends.fetch_add(1, Ordering::Relaxed);
        }
        fn apply(&self, cfg: &EngineConfig) -> Result<(), EngineError> {
            *self.config.lock().unwrap() = cfg.clone();
            self.applies.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
        fn ab_select(&self, side: AbSide, _crossfade_ms: f64) -> Result<AbStatus, EngineError> {
            Ok(AbStatus {
                enabled: true,
                current: Some(side),
                offset_db: -3.0,
                error: None,
                ..Default::default()
            })
        }
        fn snapshot(&self) -> EngineSnapshot {
            let cfg = self.config.lock().unwrap().clone();
            EngineSnapshot {
                state: EngineState::Ready,
                device: cfg.device.clone(),
                error: None,
                format: None,
                graph_rate: self
                    .graph_rate
                    .lock()
                    .unwrap()
                    .unwrap_or(cfg.rate.default_rate),
                output_rate: 48_000,
                ratio: RateRatio::ONE,
                quantum: cfg.quantum,
                latency_ms: 0.0,
                filter_latency_ms: 0.0,
                conv_latency_ms: 0.0,
                xruns: 0,
                dsp_load: 0.0,
                dsp_active: false,
                active_sink: None,
                ab: None,
                volume: self.applied.lock().unwrap().0.clone(),
                dsp_node: None,
                direct_node: None,
                frames_out: 0,
                chain_generation: 0,
                gaps: Default::default(),
                device_caps: self.caps.lock().unwrap().clone(),
                effective_rates: Vec::new(),
                direct_rates: Vec::new(),
                playable_rates: Vec::new(),
                effective_default_rate: None,
                dither_bits: None,
                bypass_resampler: None,
                rt_load_max: 0.0,
                varispeed: None,
                waiting_for_device: false,
            }
        }
        fn meters(&self) -> Meters {
            Meters {
                peak_db: vec![-12.0, -12.0],
                rms_db: vec![-18.0, -18.0],
                clipped: 0,
                clipped_ch: vec![0, 0],
                input_peak_db: vec![-6.0, -6.0],
                chain_peak_db: Vec::new(),
                limiter: None,
                xruns: 0,
                dsp_load: 0.0,
            }
        }
        fn selftest(&self) -> Result<audivane_protocol::ipc::SelfTestResult, EngineError> {
            Ok(audivane_protocol::ipc::SelfTestResult {
                passed: true,
                frames: 1024,
                rate: 48_000,
                format: None,
                expected_hash: "0".into(),
                actual_hash: "0".into(),
                detail: "mock".into(),
            })
        }
        fn release_device(&self) -> Result<(), EngineError> {
            Ok(())
        }
        fn acquire_device(&self) -> Result<(), EngineError> {
            Ok(())
        }
        fn shutdown(&self) {}
        fn refresh_sink_volume(&self) {
            self.refreshes.fetch_add(1, Ordering::Relaxed);
        }
        fn volume_state(&self) -> (Option<VolumeStatus>, u64) {
            self.applied.lock().unwrap().clone()
        }
        fn set_ab_measure(&self, epoch: u32) {
            self.measure.lock().unwrap().0 = epoch;
        }
        fn take_ab_hops(&self, out: &mut Vec<audivane_dsp_core::ab_measure::AbHop>) {
            out.append(&mut self.measure.lock().unwrap().1);
        }
    }

    impl MockEngine {
        /// `seconds` of hops from the audio thread for the running epoch:
        /// A at `a_lufs` (`-inf`: silence), B `b_db` louder.
        fn play(&self, seconds: f64, a_lufs: f64, b_db: f64) {
            let a = if a_lufs.is_finite() {
                10f64.powf((a_lufs + 0.691) / 10.0)
            } else {
                0.0
            };
            let mut m = self.measure.lock().unwrap();
            let epoch = m.0;
            for _ in 0..(seconds * 10.0).round() as usize {
                m.1.push(audivane_dsp_core::ab_measure::AbHop {
                    epoch,
                    a,
                    b: a * 10f64.powf(b_db / 10.0),
                });
            }
        }
    }

    fn service() -> (Arc<Service>, Arc<MockEngine>) {
        let engine = Arc::new(MockEngine {
            config: Mutex::new(EngineConfig {
                device: "hw:0,0".into(),
                ..EngineConfig::default()
            }),
            applies: AtomicU64::new(0),
            refreshes: AtomicU64::new(0),
            applied: Mutex::new((None, 0)),
            measure: Mutex::new((0, Vec::new())),
            graph_rate: Mutex::new(None),
            live_suspends: AtomicU64::new(0),
            caps: Mutex::new(None),
        });
        let svc = Service::new(
            Arc::clone(&engine) as Arc<dyn Engine>,
            engine.config.lock().unwrap().clone(),
            None,
            None,
            None,
            ShutdownFlag::new(),
        );
        (svc, engine)
    }

    /// `conv_response` measures at the engine's graph rate; side B needs A/B.
    #[test]
    fn conv_response_reads_the_graph_rate_and_checks_its_side() {
        let (service, engine) = service();
        *engine.graph_rate.lock().unwrap() = Some(96_000);
        let r: audivane_protocol::ipc::ConvResponse =
            serde_json::from_value(value(service.dispatch(Call::ConvResponse {
                side: None,
                device: None,
            })))
            .unwrap();
        assert_eq!(r.rate_hz, 96_000);
        assert_eq!(r.freqs_hz.len(), 241);
        assert!(r.items.is_empty() && r.outputs.is_empty());
        assert_eq!(r.combined_taps, 0);
        let err = service
            .dispatch(Call::ConvResponse {
                side: Some(AbSide::B),
                device: None,
            })
            .unwrap_err();
        assert_eq!(err.code, RpcError::INVALID_PARAMS);
    }

    /// The per-rate plan is redone for chain A's convolution, and for its
    /// EQ while a filter matrix runs (both cost the audio thread), not for
    /// an EQ edit without one.
    #[test]
    fn the_rate_plan_key_follows_chain_a_matrix_and_its_eq() {
        let key = |cfg: &EngineConfig| RatePlanKey::of(cfg, None, None, Vec::new());
        let base = EngineConfig::default();
        let mut eq_only = base.clone();
        eq_only.chain.eq = Some(EqProfile {
            preamp_db: -3.0,
            ..Default::default()
        });
        assert_eq!(key(&base), key(&eq_only));
        let mut matrix = base.clone();
        matrix.chain.convolution = vec![audivane_protocol::ConvolutionConfig {
            name: None,
            enabled: true,
            ir_path: Default::default(),
            gain_db: 0.0,
            matrix: Some(vec![audivane_protocol::MatrixEntry {
                input: 1,
                output: 0,
                ir_path: "/irs/x.wav".into(),
                channel: None,
                gain_db: 0.0,
            }]),
            calibration: false,
        }];
        assert_ne!(key(&base), key(&matrix));
        let mut matrix_eq = matrix.clone();
        matrix_eq.chain.eq = eq_only.chain.eq.clone();
        assert_ne!(key(&matrix), key(&matrix_eq));
    }

    /// Identical DACs: the outputs follow their USB ports when the names
    /// swap, are held when both moved, and `assign_devices` settles it.
    #[test]
    fn outputs_follow_their_dacs_across_a_name_swap() {
        use audivane_io_alsa::identity::testing::FakeTree;
        let (service, engine) = service();
        let t = FakeTree::new("service-swap");
        let usb = "2522:0007";
        t.usb(1, "USB", usb, "1.4.3", None);
        t.usb(2, "USB_1", usb, "1.4.1.4", None);
        service.set_identity_roots(t.roots.clone());
        let mut cfg = service.config();
        cfg.device = "hw:CARD=USB,DEV=0".into();
        let mut sub = FollowerConfig::new("hw:CARD=USB_1,DEV=0");
        sub.output_channels = Some(audivane_protocol::ipc::OutputChannels([2, 3]));
        sub.follow_volume = true;
        sub.trim_ms = 1.5;
        cfg.outputs.followers = vec![sub];
        // A client's identity is not taken: the card's is.
        cfg.device_identity = Some(audivane_protocol::ipc::DeviceIdentity {
            card: "X".into(),
            usb_id: "0000:0000".into(),
            serial: None,
            port: None,
        });
        service.dispatch(Call::SetConfig(cfg)).unwrap();
        let port = |chain: &str| Some(format!("pci-0000:00:14.0-usb-0:{chain}"));
        let c = service.config();
        assert_eq!(c.device_identity.as_ref().unwrap().port, port("1.4.3"));
        assert_eq!(
            c.outputs.followers[0].identity.as_ref().unwrap().port,
            port("1.4.1.4")
        );
        service.identity_tick();
        assert_eq!(service.config(), c, "nothing to follow");
        // What a rename must keep of the follower (all but its device and
        // identity).
        let kept = |c: &EngineConfig| {
            let f = &c.outputs.followers[0];
            (
                f.output_channels,
                f.follow_volume,
                f.follow_ref_db,
                f.trim_ms,
            )
        };
        let settings = kept(&c);
        assert_eq!(
            settings.0,
            Some(audivane_protocol::ipc::OutputChannels([2, 3]))
        );

        // A reboot probes the sub's DAC first.
        t.clear();
        t.usb(1, "USB", usb, "1.4.1.4", None);
        t.usb(2, "USB_1", usb, "1.4.3", None);
        service.identity_tick();
        let c = service.config();
        assert_eq!(c.device, "hw:CARD=USB_1,DEV=0");
        assert_eq!(c.outputs.followers[0].device, "hw:CARD=USB,DEV=0");
        assert_eq!(engine.config.lock().unwrap().device, c.device, "applied");
        assert_eq!(kept(&c), settings);
        assert!(service.status().unresolved_devices.is_empty());
        let present = |d: &str| audivane_io_alsa::device_present_gated_in(&t.roots, d);
        assert!(present("hw:CARD=USB_1,DEV=0") && present("hw:CARD=USB,DEV=0"));

        // Both moved: held, not guessed.
        t.clear();
        t.usb(1, "USB", usb, "2", None);
        t.usb(2, "USB_1", usb, "3", None);
        service.identity_tick();
        let held = service.status().unresolved_devices;
        assert_eq!(held.len(), 2);
        assert_eq!(service.config(), c);
        assert!(!present("hw:CARD=USB_1,DEV=0") && !present("hw:CARD=USB,DEV=0"));

        // The user says which is which (a swap, at once).
        let assign = |output: &str, device: &str| audivane_protocol::ipc::DeviceAssignment {
            output: output.into(),
            device: device.into(),
        };
        service
            .dispatch(Call::AssignDevices {
                assignments: vec![
                    assign("master", "hw:CARD=USB,DEV=0"),
                    assign("hw:CARD=USB,DEV=0", "hw:CARD=USB_1,DEV=0"),
                ],
            })
            .unwrap();
        let c = service.config();
        assert_eq!(c.device, "hw:CARD=USB,DEV=0");
        assert_eq!(c.device_identity.as_ref().unwrap().port, port("2"));
        assert_eq!(c.outputs.followers[0].device, "hw:CARD=USB_1,DEV=0");
        assert_eq!(
            c.outputs.followers[0].identity.as_ref().unwrap().port,
            port("3")
        );
        assert!(service.status().unresolved_devices.is_empty());
        assert_eq!(kept(&c), settings);
        assert!(present("hw:CARD=USB,DEV=0") && present("hw:CARD=USB_1,DEV=0"));
        let err = service
            .dispatch(Call::AssignDevices {
                assignments: vec![assign("master", "hw:CARD=Nope,DEV=0")],
            })
            .unwrap_err();
        assert!(
            err.message.contains("not a connected USB card"),
            "{}",
            err.message
        );
        audivane_io_alsa::identity::set_pins([]);
    }

    /// With a quantized hardware mixer the master plays another level than
    /// it asks for (−31 dB asked, −36 dB on the card): a follower's
    /// reference, its untick and its edits go by the played level, the one
    /// it tracks, so unticking keeps the level it plays at.
    #[test]
    fn follower_levels_go_by_the_masters_played_level() {
        let (service, engine) = service();
        let dev = "hw:CARD=USB,DEV=0";
        let set = |follow_volume: Option<bool>, volume_db: Option<f64>| {
            service
                .dispatch(Call::SetFollower {
                    device: dev.into(),
                    remove: false,
                    enabled: None,
                    trim_ms: None,
                    volume_db,
                    follow_volume,
                    output_channels: None,
                    chain_patch: None,
                    period_ms: None,
                })
                .unwrap();
        };
        let master = |asked: f64, played: f64| {
            service
                .dispatch(Call::SetVolume {
                    volume_db: asked,
                    balance_db: 0.0,
                    muted: false,
                })
                .unwrap();
            let status = VolumeStatus {
                volume_db: played,
                hardware: true,
                ..VolumeStatus::from_config(&service.config().volume)
            };
            let mut a = engine.applied.lock().unwrap();
            *a = (Some(status), a.1 + 1);
        };
        let fc = || service.config().outputs.followers[0].clone();
        set(None, Some(-6.0));
        master(-31.0, -36.0);
        set(Some(true), None);
        assert_eq!(fc().follow_ref_db, Some(-36.0), "the played level");
        // The master 5 dB down as played (4 dB as asked): unticked, the
        // follower keeps playing 5 dB below its −6 dB.
        master(-35.0, -41.0);
        set(Some(false), None);
        assert_eq!((fc().volume_db, fc().follow_ref_db), (-11.0, None));
        // An edit while following is the level it plays at then.
        set(Some(true), None);
        assert_eq!(fc().follow_ref_db, Some(-41.0));
        master(-33.0, -36.0);
        set(None, Some(-8.0));
        assert_eq!((fc().volume_db, fc().follow_ref_db), (-8.0, Some(-36.0)));
        let level = crate::engine::follower::followed_level_db(&fc(), -36.0);
        assert_eq!(level, -8.0);
    }

    #[test]
    fn output_channels_follow_the_device_and_are_checked() {
        use audivane_protocol::ipc::{DeviceCapabilities, OutputChannels};
        let (service, engine) = service();
        let set = |device: &str, pair: Option<[u32; 2]>| {
            service.dispatch(Call::SetDevice {
                device: device.into(),
                output_channels: pair.map(OutputChannels),
            })
        };
        let wide = "hw:CARD=USB,DEV=0";
        set(wide, Some([2, 3])).unwrap();
        let cfg = service.config();
        assert_eq!(
            (cfg.device.as_str(), cfg.output_channels),
            (wide, Some(OutputChannels([2, 3])))
        );
        assert_eq!(
            engine.config.lock().unwrap().output_channels,
            cfg.output_channels
        );
        // The same device without a pair keeps it; `[0, 1]` is no setting.
        set(wide, None).unwrap();
        assert_eq!(
            service.config().output_channels,
            Some(OutputChannels([2, 3]))
        );
        set(wide, Some([0, 1])).unwrap();
        assert_eq!(service.config().output_channels, None);
        set(wide, Some([7, 6])).unwrap();
        // Another device starts from the first two.
        set("hw:1,0", None).unwrap();
        assert_eq!(service.config().output_channels, None);
        // Not a pair: refused, nothing changes.
        set(wide, Some([7, 6])).unwrap();
        for bad in [[3, 3], [0, 64]] {
            let e = set(wide, Some(bad)).unwrap_err();
            assert!(e.message.contains("output_channels"), "{}", e.message);
        }
        assert_eq!(
            service.config().output_channels,
            Some(OutputChannels([7, 6]))
        );
        // Past the probed device's channels: refused at the edit.
        *engine.caps.lock().unwrap() = Some(DeviceCapabilities {
            device: wide.into(),
            channels: 8,
            channels_min: 8,
            channels_max: 8,
            ..Default::default()
        });
        let e = set(wide, Some([8, 9])).unwrap_err();
        assert!(
            e.message.contains("has 8 channels (0 to 7)"),
            "{}",
            e.message
        );
        set(wide, Some([4, 5])).unwrap();
        // A whole config with a bad pair is refused too.
        let mut cfg = service.config();
        cfg.output_channels = Some(OutputChannels([1, 1]));
        assert!(service.dispatch(Call::SetConfig(cfg)).is_err());
        // A follower's pair: set, kept by other edits, reset by `[0, 1]`.
        let follower = |pair: Option<[u32; 2]>, trim: Option<f64>| {
            service.dispatch(Call::SetFollower {
                device: "hw:CARD=USB_1,DEV=0".into(),
                remove: false,
                enabled: None,
                trim_ms: trim,
                volume_db: None,
                output_channels: pair.map(OutputChannels),
                follow_volume: None,
                chain_patch: None,
                period_ms: None,
            })
        };
        follower(Some([2, 3]), None).unwrap();
        follower(None, Some(1.0)).unwrap();
        let f = service.config().outputs.followers[0].clone();
        assert_eq!(
            (f.output_channels, f.trim_ms),
            (Some(OutputChannels([2, 3])), 1.0)
        );
        assert!(follower(Some([5, 5]), None).is_err());
        follower(Some([0, 1]), None).unwrap();
        assert_eq!(service.config().outputs.followers[0].output_channels, None);
    }

    #[test]
    fn a_pair_edit_is_checked_against_its_own_channel_count() {
        use audivane_protocol::SampleFormat;
        use audivane_protocol::ipc::{
            ChannelMode, DeviceCapabilities, FormatCapability, OutputChannels, RateCapability,
        };
        let (service, engine) = service();
        let wide = "hw:CARD=USB,DEV=0";
        let mut cfg = service.config();
        cfg.device = wide.into();
        cfg.output_channels = Some(OutputChannels([6, 7]));
        cfg.chain.upsampler.target = audivane_protocol::RateTarget::Rate(96_000);
        service.dispatch(Call::SetConfig(cfg.clone())).unwrap();
        // Stereo plays 96 kHz; the 8-channel mode pair 7/8 needs, 48 kHz.
        let row = |rate| RateCapability {
            rate,
            formats: vec![FormatCapability {
                format: SampleFormat::S32,
                sbits: Some(24),
            }],
        };
        let probe = DeviceCapabilities {
            device: wide.into(),
            channels: 2,
            channels_min: 2,
            channels_max: 8,
            formats: vec![SampleFormat::S32],
            modes: vec![
                ChannelMode {
                    channels: 2,
                    rates: vec![row(48_000), row(96_000)],
                },
                ChannelMode {
                    channels: 8,
                    rates: vec![row(48_000)],
                },
            ],
            ..Default::default()
        };
        // The engine reports the probe as pair 7/8 sees it.
        *engine.caps.lock().unwrap() = Some(config::caps_view(&cfg, &probe));
        let set = |pair: [u32; 2]| {
            service.dispatch(Call::SetDevice {
                device: wide.into(),
                output_channels: Some(OutputChannels(pair)),
            })
        };
        // Back to 1/2 at 96 kHz: accepted.
        set([0, 1]).unwrap();
        assert_eq!(service.config().output_channels, None);
        // 7/8 cannot play the explicit 96 kHz.
        *engine.caps.lock().unwrap() = Some(config::caps_view(&service.config(), &probe));
        let e = set([6, 7]).unwrap_err();
        assert!(e.message.contains("96000"), "{}", e.message);
    }

    #[test]
    fn set_follower_sets_its_period_and_refuses_one_out_of_range() {
        let (service, _engine) = service();
        let dev = "hw:CARD=USB,DEV=0";
        let set = |period_ms| {
            service.dispatch(Call::SetFollower {
                device: dev.into(),
                remove: false,
                enabled: None,
                trim_ms: None,
                volume_db: None,
                output_channels: None,
                follow_volume: None,
                chain_patch: None,
                period_ms,
            })
        };
        let period = || service.config().outputs.followers[0].period_ms;
        set(None).unwrap();
        assert_eq!(period(), None, "the default");
        set(Some(10.0)).unwrap();
        assert_eq!(period(), Some(10.0));
        let e = set(Some(2.0)).unwrap_err();
        assert!(
            e.message
                .contains("outputs.followers[0].period_ms must be between 5 and 50 ms"),
            "{e:?}"
        );
        assert_eq!(period(), Some(10.0), "kept");
        set(Some(0.0)).unwrap();
        assert_eq!(period(), None, "0: the default again");
    }

    #[test]
    fn set_follower_adds_edits_and_removes_through_the_config() {
        use audivane_protocol::ipc::FollowerState;
        let (service, engine) = service();
        let set = |device: &str, remove, enabled, trim_ms, volume_db| {
            service.dispatch(Call::SetFollower {
                device: device.into(),
                remove,
                enabled,
                trim_ms,
                volume_db,
                output_channels: None,
                follow_volume: None,
                chain_patch: None,
                period_ms: None,
            })
        };
        let outputs = |d: Result<Dispatch, RpcError>| match d.unwrap() {
            Dispatch::Value(v) => serde_json::from_value::<OutputsStatus>(v).unwrap(),
            other => panic!("unexpected {other:?}"),
        };
        let dev = "hw:CARD=USB,DEV=0";
        let o = outputs(set(dev, false, None, Some(1.5), None));
        assert_eq!(o.followers.len(), 1);
        assert_eq!(o.followers[0].state, FollowerState::Off);
        assert_eq!(
            (o.followers[0].trim_ms, o.followers[0].enabled),
            (1.5, true)
        );
        let applied = engine.config.lock().unwrap().outputs.clone();
        assert_eq!(applied, service.config().outputs, "applied and kept");
        // An edit keeps what it omits.
        let o = outputs(set(dev, false, Some(false), None, Some(-6.0)));
        let f = &o.followers[0];
        assert_eq!((f.trim_ms, f.enabled, f.volume_db), (1.5, false, -6.0));
        assert!(!f.follow_volume, "added by hand, it keeps its own level");
        // Following: its level against the master's now, kept as it is.
        let follow = |follow_volume: Option<bool>, volume_db: Option<f64>| {
            outputs(service.dispatch(Call::SetFollower {
                device: dev.into(),
                remove: false,
                enabled: None,
                trim_ms: None,
                volume_db,
                follow_volume,
                output_channels: None,
                chain_patch: None,
                period_ms: None,
            }))
        };
        let master = |db: f64| {
            service
                .dispatch(Call::SetVolume {
                    volume_db: db,
                    balance_db: 0.0,
                    muted: false,
                })
                .unwrap();
        };
        let fc = || service.config().outputs.followers[0].clone();
        master(-20.0);
        let o = follow(Some(true), None);
        assert!(o.followers[0].follow_volume);
        assert_eq!((fc().volume_db, fc().follow_ref_db), (-6.0, Some(-20.0)));
        // A master change leaves the config alone (the engine moves it).
        master(-26.0);
        assert_eq!((fc().volume_db, fc().follow_ref_db), (-6.0, Some(-20.0)));
        // Its level set while following: against the master's level now.
        follow(None, Some(-3.0));
        assert_eq!((fc().volume_db, fc().follow_ref_db), (-3.0, Some(-26.0)));
        // Turned off: it keeps the level it plays at.
        master(-30.0);
        let o = follow(Some(false), None);
        assert!(!o.followers[0].follow_volume);
        assert_eq!((fc().volume_db, fc().follow_ref_db), (-7.0, None));
        follow(None, Some(-6.0));
        // Refused, nothing changes.
        let e = set(dev, false, None, Some(60.0), None).unwrap_err();
        assert!(e.message.contains("trim_ms"), "{}", e.message);
        assert!(set("hw:0,0", false, None, None, None).is_err());
        assert!(set("hw:CARD=X,DEV=0", true, None, None, None).is_err());
        assert_eq!(service.config().outputs.followers[0].volume_db, -6.0);
        // The speed cap while it plays.
        let e = service
            .dispatch(Call::SetVarispeed {
                enabled: Some(true),
                speed: Some(2.0),
                ramp_ms: None,
                keep_pitch: None,
            })
            .unwrap_err();
        assert!(e.message.contains("1.996"), "{}", e.message);
        // Its own chain: kept as sent, cleared by `{}`; a patch that does
        // not apply is refused naming its path.
        let patch = |p: Value| {
            service.dispatch(Call::SetFollower {
                device: dev.into(),
                remove: false,
                enabled: None,
                trim_ms: None,
                volume_db: None,
                output_channels: None,
                follow_volume: None,
                chain_patch: Some(p),
                period_ms: None,
            })
        };
        let limit = serde_json::json!({"limiter": {"mode": "limit"}});
        outputs(patch(limit.clone()));
        assert_eq!(
            service.config().outputs.followers[0].chain_patch,
            Some(limit)
        );
        let e = patch(serde_json::json!({"output": {"bits": "x"}})).unwrap_err();
        assert!(
            e.message
                .starts_with("outputs.followers[0].chain_patch.output"),
            "{}",
            e.message
        );
        let e = patch(serde_json::json!({"limiter": {"lookahead_ms": 1000.0}})).unwrap_err();
        assert!(
            e.message.contains("outputs.followers[0].chain.limiter"),
            "{}",
            e.message
        );
        assert!(service.config().outputs.followers[0].chain_patch.is_some());
        outputs(patch(serde_json::json!({})));
        assert_eq!(service.config().outputs.followers[0].chain_patch, None);
        let o = outputs(set(dev, true, None, None, None));
        assert!(o.followers.is_empty());
        assert!(service.status().outputs.followers.is_empty());
    }

    #[test]
    fn shutdown_flag_wakes_waiters() {
        let flag = ShutdownFlag::new();
        assert!(!flag.is_requested());
        let waiter = Arc::clone(&flag);
        let handle = std::thread::spawn(move || {
            waiter.wait();
            waiter.is_requested()
        });
        assert!(!flag.wait_timeout(std::time::Duration::from_millis(20)));
        flag.request();
        assert!(flag.wait_timeout(std::time::Duration::from_millis(500)));
        assert!(handle.join().unwrap());
    }

    #[test]
    fn status_falls_back_to_config_values() {
        let (service, _) = service();
        let status = service.status();
        assert_eq!(status.device, "hw:0,0");
        assert_eq!(status.quantum, 1024);
        assert_eq!(status.ratio, 1);
        assert_eq!(status.output_rate, 48_000);
        assert!(
            status.passthrough.broken_because
                == vec![audivane_protocol::ipc::PassthroughBreak::NotPlaying]
        );
        // The limits: the library's, the process's workspace cap (the
        // floor: a test process sets none) and the CPU share.
        assert_eq!(
            status.limits,
            Some(audivane_protocol::ipc::EngineLimits {
                max_kernel_taps: 8_388_608,
                intermediate_max_taps: 2_097_152,
                design_workspace_bytes: 1536 << 20,
                cpu_share: 0.9,
            })
        );
    }

    #[test]
    fn tilt_status_reports_design_failures_from_a_cached_design() {
        use audivane_protocol::{TiltConfig, TiltPivot};
        let (service, _) = service();
        let chain = |total_db| ChainConfig {
            eq: Some(EqProfile {
                tilt: Some(TiltConfig {
                    enabled: true,
                    slope_db_per_oct: None,
                    total_db: Some(total_db),
                    pivot: TiltPivot::Start,
                    ..TiltConfig::default()
                }),
                ..EqProfile::default()
            }),
            ..ChainConfig::neutral()
        };
        let ok = service.tilt_status(&chain(-6.0), 48_000).unwrap();
        assert_eq!(ok.error, None);
        // A usable band whose design fails (here: over the total limit,
        // which config validation would normally reject first).
        let bad_chain = chain(30.0);
        assert!(tilt_status(&bad_chain, 48_000).unwrap().error.is_none());
        let bad = service.tilt_status(&bad_chain, 48_000).unwrap();
        assert!(bad.error.unwrap().contains("within +/-24 dB"));
        assert_eq!(service.tilt_design_cache.lock().unwrap().len(), 2);
        service.tilt_status(&bad_chain, 48_000).unwrap();
        assert_eq!(service.tilt_design_cache.lock().unwrap().len(), 2);
    }

    #[test]
    fn analytic_ab_offset_is_cached_and_its_errors_are_reported() {
        use audivane_protocol::ipc::{AbConfig, LevelMatch};
        use audivane_protocol::{TiltConfig, TiltPivot};
        let (service, engine) = service();
        let tilt = |start_hz, end_hz| ChainConfig {
            eq: Some(EqProfile {
                tilt: Some(TiltConfig {
                    enabled: true,
                    slope_db_per_oct: Some(1.0),
                    start_hz,
                    end_hz,
                    pivot: TiltPivot::Start,
                    ..TiltConfig::default()
                }),
                ..EqProfile::default()
            }),
            ..ChainConfig::neutral()
        };
        let set_b = |chain_b: ChainConfig| {
            service.lock_config().ab = Some(AbConfig {
                level: LevelMatch::default(),
                headroom: Default::default(),
                ..AbConfig::chain(chain_b)
            });
        };
        // Engine has no chain yet (mock snapshot reports no A/B): the offset
        // comes from the config, once per chain pair and rate.
        set_b(tilt(200.0, 2000.0));
        let first = service.ab_status();
        assert!(first.enabled && first.error.is_none());
        assert!(first.offset_db < -0.5, "{first:?}");
        assert_eq!(service.ab_status(), first);
        assert!(service.ab_offset_cache.lock().unwrap().is_some());

        // A band chain B cannot build at the stream rate is an error, not a
        // silent 0 dB offset.
        set_b(tilt(17_750.0, 20_000.0));
        engine.config.lock().unwrap().rate.default_rate = 44_100;
        let bad = service.ab_status();
        assert_eq!(bad.offset_db, 0.0);
        assert!(bad.error.unwrap().contains("at least 1/6 octave"));
    }

    #[test]
    fn tilt_status_updates_with_stream_rate_and_exposes_invalid_bands() {
        use audivane_protocol::{EqProfile, TiltConfig, TiltPivot};
        let (service, engine) = service();
        let tilt = TiltConfig {
            enabled: true,
            slope_db_per_oct: Some(1.0),
            start_hz: 200.0,
            end_hz: 20_000.0,
            pivot: TiltPivot::Start,
            ..TiltConfig::default()
        };
        service
            .dispatch(Call::SetEq(Some(EqProfile {
                tilt: Some(tilt),
                ..EqProfile::default()
            })))
            .unwrap();
        let read = || match service.dispatch(Call::Status).unwrap() {
            Dispatch::Value(v) => serde_json::from_value::<Status>(v).unwrap(),
            other => panic!("unexpected {other:?}"),
        };
        assert_eq!(read().eq_tilt.unwrap().clamp_note, None);
        engine.config.lock().unwrap().rate.default_rate = 44_100;
        let status = read().eq_tilt.unwrap();
        assert_eq!(status.sample_rate, 44_100);
        assert_eq!(status.effective_start_hz, 200.0);
        assert_eq!(status.effective_end_hz, 19_845.0);
        assert!(
            status
                .clamp_note
                .unwrap()
                .contains("20000.000 to 19845.000")
        );
        assert_eq!(status.error, None);
        engine.config.lock().unwrap().rate.default_rate = 96_000;
        let status = read().eq_tilt.unwrap();
        assert_eq!(status.effective_end_hz, 20_000.0);
        assert_eq!(status.clamp_note, None);
        assert_eq!(status.error, None);

        // Before the engine can build a requested high band at a lower rate,
        // report the invalid interpretation as an error, never as identity.
        let high = ChainConfig {
            eq: Some(EqProfile {
                tilt: Some(TiltConfig {
                    start_hz: 17_750.0,
                    end_hz: 20_000.0,
                    ..tilt
                }),
                ..EqProfile::default()
            }),
            ..ChainConfig::neutral()
        };
        let bad = tilt_status(&high, 44_100).unwrap();
        assert!(bad.clamp_note.is_some());
        assert!(bad.error.unwrap().contains("at least 1/6 octave"));
        assert!(tilt_status(&high, 96_000).unwrap().error.is_none());
        service
            .dispatch(Call::SetAb(Some(AbConfig {
                level: LevelMatch::Off,
                headroom: Default::default(),
                ..AbConfig::chain(high)
            })))
            .unwrap();
        assert!(read().eq_tilt_b.is_some());
        service.dispatch(Call::SetEq(None)).unwrap();
        assert!(read().eq_tilt.is_none());
    }

    #[test]
    fn tilt_status_spans_both_sections() {
        use audivane_protocol::{TiltConfig, TiltPivot};
        let (service, _) = service();
        let low = TiltConfig {
            enabled: true,
            slope_db_per_oct: Some(1.0),
            start_hz: 20.0,
            end_hz: 200.0,
            pivot: TiltPivot::End,
            ..TiltConfig::default()
        };
        let high = TiltConfig {
            start_hz: 4000.0,
            end_hz: 20_000.0,
            pivot: TiltPivot::Start,
            ..low
        };
        let chain = |low: Option<TiltConfig>, high: Option<TiltConfig>| ChainConfig {
            eq: Some(EqProfile {
                tilt: low,
                tilt_high: high,
                ..EqProfile::default()
            }),
            ..ChainConfig::neutral()
        };
        let both = service
            .tilt_status(&chain(Some(low), Some(high)), 44_100)
            .unwrap();
        assert_eq!(both.effective_start_hz, 20.0);
        assert_eq!(both.effective_end_hz, 19_845.0);
        assert!(
            both.clamp_note
                .unwrap()
                .starts_with("tilt_high end_hz clamped")
        );
        assert_eq!(both.error, None);
        // Only the treble section on.
        let off = TiltConfig {
            enabled: false,
            ..low
        };
        let treble = service
            .tilt_status(&chain(Some(off), Some(high)), 48_000)
            .unwrap();
        assert_eq!(treble.effective_start_hz, 4000.0);
        assert!(
            service
                .tilt_status(&chain(Some(off), None), 48_000)
                .is_none()
        );
        // A treble band the rate cannot hold names its section.
        let narrow = TiltConfig {
            start_hz: 17_750.0,
            ..high
        };
        let bad = tilt_status(&chain(Some(low), Some(narrow)), 44_100).unwrap();
        assert!(bad.error.unwrap().starts_with("tilt_high:"));
    }

    #[test]
    fn edits_are_validated_and_applied() {
        let (service, engine) = service();
        let err = service
            .dispatch(Call::SetUpsampler(
                audivane_dsp_core::resample::ManualConfig {
                    taps: 0,
                    ..Default::default()
                }
                .into_upsampler_config()
                .unwrap(),
            ))
            .unwrap_err();
        assert_eq!(err.code, RpcError::INVALID_PARAMS);
        assert!(err.message.contains("taps"));
        assert_eq!(engine.applies.load(Ordering::Relaxed), 0);

        service
            .dispatch(Call::SetUpsampler(UpsamplerConfig {
                target: audivane_protocol::RateTarget::Ratio(4),
                ..UpsamplerConfig::default()
            }))
            .unwrap();
        assert_eq!(engine.applies.load(Ordering::Relaxed), 1);
        assert_eq!(
            service.config().chain.upsampler.target,
            audivane_protocol::RateTarget::Ratio(4)
        );

        service
            .dispatch(Call::SetDspEnabled { enabled: false })
            .unwrap();
        assert!(!service.config().dsp_enabled);
        assert!(!engine.config.lock().unwrap().dsp_enabled);
    }

    /// The app's former whole-config edits each have a targeted setter that
    /// writes only its own field: a setting another client changed in the
    /// meantime (the CLI's `hardware_db`, say) is never written back from a
    /// stale copy.
    #[test]
    fn targeted_setters_leave_the_rest_of_the_config_alone() {
        use audivane_protocol::SampleFormat;
        let (service, engine) = service();
        // The app's copy, read before another client's edit.
        let stale = service.config();
        let mut cli = service.config();
        cli.volume.mode = audivane_protocol::ipc::VolumeMode::Software;
        cli.volume.hardware_db = Some(-40.0);
        service.dispatch(Call::SetConfig(cli)).unwrap();

        let mut output = stale.chain.output;
        output.bits = 16;
        service.dispatch(Call::SetOutput(output)).unwrap();
        let mut rate = stale.rate.clone();
        rate.default_rate = 44_100;
        service.dispatch(Call::SetRate(rate.clone())).unwrap();
        let formats = vec![SampleFormat::S24, SampleFormat::S32];
        service.dispatch(Call::SetFormats(formats.clone())).unwrap();
        service
            .dispatch(Call::SetDevice {
                device: "hw:1,0".into(),
                output_channels: None,
            })
            .unwrap();

        let cfg = service.config();
        assert_eq!(cfg.volume.hardware_db, Some(-40.0));
        assert_eq!(
            cfg.volume.mode,
            audivane_protocol::ipc::VolumeMode::Software
        );
        assert_eq!(cfg.chain.output, output);
        assert_eq!(cfg.rate, rate);
        assert_eq!(cfg.formats, formats);
        assert_eq!(cfg.device, "hw:1,0");
        assert_eq!(*engine.config.lock().unwrap(), cfg);
        // Validated like any edit.
        let err = service.dispatch(Call::SetFormats(Vec::new())).unwrap_err();
        assert_eq!(err.code, RpcError::INVALID_PARAMS);
        assert_eq!(service.config().formats, formats);
    }

    #[test]
    fn upsampler_response_rejects_invalid_explicit_config() {
        let (service, _) = service();
        let bad = |f: fn(&mut audivane_dsp_core::resample::ManualConfig)| {
            let mut cfg = audivane_dsp_core::resample::ManualConfig {
                target: audivane_protocol::RateTarget::Ratio(4),
                ..Default::default()
            };
            f(&mut cfg);
            cfg.into_upsampler_config().unwrap()
        };

        for (cfg, needle) in [
            (bad(|c| c.taps = usize::MAX), "taps"),
            (bad(|c| c.cutoff = 1.5), "cutoff"),
            (bad(|c| c.kaiser_beta = 99.0), "beta"),
        ] {
            let err = service
                .dispatch(Call::UpsamplerResponse {
                    config: Some(cfg),
                    source_rate: Some(44_100),
                    window: None,
                })
                .unwrap_err();
            assert_eq!(err.code, RpcError::INVALID_PARAMS);
            assert!(err.message.contains(needle), "{}", err.message);
        }
    }

    #[test]
    fn upsampler_response_is_cached_by_design_and_rate() {
        let (service, _) = service();
        let cfg = audivane_dsp_core::resample::ManualConfig {
            target: audivane_protocol::RateTarget::Ratio(2),
            taps: 512,
            ..Default::default()
        }
        .into_upsampler_config()
        .unwrap();
        let call = || Call::UpsamplerResponse {
            config: Some(cfg.clone()),
            source_rate: Some(44_100),
            window: None,
        };
        let first = service.dispatch(call()).unwrap();
        let second = service.dispatch(call()).unwrap();
        match (first, second) {
            (Dispatch::Value(a), Dispatch::Value(b)) => assert_eq!(a, b),
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(
            service.upsampler_cache.lock().unwrap().len(),
            1,
            "a repeated design must reuse the cached measurement"
        );

        service
            .dispatch(Call::UpsamplerResponse {
                config: Some(cfg.clone()),
                source_rate: Some(48_000),
                window: None,
            })
            .unwrap();
        assert_eq!(service.upsampler_cache.lock().unwrap().len(), 2);

        // A zoom window reuses the cached measurement.
        let zoomed = service
            .dispatch(Call::UpsamplerResponse {
                config: Some(cfg.clone()),
                source_rate: Some(48_000),
                window: Some(ResponseWindowRequest {
                    f_lo_hz: 23_000.0,
                    f_hi_hz: 25_000.0,
                    points: 100_000,
                }),
            })
            .unwrap();
        assert_eq!(service.upsampler_cache.lock().unwrap().len(), 2);
        let Dispatch::Value(zoomed) = zoomed else {
            panic!("unexpected dispatch");
        };
        let response: audivane_protocol::ipc::UpsamplerResponse =
            serde_json::from_value(zoomed).unwrap();
        let window = response.window.expect("window");
        assert_eq!(window.freqs_hz.len(), 4096, "points are capped");
        assert_eq!(window.freqs_hz[0], 23_000.0);
        assert_eq!(*window.freqs_hz.last().unwrap(), 25_000.0);
        assert!(!response.crossings.is_empty());
    }

    /// The reported latency counts the block the engine builds its chain
    /// with at the source rate, which follows the quantum: 512 frames at
    /// 44.1 kHz by default, the 1024-frame cap at a 4096 quantum (2048 at
    /// 44.1 kHz). Each block is its own cache entry.
    #[test]
    fn upsampler_response_latency_follows_the_engine_block() {
        let (service, _) = service();
        let cfg = audivane_dsp_core::resample::ManualConfig {
            target: audivane_protocol::RateTarget::Ratio(2),
            taps: 512,
            ..Default::default()
        }
        .into_upsampler_config()
        .unwrap();
        let latency = || {
            let v = value(service.dispatch(Call::UpsamplerResponse {
                config: Some(cfg.clone()),
                source_rate: Some(44_100),
                window: None,
            }));
            v["latency_samples"].as_u64().unwrap()
        };
        service.edit(|c| c.quantum = 1024).unwrap();
        let default = latency();
        service.edit(|c| c.quantum = 4096).unwrap();
        let large = latency();
        assert_eq!(large - default, (1024 - 512) * 2);
        assert_eq!(service.upsampler_cache.lock().unwrap().len(), 2);
    }

    fn value(d: Result<Dispatch, RpcError>) -> Value {
        match d.unwrap() {
            Dispatch::Value(v) => v,
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn upsampler_response_reports_the_design_and_plan() {
        let (service, _) = service();
        let cfg = audivane_dsp_core::resample::ManualConfig {
            target: audivane_protocol::RateTarget::Ratio(2),
            taps: 512,
            ..Default::default()
        }
        .into_upsampler_config()
        .unwrap();
        let response: audivane_protocol::ipc::UpsamplerResponse =
            serde_json::from_value(value(service.dispatch(Call::UpsamplerResponse {
                config: Some(cfg.clone()),
                source_rate: Some(44_100),
                window: None,
            })))
            .unwrap();
        assert_eq!(response.design.unwrap().design, cfg.design.unwrap().design);
        let plan = response.plan.unwrap();
        assert_eq!((plan.grid_up, plan.grid_down), (2, 1));
        assert_eq!(plan.grid_taps, response.taps);
        assert!(plan.bounded_peak_db.is_some_and(|db| db > -1.0));
        assert!(plan.cpu_load.is_some_and(|l| l > 0.0));
        // A preset: the expanded design, based on its name.
        let preset = UpsamplerConfig {
            target: audivane_protocol::RateTarget::Ratio(2),
            preset: Some("lanczos23".into()),
            ..UpsamplerConfig::default()
        };
        let response: audivane_protocol::ipc::UpsamplerResponse =
            serde_json::from_value(value(service.dispatch(Call::UpsamplerResponse {
                config: Some(preset),
                source_rate: Some(44_100),
                window: None,
            })))
            .unwrap();
        let design = response.design.unwrap();
        assert_eq!(design.based_on.as_deref(), Some("lanczos23"));
        assert_eq!(
            design.design,
            audivane_dsp_core::design::catalogue::by_name("lanczos23").unwrap()
        );
        assert!(response.plan.is_some());
    }

    #[test]
    fn resolve_design_expands_a_preset() {
        let (service, _) = service();
        let stored: audivane_protocol::StoredDesign =
            serde_json::from_value(value(service.dispatch(Call::ResolveDesign {
                preset: "recommended".into(),
            })))
            .unwrap();
        assert_eq!(stored.based_on.as_deref(), Some("recommended"));
        assert_eq!(
            stored.design,
            audivane_dsp_core::design::catalogue::recommended()
        );
        let err = service
            .dispatch(Call::ResolveDesign {
                preset: "nope".into(),
            })
            .unwrap_err();
        assert_eq!(err.code, RpcError::INVALID_PARAMS);
    }

    #[test]
    fn check_design_names_fields_and_plans_each_rate() {
        use audivane_dsp_core::resample::ManualConfig;
        use audivane_protocol::RateTarget;
        use audivane_protocol::ipc::DesignCheck;
        let (service, _) = service();
        let design = |m: ManualConfig| m.into_upsampler_config().unwrap().design.unwrap();
        let check = |design, target, source_rate| -> DesignCheck {
            serde_json::from_value(value(service.dispatch(Call::CheckDesign {
                design,
                target,
                source_rate,
            })))
            .unwrap()
        };
        let ok = design(ManualConfig {
            taps: 512,
            ..Default::default()
        });
        let c = check(ok.clone(), Some(RateTarget::Ratio(2)), None);
        assert!(c.errors.is_empty(), "{:?}", c.errors);
        let policy = service.config().rate.allowed_rates;
        assert_eq!(c.plans.len(), policy.len());
        for p in &c.plans {
            assert!(policy.contains(&p.source_rate));
            assert_eq!(p.ratio, RateRatio::integer(2));
            assert_eq!(p.plan.grid_up, 2);
            assert!(p.plan.grid_taps > 0);
            assert!(p.plan.bounded_peak_db.is_none());
        }
        let c = check(ok.clone(), Some(RateTarget::Ratio(2)), Some(48_000));
        assert_eq!(c.plans.len(), 1);
        assert_eq!(c.plans[0].source_rate, 48_000);

        // An automatic length (the recommended design): estimated, not
        // searched, at every rate of a rational target too.
        let rec = audivane_protocol::StoredDesign::new(
            audivane_dsp_core::design::catalogue::recommended(),
        );
        let c = check(rec, Some(RateTarget::MaxRateAny(768_000)), None);
        assert!(c.errors.is_empty(), "{:?}", c.errors);
        assert!(!c.plans.is_empty());

        // A bound: the field's path, no plans.
        let c = check(
            design(ManualConfig {
                kaiser_beta: 41.0,
                ..Default::default()
            }),
            Some(RateTarget::Ratio(2)),
            None,
        );
        assert_eq!(c.errors[0].path, "design.window.beta", "{:?}", c.errors);
        assert!(c.plans.is_empty());

        // The target and the source rate.
        let c = check(ok.clone(), Some(RateTarget::Ratio(70)), None);
        assert_eq!(c.errors[0].path, "target", "{:?}", c.errors);
        let c = check(ok, Some(RateTarget::Ratio(2)), Some(1));
        assert_eq!(c.errors[0].path, "source_rate", "{:?}", c.errors);

        // A budget at a rational ratio: the length, at its rate.
        let long = design(ManualConfig {
            target: RateTarget::Rate(48_000),
            taps: 6_554,
            taps_unit: audivane_protocol::TapsUnit::Input,
            kaiser_beta: 14.0,
            cutoff: 0.95,
            ..Default::default()
        });
        let c = check(long, Some(RateTarget::Rate(48_000)), Some(44_100));
        assert_eq!(c.errors.len(), 1, "{:?}", c.errors);
        assert_eq!(c.errors[0].path, "design.length");
        assert!(
            c.errors[0]
                .message
                .starts_with("6,554 taps per input sample is too heavy for a fractional ratio at 44.1k ×160/147. "),
            "{}",
            c.errors[0].message
        );
        let advice = c.advice.as_ref().unwrap();
        assert!(!advice.applied);
        assert!(
            advice.details[0].message.contains("kernel needs"),
            "{advice:?}"
        );
        assert!(c.plans.is_empty());

        // A passband end in Hz that does not fit at a rate: a note there,
        // the rate still planned; none where it fits.
        let mut pinned = audivane_dsp_core::design::catalogue::recommended();
        if let audivane_protocol::FilterDesign::Kernel(k) = &mut pinned
            && let audivane_protocol::design::Shape::WindowedSinc {
                cutoff: audivane_protocol::design::Cutoff::Band(b),
                ..
            } = &mut k.shape
        {
            b.passband_end_hz = Some(20_000.0);
        }
        let rec = audivane_protocol::StoredDesign::new(pinned);
        let c = check(rec.clone(), Some(RateTarget::Ratio(2)), Some(32_000));
        assert!(c.errors.is_empty(), "{:?}", c.errors);
        assert_eq!(c.plans.len(), 1);
        assert_eq!(c.notes.len(), 1, "{:?}", c.notes);
        assert_eq!(c.notes[0].source_rate, 32_000);
        assert_eq!(c.notes[0].path, "design.cutoff.passband_end_hz");
        let c = check(rec, Some(RateTarget::Ratio(2)), None);
        assert!(c.notes.is_empty(), "{:?}", c.notes);
    }

    /// An equiripple design of 1,279 taps over 0.9..1.0.
    fn equiripple_1279() -> audivane_protocol::design::FilterDesign {
        use audivane_protocol::design::*;
        let mut d = audivane_dsp_core::design::catalogue::recommended();
        if let FilterDesign::Kernel(k) = &mut d {
            k.shape = Shape::Optimal {
                method: OptimalMethod::Equiripple,
                length: Length::Fixed(1279),
                passband_end: 0.9,
                stopband_start: 1.0,
            };
            k.phase = Phase::Linear;
            k.numerics.path = ArithmeticPath::Fir;
        }
        d
    }

    /// A design that builds at some of the rates it plays is applied: the
    /// others play Recommended, said per rate in plain words, with fixes;
    /// one that builds nowhere is refused in plain words.
    #[test]
    fn a_design_that_builds_at_some_rates_falls_back_at_the_others() {
        use audivane_protocol::RateTarget;
        use audivane_protocol::ipc::{DesignCheck, OverloadAction, RateFallbackCause};
        let (service, engine) = service();
        let check = |design, target| -> DesignCheck {
            serde_json::from_value(value(service.dispatch(Call::CheckDesign {
                design,
                target,
                source_rate: None,
            })))
            .unwrap()
        };
        let target = RateTarget::MaxRate(768_000);
        let c = check(equiripple_1279().into(), Some(target));
        assert!(c.errors.is_empty(), "{:?}", c.errors);
        let advice = c.advice.clone().unwrap();
        assert!(advice.applied);
        eprintln!("{}", advice.message);
        assert!(
            advice.message.starts_with(
                "1,279 taps is too long for an equiripple design at 176.4k ×4 and 192k ×4. "
            ),
            "{}",
            advice.message
        );
        assert!(!advice.fixes.is_empty());
        // The ladder skips the x16 rung the design does not build at
        // (44.1k and 48k play it at x8); 176.4k and 192k build at no rung
        // and play Recommended.
        let falls: Vec<u32> = c.notes.iter().map(|n| n.source_rate).collect();
        assert_eq!(falls, [176_400, 192_000], "{:?}", c.notes);
        assert!(
            c.notes
                .iter()
                .all(|n| n.plays.as_deref() == Some("recommended"))
        );
        assert!(
            c.notes[0]
                .message
                .starts_with("too long for an equiripple design"),
            "{:?}",
            c.notes
        );
        let p44 = c.plans.iter().find(|p| p.source_rate == 44_100).unwrap();
        assert_eq!(p44.ratio, RateRatio::integer(8));
        // Every rate has a plan: the design's, or Recommended's.
        assert_eq!(c.plans.len(), 6);
        // A fix builds everywhere (the automatic length's searches are
        // left to dsp-core's test: slow in a debug build).
        for fix in advice
            .fixes
            .iter()
            .filter(|f| !f.label.contains("automatic"))
        {
            let c = check(fix.design.clone(), Some(target));
            assert!(
                c.advice.is_none() && c.errors.is_empty(),
                "{}: {c:?}",
                fix.label
            );
        }

        // Applied: the engine gets the design, and Recommended at the rates
        // it does not build at.
        let mut up = service.config().chain.upsampler;
        up.target = target;
        up.preset = None;
        up.design = Some(equiripple_1279().into());
        let r = service.dispatch(Call::SetUpsampler(up.clone()));
        assert!(r.is_ok(), "{r:?}");
        assert_eq!(engine.config.lock().unwrap().chain.upsampler, up);
        let status: audivane_protocol::ipc::Status =
            serde_json::from_value(value(service.dispatch(Call::Status))).unwrap();
        let rows = &status.upsampler_rates;
        let f = rows
            .iter()
            .find(|r| r.source_rate == 176_400)
            .and_then(|r| r.fallback.clone())
            .expect("176.4k falls back");
        assert_eq!(f.action, OverloadAction::Recommended);
        assert_eq!(f.cause, RateFallbackCause::Design);
        assert!(
            f.reason
                .unwrap()
                .starts_with("too long for an equiripple design here")
        );
        let status_falls: Vec<u32> = rows
            .iter()
            .filter(|r| r.fallback.is_some())
            .map(|r| r.source_rate)
            .collect();
        assert_eq!(status_falls, falls, "the check and the plan agree");
        let plan = service.lock_rate_plan().plan.clone();
        let cfg = service.config();
        assert_eq!(
            plan.substitute(&cfg, 176_400)
                .and_then(|u| u.preset.clone())
                .as_deref(),
            Some("recommended")
        );
        assert_eq!(plan.substitute(&cfg, 44_100), None);

        // Nowhere: refused, in plain words.
        let mut nowhere = up.clone();
        nowhere.target = RateTarget::Ratio(16);
        let c = check(equiripple_1279().into(), Some(nowhere.target));
        assert_eq!(c.errors.len(), 1, "{:?}", c.errors);
        assert!(
            c.errors[0]
                .message
                .starts_with("1,279 taps is too short at 44.1k ×16"),
            "{:?}",
            c.errors
        );
        assert!(!c.advice.unwrap().applied);
        let r = service.dispatch(Call::SetUpsampler(nowhere));
        assert!(r.is_err(), "{r:?}");
        let text = format!("{r:?}");
        assert!(
            text.contains("chain.upsampler.design.length: 1,279 taps is too short"),
            "{text}"
        );
    }

    #[test]
    fn upsampler_response_measures_up_to_x64() {
        let (service, _) = service();
        let measure = |target, source_rate, preset| {
            let cfg = audivane_dsp_core::resample::ManualConfig {
                target,
                taps: 16_384,
                preset,
                ..Default::default()
            }
            .into_upsampler_config()
            .unwrap();
            service.dispatch(Call::UpsamplerResponse {
                config: Some(cfg),
                source_rate: Some(source_rate),
                window: None,
            })
        };
        use audivane_protocol::RateTarget::{MaxRate, Ratio};
        for (target, source_rate, preset, ratio) in [
            (Ratio(64), 44_100, None, 64),
            (Ratio(48), 48_000, None, 48),
            (MaxRate(3_072_000), 48_000, None, 64),
            (
                MaxRate(3_072_000),
                44_100,
                Some(audivane_protocol::FilterPreset::GaussianLong),
                64,
            ),
        ] {
            let Dispatch::Value(value) = measure(target, source_rate, preset).unwrap() else {
                panic!("unexpected dispatch");
            };
            let r: audivane_protocol::ipc::UpsamplerResponse =
                serde_json::from_value(value).unwrap();
            assert_eq!(r.ratio, ratio, "{target:?}");
            assert_eq!(r.output_rate, source_rate * ratio);
            let nyquist = f64::from(source_rate) / 2.0;
            assert!(r.stopband_atten_db > 100.0, "{target:?}: {r:?}");
            assert!(r.stopband_start_hz >= nyquist - 1.0);
            assert!(*r.freqs_hz.last().unwrap() <= f64::from(r.output_rate) / 2.0);
            assert!(*r.freqs_hz.last().unwrap() > 0.9 * f64::from(r.output_rate) / 2.0);
        }
        // Beyond x64 the target itself is refused, in plain words.
        let err = measure(Ratio(65), 44_100, None).unwrap_err();
        assert_eq!(err.code, RpcError::INVALID_PARAMS);
        assert!(err.message.contains("between 1 and 64"), "{}", err.message);
        // A source rate above the highest output rate is refused too.
        let err = measure(Ratio(2), 3_072_001, None).unwrap_err();
        assert!(err.message.contains("3072000"), "{}", err.message);
    }

    #[test]
    fn upsampler_response_rejects_invalid_windows() {
        let (service, _) = service();
        let cfg = audivane_dsp_core::resample::ManualConfig {
            target: audivane_protocol::RateTarget::Ratio(2),
            taps: 512,
            ..Default::default()
        }
        .into_upsampler_config()
        .unwrap();
        // Output Nyquist is 44.1 kHz for 44.1 kHz x2.
        for (lo, hi) in [
            (f64::NAN, 1_000.0),
            (0.0, f64::INFINITY),
            (-1.0, 1_000.0),
            (1_000.0, 44_100.5),
            (2_000.0, 2_000.0),
            (3_000.0, 2_000.0),
        ] {
            let err = service
                .dispatch(Call::UpsamplerResponse {
                    config: Some(cfg.clone()),
                    source_rate: Some(44_100),
                    window: Some(ResponseWindowRequest {
                        f_lo_hz: lo,
                        f_hi_hz: hi,
                        points: 100,
                    }),
                })
                .unwrap_err();
            assert_eq!(err.code, RpcError::INVALID_PARAMS, "{lo} .. {hi}");
        }
        assert!(
            service.upsampler_cache.lock().unwrap().is_empty(),
            "an invalid window is rejected before measuring"
        );
        service
            .dispatch(Call::UpsamplerResponse {
                config: Some(cfg),
                source_rate: Some(44_100),
                window: Some(ResponseWindowRequest {
                    f_lo_hz: 0.0,
                    f_hi_hz: 44_100.0,
                    points: 10,
                }),
            })
            .unwrap();
    }

    #[test]
    fn ab_select_requires_ab_config() {
        let (service, _) = service();
        let err = service
            .dispatch(Call::AbSelect {
                side: AbSide::B,
                crossfade_ms: 10.0,
            })
            .unwrap_err();
        assert_eq!(err.code, RpcError::INVALID_PARAMS);
        assert!(err.message.contains("A/B"));
    }

    #[test]
    fn set_ab_then_select() {
        let (service, _) = service();
        let status = match service
            .dispatch(Call::SetAb(Some(AbConfig {
                level: LevelMatch::Manual { offset_db: -2.0 },
                headroom: Default::default(),
                ..AbConfig::chain(ChainConfig::neutral())
            })))
            .unwrap()
        {
            Dispatch::Value(value) => serde_json::from_value::<AbStatus>(value).unwrap(),
            other => panic!("unexpected {other:?}"),
        };
        assert!(status.enabled);
        assert_eq!(status.current, Some(AbSide::A));

        let status = match service
            .dispatch(Call::AbSelect {
                side: AbSide::B,
                crossfade_ms: 10.0,
            })
            .unwrap()
        {
            Dispatch::Value(value) => serde_json::from_value::<AbStatus>(value).unwrap(),
            other => panic!("unexpected {other:?}"),
        };
        assert_eq!(status.current, Some(AbSide::B));
        // The fake engine runs no pair, so the reply carries the config's
        // gains: the manual -2 dB, B cut and A untouched.
        assert_eq!(status.offset_db, -2.0);
        assert_eq!((status.gain_a_db, status.gain_b_db), (0.0, -2.0));
    }

    /// `ab_blind` hides the side from every reply and from status, refuses
    /// a level measurement, and ends on `{enabled: false}`, `set_ab` or A/B
    /// off.
    #[test]
    fn ab_blind_masks_the_side() {
        let (service, engine) = service();
        let ab_value = |call| match service.dispatch(call).unwrap() {
            Dispatch::Value(value) => serde_json::from_value::<AbStatus>(value).unwrap(),
            other => panic!("unexpected {other:?}"),
        };
        let err = service
            .dispatch(Call::AbBlind { enabled: true })
            .unwrap_err();
        assert_eq!(err.code, RpcError::INVALID_PARAMS, "no A/B yet");
        let config = || {
            Call::SetAb(Some(AbConfig {
                level: LevelMatch::Manual { offset_db: -2.0 },
                headroom: Default::default(),
                ..AbConfig::chain(ChainConfig::neutral())
            }))
        };
        ab_value(config());

        let status = ab_value(Call::AbBlind { enabled: true });
        assert!(status.blind);
        assert_eq!(status.current, None);
        // The live view forgets everything at the mask (and again at its
        // end), so nothing from the run carries over.
        assert_eq!(engine.live_suspends.load(Ordering::Relaxed), 1);
        let status = ab_value(Call::AbSelect {
            side: AbSide::B,
            crossfade_ms: 10.0,
        });
        assert_eq!((status.blind, status.current), (true, None));
        assert_eq!(status.offset_db, -2.0, "the level still shows");
        let ab = service.status().ab.expect("A/B on");
        assert_eq!((ab.blind, ab.current), (true, None));
        let err = service
            .dispatch(Call::AbMeasure {
                max_seconds: None,
                cancel: false,
                accept: false,
            })
            .unwrap_err();
        assert_eq!(err.code, RpcError::BAD_STATE);
        assert!(err.message.contains("blind"));

        // The harmonics live view hides too (it would give the side away).
        match service.dispatch(Call::HarmonicsLive).unwrap() {
            Dispatch::Value(v) => assert_eq!(v["state"], "hidden"),
            other => panic!("unexpected {other:?}"),
        }

        let status = ab_value(Call::AbBlind { enabled: false });
        assert_eq!((status.blind, status.current), (false, Some(AbSide::B)));
        assert_eq!(engine.live_suspends.load(Ordering::Relaxed), 2);

        // `set_ab` ends it.
        ab_value(Call::AbBlind { enabled: true });
        let status = ab_value(config());
        assert_eq!((status.blind, status.current), (false, Some(AbSide::A)));

        // A/B off ends it, and it does not come back with A/B.
        ab_value(Call::AbBlind { enabled: true });
        service.dispatch(Call::SetAb(None)).unwrap();
        assert!(!service.ab_blind.load(Ordering::Relaxed));
        let status = ab_value(config());
        assert!(!status.blind);
    }

    /// Step 7: a patch may give B its own bits and dither (an output
    /// compare); the output format, the device and the rate stay refused,
    /// with the field named.
    #[test]
    fn set_ab_takes_a_dither_patch_and_refuses_the_output_format() {
        let (service, _) = service();
        let patch = |p| {
            service.dispatch(Call::SetAb(Some(AbConfig::new(
                audivane_protocol::ipc::AbB::Patch(p),
            ))))
        };
        patch(serde_json::json!({"output": {"bits": 16, "dither": "shaped"}})).unwrap();
        let ab = service.config().ab.unwrap();
        assert_eq!(ab.chain_b.output.bits, 16);
        assert_eq!(
            ab.chain_b.output.dither,
            audivane_protocol::DitherKind::Shaped
        );
        let a = service.config().chain.output;
        assert_eq!(ab.chain_b.output.headroom_db, a.headroom_db);

        for (p, field) in [
            (
                serde_json::json!({"output": {"format": "s16"}}),
                "ab.b.patch.output.format",
            ),
            (
                serde_json::json!({"formats": ["s16"]}),
                "ab.b.patch.formats",
            ),
            (serde_json::json!({"device": "hw:1,0"}), "ab.b.patch.device"),
            (
                serde_json::json!({"upsampler": {"target": {"rate": 96000}}}),
                "ab.b.patch.upsampler.target",
            ),
        ] {
            let err = patch(p).unwrap_err();
            assert_eq!(err.code, RpcError::INVALID_PARAMS);
            assert!(err.message.starts_with(field), "{}", err.message);
        }
        // The refused patches left the dither compare in place.
        assert_eq!(service.config().ab.unwrap().chain_b.output.bits, 16);
    }

    /// Test 12: measurement lock, driven by a simulated hop stream.
    #[test]
    fn ab_measure_locks_once_after_the_stability_window() {
        let (service, engine) = service();
        let measure = |max_seconds, cancel, accept| {
            service
                .dispatch(Call::AbMeasure {
                    max_seconds,
                    cancel,
                    accept,
                })
                .map(|d| match d {
                    Dispatch::Value(value) => serde_json::from_value::<AbStatus>(value).unwrap(),
                    other => panic!("unexpected {other:?}"),
                })
        };
        let tick = || {
            let _mutation = service.lock_mutation();
            service.ab_measure_step();
        };
        let level = || service.config().ab.unwrap().level;
        let err = measure(None, false, false).unwrap_err();
        assert!(err.message.contains("not configured"), "{}", err.message);
        service
            .dispatch(Call::SetAb(Some(AbConfig::chain(ChainConfig::neutral()))))
            .unwrap();
        assert!(
            measure(Some(5.0), false, false).is_err(),
            "max_seconds range"
        );
        let err = measure(None, false, true).unwrap_err();
        assert_eq!(err.code, RpcError::BAD_STATE);

        let st = measure(Some(60.0), false, false).unwrap();
        let m = st.measuring.unwrap();
        assert_eq!((m.seconds, m.offset_db, m.max_seconds), (0.0, None, 60.0));
        assert_ne!(engine.measure.lock().unwrap().0, 0, "the engine meters");
        assert_eq!(
            measure(None, false, true).unwrap_err().code,
            RpcError::BAD_STATE,
            "nothing to accept yet"
        );
        let applies = engine.applies.load(Ordering::Relaxed);

        // B plays 2.5 dB louder. A pause waits; a quiet passage where B is
        // much louder is dropped by A's relative gate on both sides (only
        // the blocks at its edges, mostly loud, move the offset a little).
        engine.play(8.0, -18.0, 2.5);
        tick();
        engine.play(20.0, f64::NEG_INFINITY, 0.0);
        engine.play(3.0, -40.0, 15.0);
        tick();
        engine.play(11.0, -18.0, 2.5);
        tick();
        let st = service.ab_status();
        let m = st.measuring.expect("still measuring before 20 s");
        assert!((18.0..20.0).contains(&m.seconds), "{}", m.seconds);
        assert!((m.offset_db.unwrap() + 2.5).abs() < 0.02, "{m:?}");
        assert_eq!(level(), LevelMatch::default(), "no lock before 20 s");
        assert_eq!(engine.applies.load(Ordering::Relaxed), applies);

        // Past 20 s, settled over the last 10 s: one lock, one apply.
        engine.play(2.0, -18.0, 2.5);
        tick();
        let LevelMatch::Measured { offset_db, seconds } = level() else {
            panic!("locked: {:?}", level());
        };
        assert!((offset_db + 2.5).abs() <= 0.02, "{offset_db}");
        assert!((20.0..23.0).contains(&seconds), "{seconds}");
        assert_eq!(engine.applies.load(Ordering::Relaxed), applies + 1);
        assert_eq!(engine.measure.lock().unwrap().0, 0, "metering stopped");
        let st = service.ab_status();
        assert_eq!(st.measuring, None);
        assert_eq!(st.offset_db, offset_db);
        assert_eq!(st.level, level());
        // It never moves again.
        engine.play(30.0, -18.0, 6.0);
        tick();
        assert_eq!(engine.applies.load(Ordering::Relaxed), applies + 1);

        // Accept locks the running value; a chain change cancels; cancel
        // keeps the level.
        measure(Some(60.0), false, false).unwrap();
        engine.play(3.0, -18.0, 1.0);
        tick();
        let st = measure(None, false, true).unwrap();
        assert_eq!(st.measuring, None);
        assert!(matches!(st.level, LevelMatch::Measured { offset_db, .. } if offset_db == -1.0));
        measure(None, false, false).unwrap();
        service
            .dispatch(Call::SetDspEnabled { enabled: false })
            .unwrap();
        assert!(
            service.ab_status().measuring.is_some(),
            "a change outside the pair keeps it"
        );
        let mut chain = service.config().chain;
        chain.eq = Some(EqProfile {
            preamp_db: -1.0,
            ..Default::default()
        });
        service
            .dispatch(Call::SetConfig(EngineConfig {
                chain,
                ..service.config()
            }))
            .unwrap();
        assert_eq!(service.ab_status().measuring, None, "the pair changed");
        assert_eq!(engine.measure.lock().unwrap().0, 0);
        measure(None, false, false).unwrap();
        let before = level();
        let st = measure(None, true, false).unwrap();
        assert_eq!(st.measuring, None);
        assert_eq!(level(), before);
    }

    /// A track skip or a rate change ends the measurement and drops its
    /// queued hops, also before an accept; a silent B ends at `max_seconds`
    /// with no match.
    #[test]
    fn ab_measure_ends_on_a_track_or_rate_change_and_without_a_match() {
        use crate::control::mpris::MprisWatcher;
        use audivane_protocol::ipc::NowPlaying;
        let engine = Arc::new(MockEngine {
            config: Mutex::new(EngineConfig {
                device: "hw:0,0".into(),
                ..EngineConfig::default()
            }),
            applies: AtomicU64::new(0),
            refreshes: AtomicU64::new(0),
            applied: Mutex::new((None, 0)),
            measure: Mutex::new((0, Vec::new())),
            graph_rate: Mutex::new(None),
            live_suspends: AtomicU64::new(0),
            caps: Mutex::new(None),
        });
        let mpris = MprisWatcher::stub();
        let service = Service::new(
            Arc::clone(&engine) as Arc<dyn Engine>,
            engine.config.lock().unwrap().clone(),
            None,
            None,
            Some(Arc::clone(&mpris)),
            ShutdownFlag::new(),
        );
        let track = |title: &str| {
            Some(NowPlaying {
                player: "org.mpris.MediaPlayer2.player".into(),
                title: Some(title.into()),
                artist: None,
                album: None,
                status: "Playing".into(),
                native_rate: None,
                native_bits: None,
                track_id: None,
            })
        };
        let measure = |max_seconds, accept| {
            service.dispatch(Call::AbMeasure {
                max_seconds,
                cancel: false,
                accept,
            })
        };
        let tick = || {
            let _mutation = service.lock_mutation();
            service.ab_measure_step();
        };
        let queued = || engine.measure.lock().unwrap().1.len();
        service
            .dispatch(Call::SetAb(Some(AbConfig::chain(ChainConfig::neutral()))))
            .unwrap();

        // A skip: the old track's 25 s never lock, the new track's hops go.
        mpris.set_now_playing(track("one"));
        measure(Some(60.0), false).unwrap();
        engine.play(15.0, -18.0, 3.0);
        tick();
        // Pausing keeps the identity: not a change.
        let mut paused = track("one").unwrap();
        paused.status = "Paused".into();
        mpris.set_now_playing(Some(paused));
        tick();
        assert!(service.ab_status().measuring.is_some());
        engine.play(10.0, -18.0, 3.0);
        mpris.set_now_playing(track("two"));
        tick();
        let st = service.ab_status();
        assert_eq!(st.measuring, None);
        assert_eq!(
            st.measure_error.as_deref(),
            Some("cancelled: the track changed")
        );
        assert_eq!(queued(), 0, "the queued hops are dropped");
        assert_eq!(engine.measure.lock().unwrap().0, 0);
        assert_eq!(service.config().ab.unwrap().level, LevelMatch::default());

        // Before a manual accept too.
        measure(Some(60.0), false).unwrap();
        assert_eq!(service.ab_status().measure_error, None, "a start clears it");
        engine.play(5.0, -18.0, 3.0);
        tick();
        mpris.set_now_playing(track("three"));
        let err = measure(None, true).unwrap_err();
        assert_eq!(err.code, RpcError::BAD_STATE);
        assert!(err.message.contains("track changed"), "{}", err.message);
        assert_eq!(service.config().ab.unwrap().level, LevelMatch::default());

        // A rate change (the next track at another rate).
        measure(Some(60.0), false).unwrap();
        engine.play(5.0, -18.0, 3.0);
        tick();
        *engine.graph_rate.lock().unwrap() = Some(44_100);
        engine.play(5.0, -18.0, 3.0);
        tick();
        let st = service.ab_status();
        assert_eq!(st.measuring, None);
        assert_eq!(
            st.measure_error.as_deref(),
            Some("cancelled: the sample rate changed")
        );
        assert_eq!(queued(), 0);

        // Two untagged files in one player: only `mpris:trackid` differs.
        let untagged = |id: &str| {
            Some(NowPlaying {
                title: None,
                track_id: Some(id.into()),
                ..track("").unwrap()
            })
        };
        *engine.graph_rate.lock().unwrap() = None;
        mpris.set_now_playing(untagged("/track/1"));
        measure(Some(60.0), false).unwrap();
        engine.play(5.0, -18.0, 3.0);
        tick();
        assert!(service.ab_status().measuring.is_some());
        mpris.set_now_playing(untagged("/track/2"));
        tick();
        assert_eq!(
            service.ab_status().measure_error.as_deref(),
            Some("cancelled: the track changed")
        );

        // A silent B: at max_seconds the run ends with no match.
        measure(Some(10.0), false).unwrap();
        let epoch = engine.measure.lock().unwrap().0;
        for _ in 0..120 {
            engine
                .measure
                .lock()
                .unwrap()
                .1
                .push(audivane_dsp_core::ab_measure::AbHop {
                    epoch,
                    a: 0.01,
                    b: 0.0,
                });
        }
        tick();
        let st = service.ab_status();
        assert_eq!(st.measuring, None, "the run ends");
        assert!(
            st.measure_error.as_deref().unwrap().starts_with("no match"),
            "{st:?}"
        );
        assert_eq!(engine.measure.lock().unwrap().0, 0);
        assert_eq!(service.config().ab.unwrap().level, LevelMatch::default());
    }

    #[test]
    fn routing_without_graph_reports_bad_state() {
        let (service, _) = service();
        let err = service
            .dispatch(Call::Route {
                target: audivane_protocol::ipc::RouteTarget::Node(1),
                sink: SinkChoice::Dsp,
            })
            .unwrap_err();
        assert_eq!(err.code, RpcError::BAD_STATE);
    }

    #[test]
    fn status_reports_the_persisted_active_preset() {
        let dir = std::env::temp_dir().join(format!(
            "avdsp-svc-preset-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(ConfigStore::new(dir.join("config.json")));
        let chain = ChainConfig {
            eq: Some(EqProfile {
                bands: vec![audivane_protocol::Band {
                    kind: audivane_protocol::FilterType::Peaking,
                    freq_hz: 1000.0,
                    gain_db: 3.0,
                    q: 1.0,
                    enabled: true,
                }],
                ..Default::default()
            }),
            ..ChainConfig::neutral()
        };
        PresetStore::alongside(&store)
            .save(&Preset::new("Saved", chain.clone()))
            .unwrap();

        // A restart: the config says which preset was active, the service
        // loads its chain so `modified` is right from the first status.
        let config = EngineConfig {
            device: "hw:0,0".into(),
            chain: chain.clone(),
            active_preset: Some("Saved".into()),
            ..EngineConfig::default()
        };
        let engine = Arc::new(MockEngine {
            config: Mutex::new(config.clone()),
            applies: AtomicU64::new(0),
            refreshes: AtomicU64::new(0),
            applied: Mutex::new((None, 0)),
            measure: Mutex::new((0, Vec::new())),
            graph_rate: Mutex::new(None),
            live_suspends: AtomicU64::new(0),
            caps: Mutex::new(None),
        });
        let service = Service::new(
            Arc::clone(&engine) as Arc<dyn Engine>,
            config,
            Some(Arc::clone(&store)),
            None,
            None,
            ShutdownFlag::new(),
        );
        assert_eq!(
            service.status().preset,
            Some(PresetState {
                name: "Saved".into(),
                modified: false,
            })
        );

        // Any chain A edit marks it modified; loading the preset clears it.
        service.dispatch(Call::SetEq(None)).unwrap();
        assert!(service.status().preset.unwrap().modified);
        service
            .dispatch(Call::LoadPreset {
                name: "Saved".into(),
                part: None,
            })
            .unwrap();
        assert!(!service.status().preset.unwrap().modified);

        service.flush_persist().unwrap();
        service.stop_persist();
        drop(service);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn import_apo_writes_an_eq_preset_with_the_file_preamp() {
        let dir = std::env::temp_dir().join(format!(
            "avdsp-svc-apo-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(ConfigStore::new(dir.join("config.json")));
        let config = EngineConfig {
            device: "hw:0,0".into(),
            ..EngineConfig::default()
        };
        let engine = Arc::new(MockEngine {
            config: Mutex::new(config.clone()),
            applies: AtomicU64::new(0),
            refreshes: AtomicU64::new(0),
            applied: Mutex::new((None, 0)),
            measure: Mutex::new((0, Vec::new())),
            graph_rate: Mutex::new(None),
            live_suspends: AtomicU64::new(0),
            caps: Mutex::new(None),
        });
        let service = Service::new(
            Arc::clone(&engine) as Arc<dyn Engine>,
            config,
            Some(Arc::clone(&store)),
            None,
            None,
            ShutdownFlag::new(),
        );
        service
            .dispatch(Call::ImportApo {
                name: "HP".into(),
                text: "Preamp: -6 dB\nFilter 1: ON PK Fc 1000 Hz Gain 5 dB Q 1\n".into(),
            })
            .unwrap();
        let root = PresetStore::alongside(&store);
        assert!(root.load("HP").is_err(), "not a global preset");
        let preset = root.scoped(Some(PresetPart::Eq)).load("HP").unwrap();
        assert_eq!(preset.part, Some(PresetPart::Eq));
        assert_eq!(preset.chain.eq.as_ref().unwrap().preamp_db, -6.0);

        service.flush_persist().unwrap();
        service.stop_persist();
        drop(service);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn subscribe_is_reported() {
        let (service, _) = service();
        match service
            .dispatch(Call::Subscribe {
                topics: vec![Topic::Meters],
            })
            .unwrap()
        {
            Dispatch::Subscribe(topics) => assert_eq!(topics, vec![Topic::Meters]),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn set_config_rejects_bad_device() {
        let (service, _) = service();
        let err = service
            .dispatch(Call::SetConfig(EngineConfig {
                device: "default".into(),
                ..EngineConfig::default()
            }))
            .unwrap_err();
        assert_eq!(err.code, RpcError::INVALID_PARAMS);
    }

    /// A config that sets a ceiling below its own volume stores the
    /// ceiling, so nothing (status, the sinks, a later mode) sees the
    /// louder level.
    #[test]
    fn set_config_never_stores_a_volume_above_the_ceiling() {
        let (service, engine) = service();
        let mut cfg = service.config();
        cfg.volume.volume_db = -2.0;
        cfg.volume.max_volume_db = Some(-10.0);
        service.dispatch(Call::SetConfig(cfg)).unwrap();
        assert_eq!(service.config().volume.volume_db, -10.0);
        assert_eq!(engine.config.lock().unwrap().volume.volume_db, -10.0);
        assert_eq!(service.status().volume.unwrap().volume_db, -10.0);

        // Lowering the ceiling later through an edit clamps as well.
        service
            .edit(|cfg| cfg.volume.max_volume_db = Some(-20.0))
            .unwrap();
        assert_eq!(service.config().volume.volume_db, -20.0);
        // set_config ignores the incoming level (out of range included);
        // set_volume still rejects one out of range.
        let mut cfg = service.config();
        cfg.volume.volume_db = 40.0;
        service.dispatch(Call::SetConfig(cfg)).unwrap();
        assert_eq!(service.config().volume.volume_db, -20.0);
        assert!(
            service
                .dispatch(Call::SetVolume {
                    volume_db: 40.0,
                    balance_db: 0.0,
                    muted: false,
                })
                .is_err()
        );
    }

    #[test]
    fn level_only_edits_skip_the_routing_push() {
        let old = EngineConfig::default();
        let mut next = old.clone();
        next.volume.volume_db = -12.0;
        next.volume.muted = true;
        assert!(level_only_change(&old, &next));
        next.volume.guard.mode = GuardMode::Limit;
        assert!(!level_only_change(&old, &next));
        let mut next = old.clone();
        next.routing = RoutingMode::Apps { rules: Vec::new() };
        assert!(!level_only_change(&old, &next));
    }

    fn follow(service: &Service, edit: impl FnOnce(&mut VolumeConfig)) {
        let mut cfg = service.config();
        cfg.volume.sink_volume = SinkVolumePolicy::Follow;
        edit(&mut cfg.volume);
        // The level is set by `set_volume` only; the settings by `set_config`.
        service
            .dispatch(Call::SetVolume {
                volume_db: cfg.volume.volume_db,
                balance_db: cfg.volume.balance_db,
                muted: cfg.volume.muted,
            })
            .unwrap();
        service.dispatch(Call::SetConfig(cfg)).unwrap();
    }

    fn sink_request(volume_db: Option<f64>, muted: Option<bool>) -> SinkVolumeRequest {
        SinkVolumeRequest {
            sink: SinkChoice::Dsp,
            volume_db,
            balance_db: volume_db.map(|_| 0.0),
            muted,
        }
    }

    /// Switching to follow never gets louder than the desktop
    /// had it.
    #[test]
    fn following_adopts_the_quieter_sink_level() {
        let mut v = VolumeConfig::default();
        // DSP sink at 30 % (cubic: 0.027 linear, -31.4 dB), Direct at 100 %.
        let note = adopt_levels(&mut v, &[(Some(0.027), false), (Some(1.0), false)]);
        assert!(note.is_some());
        assert!((v.volume_db - 20.0 * 0.027f64.log10()).abs() < 0.001);
        // Already quieter: unchanged; a muted sink mutes.
        let mut v = VolumeConfig {
            volume_db: -50.0,
            ..VolumeConfig::default()
        };
        adopt_levels(&mut v, &[(Some(0.5), true)]);
        assert_eq!(v.volume_db, -50.0);
        assert!(v.muted);
        assert_eq!(adopt_levels(&mut v, &[(Some(1.0), false)]), None);
    }

    #[test]
    fn follow_adopts_only_the_sink_listened_through() {
        use crate::control::graph::{Facts, SinkFact};
        let sink = |choice, volume, muted| SinkFact {
            choice,
            node: 1,
            serial: None,
            name: String::new(),
            channels: Some(2),
            volume: Some(volume),
            muted,
        };
        let facts = Facts {
            sinks: vec![
                sink(SinkChoice::Dsp, 0.5, false),
                sink(SinkChoice::Direct, 0.1, true),
            ],
            ..Facts::default()
        };
        // Listening through the DSP sink: a muted Direct sink is not adopted.
        let mut v = VolumeConfig::default();
        adopt_levels(&mut v, &listened_sink_level(&facts, None));
        assert!(!v.muted);
        assert!((v.volume_db - 20.0 * 0.5f64.log10()).abs() < 0.001);
        let mut v = VolumeConfig::default();
        adopt_levels(&mut v, &listened_sink_level(&facts, Some(SinkChoice::Dsp)));
        assert!(!v.muted);
        // Listening through Direct: its level and mute.
        let mut v = VolumeConfig::default();
        adopt_levels(
            &mut v,
            &listened_sink_level(&facts, Some(SinkChoice::Direct)),
        );
        assert!(v.muted);
        assert!((v.volume_db + 20.0).abs() < 0.001);
    }

    #[test]
    fn sink_requests_are_ignored_unless_following() {
        let (service, _engine) = service();
        service.on_sink_volume(sink_request(Some(-10.0), Some(true)));
        assert_eq!(service.config().volume.volume_db, 0.0);
        assert!(!service.config().volume.muted);
    }

    #[test]
    fn followed_sink_volume_drives_the_config_through_the_ceiling() {
        let (service, engine) = service();
        follow(&service, |v| v.max_volume_db = Some(-6.0));
        service.on_sink_volume(sink_request(Some(-20.0), None));
        assert_eq!(service.config().volume.volume_db, -20.0);
        service.on_sink_volume(sink_request(None, Some(true)));
        assert!(service.config().volume.muted);
        assert_eq!(service.config().volume.volume_db, -20.0);

        // +10 dB from the desktop: capped at the ceiling, and logged.
        let refreshes = engine.refreshes.load(Ordering::Relaxed);
        service.on_sink_volume(sink_request(Some(10.6), None));
        assert_eq!(service.config().volume.volume_db, -6.0);
        assert!(engine.refreshes.load(Ordering::Relaxed) == refreshes);
        let status = service.status().volume.unwrap();
        assert_eq!(status.blocked, 1);
        let last = status.last_blocked.unwrap();
        assert_eq!(last.reason, BlockReason::Capped);
        assert_eq!(last.applied_db, -6.0);
        assert_eq!(
            last.source,
            VolumeSource::Sink {
                sink: SinkChoice::Dsp
            }
        );

        // Our own set_volume above the ceiling is capped as well.
        let reply = service
            .dispatch(Call::SetVolume {
                volume_db: -1.0,
                balance_db: 0.0,
                muted: false,
            })
            .unwrap();
        assert!(matches!(reply, Dispatch::Value(_)));
        assert_eq!(service.config().volume.volume_db, -6.0);
        assert_eq!(service.status().volume.unwrap().blocked, 2);
        // The other volume fields survive set_volume.
        assert_eq!(
            service.config().volume.sink_volume,
            SinkVolumePolicy::Follow
        );
        assert_eq!(service.config().volume.max_volume_db, Some(-6.0));
    }

    /// Volume keys on the desktop, at the ceiling, guard `limit` with no rise
    /// limit. The desktop's 100 % is the ceiling (cubic below it, as the driver
    /// maps it). Each key steps 5 % from what the sink shows, up to `max_pct`
    /// (a desktop may allow more than 100 %), and the sink shows the request
    /// until the engine answers. A request above the ceiling while already at
    /// it must be answered too: otherwise the sink runs above 100 % and the
    /// next presses down are lost above the ceiling.
    #[test]
    fn volume_key_bursts_at_the_ceiling_keep_the_sink_on_the_level() {
        for max_pct in [100.0, 150.0] {
            volume_key_bursts_at_the_ceiling(max_pct);
        }
    }

    fn volume_key_bursts_at_the_ceiling(max_pct: f64) {
        let (service, engine) = service();
        follow(&service, |v| {
            v.volume_db = -3.0;
            v.max_volume_db = Some(-3.0);
            v.guard.mode = GuardMode::Limit;
            v.guard.max_rise_db_per_s = 0.0;
        });
        let db_of = |pct: f64| -3.0 + 60.0 * (pct / 100.0).log10();
        let pct_of = |db: f64| 100.0 * 10f64.powf((db + 3.0) / 60.0);
        let mut shown = pct_of(-3.0);
        assert!((shown - 100.0).abs() < 1e-9, "the ceiling reads 100 %");
        let press = |up: bool, shown: &mut f64| {
            let target = (shown.round() + if up { 5.0 } else { -5.0 }).clamp(0.0, max_pct);
            *shown = target;
            let (applies, refreshes) = (
                engine.applies.load(Ordering::Relaxed),
                engine.refreshes.load(Ordering::Relaxed),
            );
            service.on_sink_volume(sink_request(Some(db_of(target)), None));
            let level = service.config().volume.volume_db;
            if engine.applies.load(Ordering::Relaxed) != applies
                || engine.refreshes.load(Ordering::Relaxed) != refreshes
            {
                *shown = pct_of(level);
            }
            assert!(level <= -3.0 + 1e-9, "above the ceiling: {level}");
            assert!(
                (db_of(*shown) - level).abs() < 0.01,
                "the sink shows {shown:.2} % ({:.2} dB), the engine is at {level:.2} dB",
                db_of(*shown)
            );
            level
        };
        // Held up at the ceiling: the level and the sink stay at -3 dB.
        for _ in 0..6 {
            assert_eq!(press(true, &mut shown), -3.0);
        }
        assert!((shown - 100.0).abs() < 1e-9, "the sink stays at 100 %");
        // Held down: every press goes down.
        let mut last = -3.0;
        for _ in 0..6 {
            let level = press(false, &mut shown);
            assert!(level < last - 0.5, "{level} after {last}");
            last = level;
        }
        // One press up is one step up.
        let level = press(true, &mut shown);
        assert!(level > last && level < last + 3.0, "{level} after {last}");
    }

    #[test]
    fn locked_sink_volume_is_reverted_but_mute_passes() {
        let (service, engine) = service();
        follow(&service, |v| {
            v.volume_db = -30.0;
            v.guard.mode = GuardMode::Lock;
        });
        service.on_sink_volume(sink_request(Some(0.0), None));
        assert_eq!(service.config().volume.volume_db, -30.0);
        assert_eq!(engine.refreshes.load(Ordering::Relaxed), 1);
        assert_eq!(
            service
                .status()
                .volume
                .unwrap()
                .last_blocked
                .unwrap()
                .reason,
            BlockReason::Locked
        );
        service.on_sink_volume(sink_request(None, Some(true)));
        assert!(service.config().volume.muted);
    }

    #[test]
    fn limited_sink_volume_rises_gradually_and_falls_at_once() {
        let (service, _engine) = service();
        follow(&service, |v| {
            v.volume_db = -30.0;
            v.guard.mode = GuardMode::Limit;
            v.guard.max_rise_db_per_s = 20.0;
        });
        let start = std::time::Instant::now();
        service.on_sink_volume(sink_request(Some(-10.0), None));
        assert_eq!(service.config().volume.volume_db, -30.0);
        assert!(service.volume_slewing());
        let mut last = -30.0;
        while service.volume_slewing() {
            std::thread::sleep(volume_guard::SLEW_TICK);
            service.volume_tick();
            let now = service.config().volume.volume_db;
            let allowed = 20.0 * start.elapsed().as_secs_f64() + 0.05;
            assert!(
                now >= last && now + 30.0 <= allowed,
                "{now} after {:?}",
                start.elapsed()
            );
            last = now;
            assert!(start.elapsed() < std::time::Duration::from_secs(5));
        }
        assert_eq!(last, -10.0);
        assert!(start.elapsed() >= std::time::Duration::from_millis(950));

        service.on_sink_volume(sink_request(Some(-40.0), None));
        assert_eq!(service.config().volume.volume_db, -40.0);
        assert!(!service.volume_slewing());

        // set_volume is never slowed down and cancels a pending rise.
        service.on_sink_volume(sink_request(Some(0.0), None));
        assert!(service.volume_slewing());
        service
            .dispatch(Call::SetVolume {
                volume_db: -5.0,
                balance_db: 0.0,
                muted: false,
            })
            .unwrap();
        assert!(!service.volume_slewing());
        assert_eq!(service.config().volume.volume_db, -5.0);
    }

    /// The revision moves whenever the reported (applied) level changes, not
    /// only the configured one: a staged hardware raise reports the interim
    /// level first and settles later under no new request. A guarded write
    /// computed from the interim status is refused, and two statuses with one
    /// revision always show the same level.
    #[test]
    fn the_revision_follows_the_applied_level() {
        let (service, engine) = service();
        let applied = |volume_db, hardware_db| VolumeStatus {
            volume_db,
            hardware_db: Some(hardware_db),
            hardware: true,
            ..VolumeStatus::from_config(&service.config().volume)
        };
        engine.set_applied(applied(-60.0, -60.0));
        let interim = service.status().volume.unwrap();
        assert_eq!(interim.volume_db, -60.0);
        // The raise lands.
        engine.set_applied(applied(-30.0, -30.0));
        let settled = service.status().volume.unwrap();
        assert_eq!(settled.volume_db, -30.0);
        assert!(settled.revision > interim.revision, "{settled:?}");
        let again = service.status().volume.unwrap();
        assert_eq!(again.revision, settled.revision);
        assert_eq!(again.volume_db, settled.volume_db);
        // A step computed from the interim status is stale.
        let err = service
            .dispatch(Call::SetVolumeGuarded {
                volume_db: Some(-59.0),
                balance_db: None,
                muted: None,
                if_instance: service.instance.clone(),
                if_revision: interim.revision.unwrap(),
            })
            .unwrap_err();
        assert_eq!(err.code, RpcError::STALE_REVISION);
        assert_ne!(service.config().volume.volume_db, -59.0);
        // From the settled one it applies, and its reply pairs the level
        // with the revision a following write is checked against.
        let reply = service
            .dispatch(Call::SetVolumeGuarded {
                volume_db: Some(-29.0),
                balance_db: None,
                muted: None,
                if_instance: service.instance.clone(),
                if_revision: settled.revision.unwrap(),
            })
            .unwrap();
        let Dispatch::Value(reply) = reply else {
            panic!("not a value")
        };
        let echo: VolumeStatus = serde_json::from_value(reply).unwrap();
        assert_eq!(echo.revision, service.status().volume.unwrap().revision);
        // Mixer readback on a reopen changes the report again.
        engine.set_applied(applied(-29.5, -29.5));
        assert!(service.status().volume.unwrap().revision > echo.revision);
    }

    /// A guarded write without level fields changes nothing: no ramp
    /// stopped, no engine apply, no new config generation or revision.
    #[test]
    fn an_empty_guarded_write_has_no_side_effects() {
        let (service, engine) = service();
        follow(&service, |v| {
            v.volume_db = -30.0;
            v.guard.mode = GuardMode::Limit;
            v.guard.max_rise_db_per_s = 20.0;
        });
        service.on_sink_volume(sink_request(Some(-10.0), None));
        assert!(service.volume_slewing());
        let applies = engine.applies.load(Ordering::Relaxed);
        let generation = service.config_generation();
        let revision = service.level_revision();
        let reply = service
            .dispatch(Call::SetVolumeGuarded {
                volume_db: None,
                balance_db: None,
                muted: None,
                if_instance: service.instance.clone(),
                if_revision: revision,
            })
            .unwrap();
        let Dispatch::Value(reply) = reply else {
            panic!("not a value")
        };
        let status: VolumeStatus = serde_json::from_value(reply).unwrap();
        assert_eq!(status.volume_db, -30.0);
        assert_eq!(status.revision, Some(revision));
        assert!(service.volume_slewing(), "the desktop ramp was stopped");
        assert_eq!(engine.applies.load(Ordering::Relaxed), applies);
        assert_eq!(service.config_generation(), generation);
        assert_eq!(service.level_revision(), revision);
        // The guards still apply to an empty write.
        let err = service
            .dispatch(Call::SetVolumeGuarded {
                volume_db: None,
                balance_db: None,
                muted: None,
                if_instance: service.instance.clone(),
                if_revision: revision + 1,
            })
            .unwrap_err();
        assert_eq!(err.code, RpcError::STALE_REVISION);
    }

    #[test]
    fn direct_sink_volume_is_refused_and_its_mute_is_the_engine_mute() {
        let (service, _engine) = service();
        follow(&service, |v| v.volume_db = -12.0);
        service.on_sink_volume(SinkVolumeRequest {
            sink: SinkChoice::Direct,
            volume_db: Some(-6.0),
            balance_db: Some(0.0),
            muted: Some(true),
        });
        let cfg = service.config();
        assert_eq!(cfg.volume.volume_db, -12.0);
        assert!(cfg.volume.muted);
        let last = service.status().volume.unwrap().last_blocked.unwrap();
        assert_eq!(last.reason, BlockReason::DirectBitPerfect);
    }

    #[test]
    fn set_volume_validates_and_surfaces_in_status() {
        let (service, engine) = service();

        // Reject out-of-range volume and balance without calling engine.apply.
        let err = service
            .dispatch(Call::SetVolume {
                volume_db: 1.0,
                balance_db: 0.0,
                muted: false,
            })
            .unwrap_err();
        assert_eq!(err.code, RpcError::INVALID_PARAMS);
        assert_eq!(engine.applies.load(Ordering::Relaxed), 0);

        let err = service
            .dispatch(Call::SetVolume {
                volume_db: -10.0,
                balance_db: -30.0,
                muted: false,
            })
            .unwrap_err();
        assert_eq!(err.code, RpcError::INVALID_PARAMS);
        assert_eq!(engine.applies.load(Ordering::Relaxed), 0);

        let reply = match service
            .dispatch(Call::SetVolume {
                volume_db: -15.0,
                balance_db: 4.5,
                muted: true,
            })
            .unwrap()
        {
            Dispatch::Value(v) => serde_json::from_value::<VolumeStatus>(v).unwrap(),
            other => panic!("unexpected {other:?}"),
        };
        assert_eq!(reply.volume_db, -15.0);
        assert_eq!(reply.balance_db, 4.5);
        assert!(reply.muted);
        assert!(!reply.hardware);
        assert_eq!(reply.min_db, audivane_protocol::ipc::VOLUME_MIN_DB);
        assert_eq!(reply.max_db, audivane_protocol::ipc::VOLUME_MAX_DB);
        assert_eq!(engine.applies.load(Ordering::Relaxed), 1);

        let status = service.status();
        assert_eq!(status.volume, Some(reply));
    }

    /// An engine that validates as the real one does (the convolution
    /// guards, the follower chains), at 48 kHz.
    struct GuardMock {
        inner: MockEngine,
        cache: Mutex<crate::engine::ir::IrCache>,
        warnings: Mutex<Vec<audivane_protocol::ipc::ConvWarning>>,
    }
    impl Engine for GuardMock {
        fn apply(&self, cfg: &EngineConfig) -> Result<(), EngineError> {
            let w = crate::engine::validate_engine_config(
                cfg,
                &mut self.cache.lock().unwrap(),
                48_000,
            )?;
            *self.warnings.lock().unwrap() = w;
            self.inner.apply(cfg)
        }
        fn conv_warnings(&self) -> Vec<audivane_protocol::ipc::ConvWarning> {
            self.warnings.lock().unwrap().clone()
        }
        fn ab_select(&self, side: AbSide, crossfade_ms: f64) -> Result<AbStatus, EngineError> {
            self.inner.ab_select(side, crossfade_ms)
        }
        fn snapshot(&self) -> EngineSnapshot {
            self.inner.snapshot()
        }
        fn meters(&self) -> Meters {
            self.inner.meters()
        }
        fn selftest(&self) -> Result<audivane_protocol::ipc::SelfTestResult, EngineError> {
            self.inner.selftest()
        }
        fn release_device(&self) -> Result<(), EngineError> {
            Ok(())
        }
        fn acquire_device(&self) -> Result<(), EngineError> {
            Ok(())
        }
        fn shutdown(&self) {}
    }

    /// The convolution guards through the control plane: a boost over the
    /// cap is refused by `set_convolution`, a preset load, `set_ab` (side
    /// B) and `set_config`, each leaving the config as it was; a boost
    /// under it applies, with its warnings in the reply's notes and in
    /// status. The engine here validates as the real one does.
    #[test]
    fn convolution_over_the_cap_is_refused_on_every_path() {
        let dir = std::env::temp_dir().join(format!(
            "avdsp-svc-convguard-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let ir = dir.join("ir.wav");
        {
            let spec = hound::WavSpec {
                channels: 1,
                sample_rate: 48_000,
                bits_per_sample: 32,
                sample_format: hound::SampleFormat::Float,
            };
            let mut w = hound::WavWriter::create(&ir, spec).unwrap();
            for i in 0..64 {
                w.write_sample(if i == 0 { 1.0_f32 } else { 0.0 }).unwrap();
            }
            w.finalize().unwrap();
        }
        let conv = |gain_db: f64| audivane_protocol::ConvolutionConfig {
            name: None,
            enabled: true,
            ir_path: ir.clone(),
            gain_db,
            matrix: None,
            calibration: false,
        };
        let store = Arc::new(ConfigStore::new(dir.join("config.json")));
        let config = EngineConfig {
            device: "hw:0,0".into(),
            ..EngineConfig::default()
        };
        let loud = ChainConfig {
            convolution: vec![conv(13.0)],
            ..config.chain.clone()
        };
        let presets = PresetStore::alongside(&store);
        presets.save(&Preset::new("Loud", loud.clone())).unwrap();
        presets
            .scoped(Some(PresetPart::Convolution))
            .save(&Preset::for_part(
                "Loud",
                PresetPart::Convolution,
                loud.clone(),
            ))
            .unwrap();
        let engine = guard_mock(&config);
        let service = Service::new(
            Arc::clone(&engine) as Arc<dyn Engine>,
            config,
            Some(Arc::clone(&store)),
            None,
            None,
            ShutdownFlag::new(),
        );

        // Under the cap: applied, its warnings (+6 dB down to DC) noted.
        let reply = match service
            .dispatch(Call::SetConvolution(vec![conv(6.0)]))
            .unwrap()
        {
            Dispatch::Value(v) => v,
            other => panic!("unexpected {other:?}"),
        };
        let notes = reply["notes"].as_array().expect("notes");
        assert!(
            notes[0]
                .as_str()
                .unwrap()
                .starts_with("Below 20 Hz: +6.0 dB on L"),
            "{reply}"
        );
        assert_eq!(service.status().conv_warnings.len(), 2);
        let before = service.config();

        let refused = |call: Call| {
            let err = service.dispatch(call).unwrap_err();
            assert_eq!(err.code, RpcError::INVALID_PARAMS);
            assert!(err.message.contains("+12.0 dB cap"), "{}", err.message);
            assert_eq!(service.config(), before);
            err.message
        };
        refused(Call::SetConvolution(vec![conv(13.0)]));
        refused(Call::LoadPreset {
            name: "Loud".into(),
            part: None,
        });
        refused(Call::LoadPreset {
            name: "Loud".into(),
            part: Some(PresetPart::Convolution),
        });
        let side_b = refused(Call::SetAb(Some(AbConfig::chain(loud.clone()))));
        assert!(side_b.starts_with("A/B side B:"), "{side_b}");
        refused(Call::SetConfig(EngineConfig {
            chain: loud,
            ..before.clone()
        }));
        assert_eq!(service.status().conv_warnings.len(), 2);

        // A higher cap lets it through.
        service
            .dispatch(Call::SetConvolutionSafety(
                audivane_protocol::ipc::ConvolutionSafety {
                    max_boost_db: 14.0,
                    ..Default::default()
                },
            ))
            .unwrap();
        service
            .dispatch(Call::SetConvolution(vec![conv(13.0)]))
            .unwrap();
        assert!(
            service
                .dispatch(Call::SetConvolutionSafety(
                    audivane_protocol::ipc::ConvolutionSafety {
                        max_boost_db: 12.0,
                        ..Default::default()
                    },
                ))
                .is_err()
        );
        assert_eq!(service.config().convolution_safety.max_boost_db, 14.0);

        service.flush_persist().unwrap();
        service.stop_persist();
        drop(service);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn guard_mock(config: &EngineConfig) -> Arc<GuardMock> {
        Arc::new(GuardMock {
            inner: MockEngine {
                config: Mutex::new(config.clone()),
                applies: AtomicU64::new(0),
                refreshes: AtomicU64::new(0),
                applied: Mutex::new((None, 0)),
                measure: Mutex::new((0, Vec::new())),
                graph_rate: Mutex::new(None),
                live_suspends: AtomicU64::new(0),
                caps: Mutex::new(None),
            },
            cache: Mutex::new(crate::engine::ir::IrCache::new()),
            warnings: Mutex::new(Vec::new()),
        })
    }

    /// A room-calibration bundle through the control plane, the engine
    /// validating as the real one does: a load applies everything or
    /// nothing (a bad IR or a boost over the cap on the follower leaves the
    /// master untouched); unload puts back the chain items, the follower's
    /// patch and trim; a preset load keeps the calibration's item (and a
    /// saved preset never holds it); `bundle_ab` makes side B chain A with
    /// another calibration, or none.
    #[test]
    fn a_calibration_bundle_loads_whole_unloads_exactly_and_survives_presets() {
        use serde_json::json;
        let dir = std::env::temp_dir().join(format!(
            "avdsp-svc-bundle-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let wav = |name: &str, channels: u16, peak: f32| {
            let spec = hound::WavSpec {
                channels,
                sample_rate: 48_000,
                bits_per_sample: 32,
                sample_format: hound::SampleFormat::Float,
            };
            let mut w = hound::WavWriter::create(dir.join(name), spec).unwrap();
            for i in 0..64 {
                for _ in 0..channels {
                    w.write_sample(if i == 0 { peak } else { 0.0 }).unwrap();
                }
            }
            w.finalize().unwrap();
        };
        wav("mine.wav", 1, 0.9);
        wav("other.wav", 1, 0.8);
        wav("ll.wav", 1, 1.0);
        wav("rr.wav", 1, 1.0);
        wav("sub.wav", 2, 0.5);
        // +14 dB on the follower's one output: over the +12 dB cap.
        wav("loud.wav", 2, 2.5);
        const SUB: &str = "hw:CARD=USB_1,DEV=0";
        let manifest = |name: &str, master_gain: f64, sub_ir: &str| {
            let body = json!({
                "bundle_version": 1, "label": name, "design_rate_hz": 48000,
                "devices": [
                    { "role": "master", "gain_db": master_gain,
                      "matrix": [ { "in": 0, "out": 0, "ir": "ll.wav" },
                                  { "in": 1, "out": 1, "ir": "rr.wav" } ] },
                    { "role": "follower", "device": SUB, "trim_ms": 1.5,
                      "matrix": [ { "in": 0, "out": 0, "ir": sub_ir, "channel": 0 },
                                  { "in": 1, "out": 0, "ir": sub_ir, "channel": 1 } ] } ]
            });
            let p = dir.join(format!("{name}.json"));
            std::fs::write(&p, body.to_string()).unwrap();
            p.display().to_string()
        };
        let conv = |file: &str| audivane_protocol::ConvolutionConfig {
            name: Some(file.into()),
            enabled: true,
            ir_path: dir.join(file),
            gain_db: 0.0,
            matrix: None,
            calibration: false,
        };
        let store = Arc::new(ConfigStore::new(dir.join("config.json")));
        let mut config = EngineConfig {
            device: "hw:CARD=USB,DEV=0".into(),
            ..EngineConfig::default()
        };
        config.chain.convolution = vec![conv("mine.wav")];
        let mut sub = FollowerConfig::new(SUB);
        sub.trim_ms = -2.0;
        sub.chain_patch = Some(json!({"limiter": {"mode": "limit"}}));
        config.outputs.followers.push(sub);
        let presets = PresetStore::alongside(&store);
        let other = ChainConfig {
            convolution: vec![conv("other.wav")],
            ..config.chain.clone()
        };
        presets
            .scoped(Some(PresetPart::Convolution))
            .save(&Preset::for_part("Other", PresetPart::Convolution, other))
            .unwrap();
        let engine = guard_mock(&config);
        let service = Service::new(
            Arc::clone(&engine) as Arc<dyn Engine>,
            config,
            Some(Arc::clone(&store)),
            None,
            None,
            ShutdownFlag::new(),
        );
        let value = |c: Call| match service.dispatch(c).unwrap() {
            Dispatch::Value(v) => v,
            other => panic!("unexpected {other:?}"),
        };
        let before = service.config();

        // Nothing changes on a bad follower part: its boost, or its file.
        let loud = manifest("loud", 0.0, "loud.wav");
        let err = service
            .dispatch(Call::BundleLoad { path: loud })
            .unwrap_err();
        assert!(
            err.message.contains("nothing changed")
                && err.message.contains(&format!("follower {SUB}"))
                && err.message.contains("+12.0 dB cap"),
            "{}",
            err.message
        );
        assert_eq!(service.config(), before);
        let missing = manifest("missing", 0.0, "nope.wav");
        let err = service
            .dispatch(Call::BundleLoad { path: missing })
            .unwrap_err();
        assert!(
            err.message.contains("devices[1].matrix[0].ir"),
            "{}",
            err.message
        );
        assert_eq!(service.config(), before);
        assert!(service.dispatch(Call::BundleUnload).is_err(), "none loaded");

        // A good one: the master's item after the user's, the follower's
        // patch and trim; status shows it.
        let desk = manifest("desk", -1.0, "sub.wav");
        let reply = value(Call::BundleLoad { path: desk.clone() });
        assert_eq!(reply["label"], "desk");
        let cfg = service.config();
        assert_eq!(cfg.chain.convolution.len(), 2);
        assert_eq!(cfg.chain.convolution[0], before.chain.convolution[0]);
        let item = cfg.chain.convolution[1].clone();
        assert!(item.calibration && item.gain_db == -1.0);
        let f = &cfg.outputs.followers[0];
        assert_eq!(f.trim_ms, 1.5);
        let patch = f.chain_patch.as_ref().unwrap();
        assert_eq!(patch["limiter"], json!({"mode": "limit"}));
        assert_eq!(
            patch["convolution"][0]["matrix"].as_array().unwrap().len(),
            2
        );
        let st = service.status().calibration.expect("status");
        assert_eq!(st.devices.len(), 2);
        assert!(st.devices.iter().all(|d| d.installed));
        assert_eq!(value(Call::BundleShow)["devices"][1]["device"], SUB);

        // Unload puts back exactly what was there, but the follower keeps
        // following at its level (an unload never moves or unmutes it).
        value(Call::BundleUnload);
        let mut want = before.clone();
        want.outputs.followers[0].follow_volume = true;
        want.outputs.followers[0].follow_ref_db = Some(0.0);
        assert_eq!(service.config(), want);
        assert!(value(Call::BundleShow).is_null());

        // Loaded again, a convolution preset replaces the user's item and
        // keeps the calibration's; it is not "modified" for it.
        value(Call::BundleLoad { path: desk.clone() });
        value(Call::LoadPreset {
            name: "Other".into(),
            part: Some(PresetPart::Convolution),
        });
        let cfg = service.config();
        assert_eq!(cfg.chain.convolution.len(), 2);
        assert_eq!(cfg.chain.convolution[0].name.as_deref(), Some("other.wav"));
        assert_eq!(cfg.chain.convolution[1], item);
        let parts = service.status().part_presets;
        assert!(parts.iter().all(|p| !p.modified), "{parts:?}");
        // A convolution edit without it gets it back.
        service
            .dispatch(Call::SetConvolution(vec![conv("mine.wav")]))
            .unwrap();
        assert_eq!(service.config().chain.convolution[1], item);
        // A saved preset holds the user's items only.
        service
            .dispatch(Call::SavePreset {
                name: "Saved".into(),
                overwrite: false,
                part: Some(PresetPart::Convolution),
                parts: None,
            })
            .unwrap();
        let saved = presets
            .scoped(Some(PresetPart::Convolution))
            .load("Saved")
            .unwrap();
        assert_eq!(saved.chain.convolution, vec![conv("mine.wav")]);

        // A/B of calibrations: B is chain A with the other one's master
        // part; A keeps its own.
        let quiet = manifest("quiet", -3.0, "sub.wav");
        let reply = value(Call::BundleAb { path: Some(quiet) });
        assert_eq!(reply["enabled"], true);
        let notes = reply["notes"].as_array().unwrap();
        assert!(
            notes
                .iter()
                .any(|n| n.as_str().unwrap().contains("master only")),
            "{notes:?}"
        );
        let cfg = service.config();
        let ab = cfg.ab.as_ref().unwrap();
        assert_eq!(ab.label.as_deref(), Some("Calibration: quiet"));
        assert_eq!(ab.chain_b.convolution.len(), 2);
        assert_eq!(ab.chain_b.convolution[0], cfg.chain.convolution[0]);
        assert_eq!(ab.chain_b.convolution[1].gain_db, -3.0);
        assert_eq!(cfg.chain.convolution[1], item);
        // A later edit of A reaches B; B keeps its calibration.
        service
            .dispatch(Call::SetConvolution(vec![conv("other.wav")]))
            .unwrap();
        let cfg = service.config();
        let b = &cfg.ab.as_ref().unwrap().chain_b;
        assert_eq!(b.convolution[0].name.as_deref(), Some("other.wav"));
        assert_eq!(b.convolution[1].gain_db, -3.0);
        // Without a calibration on B.
        value(Call::BundleAb { path: None });
        let cfg = service.config();
        let b = &cfg.ab.as_ref().unwrap().chain_b;
        assert_eq!(b.convolution, vec![conv("other.wav")]);
        // Any other side B keeps A's calibration.
        service
            .dispatch(Call::SetAb(Some(AbConfig::new(
                audivane_protocol::ipc::AbB::Patch(json!({"convolution": []})),
            ))))
            .unwrap();
        let cfg = service.config();
        assert_eq!(cfg.ab.as_ref().unwrap().chain_b.convolution, vec![item]);

        service.flush_persist().unwrap();
        service.stop_persist();
        drop(service);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A temporary folder with impulse-response WAVs (one channel each, a
    /// first tap at `peak`) and a bundle manifest writer for the tests.
    struct BundleDir(std::path::PathBuf);

    impl BundleDir {
        fn new(tag: &str) -> Self {
            let d = std::env::temp_dir().join(format!(
                "avdsp-svc-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .subsec_nanos()
            ));
            std::fs::create_dir_all(&d).unwrap();
            Self(d)
        }
        fn wav(&self, name: &str, peak: f32) -> std::path::PathBuf {
            let p = self.0.join(name);
            let spec = hound::WavSpec {
                channels: 1,
                sample_rate: 48_000,
                bits_per_sample: 32,
                sample_format: hound::SampleFormat::Float,
            };
            let mut w = hound::WavWriter::create(&p, spec).unwrap();
            for i in 0..64 {
                w.write_sample(if i == 0 { peak } else { 0.0 }).unwrap();
            }
            w.finalize().unwrap();
            p
        }
        fn manifest(&self, name: &str, follower: Option<&str>) -> String {
            let mut devices = vec![serde_json::json!(
                { "role": "master", "matrix": [ { "in": 0, "out": 0, "ir": "ll.wav" } ] })];
            if let Some(f) = follower {
                devices.push(
                    serde_json::json!({ "role": "follower", "device": f, "trim_ms": 1.5,
                    "matrix": [ { "in": 0, "out": 0, "ir": "sub.wav" } ] }),
                );
            }
            let body = serde_json::json!({ "bundle_version": 1, "label": name,
                "design_rate_hz": 48000, "devices": devices });
            let p = self.0.join(format!("{name}.json"));
            std::fs::write(&p, body.to_string()).unwrap();
            p.display().to_string()
        }
    }

    impl Drop for BundleDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn bundle_service(config: EngineConfig, dir: &BundleDir) -> Arc<Service> {
        let store = Arc::new(ConfigStore::new(dir.0.join("config.json")));
        Service::new(
            guard_mock(&config) as Arc<dyn Engine>,
            config,
            Some(store),
            None,
            None,
            ShutdownFlag::new(),
        )
    }

    /// `bundle_load`'s reply is a `CalibrationStatus`: the manifest's
    /// `notes` text stays, the load's warnings come in `warnings`.
    #[test]
    fn the_load_reply_keeps_the_manifests_notes() {
        let dir = BundleDir::new("bundle-notes");
        dir.wav("ll.wav", 1.0);
        let p = dir.0.join("noted.json");
        std::fs::write(
            &p,
            serde_json::json!({ "bundle_version": 1, "label": "noted", "design_rate_hz": 48000,
                "notes": "target and positions",
                "devices": [ { "role": "master", "device": "hw:CARD=Other,DEV=0",
                    "matrix": [ { "in": 0, "out": 0, "ir": "ll.wav" } ] } ] })
            .to_string(),
        )
        .unwrap();
        let config = EngineConfig {
            device: "hw:CARD=USB,DEV=0".into(),
            ..EngineConfig::default()
        };
        let service = bundle_service(config, &dir);
        let reply = match service
            .dispatch(Call::BundleLoad {
                path: p.display().to_string(),
            })
            .unwrap()
        {
            Dispatch::Value(v) => v,
            other => panic!("unexpected {other:?}"),
        };
        let st: audivane_protocol::ipc::CalibrationStatus = serde_json::from_value(reply).unwrap();
        assert_eq!(st.notes.as_deref(), Some("target and positions"));
        assert!(
            st.warnings
                .iter()
                .any(|w| w.contains("hw:CARD=Other,DEV=0")),
            "{:?}",
            st.warnings
        );
        service.stop_persist();
    }

    /// The calibration is the daemon's across whole-config edits: a
    /// `set_config` without it keeps it (and its record), so unload still
    /// puts the follower's trim back.
    #[test]
    fn set_config_keeps_the_calibration() {
        const SUB: &str = "hw:CARD=USB_1,DEV=0";
        let dir = BundleDir::new("bundle-setcfg");
        dir.wav("ll.wav", 1.0);
        dir.wav("sub.wav", 0.5);
        let mut config = EngineConfig {
            device: "hw:CARD=USB,DEV=0".into(),
            ..EngineConfig::default()
        };
        let mut f = FollowerConfig::new(SUB);
        f.trim_ms = -2.0;
        config.outputs.followers.push(f);
        let service = bundle_service(config, &dir);
        let before = service.config();
        service
            .dispatch(Call::BundleLoad {
                path: dir.manifest("desk", Some(SUB)),
            })
            .unwrap();
        let mut sent = service.config();
        sent.calibration = None;
        sent.chain.convolution.clear();
        sent.quantum = 512;
        service.dispatch(Call::SetConfig(sent)).unwrap();
        let cfg = service.config();
        assert!(cfg.calibration.is_some(), "kept");
        assert!(
            cfg.chain.convolution[0].calibration,
            "its item back in place"
        );
        assert_eq!(cfg.outputs.followers[0].trim_ms, 1.5);
        service.dispatch(Call::BundleUnload).unwrap();
        let cfg = service.config();
        assert_eq!(cfg.outputs.followers[0].trim_ms, -2.0);
        let mut want = before.clone();
        want.outputs.followers[0].follow_volume = true;
        want.outputs.followers[0].follow_ref_db = Some(0.0);
        assert_eq!(
            EngineConfig {
                quantum: before.quantum,
                ..cfg
            },
            want
        );
        service.stop_persist();
    }

    /// `bundle_gain` sets one part's level: the master's reaches chain A's
    /// item only, a follower's that follower's patch only; it is kept
    /// through convolution edits, a preset load and `set_config`; the boost
    /// cap refuses it (nothing changes); `null` goes back to the manifest's,
    /// and a new load or an unload drops it.
    #[test]
    fn bundle_gain_sets_keeps_and_resets_each_part() {
        const SUB: &str = "hw:CARD=USB_1,DEV=0";
        const OTHER: &str = "hw:CARD=USB_2,DEV=0";
        let dir = BundleDir::new("bundle-gain");
        dir.wav("ll.wav", 1.0);
        dir.wav("sub.wav", 0.5);
        dir.wav("mine.wav", 1.0);
        let mut config = EngineConfig {
            device: "hw:CARD=USB,DEV=0".into(),
            ..EngineConfig::default()
        };
        config.outputs.followers.push(FollowerConfig::new(OTHER));
        let store = Arc::new(ConfigStore::new(dir.0.join("config.json")));
        let presets = PresetStore::alongside(&store);
        let mine = audivane_protocol::ConvolutionConfig {
            name: Some("mine".into()),
            enabled: true,
            ir_path: dir.0.join("mine.wav"),
            gain_db: 0.0,
            matrix: None,
            calibration: false,
        };
        let chain = ChainConfig {
            convolution: vec![mine.clone()],
            ..config.chain.clone()
        };
        presets
            .scoped(Some(PresetPart::Convolution))
            .save(&Preset::for_part("Mine", PresetPart::Convolution, chain))
            .unwrap();
        let service = Service::new(
            guard_mock(&config) as Arc<dyn Engine>,
            config,
            Some(store),
            None,
            None,
            ShutdownFlag::new(),
        );
        let value = |c: Call| match service.dispatch(c).unwrap() {
            Dispatch::Value(v) => v,
            other => panic!("unexpected {other:?}"),
        };
        let gain = |device: Option<&str>, gain_db: Option<f64>| Call::BundleGain {
            device: device.map(Into::into),
            gain_db,
        };
        let master_item = |c: &EngineConfig| {
            c.chain
                .convolution
                .iter()
                .find(|c| c.calibration)
                .unwrap()
                .gain_db
        };
        let patch_gain = |c: &EngineConfig, dev: &str| {
            c.outputs
                .followers
                .iter()
                .find(|f| f.device == dev)
                .unwrap()
                .chain_patch
                .as_ref()
                .and_then(|p| p.get("convolution"))
                .map(|v| v[0]["gain_db"].as_f64().unwrap())
        };
        assert_eq!(
            service.dispatch(gain(None, Some(-1.0))).unwrap_err().code,
            RpcError::BAD_STATE,
            "none loaded"
        );
        let desk = dir.manifest("desk", Some(SUB));
        value(Call::BundleLoad { path: desk.clone() });

        // The follower's part: its patch only, the master's item untouched.
        let st = value(gain(Some(SUB), Some(3.0)));
        assert_eq!(st["devices"][1]["gain_db"], 3.0);
        assert_eq!(st["devices"][1]["manifest_gain_db"], 0.0);
        let cfg = service.config();
        assert_eq!(patch_gain(&cfg, SUB), Some(3.0));
        assert_eq!(master_item(&cfg), 0.0);
        assert_eq!(patch_gain(&cfg, OTHER), None, "another follower: none");
        // The master's: chain A only.
        value(gain(Some("master"), Some(-4.0)));
        let cfg = service.config();
        assert_eq!(master_item(&cfg), -4.0);
        assert_eq!(patch_gain(&cfg, SUB), Some(3.0));
        let st = service.status().calibration.unwrap();
        assert_eq!(
            (st.devices[0].gain_db, st.devices[0].manifest_gain_db),
            (-4.0, 0.0)
        );

        // Out of range, an unknown part, and over the boost cap: refused,
        // nothing changes.
        let before = service.config();
        assert!(service.dispatch(gain(None, Some(30.0))).is_err());
        assert!(service.dispatch(gain(Some(OTHER), Some(1.0))).is_err());
        let err = service.dispatch(gain(None, Some(20.0))).unwrap_err();
        assert!(
            err.message.contains("nothing changed") && err.message.contains("cap"),
            "{}",
            err.message
        );
        let err = service.dispatch(gain(Some(SUB), Some(24.0))).unwrap_err();
        assert!(
            err.message.contains(&format!("follower {SUB}")),
            "{}",
            err.message
        );
        assert_eq!(service.config(), before);

        // Kept through a convolution edit, a preset load and set_config.
        service
            .dispatch(Call::SetConvolution(vec![mine.clone()]))
            .unwrap();
        value(Call::LoadPreset {
            name: "Mine".into(),
            part: Some(PresetPart::Convolution),
        });
        let mut sent = service.config();
        sent.calibration = None;
        sent.chain.convolution.retain(|c| !c.calibration);
        sent.quantum = 512;
        service.dispatch(Call::SetConfig(sent)).unwrap();
        let cfg = service.config();
        assert_eq!(master_item(&cfg), -4.0);
        assert_eq!(patch_gain(&cfg, SUB), Some(3.0));

        // Reset: back to the manifest's, the record cleared.
        value(gain(Some(SUB), None));
        let cfg = service.config();
        assert_eq!(patch_gain(&cfg, SUB), Some(0.0));
        let cal = cfg.calibration.as_ref().unwrap();
        assert_eq!(cal.followers[0].manifest_gain_db, None);
        assert_eq!(cal.master_manifest_gain_db, Some(0.0));

        // A new load (the same bundle again) starts from the manifest.
        value(Call::BundleLoad { path: desk });
        let cfg = service.config();
        assert_eq!(master_item(&cfg), 0.0);
        assert_eq!(
            cfg.calibration.as_ref().unwrap().master_manifest_gain_db,
            None
        );
        // Unload removes it with the rest.
        value(gain(None, Some(-2.0)));
        service.dispatch(Call::BundleUnload).unwrap();
        let cfg = service.config();
        assert!(cfg.chain.convolution.iter().all(|c| !c.calibration));
        assert!(service.dispatch(gain(None, Some(-2.0))).is_err());
        service.stop_persist();
    }

    /// Unload works from the record as edits left it: a follower the
    /// bundle added, removed and added again by hand, stays; a follower
    /// whose IR from before the bundle is gone is put back without it (a
    /// note says so), or with what the user set on it since.
    #[test]
    fn unload_follows_the_followers_edits() {
        const SUB: &str = "hw:CARD=USB_1,DEV=0";
        let dir = BundleDir::new("bundle-edits");
        dir.wav("ll.wav", 1.0);
        dir.wav("sub.wav", 0.5);
        let own = dir.wav("own.wav", 0.5);
        let config = EngineConfig {
            device: "hw:CARD=USB,DEV=0".into(),
            ..EngineConfig::default()
        };
        let service = bundle_service(config, &dir);
        let follower = |patch: Option<Value>| Call::SetFollower {
            device: SUB.into(),
            remove: false,
            enabled: None,
            trim_ms: None,
            volume_db: None,
            output_channels: None,
            follow_volume: None,
            chain_patch: patch,
            period_ms: None,
        };
        // Added by the bundle, removed and added again by hand.
        service
            .dispatch(Call::BundleLoad {
                path: dir.manifest("desk", Some(SUB)),
            })
            .unwrap();
        assert_eq!(service.config().outputs.followers.len(), 1);
        service
            .dispatch(Call::SetFollower {
                device: SUB.into(),
                remove: true,
                enabled: None,
                trim_ms: None,
                volume_db: None,
                output_channels: None,
                follow_volume: None,
                chain_patch: None,
                period_ms: None,
            })
            .unwrap();
        assert!(service.config().calibration.unwrap().followers.is_empty());
        service.dispatch(follower(None)).unwrap();
        service.dispatch(Call::BundleUnload).unwrap();
        assert_eq!(
            service.config().outputs.followers.len(),
            1,
            "the user's stays"
        );

        // Its own IR before the bundle goes missing: unload skips it.
        let own_conv = serde_json::json!({"convolution": [{"ir_path": own}]});
        service.dispatch(follower(Some(own_conv.clone()))).unwrap();
        service
            .dispatch(Call::BundleLoad {
                path: dir.manifest("desk", Some(SUB)),
            })
            .unwrap();
        std::fs::remove_file(&own).unwrap();
        let reply = match service.dispatch(Call::BundleUnload).unwrap() {
            Dispatch::Value(v) => v,
            other => panic!("unexpected {other:?}"),
        };
        let note = reply["notes"][0].as_str().unwrap().to_string();
        assert!(
            note.contains("own.wav") && note.contains("not put back"),
            "{note}"
        );
        assert_eq!(service.config().outputs.followers[0].chain_patch, None);

        // What the user sets while calibrated is what unload puts back;
        // the calibration's item keeps playing meanwhile.
        dir.wav("own.wav", 0.5);
        service.dispatch(follower(Some(own_conv))).unwrap();
        service
            .dispatch(Call::BundleLoad {
                path: dir.manifest("desk", Some(SUB)),
            })
            .unwrap();
        std::fs::remove_file(&own).unwrap();
        service
            .dispatch(follower(Some(serde_json::json!({"convolution": []}))))
            .unwrap();
        let patch = service.config().outputs.followers[0]
            .chain_patch
            .clone()
            .unwrap();
        assert_eq!(
            patch["convolution"][0]["calibration"], true,
            "still the bundle's"
        );
        assert!(matches!(
            service.dispatch(Call::BundleUnload).unwrap(),
            Dispatch::Value(Value::Null)
        ));
        assert_eq!(
            service.config().outputs.followers[0].chain_patch,
            Some(serde_json::json!({"convolution": []}))
        );
        service.stop_persist();
    }

    /// A master-only calibration never reaches another output: a follower
    /// it does not name plays chain A without the calibration's item.
    #[test]
    fn a_master_calibration_stays_off_the_other_followers() {
        let dir = BundleDir::new("bundle-masteronly");
        dir.wav("ll.wav", 1.0);
        let mine = dir.wav("mine.wav", 0.5);
        let mut config = EngineConfig {
            device: "hw:CARD=USB,DEV=0".into(),
            ..EngineConfig::default()
        };
        config.chain.convolution = vec![audivane_protocol::ConvolutionConfig {
            name: None,
            enabled: true,
            ir_path: mine,
            gain_db: 0.0,
            matrix: None,
            calibration: false,
        }];
        config
            .outputs
            .followers
            .push(FollowerConfig::new("hw:CARD=X,DEV=0"));
        let service = bundle_service(config, &dir);
        service
            .dispatch(Call::BundleLoad {
                path: dir.manifest("mains", None),
            })
            .unwrap();
        let cfg = service.config();
        assert_eq!(cfg.chain.convolution.len(), 2);
        let f = &cfg.outputs.followers[0];
        let own = super::super::follower_chain::config(&cfg, f);
        assert_eq!(
            own.chain.convolution,
            vec![cfg.chain.convolution[0].clone()]
        );
        service.stop_persist();
    }

    #[test]
    fn ab_overload_fallback_cancels_measurement_and_reports_disabled() {
        struct OverloadMock {
            inner: MockEngine,
            rt_overloads: AtomicU64,
            generation: AtomicU64,
        }
        impl Engine for OverloadMock {
            fn apply(&self, cfg: &EngineConfig) -> Result<(), EngineError> {
                self.generation.fetch_add(1, Ordering::Relaxed);
                self.inner.apply(cfg)
            }
            fn applied_generation(&self) -> u64 {
                self.generation.load(Ordering::Relaxed)
            }
            fn ab_select(&self, side: AbSide, crossfade_ms: f64) -> Result<AbStatus, EngineError> {
                self.inner.ab_select(side, crossfade_ms)
            }
            fn snapshot(&self) -> EngineSnapshot {
                let mut s = self.inner.snapshot();
                s.state = EngineState::Playing;
                s.chain_generation = self.generation.load(Ordering::Relaxed);
                s.gaps.dsp_wanted = 1;
                s.gaps.processed_periods = 100;
                let ov = self.rt_overloads.load(Ordering::Relaxed);
                s.gaps.rt_overloads = ov;
                s.gaps.rt_signals = ov;
                s.gaps.rt_overload_generation = s.chain_generation;
                s
            }
            fn meters(&self) -> Meters {
                self.inner.meters()
            }
            fn selftest(&self) -> Result<audivane_protocol::ipc::SelfTestResult, EngineError> {
                self.inner.selftest()
            }
            fn release_device(&self) -> Result<(), EngineError> {
                Ok(())
            }
            fn acquire_device(&self) -> Result<(), EngineError> {
                Ok(())
            }
            fn shutdown(&self) {}
            fn set_ab_measure(&self, epoch: u32) {
                self.inner.set_ab_measure(epoch);
            }
            fn take_ab_hops(&self, out: &mut Vec<audivane_dsp_core::ab_measure::AbHop>) {
                self.inner.take_ab_hops(out);
            }
        }

        let engine = Arc::new(OverloadMock {
            inner: MockEngine {
                config: Mutex::new(EngineConfig {
                    device: "hw:0,0".into(),
                    ..EngineConfig::default()
                }),
                applies: AtomicU64::new(0),
                refreshes: AtomicU64::new(0),
                applied: Mutex::new((None, 0)),
                measure: Mutex::new((0, Vec::new())),
                graph_rate: Mutex::new(None),
                live_suspends: AtomicU64::new(0),
                caps: Mutex::new(None),
            },
            rt_overloads: AtomicU64::new(0),
            generation: AtomicU64::new(0),
        });
        let service = Service::new(
            Arc::clone(&engine) as Arc<dyn Engine>,
            engine.inner.config.lock().unwrap().clone(),
            None,
            None,
            None,
            ShutdownFlag::new(),
        );

        service
            .dispatch(Call::SetAb(Some(AbConfig::chain(ChainConfig::neutral()))))
            .unwrap();
        service
            .dispatch(Call::AbMeasure {
                max_seconds: Some(30.0),
                cancel: false,
                accept: false,
            })
            .unwrap();
        assert!(service.ab_status().enabled);
        assert!(service.ab_status().measuring.is_some());
        assert_ne!(engine.inner.measure.lock().unwrap().0, 0);

        // Settle, then trip an RT overload: the first fallback step turns A/B off.
        for _ in 0..8 {
            service.overload_tick();
        }
        engine.rt_overloads.store(1, Ordering::Relaxed);
        for _ in 0..8 {
            service.overload_tick();
        }

        assert!(
            service.overload_report().is_some(),
            "overload fallback should be active"
        );
        assert_eq!(
            engine.inner.measure.lock().unwrap().0,
            0,
            "engine measurement epoch must be cleared"
        );
        let ab = service.ab_status();
        assert!(
            !ab.enabled,
            "ab_status must report disabled during fallback"
        );
        assert!(ab.measuring.is_none(), "measurement must be cancelled");
        assert!(
            ab.measure_error
                .as_deref()
                .is_some_and(|e| e.contains("overload")),
            "reason must stay visible: {:?}",
            ab.measure_error
        );
        let status_ab = service
            .status()
            .ab
            .expect("status.ab should stay present with reason");
        assert!(
            !status_ab.enabled,
            "status.ab must report disabled during fallback"
        );
        assert_eq!(status_ab.measure_error, ab.measure_error);
    }
}
