//! Autonomous agent lifecycle reconciler.
//!
//! The reconciler is a proactive OODA tick — called every 3 seconds from the
//! brain loop's timeout — that drives the [`TaskLedger`] forward without
//! waiting to be explicitly asked. It is the single authority for "what should
//! happen next" in a multi-step goal.
//!
//! # What it does each tick
//!
//! 1. **Check stalled agents** — if an active dispatch has exceeded
//!    `stall_timeout`, record a failure on that step (allowing retries) or
//!    flatline it if retries are exhausted.
//! 2. **Evaluate the ledger** — call `should_replan()` to check for
//!    completion, stalls, or loop detection.
//! 3. **Dispatch the next step** — if the ledger says continue and no agent
//!    is currently active, emit `AiAction::SpawnAgent` for the next pending
//!    step.
//!
//! # Why a separate module
//!
//! The brain's OODA loop is event-driven: it reacts to terminal output,
//! user commands, and agent completions. The reconciler is time-driven: it
//! asks "given the current ledger state, is there something we should be
//! doing right now?" Keeping these two concerns separate prevents the OODA
//! loop from being cluttered with lifecycle polling logic.
//!
//! # Forge lineage
//!
//! This is the Phantom equivalent of Forge-gmh's `reconciler.rs` background
//! thread, adapted to run inside the brain's existing 3-second `recv_timeout`
//! rather than as an independent OS thread.

use std::collections::HashMap;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use phantom_agents::AgentId;

use crate::events::AiAction;
use crate::orchestrator::{ReplanDecision, StepStatus, TaskLedger};

/// How long an agent can be active without completing before we consider it
/// stalled. This is a safety valve — well-behaved agents finish via
/// `AgentComplete`; this catches the case where the agent process dies
/// without sending a completion event.
const DEFAULT_STALL_TIMEOUT: Duration = Duration::from_secs(300);

// ---------------------------------------------------------------------------
// ReconcilerState
// ---------------------------------------------------------------------------

/// Lightweight tracking table owned by the brain loop alongside the
/// [`TaskLedger`]. Survives across ticks; reset when a new goal is set.
pub struct ReconcilerState {
    /// Map: TaskLedger step index → (agent_id, time dispatched).
    active_dispatches: HashMap<usize, (AgentId, Instant)>,
    /// Monotonically increasing agent ID namespace for reconciler-dispatched
    /// agents. Starts high to avoid collision with AgentManager's IDs.
    next_agent_id: AgentId,
    /// Configurable stall timeout (overrideable in tests).
    pub stall_timeout: Duration,
}

impl ReconcilerState {
    pub fn new() -> Self {
        Self {
            active_dispatches: HashMap::new(),
            next_agent_id: 10_000,
            stall_timeout: DEFAULT_STALL_TIMEOUT,
        }
    }

    /// Reset all tracking — called when a new goal replaces the current one.
    pub fn reset(&mut self) {
        self.active_dispatches.clear();
    }

    // -----------------------------------------------------------------------
    // Public API
    // -----------------------------------------------------------------------

    /// Called every 3 seconds from the brain loop timeout.
    ///
    /// Drives the ledger forward: checks for stalled dispatches, evaluates
    /// re-plan conditions, and dispatches the next pending step if clear.
    /// Returns `true` if the ledger is still active (caller should keep it),
    /// `false` if the goal reached a terminal state (caller should drop the ledger).
    pub fn tick(&mut self, ledger: &mut TaskLedger, action_tx: &mpsc::Sender<AiAction>) -> bool {
        self.check_stalled(ledger, action_tx);

        match ledger.should_replan() {
            ReplanDecision::Complete => {
                log::info!("Reconciler: goal complete — {}", ledger.goal);
                let _ = action_tx.send(AiAction::ConsoleReply(format!(
                    "✓ Goal complete: {}",
                    ledger.goal
                )));
                false
            }

            ReplanDecision::GiveUp { reason } => {
                log::warn!("Reconciler: goal abandoned — {reason}");
                let _ = action_tx.send(AiAction::AgentFlatlined {
                    id: 0, // 0 = the whole goal flatlined, not a single agent
                    reason: format!("Goal '{}' abandoned: {reason}", ledger.goal),
                });
                false
            }

            ReplanDecision::Replan { reason, .. } => {
                // Run the inner-loop assessment so the ledger updates its
                // stall counter and recent-output loop detection.
                let _assessment = ledger.assess_progress();
                log::info!("Reconciler: re-plan triggered — {reason}");
                // Deferred to #32 (Reconciler: should_replan + dispatch_next_step).
                // When that lands, spawn a Composer agent here to generate a revised
                // plan and call ledger.replan(new_steps). For now, log and keep the
                // ledger alive — the next tick will re-evaluate.
                true
            }

            ReplanDecision::Continue => {
                self.dispatch_pending(ledger, action_tx);
                true
            }
        }
    }

    /// Called from the brain loop when `AiEvent::AgentComplete` arrives.
    ///
    /// Updates the corresponding TaskLedger step — success marks it done,
    /// failure decrements retries (re-queues or marks Failed per policy).
    ///
    /// `spawn_tag` is the reconciler-issued synthetic ID stamped on the
    /// `SpawnAgent` action and echoed back by the agent adapter. When it is
    /// `Some`, we perform an exact lookup in `active_dispatches`; when it is
    /// `None` (non-reconciler agent), the event is a no-op for the ledger.
    pub fn on_agent_complete(
        &mut self,
        ledger: &mut TaskLedger,
        _agent_id: AgentId,
        success: bool,
        summary: &str,
        spawn_tag: Option<u64>,
    ) {
        // Without a spawn_tag we cannot safely identify which ledger step
        // this completion belongs to — treat it as a non-reconciler event.
        let Some(tag) = spawn_tag else {
            return;
        };

        // Find the dispatch entry whose stored synthetic ID matches the tag.
        let idx = match self
            .active_dispatches
            .iter()
            .find(|&(_, &(stored_id, _))| stored_id as u64 == tag)
            .map(|(&idx, _)| idx)
        {
            Some(i) => i,
            None => {
                // Stale or cancelled dispatch — ignore.
                log::debug!("Reconciler: ignoring AgentComplete with unknown spawn_tag={tag}");
                return;
            }
        };

        self.active_dispatches.remove(&idx);

        if let Some(step) = ledger.plan.get_mut(idx) {
            if success {
                step.record_success(summary);
                log::info!(
                    "Reconciler: step {} done — {}",
                    idx,
                    step.description
                );
            } else {
                let retrying = step.record_failure(summary);
                if retrying {
                    log::warn!(
                        "Reconciler: step {} failed, will retry (attempt={}/{})",
                        idx, step.attempts, step.max_attempts
                    );
                } else {
                    log::error!(
                        "Reconciler: step {} exhausted retries — {}",
                        idx,
                        step.description
                    );
                }
            }
            ledger.record_output(summary);
        }
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    /// Dispatch the next pending step if no agent is currently active.
    ///
    /// Sequential execution: one active step at a time. Parallel dispatch
    /// can be added later by removing the early-return guard.
    fn dispatch_pending(&mut self, ledger: &mut TaskLedger, action_tx: &mpsc::Sender<AiAction>) {
        if !self.active_dispatches.is_empty() {
            return; // something already running
        }

        let Some((idx, step)) = ledger.next_pending_step() else {
            return; // nothing to do
        };

        let agent_id = self.next_agent_id;
        self.next_agent_id += 1;

        let task = step.assigned_task.clone();
        let description = step.description.clone();

        // Advance step to Active before emitting the SpawnAgent action so
        // that if the brain loop checks the ledger synchronously it sees
        // the correct state.
        if let Some(s) = ledger.plan.get_mut(idx) {
            s.status = StepStatus::Active;
            s.agent_id = Some(agent_id as u32);
            s.attempts += 1;
        }

        self.active_dispatches.insert(idx, (agent_id, Instant::now()));

        log::info!(
            "Reconciler: dispatching step {idx} (agent_id={agent_id}) — {description}"
        );

        let _ = action_tx.send(AiAction::SpawnAgent {
            task,
            spawn_tag: Some(agent_id as u64),
        });
    }

    /// Detect dispatches that have been running longer than `stall_timeout`.
    ///
    /// Records a failure on the step (which either re-queues for retry or
    /// marks it Failed and emits `AgentFlatlined` if retries are exhausted).
    fn check_stalled(&mut self, ledger: &mut TaskLedger, action_tx: &mpsc::Sender<AiAction>) {
        let timed_out: Vec<(usize, AgentId)> = self
            .active_dispatches
            .iter()
            .filter(|(_, (_, dispatched_at))| dispatched_at.elapsed() > self.stall_timeout)
            .map(|(&idx, &(aid, _))| (idx, aid))
            .collect();

        for (idx, agent_id) in timed_out {
            self.active_dispatches.remove(&idx);

            if let Some(step) = ledger.plan.get_mut(idx) {
                let description = step.description.clone();
                let still_retrying = step.record_failure("stall timeout");

                if still_retrying {
                    log::warn!(
                        "Reconciler: step {idx} stalled (agent_id={agent_id}), will retry — {description}"
                    );
                } else {
                    log::error!(
                        "Reconciler: step {idx} stalled and exhausted retries (agent_id={agent_id}) — {description}"
                    );
                    let _ = action_tx.send(AiAction::AgentFlatlined {
                        id: agent_id as u32,
                        reason: format!(
                            "step '{description}' stalled after {} attempts",
                            step.max_attempts
                        ),
                    });
                }
            }
        }
    }
}

impl Default for ReconcilerState {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestrator::PlanStep;
    use phantom_agents::AgentTask;

    fn free_task(prompt: &str) -> AgentTask {
        AgentTask::FreeForm {
            prompt: prompt.into(),
        }
    }

    fn make_ledger(steps: &[&str]) -> TaskLedger {
        let mut ledger = TaskLedger::new("test goal");
        ledger.set_plan(
            steps
                .iter()
                .map(|s| PlanStep::new(*s, free_task(s)))
                .collect(),
        );
        ledger
    }

    /// Helper: dispatch one step and return the spawn_tag from the emitted action.
    fn dispatch_and_capture_tag(
        state: &mut ReconcilerState,
        ledger: &mut TaskLedger,
        tx: &mpsc::Sender<AiAction>,
        rx: &mpsc::Receiver<AiAction>,
    ) -> u64 {
        state.tick(ledger, tx);
        let action = rx.try_recv().expect("expected SpawnAgent action");
        let AiAction::SpawnAgent { spawn_tag, .. } = action else {
            panic!("expected SpawnAgent variant");
        };
        spawn_tag.expect("spawn_tag must be Some for reconciler-dispatched steps")
    }

    #[test]
    fn tick_dispatches_first_pending_step() {
        let (tx, rx) = mpsc::channel();
        let mut state = ReconcilerState::new();
        let mut ledger = make_ledger(&["step one"]);

        state.tick(&mut ledger, &tx);

        // Should have emitted a SpawnAgent with a spawn_tag.
        let action = rx.try_recv().expect("expected SpawnAgent action");
        assert!(matches!(action, AiAction::SpawnAgent { .. }));

        // Step should now be Active.
        assert_eq!(ledger.plan[0].status, StepStatus::Active);
        assert_eq!(state.active_dispatches.len(), 1);
    }

    #[test]
    fn tick_does_not_double_dispatch() {
        let (tx, rx) = mpsc::channel();
        let mut state = ReconcilerState::new();
        let mut ledger = make_ledger(&["step one"]);

        state.tick(&mut ledger, &tx);
        let _ = rx.try_recv(); // consume first SpawnAgent

        // Second tick — active dispatch already running, should not dispatch again.
        state.tick(&mut ledger, &tx);
        assert!(rx.try_recv().is_err(), "should not dispatch while step active");
    }

    #[test]
    fn on_agent_complete_success_marks_step_done() {
        let (tx, rx) = mpsc::channel();
        let mut state = ReconcilerState::new();
        let mut ledger = make_ledger(&["step one"]);

        let tag = dispatch_and_capture_tag(&mut state, &mut ledger, &tx, &rx);

        state.on_agent_complete(&mut ledger, 1, true, "done", Some(tag));

        assert_eq!(ledger.plan[0].status, StepStatus::Done);
        assert!(state.active_dispatches.is_empty());
    }

    #[test]
    fn on_agent_complete_failure_requeues_when_retries_remain() {
        let (tx, rx) = mpsc::channel();
        let mut state = ReconcilerState::new();
        let mut ledger = make_ledger(&["step one"]);
        ledger.plan[0].max_attempts = 3;

        let tag = dispatch_and_capture_tag(&mut state, &mut ledger, &tx, &rx);

        state.on_agent_complete(&mut ledger, 1, false, "error", Some(tag));

        // Step re-queued (Pending) since attempts=1 < max_attempts=3.
        assert_eq!(ledger.plan[0].status, StepStatus::Pending);
        assert!(state.active_dispatches.is_empty());
    }

    #[test]
    fn on_agent_complete_exhausted_marks_failed() {
        let (tx, rx) = mpsc::channel();
        let mut state = ReconcilerState::new();
        let mut ledger = make_ledger(&["step one"]);
        ledger.plan[0].max_attempts = 1;

        let tag = dispatch_and_capture_tag(&mut state, &mut ledger, &tx, &rx);

        state.on_agent_complete(&mut ledger, 1, false, "error", Some(tag));

        assert_eq!(ledger.plan[0].status, StepStatus::Failed);
    }

    #[test]
    fn stall_detection_emits_flatline_on_exhausted_retries() {
        let (tx, rx) = mpsc::channel();
        let mut state = ReconcilerState {
            stall_timeout: Duration::from_millis(1), // tiny timeout for test
            ..ReconcilerState::new()
        };
        let mut ledger = make_ledger(&["step one"]);
        ledger.plan[0].max_attempts = 1;

        state.tick(&mut ledger, &tx); // dispatch
        let _ = rx.try_recv(); // consume SpawnAgent

        std::thread::sleep(Duration::from_millis(5)); // let timeout expire

        state.tick(&mut ledger, &tx); // should detect stall and flatline

        let action = rx.try_recv().expect("expected AgentFlatlined");
        assert!(matches!(action, AiAction::AgentFlatlined { .. }));
    }

    #[test]
    fn sequential_steps_dispatch_in_order() {
        let (tx, rx) = mpsc::channel();
        let mut state = ReconcilerState::new();
        let mut ledger = make_ledger(&["step one", "step two"]);

        // Tick 1: dispatch step 0 and capture tag.
        let tag0 = dispatch_and_capture_tag(&mut state, &mut ledger, &tx, &rx);

        // Complete step 0 via spawn_tag.
        state.on_agent_complete(&mut ledger, 1, true, "ok", Some(tag0));

        // Tick 2: should now dispatch step 1.
        state.tick(&mut ledger, &tx);
        let action = rx.try_recv().expect("expected second SpawnAgent");
        assert!(matches!(action, AiAction::SpawnAgent { .. }));
        assert_eq!(ledger.plan[1].status, StepStatus::Active);
    }

    #[test]
    fn reset_clears_active_dispatches() {
        let (tx, _rx) = mpsc::channel();
        let mut state = ReconcilerState::new();
        let mut ledger = make_ledger(&["step one"]);

        state.tick(&mut ledger, &tx);
        assert!(!state.active_dispatches.is_empty());

        state.reset();
        assert!(state.active_dispatches.is_empty());
    }

    // -- spawn_tag tests (Issue #99) -----------------------------------------

    /// The SpawnAgent action emitted by `dispatch_pending` must carry a
    /// non-None `spawn_tag` that matches the reconciler's synthetic agent ID
    /// stored in `active_dispatches`.
    #[test]
    fn dispatch_stamps_spawn_tag_on_action() {
        let (tx, rx) = mpsc::channel();
        let mut state = ReconcilerState::new();
        let mut ledger = make_ledger(&["step one"]);

        state.tick(&mut ledger, &tx);

        let action = rx.try_recv().expect("expected SpawnAgent");
        let AiAction::SpawnAgent { spawn_tag, .. } = action else {
            panic!("expected SpawnAgent variant");
        };

        // The tag must be Some and must equal the synthetic id in active_dispatches.
        let tag = spawn_tag.expect("spawn_tag must be Some after dispatch");
        let (&idx, &(stored_id, _)) = state.active_dispatches.iter().next()
            .expect("active_dispatches must have one entry");
        assert_eq!(idx, 0, "step 0 should be active");
        assert_eq!(tag, stored_id as u64, "spawn_tag must match stored synthetic id");
    }

    /// `on_agent_complete` must route by `spawn_tag`, not by sequential
    /// assumption. If we pass the wrong AgentManager ID but the correct
    /// `spawn_tag`, the step must still be completed.
    #[test]
    fn on_agent_complete_routes_by_spawn_tag_not_by_manager_id() {
        let (tx, rx) = mpsc::channel();
        let mut state = ReconcilerState::new();
        let mut ledger = make_ledger(&["step one"]);

        state.tick(&mut ledger, &tx);

        // Capture the spawn_tag from the dispatched action.
        let action = rx.try_recv().expect("expected SpawnAgent");
        let AiAction::SpawnAgent { spawn_tag, .. } = action else {
            panic!("expected SpawnAgent variant");
        };
        let tag = spawn_tag.expect("spawn_tag must be Some");

        // Simulate AgentManager assigning a completely different ID (e.g. 7).
        // The old workaround would still work here (single entry), but with the
        // correct spawn_tag the reconciler must use an exact match.
        state.on_agent_complete(&mut ledger, 7, true, "done", Some(tag));

        assert_eq!(
            ledger.plan[0].status,
            StepStatus::Done,
            "step must be Done even when AgentManager id (7) differs from synthetic id"
        );
        assert!(state.active_dispatches.is_empty());
    }

    /// If `spawn_tag` is `None` (non-reconciler agent), `on_agent_complete`
    /// must be a no-op — it must not corrupt the ledger.
    #[test]
    fn on_agent_complete_ignores_events_without_spawn_tag() {
        let (tx, _rx) = mpsc::channel();
        let mut state = ReconcilerState::new();
        let mut ledger = make_ledger(&["step one"]);

        state.tick(&mut ledger, &tx); // dispatch step 0

        // A completion event with no spawn_tag (from an unrelated agent).
        state.on_agent_complete(&mut ledger, 42, true, "unrelated", None);

        // Step must remain Active — not erroneously advanced.
        assert_eq!(
            ledger.plan[0].status,
            StepStatus::Active,
            "step must remain Active when spawn_tag is None"
        );
        assert_eq!(state.active_dispatches.len(), 1, "dispatch must remain registered");
    }

    /// `on_agent_complete` with a spawn_tag that doesn't match any active
    /// dispatch (e.g. for a previously cancelled step) must be a no-op.
    #[test]
    fn on_agent_complete_ignores_unknown_spawn_tag() {
        let (tx, _rx) = mpsc::channel();
        let mut state = ReconcilerState::new();
        let mut ledger = make_ledger(&["step one"]);

        state.tick(&mut ledger, &tx); // dispatch step 0

        // A completion event with a spawn_tag that was never issued.
        state.on_agent_complete(&mut ledger, 5, true, "stale", Some(99_999));

        assert_eq!(
            ledger.plan[0].status,
            StepStatus::Active,
            "step must remain Active when spawn_tag is unknown"
        );
    }
    // -- Issue #111: AgentError exit-path spawn_tag coverage -----------------

    /// When `drain_bus_to_brain` translates `Event::AgentError` it hard-codes
    /// `spawn_tag: None` because `AgentError` has no spawn_tag field.
    ///
    /// `on_agent_complete(..., spawn_tag: None)` must be a complete no-op:
    /// the active dispatch must remain registered, the step must stay `Active`,
    /// and no panic must occur. Cleanup of the stalled agent is left entirely
    /// to the stall-timeout path in `check_stalled`.
    #[test]
    fn agent_error_with_none_spawn_tag_is_noop() {
        let (tx, _rx) = mpsc::channel();
        let mut state = ReconcilerState::new();
        let mut ledger = make_ledger(&["step one"]);

        state.tick(&mut ledger, &tx); // dispatch step 0, spawns reconciler agent

        // Simulate what drain_bus_to_brain does when it sees Event::AgentError:
        // it emits AiEvent::AgentComplete { spawn_tag: None, success: false, ... }.
        // The brain then calls on_agent_complete with spawn_tag = None.
        state.on_agent_complete(&mut ledger, 99, false, "api error", None);

        // Must be a no-op: dispatch remains, step stays Active.
        assert_eq!(
            ledger.plan[0].status,
            StepStatus::Active,
            "AgentError (spawn_tag=None) must not advance the ledger"
        );
        assert_eq!(
            state.active_dispatches.len(),
            1,
            "active dispatch must remain registered after AgentError with spawn_tag=None"
        );
    }

    /// When `spawn_tag` is `None`, the error event must carry `None` -- not a
    /// default u64 (e.g. 0) or any fabricated value. This test exercises the
    /// same `on_agent_complete` path with an explicitly `None` tag and verifies
    /// the ledger is untouched regardless of the agent_id value.
    #[test]
    fn agent_error_none_spawn_tag_is_not_coerced_to_zero() {
        let (tx, _rx) = mpsc::channel();
        let mut state = ReconcilerState::new();
        let mut ledger = make_ledger(&["step one"]);

        state.tick(&mut ledger, &tx); // dispatch step 0

        // Passing agent_id=0 with spawn_tag=None must not be confused with
        // "synthetic id 0" -- the reconciler must gate on spawn_tag presence
        // first, before touching the ledger.
        state.on_agent_complete(&mut ledger, 0, false, "oops", None);

        assert_eq!(
            ledger.plan[0].status,
            StepStatus::Active,
            "spawn_tag=None must short-circuit before any ledger mutation"
        );
        assert_eq!(
            state.active_dispatches.len(),
            1,
            "dispatch must still be registered"
        );
    }

    /// After an `AgentError` arrives (translated with `spawn_tag: None` and
    /// silently ignored by `on_agent_complete`), the stall-timeout path must
    /// still fire and clean up the active dispatch correctly -- no double-free,
    /// no panic.
    #[test]
    fn stall_timeout_handles_cleanup_after_agent_error_noop() {
        let (tx, rx) = mpsc::channel();
        let mut state = ReconcilerState {
            stall_timeout: Duration::from_millis(1),
            ..ReconcilerState::new()
        };
        let mut ledger = make_ledger(&["step one"]);
        ledger.plan[0].max_attempts = 1;

        state.tick(&mut ledger, &tx); // dispatch
        let _ = rx.try_recv(); // consume SpawnAgent

        // Simulate AgentError arriving (no-op from reconciler's perspective).
        state.on_agent_complete(&mut ledger, 42, false, "agent error", None);

        // Step must still be Active -- the error was silently ignored.
        assert_eq!(ledger.plan[0].status, StepStatus::Active);
        assert_eq!(state.active_dispatches.len(), 1);

        // Let the stall timeout expire, then tick.
        std::thread::sleep(Duration::from_millis(5));
        state.tick(&mut ledger, &tx);

        // Stall timeout should have fired, exhausted retries, and emitted
        // AgentFlatlined -- no panic.
        let action = rx.try_recv().expect("expected AgentFlatlined from stall timeout");
        assert!(
            matches!(action, AiAction::AgentFlatlined { .. }),
            "expected AgentFlatlined, got {action:?}"
        );
        assert!(
            state.active_dispatches.is_empty(),
            "dispatch must be removed after stall timeout"
        );
    }

}