//! Agent struct and lifecycle management.
//!
//! Each [`Agent`] represents an autonomous AI worker that runs in its own
//! terminal pane. Agents carry a conversation history, a task description,
//! and a visible output log that the renderer streams into the pane.

use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

use crate::correlation::CorrelationId;
use crate::dispatch::Disposition;
use crate::semantic_context::SemanticContext;
use crate::tools::{ToolCall, ToolResult};

// ---------------------------------------------------------------------------
// AgentId
// ---------------------------------------------------------------------------

/// Unique agent identifier (monotonically increasing within a session).
///
/// Widened to `u64` (matches [`crate::role::AgentId`] and
/// [`crate::quarantine::QuarantineRegistry`]) so no narrowing cast is required
/// at the dispatch / quarantine boundary. Fixes issue #273.
pub type AgentId = u64;

// ---------------------------------------------------------------------------
// AgentStatus
// ---------------------------------------------------------------------------

/// Agent lifecycle state.
///
/// The full FSM (see issue #34):
///
/// ```text
/// Queued → Planning → AwaitingApproval → Working → Done
///                   ↘ (revision)  ↗         ↓
///                                          Failed → Queued (retry)
///                                            ↓
///                                         Flatline → Queued (manual retry)
/// ```
///
/// `Queued` may also skip straight to `Working` when there is no plan gate
/// in effect (the existing fast path for non-gated tasks).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentStatus {
    /// Waiting to start (queued behind concurrency limit).
    Queued,
    /// Generating an execution plan before requesting user approval.
    /// Entered from `Queued` when the Plan Gate is active.
    Planning,
    /// Plan produced; waiting for the user (or policy) to approve it before
    /// the agent starts executing tools. Entered from `Planning`.
    AwaitingApproval,
    /// Actively processing / reasoning.
    Working,
    /// Called a tool, waiting for the result.
    WaitingForTool,
    /// Completed successfully.
    Done,
    /// Completed with an error.
    Failed,
    /// Terminal failure requiring manual retry. Reason stored in Agent::flatline_reason.
    Flatline,
}

impl AgentStatus {
    /// Returns `true` iff transitioning from `self` to `next` is a valid
    /// lifecycle move. Invalid transitions indicate a bug in the caller.
    ///
    /// Full valid-transition table:
    ///
    /// | From              | To                |
    /// |-------------------|-------------------|
    /// | Queued            | Planning          |
    /// | Queued            | Working           | (no-gate fast path)
    /// | Planning          | AwaitingApproval  |
    /// | Planning          | Working           | (plan auto-approved)
    /// | AwaitingApproval  | Working           | (approved by user/policy)
    /// | AwaitingApproval  | Planning          | (revision requested)
    /// | Working           | WaitingForTool    |
    /// | Working           | Done              |
    /// | Working           | Failed            |
    /// | Working           | Flatline          |
    /// | WaitingForTool    | Working           |
    /// | WaitingForTool    | Failed            |
    /// | WaitingForTool    | Flatline          |
    /// | Failed            | Queued            | (retry)
    /// | Flatline          | Queued            | (manual retry)
    pub fn can_transition_to(self, next: AgentStatus) -> bool {
        use AgentStatus::*;
        matches!(
            (self, next),
            (Queued, Planning)                        // plan gate enters planning phase
                | (Queued, Working)                   // no-gate fast path
                | (Planning, AwaitingApproval)        // plan ready, awaiting user sign-off
                | (Planning, Working)                 // plan auto-approved (policy/test)
                | (AwaitingApproval, Working)         // user/policy approved
                | (AwaitingApproval, Planning)        // revision requested — re-plan
                | (Working, WaitingForTool)
                | (Working, Done)
                | (Working, Failed)
                | (Working, Flatline)
                | (WaitingForTool, Working)
                | (WaitingForTool, Failed)
                | (WaitingForTool, Flatline)
                | (Failed, Queued)                    // retry
                | (Flatline, Queued)                  // manual retry
        )
    }

    /// Returns `true` if the agent is in a terminal state that cannot advance
    /// without an explicit external trigger (retry, user action, etc.).
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, AgentStatus::Done | AgentStatus::Failed | AgentStatus::Flatline)
    }

    /// Returns `true` if the status represents active forward progress (working
    /// or waiting for a tool result). Used by the concurrency gate in
    /// [`crate::manager::AgentManager`].
    #[must_use]
    pub fn is_active(self) -> bool {
        matches!(self, AgentStatus::Working | AgentStatus::WaitingForTool)
    }
}

// ---------------------------------------------------------------------------
// AgentTask
// ---------------------------------------------------------------------------

/// What kind of task the agent is performing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentTask {
    /// Fix a compiler/runtime error.
    FixError {
        error_summary: String,
        file: Option<String>,
        context: String,
    },
    /// Run a shell command and report results.
    RunCommand { command: String },
    /// Review code in the given files.
    ReviewCode {
        files: Vec<String>,
        context: String,
    },
    /// Open-ended prompt (user-defined task).
    FreeForm { prompt: String },
    /// Watch a condition and notify when it changes.
    WatchAndNotify { description: String },
}

// ---------------------------------------------------------------------------
// AgentMessage
// ---------------------------------------------------------------------------

/// A message in the agent's conversation history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentMessage {
    /// System prompt establishing the agent's role.
    System(String),
    /// User/task input.
    User(String),
    /// AI assistant response.
    Assistant(String),
    /// Agent wants to invoke a tool.
    ToolCall(ToolCall),
    /// Result returned from a tool execution.
    ToolResult(ToolResult),
}

// ---------------------------------------------------------------------------
// Agent
// ---------------------------------------------------------------------------

/// An AI agent that works in its own terminal pane context.
#[derive(Debug)]
pub struct Agent {
    /// Unique identifier.
    id: AgentId,
    /// The task this agent was spawned to perform.
    task: AgentTask,
    /// Intent classification. Read by [`Agent::try_auto_approve`] to decide
    /// whether to skip `AwaitingApproval` (Issue #49 auto-approve fast path).
    disposition: Disposition,
    /// Current lifecycle status.
    status: AgentStatus,
    /// Full conversation history (system + user + assistant + tools).
    messages: Vec<AgentMessage>,
    /// Visible output lines shown in the agent pane.
    output_log: Vec<String>,
    /// When this agent was created.
    created_at: Instant,
    /// When this agent finished (if it has).
    completed_at: Option<Instant>,
    /// Reason for entering Flatline state. Set by `flatline()`.
    flatline_reason: Option<String>,
    /// Causality token linking this agent to the pipeline run that spawned it.
    ///
    /// All agents in the same pipeline share a single [`CorrelationId`] so that
    /// every tool call, log entry, and event they produce can be queried as a
    /// unit: `WHERE correlation_id = ?`.
    ///
    /// `None` for agents spawned outside a tracked pipeline (e.g. direct
    /// user-initiated spawns that pre-date the tracing infrastructure).
    correlation_id: Option<CorrelationId>,
    /// The [`AgentId`] of the agent that directly spawned this one, if any.
    ///
    /// `None` for top-level agents spawned by the user or the substrate.
    /// Set by the Composer when it calls `spawn_subagent` so the parent–child
    /// relationship is recoverable at query time.
    parent_id: Option<AgentId>,
    /// Ring-buffer of structured outputs from recent `RunCommand` tool calls.
    ///
    /// After every `RunCommand` result, the caller is expected to push the
    /// `ParsedOutput` into this context via [`Agent::push_semantic_output`].
    /// The context is rendered into agent system prompts via
    /// [`Agent::semantic_prompt_section`] so the model can reason about
    /// structured command history rather than raw terminal text.
    semantic_ctx: SemanticContext,
}

impl Agent {
    /// Create a new agent in `Queued` status with no correlation context.
    ///
    /// Use [`Agent::with_correlation`] when spawning inside a tracked pipeline
    /// run to propagate the causality token.
    pub fn new(id: AgentId, task: AgentTask) -> Self {
        Self {
            id,
            task,
            disposition: Disposition::default(),
            status: AgentStatus::Queued,
            messages: Vec::new(),
            output_log: Vec::new(),
            created_at: Instant::now(),
            completed_at: None,
            flatline_reason: None,
            correlation_id: None,
            parent_id: None,
            semantic_ctx: SemanticContext::new(),
        }
    }

    /// Create a new agent with an explicit [`Disposition`].
    #[must_use]
    pub fn with_disposition(id: AgentId, task: AgentTask, disposition: Disposition) -> Self {
        Self { disposition, ..Self::new(id, task) }
    }

    /// Create a new agent in `Queued` status and stamp it with the given
    /// correlation context.
    ///
    /// All agents in the same pipeline run should share the same
    /// `correlation_id`. `parent_id` should be the id of the Composer (or
    /// other spawner) that issued the `spawn_subagent` call.
    #[must_use]
    pub fn with_correlation(
        id: AgentId,
        task: AgentTask,
        correlation_id: CorrelationId,
        parent_id: Option<AgentId>,
    ) -> Self {
        Self {
            correlation_id: Some(correlation_id),
            parent_id,
            ..Self::new(id, task)
        }
    }

    /// Return the unique identifier for this agent.
    #[must_use]
    pub fn id(&self) -> AgentId {
        self.id
    }

    /// Return the task this agent was spawned to perform.
    #[must_use]
    pub fn task(&self) -> &AgentTask {
        &self.task
    }

    /// Return the current lifecycle status.
    #[must_use]
    pub fn status(&self) -> AgentStatus {
        self.status
    }

    /// Set the lifecycle status directly, bypassing FSM guards.
    ///
    /// Prefer the FSM transition helpers ([`Agent::approve_plan`],
    /// [`Agent::complete`], etc.) for well-guarded transitions.
    /// This escape hatch exists only for internal infrastructure code
    /// (manager promotion, render-loop bookkeeping) that needs to force a
    /// specific state without going through the full FSM.
    pub(crate) fn set_status(&mut self, status: AgentStatus) {
        self.status = status;
    }

    /// Return the full conversation history.
    #[must_use]
    pub fn messages(&self) -> &[AgentMessage] {
        &self.messages
    }

    /// Return the visible output lines shown in the agent pane.
    #[must_use]
    pub fn output_log(&self) -> &[String] {
        &self.output_log
    }

    /// Return the time at which this agent completed, if it has.
    #[must_use]
    pub fn completed_at(&self) -> Option<Instant> {
        self.completed_at
    }

    /// Return the reason for entering Flatline state, if set.
    #[must_use]
    pub fn flatline_reason(&self) -> Option<&str> {
        self.flatline_reason.as_deref()
    }

    /// Return the correlation id for this agent, if set.
    #[must_use]
    pub fn correlation_id(&self) -> Option<CorrelationId> {
        self.correlation_id
    }

    /// Return the parent agent id, if this agent was spawned by another agent.
    #[must_use]
    pub fn parent_id(&self) -> Option<AgentId> {
        self.parent_id
    }

    /// Return the spawn-intent classification for this agent.
    #[must_use]
    pub fn disposition(&self) -> Disposition {
        self.disposition
    }

    /// Push a `ParsedOutput` from a `RunCommand` tool result into the
    /// agent's semantic ring-buffer.
    ///
    /// Called by `AgentPane::execute_pending_tools` immediately after a
    /// `RunCommand` result is received. The ring-buffer caps at 10 entries
    /// (FIFO eviction of the oldest).
    pub fn push_semantic_output(&mut self, parsed: phantom_semantic::ParsedOutput) {
        self.semantic_ctx.push(parsed);
    }

    /// Render the agent's semantic context as a Markdown section.
    ///
    /// Returns `None` when no `RunCommand` results have been pushed yet
    /// (i.e. the ring-buffer is empty), so callers can skip injection.
    pub fn semantic_prompt_section(&self) -> Option<String> {
        self.semantic_ctx.as_prompt_section()
    }

    /// Immutable access to the agent's semantic context.
    pub fn semantic_ctx(&self) -> &SemanticContext {
        &self.semantic_ctx
    }

    /// Append a message to the conversation history.
    pub fn push_message(&mut self, msg: AgentMessage) {
        self.messages.push(msg);
    }

    /// Walk back through the message history and collect the
    /// `source_event_id` of the most-recent `depth` `ToolResult` messages.
    ///
    /// The vec is ordered most-recent-first, so callers can read it as the
    /// "input chain" for a decision: `[id_n, id_(n-1), …]`. Only `ToolResult`
    /// messages contribute; `User`, `Assistant`, `System`, and `ToolCall`
    /// messages are skipped. `ToolResult`s with `source_event_id == None`
    /// are skipped as well — they don't contribute a substrate event id.
    ///
    /// This is what Sec.1's `EventKind::CapabilityDenied { source_chain, .. }`
    /// will populate at dispatch time. Today the field defaults to an empty
    /// `Vec<u64>` because tool results carried no provenance; Sec.2 fills it
    /// in by calling `agent.source_chain_for_last_call(3)`.
    #[must_use]
    pub fn source_chain_for_last_call(&self, depth: usize) -> Vec<u64> {
        let mut chain = Vec::with_capacity(depth);
        for msg in self.messages.iter().rev() {
            if chain.len() >= depth {
                break;
            }
            if let AgentMessage::ToolResult(tr) = msg {
                if let Some(id) = tr.source_event_id {
                    chain.push(id);
                }
            }
        }
        chain
    }

    /// Append visible output text (shown in the agent pane).
    pub fn log(&mut self, text: &str) {
        self.output_log.push(text.to_owned());
    }

    /// Fast-path entry from `Queued` → `Working` for auto-approvable dispositions.
    ///
    /// When `self.disposition.auto_approve()` is `true` (i.e. `Chat`,
    /// `Synthesize`, `Decompose`, or `Audit`), the agent skips the
    /// `Planning → AwaitingApproval` gate and goes directly to `Working`.
    /// Write-side dispositions (`Feature`, `BugFix`, `Refactor`, `Chore`)
    /// return `false` so the caller falls through to the normal plan gate.
    ///
    /// Returns `true` if the agent is now `Working`, `false` if the
    /// disposition is not auto-approvable or the FSM transition failed.
    #[must_use]
    pub fn try_auto_approve(&mut self) -> bool {
        if !self.disposition.auto_approve() {
            return false;
        }
        // Queued → Working is a valid FSM transition (the no-gate fast path).
        self.approve_plan()
    }

    /// Transition into the `Planning` state.
    ///
    /// Valid only from `Queued`. Returns `true` on success, `false` if the
    /// current state does not permit this transition (no-op on failure so the
    /// caller can decide how to handle the invalid sequence).
    pub fn begin_planning(&mut self) -> bool {
        if self.status.can_transition_to(AgentStatus::Planning) {
            self.status = AgentStatus::Planning;
            true
        } else {
            false
        }
    }

    /// Transition into `AwaitingApproval` once the plan has been generated.
    ///
    /// Valid only from `Planning`. Returns `true` on success.
    pub fn submit_plan_for_approval(&mut self) -> bool {
        if self.status.can_transition_to(AgentStatus::AwaitingApproval) {
            self.status = AgentStatus::AwaitingApproval;
            true
        } else {
            false
        }
    }

    /// Approve the plan and transition to `Working`.
    ///
    /// Valid from `Planning` (auto-approve) or `AwaitingApproval` (user/policy
    /// approve). Returns `true` on success.
    pub fn approve_plan(&mut self) -> bool {
        if self.status.can_transition_to(AgentStatus::Working) {
            self.status = AgentStatus::Working;
            true
        } else {
            false
        }
    }

    /// Request plan revision — transitions from `AwaitingApproval` back to
    /// `Planning` so the agent can regenerate the plan.
    ///
    /// Returns `true` on success.
    pub fn request_revision(&mut self) -> bool {
        if self.status.can_transition_to(AgentStatus::Planning) {
            self.status = AgentStatus::Planning;
            true
        } else {
            false
        }
    }

    /// Mark the agent as completed.
    pub fn complete(&mut self, success: bool) {
        self.status = if success {
            AgentStatus::Done
        } else {
            AgentStatus::Failed
        };
        self.completed_at = Some(Instant::now());
    }

    /// Transition to Flatline — terminal failure requiring manual retry.
    ///
    /// Flatline is intentionally terminal: unlike Failed, it does not
    /// auto-retry. The user must explicitly call `retry()` to re-queue.
    pub fn flatline(&mut self, reason: impl Into<String>) {
        self.status = AgentStatus::Flatline;
        self.flatline_reason = Some(reason.into());
        self.completed_at = Some(Instant::now());
    }

    /// Reset a flatlined agent back to Queued for manual retry.
    pub fn retry(&mut self) {
        debug_assert_eq!(self.status, AgentStatus::Flatline, "retry() called on non-flatlined agent");
        self.status = AgentStatus::Queued;
        self.flatline_reason = None;
        self.completed_at = None;
    }

    /// Duration since creation.
    pub fn elapsed(&self) -> Duration {
        self.created_at.elapsed()
    }

    /// Build the system prompt based on the task type.
    ///
    /// This prompt is sent as the first message when the agent begins work.
    /// It establishes the agent's role, constraints, and expected output format.
    pub fn system_prompt(&self) -> String {
        let skill_hint = "\n\nCheck project memory for relevant skills before starting.";

        match &self.task {
            AgentTask::FixError {
                error_summary,
                file,
                context,
            } => {
                let file_hint = file
                    .as_deref()
                    .map(|f| format!(" The error is in `{f}`."))
                    .unwrap_or_default();
                format!(
                    "You are a code repair agent in the Phantom terminal.\n\
                     Your job: fix the following error and verify the fix compiles.\n\n\
                     Error: {error_summary}\n\
                     {file_hint}\n\
                     Context: {context}\n\n\
                     Steps:\n\
                     1. Read the relevant file(s).\n\
                     2. Identify the root cause.\n\
                     3. Write the fix.\n\
                     4. Run the build to verify.\n\
                     5. Report what you changed and why.\
                     {skill_hint}"
                )
            }
            AgentTask::RunCommand { command } => {
                format!(
                    "You are a command execution agent in the Phantom terminal.\n\
                     Run the following command, observe the output, and report the results.\n\n\
                     Command: `{command}`\n\n\
                     If the command fails, analyze the error and suggest a fix.\
                     {skill_hint}"
                )
            }
            AgentTask::ReviewCode { files, context } => {
                let file_list = files.join(", ");
                format!(
                    "You are a code review agent in the Phantom terminal.\n\
                     Review the following files for bugs, style issues, and improvements.\n\n\
                     Files: {file_list}\n\
                     Context: {context}\n\n\
                     For each issue found, state the file, line, severity, and suggested fix.\
                     {skill_hint}"
                )
            }
            AgentTask::FreeForm { prompt } => {
                format!(
                    "You are an AI assistant agent in the Phantom terminal.\n\
                     You have access to file and command tools in the project directory.\n\n\
                     Task: {prompt}\
                     {skill_hint}"
                )
            }
            AgentTask::WatchAndNotify { description } => {
                format!(
                    "You are a monitoring agent in the Phantom terminal.\n\
                     Watch the following condition and notify when it changes.\n\n\
                     Watch: {description}\n\n\
                     Periodically check the condition using available tools. \
                     When a change is detected, report it clearly.\
                     {skill_hint}"
                )
            }
        }
    }

    /// Serialize the agent's conversation to JSON for persistence.
    ///
    /// Used by the agent pane to save completed conversations to disk for
    /// debugging and replay.
    pub fn to_json(&self) -> serde_json::Value {
        let messages: Vec<serde_json::Value> = self
            .messages
            .iter()
            .map(|m| match m {
                AgentMessage::System(s) => {
                    serde_json::json!({"role": "system", "content": s})
                }
                AgentMessage::User(s) => {
                    serde_json::json!({"role": "user", "content": s})
                }
                AgentMessage::Assistant(s) => {
                    serde_json::json!({"role": "assistant", "content": s})
                }
                AgentMessage::ToolCall(tc) => {
                    serde_json::json!({
                        "role": "tool_call",
                        "tool": tc.tool.api_name(),
                        "args": tc.args,
                    })
                }
                AgentMessage::ToolResult(tr) => {
                    serde_json::json!({
                        "role": "tool_result",
                        "tool": tr.tool.api_name(),
                        "success": tr.success,
                        "output": tr.output,
                    })
                }
            })
            .collect();

        serde_json::json!({
            "task": format!("{:?}", self.task),
            "status": format!("{:?}", self.status),
            "messages": messages,
            "created_at": self.created_at.elapsed().as_secs(),
        })
    }

    /// Get a one-line status description for display in the UI.
    pub fn status_line(&self) -> String {
        let task_summary = match &self.task {
            AgentTask::FixError { error_summary, .. } => {
                let truncated = truncate(error_summary, 40);
                format!("fix: {truncated}")
            }
            AgentTask::RunCommand { command } => {
                let truncated = truncate(command, 40);
                format!("run: {truncated}")
            }
            AgentTask::ReviewCode { files, .. } => {
                format!("review: {} file(s)", files.len())
            }
            AgentTask::FreeForm { prompt } => {
                let truncated = truncate(prompt, 40);
                format!("task: {truncated}")
            }
            AgentTask::WatchAndNotify { description } => {
                let truncated = truncate(description, 40);
                format!("watch: {truncated}")
            }
        };

        let status_tag = match self.status {
            AgentStatus::Queued => "QUEUED",
            AgentStatus::Planning => "PLANNING",
            AgentStatus::AwaitingApproval => "PENDING APPROVAL",
            AgentStatus::Working => "WORKING",
            AgentStatus::WaitingForTool => "WAITING",
            AgentStatus::Done => "DONE",
            AgentStatus::Failed => "FAILED",
            AgentStatus::Flatline => "FLATLINE",
        };

        let elapsed = self.elapsed();
        format!(
            "[agent-{}] [{status_tag}] {task_summary} ({:.1}s)",
            self.id,
            elapsed.as_secs_f64()
        )
    }
}

/// Truncate a string to `max_len` characters, appending "..." if truncated.
fn truncate(s: &str, max_len: usize) -> String {
    if s.len() > max_len {
        format!("{}...", &s[..max_len])
    } else {
        s.to_owned()
    }
}

// ---------------------------------------------------------------------------
// AgentSpawnOpts
// ---------------------------------------------------------------------------

/// Options the GUI passes when constructing an agent pane.
///
/// Decoupled from the `AgentTask` itself so callers can layer additional
/// metadata (target chat backend, role override, parent agent id, label)
/// without bloating every `AgentTask` variant.
///
/// The minimal Phase-2 surface only carries `task` + `chat_model`; later
/// phases extend the struct with role / label / parent-id without breaking
/// existing call sites because all-but-`task` is `Option<…>` and `new` defaults
/// them to `None`.
#[derive(Debug, Clone)]
pub struct AgentSpawnOpts {
    /// What the agent is being spawned to do.
    pub task: AgentTask,
    /// Intent classification forwarded to the spawned [`Agent`].
    /// Read by [`Agent::try_auto_approve`] to determine if the agent should
    /// skip `AwaitingApproval` (Issue #49).
    disposition: Disposition,
    /// Optional chat-model override. `None` falls through to the env-var
    /// resolver (`PHANTOM_AGENT_MODEL`), and ultimately to default Claude.
    pub chat_model: Option<crate::chat::ChatModel>,
    /// Reconciler-issued spawn tag used to correlate `AgentComplete` events
    /// back to the correct `active_dispatches` entry. `None` for
    /// user-initiated spawns that are not tracked by the reconciler.
    pub spawn_tag: Option<u64>,
    /// Role the spawned agent should run under. `None` falls back to the pane
    /// default (`DEFAULT_AGENT_PANE_ROLE`, currently `Conversational`).
    ///
    /// Previously silently dropped from `SpawnSubagentRequest` — wired in for #224.
    role: Option<crate::role::AgentRole>,
    /// User-visible display label for the agent. `None` falls back to the
    /// generic `"agent-pane"` label assigned at substrate-handle wiring time.
    ///
    /// Previously silently dropped from `SpawnSubagentRequest` — wired in for #224.
    label: Option<String>,
}

impl AgentSpawnOpts {
    /// Build a new options bundle for `task`. Chat model and spawn tag default
    /// to `None` (env-var or default Claude path; non-reconciler spawn).
    #[must_use]
    pub fn new(task: AgentTask) -> Self {
        Self {
            task,
            disposition: Disposition::default(),
            chat_model: None,
            spawn_tag: None,
            role: None,
            label: None,
        }
    }

    /// Override the agent role. Replaces the pane default (`Conversational`).
    #[must_use]
    pub fn with_role(mut self, role: crate::role::AgentRole) -> Self {
        self.role = Some(role);
        self
    }

    /// Set the user-visible label for this agent pane.
    #[must_use]
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// Override the chat model.
    #[must_use]
    pub fn with_chat_model(mut self, model: crate::chat::ChatModel) -> Self {
        self.chat_model = Some(model);
        self
    }

    /// Tag this spawn for the reconciler.
    #[must_use]
    pub fn with_spawn_tag(mut self, tag: u64) -> Self {
        self.spawn_tag = Some(tag);
        self
    }

    /// Set the intent disposition for this spawn.
    #[must_use]
    pub fn with_disposition(mut self, d: Disposition) -> Self {
        self.disposition = d;
        self
    }

    /// Return the role override, if any.
    #[must_use]
    pub fn role(&self) -> Option<crate::role::AgentRole> {
        self.role
    }

    /// Return the label override, if any.
    #[must_use]
    pub fn label(&self) -> Option<&str> {
        self.label.as_deref()
    }



    /// Resolve the effective chat model for this spawn.
    ///
    /// Resolution order:
    /// 1. The explicit `chat_model` field if `Some(_)`.
    /// 2. The `PHANTOM_AGENT_MODEL` environment variable, parsed via
    ///    [`crate::chat::ChatModel::from_env_str`].
    /// 3. Default to [`crate::chat::ChatModel::default`] (Claude).
    #[must_use]
    pub fn resolve_model(&self) -> crate::chat::ChatModel {
        if let Some(ref m) = self.chat_model {
            return m.clone();
        }
        if let Ok(s) = std::env::var("PHANTOM_AGENT_MODEL") {
            if let Some(m) = crate::chat::ChatModel::from_env_str(&s) {
                return m;
            }
        }
        crate::chat::ChatModel::default()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolType;

    #[test]
    fn new_agent_starts_queued() {
        let agent = Agent::new(
            1,
            AgentTask::FreeForm {
                prompt: "hello".into(),
            },
        );
        assert_eq!(agent.id(), 1);
        assert_eq!(agent.status(), AgentStatus::Queued);
        assert!(agent.messages().is_empty());
        assert!(agent.output_log().is_empty());
        assert!(agent.completed_at().is_none());
    }

    #[test]
    fn push_message_appends() {
        let mut agent = Agent::new(
            1,
            AgentTask::FreeForm {
                prompt: "test".into(),
            },
        );
        agent.push_message(AgentMessage::User("hello".into()));
        agent.push_message(AgentMessage::Assistant("hi".into()));
        assert_eq!(agent.messages().len(), 2);
    }

    #[test]
    fn log_appends_output() {
        let mut agent = Agent::new(
            1,
            AgentTask::FreeForm {
                prompt: "test".into(),
            },
        );
        agent.log("line 1");
        agent.log("line 2");
        assert_eq!(agent.output_log(), vec!["line 1", "line 2"]);
    }

    #[test]
    fn complete_success_sets_done() {
        let mut agent = Agent::new(
            1,
            AgentTask::FreeForm {
                prompt: "test".into(),
            },
        );
        agent.complete(true);
        assert_eq!(agent.status(), AgentStatus::Done);
        assert!(agent.completed_at().is_some());
    }

    #[test]
    fn complete_failure_sets_failed() {
        let mut agent = Agent::new(
            1,
            AgentTask::FreeForm {
                prompt: "test".into(),
            },
        );
        agent.complete(false);
        assert_eq!(agent.status(), AgentStatus::Failed);
        assert!(agent.completed_at().is_some());
    }

    #[test]
    fn elapsed_returns_duration() {
        let agent = Agent::new(
            1,
            AgentTask::FreeForm {
                prompt: "test".into(),
            },
        );
        // Elapsed should be very small but valid.
        let _ = agent.elapsed();
    }

    #[test]
    fn system_prompt_fix_error_includes_summary() {
        let agent = Agent::new(
            1,
            AgentTask::FixError {
                error_summary: "mismatched types".into(),
                file: Some("src/main.rs".into()),
                context: "cargo build".into(),
            },
        );
        let prompt = agent.system_prompt();
        assert!(prompt.contains("mismatched types"));
        assert!(prompt.contains("src/main.rs"));
        assert!(prompt.contains("code repair agent"));
    }

    #[test]
    fn system_prompt_fix_error_without_file() {
        let agent = Agent::new(
            1,
            AgentTask::FixError {
                error_summary: "segfault".into(),
                file: None,
                context: "runtime crash".into(),
            },
        );
        let prompt = agent.system_prompt();
        assert!(prompt.contains("segfault"));
        // No file hint should appear.
        assert!(!prompt.contains("The error is in"));
    }

    #[test]
    fn system_prompt_run_command() {
        let agent = Agent::new(
            1,
            AgentTask::RunCommand {
                command: "cargo test".into(),
            },
        );
        let prompt = agent.system_prompt();
        assert!(prompt.contains("cargo test"));
        assert!(prompt.contains("command execution agent"));
    }

    #[test]
    fn system_prompt_review_code() {
        let agent = Agent::new(
            1,
            AgentTask::ReviewCode {
                files: vec!["src/lib.rs".into(), "src/main.rs".into()],
                context: "pre-merge review".into(),
            },
        );
        let prompt = agent.system_prompt();
        assert!(prompt.contains("src/lib.rs"));
        assert!(prompt.contains("code review agent"));
    }

    #[test]
    fn system_prompt_freeform() {
        let agent = Agent::new(
            1,
            AgentTask::FreeForm {
                prompt: "refactor the parser".into(),
            },
        );
        let prompt = agent.system_prompt();
        assert!(prompt.contains("refactor the parser"));
    }

    #[test]
    fn system_prompt_watch() {
        let agent = Agent::new(
            1,
            AgentTask::WatchAndNotify {
                description: "CI pipeline status".into(),
            },
        );
        let prompt = agent.system_prompt();
        assert!(prompt.contains("CI pipeline status"));
        assert!(prompt.contains("monitoring agent"));
    }

    #[test]
    fn status_line_contains_id_and_status() {
        let agent = Agent::new(
            42,
            AgentTask::FreeForm {
                prompt: "do something".into(),
            },
        );
        let line = agent.status_line();
        assert!(line.contains("agent-42"));
        assert!(line.contains("QUEUED"));
        assert!(line.contains("task:"));
    }

    #[test]
    fn status_line_truncates_long_prompt() {
        let long_prompt = "a".repeat(100);
        let agent = Agent::new(
            1,
            AgentTask::FreeForm {
                prompt: long_prompt,
            },
        );
        let line = agent.status_line();
        assert!(line.contains("..."));
    }

    #[test]
    fn status_line_review_shows_file_count() {
        let agent = Agent::new(
            1,
            AgentTask::ReviewCode {
                files: vec!["a.rs".into(), "b.rs".into(), "c.rs".into()],
                context: "test".into(),
            },
        );
        let line = agent.status_line();
        assert!(line.contains("review: 3 file(s)"));
    }

    #[test]
    fn to_json_includes_all_message_types() {
        let mut agent = Agent::new(
            1,
            AgentTask::FreeForm {
                prompt: "test task".into(),
            },
        );
        agent.push_message(AgentMessage::System("system prompt".into()));
        agent.push_message(AgentMessage::User("user input".into()));
        agent.push_message(AgentMessage::Assistant("assistant response".into()));
        agent.push_message(AgentMessage::ToolCall(ToolCall {
            tool: ToolType::ReadFile,
            args: serde_json::json!({"path": "test.txt"}),
        }));
        agent.push_message(AgentMessage::ToolResult(ToolResult {
            tool: ToolType::ReadFile,
            success: true,
            output: "file contents".into(),
            ..Default::default()
        }));

        let json = agent.to_json();

        // Verify top-level fields.
        assert!(json.get("task").is_some());
        assert!(json.get("status").is_some());
        assert!(json.get("created_at").is_some());

        let messages = json.get("messages").unwrap().as_array().unwrap();
        assert_eq!(messages.len(), 5);

        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "system prompt");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[2]["role"], "assistant");
        assert_eq!(messages[3]["role"], "tool_call");
        assert_eq!(messages[3]["tool"], "read_file");
        assert_eq!(messages[4]["role"], "tool_result");
        assert!(messages[4]["success"].as_bool().unwrap());
    }

    #[test]
    fn to_json_empty_messages() {
        let agent = Agent::new(
            1,
            AgentTask::FreeForm {
                prompt: "empty".into(),
            },
        );
        let json = agent.to_json();
        let messages = json.get("messages").unwrap().as_array().unwrap();
        assert!(messages.is_empty());
    }

    #[test]
    fn tool_call_message_round_trips_through_serde() {
        let call = ToolCall {
            tool: ToolType::ReadFile,
            args: serde_json::json!({"path": "test.txt"}),
        };
        let msg = AgentMessage::ToolCall(call);
        let json = serde_json::to_string(&msg).unwrap();
        let _deser: AgentMessage = serde_json::from_str(&json).unwrap();
    }

    // -- Sec.2 provenance tests ---------------------------------------------

    /// Helper: build a ToolResult tagged with a source event id.
    fn tool_result_with_event(id: u64) -> ToolResult {
        ToolResult {
            tool: ToolType::ReadFile,
            success: true,
            output: "ok".into(),
            tool_name: "read_file".into(),
            args_hash: "abcdef0123456789".into(),
            source_event_id: Some(id),
            ..Default::default()
        }
    }

    #[test]
    fn agent_message_tool_result_includes_provenance() {
        // Pushing a ToolResult onto the agent's history must preserve the
        // provenance fields. This is the substrate's promise that every
        // entry in agent.messages() can be walked back to the substrate event
        // that triggered the tool call.
        let mut agent = Agent::new(
            1,
            AgentTask::FreeForm {
                prompt: "test".into(),
            },
        );
        agent.push_message(AgentMessage::ToolResult(tool_result_with_event(99)));

        let last = agent.messages().last().expect("a message was pushed");
        match last {
            AgentMessage::ToolResult(tr) => {
                assert_eq!(tr.tool_name, "read_file");
                assert_eq!(tr.args_hash, "abcdef0123456789");
                assert_eq!(tr.source_event_id, Some(99));
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn source_chain_for_last_call_walks_back_n_results() {
        // Five ToolResults with event ids [10, 20, 30, 40, 50] in
        // chronological order. Calling source_chain_for_last_call(3) returns
        // the three most-recent ones, ordered most-recent-first: [50, 40, 30].
        let mut agent = Agent::new(
            1,
            AgentTask::FreeForm {
                prompt: "test".into(),
            },
        );
        for id in [10u64, 20, 30, 40, 50] {
            agent.push_message(AgentMessage::ToolResult(tool_result_with_event(id)));
        }

        let chain = agent.source_chain_for_last_call(3);
        assert_eq!(
            chain,
            vec![50, 40, 30],
            "chain must be most-recent-first; got {chain:?}",
        );
    }

    #[test]
    fn source_chain_excludes_user_and_assistant_messages() {
        // Only ToolResult messages contribute event ids to the chain. User,
        // Assistant, System, and ToolCall messages are skipped — they don't
        // carry a source_event_id and aren't in the input chain.
        let mut agent = Agent::new(
            1,
            AgentTask::FreeForm {
                prompt: "test".into(),
            },
        );
        agent.push_message(AgentMessage::User("hi".into()));
        agent.push_message(AgentMessage::ToolResult(tool_result_with_event(11)));
        agent.push_message(AgentMessage::Assistant("response".into()));
        agent.push_message(AgentMessage::ToolCall(ToolCall {
            tool: ToolType::ReadFile,
            args: serde_json::json!({"path": "x"}),
        }));
        agent.push_message(AgentMessage::ToolResult(tool_result_with_event(22)));
        agent.push_message(AgentMessage::User("more".into()));
        agent.push_message(AgentMessage::System("sys".into()));

        let chain = agent.source_chain_for_last_call(5);
        assert_eq!(
            chain,
            vec![22, 11],
            "only ToolResults should contribute; got {chain:?}",
        );
    }

    #[test]
    fn source_chain_for_last_call_skips_results_without_event_id() {
        // ToolResults with `source_event_id == None` (legacy / test paths
        // that didn't populate provenance) are skipped — they don't break
        // the chain, they just don't contribute an id.
        let mut agent = Agent::new(
            1,
            AgentTask::FreeForm {
                prompt: "test".into(),
            },
        );
        agent.push_message(AgentMessage::ToolResult(tool_result_with_event(1)));
        agent.push_message(AgentMessage::ToolResult(ToolResult {
            tool: ToolType::ReadFile,
            success: true,
            output: "no provenance".into(),
            ..Default::default()
        }));
        agent.push_message(AgentMessage::ToolResult(tool_result_with_event(3)));

        let chain = agent.source_chain_for_last_call(5);
        assert_eq!(chain, vec![3, 1]);
    }

    #[test]
    fn source_chain_for_last_call_zero_depth_returns_empty() {
        // depth = 0 short-circuits to an empty Vec without scanning history.
        let mut agent = Agent::new(
            1,
            AgentTask::FreeForm {
                prompt: "test".into(),
            },
        );
        agent.push_message(AgentMessage::ToolResult(tool_result_with_event(1)));

        let chain = agent.source_chain_for_last_call(0);
        assert!(chain.is_empty());
    }

    // -- FSM #34: Planning + AwaitingApproval states --------------------------

    #[test]
    fn can_transition_to_planning_from_queued() {
        assert!(AgentStatus::Queued.can_transition_to(AgentStatus::Planning));
    }

    #[test]
    fn can_transition_to_awaiting_approval_from_planning() {
        assert!(AgentStatus::Planning.can_transition_to(AgentStatus::AwaitingApproval));
    }

    #[test]
    fn can_transition_to_working_from_awaiting_approval() {
        assert!(AgentStatus::AwaitingApproval.can_transition_to(AgentStatus::Working));
    }

    #[test]
    fn can_transition_to_working_from_planning_auto_approve() {
        // The plan gate may auto-approve without going through AwaitingApproval.
        assert!(AgentStatus::Planning.can_transition_to(AgentStatus::Working));
    }

    #[test]
    fn can_transition_awaiting_approval_back_to_planning_on_revision() {
        // A user may reject the plan and request revision — agent re-plans.
        assert!(AgentStatus::AwaitingApproval.can_transition_to(AgentStatus::Planning));
    }

    #[test]
    fn invalid_transition_planning_to_done() {
        assert!(!AgentStatus::Planning.can_transition_to(AgentStatus::Done));
    }

    #[test]
    fn invalid_transition_awaiting_approval_to_done() {
        assert!(!AgentStatus::AwaitingApproval.can_transition_to(AgentStatus::Done));
    }

    #[test]
    fn invalid_transition_awaiting_approval_to_failed_directly() {
        // Cannot jump straight to Failed from AwaitingApproval — must go Working first.
        assert!(!AgentStatus::AwaitingApproval.can_transition_to(AgentStatus::Failed));
    }

    #[test]
    fn invalid_transition_done_to_planning() {
        assert!(!AgentStatus::Done.can_transition_to(AgentStatus::Planning));
    }

    #[test]
    fn begin_planning_transitions_from_queued() {
        let mut agent = Agent::new(1, AgentTask::FreeForm { prompt: "p".into() });
        let ok = agent.begin_planning();
        assert!(ok, "begin_planning() must succeed from Queued");
        assert_eq!(agent.status(), AgentStatus::Planning);
    }

    #[test]
    fn begin_planning_fails_from_working() {
        let mut agent = Agent::new(1, AgentTask::FreeForm { prompt: "p".into() });
        agent.set_status(AgentStatus::Working);
        let ok = agent.begin_planning();
        assert!(!ok, "begin_planning() must fail from Working");
        assert_eq!(agent.status(), AgentStatus::Working, "status must not change");
    }

    #[test]
    fn submit_plan_for_approval_transitions_from_planning() {
        let mut agent = Agent::new(1, AgentTask::FreeForm { prompt: "p".into() });
        agent.begin_planning();
        let ok = agent.submit_plan_for_approval();
        assert!(ok, "submit_plan_for_approval() must succeed from Planning");
        assert_eq!(agent.status(), AgentStatus::AwaitingApproval);
    }

    #[test]
    fn submit_plan_for_approval_fails_from_queued() {
        let mut agent = Agent::new(1, AgentTask::FreeForm { prompt: "p".into() });
        let ok = agent.submit_plan_for_approval();
        assert!(!ok, "submit_plan_for_approval() must fail from Queued");
        assert_eq!(agent.status(), AgentStatus::Queued);
    }

    #[test]
    fn approve_plan_from_awaiting_transitions_to_working() {
        let mut agent = Agent::new(1, AgentTask::FreeForm { prompt: "p".into() });
        agent.begin_planning();
        agent.submit_plan_for_approval();
        let ok = agent.approve_plan();
        assert!(ok, "approve_plan() must succeed from AwaitingApproval");
        assert_eq!(agent.status(), AgentStatus::Working);
    }

    #[test]
    fn approve_plan_from_planning_auto_approve() {
        // Planning → Working directly (auto-approve path, no user interaction).
        let mut agent = Agent::new(1, AgentTask::FreeForm { prompt: "p".into() });
        agent.begin_planning();
        let ok = agent.approve_plan();
        assert!(ok, "approve_plan() must succeed from Planning (auto-approve)");
        assert_eq!(agent.status(), AgentStatus::Working);
    }

    #[test]
    fn approve_plan_fast_path_from_queued() {
        // Queued → Working is the no-gate fast path and a valid transition,
        // so approve_plan() from Queued succeeds (it routes through the same
        // can_transition_to(Working) guard).
        let mut agent = Agent::new(1, AgentTask::FreeForm { prompt: "p".into() });
        let ok = agent.approve_plan();
        assert!(ok, "approve_plan() must succeed from Queued via the fast path");
        assert_eq!(agent.status(), AgentStatus::Working);
    }

    #[test]
    fn approve_plan_fails_from_done() {
        // Done is a terminal state — cannot transition to Working.
        let mut agent = Agent::new(1, AgentTask::FreeForm { prompt: "p".into() });
        agent.complete(true); // → Done
        let ok = agent.approve_plan();
        assert!(!ok, "approve_plan() must fail from Done");
        assert_eq!(agent.status(), AgentStatus::Done, "status must not change");
    }

    #[test]
    fn request_revision_transitions_awaiting_approval_back_to_planning() {
        let mut agent = Agent::new(1, AgentTask::FreeForm { prompt: "p".into() });
        agent.begin_planning();
        agent.submit_plan_for_approval();
        let ok = agent.request_revision();
        assert!(ok, "request_revision() must succeed from AwaitingApproval");
        assert_eq!(agent.status(), AgentStatus::Planning);
    }

    #[test]
    fn request_revision_fails_from_working() {
        let mut agent = Agent::new(1, AgentTask::FreeForm { prompt: "p".into() });
        agent.set_status(AgentStatus::Working);
        let ok = agent.request_revision();
        assert!(!ok, "request_revision() must fail from Working");
        assert_eq!(agent.status(), AgentStatus::Working);
    }

    #[test]
    fn full_plan_gate_happy_path() {
        // Queued → Planning → AwaitingApproval → Working → Done
        let mut agent = Agent::new(1, AgentTask::FreeForm { prompt: "p".into() });
        assert_eq!(agent.status(), AgentStatus::Queued);
        assert!(agent.begin_planning());
        assert_eq!(agent.status(), AgentStatus::Planning);
        assert!(agent.submit_plan_for_approval());
        assert_eq!(agent.status(), AgentStatus::AwaitingApproval);
        assert!(agent.approve_plan());
        assert_eq!(agent.status(), AgentStatus::Working);
        agent.complete(true);
        assert_eq!(agent.status(), AgentStatus::Done);
    }

    #[test]
    fn full_plan_gate_with_revision() {
        // Queued → Planning → AwaitingApproval → Planning → AwaitingApproval → Working
        let mut agent = Agent::new(1, AgentTask::FreeForm { prompt: "p".into() });
        agent.begin_planning();
        agent.submit_plan_for_approval();
        assert_eq!(agent.status(), AgentStatus::AwaitingApproval);
        // User rejects, requests revision.
        assert!(agent.request_revision());
        assert_eq!(agent.status(), AgentStatus::Planning);
        // Agent replans, resubmits.
        assert!(agent.submit_plan_for_approval());
        assert_eq!(agent.status(), AgentStatus::AwaitingApproval);
        // User approves.
        assert!(agent.approve_plan());
        assert_eq!(agent.status(), AgentStatus::Working);
    }

    #[test]
    fn is_terminal_for_done_failed_flatline() {
        assert!(AgentStatus::Done.is_terminal());
        assert!(AgentStatus::Failed.is_terminal());
        assert!(AgentStatus::Flatline.is_terminal());
        assert!(!AgentStatus::Queued.is_terminal());
        assert!(!AgentStatus::Planning.is_terminal());
        assert!(!AgentStatus::AwaitingApproval.is_terminal());
        assert!(!AgentStatus::Working.is_terminal());
        assert!(!AgentStatus::WaitingForTool.is_terminal());
    }

    #[test]
    fn is_active_for_working_and_waiting_for_tool() {
        assert!(AgentStatus::Working.is_active());
        assert!(AgentStatus::WaitingForTool.is_active());
        assert!(!AgentStatus::Queued.is_active());
        assert!(!AgentStatus::Planning.is_active());
        assert!(!AgentStatus::AwaitingApproval.is_active());
        assert!(!AgentStatus::Done.is_active());
        assert!(!AgentStatus::Failed.is_active());
        assert!(!AgentStatus::Flatline.is_active());
    }

    #[test]
    fn status_line_shows_planning_tag() {
        let mut agent = Agent::new(1, AgentTask::FreeForm { prompt: "task".into() });
        agent.begin_planning();
        let line = agent.status_line();
        assert!(line.contains("PLANNING"), "status line must show PLANNING tag");
    }

    #[test]
    fn status_line_shows_pending_approval_tag() {
        let mut agent = Agent::new(1, AgentTask::FreeForm { prompt: "task".into() });
        agent.begin_planning();
        agent.submit_plan_for_approval();
        let line = agent.status_line();
        assert!(
            line.contains("PENDING APPROVAL"),
            "status line must show PENDING APPROVAL tag"
        );
    }

    // ---- Correlation ID -------------------------------------------------------

    #[test]
    fn new_agent_has_no_correlation_id() {
        let agent = Agent::new(1, AgentTask::FreeForm { prompt: "test".into() });
        assert!(
            agent.correlation_id().is_none(),
            "Agent::new must leave correlation_id as None",
        );
        assert!(
            agent.parent_id().is_none(),
            "Agent::new must leave parent_id as None",
        );
    }

    #[test]
    fn with_correlation_stamps_id_and_parent() {
        let cid = CorrelationId::new();
        let agent = Agent::with_correlation(
            42,
            AgentTask::FreeForm { prompt: "child".into() },
            cid,
            Some(7),
        );

        assert_eq!(
            agent.correlation_id(),
            Some(cid),
            "with_correlation must stamp the given CorrelationId",
        );
        assert_eq!(
            agent.parent_id(),
            Some(7),
            "with_correlation must stamp the given parent_id",
        );
    }

    #[test]
    fn with_correlation_no_parent() {
        let cid = CorrelationId::new();
        let agent = Agent::with_correlation(
            10,
            AgentTask::FreeForm { prompt: "root-child".into() },
            cid,
            None,
        );

        assert_eq!(agent.correlation_id(), Some(cid));
        assert!(agent.parent_id().is_none(), "parent_id must be None when not supplied");
    }

    #[test]
    fn with_correlation_inherits_queued_status() {
        let cid = CorrelationId::new();
        let agent = Agent::with_correlation(
            5,
            AgentTask::FreeForm { prompt: "test".into() },
            cid,
            None,
        );
        assert_eq!(
            agent.status(),
            AgentStatus::Queued,
            "with_correlation must start in Queued status",
        );
    }

    #[test]
    fn pipeline_agents_share_correlation_id() {
        // All agents in a pipeline run should carry the same CorrelationId.
        let cid = CorrelationId::new();
        let parent = Agent::with_correlation(1, AgentTask::FreeForm { prompt: "orchestrator".into() }, cid, None);
        let child = Agent::with_correlation(2, AgentTask::FreeForm { prompt: "worker".into() }, cid, Some(1));

        assert_eq!(
            parent.correlation_id(),
            child.correlation_id(),
            "pipeline agents must share the same correlation id",
        );
    }

    // -----------------------------------------------------------------------
    // Issue #74: SemanticContext wiring into Agent
    // -----------------------------------------------------------------------

    /// A freshly constructed agent has an empty semantic ring-buffer.
    #[test]
    fn new_agent_semantic_ctx_empty() {
        let agent = Agent::new(1, AgentTask::FreeForm { prompt: "hi".into() });
        assert!(agent.semantic_ctx().is_empty());
        assert!(agent.semantic_prompt_section().is_none());
    }

    /// After `push_semantic_output`, the ring-buffer is non-empty and
    /// `semantic_prompt_section` returns `Some`.
    #[test]
    fn push_semantic_output_populates_ring_buffer() {
        let mut agent = Agent::new(1, AgentTask::FreeForm { prompt: "hi".into() });

        let parsed = phantom_semantic::SemanticParser::parse(
            "git status",
            "On branch main\nnothing to commit, working tree clean\n",
            "",
            Some(0),
        );
        agent.push_semantic_output(parsed);

        assert_eq!(agent.semantic_ctx().len(), 1);
        assert!(agent.semantic_prompt_section().is_some());
    }

    /// The semantic prompt section includes the "## Recent command output"
    /// heading and labels git status output correctly.
    #[test]
    fn semantic_prompt_section_contains_git_status_label() {
        let mut agent = Agent::new(1, AgentTask::FreeForm { prompt: "hi".into() });

        let parsed = phantom_semantic::SemanticParser::parse(
            "git status",
            "On branch feature/x\nnothing to commit, working tree clean\n",
            "",
            Some(0),
        );
        agent.push_semantic_output(parsed);

        let section = agent.semantic_prompt_section().unwrap();
        assert!(
            section.contains("## Recent command output"),
            "section must have heading; got: {section}"
        );
        assert!(
            section.contains("git.status"),
            "section must label git status; got: {section}"
        );
        assert!(
            section.contains("feature/x"),
            "branch name must surface; got: {section}"
        );
    }

    /// Semantic context is FIFO-capped at MAX_ENTRIES (10). Pushing 12 entries
    /// keeps the ring-buffer at 10 and evicts the two oldest.
    #[test]
    fn semantic_ctx_ring_buffer_caps_at_max_entries() {
        let mut agent = Agent::new(1, AgentTask::FreeForm { prompt: "hi".into() });

        for i in 0..12u32 {
            let parsed = phantom_semantic::SemanticParser::parse(
                &format!("echo {i}"),
                &format!("{i}\n"),
                "",
                Some(0),
            );
            agent.push_semantic_output(parsed);
        }

        // Must not exceed 10.
        assert_eq!(agent.semantic_ctx().len(), 10);

        // The oldest entries (echo 0, echo 1) must be evicted.
        let commands: Vec<&str> = agent
            .semantic_ctx()
            .entries()
            .iter()
            .map(|e| e.command.as_str())
            .collect();
        assert!(!commands.contains(&"echo 0"), "echo 0 should be evicted");
        assert!(!commands.contains(&"echo 1"), "echo 1 should be evicted");
        assert!(commands.contains(&"echo 11"), "echo 11 must be present");
    }

    /// `semantic_prompt_section` surfaces cargo test results (pass/fail counts).
    #[test]
    fn semantic_prompt_section_surfaces_test_results() {
        let mut agent = Agent::new(1, AgentTask::FreeForm { prompt: "hi".into() });

        let stdout = "\
running 3 tests
test tests::a ... ok
test tests::b ... ok
test tests::c ... ok

test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
";
        let parsed =
            phantom_semantic::SemanticParser::parse("cargo test", stdout, "", Some(0));
        agent.push_semantic_output(parsed);

        let section = agent.semantic_prompt_section().unwrap();
        assert!(section.contains("test.result"), "label must be test.result; got: {section}");
        assert!(section.contains("passed"), "pass count must surface; got: {section}");
    }

    /// Multiple sequential push calls accumulate in order; `semantic_ctx.latest()`
    /// returns the most recently pushed entry.
    #[test]
    fn semantic_ctx_latest_is_most_recent() {
        let mut agent = Agent::new(1, AgentTask::FreeForm { prompt: "hi".into() });

        let p1 = phantom_semantic::SemanticParser::parse("echo a", "a\n", "", Some(0));
        let p2 = phantom_semantic::SemanticParser::parse(
            "git status",
            "On branch main\nnothing to commit, working tree clean\n",
            "",
            Some(0),
        );

        agent.push_semantic_output(p1);
        agent.push_semantic_output(p2);

        let latest = agent.semantic_ctx().latest().expect("ring-buffer has entries");
        assert_eq!(latest.command, "git status");
    }

    // ---- AgentSpawnOpts role/label wiring (#224) ----------------------------

    #[test]
    fn spawn_opts_new_defaults_role_and_label_to_none() {
        let opts = AgentSpawnOpts::new(AgentTask::FreeForm { prompt: "task".into() });
        assert!(opts.role.is_none(), "AgentSpawnOpts::new must default role to None");
        assert!(opts.label.is_none(), "AgentSpawnOpts::new must default label to None");
    }

    #[test]
    fn spawn_opts_with_role_defender_is_preserved() {
        use crate::role::AgentRole;
        let opts = AgentSpawnOpts::new(AgentTask::FreeForm { prompt: "sec".into() })
            .with_role(AgentRole::Defender);
        assert_eq!(
            opts.role,
            Some(AgentRole::Defender),
            "with_role(Defender) must set opts.role = Some(Defender)",
        );
    }

    #[test]
    fn spawn_opts_with_label_is_preserved() {
        let opts = AgentSpawnOpts::new(AgentTask::FreeForm { prompt: "work".into() })
            .with_label("custom-defender");
        assert_eq!(
            opts.label.as_deref(),
            Some("custom-defender"),
            "with_label must set opts.label = Some(custom-defender)",
        );
    }

    #[test]
    fn spawn_subagent_request_carries_role_and_label_through_queue() {
        use crate::composer_tools::{new_spawn_subagent_queue, spawn_subagent};
        use crate::role::AgentRole;
        use serde_json::json;

        let queue = new_spawn_subagent_queue();
        let args = json!({
            "role": "defender",
            "label": "sec-watcher",
            "task": "observe denial",
        });
        let id = spawn_subagent(&args, 1, &queue).expect("spawn_subagent must succeed");

        let q = queue.lock().expect("queue lock");
        assert_eq!(q.len(), 1);
        let req = &q[0];
        assert_eq!(req.assigned_id, id, "assigned_id must match returned id");
        assert_eq!(
            req.role,
            AgentRole::Defender,
            "SpawnSubagentRequest.role must be Defender, not Conversational",
        );
        assert_eq!(
            req.label,
            "sec-watcher",
            "SpawnSubagentRequest.label must be sec-watcher",
        );
    }

    // ---- Disposition wiring (#38 / #49) ------------------------------------

    /// Chat-disposition agents must jump Queued → Working via `try_auto_approve`,
    /// never visiting `AwaitingApproval`. This is the agent-level anchor for the
    /// auto-approve fast path (Issue #49).
    #[test]
    fn try_auto_approve_chat_goes_queued_to_working_directly() {
        let mut agent = Agent::with_disposition(
            1,
            AgentTask::FreeForm { prompt: "summarise".into() },
            Disposition::Chat,
        );
        assert_eq!(agent.status(), AgentStatus::Queued);

        let approved = agent.try_auto_approve();

        assert!(approved, "try_auto_approve must return true for Chat disposition");
        assert_eq!(
            agent.status(),
            AgentStatus::Working,
            "Chat agent must be Working after try_auto_approve, not AwaitingApproval",
        );
    }

    /// `try_auto_approve` on Synthesize, Decompose, and Audit must also
    /// short-circuit to Working (all satisfy `auto_approve()`).
    #[test]
    fn try_auto_approve_synthesize_decompose_audit_go_to_working() {
        for disposition in [Disposition::Synthesize, Disposition::Decompose, Disposition::Audit] {
            let mut agent = Agent::with_disposition(
                1,
                AgentTask::FreeForm { prompt: "task".into() },
                disposition,
            );
            assert!(
                agent.try_auto_approve(),
                "{disposition:?} must auto-approve",
            );
            assert_eq!(
                agent.status(),
                AgentStatus::Working,
                "{disposition:?} must end up Working, not AwaitingApproval",
            );
        }
    }

    /// Write-side dispositions (Feature, BugFix, Refactor, Chore) must NOT
    /// auto-approve — they require the full plan gate.
    #[test]
    fn try_auto_approve_returns_false_for_write_dispositions() {
        for disposition in [
            Disposition::Feature,
            Disposition::BugFix,
            Disposition::Refactor,
            Disposition::Chore,
        ] {
            let mut agent = Agent::with_disposition(
                1,
                AgentTask::FreeForm { prompt: "task".into() },
                disposition,
            );
            assert!(
                !agent.try_auto_approve(),
                "{disposition:?} must not auto-approve",
            );
            assert_eq!(
                agent.status(),
                AgentStatus::Queued,
                "{disposition:?} must remain Queued when try_auto_approve returns false",
            );
        }
    }

    #[test]
    fn new_agent_defaults_to_chat_disposition() {
        let a = Agent::new(99, AgentTask::FreeForm { prompt: "x".into() });
        assert_eq!(a.disposition, Disposition::Chat);
    }

    #[test]
    fn with_disposition_sets_field() {
        let a = Agent::with_disposition(100, AgentTask::FreeForm { prompt: "x".into() }, Disposition::BugFix);
        assert_eq!(a.disposition, Disposition::BugFix);
        assert_eq!(a.status, AgentStatus::Queued);
    }

    #[test]
    fn spawn_opts_default_chat() {
        let o = AgentSpawnOpts::new(AgentTask::FreeForm { prompt: "x".into() });
        assert_eq!(o.disposition, Disposition::Chat);
    }

    #[test]
    fn spawn_opts_builder() {
        let o = AgentSpawnOpts::new(AgentTask::FreeForm { prompt: "x".into() })
            .with_disposition(Disposition::Feature);
        assert_eq!(o.disposition, Disposition::Feature);
        assert!(o.chat_model.is_none());
    }
}
