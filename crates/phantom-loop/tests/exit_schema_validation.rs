//! Exit-schema validation flow.
//!
//! Drives [`phantom_loop::LoopRunner`] with a stub dispatcher that returns
//! a result *failing* the spec's `exit_schema`, and asserts the runner's
//! [`phantom_loop::LoopQuarantinePolicy`] decision:
//!
//! - `FailAndStop` → `Stopped("exit schema validation failed: …")`.
//! - `Park`        → `Stopped("parked: exit schema validation failed: …")`.
//! - `SkipAndContinue` → loop back to `Pulling` and continue.

use std::sync::Arc;

use phantom_loop::{
    AgentDispatcher, CorrelationId, DispatchError, DispatchHandle, LoopInput, LoopPullResult,
    LoopQueueRegistry, LoopRunner, LoopSource, parse_spec_str,
};
use serde_json::json;
use tokio::sync::oneshot;

const REVIEWER_SPEC: &str = r#"
id = "validation-test"
on_quarantine = "fail_and_stop"

[agent]
role = "Actor"
system_prompt = "noop"

[agent.exit_schema]
type = "object"
required = ["pr_number", "decision"]

[agent.exit_schema.properties.pr_number]
type = "integer"

[agent.exit_schema.properties.decision]
enum = ["approved", "rejected", "needs_changes"]

[source]
kind = "queue"
name = "in"
"#;

/// One-shot stub source — emits one input, then `Done`.
struct OneShot {
    input: Option<LoopInput>,
}
impl LoopSource for OneShot {
    fn next(&mut self, _ctx: &phantom_loop::LoopContext) -> LoopPullResult {
        match self.input.take() {
            Some(i) => LoopPullResult::Available(i),
            None => LoopPullResult::Done,
        }
    }
}

/// Dispatcher that always returns the given (schema-invalid) result.
struct InvalidResultDispatcher {
    invalid: serde_json::Value,
}
impl AgentDispatcher for InvalidResultDispatcher {
    fn dispatch(
        &self,
        _spec: &phantom_loop::LoopAgentSpec,
        _input: &LoopInput,
    ) -> Result<DispatchHandle, DispatchError> {
        let (tx, rx) = oneshot::channel();
        let _ = tx.send(Ok(self.invalid.clone()));
        Ok(DispatchHandle::new(1, rx))
    }
}

#[tokio::test]
async fn fail_and_stop_policy_stops_runner_on_invalid_result() {
    let (spec, schema) = parse_spec_str(REVIEWER_SPEC).expect("parse spec");
    assert_eq!(
        spec.on_quarantine,
        phantom_loop::LoopQuarantinePolicy::FailAndStop
    );

    let source = Box::new(OneShot {
        input: Some(LoopInput {
            key: "k".to_string(),
            payload: json!({}),
            correlation_id: CorrelationId::new("c"),
        }),
    });

    // Schema requires pr_number:integer and decision:enum. This result
    // violates both: missing pr_number, decision is an invalid enum value.
    let dispatcher = Arc::new(InvalidResultDispatcher {
        invalid: json!({"decision": "totally_bogus"}),
    });

    let runner = LoopRunner::new(
        Arc::new(spec),
        schema,
        source,
        Arc::new(LoopQueueRegistry::new()),
        dispatcher,
    );
    let reason = runner.run().await;
    assert!(
        reason.starts_with("exit schema validation failed:"),
        "expected schema-validation stop reason, got: `{reason}`"
    );
}

#[tokio::test]
async fn park_policy_stops_runner_with_park_prefix() {
    let parked_spec = REVIEWER_SPEC.replace("fail_and_stop", "park");
    let (spec, schema) = parse_spec_str(&parked_spec).expect("parse parked spec");
    assert_eq!(spec.on_quarantine, phantom_loop::LoopQuarantinePolicy::Park);

    let source = Box::new(OneShot {
        input: Some(LoopInput {
            key: "k".to_string(),
            payload: json!({}),
            correlation_id: CorrelationId::new("c"),
        }),
    });
    let dispatcher = Arc::new(InvalidResultDispatcher {
        invalid: json!({"decision": "bogus"}),
    });

    let runner = LoopRunner::new(
        Arc::new(spec),
        schema,
        source,
        Arc::new(LoopQueueRegistry::new()),
        dispatcher,
    );
    let reason = runner.run().await;
    assert!(
        reason.starts_with("parked: exit schema validation failed:"),
        "expected `parked:` prefix, got: `{reason}`"
    );
}

#[tokio::test]
async fn skip_and_continue_policy_resumes_pulling_after_invalid_result() {
    // SkipAndContinue means: drop the invalid iteration, go back to Pulling.
    // With a one-shot source, the second pull returns Done — so the runner
    // ultimately stops with "source exhausted", proving it did *not* stop
    // on the schema failure.
    let skip_spec = REVIEWER_SPEC.replace("fail_and_stop", "skip_and_continue");
    let (spec, schema) = parse_spec_str(&skip_spec).expect("parse skip spec");
    assert_eq!(
        spec.on_quarantine,
        phantom_loop::LoopQuarantinePolicy::SkipAndContinue
    );

    let source = Box::new(OneShot {
        input: Some(LoopInput {
            key: "k".to_string(),
            payload: json!({}),
            correlation_id: CorrelationId::new("c"),
        }),
    });
    let dispatcher = Arc::new(InvalidResultDispatcher {
        invalid: json!({"decision": "bogus"}),
    });

    let runner = LoopRunner::new(
        Arc::new(spec),
        schema,
        source,
        Arc::new(LoopQueueRegistry::new()),
        dispatcher,
    );
    let reason = runner.run().await;
    assert_eq!(
        reason, "source exhausted",
        "skip_and_continue must let the runner exit on source exhaustion, not on validation"
    );
}
