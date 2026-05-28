//! Subagent reports-up-only isolation contract.
//!
//! A subagent (an [`crate::agent::Agent`] spawned with
//! [`crate::agent::AgentSpawnOpts::with_subagent(true)`]) is allowed to emit
//! only upward report events to its parent orchestrator. Lateral and internal
//! events are dropped at the emit boundary, counted, and logged at `warn`.
//!
//! The gate is a thin wrapper. Non-subagents bypass the check entirely and
//! their emit path is unchanged. The mirror of Claude Code's subagent
//! contract: subagents report up only.
//!
//! See [`phantom_protocol::EventClass`] for the classification rules and
//! `crates/phantom-protocol/src/events.rs::Event::class` for the per-variant
//! mapping.
//!
//! # Usage
//!
//! ```ignore
//! use phantom_agents::agent::{AgentSpawnOpts, AgentTask};
//! use phantom_agents::subagent_emit::SubagentEmitGuard;
//! use phantom_protocol::Event;
//!
//! let opts = AgentSpawnOpts::new(AgentTask::FreeForm { prompt: "t".into() })
//!     .with_subagent(true);
//! let mut guard = SubagentEmitGuard::from_opts(&opts);
//!
//! let ev = Event::AgentTaskComplete {
//!     agent_id: 1,
//!     success: true,
//!     summary: "ok".into(),
//!     spawn_tag: None,
//!     result: None,
//! };
//! assert!(guard.try_emit(&ev));
//! assert_eq!(guard.suppressed_lateral_emits(), 0);
//! ```

use phantom_protocol::{Event, EventClass};

use crate::agent::AgentSpawnOpts;

/// Per-agent emit gate enforcing the subagent reports-up-only contract.
///
/// Construct one per agent via [`Self::from_opts`] and call
/// [`Self::try_emit`] before pushing an event onto the bus. A `false` return
/// means the event was dropped and the suppressed-emit counter ticked up;
/// the caller MUST NOT publish the event.
///
/// For non-subagent agents the gate is a pass-through: `try_emit` always
/// returns `true` and the counter stays at zero.
#[derive(Debug, Clone)]
pub struct SubagentEmitGuard {
    /// Whether the owning agent is a subagent. Mirrors
    /// [`AgentSpawnOpts::subagent`].
    subagent: bool,
    /// Count of lateral / internal events the gate dropped during the
    /// agent's lifetime. Reset only when the guard is dropped.
    ///
    /// Named for the dominant case (lateral peer-bus emits). Internal-class
    /// blocks are counted in the same bucket because the contract treats
    /// both the same: a subagent must not emit them.
    suppressed_lateral_emits: u64,
}

impl SubagentEmitGuard {
    /// Build a guard whose mode mirrors [`AgentSpawnOpts::subagent`].
    #[must_use]
    pub fn from_opts(opts: &AgentSpawnOpts) -> Self {
        Self {
            subagent: opts.subagent(),
            suppressed_lateral_emits: 0,
        }
    }

    /// Build a guard with explicit subagent state. Useful for tests and for
    /// callers that do not own an [`AgentSpawnOpts`].
    #[must_use]
    pub fn new(subagent: bool) -> Self {
        Self {
            subagent,
            suppressed_lateral_emits: 0,
        }
    }

    /// Returns whether the gate is in subagent mode.
    #[must_use]
    pub fn is_subagent(&self) -> bool {
        self.subagent
    }

    /// Returns the count of events the gate has dropped during this agent's
    /// lifetime.
    #[must_use]
    pub fn suppressed_lateral_emits(&self) -> u64 {
        self.suppressed_lateral_emits
    }

    /// Decide whether `ev` may be emitted by the owning agent.
    ///
    /// Returns `true` when the caller should publish the event. Returns
    /// `false` when the event was dropped, in which case the counter has
    /// been incremented and a `warn`-level log line has been written.
    ///
    /// The path is panic-free. A blocked emit is observable through
    /// [`Self::suppressed_lateral_emits`] and the `tracing` log only.
    pub fn try_emit(&mut self, ev: &Event) -> bool {
        if !self.subagent {
            // Non-subagent path is unchanged.
            return true;
        }
        match ev.class() {
            EventClass::UpwardReport => true,
            class @ (EventClass::Lateral | EventClass::Internal) => {
                self.suppressed_lateral_emits =
                    self.suppressed_lateral_emits.saturating_add(1);
                tracing::warn!(
                    target: "phantom_agents::subagent_emit",
                    event_class = ?class,
                    suppressed_total = self.suppressed_lateral_emits,
                    "subagent emit blocked by reports-up-only contract"
                );
                false
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
    use crate::agent::{AgentSpawnOpts, AgentTask};

    fn task_complete() -> Event {
        Event::AgentTaskComplete {
            agent_id: 7,
            success: true,
            summary: "done".into(),
            spawn_tag: None,
            result: None,
        }
    }

    fn lateral_command_started() -> Event {
        Event::CommandStarted { app_id: 1, command: "ls".into() }
    }

    fn internal_shutdown() -> Event {
        Event::Shutdown
    }

    #[test]
    fn subagent_allows_upward_report() {
        let opts = AgentSpawnOpts::new(AgentTask::FreeForm { prompt: "p".into() })
            .with_subagent(true);
        let mut guard = SubagentEmitGuard::from_opts(&opts);
        assert!(guard.is_subagent());
        assert!(guard.try_emit(&task_complete()));
        assert_eq!(guard.suppressed_lateral_emits(), 0);
    }

    #[test]
    fn subagent_blocks_lateral_and_counts() {
        let mut guard = SubagentEmitGuard::new(true);
        let ev = lateral_command_started();
        assert!(!guard.try_emit(&ev));
        assert_eq!(guard.suppressed_lateral_emits(), 1);
        // A second attempt increments the counter again.
        assert!(!guard.try_emit(&ev));
        assert_eq!(guard.suppressed_lateral_emits(), 2);
    }

    #[test]
    fn subagent_blocks_internal_and_counts() {
        let mut guard = SubagentEmitGuard::new(true);
        assert!(!guard.try_emit(&internal_shutdown()));
        assert_eq!(guard.suppressed_lateral_emits(), 1);
    }

    #[test]
    fn non_subagent_passes_every_class_freely() {
        // Default spawn opts have `subagent = false`. The gate must be a
        // total pass-through and the counter must stay at zero.
        let opts = AgentSpawnOpts::new(AgentTask::FreeForm { prompt: "p".into() });
        let mut guard = SubagentEmitGuard::from_opts(&opts);
        assert!(!guard.is_subagent());

        assert!(guard.try_emit(&task_complete()));
        assert!(guard.try_emit(&lateral_command_started()));
        assert!(guard.try_emit(&internal_shutdown()));

        assert_eq!(guard.suppressed_lateral_emits(), 0);
    }

    #[test]
    fn default_opts_are_not_subagent() {
        let opts = AgentSpawnOpts::new(AgentTask::FreeForm { prompt: "p".into() });
        assert!(!opts.subagent());
    }

    #[test]
    fn with_subagent_toggles_field() {
        let opts = AgentSpawnOpts::new(AgentTask::FreeForm { prompt: "p".into() })
            .with_subagent(true);
        assert!(opts.subagent());
        let opts2 = opts.with_subagent(false);
        assert!(!opts2.subagent());
    }
}
