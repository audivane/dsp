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
use audivane_dsp_daemon::api::EngineError;
use audivane_protocol::ipc::{
    Call, EngineConfig, EngineState, Event, OverloadAction, OverloadReport, RpcError, SinkChoice,
    Status, Topic,
};
use audivane_protocol::{RateTarget, UpsamplerConfig};
use common::{Harness, Reply, TestClient};
use std::sync::Arc;
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
fn varispeed_steps_come_first_and_leave_the_config_alone() {
    // Varispeed runs at 1.5× (its budget lowered by a light measured
    // load) under a chain that overloads: the budget is pinned, the speed
    // held at 1×, the stage retired, then the chain's steps, with the
    // stage kept off. The varispeed steps apply no config.
    use audivane_dsp_daemon::api::{VarispeedHold, VarispeedRequest};
    let h = Harness::start_with("overload-vs", |cfg| {
        cfg.rate.allowed_rates = vec![48_000];
        cfg.chain.upsampler = manual(4 << 20);
    });
    *h.engine.varispeed.lock().unwrap() = VarispeedRequest {
        wanted: true,
        running: true,
        speed: 1.5,
        budget: 0.25,
        requests: 1,
        hold_ack: 0,
    };
    let mut events = h.client();
    events.subscribe(vec![Topic::Status]);
    let applies = h.engine.applies.load(std::sync::atomic::Ordering::Relaxed);
    h.engine.set_snapshot(|s| {
        s.state = EngineState::Playing;
        s.active_sink = Some(SinkChoice::Dsp);
        s.dsp_load = 1.4;
        s.output_rate = 768_000;
    });
    for (action, hold) in [
        (OverloadAction::VarispeedBudget, VarispeedHold::Budget),
        (OverloadAction::VarispeedUnity, VarispeedHold::Unity),
        (OverloadAction::VarispeedOff, VarispeedHold::Off),
    ] {
        let report = applied_overload(&mut events);
        assert_eq!(report.action, action, "{}", report.message);
        assert_eq!(report.stage, "varispeed");
        assert_eq!(h.engine.varispeed_holds.lock().unwrap().last(), Some(&hold));
        assert_eq!(
            h.engine.applies.load(std::sync::atomic::Ordering::Relaxed),
            applies,
            "a varispeed step applies no config"
        );
        assert_eq!(status(&h).overload.as_ref(), Some(&report));
    }
    let report = applied_overload(&mut events);
    assert_eq!(report.action, OverloadAction::UpsamplerOff);
    assert!(
        report
            .message
            .ends_with("Varispeed stays off while this runs."),
        "{}",
        report.message
    );
    wait_until("the fallback", Duration::from_secs(5), || {
        h.engine.config_now().chain.upsampler.target == RateTarget::Off
    });
    assert_eq!(
        *h.engine.varispeed_holds.lock().unwrap(),
        [
            VarispeedHold::Budget,
            VarispeedHold::Unity,
            VarispeedHold::Off
        ]
    );
    assert!(!h.engine.varispeed.lock().unwrap().running);
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

#[test]
fn a_refused_step_moves_on_and_a_refused_recovery_keeps_the_notice() {
    let h = Harness::start_with("overload-refused", |cfg| {
        cfg.rate.allowed_rates = vec![48_000];
        cfg.chain.upsampler = manual(4 << 20);
    });
    *h.engine.cpu_rates.lock().unwrap() = Some(slow_machine());
    // The engine refuses to run without the upsampler (say its reopen
    // cannot be planned), and, when `refuse_all` is set, everything.
    let refuse_all = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let engine = Arc::downgrade(&h.engine);
    let all = refuse_all.clone();
    *h.engine.on_apply.lock().unwrap() = Some(Arc::new(move |cfg: &EngineConfig| {
        let Some(engine) = engine.upgrade() else {
            return;
        };
        let refuse = all.load(std::sync::atomic::Ordering::Relaxed)
            || cfg.chain.upsampler.target == RateTarget::Off;
        *engine.fail_apply.lock().unwrap() =
            refuse.then(|| EngineError::new(RpcError::INTERNAL, "cannot reopen"));
    }));
    let mut events = h.client();
    events.subscribe(vec![Topic::Status]);
    h.engine.set_snapshot(|s| {
        s.state = EngineState::Playing;
        s.active_sink = Some(SinkChoice::Dsp);
        s.dsp_load = 1.4;
        s.output_rate = 768_000;
    });
    // Upsampler off refused: the bypass path is next.
    let report = applied_overload(&mut events);
    assert_eq!(report.action, OverloadAction::Bypass);
    let running = h.engine.config_now();
    assert!(!running.dsp_enabled);
    h.engine.set_snapshot(|s| s.dsp_load = 0.2);

    // A recovery edit the engine refuses: an error, the notice stays and
    // the engine keeps the fallback.
    refuse_all.store(true, std::sync::atomic::Ordering::Relaxed);
    let light = light_up();
    match h.client().call(Call::SetUpsampler(light.clone())) {
        Reply::Error(e) => assert!(e.message.contains("cannot reopen"), "{}", e.message),
        Reply::Ok(v) => panic!("applied: {v}"),
    }
    assert_eq!(status(&h).overload.as_ref(), Some(&report));
    assert_eq!(h.engine.config_now(), running);
    // Accepted, it ends the fallback.
    refuse_all.store(false, std::sync::atomic::Ordering::Relaxed);
    h.client().ok(Call::SetUpsampler(light));
    assert_eq!(
        h.engine.config_now().chain.upsampler.target,
        RateTarget::Ratio(4)
    );
    assert!(status(&h).overload.is_none());
}

fn caps(rates: &[u32]) -> audivane_protocol::ipc::DeviceCapabilities {
    use audivane_protocol::SampleFormat;
    use audivane_protocol::ipc::{DeviceCapabilities, FormatCapability, RateCapability};
    DeviceCapabilities {
        device: "hw:0,0".into(),
        rates: rates
            .iter()
            .map(|&rate| RateCapability {
                rate,
                formats: vec![FormatCapability {
                    format: SampleFormat::S32,
                    sbits: None,
                }],
            })
            .collect(),
        channels: 2,
        channels_min: 2,
        channels_max: 2,
        rate_min: 0,
        rate_max: 0,
        formats: vec![SampleFormat::S32],
        modes: Vec::new(),
    }
}

/// A heavy design at `ratio` on a DAC that plays only `rates`, overloaded.
fn overloaded_on(name: &str, ratio: u32, rates: &[u32]) -> (Harness, TestClient) {
    let h = Harness::start_with(name, |cfg| {
        cfg.rate.allowed_rates = vec![48_000];
        cfg.rate.default_rate = 48_000;
        cfg.chain.upsampler = UpsamplerConfig {
            target: RateTarget::Ratio(ratio),
            ..manual(4 << 20)
        };
    });
    let mut events = h.client();
    events.subscribe(vec![Topic::Status]);
    let device_caps = caps(rates);
    h.engine.set_snapshot(|s| {
        s.device_caps = Some(device_caps);
        s.state = EngineState::Playing;
        s.active_sink = Some(SinkChoice::Dsp);
        s.dsp_load = 1.4;
        s.output_rate = 48_000 * ratio;
    });
    (h, events)
}

#[test]
fn on_a_dac_without_the_graph_rate_dsp_off_goes_through_the_bypass_resampler() {
    // A DAC with only 192 kHz and a 48 kHz graph: the upsampler off plays
    // (the driver moves the graph to 192 kHz, PipeWire converts); still
    // overloaded, DSP off keeps the graph at 48 kHz and the engine bypass
    // resampler converts it (resample-only).
    let (h, mut events) = overloaded_on("overload-192", 4, &[192_000]);
    let report = applied_overload(&mut events);
    assert_eq!(report.action, OverloadAction::UpsamplerOff);
    let report = applied_overload(&mut events);
    assert_eq!(report.action, OverloadAction::Bypass);
    assert!(
        report
            .message
            .contains("the engine's bypass resampler converts it"),
        "{}",
        report.message
    );
    assert!(!h.engine.config_now().dsp_enabled);
}

#[test]
fn when_not_even_the_bypass_lands_the_light_filter_keeps_the_rate() {
    // A DAC with only 1.536 MHz: neither x1 nor the bypass resampler
    // (which converts to the graph rates) lands; the bypass resampler's
    // design at the requested x32 keeps it playing.
    let (h, mut events) = overloaded_on("overload-1536", 32, &[1_536_000]);
    let report = applied_overload(&mut events);
    assert_eq!(report.action, OverloadAction::LightUpsampler);
    let running = h.engine.config_now();
    assert!(running.dsp_enabled);
    assert_eq!(running.chain.upsampler.target, RateTarget::Ratio(32));
    assert_eq!(
        running.chain.upsampler,
        audivane_dsp_daemon::control::overload::cheapest_upsampler(&UpsamplerConfig {
            target: RateTarget::Ratio(32),
            ..manual(4 << 20)
        })
    );
}

#[test]
fn a_fallback_the_next_dac_cannot_play_is_replaced() {
    // Upsampler off, applied on a DAC that plays only 48 kHz.
    let (h, mut events) = overloaded_on("overload-swap", 32, &[48_000]);
    let report = applied_overload(&mut events);
    assert_eq!(report.action, OverloadAction::UpsamplerOff);
    assert_eq!(
        h.engine.config_now().chain.upsampler.target,
        RateTarget::Off
    );
    // Calm without it; then another DAC behind the same name that plays
    // only 1.536 MHz, where nothing plays at x1 or through the bypass
    // resampler: the bypass resampler's filter at the requested x32.
    h.engine.set_snapshot(|s| {
        s.dsp_load = 0.2;
        s.output_rate = 48_000;
    });
    std::thread::sleep(Duration::from_millis(500));
    let other = caps(&[1_536_000]);
    h.engine.set_snapshot(|s| s.device_caps = Some(other));
    let report = applied_overload(&mut events);
    assert_eq!(report.action, OverloadAction::LightUpsampler);
    let running = h.engine.config_now();
    assert!(running.dsp_enabled);
    assert_eq!(running.chain.upsampler.target, RateTarget::Ratio(32));
}

#[test]
fn the_startup_config_is_the_one_an_overload_goes_back_to() {
    // Started with a light design that runs calm at x4.
    let light = light_up();
    let start = light.clone();
    let h = Harness::start_with("overload-seed", move |cfg| {
        cfg.rate.allowed_rates = vec![48_000];
        cfg.chain.upsampler = start;
    });
    let mut events = h.client();
    events.subscribe(vec![Topic::Status]);
    h.engine.set_snapshot(|s| {
        s.state = EngineState::Playing;
        s.active_sink = Some(SinkChoice::Dsp);
        s.dsp_load = 0.2;
        s.output_rate = 192_000;
        s.ratio = audivane_protocol::RateRatio::integer(4);
    });
    // Its settle and three calm seconds, with margin.
    std::thread::sleep(Duration::from_millis(5_000));
    // A heavy design (no CPU admission here) that overloads at x16: back
    // to the startup design.
    h.client().ok(Call::SetUpsampler(manual(4 << 20)));
    h.engine.set_snapshot(|s| {
        s.dsp_load = 1.4;
        s.output_rate = 768_000;
        s.ratio = audivane_protocol::RateRatio::integer(16);
    });
    let report = applied_overload(&mut events);
    assert_eq!(report.action, OverloadAction::Reverted);
    assert_eq!(h.engine.config_now().chain.upsampler, light);
}

#[test]
fn set_varispeed_ends_varispeed_overload_step_and_releases_hold() {
    use audivane_dsp_daemon::api::{VarispeedHold, VarispeedRequest};

    let h = Harness::start_with("overload-vs-end", |cfg| {
        cfg.rate.allowed_rates = vec![48_000];
        cfg.chain.upsampler = manual(4 << 20);
        cfg.varispeed.enabled = true;
        cfg.varispeed.speed = 1.5;
    });
    *h.engine.varispeed.lock().unwrap() = VarispeedRequest {
        wanted: true,
        running: true,
        speed: 1.5,
        budget: 0.25,
        requests: 1,
        hold_ack: 0,
    };
    let mut events = h.client();
    events.subscribe(vec![Topic::Status]);
    h.engine.set_snapshot(|s| {
        s.state = EngineState::Playing;
        s.active_sink = Some(SinkChoice::Dsp);
        s.dsp_load = 1.4;
        s.output_rate = 768_000;
    });

    // Wait for the first varispeed overload step (Budget).
    let report = applied_overload(&mut events);
    assert_eq!(report.action, OverloadAction::VarispeedBudget);
    assert_eq!(
        h.engine.varispeed_holds.lock().unwrap().last(),
        Some(&VarispeedHold::Budget)
    );
    assert!(status(&h).overload.is_some());

    // Lower the load so the fresh request is not immediately re-tripped,
    // then issue an empty `set_varispeed {}`: even with all fields omitted,
    // it follows the normal request path, ends the fallback step, and releases
    // the hold at once.
    h.engine.set_snapshot(|s| s.dsp_load = 0.2);
    h.client().ok(Call::SetVarispeed {
        enabled: None,
        speed: None,
        ramp_ms: None,
        keep_pitch: None,
    });
    assert_eq!(status(&h).overload, None);
    assert_eq!(
        h.engine.varispeed_holds.lock().unwrap().last(),
        Some(&VarispeedHold::None)
    );
    assert_eq!(
        h.engine.varispeed_calls.lock().unwrap().last(),
        Some(&(true, 1.5, 200.0))
    );

    // Trigger another varispeed overload step (restoring measured budget below
    // 0.5 as the real driver does once `Hold::None` releases the pin) and end
    // it with an explicit speed change.
    h.engine.varispeed.lock().unwrap().budget = 0.25;
    h.engine.set_snapshot(|s| s.dsp_load = 1.4);
    let report = applied_overload(&mut events);
    assert_eq!(report.action, OverloadAction::VarispeedBudget);
    assert!(status(&h).overload.is_some());

    h.engine.set_snapshot(|s| s.dsp_load = 0.2);
    h.client().ok(Call::SetVarispeed {
        enabled: Some(true),
        speed: Some(1.1),
        ramp_ms: Some(200.0),
        keep_pitch: None,
    });
    assert_eq!(status(&h).overload, None);
    assert_eq!(
        h.engine.varispeed_holds.lock().unwrap().last(),
        Some(&VarispeedHold::None)
    );
}

/// A design to 768 kHz (x16 from 48 kHz, x4 from 192 kHz) and its config
/// over both rates: on the slow machine it fits at 48 kHz and not at 192
/// kHz.
fn per_rate_config(cfg: &mut EngineConfig) {
    cfg.rate.allowed_rates = vec![48_000, 192_000];
    cfg.chain.upsampler = UpsamplerConfig {
        target: RateTarget::MaxRate(768_000),
        ..manual(1 << 20)
    };
}

#[test]
fn a_design_that_fits_the_rate_playing_is_admitted_and_falls_back_per_rate() {
    use audivane_protocol::ipc::RateFallbackCause;
    let h = Harness::start_with("overload-per-rate", |cfg| {
        cfg.rate.allowed_rates = vec![48_000, 192_000];
    });
    *h.engine.cpu_rates.lock().unwrap() = Some(slow_machine());
    let mut want = h.engine.config_now();
    per_rate_config(&mut want);
    let rates = slow_machine();
    let costs =
        audivane_dsp_daemon::control::cpu_budget::landing_costs(rates, &want, None).unwrap();
    let at = |r: u32| {
        costs
            .iter()
            .find(|(_, w)| w.source_rate == r)
            .unwrap()
            .1
            .load
    };
    let budget = audivane_dsp_daemon::control::cpu_budget::RT_BUDGET;
    assert!(at(48_000) <= budget && at(192_000) > budget, "{costs:?}");

    // Playing 192 kHz: refused, naming the source rate.
    h.engine.set_snapshot(|s| {
        s.state = EngineState::Playing;
        s.active_sink = Some(SinkChoice::Dsp);
        s.graph_rate = 192_000;
        s.output_rate = 192_000;
    });
    match h
        .client()
        .call(Call::SetUpsampler(want.chain.upsampler.clone()))
    {
        Reply::Error(e) => assert!(
            e.message
                .starts_with("Too heavy for this CPU at 768 kHz from a 192 kHz source"),
            "{}",
            e.message
        ),
        other => panic!("expected a refusal, got {other:?}"),
    }
    // Playing 48 kHz: admitted; 192 kHz falls back by the estimate.
    h.engine.set_snapshot(|s| {
        s.graph_rate = 48_000;
        s.output_rate = 768_000;
        s.ratio = audivane_protocol::RateRatio::integer(16);
    });
    h.client()
        .ok(Call::SetUpsampler(want.chain.upsampler.clone()));
    assert_eq!(h.engine.config_now().chain.upsampler, want.chain.upsampler);
    let plan = h.engine.rate_plan.lock().unwrap().clone();
    let cfg = h.engine.config_now();
    assert!(plan.substitute(&cfg, 48_000).is_none());
    let light = plan
        .substitute(&cfg, 192_000)
        .expect("192 kHz falls back")
        .clone();
    assert_eq!(light.target, want.chain.upsampler.target);
    let st = status(&h);
    let row = |st: &Status, r: u32| {
        st.upsampler_rates
            .iter()
            .find(|w| w.source_rate == r)
            .cloned()
            .unwrap()
    };
    assert_eq!(row(&st, 48_000).fallback, None);
    let f = row(&st, 192_000).fallback.unwrap();
    assert_eq!(f.action, OverloadAction::Recommended);
    assert_eq!(f.cause, RateFallbackCause::Estimate);
    assert_eq!(row(&st, 192_000).output_rate, 768_000);

    // A wrong estimate: the request overloads at 48 kHz. The reactive
    // fallback runs there (the net), made for 48 kHz.
    let mut events = h.client();
    events.subscribe(vec![Topic::Status]);
    h.engine.set_snapshot(|s| s.dsp_load = 1.4);
    let report = applied_overload(&mut events);
    assert_eq!(report.source_rate, 48_000);
    assert_eq!(report.action, OverloadAction::UpsamplerOff);
    // Playback moves to 192 kHz: the fallback ends (per rate), the request
    // runs again and the plan builds the light design there; 48 kHz now
    // counts as overloaded.
    h.engine.set_snapshot(|s| {
        s.dsp_load = 0.2;
        s.graph_rate = 192_000;
        s.output_rate = 768_000;
        s.ratio = audivane_protocol::RateRatio::integer(4);
    });
    wait_until("the per-rate end", Duration::from_secs(5), || {
        status(&h).overload.is_none()
    });
    assert_eq!(h.engine.config_now().chain.upsampler, want.chain.upsampler);
    wait_until("48 kHz marked", Duration::from_secs(5), || {
        row(&status(&h), 48_000)
            .fallback
            .is_some_and(|f| f.cause == RateFallbackCause::Overload)
    });
    let plan = h.engine.rate_plan.lock().unwrap().clone();
    assert!(plan.substitute(&cfg, 48_000).is_some());
    assert!(plan.substitute(&cfg, 192_000).is_some());

    // A new request starts afresh: a lighter design runs everywhere.
    h.client().ok(Call::SetUpsampler(UpsamplerConfig {
        target: RateTarget::MaxRate(768_000),
        ..manual(4095)
    }));
    let st = status(&h);
    assert!(
        st.upsampler_rates.iter().all(|r| r.fallback.is_none()),
        "{:?}",
        st.upsampler_rates
    );
    assert_eq!(
        *h.engine.rate_plan.lock().unwrap(),
        audivane_dsp_daemon::api::RatePlan::default()
    );
}

#[test]
fn returning_to_a_rate_the_design_fits_runs_it_again() {
    // 192 kHz falls back by the estimate; 48 kHz runs the request: the
    // plan is per rate, so nothing sticks after a 192 kHz track.
    let h = Harness::start_with("overload-return", |cfg| {
        cfg.rate.allowed_rates = vec![48_000, 192_000];
    });
    *h.engine.cpu_rates.lock().unwrap() = Some(slow_machine());
    let mut want = h.engine.config_now();
    per_rate_config(&mut want);
    h.engine.set_snapshot(|s| {
        s.state = EngineState::Playing;
        s.active_sink = Some(SinkChoice::Dsp);
        s.output_rate = 768_000;
        s.ratio = audivane_protocol::RateRatio::integer(16);
    });
    h.client()
        .ok(Call::SetUpsampler(want.chain.upsampler.clone()));
    let cfg = h.engine.config_now();
    for rate in [192_000, 48_000, 192_000, 48_000] {
        h.engine.set_snapshot(|s| {
            s.state = EngineState::Playing;
            s.active_sink = Some(SinkChoice::Dsp);
            s.graph_rate = rate;
            s.output_rate = 768_000;
            s.ratio = audivane_protocol::RateRatio::integer(768_000 / rate);
        });
        std::thread::sleep(Duration::from_millis(400));
        let plan = h.engine.rate_plan.lock().unwrap().clone();
        assert_eq!(
            plan.substitute(&cfg, rate).is_some(),
            rate == 192_000,
            "{rate}"
        );
        assert_eq!(h.engine.config_now().chain.upsampler, want.chain.upsampler);
        assert!(status(&h).overload.is_none());
    }
}
