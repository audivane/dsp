//! Real position readings: logs from `io-alsa`'s `drift_capture` example,
//! what they say about each device
//! (its rate, its reading noise, its pointer steps), and the detector and
//! the loop run on them offline. Offline only (it allocates).
//!
//! **The replay.** The devices played silence at their own rates, so the
//! follower was never corrected. The replay corrects it on paper: the
//! follower's [`PositionMap`] gets, for each period written, the source
//! positions it would have carried with the loop's ε (and its skips), while
//! the readings that place the device frames in time are the real ones. So
//! the detector sees the real reading noise, timing and pointer steps,
//! and the loop closes through the map exactly as on a device.
//!
//! **Truth.** Each device's frame at the DAC over time is a smooth fit of
//! its own readings ([`Fit`]: a polynomial over the run, which keeps the
//! clock's offset and slow wander and drops the reading noise). The true
//! error is the master's fitted frame less the source position the
//! follower's map gives its fitted frame.

use super::sim::{SEGMENT, sidebands};
use super::{DriftLoop, FRAME, LoopConfig, PhaseDetector, PositionMap, Reading, hermite};

const FRAME_F: f64 = 18_446_744_073_709_551_616.0;

/// One logged wakeup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Logged {
    /// Frames given before this wakeup's write.
    pub written: u64,
    /// The ALSA delay from the status call.
    pub delay: i64,
    /// The status call's timestamp, and the clock read just before and
    /// after the call, ns (`CLOCK_MONOTONIC_RAW`).
    pub htstamp_ns: i64,
    pub before_ns: i64,
    pub after_ns: i64,
}

/// Which time a reading is placed at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeSource {
    /// The status call's own timestamp.
    Htstamp,
    /// The clock read just before the status call (what a reader without
    /// the driver's timestamp has).
    Before,
}

impl Logged {
    pub fn played(&self) -> i64 {
        self.written as i64 - self.delay
    }

    pub fn time_ns(&self, src: TimeSource) -> i64 {
        match src {
            TimeSource::Htstamp => self.htstamp_ns,
            TimeSource::Before => self.before_ns,
        }
    }

    pub fn reading(&self, src: TimeSource) -> Reading {
        Reading {
            written: self.written,
            delay: self.delay,
            time_ns: self.time_ns(src),
        }
    }
}

/// A device's log.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Log {
    pub rate: f64,
    pub period: usize,
    pub buffer: usize,
    pub dense: bool,
    pub lines: Vec<Logged>,
}

impl Log {
    /// Parse `drift_capture`'s format: a `#` header with `rate`, `period`,
    /// `buffer` and `dense`, then `written delay htstamp before after avail
    /// xruns` per line.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut log = Log::default();
        for (n, line) in text.lines().enumerate() {
            if let Some(h) = line.strip_prefix('#') {
                let w: Vec<&str> = h.split_whitespace().collect();
                for pair in w.windows(2) {
                    match pair[0] {
                        "rate" => log.rate = pair[1].parse().map_err(|e| format!("rate: {e}"))?,
                        "period" => {
                            log.period = pair[1].parse().map_err(|e| format!("period: {e}"))?
                        }
                        "buffer" => {
                            log.buffer = pair[1].parse().map_err(|e| format!("buffer: {e}"))?
                        }
                        "dense" => log.dense = pair[1] == "true",
                        _ => {}
                    }
                }
                continue;
            }
            let v: Vec<i64> = line
                .split_whitespace()
                .map(|x| x.parse::<i64>())
                .collect::<Result<_, _>>()
                .map_err(|e| format!("line {}: {e}", n + 1))?;
            if v.len() < 5 {
                return Err(format!("line {}: {} fields", n + 1, v.len()));
            }
            log.lines.push(Logged {
                written: v[0] as u64,
                delay: v[1],
                htstamp_ns: v[2],
                before_ns: v[3],
                after_ns: v[4],
            });
        }
        if log.rate <= 0.0 || log.period == 0 || log.lines.is_empty() {
            return Err("missing header or readings".into());
        }
        Ok(log)
    }
}

/// A least-squares polynomial of a device's frame at the DAC against time:
/// `frame(t) = rate · (t − t0) + Σ c_k u^k`, `u = (t − t0) / span`.
#[derive(Clone, Debug, PartialEq)]
pub struct Fit {
    pub t0_ns: i64,
    pub span_s: f64,
    pub rate: f64,
    pub coef: Vec<f64>,
}

impl Fit {
    /// Fit `(time, played)` pairs with a polynomial of `degree` in time
    /// (on top of the nominal `rate`).
    pub fn new(points: &[(i64, i64)], rate: f64, degree: usize) -> Self {
        let t0 = points[0].0;
        let span = ((points[points.len() - 1].0 - t0) as f64 * 1e-9).max(1e-9);
        let m = degree + 1;
        let mut ata = vec![vec![0.0; m]; m];
        let mut atb = vec![0.0; m];
        let p0 = points[0].1 as f64;
        for &(t, f) in points {
            let ts = (t - t0) as f64 * 1e-9;
            let u = ts / span;
            let y = f as f64 - p0 - rate * ts;
            let mut row = vec![1.0; m];
            for k in 1..m {
                row[k] = row[k - 1] * u;
            }
            for i in 0..m {
                atb[i] += row[i] * y;
                for j in 0..m {
                    ata[i][j] += row[i] * row[j];
                }
            }
        }
        let mut coef = solve(ata, atb);
        coef[0] += p0;
        Self {
            t0_ns: t0,
            span_s: span,
            rate,
            coef,
        }
    }

    /// The fitted frame at `t_ns`.
    pub fn at(&self, t_ns: i64) -> f64 {
        let ts = (t_ns - self.t0_ns) as f64 * 1e-9;
        let u = ts / self.span_s;
        let mut acc = 0.0;
        for c in self.coef.iter().rev() {
            acc = acc * u + c;
        }
        self.rate * ts + acc
    }

    /// The clock's mean offset from nominal over the run, ppm (the fitted
    /// slope averaged over the span).
    pub fn ppm(&self) -> f64 {
        let s = self.span_s;
        let d = self.at(self.t0_ns + (s * 1e9) as i64) - self.at(self.t0_ns);
        (d / (self.rate * s) - 1.0) * 1e6
    }
}

fn solve(mut a: Vec<Vec<f64>>, mut b: Vec<f64>) -> Vec<f64> {
    let n = b.len();
    for i in 0..n {
        let p = (i..n)
            .max_by(|&x, &y| a[x][i].abs().total_cmp(&a[y][i].abs()))
            .unwrap_or(i);
        a.swap(i, p);
        b.swap(i, p);
        let d = a[i][i];
        if d.abs() < 1e-300 {
            continue;
        }
        for r in (i + 1)..n {
            let f = a[r][i] / d;
            for c in i..n {
                a[r][c] -= f * a[i][c];
            }
            b[r] -= f * b[i];
        }
    }
    let mut x = vec![0.0; n];
    for i in (0..n).rev() {
        let s: f64 = ((i + 1)..n).map(|j| a[i][j] * x[j]).sum();
        x[i] = if a[i][i].abs() < 1e-300 {
            0.0
        } else {
            (b[i] - s) / a[i][i]
        };
    }
    x
}

/// What one device's readings say.
#[derive(Clone, Debug, PartialEq)]
pub struct DeviceStats {
    pub readings: usize,
    /// The clock's offset from the system clock, ppm.
    pub ppm: f64,
    /// Reading noise: the readings about the fit, frames (standard
    /// deviation, and the peak).
    pub noise_rms: f64,
    pub noise_peak: f64,
    /// The same, from one reading to the next (the differenced residual
    /// over √2: white noise reads the same as `noise_rms`, slow wander
    /// reads less).
    pub noise_diff: f64,
    /// The most common steps of the played frame between consecutive
    /// readings, beyond the period (`step − period`), with their counts.
    pub steps: Vec<(i64, usize)>,
    /// Wakeup lateness: `before − htstamp`, µs (median, 95th percentile,
    /// max).
    pub late_us: (f64, f64, f64),
}

/// The fit of a device's readings: a polynomial of `degree` on top of the
/// nominal rate.
pub fn fit(log: &Log, src: TimeSource, degree: usize) -> Fit {
    let pts: Vec<(i64, i64)> = log
        .lines
        .iter()
        .map(|l| (l.time_ns(src), l.played()))
        .collect();
    Fit::new(&pts, log.rate, degree)
}

/// A device's statistics with readings placed at `src`. Skips the first
/// `warmup` readings (the stream settling).
pub fn device_stats(log: &Log, src: TimeSource, degree: usize, warmup: usize) -> DeviceStats {
    let lines = &log.lines[warmup.min(log.lines.len() - 1)..];
    let sub = Log {
        lines: lines.to_vec(),
        ..log.clone()
    };
    let f = fit(&sub, src, degree);
    let res: Vec<f64> = lines
        .iter()
        .map(|l| l.played() as f64 - f.at(l.time_ns(src)))
        .collect();
    let n = res.len().max(1) as f64;
    let mean = res.iter().sum::<f64>() / n;
    let rms = (res.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / n).sqrt();
    let peak = res.iter().fold(0.0f64, |a, r| a.max((r - mean).abs()));
    let diff = (res.windows(2).map(|w| (w[1] - w[0]).powi(2)).sum::<f64>()
        / (res.len().saturating_sub(1)).max(1) as f64
        / 2.0)
        .sqrt();
    let mut counts: std::collections::BTreeMap<i64, usize> = Default::default();
    for w in lines.windows(2) {
        let step = w[1].played() - w[0].played() - (w[1].written - w[0].written) as i64;
        *counts.entry(step).or_default() += 1;
    }
    let mut steps: Vec<(i64, usize)> = counts.into_iter().collect();
    steps.sort_by_key(|&(_, n)| std::cmp::Reverse(n));
    steps.truncate(6);
    let mut late: Vec<f64> = lines
        .iter()
        .map(|l| (l.before_ns - l.htstamp_ns) as f64 * 1e-3)
        .collect();
    late.sort_by(f64::total_cmp);
    let q = |p: f64| late[((late.len() - 1) as f64 * p).round() as usize];
    DeviceStats {
        readings: lines.len(),
        ppm: f.ppm(),
        noise_rms: rms,
        noise_peak: peak,
        noise_diff: diff,
        steps,
        late_us: (q(0.5), q(0.95), q(1.0)),
    }
}

/// What a replay measured (as [`super::sim::Outcome`], against the fitted
/// truth).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ReplayOutcome {
    /// From when the true error stays within 1 frame of its steady mean, s.
    pub lock_s: f64,
    /// When the loop first said locked, s.
    pub lock_flag_s: f64,
    /// Peak and RMS of the true error about its steady mean, frames.
    pub steady_peak: f64,
    pub steady_rms: f64,
    /// The steady mean of the measured error less the true error: the
    /// readings' relative bias, frames.
    pub reading_bias: f64,
    /// The measured error's spread about the truth in the steady window,
    /// frames RMS (the pair's reading noise as the detector sees it).
    pub detector_noise: f64,
    /// Worst sideband 1 to 100 Hz from a 10 kHz tone, dBc.
    pub sideband_dbc: f64,
    pub sideband_hz: f64,
    /// ε at the end against the fitted clock ratio, ppm.
    pub eps_error_ppm: f64,
    /// The fitted follower-to-master offset, ppm (the follower's clock
    /// relative to the master's, as ε must cancel).
    pub pair_ppm: f64,
    pub skips: usize,
    pub unlocks: usize,
}

/// One period the follower was given: its first device frame, its source
/// position, its frames, and ε (value, slope) at its start and end.
type Span = (u64, f64, u64, (f64, f64), (f64, f64));

/// How a replay runs.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ReplayOptions {
    pub src: TimeSource,
    pub cfg: LoopConfig,
    /// The follower starts this many frames behind.
    pub initial_offset: f64,
    /// The steady window's start, seconds from the follower's first
    /// reading.
    pub steady_from_s: f64,
    /// The truth fits' polynomial degree.
    pub degree: usize,
}

impl Default for ReplayOptions {
    fn default() -> Self {
        Self {
            src: TimeSource::Htstamp,
            cfg: LoopConfig::default(),
            initial_offset: 40.0,
            steady_from_s: 120.0,
            degree: 3,
        }
    }
}

/// Run the detector and the loop on a master's and a follower's logs.
pub fn replay(master: &Log, follower: &Log, opt: &ReplayOptions) -> Result<ReplayOutcome, String> {
    let (src, cfg, initial_offset, steady_from_s) =
        (opt.src, opt.cfg, opt.initial_offset, opt.steady_from_s);
    if master.rate != follower.rate {
        return Err("the replay needs both devices at one rate".into());
    }
    let g = master.rate;
    let fit_m = fit(master, src, opt.degree);
    let fit_f = fit(follower, src, opt.degree);
    let pm = master.period as u64;
    let pf = follower.period as u64;
    let bf = (follower.buffer / follower.period).max(1);
    let period_s = follower.period as f64 / follower.rate;
    let delay_s = (bf as f64 + 0.5) * period_s;
    let mut lp = DriftLoop::new(cfg, g, delay_s).map_err(|e| e.to_string())?;
    let mut det = PhaseDetector::new(g);
    let mut map_m = PositionMap::new();
    let mut map_f = PositionMap::new();

    // The master's frame n carries source n; its buffer before the first
    // reading.
    let first_m = master.lines[0].written;
    let mut w = 0;
    while w < first_m {
        map_m.push(w, i128::from(w) * FRAME);
        map_m.push(w + pm - 1, i128::from(w + pm - 1) * FRAME);
        w += pm;
    }
    // The follower: frame 0 carries the master's fitted frame at the
    // follower's first reading, less its played frame there, less the
    // initial offset.
    let f0 = follower.lines[0];
    let t0 = f0.time_ns(src);
    let base = fit_m.at(t0) - fit_f.at(t0) - initial_offset;
    let mut spans: Vec<Span> = Vec::new();
    let mut pos = base;
    let mut state = (0.0, 0.0);
    let push_f = |map: &mut PositionMap,
                  spans: &mut Vec<Span>,
                  written: u64,
                  to: (f64, f64),
                  skip: i64,
                  pos: &mut f64,
                  state: &mut (f64, f64)| {
        *pos += skip as f64;
        let from = *state;
        let p = pf as f64;
        // The integral of (1 + ε) over the period's frames.
        let mean = 0.5 * (from.0 + to.0) + period_s * (from.1 - to.1) / 12.0;
        let last = hermite(from, to, period_s, period_s * (p - 1.0) / p);
        let mean_last =
            0.5 * (from.0 + last.0) + period_s * (p - 1.0) / p * (from.1 - last.1) / 12.0;
        map.push(written, (*pos * FRAME_F) as i128);
        map.push(
            written + pf - 1,
            ((*pos + (p - 1.0) * (1.0 + mean_last)) * FRAME_F) as i128,
        );
        spans.push((written, *pos, pf, from, to));
        *pos += p * (1.0 + mean);
        *state = to;
    };
    let mut wf = 0;
    while wf < f0.written {
        push_f(
            &mut map_f,
            &mut spans,
            wf,
            (0.0, 0.0),
            0,
            &mut pos,
            &mut state,
        );
        wf += pf;
    }

    let mut updates: Vec<(i64, f64)> = Vec::new();
    let (mut skips, mut unlocks, mut was_locked) = (0usize, 0usize, false);
    let mut lock_flag_s = f64::INFINITY;
    let mut mi = 0usize;
    for l in &follower.lines {
        let t = l.time_ns(src);
        while mi < master.lines.len() && master.lines[mi].time_ns(src) <= t {
            let r = master.lines[mi];
            det.master(r.reading(src), &map_m);
            map_m.push(r.written, i128::from(r.written) * FRAME);
            map_m.push(r.written + pm - 1, i128::from(r.written + pm - 1) * FRAME);
            mi += 1;
        }
        let e = det.follower(l.reading(src), &map_f).unwrap_or(f64::NAN);
        let c = lp.update(e, period_s);
        updates.push((t, e));
        if c.skip != 0 {
            skips += 1;
        }
        if c.locked && !was_locked {
            lock_flag_s = lock_flag_s.min((t - t0) as f64 * 1e-9);
        }
        if !c.locked && was_locked {
            unlocks += 1;
        }
        was_locked = c.locked;
        push_f(
            &mut map_f,
            &mut spans,
            l.written,
            (c.eps, c.eps_slope),
            c.skip,
            &mut pos,
            &mut state,
        );
    }

    // The follower's source position at a (fractional) device frame.
    let source_at = |frame: f64, k: &mut usize| -> f64 {
        while *k + 1 < spans.len() && (spans[*k + 1].0 as f64) <= frame {
            *k += 1;
        }
        let (first, p0, n, from, to) = spans[*k];
        let m = (frame - first as f64).clamp(0.0, n as f64);
        let h = period_s;
        let tt = h * m / n as f64;
        let e = hermite(from, to, h, tt);
        p0 + m * (1.0 + 0.5 * (from.0 + e.0) + tt * (from.1 - e.1) / 12.0)
    };
    // Truth at 1 kHz.
    let t_end = follower.lines[follower.lines.len() - 1].time_ns(src);
    let fs = 1000.0;
    let n = (((t_end - t0) as f64 * 1e-9 - 1.0) * fs).max(0.0) as usize;
    let mut err = Vec::with_capacity(n);
    let mut k = 0usize;
    for i in 0..n {
        let t = t0 + (i as f64 / fs * 1e9) as i64;
        err.push(fit_m.at(t) - source_at(fit_f.at(t), &mut k));
    }
    let steady0 = ((steady_from_s * fs) as usize).min(n);
    let steady = &err[steady0..];
    if steady.is_empty() {
        return Err("run too short for the steady window".into());
    }
    let bias = steady.iter().sum::<f64>() / steady.len() as f64;
    let peak = steady.iter().fold(0.0f64, |a, e| a.max((e - bias).abs()));
    let rms = (steady.iter().map(|e| (e - bias).powi(2)).sum::<f64>() / steady.len() as f64).sqrt();
    let lock_s = err
        .iter()
        .rposition(|e| (e - bias).abs() >= 1.0)
        .map_or(0.0, |i| (i + 1) as f64 / fs);
    // The measured error against the truth at each update in the window.
    let mut d = Vec::new();
    for &(t, e) in &updates {
        let i = ((t - t0) as f64 * 1e-9 * fs) as usize;
        if i >= steady0 && i < n && e.is_finite() {
            d.push(e - err[i]);
        }
    }
    let dm = d.iter().sum::<f64>() / d.len().max(1) as f64;
    let dn = (d.iter().map(|x| (x - dm).powi(2)).sum::<f64>() / d.len().max(1) as f64).sqrt();
    let sb = if steady.len() >= SEGMENT + 2 {
        sidebands(steady, fs, 10_000.0, g, (1.0, 100.0))
    } else {
        (f64::NAN, f64::NAN)
    };
    // The clock ratio ε must cancel at the end: master rate over follower
    // rate, from the fits' local slopes.
    let slope = |f: &Fit, t: i64| (f.at(t + 500_000_000) - f.at(t - 500_000_000)) / g;
    let te = t_end - 1_000_000_000;
    let eps_true = slope(&fit_m, te) / slope(&fit_f, te) - 1.0;
    let pair_ppm = ((1.0 + fit_f.ppm() * 1e-6) / (1.0 + fit_m.ppm() * 1e-6) - 1.0) * 1e6;
    Ok(ReplayOutcome {
        lock_s,
        lock_flag_s,
        steady_peak: peak,
        steady_rms: rms,
        reading_bias: dm,
        detector_noise: dn,
        sideband_dbc: sb.0,
        sideband_hz: sb.1,
        eps_error_ppm: (lp.eps() - eps_true) * 1e6,
        pair_ppm,
        skips,
        unlocks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A device at +50 ppm, read every 1024 frames with a pointer in
    /// 48-frame steps.
    fn synthetic(ppm: f64, n: usize) -> String {
        let mut s = String::from(
            "# device 0 rate 48000 period 1024 buffer 3072 format S32 rt None dense false\n",
        );
        let rate = 48_000.0 * (1.0 + ppm * 1e-6);
        for k in 0..n {
            let t = 0.001 + k as f64 * 1024.0 / 48_000.0;
            let played = ((t * rate) as i64 / 48) * 48;
            let written = 3072 + 1024 * k as i64;
            let ns = (t * 1e9) as i64;
            s += &format!(
                "{written} {} {ns} {} {} 1024 0\n",
                written - played,
                ns + 20_000,
                ns + 22_000
            );
        }
        s
    }

    #[test]
    fn parse_and_fit() {
        let log = Log::parse(&synthetic(50.0, 3000)).expect("parse");
        assert_eq!(
            (log.period, log.buffer, log.lines.len()),
            (1024, 3072, 3000)
        );
        assert_eq!(log.rate, 48_000.0);
        let s = device_stats(&log, TimeSource::Htstamp, 3, 0);
        // The 48-frame steps alias into a slow sawtooth: within 1 ppm.
        assert!((s.ppm - 50.0).abs() < 1.0, "{}", s.ppm);
        // Uniform quantisation over 48 frames: σ = 48 / √12 ≈ 13.9.
        assert!(s.noise_rms > 10.0 && s.noise_rms < 16.0, "{}", s.noise_rms);
        assert!((s.late_us.0 - 20.0).abs() < 0.01);
        assert!(Log::parse("1 2 3\n").is_err());
    }

    #[test]
    fn replay_locks_on_clean_readings() {
        // Two fine-pointer devices 30 ppm apart: the loop must lock and
        // hold well within a frame.
        let fine = |ppm: f64| {
            let mut s = String::from("# rate 48000 period 1024 buffer 3072 dense false\n");
            let rate = 48_000.0 * (1.0 + ppm * 1e-6);
            for k in 0..5000 {
                let t = 0.001 + k as f64 * 1024.0 / 48_000.0;
                let played = (t * rate) as i64;
                let written = 3072 + 1024 * k as i64;
                let ns = (t * 1e9) as i64;
                s += &format!("{written} {} {ns} {ns} {ns}\n", written - played);
            }
            Log::parse(&s).expect("parse")
        };
        let (m, f) = (fine(0.0), fine(30.0));
        let opt = ReplayOptions {
            steady_from_s: 60.0,
            ..ReplayOptions::default()
        };
        let o = replay(&m, &f, &opt).expect("replay");
        println!("{o:?}");
        assert!(o.lock_flag_s < 30.0, "{o:?}");
        assert!(o.steady_peak < 0.5, "{o:?}");
        assert!((o.pair_ppm - 30.0).abs() < 0.1, "{o:?}");
        assert!(o.eps_error_ppm.abs() < 1.0, "{o:?}");
    }
}
