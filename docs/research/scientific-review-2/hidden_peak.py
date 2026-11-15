"""Explain the production summary's missed peak using its stored kernel."""
from pathlib import Path
import numpy as np
from scipy.optimize import minimize_scalar

h = np.loadtxt(Path(__file__).parent / "fixtures/summary/linear.txt")
rate, gain, start = 144000, 3, 24117.1875
center = (len(h) - 1) / 2
x = np.arange(len(h), dtype=np.longdouble) - center

def level(hz):
    a = np.sum(h.astype(np.longdouble) * np.cos(2*np.arccos(np.longdouble(-1))*np.longdouble(hz)/rate*x))
    return float(20*np.log10(abs(a)/gain))

solution = minimize_scalar(lambda hz: -level(hz), bounds=(24117.1875, 24152.34375),
                           method="bounded", options={"xatol": 1e-8})
print(f"Direct symmetric long-double cosine peak: {solution.x:.12f} Hz, {level(solution.x):.12f} dB")
for nfft in [4096, 16384, 65536]:
    m = np.abs(np.fft.rfft(h, nfft))/gain
    first = int(np.ceil(start*nfft/rate))
    ks = np.arange(first, len(m))
    candidates = ks[(m[ks] >= max(m[first:])*0.5)
                    & ((ks == first) | (m[ks] >= m[ks-1]))
                    & ((ks == len(m)-1) | (m[ks] >= m[np.minimum(ks+1, len(m)-1)]))]
    candidates = sorted(candidates, key=lambda k: m[k], reverse=True)[:24]
    covered = any(max(start, (k-1)*rate/nfft) <= solution.x <= min(rate/2, (k+1)*rate/nfft)
                  for k in candidates)
    print(f"FFT {nfft}: candidate refinement covers true peak? {covered}")
    if nfft == 4096:
        for k in range(684, 691):
            print(f"  k={k} f={k*rate/nfft:.9f} db={20*np.log10(m[k]):.9f} candidate={k in candidates}")
        print("  refinement intervals near start:",
              [(max(start, (k-1)*rate/nfft), (k+1)*rate/nfft) for k in candidates[:3]])
