//! 2-loop decomposition pipeline: Synthesize → Decompose.
//!
//! Implements **Pattern 03** from the Phantom brain architecture.
//! When the [`Orchestrator`] decides a replan is needed, it calls
//! [`GoalDecomposer::decompose`] to turn a high-level goal string into a
//! prioritized sequence of concrete [`DecompositionStep`]s.
//!
//! # Two-pass pipeline
//!
//! ```text
//! goal string
//!     │
//!     ▼  Pass 1 — Synthesize
//! sub-goals  (keyword extraction + heuristics)
//!     │
//!     ▼  Pass 2 — Decompose
//! DecompositionStep[]  (sorted by priority desc)
//!     │
//!     ▼
//! DecompositionResult
//! ```
//!
//! The decomposer is intentionally rule-based (no LLM call): it is fast
//! enough to run inline on the reconciler tick. LLM-assisted planning is
//! deferred to a later phase when a Composer agent rewrites the plan.
//!
//! # Integration with [`Orchestrator`]
//!
//! The reconciler calls [`Orchestrator::decompose_and_replan`] when
//! [`Orchestrator::should_replan`] returns `true`. That method runs the
//! decomposer and converts the resulting [`DecompositionStep`]s into
//! [`crate::orchestrator::PlanStep`]s that replace the current ledger plan.
//!
//! [`Orchestrator`]: crate::orchestrator::Orchestrator

use crate::orchestrator::{Orchestrator, PlanStep, WorldState};
use phantom_agents::AgentTask;

// ---------------------------------------------------------------------------
// DecompositionStep — lightweight step type for the decomposer output
// ---------------------------------------------------------------------------

/// A single concrete step produced by the decomposition pipeline.
///
/// Deliberately lighter than [`crate::orchestrator::PlanStep`]:
/// it carries only the planning-time metadata (`priority`, `cost_ms`, `tool`)
/// and is later converted to a full `PlanStep` when installed into the ledger.
#[derive(Debug, Clone)]
pub struct DecompositionStep {
    description: String,
    tool: Option<String>,
    priority: f32,
    cost_ms: u32,
}

impl DecompositionStep {
    /// Human-readable description of what this step accomplishes.
    pub fn description(&self) -> &str {
        &self.description
    }

    /// Optional tool / agent-type hint for this step.
    pub fn tool(&self) -> Option<&str> {
        self.tool.as_deref()
    }

    /// Relative priority in [0.0, 1.0]. Higher = execute sooner.
    pub fn priority(&self) -> f32 {
        self.priority
    }

    /// Estimated wall-clock cost in milliseconds.
    pub fn cost_ms(&self) -> u32 {
        self.cost_ms
    }

    // -- Constructors (crate-private so tests can build them) ----------------

    fn new(
        description: impl Into<String>,
        tool: Option<String>,
        priority: f32,
        cost_ms: u32,
    ) -> Self {
        Self {
            description: description.into(),
            tool,
            priority: priority.clamp(0.0, 1.0),
            cost_ms,
        }
    }
}

// ---------------------------------------------------------------------------
// DecompositionResult
// ---------------------------------------------------------------------------

/// The output of a single decomposition pass.
///
/// Wraps the original goal, the ordered steps, and a pre-computed
/// `estimated_total_ms` so callers can gate on budget without iterating.
#[derive(Debug, Clone)]
pub struct DecompositionResult {
    /// The original goal string that was decomposed.
    goal: String,
    /// Steps sorted in descending priority order (index 0 = highest priority).
    steps: Vec<DecompositionStep>,
    /// Sum of `cost_ms` across all steps.
    estimated_total_ms: u32,
}

impl DecompositionResult {
    /// The original goal string that was decomposed.
    pub fn goal(&self) -> &str {
        &self.goal
    }

    /// Steps sorted in descending priority order (index 0 = highest priority).
    pub fn steps(&self) -> &[DecompositionStep] {
        &self.steps
    }

    /// Sum of `cost_ms` across all steps.
    pub fn estimated_total_ms(&self) -> u32 {
        self.estimated_total_ms
    }

    /// `true` if there is at least one step to execute.
    pub fn is_achievable(&self) -> bool {
        !self.steps.is_empty()
    }
}

// ---------------------------------------------------------------------------
// SubGoal — internal first-pass type
// ---------------------------------------------------------------------------

/// An intermediate sub-goal identified in the Synthesize pass.
///
/// Not exposed publicly; converted to [`DecompositionStep`]s in the Decompose
/// pass.
#[derive(Debug, Clone)]
struct SubGoal {
    label: String,
    category: SubGoalCategory,
}

/// Broad category assigned to a sub-goal by the keyword extractor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubGoalCategory {
    Build,
    Test,
    Fix,
    Lint,
    Deploy,
    Investigate,
    Document,
    Refactor,
    Generic,
}

// ---------------------------------------------------------------------------
// GoalDecomposer
// ---------------------------------------------------------------------------

/// Rule-based goal decomposer.
///
/// Call [`decompose`](GoalDecomposer::decompose) to run the full two-pass
/// pipeline. Constructing a `GoalDecomposer` is free — no heap allocation
/// beyond the returned result.
#[derive(Debug, Default)]
pub struct GoalDecomposer;

impl GoalDecomposer {
    /// Create a new `GoalDecomposer`.
    pub fn new() -> Self {
        Self
    }

    // -----------------------------------------------------------------------
    // Public API
    // -----------------------------------------------------------------------

    /// Decompose `goal` into a prioritised sequence of [`DecompositionStep`]s.
    ///
    /// # Pass 1 — Synthesize
    ///
    /// Extracts sub-goals from the goal string using keyword matching and
    /// simple heuristics. If no keywords match, falls back to a single
    /// generic investigate sub-goal.
    ///
    /// # Pass 2 — Decompose
    ///
    /// For each sub-goal, generates 1–3 concrete `DecompositionStep`s with
    /// estimated `cost_ms` and `priority`. Steps that depend on `context`
    /// (e.g. whether errors were recently detected) get higher priority.
    ///
    /// # Returns
    ///
    /// A [`DecompositionResult`] whose `steps` are sorted in descending
    /// priority order. Returns an empty-steps result for an empty goal.
    pub fn decompose(&self, goal: &str, context: &WorldState) -> DecompositionResult {
        let goal_str = goal.trim();

        if goal_str.is_empty() {
            return DecompositionResult {
                goal: goal_str.to_string(),
                steps: Vec::new(),
                estimated_total_ms: 0,
            };
        }

        // Pass 1: Synthesize sub-goals.
        let sub_goals = self.synthesize(goal_str);

        // Pass 2: Decompose each sub-goal into concrete steps.
        let mut steps: Vec<DecompositionStep> = sub_goals
            .iter()
            .flat_map(|sg| self.decompose_sub_goal(sg, context))
            .collect();

        // Sort descending by priority (highest first).
        steps.sort_by(|a, b| {
            b.priority
                .partial_cmp(&a.priority)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let estimated_total_ms = steps
            .iter()
            .map(|s| s.cost_ms)
            .fold(0u32, u32::saturating_add);

        DecompositionResult {
            goal: goal_str.to_string(),
            steps,
            estimated_total_ms,
        }
    }

    // -----------------------------------------------------------------------
    // Pass 1: Synthesize — keyword extraction
    // -----------------------------------------------------------------------

    fn synthesize(&self, goal: &str) -> Vec<SubGoal> {
        let lower = goal.to_lowercase();
        let mut sub_goals: Vec<SubGoal> = Vec::new();

        // Build a bitmask of detected categories so we don't duplicate.
        let mut seen = [false; 9];

        // Keyword → category mapping (ordered from most specific to least).
        let keywords: &[(&[&str], SubGoalCategory)] = &[
            (
                &["deploy", "release", "ship", "publish", "push"],
                SubGoalCategory::Deploy,
            ),
            (
                &["document", "docs", "readme", "comment"],
                SubGoalCategory::Document,
            ),
            (
                &[
                    "refactor",
                    "restructure",
                    "reorganize",
                    "cleanup",
                    "clean up",
                ],
                SubGoalCategory::Refactor,
            ),
            (&["lint", "clippy", "fmt", "format"], SubGoalCategory::Lint),
            (
                &["fix", "repair", "resolve", "patch", "debug", "correct"],
                SubGoalCategory::Fix,
            ),
            (
                &["test", "tests", "spec", "coverage", "check"],
                SubGoalCategory::Test,
            ),
            (
                &["build", "compile", "cargo", "make", "assemble"],
                SubGoalCategory::Build,
            ),
            (
                &[
                    "investigate",
                    "explore",
                    "research",
                    "analyse",
                    "analyze",
                    "understand",
                    "review",
                    "look",
                ],
                SubGoalCategory::Investigate,
            ),
            (
                &["implement", "add", "create", "write", "new", "feature"],
                SubGoalCategory::Generic,
            ),
        ];

        for (kws, category) in keywords {
            let idx = *category as usize;
            if seen[idx] {
                continue;
            }
            if kws.iter().any(|kw| lower.contains(kw)) {
                seen[idx] = true;
                sub_goals.push(SubGoal {
                    label: self.label_for(goal, *category),
                    category: *category,
                });
            }
        }

        // Fallback: if nothing matched, treat the whole goal as an investigation.
        if sub_goals.is_empty() {
            sub_goals.push(SubGoal {
                label: format!("investigate: {goal}"),
                category: SubGoalCategory::Investigate,
            });
        }

        sub_goals
    }

    // -----------------------------------------------------------------------
    // Pass 2: Decompose — generate concrete steps per sub-goal
    // -----------------------------------------------------------------------

    fn decompose_sub_goal(&self, sub: &SubGoal, context: &WorldState) -> Vec<DecompositionStep> {
        // Context modifiers: bump priority when world state is relevant.
        let errors_boost = if context.errors_detected() { 0.2 } else { 0.0 };

        match sub.category {
            SubGoalCategory::Fix => {
                let base_priority = (0.85_f32 + errors_boost).min(1.0);
                vec![
                    DecompositionStep::new(
                        "read failing output and identify root cause",
                        Some("ReadFile".into()),
                        base_priority,
                        500,
                    ),
                    DecompositionStep::new(
                        format!("apply fix: {}", sub.label),
                        Some("WriteFile".into()),
                        base_priority - 0.05,
                        2_000,
                    ),
                    DecompositionStep::new(
                        "verify fix by re-running the failing command",
                        Some("RunCommand".into()),
                        base_priority - 0.10,
                        5_000,
                    ),
                ]
            }

            SubGoalCategory::Test => {
                let base_priority = (0.75_f32 + errors_boost).min(1.0);
                vec![
                    DecompositionStep::new(
                        "run existing test suite and capture results",
                        Some("RunCommand".into()),
                        base_priority,
                        10_000,
                    ),
                    DecompositionStep::new(
                        format!("write or update tests for: {}", sub.label),
                        Some("WriteFile".into()),
                        base_priority - 0.05,
                        3_000,
                    ),
                ]
            }

            SubGoalCategory::Build => {
                vec![DecompositionStep::new(
                    "run build command and collect compiler output",
                    Some("RunCommand".into()),
                    0.80,
                    15_000,
                )]
            }

            SubGoalCategory::Lint => {
                vec![
                    DecompositionStep::new(
                        "run linter / formatter and report violations",
                        Some("RunCommand".into()),
                        0.65,
                        5_000,
                    ),
                    DecompositionStep::new(
                        "apply automatic fixes where available",
                        Some("RunCommand".into()),
                        0.60,
                        3_000,
                    ),
                ]
            }

            SubGoalCategory::Deploy => {
                vec![
                    DecompositionStep::new(
                        "run pre-flight checks (tests, lint)",
                        Some("RunCommand".into()),
                        0.90,
                        20_000,
                    ),
                    DecompositionStep::new(
                        format!("execute deployment: {}", sub.label),
                        Some("RunCommand".into()),
                        0.85,
                        30_000,
                    ),
                    DecompositionStep::new(
                        "validate deployment health",
                        Some("RunCommand".into()),
                        0.80,
                        10_000,
                    ),
                ]
            }

            SubGoalCategory::Investigate => {
                vec![
                    DecompositionStep::new(
                        format!("gather context for: {}", sub.label),
                        Some("ReadFile".into()),
                        0.50,
                        1_000,
                    ),
                    DecompositionStep::new(
                        "summarise findings and propose next steps",
                        None,
                        0.45,
                        500,
                    ),
                ]
            }

            SubGoalCategory::Document => {
                vec![
                    DecompositionStep::new(
                        "read existing documentation and identify gaps",
                        Some("ReadFile".into()),
                        0.55,
                        1_000,
                    ),
                    DecompositionStep::new(
                        format!("write documentation: {}", sub.label),
                        Some("WriteFile".into()),
                        0.50,
                        4_000,
                    ),
                ]
            }

            SubGoalCategory::Refactor => {
                vec![
                    DecompositionStep::new(
                        "identify code to refactor and plan changes",
                        Some("ReadFile".into()),
                        0.70,
                        2_000,
                    ),
                    DecompositionStep::new(
                        format!("apply refactor: {}", sub.label),
                        Some("WriteFile".into()),
                        0.65,
                        5_000,
                    ),
                    DecompositionStep::new(
                        "run tests to confirm behaviour is unchanged",
                        Some("RunCommand".into()),
                        0.60,
                        10_000,
                    ),
                ]
            }

            SubGoalCategory::Generic => {
                vec![
                    DecompositionStep::new(
                        format!("implement: {}", sub.label),
                        Some("WriteFile".into()),
                        0.70,
                        5_000,
                    ),
                    DecompositionStep::new(
                        "run tests to verify implementation",
                        Some("RunCommand".into()),
                        0.65,
                        10_000,
                    ),
                ]
            }
        }
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    /// Build a short human-readable label for a sub-goal.
    fn label_for(&self, goal: &str, category: SubGoalCategory) -> String {
        let prefix = match category {
            SubGoalCategory::Build => "build",
            SubGoalCategory::Test => "tests",
            SubGoalCategory::Fix => "fix",
            SubGoalCategory::Lint => "lint",
            SubGoalCategory::Deploy => "deploy",
            SubGoalCategory::Investigate => "investigate",
            SubGoalCategory::Document => "document",
            SubGoalCategory::Refactor => "refactor",
            SubGoalCategory::Generic => "implement",
        };
        // Trim to 60 chars so labels stay readable.
        let goal_short: String = goal.chars().take(60).collect();
        format!("{prefix}: {goal_short}")
    }
}

// ---------------------------------------------------------------------------
// Orchestrator integration
// ---------------------------------------------------------------------------

impl Orchestrator {
    /// Run the decomposition pipeline and install the result as the new plan
    /// when a replan is warranted.
    ///
    /// This is the bridge between the two loops:
    ///
    /// | Outer loop trigger        | Calls this method                    |
    /// |---------------------------|--------------------------------------|
    /// | `should_replan` → `true`  | `decompose_and_replan` is invoked    |
    /// | Result                    | Ledger plan replaced with new steps  |
    ///
    /// The returned [`DecompositionResult`] lets the caller inspect what was
    /// planned (useful for logging and tests) without needing to re-query the
    /// ledger.
    ///
    /// # Returns
    ///
    /// `Some(result)` when a replan was performed and new steps were installed.
    /// `None` when `should_replan` returned `false` — the plan is still valid.
    pub fn decompose_and_replan(
        &mut self,
        world: &WorldState,
        decomposer: &GoalDecomposer,
    ) -> Option<DecompositionResult> {
        if !self.should_replan(world) {
            return None;
        }

        let goal = self.ledger().goal.clone();
        let result = decomposer.decompose(&goal, world);

        // Convert DecompositionStep → PlanStep and install as the new plan.
        let plan_steps: Vec<PlanStep> = result
            .steps()
            .iter()
            .map(|ds| {
                PlanStep::new(
                    ds.description(),
                    AgentTask::FreeForm {
                        prompt: format!(
                            "{} [tool: {}]",
                            ds.description(),
                            ds.tool().unwrap_or("none")
                        ),
                    },
                )
            })
            .collect();

        self.replan(plan_steps);

        Some(result)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestrator::{PlanStep, StepStatus, WorldState as OrchestratorWorldState};

    fn default_world() -> OrchestratorWorldState {
        OrchestratorWorldState::default()
    }

    fn errors_world() -> OrchestratorWorldState {
        OrchestratorWorldState::builder()
            .errors_detected(true)
            .build()
    }

    // =======================================================================
    // Test 1: empty goal produces empty steps
    // =======================================================================

    #[test]
    fn empty_goal_produces_empty_steps() {
        let decomposer = GoalDecomposer::new();
        let world = default_world();

        let result = decomposer.decompose("", &world);

        assert!(
            result.steps().is_empty(),
            "empty goal must produce no steps"
        );
        assert!(
            !result.is_achievable(),
            "empty result must not be achievable"
        );
        assert_eq!(result.estimated_total_ms(), 0);
    }

    // =======================================================================
    // Test 2: whitespace-only goal is treated as empty
    // =======================================================================

    #[test]
    fn whitespace_goal_produces_empty_steps() {
        let decomposer = GoalDecomposer::new();
        let result = decomposer.decompose("   ", &default_world());
        assert!(result.steps().is_empty());
        assert_eq!(result.goal(), "");
    }

    // =======================================================================
    // Test 3: "fix tests" decomposes into multiple steps
    // =======================================================================

    #[test]
    fn fix_tests_decomposes_into_steps() {
        let decomposer = GoalDecomposer::new();
        let world = default_world();

        let result = decomposer.decompose("fix tests", &world);

        assert!(
            result.is_achievable(),
            "fix tests must produce at least one step"
        );
        // Should produce at minimum a Fix sub-goal and a Test sub-goal.
        assert!(
            result.steps().len() >= 2,
            "expected >= 2 steps for 'fix tests', got {}",
            result.steps().len()
        );
    }

    // =======================================================================
    // Test 4: steps are returned in descending priority order
    // =======================================================================

    #[test]
    fn steps_are_in_priority_order() {
        let decomposer = GoalDecomposer::new();
        let world = default_world();

        let result = decomposer.decompose("fix the failing tests and deploy", &world);

        let priorities: Vec<f32> = result.steps().iter().map(|s| s.priority()).collect();
        for window in priorities.windows(2) {
            assert!(
                window[0] >= window[1],
                "steps must be in descending priority order: {:?}",
                priorities
            );
        }
    }

    // =======================================================================
    // Test 5: cost estimation is the sum of individual step costs
    // =======================================================================

    #[test]
    fn cost_estimation_is_sum_of_step_costs() {
        let decomposer = GoalDecomposer::new();
        let world = default_world();

        let result = decomposer.decompose("build the project", &world);

        let manual_sum: u32 = result
            .steps()
            .iter()
            .map(|s| s.cost_ms())
            .fold(0, u32::saturating_add);

        assert_eq!(result.estimated_total_ms(), manual_sum);
        assert!(
            result.estimated_total_ms() > 0,
            "build goal must have non-zero cost"
        );
    }

    // =======================================================================
    // Test 6: achievability check — non-empty steps → achievable
    // =======================================================================

    #[test]
    fn achievability_check_true_when_steps_exist() {
        let decomposer = GoalDecomposer::new();
        let result = decomposer.decompose("fix the bug", &default_world());

        assert!(result.is_achievable());
    }

    // =======================================================================
    // Test 7: WorldState context — errors bump fix-step priority
    // =======================================================================

    #[test]
    fn errors_detected_bumps_fix_priority() {
        let decomposer = GoalDecomposer::new();

        let no_error_result = decomposer.decompose("fix the crash", &default_world());
        let error_result = decomposer.decompose("fix the crash", &errors_world());

        // The highest-priority fix step should be >= the no-error version.
        let no_err_max = no_error_result
            .steps()
            .iter()
            .map(|s| s.priority())
            .fold(0.0_f32, f32::max);
        let err_max = error_result
            .steps()
            .iter()
            .map(|s| s.priority())
            .fold(0.0_f32, f32::max);

        assert!(
            err_max >= no_err_max,
            "errors_detected should bump priority: no_err={no_err_max:.2} err={err_max:.2}"
        );
    }

    // =======================================================================
    // Test 8: unknown / uncategorised goal falls back to investigate
    // =======================================================================

    #[test]
    fn unknown_goal_falls_back_to_investigate() {
        let decomposer = GoalDecomposer::new();
        // No keywords at all.
        let result = decomposer.decompose("xyzzy frobnicate the quux", &default_world());

        assert!(result.is_achievable(), "fallback must still be achievable");
        // At least one step should mention investigate / context.
        let has_investigate = result.steps().iter().any(|s| {
            s.description().contains("gather context") || s.description().contains("investigate")
        });
        assert!(
            has_investigate,
            "fallback goal should produce an investigate step; got: {:?}",
            result
                .steps()
                .iter()
                .map(|s| s.description())
                .collect::<Vec<_>>()
        );
    }

    // =======================================================================
    // Test 9: deploy goal produces ≥ 3 steps (pre-flight + deploy + health)
    // =======================================================================

    #[test]
    fn deploy_goal_produces_three_steps() {
        let decomposer = GoalDecomposer::new();
        let result = decomposer.decompose("deploy to production", &default_world());

        assert!(
            result.steps().len() >= 3,
            "deploy must produce at least 3 steps, got {}",
            result.steps().len()
        );
    }

    // =======================================================================
    // Test 10: priority values are clamped to [0.0, 1.0]
    // =======================================================================

    #[test]
    fn priority_values_are_clamped() {
        let decomposer = GoalDecomposer::new();
        // errors_detected = true pushes base_priority + 0.2, which could
        // overflow 1.0 without clamping.
        let result = decomposer.decompose("fix critical crash", &errors_world());

        for step in result.steps() {
            assert!(
                step.priority() >= 0.0 && step.priority() <= 1.0,
                "priority out of range: {}",
                step.priority()
            );
        }
    }

    // =======================================================================
    // Test 11: Orchestrator::decompose_and_replan returns None when no replan
    // =======================================================================

    #[test]
    fn decompose_and_replan_returns_none_when_steady() {
        let mut orch = Orchestrator::new("fix the build");
        orch.set_plan(vec![PlanStep::new(
            "step 1",
            phantom_agents::AgentTask::FreeForm {
                prompt: "s1".into(),
            },
        )]);
        let world = OrchestratorWorldState::default(); // no triggers

        let decomposer = GoalDecomposer::new();
        let result = orch.decompose_and_replan(&world, &decomposer);

        assert!(result.is_none(), "no replan trigger → should return None");
    }

    // =======================================================================
    // Test 12: Orchestrator::decompose_and_replan installs new plan on trigger
    // =======================================================================

    #[test]
    fn decompose_and_replan_installs_plan_on_trigger() {
        let mut orch = Orchestrator::new("fix the failing tests");
        orch.set_plan(vec![PlanStep::new(
            "old step",
            phantom_agents::AgentTask::FreeForm {
                prompt: "old".into(),
            },
        )]);

        // Trigger a replan via errors_detected.
        let world = OrchestratorWorldState::builder()
            .errors_detected(true)
            .build();
        let decomposer = GoalDecomposer::new();

        let result = orch.decompose_and_replan(&world, &decomposer);

        assert!(
            result.is_some(),
            "errors_detected trigger → should produce a result"
        );
        let dr = result.unwrap();
        assert!(
            dr.is_achievable(),
            "decomposition should produce at least one step"
        );

        // The ledger plan should have been replaced.
        let new_plan = &orch.ledger().plan;
        assert!(
            !new_plan.is_empty(),
            "ledger plan must be non-empty after decompose_and_replan"
        );
        // Old step should be gone.
        let still_has_old = new_plan.iter().any(|s| s.description == "old step");
        assert!(!still_has_old, "old step should be replaced after replan");
    }

    // =======================================================================
    // Test 13: all steps have non-empty descriptions and non-zero cost
    // =======================================================================

    #[test]
    fn all_steps_have_valid_fields() {
        let decomposer = GoalDecomposer::new();
        let goals = [
            "fix the build",
            "run the tests",
            "refactor the module",
            "document the API",
            "deploy to staging",
        ];

        for goal in &goals {
            let result = decomposer.decompose(goal, &default_world());
            for step in result.steps() {
                assert!(
                    !step.description().is_empty(),
                    "step for '{goal}' has empty description"
                );
                assert!(
                    step.cost_ms() > 0,
                    "step for '{goal}' has zero cost_ms: {}",
                    step.description()
                );
                assert!(
                    step.priority() > 0.0,
                    "step for '{goal}' has zero priority: {}",
                    step.description()
                );
            }
        }
    }

    // =======================================================================
    // Test 14: step with no tool has tool() == None
    // =======================================================================

    #[test]
    fn investigate_summary_step_has_no_tool() {
        let decomposer = GoalDecomposer::new();
        // The "summarise findings" step from the Investigate category has tool = None.
        let result = decomposer.decompose("xyzzy unknown goal", &default_world());

        let no_tool_step = result.steps().iter().find(|s| s.tool().is_none());
        assert!(
            no_tool_step.is_some(),
            "expected at least one step with tool=None for investigate fallback"
        );
    }

    // =======================================================================
    // Test 15: multi-keyword goal produces steps from multiple sub-goals
    // =======================================================================

    #[test]
    fn multi_keyword_goal_produces_multi_subgoal_steps() {
        let decomposer = GoalDecomposer::new();
        // "build" + "test" + "fix" → at least 3 sub-goals → many steps.
        let result = decomposer.decompose(
            "build the project, fix errors, and run tests",
            &default_world(),
        );

        // Expect steps from at least 3 different descriptions.
        assert!(
            result.steps().len() >= 4,
            "multi-keyword goal must produce >= 4 steps, got {}",
            result.steps().len()
        );
    }

    // =======================================================================
    // Test 16: all steps in the ledger plan after decompose_and_replan are
    //           initially Pending (not Active/Done/etc.)
    // =======================================================================

    #[test]
    fn replan_steps_start_as_pending() {
        let mut orch = Orchestrator::new("deploy the service");
        let world = OrchestratorWorldState::builder()
            .errors_detected(true)
            .build();
        let decomposer = GoalDecomposer::new();

        orch.decompose_and_replan(&world, &decomposer);

        for step in &orch.ledger().plan {
            assert_eq!(
                step.status,
                StepStatus::Pending,
                "new plan step '{}' must start as Pending",
                step.description
            );
        }
    }
}
