//! Integration tests for the `try_dispatch` guarded mutator (issue #647).
//!
//! These tests exercise the runtime enforcement of `PlanStep.depends_on`
//! at the orchestrator dispatch boundary. Before this guard existed, the
//! reconciler set `s.status = StepStatus::Active` directly, bypassing the
//! dependency check that `eligible_next()` advertised at the read API.
//!
//! Coverage:
//! 1. Out-of-bounds index is rejected with `DispatchBlocked::OutOfBounds`.
//! 2. A step with unmet deps is rejected with `DispatchBlocked::UnmetDeps`;
//!    the `open` set shrinks as dependencies complete.
//! 3. A step with satisfied deps transitions to `Active`; a second call
//!    on the same step is rejected with `DispatchBlocked::NotPending`.
//! 4. End-to-end through `ReconcilerState::tick`: step 1 depends on step 0,
//!    and is dispatched only after step 0 reaches `Done`.

use std::sync::mpsc;

use phantom_agents::AgentTask;
use phantom_brain::events::AiAction;
use phantom_brain::orchestrator::{DispatchBlocked, PlanStep, StepStatus, TaskLedger};
use phantom_brain::reconciler::ReconcilerState;

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
// Test 1: Out-of-bounds index
// ---------------------------------------------------------------------------

#[test]
fn try_dispatch_rejects_out_of_bounds_index() {
    let mut ledger = TaskLedger::new("oob test");
    ledger.set_plan(vec![step("only step", vec![])]);

    let err = ledger
        .try_dispatch(7)
        .expect_err("try_dispatch must refuse an OOB index");

    assert_eq!(
        err,
        DispatchBlocked::OutOfBounds { idx: 7, len: 1 },
        "OOB rejection must carry both the requested idx and the plan length",
    );

    // No state mutation: the in-range step is still Pending.
    assert_eq!(ledger.plan[0].status, StepStatus::Pending);
}

// ---------------------------------------------------------------------------
// Test 2: Unmet dependencies
// ---------------------------------------------------------------------------

#[test]
fn try_dispatch_rejects_step_with_unmet_deps() {
    let mut ledger = TaskLedger::new("deps test");
    // Step 2 depends on both steps 0 and 1.
    ledger.set_plan(vec![
        step("s0", vec![]),
        step("s1", vec![]),
        step("s2", vec![0, 1]),
    ]);

    // Initially both deps are open.
    let err = ledger.try_dispatch(2).expect_err("deps unmet");
    let DispatchBlocked::UnmetDeps { idx, open } = err else {
        panic!("expected UnmetDeps, got {err:?}");
    };
    assert_eq!(idx, 2);
    assert_eq!(open, vec![0, 1], "both deps must be reported open");
    assert_eq!(ledger.plan[2].status, StepStatus::Pending);

    // Complete step 0 — the open set shrinks to just [1].
    ledger.plan[0].status = StepStatus::Done;
    let err = ledger.try_dispatch(2).expect_err("dep 1 still open");
    let DispatchBlocked::UnmetDeps { open, .. } = err else {
        panic!("expected UnmetDeps after one dep done");
    };
    assert_eq!(open, vec![1], "only the still-open dep should be reported");

    // Complete step 1 — now `try_dispatch` succeeds and flips status.
    ledger.plan[1].status = StepStatus::Done;
    ledger
        .try_dispatch(2)
        .expect("all deps satisfied, dispatch must succeed");
    assert_eq!(ledger.plan[2].status, StepStatus::Active);
}

// ---------------------------------------------------------------------------
// Test 3: Successful dispatch is single-shot
// ---------------------------------------------------------------------------

#[test]
fn try_dispatch_succeeds_when_deps_satisfied() {
    let mut ledger = TaskLedger::new("happy path");
    ledger.set_plan(vec![step("solo", vec![])]);

    let returned = ledger
        .try_dispatch(0)
        .expect("step with no deps must dispatch");
    assert_eq!(returned.description, "solo");
    assert_eq!(returned.status, StepStatus::Active);
    // The mutation is observable on the ledger itself.
    assert_eq!(ledger.plan[0].status, StepStatus::Active);

    // A second dispatch on the same index must be rejected — the step
    // is no longer `Pending`, it is `Active`.
    let err = ledger
        .try_dispatch(0)
        .expect_err("second dispatch must be rejected");
    assert_eq!(
        err,
        DispatchBlocked::NotPending {
            idx: 0,
            current: StepStatus::Active,
        },
        "the rejection must carry the actual current status",
    );
}

// ---------------------------------------------------------------------------
// Test 4: End-to-end through the reconciler
// ---------------------------------------------------------------------------

#[test]
fn reconciler_dispatches_dependent_step_via_try_dispatch() {
    let (tx, rx) = mpsc::channel();
    let mut state = ReconcilerState::new();
    let mut ledger = TaskLedger::new("e2e");
    ledger.set_plan(vec![
        step("step 0", vec![]),
        step("step 1", vec![0]),
    ]);

    // First tick — step 0 is dispatched, step 1 is blocked by deps.
    state.tick(&mut ledger, &tx);
    let first = rx
        .try_recv()
        .expect("first SpawnAgent must be emitted for step 0");
    assert!(matches!(first, AiAction::SpawnAgent { .. }));
    assert!(
        rx.try_recv().is_err(),
        "only step 0 should dispatch while step 1's dep is unmet",
    );
    assert_eq!(ledger.plan[0].status, StepStatus::Active);
    assert_eq!(ledger.plan[1].status, StepStatus::Pending);
    let step0_agent = ledger.plan[0]
        .agent_id
        .expect("agent_id must be stamped on dispatch");
    assert_eq!(ledger.plan[0].attempts, 1, "attempts must be incremented");

    // Complete step 0 through the standard reconciler path so the active
    // dispatch is cleared and step 0 transitions to Done. After this,
    // step 1 is the only eligible step and its dep is satisfied.
    state.on_agent_complete(&mut ledger, 1, true, "ok", Some(step0_agent));
    assert_eq!(ledger.plan[0].status, StepStatus::Done);
    // Drain any pending notifications from the completion path so the
    // next assertion is unambiguous.
    while rx.try_recv().is_ok() {}

    state.tick(&mut ledger, &tx);
    let second = rx
        .try_recv()
        .expect("second SpawnAgent must be emitted for step 1");
    assert!(matches!(second, AiAction::SpawnAgent { .. }));
    assert!(
        rx.try_recv().is_err(),
        "exactly one SpawnAgent per dispatched step",
    );
    assert_eq!(
        ledger.plan[1].status,
        StepStatus::Active,
        "step 1 must be Active after its dep is satisfied",
    );
    assert!(
        ledger.plan[1].agent_id.is_some(),
        "reconciler must stamp agent_id on the dependent step",
    );
    assert_eq!(
        ledger.plan[1].attempts, 1,
        "attempts must be incremented exactly once on dispatch",
    );
}
