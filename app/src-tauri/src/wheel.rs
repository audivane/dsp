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
    fn sign_table() {
        use HostSign::*;
        // The watcher's owner decides first.
        assert_eq!(host_sign(Some("plasmashell"), Some("GNOME")), PositiveUp);
        assert_eq!(host_sign(Some("kded6"), None), PositiveUp);
        assert_eq!(host_sign(Some("gnome-shell"), Some("KDE")), PositiveDown);
        // Then the desktop.
        assert_eq!(host_sign(None, Some("KDE")), PositiveUp);
        assert_eq!(host_sign(None, Some("ubuntu:GNOME")), PositiveDown);
        assert_eq!(host_sign(Some("snixembed"), Some("GNOME")), PositiveDown);
        assert_eq!(host_sign(None, Some("XFCE")), PositiveUp);
        assert_eq!(host_sign(Some("waybar"), Some("sway")), PositiveUp);
        assert_eq!(host_sign(None, None), PositiveUp);
    }

    #[test]
    fn one_step_per_notch_and_fractions_add_up() {
        let t = Instant::now();
        let mut w = WheelAccumulator::default();
        assert_eq!(w.push(120, HostSign::PositiveUp, false, t), 1);
        assert_eq!(w.push(240, HostSign::PositiveUp, false, t), 2);
        // A touchpad: 30-unit deltas make one step per four.
        let steps: Vec<i32> = (0..8)
            .map(|_| w.push(30, HostSign::PositiveUp, false, t))
            .collect();
        assert_eq!(steps, vec![0, 0, 0, 1, 0, 0, 0, 1]);
        assert_eq!(w.push(-120, HostSign::PositiveUp, false, t), -1);
    }

    #[test]
    fn gnome_and_reverse_flip_the_direction() {
        let t = Instant::now();
        let mut w = WheelAccumulator::default();
        // GNOME: positive is down, so quieter.
        assert_eq!(w.push(120, HostSign::PositiveDown, false, t), -1);
        assert_eq!(w.push(120, HostSign::PositiveUp, true, t), -1);
        assert_eq!(w.push(120, HostSign::PositiveDown, true, t), 1);
    }

    #[test]
    fn a_remainder_is_dropped_on_reversal_and_after_a_pause() {
        let t = Instant::now();
        let mut w = WheelAccumulator::default();
        assert_eq!(w.push(100, HostSign::PositiveUp, false, t), 0);
        // Turned back: the 100 units up do not cancel a full notch down.
        assert_eq!(w.push(-120, HostSign::PositiveUp, false, t), -1);
        assert_eq!(w.push(100, HostSign::PositiveUp, false, t), 0);
        // After a pause the old remainder does not complete a new notch.
        let later = t + IDLE_RESET + Duration::from_millis(1);
        assert_eq!(w.push(20, HostSign::PositiveUp, false, later), 0);
        assert_eq!(w.push(100, HostSign::PositiveUp, false, later), 1);
    }

    #[test]
    fn a_huge_delta_is_bounded() {
        let mut w = WheelAccumulator::default();
        assert_eq!(
            w.push(i32::MAX, HostSign::PositiveUp, false, Instant::now()),
            MAX_STEPS_PER_EVENT
        );
    }
}
