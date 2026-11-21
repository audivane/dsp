//! Wire round-trip test and legacy flat-config startup fallback.
//!
//! For every catalogue design (all 66 [`FilterPreset`]s plus `"recommended"`):
//! 1. Expand the catalogue design with `based_on: <name>` and serialize it to
//!    `serde_json::Value`, as a client sends it, with a `target` added
//!    (`preset: null`).
//! 2. Send the JSON as a `set_upsampler` RPC through the daemon's control
//!    service, read back `get_config`, and verify `based_on` and the design
//!    survive the wire round trip intact.
//! 3. Build a [`Chain`] from the round-tripped config and compare its upsampler
//!    kernel/runtime inputs (captured via `test_support::capture`), latency and
//!    two-channel impulse response against the preset's own build at 0 dB
//!    output headroom, verifying that a non-zero `output.headroom_db` appears
//!    only as the `scale_output` factor (`10^(-headroom_db / 20)`).

mod common;

use audivane_dsp_core::chain::{Chain, ChainSpec};
use audivane_dsp_core::design::{Named, StoredDesign, catalogue};
use audivane_dsp_core::test_support::capture;
use audivane_dsp_daemon::control::config::ConfigStore;
use audivane_protocol::ipc::{Call, EngineConfig, Request};
use audivane_protocol::{
    ChainConfig, DitherKind, FilterPreset, OutputConfig, RateTarget, UpsamplerConfig,
};
use common::{Harness, TempDir};
use serde_json::{Value, json};

const HEADROOM_DB: f64 = 3.5;

fn db_to_lin(db: f64) -> f64 {
    10.0_f64.powf(db / 20.0)
}

#[test]
fn every_catalogue_design_round_trips_over_the_wire_bit_exact() {
    let harness = Harness::start("design-wire-roundtrip");
    let mut client = harness.client();

    // All 66 FilterPreset names plus "recommended".
    let mut names: Vec<&str> = FilterPreset::ALL.iter().map(|p| p.name()).collect();
    names.push(catalogue::RECOMMENDED);

    let output = OutputConfig {
        bits: 32,
        dither: DitherKind::None,
        headroom_db: 0.0,
    };

    for name in names {
        let expanded = catalogue::by_name(name).expect("catalogue design");
        let stored = StoredDesign::based_on(name, expanded.clone());

        // 1. Serialize through serde_json::Value as a client sends it.
        let design_val: Value = serde_json::to_value(&stored).expect("serialize design");
        assert_eq!(design_val["based_on"], json!(name), "{name}");

        let target = RateTarget::Ratio(4);
        let req_val = json!({
            "id": 1,
            "method": "set_upsampler",
            "params": {
                "target": target,
                "preset": null,
                "design": design_val,
            }
        });

        // 2. Round-trip through the JSON-RPC wire path and daemon control handler.
        let req: Request = serde_json::from_value(req_val).expect("parse request");
        let Call::SetUpsampler(up_cfg) = req.call else {
            panic!("expected SetUpsampler");
        };
        let _ = client.ok(Call::SetUpsampler(up_cfg));

        let got_cfg: EngineConfig =
            serde_json::from_value(client.ok(Call::GetConfig)).expect("get_config");
        let rt_up = &got_cfg.chain.upsampler;
        assert_eq!(rt_up.preset, None, "{name}");
        let rt_stored = rt_up.design.as_ref().expect("stored design");
        assert_eq!(rt_stored.based_on.as_deref(), Some(name), "{name}");
        assert_eq!(rt_stored.design, expanded, "{name}");

        // 3. Build the chain from the wire-round-tripped custom design vs the
        //    preset by name (at 0 dB headroom) and verify bit-identical kernel
        //    and latency, with headroom showing up only as `scale_output`.
        let preset_up = UpsamplerConfig {
            target,
            preset: Some(name.to_string()),
            design: None,
        };
        let make_spec = |upsampler: UpsamplerConfig, headroom_db: f64| ChainSpec {
            config: ChainConfig {
                upsampler,
                output: OutputConfig {
                    headroom_db,
                    ..output
                },
                ..ChainConfig::neutral()
            },
            sample_rate: 44_100,
            channels: 2,
            max_block: 256,
            irs: None,
        };

        let (preset_chain, preset_records) = capture(|| Chain::try_new(make_spec(preset_up, 0.0)));
        let (wire_chain, wire_records) =
            capture(|| Chain::try_new(make_spec(rt_up.clone(), HEADROOM_DB)));
        let mut preset_chain = preset_chain.unwrap_or_else(|e| panic!("{name} preset build: {e}"));
        let mut wire_chain = wire_chain.unwrap_or_else(|e| panic!("{name} wire build: {e}"));

        assert_eq!(
            wire_chain.latency_out_frames(),
            preset_chain.latency_out_frames(),
            "{name}: latency mismatch"
        );
        assert_eq!(
            wire_chain.ratio(),
            preset_chain.ratio(),
            "{name}: ratio mismatch"
        );
        assert_eq!(
            wire_records, preset_records,
            "{name}: kernel/runtime inputs differ"
        );

        // Verify headroom shows up only as the `scale_output` factor on output.
        let mut inp = vec![0.0_f32; 256 * 2];
        inp[0] = 0.5;
        inp[1] = -0.25;
        let mut out_preset = vec![0_i32; 256 * 4 * 2];
        let mut out_wire = vec![0_i32; 256 * 4 * 2];
        let want_scale = db_to_lin(-HEADROOM_DB);
        // Clock through enough blocks to see non-zero response on short filters too.
        for block in 0..4 {
            let src = if block == 0 {
                &inp[..]
            } else {
                &[0.0_f32; 256 * 2][..]
            };
            preset_chain.process_f32(src, &mut out_preset);
            wire_chain.process_f32(src, &mut out_wire);
            for (&a, &b) in out_preset.iter().zip(&out_wire) {
                let expected = (f64::from(a) * want_scale).round() as i64;
                assert!(
                    (i64::from(b) - expected).abs() <= 2,
                    "{name}: output scale mismatch: preset={a} wire={b} expected={expected}"
                );
            }
        }

        // Sanity: Named::of agrees on the design.
        assert_eq!(Named::of(rt_up).unwrap().design, expanded, "{name}");
    }
}

#[test]
fn old_flat_config_starts_daemon_cleanly() {
    let dir = TempDir::new("legacy-config");
    let path = dir.path().join("config.json");

    // 1. Old flat custom config (preset: null) falls back cleanly to recommended.
    let old_custom = r#"{
        "version": 1,
        "config": {
            "device": "hw:CARD=USB,DEV=0",
            "chain": {
                "upsampler": {
                    "target": { "ratio": 4 },
                    "taps": 65536,
                    "phase": "minimum",
                    "kaiser_beta": 14.0,
                    "cutoff": 1.0,
                    "cutoff_mode": "fraction",
                    "taps_unit": "output",
                    "phase_mix": 0.5,
                    "headroom_db": 1.0,
                    "preserve_samples": false,
                    "window": "kaiser",
                    "taper": 0.25,
                    "preset": null
                }
            }
        }
    }"#;
    std::fs::write(&path, old_custom).unwrap();
    let loaded = ConfigStore::new(&path).load();
    assert!(loaded.warning.is_none(), "{:?}", loaded.warning);
    assert_eq!(loaded.config.chain.upsampler.target, RateTarget::Ratio(4));
    assert_eq!(loaded.config.chain.upsampler.preset, None);
    assert_eq!(loaded.config.chain.upsampler.design, None);

    // 2. Old preset config keeps its preset name and target.
    let old_preset = r#"{
        "version": 1,
        "config": {
            "device": "hw:CARD=USB,DEV=0",
            "chain": {
                "upsampler": {
                    "target": { "max_rate": 768000 },
                    "taps": 65536,
                    "headroom_db": 0.5,
                    "preset": "flattop_long"
                }
            }
        }
    }"#;
    std::fs::write(&path, old_preset).unwrap();
    let loaded = ConfigStore::new(&path).load();
    assert!(loaded.warning.is_none(), "{:?}", loaded.warning);
    assert_eq!(
        loaded.config.chain.upsampler.preset.as_deref(),
        Some("flattop_long")
    );
    assert_eq!(loaded.config.chain.upsampler.design, None);
}
