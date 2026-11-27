//! The engine bypass resampler's chains, designed and built off the
//! builder lock.
//!
//! Designing a rate pair's filter and building its chain can take from tens
//! of milliseconds to seconds (a 768 kHz to 44.1 kHz chain takes most of a
//! second even with its design cached). It runs on this module's worker
//! thread, never on the driver's control thread or under the builder mutex
//! that volume changes need too. The worker keeps one built chain per pair
//! ready ([`BypassDesigner::take`] hands it over and queues the next), so a
//! negotiation or an edit only publishes it. A pair whose chain is not
//! there yet is `Pending`: the open falls back to PipeWire (the driver
//! holds the pair back) or the previous chain keeps playing (a DSP toggle
//! at the same ratio) until it is, and the worker reports each finished
//! build to the callback set with [`BypassDesigner::set_on_done`].

use audivane_protocol::{OutputConfig, RateRatio};
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, OnceLock};

/// Builds a pair's chain: `Err` when it cannot be built.
pub type DesignBuild<C> = Arc<dyn Fn(&DesignKey) -> Result<C, String> + Send + Sync>;

/// Called by the worker with each finished build (`ok`: it built).
pub type DesignDone = Arc<dyn Fn(&DesignKey, bool) + Send + Sync>;

/// One pair of the engine bypass resampler, for one output section (as the
/// chain is built for the negotiated format) and quantum.
#[derive(Clone, Debug, PartialEq)]
pub struct DesignKey {
    pub output: OutputConfig,
    pub quantum: u32,
    pub input: u32,
    pub ratio: RateRatio,
}

impl DesignKey {
    /// The output rate of the pair.
    pub fn output_rate(&self) -> u32 {
        self.ratio.output_rate(self.input).unwrap_or(self.input)
    }
}

/// Where a pair stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Design {
    /// Its chain builds.
    Ready,
    /// Its chain cannot be built.
    Failed(String),
    /// Not known yet: its first build is queued or running.
    Pending,
}

struct Entry<C> {
    key: DesignKey,
    design: Design,
    /// A built chain, ready to hand over.
    spare: Option<C>,
}

/// Why a build is queued, in the order they run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Need {
    /// An open or an edit waits for it: never dropped for room.
    Waited,
    /// A spare to replace one handed over.
    Refill,
    /// Ahead of time (a rate the device lacks, before a stream plays it).
    Ahead,
}

struct State<C> {
    known: Vec<Entry<C>>,
    queue: VecDeque<(DesignKey, Need)>,
    running: Option<DesignKey>,
    /// A finished build whose `on_done` has not returned yet.
    reporting: bool,
    stop: bool,
}

struct Inner<C> {
    state: Mutex<State<C>>,
    cv: Condvar,
    build: DesignBuild<C>,
    on_done: OnceLock<DesignDone>,
}

/// The worker and what it built. Dropping it stops the worker.
pub struct BypassDesigner<C: Send + 'static> {
    inner: Arc<Inner<C>>,
}

/// How many pairs are remembered, and how many keep a spare chain (the
/// oldest go first).
const KNOWN_CAP: usize = 32;
const SPARE_CAP: usize = 8;
/// How many builds may wait. The pairs an open or an edit waits for go
/// first and are always kept; beyond this, the last queued of the others
/// (spare refills, then ahead-of-time builds) are dropped.
const QUEUE_CAP: usize = 16;

impl<C: Send + 'static> BypassDesigner<C> {
    pub fn new(build: DesignBuild<C>) -> Self {
        let inner = Arc::new(Inner {
            state: Mutex::new(State {
                known: Vec::new(),
                queue: VecDeque::new(),
                running: None,
                reporting: false,
                stop: false,
            }),
            cv: Condvar::new(),
            build,
            on_done: OnceLock::new(),
        });
        let worker = Arc::clone(&inner);
        let spawned = std::thread::Builder::new()
            .name("audivane-bypass-design".into())
            .spawn(move || run(&worker));
        if let Err(e) = spawned {
            log::error!("bypass resampler: the design thread did not start ({e})");
        }
        Self { inner }
    }

    /// Report finished builds to `done` (once; later calls are ignored).
    pub fn set_on_done(&self, done: DesignDone) {
        let _ = self.inner.on_done.set(done);
    }

    /// Where `key` stands; an unknown pair's build is queued (`Pending`)
    /// behind the others: a build ahead of time.
    pub fn status(&self, key: &DesignKey) -> Design {
        self.status_at(key, Need::Ahead)
    }

    /// As [`Self::status`], for a pair an open or an edit waits for: its
    /// build goes first and is never dropped for room.
    pub fn status_now(&self, key: &DesignKey) -> Design {
        self.status_at(key, Need::Waited)
    }

    fn status_at(&self, key: &DesignKey, need: Need) -> Design {
        let Ok(mut st) = self.inner.state.lock() else {
            return Design::Failed("the bypass resampler's designer is unavailable".into());
        };
        if let Some(e) = st.known.iter().find(|e| e.key == *key) {
            return e.design.clone();
        }
        queue(&mut st, &self.inner.cv, key, need);
        Design::Pending
    }

    /// Drop the queued builds `queued` rejects, and the spare chains of
    /// pairs `spares` rejects (an edit of the output, quantum, device or
    /// bypass mode superseded them). A build already running finishes.
    pub fn invalidate(
        &self,
        queued: impl Fn(&DesignKey) -> bool,
        spares: impl Fn(&DesignKey) -> bool,
    ) {
        let Ok(mut st) = self.inner.state.lock() else {
            return;
        };
        st.queue.retain(|(k, _)| queued(k));
        for e in st.known.iter_mut().filter(|e| !spares(&e.key)) {
            e.spare = None;
        }
        self.inner.cv.notify_all();
    }

    /// Where `key` stands, without queueing anything.
    pub fn peek(&self, key: &DesignKey) -> Design {
        let Ok(st) = self.inner.state.lock() else {
            return Design::Pending;
        };
        st.known
            .iter()
            .find(|e| e.key == *key)
            .map_or(Design::Pending, |e| e.design.clone())
    }

    /// The built chain for a `Ready` pair, and the next one queued (a
    /// refill, behind what opens and edits wait for); `None` while it is
    /// still being built (it is queued then, as waited for).
    pub fn take(&self, key: &DesignKey) -> Option<C> {
        let Ok(mut st) = self.inner.state.lock() else {
            return None;
        };
        let chain = st
            .known
            .iter_mut()
            .find(|e| e.key == *key && e.design == Design::Ready)
            .and_then(|e| e.spare.take());
        let need = if chain.is_some() {
            Need::Refill
        } else {
            Need::Waited
        };
        queue(&mut st, &self.inner.cv, key, need);
        chain
    }

    /// Wait until nothing is queued, running or being reported (tests; not
    /// while holding a lock the `on_done` callback takes).
    #[cfg(test)]
    pub fn wait_idle(&self) {
        let Ok(mut st) = self.inner.state.lock() else {
            return;
        };
        while !st.queue.is_empty() || st.running.is_some() || st.reporting {
            st = match self.inner.cv.wait(st) {
                Ok(g) => g,
                Err(_) => return,
            };
        }
    }
}

/// Queue a build of `key` for `need` unless one is running, or a spare
/// chain is there already, or it failed. A queued one moves up to a more
/// pressing need. Waited-for builds go to the front (the newest first),
/// refills behind them, ahead-of-time builds last. The queue keeps
/// [`QUEUE_CAP`] builds, never dropping a waited-for one.
fn queue<C>(st: &mut State<C>, cv: &Condvar, key: &DesignKey, need: Need) {
    let settled = st
        .known
        .iter()
        .any(|e| e.key == *key && (e.spare.is_some() || matches!(e.design, Design::Failed(_))));
    if settled || st.running.as_ref() == Some(key) {
        return;
    }
    if let Some(pos) = st.queue.iter().position(|(k, _)| k == key) {
        if st.queue[pos].1 <= need {
            return;
        }
        st.queue.remove(pos);
    }
    let at = match need {
        Need::Waited => 0,
        Need::Refill => st
            .queue
            .iter()
            .take_while(|(_, n)| *n == Need::Waited)
            .count(),
        Need::Ahead => st.queue.len(),
    };
    st.queue.insert(at, (key.clone(), need));
    while st.queue.len() > QUEUE_CAP && st.queue.back().is_some_and(|(_, n)| *n != Need::Waited) {
        st.queue.pop_back();
    }
    cv.notify_all();
}

impl<C: Send + 'static> Drop for BypassDesigner<C> {
    fn drop(&mut self) {
        // The worker may hold the last handle to the engine's builder (its
        // callback upgrades a weak one), so it is never joined here.
        if let Ok(mut st) = self.inner.state.lock() {
            st.stop = true;
        }
        self.inner.cv.notify_all();
    }
}

fn run<C>(inner: &Inner<C>) {
    loop {
        let key = {
            let Ok(mut st) = inner.state.lock() else {
                return;
            };
            loop {
                if st.stop {
                    return;
                }
                if let Some((k, _)) = st.queue.pop_front() {
                    st.running = Some(k.clone());
                    break k;
                }
                st = match inner.cv.wait(st) {
                    Ok(g) => g,
                    Err(_) => return,
                };
            }
        };
        let started = std::time::Instant::now();
        let result = (inner.build)(&key);
        log::debug!(
            "bypass resampler: {} Hz to {} Hz built in {} ms ({})",
            key.input,
            key.output_rate(),
            started.elapsed().as_millis(),
            if result.is_ok() { "ok" } else { "failed" }
        );
        let ok = result.is_ok();
        {
            let Ok(mut st) = inner.state.lock() else {
                return;
            };
            // Stopped meanwhile (the engine went away): nothing is
            // published or reported.
            if st.stop {
                return;
            }
            let (design, spare) = match result {
                Ok(c) => (Design::Ready, Some(c)),
                Err(e) => (Design::Failed(e), None),
            };
            if let Some(pos) = st.known.iter().position(|e| e.key == key) {
                st.known.remove(pos);
            }
            if st.known.len() >= KNOWN_CAP {
                st.known.remove(0);
            }
            st.known.push(Entry {
                key: key.clone(),
                design,
                spare,
            });
            let spares = st.known.iter().filter(|e| e.spare.is_some()).count();
            if spares > SPARE_CAP
                && let Some(oldest) = st.known.iter_mut().find(|e| e.spare.is_some())
            {
                oldest.spare = None;
            }
            st.running = None;
            st.reporting = true;
            inner.cv.notify_all();
        }
        let stopped = inner.state.lock().map_or(true, |st| st.stop);
        if !stopped && let Some(done) = inner.on_done.get() {
            done(&key, ok);
        }
        if let Ok(mut st) = inner.state.lock() {
            st.reporting = false;
            inner.cv.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    fn key(input: u32) -> DesignKey {
        DesignKey {
            output: OutputConfig::default(),
            quantum: 1024,
            input,
            ratio: RateRatio::from_rates(input, 48_000),
        }
    }

    /// A designer whose builds wait for `gate` and log their inputs.
    #[allow(clippy::type_complexity)]
    fn gated() -> (
        BypassDesigner<u32>,
        Arc<(Mutex<bool>, Condvar)>,
        Arc<Mutex<Vec<u32>>>,
    ) {
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let built = Arc::new(Mutex::new(Vec::new()));
        let (g, b) = (Arc::clone(&gate), Arc::clone(&built));
        let designer = BypassDesigner::new(Arc::new(move |k: &DesignKey| {
            let (open, cv) = &*g;
            let mut open = open.lock().unwrap();
            while !*open {
                open = cv.wait(open).unwrap();
            }
            b.lock().unwrap().push(k.input);
            Ok(k.input)
        }));
        (designer, gate, built)
    }

    fn open(gate: &(Mutex<bool>, Condvar)) {
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
    }

    /// Wait until the worker is inside a build (the gate holds it there).
    fn running(d: &BypassDesigner<u32>) {
        for _ in 0..500 {
            if d.inner.state.lock().unwrap().running.is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        panic!("no build started");
    }

    #[test]
    fn a_waited_for_build_is_never_dropped_for_room() {
        // While a build runs: a spare to refill, ahead-of-time builds, then
        // more waited-for pairs than the queue holds. Every waited-for one
        // builds (newest first), then the refill, then what room is left.
        let (d, gate, built) = gated();
        let wkey = |i: u32| DesignKey {
            quantum: 2048 + i,
            ..key(44_100 + i)
        };
        d.status_now(&key(48_000));
        running(&d);
        {
            // A ready pair with its spare just handed over.
            let mut st = d.inner.state.lock().unwrap();
            st.known.push(Entry {
                key: key(96_000),
                design: Design::Ready,
                spare: Some(96_000),
            });
        }
        assert_eq!(d.take(&key(96_000)), Some(96_000));
        for r in [88_200, 176_400] {
            d.status(&key(r));
        }
        for i in 0..20 {
            assert_eq!(d.status_now(&wkey(i)), Design::Pending);
        }
        // A pair asked ahead of time and then waited for moves up.
        d.status(&key(352_800));
        d.status_now(&key(352_800));
        {
            let st = d.inner.state.lock().unwrap();
            assert_eq!(st.queue.len(), 21, "{:?}", st.queue);
            assert!(st.queue.iter().all(|(_, n)| *n == Need::Waited));
        }
        open(&gate);
        d.wait_idle();
        let built = built.lock().unwrap().clone();
        let mut expected = vec![48_000, 352_800];
        expected.extend((0..20).rev().map(|i| 44_100 + i));
        assert_eq!(built, expected);
        // With room, the refill goes before ahead-of-time builds.
        let (d, gate, built) = gated();
        d.status_now(&key(48_000));
        running(&d);
        d.inner.state.lock().unwrap().known.push(Entry {
            key: key(96_000),
            design: Design::Ready,
            spare: Some(96_000),
        });
        d.status(&key(88_200));
        d.take(&key(96_000));
        d.status_now(&key(44_100));
        open(&gate);
        d.wait_idle();
        assert_eq!(
            built.lock().unwrap().as_slice(),
            &[48_000, 44_100, 96_000, 88_200]
        );
    }

    #[test]
    fn a_build_running_at_shutdown_is_neither_published_nor_reported() {
        let (d, gate, built) = gated();
        let reported = Arc::new(AtomicUsize::new(0));
        let r = Arc::clone(&reported);
        d.set_on_done(Arc::new(move |_: &DesignKey, _| {
            r.fetch_add(1, Ordering::SeqCst);
        }));
        d.status(&key(44_100));
        running(&d);
        let inner = Arc::clone(&d.inner);
        drop(d);
        open(&gate);
        for _ in 0..200 {
            if !built.lock().unwrap().is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(built.lock().unwrap().as_slice(), &[44_100]);
        assert_eq!(reported.load(Ordering::SeqCst), 0);
        assert!(inner.state.lock().unwrap().known.is_empty());
    }
}
