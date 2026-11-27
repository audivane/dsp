//! The channel workers of the main output's chains
//! ([`audivane_dsp_core::workers`]): one process-wide pool, spawned on the
//! first chain that wants it, its threads made real-time at the audio
//! thread's priority through rtkit, as the driver makes the audio thread.
//!
//! Overload: a worker is under the same guard as the audio loop. It
//! registers as a helper of the audio thread, so the `SIGXCPU` handler
//! demotes it by id whichever thread takes the signal, and it runs a
//! [`RunGuard`] between jobs (a 100 µs nap after 50 ms without a sleep).
//! Its priority follows the audio thread's ([`tick`], from the overload
//! thread): demoted while the audio thread is demoted, promoted again once
//! the audio thread is.
//!
//! Only the main output's DSP chains (single and A/B) use it, built on the
//! control thread and run on the one audio thread. Direct, DSP off and the
//! engine's resample-only chain stay single-threaded, as do followers (each
//! on its own thread, chain A only).

use super::processor::ActiveChain;
use audivane_dsp_core::workers::{LanePool, WorkerHook};
use audivane_io_alsa::RunGuard;
use audivane_protocol::ipc::EngineConfig;
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// Threads a block of the engine's chains runs on (the audio thread
/// included) when workers are on: one per channel, as far as the machine
/// has cores.
pub(crate) fn planned_lanes() -> usize {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    super::DEFAULT_CHANNELS.min(cores).max(1)
}

/// Threads a block runs on as things stand: [`planned_lanes`] until the
/// pool is tried, then the pool's workers and the audio thread (one when
/// it could not start).
pub(crate) fn available_lanes() -> usize {
    match POOL.get() {
        None => planned_lanes(),
        Some(None) => 1,
        Some(Some(pool)) => pool.workers() + 1,
    }
}

static POOL: OnceLock<Option<Arc<LanePool>>> = OnceLock::new();

/// Most workers whose priority [`tick`] follows (the engine runs one).
const TRACKED: usize = 4;
/// Kernel thread ids of the workers (0: not started, or untracked).
static WORKER_TIDS: [AtomicI32; TRACKED] = [const { AtomicI32::new(0) }; TRACKED];

/// The pool (`None` on a single core, or when the threads cannot start).
fn pool() -> Option<&'static Arc<LanePool>> {
    POOL.get_or_init(|| {
        let workers = planned_lanes() - 1;
        if workers == 0 {
            log::info!("channel workers: one core, the chain runs on the audio thread alone");
            return None;
        }
        match LanePool::spawn(workers, start) {
            Ok(pool) => {
                log::info!("channel workers: {workers} worker thread(s) started");
                Some(pool)
            }
            Err(e) => {
                log::warn!("channel workers: cannot start them ({e}); single-threaded");
                None
            }
        }
    })
    .as_ref()
}

/// A worker's overload guard ([`WorkerHook`]): a nap between jobs after
/// 50 ms without a sleep; registered with the `SIGXCPU` handler.
struct Guard {
    run: RunGuard,
    tid: Option<i32>,
}

impl WorkerHook for Guard {
    fn between_jobs(&mut self) {
        self.run.check();
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(tid) = self.tid {
            audivane_io_alsa::unregister_helper_thread(tid);
        }
    }
}

/// A worker's start: real-time at the audio thread's priority, best effort
/// (as the audio thread: without rtkit it runs at normal priority), and
/// its overload guard.
fn start(index: usize) -> Guard {
    let tid = audivane_io_alsa::register_helper_thread();
    if tid.is_none() {
        log::warn!(
            "channel worker {}: no room in the overload guard's registry",
            index + 1
        );
    }
    if let Some(slot) = WORKER_TIDS.get(index) {
        slot.store(audivane_io_alsa::current_tid(), Ordering::Release);
    }
    promote(index);
    Guard {
        run: RunGuard::default(),
        tid,
    }
}

fn promote(index: usize) {
    if cfg!(test) {
        return;
    }
    match audivane_io_alsa::promote_current_thread_rt(super::DEFAULT_RT_PRIORITY) {
        Ok(prio) => log::info!(
            "channel worker {} at {} priority {prio}",
            index + 1,
            audivane_io_alsa::thread_policy_name(0)
        ),
        Err(e) => log::warn!(
            "channel worker {}: no real-time priority via rtkit (continuing without): {e}",
            index + 1
        ),
    }
}

/// Run `chain`'s per-channel stages on the workers when the config wants
/// them. Off the audio thread, right after the chain is built. Returns
/// whether it uses them.
pub(crate) fn attach(chain: &mut ActiveChain, cfg: &EngineConfig) -> bool {
    if !cfg.channel_workers {
        return false;
    }
    let attached = match chain {
        ActiveChain::Single(c) if c.channels() >= 2 && !c.is_bit_transparent() => {
            pool().is_some_and(|p| c.attach_workers(p))
        }
        ActiveChain::Ab(ab) if ab.channels() >= 2 => pool().is_some_and(|p| ab.attach_workers(p)),
        _ => false,
    };
    if attached {
        log::debug!("chain built with channel workers");
    }
    attached
}

/// How long [`tick`] waits before it asks rtkit again for a worker it
/// could not promote.
const PROMOTE_RETRY: Duration = Duration::from_secs(30);

/// Every overload tick (100 ms, the overload thread): the workers' priority
/// follows the audio thread's, and the pool's counters are logged
/// ([`log_changes`]). A few system calls; an rtkit call only when the two
/// differ.
pub(crate) fn tick() {
    log_changes();
    if cfg!(test) || POOL.get().and_then(Option::as_ref).is_none() {
        return;
    }
    let audio = audivane_io_alsa::audio_thread_tid();
    if audio <= 0 {
        // No stream: the workers are idle, asleep; leave them as they are.
        return;
    }
    let audio_rt = audivane_io_alsa::thread_is_realtime(audio);
    static RETRY_AT: [AtomicU64; TRACKED] = [const { AtomicU64::new(0) }; TRACKED];
    for (i, slot) in WORKER_TIDS.iter().enumerate() {
        let tid = slot.load(Ordering::Acquire);
        if tid <= 0 {
            continue;
        }
        let rt = audivane_io_alsa::thread_is_realtime(tid);
        if !audio_rt && rt {
            match audivane_io_alsa::demote_thread(tid) {
                audivane_io_alsa::Demotion::Failed => {
                    log::warn!(
                        "channel worker {}: cannot demote it with the audio thread",
                        i + 1
                    )
                }
                _ => log::info!("channel worker {}: demoted with the audio thread", i + 1),
            }
        } else if audio_rt && !rt {
            let now = elapsed_ms();
            if now < RETRY_AT[i].load(Ordering::Relaxed) {
                continue;
            }
            match audivane_io_alsa::promote_thread_rt(tid, super::DEFAULT_RT_PRIORITY) {
                Ok(prio) => log::info!(
                    "channel worker {}: real-time again at priority {prio}, with the audio thread",
                    i + 1
                ),
                Err(e) => {
                    RETRY_AT[i].store(now + PROMOTE_RETRY.as_millis() as u64, Ordering::Relaxed);
                    log::warn!(
                        "channel worker {}: cannot make it real-time again (retrying in {} s): {e}",
                        i + 1,
                        PROMOTE_RETRY.as_secs()
                    );
                }
            }
        }
    }
}

/// Milliseconds since the first call (a monotonic clock for [`tick`]).
fn elapsed_ms() -> u64 {
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as u64
}

/// Log what the pool's counters gained since the last call: blocks a late
/// worker made silent, lanes the audio thread ran because their worker was
/// busy, and worker panics. Three relaxed loads when nothing moved.
fn log_changes() {
    use std::sync::atomic::Ordering::Relaxed;
    static SEEN: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
    let Some(pool) = POOL.get().and_then(Option::as_ref) else {
        return;
    };
    let s = pool.stats();
    let now = [
        s.late_blocks.load(Relaxed),
        s.inline_runs.load(Relaxed),
        s.panics.load(Relaxed),
    ];
    let new: [u64; 3] = std::array::from_fn(|i| now[i].wrapping_sub(SEEN[i].swap(now[i], Relaxed)));
    if new[0] > 0 {
        log::warn!(
            "channel workers: a worker missed its deadline; {} block(s) played as silence, the \
             chain reset when it was back",
            new[0]
        );
    }
    if new[1] > 0 {
        log::info!(
            "channel workers: {} lane(s) ran on the audio thread (worker still busy)",
            new[1]
        );
    }
    if new[2] > 0 {
        log::error!(
            "channel workers: {} lane run(s) panicked; that chain stays silent until it is \
             rebuilt",
            new[2]
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A worker's start registers it with the `SIGXCPU` handler (so it is
    /// demoted by id with the audio thread) and records its id for
    /// [`tick`]; its guard's drop unregisters it.
    #[test]
    fn a_worker_start_registers_it_with_the_overload_guard() {
        let index = TRACKED - 1;
        let (tid, guard_tid, recorded) = std::thread::spawn(move || {
            let guard = start(index);
            let recorded = WORKER_TIDS[index].load(Ordering::Acquire);
            let out = (audivane_io_alsa::current_tid(), guard.tid, recorded);
            drop(guard);
            out
        })
        .join()
        .unwrap();
        assert_eq!(guard_tid, Some(tid));
        assert_eq!(recorded, tid);
        WORKER_TIDS[index].store(0, Ordering::Release);
        // Unregistered: every entry is free again for this test's threads.
        let ids: Vec<_> = (0..audivane_io_alsa::MAX_HELPER_THREADS)
            .filter_map(|_| {
                std::thread::spawn(audivane_io_alsa::register_helper_thread)
                    .join()
                    .unwrap()
            })
            .collect();
        for &id in &ids {
            audivane_io_alsa::unregister_helper_thread(id);
        }
        assert!(ids.len() >= audivane_io_alsa::MAX_HELPER_THREADS - planned_lanes());
    }
}
