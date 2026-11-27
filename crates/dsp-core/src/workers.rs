//! Channel workers: persistent threads that run a chain's per-channel
//! stages for the audio thread, one lane each per block
//! ([`crate::chain::Chain::attach_workers`]).
//!
//! A [`LanePool`] owns the threads; the embedder spawns it once, with a
//! start hook that gives each worker the audio thread's scheduling
//! priority. A chain owns its lanes ([`Lane`]): the work for a group of
//! channels, behind a flag that says who holds it.
//!
//! # Handoff
//!
//! Per block, the audio thread fills each lane (input, frame counts) while
//! it holds it, hands lanes 1.. to their workers ([`LanePool::submit`]: an
//! atomic store into the worker's slot and one futex wake), runs lane 0
//! itself, then waits for the others ([`LanePool::wait`]): a short spin,
//! then a futex wait on the worker's slot with a timeout. A worker that
//! finds its slot empty sleeps on a futex until the next submit. No locks,
//! no allocation: a lane travels as an `Arc` clone (a reference count).
//! The worker gives up that clone before it hands the lane back, and a
//! chain dropped while a worker still holds a lane waits for it (off the
//! audio thread), so the lane's memory is always freed where the chain is
//! dropped, never on a worker. A worker takes jobs only once its start
//! hook (its priority request) has returned; until then the audio thread
//! runs its lanes.
//!
//! # Late workers
//!
//! The wait is bounded: past its deadline the audio thread stops waiting and
//! reports the lane late ([`LanePool::deadline`]). The worker still holds that lane's state, so the
//! chain cannot run it; it outputs silence until every lane is back, then
//! resets ([`crate::chain::Chain`] documents the exact rule). A worker that
//! is still busy when a block starts (a late job from before) is skipped:
//! the audio thread runs that lane itself. A panic in a lane is caught on
//! the worker and poisons the lane (the chain then stays silent). The
//! counters in [`PoolStats`] record all three.

use std::cell::UnsafeCell;
use std::mem::ManuallyDrop;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Jobs one worker can hold at once: a block of an A/B pair queues one lane
/// of each side.
const SLOT_JOBS: usize = 4;

/// Most workers a pool runs (lanes past the first): far above the engine's
/// two channels.
pub const MAX_WORKERS: usize = 63;

/// How long the audio thread spins on a lane before it sleeps on it.
const SPIN: Duration = Duration::from_micros(20);

/// What a lane runs on its worker.
pub trait LaneWork: Send {
    fn run(&mut self);
}

/// A worker's own hook, made by the pool's start hook on the worker thread:
/// [`Self::between_jobs`] runs on every turn of its loop, never during a job
/// (the engine's overload guard naps there).
pub trait WorkerHook {
    fn between_jobs(&mut self) {}
}

impl WorkerHook for () {}

/// How long dropping a lane a worker still holds waits for it to come back
/// (off the audio thread) before it leaks the lane instead of freeing it
/// under the worker.
const DROP_WAIT: Duration = Duration::from_secs(10);

const OWNED: u32 = 0;
const WITH_WORKER: u32 = 1;

struct LaneCell<W> {
    /// [`OWNED`]: the chain holds the work; [`WITH_WORKER`]: queued on or
    /// running on a worker.
    state: AtomicU32,
    poisoned: AtomicBool,
    work: UnsafeCell<W>,
}

// SAFETY: `work` is only reached by whoever `state` says holds it: the
// chain (through `&mut Lane`) while it is OWNED, one worker while it is
// WITH_WORKER. Both sides publish their writes with the state change
// (Release) and see the other's with its load (Acquire).
unsafe impl<W: Send> Sync for LaneCell<W> {}

/// A job as a worker slot holds it.
trait Job: Send + Sync {
    /// Run the work (`panic`: the test hook's injected panic). A panic is
    /// caught and poisons the lane.
    fn run(&self, stats: &PoolStats, panic: bool);
    /// The lane's state word. The worker takes it before dropping its
    /// reference and gives the work back with one `OWNED` store through it:
    /// no reference to the cell is alive when the chain may free it.
    fn state_word(&self) -> *const AtomicU32;
}

impl<W: LaneWork> Job for LaneCell<W> {
    fn run(&self, stats: &PoolStats, panic: bool) {
        debug_assert_eq!(self.state.load(Ordering::Acquire), WITH_WORKER);
        // SAFETY: the state is WITH_WORKER and this worker took the job
        // from its slot: nothing else touches `work` until it is OWNED.
        let work = unsafe { &mut *self.work.get() };
        if !self.poisoned.load(Ordering::Relaxed)
            && std::panic::catch_unwind(AssertUnwindSafe(|| {
                if panic {
                    panic!("injected lane panic (test hook)");
                }
                work.run();
            }))
            .is_err()
        {
            // Published with the hand-back's OWNED store (Release).
            self.poisoned.store(true, Ordering::Relaxed);
            stats.panics.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn state_word(&self) -> *const AtomicU32 {
        &self.state
    }
}

/// A chain's share of work for one worker: owned by the chain, lent to a
/// worker for one block at a time.
pub struct Lane<W: LaneWork + 'static> {
    /// Dropped by hand ([`Drop`]): never while a worker holds the lane.
    cell: ManuallyDrop<Arc<LaneCell<W>>>,
}

impl<W: LaneWork + 'static> Drop for Lane<W> {
    /// Free the lane here, never on a worker: a lane a worker still holds
    /// (a chain dropped during a stall) is waited for, polling, for up to
    /// `DROP_WAIT`; a worker stuck longer leaks it. Off the audio thread
    /// (chains are dropped off it).
    fn drop(&mut self) {
        let until = Instant::now() + DROP_WAIT;
        while !self.is_idle() {
            if Instant::now() >= until {
                return;
            }
            std::thread::sleep(Duration::from_micros(200));
        }
        // SAFETY: dropped once, here; the worker gave up its reference
        // before the OWNED store seen above, so this frees the cell when
        // the chain held the last one.
        unsafe { ManuallyDrop::drop(&mut self.cell) };
    }
}

impl<W: LaneWork + 'static> Lane<W> {
    pub fn new(work: W) -> Self {
        Self {
            cell: ManuallyDrop::new(Arc::new(LaneCell {
                state: AtomicU32::new(OWNED),
                poisoned: AtomicBool::new(false),
                work: UnsafeCell::new(work),
            })),
        }
    }

    /// The work, while no worker holds it.
    pub fn get_mut(&mut self) -> Option<&mut W> {
        if self.cell.state.load(Ordering::Acquire) == OWNED {
            // SAFETY: OWNED, and `&mut self` is the chain's only handle
            // (a `Lane` is not `Clone`; the worker's clone is gone once it
            // stored OWNED).
            Some(unsafe { &mut *self.cell.work.get() })
        } else {
            None
        }
    }

    /// No worker holds the work.
    pub fn is_idle(&self) -> bool {
        self.cell.state.load(Ordering::Acquire) == OWNED
    }

    /// A run of this lane panicked; its state and buffers are not to be
    /// trusted. Read it after [`Self::is_idle`] (or a successful
    /// [`LanePool::wait`]) returned true: the worker publishes it with the
    /// lane's return.
    pub fn is_poisoned(&self) -> bool {
        self.cell.poisoned.load(Ordering::Relaxed)
    }

    /// Run the work here, on the calling thread (lane 0, or a lane whose
    /// worker is busy). A panic propagates, as on a chain without workers.
    pub fn run_here(&mut self) {
        if let Some(work) = self.get_mut() {
            work.run();
        }
    }
}

const EMPTY: u32 = 0;
const FULL: u32 = 1;
const CLAIMED: u32 = 2;

struct Entry {
    /// [`EMPTY`], [`CLAIMED`] or [`FULL`]: who may touch `job` (the
    /// submitter that claimed it, the worker once it is FULL).
    state: AtomicU32,
    job: UnsafeCell<Option<Arc<dyn Job>>>,
}

struct Slot {
    entries: [Entry; SLOT_JOBS],
    /// Futex word the worker sleeps on: bumped by every submit and by
    /// shutdown.
    wake: AtomicU32,
    /// Futex word the audio thread waits on: bumped by the worker after
    /// every lane it hands back.
    done: AtomicU32,
    /// The worker's start hook returned: it takes jobs. Until then
    /// [`LanePool::is_idle`] is false and [`LanePool::submit`] refuses, so
    /// the audio thread runs its lanes.
    ready: AtomicBool,
    shutdown: AtomicBool,
    /// Test hook ([`LanePool::stall_next`]): nanoseconds to sleep before
    /// the next job.
    stall_ns: AtomicU64,
    /// Test hook ([`LanePool::panic_next`]): the next job panics.
    panic_next: AtomicBool,
}

// SAFETY: each entry's `job` is reached only by the side its `state` names
// (see `Entry`), with the hand-over published by that state.
unsafe impl Sync for Slot {}

impl Slot {
    fn new() -> Self {
        Self {
            entries: std::array::from_fn(|_| Entry {
                state: AtomicU32::new(EMPTY),
                job: UnsafeCell::new(None),
            }),
            wake: AtomicU32::new(0),
            done: AtomicU32::new(0),
            ready: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
            stall_ns: AtomicU64::new(0),
            panic_next: AtomicBool::new(false),
        }
    }

    fn idle(&self) -> bool {
        self.ready.load(Ordering::Acquire)
            && self
                .entries
                .iter()
                .all(|e| e.state.load(Ordering::Acquire) == EMPTY)
    }
}

/// Counters a pool keeps for the control plane (all relaxed).
#[derive(Debug, Default)]
pub struct PoolStats {
    /// Blocks in which a lane missed its deadline (the chain went silent).
    pub late_blocks: AtomicU64,
    /// Lanes the audio thread ran itself because their worker was busy.
    pub inline_runs: AtomicU64,
    /// Lane runs that panicked on a worker.
    pub panics: AtomicU64,
}

/// The worker threads.
pub struct LanePool {
    slots: Vec<Arc<Slot>>,
    stats: Arc<PoolStats>,
    /// The shortest wait for a lane ([`Self::deadline`]), in nanoseconds.
    min_wait_ns: AtomicU64,
}

/// The shortest wait for a lane: a block of a few frames still gets this
/// long.
const MIN_WAIT: Duration = Duration::from_millis(2);

impl LanePool {
    /// Spawn `workers` threads (at most [`MAX_WORKERS`]), each calling
    /// `on_start(index)` first (to set its priority), on the worker, and
    /// keeping the [`WorkerHook`] it returns. A worker takes jobs once its
    /// `on_start` returned. Off the real-time thread.
    pub fn spawn<F, H>(workers: usize, on_start: F) -> std::io::Result<Arc<Self>>
    where
        F: Fn(usize) -> H + Send + Sync + 'static,
        H: WorkerHook,
    {
        let on_start = Arc::new(on_start);
        let stats = Arc::new(PoolStats::default());
        let mut slots = Vec::new();
        for i in 0..workers.min(MAX_WORKERS) {
            let slot = Arc::new(Slot::new());
            let (thread_slot, thread_stats, start) =
                (Arc::clone(&slot), Arc::clone(&stats), Arc::clone(&on_start));
            std::thread::Builder::new()
                .name(format!("audivane-lane-{}", i + 1))
                .spawn(move || {
                    let mut hook = start(i);
                    drop(start);
                    thread_slot.ready.store(true, Ordering::Release);
                    worker_loop(&thread_slot, &thread_stats, &mut hook);
                })?;
            slots.push(slot);
        }
        Ok(Arc::new(Self {
            slots,
            stats,
            min_wait_ns: AtomicU64::new(MIN_WAIT.as_nanos() as u64),
        }))
    }

    pub fn workers(&self) -> usize {
        self.slots.len()
    }

    /// Wait (polling, off the audio thread) until every worker's start hook
    /// has returned, at most `timeout`. Returns whether they all have.
    pub fn wait_started(&self, timeout: Duration) -> bool {
        let until = Instant::now() + timeout;
        loop {
            if self.slots.iter().all(|s| s.ready.load(Ordering::Acquire)) {
                return true;
            }
            if Instant::now() >= until {
                return false;
            }
            std::thread::sleep(Duration::from_micros(500));
        }
    }

    pub fn stats(&self) -> &PoolStats {
        &self.stats
    }

    /// Worker `w` holds no job: a lane handed to it now starts at once.
    pub fn is_idle(&self, w: usize) -> bool {
        self.slots.get(w).is_some_and(|s| s.idle())
    }

    /// Hand `lane` to worker `w`. Returns false (and leaves the lane with
    /// the caller) when the worker has no free entry or the lane is not
    /// idle. No allocation: one reference count and one futex wake. Safe
    /// from several threads at once (an entry is claimed first), though the
    /// engine submits from its audio thread alone.
    pub fn submit<W: LaneWork + 'static>(&self, w: usize, lane: &mut Lane<W>) -> bool {
        let Some(slot) = self.slots.get(w) else {
            return false;
        };
        if !lane.is_idle() || !slot.ready.load(Ordering::Acquire) {
            return false;
        }
        // Claimed, so two submitting threads never share an entry.
        let Some(entry) = slot.entries.iter().find(|e| {
            e.state
                .compare_exchange(EMPTY, CLAIMED, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
        }) else {
            return false;
        };
        lane.cell.state.store(WITH_WORKER, Ordering::Relaxed);
        let job: Arc<dyn Job> = Arc::clone(&lane.cell) as Arc<dyn Job>;
        // SAFETY: this thread claimed the entry, so only it touches its job;
        // the worker reads it after the FULL store below.
        unsafe { *entry.job.get() = Some(job) };
        entry.state.store(FULL, Ordering::Release);
        slot.wake.fetch_add(1, Ordering::SeqCst);
        futex::wake(&slot.wake);
        true
    }

    /// Wait until worker `w` gives `lane` back, at most until `deadline`: a
    /// short spin, then a futex wait on the worker's slot (one bounded
    /// syscall per wake). Returns whether the lane is back.
    pub fn wait<W: LaneWork + 'static>(&self, w: usize, lane: &Lane<W>, deadline: Instant) -> bool {
        let Some(slot) = self.slots.get(w) else {
            return lane.is_idle();
        };
        let spin_until = Instant::now() + SPIN;
        loop {
            // Read before the check: a hand-back after it moves the word,
            // and the futex wait below returns at once.
            let seen = slot.done.load(Ordering::Acquire);
            if lane.is_idle() {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            if now < spin_until {
                std::hint::spin_loop();
                continue;
            }
            futex::wait(&slot.done, seen, Some(deadline - now));
        }
    }

    /// The deadline of a wait for lanes handed out at `start`, on a block of
    /// `frames` at `rate`: two block durations, at least 2 ms. Up to one
    /// block late the stream underruns as an overloaded chain without
    /// workers would (and the overload fallback sees it); past that a worker
    /// is stuck, and the chain goes silent instead of holding the audio
    /// thread.
    pub fn deadline(&self, start: Instant, frames: usize, rate: u32) -> Instant {
        let block = Duration::from_secs_f64(frames as f64 / f64::from(rate.max(1)));
        let floor = Duration::from_nanos(self.min_wait_ns.load(Ordering::Relaxed));
        start + (2 * block).max(floor)
    }

    /// Test hook: wait at least `d` for a lane, so a test of the output
    /// does not depend on how busy the machine is.
    #[doc(hidden)]
    pub fn set_min_wait(&self, d: Duration) {
        self.min_wait_ns
            .store(d.as_nanos() as u64, Ordering::Relaxed);
    }

    /// Test hook: worker `w` sleeps `d` before its next job (a late
    /// worker, for the tests of the deadline).
    #[doc(hidden)]
    pub fn stall_next(&self, w: usize, d: Duration) {
        if let Some(slot) = self.slots.get(w) {
            slot.stall_ns.store(d.as_nanos() as u64, Ordering::Relaxed);
        }
    }

    /// Test hook: worker `w`'s next job panics (inside the worker's catch,
    /// as a panic in a stage would).
    #[doc(hidden)]
    pub fn panic_next(&self, w: usize) {
        if let Some(slot) = self.slots.get(w) {
            slot.panic_next.store(true, Ordering::Relaxed);
        }
    }

    pub(crate) fn note_late(&self) {
        self.stats.late_blocks.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn note_inline(&self) {
        self.stats.inline_runs.fetch_add(1, Ordering::Relaxed);
    }
}

impl Drop for LanePool {
    /// Tell the workers to stop (they finish their jobs and exit on their
    /// own; nothing joins, so the last owner never blocks).
    fn drop(&mut self) {
        for slot in &self.slots {
            slot.shutdown.store(true, Ordering::SeqCst);
            slot.wake.fetch_add(1, Ordering::SeqCst);
            futex::wake(&slot.wake);
        }
    }
}

fn worker_loop(slot: &Slot, stats: &PoolStats, hook: &mut impl WorkerHook) {
    loop {
        hook.between_jobs();
        let seen = slot.wake.load(Ordering::SeqCst);
        let mut ran = false;
        for entry in &slot.entries {
            if entry.state.load(Ordering::Acquire) != FULL {
                continue;
            }
            if ran {
                // Back-to-back jobs (A/B) each get the overload guard.
                hook.between_jobs();
            }
            // SAFETY: FULL: the submitter is done with the entry and only
            // this worker touches it until the EMPTY store.
            let job = unsafe { (*entry.job.get()).take() };
            let stall = slot.stall_ns.swap(0, Ordering::Relaxed);
            if stall != 0 {
                std::thread::sleep(Duration::from_nanos(stall));
            }
            let panic = slot.panic_next.swap(false, Ordering::Relaxed);
            if let Some(job) = job {
                job.run(stats, panic);
                // Free the entry first: a chain that sees its lane back
                // sees its worker idle.
                entry.state.store(EMPTY, Ordering::Release);
                let state = job.state_word();
                // Never the last reference: the lane keeps its own until it
                // sees the store below (`Lane`'s drop waits for it), so the
                // worker never frees a lane.
                drop(job);
                // SAFETY: alive through the lane's reference, see above.
                // Only this atomic is touched, and nothing after the store:
                // the chain may free the cell as soon as it lands.
                unsafe { (*state).store(OWNED, Ordering::Release) };
            } else {
                entry.state.store(EMPTY, Ordering::Release);
            }
            slot.done.fetch_add(1, Ordering::Release);
            futex::wake(&slot.done);
            ran = true;
        }
        if ran {
            continue;
        }
        if slot.shutdown.load(Ordering::SeqCst) {
            return;
        }
        // Sleeps unless a submit (or the shutdown) moved `wake` since it
        // was read above.
        futex::wait(&slot.wake, seen, None);
    }
}

#[cfg(target_os = "linux")]
mod futex {
    use std::sync::atomic::AtomicU32;
    use std::time::Duration;

    /// Sleep while `word` holds `expected`, at most `timeout`; returns on a
    /// wake, a timeout, a signal, or at once when the word moved.
    pub(super) fn wait(word: &AtomicU32, expected: u32, timeout: Option<Duration>) {
        let ts = timeout.map(|t| libc::timespec {
            tv_sec: t.as_secs().min(i64::MAX as u64) as libc::time_t,
            tv_nsec: t.subsec_nanos() as libc::c_long,
        });
        let ts_ptr = ts
            .as_ref()
            .map_or(std::ptr::null(), |t| t as *const libc::timespec);
        // SAFETY: a futex wait on a live, aligned u32 of this process.
        unsafe {
            libc::syscall(
                libc::SYS_futex,
                word.as_ptr(),
                libc::FUTEX_WAIT | libc::FUTEX_PRIVATE_FLAG,
                expected,
                ts_ptr,
            );
        }
    }

    /// Wake every thread sleeping on `word`.
    pub(super) fn wake(word: &AtomicU32) {
        // SAFETY: a futex wake on a live, aligned u32 of this process.
        unsafe {
            libc::syscall(
                libc::SYS_futex,
                word.as_ptr(),
                libc::FUTEX_WAKE | libc::FUTEX_PRIVATE_FLAG,
                i32::MAX,
            );
        }
    }
}

/// Elsewhere (the desktop app builds dsp-core, but only the Linux engine
/// runs workers): a short sleep for a wait, nothing for a wake.
#[cfg(not(target_os = "linux"))]
mod futex {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    pub(super) fn wait(word: &AtomicU32, expected: u32, timeout: Option<Duration>) {
        if word.load(Ordering::SeqCst) == expected {
            let nap = Duration::from_micros(50);
            std::thread::sleep(timeout.map_or(nap, |t| t.min(nap)));
        }
    }

    pub(super) fn wake(_word: &AtomicU32) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Add {
        x: u64,
        delay: Duration,
        panic: bool,
    }

    impl LaneWork for Add {
        fn run(&mut self) {
            if self.panic {
                panic!("lane test panic");
            }
            if !self.delay.is_zero() {
                std::thread::sleep(self.delay);
            }
            self.x += 1;
        }
    }

    fn pool(n: usize) -> Arc<LanePool> {
        let p = LanePool::spawn(n, |_| {}).unwrap();
        assert!(p.wait_started(Duration::from_secs(5)));
        p
    }

    fn back<W: LaneWork + 'static>(p: &LanePool, w: usize, lane: &Lane<W>) -> bool {
        p.wait(w, lane, Instant::now() + Duration::from_secs(5))
    }

    fn lane(delay: Duration) -> Lane<Add> {
        Lane::new(Add {
            x: 0,
            delay,
            panic: false,
        })
    }

    #[test]
    fn a_submitted_lane_runs_and_comes_back() {
        let p = pool(2);
        let mut a = lane(Duration::ZERO);
        let mut b = lane(Duration::ZERO);
        for i in 1..=1000 {
            assert!(p.submit(0, &mut a));
            assert!(p.submit(1, &mut b));
            assert!(back(&p, 0, &a) && back(&p, 1, &b));
            assert_eq!(a.get_mut().unwrap().x, i);
            assert_eq!(b.get_mut().unwrap().x, i);
        }
    }

    #[test]
    fn a_late_lane_times_out_then_comes_back() {
        let p = pool(1);
        let mut a = lane(Duration::from_millis(50));
        assert!(p.submit(0, &mut a));
        let t0 = Instant::now();
        assert!(!p.wait(0, &a, t0 + Duration::from_millis(5)));
        assert!(
            t0.elapsed() < Duration::from_millis(40),
            "the wait is bounded"
        );
        assert!(a.get_mut().is_none(), "the worker still holds it");
        assert!(!p.is_idle(0));
        assert!(
            !p.submit(0, &mut a),
            "a lane a worker holds is not handed out again"
        );
        assert!(back(&p, 0, &a));
        assert!(p.is_idle(0), "a lane back means its worker is idle");
        assert_eq!(a.get_mut().unwrap().x, 1);
    }

    /// Records the thread its drop runs on.
    struct Tracked(
        Arc<std::sync::Mutex<Option<std::thread::ThreadId>>>,
        Duration,
    );

    impl LaneWork for Tracked {
        fn run(&mut self) {
            std::thread::sleep(self.1);
        }
    }

    impl Drop for Tracked {
        fn drop(&mut self) {
            *self.0.lock().unwrap() = Some(std::thread::current().id());
        }
    }

    #[test]
    fn a_lane_dropped_while_a_worker_runs_it_is_freed_by_its_owner() {
        let p = pool(1);
        let dropped_on = Arc::new(std::sync::Mutex::new(None));
        let mut a = Lane::new(Tracked(Arc::clone(&dropped_on), Duration::from_millis(50)));
        assert!(p.submit(0, &mut a));
        let t = Instant::now();
        drop(a);
        assert!(
            t.elapsed() >= Duration::from_millis(30),
            "the drop waited for the worker"
        );
        assert_eq!(
            *dropped_on.lock().unwrap(),
            Some(std::thread::current().id()),
            "the lane's memory is freed by the thread that dropped it, never the worker"
        );
        assert!(p.is_idle(0));
    }

    #[test]
    fn a_worker_takes_no_job_before_its_start_hook_returns() {
        let p = LanePool::spawn(1, |_| std::thread::sleep(Duration::from_millis(300))).unwrap();
        let mut a = lane(Duration::ZERO);
        assert!(!p.is_idle(0), "a worker still starting is not idle");
        assert!(!p.submit(0, &mut a), "nor is it handed a lane");
        assert!(a.get_mut().is_some(), "the lane stays with the caller");
        assert!(p.wait_started(Duration::from_secs(5)));
        assert!(p.is_idle(0));
        assert!(p.submit(0, &mut a));
        assert!(back(&p, 0, &a));
        assert_eq!(a.get_mut().unwrap().x, 1);
    }

    #[test]
    fn the_worker_hook_runs_between_jobs() {
        struct Count(Arc<AtomicU64>);
        impl WorkerHook for Count {
            fn between_jobs(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        let turns = Arc::new(AtomicU64::new(0));
        let t2 = Arc::clone(&turns);
        let p = LanePool::spawn(1, move |_| Count(Arc::clone(&t2))).unwrap();
        let mut a = lane(Duration::ZERO);
        assert!(p.wait_started(Duration::from_secs(5)));
        assert!(p.submit(0, &mut a));
        assert!(back(&p, 0, &a));
        assert!(p.submit(0, &mut a));
        assert!(back(&p, 0, &a));
        assert!(turns.load(Ordering::Relaxed) >= 2);
    }

    #[test]
    fn a_panicking_lane_is_poisoned_not_fatal() {
        let p = pool(1);
        let mut a = Lane::new(Add {
            x: 0,
            delay: Duration::ZERO,
            panic: true,
        });
        assert!(p.submit(0, &mut a));
        assert!(back(&p, 0, &a));
        assert!(a.is_poisoned());
        assert_eq!(p.stats().panics.load(Ordering::Relaxed), 1);
        // The worker survives.
        let mut b = lane(Duration::ZERO);
        assert!(p.submit(0, &mut b));
        assert!(back(&p, 0, &b));
        assert_eq!(b.get_mut().unwrap().x, 1);
        // The injected panic of the test hook poisons the same way.
        p.panic_next(0);
        assert!(p.submit(0, &mut b));
        assert!(back(&p, 0, &b));
        assert!(b.is_poisoned());
        assert_eq!(p.stats().panics.load(Ordering::Relaxed), 2);
    }
}
