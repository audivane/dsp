//! Where things live: the daemon's socket (same order as the daemon's
//! `socket_path` in crates/daemon/src/control/ipc.rs minus the `--socket`
//! flag) and the desktop app's own config directory.

use std::path::{Path, PathBuf};

pub fn socket_path() -> PathBuf {
    resolve(
        std::env::var_os("AUDIVANE_DSP_SOCKET").map(PathBuf::from),
        std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from),
        // SAFETY: getuid has no preconditions.
        unsafe { libc::getuid() },
    )
}

/// `$XDG_CONFIG_HOME/audivane-dsp/<what>`, else `$HOME/.config/audivane-dsp/<what>`.
pub fn config_dir_under(config_home: &Path, what: &str) -> PathBuf {
    config_home.join("audivane-dsp").join(what)
}

/// Where the app keeps overlay files (targets, bounds, measurements).
pub fn overlays_dir() -> Result<PathBuf, String> {
    if let Some(x) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return Ok(config_dir_under(Path::new(&x), "overlays"));
    }
    if let Some(h) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
        return Ok(config_dir_under(&Path::new(&h).join(".config"), "overlays"));
    }
    Err("neither XDG_CONFIG_HOME nor HOME is set, so there is nowhere to keep overlays".into())
}

fn resolve(explicit: Option<PathBuf>, runtime: Option<PathBuf>, uid: u32) -> PathBuf {
    if let Some(p) = explicit.filter(|p| !p.as_os_str().is_empty()) {
        return p;
    }
    if let Some(r) = runtime.filter(|p| !p.as_os_str().is_empty()) {
        return r.join("audivane-dsp.sock");
    }
    PathBuf::from(format!("/tmp/audivane-dsp-{uid}.sock"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlays_live_in_the_app_config() {
        assert_eq!(
            config_dir_under(Path::new("/home/you/.config"), "overlays"),
            PathBuf::from("/home/you/.config/audivane-dsp/overlays")
        );
    }

    #[test]
    fn order_matches_the_daemon() {
        assert_eq!(
            resolve(Some("/x/s.sock".into()), Some("/run/user/1".into()), 7),
            PathBuf::from("/x/s.sock")
        );
        assert_eq!(
            resolve(None, Some("/run/user/1".into()), 7),
            PathBuf::from("/run/user/1/audivane-dsp.sock")
        );
        assert_eq!(
            resolve(None, None, 7),
            PathBuf::from("/tmp/audivane-dsp-7.sock")
        );
        assert_eq!(
            resolve(Some("".into()), None, 7),
            PathBuf::from("/tmp/audivane-dsp-7.sock")
        );
    }
}
