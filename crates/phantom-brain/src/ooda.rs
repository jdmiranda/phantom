//! OODA loop frame integration — `Brain::tick(world, dt_ms) -> Vec<AiAction>`.
//!
//! This module wires the Observe → Orient → Decide → Act cycle to the
//! per-frame render loop. The caller (phantom-app's `update.rs`) invokes
//! [`OodaLoop::tick`] once per frame. Each tick:
//!
//! 1. **Observe** — snapshot the incoming [`WorldState`].
//! 2. **Orient** — translate the snapshot into a [`ScoringContext`] and
//!    update the internal scorer state (idle time, error flag, etc.).
//! 3. **Decide** — evaluate all registered behaviors via the
//!    [`BehaviorDecisionSystem`] and pick the highest-scoring winner.
//! 4. **Act** — if the winner's score beats the configured threshold,
//!    convert it to an [`AiAction`] and append it to the output Vec.
//!
//! # Frame budget
//!
//! The tick is bounded by a hard 2 ms frame-budget cap (configurable).
//! If the total time spent in Observe + Orient + Decide exceeds the budget
//! the Act phase is skipped and a counter is incremented so the inspector
//! can surface budget-overrun telemetry.
//!
//! # Usage (from phantom-app)
//!
//! ```rust,ignore
//! // Initialise once at startup.
//! let mut ooda = OodaLoop::new(OodaConfig::default());
//!
//! // Inside App::update():
//! let world = WorldState {
//!     idle_secs:          idle_elapsed,
//!     has_errors:         last_parse.errors.is_empty().not(),
//!     error_count:        last_parse.errors.len() as u32,
//!     has_active_process: pty_is_running,
//!     ..WorldState::default()
//! };
//! let actions = ooda.tick(&world, dt_ms);
//! for action in actions {
//!     brain.send_action(action);
//! }
//! ```

use std::time::Instant;

use crate::curves::{
    BehaviorDecisionSystem, ScoringContext, build_default_behaviors,
};
use crate::events::AiAction;

// ---------------------------------------------------------------------------
// WorldState — observable slice of the application
// ---------------------------------------------------------------------------

/// A lightweight snapshot of all observable application state.
///
/// Callers fill this in from their local data on every frame and pass it
/// to [`OodaLoop::tick`]. The struct is `Copy`-cheap — all fields are
/// primitives or small booleans.
///
/// Use [`WorldState::new`] to construct.
#[derive(Debug, Clone, Default)]
pub struct WorldState {
    /// Seconds since the user's last keyboard/mouse input.
    idle_secs: f32,
    /// Whether the most-recently-parsed command output contained errors.
    has_errors: bool,
    /// Number of errors in the most recent parse (0 when `has_errors` is false).
    error_count: u32,
    /// Whether a long-running child process (build, test, server) is active.
    has_active_process: bool,
    /// Whether a novel memory pattern was detected this frame.
    new_pattern_detected: bool,
    /// Whether an agent completed its task this frame.
    agent_just_completed: bool,
    /// Whether a file or git state change was detected this frame.
    file_or_git_changed: bool,
    /// Whether the user is inside an interactive REPL session.
    in_repl: bool,
    /// Accumulated chattiness of the brain (0.0 = silent, 1.0 = very chatty).
    chattiness: f32,
    /// Number of suggestions emitted since the last user input.
    suggestions_since_input: u32,
}

impl WorldState {
    /// Construct a [`WorldState`] snapshot with all fields specified.
    ///
    /// # Arguments
    ///
    /// * `idle_secs` — seconds since the last keyboard/mouse input.
    /// * `has_errors` — whether the most-recent parse contained errors.
    /// * `error_count` — number of errors (0 when `has_errors` is false).
    /// * `has_active_process` — whether a long-running child process is active.
    /// * `new_pattern_detected` — novel memory pattern detected this frame.
    /// * `agent_just_completed` — an agent completed its task this frame.
    /// * `file_or_git_changed` — file or git state changed this frame.
    /// * `in_repl` — user is inside an interactive REPL session.
    /// * `chattiness` — accumulated brain chattiness (0.0–1.0).
    /// * `suggestions_since_input` — suggestions emitted since last input.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        idle_secs: f32,
        has_errors: bool,
        error_count: u32,
        has_active_process: bool,
        new_pattern_detected: bool,
        agent_just_completed: bool,
        file_or_git_changed: bool,
        in_repl: bool,
        chattiness: f32,
        suggestions_since_input: u32,
    ) -> Self {
        Self {
            idle_secs,
            has_errors,
            error_count,
            has_active_process,
            new_pattern_detected,
            agent_just_completed,
            file_or_git_changed,
            in_repl,
            chattiness,
            suggestions_since_input,
        }
    }
}

// ---------------------------------------------------------------------------
// OodaConfig
// ---------------------------------------------------------------------------

/// Configuration for the per-frame OODA loop.
#[derive(Debug, Clone)]
pub struct OodaConfig {
    /// Maximum time (ms) allowed for Observe + Orient + Decide before the Act
    /// phase is skipped. Default: 2 ms.
    budget_ms: f32,
    /// Minimum BDS score required to emit an action. Scores at or below this
    /// value produce `AiAction::DoNothing`. Default: 15.0 (Basic class ceiling).
    action_threshold: f32,
}

impl OodaConfig {
    /// Create a new [`OodaConfig`].
    ///
    /// * `budget_ms` — frame-budget cap in milliseconds.
    /// * `action_threshold` — minimum BDS score required to emit an action.
    pub fn new(budget_ms: f32, action_threshold: f32) -> Self {
        Self { budget_ms, action_threshold }
    }

    /// Frame-budget cap in milliseconds.
    pub fn budget_ms(&self) -> f32 {
        self.budget_ms
    }

    /// Minimum BDS score required to emit an action.
    pub fn action_threshold(&self) -> f32 {
        self.action_threshold
    }
}

impl Default for OodaConfig {
    fn default() -> Self {
        Self {
            budget_ms: 2.0,
            action_threshold: 15.0,
        }
    }
}

// ---------------------------------------------------------------------------
// TickMetrics — observable telemetry for the inspector
// ---------------------------------------------------------------------------

/// Telemetry counters exposed to the Inspector pane.
#[derive(Debug, Clone, Default)]
pub struct TickMetrics {
    /// Total number of ticks executed.
    ticks: u64,
    /// Number of ticks where the winning score was above `action_threshold`.
    actions_emitted: u64,
    /// Number of ticks skipped because the frame budget was exceeded.
    budget_overruns: u64,
    /// Duration (ms) of the most recent tick.
    last_tick_ms: f32,
    /// The BDS winner ID from the most recent tick.
    last_winner: String,
    /// The BDS winner score from the most recent tick.
    last_winner_score: f32,
}

impl TickMetrics {
    /// Total number of ticks executed.
    pub fn ticks(&self) -> u64 {
        self.ticks
    }

    /// Number of ticks where the winning score exceeded `action_threshold`.
    pub fn actions_emitted(&self) -> u64 {
        self.actions_emitted
    }

    /// Number of ticks skipped because the frame budget was exceeded.
    pub fn budget_overruns(&self) -> u64 {
        self.budget_overruns
    }

    /// Duration (ms) of the most recent tick.
    pub fn last_tick_ms(&self) -> f32 {
        self.last_tick_ms
    }

    /// The BDS winner ID from the most recent tick.
    pub fn last_winner(&self) -> &str {
        &self.last_winner
    }

    /// The BDS winner score from the most recent tick.
    pub fn last_winner_score(&self) -> f32 {
        self.last_winner_score
    }
}

// ---------------------------------------------------------------------------
// OodaLoop — the per-frame state machine
// ---------------------------------------------------------------------------

/// Per-frame OODA loop driven by the render clock.
///
/// Owns the [`BehaviorDecisionSystem`] and all per-tick state. Create once at
/// application startup and call [`tick`](OodaLoop::tick) every frame.
pub struct OodaLoop {
    config: OodaConfig,
    bds: BehaviorDecisionSystem,
    /// Accumulated telemetry; readable by the Inspector via [`metrics`](OodaLoop::metrics).
    metrics: TickMetrics,
    /// Phase trace — records which phases ran in the last tick (for tests).
    last_phases: Vec<&'static str>,
}

impl OodaLoop {
    /// Create a new OODA loop with the default DA:I behaviors registered.
    pub fn new(config: OodaConfig) -> Self {
        let mut bds = BehaviorDecisionSystem::new();
        for behavior in build_default_behaviors() {
            bds.register(behavior);
        }
        Self {
            config,
            bds,
            metrics: TickMetrics::default(),
            last_phases: Vec::new(),
        }
    }

    /// Read the current telemetry snapshot (non-blocking, cheap clone).
    pub fn metrics(&self) -> &TickMetrics {
        &self.metrics
    }

    /// The phase names visited in the most recent [`tick`](OodaLoop::tick).
    ///
    /// Used by tests to assert ordering. In release builds this Vec stays
    /// small (at most 4 entries) and is cleared at the start of every tick.
    pub fn last_phases(&self) -> &[&'static str] {
        &self.last_phases
    }

    /// Execute one OODA cycle for the current frame.
    ///
    /// Returns a `Vec<AiAction>` containing the decided action if the
    /// winning BDS score beats `config.action_threshold`, or an empty Vec
    /// (equiv. `DoNothing`) when the brain is quiet.
    ///
    /// # Arguments
    ///
    /// * `world` — snapshot of observable app state for this frame.
    /// * `dt_ms` — frame delta time in milliseconds (used for budget tracking
    ///   and future cooldown accounting; not consumed by BDS itself).
    pub fn tick(&mut self, world: &WorldState, _dt_ms: u64) -> Vec<AiAction> {
        let tick_start = Instant::now();
        self.metrics.ticks += 1;
        self.last_phases.clear();

        // ----------------------------------------------------------------
        // Phase 1: OBSERVE — snapshot the world state.
        // ----------------------------------------------------------------
        self.last_phases.push("observe");
        let snapshot = world.clone();

        // ----------------------------------------------------------------
        // Phase 2: ORIENT — translate snapshot → ScoringContext.
        // ----------------------------------------------------------------
        self.last_phases.push("orient");
        let ctx = self.orient(&snapshot);

        // ----------------------------------------------------------------
        // Frame-budget check — measure time so far.
        // ----------------------------------------------------------------
        let elapsed_so_far = tick_start.elapsed().as_secs_f32() * 1000.0;
        if elapsed_so_far > self.config.budget_ms {
            // Budget exceeded — skip Decide/Act.
            self.metrics.budget_overruns += 1;
            self.metrics.last_tick_ms = elapsed_so_far;
            return Vec::new();
        }

        // ----------------------------------------------------------------
        // Phase 3: DECIDE — evaluate BDS, pick highest scorer.
        // ----------------------------------------------------------------
        self.last_phases.push("decide");
        let eval = self.bds.evaluate(&ctx);

        // ----------------------------------------------------------------
        // Phase 4: ACT — emit action if above threshold.
        // ----------------------------------------------------------------
        self.last_phases.push("act");
        let tick_ms = tick_start.elapsed().as_secs_f32() * 1000.0;
        self.metrics.last_tick_ms = tick_ms;
        self.metrics.last_winner = eval.winner_id.clone();
        self.metrics.last_winner_score = eval.winner_score;

        if eval.winner_score > self.config.action_threshold {
            self.metrics.actions_emitted += 1;
            let action = Self::behavior_to_action(&eval.winner_id, &snapshot);
            vec![action]
        } else {
            Vec::new()
        }
    }

    // -----------------------------------------------------------------------
    // orient — private helper
    // -----------------------------------------------------------------------

    /// Translate a `WorldState` snapshot into a `ScoringContext` for the BDS.
    fn orient(&self, world: &WorldState) -> ScoringContext {
        ScoringContext {
            idle_secs: world.idle_secs,
            has_errors: world.has_errors,
            error_count: world.error_count,
            has_active_process: world.has_active_process,
            new_pattern_detected: world.new_pattern_detected,
            agent_just_completed: world.agent_just_completed,
            file_or_git_changed: world.file_or_git_changed,
            in_repl: world.in_repl,
            chattiness: world.chattiness,
            suggestions_since_input: world.suggestions_since_input,
        }
    }

    // -----------------------------------------------------------------------
    // behavior_to_action — map BDS winner ID → AiAction
    // -----------------------------------------------------------------------

    /// Convert a winning BDS behavior ID to a concrete `AiAction`.
    ///
    /// The mapping is intentionally coarse-grained: the BDS decides *which
    /// class of action* wins; the exact message content is filled in later by
    /// the scorer / LLM pipeline. This keeps the OODA loop stateless with
    /// respect to LLM calls.
    fn behavior_to_action(behavior_id: &str, world: &WorldState) -> AiAction {
        match behavior_id {
            "fix_error" => AiAction::ShowSuggestion {
                text: format!(
                    "Detected {} error(s) — consider running the fixer agent.",
                    world.error_count
                ),
                options: vec![],
            },
            "explain_error" => AiAction::ShowSuggestion {
                text: "Would you like an explanation of the recent errors?".into(),
                options: vec![],
            },
            "offer_help" => AiAction::ShowSuggestion {
                text: "You've been idle a while — need help with anything?".into(),
                options: vec![],
            },
            "update_memory" => AiAction::UpdateMemory {
                key: "ooda_pattern".into(),
                value: "new pattern detected".into(),
            },
            "notify_agent_complete" => {
                AiAction::ShowNotification("Agent completed.".into())
            }
            "notify_change" => {
                AiAction::ShowNotification("Files or git state changed.".into())
            }
            // quiet / watch_build / unknown → DoNothing
            _ => AiAction::DoNothing,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn default_loop() -> OodaLoop {
        OodaLoop::new(OodaConfig::default())
    }

    /// A world state that should reliably trigger the `fix_error` Reaction
    /// behavior (score ≥ 50) which beats the default threshold of 15.0.
    fn error_world() -> WorldState {
        WorldState::new(5.0, true, 3, false, false, false, false, false, 0.0, 0)
    }

    /// A fully quiet world — no errors, no process, low idle — so the BDS
    /// winner should stay at the Basic "quiet" level (≤ 15).
    fn quiet_world() -> WorldState {
        WorldState::new(0.5, false, 0, false, false, false, false, false, 0.0, 0)
    }

    // =======================================================================
    // Test 1: phases run in the correct order (observe → orient → decide → act)
    // =======================================================================

    #[test]
    fn phases_run_in_order() {
        let mut ooda = default_loop();
        let _ = ooda.tick(&error_world(), 16);

        let phases = ooda.last_phases();
        assert_eq!(
            phases,
            &["observe", "orient", "decide", "act"],
            "expected all four phases in OODA order, got {:?}",
            phases
        );
    }

    // =======================================================================
    // Test 2: high-score action is dispatched (fix_error beats threshold)
    // =======================================================================

    #[test]
    fn high_score_action_is_dispatched() {
        let mut ooda = default_loop();
        let actions = ooda.tick(&error_world(), 16);

        assert!(
            !actions.is_empty(),
            "expected at least one action for error world, got none"
        );

        // The winning action should reference fix_error or explain_error.
        let got_suggestion = actions
            .iter()
            .any(|a| matches!(a, AiAction::ShowSuggestion { .. }));
        assert!(
            got_suggestion,
            "expected ShowSuggestion action for error state, got {:?}",
            actions
        );
    }

    // =======================================================================
    // Test 3: low-score action is skipped (quiet world stays below threshold)
    // =======================================================================

    #[test]
    fn low_score_action_is_skipped() {
        let mut ooda = default_loop();
        let actions = ooda.tick(&quiet_world(), 16);

        // The "quiet" behavior scores Basic (0–10) which is well below the
        // default threshold of 15.0 — so no actions should be emitted.
        assert!(
            actions.is_empty(),
            "expected no actions for quiet world, got {:?}",
            actions
        );
    }

    // =======================================================================
    // Test 4: metrics increment correctly across ticks
    // =======================================================================

    #[test]
    fn metrics_tick_counter_increments() {
        let mut ooda = default_loop();

        ooda.tick(&quiet_world(), 16);
        ooda.tick(&quiet_world(), 16);
        ooda.tick(&error_world(), 16);

        assert_eq!(ooda.metrics().ticks(), 3, "expected 3 ticks");
        assert!(
            ooda.metrics().actions_emitted() >= 1,
            "expected at least 1 action emitted (error tick)"
        );
    }

    // =======================================================================
    // Test 5: custom threshold skips action that would otherwise fire
    // =======================================================================

    #[test]
    fn custom_high_threshold_suppresses_action() {
        // Set an impossibly high threshold so nothing ever fires.
        let mut ooda = OodaLoop::new(OodaConfig::new(2.0, 9999.0));
        let actions = ooda.tick(&error_world(), 16);

        assert!(
            actions.is_empty(),
            "threshold=9999 should suppress all actions, got {:?}",
            actions
        );
    }

    // =======================================================================
    // Test 6: agent_just_completed triggers notify_agent_complete
    // =======================================================================

    #[test]
    fn agent_complete_triggers_notification() {
        let mut ooda = default_loop();
        let world = WorldState::new(0.0, false, 0, false, false, true, false, false, 0.0, 0);
        let actions = ooda.tick(&world, 16);

        // notify_agent_complete is Reaction class (50+) — should beat threshold.
        assert!(
            !actions.is_empty(),
            "expected notification action for agent_just_completed"
        );
        let has_notify = actions
            .iter()
            .any(|a| matches!(a, AiAction::ShowNotification(_)));
        assert!(
            has_notify,
            "expected ShowNotification, got {:?}",
            actions
        );
    }

    // =======================================================================
    // Test 7: orient translates world → scoring context correctly
    // =======================================================================

    #[test]
    fn orient_maps_world_to_context() {
        let ooda = default_loop();
        let world = WorldState::new(12.5, true, 7, true, false, false, false, false, 0.3, 2);
        let ctx = ooda.orient(&world);

        assert!((ctx.idle_secs - 12.5).abs() < f32::EPSILON);
        assert!(ctx.has_errors);
        assert_eq!(ctx.error_count, 7);
        assert!(ctx.has_active_process);
        assert!((ctx.chattiness - 0.3).abs() < f32::EPSILON);
        assert_eq!(ctx.suggestions_since_input, 2);
    }

    // =======================================================================
    // Test 8: last_winner is recorded in metrics
    // =======================================================================

    #[test]
    fn metrics_record_last_winner() {
        let mut ooda = default_loop();
        ooda.tick(&error_world(), 16);

        let winner = ooda.metrics().last_winner();
        assert!(
            !winner.is_empty(),
            "expected last_winner to be set after tick"
        );
        // With errors present, fix_error or explain_error should win.
        assert!(
            winner == "fix_error" || winner == "explain_error",
            "expected error-related winner, got {:?}",
            winner
        );
    }
}
