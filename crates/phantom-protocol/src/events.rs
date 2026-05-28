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

/// Identifies which fast-path lifecycle gate an agent took.
///
/// Issue #648 — used by [`Event::FastPathTaken`] so consumers can
/// distinguish auto-approve (Queued → Working skipping AwaitingApproval)
/// from a future skip-planning fast path without string-matching the
/// `reason` field.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FastPathKind {
    /// `Agent::try_auto_approve` — the `auto_approve()` disposition fast
    /// path that bypasses `Planning → AwaitingApproval`.
    AutoApprove,
    /// `policy.skip_planning` — reserved for the loop-overseer audit work
    /// once a production reader exists (issue #648 defers wiring this).
    SkipPlanning,
}

/// Typed event variants for the Phantom event bus.
#[derive(Clone, Debug)]
pub enum Event {
    // -- Terminal / PTY ------------------------------------------------------
    TerminalOutput { app_id: AppId, bytes: u64 },
    CommandStarted { app_id: AppId, command: String },
    CommandComplete { app_id: AppId, exit_code: i32 },

    /// A subprocess has taken over the terminal (rising edge).
    ///
    /// Emitted by `TerminalAdapter` on the **first frame** the takeover
    /// condition becomes true. Consumers (#366, #367) use this to render
    /// tethers and, eventually, to split the pane (issue #368/#369 — not in
    /// this PR).
    ///
    /// # Lineage-model contract (issue #365)
    ///
    /// `app_id` is the *parent* terminal pane. `pgid` is the foreground
    /// process group, which the lineage model can use to build a parent→child
    /// edge. The `process_name` field provides the human-readable label.
    SubprocessTakeoverDetected {
        /// The parent terminal pane that spawned the subprocess.
        app_id: AppId,
        /// Human-readable process name (e.g. `"vim"`, `"htop"`).
        /// `None` when the name could not be resolved.
        process_name: Option<String>,
        /// Foreground process-group ID from `TIOCGPGRP`.
        /// `None` when the ioctl failed.
        pgid: Option<i32>,
        /// Whether the detection was caused by alt-screen entry or a
        /// known-program name match.
        alt_screen: bool,
    },

    /// A previously detected subprocess takeover has ended (falling edge).
    ///
    /// Emitted on the frame the takeover condition clears (e.g. the user quits
    /// vim and the alt-screen is restored). Consumers use this to remove
    /// tethers and collapse secondary panes.
    SubprocessTakeoverCleared {
        /// The parent terminal pane whose takeover just ended.
        app_id: AppId,
    },

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
        /// Issue #646 spike: typed result payload supplied by the agent
        /// through `complete_task`, when the agent was spawned with
        /// `requires_complete_task = true`.
        ///
        /// Phase 1 (this spike) preserves the JSON object end-to-end;
        /// Phase 2 binds it to a per-task JSON schema.
        ///
        /// `None` for agents that do not opt into `requires_complete_task`,
        /// for legacy state-implicit terminations, and for `AgentError` →
        /// `AgentTaskComplete { success: false, .. }` synthesis paths.
        result: Option<serde_json::Value>,
    },
    AgentError { agent_id: AgentId, error: String },

    /// Issue #648: a fast-path lifecycle gate was bypassed.
    ///
    /// Emitted by `Agent::try_auto_approve_with_audit` (and any future
    /// fast-path entry point) so consumers — inspector pane, brain
    /// reconciler, policy auditors — can see *why* the normal
    /// `Planning → AwaitingApproval` gate was skipped. The variant is fired
    /// even when the FSM transition fails, so observers also catch the
    /// case where a fast-path was *attempted* but refused (`kind` tells
    /// which fast-path was requested; the audit log envelope carries the
    /// `approved` boolean).
    FastPathTaken {
        /// Which agent took (or attempted) the fast path.
        agent_id: AgentId,
        /// Which fast-path the agent used.
        kind: FastPathKind,
        /// Short human-readable explanation (e.g. the disposition name,
        /// or `"disposition <X> is not auto-approvable"` on refusal).
        reason: String,
    },

    // -- Sessions / Focus ----------------------------------------------------
    SessionSwitched { from: SessionId, to: SessionId },
    FocusChanged { from: Option<AppId>, to: Option<AppId> },

    // -- Brain / NLP ---------------------------------------------------------
    BrainDecision { action: String, confidence: f32 },
    NlpInterpreted { input: String, action: String },

    // -- Video / FX ----------------------------------------------------------
    VideoPlaybackStateChanged { app_id: AppId, playing: bool },
    /// Emitted by the capture pipeline after a frame passes the perceptual-hash
    /// dedup gate and is accepted into the open bundle. Issue #79 item 7.
    ///
    /// PNG bytes are NOT included — consumers should read from the bundle store.
    FrameCaptured { pane_id: AppId, timestamp_ms: u64 },
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

/// Classification of an event for the subagent isolation contract.
///
/// A subagent (spawned with `AgentSpawnOpts::with_subagent(true)`) reports
/// **upward only** to the parent orchestrator. The emit boundary in
/// `phantom_agents::subagent_emit` reads `Event::class()` and drops anything
/// that is not [`EventClass::UpwardReport`], incrementing a per-agent
/// suppressed-emit counter.
///
/// Non-subagents bypass the gate entirely; their emit path is unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EventClass {
    /// Agent-to-parent status: `AgentTaskComplete`, `AgentError`,
    /// `AgentProgress`. The only class a subagent may emit.
    UpwardReport,
    /// Peer-bus traffic — terminal output, focus changes, brain telemetry,
    /// video, custom plugin events. Subagents are blocked from emitting
    /// these.
    Lateral,
    /// Host-level lifecycle and infrastructure: memory pressure, job
    /// completion, shutdown, agent-spawn telemetry, fast-path audit.
    /// Subagents are blocked from emitting these.
    Internal,
}

impl Event {
    /// Returns the topic this event belongs to.
    #[must_use]
    pub fn topic(&self) -> EventTopic {
        match self {
            Self::TerminalOutput { .. }
            | Self::CommandStarted { .. }
            | Self::CommandComplete { .. }
            | Self::SubprocessTakeoverDetected { .. }
            | Self::SubprocessTakeoverCleared { .. } => EventTopic::Terminal,

            Self::AgentSpawned { .. }
            | Self::AgentProgress { .. }
            | Self::AgentTaskComplete { .. }
            | Self::AgentError { .. }
            | Self::FastPathTaken { .. } => EventTopic::Agents,

            Self::SessionSwitched { .. } | Self::FocusChanged { .. } => EventTopic::Sessions,

            Self::BrainDecision { .. } | Self::NlpInterpreted { .. } => EventTopic::Brain,

            Self::VideoPlaybackStateChanged { .. } | Self::FrameCaptured { .. } => EventTopic::Video,

            Self::GlitchFxTriggered { .. } => EventTopic::Fx,

            Self::MemoryPressure { .. } | Self::JobCompleted { .. } | Self::Shutdown => {
                EventTopic::System
            }

            Self::Custom { .. } => EventTopic::Custom,
        }
    }

    /// Returns the class of this event for the subagent isolation contract.
    ///
    /// See [`EventClass`] for the full contract. The summary:
    ///
    /// - `AgentTaskComplete`, `AgentError`, `AgentProgress` are upward
    ///   reports — the parent orchestrator surface.
    /// - `AgentSpawned`, `FastPathTaken`, host-level lifecycle events
    ///   (`MemoryPressure`, `JobCompleted`, `Shutdown`) are internal.
    /// - Everything else (terminal output, focus, brain telemetry, video,
    ///   sessions, custom plugin events) is lateral peer-bus traffic.
    #[must_use]
    pub fn class(&self) -> EventClass {
        match self {
            // Upward reports — agent → parent orchestrator.
            Self::AgentTaskComplete { .. }
            | Self::AgentError { .. }
            | Self::AgentProgress { .. } => EventClass::UpwardReport,

            // Internal — host/infra lifecycle, not a peer-bus message.
            Self::AgentSpawned { .. }
            | Self::FastPathTaken { .. }
            | Self::MemoryPressure { .. }
            | Self::JobCompleted { .. }
            | Self::Shutdown => EventClass::Internal,

            // Lateral — everything else on the peer bus.
            Self::TerminalOutput { .. }
            | Self::CommandStarted { .. }
            | Self::CommandComplete { .. }
            | Self::SubprocessTakeoverDetected { .. }
            | Self::SubprocessTakeoverCleared { .. }
            | Self::SessionSwitched { .. }
            | Self::FocusChanged { .. }
            | Self::BrainDecision { .. }
            | Self::NlpInterpreted { .. }
            | Self::VideoPlaybackStateChanged { .. }
            | Self::FrameCaptured { .. }
            | Self::GlitchFxTriggered { .. }
            | Self::Custom { .. } => EventClass::Lateral,
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
    fn subprocess_takeover_detected_has_terminal_topic() {
        let event = Event::SubprocessTakeoverDetected {
            app_id: 7,
            process_name: Some("vim".into()),
            pgid: Some(1234),
            alt_screen: true,
        };
        assert_eq!(event.topic(), EventTopic::Terminal);
    }

    #[test]
    fn subprocess_takeover_cleared_has_terminal_topic() {
        let event = Event::SubprocessTakeoverCleared { app_id: 7 };
        assert_eq!(event.topic(), EventTopic::Terminal);
    }

    #[test]
    fn subprocess_takeover_detected_is_clone_and_debug() {
        let event = Event::SubprocessTakeoverDetected {
            app_id: 3,
            process_name: None,
            pgid: None,
            alt_screen: false,
        };
        let cloned = event.clone();
        let s = format!("{cloned:?}");
        assert!(s.contains("SubprocessTakeoverDetected"));
    }

    #[test]
    fn subprocess_takeover_cleared_is_clone_and_debug() {
        let event = Event::SubprocessTakeoverCleared { app_id: 5 };
        let cloned = event.clone();
        let s = format!("{cloned:?}");
        assert!(s.contains("SubprocessTakeoverCleared"));
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
                result: None,
            },
            Event::AgentError { agent_id: 1, error: "oops".into() },
        ];
        for event in &events {
            assert_eq!(event.topic(), EventTopic::Agents);
        }
    }

    #[test]
    fn fast_path_taken_has_agents_topic() {
        // Issue #648: the new fast-path audit variant must route on the
        // same topic as the other agent lifecycle events so existing
        // subscribers pick it up without rewiring.
        let event = Event::FastPathTaken {
            agent_id: 42,
            kind: FastPathKind::AutoApprove,
            reason: "Disposition::Chat is auto-approvable".into(),
        };
        assert_eq!(event.topic(), EventTopic::Agents);
    }

    #[test]
    fn fast_path_taken_is_clone_and_debug() {
        let event = Event::FastPathTaken {
            agent_id: 7,
            kind: FastPathKind::SkipPlanning,
            reason: "policy.skip_planning=true".into(),
        };
        let cloned = event.clone();
        let s = format!("{cloned:?}");
        assert!(s.contains("FastPathTaken"));
        assert!(s.contains("SkipPlanning"));
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

    // -- EventClass mapping ----------------------------------------------

    #[test]
    fn agent_task_complete_classifies_as_upward_report() {
        let event = Event::AgentTaskComplete {
            agent_id: 1,
            success: true,
            summary: "done".into(),
            spawn_tag: None,
            result: None,
        };
        assert_eq!(event.class(), EventClass::UpwardReport);
    }

    #[test]
    fn agent_error_classifies_as_upward_report() {
        let event = Event::AgentError { agent_id: 1, error: "boom".into() };
        assert_eq!(event.class(), EventClass::UpwardReport);
    }

    #[test]
    fn agent_progress_classifies_as_upward_report() {
        let event = Event::AgentProgress {
            agent_id: 1,
            fraction: 0.5,
            message: "halfway".into(),
        };
        assert_eq!(event.class(), EventClass::UpwardReport);
    }

    #[test]
    fn command_started_classifies_as_lateral() {
        let event = Event::CommandStarted { app_id: 1, command: "ls".into() };
        assert_eq!(event.class(), EventClass::Lateral);
    }

    #[test]
    fn terminal_output_classifies_as_lateral() {
        let event = Event::TerminalOutput { app_id: 1, bytes: 100 };
        assert_eq!(event.class(), EventClass::Lateral);
    }

    #[test]
    fn shutdown_classifies_as_internal() {
        assert_eq!(Event::Shutdown.class(), EventClass::Internal);
    }

    #[test]
    fn memory_pressure_classifies_as_internal() {
        let event = Event::MemoryPressure { bytes_free: 1 };
        assert_eq!(event.class(), EventClass::Internal);
    }

    #[test]
    fn agent_spawned_classifies_as_internal() {
        let event = Event::AgentSpawned { agent_id: 1, task: "x".into() };
        assert_eq!(event.class(), EventClass::Internal);
    }

    #[test]
    fn fast_path_taken_classifies_as_internal() {
        let event = Event::FastPathTaken {
            agent_id: 1,
            kind: FastPathKind::AutoApprove,
            reason: "fast".into(),
        };
        assert_eq!(event.class(), EventClass::Internal);
    }

    #[test]
    fn custom_event_classifies_as_lateral() {
        let event = Event::Custom { kind: "k".into(), data: "d".into() };
        assert_eq!(event.class(), EventClass::Lateral);
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
            Event::FrameCaptured { pane_id: 0, timestamp_ms: 0 }.topic(),
            Event::GlitchFxTriggered { origin: [0.0, 0.0], intensity: 0.0 }.topic(),
            Event::Shutdown.topic(),
            Event::Custom { kind: String::new(), data: String::new() }.topic(),
        ];
        assert_eq!(topics.len(), 9);
    }
}
