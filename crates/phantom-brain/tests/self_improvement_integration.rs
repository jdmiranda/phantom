//! Integration tests for the brain self-improvement reconciler.
//!
//! These tests exercise the end-to-end path:
//!
//!     [stub gh JSON]
//!         -> GhIssueGoalSource::poll  (parses, dedups)
//!         -> SelfImprovementState::evaluate  (hard exclusions, scoring, gates)
//!         -> AiAction::EnqueueLoopMessage  (forwarded to a stub
//!                                            LoopQueueRegistry)
//!
//! The stub uses `phantom_brain::goal_source::StubGhRunner` so no real `gh`
//! subprocess ever runs.
//!
//! Coverage matrix (mirrors §8.3 of the design doc):
//!
//! | Issue | Labels             | Expected outcome                               |
//! |-------|--------------------|------------------------------------------------|
//! | #100  | priority:critical  | EnqueueLoopMessage; audit decision = enqueued  |
//! | #101  | priority:medium    | Skipped (low score); audit decision recorded   |
//! | #102  | security           | Skipped (hard exclusion); reason = security    |

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use phantom_brain::events::AiAction;
use phantom_brain::goal_source::{
    GhCommandRunner, GhIssueGoalSource, GoalSource, GoalSourceError, StubGhRunner,
};
use phantom_brain::self_improvement::{
    AuditDecision, SelfImprovementConfig, SelfImprovementState, TrustBand,
    DEFAULT_IMPLEMENTER_QUEUE,
};
use phantom_loop::queue::{LoopMessage, LoopQueueRegistry};

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// Build the canned JSON payload covering the §8.3 test matrix.
fn fixture_three_issues() -> String {
    // - #100: priority:critical, recent, good-first-issue, no body markers.
    // - #101: priority:medium, recent, no labels of consequence.
    // - #102: security, recent.
    r#"[
        {"number":100,"title":"Critical bug","body":"reproducible crash","labels":[{"name":"priority:critical"},{"name":"good-first-issue"}],"createdAt":"2099-01-01T00:00:00Z","url":"http://example/100","author":{"login":"jdmiranda"},"comments":[{},{},{}]},
        {"number":101,"title":"Tweak docs","body":"minor","labels":[{"name":"priority:medium"}],"createdAt":"2099-01-01T00:00:00Z","url":"http://example/101","author":{"login":"jdmiranda"},"comments":[]},
        {"number":102,"title":"Hardening","body":"important","labels":[{"name":"security"}],"createdAt":"2099-01-01T00:00:00Z","url":"http://example/102","author":{"login":"jdmiranda"},"comments":[]}
    ]"#
    .to_string()
}

/// Build an enabled state with rate limits relaxed so all candidates can
/// enqueue in a single tick (per-hour=10, cooldown=0). The audit-log path
/// is left `None` so the test does not touch the filesystem.
fn enabled_state() -> SelfImprovementState {
    SelfImprovementState::new(SelfImprovementConfig {
        enabled: true,
        per_hour: 10,
        per_day: 50,
        cooldown: Duration::ZERO,
        ..Default::default()
    })
}

// ---------------------------------------------------------------------------
// End-to-end integration
// ---------------------------------------------------------------------------

#[test]
fn high_priority_enqueues_medium_skipped_security_excluded() {
    let stub = StubGhRunner::new(vec![fixture_three_issues()]);
    let mut source = GhIssueGoalSource::with_runner(
        "jdmiranda/phantom",
        None,
        Duration::ZERO,
        Box::new(stub),
    );

    let mut state = enabled_state();
    let candidates = source.poll().expect("poll succeeds");
    assert_eq!(candidates.len(), 3);

    let registry = Arc::new(LoopQueueRegistry::new());
    let mut decisions = Vec::new();

    let t0 = Instant::now();
    for (i, c) in candidates.iter().enumerate() {
        let outcome = state.evaluate(c, t0 + Duration::from_secs(i as u64));
        decisions.push(outcome.audit.decision.clone());
        if let Some(AiAction::EnqueueLoopMessage {
            queue,
            from_source,
            payload,
        }) = outcome.action
        {
            // Forward to the stub registry the way the app handler would.
            registry.push(&queue, LoopMessage::new(from_source.clone(), payload));
        }
    }

    // §8.3 assertions:
    // - Issue #100 (priority:critical, good-first-issue) → enqueued
    // - Issue #101 (priority:medium) → low score (no critical floor, no CI signal)
    // - Issue #102 (security) → excluded
    assert!(matches!(decisions[0], AuditDecision::Enqueued));
    assert!(matches!(decisions[1], AuditDecision::SkippedLowScore));
    assert!(matches!(decisions[2], AuditDecision::SkippedExcluded));

    // Exactly one message must have landed on the implementer queue.
    let popped = registry.pop(DEFAULT_IMPLEMENTER_QUEUE);
    assert!(popped.is_some(), "expected one queued message");
    let msg = popped.unwrap();
    assert_eq!(msg.from_loop, "gh-issues");
    let payload = msg.payload;
    assert_eq!(
        payload["external_id"].as_str(),
        Some("gh-issue:100"),
        "payload must reference the critical issue"
    );
    assert_eq!(payload["source"].as_str(), Some("gh-issues"));
    assert!(payload["score"].as_f64().unwrap_or(0.0) >= 0.85);

    // No further messages — the queue must be drained now.
    assert!(registry.pop(DEFAULT_IMPLEMENTER_QUEUE).is_none());

    // The audit tail should hold exactly three entries (one per candidate).
    let tail = state.recent_audit_entries();
    assert_eq!(tail.len(), 3);
    assert_eq!(tail[2].decision, AuditDecision::SkippedExcluded);
    assert_eq!(tail[2].reason, "security label");
}

// ---------------------------------------------------------------------------
// Trust-budget feedback path
// ---------------------------------------------------------------------------

#[test]
fn pr_merged_feedback_bumps_trust_budget() {
    let mut state = enabled_state();
    let before = state.trust_budget().score();
    state.record_success();
    state.record_success();
    assert_eq!(state.trust_budget().score(), before + 2);
}

#[test]
fn failure_feedback_eventually_disables_auto_enqueue() {
    let mut state = enabled_state();
    // Drive the budget down to 0.
    for _ in 0..(state.trust_budget().score() + 1) {
        state.record_failure();
    }
    assert_eq!(state.trust_budget().band(), TrustBand::SuggestionOnly);

    // Now even a critical issue must be skipped.
    let stub = StubGhRunner::new(vec![fixture_three_issues()]);
    let mut source = GhIssueGoalSource::with_runner(
        "jdmiranda/phantom",
        None,
        Duration::ZERO,
        Box::new(stub),
    );
    let candidates = source.poll().unwrap();
    let outcome = state.evaluate(&candidates[0], Instant::now());
    assert_eq!(outcome.audit.decision, AuditDecision::SkippedSuggestionOnly);
    assert!(outcome.action.is_none());
}

// ---------------------------------------------------------------------------
// Rate limiting / adversarial spam
// ---------------------------------------------------------------------------

#[test]
fn fifty_phantom_brain_authored_issues_yield_zero_enqueues() {
    // Generate 50 issues all authored by "phantom-brain" (hard-exclusion #5).
    let mut json = String::from("[");
    for i in 0..50 {
        if i > 0 {
            json.push(',');
        }
        json.push_str(&format!(
            r#"{{"number":{},"title":"please review","body":"","labels":[],"createdAt":"2099-01-01T00:00:00Z","url":"http://x/{}","author":{{"login":"phantom-brain"}},"comments":[]}}"#,
            i + 1000,
            i + 1000,
        ));
    }
    json.push(']');
    let stub = StubGhRunner::new(vec![json]);
    let mut source = GhIssueGoalSource::with_runner(
        "jdmiranda/phantom",
        None,
        Duration::ZERO,
        Box::new(stub),
    );

    let mut state = enabled_state();
    let candidates = source.poll().unwrap();
    assert_eq!(candidates.len(), 50);

    let mut enqueued = 0;
    let t0 = Instant::now();
    for (i, c) in candidates.iter().enumerate() {
        let out = state.evaluate(c, t0 + Duration::from_secs(i as u64));
        if matches!(out.audit.decision, AuditDecision::Enqueued) {
            enqueued += 1;
        }
    }
    assert_eq!(
        enqueued, 0,
        "self-authored issues must be hard-excluded"
    );
}

#[test]
fn rate_limit_kicks_in_after_per_hour_cap() {
    // Same fixture but with per_hour=1 — only one critical issue enqueues.
    let mut state = SelfImprovementState::new(SelfImprovementConfig {
        enabled: true,
        per_hour: 1,
        per_day: 12,
        cooldown: Duration::ZERO,
        ..Default::default()
    });

    // Two distinct critical issues; both pass scoring.
    let json = r#"[
        {"number":200,"title":"A","body":"","labels":[{"name":"priority:critical"}],"createdAt":"2099-01-01T00:00:00Z","url":"http://x/200","author":{"login":"u"},"comments":[]},
        {"number":201,"title":"B","body":"","labels":[{"name":"priority:critical"}],"createdAt":"2099-01-01T00:00:00Z","url":"http://x/201","author":{"login":"u"},"comments":[]}
    ]"#;
    let stub = StubGhRunner::new(vec![json.into()]);
    let mut source = GhIssueGoalSource::with_runner(
        "jdmiranda/phantom",
        None,
        Duration::ZERO,
        Box::new(stub),
    );
    let candidates = source.poll().unwrap();
    let t0 = Instant::now();
    let r1 = state.evaluate(&candidates[0], t0);
    let r2 = state.evaluate(&candidates[1], t0 + Duration::from_secs(1));
    assert!(matches!(r1.audit.decision, AuditDecision::Enqueued));
    assert!(matches!(r2.audit.decision, AuditDecision::SkippedRateLimited));
    assert!(r2.audit.reason.contains("per-hour cap"));
}

// ---------------------------------------------------------------------------
// Source polling honors the gh CLI failure path without panic
// ---------------------------------------------------------------------------

#[test]
fn dependency_unavailable_surfaces_as_error_no_panic() {
    struct Fail;
    impl GhCommandRunner for Fail {
        fn run(&self, _args: &[String]) -> Result<String, GoalSourceError> {
            Err(GoalSourceError::DependencyUnavailable(
                "gh not installed".into(),
            ))
        }
    }
    let mut source = GhIssueGoalSource::with_runner(
        "jdmiranda/phantom",
        None,
        Duration::ZERO,
        Box::new(Fail),
    );
    let err = source.poll().unwrap_err();
    assert!(matches!(err, GoalSourceError::DependencyUnavailable(_)));
}

// ---------------------------------------------------------------------------
// JSONL audit log writes when audit_log_path is set
// ---------------------------------------------------------------------------

#[test]
fn jsonl_audit_log_is_written_when_path_configured() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("self-improvement-audit.jsonl");

    let mut state = SelfImprovementState::new(SelfImprovementConfig {
        enabled: true,
        per_hour: 10,
        per_day: 50,
        cooldown: Duration::ZERO,
        audit_log_path: Some(path.clone()),
        ..Default::default()
    });

    let stub = StubGhRunner::new(vec![fixture_three_issues()]);
    let mut source = GhIssueGoalSource::with_runner(
        "jdmiranda/phantom",
        None,
        Duration::ZERO,
        Box::new(stub),
    );
    let candidates = source.poll().unwrap();
    for (i, c) in candidates.iter().enumerate() {
        let _ = state.evaluate(c, Instant::now() + Duration::from_secs(i as u64));
    }

    let contents = std::fs::read_to_string(&path).expect("audit log written");
    let lines: Vec<&str> = contents.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 3, "one JSONL line per candidate");
    // Smoke-test that each line parses as JSON with the expected fields.
    for line in lines {
        let v: serde_json::Value =
            serde_json::from_str(line).expect("each line is valid JSON");
        assert!(v["external_id"].is_string());
        assert!(v["decision"].is_string());
        assert!(v["score_breakdown"].is_object());
    }
}

// ---------------------------------------------------------------------------
// Concurrent stub safety — make sure the registry/state types satisfy Send.
// ---------------------------------------------------------------------------

#[test]
fn types_are_send() {
    fn assert_send<T: Send>() {}
    assert_send::<LoopQueueRegistry>();
    assert_send::<SelfImprovementState>();
    assert_send::<Mutex<SelfImprovementState>>();
}
