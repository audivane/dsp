// One queue for every UI mutation: jobs run one at a time, in the order they
// were queued, so each job reads the daemon state its predecessors left.
//
// `latest(key, …)` is for live edits (EQ drags, the volume slider): while the
// queue's last job is a not-yet-started job with the same key, a new value is
// merged into it instead of queueing another job. The queue therefore holds
// at most one waiting job per stream of edits, and that job always runs with
// the newest value, so the final value of a drag is never dropped.

export class MutationQueue {
  private tail: Promise<void> = Promise.resolve();
  /** The last queued job, while it has a key and has not started. */
  private open: { key: string; value: unknown; done: Promise<void> } | null = null;
  private waitingByKey = new Map<string, number>();
  private queued = 0;

  /** Queue `job`; resolves (or rejects) with its result once it has run. */
  run<T>(job: () => Promise<T>): Promise<T> {
    this.open = null;
    return this.push(undefined, job);
  }

  /**
   * Queue `job(value)`, or fold `value` into the waiting job with this key
   * when that job is the last one queued. `merge(older, newer)` combines
   * values (default: the newer replaces the older). Resolves once the job
   * that carries `value` has run; never rejects.
   */
  latest<V>(key: string, value: V, job: (v: V) => Promise<void>, merge: (older: V, newer: V) => V = (_, n) => n): Promise<void> {
    if (this.open && this.open.key === key) {
      this.open.value = merge(this.open.value as V, value);
      return this.open.done;
    }
    const slot: { key: string; value: unknown; done: Promise<void> } = { key, value, done: Promise.resolve() };
    slot.done = this.push(key, async () => {
      if (this.open === slot) this.open = null;
      await job(slot.value as V);
    }).catch(() => undefined);
    this.open = slot;
    return slot.done;
  }

  /** Jobs with this key that are queued but have not started. */
  waiting(key: string): number {
    return this.waitingByKey.get(key) ?? 0;
  }

  /** True when nothing is queued or running. */
  get idle(): boolean {
    return this.queued === 0;
  }

  /** Resolves once everything queued so far has run. */
  drained(): Promise<void> {
    return this.tail;
  }

  private push<T>(key: string | undefined, job: () => Promise<T>): Promise<T> {
    this.queued++;
    if (key !== undefined) this.waitingByKey.set(key, this.waiting(key) + 1);
    const p = this.tail.then(() => {
      if (key !== undefined) this.waitingByKey.set(key, this.waiting(key) - 1);
      return job();
    });
    const settled = p.then(
      () => undefined,
      () => undefined,
    );
    this.tail = settled.then(() => {
      this.queued--;
    });
    return p;
  }
}
