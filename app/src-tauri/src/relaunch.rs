//! Noticing that a package upgrade replaced the app's binary, and the plain
//! parts of relaunching into the new one (no GTK, so tested with the workspace).
//! The Tauri side (`app.rs`) decides when to look and replaces the process with
//! `exec`: same PID, same session or autostart unit, with every descriptor of
//! the old image (its tray item, its single-instance D-Bus name) closed before
//! the new image starts.

use std::ffi::{OsStr, OsString};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// Set on the relaunched process.
pub const ENV_RELAUNCHED: &str = "AUDIVANE_RELAUNCHED";
/// The main view's place (JSON) for the relaunched page.
pub const ENV_MAIN_VIEW: &str = "AUDIVANE_MAIN_VIEW";
/// The old image's child processes (WebKit's), for the new one to reap.
pub const ENV_REAP: &str = "AUDIVANE_REAP_PIDS";

/// What identifies one installed file: a rewrite in place changes the size
/// or the time, a replacement the inode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileId {
    dev: u64,
    ino: u64,
    size: u64,
    mtime_ns: i128,
}

impl FileId {
    pub fn of(path: &Path) -> std::io::Result<FileId> {
        let m = std::fs::metadata(path)?;
        Ok(FileId {
            dev: m.dev(),
            ino: m.ino(),
            size: m.size(),
            mtime_ns: i128::from(m.mtime()) * 1_000_000_000 + i128::from(m.mtime_nsec()),
        })
    }

    fn same_inode(&self, other: &FileId) -> bool {
        self.dev == other.dev && self.ino == other.ino
    }
}

/// `/proc/self/exe` read as a link: the path the binary was started from,
/// without the " (deleted)" the kernel adds once that file was replaced.
pub fn installed_path(link: &Path) -> std::io::Result<PathBuf> {
    let target = std::fs::read_link(link)?;
    Ok(strip_deleted(target))
}

fn strip_deleted(p: PathBuf) -> PathBuf {
    match p.to_str().and_then(|s| s.strip_suffix(" (deleted)")) {
        Some(s) => PathBuf::from(s),
        None => p,
    }
}

/// The file now at `path` when it is not the running binary (`running`, e.g.
/// `/proc/self/exe`, which follows the running inode even after deletion) and
/// looks complete enough to start: a regular, executable ELF file. `None` when
/// nothing changed or the new file is not ready.
pub fn replacement(running: &Path, path: &Path) -> Option<FileId> {
    let now = FileId::of(path).ok()?;
    let old = FileId::of(running).ok()?;
    if now.same_inode(&old) {
        return None;
    }
    runnable(path).then_some(now)
}

/// A regular file this user may execute, starting with the ELF magic (an
/// empty or half-written file has not got it yet).
fn runnable(path: &Path) -> bool {
    use std::io::Read;
    use std::os::unix::ffi::OsStrExt;
    let Ok(m) = std::fs::metadata(path) else {
        return false;
    };
    if !m.is_file() || m.mode() & 0o111 == 0 {
        return false;
    }
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: a valid NUL-terminated path.
    if unsafe { libc::access(c.as_ptr(), libc::X_OK) } != 0 {
        return false;
    }
    let mut magic = [0u8; 4];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut magic))
        .is_ok()
        && magic == *b"\x7fELF"
}

/// The relaunch's arguments: the old ones (without the program name),
/// with `--hidden` only when no main window is open, so the new process
/// opens the window again or starts in the tray.
pub fn relaunch_args(old: &[OsString], hidden: bool) -> Vec<OsString> {
    let mut args: Vec<OsString> = old
        .iter()
        .skip(1)
        .filter(|a| a.as_os_str() != OsStr::new("--hidden"))
        .cloned()
        .collect();
    if hidden {
        args.push("--hidden".into());
    }
    args
}

/// Replace this process with `exe` (`argv[0]` = `exe`, then `args`), its
/// environment changed by `env` (a `None` value removes the variable).
/// `execve` itself, not `execvp`: that one runs a file the kernel refuses
/// (`ENOEXEC`) through `/bin/sh`, which would end the app instead of
/// failing. Returns only on failure.
pub fn exec(exe: &Path, args: &[OsString], env: &[(&str, Option<OsString>)]) -> std::io::Error {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let cstr = |b: &[u8]| CString::new(b).map_err(|_| std::io::ErrorKind::InvalidInput);
    let built = (|| -> Result<(CString, Vec<CString>, Vec<CString>), std::io::ErrorKind> {
        let path = cstr(exe.as_os_str().as_bytes())?;
        let mut argv = vec![path.clone()];
        for a in args {
            argv.push(cstr(a.as_bytes())?);
        }
        let mut envp = Vec::new();
        for (k, v) in std::env::vars_os() {
            if env
                .iter()
                .any(|(name, _)| k.as_os_str() == OsStr::new(name))
            {
                continue;
            }
            let mut kv = k.as_bytes().to_vec();
            kv.push(b'=');
            kv.extend_from_slice(v.as_bytes());
            envp.push(cstr(&kv)?);
        }
        for (k, v) in env {
            if let Some(v) = v {
                let mut kv = k.as_bytes().to_vec();
                kv.push(b'=');
                kv.extend_from_slice(v.as_bytes());
                envp.push(cstr(&kv)?);
            }
        }
        Ok((path, argv, envp))
    })();
    let (path, argv, envp) = match built {
        Ok(b) => b,
        Err(kind) => return kind.into(),
    };
    let mut argv_p: Vec<*const libc::c_char> = argv.iter().map(|a| a.as_ptr()).collect();
    argv_p.push(std::ptr::null());
    let mut envp_p: Vec<*const libc::c_char> = envp.iter().map(|e| e.as_ptr()).collect();
    envp_p.push(std::ptr::null());
    // SAFETY: NUL-terminated strings and NULL-terminated arrays that
    // outlive the call.
    unsafe { libc::execve(path.as_ptr(), argv_p.as_ptr(), envp_p.as_ptr()) };
    std::io::Error::last_os_error()
}

/// The processes whose parent is `pid` (from `/proc/<n>/stat`).
pub fn child_pids(pid: u32) -> Vec<u32> {
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    dir.filter_map(Result::ok)
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter(|&n| {
            std::fs::read_to_string(format!("/proc/{n}/stat"))
                .ok()
                .and_then(|s| parent_of(&s))
                == Some(pid)
        })
        .collect()
}

/// The parent PID in a `/proc/<n>/stat` line; the command name (in
/// parentheses) may hold spaces and parentheses itself.
fn parent_of(stat: &str) -> Option<u32> {
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(1)?.parse().ok()
}

pub fn format_pids(pids: &[u32]) -> String {
    pids.iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

pub fn parse_pids(s: &str) -> Vec<u32> {
    s.split(',').filter_map(|p| p.trim().parse().ok()).collect()
}

/// Reap the old image's children (WebKit's processes, which end once their
/// connection to the old image closed): after `exec` they are still this
/// process's children and nothing else waits for them. Waits on each PID by
/// name, so the new image's own children are left alone.
pub fn reap(pids: Vec<u32>) {
    if pids.is_empty() {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("reap-old".into())
        .spawn(move || {
            for pid in pids {
                let Ok(pid) = libc::pid_t::try_from(pid) else {
                    continue;
                };
                let mut status = 0;
                // SAFETY: plain syscall; fails with ECHILD for a PID that
                // is not (or no longer) our child.
                unsafe { libc::waitpid(pid, &mut status, 0) };
            }
        });
}

/// Wait (up to `limit`) until nobody owns the session-bus name `name`: the old
/// image's single-instance name, released when `exec` closed its connection.
/// The bus may see that a moment later; asking too early would hand this
/// launch to the old instance, gone.
pub fn wait_name_free(name: &str, limit: std::time::Duration) {
    zbus::block_on(async {
        let Some(Ok(conn)) = crate::dbus_util::timeout(limit, zbus::Connection::session()).await
        else {
            return;
        };
        let start = std::time::Instant::now();
        while start.elapsed() < limit {
            if crate::dbus_util::name_owner(&conn, name).await.is_none() {
                return;
            }
            crate::dbus_util::sleep(std::time::Duration::from_millis(50))
                .await
                .ok();
        }
        log::warn!("{name} is still owned after {limit:?}; starting anyway");
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn write_exe(path: &Path, body: &[u8], mode: u32) {
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn a_file_the_kernel_refuses_fails_the_exec_instead_of_running_a_shell() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app");
        // Passes `runnable` (ELF magic, executable) but is no program.
        write_exe(&path, b"\x7fELF\nexit 0\n", 0o755);
        let err = exec(&path, &[], &[(ENV_RELAUNCHED, Some("1".into()))]);
        assert_eq!(err.raw_os_error(), Some(libc::ENOEXEC));
    }
}
