//! "Start in the tray at login": an XDG autostart entry in the user's own
//! config (`$XDG_CONFIG_HOME/autostart/audivane-dsp-ui.desktop`) that runs
//! the app with `--hidden`. Nothing outside the user's home is touched. The
//! engine has its own systemd user unit and starts without this.

use std::path::{Path, PathBuf};

const FILE: &str = "audivane-dsp-ui.desktop";

/// `<config home>/autostart/audivane-dsp-ui.desktop`.
pub fn entry_path() -> Result<PathBuf, String> {
    crate::paths::config_home()
        .map(|h| h.join("autostart").join(FILE))
        .ok_or_else(|| {
            "neither XDG_CONFIG_HOME nor HOME is set, so there is no autostart folder".into()
        })
}

/// The program the entry starts: the AppImage file itself when the app
/// runs from one (`$APPIMAGE`; the binary inside it lives in a temporary
/// mount that is gone after a logout), else the running binary.
pub fn launcher(appimage: Option<std::ffi::OsString>, binary: PathBuf) -> PathBuf {
    appimage
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .unwrap_or(binary)
}

/// The entry exists and is not switched off (`Hidden=true` or
/// `X-GNOME-Autostart-enabled=false`, which desktop settings panels write).
pub fn is_enabled(path: &Path) -> bool {
    match std::fs::read_to_string(path) {
        Ok(text) => !text.lines().map(str::trim).any(|l| {
            l.eq_ignore_ascii_case("Hidden=true")
                || l.eq_ignore_ascii_case("X-GNOME-Autostart-enabled=false")
        }),
        Err(_) => false,
    }
}

/// Write the entry (for `exe`) or remove it.
pub fn set_enabled(path: &Path, enabled: bool, exe: &Path) -> Result<(), String> {
    if !enabled {
        return match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("cannot remove {}: {e}", path.display())),
        };
    }
    let dir = path.parent().ok_or("the autostart path has no folder")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let tmp = tempfile::NamedTempFile::new_in(dir)
        .map_err(|e| format!("cannot write in {}: {e}", dir.display()))?;
    std::fs::write(tmp.path(), entry_text(exe)).map_err(|e| e.to_string())?;
    tmp.persist(path)
        .map_err(|e| format!("cannot save {}: {e}", path.display()))?;
    Ok(())
}

/// The desktop entry. `NoDisplay` keeps it out of menus.
pub fn entry_text(exe: &Path) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Audivane DSP\n\
         Comment=Start the Audivane DSP app in the tray\n\
         Exec={} --hidden\n\
         Icon=audivane-dsp\n\
         Terminal=false\n\
         NoDisplay=true\n\
         X-GNOME-Autostart-enabled=true\n\
         X-KDE-autostart-phase=2\n",
        exec_arg(&exe.to_string_lossy())
    )
}

/// One `Exec` argument, quoted as the desktop entry spec asks when it has a
/// reserved character (then `"`, `` ` ``, `$` and `\` are escaped inside
/// the quotes, and every `\` is doubled again for the string value).
fn exec_arg(arg: &str) -> String {
    const RESERVED: &[char] = &[
        ' ', '\t', '\n', '"', '\'', '\\', '>', '<', '~', '|', '&', ';', '$', '*', '?', '#', '(',
        ')', '`', '%',
    ];
    let pct = arg.replace('%', "%%");
    if !arg.contains(RESERVED) {
        return pct;
    }
    let mut quoted = String::from("\"");
    for c in pct.chars() {
        if matches!(c, '"' | '`' | '$' | '\\') {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    quoted.push('"');
    quoted.replace('\\', "\\\\")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_characters_are_quoted_and_escaped() {
        assert_eq!(exec_arg("/opt/My Apps/ui"), "\"/opt/My Apps/ui\"");
        // `$` gets a backslash inside the quotes, which the string escape doubles.
        assert_eq!(exec_arg("/a$b"), "\"/a\\\\$b\"");
        assert_eq!(exec_arg("/a%b"), "\"/a%%b\"");
    }
}
