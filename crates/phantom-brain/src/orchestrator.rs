//! Goal-directed multi-agent orchestration (Magentic-One patterns).
//!
//! Implements the Orchestrator's **Task Ledger** and nested-loop re-planning
//! mechanism from "Magentic-One: A Generalist Multi-Agent System for Solving
//! Complex Tasks" (Fourney et al., 2024). Adapted for Phantom's terminal
//! context where "agents" are [`AgentTask`] instances managed by the brain.
//!
//! # Architecture mapping
//!
//! | Magentic-One concept   | Phantom equivalent                     |
//! |------------------------|----------------------------------------|
//! | Orchestrator           | `brain_loop` + `TaskLedger`            |
//! | Specialist agents      | `AgentTask` variants + `AgentManager`  |
//! | Inner loop             | Per-plan step execution via agents      |
//! | Outer loop             | `TaskLedger::should_replan` trigger     |
//! | Task ledger            | `TaskLedger` struct                    |
//! | Progress ledger        | `ProgressAssessment` (5-question eval) |

use std::collections::{HashSet, VecDeque};
use std::time::Instant;

use phantom_agents::AgentTask;

// ---------------------------------------------------------------------------
// Fact classification (task ledger knowledge categories)
// ---------------------------------------------------------------------------

/// How confident we are in a piece of knowledge.
///
/// Mirrors Magentic-One's four-tier fact classification:
/// - Verified: confirmed through tool output or agent results
/// - ToLookUp: needs external verification (file read, web, etc.)
/// - ToDerive: needs computation or multi-step reasoning
/// - Guess: inferred from context, may be wrong
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactConfidence {
    Verified,
    ToLookUp,
    ToDerive,
    Guess,
}

/// A fact in the task ledger's knowledge base.
#[derive(Debug, Clone)]
pub struct Fact {
    /// The actual content of the fact.
    pub content: String,
    /// How confident we are in this fact.
    pub confidence: FactConfidence,
    /// When this fact was recorded or last updated.
    pub updated_at: Instant,
    /// Source: which agent or event produced this fact.
    pub source: String,
}

// ---------------------------------------------------------------------------
// PlanStep
// ---------------------------------------------------------------------------

/// Status of a single step in a plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepStatus {
    /// Not yet started.
    Pending,
    /// An agent is actively working on this step.
    Active,
    /// Completed successfully.
    Done,
    /// Failed after agent attempt(s).
    Failed,
    /// Skipped (e.g., became irrelevant after re-plan).
    Skipped,
}

/// A single step in the orchestrator's plan.
///
/// From Magentic-One: "The plan is expressed in natural language and consists
/// of a sequence of steps and assignments of those steps to individual agents."
/// In Phantom, each step maps to an `AgentTask` variant and carries its own
/// status tracking for the inner-loop progress assessment.
///
/// # DAG dependency ordering (Issue #60)
///
/// Each step may declare prerequisite steps via `depends_on: Vec<usize>`.
/// The indices are positions in the `TaskLedger::plan` slice. A step is
/// *eligible* only when every step in its dependency set has reached
/// `StepStatus::Done`. Use `TaskLedger::eligible_next()` to query the
/// full set of runnable steps, and `TaskLedger::has_cycle()` to validate
/// a plan before accepting it.
///
/// # Provider routing (Issue #61)
///
/// When `preferred_provider` is `Some(id)`, the dispatch layer should look up
/// `id` in the `ProviderCatalog` and route the `AgentTask` to that backend.
/// If the ID is unknown the catalog falls back to `"claude-default"`.
#[derive(Debug, Clone)]
pub struct PlanStep {
    /// Human-readable description of what this step accomplishes.
    pub description: String,
    /// Which agent type should handle this step.
    pub assigned_task: AgentTask,
    /// Current execution status.
    pub status: StepStatus,
    /// The agent ID if one has been spawned for this step.
    ///
    /// Stored as `u64` to match [`phantom_agents::AgentId`] (fixes #273 — no
    /// narrowing cast required at the reconciler / quarantine boundary).
    pub agent_id: Option<u64>,
    /// Number of times this step has been attempted (for retry tracking).
    pub attempts: u32,
    /// Maximum attempts before marking as Failed.
    pub max_attempts: u32,
    /// Output/result summary from the agent (if completed).
    pub result_summary: Option<String>,
    /// Prerequisite step indices (Issue #60 — Task DAG).
    ///
    /// This step will not be eligible for dispatch until every step whose
    /// index appears here has reached `StepStatus::Done`.
    depends_on: Vec<usize>,
    /// Preferred provider ID for routing this step (Issue #61).
    ///
    /// When `Some`, the dispatch layer resolves this ID in the
    /// `ProviderCatalog`. Unknown IDs fall back to `"claude-default"`.
    preferred_provider: Option<String>,
}

impl PlanStep {
    /// Create a new pending plan step with no dependencies.
    pub fn new(description: impl Into<String>, task: AgentTask) -> Self {
        Self {
            description: description.into(),
            assigned_task: task,
            status: StepStatus::Pending,
            agent_id: None,
            attempts: 0,
            max_attempts: 3,
            result_summary: None,
            depends_on: Vec::new(),
            preferred_provider: None,
        }
    }

    /// Create a pending step that must wait for `depends_on` steps to finish.
    ///
    /// # Example
    /// ```ignore
    /// // step 1 (index 1) must complete before step 2 runs
    /// let step2 = PlanStep::with_deps("run tests", task, vec![1]);
    /// ```
    pub fn with_deps(
        description: impl Into<String>,
        task: AgentTask,
        depends_on: Vec<usize>,
    ) -> Self {
        Self {
            depends_on,
            ..Self::new(description, task)
        }
    }

    /// Attach a preferred provider ID to this step (builder-style).
    ///
    /// # Example
    /// ```ignore
    /// let step = PlanStep::new("fast check", task)
    ///     .with_provider("claude-fast");
    /// ```
    pub fn with_provider(mut self, provider_id: impl Into<String>) -> Self {
        self.preferred_provider = Some(provider_id.into());
        self
    }

    /// Prerequisite step indices for this step.
    ///
    /// A step is not eligible for dispatch until every index in this slice
    /// has reached [`StepStatus::Done`].
    pub fn depends_on(&self) -> &[usize] {
        &self.depends_on
    }

    /// Preferred provider ID for routing this step.
    ///
    /// When `Some`, the dispatch layer resolves the ID in the `ProviderCatalog`.
    /// Unknown IDs fall back to `"claude-default"`.
    pub fn preferred_provider(&self) -> Option<&str> {
        self.preferred_provider.as_deref()
    }

    /// Record a failed attempt. Returns true if retries remain.
    pub fn record_failure(&mut self, summary: impl Into<String>) -> bool {
        self.attempts += 1;
        self.result_summary = Some(summary.into());
        if self.attempts >= self.max_attempts {
            self.status = StepStatus::Failed;
            false
        } else {
            self.status = StepStatus::Pending; // re-queue
            true
        }
    }

    /// Record a successful completion.
    pub fn record_success(&mut self, summary: impl Into<String>) {
        self.status = StepStatus::Done;
        self.result_summary = Some(summary.into());
    }
}

// ---------------------------------------------------------------------------
// ProgressAssessment (Magentic-One's five questions)
// ---------------------------------------------------------------------------

/// The orchestrator's inner-loop progress assessment.
///
/// Magentic-One's orchestrator "answers five questions to create the progress
/// ledger" at each step. This struct captures those answers as structured
/// data rather than free-text, making them actionable by the brain loop.
#[derive(Debug, Clone)]
pub struct ProgressAssessment {
    /// Q1: "Is the request fully satisfied (i.e., task complete)?"
    pub is_complete: bool,
    /// Q2: "Is the team looping or repeating itself?"
    pub is_looping: bool,
    /// Q3: "Is forward progress being made?"
    pub has_progress: bool,
    /// Q4: "Which agent should speak next?" (index into plan steps)
    pub next_step_idx: Option<usize>,
    /// Q5: "What instruction or question should be asked of this team member?"
    pub next_instruction: Option<String>,
    /// When this assessment was made.
    pub assessed_at: Instant,
}

// ---------------------------------------------------------------------------
// TaskLedger
// ---------------------------------------------------------------------------

/// The orchestrator's task ledger -- short-term memory for a goal-directed
/// multi-step task.
///
/// From Magentic-One: the task ledger contains "verified facts, facts to look
/// up, facts to derive, and educated guesses" plus a natural-language plan.
/// The outer loop updates the ledger when the inner loop gets stuck; the
/// inner loop reads it to decide what agent to dispatch next.
///
/// In Phantom, this replaces ad-hoc single-shot agent spawning with a
/// structured plan-execute-assess-replan cycle.
#[derive(Debug)]
pub struct TaskLedger {
    /// The original user goal / task description.
    pub goal: String,
    /// Knowledge base: facts gathered during execution.
    pub facts: Vec<Fact>,
    /// The current plan (sequence of steps to achieve the goal).
    pub plan: Vec<PlanStep>,
    /// History of previous plans (for re-plan diffing).
    pub plan_history: Vec<Vec<PlanStep>>,
    /// How many consecutive inner-loop iterations showed no progress.
    /// Magentic-One calls this "a counter for how long the team has been
    /// stuck or stalled."
    pub stall_counter: u32,
    /// Threshold: if `stall_counter` exceeds this, trigger outer-loop re-plan.
    /// Magentic-One uses <= 2 in experiments.
    pub stall_threshold: u32,
    /// Total outer-loop iterations (re-plans).
    pub replan_count: u32,
    /// Maximum outer-loop iterations before giving up.
    pub max_replans: u32,
    /// Most recent progress assessment.
    pub last_assessment: Option<ProgressAssessment>,
    /// When this ledger was created.
    pub created_at: Instant,
    /// When the last outer-loop iteration started.
    pub last_replan_at: Option<Instant>,
    /// Recent agent outputs for loop detection (circular buffer).
    recent_outputs: VecDeque<String>,
}

impl TaskLedger {
    /// Create a new task ledger for a goal.
    pub fn new(goal: impl Into<String>) -> Self {
        Self {
            goal: goal.into(),
            facts: Vec::new(),
            plan: Vec::new(),
            plan_history: Vec::new(),
            stall_counter: 0,
            stall_threshold: 2,
            replan_count: 0,
            max_replans: 5,
            last_assessment: None,
            created_at: Instant::now(),
            last_replan_at: None,
            recent_outputs: VecDeque::with_capacity(10),
        }
    }

    // -- Fact management ---------------------------------------------------

    /// Add a fact to the knowledge base.
    pub fn add_fact(
        &mut self,
        content: impl Into<String>,
        confidence: FactConfidence,
        source: impl Into<String>,
    ) {
        self.facts.push(Fact {
            content: content.into(),
            confidence,
            updated_at: Instant::now(),
            source: source.into(),
        });
    }

    /// Promote a guess to verified when an agent confirms it.
    pub fn verify_fact(&mut self, content_prefix: &str) {
        for fact in &mut self.facts {
            if fact.confidence == FactConfidence::Guess
                && fact.content.starts_with(content_prefix)
            {
                fact.confidence = FactConfidence::Verified;
                fact.updated_at = Instant::now();
            }
        }
    }

    /// Get all facts at a given confidence level.
    pub fn facts_at(&self, confidence: FactConfidence) -> Vec<&Fact> {
        self.facts
            .iter()
            .filter(|f| f.confidence == confidence)
            .collect()
    }

    // -- Plan management ---------------------------------------------------

    /// Set the initial plan (from the orchestrator's first pass).
    ///
    /// If the dependency graph formed by `steps[i].depends_on` contains a
    /// cycle, **all steps are immediately marked `Failed`** and the ledger
    /// is left in a non-executable state so the reconciler can detect and
    /// report the problem without attempting to dispatch any step.
    pub fn set_plan(&mut self, steps: Vec<PlanStep>) {
        self.plan = steps;
        self.stall_counter = 0;
        if self.has_cycle() {
            log::error!(
                "TaskLedger::set_plan — dependency cycle detected; blocking all steps"
            );
            for step in &mut self.plan {
                step.status = StepStatus::Failed;
                step.result_summary =
                    Some("blocked: dependency cycle detected in plan".into());
            }
        }
    }

    /// Returns all `Pending` steps whose dependency constraints are satisfied.
    ///
    /// A step is *eligible* when every index in `step.depends_on` refers to
    /// a step that has already reached `StepStatus::Done`. Steps with no
    /// dependencies are always eligible (when `Pending`).
    ///
    /// The returned pairs are `(step_index, &PlanStep)` so the caller can
    /// simultaneously track position and content without a second lookup.
    ///
    /// # Complexity
    ///
    /// O(n × d) where n = number of plan steps and d = average dependency
    /// fan-in. For typical plans (n < 20, d < 5) this is negligible.
    pub fn eligible_next(&self) -> Vec<(usize, &PlanStep)> {
        // Collect the index set of all completed steps.
        let done: HashSet<usize> = self
            .plan
            .iter()
            .enumerate()
            .filter(|(_, s)| s.status == StepStatus::Done)
            .map(|(i, _)| i)
            .collect();

        self.plan
            .iter()
            .enumerate()
            .filter(|(_, s)| {
                s.status == StepStatus::Pending
                    && s.depends_on
                        .iter()
                        .all(|dep| *dep >= self.plan.len() || done.contains(dep))
            })
            .collect()
    }

    /// Returns `true` if the dependency graph contains a cycle.
    ///
    /// Uses an iterative depth-first search with three-colour marking
    /// (white = 0, grey = 1, black = 2) to avoid stack overflows on deep
    /// plans. Out-of-bounds indices in `depends_on` are silently skipped
    /// (they cannot form a cycle with anything).
    ///
    /// This is called automatically by `set_plan`; it can also be called
    /// before committing a plan to validate it cheaply.
    ///
    /// # Complexity
    ///
    /// O(n + e) where n = number of steps and e = total edges (sum of all
    /// `depends_on` lengths).
    pub fn has_cycle(&self) -> bool {
        let n = self.plan.len();
        // 0 = white (unvisited), 1 = grey (in stack), 2 = black (finished)
        let mut color = vec![0u8; n];

        for start in 0..n {
            if color[start] != 0 {
                continue; // already fully explored
            }

            // Stack entries: (node_index, edge_cursor).
            // edge_cursor tracks which dep we are about to explore next.
            let mut stack: Vec<(usize, usize)> = vec![(start, 0)];
            color[start] = 1; // grey: on stack

            while let Some((node, edge_idx)) = stack.last_mut() {
                let node = *node;
                let deps = &self.plan[node].depends_on;

                if *edge_idx < deps.len() {
                    let dep = deps[*edge_idx];
                    *edge_idx += 1;

                    if dep >= n {
                        // Out-of-bounds: skip silently.
                        continue;
                    }

                    match color[dep] {
                        1 => return true,  // back-edge → cycle
                        0 => {
                            // Tree-edge: push and colour grey.
                            color[dep] = 1;
                            stack.push((dep, 0));
                        }
                        _ => {} // already black (finished): safe cross-edge
                    }
                } else {
                    // All neighbours explored: colour black and pop.
                    color[node] = 2;
                    stack.pop();
                }
            }
        }

        false
    }

    /// Replace the current plan with a new one (outer-loop re-plan).
    ///
    /// Archives the old plan in `plan_history` so we can detect repeated
    /// re-plans and extract lessons from failures.
    ///
    /// From Magentic-One: "Since this plan may be revisited with each iteration
    /// of the outer loop, we force all agents to clear their contexts and reset
    /// their states after each plan update."
    pub fn replan(&mut self, new_steps: Vec<PlanStep>) {
        // Archive old plan.
        let old = std::mem::replace(&mut self.plan, new_steps);
        self.plan_history.push(old);
        self.stall_counter = 0;
        self.replan_count += 1;
        self.last_replan_at = Some(Instant::now());
    }

    /// Get the next pending step (if any).
    pub fn next_pending_step(&self) -> Option<(usize, &PlanStep)> {
        self.plan
            .iter()
            .enumerate()
            .find(|(_, s)| s.status == StepStatus::Pending)
    }

    /// Get the currently active step (if any).
    pub fn active_step(&self) -> Option<(usize, &PlanStep)> {
        self.plan
            .iter()
            .enumerate()
            .find(|(_, s)| s.status == StepStatus::Active)
    }

    /// Count steps by status.
    pub fn step_counts(&self) -> StepCounts {
        let mut counts = StepCounts::default();
        for step in &self.plan {
            match step.status {
                StepStatus::Pending => counts.pending += 1,
                StepStatus::Active => counts.active += 1,
                StepStatus::Done => counts.done += 1,
                StepStatus::Failed => counts.failed += 1,
                StepStatus::Skipped => counts.skipped += 1,
            }
        }
        counts
    }

    /// Whether the plan is fully resolved (all steps done/failed/skipped).
    pub fn is_plan_resolved(&self) -> bool {
        self.plan.iter().all(|s| {
            matches!(
                s.status,
                StepStatus::Done | StepStatus::Failed | StepStatus::Skipped
            )
        })
    }

    // -- Progress assessment (inner loop) ----------------------------------

    /// Run the Magentic-One five-question assessment on the current state.
    ///
    /// This is the brain's inner-loop decision point. It examines the plan
    /// state, recent outputs, and stall counter to answer:
    /// 1. Is the task complete?
    /// 2. Is the team looping?
    /// 3. Is forward progress being made?
    /// 4. Which agent should go next?
    /// 5. What should that agent do?
    pub fn assess_progress(&mut self) -> ProgressAssessment {
        let counts = self.step_counts();

        // Q1: Complete if all steps done and no pending work.
        let is_complete =
            counts.pending == 0 && counts.active == 0 && counts.done > 0;

        // Q2: Loop detection via output similarity.
        let is_looping = self.detect_loop();

        // Q3: Progress = at least one step completed since last assessment,
        // or the active step has changed.
        let has_progress = if let Some(ref prev) = self.last_assessment {
            let prev_done = self
                .plan
                .iter()
                .filter(|s| s.status == StepStatus::Done)
                .count();
            // Simple heuristic: if done count hasn't changed and we're not
            // on a new step, no progress.
            prev_done > 0 || prev.assessed_at.elapsed().as_secs() < 5
        } else {
            true // first assessment, assume progress
        };

        // Q4 & Q5: Next step.
        let (next_step_idx, next_instruction) =
            if let Some((idx, step)) = self.next_pending_step() {
                (Some(idx), Some(step.description.clone()))
            } else {
                (None, None)
            };

        // Update stall counter.
        if !has_progress && !is_complete {
            self.stall_counter += 1;
        } else {
            self.stall_counter = 0;
        }

        let assessment = ProgressAssessment {
            is_complete,
            is_looping,
            has_progress,
            next_step_idx,
            next_instruction,
            assessed_at: Instant::now(),
        };

        self.last_assessment = Some(assessment.clone());
        assessment
    }

    // -- Re-plan decision (outer loop) -------------------------------------

    /// The outer-loop re-plan trigger.
    ///
    /// From Magentic-One: "If a loop is detected, or there is a lack of
    /// forward progress, the counter is incremented. [...] if the counter
    /// exceeds the threshold, the Orchestrator breaks from the inner loop."
    ///
    /// Returns `ReplanDecision` indicating what the brain should do.
    pub fn should_replan(&self) -> ReplanDecision {
        // Terminal: exhausted all re-plan attempts.
        if self.replan_count >= self.max_replans {
            return ReplanDecision::GiveUp {
                reason: format!(
                    "exhausted {} re-plan attempts",
                    self.max_replans
                ),
            };
        }

        // Terminal: plan is fully resolved (all steps done/failed/skipped).
        if self.is_plan_resolved() {
            let counts = self.step_counts();
            if counts.failed == 0 {
                return ReplanDecision::Complete;
            }
            // Some steps failed -- re-plan with lessons learned.
            return ReplanDecision::Replan {
                reason: format!(
                    "{} of {} steps failed",
                    counts.failed,
                    self.plan.len()
                ),
                failed_steps: self
                    .plan
                    .iter()
                    .filter(|s| s.status == StepStatus::Failed)
                    .map(|s| s.description.clone())
                    .collect(),
            };
        }

        // Stall detection: inner loop is stuck.
        if self.stall_counter > self.stall_threshold {
            return ReplanDecision::Replan {
                reason: format!(
                    "stalled for {} iterations (threshold: {})",
                    self.stall_counter, self.stall_threshold
                ),
                failed_steps: self
                    .plan
                    .iter()
                    .filter(|s| s.status == StepStatus::Active)
                    .map(|s| s.description.clone())
                    .collect(),
            };
        }

        // Loop detection from last assessment.
        if let Some(ref assessment) = self.last_assessment {
            if assessment.is_looping {
                return ReplanDecision::Replan {
                    reason: "agents are repeating the same outputs".into(),
                    failed_steps: vec![],
                };
            }
        }

        ReplanDecision::Continue
    }

    // -- Loop detection ----------------------------------------------------

    /// Record an agent's output for loop detection.
    pub fn record_output(&mut self, output: impl Into<String>) {
        let text = output.into();
        self.recent_outputs.push_back(text);
        // Keep last 10 outputs.
        while self.recent_outputs.len() > 10 {
            self.recent_outputs.pop_front();
        }
    }

    /// Detect if agents are producing repetitive outputs.
    ///
    /// Checks if any output appears 3+ times in the recent window.
    /// This is a lightweight approximation of Magentic-One's
    /// "Is the team looping or repeating itself?" question.
    fn detect_loop(&self) -> bool {
        if self.recent_outputs.len() < 3 {
            return false;
        }

        // Check for exact duplicates.
        for i in 0..self.recent_outputs.len() {
            let count = self
                .recent_outputs
                .iter()
                .filter(|o| **o == self.recent_outputs[i])
                .count();
            if count >= 3 {
                return true;
            }
        }

        // Check for high similarity (> 80% of outputs share a common prefix).
        if self.recent_outputs.len() >= 4 {
            let last = self.recent_outputs.back().unwrap();
            let similar = self
                .recent_outputs
                .iter()
                .filter(|o| {
                    let prefix_len = last.len().min(o.len()).min(50);
                    prefix_len > 0
                        && last[..prefix_len] == o.as_str()[..prefix_len]
                })
                .count();
            if similar >= 3 {
                return true;
            }
        }

        false
    }

    // -- Context building for LLM prompts ----------------------------------

    /// Build a re-plan prompt context string containing:
    /// - The original goal
    /// - Verified facts
    /// - What was tried and what failed
    /// - Lessons from previous plans
    ///
    /// This is what gets sent to Claude when the outer loop triggers a re-plan.
    pub fn replan_context(&self) -> String {
        let mut ctx = String::with_capacity(2048);

        ctx.push_str(&format!("GOAL: {}\n\n", self.goal));

        // Verified facts.
        let verified = self.facts_at(FactConfidence::Verified);
        if !verified.is_empty() {
            ctx.push_str("VERIFIED FACTS:\n");
            for f in &verified {
                ctx.push_str(&format!("- {} (source: {})\n", f.content, f.source));
            }
            ctx.push('\n');
        }

        // Current guesses.
        let guesses = self.facts_at(FactConfidence::Guess);
        if !guesses.is_empty() {
            ctx.push_str("EDUCATED GUESSES:\n");
            for f in &guesses {
                ctx.push_str(&format!("- {}\n", f.content));
            }
            ctx.push('\n');
        }

        // What was tried.
        ctx.push_str("PREVIOUS ATTEMPTS:\n");
        for (plan_idx, old_plan) in self.plan_history.iter().enumerate() {
            ctx.push_str(&format!("  Plan {}:\n", plan_idx + 1));
            for step in old_plan {
                let status_str = match step.status {
                    StepStatus::Done => "OK",
                    StepStatus::Failed => "FAILED",
                    StepStatus::Skipped => "SKIPPED",
                    _ => "INCOMPLETE",
                };
                ctx.push_str(&format!("    [{}] {}", status_str, step.description));
                if let Some(ref summary) = step.result_summary {
                    ctx.push_str(&format!(" -- {summary}"));
                }
                ctx.push('\n');
            }
        }

        // Current plan state.
        if !self.plan.is_empty() {
            ctx.push_str("\nCURRENT PLAN:\n");
            for (i, step) in self.plan.iter().enumerate() {
                let status_str = match step.status {
                    StepStatus::Pending => "PENDING",
                    StepStatus::Active => "ACTIVE",
                    StepStatus::Done => "DONE",
                    StepStatus::Failed => "FAILED",
                    StepStatus::Skipped => "SKIPPED",
                };
                ctx.push_str(&format!(
                    "  {}. [{}] {} (attempts: {})\n",
                    i + 1,
                    status_str,
                    step.description,
                    step.attempts
                ));
            }
        }

        ctx.push_str(&format!(
            "\nRE-PLAN #{} of {} max. Stall counter: {}/{}.\n",
            self.replan_count + 1,
            self.max_replans,
            self.stall_counter,
            self.stall_threshold
        ));

        ctx
    }
}

// ---------------------------------------------------------------------------
// WorldState
// ---------------------------------------------------------------------------

/// A snapshot of the observable world at a single reconciler tick.
///
/// `should_replan` reads this struct to determine whether the active plan
/// is still valid. Each field corresponds to exactly one replan trigger so
/// that the decision logic is separately testable per trigger.
///
/// # Construction
///
/// ```rust,ignore
/// let world = WorldState::builder()
///     .errors_detected(true)
///     .build();
/// ```
#[derive(Debug, Clone, Default)]
pub struct WorldState {
    /// New compiler / runtime errors appeared since the last tick.
    errors_detected: bool,
    /// At least one agent exceeded its retry limit or stall timeout.
    agent_flatlined: bool,
    /// The user typed a command or otherwise signalled attention.
    user_input_arrived: bool,
    /// The goal string or objective changed externally.
    goal_changed: bool,
    /// The wall-clock budget for this plan iteration has expired.
    time_budget_exhausted: bool,
}

impl WorldState {
    /// Create a builder for constructing a [`WorldState`].
    pub fn builder() -> WorldStateBuilder {
        WorldStateBuilder::default()
    }

    /// `true` when new errors appeared in the terminal output.
    pub fn errors_detected(&self) -> bool {
        self.errors_detected
    }

    /// `true` when an agent flatlined (exhausted retries or stalled).
    pub fn agent_flatlined(&self) -> bool {
        self.agent_flatlined
    }

    /// `true` when the user sent input (command, interrupt, goal change).
    pub fn user_input_arrived(&self) -> bool {
        self.user_input_arrived
    }

    /// `true` when the external goal was updated and the plan no longer targets it.
    pub fn goal_changed(&self) -> bool {
        self.goal_changed
    }

    /// `true` when the wall-clock budget for the current plan iteration is up.
    pub fn time_budget_exhausted(&self) -> bool {
        self.time_budget_exhausted
    }

    /// Returns `true` if any trigger that requires replanning is active.
    ///
    /// This is the central predicate consumed by [`Orchestrator::should_replan`].
    pub fn any_replan_trigger(&self) -> bool {
        self.errors_detected
            || self.agent_flatlined
            || self.user_input_arrived
            || self.goal_changed
            || self.time_budget_exhausted
    }
}

// ---------------------------------------------------------------------------
// WorldStateBuilder
// ---------------------------------------------------------------------------

/// Fluent builder for [`WorldState`].
#[derive(Debug, Default)]
pub struct WorldStateBuilder {
    inner: WorldState,
}

impl WorldStateBuilder {
    /// Signal that new errors were detected in the latest output.
    pub fn errors_detected(mut self, v: bool) -> Self {
        self.inner.errors_detected = v;
        self
    }

    /// Signal that an agent flatlined.
    pub fn agent_flatlined(mut self, v: bool) -> Self {
        self.inner.agent_flatlined = v;
        self
    }

    /// Signal that user input arrived.
    pub fn user_input_arrived(mut self, v: bool) -> Self {
        self.inner.user_input_arrived = v;
        self
    }

    /// Signal that the goal was changed externally.
    pub fn goal_changed(mut self, v: bool) -> Self {
        self.inner.goal_changed = v;
        self
    }

    /// Signal that the time budget for the current plan iteration has expired.
    pub fn time_budget_exhausted(mut self, v: bool) -> Self {
        self.inner.time_budget_exhausted = v;
        self
    }

    /// Consume the builder and produce a [`WorldState`].
    pub fn build(self) -> WorldState {
        self.inner
    }
}

// ---------------------------------------------------------------------------
// Orchestrator
// ---------------------------------------------------------------------------

/// High-level orchestrator that wraps a [`TaskLedger`] and exposes the two
/// methods the reconciler calls each tick.
///
/// The reconciler owns the `Orchestrator` for the lifetime of a goal pursuit.
/// When `should_replan` returns `true`, the reconciler discards the current
/// plan and triggers a Composer agent to generate a new one.
///
/// # Design notes
///
/// - All state is encapsulated: callers cannot accidentally mutate internal
///   ledger fields. Accessors are provided for read-only inspection.
/// - `dispatch_next_step` consumes no `AiAction` channel — it is a pure
///   pull-from-ledger operation. The reconciler handles channel emission.
/// - No `.unwrap()` in production paths: `dispatch_next_step` returns
///   `Option<PlanStep>` so the caller can decide what to do when empty.
pub struct Orchestrator {
    ledger: TaskLedger,
}

impl Orchestrator {
    /// Create a new orchestrator wrapping a fresh [`TaskLedger`] for `goal`.
    pub fn new(goal: impl Into<String>) -> Self {
        Self {
            ledger: TaskLedger::new(goal),
        }
    }

    /// Replace the current plan with `steps`.
    ///
    /// Delegates to [`TaskLedger::set_plan`] for the initial plan or
    /// [`TaskLedger::replan`] for revisions.
    pub fn set_plan(&mut self, steps: Vec<PlanStep>) {
        self.ledger.set_plan(steps);
    }

    /// Replace the current plan with a revised set of steps (outer-loop replan).
    pub fn replan(&mut self, steps: Vec<PlanStep>) {
        self.ledger.replan(steps);
    }

    /// Read-only access to the task ledger for inspection / context building.
    pub fn ledger(&self) -> &TaskLedger {
        &self.ledger
    }

    /// Mutable access to the task ledger for recording outputs and updating
    /// step status from agent completions.
    pub fn ledger_mut(&mut self) -> &mut TaskLedger {
        &mut self.ledger
    }

    // -----------------------------------------------------------------------
    // Core reconciler API
    // -----------------------------------------------------------------------

    /// Determine whether the current plan should be discarded and regenerated.
    ///
    /// Returns `true` when at least one of the following is true:
    ///
    /// | Trigger                   | Cause                                        |
    /// |---------------------------|----------------------------------------------|
    /// | `world.errors_detected`   | New errors invalidate prior assumptions      |
    /// | `world.agent_flatlined`   | Executing agent can no longer make progress  |
    /// | `world.user_input_arrived`| User redirected attention; plan may be stale |
    /// | `world.goal_changed`      | Plan no longer targets the correct objective |
    /// | `world.time_budget_exhausted` | Outer-loop iteration budget expired      |
    ///
    /// Also returns `true` if the inner `TaskLedger::should_replan` evaluation
    /// indicates the plan is stuck or all steps have failed.
    ///
    /// Returns `false` (no replan needed) when:
    /// - None of the world-state triggers are active, AND
    /// - The ledger reports [`ReplanDecision::Continue`], AND
    /// - The ledger reports [`ReplanDecision::Complete`] (goal already done —
    ///   no point generating a new plan).
    pub fn should_replan(&self, world: &WorldState) -> bool {
        // World-state triggers: any single trigger forces a replan.
        if world.any_replan_trigger() {
            return true;
        }

        // Ledger-derived triggers: stall, loop detection, failed steps.
        match self.ledger.should_replan() {
            ReplanDecision::Replan { .. } => true,
            ReplanDecision::GiveUp { .. } => true,
            ReplanDecision::Complete | ReplanDecision::Continue => false,
        }
    }

    /// Pull the next pending step from the task ledger without dispatching it.
    ///
    /// Returns `None` when:
    /// - The plan is empty.
    /// - All steps are `Active`, `Done`, `Failed`, or `Skipped` (no pending work).
    ///
    /// The returned [`PlanStep`] is a **clone** of the ledger entry — the
    /// caller is responsible for advancing the step's status to `Active` via
    /// [`TaskLedger::plan`] after successfully dispatching the returned step.
    pub fn dispatch_next_step(&self) -> Option<PlanStep> {
        self.ledger
            .next_pending_step()
            .map(|(_, step)| step.clone())
    }
}

// ---------------------------------------------------------------------------
// ReplanDecision
// ---------------------------------------------------------------------------

/// What the outer loop decides to do after assessing progress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplanDecision {
    /// Inner loop should continue executing the current plan.
    Continue,
    /// Outer loop should generate a new plan.
    Replan {
        reason: String,
        failed_steps: Vec<String>,
    },
    /// Task is complete (all steps succeeded).
    Complete,
    /// Give up (exhausted re-plan budget).
    GiveUp { reason: String },
}

// ---------------------------------------------------------------------------
// StepCounts
// ---------------------------------------------------------------------------

/// Summary counts of plan step statuses.
#[derive(Debug, Default)]
pub struct StepCounts {
    pub pending: usize,
    pub active: usize,
    pub done: usize,
    pub failed: usize,
    pub skipped: usize,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn free_task(prompt: &str) -> AgentTask {
        AgentTask::FreeForm {
            prompt: prompt.into(),
        }
    }

    // -- TaskLedger basics -------------------------------------------------

    #[test]
    fn new_ledger_is_empty() {
        let ledger = TaskLedger::new("fix the build");
        assert_eq!(ledger.goal, "fix the build");
        assert!(ledger.facts.is_empty());
        assert!(ledger.plan.is_empty());
        assert_eq!(ledger.stall_counter, 0);
        assert_eq!(ledger.replan_count, 0);
    }

    #[test]
    fn add_and_query_facts() {
        let mut ledger = TaskLedger::new("test");
        ledger.add_fact("error is in main.rs", FactConfidence::Verified, "agent-1");
        ledger.add_fact("might be a type mismatch", FactConfidence::Guess, "brain");
        ledger.add_fact("need to check Cargo.toml", FactConfidence::ToLookUp, "brain");

        assert_eq!(ledger.facts_at(FactConfidence::Verified).len(), 1);
        assert_eq!(ledger.facts_at(FactConfidence::Guess).len(), 1);
        assert_eq!(ledger.facts_at(FactConfidence::ToLookUp).len(), 1);
        assert_eq!(ledger.facts_at(FactConfidence::ToDerive).len(), 0);
    }

    #[test]
    fn verify_fact_promotes_guess() {
        let mut ledger = TaskLedger::new("test");
        ledger.add_fact("error is E0308", FactConfidence::Guess, "brain");

        ledger.verify_fact("error is");

        assert_eq!(ledger.facts_at(FactConfidence::Guess).len(), 0);
        assert_eq!(ledger.facts_at(FactConfidence::Verified).len(), 1);
    }

    // -- PlanStep lifecycle ------------------------------------------------

    #[test]
    fn plan_step_new_is_pending() {
        let step = PlanStep::new("read the file", free_task("read"));
        assert_eq!(step.status, StepStatus::Pending);
        assert_eq!(step.attempts, 0);
        assert!(step.result_summary.is_none());
    }

    #[test]
    fn plan_step_record_failure_allows_retry() {
        let mut step = PlanStep::new("try something", free_task("try"));
        step.max_attempts = 3;

        assert!(step.record_failure("first failure"));
        assert_eq!(step.status, StepStatus::Pending); // re-queued
        assert_eq!(step.attempts, 1);

        assert!(step.record_failure("second failure"));
        assert_eq!(step.attempts, 2);

        assert!(!step.record_failure("third failure")); // exhausted
        assert_eq!(step.status, StepStatus::Failed);
        assert_eq!(step.attempts, 3);
    }

    #[test]
    fn plan_step_record_success() {
        let mut step = PlanStep::new("do the thing", free_task("do"));
        step.record_success("done");
        assert_eq!(step.status, StepStatus::Done);
        assert_eq!(step.result_summary.as_deref(), Some("done"));
    }

    // -- Plan management ---------------------------------------------------

    #[test]
    fn set_plan_resets_stall() {
        let mut ledger = TaskLedger::new("test");
        ledger.stall_counter = 5;
        ledger.set_plan(vec![PlanStep::new("step 1", free_task("s1"))]);
        assert_eq!(ledger.stall_counter, 0);
        assert_eq!(ledger.plan.len(), 1);
    }

    #[test]
    fn replan_archives_old_plan() {
        let mut ledger = TaskLedger::new("test");
        ledger.set_plan(vec![PlanStep::new("old step", free_task("old"))]);

        ledger.replan(vec![PlanStep::new("new step", free_task("new"))]);

        assert_eq!(ledger.plan.len(), 1);
        assert_eq!(ledger.plan[0].description, "new step");
        assert_eq!(ledger.plan_history.len(), 1);
        assert_eq!(ledger.plan_history[0][0].description, "old step");
        assert_eq!(ledger.replan_count, 1);
        assert_eq!(ledger.stall_counter, 0);
    }

    #[test]
    fn next_pending_step_skips_done() {
        let mut ledger = TaskLedger::new("test");
        let mut s1 = PlanStep::new("done step", free_task("s1"));
        s1.status = StepStatus::Done;
        let s2 = PlanStep::new("pending step", free_task("s2"));
        ledger.set_plan(vec![s1, s2]);

        let (idx, step) = ledger.next_pending_step().unwrap();
        assert_eq!(idx, 1);
        assert_eq!(step.description, "pending step");
    }

    #[test]
    fn is_plan_resolved_all_done() {
        let mut ledger = TaskLedger::new("test");
        let mut s1 = PlanStep::new("s1", free_task("s1"));
        s1.status = StepStatus::Done;
        let mut s2 = PlanStep::new("s2", free_task("s2"));
        s2.status = StepStatus::Failed;
        ledger.set_plan(vec![s1, s2]);

        assert!(ledger.is_plan_resolved());
    }

    #[test]
    fn is_plan_resolved_false_with_pending() {
        let mut ledger = TaskLedger::new("test");
        ledger.set_plan(vec![PlanStep::new("s1", free_task("s1"))]);
        assert!(!ledger.is_plan_resolved());
    }

    // -- Progress assessment -----------------------------------------------

    #[test]
    fn assess_complete_when_all_done() {
        let mut ledger = TaskLedger::new("test");
        let mut s1 = PlanStep::new("s1", free_task("s1"));
        s1.status = StepStatus::Done;
        ledger.set_plan(vec![s1]);

        let assessment = ledger.assess_progress();
        assert!(assessment.is_complete);
        assert!(!assessment.is_looping);
    }

    #[test]
    fn assess_not_complete_with_pending() {
        let mut ledger = TaskLedger::new("test");
        ledger.set_plan(vec![PlanStep::new("s1", free_task("s1"))]);

        let assessment = ledger.assess_progress();
        assert!(!assessment.is_complete);
        assert_eq!(assessment.next_step_idx, Some(0));
    }

    // -- Stall / re-plan decision ------------------------------------------

    #[test]
    fn should_replan_continue_when_progressing() {
        let mut ledger = TaskLedger::new("test");
        // A ledger with pending steps and no stall should continue.
        ledger.set_plan(vec![
            PlanStep::new("step 1", free_task("s1")),
            PlanStep::new("step 2", free_task("s2")),
        ]);
        assert_eq!(ledger.should_replan(), ReplanDecision::Continue);
    }

    #[test]
    fn should_replan_triggers_on_stall() {
        let mut ledger = TaskLedger::new("test");
        ledger.set_plan(vec![PlanStep::new("s1", free_task("s1"))]);
        ledger.stall_counter = 3; // exceeds default threshold of 2

        let decision = ledger.should_replan();
        assert!(matches!(decision, ReplanDecision::Replan { .. }));
    }

    #[test]
    fn should_replan_complete_when_all_succeeded() {
        let mut ledger = TaskLedger::new("test");
        let mut s1 = PlanStep::new("s1", free_task("s1"));
        s1.status = StepStatus::Done;
        ledger.set_plan(vec![s1]);

        assert_eq!(ledger.should_replan(), ReplanDecision::Complete);
    }

    #[test]
    fn should_replan_gives_up_after_max() {
        let mut ledger = TaskLedger::new("test");
        ledger.replan_count = 5; // equals max_replans
        ledger.set_plan(vec![PlanStep::new("s1", free_task("s1"))]);

        let decision = ledger.should_replan();
        assert!(matches!(decision, ReplanDecision::GiveUp { .. }));
    }

    #[test]
    fn should_replan_on_all_failed_steps() {
        let mut ledger = TaskLedger::new("test");
        let mut s1 = PlanStep::new("s1", free_task("s1"));
        s1.status = StepStatus::Failed;
        ledger.set_plan(vec![s1]);

        let decision = ledger.should_replan();
        assert!(matches!(decision, ReplanDecision::Replan { .. }));
    }

    // -- Loop detection ----------------------------------------------------

    #[test]
    fn detect_loop_on_repeated_outputs() {
        let mut ledger = TaskLedger::new("test");
        for _ in 0..3 {
            ledger.record_output("same output every time");
        }
        assert!(ledger.detect_loop());
    }

    #[test]
    fn no_loop_on_varied_outputs() {
        let mut ledger = TaskLedger::new("test");
        ledger.record_output("output A");
        ledger.record_output("output B");
        ledger.record_output("output C");
        assert!(!ledger.detect_loop());
    }

    #[test]
    fn no_loop_on_insufficient_data() {
        let mut ledger = TaskLedger::new("test");
        ledger.record_output("just one");
        assert!(!ledger.detect_loop());
    }

    // -- Step counts -------------------------------------------------------

    #[test]
    fn step_counts_accurate() {
        let mut ledger = TaskLedger::new("test");
        let mut s1 = PlanStep::new("s1", free_task("s1"));
        s1.status = StepStatus::Done;
        let s2 = PlanStep::new("s2", free_task("s2")); // Pending
        let mut s3 = PlanStep::new("s3", free_task("s3"));
        s3.status = StepStatus::Active;
        ledger.set_plan(vec![s1, s2, s3]);

        let counts = ledger.step_counts();
        assert_eq!(counts.done, 1);
        assert_eq!(counts.pending, 1);
        assert_eq!(counts.active, 1);
        assert_eq!(counts.failed, 0);
        assert_eq!(counts.skipped, 0);
    }

    // -- Replan context ---------------------

    #[test]
    fn replan_context_contains_goal_and_facts() {
        let mut ledger = TaskLedger::new("fix build errors");
        ledger.add_fact("error in main.rs", FactConfidence::Verified, "agent-1");
        ledger.add_fact("maybe type issue", FactConfidence::Guess, "brain");

        let ctx = ledger.replan_context();
        assert!(ctx.contains("fix build errors"));
        assert!(ctx.contains("error in main.rs"));
        assert!(ctx.contains("maybe type issue"));
        assert!(ctx.contains("VERIFIED FACTS"));
        assert!(ctx.contains("EDUCATED GUESSES"));
    }

    // -- WorldState builder ------------------------------------------------

    #[test]
    fn world_state_default_has_no_triggers() {
        let world = WorldState::default();
        assert!(!world.any_replan_trigger());
        assert!(!world.errors_detected());
        assert!(!world.agent_flatlined());
        assert!(!world.user_input_arrived());
        assert!(!world.goal_changed());
        assert!(!world.time_budget_exhausted());
    }

    #[test]
    fn world_state_builder_errors_detected() {
        let world = WorldState::builder().errors_detected(true).build();
        assert!(world.errors_detected());
        assert!(world.any_replan_trigger());
    }

    #[test]
    fn world_state_builder_agent_flatlined() {
        let world = WorldState::builder().agent_flatlined(true).build();
        assert!(world.agent_flatlined());
        assert!(world.any_replan_trigger());
    }

    #[test]
    fn world_state_builder_user_input_arrived() {
        let world = WorldState::builder().user_input_arrived(true).build();
        assert!(world.user_input_arrived());
        assert!(world.any_replan_trigger());
    }

    #[test]
    fn world_state_builder_goal_changed() {
        let world = WorldState::builder().goal_changed(true).build();
        assert!(world.goal_changed());
        assert!(world.any_replan_trigger());
    }

    #[test]
    fn world_state_builder_time_budget_exhausted() {
        let world = WorldState::builder().time_budget_exhausted(true).build();
        assert!(world.time_budget_exhausted());
        assert!(world.any_replan_trigger());
    }

    // -- Orchestrator::should_replan ---------------------------------------

    /// No trigger active + steady ledger → should_replan returns false.
    #[test]
    fn should_replan_false_when_steady() {
        let mut orch = Orchestrator::new("fix the build");
        orch.set_plan(vec![
            PlanStep::new("step 1", free_task("s1")),
            PlanStep::new("step 2", free_task("s2")),
        ]);
        let world = WorldState::default(); // all triggers off
        assert!(!orch.should_replan(&world));
    }

    /// New errors trigger a replan even when the ledger says Continue.
    #[test]
    fn should_replan_true_on_errors_detected() {
        let mut orch = Orchestrator::new("fix the build");
        orch.set_plan(vec![PlanStep::new("s1", free_task("s1"))]);
        let world = WorldState::builder().errors_detected(true).build();
        assert!(orch.should_replan(&world));
    }

    /// Agent flatlined triggers a replan.
    #[test]
    fn should_replan_true_on_agent_flatlined() {
        let mut orch = Orchestrator::new("build feature");
        orch.set_plan(vec![PlanStep::new("s1", free_task("s1"))]);
        let world = WorldState::builder().agent_flatlined(true).build();
        assert!(orch.should_replan(&world));
    }

    /// User input arriving triggers a replan (user may have changed direction).
    #[test]
    fn should_replan_true_on_user_input() {
        let mut orch = Orchestrator::new("deploy staging");
        orch.set_plan(vec![PlanStep::new("s1", free_task("s1"))]);
        let world = WorldState::builder().user_input_arrived(true).build();
        assert!(orch.should_replan(&world));
    }

    /// Goal changing triggers a replan.
    #[test]
    fn should_replan_true_on_goal_changed() {
        let mut orch = Orchestrator::new("old goal");
        orch.set_plan(vec![PlanStep::new("s1", free_task("s1"))]);
        let world = WorldState::builder().goal_changed(true).build();
        assert!(orch.should_replan(&world));
    }

    /// Time budget exhausted triggers a replan.
    #[test]
    fn should_replan_true_on_time_budget_exhausted() {
        let mut orch = Orchestrator::new("long running task");
        orch.set_plan(vec![PlanStep::new("s1", free_task("s1"))]);
        let world = WorldState::builder().time_budget_exhausted(true).build();
        assert!(orch.should_replan(&world));
    }

    /// Ledger stall counter exceeded triggers a replan even with no world triggers.
    #[test]
    fn should_replan_true_on_ledger_stall() {
        let mut orch = Orchestrator::new("stuck goal");
        orch.set_plan(vec![PlanStep::new("s1", free_task("s1"))]);
        // Drive stall_counter past the threshold (default 2).
        orch.ledger_mut().stall_counter = 3;
        let world = WorldState::default();
        assert!(orch.should_replan(&world));
    }

    /// All steps failed → ledger says Replan → should_replan returns true.
    #[test]
    fn should_replan_true_on_all_steps_failed() {
        let mut orch = Orchestrator::new("failing goal");
        let mut s1 = PlanStep::new("s1", free_task("s1"));
        s1.status = StepStatus::Failed;
        orch.set_plan(vec![s1]);
        let world = WorldState::default();
        assert!(orch.should_replan(&world));
    }

    /// All steps succeeded → ledger says Complete → should_replan returns false
    /// (no point generating a new plan when goal is already done).
    #[test]
    fn should_replan_false_when_complete() {
        let mut orch = Orchestrator::new("done goal");
        let mut s1 = PlanStep::new("s1", free_task("s1"));
        s1.status = StepStatus::Done;
        orch.set_plan(vec![s1]);
        let world = WorldState::default();
        assert!(!orch.should_replan(&world));
    }

    /// Replan budget exhausted → ledger says GiveUp → should_replan returns true
    /// (caller needs to act on the give-up, not silently continue).
    #[test]
    fn should_replan_true_on_giveup() {
        let mut orch = Orchestrator::new("hopeless goal");
        orch.set_plan(vec![PlanStep::new("s1", free_task("s1"))]);
        orch.ledger_mut().replan_count = 5; // equals max_replans
        let world = WorldState::default();
        assert!(orch.should_replan(&world));
    }

    // -- Orchestrator::dispatch_next_step ----------------------------------

    /// Returns None when the plan is empty.
    #[test]
    fn dispatch_next_step_none_on_empty_plan() {
        let orch = Orchestrator::new("goal with no plan");
        assert!(orch.dispatch_next_step().is_none());
    }

    /// Returns the first Pending step.
    #[test]
    fn dispatch_next_step_returns_first_pending() {
        let mut orch = Orchestrator::new("test");
        orch.set_plan(vec![
            PlanStep::new("step A", free_task("a")),
            PlanStep::new("step B", free_task("b")),
        ]);
        let step = orch.dispatch_next_step().expect("should return step A");
        assert_eq!(step.description, "step A");
    }

    /// Skips Done steps and returns the next Pending one.
    #[test]
    fn dispatch_next_step_skips_done_steps() {
        let mut orch = Orchestrator::new("test");
        let mut s1 = PlanStep::new("done step", free_task("d"));
        s1.status = StepStatus::Done;
        let s2 = PlanStep::new("pending step", free_task("p"));
        orch.set_plan(vec![s1, s2]);

        let step = orch.dispatch_next_step().expect("should return pending step");
        assert_eq!(step.description, "pending step");
    }

    /// Returns None when all steps are Done.
    #[test]
    fn dispatch_next_step_none_when_all_done() {
        let mut orch = Orchestrator::new("test");
        let mut s1 = PlanStep::new("s1", free_task("s1"));
        s1.status = StepStatus::Done;
        orch.set_plan(vec![s1]);

        assert!(orch.dispatch_next_step().is_none());
    }

    /// Returns None when all steps are Active (another step already running).
    #[test]
    fn dispatch_next_step_none_when_all_active() {
        let mut orch = Orchestrator::new("test");
        let mut s1 = PlanStep::new("s1", free_task("s1"));
        s1.status = StepStatus::Active;
        orch.set_plan(vec![s1]);

        assert!(orch.dispatch_next_step().is_none());
    }

    /// Dispatch is idempotent: calling it twice returns the same pending step
    /// because it does not mutate ledger state.
    #[test]
    fn dispatch_next_step_is_idempotent() {
        let mut orch = Orchestrator::new("test");
        orch.set_plan(vec![PlanStep::new("s1", free_task("s1"))]);

        let first = orch.dispatch_next_step();
        let second = orch.dispatch_next_step();
        assert!(first.is_some());
        assert!(second.is_some());
        assert_eq!(
            first.unwrap().description,
            second.unwrap().description,
            "dispatch_next_step must not mutate ledger"
        );
    }

    /// After the caller manually marks a step Active, dispatch_next_step moves
    /// to the following Pending step — correct sequencing in the outer loop.
    #[test]
    fn dispatch_next_step_sequences_correctly_after_activation() {
        let mut orch = Orchestrator::new("test");
        orch.set_plan(vec![
            PlanStep::new("step 1", free_task("s1")),
            PlanStep::new("step 2", free_task("s2")),
        ]);

        // Caller takes step 1 and marks it Active.
        let step1 = orch.dispatch_next_step().expect("step 1");
        assert_eq!(step1.description, "step 1");
        orch.ledger_mut().plan[0].status = StepStatus::Active;

        // Now dispatch should return step 2.
        let step2 = orch.dispatch_next_step().expect("step 2");
        assert_eq!(step2.description, "step 2");
    }

    // -- Issue #60: Task DAG + cycle detection --------------------------------

    /// A plan with no dependencies — all steps are immediately eligible.
    #[test]
    fn eligible_next_all_pending_with_no_deps() {
        let mut ledger = TaskLedger::new("test");
        ledger.set_plan(vec![
            PlanStep::new("s0", free_task("s0")),
            PlanStep::new("s1", free_task("s1")),
            PlanStep::new("s2", free_task("s2")),
        ]);
        let eligible = ledger.eligible_next();
        assert_eq!(eligible.len(), 3, "all three steps should be eligible");
    }

    /// A step with unmet dependencies is not eligible.
    #[test]
    fn eligible_next_blocked_step_not_returned() {
        let mut ledger = TaskLedger::new("test");
        // s1 depends on s0 (index 0), which is still Pending.
        ledger.set_plan(vec![
            PlanStep::new("s0", free_task("s0")),
            PlanStep::with_deps("s1", free_task("s1"), vec![0]),
        ]);
        let eligible = ledger.eligible_next();
        // Only s0 is eligible; s1 is blocked.
        assert_eq!(eligible.len(), 1);
        assert_eq!(eligible[0].1.description, "s0");
    }

    /// Once a dependency completes, the dependent step becomes eligible.
    #[test]
    fn eligible_next_unblocks_after_dep_done() {
        let mut ledger = TaskLedger::new("test");
        ledger.set_plan(vec![
            PlanStep::new("s0", free_task("s0")),
            PlanStep::with_deps("s1", free_task("s1"), vec![0]),
        ]);

        // Mark s0 done.
        ledger.plan[0].status = StepStatus::Done;

        let eligible = ledger.eligible_next();
        // s0 is Done (not Pending), s1 is now eligible.
        assert_eq!(eligible.len(), 1);
        assert_eq!(eligible[0].1.description, "s1");
    }

    /// Multiple dependencies: a step is only eligible when ALL are done.
    #[test]
    fn eligible_next_requires_all_deps_done() {
        let mut ledger = TaskLedger::new("test");
        ledger.set_plan(vec![
            PlanStep::new("s0", free_task("s0")),
            PlanStep::new("s1", free_task("s1")),
            PlanStep::with_deps("s2", free_task("s2"), vec![0, 1]),
        ]);

        // Only s0 done — s2 still blocked.
        ledger.plan[0].status = StepStatus::Done;
        assert_eq!(ledger.eligible_next().len(), 1); // only s1

        // Both done — s2 eligible now.
        ledger.plan[1].status = StepStatus::Done;
        let eligible = ledger.eligible_next();
        assert_eq!(eligible.len(), 1);
        assert_eq!(eligible[0].1.description, "s2");
    }

    /// A diamond DAG: s0 → s1, s0 → s2, s1+s2 → s3.
    #[test]
    fn eligible_next_diamond_dag() {
        let mut ledger = TaskLedger::new("test");
        // 0: root (no deps)
        // 1: left  (dep: 0)
        // 2: right (dep: 0)
        // 3: join  (deps: 1, 2)
        ledger.set_plan(vec![
            PlanStep::new("root", free_task("root")),
            PlanStep::with_deps("left", free_task("left"), vec![0]),
            PlanStep::with_deps("right", free_task("right"), vec![0]),
            PlanStep::with_deps("join", free_task("join"), vec![1, 2]),
        ]);

        // Initially only root is eligible.
        let e0 = ledger.eligible_next();
        assert_eq!(e0.len(), 1);
        assert_eq!(e0[0].1.description, "root");

        // After root done, left and right become eligible.
        ledger.plan[0].status = StepStatus::Done;
        let e1 = ledger.eligible_next();
        assert_eq!(e1.len(), 2);

        // After left + right done, join is eligible.
        ledger.plan[1].status = StepStatus::Done;
        ledger.plan[2].status = StepStatus::Done;
        let e2 = ledger.eligible_next();
        assert_eq!(e2.len(), 1);
        assert_eq!(e2[0].1.description, "join");
    }

    /// Active steps are not returned by eligible_next (only Pending).
    #[test]
    fn eligible_next_skips_active_steps() {
        let mut ledger = TaskLedger::new("test");
        ledger.set_plan(vec![
            PlanStep::new("s0", free_task("s0")),
            PlanStep::new("s1", free_task("s1")),
        ]);
        ledger.plan[0].status = StepStatus::Active;

        let eligible = ledger.eligible_next();
        assert_eq!(eligible.len(), 1);
        assert_eq!(eligible[0].1.description, "s1");
    }

    /// Out-of-bounds dep indices are treated as satisfied so the step can run.
    ///
    /// A step with `depends_on: [99]` in a 3-step plan must appear in
    /// `eligible_next()` — the OOB index cannot block it forever.
    #[test]
    fn eligible_next_skips_oob_dep_indices() {
        let mut ledger = TaskLedger::new("test");
        // Index 99 does not exist in a 3-element plan; treat as satisfied.
        ledger.set_plan(vec![
            PlanStep::new("s0", free_task("s0")),
            PlanStep::new("s1", free_task("s1")),
            PlanStep::with_deps("s2-oob", free_task("s2"), vec![99]),
        ]);

        let eligible = ledger.eligible_next();
        // All three steps should be eligible: s0 and s1 have no deps,
        // s2 has only an OOB dep (treated as satisfied).
        assert_eq!(eligible.len(), 3, "OOB dep index must not block the step");
        let descriptions: Vec<&str> =
            eligible.iter().map(|(_, s)| s.description.as_str()).collect();
        assert!(
            descriptions.contains(&"s2-oob"),
            "s2-oob must appear in eligible_next output"
        );
    }

    /// A simple acyclic graph has no cycle.
    #[test]
    fn has_cycle_false_on_dag() {
        let mut ledger = TaskLedger::new("test");
        ledger.plan = vec![
            PlanStep::new("s0", free_task("s0")),
            PlanStep::with_deps("s1", free_task("s1"), vec![0]),
            PlanStep::with_deps("s2", free_task("s2"), vec![1]),
        ];
        assert!(!ledger.has_cycle());
    }

    /// A self-loop is a cycle.
    #[test]
    fn has_cycle_detects_self_loop() {
        let mut ledger = TaskLedger::new("test");
        // s0 depends on itself.
        ledger.plan = vec![PlanStep::with_deps("s0", free_task("s0"), vec![0])];
        assert!(ledger.has_cycle());
    }

    /// A two-step mutual dependency is a cycle.
    #[test]
    fn has_cycle_detects_two_step_cycle() {
        let mut ledger = TaskLedger::new("test");
        // s0 ↔ s1 (both depend on each other).
        ledger.plan = vec![
            PlanStep::with_deps("s0", free_task("s0"), vec![1]),
            PlanStep::with_deps("s1", free_task("s1"), vec![0]),
        ];
        assert!(ledger.has_cycle());
    }

    /// A three-step cycle: s0→s1→s2→s0.
    #[test]
    fn has_cycle_detects_three_step_cycle() {
        let mut ledger = TaskLedger::new("test");
        ledger.plan = vec![
            PlanStep::with_deps("s0", free_task("s0"), vec![2]),
            PlanStep::with_deps("s1", free_task("s1"), vec![0]),
            PlanStep::with_deps("s2", free_task("s2"), vec![1]),
        ];
        assert!(ledger.has_cycle());
    }

    /// Out-of-bounds dep indices are silently ignored and do not cause a panic
    /// or false positive cycle detection.
    #[test]
    fn has_cycle_ignores_oob_dep_indices() {
        let mut ledger = TaskLedger::new("test");
        // Index 99 does not exist in a 1-element plan.
        ledger.plan = vec![PlanStep::with_deps("s0", free_task("s0"), vec![99])];
        assert!(!ledger.has_cycle());
    }

    /// set_plan with a cyclic plan marks all steps Failed immediately.
    #[test]
    fn set_plan_blocks_all_steps_on_cycle() {
        let mut ledger = TaskLedger::new("test");
        let s0 = PlanStep::with_deps("s0", free_task("s0"), vec![1]);
        let s1 = PlanStep::with_deps("s1", free_task("s1"), vec![0]);
        ledger.set_plan(vec![s0, s1]);

        for step in &ledger.plan {
            assert_eq!(
                step.status,
                StepStatus::Failed,
                "every step must be Failed when cycle is detected at set_plan"
            );
            assert!(
                step.result_summary
                    .as_deref()
                    .is_some_and(|s| s.contains("cycle")),
                "result_summary must mention 'cycle'"
            );
        }
    }

    /// A valid plan is not blocked by set_plan.
    #[test]
    fn set_plan_does_not_block_acyclic_plan() {
        let mut ledger = TaskLedger::new("test");
        let s0 = PlanStep::new("s0", free_task("s0"));
        let s1 = PlanStep::with_deps("s1", free_task("s1"), vec![0]);
        ledger.set_plan(vec![s0, s1]);

        assert_eq!(ledger.plan[0].status, StepStatus::Pending);
        assert_eq!(ledger.plan[1].status, StepStatus::Pending);
    }

    // -- Issue #60: PlanStep constructors -------------------------------------

    /// with_deps sets depends_on and leaves other fields at defaults.
    #[test]
    fn plan_step_with_deps_sets_depends_on() {
        let step = PlanStep::with_deps("my step", free_task("x"), vec![2, 5]);
        assert_eq!(step.depends_on(), &[2, 5]);
        assert_eq!(step.status, StepStatus::Pending);
        assert!(step.preferred_provider().is_none());
    }

    /// with_provider sets preferred_provider via builder style.
    #[test]
    fn plan_step_with_provider_sets_field() {
        let step = PlanStep::new("my step", free_task("x")).with_provider("claude-fast");
        assert_eq!(step.preferred_provider(), Some("claude-fast"));
        assert!(step.depends_on().is_empty());
    }

    /// with_deps + with_provider can be combined.
    #[test]
    fn plan_step_with_deps_and_provider() {
        let step =
            PlanStep::with_deps("combined", free_task("x"), vec![0]).with_provider("ollama-phi3.5");
        assert_eq!(step.depends_on(), &[0]);
        assert_eq!(step.preferred_provider(), Some("ollama-phi3.5"));
    }

    /// new() leaves depends_on empty and preferred_provider as None.
    #[test]
    fn plan_step_new_defaults() {
        let step = PlanStep::new("plain step", free_task("x"));
        assert!(step.depends_on().is_empty());
        assert!(step.preferred_provider().is_none());
    }
}
