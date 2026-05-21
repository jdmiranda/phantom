//! Interval-driven loop source.
//!
//! `CronSource` ticks every `interval` and emits a fresh
//! [`crate::runner::LoopInput`] each time the interval elapses. It's the
//! canonical source for agentless poll loops like the issue-#650 PR-finder.
//!
//! Between ticks `next` returns [`crate::runner::LoopPullResult::Empty`] and
//! the runner backs off and re-polls — see the runner's `IDLE_BACKOFF`.

use std::time::{Duration, Instant};

use serde_json::json;

use crate::runner::source::{
    CorrelationId, LoopContext, LoopInput, LoopPullResult, LoopSource,
};

/// A source that emits one input per fixed-interval tick.
///
/// The first call to [`Self::next`] *always* emits an input — there is no
/// startup delay. Subsequent calls return `Empty` until `interval` has
/// elapsed since the previous emission.
#[derive(Debug)]
pub struct CronSource {
    interval: Duration,
    last_tick: Option<Instant>,
    /// Monotonic counter for tick payloads and correlation ids. Avoids the
    /// ambiguity of a wall-clock time-since-epoch when tests pause the
    /// tokio clock.
    tick_count: u64,
}

impl CronSource {
    /// Build a source ticking every `interval`. The first `next` call
    /// emits immediately.
    #[must_use]
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            last_tick: None,
            tick_count: 0,
        }
    }

    /// Build a source ticking every `interval_seconds`. Convenience for
    /// callers wiring directly from [`crate::source::LoopSourceSpec::Cron`].
    #[must_use]
    pub fn from_seconds(interval_seconds: u64) -> Self {
        Self::new(Duration::from_secs(interval_seconds))
    }

    /// Build the payload for a tick. Public for tests, not for production.
    fn tick_payload(&self, ctx: &LoopContext) -> serde_json::Value {
        json!({
            "kind": "cron_tick",
            "loop_id": ctx.loop_id,
            "tick_count": self.tick_count,
        })
    }

    /// Build the correlation id for a tick. Format keeps tests asserting on
    /// the exact string and makes log lines self-describing.
    fn tick_correlation(&self, ctx: &LoopContext) -> CorrelationId {
        CorrelationId::new(format!("cron:{}:tick:{}", ctx.loop_id, self.tick_count))
    }
}

impl LoopSource for CronSource {
    fn next(&mut self, ctx: &LoopContext) -> LoopPullResult {
        let now = Instant::now();
        let due = match self.last_tick {
            None => true,
            Some(prev) => now.duration_since(prev) >= self.interval,
        };
        if !due {
            return LoopPullResult::Empty;
        }

        self.tick_count = self.tick_count.saturating_add(1);
        self.last_tick = Some(now);
        let payload = self.tick_payload(ctx);
        let correlation_id = self.tick_correlation(ctx);

        LoopPullResult::Available(LoopInput {
            key: format!("cron:{}:{}", ctx.loop_id, self.tick_count),
            payload,
            correlation_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_pull_always_emits() {
        let mut s = CronSource::new(Duration::from_secs(3600));
        let ctx = LoopContext { loop_id: "t".to_string() };
        match s.next(&ctx) {
            LoopPullResult::Available(input) => {
                assert_eq!(input.payload["tick_count"], 1);
                assert_eq!(input.correlation_id.as_str(), "cron:t:tick:1");
            }
            other => panic!("expected Available, got {other:?}"),
        }
    }

    #[test]
    fn second_pull_within_interval_is_empty() {
        let mut s = CronSource::new(Duration::from_secs(3600));
        let ctx = LoopContext { loop_id: "t".to_string() };
        let _ = s.next(&ctx);
        match s.next(&ctx) {
            LoopPullResult::Empty => {}
            other => panic!("expected Empty, got {other:?}"),
        }
    }

    #[test]
    fn second_pull_after_interval_emits_again() {
        // 0ms interval makes every subsequent pull due immediately —
        // simpler than sleeping in a unit test, and the runner-side
        // integration tests cover the real timing behaviour.
        let mut s = CronSource::new(Duration::from_millis(0));
        let ctx = LoopContext { loop_id: "t".to_string() };
        let _ = s.next(&ctx);
        // Sleep one millisecond so `now > last_tick + 0` even on the
        // monotonic-clock systems where two Instant reads can return equal
        // values.
        std::thread::sleep(Duration::from_millis(1));
        match s.next(&ctx) {
            LoopPullResult::Available(input) => {
                assert_eq!(input.payload["tick_count"], 2);
            }
            other => panic!("expected Available, got {other:?}"),
        }
    }

    #[test]
    fn from_seconds_matches_duration_from_secs() {
        let a = CronSource::from_seconds(60);
        let b = CronSource::new(Duration::from_secs(60));
        assert_eq!(a.interval, b.interval);
    }
}
