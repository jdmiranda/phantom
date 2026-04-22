//! Agent pane management — spawn AI agents in visible GUI panes.
//!
//! When the brain decides to spawn an agent (or the user requests one),
//! this module creates a new pane, starts a Claude API agent on a
//! background thread, and streams output into the pane each frame.

use log::{info, warn};

use phantom_agents::api::{ApiEvent, ApiHandle, ClaudeConfig, send_message};
use phantom_agents::agent::{Agent, AgentMessage};
use phantom_agents::tools::available_tools;
use phantom_agents::AgentTask;

use crate::app::App;

// ---------------------------------------------------------------------------
// AgentPane — a running agent with its output stream
// ---------------------------------------------------------------------------

/// An active agent running in a GUI pane.
#[allow(dead_code)] // Fields used by render (agent pane rendering WIP)
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

        let mut agent = Agent::new(0, task);
        let sys_prompt = agent.system_prompt();
        agent.push_message(AgentMessage::System(sys_prompt));

        let tools = available_tools();
        let handle = send_message(claude_config, &agent, &tools, &[]);

        info!("Agent pane spawned: {task_desc}");

        Self {
            task: task_desc,
            status: AgentPaneStatus::Working,
            output: String::from("● Agent working...\n\n"),
            api_handle: Some(handle),
            tool_use_ids: Vec::new(),
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
                    self.output.push_str(&text);
                    got_content = true;
                }
                Some(ApiEvent::ToolUse { id, call }) => {
                    self.output.push_str(&format!(
                        "\n▶ Tool: {:?} {}\n",
                        call.tool,
                        serde_json::to_string(&call.args).unwrap_or_default()
                    ));
                    self.tool_use_ids.push(id);
                    got_content = true;
                }
                Some(ApiEvent::Done) => {
                    self.output.push_str("\n\n✓ Agent finished.\n");
                    self.status = AgentPaneStatus::Done;
                    self.api_handle = None;
                    got_content = true;
                    break;
                }
                Some(ApiEvent::Error(e)) => {
                    self.output.push_str(&format!("\n\n✗ Error: {e}\n"));
                    self.status = AgentPaneStatus::Failed;
                    self.api_handle = None;
                    got_content = true;
                    break;
                }
                None => break,
            }
        }

        got_content
    }

    /// Whether the agent is still working.
    #[allow(dead_code)] // Used by render (agent pane rendering WIP)
    pub(crate) fn is_working(&self) -> bool {
        self.status == AgentPaneStatus::Working
    }
}

// ---------------------------------------------------------------------------
// App integration
// ---------------------------------------------------------------------------

impl App {
    /// Spawn a new agent pane for the given task.
    ///
    /// Creates the agent, starts the Claude API call on a background thread,
    /// and adds the agent pane to the app's list. The render loop will pick
    /// it up and display it in a split pane.
    pub(crate) fn spawn_agent_pane(&mut self, task: AgentTask) {
        let Some(claude_config) = ClaudeConfig::from_env() else {
            warn!("Cannot spawn agent: ANTHROPIC_API_KEY not set");
            return;
        };

        let agent_pane = AgentPane::spawn(task, &claude_config);
        self.agent_panes.push(agent_pane);
        info!("Agent pane added (total: {})", self.agent_panes.len());
    }

    /// Poll all active agent panes for new output. Call from update().
    pub(crate) fn poll_agent_panes(&mut self) {
        for pane in &mut self.agent_panes {
            pane.poll();
        }
    }
}
