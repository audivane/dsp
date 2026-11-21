//! Exercise the UI's daemon client against a running daemon.
//!
//! ```sh
//! AUDIVANE_DSP_SOCKET=/path cargo run -p audivane-dsp-ui --example probe
//! AUDIVANE_DSP_SOCKET=/path cargo run -p audivane-dsp-ui --example probe -- \
//!     --exercise IR1.wav IR2.wav
//! ```
//!
//! Without arguments it prints a short summary (status, which phase-4a calls
//! exist, events in two seconds) and changes nothing it does not restore.
//!
//! `--exercise` runs every call the app makes, with the parameters the app
//! sends, and checks the replies and the `config`/`presets` events against
//! docs/control-plane.md: split EQ, the IR chain (two IRs, plus the legacy
//! single-object and null forms), volume, devices, the preset round trip
//! (save, overwrite refusal, `modified`, load, rename, import, delete), A/B,
//! routing (`apps` with no rules), upsampler, bypass, output and format via
//! `set_config`. It restores the original config at the end and exits
//! non-zero if a check failed. Point it at a test daemon, not your own.

use audivane_dsp_ui::client::{CallError, DaemonClient, SubMsg, Subscription};
use audivane_dsp_ui::paths::socket_path;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    println!("socket: {}", socket_path().display());
    match args.first().map(String::as_str) {
        Some("--exercise") if args.len() == 3 => {
            let failed = exercise(&args[1], &args[2]);
            if failed > 0 {
                println!("\n{failed} check(s) FAILED");
                std::process::exit(1);
            }
            println!("\nall checks passed");
        }
        None => summary(),
        _ => {
            eprintln!("usage: probe [--exercise IR1.wav IR2.wav]");
            std::process::exit(2);
        }
    }
}

fn summary() {
    let path = Arc::new(socket_path);
    let c = DaemonClient::new(path.clone());
    let st = c.call("status", None).expect("status");
    println!(
        "status: state={} device={} graph_rate={}",
        st["state"], st["device"], st["graph_rate"]
    );
    for m in ["list_presets", "list_devices"] {
        match c.call(m, None) {
            Ok(v) => println!("{m}: ok ({} items)", v.as_array().map_or(0, Vec::len)),
            Err(e) => println!("{m}: {} ({})", e.message, e.code),
        }
    }
    println!("status.volume: {}", st["volume"]);
    let counts: Arc<Mutex<BTreeMap<String, usize>>> = Arc::default();
    let k = counts.clone();
    let mut sub = Subscription::spawn(
        path,
        Arc::new(move |m| {
            let key = match &m {
                SubMsg::Event(v) => v["event"].as_str().unwrap_or("?").to_string(),
                other => format!("{other:?}"),
            };
            *k.lock().unwrap().entry(key).or_default() += 1;
        }),
    );
    std::thread::sleep(Duration::from_secs(2));
    sub.stop();
    println!("events in 2 s: {:?}", counts.lock().unwrap());
}

// ---------------------------------------------------------------- exercise

struct Checks {
    failed: usize,
}

impl Checks {
    fn check(&mut self, what: &str, ok: bool, detail: impl std::fmt::Display) {
        if ok {
            println!("  ok    {what}");
        } else {
            self.failed += 1;
            println!("  FAIL  {what}: {detail}");
        }
    }
}

/// Events seen by the subscription, in order.
#[derive(Default)]
struct Seen {
    events: Vec<Value>,
    up: bool,
}

impl Seen {
    fn count(&self, name: &str) -> usize {
        self.events.iter().filter(|e| e["event"] == name).count()
    }
    fn last(&self, name: &str) -> Option<Value> {
        self.events
            .iter()
            .rev()
            .find(|e| e["event"] == name)
            .map(|e| e["data"].clone())
    }
}

fn wait_for(seen: &Mutex<Seen>, what: impl Fn(&Seen) -> bool) -> bool {
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(3) {
        if what(&seen.lock().unwrap()) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

fn err_text(r: &Result<Value, CallError>) -> String {
    match r {
        Ok(v) => format!("ok {v}"),
        Err(e) => format!("error {} ({})", e.message, e.code),
    }
}

fn exercise(ir1: &str, ir2: &str) -> usize {
    let path = Arc::new(socket_path);
    let c = DaemonClient::new(path.clone());
    let mut k = Checks { failed: 0 };
    let call = |m: &str, p: Option<Value>| c.call(m, p);

    let seen: Arc<Mutex<Seen>> = Arc::default();
    let s2 = seen.clone();
    let mut sub = Subscription::spawn(
        path,
        Arc::new(move |m| {
            let mut s = s2.lock().unwrap();
            match m {
                SubMsg::Up => s.up = true,
                SubMsg::Event(v) if v["event"] != "meters" => s.events.push(v),
                _ => {}
            }
        }),
    );

    println!("\n# connect and snapshot");
    let original = call("get_config", None).expect("get_config");
    let st = call("status", None).expect("status");
    k.check(
        "subscription up with all topics",
        wait_for(&seen, |s| {
            s.up && s.count("config") > 0 && s.count("presets") > 0
        }),
        "no config/presets snapshot",
    );
    k.check(
        "config.volume present",
        original["volume"].is_object(),
        &original["volume"],
    );
    k.check(
        "config.chain.convolution is an array",
        original["chain"]["convolution"].is_array(),
        &original["chain"]["convolution"],
    );
    k.check(
        "status.volume present with hardware flag",
        st["volume"]["hardware"].is_boolean(),
        &st["volume"],
    );
    println!(
        "        status: state={} device={} format={} volume={}",
        st["state"], st["device"], st["format"], st["volume"]
    );
    for m in ["list_streams", "now_playing"] {
        let r = call(m, None);
        k.check(m, r.is_ok(), err_text(&r));
    }

    println!("\n# devices");
    let devs = call("list_devices", None);
    let active: Vec<Value> = devs
        .as_ref()
        .ok()
        .and_then(Value::as_array)
        .map(|a| a.iter().filter(|d| d["active"] == true).cloned().collect())
        .unwrap_or_default();
    k.check(
        "list_devices has exactly one active device",
        active.len() == 1,
        err_text(&devs),
    );
    if let Some(d) = active.first() {
        println!("        active: {d}");
        k.check(
            "active device has DeviceInfo fields",
            d["id"].as_str().is_some_and(|s| s.starts_with("hw:CARD="))
                && d["card"].is_string()
                && d["hdmi"].is_boolean()
                && d["loopback"].is_boolean(),
            d,
        );
    }

    println!("\n# volume");
    let v = call(
        "set_volume",
        Some(json!({"volume_db": -10.5, "balance_db": 2.0, "muted": false})),
    );
    // A hardware mixer reports the step it actually set (e.g. -10.4 for -10.5).
    let near = |x: &Value, want: f64| x.as_f64().is_some_and(|x| (x - want).abs() <= 0.5);
    k.check(
        "set_volume → VolumeStatus (hardware value within a step)",
        v.as_ref().is_ok_and(|v| {
            near(&v["volume_db"], -10.5)
                && v["balance_db"] == 2.0
                && v["muted"] == false
                && v["hardware"].is_boolean()
                && v["min_db"].is_number()
                && v["max_db"].is_number()
        }),
        err_text(&v),
    );
    if let Ok(v) = &v {
        println!("        {v}");
    }
    let st = call("status", None).unwrap_or_default();
    k.check(
        "status.volume follows",
        near(&st["volume"]["volume_db"], -10.5),
        &st["volume"],
    );
    let r = call(
        "set_volume",
        Some(json!({"volume_db": 3.0, "balance_db": 0.0, "muted": false})),
    );
    k.check(
        "set_volume above 0 dB refused (INVALID_PARAMS)",
        matches!(&r, Err(e) if e.code == -32602),
        err_text(&r),
    );
    let r = call(
        "set_volume",
        Some(json!({"volume_db": -10.5, "balance_db": 2.0, "muted": true})),
    );
    k.check(
        "mute",
        r.as_ref().is_ok_and(|v| v["muted"] == true),
        err_text(&r),
    );
    k.check(
        "config.volume persisted in config event",
        wait_for(&seen, |s| {
            s.last("config").is_some_and(|c| {
                c["volume"] == json!({"volume_db": -10.5, "balance_db": 2.0, "muted": true})
            })
        }),
        seen.lock()
            .unwrap()
            .last("config")
            .map(|c| c["volume"].clone())
            .unwrap_or_default(),
    );

    println!("\n# layered EQ");
    let band = |f: f64, g: f64| json!({"kind": "peaking", "freq_hz": f, "gain_db": g, "q": 1.0, "enabled": true});
    let eq = json!({
        "name": "probe layered",
        "version": 2,
        "preamp_db": -3.0,
        "linked_enabled": true,
        "bands": [band(1000.0, 3.0), band(3000.0, -2.0)],
        "right": {"enabled": true, "preamp_db": -4.0, "bands": [band(120.0, 4.0)]},
    });
    let r = call("set_eq", Some(eq.clone()));
    k.check("set_eq with linked + right", r.is_ok(), err_text(&r));
    let cfg = call("get_config", None).unwrap_or_default();
    k.check(
        "right channel echoed",
        cfg["chain"]["eq"]["right"] == eq["right"] && cfg["chain"]["eq"]["bands"] == eq["bands"],
        &cfg["chain"]["eq"],
    );
    let mut linked = eq.clone();
    linked.as_object_mut().unwrap().remove("right");
    let r = call("set_eq", Some(linked));
    let cfg = call("get_config", None).unwrap_or_default();
    k.check(
        "linking drops right (omitted, not null)",
        r.is_ok() && cfg["chain"]["eq"].get("right").is_none(),
        &cfg["chain"]["eq"],
    );
    let r = call("set_eq", Some(eq.clone()));
    k.check("layered again", r.is_ok(), err_text(&r));

    println!("\n# convolution chain");
    let conv = json!([
        {"name": "Room", "enabled": true, "ir_path": ir1, "gain_db": -1.0},
        {"name": null, "enabled": true, "ir_path": ir2, "gain_db": 0.0},
    ]);
    let r = call("set_convolution", Some(conv.clone()));
    k.check("set_convolution with two IRs", r.is_ok(), err_text(&r));
    let cfg = call("get_config", None).unwrap_or_default();
    let got = &cfg["chain"]["convolution"];
    k.check(
        "two IRs echoed, name kept",
        got.as_array().map(Vec::len) == Some(2)
            && got[0]["name"] == "Room"
            && got[1]["ir_path"] == ir2,
        got,
    );
    let r = call(
        "set_convolution",
        Some(
            json!([{"name": "Room", "enabled": true, "ir_path": ir1, "gain_db": -1.0},
                    {"enabled": false, "ir_path": ir2, "gain_db": 0.0}]),
        ),
    );
    let cfg = call("get_config", None).unwrap_or_default();
    k.check(
        "disable one IR",
        r.is_ok() && cfg["chain"]["convolution"][1]["enabled"] == false,
        err_text(&r),
    );
    let r = call(
        "set_convolution",
        Some(json!({"enabled": true, "ir_path": ir1, "gain_db": 0.0})),
    );
    let cfg = call("get_config", None).unwrap_or_default();
    k.check(
        "legacy single object → array of one",
        r.is_ok() && cfg["chain"]["convolution"].as_array().map(Vec::len) == Some(1),
        &cfg["chain"]["convolution"],
    );
    let r = call(
        "set_convolution",
        Some(json!([{"enabled": true, "ir_path": "/nonexistent/ir.wav", "gain_db": 0.0}])),
    );
    k.check(
        "missing IR file refused (INVALID_PARAMS)",
        matches!(&r, Err(e) if e.code == -32602),
        err_text(&r),
    );
    let r = call("set_convolution", Some(conv.clone()));
    k.check("restore two IRs", r.is_ok(), err_text(&r));

    println!("\n# stages");
    let ups = json!({"target": {"ratio": 2}, "taps": 8192, "phase": "linear", "kaiser_beta": 14.0, "cutoff": 1.0});
    let r = call("set_upsampler", Some(ups.clone()));
    let cfg = call("get_config", None).unwrap_or_default();
    k.check(
        "set_upsampler ratio 2",
        r.is_ok() && cfg["chain"]["upsampler"]["target"] == json!({"ratio": 2}),
        err_text(&r),
    );
    let r = call("set_dsp_enabled", Some(json!({"enabled": false})));
    let r2 = call("set_dsp_enabled", Some(json!({"enabled": true})));
    k.check("bypass on/off", r.is_ok() && r2.is_ok(), err_text(&r));
    let mut next = cfg.clone();
    next["chain"]["output"] = json!({"bits": 16, "dither": "shaped"});
    let formats = next["formats"].as_array().cloned().unwrap_or_default();
    let mut f2: Vec<Value> = vec![json!("s24")];
    f2.extend(formats.into_iter().filter(|f| f != "s24"));
    next["formats"] = Value::Array(f2);
    let r = call("set_config", Some(next));
    let cfg = call("get_config", None).unwrap_or_default();
    k.check(
        "set_config: output and preferred format",
        r.is_ok() && cfg["chain"]["output"]["bits"] == 16 && cfg["formats"][0] == "s24",
        err_text(&r),
    );
    let r = call(
        "set_routing_mode",
        Some(json!({"mode": "apps", "rules": []})),
    );
    k.check("set_routing_mode apps, no rules", r.is_ok(), err_text(&r));

    println!("\n# presets");
    // Leftovers of an interrupted run.
    for n in ["Probe A", "Probe B", "Probe APO"] {
        let _ = call("delete_preset", Some(json!({"name": n})));
    }
    let presets_before = seen.lock().unwrap().count("presets");
    let r = call(
        "save_preset",
        Some(json!({"name": "Probe A", "overwrite": false})),
    );
    k.check(
        "save_preset → PresetInfo (bands 3, convolution 2)",
        r.as_ref().is_ok_and(|v| {
            v["name"] == "Probe A"
                && v["bands"] == 3
                && v["convolution"] == 2
                && v["modified_unix"].is_u64()
        }),
        err_text(&r),
    );
    let r = call(
        "save_preset",
        Some(json!({"name": "Probe A", "overwrite": false})),
    );
    k.check(
        "save without overwrite refused (BAD_STATE)",
        matches!(&r, Err(e) if e.code == 1),
        err_text(&r),
    );
    let r = call(
        "save_preset",
        Some(json!({"name": "Probe A", "overwrite": true})),
    );
    k.check("save with overwrite", r.is_ok(), err_text(&r));
    let st = call("status", None).unwrap_or_default();
    k.check(
        "status.preset = Probe A, not modified",
        st["preset"] == json!({"name": "Probe A", "modified": false}),
        &st["preset"],
    );
    let cfg = call("get_config", None).unwrap_or_default();
    k.check(
        "config.active_preset",
        cfg["active_preset"] == "Probe A",
        &cfg["active_preset"],
    );
    k.check(
        "presets event after save",
        wait_for(&seen, |s| {
            s.count("presets") > presets_before
                && s.last("presets").is_some_and(|l| {
                    l.as_array()
                        .is_some_and(|a| a.iter().any(|p| p["name"] == "Probe A"))
                })
        }),
        "none",
    );
    let r = call("list_presets", None);
    k.check(
        "list_presets has Probe A",
        r.as_ref()
            .ok()
            .and_then(Value::as_array)
            .is_some_and(|a| a.iter().any(|p| p["name"] == "Probe A")),
        err_text(&r),
    );
    let r = call("get_preset", Some(json!({"name": "Probe A"})));
    k.check(
        "get_preset → Preset { version 1, chain }",
        r.as_ref()
            .is_ok_and(|p| p["version"] == 1 && p["chain"]["eq"]["right"] == eq["right"]),
        err_text(&r),
    );
    let mut edited = eq.clone();
    edited["bands"][0]["gain_db"] = json!(5.0);
    let _ = call("set_eq", Some(edited));
    let st = call("status", None).unwrap_or_default();
    k.check(
        "edit marks modified",
        st["preset"]["modified"] == true,
        &st["preset"],
    );
    k.check(
        "status event reports modified",
        wait_for(&seen, |s| {
            s.last("status")
                .is_some_and(|st| st["preset"]["modified"] == true)
        }),
        "no status event with modified",
    );
    let r = call("load_preset", Some(json!({"name": "Probe A"})));
    let st = call("status", None).unwrap_or_default();
    k.check(
        "load_preset (revert) clears modified",
        r.as_ref().is_ok_and(Value::is_null) && st["preset"]["modified"] == false,
        err_text(&r),
    );
    let r = call(
        "rename_preset",
        Some(json!({"from": "Probe A", "to": "Probe B"})),
    );
    k.check(
        "rename_preset → PresetInfo",
        r.as_ref().is_ok_and(|v| v["name"] == "Probe B"),
        err_text(&r),
    );
    let cfg = call("get_config", None).unwrap_or_default();
    let st = call("status", None).unwrap_or_default();
    k.check(
        "active preset follows rename",
        cfg["active_preset"] == "Probe B" && st["preset"]["name"] == "Probe B",
        format!("{} / {}", cfg["active_preset"], st["preset"]),
    );
    let apo = "Preamp: -5.2 dB\nFilter 1: ON LSC Fc 105 Hz Gain 5.5 dB Q 0.71\nFilter 2: ON PK Fc 2000 Hz Gain -2 dB Q 1.4\nFilter 3: ON HSC Fc 10000 Hz Gain -3 dB Q 0.71\n";
    let r = call(
        "import_apo",
        Some(json!({"name": "Probe APO", "text": apo})),
    );
    k.check(
        "import_apo → PresetInfo (3 bands, no IR)",
        r.as_ref()
            .is_ok_and(|v| v["name"] == "Probe APO" && v["bands"] == 3 && v["convolution"] == 0),
        err_text(&r),
    );
    let cfg = call("get_config", None).unwrap_or_default();
    k.check(
        "import does not load",
        cfg["active_preset"] == "Probe B",
        &cfg["active_preset"],
    );
    let p = call("get_preset", Some(json!({"name": "Probe APO"}))).unwrap_or_default();
    k.check(
        "imported preset: EQ only, no convolution",
        p["chain"]["convolution"] == json!([]) && p["chain"]["eq"]["preamp_db"] == -5.2,
        &p["chain"],
    );
    let ups_default =
        p["chain"]["upsampler"]["target"] == "off" && p["chain"]["output"]["bits"] == 24;
    println!(
        "        imported upsampler/output: {} / {} ({})",
        p["chain"]["upsampler"]["target"],
        p["chain"]["output"],
        if ups_default {
            "defaults"
        } else {
            "copied from chain A"
        }
    );
    let r = call(
        "import_apo",
        Some(json!({"name": "Probe APO", "text": apo})),
    );
    k.check(
        "import_apo replaces an existing name",
        r.is_ok(),
        err_text(&r),
    );
    let r = call(
        "import_apo",
        Some(json!({"name": "Probe bad", "text": "this is not an EQ"})),
    );
    println!("        import of junk text: {}", err_text(&r));
    if r.is_ok() {
        let _ = call("delete_preset", Some(json!({"name": "Probe bad"})));
    }
    let r = call("load_preset", Some(json!({"name": "Probe APO"})));
    let cfg = call("get_config", None).unwrap_or_default();
    k.check(
        "load imported preset",
        r.is_ok()
            && cfg["active_preset"] == "Probe APO"
            && cfg["chain"]["convolution"] == json!([]),
        err_text(&r),
    );
    k.check(
        "config event after load",
        wait_for(&seen, |s| {
            s.last("config")
                .is_some_and(|c| c["active_preset"] == "Probe APO")
        }),
        "none",
    );
    let r = call("get_preset", Some(json!({"name": "No such preset"})));
    k.check(
        "unknown preset → INVALID_PARAMS",
        matches!(&r, Err(e) if e.code == -32602),
        err_text(&r),
    );

    println!("\n# A/B");
    let r = call(
        "set_ab",
        Some(json!({
            "b": {"patch": {"eq": null, "convolution": []}},
            "label": "Flat",
            "level": {"mode": "analytic"}
        })),
    );
    k.check(
        "set_ab flat",
        r.as_ref().is_ok_and(|v| v["enabled"] == true),
        err_text(&r),
    );
    let pb = call("get_preset", Some(json!({"name": "Probe B"}))).unwrap_or_default();
    let mut b2 = pb["chain"].clone();
    b2["eq"]["name"] = json!("Probe B");
    let r = call(
        "set_ab",
        Some(json!({
            "b": {"chain": b2},
            "label": "Probe B",
            "level": {"mode": "manual", "offset_db": -1.5}
        })),
    );
    k.check(
        "set_ab from a preset (other upsampler → notes, not refused)",
        r.is_ok(),
        err_text(&r),
    );
    if let Ok(v) = &r {
        println!("        {v}");
    }
    let r = call(
        "ab_select",
        Some(json!({"side": "b", "crossfade_ms": 10.0})),
    );
    k.check(
        "ab_select b",
        r.as_ref().is_ok_and(|v| v["current"] == "b"),
        err_text(&r),
    );
    let r = call(
        "ab_select",
        Some(json!({"side": "a", "crossfade_ms": 10.0})),
    );
    k.check("ab_select a", r.is_ok(), err_text(&r));
    let r = call("set_ab", Some(Value::Null));
    k.check("set_ab null", r.is_ok(), err_text(&r));

    println!("\n# cleanup");
    let presets_before = seen.lock().unwrap().count("presets");
    let r = call("delete_preset", Some(json!({"name": "Probe APO"})));
    let cfg = call("get_config", None).unwrap_or_default();
    let st = call("status", None).unwrap_or_default();
    k.check(
        "deleting the active preset clears it",
        r.is_ok() && cfg["active_preset"].is_null() && st["preset"].is_null(),
        format!("{} / {}", cfg["active_preset"], st["preset"]),
    );
    let r = call("delete_preset", Some(json!({"name": "Probe B"})));
    k.check("delete Probe B", r.is_ok(), err_text(&r));
    k.check(
        "presets event after delete",
        wait_for(&seen, |s| {
            s.count("presets") > presets_before
                && s.last("presets").is_some_and(|l| {
                    l.as_array()
                        .is_some_and(|a| !a.iter().any(|p| p["name"] == "Probe B"))
                })
        }),
        "none",
    );
    let r = call("set_config", Some(original.clone()));
    let cfg = call("get_config", None).unwrap_or_default();
    k.check(
        "restore the original config",
        r.is_ok() && cfg == original,
        err_text(&r),
    );
    k.check(
        "last config event equals get_config",
        wait_for(&seen, |s| s.last("config").as_ref() == Some(&cfg)),
        "differs",
    );
    sub.stop();
    let s = seen.lock().unwrap();
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for e in &s.events {
        *counts
            .entry(e["event"].as_str().unwrap_or("?").to_string())
            .or_default() += 1;
    }
    println!("        events seen (meters excluded): {counts:?}");
    k.failed
}
