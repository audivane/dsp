// The FFT polyphase engine's partition schedule and its per-callback cost,
// as the daemon prices them (`dsp-core` `nupc::polyphase_plan` and
// `design::budget::nupc_polyphase_secs`), for the mock engine's admission
// check and design check. The job cost model and the slicing are the
// runtime's; the machine is the mock's (`MOCK_MACHINE`).

const MAX_PART = 32768;
const GROWTH = 4;
const DIRECT_FFT_MAX_BLOCK = 512;
const POLY_MAC_CHUNK = 2048;
const SUBFFT_BATCH = 256;
const POLY_MAX_OUT = 1 << 19;

// Job costs in the runtime's model units (ns per element).
const NS_REAL_FFT = 0.42;
const NS_MAC = 0.75;
const NS_FWD_A = 4.4;
const NS_INV_A = 2.8;
const NS_STEP_B = 0.22;
const NS_SPLIT = 2.9;
const NS_PACK = 3.8;

/** Four-step jobs per kind over the model's share of a real FFT (the daemon's factors). */
const FOUR_STEP: Record<Kind, number> = { fwd: 1, inv: 1, mac: 1, fwdA: 2.6, fwdB: 1.8, invB: 1.3, invA: 1.7 };
const TAIL_PEAK_MARGIN = 1.15;
const TAIL_MAC = 1.25;
const FFT_ENGINE_OVERHEAD = 1.15;

/** The mock machine: spectral bins per second, seconds per real FFT point and `log2` of its size. */
export const MOCK_MACHINE = { spectralRate: 0.5e9, fftUnit: 0.3e-9 };

export interface StagePlan {
  offset: number;
  block: number;
  /** `Infinity` for the last stage. */
  end: number;
}

const log2 = Math.log2;
const realFftCost = (b: number) => NS_REAL_FFT * b * log2(2 * b);
const headCost = (b: number, r: number, parts: number) => (parts === 0 ? 0 : (1 + r) * realFftCost(b) + NS_MAC * (b + 1) * parts * r);

/** The largest stage block at ratio `r`: 32768, lower above x16 (output buffer cap). */
export function polyphaseMaxPart(base: number, r: number): number {
  const cap = 2 ** Math.floor(log2(Math.floor(POLY_MAX_OUT / Math.max(1, r))));
  return Math.max(base, Math.min(MAX_PART, cap));
}

function defaultPlan(base: number, len: number, maxPart: number): StagePlan[] {
  const plan: StagePlan[] = [];
  let block = base;
  let offset = 0;
  for (;;) {
    const next = Math.min(block * GROWTH, maxPart);
    const boundary = 2 * next;
    const last = (block === maxPart && plan.length > 0) || next <= block || boundary >= len;
    const end = last ? Infinity : boundary;
    plan.push({ offset, block, end });
    if (last) return plan;
    offset = end;
    block = next;
  }
}

export function stageParts(st: StagePlan, len: number): number {
  return len <= st.offset ? 0 : Math.ceil((Math.min(len, st.end) - st.offset) / st.block);
}

/** `fwd`, `inv`: direct transforms; the rest four-step jobs. */
type Kind = "fwd" | "mac" | "inv" | "fwdA" | "fwdB" | "invB" | "invA";
interface Job {
  kind: Kind;
  cost: number;
}

/** One period's jobs of a tail stage, in order, with their model costs. */
function stageJobs(block: number, r: number, parts: number): Job[] {
  const jobs: Job[] = [];
  const bins = block + 1;
  const macs = () => {
    for (let s = 0; s < bins; s += POLY_MAC_CHUNK) jobs.push({ kind: "mac", cost: NS_MAC * (Math.min(s + POLY_MAC_CHUNK, bins) - s) * parts });
  };
  if (block <= DIRECT_FFT_MAX_BLOCK) {
    jobs.push({ kind: "fwd", cost: realFftCost(block) });
    for (let br = 0; br < r; br++) {
      macs();
      jobs.push({ kind: "inv", cost: realFftCost(block) });
    }
    return jobs;
  }
  const m = 2 ** Math.floor(log2(block) / 2);
  const l = block / m;
  const colsA = Math.min(l, Math.max(1, Math.floor(SUBFFT_BATCH / m)));
  const rowsB = Math.min(m, Math.max(1, Math.floor(SUBFFT_BATCH / l)));
  const stepB = (rows: number) => NS_STEP_B * rows * l * log2(l);
  for (let n2 = 0; n2 < l; n2 += colsA) jobs.push({ kind: "fwdA", cost: NS_FWD_A * (Math.min(n2 + colsA, l) - n2) * m });
  for (let k1 = 0; k1 <= m / 2; k1++) {
    const rows = k1 === 0 || 2 * k1 === m ? 1 : 2;
    jobs.push({ kind: "fwdB", cost: stepB(rows) + NS_SPLIT * rows * l });
  }
  for (let br = 0; br < r; br++) {
    macs();
    for (let k1 = 0; k1 < m; k1 += rowsB) {
      const rows = Math.min(k1 + rowsB, m) - k1;
      jobs.push({ kind: "invB", cost: stepB(rows) + NS_PACK * rows * l });
    }
    for (let n2 = 0; n2 < l; n2 += colsA) jobs.push({ kind: "invA", cost: NS_INV_A * (Math.min(n2 + colsA, l) - n2) * m });
  }
  return jobs;
}

/** `price(job)` summed per base block of the period, as the runtime slices it. */
function slicedTicks(jobs: Job[], ticks: number, price: (j: Job) => number): number[] {
  const total = jobs.reduce((s, j) => s + j.cost, 0);
  const out = new Array<number>(ticks).fill(0);
  let done = 0;
  let i = 0;
  for (let t = 1; t <= ticks; t++) {
    const target = (total * t) / ticks;
    while (i < jobs.length) {
      const c = jobs[i].cost;
      if (t < ticks && done + 0.5 * c > target) break;
      out[t - 1] += price(jobs[i]);
      done += c;
      i++;
    }
  }
  return out;
}

/** The heaviest sum of `k` consecutive entries of the periodic `ticks`, across the period's end too. */
export function heaviestWindow(ticks: number[], k: number): number {
  const n = ticks.length;
  const whole = Math.floor(k / n) * ticks.reduce((s, v) => s + v, 0);
  const rest = k % n;
  if (rest === 0) return whole;
  let best = 0;
  for (let start = 0; start < n; start++) {
    let sum = 0;
    for (let j = 0; j < rest; j++) sum += ticks[(start + j) % n];
    best = Math.max(best, sum);
  }
  return whole + best;
}

function planCost(plan: StagePlan[], base: number, r: number, len: number): number {
  let sum = headCost(base, r, Math.max(1, stageParts(plan[0], len)));
  for (const st of plan.slice(1)) {
    const parts = stageParts(st, len);
    if (parts > 0) sum += stageJobs(st.block, r, parts).reduce((s, j) => s + j.cost, 0) / (st.block / base);
  }
  return sum;
}

/** The schedule for base block `base`, `r` branches of at most `len` taps (`nupc::polyphase_plan`). */
export function polyphasePlan(base: number, r: number, len: number): StagePlan[] {
  let plan = defaultPlan(base, len, polyphaseMaxPart(base, r)).filter((st) => st.offset === 0 || stageParts(st, len) > 0);
  plan[plan.length - 1] = { ...plan[plan.length - 1], end: Infinity };
  for (;;) {
    const cost = planCost(plan, base, r, len);
    let best: { cost: number; plan: StagePlan[] } | null = null;
    for (let s = 1; s < plan.length; s++) {
      const merged = plan.map((st) => ({ ...st }));
      merged[s - 1].end = merged[s].end;
      merged.splice(s, 1);
      const c = planCost(merged, base, r, len);
      if (!best || c < best.cost) best = { cost: c, plan: merged };
    }
    if (!best || best.cost >= cost) return plan;
    plan = best.plan;
  }
}

/**
 * Seconds per second of audio, per channel, of the engine on the mock machine:
 * `taps` at the output rate, ratio `r`, engine block `b`, chain block
 * `maxBlock`, source rate `fs`. One stage: the uniform engine's bins (the mock
 * prices no transform there); with tail stages, the heaviest callback (stage
 * 0's bins every base block, each tail stage's sliced period with transforms
 * priced per job kind, its heaviest run of callback-long windows anywhere in
 * the period).
 */
export function polyphaseSeconds(taps: number, r: number, b: number, maxBlock: number, fs: number): number {
  const { spectralRate, fftUnit } = MOCK_MACHINE;
  const len = Math.ceil(taps / r);
  const plan = polyphasePlan(b, r, len);
  if (plan.length === 1) {
    const parts = Math.max(1, Math.ceil(len / b));
    return ((fs / b) * parts * r * (b + 1) * FFT_ENGINE_OVERHEAD) / spectralRate;
  }
  const k = Math.max(1, Math.floor(maxBlock / b));
  const head = (Math.max(1, stageParts(plan[0], len)) * r * (b + 1)) / spectralRate;
  let peak = k * head;
  for (const st of plan.slice(1)) {
    const parts = stageParts(st, len);
    if (parts === 0) continue;
    const jobs = stageJobs(st.block, r, parts);
    const ticks = st.block / b;
    const fft = (fftUnit * 2 * st.block * log2(2 * st.block)) / realFftCost(st.block);
    const mac = TAIL_MAC / (NS_MAC * spectralRate);
    const per = slicedTicks(jobs, ticks, (j) => j.cost * (j.kind === "mac" ? mac : fft * FOUR_STEP[j.kind]));
    peak += TAIL_PEAK_MARGIN * heaviestWindow(per, k);
  }
  return ((peak * fs) / (k * b)) * FFT_ENGINE_OVERHEAD;
}
