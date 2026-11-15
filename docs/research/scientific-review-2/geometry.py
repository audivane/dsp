"""Independent short-kernel root search; production case mode validates candidates."""
import numpy as np

best = []
rng = np.random.default_rng(98324)
for i in range(20000):
    n = int(rng.integers(3, 64))
    r = int(rng.choice([2, 3, 4, 8, 16, 32]))
    b = float(rng.uniform(2, 20))
    cut = float(rng.uniform(0.001, 1))
    x = np.arange(n) - (n - 1) / 2
    h = np.sinc(cut * x / r) * np.kaiser(n, b)
    z = np.roots(h)
    rad = np.log(np.abs(z)) * n
    alpha_n = min(2, 0.05 * n)
    # The pre-reflection handles only negative real roots. Look for a complex
    # root that lies inside the contour but outside the allowed final radius.
    bad = (rad > 0.25) & (rad < alpha_n) & (np.abs(z.imag) > 1e-4)
    if bad.any():
        best.append((float(rad[bad].max()), n, r, b, cut, z[bad].tolist()))
best.sort(reverse=True)
print("Candidates for non-reflected complex exterior zeros:", len(best))
for case in best[:30]:
    print(case)
