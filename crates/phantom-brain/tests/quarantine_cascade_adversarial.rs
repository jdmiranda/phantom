//! Adversarial integration tests for [`TaskLedger::record_quarantine_failure`]
//! (issue #649) BFS cascade and per-policy semantics.
//!
//! Builds on the happy-path coverage in `quarantine_policy.rs`. These tests
//! poke at the cascade's BFS topology, policy persistence, agent_id-clearing
//! semantics, and isolation between unrelated subgraphs:
//!
//! 1. Diamond cascade — 4 steps where step 0 fans out to 1+2 and 3 depends
//!    on both 1 and 2. Cascade from step 0 must mark 1, 2, 3 all `Skipped`
//!    with `DependencyFailed`.
//! 2. Park policy persists — re-calling `record_quarantine_failure` on a
//!    `Park`-policy step is idempotent; status stays `Active`.
//! 3. FailAndAllowRetry clears `agent_id` — and the step ends in `Failed`
//!    (re-queueing is the caller's responsibility, not the ledger's).
//! 4. Cascade does not affect siblings — quarantining an unrelated step does
//!    not touch other independent subgraphs of the plan.

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

/// Drive a step's status to `Active` through the guarded mutator so the
/// fixture mirrors what production code observes.
fn drive_to_active(ledger: &mut TaskLedger, idx: usize, agent_id: u64) {
    ledger
        .try_dispatch(idx)
        .expect("try_dispatch should succeed for a Pending step with no unmet deps");
    ledger.plan[idx].agent_id = Some(agent_id);
}

// ---------------------------------------------------------------------------
// C1. Diamond cascade
// ---------------------------------------------------------------------------

#[test]
fn diamond_cascade_marks_every_transitive_dependent_skipped() {
    // Topology:
    //
    //   s0 ──┬── s1 ──┐
    //        └── s2 ──┴── s3
    //
    // Step 1 and step 2 both depend on step 0.
    // Step 3 depends on both step 1 and step 2.
    // Quarantining step 0 with FailAndCascade must propagate through
    // BOTH branches and mark s1, s2, s3 all `Skipped`.
    let mut ledger = TaskLedger::new("diamond cascade");

    let mut s0 = step("s0", vec![]);
    s0.quarantine_policy = QuarantinePolicy::FailAndCascade;
    let s1 = step("s1", vec![0]);
    let s2 = step("s2", vec![0]);
    let s3 = step("s3", vec![1, 2]);
    ledger.set_plan(vec![s0, s1, s2, s3]);

    drive_to_active(&mut ledger, 0, 42);
    ledger
        .record_quarantine_failure(0, 42, 1_700_000_000_000)
        .expect("cascade must accept the in-range index");

    // Step 0: Failed with AgentQuarantined cause.
    assert_eq!(ledger.plan[0].status, StepStatus::Failed);
    assert_eq!(
        ledger.plan[0].failure_cause,
        Some(StepFailureCause::AgentQuarantined {
            agent_id: 42,
            since_ms: 1_700_000_000_000,
        }),
    );
    assert!(
        ledger.plan[0].agent_id.is_none(),
        "FailAndCascade clears agent_id on the originating step too",
    );

    // Step 1 and step 2: Skipped with DependencyFailed { dep_idx: 0 }.
    for &j in &[1usize, 2] {
        assert_eq!(
            ledger.plan[j].status,
            StepStatus::Skipped,
            "step {j} (direct dependent) must be Skipped"
        );
        assert_eq!(
            ledger.plan[j].failure_cause,
            Some(StepFailureCause::DependencyFailed { dep_idx: 0 }),
            "step {j} must point back at the originating failure (idx 0)"
        );
    }

    // Step 3: transitively skipped. The `dep_idx` field is the origin of
    // the cascade walk (idx 0), per the cascade implementation. The walk
    // tracks the failed-set and stamps every cascaded step with the
    // originating index, not the proximate failed dep.
    assert_eq!(
        ledger.plan[3].status,
        StepStatus::Skipped,
        "step 3 (transitive dependent through both branches) must be Skipped"
    );
    assert_eq!(
        ledger.plan[3].failure_cause,
        Some(StepFailureCause::DependencyFailed { dep_idx: 0 }),
        "step 3 must point back at the originating failure (idx 0)"
    );
}

// ---------------------------------------------------------------------------
// C2. Park policy is idempotent
// ---------------------------------------------------------------------------

#[test]
fn park_policy_is_idempotent_on_repeat_calls() {
    let mut ledger = TaskLedger::new("park idempotent");
    let mut s = step("only", vec![]);
    s.quarantine_policy = QuarantinePolicy::Park;
    ledger.set_plan(vec![s]);

    drive_to_active(&mut ledger, 0, 7);

    // First call: status stays Active, cause is stamped.
    ledger
        .record_quarantine_failure(0, 7, 1_000)
        .expect("park must accept the in-range index");
    assert_eq!(ledger.plan[0].status, StepStatus::Active);
    assert_eq!(
        ledger.plan[0].failure_cause,
        Some(StepFailureCause::AgentQuarantined {
            agent_id: 7,
            since_ms: 1_000,
        }),
    );
    assert_eq!(
        ledger.plan[0].agent_id,
        Some(7),
        "Park policy must NOT clear agent_id"
    );

    // Second call with the same agent_id: idempotent — status still Active,
    // cause refreshed but otherwise no change. The reconciler may invoke
    // the path multiple times before the quarantine clears.
    ledger
        .record_quarantine_failure(0, 7, 2_000)
        .expect("park policy must accept repeat calls");
    assert_eq!(ledger.plan[0].status, StepStatus::Active);
    assert_eq!(
        ledger.plan[0].failure_cause,
        Some(StepFailureCause::AgentQuarantined {
            agent_id: 7,
            since_ms: 2_000,
        }),
        "the typed cause must reflect the most recent since_ms"
    );
    assert_eq!(
        ledger.plan[0].agent_id,
        Some(7),
        "agent_id remains stable across repeat Park calls"
    );

    // Third call with a different agent_id: still idempotent on status.
    ledger
        .record_quarantine_failure(0, 99, 3_000)
        .expect("park policy must accept a third call with a new agent_id");
    assert_eq!(ledger.plan[0].status, StepStatus::Active);
    assert_eq!(
        ledger.plan[0].failure_cause,
        Some(StepFailureCause::AgentQuarantined {
            agent_id: 99,
            since_ms: 3_000,
        }),
    );
}

// ---------------------------------------------------------------------------
// C3. FailAndAllowRetry clears agent_id; step is Failed
// ---------------------------------------------------------------------------

#[test]
fn fail_and_allow_retry_clears_agent_id_and_marks_failed() {
    // Verify the documented contract for FailAndAllowRetry: the step ends
    // up in `Failed` status with `agent_id` cleared. The step is NOT
    // automatically re-queued to `Pending` — that is the caller's
    // responsibility through the retry budget. Verify both invariants:
    let mut ledger = TaskLedger::new("retry policy");
    ledger.set_plan(vec![step("only", vec![])]);

    drive_to_active(&mut ledger, 0, 13);
    assert_eq!(ledger.plan[0].agent_id, Some(13));

    let returned = ledger
        .record_quarantine_failure(0, 13, 100)
        .expect("retry policy must accept the in-range index");

    // Status is Failed.
    assert_eq!(returned.status, StepStatus::Failed);
    // agent_id is cleared so the next dispatch can spawn fresh.
    assert!(
        returned.agent_id.is_none(),
        "FailAndAllowRetry must clear agent_id on the quarantined step"
    );
    // Cause is the typed quarantine variant.
    assert_eq!(
        returned.failure_cause,
        Some(StepFailureCause::AgentQuarantined {
            agent_id: 13,
            since_ms: 100,
        }),
    );

    // A subsequent `try_dispatch` cannot succeed: the step is Failed, not
    // Pending. PR #657's chosen semantics: the ledger preserves Failed
    // until external caller flips status (retry budget machinery).
    let dispatch_err = ledger
        .try_dispatch(0)
        .expect_err("a Failed step must not dispatch via try_dispatch");
    assert_eq!(
        dispatch_err,
        DispatchBlocked::NotPending {
            idx: 0,
            current: StepStatus::Failed,
        },
        "FailAndAllowRetry leaves the step Failed — caller must re-queue explicitly"
    );
}

// ---------------------------------------------------------------------------
// C4. Cascade does not affect siblings of an unrelated subgraph
// ---------------------------------------------------------------------------

#[test]
fn cascade_does_not_affect_unrelated_subgraphs() {
    // Topology:
    //
    //   subgraph A (cascade origin):  s_d ──── s_e
    //   subgraph B (untouched):       s_c ──── s_a
    //                                       └── s_b
    //
    // Quarantining s_d (with FailAndCascade) must mark s_e Skipped but
    // leave s_c, s_a, s_b completely untouched — they share no dependency
    // edges with the cascade origin.
    let mut ledger = TaskLedger::new("isolated subgraphs");

    // Subgraph B first so its indices are 0, 1, 2.
    let s_c = step("s_c", vec![]); // idx 0
    let s_a = step("s_a", vec![0]); // idx 1, depends on s_c
    let s_b = step("s_b", vec![0]); // idx 2, depends on s_c

    // Subgraph A with cascade policy on the root.
    let mut s_d = step("s_d", vec![]); // idx 3
    s_d.quarantine_policy = QuarantinePolicy::FailAndCascade;
    let s_e = step("s_e", vec![3]); // idx 4, depends on s_d

    ledger.set_plan(vec![s_c, s_a, s_b, s_d, s_e]);

    // Capture original statuses for the unrelated subgraph.
    let original_statuses: Vec<StepStatus> =
        (0..3).map(|i| ledger.plan[i].status).collect();

    drive_to_active(&mut ledger, 3, 77);
    ledger
        .record_quarantine_failure(3, 77, 5_000)
        .expect("cascade origin must succeed");

    // Subgraph A: s_d Failed, s_e Skipped.
    assert_eq!(ledger.plan[3].status, StepStatus::Failed);
    assert_eq!(ledger.plan[4].status, StepStatus::Skipped);
    assert_eq!(
        ledger.plan[4].failure_cause,
        Some(StepFailureCause::DependencyFailed { dep_idx: 3 }),
    );

    // Subgraph B: every step's status and failure_cause must be exactly
    // as it was before the cascade. The BFS is correctly scoped.
    for i in 0..3 {
        assert_eq!(
            ledger.plan[i].status, original_statuses[i],
            "step {i} (unrelated subgraph) must keep its original status"
        );
        assert!(
            ledger.plan[i].failure_cause.is_none(),
            "step {i} (unrelated subgraph) must not carry any failure_cause"
        );
    }
}

// ---------------------------------------------------------------------------
// C5. OOB index is rejected and no state is mutated
// ---------------------------------------------------------------------------

#[test]
fn cascade_oob_index_does_not_mutate_state() {
    // Defensive: an OOB index returns Err(OutOfBounds) and must not
    // mutate ANY step. Companion to `quarantine_policy.rs`'s OOB test
    // but specifically checks the cascade walk is not triggered.
    let mut ledger = TaskLedger::new("oob cascade");

    let mut s0 = step("s0", vec![]);
    s0.quarantine_policy = QuarantinePolicy::FailAndCascade;
    let s1 = step("s1", vec![0]);
    ledger.set_plan(vec![s0, s1]);

    let err = ledger
        .record_quarantine_failure(999, 1, 0)
        .expect_err("OOB index must reject");
    assert_eq!(err, DispatchBlocked::OutOfBounds { idx: 999, len: 2 });

    // No cascade ran: every step is still Pending and clean.
    for i in 0..2 {
        assert_eq!(ledger.plan[i].status, StepStatus::Pending);
        assert!(ledger.plan[i].failure_cause.is_none());
    }
}
