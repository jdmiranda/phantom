//! Bridge between the brain's [`phantom_brain::dispatch::ActionHandler`] and
//! the loop crate's [`crate::queue::LoopQueueRegistry`].
//!
//! Closes the phantom-on-phantom self-improvement loop. The brain's
//! self-improvement reconciler emits
//! [`phantom_brain::events::AiAction::EnqueueLoopMessage`] every time a
//! GoalCandidate clears the score / trust / rate gates. The default
//! `ActionHandler::enqueue_loop_message` is a no-op so the action evaporates
//! before reaching any queue. This module ships
//! [`LoopQueueActionHandler`] — a production sink that owns an
//! `Arc<LoopQueueRegistry>` and converts each
//! `EnqueueLoopMessage(queue, from_source, payload)` into a
//! `registry.push(&queue, LoopMessage::new(from_source, payload))` call.
//!
//! The handler is intentionally focused on the enqueue path. Every other
//! `ActionHandler` method is delegated to a pluggable `inner` handler so the
//! same struct can be the brain's sole action sink in headless mode. The
//! default inner handler is [`NoopInner`], which mirrors the trait's default
//! no-op bodies — production CLI callers either supply their own inner sink
//! (when they want to log brain commentary) or accept the silent default.
//!
//! # Wiring
//!
//! ```text
//!   brain self-improvement tick
//!         │  emits AiAction::EnqueueLoopMessage
//!         ▼
//!   LoopQueueActionHandler::enqueue_loop_message
//!         │  registry.push(&queue, LoopMessage::new(from_source, payload))
//!         ▼
//!   LoopQueue (named "implementer-queue" by default)
//!         │  popped by LoopMessageQueueSource
//!         ▼
//!   LoopRunner::pull → Dispatching → SubstrateAgentDispatcher → SubstrateDriver
//!         │  spawns a real agent that opens a PR
//!         ▼
//!   Event::AgentTaskComplete back through SubstrateCompletionRouter
//! ```
//!
//! The CLI [`phantom loop run`] subcommand constructs both the brain and the
//! handler, then spins a small forwarder thread that drains
//! `BrainHandle::try_recv_action()` and calls `AiAction::execute(&mut
//! handler)`. The forwarder lives next to the substrate driver tick loop and
//! is aborted on Ctrl-C alongside it.

use std::sync::Arc;

use phantom_brain::dispatch::ActionHandler;
use phantom_brain::events::{ConnectionState, SuggestionOption};
use phantom_agents::peer_routing::RemoteMessageContent;
use phantom_agents::{AgentId, AgentTask};
use phantom_agents::agent::PauseReason;
use phantom_agents::dispatch::Disposition;

use crate::queue::{LoopMessage, LoopQueueRegistry};

// ---------------------------------------------------------------------------
// NoopInner — fallback for callers that only care about the enqueue path.
// ---------------------------------------------------------------------------

/// Inner handler that drops every non-enqueue action.
///
/// The brain emits many side-channel actions (proactive suggestions, console
/// replies, memory updates) that have no natural CLI sink. Callers who only
/// want to wire the enqueue path can leave [`LoopQueueActionHandler`]'s
/// `inner` as the default `NoopInner` — the headless brain still ticks, the
/// audit log still records, and only the noisy display-side actions are
/// dropped on the floor.
#[derive(Debug, Default)]
pub struct NoopInner;

impl ActionHandler for NoopInner {
    fn show_suggestion(&mut self, _text: String, _options: Vec<SuggestionOption>) {}
    fn show_notification(&mut self, _msg: String) {}
    fn update_memory(&mut self, _key: String, _value: String) {}
    fn spawn_agent(
        &mut self,
        _task: AgentTask,
        _spawn_tag: Option<u64>,
        _disposition: Disposition,
    ) {
    }
    fn console_reply(&mut self, _reply: String) {}
    fn run_command(&mut self, _cmd: String) {}
    fn dismiss_adapter(&mut self, _app_id: u32) {}
    fn agent_flatlined(&mut self, _id: AgentId, _reason: String) {}
    fn suggest(&mut self, _action: String, _rationale: String, _confidence: f32) {}
    fn quarantine_agent(&mut self, _agent_id: AgentId, _denial_count: usize) {}
    fn agent_quarantined(&mut self, _agent_id: AgentId, _denial_count: usize) {}
    fn checkpoint_reached(&mut self, _step_idx: usize, _description: String) {}
    fn pause_agent(&mut self, _agent_id: AgentId, _reason: PauseReason) {}
    fn resume_agent(&mut self, _agent_id: AgentId) {}
    fn update_connection_state(&mut self, _state: ConnectionState) {}
    fn set_offline_mode(&mut self, _enabled: bool) {}
    fn deliver_inbound_relay(&mut self, _agent_id: AgentId, _content: RemoteMessageContent) {}
    fn enqueue_loop_message(
        &mut self,
        _queue: String,
        _from_source: String,
        _payload: serde_json::Value,
    ) {
    }
}

// ---------------------------------------------------------------------------
// LoopQueueActionHandler — the bridge.
// ---------------------------------------------------------------------------

/// [`ActionHandler`] that forwards `AiAction::EnqueueLoopMessage` onto a
/// shared [`LoopQueueRegistry`].
///
/// Every other `ActionHandler` method is delegated to `inner`. The default
/// inner is [`NoopInner`]; supply your own implementation via
/// [`Self::with_inner`] if you want to forward (for example) console replies
/// to stderr or memory updates to a persistent store.
pub struct LoopQueueActionHandler<I: ActionHandler = NoopInner> {
    registry: Arc<LoopQueueRegistry>,
    inner: I,
}

impl LoopQueueActionHandler<NoopInner> {
    /// Construct a handler with the no-op inner — the most common case for
    /// `phantom loop run`.
    #[must_use]
    pub fn new(registry: Arc<LoopQueueRegistry>) -> Self {
        Self {
            registry,
            inner: NoopInner,
        }
    }
}

impl<I: ActionHandler> LoopQueueActionHandler<I> {
    /// Construct a handler with a custom inner. Non-enqueue actions are
    /// forwarded to `inner` so a caller that wants to log brain commentary
    /// can supply a printing handler without losing the enqueue path.
    pub fn with_inner(registry: Arc<LoopQueueRegistry>, inner: I) -> Self {
        Self { registry, inner }
    }

    /// Borrow the registry handle. Used by tests to assert what landed on
    /// the queue.
    #[must_use]
    pub fn registry(&self) -> &Arc<LoopQueueRegistry> {
        &self.registry
    }
}

impl<I: ActionHandler> ActionHandler for LoopQueueActionHandler<I> {
    // --- The enqueue path: the only method this struct cares about. ---
    fn enqueue_loop_message(
        &mut self,
        queue: String,
        from_source: String,
        payload: serde_json::Value,
    ) {
        tracing::info!(
            queue = %queue,
            from_source = %from_source,
            "brain enqueued loop message",
        );
        self.registry
            .push(&queue, LoopMessage::new(from_source, payload));
    }

    // --- Every other method delegates to inner. ---
    fn show_suggestion(&mut self, text: String, options: Vec<SuggestionOption>) {
        self.inner.show_suggestion(text, options);
    }
    fn show_notification(&mut self, msg: String) {
        self.inner.show_notification(msg);
    }
    fn update_memory(&mut self, key: String, value: String) {
        self.inner.update_memory(key, value);
    }
    fn spawn_agent(
        &mut self,
        task: AgentTask,
        spawn_tag: Option<u64>,
        disposition: Disposition,
    ) {
        self.inner.spawn_agent(task, spawn_tag, disposition);
    }
    fn console_reply(&mut self, reply: String) {
        self.inner.console_reply(reply);
    }
    fn run_command(&mut self, cmd: String) {
        self.inner.run_command(cmd);
    }
    fn dismiss_adapter(&mut self, app_id: u32) {
        self.inner.dismiss_adapter(app_id);
    }
    fn agent_flatlined(&mut self, id: AgentId, reason: String) {
        self.inner.agent_flatlined(id, reason);
    }
    fn suggest(&mut self, action: String, rationale: String, confidence: f32) {
        self.inner.suggest(action, rationale, confidence);
    }
    fn quarantine_agent(&mut self, agent_id: AgentId, denial_count: usize) {
        self.inner.quarantine_agent(agent_id, denial_count);
    }
    fn agent_quarantined(&mut self, agent_id: AgentId, denial_count: usize) {
        self.inner.agent_quarantined(agent_id, denial_count);
    }
    fn checkpoint_reached(&mut self, step_idx: usize, description: String) {
        self.inner.checkpoint_reached(step_idx, description);
    }
    fn pause_agent(&mut self, agent_id: AgentId, reason: PauseReason) {
        self.inner.pause_agent(agent_id, reason);
    }
    fn resume_agent(&mut self, agent_id: AgentId) {
        self.inner.resume_agent(agent_id);
    }
    fn update_connection_state(&mut self, state: ConnectionState) {
        self.inner.update_connection_state(state);
    }
    fn set_offline_mode(&mut self, enabled: bool) {
        self.inner.set_offline_mode(enabled);
    }
    fn deliver_inbound_relay(&mut self, agent_id: AgentId, content: RemoteMessageContent) {
        self.inner.deliver_inbound_relay(agent_id, content);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use phantom_brain::events::AiAction;
    use serde_json::json;

    #[test]
    fn enqueue_pushes_one_message_to_the_named_queue() {
        let registry = Arc::new(LoopQueueRegistry::new());
        let mut handler = LoopQueueActionHandler::new(Arc::clone(&registry));

        let action = AiAction::EnqueueLoopMessage {
            queue: "implementer-queue".into(),
            from_source: "gh-issues".into(),
            payload: json!({"external_id": "gh-issue:123", "title": "fix the thing"}),
        };
        action.execute(&mut handler);

        let popped = registry.pop("implementer-queue").expect("queued message");
        assert_eq!(popped.from_loop, "gh-issues");
        assert_eq!(popped.payload["external_id"], "gh-issue:123");
    }

    #[test]
    fn enqueue_preserves_fifo_across_two_actions() {
        let registry = Arc::new(LoopQueueRegistry::new());
        let mut handler = LoopQueueActionHandler::new(Arc::clone(&registry));

        AiAction::EnqueueLoopMessage {
            queue: "q".into(),
            from_source: "src".into(),
            payload: json!({"i": 1}),
        }
        .execute(&mut handler);
        AiAction::EnqueueLoopMessage {
            queue: "q".into(),
            from_source: "src".into(),
            payload: json!({"i": 2}),
        }
        .execute(&mut handler);

        let a = registry.pop("q").unwrap();
        let b = registry.pop("q").unwrap();
        assert_eq!(a.payload["i"], 1);
        assert_eq!(b.payload["i"], 2);
    }

    #[test]
    fn other_actions_are_no_ops_with_default_inner() {
        let registry = Arc::new(LoopQueueRegistry::new());
        let mut handler = LoopQueueActionHandler::new(Arc::clone(&registry));
        // The handler must not panic on any of these; they all flow to NoopInner.
        AiAction::ShowNotification("hello".into()).execute(&mut handler);
        AiAction::ConsoleReply("reply".into()).execute(&mut handler);
        AiAction::DoNothing.execute(&mut handler);
        assert!(registry.pop("any").is_none(), "no-op variants do not enqueue");
    }

    #[test]
    fn custom_inner_receives_non_enqueue_actions() {
        struct Counting {
            notifications: std::sync::atomic::AtomicUsize,
        }
        impl ActionHandler for Counting {
            fn show_suggestion(&mut self, _text: String, _options: Vec<SuggestionOption>) {}
            fn show_notification(&mut self, _msg: String) {
                self.notifications
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            fn update_memory(&mut self, _key: String, _value: String) {}
            fn spawn_agent(
                &mut self,
                _task: AgentTask,
                _spawn_tag: Option<u64>,
                _disposition: Disposition,
            ) {
            }
            fn console_reply(&mut self, _reply: String) {}
            fn run_command(&mut self, _cmd: String) {}
            fn dismiss_adapter(&mut self, _app_id: u32) {}
            fn agent_flatlined(&mut self, _id: AgentId, _reason: String) {}
            fn suggest(&mut self, _action: String, _rationale: String, _confidence: f32) {}
            fn quarantine_agent(&mut self, _agent_id: AgentId, _denial_count: usize) {}
            fn agent_quarantined(&mut self, _agent_id: AgentId, _denial_count: usize) {}
        }
        let registry = Arc::new(LoopQueueRegistry::new());
        let inner = Counting {
            notifications: std::sync::atomic::AtomicUsize::new(0),
        };
        let mut handler = LoopQueueActionHandler::with_inner(registry, inner);
        AiAction::ShowNotification("a".into()).execute(&mut handler);
        AiAction::ShowNotification("b".into()).execute(&mut handler);
        assert_eq!(
            handler
                .inner
                .notifications
                .load(std::sync::atomic::Ordering::SeqCst),
            2
        );
    }
}
