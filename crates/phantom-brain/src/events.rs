//! Event types for the AI brain thread.
//!
//! [`AiEvent`] is what wakes the brain. [`AiAction`] is what it decides to do.
//! Both are passed over `std::sync::mpsc` channels between the brain thread
//! and the rest of the application.

use phantom_agents::dispatch::Disposition;
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
        /// Reconciler spawn tag echoed back from the agent adapter so
        /// `ReconcilerState::on_agent_complete` can match by tag rather
        /// than relying on sequential execution assumptions.
        spawn_tag: Option<u64>,
    },

    /// An agent needs user input before it can continue.
    AgentNeedsInput { id: AgentId, question: String },

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
    /// User set a goal for the brain to pursue autonomously.
    GoalSet {
        objective: String,
        initial_task: String,
    },

    /// An agent's tool dispatch was denied due to a capability violation.
    ///
    /// Sec.7: The brain accumulates consecutive `CapabilityDenied` events per
    /// agent and emits [`AiAction::QuarantineAgent`] when the configured
    /// threshold is reached. Each `CapabilityDenied` resets on a successful
    /// tool dispatch.
    CapabilityDenied {
        /// The agent whose tool call was denied.
        agent_id: AgentId,
        /// Name of the tool that was attempted (e.g. `"run_command"`).
        tool_name: String,
    },

    /// Graceful shutdown request.
    Shutdown,
}

// ---------------------------------------------------------------------------
// Outbound: actions the brain decides to take
// ---------------------------------------------------------------------------

/// An option in a brain suggestion overlay.
#[derive(Debug, Clone)]
pub struct SuggestionOption {
    pub key: char,
    pub label: String,
    /// Action to execute if the user picks this option. None = just dismiss.
    pub action: Option<Box<AiAction>>,
}

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
        options: Vec<SuggestionOption>,
    },

    /// Spawn a new agent to work on a task.
    ///
    /// `spawn_tag` is stamped by the reconciler so that the resulting
    /// `AgentComplete` event can be matched back to the correct
    /// `active_dispatches` entry regardless of the AgentManager's own
    /// sequential ID assignment. Non-reconciler callers leave it `None`.
    ///
    /// `disposition` carries the step's intent classification (Issue #49).
    /// When `disposition.auto_approve()` is `true`, the app layer skips the
    /// `AwaitingApproval` state and goes `Queued → Working` directly.
    SpawnAgent {
        task: AgentTask,
        /// Reconciler-assigned synthetic ID; `None` for user-initiated spawns.
        spawn_tag: Option<u64>,
        /// Intent classification forwarded from the [`crate::orchestrator::PlanStep`].
        /// Defaults to [`Disposition::Chat`] for non-reconciler spawns.
        disposition: Disposition,
    },

    /// Persist a key-value pair to project memory.
    UpdateMemory { key: String, value: String },

    /// Show a transient notification in the UI.
    ShowNotification(String),

    /// Execute a shell command on behalf of the user.
    RunCommand(String),

    /// Reply to a console query (from AiEvent::Interrupt).
    /// Routed back to the console scrollback so the user sees the answer
    /// inline where they typed the question.
    ConsoleReply(String),

    /// Dismiss (remove) an adapter by its AppId.
    DismissAdapter { app_id: u32 },

    /// An agent hit its retry limit or an unrecoverable error.
    /// The reconciler emits this when a TaskLedger step exhausts max_attempts
    /// or a stall timeout fires. Requires manual retry to clear.
    AgentFlatlined { id: AgentId, reason: String },

    /// Quarantine a repeat-offender agent.
    ///
    /// Sec.7: Emitted by the brain's denial counter when an agent's consecutive
    /// `CapabilityDenied` count reaches the threshold. The app applies this to
    /// the [`phantom_agents::quarantine::QuarantineRegistry`] and transitions
    /// the agent to `Quarantined`.
    QuarantineAgent {
        /// The agent to quarantine.
        agent_id: AgentId,
        /// Number of consecutive denials that triggered the quarantine.
        denial_count: usize,
    },

    /// An agent has been placed in quarantine.
    ///
    /// Sec.7: Emitted after the app has applied a [`AiAction::QuarantineAgent`]
    /// to the registry. The UI observes this to display a quarantine indicator
    /// on the offending agent's pane.
    AgentQuarantined {
        /// The newly-quarantined agent.
        agent_id: AgentId,
        /// Total consecutive denials that caused the quarantine.
        denial_count: usize,
    },

    /// Proactive suggestion emitted by [`ProactiveSuggester`].
    ///
    /// Unlike `ShowSuggestion`, which is driven by the utility scorer,
    /// `Suggest` carries a machine-generated rationale and a confidence
    /// score so the renderer can decide how prominently to surface it.
    Suggest {
        /// Short description of the suggested next action.
        action: String,
        /// One-sentence explanation of why the brain is suggesting this.
        rationale: String,
        /// Confidence in the suggestion, in the range `[0.0, 1.0]`.
        confidence: f32,
    },

    /// A checkpoint step is the next eligible step but cannot run until approved.
    ///
    /// Emitted by the reconciler when [`TaskLedger::eligible_next`] returns no
    /// eligible steps because the next candidate step has
    /// [`StepStatus::NeedsApproval`]. The UI should surface this as a blocking
    /// prompt so the operator can call [`TaskLedger::approve_checkpoint`].
    ///
    /// The brain emits this action **once per checkpoint encounter** to avoid
    /// spamming the UI on every reconciler tick.
    CheckpointReached {
        /// Index of the step in the active [`TaskLedger`] plan.
        step_idx: usize,
        /// Human-readable description of what the step will do once approved.
        description: String,
    },

    /// Do nothing. The brain decided silence is the best action.
    DoNothing,
}
