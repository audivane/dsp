//! `avdsp upgrade`: update a packaged install in place from the GitHub
//! releases.
//!
//! 1. Find which package manager owns this `avdsp` (pacman, dpkg or rpm) and
//!    the installed version. A source build (not owned by a package) is not
//!    upgraded.
//! 2. Read the release (`releases/latest`, or `releases/tags/<tag>` with
//!    `--version`) from the GitHub API with `curl`.
//! 3. Download the matching package and `SHA256SUMS` into a fresh directory
//!    under `$XDG_CACHE_HOME/audivane-dsp/upgrade`, check the SHA-256, and
//!    install the local file with `sudo pacman -U` / `sudo apt install` /
//!    `sudo dnf install`.
//! 4. `systemctl --user daemon-reload`, restart the engine if it was running,
//!    and wait until it answers with the new version.
//!
//! Exit codes: 0 done (or up to date), 10 `--check` found an update,
//! 1 the update ran but the engine did not come back on the new version,
//! 3 any other error.
//!
//! Testing: `AUDIVANE_UPGRADE_API` replaces the API base
//! (`https://api.github.com/repos/audivane/dsp`), e.g. with a local
//! `http://127.0.0.1:8000` serving `releases/latest` and the assets. With it
//! set, `AUDIVANE_UPGRADE_TEST_AS_ROOT=1` lets a root shell (a build root
//! without sudo) run the package step directly. Neither is for normal use.

use anyhow::{Context, Result, bail};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::ffi::OsString;
use std::io::{BufRead, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// `--check`: a newer release exists.
pub const EXIT_UPDATE_AVAILABLE: i32 = 10;

const DEFAULT_API: &str = "https://api.github.com/repos/audivane/dsp";
const API_ENV: &str = "AUDIVANE_UPGRADE_API";
const TEST_ROOT_ENV: &str = "AUDIVANE_UPGRADE_TEST_AS_ROOT";
const USER_AGENT: &str = "audivane-dsp-upgrade";
const PACKAGE: &str = "audivane-dsp";
const UNIT: &str = "audivane-dsp.service";
const UI_PROCESS: &str = "audivane-dsp-ui";
const ENGINE_WAIT: Duration = Duration::from_secs(20);

#[derive(clap::Args, Debug)]
// `--version` below is the release to install, not the program version.
#[command(disable_version_flag = true)]
pub struct UpgradeArgs {
    /// Only report whether an update exists: exit 0 when up to date, 10 when
    /// a newer release is available.
    #[arg(long)]
    pub check: bool,
    /// Do not ask before installing.
    #[arg(long, short = 'y')]
    pub yes: bool,
    /// Install this release tag (e.g. 0.9.1) instead of the latest; also
    /// reinstalls or downgrades.
    #[arg(long = "version", value_name = "TAG")]
    pub tag: Option<String>,
}

// ---------------------------------------------------------------- versions

/// A release version, compared like semver: numeric `X.Y.Z` (missing parts
/// are 0), then a pre-release (`-rc.1`) sorts before the release itself.
/// Build metadata (`+…`) is ignored.
#[derive(Debug, Clone)]
pub struct Version {
    text: String,
    nums: Vec<u64>,
    pre: Vec<String>,
}

impl Version {
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let bare = text.strip_prefix(['v', 'V']).unwrap_or(text);
        let bare = bare.split('+').next().unwrap_or(bare);
        // Debian spells a pre-release `0.10.0~rc.1`.
        let bare = bare.replacen('~', "-", 1);
        let (core, pre) = match bare.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (bare.as_str(), None),
        };
        let nums = core
            .split('.')
            .map(|part| part.parse::<u64>().ok())
            .collect::<Option<Vec<_>>>()?;
        if nums.is_empty() || nums.len() > 4 {
            return None;
        }
        let pre = match pre {
            Some("") => return None,
            Some(pre) => pre.split('.').map(str::to_owned).collect(),
            None => Vec::new(),
        };
        Some(Self {
            text: bare.trim_start_matches(['v', 'V']).to_string(),
            nums,
            pre,
        })
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        let len = self.nums.len().max(other.nums.len());
        for i in 0..len {
            let a = self.nums.get(i).copied().unwrap_or(0);
            let b = other.nums.get(i).copied().unwrap_or(0);
            match a.cmp(&b) {
                Ordering::Equal => {}
                unequal => return unequal,
            }
        }
        match (self.pre.is_empty(), other.pre.is_empty()) {
            (true, true) => return Ordering::Equal,
            (true, false) => return Ordering::Greater,
            (false, true) => return Ordering::Less,
            (false, false) => {}
        }
        for (a, b) in self.pre.iter().zip(&other.pre) {
            let order = match (a.parse::<u64>(), b.parse::<u64>()) {
                (Ok(a), Ok(b)) => a.cmp(&b),
                (Ok(_), Err(_)) => Ordering::Less,
                (Err(_), Ok(_)) => Ordering::Greater,
                (Err(_), Err(_)) => a.cmp(b),
            };
            if order != Ordering::Equal {
                return order;
            }
        }
        self.pre.len().cmp(&other.pre.len())
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Version {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Version {}

/// The upstream part of a pacman/dpkg version: `1:0.9.1-1` → `0.9.1`.
fn upstream_version(full: &str) -> &str {
    let full = full.trim();
    let full = full.split_once(':').map_or(full, |(_, rest)| rest);
    match full.rsplit_once('-') {
        Some((upstream, rel)) if !rel.is_empty() && rel.chars().all(|c| c.is_ascii_digit()) => {
            upstream
        }
        _ => full,
    }
}

/// `--check` exit code for `installed` against the release `target`.
pub fn check_exit_code(installed: &Version, target: &Version) -> i32 {
    if target > installed {
        EXIT_UPDATE_AVAILABLE
    } else {
        0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Upgrade,
    Reinstall,
    Downgrade,
}

/// What to do to go from `installed` to `target`. Without an explicit
/// `--version`, an equal or older release means "up to date" (None).
fn decide(installed: &Version, target: &Version, explicit: bool) -> Option<Action> {
    match target.cmp(installed) {
        Ordering::Greater => Some(Action::Upgrade),
        Ordering::Equal if explicit => Some(Action::Reinstall),
        Ordering::Less if explicit => Some(Action::Downgrade),
        _ => None,
    }
}

// ---------------------------------------------------------------- packages

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PackageManager {
    Pacman,
    Dpkg,
    Rpm,
}

impl PackageManager {
    fn name(self) -> &'static str {
        match self {
            Self::Pacman => "pacman",
            Self::Dpkg => "dpkg",
            Self::Rpm => "rpm",
        }
    }

    /// Does `name` look like this format's release asset for `version`?
    /// `audivane-dsp-0.9.1-1-x86_64.pkg.tar.zst`, `audivane-dsp_0.9.1-1_amd64.deb`,
    /// `audivane-dsp-0.9.1-1.x86_64.rpm` (the trailing `1` is the package release).
    fn matches_asset(self, name: &str, version: &str) -> bool {
        let (prefix, suffix) = match self {
            Self::Pacman => (format!("{PACKAGE}-{version}-"), "-x86_64.pkg.tar.zst"),
            Self::Dpkg => (format!("{PACKAGE}_{version}-"), "_amd64.deb"),
            Self::Rpm => (format!("{PACKAGE}-{version}-"), ".x86_64.rpm"),
        };
        name.strip_prefix(&prefix)
            .and_then(|rest| rest.strip_suffix(suffix))
            .is_some_and(|rel| !rel.is_empty() && rel.chars().all(|c| c.is_ascii_digit()))
    }

    /// The command (without sudo) that installs the local package `file`.
    fn install_command(self, action: Action, file: &Path) -> Vec<OsString> {
        let mut cmd: Vec<OsString> = match (self, action) {
            (Self::Pacman, _) => vec!["pacman".into(), "-U".into(), "--noconfirm".into()],
            (Self::Dpkg, Action::Upgrade) => vec!["apt".into(), "install".into(), "-y".into()],
            (Self::Dpkg, Action::Reinstall) => {
                vec![
                    "apt".into(),
                    "install".into(),
                    "-y".into(),
                    "--reinstall".into(),
                ]
            }
            (Self::Dpkg, Action::Downgrade) => vec![
                "apt".into(),
                "install".into(),
                "-y".into(),
                "--allow-downgrades".into(),
            ],
            (Self::Rpm, Action::Upgrade) => vec!["dnf".into(), "install".into(), "-y".into()],
            (Self::Rpm, Action::Reinstall) => vec!["dnf".into(), "reinstall".into(), "-y".into()],
            (Self::Rpm, Action::Downgrade) => vec!["dnf".into(), "downgrade".into(), "-y".into()],
        };
        // An absolute path: apt and dnf treat it as a local file, not a name.
        cmd.push(file.as_os_str().to_owned());
        cmd
    }
}

/// The release asset for `pm` and `version`, by name.
fn select_asset<'a>(assets: &'a [Asset], pm: PackageManager, version: &str) -> Option<&'a Asset> {
    assets.iter().find(|a| pm.matches_asset(&a.name, version))
}

struct Installed {
    pm: PackageManager,
    version: Version,
}

/// Which package owns `exe`, and its version. None for a source build.
fn detect_installed(exe: &Path) -> Result<Option<Installed>> {
    let exe = exe.to_string_lossy().into_owned();
    for pm in [
        PackageManager::Pacman,
        PackageManager::Dpkg,
        PackageManager::Rpm,
    ] {
        let found = match pm {
            PackageManager::Pacman if have("pacman") => {
                let owner = query(&["pacman", "-Qqo", &exe]);
                if owner.as_deref().map(str::trim) == Some(PACKAGE) {
                    query(&["pacman", "-Q", PACKAGE])
                        .and_then(|s| s.split_whitespace().nth(1).map(str::to_owned))
                } else {
                    None
                }
            }
            PackageManager::Dpkg if have("dpkg-query") => {
                let owner = query(&["dpkg-query", "-S", &exe]);
                let owned = owner.as_deref().is_some_and(|s| {
                    s.lines().any(|l| {
                        l.split_once(':')
                            .is_some_and(|(pkgs, _)| pkgs.split(',').any(|p| p.trim() == PACKAGE))
                    })
                });
                if owned {
                    query(&["dpkg-query", "-W", "-f=${Version}", PACKAGE])
                } else {
                    None
                }
            }
            PackageManager::Rpm if have("rpm") => {
                query(&["rpm", "-qf", "--qf", "%{NAME}\\t%{VERSION}\\n", &exe]).and_then(|s| {
                    s.lines().find_map(|l| {
                        let (name, version) = l.split_once('\t')?;
                        (name == PACKAGE).then(|| version.to_string())
                    })
                })
            }
            _ => None,
        };
        if let Some(full) = found {
            let version = Version::parse(upstream_version(&full)).with_context(|| {
                format!("cannot read the installed {} version '{full}'", pm.name())
            })?;
            return Ok(Some(Installed { pm, version }));
        }
    }
    Ok(None)
}

/// The AppImage's per-user install (`~/.local/lib/audivane-dsp/`, see
/// packaging/appimage/), or the AppImage itself: updated by a new AppImage.
fn from_appimage(exe: &Path) -> bool {
    std::env::var_os("APPIMAGE").is_some_and(|a| !a.is_empty())
        || exe
            .parent()
            .is_some_and(|d| d.ends_with(".local/lib/audivane-dsp"))
}

const APPIMAGE_HINT: &str = "this avdsp comes from the AppImage, which `avdsp upgrade` does not \
     update: download the new AppImage from https://github.com/audivane/dsp/releases and open it \
     (it updates the user install and restarts the engine)";

// ---------------------------------------------------------------- releases

#[derive(Debug, Clone, PartialEq, Eq)]
struct Asset {
    name: String,
    url: String,
}

#[derive(Debug)]
struct Release {
    tag: String,
    version: Version,
    notes_url: String,
    assets: Vec<Asset>,
}

fn parse_release(json: &str) -> Result<Release> {
    let value: Value = serde_json::from_str(json).context("the release answer is not JSON")?;
    let tag = value
        .get("tag_name")
        .and_then(Value::as_str)
        .context("the release answer has no tag_name")?
        .to_string();
    let version =
        Version::parse(&tag).with_context(|| format!("release tag '{tag}' is not a version"))?;
    let notes_url = value
        .get("html_url")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let assets = value
        .get("assets")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|a| {
                    Some(Asset {
                        name: a.get("name")?.as_str()?.to_string(),
                        url: a.get("browser_download_url")?.as_str()?.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Release {
        tag,
        version,
        notes_url,
        assets,
    })
}

/// `SHA256SUMS` (`sha256sum` output): lowercase hash per file name.
fn parse_sha256sums(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim_end();
            let (hash, rest) = line.split_once(char::is_whitespace)?;
            // `hash  name` (text mode) or `hash *name` (binary mode).
            let name = rest.strip_prefix([' ', '*']).unwrap_or(rest);
            let name = name.strip_prefix('*').unwrap_or(name);
            let ok = hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit());
            (ok && !name.is_empty()).then(|| (hash.to_ascii_lowercase(), name.to_string()))
        })
        .collect()
}

fn expected_sha256(sums: &str, name: &str) -> Option<String> {
    parse_sha256sums(sums)
        .into_iter()
        .find(|(_, n)| n == name)
        .map(|(h, _)| h)
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file =
        std::fs::File::open(path).with_context(|| format!("cannot read {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Fetches over HTTPS with `curl` (HTTP only when `AUDIVANE_UPGRADE_API`
/// points at a test server).
struct Fetcher {
    api: String,
    https_only: bool,
}

impl Fetcher {
    fn from_env() -> Self {
        match std::env::var(API_ENV) {
            Ok(api) if !api.trim().is_empty() => Self {
                api: api.trim().trim_end_matches('/').to_string(),
                https_only: false,
            },
            _ => Self {
                api: DEFAULT_API.to_string(),
                https_only: true,
            },
        }
    }

    fn curl(&self) -> Command {
        let mut cmd = Command::new("curl");
        cmd.args([
            "-fL",
            "--retry",
            "2",
            "--connect-timeout",
            "20",
            "-A",
            USER_AGENT,
        ]);
        if self.https_only {
            cmd.args(["--proto", "=https", "--proto-redir", "=https"]);
        }
        cmd
    }

    fn release(&self, tag: Option<&str>) -> Result<Release> {
        let url = match tag {
            Some(tag) => format!("{}/releases/tags/{tag}", self.api),
            None => format!("{}/releases/latest", self.api),
        };
        let out = self
            .curl()
            .args(["-sS", "-H", "Accept: application/vnd.github+json", &url])
            .stdin(Stdio::null())
            .output()
            .context("cannot run curl")?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            let what = tag.map_or("the latest release".to_string(), |t| format!("release {t}"));
            if err.contains("404") {
                bail!("{what} was not found ({url})");
            }
            bail!("cannot read {what} from {url}: {}", err.trim());
        }
        parse_release(&String::from_utf8_lossy(&out.stdout))
    }

    fn download(&self, url: &str, dest: &Path, progress: bool) -> Result<()> {
        let mut cmd = self.curl();
        cmd.arg(if progress { "--progress-bar" } else { "-sS" });
        let status = cmd
            .arg("-o")
            .arg(dest)
            .arg(url)
            .stdin(Stdio::null())
            .status()
            .context("cannot run curl")?;
        if !status.success() {
            bail!("downloading {url} failed ({status})");
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- system

fn have(program: &str) -> bool {
    find_in_path(program).is_some()
}

fn find_in_path(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|p| p.is_file())
}

/// Run a query command; its stdout when it succeeded.
fn query(args: &[&str]) -> Option<String> {
    let out = Command::new(args[0])
        .args(&args[1..])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn geteuid() -> u32 {
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    // SAFETY: geteuid has no preconditions.
    unsafe { geteuid() }
}

/// Root may run the package step itself only in a test against a local server.
fn test_as_root() -> bool {
    std::env::var(TEST_ROOT_ENV).as_deref() == Ok("1")
        && std::env::var(API_ENV).is_ok_and(|v| !v.trim().is_empty())
}

fn cache_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from)
        && dir.is_absolute()
    {
        return Ok(dir);
    }
    let home = std::env::var_os("HOME").context("neither XDG_CACHE_HOME nor HOME is set")?;
    Ok(PathBuf::from(home).join(".cache"))
}

/// A new, empty directory for this run's downloads.
fn fresh_download_dir(tag: &str) -> Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    let base = cache_dir()?.join("audivane-dsp").join("upgrade");
    std::fs::create_dir_all(&base).with_context(|| format!("cannot create {}", base.display()))?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let safe_tag: String = tag
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let dir = base.join(format!("{safe_tag}-{}-{nanos}", std::process::id()));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .with_context(|| format!("cannot create {}", dir.display()))?;
    Ok(dir)
}

fn systemctl_user(args: &[&str]) -> Option<std::process::Output> {
    Command::new("systemctl")
        .arg("--user")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .ok()
}

/// `systemctl --user is-active` of the unit: Some(true/false), None without
/// a reachable user manager.
fn service_active() -> Option<bool> {
    if !have("systemctl") {
        return None;
    }
    let out = systemctl_user(&["is-active", UNIT])?;
    let state = String::from_utf8_lossy(&out.stdout).trim().to_string();
    match state.as_str() {
        "active" | "activating" | "reloading" | "deactivating" => Some(true),
        "inactive" | "failed" | "unknown" => Some(false),
        // Empty output: no user manager on the bus.
        _ => None,
    }
}

fn wait_for_engine(timeout: Duration) -> Option<String> {
    let start = Instant::now();
    loop {
        if let Some(v) = crate::ctl::engine_version(Duration::from_millis(800)) {
            return Some(v);
        }
        if start.elapsed() >= timeout {
            return None;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Is the desktop app running for this user?
fn ui_running() -> bool {
    use std::os::unix::fs::MetadataExt;
    let me = geteuid();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return false;
    };
    entries.flatten().any(|e| {
        let pid_dir = e.path();
        let is_pid = e
            .file_name()
            .to_string_lossy()
            .chars()
            .all(|c| c.is_ascii_digit());
        is_pid
            && std::fs::metadata(&pid_dir).is_ok_and(|m| m.uid() == me)
            && std::fs::read_to_string(pid_dir.join("comm")).is_ok_and(|c| c.trim() == UI_PROCESS)
    })
}

fn confirm(question: &str) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        bail!("not asking on a non-interactive input; pass --yes to upgrade without a prompt");
    }
    print!("{question} [y/N] ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes" | "Yes" | "YES"))
}

// ---------------------------------------------------------------- entry

pub fn run(args: UpgradeArgs) -> Result<i32> {
    let cli_version = Version::parse(env!("CARGO_PKG_VERSION")).context("bad CLI version")?;
    let as_root = geteuid() == 0;
    if as_root && !args.check && !test_as_root() {
        bail!(
            "run `avdsp upgrade` as your normal user, not as root: it restarts your \
             user's engine service and asks sudo only for the package step"
        );
    }
    if !have("curl") {
        bail!("`avdsp upgrade` needs curl; install it with your package manager");
    }

    let exe = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .context("cannot find this program's path")?;
    let installed = detect_installed(&exe)?;
    let current = installed.as_ref().map_or(&cli_version, |i| &i.version);

    let fetcher = Fetcher::from_env();
    if !fetcher.https_only {
        eprintln!(
            "note: using the test release server {} ({API_ENV})",
            fetcher.api
        );
    }
    let release = fetcher.release(args.tag.as_deref())?;
    let explicit = args.tag.is_some();
    let target_word = if explicit { "requested" } else { "latest" };

    if let Some(i) = &installed
        && i.version != cli_version
    {
        println!(
            "note: the {} package is {} but this avdsp is {cli_version}",
            i.pm.name(),
            i.version
        );
    }
    let action = decide(current, &release.version, explicit);
    match action {
        None => println!(
            "Audivane DSP {current} is up to date ({target_word} release: {}).",
            release.version
        ),
        Some(_) => println!("installed {current} → {target_word} {}", release.version),
    }
    if !release.notes_url.is_empty() {
        println!("release notes: {}", release.notes_url);
    }

    if args.check {
        if installed.is_none() && from_appimage(&exe) {
            println!("note: {APPIMAGE_HINT}");
        } else if installed.is_none() {
            println!(
                "note: this avdsp ({}) is not from a package, so `avdsp upgrade` cannot update it",
                exe.display()
            );
        }
        return Ok(check_exit_code(current, &release.version));
    }
    let Some(action) = action else {
        return Ok(0);
    };
    if installed.is_none() && from_appimage(&exe) {
        bail!("{APPIMAGE_HINT}");
    }
    let Some(installed) = installed else {
        bail!(
            "this avdsp ({}) was not installed from a package (a source build?), so \
             `avdsp upgrade` cannot update it. Rebuild it from source, or install a \
             package from https://github.com/audivane/dsp/releases",
            exe.display()
        );
    };
    if std::env::consts::ARCH != "x86_64" {
        bail!("release packages are built for x86_64 only");
    }
    let asset = select_asset(&release.assets, installed.pm, release.version.as_str())
        .with_context(|| {
            format!(
                "release {} has no {} package for x86_64",
                release.tag,
                installed.pm.name()
            )
        })?;
    let sums = release
        .assets
        .iter()
        .find(|a| a.name == "SHA256SUMS")
        .with_context(|| format!("release {} has no SHA256SUMS", release.tag))?;

    let install = installed.pm.install_command(action, Path::new(&asset.name));
    let tool = install[0].to_string_lossy().into_owned();
    if !have(&tool) {
        bail!("`{tool}` is not installed; cannot install {}", asset.name);
    }
    let use_sudo = !as_root;
    if use_sudo && !have("sudo") {
        bail!("`avdsp upgrade` needs sudo for the package step");
    }

    if !args.yes {
        let verb = match action {
            Action::Upgrade => format!("Upgrade to {}", release.version),
            Action::Reinstall => format!("Reinstall {}", release.version),
            Action::Downgrade => format!("Downgrade to {}", release.version),
        };
        let sudo = if use_sudo { " (uses sudo)" } else { "" };
        if !confirm(&format!("{verb} with {}{sudo}?", installed.pm.name()))? {
            println!("Nothing changed.");
            return Ok(0);
        }
    }

    // ---- download and verify
    let dir = fresh_download_dir(&release.tag)?;
    let progress = std::io::stderr().is_terminal();
    let sums_path = dir.join("SHA256SUMS");
    let pkg_path = dir.join(&asset.name);
    let downloaded = (|| -> Result<()> {
        fetcher.download(&sums.url, &sums_path, false)?;
        println!("downloading {}", asset.name);
        fetcher.download(&asset.url, &pkg_path, progress)?;
        let sums_text = std::fs::read_to_string(&sums_path).context("cannot read SHA256SUMS")?;
        let want = expected_sha256(&sums_text, &asset.name)
            .with_context(|| format!("SHA256SUMS does not list {}", asset.name))?;
        let got = sha256_file(&pkg_path)?;
        if got != want {
            bail!(
                "SHA-256 mismatch for {}: expected {want}, got {got}; nothing was installed",
                asset.name
            );
        }
        println!("SHA-256 ok ({})", &got[..16]);
        Ok(())
    })();
    if let Err(err) = downloaded {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(err);
    }

    // ---- install
    let was_active = service_active();
    let install = installed.pm.install_command(action, &pkg_path);
    let mut cmd = if use_sudo {
        let mut c = Command::new("sudo");
        c.args(&install);
        c
    } else {
        let mut c = Command::new(&install[0]);
        c.args(&install[1..]);
        c
    };
    println!(
        "running: {}{}",
        if use_sudo { "sudo " } else { "" },
        install
            .iter()
            .map(|a| a.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ")
    );
    let status = cmd.status().with_context(|| format!("cannot run {tool}"))?;
    if !status.success() {
        bail!(
            "{tool} failed ({status}); the downloaded package is kept in {}",
            dir.display()
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
    if let Some(now) = detect_installed(&exe)?
        && now.version != release.version
    {
        bail!(
            "{tool} finished but the installed package is {}, not {}",
            now.version,
            release.version
        );
    }

    // ---- engine
    let code = restart_engine(was_active, &release.version);

    if ui_running() {
        println!(
            "The Audivane DSP app restarts itself into the new version in a few \
             seconds (if it cannot, it offers a \"Restart\" in its window and tray)."
        );
    }
    Ok(code)
}

/// Reload the user manager, restart the engine if it ran, and confirm its
/// version. Returns the exit code.
fn restart_engine(was_active: Option<bool>, target: &Version) -> i32 {
    let Some(was_active) = was_active else {
        println!(
            "No systemd user manager here; start the engine with \
             `systemctl --user enable --now audivane-dsp` in your session."
        );
        return 0;
    };
    match systemctl_user(&["daemon-reload"]) {
        Some(out) if out.status.success() => {}
        _ => eprintln!("warning: `systemctl --user daemon-reload` failed"),
    }
    if !was_active {
        if let Some(v) = crate::ctl::engine_version(Duration::from_millis(800)) {
            println!(
                "An engine started by hand is still running {v}; stop it and start \
                 the service: systemctl --user enable --now audivane-dsp"
            );
        } else {
            println!(
                "The engine service was not running, so it was left stopped. Start it with:\n    \
                 systemctl --user enable --now audivane-dsp"
            );
        }
        return 0;
    }
    // The package's own scriptlet may already have restarted it.
    let already = crate::ctl::engine_version(Duration::from_millis(800))
        .and_then(|v| Version::parse(&v))
        .is_some_and(|v| &v == target);
    if already {
        println!("The engine was already restarted by the package.");
    } else {
        println!("restarting the engine (systemctl --user restart audivane-dsp)");
        match systemctl_user(&["restart", UNIT]) {
            Some(out) if out.status.success() => {}
            Some(out) => {
                eprintln!(
                    "error: restarting the engine failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
                return 1;
            }
            None => {
                eprintln!("error: cannot run systemctl");
                return 1;
            }
        }
    }
    match wait_for_engine(ENGINE_WAIT) {
        Some(v) if Version::parse(&v).is_some_and(|v| &v == target) => {
            println!("The engine is running {v}.");
            0
        }
        Some(v) => {
            eprintln!(
                "error: the engine answers but reports {v}, not {target}; check \
                 `systemctl --user status audivane-dsp`"
            );
            1
        }
        None => {
            eprintln!(
                "error: the engine did not answer within {} s; check \
                 `systemctl --user status audivane-dsp` and `journalctl --user -u audivane-dsp`",
                ENGINE_WAIT.as_secs()
            );
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap_or_else(|| panic!("{s} should parse"))
    }

    #[test]
    fn versions_compare_like_semver() {
        assert!(v("0.9.2") > v("0.9.1"));
        assert!(v("0.10.0") > v("0.9.9"));
        assert!(v("1.0.0") > v("0.99.99"));
        assert_eq!(v("0.9.1"), v("v0.9.1"));
        assert_eq!(v("0.9"), v("0.9.0"));
        assert_eq!(v("0.9.1+build.5"), v("0.9.1"));
        assert!(v("0.10.0-rc.1") < v("0.10.0"));
        assert!(v("0.10.0-rc.1") > v("0.9.9"));
        assert!(v("0.10.0-rc.2") > v("0.10.0-rc.1"));
        assert!(v("0.10.0-rc.10") > v("0.10.0-rc.9"));
        assert!(v("0.10.0-beta") < v("0.10.0-rc"));
        assert!(v("0.10.0-rc") < v("0.10.0-rc.1"));
        assert_eq!(v("0.10.0~rc.1"), v("0.10.0-rc.1"));
        assert_eq!(v("v0.9.1").to_string(), "0.9.1");
        for bad in ["", "v", "latest", "0.9.x", "0..1", "1.2.3.4.5", "0.9.1-"] {
            assert!(Version::parse(bad).is_none(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn decide_what_to_install() {
        assert_eq!(
            decide(&v("0.9.1"), &v("0.9.2"), false),
            Some(Action::Upgrade)
        );
        assert_eq!(decide(&v("0.9.1"), &v("0.9.1"), false), None);
        assert_eq!(decide(&v("0.9.2"), &v("0.9.1"), false), None);
        assert_eq!(
            decide(&v("0.9.1"), &v("0.9.1"), true),
            Some(Action::Reinstall)
        );
        assert_eq!(
            decide(&v("0.9.2"), &v("0.9.1"), true),
            Some(Action::Downgrade)
        );
        assert_eq!(
            decide(&v("0.9.1"), &v("0.9.2"), true),
            Some(Action::Upgrade)
        );
    }

    #[test]
    fn install_commands_use_the_local_file() {
        let f = Path::new("/home/u/.cache/audivane-dsp/upgrade/x/audivane-dsp_0.9.2-1_amd64.deb");
        let s = |pm: PackageManager, a| {
            pm.install_command(a, f)
                .iter()
                .map(|x| x.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(" ")
        };
        let path = f.display();
        assert_eq!(
            s(PackageManager::Pacman, Action::Upgrade),
            format!("pacman -U --noconfirm {path}")
        );
        assert_eq!(
            s(PackageManager::Dpkg, Action::Upgrade),
            format!("apt install -y {path}")
        );
        assert_eq!(
            s(PackageManager::Dpkg, Action::Downgrade),
            format!("apt install -y --allow-downgrades {path}")
        );
        assert_eq!(
            s(PackageManager::Dpkg, Action::Reinstall),
            format!("apt install -y --reinstall {path}")
        );
        assert_eq!(
            s(PackageManager::Rpm, Action::Upgrade),
            format!("dnf install -y {path}")
        );
        assert_eq!(
            s(PackageManager::Rpm, Action::Downgrade),
            format!("dnf downgrade -y {path}")
        );
    }

    #[test]
    fn sha256sums_parsing() {
        let a = "fba00d2c7cd1cda49c352ffefa9698db62d1b5c65677edde6cf246505ce1034a";
        let b = "26158EE5373B0EA320E389AD6F5CDCDF75FBD1B2A7FAED4F30B8A85153BF43CC";
        let text = format!(
            "{b}  audivane-dsp_0.9.1-1_amd64.deb\n{a} *audivane-dsp-0.9.1-1-x86_64.pkg.tar.zst\n\
             garbage line\nabc  short-hash\n"
        );
        let parsed = parse_sha256sums(&text);
        assert_eq!(parsed.len(), 2);
        assert_eq!(
            expected_sha256(&text, "audivane-dsp_0.9.1-1_amd64.deb").as_deref(),
            Some(b.to_ascii_lowercase().as_str())
        );
        assert_eq!(
            expected_sha256(&text, "audivane-dsp-0.9.1-1-x86_64.pkg.tar.zst").as_deref(),
            Some(a)
        );
        assert_eq!(expected_sha256(&text, "short-hash"), None);
        assert_eq!(expected_sha256(&text, "missing.rpm"), None);
    }
}
