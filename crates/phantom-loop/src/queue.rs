//! In-memory cross-loop message queues.
//!
//! The loop-overseer model from issue #650 carries typed messages between
//! loops — the PR-finder loop pushes a message naming a PR to review onto
//! the `review-queue`, and the reviewer loop's [`crate::sources::LoopMessageQueueSource`]
//! pops from that same queue. For MVP we keep this entirely in-process; a
//! JSONL or SQLite backing is explicitly out of scope for C2.
//!
//! # Concurrency model
//!
//! [`LoopQueueRegistry::get_or_create`] returns an `Arc<LoopQueue>`, so
//! multiple [`crate::runner::LoopRunner`]s can hold their own handles to the
//! same queue without re-locking the registry on every push/pop. The
//! registry-level [`std::sync::Mutex`] only contends on first-touch
//! `get_or_create`, never on hot-path `push` / `pop`.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

/// A typed cross-loop message.
///
/// Effect-side encoding: when a [`crate::LoopEffect::EnqueueTo`] fires after
/// a successful iteration, the [`crate::effect_runner::run_effects`] driver
/// builds one of these from the iteration result. Source-side: a
/// [`crate::sources::LoopMessageQueueSource`] pops one and maps it onto a
/// [`crate::runner::LoopInput`].
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LoopMessage {
    /// User-chosen `id` of the loop that produced this message. Lets the
    /// consuming loop trace causality without inspecting payload fields.
    pub from_loop: String,

    /// Wall-clock unix-millis at the moment the producing loop enqueued the
    /// message. Cheap to compute (`SystemTime::now`) and stable across the
    /// test runner's `tokio::time::pause`, which only freezes the tokio
    /// clock — not the system clock.
    pub at_unix_ms: u64,

    /// Free-form JSON payload. Field shape is whatever the producing loop's
    /// [`crate::LoopEffect::EnqueueTo`] mapping built.
    pub payload: serde_json::Value,
}

impl LoopMessage {
    /// Construct a message with `at_unix_ms` filled from the current system
    /// clock. A clamped-to-zero value is returned if the system clock is set
    /// before the unix epoch (effectively impossible on a real machine, but
    /// keeps the API total).
    #[must_use]
    pub fn new(from_loop: impl Into<String>, payload: serde_json::Value) -> Self {
        let at_unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0);
        Self {
            from_loop: from_loop.into(),
            at_unix_ms,
            payload,
        }
    }
}

/// A single named FIFO queue of [`LoopMessage`]s.
///
/// Wrapped behind an `Arc` by the registry so multiple consumers can pop
/// without coordinating through the registry on every operation. The inner
/// [`Mutex<VecDeque<_>>`] is a deliberate choice over `tokio::sync::Mutex`:
/// the push/pop critical section is microseconds-long and never crosses an
/// `await`, so a sync mutex avoids the runtime-pinning that the tokio mutex
/// imposes on holders.
#[derive(Debug)]
pub struct LoopQueue {
    name: String,
    inner: Mutex<VecDeque<LoopMessage>>,
}

impl LoopQueue {
    /// Construct an empty queue with the given name.
    #[must_use]
    fn new(name: String) -> Self {
        Self {
            name,
            inner: Mutex::new(VecDeque::new()),
        }
    }

    /// Return the name passed to [`LoopQueueRegistry::get_or_create`].
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Push `msg` onto the back of the queue.
    ///
    /// A poisoned mutex is silently re-acquired via [`Mutex::into_inner`]
    /// semantics on lock: any prior panic-while-locked leaves the queue
    /// readable but possibly in a logically-inconsistent state. For MVP we
    /// prefer "best effort enqueue" over a noisy error path the runner
    /// would just log and ignore.
    pub fn push(&self, msg: LoopMessage) {
        if let Ok(mut q) = self.inner.lock() {
            q.push_back(msg);
        } else {
            tracing::warn!(queue = %self.name, "loop queue mutex poisoned; dropping enqueue");
        }
    }

    /// Pop the front message, or `None` if empty.
    #[must_use]
    pub fn pop(&self) -> Option<LoopMessage> {
        match self.inner.lock() {
            Ok(mut q) => q.pop_front(),
            Err(_) => {
                tracing::warn!(queue = %self.name, "loop queue mutex poisoned; returning None");
                None
            }
        }
    }

    /// Return the current queue depth. Primarily for tests and metrics —
    /// production loops should not branch on this value.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.lock().map(|q| q.len()).unwrap_or(0)
    }

    /// Convenience wrapper around `len() == 0`. Same poisoning semantics.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Process-global directory of named [`LoopQueue`]s.
///
/// `LoopQueueRegistry::get_or_create` is the canonical way to obtain a
/// queue handle: identical names always resolve to the same `Arc<LoopQueue>`,
/// so producers and consumers naturally rendezvous without coordinating ids.
///
/// The MVP keeps a single registry per loop-runner process. C3 will pass the
/// registry into the CLI subcommand and clone its `Arc` into each runner.
#[derive(Debug, Default)]
pub struct LoopQueueRegistry {
    queues: Mutex<HashMap<String, Arc<LoopQueue>>>,
}

impl LoopQueueRegistry {
    /// Construct an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Resolve `name` to an `Arc<LoopQueue>`, creating one if absent.
    ///
    /// Always returns the *same* `Arc` for the same name within one
    /// registry, so the runner can `clone` the handle once at startup and
    /// drop the registry reference for the lifetime of the loop.
    pub fn get_or_create(&self, name: &str) -> Arc<LoopQueue> {
        // The lock window covers the entry-vacancy check and the insert,
        // but never crosses an `await` — so it's safe to use the std mutex.
        let mut queues = match self.queues.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(q) = queues.get(name) {
            return q.clone();
        }
        let q = Arc::new(LoopQueue::new(name.to_owned()));
        queues.insert(name.to_owned(), q.clone());
        q
    }

    /// Convenience: push directly to `name`, allocating the queue if needed.
    pub fn push(&self, name: &str, msg: LoopMessage) {
        self.get_or_create(name).push(msg);
    }

    /// Convenience: pop from `name`. Returns `None` if the queue doesn't
    /// exist yet (no get-or-create on the pop side — a never-touched queue
    /// is genuinely empty).
    #[must_use]
    pub fn pop(&self, name: &str) -> Option<LoopMessage> {
        let queues = match self.queues.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        queues.get(name).and_then(|q| q.pop())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn get_or_create_is_idempotent_for_the_same_name() {
        let reg = LoopQueueRegistry::new();
        let a = reg.get_or_create("foo");
        let b = reg.get_or_create("foo");
        // Same Arc instance — the second call must reuse the first allocation.
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn distinct_names_get_distinct_queues() {
        let reg = LoopQueueRegistry::new();
        let a = reg.get_or_create("alpha");
        let b = reg.get_or_create("beta");
        assert!(!Arc::ptr_eq(&a, &b));
        assert_eq!(a.name(), "alpha");
        assert_eq!(b.name(), "beta");
    }

    #[test]
    fn push_and_pop_preserve_fifo_order() {
        let reg = LoopQueueRegistry::new();
        let m1 = LoopMessage::new("producer", json!({"i": 1}));
        let m2 = LoopMessage::new("producer", json!({"i": 2}));
        reg.push("q", m1);
        reg.push("q", m2);
        let p1 = reg.pop("q").expect("first message");
        let p2 = reg.pop("q").expect("second message");
        assert_eq!(p1.payload["i"], 1);
        assert_eq!(p2.payload["i"], 2);
        assert!(reg.pop("q").is_none(), "queue must be empty after draining");
    }

    #[test]
    fn pop_on_unknown_queue_returns_none() {
        let reg = LoopQueueRegistry::new();
        assert!(reg.pop("never-created").is_none());
    }

    #[test]
    fn len_and_is_empty_track_push_pop() {
        let reg = LoopQueueRegistry::new();
        let q = reg.get_or_create("q");
        assert!(q.is_empty());
        assert_eq!(q.len(), 0);
        q.push(LoopMessage::new("p", json!({})));
        assert!(!q.is_empty());
        assert_eq!(q.len(), 1);
        let _ = q.pop();
        assert!(q.is_empty());
    }
}
