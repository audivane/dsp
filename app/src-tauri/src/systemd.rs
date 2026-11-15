//! "Start engine": ask the systemd **user** manager to start
//! `audivane-dsp.service` over D-Bus (session bus,
//! `org.freedesktop.systemd1.Manager.StartUnit`).

use zbus::Connection;
use zbus::OwnedObjectPath;

pub const UNIT: &str = "audivane-dsp.service";

/// Start the unit; returns the systemd job path. Errors are plain sentences.
pub fn start_engine() -> Result<String, String> {
    zbus::block_on(async {
        let conn = Connection::session()
            .await
            .map_err(|e| format!("Cannot reach the session bus: {e}"))?;
        let reply = conn
            .call_method(
                Some("org.freedesktop.systemd1"),
                "/org/freedesktop/systemd1",
                Some("org.freedesktop.systemd1.Manager"),
                "StartUnit",
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
