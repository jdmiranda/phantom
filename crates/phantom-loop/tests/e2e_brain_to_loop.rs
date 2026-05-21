//! End-to-end integration test for the brain → loop bridge.
//!
//! This is the regression-net for the phantom-on-phantom self-improvement
//! loop closed by [`phantom_loop::LoopQueueActionHandler`]. Without the
//! bridge, `AiAction::EnqueueLoopMessage` evaporates inside the trait's
//! default no-op `enqueue_loop_message`; with it, the action lands on a
//! `LoopQueueRegistry` and the implementer-queue consumer loop picks it up.
//!
//! # Topology under test
//!
//! ```text
//!   StubGhRunner (canned `gh issue list` JSON)
//!       │
//!       ▼
//!   GhIssueGoalSource.poll  → GoalCandidate
//!       │
//!       ▼
//!   SelfImprovementState.evaluate  → AiAction::EnqueueLoopMessage
//!       │
//!       ▼
//!   LoopQueueActionHandler.enqueue_loop_message
//!       │  registry.push("implementer-queue", LoopMessage::new(…))
//!       ▼
//!   LoopQueueRegistry  (named "implementer-queue")
//!       │
//!       ▼
//!   LoopMessageQueueSource.next  → LoopInput
//!       │
//!       ▼
//!   LoopRunner.dispatch  (implementer-loop spec)
//!       │
//!       ▼
//!   SubstrateAgentDispatcher  → spawn_queue push
//!       │
//!       ▼
//!   SubstrateDriver.tick  → MockSubstrateBackend.run_agent
//!       │
//!       ▼
//!   Event::AgentTaskComplete  → SubstrateCompletionRouter
//!       │
//!       ▼
//!   LoopRunner validates + Stopped
//! ```
//!
//! # Why not spawn the real brain thread
//!
//! The brain's self-improvement tick is hardcoded to a 60 s interval inside
//! `brain_loop`. Spinning the OS thread up just to wait a minute is not
//! useful in a test, and `SelfImprovementState::tick` is the same function
//! the brain calls anyway — exercising it directly is byte-for-byte the
//! same logic with a deterministic clock.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use phantom_agents::composer_tools::new_spawn_subagent_queue;
use phantom_brain::events::AiAction;
use phantom_brain::goal_source::{GhIssueGoalSource, GoalSource, StubGhRunner};
use phantom_brain::self_improvement::{
    DEFAULT_IMPLEMENTER_QUEUE, SelfImprovementConfig, SelfImprovementState,
};
use phantom_loop::{
    LoopContext, LoopPullResult, LoopQueueActionHandler, LoopQueueRegistry, LoopRunner, LoopSource,
    LoopMessageQueueSource, MockSubstrateBackend, SubstrateAgentDispatcher, SubstrateBackend,
    SubstrateDriver, parse_spec_str,
};
use phantom_protocol::Event;
use serde_json::json;

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// Canned `gh issue list` response containing exactly one issue that scores
/// above the auto-enqueue threshold. Mirrors the §8.3 fixture from the
/// brain's self_improvement_integration test — the `priority:critical` +
/// `good-first-issue` combination is the cleanest path to a passing score.
fn critical_issue_fixture() -> String {
    r#"[
        {
            "number": 999,
            "title": "Phantom-on-phantom bridge test",
            "body": "Repro: send AiAction::EnqueueLoopMessage and observe the queue.",
            "labels": [{"name":"priority:critical"},{"name":"good-first-issue"}],
            "createdAt": "2099-01-01T00:00:00Z",
            "url": "http://example.test/issues/999",
            "author": {"login": "test-user"},
            "comments": [{},{}]
        }
    ]"#
    .to_string()
}

/// Schema-compliant payload the `MockSubstrateBackend` returns. Matches the
/// `implementer.toml` spec: `{issue_number: int, pr_url: string, summary: string}`.
fn canned_implementer_outcome() -> serde_json::Value {
    json!({
        "issue_number": 999,
        "pr_url": "https://github.com/jdmiranda/phantom/pull/12345",
        "summary": "stubbed PR for the phantom-on-phantom bridge test"
    })
}

/// Implementer-loop spec from `.phantom/loops/implementer.toml`, inlined so
/// the test does not depend on filesystem layout. Tool whitelist trimmed to
/// the minimum the test exercises (the spec parser does not enforce the
/// whitelist — it just round-trips it through).
const IMPLEMENTER_TOML: &str = r#"
id = "implementer"
max_concurrent = 1

[agent]
role = "Actor"
allow_tools = ["read_file", "write_file"]
system_prompt = "Implement the assigned issue."

[agent.exit_schema]
type = "object"
required = ["issue_number", "pr_url", "summary"]

[agent.exit_schema.properties.issue_number]
type = "integer"

[agent.exit_schema.properties.pr_url]
type = "string"

[agent.exit_schema.properties.summary]
type = "string"

[source]
kind = "queue"
name = "implementer-queue"
"#;

// ---------------------------------------------------------------------------
// Stage 1: brain → bridge → registry
// ---------------------------------------------------------------------------

/// Stage 1 — exercises the bridge in isolation.
///
/// `GhIssueGoalSource` (mock-backed) → `SelfImprovementState::tick` →
/// `LoopQueueActionHandler` → `LoopQueueRegistry`. After one tick the
/// `implementer-queue` must hold exactly one message with the expected
/// payload shape.
#[test]
fn stage_1_brain_tick_drives_action_into_queue_via_handler() {
    let stub = StubGhRunner::new(vec![critical_issue_fixture()]);
    let mut source: Box<dyn GoalSource> = Box::new(GhIssueGoalSource::with_runner(
        "jdmiranda/phantom",
        None,
        Duration::ZERO,
        Box::new(stub),
    ));

    // Enable self-improvement and relax the rate limits so a single tick
    // emits a single auto-enqueue. Cooldown 0 so a hypothetical second
    // tick is not artificially blocked.
    let mut state = SelfImprovementState::new(SelfImprovementConfig {
        enabled: true,
        per_hour: 10,
        per_day: 50,
        cooldown: Duration::ZERO,
        ..Default::default()
    });

    // The handler is the unit under test.
    let registry = Arc::new(LoopQueueRegistry::new());
    let mut handler = LoopQueueActionHandler::new(Arc::clone(&registry));

    // One tick produces a Vec<AiAction>; route each one through the handler
    // exactly as the CLI's forwarder thread does.
    let actions = state.tick(std::slice::from_mut(&mut source));
    assert_eq!(
        actions.len(),
        1,
        "exactly one critical issue must yield exactly one EnqueueLoopMessage"
    );
    assert!(matches!(actions[0], AiAction::EnqueueLoopMessage { .. }));
    for action in actions {
        action.execute(&mut handler);
    }

    // Bridge assertion: the queue holds the implementer payload.
    let popped = registry
        .pop(DEFAULT_IMPLEMENTER_QUEUE)
        .expect("the bridge must have pushed exactly one message");
    assert_eq!(popped.from_loop, "gh-issues", "from_loop = source name");
    assert_eq!(
        popped.payload["external_id"].as_str(),
        Some("gh-issue:999"),
        "payload threads the upstream issue id"
    );
    assert!(
        popped.payload["title"].as_str().unwrap_or("").contains("bridge"),
        "payload carries the human-readable title"
    );
    assert!(
        registry.pop(DEFAULT_IMPLEMENTER_QUEUE).is_none(),
        "exactly one message — no duplicate enqueue"
    );
}

// ---------------------------------------------------------------------------
// Stage 2: registry → runner → dispatcher → driver → mock backend
// ---------------------------------------------------------------------------

/// Stage 2 — exercises the consumer half of the pipeline.
///
/// Manually populates `implementer-queue`, then drives a real
/// [`LoopRunner`] (configured from the implementer spec) through one
/// iteration. Asserts the schema-valid mock outcome flows back through the
/// `SubstrateCompletionRouter` and the runner stops cleanly when the queue
/// drains.
#[tokio::test]
async fn stage_2_runner_drains_queue_through_substrate_driver() {
    let (spec, schema) = parse_spec_str(IMPLEMENTER_TOML).expect("implementer toml parses");

    let queues = Arc::new(LoopQueueRegistry::new());
    // Seed the implementer queue with one message — the same shape the
    // bridge would have produced.
    queues.push(
        DEFAULT_IMPLEMENTER_QUEUE,
        phantom_loop::LoopMessage::new(
            "gh-issues",
            json!({
                "external_id": "gh-issue:999",
                "title": "Phantom-on-phantom bridge test",
                "key": "issue#999"
            }),
        ),
    );

    let spawn_queue = new_spawn_subagent_queue();
    let dispatcher = Arc::new(SubstrateAgentDispatcher::with_default_parent(
        spawn_queue.clone(),
    ));

    let canned = canned_implementer_outcome();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(16);
    let backend: Arc<dyn SubstrateBackend> = Arc::new(MockSubstrateBackend::ok(canned.clone()));
    let driver = SubstrateDriver::new(spawn_queue.clone(), backend, tx)
        .with_tick_interval(Duration::from_millis(10));
    let _driver_handle = driver.run();

    // Forwarder: pipe the driver's `AgentTaskComplete` events into the
    // dispatcher's completion router so the runner's pending oneshot
    // resolves.
    let router = dispatcher.completion_router();
    let router_clone = router.clone();
    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            router_clone.on_completion(event);
        }
    });

    // Wrap the queue source so the runner stops after the seeded message is
    // drained — the default LoopMessageQueueSource is open-ended.
    struct DrainOnceSource {
        inner: LoopMessageQueueSource,
        seen: AtomicUsize,
    }
    impl LoopSource for DrainOnceSource {
        fn next(&mut self, ctx: &LoopContext) -> LoopPullResult {
            match self.inner.next(ctx) {
                LoopPullResult::Available(input) => {
                    self.seen.fetch_add(1, Ordering::SeqCst);
                    LoopPullResult::Available(input)
                }
                LoopPullResult::Empty => {
                    if self.seen.load(Ordering::SeqCst) > 0 {
                        LoopPullResult::Done
                    } else {
                        LoopPullResult::Empty
                    }
                }
                other => other,
            }
        }
    }

    let source = Box::new(DrainOnceSource {
        inner: LoopMessageQueueSource::new(&queues, DEFAULT_IMPLEMENTER_QUEUE),
        seen: AtomicUsize::new(0),
    });

    let runner = LoopRunner::new(
        Arc::new(spec),
        schema,
        source,
        queues.clone(),
        dispatcher.clone(),
    );

    let reason = runner.run().await;
    assert_eq!(reason, "source exhausted", "runner stops when queue drains");
    assert_eq!(
        dispatcher.pending_count(),
        0,
        "every dispatched agent must have been routed through completion"
    );
}

// ---------------------------------------------------------------------------
// Stage 3: full pipeline (bridge → runner → driver) in one test
// ---------------------------------------------------------------------------

/// Stage 3 — the full phantom-on-phantom path.
///
/// Combines Stage 1 (bridge produces the queue message) and Stage 2
/// (runner drains the queue and drives it through the substrate driver) in
/// a single test. The bridge is invoked once before the runner starts so
/// the source has work to do; in production the brain forwarder runs
/// concurrently with the runner, but a single ordering exercises the same
/// causal chain with deterministic timing.
#[tokio::test]
async fn stage_3_full_pipeline_brain_to_queue_to_runner_to_completion() {
    // --- Bridge half ---
    let stub = StubGhRunner::new(vec![critical_issue_fixture()]);
    let mut source: Box<dyn GoalSource> = Box::new(GhIssueGoalSource::with_runner(
        "jdmiranda/phantom",
        None,
        Duration::ZERO,
        Box::new(stub),
    ));
    let mut state = SelfImprovementState::new(SelfImprovementConfig {
        enabled: true,
        per_hour: 10,
        per_day: 50,
        cooldown: Duration::ZERO,
        ..Default::default()
    });

    let queues = Arc::new(LoopQueueRegistry::new());
    let mut handler = LoopQueueActionHandler::new(Arc::clone(&queues));
    let actions = state.tick(std::slice::from_mut(&mut source));
    assert_eq!(actions.len(), 1, "stage 1: exactly one action emitted");
    for action in actions {
        action.execute(&mut handler);
    }
    assert_eq!(
        queues.get_or_create(DEFAULT_IMPLEMENTER_QUEUE).len(),
        1,
        "stage 1: queue depth = 1 after handler routes the action"
    );

    // --- Runner half ---
    let (spec, schema) = parse_spec_str(IMPLEMENTER_TOML).expect("implementer toml parses");
    let spawn_queue = new_spawn_subagent_queue();
    let dispatcher = Arc::new(SubstrateAgentDispatcher::with_default_parent(
        spawn_queue.clone(),
    ));
    let canned = canned_implementer_outcome();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(16);
    let backend: Arc<dyn SubstrateBackend> = Arc::new(MockSubstrateBackend::ok(canned.clone()));
    let driver = SubstrateDriver::new(spawn_queue.clone(), backend, tx)
        .with_tick_interval(Duration::from_millis(10));
    let _driver_handle = driver.run();
    let router = dispatcher.completion_router();
    let router_clone = router.clone();
    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            router_clone.on_completion(event);
        }
    });

    // Same drain-once wrapper as stage 2.
    struct DrainOnceSource {
        inner: LoopMessageQueueSource,
        seen: AtomicUsize,
    }
    impl LoopSource for DrainOnceSource {
        fn next(&mut self, ctx: &LoopContext) -> LoopPullResult {
            match self.inner.next(ctx) {
                LoopPullResult::Available(input) => {
                    self.seen.fetch_add(1, Ordering::SeqCst);
                    LoopPullResult::Available(input)
                }
                LoopPullResult::Empty => {
                    if self.seen.load(Ordering::SeqCst) > 0 {
                        LoopPullResult::Done
                    } else {
                        LoopPullResult::Empty
                    }
                }
                other => other,
            }
        }
    }
    let runner_source = Box::new(DrainOnceSource {
        inner: LoopMessageQueueSource::new(&queues, DEFAULT_IMPLEMENTER_QUEUE),
        seen: AtomicUsize::new(0),
    });
    let runner = LoopRunner::new(
        Arc::new(spec),
        schema,
        runner_source,
        queues.clone(),
        dispatcher.clone(),
    );
    let reason = runner.run().await;
    assert_eq!(
        reason, "source exhausted",
        "stage 3: runner drained the brain-injected message and stopped cleanly"
    );
    assert_eq!(
        dispatcher.pending_count(),
        0,
        "stage 3: no pending dispatches left at the dispatcher"
    );

    // The queue must be empty — the message produced by the bridge was
    // popped exactly once by the runner.
    assert_eq!(
        queues.get_or_create(DEFAULT_IMPLEMENTER_QUEUE).len(),
        0,
        "stage 3: queue drained — exactly one message produced and consumed"
    );
}

// ---------------------------------------------------------------------------
// Negative path: bridge with self-improvement disabled is a no-op
// ---------------------------------------------------------------------------

/// When `SelfImprovementConfig::enabled` is `false` (the design-doc default),
/// `tick` returns no actions. The bridge is unused; the queue stays empty.
/// This mirrors the `--no-self-improve` CLI flag's runtime behaviour.
#[test]
fn disabled_self_improvement_produces_no_actions_and_no_queue_messages() {
    let stub = StubGhRunner::new(vec![critical_issue_fixture()]);
    let mut source: Box<dyn GoalSource> = Box::new(GhIssueGoalSource::with_runner(
        "jdmiranda/phantom",
        None,
        Duration::ZERO,
        Box::new(stub),
    ));
    // Default config has enabled = false — the explicit reaffirmation here
    // documents the contract.
    let mut state = SelfImprovementState::new(SelfImprovementConfig {
        enabled: false,
        ..Default::default()
    });
    let queues = Arc::new(LoopQueueRegistry::new());
    let mut handler = LoopQueueActionHandler::new(Arc::clone(&queues));

    let actions = state.tick(std::slice::from_mut(&mut source));
    assert!(
        actions.is_empty(),
        "self-improvement disabled → no actions emitted regardless of candidate score"
    );
    for action in actions {
        action.execute(&mut handler);
    }
    assert!(
        queues.pop(DEFAULT_IMPLEMENTER_QUEUE).is_none(),
        "queue stays empty when bridge is bypassed"
    );
}
