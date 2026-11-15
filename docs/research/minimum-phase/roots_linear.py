import numpy as np, sys
from scipy.signal import windows
def design(N, r, beta, cut):
    n = np.arange(N) - (N-1)/2
    h = cut*np.sinc(cut*n/r)*np.kaiser(N, beta)
    return h*r/h.sum()
def nqs(N,r,beta,k=1.2):
    A=beta/0.1102+8.7
    return max(0.5, 1-k*r*(A-7.95)/(14.36*(N-1)))
for N in [512, 1024]:
  for beta in [8,14,20]:
    r=4
    cut=nqs(N,r,beta)
    h=design(N,r,beta,cut)
    z=np.roots(h)
    rad=np.abs(z); ang=np.abs(np.angle(z))/(2*np.pi)
    out = rad>1+1e-6
    on = np.abs(rad-1)<1e-6
    o = np.sort((rad[out]-1)*N)
    print(N,beta,"out",out.sum(),"on",on.sum(),"(r-1)N min/median/max", o[:3], np.median(o), o[-3:], "angles out max", ang[out].max()*2*r, "min ang on-circle", ang[on].min()*2*r if on.any() else None)
