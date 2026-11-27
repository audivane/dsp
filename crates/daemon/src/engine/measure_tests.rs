//! Measurement playback through the real master processor (a neutral
//! chain unless said, x1, the DSP sink): where the file lands, the mute of
//! the other streams, the keepalive, the crossfades, the ceiling, the
//! drain, a stop and a fault.

use super::*;
use crate::engine::processor::{ActiveChain, DaemonProcessor, MeterBank, VolumeTarget};
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

/// The largest step between neighbouring frames of channel `c` over
/// `range`, full scale 1.
fn max_step(p: &Proc, c: usize, range: std::ops::Range<usize>) -> f64 {
    range
        .clone()
        .skip(1)
        .map(|f| (f64::from(p.frame(f, c)) - f64::from(p.frame(f - 1, c))).abs() / 2_147_483_648.0)
        .fold(0.0, f64::max)
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

/// `active`: the file enters the chain's input on its channel (the
/// neutral chain passes 24-bit values unchanged).
#[test]
fn active_plays_the_file_through_the_chain() {
    let mut p = Proc::new();
    p.blocks(2);
    let lead = 2_400u64;
    let f = file(5_000);
    let job = post(&p, MeasureChain::Active, 0.0, lead, f.clone());
    let arm = p.src as usize;
    p.blocks(20);
    let start = job.devices[0].start_frame().unwrap() as usize;
    assert_eq!(
        start,
        arm + lead as usize,
        "the neutral chain has no latency"
    );
    for (k, v) in f.iter().enumerate() {
        assert_eq!(p.frame(start + k, 1), to_i32(*v), "file frame {k}");
    }
    assert!(job.devices[0].done.load(Ordering::Acquire));
}

/// The keepalive plays at its level from the lead-in's first frame and
/// under the file.
#[test]
fn the_keepalive_runs_from_the_lead_in_to_the_files_end() {
    let mut p = Proc::new();
    p.blocks(2);
    let lead = 24_000u64;
    let ka = 1e-5;
    // A silent file: only the keepalive plays.
    let job = post(&p, MeasureChain::Bypass, ka, lead, vec![0.0; 24_000]);
    let arm = p.src as usize;
    p.blocks(100);
    let start = job.devices[0].start_frame().unwrap() as usize;
    let rms = |a: usize, b: usize| {
        let s: f64 = (a..b)
            .map(|fr| (f64::from(p.frame(fr, 1)) / 2_147_483_648.0).powi(2))
            .sum();
        (s / (b - a) as f64).sqrt()
    };
    let lead_rms = rms(arm + xfade(), start);
    let file_rms = rms(start, start + 24_000);
    assert!((lead_rms / ka - 1.0).abs() < 0.03, "lead-in {lead_rms}");
    assert!(
        (file_rms / ka - 1.0).abs() < 0.03,
        "under the file {file_rms}"
    );
    // Gone after the file and the crossfade (the muted source's silence).
    assert_eq!(p.frame(start + 24_000 + xfade(), 1), 0);
    assert!(job.devices[0].done.load(Ordering::Acquire));
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

/// The engine leaving the DSP path faults the run, and the mute lifts.
#[test]
fn leaving_the_dsp_path_faults_and_releases() {
    let mut p = Proc::new();
    p.blocks(2);
    let job = post(&p, MeasureChain::Bypass, 0.0, 2_400, vec![0.5; 96_000]);
    p.blocks(10);
    p.block_on(SinkChoice::Direct);
    assert_eq!(job.any_fault(), MeasureFault::Path);
    p.blocks(4);
    assert!(job.devices[0].done.load(Ordering::Acquire));
    job.release.store(true, Ordering::Release);
    p.blocks(4);
    assert!(job.released.load(Ordering::Acquire));
    let end = p.out.len() / CH - 1;
    assert_eq!(p.frame(end, 0), to_i32(f64::from(source(end as u64))));
}

/// Bypass keeps the master's digital volume.
#[test]
fn bypass_keeps_the_digital_volume() {
    let mut p = Proc::new();
    let vol = p.p.volume_handle();
    vol.store(VolumeTarget {
        volume_db: -6.0,
        balance_db: 0.0,
        muted: false,
        hardware_volume: false,
        hardware_mute: false,
    });
    p.blocks(4);
    let job = post(&p, MeasureChain::Bypass, 0.0, 2_400, vec![0.5; 4_800]);
    p.blocks(20);
    let start = job.devices[0].start_frame().unwrap() as usize;
    let g = 10f64.powf(-6.0 / 20.0);
    for fr in start..start + 4_800 {
        let x = f64::from(p.frame(fr, 1)) / 2_147_483_648.0;
        assert!((x - 0.5 * g).abs() < 1e-6, "frame {fr}: {x}");
    }
}

/// Bypass over music (no mute): the routed channel crossfades from the
/// music into the lead-in and back after the file, with no step larger
/// than the music's own.
#[test]
fn bypass_over_music_crossfades_in_and_out() {
    let mut p = Proc::new();
    p.blocks(4);
    let f = vec![0.0; 9_600];
    let job = post_with(&p, MeasureChain::Bypass, false, 0.0, 4_800, f);
    let arm = p.src as usize;
    p.blocks(40);
    let start = job.devices[0].start_frame().unwrap() as usize;
    // The tone's own largest step (0.25 at 1 kHz, 48 kHz).
    let tone = 0.25 * 2.0 * PI * 1000.0 / f64::from(RATE);
    let around_in = max_step(&p, 1, arm.saturating_sub(64)..arm + 2 * xfade());
    let end = start + 9_600;
    let around_out = max_step(&p, 1, end - 64..end + 2 * xfade());
    assert!(
        around_in <= tone * 1.1,
        "in: {around_in} vs the tone's {tone}"
    );
    assert!(
        around_out <= tone * 1.1,
        "out: {around_out} vs the tone's {tone}"
    );
    // Then the music again, unchanged; the other channel never moved.
    for fr in end + xfade() + 1..end + xfade() + 500 {
        assert_eq!(p.frame(fr, 1), to_i32(f64::from(source(fr as u64))));
    }
    for fr in arm..end {
        assert_eq!(p.frame(fr, 0), to_i32(f64::from(source(fr as u64))));
    }
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

/// The continuity check: a run whose DAC readings are steady reports no
/// jump; one whose queue reading steps by 800 frames mid-file (the DAC's
/// position runs 800 frames ahead from there) is reported, with its size
/// and its time from the file's start.
#[test]
fn a_step_in_the_dac_position_is_reported() {
    use crate::control::measure::{continuity, continuity_note};
    let run = |step_at: Option<usize>| {
        let mut p = Proc::new();
        p.blocks(2);
        let job = post(&p, MeasureChain::Bypass, 0.0, 4_800, vec![0.25; 48_000]);
        for b in 0..120 {
            if step_at == Some(b) {
                p.queue = QUEUE - 800;
            }
            p.blocks(1);
        }
        let d = &job.devices[0];
        assert!(d.done.load(Ordering::Acquire));
        let r = d.readings.snapshot();
        assert!(r.len() > 90, "{} readings", r.len());
        let t0 = d.start_time_ns.load(Ordering::Acquire);
        assert!(t0 > 0);
        continuity(&r, RATE, Some(t0), d.readings.dropped()).unwrap()
    };
    let clean = run(None);
    // (The readings' times are whole ns.)
    assert!(clean.max_residual_frames < 1e-3, "{clean:?}");
    assert!(clean.jump_frames.abs() < 1e-3, "{clean:?}");
    assert_eq!(continuity_note("master", &clean), None);

    let stepped = run(Some(60));
    assert!((stepped.jump_frames - 800.0).abs() < 2.0, "{stepped:?}");
    assert!((stepped.jump_us - 800.0 / 48e3 * 1e6).abs() < 50.0);
    // Block 60 (plus the two before the post) ends 62 blocks in; the file
    // starts after the 4800-frame lead-in, from the post's block.
    let expected = (62.0 * BLOCK as f64 - 2.0 * BLOCK as f64 - 4_800.0) / f64::from(RATE);
    assert!(
        (stepped.jump_at_s - expected).abs() < 2.0 * BLOCK as f64 / f64::from(RATE),
        "at {} s, expected about {expected} s",
        stepped.jump_at_s
    );
    assert!(stepped.max_residual_frames > 300.0);
    let note = continuity_note("master", &stepped).unwrap();
    assert!(note.contains("jumped by 800 frames"), "{note}");
}

/// `active`: a chain swap while the run plays ends it with a fault (the
/// file would cross from one chain to the other); in `bypass` a swap that
/// keeps the latency does not touch the file.
#[test]
fn a_chain_swap_during_a_run_faults_it() {
    use crate::engine::processor::PendingSwap;
    for (mode, faults) in [(MeasureChain::Active, true), (MeasureChain::Bypass, false)] {
        let mut p = Proc::new();
        p.blocks(2);
        let job = post(&p, mode, 1e-5, 2_400, vec![0.25; 24_000]);
        p.blocks(10);
        assert_eq!(job.any_fault(), MeasureFault::None);
        let new = ActiveChain::Single(Box::new(chain(ChainConfig::neutral())));
        assert!(
            p._keep
                .1
                .push(PendingSwap {
                    chain: new,
                    crossfade: true,
                    generation: 1,
                    swap_seq: 1,
                })
                .is_ok()
        );
        p.blocks(60);
        let d = &job.devices[0];
        assert!(d.done.load(Ordering::Acquire), "{mode:?}");
        if faults {
            assert_eq!(job.any_fault(), MeasureFault::ChainSwap);
        } else {
            assert_eq!(job.any_fault(), MeasureFault::None);
        }
    }
}

/// `active`: an EQ edit before a convolution, taken over in place (no
/// warm-up, a fade at the convolution's input) while the run plays, ends
/// it with a fault as any swap does.
#[test]
fn an_in_place_takeover_during_a_run_faults_it() {
    use crate::engine::processor::PendingSwap;
    let conv = |preamp_db: f64| {
        let mut ir = vec![0.0; 4_096];
        ir[0] = 0.8;
        let mut config = ChainConfig::neutral();
        config.eq = Some(audivane_protocol::EqProfile {
            preamp_db,
            ..Default::default()
        });
        config.convolution = vec![audivane_protocol::ConvolutionConfig {
            name: None,
            enabled: true,
            ir_path: Default::default(),
            gain_db: 0.0,
            matrix: None,
            calibration: true,
        }];
        Chain::new(ChainSpec {
            config,
            sample_rate: RATE,
            channels: CH,
            max_block: BLOCK,
            irs: Some(audivane_dsp_core::matrix::ConvIrs::Channels(vec![ir])),
        })
    };
    let mut p = Proc::with_active(ActiveChain::Single(Box::new(conv(-3.0))));
    p.blocks(2);
    let job = post(&p, MeasureChain::Active, 1e-5, 2_400, vec![0.25; 24_000]);
    p.blocks(10);
    assert_eq!(job.any_fault(), MeasureFault::None);
    assert!(
        p._keep
            .1
            .push(PendingSwap {
                chain: ActiveChain::Single(Box::new(conv(-4.0))),
                crossfade: true,
                generation: 1,
                swap_seq: 1,
            })
            .is_ok()
    );
    p.blocks(1);
    assert_eq!(p.p.active_generation(), 1, "taken over at once");
    p.blocks(60);
    assert!(job.devices[0].done.load(Ordering::Acquire));
    assert_eq!(job.any_fault(), MeasureFault::ChainSwap);
}

/// `active`: a channel worker that misses its deadline while the run plays
/// (the chain plays silence, then resets) ends the run with a fault.
#[test]
fn a_late_channel_worker_during_a_run_faults_it() {
    use audivane_dsp_core::workers::LanePool;
    use std::time::Duration;
    let pool = LanePool::spawn(1, |_| {}).unwrap();
    assert!(pool.wait_started(Duration::from_secs(10)));
    let mut cfg = ChainConfig::neutral();
    cfg.eq = Some(audivane_protocol::EqProfile {
        preamp_db: -1.0,
        bands: vec![audivane_protocol::Band {
            kind: audivane_protocol::FilterType::Peaking,
            freq_hz: 1000.0,
            gain_db: 2.0,
            q: 1.0,
            enabled: true,
        }],
        ..Default::default()
    });
    let mut c = chain(cfg);
    assert!(c.attach_workers(&pool), "the chain runs on the worker");
    let mut p = Proc::with_active(ActiveChain::Single(Box::new(c)));
    p.blocks(4);
    let job = post(&p, MeasureChain::Active, 1e-5, 2_400, vec![0.25; 24_000]);
    p.blocks(10);
    assert_eq!(job.any_fault(), MeasureFault::None);
    // The worker sleeps far past its deadline (two blocks, 21 ms).
    pool.stall_next(0, Duration::from_millis(200));
    p.blocks(3);
    assert_eq!(
        pool.stats()
            .late_blocks
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
    assert_eq!(job.any_fault(), MeasureFault::Worker);
    p.blocks(10);
    assert!(job.devices[0].done.load(Ordering::Acquire));
    // Let the stalled lane come back before the pool goes.
    std::thread::sleep(Duration::from_millis(250));
}

/// `active` through a steep low-pass (a sub's calibration): the keepalive
/// at the chain's input comes out some 20 dB down, so the keepalive after
/// the chain carries the routed channel at the set level, white (not the
/// chain's low-passed noise).
#[test]
fn the_active_keepalive_survives_a_low_pass_chain() {
    let lp = |freq_hz| audivane_protocol::Band {
        kind: audivane_protocol::FilterType::LowPass,
        freq_hz,
        gain_db: 0.0,
        q: std::f64::consts::FRAC_1_SQRT_2,
        enabled: true,
    };
    let mut cfg = ChainConfig::neutral();
    cfg.eq = Some(audivane_protocol::EqProfile {
        bands: vec![lp(60.0), lp(60.0), lp(60.0), lp(60.0)],
        ..Default::default()
    });
    let mut p = Proc::with_chain(cfg);
    p.blocks(2);
    let ka = 1e-5;
    let lead = 48_000u64;
    let job = post(&p, MeasureChain::Active, ka, lead, vec![0.0; 9_600]);
    let arm = p.src as usize;
    p.blocks(140);
    let d = &job.devices[0];
    assert!(d.done.load(Ordering::Acquire));
    assert_eq!(job.any_fault(), MeasureFault::None);
    let x = |fr: usize| f64::from(p.frame(fr, 1)) / 2_147_483_648.0;
    let rms = |a: usize, b: usize, f: &dyn Fn(usize) -> f64| {
        ((a..b).map(|fr| f(fr).powi(2)).sum::<f64>() / (b - a) as f64).sqrt()
    };
    // The lead-in past the ramp, and under the (silent) file.
    let (a, b) = (arm + 4 * xfade(), arm + lead as usize + 9_600);
    let level = rms(a, b, &x);
    assert!(
        (level / ka - 1.0).abs() < 0.05,
        "{:.1} dBFS",
        20.0 * level.log10()
    );
    // White: neighbouring samples differ by sqrt(2) times the level (the
    // low-passed noise alone would barely move between them).
    let diff = rms(a + 1, b, &|fr| x(fr) - x(fr - 1));
    assert!(
        (diff / (level * 2f64.sqrt()) - 1.0).abs() < 0.05,
        "diff {diff}, level {level}"
    );
    // Gone after the measurement's last frame.
    let last = d.last_frame().unwrap() as usize;
    let after = rms(last, last + 2_000, &x);
    assert!(after < ka * 0.05, "after the end {after}");
}

/// A worker hook that sleeps once, on its second turn after `arm` is set
/// to 2: after the job it runs next, so the job after that one (the
/// incoming chain's, in the same block) misses its deadline while the
/// playing chain's came back in time.
struct SleepAfterNext(Arc<std::sync::atomic::AtomicU32>);

impl audivane_dsp_core::workers::WorkerHook for SleepAfterNext {
    fn between_jobs(&mut self) {
        use std::sync::atomic::Ordering::SeqCst;
        if self.0.load(SeqCst) > 0 && self.0.fetch_sub(1, SeqCst) == 1 {
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }
}

/// `active`: a late worker on a chain that warms up beside the playing one
/// (unheard: its swap has not started fading) leaves the run `done`; only
/// the playing chain's own misses count.
#[test]
fn a_late_worker_on_a_warming_chain_leaves_the_run_done() {
    use crate::engine::processor::PendingSwap;
    use audivane_dsp_core::workers::LanePool;
    use std::sync::atomic::AtomicU32;
    use std::time::Duration;
    let arm = Arc::new(AtomicU32::new(0));
    let hook = Arc::clone(&arm);
    let pool = LanePool::spawn(1, move |_| SleepAfterNext(Arc::clone(&hook))).unwrap();
    assert!(pool.wait_started(Duration::from_secs(10)));
    let eq = || {
        let mut cfg = ChainConfig::neutral();
        cfg.eq = Some(audivane_protocol::EqProfile {
            preamp_db: -1.0,
            bands: vec![audivane_protocol::Band {
                kind: audivane_protocol::FilterType::Peaking,
                freq_hz: 1000.0,
                gain_db: 2.0,
                q: 1.0,
                enabled: true,
            }],
            ..Default::default()
        });
        cfg
    };
    let mut playing = chain(eq());
    assert!(playing.attach_workers(&pool));
    // The incoming chain has a convolution: it has latency, so it warms
    // up for a few blocks before its swap is heard.
    let mut cfg = eq();
    cfg.convolution = vec![audivane_protocol::ConvolutionConfig {
        name: Some("ir".into()),
        enabled: true,
        ir_path: "unused".into(),
        gain_db: 0.0,
        matrix: None,
        calibration: false,
    }];
    let mut incoming = Chain::new(ChainSpec {
        config: cfg,
        sample_rate: RATE,
        channels: CH,
        max_block: BLOCK,
        irs: Some(vec![vec![0.5; 256]].into()),
    });
    assert!(incoming.latency_out_frames() > 0);
    assert!(incoming.attach_workers(&pool));
    let mut p = Proc::with_active(ActiveChain::Single(Box::new(playing)));
    // A long queue: the DAC drains well after the output has passed the
    // file.
    p.queue = 8 * BLOCK as i64;
    p.blocks(4);
    let job = post(&p, MeasureChain::Active, 1e-5, 2_400, vec![0.25; 9_600]);
    let d = &job.devices[0];
    while d
        .last_frame()
        .is_none_or(|e| (p.out.len() / CH) as f64 <= e)
    {
        p.blocks(1);
        assert!(p.src < 200 * BLOCK as u64);
    }
    // The worker idle again, the swap arrives and the incoming chain's
    // first warm-up block misses its deadline.
    std::thread::sleep(Duration::from_millis(20));
    arm.store(2, std::sync::atomic::Ordering::SeqCst);
    assert!(
        p._keep
            .1
            .push(PendingSwap {
                chain: ActiveChain::Single(Box::new(incoming)),
                crossfade: true,
                generation: 1,
                swap_seq: 1,
            })
            .is_ok()
    );
    p.blocks(1);
    let late = || {
        pool.stats()
            .late_blocks
            .load(std::sync::atomic::Ordering::Relaxed)
    };
    assert_eq!(late(), 1, "the warming chain missed");
    assert!(d.live(), "the run still plays here");
    // Drained on the next reading, before the swap is heard.
    p.queue = 0;
    p.blocks(1);
    assert!(d.done.load(Ordering::Acquire));
    std::thread::sleep(Duration::from_millis(250));
    p.blocks(10);
    assert_eq!(job.any_fault(), MeasureFault::None);
}

/// `active` on a flat chain, 32-bit and 16-bit: the routed channel's
/// keepalive is the set level (one copy, after the chain), and the 16-bit
/// word gets no dither of the measurement's own on top (the noise, about
/// half an LSB there, dithers itself).
#[test]
fn the_active_keepalive_plays_at_its_level_on_a_flat_chain() {
    for bits in [32u32, 16] {
        let mut cfg = ChainConfig::neutral();
        cfg.output.bits = bits;
        cfg.output.dither = audivane_protocol::DitherKind::None;
        let mut p = Proc::with_chain(cfg);
        p.blocks(2);
        let ka = 1e-5;
        let lead = 48_000u64;
        let job = post(&p, MeasureChain::Active, ka, lead, vec![0.0; 24_000]);
        let arm = p.src as usize;
        p.blocks(160);
        assert!(job.devices[0].done.load(Ordering::Acquire));
        let (a, b) = (arm + 4 * xfade(), arm + lead as usize + 24_000);
        let rms = ((a..b)
            .map(|fr| (f64::from(p.frame(fr, 1)) / 2_147_483_648.0).powi(2))
            .sum::<f64>()
            / (b - a) as f64)
            .sqrt();
        let db = 20.0 * rms.log10();
        assert!((db + 100.0).abs() < 1.0, "{bits}-bit: {db:.2} dBFS");
        if bits == 16 {
            let lsb = 1 << 16;
            assert!((a..b).all(|fr| p.frame(fr, 1) % lsb == 0), "on the word");
        }
        // The unrouted channel (muted source) carries none.
        assert!((a..b).all(|fr| p.frame(fr, 0) == 0));
    }
}
