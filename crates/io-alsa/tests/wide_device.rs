//! A playback device with no stereo mode (an audio interface that plays 8
//! channels only) opens at its own channel count; the engine's two channels
//! land on the chosen pair, the other channels get exact zeros, and the
//! write stays allocation-free.
//!
//! The loopback card stands in for the device: its capture end, opened
//! first at 8 channels, holds the playback end to 8 channels, and shows
//! where every sample landed.

use alsa::pcm::{Access, Format, HwParams, PCM};
use alsa::{Direction, ValueOr};
use audivane_io_alsa::{AlsaConfig, OpenPcm};
use audivane_protocol::SampleFormat;
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
static ALLOC: Counting = Counting;

fn allocations<T>(f: impl FnOnce() -> T) -> (T, usize) {
    COUNT.with(|c| c.set(0));
    ARMED.with(|a| a.set(true));
    let out = f();
    ARMED.with(|a| a.set(false));
    (out, COUNT.with(Cell::get))
}

const PLAY: &str = "hw:Loopback,0,1";
const CAPTURE: &str = "hw:Loopback,1,1";
const WIDTH: usize = 8;
const FRAMES: usize = 2048;

fn open_capture(channels: u32) -> PCM {
    let pcm = PCM::new(CAPTURE, Direction::Capture, false).expect("open loopback capture");
    {
        let hwp = HwParams::any(&pcm).unwrap();
        hwp.set_access(Access::RWInterleaved).unwrap();
        hwp.set_format(Format::s32()).unwrap();
        hwp.set_channels(channels).unwrap();
        hwp.set_rate(48_000, ValueOr::Nearest).unwrap();
        hwp.set_period_size_near(1024, ValueOr::Nearest).unwrap();
        hwp.set_buffer_size_near(16_384).unwrap();
        pcm.hw_params(&hwp).unwrap();
    }
    pcm
}

fn sample(i: usize, right: bool) -> i32 {
    let v = ((i as i32) + 1) << 8;
    if right { -v } else { v }
}

/// One test: the three cases share the loopback subdevice.
#[test]
#[ignore = "needs the snd-aloop Loopback card"]
fn wide_and_stereo_devices_on_the_loopback() {
    an_eight_channel_only_device_plays_the_pair_and_zeros_the_rest();
    a_pair_past_the_device_is_refused();
    a_stereo_device_is_written_as_before();
}

fn an_eight_channel_only_device_plays_the_pair_and_zeros_the_rest() {
    let capture = open_capture(WIDTH as u32);
    let open = OpenPcm::open(PLAY, true).expect("open loopback playback");
    let caps = open.probe(2, &[48_000]).expect("probe");
    assert_eq!((caps.channels_min, caps.channels_max), (8, 8));
    assert_eq!(caps.channels, 8, "opens at the device's own count");
    assert!(caps.supports(48_000, SampleFormat::S32), "{caps:?}");
    assert!(caps.unusable_reason().is_none());

    let cfg = AlsaConfig {
        device: PLAY.into(),
        rate: 48_000,
        channels: 2,
        channel_map: vec![5, 2],
        period_frames: 1024,
        buffer_periods: 4,
        preferred_format: Some(SampleFormat::S32),
        strict_format: true,
        nonblock: true,
    };
    let mut out = open.configure(&cfg, &[SampleFormat::S32]).expect("open");
    let p = out.params();
    assert_eq!((p.channels, p.device_channels), (2, 8));
    out.prepare().unwrap();

    let mut stereo = vec![0_i32; FRAMES * 2];
    for i in 0..FRAMES {
        stereo[2 * i] = sample(i, false);
        stereo[2 * i + 1] = sample(i, true);
    }
    let (written, allocs) = allocations(|| out.write_i32_interleaved(&stereo));
    assert_eq!(allocs, 0, "the write allocates");
    assert_eq!(written.unwrap(), FRAMES);
    let (silence, allocs) = allocations(|| out.write_silence(1024));
    assert_eq!(allocs, 0, "the silence write allocates");
    silence.unwrap();
    capture.start().unwrap();
    out.start().unwrap();

    // Read until the signal has gone by (silence before it, while the
    // capture end waits for the playback end).
    let io = capture.io_i32().unwrap();
    let mut got: Vec<i32> = Vec::new();
    let mut buf = vec![0_i32; 1024 * WIDTH];
    while got.len() < (FRAMES + 3 * 1024) * WIDTH {
        let n = io.readi(&mut buf).expect("read capture");
        got.extend_from_slice(&buf[..n * WIDTH]);
    }
    let first = got
        .as_chunks::<WIDTH>()
        .0
        .iter()
        .position(|f| f.iter().any(|&s| s != 0))
        .expect("the signal reaches the capture end");
    let frames: Vec<&[i32]> = got
        .as_chunks::<WIDTH>()
        .0
        .iter()
        .skip(first)
        .take(FRAMES)
        .map(|f| &f[..])
        .collect();
    assert_eq!(frames.len(), FRAMES);
    for (i, f) in frames.iter().enumerate() {
        assert_eq!(f[5], sample(i, false), "frame {i}: L on channel 5");
        assert_eq!(f[2], sample(i, true), "frame {i}: R on channel 2");
        for ch in [0, 1, 3, 4, 6, 7] {
            assert_eq!(f[ch], 0, "frame {i}: channel {ch} is not silent");
        }
    }
}

fn a_pair_past_the_device_is_refused() {
    let _capture = open_capture(WIDTH as u32);
    let open = OpenPcm::open(PLAY, true).expect("open loopback playback");
    let cfg = AlsaConfig {
        device: PLAY.into(),
        rate: 48_000,
        channels: 2,
        channel_map: vec![8, 9],
        ..AlsaConfig::default()
    };
    let err = open
        .configure(&cfg, &[SampleFormat::S32])
        .err()
        .expect("refused");
    assert!(err.is_capability(), "{err}");
    assert!(err.to_string().contains("10 or more channels"), "{err}");
}

fn a_stereo_device_is_written_as_before() {
    let capture = open_capture(2);
    let open = OpenPcm::open(PLAY, true).expect("open loopback playback");
    let cfg = AlsaConfig {
        device: PLAY.into(),
        rate: 48_000,
        channels: 2,
        period_frames: 1024,
        buffer_periods: 4,
        preferred_format: Some(SampleFormat::S32),
        strict_format: true,
        ..AlsaConfig::default()
    };
    let mut out = open.configure(&cfg, &[SampleFormat::S32]).expect("open");
    assert_eq!(out.params().device_channels, 2);
    out.prepare().unwrap();
    let mut stereo = vec![0_i32; FRAMES * 2];
    for i in 0..FRAMES {
        stereo[2 * i] = sample(i, false);
        stereo[2 * i + 1] = sample(i, true);
    }
    assert_eq!(out.write_i32_interleaved(&stereo).unwrap(), FRAMES);
    out.write_silence(1024).unwrap();
    capture.start().unwrap();
    out.start().unwrap();
    let io = capture.io_i32().unwrap();
    let mut got: Vec<i32> = Vec::new();
    let mut buf = vec![0_i32; 2048];
    while got.len() < (FRAMES + 3 * 1024) * 2 {
        let n = io.readi(&mut buf).expect("read capture");
        got.extend_from_slice(&buf[..n * 2]);
    }
    let first = got
        .as_chunks::<2>()
        .0
        .iter()
        .position(|f| f.iter().any(|&s| s != 0))
        .expect("the signal reaches the capture end");
    assert_eq!(&got[first * 2..(first + FRAMES) * 2], &stereo[..]);
}
