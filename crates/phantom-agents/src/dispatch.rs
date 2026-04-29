//! Tool-name dispatch shim.
//!
//! When the LLM emits a [`crate::api::ApiEvent::ToolUse`] block, the agent
//! loop holds a tool name (`"read_file"`, `"send_to_agent"`, …) plus an
//! arbitrary JSON args object. Today that name routes through
//! [`crate::tools::execute_tool`] for the file/git surface, but `chat_tools`
//! and `composer_tools` exist as standalone unit-tested handlers with no
//! single dispatch point. This module is the last mile: one [`dispatch_tool`]
//! function that fork-routes by name through *all three* surfaces and
//! enforces capability gating uniformly at the entry-point.
//!
//! ## Routing
//!
//! - `read_file`, `write_file`, `edit_file`, `run_command`, `search_files`,
//!   `git_status`, `git_diff`, `list_files` →
//!   [`crate::tools::execute_tool`]. The role-class gate is enforced here so
//!   the dispatch site is the single source of truth for capability checks.
//! - `send_to_agent`, `read_from_agent`, `broadcast_to_role` →
//!   [`crate::chat_tools`] handlers, with a [`crate::chat_tools::ChatToolContext`]
//!   built from the dispatch context's registry / event log.
//! - `spawn_subagent`, `wait_for_agent`, `request_critique`, `event_log_query`
//!   → [`crate::composer_tools`] handlers, with the appropriate sub-context.
//! - `challenge_agent` → [`crate::defender_tools`] handler, with a
//!   [`crate::defender_tools::DefenderToolContext`]. Sec.5 — the Defender's
//!   single offensive route, gated on `Coordinate`.
//! - Anything else → a [`ToolResult`] with `success: false` and
//!   `output: "unknown tool: <name>"` so the model sees the refusal in its
//!   next turn.
//!
//! ## Capability gating
//!
//! Every fork is gated by [`check_capability`] before the handler runs. The
//! class is taken from:
//! - [`class_for`] for file/git tools (a local Sense/Act mapping),
//! - [`ChatTool::class`] for chat tools,
//! - [`ComposerTool::class`] for composer tools,
//! - [`DefenderTool::class`] for defender tools.
//!
//! On denial, a [`ToolResult`] with the canonical
//! `"capability denied: <Class> not in <Role> manifest"` body is returned.
//! The model sees this in the next `tool_result` block and self-corrects.
//!
//! ## Taint elevation (Sec.7.2)
//!
//! After the handler returns, [`dispatch_tool`] walks the `source_event_id`
//! chain backwards in the event log and elevates taint on the result:
//!
//! - Any upstream event with `kind == "capability.denied"` → at least `Suspect`.
//! - Any upstream *agent* (identified by the event's `source` field or a
//!   `"source_agent_id"` payload key) whose registry status is
//!   [`crate::inbox::AgentStatus::Failed`] (quarantined) → `Tainted`.
//!
//! Elevation is monotone: the result taint can only increase. Callers that set
//! `source_event_id: None` on the context opt out of the walk — the legacy /
//! test path.
//!
//! ## Threading
//!
//! [`DispatchContext`] borrows the working dir as `&Path` (per-call) and
//! holds `Arc<Mutex<…>>` clones of the registry / event log / spawn queue.
//! Building one per tool-use turn is cheap and keeps the borrow story local
//! to `agent_pane::execute_pending_tools`.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};

use phantom_memory::event_log::EventLog;

use crate::chat_tools::{ChatTool, ChatToolContext, broadcast_to_role, read_from_agent, send_to_agent};
use crate::composer_tools::{
    ComposerTool, SpawnSubagentRequest, event_log_query, request_critique, spawn_subagent,
    wait_for_agent,
};
use crate::correlation::CorrelationId;
use crate::defender_tools::{DefenderTool, DefenderToolContext, challenge_agent};
use crate::dispatcher::{
    DispatcherTool, DispatcherToolContext, GhTicketDispatcher, mark_ticket_done,
    mark_ticket_in_progress, request_next_ticket,
};
use crate::inbox::{AgentRegistry, AgentStatus};
use crate::quarantine::QuarantineRegistry;
use crate::role::{AgentId, AgentRef, AgentRole, CapabilityClass};
use crate::taint::TaintLevel;
use crate::tools::{ToolResult, ToolType, execute_tool};

// ---------------------------------------------------------------------------
// Disposition
// ---------------------------------------------------------------------------

/// Intent classification for an agent spawn.
///
/// The default is [`Disposition::Chat`] (zero-side-effect) so existing call
/// sites that don't set a disposition are unaffected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Disposition {
    Chat,
    Feature,
    BugFix,
    Refactor,
    Chore,
    Synthesize,
    Decompose,
    Audit,
}

impl Disposition {
    #[must_use]
    pub fn creates_branch(self) -> bool {
        matches!(self, Self::Feature | Self::BugFix | Self::Refactor | Self::Chore)
    }

    #[must_use]
    pub fn requires_plan_gate(self) -> bool {
        matches!(self, Self::Feature | Self::BugFix | Self::Refactor)
    }

    #[must_use]
    pub fn runs_hooks(self) -> bool {
        self.creates_branch()
    }

    #[must_use]
    pub fn auto_approve(self) -> bool {
        matches!(self, Self::Chat | Self::Synthesize | Self::Decompose | Self::Audit)
    }

    #[must_use]
    pub fn skill(self) -> &'static str {
        match self {
            Self::Chat => "",
            Self::Feature => "feature",
            Self::BugFix => "bugfix",
            Self::Refactor => "refactor",
            Self::Chore => "chore",
            Self::Synthesize => "synthesize",
            Self::Decompose => "decompose",
            Self::Audit => "",
        }
    }
}

impl Default for Disposition {
    fn default() -> Self {
        Self::Chat
    }
}

// ---------------------------------------------------------------------------
// Capability gating helpers
// ---------------------------------------------------------------------------

/// Default-deny gate. Returns `Ok(())` iff `role`'s manifest declares
/// `class`; otherwise returns the canonical
/// `"capability denied: <Class> not in <Role> manifest"` message the model
/// sees in its next `tool_result` block. Pinning the wording here keeps the
/// API contract stable across all three dispatch forks.
fn check_capability(role: AgentRole, class: CapabilityClass) -> Result<(), String> {
    if role.has(class) {
        Ok(())
    } else {
        Err(format!(
            "capability denied: {class:?} not in {role:?} manifest"
        ))
    }
}

/// Map a [`ToolType`] to its [`CapabilityClass`].
///
/// Read-only inspectors (file reads, listings, git status/diff) are
/// `Sense`. Mutators (file writes, edits, shell commands) are `Act`. Local
/// to this module so the `tools` crate doesn't need a per-tool class
/// declaration — the dispatch surface is the only place that intersects
/// against role manifests.
fn class_for(tool: ToolType) -> CapabilityClass {
    match tool {
        ToolType::ReadFile
        | ToolType::SearchFiles
        | ToolType::ListFiles
        | ToolType::GitStatus
        | ToolType::GitDiff => CapabilityClass::Sense,
        ToolType::WriteFile | ToolType::EditFile | ToolType::RunCommand => CapabilityClass::Act,
    }
}

// ---------------------------------------------------------------------------
// DispatchContext
// ---------------------------------------------------------------------------

/// Per-turn context for [`dispatch_tool`].
///
/// Built once per `execute_pending_tools` call so each `ToolUse` block in
/// the same turn shares the same registry / log / queue handles. Borrows
/// the working directory as `&Path` instead of `String` so the App's
/// canonical `working_dir: String` doesn't have to be cloned per dispatch.
pub struct DispatchContext<'a> {
    /// The calling agent's [`AgentRef`]. Stamped on every outgoing
    /// `agent.speak` envelope and inbox message so attribution survives
    /// hops through the substrate.
    pub self_ref: AgentRef,
    /// The calling agent's role. Default-deny gating intersects every
    /// tool's [`CapabilityClass`] against this role's manifest before the
    /// handler runs.
    pub role: AgentRole,
    /// Project root for file/git tool sandboxing. Borrowed per call.
    pub working_dir: &'a Path,
    /// Live agent directory used by chat tools (`send_to_agent`,
    /// `broadcast_to_role`) and `request_critique`.
    pub registry: Arc<Mutex<AgentRegistry>>,
    /// Shared append-only log. `None` is permitted for legacy / test paths
    /// that haven't opened a log file; chat-tool log emission becomes a
    /// no-op and `read_from_agent` / `wait_for_agent` / `event_log_query`
    /// return a structured error.
    pub event_log: Option<Arc<Mutex<EventLog>>>,
    /// Queue the Composer's `spawn_subagent` tool pushes into. The App
    /// drains this once per frame in `update.rs`.
    pub pending_spawn: Arc<Mutex<VecDeque<SpawnSubagentRequest>>>,
    /// The substrate event id that triggered this dispatch turn, if known.
    ///
    /// When set, [`dispatch_tool`] walks the `source_event_id` chain
    /// backwards in the event log and elevates the result taint:
    /// - any upstream `"capability.denied"` event → at least `Suspect`,
    /// - any upstream agent with [`AgentStatus::Failed`] (quarantined) →
    ///   `Tainted`.
    ///
    /// `None` disables the chain walk — the correct behaviour for legacy /
    /// test paths that have not wired an event log.
    pub source_event_id: Option<u64>,
    /// Sec.7.3: quarantine registry.
    ///
    /// When `Some`, [`dispatch_tool`] checks whether the calling agent
    /// (`self_ref.id`) is quarantined before routing the tool. A quarantined
    /// agent's dispatch is denied with a `DispatchError::Quarantined`-style
    /// message regardless of capability class.
    ///
    /// `None` skips the quarantine gate — the correct behaviour for legacy /
    /// test paths that have not wired a quarantine registry.
    pub quarantine: Option<Arc<Mutex<QuarantineRegistry>>>,
    /// Causality token linking this dispatch turn to the pipeline run that
    /// triggered it.
    ///
    /// When `Some`, tracing spans and log macros should attach this id so
    /// every event, tool call, and log entry in the same pipeline run can be
    /// correlated by querying `WHERE correlation_id = ?`.
    ///
    /// `None` means the dispatch was not initiated from a tracked user action
    /// (legacy / test path) — this is never an error.
    pub correlation_id: Option<CorrelationId>,
    /// Issue #24: ticket dispatcher.
    ///
    /// When `Some`, the Dispatcher role's three tools (`request_next_ticket`,
    /// `mark_ticket_in_progress`, `mark_ticket_done`) are routed to
    /// [`GhTicketDispatcher`]. `None` returns an `"unknown tool"` style error
    /// for those names — the correct behaviour for non-Dispatcher agents and
    /// legacy test paths.
    pub ticket_dispatcher: Option<Arc<GhTicketDispatcher>>,
}

// ---------------------------------------------------------------------------
// Taint elevation: Sec.7.2
// ---------------------------------------------------------------------------

/// The event-log `kind` string emitted when a capability denial fires.
const KIND_CAPABILITY_DENIED: &str = "capability.denied";

/// Walk the `source_event_id` chain in `log` starting from `start_id`.
///
/// Returns the worst [`TaintLevel`] found across all upstream events:
/// - `Tainted` if any upstream source agent has [`AgentStatus::Failed`].
/// - `Suspect` if any upstream event has `kind == "capability.denied"`.
/// - `Clean` if the chain is empty or contains neither signal.
///
/// The walk is bounded by the in-memory tail (`EventLog::tail`). Events that
/// have scrolled out of the tail are treated as clean — conservative and
/// fast, matching the log's own cap policy.
///
/// # Locking
///
/// Takes `log` and `registry` locks separately and briefly. Never holds both
/// simultaneously, so deadlocks with other lock holders are impossible.
fn taint_from_source_chain(
    start_id: u64,
    log: &Arc<Mutex<EventLog>>,
    registry: &Arc<Mutex<AgentRegistry>>,
) -> TaintLevel {
    // Snapshot the in-memory tail once — avoids repeated locking in the walk.
    let tail = {
        let Ok(guard) = log.lock() else {
            return TaintLevel::Clean;
        };
        guard.tail(usize::MAX)
    };

    // Build a lookup table: event_id → index for O(1) chain hops.
    let by_id: std::collections::HashMap<u64, &phantom_memory::event_log::EventEnvelope> =
        tail.iter().map(|e| (e.id, e)).collect();

    let mut accumulated = TaintLevel::Clean;
    let mut cursor: Option<u64> = Some(start_id);
    let mut visited: std::collections::HashSet<u64> = std::collections::HashSet::new();

    while let Some(id) = cursor {
        // Cycle guard: if we've already processed this event id, stop walking.
        if !visited.insert(id) {
            break;
        }
        let Some(ev) = by_id.get(&id) else {
            // Event has scrolled out of the tail — stop walking.
            break;
        };

        // Check for CapabilityDenied kind → Suspect.
        if ev.kind == KIND_CAPABILITY_DENIED {
            accumulated = accumulated.merge(TaintLevel::Suspect);
        }

        // Check if the source agent is quarantined (Failed) → Tainted.
        if let Some(agent_id) = source_agent_id_from_envelope(ev) {
            if agent_is_quarantined(agent_id, registry) {
                accumulated = accumulated.merge(TaintLevel::Tainted);
            }
        }

        // Short-circuit: Tainted is the maximum, no need to walk further.
        if accumulated == TaintLevel::Tainted {
            break;
        }

        // Follow the chain link stored in the event payload.
        cursor = ev
            .payload
            .get("source_event_id")
            .and_then(|v| v.as_u64());
    }

    accumulated
}

/// Extract the originating agent id from an [`EventEnvelope`], if present.
///
/// Events emitted by agents carry `source: Agent { id }`. We also check a
/// `"source_agent_id"` payload field as a fallback for hand-rolled events
/// that embed the agent id in the payload rather than via the `source` field.
fn source_agent_id_from_envelope(
    ev: &phantom_memory::event_log::EventEnvelope,
) -> Option<AgentId> {
    // Primary: structured source field.
    if let phantom_memory::event_log::EventSource::Agent { id } = ev.source {
        return Some(id);
    }
    // Fallback: payload key, used by hand-built test events.
    ev.payload
        .get("source_agent_id")
        .and_then(|v| v.as_u64())
}

/// Returns `true` iff the agent identified by `id` has [`AgentStatus::Failed`]
/// (i.e. is quarantined) in the live registry.
///
/// Returns `false` when the agent is unknown or the registry lock is poisoned —
/// conservative but safe: we never false-positive a quarantine.
fn agent_is_quarantined(id: AgentId, registry: &Arc<Mutex<AgentRegistry>>) -> bool {
    let Ok(guard) = registry.lock() else {
        return false;
    };
    guard
        .get(id)
        .map(|h| *h.status.borrow() == AgentStatus::Failed)
        .unwrap_or(false)
}

/// Sec.7.3: Returns `true` iff the agent identified by `id` is quarantined
/// according to the [`QuarantineRegistry`].
///
/// Returns `false` when the registry lock is poisoned — conservative and safe.
fn quarantine_registry_blocks(id: AgentId, quarantine: &Arc<Mutex<QuarantineRegistry>>) -> bool {
    let Ok(guard) = quarantine.lock() else {
        return false;
    };
    guard.agent_is_quarantined(id)
}

// ---------------------------------------------------------------------------
// dispatch_tool
// ---------------------------------------------------------------------------

/// Synthetic tool used as the `tool` field of unknown / cross-surface
/// `ToolResult` returns. The agent loop encodes results with this tool's
/// `api_name()` (`"read_file"`) but the body string ("unknown tool: …",
/// "capability denied: …", or the chat/composer handler output) is what the
/// model actually sees. Pinning a placeholder keeps the wire shape stable
/// without having to widen [`ToolType`] for tools that aren't file-tool.
const PLACEHOLDER_TOOL: ToolType = ToolType::ReadFile;

/// Build a `ToolResult` carrying just `(tool, success, output)` and let
/// every other field default. Hides the `..Default::default()` boilerplate
/// from the dispatch body where every fork wants the same encoded shape.
fn result(tool: ToolType, success: bool, output: String) -> ToolResult {
    ToolResult {
        tool,
        success,
        output,
        ..Default::default()
    }
}

/// Dispatch a single tool-use block by name.
///
/// Returns a [`ToolResult`] whose `success` / `output` fields are encoded
/// for the model's next turn:
///
/// - File/git tools: returns whatever [`execute_tool`] produced.
/// - Chat / composer tools: returns `success: true, output: <handler json>`
///   on success, `success: false, output: <error message>` on handler
///   failure. The body is JSON-encoded for tools whose return types are
///   structured (e.g. [`read_from_agent`]'s envelope vector,
///   [`spawn_subagent`]'s allocated id) so the model can parse them
///   uniformly.
/// - Capability-denied: `success: false, output: "capability denied: …"`.
/// - Unknown name: `success: false, output: "unknown tool: <name>"`.
///
/// After the handler returns, taint is elevated based on
/// [`DispatchContext::source_event_id`] — see the module doc for the full
/// Sec.7.2 rules.
#[must_use]
pub fn dispatch_tool(
    name: &str,
    args: &serde_json::Value,
    ctx: &DispatchContext<'_>,
) -> ToolResult {
    // ---- Sec.7.3: Quarantine gate ------------------------------------------
    //
    // If the calling agent is quarantined, deny all tool dispatches before any
    // capability check or handler runs. The quarantine registry is checked via
    // its own lock; if the lock is poisoned, we fail open (conservative) to
    // avoid wedging the dispatch path.
    if let Some(quarantine) = ctx.quarantine.as_ref() {
        if quarantine_registry_blocks(ctx.self_ref.id, quarantine) {
            return result(
                PLACEHOLDER_TOOL,
                false,
                format!(
                    "agent quarantined: all tool dispatches denied for agent {}",
                    ctx.self_ref.id
                ),
            );
        }
    }

    // ---- Route to the appropriate tool surface -----------------------------
    let mut tool_result = if let Some(tool) = ToolType::from_api_name(name) {
        // ---- File / git tools ----------------------------------------------
        if let Err(msg) = check_capability(ctx.role, class_for(tool)) {
            result(tool, false, msg)
        } else {
            let working_dir = ctx.working_dir.to_string_lossy();
            execute_tool(tool, args, &working_dir, &ctx.role)
        }
    } else if let Some(chat_tool) = ChatTool::from_api_name(name) {
        // ---- Chat tools ----------------------------------------------------
        if let Err(msg) = check_capability(ctx.role, chat_tool.class()) {
            result(PLACEHOLDER_TOOL, false, msg)
        } else {
            let chat_ctx = ChatToolContext {
                self_ref: ctx.self_ref.clone(),
                registry: ctx.registry.clone(),
                event_log: ctx.event_log.clone(),
            };
            match chat_tool {
                ChatTool::SendToAgent => match send_to_agent(args, &chat_ctx) {
                    Ok(msg) => result(PLACEHOLDER_TOOL, true, msg),
                    Err(e) => result(PLACEHOLDER_TOOL, false, e),
                },
                ChatTool::ReadFromAgent => match read_from_agent(args, &chat_ctx) {
                    Ok(envs) => {
                        let body = serde_json::to_string(&envs)
                            .unwrap_or_else(|e| format!("encode error: {e}"));
                        result(PLACEHOLDER_TOOL, true, body)
                    }
                    Err(e) => result(PLACEHOLDER_TOOL, false, e),
                },
                ChatTool::BroadcastToRole => match broadcast_to_role(args, &chat_ctx) {
                    Ok(count) => result(
                        PLACEHOLDER_TOOL,
                        true,
                        format!("delivered to {count} agent(s)"),
                    ),
                    Err(e) => result(PLACEHOLDER_TOOL, false, e),
                },
            }
        }
    } else if let Some(composer_tool) = ComposerTool::from_api_name(name) {
        // ---- Composer tools ------------------------------------------------
        if let Err(msg) = check_capability(ctx.role, composer_tool.class()) {
            result(PLACEHOLDER_TOOL, false, msg)
        } else {
            match composer_tool {
                ComposerTool::SpawnSubagent => {
                    match spawn_subagent(args, ctx.self_ref.id, &ctx.pending_spawn) {
                        Ok(id) => result(
                            PLACEHOLDER_TOOL,
                            true,
                            format!("spawned subagent id={id}"),
                        ),
                        Err(e) => result(PLACEHOLDER_TOOL, false, e),
                    }
                }
                ComposerTool::WaitForAgent => match ctx.event_log.as_ref() {
                    None => {
                        result(PLACEHOLDER_TOOL, false, "event log not configured".into())
                    }
                    Some(log) => match wait_for_agent(args, log) {
                        Ok(env) => {
                            let body = serde_json::to_string(&env)
                                .unwrap_or_else(|e| format!("encode error: {e}"));
                            result(PLACEHOLDER_TOOL, true, body)
                        }
                        Err(e) => result(PLACEHOLDER_TOOL, false, e),
                    },
                },
                ComposerTool::RequestCritique => match ctx.event_log.as_ref() {
                    None => {
                        result(PLACEHOLDER_TOOL, false, "event log not configured".into())
                    }
                    Some(log) => match ctx.registry.lock() {
                        Err(_) => result(
                            PLACEHOLDER_TOOL,
                            false,
                            "agent registry poisoned".into(),
                        ),
                        Ok(registry_guard) => {
                            match request_critique(args, &ctx.self_ref, &registry_guard, log) {
                                Ok(env) => {
                                    let body = serde_json::to_string(&env)
                                        .unwrap_or_else(|e| format!("encode error: {e}"));
                                    result(PLACEHOLDER_TOOL, true, body)
                                }
                                Err(e) => result(PLACEHOLDER_TOOL, false, e),
                            }
                        }
                    },
                },
                ComposerTool::EventLogQuery => match ctx.event_log.as_ref() {
                    None => {
                        result(PLACEHOLDER_TOOL, false, "event log not configured".into())
                    }
                    Some(log) => match event_log_query(args, log) {
                        Ok(envs) => {
                            let body = serde_json::to_string(&envs)
                                .unwrap_or_else(|e| format!("encode error: {e}"));
                            result(PLACEHOLDER_TOOL, true, body)
                        }
                        Err(e) => result(PLACEHOLDER_TOOL, false, e),
                    },
                },
            }
        }
    } else if let Some(defender_tool) = DefenderTool::from_api_name(name) {
        // ---- Defender tools ------------------------------------------------
        if let Err(msg) = check_capability(ctx.role, defender_tool.class()) {
            result(PLACEHOLDER_TOOL, false, msg)
        } else {
            let defender_ctx = DefenderToolContext {
                self_ref: ctx.self_ref.clone(),
                registry: ctx.registry.clone(),
                event_log: ctx.event_log.clone(),
            };
            match defender_tool {
                DefenderTool::ChallengeAgent => match challenge_agent(args, &defender_ctx) {
                    Ok(msg) => result(PLACEHOLDER_TOOL, true, msg),
                    Err(e) => result(PLACEHOLDER_TOOL, false, e),
                },
            }
        }
    } else if let Some(dispatcher_tool) = DispatcherTool::from_api_name(name) {
        // ---- Dispatcher tools (issue #24) ----------------------------------
        if let Err(msg) = check_capability(ctx.role, dispatcher_tool.class()) {
            result(PLACEHOLDER_TOOL, false, msg)
        } else {
            match ctx.ticket_dispatcher.as_ref() {
                None => result(
                    PLACEHOLDER_TOOL,
                    false,
                    "ticket dispatcher not configured".into(),
                ),
                Some(d) => {
                    let disp_ctx = DispatcherToolContext::new(Arc::clone(d));
                    match dispatcher_tool {
                        DispatcherTool::RequestNextTicket => {
                            match request_next_ticket(args, &disp_ctx) {
                                Ok(Some(ticket)) => {
                                    let body = serde_json::to_string(&ticket)
                                        .unwrap_or_else(|e| format!("encode error: {e}"));
                                    result(PLACEHOLDER_TOOL, true, body)
                                }
                                Ok(None) => result(PLACEHOLDER_TOOL, true, "null".into()),
                                Err(e) => result(PLACEHOLDER_TOOL, false, e),
                            }
                        }
                        DispatcherTool::MarkTicketInProgress => {
                            match mark_ticket_in_progress(args, &disp_ctx) {
                                Ok(msg) => result(PLACEHOLDER_TOOL, true, msg),
                                Err(e) => result(PLACEHOLDER_TOOL, false, e),
                            }
                        }
                        DispatcherTool::MarkTicketDone => {
                            match mark_ticket_done(args, &disp_ctx) {
                                Ok(msg) => result(PLACEHOLDER_TOOL, true, msg),
                                Err(e) => result(PLACEHOLDER_TOOL, false, e),
                            }
                        }
                    }
                }
            }
        }
    } else {
        // ---- Unknown -------------------------------------------------------
        result(PLACEHOLDER_TOOL, false, format!("unknown tool: {name}"))
    };

    // ---- Correlation ID: emit tool.invoked event with correlation_id -------
    //
    // When the dispatch context carries a correlation id AND an event log,
    // append a `tool.invoked` envelope so every tool call in a tracked
    // pipeline run is durably recorded with the causality token.  The id is
    // stored as a string payload field (`"correlation_id"`) so consumers can
    // query `WHERE payload->>'correlation_id' = ?` without needing a schema
    // migration on `EventEnvelope` itself.
    //
    // The append is best-effort: a poisoned lock or I/O error is swallowed
    // rather than converting a successful tool result into a failure.  Tracing
    // loss is preferable to breaking the agent loop.
    if let (Some(cid), Some(log)) = (ctx.correlation_id.as_ref(), ctx.event_log.as_ref()) {
        let mut payload = serde_json::json!({
            "tool": name,
            "agent_id": ctx.self_ref.id,
            "success": tool_result.success,
            "correlation_id": cid.to_string(),
        });
        if let Some(src_id) = ctx.source_event_id {
            payload["source_event_id"] = serde_json::Value::from(src_id);
        }
        if let Ok(mut guard) = log.lock() {
            let _ = guard.append(
                phantom_memory::event_log::EventSource::Agent { id: ctx.self_ref.id },
                "tool.invoked",
                payload,
            );
        }
    }

    // ---- Sec.7.2: Taint elevation via source_event_id chain walk -----------
    //
    // Walk the source_event_id chain backwards in the event log and elevate
    // result taint when upstream sources are denied or quarantined. Runs after
    // every fork so even capability-denied and unknown-tool results inherit
    // taint from their call context.
    if let (Some(start_id), Some(log)) = (ctx.source_event_id, ctx.event_log.as_ref()) {
        let chain_taint = taint_from_source_chain(start_id, log, &ctx.registry);
        tool_result.taint = tool_result.taint.merge(chain_taint);
    }

    tool_result
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::composer_tools::new_spawn_subagent_queue;
    use crate::inbox::{AgentHandle, AgentStatus, InboxMessage};
    use crate::role::SpawnSource;
    use phantom_memory::event_log::{EventLog, EventSource as LogEventSource};
    use serde_json::json;
    use std::fs;
    use tempfile::TempDir;
    use tokio::sync::{mpsc, watch};

    /// Build a fake registered agent with a receiver half so tests can
    /// observe what (if anything) was delivered.
    fn fake_agent(
        id: u64,
        role: AgentRole,
        label: &str,
    ) -> (AgentHandle, mpsc::Receiver<InboxMessage>) {
        let (tx, rx) = mpsc::channel(8);
        let (_status_tx, status_rx) = watch::channel(AgentStatus::Idle);
        let handle = AgentHandle {
            agent_ref: AgentRef::new(id, role, label, SpawnSource::Substrate),
            inbox: tx,
            status: status_rx,
        };
        (handle, rx)
    }

    /// Build a fake registered agent with a controllable status sender.
    fn fake_agent_with_status(
        id: u64,
        role: AgentRole,
        label: &str,
        initial_status: AgentStatus,
    ) -> (AgentHandle, mpsc::Receiver<InboxMessage>, watch::Sender<AgentStatus>) {
        let (tx, rx) = mpsc::channel(8);
        let (status_tx, status_rx) = watch::channel(initial_status);
        let handle = AgentHandle {
            agent_ref: AgentRef::new(id, role, label, SpawnSource::Substrate),
            inbox: tx,
            status: status_rx,
        };
        (handle, rx, status_tx)
    }

    /// Build a [`DispatchContext`] for the given calling agent with an
    /// empty registry, no event log, and a fresh spawn queue.
    fn build_ctx<'a>(
        self_id: u64,
        role: AgentRole,
        label: &str,
        working_dir: &'a Path,
    ) -> DispatchContext<'a> {
        let registry = Arc::new(Mutex::new(AgentRegistry::new()));
        let pending_spawn = new_spawn_subagent_queue();
        let self_ref = AgentRef::new(self_id, role, label, SpawnSource::User);
        DispatchContext {
            self_ref,
            role,
            working_dir,
            registry,
            event_log: None,
            pending_spawn,
            source_event_id: None,
            quarantine: None,
            correlation_id: None,
            ticket_dispatcher: None,
        }
    }

    // ---- File/git surface --------------------------------------------------

    #[test]
    fn dispatch_routes_read_file_to_tools_module() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("hello.txt"), "phantom-says-hi").unwrap();

        let ctx = build_ctx(1, AgentRole::Conversational, "speaker", tmp.path());

        let result = dispatch_tool(
            "read_file",
            &json!({"path": "hello.txt"}),
            &ctx,
        );

        assert!(result.success, "dispatch should succeed: {}", result.output);
        assert_eq!(result.output, "phantom-says-hi");
    }

    // ---- Chat tools surface ------------------------------------------------

    #[tokio::test]
    async fn dispatch_routes_send_to_agent_to_chat_tools() {
        let tmp = TempDir::new().unwrap();

        // Register two agents — sender (id=1) and recipient (id=2).
        let (sender_handle, _sender_rx) =
            fake_agent(1, AgentRole::Conversational, "sender");
        let (recipient_handle, mut recipient_rx) =
            fake_agent(2, AgentRole::Watcher, "recipient");

        let mut reg = AgentRegistry::new();
        reg.register(sender_handle);
        reg.register(recipient_handle);
        let registry = Arc::new(Mutex::new(reg));

        let ctx = DispatchContext {
            self_ref: AgentRef::new(1, AgentRole::Conversational, "sender", SpawnSource::User),
            role: AgentRole::Conversational,
            working_dir: tmp.path(),
            registry,
            event_log: None,
            pending_spawn: new_spawn_subagent_queue(),
            source_event_id: None,
            quarantine: None,
            correlation_id: None,
            ticket_dispatcher: None,
        };

        let result = dispatch_tool(
            "send_to_agent",
            &json!({"label": "recipient", "body": "hello peer"}),
            &ctx,
        );
        assert!(result.success, "send_to_agent dispatch failed: {}", result.output);
        assert!(result.output.contains("recipient"));

        // Recipient inbox must have received the AgentSpeak.
        let msg = recipient_rx
            .try_recv()
            .expect("recipient inbox must contain message");
        match msg {
            InboxMessage::AgentSpeak { from, body } => {
                assert_eq!(from.id, 1);
                assert_eq!(body, "hello peer");
            }
            other => panic!("wrong inbox message: {other:?}"),
        }
    }

    // ---- Composer tools surface --------------------------------------------

    #[test]
    fn dispatch_routes_spawn_subagent_to_composer_tools() {
        let tmp = TempDir::new().unwrap();
        let ctx = build_ctx(42, AgentRole::Composer, "composer", tmp.path());

        let result = dispatch_tool(
            "spawn_subagent",
            &json!({
                "role": "watcher",
                "label": "child-watcher",
                "task": "watch the build",
            }),
            &ctx,
        );

        assert!(
            result.success,
            "spawn_subagent dispatch failed: {}",
            result.output,
        );

        let q = ctx.pending_spawn.lock().unwrap();
        assert_eq!(q.len(), 1, "exactly one request must be queued");
        let req = &q[0];
        assert_eq!(req.role, AgentRole::Watcher);
        assert_eq!(req.label, "child-watcher");
        assert_eq!(req.task, "watch the build");
        assert_eq!(req.parent, 42);
    }

    // ---- Unknown name ------------------------------------------------------

    #[test]
    fn dispatch_unknown_name_returns_failed_tool_result() {
        let tmp = TempDir::new().unwrap();
        let ctx = build_ctx(1, AgentRole::Conversational, "speaker", tmp.path());

        let result = dispatch_tool("not_a_real_tool", &json!({}), &ctx);

        assert!(
            !result.success,
            "unknown tool dispatch must surface success=false",
        );
        assert!(
            result.output.starts_with("unknown tool"),
            "expected 'unknown tool' prefix, got: {}",
            result.output,
        );
        assert!(
            result.output.contains("not_a_real_tool"),
            "error must echo the bogus name, got: {}",
            result.output,
        );
    }

    // ---- Capability denial -------------------------------------------------

    #[test]
    fn dispatch_capability_denied_returns_structured_error() {
        // Watcher manifest has Sense+Reflect+Compute. `run_command` is Act.
        // Dispatch must short-circuit before any shell process spawns, with
        // the canonical "capability denied: <Class> not in <Role> manifest"
        // wording the model self-corrects on.
        let tmp = TempDir::new().unwrap();
        let ctx = build_ctx(1, AgentRole::Watcher, "watcher", tmp.path());

        let result = dispatch_tool(
            "run_command",
            &json!({"command": "echo SHOULD_NEVER_RUN"}),
            &ctx,
        );

        assert!(!result.success, "capability denial must yield success=false");
        assert!(
            result.output.starts_with("capability denied:"),
            "expected canonical phrasing, got: {}",
            result.output,
        );
        assert!(result.output.contains("Act"));
        assert!(result.output.contains("Watcher"));
        assert!(
            !result.output.contains("SHOULD_NEVER_RUN"),
            "shell command must not have run; got: {}",
            result.output,
        );
    }

    #[test]
    fn dispatch_capability_denied_for_chat_tool() {
        // Transcriber's manifest is Compute+Reflect — no Sense. The
        // `send_to_agent` tool is Sense-class, so it must be denied even
        // though the registry would happily accept the message.
        let tmp = TempDir::new().unwrap();
        let (recipient_handle, _rx) =
            fake_agent(2, AgentRole::Watcher, "recipient");
        let mut reg = AgentRegistry::new();
        reg.register(recipient_handle);
        let registry = Arc::new(Mutex::new(reg));

        let ctx = DispatchContext {
            self_ref: AgentRef::new(1, AgentRole::Transcriber, "x", SpawnSource::User),
            role: AgentRole::Transcriber,
            working_dir: tmp.path(),
            registry,
            event_log: None,
            pending_spawn: new_spawn_subagent_queue(),
            source_event_id: None,
            quarantine: None,
            correlation_id: None,
            ticket_dispatcher: None,
        };

        let result = dispatch_tool(
            "send_to_agent",
            &json!({"label": "recipient", "body": "denied"}),
            &ctx,
        );

        assert!(!result.success, "Transcriber must not be allowed Sense tools");
        assert!(
            result.output.starts_with("capability denied:"),
            "expected canonical phrasing, got: {}",
            result.output,
        );
        assert!(result.output.contains("Sense"));
        assert!(result.output.contains("Transcriber"));
    }

    // ---- Sec.7.2: taint elevation via source_event_id chain walk -----------

    /// Helper: open a temp EventLog file in the given directory.
    fn open_event_log(dir: &Path) -> Arc<Mutex<EventLog>> {
        let log = EventLog::open(&dir.join("events.jsonl")).unwrap();
        Arc::new(Mutex::new(log))
    }

    /// Collect the ordered list of event IDs reachable by walking the
    /// `source_event_id` chain backwards from `start_id`.
    ///
    /// This helper mirrors the walk that [`taint_from_source_chain`] performs
    /// but returns the event IDs rather than an aggregated [`TaintLevel`],
    /// making it straightforward to assert which events are (and are not)
    /// visited in chain-walk tests (#219 — function was referenced in planned
    /// tests but never defined).
    fn collect_source_chain(
        start_id: u64,
        log: &Arc<Mutex<EventLog>>,
    ) -> Vec<u64> {
        let tail = {
            let g = log.lock().unwrap();
            g.tail(usize::MAX)
        };
        let by_id: std::collections::HashMap<u64, &phantom_memory::event_log::EventEnvelope> =
            tail.iter().map(|e| (e.id, e)).collect();

        let mut chain = Vec::new();
        let mut cursor: Option<u64> = Some(start_id);
        let mut visited: std::collections::HashSet<u64> = std::collections::HashSet::new();

        while let Some(id) = cursor {
            if !visited.insert(id) {
                break; // cycle guard
            }
            let Some(ev) = by_id.get(&id) else {
                break; // event not in tail
            };
            chain.push(id);
            cursor = ev.payload.get("source_event_id").and_then(|v| v.as_u64());
        }
        chain
    }

    /// Verify that `collect_source_chain` walks a linear three-event chain
    /// correctly and returns IDs in traversal order (newest → oldest).
    #[test]
    fn collect_source_chain_walks_linear_chain() {
        let tmp = TempDir::new().unwrap();
        let log = open_event_log(tmp.path());

        // Build a three-event chain: ev3 → ev2 → ev1 (via source_event_id).
        let ev1_id = {
            let mut g = log.lock().unwrap();
            g.append(LogEventSource::Substrate, "root", json!({}))
                .unwrap()
                .id
        };
        let ev2_id = {
            let mut g = log.lock().unwrap();
            g.append(
                LogEventSource::Substrate,
                "middle",
                json!({ "source_event_id": ev1_id }),
            )
            .unwrap()
            .id
        };
        let ev3_id = {
            let mut g = log.lock().unwrap();
            g.append(
                LogEventSource::Substrate,
                "leaf",
                json!({ "source_event_id": ev2_id }),
            )
            .unwrap()
            .id
        };

        let chain = collect_source_chain(ev3_id, &log);
        assert_eq!(
            chain,
            vec![ev3_id, ev2_id, ev1_id],
            "chain must walk ev3 → ev2 → ev1 in traversal order",
        );
    }

    /// `collect_source_chain` must terminate on a self-referential event
    /// (cycle guard) and return only the event IDs visited before the cycle
    /// was detected.
    #[test]
    fn collect_source_chain_terminates_on_cycle() {
        let tmp = TempDir::new().unwrap();
        let log = open_event_log(tmp.path());

        // Create an event that references itself.
        let ev_id = {
            let mut g = log.lock().unwrap();
            // Append placeholder to learn the id.
            let placeholder = g
                .append(LogEventSource::Substrate, "self-ref", json!({}))
                .unwrap();
            let self_id = placeholder.id;
            // Append the actual self-referential event (id = self_id + 1).
            g.append(
                LogEventSource::Substrate,
                "self-ref",
                json!({ "source_event_id": self_id + 1 }),
            )
            .unwrap()
            .id
        };

        // Must return exactly one element and not loop forever.
        let chain = collect_source_chain(ev_id, &log);
        assert_eq!(chain.len(), 1, "self-referential chain must yield exactly one id");
        assert_eq!(chain[0], ev_id);
    }

    /// Acceptance test 1 (Sec.7.2): clean source chain → result taint is `Clean`.
    ///
    /// When `source_event_id` points to an event with a benign kind (not
    /// `"capability.denied"`) and the source agent is not quarantined (`Failed`),
    /// the result taint must remain `Clean`.
    #[test]
    fn dispatch_clean_source_chain_taint_is_clean() {
        // Arrange: benign upstream event, Idle source agent.
        let tmp = TempDir::new().unwrap();
        let log = open_event_log(tmp.path());
        fs::write(tmp.path().join("probe.txt"), "hello").unwrap();

        let upstream_id = {
            let mut g = log.lock().unwrap();
            g.append(
                LogEventSource::Agent { id: 10 },
                "tool.invoked",
                json!({ "tool": "read_file" }),
            )
            .unwrap()
            .id
        };

        let ctx = DispatchContext {
            self_ref: AgentRef::new(10, AgentRole::Conversational, "agent-10", SpawnSource::User),
            role: AgentRole::Conversational,
            working_dir: tmp.path(),
            registry: Arc::new(Mutex::new(AgentRegistry::new())),
            event_log: Some(log),
            pending_spawn: new_spawn_subagent_queue(),
            source_event_id: Some(upstream_id),
            quarantine: None,
            correlation_id: None,
            ticket_dispatcher: None,
        };

        // Act.
        let res = dispatch_tool("read_file", &json!({"path": "probe.txt"}), &ctx);

        // Assert.
        assert!(res.success, "dispatch should succeed: {}", res.output);
        assert_eq!(
            res.taint,
            TaintLevel::Clean,
            "clean upstream chain must yield Clean taint, got {:?}",
            res.taint,
        );
    }

    /// Acceptance test 2 (Sec.7.2): source chain contains a `CapabilityDenied`
    /// event → result taint is `Suspect`.
    ///
    /// When the upstream event has `kind == "capability.denied"`, taint must
    /// be elevated to at least `Suspect`.
    #[test]
    fn dispatch_capability_denied_upstream_taint_is_suspect() {
        // Arrange: upstream event has the canonical denied kind.
        let tmp = TempDir::new().unwrap();
        let log = open_event_log(tmp.path());
        fs::write(tmp.path().join("probe.txt"), "hello").unwrap();

        let denied_event_id = {
            let mut g = log.lock().unwrap();
            g.append(
                LogEventSource::Substrate,
                KIND_CAPABILITY_DENIED,
                json!({
                    "agent_id": 42,
                    "attempted_tool": "run_command",
                }),
            )
            .unwrap()
            .id
        };

        let ctx = DispatchContext {
            self_ref: AgentRef::new(42, AgentRole::Conversational, "agent-42", SpawnSource::User),
            role: AgentRole::Conversational,
            working_dir: tmp.path(),
            registry: Arc::new(Mutex::new(AgentRegistry::new())),
            event_log: Some(log),
            pending_spawn: new_spawn_subagent_queue(),
            source_event_id: Some(denied_event_id),
            quarantine: None,
            correlation_id: None,
            ticket_dispatcher: None,
        };

        // Act.
        let res = dispatch_tool("read_file", &json!({"path": "probe.txt"}), &ctx);

        // Assert.
        assert!(res.success, "dispatch should succeed: {}", res.output);
        assert_eq!(
            res.taint,
            TaintLevel::Suspect,
            "upstream CapabilityDenied must elevate taint to Suspect, got {:?}",
            res.taint,
        );
    }

    /// Acceptance test 3 (Sec.7.2): source agent is quarantined (`Failed`) →
    /// result taint is `Tainted`.
    ///
    /// When the upstream event originates from an agent whose registry status
    /// is `Failed`, the taint must be elevated to `Tainted`.
    #[test]
    fn dispatch_quarantined_source_agent_taint_is_tainted() {
        // Arrange: register an agent and move it to Failed (quarantined).
        let tmp = TempDir::new().unwrap();
        let log = open_event_log(tmp.path());
        fs::write(tmp.path().join("probe.txt"), "hello").unwrap();

        let quarantined_id: u64 = 77;
        let (handle, _rx, status_tx) = fake_agent_with_status(
            quarantined_id,
            AgentRole::Watcher,
            "quarantined",
            AgentStatus::Idle,
        );
        let registry = Arc::new(Mutex::new(AgentRegistry::new()));
        registry.lock().unwrap().register(handle);
        // Transition to Failed — marks this agent as quarantined.
        status_tx.send(AgentStatus::Failed).unwrap();

        // Upstream event is sourced from the quarantined agent (no denied kind —
        // pure quarantine signal).
        let upstream_id = {
            let mut g = log.lock().unwrap();
            g.append(
                LogEventSource::Agent { id: quarantined_id },
                "tool.invoked",
                json!({ "tool": "read_file" }),
            )
            .unwrap()
            .id
        };

        let ctx = DispatchContext {
            self_ref: AgentRef::new(99, AgentRole::Conversational, "caller", SpawnSource::User),
            role: AgentRole::Conversational,
            working_dir: tmp.path(),
            registry,
            event_log: Some(log),
            pending_spawn: new_spawn_subagent_queue(),
            source_event_id: Some(upstream_id),
            quarantine: None,
            correlation_id: None,
            ticket_dispatcher: None,
        };

        // Act.
        let res = dispatch_tool("read_file", &json!({"path": "probe.txt"}), &ctx);

        // Assert.
        assert!(res.success, "dispatch should succeed: {}", res.output);
        assert_eq!(
            res.taint,
            TaintLevel::Tainted,
            "quarantined source agent must elevate taint to Tainted, got {:?}",
            res.taint,
        );
    }

    // ---- Cycle detection in source-event chain --------------------------------

    #[test]
    fn dispatch_self_referential_chain_does_not_loop() {
        // Create an event whose `source_event_id` payload field points to its
        // own id. The walk must terminate (not spin forever) and return a
        // deterministic taint level.
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("f.txt"), "hi").unwrap();

        let path = tmp.path().join("events.jsonl");
        let event_id = {
            let mut raw = EventLog::open(&path).unwrap();
            // Append a placeholder first so we know the id that will be assigned.
            let placeholder = raw
                .append(
                    phantom_memory::event_log::EventSource::Substrate,
                    "agent.speak",
                    serde_json::json!({}),
                )
                .unwrap();
            let self_id = placeholder.id;

            // Re-open in append mode and write a corrected version that points to
            // itself via source_event_id. Since EventLog assigns monotonic ids we
            // cannot easily mutate; instead we directly append a second event that
            // has source_event_id == its own id (id=2 → source_event_id=2).
            let self_ref_ev = raw
                .append(
                    phantom_memory::event_log::EventSource::Substrate,
                    "agent.speak",
                    serde_json::json!({ "source_event_id": self_id + 1 }),
                )
                .unwrap();
            self_ref_ev.id
        };

        let log = Arc::new(Mutex::new(EventLog::open(&path).unwrap()));

        let ctx = DispatchContext {
            self_ref: AgentRef::new(1, AgentRole::Conversational, "a", SpawnSource::User),
            role: AgentRole::Conversational,
            working_dir: tmp.path(),
            registry: Arc::new(Mutex::new(AgentRegistry::new())),
            event_log: Some(log),
            pending_spawn: new_spawn_subagent_queue(),
            source_event_id: Some(event_id),
            quarantine: None,
            correlation_id: None,
            ticket_dispatcher: None,
        };

        // This call must return — any infinite loop would cause the test to hang
        // and be caught by the test harness timeout.
        let r = dispatch_tool("read_file", &serde_json::json!({"path": "f.txt"}), &ctx);
        assert!(r.success, "dispatch must succeed on self-referential chain");
        // Taint is Clean because neither event is capability.denied nor from a
        // quarantined agent.
        assert_eq!(
            r.taint,
            crate::taint::TaintLevel::Clean,
            "self-referential clean chain must not elevate taint",
        );
    }

    // ---- Sec.7.3: QuarantineRegistry dispatch gate -------------------------

    /// Sec.7.3: A quarantined agent must have all tool dispatches denied,
    /// regardless of capability class or tool name.
    ///
    /// When `DispatchContext::quarantine` holds a registry that reports the
    /// calling agent as quarantined, `dispatch_tool` must short-circuit with
    /// `success: false` before any capability check or handler runs.
    #[test]
    fn dispatch_denied_for_quarantined_agent() {
        use crate::quarantine::{AutoQuarantinePolicy, QuarantineRegistry};

        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("probe.txt"), "hello").unwrap();

        let agent_id = 55u64;

        // Build a registry and quarantine the agent immediately (threshold=1).
        let quarantine = Arc::new(Mutex::new(QuarantineRegistry::new_with_policy(
            AutoQuarantinePolicy { threshold: 1 },
        )));
        quarantine
            .lock()
            .unwrap()
            .check_and_escalate(agent_id, TaintLevel::Tainted, 0, "repeated violation");

        // Confirm the agent is quarantined in the registry.
        assert!(quarantine.lock().unwrap().agent_is_quarantined(agent_id));

        let ctx = DispatchContext {
            self_ref: AgentRef::new(
                agent_id,
                AgentRole::Conversational,
                "offender",
                SpawnSource::User,
            ),
            role: AgentRole::Conversational,
            working_dir: tmp.path(),
            registry: Arc::new(Mutex::new(AgentRegistry::new())),
            event_log: None,
            pending_spawn: new_spawn_subagent_queue(),
            source_event_id: None,
            quarantine: Some(quarantine),
            correlation_id: None,
            ticket_dispatcher: None,
        };

        // A normal file-read that would otherwise succeed must be denied.
        let res = dispatch_tool("read_file", &json!({"path": "probe.txt"}), &ctx);

        assert!(
            !res.success,
            "quarantined agent must have dispatch denied, got success=true"
        );
        assert!(
            res.output.contains("quarantined"),
            "denial message must mention 'quarantined', got: {}",
            res.output,
        );
        assert!(
            res.output.contains(&agent_id.to_string()),
            "denial message must name the agent id, got: {}",
            res.output,
        );
    }

    /// Issue #170 — QA: quarantine release — clean calls succeed post-release.
    ///
    /// An agent is auto-quarantined by driving 3 consecutive `TaintLevel::Tainted`
    /// observations through `check_and_escalate` (matching the default threshold).
    /// After `release`, the agent's state must be `Clean` and a permitted tool
    /// call dispatched through `dispatch_tool` must succeed — not be blocked as
    /// if still quarantined.
    ///
    /// Steps:
    /// 1. Drive 3 `Tainted` events → assert `QuarantineState::Quarantined`.
    /// 2. Call `release` → assert `QuarantineState::Clean`.
    /// 3. Dispatch a permitted `read_file` → assert `success: true`.
    #[test]
    fn dispatch_succeeds_after_quarantine_release() {
        use crate::quarantine::{QuarantineRegistry, QuarantineState};

        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("data.txt"), "post-release-content").unwrap();

        let agent_id = 170u64;

        // Step 1 — auto-quarantine via 3 consecutive Tainted observations.
        let quarantine = Arc::new(Mutex::new(QuarantineRegistry::new()));
        {
            let mut reg = quarantine.lock().unwrap();
            for i in 0..3 {
                reg.check_and_escalate(
                    agent_id,
                    TaintLevel::Tainted,
                    1_000 + i as u64,
                    format!("capability denied offense {}", i + 1),
                );
            }
            assert!(
                reg.agent_is_quarantined(agent_id),
                "agent must be quarantined after 3 consecutive Tainted observations"
            );
            assert!(
                matches!(reg.state_of(agent_id), QuarantineState::Quarantined { .. }),
                "state must be Quarantined before release"
            );
        }

        // Confirm the quarantine gate blocks dispatch before release.
        {
            let ctx = DispatchContext {
                self_ref: AgentRef::new(
                    agent_id,
                    AgentRole::Conversational,
                    "offender",
                    SpawnSource::User,
                ),
                role: AgentRole::Conversational,
                working_dir: tmp.path(),
                registry: Arc::new(Mutex::new(AgentRegistry::new())),
                event_log: None,
                pending_spawn: new_spawn_subagent_queue(),
                source_event_id: None,
                quarantine: Some(Arc::clone(&quarantine)),
                correlation_id: None,
                ticket_dispatcher: None,
            };
            let blocked = dispatch_tool("read_file", &json!({"path": "data.txt"}), &ctx);
            assert!(
                !blocked.success,
                "dispatch must be blocked while quarantined, got success=true"
            );
            assert!(
                blocked.output.contains("quarantined"),
                "blocked output must mention 'quarantined': {}",
                blocked.output,
            );
        }

        // Step 2 — release the quarantine.
        {
            let mut reg = quarantine.lock().unwrap();
            reg.release(agent_id);
            assert!(
                !reg.agent_is_quarantined(agent_id),
                "agent must not be quarantined after release"
            );
            assert_eq!(
                reg.state_of(agent_id),
                QuarantineState::Clean,
                "state must be Clean after release"
            );
        }

        // Step 3 — permitted tool call must now succeed.
        let ctx = DispatchContext {
            self_ref: AgentRef::new(
                agent_id,
                AgentRole::Conversational,
                "released-agent",
                SpawnSource::User,
            ),
            role: AgentRole::Conversational,
            working_dir: tmp.path(),
            registry: Arc::new(Mutex::new(AgentRegistry::new())),
            event_log: None,
            pending_spawn: new_spawn_subagent_queue(),
            source_event_id: None,
            quarantine: Some(Arc::clone(&quarantine)),
            correlation_id: None,
            ticket_dispatcher: None,
        };

        let res = dispatch_tool("read_file", &json!({"path": "data.txt"}), &ctx);

        assert!(
            res.success,
            "released agent must be able to dispatch tools, got: {}",
            res.output
        );
        assert_eq!(
            res.output, "post-release-content",
            "dispatch must return the file contents after release"
        );
    }

    /// Sec.7.3: A non-quarantined agent must still dispatch normally even
    /// when a quarantine registry is wired into the context.
    ///
    /// The gate must be transparent for agents that are `Clean`.
    #[test]
    fn dispatch_allowed_for_clean_agent_with_quarantine_registry() {
        use crate::quarantine::QuarantineRegistry;

        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("probe.txt"), "clean-agent-data").unwrap();

        let clean_agent_id = 66u64;

        // Build an empty quarantine registry — the agent has never been recorded.
        let quarantine = Arc::new(Mutex::new(QuarantineRegistry::new()));

        let ctx = DispatchContext {
            self_ref: AgentRef::new(
                clean_agent_id,
                AgentRole::Conversational,
                "clean-agent",
                SpawnSource::User,
            ),
            role: AgentRole::Conversational,
            working_dir: tmp.path(),
            registry: Arc::new(Mutex::new(AgentRegistry::new())),
            event_log: None,
            pending_spawn: new_spawn_subagent_queue(),
            source_event_id: None,
            quarantine: Some(quarantine),
            correlation_id: None,
            ticket_dispatcher: None,
        };

        let res = dispatch_tool("read_file", &json!({"path": "probe.txt"}), &ctx);

        assert!(
            res.success,
            "clean agent must still dispatch normally when quarantine registry is wired: {}",
            res.output,
        );
        assert_eq!(res.output, "clean-agent-data");
    }

    // ---- Correlation ID on DispatchContext ------------------------------------

    /// A `DispatchContext` built with `correlation_id: Some(id)` must
    /// successfully route the tool — the correlation field is metadata only and
    /// must not affect routing or capability checks.
    #[test]
    fn dispatch_with_correlation_id_routes_normally() {
        use crate::correlation::CorrelationId;

        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("corr.txt"), "hello-correlation").unwrap();

        let cid = CorrelationId::new();
        let ctx = DispatchContext {
            self_ref: AgentRef::new(1, AgentRole::Conversational, "traced-agent", SpawnSource::User),
            role: AgentRole::Conversational,
            working_dir: tmp.path(),
            registry: Arc::new(Mutex::new(AgentRegistry::new())),
            event_log: None,
            pending_spawn: new_spawn_subagent_queue(),
            source_event_id: None,
            quarantine: None,
            correlation_id: Some(cid),
            ticket_dispatcher: None,
        };

        let res = dispatch_tool("read_file", &json!({"path": "corr.txt"}), &ctx);

        assert!(
            res.success,
            "dispatch with correlation_id must succeed normally: {}",
            res.output,
        );
        assert_eq!(
            res.output, "hello-correlation",
            "output must be the file contents, got: {}",
            res.output,
        );
    }

    /// Two [`DispatchContext`]s built with the same [`CorrelationId`] should
    /// each route independently — the id is carried in the context but does
    /// not change the routing outcome.
    #[test]
    fn dispatch_with_shared_correlation_id_routes_independently() {
        use crate::correlation::CorrelationId;

        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("a.txt"), "file-a").unwrap();
        fs::write(tmp.path().join("b.txt"), "file-b").unwrap();

        let cid = CorrelationId::new();

        let ctx_a = DispatchContext {
            self_ref: AgentRef::new(1, AgentRole::Conversational, "agent-a", SpawnSource::User),
            role: AgentRole::Conversational,
            working_dir: tmp.path(),
            registry: Arc::new(Mutex::new(AgentRegistry::new())),
            event_log: None,
            pending_spawn: new_spawn_subagent_queue(),
            source_event_id: None,
            quarantine: None,
            correlation_id: Some(cid),
            ticket_dispatcher: None,
        };

        let ctx_b = DispatchContext {
            self_ref: AgentRef::new(2, AgentRole::Conversational, "agent-b", SpawnSource::User),
            role: AgentRole::Conversational,
            working_dir: tmp.path(),
            registry: Arc::new(Mutex::new(AgentRegistry::new())),
            event_log: None,
            pending_spawn: new_spawn_subagent_queue(),
            source_event_id: None,
            quarantine: None,
            correlation_id: Some(cid),
            ticket_dispatcher: None,
        };

        let res_a = dispatch_tool("read_file", &json!({"path": "a.txt"}), &ctx_a);
        let res_b = dispatch_tool("read_file", &json!({"path": "b.txt"}), &ctx_b);

        assert!(res_a.success, "agent-a dispatch must succeed: {}", res_a.output);
        assert!(res_b.success, "agent-b dispatch must succeed: {}", res_b.output);
        assert_eq!(res_a.output, "file-a");
        assert_eq!(res_b.output, "file-b");
    }

    // ---- Issue #163: taint escalation from denied-capability source chain ----
    //
    // Security property: when a CapabilityDenied event is in the source chain,
    // the caller's result must NOT stay Clean.  taint_from_source_chain walks
    // the upstream event log and merges Suspect onto any result whose
    // source_event_id traces back through a "capability.denied" event.

    /// A CapabilityDenied event in the source chain propagates Suspect taint.
    ///
    /// Issue #163 — when a tool call is denied due to a capability check and
    /// that denial is recorded in the event log, any subsequent result whose
    /// `source_event_id` points back to that `"capability.denied"` event must
    /// carry at least `TaintLevel::Suspect`.  Taint must not drop to `Clean`.
    #[test]
    fn capability_denied_source_chain_propagates_taint() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("data.txt"), "safe").unwrap();

        let log = open_event_log(tmp.path());

        // Append a CapabilityDenied event — simulates what the dispatch gate
        // writes when an agent calls a tool outside its manifest.
        let denied_event_id = {
            let mut g = log.lock().unwrap();
            g.append(
                LogEventSource::Substrate,
                KIND_CAPABILITY_DENIED,
                json!({
                    "agent_id": 5,
                    "attempted_tool": "run_command",
                    "role": "Watcher",
                }),
            )
            .unwrap()
            .id
        };

        // A subsequent dispatch whose source_event_id points at the denied
        // event.  Even though *this* call is a valid read, the upstream denial
        // must surface as at least Suspect taint.
        let ctx = DispatchContext {
            self_ref: AgentRef::new(5, AgentRole::Conversational, "agent-5", SpawnSource::User),
            role: AgentRole::Conversational,
            working_dir: tmp.path(),
            registry: Arc::new(Mutex::new(AgentRegistry::new())),
            event_log: Some(log),
            pending_spawn: new_spawn_subagent_queue(),
            source_event_id: Some(denied_event_id),
            quarantine: None,
            correlation_id: None,
            ticket_dispatcher: None,
        };

        let res = dispatch_tool("read_file", &json!({"path": "data.txt"}), &ctx);

        assert!(res.success, "dispatch should succeed: {}", res.output);

        // Taint must NOT be Clean — the chain contains a CapabilityDenied event.
        assert!(
            res.taint != TaintLevel::Clean,
            "issue #163: taint must not remain Clean when source chain contains \
             a CapabilityDenied event; got {:?}",
            res.taint,
        );
        assert_eq!(
            res.taint,
            TaintLevel::Suspect,
            "issue #163: CapabilityDenied in source chain must yield exactly Suspect; \
             got {:?}",
            res.taint,
        );
    }

    // ---- Issue #166: Watcher role blocked from RunCommand --------------------
    //
    // Security property: a Watcher agent (Sense+Reflect+Compute, no Act)
    // must never be able to execute a shell command through dispatch_tool.
    // The dispatch gate must deny before any shell process starts.

    /// Watcher role must receive CapabilityDenied when attempting run_command.
    ///
    /// Issue #166 — capability boundary: an agent with `AgentRole::Watcher`
    /// manifest (Sense+Reflect+Compute, no Act) that calls `run_command`
    /// (Act-class) must receive a denial with the canonical
    /// `"capability denied: Act not in Watcher manifest"` message.
    /// The shell command must never execute.
    #[test]
    fn watcher_role_blocked_from_run_command() {
        let tmp = TempDir::new().unwrap();

        // Watcher manifest has Sense+Reflect+Compute, but NOT Act.
        let ctx = DispatchContext {
            self_ref: AgentRef::new(20, AgentRole::Watcher, "watcher-agent", SpawnSource::User),
            role: AgentRole::Watcher,
            working_dir: tmp.path(),
            registry: Arc::new(Mutex::new(AgentRegistry::new())),
            event_log: None,
            pending_spawn: new_spawn_subagent_queue(),
            source_event_id: None,
            quarantine: None,
            correlation_id: None,
            ticket_dispatcher: None,
        };

        // Attempt to invoke run_command — Act-class tool.
        let res = dispatch_tool(
            "run_command",
            &json!({ "command": "echo WATCHER_BREACH" }),
            &ctx,
        );

        // Must be denied with success=false.
        assert!(
            !res.success,
            "issue #166: Watcher must be denied run_command; got success=true with: {}",
            res.output,
        );
        // Must carry canonical capability denied wording.
        assert!(
            res.output.starts_with("capability denied:"),
            "issue #166: denial must start with 'capability denied:'; got: {}",
            res.output,
        );
        assert!(
            res.output.contains("Act"),
            "issue #166: denial must name the missing class 'Act'; got: {}",
            res.output,
        );
        assert!(
            res.output.contains("Watcher"),
            "issue #166: denial must name the role 'Watcher'; got: {}",
            res.output,
        );
        // The shell command must never have run.
        assert!(
            !res.output.contains("WATCHER_BREACH"),
            "issue #166: shell command must not have run — sentinel in output: {}",
            res.output,
        );
    }

    // ---- Issue #214: correlation_id propagates into the event log -----------

    /// Acceptance test (issue #214): a tool dispatched with a known
    /// [`CorrelationId`] must emit a `tool.invoked` [`EventEnvelope`] into the
    /// event log whose payload contains `"correlation_id"` matching the
    /// originating id.
    ///
    /// Verifies the full path:
    ///   `DispatchContext::correlation_id`
    ///   → `dispatch_tool` emit
    ///   → `EventLog::append`
    ///   → payload field `"correlation_id"` == id.to_string()
    #[test]
    fn dispatch_with_correlation_id_writes_correlation_id_to_event_log() {
        use crate::correlation::CorrelationId;

        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("probe.txt"), "corr-probe").unwrap();

        let log = open_event_log(tmp.path());
        let cid = CorrelationId::new();
        let cid_str = cid.to_string();

        let agent_id = 42u64;
        let ctx = DispatchContext {
            self_ref: AgentRef::new(agent_id, AgentRole::Conversational, "corr-agent", SpawnSource::User),
            role: AgentRole::Conversational,
            working_dir: tmp.path(),
            registry: Arc::new(Mutex::new(AgentRegistry::new())),
            event_log: Some(log.clone()),
            pending_spawn: new_spawn_subagent_queue(),
            source_event_id: None,
            quarantine: None,
            correlation_id: Some(cid),
            ticket_dispatcher: None,
        };

        // Act — dispatch a normal read_file tool.
        let res = dispatch_tool("read_file", &json!({"path": "probe.txt"}), &ctx);
        assert!(res.success, "dispatch must succeed: {}", res.output);

        // Assert — the event log tail must contain a `tool.invoked` envelope
        // whose payload carries the matching correlation_id string.
        let tail = log.lock().unwrap().tail(usize::MAX);

        let corr_event = tail
            .iter()
            .find(|ev| ev.kind == "tool.invoked")
            .expect("a tool.invoked event must be present in the event log");

        let stored_cid = corr_event
            .payload
            .get("correlation_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        assert_eq!(
            stored_cid, cid_str,
            "event log payload must contain the originating correlation_id; \
             got {stored_cid:?}, expected {cid_str:?}",
        );

        // The agent_id must also be stamped so the event is attributable.
        let logged_agent_id = corr_event
            .payload
            .get("agent_id")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        assert_eq!(
            logged_agent_id, agent_id,
            "tool.invoked payload must carry agent_id={agent_id}, got {logged_agent_id}",
        );

        // No correlation_id → no tool.invoked event emitted.  Verify a
        // context without a correlation_id does not emit a tool.invoked entry.
        let log2 = open_event_log(&tmp.path().join("events2.jsonl"));
        fs::write(tmp.path().join("probe2.txt"), "no-corr").unwrap();
        let ctx_no_corr = DispatchContext {
            self_ref: AgentRef::new(1, AgentRole::Conversational, "no-corr-agent", SpawnSource::User),
            role: AgentRole::Conversational,
            working_dir: tmp.path(),
            registry: Arc::new(Mutex::new(AgentRegistry::new())),
            event_log: Some(log2.clone()),
            pending_spawn: new_spawn_subagent_queue(),
            source_event_id: None,
            quarantine: None,
            correlation_id: None,
            ticket_dispatcher: None,
        };
        let res2 = dispatch_tool("read_file", &json!({"path": "probe2.txt"}), &ctx_no_corr);
        assert!(res2.success, "no-corr dispatch must succeed: {}", res2.output);

        let tail2 = log2.lock().unwrap().tail(usize::MAX);
        assert!(
            tail2.iter().all(|ev| ev.kind != "tool.invoked"),
            "without a correlation_id, no tool.invoked event must be emitted; got: {tail2:?}",
        );
    }

    // ---- Disposition -------------------------------------------------------

    #[test]
    fn disposition_default_is_chat() {
        assert_eq!(Disposition::default(), Disposition::Chat);
    }

    #[test]
    fn chat_auto_approve_no_branch() {
        assert!(Disposition::Chat.auto_approve());
        assert!(!Disposition::Chat.creates_branch());
        assert!(!Disposition::Chat.requires_plan_gate());
    }

    #[test]
    fn feature_full_lifecycle() {
        assert!(!Disposition::Feature.auto_approve());
        assert!(Disposition::Feature.creates_branch());
        assert!(Disposition::Feature.requires_plan_gate());
        assert!(Disposition::Feature.runs_hooks());
        assert_eq!(Disposition::Feature.skill(), "feature");
    }

    #[test]
    fn bugfix_full_lifecycle() {
        assert!(Disposition::BugFix.creates_branch());
        assert!(Disposition::BugFix.requires_plan_gate());
        assert_eq!(Disposition::BugFix.skill(), "bugfix");
    }

    #[test]
    fn refactor_full_lifecycle() {
        assert!(Disposition::Refactor.creates_branch());
        assert!(Disposition::Refactor.requires_plan_gate());
        assert_eq!(Disposition::Refactor.skill(), "refactor");
    }

    #[test]
    fn chore_branch_no_gate() {
        assert!(Disposition::Chore.creates_branch());
        assert!(!Disposition::Chore.requires_plan_gate());
        assert_eq!(Disposition::Chore.skill(), "chore");
    }

    #[test]
    fn synthesize_auto_approve() {
        assert!(Disposition::Synthesize.auto_approve());
        assert!(!Disposition::Synthesize.creates_branch());
        assert_eq!(Disposition::Synthesize.skill(), "synthesize");
    }

    #[test]
    fn decompose_auto_approve() {
        assert!(Disposition::Decompose.auto_approve());
        assert_eq!(Disposition::Decompose.skill(), "decompose");
    }

    #[test]
    fn audit_auto_approve_no_skill() {
        assert!(Disposition::Audit.auto_approve());
        assert!(!Disposition::Audit.creates_branch());
        assert_eq!(Disposition::Audit.skill(), "");
    }

    #[test]
    fn disposition_serde_roundtrip() {
        for d in [Disposition::Chat, Disposition::Feature, Disposition::BugFix,
                  Disposition::Refactor, Disposition::Chore, Disposition::Synthesize,
                  Disposition::Decompose, Disposition::Audit] {
            let s = serde_json::to_string(&d).unwrap();
            let back: Disposition = serde_json::from_str(&s).unwrap();
            assert_eq!(d, back);
        }
    }

    #[test]
    fn runs_hooks_iff_creates_branch() {
        for d in [Disposition::Chat, Disposition::Feature, Disposition::BugFix,
                  Disposition::Refactor, Disposition::Chore, Disposition::Synthesize,
                  Disposition::Decompose, Disposition::Audit] {
            assert_eq!(d.runs_hooks(), d.creates_branch());
        }
    }
}
