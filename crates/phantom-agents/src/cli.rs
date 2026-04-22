//! Agent CLI command parsing and execution.
//!
//! Translates user input from the terminal command mode (backtick) or shell
//! into agent lifecycle operations. All commands funnel through
//! [`parse_agent_command`] and [`execute_agent_command`].

use crate::agent::{Agent, AgentId, AgentStatus, AgentTask};
use crate::manager::AgentManager;

// ---------------------------------------------------------------------------
// AgentCommand
// ---------------------------------------------------------------------------

/// A parsed agent command from user input.
#[derive(Debug, Clone)]
pub enum AgentCommand {
    /// Spawn a new agent: `agent "fix the failing tests"`
    Spawn { prompt: String },

    /// Spawn with specific task type: `agent fix src/main.rs`
    SpawnFix { target: String },

    /// Spawn a review: `agent review`
    SpawnReview,

    /// Spawn a watcher: `agent watch CI`
    SpawnWatch { description: String },

    /// List all agents: `agents`
    List,

    /// Show agent details: `agent 3`
    Show { id: AgentId },

    /// Kill an agent: `agent kill 3`
    Kill { id: AgentId },

    /// Kill all agents: `agent kill-all`
    KillAll,

    /// Show help: `agent help`
    Help,
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Parse a command string into an [`AgentCommand`].
///
/// Accepted input formats:
///
/// ```text
/// agent "fix the failing tests"
/// agent fix src/main.rs
/// agent review
/// agent watch CI pipeline
/// agents
/// agent 3
/// agent kill 3
/// agent kill-all
/// agent help
/// ```
pub fn parse_agent_command(input: &str) -> Option<AgentCommand> {
    let trimmed = input.trim();

    // `agents` is the list shorthand.
    if trimmed == "agents" {
        return Some(AgentCommand::List);
    }

    // Everything else must start with `agent`.
    let rest = trimmed.strip_prefix("agent")?;

    // Bare `agent` with nothing after it -- show help.
    let rest = rest.trim_start();
    if rest.is_empty() {
        return Some(AgentCommand::Help);
    }

    // Quoted prompt: `agent "do something"` or `agent 'do something'`
    if rest.starts_with('"') || rest.starts_with('\'') {
        let quote = rest.as_bytes()[0] as char;
        let inner = &rest[1..];
        let prompt = if let Some(end) = inner.find(quote) {
            &inner[..end]
        } else {
            // Unterminated quote -- take the rest.
            inner
        };
        if prompt.is_empty() {
            return None;
        }
        return Some(AgentCommand::Spawn {
            prompt: prompt.to_owned(),
        });
    }

    // Split into subcommand + args.
    let mut parts = rest.splitn(2, char::is_whitespace);
    let sub = parts.next().unwrap_or("");
    let arg = parts.next().map(str::trim).unwrap_or("");

    match sub {
        "help" => Some(AgentCommand::Help),

        "fix" => {
            if arg.is_empty() {
                None
            } else {
                Some(AgentCommand::SpawnFix {
                    target: arg.to_owned(),
                })
            }
        }

        "review" => Some(AgentCommand::SpawnReview),

        "watch" => {
            if arg.is_empty() {
                None
            } else {
                Some(AgentCommand::SpawnWatch {
                    description: arg.to_owned(),
                })
            }
        }

        "kill" => {
            if arg == "all" || sub == "kill-all" {
                Some(AgentCommand::KillAll)
            } else if let Ok(id) = arg.parse::<AgentId>() {
                Some(AgentCommand::Kill { id })
            } else {
                None
            }
        }

        "kill-all" => Some(AgentCommand::KillAll),

        "list" => Some(AgentCommand::List),

        // Bare number: `agent 3`
        _ => {
            if let Ok(id) = sub.parse::<AgentId>() {
                Some(AgentCommand::Show { id })
            } else {
                // Treat everything else as a freeform prompt.
                Some(AgentCommand::Spawn {
                    prompt: rest.to_owned(),
                })
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

/// Execute an agent command against the manager.
/// Returns lines of text output to display to the user.
pub fn execute_agent_command(cmd: &AgentCommand, manager: &mut AgentManager) -> Vec<String> {
    match cmd {
        AgentCommand::Spawn { prompt } => {
            let task = AgentTask::FreeForm {
                prompt: prompt.clone(),
            };
            let id = manager.spawn(task);
            vec![format!("spawned agent #{id}: {prompt}")]
        }

        AgentCommand::SpawnFix { target } => {
            let task = AgentTask::FixError {
                error_summary: format!("fix {target}"),
                file: Some(target.clone()),
                context: "user-initiated fix".into(),
            };
            let id = manager.spawn(task);
            vec![format!("spawned fix agent #{id} targeting {target}")]
        }

        AgentCommand::SpawnReview => {
            let task = AgentTask::ReviewCode {
                files: Vec::new(),
                context: "user-initiated review".into(),
            };
            let id = manager.spawn(task);
            vec![format!("spawned review agent #{id}")]
        }

        AgentCommand::SpawnWatch { description } => {
            let task = AgentTask::WatchAndNotify {
                description: description.clone(),
            };
            let id = manager.spawn(task);
            vec![format!("spawned watch agent #{id}: {description}")]
        }

        AgentCommand::List => format_agent_list(manager),

        AgentCommand::Show { id } => {
            if let Some(agent) = manager.get(*id) {
                format_agent_detail(agent)
            } else {
                vec![format!("agent #{id} not found")]
            }
        }

        AgentCommand::Kill { id } => {
            if manager.kill(*id) {
                vec![format!("killed agent #{id}")]
            } else {
                vec![format!("agent #{id} not found or already finished")]
            }
        }

        AgentCommand::KillAll => {
            let count = manager.kill_all();
            if count == 0 {
                vec!["no active agents to kill".into()]
            } else {
                vec![format!("killed {count} agent(s)")]
            }
        }

        AgentCommand::Help => help_text(),
    }
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

/// Format the agent list as a table (for `agents` command).
pub fn format_agent_list(manager: &AgentManager) -> Vec<String> {
    let agents = manager.agents();
    if agents.is_empty() {
        return vec![
            "+------------------------------------------------------------------+".into(),
            "|  PHANTOM AGENTS                                                  |".into(),
            "+------------------------------------------------------------------+".into(),
            "|  No agents running.                                              |".into(),
            "+------------------------------------------------------------------+".into(),
        ];
    }

    let mut lines = Vec::new();
    lines.push("+------------------------------------------------------------------+".into());
    lines.push("|  PHANTOM AGENTS                                                  |".into());
    lines.push("+------------------------------------------------------------------+".into());
    lines.push("|  ID    | STATUS   | TASK                        | TIME          |".into());
    lines.push("|--------|----------|-----------------------------|---------------|".into());

    for agent in agents {
        let id = format!("#{}", agent.id);
        let status = status_tag(agent.status);
        let task = task_summary(&agent.task);
        let time = format_duration(agent.elapsed());
        lines.push(format!(
            "|  {:<5} | {:<8} | {:<27} | {:<13} |",
            id, status, task, time,
        ));
    }

    lines.push("+------------------------------------------------------------------+".into());
    lines
}

/// Format a single agent's details (for `agent 3` command).
pub fn format_agent_detail(agent: &Agent) -> Vec<String> {
    let mut lines = Vec::new();

    lines.push(format!("Agent #{}", agent.id));
    lines.push(format!("  Status:  {}", status_tag(agent.status)));
    lines.push(format!("  Task:    {}", task_detail(&agent.task)));
    lines.push(format!("  Elapsed: {}", format_duration(agent.elapsed())));
    lines.push(format!("  Messages: {}", agent.messages.len()));

    if !agent.output_log.is_empty() {
        lines.push("  Output:".into());
        let recent = if agent.output_log.len() > 10 {
            &agent.output_log[agent.output_log.len() - 10..]
        } else {
            &agent.output_log
        };
        for line in recent {
            lines.push(format!("    {line}"));
        }
    }

    lines
}

// ---------------------------------------------------------------------------
// Help text
// ---------------------------------------------------------------------------

fn help_text() -> Vec<String> {
    vec![
        "PHANTOM AGENT COMMANDS".into(),
        "".into(),
        "  agent \"<prompt>\"       Spawn an agent with a freeform task".into(),
        "  agent fix <target>     Spawn a fix agent targeting a file".into(),
        "  agent review           Spawn a code review agent".into(),
        "  agent watch <desc>     Spawn a monitoring agent".into(),
        "  agents                 List all agents".into(),
        "  agent <id>             Show agent details".into(),
        "  agent kill <id>        Kill an agent".into(),
        "  agent kill-all         Kill all active agents".into(),
        "  agent help             Show this help".into(),
    ]
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn status_tag(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::Queued => "QUEUED",
        AgentStatus::Working => "WORKING",
        AgentStatus::WaitingForTool => "WAITING",
        AgentStatus::Done => "DONE",
        AgentStatus::Failed => "FAILED",
    }
}

/// One-line task summary for the table view (max ~27 chars).
fn task_summary(task: &AgentTask) -> String {
    let raw = match task {
        AgentTask::FixError { error_summary, .. } => format!("fix: {error_summary}"),
        AgentTask::RunCommand { command } => format!("run: {command}"),
        AgentTask::ReviewCode { files, .. } => format!("review: {} file(s)", files.len()),
        AgentTask::FreeForm { prompt } => prompt.clone(),
        AgentTask::WatchAndNotify { description } => format!("watch: {description}"),
    };
    truncate(&raw, 27)
}

/// Detailed task description for `agent <id>`.
fn task_detail(task: &AgentTask) -> String {
    match task {
        AgentTask::FixError {
            error_summary,
            file,
            ..
        } => {
            let file_hint = file
                .as_deref()
                .map(|f| format!(" ({f})"))
                .unwrap_or_default();
            format!("Fix: {error_summary}{file_hint}")
        }
        AgentTask::RunCommand { command } => format!("Run: {command}"),
        AgentTask::ReviewCode { files, context } => {
            format!("Review {} file(s) - {context}", files.len())
        }
        AgentTask::FreeForm { prompt } => prompt.clone(),
        AgentTask::WatchAndNotify { description } => format!("Watch: {description}"),
    }
}

fn truncate(s: &str, max_len: usize) -> String {
    if s.len() > max_len {
        format!("{}...", &s[..max_len.saturating_sub(3)])
    } else {
        s.to_owned()
    }
}

fn format_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{:.1}s", d.as_secs_f64())
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- Parsing tests ---------------------------------------------------------

    #[test]
    fn parse_agents_returns_list() {
        let cmd = parse_agent_command("agents").unwrap();
        assert!(matches!(cmd, AgentCommand::List));
    }

    #[test]
    fn parse_agent_list_returns_list() {
        let cmd = parse_agent_command("agent list").unwrap();
        assert!(matches!(cmd, AgentCommand::List));
    }

    #[test]
    fn parse_agent_help() {
        let cmd = parse_agent_command("agent help").unwrap();
        assert!(matches!(cmd, AgentCommand::Help));
    }

    #[test]
    fn parse_bare_agent_returns_help() {
        let cmd = parse_agent_command("agent").unwrap();
        assert!(matches!(cmd, AgentCommand::Help));
    }

    #[test]
    fn parse_quoted_prompt_double() {
        let cmd = parse_agent_command(r#"agent "fix the failing tests""#).unwrap();
        match cmd {
            AgentCommand::Spawn { prompt } => assert_eq!(prompt, "fix the failing tests"),
            other => panic!("expected Spawn, got {other:?}"),
        }
    }

    #[test]
    fn parse_quoted_prompt_single() {
        let cmd = parse_agent_command("agent 'refactor the parser'").unwrap();
        match cmd {
            AgentCommand::Spawn { prompt } => assert_eq!(prompt, "refactor the parser"),
            other => panic!("expected Spawn, got {other:?}"),
        }
    }

    #[test]
    fn parse_fix_command() {
        let cmd = parse_agent_command("agent fix src/main.rs").unwrap();
        match cmd {
            AgentCommand::SpawnFix { target } => assert_eq!(target, "src/main.rs"),
            other => panic!("expected SpawnFix, got {other:?}"),
        }
    }

    #[test]
    fn parse_fix_without_target_returns_none() {
        assert!(parse_agent_command("agent fix").is_none());
    }

    #[test]
    fn parse_review_command() {
        let cmd = parse_agent_command("agent review").unwrap();
        assert!(matches!(cmd, AgentCommand::SpawnReview));
    }

    #[test]
    fn parse_watch_command() {
        let cmd = parse_agent_command("agent watch CI pipeline").unwrap();
        match cmd {
            AgentCommand::SpawnWatch { description } => {
                assert_eq!(description, "CI pipeline");
            }
            other => panic!("expected SpawnWatch, got {other:?}"),
        }
    }

    #[test]
    fn parse_watch_without_desc_returns_none() {
        assert!(parse_agent_command("agent watch").is_none());
    }

    #[test]
    fn parse_show_by_id() {
        let cmd = parse_agent_command("agent 3").unwrap();
        match cmd {
            AgentCommand::Show { id } => assert_eq!(id, 3),
            other => panic!("expected Show, got {other:?}"),
        }
    }

    #[test]
    fn parse_kill_by_id() {
        let cmd = parse_agent_command("agent kill 5").unwrap();
        match cmd {
            AgentCommand::Kill { id } => assert_eq!(id, 5),
            other => panic!("expected Kill, got {other:?}"),
        }
    }

    #[test]
    fn parse_kill_all() {
        let cmd = parse_agent_command("agent kill-all").unwrap();
        assert!(matches!(cmd, AgentCommand::KillAll));
    }

    #[test]
    fn parse_kill_all_via_kill_subcommand() {
        let cmd = parse_agent_command("agent kill all").unwrap();
        assert!(matches!(cmd, AgentCommand::KillAll));
    }

    #[test]
    fn parse_freeform_unquoted() {
        // Words that don't match a subcommand become a freeform prompt.
        let cmd = parse_agent_command("agent refactor the parser module").unwrap();
        match cmd {
            AgentCommand::Spawn { prompt } => {
                assert_eq!(prompt, "refactor the parser module");
            }
            other => panic!("expected Spawn, got {other:?}"),
        }
    }

    #[test]
    fn parse_non_agent_command_returns_none() {
        assert!(parse_agent_command("ls -la").is_none());
        assert!(parse_agent_command("git status").is_none());
        assert!(parse_agent_command("").is_none());
    }

    #[test]
    fn parse_whitespace_trimmed() {
        let cmd = parse_agent_command("  agents  ").unwrap();
        assert!(matches!(cmd, AgentCommand::List));
    }

    // -- Formatting tests ------------------------------------------------------

    #[test]
    fn format_list_empty_manager() {
        let mgr = AgentManager::new(4);
        let lines = format_agent_list(&mgr);
        assert!(lines.iter().any(|l| l.contains("No agents running")));
    }

    #[test]
    fn format_list_with_agents() {
        let mut mgr = AgentManager::new(4);
        mgr.spawn(AgentTask::FreeForm {
            prompt: "do something".into(),
        });
        mgr.spawn(AgentTask::ReviewCode {
            files: vec!["a.rs".into()],
            context: "test".into(),
        });

        let lines = format_agent_list(&mgr);
        // Header present.
        assert!(lines.iter().any(|l| l.contains("PHANTOM AGENTS")));
        // Both agents appear.
        assert!(lines.iter().any(|l| l.contains("#1")));
        assert!(lines.iter().any(|l| l.contains("#2")));
        assert!(lines.iter().any(|l| l.contains("review: 1 file(s)")));
    }

    #[test]
    fn format_detail_shows_agent_info() {
        let agent = Agent::new(
            7,
            AgentTask::FreeForm {
                prompt: "deploy to staging".into(),
            },
        );
        let lines = format_agent_detail(&agent);
        assert!(lines.iter().any(|l| l.contains("Agent #7")));
        assert!(lines.iter().any(|l| l.contains("QUEUED")));
        assert!(lines.iter().any(|l| l.contains("deploy to staging")));
    }

    #[test]
    fn format_detail_shows_output_log() {
        let mut agent = Agent::new(
            1,
            AgentTask::FreeForm {
                prompt: "test".into(),
            },
        );
        agent.log("reading file...");
        agent.log("done");

        let lines = format_agent_detail(&agent);
        assert!(lines.iter().any(|l| l.contains("reading file...")));
        assert!(lines.iter().any(|l| l.contains("done")));
    }

    // -- Execution tests -------------------------------------------------------

    #[test]
    fn execute_spawn_creates_agent() {
        let mut mgr = AgentManager::new(4);
        let cmd = AgentCommand::Spawn {
            prompt: "hello world".into(),
        };
        let output = execute_agent_command(&cmd, &mut mgr);
        assert_eq!(mgr.agents().len(), 1);
        assert!(output[0].contains("spawned agent #1"));
    }

    #[test]
    fn execute_spawn_fix_creates_fix_agent() {
        let mut mgr = AgentManager::new(4);
        let cmd = AgentCommand::SpawnFix {
            target: "src/main.rs".into(),
        };
        let output = execute_agent_command(&cmd, &mut mgr);
        assert_eq!(mgr.agents().len(), 1);
        assert!(output[0].contains("fix agent"));
        assert!(output[0].contains("src/main.rs"));
    }

    #[test]
    fn execute_spawn_review_creates_review_agent() {
        let mut mgr = AgentManager::new(4);
        let output = execute_agent_command(&AgentCommand::SpawnReview, &mut mgr);
        assert_eq!(mgr.agents().len(), 1);
        assert!(output[0].contains("review agent"));
    }

    #[test]
    fn execute_spawn_watch_creates_watch_agent() {
        let mut mgr = AgentManager::new(4);
        let cmd = AgentCommand::SpawnWatch {
            description: "CI pipeline".into(),
        };
        let output = execute_agent_command(&cmd, &mut mgr);
        assert_eq!(mgr.agents().len(), 1);
        assert!(output[0].contains("watch agent"));
    }

    #[test]
    fn execute_kill_removes_active_agent() {
        let mut mgr = AgentManager::new(4);
        let id = mgr.spawn(AgentTask::FreeForm {
            prompt: "work".into(),
        });
        let output = execute_agent_command(&AgentCommand::Kill { id }, &mut mgr);
        assert!(output[0].contains("killed"));
        assert_eq!(mgr.get(id).unwrap().status, AgentStatus::Failed);
    }

    #[test]
    fn execute_kill_nonexistent_reports_not_found() {
        let mut mgr = AgentManager::new(4);
        let output = execute_agent_command(&AgentCommand::Kill { id: 99 }, &mut mgr);
        assert!(output[0].contains("not found"));
    }

    #[test]
    fn execute_kill_all_kills_all_active() {
        let mut mgr = AgentManager::new(4);
        mgr.spawn(AgentTask::FreeForm {
            prompt: "a".into(),
        });
        mgr.spawn(AgentTask::FreeForm {
            prompt: "b".into(),
        });
        let output = execute_agent_command(&AgentCommand::KillAll, &mut mgr);
        assert!(output[0].contains("killed 2"));
    }

    #[test]
    fn execute_kill_all_no_agents() {
        let mut mgr = AgentManager::new(4);
        let output = execute_agent_command(&AgentCommand::KillAll, &mut mgr);
        assert!(output[0].contains("no active agents"));
    }

    #[test]
    fn execute_show_existing_agent() {
        let mut mgr = AgentManager::new(4);
        let id = mgr.spawn(AgentTask::FreeForm {
            prompt: "test task".into(),
        });
        let output = execute_agent_command(&AgentCommand::Show { id }, &mut mgr);
        assert!(output.iter().any(|l| l.contains("Agent #1")));
    }

    #[test]
    fn execute_show_nonexistent() {
        let mut mgr = AgentManager::new(4);
        let output = execute_agent_command(&AgentCommand::Show { id: 42 }, &mut mgr);
        assert!(output[0].contains("not found"));
    }

    #[test]
    fn execute_list_delegates_to_format() {
        let mut mgr = AgentManager::new(4);
        mgr.spawn(AgentTask::FreeForm {
            prompt: "hello".into(),
        });
        let output = execute_agent_command(&AgentCommand::List, &mut mgr);
        assert!(output.iter().any(|l| l.contains("PHANTOM AGENTS")));
    }

    #[test]
    fn execute_help_shows_usage() {
        let mut mgr = AgentManager::new(4);
        let output = execute_agent_command(&AgentCommand::Help, &mut mgr);
        assert!(output.iter().any(|l| l.contains("PHANTOM AGENT COMMANDS")));
    }

    // -- Helper tests ----------------------------------------------------------

    #[test]
    fn truncate_short_string_unchanged() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_long_string_adds_ellipsis() {
        let result = truncate("a]very long description here", 10);
        assert!(result.ends_with("..."));
        assert!(result.len() <= 10);
    }

    #[test]
    fn format_duration_seconds() {
        let d = std::time::Duration::from_secs_f64(4.2);
        let s = format_duration(d);
        assert!(s.contains("4.2s"));
    }

    #[test]
    fn format_duration_minutes() {
        let d = std::time::Duration::from_secs(83);
        let s = format_duration(d);
        assert_eq!(s, "1m23s");
    }

    #[test]
    fn format_duration_hours() {
        let d = std::time::Duration::from_secs(3661);
        let s = format_duration(d);
        assert_eq!(s, "1h01m");
    }
}
