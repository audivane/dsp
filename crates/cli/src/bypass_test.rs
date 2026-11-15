//! `avdsp bypass-test` — prove the passthrough path is bit-exact.

use crate::BypassTestArgs;
use crate::wavio;
use anyhow::{Result, bail};
use audivane_dsp_core::bypass::hash_i32;
use audivane_dsp_core::chain::{Chain, ChainSpec};
use audivane_protocol::ChainConfig;

pub fn run(args: BypassTestArgs) -> Result<i32> {
    if args.block == 0 {
        bail!("--block must be at least 1 frame");
    }
    let wav = wavio::read_wav(&args.input)?;
    let Some(input_ints) = wav.ints.as_ref() else {
        bail!(
            "{} is a float WAV; bypass-test needs 16/24-bit integer PCM to prove bit-exactness",
            args.input.display()
        );
    };
    if wav.source_bits > 24 {
        eprintln!(
            "note: {}-bit integers do not survive the f32 bus; passthrough is only exact up to 24 bits",
            wav.source_bits
        );
    }

    let config = ChainConfig::default();
    let mut chain = Chain::new(ChainSpec {
        config,
        sample_rate: wav.rate,
        channels: wav.channels,
        max_block: args.block,
        irs: None,
    });
    if !chain.is_bit_transparent() {
        bail!("the default chain is not using the bypass path");
    }

    let channels = wav.channels;
    let mut output = vec![0i32; wav.samples.len()];
    let mut pos = 0;
    while pos < wav.samples.len() {
        let end = (pos + args.block * channels).min(wav.samples.len());
        chain.process_f32(&wav.samples[pos..end], &mut output[pos..end]);
        pos = end;
    }

    let mut mismatches = 0u64;
    let mut first = None;
    for (i, (&expected, &actual)) in input_ints.iter().zip(&output).enumerate() {
        if expected != actual {
            mismatches += 1;
            if first.is_none() {
                first = Some((i, expected, actual));
            }
        }
    }

    println!("input:  {}", args.input.display());
    println!("samples: {}", output.len());
    println!("input_hash: 0x{:016x}", hash_i32(input_ints));
    println!("output_hash: 0x{:016x}", hash_i32(&output));
    println!("mismatches: {mismatches}");
    if let Some((index, expected, actual)) = first {
        println!(
            "first_mismatch: sample {index} (frame {}, channel {}) expected {expected}, got {actual}",
            index / channels,
            index % channels
        );
    }
    if mismatches == 0 && hash_i32(input_ints) == hash_i32(&output) {
        println!("passthrough: bit-exact");
        Ok(0)
    } else {
        println!("passthrough: NOT bit-exact");
        Ok(1)
    }
}
