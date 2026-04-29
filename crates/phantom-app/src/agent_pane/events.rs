//! Substrate event emission helpers for agent panes.
//!
//! Covers payload construction and the two producer-side methods:
//! `maybe_emit_blocked_event` and `maybe_emit_capability_denied_event`.

use log::warn;

use phantom_agents::audit::{AuditOutcome, emit_tool_call};
use phantom_agents::role::CapabilityClass;
use phantom_agents::spawn_rules::{EventKind, EventSource, SubstrateEvent};
use phantom_agents::tools::{ToolResult, ToolType};

use super::{AgentPane, BlockedEventSink, DeniedEventSink, TOOL_BLOCK_THRESHOLD};
use super::dispatch_ctx::class_label;
use super::dispatch_ctx::now_unix_ms;

/// Construct a fresh, empty `BlockedEventSink`.
#[allow(dead_code)] // Producer for Phase 2.G consumer wiring; kept ahead of time.
pub(crate) fn new_blocked_event_sink() -> BlockedEventSink {
    std::sync::Arc::new(std::sync::Mutex::new(Vec::new()))
}

/// Construct a fresh, empty `DeniedEventSink`.
#[allow(dead_code)] // Producer for Sec.4 consumer wiring; kept ahead of time.
pub(crate) fn new_denied_event_sink() -> DeniedEventSink {
    std::sync::Arc::new(std::sync::Mutex::new(Vec::new()))
}

/// Build the canonical `AgentBlocked` payload documented in `fixer.rs`.
///
/// All keys here are convention; only `reason` is required at the rule layer.
/// The Fixer reads this payload to populate its system prompt at spawn time.
pub(super) fn build_blocked_payload(
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
pub(super) fn build_capability_denied_payload(
    agent_id: u64,
    agent_role: &str,
    attempted_class: CapabilityClass,
    attempted_tool: &str,
    denied_at_unix_ms: u64,
    source_chain: &[u64],
) -> serde_json::Value {
    serde_json::json!({
        "agent_id": agent_id,
        "agent_role": agent_role,
        "attempted_class": class_label(attempted_class),
        "attempted_tool": attempted_tool,
        "denied_at_unix_ms": denied_at_unix_ms,
        "source_chain": source_chain,
    })
}

impl AgentPane {
    /// Emit an `EventKind::AgentBlocked` substrate event when the agent's
    /// consecutive tool-call failure streak crosses [`TOOL_BLOCK_THRESHOLD`].
    ///
    /// Phase 2.E producer side. Resets the counter after emission so the
    /// same agent doesn't spam the bus on every subsequent failure; the
    /// `SpawnIfNotRunning` rule provides idempotency at the consumer side,
    /// but resetting here keeps the producer honest. No-op when
    /// [`AgentPane::blocked_event_sink`] is `None` (test/legacy callers
    /// without an App-owned sink).
    pub(super) fn maybe_emit_blocked_event(&mut self) {
        if self.consecutive_tool_failures < TOOL_BLOCK_THRESHOLD {
            return;
        }

        let Some(sink) = self.blocked_event_sink.clone() else {
            // No sink wired (test path without an App). Reset the streak so
            // a fresh failure starts a fresh count.
            self.consecutive_tool_failures = 0;
            self.last_tool_error = None;
            self.last_failing_capability = None;
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

        // Use the actual role and the capability class of the last failing
        // tool so the Fixer knows which permission to request.
        let role_label = self.role.label();
        let suggested_cap = self
            .last_failing_capability
            .map(class_label)
            .unwrap_or("Sense");

        let payload = build_blocked_payload(
            self.agent.id() as u64,
            role_label,
            &reason,
            now_unix_ms(),
            &context_excerpt,
            suggested_cap,
        );

        let role = self.role;
        let event = SubstrateEvent {
            kind: EventKind::AgentBlocked {
                agent_id: self.agent.id(),
                reason: reason.clone(),
            },
            payload,
            source: EventSource::Agent { role },
        };

        if let Ok(mut q) = sink.lock() {
            q.push(event);
        } else {
            warn!("blocked_event_sink mutex poisoned; dropping AgentBlocked event");
        }

        self.consecutive_tool_failures = 0;
        self.last_tool_error = None;
        self.last_failing_capability = None;
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
    /// `source_chain` is populated when `source_event_id` is `Some` and the
    /// event log is wired; otherwise it is empty (Sec.2 wiring).
    /// No-op when the sink isn't wired (test / legacy paths).
    pub(super) fn maybe_emit_capability_denied_event(
        &self,
        tool: ToolType,
        args: &serde_json::Value,
        result: &ToolResult,
        source_event_id: Option<u64>,
    ) {
        // The dispatch gate's denial message is a contract: we match on the
        // exact prefix the model sees. This keeps us in sync with the
        // canonical phrasing without coupling to the DispatchError type.
        if result.success || !result.output.starts_with("capability denied:") {
            return;
        }

        let attempted_class = tool.capability_class();
        let attempted_tool = tool.api_name().to_string();
        let agent_id = self.agent.id();
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

        // Sec.2: walk the event-log chain to collect provenance IDs.
        let source_chain: Vec<u64> =
            if let (Some(start_id), Some(log)) = (source_event_id, self.event_log.as_ref()) {
                phantom_agents::dispatch::collect_source_chain(start_id, log)
            } else {
                Vec::new()
            };

        let payload = build_capability_denied_payload(
            agent_id,
            role.label(),
            attempted_class,
            &attempted_tool,
            now_unix_ms(),
            &source_chain,
        );

        let event = SubstrateEvent {
            kind: EventKind::CapabilityDenied {
                agent_id,
                role,
                attempted_class,
                attempted_tool,
                source_chain,
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
}
