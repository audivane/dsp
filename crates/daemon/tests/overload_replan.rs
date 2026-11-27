//! Regression tests for the overload fallback's replan, through the public
//! supervisor API: an open failure without a fresh probe never advances a
//! fallback, a stalled reopen of a confirmed fallback times out, and a
//! replan that runs out of steps ends the fallback (also again after the
//! engine refused that end).

use audivane_dsp_daemon::control::overload::{Landing, Sample, Step, Supervisor};
use audivane_protocol::{RateRatio, RateTarget, ipc::EngineConfig};

fn request() -> EngineConfig {
    let mut config = EngineConfig {
        device: "hw:CARD=USB,DEV=0".into(),
        ..EngineConfig::default()
    };
    config.chain.upsampler.target = RateTarget::Ratio(32);
    config
}

fn all(_: &EngineConfig) -> Landing {
    Landing::Plays
}

fn off_confirmed(cfg: &EngineConfig) -> (Supervisor, Sample) {
    let mut supervisor = Supervisor::default();
    supervisor.seed(cfg, 1);
    let mut sample = Sample {
        running: true,
        device: 1,
        load: 1.4,
        output_rate: 768_000,
        ratio: RateRatio::integer(16),
        chain_generation: 1,
        ..Sample::default()
    };
    for _ in 0..40 {
        if let Step::Try(t) = supervisor.observe(sample, cfg, &all) {
            assert_eq!(
                supervisor.config_for(&t, cfg).chain.upsampler.target,
                RateTarget::Off
            );
            supervisor.commit(t, cfg, 2);
            sample.chain_generation = 2;
            sample.load = 0.25;
            assert_eq!(supervisor.observe(sample, cfg, &all), Step::Confirmed);
            return (supervisor, sample);
        }
    }
    panic!("failed to establish a confirmed fallback");
}

#[test]
fn confirmed_fallback_times_out_if_format_reopen_hangs() {
    let mut cfg = request();
    let (mut sup, mut s) = off_confirmed(&cfg);
    cfg.formats.reverse();
    sup.commit_request(&cfg, 3);
    s.running = false;
    s.switching = true;
    s.wanted = true;
    let advanced = (0..110).any(|_| matches!(sup.observe(s, &cfg, &all), Step::Try(_)));
    assert!(
        advanced,
        "confirmed fallback never times out when its reopen stalls"
    );
}
