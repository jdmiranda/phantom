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
//! Every fork is gated by [`capability::check_capability`] before the handler
//! runs. The class is taken from:
//! - [`capability::class_for`] for file/git tools (a local Sense/Act mapping),
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

mod capability;
mod chain;
mod context;
mod disposition;
mod runtime_mode;

#[cfg(test)]
mod tests;

// ---------------------------------------------------------------------------
// Re-exports — public API surface
// ---------------------------------------------------------------------------

pub use chain::collect_source_chain;
pub use context::DispatchContext;
pub use disposition::Disposition;
pub use runtime_mode::RuntimeMode;

// ---------------------------------------------------------------------------
// Internal imports for dispatch_tool
// ---------------------------------------------------------------------------

use std::sync::Arc;

use crate::chat_tools::{ChatTool, ChatToolContext, broadcast_to_role, read_from_agent, send_to_agent};
use crate::composer_tools::{
    ComposerTool, event_log_query, request_critique, spawn_subagent, wait_for_agent,
};
use crate::defender_tools::{DefenderTool, DefenderToolContext, challenge_agent};
use crate::dispatcher::{
    DispatcherTool, DispatcherToolContext, mark_ticket_done, mark_ticket_in_progress,
    request_next_ticket,
};
use crate::tools::{ToolResult, ToolType, execute_tool};

use capability::{check_capability, class_for};
use chain::{quarantine_registry_blocks, taint_from_source_chain};

// ---------------------------------------------------------------------------
// dispatch_tool helpers
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

// ---------------------------------------------------------------------------
// dispatch_tool
// ---------------------------------------------------------------------------

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

    // ---- Issue #105: SpawnOnly gate (layer-3) ------------------------------
    //
    // When runtime_mode is SpawnOnly, only spawn_subagent is permitted.
    // All other tools are denied before any capability check or handler runs.
    // The denial is recorded in the event log for audit completeness.
    if !ctx.runtime_mode.permits(name) {
        if let Some(log) = ctx.event_log.as_ref() {
            if let Ok(mut guard) = log.lock() {
                let _ = guard.append(
                    phantom_memory::event_log::EventSource::Agent { id: ctx.self_ref.id },
                    "runtime.denied",
                    serde_json::json!({
                        "agent_id": ctx.self_ref.id,
                        "tool": name,
                        "mode": ctx.runtime_mode.as_str(),
                    }),
                );
            }
        }
        return result(
            PLACEHOLDER_TOOL,
            false,
            format!(
                "runtime denied: {} — only spawn_subagent is permitted in {} mode (agent {})",
                name,
                ctx.runtime_mode.as_str(),
                ctx.self_ref.id,
            ),
        );
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

