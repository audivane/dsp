import numpy as np
def design(N, r, beta, cut):
    n = np.arange(N) - (N-1)/2
    h = cut*np.sinc(cut*n/r)*np.kaiser(N, beta)
    return h*r/h.sum()
def nqs(N,r,beta,k=1.2):
    A=beta/0.1102+8.7
    return max(0.5, 1-k*r*(A-7.95)/(14.36*(N-1)))
N=1024
for r in [2,4,16]:
  for beta in [0,2,5,8,14,20,30,40]:
    row=[]
    for cut in [0.5,0.95,1.0,'ns']:
      c = nqs(N,r,beta) if cut=='ns' else cut
      z=np.roots(design(N,r,beta,c)); rad=np.abs(z)
      d=(rad-1)*N
      out=d[d>0.5]; inn=d[d<-0.5]; on=d[np.abs(d)<=0.5]
      # nearest "on" zeros distance spread, nearest outside
      row.append(f"{cut}:out_min={out.min() if len(out) else float('nan'):.2f} on_spread={np.abs(on).max() if len(on) else 0:.1e} n_on={len(on)}")
    print(r,beta," | ".join(row))
