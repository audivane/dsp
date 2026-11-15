// Latest-value throttle for live edits (EQ dragging at ~25 Hz).
//
// `push(v)` sends immediately when idle, otherwise remembers `v` and sends
// the newest value once the interval has passed. `flush()` sends any pending
// value right now (used on pointer release, so the final value always goes
// out), and `cancel()` drops it.

export interface Throttle<T> {
  push(value: T): void;
  flush(): void;
  cancel(): void;
  readonly pending: boolean;
}

export interface Clock {
  now(): number;
  setTimeout(fn: () => void, ms: number): unknown;
  clearTimeout(handle: unknown): void;
}

const realClock: Clock = {
  now: () => Date.now(),
  setTimeout: (fn, ms) => setTimeout(fn, ms),
  clearTimeout: (h) => clearTimeout(h as ReturnType<typeof setTimeout>),
};

export function throttleLatest<T>(send: (value: T) => void, intervalMs: number, clock: Clock = realClock): Throttle<T> {
  let last = -Infinity;
  let timer: unknown = null;
  let has = false;
  let value: T | undefined;

  const fire = () => {
    timer = null;
    if (!has) return;
    has = false;
    last = clock.now();
    const v = value as T;
    value = undefined;
    send(v);
  };

  return {
    push(v: T) {
      value = v;
      has = true;
      const wait = last + intervalMs - clock.now();
      if (wait <= 0 && timer === null) fire();
      else if (timer === null) timer = clock.setTimeout(fire, Math.max(0, wait));
    },
    flush() {
      if (timer !== null) {
        clock.clearTimeout(timer);
        timer = null;
      }
      fire();
    },
    cancel() {
      if (timer !== null) clock.clearTimeout(timer);
      timer = null;
      has = false;
      value = undefined;
    },
    get pending() {
      return has;
    },
  };
}
