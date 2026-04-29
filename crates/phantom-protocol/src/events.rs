//! Typed event definitions for the Phantom event bus.
//!
//! Replaces stringly-typed serde_json::Value payloads with compile-time
//! checked variants. Every bus message carries one of these.

/// Unique app identifier (mirrors phantom-adapter::AppId).
pub type AppId = u32;

/// Unique agent identifier.
///
/// Widened to `u64` to match `phantom_agents::AgentId` (fixes #273 — no
/// narrowing cast required across the IPC boundary).
pub type AgentId = u64;

/// Unique session identifier.
pub type SessionId = u32;

/// Unique job identifier.
pub type JobId = u64;

/// Typed event variants for the Phantom event bus.
#[derive(Clone, Debug)]
pub enum Event {
    // -- Terminal / PTY ------------------------------------------------------
    TerminalOutput { app_id: AppId, bytes: u64 },
    CommandStarted { app_id: AppId, command: String },
    CommandComplete { app_id: AppId, exit_code: i32 },

    // -- Agents --------------------------------------------------------------
    AgentSpawned { agent_id: AgentId, task: String },
    AgentProgress { agent_id: AgentId, fraction: f32, message: String },
    AgentTaskComplete {
        agent_id: AgentId,
        success: bool,
        summary: String,
        /// Reconciler spawn tag echoed back so the brain can route the
        /// completion to the correct `active_dispatches` entry.
        spawn_tag: Option<u64>,
    },
    AgentError { agent_id: AgentId, error: String },

    // -- Sessions / Focus ----------------------------------------------------
    SessionSwitched { from: SessionId, to: SessionId },
    FocusChanged { from: Option<AppId>, to: Option<AppId> },

    // -- Brain / NLP ---------------------------------------------------------
    BrainDecision { action: String, confidence: f32 },
    NlpInterpreted { input: String, action: String },

    // -- Video / FX ----------------------------------------------------------
    VideoPlaybackStateChanged { app_id: AppId, playing: bool },
    GlitchFxTriggered { origin: [f32; 2], intensity: f32 },

    // -- System --------------------------------------------------------------
    MemoryPressure { bytes_free: usize },
    JobCompleted { job_id: JobId },
    Shutdown,

    // -- Extension / Plugin --------------------------------------------------
    /// Escape hatch for plugins and forward-compat.
    Custom { kind: String, data: String },
}

/// Coarse topic categories for routing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EventTopic {
    Terminal,
    Agents,
    Sessions,
    Brain,
    Video,
    Fx,
    System,
    Custom,
}

impl Event {
    /// Returns the topic this event belongs to.
    pub fn topic(&self) -> EventTopic {
        match self {
            Self::TerminalOutput { .. }
            | Self::CommandStarted { .. }
            | Self::CommandComplete { .. } => EventTopic::Terminal,

            Self::AgentSpawned { .. }
            | Self::AgentProgress { .. }
            | Self::AgentTaskComplete { .. }
            | Self::AgentError { .. } => EventTopic::Agents,

            Self::SessionSwitched { .. } | Self::FocusChanged { .. } => EventTopic::Sessions,

            Self::BrainDecision { .. } | Self::NlpInterpreted { .. } => EventTopic::Brain,

            Self::VideoPlaybackStateChanged { .. } => EventTopic::Video,

            Self::GlitchFxTriggered { .. } => EventTopic::Fx,

            Self::MemoryPressure { .. } | Self::JobCompleted { .. } | Self::Shutdown => {
                EventTopic::System
            }

            Self::Custom { .. } => EventTopic::Custom,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_events_have_terminal_topic() {
        let events = [
            Event::TerminalOutput { app_id: 1, bytes: 100 },
            Event::CommandStarted { app_id: 1, command: "ls".into() },
            Event::CommandComplete { app_id: 1, exit_code: 0 },
        ];
        for event in &events {
            assert_eq!(event.topic(), EventTopic::Terminal);
        }
    }

    #[test]
    fn agent_events_have_agents_topic() {
        let events = [
            Event::AgentSpawned { agent_id: 1, task: "fix".into() },
            Event::AgentProgress {
                agent_id: 1,
                fraction: 0.5,
                message: "working".into(),
            },
            Event::AgentTaskComplete {
                agent_id: 1,
                success: true,
                summary: "done".into(),
                spawn_tag: None,
            },
            Event::AgentError { agent_id: 1, error: "oops".into() },
        ];
        for event in &events {
            assert_eq!(event.topic(), EventTopic::Agents);
        }
    }

    #[test]
    fn system_events_have_system_topic() {
        let events = [
            Event::MemoryPressure { bytes_free: 1024 },
            Event::JobCompleted { job_id: 42 },
            Event::Shutdown,
        ];
        for event in &events {
            assert_eq!(event.topic(), EventTopic::System);
        }
    }

    #[test]
    fn custom_event_has_custom_topic() {
        let event = Event::Custom { kind: "plugin.foo".into(), data: "{}".into() };
        assert_eq!(event.topic(), EventTopic::Custom);
    }

    #[test]
    fn event_is_clone() {
        let event = Event::CommandComplete { app_id: 1, exit_code: 0 };
        let _cloned = event.clone();
    }

    #[test]
    fn event_is_debug() {
        let event = Event::Shutdown;
        let _s = format!("{event:?}");
    }

    #[test]
    fn all_topic_variants_covered() {
        // Ensure we have events for every topic.
        let topics = [
            Event::TerminalOutput { app_id: 0, bytes: 0 }.topic(),
            Event::AgentSpawned { agent_id: 0, task: String::new() }.topic(),
            Event::SessionSwitched { from: 0, to: 0 }.topic(),
            Event::BrainDecision { action: String::new(), confidence: 0.0 }.topic(),
            Event::VideoPlaybackStateChanged { app_id: 0, playing: false }.topic(),
            Event::GlitchFxTriggered { origin: [0.0, 0.0], intensity: 0.0 }.topic(),
            Event::Shutdown.topic(),
            Event::Custom { kind: String::new(), data: String::new() }.topic(),
        ];
        assert_eq!(topics.len(), 8);
    }
}
