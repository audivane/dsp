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

/// `$XDG_CONFIG_HOME`, else `$HOME/.config`; None when neither is set.
pub fn config_home() -> Option<PathBuf> {
    config_home_from(
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
    )
}

fn config_home_from(
    xdg: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    if let Some(x) = xdg.filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(x));
    }
    home.filter(|v| !v.is_empty())
        .map(|h| Path::new(&h).join(".config"))
}

/// Where the app keeps overlay files (targets, bounds, measurements).
pub fn overlays_dir() -> Result<PathBuf, String> {
    config_home()
        .map(|h| config_dir_under(&h, "overlays"))
        .ok_or_else(|| {
            "neither XDG_CONFIG_HOME nor HOME is set, so there is nowhere to keep overlays".into()
        })
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
