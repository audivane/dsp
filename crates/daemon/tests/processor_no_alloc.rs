//! Counting-allocator verification that `DaemonProcessor` performs zero heap
//! allocations or deallocations on the RT audio thread (`poll_updates`,
//! `process_block`, `reset_dsp_state`, `latency_out_frames`), even when the
//! recycle queue is full ("reaper paused") and long-filter swaps are spammed.

use audivane_dsp_core::chain::{Chain, ChainSpec};
use audivane_dsp_daemon::engine::processor::{
    ActiveChain, DaemonProcessor, MeterBank, PendingSwap, RtDspCmd,
};
use audivane_dsp_daemon::engine::spectrum::SpectrumRt;
use audivane_io_pw::EngineAudioProcessor;
use audivane_protocol::RateRatio;
use audivane_protocol::ipc::{AbSide, SinkChoice};
use audivane_protocol::{Band, ChainConfig, ConvolutionConfig, EqProfile, FilterType, RateTarget};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::f64::consts::PI;
use std::sync::Arc;
use std::sync::atomic::AtomicU8;

struct Counting;

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static COUNT: Cell<usize> = const { Cell::new(0) };
    /// A channel worker's thread (counted in [`WORKER_COUNT`]).
    static WORKER: Cell<bool> = const { Cell::new(false) };
}

/// Heap operations on channel worker threads since their start.
static WORKER_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn note() {
    let _ = ARMED.try_with(|armed| {
        if armed.get() {
            COUNT.with(|c| c.set(c.get() + 1));
        }
    });
    let _ = WORKER.try_with(|worker| {
        if worker.get() {
            WORKER_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    });
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note();
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note();
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        note();
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note();
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn assert_no_alloc<R>(what: &str, f: impl FnOnce() -> R) -> R {
    COUNT.with(|c| c.set(0));
    ARMED.with(|a| a.set(true));
    let result = f();
    ARMED.with(|a| a.set(false));
    let count = COUNT.with(Cell::get);
    assert_eq!(
        count, 0,
        "{what}: {count} heap alloc/dealloc operations on RT path"
    );
    result
}

fn make_eq_chain(preamp_db: f64, ratio: u32, taps: usize, conv: bool) -> ActiveChain {
    ActiveChain::Single(Box::new(Chain::new(ChainSpec {
        config: ChainConfig {
            eq: Some(EqProfile {
                name: "test".into(),
                preamp_db,
                bands: vec![Band {
                    kind: FilterType::Peaking,
                    freq_hz: 1000.0,
                    gain_db: 2.0,
                    q: 1.0,
                    enabled: true,
                }],
                left: Some(audivane_protocol::ChannelEq {
                    enabled: true,
                    preamp_db: preamp_db - 0.5,
                    bands: vec![Band {
                        kind: FilterType::Peaking,
                        freq_hz: 250.0,
                        gain_db: -1.0,
                        q: 1.5,
                        enabled: true,
                    }],
                }),
                right: Some(audivane_protocol::ChannelEq {
                    enabled: true,
                    preamp_db: preamp_db - 1.0,
                    bands: vec![Band {
                        kind: FilterType::Peaking,
                        freq_hz: 2000.0,
                        gain_db: -2.0,
                        q: 1.0,
                        enabled: true,
                    }],
                }),
                ..Default::default()
            }),
            convolution: if conv {
                vec![ConvolutionConfig {
                    name: Some("ir".into()),
                    enabled: true,
                    ir_path: "unused".into(),
                    gain_db: 0.0,
                    matrix: None,
                    calibration: false,
                }]
            } else {
                Vec::new()
            },
            upsampler: audivane_dsp_core::resample::ManualConfig {
                target: if ratio <= 1 {
                    RateTarget::Off
                } else {
                    RateTarget::Ratio(ratio)
                },
                taps,
                ..Default::default()
            }
            .into_upsampler_config()
            .unwrap(),
            ..ChainConfig::neutral()
        },
        sample_rate: 48_000,
        channels: 2,
        max_block: 1024,
        irs: if conv {
            // A compact IR (a short decay, as a calibration's): an EQ
            // edit's fade bound lets it take the playing chain over.
            Some(
                vec![
                    (0..2048)
                        .map(|i| 0.1 * 0.9f64.powi(i))
                        .collect::<Vec<f64>>(),
                ]
                .into(),
            )
        } else {
            None
        },
    })))
}

#[test]
fn rt_processor_never_allocates_or_drops_chains_when_recycle_queue_is_full() {
    let (_cmd_tx, cmd_rx) = rtrb::RingBuffer::new(16);
    let (mut swap_tx, swap_rx) = rtrb::RingBuffer::new(32);
    // Intentionally tiny recycle queue (cap = 2) with reaper "paused" (not draining).
    let (recycle_tx, mut recycle_rx) = rtrb::RingBuffer::new(2);
    let ab_side = Arc::new(AtomicU8::new(0));
    let meters = MeterBank::new(2);

    let mut proc = DaemonProcessor::new(
        make_eq_chain(0.0, 1, 16, false),
        true,
        2,
        1024,
        16,
        48_000,
        ab_side,
        meters,
        cmd_rx,
        swap_rx,
        recycle_tx,
    );

    // Build 10 replacement chains off-RT before arming the RT allocator check.
    let mut queued = Vec::new();
    for i in 0..10 {
        queued.push(PendingSwap {
            chain: make_eq_chain(-(i as f64) * 0.5, 1, 16, false),
            crossfade: true,
            generation: (i + 1) as u64,
            swap_seq: (i + 1) as u64,
        });
    }
    for swap in queued {
        swap_tx.push(swap).unwrap();
    }

    let inp = vec![0.1_f32; 512 * 2];
    let mut out = vec![0_i32; 512 * 2];

    // Reaper is PAUSED (`recycle_rx` not popped). Spam RT processing across all swaps.
    // Once `recycle_tx` (2) and bounded RT `recycle_stash` (3 used, 1 reserve) fill,
    // RT stops popping `swap_rx` rather than dropping any `ActiveChain`.
    assert_no_alloc("paused-reaper spammed swaps", || {
        for _ in 0..40 {
            proc.poll_updates(SinkChoice::Dsp, 48_000, RateRatio::integer(1), true);
            proc.process_block(
                SinkChoice::Dsp,
                &inp,
                &inp,
                &mut out,
                2,
                RateRatio::integer(1),
                48_000,
                0,
            );
        }
    });

    assert_eq!(
        proc.recycle_stash_len(),
        3,
        "RT stash should hold rejected chains while reserving 1 emergency slot"
    );
    assert!(
        swap_tx.slots() < 32,
        "remaining swaps must stay paused safely in swap_rx until recycle capacity frees (slots={})",
        swap_tx.slots()
    );

    // Unpause reaper off-RT in rounds and confirm every remaining swap finishes with 0 RT alloc/dealloc.
    let mut reaped = 0usize;
    for _ in 0..12 {
        while let Ok(old) = recycle_rx.pop() {
            drop(old);
            reaped += 1;
        }
        assert_no_alloc("unpaused-reaper drain cycles", || {
            for _ in 0..4 {
                proc.poll_updates(SinkChoice::Dsp, 48_000, RateRatio::integer(1), true);
                proc.process_block(
                    SinkChoice::Dsp,
                    &inp,
                    &inp,
                    &mut out,
                    2,
                    RateRatio::integer(1),
                    48_000,
                    0,
                );
            }
        });
    }
    while let Ok(old) = recycle_rx.pop() {
        drop(old);
        reaped += 1;
    }

    assert_eq!(
        reaped, 10,
        "every replaced chain must be retired off-RT without dropping on RT"
    );
    assert_eq!(proc.recycle_stash_len(), 0);
}

#[test]
fn rt_warmup_and_latency_mismatch_swaps_never_allocate_or_deallocate() {
    let (mut cmd_tx, cmd_rx) = rtrb::RingBuffer::new(16);
    let (mut swap_tx, swap_rx) = rtrb::RingBuffer::new(16);
    let (recycle_tx, mut recycle_rx) = rtrb::RingBuffer::new(8);
    let ab_side = Arc::new(AtomicU8::new(0));
    let meters = MeterBank::new(2);

    let mut proc = DaemonProcessor::new(
        make_eq_chain(0.0, 4, 8192, false),
        true,
        2,
        512,
        16,
        48_000,
        ab_side,
        meters,
        cmd_rx,
        swap_rx,
        recycle_tx,
    );

    // Prepare equal-latency 4x chain and mismatched-latency (+conv) 4x chain off-RT.
    let swap_same_lat = PendingSwap {
        chain: make_eq_chain(-3.0, 4, 8192, false),
        crossfade: true,
        generation: 1,
        swap_seq: 1,
    };
    let swap_diff_lat = PendingSwap {
        chain: make_eq_chain(-6.0, 4, 8192, true),
        crossfade: true,
        generation: 2,
        swap_seq: 2,
    };
    swap_tx.push(swap_same_lat).unwrap();

    let block_in = 512usize;
    let mut phase = 0.0_f64;
    let step = 2.0 * PI * 1000.0 / 48_000.0;
    let mut gen_block = || {
        let mut buf = Vec::with_capacity(block_in * 2);
        for _ in 0..block_in {
            let s = (phase.sin() * 0.4) as f32;
            buf.push(s);
            buf.push(s);
            phase += step;
        }
        buf
    };
    let mut blocks = Vec::new();
    for _ in 0..80 {
        blocks.push(gen_block());
    }
    let mut out = vec![0_i32; block_in * 4 * 2];

    assert_no_alloc("4x long-filter warm-up crossfade", || {
        for inp in &blocks[..35] {
            proc.poll_updates(SinkChoice::Dsp, 48_000, RateRatio::integer(4), true);
            proc.process_block(
                SinkChoice::Dsp,
                inp,
                inp,
                &mut out,
                2,
                RateRatio::integer(4),
                48_000,
                0,
            );
        }
        let _ = proc.latency_out_frames(SinkChoice::Dsp);
    });
    assert!(recycle_rx.pop().is_ok());

    // Now test mismatched-latency FadeOutOld -> FadeInNew + reset_dsp_state off-heap check.
    swap_tx.push(swap_diff_lat).unwrap();
    cmd_tx
        .push(RtDspCmd::AbSelect {
            side: AbSide::A,
            crossfade_ms: 10.0,
        })
        .unwrap();

    assert_no_alloc("mismatched-latency fade-out/fade-in + reset", || {
        for inp in &blocks[35..75] {
            proc.poll_updates(SinkChoice::Dsp, 48_000, RateRatio::integer(4), true);
            proc.process_block(
                SinkChoice::Dsp,
                inp,
                inp,
                &mut out,
                2,
                RateRatio::integer(4),
                48_000,
                0,
            );
        }
        proc.reset_dsp_state();
    });
    assert!(recycle_rx.pop().is_ok());

    // An EQ edit before the convolution: the incoming chain takes the
    // playing one over in place (moves) and fades the edit in before the
    // convolution.
    swap_tx
        .push(PendingSwap {
            chain: make_eq_chain(-5.0, 4, 8192, true),
            crossfade: true,
            generation: 3,
            swap_seq: 3,
        })
        .unwrap();
    let more: Vec<_> = (0..40).map(|_| gen_block()).collect();
    let mut first = 0;
    assert_no_alloc("an upstream edit taken over in place", || {
        for inp in &more {
            proc.poll_updates(SinkChoice::Dsp, 48_000, RateRatio::integer(4), true);
            if first == 0 {
                first = proc.active_generation();
            }
            proc.process_block(
                SinkChoice::Dsp,
                inp,
                inp,
                &mut out,
                2,
                RateRatio::integer(4),
                48_000,
                0,
            );
        }
    });
    assert_eq!(first, 3, "taken over at the first poll");
    assert_eq!(proc.active_generation(), 3);
    assert!(recycle_rx.pop().is_ok());
}

/// The warm-up and fade swaps above with chains on channel workers: the
/// audio thread and the worker allocate nothing.
#[test]
fn rt_swaps_of_chains_on_channel_workers_never_allocate() {
    use audivane_dsp_core::workers::LanePool;
    use std::sync::atomic::Ordering;
    let pool = LanePool::spawn(1, |_| WORKER.with(|w| w.set(true))).unwrap();
    assert!(pool.wait_started(std::time::Duration::from_secs(10)));
    let on_workers = |mut chain: ActiveChain| {
        let ActiveChain::Single(c) = &mut chain else {
            unreachable!()
        };
        assert!(c.attach_workers(&pool));
        chain
    };
    let (_cmd_tx, cmd_rx) = rtrb::RingBuffer::new(16);
    let (mut swap_tx, swap_rx) = rtrb::RingBuffer::new(16);
    let (recycle_tx, mut recycle_rx) = rtrb::RingBuffer::new(8);
    let mut proc = DaemonProcessor::new(
        on_workers(make_eq_chain(0.0, 4, 8192, false)),
        true,
        2,
        512,
        16,
        48_000,
        Arc::new(AtomicU8::new(0)),
        MeterBank::new(2),
        cmd_rx,
        swap_rx,
        recycle_tx,
    );
    swap_tx
        .push(PendingSwap {
            chain: on_workers(make_eq_chain(-3.0, 4, 8192, false)),
            crossfade: true,
            generation: 1,
            swap_seq: 1,
        })
        .unwrap();
    let inp: Vec<f32> = (0..512 * 2)
        .map(|i| ((i / 2) as f64 * 0.05).sin() as f32 * 0.4)
        .collect();
    let mut out = vec![0_i32; 512 * 4 * 2];
    let worker_before = WORKER_COUNT.load(Ordering::SeqCst);
    let mut run = |proc: &mut DaemonProcessor, blocks: usize| {
        for _ in 0..blocks {
            proc.poll_updates(SinkChoice::Dsp, 48_000, RateRatio::integer(4), true);
            proc.process_block(
                SinkChoice::Dsp,
                &inp,
                &inp,
                &mut out,
                2,
                RateRatio::integer(4),
                48_000,
                0,
            );
        }
    };
    assert_no_alloc("swap between chains on workers", || run(&mut proc, 35));
    assert!(recycle_rx.pop().is_ok());
    swap_tx
        .push(PendingSwap {
            chain: on_workers(make_eq_chain(-6.0, 4, 8192, true)),
            crossfade: true,
            generation: 2,
            swap_seq: 2,
        })
        .unwrap();
    assert_no_alloc("mismatched-latency swap on workers + reset", || {
        run(&mut proc, 40);
        proc.reset_dsp_state();
        run(&mut proc, 5);
    });
    assert!(recycle_rx.pop().is_ok());
    // An EQ edit before the convolution (in the lanes): taken over in place,
    // the fade before the convolution in each lane.
    swap_tx
        .push(PendingSwap {
            chain: on_workers(make_eq_chain(-5.0, 4, 8192, true)),
            crossfade: true,
            generation: 3,
            swap_seq: 3,
        })
        .unwrap();
    assert_no_alloc("an upstream edit taken over on workers", || {
        run(&mut proc, 10)
    });
    assert_eq!(proc.active_generation(), 3);
    assert!(recycle_rx.pop().is_ok());
    assert_eq!(
        WORKER_COUNT.load(Ordering::SeqCst) - worker_before,
        0,
        "heap operations on the channel worker"
    );
}

#[test]
fn rt_spectrum_taps_never_allocate_and_drop_when_full() {
    let (_cmd_tx, cmd_rx) = rtrb::RingBuffer::new(16);
    let (_swap_tx, swap_rx) = rtrb::RingBuffer::new(16);
    let (recycle_tx, _recycle_rx) = rtrb::RingBuffer::new(8);
    let ab_side = Arc::new(AtomicU8::new(0));
    let meters = MeterBank::new(2);
    let (spectrum, worker) = SpectrumRt::create(2).expect("spectrum rings");
    worker.set_enabled(true);

    let mut proc = DaemonProcessor::new(
        make_eq_chain(0.0, 1, 16, false),
        true,
        2,
        512,
        16,
        48_000,
        ab_side,
        meters,
        cmd_rx,
        swap_rx,
        recycle_tx,
    )
    .with_spectrum(spectrum);

    let inp = vec![0.25_f32; 512 * 2];
    let mut out = vec![0_i32; 512 * 2];

    // Enough blocks to overrun both preallocated rings several times: a full
    // ring must drop samples, never allocate.
    assert_no_alloc("spectrum tap copies, drops and direct path", || {
        for _ in 0..128 {
            proc.poll_updates(SinkChoice::Dsp, 48_000, RateRatio::integer(1), true);
            proc.process_block(
                SinkChoice::Dsp,
                &inp,
                &inp,
                &mut out,
                2,
                RateRatio::integer(1),
                48_000,
                0,
            );
        }
        for _ in 0..16 {
            proc.poll_updates(SinkChoice::Direct, 48_000, RateRatio::integer(1), true);
            proc.process_block(
                SinkChoice::Direct,
                &inp,
                &inp,
                &mut out,
                2,
                RateRatio::integer(1),
                48_000,
                0,
            );
        }
    });
}

#[test]
fn rt_volume_balance_and_mute_updates_never_allocate() {
    use audivane_dsp_daemon::engine::processor::VolumeTarget;

    let (_cmd_tx, cmd_rx) = rtrb::RingBuffer::new(16);
    let (_swap_tx, swap_rx) = rtrb::RingBuffer::new(16);
    let (recycle_tx, _recycle_rx) = rtrb::RingBuffer::new(8);
    let ab_side = Arc::new(AtomicU8::new(0));
    let meters = MeterBank::new(2);

    let mut proc = DaemonProcessor::new(
        make_eq_chain(0.0, 1, 16, false),
        true,
        2,
        512,
        16,
        48_000,
        ab_side,
        meters,
        cmd_rx,
        swap_rx,
        recycle_tx,
    );
    let shared_vol = proc.volume_handle();

    let inp = vec![0.25_f32; 512 * 2];
    let mut out = vec![0_i32; 512 * 2];

    let targets = [
        VolumeTarget {
            volume_db: -12.0,
            balance_db: -3.5,
            muted: false,
            hardware_volume: false,
            hardware_mute: false,
        },
        VolumeTarget {
            volume_db: -12.0,
            balance_db: 4.0,
            muted: true,
            hardware_volume: false,
            hardware_mute: false,
        },
        VolumeTarget {
            volume_db: -6.0,
            balance_db: 0.0,
            muted: false,
            hardware_volume: true,
            hardware_mute: true,
        },
        VolumeTarget::default(),
    ];

    assert_no_alloc("volume/balance/mute RT updates and ramps", || {
        for target in targets {
            shared_vol.store(target);
            for sink in [SinkChoice::Dsp, SinkChoice::Direct] {
                for _ in 0..4 {
                    proc.poll_updates(sink, 48_000, RateRatio::integer(1), true);
                    proc.process_block(
                        sink,
                        &inp,
                        &inp,
                        &mut out,
                        2,
                        RateRatio::integer(1),
                        48_000,
                        0,
                    );
                }
            }
        }
    });
}

fn make_rate_chain(source_rate: u32, output_rate: u32, preamp_db: f64) -> ActiveChain {
    ActiveChain::Single(Box::new(
        Chain::try_new(ChainSpec {
            config: ChainConfig {
                eq: Some(EqProfile {
                    name: "test".into(),
                    preamp_db,
                    ..Default::default()
                }),
                upsampler: audivane_dsp_core::resample::ManualConfig {
                    target: RateTarget::Rate(output_rate),
                    taps: 512,
                    ..Default::default()
                }
                .into_upsampler_config()
                .unwrap(),
                ..ChainConfig::neutral()
            },
            sample_rate: source_rate,
            channels: 2,
            max_block: 1024,
            irs: None,
        })
        .unwrap(),
    ))
}

/// Fractional and decimating chains: variable block sizes, a crossfaded
/// swap that must join the frame grid off-grid, DSP off and Direct (which
/// refuses the full chain) at the fractional ratio, then a DSP reset.
#[test]
fn rt_rational_ratio_blocks_and_swaps_never_allocate() {
    for (source_rate, output_rate) in [(44_100u32, 48_000u32), (96_000, 44_100)] {
        let ratio = RateRatio::from_rates(source_rate, output_rate);
        let (mut cmd_tx, cmd_rx) = rtrb::RingBuffer::new(16);
        let (mut swap_tx, swap_rx) = rtrb::RingBuffer::new(16);
        let (recycle_tx, mut recycle_rx) = rtrb::RingBuffer::new(8);
        let mut proc = DaemonProcessor::new(
            make_rate_chain(source_rate, output_rate, 0.0),
            true,
            2,
            1024,
            16,
            source_rate,
            Arc::new(AtomicU8::new(0)),
            MeterBank::new(2),
            cmd_rx,
            swap_rx,
            recycle_tx,
        );
        let swap = PendingSwap {
            chain: make_rate_chain(source_rate, output_rate, -3.0),
            crossfade: true,
            generation: 1,
            swap_seq: 1,
        };
        let sizes: Vec<usize> = (0..120).map(|b| 37 + (b * 211) % 980).collect();
        let blocks: Vec<Vec<f32>> = sizes
            .iter()
            .map(|&n| {
                (0..n * 2)
                    .map(|i| ((i as f64) * 0.01).sin() as f32 * 0.4)
                    .collect()
            })
            .collect();
        let mut out = vec![0_i32; ratio.max_frames_out(1024) * 2];
        let mut pos = 0u64;
        let mut swap = Some(swap);
        assert_no_alloc(&format!("{ratio} blocks, swap, DSP off and reset"), || {
            for (b, inp) in blocks.iter().enumerate() {
                if b == 7 {
                    swap_tx.push(swap.take().unwrap()).ok();
                }
                // DSP off and on again while the device stays at the
                // fractional ratio: the chain keeps running.
                if b == 60 || b == 90 {
                    cmd_tx.push(RtDspCmd::SetDspEnabled(b == 90)).ok();
                }
                // Direct away from x1 (before its x1 reopen) plays no
                // chain: silence, reported.
                let sink = if (100..110).contains(&b) {
                    SinkChoice::Direct
                } else {
                    SinkChoice::Dsp
                };
                let n = inp.len() / 2;
                let n_out = ratio.frames_out(pos, n);
                proc.poll_updates(sink, source_rate, ratio, true);
                proc.process_block(
                    sink,
                    inp,
                    inp,
                    &mut out[..n_out * 2],
                    2,
                    ratio,
                    source_rate,
                    pos,
                );
                pos += n as u64;
            }
            let _ = proc.latency_out_frames(SinkChoice::Dsp);
            let _ = proc.latency_out_frames(SinkChoice::Direct);
            proc.reset_dsp_state();
        });
        assert_eq!(proc.active_generation(), 1, "{ratio}: swap never finished");
        while recycle_rx.pop().is_ok() {}
    }
}

/// The engine bypass resampler's chain (the fixed design alone) for
/// `source_rate -> output_rate`.
fn make_resample_chain(source_rate: u32, output_rate: u32) -> ActiveChain {
    let ratio = RateRatio::from_rates(source_rate, output_rate);
    ActiveChain::Resample(Box::new(
        Chain::try_new(ChainSpec {
            config: audivane_dsp_daemon::control::config::bypass_chain_config(
                Default::default(),
                source_rate,
                ratio,
            ),
            sample_rate: source_rate,
            channels: 2,
            max_block: 1024,
            irs: None,
        })
        .unwrap(),
    ))
}

/// The resample-only mode (Direct and DSP off away from x1): DSP toggles
/// whose outgoing chain holds until the incoming one installs, its chain
/// installed and crossfaded, Direct takeovers and a reset, at variable
/// block sizes, without the heap.
#[test]
fn rt_resample_only_mode_never_allocates() {
    let (source_rate, output_rate) = (44_100u32, 48_000u32);
    let ratio = RateRatio::from_rates(source_rate, output_rate);
    let (mut cmd_tx, cmd_rx) = rtrb::RingBuffer::new(16);
    let (mut swap_tx, swap_rx) = rtrb::RingBuffer::new(16);
    let (recycle_tx, mut recycle_rx) = rtrb::RingBuffer::new(8);
    let mut proc = DaemonProcessor::new(
        make_rate_chain(source_rate, output_rate, 0.0),
        true,
        2,
        1024,
        16,
        source_rate,
        Arc::new(AtomicU8::new(0)),
        MeterBank::new(2),
        cmd_rx,
        swap_rx,
        recycle_tx,
    );
    let mut swaps: Vec<Option<PendingSwap>> = (1..=3)
        .map(|seq| {
            Some(PendingSwap {
                chain: if seq == 3 {
                    make_rate_chain(source_rate, output_rate, -3.0)
                } else {
                    make_resample_chain(source_rate, output_rate)
                },
                crossfade: true,
                generation: seq,
                swap_seq: seq,
            })
        })
        .collect();
    let sizes: Vec<usize> = (0..140).map(|b| 29 + (b * 197) % 990).collect();
    let blocks: Vec<Vec<f32>> = sizes
        .iter()
        .map(|&n| {
            (0..n * 2)
                .map(|i| ((i as f64) * 0.013).sin() as f32 * 0.4)
                .collect()
        })
        .collect();
    let mut out = vec![0_i32; ratio.max_frames_out(1024) * 2];
    let mut pos = 0u64;
    let mut played = 0usize;
    let mut held = 0usize;
    assert_no_alloc("resample-only blocks, swaps, DSP off/on, Direct", || {
        for (b, inp) in blocks.iter().enumerate() {
            match b {
                // DSP off with the resample-only chain on its way (as the
                // engine sends it): the DSP chain holds until it installs.
                10 => {
                    cmd_tx.push(RtDspCmd::SetDspEnabledHolding(false)).ok();
                }
                20 => {
                    swap_tx.push(swaps[0].take().unwrap()).ok();
                }
                // A rebuilt resample-only chain: warmed and crossfaded.
                50 => {
                    swap_tx.push(swaps[1].take().unwrap()).ok();
                }
                // DSP on, its chain on its way: the resample-only chain
                // holds until it installs.
                110 => {
                    cmd_tx.push(RtDspCmd::SetDspEnabledHolding(true)).ok();
                }
                120 => {
                    swap_tx.push(swaps[2].take().unwrap()).ok();
                }
                _ => {}
            }
            // Direct takes over in the middle: the same chain plays it.
            let sink = if (70..100).contains(&b) {
                SinkChoice::Direct
            } else {
                SinkChoice::Dsp
            };
            let n = inp.len() / 2;
            let n_out = ratio.frames_out(pos, n);
            proc.poll_updates(sink, source_rate, ratio, true);
            proc.process_block(
                sink,
                inp,
                inp,
                &mut out[..n_out * 2],
                2,
                ratio,
                source_rate,
                pos,
            );
            let sounds = out[..n_out * 2].iter().any(|&v| v != 0);
            if (75..100).contains(&b) && sounds {
                played += 1;
            }
            if (11..20).contains(&b) && sounds {
                held += 1;
            }
            let _ = proc.is_chain_ready(sink, ratio, None);
            let _ = proc.has_chain_for(SinkChoice::Dsp, ratio);
            pos += n as u64;
        }
        let _ = proc.latency_out_frames(SinkChoice::Direct);
        proc.reset_dsp_state();
    });
    assert!(played > 20, "Direct played {played} blocks");
    assert_eq!(held, 9, "the DSP chain held {held} blocks after DSP off");
    assert_eq!(proc.active_generation(), 3, "the last swap never finished");
    while recycle_rx.pop().is_ok() {}
}

/// The top ratio (48 kHz x64 = 3.072 MHz) with the spectrum taps on: the
/// processor's scratch (sized for the engine's largest ratio) takes a whole
/// x64 period, a warm-up crossfade swap and a DSP reset without the heap.
#[test]
fn rt_x64_blocks_swaps_and_taps_never_allocate() {
    let ratio = RateRatio::integer(64);
    let (_cmd_tx, cmd_rx) = rtrb::RingBuffer::new(16);
    let (mut swap_tx, swap_rx) = rtrb::RingBuffer::new(16);
    let (recycle_tx, mut recycle_rx) = rtrb::RingBuffer::new(8);
    let ab_side = Arc::new(AtomicU8::new(0));
    let meters = MeterBank::new(2);
    let (spectrum, worker) = SpectrumRt::create(2).expect("spectrum rings");
    worker.set_enabled(true);
    let mut proc = DaemonProcessor::new(
        make_eq_chain(0.0, 64, 16_384, false),
        true,
        2,
        1024,
        audivane_dsp_daemon::engine::MAX_DSP_RATIO,
        48_000,
        ab_side,
        meters,
        cmd_rx,
        swap_rx,
        recycle_tx,
    )
    .with_spectrum(spectrum);
    swap_tx
        .push(PendingSwap {
            chain: make_eq_chain(-3.0, 64, 16_384, false),
            crossfade: true,
            generation: 1,
            swap_seq: 1,
        })
        .unwrap();

    let block_in = 1024usize;
    let inp: Vec<f32> = (0..block_in * 2)
        .map(|i| ((i / 2) as f64 * 2.0 * PI * 1000.0 / 48_000.0).sin() as f32 * 0.4)
        .collect();
    // One period of the default quantum at 48 kHz, as output frames.
    let mut out = vec![0_i32; block_in * 64 * 2];
    assert_no_alloc("x64 blocks, warm-up crossfade and spectrum taps", || {
        for _ in 0..40 {
            proc.poll_updates(SinkChoice::Dsp, 48_000, ratio, true);
            proc.process_block(SinkChoice::Dsp, &inp, &inp, &mut out, 2, ratio, 48_000, 0);
        }
        let _ = proc.latency_out_frames(SinkChoice::Dsp);
        proc.reset_dsp_state();
    });
    assert!(recycle_rx.pop().is_ok(), "the old chain was swapped out");
    assert!(out.iter().any(|&s| s != 0), "the x64 chain plays");
}

/// Varispeed on the RT thread, through a reopen: the stage installed and
/// retired (with the recycle queue full, so the stash takes them), the
/// enable and disable fades, Direct takeover and the restart after it, an
/// underfeed, a swap warming a second chain on the stage's output, the
/// reopen's stage reset and DSP reset, and the driver's side of each
/// (`VarispeedDriver`: start, pulls, command forwarding, polls, a live
/// quantum change, the load budget, the hand back).
#[test]
fn rt_varispeed_stage_and_reopen_never_allocate() {
    use audivane_dsp_core::varispeed::{Varispeed, VarispeedSpec};
    use audivane_dsp_daemon::engine::varispeed::VarispeedRtCmd;
    use audivane_io_pw::varispeed_rt::{
        VarispeedCommandSlot, VarispeedDriver, VarispeedDriverStatus,
    };
    use audivane_io_pw::varispeed_sched::{PullKind, Timing};

    let (_cmd_tx, cmd_rx) = rtrb::RingBuffer::new(16);
    let (mut swap_tx, swap_rx) = rtrb::RingBuffer::new(8);
    let (recycle_tx, mut recycle_rx) = rtrb::RingBuffer::new(8);
    let (mut vs_tx, vs_rx) = rtrb::RingBuffer::new(8);
    // A one-slot stage recycle queue nobody drains until the end.
    let (vs_recycle_tx, mut vs_recycle_rx) = rtrb::RingBuffer::<Box<Varispeed>>::new(1);
    let mut proc = DaemonProcessor::new(
        make_eq_chain(-1.0, 1, 16, false),
        true,
        2,
        8192,
        16,
        48_000,
        Arc::new(AtomicU8::new(0)),
        MeterBank::new(2),
        cmd_rx,
        swap_rx,
        recycle_tx,
    )
    .with_varispeed(vs_rx, vs_recycle_tx, 2);
    let stage = || {
        Box::new(
            Varispeed::new(VarispeedSpec {
                sample_rate: 48_000,
                channels: 2,
                max_frames: 8192,
                speed: 1.0,
            })
            .unwrap(),
        )
    };
    for _ in 0..3 {
        vs_tx.push(VarispeedRtCmd::Install(stage())).ok().unwrap();
    }
    swap_tx
        .push(PendingSwap {
            chain: make_eq_chain(-3.0, 1, 16, false),
            crossfade: true,
            generation: 1,
            swap_seq: 1,
        })
        .ok()
        .unwrap();
    let slot = VarispeedCommandSlot::default();
    let status = VarispeedDriverStatus::default();
    let mut drv = VarispeedDriver::new();
    let input: Vec<f32> = (0..2 * 2 * 8192 + 4)
        .map(|i| (0.3 * (i as f64 * 0.01).sin()) as f32)
        .collect();
    let mut out = vec![0_i32; 2 * 8192];
    slot.send(1.5, 50.0);
    let one = RateRatio::ONE;
    let mut pos = 0u64;
    let mut fill = 100_000usize;

    assert_no_alloc("varispeed stage, schedule and reopen", || {
        let mut started = false;
        for b in 0..240u64 {
            let sink = if (100..110).contains(&b) {
                SinkChoice::Direct
            } else {
                SinkChoice::Dsp
            };
            let n = [256usize, 1024, 64, 8192, 0, 511][b as usize % 6];
            proc.poll_updates(sink, 48_000, one, true);
            if b == 150 {
                // A reopen: the stage resets, the schedule primes again.
                if let Some(port) = proc.varispeed(sink, one, 48_000, 2) {
                    port.reset();
                }
                drv.stop();
                started = false;
            }
            if b == 200 {
                proc.reset_dsp_state();
            }
            let mut len = n * 2;
            if let Some(port) = proc.varispeed(sink, one, 48_000, 2) {
                if !started {
                    drv.start(Timing::new(48_000, 1024, 1024), b == 150, port);
                    drv.finish_priming();
                    started = true;
                }
                if b % 37 == 5 {
                    drv.update_quantum(if b % 2 == 0 { 256 } else { 1024 }, port);
                }
                let kind = if b % 50 == 49 {
                    PullKind::Drain
                } else {
                    PullKind::Play
                };
                // An underfeed every 23 blocks.
                let ring = if b % 23 == 7 { n / 3 } else { fill };
                let pull = drv.pull(port, b * 5_000_000, n, ring, kind);
                drv.after_pull(port, &slot);
                len = (pull.take + pull.pad) * 2;
                let p = drv.poll(b * 5_000_000 + 1, fill, b / 2);
                drv.note_load(b * 5_000_000 + 2, 0.1, Some(port));
                std::hint::black_box(p);
            } else if started {
                drv.stop();
                started = false;
            }
            drv.publish(&status);
            proc.process_block(
                sink,
                &input[..len],
                &input[..len],
                &mut out[..n * 2],
                2,
                one,
                48_000,
                pos,
            );
            let _ = proc.latency_out_frames(sink);
            pos += n as u64;
            fill = fill.wrapping_add(1);
            match b {
                60 => {
                    let _ = vs_tx.push(VarispeedRtCmd::Disable);
                }
                120 => {
                    let _ = vs_tx.push(VarispeedRtCmd::Disable);
                }
                _ => {}
            }
        }
    });
    let s = status.snapshot();
    assert!(s.underfeed_events > 0, "{s:?}");
    let applied = proc.varispeed_applied().snapshot();
    assert!(applied.retired >= 3, "{applied:?}");
    // Everything retired reaches the reaper.
    let mut reaped = 0;
    for _ in 0..8 {
        while vs_recycle_rx.pop().is_ok() {
            reaped += 1;
        }
        proc.poll_updates(SinkChoice::Dsp, 48_000, one, true);
    }
    while recycle_rx.pop().is_ok() {}
    assert_eq!(reaped, applied.retired);
}

/// The driver's varispeed steps (`varispeed_loop`) around the processor:
/// split periods, the hand-back with its refill, a disable's and a DSP
/// toggle's fades (the deferred toggle, the raw fade-in), the delay at each
/// trigger. The graph stand-in completes each cycle at its trigger.
#[test]
fn rt_varispeed_driver_steps_never_allocate() {
    use audivane_dsp_core::varispeed::{Varispeed, VarispeedSpec};
    use audivane_dsp_daemon::engine::varispeed::VarispeedRtCmd;
    use audivane_io_pw::varispeed_loop::{self as vsl, PeriodBufs, PeriodPlan, VsDelay, VsGraph};
    use audivane_io_pw::varispeed_rt::{Pull, VarispeedCommandSlot, VarispeedDriver};
    use audivane_io_pw::varispeed_sched::{PullKind, RING_ALLOC_FRAMES, Timing};

    struct Graph {
        now: u64,
        completed: u64,
        prod: rtrb::Producer<f32>,
        i: u64,
    }
    impl VsGraph for Graph {
        fn now_ns(&self) -> u64 {
            self.now
        }
        fn completed(&self) -> u64 {
            self.completed
        }
        fn trigger(&mut self, _rate: u32, quantum: u64, _delay: i64, _speed: f64) {
            for _ in 0..quantum {
                if self.prod.slots() < 2 {
                    break;
                }
                let x = (0.3 * (self.i as f64 * 0.01).sin()) as f32;
                let _ = self.prod.push(x);
                let _ = self.prod.push(x);
                self.i += 1;
            }
            self.completed += 1;
        }
        fn wait_until(&mut self, until_ns: u64) {
            self.now = self.now.max(until_ns);
        }
    }

    let (mut cmd_tx, cmd_rx) = rtrb::RingBuffer::new(16);
    let (_swap_tx, swap_rx) = rtrb::RingBuffer::new(8);
    let (recycle_tx, mut recycle_rx) = rtrb::RingBuffer::new(8);
    let (mut vs_tx, vs_rx) = rtrb::RingBuffer::new(8);
    let (vs_recycle_tx, mut vs_recycle_rx) = rtrb::RingBuffer::<Box<Varispeed>>::new(4);
    let mut proc = DaemonProcessor::new(
        make_eq_chain(-1.0, 1, 16, false),
        true,
        2,
        16_384,
        16,
        48_000,
        Arc::new(AtomicU8::new(0)),
        MeterBank::new(2),
        cmd_rx,
        swap_rx,
        recycle_tx,
    )
    .with_varispeed(vs_rx, vs_recycle_tx, 2);
    for _ in 0..2 {
        vs_tx
            .push(VarispeedRtCmd::Install(Box::new(
                Varispeed::new(VarispeedSpec {
                    sample_rate: 48_000,
                    channels: 2,
                    max_frames: 8192,
                    speed: 1.0,
                })
                .unwrap(),
            )))
            .ok()
            .unwrap();
    }
    let (prod, mut ring) = rtrb::RingBuffer::new(RING_ALLOC_FRAMES * 2);
    let mut graph = Graph {
        now: 1_000_000_000,
        completed: 0,
        prod,
        i: 0,
    };
    for _ in 0..24 {
        graph.trigger(48_000, 1024, 0, 1.0);
    }
    let slot = VarispeedCommandSlot::default();
    slot.send(1.25, 50.0);
    let mut drv = VarispeedDriver::new();
    let mut delay = VsDelay::new(RateRatio::ONE, 48_000);
    let mut dsp_in = vec![0.0_f32; (2 * 8192 + 2) * 2];
    let direct_in = vec![0.0_f32; (2 * 8192 + 2) * 2];
    let mut out = vec![0_i32; 16_384 * 2];
    let one = RateRatio::ONE;
    let mut pos = 0u64;
    let mut ran_split = false;

    assert_no_alloc("varispeed driver steps", || {
        proc.poll_updates(SinkChoice::Dsp, 48_000, one, false);
        if let Some(port) = proc.varispeed(SinkChoice::Dsp, one, 48_000, 2) {
            drv.start_priming(Timing::new(48_000, 8195, 1024), port, &slot);
            drv.finish_priming();
        }
        for b in 0..160u64 {
            match b {
                30 => {
                    let _ = cmd_tx.push(RtDspCmd::SetDspEnabled(false));
                }
                60 => {
                    let _ = cmd_tx.push(RtDspCmd::SetDspEnabled(true));
                }
                100 => {
                    let _ = vs_tx.push(VarispeedRtCmd::Disable);
                }
                _ => {}
            }
            let n = if b % 4 == 0 { 8194 } else { 1024 };
            proc.poll_updates(SinkChoice::Dsp, 48_000, one, true);
            delay.dac_read(graph.now, 2048, true);
            delay.clear_pending();
            delay.set_latency(proc.latency_out_frames(SinkChoice::Dsp));
            let port = proc.varispeed(SinkChoice::Dsp, one, 48_000, 2);
            vsl::handover(
                &mut drv, &mut graph, &ring, port, 2, 48_000, 8195, 1024, n, &delay, 1_000_000,
            );
            let ran = if drv.is_active() {
                vsl::run_period(
                    &mut drv,
                    &mut graph,
                    &mut proc,
                    PeriodBufs {
                        ring: &mut ring,
                        dsp_in: &mut dsp_in,
                        direct_in: &direct_in,
                        out: &mut out[..n * 2],
                    },
                    &slot,
                    PeriodPlan {
                        sink: SinkChoice::Dsp,
                        ratio: one,
                        graph_rate: 48_000,
                        channels: 2,
                        pos,
                        n_in: n,
                        n_out: n,
                        kind: PullKind::Play,
                        prime_wait_ns: None,
                        poll: true,
                        quantum: 1024,
                    },
                    &mut delay,
                )
            } else {
                None
            };
            match ran {
                Some(run) => ran_split |= run.calls > 1,
                None => {
                    let have = (ring.slots() / 2).min(n);
                    let pull = Pull {
                        take: have,
                        pad: n - have,
                        underfeed: false,
                    };
                    let len = vsl::take(&mut ring, &mut dsp_in, pull, 2);
                    proc.process_block(
                        SinkChoice::Dsp,
                        &dsp_in[..len],
                        &direct_in[..len],
                        &mut out[..n * 2],
                        2,
                        one,
                        48_000,
                        pos,
                    );
                    for _ in 0..(n as u64).div_ceil(1024) {
                        graph.trigger(48_000, 1024, 0, 1.0);
                    }
                }
            }
            delay.wrote(n, n);
            pos += n as u64;
            graph.now += n as u64 * 1_000_000_000 / 48_000;
            while recycle_rx.pop().is_ok() {}
        }
    });
    assert!(ran_split);
    let applied = proc.varispeed_applied().snapshot();
    assert!(!applied.enabled, "{applied:?}");
    assert!(applied.retired >= 2, "{applied:?}");
    assert_eq!(applied.refused_plans, 0);
    while vs_recycle_rx.pop().is_ok() {}
}

/// The tee rings, the alignment delay and its fades, the master's map and
/// published position, the command log, and the follower's reads of all three,
/// around the varispeed stage, through Direct, a reset and a follower leaving.
#[test]
fn rt_tee_position_log_and_alignment_never_allocate() {
    use audivane_dsp_core::varispeed::{Varispeed, VarispeedSpec};
    use audivane_dsp_daemon::engine::tee::TeeHost;
    use audivane_dsp_daemon::engine::varispeed::VarispeedRtCmd;
    use audivane_protocol::ipc::EngineConfig;

    let (_cmd_tx, cmd_rx) = rtrb::RingBuffer::new(16);
    let (_swap_tx, swap_rx) = rtrb::RingBuffer::new(8);
    let (recycle_tx, _recycle_rx) = rtrb::RingBuffer::new(8);
    let (mut vs_tx, vs_rx) = rtrb::RingBuffer::new(8);
    let (vs_recycle_tx, _vs_recycle_rx) = rtrb::RingBuffer::<Box<Varispeed>>::new(4);
    let (mut host, tee_rt) = TeeHost::pair(2);
    let mut proc = DaemonProcessor::new(
        make_eq_chain(-1.0, 1, 16, false),
        true,
        2,
        8192,
        16,
        48_000,
        Arc::new(AtomicU8::new(0)),
        MeterBank::new(2),
        cmd_rx,
        swap_rx,
        recycle_tx,
    )
    .with_varispeed(vs_rx, vs_recycle_tx, 2)
    .with_tee(tee_rt);
    let cfg = EngineConfig::default();
    host.add_follower("hw:CARD=A,DEV=0", &cfg).unwrap();
    host.add_follower("hw:CARD=B,DEV=0", &cfg).unwrap();
    let mut readers: Vec<_> = ["hw:CARD=A,DEV=0", "hw:CARD=B,DEV=0"]
        .iter()
        .map(|d| host.slots_mut().get_mut(d).unwrap().reader.take().unwrap())
        .collect();
    let shared = host.shared().clone();
    vs_tx
        .push(VarispeedRtCmd::Install(Box::new(
            Varispeed::new(VarispeedSpec {
                sample_rate: 48_000,
                channels: 2,
                max_frames: 8192,
                speed: 1.0,
            })
            .unwrap(),
        )))
        .ok()
        .unwrap();
    let input: Vec<f32> = (0..2 * (2 * 8192 + 4))
        .map(|i| (0.3 * (i as f64 * 0.01).sin()) as f32)
        .collect();
    let mut out = vec![0_i32; 2 * 8192];
    let (mut a, mut b) = (vec![0.0f64; 8192], vec![0.0f64; 8192]);
    let one = RateRatio::ONE;
    let mut pos = 0u64;
    let mut entry = 0u64;
    let mut run = |proc: &mut DaemonProcessor,
                   host: &mut TeeHost,
                   readers: &mut [audivane_dsp_daemon::engine::tee::TeeReader],
                   blocks: std::ops::Range<u64>| {
        for k in blocks {
            let sink = if (60..66).contains(&k) {
                SinkChoice::Direct
            } else {
                SinkChoice::Dsp
            };
            if k % 40 == 10 {
                let _ = host.set_alignment_ms([12.0, 0.0, 30.0, 45.0][(k / 40) as usize % 4]);
            }
            if k == 120 {
                proc.reset_dsp_state();
            }
            if k == 90 {
                proc.stream_prepared();
            }
            proc.poll_updates(sink, 48_000, one, true);
            let n = [256usize, 1024, 64, 2048, 1, 511][k as usize % 6];
            let mut len = n * 2;
            if let Some(port) = proc.varispeed(sink, one, 48_000, 2) {
                let n_src = port.plan(n);
                port.set_fill(n_src, false);
                port.set_speed_limit(if k > 150 { 1.3 } else { 2.0 });
                // Up, then below unity (the alignment cap follows it down,
                // and back up once settled).
                let speed = match k {
                    0..=30 => 1.0,
                    96..=130 => 0.6,
                    _ => 1.7,
                };
                port.set_speed(speed, 40.0);
                port.pulled();
                len = n_src * 2;
            }
            proc.process_block(
                sink,
                &input[..len],
                &input[..len],
                &mut out[..n * 2],
                2,
                one,
                48_000,
                pos,
            );
            pos += n as u64;
            proc.dac_read(512, k * 1_000_000);
            let _ = proc.latency_out_frames(sink);
            // The followers' side.
            for r in readers.iter_mut() {
                while r.read(&mut [&mut a[..], &mut b[..]]).is_some() {}
            }
            std::hint::black_box(shared.position.read());
            std::hint::black_box(shared.state.read());
            std::hint::black_box(proc.alignment_frames(sink));
            while let audivane_dsp_daemon::engine::tee::LogRead::Entry(_) = shared.log.get(entry) {
                entry += 1;
            }
        }
    };
    assert_no_alloc("tee, alignment, position and log", || {
        run(&mut proc, &mut host, &mut readers, 0..140);
    });
    // A follower leaves (the slot map is the control side's), then the
    // last: the delay fades out and retires.
    host.remove_follower("hw:CARD=A,DEV=0").unwrap();
    assert_no_alloc("one follower left", || {
        run(&mut proc, &mut host, &mut readers[1..], 140..200);
    });
    host.remove_follower("hw:CARD=B,DEV=0").unwrap();
    assert_no_alloc("no follower", || {
        run(&mut proc, &mut host, &mut readers[..0], 200..240);
    });
    assert!(entry > 3, "{entry} log entries");
    // The second follower's ring and the delay line (the first ring went
    // with the second removal's own reap).
    assert_eq!(host.reap(), 2);
}

/// A follower's per-period path (its reading, the loop, the log replay with the
/// master's speed changes and restarts, the stage in follower mode, its chain,
/// skips, fades, a re-prime after an xrun) never allocates, with the master
/// playing Direct for a while, its alignment changing and the follower's trim
/// and lateness changing (known steps).
#[test]
fn rt_follower_period_never_allocates() {
    use audivane_dsp_core::varispeed::{Varispeed, VarispeedSpec};
    use audivane_dsp_daemon::engine::follower::{
        FollowerControls, FollowerParams, FollowerPcm, FollowerRt, FollowerStatus, PcmFault,
        RunExit, chain_swaps, prime_device, step_device,
    };
    use audivane_dsp_daemon::engine::tee::TeeHost;
    use audivane_dsp_daemon::engine::varispeed::VarispeedRtCmd;
    use audivane_protocol::ipc::EngineConfig;

    /// A device that plays a period per wait, on an exact clock 50 ppm
    /// fast, and underruns now and then.
    struct Fake {
        written: u64,
        played: f64,
        period: usize,
        buffer: usize,
        t_ns: i64,
        waits: u64,
        restart: bool,
        /// The errno every call fails with from here on (unplugged,
        /// suspended).
        fault: Option<i32>,
    }
    impl FollowerPcm for Fake {
        fn status(&mut self) -> Result<(i64, i64), PcmFault> {
            Ok((self.written as i64 - self.played as i64, self.t_ns))
        }
        fn avail(&mut self) -> Result<usize, PcmFault> {
            let queued = (self.written as i64 - self.played as i64).max(0) as usize;
            Ok(self.buffer.saturating_sub(queued))
        }
        fn write(&mut self, samples: &[i32]) -> Result<usize, PcmFault> {
            let n = (samples.len() / 2).min(self.avail()?);
            self.written += n as u64;
            Ok(n)
        }
        fn start(&mut self) -> Result<(), PcmFault> {
            self.restart = false;
            Ok(())
        }
        fn wait(&mut self, _timeout_ms: u32) -> Result<bool, PcmFault> {
            if let Some(e) = self.fault {
                return Err(PcmFault::Lost(e));
            }
            self.waits += 1;
            self.played += self.period as f64 * (1.0 + 50e-6);
            self.t_ns += (self.period as f64 / 48_000.0 * 1e9) as i64;
            if self.waits % 400 == 399 {
                self.restart = true;
                self.played = self.written as f64;
            }
            Ok(true)
        }
        fn needs_restart(&self) -> bool {
            self.restart
        }
    }

    let (_cmd_tx, cmd_rx) = rtrb::RingBuffer::new(16);
    let (_swap_tx, swap_rx) = rtrb::RingBuffer::new(8);
    let (recycle_tx, _recycle_rx) = rtrb::RingBuffer::new(8);
    let (mut vs_tx, vs_rx) = rtrb::RingBuffer::new(8);
    let (vs_recycle_tx, _vs_recycle_rx) = rtrb::RingBuffer::<Box<Varispeed>>::new(4);
    let (mut host, tee_rt) = TeeHost::pair(2);
    let mut proc = DaemonProcessor::new(
        make_eq_chain(-1.0, 1, 16, false),
        true,
        2,
        8192,
        16,
        48_000,
        Arc::new(AtomicU8::new(0)),
        MeterBank::new(2),
        cmd_rx,
        swap_rx,
        recycle_tx,
    )
    .with_varispeed(vs_rx, vs_recycle_tx, 2)
    .with_tee(tee_rt);
    host.add_follower("hw:CARD=A,DEV=0", &EngineConfig::default())
        .unwrap();
    let reader = host
        .slots_mut()
        .get_mut("hw:CARD=A,DEV=0")
        .unwrap()
        .reader
        .take()
        .unwrap();
    let new_stage = || {
        Box::new(
            Varispeed::new(VarispeedSpec {
                sample_rate: 48_000,
                channels: 2,
                max_frames: 8192,
                speed: 1.0,
            })
            .unwrap(),
        )
    };
    vs_tx
        .push(VarispeedRtCmd::Install(new_stage()))
        .ok()
        .unwrap();
    let input: Vec<f32> = (0..2 * (2 * 8192 + 4))
        .map(|i| (0.3 * (i as f64 * 0.01).sin()) as f32)
        .collect();
    let mut out = vec![0_i32; 2 * 1024];
    let one = RateRatio::ONE;
    let status = Arc::new(FollowerStatus::default());
    let controls = Arc::new(FollowerControls::new(1.5, -3.0));
    let p = FollowerParams {
        graph_rate: 48_000,
        channels: 2,
        device_rate: 48_000,
        period: 512,
        buffer: 1536,
        ratio: one,
    };
    let mut rt = FollowerRt::new(
        p,
        reader,
        host.shared().clone(),
        make_eq_chain(-1.0, 1, 16, false),
        Arc::clone(&status),
        Arc::clone(&controls),
    )
    .unwrap();
    // Chain swaps: an edit of the same latency (a crossfade) and one with
    // a limiter's look-ahead (a fade through silence), built here.
    let (mut swap_host, swaps) = chain_swaps();
    rt.set_swaps(Some(swaps));
    let limited = {
        let mut c = ChainConfig::default();
        c.limiter.mode = audivane_protocol::LimiterMode::Limit;
        ActiveChain::Single(Box::new(Chain::new(ChainSpec {
            config: c,
            sample_rate: 48_000,
            channels: 2,
            max_block: 1024,
            irs: None,
        })))
    };
    let mut edits = [Some(make_eq_chain(-4.0, 1, 16, false)), Some(limited)];
    let mut pcm = Fake {
        written: 0,
        played: 0.0,
        period: 512,
        buffer: 1536,
        t_ns: 1_000_000_000,
        waits: 0,
        restart: false,
        fault: None,
    };
    let mut pos = 0u64;
    let mut m_written = 0u64;
    let mut cuts = 0u64;
    // The master's first blocks, outside the check (the tee's commands).
    let mut master_block = |proc: &mut DaemonProcessor, k: u64, t_ns: i64| {
        let sink = if (300..340).contains(&k) {
            SinkChoice::Direct
        } else {
            SinkChoice::Dsp
        };
        proc.poll_updates(sink, 48_000, one, true);
        let n = 1024;
        proc.dac_read(1024, t_ns as u64);
        // The calls end where the stage asks (a scheduled command).
        let mut off = 0;
        while off < n {
            let mut m = n - off;
            let mut len = m * 2;
            if let Some(port) = proc.varispeed(sink, one, 48_000, 2) {
                let due = port.frames_until_stop();
                cuts += u64::from(due.is_some());
                m = due.map_or(m, |u| m.min(u.max(1)));
                let n_src = port.plan(m);
                port.set_fill(n_src, false);
                if off + m >= n {
                    port.set_speed_limit(1.996);
                    let speed = match k {
                        0..=150 => 1.0,
                        151..=450 => 1.4,
                        _ => 0.8,
                    };
                    port.set_speed(speed, 100.0);
                    let _ = port.peak_until_settled();
                    port.pulled();
                }
                len = n_src * 2;
            }
            proc.process_block(
                sink,
                &input[..len],
                &input[..len],
                &mut out[..m * 2],
                2,
                one,
                48_000,
                pos,
            );
            pos += m as u64;
            off += m;
        }
        m_written += n as u64;
    };
    // The master holds speed commands back for the follower (logged
    // ahead, applied at a cut call).
    host.set_command_lead_ms(20.0);
    for k in 0..4 {
        master_block(&mut proc, k, pcm.t_ns);
    }
    prime_device(&mut pcm, &mut rt).unwrap();
    let mut k = 4u64;
    assert_no_alloc("follower periods", || {
        for i in 0..1400u64 {
            if i % 2 == 0 {
                master_block(&mut proc, k, pcm.t_ns);
                k += 1;
            }
            if i == 500 || i == 900 {
                let _ = host.set_alignment_ms(if i == 500 { 20.0 } else { 26.0 });
            }
            // Known steps: the trim, the lateness, and the alignment's
            // above (the master's).
            if i == 700 {
                controls.retrim(-2.0);
            }
            if i == 800 {
                controls.set_lag_ms(3.0);
            }
            if i == 300 || i == 1000 {
                let c = edits[usize::from(i == 1000)].take().unwrap();
                assert!(swap_host.incoming.push(c).is_ok());
            }
            assert!(step_device(&mut pcm, &mut rt, &controls).is_none());
        }
        // A fault leaves the loop as its errno: nothing formatted, nothing
        // recovered here (a suspend resumes by the reopen, demoted).
        for e in [libc::ENODEV, libc::ESTRPIPE] {
            pcm.fault = Some(e);
            assert_eq!(
                step_device(&mut pcm, &mut rt, &controls),
                Some(RunExit::Lost(e))
            );
        }
    });
    let s = status.snapshot();
    assert!(s.skips >= 1, "{s:?}");
    assert!(s.known_steps >= 3, "{s:?}");
    assert!(s.xruns >= 2, "{s:?}");
    assert!(cuts > 0, "no command was scheduled");
    assert_eq!(rt.swaps_done, 2);
    let mut retired = 0;
    while swap_host.retired.pop().is_ok() {
        retired += 1;
    }
    assert_eq!(retired, 2);
}

/// Keep pitch on the RT thread. The stretch replaces a playing varispeed stage
/// under the switch's fade and back, plays the tee (its output, at nominal
/// speed) through the alignment delay to two followers' rings, rides speed
/// commands, and goes through DSP off and on, Direct, a reset and its
/// retirement, with no allocation.
#[test]
fn rt_keep_pitch_switches_and_stretched_tee_never_allocate() {
    use audivane_dsp_core::tempo::{TempoSpec, TempoStretch};
    use audivane_dsp_core::varispeed::{Varispeed, VarispeedSpec};
    use audivane_dsp_daemon::engine::tee::TeeHost;
    use audivane_dsp_daemon::engine::varispeed::VarispeedRtCmd;
    use audivane_protocol::ipc::EngineConfig;

    let (mut cmd_tx, cmd_rx) = rtrb::RingBuffer::new(16);
    let (_swap_tx, swap_rx) = rtrb::RingBuffer::new(8);
    let (recycle_tx, _recycle_rx) = rtrb::RingBuffer::new(8);
    let (mut vs_tx, vs_rx) = rtrb::RingBuffer::new(8);
    let (vs_recycle_tx, mut vs_recycle_rx) = rtrb::RingBuffer::<Box<Varispeed>>::new(8);
    let (stretch_recycle_tx, mut stretch_recycle_rx) =
        rtrb::RingBuffer::<Box<TempoStretch>>::new(8);
    let (mut host, tee_rt) = TeeHost::pair(2);
    let mut proc = DaemonProcessor::new(
        make_eq_chain(-1.0, 1, 16, false),
        true,
        2,
        8192,
        16,
        48_000,
        Arc::new(AtomicU8::new(0)),
        MeterBank::new(2),
        cmd_rx,
        swap_rx,
        recycle_tx,
    )
    .with_varispeed(vs_rx, vs_recycle_tx, 2)
    .with_stretch_recycle(stretch_recycle_tx)
    .with_tee(tee_rt);
    let cfg = EngineConfig::default();
    host.add_follower("hw:CARD=A,DEV=0", &cfg).unwrap();
    host.add_follower("hw:CARD=B,DEV=0", &cfg).unwrap();
    host.set_alignment_ms(12.0).unwrap();
    let mut readers: Vec<_> = ["hw:CARD=A,DEV=0", "hw:CARD=B,DEV=0"]
        .iter()
        .map(|d| host.slots_mut().get_mut(d).unwrap().reader.take().unwrap())
        .collect();
    let shared = host.shared().clone();
    let core = || {
        VarispeedRtCmd::Install(Box::new(
            Varispeed::new(VarispeedSpec {
                sample_rate: 48_000,
                channels: 2,
                max_frames: 8192,
                speed: 1.0,
            })
            .unwrap(),
        ))
    };
    let stretch = || {
        VarispeedRtCmd::InstallStretch(Box::new(
            TempoStretch::new(TempoSpec {
                sample_rate: 48_000,
                channels: 2,
                max_frames: 8192,
                speed: 1.0,
            })
            .unwrap(),
        ))
    };
    // Built off the RT thread; sent at the block numbers below.
    let mut sends: Vec<(u64, VarispeedRtCmd)> = vec![
        (0, core()),
        (20, stretch()),
        (60, core()),
        (61, stretch()),
        (100, core()),
        (150, stretch()),
        (230, VarispeedRtCmd::Disable),
    ];
    sends.reverse();
    let input: Vec<f32> = (0..2 * (2 * 8192 + 4))
        .map(|i| (0.3 * (i as f64 * 0.01).sin()) as f32)
        .collect();
    let mut out = vec![0_i32; 2 * 8192];
    let (mut a, mut b) = (vec![0.0f64; 8192], vec![0.0f64; 8192]);
    let one = RateRatio::ONE;
    let mut pos = 0u64;
    let mut entry = 0u64;
    let mut stretched = 0;
    assert_no_alloc("keep pitch switches and the stretched tee", || {
        for k in 0..260u64 {
            while sends.last().is_some_and(|s| s.0 == k) {
                let (_, cmd) = sends.pop().unwrap();
                let _ = vs_tx.push(cmd);
            }
            match k {
                120 => {
                    let _ = cmd_tx.push(RtDspCmd::SetDspEnabled(false));
                }
                135 => {
                    let _ = cmd_tx.push(RtDspCmd::SetDspEnabled(true));
                }
                200 => proc.reset_dsp_state(),
                _ => {}
            }
            let sink = if (180..186).contains(&k) {
                SinkChoice::Direct
            } else {
                SinkChoice::Dsp
            };
            proc.poll_updates(sink, 48_000, one, true);
            let n = [256usize, 1024, 64, 2048, 1, 511][k as usize % 6];
            let mut len = n * 2;
            if let Some(port) = proc.varispeed(sink, one, 48_000, 2) {
                let n = port.frames_until_stop().map_or(n, |u| n.min(u.max(1)));
                let n_src = port.plan(n);
                port.set_fill(n_src, false);
                port.set_speed(if (40..90).contains(&k) { 1.3 } else { 0.9 }, 40.0);
                port.pulled();
                len = n_src * 2;
            }
            proc.process_block(
                sink,
                &input[..len],
                &input[..len],
                &mut out[..n * 2],
                2,
                one,
                48_000,
                pos,
            );
            pos += n as u64;
            if proc.varispeed_applied().snapshot().stretch {
                stretched += 1;
            }
            proc.dac_read(512, k * 1_000_000);
            let _ = proc.latency_out_frames(sink);
            for r in readers.iter_mut() {
                while r.read(&mut [&mut a[..], &mut b[..]]).is_some() {}
            }
            std::hint::black_box(shared.position.read());
            std::hint::black_box(shared.state.read());
            while let audivane_dsp_daemon::engine::tee::LogRead::Entry(_) = shared.log.get(entry) {
                entry += 1;
            }
        }
    });
    assert!(sends.is_empty());
    assert!(stretched > 60, "{stretched} blocks with the stretch");
    let applied = proc.varispeed_applied().snapshot();
    assert!(!applied.enabled && !applied.stretch, "{applied:?}");
    assert_eq!(applied.refused_plans, 0);
    let (mut cores, mut stretches) = (0, 0);
    while vs_recycle_rx.pop().is_ok() {
        cores += 1;
    }
    while stretch_recycle_rx.pop().is_ok() {
        stretches += 1;
    }
    assert_eq!(
        (cores, stretches),
        (3, 3),
        "every stage retired to its queue"
    );
}

/// Measurement playback on the RT threads: the master takes the job, arms,
/// mutes the source, plays the file after its chain (`bypass`) or at its
/// input (`active`, with the keepalive again after the chain), and a
/// follower plays its part at its stage input (and around its chain for
/// `bypass`); both record their DAC readings; a stop fades out and the
/// mute lifts, and the job goes back. None of it allocates.
#[test]
fn rt_measurement_playback_never_allocates() {
    use audivane_dsp_daemon::engine::follower::{
        FollowerControls, FollowerParams, FollowerPcm, FollowerRt, FollowerStatus, PcmFault,
        prime_device, step_device,
    };
    use audivane_dsp_daemon::engine::measure::{MeasureDevice, MeasureJob, MeasureSlot};
    use audivane_dsp_daemon::engine::tee::TeeHost;
    use audivane_protocol::ipc::{EngineConfig, MeasureChain};
    use std::sync::atomic::Ordering;

    struct Fake {
        written: u64,
        played: f64,
        period: usize,
        buffer: usize,
        t_ns: i64,
    }
    impl FollowerPcm for Fake {
        fn status(&mut self) -> Result<(i64, i64), PcmFault> {
            Ok((self.written as i64 - self.played as i64, self.t_ns))
        }
        fn avail(&mut self) -> Result<usize, PcmFault> {
            let queued = (self.written as i64 - self.played as i64).max(0) as usize;
            Ok(self.buffer.saturating_sub(queued))
        }
        fn write(&mut self, samples: &[i32]) -> Result<usize, PcmFault> {
            let n = (samples.len() / 2).min(self.avail()?);
            self.written += n as u64;
            Ok(n)
        }
        fn start(&mut self) -> Result<(), PcmFault> {
            Ok(())
        }
        fn wait(&mut self, _timeout_ms: u32) -> Result<bool, PcmFault> {
            self.played += self.period as f64;
            self.t_ns += (self.period as f64 / 48_000.0 * 1e9) as i64;
            Ok(true)
        }
        fn needs_restart(&self) -> bool {
            false
        }
    }

    let (_cmd_tx, cmd_rx) = rtrb::RingBuffer::new(16);
    let (_swap_tx, swap_rx) = rtrb::RingBuffer::new(8);
    let (recycle_tx, _recycle_rx) = rtrb::RingBuffer::new(8);
    let (mut host, tee_rt) = TeeHost::pair(2);
    let slot = Arc::new(MeasureSlot::default());
    let mut proc = DaemonProcessor::new(
        make_eq_chain(-1.0, 1, 16, false),
        true,
        2,
        8192,
        16,
        48_000,
        Arc::new(AtomicU8::new(0)),
        MeterBank::new(2),
        cmd_rx,
        swap_rx,
        recycle_tx,
    )
    .with_tee(tee_rt)
    .with_measure(Arc::clone(&slot), 2);
    host.add_follower("hw:CARD=A,DEV=0", &EngineConfig::default())
        .unwrap();
    let reader = host
        .slots_mut()
        .get_mut("hw:CARD=A,DEV=0")
        .unwrap()
        .reader
        .take()
        .unwrap();
    let status = Arc::new(FollowerStatus::default());
    let controls = Arc::new(FollowerControls::new(0.0, -3.0));
    let p = FollowerParams {
        graph_rate: 48_000,
        channels: 2,
        device_rate: 48_000,
        period: 512,
        buffer: 1536,
        ratio: RateRatio::ONE,
    };
    let mut rt = FollowerRt::new(
        p,
        reader,
        host.shared().clone(),
        make_eq_chain(-1.0, 1, 16, false),
        Arc::clone(&status),
        Arc::clone(&controls),
    )
    .unwrap();
    let mut pcm = Fake {
        written: 0,
        played: 0.0,
        period: 512,
        buffer: 1536,
        t_ns: 1_000_000_000,
    };
    let input: Vec<f32> = (0..2 * 1024)
        .map(|i| (0.3 * (i as f64 * 0.01).sin()) as f32)
        .collect();
    let mut out = vec![0_i32; 2 * 1024];
    let mut pos = 0u64;
    let mut master_block = |proc: &mut DaemonProcessor, t_ns: i64| {
        proc.poll_updates(SinkChoice::Dsp, 48_000, RateRatio::ONE, true);
        proc.dac_read(1024, t_ns as u64);
        proc.process_block(
            SinkChoice::Dsp,
            &input,
            &input,
            &mut out,
            2,
            RateRatio::ONE,
            48_000,
            pos,
        );
        pos += 1024;
    };
    for _ in 0..4 {
        master_block(&mut proc, pcm.t_ns);
    }
    prime_device(&mut pcm, &mut rt).unwrap();
    let job = |chain| {
        Arc::new(MeasureJob::new(
            chain,
            true,
            1e-5,
            4_800,
            48_000,
            vec![
                MeasureDevice::new(None, 48_000, 48_000, vec![(1, vec![0.25; 9_600])], 4_800)
                    .with_readings(4096),
                MeasureDevice::new(
                    Some("hw:CARD=A,DEV=0".into()),
                    48_000,
                    48_000,
                    vec![(0, vec![0.25; 9_600])],
                    4_800,
                )
                .with_readings(4096),
            ],
        ))
    };
    let jobs = [job(MeasureChain::Bypass), job(MeasureChain::Active)];
    for (n, j) in jobs.iter().enumerate() {
        // The control side's posts (they box the job) stay outside.
        controls.measure_slot().post(Some((Arc::clone(j), 1)));
        slot.post(Some((Arc::clone(j), 0)));
        assert_no_alloc("a measurement", || {
            for i in 0..120u64 {
                if i % 2 == 0 {
                    master_block(&mut proc, pcm.t_ns);
                }
                if n == 1 && i == 40 {
                    j.cancel.store(true, Ordering::Release);
                }
                if i == 90 {
                    // Every device drained by now.
                    assert!(j.devices.iter().all(|d| d.done.load(Ordering::Acquire)));
                    j.release.store(true, Ordering::Release);
                }
                assert!(step_device(&mut pcm, &mut rt, &controls).is_none());
            }
        });
        slot.post(None);
        controls.measure_slot().post(None);
        assert_no_alloc("taking a measurement back", || {
            for _ in 0..4u64 {
                master_block(&mut proc, pcm.t_ns);
                assert!(step_device(&mut pcm, &mut rt, &controls).is_none());
            }
        });
        slot.reclaim();
        controls.measure_slot().reclaim();
        assert!(j.s0().is_some(), "armed");
        // The master's DAC readings were taken (into the buffer built on
        // the control side).
        assert!(!j.devices[0].readings.snapshot().is_empty());
        assert!(j.devices[0].done.load(Ordering::Acquire));
        assert!(j.released.load(Ordering::Acquire));
        assert_eq!(Arc::strong_count(j), 1, "the threads let go of the job");
    }
}
