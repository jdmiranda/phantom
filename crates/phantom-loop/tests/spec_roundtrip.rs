//! End-to-end TOML round-trip for [`phantom_loop::LoopSpec`].
//!
//! These tests exercise the full parse path: serde → semantic validation →
//! JSON Schema compilation. They cover the two canonical shapes from
//! issue #650:
//!
//! - A reviewer-style loop with an `[agent]` section and `gh_pr` source.
//! - An agentless PR-finder loop with a `cron` source and no `[agent]`.

use phantom_loop::{
    GhPrState, LoopEffect, LoopQuarantinePolicy, LoopSourceSpec, parse_spec_str,
};
use serde_json::json;

const REVIEWER_TOML: &str = r#"
id = "reviewer"
max_concurrent = 2

[agent]
role = "Actor"
allow_tools = ["read_file", "gh_pr_review", "gh_pr_merge"]
system_prompt = "Review the PR for correctness and style. Approve, reject, or request changes."

[agent.exit_schema]
type = "object"
required = ["pr_number", "decision"]
additionalProperties = false

[agent.exit_schema.properties.pr_number]
type = "integer"

[agent.exit_schema.properties.decision]
enum = ["approved", "rejected", "needs_changes"]

[agent.policy]
max_attempts = 2
timeout_seconds = 900

[source]
kind = "gh_pr"
repo = "jdmiranda/phantom"

[source.predicate]
state = "open"
review_required = true

[[on_complete]]
kind = "log_to_bus"
event_kind = "pr_reviewed"
"#;

#[test]
fn reviewer_spec_roundtrips_with_compiled_schema() {
    let (spec, schema) = parse_spec_str(REVIEWER_TOML).expect("parse reviewer spec");
    let schema = schema.expect("reviewer spec has an [agent] so schema must be Some");

    // Identity + structural assertions on the spec itself.
    assert_eq!(spec.id, "reviewer");
    assert_eq!(spec.max_concurrent, 2);
    assert_eq!(spec.on_quarantine, LoopQuarantinePolicy::SkipAndContinue);

    let agent = spec.agent.as_ref().expect("reviewer spec has an agent");
    assert_eq!(agent.role, phantom_agents::role::AgentRole::Actor);

    let allow_tools = agent
        .allow_tools
        .as_ref()
        .expect("reviewer narrows allow_tools");
    assert!(allow_tools.iter().any(|t| t == "read_file"));
    assert!(allow_tools.iter().any(|t| t == "gh_pr_review"));
    assert!(allow_tools.iter().any(|t| t == "gh_pr_merge"));

    // Policy was overridden in the TOML — both fields must reflect the override.
    assert_eq!(agent.policy.max_attempts, 2);
    assert_eq!(agent.policy.timeout_seconds, 900);
    // Fields not present in TOML take the LoopPolicy defaults.
    assert!(!agent.policy.auto_approve);
    assert!(!agent.policy.skip_planning);

    // Source must match GhPr { state = Open, review_required = true }.
    match &spec.source {
        LoopSourceSpec::GhPr { repo, predicate } => {
            assert_eq!(repo, "jdmiranda/phantom");
            assert_eq!(predicate.state, GhPrState::Open);
            assert!(predicate.review_required);
            assert!(!predicate.failing_ci);
        }
        other => panic!("expected GhPr, got {other:?}"),
    }

    // One on_complete effect: LogToBus { event_kind: "pr_reviewed" }.
    assert_eq!(spec.on_complete.len(), 1);
    match &spec.on_complete[0] {
        LoopEffect::LogToBus { event_kind } => assert_eq!(event_kind, "pr_reviewed"),
        other => panic!("expected LogToBus, got {other:?}"),
    }

    // Compiled exit_schema accepts a well-formed payload.
    schema
        .validate(&json!({ "pr_number": 1234, "decision": "approved" }))
        .expect("valid exit payload");

    // ... and rejects type-mismatched pr_number.
    assert!(
        schema
            .validate(&json!({ "pr_number": "not_a_number", "decision": "approved" }))
            .is_err(),
        "string pr_number must be rejected"
    );

    // ... and rejects an unknown enum value for decision.
    assert!(
        schema
            .validate(&json!({ "pr_number": 1, "decision": "bogus_value" }))
            .is_err(),
        "out-of-enum decision must be rejected"
    );

    // ... and rejects a payload missing both required fields.
    assert!(
        schema.validate(&json!({})).is_err(),
        "empty object must be rejected"
    );
}

const PR_FINDER_TOML: &str = r#"
id = "pr-finder"

[source]
kind = "cron"
interval_seconds = 300

[[on_complete]]
kind = "enqueue_to"
queue = "review-queue"

[[on_complete.fields]]
from = "result.pr_url"
to = "target_pr"
"#;

#[test]
fn agentless_pr_finder_spec_parses() {
    let (spec, schema) = parse_spec_str(PR_FINDER_TOML).expect("parse pr-finder spec");

    assert_eq!(spec.id, "pr-finder");
    assert!(spec.agent.is_none(), "PR-finder must be agentless");
    assert!(
        schema.is_none(),
        "agentless specs return no compiled exit schema"
    );

    match &spec.source {
        LoopSourceSpec::Cron { interval_seconds } => assert_eq!(*interval_seconds, 300),
        other => panic!("expected Cron, got {other:?}"),
    }

    assert_eq!(spec.on_complete.len(), 1);
    match &spec.on_complete[0] {
        LoopEffect::EnqueueTo { queue, fields } => {
            assert_eq!(queue, "review-queue");
            assert_eq!(fields.len(), 1);
            assert_eq!(fields[0].from, "result.pr_url");
            assert_eq!(fields[0].to, "target_pr");
        }
        other => panic!("expected EnqueueTo, got {other:?}"),
    }

    // Defaults: max_concurrent=1, on_quarantine=SkipAndContinue.
    assert_eq!(spec.max_concurrent, 1);
    assert_eq!(spec.on_quarantine, LoopQuarantinePolicy::SkipAndContinue);
}

#[test]
fn malformed_toml_surfaces_parse_error() {
    let (err_kind, has_msg) = match parse_spec_str("this is = not = valid = toml") {
        Err(e) => (format!("{e:?}"), !e.to_string().is_empty()),
        Ok(_) => panic!("malformed TOML must error"),
    };
    assert!(has_msg, "error must produce a non-empty Display");
    assert!(
        err_kind.contains("Toml"),
        "expected a TomlParse* variant, got {err_kind}"
    );
}

#[test]
fn invalid_exit_schema_fails_at_compile_time() {
    let bad = r#"
        id = "bad-schema"

        [agent]
        role = "Actor"
        system_prompt = "noop"

        [agent.exit_schema]
        type = 42

        [source]
        kind = "cron"
        interval_seconds = 60
    "#;
    let err = parse_spec_str(bad).expect_err("invalid schema must error");
    let kind = format!("{err:?}");
    assert!(
        kind.contains("SchemaCompile"),
        "expected SchemaCompile variant, got {kind}"
    );
}
