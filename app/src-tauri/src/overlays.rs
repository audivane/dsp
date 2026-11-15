//! Overlay files: target curves, preference bounds and measurements.
//!
//! The webview parses and draws the curves; this side only stores them, as one
//! JSON file per overlay in `$XDG_CONFIG_HOME/audivane-dsp/overlays/`
//! (`~/.config/...` when `XDG_CONFIG_HOME` is unset). A name becomes the file
//! name, validated (no control characters, no leading dot) and with `%`, `/`
//! and `\\` escaped, so no name can reach outside that directory.

use crate::client::CallError;
use crate::paths;
use serde_json::Value;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Serialize all overlay disk operations so concurrent `spawn_blocking`
/// saves/deletes/lists cannot race or resurrect a deleted overlay.
static IO_LOCK: Mutex<()> = Mutex::new(());

/// The overlay kinds the UI knows. Anything else is not an overlay file.
pub const KINDS: [&str; 3] = ["target", "bounds", "measurement"];

const MAX_NAME_CHARS: usize = 80;
/// A 1/24-octave curve is a few KB; 1 MiB is generous and still bounded.
const MAX_BYTES: usize = 1 << 20;

fn invalid(message: impl Into<String>) -> CallError {
    CallError {
        code: CallError::INVALID_REQUEST,
        message: message.into(),
    }
}

fn io_error(action: &str, path: &Path, e: &io::Error) -> CallError {
    CallError {
        code: 2,
        message: format!("could not {action} {}: {e}", path.display()),
    }
}

/// Where overlays live on this machine (see `paths::overlays_dir`).
pub fn dir() -> Result<PathBuf, CallError> {
    paths::overlays_dir().map_err(invalid)
}

/// A name that is safe to use as a file name, and the name it is.
pub fn validate_name(name: &str) -> Result<&str, CallError> {
    let n = name.trim();
    if n.is_empty() {
        return Err(invalid("an overlay needs a name"));
    }
    if n != name {
        return Err(invalid("an overlay name cannot start or end with a space"));
    }
    if n.chars().count() > MAX_NAME_CHARS {
        return Err(invalid(format!(
            "an overlay name is at most {MAX_NAME_CHARS} characters"
        )));
    }
    if n.starts_with('.') {
        return Err(invalid("an overlay name cannot start with a dot"));
    }
    if n.chars().any(char::is_control) {
        return Err(invalid("an overlay name cannot contain control characters"));
    }
    Ok(n)
}

/// A name's file stem: `%`, `/` and `\` are escaped so any display name maps
/// to exactly one file in the folder and never to a path outside it.
pub fn file_stem(name: &str) -> Result<String, CallError> {
    let n = validate_name(name)?;
    let mut out = String::with_capacity(n.len());
    for c in n.chars() {
        match c {
            '%' => out.push_str("%25"),
            '/' => out.push_str("%2F"),
            '\\' => out.push_str("%5C"),
            _ => out.push(c),
        }
    }
    Ok(out)
}

/// The file an overlay is stored in.
pub fn file_path(dir: &Path, name: &str) -> Result<PathBuf, CallError> {
    Ok(dir.join(format!("{}.json", file_stem(name)?)))
}

/// Check the shape the webview stores: an object of a known kind, named.
pub fn validate_data(data: &Value) -> Result<(), CallError> {
    let obj = data
        .as_object()
        .ok_or_else(|| invalid("an overlay file must be a JSON object"))?;
    let name = obj.get("name").and_then(Value::as_str).unwrap_or("");
    if name.trim().is_empty() {
        return Err(invalid("an overlay file needs a name"));
    }
    let kind = obj.get("kind").and_then(Value::as_str).unwrap_or("");
    if !KINDS.contains(&kind) {
        return Err(invalid(format!(
            "an overlay kind is one of {}, not \"{kind}\"",
            KINDS.join(", ")
        )));
    }
    Ok(())
}

/// Every readable overlay in `dir`, sorted by name. Broken files are skipped:
/// one hand-edited file must not hide the rest.
pub fn list(dir: &Path) -> Result<Vec<Value>, CallError> {
    let _guard = IO_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io_error("read the overlay folder", dir, &e)),
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            log::warn!("overlay {}: unreadable", path.display());
            continue;
        };
        match serde_json::from_str::<Value>(&text) {
            Ok(v) if validate_data(&v).is_ok() => out.push(v),
            _ => log::warn!("overlay {}: not an overlay file", path.display()),
        }
    }
    out.sort_by(|a, b| {
        let key = |v: &Value| {
            v.get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_lowercase()
        };
        key(a).cmp(&key(b))
    });
    Ok(out)
}

/// Write one overlay, replacing any file of the same name.
pub fn save(dir: &Path, name: &str, data: &Value) -> Result<(), CallError> {
    let stem = file_stem(name)?;
    let path = dir.join(format!("{stem}.json"));
    validate_data(data)?;
    if data.get("name").and_then(Value::as_str) != Some(name) {
        return Err(invalid("the overlay's name and its file name must match"));
    }
    let text = serde_json::to_string_pretty(data)
        .map_err(|e| invalid(format!("could not write the overlay: {e}")))?;
    if text.len() > MAX_BYTES {
        return Err(invalid(
            "the overlay is too large; a curve should be only a few thousand points",
        ));
    }
    let _guard = IO_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    fs::create_dir_all(dir).map_err(|e| io_error("create the overlay folder", dir, &e))?;
    // Write to a unique temp file beside the target, fsync, and atomically rename.
    // The temporary name is hidden and not a `.json`.
    let mut tmp = tempfile::Builder::new()
        .prefix(&format!(".{stem}."))
        .suffix(".tmp")
        .tempfile_in(dir)
        .map_err(|e| io_error("create a temporary overlay file", dir, &e))?;
    tmp.write_all(text.as_bytes())
        .and_then(|()| tmp.as_file().sync_all())
        .map_err(|e| io_error("write the overlay", tmp.path(), &e))?;
    tmp.persist(&path)
        .map_err(|e| io_error("store the overlay", &path, &e.error))?;
    Ok(())
}

/// Delete one overlay. A name with no file is not an error: the intent is "gone".
pub fn remove(dir: &Path, name: &str) -> Result<(), CallError> {
    let path = file_path(dir, name)?;
    let _guard = IO_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_error("delete the overlay", &path, &e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample(name: &str) -> Value {
        json!({
            "version": 1,
            "name": name,
            "kind": "target",
            "points": [[20.0, 3.9], [1000.0, 0.0]]
        })
    }

    #[test]
    fn names_cannot_leave_the_overlay_folder() {
        assert_eq!(
            validate_name("Harman over-ear 2018").unwrap(),
            "Harman over-ear 2018"
        );
        assert_eq!(
            validate_name("Tilt −0.7 dB/oct").unwrap(),
            "Tilt −0.7 dB/oct"
        );
        for bad in [
            "",
            "  ",
            " leading",
            "trailing ",
            ".",
            "..",
            ".hidden",
            "line\nbreak",
        ] {
            assert!(validate_name(bad).is_err(), "{bad:?} should be rejected");
        }
        assert!(validate_name(&"x".repeat(MAX_NAME_CHARS + 1)).is_err());
        assert!(validate_name(&"x".repeat(MAX_NAME_CHARS)).is_ok());

        // Names that would be paths are escaped, not followed.
        for odd in ["a/b", "a\\b", "%/\\"] {
            let p = file_path(Path::new("/cfg/overlays"), odd).unwrap();
            assert_eq!(p.parent(), Some(Path::new("/cfg/overlays")), "{odd:?}");
            assert!(p.starts_with("/cfg/overlays"), "{odd:?}");
        }
        assert!(file_path(Path::new("/cfg"), "../passwd").is_err());
        assert_eq!(file_stem("a/b").unwrap(), "a%2Fb");
        assert_eq!(file_stem("100%").unwrap(), "100%25");
        let dir = Path::new("/home/you/.config/audivane-dsp/overlays");
        assert_eq!(
            file_path(dir, "Kali IN-5").unwrap(),
            dir.join("Kali IN-5.json")
        );
        assert_eq!(
            file_path(dir, "Tilt −0.7 dB/oct").unwrap(),
            dir.join("Tilt −0.7 dB%2Foct.json")
        );
    }

    #[test]
    fn saves_lists_and_deletes() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = paths::config_dir_under(tmp.path(), "overlays");
        assert_eq!(list(&dir).unwrap(), Vec::<Value>::new());

        save(&dir, "Flat", &sample("Flat")).unwrap();
        save(&dir, "Kali IN-5", &sample("Kali IN-5")).unwrap();
        // Saving the same name again replaces the file.
        let mut updated = sample("Kali IN-5");
        updated["kind"] = json!("measurement");
        save(&dir, "Kali IN-5", &updated).unwrap();

        let all = list(&dir).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0]["name"], "Flat");
        assert_eq!(all[1]["kind"], "measurement");

        remove(&dir, "Flat").unwrap();
        assert_eq!(list(&dir).unwrap().len(), 1);
        // Deleting again is fine.
        remove(&dir, "Flat").unwrap();
    }

    #[test]
    fn refuses_broken_or_mismatched_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = paths::config_dir_under(tmp.path(), "overlays");

        assert!(save(&dir, "x", &json!("not an object")).is_err());
        assert!(save(&dir, "x", &json!({"name": "x", "kind": "curve"})).is_err());
        assert!(save(&dir, "x", &json!({"kind": "target"})).is_err());
        // The name inside must be the one asked for.
        assert!(save(&dir, "x", &sample("y")).is_err());
        // Nothing was created by the refused saves.
        assert_eq!(list(&dir).unwrap().len(), 0);

        let mut big = sample("big");
        big["points"] = json!(vec![[20.0, 0.0]; MAX_BYTES / 8]);
        assert!(save(&dir, "big", &big).is_err());
    }

    #[test]
    fn a_broken_file_on_disk_does_not_hide_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = paths::config_dir_under(tmp.path(), "overlays");
        save(&dir, "Good", &sample("Good")).unwrap();
        fs::write(dir.join("bad.json"), "{not json").unwrap();
        fs::write(dir.join("notes.txt"), "hello").unwrap();
        fs::write(dir.join("wrong.json"), r#"{"name":"w","kind":"carpet"}"#).unwrap();
        let names: Vec<String> = list(&dir)
            .unwrap()
            .iter()
            .map(|v| v["name"].as_str().unwrap_or("").to_string())
            .collect();
        assert_eq!(names, ["Good"]);
    }

    #[test]
    fn concurrent_saves_do_not_fail_or_corrupt() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = paths::config_dir_under(tmp.path(), "overlays");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let mut handles = Vec::new();
        for t in 0..8 {
            let dir = dir.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                for i in 0..100 {
                    let mut d = sample("Shared");
                    d["view"] = json!({"offset_db": (t * 100 + i) as f64 / 10.0});
                    save(&dir, "Shared", &d).unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let all = list(&dir).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0]["name"], "Shared");
        let entries: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, ["Shared.json"]);
    }
}
