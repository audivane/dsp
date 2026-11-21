//! The audio thread's overload guard runs on every loop iteration: its
//! check, including a nap, and the `SIGXCPU` handler must not touch the
//! heap. A counting global allocator, armed on this thread, enforces it.

use audivane_io_alsa::{
    RunGuard, demote_current_thread, install_overload_guard, overload_signals,
    restore_rttime_limits,
};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::time::{Duration, Instant};

struct Counting;

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static COUNT: Cell<usize> = const { Cell::new(0) };
}

fn note() {
    let _ = ARMED.try_with(|armed| {
        if armed.get() {
            COUNT.with(|c| c.set(c.get() + 1));
        }
    });
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note();
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        note();
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note();
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

fn allocations(f: impl FnOnce()) -> usize {
    COUNT.with(|c| c.set(0));
    ARMED.with(|a| a.set(true));
    f();
    ARMED.with(|a| a.set(false));
    COUNT.with(|c| c.get())
}

#[test]
fn the_guard_never_allocates() {
    install_overload_guard().unwrap();
    let mut guard = RunGuard::new(Duration::from_millis(2)).nap_when_not_realtime();
    let before = overload_signals();
    let n = allocations(|| {
        let t0 = Instant::now();
        // Busy past the run limit several times: naps included.
        while t0.elapsed() < Duration::from_millis(20) {
            std::hint::spin_loop();
            guard.check();
        }
        // The handler path (as the kernel would raise it on this thread).
        unsafe {
            libc::syscall(
                libc::SYS_tgkill,
                libc::getpid(),
                libc::syscall(libc::SYS_gettid),
                libc::SIGXCPU,
            );
        }
        demote_current_thread();
        restore_rttime_limits();
    });
    assert_eq!(n, 0, "allocations on the RT path");
    assert!(guard.naps() >= 3, "naps: {}", guard.naps());
    assert_eq!(overload_signals(), before + 1);
}
