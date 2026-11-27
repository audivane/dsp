//! Partitioned FFT convolution (non-uniform partitions for long IRs), on
//! the shared engine of `crate::nupc` with one branch per channel: the
//! default schedule (`crate::nupc::plan_schedule`, blocks growing 4x per
//! stage up to 32768, every tail stage at twice its block), stage 0 on the
//! base block (the latency, `base_block` frames), the tail stages
//! time-sliced over their periods. See `crate::nupc` for the schedule's
//! rules and the slicing.

use crate::Stage;
#[cfg(test)]
use crate::fft::complex_mul_split;
use crate::fft::{RealFftPair, RingFifo, SplitSpectrum};
use crate::nupc::{HeadChannel, HeadStage, StagePlan, TailChannel, TailStage, validate_plan};
#[cfg(test)]
use crate::nupc::{MAX_PART, plan_schedule};

/// Pre-FFT'd IR partitions of `ir[offset..end)`, scaled by `1 / (2 * block)`.
fn ir_partitions(
    fft: &mut RealFftPair,
    ir: &[f64],
    offset: usize,
    block: usize,
    end: usize,
) -> Vec<SplitSpectrum> {
    if ir.len() <= offset {
        return Vec::new();
    }
    let inv_n = 1.0 / (2 * block) as f64;
    let stage_end = ir.len().min(end);
    let num_parts = (stage_end - offset).div_ceil(block);
    (0..num_parts)
        .map(|p| {
            let start = offset + p * block;
            let stop = (start + block).min(stage_end);
            fft.forward_padded_scaled(&ir[start..stop], inv_n)
        })
        .collect()
}

/// Stage 0 of the convolver: one branch, partitions `[ir][part][0]`.
fn head_stage(irs: &[Vec<f64>], block: usize, end: usize) -> HeadStage {
    let mut fft = RealFftPair::new(2 * block);
    let spectra = irs
        .iter()
        .map(|ir| {
            ir_partitions(&mut fft, ir, 0, block, end)
                .into_iter()
                .map(|s| vec![s])
                .collect()
        })
        .collect();
    HeadStage::from_spectra(block, 1, spectra)
}

/// A tail stage of the convolver: one branch, partitions `[ir][0][part]`.
fn tail_stage(irs: &[Vec<f64>], plan: StagePlan, base: usize) -> TailStage {
    let mut fft = RealFftPair::new(2 * plan.block);
    let spectra = irs
        .iter()
        .map(|ir| {
            vec![ir_partitions(
                &mut fft,
                ir,
                plan.offset,
                plan.block,
                plan.end,
            )]
        })
        .collect();
    TailStage::from_spectra(spectra, plan, base, 1)
}

// ---------------------------------------------------------------- convolver

struct ChannelState {
    ir_idx: usize,
    in_fifo: RingFifo,
    out_fifo: RingFifo,
    head: HeadChannel,
    /// One entry per tail stage; `None` when this channel's IR ends before it.
    tails: Vec<Option<TailChannel>>,
}

pub struct Convolver {
    channels: usize,
    max_block: usize,
    base_block: usize,
    /// The longest IR, taps: how long the output rings after the input ends.
    ir_len: usize,
    head: Option<HeadStage>,
    tails: Vec<TailStage>,
    ch_states: Vec<ChannelState>,
    block_in_scratch: Vec<f64>,
    block_out_scratch: Vec<f64>,
    /// Estimated work (flop-like units) done by the last `process` call.
    last_cost: f64,
    /// [`filter_id`] of the IRs and partition schedule: another convolver
    /// with the same one runs the same filter ([`Self::inherit_state`]).
    filter_id: u64,
}

/// A fingerprint of a convolver's filter: every IR tap's bits, the IR
/// count and the partition schedule. Off the real-time thread (at build).
fn filter_id(irs: &[Vec<f64>], plan: &[StagePlan], base_block: usize) -> u64 {
    // splitmix64 over the words, chained.
    let mut h: u64 = 0x243F_6A88_85A3_08D3;
    let mut mix = |w: u64| {
        let mut z = (h ^ w).wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        h = z ^ (z >> 31);
    };
    mix(base_block as u64);
    for p in plan {
        mix(p.offset as u64);
        mix(p.block as u64);
        mix(p.end as u64);
    }
    mix(irs.len() as u64);
    for ir in irs {
        mix(ir.len() as u64);
        for &x in ir {
            mix(x.to_bits());
        }
    }
    h
}

impl Convolver {
    /// `irs`: either one IR (applied to every channel) or exactly `channels`
    /// IRs. `max_block`: the largest frame count `process` will receive.
    pub fn new(irs: &[Vec<f64>], channels: usize, max_block: usize) -> Self {
        let base = Self::base_block_for(max_block);
        let max_ir_len = irs.iter().map(|ir| ir.len()).max().unwrap_or(0);
        Self::with_plan(
            irs,
            channels,
            max_block,
            &crate::nupc::plan_schedule(base, max_ir_len),
        )
    }

    fn base_block_for(max_block: usize) -> usize {
        if max_block == 0 {
            64
        } else {
            max_block.next_power_of_two().clamp(64, 1024)
        }
    }

    /// Build with an explicit partition schedule (validated; see the module
    /// docs for the rules).
    pub(crate) fn with_plan(
        irs: &[Vec<f64>],
        channels: usize,
        max_block: usize,
        plan: &[StagePlan],
    ) -> Self {
        let base_block = Self::base_block_for(max_block);
        if channels == 0 {
            return Self {
                channels: 0,
                max_block,
                base_block,
                ir_len: irs.iter().map(Vec::len).max().unwrap_or(0),
                head: None,
                tails: Vec::new(),
                ch_states: Vec::new(),
                block_in_scratch: Vec::new(),
                block_out_scratch: Vec::new(),
                last_cost: 0.0,
                filter_id: 0,
            };
        }
        assert!(
            irs.len() == 1 || irs.len() == channels,
            "Convolver expects 1 mono IR or exactly {channels} IRs, got {}",
            irs.len()
        );
        validate_plan(plan, base_block);

        let head = head_stage(irs, base_block, plan[0].end);
        let tails: Vec<TailStage> = plan[1..]
            .iter()
            .map(|&p| tail_stage(irs, p, base_block))
            .collect();

        let mono = irs.len() == 1;
        let fifo_cap = base_block + max_block.max(base_block);
        let ch_states = (0..channels)
            .map(|ch| {
                let ir_idx = if mono { 0 } else { ch };
                let mut out_fifo = RingFifo::with_capacity(fifo_cap);
                out_fifo.reset_zeros(base_block);
                ChannelState {
                    ir_idx,
                    in_fifo: RingFifo::with_capacity(fifo_cap),
                    out_fifo,
                    head: head.new_channel(ir_idx),
                    tails: tails.iter().map(|t| t.new_channel(ir_idx)).collect(),
                }
            })
            .collect();

        Self {
            channels,
            max_block,
            base_block,
            ir_len: irs.iter().map(Vec::len).max().unwrap_or(0),
            head: Some(head),
            tails,
            ch_states,
            block_in_scratch: vec![0.0; base_block],
            block_out_scratch: vec![0.0; base_block],
            last_cost: 0.0,
            filter_id: filter_id(irs, plan, base_block),
        }
    }

    /// Frames the output still rings after its delay once the input ends:
    /// the longest IR's length less one (a room IR's peak can sit far into
    /// it). The stream-end flush adds it to [`Stage::latency_frames`].
    pub fn ir_tail_frames(&self) -> usize {
        self.ir_len.saturating_sub(1)
    }

    /// Estimated work of the last `process` call, in flop-like units. Lets
    /// tests check that tail-stage work is spread evenly without timing.
    pub fn last_process_cost(&self) -> f64 {
        self.last_cost
    }

    /// One single-channel convolver per channel, each running its channel
    /// exactly as this one does (bit for bit), from this one's state. The
    /// IR spectra and FFT tables are shared; each gets its own scratch.
    /// Off the real-time thread (it allocates); `self` is left without
    /// channels.
    pub fn split_channels(&mut self) -> Vec<Convolver> {
        let states = std::mem::take(&mut self.ch_states);
        self.channels = 0;
        states
            .into_iter()
            .map(|ch| Convolver {
                channels: 1,
                max_block: self.max_block,
                base_block: self.base_block,
                ir_len: self.ir_len,
                head: self.head.clone(),
                tails: self.tails.clone(),
                ch_states: vec![ch],
                block_in_scratch: self.block_in_scratch.clone(),
                block_out_scratch: self.block_out_scratch.clone(),
                last_cost: 0.0,
                filter_id: self.filter_id,
            })
            .collect()
    }

    /// Bytes [`Self::inherit_state`] copies: every channel's FIFOs, delay
    /// lines and tail-stage buffers.
    pub fn state_bytes(&self) -> usize {
        self.ch_states
            .iter()
            .map(|c| {
                8 * (c.in_fifo.capacity() + c.out_fifo.capacity())
                    + c.head.bytes()
                    + c.tails.iter().flatten().map(|t| t.bytes()).sum::<usize>()
            })
            .sum()
    }

    /// Whether [`Self::inherit_state`] can take `from`'s state: the same
    /// filter (IRs and schedule), block sizes and channels.
    pub fn can_inherit(&self, from: &Convolver) -> bool {
        self.channels > 0
            && self.channels == from.channels
            && self.max_block == from.max_block
            && self.base_block == from.base_block
            && self.ir_len == from.ir_len
            && self.filter_id == from.filter_id
            && self.tails.len() == from.tails.len()
            && self.ch_states.iter().zip(&from.ch_states).all(|(d, s)| {
                d.ir_idx == s.ir_idx
                    && d.in_fifo.capacity() == s.in_fifo.capacity()
                    && d.out_fifo.capacity() == s.out_fifo.capacity()
                    && d.head.same_shape(&s.head)
                    && d.tails.len() == s.tails.len()
                    && d.tails.iter().zip(&s.tails).all(|t| match t {
                        (Some(d), Some(s)) => d.same_shape(s),
                        (None, None) => true,
                        _ => false,
                    })
            })
    }

    /// Continue from `from`'s state, a convolver of the same filter
    /// ([`Self::can_inherit`]; otherwise nothing changes and this returns
    /// `false`): its input history, pending output and place in the tail
    /// stages' sliced work, so this one plays on exactly as `from` would,
    /// the filter's whole ring included. A chain swap that keeps the
    /// convolution hands it over this way. Real-time safe: it copies into
    /// buffers this convolver owns (bounded by the filter's size).
    pub fn inherit_state(&mut self, from: &Convolver) -> bool {
        if !self.can_inherit(from) {
            return false;
        }
        for (d, s) in self.ch_states.iter_mut().zip(&from.ch_states) {
            d.in_fifo.copy_from(&s.in_fifo);
            d.out_fifo.copy_from(&s.out_fifo);
            d.head.copy_from(&s.head);
            for (d, s) in d.tails.iter_mut().zip(&s.tails) {
                if let (Some(d), Some(s)) = (d, s) {
                    d.copy_from(s);
                }
            }
        }
        true
    }
}

impl Stage for Convolver {
    fn process(&mut self, chans: &mut [&mut [f64]]) {
        if self.channels == 0 {
            assert!(chans.is_empty());
            return;
        }
        assert_eq!(chans.len(), self.channels, "channel count mismatch");
        let n = chans[0].len();
        assert!(
            n <= self.max_block,
            "frame count {n} exceeds max_block {}",
            self.max_block
        );
        for ch in chans.iter() {
            assert_eq!(ch.len(), n, "all channels must have the same frame count");
        }
        self.last_cost = 0.0;
        if n == 0 {
            return;
        }

        let b0 = self.base_block;
        let head = self.head.as_mut().expect("head stage exists with channels");

        for (c, ch_buf) in chans.iter_mut().enumerate() {
            let ch = &mut self.ch_states[c];
            let ir_idx = ch.ir_idx;
            ch.in_fifo.push_slice(ch_buf);

            while ch.in_fifo.len() >= b0 {
                ch.in_fifo.pop_slice(&mut self.block_in_scratch);
                head.process_block(
                    ir_idx,
                    &mut ch.head,
                    &self.block_in_scratch,
                    &mut self.block_out_scratch,
                );
                self.last_cost += head.block_cost(ir_idx);
                for (stage, tail) in self.tails.iter_mut().zip(ch.tails.iter_mut()) {
                    if let Some(tail) = tail {
                        self.last_cost += stage.tick(
                            ir_idx,
                            tail,
                            &self.block_in_scratch,
                            &mut self.block_out_scratch,
                        );
                    }
                }
                ch.out_fifo.push_slice(&self.block_out_scratch);
            }

            ch.out_fifo.pop_slice(ch_buf);
        }
    }

    fn latency_frames(&self) -> usize {
        self.base_block
    }

    fn reset(&mut self) {
        for ch in &mut self.ch_states {
            ch.in_fifo.clear();
            ch.out_fifo.reset_zeros(self.base_block);
            ch.head.reset();
            for tail in ch.tails.iter_mut().flatten() {
                tail.reset();
            }
        }
        self.last_cost = 0.0;
    }
}

/// Maximum allowed combined impulse response length in taps (at graph rate).
pub const MAX_COMBINED_IR_TAPS: usize = 4_000_000;

/// Compute the combined length of linearly convolving IRs of lengths `lengths`.
pub fn combined_ir_length(lengths: &[usize]) -> Result<usize, crate::chain::ChainError> {
    if lengths.is_empty() {
        return Err(crate::chain::ChainError::MissingIr);
    }
    let mut sum = 0usize;
    for &len in lengths {
        if len == 0 {
            return Err(crate::chain::ChainError::EmptyIr);
        }
        sum = sum.saturating_add(len);
    }
    let taps = sum - (lengths.len() - 1);
    if taps > MAX_COMBINED_IR_TAPS {
        return Err(crate::chain::ChainError::IrTooLong {
            taps,
            max: MAX_COMBINED_IR_TAPS,
        });
    }
    Ok(taps)
}

/// Linearly convolve all enabled impulse responses (each paired with its
/// `gain_db`) into one combined IR per channel (or a single mono IR if every
/// input IR is mono), enforcing [`MAX_COMBINED_IR_TAPS`].
///
/// Mono IRs (`len == 1`) are applied to every channel; multi-channel IRs must
/// match `channels` and map channel-for-channel (e.g. stereo L→L, R→R).
pub fn combine_irs(
    irs: &[(&[Vec<f64>], f64)],
    channels: usize,
) -> Result<Vec<Vec<f64>>, crate::chain::ChainError> {
    use crate::chain::ChainError;

    if channels == 0 {
        return Err(ChainError::ZeroChannels);
    }
    if irs.is_empty() {
        return Err(ChainError::MissingIr);
    }
    for &(ir, _) in irs {
        if ir.is_empty() || ir.iter().any(Vec::is_empty) {
            return Err(ChainError::EmptyIr);
        }
        if ir.len() != 1 && ir.len() != channels {
            return Err(ChainError::IrCount {
                irs: ir.len(),
                channels,
            });
        }
    }

    let all_mono = irs.iter().all(|&(ir, _)| ir.len() == 1);
    let out_channels = if all_mono { 1 } else { channels };
    let total_gain: f64 = irs.iter().map(|&(_, g)| crate::db_to_lin(g)).product();

    let mut out_lens = Vec::with_capacity(out_channels);
    let mut max_out_len = 0usize;
    let mut ch_lengths = Vec::with_capacity(irs.len());
    for c in 0..out_channels {
        ch_lengths.clear();
        for &(ir, _) in irs {
            let slice = if ir.len() == 1 { &ir[0] } else { &ir[c] };
            ch_lengths.push(slice.len());
        }
        let len = combined_ir_length(&ch_lengths)?;
        max_out_len = max_out_len.max(len);
        out_lens.push(len);
    }

    if irs.len() == 1 {
        let (ir, _) = irs[0];
        return Ok((0..out_channels)
            .map(|c| {
                let src = if ir.len() == 1 { &ir[0] } else { &ir[c] };
                src.iter().map(|&s| s * total_gain).collect()
            })
            .collect());
    }

    let fft_len = max_out_len.max(2).next_power_of_two();
    let mut fft = RealFftPair::new(fft_len);
    let spec_len = fft.spec_len();
    let mut acc = SplitSpectrum::new(spec_len);
    let mut tmp = SplitSpectrum::new(spec_len);
    let mut time_out = vec![0.0; fft_len];
    let scale = total_gain / (fft_len as f64);

    let mut combined = Vec::with_capacity(out_channels);
    for (c, &out_len) in out_lens.iter().enumerate() {
        let first = if irs[0].0.len() == 1 {
            &irs[0].0[0]
        } else {
            &irs[0].0[c]
        };
        fft.forward_padded_into_split(first, 1.0, &mut acc);

        for &(ir, _) in &irs[1..] {
            let slice = if ir.len() == 1 { &ir[0] } else { &ir[c] };
            fft.forward_padded_into_split(slice, 1.0, &mut tmp);
            for i in 0..spec_len {
                let ar = acc.re[i];
                let ai = acc.im[i];
                let br = tmp.re[i];
                let bi = tmp.im[i];
                acc.re[i] = ar * br - ai * bi;
                acc.im[i] = ar * bi + ai * br;
            }
        }

        fft.inverse_from_split(&acc, &mut time_out);
        let ch_out: Vec<f64> = time_out[..out_len].iter().map(|&v| v * scale).collect();
        combined.push(ch_out);
    }

    Ok(combined)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lcg(seed: &mut u64) -> f64 {
        *seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let bits = ((*seed >> 11) as f64) / ((1u64 << 53) as f64);
        bits * 2.0 - 1.0
    }

    fn naive_convolve(x: &[f64], h: &[f64]) -> Vec<f64> {
        let mut y = vec![0.0; x.len()];
        for (n, yn) in y.iter_mut().enumerate() {
            let max_k = h.len().min(n + 1);
            let mut acc = 0.0;
            for k in 0..max_k {
                acc += h[k] * x[n - k];
            }
            *yn = acc;
        }
        y
    }

    /// Reference linear convolution (first `x.len()` samples) through one
    /// big real FFT.
    fn fft_convolve(x: &[f64], h: &[f64]) -> Vec<f64> {
        if h.is_empty() {
            return vec![0.0; x.len()];
        }
        let nfft = (x.len() + h.len()).next_power_of_two().max(2);
        let mut fft = RealFftPair::new(nfft);
        let spec_x = fft.forward_padded_scaled(x, 1.0);
        let spec_h = fft.forward_padded_scaled(h, 1.0 / nfft as f64);
        let mut prod = SplitSpectrum::new(fft.spec_len());
        complex_mul_split(&mut prod, &spec_x, &spec_h);
        let mut y = vec![0.0; nfft];
        fft.inverse_from_split(&prod, &mut y);
        y.truncate(x.len());
        y
    }

    /// Feed `inputs` through `conv` in chunks of varying size (0, 1, odd,
    /// `max_block`, random) and return the outputs.
    fn run_chunks(
        conv: &mut Convolver,
        inputs: &[Vec<f64>],
        max_block: usize,
        seed: u64,
    ) -> Vec<Vec<f64>> {
        let channels = inputs.len();
        let total = inputs[0].len();
        let mut seed = seed;
        let mut outputs = vec![vec![0.0; total]; channels];
        let mut work = vec![vec![0.0; max_block]; channels];
        let mut offset = 0;
        let mut step = 0usize;
        while offset < total {
            let rem = total - offset;
            let chunk = match step % 7 {
                0 => 0,
                1 => max_block,
                2 => 1,
                3 => (max_block / 2) | 1,
                _ => {
                    let r = ((lcg(&mut seed) + 1.0) * 0.5 * max_block as f64) as usize;
                    r.clamp(1, max_block)
                }
            }
            .min(rem);
            step += 1;
            for c in 0..channels {
                work[c][..chunk].copy_from_slice(&inputs[c][offset..offset + chunk]);
            }
            {
                let mut slices: Vec<&mut [f64]> =
                    work.iter_mut().map(|v| &mut v[..chunk]).collect();
                conv.process(&mut slices);
            }
            for c in 0..channels {
                outputs[c][offset..offset + chunk].copy_from_slice(&work[c][..chunk]);
            }
            offset += chunk;
        }
        outputs
    }

    /// Run random input through a convolver and compare against the
    /// reference. Returns the worst error relative to the output peak.
    fn check_against_reference(
        irs: &[Vec<f64>],
        channels: usize,
        max_block: usize,
        input_frames: usize,
        seed: u64,
    ) -> f64 {
        let mut s = seed;
        let mut conv = Convolver::new(irs, channels, max_block);
        let lat = conv.latency_frames();
        let inputs: Vec<Vec<f64>> = (0..channels)
            .map(|_| {
                let mut x: Vec<f64> = (0..input_frames).map(|_| lcg(&mut s)).collect();
                x.resize(input_frames + lat, 0.0);
                x
            })
            .collect();
        let outputs = run_chunks(&mut conv, &inputs, max_block, seed ^ 0x55);
        let mut worst = 0.0f64;
        for c in 0..channels {
            let ir = if irs.len() == 1 { &irs[0] } else { &irs[c] };
            let expected = if input_frames * ir.len() <= 20_000_000 {
                naive_convolve(&inputs[c][..input_frames], ir)
            } else {
                fft_convolve(&inputs[c][..input_frames], ir)
            };
            let peak = expected.iter().fold(1e-30f64, |m, v| m.max(v.abs()));
            let mut err = outputs[c][..lat].iter().fold(0.0f64, |m, v| m.max(v.abs()));
            for i in 0..input_frames {
                err = err.max((outputs[c][i + lat] - expected[i]).abs());
            }
            let rel = err / peak.max(1.0);
            assert!(
                rel <= 1e-12,
                "ir_len={}, ch={c}, max_block={max_block}: max_err={err:.3e} (peak {peak:.3})",
                ir.len()
            );
            worst = worst.max(rel);
        }
        worst
    }

    #[test]
    fn default_schedule_starts_each_tail_stage_at_twice_its_block() {
        // Block 1024: the 32768-block stage must start at IR offset 65536,
        // not 2 × 32768 short of it.
        let plan = plan_schedule(1024, 1 << 20);
        assert_eq!(
            plan,
            vec![
                StagePlan {
                    offset: 0,
                    block: 1024,
                    end: 8192
                },
                StagePlan {
                    offset: 8192,
                    block: 4096,
                    end: 32_768
                },
                StagePlan {
                    offset: 32_768,
                    block: 16_384,
                    end: 65_536
                },
                StagePlan {
                    offset: 65_536,
                    block: 32_768,
                    end: usize::MAX
                },
            ]
        );
        for base in [64, 128, 256, 512, 1024] {
            for ir_len in [0, 1, 8 * base, 8 * base + 1, 70_000, 600_000] {
                let plan = plan_schedule(base, ir_len);
                validate_plan(&plan, base);
                for st in &plan[1..] {
                    assert_eq!(st.offset, 2 * st.block, "base {base}: {st:?}");
                    assert!(st.block <= MAX_PART);
                }
            }
        }
    }

    #[test]
    fn single_tap_at_65536_lands_at_65536_plus_latency() {
        let mut ir = vec![0.0; 65_537];
        ir[65_536] = 1.0;
        let mut conv = Convolver::new(&[ir], 1, 1024);
        let lat = conv.latency_frames();
        assert_eq!(lat, 1024);
        let total = 70_000;
        let mut x = vec![0.0; total];
        x[0] = 1.0;
        let out = run_chunks(&mut conv, &[x], 1024, 1).pop().unwrap();
        let (peak_idx, peak) = out
            .iter()
            .enumerate()
            .fold((0, 0.0f64), |(bi, bv), (i, &v)| {
                if v.abs() > bv { (i, v.abs()) } else { (bi, bv) }
            });
        assert_eq!(peak_idx, 66_560);
        assert!((peak - 1.0).abs() < 1e-12);
    }

    #[test]
    fn impulse_taps_at_every_partition_boundary() {
        for max_block in [1024usize, 64, 256, 100, 512] {
            let base = Convolver::base_block_for(max_block);
            let plan = plan_schedule(base, 300_000);
            // Taps at every partition boundary of every stage (±1), plus
            // fixed positions around and beyond 65536.
            let mut positions = vec![
                0usize, 1, 65_535, 65_536, 65_537, 98_303, 98_304, 131_071, 131_072, 131_073,
                200_000, 262_143,
            ];
            for st in &plan {
                let end = st.end.min(270_000);
                let mut p = st.offset;
                while p < end {
                    positions.extend([p.saturating_sub(1), p, p + 1]);
                    p += st.block;
                }
            }
            positions.sort_unstable();
            positions.dedup();
            let len = positions.last().unwrap() + 1;
            let mut ir = vec![0.0; len];
            for (i, &p) in positions.iter().enumerate() {
                ir[p] = if i % 2 == 0 { 1.0 } else { -0.5 } * (1.0 - 1e-4 * i as f64);
            }

            let mut conv = Convolver::new(std::slice::from_ref(&ir), 1, max_block);
            let lat = conv.latency_frames();
            // Two impulses at different block phases: y = ir + 0.25 ir[n-777].
            let total = len + 2000 + lat;
            let mut x = vec![0.0; total];
            x[0] = 1.0;
            x[777] = 0.25;
            let out = run_chunks(&mut conv, &[x], max_block, max_block as u64)
                .pop()
                .unwrap();
            let tap = |n: isize| {
                if n >= 0 && (n as usize) < len {
                    ir[n as usize]
                } else {
                    0.0
                }
            };
            for (i, &y) in out.iter().enumerate() {
                let n = i as isize - lat as isize;
                let expected = tap(n) + 0.25 * tap(n - 777);
                assert!(
                    (y - expected).abs() < 1e-12,
                    "max_block {max_block}: output {i} (IR index {n}) = {y}, expected {expected}"
                );
            }
        }
    }

    #[test]
    fn convolver_matches_naive_various_ir_lengths_and_chunks() {
        let mut seed = 42u64;
        for ir_len in [1, 7, 1000] {
            let ir: Vec<f64> = (0..ir_len).map(|_| lcg(&mut seed) * 0.1).collect();
            check_against_reference(&[ir], 2, 128, 4500, 100 + ir_len as u64);
        }
    }

    #[test]
    fn convolver_mono_shared_and_mixed_channel_lengths() {
        let mut seed = 314u64;
        let mono_ir: Vec<f64> = (0..1500).map(|_| lcg(&mut seed) * 0.1).collect();
        check_against_reference(&[mono_ir], 3, 100, 3500, 315);

        let ir0: Vec<f64> = Vec::new();
        let ir1: Vec<f64> = (0..9).map(|_| lcg(&mut seed) * 0.1).collect();
        let ir2: Vec<f64> = (0..4000).map(|_| lcg(&mut seed) * 0.1).collect();
        check_against_reference(&[ir0, ir1, ir2], 3, 128, 5000, 316);
    }

    #[test]
    fn random_70k_stereo_irs_match_fft_reference() {
        let mut seed = 12345u64;
        let irs: Vec<Vec<f64>> = (0..2)
            .map(|c| {
                (0..70_000 + 3 * c)
                    .map(|i| lcg(&mut seed) * (-1e-5 * i as f64).exp() * 0.05)
                    .collect()
            })
            .collect();
        for max_block in [1024, 333] {
            check_against_reference(&irs, 2, max_block, 90_000, 999 + max_block as u64);
        }
    }

    #[test]
    fn random_200k_ir_matches_fft_reference() {
        let mut seed = 777u64;
        let ir: Vec<f64> = (0..200_000).map(|_| lcg(&mut seed) * 0.02).collect();
        for max_block in [64, 1000] {
            check_against_reference(
                std::slice::from_ref(&ir),
                1,
                max_block,
                230_000,
                max_block as u64,
            );
        }
    }

    #[test]
    fn random_600k_ir_matches_fft_reference() {
        let mut seed = 4242u64;
        let ir: Vec<f64> = (0..600_000).map(|_| lcg(&mut seed) * 0.01).collect();
        let worst = check_against_reference(&[ir], 1, 257, 640_000, 17);
        eprintln!("600k taps: worst error relative to peak {worst:.2e}");
    }

    #[test]
    fn explicit_schedule_with_extra_stage_delay() {
        // Stages that start later than twice their block (delay made up in
        // the frequency-domain delay line), on both the direct and the
        // four-step engine.
        let plan = [
            StagePlan {
                offset: 0,
                block: 64,
                end: 512,
            },
            StagePlan {
                offset: 512,
                block: 128,
                end: 8192,
            }, // 4·B: 2 extra slots
            StagePlan {
                offset: 8192,
                block: 4096,
                end: 24_576,
            }, // 2·B, four-step
            StagePlan {
                offset: 24_576,
                block: 8192,
                end: usize::MAX,
            }, // 3·B, four-step
        ];
        let mut seed = 99u64;
        let ir: Vec<f64> = (0..45_000).map(|_| lcg(&mut seed) * 0.05).collect();
        let mut conv = Convolver::with_plan(std::slice::from_ref(&ir), 1, 64, &plan);
        let lat = conv.latency_frames();
        let n = 60_000;
        let mut x: Vec<f64> = (0..n).map(|_| lcg(&mut seed)).collect();
        x.resize(n + lat, 0.0);
        let out = run_chunks(&mut conv, std::slice::from_ref(&x), 64, 3)
            .pop()
            .unwrap();
        let expected = fft_convolve(&x[..n], &ir);
        let peak = expected.iter().fold(0.0f64, |m, v| m.max(v.abs()));
        for i in 0..n {
            assert!(
                (out[i + lat] - expected[i]).abs() <= 1e-12 * peak.max(1.0),
                "sample {i}: {} vs {}",
                out[i + lat],
                expected[i]
            );
        }
    }

    #[test]
    #[should_panic(expected = "at least twice")]
    fn schedule_without_slack_is_rejected() {
        let plan = [
            StagePlan {
                offset: 0,
                block: 64,
                end: 256,
            },
            StagePlan {
                offset: 256,
                block: 256,
                end: usize::MAX,
            },
        ];
        let _ = Convolver::with_plan(&[vec![1.0; 1000]], 1, 64, &plan);
    }

    /// 1 s IR at 192 kHz, block 256 (the stated worst case for tail spikes).
    fn one_second_ir_192k(channels: usize) -> Vec<Vec<f64>> {
        let mut seed = 0x1234_5678_u64;
        (0..channels)
            .map(|_| {
                (0..192_000)
                    .map(|i| lcg(&mut seed) * (-(i as f64) / 40_000.0).exp() * 0.01)
                    .collect()
            })
            .collect()
    }

    #[test]
    fn tail_work_is_spread_evenly_across_process_calls() {
        let irs = one_second_ir_192k(2);
        let block = 256;
        let mut conv = Convolver::new(&irs, 2, block);
        assert_eq!(plan_schedule(256, 192_000).last().unwrap().block, MAX_PART);
        let mut a = vec![0.0; block];
        let mut b = vec![0.0; block];
        let mut costs = Vec::new();
        // Three periods of the largest stage after a one-period warm-up.
        let calls = 4 * MAX_PART / block;
        for i in 0..calls {
            a[0] = 1.0;
            b[0] = -1.0;
            conv.process(&mut [&mut a, &mut b]);
            if i >= MAX_PART / block {
                costs.push(conv.last_process_cost());
            }
        }
        let mean = costs.iter().sum::<f64>() / costs.len() as f64;
        let max = costs.iter().fold(0.0f64, |m, &c| m.max(c));
        let min = costs.iter().fold(f64::MAX, |m, &c| m.min(c));
        eprintln!(
            "work per call: mean {mean:.0}, min {min:.0}, max {max:.0} (max/mean {:.3})",
            max / mean
        );
        assert!(max <= 1.15 * mean, "max {max} vs mean {mean}");
    }

    #[test]
    fn convolver_reset_clears_state() {
        let mut seed = 77u64;
        let ir: Vec<f64> = (0..40_000).map(|_| lcg(&mut seed) * 0.1).collect();
        let n = 50_000;
        let x: Vec<f64> = (0..n).map(|_| lcg(&mut seed)).collect();
        let mut fresh = Convolver::new(std::slice::from_ref(&ir), 1, 256);
        let reference = run_chunks(&mut fresh, std::slice::from_ref(&x), 256, 5);

        let mut conv = Convolver::new(std::slice::from_ref(&ir), 1, 256);
        // Stop mid-period of every stage, then reset.
        let junk: Vec<f64> = (0..12_345).map(|_| lcg(&mut seed)).collect();
        run_chunks(&mut conv, &[junk], 256, 6);
        conv.reset();
        let again = run_chunks(&mut conv, std::slice::from_ref(&x), 256, 5);
        assert_eq!(reference, again);

        // After reset, zeros in give exact zeros out.
        conv.reset();
        let z = run_chunks(&mut conv, &[vec![0.0; 70_000]], 256, 7);
        assert!(z[0].iter().all(|&v| v == 0.0));
    }

    #[test]
    #[ignore]
    fn bench_worst_call_1s_ir_192k_block256() {
        use std::time::Instant;
        let irs = one_second_ir_192k(2);
        let block = 256;
        let mut conv = Convolver::new(&irs, 2, block);
        let mut seed = 5u64;
        let mut a = vec![0.0; block];
        let mut b = vec![0.0; block];
        let calls = 192_000 * 6 / block;
        let mut times = Vec::with_capacity(calls);
        for _ in 0..calls {
            for i in 0..block {
                a[i] = lcg(&mut seed) * 0.1;
                b[i] = lcg(&mut seed) * 0.1;
            }
            let t0 = Instant::now();
            conv.process(&mut [&mut a, &mut b]);
            times.push(t0.elapsed().as_secs_f64() * 1e6);
        }
        let warm = &mut times[750..];
        let mean = warm.iter().sum::<f64>() / warm.len() as f64;
        warm.sort_by(|x, y| x.partial_cmp(y).unwrap());
        let p99 = warm[warm.len() * 99 / 100];
        let worst = *warm.last().unwrap();
        let period = block as f64 / 192_000.0 * 1e6;
        eprintln!(
            "1 s IR @192k stereo, block {block} (period {period:.0} us): mean {mean:.1} us, p99 {p99:.1} us, worst {worst:.1} us"
        );
    }

    #[test]
    #[ignore]
    fn bench_convolver_65536_stereo_192khz() {
        use std::time::Instant;
        let mut seed = 0xdeadbeef_u64;
        let ir0: Vec<f64> = (0..65_536).map(|_| lcg(&mut seed) * 0.01).collect();
        let ir1: Vec<f64> = (0..65_536).map(|_| lcg(&mut seed) * 0.01).collect();
        let block = 1024usize;
        let mut conv = Convolver::new(&[ir0, ir1], 2, block);

        let mut ch0 = vec![0.1; block];
        let mut ch1 = vec![-0.1; block];
        for _ in 0..100 {
            conv.process(&mut [&mut ch0, &mut ch1]);
        }

        let iters = 2000usize;
        let t0 = Instant::now();
        for _ in 0..iters {
            ch0[0] = 0.1;
            ch1[0] = -0.1;
            conv.process(&mut [&mut ch0, &mut ch1]);
        }
        let elapsed = t0.elapsed().as_secs_f64();
        let audio_secs = (iters * block) as f64 / 192_000.0;
        let rtf = audio_secs / elapsed;
        eprintln!(
            "Convolver 65536-tap stereo @ 192kHz (block {block}): {audio_secs:.2}s audio in {elapsed:.4}s => {rtf:.2}x real-time"
        );
    }

    #[test]
    fn combine_irs_matches_serial_convolution_null_test() {
        let mut seed = 0xCAFE_BABEu64;
        // Mix of stereo + mono + stereo IRs with non-trivial lengths and gains.
        let ir1 = vec![
            (0..311)
                .map(|i| lcg(&mut seed) * (-0.01 * i as f64).exp())
                .collect::<Vec<_>>(),
            (0..311)
                .map(|i| lcg(&mut seed) * (-0.012 * i as f64).exp())
                .collect::<Vec<_>>(),
        ];
        let ir2 = vec![
            (0..517)
                .map(|i| lcg(&mut seed) * (-0.006 * i as f64).exp())
                .collect::<Vec<_>>(),
        ];
        let ir3 = vec![
            (0..129)
                .map(|i| lcg(&mut seed) * (-0.02 * i as f64).exp())
                .collect::<Vec<_>>(),
            (0..129)
                .map(|i| lcg(&mut seed) * (-0.015 * i as f64).exp())
                .collect::<Vec<_>>(),
        ];

        let gains = [-2.5, 1.25, -0.75];
        let combined =
            combine_irs(&[(&ir1, gains[0]), (&ir2, gains[1]), (&ir3, gains[2])], 2).unwrap();
        assert_eq!(combined.len(), 2);
        let expected_len = 311 + 517 + 129 - 2;
        assert_eq!(combined[0].len(), expected_len);
        assert_eq!(combined[1].len(), expected_len);

        // Build test signal (padded by latency + IR tails).
        let max_block = 128;
        let frames = 4096;
        let in_l: Vec<f64> = (0..frames)
            .map(|i| if i < 2048 { lcg(&mut seed) } else { 0.0 })
            .collect();
        let in_r: Vec<f64> = (0..frames)
            .map(|i| if i < 2048 { lcg(&mut seed) } else { 0.0 })
            .collect();

        // Path A: single Convolver running the combined IR.
        let mut conv_combined = Convolver::new(&combined, 2, max_block);
        let out_combined = run_chunks(
            &mut conv_combined,
            &[in_l.clone(), in_r.clone()],
            max_block,
            11,
        );

        // Path B: three Convolvers in series, each IR scaled by its own gain_db.
        let scale_ir = |ir: &[Vec<f64>], db: f64| -> Vec<Vec<f64>> {
            let g = crate::db_to_lin(db);
            ir.iter()
                .map(|c| c.iter().map(|&v| v * g).collect())
                .collect()
        };
        let mut c1 = Convolver::new(&scale_ir(&ir1, gains[0]), 2, max_block);
        let mut c2 = Convolver::new(&scale_ir(&ir2, gains[1]), 2, max_block);
        let mut c3 = Convolver::new(&scale_ir(&ir3, gains[2]), 2, max_block);
        let s1 = run_chunks(&mut c1, &[in_l, in_r], max_block, 12);
        let s2 = run_chunks(&mut c2, &s1, max_block, 13);
        let out_serial = run_chunks(&mut c3, &s2, max_block, 14);

        // Serial adds 3 * lat latency vs 1 * lat for combined, so shift by 2 * lat.
        let lat = conv_combined.latency_frames();
        assert_eq!(c1.latency_frames(), lat);
        let extra_lat = 2 * lat;
        for ch in 0..2 {
            let valid_len = frames - extra_lat;
            let peak = out_serial[ch][extra_lat..]
                .iter()
                .fold(0.0f64, |m, v| m.max(v.abs()));
            let max_err = (0..valid_len)
                .map(|i| (out_combined[ch][i] - out_serial[ch][i + extra_lat]).abs())
                .fold(0.0f64, f64::max);
            let rel = max_err / peak.max(1e-12);
            assert!(rel <= 1e-12, "ch {ch} serial vs combined rel err {rel:e}");
        }
    }

    #[test]
    fn combine_irs_keeps_mono_when_all_inputs_mono_and_enforces_cap() {
        let m1 = vec![vec![0.5, 0.25]];
        let m2 = vec![vec![1.0, -0.5, 0.25]];
        let out = combine_irs(&[(&m1, 0.0), (&m2, -6.0)], 2).unwrap();
        assert_eq!(out.len(), 1);
        let g = crate::db_to_lin(-6.0);
        let expected = [0.5 * g, 0.0, 0.0, 0.0625 * g];
        assert_eq!(out[0].len(), expected.len());
        for (a, b) in out[0].iter().zip(&expected) {
            assert!((a - b).abs() < 1e-12, "{a} vs {b}");
        }

        // Single IR > 4M taps or two IRs whose combined length > 4M taps are rejected.
        assert_eq!(
            combined_ir_length(&[MAX_COMBINED_IR_TAPS]).unwrap(),
            MAX_COMBINED_IR_TAPS
        );
        assert_eq!(
            combined_ir_length(&[2_000_001, 2_000_000]).unwrap(),
            MAX_COMBINED_IR_TAPS
        );
        assert!(matches!(
            combined_ir_length(&[2_000_001, 2_000_001]),
            Err(crate::chain::ChainError::IrTooLong {
                taps: 4_000_001,
                max: MAX_COMBINED_IR_TAPS
            })
        ));

        // Channel mismatch (3ch IR on 2ch chain) fails.
        let bad = vec![vec![1.0], vec![1.0], vec![1.0]];
        assert!(matches!(
            combine_irs(&[(&bad, 0.0)], 2),
            Err(crate::chain::ChainError::IrCount {
                irs: 3,
                channels: 2
            })
        ));
    }
}
