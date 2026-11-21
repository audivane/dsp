//! The control and RT sides of an open whose graph move PipeWire may not
//! follow (another driver or client fixes the rate), run together on the
//! driver's pure pieces: the control's graph-rate plan, its bound on
//! back-to-back moves and the open plan, and the RT thread's reopen check.
//! Direct and DSP off only play at x1: a graph that never moves to a rate
//! the device plays natively ends in an error, never in a reopen loop or
//! playback into a mismatched graph.

use audivane_io_alsa::{RatioOpen, plan_open};
use audivane_io_pw::lifecycle::{
    DesiredDspConfig, DeviceRates, NegotiatedKey, decide_rt_reopen, negotiated_key,
};
use audivane_io_pw::rate_caps::{
    GraphRate, MOVE_GIVE_UP_NS, MoveAttempts, effective_rate_policy, plan_graph_rate, rate_policies,
};
use audivane_protocol::ipc::{
    DeviceCapabilities, FormatCapability, RateCapability, RatePolicy, SinkChoice,
};
use audivane_protocol::{ALLOWED_RATES, RateRatio, RateTarget, SampleFormat};

const STEP_NS: u64 = 50_000_000;
const FORMATS: [SampleFormat; 1] = [SampleFormat::S32];

fn dac(rates: &[u32]) -> DeviceCapabilities {
    DeviceCapabilities {
        device: "hw:Test".into(),
        channels: 2,
        channels_min: 2,
        channels_max: 2,
        rates: rates
            .iter()
            .map(|&rate| RateCapability {
                rate,
                formats: vec![FormatCapability {
                    format: SampleFormat::S32,
                    sbits: Some(32),
                }],
            })
            .collect(),
        rate_min: rates.iter().copied().min().unwrap_or(0),
        rate_max: rates.iter().copied().max().unwrap_or(0),
        formats: vec![SampleFormat::S32],
    }
}

struct Opened {
    graph: u32,
    ratio: RateRatio,
    key: NegotiatedKey,
}

/// The control thread's reopen for `target` (as the RT thread saw the
/// graph); `Err` when it gives up.
fn control_open(
    caps: &DeviceCapabilities,
    cfg: DesiredDspConfig,
    sink: SinkChoice,
    moves: &mut Option<MoveAttempts>,
    target: u32,
    now: u64,
    landings: u64,
) -> Result<Opened, ()> {
    let eff = effective_rate_policy(&RatePolicy::default(), caps, cfg, &FORMATS, ALLOWED_RATES)
        .unwrap()
        .policy;
    let policies = rate_policies(eff, Some(caps), &FORMATS, cfg);
    let graph = match plan_graph_rate(&policies, target, sink) {
        GraphRate::Keep => {
            *moves = None;
            target
        }
        GraphRate::Move(rate) => {
            if MoveAttempts::note(moves, target, rate, now, landings) {
                return Err(());
            }
            rate
        }
        GraphRate::DirectCannotPlay => return Err(()),
    };
    let ladder = cfg.ratio_ladder_for(sink, graph, Some(caps), &FORMATS);
    let ratio = plan_open(caps, graph, &ladder, &FORMATS).map_err(|_| ())?.0;
    let rates = DeviceRates::playable(Some(caps), &FORMATS);
    Ok(Opened {
        graph,
        ratio,
        key: negotiated_key(cfg, sink, graph, rates, &RatioOpen::at(ratio)),
    })
}

#[derive(Debug)]
struct Outcome {
    opens: usize,
    /// When the control gave up (the engine reports the error and stays
    /// there until the route or the graph rate changes).
    failed_ns: Option<u64>,
    graph: u32,
    ratio: RateRatio,
    plays: usize,
}

/// `seconds` of RT passes on `sink`, the graph running at `clock(now)`.
fn run(
    caps: &DeviceCapabilities,
    cfg: DesiredDspConfig,
    sink: SinkChoice,
    clock: impl Fn(u64) -> u32,
    seconds: u64,
) -> Outcome {
    run_every(caps, cfg, sink, clock, seconds, STEP_NS)
}

/// [`run`] with an RT pass every `step_ns` (a reopen takes that long).
fn run_every(
    caps: &DeviceCapabilities,
    cfg: DesiredDspConfig,
    sink: SinkChoice,
    clock: impl Fn(u64) -> u32,
    seconds: u64,
    step_ns: u64,
) -> Outcome {
    let mut moves = None;
    let rates = DeviceRates::playable(Some(caps), &FORMATS);
    // Opened for no rate yet: the first pass reopens for the graph's.
    let mut active = Opened {
        graph: 0,
        ratio: RateRatio::ONE,
        key: negotiated_key(cfg, sink, 0, rates, &RatioOpen::at(RateRatio::ONE)),
    };
    let mut out = Outcome {
        opens: 0,
        failed_ns: None,
        graph: 0,
        ratio: RateRatio::ONE,
        plays: 0,
    };
    let mut aborted_at: Option<u32> = None;
    // The RT thread's count of opens it saw land (`graph_landings`).
    let (mut landings, mut landed) = (0u64, false);
    let mut now = 0u64;
    while now < seconds * 1_000_000_000 {
        let cur = clock(now);
        if !landed && aborted_at.is_none() && cur == active.graph {
            landed = true;
            landings += 1;
        }
        if let Some(rate) = aborted_at {
            // After a failed open the RT thread retries only when the route
            // or the graph rate changes.
            if cur != rate {
                aborted_at = None;
                active.graph = 0;
            } else {
                now += step_ns;
                continue;
            }
        }
        let reopen = decide_rt_reopen(
            cfg,
            Some(active.key),
            sink,
            sink,
            active.graph,
            cur,
            active.ratio,
        );
        if reopen.is_some() {
            out.opens += 1;
            match control_open(caps, cfg, sink, &mut moves, cur, now, landings) {
                Ok(o) => {
                    active = o;
                    landed = false;
                }
                Err(()) => {
                    out.failed_ns.get_or_insert(now);
                    aborted_at = Some(cur);
                }
            }
        } else {
            assert_eq!(
                active.graph, cur,
                "played a {cur} Hz graph into a device opened for {}",
                active.graph
            );
            out.plays += 1;
        }
        now += step_ns;
    }
    out.graph = active.graph;
    out.ratio = active.ratio;
    out
}

fn bypass_sinks() -> [(SinkChoice, DesiredDspConfig); 2] {
    let on = DesiredDspConfig {
        rate_target: RateTarget::MaxRateAny(48_000),
        ..DesiredDspConfig::default()
    };
    let off = DesiredDspConfig {
        dsp_enabled: false,
        ..on
    };
    [(SinkChoice::Direct, on), (SinkChoice::Dsp, off)]
}

#[test]
fn a_graph_that_stays_ends_in_an_error_not_a_reopen_loop() {
    // A 48 kHz-only DAC, the graph fixed at 44.1 kHz by someone else.
    let caps = dac(&[48_000]);
    for (sink, cfg) in bypass_sinks() {
        let o = run(&caps, cfg, sink, |_| 44_100, 20);
        let failed = o.failed_ns.expect("never gave up");
        assert!(
            (MOVE_GIVE_UP_NS..=MOVE_GIVE_UP_NS + 2 * STEP_NS).contains(&failed),
            "{sink:?}: {failed}"
        );
        // Every open in that window was a move attempt; none after it.
        assert!(
            o.opens as u64 <= MOVE_GIVE_UP_NS / STEP_NS + 2,
            "{sink:?}: {}",
            o.opens
        );
        assert_eq!(o.plays, 0, "{sink:?}");
    }
}

#[test]
fn a_graph_that_follows_the_move_plays_at_x1() {
    let caps = dac(&[48_000]);
    for (sink, cfg) in bypass_sinks() {
        let o = run(
            &caps,
            cfg,
            sink,
            |now| if now < 200_000_000 { 44_100 } else { 48_000 },
            5,
        );
        assert_eq!(o.failed_ns, None, "{sink:?}");
        assert_eq!((o.graph, o.ratio), (48_000, RateRatio::ONE), "{sink:?}");
        assert!(o.opens <= 5, "{sink:?}: {}", o.opens);
        assert!(o.plays > 80, "{sink:?}");
    }
}

#[test]
fn after_the_error_a_graph_that_moves_plays_at_x1() {
    let caps = dac(&[48_000]);
    for (sink, cfg) in bypass_sinks() {
        let o = run(
            &caps,
            cfg,
            sink,
            |now| if now < 6_000_000_000 { 44_100 } else { 48_000 },
            8,
        );
        assert!(o.failed_ns.is_some(), "{sink:?}");
        assert_eq!((o.graph, o.ratio), (48_000, RateRatio::ONE), "{sink:?}");
    }
}

#[test]
fn dsp_on_keeps_a_fixed_graph_with_a_fractional_ratio() {
    // The DSP sink with DSP on plays 44.1 kHz on the 48 kHz-only DAC at
    // 160/147: one open, no move.
    let caps = dac(&[48_000]);
    let on = bypass_sinks()[0].1;
    let o = run(&caps, on, SinkChoice::Dsp, |_| 44_100, 5);
    assert_eq!(o.opens, 1);
    assert_eq!(o.failed_ns, None);
    assert_eq!((o.graph, o.ratio), (44_100, RateRatio::new(160, 147)));
}

#[test]
fn reopens_a_second_apart_still_give_up() {
    // Each reopen takes a second (a slow device): the graph stays, and the
    // open still gives up once, about 3 s after the first attempt.
    let caps = dac(&[48_000]);
    for (sink, cfg) in bypass_sinks() {
        let o = run_every(&caps, cfg, sink, |_| 44_100, 20, 1_000_000_000);
        let failed = o.failed_ns.expect("retried forever");
        assert!(
            failed <= MOVE_GIVE_UP_NS + 1_000_000_000,
            "{sink:?}: {failed}"
        );
        assert!(o.opens <= 5, "{sink:?}: {}", o.opens);
    }
}

#[test]
fn a_slow_move_recovers_once_it_lands() {
    // PipeWire follows, but only after 3.5 s: the open gives up at 3 s,
    // and the landing (a graph-rate change) brings playback back at x1.
    let caps = dac(&[48_000]);
    for (sink, cfg) in bypass_sinks() {
        let o = run(
            &caps,
            cfg,
            sink,
            |now| if now < 3_500_000_000 { 44_100 } else { 48_000 },
            6,
        );
        assert!(o.failed_ns.is_some(), "{sink:?}");
        assert_eq!((o.graph, o.ratio), (48_000, RateRatio::ONE), "{sink:?}");
        assert!(o.plays > 40, "{sink:?}: {}", o.plays);
    }
}

#[test]
fn a_landed_move_never_counts_against_a_later_one() {
    // The move lands at once; 10 s later the graph is back at 44.1 kHz and
    // pinned there. The new move gets its own 3 s, not the old deadline.
    let caps = dac(&[48_000]);
    for (sink, cfg) in bypass_sinks() {
        let o = run(
            &caps,
            cfg,
            sink,
            |now| {
                if (100_000_000..10_000_000_000).contains(&now) {
                    48_000
                } else {
                    44_100
                }
            },
            20,
        );
        let failed = o.failed_ns.expect("never gave up");
        assert!(
            failed >= 10_000_000_000 + MOVE_GIVE_UP_NS,
            "{sink:?}: {failed}"
        );
    }
}
