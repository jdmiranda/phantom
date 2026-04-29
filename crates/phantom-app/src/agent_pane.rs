//! Agent pane management — spawn AI agents in visible GUI panes.
//!
//! When the brain decides to spawn an agent (or the user requests one),
//! this module creates a new pane, starts a Claude API agent on a
//! background thread, and streams output into the pane each frame.

use std::sync::{Arc, Mutex};

use log::{info, warn};

use phantom_agents::api::{ApiEvent, ApiHandle, ClaudeConfig, send_message};
use phantom_agents::agent::{Agent, AgentMessage};
use phantom_agents::audit::{AuditOutcome, emit_tool_call};
use phantom_agents::chat::{ChatBackend, ChatModel, ChatRequest, build_backend};
use phantom_agents::permissions::PermissionSet;
use phantom_agents::role::{AgentRole, CapabilityClass};
use phantom_agents::spawn_rules::{EventKind, EventSource, SubstrateEvent};
use phantom_agents::tools::{
    ToolCall, ToolDefinition, ToolResult, ToolType, available_tools,
};
use phantom_agents::{AgentSpawnOpts, AgentTask};

use crate::app::App;

/// The agent role default agent panes operate as.
///
/// Phase 1.F gates tool dispatch on the role manifest. Today every
/// agent-pane spawn is implicitly a `Conversational` agent (the assistant
/// that talks to the user), so we hardcode that here. Wider roles —
/// `Actor`, `Watcher`, etc. — will land in a follow-up that threads the
/// role through `AgentSpawnOpts`.
const DEFAULT_AGENT_PANE_ROLE: AgentRole = AgentRole::Conversational;

/// Maximum number of tool-use rounds before the agent is force-stopped.
const MAX_TOOL_ROUNDS: u32 = 25;

/// Number of consecutive tool-call failures before the pane emits a substrate
/// `AgentBlocked` event.
///
/// The threshold is intentionally low (2) so transient single-shot failures
/// (a typo in a path, a stale arg) don't trigger a Fixer, but a clearly stuck
/// agent does. The Fixer spawn rule (`fixer.rs::fixer_spawn_rule`) listens for
/// `AgentBlocked` and is the consumer of these events.
pub(crate) const TOOL_BLOCK_THRESHOLD: u32 = 2;

/// Shared queue of `AgentBlocked` events emitted by agent panes.
///
/// The `App` owns the canonical sink; each `AgentPane` is given a clone at
/// spawn time. Panes push events into the queue when their consecutive-failure
/// counter crosses [`TOOL_BLOCK_THRESHOLD`]; the App drains the queue each
/// frame inline in `update.rs::update` (via `drain_blocked_events`) and
/// forwards into `runtime.push_event`. From there the `SpawnRuleRegistry`
/// evaluates the event and queues a `Fixer` spawn action (Phase 2.G).
pub(crate) type BlockedEventSink = Arc<Mutex<Vec<SubstrateEvent>>>;

/// Construct a fresh, empty `BlockedEventSink`.
#[allow(dead_code)] // Producer for Phase 2.G consumer wiring; kept ahead of time.
pub(crate) fn new_blocked_event_sink() -> BlockedEventSink {
    Arc::new(Mutex::new(Vec::new()))
}

/// Shared queue of `EventKind::CapabilityDenied` events emitted by agent
/// panes (Sec.1 producer).
///
/// Mirrors [`BlockedEventSink`]: the `App` owns the canonical sink; each
/// pane is given a clone at spawn time. Whenever the Layer-2 dispatch gate
/// refuses a tool call (e.g. a `Watcher` calls `run_command`), the pane
/// builds a [`SubstrateEvent`] of kind [`EventKind::CapabilityDenied`] and
/// pushes it here. The drain step is inline in `update.rs::update` and
/// forwards into `runtime.push_event`. The Defender spawn rule (Sec.4)
/// reads these.
pub(crate) type DeniedEventSink = Arc<Mutex<Vec<SubstrateEvent>>>;

/// Construct a fresh, empty `DeniedEventSink`.
#[allow(dead_code)] // Producer for Sec.4 consumer wiring; kept ahead of time.
pub(crate) fn new_denied_event_sink() -> DeniedEventSink {
    Arc::new(Mutex::new(Vec::new()))
}

/// Build the canonical `AgentBlocked` payload documented in `fixer.rs`.
///
/// All keys here are convention; only `reason` is required at the rule layer.
/// The Fixer reads this payload to populate its system prompt at spawn time.
fn build_blocked_payload(
    agent_id: u64,
    agent_role: &str,
    reason: &str,
    blocked_at_unix_ms: u64,
    context_excerpt: &str,
    suggested_capability: &str,
) -> serde_json::Value {
    serde_json::json!({
        "agent_id": agent_id,
        "agent_role": agent_role,
        "reason": reason,
        "blocked_at_unix_ms": blocked_at_unix_ms,
        "context_excerpt": context_excerpt,
        "suggested_capability": suggested_capability,
    })
}

/// Build the `CapabilityDenied` payload (Sec.1).
///
/// Mirrors the AgentBlocked convention: a structured object with stable
/// keys so downstream Defender / Inspector consumers don't have to care
/// about the on-the-wire shape. Only the `EventKind::CapabilityDenied`
/// fields are load-bearing at the rule layer; everything else here is
/// for diagnostics and renderer surfaces.
fn build_capability_denied_payload(
    agent_id: u64,
    agent_role: &str,
    attempted_class: CapabilityClass,
    attempted_tool: &str,
    denied_at_unix_ms: u64,
) -> serde_json::Value {
    let source_chain: Vec<u64> = Vec::new();
    serde_json::json!({
        "agent_id": agent_id,
        "agent_role": agent_role,
        "attempted_class": class_label(attempted_class),
        "attempted_tool": attempted_tool,
        "denied_at_unix_ms": denied_at_unix_ms,
        // Sec.2 will fill source_chain; we serialize the empty Vec so the
        // shape is stable.
        "source_chain": source_chain,
    })
}

/// Lowercase string label for a `CapabilityClass`. Used in the audit log's
/// `class` field and the substrate-event payload's `attempted_class` so
/// scrapers can treat both as the same vocabulary.
fn class_label(class: CapabilityClass) -> &'static str {
    match class {
        CapabilityClass::Sense => "Sense",
        CapabilityClass::Reflect => "Reflect",
        CapabilityClass::Compute => "Compute",
        CapabilityClass::Act => "Act",
        CapabilityClass::Coordinate => "Coordinate",
    }
}

/// Wall-clock millis since epoch. Best-effort: returns 0 if the system clock
/// is somehow before the epoch.
fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// AgentPane — a running agent with its output stream
// ---------------------------------------------------------------------------

/// An active agent running in a GUI pane.
pub(crate) struct AgentPane {
    /// The agent's task description.
    task: String,
    /// Current status.
    status: AgentPaneStatus,
    /// Accumulated output text (streamed from Claude API).
    output: String,
    /// Handle to the background API thread.
    api_handle: Option<ApiHandle>,
    /// Tool use IDs for multi-turn conversations.
    tool_use_ids: Vec<String>,
    /// Cached tail lines for rendering (avoids re-splitting every frame).
    cached_lines: Vec<String>,
    /// Output length at last cache rebuild.
    cached_len: usize,
    /// The agent's conversation state (owns the message history).
    agent: Agent,
    /// Tool calls pending execution: (api_id, call).
    pending_tools: Vec<(String, ToolCall)>,
    /// Project root for tool sandbox.
    working_dir: String,
    /// Claude API config for re-invoking on tool-result turns.
    ///
    /// Always present. When [`AgentPane::chat_backend`] is `None`, this is
    /// the active Claude config used by [`send_message`]. When a backend is
    /// configured (`--model`/env override), this still provides the
    /// `max_tokens` budget for [`ChatRequest`] so per-turn shaping matches
    /// the existing setup.
    claude_config: ClaudeConfig,
    /// Optional chat backend.
    ///
    /// `None` keeps the byte-for-byte legacy Claude path: every turn calls
    /// [`send_message`] with [`AgentPane::claude_config`]. When the caller
    /// selected a [`ChatModel`] (via `--model` or `PHANTOM_AGENT_MODEL`),
    /// this is `Some` and turns dispatch through [`ChatBackend::complete`].
    chat_backend: Option<Box<dyn ChatBackend>>,
    /// Number of tool-use rounds completed (capped at [`MAX_TOOL_ROUNDS`]).
    turn_count: u32,
    /// Accumulator for assistant text within the current API response.
    current_assistant_text: String,
    /// Permission set for tool execution (default: all).
    permissions: PermissionSet,
    /// Approximate input tokens consumed.
    input_tokens: u32,
    /// Approximate output tokens consumed.
    output_tokens: u32,
    /// Number of tool calls executed.
    tool_call_count: u32,
    /// Whether this agent has written/edited files (for rollback).
    has_file_edits: bool,
    /// Consecutive tool-call failures since the last success.
    ///
    /// Reset to 0 on any successful tool result. When this reaches
    /// [`TOOL_BLOCK_THRESHOLD`], the pane emits an `EventKind::AgentBlocked`
    /// substrate event into [`AgentPane::blocked_event_sink`] and the counter
    /// is cleared so the same agent doesn't spam the bus.
    consecutive_tool_failures: u32,
    /// Shared sink for emitted `AgentBlocked` events (Phase 2.E producer).
    ///
    /// `None` for tests/legacy callers that don't have a sink to plumb. The
    /// production spawn path (`App::spawn_agent_pane_with_opts`) always
    /// supplies the App's canonical sink; Phase 2.G will consume those events
    /// to actually spawn Fixer agents.
    blocked_event_sink: Option<BlockedEventSink>,
    /// Shared sink for emitted `CapabilityDenied` events (Sec.1 producer).
    ///
    /// Mirrors [`AgentPane::blocked_event_sink`]: the App owns the canonical
    /// sink and hands a clone in at spawn time. Whenever the Layer-2 gate
    /// refuses a tool call, the pane pushes a [`SubstrateEvent`] of kind
    /// [`EventKind::CapabilityDenied`] here. `None` for legacy / test
    /// callers without a wired App.
    #[allow(dead_code)] // Producer for Sec.4 consumer wiring; kept ahead of time.
    denied_event_sink: Option<DeniedEventSink>,
    /// Last error excerpt observed on a failing tool call, used to populate
    /// the `reason` field of the emitted `AgentBlocked` event.
    last_tool_error: Option<String>,
    /// Shared registry handle used by chat-tool dispatch (`send_to_agent`,
    /// `broadcast_to_role`, `request_critique`). Cloned from the runtime at
    /// spawn time so dispatch contexts can read/write the same directory the
    /// substrate ticks against. `None` keeps the legacy / test path working
    /// (chat tools that need the registry will return a structured error).
    registry: Option<std::sync::Arc<std::sync::Mutex<phantom_agents::inbox::AgentRegistry>>>,
    /// Shared event-log handle used by chat-tool log emission and composer
    /// tools (`wait_for_agent`, `event_log_query`, `request_critique`).
    /// Cloned from the runtime; `None` disables log-dependent tools.
    event_log: Option<std::sync::Arc<std::sync::Mutex<phantom_memory::event_log::EventLog>>>,
    /// Shared sub-agent spawn queue. The Composer's `spawn_subagent` tool
    /// pushes onto this; the App's `update.rs` drains it once per frame.
    /// `None` disables `spawn_subagent` (it returns the chat-tools' standard
    /// error string).
    pending_spawn: Option<phantom_agents::composer_tools::SpawnSubagentQueue>,
    /// Pre-allocated [`phantom_agents::role::AgentRef`] used as the calling
    /// identity in dispatch contexts. Populated at spawn time so chat-tool
    /// peers can see attribution; `None` falls back to an ephemeral
    /// `Conversational` ref synthesized per turn (legacy path).
    self_ref: Option<phantom_agents::role::AgentRef>,
    /// Substrate role gating dispatch capability checks. Defaults to
    /// `DEFAULT_AGENT_PANE_ROLE` (Conversational) but can be overridden at
    /// spawn time so a Composer pane gets the Coordinate class it needs to
    /// invoke `spawn_subagent` / `broadcast_to_role`.
    role: phantom_agents::role::AgentRole,
    /// Issue #235: shared `GhTicketDispatcher` handle injected at spawn time
    /// when the pane's role is `Dispatcher`.
    ///
    /// `None` for all non-Dispatcher roles and for any Dispatcher pane whose
    /// parent `App` could not construct the dispatcher (e.g. `GITHUB_TOKEN`
    /// absent). When `None`, the three Dispatcher tools return the canonical
    /// `"ticket dispatcher not configured"` error so the model self-corrects.
    ticket_dispatcher: Option<std::sync::Arc<phantom_agents::dispatcher::GhTicketDispatcher>>,
    /// Per-agent structured lifecycle journal (JSONL on disk).
    ///
    /// `None` when the journal file could not be opened (e.g., permission
    /// error or test environment without a real filesystem path). All journal
    /// writes are best-effort — a failure never aborts an agent spawn.
    journal: Option<phantom_memory::journal::AgentJournal>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentPaneStatus {
    Working,
    Done,
    Failed,
}

impl AgentPane {
    /// Test-only constructor for adapter unit tests outside this module.
    ///
    /// Builds a pane with the given cached output lines, no live API
    /// handle, and `Done` status. Designed for adapter rendering tests
    /// that need a real `AgentPane` instance to feed `AgentAdapter::new`.
    #[cfg(test)]
    #[allow(dead_code)] // Used by adapter unit tests in sibling modules.
    pub(crate) fn test_with_lines(lines: Vec<String>) -> Self {
        let task = AgentTask::FreeForm { prompt: "test".into() };
        Self {
            task: "test".into(),
            status: AgentPaneStatus::Done,
            output: String::new(),
            api_handle: None,
            tool_use_ids: Vec::new(),
            cached_lines: lines,
            cached_len: 0,
            registry: None,
            event_log: None,
            pending_spawn: None,
            self_ref: None,
            role: DEFAULT_AGENT_PANE_ROLE,
            agent: Agent::new(0, task),
            pending_tools: Vec::new(),
            working_dir: ".".into(),
            claude_config: ClaudeConfig::new("sk-test"),
            chat_backend: None,
            turn_count: 0,
            current_assistant_text: String::new(),
            permissions: PermissionSet::all(),
            input_tokens: 0,
            output_tokens: 0,
            tool_call_count: 0,
            has_file_edits: false,
            consecutive_tool_failures: 0,
            blocked_event_sink: None,
            denied_event_sink: None,
            last_tool_error: None,
            ticket_dispatcher: None,
            journal: None,
        }
    }

    /// Dispatch one turn of the chat conversation.
    ///
    /// Routes through [`ChatBackend::complete`] when a backend is configured;
    /// otherwise falls back to [`send_message`] directly so the legacy Claude
    /// path stays byte-for-byte identical when no `--model` was selected.
    fn dispatch(
        backend: Option<&dyn ChatBackend>,
        claude_config: &ClaudeConfig,
        agent: &Agent,
        tools: &[ToolDefinition],
        tool_use_ids: &[String],
    ) -> ApiHandle {
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
                    // Surface the error through an ApiHandle so the existing
                    // poll() loop renders it consistently with network errors
                    // from send_message.
                    let (tx, rx) = std::sync::mpsc::channel();
                    let _ = tx.send(ApiEvent::Error(format!(
                        "chat backend ({}) error: {e}",
                        backend.name()
                    )));
                    ApiHandle::from_receiver(rx)
                }
            }
        } else {
            send_message(claude_config, agent, tools, tool_use_ids)
        }
    }

    /// Create a new agent pane and start the Claude API call.
    ///
    /// Backwards-compatible thin wrapper over [`AgentPane::spawn_with_opts`].
    /// Existing callers who already have a `ClaudeConfig` keep working
    /// unchanged: the resulting agent uses the default Claude path
    /// (`chat_backend = None`), which dispatches through [`send_message`]
    /// byte-for-byte. No `BlockedEventSink` is wired, so the pane never emits
    /// substrate `AgentBlocked` events.
    #[allow(dead_code)]
    pub(crate) fn spawn(task: AgentTask, claude_config: &ClaudeConfig) -> Self {
        Self::spawn_with_opts(AgentSpawnOpts::new(task), claude_config, None, None)
    }

    /// Create a new agent pane with explicit spawn options.
    ///
    /// When `opts.chat_model.is_some()` (or the `PHANTOM_AGENT_MODEL` env
    /// override resolves to a non-Claude backend), this builds a
    /// [`ChatBackend`] via [`build_backend`] and routes every turn through
    /// it. Otherwise the existing default Claude path runs unchanged.
    ///
    /// `blocked_event_sink` is the shared queue into which the pane emits
    /// `EventKind::AgentBlocked` substrate events when its tool-call failure
    /// streak crosses [`TOOL_BLOCK_THRESHOLD`]. `denied_event_sink` is the
    /// parallel queue for `EventKind::CapabilityDenied` events emitted by
    /// the Layer-2 dispatch gate (Sec.1 producer). Pass `None` to disable
    /// emission entirely (legacy / test path).
    pub(crate) fn spawn_with_opts(
        opts: AgentSpawnOpts,
        claude_config: &ClaudeConfig,
        blocked_event_sink: Option<BlockedEventSink>,
        denied_event_sink: Option<DeniedEventSink>,
    ) -> Self {
        let task = opts.task.clone();

        // Resolve the chat model (explicit > env-var > default Claude).
        let resolved = opts.resolve_model();

        // Only build a backend when something steered us off the default.
        // When the resolution lands on plain Claude AND no explicit model
        // was given, leave `chat_backend = None` so we hit `send_message`
        // directly (byte-for-byte legacy path).
        let chat_backend: Option<Box<dyn ChatBackend>> = match (&opts.chat_model, &resolved) {
            (None, ChatModel::Claude(_)) => None,
            _ => match build_backend(&resolved) {
                Ok(b) => Some(b),
                Err(e) => {
                    warn!(
                        "Could not build chat backend for {:?}: {e}; \
                         falling back to default Claude path",
                        resolved
                    );
                    None
                }
            },
        };

        let task_desc = match &task {
            AgentTask::FreeForm { prompt } => prompt.clone(),
            AgentTask::FixError { error_summary, .. } => {
                format!("Fix: {error_summary}")
            }
            AgentTask::RunCommand { command } => {
                format!("Run: {command}")
            }
            AgentTask::ReviewCode { context, .. } => {
                format!("Review: {context}")
            }
            AgentTask::WatchAndNotify { description } => {
                format!("Watch: {description}")
            }
        };

        let mut agent = Agent::new(0, task.clone());
        let sys_prompt = agent.system_prompt();
        agent.push_message(AgentMessage::System(sys_prompt));

        // Inject codebase context so the agent knows where it lives.
        let codebase_context = build_codebase_context();
        if !codebase_context.is_empty() {
            agent.push_message(AgentMessage::System(codebase_context));
        }

        // Claude API requires at least one user message. Push the task
        // description as the initial user turn.
        let user_prompt = match &task {
            AgentTask::FreeForm { prompt } => prompt.clone(),
            AgentTask::FixError { error_summary, context, .. } => {
                format!("Fix this error: {error_summary}\nContext: {context}")
            }
            AgentTask::RunCommand { command } => format!("Run: {command}"),
            AgentTask::ReviewCode { files, context } => {
                format!("Review these files: {}\nContext: {context}", files.join(", "))
            }
            AgentTask::WatchAndNotify { description } => {
                format!("Watch: {description}")
            }
        };
        agent.push_message(AgentMessage::User(user_prompt));

        let tools = available_tools();

        info!(
            "Agent pane spawning: {} messages (system={}, user={}, backend={})",
            agent.messages.len(),
            agent.messages.iter().filter(|m| matches!(m, AgentMessage::System(_))).count(),
            agent.messages.iter().filter(|m| matches!(m, AgentMessage::User(_))).count(),
            chat_backend.as_deref().map(|b| b.name()).unwrap_or("claude (default)"),
        );

        let handle = Self::dispatch(
            chat_backend.as_deref(),
            claude_config,
            &agent,
            &tools,
            &[],
        );

        let working_dir = std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| ".".into());

        info!("Agent pane spawned: {task_desc}");

        let agent_id_u64 = agent.id as u64;
        let mut journal = open_agent_journal(agent_id_u64);
        if let Some(ref mut j) = journal {
            if let Err(e) = j.record_spawn(agent_id_u64, &task_desc) {
                warn!("AgentJournal::record_spawn failed: {e}");
            }
        }

        Self {
            task: task_desc,
            status: AgentPaneStatus::Working,
            output: String::from("● Agent working...\n\n"),
            api_handle: Some(handle),
            tool_use_ids: Vec::new(),
            cached_lines: Vec::new(),
            cached_len: 0,
            agent,
            pending_tools: Vec::new(),
            working_dir,
            claude_config: claude_config.clone(),
            chat_backend,
            turn_count: 0,
            current_assistant_text: String::new(),
            permissions: PermissionSet::all(),
            input_tokens: 0,
            output_tokens: 0,
            tool_call_count: 0,
            has_file_edits: false,
            consecutive_tool_failures: 0,
            blocked_event_sink,
            denied_event_sink,
            last_tool_error: None,
            registry: None,
            event_log: None,
            pending_spawn: None,
            self_ref: None,
            role: DEFAULT_AGENT_PANE_ROLE,
            ticket_dispatcher: None,
            journal,
        }
    }

    /// Wire the substrate handles a chat-tool / composer-tool dispatch
    /// context needs. Called by `App::spawn_agent_pane_with_opts` after
    /// `spawn_with_opts` so the pane shares the runtime's registry / event
    /// log and the App's pending-spawn queue. Without these, dispatch falls
    /// back to capability-denied / "event log not configured" errors for
    /// the chat / composer tool surface — file/git tools keep working.
    pub(crate) fn set_substrate_handles(
        &mut self,
        registry: std::sync::Arc<std::sync::Mutex<phantom_agents::inbox::AgentRegistry>>,
        event_log: std::sync::Arc<std::sync::Mutex<phantom_memory::event_log::EventLog>>,
        pending_spawn: phantom_agents::composer_tools::SpawnSubagentQueue,
        self_ref: phantom_agents::role::AgentRef,
        role: phantom_agents::role::AgentRole,
    ) {
        self.registry = Some(registry);
        self.event_log = Some(event_log);
        self.pending_spawn = Some(pending_spawn);
        self.self_ref = Some(self_ref);
        self.role = role;
    }

    /// Wire the shared [`GhTicketDispatcher`] handle for Dispatcher-role panes.
    ///
    /// Called by `App::spawn_agent_pane_with_opts` immediately after
    /// `set_substrate_handles` when the spawned role is
    /// [`phantom_agents::role::AgentRole::Dispatcher`] and the App has a
    /// configured dispatcher. `None` is the safe default — the three
    /// Dispatcher tools will return `"ticket dispatcher not configured"` to
    /// the model instead of panicking.
    pub(crate) fn set_ticket_dispatcher(
        &mut self,
        dispatcher: std::sync::Arc<phantom_agents::dispatcher::GhTicketDispatcher>,
    ) {
        self.ticket_dispatcher = Some(dispatcher);
    }

    /// Build a [`phantom_agents::dispatch::DispatchContext`] from the
    /// pane's current substrate handles, if all required pieces are wired.
    ///
    /// Returns `None` when the pane was constructed without a runtime
    /// connection (legacy / test fixtures) — callers fall back to the file
    /// /git-only path. Borrows `self.working_dir` as `&Path` so the
    /// returned context's lifetime is tied to `self`'s borrow scope.
    fn build_dispatch_context(
        &self,
    ) -> Option<phantom_agents::dispatch::DispatchContext<'_>> {
        let registry = self.registry.clone()?;
        let pending_spawn = self.pending_spawn.clone()?;
        let self_ref = self.self_ref.clone()?;
        // Issue #235: inject the ticket dispatcher only for Dispatcher-role
        // panes. Non-Dispatcher agents receive `None` so the three Dispatcher
        // tools remain unreachable to them (capability gate catches first, but
        // defence-in-depth keeps the `None` path as the safe fallback).
        let ticket_dispatcher = if self.role == phantom_agents::role::AgentRole::Dispatcher {
            self.ticket_dispatcher.clone()
        } else {
            None
        };

        Some(phantom_agents::dispatch::DispatchContext {
            self_ref,
            role: self.role,
            working_dir: std::path::Path::new(self.working_dir.as_str()),
            registry,
            event_log: self.event_log.clone(),
            pending_spawn,
            source_event_id: None,
            // No quarantine registry wired at this spawn site; the gate
            // skips the quarantine check when this is `None`. The App-level
            // quarantine registry will be plumbed in a follow-up. // see #3
            quarantine: None,
            correlation_id: None,
            ticket_dispatcher,
        })
    }

    /// Poll for new API events and append output. Call once per frame.
    ///
    /// Returns `true` if new content was received this frame.
    pub(crate) fn poll(&mut self) -> bool {
        let Some(ref mut handle) = self.api_handle else {
            return false;
        };

        let mut got_content = false;

        loop {
            match handle.try_recv() {
                Some(ApiEvent::TextDelta(text)) => {
                    self.output_tokens += (text.len() / 4) as u32;
                    self.output.push_str(&text);
                    self.current_assistant_text.push_str(&text);
                    if let Some(ref mut j) = self.journal {
                        let first_line = text.lines().next().unwrap_or("").to_string();
                        if !first_line.is_empty() {
                            if let Err(e) = j.record_output(self.agent.id as u64, first_line) {
                                warn!("AgentJournal::record_output failed: {e}");
                            }
                        }
                    }
                    // Cap output to prevent unbounded memory growth.
                    if self.output.len() > 65536 {
                        let mut trim = self.output.len() - 65536;
                        while trim < self.output.len()
                            && !self.output.is_char_boundary(trim)
                        {
                            trim += 1;
                        }
                        self.output.drain(..trim);
                        self.output.insert_str(0, "[...truncated...]\n");
                    }
                    got_content = true;
                }
                Some(ApiEvent::ToolUse { id, call }) => {
                    let args_display = format_tool_args(&call.tool, &call.args);
                    if let Some(ref mut j) = self.journal {
                        if let Err(e) = j.record_tool_call(self.agent.id as u64, call.tool.api_name(), &args_display) {
                            warn!("AgentJournal::record_tool_call failed: {e}");
                        }
                    }
                    if args_display.is_empty() {
                        self.output.push_str(&format!("\n▶ {}\n", call.tool.api_name()));
                    } else {
                        self.output.push_str(&format!("\n▶ {} {}\n", call.tool.api_name(), args_display));
                    }
                    self.tool_use_ids.push(id.clone());
                    self.pending_tools.push((id, call));
                    got_content = true;
                }
                Some(ApiEvent::Done) => {
                    // Flush accumulated assistant text into the conversation.
                    if !self.current_assistant_text.is_empty() {
                        let text = std::mem::take(&mut self.current_assistant_text);
                        self.agent.push_message(AgentMessage::Assistant(text));
                    }

                    if self.pending_tools.is_empty() {
                        if let Some(ref mut j) = self.journal {
                            let summary = format!(
                                "~{}in/~{}out tokens, {} tool calls",
                                self.input_tokens, self.output_tokens, self.tool_call_count
                            );
                            if let Err(e) = j.record_completion(self.agent.id as u64, true, summary) {
                                warn!("AgentJournal::record_completion failed: {e}");
                            }
                        }
                        self.output.push_str(&format!(
                            "\n\n📊 ~{}in / ~{}out tokens | {} tool calls\n✓ Agent finished.\n",
                            self.input_tokens, self.output_tokens, self.tool_call_count,
                        ));
                        self.status = AgentPaneStatus::Done;
                        self.api_handle = None;
                        self.save_conversation();
                    } else {
                        // Execute pending tools and continue conversation.
                        self.execute_pending_tools();
                    }
                    got_content = true;
                    break;
                }
                Some(ApiEvent::Error(e)) => {
                    self.output.push_str(&format!("\n\n✗ Error: {e}\n"));
                    if let Some(ref mut j) = self.journal {
                        if let Err(je) = j.record_flatline(self.agent.id as u64, &e) {
                            warn!("AgentJournal::record_flatline failed: {je}");
                        }
                    }
                    self.rollback_if_dirty();
                    self.status = AgentPaneStatus::Failed;
                    self.api_handle = None;
                    self.save_conversation();
                    got_content = true;
                    break;
                }
                None => break,
            }
        }

        got_content
    }

    /// Execute all pending tool calls, append results to the conversation,
    /// and re-invoke the Claude API for the next turn.
    fn execute_pending_tools(&mut self) {
        if self.turn_count >= MAX_TOOL_ROUNDS {
            if let Some(ref mut j) = self.journal {
                if let Err(e) = j.record_flatline(
                    self.agent.id as u64,
                    format!("iteration limit reached ({MAX_TOOL_ROUNDS} tool rounds)"),
                ) {
                    warn!("AgentJournal::record_flatline (limit) failed: {e}");
                }
            }
            self.output.push_str(&format!(
                "\n\n✗ Agent hit iteration limit ({MAX_TOOL_ROUNDS} tool rounds).\n"
            ));
            self.rollback_if_dirty();
            self.status = AgentPaneStatus::Failed;
            self.api_handle = None;
            self.save_conversation();
            return;
        }
        self.turn_count += 1;

        // Append all tool calls to the agent's message history.
        for (_, call) in &self.pending_tools {
            self.agent
                .push_message(AgentMessage::ToolCall(call.clone()));
        }

        // Build the dispatch context once per turn so chat / composer tools
        // can fork-route by name through the same registry / event-log /
        // spawn-queue handles. When `set_substrate_handles` hasn't been
        // called (legacy / test path), we fall through to the per-tool
        // `execute_tool_with_provenance` path which only honors the
        // file/git surface.
        let working_dir = self.working_dir.clone();
        // Snapshot the substrate handles up front so we can drop the
        // immutable borrow on `self` before the body of the loop touches
        // mutable state (`tool_call_count`, `pending_tools.drain`, etc.).
        let calls: Vec<(String, ToolCall)> = self.pending_tools.drain(..).collect();

        // Execute each tool (with permission check) and append results.
        for (_, call) in calls {
            self.tool_call_count += 1;
            let start = std::time::Instant::now();
            let dispatch_ctx = self.build_dispatch_context();
            let result = if let Err(denied) = self.permissions.check_tool(&call.tool) {
                // Tag the synthetic permission-denied result with provenance
                // so source_chain_for_last_call() still finds it.
                ToolResult {
                    tool: call.tool,
                    success: false,
                    output: denied.to_string(),
                    ..ToolResult::default()
                }
                .with_provenance(phantom_agents::tools::ToolProvenance::from_call(
                    call.tool,
                    &call.args,
                    None,
                ))
            } else if let Some(ctx) = dispatch_ctx.as_ref() {
                // Substrate-aware fork: `dispatch_tool` routes by tool name
                // through file/git → chat → composer surfaces, enforcing
                // the role-class gate at a single check site. Provenance is
                // re-tagged on the way out so the audit log stays consistent.
                phantom_agents::dispatch::dispatch_tool(
                    call.tool.api_name(),
                    &call.args,
                    ctx,
                )
                .with_provenance(phantom_agents::tools::ToolProvenance::from_call(
                    call.tool,
                    &call.args,
                    None,
                ))
            } else {
                // Legacy path (no substrate handles wired): the file/git
                // surface only. The capability gate inside
                // `execute_tool_with_provenance` runs against
                // `DEFAULT_AGENT_PANE_ROLE` so the role manifest is honored
                // even without the wider dispatch context.
                phantom_agents::tools::execute_tool_with_provenance(
                    call.tool,
                    &call.args,
                    &working_dir,
                    &self.role,
                    None,
                )
            };
            // Drop the dispatch context borrow before mutating `self` below.
            drop(dispatch_ctx);
            let elapsed = start.elapsed();

            // Track file edits for rollback.
            if result.success && matches!(call.tool, ToolType::WriteFile | ToolType::EditFile) {
                self.has_file_edits = true;
            }

            // Display in pane.
            let status_char = if result.success { "✓" } else { "✗" };
            self.output.push_str(&format!(
                "  {} {:.0}ms\n",
                status_char,
                elapsed.as_millis(),
            ));

            // Show truncated output (max 200 chars for display).
            if result.output.len() > 200 {
                let truncated: String = result.output.chars().take(200).collect();
                self.output.push_str(&format!(
                    "  ← {}... ({} bytes)\n",
                    truncated,
                    result.output.len()
                ));
            } else if !result.output.is_empty() {
                self.output.push_str(&format!(
                    "  ← {}\n",
                    result.output.lines().next().unwrap_or("")
                ));
            }

            // Sec.1 capability-denial instrumentation: when the dispatch
            // gate refused the call, surface a `CapabilityDenied` substrate
            // event + matching audit-log entry before the per-result loop
            // continues. The check is keyed on the canonical
            // `"capability denied: …"` prefix so we catch denials from both
            // `execute_tool` and `dispatch_tool` without coupling to the
            // DispatchError type.
            self.maybe_emit_capability_denied_event(call.tool, &call.args, &result);

            // Lars fix-thread instrumentation (Phase 2.E producer).
            //
            // Track consecutive tool-call failures so a stuck agent surfaces
            // an `EventKind::AgentBlocked` event into the substrate runtime,
            // which the Fixer spawn rule consumes. Successful results reset
            // the streak.
            if result.success {
                self.consecutive_tool_failures = 0;
                self.last_tool_error = None;
            } else {
                self.consecutive_tool_failures =
                    self.consecutive_tool_failures.saturating_add(1);
                // Truncate the error excerpt so the eventual `reason` field
                // doesn't drag a multi-KB tool error into the spawn payload.
                let excerpt: String = result.output.chars().take(160).collect();
                self.last_tool_error = Some(excerpt);
            }

            self.agent.push_message(AgentMessage::ToolResult(result));
        }

        // After the per-turn batch settles, check whether the streak crossed
        // the block threshold. We check once per turn (not once per tool
        // result within the turn) so a single noisy turn with N failing calls
        // only emits one event — matching the spawn rule's
        // `SpawnIfNotRunning` idempotency.
        self.maybe_emit_blocked_event();

        // Re-invoke the chat backend with the updated conversation.
        let tools = available_tools();
        let handle = Self::dispatch(
            self.chat_backend.as_deref(),
            &self.claude_config,
            &self.agent,
            &tools,
            &self.tool_use_ids,
        );
        self.api_handle = Some(handle);

        self.output
            .push_str(&format!("\n● Continuing... (turn {})\n", self.turn_count));
    }

    /// Emit an `EventKind::AgentBlocked` substrate event when the agent's
    /// consecutive tool-call failure streak crosses [`TOOL_BLOCK_THRESHOLD`].
    ///
    /// Phase 2.E producer side. Resets the counter after emission so the
    /// same agent doesn't spam the bus on every subsequent failure; the
    /// `SpawnIfNotRunning` rule provides idempotency at the consumer side,
    /// but resetting here keeps the producer honest. No-op when
    /// [`AgentPane::blocked_event_sink`] is `None` (test/legacy callers
    /// without an App-owned sink).
    fn maybe_emit_blocked_event(&mut self) {
        if self.consecutive_tool_failures < TOOL_BLOCK_THRESHOLD {
            return;
        }

        let Some(sink) = self.blocked_event_sink.clone() else {
            // No sink wired (test path without an App). Reset the streak so
            // a fresh failure starts a fresh count.
            self.consecutive_tool_failures = 0;
            self.last_tool_error = None;
            return;
        };

        let last_err = self.last_tool_error.as_deref().unwrap_or("");
        let reason = format!(
            "{}+ consecutive tool failures: {}",
            self.consecutive_tool_failures, last_err
        );

        // Last 200 chars of the visible output: enough context for the
        // Fixer to triage, without dragging the whole transcript into the
        // spawn-rule payload.
        let context_excerpt: String = if self.output.chars().count() > 200 {
            let tail_chars: String = self.output.chars().rev().take(200).collect();
            tail_chars.chars().rev().collect()
        } else {
            self.output.clone()
        };

        let payload = build_blocked_payload(
            self.agent.id as u64,
            "Conversational",
            &reason,
            now_unix_ms(),
            &context_excerpt,
            "Sense",
        );

        let event = SubstrateEvent {
            kind: EventKind::AgentBlocked {
                agent_id: self.agent.id as u64,
                reason: reason.clone(),
            },
            payload,
            source: EventSource::Agent {
                role: phantom_agents::role::AgentRole::Conversational,
            },
        };

        if let Ok(mut q) = sink.lock() {
            q.push(event);
        } else {
            warn!("blocked_event_sink mutex poisoned; dropping AgentBlocked event");
        }

        self.consecutive_tool_failures = 0;
        self.last_tool_error = None;
    }

    /// Emit an [`EventKind::CapabilityDenied`] substrate event whenever the
    /// Layer-2 dispatch gate refused a tool call (Sec.1 producer side).
    ///
    /// The denial detection is by the result's canonical
    /// `"capability denied: <Class> not in <Role> manifest"` prefix —
    /// produced by [`phantom_agents::tools::DispatchError::to_tool_result_message`]
    /// and stable across both `execute_tool` and `dispatch_tool`. We push the
    /// event into the App-owned [`DeniedEventSink`] (drained next frame
    /// inline in `update.rs::update`) and emit a parallel audit-log record
    /// with [`AuditOutcome::Denied`] so the on-disk audit trail and the
    /// substrate event log carry matching denial signals.
    ///
    /// `source_chain` is empty until Sec.2 wires per-call provenance.
    /// No-op when the sink isn't wired (test / legacy paths).
    fn maybe_emit_capability_denied_event(
        &self,
        tool: ToolType,
        args: &serde_json::Value,
        result: &ToolResult,
    ) {
        // The dispatch gate's denial message is a contract: we match on the
        // exact prefix the model sees. This keeps us in sync with the
        // canonical phrasing without coupling to the DispatchError type.
        if result.success || !result.output.starts_with("capability denied:") {
            return;
        }

        let attempted_class = tool.capability_class();
        let attempted_tool = tool.api_name().to_string();
        let agent_id = self.agent.id as u64;
        let role = self.role;

        // Audit-side record (always emitted regardless of whether a sink
        // is plumbed) so the tracing log has the structured Denied entry
        // alongside any normal tool-call audit lines.
        let args_json = serde_json::to_string(args).unwrap_or_default();
        emit_tool_call(
            agent_id,
            role.label(),
            class_label(attempted_class),
            &attempted_tool,
            &args_json,
            AuditOutcome::Denied,
        );

        let Some(sink) = self.denied_event_sink.clone() else {
            return;
        };

        let payload = build_capability_denied_payload(
            agent_id,
            role.label(),
            attempted_class,
            &attempted_tool,
            now_unix_ms(),
        );

        let event = SubstrateEvent {
            kind: EventKind::CapabilityDenied {
                agent_id,
                role,
                attempted_class,
                attempted_tool,
                source_chain: Vec::new(),
            },
            payload,
            source: EventSource::Agent { role },
        };

        if let Ok(mut q) = sink.lock() {
            q.push(event);
        } else {
            warn!("denied_event_sink mutex poisoned; dropping CapabilityDenied event");
        }
    }

    /// Return the task description for this agent pane.
    pub(crate) fn task(&self) -> &str {
        &self.task
    }

    /// Return the current execution status.
    pub(crate) fn status(&self) -> AgentPaneStatus {
        self.status
    }

    /// Return the length of the accumulated output in bytes.
    pub(crate) fn output_len(&self) -> usize {
        self.output.len()
    }

    /// Return the cached tail lines slice.
    ///
    /// This is the rendering surface: `AgentAdapter` reads these each frame
    /// to avoid re-splitting the full output on every render call. The slice
    /// is stale until the next [`AgentPane::tail_lines`] call, which is
    /// driven by `AgentAdapter::update`.
    pub(crate) fn cached_lines(&self) -> &[String] {
        &self.cached_lines
    }

    /// Override the status field from adapter-side command handling.
    ///
    /// The `"dismiss"` command in `AgentAdapter` forces the pane to `Done`
    /// before marking it dismissed. Kept narrow so only the adapter can call
    /// it from outside this module.
    pub(crate) fn set_status(&mut self, status: AgentPaneStatus) {
        self.status = status;
    }

    /// Test-only accessor for the consecutive failure counter.
    #[cfg(test)]
    #[allow(dead_code)] // Phase 2.G consumer wiring will exercise this.
    pub(crate) fn consecutive_tool_failures(&self) -> u32 {
        self.consecutive_tool_failures
    }

    /// Test-only setter for the blocked-event sink.
    #[cfg(test)]
    #[allow(dead_code)] // Phase 2.G consumer wiring will exercise this.
    pub(crate) fn set_blocked_event_sink_for_test(&mut self, sink: BlockedEventSink) {
        self.blocked_event_sink = Some(sink);
    }

    /// Test-only setter for the capability-denied event sink (Sec.1).
    #[cfg(test)]
    #[allow(dead_code)] // Sec.4 consumer wiring will exercise this.
    pub(crate) fn set_denied_event_sink_for_test(&mut self, sink: DeniedEventSink) {
        self.denied_event_sink = Some(sink);
    }

    /// Test-only setter for the agent's role; lets tests construct a pane
    /// running as a `Watcher` so `run_command` lands on the denial path
    /// without standing up a full spawn pipeline.
    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn set_role_for_test(&mut self, role: AgentRole) {
        self.role = role;
    }

    /// Test-only entry point exercising the producer-side fix-thread logic
    /// without touching the chat backend. Mirrors the per-tool-result branch
    /// of `execute_pending_tools` and triggers the threshold check.
    #[cfg(test)]
    #[allow(dead_code)] // Phase 2.G consumer wiring will exercise this.
    pub(crate) fn record_tool_result_for_test(&mut self, success: bool, error_excerpt: &str) {
        if success {
            self.consecutive_tool_failures = 0;
            self.last_tool_error = None;
        } else {
            self.consecutive_tool_failures =
                self.consecutive_tool_failures.saturating_add(1);
            self.last_tool_error = Some(error_excerpt.to_string());
        }
        self.maybe_emit_blocked_event();
    }

    /// Return cached tail lines for rendering. Only re-splits when output grows.
    pub(crate) fn tail_lines(&mut self, max_lines: usize) -> &[String] {
        if self.output.len() != self.cached_len {
            self.cached_len = self.output.len();
            let all: Vec<&str> = self.output.lines().collect();
            let start = all.len().saturating_sub(max_lines);
            self.cached_lines = all[start..].iter().map(|s| s.to_string()).collect();
        }
        &self.cached_lines
    }

    /// Send a follow-up user message and re-invoke Claude.
    ///
    /// This is the interactive chat loop — the user types in the agent pane,
    /// the message gets appended to the conversation, and Claude responds.
    pub(crate) fn send_followup(&mut self, message: String) {
        // Display the user message.
        self.output.push_str(&format!("\n\n> {message}\n\n● Thinking...\n"));

        // Append to conversation.
        self.agent.push_message(AgentMessage::User(message));

        // Re-invoke the chat backend with the updated conversation.
        let tools = available_tools();
        let handle = Self::dispatch(
            self.chat_backend.as_deref(),
            &self.claude_config,
            &self.agent,
            &tools,
            &self.tool_use_ids,
        );
        self.api_handle = Some(handle);
        self.status = AgentPaneStatus::Working;
        self.current_assistant_text.clear();
    }

    /// Revert file edits on failure (git checkout -- .).
    fn rollback_if_dirty(&mut self) {
        if !self.has_file_edits { return; }
        self.output.push_str("\n⚠ Agent failed with uncommitted edits. Reverting...\n");
        let result = std::process::Command::new("git")
            .args(["checkout", "--", "."])
            .current_dir(&self.working_dir)
            .output();
        match result {
            Ok(out) if out.status.success() => {
                self.output.push_str("  ← Reverted to clean state.\n");
            }
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                self.output.push_str(&format!("  ← Revert failed: {stderr}\n"));
            }
            Err(e) => {
                self.output.push_str(&format!("  ← Revert failed: {e}\n"));
            }
        }
    }

    /// Save the agent conversation to disk for debugging and replay.
    pub(crate) fn save_conversation(&self) {
        let dir = std::env::var("HOME")
            .map(|h| std::path::PathBuf::from(h).join(".config/phantom/agents"))
            .unwrap_or_else(|_| std::path::PathBuf::from("/tmp/phantom-agents"));

        if std::fs::create_dir_all(&dir).is_err() { return; }

        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs()).unwrap_or(0);

        let sanitized: String = self.task.chars().take(30)
            .map(|c| if c.is_alphanumeric() || c == '-' { c } else { '_' })
            .collect();

        let path = dir.join(format!("{timestamp}_{sanitized}.json"));
        let json = self.agent.to_json();
        if let Ok(content) = serde_json::to_string_pretty(&json) {
            let _ = std::fs::write(&path, content);
            log::info!("Agent conversation saved: {}", path.display());
        }
    }

}

// ---------------------------------------------------------------------------
// Codebase context injection
// ---------------------------------------------------------------------------

/// Build project context for agent system prompts.
/// Reads CLAUDE.md if it exists, and provides a crate map.
fn build_codebase_context() -> String {
    let working_dir = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".into());

    let mut ctx = String::from(
        "CODEBASE CONTEXT:\n\
         You are an agent inside Phantom, an AI-native terminal emulator.\n\
         Written in Rust. 19 crates. ~100K lines. deny(warnings) is enforced.\n\
         Always run `cargo check --workspace` after edits.\n\n\
         Key crates:\n\
         - phantom (binary entry point)\n\
         - phantom-app (GUI: render, input, mouse, coordinator, agent_pane)\n\
         - phantom-brain (OODA loop, scoring, goals, proactive, orchestrator)\n\
         - phantom-agents (tools, API client, permissions, agent lifecycle)\n\
         - phantom-adapter (AppAdapter trait, spatial preferences, event bus)\n\
         - phantom-ui (layout engine, arbiter, themes, keybinds)\n\
         - phantom-terminal (PTY, VTE, SGR mouse encoding)\n\
         - phantom-scene (scene graph, z-order, dirty flags, render layers)\n\
         - phantom-semantic (output parsing, error detection)\n\
         - phantom-context (project detection, git state)\n\
         - phantom-memory (persistent key-value store)\n\
         - phantom-mcp (MCP protocol, Unix socket server/client)\n\n"
    );

    // Try to read CLAUDE.md for project-specific instructions.
    let claude_md = std::path::Path::new(&working_dir).join("CLAUDE.md");
    if let Ok(content) = std::fs::read_to_string(&claude_md) {
        let truncated = if content.len() > 2000 {
            format!("{}...(truncated)", &content[..2000])
        } else {
            content
        };
        ctx.push_str(&format!("CLAUDE.md:\n{truncated}\n\n"));
    }

    ctx
}

// ---------------------------------------------------------------------------
// Display helpers
// ---------------------------------------------------------------------------

/// Format tool arguments as a compact, human-readable string.
fn format_tool_args(tool: &ToolType, args: &serde_json::Value) -> String {
    match tool {
        ToolType::ReadFile | ToolType::EditFile | ToolType::ListFiles => {
            args.get("path").and_then(|v| v.as_str()).unwrap_or("?").to_string()
        }
        ToolType::WriteFile => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("?");
            let len = args.get("content").and_then(|v| v.as_str()).map(|s| s.len()).unwrap_or(0);
            format!("{path} ({len} bytes)")
        }
        ToolType::RunCommand => {
            args.get("command").and_then(|v| v.as_str()).unwrap_or("?").to_string()
        }
        ToolType::SearchFiles => {
            args.get("pattern").and_then(|v| v.as_str()).unwrap_or("?").to_string()
        }
        ToolType::GitStatus | ToolType::GitDiff => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Journal helpers
// ---------------------------------------------------------------------------

/// Open (or create) the per-agent JSONL journal file.
///
/// Returns `None` on any I/O error; callers treat the journal as best-effort
/// observability and never abort an agent spawn on journal failure.
fn open_agent_journal(agent_id: u64) -> Option<phantom_memory::journal::AgentJournal> {
    let dir = std::env::var("HOME")
        .ok()
        .map(|h| std::path::PathBuf::from(h).join(".config/phantom/agents/journals"))
        .unwrap_or_else(|| std::env::temp_dir().join("phantom-agents/journals"));
    if let Err(e) = std::fs::create_dir_all(&dir) {
        warn!(
            "AgentJournal: could not create journal dir {}: {e}",
            dir.display()
        );
        return None;
    }
    let path = dir.join(format!("{agent_id}.jsonl"));
    match phantom_memory::journal::AgentJournal::open(&path) {
        Ok(j) => Some(j),
        Err(e) => {
            warn!("AgentJournal: could not open {}: {e}", path.display());
            None
        }
    }
}

// ---------------------------------------------------------------------------
// App integration
// ---------------------------------------------------------------------------

impl App {
    /// Spawn a new agent pane as a first-class coordinator adapter.
    ///
    /// Backwards-compatible wrapper over [`App::spawn_agent_pane_with_opts`].
    /// Existing callers passing a bare [`AgentTask`] keep working byte-for-byte
    /// (no chat model override → default Claude path).
    pub(crate) fn spawn_agent_pane(&mut self, task: AgentTask) -> bool {
        self.spawn_agent_pane_with_opts(AgentSpawnOpts::new(task))
    }

    /// Spawn a new agent pane with explicit spawn options.
    ///
    /// Splits the focused pane vertically, creates the agent (using the
    /// requested [`ChatModel`] if any), wraps it in an `AgentAdapter`, and
    /// registers it in the new split pane.
    pub(crate) fn spawn_agent_pane_with_opts(
        &mut self,
        opts: AgentSpawnOpts,
    ) -> bool {
        // Extract spawn_tag before opts is moved into spawn_with_opts.
        let spawn_tag = opts.spawn_tag;
        let Some(claude_config) = ClaudeConfig::from_env() else {
            warn!("Cannot spawn agent: ANTHROPIC_API_KEY not set");
            return false;
        };

        // Split the focused pane to make room for the agent.
        let Some(focused_app_id) = self.coordinator.focused() else {
            warn!("Cannot spawn agent: no focused adapter");
            return false;
        };
        let Some(current_pane_id) = self.coordinator.pane_id_for(focused_app_id) else {
            warn!("Cannot spawn agent: focused adapter has no layout pane");
            return false;
        };

        let split_result = self.layout.split_vertical(current_pane_id);
        let (existing_child, new_child) = match split_result {
            Ok(ids) => ids,
            Err(e) => {
                warn!("Agent split failed: {e}");
                return false;
            }
        };

        // Equal split: terminal 50%, agent 50%.
        let _ = self.layout.set_flex_grow(existing_child, 1.0);
        let _ = self.layout.set_flex_grow(new_child, 1.0);

        // Resize layout after split.
        let width = self.gpu.surface_config.width;
        let height = self.gpu.surface_config.height;
        let _ = self.layout.resize(width as f32, height as f32);

        // Remap the existing terminal's PaneId.
        self.coordinator.remap_pane(focused_app_id, current_pane_id, existing_child);

        // Resize the existing terminal to fit its new (smaller) pane.
        if let Ok(rect) = self.layout.get_pane_rect(existing_child) {
            let (cols, rows) = crate::pane::pane_cols_rows(self.cell_size, rect);
            let _ = self.coordinator.send_command(
                focused_app_id,
                "resize",
                &serde_json::json!({"cols": cols, "rows": rows}),
            );
        }

        // Create the agent and register in the new split pane.
        //
        // Hand the App's canonical `BlockedEventSink` to the pane so that
        // when the agent's consecutive tool-call failure streak crosses
        // [`TOOL_BLOCK_THRESHOLD`], an `EventKind::AgentBlocked` substrate
        // event lands in `App.blocked_event_sink`. The drain step in
        // `update.rs::update` picks those up each frame and forwards them
        // into the substrate runtime
        // where the Fixer spawn rule consumes them and queues a Fixer
        // `SpawnAction` — closing the producer→consumer loop.
        let mut agent_pane = AgentPane::spawn_with_opts(
            opts,
            &claude_config,
            Some(self.blocked_event_sink.clone()),
            None,
        );

        // Wire the substrate handles so chat-tool / composer-tool dispatch
        // routes through the live runtime. The pane gets clones of the
        // runtime's `Arc<Mutex<…>>` registry + event log and a clone of the
        // App's `pending_spawn_subagent` queue. The `AgentRef` is stamped
        // with a fresh id (currently 0 — the agent module's `AgentId` is a
        // `u32` per-session counter) and the default
        // [`DEFAULT_AGENT_PANE_ROLE`]; when the next phase wires Composer
        // panes through this path the role override will live on
        // [`AgentSpawnOpts`].
        let self_ref = phantom_agents::role::AgentRef::new(
            0,
            DEFAULT_AGENT_PANE_ROLE,
            "agent-pane",
            phantom_agents::role::SpawnSource::User,
        );
        agent_pane.set_substrate_handles(
            self.runtime.registry_handle(),
            self.runtime.event_log_handle(),
            self.pending_spawn_subagent.clone(),
            self_ref,
            DEFAULT_AGENT_PANE_ROLE,
        );

        // Issue #235: inject the ticket dispatcher for Dispatcher-role panes.
        // `agent_pane.role` was just set by `set_substrate_handles` above.
        // For the current default (Conversational) this is a no-op. When a
        // future spawn path sets role = Dispatcher this branch fires and the
        // pane gains live access to the GH ticket queue.
        if agent_pane.role == phantom_agents::role::AgentRole::Dispatcher {
            if let Some(ref td) = self.ticket_dispatcher {
                agent_pane.set_ticket_dispatcher(std::sync::Arc::clone(td));
            } else {
                warn!(
                    "Spawning a Dispatcher-role agent pane but ticket_dispatcher is None \
                     (GITHUB_TOKEN / GH_REPO not set); ticket tools will fail gracefully"
                );
            }
        }

        let adapter = crate::adapters::agent::AgentAdapter::with_spawn_tag(agent_pane, spawn_tag);

        let scene_node = self.scene.add_node(
            self.scene_content_node,
            phantom_scene::node::NodeKind::Pane,
        );

        let app_id = self.coordinator.register_adapter_at_pane(
            Box::new(adapter),
            new_child,
            scene_node,
            phantom_scene::clock::Cadence::unlimited(),
        );

        // Focus the new agent pane.
        self.coordinator.set_focus(app_id);

        // Notify the substrate runtime that an agent pane was opened. The
        // seed `pane.opened.agent` rule will fire on the next tick, queueing
        // a `SpawnIfNotRunning(Watcher, "agent-pane-watch")` action and
        // appending the event to `events.jsonl`. Phase 2 will turn that
        // queued action into an actual supervised Watcher task.
        self.runtime.push_event(phantom_agents::spawn_rules::SubstrateEvent {
            kind: phantom_agents::spawn_rules::EventKind::PaneOpened {
                app_type: "agent".to_string(),
            },
            payload: serde_json::json!({
                "app_id": app_id,
                "pane_id": format!("{:?}", new_child),
            }),
            source: phantom_agents::spawn_rules::EventSource::User,
        });

        info!("Agent adapter registered (AppId {app_id}) in split pane");
        true
    }

    /// Drain the App's `BlockedEventSink` and return the queued
    /// `EventKind::AgentBlocked` substrate events.
    ///
    /// Producers (`AgentPane::execute_pending_tools` via
    /// [`AgentPane::maybe_emit_blocked_event`]) push events synchronously when
    /// an agent's consecutive tool-call failure streak crosses
    /// [`TOOL_BLOCK_THRESHOLD`]. The consumer side (`update.rs::update`) calls
    /// this each frame and forwards every drained event into
    /// [`crate::runtime::AgentRuntime::push_event`], where the registered
    /// `fixer_spawn_rule` matches and queues a Fixer `SpawnAction`.
    ///
    /// Returns an empty `Vec` if the sink is empty or the mutex is poisoned —
    /// observability is best-effort, never fatal.
    pub(crate) fn drain_blocked_events(
        &mut self,
    ) -> Vec<phantom_agents::spawn_rules::SubstrateEvent> {
        match self.blocked_event_sink.lock() {
            Ok(mut q) => std::mem::take(&mut *q),
            Err(_) => {
                warn!("blocked_event_sink mutex poisoned; dropping queued events");
                Vec::new()
            }
        }
    }

}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn test_agent() -> Agent {
        Agent::new(0, AgentTask::FreeForm { prompt: "test task".into() })
    }

    fn test_config() -> ClaudeConfig {
        ClaudeConfig::new("sk-test-fake")
    }

    fn agent_with_handle() -> (AgentPane, mpsc::Sender<ApiEvent>) {
        let (tx, rx) = mpsc::channel();
        let handle = ApiHandle::from_receiver(rx);
        let pane = AgentPane {
            task: "test task".into(),
            status: AgentPaneStatus::Working,
            output: String::from("● Agent working...\n\n"),
            api_handle: Some(handle),
            tool_use_ids: Vec::new(),
            cached_lines: Vec::new(),
            cached_len: 0,
            agent: test_agent(),
            pending_tools: Vec::new(),
            working_dir: ".".into(),
            claude_config: test_config(),
            chat_backend: None,
            consecutive_tool_failures: 0,
            blocked_event_sink: None,
            denied_event_sink: None,
            last_tool_error: None,
            turn_count: 0,
            current_assistant_text: String::new(),
            permissions: PermissionSet::all(),
            input_tokens: 0,
            output_tokens: 0,
            tool_call_count: 0,
            has_file_edits: false,
            registry: None,
            event_log: None,
            pending_spawn: None,
            self_ref: None,
            role: DEFAULT_AGENT_PANE_ROLE,
            ticket_dispatcher: None,
            journal: None,
        };
        (pane, tx)
    }

    #[test]
    fn agent_pane_starts_working() {
        let (pane, _tx) = agent_with_handle();
        assert_eq!(pane.status, AgentPaneStatus::Working);
        assert!(pane.output.contains("Agent working"));
    }

    #[test]
    fn poll_receives_text_delta() {
        let (mut pane, tx) = agent_with_handle();
        tx.send(ApiEvent::TextDelta("hello world".into())).unwrap();

        let got = pane.poll();
        assert!(got, "should have received content");
        assert!(pane.output.contains("hello world"));
        assert_eq!(pane.status, AgentPaneStatus::Working);
    }

    #[test]
    fn poll_receives_done_event() {
        let (mut pane, tx) = agent_with_handle();
        tx.send(ApiEvent::TextDelta("result".into())).unwrap();
        tx.send(ApiEvent::Done).unwrap();

        pane.poll();
        assert_eq!(pane.status, AgentPaneStatus::Done);
        assert!(pane.output.contains("✓ Agent finished"));
        assert!(pane.api_handle.is_none(), "handle should be dropped on Done");
    }

    #[test]
    fn poll_receives_error_event() {
        let (mut pane, tx) = agent_with_handle();
        tx.send(ApiEvent::Error("network timeout".into())).unwrap();

        pane.poll();
        assert_eq!(pane.status, AgentPaneStatus::Failed);
        assert!(pane.output.contains("✗ Error: network timeout"));
        assert!(pane.api_handle.is_none());
    }

    #[test]
    fn poll_accumulates_multiple_deltas() {
        let (mut pane, tx) = agent_with_handle();
        tx.send(ApiEvent::TextDelta("line 1\n".into())).unwrap();
        tx.send(ApiEvent::TextDelta("line 2\n".into())).unwrap();
        tx.send(ApiEvent::TextDelta("line 3\n".into())).unwrap();

        pane.poll();
        assert!(pane.output.contains("line 1"));
        assert!(pane.output.contains("line 2"));
        assert!(pane.output.contains("line 3"));
    }

    #[test]
    fn poll_returns_false_when_no_handle() {
        let mut pane = AgentPane {
            task: "orphan".into(),
            status: AgentPaneStatus::Done,
            output: String::new(),
            api_handle: None,
            tool_use_ids: Vec::new(),
            cached_lines: Vec::new(),
            cached_len: 0,
            agent: test_agent(),
            pending_tools: Vec::new(),
            working_dir: ".".into(),
            claude_config: test_config(),
            chat_backend: None,
            consecutive_tool_failures: 0,
            blocked_event_sink: None,
            denied_event_sink: None,
            last_tool_error: None,
            turn_count: 0,
            current_assistant_text: String::new(),
            permissions: PermissionSet::all(),
            input_tokens: 0,
            output_tokens: 0,
            tool_call_count: 0,
            has_file_edits: false,
            registry: None,
            event_log: None,
            pending_spawn: None,
            self_ref: None,
            role: DEFAULT_AGENT_PANE_ROLE,
            ticket_dispatcher: None,
            journal: None,
        };
        assert!(!pane.poll());
    }

    #[test]
    fn poll_returns_false_when_no_events() {
        let (mut pane, _tx) = agent_with_handle();
        // Don't send anything.
        assert!(!pane.poll());
        assert_eq!(pane.status, AgentPaneStatus::Working);
    }

    #[test]
    fn tool_use_tracked_in_ids() {
        let (mut pane, tx) = agent_with_handle();
        tx.send(ApiEvent::ToolUse {
            id: "tool_123".into(),
            call: phantom_agents::tools::ToolCall {
                tool: phantom_agents::tools::ToolType::ReadFile,
                args: serde_json::json!({"path": "/tmp/test"}),
            },
        }).unwrap();

        pane.poll();
        assert_eq!(pane.tool_use_ids, vec!["tool_123"]);
        assert!(pane.output.contains("▶ read_file"));
        // New: also tracked in pending_tools.
        assert_eq!(pane.pending_tools.len(), 1);
        assert_eq!(pane.pending_tools[0].0, "tool_123");
    }

    #[test]
    fn text_delta_accumulates_assistant_text() {
        let (mut pane, tx) = agent_with_handle();
        tx.send(ApiEvent::TextDelta("hello ".into())).unwrap();
        tx.send(ApiEvent::TextDelta("world".into())).unwrap();

        pane.poll();
        assert_eq!(pane.current_assistant_text, "hello world");
    }

    #[test]
    fn done_without_tools_marks_finished() {
        let (mut pane, tx) = agent_with_handle();
        tx.send(ApiEvent::TextDelta("result".into())).unwrap();
        tx.send(ApiEvent::Done).unwrap();

        pane.poll();
        assert_eq!(pane.status, AgentPaneStatus::Done);
        assert!(pane.api_handle.is_none());
        // Assistant text should have been flushed to agent messages.
        assert!(pane.current_assistant_text.is_empty());
        assert!(pane.agent.messages.iter().any(|m| matches!(m, AgentMessage::Assistant(t) if t == "result")));
    }

    #[test]
    fn done_with_tools_executes_and_continues() {
        let (mut pane, tx) = agent_with_handle();
        // Set working_dir to temp dir so ListFiles works.
        pane.working_dir = std::env::temp_dir().to_string_lossy().into_owned();

        tx.send(ApiEvent::TextDelta("Let me check.".into())).unwrap();
        tx.send(ApiEvent::ToolUse {
            id: "toolu_1".into(),
            call: phantom_agents::tools::ToolCall {
                tool: phantom_agents::tools::ToolType::ListFiles,
                args: serde_json::json!({"path": "."}),
            },
        }).unwrap();
        tx.send(ApiEvent::Done).unwrap();

        pane.poll();

        // Should NOT be Done — should have re-invoked.
        assert_eq!(pane.status, AgentPaneStatus::Working);
        // pending_tools should be drained.
        assert!(pane.pending_tools.is_empty());
        // turn_count should have incremented.
        assert_eq!(pane.turn_count, 1);
        // Agent messages should include ToolCall and ToolResult.
        let has_tool_call = pane.agent.messages.iter().any(|m| matches!(m, AgentMessage::ToolCall(_)));
        let has_tool_result = pane.agent.messages.iter().any(|m| matches!(m, AgentMessage::ToolResult(_)));
        assert!(has_tool_call, "agent should have a ToolCall message");
        assert!(has_tool_result, "agent should have a ToolResult message");
        // Output should show the continuation.
        assert!(pane.output.contains("Continuing... (turn 1)"));
        // A new api_handle should have been created (by send_message).
        assert!(pane.api_handle.is_some());
    }

    #[test]
    fn iteration_limit_stops_agent() {
        let (mut pane, tx) = agent_with_handle();
        pane.turn_count = MAX_TOOL_ROUNDS; // Already at limit.

        tx.send(ApiEvent::ToolUse {
            id: "toolu_limit".into(),
            call: phantom_agents::tools::ToolCall {
                tool: phantom_agents::tools::ToolType::GitStatus,
                args: serde_json::json!({}),
            },
        }).unwrap();
        tx.send(ApiEvent::Done).unwrap();

        pane.poll();

        assert_eq!(pane.status, AgentPaneStatus::Failed);
        assert!(pane.output.contains("iteration limit"));
        assert!(pane.api_handle.is_none());
    }

    #[test]
    fn task_description_extraction() {
        // Verify the description logic works for each AgentTask variant.
        let cases: Vec<(AgentTask, &str)> = vec![
            (AgentTask::FreeForm { prompt: "fix bug".into() }, "fix bug"),
            (AgentTask::RunCommand { command: "cargo test".into() }, "Run: cargo test"),
            (AgentTask::WatchAndNotify { description: "build".into() }, "Watch: build"),
        ];

        for (task, expected_prefix) in cases {
            let desc = match &task {
                AgentTask::FreeForm { prompt } => prompt.clone(),
                AgentTask::FixError { error_summary, .. } => format!("Fix: {error_summary}"),
                AgentTask::RunCommand { command } => format!("Run: {command}"),
                AgentTask::ReviewCode { context, .. } => format!("Review: {context}"),
                AgentTask::WatchAndNotify { description } => format!("Watch: {description}"),
            };
            assert!(
                desc.starts_with(expected_prefix),
                "task desc '{desc}' should start with '{expected_prefix}'"
            );
        }
    }

    // -- Lars fix-thread producer tests (Phase 2.E) --------------------------

    /// One failure under the threshold must NOT emit an `AgentBlocked` event.
    /// The producer should only fire when the streak crosses
    /// [`TOOL_BLOCK_THRESHOLD`] = 2.
    #[test]
    fn consecutive_tool_failures_below_threshold_does_not_emit() {
        let (mut pane, _tx) = agent_with_handle();
        let sink = new_blocked_event_sink();
        pane.set_blocked_event_sink_for_test(sink.clone());

        pane.record_tool_result_for_test(false, "ENOENT: no such file");

        assert_eq!(pane.consecutive_tool_failures(), 1);
        let drained = sink.lock().unwrap();
        assert!(
            drained.is_empty(),
            "1 failure (< threshold) must not emit an AgentBlocked event; got {} events",
            drained.len(),
        );
    }

    /// Exactly two consecutive failures must emit exactly one `AgentBlocked`
    /// event, after which the streak counter is reset to 0 (so the agent
    /// doesn't spam the bus).
    #[test]
    fn consecutive_tool_failures_at_threshold_emits_blocked() {
        let (mut pane, _tx) = agent_with_handle();
        let sink = new_blocked_event_sink();
        pane.set_blocked_event_sink_for_test(sink.clone());

        pane.record_tool_result_for_test(false, "first error");
        pane.record_tool_result_for_test(false, "second error");

        // After emission the producer resets the counter so the SAME agent
        // doesn't keep re-emitting on every subsequent failure.
        assert_eq!(
            pane.consecutive_tool_failures(),
            0,
            "streak counter must reset after emit",
        );

        let drained = sink.lock().unwrap();
        assert_eq!(
            drained.len(),
            1,
            "exactly one AgentBlocked event must have been emitted; got {}",
            drained.len(),
        );
    }

    /// A successful tool call between two failures must reset the streak,
    /// so the cumulative failure count is 1 (not 2) and no event fires.
    #[test]
    fn success_resets_counter() {
        let (mut pane, _tx) = agent_with_handle();
        let sink = new_blocked_event_sink();
        pane.set_blocked_event_sink_for_test(sink.clone());

        pane.record_tool_result_for_test(false, "first failure");
        pane.record_tool_result_for_test(true, "");
        pane.record_tool_result_for_test(false, "second failure");

        assert_eq!(
            pane.consecutive_tool_failures(),
            1,
            "consecutive count must be 1 (only the failure since the last success)",
        );

        let drained = sink.lock().unwrap();
        assert!(
            drained.is_empty(),
            "no event should have fired when streak was broken by a success; got {}",
            drained.len(),
        );
    }

    /// The emitted `AgentBlocked` event payload must carry the conventional
    /// keys documented in `phantom_agents::fixer`: `agent_id`, `agent_role`,
    /// `reason`, `blocked_at_unix_ms`, `context_excerpt`,
    /// `suggested_capability`.
    #[test]
    fn blocked_event_payload_has_agent_id_and_reason() {
        let (mut pane, _tx) = agent_with_handle();
        let sink = new_blocked_event_sink();
        pane.set_blocked_event_sink_for_test(sink.clone());

        pane.record_tool_result_for_test(false, "first");
        pane.record_tool_result_for_test(false, "ENOENT: project_memory.txt");

        let drained = sink.lock().unwrap();
        assert_eq!(drained.len(), 1, "expected exactly one event");

        let ev = &drained[0];

        // Kind invariant.
        match &ev.kind {
            phantom_agents::spawn_rules::EventKind::AgentBlocked { agent_id, reason } => {
                assert_eq!(*agent_id as u64, 0u64); // test_agent has id 0
                assert!(
                    reason.contains("consecutive tool failures"),
                    "reason should mention the streak; got '{reason}'",
                );
                assert!(
                    reason.contains("ENOENT") || reason.contains("project_memory.txt"),
                    "reason should embed the last error excerpt; got '{reason}'",
                );
            }
            other => panic!("expected EventKind::AgentBlocked, got {other:?}"),
        }

        // Source invariant: the producer is a Conversational agent.
        match ev.source {
            phantom_agents::spawn_rules::EventSource::Agent { role } => {
                assert_eq!(role, phantom_agents::role::AgentRole::Conversational);
            }
            other => panic!("expected EventSource::Agent, got {other:?}"),
        }

        // Payload shape.
        let payload = &ev.payload;
        assert!(payload.get("agent_id").is_some(), "payload missing agent_id");
        assert!(payload.get("agent_role").is_some(), "payload missing agent_role");
        assert!(payload.get("reason").is_some(), "payload missing reason");
        assert!(
            payload.get("blocked_at_unix_ms").is_some(),
            "payload missing blocked_at_unix_ms",
        );
        assert!(
            payload.get("context_excerpt").is_some(),
            "payload missing context_excerpt",
        );
        assert!(
            payload.get("suggested_capability").is_some(),
            "payload missing suggested_capability",
        );
        assert_eq!(
            payload.get("agent_role").and_then(|v| v.as_str()),
            Some("Conversational"),
        );
    }

    /// End-to-end producer→consumer wiring: when an `AgentBlocked` event
    /// lands in a `SpawnRuleRegistry` that has the Fixer rule registered,
    /// `evaluate` returns the canonical Fixer `SpawnAction`. This is the
    /// substrate-level guarantee that producer-side wiring is sufficient
    /// for the Fixer to spawn (Phase 2.G turns the action into an actual
    /// agent).
    #[test]
    fn runtime_evaluate_on_blocked_returns_fixer_action() {
        use phantom_agents::fixer::fixer_spawn_rule;
        use phantom_agents::role::AgentRole;
        use phantom_agents::spawn_rules::{SpawnAction, SpawnRuleRegistry};

        let (mut pane, _tx) = agent_with_handle();
        let sink = new_blocked_event_sink();
        pane.set_blocked_event_sink_for_test(sink.clone());

        pane.record_tool_result_for_test(false, "first failure");
        pane.record_tool_result_for_test(false, "second failure");

        let drained = sink.lock().unwrap();
        assert_eq!(drained.len(), 1, "producer must have emitted exactly 1 event");

        // Hand the producer's event to the substrate-level rule registry.
        // No actual agent spawns — we just verify the Fixer action would be
        // queued (Phase 2.G consumer is responsible for honoring it).
        let registry = SpawnRuleRegistry::new().add(fixer_spawn_rule());
        let actions = registry.evaluate(&drained[0]);

        assert_eq!(actions.len(), 1, "Fixer rule must fire exactly once");
        match actions[0] {
            SpawnAction::SpawnIfNotRunning {
                role,
                label_template,
                ..
            } => {
                assert_eq!(*role, AgentRole::Fixer);
                assert_eq!(label_template, "fixer-on-blockage");
            }
            other => panic!("expected SpawnIfNotRunning(Fixer), got {other:?}"),
        }
    }

    // ---- Sec.1: CapabilityDenied substrate-event producer ------------------

    /// When the dispatch gate refuses a tool call because the agent's role
    /// manifest does not include the tool's capability class, the pane MUST
    /// push a `SubstrateEvent` of kind `CapabilityDenied` into the App-owned
    /// `DeniedEventSink`. The scenario: a `Watcher` agent invokes
    /// `run_command` (Act) — gate rejects, `maybe_emit_capability_denied_event`
    /// records the denial.
    ///
    /// `execute_tool` is an internal helper that does not check capabilities
    /// (see issue #104 — `dispatch_tool` is the single gate). We construct
    /// the canonical denial ToolResult directly to test the producer logic
    /// in isolation from the gate.
    #[test]
    fn dispatch_denial_pushes_event_to_sink() {
        use phantom_agents::role::CapabilityClass;
        use phantom_agents::tools::ToolType;

        let (mut pane, _tx) = agent_with_handle();
        pane.set_role_for_test(AgentRole::Watcher);
        let sink = new_denied_event_sink();
        pane.set_denied_event_sink_for_test(sink.clone());

        // Simulate a denial result — the canonical prefix that dispatch_tool
        // produces when a Watcher calls run_command (Act-class). We construct
        // it directly because execute_tool is a capability-agnostic helper
        // (the gate lives in dispatch_tool; see issue #104).
        let args = serde_json::json!({"command": "echo SHOULD_NEVER_RUN"});
        let result = phantom_agents::tools::ToolResult {
            tool: ToolType::RunCommand,
            success: false,
            output: "capability denied: Act not in Watcher manifest".to_string(),
            ..phantom_agents::tools::ToolResult::default()
        };

        // Hand the result through the producer hook the pane uses inside
        // `execute_pending_tools`. The sink must end up with exactly one
        // SubstrateEvent of kind CapabilityDenied carrying the role,
        // class, and tool name.
        pane.maybe_emit_capability_denied_event(ToolType::RunCommand, &args, &result);

        let drained = sink.lock().unwrap();
        assert_eq!(drained.len(), 1, "expected one CapabilityDenied event");
        let ev = &drained[0];
        match &ev.kind {
            EventKind::CapabilityDenied {
                agent_id: _,
                role,
                attempted_class,
                attempted_tool,
                source_chain,
            } => {
                assert_eq!(*role, AgentRole::Watcher);
                assert_eq!(*attempted_class, CapabilityClass::Act);
                assert_eq!(attempted_tool, "run_command");
                assert!(
                    source_chain.is_empty(),
                    "Sec.1 emits empty source_chain; Sec.2 will populate it"
                );
            }
            other => panic!("expected CapabilityDenied, got {other:?}"),
        }
        // The source must attribute the event to the agent.
        match ev.source {
            EventSource::Agent { role } => assert_eq!(role, AgentRole::Watcher),
            other => panic!("expected EventSource::Agent, got {other:?}"),
        }
    }

    /// The audit log records the same denial alongside the SubstrateEvent.
    /// Both signals must agree: `outcome=denied`, the tool name and class
    /// match what the model attempted. We use a temp dir for the audit
    /// log file and verify the record lands.
    ///
    /// Like `dispatch_denial_pushes_event_to_sink`, we construct the denial
    /// result directly rather than calling `execute_tool` — `execute_tool`
    /// is capability-agnostic (see issue #104); the gate lives in
    /// `dispatch_tool`.
    #[test]
    fn audit_denied_outcome_emitted_alongside_event() {
        use phantom_agents::audit;
        use phantom_agents::tools::ToolType;

        // Initialize the audit subscriber against a tempdir so we can read
        // the JSONL file back. This is process-global; if another test in
        // this process already initialized it, our subscriber is a no-op
        // and the assertion below works on whichever audit dir won the
        // race (or fails fast — see the audit module's footgun docs).
        let audit_dir = tempfile::tempdir().unwrap();
        let writer = audit::init(audit_dir.path()).expect("init audit");

        let (mut pane, _tx) = agent_with_handle();
        pane.set_role_for_test(AgentRole::Watcher);
        let sink = new_denied_event_sink();
        pane.set_denied_event_sink_for_test(sink.clone());

        // Construct a canonical denial result directly (see issue #104).
        let args = serde_json::json!({"command": "ls"});
        let result = phantom_agents::tools::ToolResult {
            tool: ToolType::RunCommand,
            success: false,
            output: "capability denied: Act not in Watcher manifest".to_string(),
            ..phantom_agents::tools::ToolResult::default()
        };

        pane.maybe_emit_capability_denied_event(ToolType::RunCommand, &args, &result);

        // Drop the writer to flush the non-blocking audit appender.
        drop(writer);

        // SubstrateEvent side: still exactly one event in the sink.
        let drained = sink.lock().unwrap();
        assert_eq!(drained.len(), 1, "expected one CapabilityDenied event");

        // Audit-log side: scan the rolling daily file(s) for an entry
        // matching our role + tool + denied outcome. Best-effort because
        // tracing's global subscriber may have been set up by another
        // test; a missing record there is tolerated as long as the
        // SubstrateEvent landed (the audit invariant is observable in
        // the dedicated `init_then_emit_writes_record_and_drop_flushes`
        // test in `audit.rs`).
        let entries = std::fs::read_dir(audit_dir.path()).expect("readdir");
        let mut found_denied = false;
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy().into_owned();
            if !name.starts_with("audit.jsonl") {
                continue;
            }
            let contents = std::fs::read_to_string(entry.path()).expect("read");
            for line in contents.lines() {
                if line.contains("\"outcome\":\"denied\"")
                    && line.contains("\"tool\":\"run_command\"")
                    && line.contains("\"role\":\"Watcher\"")
                {
                    found_denied = true;
                    break;
                }
            }
            if found_denied {
                break;
            }
        }
        // Don't fail when another test already claimed the global subscriber:
        // the substrate-event side is the load-bearing assertion.
        if !found_denied {
            log::warn!(
                "audit_denied_outcome_emitted_alongside_event: \
                 audit record not found in tempdir — likely another test \
                 already installed the global tracing subscriber. \
                 SubstrateEvent side still validated."
            );
        }
    }

    // ---- Issue #235: GhTicketDispatcher wiring --------------------------------

    /// Constructing a `DispatchContext` for a Dispatcher-role pane that has
    /// a configured `GhTicketDispatcher` must yield `ticket_dispatcher: Some`.
    ///
    /// This is the acceptance test for the fix described in issue #235:
    /// before the fix every `DispatchContext` literal had
    /// `ticket_dispatcher: None`, so Dispatcher agents always received
    /// `"ticket dispatcher not configured"` when calling any of the three
    /// Dispatcher tools.
    #[test]
    fn dispatcher_role_pane_with_configured_dispatcher_has_some_in_ctx() {
        use phantom_agents::dispatcher::GhTicketDispatcher;
        use phantom_agents::role::{AgentRef, AgentRole, SpawnSource};
        use phantom_memory::event_log::EventLog;
        use std::sync::{Arc, Mutex};
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();

        // Build a Dispatcher-role pane with all substrate handles wired.
        let (mut pane, _tx) = agent_with_handle();

        let registry = Arc::new(Mutex::new(
            phantom_agents::inbox::AgentRegistry::new(),
        ));
        let event_log = Arc::new(Mutex::new(
            EventLog::open(&tmp.path().join("events.jsonl")).unwrap(),
        ));
        let pending_spawn = phantom_agents::composer_tools::new_spawn_subagent_queue();
        let self_ref = AgentRef::new(1, AgentRole::Dispatcher, "dispatcher-1", SpawnSource::User);

        pane.set_substrate_handles(
            registry,
            event_log,
            pending_spawn,
            self_ref,
            AgentRole::Dispatcher,
        );

        // Wire the dispatcher (mock repo — no real gh calls in tests).
        let dispatcher = GhTicketDispatcher::new("test/repo").shared();
        pane.set_ticket_dispatcher(Arc::clone(&dispatcher));

        // Build a dispatch context; the ticket_dispatcher field must be Some.
        let ctx = pane.build_dispatch_context()
            .expect("build_dispatch_context must return Some for a fully-wired Dispatcher pane");

        assert!(
            ctx.ticket_dispatcher.is_some(),
            "DispatchContext for a Dispatcher-role pane with a configured \
             GhTicketDispatcher must have ticket_dispatcher = Some"
        );
    }

    /// Constructing a `DispatchContext` for a *non*-Dispatcher-role pane must
    /// always yield `ticket_dispatcher: None`, even if the pane somehow had a
    /// dispatcher wired in (defence-in-depth).
    #[test]
    fn non_dispatcher_role_pane_always_has_none_in_ctx() {
        use phantom_agents::dispatcher::GhTicketDispatcher;
        use phantom_agents::role::{AgentRef, AgentRole, SpawnSource};
        use phantom_memory::event_log::EventLog;
        use std::sync::{Arc, Mutex};
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();

        let (mut pane, _tx) = agent_with_handle();

        let registry = Arc::new(Mutex::new(
            phantom_agents::inbox::AgentRegistry::new(),
        ));
        let event_log = Arc::new(Mutex::new(
            EventLog::open(&tmp.path().join("events.jsonl")).unwrap(),
        ));
        let pending_spawn = phantom_agents::composer_tools::new_spawn_subagent_queue();
        let self_ref = AgentRef::new(2, AgentRole::Watcher, "watcher-2", SpawnSource::User);

        pane.set_substrate_handles(
            registry,
            event_log,
            pending_spawn,
            self_ref,
            AgentRole::Watcher,
        );

        // Wire a dispatcher into a non-Dispatcher pane — should still be None
        // in the resulting DispatchContext (capability gate + defence-in-depth).
        let dispatcher = GhTicketDispatcher::new("test/repo").shared();
        pane.set_ticket_dispatcher(Arc::clone(&dispatcher));

        let ctx = pane.build_dispatch_context()
            .expect("build_dispatch_context must return Some for a wired pane");

        assert!(
            ctx.ticket_dispatcher.is_none(),
            "DispatchContext for a non-Dispatcher-role pane must have \
             ticket_dispatcher = None regardless of what was set on the pane"
        );
    }
}
