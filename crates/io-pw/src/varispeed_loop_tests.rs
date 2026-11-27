//! The RT loop's varispeed steps driven offline: a simulated graph
//! ([`super::sim`]), a stand-in stage and processor.

use super::sim::SimGraph;
use super::*;

const RATE: u32 = 48_000;
const CH: usize = 2;

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
