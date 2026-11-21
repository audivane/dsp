//! "Start engine" / "Restart engine": ask the systemd **user** manager to
//! start or restart `audivane-dsp.service` over D-Bus (session bus,
//! `org.freedesktop.systemd1.Manager.StartUnit` / `RestartUnit`).

use zbus::Connection;
use zbus::OwnedObjectPath;

pub const UNIT: &str = "audivane-dsp.service";

/// Start the unit; returns the systemd job path. Errors are plain sentences.
pub fn start_engine() -> Result<String, String> {
    // After a crash loop systemd refuses new starts for a while
    // ("start-limit-hit"); the user asked explicitly, so clear that first.
    let _ = reset_failed();
    unit_job("StartUnit")
}

fn reset_failed() -> Result<(), String> {
    zbus::block_on(async {
        let conn = Connection::session().await.map_err(|e| e.to_string())?;
        conn.call_method(
            Some("org.freedesktop.systemd1"),
            "/org/freedesktop/systemd1",
            Some("org.freedesktop.systemd1.Manager"),
            "ResetFailedUnit",
            &(UNIT,),
        )
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
    })
}

/// The engine's own last error line from the journal (it prints
/// `audivane-dsp: <reason>` before exiting), when it did not come up.
pub fn last_engine_error() -> Option<String> {
    let out = std::process::Command::new("journalctl")
        .args(["--user", "-u", UNIT, "-n", "40", "-o", "cat", "--no-pager"])
        .output()
        .ok()?;
    last_error_line(&String::from_utf8_lossy(&out.stdout))
}

fn last_error_line(journal: &str) -> Option<String> {
    journal
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix("audivane-dsp: "))
        .map(|reason| reason.trim().to_string())
        .filter(|reason| !reason.is_empty())
}

/// Whether the engine starts at login (the unit file is enabled).
pub fn engine_at_login() -> Result<bool, String> {
    zbus::block_on(async {
        let conn = Connection::session()
            .await
            .map_err(|e| format!("Cannot reach the session bus: {e}"))?;
        let reply = conn
            .call_method(
                Some("org.freedesktop.systemd1"),
                "/org/freedesktop/systemd1",
                Some("org.freedesktop.systemd1.Manager"),
                "GetUnitFileState",
                &(UNIT,),
            )
            .await
            .map_err(describe)?;
        let state: String = reply
            .body()
            .deserialize()
            .map_err(|e| format!("Unexpected reply from systemd: {e}"))?;
        Ok(state == "enabled")
    })
}

/// Enable or disable the unit for the user's logins (`systemctl --user
/// enable|disable audivane-dsp`); it does not start or stop it.
pub fn set_engine_at_login(on: bool) -> Result<(), String> {
    zbus::block_on(async {
        let conn = Connection::session()
            .await
            .map_err(|e| format!("Cannot reach the session bus: {e}"))?;
        let mgr = |method: &'static str| {
            let conn = conn.clone();
            async move {
                let files = vec![UNIT];
                let r = if method == "EnableUnitFiles" {
                    conn.call_method(
                        Some("org.freedesktop.systemd1"),
                        "/org/freedesktop/systemd1",
                        Some("org.freedesktop.systemd1.Manager"),
                        method,
                        &(files, false, true),
                    )
                    .await
                } else {
                    conn.call_method(
                        Some("org.freedesktop.systemd1"),
                        "/org/freedesktop/systemd1",
                        Some("org.freedesktop.systemd1.Manager"),
                        method,
                        &(files, false),
                    )
                    .await
                };
                r.map(|_| ()).map_err(describe)
            }
        };
        mgr(if on {
            "EnableUnitFiles"
        } else {
            "DisableUnitFiles"
        })
        .await?;
        conn.call_method(
            Some("org.freedesktop.systemd1"),
            "/org/freedesktop/systemd1",
            Some("org.freedesktop.systemd1.Manager"),
            "Reload",
            &(),
        )
        .await
        .map(|_| ())
        .map_err(describe)
    })
}

/// Restart the unit (start it when stopped), e.g. after a package upgrade
/// left the old engine running.
pub fn restart_engine() -> Result<String, String> {
    unit_job("RestartUnit")
}

fn unit_job(method: &'static str) -> Result<String, String> {
    zbus::block_on(async {
        let conn = Connection::session()
            .await
            .map_err(|e| format!("Cannot reach the session bus: {e}"))?;
        let reply = conn
            .call_method(
                Some("org.freedesktop.systemd1"),
                "/org/freedesktop/systemd1",
                Some("org.freedesktop.systemd1.Manager"),
                method,
                &(UNIT, "replace"),
            )
            .await
            .map_err(describe)?;
        let job: OwnedObjectPath = reply
            .body()
            .deserialize()
            .map_err(|e| format!("Unexpected reply from systemd: {e}"))?;
        Ok(job.to_string())
    })
}

fn describe(e: zbus::Error) -> String {
    let text = e.to_string();
    if text.contains("NoSuchUnit") || text.contains("not found") {
        format!(
            "The {UNIT} user service is not installed. Install the audivane-dsp package, then run `systemctl --user daemon-reload`."
        )
    } else if text.contains("ServiceUnknown")
        || text.contains("systemd1") && text.contains("not provided")
    {
        "The systemd user manager is not running in this session.".to_string()
    } else {
        format!("systemd could not start {UNIT}: {text}")
    }
}

#[cfg(test)]
mod tests {
    use super::last_error_line;

    #[test]
    fn picks_the_engines_own_last_error() {
        let journal = "\
[2026-11-16T16:37:31.174Z INFO  audivane_dsp] audivane-dsp 0.9.3 starting
audivane-dsp: cannot start the engine: first
[2026-11-16T16:37:33.932Z ERROR audivane_dsp] cannot start the engine: second
audivane-dsp: cannot start the engine: second
Started Audivane DSP engine.
";
        assert_eq!(
            last_error_line(journal).as_deref(),
            Some("cannot start the engine: second")
        );
        assert_eq!(last_error_line("nothing here\n"), None);
    }
}
