//! Linked look-ahead peak protection. Only the detector interpolates: audio
//! remains an exact delayed copy whenever the gain is one.
//!
//! A bounded-slope attack is scheduled before the entire detector support,
//! then held across that support. Monotonic queues make scheduling amortized
//! O(1) per frame, independent of look-ahead and caller block size.

use crate::design::paths::fir::windows::bessel_i0_series;
use crate::{Stage, db_to_lin};
use audivane_protocol::{LimiterConfig, LimiterMode, LimiterStatus};
use std::{collections::VecDeque, f64::consts::PI};

const TAPS: usize = 192;
/// Kaiser window of the detector phases, chosen against a Blackman window of
/// the same length: 0.003 dB of passband ripple for a much sharper edge,
/// under 0.012 dB magnitude error up to 0.49 cycles per sample instead of
/// 0.26 dB, which is what near-Nyquist content needs.
const KAISER_BETA: f64 = 6.0;
const DETECTOR_MARGIN_DB: f64 = 0.2;
/// Detection points per base-rate (44.1/48 kHz) sample.
const DETECTION_DENSITY: u32 = 8;
const HEADROOM_STEP_DB: f64 = 0.5;
/// Largest held attenuation. Anything beyond is left to the limiter, so one
/// absurd sample cannot mute the session.
pub const MAX_HEADROOM_DB: f64 = 12.0;
/// Limit-mode onsets re-arm once the reduction is below this (0.01 dB).
const ONSET_REARM: f64 = 0.998_849_369_936_505_2;
/// Below this volume gain (-120 dB) the pre-volume level is unknown.
const MIN_VOLUME: f64 = 1e-6;
/// Largest material need tracked per channel (before volume). The applied
/// headroom is capped at [`MAX_HEADROOM_DB`]; this only keeps the stored
/// need finite.
const MAX_MATERIAL_DB: f64 = 240.0;
/// Gains this close to their release target snap to it, so a released
/// limiter is bit-exact again.
const SNAP: f64 = 1e-12;

/// Detector phases per output sample, from the rate family: eight points per
/// 44.1/48 kHz sample at the base rate, four at 2x, two at 4x and the output
/// samples alone from 8x (352.8 kHz) up, whether or not the chain upsamples.
pub fn detection_factor(output_rate: u32) -> usize {
    let base = if output_rate.is_multiple_of(11_025) {
        44_100
    } else {
        48_000
    };
    (DETECTION_DENSITY * base)
        .div_ceil(output_rate.max(1))
        .max(1) as usize
}

struct Minimum {
    entries: VecDeque<(u64, f64)>,
    slope: f64,
}

impl Minimum {
    fn new(capacity: usize, slope: f64) -> Self {
        Self {
            entries: VecDeque::with_capacity(capacity),
            slope,
        }
    }

    fn push(&mut self, end: u64, value: f64) {
        while self
            .entries
            .back()
            .is_some_and(|&(old_end, v)| value + self.slope * (end - old_end) as f64 <= v)
        {
            self.entries.pop_back();
        }
        self.entries.push_back((end, value));
    }

    fn get(&mut self, time: u64) -> f64 {
        while self.entries.front().is_some_and(|&(end, _)| end < time) {
            self.entries.pop_front();
        }
        self.entries.front().map_or(f64::INFINITY, |&(end, v)| {
            v + self.slope * (end - time) as f64
        })
    }
}

/// Held automatic headroom, per channel: `material_db[c]` is what channel
/// `c`'s material needs before its digital volume/balance gain `gain_db[c]`;
/// the applied headroom is the largest `material_db[c] + gain_db[c]`, so a
/// volume or balance change moves it at once, both ways. Fixed-size after
/// construction, so a chain rebuild can carry it over on the audio thread.
#[derive(Clone, Debug, PartialEq)]
struct Held {
    material_db: Vec<f64>,
    material_gain: Vec<f64>,
    /// Largest gain of each channel in the current block, in dB
    /// (`-inf` at or below [`MIN_VOLUME`]).
    gain_db: Vec<f64>,
    /// 1 / that gain (0 at or below [`MIN_VOLUME`]).
    inv_gain: Vec<f64>,
    db: f64,
    gain: f64,
}

impl Held {
    fn new(channels: usize) -> Self {
        Self {
            material_db: vec![0.0; channels],
            material_gain: vec![1.0; channels],
            gain_db: vec![0.0; channels],
            inv_gain: vec![1.0; channels],
            db: 0.0,
            gain: 1.0,
        }
    }

    fn forget_material(&mut self) {
        self.material_db.fill(0.0);
        self.material_gain.fill(1.0);
        self.update();
    }

    fn update(&mut self) {
        let db = self
            .material_db
            .iter()
            .zip(&self.gain_db)
            .fold(0.0_f64, |m, (material, gain)| m.max(material + gain))
            .min(MAX_HEADROOM_DB);
        self.db = db;
        self.gain = if db == 0.0 { 1.0 } else { db_to_lin(-db) };
    }
}

#[inline]
fn dot(taps: &[f64; TAPS], x: &[f64]) -> f64 {
    // Eight independent sums let the compiler vectorise the loop.
    let mut acc = [0.0_f64; 8];
    let (taps, _) = taps.as_chunks::<8>();
    let (x, _) = x[..TAPS].as_chunks::<8>();
    for (t, x) in taps.iter().zip(x) {
        for k in 0..8 {
            acc[k] += t[k] * x[k];
        }
    }
    ((acc[0] + acc[4]) + (acc[1] + acc[5])) + ((acc[2] + acc[6]) + (acc[3] + acc[7]))
}

pub struct Limiter {
    config: LimiterConfig,
    mode: LimiterMode,
    ceiling: f64,
    detector_ceiling: f64,
    delay: Vec<Vec<f64>>,
    history: Vec<[f64; TAPS * 2]>,
    phases: Vec<[f64; TAPS]>,
    delay_pos: usize,
    history_pos: usize,
    attack: usize,
    pending: Vec<f64>,
    pending_pos: usize,
    future: Minimum,
    hold: Minimum,
    time: u64,
    gain: f64,
    applied: f64,
    min_applied: f64,
    /// Limit mode: the next gain decrease counts as an onset.
    armed: bool,
    release: f64,
    held: Held,
    /// Per-channel material peak of the current frame (scratch).
    material: Vec<f64>,
    /// After a volume decrease the detector window still holds louder
    /// samples: no material decision before this frame.
    volume_settles_at: u64,
    /// The volume/balance gain changes within the coming block: no material
    /// decision in it or in the detector span after it.
    ramping: bool,
    output_rate: u32,
    /// Frame of the last headroom step, for grouping steps into events.
    last_step: Option<u64>,
    /// Whether held headroom may change and events count (see
    /// [`Limiter::set_learning`]).
    learning: bool,
    events: u64,
    /// Frame of the last peak over the detector ceiling (event attribution).
    last_over: u64,
}

impl Limiter {
    /// `output_rate` is the rate the stage runs at; it alone sets the
    /// detection density (see [`detection_factor`]).
    pub fn new(config: &LimiterConfig, output_rate: u32, channels: usize) -> Result<Self, String> {
        config.validate()?;
        if !(44_100..=3_072_000).contains(&output_rate) || channels == 0 || channels > 128 {
            return Err("limiter needs 44.1..3072 kHz and 1..128 channels".into());
        }
        let factor = detection_factor(output_rate);
        let support = if factor > 1 { TAPS - 1 } else { 0 };
        let latency = (f64::from(output_rate) * config.lookahead_ms / 1000.0).ceil() as usize;
        if latency <= support {
            return Err("limiter look-ahead is shorter than detector support at this rate".into());
        }
        let i0_beta = bessel_i0_series(KAISER_BETA);
        let half = (TAPS / 2) as f64;
        let phases = (1..factor)
            .map(|phase| {
                let frac = phase as f64 / factor as f64;
                let mut taps = std::array::from_fn(|i| {
                    let x = i as f64 - (TAPS / 2 - 1) as f64 - frac;
                    let sinc = (PI * x).sin() / (PI * x);
                    let t = x / half;
                    let w = bessel_i0_series(KAISER_BETA * (1.0 - t * t).max(0.0).sqrt()) / i0_beta;
                    sinc * w
                });
                let sum: f64 = taps.iter().sum();
                for tap in &mut taps {
                    *tap /= sum;
                }
                taps
            })
            .collect();
        let attack = latency - support;
        let detector_db = config.ceiling_dbfs - DETECTOR_MARGIN_DB;
        Ok(Self {
            config: config.clone(),
            mode: config.mode,
            ceiling: db_to_lin(config.ceiling_dbfs),
            detector_ceiling: db_to_lin(detector_db),
            delay: vec![vec![0.0; latency]; channels],
            history: vec![[0.0; TAPS * 2]; channels],
            phases,
            delay_pos: 0,
            history_pos: 0,
            attack,
            pending: vec![1.0; attack],
            pending_pos: 0,
            future: Minimum::new(latency + 2, 1.0 / attack as f64),
            hold: Minimum::new(latency + 2, 0.0),
            time: 0,
            gain: 1.0,
            applied: 1.0,
            min_applied: 1.0,
            armed: true,
            release: -(-1.0 / (f64::from(output_rate) * config.release_ms / 1000.0)).exp_m1(),
            held: Held::new(channels),
            material: vec![0.0; channels],
            volume_settles_at: 0,
            ramping: false,
            output_rate,
            last_step: None,
            learning: true,
            events: 0,
            last_over: 0,
        })
    }

    /// Drain the counters: events since the last drain (headroom steps in
    /// `auto_headroom`, limiting onsets in `limit`) and the largest gain
    /// reduction since the last drain. Headroom is the current value.
    pub fn take_status(&mut self) -> LimiterStatus {
        let reduction = -20.0 * self.min_applied.max(f64::MIN_POSITIVE).log10();
        self.min_applied = self.applied;
        LimiterStatus {
            active: self.mode != LimiterMode::Off,
            headroom_db: self.held.db,
            gain_reduction_db: reduction.max(0.0),
            events: std::mem::take(&mut self.events),
        }
    }

    /// Digital volume/balance gain applied ahead of the stage in the coming
    /// block, the same for every channel. See [`Limiter::set_channel_gains`].
    pub fn set_volume_gain(&mut self, gain: f64) {
        self.set_channel_gains(|_| gain);
    }

    /// Largest digital volume/balance gain of each channel (`gain(c)` for
    /// channel `c`) applied ahead of the stage in the coming block. Held
    /// headroom tracks the material before that gain, channel by channel,
    /// so a volume or balance change moves the applied headroom at once,
    /// both ways. No allocation.
    pub fn set_channel_gains(&mut self, gain: impl Fn(usize) -> f64) {
        let held = &mut self.held;
        let mut decreased = false;
        for (c, (inv, gain_db)) in held.inv_gain.iter_mut().zip(&mut held.gain_db).enumerate() {
            let g = gain(c);
            // A gain above unity (a boost; the daemon never applies one)
            // counts as unity: the material is then measured after it.
            let g = if g.is_finite() {
                g.clamp(0.0, 1.0)
            } else {
                1.0
            };
            let (new_inv, new_db) = if g > MIN_VOLUME {
                (1.0 / g, 20.0 * g.log10())
            } else {
                (0.0, f64::NEG_INFINITY)
            };
            // A lower gain on any channel: its detector window still holds
            // samples at the old, higher gain.
            if new_db < *gain_db {
                decreased = true;
            }
            *inv = new_inv;
            *gain_db = new_db;
        }
        if decreased {
            let support = (self.delay[0].len() - self.attack) as u64;
            self.volume_settles_at = self.time + support + 1;
        }
        held.update();
    }

    /// The volume/balance gain ramps within the coming block (its gains
    /// before and after differ). Material decisions pause for the block and
    /// one detector span after it, so what is learnt does not depend on how
    /// a ramp is split into blocks; limiting itself does not pause.
    pub fn set_ramping(&mut self, ramping: bool) {
        self.ramping = ramping;
    }

    /// Currently applied automatic headroom in dB.
    pub fn headroom_db(&self) -> f64 {
        self.held.db
    }

    /// Carry the held headroom over from the limiter of a chain being
    /// replaced, when both run `auto_headroom` with the same config at the
    /// same output rate and channel count (a rate change is usually a new
    /// track). Plain copy:
    /// safe on the audio thread. Returns whether it did.
    pub fn inherit_headroom(&mut self, from: &Limiter) -> bool {
        if self.mode != LimiterMode::AutoHeadroom
            || self.config != from.config
            || self.output_rate != from.output_rate
            || self.delay.len() != from.delay.len()
        {
            return false;
        }
        self.held
            .material_db
            .copy_from_slice(&from.held.material_db);
        self.held
            .material_gain
            .copy_from_slice(&from.held.material_gain);
        self.held.update();
        true
    }

    /// While off, held headroom does not step and no events
    /// count; limiting itself is unchanged. The daemon turns it off while a
    /// replacement chain warms up on audio that is not played.
    pub fn set_learning(&mut self, learning: bool) {
        self.learning = learning;
    }

    /// `material` is channel `c`'s post-volume peak over that channel's
    /// largest gain in the block: never above the true pre-volume peak, so a
    /// ramp cannot leave extra headroom.
    /// Returns whether a new event was counted.
    fn track_headroom(&mut self, c: usize, material: f64) -> bool {
        let held = &mut self.held;
        if held.inv_gain[c] == 0.0
            || self.time < self.volume_settles_at
            || material * held.material_gain[c] <= self.detector_ceiling
        {
            return false;
        }
        let needed = (20.0 * (material / self.detector_ceiling).log10() / HEADROOM_STEP_DB).ceil()
            * HEADROOM_STEP_DB;
        // The need is kept as measured (the cap is on the applied headroom,
        // after each channel's gain), so lowering the volume later gives
        // back exactly that much.
        let needed = needed.clamp(0.0, MAX_MATERIAL_DB);
        if needed > held.material_db[c] {
            let applied = held.db;
            held.material_db[c] = needed;
            held.material_gain[c] = db_to_lin(-needed);
            held.update();
            if held.db <= applied {
                // The digital volume already covers it: nothing pulled down.
                return false;
            }
            // Steps within one look-ahead span (a peak still building up)
            // are one event.
            let span = self.delay[0].len() as u64;
            let new = self.last_step.is_none_or(|t| self.time - t > span);
            if new {
                self.events = self.events.saturating_add(1);
            }
            self.last_step = Some(self.time);
            return new;
        }
        false
    }
}

impl Limiter {
    /// [`Stage::process`], calling `on_event(frame, age)` for every event it
    /// counts (see [`Self::take_status`]): `frame` is the index in this
    /// block, and the frame that caused the event entered the stage `age`
    /// frames before it (at most [`Stage::latency_frames`]). A headroom step
    /// is caused by the frame entering (age 0); a limiting onset, which the
    /// look-ahead starts before its peak leaves, by the last peak that
    /// entered over the detector, or by the frame leaving when the exact
    /// sample ceiling acts. Allocation-free.
    pub fn process_with(
        &mut self,
        chans: &mut [&mut [f64]],
        mut on_event: impl FnMut(usize, usize),
    ) {
        assert_eq!(chans.len(), self.delay.len());
        if self.mode == LimiterMode::Off {
            return;
        }
        let frames = chans[0].len();
        assert!(chans.iter().all(|c| c.len() == frames));
        if std::mem::take(&mut self.ramping) {
            let support = (self.delay[0].len() - self.attack) as u64;
            self.volume_settles_at = self
                .volume_settles_at
                .max(self.time + frames as u64 + support + 1);
        }
        for i in 0..frames {
            let mut peak = 0.0_f64;
            for ((chan, history), (material, &inv)) in chans
                .iter_mut()
                .zip(&mut self.history)
                .zip(self.material.iter_mut().zip(&self.held.inv_gain))
            {
                let x = if chan[i].is_finite() { chan[i] } else { 0.0 };
                chan[i] = x;
                history[self.history_pos] = x;
                history[self.history_pos + TAPS] = x;
                let mut channel_peak = x.abs();
                let window = &history[self.history_pos + 1..self.history_pos + 1 + TAPS];
                for taps in &self.phases {
                    channel_peak = channel_peak.max(dot(taps, window).abs());
                }
                peak = peak.max(channel_peak);
                *material = channel_peak * inv;
            }
            self.history_pos = (self.history_pos + 1) % TAPS;
            if self.mode == LimiterMode::AutoHeadroom && self.learning {
                for c in 0..self.material.len() {
                    if self.track_headroom(c, self.material[c]) {
                        on_event(i, 0);
                    }
                }
            }
            let required = if peak > self.detector_ceiling {
                self.detector_ceiling / peak
            } else {
                1.0
            };
            let required = required.min(self.held.gain);
            let end = self.time + self.attack as u64;
            if required < 1.0 {
                self.future.push(end, required);
                self.last_over = self.time;
            }
            let mature = self.pending[self.pending_pos];
            self.pending[self.pending_pos] = required;
            self.pending_pos = (self.pending_pos + 1) % self.pending.len();
            let support = self.delay[0].len() - self.attack;
            self.hold.push(self.time + support as u64, mature);
            let target = self
                .future
                .get(self.time)
                .min(self.hold.get(self.time))
                .clamp(0.0, 1.0);
            self.gain = target.min(self.gain + self.release * (1.0 - self.gain));
            if target - self.gain < SNAP {
                self.gain = target;
            }
            let mut delayed_peak = 0.0_f64;
            for delay in &self.delay {
                delayed_peak = delayed_peak.max(delay[self.delay_pos].abs());
            }
            // Exact sample ceiling even for extreme inputs or estimator error.
            let safe = if delayed_peak > self.ceiling {
                self.ceiling / delayed_peak
            } else {
                1.0
            };
            let gain = self.gain.min(safe);
            if gain < self.applied && self.armed {
                if self.mode == LimiterMode::Limit && self.learning {
                    self.events = self.events.saturating_add(1);
                    let latency = self.delay[0].len();
                    let age = if safe < self.gain {
                        latency
                    } else {
                        (self.time - self.last_over).min(latency as u64) as usize
                    };
                    on_event(i, age);
                }
                self.armed = false;
            } else if gain >= ONSET_REARM {
                self.armed = true;
            }
            self.applied = gain;
            self.min_applied = self.min_applied.min(gain);
            for (chan, delay) in chans.iter_mut().zip(&mut self.delay) {
                let x = std::mem::replace(&mut delay[self.delay_pos], chan[i]);
                let y = x * gain;
                chan[i] = y.clamp(-self.ceiling, self.ceiling);
            }
            self.delay_pos = (self.delay_pos + 1) % self.delay[0].len();
            self.time += 1;
        }
    }
}

impl Stage for Limiter {
    fn process(&mut self, chans: &mut [&mut [f64]]) {
        self.process_with(chans, |_, _| {});
    }

    fn latency_frames(&self) -> usize {
        if self.mode == LimiterMode::Off {
            0
        } else {
            self.delay[0].len()
        }
    }

    fn reset(&mut self) {
        for delay in &mut self.delay {
            delay.fill(0.0);
        }
        self.history.fill([0.0; TAPS * 2]);
        self.pending.fill(1.0);
        self.future.entries.clear();
        self.hold.entries.clear();
        self.delay_pos = 0;
        self.history_pos = 0;
        self.pending_pos = 0;
        self.time = 0;
        self.gain = 1.0;
        self.applied = 1.0;
        self.min_applied = 1.0;
        self.armed = true;
        self.held.forget_material();
        self.volume_settles_at = 0;
        self.ramping = false;
        self.last_step = None;
        self.events = 0;
        self.last_over = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detector_phases_have_unity_dc_and_flat_audio_passband() {
        let config = LimiterConfig {
            mode: LimiterMode::Limit,
            ..Default::default()
        };
        for rate in [44_100, 48_000, 88_200, 96_000, 176_400, 192_000] {
            let limiter = Limiter::new(&config, rate, 1).unwrap();
            for taps in limiter.phases {
                // Only the actual detector support contributes to a frame.
                let dc: f64 = taps.iter().take(TAPS).sum();
                assert!((dc - 1.0).abs() < 1e-12);
                for bin in 0..=49 {
                    let frequency = bin as f64 / 100.0;
                    let (mut real, mut imag) = (0.0, 0.0);
                    for (i, tap) in taps.iter().take(TAPS).enumerate() {
                        let angle = 2.0 * PI * frequency * i as f64;
                        real += tap * angle.cos();
                        imag += tap * angle.sin();
                    }
                    // 0.0015 is about 0.013 dB.
                    assert!(
                        (real.hypot(imag) - 1.0).abs() < 0.0015,
                        "{rate} Hz, {frequency}"
                    );
                }
            }
        }
    }

    #[test]
    fn scheduled_minimum_matches_brute_force_even_after_long_uptime() {
        for slope in [0.0, 1.0 / 49.0] {
            let mut ordinary = Minimum::new(256, slope);
            let mut late = Minimum::new(256, slope);
            let mut events = Vec::new();
            let epoch = 1_u64 << 60;
            for time in 0..500 {
                let value = ((time * 37) % 101) as f64 / 100.0;
                let end = time + 49;
                ordinary.push(end, value);
                late.push(epoch + end, value);
                events.push((end, value));
                let expected = events
                    .iter()
                    .filter(|&&(end, _)| end >= time)
                    .map(|&(end, value)| value + slope * (end - time) as f64)
                    .fold(f64::INFINITY, f64::min);
                assert_eq!(ordinary.get(time), expected);
                assert_eq!(late.get(epoch + time), expected);
            }
        }
    }
}
