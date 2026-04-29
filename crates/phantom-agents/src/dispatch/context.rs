//! [`DispatchContext`] — per-turn context for [`dispatch_tool`].
//!
//! [`dispatch_tool`]: super::dispatch_tool

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};

use phantom_memory::event_log::EventLog;

use crate::composer_tools::SpawnSubagentRequest;
use crate::correlation::CorrelationId;
use crate::dispatcher::GhTicketDispatcher;
use crate::inbox::AgentRegistry;
use crate::quarantine::QuarantineRegistry;
use crate::role::{AgentRef, AgentRole};

use super::runtime_mode::RuntimeMode;

/// Per-turn context for [`super::dispatch_tool`].
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
    ///
    /// [`CapabilityClass`]: crate::role::CapabilityClass
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
    /// When set, [`super::dispatch_tool`] walks the `source_event_id` chain
    /// backwards in the event log and elevates the result taint:
    /// - any upstream `"capability.denied"` event → at least `Suspect`,
    /// - any upstream agent with [`crate::inbox::AgentStatus::Failed`] (quarantined) →
    ///   `Tainted`.
    ///
    /// `None` disables the chain walk — the correct behaviour for legacy /
    /// test paths that have not wired an event log.
    pub source_event_id: Option<u64>,
    /// Sec.7.3: quarantine registry.
    ///
    /// When `Some`, [`super::dispatch_tool`] checks whether the calling agent
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
    /// Issue #105: runtime execution mode gate.
    ///
    /// Defaults to [`RuntimeMode::Normal`] (no extra restriction). Set to
    /// [`RuntimeMode::SpawnOnly`] in the orchestrator harness to block every
    /// tool other than `spawn_subagent` before the capability gate runs.
    ///
    /// Legacy and test paths that do not set this field explicitly should use
    /// `RuntimeMode::Normal`.
    pub runtime_mode: RuntimeMode,
}
