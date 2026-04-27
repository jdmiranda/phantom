//! Voyager-inspired automatic curriculum generator.
//!
//! Proposes proactive tasks for the brain based on project state, completed
//! tasks, and failed tasks. Uses the LLM to generate contextually appropriate
//! next actions, with a warm-up gate that increases context richness as the
//! brain accumulates experience.
//!
//! # Voyager mapping
//!
//! | Voyager concept          | Phantom equivalent                        |
//! |--------------------------|-------------------------------------------|
//! | Minecraft game state     | `ProjectContext` + `MemoryStore`           |
//! | Inventory / biome        | Project type, git state, recent commands   |
//! | Completed tasks          | `completed_tasks` Vec<String>             |
//! | Failed tasks             | `failed_tasks` Vec<String>                |
//! | QA pairs                 | Memory search results                     |
//! | GPT-3.5 curriculum call  | Claude / Ollama generate call             |
//! | Warm-up thresholds       | `WarmUpGate` gating context sections      |
//! | Skill retrieval (top-5)  | `SkillStore::retrieve` for relevant skills|

use std::collections::HashSet;
use std::time::Instant;

use phantom_agents::AgentTask;
use phantom_context::ProjectContext;
use phantom_memory::MemoryStore;

use crate::skill_store::SkillStore;

// ---------------------------------------------------------------------------
// WarmUpGate — controls when context sections become available
// ---------------------------------------------------------------------------

/// Gating thresholds that control when observation sections are injected
/// into the curriculum prompt. Mirrors Voyager's warm-up dictionary:
/// context only appears after N completed tasks.
#[derive(Debug, Clone)]
pub struct WarmUpGate {
    /// Include git state after this many completions.
    pub git_context: u32,
    /// Include memory/conventions after this many completions.
    pub memory_context: u32,
    /// Include failed tasks after this many completions.
    pub failed_context: u32,
    /// Include skill library hints after this many completions.
    pub skill_context: u32,
}

impl Default for WarmUpGate {
    fn default() -> Self {
        Self {
            git_context: 0,     // always available
            memory_context: 3,  // after 3 tasks
            failed_context: 5,  // after 5 tasks
            skill_context: 8,   // after 8 tasks
        }
    }
}

// ---------------------------------------------------------------------------
// ProposedTask
// ---------------------------------------------------------------------------

/// A task proposed by the curriculum generator with reasoning.
#[derive(Debug, Clone)]
pub struct ProposedTask {
    /// The agent task to execute.
    pub task: AgentTask,
    /// Why this task was proposed (for logging / debug overlay).
    pub reasoning: String,
    /// Estimated complexity (0.0 = trivial, 1.0 = hard).
    pub estimated_difficulty: f32,
}

// ---------------------------------------------------------------------------
// CurriculumGenerator
// ---------------------------------------------------------------------------

/// Generates proactive tasks based on project state and learning history.
///
/// The curriculum tracks completed and failed tasks, deduplicates them,
/// and builds a rich prompt for the LLM to propose the next task. Like
/// Voyager, it starts with simple hardcoded tasks and gradually increases
/// sophistication as the brain accumulates experience.
pub struct CurriculumGenerator {
    /// Tasks the brain has successfully completed.
    completed_tasks: Vec<String>,
    /// Tasks the brain attempted but failed.
    failed_tasks: Vec<String>,
    /// Warm-up gates controlling prompt richness.
    warm_up: WarmUpGate,
    /// When the last task was proposed (rate limiting).
    last_proposal_time: Option<Instant>,
    /// Minimum seconds between proposals.
    proposal_cooldown_secs: f32,
}

impl CurriculumGenerator {
    pub fn new() -> Self {
        Self {
            completed_tasks: Vec::new(),
            failed_tasks: Vec::new(),
            warm_up: WarmUpGate::default(),
            last_proposal_time: None,
            proposal_cooldown_secs: 60.0,
        }
    }

    /// Record a task as completed. Removes it from failed_tasks if present.
    pub fn record_success(&mut self, task_description: &str) {
        let desc = task_description.to_owned();
        if !self.completed_tasks.contains(&desc) {
            self.completed_tasks.push(desc.clone());
        }
        self.failed_tasks.retain(|t| t != &desc);
    }

    /// Record a task as failed.
    pub fn record_failure(&mut self, task_description: &str) {
        let desc = task_description.to_owned();
        // Don't add to failed if already completed.
        if !self.completed_tasks.contains(&desc) && !self.failed_tasks.contains(&desc) {
            self.failed_tasks.push(desc);
        }
    }

    /// Number of completed tasks (drives warm-up gating).
    pub fn progress(&self) -> u32 {
        self.completed_tasks.len() as u32
    }

    /// Whether the cooldown has elapsed since the last proposal.
    pub fn ready_to_propose(&self) -> bool {
        self.last_proposal_time
            .map(|t| t.elapsed().as_secs_f32() >= self.proposal_cooldown_secs)
            .unwrap_or(true)
    }

    // -----------------------------------------------------------------------
    // Prompt construction (Voyager-style context injection)
    // -----------------------------------------------------------------------

    /// Build the curriculum prompt with all gated context sections.
    ///
    /// This is the core Voyager-inspired mechanism: we build a rich context
    /// string describing the current project state, then ask the LLM to
    /// propose a single proactive task.
    pub fn build_prompt(
        &self,
        context: &ProjectContext,
        memory: &MemoryStore,
        skill_store: &SkillStore,
    ) -> String {
        let progress = self.progress();
        let mut sections = Vec::new();

        // -- Always included: project identity --
        sections.push(format!(
            "Project: {} ({:?}, {:?})",
            context.name, context.project_type, context.package_manager
        ));

        // -- Git state (gated) --
        if progress >= self.warm_up.git_context {
            if let Some(ref git) = context.git {
                sections.push(format!(
                    "Git: branch={}, dirty={}, ahead={}, behind={}{}",
                    git.branch,
                    git.is_dirty,
                    git.ahead,
                    git.behind,
                    git.last_commit_message
                        .as_deref()
                        .map(|m| format!(", last_commit=\"{m}\""))
                        .unwrap_or_default(),
                ));
            }
        }

        // -- Project commands --
        let cmds = &context.commands;
        let mut cmd_parts = Vec::new();
        if let Some(ref b) = cmds.build { cmd_parts.push(format!("build: {b}")); }
        if let Some(ref t) = cmds.test { cmd_parts.push(format!("test: {t}")); }
        if let Some(ref l) = cmds.lint { cmd_parts.push(format!("lint: {l}")); }
        if !cmd_parts.is_empty() {
            sections.push(format!("Commands: {}", cmd_parts.join(", ")));
        }

        // -- Memory / conventions (gated) --
        if progress >= self.warm_up.memory_context {
            let mem_ctx = memory.agent_context();
            if mem_ctx != "No project memories stored." {
                sections.push(format!("Project memory:\n{mem_ctx}"));
            }
        }

        // -- Completed tasks --
        if !self.completed_tasks.is_empty() {
            let recent: Vec<&str> = self.completed_tasks
                .iter()
                .rev()
                .take(10)
                .map(|s| s.as_str())
                .collect();
            sections.push(format!(
                "Completed tasks (recent {}):\n{}",
                recent.len(),
                recent.iter().map(|t| format!("  - {t}")).collect::<Vec<_>>().join("\n")
            ));
        }

        // -- Failed tasks (gated) --
        if progress >= self.warm_up.failed_context && !self.failed_tasks.is_empty() {
            let recent_fails: Vec<&str> = self.failed_tasks
                .iter()
                .rev()
                .take(5)
                .map(|s| s.as_str())
                .collect();
            sections.push(format!(
                "Failed tasks (avoid similar):\n{}",
                recent_fails.iter().map(|t| format!("  - {t}")).collect::<Vec<_>>().join("\n")
            ));
        }

        // -- Available skills (gated) --
        if progress >= self.warm_up.skill_context {
            let skill_names = skill_store.list_skill_names();
            if !skill_names.is_empty() {
                let display: Vec<&str> = skill_names.iter().take(10).map(|s| s.as_str()).collect();
                sections.push(format!(
                    "Known skills: {}",
                    display.join(", ")
                ));
            }
        }

        let context_block = sections.join("\n\n");

        format!(
            "You are the AI brain of Phantom, an AI-native terminal emulator.\n\
             Your goal is to discover diverse, useful tasks you can proactively \
             perform for the developer. Prioritize novel actions over repetitive ones.\n\n\
             Current state:\n{context_block}\n\n\
             Based on the current project state, completed tasks, and failed tasks, \
             propose ONE specific, actionable task that would be genuinely useful \
             to the developer right now. The task should be:\n\
             - Concrete (\"run cargo clippy and summarize warnings\" not \"help with code quality\")\n\
             - Novel (not already completed)\n\
             - Appropriately difficult (not too easy, not too hard given past failures)\n\
             - Proactive (something the developer hasn't asked for but would appreciate)\n\n\
             Respond in this exact format:\n\
             Reasoning: <why this task is useful right now>\n\
             Task: <the specific task description>\n\
             Difficulty: <low|medium|high>"
        )
    }

    /// Parse an LLM response into a ProposedTask.
    ///
    /// Expected format:
    /// ```text
    /// Reasoning: <why>
    /// Task: <what>
    /// Difficulty: <low|medium|high>
    /// ```
    pub fn parse_response(&mut self, response: &str) -> Option<ProposedTask> {
        let mut reasoning = String::new();
        let mut task_desc = String::new();
        let mut difficulty = 0.5_f32;

        for line in response.lines() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("Reasoning:") {
                reasoning = rest.trim().to_owned();
            } else if let Some(rest) = trimmed.strip_prefix("Task:") {
                task_desc = rest.trim().to_owned();
            } else if let Some(rest) = trimmed.strip_prefix("Difficulty:") {
                difficulty = match rest.trim().to_lowercase().as_str() {
                    "low" => 0.2,
                    "medium" => 0.5,
                    "high" => 0.8,
                    _ => 0.5,
                };
            }
        }

        if task_desc.is_empty() {
            return None;
        }

        self.last_proposal_time = Some(Instant::now());

        Some(ProposedTask {
            task: AgentTask::FreeForm {
                prompt: task_desc.clone(),
            },
            reasoning,
            estimated_difficulty: difficulty,
        })
    }

    // -----------------------------------------------------------------------
    // Hardcoded bootstrap tasks (Voyager: "Mine 1 wood log")
    // -----------------------------------------------------------------------

    /// Generate a bootstrap task when progress is 0 (no LLM needed).
    ///
    /// Like Voyager's hardcoded first task, we start with something simple
    /// and universally useful.
    pub fn bootstrap_task(&mut self, _context: &ProjectContext) -> Option<ProposedTask> {
        if !self.ready_to_propose() {
            return None;
        }

        let completed: HashSet<&str> = self.completed_tasks.iter().map(|s| s.as_str()).collect();

        // Ordered list of bootstrap tasks -- propose the first one not yet completed.
        let bootstrap_sequence: Vec<(&str, &str)> = vec![
            ("Detect project structure and record conventions to memory",
             "First contact: learn the project layout"),
            ("Run the build command and verify it compiles cleanly",
             "Verify the project builds"),
            ("Run the test suite and summarize results",
             "Verify tests pass"),
            ("Check for common linting issues",
             "Code quality baseline"),
        ];

        for (task, reasoning) in bootstrap_sequence {
            if !completed.contains(task) {
                self.last_proposal_time = Some(Instant::now());
                return Some(ProposedTask {
                    task: AgentTask::FreeForm {
                        prompt: task.to_owned(),
                    },
                    reasoning: reasoning.to_owned(),
                    estimated_difficulty: 0.2,
                });
            }
        }

        None // All bootstrap tasks completed, use LLM-driven proposals.
    }
}

impl Default for CurriculumGenerator {
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
    use phantom_context::*;

    fn test_context() -> ProjectContext {
        ProjectContext {
            root: "/tmp/test".into(),
            name: "test-project".into(),
            project_type: ProjectType::Rust,
            package_manager: PackageManager::Cargo,
            framework: Framework::None,
            commands: ProjectCommands {
                build: Some("cargo build".into()),
                test: Some("cargo test".into()),
                run: None,
                lint: Some("cargo clippy".into()),
                format: None,
            },
            git: Some(GitInfo {
                branch: "main".into(),
                remote_url: None,
                is_dirty: false,
                ahead: 0,
                behind: 0,
                last_commit_message: Some("initial commit".into()),
                last_commit_age: None,
            }),
            rust_version: None,
            node_version: None,
            python_version: None,
        }
    }

    #[test]
    fn bootstrap_proposes_first_task() {
        let mut curriculum = CurriculumGenerator::new();
        let ctx = test_context();
        let task = curriculum.bootstrap_task(&ctx);
        assert!(task.is_some());
        let t = task.unwrap();
        assert!(
            matches!(&t.task, AgentTask::FreeForm { prompt } if prompt.contains("project structure")),
            "first bootstrap should be project detection"
        );
    }

    #[test]
    fn bootstrap_skips_completed_tasks() {
        let mut curriculum = CurriculumGenerator::new();
        curriculum.record_success("Detect project structure and record conventions to memory");
        let ctx = test_context();
        let task = curriculum.bootstrap_task(&ctx);
        assert!(task.is_some());
        let t = task.unwrap();
        assert!(
            matches!(&t.task, AgentTask::FreeForm { prompt } if prompt.contains("build command")),
            "should skip to second bootstrap task"
        );
    }

    #[test]
    fn record_success_removes_from_failed() {
        let mut curriculum = CurriculumGenerator::new();
        curriculum.record_failure("run tests");
        assert_eq!(curriculum.failed_tasks.len(), 1);
        curriculum.record_success("run tests");
        assert!(curriculum.failed_tasks.is_empty());
        assert_eq!(curriculum.completed_tasks.len(), 1);
    }

    #[test]
    fn record_failure_ignores_already_completed() {
        let mut curriculum = CurriculumGenerator::new();
        curriculum.record_success("run tests");
        curriculum.record_failure("run tests");
        assert!(curriculum.failed_tasks.is_empty(), "should not add completed task to failures");
    }

    #[test]
    fn parse_valid_response() {
        let mut curriculum = CurriculumGenerator::new();
        let response = "\
            Reasoning: The project has no clippy checks recorded\n\
            Task: Run cargo clippy and summarize top 3 warnings\n\
            Difficulty: medium";
        let task = curriculum.parse_response(response);
        assert!(task.is_some());
        let t = task.unwrap();
        assert!(matches!(&t.task, AgentTask::FreeForm { prompt } if prompt.contains("clippy")));
        assert!((t.estimated_difficulty - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn parse_empty_task_returns_none() {
        let mut curriculum = CurriculumGenerator::new();
        let response = "Reasoning: something\nDifficulty: low";
        assert!(curriculum.parse_response(response).is_none());
    }

    #[test]
    fn progress_tracks_completions() {
        let mut curriculum = CurriculumGenerator::new();
        assert_eq!(curriculum.progress(), 0);
        curriculum.record_success("task 1");
        curriculum.record_success("task 2");
        assert_eq!(curriculum.progress(), 2);
    }

    #[test]
    fn build_prompt_includes_project_name() {
        let curriculum = CurriculumGenerator::new();
        let ctx = test_context();
        let dir = tempfile::tempdir().unwrap();
        let memory = phantom_memory::MemoryStore::open_in("/tmp/test", dir.path()).unwrap();
        let skill_store = SkillStore::new();
        let prompt = curriculum.build_prompt(&ctx, &memory, &skill_store);
        assert!(prompt.contains("test-project"));
        assert!(prompt.contains("Phantom"));
    }

    #[test]
    fn warm_up_gates_memory_context() {
        let curriculum = CurriculumGenerator::new();
        let ctx = test_context();
        let dir = tempfile::tempdir().unwrap();
        let mut memory = phantom_memory::MemoryStore::open_in("/tmp/test", dir.path()).unwrap();
        memory
            .set("test_key", "test_value", phantom_memory::MemoryCategory::Convention, phantom_memory::MemorySource::Auto)
            .unwrap();
        let skill_store = SkillStore::new();

        // At progress 0, memory should NOT be included (gate = 3).
        let prompt = curriculum.build_prompt(&ctx, &memory, &skill_store);
        assert!(!prompt.contains("test_key"), "memory should be gated at progress 0");
    }
}
