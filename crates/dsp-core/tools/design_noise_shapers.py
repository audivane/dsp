#!/usr/bin/env python3
"""Derive the error-feedback noise-shaping filters used by dither.rs.

An error-feedback quantizer y = Q(x - H(e) + d), e = y - v has the noise
transfer function NTF(z) = 1 - H(z) = 1 - sum_k h[k] z^-(k+1). For a
weighting W(f), the monic order-p FIR NTF minimising the weighted noise
power  integral W(f) |NTF(f)|^2 df  is the order-p linear predictor of a
process whose power spectrum is W (Levinson-Durbin on the autocorrelation
of W); it is minimum phase by construction.

44.1 kHz uses Wannamaker's published 9-tap F-weighted filter verbatim
(R. A. Wannamaker, "Psychoacoustically Optimal Noise Shaping", JAES 40(7/8),
1992). Its implied weighting W44(f) = 1 / |NTF44(f)|^2 is exactly what an
order-9 predictor reproduces (this script checks that), so the other rates
re-derive a 9-tap filter at their own rate from that same perceptual target:

- 48 kHz: W44 up to 22.05 kHz, held flat above (inaudible band).
- 88.2 kHz and up: W44 up to 20 kHz, held flat at W44(20 kHz) above. The
  order-9 fit then moves the noise into the ultrasonic band but only
  gently: total (unweighted) noise power rises by +6..+11 dB over plain
  TPDF, instead of the +18 dB of the audio-band shapers.

Prints the coefficient tables in Rust syntax. Needs numpy only.
"""
import numpy as np

W44 = [2.412, -3.370, 3.937, -4.174, 3.353, -2.205, 1.281, -0.569, 0.0847]
ORDER = 9
GRID = 1 << 15


def ntf(h, f, fs):
    z = np.exp(-2j * np.pi * f / fs)
    return 1 - sum(hk * z ** (k + 1) for k, hk in enumerate(h))


def levinson(r, p):
    a = np.zeros(p + 1)
    a[0] = 1.0
    err = r[0]
    for i in range(1, p + 1):
        k = -(r[i] + np.dot(a[1:i], r[i - 1:0:-1])) / err
        nxt = a.copy()
        for j in range(1, i):
            nxt[j] = a[j] + k * a[i - j]
        nxt[i] = k
        a = nxt
        err *= 1 - k * k
    return -a[1:]


def design(weight, fs):
    f = (np.arange(GRID) + 0.5) * fs / 2 / GRID
    w = weight(f)
    r = np.array([np.mean(w * np.cos(2 * np.pi * f * k / fs)) for k in range(ORDER + 1)])
    return levinson(r, ORDER)


def implied(f):
    return 1 / np.abs(ntf(W44, np.minimum(f, 22050.0), 44100)) ** 2


def total_gain_db(h, fs):
    f = np.linspace(1, fs / 2, 40000)
    return 10 * np.log10(np.mean(np.abs(ntf(h, f, fs)) ** 2))


def ultrasonic(fs):
    return design(lambda f: implied(np.minimum(f, 20000.0)), fs)


def main():
    check = design(implied, 44100)
    assert np.allclose(check, W44, atol=1e-6), check
    tables = [(44100, np.array(W44), "Wannamaker 1992, 9-tap F-weighted (published)")]
    tables.append((48000, design(implied, 48000), "W44 target re-derived at 48 kHz"))
    for fs in (88200, 96000, 176400, 192000):
        tables.append((fs, ultrasonic(fs), "W44 target to 20 kHz, flat above"))
    for fs, h, note in tables:
        print(f"// {fs} Hz: {note}; total noise gain {total_gain_db(h, fs):+.2f} dB")
        print("[" + ", ".join(repr(float(x)) for x in h) + "],")


if __name__ == "__main__":
    main()
