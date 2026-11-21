//! The real-time overload guard: a thread that runs past its
//! `RLIMIT_RTTIME` soft limit is demoted, never killed, and the run guard
//! keeps a busy real-time thread under the limit by napping.
//!
//! `RLIMIT_RTTIME` is per process, so this file holds a single test (its
//! own process). It runs for real when rtkit grants real-time scheduling
//! (`SCHED_RR`) to the
//! test thread; otherwise it raises `SIGXCPU` itself and checks the
//! handler (simulated). The output says which ran.

use audivane_io_alsa::{
    RunGuard, audio_thread_demotion_failures, audio_thread_demotions, current_tid,
    demote_current_thread, install_overload_guard, last_overload_tid, overload_signals,
    promote_current_thread_rt, register_audio_thread, restore_rttime_limits, set_rttime_limits,
    thread_is_realtime, thread_policy_name,
};
use std::time::{Duration, Instant};

/// Hard limit for the test, in microseconds: the soft one is half (30 ms).
const HARD_USEC: u64 = 60_000;

fn soft_limit_usec() -> u64 {
    let mut cur = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    unsafe { libc::getrlimit(libc::RLIMIT_RTTIME, &mut cur) };
    cur.rlim_cur
}

/// Busy work without any system call that could sleep.
fn spin_for(d: Duration) -> u64 {
    let t0 = Instant::now();
    let mut x = 1u64;
    while t0.elapsed() < d {
        for _ in 0..1000 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
        }
    }
    x
}

/// Voluntary context switches of the calling thread.
fn voluntary_switches() -> i64 {
    unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_THREAD, &mut usage);
        usage.ru_nvcsw as i64
    }
}

fn real_run() {
    let tid = register_audio_thread();
    // rtkit wants the hard limit at most RTTimeUSecMax when it promotes;
    // lowering it afterwards is always allowed.
    set_rttime_limits(HARD_USEC).unwrap();
    assert_eq!(soft_limit_usec(), HARD_USEC / 2);

    // 1. One long block: 300 ms of CPU without sleeping, 5x the hard limit.
    let before = overload_signals();
    let demotions = audio_thread_demotions();
    std::hint::black_box(spin_for(Duration::from_millis(300)));
    assert!(
        overload_signals() > before,
        "SIGXCPU was not handled (the process would have been killed at the hard limit)"
    );
    assert!(audio_thread_demotions() > demotions, "demotion recorded");
    assert_eq!(
        last_overload_tid(),
        tid,
        "the thread that ran over is the one demoted"
    );
    assert!(
        !thread_is_realtime(0),
        "the handler demoted the thread to SCHED_OTHER"
    );
    // The kernel raised the soft limit by a second when it sent SIGXCPU.
    assert!(
        soft_limit_usec() > HARD_USEC,
        "kernel bumped the soft limit"
    );
    assert!(restore_rttime_limits());
    assert_eq!(soft_limit_usec(), HARD_USEC / 2, "soft limit restored");
    eprintln!("rt_overload: long block demoted at the soft limit, process alive");

    // 2. Prevention: promoted again, busy for 400 ms, but the run guard
    // naps every 10 ms (a third of the soft limit): no SIGXCPU, still RT.
    // Each nap is OVERLOAD_NAP (100 µs) and must really block: the
    // voluntary switch count moves with every nap, and the kernel's count
    // is reset by it (else 400 ms of spinning would pass the 30 ms soft
    // limit).
    promote_current_thread_rt(10).expect("re-promote via rtkit");
    set_rttime_limits(HARD_USEC).unwrap();
    assert!(thread_is_realtime(0));
    // rtkit grants SCHED_RR: the guard must count it as real-time (else it
    // never naps under rtkit and the kernel's count runs to the limit).
    let policy = thread_policy_name(0);
    assert!(
        policy == "SCHED_RR" || policy == "SCHED_FIFO",
        "promoted to {policy}"
    );
    let before = overload_signals();
    let mut guard = RunGuard::new(Duration::from_millis(10));
    let (mut blocked, mut naps) = (0u64, 0u64);
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(400) {
        std::hint::black_box(spin_for(Duration::from_millis(1)));
        let switches = voluntary_switches();
        if guard.check() {
            naps += 1;
            if voluntary_switches() != switches {
                blocked += 1;
            }
        }
    }
    assert_eq!(blocked, naps, "every 100 µs nap blocked");
    assert_eq!(
        overload_signals(),
        before,
        "the run guard kept it under the soft limit"
    );
    assert!(thread_is_realtime(0), "still real-time");
    assert!(guard.naps() >= 20, "napped {} times", guard.naps());

    // 3. A demoted thread is not counted by the kernel: the guard does not
    // nap for it.
    demote_current_thread();
    let mut idle = RunGuard::new(Duration::from_millis(10));
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(50) {
        std::hint::black_box(spin_for(Duration::from_millis(1)));
        idle.check();
    }
    assert_eq!(idle.naps(), 0, "no nap off real time");

    // 4. A signal for the audio thread at normal priority already: a
    // signal, not a demotion.
    signal_at_normal_priority_is_no_demotion();
    eprintln!(
        "rt_overload: run guard kept a busy {policy} thread alive ({} naps in 400 ms)",
        guard.naps()
    );
}

/// A `SIGXCPU` for the registered audio thread while it runs at normal
/// priority counts as a signal only.
fn signal_at_normal_priority_is_no_demotion() {
    register_audio_thread();
    assert!(!thread_is_realtime(0));
    let (signals, demotions, failures) = (
        overload_signals(),
        audio_thread_demotions(),
        audio_thread_demotion_failures(),
    );
    unsafe {
        libc::syscall(
            libc::SYS_tgkill,
            libc::getpid(),
            current_tid(),
            libc::SIGXCPU,
        );
    }
    assert_eq!(overload_signals(), signals + 1);
    assert_eq!(audio_thread_demotions(), demotions, "no transition");
    assert_eq!(audio_thread_demotion_failures(), failures);
}

fn simulated_run() {
    install_overload_guard().unwrap();
    let tid = current_tid();
    let before = overload_signals();
    unsafe {
        libc::syscall(libc::SYS_tgkill, libc::getpid(), tid, libc::SIGXCPU);
    }
    assert_eq!(overload_signals(), before + 1, "the handler ran");
    assert_eq!(last_overload_tid(), tid);
    assert!(!thread_is_realtime(0));
    // Not real-time: the guard does not nap (forced, it does).
    let mut guard = RunGuard::new(Duration::from_millis(10));
    let mut forced = RunGuard::new(Duration::from_millis(10)).nap_when_not_realtime();
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(100) {
        std::hint::black_box(spin_for(Duration::from_millis(1)));
        guard.check();
        forced.check();
    }
    assert_eq!(guard.naps(), 0, "no nap off real time");
    assert!(forced.naps() >= 5, "napped {} times", forced.naps());
    signal_at_normal_priority_is_no_demotion();
    eprintln!(
        "rt_overload: SIMULATED (rtkit did not grant real-time scheduling): handler and run guard"
    );
}

#[test]
fn an_overloaded_realtime_thread_is_demoted_not_killed() {
    std::thread::spawn(|| match promote_current_thread_rt(10) {
        Ok(_) if thread_is_realtime(0) => real_run(),
        other => {
            eprintln!(
                "rt_overload: rtkit promotion unavailable: {:?}",
                other.err()
            );
            simulated_run();
        }
    })
    .join()
    .expect("test thread");
}
