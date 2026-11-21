//! Counting-allocator check that the RT thread's reopen decision
//! (`decide_rt_reopen`, and the open landing it predicts from the last
//! open's key, the engine bypass resampler's landing included) never
//! allocates.

use audivane_io_pw::engine_driver::{CARRY_MAX_FRAMES, OutCarry, PcmSink};
use audivane_io_pw::lifecycle::{
    BypassLeg, DesiredDspConfig, DeviceRates, MASK_OUTPUT_RATES, RatePairMask, decide_rt_reopen,
    negotiated_key,
};
use audivane_protocol::ipc::SinkChoice;
use audivane_protocol::{RateRatio, RateTarget};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

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

#[test]
fn the_reopen_decision_never_allocates() {
    let integer_only = RatePairMask::from_fn(|i, o| o % i == 0);
    let rate_sets = [
        DeviceRates::from_rates([44_100, 48_000]),
        DeviceRates::from_rates([48_000, 96_000]),
        DeviceRates::from_rates(MASK_OUTPUT_RATES),
        None,
    ];
    let targets = [
        RateTarget::Ratio(4),
        RateTarget::MaxRate(192_000),
        RateTarget::Rate(48_000),
        RateTarget::MaxRateAny(48_000),
        RateTarget::MaxRateAny(768_000),
    ];
    let mut keys = Vec::new();
    let mut cfgs = Vec::new();
    for rates in rate_sets {
        for rate_target in targets {
            for buildable in [RatePairMask::ALL, integer_only] {
                for (dsp_enabled, engine) in
                    [(true, false), (false, false), (true, true), (false, true)]
                {
                    let cfg = DesiredDspConfig {
                        dsp_enabled,
                        rate_target,
                        buildable,
                        bypass: BypassLeg {
                            engine,
                            buildable,
                            allowed: BypassLeg::allowed_bits(audivane_protocol::ALLOWED_RATES),
                        },
                        ..DesiredDspConfig::default()
                    };
                    for landed in [RateRatio::ONE, RateRatio::integer(4)] {
                        let open = audivane_io_alsa::RatioOpen {
                            ratio: landed,
                            refused: vec![RateRatio::integer(8)],
                            transient: vec![RateRatio::integer(6)],
                        };
                        keys.push(negotiated_key(cfg, SinkChoice::Dsp, 44_100, rates, &open));
                    }
                    cfgs.push(cfg);
                }
            }
        }
    }
    let actives = [
        RateRatio::ONE,
        RateRatio::new(160, 147),
        RateRatio::integer(4),
    ];

    COUNT.with(|c| c.set(0));
    ARMED.with(|a| a.set(true));
    let mut reopens = 0usize;
    for key in &keys {
        for cfg in &cfgs {
            for sink in [SinkChoice::Dsp, SinkChoice::Direct] {
                for active in actives {
                    for cur in [44_100, 48_000, 176_400] {
                        let r = decide_rt_reopen(*cfg, Some(*key), sink, sink, 44_100, cur, active);
                        reopens += usize::from(r.is_some());
                    }
                }
            }
        }
    }
    ARMED.with(|a| a.set(false));
    let count = COUNT.with(Cell::get);
    assert_eq!(count, 0, "{count} heap operations in decide_rt_reopen");
    assert!(reopens > 0);
}

/// A device buffer that takes what fits (preallocated, never grows).
struct Sink {
    room: usize,
    channels: usize,
}

impl PcmSink for Sink {
    fn room(&mut self) -> Option<usize> {
        Some(self.room)
    }

    fn needs_restart(&self) -> bool {
        false
    }

    fn write_all(&mut self, samples: &[i32], _stop: &dyn Fn() -> bool) -> Option<usize> {
        let n = (samples.len() / self.channels).min(self.room);
        self.room -= n;
        Some(n)
    }

    fn write_silence(&mut self, frames: usize) -> Option<usize> {
        let n = frames.min(self.room);
        self.room -= n;
        Some(n)
    }
}

#[test]
fn the_prefill_carry_never_allocates() {
    let mut carry = OutCarry::with_capacity(2 * CARRY_MAX_FRAMES);
    let out = vec![7_i32; 2 * 513];
    let mut sink = Sink {
        room: 1536,
        channels: 2,
    };
    let stop = || false;
    COUNT.with(|c| c.set(0));
    ARMED.with(|a| a.set(true));
    let mut written = 0;
    for _ in 0..3 {
        written += carry.prime_write(&mut sink, &out, 2, &stop).0;
    }
    sink.room = 512;
    written += carry.write_ahead(&mut sink, 2, &stop).0;
    for _ in 0..4 {
        sink.room = 1536;
        written += carry.prime_write(&mut sink, &out, 2, &stop).0;
        carry.clear();
    }
    ARMED.with(|a| a.set(false));
    let count = COUNT.with(Cell::get);
    assert_eq!(count, 0, "{count} heap operations in the prefill carry");
    assert!(written > 1536);
}
