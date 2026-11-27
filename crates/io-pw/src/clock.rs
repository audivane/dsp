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
/// PipeWire maps and unmaps those areas around `io_changed` and
/// `StreamState::Unconnected`, and may reuse the old area as soon as
/// `io_changed` returns. So, all `SeqCst`: the writer (PipeWire thread)
/// stores the new pointer (or null), then calls [`Self::quiesce_reader`] and
/// waits until `reader_active` is `false`; the reader (the ALSA RT thread
/// only) touches the pointers solely inside [`Self::with_clock`], whose
/// [`IoAccessGuard`] sets `reader_active = true` before loading one and
/// clears it on drop. Either the writer sees the flag and waits, or the
/// reader sees the new pointer and never touches the old mapping.
///
/// While holding the guard, the ALSA thread performs no syscalls, locks or
/// waits: only ~20 ns of non-blocking loads/stores on `spa_io_clock`.
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

    /// Block unconditionally until `reader_active` is observed `false`: it
    /// must never return while the reader is inside `with_clock`, or
    /// PipeWire could unmap the old IO area under it (a bounded timeout is
    /// unsafe for that reason). [`DriverClockState::advance_and_trigger`]
    /// writes the driver clock in place right before
    /// `pw_stream_trigger_process`, so copying the struct would not do. The
    /// reader holds its guard only around lock-free, syscall-free
    /// loads/stores (~20 ns), so the wait spins briefly, then yields and
    /// sleeps, and logs once if it exceeds 100 ms.
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

/// `CLOCK_MONOTONIC_RAW`, ns: the clock ALSA's status timestamps use
/// (`io-alsa` sets `TstampType::MonotonicRaw`), so DAC readings from every
/// device share one domain (multi-device phase comparisons).
#[inline]
pub fn mono_raw_now_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // Safety: CLOCK_MONOTONIC_RAW is always valid on Linux.
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC_RAW, &mut ts);
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
        self.advance_and_trigger_at(io, raw_stream, rate, duration, delay_graph_frames, 1.0);
    }

    /// [`Self::advance_and_trigger`] for cycles scheduled at `speed` times
    /// the graph rate (varispeed): `next_nsec` is one cycle interval
    /// `Q / (speed · G)` ahead, and the resync threshold is measured in the
    /// same interval. `position` still advances by `Q`, `rate` stays
    /// `1/G` and `rate_diff` stays 1.0.
    pub(crate) fn advance_and_trigger_at(
        &mut self,
        io: &SynchronizedIo,
        raw_stream: *mut pw::sys::pw_stream,
        rate: u32,
        duration: u64,
        delay_graph_frames: i64,
        speed: f64,
    ) {
        let now = mono_now_ns();
        let nominal_ns = cycle_interval_ns(duration, rate, speed);

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

/// One graph cycle's interval, ns: `duration / (speed · rate)`, or a
/// 5.33 ms default for a zero rate. `speed` 1 (or not a usable speed) gives
/// the plain integer interval.
pub(crate) fn cycle_interval_ns(duration: u64, rate: u32, speed: f64) -> u64 {
    let base = duration
        .saturating_mul(1_000_000_000)
        .checked_div(rate as u64)
        .unwrap_or(5_333_333);
    if speed == 1.0 || !speed.is_finite() || speed <= 0.0 {
        return base;
    }
    let ns = duration as f64 * 1e9 / (f64::from(rate.max(1)) * speed);
    (ns as u64).max(1)
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
}
