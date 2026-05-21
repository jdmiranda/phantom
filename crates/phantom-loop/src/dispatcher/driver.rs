//! Headless substrate driver for `phantom loop run`.
//!
//! The full `phantom-app::App` event loop drains
//! [`phantom_agents::composer_tools::SpawnSubagentQueue`] every frame and
//! materialises each [`SpawnSubagentRequest`] into a real agent pane backed
//! by a Claude API call. That path requires a winit window, a wgpu surface,
//! a layout engine, and a scene graph — all of which `phantom loop run`
//! deliberately avoids.
//!
//! This module ships the CLI-side counterpart: a [`SubstrateDriver`] that
//! drains the same queue from an async task, drives the agent through a
//! pluggable [`SubstrateBackend`] (real Claude API by default, mockable for
//! tests), and emits a `phantom_protocol::Event::AgentTaskComplete` onto a
//! tokio mpsc bus when each agent finishes. The
//! [`crate::SubstrateCompletionRouter`] is then fed from the other end of
//! that bus, closing the loop with the [`crate::SubstrateAgentDispatcher`].
//!
//! # Topology
//!
//! ```text
//!   LoopRunner.dispatch
//!         │  (1) push SpawnSubagentRequest onto SpawnSubagentQueue
//!         ▼
//!   SubstrateAgentDispatcher        ◄───────────────────────────────┐
//!         │                                                         │
//!         │  (2) registers pending oneshot for AgentId              │
//!         ▼                                                         │
//!   ┌─────────────────────────────────────────────────────────┐     │
//!   │ SubstrateDriver.tick()                                  │     │
//!   │   for each pending SpawnSubagentRequest:                │     │
//!   │     spawn task → SubstrateBackend.run_agent(req)        │     │
//!   │     on completion: send Event::AgentTaskComplete onto   │     │
//!   │       the driver's event sender                         │     │
//!   └─────────────────────────────────────────────────────────┘     │
//!                                          │                        │
//!                                          ▼                        │
//!   ┌─────────────────────────────────────────────────────────┐     │
//!   │ Forwarder task: read Event from rx, call                │     │
//!   │ router.on_completion(event) which fulfils the oneshot   │─────┘
//!   └─────────────────────────────────────────────────────────┘
//! ```
//!
//! # Why a separate trait
//!
//! Mocking a `phantom-agents` `ChatBackend` end-to-end across the agent
//! loop (system prompt, multi-turn tool calls, JSON validation, completion
//! emission) is brittle. [`SubstrateBackend`] is a one-method abstraction
//! over "drive a [`SpawnSubagentRequest`] to a terminal result". The
//! default impl in this module wraps the real Claude / OpenAI API. Tests
//! plug in [`MockSubstrateBackend`] to return a canned `complete_task`
//! result without touching the network.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::mpsc;
use tokio::time::interval;

use phantom_agents::agent::{Agent, AgentMessage, AgentTask, allocate_agent_id};
use phantom_agents::api::{ApiEvent, ClaudeConfig, send_message};
use phantom_agents::chat::{
    ChatBackend, ChatError, ChatModel, ChatRequest, build_backend_with_privacy,
};
use phantom_agents::composer_tools::{SpawnSubagentQueue, SpawnSubagentRequest};
use phantom_agents::tools::{
    ToolDefinition, available_tools, execute_tool_with_provenance, lifecycle_tools,
};
use phantom_protocol::Event;

// ---------------------------------------------------------------------------
// SubstrateBackend
// ---------------------------------------------------------------------------

/// Pluggable agent runner.
///
/// The driver does not know how to talk to Claude — it only knows how to
/// drain the queue and emit completion events. The mapping from one
/// [`SpawnSubagentRequest`] to one terminal result lives behind this trait.
///
/// Implementations may be sync or async internally. Each invocation is
/// expected to return promptly; the actual long-running agent execution
/// runs inside the spawned tokio task the driver creates.
pub trait SubstrateBackend: Send + Sync {
    /// Drive `req` to completion and return the agent's `complete_task`
    /// payload (or an error describing why the agent failed).
    ///
    /// This call is made from inside a tokio task spawned by
    /// [`SubstrateDriver::tick`]. The implementation may block — the
    /// caller is on a dedicated task and a blocking call is the cheapest
    /// path for the synchronous `ureq`-backed [`send_message`] path.
    fn run_agent(&self, req: &SpawnSubagentRequest) -> Result<Value, String>;
}

// ---------------------------------------------------------------------------
// ChatBackedSubstrateBackend — the production impl
// ---------------------------------------------------------------------------

/// Production [`SubstrateBackend`] backed by the real `phantom-agents` chat
/// stack.
///
/// Drives one agent's full conversation: build the [`Agent`], pump
/// [`ApiEvent`]s from the chat backend, execute tool calls, loop until the
/// agent emits `complete_task` or hits the round budget.
///
/// `MAX_ROUNDS` mirrors the GUI pane's `MAX_TOOL_ROUNDS` so loop agents do
/// not differ from interactive panes on the round budget. `MAX_TOKENS`
/// matches the default per-request budget downstream of `ClaudeConfig`.
///
/// The working directory is the process cwd; loop agents are expected to
/// have file tools sandboxed to the repo root, which `phantom loop run`
/// canonicalises ahead of time.
pub struct ChatBackedSubstrateBackend {
    privacy_mode: bool,
    max_rounds: usize,
}

/// Maximum tool-call rounds before a loop agent flatlines with
/// `"iteration limit"`. Mirrors the GUI's `MAX_TOOL_ROUNDS`.
pub const DEFAULT_MAX_ROUNDS: usize = 32;

impl Default for ChatBackedSubstrateBackend {
    fn default() -> Self {
        Self {
            privacy_mode: false,
            max_rounds: DEFAULT_MAX_ROUNDS,
        }
    }
}

impl ChatBackedSubstrateBackend {
    /// Construct with explicit settings.
    #[must_use]
    pub fn new(privacy_mode: bool, max_rounds: usize) -> Self {
        Self {
            privacy_mode,
            max_rounds,
        }
    }

    /// Drive the agent loop synchronously. Returns the
    /// `complete_task(result)` payload on success.
    fn drive_agent(&self, req: &SpawnSubagentRequest) -> Result<Value, String> {
        let chat_model = req.chat_model.clone().unwrap_or_default();

        // Resolve the API key for the requested backend. ClaudeConfig is
        // also used by the legacy `send_message` path when no chat backend
        // is configured, so we always need a config to thread through.
        let Some(claude_config) = resolve_api_config(&chat_model) else {
            return Err(format!(
                "no API key configured for {}",
                chat_model.backend_name()
            ));
        };

        // Build the chat backend. PrivacyGuard wraps every cloud backend
        // even though privacy_mode is typically off in CLI mode.
        let chat_backend: Option<Box<dyn ChatBackend>> =
            match build_backend_with_privacy(&chat_model, self.privacy_mode) {
                Ok(b) => Some(b),
                Err(ChatError::NotConfigured(msg)) => {
                    return Err(format!("chat backend not configured: {msg}"));
                }
                Err(e) => return Err(format!("chat backend init failed: {e}")),
            };

        // Build the Agent. Loop agents always opt into the complete_task
        // contract so the runner can validate the exit payload against the
        // configured ExitSchema.
        let mut agent = Agent::new(
            req.assigned_id.max(allocate_agent_id()),
            AgentTask::FreeForm {
                prompt: req.task.clone(),
            },
        );
        agent.set_requires_complete_task(true);

        let sys = agent.system_prompt();
        agent.push_message(AgentMessage::System(sys));
        agent.push_message(AgentMessage::User(req.task.clone()));

        let mut tools = available_tools();
        tools.extend(lifecycle_tools());

        let working_dir = std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| ".".into());

        let mut tool_use_ids: Vec<String> = Vec::new();

        for round in 0..self.max_rounds {
            tracing::debug!(
                agent_id = req.assigned_id,
                round,
                "loop substrate driver: starting round"
            );

            let mut handle = dispatch_one_turn(
                chat_backend.as_deref(),
                &claude_config,
                &agent,
                &tools,
                &tool_use_ids,
            );

            // Pump events from this turn until Done / Error / CompleteTask.
            let mut pending_calls: Vec<(String, phantom_agents::tools::ToolCall)> = Vec::new();
            let mut current_assistant = String::new();

            loop {
                match handle.try_recv() {
                    Some(ApiEvent::TextDelta(text)) => {
                        current_assistant.push_str(&text);
                    }
                    Some(ApiEvent::ToolUse { id, call }) => {
                        tool_use_ids.push(id.clone());
                        pending_calls.push((id, call));
                    }
                    Some(ApiEvent::CompleteTask { id: _, result }) => {
                        if !current_assistant.is_empty() {
                            agent.push_message(AgentMessage::Assistant(current_assistant));
                        }
                        if !result.is_object() {
                            return Err(format!(
                                "complete_task: result must be a JSON object (got {result})"
                            ));
                        }
                        agent.complete_with_result(result.clone());
                        return Ok(result);
                    }
                    Some(ApiEvent::Done) => {
                        if !current_assistant.is_empty() {
                            agent
                                .push_message(AgentMessage::Assistant(std::mem::take(
                                    &mut current_assistant,
                                )));
                        }
                        break;
                    }
                    Some(ApiEvent::Error(e)) => {
                        return Err(format!("chat backend error: {e}"));
                    }
                    None => {
                        if handle.is_done() {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(25));
                    }
                }
            }

            if pending_calls.is_empty() {
                // Conversation ended without complete_task. Treat as
                // failure so the runner records the iteration as failed.
                return Err("agent exited without complete_task".to_string());
            }

            // Execute pending tools, append results, loop back for the
            // next turn.
            for (_id, call) in pending_calls.drain(..) {
                agent.push_message(AgentMessage::ToolCall(call.clone()));
                let result = execute_tool_with_provenance(
                    call.tool,
                    &call.args,
                    &working_dir,
                    &req.role,
                    None,
                );
                agent.push_message(AgentMessage::ToolResult(result));
            }
        }

        Err(format!(
            "iteration limit reached ({} tool rounds)",
            self.max_rounds
        ))
    }
}

impl SubstrateBackend for ChatBackedSubstrateBackend {
    fn run_agent(&self, req: &SpawnSubagentRequest) -> Result<Value, String> {
        self.drive_agent(req)
    }
}

/// Resolve the appropriate API config for the requested chat model.
///
/// Returns `None` when the required env var is unset. Mirrors the
/// `phantom-app::agent_pane::resolve_api_config` private helper without
/// pulling phantom-app into this crate's dependency graph.
fn resolve_api_config(model: &ChatModel) -> Option<ClaudeConfig> {
    match model {
        ChatModel::Claude(_) => ClaudeConfig::from_env(),
        ChatModel::OpenAi(_) => {
            let key = std::env::var("OPENAI_API_KEY").ok().filter(|k| !k.is_empty());
            // OpenAI keys are consumed inside the ChatBackend, but the
            // legacy `send_message` path still wants a ClaudeConfig for
            // its `max_tokens` field. Mirror phantom-app and synthesise a
            // placeholder.
            key.map(|_| ClaudeConfig::new("__openai__"))
        }
    }
}

/// Dispatch one turn of the chat conversation, mirroring the GUI pane's
/// dispatch path. Routes through [`ChatBackend::complete`] when a backend
/// is configured, otherwise falls through to the legacy [`send_message`]
/// Claude path so behaviour stays byte-for-byte identical when no
/// `--model` was selected.
fn dispatch_one_turn(
    backend: Option<&dyn ChatBackend>,
    claude_config: &ClaudeConfig,
    agent: &Agent,
    tools: &[ToolDefinition],
    tool_use_ids: &[String],
) -> phantom_agents::api::ApiHandle {
    if let Some(backend) = backend {
        let request = ChatRequest {
            agent,
            tools,
            tool_use_ids,
            max_tokens: claude_config.max_tokens,
        };
        match backend.complete(request) {
            Ok(response) => response.into_handle(),
            Err(e) => {
                let (tx, rx) = std::sync::mpsc::channel();
                let _ = tx.send(ApiEvent::Error(format!(
                    "chat backend ({}) error: {e}",
                    backend.name()
                )));
                phantom_agents::api::ApiHandle::from_receiver(rx)
            }
        }
    } else {
        send_message(claude_config, agent, tools, tool_use_ids)
    }
}

// ---------------------------------------------------------------------------
// SubstrateDriver
// ---------------------------------------------------------------------------

/// Default queue drain cadence. The substrate is push-only on the
/// dispatcher side, so an aggressive poll keeps newly-dispatched agents
/// from sitting in the queue longer than necessary.
pub const DEFAULT_TICK_INTERVAL: Duration = Duration::from_millis(100);

/// The headless queue drainer.
///
/// Owns the same [`SpawnSubagentQueue`] the
/// [`crate::SubstrateAgentDispatcher`] writes to. Each tick pops every
/// pending request and spawns one tokio task per request. The task drives
/// the request through the configured [`SubstrateBackend`] and emits an
/// [`Event::AgentTaskComplete`] on the driver's event sender when done.
///
/// Construct with [`Self::new`]; spawn the periodic tick loop with
/// [`Self::run`].
pub struct SubstrateDriver {
    spawn_queue: SpawnSubagentQueue,
    backend: Arc<dyn SubstrateBackend>,
    event_tx: mpsc::Sender<Event>,
    tick_interval: Duration,
}

impl SubstrateDriver {
    /// Build a driver. The `event_tx` is the bus the
    /// [`crate::SubstrateCompletionRouter`] is expected to subscribe to —
    /// typically the CLI sets up a forwarder task that reads each event
    /// and calls `router.on_completion(event)`.
    #[must_use]
    pub fn new(
        spawn_queue: SpawnSubagentQueue,
        backend: Arc<dyn SubstrateBackend>,
        event_tx: mpsc::Sender<Event>,
    ) -> Self {
        Self {
            spawn_queue,
            backend,
            event_tx,
            tick_interval: DEFAULT_TICK_INTERVAL,
        }
    }

    /// Override the queue-drain interval.
    #[must_use]
    pub fn with_tick_interval(mut self, interval: Duration) -> Self {
        self.tick_interval = interval;
        self
    }

    /// Spawn the periodic drain loop. The returned [`tokio::task::JoinHandle`]
    /// can be aborted on Ctrl-C to stop the driver.
    #[must_use]
    pub fn run(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(self.run_loop())
    }

    async fn run_loop(self) {
        let mut ticker = interval(self.tick_interval);
        // `interval` fires immediately for the first tick, which causes
        // a no-op drain (queue is empty at boot). Skip it so the first
        // actual drain happens after the first interval elapses.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            self.tick();
        }
    }

    /// One drain cycle. Public so tests can step the driver synchronously
    /// rather than racing against an `interval()` clock.
    pub fn tick(&self) {
        let pending: Vec<SpawnSubagentRequest> = match self.spawn_queue.lock() {
            Ok(mut q) => q.drain(..).collect(),
            Err(_) => {
                tracing::warn!("substrate driver: spawn queue mutex poisoned");
                Vec::new()
            }
        };

        for req in pending {
            let backend = Arc::clone(&self.backend);
            let tx = self.event_tx.clone();
            tokio::spawn(async move {
                let agent_id = req.assigned_id;
                tracing::debug!(agent_id, label = %req.label, "substrate driver: running agent");

                // Drive the backend on a blocking thread so blocking
                // network calls (`send_message`'s `ureq` path) do not
                // park a tokio worker.
                let outcome = tokio::task::spawn_blocking(move || backend.run_agent(&req))
                    .await
                    .unwrap_or_else(|e| Err(format!("backend task panicked: {e}")));

                let event = match outcome {
                    Ok(result) => Event::AgentTaskComplete {
                        agent_id,
                        success: true,
                        summary: format!("agent #{agent_id} completed via substrate driver"),
                        spawn_tag: None,
                        result: Some(result),
                    },
                    Err(reason) => Event::AgentTaskComplete {
                        agent_id,
                        success: false,
                        summary: reason,
                        spawn_tag: None,
                        result: None,
                    },
                };

                if tx.send(event).await.is_err() {
                    tracing::trace!(
                        agent_id,
                        "substrate driver: event bus closed; dropping completion"
                    );
                }
            });
        }
    }
}

// ---------------------------------------------------------------------------
// MockSubstrateBackend — test helper
// ---------------------------------------------------------------------------

/// In-process [`SubstrateBackend`] that returns a canned outcome.
///
/// Used in the e2e smoke test to exercise the driver → dispatcher
/// completion path without hitting a Claude endpoint. The same `outcome`
/// is returned for every spawned request.
pub struct MockSubstrateBackend {
    outcome: std::sync::Mutex<Result<Value, String>>,
}

impl MockSubstrateBackend {
    /// Build a mock that returns `Ok(result)` on every spawn.
    #[must_use]
    pub fn ok(result: Value) -> Self {
        Self {
            outcome: std::sync::Mutex::new(Ok(result)),
        }
    }

    /// Build a mock that returns `Err(reason)` on every spawn.
    #[must_use]
    pub fn err(reason: impl Into<String>) -> Self {
        Self {
            outcome: std::sync::Mutex::new(Err(reason.into())),
        }
    }
}

impl SubstrateBackend for MockSubstrateBackend {
    fn run_agent(&self, _req: &SpawnSubagentRequest) -> Result<Value, String> {
        self.outcome
            .lock()
            .map(|g| g.clone())
            .unwrap_or_else(|_| Err("mock backend mutex poisoned".to_string()))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SubstrateAgentDispatcher;
    use crate::runner::dispatcher::AgentDispatcher;
    use crate::runner::source::{CorrelationId, LoopInput};
    use crate::spec::{LoopAgentSpec, LoopPolicy};
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
    async fn mock_backend_returns_canned_outcome() {
        let backend = MockSubstrateBackend::ok(json!({"answer": 42}));
        let req = SpawnSubagentRequest {
            assigned_id: 1,
            role: phantom_agents::role::AgentRole::Actor,
            label: "test".to_string(),
            task: "noop".to_string(),
            chat_model: None,
            parent: 0,
            handoff_context: None,
        };
        let outcome = backend.run_agent(&req).unwrap();
        assert_eq!(outcome, json!({"answer": 42}));
    }

    /// End-to-end driver test: a dispatcher pushes onto the queue, the
    /// driver drains and runs the mock backend, the event bus carries an
    /// AgentTaskComplete back, the completion router fulfils the
    /// pending oneshot.
    #[tokio::test]
    async fn driver_drains_queue_and_routes_completion_back_to_dispatcher() {
        let queue = new_spawn_subagent_queue();
        let dispatcher = Arc::new(SubstrateAgentDispatcher::with_default_parent(queue.clone()));
        let router = dispatcher.completion_router();

        let (tx, mut rx) = mpsc::channel::<Event>(8);
        let backend: Arc<dyn SubstrateBackend> = Arc::new(MockSubstrateBackend::ok(json!({
            "decision": "approved"
        })));
        let driver = SubstrateDriver::new(queue.clone(), backend, tx);

        // Forwarder task: pipe events into the router.
        let router_clone = router.clone();
        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                router_clone.on_completion(event);
            }
        });

        // Dispatch one iteration through the dispatcher (pushes onto queue).
        let handle = dispatcher
            .dispatch(&make_spec(), &make_input())
            .expect("dispatch must succeed");

        // Drive the driver once — it spawns a task that runs the mock
        // backend and emits onto the event bus.
        driver.tick();

        // Await completion. The forwarder routes the event into the
        // router, which fulfils the oneshot.
        let outcome = handle
            .completion_rx
            .await
            .expect("oneshot must resolve")
            .expect("dispatch must succeed");
        assert_eq!(outcome, json!({"decision": "approved"}));
        assert_eq!(dispatcher.pending_count(), 0);
    }

    #[tokio::test]
    async fn driver_routes_backend_error_as_dispatch_failure() {
        let queue = new_spawn_subagent_queue();
        let dispatcher = Arc::new(SubstrateAgentDispatcher::with_default_parent(queue.clone()));
        let router = dispatcher.completion_router();

        let (tx, mut rx) = mpsc::channel::<Event>(8);
        let backend: Arc<dyn SubstrateBackend> = Arc::new(MockSubstrateBackend::err("synthetic"));
        let driver = SubstrateDriver::new(queue.clone(), backend, tx);

        let router_clone = router.clone();
        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                router_clone.on_completion(event);
            }
        });

        let handle = dispatcher
            .dispatch(&make_spec(), &make_input())
            .expect("dispatch must succeed");
        driver.tick();

        let outcome = handle.completion_rx.await.expect("oneshot resolves");
        match outcome {
            Err(crate::DispatchError::Execution(msg)) => {
                assert!(msg.contains("synthetic"), "got `{msg}`");
            }
            other => panic!("expected Execution error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn driver_tick_no_panic_on_empty_queue() {
        let queue = new_spawn_subagent_queue();
        let (tx, _rx) = mpsc::channel::<Event>(8);
        let backend: Arc<dyn SubstrateBackend> = Arc::new(MockSubstrateBackend::ok(json!({})));
        let driver = SubstrateDriver::new(queue, backend, tx);
        // First tick on an empty queue must be a no-op.
        driver.tick();
    }
}
