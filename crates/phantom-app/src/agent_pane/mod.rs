//! Agent pane management — spawn AI agents in visible GUI panes.
//!
//! When the brain decides to spawn an agent (or the user requests one),
//! this module creates a new pane, starts a Claude API agent on a
//! background thread, and streams output into the pane each frame.

mod capture;
mod dispatch_ctx;
mod events;
mod execute;
mod journal;
mod spawn;
#[cfg(test)]
mod tests;

use std::sync::{Arc, Mutex};

use log::{info, warn};

use phantom_agents::agent::{Agent, AgentMessage};
use phantom_agents::api::{ApiEvent, ApiHandle, ClaudeConfig};
use phantom_agents::chat::{ChatBackend, ChatModel, build_backend};
use phantom_agents::permissions::PermissionSet;
use phantom_agents::role::{AgentRole, CapabilityClass};
use phantom_agents::spawn_rules::SubstrateEvent;
use phantom_agents::tools::{ToolCall, available_tools};
use phantom_agents::{AgentSpawnOpts, AgentTask};
use phantom_history::{AgentOutputCapture, ToolCall as HistoryToolCall};
use phantom_session::AgentSnapshot;

use dispatch_ctx::build_codebase_context;
use execute::format_tool_args;
use journal::open_agent_journal;

pub(crate) use events::{new_blocked_event_sink, new_denied_event_sink};

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

/// Shared queue of [`AgentSnapshot`] records pushed by agent panes when they
/// reach a terminal state (Done or Failed).
///
/// The `App` owns the canonical queue; each spawned pane is given a clone.
/// On completion the pane captures a snapshot of itself and pushes it here.
/// `App::shutdown` drains the queue and hands it to
/// [`phantom_session::AgentStateFile`] for persistence alongside the session.
pub(crate) type AgentSnapshotQueue = Arc<Mutex<Vec<AgentSnapshot>>>;

/// Construct a fresh, empty [`AgentSnapshotQueue`].
pub(crate) fn new_agent_snapshot_queue() -> AgentSnapshotQueue {
    Arc::new(Mutex::new(Vec::new()))
}

// ---------------------------------------------------------------------------
// AgentPane — a running agent with its output stream
// ---------------------------------------------------------------------------

/// An active agent running in a GUI pane.
pub(crate) struct AgentPane {
    /// The agent's task description.
    pub(super) task: String,
    /// Current status.
    pub(super) status: AgentPaneStatus,
    /// Accumulated output text (streamed from Claude API).
    pub(super) output: String,
    /// Handle to the background API thread.
    pub(super) api_handle: Option<ApiHandle>,
    /// Tool use IDs for multi-turn conversations.
    pub(super) tool_use_ids: Vec<String>,
    /// Cached tail lines for rendering (avoids re-splitting every frame).
    pub(super) cached_lines: Vec<String>,
    /// Output length at last cache rebuild.
    pub(super) cached_len: usize,
    /// The agent's conversation state (owns the message history).
    pub(super) agent: Agent,
    /// Tool calls pending execution: (api_id, call).
    pub(super) pending_tools: Vec<(String, ToolCall)>,
    /// Project root for tool sandbox.
    pub(super) working_dir: String,
    /// Claude API config for re-invoking on tool-result turns.
    ///
    /// Always present. When [`AgentPane::chat_backend`] is `None`, this is
    /// the active Claude config used by [`send_message`]. When a backend is
    /// configured (`--model`/env override), this still provides the
    /// `max_tokens` budget for [`ChatRequest`] so per-turn shaping matches
    /// the existing setup.
    pub(super) claude_config: ClaudeConfig,
    /// Optional chat backend.
    ///
    /// `None` keeps the byte-for-byte legacy Claude path: every turn calls
    /// [`send_message`] with [`AgentPane::claude_config`]. When the caller
    /// selected a [`ChatModel`] (via `--model` or `PHANTOM_AGENT_MODEL`),
    /// this is `Some` and turns dispatch through [`ChatBackend::complete`].
    pub(super) chat_backend: Option<Box<dyn ChatBackend>>,
    /// Number of tool-use rounds completed (capped at [`MAX_TOOL_ROUNDS`]).
    pub(super) turn_count: u32,
    /// Accumulator for assistant text within the current API response.
    pub(super) current_assistant_text: String,
    /// Permission set for tool execution (default: all).
    pub(super) permissions: PermissionSet,
    /// Approximate input tokens consumed.
    pub(super) input_tokens: u32,
    /// Approximate output tokens consumed.
    pub(super) output_tokens: u32,
    /// Number of tool calls executed.
    pub(super) tool_call_count: u32,
    /// Whether this agent has written/edited files (for rollback).
    pub(super) has_file_edits: bool,
    /// Consecutive tool-call failures since the last success.
    ///
    /// Reset to 0 on any successful tool result. When this reaches
    /// [`TOOL_BLOCK_THRESHOLD`], the pane emits an `EventKind::AgentBlocked`
    /// substrate event into [`AgentPane::blocked_event_sink`] and the counter
    /// is cleared so the same agent doesn't spam the bus.
    pub(super) consecutive_tool_failures: u32,
    /// Shared sink for emitted `AgentBlocked` events (Phase 2.E producer).
    ///
    /// `None` for tests/legacy callers that don't have a sink to plumb. The
    /// production spawn path (`App::spawn_agent_pane_with_opts`) always
    /// supplies the App's canonical sink; Phase 2.G will consume those events
    /// to actually spawn Fixer agents.
    pub(super) blocked_event_sink: Option<BlockedEventSink>,
    /// Shared sink for emitted `CapabilityDenied` events (Sec.1 producer).
    ///
    /// Mirrors [`AgentPane::blocked_event_sink`]: the App owns the canonical
    /// sink and hands a clone in at spawn time. Whenever the Layer-2 gate
    /// refuses a tool call, the pane pushes a [`SubstrateEvent`] of kind
    /// [`EventKind::CapabilityDenied`] here. `None` for legacy / test
    /// callers without a wired App.
    #[allow(dead_code)] // Producer for Sec.4 consumer wiring; kept ahead of time.
    pub(super) denied_event_sink: Option<DeniedEventSink>,
    /// Last error excerpt observed on a failing tool call, used to populate
    /// the `reason` field of the emitted `AgentBlocked` event.
    pub(super) last_tool_error: Option<String>,
    /// Shared queue into which the pane pushes an [`AgentSnapshot`] whenever
    /// it reaches a terminal state (Done or Failed). The `App` drains this at
    /// shutdown and persists the snapshots via [`phantom_session::AgentStateFile`].
    /// `None` for legacy/test callers that do not have a wired App.
    pub(super) snapshot_sink: Option<AgentSnapshotQueue>,
    /// Capability class of the last failing tool call.  Used to populate the
    /// `suggested_capability` field of `AgentBlocked` events so the Fixer can
    /// request the correct class instead of hardcoding `"Sense"`.
    pub(super) last_failing_capability: Option<CapabilityClass>,
    /// Agent output capture sidecar — `None` when launched without App wiring
    /// (legacy / test path). The production path sets this via
    /// [`AgentPane::set_agent_capture`] after [`AgentPane::spawn_with_opts`].
    pub(super) agent_capture: Option<AgentOutputCapture>,
    /// Session UUID forwarded from the App's `session_uuid` field. Used as the
    /// `session_id` parameter when appending to the capture sidecar.
    pub(super) capture_session_uuid: uuid::Uuid,
    /// Tool calls accumulated during the current agent turn, flushed to the
    /// sidecar on `ApiEvent::Done` or `ApiEvent::Error`.
    pub(super) capture_tool_calls: Vec<HistoryToolCall>,
    /// Shared registry handle used by chat-tool dispatch (`send_to_agent`,
    /// `broadcast_to_role`, `request_critique`). Cloned from the runtime at
    /// spawn time so dispatch contexts can read/write the same directory the
    /// substrate ticks against. `None` keeps the legacy / test path working
    /// (chat tools that need the registry will return a structured error).
    pub(super) registry: Option<Arc<Mutex<phantom_agents::inbox::AgentRegistry>>>,
    /// Shared event-log handle used by chat-tool log emission and composer
    /// tools (`wait_for_agent`, `event_log_query`, `request_critique`).
    /// Cloned from the runtime; `None` disables log-dependent tools.
    pub(super) event_log: Option<Arc<Mutex<phantom_memory::event_log::EventLog>>>,
    /// Shared sub-agent spawn queue. The Composer's `spawn_subagent` tool
    /// pushes onto this; the App's `update.rs` drains it once per frame.
    /// `None` disables `spawn_subagent` (it returns the chat-tools' standard
    /// error string).
    pub(super) pending_spawn: Option<phantom_agents::composer_tools::SpawnSubagentQueue>,
    /// Pre-allocated [`phantom_agents::role::AgentRef`] used as the calling
    /// identity in dispatch contexts. Populated at spawn time so chat-tool
    /// peers can see attribution; `None` falls back to an ephemeral
    /// `Conversational` ref synthesized per turn (legacy path).
    pub(super) self_ref: Option<phantom_agents::role::AgentRef>,
    /// Substrate role gating dispatch capability checks. Defaults to
    /// `DEFAULT_AGENT_PANE_ROLE` (Conversational) but can be overridden at
    /// spawn time so a Composer pane gets the Coordinate class it needs to
    /// invoke `spawn_subagent` / `broadcast_to_role`.
    pub(super) role: phantom_agents::role::AgentRole,
    /// Issue #235: shared `GhTicketDispatcher` handle injected at spawn time
    /// when the pane's role is `Dispatcher`.
    ///
    /// `None` for all non-Dispatcher roles and for any Dispatcher pane whose
    /// parent `App` could not construct the dispatcher (e.g. `GITHUB_TOKEN`
    /// absent). When `None`, the three Dispatcher tools return the canonical
    /// `"ticket dispatcher not configured"` error so the model self-corrects.
    pub(super) ticket_dispatcher:
        Option<Arc<phantom_agents::dispatcher::GhTicketDispatcher>>,
    /// Issue #105: runtime execution mode gate. Defaults to `Normal`.
    #[allow(dead_code)]
    pub(super) runtime_mode: phantom_agents::dispatch::RuntimeMode,
    /// Per-agent structured lifecycle journal (JSONL on disk).
    ///
    /// `None` when the journal file could not be opened (e.g., permission
    /// error or test environment without a real filesystem path). All journal
    /// writes are best-effort — a failure never aborts an agent spawn.
    pub(super) journal: Option<phantom_memory::journal::AgentJournal>,
    /// Sec.7.3 quarantine registry shared with the App orchestrator.
    ///
    /// When `Some`, [`build_dispatch_context`] passes the real registry into
    /// the [`DispatchContext`], enabling the quarantine gate in
    /// [`phantom_agents::dispatch::dispatch_tool`] to block quarantined agents
    /// before any capability check or handler runs.
    ///
    /// `None` for legacy / test callers that do not wire a quarantine registry;
    /// the gate skips the check (fail-open) so old paths are unaffected.
    pub(super) quarantine: Option<Arc<Mutex<phantom_agents::quarantine::QuarantineRegistry>>>,
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
        let task = AgentTask::FreeForm {
            prompt: "test".into(),
        };
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
            quarantine: None,
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
            snapshot_sink: None,
            last_failing_capability: None,
            ticket_dispatcher: None,
            runtime_mode: phantom_agents::dispatch::RuntimeMode::Normal,
            journal: None,
            agent_capture: None,
            capture_session_uuid: uuid::Uuid::nil(),
            capture_tool_calls: Vec::new(),
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
        // Capture the requested role before opts fields are consumed.
        let initial_role = opts.role().unwrap_or(DEFAULT_AGENT_PANE_ROLE);

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
            AgentTask::FixError {
                error_summary,
                context,
                ..
            } => {
                format!("Fix this error: {error_summary}\nContext: {context}")
            }
            AgentTask::RunCommand { command } => format!("Run: {command}"),
            AgentTask::ReviewCode { files, context } => {
                format!(
                    "Review these files: {}\nContext: {context}",
                    files.join(", ")
                )
            }
            AgentTask::WatchAndNotify { description } => {
                format!("Watch: {description}")
            }
        };
        agent.push_message(AgentMessage::User(user_prompt));

        let tools = available_tools();

        info!(
            "Agent pane spawning: {} messages (system={}, user={}, backend={})",
            agent.messages().len(),
            agent.messages().iter().filter(|m| matches!(m, AgentMessage::System(_))).count(),
            agent.messages().iter().filter(|m| matches!(m, AgentMessage::User(_))).count(),
            chat_backend.as_deref().map(|b| b.name()).unwrap_or("claude (default)"),
        );

        let handle = Self::dispatch(chat_backend.as_deref(), claude_config, &agent, &tools, &[]);

        let working_dir = std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| ".".into());

        info!("Agent pane spawned: {task_desc}");

        let agent_id = agent.id();
        let mut journal = open_agent_journal(agent_id);
        if let Some(ref mut j) = journal {
            if let Err(e) = j.record_spawn(agent_id, &task_desc) {
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
            snapshot_sink: None,
            last_failing_capability: None,
            registry: None,
            event_log: None,
            pending_spawn: None,
            self_ref: None,
            role: initial_role,
            ticket_dispatcher: None,
            runtime_mode: phantom_agents::dispatch::RuntimeMode::Normal,
            journal,
            agent_capture: None,
            capture_session_uuid: uuid::Uuid::nil(),
            capture_tool_calls: Vec::new(),
            quarantine: None,
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
        registry: Arc<Mutex<phantom_agents::inbox::AgentRegistry>>,
        event_log: Arc<Mutex<phantom_memory::event_log::EventLog>>,
        pending_spawn: phantom_agents::composer_tools::SpawnSubagentQueue,
        self_ref: phantom_agents::role::AgentRef,
        role: phantom_agents::role::AgentRole,
        quarantine: Arc<Mutex<phantom_agents::quarantine::QuarantineRegistry>>,
    ) {
        self.registry = Some(registry);
        self.event_log = Some(event_log);
        self.pending_spawn = Some(pending_spawn);
        self.self_ref = Some(self_ref);
        self.role = role;
        self.quarantine = Some(quarantine);
    }

    /// Wire the shared snapshot queue so this pane pushes an [`AgentSnapshot`]
    /// into it whenever it reaches a terminal state (Done or Failed).
    ///
    /// Called by `App::spawn_agent_pane_with_opts` after construction so the
    /// App's canonical queue receives the snapshot at completion time.
    pub(crate) fn set_snapshot_sink(&mut self, sink: AgentSnapshotQueue) {
        self.snapshot_sink = Some(sink);
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
        dispatcher: Arc<phantom_agents::dispatcher::GhTicketDispatcher>,
    ) {
        self.ticket_dispatcher = Some(dispatcher);
    }

    /// Wire the history capture sidecar into this pane.
    ///
    /// Called by `App::spawn_agent_pane_with_opts` after `spawn_with_opts`.
    /// When `None` is held (legacy / test path), tool calls and output are
    /// not recorded.
    pub(crate) fn set_agent_capture(&mut self, capture: AgentOutputCapture, session_uuid: uuid::Uuid) {
        self.agent_capture = Some(capture);
        self.capture_session_uuid = session_uuid;
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
                            if let Err(e) = j.record_output(self.agent.id() as u64, first_line) {
                                warn!("AgentJournal::record_output failed: {e}");
                            }
                        }
                    }
                    // Cap output to prevent unbounded memory growth.
                    if self.output.len() > 65536 {
                        let mut trim = self.output.len() - 65536;
                        while trim < self.output.len() && !self.output.is_char_boundary(trim) {
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
                        if let Err(e) = j.record_tool_call(self.agent.id() as u64, call.tool.api_name(), &args_display) {
                            warn!("AgentJournal::record_tool_call failed: {e}");
                        }
                    }
                    if args_display.is_empty() {
                        self.output
                            .push_str(&format!("\n▶ {}\n", call.tool.api_name()));
                    } else {
                        self.output.push_str(&format!(
                            "\n▶ {} {}\n",
                            call.tool.api_name(),
                            args_display
                        ));
                    }
                    // Record for the capture sidecar flush on Done/Error.
                    let args_json = serde_json::to_string(&call.args).unwrap_or_default();
                    self.capture_tool_calls.push(HistoryToolCall::new(
                        call.tool.api_name(),
                        args_json,
                        None,
                    ));
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
                            if let Err(e) = j.record_completion(self.agent.id() as u64, true, summary) {
                                warn!("AgentJournal::record_completion failed: {e}");
                            }
                        }
                        self.output.push_str(&format!(
                            "\n\n📊 ~{}in / ~{}out tokens | {} tool calls\n✓ Agent finished.\n",
                            self.input_tokens, self.output_tokens, self.tool_call_count,
                        ));
                        self.status = AgentPaneStatus::Done;
                        self.api_handle = None;
                        self.push_snapshot();
                        self.flush_capture_record();
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
                        if let Err(je) = j.record_flatline(self.agent.id() as u64, &e) {
                            warn!("AgentJournal::record_flatline failed: {je}");
                        }
                    }
                    self.rollback_if_dirty();
                    self.flush_capture_record();
                    self.status = AgentPaneStatus::Failed;
                    self.api_handle = None;
                    self.push_snapshot();
                    self.save_conversation();
                    got_content = true;
                    break;
                }
                None => break,
            }
        }

        got_content
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
            self.last_failing_capability = None;
        } else {
            self.consecutive_tool_failures = self.consecutive_tool_failures.saturating_add(1);
            self.last_tool_error = Some(error_excerpt.to_string());
            // Tests that only exercise the streak logic (not capability routing)
            // leave `last_failing_capability` unset — `maybe_emit_blocked_event`
            // will fall back to `"Sense"` which is acceptable for those tests.
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
        self.output
            .push_str(&format!("\n\n> {message}\n\n● Thinking...\n"));

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

    /// Save the agent conversation to disk for debugging and replay.
    pub(crate) fn save_conversation(&self) {
        let dir = std::env::var("HOME")
            .map(|h| std::path::PathBuf::from(h).join(".config/phantom/agents"))
            .unwrap_or_else(|_| std::path::PathBuf::from("/tmp/phantom-agents"));

        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }

        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let sanitized: String = self
            .task
            .chars()
            .take(30)
            .map(|c| {
                if c.is_alphanumeric() || c == '-' {
                    c
                } else {
                    '_'
                }
            })
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
// API-key resolution per backend
// ---------------------------------------------------------------------------

/// Check that the correct API key is present for the requested `model`.
///
/// * `ChatModel::Claude(_)` → `ANTHROPIC_API_KEY` must be set
/// * `ChatModel::OpenAi(_)` → `OPENAI_API_KEY` must be set
///
/// Returns `Some(ClaudeConfig)` on success (for OpenAI, a placeholder config
/// is returned because only `max_tokens` is used downstream; the real OpenAI
/// key lives inside the `ChatBackend` built by `build_backend`).
/// Returns `None` and emits a `warn!` when the required key is missing.
pub(crate) fn resolve_api_config(model: &ChatModel) -> Option<ClaudeConfig> {
    match model {
        ChatModel::Claude(_) => {
            let config = ClaudeConfig::from_env();
            if config.is_none() {
                warn!("Cannot spawn agent: ANTHROPIC_API_KEY not set");
            }
            config
        }
        ChatModel::OpenAi(_) => {
            let key = std::env::var("OPENAI_API_KEY").ok().filter(|k| !k.is_empty());
            if key.is_none() {
                warn!("Cannot spawn agent: OPENAI_API_KEY not set");
            }
            key.map(|_| ClaudeConfig::new("__openai__"))
        }
    }
}
