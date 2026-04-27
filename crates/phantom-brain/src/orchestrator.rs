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

use std::collections::VecDeque;
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
#[derive(Debug, Clone)]
pub struct PlanStep {
    /// Human-readable description of what this step accomplishes.
    pub description: String,
    /// Which agent type should handle this step.
    pub assigned_task: AgentTask,
    /// Current execution status.
    pub status: StepStatus,
    /// The agent ID if one has been spawned for this step.
    pub agent_id: Option<u32>,
    /// Number of times this step has been attempted (for retry tracking).
    pub attempts: u32,
    /// Maximum attempts before marking as Failed.
    pub max_attempts: u32,
    /// Output/result summary from the agent (if completed).
    pub result_summary: Option<String>,
}

impl PlanStep {
    /// Create a new pending plan step.
    pub fn new(description: impl Into<String>, task: AgentTask) -> Self {
        Self {
            description: description.into(),
            assigned_task: task,
            status: StepStatus::Pending,
            agent_id: None,
            attempts: 0,
            max_attempts: 3,
            result_summary: None,
        }
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
    pub fn set_plan(&mut self, steps: Vec<PlanStep>) {
        self.plan = steps;
        self.stall_counter = 0;
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
}
