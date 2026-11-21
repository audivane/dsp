//! The `SIGXCPU` backstop when the signal reaches another thread: the
//! kernel sends it to the process and delivers it to any thread that does
//! not block it, so the handler demotes the registered audio thread by its
//! id. Here the real-time thread blocks `SIGXCPU`, so another thread (the
//! test's main thread or a busy helper) must take it.
//!
//! Its own file: `RLIMIT_RTTIME` and the handler's counters are per
//! process. It runs for real when rtkit grants real-time scheduling
//! (`SCHED_RR`); otherwise it sends the signal to a helper thread itself
//! (simulated). The output says which ran.

use audivane_io_alsa::{
    audio_thread_demotions, current_tid, install_overload_guard, last_overload_tid,
    overload_signals, promote_current_thread_rt, register_audio_thread, set_rttime_limits,
    thread_is_realtime, unregister_audio_thread,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::{Duration, Instant};

const HARD_USEC: u64 = 60_000;

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

fn block_sigxcpu_here() {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGXCPU);
        assert_eq!(
            libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()),
            0
        );
    }
}

/// Two busy SCHED_OTHER helpers with `SIGXCPU` unblocked.
fn helpers(stop: &Arc<AtomicBool>, tids: &Arc<[AtomicI32; 2]>) -> Vec<std::thread::JoinHandle<()>> {
    (0..2)
        .map(|i| {
            let (stop, tids) = (stop.clone(), tids.clone());
            std::thread::spawn(move || {
                tids[i].store(current_tid(), Ordering::Release);
                while !stop.load(Ordering::Acquire) {
                    std::hint::black_box(spin_for(Duration::from_millis(1)));
                }
            })
        })
        .collect()
}

#[test]
fn the_audio_thread_is_demoted_when_another_thread_takes_the_signal() {
    install_overload_guard().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let tids = Arc::new([AtomicI32::new(0), AtomicI32::new(0)]);
    let workers = helpers(&stop, &tids);
    while tids.iter().any(|t| t.load(Ordering::Acquire) == 0) {
        std::thread::yield_now();
    }
    let helper = tids[0].load(Ordering::Acquire);

    let (real, audio_tid) = std::thread::spawn(move || {
        block_sigxcpu_here();
        let tid = register_audio_thread();
        let real = promote_current_thread_rt(10).is_ok() && thread_is_realtime(0);
        let signals = overload_signals();
        let demotions = audio_thread_demotions();
        if real {
            set_rttime_limits(HARD_USEC).unwrap();
            // 300 ms without sleeping: past the soft limit (30 ms) and far
            // past the hard one (60 ms). The signal is blocked here, so
            // another thread handles it and must demote this one by id.
            std::hint::black_box(spin_for(Duration::from_millis(300)));
        } else {
            eprintln!("rt_overload_threads: rtkit promotion unavailable");
            unsafe {
                libc::syscall(libc::SYS_tgkill, libc::getpid(), helper, libc::SIGXCPU);
            }
            let t0 = Instant::now();
            while overload_signals() == signals && t0.elapsed() < Duration::from_secs(2) {
                std::thread::yield_now();
            }
        }
        assert!(overload_signals() > signals, "SIGXCPU handled");
        assert!(
            audio_thread_demotions() > demotions,
            "audio thread demotion recorded"
        );
        assert!(!thread_is_realtime(0), "the audio thread was demoted by id");
        assert_ne!(last_overload_tid(), tid, "another thread took the signal");
        unregister_audio_thread(tid);
        (real, tid)
    })
    .join()
    .expect("audio thread (killed at the hard limit if not demoted)");

    stop.store(true, Ordering::Release);
    for w in workers {
        w.join().unwrap();
    }
    assert_ne!(last_overload_tid(), audio_tid);
    eprintln!(
        "rt_overload_threads: {}: signal taken by thread {}, audio thread {} demoted by id",
        if real { "REAL" } else { "SIMULATED" },
        last_overload_tid(),
        audio_tid
    );
}
