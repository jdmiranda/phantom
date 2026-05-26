//! Rate limiters for the relay.
//!
//! Two complementary limiters are provided:
//!
//! - [`TokenBucket`]: a continuous token-bucket that smooths sustained
//!   throughput (one bucket per peer, keyed in the router's `rate_buckets`
//!   map).
//! - [`SlidingWindow`]: a hard per-connection window that caps the absolute
//!   number of messages in any rolling period. This is the basis of the
//!   per-connection policy-violation close (WS 1008).

use std::time::{Duration, Instant};

/// A single token-bucket for one peer.
///
/// Tokens refill continuously at `rate` tokens/second up to a `capacity` of
/// `rate` (one second's worth of bursting).  Each `check` call consumes one
/// token; if the bucket is empty the call returns `false` and the caller
/// should send a [`RateLimitExceeded`](crate::envelope::RelayMessage::RateLimitExceeded)
/// response rather than forwarding the envelope.
#[derive(Debug)]
pub struct TokenBucket {
    /// Maximum tokens (== rate for a 1-second burst window).
    capacity: f64,
    /// Fill rate in tokens per second.
    rate: f64,
    /// Current token count.
    tokens: f64,
    /// Monotonic timestamp of the last refill.
    last_refill: Instant,
}

impl TokenBucket {
    /// Create a new bucket limited to `rate` messages per second.
    #[must_use]
    pub fn new(rate: u32) -> Self {
        let capacity = f64::from(rate);
        Self {
            capacity,
            rate: capacity,
            tokens: capacity,
            last_refill: Instant::now(),
        }
    }

    /// Attempt to consume one token.
    ///
    /// Returns `true` if the message should be forwarded, `false` if the
    /// sender is over-rate.  Also returns the estimated milliseconds until
    /// one token becomes available (useful for the `retry_after_ms` field).
    pub fn check(&mut self) -> (bool, u64) {
        self.refill();
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            (true, 0)
        } else {
            // Time until we accumulate one full token.
            let missing = 1.0 - self.tokens;
            let retry_secs = missing / self.rate;
            let retry_ms = (retry_secs * 1_000.0).ceil() as u64;
            (false, retry_ms)
        }
    }

    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.rate).min(self.capacity);
        self.last_refill = now;
    }
}

// ── SlidingWindow ─────────────────────────────────────────────────────────────

/// A fixed (tumbling) window counter for per-connection message rate limiting.
///
/// Within each `window` period the counter allows at most `max_count`
/// messages. When a new message arrives after the window has expired, the
/// counter resets and the message is counted as the first in the new window.
///
/// Unlike [`TokenBucket`] this does **not** smooth bursts — it grants the full
/// `max_count` budget at the start of every window and denies all further
/// messages until the window turns over.
///
/// # Tumbling vs. truly sliding
///
/// Despite the historic type name, this is a *fixed-window* (tumbling)
/// counter, not a per-message-timestamp sliding window. The practical
/// consequence is that a sender can deliver `max_count` messages near the end
/// of one window and another `max_count` near the start of the next, yielding
/// a short-lived burst of up to `2 * max_count` across the boundary. This is
/// an acceptable trade-off for the relay (the absolute ceiling is still
/// bounded and the implementation is allocation-free), but callers should
/// size `max_count` and `window` with the worst-case `2 * max_count` burst
/// in mind.
#[derive(Debug)]
pub struct SlidingWindow {
    /// Maximum messages allowed per window.
    max_count: u32,
    /// Duration of each window.
    window: Duration,
    /// Messages counted in the current window.
    message_count: u32,
    /// Start of the current window.
    window_start: Instant,
}

impl SlidingWindow {
    /// Create a new sliding window allowing `max_count` messages per `window`.
    #[must_use]
    pub fn new(max_count: u32, window: Duration) -> Self {
        Self {
            max_count,
            window,
            message_count: 0,
            window_start: Instant::now(),
        }
    }

    /// Record one incoming message and decide whether it is within the limit.
    ///
    /// Returns `true` when the message is allowed, `false` when the sender has
    /// exceeded `max_count` for the current window.
    ///
    /// When the window has expired since the last call the counter resets
    /// before the check so the new window starts fresh.
    pub fn check(&mut self) -> bool {
        let now = Instant::now();
        if now.duration_since(self.window_start) >= self.window {
            // Window has rolled over — start a new one.
            self.message_count = 0;
            self.window_start = now;
        }
        // `saturating_add` ensures a sustained-attack peer that pushes the
        // counter to `u32::MAX` cannot wrap it back to zero and earn a fresh
        // budget for free. Once saturated the comparison stays `false` until
        // the window resets.
        self.message_count = self.message_count.saturating_add(1);
        self.message_count <= self.max_count
    }

    /// Remaining messages allowed in the current window (0 when exhausted).
    #[must_use]
    pub fn remaining(&self) -> u32 {
        self.max_count.saturating_sub(self.message_count)
    }

    /// Back-date the window start by `delta` so the current window appears
    /// expired. Intended exclusively for tests that need to simulate window
    /// rollover without sleeping.
    #[cfg(test)]
    pub fn backdate_window(&mut self, delta: Duration) {
        self.window_start -= delta;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_up_to_rate() {
        let mut bucket = TokenBucket::new(5);
        for _ in 0..5 {
            let (ok, _) = bucket.check();
            assert!(ok);
        }
        let (ok, retry_ms) = bucket.check();
        assert!(!ok, "sixth message should be rejected");
        assert!(retry_ms > 0, "retry_after_ms should be positive");
    }

    #[test]
    fn refills_over_time() {
        let mut bucket = TokenBucket::new(100);
        // Drain the bucket completely.
        for _ in 0..100 {
            bucket.check();
        }
        let (ok, _) = bucket.check();
        assert!(!ok, "bucket should be empty");

        // Manually back-date last_refill by 1 second.
        bucket.last_refill -= std::time::Duration::from_secs(1);
        let (ok, _) = bucket.check();
        assert!(ok, "bucket should have refilled after 1 s");
    }

    // ── SlidingWindow tests ────────────────────────────────────────────────────

    #[test]
    fn sliding_window_allows_up_to_max() {
        let mut sw = SlidingWindow::new(3, Duration::from_secs(10));
        assert!(sw.check(), "msg 1 must be allowed");
        assert!(sw.check(), "msg 2 must be allowed");
        assert!(sw.check(), "msg 3 must be allowed");
        assert!(!sw.check(), "msg 4 must be denied");
    }

    #[test]
    fn sliding_window_resets_after_window_expires() {
        let mut sw = SlidingWindow::new(2, Duration::from_millis(1));
        assert!(sw.check());
        assert!(sw.check());
        assert!(!sw.check(), "must be denied while window is full");

        // Back-date the window start so the window is expired.
        sw.backdate_window(Duration::from_millis(10));
        // Next check opens a new window.
        assert!(sw.check(), "must allow after window resets");
    }
}
