//! Issue #646 — broader `complete_task` wiring.
//!
//! Builds on the spike (`complete_task_spike.rs`) which proved the parser
//! short-circuit, the `Agent::complete_with_result` lifecycle path, and the
//! `AgentSpawnOpts::with_requires_complete_task` builder. This file exercises
//! the broader-implementation surface the spike was explicit about deferring:
//!
//! - `tools::lifecycle_tools()` exposes the LLM-facing `complete_task` /
//!   `abort_task` `ToolDefinition`s so the agent pane can extend its manifest
//!   when the agent opted in.
//! - The Claude parser (`api::parse_response`) lets non-object
//!   `complete_task` inputs through to `ApiEvent::CompleteTask` so the
//!   consumer-side validation gate (in `phantom-app`) can count them toward
//!   the 3-strike `validation_failure_count` flatline. The file/git tool
//!   path keeps the strict non-object guard.
//!
//! Agent-pane-side behaviour (the 3-strike flatline itself and the
//! `MAX_TOOL_ROUNDS → "exited without complete_task"` reason rewrite) is
//! covered by unit tests in `crates/phantom-app/src/agent_pane/tests.rs`
//! because those depend on `AgentPane`, which is `pub(crate)` and not
//! reachable across crate boundaries.

use std::sync::mpsc;

use phantom_agents::api::{self, ApiEvent};
use phantom_agents::tools::lifecycle_tools;

use serde_json::json;

fn claude_tool_response(name: &str, input: serde_json::Value) -> serde_json::Value {
    json!({
        "content": [
            {
                "type": "tool_use",
                "id": "toolu_broader_test_001",
                "name": name,
                "input": input,
            }
        ]
    })
}

#[test]
fn lifecycle_tools_returns_complete_task_and_abort_task() {
    let tools = lifecycle_tools();

    // Exactly two lifecycle definitions: complete_task + abort_task.
    assert_eq!(
        tools.len(),
        2,
        "lifecycle_tools() must return exactly [complete_task, abort_task]; got {tools:?}"
    );
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    assert!(
        names.contains(&"complete_task"),
        "lifecycle_tools() must include complete_task; got {names:?}"
    );
    assert!(
        names.contains(&"abort_task"),
        "lifecycle_tools() must include abort_task; got {names:?}"
    );

    // Each definition must carry an `object`-typed JSON schema.
    for tool in &tools {
        let schema_type = tool
            .parameters
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        assert_eq!(
            schema_type, "object",
            "tool {} must declare an object schema; got {}",
            tool.name, tool.parameters
        );
    }
}

#[test]
fn lifecycle_tools_disjoint_from_available_tools() {
    use phantom_agents::tools::available_tools;

    let file_tool_names: Vec<String> = available_tools()
        .iter()
        .map(|t| t.name.clone())
        .collect();
    let lifecycle_names: Vec<String> = lifecycle_tools()
        .iter()
        .map(|t| t.name.clone())
        .collect();

    for ln in &lifecycle_names {
        assert!(
            !file_tool_names.contains(ln),
            "lifecycle tool {ln:?} must NOT collide with a file/git tool name in available_tools(); \
             collision would cause the parser to misroute the lifecycle signal through \
             ToolType::from_api_name. file_tool_names = {file_tool_names:?}",
        );
    }
}

#[test]
fn parser_passes_non_object_complete_task_input_through_as_completetask() {
    // The broader implementation relaxes the strict non-object guard
    // specifically for `complete_task` so the consumer-side validation gate
    // can count schema-invalid calls. A string, null, or array `input` must
    // emit `ApiEvent::CompleteTask` (with the malformed payload preserved
    // for the consumer to inspect) rather than `ApiEvent::Error`.
    let bad_inputs = vec![
        json!("not an object"),
        json!(null),
        json!([1, 2, 3]),
        json!(42),
    ];

    for input in bad_inputs {
        let body = claude_tool_response("complete_task", input.clone());
        let (tx, rx) = mpsc::channel::<ApiEvent>();
        api::parse_response(&body, &tx);
        drop(tx);
        let events: Vec<ApiEvent> = rx.into_iter().collect();

        assert_eq!(
            events.len(),
            2,
            "non-object complete_task must still emit CompleteTask + Done; got {events:?} for input {input}",
        );
        assert!(
            matches!(&events[0], ApiEvent::CompleteTask { result, .. } if result == &input),
            "non-object input must round-trip through CompleteTask unchanged; got {:?} for input {input}",
            events[0],
        );
        assert!(
            matches!(&events[1], ApiEvent::Done),
            "second event must be Done; got {:?}",
            events[1]
        );
    }
}

#[test]
fn parser_preserves_strict_non_object_guard_for_file_tools() {
    // The relaxation must ONLY apply to `complete_task`. File/git tools that
    // emit a non-object input must still produce an Error event, because they
    // have no consumer-side validation gate and the runtime expects an
    // `Object` shape downstream.
    let body = claude_tool_response("read_file", json!("just a string"));
    let (tx, rx) = mpsc::channel::<ApiEvent>();
    api::parse_response(&body, &tx);
    drop(tx);
    let events: Vec<ApiEvent> = rx.into_iter().collect();

    assert!(
        events.iter().any(|e| matches!(e, ApiEvent::Error(_))),
        "non-object read_file input must still produce an Error event; got {events:?}",
    );
    assert!(
        !events.iter().any(|e| matches!(e, ApiEvent::ToolUse { .. })),
        "non-object read_file input must NOT produce a ToolUse event; got {events:?}",
    );
}

#[test]
fn parser_routes_valid_complete_task_through_as_completetask() {
    // The relaxation must not regress the spike's valid-object path. An
    // object `complete_task` input must still emit `CompleteTask + Done`.
    let body = claude_tool_response(
        "complete_task",
        json!({"summary": "did the thing", "artifacts": ["a.rs"]}),
    );
    let (tx, rx) = mpsc::channel::<ApiEvent>();
    api::parse_response(&body, &tx);
    drop(tx);
    let events: Vec<ApiEvent> = rx.into_iter().collect();

    assert_eq!(events.len(), 2, "expected CompleteTask + Done; got {events:?}");
    match &events[0] {
        ApiEvent::CompleteTask { result, .. } => {
            assert_eq!(result.get("summary").and_then(|v| v.as_str()), Some("did the thing"));
        }
        other => panic!("first event must be CompleteTask; got {other:?}"),
    }
    assert!(matches!(&events[1], ApiEvent::Done));
}
