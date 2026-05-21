//! End-to-end integration test for the C3 substrate dispatcher.
//!
//! Wires the real [`phantom_loop::SubstrateAgentDispatcher`] (against a
//! stub spawn-queue substrate) to a real [`phantom_loop::LoopRunner`] with
//! a one-shot stub source, and verifies the full happy path:
//!
//! 1. Runner asks dispatcher to `dispatch`.
//! 2. Dispatcher pushes a [`phantom_agents::composer_tools::SpawnSubagentRequest`]
//!    onto the spawn queue and returns a `DispatchHandle` with a pending
//!    `completion_rx`.
//! 3. A test-driven completion router fires
//!    [`phantom_protocol::Event::AgentTaskComplete`] back at the
//!    dispatcher's router, which fulfils the oneshot.
//! 4. Runner validates the result against the spec's `ExitSchema`.
//! 5. Effects fire and a downstream queue message lands.
//! 6. Runner stops cleanly with the "source exhausted" reason.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use phantom_agents::composer_tools::new_spawn_subagent_queue;
use phantom_loop::{
    CorrelationId, LoopContext, LoopInput, LoopMessageQueueSource, LoopPullResult,
    LoopQueueRegistry, LoopRunner, LoopSource, SubstrateAgentDispatcher, parse_spec_str,
};
use phantom_protocol::Event;
use serde_json::json;

/// Stub source: emits one input, then `Done`.
struct OneShotSource {
    input: Option<LoopInput>,
    pull_count: Arc<AtomicUsize>,
}

impl LoopSource for OneShotSource {
    fn next(&mut self, _ctx: &LoopContext) -> LoopPullResult {
        self.pull_count.fetch_add(1, Ordering::SeqCst);
        match self.input.take() {
            Some(i) => LoopPullResult::Available(i),
            None => LoopPullResult::Done,
        }
    }
}

const REVIEWER_TOML: &str = r#"
id = "e2e-reviewer"

[agent]
role = "Actor"
system_prompt = "Review the PR."

[agent.exit_schema]
type = "object"
required = ["pr_number", "decision"]

[agent.exit_schema.properties.pr_number]
type = "integer"

[agent.exit_schema.properties.decision]
enum = ["approved", "rejected", "needs_changes"]

[source]
kind = "queue"
name = "review-queue"

[[on_complete]]
kind = "enqueue_to"
queue = "downstream"

[[on_complete.fields]]
from = "pr_number"
to = "reviewed_pr"
"#;

/// Runs one full iteration of a loop end-to-end: stub source → real
/// SubstrateAgentDispatcher → router-fed AgentTaskComplete → schema
/// validation → effect fires → runner stops.
#[tokio::test]
async fn one_iteration_round_trips_through_substrate_dispatcher_and_router() {
    let (spec, schema) = parse_spec_str(REVIEWER_TOML).expect("parse e2e spec");

    let pulls = Arc::new(AtomicUsize::new(0));
    let queues = Arc::new(LoopQueueRegistry::new());

    let source = Box::new(OneShotSource {
        input: Some(LoopInput {
            key: "pr#42".to_string(),
            payload: json!({"pr_number": 42}),
            correlation_id: CorrelationId::new("e2e-corr-1"),
        }),
        pull_count: pulls.clone(),
    });

    // Real SubstrateAgentDispatcher against a stub spawn queue (we never
    // drain it — we directly drive the router instead).
    let spawn_queue = new_spawn_subagent_queue();
    let dispatcher = Arc::new(SubstrateAgentDispatcher::with_default_parent(
        spawn_queue.clone(),
    ));
    let router = dispatcher.completion_router();

    // Background task: watch the spawn queue, and whenever a request
    // appears, fire a matching `AgentTaskComplete` event at the router.
    //
    // This is the "stub substrate" — it replaces what App::update +
    // App::spawn_agent_pane_with_opts + AgentPane FSM + AgentAdapter would
    // do in production.
    let router_clone = router.clone();
    let spawn_queue_clone = spawn_queue.clone();
    tokio::spawn(async move {
        loop {
            let popped = {
                spawn_queue_clone
                    .lock()
                    .ok()
                    .and_then(|mut q| q.pop_front())
            };
            if let Some(req) = popped {
                // Simulate: the agent ran, called complete_task with a
                // valid result, and the substrate emitted the protocol
                // event.
                router_clone.on_completion(Event::AgentTaskComplete {
                    agent_id: req.assigned_id,
                    success: true,
                    summary: "ok".to_string(),
                    spawn_tag: None,
                    result: Some(json!({"pr_number": 42, "decision": "approved"})),
                });
            } else {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        }
    });

    let runner = LoopRunner::new(
        Arc::new(spec),
        schema,
        source,
        queues.clone(),
        dispatcher.clone(),
    );

    let reason = runner.run().await;
    assert_eq!(reason, "source exhausted");

    // Source must have been pulled at least twice (initial + Done).
    assert!(
        pulls.load(Ordering::SeqCst) >= 2,
        "expected ≥2 pulls, got {}",
        pulls.load(Ordering::SeqCst)
    );

    // The downstream queue must have one message with the field map applied.
    let downstream = queues.get_or_create("downstream");
    assert_eq!(downstream.len(), 1);
    let msg = downstream.pop().expect("message");
    assert_eq!(msg.from_loop, "e2e-reviewer");
    assert_eq!(msg.payload["reviewed_pr"], 42);

    // After the round trip, no pending dispatches must remain on the
    // dispatcher — the router cleaned up.
    assert_eq!(dispatcher.pending_count(), 0);
}

/// Failure-path mirror: the substrate reports `success: false` and the
/// runner stops with a dispatch-failure reason.
#[tokio::test]
async fn substrate_failure_event_stops_runner_with_dispatch_failure_reason() {
    let (spec, schema) = parse_spec_str(REVIEWER_TOML).expect("parse e2e spec");
    let queues = Arc::new(LoopQueueRegistry::new());

    queues.push(
        "review-queue",
        phantom_loop::LoopMessage::new("pr-finder", json!({"key": "pr#1"})),
    );
    let source = Box::new(LoopMessageQueueSource::new(&queues, "review-queue"));

    let spawn_queue = new_spawn_subagent_queue();
    let dispatcher = Arc::new(SubstrateAgentDispatcher::with_default_parent(
        spawn_queue.clone(),
    ));
    let router = dispatcher.completion_router();

    let router_clone = router.clone();
    let spawn_queue_clone = spawn_queue.clone();
    tokio::spawn(async move {
        loop {
            let popped = {
                spawn_queue_clone
                    .lock()
                    .ok()
                    .and_then(|mut q| q.pop_front())
            };
            if let Some(req) = popped {
                router_clone.on_completion(Event::AgentTaskComplete {
                    agent_id: req.assigned_id,
                    success: false,
                    summary: "agent panicked".to_string(),
                    spawn_tag: None,
                    result: None,
                });
            } else {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        }
    });

    let runner = LoopRunner::new(
        Arc::new(spec),
        schema,
        source,
        queues.clone(),
        dispatcher,
    );

    let reason = runner.run().await;
    assert!(
        reason.starts_with("dispatch failure:"),
        "expected dispatch-failure reason, got `{reason}`"
    );
    assert!(reason.contains("agent panicked"));
}
