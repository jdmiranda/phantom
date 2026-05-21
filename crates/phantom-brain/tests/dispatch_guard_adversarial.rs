//! Adversarial integration tests for [`TaskLedger::try_dispatch`] (issue #647).
//!
//! Builds on the happy-path coverage in `dispatch_guard.rs`. These tests
//! exercise edge cases the orchestrator's `try_dispatch` guard must handle
//! correctly:
//!
//! 1. Empty `depends_on` — a step with no deps must dispatch.
//! 2. Self-dependency — verifies behavior when a step lists its own index.
//! 3. Out-of-bounds dep index — per the spike, treated as satisfied.
//! 4. Dep reverted mid-flight — a previously-Done dep flipped back must
//!    re-block dispatch.
//! 5. Diamond DAG — 4 steps with a fan-out / fan-in topology; verifies
//!    sequential dispatchability with partial dep completion.
//! 6. Double dispatch — the second call must reject with `NotPending`.

use phantom_agents::AgentTask;
use phantom_brain::orchestrator::{DispatchBlocked, PlanStep, StepStatus, TaskLedger};

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

// ---------------------------------------------------------------------------
// B1. Empty `depends_on` succeeds
// ---------------------------------------------------------------------------

#[test]
fn try_dispatch_with_empty_depends_on_succeeds() {
    let mut ledger = TaskLedger::new("empty deps");
    ledger.set_plan(vec![step("solo", vec![])]);

    // A step with `depends_on = []` and `Pending` status must dispatch
    // successfully on the first call.
    let returned = ledger
        .try_dispatch(0)
        .expect("empty depends_on must dispatch immediately");
    assert_eq!(returned.status, StepStatus::Active);
    assert!(
        returned.depends_on().is_empty(),
        "the returned step must carry its empty depends_on unchanged"
    );
    assert_eq!(ledger.plan[0].status, StepStatus::Active);
}

// ---------------------------------------------------------------------------
// B2. Self-dependency
// ---------------------------------------------------------------------------

#[test]
fn try_dispatch_self_dependency_is_blocked_by_unmet_deps() {
    // A step that depends on itself can never be satisfied: the dep at idx 0
    // is in-bounds and references itself, and its status is `Pending` (not
    // `Done`). Per `deps_satisfied`, the open set must include the
    // self-index. `set_plan` detects the cycle and marks the step `Failed`,
    // which then causes `try_dispatch` to reject with `NotPending`.
    let mut ledger = TaskLedger::new("self dep");
    ledger.set_plan(vec![step("self", vec![0])]);

    // `set_plan` runs `has_cycle()` — a self-loop is a cycle, so the step is
    // marked `Failed` rather than left in `Pending`.
    assert_eq!(
        ledger.plan[0].status,
        StepStatus::Failed,
        "set_plan's cycle detector must mark a self-dependent step Failed"
    );

    let err = ledger
        .try_dispatch(0)
        .expect_err("a self-dependent step must not dispatch");
    // The cycle detector pre-empts the unmet-deps check, so the rejection
    // shape is `NotPending { current: Failed }`.
    assert_eq!(
        err,
        DispatchBlocked::NotPending {
            idx: 0,
            current: StepStatus::Failed,
        },
        "self-dependency must surface as NotPending after cycle detection promotes Failed"
    );
}

// ---------------------------------------------------------------------------
// B3. Out-of-bounds dep index — treated as satisfied
// ---------------------------------------------------------------------------

#[test]
fn try_dispatch_oob_dep_index_is_treated_as_satisfied() {
    // Per `deps_satisfied`'s documented contract: out-of-bounds dep indices
    // are silently filtered out and treated as satisfied. A step whose only
    // dep is OOB must therefore dispatch successfully on a fresh ledger.
    let mut ledger = TaskLedger::new("oob deps");
    ledger.set_plan(vec![step("ghost dep", vec![999_999])]);

    // `has_cycle()` skips OOB indices, so the cycle detector does not flag
    // this step.
    assert_eq!(ledger.plan[0].status, StepStatus::Pending);

    let returned = ledger
        .try_dispatch(0)
        .expect("OOB deps must be treated as satisfied");
    assert_eq!(returned.status, StepStatus::Active);
    assert_eq!(ledger.plan[0].status, StepStatus::Active);
}

// ---------------------------------------------------------------------------
// B4. Dep reverted mid-flight
// ---------------------------------------------------------------------------

#[test]
fn try_dispatch_rejects_when_dep_reverted_between_calls() {
    // Scenario: `eligible_next()` observes step 1 as runnable because step 0
    // is `Done`. Before `try_dispatch(1)` runs, external code reverts step 0
    // back to `Pending` (e.g. a manual retry was wired in, or a test
    // fixture forced it). The guard must re-check on dispatch and reject
    // with the current open dep set rather than racing through.
    let mut ledger = TaskLedger::new("dep revert");
    ledger.set_plan(vec![step("s0", vec![]), step("s1", vec![0])]);

    // Mark step 0 Done so step 1 looks dispatchable.
    ledger.plan[0].status = StepStatus::Done;
    assert!(
        ledger
            .eligible_next()
            .iter()
            .any(|(i, _)| *i == 1),
        "step 1 must be reported eligible while step 0 is Done"
    );

    // Revert step 0 to Pending (simulating mid-poll retry or manual rollback).
    ledger.plan[0].status = StepStatus::Pending;

    // The guard must observe the reverted dep and reject with the current
    // open list, not blindly trust the eligibility read from earlier.
    let err = ledger
        .try_dispatch(1)
        .expect_err("dep reverted to Pending must block dispatch");
    assert_eq!(
        err,
        DispatchBlocked::UnmetDeps {
            idx: 1,
            open: vec![0],
        },
        "the rejection must carry the freshly-recomputed open list"
    );
    assert_eq!(
        ledger.plan[1].status,
        StepStatus::Pending,
        "no state mutation occurs on rejection"
    );
}

// ---------------------------------------------------------------------------
// B5. Diamond DAG
// ---------------------------------------------------------------------------

#[test]
fn try_dispatch_diamond_dag_sequences_correctly() {
    // Topology:
    //
    //   s0 ──┬── s1 ──┐
    //        └── s2 ──┴── s3
    //
    // Step 1 and step 2 both depend on step 0.
    // Step 3 depends on both step 1 and step 2.
    let mut ledger = TaskLedger::new("diamond");
    ledger.set_plan(vec![
        step("s0", vec![]),
        step("s1", vec![0]),
        step("s2", vec![0]),
        step("s3", vec![1, 2]),
    ]);

    // Initially only step 0 can dispatch — step 1 and 2 need 0, step 3
    // needs both 1 and 2.
    let returned_0 = ledger.try_dispatch(0).expect("step 0 must dispatch");
    assert_eq!(returned_0.status, StepStatus::Active);

    // Step 1 / 2 / 3 are still blocked because step 0 is `Active`, not
    // `Done`. Verify all three reject with the right open lists.
    let err_1 = ledger.try_dispatch(1).expect_err("step 1 blocked on step 0");
    assert_eq!(err_1, DispatchBlocked::UnmetDeps { idx: 1, open: vec![0] });

    // Mark step 0 Done so the dep is satisfied for 1 and 2.
    ledger.plan[0].status = StepStatus::Done;

    // Now step 1 and step 2 both dispatch.
    let returned_1 = ledger.try_dispatch(1).expect("step 1 must dispatch after 0 done");
    assert_eq!(returned_1.status, StepStatus::Active);
    let returned_2 = ledger.try_dispatch(2).expect("step 2 must dispatch after 0 done");
    assert_eq!(returned_2.status, StepStatus::Active);

    // Step 3 is still blocked: both step 1 and 2 are `Active`, not `Done`.
    let err_3 = ledger
        .try_dispatch(3)
        .expect_err("step 3 blocked on still-Active step 1 and step 2");
    let DispatchBlocked::UnmetDeps { idx, open } = err_3 else {
        panic!("expected UnmetDeps, got {err_3:?}");
    };
    assert_eq!(idx, 3);
    // Both deps are open; the order is preserved from `depends_on`.
    assert_eq!(open, vec![1, 2]);

    // Mark step 1 Done — step 3 is still blocked by step 2.
    ledger.plan[1].status = StepStatus::Done;
    let err_3_partial = ledger.try_dispatch(3).expect_err("step 3 still blocked on step 2");
    let DispatchBlocked::UnmetDeps { open, .. } = err_3_partial else {
        panic!("expected UnmetDeps after partial completion");
    };
    assert_eq!(open, vec![2]);

    // Mark step 2 Done — step 3 finally dispatches.
    ledger.plan[2].status = StepStatus::Done;
    let returned_3 = ledger.try_dispatch(3).expect("step 3 must dispatch after both deps done");
    assert_eq!(returned_3.status, StepStatus::Active);
    assert_eq!(ledger.plan[3].status, StepStatus::Active);
}

// ---------------------------------------------------------------------------
// B6. Double-dispatch of same step
// ---------------------------------------------------------------------------

#[test]
fn try_dispatch_double_call_rejects_with_not_pending() {
    let mut ledger = TaskLedger::new("double dispatch");
    ledger.set_plan(vec![step("solo", vec![])]);

    // First call transitions Pending -> Active.
    let returned = ledger.try_dispatch(0).expect("first dispatch must succeed");
    assert_eq!(returned.status, StepStatus::Active);

    // Second call must reject — the step is no longer Pending.
    let err = ledger
        .try_dispatch(0)
        .expect_err("second dispatch on the same step must reject");
    assert_eq!(
        err,
        DispatchBlocked::NotPending {
            idx: 0,
            current: StepStatus::Active,
        },
        "the rejection must carry the actual current status (Active)"
    );

    // The step's status is unchanged by the failed second call.
    assert_eq!(ledger.plan[0].status, StepStatus::Active);
}

// ---------------------------------------------------------------------------
// B7. Cascade of empty depends_on for many fresh steps
// ---------------------------------------------------------------------------

#[test]
fn try_dispatch_many_independent_steps_all_dispatch() {
    // Defensive: a plan of N independent steps (no deps between any of
    // them) must allow N successful dispatches. This guards against
    // accidental ordering coupling in the guard.
    let mut ledger = TaskLedger::new("independents");
    let n = 5usize;
    let plan: Vec<PlanStep> = (0..n).map(|i| step(&format!("s{i}"), vec![])).collect();
    ledger.set_plan(plan);

    for i in 0..n {
        let returned = ledger
            .try_dispatch(i)
            .unwrap_or_else(|e| panic!("step {i} must dispatch but got {e:?}"));
        assert_eq!(returned.status, StepStatus::Active);
    }
    for i in 0..n {
        assert_eq!(ledger.plan[i].status, StepStatus::Active);
    }
}
