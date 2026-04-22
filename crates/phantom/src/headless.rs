//! Headless REPL mode — the brain without the body.
//!
//! Runs Phantom with no window, no GPU, no renderer. Just the AI brain,
//! agents, semantic parser, memory, NLP interpreter, and a stdin/stdout REPL.
//! Everything except pixels.

use std::io::{self, BufRead, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;

use phantom_agents::api::{ApiEvent, ClaudeConfig, send_message};
use phantom_agents::cli::{AgentCommand, execute_agent_command, parse_agent_command};
use phantom_agents::manager::AgentManager;
use phantom_agents::tools::{available_tools, execute_tool};
use phantom_agents::{AgentMessage, AgentStatus};
use phantom_app::config::PhantomConfig;
use phantom_brain::brain::{BrainConfig, spawn_brain};
use phantom_brain::events::{AiAction, AiEvent};
use phantom_context::ProjectContext;
use phantom_history::HistoryStore;
use phantom_history::store::HistoryEntry;
use phantom_memory::MemoryStore;
use phantom_nlp::NlpInterpreter;
use phantom_nlp::interpreter::ResolvedAction;
use phantom_semantic::SemanticParser;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run Phantom in headless mode: no window, no GPU, just the brain + REPL.
pub fn run_headless(_config: PhantomConfig) -> Result<()> {
    let project_dir = std::env::current_dir()?.to_string_lossy().to_string();
    let context = ProjectContext::detect(Path::new(&project_dir));

    println!(
        "PHANTOM [headless] \u{2014} {} [{:?}]",
        context.name, context.project_type
    );

    // Open persistent stores.
    let memory = MemoryStore::open(&project_dir)?;
    let history = HistoryStore::open(&project_dir)?;

    // Spawn the AI brain thread.
    let brain = spawn_brain(BrainConfig {
        project_dir: project_dir.clone(),
        enable_suggestions: true,
        enable_memory: true,
        quiet_threshold: 0.5,
    });

    // Agent manager (max 5 concurrent agents).
    let mut agents = AgentManager::new(5);

    // Claude API config (optional -- agents work without it, just no reasoning).
    let claude_config = ClaudeConfig::from_env();
    if claude_config.is_none() {
        println!("(ANTHROPIC_API_KEY not set \u{2014} agents and chat will not work)");
    }

    // Collect tool_use IDs across the agent loop for multi-turn conversations.
    let mut tool_use_ids: Vec<String> = Vec::new();

    // Chat conversation buffer — persists across messages within the session.
    let mut chat_history: Vec<AgentMessage> = Vec::new();

    print!("> ");
    io::stdout().flush()?;

    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let input = line?.trim().to_string();
        if input.is_empty() {
            print!("> ");
            io::stdout().flush()?;
            continue;
        }

        // ------------------------------------------------------------------
        // Built-in commands
        // ------------------------------------------------------------------
        let mut handled = true;
        match input.as_str() {
            "quit" | "exit" => break,
            "status" => print_status(&context, &agents, &memory),
            "context" => println!("{}", context.agent_context()),
            "memory" => println!("{}", memory.agent_context()),
            "history" => {
                match history.recent(20) {
                    Ok(entries) => {
                        if entries.is_empty() {
                            println!("No history yet.");
                        } else {
                            for entry in &entries {
                                let code = entry
                                    .parsed
                                    .exit_code
                                    .map(|c| format!(" [exit {c}]"))
                                    .unwrap_or_default();
                                println!("  $ {}{code}", entry.parsed.command);
                            }
                        }
                    }
                    Err(e) => println!("Error reading history: {e}"),
                }
            }
            "render" => {
                println!("Opening GUI window...");
                println!("(not yet implemented \u{2014} run `cargo run --bin phantom` in another terminal)");
            }
            "help" => print_help(),
            _ => handled = false,
        }

        if handled {
            drain_brain(&brain);
            print!("> ");
            io::stdout().flush()?;
            continue;
        }

        // ------------------------------------------------------------------
        // Chat with AI (session-persistent conversation)
        // ------------------------------------------------------------------
        if input.starts_with("chat ") || input == "chat" {
            let msg = if input == "chat" {
                "What can you help me with?"
            } else {
                &input[5..]
            };

            if let Some(ref cfg) = claude_config {
                run_chat(cfg, msg, &context, &memory, &mut chat_history);
            } else {
                println!("[PHANTOM]: ANTHROPIC_API_KEY not set. Cannot chat.");
            }

            drain_brain(&brain);
            print!("> ");
            io::stdout().flush()?;
            continue;
        }

        // ------------------------------------------------------------------
        // Agent commands
        // ------------------------------------------------------------------
        if input.starts_with("agent") || input == "agents" {
            if let Some(cmd) = parse_agent_command(&input) {
                let output = execute_agent_command(&cmd, &mut agents);
                for line in &output {
                    println!("{line}");
                }

                // If we just spawned an agent and have an API key, drive it.
                let is_spawn = matches!(
                    cmd,
                    AgentCommand::Spawn { .. }
                        | AgentCommand::SpawnFix { .. }
                        | AgentCommand::SpawnReview
                        | AgentCommand::SpawnWatch { .. }
                );
                if is_spawn {
                    if let Some(ref cfg) = claude_config {
                        tool_use_ids.clear();
                        run_agent_loop(&mut agents, cfg, &project_dir, &mut tool_use_ids);
                    } else {
                        println!("Warning: ANTHROPIC_API_KEY not set. Agent spawned but cannot reason.");
                    }
                }

                drain_brain(&brain);
                print!("> ");
                io::stdout().flush()?;
                continue;
            }
        }

        // ------------------------------------------------------------------
        // NLP interpretation
        // ------------------------------------------------------------------
        let action = NlpInterpreter::interpret(&input, &context);
        match action {
            ResolvedAction::RunCommand(cmd) => {
                run_shell_command(
                    &cmd,
                    &project_dir,
                    &brain,
                    &history,
                    &agents,
                    &claude_config,
                );
            }
            ResolvedAction::SpawnAgent(prompt) => {
                println!("[PHANTOM]: Spawning agent: {prompt}");
                let synthetic = format!("agent \"{prompt}\"");
                if let Some(cmd) = parse_agent_command(&synthetic) {
                    let output = execute_agent_command(&cmd, &mut agents);
                    for line in &output {
                        println!("{line}");
                    }
                    if let Some(ref cfg) = claude_config {
                        tool_use_ids.clear();
                        run_agent_loop(&mut agents, cfg, &project_dir, &mut tool_use_ids);
                    }
                }
            }
            ResolvedAction::ShowInfo(info) => {
                println!("{info}");
            }
            ResolvedAction::Ambiguous { input: inp, options } => {
                println!("Ambiguous: {inp}");
                for opt in &options {
                    println!("  - {opt}");
                }
            }
            ResolvedAction::PassThrough => {
                run_shell_command(
                    &input,
                    &project_dir,
                    &brain,
                    &history,
                    &agents,
                    &claude_config,
                );
            }
        }

        drain_brain(&brain);
        print!("> ");
        io::stdout().flush()?;
    }

    let _ = brain.send_event(AiEvent::Shutdown);
    println!("\n[PHANTOM]: Shutdown.");
    Ok(())
}

// ---------------------------------------------------------------------------
// Shell command execution
// ---------------------------------------------------------------------------

/// Execute a shell command, parse the output semantically, feed the brain,
/// offer agent suggestions, and record to history.
fn run_shell_command(
    cmd: &str,
    project_dir: &str,
    brain: &phantom_brain::brain::BrainHandle,
    history: &HistoryStore,
    _agents: &AgentManager,
    _claude_config: &Option<ClaudeConfig>,
) {
    println!("$ {cmd}");

    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(project_dir)
        .output();

    match output {
        Ok(out) => {
            let stdout_str = String::from_utf8_lossy(&out.stdout);
            let stderr_str = String::from_utf8_lossy(&out.stderr);

            if !stdout_str.is_empty() {
                print!("{stdout_str}");
            }
            if !stderr_str.is_empty() {
                eprint!("{stderr_str}");
            }

            // Semantic parse.
            let parsed = SemanticParser::parse(cmd, &stdout_str, &stderr_str, out.status.code());

            // Feed the brain.
            let _ = brain.send_event(AiEvent::CommandComplete(parsed.clone()));

            // Offer agent suggestion on errors.
            if let Some(suggestion) = phantom_agents::suggest::suggest(&parsed, project_dir) {
                println!("\n{}", suggestion.prompt_text);
                for opt in &suggestion.options {
                    print!("  [{}] {}  ", opt.key, opt.label);
                }
                println!();
            }

            // Append to history.
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);

            let entry = HistoryEntry {
                timestamp,
                working_dir: project_dir.to_string(),
                parsed,
            };
            if let Err(e) = history.append(&entry) {
                log::warn!("failed to append history: {e}");
            }
        }
        Err(e) => println!("Error: {e}"),
    }
}

// ---------------------------------------------------------------------------
// Agent execution loop
// ---------------------------------------------------------------------------

/// Drive an agent through the Claude API tool-use loop until completion.
///
/// Finds the most recently spawned working agent and runs it synchronously.
/// Each turn: send conversation to Claude, process the response (text or
/// tool use), loop back if a tool was called, stop on Done/Error.
fn run_agent_loop(
    agents: &mut AgentManager,
    config: &ClaudeConfig,
    working_dir: &str,
    tool_use_ids: &mut Vec<String>,
) {
    let working: Vec<_> = agents.by_status(AgentStatus::Working);
    let Some(agent) = working.last() else {
        return;
    };
    let agent_id = agent.id;

    // Ensure the agent has a system prompt and initial user message.
    {
        let agent = agents.get_mut(agent_id).unwrap();
        if agent.messages.is_empty() {
            let sys = agent.system_prompt();
            agent.push_message(AgentMessage::System(sys));

            let task_prompt = match &agent.task {
                phantom_agents::AgentTask::FreeForm { prompt } => prompt.clone(),
                phantom_agents::AgentTask::FixError {
                    error_summary,
                    context,
                    ..
                } => format!("{error_summary}\n\nContext:\n{context}"),
                phantom_agents::AgentTask::RunCommand { command } => {
                    format!("Run: {command}")
                }
                phantom_agents::AgentTask::ReviewCode { context, .. } => context.clone(),
                phantom_agents::AgentTask::WatchAndNotify { description } => description.clone(),
            };
            agent.push_message(AgentMessage::User(task_prompt));
        }
    }

    let tools = available_tools();

    loop {
        let agent = agents.get(agent_id).unwrap();
        if agent.status != AgentStatus::Working && agent.status != AgentStatus::WaitingForTool {
            break;
        }

        // Send to Claude API.
        let mut handle = send_message(config, agent, &tools, tool_use_ids);

        println!("[AGENT #{}]: thinking...", agent_id);

        let mut got_tool_use = false;

        // Poll for events from the API.
        loop {
            match handle.try_recv() {
                Some(ApiEvent::TextDelta(text)) => {
                    print!("{text}");
                    io::stdout().flush().ok();
                    let agent = agents.get_mut(agent_id).unwrap();
                    agent.push_message(AgentMessage::Assistant(text.clone()));
                    agent.log(&text);
                }
                Some(ApiEvent::ToolUse { id, call }) => {
                    println!("\n[AGENT #{}]: using tool {:?}", agent_id, call.tool);
                    let result = execute_tool(call.tool, &call.args, working_dir);
                    let success_label = if result.success { "ok" } else { "failed" };
                    println!("[TOOL]: {success_label}");

                    // Truncate tool output for display.
                    let display_output: String = if result.output.len() > 200 {
                        format!("{}...", &result.output[..200])
                    } else {
                        result.output.clone()
                    };
                    if !display_output.is_empty() {
                        println!("[TOOL output]: {display_output}");
                    }

                    tool_use_ids.push(id);

                    let agent = agents.get_mut(agent_id).unwrap();
                    agent.push_message(AgentMessage::ToolCall(call));
                    agent.push_message(AgentMessage::ToolResult(result));
                    agent.status = AgentStatus::Working;
                    got_tool_use = true;
                }
                Some(ApiEvent::Done) => {
                    println!("\n[AGENT #{}]: turn complete", agent_id);
                    if !got_tool_use {
                        // No tool call -- the agent is done reasoning.
                        agents.get_mut(agent_id).unwrap().complete(true);
                    }
                    break;
                }
                Some(ApiEvent::Error(e)) => {
                    println!("\n[AGENT #{}]: error: {e}", agent_id);
                    agents.get_mut(agent_id).unwrap().complete(false);
                    break;
                }
                None => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
            }
        }

        // If the agent completed or failed, stop looping.
        let agent = agents.get(agent_id).unwrap();
        if agent.status != AgentStatus::Working {
            break;
        }
        // Otherwise the agent made a tool call -- loop back for the next turn.
    }

    let agent = agents.get(agent_id).unwrap();
    let status_tag = match agent.status {
        AgentStatus::Done => "DONE",
        AgentStatus::Failed => "FAILED",
        _ => "ACTIVE",
    };
    println!("[AGENT #{}]: {status_tag}", agent_id);
}

// ---------------------------------------------------------------------------
// Brain drain — print any pending brain actions
// ---------------------------------------------------------------------------

fn drain_brain(brain: &phantom_brain::brain::BrainHandle) {
    while let Some(action) = brain.try_recv_action() {
        match action {
            AiAction::ShowSuggestion { text, .. } => println!("[BRAIN]: {text}"),
            AiAction::ShowNotification(n) => println!("[PHANTOM]: {n}"),
            AiAction::UpdateMemory { key, value } => {
                println!("[MEMORY]: {key} = {value}");
            }
            AiAction::RunCommand(cmd) => {
                println!("[BRAIN]: suggested command: {cmd}");
            }
            AiAction::SpawnAgent(task) => {
                println!("[BRAIN]: suggested agent: {task:?}");
            }
            AiAction::DoNothing => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Status + Help
// ---------------------------------------------------------------------------

fn print_status(context: &ProjectContext, agents: &AgentManager, memory: &MemoryStore) {
    println!("PHANTOM STATUS");
    println!(
        "  project:  {} [{:?}]",
        context.name, context.project_type
    );
    println!(
        "  branch:   {}",
        context
            .git
            .as_ref()
            .map(|g| g.branch.as_str())
            .unwrap_or("n/a")
    );
    println!(
        "  agents:   {} active, {} total",
        agents.active_count(),
        agents.agents().len()
    );
    println!("  memory:   {} entries", memory.count());
}

// ---------------------------------------------------------------------------
// Chat with AI
// ---------------------------------------------------------------------------

/// Run a chat turn with Claude, maintaining conversation history across the session.
fn run_chat(
    config: &ClaudeConfig,
    user_msg: &str,
    context: &ProjectContext,
    memory: &MemoryStore,
    chat_history: &mut Vec<AgentMessage>,
) {
    // Build system prompt with project context + memory on first message.
    if chat_history.is_empty() {
        let system = format!(
            "You are Phantom, an AI-native terminal assistant. You're running in headless mode \
            inside the user's terminal. Be concise, technical, and helpful.\n\n\
            PROJECT CONTEXT:\n{}\n\n\
            PROJECT MEMORY:\n{}",
            context.agent_context(),
            memory.agent_context(),
        );
        chat_history.push(AgentMessage::System(system));
    }

    // Add user message.
    chat_history.push(AgentMessage::User(user_msg.to_string()));

    // Build a temporary agent to send the conversation.
    use phantom_agents::agent::{Agent, AgentTask};
    let mut temp_agent = Agent::new(9999, AgentTask::FreeForm {
        prompt: user_msg.to_string(),
    });

    // Copy chat history into the agent's messages.
    for msg in chat_history.iter() {
        temp_agent.push_message(msg.clone());
    }

    // Send to Claude (no tools — this is just chat, not agent work).
    let mut handle = send_message(config, &temp_agent, &[], &[]);

    // Stream the response.
    let mut full_response = String::new();
    print!("[PHANTOM]: ");
    io::stdout().flush().ok();

    loop {
        match handle.try_recv() {
            Some(ApiEvent::TextDelta(text)) => {
                print!("{text}");
                io::stdout().flush().ok();
                full_response.push_str(&text);
            }
            Some(ApiEvent::Done) => {
                println!();
                break;
            }
            Some(ApiEvent::Error(e)) => {
                println!("\n[ERROR]: {e}");
                break;
            }
            Some(_) => {}
            None => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }

    // Save assistant response to chat history for context in next turn.
    if !full_response.is_empty() {
        chat_history.push(AgentMessage::Assistant(full_response));
    }
}

// ---------------------------------------------------------------------------
// Help
// ---------------------------------------------------------------------------

fn print_help() {
    println!("PHANTOM HEADLESS MODE");
    println!();
    println!("Commands:");
    println!("  chat <message>    Talk to AI (session context preserved)");
    println!("  <any command>     Run in shell (semantically parsed)");
    println!("  agent \"prompt\"    Spawn an AI agent with tools");
    println!("  agents            List running agents");
    println!("  status            Show project/agent/memory status");
    println!("  context           Show detected project context");
    println!("  memory            Show project memory");
    println!("  history           Show recent command history");
    println!("  render            Open GUI window (not yet implemented)");
    println!("  help              This message");
    println!("  quit              Exit");
    println!();
    println!("Natural language works too:");
    println!("  build             \u{2192} cargo build / npm run build / etc");
    println!("  test              \u{2192} cargo test / npm test / etc");
    println!("  what changed      \u{2192} git log --oneline -10");
    println!("  fix it            \u{2192} spawn agent to fix last error");
    println!();
    println!("Chat vs Agent:");
    println!("  chat     = conversation (no tools, remembers context)");
    println!("  agent    = worker (has tools, reads/writes files, runs commands)");
}
