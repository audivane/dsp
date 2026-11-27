//! Measurement playback, the control half (`measure_play`, `measure_stop`,
//! `measure_status`): check the request and the file, keep the graph
//! running with a silent stream of our own, wait for the engine and the
//! followers, build each device's buffer at its injection rate (resampled
//! offline when the rates differ), hand the job to the audio threads
//! (`engine::measure`), follow it and take it back.

use crate::api::{Engine, EngineError};
use crate::engine::measure::{MeasureDevice, MeasureFault, MeasureJob, READING_INTERVAL_NS};
use audivane_protocol::RateRatio;
use audivane_protocol::ipc::{
    EngineConfig, EngineState, FollowerState, MeasureChain, MeasureContinuity, MeasureDeviceTiming,
    MeasurePlayRequest, MeasureState, MeasureStatus, RpcError, SinkChoice,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// The highest file peak accepted in `active`, dBFS.
pub const MAX_PEAK_DBFS: f64 = -1.0;
/// The highest file peak accepted in `bypass`, dBFS: no limiter, and room
/// for a follower's live resampler (its overshoot on a step) and the
/// keepalive under the ceiling (`engine::measure::CEILING`, −1 dBFS).
pub const MAX_PEAK_BYPASS_DBFS: f64 = -3.0;

/// The highest file peak accepted for `chain`, dBFS.
pub fn max_peak_dbfs(chain: MeasureChain) -> f64 {
    match chain {
        MeasureChain::Bypass => MAX_PEAK_BYPASS_DBFS,
        MeasureChain::Active => MAX_PEAK_DBFS,
    }
}
/// The keepalive's range, dBFS RMS.
pub const KEEPALIVE_DB: (f64, f64) = (-140.0, -60.0);
/// The lead-in's range, ms.
pub const LEAD_IN_MS: (f64, f64) = (1500.0, 10_000.0);
/// The longest file, seconds.
pub const MAX_SECONDS: f64 = 600.0;
/// The most samples the buffers of one run may hold (all devices).
pub const MAX_SAMPLES: usize = 64 << 20;
/// How long the audio thread may take to arm once the job is posted.
const ARM_TIMEOUT: Duration = Duration::from_secs(3);
/// Time a device may go without progress while it should play.
const STALL_TIMEOUT: Duration = Duration::from_secs(2);
/// After the injection's end: each device's processing and queue, and
/// this margin, before the run gives up waiting for a device to drain.
const DRAIN_MARGIN: Duration = Duration::from_secs(2);
/// How long the master's mute ramp may take after the release.
const RELEASE_TIMEOUT: Duration = Duration::from_secs(1);
/// How long the run waits for the audio threads to hand an old job back
/// before it leaves it to the engine's reaper.
const RETIRE_WAIT: Duration = Duration::from_secs(2);

/// A loaded WAV: its rate and planar channels in `[-1, 1]`.
#[derive(Debug)]
pub struct Wav {
    pub rate: u32,
    pub channels: Vec<Vec<f64>>,
}

/// Read a 16/24/32-bit integer or 32-bit float WAV (exact: every sample
/// maps to an `f64` that is the left-justified `i32` over 2³¹).
pub fn read_wav(path: &str) -> Result<Wav, String> {
    let mut r = hound::WavReader::open(path).map_err(|e| format!("{path}: {e}"))?;
    let spec = r.spec();
    let ch = usize::from(spec.channels);
    if ch == 0 {
        return Err(format!("{path}: no channels"));
    }
    if spec.sample_rate < 8_000 || spec.sample_rate > audivane_protocol::MAX_OUTPUT_RATE {
        return Err(format!(
            "{path}: rate {} Hz is out of range",
            spec.sample_rate
        ));
    }
    let frames = r.duration() as usize;
    if frames as f64 / f64::from(spec.sample_rate) > MAX_SECONDS {
        return Err(format!(
            "{path}: {:.0} s is longer than {MAX_SECONDS:.0} s",
            frames as f64 / f64::from(spec.sample_rate)
        ));
    }
    if frames.saturating_mul(ch) > MAX_SAMPLES {
        return Err(format!(
            "{path}: {frames} frames x {ch} channels is more than {} M samples",
            MAX_SAMPLES >> 20
        ));
    }
    let mut out = vec![Vec::with_capacity(frames); ch];
    match (spec.sample_format, spec.bits_per_sample) {
        (hound::SampleFormat::Float, 32) => {
            for (i, s) in r.samples::<f32>().enumerate() {
                let s = s.map_err(|e| format!("{path}: {e}"))?;
                if !s.is_finite() {
                    return Err(format!(
                        "{path}: sample {} (channel {}) is not a finite number",
                        i / ch,
                        i % ch
                    ));
                }
                if i >= frames * ch {
                    return Err(format!("{path}: more samples than its header says"));
                }
                out[i % ch].push(f64::from(s));
            }
        }
        (hound::SampleFormat::Int, b @ (16 | 24 | 32)) => {
            let scale = 1.0 / f64::from(1u32 << (b - 1));
            let scale = if b == 32 {
                1.0 / 2_147_483_648.0
            } else {
                scale
            };
            for (i, s) in r.samples::<i32>().enumerate() {
                let s = s.map_err(|e| format!("{path}: {e}"))?;
                if i >= frames * ch {
                    return Err(format!("{path}: more samples than its header says"));
                }
                out[i % ch].push(f64::from(s) * scale);
            }
        }
        (f, b) => {
            return Err(format!(
                "{path}: {b}-bit {} samples are not supported (16/24/32-bit integer or 32-bit float)",
                if f == hound::SampleFormat::Float {
                    "float"
                } else {
                    "integer"
                }
            ));
        }
    }
    let n = out.iter().map(Vec::len).min().unwrap_or(0);
    for c in &mut out {
        c.truncate(n);
    }
    if n == 0 {
        return Err(format!("{path}: no samples"));
    }
    Ok(Wav {
        rate: spec.sample_rate,
        channels: out,
    })
}

/// A step (or a stray) in a device's playback position larger than this
/// gets a note, ms.
pub const CONTINUITY_NOTE_MS: f64 = 1.0;
/// The time after the file a device's readings are kept for (its tail, its
/// queue and the drain's margin), seconds.
const READINGS_AFTER_S: f64 = 62.0;

/// Readings on either side a step must hold across ([`continuity`]).
const STEP_WINDOW: usize = 8;
/// A step or a stray counts only this many times the readings' jitter
/// above it.
const JITTER_FACTOR: f64 = 8.0;

/// The median of `x` (reordered; empty: 0).
fn median(x: &mut [f64]) -> f64 {
    if x.is_empty() {
        return 0.0;
    }
    let m = x.len() / 2;
    let (_, v, _) = x.select_nth_unstable_by(m, f64::total_cmp);
    *v
}

/// A least-squares quadratic through `(t, q)`: the value at each `t`.
fn quadratic_fit(pts: &[(f64, f64)]) -> Option<impl Fn(f64) -> f64> {
    let n = pts.len() as f64;
    if pts.len() < 3 {
        return None;
    }
    // Centred and scaled time keeps the normal equations well conditioned.
    let mt = pts.iter().map(|p| p.0).sum::<f64>() / n;
    let sc = pts
        .iter()
        .map(|p| (p.0 - mt).abs())
        .fold(0.0f64, f64::max)
        .max(1e-9);
    let mut s = [0.0f64; 5];
    let mut r = [0.0f64; 3];
    for &(t, q) in pts {
        let x = (t - mt) / sc;
        let mut xi = 1.0;
        for (k, sk) in s.iter_mut().enumerate() {
            *sk += xi;
            if k < 3 {
                r[k] += xi * q;
            }
            xi *= x;
        }
    }
    let m = [[s[0], s[1], s[2]], [s[1], s[2], s[3]], [s[2], s[3], s[4]]];
    let det = |m: [[f64; 3]; 3]| {
        m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
            - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
            + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0])
    };
    let d = det(m);
    if d.abs() < 1e-12 * s[0].powi(3) {
        return None;
    }
    let coef: [f64; 3] = std::array::from_fn(|c| {
        let mut mc = m;
        for (row, rv) in mc.iter_mut().zip(r) {
            row[c] = rv;
        }
        det(mc) / d
    });
    Some(move |t: f64| {
        let x = (t - mt) / sc;
        coef[0] + coef[1] * x + coef[2] * x * x
    })
}

/// The playback continuity check ([`MeasureContinuity`]) of `readings`
/// (DAC position, `CLOCK_MONOTONIC_RAW` ns) at the device's `rate`;
/// `t0_ns` is when the file's first frame reached the DAC (the jump's time
/// is from it; the first reading's without it). `None` with fewer than
/// three readings.
///
/// Robust to what a clean run has: the device's clock off the host's and
/// drifting slowly (a median rate takes the line out, a quadratic the
/// drift), timestamp jitter, and a lone reading off its neighbours (a
/// stray, set aside). A step is the difference of the medians of
/// `STEP_WINDOW` readings on either side: it must hold across most of
/// them, so a stray never reads as one.
pub fn continuity(
    readings: &[(i64, u64)],
    rate: u32,
    t0_ns: Option<u64>,
    dropped: u64,
) -> Option<MeasureContinuity> {
    let n = readings.len();
    if n < 3 {
        return None;
    }
    let (p0, s0) = readings[0];
    // Seconds and frames from the first reading (exact in f64 for a run).
    let pts: Vec<(f64, f64)> = readings
        .iter()
        .map(|&(p, t)| (t.saturating_sub(s0) as f64 * 1e-9, (p - p0) as f64))
        .collect();
    // The rate: the median slope over a few readings (a step or a stray
    // moves a handful of them).
    let lag = STEP_WINDOW.min((n - 1) / 2).max(1);
    let mut slopes: Vec<f64> = (lag..n)
        .filter_map(|i| {
            let dt = pts[i].0 - pts[i - lag].0;
            (dt > 0.0).then(|| (pts[i].1 - pts[i - lag].1) / dt)
        })
        .collect();
    let slope = median(&mut slopes);
    let q: Vec<f64> = pts.iter().map(|(t, p)| p - slope * t).collect();
    // The jitter: the spread of neighbouring differences (robust).
    let mut d: Vec<f64> = q.windows(2).map(|w| w[1] - w[0]).collect();
    let md = median(&mut d.clone());
    for x in &mut d {
        *x = (*x - md).abs();
    }
    let jitter = 1.4826 * median(&mut d) / 2f64.sqrt();
    let limit = (CONTINUITY_NOTE_MS * 1e-3 * f64::from(rate.max(1))).max(JITTER_FACTOR * jitter);
    // Strays: readings far off the running median around them.
    let w = STEP_WINDOW;
    let mut buf = Vec::with_capacity(2 * w + 1);
    let stray: Vec<bool> = (0..n)
        .map(|i| {
            buf.clear();
            buf.extend_from_slice(&q[i.saturating_sub(w)..(i + w + 1).min(n)]);
            (q[i] - median(&mut buf)).abs() > limit
        })
        .collect();
    let strays = stray.iter().filter(|s| **s).count();
    // The step: the medians of the readings on either side.
    let (mut jump, mut at) = (0.0f64, 0usize);
    if n > 2 * w {
        for k in w..=n - w {
            buf.clear();
            buf.extend_from_slice(&q[k..k + w]);
            let after = median(&mut buf);
            buf.clear();
            buf.extend_from_slice(&q[k - w..k]);
            let j = after - median(&mut buf);
            if j.abs() > jump.abs() {
                jump = j;
                at = k;
            }
        }
        // Where it steps: the largest neighbouring difference that way
        // around there.
        let sign = jump.signum();
        at = (at.saturating_sub(w / 2).max(1)..(at + w / 2).min(n - 1) + 1)
            .max_by(|&a, &b| ((q[a] - q[a - 1]) * sign).total_cmp(&((q[b] - q[b - 1]) * sign)))
            .unwrap_or(at);
    }
    // The residual: off a slowly drifting clock, the strays set aside.
    let kept: Vec<(f64, f64)> = pts
        .iter()
        .zip(&q)
        .zip(&stray)
        .filter(|(_, s)| !**s)
        .map(|((p, q), _)| (p.0, *q))
        .collect();
    let max_res = quadratic_fit(&kept).map_or(0.0, |f| {
        kept.iter()
            .fold(0.0f64, |m, &(t, q)| m.max((q - f(t)).abs()))
    });
    let us = |frames: f64| frames * 1e6 / f64::from(rate.max(1));
    let origin = t0_ns.unwrap_or(s0) as f64;
    Some(MeasureContinuity {
        readings: n.min(u32::MAX as usize) as u32,
        max_residual_frames: max_res,
        max_residual_us: us(max_res),
        jump_frames: jump,
        jump_us: us(jump),
        jump_at_s: (readings[at].1 as f64 - origin) * 1e-9,
        jitter_us: us(jitter),
        strays: strays.min(u32::MAX as usize) as u32,
        dropped,
    })
}

/// The note for a device whose check shows a step (or, without one, a
/// residual) larger than [`CONTINUITY_NOTE_MS`] and [`JITTER_FACTOR`]
/// times the readings' jitter.
pub(crate) fn continuity_note(device: &str, c: &MeasureContinuity) -> Option<String> {
    let limit = (CONTINUITY_NOTE_MS * 1e3).max(JITTER_FACTOR * c.jitter_us);
    if c.jump_us.abs() > limit {
        Some(format!(
            "{device}: its playback position jumped by {:.0} frames ({:.0} µs) at {:.3} s (from the file's start): the timeline after it is shifted",
            c.jump_frames, c.jump_us, c.jump_at_s
        ))
    } else if c.max_residual_us > limit {
        Some(format!(
            "{device}: its playback position strayed {:.0} frames ({:.0} µs) from a steady clock",
            c.max_residual_frames, c.max_residual_us
        ))
    } else {
        None
    }
}

/// Room for a device's readings over a run: the lead-in, the file and the
/// time after it, one per [`READING_INTERVAL_NS`].
fn readings_capacity(lead_in_ms: f64, duration_s: f64) -> usize {
    let secs = lead_in_ms * 1e-3 + duration_s + READINGS_AFTER_S;
    (secs * 1e9 / READING_INTERVAL_NS as f64).ceil() as usize + 64
}

fn peak_dbfs(x: &[f64]) -> f64 {
    let p = x.iter().fold(0.0f64, |m, v| m.max(v.abs()));
    if p > 0.0 {
        20.0 * p.log10()
    } else {
        f64::NEG_INFINITY
    }
}

/// The samples a run's buffers hold: `frames` file frames at `file_rate`,
/// resampled to each device's (injection rate, routes).
fn projected_samples(
    frames: usize,
    file_rate: u32,
    devices: impl Iterator<Item = (u32, usize)>,
) -> u128 {
    devices
        .map(|(rate, routes)| {
            let f = (frames as u128 * u128::from(rate)).div_ceil(u128::from(file_rate.max(1))) + 1;
            f * routes as u128
        })
        .sum()
}

fn invalid(msg: impl Into<String>) -> EngineError {
    EngineError::new(RpcError::INVALID_PARAMS, msg)
}

fn bad_state(msg: impl Into<String>) -> EngineError {
    EngineError::new(RpcError::BAD_STATE, msg)
}

/// A route resolved against the config: the device (`None`: the master)
/// and its channel.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Route {
    follower: Option<String>,
    channel: usize,
}

/// One device's routes: (device, [(channel, file channel)]).
type DeviceRoutes = Vec<(Option<String>, Vec<(usize, usize)>)>;

/// Check the request against the config and the file: what plays where.
fn check_request(
    req: &MeasurePlayRequest,
    cfg: &EngineConfig,
    channels: usize,
    wav: &Wav,
) -> Result<DeviceRoutes, EngineError> {
    if req.routes.is_empty() {
        return Err(invalid("measure_play: routes is empty"));
    }
    if let Some(db) = req.keepalive_db
        && !(KEEPALIVE_DB.0..=KEEPALIVE_DB.1).contains(&db)
    {
        return Err(invalid(format!(
            "measure_play: keepalive_db must be between {} and {} dBFS (got {db})",
            KEEPALIVE_DB.0, KEEPALIVE_DB.1
        )));
    }
    if !(LEAD_IN_MS.0..=LEAD_IN_MS.1).contains(&req.lead_in_ms) {
        return Err(invalid(format!(
            "measure_play: lead_in_ms must be between {} and {} ms (got {})",
            LEAD_IN_MS.0, LEAD_IN_MS.1, req.lead_in_ms
        )));
    }
    let mut routes: Vec<Route> = Vec::new();
    for (i, r) in req.routes.iter().enumerate() {
        let device = r.device.trim();
        let follower = if device == "master" || device == cfg.device {
            None
        } else if let Some(f) = cfg.outputs.followers.iter().find(|f| f.device == device) {
            if !f.enabled {
                return Err(invalid(format!(
                    "measure_play: routes[{i}]: follower {device} is disabled"
                )));
            }
            Some(f.device.clone())
        } else {
            return Err(invalid(format!(
                "measure_play: routes[{i}]: {device} is neither the main device ({}) nor a follower",
                cfg.device
            )));
        };
        let channel = r.channel as usize;
        if channel >= channels {
            return Err(invalid(format!(
                "measure_play: routes[{i}]: channel {channel} (the engine plays {channels} channels, from 0)"
            )));
        }
        let route = Route { follower, channel };
        if routes.contains(&route) {
            return Err(invalid(format!(
                "measure_play: routes[{i}]: {device} channel {channel} is listed twice"
            )));
        }
        routes.push(route);
    }
    let file_ch = wav.channels.len();
    if file_ch != 1 && file_ch != routes.len() {
        return Err(invalid(format!(
            "measure_play: the file has {file_ch} channels: give one route per channel ({} routes), or a mono file",
            routes.len()
        )));
    }
    let limit = max_peak_dbfs(req.chain);
    for (c, x) in wav.channels.iter().enumerate() {
        let p = peak_dbfs(x);
        if p > limit {
            return Err(invalid(format!(
                "measure_play: the file's channel {c} peaks at {p:.2} dBFS, above {limit} dBFS{}",
                match req.chain {
                    MeasureChain::Bypass =>
                        " (bypass skips the limiter and leaves room for a follower's resampler)",
                    MeasureChain::Active => "",
                }
            )));
        }
    }
    let mut by_device: DeviceRoutes = Vec::new();
    for (i, r) in routes.into_iter().enumerate() {
        let src = if file_ch == 1 { 0 } else { i };
        match by_device.iter_mut().find(|(d, _)| *d == r.follower) {
            Some((_, v)) => v.push((r.channel, src)),
            None => by_device.push((r.follower, vec![(r.channel, src)])),
        }
    }
    // The master first: the status lists it first.
    by_device.sort_by_key(|(d, _)| d.is_some());
    Ok(by_device)
}

// ---------------------------------------------------------------------------
// Readiness: what the run waits for before it arms.

/// One look at the engine before a run arms.
#[derive(Clone, Debug, Default)]
struct Readiness {
    /// Why the engine itself is not ready (not playing the DSP sink, no
    /// graph rate).
    engine: Option<String>,
    /// Why the multi-device alignment may still move (`outputs.settling`).
    settling: Option<String>,
    /// The first routed follower that is not heard: (device, its state
    /// explained).
    inaudible: Option<(String, String)>,
    /// What moves every device when it changes: the master's alignment and
    /// compensation (ms, µs).
    alignment: (f64, f64),
    /// The plan, when the engine is ready.
    plan: Option<Plan>,
}

/// A follower's state, for an error.
fn follower_state_text(s: &audivane_protocol::ipc::FollowerStatus) -> String {
    let what = match s.state {
        FollowerState::Off => "not running".to_string(),
        FollowerState::Opening => "opening its device".to_string(),
        FollowerState::Locking => "locking (muted until its drift loop locks)".to_string(),
        FollowerState::Muted => {
            if !s.enabled {
                "muted (disabled)".to_string()
            } else {
                "muted (the master is off the DSP path, or its chain compensation is settling)"
                    .to_string()
            }
        }
        FollowerState::Lost => "lost (its device is gone or failed)".to_string(),
        FollowerState::Locked | FollowerState::Unaligned => "heard".to_string(),
    };
    match &s.error {
        Some(e) if !e.is_empty() => format!("{what}: {e}"),
        _ => what,
    }
}

/// Look at the engine and the routed followers: what is not ready yet,
/// and the plan once the engine plays. An error ends the run at once
/// (varispeed, a `bypass` route on a follower that is not at ×1).
fn readiness(
    snap: &crate::api::EngineSnapshot,
    outputs: &audivane_protocol::ipc::OutputsStatus,
    routes: &DeviceRoutes,
    chain: MeasureChain,
) -> Result<Readiness, String> {
    let mut r = Readiness {
        settling: outputs.settling.clone(),
        alignment: (outputs.alignment_ms, outputs.master.compensation_us),
        ..Readiness::default()
    };
    if snap.varispeed.as_ref().is_some_and(|v| v.active) {
        return Err("varispeed is on".into());
    }
    if snap.state != EngineState::Playing || snap.active_sink != Some(SinkChoice::Dsp) {
        r.engine = Some(format!(
            "the engine is {:?} on {:?}, not playing the DSP sink",
            snap.state, snap.active_sink
        ));
    } else if snap.graph_rate == 0 {
        r.engine = Some("no graph rate".into());
    }
    let mut devices = Vec::new();
    for (dev, _) in routes {
        match dev {
            None => devices.push(("master".into(), snap.device.clone(), snap.output_rate)),
            Some(f) => match outputs.followers.iter().find(|s| &s.device == f) {
                Some(s) if matches!(s.state, FollowerState::Locked | FollowerState::Unaligned) => {
                    if chain == MeasureChain::Bypass && s.device_rate != snap.graph_rate {
                        return Err(format!(
                            "bypass on follower {f} needs its device at the graph rate ({} Hz); it runs at {} Hz: use the active chain, or a rate setting without upsampling",
                            snap.graph_rate, s.device_rate
                        ));
                    }
                    devices.push(("follower".into(), f.clone(), s.device_rate));
                }
                Some(s) => {
                    r.inaudible
                        .get_or_insert_with(|| (f.clone(), follower_state_text(s)));
                }
                None => {
                    r.inaudible
                        .get_or_insert_with(|| (f.clone(), "not running".into()));
                }
            },
        }
    }
    if r.engine.is_none() {
        r.plan = Some(Plan {
            graph_rate: snap.graph_rate,
            ratio: snap.ratio,
            output_rate: snap.output_rate,
            master: snap.device.clone(),
            devices,
        });
    }
    Ok(r)
}

/// How long the engine may take to play, and the alignment to settle.
const READY_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a routed follower may stay unheard once the engine plays (and
/// since the alignment last moved: every follower re-acquires then).
const AUDIBLE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long everything must hold still before the run arms.
const STEADY: Duration = Duration::from_millis(500);

/// Paces [`readiness`]: the run arms once the engine plays, the alignment
/// has settled and every routed follower is heard, all for [`STEADY`]; it
/// gives up on a routed follower that stays unheard for
/// [`AUDIBLE_TIMEOUT`], and on anything else after [`READY_TIMEOUT`].
struct ReadyGate {
    start: Instant,
    /// Since when the engine plays and the alignment holds its value.
    audible_from: Option<Instant>,
    alignment: Option<(f64, f64)>,
    steady_since: Option<(Instant, u32, RateRatio)>,
}

impl ReadyGate {
    fn new(now: Instant) -> Self {
        Self {
            start: now,
            audible_from: None,
            alignment: None,
            steady_since: None,
        }
    }

    /// One look: `Some(plan)` to arm, `None` to wait, an error to give up.
    fn step(&mut self, now: Instant, r: Readiness) -> Result<Option<Plan>, String> {
        let late = now.duration_since(self.start) >= READY_TIMEOUT;
        if let Some(why) = r.engine {
            self.steady_since = None;
            self.audible_from = None;
            if late {
                return Err(format!(
                    "the engine was not ready within {} s: {why}",
                    READY_TIMEOUT.as_secs()
                ));
            }
            return Ok(None);
        }
        if self.alignment != Some(r.alignment) {
            self.alignment = Some(r.alignment);
            self.audible_from = Some(now);
        }
        let audible_from = *self.audible_from.get_or_insert(now);
        if let Some((device, state)) = &r.inaudible {
            self.steady_since = None;
            if now.duration_since(audible_from) >= AUDIBLE_TIMEOUT {
                return Err(format!(
                    "follower {device} is not heard after {} s: it is {state}",
                    AUDIBLE_TIMEOUT.as_secs()
                ));
            }
        }
        if let Some(why) = &r.settling {
            self.steady_since = None;
            if late {
                return Err(format!(
                    "the multi-device alignment did not settle within {} s: {why}",
                    READY_TIMEOUT.as_secs()
                ));
            }
            return Ok(None);
        }
        // Every path that keeps waiting ends at the deadline.
        let waiting = if let Some((device, state)) = &r.inaudible {
            format!("follower {device} is {state}")
        } else if let Some(plan) = r.plan {
            let key = (plan.graph_rate, plan.ratio);
            match self.steady_since {
                Some((t, g, q)) if (g, q) == key => {
                    if now.duration_since(t) >= STEADY {
                        return Ok(Some(plan));
                    }
                }
                _ => self.steady_since = Some((now, key.0, key.1)),
            }
            format!(
                "the rate is not steady ({} Hz, {})",
                plan.graph_rate, plan.ratio
            )
        } else {
            "no plan".to_string()
        };
        if late {
            return Err(format!(
                "the engine was not ready within {} s: {waiting}",
                READY_TIMEOUT.as_secs()
            ));
        }
        Ok(None)
    }
}

// ---------------------------------------------------------------------------
// The carrier: a silent stream into our DSP sink, so the graph (and the
// engine) runs whether or not anything else plays.

struct Carrier {
    quit: pipewire::channel::Sender<()>,
    thread: Option<JoinHandle<()>>,
}

impl Carrier {
    fn start(rate: u32, channels: u32) -> Result<Self, String> {
        let (quit_tx, quit_rx) = pipewire::channel::channel::<()>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let thread = std::thread::Builder::new()
            .name("audivane-measure-carrier".into())
            .spawn(move || {
                if let Err(e) = run_carrier(rate, channels, quit_rx, &ready_tx) {
                    let _ = ready_tx.send(Err(e));
                }
            })
            .map_err(|e| format!("carrier thread: {e}"))?;
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(Self {
                quit: quit_tx,
                thread: Some(thread),
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                let _ = quit_tx.send(());
                Err("the measurement stream did not start".into())
            }
        }
    }
}

impl Drop for Carrier {
    fn drop(&mut self) {
        let _ = self.quit.send(());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn run_carrier(
    rate: u32,
    channels: u32,
    quit: pipewire::channel::Receiver<()>,
    ready: &std::sync::mpsc::Sender<Result<(), String>>,
) -> Result<(), String> {
    use pipewire as pw;
    use pw::spa;
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|e| e.to_string())?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(|e| e.to_string())?;
    let core = context.connect_rc(None).map_err(|e| e.to_string())?;
    let props = pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Playback",
        *pw::keys::MEDIA_ROLE => "Production",
        *pw::keys::NODE_NAME => audivane_io_pw::MEASURE_CARRIER_NODE,
        *pw::keys::NODE_DESCRIPTION => "Audivane measurement",
        *pw::keys::APP_NAME => "Audivane measurement",
        *pw::keys::TARGET_OBJECT => "audivane_dsp",
        *pw::keys::NODE_DONT_RECONNECT => "true",
        *pw::keys::AUDIO_CHANNELS => format!("{channels}"),
    };
    let stream = pw::stream::StreamBox::new(&core, "Audivane measurement", props)
        .map_err(|e| e.to_string())?;
    let stride = 4 * channels as usize;
    let _listener = stream
        .add_local_listener_with_user_data(())
        .process(move |stream, _| {
            if let Some(mut buffer) = stream.dequeue_buffer() {
                let datas = buffer.datas_mut();
                let Some(data) = datas.first_mut() else {
                    return;
                };
                let n = match data.data() {
                    Some(slice) => {
                        slice.fill(0);
                        slice.len() / stride
                    }
                    None => 0,
                };
                let chunk = data.chunk_mut();
                *chunk.offset_mut() = 0;
                *chunk.stride_mut() = stride as _;
                *chunk.size_mut() = (stride * n) as _;
            }
        })
        .register()
        .map_err(|e| e.to_string())?;
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(rate);
    info.set_channels(channels);
    let pod = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(spa::pod::Object {
            type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
            id: spa::param::ParamType::EnumFormat.as_raw(),
            properties: info.into(),
        }),
    )
    .map_err(|e| format!("{e:?}"))?
    .0
    .into_inner();
    let mut params = [spa::pod::Pod::from_bytes(&pod).ok_or("bad format pod")?];
    stream
        .connect(
            spa::utils::Direction::Output,
            None,
            pw::stream::StreamFlags::AUTOCONNECT
                | pw::stream::StreamFlags::MAP_BUFFERS
                | pw::stream::StreamFlags::RT_PROCESS,
            &mut params,
        )
        .map_err(|e| e.to_string())?;
    let _quit = quit.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |()| mainloop.quit()
    });
    let _ = ready.send(Ok(()));
    mainloop.run();
    let _ = stream.disconnect();
    Ok(())
}

// ---------------------------------------------------------------------------
// The run.

/// What the service keeps: the status of the running (or last) run and its
/// stop flag.
#[derive(Default)]
pub struct Measurer {
    inner: Mutex<MeasurerInner>,
}

#[derive(Default)]
struct MeasurerInner {
    status: Arc<Mutex<MeasureStatus>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    runs: u64,
}

/// What a run ends with when `measure_stop` ends it before it played.
const STOPPED: &str = "stopped";

fn running(s: MeasureState) -> bool {
    matches!(
        s,
        MeasureState::Preparing | MeasureState::LeadIn | MeasureState::Playing | MeasureState::Tail
    )
}

impl Measurer {
    pub fn status(&self) -> MeasureStatus {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.status.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Stop the running measurement and wait for it to end (the fade, every
    /// device's drain and the mute's ramp): the reply is its final status.
    pub fn stop(&self) -> MeasureStatus {
        let thread = {
            let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            g.stop.store(true, Ordering::Release);
            g.thread.take()
        };
        if let Some(t) = thread {
            let _ = t.join();
        }
        self.status()
    }

    /// Check the request, load the file and start a run.
    pub fn play(
        &self,
        engine: Arc<dyn Engine>,
        cfg: EngineConfig,
        req: MeasurePlayRequest,
    ) -> Result<MeasureStatus, EngineError> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if running(g.status.lock().unwrap_or_else(|e| e.into_inner()).state) {
            return Err(bad_state("a measurement is running (measure_stop ends it)"));
        }
        if let Some(t) = g.thread.take() {
            let _ = t.join();
        }
        let channels = crate::engine::DEFAULT_CHANNELS;
        let wav = read_wav(&req.file).map_err(invalid)?;
        let routes = check_request(&req, &cfg, channels, &wav)?;
        if !cfg.dsp_enabled {
            return Err(bad_state(
                "measure_play needs DSP on (bypass skips the chain's stages anyway)",
            ));
        }
        if cfg.varispeed.enabled {
            return Err(bad_state("measure_play needs varispeed off"));
        }
        g.runs += 1;
        let frames = wav.channels[0].len() as u64;
        let status = MeasureStatus {
            run: g.runs,
            state: MeasureState::Preparing,
            error: None,
            file: Some(req.file.clone()),
            chain: Some(req.chain),
            mute_others: req.mute_others,
            keepalive_db: req.keepalive_db,
            lead_in_ms: req.lead_in_ms,
            file_rate_hz: wav.rate,
            file_frames: frames,
            duration_s: frames as f64 / f64::from(wav.rate),
            ..MeasureStatus::default()
        };
        let shared = Arc::new(Mutex::new(status.clone()));
        let stop = Arc::new(AtomicBool::new(false));
        let run = Run {
            engine,
            req,
            wav,
            routes,
            status: Arc::clone(&shared),
            stop: Arc::clone(&stop),
            channels,
        };
        let thread = std::thread::Builder::new()
            .name("audivane-measure".into())
            .spawn(move || run.run())
            .map_err(|e| EngineError::new(RpcError::INTERNAL, format!("measure thread: {e}")))?;
        g.status = shared;
        g.stop = stop;
        g.thread = Some(thread);
        Ok(status)
    }
}

impl Drop for Measurer {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

struct Run {
    engine: Arc<dyn Engine>,
    req: MeasurePlayRequest,
    wav: Wav,
    routes: DeviceRoutes,
    status: Arc<Mutex<MeasureStatus>>,
    stop: Arc<AtomicBool>,
    channels: usize,
}

/// Where each device takes the file.
#[derive(Clone, Debug)]
struct Plan {
    graph_rate: u32,
    ratio: RateRatio,
    output_rate: u32,
    /// The main device's name.
    master: String,
    /// Per device in `routes` order: (role text, device name, output rate).
    devices: Vec<(String, String, u32)>,
}

impl Run {
    fn set(&self, f: impl FnOnce(&mut MeasureStatus)) {
        let mut g = self.status.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut g);
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    fn run(self) {
        let carrier = match Carrier::start(self.wav.rate, self.channels as u32) {
            Ok(c) => Some(c),
            Err(e) => {
                self.fail(format!("could not start the measurement stream: {e}"));
                return;
            }
        };
        let result = self.prepare().and_then(|job| {
            if let Err(e) = self.engine.measure_post(Some(Arc::clone(&job))) {
                return Err(e.message);
            }
            let r = self.follow(&job);
            let _ = self.engine.measure_post(None);
            self.retire(job);
            r
        });
        drop(carrier);
        match result {
            Ok(state) => self.set(|s| s.state = state),
            Err(e) => self.fail(e),
        }
    }

    /// Let go of a job the audio threads are handing back: they never free
    /// it, so it is dropped here (or by the engine's reaper) once they have.
    fn retire(&self, job: Arc<MeasureJob>) {
        let deadline = Instant::now() + RETIRE_WAIT;
        loop {
            self.engine.measure_reclaim();
            if Arc::strong_count(&job) == 1 {
                return;
            }
            if Instant::now() >= deadline {
                log::warn!("measure: an audio thread still holds the job; freed later");
                self.engine.measure_retire(job);
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// End the run with `e`: `stopped` when a stop ended it before it
    /// played; anything else (a fault, a drain timeout, even after a stop)
    /// is an `error` with its message.
    fn fail(&self, e: String) {
        if e == STOPPED && self.stopped() {
            self.set(|s| s.state = MeasureState::Stopped);
            return;
        }
        log::warn!("measure: {e}");
        self.set(|s| {
            s.state = MeasureState::Error;
            s.error = Some(e);
        });
    }

    /// Wait for the engine and the followers, then build the job.
    fn prepare(&self) -> Result<Arc<MeasureJob>, String> {
        let mut gate = ReadyGate::new(Instant::now());
        let plan = loop {
            if self.stopped() {
                return Err(STOPPED.into());
            }
            let snap = self.engine.snapshot();
            let outputs = self.engine.outputs().unwrap_or_default();
            let check = readiness(&snap, &outputs, &self.routes, self.req.chain)?;
            if let Some(plan) = gate.step(Instant::now(), check)? {
                break plan;
            }
            std::thread::sleep(Duration::from_millis(50));
        };

        // The lead-in in graph frames, a whole number of the ratio's
        // `down` (so it is whole at the output rate too).
        let down = u64::from(plan.ratio.down.max(1));
        let lead_graph = ((self.req.lead_in_ms * 1e-3 * f64::from(plan.graph_rate) / down as f64)
            .ceil() as u64)
            * down;
        let injection_rate = |dev: &Option<String>| {
            if dev.is_none() && self.req.chain == MeasureChain::Bypass {
                plan.output_rate
            } else {
                plan.graph_rate
            }
        };
        // The buffers' size before any is built (a resample grows them).
        let projected = projected_samples(
            self.wav.channels[0].len(),
            self.wav.rate,
            self.routes
                .iter()
                .map(|(dev, chans)| (injection_rate(dev), chans.len())),
        );
        if projected > MAX_SAMPLES as u128 {
            return Err(format!(
                "the file is too long for these routes at {} Hz ({} M samples for every device together, at most {} M)",
                plan.graph_rate.max(plan.output_rate),
                projected >> 20,
                MAX_SAMPLES >> 20
            ));
        }
        let limit = max_peak_dbfs(self.req.chain);
        let mut devices = Vec::new();
        let mut notes = Vec::new();
        let mut total = 0usize;
        let mut timing = Vec::new();
        for ((dev, chans), (role, name, out_rate)) in self.routes.iter().zip(&plan.devices) {
            let master_bypass = dev.is_none() && self.req.chain == MeasureChain::Bypass;
            let rate = injection_rate(dev);
            let lead = if master_bypass {
                lead_graph * u64::from(plan.ratio.up) / down
            } else {
                lead_graph
            };
            let resampled = rate != self.wav.rate;
            if resampled {
                notes.push(format!(
                    "{name}: the file was resampled from {} to {rate} Hz",
                    self.wav.rate
                ));
            }
            let mut routes = Vec::new();
            for &(channel, src) in chans {
                let x = &self.wav.channels[src];
                let samples = if resampled {
                    audivane_dsp_core::resample::resample_offline(x, self.wav.rate, rate)
                } else {
                    x.clone()
                };
                let p = peak_dbfs(&samples);
                if p > limit {
                    return Err(format!(
                        "after resampling to {rate} Hz the file peaks at {p:.2} dBFS on {name} channel {channel}, above {limit} dBFS"
                    ));
                }
                total += samples.len();
                if total > MAX_SAMPLES {
                    return Err(format!(
                        "the file is too long for {rate} Hz (more than {} M samples for every device together)",
                        MAX_SAMPLES >> 20
                    ));
                }
                routes.push((channel, samples));
            }
            timing.push(MeasureDeviceTiming {
                device: name.clone(),
                role: role.clone(),
                channels: chans.iter().map(|(c, _)| *c as u32).collect(),
                rate_hz: *out_rate,
                injection_rate_hz: rate,
                resampled,
                latency_frames: 0.0,
                latency_us: 0.0,
                chain_latency_frames: 0.0,
                queue_frames: 0.0,
                start_frame: None,
                start_time_ns: None,
                frames_played: 0,
                clamped_samples: 0,
                drained: false,
                continuity: None,
            });
            let readings = readings_capacity(
                self.req.lead_in_ms,
                self.wav.channels[0].len() as f64 / f64::from(self.wav.rate),
            );
            devices.push(
                MeasureDevice::new(dev.clone(), rate, *out_rate, routes, lead)
                    .with_readings(readings),
            );
        }
        let keepalive = self
            .req
            .keepalive_db
            .map_or(0.0, |db| 10f64.powf(db / 20.0));
        let mut job = MeasureJob::new(
            self.req.chain,
            self.req.mute_others,
            keepalive,
            lead_graph,
            plan.graph_rate,
            devices,
        );
        job.master_device = Some(plan.master);
        let job = Arc::new(job);
        self.set(|s| {
            s.graph_rate_hz = plan.graph_rate;
            s.devices = timing;
            s.notes = notes;
        });
        Ok(job)
    }

    /// Publish the job's progress and timing.
    fn publish(&self, job: &MeasureJob, state: MeasureState) {
        self.set(|s| {
            s.state = state;
            s.source_frame = job.s0();
            let mut progress = f64::INFINITY;
            for (t, d) in s.devices.iter_mut().zip(&job.devices) {
                let pos = d.pos.load(Ordering::Relaxed);
                let played = if pos == i64::MIN {
                    0
                } else {
                    pos.max(0) as u64
                };
                t.frames_played = played;
                progress = progress.min(played as f64 / f64::from(d.rate.max(1)));
                if let Some(sf) = d.start_frame() {
                    t.start_frame = Some(sf.round() as u64);
                }
                let ts = d.start_time_ns.load(Ordering::Acquire);
                t.start_time_ns = (ts != 0).then_some(ts);
                t.chain_latency_frames = d.chain();
                t.clamped_samples = d.clamped.load(Ordering::Relaxed);
                t.drained = d.done.load(Ordering::Acquire);
                if let Some(q) = d.queue() {
                    t.queue_frames = q;
                    t.latency_frames = t.chain_latency_frames + q;
                    t.latency_us = t.latency_frames * 1e6 / f64::from(t.rate_hz.max(1));
                }
            }
            s.progress_s = if progress.is_finite() { progress } else { 0.0 };
        });
    }

    /// How long the devices may take to drain after the injection's end:
    /// the longest processing and queue (an estimate until read), and a
    /// margin.
    fn drain_budget(&self, job: &MeasureJob) -> Duration {
        let s = self.status.lock().unwrap_or_else(|e| e.into_inner());
        let longest = job
            .devices
            .iter()
            .zip(&s.devices)
            .map(|(d, t)| {
                let rate = f64::from(t.rate_hz.max(1));
                // From the injection's end to the device's last touched
                // frame (the chain's whole tail included), then the queue.
                let queue = d.queue().unwrap_or(0.2 * rate);
                (d.tail() + queue) / rate
            })
            .fold(0.0f64, f64::max);
        Duration::from_secs_f64(longest.min(60.0)) + DRAIN_MARGIN
    }

    /// Follow a posted job to its end: the state it ended in. It ends once
    /// every device's DAC has played the measurement out (or the run gave
    /// up on one), and the master's mute is back up.
    fn follow(&self, job: &Arc<MeasureJob>) -> Result<MeasureState, String> {
        let posted = Instant::now();
        let mut last_progress = (Instant::now(), i64::MIN);
        let mut stall: Option<String> = None;
        // When the injection ended everywhere (or the run began to end).
        let mut fed_at: Option<Instant> = None;
        let mut release_at: Option<Instant> = None;
        let mut undrained: Vec<String> = Vec::new();
        loop {
            std::thread::sleep(Duration::from_millis(10));
            let snap = self.engine.snapshot();
            if self.stopped() {
                job.cancel.store(true, Ordering::Release);
            }
            let ending = job.ending();
            let armed = job.s0().is_some();
            if !armed && !ending && posted.elapsed() > ARM_TIMEOUT {
                // Nothing played (the mute never fell).
                return Err(format!(
                    "the engine did not start the measurement within {} s ({:?} on {:?})",
                    ARM_TIMEOUT.as_secs(),
                    snap.state,
                    snap.active_sink
                ));
            }
            let fed = job.devices.iter().all(|d| {
                d.fed.load(Ordering::Acquire) || (ending && !d.started.load(Ordering::Acquire))
            });
            // Progress: the slowest device's position must move while it
            // injects (the audio thread stopped: a lost device, a stop).
            let pos = job
                .devices
                .iter()
                .filter(|d| !d.fed.load(Ordering::Relaxed))
                .map(|d| d.pos.load(Ordering::Relaxed))
                .min()
                .unwrap_or(i64::MAX);
            if pos != last_progress.1 {
                last_progress = (Instant::now(), pos);
            } else if armed && !fed && !ending && last_progress.0.elapsed() > STALL_TIMEOUT {
                job.set_fault(MeasureFault::Interrupted);
                stall = Some(format!(
                    "playback stopped ({:?}: {})",
                    snap.state,
                    snap.error.clone().unwrap_or_else(|| "no progress".into())
                ));
                continue;
            }
            if (fed || ending) && fed_at.is_none() {
                fed_at = Some(Instant::now());
            }
            match release_at {
                None => {
                    let drained = job.devices.iter().all(|d| d.finished(ending));
                    // (Read again each time: a follower knows its tail once
                    // its start reached the output.)
                    let late = fed_at.is_some_and(|t| t.elapsed() >= self.drain_budget(job));
                    if drained || late {
                        if !drained {
                            undrained = self.undrained(job, ending);
                            log::warn!(
                                "measure: {} did not drain; lifting the mute anyway",
                                undrained.join(", ")
                            );
                        }
                        job.release.store(true, Ordering::Release);
                        release_at = Some(Instant::now());
                    }
                }
                Some(t) => {
                    if job.released.load(Ordering::Acquire) || t.elapsed() > RELEASE_TIMEOUT {
                        self.publish(job, MeasureState::Tail);
                        self.check_continuity(job);
                        self.set(|s| {
                            for t in &s.devices {
                                if t.clamped_samples > 0 {
                                    let n = format!(
                                        "{}: {} samples were clamped at -1 dBFS",
                                        t.device, t.clamped_samples
                                    );
                                    s.notes.push(n);
                                }
                            }
                        });
                        let fault = job.any_fault();
                        return if let Some(e) = stall {
                            Err(e)
                        } else if fault != MeasureFault::None {
                            Err(fault.text().to_string())
                        } else if !undrained.is_empty() {
                            Err(format!(
                                "{} did not finish playing the file within {:.1} s of its end",
                                undrained.join(", "),
                                self.drain_budget(job).as_secs_f64()
                            ))
                        } else if job.cancel.load(Ordering::Acquire) {
                            Ok(MeasureState::Stopped)
                        } else {
                            self.publish(job, MeasureState::Done);
                            Ok(MeasureState::Done)
                        };
                    }
                    continue;
                }
            }
            let state = if !armed
                || job
                    .devices
                    .iter()
                    .all(|d| d.pos.load(Ordering::Relaxed) < 0)
            {
                MeasureState::LeadIn
            } else if fed || ending {
                MeasureState::Tail
            } else {
                MeasureState::Playing
            };
            self.publish(job, state);
        }
    }

    /// The playback continuity check of every device, into the status and
    /// its notes.
    fn check_continuity(&self, job: &MeasureJob) {
        let checks: Vec<Option<MeasureContinuity>> = {
            let s = self.status.lock().unwrap_or_else(|e| e.into_inner());
            job.devices
                .iter()
                .zip(&s.devices)
                .map(|(d, t)| {
                    continuity(
                        &d.readings.snapshot(),
                        t.rate_hz,
                        t.start_time_ns,
                        d.readings.dropped(),
                    )
                })
                .collect()
        };
        self.set(|s| {
            for (t, c) in s.devices.iter_mut().zip(checks) {
                if let Some(c) = &c {
                    if let Some(n) = continuity_note(&t.device, c) {
                        log::warn!("measure: {n}");
                        s.notes.push(n);
                    }
                    if c.dropped > 0 {
                        s.notes.push(format!(
                            "{}: the continuity check covers its first {} readings ({} more found no room)",
                            t.device, c.readings, c.dropped
                        ));
                    }
                }
                t.continuity = c;
            }
        });
    }

    /// The devices still playing the measurement out.
    fn undrained(&self, job: &MeasureJob, ending: bool) -> Vec<String> {
        let s = self.status.lock().unwrap_or_else(|e| e.into_inner());
        job.devices
            .iter()
            .zip(&s.devices)
            .filter(|(d, _)| !d.finished(ending))
            .map(|(_, t)| t.device.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use audivane_protocol::ipc::{FollowerConfig, MeasureRoute};

    fn wav(channels: usize, peak: f64) -> Wav {
        Wav {
            rate: 48_000,
            channels: vec![vec![peak; 100]; channels],
        }
    }

    fn req(routes: &[(&str, u32)]) -> MeasurePlayRequest {
        MeasurePlayRequest {
            file: "x.wav".into(),
            routes: routes
                .iter()
                .map(|(d, c)| MeasureRoute {
                    device: d.to_string(),
                    channel: *c,
                })
                .collect(),
            chain: MeasureChain::Bypass,
            mute_others: true,
            keepalive_db: Some(-100.0),
            lead_in_ms: 2000.0,
        }
    }

    fn cfg() -> EngineConfig {
        let mut c = EngineConfig {
            device: "hw:CARD=USB,DEV=0".into(),
            ..EngineConfig::default()
        };
        c.outputs.followers.push(FollowerConfig {
            device: "hw:CARD=USB_1,DEV=0".into(),
            identity: None,
            enabled: true,
            trim_ms: 0.0,
            volume_db: 0.0,
            output_channels: None,
            follow_volume: false,
            follow_ref_db: None,
            chain_patch: None,
            period_ms: None,
        });
        c
    }

    #[test]
    fn routes_resolve_to_devices_and_file_channels() {
        let r = check_request(
            &req(&[
                ("hw:CARD=USB_1,DEV=0", 0),
                ("master", 1),
                ("hw:CARD=USB,DEV=0", 0),
            ]),
            &cfg(),
            2,
            &wav(3, 0.5),
        )
        .unwrap();
        assert_eq!(
            r,
            vec![
                (None, vec![(1, 1), (0, 2)]),
                (Some("hw:CARD=USB_1,DEV=0".to_string()), vec![(0, 0)]),
            ]
        );
        // A mono file plays on every route.
        let r = check_request(
            &req(&[("master", 0), ("master", 1)]),
            &cfg(),
            2,
            &wav(1, 0.5),
        )
        .unwrap();
        assert_eq!(r, vec![(None, vec![(0, 0), (1, 0)])]);
    }

    #[test]
    fn bad_requests_are_refused() {
        let c = cfg();
        let err = |r: MeasurePlayRequest, w: Wav| check_request(&r, &c, 2, &w).unwrap_err().message;
        assert!(err(req(&[("master", 0)]), wav(1, 0.95)).contains("peaks at -0.45 dBFS"));
        // Bypass leaves 3 dB; active 1 dB.
        let x = 10f64.powf(-2.5 / 20.0);
        assert!(err(req(&[("master", 0)]), wav(1, x)).contains("above -3 dBFS"));
        let mut r = req(&[("master", 0)]);
        r.chain = MeasureChain::Active;
        assert!(check_request(&r, &c, 2, &wav(1, x)).is_ok());
        assert!(err(req(&[("master", 2)]), wav(1, 0.5)).contains("channel 2"));
        assert!(err(req(&[("hw:CARD=X,DEV=0", 0)]), wav(1, 0.5)).contains("neither"));
        assert!(err(req(&[("master", 0), ("master", 0)]), wav(1, 0.5)).contains("twice"));
        assert!(err(req(&[("master", 0)]), wav(2, 0.5)).contains("2 channels"));
        assert!(err(req(&[]), wav(1, 0.5)).contains("empty"));
        let mut r = req(&[("master", 0)]);
        r.keepalive_db = Some(-20.0);
        assert!(err(r, wav(1, 0.5)).contains("keepalive_db"));
        let mut r = req(&[("master", 0)]);
        r.lead_in_ms = 100.0;
        assert!(err(r, wav(1, 0.5)).contains("lead_in_ms"));
        let mut c2 = cfg();
        c2.outputs.followers[0].enabled = false;
        assert!(
            check_request(&req(&[("hw:CARD=USB_1,DEV=0", 0)]), &c2, 2, &wav(1, 0.5))
                .unwrap_err()
                .message
                .contains("disabled")
        );
    }

    #[test]
    fn wav_files_read_exactly() {
        let dir = std::env::temp_dir().join(format!("audivane-measure-wav-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("a.wav");
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 44_100,
            bits_per_sample: 24,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(&p, spec).unwrap();
        for s in [1i32, -2, 8_388_607, -8_388_608] {
            w.write_sample(s).unwrap();
        }
        w.finalize().unwrap();
        let x = read_wav(p.to_str().unwrap()).unwrap();
        assert_eq!(x.rate, 44_100);
        assert_eq!(
            x.channels[0],
            vec![1.0 / 8_388_608.0, 8_388_607.0 / 8_388_608.0]
        );
        assert_eq!(x.channels[1], vec![-2.0 / 8_388_608.0, -1.0]);
        // Exact at the left-justified word.
        assert_eq!((x.channels[0][0] * 2_147_483_648.0) as i32, 1 << 8);
        // A float file with a NaN is refused.
        let q = dir.join("nan.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48_000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut w = hound::WavWriter::create(&q, spec).unwrap();
        for s in [0.1f32, f32::NAN, 0.2] {
            w.write_sample(s).unwrap();
        }
        w.finalize().unwrap();
        let e = read_wav(q.to_str().unwrap()).unwrap_err();
        assert!(e.contains("sample 1") && e.contains("finite"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn snap() -> crate::api::EngineSnapshot {
        crate::api::EngineSnapshot {
            state: EngineState::Playing,
            device: "hw:CARD=USB,DEV=0".into(),
            error: None,
            format: None,
            graph_rate: 48_000,
            output_rate: 48_000,
            ratio: RateRatio::ONE,
            quantum: 1024,
            latency_ms: 5.0,
            filter_latency_ms: 0.0,
            conv_latency_ms: 0.0,
            xruns: 0,
            dsp_load: 0.0,
            dsp_active: true,
            active_sink: Some(SinkChoice::Dsp),
            ab: None,
            volume: None,
            dsp_node: None,
            direct_node: None,
            frames_out: 0,
            chain_generation: 0,
            gaps: Default::default(),
            device_caps: None,
            effective_rates: Vec::new(),
            direct_rates: Vec::new(),
            playable_rates: Vec::new(),
            effective_default_rate: None,
            dither_bits: None,
            bypass_resampler: None,
            rt_load_max: 0.0,
            varispeed: None,
            waiting_for_device: false,
        }
    }

    /// The outputs with the one follower of `cfg()` in `state`.
    fn outputs(state: FollowerState) -> audivane_protocol::ipc::OutputsStatus {
        let mut o = audivane_protocol::ipc::OutputsStatus::from_config(&cfg().outputs);
        o.followers[0].state = state;
        o.followers[0].device_rate = 48_000;
        o
    }

    const FOLLOWER: &str = "hw:CARD=USB_1,DEV=0";

    fn routes_to(follower: bool) -> DeviceRoutes {
        if follower {
            vec![(Some(FOLLOWER.to_string()), vec![(0, 0)])]
        } else {
            vec![(None, vec![(0, 0)])]
        }
    }

    /// Run the gate every 50 ms for `secs` on what `at` says at each time:
    /// the time it armed (or the error).
    fn run_gate(
        secs: f64,
        mut at: impl FnMut(f64) -> (audivane_protocol::ipc::OutputsStatus, DeviceRoutes),
    ) -> Result<f64, String> {
        let t0 = Instant::now();
        let mut gate = ReadyGate::new(t0);
        let mut t = 0.0;
        while t <= secs {
            let (o, routes) = at(t);
            let r = readiness(&snap(), &o, &routes, MeasureChain::Active)?;
            if gate.step(t0 + Duration::from_secs_f64(t), r)?.is_some() {
                return Ok(t);
            }
            t += 0.05;
        }
        Err("still waiting".into())
    }

    /// A run routed to a follower that stays unheard (locking, or muted)
    /// ends with an error naming it and its state, not a silent capture.
    #[test]
    fn a_routed_follower_that_stays_unheard_ends_the_run() {
        for (state, what) in [
            (FollowerState::Locking, "locking"),
            (FollowerState::Muted, "muted"),
            (FollowerState::Opening, "opening"),
        ] {
            let e = run_gate(20.0, |_| (outputs(state), routes_to(true))).unwrap_err();
            assert!(e.contains(FOLLOWER) && e.contains(what), "{e}");
            assert!(e.contains("not heard after 10 s"), "{e}");
        }
        // Disabled, it says so.
        let e = run_gate(20.0, |_| {
            let mut o = outputs(FollowerState::Muted);
            o.followers[0].enabled = false;
            (o, routes_to(true))
        })
        .unwrap_err();
        assert!(e.contains("disabled"), "{e}");
        // Heard after 4 s: it arms half a second later.
        let t = run_gate(20.0, |t| {
            let st = if t < 4.0 {
                FollowerState::Locking
            } else {
                FollowerState::Locked
            };
            (outputs(st), routes_to(true))
        })
        .unwrap();
        assert!((4.4..4.7).contains(&t), "{t}");
    }

    /// A master-only run waits while an unrouted follower settles (it
    /// moves the master's alignment), then arms; one that never settles
    /// ends the run after 30 s with the reason.
    #[test]
    fn a_master_run_waits_for_the_alignment_to_settle() {
        let t = run_gate(40.0, |t| {
            let mut o = outputs(if t < 3.0 {
                FollowerState::Locking
            } else {
                FollowerState::Locked
            });
            // The alignment grows at 3 s and 6 s, each change reported
            // as settling for 3 s.
            o.alignment_ms = if t < 3.0 {
                0.0
            } else if t < 6.0 {
                2.0
            } else {
                4.0
            };
            if t < 9.0 {
                o.settling = Some("the master's alignment changed".into());
            }
            (o, routes_to(false))
        })
        .unwrap();
        assert!((9.4..9.7).contains(&t), "{t}");
        // The follower's state alone does not hold a master run (the engine
        // reports it in `settling` when it matters).
        let t = run_gate(5.0, |_| (outputs(FollowerState::Muted), routes_to(false))).unwrap();
        assert!(t < 0.7, "{t}");
        let e = run_gate(40.0, |_| {
            let mut o = outputs(FollowerState::Locking);
            o.settling = Some(format!("follower {FOLLOWER} is locking"));
            (o, routes_to(false))
        })
        .unwrap_err();
        assert!(
            e.contains("did not settle within 30 s") && e.contains("is locking"),
            "{e}"
        );
    }

    /// A rate (or ratio) that keeps changing ends the wait at the 30 s
    /// deadline instead of preparing for ever; so does a routed follower
    /// whose 10 s keep restarting with alignment moves.
    #[test]
    fn the_deadline_holds_on_every_waiting_path() {
        let t0 = Instant::now();
        let mut gate = ReadyGate::new(t0);
        let mut t = 0.0;
        let err = loop {
            let mut s = snap();
            s.graph_rate = if (t * 10.0) as u64 % 6 < 3 {
                48_000
            } else {
                44_100
            };
            let o = outputs(FollowerState::Locked);
            let r = readiness(&s, &o, &routes_to(false), MeasureChain::Active).unwrap();
            match gate.step(t0 + Duration::from_secs_f64(t), r) {
                Ok(None) => {}
                Ok(Some(_)) => panic!("armed on a moving rate at {t}"),
                Err(e) => break (t, e),
            }
            t += 0.05;
            assert!(t < 40.0, "still preparing");
        };
        assert!((30.0..30.2).contains(&err.0), "{err:?}");
        assert!(err.1.contains("not steady"), "{}", err.1);
        let e = run_gate(60.0, |t| {
            let mut o = outputs(FollowerState::Locking);
            o.alignment_ms = (t / 8.0).floor();
            (o, routes_to(true))
        })
        .unwrap_err();
        assert!(e.contains("within 30 s") && e.contains("locking"), "{e}");
    }

    /// A routed follower that re-acquires after the alignment moves gets
    /// its 10 s again from the move.
    #[test]
    fn an_alignment_move_restarts_the_audible_wait() {
        let t = run_gate(40.0, |t| {
            let mut o = outputs(if t < 14.0 {
                FollowerState::Locking
            } else {
                FollowerState::Locked
            });
            o.alignment_ms = if t < 8.0 { 0.0 } else { 3.0 };
            if t < 11.0 {
                o.settling = Some("moving".into());
            }
            (o, routes_to(true))
        })
        .unwrap();
        assert!((14.4..14.7).contains(&t), "{t}");
    }

    /// The fit takes a device clock 80 ppm off the host's, and readings
    /// with 60 µs of jitter, as steady (no note); a 15 ms step back (a
    /// buffer-sized step) shows with its size and time.
    #[test]
    fn the_continuity_check_sees_a_step_and_not_drift_or_jitter() {
        let rate = 768_000u32;
        let ppm = 80e-6;
        // A reading every 1.33 ms (a 1024-frame period) for 30 s, kept one
        // per 2 ms as the audio thread does.
        let mut jitter = 0x1234_5678u64;
        let mut readings = |step: Option<(f64, i64)>| {
            let mut r: Vec<(i64, u64)> = Vec::new();
            let mut t = 5e9;
            while t < 35e9 {
                jitter = jitter
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1);
                let j = ((jitter >> 33) as f64 / (1u64 << 31) as f64 - 0.5) * 120e3;
                let secs = (t - 5e9) * 1e-9;
                let mut pos = (secs * f64::from(rate) * (1.0 + ppm)) as i64;
                if let Some((at, n)) = step
                    && secs >= at
                {
                    pos += n;
                }
                let time = (t + j) as u64;
                if r.last().is_none_or(|l| time >= l.1 + READING_INTERVAL_NS) {
                    r.push((pos, time));
                }
                t += 1024.0 / f64::from(rate) * 1e9;
            }
            r
        };
        let clean = continuity(&readings(None), rate, Some(6_000_000_000), 0).unwrap();
        assert!(clean.max_residual_us < 100.0, "{clean:?}");
        assert!(clean.jump_us.abs() < 150.0, "{clean:?}");
        assert_eq!(continuity_note("hw:CARD=USB,DEV=0", &clean), None);
        let n = -(0.015 * f64::from(rate)) as i64;
        let stepped = continuity(&readings(Some((20.0, n))), rate, Some(6_000_000_000), 0).unwrap();
        assert!(
            (stepped.jump_frames - n as f64).abs() < 0.2e-3 * f64::from(rate),
            "{stepped:?}"
        );
        assert!((stepped.jump_at_s - 19.0).abs() < 0.01, "{stepped:?}");
        let note = continuity_note("hw:CARD=USB,DEV=0", &stepped).unwrap();
        assert!(note.contains("jumped by -115"), "{note}");
        assert_eq!(stepped.strays, 0);
        // Too few readings: no check.
        assert_eq!(continuity(&[(0, 0), (1, 1)], rate, None, 0), None);
        // Room for a 600 s file and its lead-in: some 335 k readings.
        assert_eq!(readings_capacity(10_000.0, 600.0), 336_064);
    }

    /// Readings every 4 ms at 768 kHz for 600 s, the device's
    /// clock drifting smoothly from 0 to 20 ppm off the host's. No step and
    /// no stray: no note. A lone reading 5 ms late (at the start, or mid
    /// run) is a stray, set aside, not a shift.
    #[test]
    fn smooth_drift_and_a_lone_stray_are_not_steps() {
        let rate = 768_000.0f64;
        let mut jitter = 0x9e37_79b9u64;
        let readings = |late: &[usize], jitter: &mut u64| -> Vec<(i64, u64)> {
            (0..150_000usize)
                .map(|i| {
                    let t = i as f64 * 4e-3;
                    *jitter = jitter
                        .wrapping_mul(6_364_136_223_846_793_005)
                        .wrapping_add(1);
                    let j = ((*jitter >> 33) as f64 / (1u64 << 31) as f64 - 0.5) * 40e-6;
                    let pos = rate * (t + 0.5 * 20e-6 * t * t / 600.0);
                    let off = if late.contains(&i) { 5e-3 } else { 0.0 };
                    (pos as i64, ((1.0 + t + j + off) * 1e9) as u64)
                })
                .collect()
        };
        for late in [&[][..], &[2][..], &[75_000][..], &[2, 75_000][..]] {
            let c = continuity(
                &readings(late, &mut jitter),
                768_000,
                Some(1_000_000_000),
                0,
            )
            .unwrap();
            assert_eq!(
                continuity_note("hw:CARD=USB,DEV=0", &c),
                None,
                "{late:?}: {c:?}"
            );
            assert!(c.jump_us.abs() < 100.0, "{late:?}: {c:?}");
            assert!(c.max_residual_us < 100.0, "{late:?}: {c:?}");
            assert_eq!(c.strays as usize, late.len(), "{late:?}: {c:?}");
        }
        // The same drift with a 2 ms step at 300 s: found, at its place.
        let mut r = readings(&[], &mut jitter);
        for x in &mut r[75_000..] {
            x.0 += 1536;
        }
        let c = continuity(&r, 768_000, Some(1_000_000_000), 0).unwrap();
        assert!((c.jump_frames - 1536.0).abs() < 30.0, "{c:?}");
        assert!((c.jump_at_s - 300.0).abs() < 0.01, "{c:?}");
        assert!(continuity_note("x", &c).unwrap().contains("jumped by"));
    }

    /// The size check runs on the resampled size, before any buffer.
    #[test]
    fn the_projected_size_counts_the_resampled_buffers() {
        // 600 s mono at 44.1 kHz on two routes at 768 kHz: about 879 M.
        let n = projected_samples(600 * 44_100, 44_100, [(768_000, 2)].into_iter());
        assert!(n > MAX_SAMPLES as u128);
        assert_eq!(n, 2 * (600 * 768_000 + 1));
        let n = projected_samples(48_000, 48_000, [(48_000, 1), (96_000, 1)].into_iter());
        assert_eq!(n, 48_001 + 96_001);
    }
}
