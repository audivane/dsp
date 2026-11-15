#!/usr/bin/env python3
"""Generate deterministic 24-bit white noise WAV and verify bit-perfect S32 capture."""

import random
import struct
import sys
import wave


def write_24bit_wav(path: str, rate: int, nframes: int, seed: int = 0xA0D1_7A4E):
    rng = random.Random(seed)
    # Write standard 24-bit signed PCM (3 bytes per sample, little endian).
    buf = bytearray(nframes * 2 * 3)
    pos = 0
    for f in range(nframes):
        for ch in range(2):
            if f < 4:
                # Distinct sync header covering negative/positive extremes
                vals = [-8388608, 8388607, -5123456, 6543210, -1, 1, -1234567, 2345678]
                s24 = vals[(f * 2 + ch) % len(vals)]
            else:
                s24 = rng.randint(-8388608, 8388607)
            b = s24.to_bytes(4, "little", signed=True)
            buf[pos : pos + 3] = b[0:3]
            pos += 3

    with wave.open(path, "wb") as wf:
        wf.setnchannels(2)
        wf.setsampwidth(3)
        wf.setframerate(rate)
        wf.writeframes(bytes(buf))


def read_24bit_wav(path: str):
    with wave.open(path, "rb") as wf:
        ch = wf.getnchannels()
        sw = wf.getsampwidth()
        nframes = wf.getnframes()
        raw = wf.readframes(nframes)
    assert ch == 2 and sw == 3
    out = []
    for i in range(0, len(raw), 3):
        b0, b1, b2 = raw[i], raw[i + 1], raw[i + 2]
        val = b0 | (b1 << 8) | (b2 << 16)
        if val & 0x800000:
            val -= 0x1000000
        out.append(val)
    return out


def read_s32_wav(path: str):
    with wave.open(path, "rb") as wf:
        ch = wf.getnchannels()
        sw = wf.getsampwidth()
        nframes = wf.getnframes()
        raw = wf.readframes(nframes)
    assert ch == 2 and sw == 4
    return list(struct.unpack(f"<{nframes * ch}i", raw))


def verify(orig_wav: str, cap_wav: str):
    orig = read_24bit_wav(orig_wav)  # interleaved L, R 24-bit signed ints
    cap_raw = read_s32_wav(cap_wav)  # interleaved L, R 32-bit signed ints
    cap_24 = [x >> 8 for x in cap_raw]

    # Locate the non-zero anchor sequence from frame 100..132 of orig inside cap_24.
    anchor_start_frame = 100
    anchor_len_samples = 64  # 32 stereo frames
    anchor = orig[anchor_start_frame * 2 : anchor_start_frame * 2 + anchor_len_samples]

    match_sample_idx = None
    first = anchor[0]
    for idx in range(0, len(cap_24) - anchor_len_samples, 2):
        if cap_24[idx] == first and cap_24[idx : idx + anchor_len_samples] == anchor:
            match_sample_idx = idx
            break

    if match_sample_idx is None:
        # Check whether channels got swapped or scaled, for diagnostic clarity
        non_zero = sum(1 for x in cap_24 if x != 0)
        raise AssertionError(
            f"Could not find bit-exact 64-sample anchor in captured audio (captured {len(cap_24)//2} frames, non_zero={non_zero})"
        )

    cap_start_sample = match_sample_idx - anchor_start_frame * 2
    if cap_start_sample < 0:
        # Missing very first frames at start; align from anchor_start_frame onward
        orig_offset = anchor_start_frame * 2
        cap_offset = match_sample_idx
    else:
        orig_offset = 0
        cap_offset = cap_start_sample

    comparable = min(len(orig) - orig_offset, len(cap_24) - cap_offset)
    # Trim trailing zero-padding at the end if capture stopped early or pw-play padded silence
    # Walk backwards from end of orig window
    mismatches = []
    matched_samples = 0
    low_byte_nonzero = 0
    for i in range(comparable):
        o = orig[orig_offset + i]
        c_raw = cap_raw[cap_offset + i]
        c = c_raw >> 8
        if (c_raw & 0xFF) != 0:
            low_byte_nonzero += 1
        if o != c:
            mismatches.append((i // 2, i % 2, o, c))
            if len(mismatches) >= 10:
                break
        else:
            matched_samples += 1

    if comparable < 48000 * 2 * 2:
        raise AssertionError(f"Too few comparable samples: {comparable // 2} frames")

    if mismatches:
        raise AssertionError(
            f"Bit-perfect mismatch after {matched_samples} samples; first mismatches (frame, ch, orig, cap): {mismatches}"
        )

    print(
        f"PASS: {comparable // 2} consecutive stereo frames ({comparable} samples) "
        f"100% bit-identical in top 24 bits of S32_LE (offset={cap_offset // 2} frames, "
        f"low_byte_nonzero={low_byte_nonzero})"
    )


def main():
    cmd = sys.argv[1]
    if cmd == "gen":
        write_24bit_wav(sys.argv[2], 48000, 48000 * 3)
    elif cmd == "verify":
        verify(sys.argv[2], sys.argv[3])
    else:
        sys.exit(2)


if __name__ == "__main__":
    main()
