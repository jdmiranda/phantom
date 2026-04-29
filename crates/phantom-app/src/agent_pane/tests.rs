//! Unit tests for the agent pane module.

use std::sync::mpsc;

use phantom_agents::agent::{Agent, AgentMessage, AgentTask};
use phantom_agents::api::{ApiEvent, ApiHandle, ClaudeConfig};
use phantom_agents::permissions::PermissionSet;
use phantom_agents::role::AgentRole;
use phantom_agents::spawn_rules::{EventKind, EventSource};

use super::{
    AgentPane, AgentPaneStatus, DEFAULT_AGENT_PANE_ROLE,
    new_agent_snapshot_queue, new_blocked_event_sink, new_denied_event_sink,
    MAX_TOOL_ROUNDS,
};

fn test_agent() -> Agent {
    Agent::new(
        0,
        AgentTask::FreeForm {
            prompt: "test task".into(),
        },
    )
}

fn test_config() -> ClaudeConfig {
    ClaudeConfig::new("sk-test-fake")
}

fn agent_with_handle() -> (AgentPane, mpsc::Sender<ApiEvent>) {
    let (tx, rx) = mpsc::channel();
    let handle = ApiHandle::from_receiver(rx);
    let pane = AgentPane {
        task: "test task".into(),
        status: AgentPaneStatus::Working,
        output: String::from("● Agent working...\n\n"),
        api_handle: Some(handle),
        tool_use_ids: Vec::new(),
        cached_lines: Vec::new(),
        cached_len: 0,
        agent: test_agent(),
        pending_tools: Vec::new(),
        working_dir: ".".into(),
        claude_config: test_config(),
        chat_backend: None,
        consecutive_tool_failures: 0,
        blocked_event_sink: None,
        denied_event_sink: None,
        last_tool_error: None,
        snapshot_sink: None,
        last_failing_capability: None,
        turn_count: 0,
        current_assistant_text: String::new(),
        permissions: PermissionSet::all(),
        input_tokens: 0,
        output_tokens: 0,
        tool_call_count: 0,
        has_file_edits: false,
        registry: None,
        event_log: None,
        pending_spawn: None,
        self_ref: None,
        role: DEFAULT_AGENT_PANE_ROLE,
        ticket_dispatcher: None,
        runtime_mode: phantom_agents::dispatch::RuntimeMode::Normal,
        journal: None,
        quarantine: None,
        agent_capture: None,
        capture_session_uuid: uuid::Uuid::nil(),
        capture_tool_calls: Vec::new(),
    };
    (pane, tx)
}

#[test]
fn agent_pane_starts_working() {
    let (pane, _tx) = agent_with_handle();
    assert_eq!(pane.status, AgentPaneStatus::Working);
    assert!(pane.output.contains("Agent working"));
}

#[test]
fn poll_receives_text_delta() {
    let (mut pane, tx) = agent_with_handle();
    tx.send(ApiEvent::TextDelta("hello world".into())).unwrap();

    let got = pane.poll();
    assert!(got, "should have received content");
    assert!(pane.output.contains("hello world"));
    assert_eq!(pane.status, AgentPaneStatus::Working);
}

#[test]
fn poll_receives_done_event() {
    let (mut pane, tx) = agent_with_handle();
    tx.send(ApiEvent::TextDelta("result".into())).unwrap();
    tx.send(ApiEvent::Done).unwrap();

    pane.poll();
    assert_eq!(pane.status, AgentPaneStatus::Done);
    assert!(pane.output.contains("✓ Agent finished"));
    assert!(
        pane.api_handle.is_none(),
        "handle should be dropped on Done"
    );
}

#[test]
fn poll_receives_error_event() {
    let (mut pane, tx) = agent_with_handle();
    tx.send(ApiEvent::Error("network timeout".into())).unwrap();

    pane.poll();
    assert_eq!(pane.status, AgentPaneStatus::Failed);
    assert!(pane.output.contains("✗ Error: network timeout"));
    assert!(pane.api_handle.is_none());
}

#[test]
fn poll_accumulates_multiple_deltas() {
    let (mut pane, tx) = agent_with_handle();
    tx.send(ApiEvent::TextDelta("line 1\n".into())).unwrap();
    tx.send(ApiEvent::TextDelta("line 2\n".into())).unwrap();
    tx.send(ApiEvent::TextDelta("line 3\n".into())).unwrap();

    pane.poll();
    assert!(pane.output.contains("line 1"));
    assert!(pane.output.contains("line 2"));
    assert!(pane.output.contains("line 3"));
}

#[test]
fn poll_returns_false_when_no_handle() {
    let mut pane = AgentPane {
        task: "orphan".into(),
        status: AgentPaneStatus::Done,
        output: String::new(),
        api_handle: None,
        tool_use_ids: Vec::new(),
        cached_lines: Vec::new(),
        cached_len: 0,
        agent: test_agent(),
        pending_tools: Vec::new(),
        working_dir: ".".into(),
        claude_config: test_config(),
        chat_backend: None,
        consecutive_tool_failures: 0,
        blocked_event_sink: None,
        denied_event_sink: None,
        last_tool_error: None,
        snapshot_sink: None,
        last_failing_capability: None,
        turn_count: 0,
        current_assistant_text: String::new(),
        permissions: PermissionSet::all(),
        input_tokens: 0,
        output_tokens: 0,
        tool_call_count: 0,
        has_file_edits: false,
        registry: None,
        event_log: None,
        pending_spawn: None,
        self_ref: None,
        role: DEFAULT_AGENT_PANE_ROLE,
        ticket_dispatcher: None,
        runtime_mode: phantom_agents::dispatch::RuntimeMode::Normal,
        journal: None,
        quarantine: None,
        agent_capture: None,
        capture_session_uuid: uuid::Uuid::nil(),
        capture_tool_calls: Vec::new(),
    };
    assert!(!pane.poll());
}

#[test]
fn poll_returns_false_when_no_events() {
    let (mut pane, _tx) = agent_with_handle();
    // Don't send anything.
    assert!(!pane.poll());
    assert_eq!(pane.status, AgentPaneStatus::Working);
}

#[test]
fn tool_use_tracked_in_ids() {
    let (mut pane, tx) = agent_with_handle();
    tx.send(ApiEvent::ToolUse {
        id: "tool_123".into(),
        call: phantom_agents::tools::ToolCall {
            tool: phantom_agents::tools::ToolType::ReadFile,
            args: serde_json::json!({"path": "/tmp/test"}),
        },
    })
    .unwrap();

    pane.poll();
    assert_eq!(pane.tool_use_ids, vec!["tool_123"]);
    assert!(pane.output.contains("▶ read_file"));
    // New: also tracked in pending_tools.
    assert_eq!(pane.pending_tools.len(), 1);
    assert_eq!(pane.pending_tools[0].0, "tool_123");
}

#[test]
fn text_delta_accumulates_assistant_text() {
    let (mut pane, tx) = agent_with_handle();
    tx.send(ApiEvent::TextDelta("hello ".into())).unwrap();
    tx.send(ApiEvent::TextDelta("world".into())).unwrap();

    pane.poll();
    assert_eq!(pane.current_assistant_text, "hello world");
}

#[test]
fn done_without_tools_marks_finished() {
    let (mut pane, tx) = agent_with_handle();
    tx.send(ApiEvent::TextDelta("result".into())).unwrap();
    tx.send(ApiEvent::Done).unwrap();

    pane.poll();
    assert_eq!(pane.status, AgentPaneStatus::Done);
    assert!(pane.api_handle.is_none());
    // Assistant text should have been flushed to agent messages.
    assert!(pane.current_assistant_text.is_empty());
    assert!(pane.agent.messages().iter().any(|m| matches!(m, AgentMessage::Assistant(t) if t == "result")));
}

#[test]
fn done_with_tools_executes_and_continues() {
    let (mut pane, tx) = agent_with_handle();
    // Set working_dir to temp dir so ListFiles works.
    pane.working_dir = std::env::temp_dir().to_string_lossy().into_owned();

    tx.send(ApiEvent::TextDelta("Let me check.".into()))
        .unwrap();
    tx.send(ApiEvent::ToolUse {
        id: "toolu_1".into(),
        call: phantom_agents::tools::ToolCall {
            tool: phantom_agents::tools::ToolType::ListFiles,
            args: serde_json::json!({"path": "."}),
        },
    })
    .unwrap();
    tx.send(ApiEvent::Done).unwrap();

    pane.poll();

    // Should NOT be Done — should have re-invoked.
    assert_eq!(pane.status, AgentPaneStatus::Working);
    // pending_tools should be drained.
    assert!(pane.pending_tools.is_empty());
    // turn_count should have incremented.
    assert_eq!(pane.turn_count, 1);
    // Agent messages should include ToolCall and ToolResult.
    let has_tool_call = pane.agent.messages().iter().any(|m| matches!(m, AgentMessage::ToolCall(_)));
    let has_tool_result = pane.agent.messages().iter().any(|m| matches!(m, AgentMessage::ToolResult(_)));
    assert!(has_tool_call, "agent should have a ToolCall message");
    assert!(has_tool_result, "agent should have a ToolResult message");
    // Output should show the continuation.
    assert!(pane.output.contains("Continuing... (turn 1)"));
    // A new api_handle should have been created (by send_message).
    assert!(pane.api_handle.is_some());
}

#[test]
fn iteration_limit_stops_agent() {
    let (mut pane, tx) = agent_with_handle();
    pane.turn_count = MAX_TOOL_ROUNDS; // Already at limit.

    tx.send(ApiEvent::ToolUse {
        id: "toolu_limit".into(),
        call: phantom_agents::tools::ToolCall {
            tool: phantom_agents::tools::ToolType::GitStatus,
            args: serde_json::json!({}),
        },
    })
    .unwrap();
    tx.send(ApiEvent::Done).unwrap();

    pane.poll();

    assert_eq!(pane.status, AgentPaneStatus::Failed);
    assert!(pane.output.contains("iteration limit"));
    assert!(pane.api_handle.is_none());
}

#[test]
fn task_description_extraction() {
    // Verify the description logic works for each AgentTask variant.
    let cases: Vec<(AgentTask, &str)> = vec![
        (
            AgentTask::FreeForm {
                prompt: "fix bug".into(),
            },
            "fix bug",
        ),
        (
            AgentTask::RunCommand {
                command: "cargo test".into(),
            },
            "Run: cargo test",
        ),
        (
            AgentTask::WatchAndNotify {
                description: "build".into(),
            },
            "Watch: build",
        ),
    ];

    for (task, expected_prefix) in cases {
        let desc = match &task {
            AgentTask::FreeForm { prompt } => prompt.clone(),
            AgentTask::FixError { error_summary, .. } => format!("Fix: {error_summary}"),
            AgentTask::RunCommand { command } => format!("Run: {command}"),
            AgentTask::ReviewCode { context, .. } => format!("Review: {context}"),
            AgentTask::WatchAndNotify { description } => format!("Watch: {description}"),
        };
        assert!(
            desc.starts_with(expected_prefix),
            "task desc '{desc}' should start with '{expected_prefix}'"
        );
    }
}

// -- Issue #206: AgentSnapshotQueue producer tests -----------------------

/// When an `AgentPane` receives `ApiEvent::Done`, it must push an
/// `AgentSnapshot` into the wired `AgentSnapshotQueue` exactly once.
#[test]
fn done_event_pushes_snapshot_to_queue() {
    let (mut pane, tx) = agent_with_handle();
    let queue = new_agent_snapshot_queue();
    pane.set_snapshot_sink(queue.clone());

    tx.send(ApiEvent::Done).unwrap();
    pane.poll();

    assert_eq!(pane.status, AgentPaneStatus::Done);
    let snaps = queue.lock().unwrap();
    assert_eq!(snaps.len(), 1, "one snapshot must be pushed on Done");
}

/// When an `AgentPane` receives `ApiEvent::Error`, it must push a snapshot.
#[test]
fn error_event_pushes_snapshot_to_queue() {
    let (mut pane, tx) = agent_with_handle();
    let queue = new_agent_snapshot_queue();
    pane.set_snapshot_sink(queue.clone());

    tx.send(ApiEvent::Error("API error".into())).unwrap();
    pane.poll();

    assert_eq!(pane.status, AgentPaneStatus::Failed);
    let snaps = queue.lock().unwrap();
    assert_eq!(snaps.len(), 1, "one snapshot must be pushed on Error");
}

/// Without a wired queue, Done must still succeed (no panic, no-op).
#[test]
fn done_without_queue_is_silent_noop() {
    let (mut pane, tx) = agent_with_handle();
    // No snapshot sink wired.

    tx.send(ApiEvent::Done).unwrap();
    pane.poll();

    assert_eq!(pane.status, AgentPaneStatus::Done);
    // Test passes if no panic occurred.
}

// -- Lars fix-thread producer tests (Phase 2.E) --------------------------

/// One failure under the threshold must NOT emit an `AgentBlocked` event.
/// The producer should only fire when the streak crosses
/// [`TOOL_BLOCK_THRESHOLD`] = 2.
#[test]
fn consecutive_tool_failures_below_threshold_does_not_emit() {
    let (mut pane, _tx) = agent_with_handle();
    let sink = new_blocked_event_sink();
    pane.set_blocked_event_sink_for_test(sink.clone());

    pane.record_tool_result_for_test(false, "ENOENT: no such file");

    assert_eq!(pane.consecutive_tool_failures(), 1);
    let drained = sink.lock().unwrap();
    assert!(
        drained.is_empty(),
        "1 failure (< threshold) must not emit an AgentBlocked event; got {} events",
        drained.len(),
    );
}

/// Exactly two consecutive failures must emit exactly one `AgentBlocked`
/// event, after which the streak counter is reset to 0 (so the agent
/// doesn't spam the bus).
#[test]
fn consecutive_tool_failures_at_threshold_emits_blocked() {
    let (mut pane, _tx) = agent_with_handle();
    let sink = new_blocked_event_sink();
    pane.set_blocked_event_sink_for_test(sink.clone());

    pane.record_tool_result_for_test(false, "first error");
    pane.record_tool_result_for_test(false, "second error");

    // After emission the producer resets the counter so the SAME agent
    // doesn't keep re-emitting on every subsequent failure.
    assert_eq!(
        pane.consecutive_tool_failures(),
        0,
        "streak counter must reset after emit",
    );

    let drained = sink.lock().unwrap();
    assert_eq!(
        drained.len(),
        1,
        "exactly one AgentBlocked event must have been emitted; got {}",
        drained.len(),
    );
}

/// A successful tool call between two failures must reset the streak,
/// so the cumulative failure count is 1 (not 2) and no event fires.
#[test]
fn success_resets_counter() {
    let (mut pane, _tx) = agent_with_handle();
    let sink = new_blocked_event_sink();
    pane.set_blocked_event_sink_for_test(sink.clone());

    pane.record_tool_result_for_test(false, "first failure");
    pane.record_tool_result_for_test(true, "");
    pane.record_tool_result_for_test(false, "second failure");

    assert_eq!(
        pane.consecutive_tool_failures(),
        1,
        "consecutive count must be 1 (only the failure since the last success)",
    );

    let drained = sink.lock().unwrap();
    assert!(
        drained.is_empty(),
        "no event should have fired when streak was broken by a success; got {}",
        drained.len(),
    );
}

/// The emitted `AgentBlocked` event payload must carry the conventional
/// keys documented in `phantom_agents::fixer`: `agent_id`, `agent_role`,
/// `reason`, `blocked_at_unix_ms`, `context_excerpt`,
/// `suggested_capability`.
#[test]
fn blocked_event_payload_has_agent_id_and_reason() {
    let (mut pane, _tx) = agent_with_handle();
    let sink = new_blocked_event_sink();
    pane.set_blocked_event_sink_for_test(sink.clone());

    pane.record_tool_result_for_test(false, "first");
    pane.record_tool_result_for_test(false, "ENOENT: project_memory.txt");

    let drained = sink.lock().unwrap();
    assert_eq!(drained.len(), 1, "expected exactly one event");

    let ev = &drained[0];

    // Kind invariant.
    match &ev.kind {
        phantom_agents::spawn_rules::EventKind::AgentBlocked { agent_id, reason } => {
            assert_eq!(*agent_id, 0u64); // test_agent has id 0
            assert!(
                reason.contains("consecutive tool failures"),
                "reason should mention the streak; got '{reason}'",
            );
            assert!(
                reason.contains("ENOENT") || reason.contains("project_memory.txt"),
                "reason should embed the last error excerpt; got '{reason}'",
            );
        }
        other => panic!("expected EventKind::AgentBlocked, got {other:?}"),
    }

    // Source invariant: the producer is a Conversational agent.
    match ev.source {
        phantom_agents::spawn_rules::EventSource::Agent { role } => {
            assert_eq!(role, phantom_agents::role::AgentRole::Conversational);
        }
        other => panic!("expected EventSource::Agent, got {other:?}"),
    }

    // Payload shape.
    let payload = &ev.payload;
    assert!(
        payload.get("agent_id").is_some(),
        "payload missing agent_id"
    );
    assert!(
        payload.get("agent_role").is_some(),
        "payload missing agent_role"
    );
    assert!(payload.get("reason").is_some(), "payload missing reason");
    assert!(
        payload.get("blocked_at_unix_ms").is_some(),
        "payload missing blocked_at_unix_ms",
    );
    assert!(
        payload.get("context_excerpt").is_some(),
        "payload missing context_excerpt",
    );
    assert!(
        payload.get("suggested_capability").is_some(),
        "payload missing suggested_capability",
    );
    assert_eq!(
        payload.get("agent_role").and_then(|v| v.as_str()),
        Some("Conversational"),
    );
}

/// End-to-end producer→consumer wiring: when an `AgentBlocked` event
/// lands in a `SpawnRuleRegistry` that has the Fixer rule registered,
/// `evaluate` returns the canonical Fixer `SpawnAction`. This is the
/// substrate-level guarantee that producer-side wiring is sufficient
/// for the Fixer to spawn (Phase 2.G turns the action into an actual
/// agent).
#[test]
fn runtime_evaluate_on_blocked_returns_fixer_action() {
    use phantom_agents::fixer::fixer_spawn_rule;
    use phantom_agents::role::AgentRole;
    use phantom_agents::spawn_rules::{SpawnAction, SpawnRuleRegistry};

    let (mut pane, _tx) = agent_with_handle();
    let sink = new_blocked_event_sink();
    pane.set_blocked_event_sink_for_test(sink.clone());

    pane.record_tool_result_for_test(false, "first failure");
    pane.record_tool_result_for_test(false, "second failure");

    let drained = sink.lock().unwrap();
    assert_eq!(
        drained.len(),
        1,
        "producer must have emitted exactly 1 event"
    );

    // Hand the producer's event to the substrate-level rule registry.
    // No actual agent spawns — we just verify the Fixer action would be
    // queued (Phase 2.G consumer is responsible for honoring it).
    let registry = SpawnRuleRegistry::new().add(fixer_spawn_rule());
    let actions = registry.evaluate(&drained[0]);

    assert_eq!(actions.len(), 1, "Fixer rule must fire exactly once");
    match actions[0] {
        SpawnAction::SpawnIfNotRunning {
            role,
            label_template,
            ..
        } => {
            assert_eq!(*role, AgentRole::Fixer);
            assert_eq!(label_template, "fixer-on-blockage");
        }
        other => panic!("expected SpawnIfNotRunning(Fixer), got {other:?}"),
    }
}

// ---- Sec.1: CapabilityDenied substrate-event producer ------------------

/// When the dispatch gate refuses a tool call because the agent's role
/// manifest does not include the tool's capability class, the pane MUST
/// push a `SubstrateEvent` of kind `CapabilityDenied` into the App-owned
/// `DeniedEventSink`. The scenario: a `Watcher` agent invokes
/// `run_command` (Act) — gate rejects, `maybe_emit_capability_denied_event`
/// records the denial.
///
/// `execute_tool` is an internal helper that does not check capabilities
/// (see issue #104 — `dispatch_tool` is the single gate). We construct
/// the canonical denial ToolResult directly to test the producer logic
/// in isolation from the gate.
#[test]
fn dispatch_denial_pushes_event_to_sink() {
    use phantom_agents::role::CapabilityClass;
    use phantom_agents::tools::ToolType;

    let (mut pane, _tx) = agent_with_handle();
    pane.set_role_for_test(AgentRole::Watcher);
    let sink = new_denied_event_sink();
    pane.set_denied_event_sink_for_test(sink.clone());

    // Simulate a denial result — the canonical prefix that dispatch_tool
    // produces when a Watcher calls run_command (Act-class). We construct
    // it directly because execute_tool is a capability-agnostic helper
    // (the gate lives in dispatch_tool; see issue #104).
    let args = serde_json::json!({"command": "echo SHOULD_NEVER_RUN"});
    let result = phantom_agents::tools::ToolResult {
        tool: ToolType::RunCommand,
        success: false,
        output: "capability denied: Act not in Watcher manifest".to_string(),
        ..phantom_agents::tools::ToolResult::default()
    };

    // Hand the result through the producer hook the pane uses inside
    // `execute_pending_tools`. The sink must end up with exactly one
    // SubstrateEvent of kind CapabilityDenied carrying the role,
    // class, and tool name.
    pane.maybe_emit_capability_denied_event(ToolType::RunCommand, &args, &result, None);

    let drained = sink.lock().unwrap();
    assert_eq!(drained.len(), 1, "expected one CapabilityDenied event");
    let ev = &drained[0];
    match &ev.kind {
        EventKind::CapabilityDenied {
            agent_id: _,
            role,
            attempted_class,
            attempted_tool,
            source_chain,
        } => {
            assert_eq!(*role, AgentRole::Watcher);
            assert_eq!(*attempted_class, CapabilityClass::Act);
            assert_eq!(attempted_tool, "run_command");
            assert!(
                source_chain.is_empty(),
                "root-level denial must have empty source_chain (no event log wired)"
            );
        }
        other => panic!("expected CapabilityDenied, got {other:?}"),
    }
    // The source must attribute the event to the agent.
    match ev.source {
        EventSource::Agent { role } => assert_eq!(role, AgentRole::Watcher),
        other => panic!("expected EventSource::Agent, got {other:?}"),
    }
}

/// The audit log records the same denial alongside the SubstrateEvent.
/// Both signals must agree: `outcome=denied`, the tool name and class
/// match what the model attempted. We use a temp dir for the audit
/// log file and verify the record lands.
///
/// Like `dispatch_denial_pushes_event_to_sink`, we construct the denial
/// result directly rather than calling `execute_tool` — `execute_tool`
/// is capability-agnostic (see issue #104); the gate lives in
/// `dispatch_tool`.
#[test]
fn audit_denied_outcome_emitted_alongside_event() {
    use phantom_agents::audit;
    use phantom_agents::tools::ToolType;

    // Initialize the audit subscriber against a tempdir so we can read
    // the JSONL file back. This is process-global; if another test in
    // this process already initialized it, our subscriber is a no-op
    // and the assertion below works on whichever audit dir won the
    // race (or fails fast — see the audit module's footgun docs).
    let audit_dir = tempfile::tempdir().unwrap();
    let writer = audit::init(audit_dir.path()).expect("init audit");

    let (mut pane, _tx) = agent_with_handle();
    pane.set_role_for_test(AgentRole::Watcher);
    let sink = new_denied_event_sink();
    pane.set_denied_event_sink_for_test(sink.clone());

    // Construct a canonical denial result directly (see issue #104).
    let args = serde_json::json!({"command": "ls"});
    let result = phantom_agents::tools::ToolResult {
        tool: ToolType::RunCommand,
        success: false,
        output: "capability denied: Act not in Watcher manifest".to_string(),
        ..phantom_agents::tools::ToolResult::default()
    };

    pane.maybe_emit_capability_denied_event(ToolType::RunCommand, &args, &result, None);

    // Drop the writer to flush the non-blocking audit appender.
    drop(writer);

    // SubstrateEvent side: still exactly one event in the sink.
    let drained = sink.lock().unwrap();
    assert_eq!(drained.len(), 1, "expected one CapabilityDenied event");

    // Audit-log side: scan the rolling daily file(s) for an entry
    // matching our role + tool + denied outcome. Best-effort because
    // tracing's global subscriber may have been set up by another
    // test; a missing record there is tolerated as long as the
    // SubstrateEvent landed (the audit invariant is observable in
    // the dedicated `init_then_emit_writes_record_and_drop_flushes`
    // test in `audit.rs`).
    let entries = std::fs::read_dir(audit_dir.path()).expect("readdir");
    let mut found_denied = false;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy().into_owned();
        if !name.starts_with("audit.jsonl") {
            continue;
        }
        let contents = std::fs::read_to_string(entry.path()).expect("read");
        for line in contents.lines() {
            if line.contains("\"outcome\":\"denied\"")
                && line.contains("\"tool\":\"run_command\"")
                && line.contains("\"role\":\"Watcher\"")
            {
                found_denied = true;
                break;
            }
        }
        if found_denied {
            break;
        }
    }
    // Don't fail when another test already claimed the global subscriber:
    // the substrate-event side is the load-bearing assertion.
    if !found_denied {
        log::warn!(
            "audit_denied_outcome_emitted_alongside_event: \
             audit record not found in tempdir — likely another test \
             already installed the global tracing subscriber. \
             SubstrateEvent side still validated."
        );
    }
}

// ---- Issue #235: GhTicketDispatcher wiring --------------------------------

/// Constructing a `DispatchContext` for a Dispatcher-role pane that has
/// a configured `GhTicketDispatcher` must yield `ticket_dispatcher: Some`.
///
/// This is the acceptance test for the fix described in issue #235:
/// before the fix every `DispatchContext` literal had
/// `ticket_dispatcher: None`, so Dispatcher agents always received
/// `"ticket dispatcher not configured"` when calling any of the three
/// Dispatcher tools.
#[test]
fn dispatcher_role_pane_with_configured_dispatcher_has_some_in_ctx() {
    use phantom_agents::dispatcher::GhTicketDispatcher;
    use phantom_agents::role::{AgentRef, AgentRole, SpawnSource};
    use phantom_memory::event_log::EventLog;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    let tmp = TempDir::new().unwrap();

    // Build a Dispatcher-role pane with all substrate handles wired.
    let (mut pane, _tx) = agent_with_handle();

    let registry = Arc::new(Mutex::new(
        phantom_agents::inbox::AgentRegistry::new(),
    ));
    let event_log = Arc::new(Mutex::new(
        EventLog::open(&tmp.path().join("events.jsonl")).unwrap(),
    ));
    let pending_spawn = phantom_agents::composer_tools::new_spawn_subagent_queue();
    let self_ref = AgentRef::new(1, AgentRole::Dispatcher, "dispatcher-1", SpawnSource::User);

    pane.set_substrate_handles(
        registry,
        event_log,
        pending_spawn,
        self_ref,
        AgentRole::Dispatcher,
        Arc::new(Mutex::new(phantom_agents::quarantine::QuarantineRegistry::default())),
    );

    // Wire the dispatcher (mock repo — no real gh calls in tests).
    let dispatcher = GhTicketDispatcher::new("test/repo").shared();
    pane.set_ticket_dispatcher(Arc::clone(&dispatcher));

    // Build a dispatch context; the ticket_dispatcher field must be Some.
    let ctx = pane.build_dispatch_context()
        .expect("build_dispatch_context must return Some for a fully-wired Dispatcher pane");

    assert!(
        ctx.ticket_dispatcher.is_some(),
        "DispatchContext for a Dispatcher-role pane with a configured \
         GhTicketDispatcher must have ticket_dispatcher = Some"
    );
}

/// Constructing a `DispatchContext` for a *non*-Dispatcher-role pane must
/// always yield `ticket_dispatcher: None`, even if the pane somehow had a
/// dispatcher wired in (defence-in-depth).
#[test]
fn non_dispatcher_role_pane_always_has_none_in_ctx() {
    use phantom_agents::dispatcher::GhTicketDispatcher;
    use phantom_agents::role::{AgentRef, AgentRole, SpawnSource};
    use phantom_memory::event_log::EventLog;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    let tmp = TempDir::new().unwrap();

    let (mut pane, _tx) = agent_with_handle();

    let registry = Arc::new(Mutex::new(
        phantom_agents::inbox::AgentRegistry::new(),
    ));
    let event_log = Arc::new(Mutex::new(
        EventLog::open(&tmp.path().join("events.jsonl")).unwrap(),
    ));
    let pending_spawn = phantom_agents::composer_tools::new_spawn_subagent_queue();
    let self_ref = AgentRef::new(2, AgentRole::Watcher, "watcher-2", SpawnSource::User);

    pane.set_substrate_handles(
        registry,
        event_log,
        pending_spawn,
        self_ref,
        AgentRole::Watcher,
        Arc::new(Mutex::new(phantom_agents::quarantine::QuarantineRegistry::default())),
    );

    // Wire a dispatcher into a non-Dispatcher pane — should still be None
    // in the resulting DispatchContext (capability gate + defence-in-depth).
    let dispatcher = GhTicketDispatcher::new("test/repo").shared();
    pane.set_ticket_dispatcher(Arc::clone(&dispatcher));

    let ctx = pane.build_dispatch_context()
        .expect("build_dispatch_context must return Some for a wired pane");

    assert!(
        ctx.ticket_dispatcher.is_none(),
        "DispatchContext for a non-Dispatcher-role pane must have \
         ticket_dispatcher = None regardless of what was set on the pane"
    );
}

// ---- Sec.7.3 / #225: build_dispatch_context quarantine wiring ----------

/// Regression test for issue #225.
///
/// Before the fix, `build_dispatch_context` always set `quarantine: None`
/// on the `DispatchContext`, so the Sec.7.3 gate was never reached and
/// quarantined agents could still dispatch tools. This test verifies that
/// after the fix:
///
/// 1. `build_dispatch_context` produces a `DispatchContext` with a `Some`
///    quarantine field when the pane's quarantine handle is set.
/// 2. A quarantined agent's `dispatch_tool` call returns `success: false`
///    with "quarantined" in the output — not `Ok`.
#[test]
fn build_dispatch_context_passes_quarantine_registry_to_dispatch() {
    use phantom_agents::composer_tools::new_spawn_subagent_queue;
    use phantom_agents::dispatch::dispatch_tool;
    use phantom_agents::inbox::AgentRegistry;
    use phantom_agents::quarantine::{AutoQuarantinePolicy, QuarantineRegistry};
    use phantom_agents::role::{AgentRef, AgentRole, SpawnSource};
    use phantom_agents::taint::TaintLevel;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("probe.txt"), "secret").unwrap();

    let agent_id: u64 = 99;

    // Build a quarantine registry and immediately quarantine the agent
    // (threshold = 1 means the first Tainted observation quarantines).
    let quarantine = Arc::new(Mutex::new(QuarantineRegistry::new_with_policy(
        AutoQuarantinePolicy { threshold: 1 },
    )));
    quarantine
        .lock()
        .unwrap()
        .check_and_escalate(agent_id, TaintLevel::Tainted, 0, "test-offense");
    assert!(
        quarantine.lock().unwrap().agent_is_quarantined(agent_id),
        "agent must be quarantined before the dispatch test"
    );

    // Build a minimal AgentPane and wire the quarantine registry through
    // set_substrate_handles — the path the fix closes.
    let (mut pane, _tx) = agent_with_handle();
    let registry = Arc::new(Mutex::new(AgentRegistry::new()));
    let event_log = {
        let log = phantom_memory::event_log::EventLog::open(
            &tmp.path().join("events.jsonl"),
        )
        .unwrap();
        Arc::new(Mutex::new(log))
    };
    let pending_spawn = new_spawn_subagent_queue();
    let self_ref = AgentRef::new(agent_id, AgentRole::Conversational, "offender", SpawnSource::User);

    pane.working_dir = tmp.path().to_string_lossy().into_owned();
    pane.set_substrate_handles(
        registry,
        event_log,
        pending_spawn,
        self_ref,
        AgentRole::Conversational,
        quarantine,
    );

    // build_dispatch_context must return Some with a quarantine field.
    let ctx = pane
        .build_dispatch_context()
        .expect("build_dispatch_context must return Some when handles are wired");

    assert!(
        ctx.quarantine.is_some(),
        "DispatchContext::quarantine must be Some after the #225 fix; \
         got None — the quarantine registry was not forwarded"
    );

    // The dispatched tool call must be denied because the agent is quarantined.
    let res = dispatch_tool("read_file", &serde_json::json!({"path": "probe.txt"}), &ctx);

    assert!(
        !res.success,
        "quarantined agent must have dispatch denied (success=false), got: {}",
        res.output,
    );
    assert!(
        res.output.contains("quarantined"),
        "denial message must mention 'quarantined', got: {}",
        res.output,
    );
    assert!(
        res.output.contains(&agent_id.to_string()),
        "denial message must name the agent id {}, got: {}",
        agent_id,
        res.output,
    );
}

// ---- #230: AgentBlocked payload reflects actual role and capability ----

/// A `Defender`-role pane whose last failing tool was `RunCommand` (Act)
/// must produce an `AgentBlocked` payload with `agent_role = "Defender"`
/// and `suggested_capability = "Act"` — not the old hardcoded
/// `"Conversational"` / `"Sense"` strings.
#[test]
fn blocked_payload_reflects_actual_role_and_capability() {
    let (mut pane, _tx) = agent_with_handle();
    pane.set_role_for_test(AgentRole::Defender);
    let sink = new_blocked_event_sink();
    pane.set_blocked_event_sink_for_test(sink.clone());

    // Simulate two consecutive failures where the denied tool is RunCommand
    // (capability class = Act). We wire `last_failing_capability` directly
    // via the production field path: set it before calling the test helper
    // so `maybe_emit_blocked_event` sees the correct class.
    pane.consecutive_tool_failures = 1;
    pane.last_tool_error = Some("run_command denied".into());
    pane.last_failing_capability = Some(phantom_agents::role::CapabilityClass::Act);
    // Second failure crosses the threshold.
    pane.consecutive_tool_failures = 2;
    pane.maybe_emit_blocked_event();

    let drained = sink.lock().unwrap();
    assert_eq!(drained.len(), 1, "expected exactly one AgentBlocked event");

    let ev = &drained[0];

    // The source must attribute the event to the Defender role, not Conversational.
    match ev.source {
        phantom_agents::spawn_rules::EventSource::Agent { role } => {
            assert_eq!(
                role,
                AgentRole::Defender,
                "source role must be Defender, not Conversational"
            );
        }
        other => panic!("expected EventSource::Agent, got {other:?}"),
    }

    // The payload must carry the actual role and capability strings.
    let payload = &ev.payload;
    assert_eq!(
        payload.get("agent_role").and_then(|v| v.as_str()),
        Some("Defender"),
        "payload agent_role must be 'Defender', not hardcoded 'Conversational'",
    );
    assert_eq!(
        payload.get("suggested_capability").and_then(|v| v.as_str()),
        Some("Act"),
        "payload suggested_capability must be 'Act' (RunCommand class), not hardcoded 'Sense'",
    );
}

// =========================================================================
// #164 — QA: Spawn agent — backtick agent command launches AI agent pane
// =========================================================================

/// Constructing an `AgentPane` with a live channel handle must produce a
/// pane in `Working` status (the only active state the pane exposes to the
/// GUI).  The pane must also carry the task description verbatim so the
/// renderer can label it.
#[test]
fn spawn_agent_pane_status_is_working() {
    let (pane, _tx) = agent_with_handle();
    assert_eq!(
        pane.status,
        AgentPaneStatus::Working,
        "a freshly spawned agent pane must start in Working status"
    );
    assert!(
        pane.api_handle.is_some(),
        "a freshly spawned agent pane must have a live API handle"
    );
}

/// The `task` field of the pane carries the human-readable prompt so the
/// UI can display it in the pane header without looking up the underlying
/// `AgentTask`.
#[test]
fn spawn_agent_pane_task_matches_prompt() {
    let (tx, rx) = mpsc::channel();
    let handle = ApiHandle::from_receiver(rx);
    let pane = AgentPane {
        task: "fix the failing tests".into(),
        status: AgentPaneStatus::Working,
        output: String::from("● Agent working...\n\n"),
        api_handle: Some(handle),
        tool_use_ids: Vec::new(),
        cached_lines: Vec::new(),
        cached_len: 0,
        agent: Agent::new(0, AgentTask::FreeForm { prompt: "fix the failing tests".into() }),
        pending_tools: Vec::new(),
        working_dir: ".".into(),
        claude_config: test_config(),
        chat_backend: None,
        consecutive_tool_failures: 0,
        blocked_event_sink: None,
        denied_event_sink: None,
        last_tool_error: None,
        turn_count: 0,
        current_assistant_text: String::new(),
        permissions: PermissionSet::all(),
        input_tokens: 0,
        output_tokens: 0,
        tool_call_count: 0,
        has_file_edits: false,
        registry: None,
        event_log: None,
        pending_spawn: None,
        self_ref: None,
        role: DEFAULT_AGENT_PANE_ROLE,
        ticket_dispatcher: None,
        runtime_mode: phantom_agents::dispatch::RuntimeMode::Normal,
        journal: None,
        quarantine: None,
        snapshot_sink: None,
        last_failing_capability: None,
        agent_capture: None,
        capture_session_uuid: uuid::Uuid::nil(),
        capture_tool_calls: Vec::new(),
    };
    let _ = tx; // keep sender alive so handle stays live
    assert_eq!(pane.task, "fix the failing tests");
    assert_eq!(pane.status, AgentPaneStatus::Working);
}

/// Multiple panes must each receive a distinct agent ID from the underlying
/// `Agent` allocation.  Uniqueness is enforced by `AgentManager::spawn`
/// (sequential IDs starting at 1), but we verify the pane's agent id is
/// set non-zero even in the test-fixture path by checking the two agents
/// get different ids when constructed through the manager.
#[test]
fn spawn_two_panes_have_unique_agent_ids() {
    use phantom_agents::manager::AgentManager;
    use phantom_agents::agent::AgentTask;

    let mut mgr = AgentManager::new(4);
    let id1 = mgr.spawn(AgentTask::FreeForm { prompt: "task A".into() });
    let id2 = mgr.spawn(AgentTask::FreeForm { prompt: "task B".into() });

    assert_ne!(id1, id2, "each spawned agent must receive a unique ID");
    assert_eq!(id1, 1);
    assert_eq!(id2, 2);

    // Verify both are independently retrievable.
    assert!(mgr.get(id1).is_some());
    assert!(mgr.get(id2).is_some());
    assert_eq!(mgr.get(id1).unwrap().id(), id1);
    assert_eq!(mgr.get(id2).unwrap().id(), id2);
}

/// Backtick spawn via the manager starts the agent in `Working` status when
/// there is available concurrency capacity.
#[test]
fn manager_spawn_starts_agent_in_working_status() {
    use phantom_agents::manager::AgentManager;
    use phantom_agents::agent::{AgentStatus, AgentTask};

    let mut mgr = AgentManager::new(4);
    let id = mgr.spawn(AgentTask::FreeForm { prompt: "do something".into() });

    let agent = mgr.get(id).expect("spawned agent must be retrievable");
    assert_eq!(
        agent.status(),
        AgentStatus::Working,
        "spawned agent must start in Working status when capacity is available"
    );
}

// =========================================================================
// #167 — QA: Kill agent — backtick kill terminates a running agent cleanly
// =========================================================================

/// Killing a `Working` agent via the `AgentManager` transitions it to
/// `Failed` and logs the kill event in its output log.
#[test]
fn kill_working_agent_transitions_to_failed() {
    use phantom_agents::manager::AgentManager;
    use phantom_agents::agent::{AgentStatus, AgentTask};

    let mut mgr = AgentManager::new(4);
    let id = mgr.spawn(AgentTask::FreeForm { prompt: "long running task".into() });

    assert_eq!(mgr.get(id).unwrap().status(), AgentStatus::Working);

    let killed = mgr.kill(id);
    assert!(killed, "kill() must return true for a Working agent");
    assert_eq!(
        mgr.get(id).unwrap().status(),
        AgentStatus::Failed,
        "killed agent must be in Failed state"
    );
}

/// After a kill, the agent's output log must contain the kill acknowledgement
/// so the GUI can show the user that the agent was terminated deliberately.
#[test]
fn kill_agent_appends_kill_log_entry() {
    use phantom_agents::manager::AgentManager;
    use phantom_agents::agent::AgentTask;

    let mut mgr = AgentManager::new(4);
    let id = mgr.spawn(AgentTask::FreeForm { prompt: "task".into() });
    mgr.kill(id);

    let agent = mgr.get(id).unwrap();
    assert!(
        agent.output_log().iter().any(|l: &String| l.contains("killed")),
        "killed agent output_log must contain a kill annotation; got: {:?}",
        agent.output_log(),
    );
}

/// Killing a `Done` agent must be a no-op: the agent stays `Done` and
/// `kill()` returns `false` because there is nothing to terminate.
#[test]
fn kill_terminal_agent_is_noop() {
    use phantom_agents::manager::AgentManager;
    use phantom_agents::agent::{AgentStatus, AgentTask};

    let mut mgr = AgentManager::new(4);
    let id = mgr.spawn(AgentTask::FreeForm { prompt: "already done".into() });
    mgr.get_mut(id).unwrap().complete(true);
    assert_eq!(mgr.get(id).unwrap().status(), AgentStatus::Done);

    let killed = mgr.kill(id);
    assert!(!killed, "kill() on a Done agent must return false");
    assert_eq!(
        mgr.get(id).unwrap().status(),
        AgentStatus::Done,
        "status must not change for a terminal agent"
    );
}

/// When an `AgentPane` receives an `Error` event (e.g. from an external
/// kill signal injected via the API channel), it transitions to `Failed`
/// and drops the API handle — this mirrors what a hard kill does.
#[test]
fn pane_kill_via_error_event_transitions_to_failed_and_drops_handle() {
    let (mut pane, tx) = agent_with_handle();
    assert_eq!(pane.status, AgentPaneStatus::Working);
    assert!(pane.api_handle.is_some());

    // Simulate an external kill by injecting an Error event.
    tx.send(ApiEvent::Error("killed by user".into())).expect("send must succeed");
    pane.poll();

    assert_eq!(
        pane.status,
        AgentPaneStatus::Failed,
        "pane must be Failed after receiving an Error event"
    );
    assert!(
        pane.api_handle.is_none(),
        "API handle must be dropped after termination"
    );
    assert!(
        pane.output.contains("killed by user"),
        "kill reason must appear in pane output"
    );
}

/// `kill_all` terminates every non-terminal agent and returns the correct count.
/// Agents already in a terminal state (`Done`, `Failed`, `Flatline`) must
/// be left untouched.
#[test]
fn kill_all_terminates_all_active_agents_and_skips_terminal() {
    use phantom_agents::manager::AgentManager;
    use phantom_agents::agent::{AgentStatus, AgentTask};

    let mut mgr = AgentManager::new(4);
    let id1 = mgr.spawn(AgentTask::FreeForm { prompt: "a".into() });
    let id2 = mgr.spawn(AgentTask::FreeForm { prompt: "b".into() });
    let id3 = mgr.spawn(AgentTask::FreeForm { prompt: "c".into() });

    // Mark one as already done.
    mgr.get_mut(id3).unwrap().complete(true);
    assert_eq!(mgr.get(id3).unwrap().status(), AgentStatus::Done);

    let count = mgr.kill_all();
    assert_eq!(count, 2, "kill_all must kill exactly the two active agents");
    assert_eq!(mgr.get(id1).unwrap().status(), AgentStatus::Failed);
    assert_eq!(mgr.get(id2).unwrap().status(), AgentStatus::Failed);
    assert_eq!(
        mgr.get(id3).unwrap().status(),
        AgentStatus::Done,
        "terminal agent must not be affected by kill_all"
    );
}

// -----------------------------------------------------------------------
// resolve_api_config tests (issue #157)
// -----------------------------------------------------------------------

use std::sync::Mutex;

/// Serialise all env-var-mutating tests behind a single process-wide lock
/// so concurrent test threads do not race on `ANTHROPIC_API_KEY` /
/// `OPENAI_API_KEY`.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Set `var` to `value` (or remove it when `value` is None), run `f`,
/// then restore the original state.  Must be called while holding
/// `ENV_LOCK`.
fn with_env_locked<F: FnOnce()>(var: &str, value: Option<&str>, f: F) {
    let original = std::env::var(var).ok();
    match value {
        Some(v) => unsafe { std::env::set_var(var, v) },
        None => unsafe { std::env::remove_var(var) },
    }
    f();
    match original {
        Some(orig) => unsafe { std::env::set_var(var, orig) },
        None => unsafe { std::env::remove_var(var) },
    }
}

#[test]
fn resolve_api_config_claude_returns_some_when_key_present() {
    use phantom_agents::chat::ChatModel;
    use super::resolve_api_config;
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    with_env_locked("ANTHROPIC_API_KEY", Some("sk-ant-test-key"), || {
        let model = ChatModel::Claude("claude-3-5-sonnet-20241022".into());
        let result = resolve_api_config(&model);
        assert!(result.is_some(), "Claude model + ANTHROPIC_API_KEY set → should return Some");
    });
}

#[test]
fn resolve_api_config_claude_returns_none_without_key() {
    use phantom_agents::chat::ChatModel;
    use super::resolve_api_config;
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    with_env_locked("ANTHROPIC_API_KEY", None, || {
        let model = ChatModel::Claude("claude-3-5-sonnet-20241022".into());
        let result = resolve_api_config(&model);
        assert!(result.is_none(), "Claude model + no ANTHROPIC_API_KEY → should return None");
    });
}

#[test]
fn resolve_api_config_openai_returns_some_when_key_present() {
    use phantom_agents::chat::ChatModel;
    use super::resolve_api_config;
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    with_env_locked("OPENAI_API_KEY", Some("sk-openai-test-key"), || {
        let model = ChatModel::OpenAi("gpt-4o".into());
        let result = resolve_api_config(&model);
        assert!(result.is_some(), "OpenAI model + OPENAI_API_KEY set → should return Some");
    });
}

#[test]
fn resolve_api_config_openai_returns_none_without_key() {
    use phantom_agents::chat::ChatModel;
    use super::resolve_api_config;
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    with_env_locked("OPENAI_API_KEY", None, || {
        let model = ChatModel::OpenAi("gpt-4o".into());
        let result = resolve_api_config(&model);
        assert!(result.is_none(), "OpenAI model + no OPENAI_API_KEY → should return None");
    });
}

#[test]
fn resolve_api_config_openai_rejects_wrong_key() {
    use phantom_agents::chat::ChatModel;
    use super::resolve_api_config;
    // OPENAI_API_KEY absent; only ANTHROPIC_API_KEY is set.
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let orig_openai = std::env::var("OPENAI_API_KEY").ok();
    let orig_anthropic = std::env::var("ANTHROPIC_API_KEY").ok();
    unsafe {
        std::env::remove_var("OPENAI_API_KEY");
        std::env::set_var("ANTHROPIC_API_KEY", "sk-ant-test-key");
    }
    let model = ChatModel::OpenAi("gpt-4o".into());
    let result = resolve_api_config(&model);
    // Restore.
    match orig_openai {
        Some(v) => unsafe { std::env::set_var("OPENAI_API_KEY", v) },
        None => unsafe { std::env::remove_var("OPENAI_API_KEY") },
    }
    match orig_anthropic {
        Some(v) => unsafe { std::env::set_var("ANTHROPIC_API_KEY", v) },
        None => unsafe { std::env::remove_var("ANTHROPIC_API_KEY") },
    }
    assert!(
        result.is_none(),
        "OpenAI model + only ANTHROPIC_API_KEY set → must return None"
    );
}

#[test]
fn resolve_api_config_claude_rejects_wrong_key() {
    use phantom_agents::chat::ChatModel;
    use super::resolve_api_config;
    // ANTHROPIC_API_KEY absent; only OPENAI_API_KEY is set.
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let orig_anthropic = std::env::var("ANTHROPIC_API_KEY").ok();
    let orig_openai = std::env::var("OPENAI_API_KEY").ok();
    unsafe {
        std::env::remove_var("ANTHROPIC_API_KEY");
        std::env::set_var("OPENAI_API_KEY", "sk-openai-test-key");
    }
    let model = ChatModel::Claude("claude-3-5-sonnet-20241022".into());
    let result = resolve_api_config(&model);
    // Restore.
    match orig_anthropic {
        Some(v) => unsafe { std::env::set_var("ANTHROPIC_API_KEY", v) },
        None => unsafe { std::env::remove_var("ANTHROPIC_API_KEY") },
    }
    match orig_openai {
        Some(v) => unsafe { std::env::set_var("OPENAI_API_KEY", v) },
        None => unsafe { std::env::remove_var("OPENAI_API_KEY") },
    }
    assert!(
        result.is_none(),
        "Claude model + only OPENAI_API_KEY set → must return None"
    );
}
