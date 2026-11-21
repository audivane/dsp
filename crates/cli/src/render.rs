//! `avdsp render` — play a WAV file through the varispeed stage offline:
//! tempo and pitch change together by the speed, the rate stays the same.

use crate::RenderArgs;
use crate::wavio;
use anyhow::{Context, Result, bail};
use audivane_dsp_core::varispeed::{
    MAX_FRAMES, MAX_RAMP_MS, MAX_SPEED, MIN_SPEED, Varispeed, VarispeedSpec,
};
use std::time::Instant;

/// A speed change at a point of the output.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Change {
    /// Output time, seconds.
    pub at: f64,
    pub speed: f64,
    pub ramp_ms: f64,
}

/// `TIME:SPEED[:RAMP_MS]`.
pub fn parse_change(text: &str) -> Result<Change> {
    let parts: Vec<&str> = text.split(':').collect();
    if !(2..=3).contains(&parts.len()) {
        bail!("--change {text:?}: expected TIME:SPEED[:RAMP_MS]");
    }
    let num = |s: &str, what: &str| -> Result<f64> {
        let v: f64 = s
            .trim()
            .parse()
            .with_context(|| format!("--change {text:?}: bad {what} {s:?}"))?;
        if !v.is_finite() {
            bail!("--change {text:?}: {what} must be finite");
        }
        Ok(v)
    };
    let at = num(parts[0], "time")?;
    if at < 0.0 {
        bail!("--change {text:?}: the time must not be negative");
    }
    Ok(Change {
        at,
        speed: num(parts[1], "speed")?,
        ramp_ms: parts.get(2).map_or(Ok(0.0), |s| num(s, "ramp"))?,
    })
}

/// What a render produced.
#[derive(Debug)]
pub struct Rendered {
    /// Planar output at the input's rate.
    pub out: Vec<Vec<f64>>,
    /// The stage's delay at the start speed, frames (trimmed or kept).
    pub start_latency: f64,
    pub trimmed: usize,
    /// The output frame each change was applied at.
    pub change_frames: Vec<usize>,
    /// The speeds applied while the input plays (up to the core output that
    /// reaches its end; the frames after it are cut): the lowest, the
    /// highest, and the one at its end.
    pub low_speed: f64,
    pub peak_speed: f64,
    pub end_speed: f64,
    /// Seconds spent in the stage's own `plan` and `process`.
    pub stage_time: f64,
}

/// Run `input` (planar) through the stage at `rate`: from `speed`, with
/// `changes` applied at their output frames (calls are cut there), in
/// calls of at most `block` frames, until the output frame that plays the
/// end of the input (found on the stage's exact trajectory), then dropping
/// the start speed's delay when `trim`.
pub fn render(
    input: &[Vec<f64>],
    rate: u32,
    speed: f64,
    changes: &[Change],
    block: usize,
    trim: bool,
) -> Result<Rendered> {
    if block == 0 || block > MAX_FRAMES {
        bail!("--block must be 1 to {MAX_FRAMES} frames");
    }
    let channels = input.len();
    let frames = input.first().map_or(0, Vec::len);
    let mut changes = changes.to_vec();
    changes.sort_by(|a, b| a.at.total_cmp(&b.at));
    for c in &changes {
        // Every change is checked before the run.
        if !(MIN_SPEED..=MAX_SPEED).contains(&c.speed) {
            bail!(
                "--change speed {} must be {MIN_SPEED} to {MAX_SPEED}",
                c.speed
            );
        }
        if !(0.0..=MAX_RAMP_MS).contains(&c.ramp_ms) {
            bail!("--change ramp {} ms must be 0 to {MAX_RAMP_MS}", c.ramp_ms);
        }
    }
    let mut vs = Varispeed::new(VarispeedSpec {
        sample_rate: rate,
        channels,
        max_frames: block,
        speed,
    })?;
    let start_latency = vs.latency_frames();
    // Source time t (frames) sits at 2t + d_up in the ×2's output; the core
    // output at position p plays p - d_core there; output frame m of the ÷2
    // is centred on core output 2m - d_down.
    let (d_up, d_down) = (vs.design().up_delay(), vs.design().down_delay());
    let goal = 2.0 * frames as f64 + d_up + vs.core().delay();
    let change_frame = |c: &Change| (c.at * f64::from(rate)).round() as usize;

    let mut src = vec![vec![0.0; 2 * block + 1]; channels];
    let mut out = vec![vec![0.0; block]; channels];
    let mut result: Vec<Vec<f64>> = vec![Vec::new(); channels];
    let (mut consumed, mut produced, mut next_change) = (0usize, 0usize, 0usize);
    let mut core_index = 0u64;
    let mut prev_pos: Option<f64> = None;
    let mut prev_q: Option<u128> = None;
    let mut end: Option<usize> = (frames == 0).then_some(0);
    let mut change_frames = Vec::new();
    let mut stage_time = 0.0f64;
    let (mut low_speed, mut peak_speed, mut end_speed) = (vs.speed(), vs.speed(), vs.speed());
    loop {
        while next_change < changes.len() && change_frame(&changes[next_change]) <= produced {
            let c = changes[next_change];
            vs.set_speed(c.speed, c.ramp_ms)?;
            change_frames.push(produced);
            next_change += 1;
        }
        if end.is_some_and(|e| produced >= e) {
            break;
        }
        // Cut the call at the next change and at the end.
        let mut n = block;
        if let Some(c) = changes.get(next_change) {
            n = n.min(change_frame(c) - produced);
        }
        if let Some(e) = end {
            n = n.min(e - produced);
        }
        let t0 = Instant::now();
        let n_src = vs.plan(n)?;
        stage_time += t0.elapsed().as_secs_f64();
        // The exact position of every core output of the call: the end of
        // the input, and the speeds applied until then (2G frames per
        // output, exact).
        vs.planned_positions(|q| {
            let p = (q >> 64) as f64 + (q as u64) as f64 / 18_446_744_073_709_551_616.0;
            if end.is_some() {
                return;
            }
            if let Some(a) = prev_q {
                let step = (q - a) as f64 / 18_446_744_073_709_551_616.0;
                low_speed = low_speed.min(step);
                peak_speed = peak_speed.max(step);
                end_speed = step;
            }
            prev_q = Some(q);
            if p >= goal {
                let k = match prev_pos {
                    Some(a) if p > a => core_index as f64 - 1.0 + (goal - a) / (p - a),
                    _ => core_index as f64,
                };
                end = Some(((k + d_down) / 2.0).ceil() as usize);
            }
            prev_pos = Some(p);
            core_index += 1;
        });
        for (s, x) in src.iter_mut().zip(input) {
            for (j, v) in s[..n_src].iter_mut().enumerate() {
                *v = x.get(consumed + j).copied().unwrap_or(0.0);
            }
        }
        let ins: Vec<&[f64]> = src.iter().map(|s| &s[..n_src]).collect();
        let mut outs: Vec<&mut [f64]> = out.iter_mut().map(|o| &mut o[..n]).collect();
        let t0 = Instant::now();
        vs.process(&ins, &mut outs);
        stage_time += t0.elapsed().as_secs_f64();
        for (r, o) in result.iter_mut().zip(&out) {
            r.extend_from_slice(&o[..n]);
        }
        consumed += n_src;
        produced += n;
    }
    // The call that found the end may have run past it.
    if let Some(e) = end {
        for r in &mut result {
            r.truncate(e);
        }
    }
    let trimmed = if trim {
        (start_latency.round() as usize).min(result.first().map_or(0, Vec::len))
    } else {
        0
    };
    for r in &mut result {
        r.drain(..trimmed);
    }
    Ok(Rendered {
        out: result,
        start_latency,
        trimmed,
        change_frames,
        low_speed,
        peak_speed,
        end_speed,
        stage_time,
    })
}

pub fn run(args: RenderArgs) -> Result<i32> {
    let changes = args
        .changes
        .iter()
        .map(|c| parse_change(c))
        .collect::<Result<Vec<_>>>()?;
    let wav = wavio::read_wav(&args.input)?;
    let (rate, channels, frames) = (wav.rate, wav.channels, wav.frames());
    let planar: Vec<Vec<f64>> = (0..channels)
        .map(|c| {
            wav.samples
                .iter()
                .skip(c)
                .step_by(channels)
                .map(|&v| f64::from(v))
                .collect()
        })
        .collect();
    let r = render(
        &planar,
        rate,
        args.speed,
        &changes,
        args.block,
        args.trim_latency,
    )?;
    let out_frames = r.out.first().map_or(0, Vec::len);
    let mut interleaved = Vec::with_capacity(out_frames * channels);
    let mut peak = 0.0f64;
    for i in 0..out_frames {
        for c in &r.out {
            peak = peak.max(c[i].abs());
            interleaved.push(c[i] as f32);
        }
    }
    wavio::write_wav_f32(&args.output, rate, channels, &interleaved)?;

    let out_seconds = out_frames as f64 / f64::from(rate);
    println!(
        "input:  {} ({rate} Hz, {channels} ch, {} frames)",
        args.input.display(),
        frames
    );
    println!(
        "output: {} ({rate} Hz, {channels} ch, f32, {out_frames} frames, {out_seconds:.3} s)",
        args.output.display()
    );
    println!(
        "speed: {} at the start, {} at the end, {:.6}..{:.6} applied, {} change(s){}",
        args.speed,
        r.end_speed,
        r.low_speed,
        r.peak_speed,
        changes.len(),
        if r.change_frames.is_empty() {
            String::new()
        } else {
            format!(" at output frames {:?}", r.change_frames)
        }
    );
    println!(
        "latency: {} frames at the start speed ({})",
        r.start_latency,
        if args.trim_latency {
            format!("{} trimmed", r.trimmed)
        } else {
            "kept".to_owned()
        }
    );
    println!("peak: {:.2} dBFS", crate::signal::peak_dbfs(peak));
    let ms_per_s = if out_seconds > 0.0 {
        r.stage_time / out_seconds * 1e3
    } else {
        0.0
    };
    println!(
        "stage: {:.3} s for {out_seconds:.3} s of output, {ms_per_s:.2} ms per second \
         of audio ({:.1}x real time)",
        r.stage_time,
        crate::process::realtime_factor(out_seconds, r.stage_time)
    );
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changes_parse() {
        assert_eq!(
            parse_change("1.5:1.08").unwrap(),
            Change {
                at: 1.5,
                speed: 1.08,
                ramp_ms: 0.0
            }
        );
        assert_eq!(parse_change("0:0.5:400").unwrap().ramp_ms, 400.0);
        for bad in ["", "1", "1:2:3:4", "x:1", "1:y", "-1:1", "1:1:nan", "inf:1"] {
            assert!(parse_change(bad).is_err(), "{bad}");
        }
    }

    fn tone(rate: u32, seconds: f64, f: f64) -> Vec<Vec<f64>> {
        let n = (seconds * f64::from(rate)).round() as usize;
        let w = 2.0 * std::f64::consts::PI * f / f64::from(rate);
        vec![(0..n).map(|j| 0.5 * (w * j as f64).sin()).collect()]
    }

    fn change(at: f64, speed: f64, ramp_ms: f64) -> Change {
        Change { at, speed, ramp_ms }
    }

    /// The end of the input is found on the stage's trajectory: the output
    /// length does not depend on the call size, also when the input ends
    /// inside a ramp, and the tone plays to its end.
    #[test]
    fn the_output_length_does_not_depend_on_the_block_size() {
        let cases: [(f64, Vec<Change>); 4] = [
            (0.5, vec![change(0.0, 2.0, 0.0)]),
            (1.0, vec![]),
            (2.0, vec![change(0.01, 0.5, 30.0)]),
            (1.0, vec![change(0.02, 1.7, 0.0), change(0.03, 0.6, 10.0)]),
        ];
        let x = tone(44_100, 0.05, 1_000.0);
        for (speed, changes) in &cases {
            let mut lengths = Vec::new();
            let mut speeds = Vec::new();
            for block in [1usize, 7, 333, 1024, 8192] {
                let r = render(&x, 44_100, *speed, changes, block, true).unwrap();
                speeds.push([r.low_speed, r.peak_speed, r.end_speed]);
                let out = &r.out[0];
                // The tone runs to the end: the last 50 frames still carry it.
                let tail = out[out.len() - 50..]
                    .iter()
                    .fold(0.0f64, |a, v| a.max(v.abs()));
                assert!(tail > 0.3, "speed {speed}, block {block}: tail {tail}");
                lengths.push(out.len());
            }
            let (lo, hi) = (lengths.iter().min().unwrap(), lengths.iter().max().unwrap());
            println!("speed {speed}, {changes:?}: {lengths:?} frames, speeds {speeds:?}");
            assert!(hi - lo <= 1, "lengths {lengths:?}");
            // The speeds reported are those up to the end of the input,
            // whatever the call size, up to how finely the pieces follow the
            // curve.
            for s in &speeds {
                for (a, b) in s.iter().zip(&speeds[0]) {
                    assert!((a - b).abs() < 1e-3, "speeds {speeds:?}");
                }
            }
        }
        // The first case ends inside its ramp: from 0.5 up, still short of 2.
        let r = render(&x, 44_100, 0.5, &cases[0].1, 8192, true).unwrap();
        assert_eq!(r.low_speed, 0.5);
        assert_eq!(r.peak_speed, r.end_speed);
        assert!(r.end_speed > 0.6 && r.end_speed < 1.9, "{}", r.end_speed);
        // At constant speed the length is the input's over the speed.
        for s in [0.5, 1.0, 1.0123, 2.0] {
            let r = render(&x, 44_100, s, &[], 1024, true).unwrap();
            let want = 2205.0 / s;
            assert!(
                (r.out[0].len() as f64 - want).abs() <= 1.0,
                "{s}: {}",
                r.out[0].len()
            );
        }
        assert!(render(&[vec![]], 44_100, 1.0, &[], 64, true).unwrap().out[0].is_empty());
    }

    /// Changes take effect at their own output frames, whatever the call
    /// size.
    #[test]
    fn changes_apply_at_their_frames() {
        let x = tone(44_100, 0.5, 1_000.0);
        let changes = [change(0.03, 1.5, 0.0), change(0.07, 1.0, 0.0)];
        let mut outs = Vec::new();
        for block in [1usize, 8192] {
            let r = render(&x, 44_100, 1.0, &changes, block, false).unwrap();
            assert_eq!(r.change_frames, vec![1323, 3087], "block {block}");
            outs.push(r.out.into_iter().next().unwrap());
        }
        let unity = render(&x, 44_100, 1.0, &[], 8192, false).unwrap();
        let (a, b) = (&outs[0], &outs[1]);
        assert_eq!(a.len(), b.len());
        // The same trajectory whatever the call size, up to how finely the
        // pieces follow the curve ...
        let diff = a
            .iter()
            .zip(b)
            .fold(0.0f64, |m, (x, y)| m.max((x - y).abs()));
        assert!(diff < 0.05, "block 1 against 8192: {diff:e}");
        // ... and not unity.
        let off = a
            .iter()
            .zip(&unity.out[0])
            .fold(0.0f64, |m, (x, y)| m.max((x - y).abs()));
        assert!(off > 0.1, "{off}");
    }
}
