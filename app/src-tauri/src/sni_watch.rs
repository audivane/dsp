//! Is there a tray? The StatusNotifier watcher (`org.kde.StatusNotifierWatcher`)
//! must be on the session bus and report a registered host
//! (`IsStatusNotifierHostRegistered`): only then does an icon show
//! anywhere. The app follows both, so close-to-tray is on only while a tray
//! can bring the window back. The process owning the watcher also tells
//! which way the host's wheel deltas go (`wheel::host_sign`).
//!
//! Panels that only speak the old XEmbed tray protocol show nothing: the
//! app no longer links an appindicator library to fall back to.

use crate::dbus_util::{CALL_TIMEOUT, name_owner, owner_changes, owner_process, timeout};
use futures_util::StreamExt;
use zbus::{Connection, MatchRule, MessageStream, OwnedVariant};

pub const WATCHER: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TrayHost {
    /// A watcher with a registered host: an icon will show.
    pub available: bool,
    /// The watcher's process name, when known.
    pub watcher_process: Option<String>,
}

/// A tray shows when the watcher has an owner and does not say that no
/// host is registered (one that lacks the property is taken at its word).
pub fn available(owner: Option<&str>, host_registered: Option<bool>) -> bool {
    owner.is_some_and(|o| !o.is_empty()) && host_registered != Some(false)
}

async fn query(conn: &Connection) -> TrayHost {
    let Some(owner) = name_owner(conn, WATCHER).await else {
        return TrayHost::default();
    };
    let registered = timeout(
        CALL_TIMEOUT,
        conn.call_method(
            Some(WATCHER),
            WATCHER_PATH,
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &(WATCHER, "IsStatusNotifierHostRegistered"),
        ),
    )
    .await
    .and_then(|r| r.ok())
    .and_then(|r| r.body().deserialize::<OwnedVariant>().ok())
    .and_then(|v| bool::try_from(v).ok());
    TrayHost {
        available: available(Some(&owner), registered),
        watcher_process: owner_process(conn, &owner).await,
    }
}

/// Follow the tray host on its own thread (for the app's lifetime).
/// `on_change` hears the first state, then every change.
pub fn watch(on_change: impl Fn(TrayHost) + Send + 'static) {
    watch_on(None, on_change);
}

fn watch_on(address: Option<String>, on_change: impl Fn(TrayHost) + Send + 'static) {
    std::thread::Builder::new()
        .name("tray-host".into())
        .spawn(move || {
            zbus::block_on(async {
                let connect = async {
                    match &address {
                        Some(a) => zbus::conn::Builder::address(a.as_str()).build().await,
                        None => Connection::session().await,
                    }
                };
                let Some(Ok(conn)) = timeout(CALL_TIMEOUT, connect).await else {
                    on_change(TrayHost::default());
                    return;
                };
                let Ok(owners) = owner_changes(&conn, WATCHER).await else {
                    on_change(TrayHost::default());
                    return;
                };
                let hosts = MatchRule::builder()
                    .msg_type(zbus::message::Type::Signal)
                    .interface(WATCHER)
                    .build();
                let hosts = match hosts {
                    Ok(rule) => MessageStream::for_match_rule(rule, &conn, None).await.ok(),
                    Err(_) => None,
                };
                let mut events: futures_util::stream::BoxStream<'_, _> = match hosts {
                    Some(h) => Box::pin(futures_util::stream::select(owners, h)),
                    None => Box::pin(owners),
                };
                let mut last: Option<TrayHost> = None;
                loop {
                    let now = query(&conn).await;
                    if last.as_ref() != Some(&now) {
                        on_change(now.clone());
                        last = Some(now);
                    }
                    if events.next().await.is_none() {
                        break;
                    }
                }
                on_change(TrayHost::default());
            });
        })
        .expect("spawn the tray-host thread");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dbus_util::test_bus::private_bus;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{Receiver, channel};
    use std::time::Duration;

    #[test]
    fn availability_rule() {
        assert!(available(Some(":1.5"), Some(true)));
        assert!(available(Some(":1.5"), None));
        assert!(!available(Some(":1.5"), Some(false)));
        assert!(!available(None, Some(true)));
        assert!(!available(Some(""), None));
    }

    struct FakeWatcher {
        host: AtomicBool,
    }

    #[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
    impl FakeWatcher {
        #[zbus(property)]
        async fn is_status_notifier_host_registered(&self) -> bool {
            self.host.load(Ordering::Relaxed)
        }
    }

    async fn serve(address: &str, host: bool) -> Connection {
        zbus::conn::Builder::address(address)
            .serve_at(
                WATCHER_PATH,
                FakeWatcher {
                    host: AtomicBool::new(host),
                },
            )
            .name(WATCHER)
            .build()
            .await
            .unwrap()
    }

    fn next(rx: &Receiver<TrayHost>) -> TrayHost {
        rx.recv_timeout(Duration::from_secs(5))
            .expect("a change in time")
    }

    #[test]
    fn follows_the_watcher_and_its_host() {
        let Some(bus) = private_bus() else {
            eprintln!("dbus-daemon not available: skipped");
            return;
        };
        let (tx, rx) = channel();
        let tx = Mutex::new(tx);
        watch_on(Some(bus.address.clone()), move |h| {
            let _ = tx.lock().unwrap().send(h);
        });
        // No watcher yet: no tray.
        assert!(!next(&rx).available);
        // A watcher without a host: still none.
        let w = zbus::block_on(serve(&bus.address, false));
        let h = next(&rx);
        assert!(!h.available);
        assert!(h.watcher_process.is_some());
        // The host registers: a tray.
        zbus::block_on(async {
            let iface = w
                .object_server()
                .interface::<_, FakeWatcher>(WATCHER_PATH)
                .await
                .unwrap();
            iface.get().await.host.store(true, Ordering::Relaxed);
            w.emit_signal(
                None::<&str>,
                WATCHER_PATH,
                WATCHER,
                "StatusNotifierHostRegistered",
                &(),
            )
            .await
            .unwrap();
        });
        assert!(next(&rx).available);
        // The watcher goes away (the panel restarts): no tray...
        drop(w);
        assert!(!next(&rx).available);
        // ...and it comes back.
        let _w = zbus::block_on(serve(&bus.address, true));
        assert!(next(&rx).available);
    }
}
