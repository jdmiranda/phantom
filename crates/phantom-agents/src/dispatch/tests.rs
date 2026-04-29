//! Tests for the dispatch module.

use super::*;
use crate::composer_tools::new_spawn_subagent_queue;
use crate::inbox::{AgentHandle, AgentRegistry, AgentStatus, InboxMessage};
use crate::role::{AgentRef, AgentRole, SpawnSource};
use crate::taint::TaintLevel;
use chain::KIND_CAPABILITY_DENIED;
use phantom_memory::event_log::{EventLog, EventSource as LogEventSource};
use serde_json::json;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};
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
        runtime_mode: RuntimeMode::Normal,
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
        runtime_mode: RuntimeMode::Normal,
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
        runtime_mode: RuntimeMode::Normal,
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
        runtime_mode: RuntimeMode::Normal,
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
        runtime_mode: RuntimeMode::Normal,
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
        runtime_mode: RuntimeMode::Normal,
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
        runtime_mode: RuntimeMode::Normal,
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
        runtime_mode: RuntimeMode::Normal,
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
            runtime_mode: RuntimeMode::Normal,
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
        runtime_mode: RuntimeMode::Normal,
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
        runtime_mode: RuntimeMode::Normal,
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
        runtime_mode: RuntimeMode::Normal,
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
        runtime_mode: RuntimeMode::Normal,
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
        runtime_mode: RuntimeMode::Normal,
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
        runtime_mode: RuntimeMode::Normal,
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
        runtime_mode: RuntimeMode::Normal,
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
        runtime_mode: RuntimeMode::Normal,
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
        runtime_mode: RuntimeMode::Normal,
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

// ---- Sec.2: collect_source_chain -----------------------------------------

/// Helper: write a minimal event envelope to `log` and return its assigned
/// id. The optional `source_event_id` value is embedded in the payload so
/// `collect_source_chain` can follow the chain link.
fn push_event_with_source(
    log: &Arc<Mutex<EventLog>>,
    kind: &str,
    source_event_id: Option<u64>,
) -> u64 {
    use phantom_memory::event_log::EventSource as LogEventSource;
    let mut payload = serde_json::json!({});
    if let Some(id) = source_event_id {
        payload["source_event_id"] = serde_json::json!(id);
    }
    log.lock()
        .unwrap()
        .append(LogEventSource::Substrate, kind, payload)
        .unwrap()
        .id
}

/// A 3-event chain A -> B -> C (C triggered by B, B triggered by A) must
/// produce [C_id, B_id, A_id] — i.e. from the leaf to the root.
#[test]
fn source_chain_collects_full_causal_path() {
    let tmp = TempDir::new().unwrap();
    let log = Arc::new(Mutex::new(
        EventLog::open(&tmp.path().join("ev.jsonl")).unwrap(),
    ));

    let a_id = push_event_with_source(&log, "root.event", None);
    let b_id = push_event_with_source(&log, "middle.event", Some(a_id));
    let c_id = push_event_with_source(&log, "leaf.event", Some(b_id));

    let chain = collect_source_chain(c_id, &log);

    assert_eq!(
        chain,
        vec![c_id, b_id, a_id],
        "chain must traverse C -> B -> A, got {chain:?}"
    );
}

/// A root event with no `source_event_id` in its payload must produce a
/// single-element chain containing only its own ID.
#[test]
fn source_chain_root_event_returns_single_element_chain() {
    let tmp = TempDir::new().unwrap();
    let log = Arc::new(Mutex::new(
        EventLog::open(&tmp.path().join("ev.jsonl")).unwrap(),
    ));

    let root_id = push_event_with_source(&log, "standalone.event", None);

    let chain = collect_source_chain(root_id, &log);

    assert_eq!(
        chain,
        vec![root_id],
        "single root event must yield [root_id], got {chain:?}"
    );
}

/// A self-referential event (source_event_id == own id) must not loop:
/// the cycle guard must terminate the walk after visiting the ID once.
#[test]
fn source_chain_self_referential_event_does_not_loop() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("ev.jsonl");

    let log = Arc::new(Mutex::new(EventLog::open(&path).unwrap()));

    // Step 1: write a normal event to get an id.
    let placeholder_id = push_event_with_source(&log, "placeholder", None);
    // The next id will be placeholder_id + 1. Write an event whose
    // source_event_id points to its own id (the +1 slot).
    let self_id = placeholder_id + 1;
    {
        use phantom_memory::event_log::EventSource as LogEventSource;
        let payload = serde_json::json!({ "source_event_id": self_id });
        log.lock()
            .unwrap()
            .append(LogEventSource::Substrate, "self.ref", payload)
            .unwrap();
    }

    let chain = collect_source_chain(self_id, &log);

    assert_eq!(
        chain,
        vec![self_id],
        "self-referential event must yield [self_id], got {chain:?}"
    );
}

/// `collect_source_chain` must traverse the same path as a manual walk
/// of `source_event_id` payload links. A 4-node chain is used to cover
/// more hops than the basic 3-node test.
#[test]
fn collect_source_chain_matches_source_event_id_traversal() {
    let tmp = TempDir::new().unwrap();
    let log = Arc::new(Mutex::new(
        EventLog::open(&tmp.path().join("ev.jsonl")).unwrap(),
    ));

    let e1 = push_event_with_source(&log, "ev1", None);
    let e2 = push_event_with_source(&log, "ev2", Some(e1));
    let e3 = push_event_with_source(&log, "ev3", Some(e2));
    let e4 = push_event_with_source(&log, "ev4", Some(e3));

    let chain = collect_source_chain(e4, &log);

    assert_eq!(
        chain,
        vec![e4, e3, e2, e1],
        "4-node chain must traverse e4 -> e3 -> e2 -> e1, got {chain:?}"
    );
}

// ---- Issue #105: RuntimeMode::SpawnOnly gate ----------------------------

/// In `SpawnOnly` mode every non-spawn tool is denied before any
/// capability or handler runs. Calling `read_file` must return a
/// runtime-denied failure even though the agent is Conversational (which
/// has Sense capability).
#[test]
fn spawn_only_blocks_non_spawn_tools() {
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join("probe.txt"), "should-not-be-read").unwrap();

    let ctx = DispatchContext {
        self_ref: AgentRef::new(1, AgentRole::Conversational, "harness-agent", SpawnSource::User),
        role: AgentRole::Conversational,
        working_dir: tmp.path(),
        registry: Arc::new(Mutex::new(AgentRegistry::new())),
        event_log: None,
        pending_spawn: new_spawn_subagent_queue(),
        source_event_id: None,
        quarantine: None,
        correlation_id: None,
        ticket_dispatcher: None,
        runtime_mode: RuntimeMode::SpawnOnly,
    };

    let res = dispatch_tool("read_file", &json!({"path": "probe.txt"}), &ctx);

    assert!(!res.success, "SpawnOnly must block read_file");
    assert!(
        res.output.contains("runtime denied"),
        "expected runtime denied message, got: {}",
        res.output,
    );
}

/// In `SpawnOnly` mode `spawn_subagent` still passes through to the
/// handler (which may return an error about missing args, but that proves
/// the gate did not block it).
#[test]
fn spawn_only_permits_spawn_subagent() {
    let tmp = TempDir::new().unwrap();

    let ctx = DispatchContext {
        self_ref: AgentRef::new(2, AgentRole::Composer, "harness-orch", SpawnSource::User),
        role: AgentRole::Composer,
        working_dir: tmp.path(),
        registry: Arc::new(Mutex::new(AgentRegistry::new())),
        event_log: None,
        pending_spawn: new_spawn_subagent_queue(),
        source_event_id: None,
        quarantine: None,
        correlation_id: None,
        ticket_dispatcher: None,
        runtime_mode: RuntimeMode::SpawnOnly,
    };

    // Empty args — handler may fail on validation but must not be
    // blocked by the runtime gate. The output must NOT start with
    // "runtime denied".
    let res = dispatch_tool("spawn_subagent", &json!({}), &ctx);
    assert!(
        !res.output.starts_with("runtime denied"),
        "spawn_subagent must pass the SpawnOnly gate; got: {}",
        res.output,
    );
}

/// In `SpawnOnly` mode a denied tool call must be appended to the event
/// log with kind `"runtime.denied"`.
#[test]
fn spawn_only_denial_is_logged_to_event_log() {
    use phantom_memory::event_log::EventLog;
    let tmp = TempDir::new().unwrap();
    let log_path = tmp.path().join("events.jsonl");
    let log = Arc::new(Mutex::new(
        EventLog::open(&log_path).expect("open event log"),
    ));

    let ctx = DispatchContext {
        self_ref: AgentRef::new(3, AgentRole::Conversational, "spy-agent", SpawnSource::User),
        role: AgentRole::Conversational,
        working_dir: tmp.path(),
        registry: Arc::new(Mutex::new(AgentRegistry::new())),
        event_log: Some(log.clone()),
        pending_spawn: new_spawn_subagent_queue(),
        source_event_id: None,
        quarantine: None,
        correlation_id: None,
        ticket_dispatcher: None,
        runtime_mode: RuntimeMode::SpawnOnly,
    };

    let res = dispatch_tool("write_file", &json!({"path": "x.txt", "content": "boom"}), &ctx);
    assert!(!res.success, "must be denied");
    assert!(res.output.contains("runtime denied"), "wrong error: {}", res.output);

    // The event log must contain a runtime.denied entry.
    let events = log.lock().unwrap().tail(32);
    let denied = events.iter().any(|e| e.kind == "runtime.denied");
    assert!(denied, "runtime.denied must be in the event log; got: {:?}", events);
}

/// `Normal` mode must be transparent — tools are routed by capability
/// class as always, with no additional restriction from `runtime_mode`.
#[test]
fn normal_mode_is_transparent() {
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join("visible.txt"), "hello-normal").unwrap();

    let ctx = DispatchContext {
        self_ref: AgentRef::new(4, AgentRole::Conversational, "normal-agent", SpawnSource::User),
        role: AgentRole::Conversational,
        working_dir: tmp.path(),
        registry: Arc::new(Mutex::new(AgentRegistry::new())),
        event_log: None,
        pending_spawn: new_spawn_subagent_queue(),
        source_event_id: None,
        quarantine: None,
        correlation_id: None,
        ticket_dispatcher: None,
        runtime_mode: RuntimeMode::Normal,
    };

    let res = dispatch_tool("read_file", &json!({"path": "visible.txt"}), &ctx);
    assert!(res.success, "Normal mode must allow Sense tools: {}", res.output);
    assert_eq!(res.output, "hello-normal");
}
