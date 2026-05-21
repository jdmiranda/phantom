//! End-to-end smoke test for [`phantom_loop::LoopRunner`].
//!
//! Wires a one-shot stub source, an immediately-resolving stub
//! [`phantom_loop::AgentDispatcher`], and a real
//! [`phantom_loop::LoopQueueRegistry`] together to verify that the FSM
//! traverses every public state on the happy path:
//!
//! `Idle → Pulling → Dispatching → Awaiting → Validating → (effects) →
//! Pulling → Stopped("source exhausted")`.
//!
//! The smoke test asserts:
//!
//! - The source is pulled (its `next` is called).
//! - The dispatcher is called with the agent spec.
//! - On a valid result, the configured [`phantom_loop::LoopEffect::EnqueueTo`]
//!   fires and the message lands on the named queue.
//! - The runner reaches `Stopped("source exhausted")` after the source
//!   returns `Done`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use phantom_loop::{
    AgentDispatcher, CorrelationId, DispatchError, DispatchHandle, LoopContext, LoopInput,
    LoopMessageQueueSource, LoopPullResult, LoopQueueRegistry, LoopRunner, LoopSource,
    parse_spec_str,
};
use serde_json::json;
use tokio::sync::oneshot;

/// Stub source: emits one input, then `Done`. Counts pulls for assertion.
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

/// Stub dispatcher: immediately resolves the oneshot with a canned result.
/// Counts dispatches for assertion.
struct StubDispatcher {
    canned_result: serde_json::Value,
    dispatch_count: Arc<AtomicUsize>,
}

impl AgentDispatcher for StubDispatcher {
    fn dispatch(
        &self,
        _spec: &phantom_loop::LoopAgentSpec,
        _input: &LoopInput,
    ) -> Result<DispatchHandle, DispatchError> {
        self.dispatch_count.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        tx.send(Ok(self.canned_result.clone()))
            .expect("oneshot send");
        Ok(DispatchHandle::new(7777, rx))
    }
}

/// Reviewer-style spec wired into the smoke test: one PR-number input, one
/// `EnqueueTo` effect that maps the agent's `pr_number` field onto a
/// `target_pr` field on the outgoing queue message.
const REVIEWER_TOML: &str = r#"
id = "smoke-reviewer"

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

#[tokio::test]
async fn happy_path_traverses_full_fsm_and_fires_effects() {
    let (spec, schema) = parse_spec_str(REVIEWER_TOML).expect("parse smoke spec");
    let pulls = Arc::new(AtomicUsize::new(0));
    let dispatches = Arc::new(AtomicUsize::new(0));
    let queues = Arc::new(LoopQueueRegistry::new());

    let source = Box::new(OneShotSource {
        input: Some(LoopInput {
            key: "pr#42".to_string(),
            payload: json!({"pr_number": 42}),
            correlation_id: CorrelationId::new("smoke:1"),
        }),
        pull_count: pulls.clone(),
    });

    let dispatcher = Arc::new(StubDispatcher {
        canned_result: json!({"pr_number": 42, "decision": "approved"}),
        dispatch_count: dispatches.clone(),
    });

    let runner = LoopRunner::new(
        Arc::new(spec),
        schema,
        source,
        queues.clone(),
        dispatcher,
    );

    // Drive the runner to completion.
    let reason = runner.run().await;
    assert_eq!(reason, "source exhausted");

    // Source must have been pulled at least twice: once to get the input,
    // once more to receive Done.
    assert!(
        pulls.load(Ordering::SeqCst) >= 2,
        "expected ≥2 pulls, got {}",
        pulls.load(Ordering::SeqCst)
    );

    // Dispatcher must have been called exactly once for the one input.
    assert_eq!(
        dispatches.load(Ordering::SeqCst),
        1,
        "dispatcher must have been called once"
    );

    // The effect must have enqueued one message on `downstream` with the
    // field map applied.
    let downstream = queues.get_or_create("downstream");
    assert_eq!(downstream.len(), 1, "one downstream message must have been enqueued");
    let msg = downstream.pop().expect("message");
    assert_eq!(msg.from_loop, "smoke-reviewer");
    assert_eq!(msg.payload["reviewed_pr"], 42);
}

#[tokio::test]
async fn step_by_step_progression_matches_state_diagram() {
    // Same spec, but drive the FSM one transition at a time and assert on
    // the state shape at each step.
    let (spec, schema) = parse_spec_str(REVIEWER_TOML).expect("parse smoke spec");
    let queues = Arc::new(LoopQueueRegistry::new());
    let pulls = Arc::new(AtomicUsize::new(0));
    let dispatches = Arc::new(AtomicUsize::new(0));

    let source = Box::new(OneShotSource {
        input: Some(LoopInput {
            key: "pr#1".to_string(),
            payload: json!({}),
            correlation_id: CorrelationId::new("step:1"),
        }),
        pull_count: pulls.clone(),
    });
    let dispatcher = Arc::new(StubDispatcher {
        canned_result: json!({"pr_number": 1, "decision": "approved"}),
        dispatch_count: dispatches.clone(),
    });

    let mut runner = LoopRunner::new(
        Arc::new(spec),
        schema,
        source,
        queues.clone(),
        dispatcher,
    );

    // Step 1: Idle → Pulling.
    assert!(matches!(
        runner.state(),
        phantom_loop::LoopState::Idle { .. }
    ));
    runner.step().await;
    assert!(matches!(runner.state(), phantom_loop::LoopState::Pulling));

    // Step 2: Pulling → Dispatching.
    runner.step().await;
    assert!(matches!(
        runner.state(),
        phantom_loop::LoopState::Dispatching { .. }
    ));

    // Step 3: Dispatching → Awaiting (the dispatcher resolved the oneshot
    // immediately, but the state still passes through Awaiting because the
    // FSM doesn't poll the receiver inside `dispatch`).
    runner.step().await;
    assert!(matches!(
        runner.state(),
        phantom_loop::LoopState::Awaiting { agent_id: 7777, .. }
    ));

    // Step 4: Awaiting → Validating.
    runner.step().await;
    assert!(matches!(
        runner.state(),
        phantom_loop::LoopState::Validating { .. }
    ));

    // Step 5: Validating → Pulling (effects fired; queue has one message).
    runner.step().await;
    assert!(matches!(runner.state(), phantom_loop::LoopState::Pulling));
    assert_eq!(queues.get_or_create("downstream").len(), 1);

    // Step 6: Pulling → Stopped (source returned Done).
    runner.step().await;
    match runner.state() {
        phantom_loop::LoopState::Stopped { reason } => {
            assert_eq!(reason, "source exhausted")
        }
        other => panic!("expected Stopped, got {other:?}"),
    }
}

#[tokio::test]
async fn queue_source_drains_a_message_pushed_by_the_test() {
    // Verifies the `Queue` source variant works end-to-end with the runner.
    // We push one message into `review-queue` ourselves, then run the
    // reviewer spec which consumes from that same queue.
    let (spec, schema) = parse_spec_str(REVIEWER_TOML).expect("parse spec");
    let queues = Arc::new(LoopQueueRegistry::new());

    // Pre-populate the queue.
    queues.push(
        "review-queue",
        phantom_loop::LoopMessage::new(
            "pr-finder",
            json!({"key": "pr#99", "pr_url": "https://github.com/x/y/pull/99"}),
        ),
    );

    // Source = the real LoopMessageQueueSource, not a stub.
    let source = Box::new(LoopMessageQueueSource::new(&queues, "review-queue"));

    // Dispatcher returns a canned valid result.
    let dispatches = Arc::new(AtomicUsize::new(0));
    let dispatcher = Arc::new(StubDispatcher {
        canned_result: json!({"pr_number": 99, "decision": "approved"}),
        dispatch_count: dispatches.clone(),
    });

    let runner = LoopRunner::new(
        Arc::new(spec),
        schema,
        source,
        queues.clone(),
        dispatcher,
    );

    // The queue source returns `Empty` after the first message — the runner
    // will back off and re-poll forever in production. We give it a budget
    // of state-transitions via step() and assert observed effects.
    drive_for_n_steps(runner, 50).await;
    assert_eq!(
        dispatches.load(Ordering::SeqCst),
        1,
        "dispatcher must have been called once for the one queued message"
    );
    assert_eq!(queues.get_or_create("downstream").len(), 1);
}

/// Drive a runner for at most `n` steps. Used by the queue-source test where
/// the source legitimately stays `Empty` forever — there's no `Done` signal.
async fn drive_for_n_steps(mut runner: LoopRunner, n: usize) {
    // Pause the tokio clock so the IDLE_BACKOFF sleeps inside `Pulling`
    // resolve instantly rather than wallclock-waiting.
    tokio::time::pause();
    for _ in 0..n {
        if matches!(runner.state(), phantom_loop::LoopState::Stopped { .. }) {
            break;
        }
        runner.step().await;
        // Advance the tokio clock past the runner's IDLE_BACKOFF (100ms)
        // so any pending sleeps resolve.
        tokio::time::advance(std::time::Duration::from_millis(200)).await;
    }
}
