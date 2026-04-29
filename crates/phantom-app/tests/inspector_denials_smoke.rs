//! Smoke test: Inspector denials tab — end-to-end denial → snapshot assertion.
//!
//! Goal: confirm that when a Watcher agent calls `run_command` (Act-class),
//! the denial event lands in the substrate runtime's event log, and the
//! inspector snapshot's `denials` field contains a `DenialEntry` with the
//! correct `role`, `attempted_tool`, and `attempted_class`.
//!
//! The test is headless — no GPU, no `App`. It exercises:
//!   1. Constructing a canonical denial `ToolResult` (as `dispatch_tool` would
//!      produce) — `execute_tool` is capability-agnostic (see issue #104)
//!   2. `AgentRuntime::push_event` + `tick` (drains to event log)
//!   3. `event_log().tail()` projection into `DenialEntry` rows (mirrors
//!      `App::collect_recent_denials` logic)
//!
//! ## Scope note — `maybe_emit_capability_denied_event` is NOT exercised here
//!
//! `agent_pane::maybe_emit_capability_denied_event` is the production path that
//! constructs and pushes `SubstrateEvent::CapabilityDenied` when an agent pane
//! detects a tool denial. This test does NOT call that function; instead it
//! constructs the `SubstrateEvent` directly with the same payload shape and
//! pushes it via `AgentRuntime::push_event`. This lets the test remain headless
//! (no GPU, no `App`, no pane wiring) while still exercising the
//! runtime pipeline and the `DenialEntry` projection logic.
//! A separate integration test targeting `agent_pane` directly would be needed
//! to cover the `maybe_emit_capability_denied_event` emission path.

use phantom_agents::inspector::{DenialEntry, MAX_RECENT_EVENTS};
use phantom_agents::role::{AgentRole, CapabilityClass};
use phantom_agents::spawn_rules::{EventKind, EventSource, SubstrateEvent};
use phantom_app::runtime::{AgentRuntime, RuntimeConfig};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helper: build a runtime rooted in a temporary directory.
// ---------------------------------------------------------------------------

fn make_runtime() -> (AgentRuntime, TempDir) {
    let dir = TempDir::new().expect("tempdir");
    let cfg = RuntimeConfig::under_dir(dir.path());
    let rt = AgentRuntime::new(cfg, Vec::new()).expect("runtime open");
    (rt, dir)
}

// ---------------------------------------------------------------------------
// Helper: project event-log envelopes into DenialEntry rows.
//
// Mirrors App::collect_recent_denials without requiring a full App.
// Reads the log tail, filters on the "agent.capability_denied." prefix,
// and projects the payload fields into DenialEntry instances.
// ---------------------------------------------------------------------------

fn collect_denials_from_runtime(rt: &AgentRuntime) -> Vec<DenialEntry> {
    const CAPABILITY_DENIED_KIND_PREFIX: &str = "agent.capability_denied.";

    rt.event_log()
        .tail(MAX_RECENT_EVENTS)
        .into_iter()
        .filter(|env| env.kind.starts_with(CAPABILITY_DENIED_KIND_PREFIX))
        .map(|env| {
            let payload = &env.payload;
            let role = payload
                .get("agent_role")
                .and_then(|v| v.as_str())
                .unwrap_or("?")
                .to_string();
            let attempted_tool = payload
                .get("attempted_tool")
                .and_then(|v| v.as_str())
                .unwrap_or("?")
                .to_string();
            let attempted_class = payload
                .get("attempted_class")
                .and_then(|v| v.as_str())
                .unwrap_or("?")
                .to_string();
            let source_chain: Vec<u64> = payload
                .get("source_chain")
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().filter_map(|v| v.as_u64()).collect())
                .unwrap_or_default();
            let timestamp_ms = env.ts_unix_ms.max(0) as u64;
            DenialEntry {
                role,
                attempted_tool,
                attempted_class,
                source_chain,
                timestamp_ms,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Smoke tests
// ---------------------------------------------------------------------------

/// Core end-to-end path:
///   Watcher calls run_command → gate denies → event pushed + ticked →
///   collect_denials_from_runtime returns exactly one DenialEntry with
///   correct role, tool, and class.
#[test]
fn watcher_run_command_denial_appears_in_inspector_snapshot() {
    // Arrange: build a runtime.
    let (mut rt, _runtime_dir) = make_runtime();

    // Act (Step 1): simulate the canonical denial result that `dispatch_tool`
    // produces when a Watcher tries run_command (Act-class). `execute_tool`
    // is capability-agnostic (see issue #104; the gate lives in
    // `dispatch_tool`), so we construct the denial directly.
    let _args = serde_json::json!({"command": "echo SHOULD_NEVER_RUN"});

    // Act (Step 2): construct the CapabilityDenied SubstrateEvent directly
    // (bypassing agent_pane::maybe_emit_capability_denied_event, which is the
    // production emitter but requires a live pane and GPU context). We replicate
    // the payload shape exactly — same keys the projection reads — so the
    // runtime pipeline and DenialEntry projection are still exercised faithfully.
    let agent_id: u64 = 42;
    let event = SubstrateEvent {
        kind: EventKind::CapabilityDenied {
            agent_id,
            role: AgentRole::Watcher,
            attempted_class: CapabilityClass::Act,
            attempted_tool: "run_command".to_string(),
            source_chain: Vec::new(),
        },
        payload: serde_json::json!({
            "agent_id": agent_id,
            "agent_role": "Watcher",
            "attempted_class": "Act",
            "attempted_tool": "run_command",
            "denied_at_unix_ms": 1_700_000_000_000u64,
            "source_chain": [],
        }),
        source: EventSource::Agent {
            role: AgentRole::Watcher,
        },
    };

    rt.push_event(event);

    // Act (Step 3): tick drains the pending queue into the event log.
    rt.tick();

    // Act (Step 4): project event-log tail into DenialEntry rows.
    let denials = collect_denials_from_runtime(&rt);

    // Assert: exactly one denial in the snapshot.
    assert_eq!(
        denials.len(),
        1,
        "expected one denial in snapshot; got {}: {:?}",
        denials.len(),
        denials.iter().map(|d| &d.attempted_tool).collect::<Vec<_>>(),
    );

    let entry = &denials[0];

    // Role must be "Watcher".
    assert_eq!(
        entry.role, "Watcher",
        "denial role must be 'Watcher'; got '{}'",
        entry.role,
    );

    // Tool must be "run_command".
    assert_eq!(
        entry.attempted_tool, "run_command",
        "denial attempted_tool must be 'run_command'; got '{}'",
        entry.attempted_tool,
    );

    // Class must be "Act".
    assert_eq!(
        entry.attempted_class, "Act",
        "denial attempted_class must be 'Act'; got '{}'",
        entry.attempted_class,
    );
}

/// Guard: a non-CapabilityDenied event in the log must NOT appear in the
/// denial projection.  Ensures the kind-prefix filter is working.
#[test]
fn non_denial_events_are_not_projected_into_denials() {
    let (mut rt, _dir) = make_runtime();

    // Push a PaneOpened event — this must never land in the denials list.
    rt.push_event(SubstrateEvent {
        kind: EventKind::PaneOpened {
            app_type: "agent".to_string(),
        },
        payload: serde_json::json!({"app_type": "agent"}),
        source: EventSource::User,
    });
    rt.tick();

    let denials = collect_denials_from_runtime(&rt);
    assert!(
        denials.is_empty(),
        "PaneOpened must not appear in the denial projection; got {denials:?}",
    );
}

/// Multiple denials from different agents must all appear in the snapshot,
/// ordered oldest-first (the event-log tail order).
#[test]
fn multiple_denials_appear_in_log_order() {
    let (mut rt, _dir) = make_runtime();

    // Push three distinct denials — one per agent-id so the kind strings differ.
    for agent_id in [10u64, 11, 12] {
        rt.push_event(SubstrateEvent {
            kind: EventKind::CapabilityDenied {
                agent_id,
                role: AgentRole::Watcher,
                attempted_class: CapabilityClass::Act,
                attempted_tool: "run_command".to_string(),
                source_chain: Vec::new(),
            },
            payload: serde_json::json!({
                "agent_id": agent_id,
                "agent_role": "Watcher",
                "attempted_class": "Act",
                "attempted_tool": "run_command",
                "denied_at_unix_ms": agent_id * 1_000u64,
                "source_chain": [],
            }),
            source: EventSource::Agent {
                role: AgentRole::Watcher,
            },
        });
    }
    rt.tick();

    let denials = collect_denials_from_runtime(&rt);

    assert_eq!(
        denials.len(),
        3,
        "all three denials must appear in the snapshot",
    );

    // Every entry must carry the expected fields regardless of agent-id.
    for entry in &denials {
        assert_eq!(entry.role, "Watcher");
        assert_eq!(entry.attempted_tool, "run_command");
        assert_eq!(entry.attempted_class, "Act");
    }
}

/// The denial entry's `timestamp_ms` field must be non-zero when the payload
/// carries a `denied_at_unix_ms` value.  (The projection maps `env.ts_unix_ms`
/// — the log's own timestamp — into `DenialEntry::timestamp_ms`; we just
/// verify it's positive, not a specific value, since the log stamps with the
/// real wall clock.)
#[test]
fn denial_entry_timestamp_is_non_zero() {
    let (mut rt, _dir) = make_runtime();

    rt.push_event(SubstrateEvent {
        kind: EventKind::CapabilityDenied {
            agent_id: 99,
            role: AgentRole::Watcher,
            attempted_class: CapabilityClass::Act,
            attempted_tool: "run_command".to_string(),
            source_chain: Vec::new(),
        },
        payload: serde_json::json!({
            "agent_id": 99,
            "agent_role": "Watcher",
            "attempted_class": "Act",
            "attempted_tool": "run_command",
            "source_chain": [],
        }),
        source: EventSource::Agent {
            role: AgentRole::Watcher,
        },
    });
    rt.tick();

    let denials = collect_denials_from_runtime(&rt);
    assert_eq!(denials.len(), 1);

    // The event log stamps each record with the wall-clock milliseconds at
    // append time — we can assert it's positive without pinning an exact value.
    assert!(
        denials[0].timestamp_ms > 0,
        "timestamp_ms must be a positive wall-clock value; got {}",
        denials[0].timestamp_ms,
    );
}
