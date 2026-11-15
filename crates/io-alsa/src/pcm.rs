//! Safe direct ALSA `hw:` PCM playback wrapper with zero-alloc audio-path writes.

use crate::caps::{describe_pcm, plan_open, probe_pcm, ratio_ladder, to_alsa_format};
use crate::error::{AlsaError, Result};
use alsa::pcm::{Access, HwParams, PCM, State, SwParams, TstampType};
use alsa::poll::Descriptors;
use alsa::{Direction, ValueOr};
use audivane_protocol::SampleFormat;
use audivane_protocol::ipc::DeviceCapabilities;
use std::time::Duration;

/// Preferred format negotiation order: highest bit depth first.
pub const FORMAT_PREFERENCE: [SampleFormat; 4] = [
    SampleFormat::S32,
    SampleFormat::S24,
    SampleFormat::S24_3,
    SampleFormat::S16,
];

/// Configuration for opening an ALSA `hw:` playback PCM.
#[derive(Clone, Debug)]
pub struct AlsaConfig {
    /// ALSA PCM device string (e.g. `"hw:Loopback,0,0"`).
    pub device: String,
    /// Target output sample rate in Hz (e.g. `192_000`).
    pub rate: u32,
    /// Number of interleaved channels (default `2`).
    pub channels: u32,
    /// Desired period size in output frames.
    pub period_frames: u32,
    /// Desired buffer size as a multiple of `period_frames` (e.g. `3` or `4`).
    pub buffer_periods: u32,
    /// Optional preferred sample format. If `None`, negotiates in order
    /// `S32_LE -> S24_LE -> S24_3LE -> S16_LE`.
    pub preferred_format: Option<SampleFormat>,
    /// When `true` and `preferred_format` is `Some(_)`, fail instead of
    /// falling back to other formats.
    pub strict_format: bool,
    /// Open the PCM in non-blocking mode (`true` recommended for poll/event loops).
    pub nonblock: bool,
}

impl Default for AlsaConfig {
    fn default() -> Self {
        Self {
            device: "hw:Loopback,0,0".to_string(),
            rate: 48_000,
            channels: 2,
            period_frames: 1024,
            buffer_periods: 3,
            preferred_format: None,
            strict_format: false,
            nonblock: true,
        }
    }
}

/// Hardware/software parameters actually negotiated with the ALSA driver.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NegotiatedParams {
    pub format: SampleFormat,
    pub rate: u32,
    pub channels: u32,
    pub period_frames: u32,
    pub buffer_frames: u32,
}

/// Maximum consecutive zero-progress poll waits before `write_i32_all*` aborts.
const MAX_ZERO_PROGRESS_WAITS: usize = 10;

/// An opened playback PCM that is not configured yet. Probe it with
/// [`OpenPcm::probe`] (no second handle needed, so this works while the
/// engine holds the device), then turn it into an [`AlsaOutput`].
pub struct OpenPcm {
    pcm: PCM,
    device: String,
}

impl OpenPcm {
    /// Open `device` for playback, retrying briefly on `EBUSY`.
    pub fn open(device: &str, nonblock: bool) -> Result<Self> {
        Ok(Self {
            pcm: open_pcm_with_ebusy_retry(device, nonblock)?,
            device: device.to_string(),
        })
    }

    pub fn device(&self) -> &str {
        &self.device
    }

    /// Supported (rate, format) pairs for `channels` channels, over `rates`.
    /// Leaves the PCM unconfigured.
    pub fn probe(&self, channels: u32, rates: &[u32]) -> Result<DeviceCapabilities> {
        probe_pcm(&self.pcm, &self.device, channels, rates)
    }

    /// Configure at exactly `config.rate`, trying `formats` in order.
    pub fn configure(self, config: &AlsaConfig, formats: &[SampleFormat]) -> Result<AlsaOutput> {
        let candidate_list = if formats.is_empty() {
            FORMAT_PREFERENCE.to_vec()
        } else {
            formats.to_vec()
        };
        let params = negotiate_params(&self.pcm, &self.device, config, &candidate_list)?;
        AlsaOutput::from_pcm(self.pcm, &self.device, params)
    }

    /// Configure at `source_rate * ratio` for the first ratio on
    /// [`ratio_ladder`]`(desired_ratio)` the device takes, returning the ratio
    /// used. With `caps` the ratio and format are planned from the probe
    /// ([`plan_open`]); without, each rate is tried. There is no "nearest
    /// rate" fallback: the device rate is always exactly `source_rate * ratio`,
    /// otherwise the graph would play at the wrong pitch.
    ///
    /// `period_frames` is scaled as `quantum * ratio` whenever `quantum >= 32`.
    pub fn configure_with_ratio_fallback(
        self,
        config: &AlsaConfig,
        source_rate: u32,
        desired_ratio: u32,
        quantum: u32,
        formats: &[SampleFormat],
        caps: Option<&DeviceCapabilities>,
    ) -> Result<(AlsaOutput, u32)> {
        let candidate_list = if formats.is_empty() {
            FORMAT_PREFERENCE.to_vec()
        } else {
            formats.to_vec()
        };
        let cfg_for = |ratio: u32| {
            let mut c = config.clone();
            c.rate = source_rate.saturating_mul(ratio);
            if quantum >= 32 {
                c.period_frames = quantum.saturating_mul(ratio);
            }
            c
        };

        if let Some(caps) = caps.filter(|c| !c.rates.is_empty()) {
            let (ratio, format) = plan_open(caps, source_rate, desired_ratio, &candidate_list)?;
            let try_cfg = cfg_for(ratio);
            // The planned format first, then the other candidates the device
            // takes at this rate (in case the driver disagrees with the probe).
            let mut order = vec![format];
            order.extend(
                candidate_list
                    .iter()
                    .copied()
                    .filter(|&f| f != format && caps.supports(try_cfg.rate, f)),
            );
            if ratio != desired_ratio.max(1) {
                log::info!(
                    "ALSA '{}' has no {} Hz; using x{ratio} ({} Hz) instead of x{desired_ratio}",
                    self.device,
                    source_rate.saturating_mul(desired_ratio.max(1)),
                    try_cfg.rate
                );
            }
            let params = negotiate_params(&self.pcm, &self.device, &try_cfg, &order)?;
            let out = AlsaOutput::from_pcm(self.pcm, &self.device, params)?;
            return Ok((out, ratio));
        }

        let mut last_err = None;
        for ratio in ratio_ladder(desired_ratio) {
            let try_cfg = cfg_for(ratio);
            match negotiate_params(&self.pcm, &self.device, &try_cfg, &candidate_list) {
                Ok(params) => {
                    let out = AlsaOutput::from_pcm(self.pcm, &self.device, params)?;
                    return Ok((out, ratio));
                }
                Err(e) => {
                    log::debug!(
                        "ALSA '{}' rejected {} Hz: {e}; trying a smaller ratio",
                        self.device,
                        try_cfg.rate
                    );
                    last_err = Some(e);
                }
            }
        }
        // The ratio-1 attempt is the last one: its error is the most telling.
        Err(last_err.unwrap_or_else(|| AlsaError::Unsupported {
            device: self.device.clone(),
            rejected: format!("rate {source_rate} Hz"),
            supported: describe_pcm(&self.pcm),
        }))
    }
}

/// Direct ALSA PCM playback handle with pre-allocated packing scratch buffer.
pub struct AlsaOutput {
    pcm: PCM,
    params: NegotiatedParams,
    scratch: Vec<u8>,
    xruns: u64,
    /// Set whenever `recover()` executes (EPIPE/ESTRPIPE) so the driver knows
    /// the PCM dropped back to `State::Prepared` and must prefill + `start()`
    /// (because `start_threshold` is set to `boundary`).
    needs_restart: bool,
}

impl AlsaOutput {
    /// Open and configure an ALSA `hw:` playback device.
    ///
    /// Retries briefly on `EBUSY` in case WirePlumber is still closing its fd
    /// right after `ReserveDevice1.RequestRelease`.
    pub fn open(config: &AlsaConfig) -> Result<Self> {
        let formats = candidate_formats_from_config(config);
        Self::open_with_formats(config, &formats)
    }

    /// Open and configure an ALSA `hw:` playback device trying `formats` in order.
    pub fn open_with_formats(config: &AlsaConfig, formats: &[SampleFormat]) -> Result<Self> {
        OpenPcm::open(&config.device, config.nonblock)?.configure(config, formats)
    }

    /// Open the device and configure it at `source_rate * ratio`, see
    /// [`OpenPcm::configure_with_ratio_fallback`]. Without `caps` the ratio
    /// ladder is tried blind.
    pub fn open_with_ratio_fallback(
        config: &AlsaConfig,
        source_rate: u32,
        desired_ratio: u32,
        quantum: u32,
        formats: &[SampleFormat],
        caps: Option<&DeviceCapabilities>,
    ) -> Result<(Self, u32)> {
        OpenPcm::open(&config.device, config.nonblock)?.configure_with_ratio_fallback(
            config,
            source_rate,
            desired_ratio,
            quantum,
            formats,
            caps,
        )
    }

    fn from_pcm(pcm: PCM, device: &str, params: NegotiatedParams) -> Result<Self> {
        // Pre-allocate packing buffer once so the audio path never allocates.
        let bytes_per_sample = bytes_per_sample(params.format);
        let max_bytes =
            (params.buffer_frames as usize) * (params.channels as usize) * bytes_per_sample;
        let scratch = vec![0u8; max_bytes.max(4096)];

        log::info!(
            "Opened ALSA '{}' format={:?} rate={} ch={} period={} buffer={}",
            device,
            params.format,
            params.rate,
            params.channels,
            params.period_frames,
            params.buffer_frames
        );

        Ok(Self {
            pcm,
            params,
            scratch,
            xruns: 0,
            needs_restart: false,
        })
    }

    /// Negotiated hardware parameters.
    pub fn params(&self) -> NegotiatedParams {
        self.params
    }

    /// Total xrun count recovered on this handle.
    pub fn xruns(&self) -> u64 {
        self.xruns
    }

    /// Returns `true` if `recover()` was invoked since the last `prepare()` / `start()`.
    /// Because `start_threshold` is set to `boundary`, callers must re-prefill
    /// and call `start()` to resume playback.
    pub fn needs_restart(&self) -> bool {
        self.needs_restart
    }

    /// Returns `true` when the underlying PCM is actively `State::Running`.
    pub fn is_running(&self) -> bool {
        self.pcm.state() == State::Running
    }

    /// Current PCM state.
    pub fn state(&self) -> State {
        self.pcm.state()
    }

    /// Prepare the PCM for playback.
    pub fn prepare(&mut self) -> Result<()> {
        self.pcm.prepare()?;
        self.needs_restart = false;
        Ok(())
    }

    /// Start the PCM stream manually (after prefilling).
    /// No-op if the PCM is already in `State::Running`.
    pub fn start(&mut self) -> Result<()> {
        if self.pcm.state() != State::Running {
            self.pcm.start()?;
        }
        self.needs_restart = false;
        Ok(())
    }

    /// Stop and drop queued frames immediately.
    pub fn drop_frames(&mut self) -> Result<()> {
        self.pcm.drop()?;
        self.needs_restart = false;
        Ok(())
    }

    /// Recover from an ALSA error (e.g. `EPIPE` underrun or `ESTRPIPE` suspend).
    /// Increments the xrun counter when `EPIPE` is observed and marks `needs_restart`.
    pub fn recover(&mut self, err: alsa::Error) -> Result<()> {
        if err.errno() == libc::EPIPE {
            self.xruns += 1;
            log::warn!("ALSA xrun #{} on playback", self.xruns);
        }
        self.pcm.try_recover(err, true)?;
        self.needs_restart = true;
        Ok(())
    }

    /// Available frames that can currently be written without blocking.
    pub fn avail(&mut self) -> Result<u32> {
        match self.pcm.avail() {
            Ok(a) => Ok(a.max(0) as u32),
            Err(err) if err.errno() == libc::EPIPE || err.errno() == libc::ESTRPIPE => {
                self.recover(err)?;
                Ok(self.pcm.avail()?.max(0) as u32)
            }
            Err(err) => Err(AlsaError::Alsa(err)),
        }
    }

    /// Current hardware + software delay in output frames.
    pub fn delay(&mut self) -> Result<i64> {
        match self.pcm.delay() {
            Ok(d) => Ok(d),
            Err(err) if err.errno() == libc::EPIPE || err.errno() == libc::ESTRPIPE => {
                self.recover(err)?;
                Ok(self.pcm.delay()?)
            }
            Err(err) => Err(AlsaError::Alsa(err)),
        }
    }

    /// Query both `avail` and `delay` in output frames in a single ALSA query.
    pub fn avail_and_delay(&mut self) -> Result<(u32, i64)> {
        match self.pcm.avail_delay() {
            Ok((a, d)) => Ok((a.max(0) as u32, d)),
            Err(err) if err.errno() == libc::EPIPE || err.errno() == libc::ESTRPIPE => {
                self.recover(err)?;
                let (a, d) = self.pcm.avail_delay()?;
                Ok((a.max(0) as u32, d))
            }
            Err(err) => Err(AlsaError::Alsa(err)),
        }
    }

    /// Read the PCM status monotonic timestamp in nanoseconds (`htstamp`) and
    /// current delay in frames.
    pub fn status_timestamp_ns(&self) -> Result<(u64, i64)> {
        let status = self.pcm.status()?;
        let ts = status.get_htstamp();
        let ns = (ts.tv_sec as u64)
            .saturating_mul(1_000_000_000)
            .saturating_add(ts.tv_nsec as u64);
        Ok((ns, status.get_delay()))
    }

    /// Return poll file descriptors for integrating with custom `poll()` loops.
    pub fn poll_descriptors(&self) -> Result<Vec<libc::pollfd>> {
        Ok(Descriptors::get(&self.pcm)?)
    }

    /// Decode `revents` from `poll()` on this PCM's descriptors.
    pub fn poll_revents(&self, fds: &[libc::pollfd]) -> Result<alsa::poll::Flags> {
        Ok(Descriptors::revents(&self.pcm, fds)?)
    }

    /// Block until at least `avail_min` frames are ready or `timeout_ms` elapses.
    /// Returns `true` if ready, `false` on timeout.
    pub fn wait_for_period(&mut self, timeout_ms: u32) -> Result<bool> {
        match self.pcm.wait(Some(timeout_ms)) {
            Ok(ready) => Ok(ready),
            Err(err) if err.errno() == libc::EPIPE || err.errno() == libc::ESTRPIPE => {
                self.recover(err)?;
                Ok(true)
            }
            Err(err) => Err(AlsaError::Alsa(err)),
        }
    }

    /// Write left-justified `i32` interleaved samples, packing into the
    /// negotiated device format inside the pre-allocated scratch buffer.
    ///
    /// Returns the number of frames written in this call (may be fewer than
    /// requested in non-blocking mode if the ring buffer is full or if an xrun
    /// recovery happened and the PCM must be restarted).
    pub fn write_i32_interleaved(&mut self, samples: &[i32]) -> Result<usize> {
        let ch = self.params.channels as usize;
        if ch == 0 || !samples.len().is_multiple_of(ch) {
            return Err(AlsaError::InvalidSampleLength {
                len: samples.len(),
                channels: self.params.channels,
            });
        }
        let total_frames = samples.len() / ch;
        if total_frames == 0 {
            return Ok(0);
        }

        let bps = bytes_per_sample(self.params.format);
        let frame_bytes = ch * bps;
        let max_chunk_frames = self.scratch.len() / frame_bytes;
        let mut written_frames = 0usize;

        while written_frames < total_frames {
            let chunk_frames = (total_frames - written_frames).min(max_chunk_frames);
            let start_sample = written_frames * ch;
            let end_sample = start_sample + chunk_frames * ch;
            let n_bytes = pack_left_justified_i32(
                self.params.format,
                &samples[start_sample..end_sample],
                &mut self.scratch,
            );

            let res = {
                let io = self.pcm.io_bytes();
                io.writei(&self.scratch[..n_bytes])
            };
            match res {
                Ok(n) => {
                    written_frames += n;
                    if n < chunk_frames {
                        break;
                    }
                }
                Err(err) if err.errno() == libc::EAGAIN => {
                    break;
                }
                Err(err) if err.errno() == libc::EPIPE || err.errno() == libc::ESTRPIPE => {
                    // Recover prepares the PCM (`start_threshold = boundary`), so
                    // stop writing immediately and let the driver prefill + start.
                    self.recover(err)?;
                    break;
                }
                Err(err) => return Err(AlsaError::Alsa(err)),
            }
        }

        Ok(written_frames)
    }

    /// Write all `samples` (left-justified `i32` interleaved) with bounded
    /// zero-progress retries.
    pub fn write_i32_all(&mut self, samples: &[i32]) -> Result<usize> {
        self.write_i32_all_until(samples, || false)
    }

    /// Write all `samples` (left-justified `i32` interleaved), waiting via
    /// `pcm.wait` if non-blocking `writei` encounters `EAGAIN`.
    ///
    /// Bounded against infinite zero-progress spins (`AlsaError::WriteTimeout`),
    /// interruptible via `should_stop` (`AlsaError::Interrupted`), and stops
    /// early if `recover()` sets `needs_restart` so the caller can re-prefill
    /// and restart playback.
    pub fn write_i32_all_until(
        &mut self,
        samples: &[i32],
        should_stop: impl Fn() -> bool,
    ) -> Result<usize> {
        let ch = self.params.channels as usize;
        if ch == 0 || !samples.len().is_multiple_of(ch) {
            return Err(AlsaError::InvalidSampleLength {
                len: samples.len(),
                channels: self.params.channels,
            });
        }
        let total_frames = samples.len() / ch;
        let mut written = 0usize;
        let mut zero_waits = 0usize;
        while written < total_frames {
            if should_stop() {
                return Err(AlsaError::Interrupted);
            }
            let n = self.write_i32_interleaved(&samples[written * ch..])?;
            written += n;
            if self.needs_restart {
                break;
            }
            if written < total_frames {
                if n == 0 {
                    zero_waits += 1;
                    if zero_waits > MAX_ZERO_PROGRESS_WAITS {
                        return Err(AlsaError::WriteTimeout);
                    }
                    let _ = self.wait_for_period(20)?;
                    if self.needs_restart {
                        break;
                    }
                } else {
                    zero_waits = 0;
                }
            }
        }
        Ok(written)
    }

    /// Write `frames` of digital silence (zero bytes) to ALSA using the
    /// pre-allocated scratch buffer.
    pub fn write_silence(&mut self, frames: u32) -> Result<usize> {
        let ch = self.params.channels as usize;
        let bps = bytes_per_sample(self.params.format);
        let frame_bytes = ch * bps;
        let max_chunk_frames = self.scratch.len() / frame_bytes;
        let mut written = 0usize;
        let target = frames as usize;

        while written < target {
            let chunk = (target - written).min(max_chunk_frames);
            let n_bytes = chunk * frame_bytes;
            self.scratch[..n_bytes].fill(0);
            let res = {
                let io = self.pcm.io_bytes();
                io.writei(&self.scratch[..n_bytes])
            };
            match res {
                Ok(n) => {
                    written += n;
                    if n < chunk {
                        break;
                    }
                }
                Err(err) if err.errno() == libc::EAGAIN => break,
                Err(err) if err.errno() == libc::EPIPE || err.errno() == libc::ESTRPIPE => {
                    self.recover(err)?;
                    break;
                }
                Err(err) => return Err(AlsaError::Alsa(err)),
            }
        }
        Ok(written)
    }
}

fn open_pcm_with_ebusy_retry(device: &str, nonblock: bool) -> Result<PCM> {
    let mut last_err = None;
    for attempt in 0..25 {
        match PCM::new(device, Direction::Playback, nonblock) {
            Ok(p) => return Ok(p),
            Err(err) if err.errno() == libc::EBUSY && attempt < 24 => {
                last_err = Some(err);
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(err) => return Err(AlsaError::Alsa(err)),
        }
    }
    Err(AlsaError::Alsa(last_err.unwrap()))
}

fn candidate_formats_from_config(config: &AlsaConfig) -> Vec<SampleFormat> {
    let mut candidate_formats = Vec::with_capacity(4);
    if let Some(pref) = config.preferred_format {
        candidate_formats.push(pref);
        if !config.strict_format {
            for f in FORMAT_PREFERENCE {
                if f != pref {
                    candidate_formats.push(f);
                }
            }
        }
    } else {
        candidate_formats.extend_from_slice(&FORMAT_PREFERENCE);
    }
    candidate_formats
}

fn negotiate_params(
    pcm: &PCM,
    device: &str,
    config: &AlsaConfig,
    candidate_formats: &[SampleFormat],
) -> Result<NegotiatedParams> {
    // Why each candidate failed, so the error names the parameter.
    let mut access_rejected = false;
    let mut format_rejected: Vec<SampleFormat> = Vec::new();
    let mut channels_rejected = false;
    let mut rate_rejected: Vec<SampleFormat> = Vec::new();
    let mut install_err: Option<String> = None;

    let mut chosen_format = None;
    for &fmt in candidate_formats {
        let hwp = HwParams::any(pcm)?;
        let _ = hwp.set_rate_resample(false);
        if hwp.set_access(Access::RWInterleaved).is_err() {
            access_rejected = true;
            break;
        }
        if hwp.set_format(to_alsa_format(fmt)).is_err() {
            format_rejected.push(fmt);
            continue;
        }
        if hwp.set_channels(config.channels).is_err() {
            channels_rejected = true;
            continue;
        }
        if hwp.set_rate(config.rate, ValueOr::Nearest).is_err()
            || hwp.get_rate().ok() != Some(config.rate)
        {
            rate_rejected.push(fmt);
            continue;
        }
        let period = hwp.set_period_size_near(config.period_frames as _, ValueOr::Nearest)?;
        let desired_buf = (period as u32).saturating_mul(config.buffer_periods.max(2));
        let _buffer = hwp.set_buffer_size_near(desired_buf as _)?;
        match pcm.hw_params(&hwp) {
            Ok(()) => {
                chosen_format = Some(fmt);
                break;
            }
            Err(e) => install_err = Some(e.to_string()),
        }
    }

    let Some(format) = chosen_format else {
        let names = |v: &[SampleFormat]| v.iter().map(|f| f.name()).collect::<Vec<_>>().join(", ");
        let rejected = if access_rejected {
            "interleaved read/write access".to_string()
        } else if !rate_rejected.is_empty() {
            format!(
                "rate {} Hz (with {} channels, format {})",
                config.rate,
                config.channels,
                names(&rate_rejected)
            )
        } else if channels_rejected {
            format!("{} channels", config.channels)
        } else if format_rejected.len() == candidate_formats.len() {
            format!("sample format {}", names(&format_rejected))
        } else if let Some(e) = install_err {
            format!(
                "{} Hz, {} channels with the requested buffer ({e})",
                config.rate, config.channels
            )
        } else {
            format!("{} Hz, {} channels", config.rate, config.channels)
        };
        return Err(AlsaError::Unsupported {
            device: device.to_string(),
            rejected,
            supported: describe_pcm(pcm),
        });
    };

    let hwp = pcm.hw_params_current()?;
    let rate = hwp.get_rate()?;
    let channels = hwp.get_channels()?;
    let period_frames = hwp.get_period_size()? as u32;
    let buffer_frames = hwp.get_buffer_size()? as u32;

    let swp = pcm.sw_params_current()?;
    swp.set_avail_min(period_frames as _)?;
    // Disable auto-start during prefill so we start manually after prefilling the buffer.
    let boundary = swp.get_boundary()?;
    swp.set_start_threshold(boundary)?;
    let _ = swp.set_tstamp_mode(true);
    let _ = swp.set_tstamp_type(TstampType::MonotonicRaw);
    pcm.sw_params(&swp)?;

    Ok(NegotiatedParams {
        format,
        rate,
        channels,
        period_frames,
        buffer_frames,
    })
}

/// Bytes per sample on the ALSA wire for a [`SampleFormat`].
pub const fn bytes_per_sample(fmt: SampleFormat) -> usize {
    match fmt {
        SampleFormat::S16 => 2,
        SampleFormat::S24 | SampleFormat::S32 => 4,
        SampleFormat::S24_3 => 3,
    }
}

/// Pack left-justified `i32` samples into `dst` in `fmt` byte layout.
/// Returns the number of bytes written to `dst`.
///
/// `dst` must have length `>= samples.len() * bytes_per_sample(fmt)`.
#[inline]
pub fn pack_left_justified_i32(fmt: SampleFormat, samples: &[i32], dst: &mut [u8]) -> usize {
    match fmt {
        SampleFormat::S32 => {
            let mut off = 0;
            for &s in samples {
                dst[off..off + 4].copy_from_slice(&s.to_le_bytes());
                off += 4;
            }
            off
        }
        SampleFormat::S24 => {
            // ALSA S24_LE: 24-bit signed sample in the low 3 bytes of a 32-bit LE word.
            let mut off = 0;
            for &s in samples {
                dst[off..off + 4].copy_from_slice(&(s >> 8).to_le_bytes());
                off += 4;
            }
            off
        }
        SampleFormat::S24_3 => {
            // ALSA S24_3LE: packed 3-byte little-endian (bits [31:8] of left-justified i32).
            let mut off = 0;
            for &s in samples {
                let b = s.to_le_bytes();
                dst[off] = b[1];
                dst[off + 1] = b[2];
                dst[off + 2] = b[3];
                off += 3;
            }
            off
        }
        SampleFormat::S16 => {
            let mut off = 0;
            for &s in samples {
                let v = (s >> 16) as i16;
                dst[off..off + 2].copy_from_slice(&v.to_le_bytes());
                off += 2;
            }
            off
        }
    }
}

/// Read-only helper that checks `SwParams` is valid on drop-free path.
#[allow(dead_code)]
fn _assert_sw_params_send(_: &SwParams<'_>) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_s32_exact() {
        let samples = [0x1234_5678_i32, -0x0102_0300_i32];
        let mut out = [0u8; 8];
        let n = pack_left_justified_i32(SampleFormat::S32, &samples, &mut out);
        assert_eq!(n, 8);
        assert_eq!(&out[0..4], &0x1234_5678_i32.to_le_bytes());
        assert_eq!(&out[4..8], &(-0x0102_0300_i32).to_le_bytes());
    }

    #[test]
    fn pack_s24_le_shifts_right_8() {
        let samples = [0x7FFF_FF00_i32, i32::MIN, -256_i32];
        let mut out = [0u8; 12];
        let n = pack_left_justified_i32(SampleFormat::S24, &samples, &mut out);
        assert_eq!(n, 12);
        assert_eq!(
            i32::from_le_bytes(out[0..4].try_into().unwrap()),
            0x007F_FFFF
        );
        assert_eq!(
            i32::from_le_bytes(out[4..8].try_into().unwrap()),
            -8_388_608
        );
        assert_eq!(i32::from_le_bytes(out[8..12].try_into().unwrap()), -1);
    }

    #[test]
    fn pack_s24_3le_exact_top_24_bits() {
        let samples = [0x7FFF_FF00_i32, i32::MIN, -256_i32];
        let mut out = [0u8; 9];
        let n = pack_left_justified_i32(SampleFormat::S24_3, &samples, &mut out);
        assert_eq!(n, 9);
        assert_eq!(&out[0..3], &[0xFF, 0xFF, 0x7F]);
        assert_eq!(&out[3..6], &[0x00, 0x00, 0x80]);
        assert_eq!(&out[6..9], &[0xFF, 0xFF, 0xFF]);
    }

    #[test]
    fn pack_s16_top_16_bits() {
        let samples = [0x7FFF_0000_i32, i32::MIN];
        let mut out = [0u8; 4];
        let n = pack_left_justified_i32(SampleFormat::S16, &samples, &mut out);
        assert_eq!(n, 4);
        assert_eq!(i16::from_le_bytes(out[0..2].try_into().unwrap()), i16::MAX);
        assert_eq!(i16::from_le_bytes(out[2..4].try_into().unwrap()), i16::MIN);
    }
}
