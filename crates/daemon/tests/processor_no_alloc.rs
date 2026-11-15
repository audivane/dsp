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
use audivane_protocol::ipc::{AbSide, SinkChoice};
use audivane_protocol::{
    Band, ChainConfig, ConvolutionConfig, EqProfile, FilterType, RateTarget, UpsamplerConfig,
};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::f64::consts::PI;
use std::sync::Arc;
use std::sync::atomic::AtomicU8;

struct Counting;

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static COUNT: Cell<usize> = const { Cell::new(0) };
}

fn note() {
    let _ = ARMED.try_with(|armed| {
        if armed.get() {
            COUNT.with(|c| c.set(c.get() + 1));
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
                }]
            } else {
                Vec::new()
            },
            upsampler: UpsamplerConfig {
                target: if ratio <= 1 {
                    RateTarget::Off
                } else {
                    RateTarget::Ratio(ratio)
                },
                taps,
                ..Default::default()
            },
            ..Default::default()
        },
        sample_rate: 48_000,
        channels: 2,
        max_block: 1024,
        irs: if conv {
            Some(vec![vec![1.0; 2048]])
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
            proc.poll_updates(SinkChoice::Dsp, 48_000, 1, true);
            proc.process_block(SinkChoice::Dsp, &inp, &inp, &mut out, 2, 1, 48_000);
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
                proc.poll_updates(SinkChoice::Dsp, 48_000, 1, true);
                proc.process_block(SinkChoice::Dsp, &inp, &inp, &mut out, 2, 1, 48_000);
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
            proc.poll_updates(SinkChoice::Dsp, 48_000, 4, true);
            proc.process_block(SinkChoice::Dsp, inp, inp, &mut out, 2, 4, 48_000);
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
            proc.poll_updates(SinkChoice::Dsp, 48_000, 4, true);
            proc.process_block(SinkChoice::Dsp, inp, inp, &mut out, 2, 4, 48_000);
        }
        proc.reset_dsp_state();
    });
    assert!(recycle_rx.pop().is_ok());
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
            proc.poll_updates(SinkChoice::Dsp, 48_000, 1, true);
            proc.process_block(SinkChoice::Dsp, &inp, &inp, &mut out, 2, 1, 48_000);
        }
        for _ in 0..16 {
            proc.poll_updates(SinkChoice::Direct, 48_000, 1, true);
            proc.process_block(SinkChoice::Direct, &inp, &inp, &mut out, 2, 1, 48_000);
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
                    proc.poll_updates(sink, 48_000, 1, true);
                    proc.process_block(sink, &inp, &inp, &mut out, 2, 1, 48_000);
                }
            }
        }
    });
}
