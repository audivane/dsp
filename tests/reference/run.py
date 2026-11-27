#!/usr/bin/env python3
"""Reference checks for Audivane DSP against numpy/scipy.

Runs the real `avdsp` binary (built separately) and compares its output with
independent implementations written here:

  eq_response   EQ magnitude/phase vs scipy sosfreqz of RBJ coefficients
  convolution   FIR convolution vs scipy.signal.fftconvolve
  matrix        FIR matrix (2->1 sum, 2x2 with cross terms) vs fftconvolve sums
  upsampling    integer-ratio interpolation vs scipy.signal.upfirdn
  bypass        16/24-bit passthrough is bit-identical on disk
  levelmatch    analytic A-weighted offset of a +6 dB profile is -6 dB

Usage:
    python3 tests/reference/run.py [--avdsp PATH] [--only SUBSTRING] [--keep]

Exit code is non-zero if any check fails.
"""

from __future__ import annotations

import argparse
import json
import math
import os
import re
import shutil
import struct
import subprocess
import sys
import tempfile
import wave
from pathlib import Path

import numpy as np
from scipy.io import wavfile
from scipy.signal import fftconvolve, sosfreqz, upfirdn

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]  # audivane-dsp/


# --------------------------------------------------------------------- setup

class Ctx:
    def __init__(self, avdsp: Path, workdir: Path):
        self.avdsp = avdsp
        self.dir = workdir

    def path(self, name: str) -> Path:
        return self.dir / name

    def run(self, *args: str, check: bool = True) -> subprocess.CompletedProcess:
        proc = subprocess.run(
            [str(self.avdsp), *map(str, args)],
            cwd=self.dir,
            capture_output=True,
            text=True,
        )
        if check and proc.returncode != 0:
            raise AssertionError(
                f"avdsp {' '.join(map(str, args))} exited {proc.returncode}\n"
                f"stdout:\n{proc.stdout}\nstderr:\n{proc.stderr}"
            )
        return proc


# ------------------------------------------------------------------ helpers

def parse_float_field(text: str, key: str) -> float:
    match = re.search(rf"^{re.escape(key)}:\s*(\S+)\s*$", text, re.MULTILINE)
    if not match:
        raise AssertionError(f"missing {key!r} in:\n{text}")
    return float(match.group(1))


def parse_int_field(text: str, key: str) -> int:
    match = re.search(rf"{re.escape(key)}:?\s+(\d+)", text)
    if not match:
        raise AssertionError(f"missing {key!r} in:\n{text}")
    return int(match.group(1))


def wav_data_bytes(path: Path) -> bytes:
    data = path.read_bytes()
    if data[:4] != b"RIFF" or data[8:12] != b"WAVE":
        raise AssertionError(f"{path} is not a RIFF/WAVE file")
    pos = 12
    while pos + 8 <= len(data):
        chunk = data[pos : pos + 4]
        size = int.from_bytes(data[pos + 4 : pos + 8], "little")
        start = pos + 8
        if chunk == b"data":
            return data[start : start + size]
        pos = start + size + (size & 1)
    raise AssertionError(f"{path} has no data chunk")


def read_wav_float(path: Path) -> tuple[int, np.ndarray]:
    """Read a WAV as interleaved float64 at nominal full scale."""
    rate, samples = wavfile.read(path)
    if samples.dtype == np.float32:
        return rate, samples.astype(np.float64)
    if samples.dtype == np.int16:
        return rate, samples.astype(np.float64) / 2.0**15
    if samples.dtype == np.int32:
        # 32-bit integer, or 24-bit left-justified into int32.
        return rate, samples.astype(np.float64) / 2.0**31
    if samples.dtype == np.int64:
        return rate, samples.astype(np.float64) / 2.0**63
    raise AssertionError(f"unexpected WAV dtype {samples.dtype}")


def write_pcm_wav(path: Path, rate: int, channels: int, bits: int, ints: np.ndarray) -> None:
    """Write packed integer PCM (16 or 24 bit) without touching the samples."""
    if bits == 16:
        payload = ints.astype("<i2").tobytes()
    elif bits == 24:
        flat = ints.astype(np.int64).ravel()
        buf = bytearray(flat.size * 3)
        for i, v in enumerate(flat):
            buf[3 * i : 3 * i + 3] = int(v & 0xFFFFFF).to_bytes(3, "little")
        payload = bytes(buf)
    else:
        raise AssertionError("bits must be 16 or 24")
    with wave.open(str(path), "wb") as handle:
        handle.setnchannels(channels)
        handle.setsampwidth(bits // 8)
        handle.setframerate(rate)
        handle.writeframes(payload)


# --------------------------------------------------------------------- RBJ

def rbj_coeffs(kind: str, freq: float, gain_db: float, q: float, rate: float):
    """RBJ cookbook coefficients, independent of the Rust implementation."""
    nyquist = rate / 2.0
    if freq <= 0.0 or freq >= nyquist:
        return np.array([1.0, 0.0, 0.0]), np.array([1.0, 0.0, 0.0])
    w0 = 2.0 * math.pi * freq / rate
    cos_w0 = math.cos(w0)
    sin_w0 = math.sin(w0)
    alpha = sin_w0 / (2.0 * q)
    a = 10.0 ** (gain_db / 40.0)
    sqrt_a = math.sqrt(a)

    if kind == "pk":
        b = [1 + alpha * a, -2 * cos_w0, 1 - alpha * a]
        d = [1 + alpha / a, -2 * cos_w0, 1 - alpha / a]
    elif kind == "ls":
        # protocol: shelf `q` is the cookbook Q (alpha = sin(w0)/(2Q)), not slope S
        b = [
            a * ((a + 1) - (a - 1) * cos_w0 + 2 * sqrt_a * alpha),
            2 * a * ((a - 1) - (a + 1) * cos_w0),
            a * ((a + 1) - (a - 1) * cos_w0 - 2 * sqrt_a * alpha),
        ]
        d = [
            (a + 1) + (a - 1) * cos_w0 + 2 * sqrt_a * alpha,
            -2 * ((a - 1) + (a + 1) * cos_w0),
            (a + 1) + (a - 1) * cos_w0 - 2 * sqrt_a * alpha,
        ]
    elif kind == "hs":
        # protocol: shelf `q` is the cookbook Q (alpha = sin(w0)/(2Q)), not slope S
        b = [
            a * ((a + 1) + (a - 1) * cos_w0 + 2 * sqrt_a * alpha),
            -2 * a * ((a - 1) + (a + 1) * cos_w0),
            a * ((a + 1) + (a - 1) * cos_w0 - 2 * sqrt_a * alpha),
        ]
        d = [
            (a + 1) - (a - 1) * cos_w0 + 2 * sqrt_a * alpha,
            2 * ((a - 1) - (a + 1) * cos_w0),
            (a + 1) - (a - 1) * cos_w0 - 2 * sqrt_a * alpha,
        ]
    elif kind == "lp":
        b = [(1 - cos_w0) / 2, 1 - cos_w0, (1 - cos_w0) / 2]
        d = [1 + alpha, -2 * cos_w0, 1 - alpha]
    elif kind == "hp":
        b = [(1 + cos_w0) / 2, -(1 + cos_w0), (1 + cos_w0) / 2]
        d = [1 + alpha, -2 * cos_w0, 1 - alpha]
    elif kind == "bp":
        b = [alpha, 0.0, -alpha]
        d = [1 + alpha, -2 * cos_w0, 1 - alpha]
    elif kind == "no":
        b = [1.0, -2 * cos_w0, 1.0]
        d = [1 + alpha, -2 * cos_w0, 1 - alpha]
    elif kind == "ap":
        b = [1 - alpha, -2 * cos_w0, 1 + alpha]
        d = [1 + alpha, -2 * cos_w0, 1 - alpha]
    else:
        raise AssertionError(f"unknown filter kind {kind}")
    return np.array(b) / d[0], np.array(d) / d[0]


PROFILE = """\
# reference profile used by the EQ check
Preamp: -3.5 dB
Filter 1: ON PK Fc 105 Hz Gain 3.4 dB Q 0.70
Filter 2: ON LS Fc 80 Hz Gain -2.5 dB Q 0.7071
Filter 3: ON HS Fc 8000 Hz Gain 1.5 dB Q 0.7071
Filter 4: ON LP Fc 18000 Hz Q 0.7071
Filter 5: ON HP Fc 30 Hz Q 0.7071
Filter 6: ON PK Fc 1000 Hz Gain -2.0 dB Q 2.0
Filter 7: ON NO Fc 3000 Hz Q 4.0
Filter 8: ON BP Fc 500 Hz Q 1.0
Filter 9: ON AP Fc 2000 Hz Q 0.5
Filter 10: OFF PK Fc 4000 Hz Gain 12.0 dB Q 1.0
"""


def rbj_response(freqs: np.ndarray, rate: float) -> np.ndarray:
    preamp = 10.0 ** (-3.5 / 20.0)
    bands = [
        ("pk", 105.0, 3.4, 0.70),
        ("ls", 80.0, -2.5, 0.7071),
        ("hs", 8000.0, 1.5, 0.7071),
        ("lp", 18000.0, 0.0, 0.7071),
        ("hp", 30.0, 0.0, 0.7071),
        ("pk", 1000.0, -2.0, 2.0),
        ("no", 3000.0, 0.0, 4.0),
        ("bp", 500.0, 0.0, 1.0),
        ("ap", 2000.0, 0.0, 0.5),
    ]
    sections = np.empty((len(bands), 6))
    for i, (kind, freq, gain, q) in enumerate(bands):
        b, a = rbj_coeffs(kind, freq, gain, q, rate)
        sections[i] = [b[0], b[1], b[2], 1.0, a[1], a[2]]
    w = 2.0 * np.pi * np.asarray(freqs, dtype=float) / rate
    _, h = sosfreqz(sections, worN=w)
    return np.asarray(h) * preamp


# ------------------------------------------------------------------- checks

def check_eq_response(ctx: Ctx) -> None:
    profile = ctx.path("profile.txt")
    profile.write_text(PROFILE)
    errors = []
    for rate in (44_100, 192_000):
        proc = ctx.run(
            "eq", "response", profile, "--rate", rate, "--points", 300
        )
        lines = proc.stdout.strip().splitlines()
        if not lines or not lines[0].startswith("freq_hz"):
            raise AssertionError(f"unexpected response output:\n{proc.stdout}")
        rows = np.array(
            [[float(x) for x in line.split(",")] for line in lines[1:] if line.strip()]
        )
        freqs, mag_db, phase_deg = rows[:, 0], rows[:, 1], rows[:, 2]
        reference = rbj_response(freqs, rate)
        ref_mag = 20.0 * np.log10(np.abs(reference))
        mag_error = np.max(np.abs(mag_db - ref_mag))
        ref_phase = np.degrees(np.angle(reference))
        phase_error = np.max(
            np.abs(np.angle(np.exp(1j * (np.radians(phase_deg - ref_phase)))))
        )
        print(
            f"    {rate} Hz: max |mag error| = {mag_error:.3e} dB, "
            f"max |phase error| = {math.degrees(phase_error):.3e} deg"
        )
        if mag_error > 1e-6:
            errors.append(f"{rate} Hz magnitude error {mag_error:.3e} dB > 1e-6")
        if phase_error > 1e-6:
            errors.append(f"{rate} Hz phase error {phase_error:.3e} rad > 1e-6")
    if errors:
        raise AssertionError("; ".join(errors))


def check_convolution(ctx: Ctx) -> None:
    rng = np.random.default_rng(2024)
    rate = 44_100
    x = (rng.standard_normal(4000) * 0.1).astype(np.float32)
    ir = (rng.standard_normal(257) * np.exp(-np.arange(257) / 80.0) * 0.2).astype(np.float32)
    wavfile.write(ctx.path("noise.wav"), rate, x)
    wavfile.write(ctx.path("ir.wav"), rate, ir)
    proc = ctx.run(
        "process",
        "noise.wav",
        "conv.wav",
        "--conv",
        "ir.wav",
        "--format",
        "f32",
        "--dither",
        "none",
        "--trim-latency",
        "--block",
        "333",
    )
    got_rate, got = read_wav_float(ctx.path("conv.wav"))
    assert got_rate == rate, f"output rate {got_rate}"
    reference = fftconvolve(x.astype(np.float64), ir.astype(np.float64))[: len(x)]
    assert len(got) == len(reference), f"length {len(got)} != {len(reference)}"
    error = np.max(np.abs(got - reference))
    print(f"    max |error| = {error:.3e} over {len(got)} samples (IR 257 taps, block 333)")
    if error > 1e-6:
        raise AssertionError(
            f"convolution error {error:.3e} > 1e-6; reported latency: "
            f"{next((l for l in proc.stdout.splitlines() if l.startswith('latency:')), '?')}"
        )
    # Stereo, one IR per channel.
    rng = np.random.default_rng(99)
    stereo = (rng.standard_normal((1500, 2)) * 0.1).astype(np.float32)
    ir_stereo = (rng.standard_normal((129, 2)) * 0.2).astype(np.float32)
    wavfile.write(ctx.path("noise2.wav"), rate, stereo)
    wavfile.write(ctx.path("ir2.wav"), rate, ir_stereo)
    ctx.run(
        "process",
        "noise2.wav",
        "conv2.wav",
        "--conv",
        "ir2.wav",
        "--format",
        "f32",
        "--dither",
        "none",
        "--trim-latency",
        "--block",
        "127",
    )
    _, got2 = read_wav_float(ctx.path("conv2.wav"))
    for channel in range(2):
        reference = fftconvolve(
            stereo[:, channel].astype(np.float64), ir_stereo[:, channel].astype(np.float64)
        )[: len(stereo)]
        error = np.max(np.abs(got2[:, channel] - reference))
        print(f"    channel {channel}: max |error| = {error:.3e}")
        if error > 1e-6:
            raise AssertionError(f"stereo convolution channel {channel} error {error:.3e} > 1e-6")


def check_matrix(ctx: Ctx) -> None:
    """FIR matrix: out_j = sum_i F_ji * x_i, against numpy sums of fftconvolve."""
    rng = np.random.default_rng(7)
    rate = 48_000
    frames = 6000
    x = (rng.standard_normal((frames, 2)) * 0.1).astype(np.float32)
    wavfile.write(ctx.path("mx_in.wav"), rate, x)

    def ir(taps: int, decay: float) -> np.ndarray:
        return (rng.standard_normal(taps) * np.exp(-np.arange(taps) / decay) * 0.08).astype(
            np.float32
        )

    def conv(sig: np.ndarray, h: np.ndarray) -> np.ndarray:
        return fftconvolve(sig.astype(np.float64), h.astype(np.float64))[:frames]

    def process(out: str, *conv_args: str) -> np.ndarray:
        args = []
        for a in conv_args:
            args += ["--conv", a]
        ctx.run(
            "process", "mx_in.wav", out, *args,
            "--format", "f32", "--dither", "none", "--trim-latency", "--block", "301",
        )
        got_rate, got = read_wav_float(ctx.path(out))
        assert got_rate == rate, f"output rate {got_rate}"
        assert got.shape == x.shape, f"shape {got.shape}"
        return got

    def compare(what: str, got: np.ndarray, want: np.ndarray) -> None:
        error = np.max(np.abs(got - want))
        print(f"    {what}: max |error| = {error:.3e}")
        if error > 1e-6:
            raise AssertionError(f"{what} error {error:.3e} > 1e-6")

    # 2 -> 1: a sub feed from one stereo WAV (channel 0 for L, 1 for R).
    sub = np.stack([ir(400, 120.0), ir(400, 90.0)], axis=1)
    wavfile.write(ctx.path("mx_sub.wav"), rate, sub)
    got = process("mx_sum.wav", "0:0:0=mx_sub.wav", "1:0:1=mx_sub.wav")
    compare("2->1 sum", got[:, 0], conv(x[:, 0], sub[:, 0]) + conv(x[:, 1], sub[:, 1]))
    if np.any(got[:, 1] != 0.0):
        raise AssertionError("the output with no entry is not silent")

    # Full 2x2 with cross terms, then a stereo IR in series.
    f = {k: ir(200 + 60 * n, 70.0) for n, k in enumerate(["00", "01", "10", "11"])}
    for k, h in f.items():
        wavfile.write(ctx.path(f"mx_{k}.wav"), rate, h)
    after = np.stack([ir(110, 30.0), ir(110, 40.0)], axis=1)
    wavfile.write(ctx.path("mx_after.wav"), rate, after)
    # Entry name "ji": from input i to output j.
    entries = [f"{k[1]}:{k[0]}=mx_{k}.wav" for k in f]
    got = process("mx_full.wav", *entries, "mx_after.wav")
    for j in range(2):
        y = conv(x[:, 0], f[f"{j}0"]) + conv(x[:, 1], f[f"{j}1"])
        compare(f"2x2 then stereo IR, output {j}", got[:, j], conv(y, after[:, j]))


def check_upsampling(ctx: Ctx) -> None:
    ratio = 4
    taps = 1024
    ctx.run("gen", "white", "--rate", "44100", "--seconds", "0.05", "--bits", "24", "--seed", "3", "up_in.wav")
    proc = ctx.run(
        "process",
        "up_in.wav",
        "up_out.wav",
        "--upsample",
        str(ratio),
        "--taps",
        str(taps),
        "--format",
        "f32",
        "--dither",
        "none",
        "--trim-latency",
        "--block",
        "301",
    )
    latency = parse_int_field(proc.stdout, "latency")
    kernel_proc = ctx.run(
        "kernel",
        "--rate",
        "44100",
        "--ratio",
        str(ratio),
        "--taps",
        str(taps),
        "--phase",
        "linear",
        "--beta",
        "14",
        "--cutoff",
        "1.0",
        "--out",
        "kernel.f64",
        "--json",
    )
    meta = json.loads(kernel_proc.stdout.strip().splitlines()[-1])
    kernel = np.fromfile(ctx.path("kernel.f64"), dtype="<f8")
    assert len(kernel) == taps, f"kernel length {len(kernel)}"
    # The dumped kernel is used verbatim by both sides, so the comparison does
    # not depend on its normalization; report it as a sanity number.
    print(f"    kernel DC gain = {meta['sum']:.9f} (ratio {ratio})")

    rate, x = read_wav_float(ctx.path("up_in.wav"))
    out_rate, got = read_wav_float(ctx.path("up_out.wav"))
    assert rate == 44_100 and out_rate == rate * ratio, f"rates {rate} -> {out_rate}"
    full = upfirdn(kernel, x, up=ratio)
    # `--trim-latency` removes both the engine's block latency and the
    # kernel's group delay; upfirdn has no block latency, so the trimmed output
    # lines up with full[group_delay:].
    start = (taps - 1) // 2
    reference = full[start : start + len(x) * ratio]
    assert len(got) == len(reference), f"length {len(got)} != {len(reference)}"

    best = None
    for offset in (0, -1, 1):
        index = start + offset
        if index < 0:
            continue
        candidate = full[index : index + len(got)]
        error = np.max(np.abs(got - candidate))
        if best is None or error < best[0]:
            best = (error, offset)
    error, offset = best
    print(
        f"    max |error| = {error:.3e} at reported latency {latency} "
        f"(alignment offset {offset:+d}), kernel taps {taps}"
    )
    if error > 1e-6:
        raise AssertionError(
            f"upsampling error {error:.3e} > 1e-6; reported latency {latency}, "
            f"best alignment offset {offset:+d}"
        )


def check_bypass(ctx: Ctx) -> None:
    rng = np.random.default_rng(1234)
    cases = [
        (16, 3528, "s16"),
        (24, 3528, "s24"),
    ]
    for bits, frames, fmt in cases:
        limit = 2 ** (bits - 1)
        ints = rng.integers(-limit, limit - 1, size=frames, endpoint=False).astype(np.int64)
        source = ctx.path(f"bypass{bits}.wav")
        write_pcm_wav(source, 48_000, 1, bits, ints)
        proc = ctx.run("process", source, f"bypass{bits}_out.wav", "--format", fmt)
        if wav_data_bytes(source) != wav_data_bytes(ctx.path(f"bypass{bits}_out.wav")):
            raise AssertionError(f"{bits}-bit passthrough is not bit-identical")
        check = ctx.run("bypass-test", source)
        if "passthrough: bit-exact" not in check.stdout:
            raise AssertionError(f"bypass-test did not confirm bit-exactness:\n{check.stdout}")
        print(f"    {bits}-bit: {frames} samples bit-identical (hash verified)")

    # A longer, multichannel 16-bit file exercises the block path and odd tails.
    frames = 5000
    channels = 2
    ints = rng.integers(-32768, 32767, size=frames * channels, endpoint=False).astype(np.int64)
    source = ctx.path("bypass_stereo.wav")
    write_pcm_wav(source, 44_100, channels, 16, ints)
    ctx.run("process", source, "bypass_stereo_out.wav", "--block", "257")
    if wav_data_bytes(source) != wav_data_bytes(ctx.path("bypass_stereo_out.wav")):
        raise AssertionError("stereo 16-bit passthrough is not bit-identical")
    print(f"    stereo 16-bit, block 257: {frames} frames bit-identical")


def check_levelmatch(ctx: Ctx) -> None:
    flat = ctx.path("flat.txt")
    flat.write_text("Preamp: 0 dB\n")
    boosted = ctx.path("boost6.txt")
    boosted.write_text("Preamp: 6 dB\n")
    proc = ctx.run("levelmatch", flat, boosted, "--rate", "44100")
    offset = parse_float_field(proc.stdout, "offset_db")
    print(f"    analytic offset for +6 dB profile: {offset:.4f} dB")
    if abs(offset + 6.0) > 0.05:
        raise AssertionError(f"expected -6.0 dB, got {offset:.4f} dB")
    # Symmetry: B flat against A boosted must be the same magnitude, opposite sign.
    proc = ctx.run("levelmatch", boosted, flat, "--rate", "44100")
    reverse = parse_float_field(proc.stdout, "offset_db")
    print(f"    reverse offset: {reverse:.4f} dB")
    if abs(reverse - 6.0) > 0.05:
        raise AssertionError(f"expected +6.0 dB, got {reverse:.4f} dB")


CHECKS = [
    ("eq_response", check_eq_response),
    ("convolution", check_convolution),
    ("matrix", check_matrix),
    ("upsampling", check_upsampling),
    ("bypass", check_bypass),
    ("levelmatch", check_levelmatch),
]


# --------------------------------------------------------------------- main

def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument(
        "--avdsp",
        default=os.environ.get("AVDSP", str(ROOT / "target" / "debug" / "avdsp")),
        help="path to the avdsp binary (default: target/debug/avdsp)",
    )
    parser.add_argument("--only", default=None, help="run only checks whose name contains this")
    parser.add_argument("--keep", action="store_true", help="keep the temporary directory")
    args = parser.parse_args()

    avdsp = Path(args.avdsp).resolve()
    if not avdsp.is_file():
        print(f"error: avdsp not found at {avdsp}\nbuild it first: cargo build -p audivane-dsp-cli")
        return 2

    workdir = Path(tempfile.mkdtemp(prefix="audivane-ref-"))
    ctx = Ctx(avdsp, workdir)
    failures = 0
    try:
        for name, check in CHECKS:
            if args.only and args.only not in name:
                continue
            print(f"{name}:")
            try:
                check(ctx)
            except Exception as exc:  # noqa: BLE001 - report and continue
                failures += 1
                print(f"FAIL {name}: {exc}")
            else:
                print(f"PASS {name}")
    finally:
        if args.keep:
            print(f"temporary files kept in {workdir}")
        else:
            shutil.rmtree(workdir, ignore_errors=True)

    total = len([1 for name, _ in CHECKS if not args.only or args.only in name])
    print(f"\n{total - failures}/{total} checks passed")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
