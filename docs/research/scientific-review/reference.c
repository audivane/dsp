#include <math.h>
#include <stddef.h>
#include <complex.h>
// x87 64-bit mantissa reference. Avoid f64 multiplication and summation;
// the exact argument is the supplied f64 cycles/sample, promoted to long double.
void reference(const double *h, size_t n, double cycles, double *out) {
    const long double tau = 2 * acosl(-1.0L);
    long double tr[256], ti[256], re = 0, im = 0;
    for (size_t m=0; m<256; ++m) {
        long double a = tau * remainderl((long double)cycles * m, 1.0L);
        tr[m] = cosl(a); ti[m] = -sinl(a);
    }
    for (size_t b=0; b<n; b+=256) {
        long double sr=0, si=0;
        size_t end = n-b < 256 ? n-b : 256;
        for (size_t m=0; m<end; ++m) {
            sr += h[b+m]*tr[m]; si += h[b+m]*ti[m];
        }
        long double p = (long double)cycles*b;
        long double low = fmal((long double)cycles,b,-p);
        long double a = tau * (remainderl(p,1.0L)+low);
        long double c=cosl(a), s=sinl(a);
        re += sr*c + si*s; im += si*c - sr*s;
    }
    out[0] = re; out[1] = im;
}

// Newton in u=z^-1 with long-double Horner evaluation. Returns root z,
// normalized polynomial residual, and iteration count.
void root(const double *h, size_t n, double angle, double *out) {
    long double complex u = cexpl(-I*(long double)angle) / (1.0L+1.0L/n);
    int count=0;
    for (; count<80; ++count) {
        long double complex p=h[n-1], dp=0;
        for (size_t j=n-1;j>0;--j) {dp=dp*u+p;p=p*u+h[j-1];}
        if (cabsl(dp)==0) break;
        long double complex step=p/dp;
        u-=step;
        if (cabsl(step)<1e-18L) break;
        if (!isfinite((double)cabsl(u))) break;
    }
    long double complex p=h[n-1];
    for (size_t j=n-1;j>0;--j) p=p*u+h[j-1];
    long double complex z=1.0L/u;
    out[0]=creall(z);out[1]=cimagl(z);out[2]=cabsl(z)-1;out[3]=cabsl(p);out[4]=count;
}
