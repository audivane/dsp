//! The audio thread's overload guard, as the engine driver runs it (the
//! mechanism and its numbers are in [`audivane_io_alsa::rt`]).
//!
//! - The RT thread calls [`RtLoopGuard::on_iteration`] on every loop
//!   iteration and after its wait: the run guard naps when the thread has
//!   not slept for 50 ms (only while it is real-time), and a `SIGXCPU`
//!   the handler took (on whichever thread; it demoted the registered audio
//!   thread by id) is recorded here: the thread makes sure it is demoted
//!   and puts the soft limit back. Three counts stay apart: signals, real
//!   real-time to normal transitions of the audio thread (the handler's
//!   and its own), and demotions the kernel refused. Only a transition
//!   marks the thread demoted (and is an overload for the control plane).
//! - The control thread calls [`RepromoteGate::tick`] every wake: it puts
//!   the soft limit back when something else moved it, logs one line per
//!   episode, and promotes the audio thread again through rtkit once the
//!   load has stayed under real time for [`REPROMOTE_AFTER`] (the control
//!   plane's fallback has taken effect by then).

use audivane_io_alsa::{
    Demotion, RunGuard, audio_thread_demotion_failures, audio_thread_demotions, demote_thread,
    overload_signals, promote_thread_rt, register_audio_thread, restore_rttime_limits,
    unregister_audio_thread,
};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// How long the load must stay under [`REPROMOTE_LOAD`] with no new xrun
/// before the demoted audio thread is made real-time again.
pub const REPROMOTE_AFTER: Duration = Duration::from_secs(2);
/// DSP load (callback time over period time) under which the load counts
/// as back under real time.
pub const REPROMOTE_LOAD: f64 = 0.8;
/// Wait after a refused promotion before asking rtkit again.
const REPROMOTE_RETRY: Duration = Duration::from_secs(30);

/// Overload state shared by the RT and the control thread (atomics only).
#[derive(Debug, Default)]
pub(crate) struct RtOverloadState {
    /// Kernel id of the audio thread (0 before it started).
    tid: AtomicI32,
    /// Whether the audio thread was made real-time at start.
    promoted: AtomicBool,
    /// Demoted after an overload and not promoted again yet.
    demoted: AtomicBool,
    /// `SIGXCPU` signals (a soft limit reached) the audio thread saw.
    signals: AtomicU64,
    /// Real-time to normal transitions of the audio thread after a signal
    /// (the overloads the control plane acts on).
    demotions: AtomicU64,
    /// The chain generation on the audio thread at the last of them,
    /// stored before `demotions` moves, so a reader of the count sees the
    /// generation of that event (or a later one's).
    demotion_generation: AtomicU64,
    /// The varispeed hold's sequence the audio thread applied at the last
    /// of them (stored with the generation): a demotion before the thread
    /// applied a hold is not that hold's.
    demotion_hold_seq: AtomicU64,
    /// Demotions of the real-time audio thread the kernel refused (logged
    /// by the control thread).
    failed_demotions: AtomicU64,
    /// Naps the run guard took (the thread had not slept for 50 ms).
    naps: AtomicU64,
    /// Steady-state periods the audio thread processed and wrote, across
    /// reopens (the control plane's overload detector divides underruns
    /// by them).
    periods: AtomicU64,
}

impl RtOverloadState {
    pub(crate) fn signals(&self) -> u64 {
        self.signals.load(Ordering::Relaxed)
    }

    pub(crate) fn demotions(&self) -> u64 {
        self.demotions.load(Ordering::Acquire)
    }

    /// Read after [`Self::demotions`].
    pub(crate) fn demotion_generation(&self) -> u64 {
        self.demotion_generation.load(Ordering::Acquire)
    }

    /// Read after [`Self::demotions`].
    pub(crate) fn demotion_hold_seq(&self) -> u64 {
        self.demotion_hold_seq.load(Ordering::Acquire)
    }

    pub(crate) fn failed_demotions(&self) -> u64 {
        self.failed_demotions.load(Ordering::Relaxed)
    }

    pub(crate) fn naps(&self) -> u64 {
        self.naps.load(Ordering::Relaxed)
    }

    pub(crate) fn periods(&self) -> u64 {
        self.periods.load(Ordering::Relaxed)
    }

    /// Whether the audio thread is demoted now (after an overload, until
    /// it is promoted again).
    pub(crate) fn demoted(&self) -> bool {
        self.demoted.load(Ordering::Acquire)
    }
}

/// The RT thread's half. Built on the audio thread, right after its
/// promotion; it registers the thread as the one the `SIGXCPU` handler
/// demotes, until it is dropped.
pub(crate) struct RtLoopGuard {
    run: RunGuard,
    seen_signals: u64,
    seen_demotions: u64,
    seen_failures: u64,
    tid: i32,
}

impl Drop for RtLoopGuard {
    fn drop(&mut self) {
        unregister_audio_thread(self.tid);
    }
}

impl RtLoopGuard {
    /// Start of the audio thread, after its allocations: promote it to
    /// real-time through rtkit (best effort) and build its guard. All the
    /// audio loop's overload hooks are this, [`Self::on_iteration`] and
    /// [`Self::period_done`].
    pub(crate) fn start(state: &RtOverloadState, priority: u32) -> Self {
        let promoted = audivane_io_alsa::try_promote_current_thread_rt(priority).is_some();
        Self::new(state, promoted)
    }

    pub(crate) fn new(state: &RtOverloadState, promoted: bool) -> Self {
        // The baselines before the thread is published to the handler: a
        // signal after this point is counted by the first iteration (the
        // handler's demotion by id, or this thread's own check), never
        // folded into a baseline.
        let seen_failures = audio_thread_demotion_failures();
        let seen_demotions = audio_thread_demotions();
        let seen_signals = overload_signals();
        let tid = register_audio_thread();
        state.tid.store(tid, Ordering::Release);
        state.promoted.store(promoted, Ordering::Release);
        Self {
            run: RunGuard::default(),
            seen_signals,
            seen_demotions,
            seen_failures,
            tid,
        }
    }

    /// Once per loop iteration, after the loop's wait and after every
    /// nested wait (priming, the drain, the varispeed schedule), with the
    /// chain generation running on the thread
    /// ([`crate::EngineAudioProcessor::running_generation`]) and the
    /// varispeed hold's sequence it applies
    /// ([`crate::varispeed_rt::VarispeedDriver::hold_seq`]), which a
    /// demotion is published with. No allocation, no lock; a nap is one
    /// 100 µs sleep.
    #[inline]
    pub(crate) fn on_iteration(&mut self, state: &RtOverloadState, generation: u64, hold_seq: u64) {
        if self.run.check() {
            state.naps.fetch_add(1, Ordering::Relaxed);
        }
        let signals = overload_signals();
        if signals != self.seen_signals {
            state
                .signals
                .fetch_add(signals.wrapping_sub(self.seen_signals), Ordering::Relaxed);
            self.seen_signals = signals;
            // What the handler did to this thread (by id, whichever thread
            // it ran on), then this thread's own check, which covers a
            // refused call there. A signal that finds it at normal priority
            // already is no demotion.
            let handler = audio_thread_demotions();
            let refused = audio_thread_demotion_failures();
            let mut demotions = handler.wrapping_sub(self.seen_demotions);
            let mut failures = refused.wrapping_sub(self.seen_failures);
            self.seen_demotions = handler;
            self.seen_failures = refused;
            match demote_thread(0) {
                Demotion::Demoted => demotions += 1,
                Demotion::Failed => failures += 1,
                Demotion::NotRealtime => {}
            }
            if demotions > 0 {
                state
                    .demotion_generation
                    .store(generation, Ordering::Release);
                state.demotion_hold_seq.store(hold_seq, Ordering::Release);
                state.demotions.fetch_add(demotions, Ordering::AcqRel);
                state.demoted.store(true, Ordering::Release);
            }
            if failures > 0 {
                state
                    .failed_demotions
                    .fetch_add(failures, Ordering::Relaxed);
            }
            restore_rttime_limits();
        }
    }

    /// A steady-state period was processed and written (one atomic add).
    #[inline]
    pub(crate) fn period_done(&self, state: &RtOverloadState) {
        state.periods.fetch_add(1, Ordering::Relaxed);
    }
}

/// What [`RepromoteGate::tick`] decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Repromote {
    Nothing,
    /// The audio thread was just found demoted (log the episode).
    Demoted,
    /// Time to promote it again.
    Promote,
}

/// The control thread's half: when to promote the demoted audio thread
/// again. Pure apart from [`Self::tick`]'s promotion call.
#[derive(Debug, Default)]
pub(crate) struct RepromoteGate {
    seen_overloads: u64,
    seen_failed: u64,
    /// Since when the load has been under [`REPROMOTE_LOAD`] with no xrun.
    calm_since: Option<Instant>,
    last_xruns: u64,
    retry_after: Option<Instant>,
    in_episode: bool,
}

impl RepromoteGate {
    /// The decision for one control-thread wake (no side effects).
    pub(crate) fn decide(
        &mut self,
        overloads: u64,
        demoted: bool,
        dsp_load: f64,
        xruns: u64,
        now: Instant,
    ) -> Repromote {
        let new_xruns = xruns != self.last_xruns;
        self.last_xruns = xruns;
        if overloads != self.seen_overloads {
            self.seen_overloads = overloads;
            self.calm_since = None;
            if !self.in_episode {
                self.in_episode = true;
                return Repromote::Demoted;
            }
        }
        if !demoted {
            self.in_episode = false;
            self.calm_since = None;
            return Repromote::Nothing;
        }
        if new_xruns || dsp_load >= REPROMOTE_LOAD {
            self.calm_since = None;
            return Repromote::Nothing;
        }
        let since = *self.calm_since.get_or_insert(now);
        if now.duration_since(since) < REPROMOTE_AFTER || self.retry_after.is_some_and(|t| now < t)
        {
            return Repromote::Nothing;
        }
        Repromote::Promote
    }

    /// One control-thread wake: restore the soft limit if it moved, log
    /// the episode, and promote the audio thread again when it is time.
    pub(crate) fn tick(
        &mut self,
        state: &RtOverloadState,
        priority: u32,
        dsp_load: f64,
        xruns: u64,
    ) {
        if restore_rttime_limits() {
            log::debug!("RLIMIT_RTTIME soft limit put back at half the hard limit");
        }
        let failed = state.failed_demotions.load(Ordering::Relaxed);
        if failed != self.seen_failed {
            self.seen_failed = failed;
            log::error!(
                "audio thread overload: the kernel refused to move it to normal priority; \
                 it may be killed at the RLIMIT_RTTIME hard limit"
            );
        }
        let demoted = state.demoted.load(Ordering::Acquire);
        let now = Instant::now();
        match self.decide(state.demotions(), demoted, dsp_load, xruns, now) {
            Repromote::Nothing => {}
            Repromote::Demoted => log::warn!(
                "audio thread got SIGXCPU (it ran 100 ms without sleeping): moved to normal \
                 priority so it is not killed; real-time again once the load stays under {:.0}%",
                REPROMOTE_LOAD * 100.0
            ),
            Repromote::Promote => {
                let tid = state.tid.load(Ordering::Acquire);
                if tid <= 0 || !state.promoted.load(Ordering::Acquire) {
                    state.demoted.store(false, Ordering::Release);
                    return;
                }
                match promote_thread_rt(tid, priority) {
                    Ok(prio) => {
                        state.demoted.store(false, Ordering::Release);
                        self.retry_after = None;
                        log::info!(
                            "audio thread back to {} priority {prio} (load {:.0}%)",
                            audivane_io_alsa::thread_policy_name(tid),
                            dsp_load * 100.0
                        );
                    }
                    Err(err) => {
                        self.retry_after = Some(now + REPROMOTE_RETRY);
                        log::warn!("could not make the audio thread real-time again: {err}");
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The handler's audio thread and counters are per process: the tests
    /// that register one take turns.
    static AUDIO_THREAD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn sigxcpu_to_self() {
        // SAFETY: a signal to the calling thread; the guard is installed.
        unsafe {
            libc::syscall(
                libc::SYS_tgkill,
                libc::getpid(),
                libc::syscall(libc::SYS_gettid),
                libc::SIGXCPU,
            );
        }
    }

    #[test]
    fn a_signal_for_a_thread_at_normal_priority_is_no_demotion() {
        let _turn = AUDIO_THREAD.lock().unwrap_or_else(|e| e.into_inner());
        // The test thread is not real-time: a SIGXCPU reaching it (as a
        // process-wide one can) moves the signal count only.
        audivane_io_alsa::install_overload_guard().unwrap();
        let state = RtOverloadState::default();
        let mut guard = RtLoopGuard::new(&state, false);
        sigxcpu_to_self();
        guard.on_iteration(&state, 0, 0);
        assert_eq!(state.signals(), 1);
        assert_eq!(state.demotions(), 0);
        assert_eq!(state.failed_demotions(), 0);
        assert!(!state.demoted());
        drop(guard);
    }

    #[test]
    fn a_high_load_keeps_it_demoted() {
        let mut gate = RepromoteGate::default();
        let t0 = Instant::now();
        assert_eq!(gate.decide(1, true, 1.1, 0, t0), Repromote::Demoted);
        for s in 1..10 {
            let now = t0 + Duration::from_secs(s);
            assert_eq!(gate.decide(1, true, 0.9, 0, now), Repromote::Nothing);
        }
    }
}
