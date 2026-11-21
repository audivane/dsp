//! The RT loop's varispeed steps driven offline: a simulated graph
//! ([`super::sim`]), a stand-in stage and processor.

use super::sim::{SimGraph, idle_until};
use super::*;
use crate::varispeed_sched::pull_bound;

const RATE: u32 = 48_000;
const CH: usize = 2;
const MS: u64 = 1_000_000;

/// A stage stand-in: plans `ceil(n · s)`; a ramped command applies at the
/// next plan, an unramped one and a reset at once (at the capped target).
#[derive(Debug)]
struct Stage {
    speed: f64,
    limit: f64,
    req: f64,
    ramp: f64,
    max: usize,
    latched: Option<(usize, usize)>,
    /// `(n, n_src, speed)` per plan.
    plans: Vec<(usize, usize, f64)>,
    resets: usize,
    raw_next: bool,
    until_stop: Option<usize>,
    latency: f64,
}

impl Stage {
    fn new(speed: f64) -> Self {
        Self {
            speed,
            limit: 2.0,
            req: speed,
            ramp: 0.0,
            max: 8192,
            latched: None,
            plans: Vec::new(),
            resets: 0,
            raw_next: false,
            until_stop: None,
            latency: 0.0,
        }
    }
}

impl VarispeedPort for Stage {
    fn plan(&mut self, n: usize) -> usize {
        assert!(
            n <= self.max,
            "a call of {n} frames, more than {}",
            self.max
        );
        if self.ramp > 0.0 {
            self.speed = self.req.min(self.limit);
            self.ramp = 0.0;
        }
        let n_src = (n as f64 * self.speed).ceil() as usize;
        self.latched = Some((n, n_src));
        self.plans.push((n, n_src, self.speed));
        n_src
    }
    fn set_fill(&mut self, _valid: usize, _underfeed: bool) {}
    fn set_speed_limit(&mut self, max: f64) {
        self.limit = max;
    }
    fn set_speed(&mut self, speed: f64, ramp_ms: f64) {
        if speed == self.req && self.speed == speed.min(self.limit) {
            return;
        }
        self.req = speed;
        self.ramp = ramp_ms;
        if ramp_ms == 0.0 {
            self.speed = speed.min(self.limit);
        }
    }
    fn peak_until_settled(&self) -> f64 {
        self.speed.max(self.req.min(self.limit))
    }
    fn speed(&self) -> f64 {
        self.speed
    }
    fn latency_frames(&self) -> f64 {
        self.latency
    }
    fn reset(&mut self) {
        self.resets += 1;
        self.speed = self.req.min(self.limit);
        self.ramp = 0.0;
    }
    fn max_frames(&self) -> usize {
        self.max
    }
    fn raw_next(&self) -> bool {
        self.raw_next
    }
    fn frames_until_stop(&self) -> Option<usize> {
        self.until_stop
    }
}

/// A processor stand-in: records each block, outputs ones.
struct Proc {
    stage: Option<Stage>,
    /// `(source frames, output frames, planned, in_pos)` per block.
    blocks: Vec<(usize, usize, bool, u64)>,
}

impl Proc {
    fn new(stage: Stage) -> Self {
        Self {
            stage: Some(stage),
            blocks: Vec::new(),
        }
    }
    fn stage(&self) -> &Stage {
        self.stage.as_ref().unwrap()
    }
}

impl EngineAudioProcessor for Proc {
    fn process_block(
        &mut self,
        _sink: SinkChoice,
        dsp_in: &[f32],
        _direct_in: &[f32],
        out: &mut [i32],
        channels: usize,
        _ratio: RateRatio,
        _graph_rate: u32,
        in_pos: u64,
    ) {
        let planned = self.stage.as_mut().and_then(|s| s.latched.take()).is_some();
        self.blocks.push((
            dsp_in.len() / channels,
            out.len() / channels,
            planned,
            in_pos,
        ));
        out.fill(1);
    }
    fn run_selftest_direct(&mut self, _in: &[f32], _out: &mut [i32]) {}
    fn poll_updates(&mut self, _s: SinkChoice, _g: u32, _r: RateRatio, _st: bool) {}
    fn latency_out_frames(&self, _s: SinkChoice) -> usize {
        0
    }
    fn reset_dsp_state(&mut self) {}
    fn varispeed(
        &mut self,
        _sink: SinkChoice,
        _ratio: RateRatio,
        _graph_rate: u32,
        _channels: usize,
    ) -> Option<&mut dyn VarispeedPort> {
        self.stage.as_mut().map(|s| s as &mut dyn VarispeedPort)
    }
}

struct Rig {
    vs: VarispeedDriver,
    graph: SimGraph,
    ring: rtrb::Consumer<f32>,
    proc: Proc,
    slot: VarispeedCommandSlot,
    delay: VsDelay,
    dsp_in: Vec<f32>,
    direct_in: Vec<f32>,
    out: Vec<i32>,
}

impl Rig {
    fn new(stage: Stage) -> Self {
        let (prod, ring) = rtrb::RingBuffer::new(crate::varispeed_sched::RING_ALLOC_FRAMES * CH);
        Self {
            vs: VarispeedDriver::new(),
            graph: SimGraph::new(prod, CH, 300_000),
            ring,
            proc: Proc::new(stage),
            slot: VarispeedCommandSlot::default(),
            delay: VsDelay::new(RateRatio::new(1, 1), RATE),
            dsp_in: vec![0.0; (2 * 8192 + 2) * CH],
            direct_in: vec![0.0; (2 * 8192 + 2) * CH],
            out: vec![0; 16_384 * CH],
        }
    }

    fn period(&mut self, ratio: RateRatio, pos: u64, n_in: usize, n_out: usize) -> PeriodRun {
        let q = self.vs.timing().quantum;
        vsl_run(self, ratio, pos, n_in, n_out, u64::from(q), None).expect("a stage runs")
    }
}

fn vsl_run(
    rig: &mut Rig,
    ratio: RateRatio,
    pos: u64,
    n_in: usize,
    n_out: usize,
    quantum: u64,
    prime_wait_ns: Option<u64>,
) -> Option<PeriodRun> {
    run_period(
        &mut rig.vs,
        &mut rig.graph,
        &mut rig.proc,
        PeriodBufs {
            ring: &mut rig.ring,
            dsp_in: &mut rig.dsp_in,
            direct_in: &rig.direct_in,
            out: &mut rig.out[..n_out * CH],
        },
        &rig.slot,
        PeriodPlan {
            sink: SinkChoice::Dsp,
            ratio,
            graph_rate: RATE,
            channels: CH,
            pos,
            n_in,
            n_out,
            kind: PullKind::Play,
            prime_wait_ns,
            poll: prime_wait_ns.is_none(),
            quantum,
        },
        &mut rig.delay,
    )
}

#[test]
fn an_oversized_period_runs_as_bounded_stage_calls() {
    // 192 kHz → 44.1 kHz at a quantum of 8192: the period rounds to 1 882
    // output frames, 8 194 graph frames: more than one stage call takes.
    let ratio = RateRatio::from_rates(192_000, 44_100);
    // The planner asks 8 190, then 8 194 frames (the credit of the first).
    assert_eq!(ratio.frames_in_for(0, 1_882), 8_190);
    let pos = 8_190;
    let n_in = 8_194;
    let n_out = ratio.frames_out(pos, n_in);
    assert!((1_882..=1_883).contains(&n_out), "{n_out}");
    let mut rig = Rig::new(Stage::new(1.25));
    rig.vs.start(
        Timing::new(192_000, n_in as u32 + 1, 8192),
        false,
        rig.proc.stage.as_mut().unwrap(),
    );
    // The ring holds the period's source (a full prime).
    rig.graph.advance(rig.graph.now);
    for _ in 0..4 {
        rig.graph.trigger(192_000, 8192, 0, 1.0);
    }
    rig.graph.advance(u64::MAX / 2);
    let run = rig.period(ratio, pos, n_in, n_out);
    assert_eq!(run.calls, 2);
    let plans = &rig.proc.stage().plans;
    assert_eq!(plans.len(), 2);
    assert!(plans.iter().all(|p| p.0 <= 8192));
    assert_eq!(plans.iter().map(|p| p.0).sum::<usize>(), n_in);
    // Each call is planned and processed before the next is planned; the
    // blocks tile the period on the frame grid.
    let blocks = &rig.proc.blocks;
    assert_eq!(blocks.len(), 2);
    assert!(blocks.iter().all(|b| b.2), "every block ran the stage");
    assert_eq!(blocks[0].3, pos);
    assert_eq!(blocks[1].3, pos + plans[0].0 as u64);
    assert_eq!(blocks.iter().map(|b| b.1).sum::<usize>(), n_out);
    assert_eq!(run.taken, plans.iter().map(|p| p.1).sum::<usize>());
    assert!(!run.underfeed);
}

#[test]
fn a_stopping_stage_ends_its_call_at_the_fade_and_the_rest_plays_raw() {
    let mut rig = Rig::new(Stage::new(1.0));
    rig.vs.start(
        Timing::new(RATE, 1024, 1024),
        false,
        rig.proc.stage.as_mut().unwrap(),
    );
    for _ in 0..3 {
        rig.graph.trigger(RATE, 1024, 0, 1.0);
    }
    rig.graph.advance(u64::MAX / 2);
    // The stage stops 240 frames in: the call ends there.
    rig.proc.stage.as_mut().unwrap().until_stop = Some(240);
    let run = rig.period(RateRatio::new(1, 1), 0, 1024, 1024);
    assert_eq!(rig.proc.stage().plans[0].0, 240);
    // (The stand-in keeps saying 240, so every call is cut there.)
    assert_eq!(run.calls, 5);
    // A stage gone after its fade: the rest of the period is one raw block.
    let mut rig = Rig::new(Stage::new(1.0));
    rig.vs.start(
        Timing::new(RATE, 1024, 1024),
        false,
        rig.proc.stage.as_mut().unwrap(),
    );
    for _ in 0..3 {
        rig.graph.trigger(RATE, 1024, 0, 1.0);
    }
    rig.graph.advance(u64::MAX / 2);
    struct Retiring(Proc, usize);
    impl EngineAudioProcessor for Retiring {
        fn process_block(
            &mut self,
            sink: SinkChoice,
            dsp_in: &[f32],
            direct_in: &[f32],
            out: &mut [i32],
            channels: usize,
            ratio: RateRatio,
            graph_rate: u32,
            in_pos: u64,
        ) {
            self.0.process_block(
                sink, dsp_in, direct_in, out, channels, ratio, graph_rate, in_pos,
            );
            self.1 += 1;
            if self.1 == 1 {
                self.0.stage = None;
            }
        }
        fn run_selftest_direct(&mut self, _in: &[f32], _out: &mut [i32]) {}
        fn poll_updates(&mut self, _s: SinkChoice, _g: u32, _r: RateRatio, _st: bool) {}
        fn latency_out_frames(&self, _s: SinkChoice) -> usize {
            0
        }
        fn reset_dsp_state(&mut self) {}
        fn varispeed(
            &mut self,
            s: SinkChoice,
            r: RateRatio,
            g: u32,
            c: usize,
        ) -> Option<&mut dyn VarispeedPort> {
            self.0.varispeed(s, r, g, c)
        }
    }
    let mut stage = Stage::new(1.0);
    stage.until_stop = Some(240);
    let mut p = Retiring(Proc::new(stage), 0);
    let run = run_period(
        &mut rig.vs,
        &mut rig.graph,
        &mut p,
        PeriodBufs {
            ring: &mut rig.ring,
            dsp_in: &mut rig.dsp_in,
            direct_in: &rig.direct_in,
            out: &mut rig.out[..1024 * CH],
        },
        &rig.slot,
        PeriodPlan {
            sink: SinkChoice::Dsp,
            ratio: RateRatio::new(1, 1),
            graph_rate: RATE,
            channels: CH,
            pos: 0,
            n_in: 1024,
            n_out: 1024,
            kind: PullKind::Play,
            prime_wait_ns: None,
            poll: false,
            quantum: 1024,
        },
        &mut rig.delay,
    )
    .unwrap();
    assert_eq!(run.calls, 2);
    let b = &p.0.blocks;
    assert_eq!((b[0].0, b[0].2), (240, true));
    assert_eq!((b[1].0, b[1].1, b[1].2, b[1].3), (784, 784, false, 240));
    assert_eq!(run.taken, 1024);
    assert!(!run.underfeed);
}

#[test]
fn a_prime_starts_at_the_commanded_speed_under_the_new_cap() {
    // Enabled while idle, a command posted: the first prime block plays at
    // it (the command is taken and the stage reset before the first plan).
    let mut rig = Rig::new(Stage::new(1.0));
    rig.slot.send(1.5, 200.0);
    rig.vs.start_priming(
        Timing::new(RATE, 1024, 1024),
        rig.proc.stage.as_mut().unwrap(),
        &rig.slot,
    );
    let run = vsl_run(
        &mut rig,
        RateRatio::new(1, 1),
        0,
        1024,
        1024,
        1024,
        Some(80 * MS),
    )
    .expect("the prime found its source");
    assert!(!run.underfeed);
    let st = rig.proc.stage();
    assert_eq!(st.resets, 1);
    assert_eq!(st.plans[0].2, 1.5, "{:?}", st.plans);
    assert_eq!(rig.vs.scheduler().status().requested, 1.5);
    // A reopen into a geometry that caps the speed (P = 1024, Q = 64 at
    // the default budget: unity): the reset already holds at the cap.
    let mut rig = Rig::new(Stage::new(1.0));
    rig.slot.send(1.5, 200.0);
    rig.vs.start_priming(
        Timing::new(RATE, 1024, 64),
        rig.proc.stage.as_mut().unwrap(),
        &rig.slot,
    );
    vsl_run(
        &mut rig,
        RateRatio::new(1, 1),
        0,
        1024,
        1024,
        64,
        Some(200 * MS),
    )
    .unwrap();
    let st = rig.proc.stage();
    assert_eq!(st.limit, 1.0);
    assert_eq!(st.plans[0].2, 1.0);
}

#[test]
fn a_hand_back_waits_for_the_first_raw_pull() {
    // P = 1024, Q = 128, s = 0.5: the watermark is 643 frames, less than
    // the 1 024 the first raw pull takes.
    let mut rig = Rig::new(Stage::new(0.5));
    rig.slot.send(0.5, 0.0);
    rig.vs.start(
        Timing::new(RATE, 1024, 128),
        false,
        rig.proc.stage.as_mut().unwrap(),
    );
    rig.vs
        .after_pull(rig.proc.stage.as_mut().unwrap(), &rig.slot);
    let wm = rig.vs.scheduler().watermark();
    assert_eq!(wm, pull_bound(0.5, 1024) + 128);
    assert!(fill_until(
        &mut rig.vs,
        &mut rig.graph,
        &rig.ring,
        CH,
        RATE,
        128,
        &rig.delay,
        0.5,
        wm,
        100 * MS,
    ));
    // Let the cycles in flight land: the ring sits at the watermark.
    rig.graph.advance(rig.graph.now + 10 * MS);
    let fill = ring_fill(&rig.ring, CH);
    assert!(fill < 1024, "{fill}");
    let h = handover(
        &mut rig.vs,
        &mut rig.graph,
        &rig.ring,
        None,
        CH,
        RATE,
        1024,
        128,
        1024,
        &rig.delay,
        10 * MS,
    );
    assert_eq!(h, Handover::HandedBack { refilled: true });
    assert!(ring_fill(&rig.ring, CH) >= 1024);
    assert!(!rig.vs.is_active());
}

#[test]
fn a_stopping_stage_sizes_the_next_pull_for_unity() {
    let mut rig = Rig::new(Stage::new(0.5));
    rig.slot.send(0.5, 0.0);
    rig.vs.start(
        Timing::new(RATE, 1024, 128),
        false,
        rig.proc.stage.as_mut().unwrap(),
    );
    rig.vs
        .after_pull(rig.proc.stage.as_mut().unwrap(), &rig.slot);
    let low = rig.vs.scheduler().watermark();
    rig.proc.stage.as_mut().unwrap().raw_next = true;
    rig.vs
        .after_pull(rig.proc.stage.as_mut().unwrap(), &rig.slot);
    let wm = rig.vs.scheduler().watermark();
    assert!(wm > low);
    assert_eq!(wm, pull_bound(1.0, 1024) + 128);
}

#[test]
fn the_delay_is_worked_out_at_each_trigger() {
    // 48 kHz, unity ratio, 100 frames of chain latency, speed 1.5.
    let mut d = VsDelay::new(RateRatio::new(1, 1), RATE);
    d.set_latency(100);
    d.dac_read(0, 2048, true);
    // 10 ms later the DAC played 480 frames.
    assert_eq!(d.dac_at(10 * MS), 1568);
    assert_eq!(
        d.at(10 * MS, 300, 1.5),
        ((1568.0 + 100.0) * 1.5_f64).round() as i64 + 300
    );
    // The block in work counts until it is written; then the DAC holds it.
    d.took(1024);
    assert_eq!(d.at(10 * MS, 0, 1.0), 1568 + 100 + 1024);
    d.wrote(1024, 1024);
    assert_eq!(d.at(10 * MS, 0, 1.0), 1568 + 100 + 1024);
    // Priming: the DAC does not run, so nothing it holds is played.
    let mut p = VsDelay::new(RateRatio::new(1, 1), RATE);
    p.set_latency(50);
    p.dac_read(0, 0, false);
    p.wrote(1024, 1024);
    assert_eq!(p.at(500 * MS, 200, 1.0), 1024 + 50 + 200);
    // Through a ratio: output frames count as input frames.
    let mut r = VsDelay::new(RateRatio::new(4, 1), RATE);
    r.dac_read(0, 4096, false);
    assert_eq!(r.at(0, 0, 1.0), 1024);
}

#[test]
fn triggers_between_dac_wakeups_publish_the_live_delay() {
    // Steady playback at 1.5 with Q = 256: several triggers per period, each
    // with the delay at its own time (the DAC drains, the ring fills).
    let mut rig = Rig::new(Stage::new(1.5));
    let period = 1024usize;
    let q = 256u64;
    rig.vs.start_priming(
        Timing::new(RATE, period as u32, q as u32),
        rig.proc.stage.as_mut().unwrap(),
        &rig.slot,
    );
    rig.delay.dac_read(rig.graph.now, 0, false);
    // Prime two periods, then the watermark.
    for p in 0..2u64 {
        let run = vsl_run(
            &mut rig,
            RateRatio::new(1, 1),
            p * 1024,
            1024,
            1024,
            q,
            Some(80 * MS),
        )
        .unwrap();
        rig.delay.wrote(1024, run.pulled.min(1024));
    }
    let wm = rig.vs.scheduler().watermark();
    fill_until(
        &mut rig.vs,
        &mut rig.graph,
        &rig.ring,
        CH,
        RATE,
        q,
        &rig.delay,
        1.5,
        wm,
        200 * MS,
    );
    rig.vs.finish_priming();
    let period_ns = period as u64 * 1_000_000_000 / u64::from(RATE);
    let mut wake = None;
    let mut t = rig.graph.now;
    let mut checked = 0;
    for k in 0..40u64 {
        // DAC wakeup: 2 periods queued.
        rig.delay.dac_read(t, 2048, true);
        rig.delay.clear_pending();
        let before = rig.graph.triggers.len();
        let run = rig.period(RateRatio::new(1, 1), (k + 2) * 1024, 1024, 1024);
        assert!(!run.underfeed, "period {k}");
        rig.delay.wrote(1024, 1024);
        wake = run.wake_at_ns.or(wake);
        idle_until(
            &mut rig.vs,
            &mut rig.graph,
            &rig.ring,
            CH,
            RATE,
            q,
            &rig.delay,
            1.5,
            &mut wake,
            t + period_ns,
        );
        // Every trigger of the period: the ring fill then plus 1.5 times
        // the DAC's queue at that instant (3 072 frames after the write,
        // minus what it played since the wakeup).
        for tr in &rig.graph.triggers[before..] {
            let played = ((tr.at_ns - t) as u128 * u128::from(RATE) / 1_000_000_000) as i64;
            let dac = (2048 + 1024 - played).max(0);
            let pending = if tr.at_ns == t { 1024 } else { 0 };
            let want = ((dac + pending) as f64 * 1.5).round() as i64 + tr.fill as i64;
            if pending == 0 {
                assert_eq!(tr.delay, want, "period {k}: {tr:?}");
                checked += 1;
            }
        }
        t += period_ns;
    }
    assert!(checked > 40, "{checked} triggers checked");
    let distinct: std::collections::BTreeSet<i64> =
        rig.graph.triggers.iter().map(|t| t.delay).collect();
    assert!(distinct.len() > 20);
}

#[test]
fn the_drain_pulls_the_ring_then_the_whole_tail() {
    let mut d = VsDrain::default();
    let mut ring = 3_000usize;
    let mut periods = 0;
    while d.more(ring, 600) {
        ring = ring.saturating_sub(1024);
        d.pulled(1024);
        periods += 1;
    }
    // 3 periods empty the ring, one more pads the 600-frame tail.
    assert_eq!(periods, 4);
    let mut d = VsDrain::default();
    assert!(d.more(0, 2_000));
    d.pulled(1024);
    assert!(d.more(0, 2_000));
    d.pulled(1024);
    assert!(!d.more(0, 2_000));
}

#[test]
fn the_drain_deadline_covers_long_periods_at_half_speed() {
    // 44.1 kHz, P = 8192 at 0.5×: two periods of ring take 0.74 s to play,
    // the device's two periods 0.37 s more.
    let ratio = RateRatio::new(1, 1);
    let ns = drain_deadline_ns(16_384, 0.5, 600, 16_384, ratio, 44_100, 8192);
    let needed = (16_384.0 / 0.5 + 600.0 + 16_384.0) / 44_100.0;
    assert!(ns as f64 / 1e9 > needed, "{ns}");
    assert!(ns > DRAIN_MIN_NS);
    // Short ones stay at the floor.
    assert_eq!(
        drain_deadline_ns(256, 1.0, 0, 256, ratio, 48_000, 256),
        DRAIN_MIN_NS
    );
}

#[test]
fn the_clock_delay_takes_the_negotiated_geometry() {
    // A stage enabled live at 48 → 192 kHz: the DAC drains at 192 kHz, so
    // 50 ms take 9600 output frames off its queue (not 2400 at 48 kHz).
    let ratio = RateRatio::new(4, 1);
    let mut d = VsDelay::default();
    d.set_geometry(ratio, 48_000);
    d.dac_read(0, 19_200, true);
    assert_eq!(d.dac_at(50_000_000), 9_600);
    let mut fresh = VsDelay::new(ratio, 48_000);
    fresh.dac_read(0, 19_200, true);
    assert_eq!(d.at(50_000_000, 100, 1.0), fresh.at(50_000_000, 100, 1.0));
}

#[test]
fn the_nested_loops_reach_the_overload_guard() {
    // A period of five stage calls with its source already buffered never
    // waits: the guard runs after each call all the same.
    let mut rig = Rig::new(Stage::new(1.0));
    rig.vs.start(
        Timing::new(RATE, 1024, 1024),
        false,
        rig.proc.stage.as_mut().unwrap(),
    );
    for _ in 0..3 {
        rig.graph.trigger(RATE, 1024, 0, 1.0);
    }
    rig.graph.advance(u64::MAX / 2);
    rig.proc.stage.as_mut().unwrap().until_stop = Some(240);
    let run = rig.period(RateRatio::new(1, 1), 0, 1024, 1024);
    assert_eq!(run.calls, 5);
    assert_eq!(rig.graph.checkpoints, 5);
    // A fill that waits for cycles: the guard runs after every wait.
    let mut rig = Rig::new(Stage::new(1.0));
    rig.vs.start(
        Timing::new(RATE, 1024, 128),
        false,
        rig.proc.stage.as_mut().unwrap(),
    );
    assert!(fill_until(
        &mut rig.vs,
        &mut rig.graph,
        &rig.ring,
        CH,
        RATE,
        128,
        &rig.delay,
        1.0,
        1024,
        100 * MS,
    ));
    assert!(rig.graph.checkpoints >= 8, "{}", rig.graph.checkpoints);
}
