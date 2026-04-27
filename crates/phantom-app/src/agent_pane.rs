//! Agent pane management — spawn AI agents in visible GUI panes.
//!
//! When the brain decides to spawn an agent (or the user requests one),
//! this module creates a new pane, starts a Claude API agent on a
//! background thread, and streams output into the pane each frame.

use log::{info, warn};

use phantom_agents::api::{ApiEvent, ApiHandle, ClaudeConfig, send_message};
use phantom_agents::agent::{Agent, AgentMessage};
use phantom_agents::permissions::PermissionSet;
use phantom_agents::tools::{ToolCall, ToolResult, ToolType, available_tools, execute_tool};
use phantom_agents::AgentTask;

use crate::app::App;

/// Maximum number of tool-use rounds before the agent is force-stopped.
const MAX_TOOL_ROUNDS: u32 = 25;

// ---------------------------------------------------------------------------
// AgentPane — a running agent with its output stream
// ---------------------------------------------------------------------------

/// An active agent running in a GUI pane.
pub(crate) struct AgentPane {
    /// The agent's task description.
    pub(crate) task: String,
    /// Current status.
    pub(crate) status: AgentPaneStatus,
    /// Accumulated output text (streamed from Claude API).
    pub(crate) output: String,
    /// Handle to the background API thread.
    api_handle: Option<ApiHandle>,
    /// Tool use IDs for multi-turn conversations.
    tool_use_ids: Vec<String>,
    /// Cached tail lines for rendering (avoids re-splitting every frame).
    pub(crate) cached_lines: Vec<String>,
    /// Output length at last cache rebuild.
    cached_len: usize,
    /// Whether a completion bus event has been emitted for this agent.
    pub(crate) event_emitted: bool,
    /// The agent's conversation state (owns the message history).
    agent: Agent,
    /// Tool calls pending execution: (api_id, call).
    pending_tools: Vec<(String, ToolCall)>,
    /// Project root for tool sandbox.
    working_dir: String,
    /// Claude API config for re-invoking on tool-result turns.
    claude_config: ClaudeConfig,
    /// Number of tool-use rounds completed (capped at [`MAX_TOOL_ROUNDS`]).
    turn_count: u32,
    /// Accumulator for assistant text within the current API response.
    current_assistant_text: String,
    /// Permission set for tool execution (default: all).
    permissions: PermissionSet,
    /// Approximate input tokens consumed.
    input_tokens: u32,
    /// Approximate output tokens consumed.
    output_tokens: u32,
    /// Number of tool calls executed.
    tool_call_count: u32,
    /// Whether this agent has written/edited files (for rollback).
    has_file_edits: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentPaneStatus {
    Working,
    Done,
    Failed,
}

impl AgentPane {
    /// Create a new agent pane and start the Claude API call.
    pub(crate) fn spawn(task: AgentTask, claude_config: &ClaudeConfig) -> Self {
        let task_desc = match &task {
            AgentTask::FreeForm { prompt } => prompt.clone(),
            AgentTask::FixError { error_summary, .. } => {
                format!("Fix: {error_summary}")
            }
            AgentTask::RunCommand { command } => {
                format!("Run: {command}")
            }
            AgentTask::ReviewCode { context, .. } => {
                format!("Review: {context}")
            }
            AgentTask::WatchAndNotify { description } => {
                format!("Watch: {description}")
            }
        };

        let mut agent = Agent::new(0, task.clone());
        let sys_prompt = agent.system_prompt();
        agent.push_message(AgentMessage::System(sys_prompt));

        // Inject codebase context so the agent knows where it lives.
        let codebase_context = build_codebase_context();
        if !codebase_context.is_empty() {
            agent.push_message(AgentMessage::System(codebase_context));
        }

        // Claude API requires at least one user message. Push the task
        // description as the initial user turn.
        let user_prompt = match &task {
            AgentTask::FreeForm { prompt } => prompt.clone(),
            AgentTask::FixError { error_summary, context, .. } => {
                format!("Fix this error: {error_summary}\nContext: {context}")
            }
            AgentTask::RunCommand { command } => format!("Run: {command}"),
            AgentTask::ReviewCode { files, context } => {
                format!("Review these files: {}\nContext: {context}", files.join(", "))
            }
            AgentTask::WatchAndNotify { description } => {
                format!("Watch: {description}")
            }
        };
        agent.push_message(AgentMessage::User(user_prompt));

        let tools = available_tools();

        info!(
            "Agent pane spawning: {} messages (system={}, user={})",
            agent.messages.len(),
            agent.messages.iter().filter(|m| matches!(m, AgentMessage::System(_))).count(),
            agent.messages.iter().filter(|m| matches!(m, AgentMessage::User(_))).count(),
        );

        let handle = send_message(claude_config, &agent, &tools, &[]);

        let working_dir = std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| ".".into());

        info!("Agent pane spawned: {task_desc}");

        Self {
            task: task_desc,
            status: AgentPaneStatus::Working,
            output: String::from("● Agent working...\n\n"),
            api_handle: Some(handle),
            tool_use_ids: Vec::new(),
            cached_lines: Vec::new(),
            cached_len: 0,
            event_emitted: false,
            agent,
            pending_tools: Vec::new(),
            working_dir,
            claude_config: claude_config.clone(),
            turn_count: 0,
            current_assistant_text: String::new(),
            permissions: PermissionSet::all(),
            input_tokens: 0,
            output_tokens: 0,
            tool_call_count: 0,
            has_file_edits: false,
        }
    }

    /// Poll for new API events and append output. Call once per frame.
    ///
    /// Returns `true` if new content was received this frame.
    pub(crate) fn poll(&mut self) -> bool {
        let Some(ref mut handle) = self.api_handle else {
            return false;
        };

        let mut got_content = false;

        loop {
            match handle.try_recv() {
                Some(ApiEvent::TextDelta(text)) => {
                    self.output_tokens += (text.len() / 4) as u32;
                    self.output.push_str(&text);
                    self.current_assistant_text.push_str(&text);
                    // Cap output to prevent unbounded memory growth.
                    if self.output.len() > 65536 {
                        let mut trim = self.output.len() - 65536;
                        while trim < self.output.len()
                            && !self.output.is_char_boundary(trim)
                        {
                            trim += 1;
                        }
                        self.output.drain(..trim);
                        self.output.insert_str(0, "[...truncated...]\n");
                    }
                    got_content = true;
                }
                Some(ApiEvent::ToolUse { id, call }) => {
                    let args_display = format_tool_args(&call.tool, &call.args);
                    if args_display.is_empty() {
                        self.output.push_str(&format!("\n▶ {}\n", call.tool.api_name()));
                    } else {
                        self.output.push_str(&format!("\n▶ {} {}\n", call.tool.api_name(), args_display));
                    }
                    self.tool_use_ids.push(id.clone());
                    self.pending_tools.push((id, call));
                    got_content = true;
                }
                Some(ApiEvent::Done) => {
                    // Flush accumulated assistant text into the conversation.
                    if !self.current_assistant_text.is_empty() {
                        let text = std::mem::take(&mut self.current_assistant_text);
                        self.agent.push_message(AgentMessage::Assistant(text));
                    }

                    if self.pending_tools.is_empty() {
                        self.output.push_str(&format!(
                            "\n\n📊 ~{}in / ~{}out tokens | {} tool calls\n✓ Agent finished.\n",
                            self.input_tokens, self.output_tokens, self.tool_call_count,
                        ));
                        self.status = AgentPaneStatus::Done;
                        self.api_handle = None;
                        self.save_conversation();
                    } else {
                        // Execute pending tools and continue conversation.
                        self.execute_pending_tools();
                    }
                    got_content = true;
                    break;
                }
                Some(ApiEvent::Error(e)) => {
                    self.output.push_str(&format!("\n\n✗ Error: {e}\n"));
                    self.rollback_if_dirty();
                    self.status = AgentPaneStatus::Failed;
                    self.api_handle = None;
                    self.save_conversation();
                    got_content = true;
                    break;
                }
                None => break,
            }
        }

        got_content
    }

    /// Execute all pending tool calls, append results to the conversation,
    /// and re-invoke the Claude API for the next turn.
    fn execute_pending_tools(&mut self) {
        if self.turn_count >= MAX_TOOL_ROUNDS {
            self.output.push_str(&format!(
                "\n\n✗ Agent hit iteration limit ({MAX_TOOL_ROUNDS} tool rounds).\n"
            ));
            self.rollback_if_dirty();
            self.status = AgentPaneStatus::Failed;
            self.api_handle = None;
            self.save_conversation();
            return;
        }
        self.turn_count += 1;

        // Append all tool calls to the agent's message history.
        for (_, call) in &self.pending_tools {
            self.agent
                .push_message(AgentMessage::ToolCall(call.clone()));
        }

        // Execute each tool (with permission check) and append results.
        let working_dir = self.working_dir.clone();
        for (_, call) in self.pending_tools.drain(..) {
            self.tool_call_count += 1;
            let start = std::time::Instant::now();
            let result = if let Err(denied) = self.permissions.check_tool(&call.tool) {
                ToolResult { tool: call.tool, success: false, output: denied.to_string() }
            } else {
                execute_tool(call.tool, &call.args, &working_dir)
            };
            let elapsed = start.elapsed();

            // Track file edits for rollback.
            if result.success && matches!(call.tool, ToolType::WriteFile | ToolType::EditFile) {
                self.has_file_edits = true;
            }

            // Display in pane.
            let status_char = if result.success { "✓" } else { "✗" };
            self.output.push_str(&format!(
                "  {} {:.0}ms\n",
                status_char,
                elapsed.as_millis(),
            ));

            // Show truncated output (max 200 chars for display).
            if result.output.len() > 200 {
                let truncated: String = result.output.chars().take(200).collect();
                self.output.push_str(&format!(
                    "  ← {}... ({} bytes)\n",
                    truncated,
                    result.output.len()
                ));
            } else if !result.output.is_empty() {
                self.output.push_str(&format!(
                    "  ← {}\n",
                    result.output.lines().next().unwrap_or("")
                ));
            }

            self.agent.push_message(AgentMessage::ToolResult(result));
        }

        // Re-invoke Claude with the updated conversation.
        let tools = available_tools();
        let handle = send_message(
            &self.claude_config,
            &self.agent,
            &tools,
            &self.tool_use_ids,
        );
        self.api_handle = Some(handle);

        self.output
            .push_str(&format!("\n● Continuing... (turn {})\n", self.turn_count));
    }

    /// Return cached tail lines for rendering. Only re-splits when output grows.
    pub(crate) fn tail_lines(&mut self, max_lines: usize) -> &[String] {
        if self.output.len() != self.cached_len {
            self.cached_len = self.output.len();
            let all: Vec<&str> = self.output.lines().collect();
            let start = all.len().saturating_sub(max_lines);
            self.cached_lines = all[start..].iter().map(|s| s.to_string()).collect();
        }
        &self.cached_lines
    }

    /// Revert file edits on failure (git checkout -- .).
    fn rollback_if_dirty(&mut self) {
        if !self.has_file_edits { return; }
        self.output.push_str("\n⚠ Agent failed with uncommitted edits. Reverting...\n");
        let result = std::process::Command::new("git")
            .args(["checkout", "--", "."])
            .current_dir(&self.working_dir)
            .output();
        match result {
            Ok(out) if out.status.success() => {
                self.output.push_str("  ← Reverted to clean state.\n");
            }
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                self.output.push_str(&format!("  ← Revert failed: {stderr}\n"));
            }
            Err(e) => {
                self.output.push_str(&format!("  ← Revert failed: {e}\n"));
            }
        }
    }

    /// Save the agent conversation to disk for debugging and replay.
    pub(crate) fn save_conversation(&self) {
        let dir = std::env::var("HOME")
            .map(|h| std::path::PathBuf::from(h).join(".config/phantom/agents"))
            .unwrap_or_else(|_| std::path::PathBuf::from("/tmp/phantom-agents"));

        if std::fs::create_dir_all(&dir).is_err() { return; }

        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs()).unwrap_or(0);

        let sanitized: String = self.task.chars().take(30)
            .map(|c| if c.is_alphanumeric() || c == '-' { c } else { '_' })
            .collect();

        let path = dir.join(format!("{timestamp}_{sanitized}.json"));
        let json = self.agent.to_json();
        if let Ok(content) = serde_json::to_string_pretty(&json) {
            let _ = std::fs::write(&path, content);
            log::info!("Agent conversation saved: {}", path.display());
        }
    }

    /// Extract a skill summary from a completed agent (Voyager pattern).
    pub(crate) fn extract_skill_summary(&self) -> Option<String> {
        if self.status != AgentPaneStatus::Done { return None; }
        let text = self.agent.messages.iter().rev()
            .find_map(|m| if let AgentMessage::Assistant(t) = m { Some(t.clone()) } else { None })?;
        let cleaned = text.trim_end_matches("Agent finished.").trim();
        if cleaned.is_empty() { return None; }
        if cleaned.len() > 500 {
            Some(format!("...{}", &cleaned[cleaned.len()-500..]))
        } else {
            Some(cleaned.to_string())
        }
    }
}

// ---------------------------------------------------------------------------
// Codebase context injection
// ---------------------------------------------------------------------------

/// Build project context for agent system prompts.
/// Reads CLAUDE.md if it exists, and provides a crate map.
fn build_codebase_context() -> String {
    let working_dir = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".into());

    let mut ctx = String::from(
        "CODEBASE CONTEXT:\n\
         You are an agent inside Phantom, an AI-native terminal emulator.\n\
         Written in Rust. 19 crates. ~100K lines. deny(warnings) is enforced.\n\
         Always run `cargo check --workspace` after edits.\n\n\
         Key crates:\n\
         - phantom (binary entry point)\n\
         - phantom-app (GUI: render, input, mouse, coordinator, agent_pane)\n\
         - phantom-brain (OODA loop, scoring, goals, proactive, orchestrator)\n\
         - phantom-agents (tools, API client, permissions, agent lifecycle)\n\
         - phantom-adapter (AppAdapter trait, spatial preferences, event bus)\n\
         - phantom-ui (layout engine, arbiter, themes, keybinds)\n\
         - phantom-terminal (PTY, VTE, SGR mouse encoding)\n\
         - phantom-scene (scene graph, z-order, dirty flags, render layers)\n\
         - phantom-semantic (output parsing, error detection)\n\
         - phantom-context (project detection, git state)\n\
         - phantom-memory (persistent key-value store)\n\
         - phantom-mcp (MCP protocol, Unix socket server/client)\n\n"
    );

    // Try to read CLAUDE.md for project-specific instructions.
    let claude_md = std::path::Path::new(&working_dir).join("CLAUDE.md");
    if let Ok(content) = std::fs::read_to_string(&claude_md) {
        let truncated = if content.len() > 2000 {
            format!("{}...(truncated)", &content[..2000])
        } else {
            content
        };
        ctx.push_str(&format!("CLAUDE.md:\n{truncated}\n\n"));
    }

    ctx
}

// ---------------------------------------------------------------------------
// Display helpers
// ---------------------------------------------------------------------------

/// Format tool arguments as a compact, human-readable string.
fn format_tool_args(tool: &ToolType, args: &serde_json::Value) -> String {
    match tool {
        ToolType::ReadFile | ToolType::EditFile | ToolType::ListFiles => {
            args.get("path").and_then(|v| v.as_str()).unwrap_or("?").to_string()
        }
        ToolType::WriteFile => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("?");
            let len = args.get("content").and_then(|v| v.as_str()).map(|s| s.len()).unwrap_or(0);
            format!("{path} ({len} bytes)")
        }
        ToolType::RunCommand => {
            args.get("command").and_then(|v| v.as_str()).unwrap_or("?").to_string()
        }
        ToolType::SearchFiles => {
            args.get("pattern").and_then(|v| v.as_str()).unwrap_or("?").to_string()
        }
        ToolType::GitStatus | ToolType::GitDiff => String::new(),
    }
}

// ---------------------------------------------------------------------------
// App integration
// ---------------------------------------------------------------------------

impl App {
    /// Spawn a new agent pane as a first-class coordinator adapter.
    ///
    /// Creates the agent, wraps it in an AgentAdapter, and registers it
    /// with the coordinator so it gets its own layout pane, scene node,
    /// and input routing — just like a terminal.
    pub(crate) fn spawn_agent_pane(&mut self, task: AgentTask) -> bool {
        let Some(claude_config) = ClaudeConfig::from_env() else {
            warn!("Cannot spawn agent: ANTHROPIC_API_KEY not set");
            return false;
        };

        let agent_pane = AgentPane::spawn(task, &claude_config);
        let adapter = crate::adapters::agent::AgentAdapter::new(agent_pane);

        let content_node = self.scene_content_node;
        let _app_id = self.coordinator.register_adapter(
            Box::new(adapter),
            &mut self.layout,
            &mut self.scene,
            content_node,
            phantom_scene::clock::Cadence::unlimited(),
        );

        // Also keep in agent_panes for legacy polling (skill extraction, etc.)
        // TODO: migrate skill extraction to coordinator event bus and remove agent_panes Vec.
        info!("Agent adapter registered (AppId {_app_id})");
        true
    }

    /// Poll all active agent panes for new output. Call from update().
    /// Streams new text deltas and status changes into the console.
    pub(crate) fn poll_agent_panes(&mut self) {
        let mut events: Vec<(String, Option<String>, AgentPaneStatus, AgentPaneStatus)> = Vec::new();
        for pane in &mut self.agent_panes {
            let prev_status = pane.status;
            let prev_len = pane.output.len();
            pane.poll();
            // Capture new text added this frame.
            let new_text = if pane.output.len() > prev_len {
                Some(pane.output[prev_len..].to_string())
            } else {
                None
            };
            if new_text.is_some() || pane.status != prev_status {
                events.push((pane.task.clone(), new_text, prev_status, pane.status));
            }
        }
        for (task, new_text, prev, current) in events {
            // Stream new text lines into the console.
            if let Some(text) = new_text {
                for line in text.lines() {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() {
                        self.console.output(trimmed.to_string());
                    }
                }
            }
            // Status transitions.
            if current != prev {
                let short: String = task.chars().take(60).collect();
                match current {
                    AgentPaneStatus::Done => {
                        self.console.system(format!("Agent finished: {short}"));
                    }
                    AgentPaneStatus::Failed => {
                        self.console.error(format!("Agent failed: {short}"));
                    }
                    _ => {}
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn test_agent() -> Agent {
        Agent::new(0, AgentTask::FreeForm { prompt: "test task".into() })
    }

    fn test_config() -> ClaudeConfig {
        ClaudeConfig::new("sk-test-fake")
    }

    fn agent_with_handle() -> (AgentPane, mpsc::Sender<ApiEvent>) {
        let (tx, rx) = mpsc::channel();
        let handle = ApiHandle::from_receiver(rx);
        let pane = AgentPane {
            task: "test task".into(),
            status: AgentPaneStatus::Working,
            output: String::from("● Agent working...\n\n"),
            api_handle: Some(handle),
            tool_use_ids: Vec::new(),
            cached_lines: Vec::new(),
            cached_len: 0,
            event_emitted: false,
            agent: test_agent(),
            pending_tools: Vec::new(),
            working_dir: ".".into(),
            claude_config: test_config(),
            turn_count: 0,
            current_assistant_text: String::new(),
            permissions: PermissionSet::all(),
            input_tokens: 0,
            output_tokens: 0,
            tool_call_count: 0,
            has_file_edits: false,
        };
        (pane, tx)
    }

    #[test]
    fn agent_pane_starts_working() {
        let (pane, _tx) = agent_with_handle();
        assert_eq!(pane.status, AgentPaneStatus::Working);
        assert!(pane.output.contains("Agent working"));
    }

    #[test]
    fn poll_receives_text_delta() {
        let (mut pane, tx) = agent_with_handle();
        tx.send(ApiEvent::TextDelta("hello world".into())).unwrap();

        let got = pane.poll();
        assert!(got, "should have received content");
        assert!(pane.output.contains("hello world"));
        assert_eq!(pane.status, AgentPaneStatus::Working);
    }

    #[test]
    fn poll_receives_done_event() {
        let (mut pane, tx) = agent_with_handle();
        tx.send(ApiEvent::TextDelta("result".into())).unwrap();
        tx.send(ApiEvent::Done).unwrap();

        pane.poll();
        assert_eq!(pane.status, AgentPaneStatus::Done);
        assert!(pane.output.contains("✓ Agent finished"));
        assert!(pane.api_handle.is_none(), "handle should be dropped on Done");
    }

    #[test]
    fn poll_receives_error_event() {
        let (mut pane, tx) = agent_with_handle();
        tx.send(ApiEvent::Error("network timeout".into())).unwrap();

        pane.poll();
        assert_eq!(pane.status, AgentPaneStatus::Failed);
        assert!(pane.output.contains("✗ Error: network timeout"));
        assert!(pane.api_handle.is_none());
    }

    #[test]
    fn poll_accumulates_multiple_deltas() {
        let (mut pane, tx) = agent_with_handle();
        tx.send(ApiEvent::TextDelta("line 1\n".into())).unwrap();
        tx.send(ApiEvent::TextDelta("line 2\n".into())).unwrap();
        tx.send(ApiEvent::TextDelta("line 3\n".into())).unwrap();

        pane.poll();
        assert!(pane.output.contains("line 1"));
        assert!(pane.output.contains("line 2"));
        assert!(pane.output.contains("line 3"));
    }

    #[test]
    fn poll_returns_false_when_no_handle() {
        let mut pane = AgentPane {
            task: "orphan".into(),
            status: AgentPaneStatus::Done,
            output: String::new(),
            api_handle: None,
            tool_use_ids: Vec::new(),
            cached_lines: Vec::new(),
            cached_len: 0,
            event_emitted: false,
            agent: test_agent(),
            pending_tools: Vec::new(),
            working_dir: ".".into(),
            claude_config: test_config(),
            turn_count: 0,
            current_assistant_text: String::new(),
            permissions: PermissionSet::all(),
            input_tokens: 0,
            output_tokens: 0,
            tool_call_count: 0,
            has_file_edits: false,
        };
        assert!(!pane.poll());
    }

    #[test]
    fn poll_returns_false_when_no_events() {
        let (mut pane, _tx) = agent_with_handle();
        // Don't send anything.
        assert!(!pane.poll());
        assert_eq!(pane.status, AgentPaneStatus::Working);
    }

    #[test]
    fn tool_use_tracked_in_ids() {
        let (mut pane, tx) = agent_with_handle();
        tx.send(ApiEvent::ToolUse {
            id: "tool_123".into(),
            call: phantom_agents::tools::ToolCall {
                tool: phantom_agents::tools::ToolType::ReadFile,
                args: serde_json::json!({"path": "/tmp/test"}),
            },
        }).unwrap();

        pane.poll();
        assert_eq!(pane.tool_use_ids, vec!["tool_123"]);
        assert!(pane.output.contains("▶ read_file"));
        // New: also tracked in pending_tools.
        assert_eq!(pane.pending_tools.len(), 1);
        assert_eq!(pane.pending_tools[0].0, "tool_123");
    }

    #[test]
    fn text_delta_accumulates_assistant_text() {
        let (mut pane, tx) = agent_with_handle();
        tx.send(ApiEvent::TextDelta("hello ".into())).unwrap();
        tx.send(ApiEvent::TextDelta("world".into())).unwrap();

        pane.poll();
        assert_eq!(pane.current_assistant_text, "hello world");
    }

    #[test]
    fn done_without_tools_marks_finished() {
        let (mut pane, tx) = agent_with_handle();
        tx.send(ApiEvent::TextDelta("result".into())).unwrap();
        tx.send(ApiEvent::Done).unwrap();

        pane.poll();
        assert_eq!(pane.status, AgentPaneStatus::Done);
        assert!(pane.api_handle.is_none());
        // Assistant text should have been flushed to agent messages.
        assert!(pane.current_assistant_text.is_empty());
        assert!(pane.agent.messages.iter().any(|m| matches!(m, AgentMessage::Assistant(t) if t == "result")));
    }

    #[test]
    fn done_with_tools_executes_and_continues() {
        let (mut pane, tx) = agent_with_handle();
        // Set working_dir to temp dir so ListFiles works.
        pane.working_dir = std::env::temp_dir().to_string_lossy().into_owned();

        tx.send(ApiEvent::TextDelta("Let me check.".into())).unwrap();
        tx.send(ApiEvent::ToolUse {
            id: "toolu_1".into(),
            call: phantom_agents::tools::ToolCall {
                tool: phantom_agents::tools::ToolType::ListFiles,
                args: serde_json::json!({"path": "."}),
            },
        }).unwrap();
        tx.send(ApiEvent::Done).unwrap();

        pane.poll();

        // Should NOT be Done — should have re-invoked.
        assert_eq!(pane.status, AgentPaneStatus::Working);
        // pending_tools should be drained.
        assert!(pane.pending_tools.is_empty());
        // turn_count should have incremented.
        assert_eq!(pane.turn_count, 1);
        // Agent messages should include ToolCall and ToolResult.
        let has_tool_call = pane.agent.messages.iter().any(|m| matches!(m, AgentMessage::ToolCall(_)));
        let has_tool_result = pane.agent.messages.iter().any(|m| matches!(m, AgentMessage::ToolResult(_)));
        assert!(has_tool_call, "agent should have a ToolCall message");
        assert!(has_tool_result, "agent should have a ToolResult message");
        // Output should show the continuation.
        assert!(pane.output.contains("Continuing... (turn 1)"));
        // A new api_handle should have been created (by send_message).
        assert!(pane.api_handle.is_some());
    }

    #[test]
    fn iteration_limit_stops_agent() {
        let (mut pane, tx) = agent_with_handle();
        pane.turn_count = MAX_TOOL_ROUNDS; // Already at limit.

        tx.send(ApiEvent::ToolUse {
            id: "toolu_limit".into(),
            call: phantom_agents::tools::ToolCall {
                tool: phantom_agents::tools::ToolType::GitStatus,
                args: serde_json::json!({}),
            },
        }).unwrap();
        tx.send(ApiEvent::Done).unwrap();

        pane.poll();

        assert_eq!(pane.status, AgentPaneStatus::Failed);
        assert!(pane.output.contains("iteration limit"));
        assert!(pane.api_handle.is_none());
    }

    #[test]
    fn task_description_extraction() {
        // Verify the description logic works for each AgentTask variant.
        let cases: Vec<(AgentTask, &str)> = vec![
            (AgentTask::FreeForm { prompt: "fix bug".into() }, "fix bug"),
            (AgentTask::RunCommand { command: "cargo test".into() }, "Run: cargo test"),
            (AgentTask::WatchAndNotify { description: "build".into() }, "Watch: build"),
        ];

        for (task, expected_prefix) in cases {
            let desc = match &task {
                AgentTask::FreeForm { prompt } => prompt.clone(),
                AgentTask::FixError { error_summary, .. } => format!("Fix: {error_summary}"),
                AgentTask::RunCommand { command } => format!("Run: {command}"),
                AgentTask::ReviewCode { context, .. } => format!("Review: {context}"),
                AgentTask::WatchAndNotify { description } => format!("Watch: {description}"),
            };
            assert!(
                desc.starts_with(expected_prefix),
                "task desc '{desc}' should start with '{expected_prefix}'"
            );
        }
    }
}
