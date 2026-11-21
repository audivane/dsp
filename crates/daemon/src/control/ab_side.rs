//! A/B side B, materialised from its description ([`AbB`]) and chain A.
//!
//! A patch B is a JSON merge patch (RFC 7386) on chain A, re-applied on
//! every config edit and load, so B follows A everywhere the patch does not
//! reach. A chain B (a stored preset) keeps its own stages and filter
//! design; its upsampler target, limiter and output follow chain A, so a
//! preset never quietly becomes an output compare.
//!
//! A patch may set B's `output.bits` and `output.dither` (the noise shaper
//! is a dither kind): the pair then runs a second quantizer and cuts
//! between the two quantized streams at a switch
//! ([`audivane_dsp_core::chain::AbChain`]). The device, its format and the
//! rate are the engine's, not a chain's: a patch naming them is refused.

use audivane_protocol::ChainConfig;
use audivane_protocol::ipc::AbB;
use serde_json::{Map, Value};

/// Why a field may not be patched: both sides share it.
const SHARED: [(&str, &str); 5] = [
    ("limiter", "both sides share the limiter after the mix"),
    ("device", "both sides play through the same device"),
    ("formats", "both sides play through the same device format"),
    ("format", "both sides play through the same device format"),
    ("rate", "both sides play at the same output rate"),
];

/// The `output` fields side B may set: its own quantizer's.
const OUTPUT_OWN: [&str; 2] = ["bits", "dither"];

/// Chain B as its stages are built: with chain A's bits and dither, so an
/// automatic (`match_output`) filter budget resolves the same on both
/// sides. B's own bits and dither apply to its quantizer only (the engine
/// builds the pair the same way, [`audivane_dsp_core::chain::AbChain`]).
pub fn design_view(b: &ChainConfig, a: &ChainConfig) -> ChainConfig {
    let mut b = b.clone();
    b.output.bits = a.output.bits;
    b.output.dither = a.output.dither;
    b
}

/// The chains whose filters `cfg` builds, as built ([`design_view`] for
/// side B): chain A and, with A/B, chain B.
pub fn designed_chains(cfg: &audivane_protocol::ipc::EngineConfig) -> Vec<ChainConfig> {
    std::iter::once(cfg.chain.clone())
        .chain(
            cfg.ab
                .as_ref()
                .map(|ab| design_view(&ab.chain_b, &cfg.chain)),
        )
        .collect()
}

/// Chain B for `b` against chain A, or why it cannot be built (the patch
/// path named).
pub fn materialize(a: &ChainConfig, b: &AbB) -> Result<ChainConfig, String> {
    match b {
        AbB::Chain(chain) => {
            let mut chain = (**chain).clone();
            align_chain(&mut chain, a);
            Ok(chain)
        }
        AbB::Patch(patch) => {
            check_patch(patch)?;
            let base = serde_json::to_value(a).map_err(|e| format!("chain A: {e}"))?;
            let mut merged = base.clone();
            merge(&mut merged, patch);
            serde_json::from_value(merged).map_err(|err| {
                // Name the first top-level key that fails on its own.
                let key = patch.as_object().and_then(|p| {
                    p.iter().find_map(|(k, v)| {
                        let mut one = base.clone();
                        merge(
                            &mut one,
                            &Value::Object(Map::from_iter([(k.clone(), v.clone())])),
                        );
                        serde_json::from_value::<ChainConfig>(one)
                            .is_err()
                            .then(|| k.clone())
                    })
                });
                match key {
                    Some(k) => format!("ab.b.patch.{k}: {err}"),
                    None => format!("ab.b.patch: {err}"),
                }
            })
        }
    }
}

/// A patch is an object that leaves the shared fields alone. A ratio
/// compare would need the device reopened at each switch.
pub fn check_patch(patch: &Value) -> Result<(), String> {
    let obj = patch
        .as_object()
        .ok_or("ab.b.patch must be a JSON object")?;
    for (key, why) in SHARED {
        if obj.contains_key(key) {
            return Err(format!(
                "ab.b.patch.{key} may not differ from chain A ({why})"
            ));
        }
    }
    if let Some(output) = obj.get("output") {
        let output = output.as_object().ok_or(
            "ab.b.patch.output must be an object (side B may set its own bits and dither)",
        )?;
        if let Some(key) = output.keys().find(|k| !OUTPUT_OWN.contains(&k.as_str())) {
            let why = if key == "headroom_db" {
                "both sides take the same output headroom before the mix"
            } else {
                "side B may set only its own bits and dither; the device, its format and \
                 the rate are shared"
            };
            return Err(format!(
                "ab.b.patch.output.{key} may not differ from chain A ({why})"
            ));
        }
    }
    if let Some(up) = obj.get("upsampler") {
        let up = up.as_object().ok_or(
            "ab.b.patch.upsampler must be an object (B runs at chain A's ratio; \
             a filter may differ)",
        )?;
        if up.contains_key("target") {
            return Err(
                "ab.b.patch.upsampler.target may not differ from chain A (a ratio \
                        compare needs the DAC reopened, not supported in A/B yet)"
                    .into(),
            );
        }
    }
    Ok(())
}

/// Copy chain A's upsampler target, limiter and output (bits, dither and
/// headroom) into a chain B. The filter design (`preset`, `design`) stays
/// B's own. Returns the fields that changed.
pub fn align_chain(b: &mut ChainConfig, a: &ChainConfig) -> Vec<&'static str> {
    let mut changed = Vec::new();
    if b.upsampler.target != a.upsampler.target {
        b.upsampler.target = a.upsampler.target;
        changed.push("upsampler.target");
    }
    if b.limiter != a.limiter {
        b.limiter = a.limiter.clone();
        changed.push("limiter");
    }
    if b.output != a.output {
        b.output = a.output;
        changed.push("output");
    }
    changed
}

/// RFC 7386 merge, with one addition: a patch that only removes fields
/// from a stage chain A does not have leaves that stage absent (tilt off
/// with no EQ in A stays no EQ, not an empty profile).
fn merge(target: &mut Value, patch: &Value) {
    let Value::Object(p) = patch else {
        *target = patch.clone();
        return;
    };
    let absent = target.is_null();
    if !target.is_object() {
        *target = Value::Object(Map::new());
    }
    let Value::Object(t) = target else {
        return;
    };
    for (key, value) in p {
        if value.is_null() {
            t.remove(key);
        } else {
            merge(t.entry(key.clone()).or_insert(Value::Null), value);
        }
    }
    if absent && t.is_empty() && !p.is_empty() {
        *target = Value::Null;
    }
}

/// The paths (`convolution`, `eq.bands`) where a patch replaces one of
/// chain A's lists with a non-empty list of its own: B keeps that list
/// when A's changes. An empty list ("off") is the same whatever A has.
pub fn replaced_lists(patch: &Value) -> Vec<String> {
    fn walk(prefix: &str, v: &Value, out: &mut Vec<String>) {
        let Value::Object(map) = v else { return };
        for (k, v) in map {
            let path = if prefix.is_empty() {
                k.clone()
            } else {
                format!("{prefix}.{k}")
            };
            match v {
                Value::Array(items) if !items.is_empty() => out.push(path),
                Value::Object(_) => walk(&path, v, out),
                _ => {}
            }
        }
    }
    let mut out = Vec::new();
    walk("", patch, &mut out);
    out
}

/// The value at a dotted `path` in chain A.
fn value_at(chain: &ChainConfig, path: &str) -> Option<Value> {
    let mut v = serde_json::to_value(chain).ok()?;
    for key in path.split('.') {
        v = v.get_mut(key)?.take();
    }
    Some(v)
}

/// Notes for a new B: the lists it keeps as they are now.
pub fn set_notes(b: &AbB) -> Vec<String> {
    let AbB::Patch(patch) = b else {
        return Vec::new();
    };
    replaced_lists(patch)
        .into_iter()
        .map(|path| {
            format!(
                "side B keeps its own {path} list: later edits to chain A's list do not \
                 reach B until it is set again"
            )
        })
        .collect()
}

/// Notes for an edit of chain A from `old` to `new` under a patch B: the
/// lists A changed that B replaces, so B kept its own.
pub fn edit_notes(b: &AbB, old: &ChainConfig, new: &ChainConfig) -> Vec<String> {
    let AbB::Patch(patch) = b else {
        return Vec::new();
    };
    replaced_lists(patch)
        .into_iter()
        .filter(|path| value_at(old, path) != value_at(new, path))
        .map(|path| format!("chain A's {path} changed; side B keeps its own {path} list"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use audivane_protocol::{
        CrossfeedConfig, DitherKind, EqProfile, LimiterMode, RateTarget, TiltConfig,
    };
    use serde_json::json;

    fn chain_a() -> ChainConfig {
        ChainConfig {
            crossfeed: Some(CrossfeedConfig::default()),
            eq: Some(EqProfile {
                preamp_db: -2.0,
                ..EqProfile::default()
            }),
            upsampler: audivane_protocol::UpsamplerConfig {
                target: RateTarget::Ratio(4),
                preset: Some("recommended".into()),
                design: None,
            },
            ..ChainConfig::default()
        }
    }

    #[test]
    fn a_patch_changes_only_what_it_names() {
        let a = chain_a();
        let b = materialize(&a, &AbB::Patch(json!({"crossfeed": {"enabled": false}}))).unwrap();
        assert!(!b.crossfeed.as_ref().unwrap().enabled);
        assert_eq!(b.eq, a.eq);
        assert_eq!(b.upsampler, a.upsampler);
        let b = materialize(&a, &AbB::Patch(json!({"eq": null, "convolution": []}))).unwrap();
        assert_eq!(b.eq, None);
        assert_eq!(b.crossfeed, a.crossfeed);
        let b = materialize(
            &a,
            &AbB::Patch(json!({"upsampler": {"preset": "other", "design": null}})),
        )
        .unwrap();
        assert_eq!(b.upsampler.preset.as_deref(), Some("other"));
        assert_eq!(b.upsampler.target, a.upsampler.target);
    }

    #[test]
    fn two_section_tilt_patches_round_trip() {
        use audivane_protocol::{EqProfile, TiltPivot};
        let low = TiltConfig {
            enabled: true,
            slope_db_per_oct: Some(2.0),
            total_db: None,
            start_hz: 20.0,
            end_hz: 200.0,
            pivot: TiltPivot::End,
        };
        let high = TiltConfig {
            slope_db_per_oct: Some(-1.0),
            start_hz: 1000.0,
            end_hz: 20_000.0,
            pivot: TiltPivot::Start,
            ..low
        };
        let mut a = chain_a();
        a.eq = Some(EqProfile {
            tilt: Some(low),
            tilt_high: Some(high),
            ..EqProfile::default()
        });
        // Treble section off in B: the bass section stays.
        let b = materialize(&a, &AbB::Patch(json!({"eq": {"tilt_high": null}}))).unwrap();
        let eq = b.eq.unwrap();
        assert_eq!((eq.tilt, eq.tilt_high), (Some(low), None));
        // Both off.
        let b = materialize(
            &a,
            &AbB::Patch(json!({"eq": {"tilt": null, "tilt_high": null}})),
        )
        .unwrap();
        let eq = b.eq.unwrap();
        assert_eq!((eq.tilt, eq.tilt_high), (None, None));
        // Disabled, settings kept.
        let patch = json!({"eq": {"tilt": {"enabled": false}, "tilt_high": {"enabled": false}}});
        let eq = materialize(&a, &AbB::Patch(patch)).unwrap().eq.unwrap();
        assert_eq!(
            eq.tilt,
            Some(TiltConfig {
                enabled: false,
                ..low
            })
        );
        assert_eq!(
            eq.tilt_high,
            Some(TiltConfig {
                enabled: false,
                ..high
            })
        );
        // A second section added in B only.
        let mut single = a.clone();
        single.eq.as_mut().unwrap().tilt_high = None;
        let patch = json!({"eq": {"tilt_high": serde_json::to_value(high).unwrap()}});
        let b = materialize(&single, &AbB::Patch(patch)).unwrap();
        assert_eq!(b.eq, a.eq);
        // Config validation sees B's pair.
        let patch = json!({"eq": {"tilt_high": {"start_hz": 100.0}}});
        let b = materialize(&a, &AbB::Patch(patch)).unwrap();
        assert!(
            crate::control::config::validate_chain("ab.b", &b)
                .unwrap_err()
                .contains("overlap")
        );
    }

    #[test]
    fn removing_from_an_absent_stage_leaves_it_absent() {
        let mut a = chain_a();
        a.eq = None;
        let b = materialize(&a, &AbB::Patch(json!({"eq": {"tilt": null}}))).unwrap();
        assert_eq!(b.eq, None);
        // Setting something still creates the stage.
        let b = materialize(&a, &AbB::Patch(json!({"eq": {"tilt": {"enabled": false}}}))).unwrap();
        assert_eq!(
            b.eq.unwrap().tilt,
            Some(TiltConfig {
                enabled: false,
                ..serde_json::from_value(json!({})).unwrap()
            })
        );
    }

    #[test]
    fn shared_fields_and_bad_values_are_named() {
        let a = chain_a();
        for (patch, field) in [
            (json!({"limiter": {"mode": "off"}}), "ab.b.patch.limiter"),
            (
                json!({"output": {"headroom_db": 0.0}}),
                "ab.b.patch.output.headroom_db",
            ),
            (
                json!({"output": {"format": "s16"}}),
                "ab.b.patch.output.format",
            ),
            (json!({"output": null}), "ab.b.patch.output"),
            (json!({"formats": ["s16"]}), "ab.b.patch.formats"),
            (json!({"device": "hw:0,0"}), "ab.b.patch.device"),
            (
                json!({"upsampler": {"target": {"ratio": 2}}}),
                "ab.b.patch.upsampler.target",
            ),
            (json!({"upsampler": null}), "ab.b.patch.upsampler"),
            (json!({"pitch": {"cents": "high"}}), "ab.b.patch.pitch"),
            (json!([1]), "ab.b.patch"),
        ] {
            let err = materialize(&a, &AbB::Patch(patch)).unwrap_err();
            assert!(err.starts_with(field), "{err}");
        }
    }

    #[test]
    fn a_chain_keeps_its_filter_and_follows_the_shared_fields() {
        let a = chain_a();
        let mut preset = ChainConfig::neutral();
        preset.upsampler.preset = Some("other".into());
        let b = materialize(&a, &AbB::Chain(Box::new(preset))).unwrap();
        assert_eq!(b.upsampler.preset.as_deref(), Some("other"));
        assert_eq!(b.upsampler.target, a.upsampler.target);
        assert_eq!(b.limiter, a.limiter);
        assert_eq!(b.output, a.output);
        assert_ne!(a.limiter.mode, LimiterMode::Off);
        // Its own bits and dither follow A too: a preset is never an
        // output compare (only a patch is).
        let mut preset = ChainConfig::neutral();
        preset.output.bits = 16;
        preset.output.dither = DitherKind::Shaped;
        preset.output.headroom_db = 0.0;
        let b = materialize(&a, &AbB::Chain(Box::new(preset))).unwrap();
        assert_eq!(b.output, a.output);
    }

    #[test]
    fn a_patch_may_set_bits_and_dither() {
        let a = chain_a();
        let b = materialize(
            &a,
            &AbB::Patch(json!({"output": {"bits": 16, "dither": "shaped"}})),
        )
        .unwrap();
        assert_eq!((b.output.bits, b.output.dither), (16, DitherKind::Shaped));
        assert_eq!(b.output.headroom_db, a.output.headroom_db);
        assert_eq!(b.eq, a.eq);
    }

    #[test]
    fn list_notes_only_for_a_kept_list_that_a_changed() {
        let ir = json!([{"ir_path": "/x.wav"}]);
        let b = AbB::Patch(json!({"convolution": ir, "eq": {"bands": []}}));
        assert_eq!(set_notes(&b).len(), 1);
        let old = chain_a();
        let mut new = old.clone();
        assert!(edit_notes(&b, &old, &new).is_empty());
        new.convolution = serde_json::from_value(json!([{"ir_path": "/y.wav"}])).unwrap();
        let notes = edit_notes(&b, &old, &new);
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("convolution"), "{notes:?}");
        assert!(set_notes(&AbB::Patch(json!({"convolution": []}))).is_empty());
    }
}
