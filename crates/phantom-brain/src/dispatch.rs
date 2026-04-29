//! Single-dispatch trait for [`AiAction`].
//!
//! Defines the [`ActionHandler`] trait — the contract that both the full GUI
//! app and the headless REPL must satisfy — and implements
//! [`AiAction::execute`], the **single exhaustive match** over every variant.
//!
//! # Why one match?
//!
//! Previously `AiAction` was matched in two separate places:
//! - `phantom_app::update::App::execute_brain_action` (GUI path)
//! - `phantom::headless::drain_brain` (headless path)
//!
//! When a new variant was added to `AiAction`, both sites had to be updated
//! independently — a silent source of drift. With `ExecuteAction`, the
//! compiler enforces exhaustiveness in exactly one place; call sites just
//! implement `ActionHandler`.

use phantom_agents::{AgentId, AgentTask};
use phantom_agents::agent::PauseReason;
use phantom_agents::dispatch::Disposition;

use crate::events::{AiAction, ConnectionState, SuggestionOption};

// ---------------------------------------------------------------------------
// ActionHandler trait
// ---------------------------------------------------------------------------

/// Contextual handler for each [`AiAction`] variant.
///
/// Implement this trait for the context that will process brain actions.
/// The GUI app implements it with full coordinator/scene/memory access;
/// the headless REPL implements it with simple `println!` output.
///
/// Every method has a default no-op body so partial implementations compile
/// for test stubs. Production implementations should override all methods.
pub trait ActionHandler {
    /// Show an inline suggestion overlay.
    fn show_suggestion(&mut self, text: String, options: Vec<SuggestionOption>);

    /// Persist a transient notification.
    fn show_notification(&mut self, msg: String);

    /// Persist a key-value pair to project memory.
    fn update_memory(&mut self, key: String, value: String);

    /// Request that an agent be spawned with the given task and metadata.
    fn spawn_agent(&mut self, task: AgentTask, spawn_tag: Option<u64>, disposition: Disposition);

    /// Route a reply back to the console / scrollback.
    fn console_reply(&mut self, reply: String);

    /// Execute a shell command on the focused pane.
    fn run_command(&mut self, cmd: String);

    /// Dismiss (remove) the adapter identified by `app_id`.
    fn dismiss_adapter(&mut self, app_id: u32);

    /// An agent hit its retry limit or encountered an unrecoverable error.
    fn agent_flatlined(&mut self, id: AgentId, reason: String);

    /// A proactive suggestion was emitted by the brain's suggestion engine.
    fn suggest(&mut self, action: String, rationale: String, confidence: f32);

    /// Quarantine a repeat-offender agent.
    fn quarantine_agent(&mut self, agent_id: AgentId, denial_count: usize);

    /// Notification that an agent has been placed in quarantine.
    fn agent_quarantined(&mut self, agent_id: AgentId, denial_count: usize);

    /// A human-checkpoint gate was reached in the active plan.
    fn checkpoint_reached(&mut self, step_idx: usize, description: String) {
        let _ = (step_idx, description);
    }

    /// Pause a running agent because its backend became unavailable.
    fn pause_agent(&mut self, agent_id: AgentId, reason: PauseReason) {
        let _ = (agent_id, reason);
    }

    /// Resume a previously paused agent because its backend is available again.
    fn resume_agent(&mut self, agent_id: AgentId) {
        let _ = agent_id;
    }

    /// Update the connection state indicator in the status bar.
    fn update_connection_state(&mut self, state: ConnectionState) {
        let _ = state;
    }
}

// ---------------------------------------------------------------------------
// Single-dispatch impl on AiAction
// ---------------------------------------------------------------------------

impl AiAction {
    /// Dispatch this action to a handler.
    ///
    /// This is the **one and only** exhaustive match over [`AiAction`] variants.
    /// Adding a new variant requires updating only this method; the compiler
    /// then flags every `ActionHandler` impl that is missing the corresponding
    /// method.
    pub fn execute(self, handler: &mut dyn ActionHandler) {
        match self {
            AiAction::ShowSuggestion { text, options } => {
                handler.show_suggestion(text, options);
            }
            AiAction::ShowNotification(msg) => {
                handler.show_notification(msg);
            }
            AiAction::UpdateMemory { key, value } => {
                handler.update_memory(key, value);
            }
            AiAction::SpawnAgent { task, spawn_tag, disposition } => {
                handler.spawn_agent(task, spawn_tag, disposition);
            }
            AiAction::ConsoleReply(reply) => {
                handler.console_reply(reply);
            }
            AiAction::RunCommand(cmd) => {
                handler.run_command(cmd);
            }
            AiAction::DismissAdapter { app_id } => {
                handler.dismiss_adapter(app_id);
            }
            AiAction::AgentFlatlined { id, reason } => {
                handler.agent_flatlined(id, reason);
            }
            AiAction::Suggest { action, rationale, confidence } => {
                handler.suggest(action, rationale, confidence);
            }
            AiAction::QuarantineAgent { agent_id, denial_count } => {
                handler.quarantine_agent(agent_id, denial_count);
            }
            AiAction::AgentQuarantined { agent_id, denial_count } => {
                handler.agent_quarantined(agent_id, denial_count);
            }
            AiAction::CheckpointReached { step_idx, description } => {
                handler.checkpoint_reached(step_idx, description);
            }
            AiAction::PauseAgent { agent_id, reason } => {
                handler.pause_agent(agent_id, reason);
            }
            AiAction::ResumeAgent { agent_id } => {
                handler.resume_agent(agent_id);
            }
            AiAction::UpdateConnectionState { state } => {
                handler.update_connection_state(state);
            }
            AiAction::DoNothing => {}
        }
    }
}
