//! `org.freedesktop.ReserveDevice1` D-Bus protocol for exclusive ALSA card ownership.

use crate::error::{AlsaError, Result};
use std::ffi::CString;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use zbus::fdo::{DBusProxy, RequestNameFlags, RequestNameReply};
use zbus::names::WellKnownName;
use zbus::{Connection, Proxy};

/// Default high priority so Audivane takes precedence over WirePlumber (-20).
pub const DEFAULT_RESERVE_PRIORITY: i32 = 1000;

struct ReserveDeviceInterface {
    priority: i32,
    app_name: String,
    app_device_name: String,
    release_requested: Arc<AtomicBool>,
    on_release: Option<Arc<dyn Fn() + Send + Sync>>,
}

#[zbus::interface(name = "org.freedesktop.ReserveDevice1")]
impl ReserveDeviceInterface {
    fn request_release(&self, priority: i32) -> bool {
        if priority > self.priority {
            log::info!(
                "ReserveDevice1: yielding {} to higher priority {}",
                self.app_device_name,
                priority
            );
            self.release_requested.store(true, Ordering::Release);
            if let Some(cb) = &self.on_release {
                cb();
            }
            true
        } else {
            log::debug!(
                "ReserveDevice1: refusing release of {} to lower/equal priority {} (ours {})",
                self.app_device_name,
                priority,
                self.priority
            );
            false
        }
    }

    #[zbus(property)]
    fn priority(&self) -> i32 {
        self.priority
    }

    #[zbus(property)]
    fn application_name(&self) -> &str {
        &self.app_name
    }

    #[zbus(property)]
    fn application_device_name(&self) -> &str {
        &self.app_device_name
    }
}

/// Active `org.freedesktop.ReserveDevice1.Audio<card>` reservation on the
/// session D-Bus. Releases the bus name automatically when dropped.
pub struct DeviceReservation {
    conn: Connection,
    bus_name: String,
    obj_path: String,
    card: u32,
    priority: i32,
    release_requested: Arc<AtomicBool>,
}

impl DeviceReservation {
    /// Acquire `org.freedesktop.ReserveDevice1.Audio<card>` for an ALSA device
    /// string (e.g. `"hw:Loopback,0,0"` or `"hw:10,0,0"`).
    ///
    /// If another owner (e.g. WirePlumber) holds the device, sends
    /// `RequestRelease(priority)` and waits for it to drop the reservation.
    pub fn acquire_for_device(device: &str, priority: i32) -> Result<Self> {
        let card = card_index_from_device(device)?;
        Self::acquire_with_wakeup(card, device.to_string(), priority, None)
    }

    /// Acquire `org.freedesktop.ReserveDevice1.Audio<card>` for an ALSA device,
    /// invoking `on_release` immediately when a higher-priority requester
    /// calls `RequestRelease` and is granted release.
    pub fn acquire_for_device_with_wakeup(
        device: &str,
        priority: i32,
        on_release: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Self> {
        let card = card_index_from_device(device)?;
        Self::acquire_with_wakeup(card, device.to_string(), priority, Some(on_release))
    }

    /// Acquire `org.freedesktop.ReserveDevice1.Audio<card>` for a card index.
    pub fn acquire(card: u32, device_name: String, priority: i32) -> Result<Self> {
        Self::acquire_with_wakeup(card, device_name, priority, None)
    }

    /// Acquire `org.freedesktop.ReserveDevice1.Audio<card>` for a card index
    /// with an optional release-request wakeup hook.
    pub fn acquire_with_wakeup(
        card: u32,
        device_name: String,
        priority: i32,
        on_release: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Result<Self> {
        zbus::block_on(async {
            let conn = Connection::session().await?;
            let bus_name = format!("org.freedesktop.ReserveDevice1.Audio{card}");
            let obj_path = format!("/org/freedesktop/ReserveDevice1/Audio{card}");
            let release_requested = Arc::new(AtomicBool::new(false));

            let iface = ReserveDeviceInterface {
                priority,
                app_name: "Audivane DSP".to_string(),
                app_device_name: device_name,
                release_requested: Arc::clone(&release_requested),
                on_release,
            };
            // Remove any stale object from a previous reservation on the shared connection.
            let _ = conn
                .object_server()
                .remove::<ReserveDeviceInterface, _>(obj_path.as_str())
                .await;
            conn.object_server().at(obj_path.as_str(), iface).await?;

            let dbus = DBusProxy::new(&conn).await?;
            let well_known = WellKnownName::try_from(bus_name.as_str())?;
            let flags = RequestNameFlags::DO_NOT_QUEUE | RequestNameFlags::ALLOW_REPLACEMENT;

            let reply = dbus.request_name(well_known.clone(), flags).await?;
            match reply {
                RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner => {
                    log::info!("Acquired {bus_name} immediately (priority={priority})");
                }
                RequestNameReply::Exists | RequestNameReply::InQueue => {
                    log::info!("{bus_name} is owned; calling RequestRelease({priority})");
                    let owner_proxy = Proxy::new(
                        &conn,
                        bus_name.as_str(),
                        obj_path.as_str(),
                        "org.freedesktop.ReserveDevice1",
                    )
                    .await?;
                    let granted: bool = owner_proxy.call("RequestRelease", &(priority,)).await?;
                    if !granted {
                        let _ = conn
                            .object_server()
                            .remove::<ReserveDeviceInterface, _>(obj_path.as_str())
                            .await;
                        return Err(AlsaError::ReservationDenied { card });
                    }

                    // Claim the bus name immediately before WirePlumber re-acquires it
                    // (WirePlumber drops and re-probes within ~1 ms if unowned).
                    let claim_flags = flags | RequestNameFlags::REPLACE_EXISTING;
                    let mut acquired = matches!(
                        dbus.request_name(well_known.clone(), claim_flags).await?,
                        RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner
                    );

                    if !acquired {
                        for _ in 0..40 {
                            std::thread::sleep(Duration::from_millis(5));
                            match dbus.request_name(well_known.clone(), claim_flags).await? {
                                RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner => {
                                    acquired = true;
                                    break;
                                }
                                _ => {}
                            }
                        }
                    }
                    if !acquired {
                        let _ = conn
                            .object_server()
                            .remove::<ReserveDeviceInterface, _>(obj_path.as_str())
                            .await;
                        return Err(AlsaError::ReservationTimeout { card });
                    }
                    // Give WirePlumber a brief moment to finish closing its ALSA fd if it
                    // emitted the name release slightly ahead of snd_pcm_close().
                    std::thread::sleep(Duration::from_millis(50));
                    log::info!("Acquired {bus_name} after RequestRelease (priority={priority})");
                }
            }

            Ok(Self {
                conn,
                bus_name,
                obj_path,
                card,
                priority,
                release_requested,
            })
        })
    }

    /// Card index held by this reservation.
    pub fn card(&self) -> u32 {
        self.card
    }

    /// Configured reservation priority.
    pub fn priority(&self) -> i32 {
        self.priority
    }

    /// Returns `true` if a higher-priority requester asked us to release.
    pub fn release_requested(&self) -> bool {
        self.release_requested.load(Ordering::Acquire)
    }

    /// Shareable flag that flips to `true` when a higher-priority requester is granted release.
    pub fn release_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.release_requested)
    }

    /// Explicitly release the D-Bus reservation.
    pub fn release(self) {
        drop(self);
    }
}

impl Drop for DeviceReservation {
    fn drop(&mut self) {
        let bus_name = self.bus_name.clone();
        let obj_path = self.obj_path.clone();
        let conn = self.conn.clone();
        zbus::block_on(async {
            let _ = conn
                .object_server()
                .remove::<ReserveDeviceInterface, _>(obj_path.as_str())
                .await;
            if let Ok(well_known) = WellKnownName::try_from(bus_name.as_str())
                && let Ok(dbus) = DBusProxy::new(&conn).await
            {
                let _ = dbus.release_name(well_known).await;
                log::info!("Released {bus_name}");
            }
        });
    }
}

/// Resolve an ALSA PCM device string (such as `"hw:Loopback,0,0"` or `"hw:10"`)
/// into its kernel ALSA card index.
pub fn card_index_from_device(device: &str) -> Result<u32> {
    let rest = device
        .strip_prefix("hw:")
        .or_else(|| device.strip_prefix("plughw:"))
        .ok_or_else(|| AlsaError::InvalidDeviceName(device.to_string()))?;

    let card_part = rest.split(',').next().unwrap_or("");
    if card_part.is_empty() {
        return Err(AlsaError::InvalidDeviceName(device.to_string()));
    }
    if let Ok(idx) = card_part.parse::<u32>() {
        return Ok(idx);
    }
    if let Some(idx_str) = card_part.strip_prefix("CARD=")
        && let Ok(idx) = idx_str.parse::<u32>()
    {
        return Ok(idx);
    }
    let name_part = card_part.strip_prefix("CARD=").unwrap_or(card_part);
    let cstr =
        CString::new(name_part).map_err(|_| AlsaError::InvalidDeviceName(device.to_string()))?;
    let card = alsa::card::Card::from_str(&cstr)?;
    let idx = card.get_index();
    if idx < 0 {
        return Err(AlsaError::InvalidDeviceName(device.to_string()));
    }
    Ok(idx as u32)
}

/// Whether the card and the playback PCM `device` names (`hw:` or
/// `plughw:`, by card index or id, `DEV=` or positional device, default 0)
/// are in the system now. Reads `/proc/asound` only; nothing is opened, so
/// it is cheap enough to poll and never disturbs a device someone holds.
pub fn device_present(device: &str) -> bool {
    device_present_in(std::path::Path::new("/proc/asound"), device)
}

/// [`device_present`] against another `/proc/asound` tree (tests).
pub fn device_present_in(root: &std::path::Path, device: &str) -> bool {
    let Some(rest) = device
        .strip_prefix("hw:")
        .or_else(|| device.strip_prefix("plughw:"))
    else {
        return false;
    };
    let mut parts = rest.split(',').map(str::trim);
    let card = parts.next().unwrap_or("");
    let card = card.strip_prefix("CARD=").unwrap_or(card);
    if card.is_empty() {
        return false;
    }
    let dev = match parts.next() {
        None => 0,
        Some(p) => match p.strip_prefix("DEV=").unwrap_or(p).parse::<u32>() {
            Ok(d) => d,
            Err(_) => return false,
        },
    };
    let card_dir = if card.bytes().all(|b| b.is_ascii_digit()) {
        root.join(format!("card{card}"))
    } else {
        // The `/proc/asound/<id>` symlink is not always there: match the
        // id file of each card instead.
        let Ok(entries) = std::fs::read_dir(root) else {
            return false;
        };
        let found = entries.flatten().find(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.strip_prefix("card")
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
                && std::fs::read_to_string(e.path().join("id")).is_ok_and(|id| id.trim() == card)
        });
        match found {
            Some(e) => e.path(),
            None => return false,
        }
    };
    card_dir.join(format!("pcm{dev}p")).exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_numeric_card() {
        assert_eq!(card_index_from_device("hw:10,0,0").unwrap(), 10);
        assert_eq!(card_index_from_device("hw:0").unwrap(), 0);
        assert_eq!(card_index_from_device("plughw:2,1").unwrap(), 2);
    }

    #[test]
    fn device_presence_reads_proc_asound() {
        let root = std::env::temp_dir().join(format!("avdsp-asound-{}", std::process::id()));
        let card = root.join("card3");
        std::fs::create_dir_all(&card).unwrap();
        std::fs::write(card.join("id"), "USB\n").unwrap();
        std::fs::create_dir_all(card.join("pcm0p")).unwrap();
        std::fs::create_dir_all(card.join("pcm1c")).unwrap();
        assert!(device_present_in(&root, "hw:CARD=USB,DEV=0"));
        assert!(device_present_in(&root, "hw:USB"));
        assert!(device_present_in(&root, "hw:3,0,0"));
        assert!(device_present_in(&root, "plughw:CARD=3,DEV=0"));
        // Capture-only device, another card, a malformed name.
        assert!(!device_present_in(&root, "hw:CARD=USB,DEV=1"));
        assert!(!device_present_in(&root, "hw:CARD=DAC,DEV=0"));
        assert!(!device_present_in(&root, "hw:4,0"));
        assert!(!device_present_in(&root, "default"));
        assert!(!device_present_in(&root, "hw:USB,x"));
        // A card whose playback PCM is gone (unplugged mid-teardown).
        let gone = root.join("card4");
        std::fs::create_dir_all(&gone).unwrap();
        std::fs::write(gone.join("id"), "DAC\n").unwrap();
        assert!(!device_present_in(&root, "hw:CARD=DAC,DEV=0"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn reject_non_hw_device() {
        assert!(card_index_from_device("default").is_err());
        assert!(card_index_from_device("hw:").is_err());
    }

    #[test]
    fn request_release_respects_priority_and_invokes_wakeup() {
        let release_requested = Arc::new(AtomicBool::new(false));
        let woken = Arc::new(AtomicBool::new(false));
        let woken_cb = Arc::clone(&woken);
        let iface = ReserveDeviceInterface {
            priority: 1000,
            app_name: "Audivane DSP".to_string(),
            app_device_name: "hw:Loopback,0,0".to_string(),
            release_requested: Arc::clone(&release_requested),
            on_release: Some(Arc::new(move || {
                woken_cb.store(true, Ordering::Release);
            })),
        };

        assert!(!iface.request_release(999));
        assert!(!iface.request_release(1000));
        assert!(!release_requested.load(Ordering::Acquire));
        assert!(!woken.load(Ordering::Acquire));

        assert!(iface.request_release(1001));
        assert!(release_requested.load(Ordering::Acquire));
        assert!(woken.load(Ordering::Acquire));
    }
}
