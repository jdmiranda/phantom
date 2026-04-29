//! Monotonically-increasing sequence clock for ordering memory blocks and events.
//!
//! [`SequenceClock`] wraps an [`AtomicU64`] and provides a simple
//! lock-free, thread-safe counter.  Every call to [`SequenceClock::next`]
//! returns a value strictly greater than the previous call (assuming fewer than
//! `u64::MAX` total increments, which is safe in practice).
//!
//! # Why this exists
//!
//! Wall-clock timestamps are subject to NTP skew and monotonic-clock resets
//! across process restarts.  Sequence numbers have neither problem: they are
//! deterministically ordered regardless of when or on which thread they were
//! generated.
//!
//! # Usage
//!
//! ```rust
//! use phantom_memory::clock::SequenceClock;
//!
//! let clock = SequenceClock::new();
//! let first = clock.next();   // 0
//! let second = clock.next();  // 1
//! assert!(second > first);
//! ```

use std::sync::atomic::{AtomicU64, Ordering};

/// A lock-free, monotonically-increasing sequence clock.
///
/// Each call to [`next`](Self::next) increments the counter and returns the
/// *previous* value (i.e. the counter starts at 0 and the first `next()`
/// returns 0, the second returns 1, and so on).  Use
/// [`current`](Self::current) to observe the counter without advancing it.
///
/// `SequenceClock` is `Send + Sync` and can be placed in an `Arc` to share
/// it across threads.
#[derive(Debug)]
pub struct SequenceClock {
    inner: AtomicU64,
}

impl Default for SequenceClock {
    fn default() -> Self {
        Self::new()
    }
}

impl SequenceClock {
    /// Create a new clock starting at zero.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: AtomicU64::new(0),
        }
    }

    /// Advance the clock and return the next sequence number.
    ///
    /// The returned value is monotonically increasing across all callers,
    /// even when called concurrently from multiple threads.
    pub fn next(&self) -> u64 {
        self.inner.fetch_add(1, Ordering::SeqCst)
    }

    /// Observe the current counter value without advancing it.
    ///
    /// Note: the returned value may already have been surpassed by another
    /// thread calling [`next`](Self::next) concurrently.
    #[must_use]
    pub fn current(&self) -> u64 {
        self.inner.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::Arc;
    use std::thread;

    use super::*;

    #[test]
    fn clock_starts_at_zero() {
        let clock = SequenceClock::new();
        assert_eq!(clock.current(), 0, "clock must start at zero");
        let first = clock.next();
        assert_eq!(first, 0, "first next() must return 0");
    }

    #[test]
    fn clock_next_increments_monotonically() {
        let clock = SequenceClock::new();
        let values: Vec<u64> = (0..10).map(|_| clock.next()).collect();

        for pair in values.windows(2) {
            assert!(
                pair[1] > pair[0],
                "sequence must be strictly increasing: {pair:?}"
            );
        }
        // After 10 next() calls, current should be 10.
        assert_eq!(clock.current(), 10);
    }

    #[test]
    fn clock_is_thread_safe() {
        let clock = Arc::new(SequenceClock::new());
        let handles: Vec<_> = (0..100)
            .map(|_| {
                let c = Arc::clone(&clock);
                thread::spawn(move || c.next())
            })
            .collect();

        let values: Vec<u64> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let unique: HashSet<u64> = values.iter().copied().collect();

        assert_eq!(
            unique.len(),
            100,
            "all 100 sequence numbers must be unique; got duplicates among {values:?}"
        );
    }
}
