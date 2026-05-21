//! End-to-end integration test for the substrate driver wired into a real
//! [`phantom_loop::LoopRunner`].
//!
//! This is the regression-net for the structural blocker that
//! [`phantom_loop::SubstrateDriver`] solves: when the driver is wired into
//! a runner via a [`phantom_loop::SubstrateAgentDispatcher`] +
//! [`phantom_loop::SubstrateCompletionRouter`], `phantom loop run` can
//! actually drain its spawn queue and complete iterations without booting
//! the full GUI app.
//!
//! The driver is exercised with a [`phantom_loop::MockSubstrateBackend`]
//! so the test does not hit Claude. The mock returns a canned schema-valid
//! payload — exactly the contract real agents are expected to honor via
//! `complete_task`. Every component between the dispatcher and the router
//! is real: the runner FSM, the spawn queue, the driver tick loop, the
//! tokio mpsc event bus, and the completion router. The only thing we mock
//! is the chat backend.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use phantom_agents::composer_tools::new_spawn_subagent_queue;
use phantom_loop::{
    CorrelationId, LoopContext, LoopInput, LoopPullResult, LoopQueueRegistry, LoopRunner,
    LoopSource, MockSubstrateBackend, SubstrateAgentDispatcher, SubstrateBackend, SubstrateDriver,
    parse_spec_str,
};
use phantom_protocol::Event;
use serde_json::json;

/// Stub source: emits one input, then `Done`. Mirrors the helper in
/// `tests/end_to_end_loop.rs` but kept local so the e2e test does not
/// depend on cross-file fixture wiring.
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

/// Minimal reviewer-style spec: one agent, integer `pr_number`, decision
/// enum, one effect that pushes onto a downstream queue. Used in both the
/// success and failure paths below.
const REVIEWER_TOML: &str = r#"
id = "e2e-driver-reviewer"

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

/// Happy path: one iteration round-trips from a real `LoopRunner` through
/// the dispatcher → driver → mock backend → event bus → completion
/// router. The downstream queue receives the field-mapped message.
#[tokio::test]
async fn driver_round_trips_real_runner_to_mock_backend() {
    let (spec, schema) = parse_spec_str(REVIEWER_TOML).expect("parse e2e spec");

    let pulls = Arc::new(AtomicUsize::new(0));
    let queues = Arc::new(LoopQueueRegistry::new());

    let source = Box::new(OneShotSource {
        input: Some(LoopInput {
            key: "pr#7".to_string(),
            payload: json!({"pr_number": 7}),
            correlation_id: CorrelationId::new("e2e-driver-corr"),
        }),
        pull_count: pulls.clone(),
    });

    // Real dispatcher against a real spawn queue.
    let spawn_queue = new_spawn_subagent_queue();
    let dispatcher = Arc::new(SubstrateAgentDispatcher::with_default_parent(
        spawn_queue.clone(),
    ));

    // Real driver with a mock backend. The backend returns a canned
    // schema-valid payload for every spawned request, simulating an agent
    // that called `complete_task({"pr_number": 7, "decision": "approved"})`.
    let canned = json!({"pr_number": 7, "decision": "approved"});
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(16);
    let backend: Arc<dyn SubstrateBackend> = Arc::new(MockSubstrateBackend::ok(canned.clone()));
    let driver = SubstrateDriver::new(spawn_queue.clone(), backend, tx)
        .with_tick_interval(std::time::Duration::from_millis(10));
    let _driver_handle = driver.run();

    // Forwarder: pipe events into the completion router.
    let router = dispatcher.completion_router();
    let router_clone = router.clone();
    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            router_clone.on_completion(event);
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

    assert!(
        pulls.load(Ordering::SeqCst) >= 2,
        "expected ≥2 pulls, got {}",
        pulls.load(Ordering::SeqCst)
    );

    let downstream = queues.get_or_create("downstream");
    assert_eq!(downstream.len(), 1);
    let msg = downstream.pop().expect("downstream message");
    assert_eq!(msg.from_loop, "e2e-driver-reviewer");
    assert_eq!(msg.payload["reviewed_pr"], 7);

    assert_eq!(dispatcher.pending_count(), 0);
}

/// Failure path: the backend returns an error. The driver synthesises a
/// failed `AgentTaskComplete` event; the runner stops with a
/// dispatch-failure reason carrying the backend's error message.
#[tokio::test]
async fn driver_propagates_backend_error_to_runner_stop_reason() {
    let (spec, schema) = parse_spec_str(REVIEWER_TOML).expect("parse e2e spec");
    let queues = Arc::new(LoopQueueRegistry::new());
    let pulls = Arc::new(AtomicUsize::new(0));

    let source = Box::new(OneShotSource {
        input: Some(LoopInput {
            key: "pr#8".to_string(),
            payload: json!({"pr_number": 8}),
            correlation_id: CorrelationId::new("e2e-driver-err"),
        }),
        pull_count: pulls.clone(),
    });

    let spawn_queue = new_spawn_subagent_queue();
    let dispatcher = Arc::new(SubstrateAgentDispatcher::with_default_parent(
        spawn_queue.clone(),
    ));

    let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(16);
    let backend: Arc<dyn SubstrateBackend> =
        Arc::new(MockSubstrateBackend::err("synthetic-backend-failure"));
    let driver = SubstrateDriver::new(spawn_queue.clone(), backend, tx)
        .with_tick_interval(std::time::Duration::from_millis(10));
    let _driver_handle = driver.run();

    let router = dispatcher.completion_router();
    let router_clone = router.clone();
    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            router_clone.on_completion(event);
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
    assert!(
        reason.contains("synthetic-backend-failure"),
        "expected backend error to be threaded through, got `{reason}`"
    );
}
