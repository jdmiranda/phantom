//! Issue #646 — `complete_task` delivery-path spike.
//!
//! This test grounds the three architectural design choices the broader
//! issue (#646) is built on, against real code:
//!
//! 1. **Parser short-circuit**: `complete_task` must be diverted ahead of
//!    `ToolType::from_api_name`, otherwise it falls through the
//!    `unknown tool in response` arm at `crates/phantom-agents/src/api.rs`
//!    and never reaches the agent loop. The spike chooses **option (b)** —
//!    a pre-parser lifecycle filter that emits a dedicated
//!    [`ApiEvent::CompleteTask`] variant. This keeps the 8-variant
//!    `ToolType` enum focused on file/git tools (which all return
//!    `ToolResult`) and gives the agent loop a single, ergonomic match arm
//!    for a non-tool event.
//!
//! 2. **`requires_complete_task` storage**: The agent must remember whether
//!    it was spawned with the lifecycle requirement. The spike chooses
//!    **option (a)** — a `requires_complete_task: bool` field on `Agent`
//!    set at construction from `AgentSpawnOpts::with_requires_complete_task`.
//!    Storing the flag (rather than the whole `AgentSpawnOpts`) keeps the
//!    `Agent` struct's surface focused on lifecycle and avoids carrying
//!    spawn-time metadata (`chat_model`, `spawn_tag`, …) past the point
//!    they are needed.
//!
//! 3. **`AgentTaskComplete.result` extension**: The typed result must reach
//!    consumers (capture sidecar, parent-agent `wait_for_agent`, brain
//!    reconciler). The spike chooses **option (a)** — a new
//!    `result: Option<serde_json::Value>` field on
//!    `phantom_protocol::Event::AgentTaskComplete`, with the exhaustive
//!    destructure at `update.rs` widened. A separate event keyed on
//!    `agent_id` (option b) would create an ordering window between the
//!    completion and its payload; a registry lookup (option c) introduces
//!    async / lock-poisoning failure modes the existing event bus does
//!    not have. Carrying the payload on the same variant is the
//!    least-invasive shape.
//!
//! ## What the test exercises
//!
//! - The Claude-side parser at `parse_response` lifts a synthesised
//!   `complete_task` `tool_use` block into [`ApiEvent::CompleteTask`].
//! - An [`Agent`] driven through [`Agent::complete_with_result`] reaches
//!   [`AgentStatus::Done`] with the typed result captured.
//! - The opt-in `requires_complete_task` flag round-trips through
//!   `AgentSpawnOpts::with_requires_complete_task`.
//!
//! ## Out of scope (deliberately)
//!
//! Schema validation, `validation_failure_count`, the `MAX_TOOL_ROUNDS →
//! Failed` enforcement, and `abort_task` are out of scope and left as
//! TODOs in the production callsites the broader implementation will
//! extend (see PR description for the list).

use std::sync::mpsc;

use phantom_agents::agent::{Agent, AgentSpawnOpts, AgentStatus, AgentTask};
use phantom_agents::api::{self, ApiEvent};

use serde_json::json;

/// Synthesise the JSON shape Claude returns for a single `complete_task`
/// `tool_use` block. Mirrors the live API contract documented at
/// `crates/phantom-agents/src/api.rs` `parse_response`.
fn claude_complete_task_response(result: serde_json::Value) -> serde_json::Value {
    json!({
        "content": [
            {
                "type": "tool_use",
                "id": "toolu_complete_task_001",
                "name": "complete_task",
                "input": result,
            }
        ]
    })
}

#[test]
fn parser_diverts_complete_task_to_dedicated_event() {
    // ---- ARRANGE ---------------------------------------------------------
    let body = claude_complete_task_response(json!({"foo": "bar"}));
    let (tx, rx) = mpsc::channel::<ApiEvent>();

    // ---- ACT -------------------------------------------------------------
    api::parse_response(&body, &tx);
    drop(tx);
    let events: Vec<ApiEvent> = rx.into_iter().collect();

    // ---- ASSERT ----------------------------------------------------------
    //
    // Expectation: exactly one `CompleteTask` event with the typed result,
    // followed by `Done`. No `ToolUse` (which would mean the lifecycle name
    // got routed through the file/git tool path), no `Error` (which would
    // mean the name fell through to `unknown tool in response`).
    assert_eq!(events.len(), 2, "parser must emit CompleteTask + Done; got {events:?}");

    let captured_result = match &events[0] {
        ApiEvent::CompleteTask { id, result } => {
            assert_eq!(id, "toolu_complete_task_001");
            result.clone()
        }
        other => panic!("first event must be CompleteTask, got {other:?}"),
    };
    assert_eq!(
        captured_result,
        json!({"foo": "bar"}),
        "complete_task result payload must round-trip through the parser unchanged",
    );
    assert!(
        matches!(&events[1], ApiEvent::Done),
        "parser must emit Done after CompleteTask; got {:?}",
        events[1]
    );
}

#[test]
fn agent_complete_with_result_transitions_to_done_and_captures_result() {
    // ---- ARRANGE ---------------------------------------------------------
    let mut agent = Agent::new(
        42,
        AgentTask::FreeForm { prompt: "complete-task spike".into() },
    );
    agent.set_requires_complete_task(true);
    // Drive into Working so the FSM transition to Done is valid (Queued →
    // Working is a no-gate fast-path on `AgentStatus`).
    assert!(agent.approve_plan(), "Queued → Working transition must succeed");
    assert_eq!(agent.status(), AgentStatus::Working);

    let result = json!({"foo": "bar"});

    // ---- ACT -------------------------------------------------------------
    agent.complete_with_result(result.clone());

    // ---- ASSERT ----------------------------------------------------------
    assert_eq!(
        agent.status(),
        AgentStatus::Done,
        "complete_with_result must transition the agent to Done",
    );
    assert!(
        agent.requires_complete_task(),
        "the requires_complete_task flag must remain set after termination",
    );
    assert_eq!(
        agent.completion_result(),
        Some(&result),
        "the typed result payload must be retrievable after termination",
    );
}

#[test]
fn spawn_opts_carry_requires_complete_task_flag() {
    // ---- ARRANGE / ACT ---------------------------------------------------
    let opts_default = AgentSpawnOpts::new(AgentTask::FreeForm { prompt: "x".into() });
    let opts_required = AgentSpawnOpts::new(AgentTask::FreeForm { prompt: "x".into() })
        .with_requires_complete_task(true);
    let opts_explicit_off = AgentSpawnOpts::new(AgentTask::FreeForm { prompt: "x".into() })
        .with_requires_complete_task(false);

    // ---- ASSERT ----------------------------------------------------------
    assert!(
        !opts_default.requires_complete_task(),
        "AgentSpawnOpts::new must default `requires_complete_task` to false to preserve \
         legacy state-implicit termination",
    );
    assert!(
        opts_required.requires_complete_task(),
        "with_requires_complete_task(true) must set the field",
    );
    assert!(
        !opts_explicit_off.requires_complete_task(),
        "with_requires_complete_task(false) must keep the field unset",
    );
}

#[test]
fn end_to_end_parser_to_agent_done() {
    // The integration the issue actually asks for: stub LLM emits
    // `complete_task({"foo":"bar"})`, agent reaches `AgentStatus::Done` with
    // the typed result captured.

    // ---- ARRANGE ---------------------------------------------------------
    let body = claude_complete_task_response(json!({"foo": "bar"}));
    let (tx, rx) = mpsc::channel::<ApiEvent>();

    let opts = AgentSpawnOpts::new(AgentTask::FreeForm {
        prompt: "complete-task spike e2e".into(),
    })
    .with_requires_complete_task(true);
    let mut agent = Agent::new(7, opts.task.clone());
    agent.set_requires_complete_task(opts.requires_complete_task());
    assert!(agent.approve_plan(), "Queued → Working transition must succeed");

    // ---- ACT -------------------------------------------------------------
    api::parse_response(&body, &tx);
    drop(tx);

    // The agent loop in production is `crates/phantom-app/src/agent_pane/mod.rs`;
    // the spike inlines the minimal handling here so the test stays in
    // `phantom-agents` and the wiring is fully exercised end-to-end without
    // pulling in the full pane / GPU stack.
    for event in rx.iter() {
        match event {
            ApiEvent::CompleteTask { result, .. } => {
                agent.complete_with_result(result);
            }
            ApiEvent::Done => break,
            ApiEvent::TextDelta(_) | ApiEvent::ToolUse { .. } => {
                panic!("unexpected non-lifecycle event in spike: {event:?}");
            }
            ApiEvent::Error(e) => panic!("parser emitted Error: {e}"),
        }
    }

    // ---- ASSERT ----------------------------------------------------------
    assert_eq!(agent.status(), AgentStatus::Done);
    assert_eq!(agent.completion_result(), Some(&json!({"foo": "bar"})));
}
