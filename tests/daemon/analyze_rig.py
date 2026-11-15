#!/usr/bin/env python3
"""Analysis helpers for Audivane daemon engine rig tests."""

import json
import math
import random
import struct
import sys
import wave


def read_s32_wav(path: str):
    with wave.open(path, "rb") as wf:
        ch = wf.getnchannels()
        sw = wf.getsampwidth()
        rate = wf.getframerate()
        nframes = wf.getnframes()
        raw = wf.readframes(nframes)
    if ch != 2 or sw != 4:
        raise RuntimeError(f"Expected stereo S32_LE WAV, got ch={ch} sw={sw}")
    samples = struct.unpack(f"<{nframes * ch}i", raw)
    left = [samples[i] / 2147483648.0 for i in range(0, len(samples), 2)]
    right = [samples[i + 1] / 2147483648.0 for i in range(0, len(samples), 2)]
    return rate, left, right


def load_snapshots(jsonl_path: str):
    snaps = []
    replies = []
    with open(jsonl_path, "r", encoding="utf-8", errors="replace") as f:
        for line in f:
            line = line.strip()
            if not line or not line.startswith("{"):
                continue
            obj = json.loads(line)
            if obj.get("type") == "snapshot":
                snaps.append(obj)
            elif obj.get("type") == "reply":
                replies.append(obj)
    return snaps, replies


def cmd_check_dsp_60s(jsonl_path: str, wav_path: str, pwtop_path: str, expected_1khz_dbfs: float):
    snaps, _ = load_snapshots(jsonl_path)
    playing = [s for s in snaps if s.get("state") == "playing"]
    if not playing:
        raise AssertionError("No playing snapshots observed")

    max_xruns = max(s["xruns"] for s in snaps)
    rates = {s["graph_rate"] for s in playing}
    out_rates = {s["output_rate"] for s in playing}
    ratios = {s["ratio"] for s in playing}
    sinks = {s["active_sink"] for s in playing}
    dsp_actives = {s["dsp_active"] for s in playing}
    avg_load = sum(s["dsp_load"] for s in playing) / len(playing)
    max_load = max(s["dsp_load"] for s in playing)
    avg_lat = sum(s["latency_ms"] for s in playing) / len(playing)
    max_frames = max(s["frames_out"] for s in playing)

    print(
        f"Snapshots: playing={len(playing)} rates={sorted(rates)} out_rates={sorted(out_rates)} "
        f"ratios={sorted(ratios)} xruns={max_xruns} dsp_load(avg={avg_load:.4f}, max={max_load:.4f}) "
        f"latency_ms={avg_lat:.2f} frames_out={max_frames}"
    )

    if max_xruns != 0:
        raise AssertionError(f"Expected 0 xruns, got {max_xruns}")
    if rates != {48000} or out_rates != {192000} or ratios != {4}:
        raise AssertionError(f"Unexpected rates/ratio: graph={rates} out={out_rates} ratio={ratios}")
    if sinks != {"dsp"} or dsp_actives != {True}:
        raise AssertionError(f"Expected active_sink=dsp & dsp_active=true, got {sinks} / {dsp_actives}")

    with open(pwtop_path, "r", encoding="utf-8", errors="replace") as f:
        pwtop_txt = f.read()
    aud_lines = [ln for ln in pwtop_txt.splitlines() if "audivane_dsp" in ln and ln.startswith("R ")]
    if not aud_lines:
        raise AssertionError("pw-top did not show active R audivane_dsp line")
    err_val = int(aud_lines[-1].split()[8])
    if err_val != 0:
        raise AssertionError(f"pw-top ERR={err_val}")

    rate, left, right = read_s32_wav(wav_path)
    if rate != 192000:
        raise AssertionError(f"Expected 192000 Hz capture, got {rate}")

    for label, ch in (("L", left), ("R", right)):
        win = rate // 100  # 10 ms
        blocks = []
        for pos in range(0, len(ch) - win + 1, win):
            seg = ch[pos : pos + win]
            rms = math.sqrt(sum(x * x for x in seg) / win)
            blocks.append((pos, rms))
        first = next((i for i, (_, r) in enumerate(blocks) if r > 0.05), None)
        last = next((i for i in range(len(blocks) - 1, -1, -1) if blocks[i][1] > 0.05), None)
        if first is None or last is None or last <= first + 100:
            raise AssertionError(f"{label}: failed to detect continuous 1 kHz tone")
        start_idx = first + 25
        end_idx = last - 25
        dur = (end_idx - start_idx) * (win / rate)
        if dur < 55.0:
            raise AssertionError(f"{label}: tone duration too short ({dur:.2f}s < 55s)")

        rms_vals = [blocks[i][1] for i in range(start_idx, end_idx)]
        med_rms = sorted(rms_vals)[len(rms_vals) // 2]
        min_rms = min(rms_vals)
        max_rms = max(rms_vals)
        if min_rms < med_rms * 0.95 or max_rms > med_rms * 1.05:
            raise AssertionError(
                f"{label}: dropout/spike in RMS (min={min_rms:.5f}, med={med_rms:.5f}, max={max_rms:.5f})"
            )

        # Full-scale sine RMS in dBFS: 20 * log10(rms_sin / (1/sqrt(2))) = 20 * log10(rms * sqrt(2))
        measured_dbfs = 20.0 * math.log10(med_rms * math.sqrt(2.0))
        level_err = abs(measured_dbfs - expected_1khz_dbfs)
        if level_err > 0.05:
            raise AssertionError(
                f"{label}: 1 kHz level {measured_dbfs:.4f} dBFS deviates from expected {expected_1khz_dbfs:.4f} dBFS by {level_err:.4f} dB (> 0.05 dB)"
            )

        # Check phase coherence and residual distortion every 100 ms across whole active span
        chunk = rate // 10
        s_start = blocks[start_idx][0]
        s_end = blocks[end_idx][0]
        w = 2.0 * math.pi * 1000.0 / rate
        max_res_db = -999.0
        prev_phase = None
        max_jump = 0.0
        for pos in range(s_start, s_end - chunk + 1, chunk):
            seg = ch[pos : pos + chunk]
            cs = sum(x * math.cos(w * k) for k, x in enumerate(seg))
            ss = sum(x * math.sin(w * k) for k, x in enumerate(seg))
            a = 2.0 * cs / chunk
            b = 2.0 * ss / chunk
            res_sq = 0.0
            sig_sq = 0.0
            for k, x in enumerate(seg):
                fit = a * math.cos(w * k) + b * math.sin(w * k)
                d = x - fit
                res_sq += d * d
                sig_sq += fit * fit
            res_db = 10.0 * math.log10((res_sq + 1e-18) / (sig_sq + 1e-18))
            max_res_db = max(max_res_db, res_db)
            phi = (math.atan2(a, b) - w * pos) % (2.0 * math.pi)
            if prev_phase is not None:
                diff = (phi - prev_phase + math.pi) % (2.0 * math.pi) - math.pi
                max_jump = max(max_jump, abs(diff))
            prev_phase = phi

        if max_res_db > -45.0:
            raise AssertionError(f"{label}: high distortion/residual {max_res_db:.2f} dB")
        if max_jump > 0.05:
            raise AssertionError(f"{label}: phase discontinuity {max_jump:.5f} rad")

        print(
            f"{label}: dur={dur:.2f}s level={measured_dbfs:.4f} dBFS (expected {expected_1khz_dbfs:.2f}, err={level_err:.4f} dB) "
            f"res={max_res_db:.1f}dB phase_jump={max_jump:.5f}rad"
        )
    print("PASS: DSP 60s EQ + 4x 65k-tap upsampling verified.")


def cmd_check_ab(wav_path: str):
    rate, left, _right = read_s32_wav(wav_path)
    win = rate // 100  # 10 ms blocks
    rms_db = []
    for pos in range(0, len(left) - win + 1, win):
        seg = left[pos : pos + win]
        rms = math.sqrt(sum(x * x for x in seg) / win)
        db = 20.0 * math.log10(rms * math.sqrt(2.0)) if rms > 1e-7 else -160.0
        rms_db.append(db)

    # Locate region where audio is playing (> -30 dBFS)
    active = [db for db in rms_db if db > -30.0]
    if len(active) < 350:
        raise AssertionError(f"Expected >= 3.5s active audio in A/B capture, got {len(active)*10} ms")

    # Trim edges inside active window
    first = next(i for i, db in enumerate(rms_db) if db > -30.0) + 15
    last = len(rms_db) - 1 - next(i for i, db in enumerate(reversed(rms_db)) if db > -30.0) - 15
    span = rms_db[first:last]

    # Check zero gaps (no block inside active span drops below -20 dBFS when switching -6 dBFS -> -12 dBFS)
    min_active_db = min(span)
    if min_active_db < -16.0:
        raise AssertionError(f"Audio gap detected during A/B switch (min block = {min_active_db:.2f} dBFS)")

    # Side A segment (first 500 ms after trim) vs Side B segment (last 500 ms before trim)
    lvl_a = sum(span[:50]) / 50.0
    lvl_b = sum(span[-50:]) / 50.0
    delta = lvl_b - lvl_a
    if abs(delta - (-6.0)) > 0.15:
        raise AssertionError(f"Expected -6.0 dB offset between A and B, got A={lvl_a:.3f} B={lvl_b:.3f} (delta={delta:.3f} dB)")

    print(
        f"PASS: A/B switch gapless (min_block={min_active_db:.2f} dBFS, A={lvl_a:.3f} dBFS, B={lvl_b:.3f} dBFS, delta={delta:.3f} dB)"
    )


def cmd_check_xrun(jsonl_path: str, wav_path: str):
    snaps, _ = load_snapshots(jsonl_path)
    max_xruns = max((s["xruns"] for s in snaps), default=0)
    if max_xruns < 1:
        raise AssertionError(f"Expected xruns >= 1 after injected stall, got {max_xruns}")
    max_frames = max((s["frames_out"] for s in snaps), default=0)
    if max_frames < 48000 * 3:
        raise AssertionError(f"Playback did not continue after xrun (frames_out={max_frames})")

    rate, left, _right = read_s32_wav(wav_path)
    tail = left[-rate:]  # last 1s of capture
    tail_rms = math.sqrt(sum(x * x for x in tail) / len(tail))
    if tail_rms < 0.25:
        raise AssertionError(f"Recovered tail RMS too low ({tail_rms:.5f}); playback likely stayed stuck after xrun")
    print(f"PASS: XRUN recovery verified (xruns={max_xruns}, frames_out={max_frames}, tail_rms={tail_rms:.5f})")


def write_24bit_wav(path: str, rate: int, seconds: float, seed: int = 0xA0D1_7A4E):
    rng = random.Random(seed)
    nframes = int(round(rate * seconds))
    buf = bytearray(nframes * 2 * 3)
    pos = 0
    for f in range(nframes):
        for ch in range(2):
            if f < 4:
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
        rate = wf.getframerate()
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
    return rate, out


def cmd_verify_bitperfect(orig_wav: str, cap_wav: str):
    orig_rate, orig = read_24bit_wav(orig_wav)
    with wave.open(cap_wav, "rb") as wf:
        cap_rate = wf.getframerate()
        ch = wf.getnchannels()
        sw = wf.getsampwidth()
        nframes = wf.getnframes()
        raw = wf.readframes(nframes)
    if cap_rate != orig_rate or ch != 2 or sw != 4:
        raise AssertionError(
            f"Capture header mismatch: orig_rate={orig_rate} cap=({cap_rate} Hz, ch={ch}, sw={sw})"
        )
    cap_raw = list(struct.unpack(f"<{nframes * ch}i", raw))
    cap_24 = [x >> 8 for x in cap_raw]

    anchor_start_frame = 100
    anchor_len = 64
    anchor = orig[anchor_start_frame * 2 : anchor_start_frame * 2 + anchor_len]
    match_idx = None
    first = anchor[0]
    for idx in range(0, len(cap_24) - anchor_len, 2):
        if cap_24[idx] == first and cap_24[idx : idx + anchor_len] == anchor:
            match_idx = idx
            break
    if match_idx is None:
        non_zero = sum(1 for x in cap_24 if x != 0)
        raise AssertionError(
            f"Could not find bit-exact 64-sample anchor in capture ({len(cap_24)//2} frames, non_zero={non_zero})"
        )

    cap_start = match_idx - anchor_start_frame * 2
    if cap_start < 0:
        orig_off = anchor_start_frame * 2
        cap_off = match_idx
    else:
        orig_off = 0
        cap_off = cap_start

    comparable = min(len(orig) - orig_off, len(cap_24) - cap_off)
    if comparable < orig_rate * 2 * 2:
        raise AssertionError(f"Too few comparable frames: {comparable // 2}")

    mismatches = []
    low_byte_nonzero = 0
    for i in range(comparable):
        o = orig[orig_off + i]
        c_raw = cap_raw[cap_off + i]
        c = c_raw >> 8
        if (c_raw & 0xFF) != 0:
            low_byte_nonzero += 1
        if o != c:
            mismatches.append((i // 2, i % 2, o, c))
            if len(mismatches) >= 10:
                break

    if mismatches:
        raise AssertionError(f"Bit-perfect mismatch: first={mismatches}")

    print(
        f"PASS: {comparable // 2} consecutive stereo frames @ {orig_rate} Hz 100% bit-identical "
        f"in top 24 bits of S32_LE (offset={cap_off // 2} frames, low_byte_nonzero={low_byte_nonzero})"
    )


def cmd_check_tone_level(wav_path: str, expected_rate: int, min_dur_s: float, expected_dbfs: float):
    rate, left, _right = read_s32_wav(wav_path)
    if rate != expected_rate:
        raise AssertionError(f"Expected capture rate {expected_rate}, got {rate}")
    win = max(1, rate // 100)  # 10 ms
    rms_db = []
    for pos in range(0, len(left) - win + 1, win):
        seg = left[pos : pos + win]
        rms = math.sqrt(sum(x * x for x in seg) / win)
        db = 20.0 * math.log10(rms * math.sqrt(2.0)) if rms > 1e-7 else -160.0
        rms_db.append(db)

    active = [i for i, db in enumerate(rms_db) if db > -35.0]
    if not active or len(active) * 0.01 < min_dur_s:
        raise AssertionError(
            f"Expected >= {min_dur_s:.2f}s active audio at {expected_rate} Hz, got {len(active) * 0.01:.2f}s"
        )
    span = rms_db[active[0] + 15 : active[-1] - 15]
    if not span:
        raise AssertionError("Active audio span too short after edge trim")
    min_db = min(span)
    med_db = sorted(span)[len(span) // 2]
    if abs(med_db - expected_dbfs) > 0.35:
        raise AssertionError(
            f"Level {med_db:.3f} dBFS deviates from expected {expected_dbfs:.3f} dBFS"
        )
    if min_db < expected_dbfs - 3.0:
        raise AssertionError(f"Gap detected in tone (min 10ms block={min_db:.2f} dBFS)")
    print(
        f"PASS: {len(active) * 0.01:.2f}s tone @ {rate} Hz, median={med_db:.3f} dBFS (expected {expected_dbfs:.2f}), min_block={min_db:.2f} dBFS"
    )


def cmd_check_swap_steps(jsonl_path: str, wav_path: str, expected_rate: int, expected_levels_csv: str):
    snaps, replies = load_snapshots(jsonl_path)
    max_xruns = max((s["xruns"] for s in snaps), default=0)
    if max_xruns != 0:
        raise AssertionError(f"Expected 0 xruns during swaps, got {max_xruns}")
    apply_replies = [r for r in replies if r.get("cmd") == "apply"]
    if any(not r.get("ok") for r in apply_replies):
        raise AssertionError(f"Apply failed: {apply_replies}")

    expected_levels = [float(x) for x in expected_levels_csv.split(",")]
    rate, left, _right = read_s32_wav(wav_path)
    if rate != expected_rate:
        raise AssertionError(f"Expected capture rate {expected_rate}, got {rate}")

    win = max(1, rate // 100)  # 10 ms windows
    rms_db = []
    for pos in range(0, len(left) - win + 1, win):
        seg = left[pos : pos + win]
        rms = math.sqrt(sum(x * x for x in seg) / win)
        db = 20.0 * math.log10(rms * math.sqrt(2.0)) if rms > 1e-7 else -160.0
        rms_db.append(db)

    first = next((i for i, db in enumerate(rms_db) if db > -30.0), None)
    last = next((len(rms_db) - 1 - i for i, db in enumerate(reversed(rms_db)) if db > -30.0), None)
    if first is None or last is None or last - first < 600:
        raise AssertionError("Insufficient active tone captured for swap-gap analysis")

    span = rms_db[first + 25 : last - 25]
    floor_expected = min(expected_levels) - 2.0
    min_block = min(span)
    if min_block < floor_expected:
        min_idx = span.index(min_block)
        raise AssertionError(
            f"Audio gap during chain swap: 10ms block #{min_idx} ({min_idx*0.01:.2f}s) dropped to {min_block:.2f} dBFS < {floor_expected:.2f} dBFS"
        )

    # Locate chronological plateaus matching each expected level in order.
    cursor = 0
    matched_levels = []
    for target in expected_levels:
        found = None
        for i in range(cursor, len(span) - 25):
            window = span[i : i + 25]  # 250 ms plateau
            avg = sum(window) / len(window)
            spread = max(window) - min(window)
            if abs(avg - target) <= 0.25 and spread <= 0.25:
                found = (i, avg)
                cursor = i + 25
                break
        if found is None:
            raise AssertionError(
                f"Did not observe expected plateau {target:.2f} dBFS after index {cursor} (matched so far: {matched_levels})"
            )
        matched_levels.append(round(found[1], 3))

    print(
        f"PASS: 65536-tap 4x chain swapped {len(expected_levels) - 1}x mid-playback with ZERO gaps "
        f"(min_10ms={min_block:.2f} dBFS >= {floor_expected:.2f}, xruns={max_xruns}, plateaus={matched_levels})"
    )


def main():
    cmd = sys.argv[1]
    if cmd == "gen_bitperfect":
        write_24bit_wav(sys.argv[2], int(sys.argv[3]), float(sys.argv[4]))
    elif cmd == "verify_bitperfect":
        cmd_verify_bitperfect(sys.argv[2], sys.argv[3])
    elif cmd == "check_dsp_60s":
        cmd_check_dsp_60s(sys.argv[2], sys.argv[3], sys.argv[4], float(sys.argv[5]))
    elif cmd == "check_ab":
        cmd_check_ab(sys.argv[2])
    elif cmd == "check_xrun":
        cmd_check_xrun(sys.argv[2], sys.argv[3])
    elif cmd == "check_tone_level":
        cmd_check_tone_level(sys.argv[2], int(sys.argv[3]), float(sys.argv[4]), float(sys.argv[5]))
    elif cmd == "check_swap_steps":
        cmd_check_swap_steps(sys.argv[2], sys.argv[3], int(sys.argv[4]), sys.argv[5])
    else:
        sys.exit(2)


if __name__ == "__main__":
    main()
