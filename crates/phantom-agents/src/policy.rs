//! Per-agent execution policy.
//!
//! [`AgentPolicy`] replaces the global `DEFAULT_STALL_TIMEOUT` constant and
//! any hardcoded retry limits in the reconciler. Every [`crate::Agent`] carries
//! its own policy so different task types can have different time budgets and
//! retry allowances without touching shared constants.
//!
//! # Defaults
//!
//! The [`Default`] implementation deliberately matches the values that were
//! previously hardcoded globally:
//!
//! | Field             | Default | Former constant              |
//! |-------------------|---------|------------------------------|
//! | `max_attempts`    | `3`     | `PlanStep::max_attempts = 3` |
//! | `timeout_seconds` | `1800`  | `DEFAULT_STALL_TIMEOUT` (30-min orchestration rule) |
//! | `auto_approve`    | `false` | n/a                          |
//! | `skip_planning`   | `false` | n/a                          |

/// Execution policy attached to every [`crate::Agent`].
///
/// The reconciler reads `timeout_seconds` to detect stalled dispatches and
/// `max_attempts` as the per-step retry ceiling. Setting these per-agent lets
/// long-running tasks (e.g. a full workspace rebuild) opt out of the short
/// global timeout without affecting quick chat agents.
#[derive(Debug, Clone)]
pub struct AgentPolicy {
    /// Maximum number of execution attempts before the step is marked Failed.
    ///
    /// Maps directly to `PlanStep::max_attempts` — the reconciler reads this
    /// value when it dispatches the step so the ledger uses the agent's own
    /// retry budget rather than a shared constant.
    pub max_attempts: u32,

    /// How many seconds an agent may be active before the reconciler considers
    /// it stalled and records a failure (allowing retry up to `max_attempts`).
    ///
    /// Default: 1 800 s (30 minutes), matching the CLAUDE.md orchestration rule
    /// "any implementation agent running for more than 30 minutes should be
    /// checked on".
    pub timeout_seconds: u64,

    /// When `true`, the agent skips the `AwaitingApproval` gate and goes
    /// `Queued → Working` directly. Mirrors [`crate::dispatch::Disposition::auto_approve`]
    /// but expressed as a policy override independent of the task's disposition.
    pub auto_approve: bool,

    /// When `true`, the agent skips the `Planning` phase entirely and begins
    /// executing tools immediately from `Queued → Working`.
    pub skip_planning: bool,
}

impl Default for AgentPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            timeout_seconds: 1800,
            auto_approve: false,
            skip_planning: false,
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
    fn default_policy_matches_former_global_values() {
        let p = AgentPolicy::default();
        assert_eq!(p.max_attempts, 3, "max_attempts must default to 3");
        assert_eq!(p.timeout_seconds, 1800, "timeout_seconds must default to 1800");
        assert!(!p.auto_approve, "auto_approve must default to false");
        assert!(!p.skip_planning, "skip_planning must default to false");
    }

    #[test]
    fn policy_fields_are_independently_mutable() {
        let mut p = AgentPolicy::default();
        p.max_attempts = 5;
        p.timeout_seconds = 600;
        p.auto_approve = true;
        p.skip_planning = true;

        assert_eq!(p.max_attempts, 5);
        assert_eq!(p.timeout_seconds, 600);
        assert!(p.auto_approve);
        assert!(p.skip_planning);
    }

    #[test]
    fn policy_clone_is_independent() {
        let original = AgentPolicy::default();
        let mut cloned = original.clone();
        cloned.max_attempts = 99;
        // Original must not be affected by mutation of the clone.
        assert_eq!(
            AgentPolicy::default().max_attempts,
            3,
            "cloning must produce an independent copy"
        );
        assert_eq!(cloned.max_attempts, 99);
    }
}
