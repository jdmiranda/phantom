//! Adversarial integration tests for the `complete_task` delivery path
//! (issue #646), building on `complete_task_spike.rs` and
//! `complete_task_broader.rs`.
//!
//! These tests exercise edge cases at the **phantom-agents** boundary:
//!
//! - The parser side: malformed `complete_task` inputs (non-object, null,
//!   array, primitive) must still emit `ApiEvent::CompleteTask` so the
//!   consumer-side validation gate can count them. The broader test
//!   already covers this for a handful of bad shapes; this file extends
//!   the matrix and verifies parser stability under sequences of malformed
//!   calls in one response.
//! - The agent-FSM side: `complete_with_result` semantics under repeated
//!   invocation, the `requires_complete_task` flag's persistence, and the
//!   completion_result payload's round-trip for unusual JSON shapes.
//! - End-to-end inlining of the agent loop for the parser → agent path
//!   under adversarial conditions.
//!
//! Note: the 3-strike `validation_failure_count` flatline and the
//! `MAX_TOOL_ROUNDS → "exited without complete_task"` reason rewrite live
//! in `phantom-app/src/agent_pane/` and `AgentPane`/`AgentPaneStatus` are
//! `pub(crate)`. Pane-level adversarial scenarios are covered by sibling
//! tests in `crates/phantom-app/src/agent_pane/tests.rs` — they cannot be
//! reached from an integration test in `phantom-agents/tests/`.

use std::sync::mpsc;

use phantom_agents::agent::{Agent, AgentSpawnOpts, AgentStatus, AgentTask};
use phantom_agents::api::{self, ApiEvent};

use serde_json::json;

/// Build the JSON body Claude returns for a single tool_use block.
fn claude_tool_response(name: &str, input: serde_json::Value) -> serde_json::Value {
    json!({
        "content": [
            {
                "type": "tool_use",
                "id": "toolu_adv_001",
                "name": name,
                "input": input,
            }
        ]
    })
}

/// Build the JSON body Claude returns for two tool_use blocks in one
/// response.
fn claude_two_tool_response(
    name_a: &str,
    input_a: serde_json::Value,
    name_b: &str,
    input_b: serde_json::Value,
) -> serde_json::Value {
    json!({
        "content": [
            {
                "type": "tool_use",
                "id": "toolu_adv_a",
                "name": name_a,
                "input": input_a,
            },
            {
                "type": "tool_use",
                "id": "toolu_adv_b",
                "name": name_b,
                "input": input_b,
            }
        ]
    })
}

// ---------------------------------------------------------------------------
// A1. Parser routes string-typed result through as CompleteTask
// ---------------------------------------------------------------------------

#[test]
fn parser_routes_string_complete_task_input_as_completetask() {
    // A bare string `result` is invalid under the consumer-side schema (must
    // be a JSON object) but the parser MUST let it through so the gate can
    // count it. The broader test covers this happy-path; this assertion
    // ensures the parser does NOT emit an Error event for the string shape.
    let body = claude_tool_response("complete_task", json!("just a string"));
    let (tx, rx) = mpsc::channel::<ApiEvent>();
    api::parse_response(&body, &tx);
    drop(tx);
    let events: Vec<ApiEvent> = rx.into_iter().collect();

    assert_eq!(events.len(), 2, "expected CompleteTask + Done; got {events:?}");
    match &events[0] {
        ApiEvent::CompleteTask { result, .. } => {
            assert_eq!(result, &json!("just a string"));
        }
        other => panic!("first event must be CompleteTask, got {other:?}"),
    }
    assert!(matches!(&events[1], ApiEvent::Done));

    // Defensive: no Error event in the stream.
    assert!(
        !events.iter().any(|e| matches!(e, ApiEvent::Error(_))),
        "parser must NOT emit Error for non-object complete_task input"
    );
}

// ---------------------------------------------------------------------------
// A2. Parser routes null-typed result through as CompleteTask
// ---------------------------------------------------------------------------

#[test]
fn parser_routes_null_complete_task_input_as_completetask() {
    let body = claude_tool_response("complete_task", json!(null));
    let (tx, rx) = mpsc::channel::<ApiEvent>();
    api::parse_response(&body, &tx);
    drop(tx);
    let events: Vec<ApiEvent> = rx.into_iter().collect();

    assert_eq!(events.len(), 2, "expected CompleteTask + Done; got {events:?}");
    match &events[0] {
        ApiEvent::CompleteTask { result, .. } => {
            assert!(result.is_null(), "result must round-trip as JSON null");
        }
        other => panic!("first event must be CompleteTask, got {other:?}"),
    }
    assert!(matches!(&events[1], ApiEvent::Done));
}

// ---------------------------------------------------------------------------
// A3. Parser routes array-typed result through as CompleteTask
// ---------------------------------------------------------------------------

#[test]
fn parser_routes_array_complete_task_input_as_completetask() {
    let body = claude_tool_response("complete_task", json!([1, 2, 3]));
    let (tx, rx) = mpsc::channel::<ApiEvent>();
    api::parse_response(&body, &tx);
    drop(tx);
    let events: Vec<ApiEvent> = rx.into_iter().collect();

    assert_eq!(events.len(), 2, "expected CompleteTask + Done; got {events:?}");
    match &events[0] {
        ApiEvent::CompleteTask { result, .. } => {
            assert_eq!(result, &json!([1, 2, 3]));
        }
        other => panic!("first event must be CompleteTask, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// A4. Two complete_task calls in same turn — parser emits both
// ---------------------------------------------------------------------------

#[test]
fn parser_emits_two_completetask_events_for_two_blocks_same_turn() {
    // A model that emits two complete_task tool_use blocks in one response
    // MUST be visible to the consumer as two events. The consumer (the
    // agent pane) is responsible for idempotency — first call wins, second
    // is logged but doesn't re-transition. This test pins the parser's
    // contract: emit both, in source order.
    let body = claude_two_tool_response(
        "complete_task",
        json!({"v": 1}),
        "complete_task",
        json!({"v": 2}),
    );
    let (tx, rx) = mpsc::channel::<ApiEvent>();
    api::parse_response(&body, &tx);
    drop(tx);
    let events: Vec<ApiEvent> = rx.into_iter().collect();

    // CompleteTask + CompleteTask + Done.
    assert_eq!(events.len(), 3, "expected two CompleteTask + Done; got {events:?}");

    let firsts: Vec<&ApiEvent> = events
        .iter()
        .filter(|e| matches!(e, ApiEvent::CompleteTask { .. }))
        .collect();
    assert_eq!(firsts.len(), 2, "exactly two CompleteTask events");
    if let (
        ApiEvent::CompleteTask { id: id_1, result: r_1 },
        ApiEvent::CompleteTask { id: id_2, result: r_2 },
    ) = (firsts[0], firsts[1])
    {
        // Source order preserved.
        assert_eq!(id_1, "toolu_adv_a");
        assert_eq!(id_2, "toolu_adv_b");
        assert_eq!(r_1, &json!({"v": 1}));
        assert_eq!(r_2, &json!({"v": 2}));
    } else {
        panic!("both filtered events must be CompleteTask");
    }
}

// ---------------------------------------------------------------------------
// A5. Agent first-call-wins semantics for complete_with_result
// ---------------------------------------------------------------------------

#[test]
fn agent_first_complete_with_result_call_captures_payload() {
    // Drive an agent into Working, call complete_with_result once. The agent
    // ends in Done with the supplied payload — verifies the spike's contract
    // under the first call.
    let mut agent = Agent::new(
        1,
        AgentTask::FreeForm { prompt: "task".into() },
    );
    agent.set_requires_complete_task(true);
    assert!(agent.approve_plan(), "Queued -> Working");
    assert_eq!(agent.status(), AgentStatus::Working);

    let first_result = json!({"summary": "first call"});
    agent.complete_with_result(first_result.clone());

    assert_eq!(agent.status(), AgentStatus::Done);
    assert_eq!(agent.completion_result(), Some(&first_result));
    assert!(
        agent.requires_complete_task(),
        "the requires_complete_task flag must persist after termination"
    );
}

// ---------------------------------------------------------------------------
// A6. Agent — second complete_with_result call leaves status terminal
// ---------------------------------------------------------------------------

#[test]
fn agent_second_complete_with_result_call_leaves_status_done() {
    // The pane-level consumer is responsible for idempotency (it breaks the
    // loop on the first CompleteTask event). But if the agent-FSM-level API
    // is called twice — e.g. by a manual retry path that mishandles the
    // event stream — the agent's terminal state must still be Done.
    //
    // Note: `complete_with_result` does NOT enforce idempotency at the
    // FSM-helper level; it overwrites `completion_result` and re-stamps
    // Done. This test pins the observable invariant: status stays terminal
    // (Done). If a future change makes the helper reject the second call,
    // both invariants below will still hold.
    let mut agent = Agent::new(
        1,
        AgentTask::FreeForm { prompt: "task".into() },
    );
    agent.set_requires_complete_task(true);
    assert!(agent.approve_plan(), "Queued -> Working");

    agent.complete_with_result(json!({"v": 1}));
    let first_status = agent.status();
    assert_eq!(first_status, AgentStatus::Done);

    // Second call — agent is already terminal.
    agent.complete_with_result(json!({"v": 2}));

    // Invariant 1: status remains terminal.
    assert!(
        agent.status().is_terminal(),
        "agent must remain in a terminal state after a second complete_with_result"
    );
    // Invariant 2: completion_result is populated with SOMETHING (either
    // the first or second payload, depending on idempotency policy). The
    // pane-level consumer is the source of truth for which call wins.
    assert!(
        agent.completion_result().is_some(),
        "completion_result must remain populated"
    );
}

// ---------------------------------------------------------------------------
// A7. AgentSpawnOpts — requires_complete_task flag round-trips false
// ---------------------------------------------------------------------------

#[test]
fn spawn_opts_requires_complete_task_explicit_false_does_not_drift() {
    // Defensive: an explicit `false` from a caller that wants to opt OUT
    // of the contract must NOT drift to `true` after passing through the
    // builder. This guards against accidental default-flips in future
    // refactors.
    let opts_false = AgentSpawnOpts::new(AgentTask::FreeForm { prompt: "x".into() })
        .with_requires_complete_task(false);
    assert!(!opts_false.requires_complete_task());

    // Default constructor: also false.
    let opts_default = AgentSpawnOpts::new(AgentTask::FreeForm { prompt: "x".into() });
    assert!(
        !opts_default.requires_complete_task(),
        "the default must be false to preserve legacy state-implicit termination"
    );

    // Explicit true → false: the most recent setter wins.
    let opts_toggled = AgentSpawnOpts::new(AgentTask::FreeForm { prompt: "x".into() })
        .with_requires_complete_task(true)
        .with_requires_complete_task(false);
    assert!(
        !opts_toggled.requires_complete_task(),
        "the most recent with_requires_complete_task call must win"
    );
}

// ---------------------------------------------------------------------------
// A8. End-to-end — parser handles invalid + valid in one response
// ---------------------------------------------------------------------------

#[test]
fn parser_emits_both_invalid_and_valid_completetask_events() {
    // A model that emits an invalid `complete_task` followed by a valid one
    // in the same response — the parser MUST emit both events in source
    // order so the consumer can choose its semantics (count invalid, then
    // accept valid; or vice versa).
    let body = claude_two_tool_response(
        "complete_task",
        json!("first call is bad"),
        "complete_task",
        json!({"summary": "second call is good"}),
    );
    let (tx, rx) = mpsc::channel::<ApiEvent>();
    api::parse_response(&body, &tx);
    drop(tx);
    let events: Vec<ApiEvent> = rx.into_iter().collect();

    // Both CompleteTask events appear, in source order, followed by Done.
    assert_eq!(events.len(), 3, "expected two CompleteTask + Done; got {events:?}");
    let cts: Vec<&ApiEvent> = events
        .iter()
        .filter(|e| matches!(e, ApiEvent::CompleteTask { .. }))
        .collect();
    assert_eq!(cts.len(), 2);
    if let (
        ApiEvent::CompleteTask { result: bad, .. },
        ApiEvent::CompleteTask { result: good, .. },
    ) = (cts[0], cts[1])
    {
        // Invalid event surfaces the malformed payload unchanged.
        assert!(!bad.is_object(), "first event carries the non-object payload");
        // Valid event surfaces the object payload.
        assert!(good.is_object(), "second event carries the object payload");
    } else {
        panic!("filtered events were not CompleteTask");
    }
}

// ---------------------------------------------------------------------------
// A9. Agent message after complete — pane is responsible for break
// ---------------------------------------------------------------------------

#[test]
fn agent_complete_with_result_then_subsequent_messages_does_not_revive() {
    // Once `complete_with_result` runs, the FSM is terminal. Any caller that
    // then tries to push more messages or invoke FSM transitions on the
    // agent must NOT see the agent flip back to a non-terminal status
    // through normal helpers. The pane's responsibility is to break the
    // loop on `CompleteTask`; this test verifies the agent FSM cooperates
    // even if the loop is buggy.
    let mut agent = Agent::new(
        1,
        AgentTask::FreeForm { prompt: "task".into() },
    );
    agent.set_requires_complete_task(true);
    assert!(agent.approve_plan(), "Queued -> Working");

    agent.complete_with_result(json!({"summary": "done"}));
    assert_eq!(agent.status(), AgentStatus::Done);

    // Helper transitions FROM Done are not in the FSM table — verify by
    // attempting and observing the status does not change.
    assert!(
        !agent.approve_plan(),
        "approve_plan from Done must reject (Done is terminal until retry)"
    );
    assert_eq!(agent.status(), AgentStatus::Done);

    assert!(
        !agent.begin_planning(),
        "begin_planning from Done must reject (Done is terminal until retry)"
    );
    assert_eq!(agent.status(), AgentStatus::Done);
}

// ---------------------------------------------------------------------------
// A10. End-to-end — agent stays Done after parser emits invalid then valid
// ---------------------------------------------------------------------------

#[test]
fn agent_end_to_end_first_valid_complete_task_wins() {
    // Drive the parser → agent path. Stream is one valid CompleteTask.
    // The agent loop calls complete_with_result on the first valid
    // CompleteTask event. Subsequent events in the stream do NOT revive
    // the agent.
    let body = claude_two_tool_response(
        "complete_task",
        json!({"summary": "first"}),
        "complete_task",
        json!({"summary": "second"}),
    );
    let (tx, rx) = mpsc::channel::<ApiEvent>();
    let mut agent = Agent::new(
        7,
        AgentTask::FreeForm { prompt: "e2e".into() },
    );
    agent.set_requires_complete_task(true);
    assert!(agent.approve_plan());

    api::parse_response(&body, &tx);
    drop(tx);

    // Mirror the pane's first-call-wins semantics: break out after the
    // first CompleteTask is processed.
    let mut processed = 0u32;
    for event in rx.iter() {
        match event {
            ApiEvent::CompleteTask { result, .. } => {
                if processed == 0 {
                    agent.complete_with_result(result);
                    processed += 1;
                    break;
                }
            }
            ApiEvent::Done => break,
            ApiEvent::TextDelta(_) | ApiEvent::ToolUse { .. } => {
                panic!("unexpected non-lifecycle event in e2e: {event:?}");
            }
            ApiEvent::Error(e) => panic!("parser emitted Error: {e}"),
        }
    }

    assert_eq!(processed, 1, "the loop must accept exactly one CompleteTask");
    assert_eq!(agent.status(), AgentStatus::Done);
    assert_eq!(
        agent.completion_result(),
        Some(&json!({"summary": "first"})),
        "first valid CompleteTask must win the payload race",
    );
}

// ---------------------------------------------------------------------------
// A11. Parser — unknown name vs complete_task name disambiguation
// ---------------------------------------------------------------------------

#[test]
fn parser_does_not_confuse_complete_task_with_lookalike_names() {
    // The parser short-circuits on the literal name `"complete_task"`. A
    // close-but-not-equal name MUST fall through to the normal path and
    // emit Error (unknown tool) since it isn't in ToolType.
    for bad_name in &["Complete_Task", "complete_task ", "complete-task", "completed_task"] {
        let body = claude_tool_response(bad_name, json!({"v": 1}));
        let (tx, rx) = mpsc::channel::<ApiEvent>();
        api::parse_response(&body, &tx);
        drop(tx);
        let events: Vec<ApiEvent> = rx.into_iter().collect();

        // Must NOT emit CompleteTask.
        assert!(
            !events.iter().any(|e| matches!(e, ApiEvent::CompleteTask { .. })),
            "name {bad_name:?} must NOT route through the lifecycle filter"
        );
        // Must emit an Error (the name is not in ToolType nor lifecycle).
        assert!(
            events.iter().any(|e| matches!(e, ApiEvent::Error(_))),
            "name {bad_name:?} must surface as an unknown-tool Error"
        );
    }
}

// ---------------------------------------------------------------------------
// A12. Parser — complex nested object payload round-trips
// ---------------------------------------------------------------------------

#[test]
fn parser_round_trips_deeply_nested_complete_task_payload() {
    // Defensive: a complex nested JSON object as the result must round-trip
    // through the parser unchanged, including nested arrays, mixed types,
    // and unicode strings.
    let payload = json!({
        "summary": "工作完成 🎉",
        "artifacts": ["a.rs", "b.rs"],
        "metrics": {
            "tokens_in": 12345,
            "tokens_out": 678,
            "tool_calls": 9,
            "errors": []
        },
        "next_steps": null,
        "notes": [
            {"kind": "info", "body": "all green"},
            {"kind": "warn", "body": "consider lints"}
        ]
    });
    let body = claude_tool_response("complete_task", payload.clone());
    let (tx, rx) = mpsc::channel::<ApiEvent>();
    api::parse_response(&body, &tx);
    drop(tx);
    let events: Vec<ApiEvent> = rx.into_iter().collect();

    assert_eq!(events.len(), 2);
    match &events[0] {
        ApiEvent::CompleteTask { result, .. } => {
            assert_eq!(result, &payload, "complex payload must round-trip byte-for-byte");
        }
        other => panic!("expected CompleteTask, got {other:?}"),
    }
}
