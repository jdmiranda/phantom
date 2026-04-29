//! Agent CLI command parsing and execution.
//!
//! Translates user input from the terminal command mode (backtick) or shell
//! into agent lifecycle operations. All commands funnel through
//! [`parse_agent_command`] and [`execute_agent_command`].

use crate::agent::{Agent, AgentId, AgentStatus, AgentTask};
use crate::chat::ChatModel;
use crate::manager::AgentManager;
use crate::role::{AgentRole, CapabilityClass};

// ---------------------------------------------------------------------------
// SpawnFlags — parsed flag block
// ---------------------------------------------------------------------------

/// Optional overrides parsed from `--flag value` pairs in an agent command.
///
/// All fields are `None` when absent; callers fall back to system defaults.
/// The `warnings` field collects non-fatal parse errors (unknown role value,
/// non-numeric TTL, flags found after the prompt text, etc.) that are surfaced
/// to the user via [`execute_agent_command`] without aborting the spawn.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SpawnFlags {
    /// `--model <spec>` — chat-model override. Parsed by
    /// [`ChatModel::from_env_str`] so `claude`, `claude:opus-4-7`, `openai`,
    /// `openai:gpt-4o` are all valid.
    pub model: Option<ChatModel>,
    /// `--role <name>` — role override. Must be a known [`AgentRole`] label
    /// (case-insensitive).
    pub role: Option<AgentRole>,
    /// `--ttl <secs>` — maximum lifetime in seconds before the agent is
    /// auto-killed. Zero means "no limit".
    pub ttl_secs: Option<u64>,
    /// `--capability <class>` / `--cap <class>` — required capability class
    /// the caller expects the spawned role to hold. Validated at spawn time.
    pub capability: Option<CapabilityClass>,
    /// Non-fatal parse warnings accumulated during flag parsing.
    /// Displayed as prefixed lines in the spawn output so the user knows
    /// something was ignored rather than silently misbehaving.
    pub warnings: Vec<String>,
}

// ---------------------------------------------------------------------------
// AgentCommand
// ---------------------------------------------------------------------------

/// A parsed agent command from user input.
#[derive(Debug, Clone)]
pub enum AgentCommand {
    /// Spawn a new agent: `agent "fix the failing tests"`
    Spawn { prompt: String },

    /// Spawn with flags: `agent --model claude --role actor "do something"`
    SpawnWithFlags { prompt: String, flags: SpawnFlags },

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
/// agent --model claude "fix the failing tests"
/// agent --model openai:gpt-4o --role actor --ttl 300 --capability act "deploy"
/// agent "my prompt" --role actor          ← flags after prompt are accepted
/// agent fix src/main.rs
/// agent review
/// agent watch CI pipeline
/// agents
/// agent 3
/// agent kill 3
/// agent kill-all
/// agent help
/// ```
///
/// Flag scanning is position-independent: `--key value` pairs are extracted
/// from the full token list regardless of where they appear relative to the
/// prompt text. The remaining non-flag tokens are joined to form the prompt.
/// Unknown flag values (e.g. `--role badval`) emit a warning rather than
/// silently dropping the flag.
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

    // Route to flag-aware parsing whenever `--` appears anywhere in the
    // remainder.  This handles both `--flag prompt` and `prompt --flag`.
    if rest.contains("--") {
        return parse_flagged_spawn(rest);
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
// Flag parsing (internal)
// ---------------------------------------------------------------------------

/// Parse a mixed token stream of `--flag value` pairs and prompt text.
///
/// The parser makes a single pass through the token list.  Tokens that look
/// like a known flag consume the next token as their value.  All remaining
/// tokens are joined (in order) to form the prompt.  This means flags may
/// appear before, after, or interleaved with the prompt text.
///
/// Non-fatal problems (unknown role name, non-numeric TTL, duplicate flag)
/// are recorded in [`SpawnFlags::warnings`] and surfaced to the user via
/// [`execute_agent_command`] rather than silently dropped.
///
/// Returns `None` only when the reconstructed prompt is empty.
fn parse_flagged_spawn(rest: &str) -> Option<AgentCommand> {
    let tokens = tokenise(rest);
    let mut flags = SpawnFlags::default();
    let mut prompt_tokens: Vec<String> = Vec::new();
    let mut i = 0;

    while i < tokens.len() {
        match tokens[i].as_str() {
            "--model" => {
                i += 1;
                if let Some(val) = tokens.get(i) {
                    if flags.model.is_some() {
                        flags
                            .warnings
                            .push("duplicate --model flag; last value wins".into());
                    }
                    flags.model = Some(ChatModel::from_env_str(val).unwrap_or_else(|| {
                        // Treat unknown values as a raw Claude model id string.
                        ChatModel::Claude(val.clone())
                    }));
                }
            }
            "--role" => {
                i += 1;
                if let Some(val) = tokens.get(i) {
                    if flags.role.is_some() {
                        flags
                            .warnings
                            .push("duplicate --role flag; last value wins".into());
                    }
                    match parse_role(val) {
                        Some(role) => flags.role = Some(role),
                        None => flags.warnings.push(format!(
                            "unknown role '{}' — valid roles: conversational, watcher, \
                             capturer, transcriber, reflector, indexer, actor, composer, \
                             fixer, defender",
                            val
                        )),
                    }
                }
            }
            "--ttl" => {
                i += 1;
                if let Some(val) = tokens.get(i) {
                    if flags.ttl_secs.is_some() {
                        flags
                            .warnings
                            .push("duplicate --ttl flag; last value wins".into());
                    }
                    match val.parse::<u64>() {
                        Ok(secs) => flags.ttl_secs = Some(secs),
                        Err(_) => flags.warnings.push(format!(
                            "invalid --ttl value '{}' — expected a non-negative integer",
                            val
                        )),
                    }
                }
            }
            "--capability" | "--cap" => {
                i += 1;
                if let Some(val) = tokens.get(i) {
                    if flags.capability.is_some() {
                        flags
                            .warnings
                            .push("duplicate --capability flag; last value wins".into());
                    }
                    match parse_capability(val) {
                        Some(cap) => flags.capability = Some(cap),
                        None => flags.warnings.push(format!(
                            "unknown capability '{}' — valid values: sense, reflect, \
                             compute, act, coordinate",
                            val
                        )),
                    }
                }
            }
            tok => {
                // Not a recognised flag: treat as part of the prompt.
                prompt_tokens.push(tok.to_owned());
            }
        }
        i += 1;
    }

    let prompt = prompt_tokens.join(" ");
    if prompt.is_empty() {
        return None;
    }

    // If no flags were actually set (and no warnings), degrade to a plain Spawn.
    let any_flag = flags.model.is_some()
        || flags.role.is_some()
        || flags.ttl_secs.is_some()
        || flags.capability.is_some()
        || !flags.warnings.is_empty();

    if any_flag {
        Some(AgentCommand::SpawnWithFlags { prompt, flags })
    } else {
        Some(AgentCommand::Spawn { prompt })
    }
}

/// Tokenise `s` by whitespace, collapsing quoted spans into a single token.
///
/// The opening quote character (`"` or `'`) determines which character closes
/// the span — a double-quoted token is only closed by `"`, never by `'`, and
/// vice versa.  Only the outermost quote level is handled (no nesting).
/// An unclosed quote consumes to end-of-string.
fn tokenise(s: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut chars = s.chars().peekable();

    while chars.peek().is_some() {
        // Skip whitespace between tokens.
        while chars.peek().map(|c| c.is_whitespace()).unwrap_or(false) {
            chars.next();
        }
        if chars.peek().is_none() {
            break;
        }

        let first = *chars.peek().unwrap();
        if first == '"' || first == '\'' {
            // Consume opening quote; remember it to find the matching close.
            let open_quote = first;
            chars.next();
            let mut buf = String::new();
            loop {
                match chars.next() {
                    None => break,
                    Some(c) if c == open_quote => break,
                    Some(c) => buf.push(c),
                }
            }
            if !buf.is_empty() {
                tokens.push(buf);
            }
        } else {
            // Unquoted token: consume until whitespace.
            let mut buf = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() {
                    break;
                }
                buf.push(c);
                chars.next();
            }
            if !buf.is_empty() {
                tokens.push(buf);
            }
        }
    }

    tokens
}

/// Parse a role name (case-insensitive) into an [`AgentRole`].
fn parse_role(s: &str) -> Option<AgentRole> {
    match s.to_ascii_lowercase().as_str() {
        "conversational" | "conv" => Some(AgentRole::Conversational),
        "watcher" | "watch" => Some(AgentRole::Watcher),
        "capturer" | "capture" => Some(AgentRole::Capturer),
        "transcriber" | "transcribe" => Some(AgentRole::Transcriber),
        "reflector" | "reflect" => Some(AgentRole::Reflector),
        "indexer" | "index" => Some(AgentRole::Indexer),
        "actor" | "act" => Some(AgentRole::Actor),
        "composer" | "compose" => Some(AgentRole::Composer),
        "fixer" | "fix" => Some(AgentRole::Fixer),
        "defender" | "defend" => Some(AgentRole::Defender),
        _ => None,
    }
}

/// Parse a capability class name (case-insensitive) into a [`CapabilityClass`].
fn parse_capability(s: &str) -> Option<CapabilityClass> {
    match s.to_ascii_lowercase().as_str() {
        "sense" => Some(CapabilityClass::Sense),
        "reflect" => Some(CapabilityClass::Reflect),
        "compute" => Some(CapabilityClass::Compute),
        "act" => Some(CapabilityClass::Act),
        "coordinate" | "coord" => Some(CapabilityClass::Coordinate),
        _ => None,
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

        AgentCommand::SpawnWithFlags { prompt, flags } => {
            let task = AgentTask::FreeForm {
                prompt: prompt.clone(),
            };
            let id = manager.spawn(task);
            let mut lines = vec![format!("spawned agent #{id}: {prompt}")];
            if let Some(ref model) = flags.model {
                lines.push(format!("  model:      {}", format_model(model)));
            }
            if let Some(role) = flags.role {
                lines.push(format!("  role:       {}", role.label()));
            }
            if let Some(ttl) = flags.ttl_secs {
                if ttl == 0 {
                    lines.push("  ttl:        unlimited".into());
                } else {
                    lines.push(format!("  ttl:        {ttl}s"));
                }
            }
            if let Some(cap) = flags.capability {
                lines.push(format!("  capability: {}", format_capability(cap)));
            }
            for warn in &flags.warnings {
                lines.push(format!("  warning:    {warn}"));
            }
            lines
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
        "  agent \"<prompt>\"              Spawn an agent with a freeform task".into(),
        "  agent fix <target>            Spawn a fix agent targeting a file".into(),
        "  agent review                  Spawn a code review agent".into(),
        "  agent watch <desc>            Spawn a monitoring agent".into(),
        "  agents                        List all agents".into(),
        "  agent <id>                    Show agent details".into(),
        "  agent kill <id>               Kill an agent".into(),
        "  agent kill-all                Kill all active agents".into(),
        "  agent help                    Show this help".into(),
        "".into(),
        "FLAGS (position-independent — may appear before or after the prompt):".into(),
        "  --model <spec>        Chat backend: claude, claude:<id>, openai, openai:<id>".into(),
        "  --role  <name>        Role override: conversational, watcher, capturer,".into(),
        "                        transcriber, reflector, indexer, actor, composer,".into(),
        "                        fixer, defender".into(),
        "  --ttl   <secs>        Max lifetime in seconds (0 = unlimited)".into(),
        "  --capability <c>      Required capability: sense, reflect, compute, act,".into(),
        "  --cap <c>             coordinate  (--cap is an alias for --capability)".into(),
        "".into(),
        "EXAMPLES".into(),
        "  agent --model claude \"fix the failing tests\"".into(),
        "  agent --role actor --ttl 300 \"deploy to staging\"".into(),
        "  agent --model openai:gpt-4o --capability act \"refactor parser\"".into(),
        "  agent --cap coord \"orchestrate the pipeline\"".into(),
        "  agent \"my prompt\" --role actor          ← flags after prompt also work".into(),
    ]
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn format_model(model: &ChatModel) -> String {
    match model {
        ChatModel::Claude(id) => format!("claude:{id}"),
        ChatModel::OpenAi(id) => format!("openai:{id}"),
    }
}

fn format_capability(cap: CapabilityClass) -> &'static str {
    match cap {
        CapabilityClass::Sense => "sense",
        CapabilityClass::Reflect => "reflect",
        CapabilityClass::Compute => "compute",
        CapabilityClass::Act => "act",
        CapabilityClass::Coordinate => "coordinate",
    }
}

fn status_tag(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::Queued => "QUEUED",
        AgentStatus::Planning => "PLANNING",
        AgentStatus::AwaitingApproval => "PENDING",
        AgentStatus::Working => "WORKING",
        AgentStatus::WaitingForTool => "WAITING",
        AgentStatus::Done => "DONE",
        AgentStatus::Failed => "FAILED",
        AgentStatus::Flatline => "FLATLINE",
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

    // -- Flag / SpawnWithFlags tests -------------------------------------------

    #[test]
    fn parse_model_flag_claude_default() {
        let cmd = parse_agent_command(r#"agent --model claude "do the thing""#).unwrap();
        match cmd {
            AgentCommand::SpawnWithFlags { prompt, flags } => {
                assert_eq!(prompt, "do the thing");
                assert_eq!(flags.model, Some(ChatModel::default_claude()));
            }
            other => panic!("expected SpawnWithFlags, got {other:?}"),
        }
    }

    #[test]
    fn parse_model_flag_explicit_claude_id() {
        let cmd =
            parse_agent_command(r#"agent --model claude:claude-opus-4-7 "task""#).unwrap();
        match cmd {
            AgentCommand::SpawnWithFlags { prompt, flags } => {
                assert_eq!(prompt, "task");
                assert_eq!(
                    flags.model,
                    Some(ChatModel::Claude("claude-opus-4-7".into()))
                );
            }
            other => panic!("expected SpawnWithFlags, got {other:?}"),
        }
    }

    #[test]
    fn parse_model_flag_openai() {
        let cmd = parse_agent_command(r#"agent --model openai "do it""#).unwrap();
        match cmd {
            AgentCommand::SpawnWithFlags { flags, .. } => {
                assert_eq!(flags.model, Some(ChatModel::default_openai()));
            }
            other => panic!("expected SpawnWithFlags, got {other:?}"),
        }
    }

    #[test]
    fn parse_model_flag_openai_explicit_id() {
        let cmd =
            parse_agent_command(r#"agent --model openai:gpt-4o "task""#).unwrap();
        match cmd {
            AgentCommand::SpawnWithFlags { flags, .. } => {
                assert_eq!(flags.model, Some(ChatModel::OpenAi("gpt-4o".into())));
            }
            other => panic!("expected SpawnWithFlags, got {other:?}"),
        }
    }

    #[test]
    fn parse_role_flag_actor() {
        let cmd = parse_agent_command(r#"agent --role actor "deploy""#).unwrap();
        match cmd {
            AgentCommand::SpawnWithFlags { flags, .. } => {
                assert_eq!(flags.role, Some(AgentRole::Actor));
            }
            other => panic!("expected SpawnWithFlags, got {other:?}"),
        }
    }

    #[test]
    fn parse_role_flag_case_insensitive() {
        let cmd = parse_agent_command(r#"agent --role DEFENDER "watch""#).unwrap();
        match cmd {
            AgentCommand::SpawnWithFlags { flags, .. } => {
                assert_eq!(flags.role, Some(AgentRole::Defender));
            }
            other => panic!("expected SpawnWithFlags, got {other:?}"),
        }
    }

    #[test]
    fn parse_ttl_flag() {
        let cmd = parse_agent_command(r#"agent --ttl 300 "do it""#).unwrap();
        match cmd {
            AgentCommand::SpawnWithFlags { flags, .. } => {
                assert_eq!(flags.ttl_secs, Some(300));
            }
            other => panic!("expected SpawnWithFlags, got {other:?}"),
        }
    }

    #[test]
    fn parse_ttl_zero_means_unlimited() {
        let cmd = parse_agent_command(r#"agent --ttl 0 "run forever""#).unwrap();
        match cmd {
            AgentCommand::SpawnWithFlags { flags, .. } => {
                assert_eq!(flags.ttl_secs, Some(0));
            }
            other => panic!("expected SpawnWithFlags, got {other:?}"),
        }
    }

    #[test]
    fn parse_capability_flag_act() {
        let cmd = parse_agent_command(r#"agent --capability act "mutate""#).unwrap();
        match cmd {
            AgentCommand::SpawnWithFlags { flags, .. } => {
                assert_eq!(flags.capability, Some(CapabilityClass::Act));
            }
            other => panic!("expected SpawnWithFlags, got {other:?}"),
        }
    }

    #[test]
    fn parse_capability_flag_coord_alias() {
        let cmd = parse_agent_command(r#"agent --cap coord "orchestrate""#).unwrap();
        match cmd {
            AgentCommand::SpawnWithFlags { flags, .. } => {
                assert_eq!(flags.capability, Some(CapabilityClass::Coordinate));
            }
            other => panic!("expected SpawnWithFlags, got {other:?}"),
        }
    }

    #[test]
    fn parse_all_flags_combined() {
        let cmd = parse_agent_command(
            r#"agent --model openai:gpt-4o --role actor --ttl 600 --capability act "ship it""#,
        )
        .unwrap();
        match cmd {
            AgentCommand::SpawnWithFlags { prompt, flags } => {
                assert_eq!(prompt, "ship it");
                assert_eq!(flags.model, Some(ChatModel::OpenAi("gpt-4o".into())));
                assert_eq!(flags.role, Some(AgentRole::Actor));
                assert_eq!(flags.ttl_secs, Some(600));
                assert_eq!(flags.capability, Some(CapabilityClass::Act));
            }
            other => panic!("expected SpawnWithFlags, got {other:?}"),
        }
    }

    #[test]
    fn parse_flag_without_prompt_returns_none() {
        assert!(parse_agent_command("agent --model claude").is_none());
    }

    #[test]
    fn execute_spawn_with_flags_reports_all_options() {
        let mut mgr = AgentManager::new(4);
        let cmd = AgentCommand::SpawnWithFlags {
            prompt: "do something".into(),
            flags: SpawnFlags {
                model: Some(ChatModel::Claude("claude-opus-4-7".into())),
                role: Some(AgentRole::Actor),
                ttl_secs: Some(120),
                capability: Some(CapabilityClass::Act),
                warnings: vec![],
            },
        };
        let output = execute_agent_command(&cmd, &mut mgr);
        assert!(output[0].contains("spawned agent #1"));
        assert!(output.iter().any(|l| l.contains("claude:claude-opus-4-7")));
        assert!(output.iter().any(|l| l.contains("Actor")));
        assert!(output.iter().any(|l| l.contains("120s")));
        assert!(output.iter().any(|l| l.contains("act")));
    }

    #[test]
    fn execute_spawn_with_flags_ttl_zero_says_unlimited() {
        let mut mgr = AgentManager::new(4);
        let cmd = AgentCommand::SpawnWithFlags {
            prompt: "run".into(),
            flags: SpawnFlags {
                ttl_secs: Some(0),
                ..Default::default()
            },
        };
        let output = execute_agent_command(&cmd, &mut mgr);
        assert!(output.iter().any(|l| l.contains("unlimited")));
    }

    #[test]
    fn help_text_includes_all_flags() {
        let mut mgr = AgentManager::new(4);
        let output = execute_agent_command(&AgentCommand::Help, &mut mgr);
        let full = output.join("\n");
        assert!(full.contains("--model"), "missing --model in help");
        assert!(full.contains("--role"), "missing --role in help");
        assert!(full.contains("--ttl"), "missing --ttl in help");
        assert!(full.contains("--capability"), "missing --capability in help");
        assert!(full.contains("--cap"), "missing --cap alias in help");
    }

    // =========================================================================
    // NEW TESTS for the three bug fixes
    // =========================================================================

    // -- Fix 1: Flags after prompt are not silently dropped --------------------

    #[test]
    fn flags_after_prompt_are_parsed() {
        // `--role` appears after the quoted prompt; it must not be silently dropped.
        let cmd = parse_agent_command(r#"agent "my prompt" --role actor"#).unwrap();
        match cmd {
            AgentCommand::SpawnWithFlags { prompt, flags } => {
                assert_eq!(prompt, "my prompt");
                assert_eq!(flags.role, Some(AgentRole::Actor));
                assert!(flags.warnings.is_empty(), "unexpected warnings: {:?}", flags.warnings);
            }
            other => panic!("expected SpawnWithFlags, got {other:?}"),
        }
    }

    #[test]
    fn flags_interleaved_with_prompt_words() {
        // Prompt tokens appear between flags; all should be reconstructed correctly.
        let cmd =
            parse_agent_command(r#"agent --ttl 60 "deploy staging" --role actor"#).unwrap();
        match cmd {
            AgentCommand::SpawnWithFlags { prompt, flags } => {
                assert_eq!(prompt, "deploy staging");
                assert_eq!(flags.ttl_secs, Some(60));
                assert_eq!(flags.role, Some(AgentRole::Actor));
            }
            other => panic!("expected SpawnWithFlags, got {other:?}"),
        }
    }

    #[test]
    fn prompt_only_after_flags_still_works() {
        // Baseline: flags before prompt continues to work as before.
        let cmd = parse_agent_command(r#"agent --model claude "do the thing""#).unwrap();
        match cmd {
            AgentCommand::SpawnWithFlags { prompt, flags } => {
                assert_eq!(prompt, "do the thing");
                assert_eq!(flags.model, Some(ChatModel::default_claude()));
            }
            other => panic!("expected SpawnWithFlags, got {other:?}"),
        }
    }

    // -- Fix 2: tokenise closes on matching quote char only --------------------

    #[test]
    fn tokenise_double_quote_containing_single_quote() {
        // `"she said 'hello'"` — the single-quote inside must NOT close the token.
        let tokens = tokenise(r#"--role actor "she said 'hello'""#);
        assert_eq!(tokens, vec!["--role", "actor", "she said 'hello'"]);
    }

    #[test]
    fn tokenise_single_quote_containing_double_quote() {
        // `'say "world"'` — the double-quote inside must NOT close the token.
        let tokens = tokenise(r#"--role actor 'say "world"'"#);
        assert_eq!(tokens, vec!["--role", "actor", r#"say "world""#]);
    }

    #[test]
    fn parse_agent_with_mixed_quotes_in_prompt() {
        // End-to-end: double-quoted prompt containing a single-quote apostrophe.
        let cmd = parse_agent_command(r#"agent --role actor "she said 'hello'""#).unwrap();
        match cmd {
            AgentCommand::SpawnWithFlags { prompt, flags } => {
                assert_eq!(prompt, "she said 'hello'");
                assert_eq!(flags.role, Some(AgentRole::Actor));
            }
            other => panic!("expected SpawnWithFlags, got {other:?}"),
        }
    }

    // -- Fix 3: Invalid flag values produce warnings, not silent drops ---------

    #[test]
    fn invalid_role_produces_warning_not_silent_drop() {
        let cmd = parse_agent_command(r#"agent --role badval "do it""#).unwrap();
        match cmd {
            AgentCommand::SpawnWithFlags { flags, .. } => {
                assert!(
                    flags.role.is_none(),
                    "role should be None for unknown value"
                );
                assert!(
                    !flags.warnings.is_empty(),
                    "expected a warning about unknown role"
                );
                let warn = flags.warnings.join("\n");
                assert!(
                    warn.contains("badval"),
                    "warning should mention the bad value"
                );
                assert!(
                    warn.contains("actor"),
                    "warning should list valid roles (e.g. 'actor')"
                );
            }
            other => panic!("expected SpawnWithFlags, got {other:?}"),
        }
    }

    #[test]
    fn invalid_role_warning_appears_in_execute_output() {
        let mut mgr = AgentManager::new(4);
        let cmd = parse_agent_command(r#"agent --role oops "do it""#).unwrap();
        let output = execute_agent_command(&cmd, &mut mgr);
        let full = output.join("\n");
        assert!(
            full.contains("warning"),
            "execute output should contain 'warning'"
        );
        assert!(
            full.contains("oops"),
            "warning should mention the invalid value"
        );
    }

    #[test]
    fn invalid_ttl_produces_warning_not_silent_drop() {
        let cmd = parse_agent_command(r#"agent --ttl notanumber "run""#).unwrap();
        match cmd {
            AgentCommand::SpawnWithFlags { flags, .. } => {
                assert!(
                    flags.ttl_secs.is_none(),
                    "ttl_secs should be None for invalid input"
                );
                assert!(
                    !flags.warnings.is_empty(),
                    "expected a warning about invalid TTL"
                );
                let warn = flags.warnings.join("\n");
                assert!(
                    warn.contains("notanumber"),
                    "warning should mention the bad value"
                );
            }
            other => panic!("expected SpawnWithFlags, got {other:?}"),
        }
    }

    #[test]
    fn invalid_ttl_warning_appears_in_execute_output() {
        let mut mgr = AgentManager::new(4);
        let cmd = parse_agent_command(r#"agent --ttl xyz "run""#).unwrap();
        let output = execute_agent_command(&cmd, &mut mgr);
        let full = output.join("\n");
        assert!(full.contains("warning"));
        assert!(full.contains("xyz"));
    }

    // -- Duplicate flags -------------------------------------------------------

    #[test]
    fn duplicate_role_flag_last_value_wins_with_warning() {
        // `--role actor --role defender` — defender wins, warning emitted.
        let cmd =
            parse_agent_command(r#"agent --role actor --role defender "task""#).unwrap();
        match cmd {
            AgentCommand::SpawnWithFlags { flags, .. } => {
                assert_eq!(
                    flags.role,
                    Some(AgentRole::Defender),
                    "last --role should win"
                );
                let warn = flags.warnings.join("\n");
                assert!(
                    warn.contains("duplicate"),
                    "expected duplicate-flag warning, got: {warn}"
                );
            }
            other => panic!("expected SpawnWithFlags, got {other:?}"),
        }
    }

    #[test]
    fn duplicate_ttl_flag_last_value_wins_with_warning() {
        let cmd = parse_agent_command(r#"agent --ttl 100 --ttl 200 "task""#).unwrap();
        match cmd {
            AgentCommand::SpawnWithFlags { flags, .. } => {
                assert_eq!(flags.ttl_secs, Some(200), "last --ttl should win");
                let warn = flags.warnings.join("\n");
                assert!(warn.contains("duplicate"));
            }
            other => panic!("expected SpawnWithFlags, got {other:?}"),
        }
    }

    #[test]
    fn duplicate_model_flag_last_value_wins_with_warning() {
        let cmd =
            parse_agent_command(r#"agent --model claude --model openai "task""#).unwrap();
        match cmd {
            AgentCommand::SpawnWithFlags { flags, .. } => {
                assert_eq!(flags.model, Some(ChatModel::default_openai()));
                let warn = flags.warnings.join("\n");
                assert!(warn.contains("duplicate"));
            }
            other => panic!("expected SpawnWithFlags, got {other:?}"),
        }
    }

    #[test]
    fn duplicate_capability_flag_last_value_wins_with_warning() {
        let cmd =
            parse_agent_command(r#"agent --cap act --capability sense "task""#).unwrap();
        match cmd {
            AgentCommand::SpawnWithFlags { flags, .. } => {
                assert_eq!(flags.capability, Some(CapabilityClass::Sense));
                let warn = flags.warnings.join("\n");
                assert!(warn.contains("duplicate"));
            }
            other => panic!("expected SpawnWithFlags, got {other:?}"),
        }
    }

    // =========================================================================
    // #165 — QA: Agent model flag — --model flag routes to the correct model
    // =========================================================================

    /// `--model claude:claude-opus-4-7` must parse to
    /// `Claude("claude-opus-4-7")` and backend name must be `"claude"`.
    #[test]
    fn model_flag_claude_opus_4_7_resolves_to_correct_model_id() {
        let cmd = parse_agent_command(r#"agent --model claude:claude-opus-4-7 "task""#)
            .expect("parse must succeed");

        let flags = match cmd {
            AgentCommand::SpawnWithFlags { ref flags, .. } => flags.clone(),
            other => panic!("expected SpawnWithFlags, got {other:?}"),
        };

        let model = flags.model.expect("--model flag must be parsed");
        match &model {
            ChatModel::Claude(id) => assert_eq!(
                id, "claude-opus-4-7",
                "model id must be exactly 'claude-opus-4-7', got '{id}'"
            ),
            other => panic!("expected Claude variant, got {other:?}"),
        }
        assert_eq!(model.backend_name(), "claude");
    }

    /// `--model claude:claude-sonnet-4-6` must parse to
    /// `Claude("claude-sonnet-4-6")`.
    #[test]
    fn model_flag_claude_sonnet_4_6_resolves_to_correct_model_id() {
        let cmd = parse_agent_command(r#"agent --model claude:claude-sonnet-4-6 "task""#)
            .expect("parse must succeed");

        let flags = match cmd {
            AgentCommand::SpawnWithFlags { ref flags, .. } => flags.clone(),
            other => panic!("expected SpawnWithFlags, got {other:?}"),
        };

        let model = flags.model.expect("--model flag must be parsed");
        match &model {
            ChatModel::Claude(id) => assert_eq!(
                id, "claude-sonnet-4-6",
                "model id must be exactly 'claude-sonnet-4-6', got '{id}'"
            ),
            other => panic!("expected Claude variant, got {other:?}"),
        }
    }

    /// `--model claude:claude-haiku-4-5-20251001` must parse to
    /// `Claude("claude-haiku-4-5-20251001")`.
    #[test]
    fn model_flag_claude_haiku_4_5_20251001_resolves_to_correct_model_id() {
        let cmd =
            parse_agent_command(r#"agent --model claude:claude-haiku-4-5-20251001 "task""#)
                .expect("parse must succeed");

        let flags = match cmd {
            AgentCommand::SpawnWithFlags { ref flags, .. } => flags.clone(),
            other => panic!("expected SpawnWithFlags, got {other:?}"),
        };

        let model = flags.model.expect("--model flag must be parsed");
        match &model {
            ChatModel::Claude(id) => assert_eq!(
                id, "claude-haiku-4-5-20251001",
                "model id must be exactly 'claude-haiku-4-5-20251001', got '{id}'"
            ),
            other => panic!("expected Claude variant, got {other:?}"),
        }
    }

    /// An unknown model spec without a recognised backend prefix returns `None`
    /// from `ChatModel::from_env_str` — the boundary callers use to detect
    /// unrecognised inputs.
    #[test]
    fn model_flag_unknown_env_str_returns_none() {
        let unknown_inputs = [
            "notamodel",
            "",
            "totally-unknown-provider:foo",
            "??bad??",
        ];
        for input in &unknown_inputs {
            let result = ChatModel::from_env_str(input);
            assert!(
                result.is_none(),
                "from_env_str({input:?}) must return None for an unknown model spec, got {result:?}"
            );
        }
    }

    /// All three target Claude model IDs must round-trip through
    /// `AgentSpawnOpts::resolve_model()` unchanged when set explicitly.
    #[test]
    fn model_flag_all_three_claude_models_resolve_correctly() {
        use crate::agent::{AgentSpawnOpts, AgentTask};

        let cases = [
            ("claude-opus-4-7", ChatModel::Claude("claude-opus-4-7".into())),
            ("claude-sonnet-4-6", ChatModel::Claude("claude-sonnet-4-6".into())),
            (
                "claude-haiku-4-5-20251001",
                ChatModel::Claude("claude-haiku-4-5-20251001".into()),
            ),
        ];

        for (expected_id, model) in &cases {
            let task = AgentTask::FreeForm { prompt: "test".into() };
            let mut opts = AgentSpawnOpts::new(task);
            opts.chat_model = Some(model.clone());

            let resolved = opts.resolve_model();
            match &resolved {
                ChatModel::Claude(id) => assert_eq!(
                    id.as_str(),
                    *expected_id,
                    "resolve_model() must return '{expected_id}', got '{id}'"
                ),
                other => panic!(
                    "expected Claude variant for {expected_id:?}, got {other:?}"
                ),
            }
            assert_eq!(resolved.backend_name(), "claude");
        }
    }
}
