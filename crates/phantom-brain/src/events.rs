//! Event types for the AI brain thread.
//!
//! [`AiEvent`] is what wakes the brain. [`AiAction`] is what it decides to do.
//! Both are passed over `std::sync::mpsc` channels between the brain thread
//! and the rest of the application.

use phantom_agents::{AgentId, AgentTask};
use phantom_semantic::ParsedOutput;

// ---------------------------------------------------------------------------
// Inbound: events that wake the brain
// ---------------------------------------------------------------------------

/// Events that wake the AI brain thread.
///
/// Produced by the terminal I/O thread, the render thread, file watchers,
/// timers, and agents. The brain blocks on a channel receiver until one of
/// these arrives.
#[derive(Debug, Clone)]
pub enum AiEvent {
    // -- Terminal I/O --------------------------------------------------------

    /// A command finished executing and its output has been semantically parsed.
    CommandComplete(ParsedOutput),

    /// A chunk of streaming output arrived (partial, not yet parsed).
    OutputChunk(String),

    // -- User ----------------------------------------------------------------

    /// The user pressed the interrupt key (e.g. `!`) with an optional command.
    Interrupt(String),

    /// The user explicitly requested an agent task.
    AgentRequest(AgentTask),

    // -- Agents --------------------------------------------------------------

    /// An agent finished its work.
    AgentComplete {
        id: AgentId,
        success: bool,
        summary: String,
    },

    /// An agent needs user input before it can continue.
    AgentNeedsInput {
        id: AgentId,
        question: String,
    },

    // -- Environment ---------------------------------------------------------

    /// A watched file changed on disk.
    FileChanged(String),

    /// The git state changed (branch switch, commit, stash, etc.).
    GitStateChanged,

    // -- Timers --------------------------------------------------------------

    /// The user has been idle for `seconds` since their last input.
    UserIdle { seconds: f32 },

    /// Periodic tick for a running watcher agent.
    WatcherTick { agent_id: AgentId },

    // -- System --------------------------------------------------------------

    /// Graceful shutdown request.
    Shutdown,
}

// ---------------------------------------------------------------------------
// Outbound: actions the brain decides to take
// ---------------------------------------------------------------------------

/// Actions the AI brain can emit (sent back to the render / app thread).
///
/// The render thread polls for these and applies them to the UI or system
/// state. The brain only emits an action when its utility score exceeds the
/// configured quiet threshold.
#[derive(Debug, Clone)]
pub enum AiAction {
    /// Show an inline suggestion with optional shortcut options.
    ShowSuggestion {
        text: String,
        options: Vec<(char, String)>,
    },

    /// Spawn a new agent to work on a task.
    SpawnAgent(AgentTask),

    /// Persist a key-value pair to project memory.
    UpdateMemory { key: String, value: String },

    /// Show a transient notification in the UI.
    ShowNotification(String),

    /// Execute a shell command on behalf of the user.
    RunCommand(String),

    /// Do nothing. The brain decided silence is the best action.
    DoNothing,
}
