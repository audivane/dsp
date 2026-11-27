//! The app running from the AppImage with no user install (its `AppRun`
//! opens an installed copy whenever there is one): the "engine is not
//! running" page offers the per-user setup (`packaging/appimage/setup.sh`
//! inside the AppImage), and once it is done the app relaunches into the
//! installed copy under `~/.local/lib/audivane-dsp/`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The per-user install's folder, under the home directory.
const LIB: &str = ".local/lib/audivane-dsp";
/// A system package's engine: with it there, the setup refuses and the app
/// uses that engine.
const SYSTEM_ENGINE: &str = "/usr/bin/audivane-dsp";

/// What the setup offer needs to know. Only when running from an AppImage.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupState {
    /// A system package is installed: no setup, the app uses its engine.
    pub system_package: bool,
    /// The user install's version, if there is one.
    pub installed: Option<String>,
}

/// The AppImage's mount (`$APPDIR`), when the app runs from an AppImage
/// (`$APPIMAGE` names the file).
pub fn appdir(appimage: Option<OsString>, appdir: Option<OsString>) -> Option<PathBuf> {
    appimage.filter(|a| !a.is_empty())?;
    appdir.filter(|d| !d.is_empty()).map(PathBuf::from)
}

pub fn state(home: &Path, system_engine: &Path) -> SetupState {
    let installed = std::fs::read_to_string(home.join(LIB).join("VERSION"))
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    SetupState {
        system_package: system_engine.exists(),
        installed,
    }
}

pub fn current_state(home: &Path) -> SetupState {
    state(home, Path::new(SYSTEM_ENGINE))
}

/// The installed app, which the AppImage's app relaunches into.
pub fn installed_app(home: &Path) -> PathBuf {
    home.join(LIB).join("audivane-dsp-ui")
}

/// The variables that make a process think it runs from an AppImage; the
/// relaunch into the installed app drops them.
pub const APPIMAGE_ENV: [&str; 4] = ["APPIMAGE", "APPDIR", "ARGV0", "OWD"];

/// Run the setup (`setup.sh install`) from the AppImage's mount. Blocks
/// until it is done (it starts the engine). Errors are its own words.
pub fn install(appdir: &Path, login_start: bool) -> Result<String, String> {
    let script = appdir.join("usr/lib/audivane-dsp/setup.sh");
    let mut cmd = std::process::Command::new(&script);
    cmd.arg("install");
    if !login_start {
        cmd.arg("--no-login-start");
    }
    let out = cmd
        .env("APPDIR", appdir)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("cannot run {}: {e}", script.display()))?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr);
    for line in stdout.lines().chain(stderr.lines()) {
        log::info!("setup: {line}");
    }
    if out.status.success() {
        Ok(stdout)
    } else {
        Err(setup_error(&stderr).unwrap_or_else(|| format!("The setup failed ({}).", out.status)))
    }
}

/// The setup's own `error: …` line, capitalised as a sentence.
fn setup_error(stderr: &str) -> Option<String> {
    let line = stderr
        .lines()
        .rev()
        .find_map(|l| l.strip_prefix("error: "))?
        .trim();
    let mut c = line.chars();
    let first = c.next()?;
    Some(first.to_uppercase().chain(c).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_appimage_has_an_appdir() {
        assert_eq!(appdir(None, Some("/tmp/.mount_x".into())), None);
        assert_eq!(appdir(Some("".into()), Some("/tmp/.mount_x".into())), None);
        assert_eq!(appdir(Some("/a/A.AppImage".into()), None), None);
        assert_eq!(
            appdir(Some("/a/A.AppImage".into()), Some("/tmp/.mount_x".into())),
            Some(PathBuf::from("/tmp/.mount_x"))
        );
    }

    #[test]
    fn state_reads_the_install_and_the_package() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let pkg = home.join("usr-bin-audivane-dsp");
        assert_eq!(
            state(home, &pkg),
            SetupState {
                system_package: false,
                installed: None
            }
        );
        std::fs::create_dir_all(home.join(LIB)).unwrap();
        std::fs::write(home.join(LIB).join("VERSION"), "0.11.0\n").unwrap();
        std::fs::write(&pkg, "").unwrap();
        assert_eq!(
            state(home, &pkg),
            SetupState {
                system_package: true,
                installed: Some("0.11.0".into())
            }
        );
        assert_eq!(
            installed_app(home),
            home.join(".local/lib/audivane-dsp/audivane-dsp-ui")
        );
    }

    #[test]
    fn the_setup_error_is_its_last_error_line() {
        let err = "warning: x\nerror: a system package of Audivane DSP is installed (/usr/bin/audivane-dsp).\n";
        assert_eq!(
            setup_error(err).as_deref(),
            Some("A system package of Audivane DSP is installed (/usr/bin/audivane-dsp).")
        );
        assert_eq!(setup_error("nothing\n"), None);
    }
}
