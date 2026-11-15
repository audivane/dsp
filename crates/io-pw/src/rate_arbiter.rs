//! Which rate the graph runs at while apps play.
//!
//! Pure decision logic, kept apart from the PipeWire plumbing in
//! `engine_driver` so it can be unit-tested with synthetic clients and clocks.
//!
//! Rules (see `docs/architecture.md`, "Sample rate and clock"):
//!
//! - Anything playing into the Direct (bit-perfect) sink decides on its own.
//! - Otherwise the running clients of the DSP sink decide. When they agree
//!   (after mapping through [`choose_allowed_rate`]) the graph follows them.
//!   When they disagree, [`RateConflict`] picks: keep the current rate while a
//!   client still plays at it, else follow the client that started first
//!   (default); or the highest rate; or `default_rate`.
//! - A switch is never made within [`MIN_DWELL`] of the previous one; a switch
//!   still wanted when the dwell ends is made then (the caller re-checks at
//!   the returned instant).
//! - Clients pausing and resuming because of our own switch keep their place
//!   in the start order and cannot start another switch.

use audivane_protocol::ipc::{RateConflict, RatePolicy};
use std::time::{Duration, Instant};

/// Minimum time between two rate switches.
pub const MIN_DWELL: Duration = Duration::from_secs(2);
/// After a switch, client pause/resume events are treated as caused by it for
/// at least this long.
pub const FLAP_GRACE: Duration = Duration::from_millis(1500);
/// While the driver still reports `Switching`, the flap window stays open, but
/// never longer than this after our switch.
pub const FLAP_MAX: Duration = Duration::from_secs(6);
/// How soon to look again while the driver is still switching.
const SWITCHING_POLL: Duration = Duration::from_millis(100);

/// The allowed graph rate for a client's native rate: the rate itself when
/// allowed, else the next higher allowed rate, else the closest one.
pub fn choose_allowed_rate(native: u32, policy: &RatePolicy) -> u32 {
    if policy.allowed_rates.is_empty() {
        return native;
    }
    if policy.allowed_rates.contains(&native) {
        return native;
    }
    let mut above: Option<u32> = None;
    let mut best_closest = policy.default_rate;
    let mut best_diff = u32::MAX;
    for &r in &policy.allowed_rates {
        if r >= native {
            above = Some(above.map_or(r, |cur| cur.min(r)));
        }
        let d = r.abs_diff(native);
        if d < best_diff {
            best_diff = d;
            best_closest = r;
        }
    }
    above.unwrap_or(best_closest)
}

/// A running client with a known native rate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RateClient {
    pub id: u32,
    pub name: String,
    pub native_rate: u32,
    /// Start-order sequence number (lower = started earlier).
    pub since: u64,
}

pub struct RateInputs<'a> {
    pub policy: &'a RatePolicy,
    /// The rate the graph is forced to now.
    pub current: u32,
    /// Something is linked to or playing into the Direct sink: it decides.
    pub direct_active: bool,
    /// Running clients of the Direct sink.
    pub direct: &'a [RateClient],
    /// Running clients of the DSP sink.
    pub dsp: &'a [RateClient],
    /// Rates the Direct sink can run at (the device's native rates among the
    /// effective ones, see `rate_caps::direct_rate_policy`). `None`, or an
    /// empty list, means the same as `policy`.
    pub direct_policy: Option<&'a RatePolicy>,
}

/// Why a rate was chosen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reason {
    /// Rate following is off: always `default_rate`.
    FollowOff,
    /// One client (or several that agree) plays at this rate.
    Playing {
        name: String,
        native: u32,
        others: usize,
    },
    /// Clients disagree; this one still plays at the current rate.
    StillPlaying { name: String, native: u32 },
    /// Clients disagree and none plays at the current rate: the one that
    /// started first wins.
    FirstStarted { name: String, native: u32 },
    /// Clients disagree; `highest` mode picked this one.
    Highest { name: String, native: u32 },
    /// Clients disagree; `default_rate` mode.
    DefaultOnConflict { clients: usize },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choice {
    pub rate: u32,
    /// Native rate of the client being followed (None for policy defaults).
    pub native: Option<u32>,
    /// Decided by the Direct sink.
    pub direct: bool,
    pub reason: Reason,
}

impl Choice {
    fn describe(&self) -> String {
        let sink = if self.direct { " (Direct sink)" } else { "" };
        match &self.reason {
            Reason::FollowOff => "rate following is off".to_string(),
            Reason::Playing {
                name,
                native,
                others: 0,
            } => {
                format!("{name} plays at {native}{sink}")
            }
            Reason::Playing {
                name,
                native,
                others,
            } => {
                format!("{name} and {others} more play at {native}{sink}")
            }
            Reason::StillPlaying { name, native } => {
                format!("{name} still playing at {native}{sink}; others are resampled")
            }
            Reason::FirstStarted { name, native } => {
                format!("{name} started first, plays at {native}{sink}; others are resampled")
            }
            Reason::Highest { name, native } => {
                format!("{name} has the highest rate, {native}; others are resampled")
            }
            Reason::DefaultOnConflict { clients } => {
                format!("{clients} apps play at different rates; using the default rate")
            }
        }
    }
}

/// The rate the clients ask for, ignoring timing. `None`: nobody has an
/// opinion, leave the rate alone.
pub fn choose_rate(inp: &RateInputs<'_>) -> Option<Choice> {
    let policy = inp.policy;
    // Direct never upsamples: it only runs the graph at rates the device
    // plays natively.
    let direct_policy = inp
        .direct_policy
        .filter(|p| !p.allowed_rates.is_empty())
        .unwrap_or(policy);
    if !policy.follow_source {
        if inp.direct_active && !direct_policy.allowed_rates.contains(&policy.default_rate) {
            return Some(Choice {
                rate: direct_policy.default_rate,
                native: None,
                direct: true,
                reason: Reason::FollowOff,
            });
        }
        return Some(Choice {
            rate: policy.default_rate,
            native: None,
            direct: false,
            reason: Reason::FollowOff,
        });
    }
    if inp.direct_active {
        // Bit-perfect sink: sticky + first come, first served; never mixed
        // with DSP-sink clients.
        return pick(
            inp.direct,
            inp.current,
            direct_policy,
            RateConflict::KeepCurrent,
        )
        .map(|mut c| {
            c.direct = true;
            c
        });
    }
    pick(inp.dsp, inp.current, policy, policy.conflict)
}

fn pick(
    clients: &[RateClient],
    current: u32,
    policy: &RatePolicy,
    mode: RateConflict,
) -> Option<Choice> {
    let mut sorted: Vec<&RateClient> = clients.iter().collect();
    sorted.sort_by_key(|c| (c.since, c.id));
    let first = *sorted.first()?;
    let target = |c: &RateClient| choose_allowed_rate(c.native_rate, policy);
    let first_target = target(first);
    if sorted.iter().all(|c| target(c) == first_target) {
        // Prefer naming a client that plays at exactly the chosen rate.
        let named = sorted
            .iter()
            .find(|c| c.native_rate == first_target)
            .copied()
            .unwrap_or(first);
        return Some(Choice {
            rate: first_target,
            native: Some(named.native_rate),
            direct: false,
            reason: Reason::Playing {
                name: named.name.clone(),
                native: named.native_rate,
                others: sorted.len() - 1,
            },
        });
    }
    let follow = |c: &RateClient, reason: Reason| Choice {
        rate: target(c),
        native: Some(c.native_rate),
        direct: false,
        reason,
    };
    Some(match mode {
        RateConflict::KeepCurrent => {
            if let Some(c) = sorted.iter().find(|c| target(c) == current) {
                follow(
                    c,
                    Reason::StillPlaying {
                        name: c.name.clone(),
                        native: c.native_rate,
                    },
                )
            } else {
                follow(
                    first,
                    Reason::FirstStarted {
                        name: first.name.clone(),
                        native: first.native_rate,
                    },
                )
            }
        }
        RateConflict::Highest => {
            // Highest native rate; the earliest client wins a tie.
            let best = sorted
                .iter()
                .copied()
                .reduce(|a, b| if b.native_rate > a.native_rate { b } else { a })
                .unwrap_or(first);
            follow(
                best,
                Reason::Highest {
                    name: best.name.clone(),
                    native: best.native_rate,
                },
            )
        }
        RateConflict::DefaultRate => Choice {
            rate: policy.default_rate,
            native: None,
            direct: false,
            reason: Reason::DefaultOnConflict {
                clients: sorted.len(),
            },
        },
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitWhy {
    /// The previous switch was less than [`MIN_DWELL`] ago.
    Dwell,
    /// Our previous switch is still settling (driver reopening ALSA).
    Settling,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// No client has an opinion: leave the rate alone.
    Idle,
    /// The current rate is the right one.
    Keep(Choice),
    /// Switch to `choice.rate` now.
    Switch(Choice),
    /// A switch is wanted but held back; decide again at `until`.
    Wait {
        choice: Choice,
        until: Instant,
        why: WaitWhy,
    },
}

impl Verdict {
    /// Whether this is a decision worth logging: a switch, a held-back
    /// switch, or keeping a rate while clients disagree. Following a single
    /// client (or clients that agree) at the rate already running is not.
    pub fn is_notable(&self) -> bool {
        match self {
            Verdict::Idle => false,
            Verdict::Switch(_) | Verdict::Wait { .. } => true,
            Verdict::Keep(c) => !matches!(c.reason, Reason::Playing { .. } | Reason::FollowOff),
        }
    }

    /// One line for the log, without timestamps, so the caller can log each
    /// distinct decision once.
    pub fn describe(&self, current: u32) -> Option<String> {
        match self {
            Verdict::Idle => None,
            Verdict::Keep(c) => Some(format!("kept {current}: {}", c.describe())),
            Verdict::Switch(c) => Some(format!("{current} -> {}: {}", c.rate, c.describe())),
            Verdict::Wait { choice, why, .. } => Some(format!(
                "holding {current}, want {} once the {} ends: {}",
                choice.rate,
                match why {
                    WaitWhy::Dwell => "minimum dwell",
                    WaitWhy::Settling => "previous switch",
                },
                choice.describe()
            )),
        }
    }
}

/// Timing state for rate switches and the client start order.
#[derive(Debug, Default)]
pub struct RateArbiter {
    last_switch: Option<Instant>,
    next_seq: u64,
}

impl RateArbiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that the graph rate was just changed by us.
    pub fn note_switch(&mut self, now: Instant) {
        self.last_switch = Some(now);
    }

    /// Whether client pause/resume events now are the fallout of our own
    /// switch. `driver_switching`: the driver reports `EngineState::Switching`.
    pub fn in_flap_window(&self, now: Instant, driver_switching: bool) -> bool {
        let Some(t) = self.last_switch else {
            return false;
        };
        let since = now.saturating_duration_since(t);
        since < FLAP_GRACE || (driver_switching && since < FLAP_MAX)
    }

    /// New start-order value for a client whose running state changed from
    /// `was_running` to `running`. `since` is its current value. A client that
    /// pauses and resumes inside the flap window keeps its place.
    pub fn update_since(
        &mut self,
        was_running: bool,
        running: bool,
        since: Option<u64>,
        now: Instant,
        driver_switching: bool,
    ) -> Option<u64> {
        let flap = self.in_flap_window(now, driver_switching);
        match (was_running, running) {
            (true, true) => since.or_else(|| Some(self.take_seq())),
            (true, false) => {
                if flap {
                    since
                } else {
                    None
                }
            }
            (false, true) => {
                if flap && since.is_some() {
                    since
                } else {
                    Some(self.take_seq())
                }
            }
            (false, false) => {
                if flap {
                    since
                } else {
                    None
                }
            }
        }
    }

    fn take_seq(&mut self) -> u64 {
        let s = self.next_seq;
        self.next_seq += 1;
        s
    }

    /// Decide what to do now. Does not record the switch: call
    /// [`RateArbiter::note_switch`] once the new rate is applied.
    pub fn decide(&self, inp: &RateInputs<'_>, now: Instant, driver_switching: bool) -> Verdict {
        let Some(choice) = choose_rate(inp) else {
            return Verdict::Idle;
        };
        if choice.rate == inp.current {
            return Verdict::Keep(choice);
        }
        // Turning following off is a user action: apply at once.
        if matches!(choice.reason, Reason::FollowOff) {
            return Verdict::Switch(choice);
        }
        let Some(t) = self.last_switch else {
            return Verdict::Switch(choice);
        };
        let since = now.saturating_duration_since(t);
        if driver_switching && since < FLAP_MAX && since + SWITCHING_POLL >= MIN_DWELL {
            return Verdict::Wait {
                choice,
                until: now + SWITCHING_POLL,
                why: WaitWhy::Settling,
            };
        }
        if since < MIN_DWELL {
            return Verdict::Wait {
                choice,
                until: t + MIN_DWELL,
                why: WaitWhy::Dwell,
            };
        }
        Verdict::Switch(choice)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BRAVE: u32 = 1;
    const GAME: u32 = 2;
    const PLAYER: u32 = 3;

    fn policy(conflict: RateConflict) -> RatePolicy {
        RatePolicy {
            conflict,
            ..RatePolicy::default()
        }
    }

    fn client(id: u32, name: &str, native_rate: u32, since: u64) -> RateClient {
        RateClient {
            id,
            name: name.into(),
            native_rate,
            since,
        }
    }

    fn dsp_inputs<'a>(
        policy: &'a RatePolicy,
        current: u32,
        dsp: &'a [RateClient],
    ) -> RateInputs<'a> {
        RateInputs {
            policy,
            current,
            direct_active: false,
            direct: &[],
            dsp,
            direct_policy: None,
        }
    }

    /// A tiny model of the engine: forced rate + arbiter + a clock.
    struct Sim {
        policy: RatePolicy,
        arb: RateArbiter,
        rate: u32,
        t0: Instant,
        now: Instant,
        switches: Vec<u32>,
    }

    impl Sim {
        fn new(policy: RatePolicy, rate: u32) -> Self {
            let t0 = Instant::now();
            Self {
                policy,
                arb: RateArbiter::new(),
                rate,
                t0,
                now: t0,
                switches: Vec::new(),
            }
        }
        fn at(&mut self, ms: u64) {
            self.now = self.t0 + Duration::from_millis(ms);
        }
        fn step(&mut self, dsp: &[RateClient]) -> Verdict {
            let v = self
                .arb
                .decide(&dsp_inputs(&self.policy, self.rate, dsp), self.now, false);
            if let Verdict::Switch(c) = &v {
                self.rate = c.rate;
                self.arb.note_switch(self.now);
                self.switches.push(c.rate);
            }
            v
        }
    }

    #[test]
    fn choose_allowed_rate_picks_exact_or_next_higher() {
        let policy = RatePolicy {
            allowed_rates: vec![44_100, 48_000, 96_000],
            ..RatePolicy::default()
        };
        assert_eq!(choose_allowed_rate(44_100, &policy), 44_100);
        assert_eq!(choose_allowed_rate(88_200, &policy), 96_000);
        assert_eq!(choose_allowed_rate(192_000, &policy), 96_000);
    }

    #[test]
    fn single_client_is_followed() {
        let p = policy(RateConflict::KeepCurrent);
        let dsp = [client(GAME, "game", 44_100, 0)];
        let c = choose_rate(&dsp_inputs(&p, 48_000, &dsp)).unwrap();
        assert_eq!(c.rate, 44_100);
        assert_eq!(c.native, Some(44_100));
        assert!(!c.direct);
    }

    #[test]
    fn no_clients_means_no_opinion() {
        let p = policy(RateConflict::KeepCurrent);
        assert_eq!(choose_rate(&dsp_inputs(&p, 48_000, &[])), None);
        assert_eq!(
            RateArbiter::new().decide(&dsp_inputs(&p, 48_000, &[]), Instant::now(), false),
            Verdict::Idle
        );
    }

    #[test]
    fn brave_first_then_game_keeps_48k() {
        let mut s = Sim::new(policy(RateConflict::KeepCurrent), 44_100);
        let brave = client(BRAVE, "Brave", 48_000, 0);
        let game = client(GAME, "game", 44_100, 1);
        assert!(matches!(
            s.step(std::slice::from_ref(&brave)),
            Verdict::Switch(_)
        ));
        assert_eq!(s.rate, 48_000);
        for ms in (100..30_000).step_by(100) {
            s.at(ms);
            let v = s.step(&[brave.clone(), game.clone()]);
            assert!(matches!(v, Verdict::Keep(_)), "{v:?}");
        }
        assert_eq!(s.switches, vec![48_000]);
        let v = s.step(&[game.clone(), brave.clone()]);
        assert_eq!(
            v.describe(48_000).unwrap(),
            "kept 48000: Brave still playing at 48000; others are resampled"
        );
    }

    #[test]
    fn game_first_then_brave_keeps_44k() {
        let mut s = Sim::new(policy(RateConflict::KeepCurrent), 48_000);
        let game = client(GAME, "game", 44_100, 0);
        let brave = client(BRAVE, "Brave", 48_000, 1);
        assert!(matches!(
            s.step(std::slice::from_ref(&game)),
            Verdict::Switch(_)
        ));
        for ms in (100..30_000).step_by(100) {
            s.at(ms);
            assert!(matches!(
                s.step(&[brave.clone(), game.clone()]),
                Verdict::Keep(_)
            ));
        }
        assert_eq!(s.switches, vec![44_100]);
    }

    #[test]
    fn earliest_client_wins_regardless_of_list_order() {
        // Neither plays at the current rate: start order decides, not the
        // order the clients are listed in (HashMap order in the engine).
        let p = policy(RateConflict::KeepCurrent);
        let a = client(BRAVE, "Brave", 48_000, 5);
        let b = client(GAME, "game", 44_100, 7);
        for dsp in [[a.clone(), b.clone()], [b.clone(), a.clone()]] {
            let c = choose_rate(&dsp_inputs(&p, 96_000, &dsp)).unwrap();
            assert_eq!(c.rate, 48_000);
            assert!(matches!(c.reason, Reason::FirstStarted { .. }));
        }
    }

    #[test]
    fn first_app_stopping_hands_rate_to_the_other_after_dwell() {
        let mut s = Sim::new(policy(RateConflict::KeepCurrent), 44_100);
        let brave = client(BRAVE, "Brave", 48_000, 0);
        let game = client(GAME, "game", 44_100, 1);
        s.step(std::slice::from_ref(&brave)); // switch to 48k at t=0
        s.at(500);
        assert!(matches!(
            s.step(&[brave.clone(), game.clone()]),
            Verdict::Keep(_)
        ));
        // Brave stops at 1 s: game wants 44.1k, but the dwell holds until 2 s.
        s.at(1_000);
        match s.step(std::slice::from_ref(&game)) {
            Verdict::Wait { choice, until, why } => {
                assert_eq!(choice.rate, 44_100);
                assert_eq!(why, WaitWhy::Dwell);
                assert_eq!(until, s.t0 + MIN_DWELL);
            }
            v => panic!("{v:?}"),
        }
        assert_eq!(s.rate, 48_000);
        // The re-check at the end of the dwell makes the switch.
        s.at(2_000);
        assert!(matches!(
            s.step(std::slice::from_ref(&game)),
            Verdict::Switch(_)
        ));
        assert_eq!(s.rate, 44_100);
        assert_eq!(s.switches, vec![48_000, 44_100]);
    }

    #[test]
    fn stopping_long_after_last_switch_switches_at_once() {
        let mut s = Sim::new(policy(RateConflict::KeepCurrent), 44_100);
        let brave = client(BRAVE, "Brave", 48_000, 0);
        let game = client(GAME, "game", 44_100, 1);
        s.step(std::slice::from_ref(&brave));
        s.at(10_000);
        s.step(&[brave, game.clone()]);
        s.at(20_000);
        assert!(matches!(s.step(&[game]), Verdict::Switch(_)));
        assert_eq!(s.rate, 44_100);
    }

    #[test]
    fn same_family_pair_is_a_conflict() {
        // 44.1k and 88.2k are both allowed: they disagree.
        let p = policy(RateConflict::KeepCurrent);
        let dsp = [
            client(PLAYER, "player", 88_200, 0),
            client(GAME, "game", 44_100, 1),
        ];
        let c = choose_rate(&dsp_inputs(&p, 44_100, &dsp)).unwrap();
        assert_eq!(c.rate, 44_100);
        assert!(matches!(c.reason, Reason::StillPlaying { .. }));
        let c = choose_rate(&dsp_inputs(&p, 48_000, &dsp)).unwrap();
        assert_eq!(c.rate, 88_200);
        assert!(matches!(c.reason, Reason::FirstStarted { .. }));
    }

    #[test]
    fn clients_mapping_to_the_same_allowed_rate_agree() {
        // With only 48k allowed in the 48k family, a 96k client maps to 48k
        // and agrees with a 48k client.
        let p = RatePolicy {
            allowed_rates: vec![44_100, 48_000],
            ..RatePolicy::default()
        };
        let dsp = [
            client(PLAYER, "player", 96_000, 0),
            client(BRAVE, "Brave", 48_000, 1),
        ];
        let c = choose_rate(&dsp_inputs(&p, 44_100, &dsp)).unwrap();
        assert_eq!(c.rate, 48_000);
        assert_eq!(
            c.reason,
            Reason::Playing {
                name: "Brave".into(),
                native: 48_000,
                others: 1
            }
        );
    }

    #[test]
    fn cross_family_pair_keeps_current() {
        let p = policy(RateConflict::KeepCurrent);
        let dsp = [
            client(PLAYER, "player", 96_000, 0),
            client(GAME, "game", 44_100, 1),
        ];
        for current in [96_000, 44_100] {
            assert_eq!(
                choose_rate(&dsp_inputs(&p, current, &dsp)).unwrap().rate,
                current
            );
        }
    }

    #[test]
    fn highest_mode_picks_highest_native_rate() {
        let p = policy(RateConflict::Highest);
        let dsp = [
            client(BRAVE, "Brave", 48_000, 0),
            client(GAME, "game", 44_100, 1),
            client(PLAYER, "player", 96_000, 2),
        ];
        for current in [44_100, 48_000, 96_000] {
            let c = choose_rate(&dsp_inputs(&p, current, &dsp)).unwrap();
            assert_eq!(c.rate, 96_000);
            assert_eq!(c.native, Some(96_000));
            assert!(matches!(c.reason, Reason::Highest { .. }));
        }
        // A single client is still simply followed.
        let one = [client(GAME, "game", 44_100, 1)];
        assert_eq!(
            choose_rate(&dsp_inputs(&p, 96_000, &one)).unwrap().rate,
            44_100
        );
    }

    #[test]
    fn default_rate_mode_uses_default_only_on_conflict() {
        let p = RatePolicy {
            conflict: RateConflict::DefaultRate,
            default_rate: 96_000,
            ..RatePolicy::default()
        };
        let both = [
            client(BRAVE, "Brave", 48_000, 0),
            client(GAME, "game", 44_100, 1),
        ];
        for current in [44_100, 48_000] {
            let c = choose_rate(&dsp_inputs(&p, current, &both)).unwrap();
            assert_eq!(c.rate, 96_000);
            assert_eq!(c.native, None);
            assert_eq!(c.reason, Reason::DefaultOnConflict { clients: 2 });
        }
        let one = [client(GAME, "game", 44_100, 1)];
        assert_eq!(
            choose_rate(&dsp_inputs(&p, 96_000, &one)).unwrap().rate,
            44_100
        );
    }

    #[test]
    fn every_mode_settles_in_both_start_orders() {
        for mode in [
            RateConflict::KeepCurrent,
            RateConflict::Highest,
            RateConflict::DefaultRate,
        ] {
            for (first, second) in [(48_000, 44_100), (44_100, 48_000)] {
                let mut s = Sim::new(policy(mode), 96_000);
                let a = client(BRAVE, "a", first, 0);
                let b = client(GAME, "b", second, 1);
                s.step(std::slice::from_ref(&a));
                for ms in (100..20_000).step_by(100) {
                    s.at(ms);
                    s.step(&[b.clone(), a.clone()]);
                }
                // At most one extra switch when the second app starts, then
                // the rate stays put.
                assert!(s.switches.len() <= 2, "{mode:?} {:?}", s.switches);
                let expected = match mode {
                    RateConflict::KeepCurrent => first,
                    RateConflict::Highest => 48_000,
                    RateConflict::DefaultRate => 48_000,
                };
                assert_eq!(s.rate, expected, "{mode:?} first={first}");
            }
        }
    }

    #[test]
    fn direct_sink_client_overrides_dsp_clients() {
        let p = policy(RateConflict::Highest);
        let dsp = [client(BRAVE, "Brave", 96_000, 0)];
        let direct = [client(PLAYER, "player", 44_100, 3)];
        let inp = RateInputs {
            policy: &p,
            current: 96_000,
            direct_active: true,
            direct: &direct,
            dsp: &dsp,
            direct_policy: None,
        };
        let c = choose_rate(&inp).unwrap();
        assert_eq!(c.rate, 44_100);
        assert!(c.direct);
        assert_eq!(
            RateArbiter::new().decide(&inp, Instant::now(), false),
            Verdict::Switch(c)
        );
        // Linked to Direct but nothing running there yet: no opinion, the
        // DSP clients do not take over.
        let inp = RateInputs { direct: &[], ..inp };
        assert_eq!(choose_rate(&inp), None);
    }

    #[test]
    fn direct_runs_at_a_native_rate() {
        // 96k-only device, DSP x2: effective [48k, 96k], Direct [96k].
        let p = RatePolicy {
            allowed_rates: vec![48_000, 96_000],
            default_rate: 48_000,
            ..RatePolicy::default()
        };
        let d = RatePolicy {
            allowed_rates: vec![96_000],
            default_rate: 96_000,
            ..p.clone()
        };
        let direct = [client(1, "player", 48_000, 0)];
        let inp = RateInputs {
            policy: &p,
            current: 48_000,
            direct_active: true,
            direct: &direct,
            dsp: &[],
            direct_policy: Some(&d),
        };
        let c = choose_rate(&inp).unwrap();
        assert_eq!((c.rate, c.direct), (96_000, true));
        // Following off: the default is not native, Direct moves the graph.
        let off = RatePolicy {
            follow_source: false,
            ..p.clone()
        };
        let inp = RateInputs {
            policy: &off,
            ..inp
        };
        assert_eq!(choose_rate(&inp).unwrap().rate, 96_000);
        // DSP-only playback keeps the default.
        let inp = RateInputs {
            direct_active: false,
            direct: &[],
            ..inp
        };
        assert_eq!(choose_rate(&inp).unwrap().rate, 48_000);
    }

    #[test]
    fn follow_off_forces_default_without_dwell() {
        let p = RatePolicy {
            follow_source: false,
            ..RatePolicy::default()
        };
        let mut arb = RateArbiter::new();
        let now = Instant::now();
        arb.note_switch(now);
        let dsp = [client(GAME, "game", 44_100, 0)];
        let v = arb.decide(&dsp_inputs(&p, 44_100, &dsp), now, true);
        assert!(matches!(v, Verdict::Switch(Choice { rate: 48_000, .. })));
    }

    #[test]
    fn dwell_holds_back_and_reports_when_to_look_again() {
        let p = policy(RateConflict::KeepCurrent);
        let mut arb = RateArbiter::new();
        let t0 = Instant::now();
        arb.note_switch(t0);
        let dsp = [client(GAME, "game", 44_100, 0)];
        let inp = dsp_inputs(&p, 48_000, &dsp);
        for ms in [0, 10, 1_000, 1_999] {
            match arb.decide(&inp, t0 + Duration::from_millis(ms), false) {
                Verdict::Wait { until, why, .. } => {
                    assert_eq!(until, t0 + MIN_DWELL);
                    assert_eq!(why, WaitWhy::Dwell);
                }
                v => panic!("{ms}: {v:?}"),
            }
        }
        assert!(matches!(
            arb.decide(&inp, t0 + MIN_DWELL, false),
            Verdict::Switch(_)
        ));
        // Keeping the current rate is never delayed.
        let inp = dsp_inputs(&p, 44_100, &dsp);
        assert!(matches!(arb.decide(&inp, t0, false), Verdict::Keep(_)));
    }

    #[test]
    fn driver_still_switching_extends_the_hold_up_to_a_cap() {
        let p = policy(RateConflict::KeepCurrent);
        let mut arb = RateArbiter::new();
        let t0 = Instant::now();
        arb.note_switch(t0);
        let dsp = [client(GAME, "game", 44_100, 0)];
        let inp = dsp_inputs(&p, 48_000, &dsp);
        let at = |ms| t0 + Duration::from_millis(ms);
        assert!(matches!(
            arb.decide(&inp, at(2_500), true),
            Verdict::Wait {
                why: WaitWhy::Settling,
                ..
            }
        ));
        assert!(matches!(
            arb.decide(&inp, at(2_500), false),
            Verdict::Switch(_)
        ));
        // A driver stuck in Switching does not block rate following forever.
        assert!(matches!(
            arb.decide(&inp, at(6_000), true),
            Verdict::Switch(_)
        ));
    }

    #[test]
    fn flaps_during_our_switch_keep_start_order() {
        let mut arb = RateArbiter::new();
        let t0 = Instant::now();
        let at = |ms| t0 + Duration::from_millis(ms);
        // game starts, then Brave.
        let game = arb.update_since(false, true, None, at(0), false);
        let brave = arb.update_since(false, true, None, at(100), false);
        assert!(game < brave);
        // We switch at 1 s; both pause and resume inside the flap window,
        // Brave resuming first.
        arb.note_switch(at(1_000));
        let game_p = arb.update_since(true, false, game, at(1_050), true);
        let brave_p = arb.update_since(true, false, brave, at(1_060), true);
        let brave_r = arb.update_since(false, true, brave_p, at(1_300), true);
        let game_r = arb.update_since(false, true, game_p, at(1_400), false);
        assert_eq!(game_r, game);
        assert_eq!(brave_r, brave);
        // Outside the window a pause is a real stop: resuming goes to the back.
        let game_s = arb.update_since(true, false, game_r, at(5_000), false);
        assert_eq!(game_s, None);
        let game_r2 = arb.update_since(false, true, game_s, at(5_100), false);
        assert!(game_r2 > brave_r);
    }

    #[test]
    fn flap_during_switch_cannot_start_another_switch() {
        // Brave (48k) started first and holds 48k; the game (44.1k) also
        // plays. A switch from elsewhere (e.g. a policy change) just happened,
        // and Brave briefly pauses: only the game is running for a moment.
        let mut s = Sim::new(policy(RateConflict::KeepCurrent), 48_000);
        s.arb.note_switch(s.now);
        let brave = client(BRAVE, "Brave", 48_000, 0);
        let game = client(GAME, "game", 44_100, 1);
        s.at(200);
        let v = s.step(std::slice::from_ref(&game));
        assert!(matches!(v, Verdict::Wait { .. }), "{v:?}");
        // Brave is back before the hold ends: nothing changes.
        s.at(700);
        assert!(matches!(
            s.step(&[game.clone(), brave.clone()]),
            Verdict::Keep(_)
        ));
        s.at(2_000);
        assert!(matches!(s.step(&[game, brave]), Verdict::Keep(_)));
        assert!(s.switches.is_empty());
    }

    #[test]
    fn decisions_describe_their_reason() {
        let p = policy(RateConflict::KeepCurrent);
        let dsp = [client(GAME, "game", 44_100, 0)];
        let v = RateArbiter::new().decide(&dsp_inputs(&p, 48_000, &dsp), Instant::now(), false);
        assert_eq!(
            v.describe(48_000).unwrap(),
            "48000 -> 44100: game plays at 44100"
        );
        assert_eq!(Verdict::Idle.describe(48_000), None);
        assert!(v.is_notable());
        assert!(!Verdict::Idle.is_notable());
        let keep_single =
            RateArbiter::new().decide(&dsp_inputs(&p, 44_100, &dsp), Instant::now(), false);
        assert!(!keep_single.is_notable());
        let both = [
            client(GAME, "game", 44_100, 0),
            client(BRAVE, "Brave", 48_000, 1),
        ];
        let keep_conflict =
            RateArbiter::new().decide(&dsp_inputs(&p, 44_100, &both), Instant::now(), false);
        assert!(keep_conflict.is_notable());
    }
}
