//! Integration coverage for the agentless-dispatch branch of
//! [`phantom_loop::LoopRunner`].
//!
//! Regression for [#665](https://github.com/jdmiranda/phantom/issues/665):
//! `LoopRunner::dispatch` used to forward `serde_json::Value::Null` as the
//! effect-runner result on the agentless path, discarding both `LoopInput.key`
//! and `LoopInput.payload`. That meant `EnqueueTo.fields` mappings with
//! `from = "key"` or `from = "payload.<field>"` could not resolve — every
//! agentless loop with a non-empty field map (e.g. `pr_finder_review.toml`,
//! `pr_finder_impl.toml`) stopped on the first input with
//! `EffectError::FieldMapMissing`.
//!
//! After the fix, `dispatch` synthesises a `{ "key": ..., "payload": ... }`
//! JSON object from the source's `LoopInput` and hands it to the effect
//! runner so the documented contract holds.

use std::sync::Arc;

use phantom_loop::{
    AgentDispatcher, CorrelationId, DispatchError, DispatchHandle, LoopAgentSpec, LoopContext,
    LoopInput, LoopPullResult, LoopQueueRegistry, LoopRunner, LoopSource,
    parse_spec_str,
};
use serde_json::json;

/// One-shot stub source: emits a single canned [`LoopInput`], then `Done`.
struct OneShotSource {
    input: Option<LoopInput>,
}

impl LoopSource for OneShotSource {
    fn next(&mut self, _ctx: &LoopContext) -> LoopPullResult {
        match self.input.take() {
            Some(i) => LoopPullResult::Available(i),
            None => LoopPullResult::Done,
        }
    }
}

/// Dispatcher stub that should never be invoked on the agentless path.
///
/// If the runner ever calls into this on an agentless spec, the test fails
/// loudly via the `panic!` — the agentless branch is supposed to bypass the
/// dispatcher entirely.
struct UnreachableDispatcher;

impl AgentDispatcher for UnreachableDispatcher {
    fn dispatch(
        &self,
        _spec: &LoopAgentSpec,
        _input: &LoopInput,
    ) -> Result<DispatchHandle, DispatchError> {
        panic!("agentless runner must not invoke the agent dispatcher");
    }
}

/// Agentless spec wired against a cron source: the runner doesn't actually
/// drive the cron — the test swaps in a one-shot stub source via
/// [`LoopRunner::new`] — but the spec still has to parse, so we declare a
/// `cron` source to satisfy schema requirements.
///
/// `on_complete` declares the exact field-map shape from the broken
/// `pr_finder_*.toml` specs: `from = "key"` resolves to the input's stable
/// identifier; `from = "payload.title"` resolves into a source-emitted
/// payload field. Both must resolve against the synthesised result the
/// agentless path now forwards.
const AGENTLESS_TOML: &str = r#"
id = "test-agentless"

[source]
kind = "cron"
interval_seconds = 60

[[on_complete]]
kind = "enqueue_to"
queue = "test-queue"

[[on_complete.fields]]
from = "key"
to = "pr_number"

[[on_complete.fields]]
from = "payload.title"
to = "title"
"#;

#[tokio::test]
async fn agentless_loop_forwards_key_and_payload_fields_to_effects() {
    let (spec, schema) = parse_spec_str(AGENTLESS_TOML).expect("parse agentless spec");
    assert!(spec.agent.is_none(), "spec must be agentless for this test");

    let queues = Arc::new(LoopQueueRegistry::new());

    let source = Box::new(OneShotSource {
        input: Some(LoopInput {
            key: "pr#1241".to_string(),
            payload: json!({"title": "fix X", "author": "alice"}),
            correlation_id: CorrelationId::new("agentless:1"),
        }),
    });

    let mut runner = LoopRunner::new(
        Arc::new(spec),
        schema,
        source,
        queues.clone(),
        Arc::new(UnreachableDispatcher),
    );

    // Drive: Idle → Pulling → Dispatching → (agentless effects) → Pulling.
    // We stop after the effects fire, before the second Pulling would call
    // `next` again and observe `Done`, so each transition is asserted on the
    // shape downstream consumers actually see.
    runner.step().await; // Idle → Pulling
    runner.step().await; // Pulling → Dispatching
    runner.step().await; // Dispatching → (effects) → Pulling

    // Pop from `test-queue` and confirm the payload carries both mappings.
    let msg = queues
        .pop("test-queue")
        .expect("agentless effect must enqueue exactly one message");
    assert_eq!(msg.from_loop, "test-agentless");
    assert_eq!(
        msg.payload,
        json!({"pr_number": "pr#1241", "title": "fix X"}),
        "field map must resolve `key` to the input key and `payload.title` to the source-emitted title",
    );

    // The agentless branch must bypass the dispatcher entirely. The
    // `UnreachableDispatcher::dispatch` panic above is the load-bearing
    // assertion — reaching this point at all proves the dispatcher was not
    // invoked.
}
