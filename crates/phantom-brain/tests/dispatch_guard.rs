//! Integration tests for the dispatch guard introduced in Issue #647.
//!
//! [`TaskLedger::try_dispatch`] is the only path that should transition a
//! [`PlanStep`] into [`StepStatus::Active`] in production. These tests assert:
//!
//! 1. Out-of-bounds indices are rejected with [`DispatchBlocked::OutOfBounds`].
//! 2. Steps with unmet `depends_on` indices are rejected with
//!    [`DispatchBlocked::UnmetDeps`] and the open dep list is reported.
//! 3. A step whose deps are all `Done` succeeds and transitions to `Active`.
//! 4. The reconciler dispatches a previously-blocked dependent step on the
//!    tick after its dependency completes — exercising the migrated
//!    `dispatch_pending` path that now goes through `try_dispatch`.

use std::sync::mpsc;

use phantom_agents::AgentTask;
use phantom_brain::events::AiAction;
use phantom_brain::reconciler::ReconcilerState;
use phantom_brain::{DispatchBlocked, PlanStep, StepStatus, TaskLedger};

fn free_task(prompt: &str) -> AgentTask {
    AgentTask::FreeForm {
        prompt: prompt.into(),
    }
}

/// Build a ledger with three independent (no-deps) steps, used as a baseline
/// for the OOB / status mismatch tests.
fn make_three_step_ledger() -> TaskLedger {
    let mut ledger = TaskLedger::new("dispatch guard test goal");
    ledger.set_plan(vec![
        PlanStep::new("step-0", free_task("zero")),
        PlanStep::new("step-1", free_task("one")),
        PlanStep::new("step-2", free_task("two")),
    ]);
    ledger
}

// ---------------------------------------------------------------------------
// Test 1: out-of-bounds index → DispatchBlocked::OutOfBounds
// ---------------------------------------------------------------------------

#[test]
fn try_dispatch_rejects_out_of_bounds_index() {
    let mut ledger = make_three_step_ledger();
    let len = ledger.plan.len();

    let err = ledger
        .try_dispatch(99)
        .expect_err("OOB index must be rejected");

    assert_eq!(
        err,
        DispatchBlocked::OutOfBounds { idx: 99, len },
        "guard must report the offending idx and current plan length"
    );

    // Ledger must be untouched.
    for (i, step) in ledger.plan.iter().enumerate() {
        assert_eq!(
            step.status,
            StepStatus::Pending,
            "step {i} must remain Pending after rejected OOB dispatch"
        );
    }
}

// ---------------------------------------------------------------------------
// Test 2: unmet deps → DispatchBlocked::UnmetDeps with the open list
// ---------------------------------------------------------------------------

#[test]
fn try_dispatch_rejects_step_with_unmet_deps() {
    let mut ledger = TaskLedger::new("unmet deps test");
    // Plan: step 2 depends on steps 0 and 1, both still Pending.
    ledger.set_plan(vec![
        PlanStep::new("root-a", free_task("a")),
        PlanStep::new("root-b", free_task("b")),
        PlanStep::with_deps("dependent", free_task("dep"), vec![0, 1]),
    ]);

    let err = ledger
        .try_dispatch(2)
        .expect_err("dependent step with two unmet deps must be rejected");

    match err {
        DispatchBlocked::UnmetDeps { idx, mut open } => {
            assert_eq!(idx, 2);
            open.sort_unstable();
            assert_eq!(
                open,
                vec![0, 1],
                "open dep list must enumerate the unmet in-bounds deps"
            );
        }
        other => panic!("expected UnmetDeps, got {other:?}"),
    }

    // The dependent step must remain Pending.
    assert_eq!(ledger.plan[2].status, StepStatus::Pending);

    // Mark dep 0 done — dep 1 is still Pending, so the guard should still
    // refuse, this time with `open = [1]`.
    ledger.plan[0].status = StepStatus::Done;
    let err = ledger
        .try_dispatch(2)
        .expect_err("dependent step with one remaining unmet dep must be rejected");
    match err {
        DispatchBlocked::UnmetDeps { idx, open } => {
            assert_eq!(idx, 2);
            assert_eq!(open, vec![1], "only dep 1 should remain open");
        }
        other => panic!("expected UnmetDeps, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Test 3: deps satisfied → Active transition
// ---------------------------------------------------------------------------

#[test]
fn try_dispatch_succeeds_when_deps_satisfied() {
    let mut ledger = TaskLedger::new("satisfied deps test");
    ledger.set_plan(vec![
        PlanStep::new("root", free_task("root")),
        PlanStep::with_deps("leaf", free_task("leaf"), vec![0]),
    ]);

    // Mark the root done so the leaf becomes eligible.
    ledger.plan[0].status = StepStatus::Done;

    {
        let dispatched = ledger
            .try_dispatch(1)
            .expect("leaf must dispatch once its dep is Done");
        assert_eq!(dispatched.description, "leaf");
        assert_eq!(dispatched.status, StepStatus::Active);
    }

    assert_eq!(
        ledger.plan[1].status,
        StepStatus::Active,
        "successful try_dispatch must transition the step to Active"
    );

    // A second try_dispatch on the same idx must now fail with NotPending,
    // confirming the guard rejects double-dispatch.
    let err = ledger
        .try_dispatch(1)
        .expect_err("re-dispatch of an Active step must be rejected");
    match err {
        DispatchBlocked::NotPending { idx, current } => {
            assert_eq!(idx, 1);
            assert_eq!(current, StepStatus::Active);
        }
        other => panic!("expected NotPending, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Test 4: end-to-end reconciler path — step 1 unblocks after step 0 is Done
// ---------------------------------------------------------------------------

#[test]
fn reconciler_dispatches_dependent_step_via_try_dispatch() {
    let (tx, rx) = mpsc::channel::<AiAction>();
    let mut state = ReconcilerState::new();

    let mut ledger = TaskLedger::new("reconciler integration");
    ledger.set_plan(vec![
        PlanStep::new("root", free_task("root")),
        PlanStep::with_deps("dependent", free_task("dependent"), vec![0]),
    ]);

    // Pre-condition: root is Pending (needs to be dispatched), dependent is
    // Pending and blocked. We simulate root having already completed by
    // marking it Done directly (the reconciler-side `on_agent_complete`
    // path is exercised in the existing reconciler tests). The point of
    // *this* test is: with root Done, can the reconciler dispatch the
    // previously-blocked dependent step through the new try_dispatch route?
    ledger.plan[0].status = StepStatus::Done;

    // Sanity: dependent is still Pending before tick.
    assert_eq!(ledger.plan[1].status, StepStatus::Pending);

    // Tick the reconciler — it should walk eligible_next, find step 1, and
    // dispatch it via try_dispatch (the migrated production path).
    state.tick(&mut ledger, &tx);

    assert_eq!(
        ledger.plan[1].status,
        StepStatus::Active,
        "reconciler must transition step 1 to Active via try_dispatch \
         once its dependency on step 0 is satisfied"
    );

    // The reconciler must have stamped agent_id and incremented attempts
    // (these fields are owned by the reconciler, not by try_dispatch).
    assert!(
        ledger.plan[1].agent_id.is_some(),
        "reconciler must set agent_id after a successful try_dispatch"
    );
    assert_eq!(
        ledger.plan[1].attempts, 1,
        "reconciler must increment attempts after a successful try_dispatch"
    );

    // Exactly one SpawnAgent should have been emitted (for step 1; step 0
    // was already Done before the tick).
    let action = rx.try_recv().expect("reconciler must emit SpawnAgent");
    assert!(
        matches!(action, AiAction::SpawnAgent { .. }),
        "expected SpawnAgent, got {action:?}"
    );
    assert!(
        rx.try_recv().is_err(),
        "no further actions should be emitted in this tick"
    );
}
