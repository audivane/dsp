//! Counting-allocator check that the varispeed scheduler never allocates:
//! every call the RT thread makes (pulls, polls, bounds, commands, resets,
//! status) runs under an armed counter.

use audivane_io_pw::varispeed_sched::{
    PullKind, Timing, VarispeedScheduler, max_speed, pull_bound, ring_capacity,
};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

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
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note();
        unsafe { System.alloc_zeroed(layout) }
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
static ALLOCATOR: Counting = Counting;

#[test]
fn the_scheduler_never_allocates() {
    let timings = [
        Timing::new(48_000, 1024, 1024),
        Timing::new(48_000, 1024, 64),
        Timing::new(48_000, 256, 64),
        Timing::new(768_000, 8192, 32),
        Timing::new(48_000, 16_384, 8192),
    ];
    ARMED.with(|a| a.set(true));
    let mut s = VarispeedScheduler::new(timings[0]);
    let mut fill = 0usize;
    let mut done = 0u64;
    let mut now = 0u64;
    let mut sink = 0.0f64;
    for (round, &t) in timings.iter().cycle().take(16).enumerate() {
        s.reset(t);
        s.command(0.5 + round as f64 * 0.1);
        s.finish_priming();
        s.update_timing(t.with_quantum(t.quantum * 2));
        s.update_timing(t);
        for k in 0..2000u64 {
            now += 37_000;
            let p = s.poll(now, fill, done);
            if p.trigger {
                fill += t.quantum as usize;
                done += 1;
            }
            if k % 7 == 0 {
                let n_src = pull_bound(s.target(), t.period) - 2;
                let kind = if k % 91 == 0 {
                    PullKind::Drain
                } else {
                    PullKind::Play
                };
                let pull = s.on_pull(now, fill, n_src, kind);
                fill -= pull.take;
                s.set_next_bound(pull_bound(s.target(), t.period));
            }
        }
        let st = s.status();
        sink += st.target + st.cap.max_speed + st.counters.pulls as f64;
        sink += max_speed(
            t.period,
            t.quantum,
            t.graph_rate,
            t.cycle_bound_ns,
            t.proc_budget_ns,
        );
        sink += f64::from(u8::from(s.is_priming()));
        sink += ring_capacity(t.period, t.quantum) as f64;
        sink += st.cap.reason.map_or(0, |r| r.as_str().len()) as f64;
        sink += st.held.map_or(0, |h| h.as_str().len()) as f64;
    }
    ARMED.with(|a| a.set(false));
    assert!(sink.is_finite());
    assert_eq!(COUNT.with(Cell::get), 0, "the scheduler allocated");
}
