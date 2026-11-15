import numpy as np
def design(N, r, beta, cut):
    n = np.arange(N) - (N-1)/2
    h = cut*np.sinc(cut*n/r)*np.kaiser(N, beta)
    return h*r/h.sum()
for N,r,b in [(63,2,8),(64,2,8),(63,4,8),(63,16,8),(63,2,14),(63,2,20),(40,2,8),(100,2,8),(255,2,8)]:
    z=np.roots(design(N,r,b,1.0)); d=(np.abs(z)-1)*N
    out=np.sort(d[d>1e-6]); 
    print(N,r,b,"outside (|z|-1)*N smallest:", np.round(out[:4],3), " on-circle count", (np.abs(d)<1e-6).sum())
