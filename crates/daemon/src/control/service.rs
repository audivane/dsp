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
use crate::control::config::{self, ConfigStore};
use crate::control::graph::{Facts, GraphWatcher};
use crate::control::mpris::MprisWatcher;
use crate::control::passthrough::{self, SinkView, StreamView};
use crate::control::persist::Persister;
use crate::control::presets::{self, PresetError, PresetStore};
use audivane_protocol::ipc::{
    AbConfig, AbSide, AbStatus, Call, DeviceInfo, EngineConfig, EngineState, LevelMatch, Meters,
    NowPlaying, Passthrough, Preset, PresetInfo, PresetState, ResponseWindowRequest, RoutingMode,
    RpcError, SinkChoice, Spectrum, Status, StreamInfo, Topic, VOLUME_MAX_DB, VOLUME_MIN_DB,
    VolumeConfig, VolumeStatus,
};
use audivane_protocol::{ChainConfig, UpsamplerConfig, resolve_ratio};
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

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
    /// Serializes all config, preset, routing, and A/B mutations so engine
    /// apply, routing updates, persistence queuing, and preset files stay in
    /// commit order.
    mutation: Mutex<()>,
    config: Mutex<EngineConfig>,
    store: Option<Arc<ConfigStore>>,
    /// Coalesced writer for `config.json` (None when there is no store).
    persister: Option<Arc<Persister>>,
    /// Chain of the active preset, as last loaded/saved. `Status.preset.modified`
    /// compares chain A with it, so `Status` never has to read the preset dir.
    active_preset_chain: Mutex<Option<(String, ChainConfig)>>,
    graph: Option<Arc<GraphWatcher>>,
    mpris: Option<Arc<MprisWatcher>>,
    shutdown: Arc<ShutdownFlag>,
    gate: Mutex<DispatchGate>,
    gate_cv: Condvar,
    /// Current A/B side, as last commanded. The engine snapshot is preferred
    /// when it reports one.
    ab_current: Mutex<AbSide>,
    /// Bumped on every config change (any client or file load); the event
    /// pump uses it to push `Event::Config`.
    config_gen: AtomicU64,
    /// Bumped whenever the preset directory changes through the daemon.
    presets_gen: AtomicU64,
    /// Recently measured upsampler responses, keyed by design and source rate.
    /// Reopening the view is instant; measuring a long kernel is expensive.
    /// Each entry keeps the measured impulse, so zoom windows are evaluated
    /// from it without measuring again.
    upsampler_cache: Mutex<Vec<UpsamplerCacheEntry>>,
}

use audivane_dsp_core::resample::MeasuredUpsampler;

type UpsamplerCacheEntry = ((UpsamplerConfig, u32), Arc<MeasuredUpsampler>);

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
        let active_preset_chain = load_active_preset(store.as_ref(), &config);
        Arc::new(Self {
            engine,
            mutation: Mutex::new(()),
            config: Mutex::new(config),
            store,
            persister,
            active_preset_chain: Mutex::new(active_preset_chain),
            graph,
            mpris,
            shutdown,
            gate: Mutex::new(DispatchGate::default()),
            gate_cv: Condvar::new(),
            ab_current: Mutex::new(AbSide::A),
            config_gen: AtomicU64::new(0),
            presets_gen: AtomicU64::new(0),
            upsampler_cache: Mutex::new(Vec::new()),
        })
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
    fn cached_upsampler_response(
        &self,
        key: &(UpsamplerConfig, u32),
    ) -> Option<Arc<MeasuredUpsampler>> {
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

    fn store_upsampler_response(
        &self,
        key: (UpsamplerConfig, u32),
        measured: Arc<MeasuredUpsampler>,
    ) {
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
                if let Some(note) = config::normalize_config(&mut cfg) {
                    log::info!("set_config: {note}");
                }
                config::validate(&cfg).map_err(invalid_params)?;
                self.apply_config(cfg)?;
                Ok(Value::Null)
            }
            Call::SetEq(eq) => {
                self.edit(|cfg| cfg.chain.eq = eq)?;
                Ok(Value::Null)
            }
            Call::SetConvolution(conv) => {
                self.edit(|cfg| cfg.chain.convolution = conv)?;
                Ok(Value::Null)
            }
            Call::SetUpsampler(upsampler) => {
                self.edit(|cfg| cfg.chain.upsampler = upsampler)?;
                Ok(Value::Null)
            }
            Call::SetDspEnabled { enabled } => {
                self.edit(|cfg| cfg.dsp_enabled = enabled)?;
                Ok(Value::Null)
            }
            Call::SetAb(ab) => {
                let (status, notes) = self.set_ab(ab)?;
                let mut value = to_value(&status)?;
                if !notes.is_empty() {
                    // Additive: clients that only want `AbStatus` ignore it.
                    if let Some(object) = value.as_object_mut() {
                        object.insert(
                            "notes".into(),
                            Value::Array(notes.into_iter().map(Value::String).collect()),
                        );
                    }
                }
                Ok(value)
            }
            Call::AbSelect { side, crossfade_ms } => {
                let status = self.ab_select(side, crossfade_ms)?;
                to_value(&status)
            }
            Call::SetVolume {
                volume_db,
                balance_db,
                muted,
            } => {
                let cfg = self.edit(|cfg| {
                    cfg.volume = VolumeConfig {
                        volume_db,
                        balance_db,
                        muted,
                    };
                })?;
                let snap = self.engine.snapshot();
                to_value(&self.volume_status(&cfg, &snap))
            }
            Call::ListStreams => to_value(&self.streams()),
            Call::ListPresets => to_value(&self.list_presets()?),
            Call::GetPreset { name } => to_value(&self.get_preset(&name)?),
            Call::SavePreset { name, overwrite } => to_value(&self.save_preset(&name, overwrite)?),
            Call::LoadPreset { name } => {
                self.load_preset(&name)?;
                Ok(Value::Null)
            }
            Call::DeletePreset { name } => {
                self.delete_preset(&name)?;
                Ok(Value::Null)
            }
            Call::RenamePreset { from, to } => to_value(&self.rename_preset(&from, &to)?),
            Call::ImportApo { name, text } => to_value(&self.import_apo(&name, &text)?),
            Call::ListDevices => to_value(&self.devices()),
            Call::Route { target, sink } => {
                self.route(target, sink)?;
                Ok(Value::Null)
            }
            Call::SetRoutingMode(mode) => {
                self.edit(|cfg| cfg.routing = mode)?;
                Ok(Value::Null)
            }
            Call::NowPlaying => to_value(&self.now_playing()),
            Call::SelfTest => {
                let result = self.engine.selftest().map_err(engine_error)?;
                to_value(&result)
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
                let config = config.unwrap_or(active.chain.upsampler);
                let source_rate = source_rate.unwrap_or_else(|| {
                    let graph = self.engine.snapshot().graph_rate;
                    if graph > 0 { graph } else { 44_100 }
                });
                if !(8_000..=1_536_000).contains(&source_rate) {
                    return Err(invalid_params(format!(
                        "source_rate must be between 8000 and 1536000 Hz (got {source_rate})"
                    )));
                }
                // An explicit config must obey the same kernel limits as the
                // active chain (taps, cutoff, beta, target), not just the
                // resolved ratio.
                config::validate_upsampler("upsampler", &config).map_err(invalid_params)?;
                let ratio = resolve_ratio(config.target, source_rate);
                if !(1..=32).contains(&ratio) {
                    return Err(invalid_params(format!(
                        "this design resolves to an unsupported upsampling ratio x{ratio}"
                    )));
                }
                if let Some(window) = &window {
                    let nyquist_out = f64::from(source_rate) * f64::from(ratio) / 2.0;
                    validate_response_window(window, nyquist_out).map_err(invalid_params)?;
                }
                let key = (config.clone(), source_rate);
                let measured = match self.cached_upsampler_response(&key) {
                    Some(measured) => measured,
                    None => {
                        let measured = Arc::new(
                            audivane_dsp_core::resample::measure_upsampler(&config, source_rate)
                                .map_err(invalid_params)?,
                        );
                        self.store_upsampler_response(key, Arc::clone(&measured));
                        measured
                    }
                };
                to_value(&measured.with_window(window))
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

    // ------------------------------------------------------------ config

    /// Validate, apply, remember and queue a whole config.
    ///
    /// If the engine rejects it nothing changes. The disk write is coalesced
    /// (see [`Persister`]): a queued write failure is logged and reported in
    /// `Status.error` instead of failing this call, because the reply may
    /// already be on the wire.
    fn apply_config(&self, cfg: EngineConfig) -> Result<(), RpcError> {
        let _mutation = self.lock_mutation();
        let mut cfg = cfg;
        // Chain B is normalized by the caller; keep this a hard invariant.
        config::normalize_config(&mut cfg);
        self.engine.apply(&cfg).map_err(engine_error)?;
        let preloaded = match &cfg.active_preset {
            Some(name) => {
                let already_cached = self
                    .active_preset_chain
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                    .is_some_and(|(cached, _)| cached == name);
                if already_cached {
                    None
                } else {
                    Some(load_active_preset(self.store.as_ref(), &cfg))
                }
            }
            None => Some(None),
        };
        {
            let mut guard = self.lock_config();
            *guard = cfg.clone();
            if let Some(entry) = preloaded {
                *self
                    .active_preset_chain
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = entry;
            }
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
        preset_cache: Option<(String, ChainConfig)>,
    ) -> Result<EngineConfig, RpcError>
    where
        F: FnOnce(&mut EngineConfig),
    {
        let mut next = self.config();
        edit(&mut next);
        // Normalize before validating: chain B must follow A, never make a
        // chain A edit fail because chain B still has the old upsampler.
        if let Some(note) = config::normalize_config(&mut next) {
            log::info!("{note}");
        }
        config::validate(&next).map_err(invalid_params)?;
        self.engine.apply(&next).map_err(engine_error)?;
        {
            let mut guard = self.lock_config();
            *guard = next.clone();
            if let Some(entry) = preset_cache {
                *self
                    .active_preset_chain
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(entry);
            }
        }
        self.apply_routing_locked(&next);
        self.note_config_change();
        self.persist(&next)?;
        Ok(next)
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
        Ok(self.preset_store()?.list())
    }

    /// Preset list for events/snapshots; never fails (an unreadable directory
    /// is logged and reads as empty).
    pub fn presets_info(&self) -> Vec<PresetInfo> {
        self.preset_store()
            .map(|store| store.list())
            .unwrap_or_default()
    }

    fn get_preset(&self, name: &str) -> Result<Preset, RpcError> {
        self.preset_store()?.load(name).map_err(preset_error)
    }

    fn save_preset(&self, name: &str, overwrite: bool) -> Result<PresetInfo, RpcError> {
        let _mutation = self.lock_mutation();
        let store = self.preset_store()?;
        let name = presets::normalize_name(name).map_err(invalid_params)?;
        let chain = self.config().chain;
        let exists = store.load(&name).is_ok();
        if exists && !overwrite {
            return Err(RpcError {
                code: RpcError::BAD_STATE,
                message: format!(
                    "a preset named {name:?} already exists; pass overwrite to replace it"
                ),
            });
        }
        let preset = Preset::new(name.clone(), chain.clone());
        presets::validate(&preset).map_err(invalid_params)?;
        let info = store.save(&preset).map_err(preset_error)?;
        let changed_cfg = {
            let mut guard = self.lock_config();
            *self
                .active_preset_chain
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some((name.clone(), chain));
            if guard.active_preset.as_deref() == Some(name.as_str()) {
                None
            } else {
                guard.active_preset = Some(name);
                Some(guard.clone())
            }
        };
        if let Some(cfg) = changed_cfg {
            self.note_config_change();
            self.persist(&cfg)?;
        }
        self.presets_gen.fetch_add(1, Ordering::Relaxed);
        Ok(info)
    }

    fn load_preset(&self, name: &str) -> Result<(), RpcError> {
        let _mutation = self.lock_mutation();
        let store = self.preset_store()?;
        let name = presets::normalize_name(name).map_err(invalid_params)?;
        let preset = store.load(&name).map_err(preset_error)?;
        presets::validate(&preset).map_err(invalid_params)?;
        let chain = preset.chain.clone();
        let preset_name = preset.name.clone();
        self.edit_locked(
            |cfg| {
                cfg.chain = chain;
                cfg.active_preset = Some(preset_name);
            },
            Some((preset.name, preset.chain)),
        )?;
        Ok(())
    }

    fn delete_preset(&self, name: &str) -> Result<(), RpcError> {
        let _mutation = self.lock_mutation();
        let store = self.preset_store()?;
        let name = presets::normalize_name(name).map_err(invalid_params)?;
        let was_active = self.config().active_preset.as_deref() == Some(name.as_str());
        store.delete(&name).map_err(preset_error)?;
        if was_active {
            let changed_cfg = {
                let mut guard = self.lock_config();
                *self
                    .active_preset_chain
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = None;
                if guard.active_preset.is_none() {
                    None
                } else {
                    guard.active_preset = None;
                    Some(guard.clone())
                }
            };
            if let Some(cfg) = changed_cfg {
                self.note_config_change();
                self.persist(&cfg)?;
            }
        }
        self.presets_gen.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn rename_preset(&self, from: &str, to: &str) -> Result<PresetInfo, RpcError> {
        let _mutation = self.lock_mutation();
        let store = self.preset_store()?;
        let from = presets::normalize_name(from).map_err(invalid_params)?;
        let to = presets::normalize_name(to).map_err(invalid_params)?;
        let was_active = self.config().active_preset.as_deref() == Some(from.as_str());
        let info = store.rename(&from, &to).map_err(preset_error)?;
        if was_active {
            let changed_cfg = {
                let mut guard = self.lock_config();
                if let Some((name, _)) = self
                    .active_preset_chain
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_mut()
                {
                    *name = info.name.clone();
                }
                if guard.active_preset.as_deref() == Some(info.name.as_str()) {
                    None
                } else {
                    guard.active_preset = Some(info.name.clone());
                    Some(guard.clone())
                }
            };
            if let Some(cfg) = changed_cfg {
                self.note_config_change();
                self.persist(&cfg)?;
            }
        }
        self.presets_gen.fetch_add(1, Ordering::Relaxed);
        Ok(info)
    }

    /// Equalizer APO / AutoEQ text → preset with that EQ and flat defaults for
    /// the rest of the chain. It does not load the preset.
    fn import_apo(&self, name: &str, text: &str) -> Result<PresetInfo, RpcError> {
        let _mutation = self.lock_mutation();
        let store = self.preset_store()?;
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
        let preset = Preset::new(name.clone(), chain.clone());
        presets::validate(&preset).map_err(invalid_params)?;
        let info = store.save(&preset).map_err(preset_error)?;
        {
            let guard = self.lock_config();
            if guard.active_preset.as_deref() == Some(name.as_str()) {
                *self
                    .active_preset_chain
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some((name, chain));
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
        devices
    }

    // ------------------------------------------------------------ A/B

    /// Configure or clear chain B. Chain B's upsampler and output always
    /// follow chain A: a differing chain B is realigned (and the reply says
    /// so) rather than rejected, because the client usually has no way to
    /// express "A's settings" and a rejection would make `ab set` fail after
    /// any chain A change.
    fn set_ab(&self, ab: Option<AbConfig>) -> Result<(AbStatus, Vec<String>), RpcError> {
        let _mutation = self.lock_mutation();
        let mut notes = Vec::new();
        let mut next = self.config();
        next.ab = ab;
        if let Some(note) = config::normalize_config(&mut next) {
            notes.push(note);
        }
        config::validate(&next).map_err(invalid_params)?;
        self.engine.apply(&next).map_err(engine_error)?;
        *self.lock_config() = next.clone();
        *self.ab_current.lock().unwrap_or_else(|e| e.into_inner()) = AbSide::A;
        self.note_config_change();
        self.persist(&next)?;
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
        *self.ab_current.lock().unwrap_or_else(|e| e.into_inner()) = status.current;
        Ok(status)
    }

    /// Best-effort A/B status from the config (used when the engine reports
    /// nothing, e.g. before it has loaded the chain).
    fn ab_status(&self) -> AbStatus {
        let cfg = self.config();
        let current = *self.ab_current.lock().unwrap_or_else(|e| e.into_inner());
        match &cfg.ab {
            Some(ab) => {
                let offset_db = match ab.level {
                    LevelMatch::Off => 0.0,
                    LevelMatch::Manual { offset_db } => offset_db,
                    LevelMatch::Analytic => {
                        let graph_rate = self.engine.snapshot().graph_rate;
                        let rate = if graph_rate > 0 {
                            graph_rate
                        } else {
                            cfg.rate.default_rate.max(1)
                        };
                        audivane_dsp_core::chain::AbChain::analytic_offset_db(
                            &cfg.chain,
                            &ab.chain_b,
                            rate,
                        )
                    }
                };
                AbStatus {
                    enabled: true,
                    current,
                    offset_db,
                }
            }
            None => AbStatus {
                enabled: false,
                current,
                offset_db: 0.0,
            },
        }
    }

    // ------------------------------------------------------------ status

    pub fn status(&self) -> Status {
        let (cfg, preset) = {
            let guard = self.lock_config();
            let preset = self.preset_state(&guard);
            (guard.clone(), preset)
        };
        let snap = self.engine.snapshot();
        let facts = self.facts();

        let graph_rate = if snap.graph_rate > 0 {
            snap.graph_rate
        } else {
            cfg.rate.default_rate
        };
        let ratio = if snap.ratio > 0 {
            snap.ratio
        } else {
            resolve_ratio(cfg.chain.upsampler.target, graph_rate).max(1)
        };
        let output_rate = if snap.output_rate > 0 {
            snap.output_rate
        } else {
            graph_rate.saturating_mul(ratio)
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

        let ab = snap.ab.clone().or_else(|| {
            cfg.ab
                .as_ref()
                .map(|_| self.ab_status())
                .filter(|status| status.enabled)
        });
        let volume = Some(self.volume_status(&cfg, &snap));

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
            quantum,
            latency_ms: snap.latency_ms,
            filter_latency_ms: snap.filter_latency_ms,
            xruns: snap.xruns,
            dsp_load: snap.dsp_load,
            dsp_enabled: cfg.dsp_enabled,
            active_sink,
            passthrough,
            ab,
            preset,
            volume,
            device_caps: snap.device_caps.clone(),
            effective_rates: snap.effective_rates.clone(),
            direct_rates: snap.direct_rates.clone(),
            effective_default_rate: snap.effective_default_rate,
            dither_bits: snap.dither_bits,
            normal_output: facts.normal_output.clone(),
        }
    }

    /// The active preset and whether chain A still matches it.
    fn preset_state(&self, cfg: &EngineConfig) -> Option<PresetState> {
        let name = cfg.active_preset.as_ref()?;
        let cache = self
            .active_preset_chain
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (cached_name, chain) = cache.as_ref()?;
        (cached_name == name).then(|| PresetState {
            name: name.clone(),
            modified: *chain != cfg.chain,
        })
    }

    fn volume_status(&self, cfg: &EngineConfig, snap: &crate::api::EngineSnapshot) -> VolumeStatus {
        snap.volume.unwrap_or(VolumeStatus {
            volume_db: cfg.volume.volume_db,
            balance_db: cfg.volume.balance_db,
            muted: cfg.volume.muted,
            hardware: false,
            min_db: VOLUME_MIN_DB,
            max_db: VOLUME_MAX_DB,
        })
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
            })
            .collect()
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

    /// Convenience for tests and the daemon start-up path.
    pub fn refresh(&self) {
        let _mutation = self.lock_mutation();
        let cfg = self.config();
        self.apply_routing_locked(&cfg);
    }
}

fn to_value<T: serde::Serialize>(value: &T) -> Result<Value, RpcError> {
    serde_json::to_value(value).map_err(|err| RpcError {
        code: RpcError::INTERNAL,
        message: format!("cannot serialize reply: {err}"),
    })
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

/// Load the chain of the preset named by `cfg.active_preset`, if any.
fn load_active_preset(
    store: Option<&Arc<ConfigStore>>,
    cfg: &EngineConfig,
) -> Option<(String, ChainConfig)> {
    let name = cfg.active_preset.as_ref()?;
    let store = store?;
    match PresetStore::alongside(store).load(name) {
        Ok(preset) => Some((preset.name, preset.chain)),
        Err(err) => {
            log::warn!("active preset {name:?}: {err}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::EngineSnapshot;
    use audivane_protocol::ipc::{AbSide, EngineState, Meters};
    use audivane_protocol::{ChainConfig, EqProfile, UpsamplerConfig};
    use std::sync::atomic::{AtomicU64, Ordering};

    struct MockEngine {
        config: Mutex<EngineConfig>,
        applies: AtomicU64,
    }

    impl Engine for MockEngine {
        fn apply(&self, cfg: &EngineConfig) -> Result<(), EngineError> {
            *self.config.lock().unwrap() = cfg.clone();
            self.applies.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
        fn ab_select(&self, side: AbSide, _crossfade_ms: f64) -> Result<AbStatus, EngineError> {
            Ok(AbStatus {
                enabled: true,
                current: side,
                offset_db: -3.0,
            })
        }
        fn snapshot(&self) -> EngineSnapshot {
            let cfg = self.config.lock().unwrap().clone();
            EngineSnapshot {
                state: EngineState::Ready,
                device: cfg.device.clone(),
                error: None,
                format: None,
                graph_rate: 48_000,
                output_rate: 48_000,
                ratio: 1,
                quantum: cfg.quantum,
                latency_ms: 0.0,
                filter_latency_ms: 0.0,
                xruns: 0,
                dsp_load: 0.0,
                dsp_active: false,
                active_sink: None,
                ab: None,
                volume: None,
                dsp_node: None,
                direct_node: None,
                frames_out: 0,
                chain_generation: 0,
                gaps: Default::default(),
                device_caps: None,
                effective_rates: Vec::new(),
                direct_rates: Vec::new(),
                effective_default_rate: None,
                dither_bits: None,
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
    }

    fn service() -> (Arc<Service>, Arc<MockEngine>) {
        let engine = Arc::new(MockEngine {
            config: Mutex::new(EngineConfig {
                device: "hw:0,0".into(),
                ..EngineConfig::default()
            }),
            applies: AtomicU64::new(0),
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
    }

    #[test]
    fn edits_are_validated_and_applied() {
        let (service, engine) = service();
        let err = service
            .dispatch(Call::SetUpsampler(UpsamplerConfig {
                taps: 0,
                ..UpsamplerConfig::default()
            }))
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

    #[test]
    fn upsampler_response_rejects_invalid_explicit_config() {
        let (service, _) = service();
        let bad = |f: fn(&mut UpsamplerConfig)| {
            let mut cfg = UpsamplerConfig {
                target: audivane_protocol::RateTarget::Ratio(4),
                ..UpsamplerConfig::default()
            };
            f(&mut cfg);
            cfg
        };

        for (cfg, needle) in [
            (bad(|c| c.taps = usize::MAX), "taps"),
            (bad(|c| c.cutoff = 1.5), "cutoff"),
            (bad(|c| c.kaiser_beta = 99.0), "kaiser_beta"),
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
        let cfg = UpsamplerConfig {
            target: audivane_protocol::RateTarget::Ratio(2),
            taps: 512,
            ..UpsamplerConfig::default()
        };
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

    #[test]
    fn upsampler_response_rejects_invalid_windows() {
        let (service, _) = service();
        let cfg = UpsamplerConfig {
            target: audivane_protocol::RateTarget::Ratio(2),
            taps: 512,
            ..UpsamplerConfig::default()
        };
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
                chain_b: ChainConfig::default(),
                level: LevelMatch::Manual { offset_db: -2.0 },
            })))
            .unwrap()
        {
            Dispatch::Value(value) => serde_json::from_value::<AbStatus>(value).unwrap(),
            other => panic!("unexpected {other:?}"),
        };
        assert!(status.enabled);
        assert_eq!(status.current, AbSide::A);

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
        assert_eq!(status.current, AbSide::B);
        assert_eq!(status.offset_db, -3.0);
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
            ..ChainConfig::default()
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
            })
            .unwrap();
        assert!(!service.status().preset.unwrap().modified);

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
        assert_eq!(reply.min_db, VOLUME_MIN_DB);
        assert_eq!(reply.max_db, VOLUME_MAX_DB);
        assert_eq!(engine.applies.load(Ordering::Relaxed), 1);

        let status = service.status();
        assert_eq!(status.volume, Some(reply));
    }
}
