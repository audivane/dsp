import numpy as np, sys
for b in [8,14,20]:
    h=np.loadtxt(f"min_1024_{float(b)}.txt") if False else np.loadtxt(f"min_1024_{b}.txt")
    z=np.roots(h); d=(np.abs(z)-1)*1024
    print(b, "max (|z|-1)*N =", d.max(), " count >1e-3:", (d>1e-3).sum(), " near-circle:", (np.abs(d)<1e-3).sum())
