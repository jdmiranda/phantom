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
use crate::defender_tools::{DefenderTool, DefenderToolContext, challenge_agent};
use crate::inbox::AgentRegistry;
use crate::role::{AgentRef, AgentRole, CapabilityClass};
use crate::tools::{ToolResult, ToolType, execute_tool};

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
#[must_use]
pub fn dispatch_tool(
    name: &str,
    args: &serde_json::Value,
    ctx: &DispatchContext<'_>,
) -> ToolResult {
    // ---- File / git tools (existing surface) -------------------------------
    if let Some(tool) = ToolType::from_api_name(name) {
        if let Err(msg) = check_capability(ctx.role, class_for(tool)) {
            return result(tool, false, msg);
        }
        let working_dir = ctx.working_dir.to_string_lossy();
        return execute_tool(tool, args, &working_dir, &ctx.role);
    }

    // ---- Chat tools --------------------------------------------------------
    if let Some(chat_tool) = ChatTool::from_api_name(name) {
        if let Err(msg) = check_capability(ctx.role, chat_tool.class()) {
            return result(PLACEHOLDER_TOOL, false, msg);
        }
        let chat_ctx = ChatToolContext {
            self_ref: ctx.self_ref.clone(),
            registry: ctx.registry.clone(),
            event_log: ctx.event_log.clone(),
        };
        return match chat_tool {
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
        };
    }

    // ---- Composer tools ----------------------------------------------------
    if let Some(composer_tool) = ComposerTool::from_api_name(name) {
        if let Err(msg) = check_capability(ctx.role, composer_tool.class()) {
            return result(PLACEHOLDER_TOOL, false, msg);
        }
        return match composer_tool {
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
            ComposerTool::WaitForAgent => {
                let Some(log) = ctx.event_log.as_ref() else {
                    return result(
                        PLACEHOLDER_TOOL,
                        false,
                        "event log not configured".into(),
                    );
                };
                match wait_for_agent(args, log) {
                    Ok(env) => {
                        let body = serde_json::to_string(&env)
                            .unwrap_or_else(|e| format!("encode error: {e}"));
                        result(PLACEHOLDER_TOOL, true, body)
                    }
                    Err(e) => result(PLACEHOLDER_TOOL, false, e),
                }
            }
            ComposerTool::RequestCritique => {
                let Some(log) = ctx.event_log.as_ref() else {
                    return result(
                        PLACEHOLDER_TOOL,
                        false,
                        "event log not configured".into(),
                    );
                };
                let registry_guard = match ctx.registry.lock() {
                    Ok(g) => g,
                    Err(_) => {
                        return result(
                            PLACEHOLDER_TOOL,
                            false,
                            "agent registry poisoned".into(),
                        );
                    }
                };
                match request_critique(args, &ctx.self_ref, &registry_guard, log) {
                    Ok(env) => {
                        let body = serde_json::to_string(&env)
                            .unwrap_or_else(|e| format!("encode error: {e}"));
                        result(PLACEHOLDER_TOOL, true, body)
                    }
                    Err(e) => result(PLACEHOLDER_TOOL, false, e),
                }
            }
            ComposerTool::EventLogQuery => {
                let Some(log) = ctx.event_log.as_ref() else {
                    return result(
                        PLACEHOLDER_TOOL,
                        false,
                        "event log not configured".into(),
                    );
                };
                match event_log_query(args, log) {
                    Ok(envs) => {
                        let body = serde_json::to_string(&envs)
                            .unwrap_or_else(|e| format!("encode error: {e}"));
                        result(PLACEHOLDER_TOOL, true, body)
                    }
                    Err(e) => result(PLACEHOLDER_TOOL, false, e),
                }
            }
        };
    }

    // ---- Defender tools ----------------------------------------------------
    if let Some(defender_tool) = DefenderTool::from_api_name(name) {
        if let Err(msg) = check_capability(ctx.role, defender_tool.class()) {
            return result(PLACEHOLDER_TOOL, false, msg);
        }
        let defender_ctx = DefenderToolContext {
            self_ref: ctx.self_ref.clone(),
            registry: ctx.registry.clone(),
            event_log: ctx.event_log.clone(),
        };
        return match defender_tool {
            DefenderTool::ChallengeAgent => match challenge_agent(args, &defender_ctx) {
                Ok(msg) => result(PLACEHOLDER_TOOL, true, msg),
                Err(e) => result(PLACEHOLDER_TOOL, false, e),
            },
        };
    }

    // ---- Unknown ----------------------------------------------------------
    result(PLACEHOLDER_TOOL, false, format!("unknown tool: {name}"))
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
}
