//! The mouse wheel over the tray icon. Tray hosts forward the wheel as
//! `Scroll(delta, orientation)`, 120 units per notch, but they disagree on
//! the sign: Plasma sends a positive delta for wheel-up (the reference),
//! while GNOME's AppIndicator extension forwards Mutter's convention, where
//! positive means down. Touchpads and smooth wheels send fractions of a
//! notch. Deltas are accumulated and one volume step is taken per full
//! 120 units, louder for wheel-up on every host (or reversed by the user).

use std::time::{Duration, Instant};

/// One wheel notch.
pub const NOTCH: i32 = 120;
/// A remainder older than this is forgotten (a new gesture starts clean).
pub const IDLE_RESET: Duration = Duration::from_millis(800);
/// At most this many steps from one event (a runaway delta).
pub const MAX_STEPS_PER_EVENT: i32 = 10;

/// Which way a positive delta goes for this tray host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostSign {
    /// Positive is wheel-up (Plasma and most StatusNotifier hosts).
    PositiveUp,
    /// Positive is wheel-down (GNOME Shell's AppIndicator extension).
    PositiveDown,
}

/// The sign for a tray host, from the process owning the
/// StatusNotifierWatcher (`/proc/<pid>/comm`) when known, else from
/// `XDG_CURRENT_DESKTOP`. Plasma's convention is the default.
pub fn host_sign(watcher_process: Option<&str>, current_desktop: Option<&str>) -> HostSign {
    if let Some(p) = watcher_process.map(str::to_ascii_lowercase) {
        if p.starts_with("gnome-shell") {
            return HostSign::PositiveDown;
        }
        if p.starts_with("plasmashell") || p.starts_with("kded") {
            return HostSign::PositiveUp;
        }
    }
    let gnome = current_desktop.is_some_and(|d| {
        d.split(':').any(|x| {
            matches!(
                x.to_ascii_lowercase().as_str(),
                "gnome" | "ubuntu" | "unity"
            )
        })
    });
    // Another watcher process on a GNOME session (an extension's helper)
    // still hands the deltas through GNOME Shell.
    if gnome {
        HostSign::PositiveDown
    } else {
        HostSign::PositiveUp
    }
}

/// Adds up wheel deltas; whole notches come out as volume steps.
#[derive(Debug, Default)]
pub struct WheelAccumulator {
    /// Units towards louder (positive) or quieter.
    acc: i32,
    last: Option<Instant>,
}

impl WheelAccumulator {
    /// A delta from the host: the steps to take now, positive = louder.
    pub fn push(&mut self, delta: i32, sign: HostSign, reverse: bool, now: Instant) -> i32 {
        if self
            .last
            .is_some_and(|t| now.duration_since(t) > IDLE_RESET)
        {
            self.acc = 0;
        }
        self.last = Some(now);
        let mut d = delta.clamp(-NOTCH * MAX_STEPS_PER_EVENT, NOTCH * MAX_STEPS_PER_EVENT);
        if sign == HostSign::PositiveDown {
            d = -d;
        }
        if reverse {
            d = -d;
        }
        // A turn the other way drops what was gathered the old way.
        if (self.acc > 0 && d < 0) || (self.acc < 0 && d > 0) {
            self.acc = 0;
        }
        self.acc += d;
        let steps = (self.acc / NOTCH).clamp(-MAX_STEPS_PER_EVENT, MAX_STEPS_PER_EVENT);
        self.acc -= steps * NOTCH;
        steps
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_huge_delta_is_bounded() {
        let mut w = WheelAccumulator::default();
        assert_eq!(
            w.push(i32::MAX, HostSign::PositiveUp, false, Instant::now()),
            MAX_STEPS_PER_EVENT
        );
    }
}
