//! Measurement playback through the real master processor (a neutral
//! chain unless said, x1, the DSP sink): where the file lands, the mute of
//! the other streams, the keepalive, the crossfades, the ceiling, the
//! drain, a stop and a fault.

use super::*;
use crate::engine::processor::{ActiveChain, DaemonProcessor, MeterBank};
use audivane_dsp_core::chain::{Chain, ChainSpec};
use audivane_io_pw::EngineAudioProcessor;
use audivane_protocol::ChainConfig;
use std::sync::atomic::AtomicU8;

const CH: usize = 2;
const RATE: u32 = 48_000;
const BLOCK: usize = 512;
/// The simulated device queue the DAC readings report, frames.
const QUEUE: i64 = 1024;

struct Proc {
    p: DaemonProcessor,
    slot: Arc<MeasureSlot>,
    /// The source position (and the processor's `in_pos`).
    src: u64,
    /// Every output frame (interleaved).
    out: Vec<i32>,
    /// The queue the DAC readings report (a step in it moves the DAC's
    /// position).
    queue: i64,
    _keep: (
        rtrb::Producer<crate::engine::processor::RtDspCmd>,
        rtrb::Producer<crate::engine::processor::PendingSwap>,
        rtrb::Consumer<ActiveChain>,
    ),
}

/// The source: a 24-bit-exact tone on both channels, so a mute shows.
fn source(i: u64) -> f32 {
    let v = (0.25 * (2.0 * PI * 1000.0 * i as f64 / f64::from(RATE)).sin() * 8_388_608.0).round();
    (v / 8_388_608.0) as f32
}

impl Proc {
    fn new() -> Self {
        Self::with_chain(ChainConfig::neutral())
    }

    fn with_chain(config: ChainConfig) -> Self {
        Self::with_active(ActiveChain::Single(Box::new(chain(config))))
    }

    fn with_active(chain: ActiveChain) -> Self {
        let (cmd_tx, cmd_rx) = rtrb::RingBuffer::new(8);
        let (swap_tx, swap_rx) = rtrb::RingBuffer::new(8);
        let (recycle_tx, recycle_rx) = rtrb::RingBuffer::new(8);
        let slot = Arc::new(MeasureSlot::default());
        let p = DaemonProcessor::new(
            chain,
            true,
            CH,
            BLOCK,
            16,
            RATE,
            Arc::new(AtomicU8::new(0)),
            MeterBank::new(CH),
            cmd_rx,
            swap_rx,
            recycle_tx,
        )
        .with_measure(Arc::clone(&slot), CH);
        Self {
            p,
            slot,
            src: 0,
            out: Vec::new(),
            queue: QUEUE,
            _keep: (cmd_tx, swap_tx, recycle_rx),
        }
    }

    fn block_on(&mut self, sink: SinkChoice) {
        self.p.poll_updates(sink, RATE, RateRatio::ONE, true);
        let input: Vec<f32> = (0..BLOCK as u64)
            .flat_map(|i| {
                let x = source(self.src + i);
                [x, x]
            })
            .collect();
        let mut out = vec![0i32; BLOCK * CH];
        self.p.process_block(
            sink,
            &input,
            &input,
            &mut out,
            CH,
            RateRatio::ONE,
            RATE,
            self.src,
        );
        self.src += BLOCK as u64;
        self.out.extend_from_slice(&out);
        // The DAC reads after each period, `queue` frames behind.
        self.p
            .dac_read(self.queue, self.src * 1_000_000_000 / u64::from(RATE));
    }

    fn blocks(&mut self, n: usize) {
        for _ in 0..n {
            self.block_on(SinkChoice::Dsp);
        }
    }

    fn frame(&self, f: usize, c: usize) -> i32 {
        self.out[f * CH + c]
    }
}

fn chain(config: ChainConfig) -> Chain {
    Chain::new(ChainSpec {
        config,
        sample_rate: RATE,
        channels: CH,
        max_block: BLOCK,
        irs: None,
    })
}

/// A file of 24-bit-exact values (a ramp of odd steps, never zero).
fn file(n: usize) -> Vec<f64> {
    (0..n)
        .map(|i| (((i * 7919) % 4_000_001) as f64 - 2_000_000.0 + 0.5).round() / 8_388_608.0)
        .collect()
}

fn post(
    p: &Proc,
    chain: MeasureChain,
    keepalive: f64,
    lead: u64,
    samples: Vec<f64>,
) -> Arc<MeasureJob> {
    post_with(p, chain, true, keepalive, lead, samples)
}

fn post_with(
    p: &Proc,
    chain: MeasureChain,
    mute_others: bool,
    keepalive: f64,
    lead: u64,
    samples: Vec<f64>,
) -> Arc<MeasureJob> {
    let job = Arc::new(MeasureJob::new(
        chain,
        mute_others,
        keepalive,
        lead,
        RATE,
        vec![MeasureDevice::new(None, RATE, RATE, vec![(1, samples)], lead).with_readings(4096)],
    ));
    p.slot.post(Some((Arc::clone(&job), 0)));
    job
}

/// The crossfade, frames at the test rate.
fn xfade() -> usize {
    (XFADE_MS * 1e-3 * f64::from(RATE)) as usize
}

fn to_i32(x: f64) -> i32 {
    (x * 2_147_483_648.0).round() as i32
}

/// The file reaches only its routed channel, sample for sample (bypass at
/// unity volume), after exactly the lead-in; the other channel is muted
/// while it plays and comes back after the release.
#[test]
fn bypass_plays_the_file_exactly_on_the_routed_channel_only() {
    let mut p = Proc::new();
    p.blocks(4);
    let lead = 4_800u64;
    let f = file(20_000);
    let job = post(&p, MeasureChain::Bypass, 0.0, lead, f.clone());
    let arm_block = p.src as usize;
    p.blocks(60);
    let d = &job.devices[0];
    assert!(d.done.load(Ordering::Acquire));
    let s0 = job.s0().unwrap();
    assert_eq!(s0, arm_block as u64 + lead);
    let start = d.start_frame().unwrap() as usize;
    assert_eq!(start, arm_block + lead as usize);
    for (k, v) in f.iter().enumerate() {
        assert_eq!(p.frame(start + k, 1), to_i32(*v), "file frame {k}");
    }
    // The lead-in (no keepalive): silence on the routed channel, after
    // the crossfade from the (muting) source.
    for fr in arm_block + xfade()..start {
        assert_eq!(p.frame(fr, 1), 0, "lead-in frame {fr}");
    }
    // Drained: the queue played past the crossfade after the file.
    assert!(d.last_frame().unwrap() as usize >= start + f.len() + xfade() - 1);
    assert!(
        !job.released.load(Ordering::Acquire),
        "the mute waits for the release"
    );
    // The other channel: muted after the 10 ms ramp, for the whole file.
    let ramp = (MUTE_RAMP_MS * 1e-3 * f64::from(RATE)) as usize;
    for fr in arm_block + ramp + 1..start + f.len() {
        assert_eq!(p.frame(fr, 0), 0, "muted frame {fr}");
    }
    // Released: the source comes back (ramped), the job reports it.
    job.release.store(true, Ordering::Release);
    let before = p.out.len() / CH;
    p.blocks(4);
    assert!(job.released.load(Ordering::Acquire));
    let after = before + ramp + BLOCK;
    for fr in after..p.out.len() / CH {
        assert_eq!(
            p.frame(fr, 0),
            to_i32(f64::from(source(fr as u64))),
            "frame {fr}"
        );
        assert_eq!(
            p.frame(fr, 1),
            to_i32(f64::from(source(fr as u64))),
            "frame {fr}"
        );
    }
    // Taken back: the processor hands its box back, the control side
    // frees it.
    p.slot.post(None);
    p.blocks(1);
    assert_eq!(
        Arc::strong_count(&job),
        2,
        "never freed on the audio thread"
    );
    p.slot.reclaim();
    assert_eq!(Arc::strong_count(&job), 1);
}

/// A stop fades the file out; the device drains; the mute lifts on the
/// release.
#[test]
fn a_stop_fades_out_and_drains() {
    let mut p = Proc::new();
    p.blocks(2);
    let job = post(&p, MeasureChain::Bypass, 0.0, 2_400, vec![0.5; 96_000]);
    p.blocks(20);
    assert!(!job.devices[0].done.load(Ordering::Acquire));
    let at = p.out.len() / CH;
    assert_eq!(p.frame(at - 1, 1), to_i32(0.5));
    assert_eq!(p.frame(at - 1, 0), 0);
    job.cancel.store(true, Ordering::Release);
    p.blocks(2);
    let d = &job.devices[0];
    assert!(d.fed.load(Ordering::Acquire));
    assert!(!d.done.load(Ordering::Acquire), "still in the queue");
    p.blocks(2);
    assert!(d.done.load(Ordering::Acquire));
    // The file faded over 5 ms (no step), then the muted source.
    let fade = (STOP_FADE_MS * 1e-3 * f64::from(RATE)) as usize;
    let x: Vec<i32> = (at..at + fade + 2).map(|fr| p.frame(fr, 1)).collect();
    assert!(x.windows(2).all(|w| w[1] <= w[0]), "fades down");
    assert_eq!(*x.last().unwrap(), 0);
    let end = p.out.len() / CH - 1;
    assert_eq!(p.frame(end, 0), 0, "muted until the release");
    job.release.store(true, Ordering::Release);
    p.blocks(4);
    assert!(job.released.load(Ordering::Acquire));
    let end = p.out.len() / CH - 1;
    assert_eq!(p.frame(end, 0), to_i32(f64::from(source(end as u64))));
}

/// `active` with a +12 dB preamp and no limiter: a -6 dBFS file would
/// reach +6 dBFS; the output stays at the ceiling, and counts it.
#[test]
fn active_without_a_limiter_stays_under_the_ceiling() {
    let mut cfg = ChainConfig::neutral();
    cfg.eq = Some(audivane_protocol::EqProfile {
        preamp_db: 12.0,
        ..Default::default()
    });
    let mut p = Proc::with_chain(cfg);
    p.blocks(2);
    let f = vec![0.5; 9_600];
    let job = post(&p, MeasureChain::Active, 1e-5, 2_400, f);
    p.blocks(40);
    let ceiling = (CEILING * 2_147_483_648.0) as i32;
    let start = job.devices[0].start_frame().unwrap() as usize;
    let peak = (start..start + 9_600)
        .flat_map(|fr| [p.frame(fr, 0).abs(), p.frame(fr, 1).abs()])
        .max()
        .unwrap();
    assert_eq!(peak, ceiling);
    assert!(job.devices[0].clamped.load(Ordering::Relaxed) >= 9_600);
    assert!(job.devices[0].done.load(Ordering::Acquire));
}

/// `bypass` with the music on (no mute) through a +12 dB preamp and no
/// limiter: while the measurement runs, the master's whole output stays
/// at the ceiling, the unrouted channel included.
#[test]
fn bypass_clamps_the_whole_output_while_it_runs() {
    let mut cfg = ChainConfig::neutral();
    cfg.eq = Some(audivane_protocol::EqProfile {
        preamp_db: 12.0,
        ..Default::default()
    });
    let mut p = Proc::with_chain(cfg);
    p.blocks(2);
    let job = post_with(
        &p,
        MeasureChain::Bypass,
        false,
        0.0,
        2_400,
        vec![0.25; 4_800],
    );
    let arm = p.src as usize;
    p.blocks(20);
    let d = &job.devices[0];
    assert!(d.done.load(Ordering::Acquire));
    let ceiling = (CEILING * 2_147_483_648.0) as i32;
    let last = d.last_frame().unwrap() as usize;
    let peak = (arm..last).map(|fr| p.frame(fr, 0).abs()).max().unwrap();
    assert_eq!(peak, ceiling, "the music on channel 0 (0.25 at +12 dB)");
    assert!(d.clamped.load(Ordering::Relaxed) > 0);
}
