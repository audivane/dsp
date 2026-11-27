//! The `SIGXCPU` backstop for a registered helper thread (a channel worker):
//! a real-time helper that runs over while another thread takes the signal
//! is demoted by its id, as the audio thread is, never left real-time until
//! the hard limit kills the process.
//!
//! Its own file: `RLIMIT_RTTIME` and the handler's counters are per
//! process. It runs for real when rtkit grants real-time scheduling
//! (`SCHED_RR`); otherwise it only checks the registry and that the signal
//! is handled (simulated). The output says which ran.

use audivane_io_alsa::{
    MAX_HELPER_THREADS, current_tid, helper_thread_demotions, install_overload_guard,
    last_overload_tid, overload_signals, promote_current_thread_rt, register_helper_thread,
    set_rttime_limits, thread_is_realtime, unregister_helper_thread,
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

#[test]
fn a_registered_helper_is_demoted_when_another_thread_takes_the_signal() {
    install_overload_guard().unwrap();
    // A busy SCHED_OTHER thread with SIGXCPU unblocked, to take the signal.
    let stop = Arc::new(AtomicBool::new(false));
    let taker = Arc::new(AtomicI32::new(0));
    let busy = {
        let (stop, taker) = (stop.clone(), taker.clone());
        std::thread::spawn(move || {
            taker.store(current_tid(), Ordering::Release);
            while !stop.load(Ordering::Acquire) {
                std::hint::black_box(spin_for(Duration::from_millis(1)));
            }
        })
    };
    while taker.load(Ordering::Acquire) == 0 {
        std::thread::yield_now();
    }
    let taker_tid = taker.load(Ordering::Acquire);

    let (real, helper_tid) = std::thread::spawn(move || {
        block_sigxcpu_here();
        let real = promote_current_thread_rt(10).is_ok() && thread_is_realtime(0);
        let tid = register_helper_thread().expect("a free helper entry");
        let signals = overload_signals();
        let demotions = helper_thread_demotions();
        if real {
            set_rttime_limits(HARD_USEC).unwrap();
            // 300 ms without sleeping: past the soft limit (30 ms) and far
            // past the hard one (60 ms). The signal is blocked here, so
            // another thread handles it and must demote this one by id.
            std::hint::black_box(spin_for(Duration::from_millis(300)));
            assert!(
                helper_thread_demotions() > demotions,
                "helper demotion recorded"
            );
            assert!(!thread_is_realtime(0), "the helper was demoted by id");
        } else {
            eprintln!("rt_overload_helpers: rtkit promotion unavailable");
            unsafe {
                libc::syscall(libc::SYS_tgkill, libc::getpid(), taker_tid, libc::SIGXCPU);
            }
            let t0 = Instant::now();
            while overload_signals() == signals && t0.elapsed() < Duration::from_secs(2) {
                std::thread::yield_now();
            }
        }
        assert!(overload_signals() > signals, "SIGXCPU handled");
        assert_ne!(last_overload_tid(), tid, "another thread took the signal");
        unregister_helper_thread(tid);
        (real, tid)
    })
    .join()
    .expect("helper thread (killed at the hard limit if not demoted)");

    stop.store(true, Ordering::Release);
    busy.join().unwrap();
    eprintln!(
        "rt_overload_helpers: {}: signal taken by thread {}, helper {} demoted by id",
        if real { "REAL" } else { "SIMULATED" },
        last_overload_tid(),
        helper_tid
    );

    // The registry: entries are freed by unregistering, and a full one says so.
    let ids: Vec<_> = (0..MAX_HELPER_THREADS)
        .map(|_| {
            std::thread::spawn(|| register_helper_thread().expect("a free entry"))
                .join()
                .unwrap()
        })
        .collect();
    assert_eq!(register_helper_thread(), None, "full");
    for id in ids {
        unregister_helper_thread(id);
    }
    let me = register_helper_thread().expect("free again");
    unregister_helper_thread(me);
}
