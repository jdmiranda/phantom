//! Agent struct and lifecycle management.
//!
//! Each [`Agent`] represents an autonomous AI worker that runs in its own
//! terminal pane. Agents carry a conversation history, a task description,
//! and a visible output log that the renderer streams into the pane.

use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

use crate::tools::{ToolCall, ToolResult};

// ---------------------------------------------------------------------------
// AgentId
// ---------------------------------------------------------------------------

/// Unique agent identifier (monotonically increasing within a session).
pub type AgentId = u32;

// ---------------------------------------------------------------------------
// AgentStatus
// ---------------------------------------------------------------------------

/// Agent lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentStatus {
    /// Waiting to start (queued behind concurrency limit).
    Queued,
    /// Actively processing / reasoning.
    Working,
    /// Called a tool, waiting for the result.
    WaitingForTool,
    /// Completed successfully.
    Done,
    /// Completed with an error.
    Failed,
}

// ---------------------------------------------------------------------------
// AgentTask
// ---------------------------------------------------------------------------

/// What kind of task the agent is performing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentTask {
    /// Fix a compiler/runtime error.
    FixError {
        error_summary: String,
        file: Option<String>,
        context: String,
    },
    /// Run a shell command and report results.
    RunCommand { command: String },
    /// Review code in the given files.
    ReviewCode {
        files: Vec<String>,
        context: String,
    },
    /// Open-ended prompt (user-defined task).
    FreeForm { prompt: String },
    /// Watch a condition and notify when it changes.
    WatchAndNotify { description: String },
}

// ---------------------------------------------------------------------------
// AgentMessage
// ---------------------------------------------------------------------------

/// A message in the agent's conversation history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentMessage {
    /// System prompt establishing the agent's role.
    System(String),
    /// User/task input.
    User(String),
    /// AI assistant response.
    Assistant(String),
    /// Agent wants to invoke a tool.
    ToolCall(ToolCall),
    /// Result returned from a tool execution.
    ToolResult(ToolResult),
}

// ---------------------------------------------------------------------------
// Agent
// ---------------------------------------------------------------------------

/// An AI agent that works in its own terminal pane context.
#[derive(Debug)]
pub struct Agent {
    /// Unique identifier.
    pub id: AgentId,
    /// The task this agent was spawned to perform.
    pub task: AgentTask,
    /// Current lifecycle status.
    pub status: AgentStatus,
    /// Full conversation history (system + user + assistant + tools).
    pub messages: Vec<AgentMessage>,
    /// Visible output lines shown in the agent pane.
    pub output_log: Vec<String>,
    /// When this agent was created.
    pub created_at: Instant,
    /// When this agent finished (if it has).
    pub completed_at: Option<Instant>,
}

impl Agent {
    /// Create a new agent in `Queued` status.
    pub fn new(id: AgentId, task: AgentTask) -> Self {
        Self {
            id,
            task,
            status: AgentStatus::Queued,
            messages: Vec::new(),
            output_log: Vec::new(),
            created_at: Instant::now(),
            completed_at: None,
        }
    }

    /// Append a message to the conversation history.
    pub fn push_message(&mut self, msg: AgentMessage) {
        self.messages.push(msg);
    }

    /// Append visible output text (shown in the agent pane).
    pub fn log(&mut self, text: &str) {
        self.output_log.push(text.to_owned());
    }

    /// Mark the agent as completed.
    pub fn complete(&mut self, success: bool) {
        self.status = if success {
            AgentStatus::Done
        } else {
            AgentStatus::Failed
        };
        self.completed_at = Some(Instant::now());
    }

    /// Duration since creation.
    pub fn elapsed(&self) -> Duration {
        self.created_at.elapsed()
    }

    /// Build the system prompt based on the task type.
    ///
    /// This prompt is sent as the first message when the agent begins work.
    /// It establishes the agent's role, constraints, and expected output format.
    pub fn system_prompt(&self) -> String {
        match &self.task {
            AgentTask::FixError {
                error_summary,
                file,
                context,
            } => {
                let file_hint = file
                    .as_deref()
                    .map(|f| format!(" The error is in `{f}`."))
                    .unwrap_or_default();
                format!(
                    "You are a code repair agent in the Phantom terminal.\n\
                     Your job: fix the following error and verify the fix compiles.\n\n\
                     Error: {error_summary}\n\
                     {file_hint}\n\
                     Context: {context}\n\n\
                     Steps:\n\
                     1. Read the relevant file(s).\n\
                     2. Identify the root cause.\n\
                     3. Write the fix.\n\
                     4. Run the build to verify.\n\
                     5. Report what you changed and why."
                )
            }
            AgentTask::RunCommand { command } => {
                format!(
                    "You are a command execution agent in the Phantom terminal.\n\
                     Run the following command, observe the output, and report the results.\n\n\
                     Command: `{command}`\n\n\
                     If the command fails, analyze the error and suggest a fix."
                )
            }
            AgentTask::ReviewCode { files, context } => {
                let file_list = files.join(", ");
                format!(
                    "You are a code review agent in the Phantom terminal.\n\
                     Review the following files for bugs, style issues, and improvements.\n\n\
                     Files: {file_list}\n\
                     Context: {context}\n\n\
                     For each issue found, state the file, line, severity, and suggested fix."
                )
            }
            AgentTask::FreeForm { prompt } => {
                format!(
                    "You are an AI assistant agent in the Phantom terminal.\n\
                     You have access to file and command tools in the project directory.\n\n\
                     Task: {prompt}"
                )
            }
            AgentTask::WatchAndNotify { description } => {
                format!(
                    "You are a monitoring agent in the Phantom terminal.\n\
                     Watch the following condition and notify when it changes.\n\n\
                     Watch: {description}\n\n\
                     Periodically check the condition using available tools. \
                     When a change is detected, report it clearly."
                )
            }
        }
    }

    /// Get a one-line status description for display in the UI.
    pub fn status_line(&self) -> String {
        let task_summary = match &self.task {
            AgentTask::FixError { error_summary, .. } => {
                let truncated = truncate(error_summary, 40);
                format!("fix: {truncated}")
            }
            AgentTask::RunCommand { command } => {
                let truncated = truncate(command, 40);
                format!("run: {truncated}")
            }
            AgentTask::ReviewCode { files, .. } => {
                format!("review: {} file(s)", files.len())
            }
            AgentTask::FreeForm { prompt } => {
                let truncated = truncate(prompt, 40);
                format!("task: {truncated}")
            }
            AgentTask::WatchAndNotify { description } => {
                let truncated = truncate(description, 40);
                format!("watch: {truncated}")
            }
        };

        let status_tag = match self.status {
            AgentStatus::Queued => "QUEUED",
            AgentStatus::Working => "WORKING",
            AgentStatus::WaitingForTool => "WAITING",
            AgentStatus::Done => "DONE",
            AgentStatus::Failed => "FAILED",
        };

        let elapsed = self.elapsed();
        format!(
            "[agent-{}] [{status_tag}] {task_summary} ({:.1}s)",
            self.id,
            elapsed.as_secs_f64()
        )
    }
}

/// Truncate a string to `max_len` characters, appending "..." if truncated.
fn truncate(s: &str, max_len: usize) -> String {
    if s.len() > max_len {
        format!("{}...", &s[..max_len])
    } else {
        s.to_owned()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolType;

    #[test]
    fn new_agent_starts_queued() {
        let agent = Agent::new(
            1,
            AgentTask::FreeForm {
                prompt: "hello".into(),
            },
        );
        assert_eq!(agent.id, 1);
        assert_eq!(agent.status, AgentStatus::Queued);
        assert!(agent.messages.is_empty());
        assert!(agent.output_log.is_empty());
        assert!(agent.completed_at.is_none());
    }

    #[test]
    fn push_message_appends() {
        let mut agent = Agent::new(
            1,
            AgentTask::FreeForm {
                prompt: "test".into(),
            },
        );
        agent.push_message(AgentMessage::User("hello".into()));
        agent.push_message(AgentMessage::Assistant("hi".into()));
        assert_eq!(agent.messages.len(), 2);
    }

    #[test]
    fn log_appends_output() {
        let mut agent = Agent::new(
            1,
            AgentTask::FreeForm {
                prompt: "test".into(),
            },
        );
        agent.log("line 1");
        agent.log("line 2");
        assert_eq!(agent.output_log, vec!["line 1", "line 2"]);
    }

    #[test]
    fn complete_success_sets_done() {
        let mut agent = Agent::new(
            1,
            AgentTask::FreeForm {
                prompt: "test".into(),
            },
        );
        agent.complete(true);
        assert_eq!(agent.status, AgentStatus::Done);
        assert!(agent.completed_at.is_some());
    }

    #[test]
    fn complete_failure_sets_failed() {
        let mut agent = Agent::new(
            1,
            AgentTask::FreeForm {
                prompt: "test".into(),
            },
        );
        agent.complete(false);
        assert_eq!(agent.status, AgentStatus::Failed);
        assert!(agent.completed_at.is_some());
    }

    #[test]
    fn elapsed_returns_duration() {
        let agent = Agent::new(
            1,
            AgentTask::FreeForm {
                prompt: "test".into(),
            },
        );
        // Elapsed should be very small but valid.
        let _ = agent.elapsed();
    }

    #[test]
    fn system_prompt_fix_error_includes_summary() {
        let agent = Agent::new(
            1,
            AgentTask::FixError {
                error_summary: "mismatched types".into(),
                file: Some("src/main.rs".into()),
                context: "cargo build".into(),
            },
        );
        let prompt = agent.system_prompt();
        assert!(prompt.contains("mismatched types"));
        assert!(prompt.contains("src/main.rs"));
        assert!(prompt.contains("code repair agent"));
    }

    #[test]
    fn system_prompt_fix_error_without_file() {
        let agent = Agent::new(
            1,
            AgentTask::FixError {
                error_summary: "segfault".into(),
                file: None,
                context: "runtime crash".into(),
            },
        );
        let prompt = agent.system_prompt();
        assert!(prompt.contains("segfault"));
        // No file hint should appear.
        assert!(!prompt.contains("The error is in"));
    }

    #[test]
    fn system_prompt_run_command() {
        let agent = Agent::new(
            1,
            AgentTask::RunCommand {
                command: "cargo test".into(),
            },
        );
        let prompt = agent.system_prompt();
        assert!(prompt.contains("cargo test"));
        assert!(prompt.contains("command execution agent"));
    }

    #[test]
    fn system_prompt_review_code() {
        let agent = Agent::new(
            1,
            AgentTask::ReviewCode {
                files: vec!["src/lib.rs".into(), "src/main.rs".into()],
                context: "pre-merge review".into(),
            },
        );
        let prompt = agent.system_prompt();
        assert!(prompt.contains("src/lib.rs"));
        assert!(prompt.contains("code review agent"));
    }

    #[test]
    fn system_prompt_freeform() {
        let agent = Agent::new(
            1,
            AgentTask::FreeForm {
                prompt: "refactor the parser".into(),
            },
        );
        let prompt = agent.system_prompt();
        assert!(prompt.contains("refactor the parser"));
    }

    #[test]
    fn system_prompt_watch() {
        let agent = Agent::new(
            1,
            AgentTask::WatchAndNotify {
                description: "CI pipeline status".into(),
            },
        );
        let prompt = agent.system_prompt();
        assert!(prompt.contains("CI pipeline status"));
        assert!(prompt.contains("monitoring agent"));
    }

    #[test]
    fn status_line_contains_id_and_status() {
        let agent = Agent::new(
            42,
            AgentTask::FreeForm {
                prompt: "do something".into(),
            },
        );
        let line = agent.status_line();
        assert!(line.contains("agent-42"));
        assert!(line.contains("QUEUED"));
        assert!(line.contains("task:"));
    }

    #[test]
    fn status_line_truncates_long_prompt() {
        let long_prompt = "a".repeat(100);
        let agent = Agent::new(
            1,
            AgentTask::FreeForm {
                prompt: long_prompt,
            },
        );
        let line = agent.status_line();
        assert!(line.contains("..."));
    }

    #[test]
    fn status_line_review_shows_file_count() {
        let agent = Agent::new(
            1,
            AgentTask::ReviewCode {
                files: vec!["a.rs".into(), "b.rs".into(), "c.rs".into()],
                context: "test".into(),
            },
        );
        let line = agent.status_line();
        assert!(line.contains("review: 3 file(s)"));
    }

    #[test]
    fn tool_call_message_round_trips_through_serde() {
        let call = ToolCall {
            tool: ToolType::ReadFile,
            args: serde_json::json!({"path": "test.txt"}),
        };
        let msg = AgentMessage::ToolCall(call);
        let json = serde_json::to_string(&msg).unwrap();
        let _deser: AgentMessage = serde_json::from_str(&json).unwrap();
    }
}
