//! The long preset kernels on non-uniform partitions against the uniformly
//! partitioned engine on the same kernel (the one each preset's build
//! handed its runtime, captured in the constructor): the same latency and,
//! over the whole impulse response, at most 1e-13 of the peak apart. These
//! are the designs whose fingerprint `runtime_hash` moved with non-uniform
//! partitions (`taps_hash` and `latency_frames` did not).

use audivane_dsp_core::design::{self, BuildContext};
use audivane_dsp_core::presets::UpsamplerRuntime;
use audivane_dsp_core::resample::Upsampler;
use audivane_dsp_core::test_support;
use audivane_protocol::{RateRatio, RateTarget, UpsamplerConfig};

const MAX_BLOCK: usize = 1024;

/// The response to an impulse at frame 0 over `frames` input frames.
fn impulse(rt: &mut dyn UpsamplerRuntime, frames: usize) -> Vec<f64> {
    let r = rt.ratio().up as usize;
    let mut out = Vec::with_capacity(frames * r);
    let mut x = vec![0.0; MAX_BLOCK];
    let mut y = vec![0.0; MAX_BLOCK * r];
    x[0] = 1.0;
    let mut pos = 0;
    while pos < frames {
        let n = MAX_BLOCK.min(frames - pos);
        rt.process(&[&x[..n]], &mut [&mut y[..n * r]]);
        out.extend_from_slice(&y[..n * r]);
        x[0] = 0.0;
        pos += n;
    }
    out
}

#[test]
fn long_presets_match_the_uniform_engine() {
    for (preset, ratio) in [
        ("flattop_huge", 2u32),
        ("flattop_huge", 16),
        ("flattop_mega", 8),
        ("gaussian_huge", 4),
        ("gaussian_huge_apod", 16),
        ("lanczos32767", 32),
        ("lanczos65535", 2),
    ] {
        let up = UpsamplerConfig {
            target: RateTarget::Ratio(ratio),
            preset: Some(preset.to_string()),
            ..UpsamplerConfig::default()
        };
        let cx = BuildContext {
            source_rate: 44_100,
            ratio: RateRatio::integer(ratio),
            channels: 1,
            max_block: MAX_BLOCK,
            pipelines: design::budget::RATIONAL_SWAP_PIPELINES,
            output: Default::default(),
            workspace: design::budget::design_workspace(),
        };
        let named = design::Named::of(&up).unwrap();
        let (rt, records) = test_support::capture(|| design::upsampler(&named, &cx, 0.0));
        let mut rt = rt.unwrap().unwrap();
        let w = &records.iter().find(|r| r.tag == "upsampler").unwrap().words;
        let kernel: Vec<f64> = w[4..].iter().map(|&b| f64::from_bits(b)).collect();
        assert_eq!(w[3] as usize, kernel.len());
        let mut uni =
            Upsampler::from_kernel_uniform(&kernel, ratio as usize, w[1] as usize, 1, MAX_BLOCK);
        assert_eq!(
            rt.latency_out_frames(),
            UpsamplerRuntime::latency_out_frames(&uni)
        );
        let frames = rt.impulse_span_out_frames().div_ceil(ratio as usize) + 1;
        let a = impulse(rt.as_mut(), frames);
        let b = impulse(&mut uni, frames);
        let peak = b.iter().fold(0.0f64, |m, v| m.max(v.abs()));
        let err = a
            .iter()
            .zip(&b)
            .fold(0.0f64, |m, (p, q)| m.max((p - q).abs()));
        let db = 20.0 * (err / peak).max(1e-30).log10();
        eprintln!(
            "{preset} x{ratio} ({} taps): {db:.1} dB of the peak",
            kernel.len()
        );
        assert!(
            err <= 1e-13 * peak,
            "{preset} x{ratio}: {err:.3e} of {peak:.3e}"
        );
    }
}
