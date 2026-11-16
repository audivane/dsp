//! Driver-mode spike binary: owns an ALSA `hw:` device and drives a
//! PipeWire `Audio/Sink` (`audivane_dsp`) at the source rate with fixed integer
//! upsampling.

use anyhow::{Context, Result};
use audivane_io_alsa::AlsaConfig;
use audivane_io_pw::{AudioProcessor, DriverConfig, DriverSinkHandle, f32_to_left_justified_i32};
use audivane_protocol::SampleFormat;
use clap::{Parser, ValueEnum};
use std::f64::consts::PI;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_signal(_sig: libc::c_int) {
    STOP_REQUESTED.store(true, Ordering::SeqCst);
}

fn install_signal_handlers() {
    // Safety: installing a plain async-signal-safe atomic store handler for SIGINT/SIGTERM.
    unsafe {
        libc::signal(
            libc::SIGINT,
            handle_signal as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGTERM,
            handle_signal as *const () as libc::sighandler_t,
        );
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum CliFormat {
    S32,
    S24,
    S24_3,
    S16,
}

impl From<CliFormat> for SampleFormat {
    fn from(v: CliFormat) -> Self {
        match v {
            CliFormat::S32 => SampleFormat::S32,
            CliFormat::S24 => SampleFormat::S24,
            CliFormat::S24_3 => SampleFormat::S24_3,
            CliFormat::S16 => SampleFormat::S16,
        }
    }
}

#[derive(Parser, Debug)]
#[command(name = "driver_spike", about = "Audivane PipeWire driver-mode spike")]
struct Args {
    /// ALSA hardware device to open exclusively via ReserveDevice1
    #[arg(long, default_value = "hw:Loopback,0,0")]
    device: String,

    /// PipeWire source / graph sample rate in Hz
    #[arg(long, default_value_t = 48_000)]
    rate: u32,

    /// Integer output upsampling ratio (DAC rate = rate * ratio)
    #[arg(long, default_value_t = 4)]
    ratio: u32,

    /// Preferred ALSA sample format
    #[arg(long, value_enum, default_value_t = CliFormat::S32)]
    format: CliFormat,

    /// ALSA output period size in frames
    #[arg(long, default_value_t = 1024)]
    period: u32,

    /// ALSA ring buffer size in periods
    #[arg(long, default_value_t = 3)]
    buffer_periods: u32,

    /// Duration to run in seconds (0 = run until SIGINT/SIGTERM)
    #[arg(long, default_value_t = 0)]
    seconds: u64,

    /// Allow PipeWire rate switching across common PCM rates and reopen ALSA
    #[arg(long, default_value_t = false)]
    follow_rate: bool,

    /// Disable ReserveDevice1 D-Bus acquisition
    #[arg(long, default_value_t = false)]
    no_reserve: bool,
}

/// Self-contained allocation-free Blackman-windowed sinc polyphase upsampler
/// for the spike (`ratio == 1` is an exact bit-perfect passthrough).
struct SpikeUpsampler {
    channels: usize,
    ratio: usize,
    taps: usize,
    /// Polyphase coefficient table: `coeffs[phase * taps + tap]`.
    coeffs: Vec<f64>,
    /// Delay line per channel (`channels * taps` samples, most recent at index `head`).
    history: Vec<f64>,
    head: usize,
}

impl SpikeUpsampler {
    fn new(channels: usize, ratio: u32) -> Self {
        let r = ratio.max(1) as usize;
        let taps = 16usize;
        let mut coeffs = vec![0.0f64; r * taps];
        let half = (taps as f64) * 0.5;

        for phase in 0..r {
            let frac = (phase as f64) / (r as f64);
            let center = half - frac;
            let mut sum = 0.0;
            for k in 0..taps {
                // `hist[idx]` at iteration `k` holds `x[n - k]`. Advancing `phase`
                // forward in time means decreasing the delay `center = half - frac`.
                let x = (k as f64) - center;
                let sinc = if x.abs() < 1e-12 {
                    1.0
                } else {
                    let px = PI * x;
                    px.sin() / px
                };
                // Symmetric Blackman window centered around `center` over width `taps`.
                let u = (x + half) / (taps as f64);
                let w = if (0.0..=1.0).contains(&u) {
                    0.42 - 0.5 * (2.0 * PI * u).cos() + 0.08 * (4.0 * PI * u).cos()
                } else {
                    0.0
                };
                let c = sinc * w;
                coeffs[phase * taps + k] = c;
                sum += c;
            }
            if sum.abs() > 1e-12 {
                for k in 0..taps {
                    coeffs[phase * taps + k] /= sum;
                }
            }
        }

        Self {
            channels,
            ratio: r,
            taps,
            coeffs,
            history: vec![0.0; channels * taps],
            head: 0,
        }
    }
}

impl AudioProcessor for SpikeUpsampler {
    fn process_f32_to_i32(
        &mut self,
        in_samples: &[f32],
        out_samples: &mut [i32],
        channels: usize,
        ratio: u32,
    ) {
        if ratio <= 1 || self.ratio <= 1 {
            for (dst, &src) in out_samples.iter_mut().zip(in_samples.iter()) {
                *dst = f32_to_left_justified_i32(src);
            }
            return;
        }

        debug_assert_eq!(channels, self.channels);
        let r = self.ratio;
        let taps = self.taps;
        let in_frames = in_samples.len() / channels;

        for f in 0..in_frames {
            // Advance circular delay line and insert new input sample per channel.
            self.head = if self.head + 1 == taps {
                0
            } else {
                self.head + 1
            };
            for ch in 0..channels {
                self.history[ch * taps + self.head] = in_samples[f * channels + ch] as f64;
            }

            // Emit `r` polyphase output frames from the history ring.
            for phase in 0..r {
                let phase_coeffs = &self.coeffs[phase * taps..(phase + 1) * taps];
                let out_base = (f * r + phase) * channels;
                for ch in 0..channels {
                    let hist_ch = &self.history[ch * taps..(ch + 1) * taps];
                    let mut acc = 0.0f64;
                    let mut idx = self.head;
                    for &c in phase_coeffs.iter() {
                        acc += hist_ch[idx] * c;
                        idx = if idx == 0 { taps - 1 } else { idx - 1 };
                    }
                    let scaled = acc * 2_147_483_648.0;
                    out_samples[out_base + ch] =
                        scaled.round().clamp(i32::MIN as f64, i32::MAX as f64) as i32;
                }
            }
        }
    }
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    install_signal_handlers();

    let args = Args::parse();
    let out_rate = args.rate.saturating_mul(args.ratio.max(1));

    let allowed_rates = if args.follow_rate {
        vec![44_100, 48_000, 88_200, 96_000, 176_400, 192_000]
    } else {
        vec![args.rate]
    };

    let cfg = DriverConfig {
        alsa: AlsaConfig {
            device: args.device.clone(),
            rate: out_rate,
            channels: 2,
            period_frames: args.period,
            buffer_periods: args.buffer_periods,
            preferred_format: Some(args.format.into()),
            strict_format: false,
            nonblock: true,
        },
        source_rate: args.rate,
        ratio: args.ratio.max(1),
        node_name: "audivane_dsp".to_string(),
        node_description: "Audivane DSP".to_string(),
        allowed_rates,
        adapt_alsa_rate: args.follow_rate,
        reserve_device: !args.no_reserve,
        ..DriverConfig::default()
    };

    let upsampler = SpikeUpsampler::new(2, cfg.ratio);
    let handle =
        DriverSinkHandle::spawn(cfg, upsampler).context("failed to spawn Audivane driver sink")?;
    let nego = handle.negotiated_alsa();

    println!(
        "Audivane driver spike started: pw_node_id={} device={} format={:?} alsa_rate={} ch={} alsa_period={} alsa_buf={} ratio={}",
        handle.node_id(),
        args.device,
        nego.format,
        nego.rate,
        nego.channels,
        nego.period_frames,
        nego.buffer_frames,
        args.ratio,
    );
    let _ = std::io::stdout().flush();

    let start = Instant::now();
    let mut next_print = start + Duration::from_secs(1);

    while !STOP_REQUESTED.load(Ordering::SeqCst) {
        let now = Instant::now();
        if args.seconds > 0 && now.duration_since(start) >= Duration::from_secs(args.seconds) {
            break;
        }
        if now >= next_print {
            let snap = handle.snapshot();
            println!(
                "t={:.1}s graph_rate={} quantum={} rx_frames={} tx_frames={} alsa_delay={} xruns={} ring_underruns={} drift_frames={:.2} streaming={} driving={}",
                now.duration_since(start).as_secs_f64(),
                snap.graph_rate,
                snap.quantum,
                snap.frames_received,
                snap.frames_written,
                snap.alsa_delay_frames,
                snap.alsa_xruns,
                snap.ring_underruns,
                snap.drift_frames,
                snap.is_streaming,
                snap.is_driving,
            );
            let _ = std::io::stdout().flush();
            next_print += Duration::from_secs(1);
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let final_snap = handle.snapshot();
    println!(
        "FINAL graph_rate={} quantum={} rx_frames={} tx_frames={} alsa_delay={} xruns={} ring_underruns={} drift_frames={:.2}",
        final_snap.graph_rate,
        final_snap.quantum,
        final_snap.frames_received,
        final_snap.frames_written,
        final_snap.alsa_delay_frames,
        final_snap.alsa_xruns,
        final_snap.ring_underruns,
        final_snap.drift_frames,
    );
    let _ = std::io::stdout().flush();

    handle.stop();
    Ok(())
}
