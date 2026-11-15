//! Pure, hardware-free lifecycle state machines for ratio resolution, sink
//! transition checks, and `ReserveDevice1` + ALSA device handoff.

use audivane_protocol::ipc::{EngineState, SinkChoice};
use audivane_protocol::{RateTarget, resolve_ratio};

/// Desired DSP rate-target snapshot propagated from control to RT with a
/// monotonic generation counter so Direct <-> DSP transitions never consult
/// stale startup configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DesiredDspConfig {
    pub dsp_enabled: bool,
    pub rate_target: RateTarget,
    pub max_ratio: u32,
    pub generation: u64,
}

impl Default for DesiredDspConfig {
    fn default() -> Self {
        Self {
            dsp_enabled: true,
            rate_target: RateTarget::Off,
            max_ratio: 16,
            generation: 0,
        }
    }
}

impl DesiredDspConfig {
    /// Resolve the integer output/input ratio for `(sink, graph_rate)`.
    pub fn resolve_ratio_for(self, sink: SinkChoice, graph_rate: u32) -> u32 {
        if sink == SinkChoice::Direct || !self.dsp_enabled {
            1
        } else {
            resolve_ratio(self.rate_target, graph_rate.max(8_000)).clamp(1, self.max_ratio.max(1))
        }
    }
}

/// What the last ALSA open was asked for: `(sink, graph_rate, requested
/// ratio)`. The device may have given a smaller ratio (rate not supported);
/// asking again for the same thing would only reopen into the same result.
pub type NegotiatedKey = (SinkChoice, u32, u32);

/// The key for opening `sink` at `graph_rate` under `cfg`.
pub fn negotiated_key(cfg: DesiredDspConfig, sink: SinkChoice, graph_rate: u32) -> NegotiatedKey {
    (sink, graph_rate, cfg.resolve_ratio_for(sink, graph_rate))
}

/// Pure decision function called by the RT thread (and control startup) to
/// decide whether a graph rate change, sink transition, or DSP config update
/// requires reopening ALSA, returning `Some(required_ratio)` if so.
///
/// A config update alone (new generation, e.g. a filter-length change) never
/// reopens when it asks for the same ratio the last open was asked for: on a
/// device that fell back to a smaller ratio that would reopen (and gap) on
/// every edit for nothing.
#[inline]
pub fn decide_rt_reopen(
    rt_cfg: DesiredDspConfig,
    last_negotiated_key: Option<NegotiatedKey>,
    active_sink: SinkChoice,
    target_sink: SinkChoice,
    active_graph_rate: u32,
    cur_rate: u32,
    active_ratio: u32,
) -> Option<u32> {
    let required_ratio = rt_cfg.resolve_ratio_for(target_sink, cur_rate);
    if cur_rate != active_graph_rate {
        return Some(required_ratio);
    }
    if target_sink != active_sink && required_ratio != active_ratio {
        return Some(required_ratio);
    }
    let cur_key = (target_sink, cur_rate, required_ratio);
    if required_ratio != active_ratio && last_negotiated_key != Some(cur_key) {
        return Some(required_ratio);
    }
    None
}

/// Pure reopen coordination gate preventing duplicate `SuspendForReopen` /
/// `NeedReopen` cycles while still permitting recovery from `EngineState::Error`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReopenGate {
    pub awaiting_reopen_ack: bool,
    pub rt_reopen_in_flight: bool,
}

impl ReopenGate {
    pub const fn new() -> Self {
        Self {
            awaiting_reopen_ack: false,
            rt_reopen_in_flight: false,
        }
    }

    /// Control thread: returns `true` when `SuspendForReopen` should be sent.
    pub fn ctrl_request_reopen(
        &mut self,
        force_reopen: bool,
        released_or_teardown: bool,
        state: EngineState,
    ) -> bool {
        if force_reopen
            && !released_or_teardown
            && state != EngineState::Idle
            && !self.awaiting_reopen_ack
        {
            self.awaiting_reopen_ack = true;
            true
        } else {
            false
        }
    }

    /// Control thread: consumes `awaiting_reopen_ack` when handling `NeedReopen`.
    pub fn ctrl_take_awaiting_cfg(&mut self) -> bool {
        std::mem::take(&mut self.awaiting_reopen_ack)
    }

    /// Control thread: clears pending reopen state when entering release teardown.
    pub fn ctrl_clear_for_release(&mut self) {
        self.awaiting_reopen_ack = false;
    }

    /// RT thread: returns `true` if a `NeedReopen` event should be emitted.
    pub fn rt_try_begin_reopen(&mut self) -> bool {
        if self.rt_reopen_in_flight {
            false
        } else {
            self.rt_reopen_in_flight = true;
            true
        }
    }

    /// RT thread: clears `rt_reopen_in_flight` on `Resume`, `ReopenAborted`, or
    /// `SuspendForRelease`.
    pub fn rt_finish_reopen(&mut self) {
        self.rt_reopen_in_flight = false;
    }
}

/// Abstract backend for reservation + PCM open/close so device handoff and
/// error-state invariants can be tested exhaustively without ALSA/D-Bus.
pub trait DeviceBackend {
    type Reservation;
    type Output;

    fn card_index(&mut self, device: &str) -> std::result::Result<u32, String>;
    fn reserve(
        &mut self,
        device: &str,
        card: u32,
        priority: i32,
    ) -> std::result::Result<Self::Reservation, String>;
    fn is_release_requested(&self, res: &Self::Reservation) -> bool;
    fn release_reservation(&mut self, res: Self::Reservation);
    fn open_and_prepare(
        &mut self,
        device: &str,
        graph_rate: u32,
        desired_ratio: u32,
        quantum: u32,
    ) -> std::result::Result<OpenedDevice<Self::Output>, String>;
    fn close_output(&mut self, out: Self::Output);

    /// Called once the reservation is held, right before `open_and_prepare`.
    /// The backend may move the graph rate (and its quantum) to one the
    /// device can play; [`DeviceLifecycle::handoff_and_open`] then opens at
    /// the returned rate and reports it in [`OpenedDevice::graph_rate`].
    fn resolve_graph_rate(
        &mut self,
        _device: &str,
        graph_rate: u32,
        quantum: u32,
        _target_sink: SinkChoice,
        _desired_dsp: DesiredDspConfig,
    ) -> std::result::Result<(u32, u32), String> {
        Ok((graph_rate, quantum))
    }
}

/// Result returned by [`DeviceBackend::open_and_prepare`].
#[derive(Debug)]
pub struct OpenedDevice<O> {
    pub output: O,
    pub actual_ratio: u32,
    pub output_rate: u32,
    /// Graph rate the device was opened for (filled in by
    /// [`DeviceLifecycle::handoff_and_open`]; may differ from the requested
    /// one after [`DeviceBackend::resolve_graph_rate`]).
    pub graph_rate: u32,
    /// Quantum used for that graph rate.
    pub quantum: u32,
}

/// Lifecycle manager enforcing full close-old -> release-old -> reserve-new ->
/// open-new handoff semantics and explicit `Error` state on any failure.
pub struct DeviceLifecycle<B: DeviceBackend> {
    pub state: EngineState,
    pub last_error: Option<String>,
    pub reservation: Option<(u32, B::Reservation)>,
    pub active_device: Option<String>,
    pub active_graph_rate: u32,
    pub active_ratio: u32,
    pub active_sink: SinkChoice,
    pub output_held: bool,
}

impl<B: DeviceBackend> Default for DeviceLifecycle<B> {
    fn default() -> Self {
        Self::new()
    }
}

impl<B: DeviceBackend> DeviceLifecycle<B> {
    pub fn new() -> Self {
        Self {
            state: EngineState::Idle,
            last_error: None,
            reservation: None,
            active_device: None,
            active_graph_rate: 48_000,
            active_ratio: 1,
            active_sink: SinkChoice::Dsp,
            output_held: false,
        }
    }

    pub fn release_reservation(&mut self, backend: &mut B) {
        if let Some((_, res)) = self.reservation.take() {
            backend.release_reservation(res);
        }
    }

    pub fn reservation_release_requested(&self, backend: &B) -> bool {
        self.reservation
            .as_ref()
            .is_some_and(|(_, r)| backend.is_release_requested(r))
    }

    /// Release any held reservation cleanly when transitioning to `Idle`.
    pub fn finish_release_to_idle(&mut self, backend: &mut B, old_out: Option<B::Output>) {
        if let Some(out) = old_out {
            backend.close_output(out);
        }
        self.output_held = false;
        self.release_reservation(backend);
        self.active_device = None;
        self.state = EngineState::Idle;
    }

    /// Execute a full device handoff / reopen:
    /// 1. Close `old_out` (if provided).
    /// 2. Ensure reservation matches `target_device`'s card (reusing if same
    ///    card and still valid, otherwise releasing old card's reservation BEFORE
    ///    acquiring the new card's reservation).
    /// 3. Resolve desired ratio at `graph_rate` and open `target_device`.
    /// 4. On ANY failure, leave no active output and release any held
    ///    reservation, setting `state = EngineState::Error` and `last_error` so
    ///    device A is never silently kept.
    #[allow(clippy::too_many_arguments)]
    pub fn handoff_and_open(
        &mut self,
        backend: &mut B,
        old_out: Option<B::Output>,
        target_device: &str,
        reserve_device: bool,
        reserve_priority: i32,
        graph_rate: u32,
        quantum: u32,
        target_sink: SinkChoice,
        desired_dsp: DesiredDspConfig,
    ) -> std::result::Result<OpenedDevice<B::Output>, String> {
        if let Some(out) = old_out {
            backend.close_output(out);
        }
        self.output_held = false;

        if reserve_device {
            let target_card = match backend.card_index(target_device) {
                Ok(c) => c,
                Err(e) => {
                    self.release_reservation(backend);
                    self.active_device = None;
                    let msg = format!("Invalid device '{target_device}': {e}");
                    self.last_error = Some(msg.clone());
                    self.state = EngineState::Error;
                    return Err(msg);
                }
            };

            let keep_existing = self.reservation.as_ref().is_some_and(|(held_card, res)| {
                *held_card == target_card && !backend.is_release_requested(res)
            });

            if !keep_existing {
                // Release old card reservation BEFORE reserving new card.
                self.release_reservation(backend);
                match backend.reserve(target_device, target_card, reserve_priority) {
                    Ok(res) => {
                        self.reservation = Some((target_card, res));
                    }
                    Err(e) => {
                        self.active_device = None;
                        let msg = format!("ReserveDevice1 failed for {target_device}: {e}");
                        self.last_error = Some(msg.clone());
                        self.state = EngineState::Error;
                        return Err(msg);
                    }
                }
            }
        } else {
            self.release_reservation(backend);
        }

        let (graph_rate, quantum) = match backend.resolve_graph_rate(
            target_device,
            graph_rate,
            quantum,
            target_sink,
            desired_dsp,
        ) {
            Ok(v) => v,
            Err(e) => {
                self.release_reservation(backend);
                self.active_device = None;
                let msg = format!("ALSA open/prepare failed for {target_device}: {e}");
                self.last_error = Some(msg.clone());
                self.state = EngineState::Error;
                return Err(msg);
            }
        };
        let desired_ratio = desired_dsp.resolve_ratio_for(target_sink, graph_rate);
        match backend.open_and_prepare(target_device, graph_rate, desired_ratio, quantum) {
            Ok(mut opened) => {
                opened.graph_rate = graph_rate;
                opened.quantum = quantum;
                self.active_device = Some(target_device.to_string());
                self.active_graph_rate = graph_rate;
                self.active_ratio = opened.actual_ratio;
                self.active_sink = target_sink;
                self.output_held = true;
                self.last_error = None;
                self.state = EngineState::Ready;
                Ok(opened)
            }
            Err(e) => {
                // Do not sit on a reservation or claim old device A is still open.
                self.release_reservation(backend);
                self.active_device = None;
                let msg = format!("ALSA open/prepare failed for {target_device}: {e}");
                self.last_error = Some(msg.clone());
                self.state = EngineState::Error;
                Err(msg)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Op {
        Reserve(u32, String),
        Release(u32),
        Open(String, u32, u32),
        Close(String),
    }

    struct MockBackend {
        ops: Vec<Op>,
        fail_reserve_cards: HashSet<u32>,
        fail_open_devices: HashSet<String>,
        max_rate_for_device: Option<u32>,
        /// Device plays only this graph rate (like a 48k-only USB DAC).
        only_rate: Option<u32>,
    }

    impl MockBackend {
        fn new() -> Self {
            Self {
                ops: Vec::new(),
                fail_reserve_cards: HashSet::new(),
                fail_open_devices: HashSet::new(),
                max_rate_for_device: None,
                only_rate: None,
            }
        }
    }

    impl DeviceBackend for MockBackend {
        type Reservation = u32;
        type Output = String;

        fn card_index(&mut self, device: &str) -> std::result::Result<u32, String> {
            let rest = device
                .strip_prefix("hw:")
                .ok_or_else(|| format!("not hw: {device}"))?;
            let card = rest.split(',').next().unwrap_or("");
            if card == "Loopback" {
                return Ok(10);
            }
            if card == "CardA" {
                return Ok(1);
            }
            if card == "CardB" {
                return Ok(2);
            }
            card.parse::<u32>()
                .map_err(|_| format!("bad card name: {device}"))
        }

        fn reserve(
            &mut self,
            device: &str,
            card: u32,
            _priority: i32,
        ) -> std::result::Result<Self::Reservation, String> {
            if self.fail_reserve_cards.contains(&card) {
                return Err(format!("busy card {card}"));
            }
            self.ops.push(Op::Reserve(card, device.to_string()));
            Ok(card)
        }

        fn is_release_requested(&self, _res: &Self::Reservation) -> bool {
            false
        }

        fn release_reservation(&mut self, res: Self::Reservation) {
            self.ops.push(Op::Release(res));
        }

        fn open_and_prepare(
            &mut self,
            device: &str,
            graph_rate: u32,
            desired_ratio: u32,
            _quantum: u32,
        ) -> std::result::Result<OpenedDevice<Self::Output>, String> {
            if self.fail_open_devices.contains(device) {
                return Err(format!("cannot open {device}"));
            }
            let mut r = desired_ratio.max(1);
            if let Some(max_r) = self.max_rate_for_device {
                while r > 1 && graph_rate * r > max_r {
                    r /= 2;
                }
            }
            let out_rate = graph_rate * r;
            self.ops.push(Op::Open(device.to_string(), graph_rate, r));
            Ok(OpenedDevice {
                output: device.to_string(),
                actual_ratio: r,
                output_rate: out_rate,
                graph_rate,
                quantum: _quantum,
            })
        }

        fn close_output(&mut self, out: Self::Output) {
            self.ops.push(Op::Close(out));
        }

        fn resolve_graph_rate(
            &mut self,
            _device: &str,
            graph_rate: u32,
            quantum: u32,
            _target_sink: SinkChoice,
            _desired_dsp: DesiredDspConfig,
        ) -> std::result::Result<(u32, u32), String> {
            Ok(match self.only_rate {
                Some(r) if r != graph_rate => (r, quantum * r / graph_rate),
                _ => (graph_rate, quantum),
            })
        }
    }

    #[test]
    fn resolved_graph_rate_is_opened_and_reported() {
        let mut backend = MockBackend::new();
        backend.only_rate = Some(48_000);
        let mut lc = DeviceLifecycle::new();
        let dsp = DesiredDspConfig::default();
        let opened = lc
            .handoff_and_open(
                &mut backend,
                None,
                "hw:CardA,0",
                true,
                10,
                44_100,
                882,
                SinkChoice::Dsp,
                dsp,
            )
            .unwrap();
        assert_eq!(opened.graph_rate, 48_000);
        assert_eq!(opened.quantum, 960);
        assert_eq!(lc.active_graph_rate, 48_000);
        assert!(
            backend
                .ops
                .contains(&Op::Open("hw:CardA,0".into(), 48_000, 1))
        );
    }

    #[test]
    fn switching_between_different_cards_releases_old_reservation_before_reserving_new() {
        let mut backend = MockBackend::new();
        let mut lc = DeviceLifecycle::new();
        let dsp = DesiredDspConfig {
            dsp_enabled: true,
            rate_target: RateTarget::Ratio(4),
            max_ratio: 16,
            generation: 1,
        };

        let out_a = lc
            .handoff_and_open(
                &mut backend,
                None,
                "hw:CardA,0,0",
                true,
                1000,
                48_000,
                1024,
                SinkChoice::Dsp,
                dsp,
            )
            .unwrap()
            .output;
        assert_eq!(lc.state, EngineState::Ready);
        assert_eq!(lc.reservation, Some((1, 1)));

        let out_b = lc
            .handoff_and_open(
                &mut backend,
                Some(out_a),
                "hw:CardB,0,0",
                true,
                1000,
                48_000,
                1024,
                SinkChoice::Dsp,
                dsp,
            )
            .unwrap()
            .output;
        assert_eq!(out_b, "hw:CardB,0,0");
        assert_eq!(lc.state, EngineState::Ready);
        assert_eq!(lc.reservation, Some((2, 2)));
        assert_eq!(lc.active_device.as_deref(), Some("hw:CardB,0,0"));
        assert_eq!(
            backend.ops,
            vec![
                Op::Reserve(1, "hw:CardA,0,0".into()),
                Op::Open("hw:CardA,0,0".into(), 48_000, 4),
                Op::Close("hw:CardA,0,0".into()),
                Op::Release(1),
                Op::Reserve(2, "hw:CardB,0,0".into()),
                Op::Open("hw:CardB,0,0".into(), 48_000, 4),
            ]
        );
    }

    #[test]
    fn switching_substreams_on_same_card_keeps_reservation_without_dropping() {
        let mut backend = MockBackend::new();
        let mut lc = DeviceLifecycle::new();
        let dsp = DesiredDspConfig::default();

        let out0 = lc
            .handoff_and_open(
                &mut backend,
                None,
                "hw:Loopback,0,0",
                true,
                1000,
                48_000,
                1024,
                SinkChoice::Dsp,
                dsp,
            )
            .unwrap()
            .output;
        let _out1 = lc
            .handoff_and_open(
                &mut backend,
                Some(out0),
                "hw:Loopback,0,1",
                true,
                1000,
                48_000,
                1024,
                SinkChoice::Dsp,
                dsp,
            )
            .unwrap()
            .output;

        assert_eq!(lc.reservation, Some((10, 10)));
        assert_eq!(lc.active_device.as_deref(), Some("hw:Loopback,0,1"));
        assert_eq!(
            backend.ops,
            vec![
                Op::Reserve(10, "hw:Loopback,0,0".into()),
                Op::Open("hw:Loopback,0,0".into(), 48_000, 1),
                Op::Close("hw:Loopback,0,0".into()),
                Op::Open("hw:Loopback,0,1".into(), 48_000, 1),
            ]
        );
    }

    #[test]
    fn device_change_failure_enters_error_and_does_not_silently_keep_old_device() {
        let mut backend = MockBackend::new();
        backend.fail_open_devices.insert("hw:CardB,0,0".into());
        let mut lc = DeviceLifecycle::new();
        let dsp = DesiredDspConfig::default();

        let out_a = lc
            .handoff_and_open(
                &mut backend,
                None,
                "hw:CardA,0,0",
                true,
                1000,
                48_000,
                1024,
                SinkChoice::Dsp,
                dsp,
            )
            .unwrap()
            .output;

        let err = lc
            .handoff_and_open(
                &mut backend,
                Some(out_a),
                "hw:CardB,0,0",
                true,
                1000,
                48_000,
                1024,
                SinkChoice::Dsp,
                dsp,
            )
            .unwrap_err();
        assert!(err.contains("hw:CardB,0,0"), "got: {err}");
        assert_eq!(lc.state, EngineState::Error);
        assert!(lc.last_error.is_some());
        assert_eq!(lc.reservation, None, "failed switch must release card B");
        assert_eq!(lc.active_device, None, "old device A must not stay active");
        assert!(!lc.output_held);

        // Switching to a working device afterwards recovers cleanly out of Error.
        let _out_c = lc
            .handoff_and_open(
                &mut backend,
                None,
                "hw:Loopback,0,0",
                true,
                1000,
                48_000,
                1024,
                SinkChoice::Dsp,
                dsp,
            )
            .unwrap()
            .output;
        assert_eq!(lc.state, EngineState::Ready);
        assert!(lc.last_error.is_none());
        assert_eq!(lc.active_device.as_deref(), Some("hw:Loopback,0,0"));
    }

    #[test]
    fn rate_target_max_rate_resolves_against_actual_graph_rate_and_direct_dsp_transitions() {
        let cfg1 = DesiredDspConfig {
            dsp_enabled: true,
            rate_target: RateTarget::Off,
            max_ratio: 16,
            generation: 1,
        };
        // Scenario 1: start ratio 1 -> update config to x4 (gen 2) -> Direct (x1) -> DSP (must be x4!).
        let cfg2 = DesiredDspConfig {
            dsp_enabled: true,
            rate_target: RateTarget::Ratio(4),
            max_ratio: 16,
            generation: 2,
        };
        assert_eq!(cfg1.resolve_ratio_for(SinkChoice::Dsp, 48_000), 1);
        assert_eq!(
            decide_rt_reopen(
                cfg2,
                Some((SinkChoice::Dsp, 48_000, 1)),
                SinkChoice::Dsp,
                SinkChoice::Dsp,
                48_000,
                48_000,
                1
            ),
            Some(4)
        );
        // On Direct, must reopen at x1:
        assert_eq!(
            decide_rt_reopen(
                cfg2,
                Some((SinkChoice::Dsp, 48_000, 4)),
                SinkChoice::Dsp,
                SinkChoice::Direct,
                48_000,
                48_000,
                4
            ),
            Some(1)
        );
        // Back from Direct -> DSP using updated cfg2 (gen 2) must reopen at x4:
        assert_eq!(
            decide_rt_reopen(
                cfg2,
                Some((SinkChoice::Direct, 48_000, 1)),
                SinkChoice::Direct,
                SinkChoice::Dsp,
                48_000,
                48_000,
                1
            ),
            Some(4)
        );

        // Scenario 1b: Hardware capped at 96k (ratio 2 instead of 4) must NOT loop reopening!
        assert_eq!(
            decide_rt_reopen(
                cfg2,
                Some((SinkChoice::Dsp, 48_000, 4)),
                SinkChoice::Dsp,
                SinkChoice::Dsp,
                48_000,
                48_000,
                2
            ),
            None
        );

        // Scenario 2: MaxRate(192000) resolves 44.1k->x4(176.4k), 48k->x4(192k), 96k->x2(192k).
        let max192 = DesiredDspConfig {
            dsp_enabled: true,
            rate_target: RateTarget::MaxRate(192_000),
            max_ratio: 16,
            generation: 3,
        };
        assert_eq!(max192.resolve_ratio_for(SinkChoice::Dsp, 44_100), 4);
        assert_eq!(max192.resolve_ratio_for(SinkChoice::Dsp, 48_000), 4);
        assert_eq!(max192.resolve_ratio_for(SinkChoice::Dsp, 96_000), 2);
        assert_eq!(max192.resolve_ratio_for(SinkChoice::Dsp, 192_000), 1);

        // Switching source 48k -> 96k while on DSP triggers reopen with ratio 2 (192k), NOT ratio 4 (384k).
        assert_eq!(
            decide_rt_reopen(
                max192,
                Some((SinkChoice::Dsp, 48_000, 4)),
                SinkChoice::Dsp,
                SinkChoice::Dsp,
                48_000,
                96_000,
                4
            ),
            Some(2)
        );
    }

    #[test]
    fn config_edit_on_ratio_capped_device_does_not_reopen() {
        // 48k-only device: x4 wanted, x1 granted. A later config change (new
        // generation, same target) must not reopen; a new target must.
        let gen1 = DesiredDspConfig {
            dsp_enabled: true,
            rate_target: RateTarget::Ratio(4),
            max_ratio: 16,
            generation: 1,
        };
        let key = negotiated_key(gen1, SinkChoice::Dsp, 48_000);
        let gen2 = DesiredDspConfig {
            generation: 2,
            ..gen1
        };
        let r = |cfg| {
            decide_rt_reopen(
                cfg,
                Some(key),
                SinkChoice::Dsp,
                SinkChoice::Dsp,
                48_000,
                48_000,
                1,
            )
        };
        assert_eq!(r(gen2), None);
        let gen3 = DesiredDspConfig {
            rate_target: RateTarget::Ratio(2),
            generation: 3,
            ..gen1
        };
        assert_eq!(r(gen3), Some(2));
    }

    #[test]
    fn reopen_gate_suppresses_duplicate_reopens_and_allows_error_recovery() {
        let mut gate = ReopenGate::new();

        // Rapid double force_reopen on control thread emits only one SuspendForReopen.
        assert!(gate.ctrl_request_reopen(true, false, EngineState::Ready));
        assert!(!gate.ctrl_request_reopen(true, false, EngineState::Switching));

        // RT thread accepts SuspendForReopen and suppresses concurrent NeedReopen.
        assert!(gate.rt_try_begin_reopen());
        assert!(!gate.rt_try_begin_reopen());

        // Control thread processes NeedReopen, then fails -> sends ReopenAborted.
        assert!(gate.ctrl_take_awaiting_cfg());
        assert!(!gate.ctrl_take_awaiting_cfg());
        gate.rt_finish_reopen();

        // Recovery apply from EngineState::Error MUST trigger a fresh reopen.
        assert!(gate.ctrl_request_reopen(true, false, EngineState::Error));
        assert!(gate.rt_try_begin_reopen());
        assert!(gate.ctrl_take_awaiting_cfg());
        gate.rt_finish_reopen();

        // Released / Idle state suppresses reopen requests.
        assert!(!gate.ctrl_request_reopen(true, true, EngineState::Ready));
        assert!(!gate.ctrl_request_reopen(true, false, EngineState::Idle));
    }
}
