//! Real-time thread priority via `org.freedesktop.RealtimeKit1`, and the
//! guard that keeps an overloaded real-time thread from being killed.
//!
//! rtkit only grants real-time scheduling (`SCHED_RR`) to a process whose
//! `RLIMIT_RTTIME` hard limit is at most its `RTTimeUSecMax` (200 ms by
//! default). The kernel counts, per thread, the CPU time a real-time thread
//! uses without sleeping (a sleep resets it, `sched_yield` does not): at the
//! soft limit it sends `SIGXCPU` and raises that limit by one second, at the
//! hard limit it kills the process. A chain that costs more than real time
//! never lets the audio thread sleep, so without a guard an overload kills
//! the engine 200 ms in.
//!
//! - **Prevention** ([`RunGuard`]): the audio loop checks, through the
//!   thread's voluntary context switch count, when it last really slept.
//!   After [`MAX_RUN_WITHOUT_SLEEP`] (50 ms) without one, and only while it
//!   is real-time, it sleeps for [`OVERLOAD_NAP`] (100 µs), which resets the
//!   kernel's counter: an overloaded thread stays real-time and keeps
//!   playing (with underruns) until the control plane falls back to a
//!   lighter chain.
//! - **Backstop** (`SIGXCPU`): the soft limit is half the hard one
//!   ([`RTTIME_SOFT_FRACTION`]). The signal reaches the thread that ran
//!   over, or another one when that thread blocks it, so the handler
//!   demotes the thread it runs on when that one is real-time, the
//!   registered audio thread ([`register_audio_thread`]) by its id, and
//!   every registered helper thread ([`register_helper_thread`]: real-time
//!   threads that work for the audio thread, such as the channel workers)
//!   by its id. It
//!   counts the signal ([`overload_signals`]) and each accepted demotion of
//!   the audio thread ([`audio_thread_demotions`]); the audio loop then
//!   makes sure it is demoted ([`demote_current_thread`]) and restores the
//!   soft limit the kernel raised ([`restore_rttime_limits`]), and its
//!   control thread promotes it again ([`promote_thread_rt`]) once the load
//!   is back under real time.
//!
//! Signal masks are left alone: `SIGXCPU` blocked everywhere but on the
//! audio thread would land there when another real-time thread ran over.
//! See `docs/architecture.md` ("Overload") for the rest.

use crate::error::{AlsaError, Result};
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use zbus::{Connection, Proxy};

/// Soft `RLIMIT_RTTIME` as a fraction of the hard one: the `SIGXCPU`
/// backstop fires halfway to the kill.
pub const RTTIME_SOFT_FRACTION: f64 = 0.5;

/// Longest the audio loop runs without a real sleep before [`RunGuard`]
/// makes it take one: half the soft limit at rtkit's default 200 ms hard
/// limit, and longer than any period the engine runs (a whole 8192-frame
/// quantum at 44.1 kHz is 186 ms of audio, but only work, not waiting,
/// counts: a period that waits for the device sleeps in it).
pub const MAX_RUN_WITHOUT_SLEEP: std::time::Duration = std::time::Duration::from_millis(50);

/// How long [`RunGuard`] sleeps. Any sleep that blocks resets the kernel's
/// count (it is reset on the wakeup), and a real-time thread has no timer
/// slack, so 100 µs always blocks; the real-time test measures that the
/// count is reset by it. It costs 0.2 % of an overloaded thread's time.
pub const OVERLOAD_NAP: std::time::Duration = std::time::Duration::from_micros(100);

/// `SIGXCPU` signals handled since the process started.
static OVERLOAD_SIGNALS: AtomicU64 = AtomicU64::new(0);
/// Thread id that received the last `SIGXCPU` (usually the one that ran
/// over; any thread when that one blocks the signal).
static OVERLOAD_TID: AtomicI32 = AtomicI32::new(0);
/// The audio thread, which the handler demotes by id (0: none).
static AUDIO_TID: AtomicI32 = AtomicI32::new(0);
/// Real-time to normal transitions of the audio thread made by the
/// handler (it was real-time, and the kernel accepted the demotion).
static AUDIO_DEMOTIONS: AtomicU64 = AtomicU64::new(0);
/// Demotions of the real-time audio thread by the handler that the kernel
/// refused.
static AUDIO_DEMOTION_FAILURES: AtomicU64 = AtomicU64::new(0);
/// Most helper threads the handler demotes by id.
pub const MAX_HELPER_THREADS: usize = 8;
/// Registered helper threads ([`register_helper_thread`]); 0: a free entry.
static HELPER_TIDS: [AtomicI32; MAX_HELPER_THREADS] =
    [const { AtomicI32::new(0) }; MAX_HELPER_THREADS];
/// Real-time to normal transitions of helper threads made by the handler.
static HELPER_DEMOTIONS: AtomicU64 = AtomicU64::new(0);
/// The hard `RLIMIT_RTTIME` set by the last promotion, in microseconds
/// (0: none), for [`restore_rttime_limits`].
static RTTIME_HARD_USEC: AtomicU64 = AtomicU64::new(0);

static GUARD: std::sync::Once = std::sync::Once::new();

/// `sched_setscheduler(tid, SCHED_OTHER | SCHED_RESET_ON_FORK)` as a raw
/// system call (async-signal-safe); whether the kernel accepted it.
fn demote_raw(tid: i32) -> bool {
    let param = libc::sched_param { sched_priority: 0 };
    // SAFETY: a plain system call with a local parameter block.
    unsafe {
        libc::syscall(
            libc::SYS_sched_setscheduler,
            tid,
            libc::SCHED_OTHER | libc::SCHED_RESET_ON_FORK,
            &raw const param,
        ) == 0
    }
}

/// The raw scheduling policy of `tid` (0: the calling thread), negative
/// on an error (async-signal-safe).
fn policy_raw(tid: i32) -> i32 {
    // SAFETY: a plain system call.
    unsafe { libc::syscall(libc::SYS_sched_getscheduler, tid) as i32 }
}

/// Whether a `sched_getscheduler` result is a real-time policy: both
/// `SCHED_FIFO` and `SCHED_RR` (what rtkit grants), with or without
/// `SCHED_RESET_ON_FORK`. The kernel counts `RLIMIT_RTTIME` for both.
fn policy_is_realtime(policy: i32) -> bool {
    policy >= 0
        && matches!(
            policy & !libc::SCHED_RESET_ON_FORK,
            libc::SCHED_FIFO | libc::SCHED_RR
        )
}

/// Whether `tid` (0: the calling thread) runs a real-time policy, as a raw
/// system call (async-signal-safe).
fn is_realtime_raw(tid: i32) -> bool {
    policy_is_realtime(policy_raw(tid))
}

/// The name of a `sched_getscheduler` result, for logs.
fn policy_name(policy: i32) -> &'static str {
    if policy < 0 {
        return "unknown policy";
    }
    match policy & !libc::SCHED_RESET_ON_FORK {
        libc::SCHED_FIFO => "SCHED_FIFO",
        libc::SCHED_RR => "SCHED_RR",
        libc::SCHED_OTHER => "SCHED_OTHER",
        libc::SCHED_BATCH => "SCHED_BATCH",
        libc::SCHED_IDLE => "SCHED_IDLE",
        _ => "another policy",
    }
}

/// The scheduling policy `tid` (0: the calling thread) runs now, read back
/// from the kernel, by name (`SCHED_RR` after an rtkit promotion), for
/// logs.
pub fn thread_policy_name(tid: i32) -> &'static str {
    policy_name(policy_raw(tid))
}

/// What [`demote_thread`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Demotion {
    /// The thread was not real-time: nothing to do (a signal for a thread
    /// already at normal priority is no demotion).
    NotRealtime,
    /// Real-time before, normal priority now.
    Demoted,
    /// Real-time, and the kernel refused the change.
    Failed,
}

/// Demote `tid` (0: the calling thread) to `SCHED_OTHER` if it is
/// real-time, saying which of the three happened. Async-signal-safe (raw
/// system calls only).
pub fn demote_thread(tid: i32) -> Demotion {
    if !is_realtime_raw(tid) {
        Demotion::NotRealtime
    } else if demote_raw(tid) && !is_realtime_raw(tid) {
        Demotion::Demoted
    } else {
        Demotion::Failed
    }
}

/// The `SIGXCPU` handler. The thread it runs on is the one that ran over
/// unless that one blocks the signal, so it demotes the registered audio
/// thread and every registered helper by id, then the calling thread when
/// it is still real-time. It counts four things apart: the signal, each
/// real-time to normal transition of the audio thread, each demotion of it
/// the kernel refused (a signal that finds the audio thread at normal
/// priority already is neither), and each helper's transition. Only system
/// calls and atomics.
extern "C" fn on_sigxcpu(_signal: libc::c_int) {
    // SAFETY: errno is thread-local; saved and restored around the calls.
    unsafe {
        let errno = libc::__errno_location();
        let saved = *errno;
        let tid = libc::syscall(libc::SYS_gettid) as i32;
        let audio = AUDIO_TID.load(Ordering::Acquire);
        for helper in &HELPER_TIDS {
            let id = helper.load(Ordering::Acquire);
            if id > 0 && id != audio && demote_thread(id) == Demotion::Demoted {
                HELPER_DEMOTIONS.fetch_add(1, Ordering::Release);
            }
        }
        if audio != tid && is_realtime_raw(0) {
            demote_raw(0);
        }
        if audio > 0 {
            match demote_thread(audio) {
                Demotion::Demoted => {
                    AUDIO_DEMOTIONS.fetch_add(1, Ordering::Release);
                }
                Demotion::Failed => {
                    AUDIO_DEMOTION_FAILURES.fetch_add(1, Ordering::Release);
                }
                Demotion::NotRealtime => {}
            }
        }
        OVERLOAD_TID.store(tid, Ordering::Relaxed);
        OVERLOAD_SIGNALS.fetch_add(1, Ordering::Release);
        *errno = saved;
    }
}

/// Install the `SIGXCPU` handler (once per process). Done before any soft
/// limit below the hard one is set: `SIGXCPU`'s default action ends the
/// process.
pub fn install_overload_guard() -> Result<()> {
    let mut result = Ok(());
    GUARD.call_once(|| {
        // SAFETY: a zeroed sigaction with a plain handler and an empty mask.
        let rc = unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_sigxcpu as *const () as libc::sighandler_t;
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(libc::SIGXCPU, &action, std::ptr::null_mut())
        };
        if rc != 0 {
            result = Err(AlsaError::Io(std::io::Error::last_os_error()));
        }
    });
    result
}

/// `SIGXCPU` signals handled since the process started. Receiving one is
/// not a demotion: see [`audio_thread_demotions`].
pub fn overload_signals() -> u64 {
    OVERLOAD_SIGNALS.load(Ordering::Acquire)
}

/// Thread id that received the last `SIGXCPU` (0: none yet). Not always
/// the one that ran over.
pub fn last_overload_tid() -> i32 {
    OVERLOAD_TID.load(Ordering::Relaxed)
}

/// Register the calling thread as the audio thread: the one the `SIGXCPU`
/// handler demotes by id, whichever thread the signal reaches. Returns its
/// id. One audio thread per process; the last registration wins.
pub fn register_audio_thread() -> i32 {
    let tid = current_tid();
    AUDIO_TID.store(tid, Ordering::Release);
    tid
}

/// Forget the audio thread if `tid` is still the registered one (call
/// before the thread ends, so a later signal does not demote a reused id).
pub fn unregister_audio_thread(tid: i32) {
    let _ = AUDIO_TID.compare_exchange(tid, 0, Ordering::AcqRel, Ordering::Relaxed);
}

/// The registered audio thread's id (0: none).
pub fn audio_thread_tid() -> i32 {
    AUDIO_TID.load(Ordering::Acquire)
}

/// Register the calling thread as a helper of the audio thread: a
/// real-time thread the `SIGXCPU` handler demotes by id with the audio
/// thread, whichever thread the signal reaches (an overloaded helper that
/// blocks it, or that another thread beats to it, would otherwise stay
/// real-time until the hard limit kills the process). Returns its id, or
/// `None` when all [`MAX_HELPER_THREADS`] entries are taken.
pub fn register_helper_thread() -> Option<i32> {
    let tid = current_tid();
    HELPER_TIDS
        .iter()
        .find(|e| {
            e.compare_exchange(0, tid, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
        })
        .map(|_| tid)
}

/// Forget helper `tid` (call before the thread ends, so a later signal does
/// not demote a reused id).
pub fn unregister_helper_thread(tid: i32) {
    for e in &HELPER_TIDS {
        let _ = e.compare_exchange(tid, 0, Ordering::AcqRel, Ordering::Relaxed);
    }
}

/// Real-time to normal transitions of registered helper threads made by
/// the `SIGXCPU` handler, since the process started.
pub fn helper_thread_demotions() -> u64 {
    HELPER_DEMOTIONS.load(Ordering::Acquire)
}

/// Real-time to normal transitions of the registered audio thread made by
/// the `SIGXCPU` handler, since the process started.
pub fn audio_thread_demotions() -> u64 {
    AUDIO_DEMOTIONS.load(Ordering::Acquire)
}

/// Demotions of the real-time audio thread by the `SIGXCPU` handler that
/// the kernel refused, since the process started.
pub fn audio_thread_demotion_failures() -> u64 {
    AUDIO_DEMOTION_FAILURES.load(Ordering::Acquire)
}

/// The kernel thread id of the calling thread.
pub fn current_tid() -> i32 {
    // SAFETY: gettid has no arguments and cannot fail.
    unsafe { libc::syscall(libc::SYS_gettid) as i32 }
}

/// Set `RLIMIT_RTTIME` to `hard_usec` with the soft limit at
/// [`RTTIME_SOFT_FRACTION`] of it, after installing the `SIGXCPU` guard.
pub fn set_rttime_limits(hard_usec: u64) -> Result<()> {
    install_overload_guard()?;
    let soft = ((hard_usec as f64) * RTTIME_SOFT_FRACTION) as u64;
    let limit = libc::rlimit {
        rlim_cur: soft.max(1) as libc::rlim_t,
        rlim_max: hard_usec as libc::rlim_t,
    };
    // SAFETY: standard setrlimit for RLIMIT_RTTIME (process-wide).
    if unsafe { libc::setrlimit(libc::RLIMIT_RTTIME, &limit) } != 0 {
        return Err(AlsaError::Io(std::io::Error::last_os_error()));
    }
    RTTIME_HARD_USEC.store(hard_usec, Ordering::Relaxed);
    Ok(())
}

/// The current hard `RLIMIT_RTTIME` in microseconds (`None`: unlimited).
fn current_hard_usec() -> Option<u64> {
    let mut cur = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit into a local.
    if unsafe { libc::getrlimit(libc::RLIMIT_RTTIME, &mut cur) } != 0
        || cur.rlim_max == libc::RLIM_INFINITY
    {
        return None;
    }
    Some(cur.rlim_max)
}

/// Put the soft limit back at [`RTTIME_SOFT_FRACTION`] of the hard one
/// when something moved it: the kernel raises it by a second with every
/// `SIGXCPU`, and a PipeWire context loading its RT module sets soft and
/// hard to the same value. Without this a second overload would reach the
/// hard limit with no warning. Returns whether it changed anything.
/// Cheap (one `getrlimit`); no-op before a promotion set the limits.
pub fn restore_rttime_limits() -> bool {
    if RTTIME_HARD_USEC.load(Ordering::Relaxed) == 0 {
        return false;
    }
    let mut cur = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit into a local.
    if unsafe { libc::getrlimit(libc::RLIMIT_RTTIME, &mut cur) } != 0
        || cur.rlim_max == libc::RLIM_INFINITY
    {
        return false;
    }
    let hard = cur.rlim_max;
    let soft = (((hard as f64) * RTTIME_SOFT_FRACTION) as u64).max(1);
    if cur.rlim_cur == soft {
        return false;
    }
    set_rttime_limits(hard).is_ok()
}

/// Demote the calling thread to `SCHED_OTHER` (what the `SIGXCPU` handler
/// does; idempotent). Keeps `SCHED_RESET_ON_FORK`, which an unprivileged
/// thread may not clear once rtkit set it.
pub fn demote_current_thread() -> bool {
    demote_raw(0)
}

/// Whether the thread `tid` (0: the calling one) runs a real-time policy.
pub fn thread_is_realtime(tid: i32) -> bool {
    is_realtime_raw(tid)
}

/// Voluntary context switches of the calling thread: it moves each time
/// the thread blocks (and so each time the kernel's real-time run counter
/// is reset on its wakeup).
fn voluntary_switches() -> i64 {
    // SAFETY: getrusage into a zeroed local.
    unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        if libc::getrusage(libc::RUSAGE_THREAD, &mut usage) != 0 {
            return -1;
        }
        usage.ru_nvcsw as i64
    }
}

/// Monotonic time in nanoseconds.
fn mono_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime into a local.
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

/// Prevention half of the overload guard, owned by the audio thread: call
/// [`Self::check`] on every loop iteration (and right after the loop's
/// wait). When the thread has not slept for [`MAX_RUN_WITHOUT_SLEEP`] and
/// is real-time, it sleeps for [`OVERLOAD_NAP`]. No allocation, no lock:
/// two system calls per check, one more when the limit is reached.
#[derive(Debug)]
pub struct RunGuard {
    switches: i64,
    since_ns: u64,
    max_run_ns: u64,
    naps: u64,
    only_realtime: bool,
}

impl Default for RunGuard {
    fn default() -> Self {
        Self::new(MAX_RUN_WITHOUT_SLEEP)
    }
}

impl RunGuard {
    /// A guard for the calling thread with its own run limit (tests).
    pub fn new(max_run: std::time::Duration) -> Self {
        Self {
            switches: voluntary_switches(),
            since_ns: mono_ns(),
            max_run_ns: max_run.as_nanos() as u64,
            naps: 0,
            only_realtime: true,
        }
    }

    /// Nap whatever the thread's policy (the allocation test, which has no
    /// real-time thread).
    #[doc(hidden)]
    pub fn nap_when_not_realtime(mut self) -> Self {
        self.only_realtime = false;
        self
    }

    /// Note a sleep if there was one; nap if there was none for too long
    /// and the thread is real-time (a `SCHED_OTHER` thread is not counted
    /// by the kernel and needs no nap). Returns whether it napped.
    pub fn check(&mut self) -> bool {
        let now = mono_ns();
        let switches = voluntary_switches();
        if switches != self.switches {
            self.switches = switches;
            self.since_ns = now;
            return false;
        }
        if now.saturating_sub(self.since_ns) < self.max_run_ns {
            return false;
        }
        if self.only_realtime && !is_realtime_raw(0) {
            self.since_ns = now;
            return false;
        }
        std::thread::sleep(OVERLOAD_NAP);
        self.switches = voluntary_switches();
        self.since_ns = mono_ns();
        self.naps += 1;
        true
    }

    /// Naps taken so far.
    pub fn naps(&self) -> u64 {
        self.naps
    }
}

/// Promote the calling thread to real-time priority via rtkit (which
/// grants `SCHED_RR`).
///
/// Clamps `requested_priority` to the system's `MaxRealtimePriority` and sets
/// `RLIMIT_RTTIME` (hard: `RTTimeUSecMax`, as `rtkit-daemon` requires; soft:
/// half of it, with the `SIGXCPU` guard installed, see the module docs).
/// Returns the granted priority on success.
pub fn promote_current_thread_rt(requested_priority: u32) -> Result<u32> {
    let tid = current_tid();
    if tid <= 0 {
        return Err(AlsaError::Io(std::io::Error::last_os_error()));
    }
    promote_thread_rt(tid, requested_priority)
}

/// [`promote_current_thread_rt`] for the thread `tid` of this process, from
/// any thread (the driver's control thread promotes the audio thread again
/// after an overload with it).
pub fn promote_thread_rt(tid: i32, requested_priority: u32) -> Result<u32> {
    zbus::block_on(async {
        let conn = Connection::system().await?;
        let proxy = Proxy::new(
            &conn,
            "org.freedesktop.RealtimeKit1",
            "/org/freedesktop/RealtimeKit1",
            "org.freedesktop.RealtimeKit1",
        )
        .await?;

        let max_prio: i32 = proxy.get_property("MaxRealtimePriority").await?;
        let max_rttime_usec: i64 = proxy.get_property("RTTimeUSecMax").await?;

        if max_prio <= 0 {
            return Err(AlsaError::Rtkit(format!(
                "rtkit MaxRealtimePriority is {max_prio}"
            )));
        }
        let prio = requested_priority.clamp(1, max_prio as u32);

        if max_rttime_usec > 0 {
            // Never above the current hard limit: raising it needs a
            // privilege (an earlier promotion, or the test, may have set a
            // lower one).
            let hard = match current_hard_usec() {
                Some(cur) => cur.min(max_rttime_usec as u64),
                None => max_rttime_usec as u64,
            };
            set_rttime_limits(hard)?;
        }

        let _: () = proxy
            .call("MakeThreadRealtime", &(tid as u64, prio))
            .await?;
        Ok(prio)
    })
}

/// rtkit's `RTTimeUSecMax` (asked once per process; `None`: no rtkit).
pub fn rtkit_rttime_max_usec() -> Option<u64> {
    static MAX: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    *MAX.get_or_init(|| {
        zbus::block_on(async {
            let conn = Connection::system().await.ok()?;
            let proxy = Proxy::new(
                &conn,
                "org.freedesktop.RealtimeKit1",
                "/org/freedesktop/RealtimeKit1",
                "org.freedesktop.RealtimeKit1",
            )
            .await
            .ok()?;
            let max: i64 = proxy.get_property("RTTimeUSecMax").await.ok()?;
            (max > 0).then_some(max as u64)
        })
    })
}

/// The `RLIMIT_RTTIME` (soft, hard) pair, in microseconds, that a
/// promotion of this process sets or has set: the current hard limit when
/// it is finite, else rtkit's maximum; the soft one at
/// [`RTTIME_SOFT_FRACTION`] of it. `None` without either. For other code
/// that sets the limits (a PipeWire context's RT module), so it sets the
/// same ones.
pub fn rttime_limits_usec() -> Option<(u64, u64)> {
    let hard = match (current_hard_usec(), rtkit_rttime_max_usec()) {
        (Some(cur), Some(max)) => cur.min(max),
        (Some(cur), None) => cur,
        (None, max) => max?,
    };
    let soft = (((hard as f64) * RTTIME_SOFT_FRACTION) as u64).max(1);
    Some((soft, hard))
}

/// Best-effort promotion to real-time priority via rtkit; logs a warning and
/// returns `None` if rtkit is unavailable or denies the request.
pub fn try_promote_current_thread_rt(requested_priority: u32) -> Option<u32> {
    match promote_current_thread_rt(requested_priority) {
        Ok(prio) => {
            log::info!(
                "ALSA thread promoted to {} priority {prio} via rtkit",
                thread_policy_name(0)
            );
            Some(prio)
        }
        Err(err) => {
            log::warn!("Could not promote ALSA thread via rtkit (continuing without RT): {err}");
            None
        }
    }
}

/// alsa-lib messages raised on threads set up by
/// [`silence_alsa_lib_on_this_thread`].
static ALSA_LIB_ERRORS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Route alsa-lib's own messages on the calling thread (e.g. from a failed
/// recovery `snd_pcm_prepare`) to a counter instead of stderr: the audio
/// thread must not write to stderr. Call once when the thread starts,
/// before it turns real-time; other threads keep alsa-lib's messages.
/// [`alsa_lib_errors`] reads the count, for logging from another thread.
pub fn silence_alsa_lib_on_this_thread() {
    // A thread-local handler: it takes a `va_list`, so unlike the global
    // (variadic) one it can be written in Rust. It replaces the default
    // printing for this thread only.
    unsafe extern "C" fn count(
        _file: *const std::ffi::c_char,
        _line: std::ffi::c_int,
        _function: *const std::ffi::c_char,
        _errcode: std::ffi::c_int,
        _fmt: *const std::ffi::c_char,
        _arg: *mut alsa_sys::__va_list_tag,
    ) {
        ALSA_LIB_ERRORS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    unsafe {
        alsa_sys::snd_lib_error_set_local(Some(count));
    }
}

/// alsa-lib messages counted on silenced threads since the process started.
pub fn alsa_lib_errors() -> u64 {
    ALSA_LIB_ERRORS.load(std::sync::atomic::Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rr_and_fifo_are_real_time_with_or_without_reset_on_fork() {
        for policy in [libc::SCHED_RR, libc::SCHED_FIFO] {
            assert!(policy_is_realtime(policy));
            assert!(policy_is_realtime(policy | libc::SCHED_RESET_ON_FORK));
        }
        for policy in [libc::SCHED_OTHER, libc::SCHED_BATCH, libc::SCHED_IDLE, -1] {
            assert!(!policy_is_realtime(policy));
            assert!(!policy_is_realtime(policy | libc::SCHED_RESET_ON_FORK));
        }
        assert_eq!(
            policy_name(libc::SCHED_RR | libc::SCHED_RESET_ON_FORK),
            "SCHED_RR"
        );
        assert_eq!(policy_name(libc::SCHED_FIFO), "SCHED_FIFO");
        assert_eq!(thread_policy_name(0), "SCHED_OTHER");
    }
}
