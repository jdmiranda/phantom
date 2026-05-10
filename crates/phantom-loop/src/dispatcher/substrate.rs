//! Substrate-backed [`crate::AgentDispatcher`] implementation.
//!
//! [`SubstrateAgentDispatcher`] is the production wire-up between a
//! [`crate::LoopRunner`] and the phantom-agents spawn-subagent substrate.
//!
//! # Topology
//!
//! The substrate has two halves that meet through this dispatcher:
//!
//! ```text
//!  ┌──────────────────────────────────────────────────────────┐
//!  │ phantom-loop side (this crate)                           │
//!  │                                                          │
//!  │   LoopRunner.dispatch                                    │
//!  │      │                                                   │
//!  │      ▼                                                   │
//!  │   SubstrateAgentDispatcher                               │
//!  │      │  (1) allocate AgentId                             │
//!  │      │  (2) register pending oneshot under that id       │
//!  │      │  (3) push SpawnSubagentRequest onto spawn_queue   │
//!  │      │  (4) return DispatchHandle{ agent_id, rx }        │
//!  └──────│───────────────────────────────────────────────────┘
//!         ▼
//!  ┌──────────────────────────────────────────────────────────┐
//!  │ phantom-agents / phantom-app substrate                    │
//!  │                                                          │
//!  │   spawn_queue ──drained by App.update──► spawn pane      │
//!  │   pane runs Agent loop, agent emits complete_task        │
//!  │   pane adapter emits Event::AgentTaskComplete            │
//!  └──────│───────────────────────────────────────────────────┘
//!         ▼
//!  ┌──────────────────────────────────────────────────────────┐
//!  │ Completion routing (this module)                          │
//!  │                                                          │
//!  │   SubstrateCompletionRouter.on_completion(event)         │
//!  │      │  (5) look up pending entry by agent_id            │
//!  │      │  (6) fulfil the oneshot with the result payload   │
//!  └──────────────────────────────────────────────────────────┘
//! ```
//!
//! Routing the event onto the dispatcher is the substrate caller's job.
//! Typical wiring inside `phantom loop run`:
//!
//! ```rust,ignore
//! let dispatcher = Arc::new(SubstrateAgentDispatcher::new(spawn_queue));
//! let router = dispatcher.completion_router();
//! tokio::spawn(async move {
//!     // …subscribe to the App's event bus…
//!     while let Some(event) = bus_rx.recv().await {
//!         if let phantom_protocol::Event::AgentTaskComplete { .. } = &event {
//!             router.on_completion(event);
//!         }
//!     }
//! });
//! ```
//!
//! The dispatcher itself stays passive — it does not pull from any
//! subscription, which keeps it usable in tests with manual event injection
//! (see `tests/end_to_end_loop.rs`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;
use tokio::sync::oneshot;

use phantom_agents::agent::AgentId;
use phantom_agents::composer_tools::{SpawnSubagentQueue, SpawnSubagentRequest};
use phantom_protocol::Event;

use crate::runner::dispatcher::{AgentDispatcher, DispatchError, DispatchHandle};
use crate::runner::source::LoopInput;
use crate::spec::LoopAgentSpec;

/// Starting id for dispatcher-allocated `AgentId`s.
///
/// Picked well above the typical composer-side allocator
/// ([`phantom_agents::composer_tools::allocate_subagent_id`] starts at
/// 10_000) so dispatcher-allocated ids and composer-allocated ids do not
/// alias in the same process. C3 doesn't share an allocator with the
/// composer; if the CLI later booted a full App, the App's allocator
/// would replace this.
const SUBSTRATE_DISPATCHER_ID_BASE: u64 = 50_000;

// ---------------------------------------------------------------------------
// SubstrateAgentDispatcher
// ---------------------------------------------------------------------------

/// Production [`AgentDispatcher`] backed by
/// [`SpawnSubagentQueue`] for spawning and an
/// in-process map for completion correlation.
///
/// Construct one per process (or per `phantom loop run` invocation),
/// share via `Arc`. Internally uses a `Mutex<HashMap<AgentId, _>>` to
/// register pending oneshots; lock contention is microseconds-long and
/// never crosses an `.await`.
pub struct SubstrateAgentDispatcher {
    /// The spawn queue the substrate drains. The dispatcher does **not**
    /// own the drainer — the substrate's `App::update` (or a CLI-side
    /// headless driver) is responsible for popping requests off this queue
    /// and actually starting the agent.
    spawn_queue: SpawnSubagentQueue,
    /// In-flight dispatch correlation: maps an allocated `AgentId` to the
    /// oneshot sender the runner is awaiting on.
    pending: Mutex<HashMap<AgentId, oneshot::Sender<Result<Value, DispatchError>>>>,
    /// Monotonic allocator for fresh `AgentId`s.
    next_id: AtomicU64,
    /// Parent agent id stamped on every emitted [`SpawnSubagentRequest`].
    /// `phantom loop run` is the "parent" — substrate code reads this to
    /// thread provenance through `SpawnSource::Agent`.
    parent_id: AgentId,
}

impl SubstrateAgentDispatcher {
    /// Construct a dispatcher backed by `spawn_queue`. The `parent_id` is
    /// stamped on every [`SpawnSubagentRequest`] so substrate-side
    /// auditing can trace causality back to the CLI invocation.
    #[must_use]
    pub fn new(spawn_queue: SpawnSubagentQueue, parent_id: AgentId) -> Self {
        Self {
            spawn_queue,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(SUBSTRATE_DISPATCHER_ID_BASE),
            parent_id,
        }
    }

    /// Construct a default dispatcher with parent_id `0`. Useful for tests
    /// that don't care about provenance threading.
    #[must_use]
    pub fn with_default_parent(spawn_queue: SpawnSubagentQueue) -> Self {
        Self::new(spawn_queue, 0)
    }

    /// Allocate a fresh `AgentId` from the dispatcher's monotonic counter.
    fn alloc_id(&self) -> AgentId {
        self.next_id.fetch_add(1, Ordering::SeqCst)
    }

    /// Build a [`SubstrateCompletionRouter`] handle that fulfils pending
    /// oneshots when fed `AgentTaskComplete` events.
    ///
    /// One router can be cloned freely — internally it just holds the
    /// same `Arc<Mutex<…>>` view into the dispatcher's pending map.
    #[must_use]
    pub fn completion_router(self: &Arc<Self>) -> SubstrateCompletionRouter {
        SubstrateCompletionRouter {
            dispatcher: Arc::clone(self),
        }
    }

    /// Inspect current pending count. Public for tests and `phantom loop
    /// status`.
    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.pending.lock().map(|p| p.len()).unwrap_or(0)
    }

    /// Internal: register a pending oneshot under `agent_id`. Replaces any
    /// prior entry (which would only happen on AgentId collision — a
    /// substrate-side bug).
    fn register_pending(
        &self,
        agent_id: AgentId,
        sender: oneshot::Sender<Result<Value, DispatchError>>,
    ) {
        let mut map = match self.pending.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if map.insert(agent_id, sender).is_some() {
            tracing::warn!(
                agent_id,
                "SubstrateAgentDispatcher: AgentId collision — overwriting prior pending entry"
            );
        }
    }

    /// Internal: remove and return the pending entry for `agent_id`, if any.
    fn take_pending(
        &self,
        agent_id: AgentId,
    ) -> Option<oneshot::Sender<Result<Value, DispatchError>>> {
        let mut map = match self.pending.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        map.remove(&agent_id)
    }

    /// Push a [`SpawnSubagentRequest`] onto the substrate spawn queue.
    fn enqueue_spawn(&self, req: SpawnSubagentRequest) -> Result<(), DispatchError> {
        let mut q = self
            .spawn_queue
            .lock()
            .map_err(|_| DispatchError::Spawn("spawn queue poisoned".to_string()))?;
        q.push_back(req);
        Ok(())
    }
}

impl AgentDispatcher for SubstrateAgentDispatcher {
    fn dispatch(
        &self,
        spec: &LoopAgentSpec,
        input: &LoopInput,
    ) -> Result<DispatchHandle, DispatchError> {
        // Allocate a fresh id, register the pending oneshot under it, push
        // the spawn request, and return the handle.
        let agent_id = self.alloc_id();
        let (tx, rx) = oneshot::channel();
        self.register_pending(agent_id, tx);

        // Assemble the task prompt: the spec's `system_prompt` is the
        // template, the input payload is injected as a JSON block so the
        // agent can read the LoopInput verbatim.
        let task_prompt = format!(
            "{}\n\n## Loop input\n\nkey: {}\ncorrelation_id: {}\npayload:\n```json\n{}\n```",
            spec.system_prompt,
            input.key,
            input.correlation_id,
            serde_json::to_string_pretty(&input.payload).unwrap_or_else(|_| "{}".to_string()),
        );

        let req = SpawnSubagentRequest {
            assigned_id: agent_id,
            role: spec.role,
            label: format!("loop-agent#{agent_id}"),
            task: task_prompt,
            chat_model: None,
            parent: self.parent_id,
            handoff_context: None,
        };

        if let Err(e) = self.enqueue_spawn(req) {
            // Roll back the pending registration so the oneshot does not
            // leak.
            let _ = self.take_pending(agent_id);
            return Err(e);
        }

        Ok(DispatchHandle::new(agent_id, rx))
    }
}

// ---------------------------------------------------------------------------
// SubstrateCompletionRouter
// ---------------------------------------------------------------------------

/// Helper that closes the loop between a substrate-emitted
/// [`Event::AgentTaskComplete`] and the runner-side oneshot the
/// [`SubstrateAgentDispatcher`] is holding.
///
/// Construct via [`SubstrateAgentDispatcher::completion_router`]. The CLI
/// (or any other substrate caller) is expected to subscribe to an event
/// bus and call [`Self::on_completion`] for every received event — the
/// router filters and ignores non-completion events itself.
#[derive(Clone)]
pub struct SubstrateCompletionRouter {
    dispatcher: Arc<SubstrateAgentDispatcher>,
}

impl SubstrateCompletionRouter {
    /// Feed an event into the router. No-ops on non-completion events.
    ///
    /// On [`Event::AgentTaskComplete`] with `success: true`, the router
    /// fulfils the matching oneshot with the event's `result` payload
    /// (or `null` if the substrate did not provide one). On
    /// `success: false`, the oneshot is fulfilled with
    /// [`DispatchError::Execution`] carrying the substrate's `summary`.
    ///
    /// Events whose `agent_id` does not match any pending dispatch are
    /// dropped silently (they are completions for non-loop agents).
    pub fn on_completion(&self, event: Event) {
        let Event::AgentTaskComplete {
            agent_id,
            success,
            summary,
            spawn_tag: _,
            result,
        } = event
        else {
            return;
        };

        let Some(sender) = self.dispatcher.take_pending(agent_id) else {
            tracing::trace!(
                agent_id,
                "SubstrateCompletionRouter: no pending entry for completion event; dropping"
            );
            return;
        };

        let outcome = if success {
            Ok(result.unwrap_or(Value::Null))
        } else {
            Err(DispatchError::Execution(summary))
        };
        // Receiver may have been dropped if the runner gave up — that's
        // not a programmer error, so we silently swallow the send error.
        let _ = sender.send(outcome);
    }

    /// Direct fulfilment hook for tests that don't have a real
    /// [`Event`] handy. Mirrors [`Self::on_completion`] but takes the
    /// constituent fields without constructing the protocol event.
    pub fn complete_for_test(
        &self,
        agent_id: AgentId,
        outcome: Result<Value, DispatchError>,
    ) {
        if let Some(sender) = self.dispatcher.take_pending(agent_id) {
            let _ = sender.send(outcome);
        }
    }

    /// Count of pending dispatches the router can still resolve. For tests
    /// and observability — never a basis for production branching.
    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.dispatcher.pending_count()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::source::CorrelationId;
    use crate::spec::LoopPolicy;
    use phantom_agents::composer_tools::new_spawn_subagent_queue;
    use serde_json::json;

    fn make_spec() -> LoopAgentSpec {
        LoopAgentSpec {
            role: phantom_agents::role::AgentRole::Actor,
            allow_tools: None,
            system_prompt: "Do the thing.".to_string(),
            exit_schema: json!({"type": "object"}),
            policy: LoopPolicy::default(),
        }
    }

    fn make_input() -> LoopInput {
        LoopInput {
            key: "test-key".to_string(),
            payload: json!({"hello": "world"}),
            correlation_id: CorrelationId::new("corr-1"),
        }
    }

    #[tokio::test]
    async fn dispatch_pushes_request_onto_spawn_queue() {
        let q = new_spawn_subagent_queue();
        let d = SubstrateAgentDispatcher::with_default_parent(q.clone());
        let _handle = d.dispatch(&make_spec(), &make_input()).expect("dispatch");

        let queued = q.lock().unwrap();
        assert_eq!(queued.len(), 1);
        let req = queued.front().unwrap();
        assert_eq!(req.role, phantom_agents::role::AgentRole::Actor);
        assert!(req.task.contains("Do the thing."));
        assert!(req.task.contains("test-key"));
    }

    #[tokio::test]
    async fn router_fulfils_pending_on_success_complete() {
        let q = new_spawn_subagent_queue();
        let d = Arc::new(SubstrateAgentDispatcher::with_default_parent(q));
        let router = d.completion_router();

        let handle = d.dispatch(&make_spec(), &make_input()).expect("dispatch");
        let assigned_id = handle.agent_id;

        router.on_completion(Event::AgentTaskComplete {
            agent_id: assigned_id,
            success: true,
            summary: "ok".to_string(),
            spawn_tag: None,
            result: Some(json!({"ok": true})),
        });

        let res = handle.completion_rx.await.expect("oneshot must resolve");
        let payload = res.expect("must be Ok");
        assert_eq!(payload, json!({"ok": true}));
        assert_eq!(d.pending_count(), 0);
    }

    #[tokio::test]
    async fn router_fulfils_pending_on_failure_complete() {
        let q = new_spawn_subagent_queue();
        let d = Arc::new(SubstrateAgentDispatcher::with_default_parent(q));
        let router = d.completion_router();

        let handle = d.dispatch(&make_spec(), &make_input()).expect("dispatch");
        let assigned_id = handle.agent_id;

        router.on_completion(Event::AgentTaskComplete {
            agent_id: assigned_id,
            success: false,
            summary: "boom".to_string(),
            spawn_tag: None,
            result: None,
        });

        let res = handle.completion_rx.await.expect("oneshot resolves");
        match res {
            Err(DispatchError::Execution(msg)) => assert_eq!(msg, "boom"),
            other => panic!("expected Execution error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn router_drops_completion_for_unknown_agent_id() {
        let q = new_spawn_subagent_queue();
        let d = Arc::new(SubstrateAgentDispatcher::with_default_parent(q));
        let router = d.completion_router();

        // No dispatch — no pending entry.
        router.on_completion(Event::AgentTaskComplete {
            agent_id: 9_999_999,
            success: true,
            summary: "ok".to_string(),
            spawn_tag: None,
            result: None,
        });
        // Just verifying no panic — and pending count stays at 0.
        assert_eq!(d.pending_count(), 0);
    }

    #[tokio::test]
    async fn router_ignores_non_completion_events() {
        let q = new_spawn_subagent_queue();
        let d = Arc::new(SubstrateAgentDispatcher::with_default_parent(q));
        let router = d.completion_router();

        // Register a pending dispatch so we can verify it stays unresolved.
        let _handle = d.dispatch(&make_spec(), &make_input()).expect("dispatch");
        assert_eq!(d.pending_count(), 1);

        router.on_completion(Event::AgentSpawned {
            agent_id: 1,
            task: "x".to_string(),
        });
        router.on_completion(Event::AgentProgress {
            agent_id: 1,
            fraction: 0.5,
            message: "thinking".to_string(),
        });

        // Pending must still be 1 — neither event matched.
        assert_eq!(d.pending_count(), 1);
    }

    #[tokio::test]
    async fn dispatch_allocates_monotonic_unique_ids() {
        let q = new_spawn_subagent_queue();
        let d = SubstrateAgentDispatcher::with_default_parent(q);
        let h1 = d.dispatch(&make_spec(), &make_input()).expect("dispatch 1");
        let h2 = d.dispatch(&make_spec(), &make_input()).expect("dispatch 2");
        assert!(h2.agent_id > h1.agent_id);
        assert!(h1.agent_id >= SUBSTRATE_DISPATCHER_ID_BASE);
    }
}
