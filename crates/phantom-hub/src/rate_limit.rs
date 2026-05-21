//! Per-IP sliding-window rate limiter for the `/auth/register` endpoint.
//!
//! [`IpRateLimiter`] counts requests per [`IpAddr`] within a fixed-width
//! time window.  When the window expires for an IP the counter resets.
//! A background task should call [`IpRateLimiter::evict_stale`] periodically
//! to bound memory growth.
//!
//! # Thread safety
//!
//! All methods take `&self` — interior mutability is provided by a
//! [`std::sync::Mutex`] over a [`HashMap`].  This is intentional: the map is
//! held for microseconds at most and contention at the lock is far cheaper
//! than any alternative that involves async primitives.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// IpRateLimiter
// ---------------------------------------------------------------------------

/// Sliding-window rate limiter keyed on client [`IpAddr`].
///
/// Each IP is allowed at most `max_requests` calls within any `window`-length
/// interval.  The window resets the first time a request arrives after the
/// previous window has elapsed.
pub struct IpRateLimiter {
    /// Length of each counting window.
    window: Duration,
    /// Maximum number of requests allowed per window per IP.
    max_requests: usize,
    /// IP → (window_start, count).
    state: Mutex<HashMap<IpAddr, (Instant, usize)>>,
}

impl IpRateLimiter {
    /// Construct a new limiter.
    ///
    /// * `window`       — how long a counting window lasts.
    /// * `max_requests` — number of requests allowed per IP per window.
    #[must_use]
    pub fn new(window: Duration, max_requests: usize) -> Self {
        Self {
            window,
            max_requests,
            state: Mutex::new(HashMap::new()),
        }
    }

    /// Check whether `ip` is within its rate limit and, if so, record the
    /// request.
    ///
    /// Returns `true` if the request is allowed, `false` if the IP has
    /// exceeded `max_requests` within the current window.
    #[must_use]
    pub fn check_and_record(&self, ip: IpAddr) -> bool {
        let now = Instant::now();
        let mut state = self.state.lock().unwrap();
        let entry = state.entry(ip).or_insert((now, 0));

        if now.duration_since(entry.0) >= self.window {
            // Window has expired — start a fresh window.
            *entry = (now, 1);
            true
        } else if entry.1 < self.max_requests {
            entry.1 += 1;
            true
        } else {
            false
        }
    }

    /// Remove entries whose window started more than `2 × window` ago.
    ///
    /// Call this periodically (e.g. every two minutes) to prevent unbounded
    /// memory growth in long-running deployments.
    pub fn evict_stale(&self) {
        let cutoff = Instant::now() - self.window * 2;
        let mut state = self.state.lock().unwrap();
        state.retain(|_, (start, _)| *start > cutoff);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;
    use std::str::FromStr;

    fn ip(s: &str) -> IpAddr {
        IpAddr::from_str(s).unwrap()
    }

    // -----------------------------------------------------------------------
    // rate_limiter_allows_requests_within_limit
    // -----------------------------------------------------------------------

    #[test]
    fn rate_limiter_allows_requests_within_limit() {
        let limiter = IpRateLimiter::new(Duration::from_secs(60), 10);
        let client = ip("127.0.0.1");

        for i in 1..=10 {
            assert!(
                limiter.check_and_record(client),
                "request {i} should be allowed"
            );
        }
    }

    // -----------------------------------------------------------------------
    // rate_limiter_blocks_after_limit
    // -----------------------------------------------------------------------

    #[test]
    fn rate_limiter_blocks_after_limit() {
        let limiter = IpRateLimiter::new(Duration::from_secs(60), 10);
        let client = ip("10.0.0.1");

        for _ in 0..10 {
            let _ = limiter.check_and_record(client);
        }

        assert!(
            !limiter.check_and_record(client),
            "11th request must be blocked"
        );
    }

    // -----------------------------------------------------------------------
    // rate_limiter_resets_after_window
    // -----------------------------------------------------------------------

    #[test]
    fn rate_limiter_resets_after_window() {
        // Use a 1 ns window so it expires immediately.
        let limiter = IpRateLimiter::new(Duration::from_nanos(1), 2);
        let client = ip("192.168.1.1");

        // Fill the window.
        let _ = limiter.check_and_record(client);
        let _ = limiter.check_and_record(client);

        // Spin-wait until the window definitely expires (at least 1 µs).
        let deadline = std::time::Instant::now() + Duration::from_micros(10);
        while std::time::Instant::now() < deadline {
            std::hint::spin_loop();
        }

        // The window should have reset — this request must be allowed.
        assert!(
            limiter.check_and_record(client),
            "request after window expiry must be allowed"
        );
    }

    // -----------------------------------------------------------------------
    // rate_limiter_evicts_stale_entries
    // -----------------------------------------------------------------------

    #[test]
    fn rate_limiter_evicts_stale_entries() {
        // 1 ns window → entries are stale almost immediately.
        let limiter = IpRateLimiter::new(Duration::from_nanos(1), 10);
        let client = ip("172.16.0.1");

        let _ = limiter.check_and_record(client);

        // Wait longer than 2× the window.
        let deadline = std::time::Instant::now() + Duration::from_micros(10);
        while std::time::Instant::now() < deadline {
            std::hint::spin_loop();
        }

        {
            let state = limiter.state.lock().unwrap();
            assert_eq!(state.len(), 1, "entry present before eviction");
        }

        limiter.evict_stale();

        {
            let state = limiter.state.lock().unwrap();
            assert_eq!(state.len(), 0, "entry must be removed after evict_stale");
        }
    }
}
