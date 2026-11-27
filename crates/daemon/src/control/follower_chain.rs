//! A follower's own chain (`outputs.followers[].chain_patch`): chain A with
//! a JSON merge patch, applied as A/B side B's patch is
//! ([`super::ab_side::apply_patch`]) and re-applied on every edit of chain
//! A, so the follower keeps following A wherever its patch does not reach.
//!
//! Without a patch a follower plays chain A as before. A/B stays on the
//! master: a follower's config never carries it.

use super::ab_side;
use audivane_protocol::ChainConfig;
use audivane_protocol::ipc::{EngineConfig, FollowerConfig};

/// Where follower `i`'s patch sits in the config, for error messages.
pub fn patch_path(i: usize) -> String {
    format!("outputs.followers[{i}].chain_patch")
}

/// Follower `i`'s chain: chain A of `cfg`, with its patch when it has one,
/// or why the patch does not apply.
///
/// The master's room-calibration item (tagged `calibration`) never reaches
/// a follower: it is left out of the chain A a follower inherits, before
/// its own patch (a calibrated follower's patch carries its own part).
pub fn chain(cfg: &EngineConfig, f: &FollowerConfig, i: usize) -> Result<ChainConfig, String> {
    let mut base = cfg.chain.clone();
    base.convolution.retain(|c| !c.calibration);
    match &f.chain_patch {
        None => Ok(base),
        Some(patch) => {
            let at = patch_path(i);
            if !patch.is_object() {
                return Err(format!(
                    "{at} must be a JSON object (a merge patch on chain A)"
                ));
            }
            ab_side::apply_patch(&base, patch, &at)
        }
    }
}

/// The config a follower's chain is built from: `cfg` with its own chain
/// and without A/B. A patch that no longer applies (validated on every edit,
/// so only a hand-edited file) plays chain A, logged.
pub fn config(cfg: &EngineConfig, f: &FollowerConfig) -> EngineConfig {
    let i = cfg
        .outputs
        .followers
        .iter()
        .position(|g| g.device == f.device)
        .unwrap_or(0);
    let mut out = cfg.clone();
    out.ab = None;
    out.output_channels = f.output_channels;
    match chain(cfg, f, i) {
        Ok(c) => out.chain = c,
        Err(e) => log::warn!("follower {}: its chain patch is not applied: {e}", f.device),
    }
    out
}

/// The top-level fields where follower `f`'s chain differs from chain A
/// (empty without a patch).
pub fn differing(cfg: &EngineConfig, f: &FollowerConfig) -> Vec<String> {
    if f.chain_patch.is_none() {
        return Vec::new();
    }
    let own = config(cfg, f);
    ab_side::differing_fields(&cfg.chain, &own.chain)
}

/// Normalise a patch a client sends: `{}` or `null` clears it.
pub fn normalise(patch: Option<serde_json::Value>) -> Option<serde_json::Value> {
    match patch {
        Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::Object(m)) if m.is_empty() => None,
        p => p,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg_with(patch: Option<serde_json::Value>) -> EngineConfig {
        let mut cfg: EngineConfig = serde_json::from_str(r#"{"device":"hw:0,0"}"#).unwrap();
        let mut f = FollowerConfig::new("hw:CARD=USB,DEV=0");
        f.chain_patch = patch;
        cfg.outputs.followers = vec![f];
        cfg
    }

    #[test]
    fn without_a_patch_a_follower_plays_chain_a() {
        let cfg = cfg_with(None);
        let f = &cfg.outputs.followers[0];
        let own = config(&cfg, f);
        assert_eq!(own.chain, cfg.chain);
        assert!(own.ab.is_none());
        assert!(differing(&cfg, f).is_empty());
    }

    #[test]
    fn a_patch_overrides_only_what_it_names_and_follows_a_elsewhere() {
        let mut cfg = cfg_with(Some(
            json!({"convolution": [], "limiter": {"mode": "limit"}}),
        ));
        cfg.chain.output.headroom_db = 3.0;
        let f = cfg.outputs.followers[0].clone();
        let own = config(&cfg, &f);
        assert_eq!(own.chain.output.headroom_db, 3.0, "A's edit reaches it");
        assert_eq!(differing(&cfg, &f), vec!["limiter".to_string()]);
        // An edit of A that the patch overrides does not.
        cfg.chain.limiter.mode = audivane_protocol::LimiterMode::AutoHeadroom;
        assert_eq!(
            config(&cfg, &f).chain.limiter.mode,
            audivane_protocol::LimiterMode::Limit
        );
    }

    #[test]
    fn a_bad_patch_names_its_path() {
        let cfg = cfg_with(Some(json!({"output": {"bits": "x"}})));
        let e = chain(&cfg, &cfg.outputs.followers[0], 0).unwrap_err();
        assert!(
            e.starts_with("outputs.followers[0].chain_patch.output"),
            "{e}"
        );
        let cfg = cfg_with(Some(json!([1])));
        assert!(chain(&cfg, &cfg.outputs.followers[0], 0).is_err());
    }

    #[test]
    fn an_empty_patch_clears() {
        assert_eq!(normalise(Some(json!({}))), None);
        assert_eq!(normalise(Some(serde_json::Value::Null)), None);
        assert_eq!(
            normalise(Some(json!({"eq": null}))),
            Some(json!({"eq": null}))
        );
    }
}
