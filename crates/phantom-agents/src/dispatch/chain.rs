//! Source-chain walking and taint elevation (Sec.7.2) for the dispatch layer.

use std::sync::{Arc, Mutex};

use phantom_memory::event_log::EventLog;

use crate::inbox::{AgentRegistry, AgentStatus};
use crate::quarantine::QuarantineRegistry;
use crate::role::AgentId;
use crate::taint::TaintLevel;

/// The event-log `kind` string emitted when a capability denial fires.
pub(super) const KIND_CAPABILITY_DENIED: &str = "capability.denied";

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
pub(super) fn taint_from_source_chain(
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

/// Walk the `source_event_id` chain in `log` starting from `start_id` and
/// return the ordered list of event IDs encountered, including `start_id`
/// itself.
///
/// The list is ordered from `start_id` to the earliest ancestor reachable
/// through the chain. The walk terminates when:
/// - an event ID is not found in the in-memory tail (scrolled out), or
/// - a cycle is detected (the same ID would be visited twice).
///
/// This is the Sec.2 counterpart to [`taint_from_source_chain`]: instead of
/// accumulating taint it collects the provenance chain that is stored in
/// `EventKind::CapabilityDenied::source_chain`.
///
/// # Locking
///
/// Takes `log` once to snapshot the tail; no lock is held during the walk.
pub fn collect_source_chain(start_id: u64, log: &Arc<Mutex<EventLog>>) -> Vec<u64> {
    // Snapshot the in-memory tail once — avoids repeated locking in the walk.
    let tail = {
        let Ok(guard) = log.lock() else {
            return vec![start_id];
        };
        guard.tail(usize::MAX)
    };

    // Build a lookup table: event_id → envelope for O(1) chain hops.
    let by_id: std::collections::HashMap<u64, &phantom_memory::event_log::EventEnvelope> =
        tail.iter().map(|e| (e.id, e)).collect();

    let mut chain: Vec<u64> = Vec::new();
    let mut visited: std::collections::HashSet<u64> = std::collections::HashSet::new();
    let mut cursor: Option<u64> = Some(start_id);

    while let Some(id) = cursor {
        // Cycle guard: if we've already visited this ID, stop walking.
        if !visited.insert(id) {
            break;
        }
        chain.push(id);
        let Some(ev) = by_id.get(&id) else {
            // Event has scrolled out of the tail — stop walking.
            break;
        };
        // Follow the chain link stored in the event payload.
        cursor = ev
            .payload
            .get("source_event_id")
            .and_then(|v| v.as_u64());
    }

    chain
}

/// Extract the originating agent id from an [`EventEnvelope`], if present.
///
/// Events emitted by agents carry `source: Agent { id }`. We also check a
/// `"source_agent_id"` payload field as a fallback for hand-rolled events
/// that embed the agent id in the payload rather than via the `source` field.
pub(super) fn source_agent_id_from_envelope(
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
pub(super) fn agent_is_quarantined(id: AgentId, registry: &Arc<Mutex<AgentRegistry>>) -> bool {
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
pub(super) fn quarantine_registry_blocks(
    id: AgentId,
    quarantine: &Arc<Mutex<QuarantineRegistry>>,
) -> bool {
    let Ok(guard) = quarantine.lock() else {
        return false;
    };
    guard.agent_is_quarantined(id)
}
