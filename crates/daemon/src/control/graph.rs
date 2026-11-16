//! PipeWire graph watcher and router.
//!
//! Runs a private PipeWire main loop on its own thread and keeps a small,
//! lock-protected snapshot of what matters to the control plane:
//!
//! - the `Stream/Output/Audio` nodes (app name, binary, media name, negotiated
//!   format/rate/channels, software volume/mute),
//! - the sinks our two driver nodes are linked to (`node.name`
//!   `audivane_dsp` and `audivane_direct`),
//! - the session default sink from the `default` metadata.
//!
//! Routing:
//!
//! - [`RoutingMode::System`] makes "Audivane DSP" the default sink by writing
//!   `default.audio.sink` and `default.configured.audio.sink` in the `default`
//!   metadata (WirePlumber honours the latter). The previous default is
//!   remembered and restored when system mode is left or the daemon exits.
//! - [`RoutingMode::Apps`] moves individual streams by writing
//!   `target.object` (and `target.node`) on the stream's metadata subject.
//!   `SinkChoice::Default` clears both keys.
//! - App rules are applied to streams as they appear, and when they change.

use audivane_protocol::ipc::{AppRule, SinkChoice};
use pipewire as pw;
use pw::metadata::Metadata;
use pw::node::Node;
use pw::proxy::ProxyT;
use pw::registry::GlobalObject;
use pw::spa;
use pw::spa::utils::dict::DictRef as SpaDictRef;
use serde::{Deserialize, Serialize};
use spa::param::ParamType;
use spa::param::audio::AudioInfoRaw;
use spa::pod::Pod;
use spa::utils::dict::DictRef;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub const DSP_SINK_NAME: &str = "audivane_dsp";
pub const DIRECT_SINK_NAME: &str = "audivane_direct";

/// How often the supervisor re-asserts the system default.
const DEFAULT_ASSERT_INTERVAL: Duration = Duration::from_millis(500);

/// Supervisor ticks to wait for the `default` metadata contents before
/// treating "nothing observed" as "no keys set" (only when the metadata global
/// appeared after start-up; normally the start-up barrier already knows).
const METADATA_SETTLE_TICKS: u32 = 3;

/// Bounded wait during `Command::Quit` for PipeWire `core.sync` to confirm
/// default-sink restoration before disconnecting.
const RESTORE_SYNC_TIMEOUT: Duration = Duration::from_secs(2);

/// Tracks the default-sink restore journal across async PipeWire `core.sync`
/// acknowledgements so the on-disk file is never removed before PipeWire
/// confirms the restore writes, and is preserved on disconnect/error/timeout.
#[derive(Debug)]
struct RestoreJournalTracker<S> {
    journal_gen: u64,
    pending_syncs: Vec<(S, u64, Option<PathBuf>)>,
    quit_seq: Option<S>,
    disconnected: bool,
}

impl<S> Default for RestoreJournalTracker<S> {
    fn default() -> Self {
        Self {
            journal_gen: 0,
            pending_syncs: Vec::new(),
            quit_seq: None,
            disconnected: false,
        }
    }
}

impl<S: Copy + Eq> RestoreJournalTracker<S> {
    fn note_snapshot_saved(&mut self) -> u64 {
        self.journal_gen = self.journal_gen.saturating_add(1);
        self.journal_gen
    }

    fn finish_restore<F>(&mut self, path: Option<&Path>, touched: bool, sync_fn: F)
    where
        F: FnOnce() -> Result<S, ()>,
    {
        if !touched {
            if let Some(p) = path
                && !self
                    .pending_syncs
                    .iter()
                    .any(|(_, sync_gen, _)| *sync_gen == self.journal_gen)
            {
                remove_snapshot(p);
            }
            return;
        }
        if self.disconnected {
            // Preserve journal on disk when PipeWire is disconnected/errored.
            return;
        }
        match sync_fn() {
            Ok(seq) => {
                self.pending_syncs
                    .push((seq, self.journal_gen, path.map(Path::to_path_buf)));
            }
            Err(()) => {
                // PipeWire rejected core.sync; preserve the journal on disk so
                // the next daemon start recovers it.
                if let Some(p) = path {
                    log::warn!(
                        "core.sync failed after requesting default-sink restore; keeping {}",
                        p.display()
                    );
                }
            }
        }
    }

    /// Handle a `PW_ID_CORE` `done` event. Deletes the journal only if its
    /// generation still matches `self.journal_gen` and no disconnect occurred.
    /// Returns `true` when `Command::Quit` was waiting on this `seq`.
    fn on_sync_done(&mut self, seq: S) -> bool {
        let mut remaining = Vec::with_capacity(self.pending_syncs.len());
        for (pending_seq, sync_gen, path_opt) in self.pending_syncs.drain(..) {
            if pending_seq == seq {
                if !self.disconnected
                    && sync_gen == self.journal_gen
                    && let Some(path) = path_opt
                {
                    remove_snapshot(&path);
                }
            } else {
                remaining.push((pending_seq, sync_gen, path_opt));
            }
        }
        self.pending_syncs = remaining;

        if self.quit_seq == Some(seq) {
            self.quit_seq = None;
            true
        } else {
            false
        }
    }

    fn on_core_error(&mut self) {
        self.disconnected = true;
        self.quit_seq = None;
        // Preserve any un-acknowledged restore journals on disk.
    }

    /// Begin waiting for the latest pending restore sync during `Quit`.
    /// Returns `true` if `main_loop` should remain running until `done` or timeout.
    fn begin_quit_wait(&mut self) -> bool {
        if self.disconnected {
            return false;
        }
        if let Some((seq, _, _)) = self.pending_syncs.last() {
            self.quit_seq = Some(*seq);
            true
        } else {
            false
        }
    }

    fn is_waiting_for_quit(&self) -> bool {
        self.quit_seq.is_some() && !self.disconnected
    }

    /// Called when the bounded `Quit` fallback timer expires before `core.done`.
    /// Preserves the un-acknowledged journal on disk.
    fn on_quit_timeout(&mut self) -> bool {
        if self.quit_seq.take().is_some() {
            log::warn!(
                "timed out waiting for PipeWire sync after default-sink restore; \
                 preserving restore journal"
            );
            true
        } else {
            false
        }
    }
}

/// Crash-safety file for the session default (see [`Graph::enter_system_default`]).
pub fn default_restore_path() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR").map(|dir| {
        PathBuf::from(dir)
            .join("audivane-dsp")
            .join("default-sink-restore.json")
    })
}

// ---------------------------------------------------------------- facts

/// One `Stream/Output/Audio` node.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StreamFact {
    pub node: u32,
    pub app_name: Option<String>,
    pub binary: Option<String>,
    pub media_name: Option<String>,
    pub rate: Option<u32>,
    pub channels: Option<u32>,
    /// Negotiated format name, e.g. `S32LE`.
    pub format: Option<String>,
    pub volume: Option<f64>,
    pub muted: bool,
    /// One of our sinks, when the stream is linked to one.
    pub sink: Option<SinkChoice>,
    pub sink_node: Option<u32>,
    /// True while PipeWire reports the node as running.
    pub running: bool,
}

/// One of our two driver sinks, when registered.
#[derive(Clone, Debug, PartialEq)]
pub struct SinkFact {
    pub choice: SinkChoice,
    pub node: u32,
    /// `object.serial`, which is what `target.object` accepts.
    pub serial: Option<u64>,
    pub name: String,
    pub channels: Option<u32>,
    pub volume: Option<f64>,
    pub muted: bool,
}

/// Everything the control plane needs from the graph, as one comparable value.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Facts {
    pub streams: Vec<StreamFact>,
    pub sinks: Vec<SinkFact>,
    /// `default.audio.sink`, the effective default, when the metadata has one.
    pub default_sink: Option<String>,
    /// `default.configured.audio.sink`, the user's preference, when set. It
    /// can differ from `default_sink` (e.g. a preferred device is unplugged).
    pub default_configured_sink: Option<String>,
    /// `node.description` of the effective default sink when it is not one of
    /// ours (the output an unrouted stream plays on in per-app mode). `None`
    /// when the default is ours, unset, or names a sink that does not exist.
    pub normal_output: Option<String>,
}

impl Facts {
    pub fn sink(&self, choice: SinkChoice) -> Option<&SinkFact> {
        self.sinks.iter().find(|s| s.choice == choice)
    }

    /// Streams linked to one of our sinks.
    pub fn streams_on(&self, choice: SinkChoice) -> Vec<&StreamFact> {
        self.streams
            .iter()
            .filter(|s| s.sink == Some(choice))
            .collect()
    }
}

#[derive(Clone, Debug)]
pub struct GraphOptions {
    pub dsp_sink_name: String,
    pub direct_sink_name: String,
    /// App rules to apply to newly appearing streams.
    pub rules: Vec<AppRule>,
    /// Start in system-wide mode (make our sink the default).
    pub system_default: bool,
    /// Where the pre-takeover session default is persisted so a crash can be
    /// repaired on the next start. `None` disables crash safety (tests that
    /// never touch the default).
    pub restore_path: Option<PathBuf>,
}

impl Default for GraphOptions {
    fn default() -> Self {
        Self {
            dsp_sink_name: DSP_SINK_NAME.to_string(),
            direct_sink_name: DIRECT_SINK_NAME.to_string(),
            rules: Vec::new(),
            system_default: true,
            restore_path: default_restore_path(),
        }
    }
}

// ---------------------------------------------------------------- default sink

/// The session default sink, as it was before this daemon took it over.
///
/// Both `default` metadata keys are kept independently: WirePlumber's
/// effective default (`default.audio.sink`) is *not* the user's configured
/// preference (`default.configured.audio.sink`) — a preferred device that is
/// currently unavailable makes them differ — and restoring one over the other
/// destroys that preference. Raw JSON values are stored so an absent key
/// (`None`) is restored as absent, not as an empty default.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct DefaultSnapshot {
    /// Sink names this daemon wrote. A key is only restored while it still
    /// points at one of them; a user's later choice is never overwritten.
    written: Vec<String>,
    /// Raw `default.audio.sink` value, `None` when the key was absent.
    effective: Option<String>,
    /// Raw `default.configured.audio.sink` value, `None` when absent.
    configured: Option<String>,
}

const EFFECTIVE_KEY: &str = "default.audio.sink";
const CONFIGURED_KEY: &str = "default.configured.audio.sink";

/// Atomically persist `snap` (temp file in the same directory, fsync, rename).
fn save_snapshot(path: &Path, snap: &DefaultSnapshot) -> std::io::Result<()> {
    let json = serde_json::to_vec_pretty(snap)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "default-sink-restore.json".into());
    let tmp = path.with_file_name(format!(".{name}.tmp.{}", std::process::id()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(&json)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    drop(file);
    match fs::rename(&tmp, path) {
        Ok(()) => {}
        Err(err) => {
            let _ = fs::remove_file(&tmp);
            return Err(err);
        }
    }
    if let Some(parent) = path.parent()
        && let Ok(dir) = fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }
    Ok(())
}

fn load_snapshot(path: &Path) -> Option<DefaultSnapshot> {
    let text = fs::read_to_string(path).ok()?;
    match serde_json::from_str::<DefaultSnapshot>(&text) {
        Ok(snapshot) => Some(snapshot),
        Err(err) => {
            log::warn!("ignoring unreadable {}: {err}", path.display());
            None
        }
    }
}

fn remove_snapshot(path: &Path) {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => log::warn!("cannot remove {}: {err}", path.display()),
    }
}

// ---------------------------------------------------------------- handle

struct Shared {
    facts: Mutex<Facts>,
    generation: AtomicU64,
    want_default: AtomicBool,
    default_name: Mutex<String>,
    /// The `default` metadata global is bound and writable.
    meta_ready: AtomicBool,
    quit: AtomicBool,
}

/// Handle to the watcher thread. All methods are cheap and non-blocking.
pub struct GraphWatcher {
    shared: Arc<Shared>,
    tx: pw::channel::Sender<Command>,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

impl GraphWatcher {
    /// Connect to PipeWire and start watching + routing.
    ///
    /// Returns an error when there is no usable PipeWire session; callers
    /// normally log that and run with an empty graph.
    pub fn spawn(opts: GraphOptions) -> anyhow::Result<Arc<Self>> {
        let (tx, rx) = pw::channel::channel();
        let shared = Arc::new(Shared {
            facts: Mutex::new(Facts::default()),
            generation: AtomicU64::new(0),
            want_default: AtomicBool::new(opts.system_default),
            default_name: Mutex::new(opts.dsp_sink_name.clone()),
            meta_ready: AtomicBool::new(false),
            quit: AtomicBool::new(false),
        });
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();

        let thread_shared = Arc::clone(&shared);
        let thread_opts = opts.clone();
        let handle = thread::Builder::new()
            .name("avdsp-graph".into())
            .spawn(move || run_loop(thread_shared, thread_opts, rx, ready_tx))?;

        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                let _ = handle.join();
                anyhow::bail!("cannot connect to PipeWire: {err}");
            }
            Err(_) => {
                anyhow::bail!("timed out connecting to PipeWire");
            }
        }

        // Supervisor: keeps the system default pointed at our sink while
        // system mode is active (WirePlumber may re-evaluate it when devices
        // come and go). Doing this from its own thread keeps the metadata
        // write out of PipeWire property callbacks.
        let sup_shared = Arc::clone(&shared);
        let sup_tx = tx.clone();
        let supervisor = thread::Builder::new()
            .name("avdsp-graph-sup".into())
            .spawn(move || {
                while !sup_shared.quit.load(Ordering::Relaxed) {
                    thread::sleep(DEFAULT_ASSERT_INTERVAL);
                    if sup_shared.quit.load(Ordering::Relaxed) {
                        break;
                    }
                    if !sup_shared.want_default.load(Ordering::Relaxed) {
                        continue;
                    }
                    let name = sup_shared
                        .default_name
                        .lock()
                        .map(|n| n.clone())
                        .unwrap_or_default();
                    let stale = sup_shared
                        .facts
                        .lock()
                        .map(|f| {
                            // Also when our sink vanished while still the
                            // default: the handler then hands it back.
                            f.default_sink.as_deref() != Some(name.as_str())
                                || f.sink(SinkChoice::Dsp).is_none()
                        })
                        .unwrap_or(false);
                    if stale {
                        let _ = sup_tx.send(Command::AssertSystemDefault);
                    }
                }
            })?;

        Ok(Arc::new(Self {
            shared,
            tx,
            threads: Mutex::new(vec![handle, supervisor]),
        }))
    }

    pub fn facts(&self) -> Facts {
        self.shared
            .facts
            .lock()
            .map(|f| f.clone())
            .unwrap_or_default()
    }

    /// Bumped whenever the facts change.
    pub fn generation(&self) -> u64 {
        self.shared.generation.load(Ordering::Relaxed)
    }

    /// False once the PipeWire loop has stopped (commands are dropped then).
    pub fn is_running(&self) -> bool {
        !self.shared.quit.load(Ordering::Relaxed)
    }

    /// True while the `default` metadata is bound, i.e. requests that write
    /// per-stream routing keys are actually delivered.
    pub fn has_metadata(&self) -> bool {
        self.shared.meta_ready.load(Ordering::Relaxed)
    }

    /// Enter or leave system-wide routing. Entering remembers the current
    /// default and points `default.audio.sink` at our DSP sink; leaving
    /// restores (or clears) it.
    pub fn set_system_default(&self, enabled: bool) {
        self.shared.want_default.store(enabled, Ordering::Relaxed);
        self.send(Command::SetSystemDefault(enabled));
    }

    /// Replace the app rules; matching streams are moved immediately.
    pub fn set_rules(&self, rules: Vec<AppRule>) {
        self.send(Command::SetRules(rules));
    }

    /// Route one stream to one of our sinks (`Default` unroutes it).
    pub fn route_node(&self, node: u32, sink: SinkChoice) {
        self.send(Command::RouteNode { node, sink });
    }

    /// Queue a command; a stopped watcher must not look like success in the
    /// log.
    fn send(&self, command: Command) {
        if self.tx.send(command).is_err() {
            log::warn!(
                "the PipeWire graph watcher has stopped; the routing change was not applied"
            );
        }
    }

    /// Stop the loop, restoring the previous default sink first.
    pub fn shutdown(&self) {
        self.stop(Command::Quit);
    }

    /// Test hook: stop the loop *without* restoring the session default, so
    /// what remains behind is exactly what `kill -9` would leave (the default
    /// still pointing at our sink and the restore file still on disk).
    #[doc(hidden)]
    pub fn stop_without_restore(&self) {
        self.stop(Command::QuitAbandon);
    }

    fn stop(&self, command: Command) {
        if self.shared.quit.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = self.tx.send(command);
        let threads = self
            .threads
            .lock()
            .map(|mut t| std::mem::take(&mut *t))
            .unwrap_or_default();
        for thread in threads {
            let _ = thread.join();
        }
    }
}

impl Drop for GraphWatcher {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ---------------------------------------------------------------- commands

enum Command {
    SetSystemDefault(bool),
    AssertSystemDefault,
    SetRules(Vec<AppRule>),
    RouteNode {
        node: u32,
        sink: SinkChoice,
    },
    Quit,
    /// Stop without restoring the default (test hook / crash simulation).
    QuitAbandon,
}

// ---------------------------------------------------------------- pw thread

/// Per-node state cached from PipeWire events.
#[derive(Default)]
struct Tracked {
    node: Option<Node>,
    listener: Option<pw::node::NodeListener>,
    choice: Option<SinkChoice>,
    node_name: Option<String>,
    media_class: Option<String>,
    app_name: Option<String>,
    binary: Option<String>,
    media_name: Option<String>,
    rate: Option<u32>,
    channels: Option<u32>,
    format: Option<String>,
    volume: Option<f64>,
    muted: bool,
    serial: Option<u64>,
    running: bool,
}

impl Tracked {
    fn is_stream(&self) -> bool {
        self.media_class.as_deref() == Some("Stream/Output/Audio")
    }
}

struct Graph {
    opts: GraphOptions,
    shared: Arc<Shared>,
    core: Option<pw::core::CoreRc>,
    nodes: HashMap<u32, Tracked>,
    /// link id -> (output node, input node)
    links: BTreeMap<u32, (u32, u32)>,
    /// Other `Audio/Sink` nodes: id -> (`node.name`, `node.description`).
    /// Only read from the registry globals; never bound.
    other_sinks: HashMap<u32, (String, Option<String>)>,
    meta: Option<Metadata>,
    meta_listener: Option<pw::metadata::MetadataListener>,
    /// Parsed `default.audio.sink`.
    default_sink: Option<String>,
    /// Raw `default.audio.sink` value, `None` when the key is absent.
    default_sink_raw: Option<String>,
    /// Parsed `default.configured.audio.sink`.
    configured_sink: Option<String>,
    /// Raw `default.configured.audio.sink` value.
    configured_sink_raw: Option<String>,
    /// True once the contents of the `default` metadata are known (so
    /// "nothing observed" can be trusted as "no keys set").
    defaults_known: bool,
    /// Supervisor ticks spent waiting for [`Self::defaults_known`].
    settle_ticks: u32,
    rules: Vec<AppRule>,
    system_default: bool,
    /// What the default was before we took it over; kept until it is restored.
    prev_defaults: Option<DefaultSnapshot>,
    /// A restore file left behind by an earlier run, applied once the
    /// metadata contents are known.
    pending_recovery: Option<(PathBuf, DefaultSnapshot)>,
    /// Tracks async PipeWire `core.sync` barriers protecting the restore file.
    journal: RestoreJournalTracker<spa::utils::result::AsyncSeq>,
    /// Streams an app rule moved (so the rule can be undone).
    rule_routed: HashSet<u32>,
    /// Streams routed explicitly by the user (rules must not override them).
    routed: HashSet<u32>,
    published: Facts,
}

impl Graph {
    fn own_sink_name(&self, choice: SinkChoice) -> Option<&str> {
        match choice {
            SinkChoice::Dsp => Some(&self.opts.dsp_sink_name),
            SinkChoice::Direct => Some(&self.opts.direct_sink_name),
            SinkChoice::Default => None,
        }
    }

    /// Find one of our sink nodes by choice.
    fn own_sink(&self, choice: SinkChoice) -> Option<(u32, Option<u64>)> {
        let name = self.own_sink_name(choice)?;
        self.nodes.iter().find_map(|(id, node)| {
            (node.choice == Some(choice) && node.node_name.as_deref() == Some(name))
                .then_some((*id, node.serial))
        })
    }

    fn build_facts(&self) -> Facts {
        let mut sinks: Vec<SinkFact> = self
            .nodes
            .iter()
            .filter_map(|(id, node)| {
                let choice = node.choice?;
                Some(SinkFact {
                    choice,
                    node: *id,
                    serial: node.serial,
                    name: node.node_name.clone().unwrap_or_default(),
                    channels: node.channels,
                    volume: node.volume,
                    muted: node.muted,
                })
            })
            .collect();
        sinks.sort_by_key(|s| s.node);

        let mut streams: Vec<StreamFact> = self
            .nodes
            .iter()
            .filter(|(_, node)| node.is_stream())
            .map(|(id, node)| {
                let linked = self
                    .links
                    .values()
                    .find(|(out, input)| out == id && self.sink_choice(*input).is_some());
                let sink_node = linked.map(|(_, input)| *input);
                StreamFact {
                    node: *id,
                    app_name: node.app_name.clone(),
                    binary: node.binary.clone(),
                    media_name: node.media_name.clone(),
                    rate: node.rate,
                    channels: node.channels,
                    format: node.format.clone(),
                    volume: node.volume,
                    muted: node.muted,
                    sink: sink_node.and_then(|n| self.sink_choice(n)),
                    sink_node,
                    running: node.running,
                }
            })
            .collect();
        streams.sort_by_key(|s| s.node);

        Facts {
            streams,
            sinks,
            default_sink: self.default_sink.clone(),
            default_configured_sink: self.configured_sink.clone(),
            normal_output: self.normal_output(),
        }
    }

    /// Description of the default sink when it is someone else's.
    fn normal_output(&self) -> Option<String> {
        let name = self.default_sink.as_deref()?;
        if name == self.opts.dsp_sink_name || name == self.opts.direct_sink_name {
            return None;
        }
        self.other_sinks
            .values()
            .find(|(sink, _)| sink == name)
            .map(|(sink, description)| description.clone().unwrap_or_else(|| sink.clone()))
    }

    fn sink_choice(&self, node: u32) -> Option<SinkChoice> {
        self.nodes.get(&node).and_then(|n| n.choice)
    }

    fn publish(&mut self) {
        let facts = self.build_facts();
        if facts != self.published {
            self.published = facts.clone();
            if let Ok(mut shared) = self.shared.facts.lock() {
                *shared = facts.clone();
            }
            self.shared.generation.fetch_add(1, Ordering::Relaxed);
        }
        self.apply_rules(&facts);
    }

    fn apply_rules(&mut self, facts: &Facts) {
        if self.rules.is_empty() {
            return;
        }
        for stream in &facts.streams {
            if self.routed.contains(&stream.node) {
                continue;
            }
            let rule = self.rules.iter().find(|rule| {
                let want = rule.app.trim();
                [stream.app_name.as_deref(), stream.binary.as_deref()]
                    .into_iter()
                    .flatten()
                    .any(|value| value.eq_ignore_ascii_case(want))
            });
            let Some(rule) = rule else { continue };
            let sink = rule.sink;
            // A rule for a sink that is not registered (yet) must not pin the
            // stream anywhere: retry when the sink appears.
            if sink != SinkChoice::Default && self.own_sink(sink).is_none() {
                continue;
            }
            self.route(stream.node, sink);
            self.rule_routed.insert(stream.node);
        }
    }

    /// Clear the routing keys an app rule wrote, so a rule that is removed (or
    /// no longer matches) lets the stream fall back to its own target. The
    /// `target.*` metadata is written state: leaving it behind would keep the
    /// stream pinned forever.
    fn undo_rules(&mut self) {
        let previous: Vec<u32> = self.rule_routed.drain().collect();
        for node in previous {
            if self.nodes.contains_key(&node) {
                self.route(node, SinkChoice::Default);
            }
        }
    }

    fn route(&mut self, node: u32, sink: SinkChoice) {
        let Some(meta) = &self.meta else {
            log::debug!("cannot route node {node}: default metadata not available");
            return;
        };
        match self.own_sink(sink) {
            Some((sink_node, serial)) => {
                if let Some(serial) = serial {
                    log::debug!(
                        "routing stream {node} to {} (node {sink_node}, serial {serial})",
                        sink_name(sink)
                    );
                    meta.set_property(
                        node,
                        "target.object",
                        Some("Spa:Id"),
                        Some(&serial.to_string()),
                    );
                    meta.set_property(
                        node,
                        "target.node",
                        Some("Spa:Id"),
                        Some(&sink_node.to_string()),
                    );
                } else {
                    // Should not happen: PipeWire always sets object.serial.
                    log::warn!("sink {sink_node} has no object.serial; using target.node only");
                    meta.set_property(
                        node,
                        "target.node",
                        Some("Spa:Id"),
                        Some(&sink_node.to_string()),
                    );
                }
            }
            None => {
                // `Default`, or the requested sink is not registered: let
                // PipeWire pick by clearing our routing keys.
                log::debug!("unrouting stream {node} (target {sink:?})");
                meta.set_property(node, "target.object", None, None);
                meta.set_property(node, "target.node", None, None);
            }
        }
    }

    // -------------------------------------------------------- default sink

    /// Write one `default` key from a raw JSON value (`None` deletes it) and
    /// keep the tracked facts in step without waiting for the echo.
    fn write_raw_default(&mut self, key: &str, raw: Option<String>) {
        let Some(meta) = self.meta.as_ref() else {
            log::warn!("cannot write {key}: the `default` metadata is not available");
            return;
        };
        match &raw {
            Some(value) => meta.set_property(0, key, Some("Spa:String:JSON"), Some(value)),
            None => meta.set_property(0, key, None, None),
        };
        let name = raw.as_deref().and_then(parse_default_name);
        match key {
            EFFECTIVE_KEY => {
                self.default_sink = name;
                self.default_sink_raw = raw;
            }
            CONFIGURED_KEY => {
                self.configured_sink = name;
                self.configured_sink_raw = raw;
            }
            _ => return,
        }
        self.publish();
    }

    /// Point both default keys at `name`, the way system-wide routing wants
    /// WirePlumber to see it.
    fn write_default(&mut self, name: &str) {
        let value = serde_json::json!({ "name": name }).to_string();
        self.write_raw_default(EFFECTIVE_KEY, Some(value.clone()));
        self.write_raw_default(CONFIGURED_KEY, Some(value));
    }

    /// Snapshot both keys exactly as they are now, persist the snapshot for
    /// crash recovery, and then make our sink the default.
    fn enter_system_default(&mut self) {
        self.system_default = true;
        if !self.defaults_known || self.meta.is_none() {
            // The metadata (or its contents) have not arrived yet: writing now
            // could overwrite a value we have not seen. The supervisor retries.
            log::debug!("system-wide routing: waiting for the `default` metadata contents");
            return;
        }
        // A previous run may have crashed after taking the default; put that
        // right before snapshotting again (otherwise the snapshot would be
        // taken from our own sink).
        self.recover_defaults();
        if self.prev_defaults.is_none() {
            let snapshot = DefaultSnapshot {
                written: vec![self.opts.dsp_sink_name.clone()],
                effective: self.default_sink_raw.clone(),
                configured: self.configured_sink_raw.clone(),
            };
            // Advance the journal generation before persisting so any older
            // in-flight restore `core.sync` cannot remove this newer snapshot.
            self.journal.note_snapshot_saved();
            // Persist *before* changing anything: a crash after this point can
            // be repaired on the next start.
            if let Some(path) = &self.opts.restore_path {
                match save_snapshot(path, &snapshot) {
                    Ok(()) => log::debug!("saved the previous default sink to {}", path.display()),
                    Err(err) => log::warn!(
                        "cannot persist the previous default sink to {}: {err} \
                         (a crash will not be repaired)",
                        path.display()
                    ),
                }
            }
            self.prev_defaults = Some(snapshot);
        }
        let name = self.opts.dsp_sink_name.clone();
        log::info!(
            "system-wide routing: default sink -> {name} (was effective {}, preferred {})",
            display_name(self.default_sink.as_deref()),
            display_name(self.configured_sink.as_deref())
        );
        self.write_default(&name);
    }

    /// Restore each key independently, and only while it still points at a
    /// sink this daemon wrote. Keeps any on-disk restore journal until PipeWire
    /// confirms via `core.sync` that the metadata writes were processed.
    fn restore_defaults(&mut self) {
        let Some(snapshot) = self.prev_defaults.clone() else {
            return;
        };
        if self.meta.is_none() {
            // Keep the snapshot (and the file) for the next start instead of
            // pretending the default was restored.
            log::warn!(
                "cannot restore the previous default sink now: the `default` metadata is \
                 not available; it will be restored on the next start"
            );
            return;
        }
        let ours = |name: Option<&str>| {
            name.is_some_and(|name| snapshot.written.iter().any(|written| written == name))
        };
        let mut touched = false;
        if ours(self.default_sink.as_deref()) {
            log::info!(
                "restoring default.audio.sink -> {}",
                display_name(
                    snapshot
                        .effective
                        .as_deref()
                        .and_then(parse_default_name)
                        .as_deref()
                )
            );
            self.write_raw_default(EFFECTIVE_KEY, snapshot.effective.clone());
            touched = true;
        } else {
            log::info!(
                "leaving default.audio.sink alone (now {}, not ours)",
                display_name(self.default_sink.as_deref())
            );
        }
        if ours(self.configured_sink.as_deref()) {
            log::info!(
                "restoring default.configured.audio.sink -> {}",
                display_name(
                    snapshot
                        .configured
                        .as_deref()
                        .and_then(parse_default_name)
                        .as_deref()
                )
            );
            self.write_raw_default(CONFIGURED_KEY, snapshot.configured.clone());
            touched = true;
        } else {
            log::info!(
                "leaving default.configured.audio.sink alone (now {}, not ours)",
                display_name(self.configured_sink.as_deref())
            );
        }
        self.prev_defaults = None;
        let path = self.opts.restore_path.clone();
        let core = self.core.clone();
        self.journal.finish_restore(path.as_deref(), touched, || {
            core.as_ref()
                .ok_or(())
                .and_then(|c| c.sync(0).map_err(|_| ()))
        });
    }

    /// Repair a session default left behind by a previous run that died before
    /// restoring it. Runs once, when the metadata contents are known. Keeps the
    /// journal on disk until PipeWire confirms the restore sync (if writes were
    /// issued).
    fn recover_defaults(&mut self) {
        let Some((path, snapshot)) = self.pending_recovery.clone() else {
            return;
        };
        if !self.defaults_known || self.meta.is_none() {
            return;
        }
        let ours = |name: Option<&str>| {
            name.is_some_and(|name| snapshot.written.iter().any(|written| written == name))
        };
        let mut touched = false;
        if ours(self.default_sink.as_deref()) {
            log::info!("recovering the session default sink left over from an earlier run");
            self.write_raw_default(EFFECTIVE_KEY, snapshot.effective.clone());
            touched = true;
        }
        if ours(self.configured_sink.as_deref()) {
            self.write_raw_default(CONFIGURED_KEY, snapshot.configured.clone());
            touched = true;
        }
        if !touched {
            log::debug!(
                "discarding {}: neither default key points at our sink",
                path.display()
            );
        }
        let core = self.core.clone();
        self.journal.finish_restore(Some(&path), touched, || {
            core.as_ref()
                .ok_or(())
                .and_then(|c| c.sync(0).map_err(|_| ()))
        });
        self.pending_recovery = None;
    }

    /// Whether our DSP sink is in the graph right now.
    fn dsp_sink_present(&self) -> bool {
        self.nodes
            .values()
            .any(|node| node.choice == Some(SinkChoice::Dsp))
    }

    fn set_system_default(&mut self, enabled: bool) {
        if !enabled && !self.system_default && self.prev_defaults.is_none() {
            // Nothing to restore: never clobber the session default.
            return;
        }
        if enabled {
            if self.dsp_sink_present() {
                self.enter_system_default();
            } else {
                // No sink to point at yet (the engine waits for its device):
                // the supervisor takes the default once it appears.
                self.system_default = true;
            }
        } else {
            self.system_default = false;
            self.restore_defaults();
        }
    }

    fn handle(&mut self, command: Command) {
        match command {
            Command::SetSystemDefault(enabled) => self.set_system_default(enabled),
            Command::AssertSystemDefault => {
                if self.system_default {
                    if !self.defaults_known {
                        // The metadata appeared after start-up; give its
                        // contents a few ticks to arrive before believing
                        // "nothing set".
                        self.settle_ticks += 1;
                        if self.settle_ticks < METADATA_SETTLE_TICKS {
                            return;
                        }
                        self.defaults_known = true;
                    }
                    if !self.dsp_sink_present() {
                        // Our sink is gone (its device was unplugged and the
                        // driver went idle): a default pointing at it would
                        // leave the session silent. Hand the default back and
                        // take it again when the sink returns.
                        if self.prev_defaults.is_some() {
                            log::info!(
                                "system-wide routing: {} is gone; handing the default sink back",
                                self.opts.dsp_sink_name
                            );
                            self.restore_defaults();
                        }
                        return;
                    }
                    self.enter_system_default();
                }
            }
            Command::SetRules(rules) => {
                // Undo what the old rules pinned before applying the new ones,
                // otherwise a stream stays on a sink nobody asked for.
                self.undo_rules();
                self.rules = rules;
                self.routed.clear();
                let facts = self.build_facts();
                self.apply_rules(&facts);
            }
            Command::RouteNode { node, sink } => {
                if sink == SinkChoice::Default {
                    // Unrouting hands the stream back to its own default, so
                    // app rules may match it again.
                    self.routed.remove(&node);
                    self.rule_routed.remove(&node);
                } else {
                    self.routed.insert(node);
                    self.rule_routed.remove(&node);
                }
                self.route(node, sink);
            }
            Command::Quit => {}
            Command::QuitAbandon => {}
        }
    }

    // -------------------------------------------------------- node events

    fn on_node_props(&mut self, id: u32, dict: &DictRef) {
        let mut changed = false;
        {
            let Some(node) = self.nodes.get_mut(&id) else {
                return;
            };
            let update = |slot: &mut Option<String>, key: &str, changed: &mut bool| {
                if let Some(value) = dict.get(key)
                    && slot.as_deref() != Some(value)
                {
                    *slot = Some(value.to_string());
                    *changed = true;
                }
            };
            update(&mut node.app_name, "application.name", &mut changed);
            update(&mut node.binary, "application.process.binary", &mut changed);
            update(&mut node.media_name, "media.name", &mut changed);
            update(&mut node.media_class, "media.class", &mut changed);
            update(&mut node.node_name, "node.name", &mut changed);
            if let Some(serial) = dict.get("object.serial").and_then(|v| v.parse().ok())
                && node.serial != Some(serial)
            {
                node.serial = Some(serial);
                changed = true;
            }
            if node.rate.is_none()
                && let Some(rate) = dict.get("audio.rate").and_then(|v| v.parse().ok())
            {
                node.rate = Some(rate);
                changed = true;
            }
            if node.channels.is_none()
                && let Some(channels) = dict.get("audio.channels").and_then(|v| v.parse().ok())
            {
                node.channels = Some(channels);
                changed = true;
            }
        }
        if changed {
            self.refresh_role(id);
            self.publish();
        }
    }

    /// After properties change, work out whether this node is one of our sinks.
    fn refresh_role(&mut self, id: u32) {
        let Some(node) = self.nodes.get(&id) else {
            return;
        };
        let is_sink = node.media_class.as_deref() == Some("Audio/Sink");
        let choice = if is_sink {
            match node.node_name.as_deref() {
                Some(name) if name == self.opts.dsp_sink_name => Some(SinkChoice::Dsp),
                Some(name) if name == self.opts.direct_sink_name => Some(SinkChoice::Direct),
                _ => None,
            }
        } else {
            None
        };
        if let Some(node) = self.nodes.get_mut(&id) {
            node.choice = choice;
        }
    }

    fn on_param(&mut self, id: u32, param: ParamType, pod: &Pod) {
        log::debug!("node {id} param {param:?} ({} bytes)", pod.as_bytes().len());
        if param == ParamType::Format {
            let mut info = AudioInfoRaw::new();
            if let Err(err) = info.parse(pod) {
                log::debug!("node {id}: cannot parse Format param: {err:?}");
                return;
            }
            let rate = (info.rate() > 0).then_some(info.rate());
            let channels = (info.channels() > 0).then_some(info.channels());
            let format = format_name(info.format());
            log::debug!(
                "node {id} format: {} Hz, {} ch, {:?}",
                info.rate(),
                info.channels(),
                format
            );
            let Some(node) = self.nodes.get_mut(&id) else {
                return;
            };
            let changed = node.rate != rate || node.channels != channels || node.format != format;
            if changed {
                node.rate = rate;
                node.channels = channels;
                node.format = format;
            }
            if changed {
                self.publish();
            }
        } else if param == ParamType::Props {
            let Some((volume, muted)) = parse_props(pod) else {
                return;
            };
            let Some(node) = self.nodes.get_mut(&id) else {
                return;
            };
            let changed = node.volume != volume || node.muted != muted;
            if changed {
                node.volume = volume;
                node.muted = muted;
            }
            if changed {
                self.publish();
            }
        }
    }
}

fn sink_name(choice: SinkChoice) -> &'static str {
    match choice {
        SinkChoice::Dsp => DSP_SINK_NAME,
        SinkChoice::Direct => DIRECT_SINK_NAME,
        SinkChoice::Default => "the default sink",
    }
}

/// `AudioFormat::S32LE` -> `S32LE`.
fn format_name(format: spa::param::audio::AudioFormat) -> Option<String> {
    let debug = format!("{format:?}");
    debug
        .strip_prefix("AudioFormat::")
        .filter(|name| *name != "Unknown" && *name != "Encoded")
        .map(str::to_string)
}

/// Extract the effective volume and mute flag from a `Props` pod.
///
/// `channelVolumes` are linear volumes (1.0 = unity); we report the loudest
/// channel so `volume == 1.0` means "no channel is attenuated". Both `mute`
/// and `softMute` are folded into one flag.
fn parse_props(pod: &Pod) -> Option<(Option<f64>, bool)> {
    let value =
        spa::pod::deserialize::PodDeserializer::deserialize_from::<spa::pod::Value>(pod.as_bytes())
            .ok()?
            .1;
    let spa::pod::Value::Object(object) = value else {
        return None;
    };
    let mut volume = None;
    let mut muted = false;
    for property in object.properties {
        let key = property.key;
        if key == spa::sys::SPA_PROP_channelVolumes {
            if let spa::pod::Value::ValueArray(spa::pod::ValueArray::Float(values)) = property.value
            {
                volume = values
                    .into_iter()
                    .map(f64::from)
                    .reduce(f64::max)
                    .or(volume);
            }
        } else if key == spa::sys::SPA_PROP_volume {
            if let spa::pod::Value::Float(v) = property.value {
                volume = Some(f64::from(v));
            }
        } else if (key == spa::sys::SPA_PROP_mute || key == spa::sys::SPA_PROP_softMute)
            && let spa::pod::Value::Bool(value) = property.value
        {
            muted |= value;
        }
    }
    Some((volume, muted))
}

// ---------------------------------------------------------------- thread

/// Run the loop until the server answers a `sync`; every request sent before
/// this one (proxy binds, property writes) has been answered by then.
fn roundtrip(core: &pw::core::Core, main_loop: &pw::main_loop::MainLoopRc, shared: &Arc<Shared>) {
    let Ok(pending) = core.sync(0) else { return };
    let done = Rc::new(std::cell::Cell::new(false));
    let sync_listener = core
        .add_listener_local()
        .done({
            let done = Rc::clone(&done);
            let main_loop = main_loop.clone();
            move |id, seq| {
                if id == pw::core::PW_ID_CORE && seq == pending {
                    done.set(true);
                    main_loop.quit();
                }
            }
        })
        .register();
    while !done.get() && !shared.quit.load(Ordering::Relaxed) {
        main_loop.run();
    }
    drop(sync_listener);
}

fn run_loop(
    shared: Arc<Shared>,
    opts: GraphOptions,
    receiver: pw::channel::Receiver<Command>,
    ready: mpsc::Sender<Result<(), String>>,
) {
    pw::init();
    let main_loop = match pw::main_loop::MainLoopRc::new(None) {
        Ok(main_loop) => main_loop,
        Err(err) => {
            let _ = ready.send(Err(format!("cannot create main loop: {err}")));
            return;
        }
    };
    let context = match pw::context::ContextRc::new(&main_loop, None) {
        Ok(context) => context,
        Err(err) => {
            let _ = ready.send(Err(format!("cannot create context: {err}")));
            return;
        }
    };
    let core = match context.connect_rc(None) {
        Ok(core) => core,
        Err(err) => {
            let _ = ready.send(Err(format!("cannot connect: {err}")));
            return;
        }
    };
    let registry = match core.get_registry_rc() {
        Ok(registry) => registry,
        Err(err) => {
            let _ = ready.send(Err(format!("cannot get registry: {err}")));
            return;
        }
    };

    let pending_recovery = opts
        .restore_path
        .as_deref()
        .and_then(|path| load_snapshot(path).map(|snapshot| (path.to_path_buf(), snapshot)));
    let mut journal = RestoreJournalTracker::default();
    if pending_recovery.is_some() {
        journal.note_snapshot_saved();
    }
    let graph = Rc::new(RefCell::new(Graph {
        rules: opts.rules.clone(),
        system_default: opts.system_default,
        opts,
        shared: Arc::clone(&shared),
        core: Some(core.clone()),
        nodes: HashMap::new(),
        links: BTreeMap::new(),
        other_sinks: HashMap::new(),
        meta: None,
        meta_listener: None,
        default_sink: None,
        default_sink_raw: None,
        configured_sink: None,
        configured_sink_raw: None,
        defaults_known: false,
        settle_ticks: 0,
        prev_defaults: None,
        pending_recovery,
        journal,
        rule_routed: HashSet::new(),
        routed: HashSet::new(),
        published: Facts::default(),
    }));

    // Registry listener: lives until this function returns (the loop thread
    // ends), so it needs no other owner.
    let registry_listener = registry
        .add_listener_local()
        .global({
            let weak = Rc::downgrade(&graph);
            let registry = registry.clone();
            move |global| {
                if let Some(graph) = weak.upgrade() {
                    on_global(&registry, &graph, global);
                }
            }
        })
        .global_remove({
            let weak = Rc::downgrade(&graph);
            move |id| {
                if let Some(graph) = weak.upgrade() {
                    graph.borrow_mut().on_global_remove(id);
                }
            }
        })
        .register();

    let core_listener = core
        .add_listener_local()
        .done({
            let weak = Rc::downgrade(&graph);
            let main_loop = main_loop.downgrade();
            move |id, seq| {
                if id == pw::core::PW_ID_CORE
                    && let Some(graph) = weak.upgrade()
                    && let Ok(mut g) = graph.try_borrow_mut()
                    && g.journal.on_sync_done(seq)
                    && let Some(main_loop) = main_loop.upgrade()
                {
                    main_loop.quit();
                }
            }
        })
        .error({
            let weak = Rc::downgrade(&graph);
            let main_loop = main_loop.downgrade();
            move |id, seq, res, message| {
                if id == pw::core::PW_ID_CORE {
                    log::error!("PipeWire error id:{id} seq:{seq} res:{res}: {message}");
                    if let Some(graph) = weak.upgrade()
                        && let Ok(mut g) = graph.try_borrow_mut()
                    {
                        g.journal.on_core_error();
                    }
                    if let Some(main_loop) = main_loop.upgrade() {
                        main_loop.quit();
                    }
                } else {
                    // e.g. `EnumFormat` on a node that has no format yet.
                    log::debug!("PipeWire error id:{id} seq:{seq} res:{res}: {message}");
                }
            }
        })
        .register();

    // Two round-trips: the first discovers every global (binding our node and
    // metadata proxies), the second flushes the contents those proxies report
    // on bind. Only then are the session default keys actually known, which
    // the snapshot below depends on.
    roundtrip(&core, &main_loop, &shared);
    roundtrip(&core, &main_loop, &shared);

    // Apply the initial routing mode now that the current default is known.
    {
        let mut graph = graph.borrow_mut();
        graph.publish();
        if graph.meta.is_some() {
            graph.defaults_known = true;
        }
        graph.recover_defaults();
        let system_default = graph.system_default;
        if system_default {
            if graph.dsp_sink_present() {
                graph.enter_system_default();
            }
            // Otherwise the supervisor takes the default once the sink
            // appears (`AssertSystemDefault`).
        } else if !graph.rules.is_empty() {
            let rules = graph.rules.clone();
            graph.handle(Command::SetRules(rules));
        }
    }

    if shared.quit.load(Ordering::Relaxed) {
        let _ = ready.send(Ok(()));
        return;
    }

    let _receiver = receiver.attach(main_loop.loop_(), {
        let main_loop = main_loop.clone();
        let graph = Rc::clone(&graph);
        move |command: Command| match command {
            Command::Quit => {
                if let Ok(mut graph) = graph.try_borrow_mut() {
                    // Restore the previous default on the way out; if a sync was
                    // issued, the loop below waits for `core.done` (bounded) before
                    // tearing down the PipeWire connection.
                    graph.set_system_default(false);
                    graph.journal.begin_quit_wait();
                }
                main_loop.quit();
            }
            Command::QuitAbandon => {
                if let Ok(mut graph) = graph.try_borrow_mut() {
                    graph.journal.on_core_error();
                }
                main_loop.quit();
            }
            other => {
                if let Ok(mut graph) = graph.try_borrow_mut() {
                    graph.handle(other);
                }
            }
        }
    });

    let _ = ready.send(Ok(()));
    main_loop.run();

    if graph.borrow().journal.is_waiting_for_quit() {
        let quit_timer = main_loop.loop_().add_timer({
            let weak = Rc::downgrade(&graph);
            let main_loop = main_loop.downgrade();
            move |_exp| {
                let should_quit = weak.upgrade().is_some_and(|graph| {
                    graph
                        .try_borrow_mut()
                        .map(|mut g| g.journal.on_quit_timeout())
                        .unwrap_or(true)
                });
                if should_quit && let Some(main_loop) = main_loop.upgrade() {
                    main_loop.quit();
                }
            }
        });
        let _ = quit_timer.update_timer(Some(RESTORE_SYNC_TIMEOUT), None);
        while graph.borrow().journal.is_waiting_for_quit() {
            main_loop.run();
        }
        drop(quit_timer);
    }

    drop(_receiver);
    drop(core_listener);
    drop(registry_listener);
    shared.quit.store(true, Ordering::SeqCst);
}

fn on_global(
    registry: &pw::registry::Registry,
    graph: &Rc<RefCell<Graph>>,
    global: &GlobalObject<&SpaDictRef>,
) {
    match global.type_ {
        pw::types::ObjectType::Node => on_node_global(registry, graph, global),
        pw::types::ObjectType::Link => {
            let props = global.props;
            let output = props
                .and_then(|p| p.get("link.output.node"))
                .and_then(parse_u32);
            let input = props
                .and_then(|p| p.get("link.input.node"))
                .and_then(parse_u32);
            if let (Some(output), Some(input)) = (output, input) {
                let mut graph = graph.borrow_mut();
                graph.links.insert(global.id, (output, input));
                graph.publish();
            }
        }
        pw::types::ObjectType::Metadata => on_metadata_global(registry, graph, global),
        _ => {}
    }
}

fn on_node_global(
    registry: &pw::registry::Registry,
    graph: &Rc<RefCell<Graph>>,
    global: &GlobalObject<&SpaDictRef>,
) {
    let props = global.props;
    let media_class = props.and_then(|p| p.get("media.class"));
    let node_name = props.and_then(|p| p.get("node.name"));
    let (dsp_name, direct_name) = {
        let graph = graph.borrow();
        (
            graph.opts.dsp_sink_name.clone(),
            graph.opts.direct_sink_name.clone(),
        )
    };
    let is_stream = media_class == Some("Stream/Output/Audio");
    let is_ours =
        node_name.is_some_and(|name| name == dsp_name.as_str() || name == direct_name.as_str());
    if !is_stream && !is_ours {
        if media_class == Some("Audio/Sink")
            && let Some(name) = node_name
        {
            let description = props
                .and_then(|p| p.get("node.description"))
                .map(str::to_string);
            let mut graph = graph.borrow_mut();
            graph
                .other_sinks
                .insert(global.id, (name.to_string(), description));
            graph.publish();
        }
        return;
    }
    let Ok(node) = registry.bind::<Node, _>(global) else {
        log::debug!("cannot bind node {}", global.id);
        return;
    };
    let id = global.id;
    {
        let mut graph = graph.borrow_mut();
        let entry = graph.nodes.entry(id).or_default();
        if let Some(props) = props {
            entry.app_name = props.get("application.name").map(str::to_string);
            entry.binary = props.get("application.process.binary").map(str::to_string);
            entry.media_name = props.get("media.name").map(str::to_string);
            entry.media_class = props.get("media.class").map(str::to_string);
            entry.node_name = props.get("node.name").map(str::to_string);
            entry.serial = props.get("object.serial").and_then(|v| v.parse().ok());
            entry.rate = props.get("audio.rate").and_then(|v| v.parse().ok());
            entry.channels = props.get("audio.channels").and_then(|v| v.parse().ok());
        }
        graph.refresh_role(id);
    }

    let weak = Rc::downgrade(graph);
    let listener = node
        .add_listener_local()
        .info({
            let weak = Weak::clone(&weak);
            move |info| {
                let Some(graph) = weak.upgrade() else { return };
                let mut graph = graph.borrow_mut();
                if let Some(props) = info.props() {
                    graph.on_node_props(id, props);
                }
                let running = matches!(info.state(), pw::node::NodeState::Running);
                if let Some(node) = graph.nodes.get_mut(&id) {
                    node.running = running;
                }
                // Params are only meaningful once the node has a format;
                // re-enumerate when it starts running.
                if running && let Some(node) = graph.nodes.get(&id).and_then(|n| n.node.as_ref()) {
                    node.enum_params(0, Some(ParamType::Format), 0, u32::MAX);
                    node.enum_params(1, Some(ParamType::Props), 0, u32::MAX);
                }
                graph.publish();
            }
        })
        .param({
            let weak = Weak::clone(&weak);
            move |_seq, param, _index, _next, pod| {
                let Some(graph) = weak.upgrade() else { return };
                let Some(pod) = pod else { return };
                graph.borrow_mut().on_param(id, param, pod);
            }
        })
        .register();
    {
        let mut graph = graph.borrow_mut();
        if let Some(entry) = graph.nodes.get_mut(&id) {
            entry.node = Some(node);
            entry.listener = Some(listener);
        }
        graph.publish();
        // Subscribe after the proxy is owned by the graph so it lives as long
        // as the node does.
        if let Some(node) = graph.nodes.get(&id).and_then(|n| n.node.as_ref()) {
            node.subscribe_params(&[ParamType::Format, ParamType::Props]);
            node.enum_params(0, Some(ParamType::Format), 0, u32::MAX);
            node.enum_params(1, Some(ParamType::Props), 0, u32::MAX);
        }
    }
}

fn on_metadata_global(
    registry: &pw::registry::Registry,
    graph: &Rc<RefCell<Graph>>,
    global: &GlobalObject<&SpaDictRef>,
) {
    let Some(props) = global.props else { return };
    if props.get("metadata.name") != Some("default") {
        return;
    }
    let Ok(meta) = registry.bind::<Metadata, _>(global) else {
        log::debug!("cannot bind default metadata");
        return;
    };
    let weak = Rc::downgrade(graph);
    let listener = meta
        .add_listener_local()
        .property(move |subject, key, _type, value| {
            let Some(graph) = weak.upgrade() else {
                return 0;
            };
            if subject != 0 {
                return 0;
            }
            let mut graph = graph.borrow_mut();
            let raw = value.map(str::to_string);
            match key {
                Some(EFFECTIVE_KEY) => {
                    graph.default_sink = raw.as_deref().and_then(parse_default_name)
                }
                Some(CONFIGURED_KEY) => {
                    graph.configured_sink = raw.as_deref().and_then(parse_default_name)
                }
                _ => return 0,
            }
            match key {
                Some(EFFECTIVE_KEY) => graph.default_sink_raw = raw,
                Some(CONFIGURED_KEY) => graph.configured_sink_raw = raw,
                _ => {}
            }
            // The metadata's contents arrived: from here on, "nothing set"
            // can be trusted, and a crash left-over can be repaired.
            graph.defaults_known = true;
            graph.publish();
            0
        })
        .register();
    let mut graph = graph.borrow_mut();
    graph.meta = Some(meta);
    graph.meta_listener = Some(listener);
    graph.shared.meta_ready.store(true, Ordering::Relaxed);
}

impl Graph {
    fn on_global_remove(&mut self, id: u32) {
        if self
            .meta
            .as_ref()
            .is_some_and(|meta| meta.upcast_ref().id() == id)
        {
            // WirePlumber restarted; a new metadata global will be bound (and
            // reassert our default then). Keep `defaults_known`: the snapshot
            // we must eventually restore is already safe.
            self.meta = None;
            self.meta_listener = None;
            self.default_sink = None;
            self.default_sink_raw = None;
            self.configured_sink = None;
            self.configured_sink_raw = None;
            self.shared.meta_ready.store(false, Ordering::Relaxed);
            self.publish();
            return;
        }
        let removed_node = self.nodes.remove(&id).is_some();
        let removed_link = self.links.remove(&id).is_some();
        let removed_sink = self.other_sinks.remove(&id).is_some();
        if !removed_node && !removed_link && !removed_sink {
            return;
        }
        self.routed.remove(&id);
        self.rule_routed.remove(&id);
        // A stream is no longer ours to route once it is gone; keep rules for
        // the next instance of the app.
        self.publish();
    }
}

fn parse_u32(value: &str) -> Option<u32> {
    value.trim().parse().ok()
}

fn parse_default_name(value: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(value).ok()?;
    value
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

fn display_name(name: Option<&str>) -> &str {
    name.unwrap_or("(unset)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_name_parsing() {
        assert_eq!(
            parse_default_name(r#"{"name":"audivane_dsp"}"#),
            Some("audivane_dsp".into())
        );
        assert_eq!(
            parse_default_name(r#"{ "name" : "alsa_output.pci" }"#),
            Some("alsa_output.pci".into())
        );
        assert_eq!(parse_default_name("audivane_dsp"), None);
        assert_eq!(parse_default_name(""), None);
    }

    #[test]
    fn facts_helpers() {
        let facts = Facts {
            streams: vec![
                StreamFact {
                    node: 2,
                    sink: Some(SinkChoice::Dsp),
                    ..Default::default()
                },
                StreamFact {
                    node: 3,
                    sink: Some(SinkChoice::Direct),
                    ..Default::default()
                },
            ],
            sinks: vec![SinkFact {
                choice: SinkChoice::Dsp,
                node: 10,
                serial: Some(42),
                name: DSP_SINK_NAME.into(),
                channels: Some(2),
                volume: Some(1.0),
                muted: false,
            }],
            default_sink: Some("audivane_dsp".into()),
            default_configured_sink: Some("alsa_output.pci".into()),
            normal_output: None,
        };
        assert_eq!(facts.sink(SinkChoice::Dsp).map(|s| s.node), Some(10));
        assert_eq!(facts.sink(SinkChoice::Direct), None);
        assert_eq!(facts.streams_on(SinkChoice::Dsp).len(), 1);
        assert_eq!(facts.streams_on(SinkChoice::Direct).len(), 1);
        assert_ne!(facts.default_sink, facts.default_configured_sink);
    }

    #[test]
    fn default_snapshot_file_round_trip() {
        let dir = std::env::temp_dir().join(format!(
            "avdsp-default-snapshot-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("default-sink-restore.json");
        let snapshot = DefaultSnapshot {
            written: vec!["audivane_dsp".into()],
            effective: Some(r#"{"name":"alsa_output.hdmi"}"#.into()),
            configured: None,
        };
        save_snapshot(&path, &snapshot).unwrap();
        assert!(path.is_file());
        assert_eq!(load_snapshot(&path), Some(snapshot.clone()));
        // The temp file never lingers.
        let names: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["default-sink-restore.json".to_string()]);
        // Unreadable content is ignored rather than panicking.
        fs::write(&path, "{ not json").unwrap();
        assert_eq!(load_snapshot(&path), None);
        remove_snapshot(&path);
        assert!(!path.exists());
        remove_snapshot(&path); // idempotent
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_snapshot_restores_absent_keys_as_absent() {
        let snapshot = DefaultSnapshot {
            written: vec!["audivane_dsp".into()],
            effective: None,
            configured: Some(r#"{"name":"different"}"#.into()),
        };
        let text = serde_json::to_string(&snapshot).unwrap();
        let back: DefaultSnapshot = serde_json::from_str(&text).unwrap();
        assert_eq!(back, snapshot);
        assert!(back.effective.is_none());
        assert!(back.configured.is_some());
    }

    #[test]
    fn rules_match_app_or_binary_case_insensitively() {
        let stream = StreamFact {
            node: 7,
            app_name: Some("Strawberry".into()),
            binary: Some("strawberry".into()),
            ..Default::default()
        };
        let matches = |app: &str| {
            [stream.app_name.as_deref(), stream.binary.as_deref()]
                .into_iter()
                .flatten()
                .any(|v| v.eq_ignore_ascii_case(app))
        };
        assert!(matches("strawberry"));
        assert!(matches("STRAWBERRY"));
        assert!(!matches("straw"));
    }

    fn temp_restore_path(tag: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "avdsp-journal-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("default-sink-restore.json");
        (dir, path)
    }

    #[test]
    fn restore_journal_waits_for_sync_then_removes_file() {
        let (dir, path) = temp_restore_path("sync");
        let snapshot = DefaultSnapshot {
            written: vec!["audivane_dsp".into()],
            effective: Some(r#"{"name":"alsa_output.hdmi"}"#.into()),
            configured: None,
        };
        save_snapshot(&path, &snapshot).unwrap();

        let mut tracker = RestoreJournalTracker::<u32>::default();
        tracker.note_snapshot_saved();
        tracker.finish_restore(Some(&path), true, || Ok(42));
        // File MUST still be present immediately after issuing restore metadata writes.
        assert!(path.is_file());
        // Quit must wait for the pending sync seq.
        assert!(tracker.begin_quit_wait());
        // Unrelated sync completion must neither delete the file nor exit the quit wait.
        assert!(!tracker.on_sync_done(41));
        assert!(path.is_file());
        // Matching sync completion deletes the file and ends the quit wait.
        assert!(tracker.on_sync_done(42));
        assert!(!path.exists());
        assert!(!tracker.begin_quit_wait());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn restore_journal_preserved_on_sync_error_disconnect_and_quit_timeout() {
        // 1) core.sync failure preserves journal and does not stall Quit.
        let (dir1, path1) = temp_restore_path("err");
        save_snapshot(&path1, &DefaultSnapshot::default()).unwrap();
        let mut t1 = RestoreJournalTracker::<u32>::default();
        t1.note_snapshot_saved();
        t1.finish_restore(Some(&path1), true, || Err(()));
        assert!(path1.is_file());
        assert!(!t1.begin_quit_wait());
        let _ = fs::remove_dir_all(&dir1);

        // 2) Core error / disconnect preserves journal even if an ack races in later.
        let (dir2, path2) = temp_restore_path("disc");
        save_snapshot(&path2, &DefaultSnapshot::default()).unwrap();
        let mut t2 = RestoreJournalTracker::<u32>::default();
        t2.note_snapshot_saved();
        t2.finish_restore(Some(&path2), true, || Ok(7));
        t2.on_core_error();
        assert!(!t2.begin_quit_wait());
        assert!(!t2.on_sync_done(7));
        assert!(path2.is_file());
        let _ = fs::remove_dir_all(&dir2);

        // 3) Quit timeout preserves un-acked journal.
        let (dir3, path3) = temp_restore_path("timeout");
        save_snapshot(&path3, &DefaultSnapshot::default()).unwrap();
        let mut t3 = RestoreJournalTracker::<u32>::default();
        t3.note_snapshot_saved();
        t3.finish_restore(Some(&path3), true, || Ok(9));
        assert!(t3.begin_quit_wait());
        assert!(t3.on_quit_timeout());
        assert!(path3.is_file());
        // Second timeout firing is a no-op.
        assert!(!t3.on_quit_timeout());
        let _ = fs::remove_dir_all(&dir3);
    }

    #[test]
    fn restore_journal_stale_sync_does_not_delete_newer_snapshot() {
        let (dir, path) = temp_restore_path("gen");
        save_snapshot(&path, &DefaultSnapshot::default()).unwrap();

        let mut tracker = RestoreJournalTracker::<u32>::default();
        tracker.note_snapshot_saved();
        // Recover previous session emits sync seq 10.
        tracker.finish_restore(Some(&path), true, || Ok(10));
        assert!(path.is_file());
        // Before seq 10 completes, enter_system_default persists a fresh snapshot (gen + 1).
        tracker.note_snapshot_saved();
        save_snapshot(&path, &DefaultSnapshot::default()).unwrap();

        // Stale seq 10 completes: must NOT delete the newer snapshot!
        assert!(!tracker.on_sync_done(10));
        assert!(path.is_file());

        // Subsequent restore for the newer generation removes it on sync 11.
        tracker.finish_restore(Some(&path), true, || Ok(11));
        assert!(path.is_file());
        assert!(!tracker.on_sync_done(11));
        assert!(!path.exists());

        let _ = fs::remove_dir_all(&dir);
    }
}
