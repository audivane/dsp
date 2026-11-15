"""Root stored production coefficients; refine with 80-bit complex Horner.

numpy roots only supplies initial guesses. The root and residual below use
long-double Newton evaluation of the exact stored f64 polynomial.
"""
from pathlib import Path
import sys
import numpy as np

directory = Path(sys.argv[1]) if len(sys.argv) > 1 else Path(__file__).parent / "fixtures/n15"
for name in ("linear", "minimum"):
    h = np.loadtxt(directory / f"{name}.txt")
    roots = np.roots(h)
    print(name, "N=", len(h), "allowed radius=", np.exp(0.25 / len(h)))
    for initial in roots[np.abs(roots) > np.exp(0.25 / len(h))]:
        u = 1 / np.clongdouble(initial)
        for _ in range(80):
            p = np.clongdouble(h[-1])
            dp = np.clongdouble(0)
            for a in h[-2::-1]:
                dp = dp * u + p
                p = p * u + a
            step = p / dp
            u -= step
            if abs(step) < 1e-18:
                break
        p = np.clongdouble(h[-1])
        for a in h[-2::-1]:
            p = p * u + a
        z = 1 / u
        angle = float(np.angle(z))
        f = abs(angle) / (2 * np.pi)
        # Response at the unit-circle point with the same angle, long double.
        unit = np.exp(-1j * np.longdouble(angle))
        punit = np.clongdouble(h[-1])
        for a in h[-2::-1]:
            punit = punit * unit + a
        print(f"  z={z.real:.17g}{z.imag:+.17g}j |z|={abs(z):.17g} "
              f"N*log|z|={len(h)*np.log(abs(z)):.12g} "
              f"residual/sum|h|={abs(p)/np.sum(abs(h)):.3e} "
              f"f={f:.12g} on-circle-level={20*np.log10(abs(punit)/h.sum()):.6f} dB")
