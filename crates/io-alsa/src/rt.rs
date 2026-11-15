//! Real-time thread priority via `org.freedesktop.RealtimeKit1`.

use crate::error::{AlsaError, Result};
use zbus::{Connection, Proxy};

/// Promote the calling thread to `SCHED_FIFO` real-time priority via rtkit.
///
/// Clamps `requested_priority` to the system's `MaxRealtimePriority` and sets
/// `RLIMIT_RTTIME` to `RTTimeUSecMax` as required by `rtkit-daemon`.
/// Returns the granted priority on success.
pub fn promote_current_thread_rt(requested_priority: u32) -> Result<u32> {
    let tid = unsafe { libc::syscall(libc::SYS_gettid) };
    if tid <= 0 {
        return Err(AlsaError::Io(std::io::Error::last_os_error()));
    }

    zbus::block_on(async {
        let conn = Connection::system().await?;
        let proxy = Proxy::new(
            &conn,
            "org.freedesktop.RealtimeKit1",
            "/org/freedesktop/RealtimeKit1",
            "org.freedesktop.RealtimeKit1",
        )
        .await?;

        let max_prio: i32 = proxy.get_property("MaxRealtimePriority").await?;
        let max_rttime_usec: i64 = proxy.get_property("RTTimeUSecMax").await?;

        if max_prio <= 0 {
            return Err(AlsaError::Rtkit(format!(
                "rtkit MaxRealtimePriority is {max_prio}"
            )));
        }
        let prio = requested_priority.clamp(1, max_prio as u32);

        if max_rttime_usec > 0 {
            let limit = libc::rlimit {
                rlim_cur: max_rttime_usec as libc::rlim_t,
                rlim_max: max_rttime_usec as libc::rlim_t,
            };
            // Safety: standard POSIX setrlimit for RLIMIT_RTTIME on the calling process/thread.
            let rc = unsafe { libc::setrlimit(libc::RLIMIT_RTTIME, &limit) };
            if rc != 0 {
                return Err(AlsaError::Io(std::io::Error::last_os_error()));
            }
        }

        let _: () = proxy
            .call("MakeThreadRealtime", &(tid as u64, prio))
            .await?;
        Ok(prio)
    })
}

/// Best-effort promotion to real-time priority via rtkit; logs a warning and
/// returns `None` if rtkit is unavailable or denies the request.
pub fn try_promote_current_thread_rt(requested_priority: u32) -> Option<u32> {
    match promote_current_thread_rt(requested_priority) {
        Ok(prio) => {
            log::info!("ALSA thread promoted to SCHED_FIFO priority {prio} via rtkit");
            Some(prio)
        }
        Err(err) => {
            log::warn!("Could not promote ALSA thread via rtkit (continuing without RT): {err}");
            None
        }
    }
}
