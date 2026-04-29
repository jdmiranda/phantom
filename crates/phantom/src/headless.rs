//! Headless REPL mode — the brain without the body.
//!
//! Runs Phantom with no window, no GPU, no renderer. Just the AI brain,
//! agents, semantic parser, memory, NLP interpreter, and a stdin/stdout REPL.
//! Everything except pixels.

use std::io::{self, BufRead, Write};
use std::path::Path;

use anyhow::Result;
use uuid::Uuid;

use phantom_agents::api::{ApiEvent, ClaudeConfig, send_message};
use phantom_agents::cli::{AgentCommand, execute_agent_command, parse_agent_command};
use phantom_agents::manager::AgentManager;
use phantom_agents::tools::{available_tools, execute_tool};
use phantom_agents::{AgentMessage, AgentStatus};
use phantom_app::config::PhantomConfig;
use phantom_brain::brain::{BrainConfig, spawn_brain};
use phantom_brain::events::{AiAction, AiEvent};
use phantom_context::ProjectContext;
use phantom_history::{HistoryEntry, HistoryStore};
use phantom_memory::MemoryStore;
use phantom_nlp::NlpInterpreter;
use phantom_nlp::interpreter::ResolvedAction;
use phantom_semantic::SemanticParser;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run Phantom in headless mode: no window, no GPU, just the brain + REPL.
pub fn run_headless(_config: PhantomConfig) -> Result<()> {
    // Load .env file if present.
    load_dotenv();

    let project_dir = std::env::current_dir()?.to_string_lossy().to_string();
    let context = ProjectContext::detect(Path::new(&project_dir));

    println!(
        "PHANTOM [headless] \u{2014} {} [{:?}]",
        context.name, context.project_type
    );

    // Open persistent stores.
    let memory = MemoryStore::open(&project_dir)?;
    let session_id = Uuid::new_v4();
    let mut history = HistoryStore::open(session_id)?;

    // Spawn the AI brain thread.
    let brain = spawn_brain(BrainConfig {
        project_dir: project_dir.clone(),
        enable_suggestions: true,
        enable_memory: true,
        quiet_threshold: 0.5,
        router: None,
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

    print!("[USER]: ");
    io::stdout().flush()?;

    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let input = line?.trim().to_string();
        if input.is_empty() {
            print!("[USER]: ");
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
                                    .exit_code()
                                    .map(|c| format!(" [exit {c}]"))
                                    .unwrap_or_default();
                                println!("  $ {}{code}", entry.command());
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
            print!("[USER]: ");
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
                run_chat(cfg, msg, &context, &memory, &mut chat_history, &mut agents, &project_dir, &mut tool_use_ids);
            } else {
                println!("[PHANTOM]: ANTHROPIC_API_KEY not set. Cannot chat.");
            }

            drain_brain(&brain);
            print!("[USER]: ");
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
                print!("[USER]: ");
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
                    &mut history,
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
                    &mut history,
                    &agents,
                    &claude_config,
                );
            }
        }

        drain_brain(&brain);
        print!("[USER]: ");
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
    history: &mut HistoryStore,
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
            let entry = HistoryEntry::builder(
                cmd,
                project_dir,
                Uuid::new_v4(),
            )
            .exit_code(out.status.code().unwrap_or(-1))
            .build();
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
        let Some(agent) = agents.get_mut(agent_id) else { return };
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
        let Some(agent) = agents.get(agent_id) else { break };
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
                    let Some(agent) = agents.get_mut(agent_id) else { break };
                    agent.push_message(AgentMessage::Assistant(text.clone()));
                    agent.log(&text);
                }
                Some(ApiEvent::ToolUse { id, call }) => {
                    println!("\n[AGENT #{}]: using tool {:?}", agent_id, call.tool);
                    let result = execute_tool(
                        call.tool,
                        &call.args,
                        working_dir,
                        &phantom_agents::role::AgentRole::Conversational,
                    );
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

                    let Some(agent) = agents.get_mut(agent_id) else { break };
                    agent.push_message(AgentMessage::ToolCall(call));
                    agent.push_message(AgentMessage::ToolResult(result));
                    agent.status = AgentStatus::Working;
                    got_tool_use = true;
                }
                Some(ApiEvent::Done) => {
                    println!("\n[AGENT #{}]: turn complete", agent_id);
                    if !got_tool_use {
                        if let Some(a) = agents.get_mut(agent_id) { a.complete(true); }
                    }
                    break;
                }
                Some(ApiEvent::Error(e)) => {
                    println!("\n[AGENT #{}]: error: {e}", agent_id);
                    if let Some(a) = agents.get_mut(agent_id) { a.complete(false); }
                    break;
                }
                None => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
            }
        }

        // If the agent completed or failed, stop looping.
        let Some(agent) = agents.get(agent_id) else { break };
        if agent.status != AgentStatus::Working {
            break;
        }
        // Otherwise the agent made a tool call -- loop back for the next turn.
    }

    let Some(agent) = agents.get(agent_id) else { return };
    let status_tag = match agent.status {
        AgentStatus::Done => "DONE",
        AgentStatus::Failed => "FAILED",
        AgentStatus::Flatline => "FLATLINE",
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
            AiAction::SpawnAgent { task, spawn_tag } => {
                println!("[BRAIN]: suggested agent: {task:?} (spawn_tag={spawn_tag:?})");
            }
            AiAction::ConsoleReply(reply) => {
                println!("[PHANTOM]: {reply}");
            }
            AiAction::DismissAdapter { app_id } => {
                println!("[BRAIN]: dismiss adapter {app_id}");
            }
            AiAction::AgentFlatlined { id, reason } => {
                println!("[BRAIN]: agent {id} flatlined: {reason}");
            }
            AiAction::Suggest { action, rationale, confidence } => {
                println!("[BRAIN]: proactive suggestion ({confidence:.2}): {action} — {rationale}");
            }
            AiAction::QuarantineAgent { agent_id, denial_count } => {
                println!("[BRAIN]: quarantine agent {agent_id} after {denial_count} denials");
            }
            AiAction::AgentQuarantined { agent_id, denial_count } => {
                println!("[BRAIN]: agent {agent_id} quarantined after {denial_count} denials");
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

/// Run a chat turn with Claude. Chat is the COMMANDER — it can spawn
/// constrained agents to do work but never touches files directly.
///
/// Chat gets one meta-tool: `spawn_agent`. When it needs to read files,
/// run commands, or modify code, it spawns an agent with the minimum
/// permission set for the task.
fn run_chat(
    config: &ClaudeConfig,
    user_msg: &str,
    context: &ProjectContext,
    memory: &MemoryStore,
    chat_history: &mut Vec<AgentMessage>,
    agents: &mut AgentManager,
    project_dir: &str,
    tool_use_ids: &mut Vec<String>,
) {
    use phantom_agents::agent::{Agent, AgentTask};
    // Build system prompt with project context + memory on first message.
    if chat_history.is_empty() {
        let system = format!(
            "You are Phantom, an AI-native terminal assistant running in headless mode.\n\
            You are the COMMANDER. You do NOT have direct file or command access.\n\
            Instead, you have a `spawn_agent` tool to delegate work to sandboxed agents.\n\n\
            When the user asks you to read files, run commands, or modify code, spawn an agent \
            with the MINIMUM permissions needed:\n\
            - Reading files: permissions = [\"ReadFiles\"]\n\
            - Running commands: permissions = [\"RunCommands\"]\n\
            - Writing/editing files: permissions = [\"ReadFiles\", \"WriteFiles\"]\n\
            - Full dev work: permissions = [\"ReadFiles\", \"WriteFiles\", \"RunCommands\", \"GitAccess\"]\n\n\
            Be concise, technical, and helpful. Show the agent's output to the user.\n\n\
            PROJECT CONTEXT:\n{}\n\n\
            PROJECT MEMORY:\n{}",
            context.agent_context(),
            memory.agent_context(),
        );
        chat_history.push(AgentMessage::System(system));
    }

    // Add user message.
    chat_history.push(AgentMessage::User(user_msg.to_string()));

    // Build the spawn_agent tool definition.
    let spawn_tool = phantom_agents::tools::ToolDefinition {
        name: "spawn_agent".to_string(),
        description: "Spawn a sandboxed agent to perform work. The agent has tools (ReadFile, WriteFile, RunCommand, etc) constrained by the permissions you specify. Use minimum permissions needed.".to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "What the agent should do"
                },
                "permissions": {
                    "type": "array",
                    "items": { "type": "string", "enum": ["ReadFiles", "WriteFiles", "RunCommands", "GitAccess", "Network"] },
                    "description": "Minimum permissions the agent needs"
                }
            },
            "required": ["task", "permissions"]
        }),
    };

    // Build temp agent with chat history.
    let mut temp_agent = Agent::new(9999, AgentTask::FreeForm {
        prompt: user_msg.to_string(),
    });
    for msg in chat_history.iter() {
        temp_agent.push_message(msg.clone());
    }

    // Send to Claude with the spawn_agent tool.
    let mut handle = send_message(config, &temp_agent, &[spawn_tool], tool_use_ids);

    // Stream the response, handle tool calls.
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
            Some(ApiEvent::ToolUse { id, call }) => {
                // Chat wants to spawn an agent.
                let task_str = call.args.get("task")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown task")
                    .to_string();

                let perms: Vec<String> = call.args.get("permissions")
                    .and_then(|v| v.as_array())
                    .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                    .unwrap_or_default();

                println!("\n[PHANTOM]: Spawning agent: \"{task_str}\" [perms: {}]", perms.join(", "));

                // Spawn the agent.
                let agent_task = AgentTask::FreeForm { prompt: task_str.clone() };
                let agent_id = agents.spawn(agent_task);

                // Drive the agent with full tools (filtered by permissions).
                let mut agent_output = String::new();
                tool_use_ids.clear();
                if let Some(agent) = agents.get_mut(agent_id) {
                    let sys = agent.system_prompt();
                    agent.push_message(AgentMessage::System(sys));
                    agent.push_message(AgentMessage::User(task_str));
                }

                // Run the agent loop.
                run_agent_loop(agents, config, project_dir, tool_use_ids);

                // Collect agent output.
                if let Some(agent) = agents.get(agent_id) {
                    for line in &agent.output_log {
                        agent_output.push_str(line);
                        agent_output.push('\n');
                    }
                    // Also get the last assistant message.
                    for msg in agent.messages.iter().rev() {
                        if let AgentMessage::Assistant(text) = msg {
                            if agent_output.is_empty() {
                                agent_output = text.clone();
                            }
                            break;
                        }
                    }
                }

                if agent_output.is_empty() {
                    agent_output = "Agent completed with no output.".to_string();
                }

                // Feed agent result back to chat as a tool result.
                let _ = id; // tool_use_id tracked for multi-turn
                chat_history.push(AgentMessage::ToolCall(call));
                chat_history.push(AgentMessage::ToolResult(phantom_agents::ToolResult {
                    tool: phantom_agents::ToolType::ReadFile, // placeholder type
                    success: true,
                    output: agent_output.clone(),
                    ..Default::default()
                }));

                println!("[AGENT RESULT]: {}", &agent_output[..agent_output.len().min(500)]);

                // Continue the chat conversation with the tool result.
                // Rebuild and resend.
                let mut followup_agent = Agent::new(9999, AgentTask::FreeForm {
                    prompt: user_msg.to_string(),
                });
                for msg in chat_history.iter() {
                    followup_agent.push_message(msg.clone());
                }

                let spawn_tool_again = phantom_agents::tools::ToolDefinition {
                    name: "spawn_agent".to_string(),
                    description: "Spawn a sandboxed agent to perform work.".to_string(),
                    parameters: serde_json::json!({"type": "object", "properties": {"task": {"type": "string"}, "permissions": {"type": "array", "items": {"type": "string"}}}, "required": ["task", "permissions"]}),
                };

                handle = send_message(config, &followup_agent, &[spawn_tool_again], tool_use_ids);
                print!("[PHANTOM]: ");
                io::stdout().flush().ok();
            }
            Some(ApiEvent::Done) => {
                println!();
                break;
            }
            Some(ApiEvent::Error(e)) => {
                println!("\n[ERROR]: {e}");
                break;
            }
            None => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }

    // Save final response to chat history.
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

/// Load a `.env` file from the current directory if it exists.
fn load_dotenv() {
    let path = std::path::Path::new(".env");
    if !path.exists() {
        return;
    }
    if let Ok(contents) = std::fs::read_to_string(path) {
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((key, value)) = line.split_once('=') {
                let key = key.trim();
                let value = value.trim();
                if std::env::var(key).is_err() {
                    // SAFETY: single-threaded at this point (before brain spawn).
                    unsafe { std::env::set_var(key, value); }
                }
            }
        }
    }
}
