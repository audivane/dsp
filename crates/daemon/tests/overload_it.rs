//! DSP overload, end to end through the daemon (mock engine): a saved
//! config that overloads is never refused at start; the engine falls back
//! (status, events: accepted, then applied) while the saved config stays
//! as it was; a new heavy upsampler is refused with a plain message; a
//! light one ends the fallback. A step the engine refuses moves on to the
//! next one, and a recovery edit the engine refuses keeps the notice. A
//! fallback another DAC cannot play is replaced; the startup config counts
//! as the last one that ran fine.

mod common;

use audivane_dsp_core::cpu_bench::KernelRates;
use audivane_protocol::ipc::{
    Call, EngineState, Event, OverloadAction, OverloadReport, SinkChoice, Status, Topic,
};
use audivane_protocol::{RateTarget, UpsamplerConfig};
use common::{Harness, Reply, TestClient};
use std::time::{Duration, Instant};

/// A machine that sustains 1 G multiply-adds per second in real time.
fn slow_machine() -> &'static KernelRates {
    Box::leak(Box::new(KernelRates {
        dot: vec![(256 << 10, 1.0e9), (8 << 20, 0.8e9)],
        // Slow spectra and transforms (a slow core): on a desktop the FFT
        // polyphase engine runs the heavy designs below well within budget.
        spectral: vec![(256 << 10, 0.06e9), (8 << 20, 0.05e9), (64 << 20, 0.03e9)],
        fft: vec![(2048, 4.0e-9), (32768, 4.0e-9)],
        mul_add: 0.3e9,
        quantizer: KernelRates::floor().quantizer,
        elapsed: Duration::ZERO,
    }))
}

fn manual(taps: usize) -> UpsamplerConfig {
    audivane_dsp_core::resample::ManualConfig {
        target: RateTarget::Ratio(16),
        taps,
        ..Default::default()
    }
    .into_upsampler_config()
    .unwrap()
}

fn light_up() -> UpsamplerConfig {
    audivane_dsp_core::resample::ManualConfig {
        target: RateTarget::Ratio(4),
        taps: 4095,
        ..Default::default()
    }
    .into_upsampler_config()
    .unwrap()
}

/// The next `overload` event that reports its step applied (an accepted
/// step is announced first, then again once it runs; the pump may send
/// only the second).
fn applied_overload(events: &mut TestClient) -> OverloadReport {
    loop {
        let Some(Event::Overload(report)) = events.next_event("overload", Duration::from_secs(10))
        else {
            panic!("no overload event");
        };
        if report.applied {
            return report;
        }
    }
}

fn status(h: &Harness) -> Status {
    serde_json::from_value(h.client().ok(Call::Status)).unwrap()
}

fn wait_until(what: &str, timeout: Duration, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_saved_config_that_overloads_falls_back_and_is_kept() {
    // The saved config at start: 4 Mi taps at x16. Starting never refuses.
    // Without channel workers, so the refusal below prices one thread.
    let h = Harness::start_with("overload", |cfg| {
        cfg.rate.allowed_rates = vec![48_000];
        cfg.chain.upsampler = manual(4 << 20);
        cfg.channel_workers = false;
    });
    *h.engine.cpu_rates.lock().unwrap() = Some(slow_machine());
    let mut events = h.client();
    events.subscribe(vec![Topic::Status]);

    // The engine plays over real time.
    h.engine.set_snapshot(|s| {
        s.state = EngineState::Playing;
        s.active_sink = Some(SinkChoice::Dsp);
        s.dsp_load = 1.4;
        s.output_rate = 768_000;
    });
    let report = applied_overload(&mut events);
    assert_eq!(report.action, OverloadAction::UpsamplerOff);
    assert_eq!(report.stage, "upsampler");
    assert_eq!(report.output_rate, 768_000);
    assert!(
        report
            .message
            .starts_with("The engine switched the upsampler off: it needed more CPU"),
        "{}",
        report.message
    );
    // The engine runs without the upsampler; the request stays as saved.
    wait_until("the fallback", Duration::from_secs(5), || {
        h.engine.config_now().chain.upsampler.target == RateTarget::Off
    });
    let st = status(&h);
    assert_eq!(st.overload.as_ref(), Some(&report));
    let cfg: audivane_protocol::ipc::EngineConfig =
        serde_json::from_value(h.client().ok(Call::GetConfig)).unwrap();
    assert_eq!(cfg.chain.upsampler, manual(4 << 20));
    h.engine.set_snapshot(|s| s.dsp_load = 0.2);

    // Another heavy design is refused with a plain message.
    match h.client().call(Call::SetUpsampler(manual(2 << 20))) {
        Reply::Error(e) => {
            assert!(
                e.message
                    .starts_with("Too heavy for this CPU at 768 kHz from a "),
                "{}",
                e.message
            );
            assert!(
                e.message.ends_with(
                    "this machine sustains about 1.8 G MAC/s in real time. Use fewer \
                                taps or a lower output rate."
                ),
                "{}",
                e.message
            );
        }
        Reply::Ok(v) => panic!("admitted: {v}"),
    }
    // An edit elsewhere in the chain is not refused and keeps the fallback.
    h.client().ok(Call::SetDcBlock(Some(
        audivane_protocol::DcBlockConfig::default(),
    )));
    let running = h.engine.config_now();
    assert!(running.chain.dc_block.is_some());
    assert_eq!(running.chain.upsampler.target, RateTarget::Off);
    assert!(status(&h).overload.is_some());

    // A light upsampler is admitted and ends the fallback.
    h.client().ok(Call::SetUpsampler(light_up()));
    let running = h.engine.config_now();
    assert_eq!(running.chain.upsampler.target, RateTarget::Ratio(4));
    assert!(status(&h).overload.is_none());
}
