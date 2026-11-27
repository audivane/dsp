//! Measured A/B level matching ("Match to this track"), the control-plane
//! half: BS.1770 gating of the audio thread's 100 ms hops
//! ([`audivane_dsp_core::ab_measure`]) and the lock rule.
//!
//! - **Blocks.** Four consecutive hops make a 400 ms block (75 % overlap),
//!   one per hop once four have arrived.
//! - **Gating on A.** A block counts when A's loudness passes the absolute
//!   gate (−70 LUFS) and the relative gate (10 LU below the mean of A's
//!   absolutely gated blocks). The same blocks are used for B, so a quiet
//!   passage cannot be gated in on one side and out on the other.
//! - **The offset** is `10·log10(ΣE_A / ΣE_B)` over the gated blocks: B
//!   plus it is as loud as A.
//! - **Time** counts the blocks that contribute to the offset, those that
//!   pass both of A's gates (0.1 s each): silence, pauses and quiet
//!   passages do not advance it, so the measurement waits. A rise of the
//!   relative gate can take blocks out again, so it can step back.
//! - **Lock rule.** Stable once at least [`MIN_SECONDS`] contribute and
//!   the offset moved by less than [`STABLE_DB`] over the last
//!   [`WINDOW_SECONDS`] of contributing blocks; the measurement locks then,
//!   or at `max_seconds` of them. At `max_seconds` without an offset (B
//!   silent) it ends with no match.
//!
//! Nothing here runs on the audio thread.

use audivane_dsp_core::ab_measure::{AbHop, HOP_SECONDS};
use audivane_protocol::ipc::AbMeasuring;
use std::collections::VecDeque;

/// Least measured time before a lock.
pub const MIN_SECONDS: f64 = 20.0;
/// The offset must have settled over this much measured time.
pub const WINDOW_SECONDS: f64 = 10.0;
/// ...to within this much (max − min over the window).
pub const STABLE_DB: f64 = 0.1;

const BLOCK_HOPS: usize = 4;
const ABSOLUTE_GATE_LUFS: f64 = -70.0;
const RELATIVE_GATE_LU: f64 = 10.0;
const LUFS_OFFSET: f64 = -0.691;

fn blocks_for(seconds: f64) -> u64 {
    (seconds / HOP_SECONDS).round() as u64
}

fn loudness(energy: f64) -> f64 {
    LUFS_OFFSET + 10.0 * energy.log10()
}

/// What a measurement comes to.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Outcome {
    /// Lock this offset for B (dB).
    Lock(f64),
    /// `max_seconds` reached without a finite offset (B silent).
    NoMatch,
}

/// One measurement, fed hops in order.
#[derive(Clone, Debug)]
pub struct Measurement {
    epoch: u32,
    max_blocks: u64,
    /// The last hops `(A, B)`, oldest first.
    hops: VecDeque<(f64, f64)>,
    /// Block energies `(A, B)` that passed A's absolute gate.
    blocks: Vec<(f64, f64)>,
    /// Sum of A's energies over `blocks` (for the relative gate).
    sum_a: f64,
    offset_db: Option<f64>,
    /// Blocks that pass both of A's gates (the offset's blocks).
    counted: u64,
    /// `(counted, offset)` after each block, over the last window of
    /// counted blocks; `counted` increases along it.
    history: VecDeque<(u64, f64)>,
}

impl Measurement {
    /// A measurement of the hops tagged `epoch` that locks at
    /// `max_seconds` of measured time at the latest.
    pub fn new(epoch: u32, max_seconds: f64) -> Self {
        Self {
            epoch,
            max_blocks: blocks_for(max_seconds).max(1),
            hops: VecDeque::with_capacity(BLOCK_HOPS),
            blocks: Vec::new(),
            sum_a: 0.0,
            offset_db: None,
            counted: 0,
            history: VecDeque::new(),
        }
    }

    pub fn epoch(&self) -> u32 {
        self.epoch
    }

    /// Feed one hop. Hops of another epoch are ignored; a non-finite one
    /// restarts the block (four new hops are needed).
    pub fn push(&mut self, hop: AbHop) {
        if hop.epoch != self.epoch {
            return;
        }
        if !(hop.a.is_finite() && hop.b.is_finite()) || hop.a < 0.0 || hop.b < 0.0 {
            self.hops.clear();
            return;
        }
        if self.hops.len() == BLOCK_HOPS {
            self.hops.pop_front();
        }
        self.hops.push_back((hop.a, hop.b));
        if self.hops.len() < BLOCK_HOPS {
            return;
        }
        let n = BLOCK_HOPS as f64;
        let a = self.hops.iter().map(|h| h.0).sum::<f64>() / n;
        let b = self.hops.iter().map(|h| h.1).sum::<f64>() / n;
        // The absolute gate also drops silence (log of 0).
        let la = loudness(a);
        if la.is_nan() || la <= ABSOLUTE_GATE_LUFS {
            return;
        }
        self.blocks.push((a, b));
        self.sum_a += a;
        let (offset_db, counted) = self.integrate();
        self.offset_db = offset_db;
        // A rise of the relative gate can lower the count: the estimate is
        // over another set of blocks now, so the whole window refills
        // before it is stable.
        if counted < self.counted {
            self.history.clear();
        }
        self.counted = counted;
        while self.history.back().is_some_and(|h| h.0 >= counted) {
            self.history.pop_back();
        }
        if let Some(o) = offset_db {
            self.history.push_back((counted, o));
        }
        let window = blocks_for(WINDOW_SECONDS);
        // Keep the newest entry at or before the window start too.
        while self.history.len() > 1 && self.history[1].0 + window <= counted {
            self.history.pop_front();
        }
    }

    /// The offset over the blocks that pass both of A's gates, and how many
    /// they are.
    fn integrate(&self) -> (Option<f64>, u64) {
        let threshold = loudness(self.sum_a / self.blocks.len() as f64) - RELATIVE_GATE_LU;
        let (mut sa, mut sb, mut n) = (0.0, 0.0, 0);
        for &(a, b) in &self.blocks {
            if loudness(a) > threshold {
                sa += a;
                sb += b;
                n += 1;
            }
        }
        let o = 10.0 * (sa / sb).log10();
        ((sb > 0.0 && o.is_finite()).then_some(o), n)
    }

    /// Measured time: the blocks that contribute to the offset.
    pub fn seconds(&self) -> f64 {
        self.counted as f64 * HOP_SECONDS
    }

    /// The running offset for B (dB).
    pub fn offset_db(&self) -> Option<f64> {
        self.offset_db
    }

    /// At least [`MIN_SECONDS`] measured, and over the last
    /// [`WINDOW_SECONDS`] the offset moved by less than [`STABLE_DB`].
    pub fn stable(&self) -> bool {
        let at = self.counted;
        let window = blocks_for(WINDOW_SECONDS);
        if self.offset_db.is_none() || at < blocks_for(MIN_SECONDS) {
            return false;
        }
        // The history must reach back over the whole window.
        if self.history.front().is_none_or(|h| h.0 + window > at) {
            return false;
        }
        let (lo, hi) = self
            .history
            .iter()
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), h| {
                (lo.min(h.1), hi.max(h.1))
            });
        hi - lo < STABLE_DB
    }

    /// The offset to lock now, if the rule says so: stable, or at
    /// `max_seconds` with an offset.
    pub fn lock(&self) -> Option<f64> {
        match self.outcome() {
            Some(Outcome::Lock(o)) => Some(o),
            _ => None,
        }
    }

    /// Whether the run is over: a lock (stable, or `max_seconds` with an
    /// offset) or no match (`max_seconds` without one).
    pub fn outcome(&self) -> Option<Outcome> {
        let capped = self.counted >= self.max_blocks;
        match self.offset_db {
            Some(o) if capped || self.stable() => Some(Outcome::Lock(o)),
            None if capped => Some(Outcome::NoMatch),
            _ => None,
        }
    }

    pub fn status(&self) -> AbMeasuring {
        AbMeasuring {
            seconds: self.seconds(),
            offset_db: self.offset_db,
            stable: self.stable(),
            max_seconds: self.max_blocks as f64 * HOP_SECONDS,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Energy of a block at `lufs`.
    fn energy(lufs: f64) -> f64 {
        10f64.powf((lufs - LUFS_OFFSET) / 10.0)
    }

    fn hop(a_lufs: f64, b_db: f64) -> AbHop {
        let a = if a_lufs.is_finite() {
            energy(a_lufs)
        } else {
            0.0
        };
        AbHop {
            epoch: 1,
            a,
            b: a * 10f64.powf(b_db / 10.0),
        }
    }

    fn feed(m: &mut Measurement, seconds: f64, a_lufs: f64, b_db: f64) {
        for _ in 0..blocks_for(seconds) {
            m.push(hop(a_lufs, b_db));
        }
    }

    #[test]
    fn steady_material_locks_after_the_minimum() {
        let mut m = Measurement::new(1, 60.0);
        // B is 3 dB louder than A.
        feed(&mut m, 19.9, -20.0, 3.0);
        assert!((m.offset_db().unwrap() + 3.0).abs() < 1e-9);
        assert!(!m.stable());
        assert_eq!(m.lock(), None, "no lock before 20 s");
        feed(&mut m, 0.5, -20.0, 3.0);
        assert!(m.seconds() >= MIN_SECONDS);
        assert!(m.stable());
        assert!((m.lock().unwrap() + 3.0).abs() < 1e-9);
    }

    #[test]
    fn a_silent_b_gives_no_offset() {
        let mut m = Measurement::new(1, 10.0);
        for _ in 0..200 {
            m.push(AbHop {
                epoch: 1,
                a: energy(-20.0),
                b: 0.0,
            });
        }
        assert_eq!(m.offset_db(), None);
        assert_eq!(m.lock(), None);
        assert_eq!(m.outcome(), Some(Outcome::NoMatch), "the cap ends the run");
    }
}

/// The whole path on generated programme material: a real pair meters
/// both sides on the audio-thread code, the hops are gated here, and the
/// result is compared with offline BS.1770 loudness of each side's output.
#[cfg(test)]
mod material {
    use super::*;
    use audivane_dsp_core::ab_level::AbLevel;
    use audivane_dsp_core::chain::{AbChain, Chain, ChainSpec};
    use audivane_dsp_core::gain::StereoGainRamp;
    use audivane_dsp_core::loudness::LufsMeter;
    use audivane_protocol::{
        Band, ChainConfig, CrossfeedConfig, CrossfeedMode, EqProfile, FilterType,
    };

    const FS: u32 = 48_000;
    const SECTION_SECONDS: usize = 5;

    /// Stereo programme: 5 s sections cycling through bass-heavy, bright,
    /// quiet, mid-band and hard-panned content, with a 2 Hz beat.
    fn programme(seconds: usize) -> Vec<f32> {
        let fs = f64::from(FS);
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut noise = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0
        };
        let pole = |f: f64| (-2.0 * std::f64::consts::PI * f / fs).exp();
        let (p150, p500, p2k, p3k) = (pole(150.0), pole(500.0), pole(2000.0), pole(3000.0));
        let mut st = [[0.0f64; 4]; 2];
        let mut out = Vec::with_capacity(seconds * FS as usize * 2);
        for i in 0..seconds * FS as usize {
            let t = i as f64 / fs;
            let section = (i / (SECTION_SECONDS * FS as usize)) % 5;
            let beat = 0.6 + 0.4 * (2.0 * std::f64::consts::PI * t).sin().abs();
            for (ch, s) in st.iter_mut().enumerate() {
                let w = noise();
                s[0] = p150 * s[0] + (1.0 - p150) * w;
                s[1] = p500 * s[1] + (1.0 - p500) * w;
                s[2] = p2k * s[2] + (1.0 - p2k) * (w - s[1]);
                s[3] = p3k * s[3] + (1.0 - p3k) * w;
                let x = match section {
                    0 => 10.0 * s[0] + 0.05 * w,
                    1 => w - s[3],
                    2 => 0.03 * w,
                    3 => 1.5 * s[2],
                    _ => {
                        if ch == 0 {
                            0.8 * w
                        } else {
                            0.05 * w
                        }
                    }
                };
                out.push((0.25 * beat * x) as f32);
            }
        }
        out
    }

    fn spec(config: ChainConfig) -> ChainSpec {
        ChainSpec {
            config,
            sample_rate: FS,
            channels: 2,
            max_block: 1024,
            irs: None,
        }
    }

    /// Offline integrated loudness of `config`'s output for `input`.
    fn offline_lufs(config: &ChainConfig, input: &[f32]) -> f64 {
        let mut chain = Chain::new(spec(config.clone()));
        let mut gain = StereoGainRamp::unity(FS);
        let mut out = vec![0i32; 2048];
        let mut tap = vec![0f32; 2048];
        let mut meter = LufsMeter::new(f64::from(FS), 2);
        let mut planar = [vec![0.0; 1024], vec![0.0; 1024]];
        for block in input.chunks(2048) {
            let n = block.len() / 2;
            chain.process_f32_with_gain_tapped(block, &mut out, &mut gain, Some(&mut tap));
            for f in 0..n {
                planar[0][f] = f64::from(tap[2 * f]);
                planar[1][f] = f64::from(tap[2 * f + 1]);
            }
            meter.push(&[&planar[0][..n], &planar[1][..n]]);
        }
        meter.integrated_lufs()
    }

    /// `(lock time, locked offset, running offset at the end)`.
    fn measure(b: &ChainConfig, input: &[f32]) -> (f64, f64, f64) {
        let mut ab = AbChain::new(
            spec(ChainConfig::neutral()),
            spec(b.clone()),
            AbLevel::fixed(0.0),
        )
        .unwrap();
        ab.set_measure_epoch(1);
        let mut m = Measurement::new(1, 300.0);
        let mut out = vec![0i32; 2048];
        let mut lock = None;
        for block in input.chunks(2048) {
            ab.process_f32(block, &mut out);
            while let Some(hop) = ab.pop_measure_hop() {
                m.push(hop);
                if lock.is_none()
                    && let Some(o) = m.lock()
                {
                    lock = Some((m.seconds(), o));
                }
            }
        }
        let (at, o) = lock.unwrap_or((f64::NAN, f64::NAN));
        (at, o, m.offset_db().unwrap())
    }

    #[test]
    fn measured_offsets_match_offline_lufs_on_programme_material() {
        let input = programme(50);
        let flat = ChainConfig::neutral();
        let reference = offline_lufs(&flat, &input);
        let shelf = |kind, freq_hz, gain_db| ChainConfig {
            eq: Some(EqProfile {
                bands: vec![Band {
                    kind,
                    freq_hz,
                    gain_db,
                    q: std::f64::consts::FRAC_1_SQRT_2,
                    enabled: true,
                }],
                ..Default::default()
            }),
            ..ChainConfig::neutral()
        };
        let cases = [
            (
                "bass shelf +6 dB @105 Hz",
                shelf(FilterType::LowShelf, 105.0, 6.0),
            ),
            (
                "treble shelf -4 dB @8 kHz",
                shelf(FilterType::HighShelf, 8000.0, -4.0),
            ),
            (
                "crossfeed bs2b default",
                ChainConfig {
                    crossfeed: Some(CrossfeedConfig {
                        enabled: true,
                        mode: CrossfeedMode::Bs2bDefault,
                        ..Default::default()
                    }),
                    ..ChainConfig::neutral()
                },
            ),
        ];
        for (name, b) in cases {
            let offline = reference - offline_lufs(&b, &input);
            let (at, locked, end) = measure(&b, &input);
            eprintln!(
                "{name}: offline {offline:+.3} dB, running at 50 s {end:+.3} dB ({:+.3}), \
                 locked {locked:+.3} dB at {at:.1} s ({:+.3})",
                end - offline,
                locked - offline
            );
            // Over the same audio the measurement is BS.1770 loudness,
            // gated on A's blocks instead of each side's own.
            assert!((end - offline).abs() < 0.05, "{name}");
            assert!(at >= MIN_SECONDS, "{name}");
            assert!((locked - offline).abs() < 1.0, "{name}");
        }
    }
}
