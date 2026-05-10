//! Integration tests for the typed-quarantine-recovery mutator (issue #649).
//!
//! These tests cover [`TaskLedger::record_quarantine_failure`] across every
//! [`QuarantinePolicy`] variant and verify that the typed failure cause is
//! stamped onto the affected `PlanStep`(s). Read-API behaviour for the new
//! `failure_cause` and `quarantine_policy` fields is exercised here too so
//! downstream observers (inspector pane, reconciler) can rely on them.
//!
//! Coverage:
//! 1. Default policy is `FailAndAllowRetry`; `failure_cause` is `None`
//!    before any failure has been recorded.
//! 2. `record_quarantine_failure` with `FailAndAllowRetry` marks the
//!    step `Failed`, sets the typed cause, and clears `agent_id` for
//!    fresh-respawn semantics.
//! 3. `record_quarantine_failure` with `FailAndCascade` cascades to
//!    every transitive dependent, marking each `Skipped` with
//!    `DependencyFailed { dep_idx: idx }`.
//! 4. `record_quarantine_failure` with `Park` leaves the step `Active`
//!    and only stamps the typed cause for diagnostics.
//! 5. `record_quarantine_failure` on an out-of-bounds index returns
//!    `DispatchBlocked::OutOfBounds` and mutates no state.
//! 6. `PlanStep::record_failure` still stamps `StepFailureCause::AgentFailed`
//!    on the non-quarantine path so observers can disambiguate.

use phantom_agents::AgentTask;
use phantom_brain::orchestrator::{
    DispatchBlocked, PlanStep, QuarantinePolicy, StepFailureCause, StepStatus, TaskLedger,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn free_task(prompt: &str) -> AgentTask {
    AgentTask::FreeForm {
        prompt: prompt.into(),
    }
}

fn step(description: &str, depends_on: Vec<usize>) -> PlanStep {
    PlanStep::with_deps(description, free_task(description), depends_on)
}

/// Drive a step's status to [`StepStatus::Active`] via the guarded mutator
/// so the test fixture mirrors what production code observes: the step
/// holds an `agent_id`, has been incremented through the dispatch path,
/// and is then completed externally (success or quarantine-tagged
/// failure).
fn drive_to_active(ledger: &mut TaskLedger, idx: usize, agent_id: u64) {
    ledger
        .try_dispatch(idx)
        .expect("try_dispatch should succeed for a Pending step with no unmet deps");
    // The reconciler stamps `agent_id` after a successful dispatch; mirror
    // that side effect in the fixture so `record_quarantine_failure`'s
    // `agent_id` clearing is observable.
    ledger.plan[idx].agent_id = Some(agent_id);
    // Force the step status to `Active` for the Park policy test, since the
    // policy explicitly leaves the status as-is. `try_dispatch` already
    // transitioned to `Active`; this is redundant but documents intent.
    assert_eq!(ledger.plan[idx].status, StepStatus::Active);
}

// ---------------------------------------------------------------------------
// Test 1: defaults
// ---------------------------------------------------------------------------

#[test]
fn plan_step_defaults_are_failand_allow_retry_and_no_cause() {
    let s = PlanStep::new("only step", free_task("only step"));
    assert_eq!(s.quarantine_policy, QuarantinePolicy::FailAndAllowRetry);
    assert!(
        s.failure_cause.is_none(),
        "fresh step must not carry a failure cause"
    );
}

// ---------------------------------------------------------------------------
// Test 2: FailAndAllowRetry policy
// ---------------------------------------------------------------------------

#[test]
fn record_quarantine_failure_fail_and_allow_retry_marks_failed_and_clears_agent_id() {
    let mut ledger = TaskLedger::new("retry policy");
    ledger.set_plan(vec![step("s0", vec![])]);

    drive_to_active(&mut ledger, 0, 42);
    assert_eq!(ledger.plan[0].agent_id, Some(42));

    let returned = ledger
        .record_quarantine_failure(0, 42, 1_700_000_000_000)
        .expect("record_quarantine_failure must succeed for an in-range step");

    assert_eq!(returned.status, StepStatus::Failed);
    assert_eq!(
        returned.failure_cause,
        Some(StepFailureCause::AgentQuarantined {
            agent_id: 42,
            since_ms: 1_700_000_000_000,
        }),
    );
    // The agent_id is cleared so the next dispatch can spawn fresh.
    assert!(
        returned.agent_id.is_none(),
        "FailAndAllowRetry must clear agent_id"
    );
}

// ---------------------------------------------------------------------------
// Test 3: FailAndCascade policy — direct dependents
// ---------------------------------------------------------------------------

#[test]
fn record_quarantine_failure_cascade_marks_dependents_skipped() {
    let mut ledger = TaskLedger::new("cascade policy");
    // Build a fan-out: step 1 and step 2 both depend on step 0.
    let mut s0 = step("s0", vec![]);
    s0.quarantine_policy = QuarantinePolicy::FailAndCascade;
    let s1 = step("s1", vec![0]);
    let s2 = step("s2", vec![0]);
    ledger.set_plan(vec![s0, s1, s2]);

    drive_to_active(&mut ledger, 0, 7);
    ledger
        .record_quarantine_failure(0, 7, 1_000)
        .expect("cascade must accept the in-range index");

    assert_eq!(ledger.plan[0].status, StepStatus::Failed);
    assert_eq!(
        ledger.plan[0].failure_cause,
        Some(StepFailureCause::AgentQuarantined {
            agent_id: 7,
            since_ms: 1_000,
        }),
    );

    // Direct dependents are now `Skipped` with `DependencyFailed`.
    for &j in &[1usize, 2] {
        assert_eq!(
            ledger.plan[j].status,
            StepStatus::Skipped,
            "step {j} must be Skipped by the cascade"
        );
        assert_eq!(
            ledger.plan[j].failure_cause,
            Some(StepFailureCause::DependencyFailed { dep_idx: 0 }),
            "step {j} must carry DependencyFailed pointing at the originating failure"
        );
    }
}

// ---------------------------------------------------------------------------
// Test 4: FailAndCascade — transitive dependents
// ---------------------------------------------------------------------------

#[test]
fn record_quarantine_failure_cascade_propagates_transitively() {
    let mut ledger = TaskLedger::new("transitive cascade");
    // Linear chain: 0 → 1 → 2 → 3. Cascading at 0 should mark 1, 2, 3 all
    // Skipped.
    let mut s0 = step("s0", vec![]);
    s0.quarantine_policy = QuarantinePolicy::FailAndCascade;
    let s1 = step("s1", vec![0]);
    let s2 = step("s2", vec![1]);
    let s3 = step("s3", vec![2]);
    ledger.set_plan(vec![s0, s1, s2, s3]);

    drive_to_active(&mut ledger, 0, 99);
    ledger
        .record_quarantine_failure(0, 99, 2_000)
        .expect("cascade must accept the in-range index");

    assert_eq!(ledger.plan[0].status, StepStatus::Failed);
    for j in 1..=3 {
        assert_eq!(
            ledger.plan[j].status,
            StepStatus::Skipped,
            "step {j} must be Skipped by the transitive cascade"
        );
        assert_eq!(
            ledger.plan[j].failure_cause,
            Some(StepFailureCause::DependencyFailed { dep_idx: 0 }),
            "step {j} must point back at the originating failure (idx 0)"
        );
    }
}

// ---------------------------------------------------------------------------
// Test 5: FailAndCascade leaves terminal-status steps alone
// ---------------------------------------------------------------------------

#[test]
fn record_quarantine_failure_cascade_does_not_clobber_done_dependents() {
    let mut ledger = TaskLedger::new("done dependents");
    let mut s0 = step("s0", vec![]);
    s0.quarantine_policy = QuarantinePolicy::FailAndCascade;
    let s1 = step("s1", vec![0]);
    ledger.set_plan(vec![s0, s1]);

    // Pretend step 1 already completed independently (e.g. previously
    // ran with a relaxed dep policy). Cascade must leave it as `Done`.
    drive_to_active(&mut ledger, 0, 1);
    ledger.plan[1].status = StepStatus::Done;

    ledger
        .record_quarantine_failure(0, 1, 0)
        .expect("cascade must accept the in-range index");

    assert_eq!(ledger.plan[0].status, StepStatus::Failed);
    assert_eq!(
        ledger.plan[1].status,
        StepStatus::Done,
        "cascade must not reclassify a `Done` step"
    );
    assert!(
        ledger.plan[1].failure_cause.is_none(),
        "cascade must not stamp a cause on a completed step"
    );
}

// ---------------------------------------------------------------------------
// Test 6: Park policy
// ---------------------------------------------------------------------------

#[test]
fn record_quarantine_failure_park_leaves_status_unchanged_and_stamps_cause() {
    let mut ledger = TaskLedger::new("park policy");
    let mut s = step("only", vec![]);
    s.quarantine_policy = QuarantinePolicy::Park;
    ledger.set_plan(vec![s]);

    drive_to_active(&mut ledger, 0, 13);
    let prior_agent_id = ledger.plan[0].agent_id;

    let returned = ledger
        .record_quarantine_failure(0, 13, 3_141)
        .expect("park must accept the in-range index");

    assert_eq!(
        returned.status,
        StepStatus::Active,
        "Park must leave status untouched"
    );
    assert_eq!(
        returned.failure_cause,
        Some(StepFailureCause::AgentQuarantined {
            agent_id: 13,
            since_ms: 3_141,
        }),
    );
    assert_eq!(
        returned.agent_id, prior_agent_id,
        "Park must not clear agent_id — the reconciler may revive the step on quarantine release"
    );
}

// ---------------------------------------------------------------------------
// Test 7: out-of-bounds index
// ---------------------------------------------------------------------------

#[test]
fn record_quarantine_failure_rejects_out_of_bounds_index() {
    let mut ledger = TaskLedger::new("oob test");
    ledger.set_plan(vec![step("only", vec![])]);

    let err = ledger
        .record_quarantine_failure(7, 1, 0)
        .expect_err("OOB index must be rejected");

    assert_eq!(err, DispatchBlocked::OutOfBounds { idx: 7, len: 1 });
    // No state mutation: the in-range step is still Pending.
    assert_eq!(ledger.plan[0].status, StepStatus::Pending);
    assert!(ledger.plan[0].failure_cause.is_none());
}

// ---------------------------------------------------------------------------
// Test 8: non-quarantine record_failure stamps the AgentFailed cause
// ---------------------------------------------------------------------------

#[test]
fn record_failure_stamps_agent_failed_cause() {
    let mut step = PlanStep::new("flaky", free_task("flaky"));
    step.max_attempts = 1;
    // record_failure exhausts retries immediately (max_attempts = 1).
    assert!(!step.record_failure("flaked out"));
    assert_eq!(step.status, StepStatus::Failed);
    assert_eq!(step.failure_cause, Some(StepFailureCause::AgentFailed));
}

// ---------------------------------------------------------------------------
// Test 9: builder threads the policy through
// ---------------------------------------------------------------------------

#[test]
fn with_quarantine_policy_threads_policy_to_field() {
    let s = PlanStep::new("opt-in cascade", free_task("opt-in cascade"))
        .with_quarantine_policy(QuarantinePolicy::FailAndCascade);
    assert_eq!(s.quarantine_policy, QuarantinePolicy::FailAndCascade);
}
