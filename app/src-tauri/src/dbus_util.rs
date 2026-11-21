//! Small helpers for the D-Bus clients (the shortcuts portal, the tray
//! host watch, Plasma's OSD): bounded waits without an async timer, and
//! name-owner tracking.

use futures_util::StreamExt;
use futures_util::future::{Either, select};
use std::future::Future;
use std::time::Duration;
use zbus::{Connection, MatchRule, MessageStream};

/// The usual bound on one D-Bus round trip from the app.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(2);

/// Resolves after `d` (a short-lived thread sleeps; no runtime timer).
pub fn sleep(d: Duration) -> futures_channel::oneshot::Receiver<()> {
    let (tx, rx) = futures_channel::oneshot::channel();
    std::thread::spawn(move || {
        std::thread::sleep(d);
        let _ = tx.send(());
    });
    rx
}

/// `fut`'s output, or None when it takes longer than `d`.
pub async fn timeout<T>(d: Duration, fut: impl Future<Output = T>) -> Option<T> {
    match select(Box::pin(fut), sleep(d)).await {
        Either::Left((v, _)) => Some(v),
        Either::Right(_) => None,
    }
}

/// `NameOwnerChanged` for one well-known name.
pub async fn owner_changes(conn: &Connection, name: &str) -> zbus::Result<MessageStream> {
    let rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender("org.freedesktop.DBus")
        .interface("org.freedesktop.DBus")
        .member("NameOwnerChanged")
        .arg(0, name)
        .build()?;
    MessageStream::for_match_rule(rule, conn, None).await
}

/// The unique name owning `name` now, if any.
pub async fn name_owner(conn: &Connection, name: &str) -> Option<String> {
    let reply = timeout(
        CALL_TIMEOUT,
        conn.call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "GetNameOwner",
            &(name,),
        ),
    )
    .await?
    .ok()?;
    reply.body().deserialize::<String>().ok()
}

/// The new owner in a `NameOwnerChanged` message (empty: the name is gone).
pub fn new_owner(msg: &zbus::Message) -> Option<String> {
    msg.body()
        .deserialize::<(String, String, String)>()
        .ok()
        .map(|(_, _, new)| new)
}

/// Wait until `name` has an owner (now, or once one appears).
pub async fn wait_for_owner(conn: &Connection, name: &str) -> zbus::Result<()> {
    let mut changes = owner_changes(conn, name).await?;
    if name_owner(conn, name).await.is_some() {
        return Ok(());
    }
    while let Some(msg) = changes.next().await {
        if let Ok(msg) = msg
            && new_owner(&msg).is_some_and(|o| !o.is_empty())
        {
            return Ok(());
        }
    }
    Err(zbus::Error::Failure("the bus connection closed".into()))
}

/// The process name (`/proc/<pid>/comm`) of a bus connection's owner.
pub async fn owner_process(conn: &Connection, unique: &str) -> Option<String> {
    let reply = timeout(
        CALL_TIMEOUT,
        conn.call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "GetConnectionUnixProcessID",
            &(unique,),
        ),
    )
    .await?
    .ok()?;
    let pid = reply.body().deserialize::<u32>().ok()?;
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|s| s.trim().to_string())
}

/// A private `dbus-daemon` for tests (None when it is not installed), on an
/// abstract socket, so no files are made. Killed when dropped.
#[cfg(test)]
pub mod test_bus {
    use std::process::{Child, Command, Stdio};

    pub struct Bus {
        child: Child,
        pub address: String,
    }

    impl Drop for Bus {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    pub fn private_bus() -> Option<Bus> {
        let mut child = Command::new("dbus-daemon")
            .args([
                "--session",
                "--nofork",
                "--print-address=1",
                "--address=unix:tmpdir=/tmp",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let mut out = child.stdout.take()?;
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        while std::io::Read::read(&mut out, &mut byte).ok()? == 1 && byte[0] != b'\n' {
            line.push(byte[0]);
        }
        let address = String::from_utf8(line).ok()?.trim().to_string();
        (!address.is_empty()).then_some(Bus { child, address })
    }

    pub async fn connect(address: &str) -> zbus::Connection {
        zbus::conn::Builder::address(address).build().await.unwrap()
    }
}
