//! Shared lock-free clock, PipeWire IO-area quiescence synchronization,
//! `eventfd` signaling, and click-free fade ramps.

use crate::error::{PwError, Result};
use pipewire as pw;
use pw::spa;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering, fence};

/// Lightweight non-blocking Linux `eventfd` wrapper for RT-safe wakeups.
pub(crate) struct EventFd {
    fd: OwnedFd,
}

impl EventFd {
    pub(crate) fn new() -> Result<Self> {
        // Safety: standard Linux eventfd creation with nonblock + cloexec.
        let raw = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        if raw < 0 {
            return Err(PwError::Io(std::io::Error::last_os_error()));
        }
        // Safety: `raw` is a newly created valid fd owned solely by this struct.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        Ok(Self { fd })
    }

    #[inline]
    pub(crate) fn as_raw_fd(&self) -> libc::c_int {
        self.fd.as_raw_fd()
    }

    #[inline]
    pub(crate) fn signal(&self) {
        let val: u64 = 1;
        // Safety: writing 8 bytes (`u64`) to a valid `eventfd`.
        unsafe {
            libc::write(
                self.fd.as_raw_fd(),
                (&val as *const u64).cast(),
                std::mem::size_of::<u64>(),
            );
        }
    }

    #[inline]
    pub(crate) fn drain(&self) {
        let mut val: u64 = 0;
        // Safety: reading 8 bytes (`u64`) from a non-blocking `eventfd`.
        unsafe {
            libc::read(
                self.fd.as_raw_fd(),
                (&mut val as *mut u64).cast(),
                std::mem::size_of::<u64>(),
            );
        }
    }
}

/// Synchronized PipeWire IO-area pointers (`SPA_IO_Position` / `SPA_IO_Clock`)
/// shared between PipeWire's `io_changed` / `state_changed` callbacks and the
/// single ALSA real-time reader thread.
///
/// # Lifetime & Soundness Invariant (Single-Reader SeqCst Quiescence)
/// PipeWire maps and unmaps `SPA_IO_Position` / `SPA_IO_Clock` memory areas
/// around `io_changed` and `StreamState::Unconnected` callbacks. Once
/// `io_changed` returns, PipeWire may immediately unmap or reuse the old area.
///
/// To guarantee the ALSA thread never dereferences an unmapped IO area:
/// - **Writer (PipeWire thread)**: stores the new pointer (or null) into
///   `io_position` / `io_clock` with `Ordering::SeqCst`, then calls
///   [`Self::quiesce_reader`], spinning until `reader_active` is observed
///   `false` with `Ordering::SeqCst` before returning to PipeWire.
/// - **Reader (ALSA RT thread only)**: accesses the pointers exclusively
///   inside [`Self::with_clock`] (which holds an RAII [`IoAccessGuard`] setting
///   `reader_active = true` with `Ordering::SeqCst` before loading any pointer
///   and resetting `reader_active = false` with `Ordering::SeqCst` on drop).
///
/// By `SeqCst` total ordering:
/// - If the reader stores `reader_active = true` before the writer updates the
///   pointer, the writer MUST observe `reader_active == true` and wait until
///   the reader drops the guard.
/// - If the writer updates the pointer before the reader stores
///   `reader_active = true`, the reader MUST observe the new pointer and never
///   touches the old mapping.
///
/// While holding the guard, the ALSA thread performs no syscalls, locks, or
/// waits — only ~20 ns of non-blocking loads/stores on `spa_io_clock`.
pub(crate) struct SynchronizedIo {
    io_position: AtomicPtr<spa::sys::spa_io_position>,
    io_clock: AtomicPtr<spa::sys::spa_io_clock>,
    reader_active: AtomicBool,
}

/// Generous threshold after which a stalled reader quiescence logs a single
/// warning on the non-RT PipeWire thread while continuing to wait.
const QUIESCE_SLOW_LOG_THRESHOLD_NS: u64 = 100_000_000;

impl SynchronizedIo {
    pub(crate) fn new() -> Self {
        Self {
            io_position: AtomicPtr::new(std::ptr::null_mut()),
            io_clock: AtomicPtr::new(std::ptr::null_mut()),
            reader_active: AtomicBool::new(false),
        }
    }

    /// Update `SPA_IO_Position` pointer and quiesce until the ALSA RT reader
    /// has exited any critical section referencing the previous mapping.
    pub(crate) fn store_position(&self, ptr: *mut spa::sys::spa_io_position) {
        self.io_position.store(ptr, Ordering::SeqCst);
        self.quiesce_reader();
    }

    /// Update `SPA_IO_Clock` pointer and quiesce until the ALSA RT reader
    /// has exited any critical section referencing the previous mapping.
    pub(crate) fn store_clock(&self, ptr: *mut spa::sys::spa_io_clock) {
        self.io_clock.store(ptr, Ordering::SeqCst);
        self.quiesce_reader();
    }

    /// Clear both IO pointers (e.g. on `Unconnected`) and quiesce.
    pub(crate) fn clear_all(&self) {
        self.io_position
            .store(std::ptr::null_mut(), Ordering::SeqCst);
        self.io_clock.store(std::ptr::null_mut(), Ordering::SeqCst);
        self.quiesce_reader();
    }

    /// Block unconditionally until `reader_active` is observed `false`.
    ///
    /// # Why unbounded wait instead of copying-only or a bounded timeout
    /// [`DriverClockState::advance_and_trigger`] must write `nsec`, `rate`,
    /// `position`, `duration`, `delay`, `rate_diff`, and `next_nsec` directly
    /// into PipeWire's `spa_io_clock` synchronously right before
    /// `pw_stream_trigger_process`, so reading a PipeWire-loop-copied struct
    /// alone would not satisfy driver-clock publication. Meanwhile, the ALSA RT
    /// reader holds [`IoAccessGuard`] only around lock-free, syscall-free memory
    /// loads/stores (~20 ns, never across `pw_stream_trigger_process`). A bounded
    /// timeout cannot protect pointer lifetime because returning early to
    /// PipeWire allows PipeWire to immediately unmap the old IO area while the
    /// reader is inside `with_clock`, causing use-after-unmap UB. Therefore this
    /// wait must never return while `reader_active == true`; it spins briefly,
    /// backs off via `yield_now` -> short sleep, and logs once if it exceeds
    /// 100 ms.
    #[inline]
    fn quiesce_reader(&self) {
        self.quiesce_reader_with_warn_threshold(QUIESCE_SLOW_LOG_THRESHOLD_NS);
    }

    fn quiesce_reader_with_warn_threshold(&self, warn_threshold_ns: u64) {
        let mut spins = 0u32;
        while self.reader_active.load(Ordering::SeqCst) {
            if spins < 1024 {
                spins += 1;
                std::hint::spin_loop();
                continue;
            }
            let start_ns = mono_now_ns();
            let mut yields = 0u32;
            let mut warned = false;
            while self.reader_active.load(Ordering::SeqCst) {
                let elapsed_ns = mono_now_ns().saturating_sub(start_ns);
                if !warned && elapsed_ns >= warn_threshold_ns {
                    log::warn!(
                        "SynchronizedIo: reader quiescence exceeded {} ms; continuing to wait to preserve PipeWire IO mapping lifetime",
                        warn_threshold_ns / 1_000_000
                    );
                    warned = true;
                }
                if yields < 256 {
                    yields += 1;
                    std::thread::yield_now();
                } else {
                    std::thread::sleep(std::time::Duration::from_micros(50));
                }
            }
            return;
        }
    }

    /// Execute `f` with a pointer to the current `spa_io_clock` (if mapped),
    /// guarded by [`IoAccessGuard`] so PipeWire cannot unmap the memory while
    /// `f` runs.
    #[inline]
    pub(crate) fn with_clock<R>(
        &self,
        f: impl FnOnce(Option<*mut spa::sys::spa_io_clock>) -> R,
    ) -> R {
        let _guard = IoAccessGuard::enter(&self.reader_active);
        let pos = self.io_position.load(Ordering::SeqCst);
        let clk = if !pos.is_null() {
            // Safety: `clock` is the first field of `spa_io_position`, and
            // `_guard` prevents `io_changed` from returning while we use it.
            unsafe { std::ptr::addr_of_mut!((*pos).clock) }
        } else {
            self.io_clock.load(Ordering::SeqCst)
        };
        if clk.is_null() { f(None) } else { f(Some(clk)) }
    }
}

struct IoAccessGuard<'a> {
    active: &'a AtomicBool,
}

impl<'a> IoAccessGuard<'a> {
    #[inline]
    fn enter(active: &'a AtomicBool) -> Self {
        active.store(true, Ordering::SeqCst);
        Self { active }
    }
}

impl Drop for IoAccessGuard<'_> {
    #[inline]
    fn drop(&mut self) {
        self.active.store(false, Ordering::SeqCst);
    }
}

/// Fixed-capacity RT-owned overflow queue for `rtrb::Producer<T>` so non-Copy
/// heap handles (`Box<AlsaOutput>`, `Vec`, ring consumers) returned from the
/// audio thread are never dropped on the RT thread if `rtrb` is momentarily full.
pub(crate) struct BoundedEventStash<T, const N: usize> {
    slots: [Option<T>; N],
    len: usize,
}

impl<T, const N: usize> BoundedEventStash<T, N> {
    pub(crate) fn new() -> Self {
        Self {
            slots: std::array::from_fn(|_| None),
            len: 0,
        }
    }

    #[inline]
    pub(crate) fn has_pending(&self) -> bool {
        self.len > 0
    }

    pub(crate) fn flush(&mut self, tx: &mut rtrb::Producer<T>, notify_efd: &EventFd) -> bool {
        let mut pushed_any = false;
        let mut idx = 0;
        while idx < self.len {
            let Some(item) = self.slots[idx].take() else {
                idx += 1;
                continue;
            };
            match tx.push(item) {
                Ok(()) => {
                    pushed_any = true;
                    idx += 1;
                }
                Err(rtrb::PushError::Full(returned)) => {
                    self.slots[idx] = Some(returned);
                    break;
                }
            }
        }
        if idx > 0 {
            let rem = self.len - idx;
            for dst in 0..rem {
                self.slots[dst] = self.slots[dst + idx].take();
            }
            self.len = rem;
        }
        if pushed_any {
            notify_efd.signal();
        }
        pushed_any
    }

    pub(crate) fn push_or_stash(
        &mut self,
        ev: T,
        tx: &mut rtrb::Producer<T>,
        notify_efd: &EventFd,
    ) -> std::result::Result<(), T> {
        self.flush(tx, notify_efd);
        if self.len == 0 {
            match tx.push(ev) {
                Ok(()) => {
                    notify_efd.signal();
                    return Ok(());
                }
                Err(rtrb::PushError::Full(returned)) => {
                    if self.len < N {
                        self.slots[self.len] = Some(returned);
                        self.len += 1;
                        notify_efd.signal();
                        return Ok(());
                    }
                    return Err(returned);
                }
            }
        }
        if self.len < N {
            self.slots[self.len] = Some(ev);
            self.len += 1;
            notify_efd.signal();
            Ok(())
        } else {
            Err(ev)
        }
    }
}

#[inline]
pub(crate) fn mono_now_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // Safety: CLOCK_MONOTONIC is always valid on Linux.
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    (ts.tv_sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(ts.tv_nsec as u64)
}

/// Read `target_rate` and `target_duration` from PipeWire's `spa_io_clock`
/// using its seqlock (`target_seq`) inside [`SynchronizedIo::with_clock`].
#[inline]
pub(crate) fn read_target_clock(
    io: &SynchronizedIo,
    default_rate: u32,
    default_duration: u64,
) -> (u32, u64) {
    io.with_clock(|clk_opt| {
        let Some(clk) = clk_opt else {
            return (default_rate, default_duration);
        };
        for _ in 0..8 {
            // Safety: `clk` is guarded by `SynchronizedIo::with_clock`.
            unsafe {
                let s1 = std::ptr::read_volatile(&(*clk).target_seq);
                fence(Ordering::Acquire);
                if (s1 & 1) != 0 {
                    std::hint::spin_loop();
                    continue;
                }
                let rate = (*clk).target_rate;
                let dur = (*clk).target_duration;
                fence(Ordering::Acquire);
                let s2 = std::ptr::read_volatile(&(*clk).target_seq);
                if s1 == s2 {
                    let r = if rate.denom != 0 {
                        rate.denom
                    } else {
                        default_rate
                    };
                    let d = if dur != 0 { dur } else { default_duration };
                    return (r, d);
                }
            }
        }
        (default_rate, default_duration)
    })
}

/// Monotonic PipeWire driver clock state owned by the ALSA RT thread.
pub(crate) struct DriverClockState {
    pub(crate) position: u64,
    pub(crate) prev_duration: u64,
    pub(crate) next_time_ns: u64,
}

impl DriverClockState {
    pub(crate) fn new() -> Self {
        Self {
            position: 0,
            prev_duration: 0,
            next_time_ns: 0,
        }
    }

    pub(crate) fn reset(&mut self) {
        self.position = 0;
        self.prev_duration = 0;
        self.next_time_ns = mono_now_ns();
    }

    pub(crate) fn advance_and_trigger(
        &mut self,
        io: &SynchronizedIo,
        raw_stream: *mut pw::sys::pw_stream,
        rate: u32,
        duration: u64,
        delay_graph_frames: i64,
    ) {
        let now = mono_now_ns();
        let nominal_ns = duration
            .saturating_mul(1_000_000_000)
            .checked_div(rate as u64)
            .unwrap_or(5_333_333);

        if self.next_time_ns == 0 || now.abs_diff(self.next_time_ns) > nominal_ns.saturating_mul(2)
        {
            self.next_time_ns = now;
        }
        let this_time = self.next_time_ns;
        self.next_time_ns = this_time.saturating_add(nominal_ns);

        self.position = self.position.saturating_add(self.prev_duration);
        self.prev_duration = duration;

        io.with_clock(|clk_opt| {
            if let Some(clk) = clk_opt {
                // Safety: driver owns writing `nsec`, `rate`, `position`, `duration`,
                // `delay`, `rate_diff`, `next_nsec` before calling `pw_stream_trigger_process`,
                // and `SynchronizedIo::with_clock` ensures `clk` stays mapped.
                unsafe {
                    (*clk).nsec = this_time;
                    (*clk).rate = spa::sys::spa_fraction {
                        num: 1,
                        denom: rate,
                    };
                    (*clk).position = self.position;
                    (*clk).duration = duration;
                    (*clk).delay = delay_graph_frames;
                    (*clk).rate_diff = 1.0;
                    (*clk).next_nsec = self.next_time_ns;
                }
            }
        });

        // Safety: caller ensures `raw_stream` remains valid until ALSA thread joins,
        // and the `IoAccessGuard` has already been released before calling into PipeWire.
        if !raw_stream.is_null() {
            unsafe {
                pw::sys::pw_stream_trigger_process(raw_stream);
            }
        }
    }
}

/// Allocation-free raised-cosine mute/fade ramp applied to interleaved `i32`
/// output frames around device switches.
///
/// When at steady unity (`gain == 1.0`, not ramping), [`Self::apply_interleaved`]
/// is an instant no-op so bit-perfect passthrough is untouched.
#[derive(Clone, Debug)]
pub(crate) struct FadeRamp {
    gain: f64,
    from: f64,
    to: f64,
    pos: usize,
    len: usize,
}

impl FadeRamp {
    pub(crate) fn unity() -> Self {
        Self {
            gain: 1.0,
            from: 1.0,
            to: 1.0,
            pos: 0,
            len: 0,
        }
    }

    pub(crate) fn start_fade_out(&mut self, frames: usize) {
        if frames == 0 {
            self.gain = 0.0;
            self.from = 0.0;
            self.to = 0.0;
            self.pos = 0;
            self.len = 0;
            return;
        }
        self.from = self.gain;
        self.to = 0.0;
        self.pos = 0;
        self.len = frames;
    }

    pub(crate) fn start_fade_in(&mut self, frames: usize) {
        if frames == 0 {
            self.gain = 1.0;
            self.from = 1.0;
            self.to = 1.0;
            self.pos = 0;
            self.len = 0;
            return;
        }
        self.gain = 0.0;
        self.from = 0.0;
        self.to = 1.0;
        self.pos = 0;
        self.len = frames;
    }

    #[inline]
    pub(crate) fn is_ramping(&self) -> bool {
        self.pos < self.len
    }

    #[inline]
    pub(crate) fn is_fade_out_done(&self) -> bool {
        self.to == 0.0 && self.pos >= self.len && self.gain == 0.0
    }

    #[inline]
    pub(crate) fn apply_interleaved(&mut self, samples: &mut [i32], channels: usize) {
        if channels == 0 || samples.is_empty() {
            return;
        }
        if !self.is_ramping() {
            if (self.gain - 1.0).abs() < f64::EPSILON {
                return;
            }
            if self.gain == 0.0 {
                samples.fill(0);
                return;
            }
            for s in samples.iter_mut() {
                *s = ((*s as f64) * self.gain)
                    .round()
                    .clamp(i32::MIN as f64, i32::MAX as f64) as i32;
            }
            return;
        }

        let frames = samples.len() / channels;
        for f in 0..frames {
            let g = self.next_frame_gain();
            let base = f * channels;
            for ch in 0..channels {
                let s = samples[base + ch];
                samples[base + ch] = ((s as f64) * g)
                    .round()
                    .clamp(i32::MIN as f64, i32::MAX as f64)
                    as i32;
            }
        }
    }

    #[inline]
    fn next_frame_gain(&mut self) -> f64 {
        if self.pos < self.len {
            self.pos += 1;
            if self.pos >= self.len {
                self.gain = self.to;
            } else {
                let t = (self.pos as f64) / (self.len as f64);
                let shape = 0.5 - 0.5 * (std::f64::consts::PI * t).cos();
                self.gain = self.from + (self.to - self.from) * shape;
            }
        }
        self.gain
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;
    use std::time::Duration;

    #[test]
    fn synchronized_io_quiesces_before_freeing_old_mapping() {
        // Hammer writer pointer swaps + Box::drop concurrently with reader
        // dereferences; any UAF or race would trigger UB/panic/mismatch.
        let io = Arc::new(SynchronizedIo::new());
        let initial = Box::into_raw(Box::new(unsafe {
            std::mem::zeroed::<spa::sys::spa_io_clock>()
        }));
        unsafe {
            (*initial).target_seq = 2;
            (*initial).target_rate = spa::sys::spa_fraction {
                num: 1,
                denom: 48_000,
            };
            (*initial).target_duration = 1024;
        }
        io.store_clock(initial);

        let stop = Arc::new(AtomicBool::new(false));
        let reads = Arc::new(AtomicU64::new(0));

        let io_r = Arc::clone(&io);
        let stop_r = Arc::clone(&stop);
        let reads_r = Arc::clone(&reads);
        let reader = std::thread::spawn(move || {
            while !stop_r.load(Ordering::Relaxed) {
                let (r, d) = read_target_clock(&io_r, 44_100, 512);
                assert!(r == 48_000 || r == 96_000 || r == 44_100);
                assert!(d == 1024 || d == 2048 || d == 512);
                reads_r.fetch_add(1, Ordering::Relaxed);
            }
        });

        let mut current = initial;
        for iter in 0..20_000u64 {
            let next = Box::into_raw(Box::new(unsafe {
                std::mem::zeroed::<spa::sys::spa_io_clock>()
            }));
            unsafe {
                (*next).target_seq = 2;
                (*next).target_rate = spa::sys::spa_fraction {
                    num: 1,
                    denom: if iter % 2 == 0 { 96_000 } else { 48_000 },
                };
                (*next).target_duration = if iter % 2 == 0 { 2048 } else { 1024 };
            }
            // Publish new pointer and wait for reader quiescence.
            io.store_clock(next);
            // Safe to free old pointer now that `store_clock` returned.
            unsafe {
                drop(Box::from_raw(current));
            }
            current = next;
        }

        stop.store(true, Ordering::Release);
        reader.join().unwrap();
        io.clear_all();
        unsafe {
            drop(Box::from_raw(current));
        }
        assert!(reads.load(Ordering::Relaxed) > 0);
    }

    #[test]
    fn fade_ramp_unity_is_bit_exact_and_fade_out_reaches_zero() {
        let mut ramp = FadeRamp::unity();
        let orig = [123_456_789_i32, -987_654_321_i32, i32::MAX, i32::MIN];
        let mut buf = orig;
        ramp.apply_interleaved(&mut buf, 2);
        assert_eq!(buf, orig);

        ramp.start_fade_out(10);
        let mut out = vec![1_000_000_i32; 20];
        ramp.apply_interleaved(&mut out, 2);
        assert!(ramp.is_fade_out_done());
        assert_eq!(out[18], 0);
        assert_eq!(out[19], 0);

        ramp.start_fade_in(10);
        let mut inp = vec![1_000_000_i32; 20];
        ramp.apply_interleaved(&mut inp, 2);
        assert!(!ramp.is_ramping());
        assert_eq!(inp[18], 1_000_000);
        assert_eq!(inp[19], 1_000_000);
    }

    #[test]
    fn synchronized_io_quiesce_waits_until_reader_releases_past_warn_threshold() {
        let io = Arc::new(SynchronizedIo::new());
        io.reader_active.store(true, Ordering::SeqCst);

        let finished = Arc::new(AtomicBool::new(false));
        let io_w = Arc::clone(&io);
        let finished_w = Arc::clone(&finished);
        let writer = std::thread::spawn(move || {
            io_w.store_clock(std::ptr::null_mut());
            finished_w.store(true, Ordering::SeqCst);
        });

        // Sleep longer than QUIESCE_SLOW_LOG_THRESHOLD_NS (100 ms): writer must
        // NOT return early and unprotect the old mapping.
        std::thread::sleep(Duration::from_millis(130));
        assert!(
            !finished.load(Ordering::SeqCst),
            "quiescence must wait unconditionally while reader is active"
        );

        io.reader_active.store(false, Ordering::SeqCst);
        writer.join().unwrap();
        assert!(finished.load(Ordering::SeqCst));
    }

    #[test]
    fn synchronized_io_stress_poison_and_free_under_preemption() {
        const POISON_FLAG: u32 = 0xDEAD_DEAD;
        let io = Arc::new(SynchronizedIo::new());

        let alloc_clock = |gen_idx: u32| -> *mut spa::sys::spa_io_clock {
            let p = Box::into_raw(Box::new(unsafe {
                std::mem::zeroed::<spa::sys::spa_io_clock>()
            }));
            unsafe {
                (*p).flags = gen_idx;
                (*p).target_seq = gen_idx.wrapping_mul(2);
                (*p).target_rate = spa::sys::spa_fraction {
                    num: 1,
                    denom: 48_000 + (gen_idx % 4),
                };
                (*p).target_duration = 1024;
            }
            p
        };

        let alloc_pos = |gen_idx: u32| -> *mut spa::sys::spa_io_position {
            let p = Box::into_raw(Box::new(unsafe {
                std::mem::zeroed::<spa::sys::spa_io_position>()
            }));
            unsafe {
                (*p).clock.flags = gen_idx;
                (*p).clock.target_seq = gen_idx.wrapping_mul(2);
                (*p).clock.target_rate = spa::sys::spa_fraction {
                    num: 1,
                    denom: 48_000 + (gen_idx % 4),
                };
                (*p).clock.target_duration = 1024;
            }
            p
        };

        let init_clk = alloc_clock(1);
        io.store_clock(init_clk);

        let stop = Arc::new(AtomicBool::new(false));
        let reads = Arc::new(AtomicU64::new(0));

        let io_r = Arc::clone(&io);
        let stop_r = Arc::clone(&stop);
        let reads_r = Arc::clone(&reads);
        let reader = std::thread::spawn(move || {
            let mut count = 0u64;
            while !stop_r.load(Ordering::Relaxed) {
                io_r.with_clock(|clk_opt| {
                    if let Some(clk) = clk_opt {
                        unsafe {
                            let f1 = std::ptr::read_volatile(&(*clk).flags);
                            assert_ne!(f1, POISON_FLAG, "reader observed poisoned/freed clock");
                            // Simulate occasional scheduler preemption inside the critical section.
                            if count.is_multiple_of(17) {
                                std::thread::yield_now();
                            }
                            let seq = std::ptr::read_volatile(&(*clk).target_seq);
                            let rate = std::ptr::read_volatile(&(*clk).target_rate);
                            let f2 = std::ptr::read_volatile(&(*clk).flags);
                            assert_ne!(
                                f2, POISON_FLAG,
                                "reader observed poisoned clock after preemption"
                            );
                            assert_eq!(f1, f2, "clock generation mutated in place while guarded");
                            assert_eq!(seq, f1.wrapping_mul(2));
                            assert_eq!(rate.denom, 48_000 + (f1 % 4));
                        }
                    }
                });
                count += 1;
            }
            reads_r.store(count, Ordering::Relaxed);
        });

        let mut cur_clk: Option<*mut spa::sys::spa_io_clock> = Some(init_clk);
        let mut cur_pos: Option<*mut spa::sys::spa_io_position> = None;

        for iter in 2..15_002u32 {
            match iter % 3 {
                0 => {
                    let next_pos = alloc_pos(iter);
                    io.store_position(next_pos);
                    if let Some(old_pos) = cur_pos.take() {
                        unsafe {
                            (*old_pos).clock.flags = POISON_FLAG;
                            (*old_pos).clock.target_seq = u32::MAX;
                            (*old_pos).clock.target_rate.denom = 0;
                            drop(Box::from_raw(old_pos));
                        }
                    }
                    cur_pos = Some(next_pos);
                }
                1 => {
                    let next_clk = alloc_clock(iter);
                    io.store_clock(next_clk);
                    if let Some(old_clk) = cur_clk.take() {
                        unsafe {
                            (*old_clk).flags = POISON_FLAG;
                            (*old_clk).target_seq = u32::MAX;
                            (*old_clk).target_rate.denom = 0;
                            drop(Box::from_raw(old_clk));
                        }
                    }
                    cur_clk = Some(next_clk);
                }
                _ => {
                    io.clear_all();
                    if let Some(old_pos) = cur_pos.take() {
                        unsafe {
                            (*old_pos).clock.flags = POISON_FLAG;
                            (*old_pos).clock.target_seq = u32::MAX;
                            (*old_pos).clock.target_rate.denom = 0;
                            drop(Box::from_raw(old_pos));
                        }
                    }
                    if let Some(old_clk) = cur_clk.take() {
                        unsafe {
                            (*old_clk).flags = POISON_FLAG;
                            (*old_clk).target_seq = u32::MAX;
                            (*old_clk).target_rate.denom = 0;
                            drop(Box::from_raw(old_clk));
                        }
                    }
                }
            }
        }

        stop.store(true, Ordering::Release);
        reader.join().unwrap();
        io.clear_all();
        if let Some(old_pos) = cur_pos.take() {
            unsafe { drop(Box::from_raw(old_pos)) };
        }
        if let Some(old_clk) = cur_clk.take() {
            unsafe { drop(Box::from_raw(old_clk)) };
        }
        assert!(reads.load(Ordering::Relaxed) > 0);
    }
}
