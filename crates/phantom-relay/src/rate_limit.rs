//! Token-bucket rate limiter, one bucket per `PeerId`.

use std::time::Instant;

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
}
