//! Agent lifecycle hooks — inversion-of-control slots for spawn/start/complete/fail/approve/deny.
//!
//! Every significant state transition an agent can undergo is represented as a
//! [`LifecycleEvent`] variant. External callers register [`LifecycleHook`]
//! callbacks via [`LifecycleHooks::register`]; the substrate calls
//! [`LifecycleHooks::fire`] at each transition.
//!
//! ## Design rules
//!
//! - Hooks run synchronously in the caller's thread — they are cheap observers,
//!   not async workers.
//! - A panicking hook does **not** propagate to the caller. Each hook invocation
//!   is wrapped in [`std::panic::catch_unwind`] so a broken observer cannot
//!   crash the agent runtime.
//! - `LifecycleHooks` is `Send + Sync` via the `Arc<dyn Fn…>` bound on
//!   [`LifecycleHook`].
//!
//! ## Example
//!
//! ```rust
//! use phantom_agents::lifecycle::{LifecycleEvent, LifecycleHooks};
//! use phantom_agents::dispatch::Disposition;
//! use std::sync::Arc;
//!
//! let mut hooks = LifecycleHooks::new();
//! hooks.register(Arc::new(|event| {
//!     eprintln!("[lifecycle] {event:?}");
//! }));
//! hooks.fire(&LifecycleEvent::Started { agent_id: 1 });
//! ```

use std::sync::Arc;

use crate::dispatch::Disposition;

// ---------------------------------------------------------------------------
// LifecycleEvent
// ---------------------------------------------------------------------------

/// A significant state transition in an agent's lifetime.
///
/// Each variant corresponds to one well-defined phase of the agent FSM
/// (see [`crate::agent::AgentStatus`]).
#[derive(Debug, Clone)]
pub enum LifecycleEvent {
    /// The agent has been registered and queued for execution.
    ///
    /// Fired immediately after the agent is inserted into the manager pool,
    /// before it enters the `Working` state.
    Spawned {
        /// Stable numeric identifier for this agent within the session.
        agent_id: u64,
        /// The task string passed to the agent at spawn time.
        task: String,
        /// Spawn-time intent classification (e.g. `Feature`, `BugFix`).
        disposition: Disposition,
    },

    /// The agent has transitioned from `Queued` (or `Planning`) to `Working`.
    ///
    /// This is the "first tool turn" boundary — fired once per agent lifetime.
    Started {
        /// Stable numeric identifier for this agent within the session.
        agent_id: u64,
    },

    /// The agent finished successfully (`AgentStatus::Done`).
    Completed {
        /// Stable numeric identifier for this agent within the session.
        agent_id: u64,
        /// Short human-readable summary of what the agent accomplished.
        summary: String,
    },

    /// The agent transitioned to `AgentStatus::Failed` or `AgentStatus::Flatline`.
    Failed {
        /// Stable numeric identifier for this agent within the session.
        agent_id: u64,
        /// Human-readable reason for the failure.
        reason: String,
    },

    /// The agent has produced a plan and is waiting for user/policy approval
    /// (`AgentStatus::AwaitingApproval`).
    ApprovalRequired {
        /// Stable numeric identifier for this agent within the session.
        agent_id: u64,
    },

    /// The agent's plan was approved and it has resumed `Working`.
    Approved {
        /// Stable numeric identifier for this agent within the session.
        agent_id: u64,
    },

    /// A tool dispatch was denied for the agent (capability gate or quarantine).
    ///
    /// Fired by the dispatch layer on every `CapabilityDenied` outcome so
    /// audit hooks can track denial patterns without coupling to the
    /// quarantine registry.
    Denied {
        /// Stable numeric identifier for this agent within the session.
        agent_id: u64,
        /// Human-readable description of the denied capability or tool.
        capability: String,
    },
}

// ---------------------------------------------------------------------------
// LifecycleHook
// ---------------------------------------------------------------------------

/// A registered lifecycle observer.
///
/// The `Arc<dyn Fn(…)>` form lets multiple owners share the same hook without
/// cloning the closure body, and satisfies `Send + Sync` for cross-thread
/// registrars.
pub type LifecycleHook = Arc<dyn Fn(&LifecycleEvent) + Send + Sync>;

// ---------------------------------------------------------------------------
// LifecycleHooks
// ---------------------------------------------------------------------------

/// Registry of zero-or-more [`LifecycleHook`] callbacks.
///
/// Register hooks with [`register`](Self::register); fire them with
/// [`fire`](Self::fire). A panicking hook is caught and silently discarded —
/// it does not abort the `fire` loop or propagate to the caller.
///
/// `LifecycleHooks` is `Clone` (clones the `Arc` handles, not the closures).
#[derive(Default, Clone)]
pub struct LifecycleHooks {
    hooks: Vec<LifecycleHook>,
}

impl LifecycleHooks {
    /// Create an empty hook registry.
    #[must_use]
    pub fn new() -> Self {
        Self { hooks: Vec::new() }
    }

    /// Register a new lifecycle observer.
    ///
    /// Hooks are called in registration order by [`fire`](Self::fire).
    pub fn register(&mut self, hook: LifecycleHook) {
        self.hooks.push(hook);
    }

    /// Invoke all registered hooks with `event`.
    ///
    /// Each hook is called in registration order. A hook that panics is
    /// caught via [`std::panic::catch_unwind`]; the panic is swallowed and
    /// the remaining hooks continue to run.
    pub fn fire(&self, event: &LifecycleEvent) {
        for hook in &self.hooks {
            // Clone the Arc so the closure can be called via `catch_unwind`.
            // `AssertUnwindSafe` is intentional: we own the event reference
            // and the hook's closure; a panic inside the hook should not
            // corrupt our state — we simply discard it.
            let hook = hook.clone();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                hook(event);
            }));
            // Silently discard panics — a broken observer must not crash the
            // caller. Callers that need visibility can use the `Failed` event
            // variant instead.
            let _ = result;
        }
    }

    /// Return the number of registered hooks.
    #[must_use]
    pub fn len(&self) -> usize {
        self.hooks.len()
    }

    /// Return `true` iff no hooks have been registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }
}

impl std::fmt::Debug for LifecycleHooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LifecycleHooks")
            .field("hook_count", &self.hooks.len())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Helper: build a `Spawned` event for use in multiple tests.
    fn spawned_event(agent_id: u64) -> LifecycleEvent {
        LifecycleEvent::Spawned {
            agent_id,
            task: "write the tests".into(),
            disposition: Disposition::Feature,
        }
    }

    /// A registered hook fires when `fire` is called with a `Spawned` event.
    #[test]
    fn hook_fires_on_spawned_event() {
        let fired: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
        let fired_clone = fired.clone();

        let mut hooks = LifecycleHooks::new();
        hooks.register(Arc::new(move |event| {
            if let LifecycleEvent::Spawned { agent_id, .. } = event {
                fired_clone.lock().unwrap().push(*agent_id);
            }
        }));

        hooks.fire(&spawned_event(42));

        let ids = fired.lock().unwrap();
        assert_eq!(*ids, vec![42u64], "hook must fire once with agent_id=42");
    }

    /// A registered hook fires when `fire` is called with a `Completed` event.
    #[test]
    fn hook_fires_on_completed_event() {
        let summaries: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let summaries_clone = summaries.clone();

        let mut hooks = LifecycleHooks::new();
        hooks.register(Arc::new(move |event| {
            if let LifecycleEvent::Completed { summary, .. } = event {
                summaries_clone.lock().unwrap().push(summary.clone());
            }
        }));

        hooks.fire(&LifecycleEvent::Completed {
            agent_id: 7,
            summary: "opened PR #42".into(),
        });

        let got = summaries.lock().unwrap();
        assert_eq!(got.as_slice(), ["opened PR #42"]);
    }

    /// Every registered hook fires — none are skipped.
    #[test]
    fn multiple_hooks_all_fire() {
        let counter: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));

        let mut hooks = LifecycleHooks::new();
        for _ in 0..5 {
            let counter_clone = counter.clone();
            hooks.register(Arc::new(move |_event| {
                *counter_clone.lock().unwrap() += 1;
            }));
        }

        hooks.fire(&LifecycleEvent::Started { agent_id: 1 });

        assert_eq!(
            *counter.lock().unwrap(),
            5,
            "all 5 hooks must fire exactly once"
        );
    }

    /// A hook that panics must not propagate the panic to the caller, and
    /// subsequent hooks in the chain must still execute.
    #[test]
    fn hook_panic_does_not_propagate() {
        let after_panic_fired: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
        let after_panic_fired_clone = after_panic_fired.clone();

        let mut hooks = LifecycleHooks::new();

        // Hook 1: panics.
        hooks.register(Arc::new(|_event| {
            panic!("intentional hook panic — must be swallowed");
        }));

        // Hook 2: must still run after hook 1 panics.
        hooks.register(Arc::new(move |_event| {
            *after_panic_fired_clone.lock().unwrap() = true;
        }));

        // This call must not panic or unwind into the test.
        hooks.fire(&LifecycleEvent::Failed {
            agent_id: 99,
            reason: "something went wrong".into(),
        });

        assert!(
            *after_panic_fired.lock().unwrap(),
            "hook after the panicking hook must still execute"
        );
    }

    /// `Denied` event carries the capability string.
    #[test]
    fn denied_event_carries_capability() {
        let capabilities: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let cap_clone = capabilities.clone();

        let mut hooks = LifecycleHooks::new();
        hooks.register(Arc::new(move |event| {
            if let LifecycleEvent::Denied { capability, .. } = event {
                cap_clone.lock().unwrap().push(capability.clone());
            }
        }));

        hooks.fire(&LifecycleEvent::Denied {
            agent_id: 3,
            capability: "Act not in Watcher manifest".into(),
        });

        let got = capabilities.lock().unwrap();
        assert_eq!(got.as_slice(), ["Act not in Watcher manifest"]);
    }

    /// Empty hook registry: `fire` is a no-op (must not panic).
    #[test]
    fn empty_hooks_fire_is_noop() {
        let hooks = LifecycleHooks::new();
        // Must not panic.
        hooks.fire(&LifecycleEvent::Started { agent_id: 0 });
        assert!(hooks.is_empty());
        assert_eq!(hooks.len(), 0);
    }

    /// `ApprovalRequired` and `Approved` events round-trip through the hook.
    #[test]
    fn approval_events_fire() {
        let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let events_clone = events.clone();

        let mut hooks = LifecycleHooks::new();
        hooks.register(Arc::new(move |event| {
            let label = match event {
                LifecycleEvent::ApprovalRequired { .. } => "required",
                LifecycleEvent::Approved { .. } => "approved",
                _ => "other",
            };
            events_clone.lock().unwrap().push(label.to_string());
        }));

        hooks.fire(&LifecycleEvent::ApprovalRequired { agent_id: 5 });
        hooks.fire(&LifecycleEvent::Approved { agent_id: 5 });

        let got = events.lock().unwrap();
        assert_eq!(got.as_slice(), ["required", "approved"]);
    }
}
