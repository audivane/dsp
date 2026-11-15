#!/usr/bin/env python3
"""Analyzer for 60-second PipeWire driver-mode spike test."""

import re
import struct
import sys
import wave
import math


def parse_spike_log(path: str):
    line_re = re.compile(
        r"graph_rate=(\d+)\s+quantum=(\d+)\s+rx_frames=(\d+)\s+tx_frames=(\d+)\s+"
        r"alsa_delay=(-?\d+)\s+xruns=(\d+)\s+ring_underruns=(\d+)\s+drift_frames=([+-]?\d+(?:\.\d+)?)"
    )
    rows = []
    final = None
    with open(path, "r", encoding="utf-8", errors="replace") as f:
        for line in f:
            m = line_re.search(line)
            if not m:
                continue
            row = {
                "graph_rate": int(m.group(1)),
                "quantum": int(m.group(2)),
                "rx_frames": int(m.group(3)),
                "tx_frames": int(m.group(4)),
                "alsa_delay": int(m.group(5)),
                "xruns": int(m.group(6)),
                "ring_underruns": int(m.group(7)),
                "drift_frames": float(m.group(8)),
            }
            if "FINAL " in line:
                final = row
            else:
                rows.append(row)
    return rows, final


def read_s32_wav(path: str):
    with wave.open(path, "rb") as wf:
        ch = wf.getnchannels()
        sw = wf.getsampwidth()
        rate = wf.getframerate()
        nframes = wf.getnframes()
        raw = wf.readframes(nframes)
    if sw != 4 or ch != 2:
        raise RuntimeError(f"Expected stereo S32_LE WAV, got ch={ch} sw={sw}")
    total_samples = nframes * ch
    samples = struct.unpack(f"<{total_samples}i", raw)
    left = [samples[i] / 2147483648.0 for i in range(0, total_samples, 2)]
    right = [samples[i + 1] / 2147483648.0 for i in range(0, total_samples, 2)]
    return rate, left, right


def analyze_tone(rate: int, ch_samples: list[float], label: str):
    # Find active region where local 10ms RMS > 0.05
    win = rate // 100  # 10 ms = 1920 frames @ 192kHz
    n = len(ch_samples)
    active_blocks = []
    for start in range(0, n - win + 1, win):
        blk = ch_samples[start : start + win]
        rms = math.sqrt(sum(x * x for x in blk) / win)
        active_blocks.append((start, rms))

    first_active = next((i for i, (_, r) in enumerate(active_blocks) if r > 0.05), None)
    last_active = next((i for i in range(len(active_blocks) - 1, -1, -1) if active_blocks[i][1] > 0.05), None)
    if first_active is None or last_active is None or last_active <= first_active + 100:
        raise AssertionError(f"{label}: failed to detect continuous active sine wave")

    # Skip first and last 200ms fade/edge blocks inside active span
    trim = 20
    start_idx = first_active + trim
    end_idx = last_active - trim
    if end_idx <= start_idx:
        raise AssertionError(f"{label}: active span too short ({last_active - first_active} blocks)")

    active_sec = (end_idx - start_idx) * (win / rate)
    if active_sec < 55.0:
        raise AssertionError(f"{label}: active sine duration too short: {active_sec:.2f}s (< 55s)")

    rms_vals = [active_blocks[i][1] for i in range(start_idx, end_idx)]
    min_rms = min(rms_vals)
    max_rms = max(rms_vals)
    med_rms = sorted(rms_vals)[len(rms_vals) // 2]
    if min_rms < med_rms * 0.90 or max_rms > med_rms * 1.10:
        raise AssertionError(
            f"{label}: RMS dropout/spike detected (min={min_rms:.5f}, med={med_rms:.5f}, max={max_rms:.5f})"
        )

    # Check 1000 Hz phase coherence & THD+N across every 100ms chunk
    chunk = rate // 10  # 19200 frames = 100ms
    s_start = active_blocks[start_idx][0]
    s_end = active_blocks[end_idx][0]
    w = 2.0 * math.pi * 1000.0 / rate
    max_residual_db = -999.0
    prev_phase = None
    max_phase_jump = 0.0

    for pos in range(s_start, s_end - chunk + 1, chunk):
        seg = ch_samples[pos : pos + chunk]
        # Project onto cos(w*k), sin(w*k) where k = 0..chunk-1
        c_sum = 0.0
        s_sum = 0.0
        for k, x in enumerate(seg):
            ang = w * k
            c_sum += x * math.cos(ang)
            s_sum += x * math.sin(ang)
        a = 2.0 * c_sum / chunk
        b = 2.0 * s_sum / chunk
        amp = math.hypot(a, b)
        # Residual power against pure 1000 Hz tone
        res_sq = 0.0
        sig_sq = 0.0
        for k, x in enumerate(seg):
            ang = w * k
            fit = a * math.cos(ang) + b * math.sin(ang)
            d = x - fit
            res_sq += d * d
            sig_sq += fit * fit
        res_db = 10.0 * math.log10((res_sq + 1e-18) / (sig_sq + 1e-18))
        if res_db > max_residual_db:
            max_residual_db = res_db

        # Phase at pos relative to global k = pos
        # fit = amp * sin(w*k + phi) = amp*(sin(w*k)*cos(phi) + cos(w*k)*sin(phi)) -> phi = atan2(a, b)
        # Subtract global phase w*pos to check cycle-to-cycle continuity
        phi_local = math.atan2(a, b)
        phi_global = (phi_local - w * pos) % (2.0 * math.pi)
        if prev_phase is not None:
            diff = (phi_global - prev_phase + math.pi) % (2.0 * math.pi) - math.pi
            max_phase_jump = max(max_phase_jump, abs(diff))
        prev_phase = phi_global

    if max_residual_db > -45.0:
        raise AssertionError(f"{label}: high 1kHz sine residual/distortion {max_residual_db:.2f} dB (> -45 dB)")
    if max_phase_jump > 0.05:
        raise AssertionError(f"{label}: phase discontinuity detected ({max_phase_jump:.4f} rad)")

    print(
        f"{label}: duration={active_sec:.2f}s rms_med={med_rms:.5f} (range {min_rms:.5f}..{max_rms:.5f}) "
        f"max_residual={max_residual_db:.1f}dB max_phase_jump={max_phase_jump:.5f}rad"
    )


def main():
    if len(sys.argv) != 4:
        print("Usage: analyze_driver.py <spike.log> <capture.wav> <pwtop.txt>", file=sys.stderr)
        sys.exit(2)

    log_path, wav_path, pwtop_path = sys.argv[1:4]
    rows, final = parse_spike_log(log_path)
    if final is None:
        raise AssertionError("No FINAL telemetry line found in spike.log")

    all_rows = rows + [final]
    max_xruns = max(r["xruns"] for r in all_rows)
    max_ring_underruns = max(r["ring_underruns"] for r in all_rows)
    max_abs_drift = max(abs(r["drift_frames"]) for r in all_rows)
    quantum = final["quantum"]

    print(
        f"Telemetry: rx={final['rx_frames']} tx={final['tx_frames']} quantum={quantum} "
        f"rate={final['graph_rate']} xruns={max_xruns} ring_underruns={max_ring_underruns} "
        f"max_abs_drift={max_abs_drift:.2f} frames"
    )

    if max_xruns != 0:
        raise AssertionError(f"Expected 0 ALSA xruns, got {max_xruns}")
    if max_ring_underruns != 0:
        raise AssertionError(f"Expected 0 ring underruns, got {max_ring_underruns}")
    if final["rx_frames"] < 48000 * 55:
        raise AssertionError(f"Too few frames received from PipeWire: {final['rx_frames']}")
    if max_abs_drift >= 2 * max(quantum, 256):
        raise AssertionError(f"Drift {max_abs_drift:.2f} >= 2 periods ({2 * max(quantum, 256)})")

    with open(pwtop_path, "r", encoding="utf-8", errors="replace") as f:
        pwtop_txt = f.read()
    aud_lines = [ln for ln in pwtop_txt.splitlines() if "audivane_dsp" in ln and ln.startswith("R ")]
    if not aud_lines:
        raise AssertionError("pw-top did not show audivane_dsp in active R (driver) state")
    last_aud = aud_lines[-1].split()
    # Format: S ID QUANT RATE WAIT BUSY W/Q B/Q ERR ...
    err_val = int(last_aud[8])
    quant_val = int(last_aud[2])
    rate_val = int(last_aud[3])
    print(f"pw-top audivane_dsp: quant={quant_val} rate={rate_val} ERR={err_val}")
    if rate_val != 48000:
        raise AssertionError(f"pw-top rate expected 48000, got {rate_val}")
    if err_val != 0:
        raise AssertionError(f"pw-top ERR expected 0, got {err_val}")

    rate, left, right = read_s32_wav(wav_path)
    if rate != 192000:
        raise AssertionError(f"Expected 192000 Hz capture, got {rate}")
    analyze_tone(rate, left, "L")
    analyze_tone(rate, right, "R")
    print("PASS: 60s 48k->192k driver test verified cleanly!")


if __name__ == "__main__":
    main()
