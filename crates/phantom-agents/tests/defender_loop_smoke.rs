//! Smoke test: Sec.4–5 Defender → challenge → response loop.
//!
//! Verifies the full causal chain:
//!
//! 1. A Watcher (offending agent) attempts an `Act`-class tool (`run_command`).
//! 2. The Layer-2 dispatch gate emits `CapabilityDenied` (returns denied result).
//! 3. The `defender_spawn_rule` fires on the corresponding `SubstrateEvent`.
//! 4. The substrate manually spawns a Defender (simulated; no LLM needed).
//! 5. The Defender calls `challenge_agent` via `dispatch_tool`.
//! 6. The offending Watcher's inbox receives the challenge, correctly tagged.
//! 7. Each step is observable in the event log.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use phantom_agents::defender::defender_spawn_rule;
use phantom_agents::dispatch::{DispatchContext, RuntimeMode, dispatch_tool};
use phantom_agents::inbox::{AgentHandle, AgentRegistry, AgentStatus, InboxMessage};
use phantom_agents::role::{AgentId, AgentRef, AgentRole, CapabilityClass, SpawnSource};
use phantom_agents::spawn_rules::{
    EventKind, EventSource as SubstrateEventSource, SpawnAction, SpawnRuleRegistry, SubstrateEvent,
};
use phantom_memory::event_log::{EventLog, EventSource as LogEventSource};

use serde_json::json;
use tempfile::tempdir;
use tokio::sync::{mpsc, watch};
use tokio::time::timeout;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a fake agent handle + receiver.
fn make_agent(
    id: AgentId,
    role: AgentRole,
    label: &str,
) -> (AgentHandle, mpsc::Receiver<InboxMessage>) {
    let (tx, rx) = mpsc::channel(16);
    let (_status_tx, status_rx) = watch::channel(AgentStatus::Idle);
    let handle = AgentHandle {
        agent_ref: AgentRef::new(id, role, label, SpawnSource::Substrate),
        inbox: tx,
        status: status_rx,
    };
    (handle, rx)
}

/// Build a fresh spawn-queue (same shape used by the production dispatch path).
fn new_spawn_queue() -> Arc<Mutex<std::collections::VecDeque<phantom_agents::composer_tools::SpawnSubagentRequest>>> {
    phantom_agents::composer_tools::new_spawn_subagent_queue()
}

// ---------------------------------------------------------------------------
// The smoke test
// ---------------------------------------------------------------------------

/// Full Sec.4–5 loop: denial → rule fires → Defender spawned → challenge delivered.
#[tokio::test]
async fn defender_challenge_loop_smoke() {
    // -----------------------------------------------------------------------
    // Arrange — shared substrate primitives
    // -----------------------------------------------------------------------
    let dir = tempdir().expect("tempdir");
    let log_path = dir.path().join("events.jsonl");

    // Open the shared event log.
    let event_log: Arc<Mutex<EventLog>> =
        Arc::new(Mutex::new(EventLog::open(&log_path).expect("open event log")));

    // Shared registry — will hold both the offender and the Defender.
    let registry: Arc<Mutex<AgentRegistry>> = Arc::new(Mutex::new(AgentRegistry::new()));

    // Register the offending Watcher (id=10).
    const OFFENDER_ID: AgentId = 10;
    let (offender_handle, mut offender_rx) = make_agent(OFFENDER_ID, AgentRole::Watcher, "offender-watcher");
    registry.lock().unwrap().register(offender_handle);

    // -----------------------------------------------------------------------
    // Step 1 — Offender attempts an Act-class tool (run_command).
    //           The dispatch gate must deny it and return a failed ToolResult.
    // -----------------------------------------------------------------------
    let offender_ref = AgentRef::new(OFFENDER_ID, AgentRole::Watcher, "offender-watcher", SpawnSource::Substrate);
    let offender_ctx = DispatchContext {
        self_ref: offender_ref,
        role: AgentRole::Watcher,
        working_dir: dir.path(),
        registry: registry.clone(),
        event_log: Some(event_log.clone()),
        pending_spawn: new_spawn_queue(),
        source_event_id: None,
        quarantine: None,
        correlation_id: None,
        ticket_dispatcher: None,
        runtime_mode: RuntimeMode::Normal,
    };

    let denied_result = dispatch_tool(
        "run_command",
        &json!({ "command": "echo SHOULD_NEVER_RUN" }),
        &offender_ctx,
    );

    // Step 1 assertion: dispatch gate denied the call.
    assert!(
        !denied_result.success,
        "Step 1 FAIL — Watcher run_command must be denied, got: {}",
        denied_result.output,
    );
    assert!(
        denied_result.output.starts_with("capability denied:"),
        "Step 1 FAIL — expected canonical denial phrasing, got: {}",
        denied_result.output,
    );
    assert!(
        denied_result.output.contains("Act"),
        "Step 1 FAIL — denial must name the missing class, got: {}",
        denied_result.output,
    );

    // -----------------------------------------------------------------------
    // Step 2 — Layer-2 emits CapabilityDenied to the substrate event log.
    //           (In production, dispatch_tool does this; here we emit it
    //           explicitly to exercise the exact event shape the spawn-rule
    //           registry consumes.)
    // -----------------------------------------------------------------------
    let denial_event_id: u64 = {
        let mut log = event_log.lock().unwrap();
        log.append(
            LogEventSource::Substrate,
            "capability.denied",
            json!({
                "agent_id": OFFENDER_ID,
                "role": "Watcher",
                "attempted_class": "Act",
                "attempted_tool": "run_command",
            }),
        )
        .expect("append capability.denied event")
        .id
    };

    // Step 2 assertion: event is in the log.
    {
        let log = event_log.lock().unwrap();
        let tail = log.tail(64);
        let found = tail.iter().any(|e| e.kind == "capability.denied" && e.id == denial_event_id);
        assert!(
            found,
            "Step 2 FAIL — capability.denied event (id={denial_event_id}) not found in log tail",
        );
    }

    // -----------------------------------------------------------------------
    // Step 3 — defender_spawn_rule fires on the CapabilityDenied substrate event.
    // -----------------------------------------------------------------------
    let rule_registry = SpawnRuleRegistry::new().add(defender_spawn_rule());

    let substrate_event = SubstrateEvent {
        kind: EventKind::CapabilityDenied {
            agent_id: OFFENDER_ID,
            role: AgentRole::Watcher,
            attempted_class: CapabilityClass::Act,
            attempted_tool: "run_command".to_string(),
            source_chain: vec![denial_event_id],
        },
        payload: serde_json::Value::Null,
        source: SubstrateEventSource::Substrate,
    };

    let actions = rule_registry.evaluate(&substrate_event);

    // Step 3 assertion: exactly one SpawnIfNotRunning(Defender) action fires.
    assert_eq!(
        actions.len(),
        1,
        "Step 3 FAIL — expected 1 spawn action from defender_spawn_rule, got {}",
        actions.len(),
    );
    let defender_role = match actions[0] {
        SpawnAction::SpawnIfNotRunning { role, label_template, .. } => {
            assert_eq!(
                *role, AgentRole::Defender,
                "Step 3 FAIL — spawn action must target Defender, got {role:?}",
            );
            assert_eq!(
                label_template, "defender-on-denial",
                "Step 3 FAIL — label template mismatch: {label_template}",
            );
            *role
        }
        SpawnAction::Spawn { .. } => {
            panic!("Step 3 FAIL — expected SpawnIfNotRunning, got Spawn");
        }
    };

    // -----------------------------------------------------------------------
    // Step 4 — Substrate spawns the Defender (simulated; no LLM).
    //           Register it in the shared registry so dispatch can locate it.
    // -----------------------------------------------------------------------
    const DEFENDER_ID: AgentId = 99;
    let (defender_handle, _defender_rx) =
        make_agent(DEFENDER_ID, defender_role, "defender-on-denial");
    registry.lock().unwrap().register(defender_handle);

    // Step 4 assertion: Defender is visible in the registry.
    {
        let reg = registry.lock().unwrap();
        let d = reg.get(DEFENDER_ID).expect("Step 4 FAIL — Defender not registered");
        assert_eq!(d.agent_ref.role, AgentRole::Defender);
    }

    // -----------------------------------------------------------------------
    // Step 5 — Defender calls challenge_agent via dispatch_tool.
    //           The Defender holds Coordinate, so the gate passes.
    // -----------------------------------------------------------------------
    let defender_ref = AgentRef::new(
        DEFENDER_ID,
        AgentRole::Defender,
        "defender-on-denial",
        SpawnSource::Substrate,
    );
    let defender_ctx = DispatchContext {
        self_ref: defender_ref,
        role: AgentRole::Defender,
        working_dir: dir.path(),
        registry: registry.clone(),
        event_log: Some(event_log.clone()),
        pending_spawn: new_spawn_queue(),
        source_event_id: None,
        quarantine: None,
        correlation_id: None,
        ticket_dispatcher: None,
        runtime_mode: RuntimeMode::Normal,
    };

    let challenge_result = dispatch_tool(
        "challenge_agent",
        &json!({
            "target_agent_id": OFFENDER_ID as u32,
            "denial_event_id": denial_event_id,
            "question": "why did you attempt run_command? your Watcher role does not have Act capability.",
        }),
        &defender_ctx,
    );

    // Step 5 assertion: challenge_agent succeeded through dispatch.
    assert!(
        challenge_result.success,
        "Step 5 FAIL — Defender challenge_agent call must succeed, got: {}",
        challenge_result.output,
    );
    assert!(
        challenge_result.output.contains(&OFFENDER_ID.to_string()),
        "Step 5 FAIL — success message must echo target id, got: {}",
        challenge_result.output,
    );

    // -----------------------------------------------------------------------
    // Step 6 — Offender's inbox receives the challenge, correctly tagged.
    // -----------------------------------------------------------------------
    let inbox_msg = timeout(Duration::from_millis(500), offender_rx.recv())
        .await
        .expect("Step 6 FAIL — timed out waiting for challenge in offender inbox")
        .expect("Step 6 FAIL — offender inbox channel closed");

    match inbox_msg {
        InboxMessage::AgentSpeak { from, body } => {
            assert_eq!(
                from.id, DEFENDER_ID,
                "Step 6 FAIL — challenge must be tagged from Defender (id={DEFENDER_ID}), got id={}",
                from.id,
            );
            assert_eq!(
                from.role,
                AgentRole::Defender,
                "Step 6 FAIL — from.role must be Defender, got {:?}",
                from.role,
            );
            assert!(
                body.starts_with("[defender challenge re: denial #"),
                "Step 6 FAIL — body must carry canonical Defender framing, got: {body}",
            );
            assert!(
                body.contains(&denial_event_id.to_string()),
                "Step 6 FAIL — body must embed denial_event_id={denial_event_id}, got: {body}",
            );
            assert!(
                body.contains("why did you attempt run_command"),
                "Step 6 FAIL — body must carry the original question, got: {body}",
            );
        }
        other => panic!("Step 6 FAIL — wrong inbox message type: {other:?}"),
    }

    // -----------------------------------------------------------------------
    // Step 7 — agent.challenge envelope is in the event log.
    // -----------------------------------------------------------------------
    let challenge_envelope = {
        let log = event_log.lock().unwrap();
        let tail = log.tail(64);
        tail.into_iter()
            .find(|e| e.kind == "agent.challenge")
            .expect("Step 7 FAIL — agent.challenge envelope not found in event log")
    };

    assert_eq!(
        challenge_envelope.payload["from"]["id"].as_u64(),
        Some(DEFENDER_ID),
        "Step 7 FAIL — challenge envelope from.id must be Defender id={DEFENDER_ID}",
    );
    assert_eq!(
        challenge_envelope.payload["to"]["id"].as_u64(),
        Some(OFFENDER_ID),
        "Step 7 FAIL — challenge envelope to.id must be offender id={OFFENDER_ID}",
    );
    assert_eq!(
        challenge_envelope.payload["denial_event_id"].as_u64(),
        Some(denial_event_id),
        "Step 7 FAIL — challenge envelope must carry the original denial_event_id",
    );
    assert!(
        challenge_envelope.payload["question"]
            .as_str()
            .unwrap_or("")
            .contains("why did you attempt run_command"),
        "Step 7 FAIL — challenge envelope must carry the original question",
    );
}

// ---------------------------------------------------------------------------
// Negative: offender cannot challenge_agent back (no Coordinate in Watcher)
// ---------------------------------------------------------------------------

/// Security property: a Watcher (the offending role) cannot call
/// `challenge_agent` itself. Without this gate, a compromised offender could
/// spam-challenge the Defender or any other agent.
#[test]
fn offender_cannot_call_challenge_agent() {
    let dir = tempdir().expect("tempdir");

    let (peer_handle, _peer_rx) = make_agent(99, AgentRole::Defender, "defender");
    let mut reg = AgentRegistry::new();
    reg.register(peer_handle);
    let registry = Arc::new(Mutex::new(reg));

    let watcher_ref = AgentRef::new(10, AgentRole::Watcher, "offender-watcher", SpawnSource::Substrate);
    let ctx = DispatchContext {
        self_ref: watcher_ref,
        role: AgentRole::Watcher,
        working_dir: dir.path(),
        registry,
        event_log: None,
        pending_spawn: new_spawn_queue(),
        source_event_id: None,
        quarantine: None,
        correlation_id: None,
        ticket_dispatcher: None,
        runtime_mode: RuntimeMode::Normal,
    };

    let result = dispatch_tool(
        "challenge_agent",
        &json!({
            "target_agent_id": 99u32,
            "denial_event_id": 1u64,
            "question": "try to challenge back",
        }),
        &ctx,
    );

    assert!(
        !result.success,
        "Watcher must not be allowed to call challenge_agent (no Coordinate), got: {}",
        result.output,
    );
    assert!(
        result.output.starts_with("capability denied:"),
        "expected canonical denial phrasing, got: {}",
        result.output,
    );
    assert!(
        result.output.contains("Coordinate"),
        "denial must name the missing class, got: {}",
        result.output,
    );
}
