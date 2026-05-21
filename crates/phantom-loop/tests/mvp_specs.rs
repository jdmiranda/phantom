//! Validation for the three MVP loop spec TOML files committed at the
//! repo root in `.phantom/loops/`. These are the day-one specs the loop
//! overseer consumes when the user runs `phantom loop run` against the
//! `jdmiranda/phantom` repo itself.
//!
//! Until C3 wires the CLI, this test is the only consumer that proves
//! the three specs parse cleanly against the C1 schema. If a future
//! schema change breaks compatibility with one of these specs, this
//! test fails loudly rather than silently shipping broken fixtures.
//!
//! Layout
//! ------
//! `CARGO_MANIFEST_DIR` is `<repo>/crates/phantom-loop`, so the specs
//! live at `<CARGO_MANIFEST_DIR>/../../.phantom/loops/<name>.toml`.

use std::path::PathBuf;

use phantom_loop::{GhPrState, LoopEffect, LoopQuarantinePolicy, LoopSourceSpec, load_spec};
use serde_json::json;

// ---------------------------------------------------------------------------
// Path helper
// ---------------------------------------------------------------------------

fn loops_dir() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(|p| p.parent())
        .expect("phantom-loop manifest sits two levels below the repo root")
        .join(".phantom")
        .join("loops")
}

fn spec_path(name: &str) -> PathBuf {
    loops_dir().join(format!("{name}.toml"))
}

// ---------------------------------------------------------------------------
// pr_finder split — `pr_finder_review.toml` + `pr_finder_impl.toml`
// ---------------------------------------------------------------------------
//
// The original `pr_finder.toml` was an agentless cron loop that
// fan-routed PRs to two queues based on per-PR state. That design hit
// three C1 schema gaps (no `Poll` effect, no predicate on `EnqueueTo`,
// no `exit_schema` on agentless loops). The split below replaces it with
// two single-source-single-destination loops driven by the `gh_pr`
// source — which post-#664 owns its own `gh pr list` poll and dedup, so
// no cron + no conditional routing is needed.

#[test]
fn pr_finder_review_spec_parses() {
    let path = spec_path("pr_finder_review");
    assert!(
        path.exists(),
        "missing fixture: {} — the MVP specs live at .phantom/loops/",
        path.display()
    );

    let (spec, schema) =
        load_spec(&path).expect("pr_finder_review.toml must parse against the schema");

    assert_eq!(spec.id, "pr_finder_review");
    assert!(
        spec.agent.is_none(),
        "pr_finder_review is agentless — no [agent] table expected"
    );
    assert!(
        schema.is_none(),
        "agentless specs return no compiled exit schema"
    );

    // Source must be the `gh_pr` variant with `review_required = true`.
    match &spec.source {
        LoopSourceSpec::GhPr { repo, predicate } => {
            assert_eq!(repo, "jdmiranda/phantom");
            assert_eq!(predicate.state, GhPrState::Open);
            assert!(
                predicate.review_required,
                "pr_finder_review predicate must set review_required = true"
            );
            assert!(
                !predicate.failing_ci,
                "pr_finder_review predicate must not also gate on failing_ci"
            );
        }
        other => panic!("expected GhPr source, got {other:?}"),
    }

    // One enqueue effect targeting `review-queue`.
    assert_eq!(
        spec.on_complete.len(),
        1,
        "pr_finder_review forwards to exactly one queue"
    );
    match &spec.on_complete[0] {
        LoopEffect::EnqueueTo { queue, .. } => {
            assert_eq!(
                queue, "review-queue",
                "pr_finder_review must route to review-queue"
            );
        }
        other => panic!("expected EnqueueTo, got {other:?}"),
    }

    // Sequential by default; skip-and-continue on quarantine.
    assert_eq!(spec.max_concurrent, 1);
    assert_eq!(spec.on_quarantine, LoopQuarantinePolicy::SkipAndContinue);
}

#[test]
fn pr_finder_impl_spec_parses() {
    let path = spec_path("pr_finder_impl");
    assert!(
        path.exists(),
        "missing fixture: {} — the MVP specs live at .phantom/loops/",
        path.display()
    );

    let (spec, schema) =
        load_spec(&path).expect("pr_finder_impl.toml must parse against the schema");

    assert_eq!(spec.id, "pr_finder_impl");
    assert!(
        spec.agent.is_none(),
        "pr_finder_impl is agentless — no [agent] table expected"
    );
    assert!(
        schema.is_none(),
        "agentless specs return no compiled exit schema"
    );

    // Source must be the `gh_pr` variant with `failing_ci = true`.
    match &spec.source {
        LoopSourceSpec::GhPr { repo, predicate } => {
            assert_eq!(repo, "jdmiranda/phantom");
            assert_eq!(predicate.state, GhPrState::Open);
            assert!(
                predicate.failing_ci,
                "pr_finder_impl predicate must set failing_ci = true"
            );
            assert!(
                !predicate.review_required,
                "pr_finder_impl predicate must not also gate on review_required"
            );
        }
        other => panic!("expected GhPr source, got {other:?}"),
    }

    // One enqueue effect targeting `implementer-queue`.
    assert_eq!(
        spec.on_complete.len(),
        1,
        "pr_finder_impl forwards to exactly one queue"
    );
    match &spec.on_complete[0] {
        LoopEffect::EnqueueTo { queue, .. } => {
            assert_eq!(
                queue, "implementer-queue",
                "pr_finder_impl must route to implementer-queue"
            );
        }
        other => panic!("expected EnqueueTo, got {other:?}"),
    }

    // Sequential by default; skip-and-continue on quarantine.
    assert_eq!(spec.max_concurrent, 1);
    assert_eq!(spec.on_quarantine, LoopQuarantinePolicy::SkipAndContinue);
}

// ---------------------------------------------------------------------------
// reviewer.toml — Actor agent draining review-queue
// ---------------------------------------------------------------------------

#[test]
fn reviewer_spec_parses_and_schema_gates_payloads() {
    let path = spec_path("reviewer");
    assert!(path.exists(), "missing fixture: {}", path.display());

    let (spec, schema) = load_spec(&path).expect("reviewer.toml must parse against C1 schema");
    let schema = schema.expect("reviewer has [agent] so schema must be Some");

    assert_eq!(spec.id, "reviewer");

    let agent = spec.agent.as_ref().expect("reviewer has an agent");
    assert_eq!(agent.role, phantom_agents::role::AgentRole::Actor);

    // Tool whitelist must be the three gh-PR read/write tools.
    let allow_tools = agent
        .allow_tools
        .as_ref()
        .expect("reviewer narrows allow_tools");
    for tool in ["read_file", "gh_pr_review", "gh_pr_merge"] {
        assert!(
            allow_tools.iter().any(|t| t == tool),
            "reviewer must allow {tool}, got {allow_tools:?}"
        );
    }

    // Policy overrides from TOML.
    assert!(agent.policy.auto_approve, "reviewer auto_approve must be true");
    assert_eq!(agent.policy.timeout_seconds, 1200);

    // Source: queue.
    match &spec.source {
        LoopSourceSpec::Queue { name } => assert_eq!(name, "review-queue"),
        other => panic!("expected Queue source, got {other:?}"),
    }

    // on_complete: log_to_bus.
    assert_eq!(spec.on_complete.len(), 1);
    match &spec.on_complete[0] {
        LoopEffect::LogToBus { event_kind } => {
            assert_eq!(event_kind, "reviewer.decision");
        }
        other => panic!("expected LogToBus, got {other:?}"),
    }

    // Schema gates a well-formed approve decision.
    schema
        .validate(&json!({ "pr_number": 658, "decision": "approved" }))
        .expect("approved decision must validate");
    schema
        .validate(&json!({ "pr_number": 658, "decision": "changes_requested" }))
        .expect("changes_requested decision must validate");
    schema
        .validate(&json!({ "pr_number": 658, "decision": "merged" }))
        .expect("merged decision must validate");

    // ... and rejects an out-of-enum decision.
    assert!(
        schema
            .validate(&json!({ "pr_number": 1, "decision": "lgtm" }))
            .is_err(),
        "out-of-enum decision must be rejected"
    );

    // ... and rejects a non-integer pr_number.
    assert!(
        schema
            .validate(&json!({ "pr_number": "658", "decision": "approved" }))
            .is_err(),
        "string pr_number must be rejected"
    );

    // ... and rejects missing-required-field payloads.
    assert!(
        schema.validate(&json!({})).is_err(),
        "empty payload must be rejected"
    );
    assert!(
        schema
            .validate(&json!({ "pr_number": 1 }))
            .is_err(),
        "payload missing decision must be rejected"
    );
}

// ---------------------------------------------------------------------------
// implementer.toml — Actor agent draining implementer-queue, forwarding PR
// ---------------------------------------------------------------------------

#[test]
fn implementer_spec_parses_and_schema_gates_payloads() {
    let path = spec_path("implementer");
    assert!(path.exists(), "missing fixture: {}", path.display());

    let (spec, schema) =
        load_spec(&path).expect("implementer.toml must parse against C1 schema");
    let schema = schema.expect("implementer has [agent] so schema must be Some");

    assert_eq!(spec.id, "implementer");

    let agent = spec.agent.as_ref().expect("implementer has an agent");
    assert_eq!(agent.role, phantom_agents::role::AgentRole::Actor);

    // Tool whitelist is wider — must include write_file, run_command, gh_pr_create.
    let allow_tools = agent
        .allow_tools
        .as_ref()
        .expect("implementer narrows allow_tools");
    for tool in [
        "read_file",
        "write_file",
        "edit_file",
        "run_command",
        "gh_pr_create",
    ] {
        assert!(
            allow_tools.iter().any(|t| t == tool),
            "implementer must allow {tool}, got {allow_tools:?}"
        );
    }

    // Policy.
    assert!(agent.policy.auto_approve);
    assert_eq!(agent.policy.timeout_seconds, 1800);

    // Source.
    match &spec.source {
        LoopSourceSpec::Queue { name } => assert_eq!(name, "implementer-queue"),
        other => panic!("expected Queue source, got {other:?}"),
    }

    // on_complete must forward the PR URL to review-queue.
    assert_eq!(spec.on_complete.len(), 1);
    match &spec.on_complete[0] {
        LoopEffect::EnqueueTo { queue, fields } => {
            assert_eq!(queue, "review-queue");
            assert_eq!(
                fields.len(),
                2,
                "implementer forwards pr_url and issue_number"
            );
            // Field map: result.pr_url -> pr_url, result.issue_number -> issue_number.
            assert!(
                fields
                    .iter()
                    .any(|f| f.from == "result.pr_url" && f.to == "pr_url"),
                "missing pr_url field map: {fields:?}"
            );
            assert!(
                fields.iter().any(
                    |f| f.from == "result.issue_number" && f.to == "issue_number"
                ),
                "missing issue_number field map: {fields:?}"
            );
        }
        other => panic!("expected EnqueueTo, got {other:?}"),
    }

    // Schema accepts a well-formed completion.
    schema
        .validate(&json!({
            "issue_number": 650,
            "pr_url": "https://github.com/jdmiranda/phantom/pull/999",
            "summary": "Wired loop overseer end-to-end."
        }))
        .expect("well-formed completion must validate");

    // ... and rejects missing fields.
    assert!(
        schema
            .validate(&json!({
                "issue_number": 1,
                "summary": "no pr_url"
            }))
            .is_err(),
        "missing pr_url must be rejected"
    );
    assert!(
        schema
            .validate(&json!({
                "pr_url": "https://example.invalid",
                "summary": "no issue"
            }))
            .is_err(),
        "missing issue_number must be rejected"
    );

    // ... and rejects a non-integer issue_number.
    assert!(
        schema
            .validate(&json!({
                "issue_number": "650",
                "pr_url": "https://example.invalid/p/1",
                "summary": "stringly typed"
            }))
            .is_err(),
        "string issue_number must be rejected"
    );
}
