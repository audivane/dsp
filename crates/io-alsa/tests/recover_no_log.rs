//! Underrun recovery runs on the real-time audio thread: it must count the
//! xrun and prepare the device without logging (a log line takes the stderr
//! lock and may block on the write). The driver logs xruns from its control
//! thread instead.

use audivane_io_alsa::{AlsaConfig, AlsaOutput};
use std::sync::atomic::{AtomicUsize, Ordering};

static RECORDS: AtomicUsize = AtomicUsize::new(0);

struct Counting;

impl log::Log for Counting {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }
    fn log(&self, _: &log::Record) {
        RECORDS.fetch_add(1, Ordering::Relaxed);
    }
    fn flush(&self) {}
}

static LOGGER: Counting = Counting;

#[test]
#[ignore = "needs the snd-aloop Loopback card"]
fn an_underrun_recovery_does_not_log() {
    log::set_logger(&LOGGER).unwrap();
    log::set_max_level(log::LevelFilter::Trace);
    let cfg = AlsaConfig {
        device: "hw:Loopback,1,2".into(),
        rate: 48_000,
        channels: 2,
        channel_map: Vec::new(),
        period_frames: 1024,
        buffer_periods: 2,
        preferred_format: None,
        strict_format: false,
        nonblock: true,
    };
    let mut out = AlsaOutput::open(&cfg).expect("open loopback");
    out.prepare().unwrap();
    let period = out.params().period_frames as usize;
    out.write_i32_all_until(&vec![0_i32; period * 2], || false)
        .unwrap();
    out.start().unwrap();
    // Let the buffer run dry: the device underruns.
    let buffer_ms = u64::from(out.params().buffer_frames) * 1000 / 48_000;
    std::thread::sleep(std::time::Duration::from_millis(buffer_ms * 3 + 20));
    let opened = RECORDS.load(Ordering::Relaxed);
    let _ = out.avail_and_delay();
    let _ = out.write_i32_interleaved(&vec![0_i32; period * 2]);
    let _ = out.wait_for_period(5);
    assert!(out.xruns() >= 1, "no underrun was seen");
    assert!(out.needs_restart());
    assert_eq!(
        RECORDS.load(Ordering::Relaxed) - opened,
        0,
        "the recovery logged"
    );
}
