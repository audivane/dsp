//! Real-time side: owns the DAC (ALSA hw), the two PipeWire driver sinks
//! ("Audivane DSP", "Audivane Direct") and the DSP chains.

pub mod ir;
pub mod processor;
pub mod spectrum;
pub mod waiting;

use crate::api::{Engine, EngineError, EngineSnapshot};
use audivane_dsp_core::bypass::{hash_i32, i32_to_f32};
use audivane_dsp_core::chain::{AbChain, Chain, ChainSpec, Side};
use audivane_io_alsa::{DEFAULT_RESERVE_PRIORITY, MixerApplyOutcome, apply_playback_volume};
use audivane_io_pw::{
    DriverWaker, EngineDriverConfig, EngineDriverHandle, NegotiatedChain, RatioNegotiator,
    scaled_quantum,
};
use audivane_protocol::ipc::{
    AbSide, AbStatus, BALANCE_MAX_DB, EngineConfig, EngineState, LevelMatch, Meters, RpcError,
    SelfTestResult, SinkChoice, Spectrum, VOLUME_MAX_DB, VOLUME_MIN_DB, VolumeStatus,
};
use audivane_protocol::{
    ChainConfig, RateTarget, SampleFormat, effective_dither_bits, resolve_ratio,
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
const MAX_DSP_RATIO: usize = 16;

type MixerFn = Arc<dyn Fn(&str, f64, bool) -> MixerApplyOutcome + Send + Sync>;

fn apply_alsa_mixer(device: &str, volume_db: f64, muted: bool) -> MixerApplyOutcome {
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
    build_ratio: u32,
    ab_offset_db: f64,
    has_effect: bool,
    ab_enabled: bool,
}

struct BuilderState {
    cfg: EngineConfig,
    ir_cache: IrCache,
    ab_side: AbSide,
    ab_offset_db: f64,
    last_rate: u32,
    last_ratio: u32,
    last_dsp_ratio: u32,
    /// Format ALSA negotiated on the last open: caps the dither depth.
    out_format: Option<SampleFormat>,
    config_generation: u64,
    built_generation: u64,
    next_swap_seq: u64,
    pending_publication: Option<PendingPublication>,
    has_pending_swap: Arc<AtomicBool>,
    shared_volume: Arc<SharedVolume>,
    mixer: MixerFn,
    hardware_volume: bool,
    cmd_tx: rtrb::Producer<RtDspCmd>,
    swap_tx: rtrb::Producer<PendingSwap>,
}

impl BuilderState {
    fn sync_volume(&mut self, mirror: &SharedMirror, waker: Option<&DriverWaker>) {
        let outcome = (self.mixer)(
            &self.cfg.device,
            self.cfg.volume.volume_db,
            self.cfg.volume.muted,
        );
        self.hardware_volume = outcome.hardware_volume;
        self.shared_volume.store(VolumeTarget {
            volume_db: self.cfg.volume.volume_db,
            balance_db: self.cfg.volume.balance_db,
            muted: self.cfg.volume.muted,
            hardware_volume: outcome.hardware_volume,
            hardware_mute: outcome.hardware_mute,
        });
        mirror.set_volume(VolumeStatus {
            volume_db: outcome.volume_db,
            balance_db: self.cfg.volume.balance_db,
            muted: self.cfg.volume.muted,
            hardware: outcome.hardware_volume,
            min_db: outcome.min_db,
            max_db: outcome.max_db,
        });
        mirror.dsp_has_effect.store(
            dsp_has_effect(&self.cfg, self.hardware_volume),
            Ordering::Release,
        );
        if let Some(w) = waker {
            w.wake_rt();
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
        build_ratio: u32,
        ab_offset_db: f64,
        has_effect: bool,
        ab_enabled: bool,
        mirror: &SharedMirror,
    ) {
        self.built_generation = built_generation;
        self.last_dsp_ratio = build_ratio;
        self.ab_offset_db = ab_offset_db;
        mirror.dsp_has_effect.store(has_effect, Ordering::Release);
        // A/B selection is independent engine state (`self.ab_side`), never
        // publication metadata: a deferred publication must not restore the
        // side that happened to be selected when the chain was built.
        mirror.set_ab(ab_enabled, self.ab_side, ab_offset_db);
    }

    /// Attempt to push `pub_item` into `swap_tx`. Only advances
    /// `built_generation` and mirror metadata once `swap_tx` actually accepts
    /// the chain; if full, keeps the latest swap in `pending_publication` for
    /// background retry by the chain reaper until accepted or superseded.
    ///
    /// The latest A/B selection is applied to the chain immediately before the
    /// push (and again on every deferred retry), so selecting B while a swap is
    /// deferred cannot be undone when the queued chain is finally installed.
    fn publish_or_defer(
        &mut self,
        mut pub_item: PendingPublication,
        mirror: &SharedMirror,
        waker: Option<&DriverWaker>,
    ) -> bool {
        pub_item.swap.chain.apply_ab_side(self.ab_side);
        match self.swap_tx.push(pub_item.swap) {
            Ok(()) => {
                self.commit_published(
                    pub_item.built_generation,
                    pub_item.build_ratio,
                    pub_item.ab_offset_db,
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
        build_ratio: u32,
        ab_offset_db: f64,
        has_effect: bool,
        crossfade: bool,
        generation: u64,
        mirror: &SharedMirror,
        waker: Option<&DriverWaker>,
    ) -> u64 {
        self.pending_publication = None;
        self.has_pending_swap.store(false, Ordering::Release);
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
                build_ratio,
                ab_offset_db,
                has_effect,
                ab_enabled: cfg.ab.is_some(),
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
        let current_rate = drv_rate.max(8_000);
        validate_engine_config(cfg, &mut self.ir_cache, current_rate)?;

        let old_cfg = self.cfg.clone();
        let force_reopen =
            drv_state == EngineState::Error || diff_requires_reopen(&old_cfg, cfg, current_rate);
        let next_side = if cfg.ab.is_none() {
            AbSide::A
        } else {
            self.ab_side
        };
        let prev_gen = self.config_generation;
        let next_gen = prev_gen.saturating_add(1);

        let target_ratio = if force_reopen || drv_sink == Some(SinkChoice::Direct) {
            desired_ratio_for(cfg, current_rate)
        } else {
            self.last_dsp_ratio.max(1)
        };

        let mut old_without_volume = old_cfg.clone();
        old_without_volume.volume = cfg.volume;
        let volume_only = !force_reopen && old_without_volume == *cfg;

        // Pre-build the chain off-RT before committing config/mixer state so a
        // failing config leaves everything untouched.
        let built = if !volume_only && !force_reopen {
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

        // Always update `dsp_enabled` first so RT processor reflects toggle promptly.
        if old_cfg.dsp_enabled != cfg.dsp_enabled {
            self.push_cmd(RtDspCmd::SetDspEnabled(cfg.dsp_enabled), driver)?;
        }

        self.config_generation = next_gen;
        self.cfg = cfg.clone();
        mirror
            .output_bits
            .store(cfg.chain.output.bits, Ordering::Release);
        self.ab_side = next_side;

        let waker = driver.map(EngineDriverHandle::waker);
        self.sync_volume(mirror, waker.as_ref());

        if volume_only {
            if self.built_generation == prev_gen && self.pending_publication.is_none() {
                self.built_generation = next_gen;
            }
            return Ok(false);
        }

        if let Some((chain, ab_offset_db, chain_effect)) = built {
            mirror.clear_chain_error();
            let has_effect = chain_effect || dsp_has_effect(cfg, self.hardware_volume);
            let _ = self.publish_built(
                cfg,
                chain,
                target_ratio,
                ab_offset_db,
                has_effect,
                true,
                next_gen,
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
            let ab_offset_db = compute_ab_offset(cfg, current_rate);
            self.ab_offset_db = ab_offset_db;
            self.last_dsp_ratio = target_ratio;
            mirror
                .dsp_has_effect
                .store(dsp_has_effect(cfg, self.hardware_volume), Ordering::Release);
            mirror.set_ab(cfg.ab.is_some(), next_side, ab_offset_db);
        }

        Ok(force_reopen)
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_negotiated(
        &mut self,
        graph_rate: u32,
        actual_ratio: u32,
        active_sink: SinkChoice,
        format: SampleFormat,
        mirror: &SharedMirror,
        ab_atomic: &AtomicU8,
        waker: Option<&DriverWaker>,
    ) -> NegotiatedChain {
        let rate_changed = self.last_rate != graph_rate;
        let format_changed = self.out_format != Some(format);
        self.out_format = Some(format);
        let cfg_stale =
            self.built_generation != self.config_generation || self.pending_publication.is_some();
        self.last_rate = graph_rate;
        self.last_ratio = actual_ratio;
        // The chain is built for the format just negotiated: dither never
        // targets more bits than the DAC format carries.
        let cfg_clone = cap_dither_bits(&self.cfg, Some(format));
        let cur_side = self.ab_side;
        let cfg_gen = self.config_generation;
        ab_atomic.store(side_to_u8(cur_side), Ordering::Release);

        // Re-apply hardware/digital volume on every open/reopen/acquire and
        // device change before early-returning or publishing the chain.
        self.sync_volume(mirror, waker);
        let hw_vol = self.hardware_volume;

        let build_ratio = if active_sink == SinkChoice::Direct {
            if !rate_changed && !format_changed && !cfg_stale && self.last_dsp_ratio > 0 {
                // Direct only temporarily holds ALSA at 1x and no config or rate
                // change occurred; keep the warm DSP chain untouched.
                return NegotiatedChain {
                    generation: cfg_gen,
                    ..NegotiatedChain::default()
                };
            }
            desired_ratio_for(&cfg_clone, graph_rate)
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
            Ok((chain, ab_offset_db, chain_effect)) => {
                mirror.clear_chain_error();
                let has_effect = chain_effect || dsp_has_effect(&cfg_clone, hw_vol);
                let swap_seq = self.publish_built(
                    &cfg_clone,
                    chain,
                    build_ratio,
                    ab_offset_db,
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
                }
            }
            Err(e) => {
                let primary = format!(
                    "failed to rebuild DSP chain for ({graph_rate} Hz x{build_ratio}): {e}"
                );
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
                    Ok((chain, ab_offset_db, chain_effect)) => {
                        let has_effect = chain_effect || dsp_has_effect(&cfg_clone, hw_vol);
                        let swap_seq = self.publish_built(
                            &cfg_clone,
                            chain,
                            build_ratio,
                            ab_offset_db,
                            has_effect,
                            false,
                            cfg_gen,
                            mirror,
                            waker,
                        );
                        let msg = format!(
                            "{primary}; playing fallback chain instead (failing stage disabled)"
                        );
                        log::warn!("{msg}");
                        mirror.set_chain_error(msg.clone());
                        NegotiatedChain {
                            required_chain_seq: gate_seq(swap_seq),
                            error: Some(msg),
                            fatal: false,
                            generation: cfg_gen,
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
                        }
                    }
                }
            }
        }
    }
}

/// Shared non-RT chain-building + status atomics accessed both by [`RealEngine`]
/// and by the [`RatioNegotiator`] callback on ALSA reopen.
struct SharedMirror {
    dsp_has_effect: AtomicBool,
    /// Configured `chain.output.bits` ("Dither to"), before the format cap.
    output_bits: AtomicU32,
    ab_enabled: AtomicBool,
    ab_side: AtomicU8,
    ab_offset_bits: AtomicU64,
    volume_status: Mutex<VolumeStatus>,
    chain_error: Mutex<Option<String>>,
}

impl SharedMirror {
    fn new(cfg: &EngineConfig) -> Self {
        let has_effect = dsp_has_effect(cfg, false);
        Self {
            dsp_has_effect: AtomicBool::new(has_effect),
            output_bits: AtomicU32::new(cfg.chain.output.bits),
            ab_enabled: AtomicBool::new(cfg.ab.is_some()),
            ab_side: AtomicU8::new(0),
            ab_offset_bits: AtomicU64::new(0.0_f64.to_bits()),
            volume_status: Mutex::new(VolumeStatus {
                volume_db: cfg.volume.volume_db,
                balance_db: cfg.volume.balance_db,
                muted: cfg.volume.muted,
                hardware: false,
                min_db: VOLUME_MIN_DB,
                max_db: VOLUME_MAX_DB,
            }),
            chain_error: Mutex::new(None),
        }
    }

    fn set_volume(&self, status: VolumeStatus) {
        if let Ok(mut g) = self.volume_status.lock() {
            *g = status;
        }
    }

    fn volume_status(&self) -> VolumeStatus {
        self.volume_status
            .lock()
            .map(|g| *g)
            .unwrap_or(VolumeStatus {
                volume_db: 0.0,
                balance_db: 0.0,
                muted: false,
                hardware: false,
                min_db: VOLUME_MIN_DB,
                max_db: VOLUME_MAX_DB,
            })
    }

    fn set_ab(&self, enabled: bool, side: AbSide, offset_db: f64) {
        self.ab_enabled.store(enabled, Ordering::Release);
        self.ab_side.store(side_to_u8(side), Ordering::Release);
        self.ab_offset_bits
            .store(offset_db.to_bits(), Ordering::Release);
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
        Some(AbStatus {
            enabled: true,
            current: u8_to_side(self.ab_side.load(Ordering::Acquire)),
            offset_db: f64::from_bits(self.ab_offset_bits.load(Ordering::Acquire)),
        })
    }
}

pub struct RealEngine {
    builder: Arc<Mutex<BuilderState>>,
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
}

impl RealEngine {
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
        let mut ir_cache = IrCache::new();
        let init_rate = cfg.rate.default_rate.max(8_000);
        validate_engine_config(cfg, &mut ir_cache, init_rate)?;

        let desired_ratio = desired_ratio_for(cfg, init_rate);
        let init_max_block = scaled_quantum(cfg.quantum, init_rate).clamp(512, 8192) as usize;

        let (cmd_tx, cmd_rx) = rtrb::RingBuffer::<RtDspCmd>::new(64);
        let (swap_tx, swap_rx) = rtrb::RingBuffer::<PendingSwap>::new(64);
        let (recycle_tx, mut recycle_rx) = rtrb::RingBuffer::<ActiveChain>::new(64);

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
            MAX_DSP_RATIO.max(desired_ratio as usize),
            init_rate,
            Arc::clone(&ab_side_atomic),
            Arc::clone(&meters),
            cmd_rx,
            swap_rx,
            recycle_tx,
        )
        .with_spectrum(spectrum_rt);
        if std::env::var_os("AUDIVANE_TEST_PAUSE_RT_DRAIN").is_some_and(|v| v == "1" || v == "true")
        {
            processor.set_swap_drain_paused(true);
        }
        let shared_volume = processor.volume_handle();
        let chain_generation = processor.active_generation_handle();
        let rt_drain_paused = processor.swap_drain_paused_handle();
        let has_pending_swap = Arc::new(AtomicBool::new(false));

        let builder = Arc::new(Mutex::new(BuilderState {
            cfg: cfg.clone(),
            ir_cache,
            ab_side: AbSide::A,
            ab_offset_db: 0.0,
            last_rate: init_rate,
            last_ratio: desired_ratio,
            last_dsp_ratio: desired_ratio,
            out_format: None,
            config_generation: 0,
            built_generation: 0,
            next_swap_seq: 0,
            pending_publication: None,
            has_pending_swap: Arc::clone(&has_pending_swap),
            shared_volume,
            mixer: Arc::new(apply_alsa_mixer),
            hardware_volume: false,
            cmd_tx,
            swap_tx,
        }));

        let builder_cb = Arc::clone(&builder);
        let mirror_cb = Arc::clone(&mirror);
        let ab_atomic_cb = Arc::clone(&ab_side_atomic);
        let waker_slot: Arc<OnceLock<DriverWaker>> = Arc::new(OnceLock::new());
        let waker_cb = Arc::clone(&waker_slot);
        let on_negotiated: RatioNegotiator =
            Arc::new(move |graph_rate, actual_ratio, active_sink, format| {
                let Ok(mut st) = builder_cb.lock() else {
                    return NegotiatedChain {
                        required_chain_seq: None,
                        error: Some("engine builder lock poisoned".into()),
                        fatal: true,
                        generation: 0,
                    };
                };
                st.apply_negotiated(
                    graph_rate,
                    actual_ratio,
                    active_sink,
                    format,
                    &mirror_cb,
                    &ab_atomic_cb,
                    waker_cb.get(),
                )
            });

        let drv_cfg = to_driver_config(cfg);
        let driver = EngineDriverHandle::spawn(drv_cfg, processor, Some(on_negotiated))
            .map_err(|e| EngineError::new(RpcError::IO, format!("driver start failed: {e}")))?;
        let _ = waker_slot.set(driver.waker());

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
                        if reaped {
                            reaper_waker.wake_rt();
                            woke_rt = true;
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
            })
            .map_err(|e| {
                driver.stop();
                EngineError::new(
                    RpcError::INTERNAL,
                    format!("spawn reaper thread failed: {e}"),
                )
            })?;

        Ok(Arc::new(Self {
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
            reaper_thread: Mutex::new(Some(reaper_thread)),
        }))
    }
}

impl Engine for RealEngine {
    fn apply(&self, cfg: &EngineConfig) -> Result<(), EngineError> {
        let mut st = self
            .builder
            .lock()
            .map_err(|_| EngineError::new(RpcError::INTERNAL, "engine builder lock poisoned"))?;
        let drv_snap = self.driver.snapshot();
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

        if let Ok(mut dev) = self.device_name.lock() {
            *dev = cfg.device.clone();
        }
        let drv_cfg = to_driver_config(cfg);
        self.driver.update_config(drv_cfg, force_reopen);
        drop(st);
        Ok(())
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
        let offset_db = st.ab_offset_db;
        self.mirror.set_ab(true, side, offset_db);
        st.push_cmd(
            RtDspCmd::AbSelect { side, crossfade_ms },
            Some(&self.driver),
        )?;

        Ok(AbStatus {
            enabled: true,
            current: side,
            offset_db,
        })
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

        let dsp_active = drv.state != EngineState::Idle
            && drv.active_sink != Some(SinkChoice::Direct)
            && self.mirror.dsp_has_effect.load(Ordering::Acquire);

        EngineSnapshot {
            state: drv.state,
            device,
            error: drv.error.or_else(|| self.mirror.chain_error()),
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
            ab: self.mirror.ab_status(),
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
            device_caps: drv.device_caps,
        }
    }

    fn meters(&self) -> Meters {
        let drv = self.driver.snapshot();
        self.meters.read_and_reset(drv.xruns, drv.dsp_load)
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
    }

    fn acquire_device(&self) -> Result<(), EngineError> {
        self.driver
            .acquire_device()
            .map_err(|e| EngineError::new(RpcError::IO, format!("acquire_device failed: {e}")))
    }

    fn shutdown(&self) {
        self.driver.stop();
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

fn chain_has_effect(cfg: &EngineConfig) -> bool {
    if !cfg.dsp_enabled {
        return false;
    }
    let a_active = !cfg.chain.is_passthrough();
    let b_active = cfg
        .ab
        .as_ref()
        .is_some_and(|ab| !ab.chain_b.is_passthrough());
    a_active || b_active
}

fn dsp_has_effect(cfg: &EngineConfig, hardware_volume: bool) -> bool {
    if !cfg.dsp_enabled {
        return false;
    }
    if chain_has_effect(cfg) {
        return true;
    }
    let digital_vol = !hardware_volume && cfg.volume.volume_db != 0.0;
    let digital_bal = cfg.volume.balance_db != 0.0;
    digital_vol || digital_bal
}

fn desired_ratio_for(cfg: &EngineConfig, rate: u32) -> u32 {
    if !cfg.dsp_enabled {
        1
    } else {
        resolve_ratio(cfg.chain.upsampler.target, rate.max(8_000)).clamp(1, MAX_DSP_RATIO as u32)
    }
}

fn diff_requires_reopen(old: &EngineConfig, new: &EngineConfig, current_rate: u32) -> bool {
    if old.device != new.device || old.formats != new.formats || old.quantum != new.quantum {
        return true;
    }
    if desired_ratio_for(old, current_rate) != desired_ratio_for(new, current_rate) {
        return true;
    }
    if old.rate != new.rate && !new.rate.follow_source && new.rate.default_rate != current_rate {
        return true;
    }
    false
}

fn to_driver_config(cfg: &EngineConfig) -> EngineDriverConfig {
    EngineDriverConfig {
        device: cfg.device.clone(),
        formats: cfg.formats.clone(),
        channels: DEFAULT_CHANNELS as u32,
        quantum: cfg.quantum.max(32),
        buffer_periods: DEFAULT_BUFFER_PERIODS,
        dsp_enabled: cfg.dsp_enabled,
        dsp_rate_target: cfg.chain.upsampler.target,
        max_dsp_ratio: MAX_DSP_RATIO as u32,
        rate_policy: cfg.rate.clone(),
        reserve_device: true,
        reserve_priority: DEFAULT_RESERVE_PRIORITY,
        rt_priority: DEFAULT_RT_PRIORITY,
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
    if cfg.quantum < 32 {
        return Err(EngineError::new(
            RpcError::INVALID_PARAMS,
            "quantum must be >= 32",
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

    if let Some(ab) = &cfg.ab {
        validate_chain_params(&ab.chain_b)?;
        if cfg.chain.output != ab.chain_b.output {
            return Err(EngineError::new(
                RpcError::INVALID_PARAMS,
                "A/B output configs must match",
            ));
        }
        let ratio_a = resolve_ratio(cfg.chain.upsampler.target, rate);
        let ratio_b = resolve_ratio(ab.chain_b.upsampler.target, rate);
        if ratio_a != ratio_b {
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

fn validate_chain_params(c: &ChainConfig) -> Result<(), EngineError> {
    if !(1..=32).contains(&c.output.bits) {
        return Err(EngineError::new(
            RpcError::INVALID_PARAMS,
            format!("invalid output.bits: {}", c.output.bits),
        ));
    }
    if !matches!(c.upsampler.target, RateTarget::Off) {
        if c.upsampler.taps < 16 {
            return Err(EngineError::new(
                RpcError::INVALID_PARAMS,
                "upsampler.taps must be >= 16",
            ));
        }
        if !(0.1..=1.0).contains(&c.upsampler.cutoff) || !c.upsampler.cutoff.is_finite() {
            return Err(EngineError::new(
                RpcError::INVALID_PARAMS,
                "upsampler.cutoff must be in 0.1..=1.0",
            ));
        }
        if !c.upsampler.kaiser_beta.is_finite() || c.upsampler.kaiser_beta < 0.0 {
            return Err(EngineError::new(
                RpcError::INVALID_PARAMS,
                "upsampler.kaiser_beta must be finite and >= 0",
            ));
        }
    }
    Ok(())
}

fn compute_ab_offset(cfg: &EngineConfig, rate: u32) -> f64 {
    let Some(ab) = &cfg.ab else {
        return 0.0;
    };
    match ab.level {
        LevelMatch::Off => 0.0,
        LevelMatch::Manual { offset_db } => offset_db,
        LevelMatch::Analytic => AbChain::analytic_offset_db(&cfg.chain, &ab.chain_b, rate),
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

fn with_actual_ratio(mut cfg: ChainConfig, actual_ratio: u32) -> ChainConfig {
    cfg.upsampler.target = if actual_ratio <= 1 {
        RateTarget::Off
    } else {
        RateTarget::Ratio(actual_ratio)
    };
    cfg
}

fn build_active_chain(
    cfg: &EngineConfig,
    ir_cache: &mut IrCache,
    graph_rate: u32,
    actual_ratio: u32,
    current_ab_side: AbSide,
) -> Result<(ActiveChain, f64, bool), EngineError> {
    let has_effect = chain_has_effect(cfg);
    if !cfg.dsp_enabled {
        return Ok((
            ActiveChain::Bypass,
            compute_ab_offset(cfg, graph_rate),
            false,
        ));
    }

    let max_block = scaled_quantum(cfg.quantum, graph_rate).clamp(512, 8192) as usize;
    let a_cfg = with_actual_ratio(cfg.chain.clone(), actual_ratio);

    let irs_a = load_and_combine_irs(&a_cfg.convolution, ir_cache, graph_rate)?;

    if let Some(ab) = &cfg.ab {
        let b_cfg = with_actual_ratio(ab.chain_b.clone(), actual_ratio);
        let irs_b = load_and_combine_irs(&b_cfg.convolution, ir_cache, graph_rate)?;
        let offset_db = compute_ab_offset(cfg, graph_rate);
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
        let mut ab_chain = AbChain::new(spec_a, spec_b, offset_db).map_err(|e| {
            EngineError::new(RpcError::INVALID_PARAMS, format!("invalid A/B chain: {e}"))
        })?;
        if current_ab_side == AbSide::B {
            ab_chain.select(Side::B, 0);
        }
        return Ok((ActiveChain::Ab(Box::new(ab_chain)), offset_db, has_effect));
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
    Ok((ActiveChain::Single(Box::new(chain)), 0.0, has_effect))
}

/// Build a reduced chain that is still valid for `actual_ratio` after
/// [`build_active_chain`] failed for the freshly negotiated format.
///
/// Policy: keep as much of the requested processing as possible, because the
/// alternative is either silent playback on a stale chain or staying stuck in
/// `Switching`/`Error`.
///
/// 1. Disable convolution on both A/B sides and rebuild. A missing/invalid IR
///    at the new rate is the common failure (the per-rate cache key changes
///    when the file or its mtime disappears), and EQ/upsampling stay intact.
/// 2. If that still fails, build an upsampler-only chain (no EQ, no
///    convolution, no A/B) at the same ratio so the resume gate's ratio check
///    matches.
fn build_fallback_chain(
    cfg: &EngineConfig,
    ir_cache: &mut IrCache,
    graph_rate: u32,
    actual_ratio: u32,
    current_ab_side: AbSide,
) -> Result<(ActiveChain, f64, bool), EngineError> {
    let mut stripped = cfg.clone();
    stripped.chain.convolution.clear();
    if let Some(ab) = &mut stripped.ab {
        ab.chain_b.convolution.clear();
    }
    if let Ok(built) = build_active_chain(
        &stripped,
        ir_cache,
        graph_rate,
        actual_ratio,
        current_ab_side,
    ) {
        return Ok(built);
    }

    // Last resort: upsampling only, at exactly the negotiated ratio.
    let reduced = with_actual_ratio(
        ChainConfig {
            upsampler: cfg.chain.upsampler.clone(),
            output: cfg.chain.output,
            ..Default::default()
        },
        actual_ratio,
    );
    if reduced.is_passthrough() {
        return Ok((ActiveChain::Bypass, 0.0, false));
    }
    let max_block = scaled_quantum(cfg.quantum, graph_rate).clamp(512, 8192) as usize;
    let spec = ChainSpec {
        config: reduced,
        sample_rate: graph_rate,
        channels: DEFAULT_CHANNELS,
        max_block,
        irs: None,
    };
    let chain = Chain::try_new(spec).map_err(|e| {
        EngineError::new(
            RpcError::INVALID_PARAMS,
            format!("invalid fallback chain: {e}"),
        )
    })?;
    Ok((ActiveChain::Single(Box::new(chain)), 0.0, true))
}

#[cfg(test)]
mod tests {
    use super::*;
    use audivane_protocol::ipc::AbConfig;
    use audivane_protocol::{
        Band, ConvolutionConfig, EqProfile, FilterType, OutputConfig, UpsamplerConfig,
    };

    fn test_cfg() -> EngineConfig {
        EngineConfig {
            device: "hw:Loopback,0,0".into(),
            ..Default::default()
        }
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
        assert!(!diff_requires_reopen(&base, &eq_change, 48_000));

        // Adding A/B with same ratio does NOT reopen ALSA.
        let mut ab_change = base.clone();
        ab_change.ab = Some(AbConfig {
            chain_b: eq_change.chain.clone(),
            level: LevelMatch::Analytic,
        });
        assert!(!diff_requires_reopen(&base, &ab_change, 48_000));

        // Enabling 4x upsampling DOES reopen ALSA.
        let mut up_change = base.clone();
        up_change.chain.upsampler = UpsamplerConfig {
            target: RateTarget::Ratio(4),
            ..Default::default()
        };
        assert!(diff_requires_reopen(&base, &up_change, 48_000));

        // Toggling dsp_enabled off when upsampling was active DOES reopen ALSA back to 1x.
        let mut dsp_off = up_change.clone();
        dsp_off.dsp_enabled = false;
        assert!(diff_requires_reopen(&up_change, &dsp_off, 48_000));

        // Changing quantum or device DOES reopen ALSA.
        let mut q_change = base.clone();
        q_change.quantum = 512;
        assert!(diff_requires_reopen(&base, &q_change, 48_000));

        let mut dev_change = base.clone();
        dev_change.device = "hw:CARD=DAC,DEV=0".into();
        assert!(diff_requires_reopen(&base, &dev_change, 48_000));
    }

    #[test]
    fn validates_ab_output_and_ratio_parity() {
        let mut cache = IrCache::new();
        let mut bad_out = test_cfg();
        bad_out.ab = Some(AbConfig {
            chain_b: ChainConfig {
                output: OutputConfig {
                    bits: 16,
                    ..Default::default()
                },
                ..Default::default()
            },
            level: LevelMatch::Off,
        });
        assert!(validate_engine_config(&bad_out, &mut cache, 48_000).is_err());

        let mut bad_ratio = test_cfg();
        bad_ratio.ab = Some(AbConfig {
            chain_b: ChainConfig {
                upsampler: UpsamplerConfig {
                    target: RateTarget::Ratio(4),
                    ..Default::default()
                },
                ..Default::default()
            },
            level: LevelMatch::Off,
        });
        assert!(validate_engine_config(&bad_ratio, &mut cache, 48_000).is_err());
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
            chain_b: ChainConfig {
                eq: Some(EqProfile {
                    name: "hot".into(),
                    preamp_db: 3.0,
                    ..Default::default()
                }),
                ..Default::default()
            },
            level: LevelMatch::Analytic,
        });

        let (chain, offset_db, has_effect) =
            build_active_chain(&cfg, &mut cache, 48_000, 1, AbSide::B).unwrap();
        assert!(has_effect);
        assert!((offset_db + 3.0).abs() < 1e-6, "got {offset_db}");
        match chain {
            ActiveChain::Ab(ab) => assert_eq!(ab.current(), Side::B),
            _ => panic!("expected ActiveChain::Ab"),
        }
    }

    #[test]
    fn max_rate_target_resolves_against_actual_source_rates() {
        let mut cfg = test_cfg();
        cfg.chain.upsampler = UpsamplerConfig {
            target: RateTarget::MaxRate(192_000),
            ..Default::default()
        };
        assert_eq!(desired_ratio_for(&cfg, 44_100), 4); // 176.4 kHz
        assert_eq!(desired_ratio_for(&cfg, 48_000), 4); // 192 kHz
        assert_eq!(desired_ratio_for(&cfg, 96_000), 2); // 192 kHz (NOT 4x = 384 kHz)
        assert_eq!(desired_ratio_for(&cfg, 192_000), 1); // 192 kHz
    }

    struct BuilderProcHarness {
        state: BuilderState,
        mirror: SharedMirror,
        ab_atomic: Arc<AtomicU8>,
        proc: DaemonProcessor,
        recycle_rx: rtrb::Consumer<ActiveChain>,
        gen_handle: Arc<AtomicU64>,
    }

    impl BuilderProcHarness {
        fn new(init_cfg: EngineConfig) -> Self {
            Self::with_mixer(
                init_cfg,
                Arc::new(|_dev, vol_db, _muted| MixerApplyOutcome::digital(vol_db)),
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
            );
            let shared_volume = proc.volume_handle();
            let gen_handle = proc.active_generation_handle();
            let state = BuilderState {
                cfg: init_cfg,
                ir_cache: IrCache::new(),
                ab_side: AbSide::A,
                ab_offset_db: 0.0,
                last_rate: 48_000,
                last_ratio: 1,
                last_dsp_ratio: 1,
                out_format: None,
                config_generation: 0,
                built_generation: 0,
                next_swap_seq: 0,
                pending_publication: None,
                has_pending_swap: Arc::new(AtomicBool::new(false)),
                shared_volume,
                mixer,
                hardware_volume: false,
                cmd_tx,
                swap_tx,
            };
            Self {
                state,
                mirror,
                ab_atomic,
                proc,
                recycle_rx,
                gen_handle,
            }
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
                    ratio,
                    true,
                );
                audivane_io_pw::EngineAudioProcessor::process_block(
                    &mut self.proc,
                    SinkChoice::Dsp,
                    &inp,
                    &inp,
                    &mut out,
                    2,
                    ratio,
                    48_000,
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
                1,
                true,
            );
            audivane_io_pw::EngineAudioProcessor::process_block(
                &mut h.proc,
                SinkChoice::Dsp,
                &inp,
                &inp,
                &mut out,
                2,
                1,
                48_000,
            );
            all.extend_from_slice(&out);
        }
        while h.recycle_rx.pop().is_ok() {}
        all
    }

    #[test]
    fn s16_device_dithers_at_16_bits_even_when_configured_for_24() {
        // "Dither to 24" (the default) with an EQ so the chain is active.
        let mut cfg = test_cfg();
        cfg.chain.eq = Some(eq_profile(-1.0, -1.0));
        cfg.chain.output.bits = 24;
        cfg.chain.output.dither = audivane_protocol::DitherKind::Tpdf;

        // Device negotiated S16 (e.g. Creative Pebble V3): every sample must
        // already be a 16-bit value with dither noise in it, so the S16
        // packer drops nothing.
        let mut h = BuilderProcHarness::new(cfg.clone());
        h.state.apply_negotiated(
            48_000,
            1,
            SinkChoice::Dsp,
            SampleFormat::S16,
            &h.mirror,
            &h.ab_atomic,
            None,
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
        h.state.apply_negotiated(
            48_000,
            1,
            SinkChoice::Dsp,
            SampleFormat::S32,
            &h.mirror,
            &h.ab_atomic,
            None,
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
        up_cfg.chain.upsampler = UpsamplerConfig {
            target: RateTarget::Ratio(4),
            taps: 8192,
            ..Default::default()
        };
        let reopen2 = h
            .state
            .apply_config(&up_cfg, &h.mirror, EngineState::Ready, 48_000, None, None)
            .unwrap();
        assert!(reopen2);

        // Simulate both RT polling between apply 1 and 2, AND skipping straight to on_negotiated
        h.state.apply_negotiated(
            48_000,
            4,
            SinkChoice::Dsp,
            SampleFormat::S32,
            &h.mirror,
            &h.ab_atomic,
            None,
        );
        audivane_io_pw::EngineAudioProcessor::poll_updates(
            &mut h.proc,
            SinkChoice::Dsp,
            48_000,
            4,
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
        up_cfg.chain.upsampler = UpsamplerConfig {
            target: RateTarget::Ratio(4),
            taps: 8192,
            ..Default::default()
        };
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
            4,
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
            4,
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
            4,
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
            4,
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
            4,
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
        let mut c1 = test_cfg();
        c1.chain.upsampler = UpsamplerConfig {
            target: RateTarget::Ratio(4),
            taps: 8192,
            ..Default::default()
        };
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
        // `apply_negotiated` at the same 48 kHz rate on Direct MUST rebuild gen 2 instead of skipping!
        h.state.apply_negotiated(
            48_000,
            1,
            SinkChoice::Direct,
            SampleFormat::S32,
            &h.mirror,
            &h.ab_atomic,
            None,
        );
        audivane_io_pw::EngineAudioProcessor::poll_updates(
            &mut h.proc,
            SinkChoice::Direct,
            48_000,
            1,
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
            1,
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
    fn builder_deferred_publication_retries_across_full_queue_and_mutes_mismatch() {
        let mut h = BuilderProcHarness::new(test_cfg());
        // Pause RT swap drain (test hook) and saturate the 32-slot swap queue with 1x swaps.
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
        cfg_4x.chain.upsampler = UpsamplerConfig {
            target: RateTarget::Ratio(4),
            taps: 8192,
            ..Default::default()
        };
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
        let neg1 = h.state.apply_negotiated(
            48_000,
            4,
            SinkChoice::Dsp,
            SampleFormat::S32,
            &h.mirror,
            &h.ab_atomic,
            None,
        );
        let seq1 = neg1.required_chain_seq;
        assert!(seq1.is_some());
        assert!(neg1.error.is_none());
        // Must NOT mark built_generation = gen_first_4x when push_swap failed!
        assert_eq!(h.state.built_generation, 32);
        assert!(h.state.pending_publication.is_some());
        assert!(h.state.has_pending_swap.load(Ordering::Acquire));
        assert!(!h.state.flush_pending_swap(&h.mirror, None));

        // While RT drain is paused and the 4x chain isn't installed, is_chain_ready
        // is false (Resume cannot report Ready/Playing) and any block processed at
        // 4x outputs silence rather than mismatched passthrough!
        audivane_io_pw::EngineAudioProcessor::poll_updates(
            &mut h.proc,
            SinkChoice::Dsp,
            48_000,
            4,
            false,
        );
        assert!(!audivane_io_pw::EngineAudioProcessor::is_chain_ready(
            &h.proc,
            SinkChoice::Dsp,
            4,
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
            4,
            48_000,
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
        let neg_latest = h.state.apply_negotiated(
            48_000,
            4,
            SinkChoice::Dsp,
            SampleFormat::S32,
            &h.mirror,
            &h.ab_atomic,
            None,
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
                4,
                false,
            );
            while h.recycle_rx.pop().is_ok() {}
            h.state.flush_pending_swap(&h.mirror, None);
        }
        audivane_io_pw::EngineAudioProcessor::poll_updates(
            &mut h.proc,
            SinkChoice::Dsp,
            48_000,
            4,
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
            4,
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

    #[test]
    fn failed_ratio_changing_rebuild_plays_fallback_chain_and_reports_error() {
        let ir_path = temp_ir_path("ratio_fallback");
        write_test_ir(&ir_path, 48_000);

        let mut h = BuilderProcHarness::new(test_cfg());
        let mut cfg = test_cfg();
        cfg.chain.eq = Some(eq_profile(-3.0, 0.0));
        cfg.chain.convolution = vec![conv_cfg(&ir_path)];
        cfg.chain.upsampler = UpsamplerConfig {
            target: RateTarget::Ratio(4),
            taps: 1024,
            ..Default::default()
        };
        // Apply at 44.1 kHz: the IR is validated and cached for 44.1 kHz.
        assert!(
            h.state
                .apply_config(&cfg, &h.mirror, EngineState::Ready, 44_100, None, None)
                .unwrap()
        );

        // The IR becomes unavailable before the ratio-changing reopen to 48 kHz x4.
        std::fs::remove_file(&ir_path).unwrap();

        let neg = h.state.apply_negotiated(
            48_000,
            4,
            SinkChoice::Dsp,
            SampleFormat::S32,
            &h.mirror,
            &h.ab_atomic,
            None,
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
            4,
            Some(seq)
        ));
        audivane_io_pw::EngineAudioProcessor::poll_updates(
            &mut h.proc,
            SinkChoice::Dsp,
            48_000,
            4,
            false,
        );
        assert!(audivane_io_pw::EngineAudioProcessor::is_chain_ready(
            &h.proc,
            SinkChoice::Dsp,
            4,
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
        cfg.chain.upsampler = UpsamplerConfig {
            target: RateTarget::Ratio(4),
            taps: 1024,
            ..Default::default()
        };
        assert!(
            h.state
                .apply_config(&cfg, &h.mirror, EngineState::Ready, 48_000, None, None)
                .unwrap()
        );
        let first = h.state.apply_negotiated(
            48_000,
            4,
            SinkChoice::Dsp,
            SampleFormat::S32,
            &h.mirror,
            &h.ab_atomic,
            None,
        );
        assert!(first.error.is_none());
        audivane_io_pw::EngineAudioProcessor::poll_updates(
            &mut h.proc,
            SinkChoice::Dsp,
            48_000,
            4,
            false,
        );
        assert_eq!(h.active_gen(), h.state.config_generation);
        while h.recycle_rx.pop().is_ok() {}

        // A same-rate device re-acquire reopens at 48 kHz x4 after the IR vanished.
        std::fs::remove_file(&ir_path).unwrap();
        let neg = h.state.apply_negotiated(
            48_000,
            4,
            SinkChoice::Dsp,
            SampleFormat::S32,
            &h.mirror,
            &h.ab_atomic,
            None,
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
            4,
            false,
        );
        assert!(audivane_io_pw::EngineAudioProcessor::is_chain_ready(
            &h.proc,
            SinkChoice::Dsp,
            4,
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
            chain_b: ChainConfig {
                eq: Some(eq_profile(-9.0, 0.0)),
                ..Default::default()
            },
            level: LevelMatch::Off,
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
        h.mirror.set_ab(true, AbSide::B, 0.0);
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
                1,
                false,
            );
            while h.recycle_rx.pop().is_ok() {}
            h.state.flush_pending_swap(&h.mirror, None);
        }
        audivane_io_pw::EngineAudioProcessor::poll_updates(
            &mut h.proc,
            SinkChoice::Dsp,
            48_000,
            1,
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
            1,
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

    #[test]
    fn volume_only_fast_path_skips_chain_swap_and_sets_dsp_has_effect() {
        let mut h = BuilderProcHarness::new(test_cfg());
        h.state.apply_negotiated(
            48_000,
            1,
            SinkChoice::Dsp,
            SampleFormat::S32,
            &h.mirror,
            &h.ab_atomic,
            None,
        );
        audivane_io_pw::EngineAudioProcessor::poll_updates(
            &mut h.proc,
            SinkChoice::Dsp,
            48_000,
            1,
            false,
        );
        let initial_seq = h.state.next_swap_seq;
        assert!(!h.mirror.dsp_has_effect.load(Ordering::Acquire));

        // Digital -10 dB volume-only update must NOT build/queue a new Chain swap,
        // must mark dsp_has_effect = true, and must keep built_generation current.
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
            1,
            false,
        );
        // -6 dBFS test sine attenuated by -10 dB digital volume on L -> -16 dBFS.
        let db = h.measure_1khz_peak_db(1);
        assert!(
            (db - (-16.0)).abs() < 0.25,
            "expected -16.0 dBFS with digital volume, got {db:.3}"
        );

        // Direct path remains bit-exact despite digital volume and balance.
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
            1,
            48_000,
        );
        assert_eq!(hash_i32(&direct_out), hash_i32(&expected_i32));
        assert_eq!(direct_out, expected_i32);
    }

    #[test]
    fn hardware_mixer_reapplies_on_reopen_acquire_and_device_switch() {
        let calls: Arc<Mutex<Vec<(String, f64, bool)>>> = Arc::new(Mutex::new(Vec::new()));
        let calls_cb = Arc::clone(&calls);
        let mixer: MixerFn = Arc::new(move |dev, vol_db, muted| {
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
        h.state.apply_negotiated(
            48_000,
            1,
            SinkChoice::Dsp,
            SampleFormat::S32,
            &h.mirror,
            &h.ab_atomic,
            None,
        );
        let status = h.mirror.volume_status();
        assert!(status.hardware);
        assert_eq!(status.min_db, -64.0);
        assert_eq!(status.max_db, 0.0);
        assert_eq!(status.volume_db, -14.0);
        assert!(status.muted);
        // Hardware volume + 0 balance keeps dsp_has_effect = false (bit-perfect passthrough).
        assert!(!h.mirror.dsp_has_effect.load(Ordering::Acquire));

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
        h.state.apply_negotiated(
            96_000,
            1,
            SinkChoice::Direct,
            SampleFormat::S32,
            &h.mirror,
            &h.ab_atomic,
            None,
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
        h.state.apply_negotiated(
            96_000,
            1,
            SinkChoice::Dsp,
            SampleFormat::S32,
            &h.mirror,
            &h.ab_atomic,
            None,
        );
        let fallback_status = h.mirror.volume_status();
        assert!(!fallback_status.hardware);
        assert_eq!(fallback_status.min_db, VOLUME_MIN_DB);
        assert_eq!(fallback_status.max_db, VOLUME_MAX_DB);

        let recorded = calls.lock().unwrap();
        assert!(
            recorded
                .iter()
                .any(|(d, v, m)| d == "hw:CARD=DAC,DEV=0" && *v == -14.0 && *m)
        );
        assert!(recorded.iter().any(|(d, _, _)| d == "hw:Loopback,0,0"));
        assert!(recorded.len() >= 4);
    }
}
