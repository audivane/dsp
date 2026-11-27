// The mock engine's `resolve_design`: every catalogue preset as its
// `FilterDesign`, ported from `crates/dsp-core/src/design/catalogue.rs`
// (same values, so "Edit as custom" in `npm run dev` shows what the daemon
// would). Mock only.

import type { Cutoff, FilterDesign, FilterPreset, Length, Phase, Rates, Window } from "./types";

type Sinc = Extract<FilterDesign, { kind: "windowed_sinc" }>;
type Kernel = Sinc | Extract<FilterDesign, { kind: "lanczos" }>;

const FC_XL = 1 - 256.25 / 22050;
const FC_STD = 21025 / 22050;
const FC_APOD = 20512.5 / 22050;
const FC_STEEP = 253 / 256;
const FC_STEEP_SHORT = 125 / 133;
const FC_EARLY = 27 / 32;
const FC_SHORT_LEAKY = 380 / 441;
const FC_NYQUIST = 1;
const CHEB_32 = 1 / 4_294_967_296;
const CHEB_24 = 1 / 16_777_216;
const CHEB_20 = 1 / 1_048_576;
const CHEB_16 = 1 / 65_536;
const MIX = 0.75;

const gauss = (alpha: number): Window => ({ type: "gaussian", alpha, sampling: "integer_centre" });
const kaiser = (beta: number): Window => ({ type: "kaiser", beta, sampling: "periodic" });
const flattop = (set: "hft196d" | "hft223d" | "hft248d"): Window => ({ type: "flattop", set, sampling: "periodic" });
const cheb = (sidelobe_ratio: number): Window => ({ type: "chebyshev", sidelobe_ratio, sampling: "symmetric" });
const nuttall: Window = { type: "nuttall", sampling: "symmetric" };
const hann: Window = { type: "hann", sampling: "periodic" };
const hft196 = flattop("hft196d");
const hft223 = flattop("hft223d");
const hft248 = flattop("hft248d");
const kaiser7pi = kaiser(7 * Math.PI);
const minimum: Phase = { type: "minimum", method: "cepstral" };
const intermediate: Phase = { type: "intermediate", mix: MIX, method: "cepstral" };
const perInput = (taps: number): Length => ({ rule: "per_input", taps });
const ladder: Length = { rule: "output_span", taps: "ladder" };
const fc = (value: number): Cutoff => ({ mode: "fraction", value });

function sinc(window: Window, length: Length, cutoff: Cutoff): Sinc {
  return {
    kind: "windowed_sinc",
    window,
    length,
    decimation: { length: "scale", odd: "down", base_taps: null },
    cutoff,
    phase: { type: "linear" },
    preserve_samples: false,
    rates: { accept: "any", fallback_window: null },
    numerics: { path: "fir", grid_cutoff: "scaled" },
  };
}
const fixed = (w: Window, taps: number, cutoff: number) => sinc(w, perInput(taps), fc(cutoff));
const compact = (w: Window) =>
  sinc(
    w,
    { rule: "per_input", taps: { by_source_rate: [[50000, 167], [100000, 83], [null, 63]] } },
    { mode: "fraction", value: { by_source_rate: [[50000, 0.88], [100000, 0.725], [null, 0.6]] } },
  );
const accept = <K extends Kernel>(d: K, a: Rates["accept"]): K => ({ ...d, rates: { ...(d.rates ?? { fallback_window: null }), accept: a } });
const phase = <K extends Kernel>(d: K, p: Phase, path: Kernel["numerics"]["path"]): K => ({ ...d, phase: p, numerics: { path, grid_cutoff: "scaled" } });
function lanczos(lobes: number): Kernel {
  const { window: _w, length: _l, cutoff: _c, kind: _k, ...rest } = accept(fixed(nuttall, 1, FC_NYQUIST), "upsampling");
  return { ...rest, kind: "lanczos", lobes };
}
const chebShort = (taps: number, cutoff: number, base: number): Sinc => ({
  ...fixed(cheb(CHEB_32), taps, cutoff),
  decimation: { length: "scale", odd: "up", base_taps: base },
  rates: { accept: "any", fallback_window: { when: "not_multiple", window: kaiser7pi } },
});
const chebyshev = chebShort(117, FC_STD, 191);
const chebyshevShort = chebShort(71, FC_APOD, 127);
const chebyshevLong: Sinc = { ...fixed(cheb(CHEB_32), 207, FC_APOD), rates: { accept: "any", fallback_window: { when: "decimating", window: kaiser7pi } } };
const steep = (taps: number, cutoff: number): Sinc => ({ ...fixed(kaiser(8.75 * Math.PI), taps, cutoff), decimation: { length: "scale", odd: "up", base_taps: null } });
const gaussianEarly = fixed(gauss(0.128), 255, FC_EARLY);
const flattopEarly = fixed(hft248, 255, FC_EARLY);
const leaky = accept(fixed(nuttall, 47, FC_SHORT_LEAKY), "upsampling");
const lanczos23 = lanczos(23);

const TABLE: Record<FilterPreset, () => FilterDesign> = {
  gaussian_short: () => fixed(gauss(0.175), 127, FC_STD),
  gaussian_medium: () => fixed(gauss(0.175), 255, FC_APOD),
  gaussian_long: () => fixed(gauss(0.125), 511, FC_APOD),
  gaussian_xlong: () => fixed(gauss(0.125), 4095, FC_XL),
  gaussian_xlong_apod: () => fixed(gauss(0.125), 4095, FC_APOD),
  gaussian_early: () => gaussianEarly,
  gaussian_early_ip: () => phase(gaussianEarly, intermediate, "double_double"),
  gaussian_early_mp: () => phase(gaussianEarly, minimum, "double_double"),
  gaussian_nyquist: () => fixed(gauss(0.125), 451, FC_NYQUIST),
  gaussian_nyquist_short: () => fixed(gauss(0.2), 191, FC_NYQUIST),
  gaussian_compact: () => accept(compact(gauss(0.15)), "upsampling"),
  gaussian_huge: () => accept(sinc(gauss(0.125), ladder, fc(FC_XL)), "integer"),
  gaussian_huge_apod: () => accept(sinc(gauss(0.125), ladder, fc(FC_APOD)), "integer"),
  nuttall: () => accept(fixed(nuttall, 117, FC_STD), "upsampling"),
  flattop: () => fixed(hft248, 511, FC_STD),
  flattop_short: () => fixed(hft196, 127, FC_STD),
  flattop_medium: () => fixed(hft196, 255, FC_APOD),
  flattop_long: () => fixed(hft248, 511, FC_APOD),
  flattop_xlong: () => fixed(hft248, 4095, FC_XL),
  flattop_xlong_apod: () => fixed(hft248, 4095, FC_APOD),
  flattop_xlong_std: () => accept(fixed(hft248, 4095, FC_STD), "integer"),
  flattop_early: () => flattopEarly,
  flattop_early_ip: () => phase(flattopEarly, intermediate, "fir"),
  flattop_early_mp: () => phase(flattopEarly, minimum, "fir"),
  flattop_compact: () => accept(compact(hft223), "upsampling"),
  flattop_huge: () => accept(sinc(hft248, ladder, fc(FC_STD)), "integer"),
  flattop_mega: () => accept(sinc(hft248, { rule: "output_span", taps: 1 << 20 }, fc(FC_STD)), "integer"),
  chebyshev: () => chebyshev,
  chebyshev_mp: () => phase(chebyshev, minimum, "exact_pi_inverse_after"),
  chebyshev_short: () => chebyshevShort,
  chebyshev_short_mp: () => phase(chebyshevShort, minimum, "exact_pi_inverse_after"),
  chebyshev_long: () => chebyshevLong,
  chebyshev_long_ip: () => phase(chebyshevLong, intermediate, "exact_over_ratio_before"),
  chebyshev_long_mp: () => phase(chebyshevLong, minimum, "exact_over_ratio_before"),
  kaiser_steep: () => steep(577, FC_STEEP),
  kaiser_steep_mp: () => phase(steep(577, FC_STEEP), minimum, "exact_pi_inverse_after"),
  kaiser_steep_short: () => steep(255, FC_STEEP_SHORT),
  kaiser_steep_short_mp: () => phase(steep(255, FC_STEEP_SHORT), minimum, "exact_pi_inverse_after"),
  kaiser_compact: () => accept(compact(kaiser(8 * Math.PI)), "upsampling"),
  halfband_kaiser: () => accept(fixed(kaiser7pi, 255, FC_NYQUIST), "upsampling"),
  halfband_large: () => accept(fixed(cheb(CHEB_32), 255, FC_NYQUIST), "upsampling"),
  halfband_medium: () => accept(fixed(cheb(CHEB_24), 127, FC_NYQUIST), "upsampling"),
  halfband_small: () => accept(fixed(cheb(CHEB_20), 63, FC_NYQUIST), "upsampling"),
  halfband_tiny: () => accept(fixed(cheb(CHEB_16), 35, FC_NYQUIST), "upsampling"),
  hann_nyquist: () => accept(fixed(hann, 16383, FC_NYQUIST), "upsampling"),
  lanczos23: () => lanczos23,
  lanczos23_mp: () => phase(lanczos23, minimum, "fir_after"),
  lanczos511: () => lanczos(511),
  lanczos2047: () => lanczos(2047),
  lanczos8191: () => lanczos(8191),
  lanczos32767: () => accept(lanczos(32767), "integer"),
  lanczos65535: () => accept(lanczos(65535), "integer"),
  short_leaky: () => leaky,
  short_leaky_mp: () => phase(leaky, minimum, "exact_pi_inverse_pairwise"),
  // `catalogue::craven`: a Kaiser band design, flat to 20 kHz at every
  // source rate (the band's `passband_end_hz`), 150 dB from the Nyquist,
  // reflected minimum phase.
  craven_apod: () => ({
    ...sinc({ type: "kaiser", beta: null, sampling: "symmetric" }, { rule: "band" }, { mode: "band", passband_end: 20000 / 22050, passband_end_hz: 20000, stopband_start: 1, attenuation_db: 150 }),
    phase: { type: "minimum", method: "reflect" },
  }),
  cascade_fir: () => ({ kind: "cascade", stage: "fir" }),
  cascade_fir_mp: () => ({ kind: "cascade", stage: "fir_minimum" }),
  cascade_fir_asym: () => ({ kind: "cascade", stage: "fir_asymmetric" }),
  cascade_fft: () => ({ kind: "cascade", stage: "frequency_sampled" }),
  cascade_elliptic: () => ({ kind: "cascade", stage: "elliptic" }),
  cascade_inverse_chebyshev: () => ({ kind: "cascade", stage: "inverse_chebyshev" }),
  nos: () => ({ kind: "interpolator", method: "nos" }),
  cosine: () => ({ kind: "interpolator", method: "cosine" }),
  catmull_rom: () => ({ kind: "interpolator", method: "catmull_rom" }),
  table_sinc: () => ({ kind: "interpolator", method: "table_sinc" }),
  block_fft: () => ({ kind: "block_fft", block: { input: 16384 }, overlap: "quarter_average" }),
  block_fft_fast: () => ({ kind: "block_fft", block: { input: 16384 }, overlap: "half" }),
  block_fft_large: () => ({ kind: "block_fft", block: { output: 1 << 20 }, overlap: "quarter_average" }),
};

function recommended(): FilterDesign {
  return {
    kind: "windowed_sinc",
    window: { type: "kaiser", beta: null, sampling: "symmetric" },
    length: { rule: "auto", budget: "match_output" },
    decimation: { length: "keep" },
    // `catalogue::recommended`: 20 kHz at every source rate, 0.908 where it
    // does not fit.
    cutoff: { mode: "band", passband_end: 0.908, passband_end_hz: 20000, stopband_start: 1.0, attenuation_db: 150 },
    phase: { type: "linear" },
    preserve_samples: false,
    rates: { accept: "any", fallback_window: null },
    numerics: { path: "standard", grid_cutoff: "design_ratio" },
  };
}

/** The design `name` expands to, with `based_on` set; null for an unknown name. */
export function catalogueDesign(name: string): FilterDesign | null {
  const make = name === "recommended" ? recommended : (TABLE as Record<string, (() => FilterDesign) | undefined>)[name];
  if (!make) return null;
  return { based_on: name, ...structuredClone(make()) };
}
