//! Queue-driven loop source.
//!
//! `LoopMessageQueueSource` pops from a named [`crate::LoopQueue`] in a
//! [`crate::LoopQueueRegistry`]. It's the consumer side of the
//! producer/consumer pattern that the PR-finder/Reviewer/Implementer
//! pipeline from issue #650 relies on.

use std::sync::Arc;

use crate::queue::{LoopMessage, LoopQueue, LoopQueueRegistry};
use crate::runner::source::{
    CorrelationId, LoopContext, LoopInput, LoopPullResult, LoopSource,
};

/// Source that drains messages from a specific named queue.
///
/// Returns [`LoopPullResult::Available`] for each popped message,
/// [`LoopPullResult::Empty`] when the queue is empty. Never returns `Done`
/// — a queue source is open-ended by design; producers may push at any
/// time.
pub struct LoopMessageQueueSource {
    queue: Arc<LoopQueue>,
    /// Counter used to assign monotonic ids when a message has no
    /// natural key field in its payload. Survives queue drains; resets
    /// only on source recreation.
    pop_count: u64,
}

impl LoopMessageQueueSource {
    /// Build a source backed by the queue named `name` inside `registry`.
    /// The queue is allocated lazily via
    /// [`LoopQueueRegistry::get_or_create`], so this constructor cannot
    /// fail even if no producer has touched the queue yet.
    #[must_use]
    pub fn new(registry: &LoopQueueRegistry, name: &str) -> Self {
        Self {
            queue: registry.get_or_create(name),
            pop_count: 0,
        }
    }

    /// Convenience: build directly from an `Arc<LoopQueue>`. Useful in
    /// tests that already hold a queue handle.
    #[must_use]
    pub fn from_queue(queue: Arc<LoopQueue>) -> Self {
        Self { queue, pop_count: 0 }
    }

    /// Translate a popped [`LoopMessage`] into a [`LoopInput`].
    ///
    /// `key` is taken from `payload.key` if present, falling back to
    /// `<queue-name>:msg:<pop_count>`. `correlation_id` inherits from the
    /// producing loop (`<from_loop>:msg:<at_unix_ms>`) so causality
    /// threads through the system without re-coining ids on every hop.
    fn message_to_input(&self, msg: LoopMessage) -> LoopInput {
        let key = msg
            .payload
            .get("key")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| format!("{}:msg:{}", self.queue.name(), self.pop_count));
        let correlation_id = CorrelationId::new(format!(
            "{}:msg:{}",
            msg.from_loop, msg.at_unix_ms
        ));
        LoopInput {
            key,
            payload: msg.payload,
            correlation_id,
        }
    }
}

impl LoopSource for LoopMessageQueueSource {
    fn next(&mut self, _ctx: &LoopContext) -> LoopPullResult {
        match self.queue.pop() {
            Some(msg) => {
                self.pop_count = self.pop_count.saturating_add(1);
                LoopPullResult::Available(self.message_to_input(msg))
            }
            None => LoopPullResult::Empty,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn empty_queue_returns_empty() {
        let reg = LoopQueueRegistry::new();
        let mut s = LoopMessageQueueSource::new(&reg, "q");
        let ctx = LoopContext { loop_id: "consumer".to_string() };
        assert!(matches!(s.next(&ctx), LoopPullResult::Empty));
    }

    #[test]
    fn popped_message_maps_to_input_with_inherited_correlation() {
        let reg = LoopQueueRegistry::new();
        reg.push(
            "q",
            LoopMessage {
                from_loop: "producer".to_string(),
                at_unix_ms: 1_700_000_000_000,
                payload: json!({"target_pr": "https://github.com/x/y/pull/1"}),
            },
        );
        let mut s = LoopMessageQueueSource::new(&reg, "q");
        let ctx = LoopContext { loop_id: "consumer".to_string() };
        match s.next(&ctx) {
            LoopPullResult::Available(input) => {
                assert_eq!(
                    input.correlation_id.as_str(),
                    "producer:msg:1700000000000",
                    "correlation must inherit from the producing loop"
                );
                assert_eq!(input.key, "q:msg:1");
                assert_eq!(
                    input.payload["target_pr"],
                    "https://github.com/x/y/pull/1"
                );
            }
            other => panic!("expected Available, got {other:?}"),
        }
    }

    #[test]
    fn payload_key_field_overrides_synthetic_key() {
        let reg = LoopQueueRegistry::new();
        reg.push(
            "q",
            LoopMessage {
                from_loop: "p".to_string(),
                at_unix_ms: 1,
                payload: json!({"key": "issue#1234"}),
            },
        );
        let mut s = LoopMessageQueueSource::new(&reg, "q");
        let ctx = LoopContext { loop_id: "consumer".to_string() };
        match s.next(&ctx) {
            LoopPullResult::Available(input) => assert_eq!(input.key, "issue#1234"),
            other => panic!("expected Available, got {other:?}"),
        }
    }

    #[test]
    fn draining_queue_returns_empty_after_last_message() {
        let reg = LoopQueueRegistry::new();
        reg.push("q", LoopMessage::new("p", json!({})));
        let mut s = LoopMessageQueueSource::new(&reg, "q");
        let ctx = LoopContext { loop_id: "c".to_string() };
        assert!(matches!(s.next(&ctx), LoopPullResult::Available(_)));
        assert!(matches!(s.next(&ctx), LoopPullResult::Empty));
    }
}
