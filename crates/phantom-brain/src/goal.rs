//! Goal decomposition — high-level goal → [`TaskLedger`] of executable steps.
//!
//! When the user (or the brain) sets a high-level goal such as "fix all
//! failing tests" or "refactor the auth module", the brain cannot simply hand
//! that string to an agent. It first needs to break it into a concrete,
//! ordered plan: a DAG of [`Step`]s that the [`ReconcilerState`] can execute
//! one at a time.
//!
//! # How it works
//!
//! [`decompose`] calls a [`ChatBackend`] with a structured prompt asking the
//! LLM to return a numbered plan. The response is parsed into a [`Vec<Step>`],
//! dependency indices are resolved, and the result is loaded into a fresh
//! [`TaskLedger`] via [`TaskLedger::set_plan`].
//!
//! If the LLM call fails (network error, API key not set, etc.), `decompose`
//! returns an `Err` — the caller is responsible for deciding whether to retry
//! or fall back to a single-step ledger.
//!
//! # DAG semantics
//!
//! Each [`Step`] carries a `dependencies: Vec<usize>` field listing the
//! 0-based indices of steps that must complete before this one starts. The
//! [`ReconcilerState`] currently executes steps sequentially, so it will
//! advance past a step only after all its dependencies are `Done`.
//!
//! Steps with no dependencies (empty `Vec`) can run immediately. Circular
//! dependencies are not validated by this module — the caller should detect
//! them if needed.
//!
//! # Mocking in tests
//!
//! [`ChatBackend`] is a trait so tests can inject a mock that returns a fixed
//! plan string without making real HTTP calls. See the test section below.
//!
//! # Issue
//!
//! Implements GitHub issue #47.

use phantom_context::ProjectContext;

use crate::orchestrator::{PlanStep, TaskLedger};

// ---------------------------------------------------------------------------
// Goal
// ---------------------------------------------------------------------------

/// A high-level goal given to the brain.
///
/// The `description` is what the user typed (e.g., "fix all failing tests").
/// `success_criteria` is an optional natural-language definition of done
/// (e.g., "all tests pass, no compiler warnings").
#[derive(Debug, Clone)]
pub struct Goal {
    /// Human-readable description of what to accomplish.
    description: String,
    /// Natural-language definition of "done" for this goal.
    /// If empty, the brain uses a generic criterion.
    success_criteria: String,
}

impl Goal {
    /// Create a new [`Goal`].
    pub fn new(description: impl Into<String>, success_criteria: impl Into<String>) -> Self {
        Self {
            description: description.into(),
            success_criteria: success_criteria.into(),
        }
    }

    /// The goal description.
    pub fn description(&self) -> &str {
        &self.description
    }

    /// The success criteria.
    pub fn success_criteria(&self) -> &str {
        &self.success_criteria
    }
}

// ---------------------------------------------------------------------------
// Step
// ---------------------------------------------------------------------------

/// A single executable step in a goal's plan.
///
/// Steps form a DAG via `dependencies`. The reconciler executes each step by
/// mapping it to an `AgentTask` using the `tool_hint` field.
#[derive(Debug, Clone)]
pub struct Step {
    /// Human-readable description of what this step should do.
    description: String,
    /// Maximum number of agent attempts before this step is marked failed.
    max_attempts: u8,
    /// Optional hint naming which tool or agent type should handle this step.
    ///
    /// Examples: `"ReadFile"`, `"RunCommand"`, `"WriteFile"`.
    /// `None` means the reconciler should use a generic `FreeForm` agent.
    tool_hint: Option<String>,
    /// 0-based indices of steps that must complete before this one starts.
    ///
    /// An empty vec means this step has no dependencies and can run immediately.
    dependencies: Vec<usize>,
}

impl Step {
    /// Create a new [`Step`].
    pub fn new(
        description: String,
        max_attempts: u8,
        tool_hint: Option<String>,
        dependencies: Vec<usize>,
    ) -> Self {
        Self {
            description,
            max_attempts,
            tool_hint,
            dependencies,
        }
    }

    /// The human-readable description of what this step should do.
    pub fn description(&self) -> &str {
        &self.description
    }

    /// Maximum number of agent attempts before this step is marked failed.
    pub fn max_attempts(&self) -> u8 {
        self.max_attempts
    }

    /// Optional hint naming which tool or agent type should handle this step.
    pub fn tool_hint(&self) -> Option<&str> {
        self.tool_hint.as_deref()
    }

    /// 0-based indices of steps that must complete before this one starts.
    pub fn dependencies(&self) -> &[usize] {
        &self.dependencies
    }

    /// Convert this [`Step`] into a [`PlanStep`] suitable for a [`TaskLedger`].
    fn into_plan_step(self) -> PlanStep {
        use phantom_agents::AgentTask;

        let prompt = match &self.tool_hint {
            Some(hint) => format!("[{}] {}", hint, self.description),
            None => self.description.clone(),
        };

        let mut plan_step = PlanStep::new(self.description, AgentTask::FreeForm { prompt });
        plan_step.max_attempts = self.max_attempts as u32;
        plan_step
    }
}

// ---------------------------------------------------------------------------
// ChatBackend — injectable LLM interface
// ---------------------------------------------------------------------------

/// A thin, synchronous chat interface used by [`decompose`].
///
/// The trait is deliberately minimal: pass a prompt, get a string back.
/// Any LLM backend (Claude, Ollama, mock) can implement this.
///
/// # Thread safety
///
/// Implementations must be [`Send`] + [`Sync`] so they can be held by the
/// brain thread and potentially shared via `Arc`.
pub trait ChatBackend: Send + Sync {
    /// Send `prompt` to the model and return the response text.
    ///
    /// Returns `Err` if the call fails (network, auth, rate-limit, etc.).
    fn chat(&self, prompt: &str) -> Result<String, String>;
}

/// A [`ChatBackend`] that calls the Claude Messages API synchronously using
/// the `ANTHROPIC_API_KEY` environment variable.
///
/// This is the production backend. For tests, use a mock struct that
/// implements [`ChatBackend`].
pub struct ClaudeChatBackend {
    model: String,
    max_tokens: u32,
}

impl ClaudeChatBackend {
    /// Create a backend that calls the given Claude model.
    pub fn new(model: impl Into<String>, max_tokens: u32) -> Self {
        Self {
            model: model.into(),
            max_tokens,
        }
    }

    /// Create a backend using `claude-sonnet-4-20250514` with 1024 max tokens.
    pub fn default_model() -> Self {
        Self::new("claude-sonnet-4-20250514", 1024)
    }
}

impl ChatBackend for ClaudeChatBackend {
    fn chat(&self, prompt: &str) -> Result<String, String> {
        crate::claude::generate(&self.model, prompt, self.max_tokens).map(|(text, _latency)| text)
    }
}

// ---------------------------------------------------------------------------
// decompose — the main entry point
// ---------------------------------------------------------------------------

/// Decompose a high-level [`Goal`] into a [`TaskLedger`] of executable steps.
///
/// Calls `backend` with a structured prompt asking the LLM to plan the goal
/// as a numbered list of steps. The response is parsed into [`Step`]s and
/// loaded into a new [`TaskLedger`].
///
/// Returns `Err` if the LLM call fails or the response cannot be parsed into
/// at least one step.
///
/// # Example
///
/// ```rust,ignore
/// let goal = Goal::new("fix all failing tests", "cargo test returns 0");
/// let backend = ClaudeChatBackend::default_model();
/// let ledger = decompose(&goal, &context, &backend)?;
/// // ledger.plan now contains one PlanStep per LLM-generated step.
/// ```
pub fn decompose(
    goal: &Goal,
    ctx: &ProjectContext,
    backend: &dyn ChatBackend,
) -> Result<TaskLedger, String> {
    let prompt = build_decomposition_prompt(goal, ctx);

    let response = backend.chat(&prompt)?;

    let steps = parse_steps(&response);

    if steps.is_empty() {
        return Err(format!(
            "goal decomposition produced no steps for: {}",
            goal.description
        ));
    }

    let mut ledger = TaskLedger::new(goal.description.clone());
    let plan: Vec<PlanStep> = steps.into_iter().map(Step::into_plan_step).collect();
    ledger.set_plan(plan);

    Ok(ledger)
}

// ---------------------------------------------------------------------------
// build_decomposition_prompt
// ---------------------------------------------------------------------------

/// Build the LLM prompt that asks for a step-by-step plan.
fn build_decomposition_prompt(goal: &Goal, ctx: &ProjectContext) -> String {
    let criteria = if goal.success_criteria.is_empty() {
        "The goal is achieved when all steps complete successfully.".to_string()
    } else {
        format!("Success criteria: {}", goal.success_criteria)
    };

    format!(
        "You are Phantom, an AI brain embedded in a terminal emulator.\n\
         The developer is working in a {project_type:?} project called `{project_name}`.\n\n\
         Goal: {description}\n\
         {criteria}\n\n\
         Break this goal into a concrete, ordered list of terminal/coding steps.\n\
         Rules:\n\
         - Return ONLY a numbered list. One step per line.\n\
         - Each step must be a single, atomic action (read a file, run a command, write a change).\n\
         - If a step requires a prior step's output, note the dependency as: \"[depends: N]\" \
           where N is the step number (1-based).\n\
         - Optionally annotate a tool hint as: \"[tool: ToolName]\" where ToolName is one of: \
           ReadFile, WriteFile, RunCommand, ListDirectory.\n\
         - Do not include explanations, headers, or prose. Only the numbered list.\n\
         - Aim for 3–8 steps. Never exceed 10.\n\n\
         Example format:\n\
         1. [tool: RunCommand] Run `cargo test` to identify failing tests\n\
         2. [tool: ReadFile] Read the failing test file src/lib.rs [depends: 1]\n\
         3. [tool: WriteFile] Fix the identified issue in src/lib.rs [depends: 2]\n\
         4. [tool: RunCommand] Run `cargo test` again to verify the fix [depends: 3]\n\n\
         Now produce the plan:",
        project_type = ctx.project_type,
        project_name = ctx.name,
        description = goal.description,
        criteria = criteria,
    )
}

// ---------------------------------------------------------------------------
// parse_steps — extract Step list from LLM response
// ---------------------------------------------------------------------------

/// Parse a numbered-list LLM response into a [`Vec<Step>`].
///
/// Handles lines of the form:
/// ```text
/// 1. [tool: RunCommand] Run `cargo test` [depends: 2, 3]
/// ```
///
/// Lines that don't start with a digit followed by `.` are skipped.
/// Dependency and tool annotations are optional.
pub fn parse_steps(response: &str) -> Vec<Step> {
    let mut steps = Vec::new();

    for line in response.lines() {
        let trimmed = line.trim();

        // Must start with a digit and a period.
        let Some(dot_pos) = trimmed.find('.') else {
            continue;
        };
        let prefix = &trimmed[..dot_pos];
        if !prefix.chars().all(|c| c.is_ascii_digit()) || prefix.is_empty() {
            continue;
        }

        let rest = trimmed[dot_pos + 1..].trim();
        if rest.is_empty() {
            continue;
        }

        // Parse optional annotations: [tool: X] and [depends: N, M, ...]
        let (description, tool_hint, dependencies) = extract_annotations(rest);

        if description.is_empty() {
            continue;
        }

        steps.push(Step::new(description, 3, tool_hint, dependencies));
    }

    steps
}

/// Extract `[tool: X]` and `[depends: N, ...]` annotations from a step line.
///
/// Returns `(clean_description, tool_hint, dependency_indices)`.
/// Dependency numbers are converted from 1-based to 0-based.
fn extract_annotations(line: &str) -> (String, Option<String>, Vec<usize>) {
    let mut tool_hint: Option<String> = None;
    let mut dependencies: Vec<usize> = Vec::new();
    let mut description = line.to_string();

    // Extract [tool: X] annotation.
    if let Some(start) = description.find("[tool:") {
        if let Some(end) = description[start..].find(']') {
            let annotation = &description[start..start + end + 1];
            let inner = annotation
                .trim_start_matches("[tool:")
                .trim_end_matches(']')
                .trim();
            if !inner.is_empty() {
                tool_hint = Some(inner.to_string());
            }
            description = format!(
                "{}{}",
                &description[..start].trim_end(),
                &description[start + end + 1..].trim_start()
            );
        }
    }

    // Extract [depends: N, M, ...] annotation.
    if let Some(start) = description.find("[depends:") {
        if let Some(end) = description[start..].find(']') {
            let annotation = &description[start..start + end + 1];
            let inner = annotation
                .trim_start_matches("[depends:")
                .trim_end_matches(']')
                .trim();
            for part in inner.split(',') {
                let part = part.trim();
                if let Ok(n) = part.parse::<usize>() {
                    if n >= 1 {
                        // Convert from 1-based to 0-based.
                        dependencies.push(n - 1);
                    }
                }
            }
            description = format!(
                "{}{}",
                &description[..start].trim_end(),
                &description[start + end + 1..].trim_start()
            );
        }
    }

    (description.trim().to_string(), tool_hint, dependencies)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use phantom_context::{
        Framework, GitInfo, PackageManager, ProjectCommands, ProjectContext, ProjectType,
    };

    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

    fn test_context() -> ProjectContext {
        ProjectContext {
            root: "/tmp/test-project".into(),
            name: "test-project".into(),
            project_type: ProjectType::Rust,
            package_manager: PackageManager::Cargo,
            framework: Framework::None,
            commands: ProjectCommands {
                build: Some("cargo build".into()),
                test: Some("cargo test".into()),
                run: Some("cargo run".into()),
                lint: None,
                format: None,
            },
            git: Some(GitInfo {
                branch: "main".into(),
                remote_url: None,
                is_dirty: false,
                ahead: 0,
                behind: 0,
                last_commit_message: None,
                last_commit_age: None,
            }),
            rust_version: None,
            node_version: None,
            python_version: None,
        }
    }

    /// A mock backend that returns a fixed response string.
    struct MockChatBackend {
        response: String,
    }

    impl MockChatBackend {
        fn new(response: impl Into<String>) -> Self {
            Self {
                response: response.into(),
            }
        }
    }

    impl ChatBackend for MockChatBackend {
        fn chat(&self, _prompt: &str) -> Result<String, String> {
            Ok(self.response.clone())
        }
    }

    /// A mock backend that always returns an error.
    struct FailingChatBackend;

    impl ChatBackend for FailingChatBackend {
        fn chat(&self, _prompt: &str) -> Result<String, String> {
            Err("simulated API error".into())
        }
    }

    // -----------------------------------------------------------------------
    // Goal
    // -----------------------------------------------------------------------

    #[test]
    fn goal_accessors() {
        let goal = Goal::new("fix tests", "all tests pass");
        assert_eq!(goal.description(), "fix tests");
        assert_eq!(goal.success_criteria(), "all tests pass");
    }

    #[test]
    fn goal_empty_criteria_allowed() {
        let goal = Goal::new("do something", "");
        assert!(goal.success_criteria().is_empty());
    }

    // -----------------------------------------------------------------------
    // parse_steps
    // -----------------------------------------------------------------------

    #[test]
    fn parse_steps_basic_numbered_list() {
        let response = "\
            1. Run `cargo test` to identify failures\n\
            2. Read the failing test file\n\
            3. Apply the fix\n";
        let steps = parse_steps(response);
        assert_eq!(steps.len(), 3);
        assert_eq!(
            steps[0].description(),
            "Run `cargo test` to identify failures"
        );
        assert_eq!(steps[1].description(), "Read the failing test file");
        assert_eq!(steps[2].description(), "Apply the fix");
    }

    #[test]
    fn parse_steps_extracts_tool_hint() {
        let response = "1. [tool: RunCommand] Run `cargo test`\n";
        let steps = parse_steps(response);
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].tool_hint(), Some("RunCommand"));
        assert_eq!(steps[0].description(), "Run `cargo test`");
    }

    #[test]
    fn parse_steps_extracts_dependency() {
        let response = "1. First step\n2. Second step [depends: 1]\n";
        let steps = parse_steps(response);
        assert_eq!(steps.len(), 2);
        assert!(steps[0].dependencies().is_empty());
        assert_eq!(steps[1].dependencies(), &[0usize]); // 1-based 1 → 0-based 0
    }

    #[test]
    fn parse_steps_multiple_dependencies() {
        let response = "1. Step A\n2. Step B\n3. Step C [depends: 1, 2]\n";
        let steps = parse_steps(response);
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[2].dependencies(), &[0usize, 1usize]);
    }

    #[test]
    fn parse_steps_tool_and_depends_together() {
        let response = "1. [tool: ReadFile] Read the file [depends: 2]\n";
        let steps = parse_steps(response);
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].tool_hint(), Some("ReadFile"));
        assert_eq!(steps[0].dependencies(), &[1usize]); // 1-based 2 → 0-based 1
    }

    #[test]
    fn parse_steps_skips_non_numbered_lines() {
        let response = "Here is the plan:\n1. Step one\nSome prose\n2. Step two\n";
        let steps = parse_steps(response);
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].description(), "Step one");
        assert_eq!(steps[1].description(), "Step two");
    }

    #[test]
    fn parse_steps_empty_response_gives_empty_vec() {
        let steps = parse_steps("");
        assert!(steps.is_empty());
    }

    #[test]
    fn parse_steps_ignores_all_prose() {
        let response = "I will help you. Here's the plan: do nothing.";
        let steps = parse_steps(response);
        assert!(steps.is_empty());
    }

    #[test]
    fn parse_steps_default_max_attempts_is_three() {
        let response = "1. Do something\n";
        let steps = parse_steps(response);
        assert_eq!(steps[0].max_attempts(), 3);
    }

    // -----------------------------------------------------------------------
    // Issue #47 acceptance: decompose with mocked LLM
    // -----------------------------------------------------------------------

    /// Issue #47 acceptance test: `decompose` with a mocked backend that
    /// returns a known plan produces a `TaskLedger` matching that plan.
    #[test]
    fn decompose_produces_task_ledger_matching_plan() {
        let plan_response = "\
            1. [tool: RunCommand] Run `cargo test` to find failing tests\n\
            2. [tool: ReadFile] Read the failing test file [depends: 1]\n\
            3. [tool: WriteFile] Apply the fix [depends: 2]\n\
            4. [tool: RunCommand] Run `cargo test` again to verify [depends: 3]\n";

        let backend = MockChatBackend::new(plan_response);
        let goal = Goal::new("fix all failing tests", "cargo test returns 0");
        let ctx = test_context();

        let ledger = decompose(&goal, &ctx, &backend).expect("decompose must succeed");

        assert_eq!(ledger.goal, "fix all failing tests");
        assert_eq!(
            ledger.plan.len(),
            4,
            "ledger must have one step per plan line"
        );

        assert_eq!(
            ledger.plan[0].description,
            "Run `cargo test` to find failing tests"
        );
        assert_eq!(ledger.plan[1].description, "Read the failing test file");
        assert_eq!(ledger.plan[2].description, "Apply the fix");
        assert_eq!(
            ledger.plan[3].description,
            "Run `cargo test` again to verify"
        );
    }

    #[test]
    fn decompose_all_steps_are_pending_initially() {
        use crate::orchestrator::StepStatus;

        let plan_response = "1. Step A\n2. Step B\n";
        let backend = MockChatBackend::new(plan_response);
        let goal = Goal::new("do something", "");
        let ctx = test_context();

        let ledger = decompose(&goal, &ctx, &backend).unwrap();

        for step in &ledger.plan {
            assert_eq!(
                step.status,
                StepStatus::Pending,
                "all steps must be Pending after decompose"
            );
        }
    }

    #[test]
    fn decompose_error_on_backend_failure() {
        let backend = FailingChatBackend;
        let goal = Goal::new("fix build", "");
        let ctx = test_context();

        let result = decompose(&goal, &ctx, &backend);
        assert!(
            result.is_err(),
            "decompose must return Err on backend failure"
        );
    }

    #[test]
    fn decompose_error_when_no_steps_parsed() {
        let backend = MockChatBackend::new("I cannot help with that.");
        let goal = Goal::new("fix build", "");
        let ctx = test_context();

        let result = decompose(&goal, &ctx, &backend);
        assert!(
            result.is_err(),
            "decompose must return Err when LLM returns no steps"
        );
    }

    #[test]
    fn decompose_reconciler_can_iterate_result() {
        use crate::events::AiAction;
        use crate::orchestrator::StepStatus;
        use crate::reconciler::ReconcilerState;
        use std::sync::mpsc;

        let plan_response = "1. Run tests\n2. Fix the issue\n";
        let backend = MockChatBackend::new(plan_response);
        let goal = Goal::new("fix tests", "tests pass");
        let ctx = test_context();

        let mut ledger = decompose(&goal, &ctx, &backend).unwrap();
        let mut reconciler = ReconcilerState::new();
        let (tx, rx) = mpsc::channel();

        // Tick the reconciler — should dispatch the first pending step.
        reconciler.tick(&mut ledger, &tx);

        // A SpawnAgent action must have been emitted for step 0.
        let action = rx.try_recv().expect("reconciler must emit SpawnAgent");
        assert!(
            matches!(action, AiAction::SpawnAgent { .. }),
            "reconciler must emit SpawnAgent for first pending step"
        );
        assert_eq!(
            ledger.plan[0].status,
            StepStatus::Active,
            "step 0 must be Active after dispatch"
        );
    }

    // -----------------------------------------------------------------------
    // build_decomposition_prompt
    // -----------------------------------------------------------------------

    #[test]
    fn prompt_contains_goal_and_criteria() {
        let goal = Goal::new("deploy to staging", "staging URL is reachable");
        let ctx = test_context();
        let prompt = build_decomposition_prompt(&goal, &ctx);
        assert!(prompt.contains("deploy to staging"));
        assert!(prompt.contains("staging URL is reachable"));
    }

    #[test]
    fn prompt_contains_project_name() {
        let goal = Goal::new("fix build", "");
        let ctx = test_context();
        let prompt = build_decomposition_prompt(&goal, &ctx);
        assert!(prompt.contains("test-project"));
    }

    #[test]
    fn prompt_uses_generic_criteria_when_empty() {
        let goal = Goal::new("do something", "");
        let ctx = test_context();
        let prompt = build_decomposition_prompt(&goal, &ctx);
        assert!(prompt.contains("all steps complete successfully"));
    }
}
