//! Test support for the filter fingerprint tests
//! (`tests/filter_fingerprints.rs`); not part of the engine's interface.
//!
//! While [`capture`] runs a closure, every upsampler runtime constructed on
//! the calling thread records the data it was handed (the kernel and its
//! delay, the cascade stage kernels, the recursive tables, the
//! interpolator phase tables, the block FFT geometry and window). Outside
//! a capture a constructor only checks an empty thread-local, so the
//! engine does no extra work. Constructors run off the real-time thread.

use std::cell::RefCell;

/// One constructor's data: `tag` names the runtime, `words` its inputs
/// (counts as integers, `f64` values by their bits).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub tag: &'static str,
    pub words: Vec<u64>,
}

thread_local! {
    static RECORDS: RefCell<Option<Vec<Record>>> = const { RefCell::new(None) };
}

/// Run `f`, returning its result and what the runtimes it constructed (on
/// this thread) were built from, in construction order.
pub fn capture<T>(f: impl FnOnce() -> T) -> (T, Vec<Record>) {
    /// Puts the enclosing capture back, also when `f` panics.
    struct Restore(Option<Option<Vec<Record>>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            if let Some(outer) = self.0.take() {
                RECORDS.with(|r| r.replace(outer));
            }
        }
    }
    let mut restore = Restore(Some(RECORDS.with(|r| r.replace(Some(Vec::new())))));
    let out = f();
    let outer = restore.0.take().unwrap_or_default();
    let records = RECORDS.with(|r| r.replace(outer)).unwrap_or_default();
    (out, records)
}

/// Record `words()` under `tag` when a capture is running on this thread.
#[inline]
pub(crate) fn record(tag: &'static str, words: impl FnOnce() -> Vec<u64>) {
    RECORDS.with(|r| {
        if let Some(records) = r.borrow_mut().as_mut() {
            records.push(Record {
                tag,
                words: words(),
            });
        }
    });
}

/// `head`, then the bits of every value of `values`.
pub(crate) fn words(head: &[u64], values: &[f64]) -> Vec<u64> {
    head.iter()
        .copied()
        .chain(values.iter().map(|v| v.to_bits()))
        .collect()
}
