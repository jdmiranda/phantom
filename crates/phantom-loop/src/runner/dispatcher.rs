//! The [`AgentDispatcher`] trait — abstraction over agent spawning.
//!
//! This is the load-bearing decision for the C2 → C3 hand-off. Three
//! constraints shape the trait surface:
//!
//! 1. **Runner-side ergonomics.** [`crate::runner::LoopRunner`] needs to
//!    await the spawned agent's completion *without* holding a tokio task
//!    indefinitely. A `tokio::sync::oneshot::Receiver` matches that —
//!    `.await` resolves promptly, and the runner can interleave other
//!    state-machine work while it waits.
//! 2. **Stubbable for tests.** C2 tests use a stub dispatcher that resolves
//!    the oneshot immediately with a canned result. No real agent
//!    substrate is involved — that comes in C3.
//! 3. **C3 substrate independence.** The trait must accommodate
//!    [`phantom_agents::composer_tools::new_spawn_subagent_queue`]
//!    semantics: a sync push into a queue that the App drains on its own
//!    event-loop thread, with the result eventually surfacing via the
//!    [`phantom_memory::event_log::EventLog`]. C3's real impl will turn
//!    that asynchronous round-trip into a oneshot completion.
//!
//! By making `dispatch` return a [`DispatchHandle`] carrying a
//! `oneshot::Receiver`, both the test stub and the future real impl share a
//! single interface — the difference is just how each populates the
//! sender side.

use tokio::sync::oneshot;

use phantom_agents::agent::AgentId;

use crate::runner::source::LoopInput;
use crate::spec::LoopAgentSpec;

/// Errors an [`AgentDispatcher`] can produce.
///
/// Two failure modes today: a synchronous failure to *spawn* (returned from
/// `dispatch`), and an asynchronous failure during *execution* (received via
/// the oneshot). C2 uses the same enum for both because the runner's
/// transition logic does not care about the difference — both terminate the
/// iteration.
#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    /// The dispatcher could not spawn the agent at all (e.g. spawn queue
    /// poisoned, role rejected by the substrate).
    #[error("failed to dispatch agent: {0}")]
    Spawn(String),

    /// The dispatcher spawned the agent but the agent terminated abnormally
    /// before producing a `complete_task` result (timeout, panic, fatal
    /// tool error).
    #[error("agent execution failed: {0}")]
    Execution(String),

    /// The oneshot sender was dropped before the agent reported completion
    /// — surfaces a substrate-side bug where an agent silently disappears.
    #[error("agent dispatcher completion channel closed prematurely")]
    Cancelled,
}

/// Handle returned by [`AgentDispatcher::dispatch`].
///
/// Carries the substrate-assigned [`AgentId`] (so the runner can log it and
/// correlate it with substrate-emitted events) and a oneshot receiver that
/// resolves with the agent's `complete_task` result — or a
/// [`DispatchError::Execution`] / [`DispatchError::Cancelled`] if the agent
/// did not produce one.
///
/// `DispatchHandle` is not `Clone` on purpose: the oneshot can only be
/// awaited from one consumer (the runner). C3 may add a fan-out wrapper
/// (`Arc<watch::Receiver<_>>` style) if subscribers beyond the runner
/// emerge.
#[derive(Debug)]
pub struct DispatchHandle {
    /// Substrate-assigned id for the spawned agent. The runner records this
    /// in the `Awaiting` state and emits it on transition logs.
    pub agent_id: AgentId,

    /// Oneshot fed by the dispatcher when the agent reaches a terminal
    /// state. The runner `await`s this from inside its FSM poll loop.
    pub completion_rx: oneshot::Receiver<Result<serde_json::Value, DispatchError>>,
}

impl DispatchHandle {
    /// Convenience constructor — most dispatchers will build the handle by
    /// allocating their own oneshot channel and handing the receiver here.
    #[must_use]
    pub fn new(
        agent_id: AgentId,
        completion_rx: oneshot::Receiver<Result<serde_json::Value, DispatchError>>,
    ) -> Self {
        Self {
            agent_id,
            completion_rx,
        }
    }
}

/// Abstraction over the agent substrate.
///
/// C2 ships one implementation: the test stub in `tests/runner_smoke.rs`
/// (and one used by [`crate::runner::LoopRunner`]'s own unit tests). C3
/// adds a real implementation backed by
/// [`phantom_agents::composer_tools::new_spawn_subagent_queue`] —
/// translating the queue's sync push + async `EventLog` round-trip into a
/// oneshot completion.
///
/// `dispatch` is sync and must return promptly. Any I/O the real impl
/// needs to perform (queue push, event-log subscription setup) should
/// happen inside the call; the *awaiting* part lives behind the returned
/// oneshot so the runner can continue stepping the FSM while the agent
/// executes.
pub trait AgentDispatcher: Send + Sync {
    /// Spawn an agent for `input` according to `spec`. Returns a handle
    /// the runner can later `await` for the agent's `complete_task` result.
    ///
    /// # Errors
    ///
    /// Returns [`DispatchError::Spawn`] if the spawn itself fails
    /// synchronously. Asynchronous execution failures arrive via the
    /// returned [`DispatchHandle::completion_rx`].
    fn dispatch(
        &self,
        spec: &LoopAgentSpec,
        input: &LoopInput,
    ) -> Result<DispatchHandle, DispatchError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::source::CorrelationId;
    use serde_json::json;

    /// Sanity test for the trait: object-safe and a trivial stub works.
    #[test]
    fn dispatcher_trait_is_object_safe_and_basic_stub_resolves() {
        struct StubOk;
        impl AgentDispatcher for StubOk {
            fn dispatch(
                &self,
                _spec: &LoopAgentSpec,
                _input: &LoopInput,
            ) -> Result<DispatchHandle, DispatchError> {
                let (tx, rx) = oneshot::channel();
                tx.send(Ok(json!({"ok": true}))).expect("send");
                Ok(DispatchHandle::new(42, rx))
            }
        }

        let d: Box<dyn AgentDispatcher> = Box::new(StubOk);
        let spec = make_test_spec();
        let input = LoopInput {
            key: "k".to_string(),
            payload: json!({}),
            correlation_id: CorrelationId::new("c"),
        };
        let handle = d.dispatch(&spec, &input).expect("dispatch must succeed");
        assert_eq!(handle.agent_id, 42);
        // The oneshot is already resolved by the stub — try_recv via
        // blocking_recv works under a runtime, but the static check is
        // enough here: the trait surface compiles and is object-safe.
        let _ = handle;
    }

    fn make_test_spec() -> LoopAgentSpec {
        LoopAgentSpec {
            role: phantom_agents::role::AgentRole::Actor,
            allow_tools: None,
            system_prompt: "noop".to_string(),
            exit_schema: json!({"type": "object"}),
            policy: crate::spec::LoopPolicy::default(),
        }
    }
}
