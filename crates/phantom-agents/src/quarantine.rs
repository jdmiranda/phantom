//! Sec.7.3 — QuarantineState + auto-quarantine policy.
//!
//! Auto-quarantines repeat-offender agents and denies their tool dispatches.
//!
//! ## State machine
//!
//! Each agent moves through three states:
//!
//! ```text
//!   Clean ──(N consecutive Tainted checks)──► Quarantined
//!     ▲                                              │
//!     └────────────(Clean check resets counter)──────┘
//! ```
//!
//! - [`QuarantineState::Clean`]: the default. No pending offenses recorded.
//! - [`QuarantineState::Suspect`]: at least one `Tainted` check has occurred
//!   since the last reset, but the threshold has not been reached.
//! - [`QuarantineState::Quarantined`]: the agent has hit the threshold. All
//!   tool dispatches are denied until the state is released.
//!
//! ## Policy
//!
//! [`AutoQuarantinePolicy`] configures:
//! - `threshold` — number of consecutive `TaintLevel::Tainted` checks before
//!   escalation (default 3).
//!
//! A `TaintLevel::Clean` check resets the consecutive counter back to zero
//! (the agent earns back trust). `TaintLevel::Suspect` does **not** advance
//! or reset the counter — it is a soft signal and is ignored for quarantine
//! purposes.
//!
//! ## Registry
//!
//! [`QuarantineRegistry`] is the single owner of the `AgentId →
//! QuarantineState` map. The dispatch gate calls
//! [`QuarantineRegistry::agent_is_quarantined`] to implement the fast
//! query path, and [`QuarantineRegistry::check_and_escalate`] to record
//! taint observations and drive state transitions.
//!
//! The registry is `Send + Sync`-safe and intended to be shared via
//! `Arc<Mutex<QuarantineRegistry>>` at the orchestrator boundary — the
//! same pattern used by [`crate::inbox::AgentRegistry`].

use std::collections::HashMap;

use crate::role::AgentId;
use crate::taint::TaintLevel;

// ---------------------------------------------------------------------------
// Defaults
// ---------------------------------------------------------------------------

/// Number of consecutive `TaintLevel::Tainted` observations before an agent
/// is automatically quarantined.
///
/// Three matches the `NotificationCenter` threshold from Sec.8 — the same
/// pattern that fires a banner also quarantines the offender.
pub const DEFAULT_QUARANTINE_THRESHOLD: usize = 3;

// ---------------------------------------------------------------------------
// QuarantineState
// ---------------------------------------------------------------------------

/// Lifecycle state for a single agent in the quarantine registry.
///
/// Ordered from safest to most restricted:
/// `Clean` → `Suspect` → `Quarantined`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuarantineState {
    /// No recorded offenses. The default; most agents never leave this state.
    Clean,
    /// At least one consecutive `Tainted` observation, but below the
    /// escalation threshold. The agent can still dispatch tools; it is
    /// being watched.
    Suspect {
        /// Number of consecutive `TaintLevel::Tainted` checks since the
        /// last reset. Always in `1..threshold`.
        consecutive_tainted: usize,
    },
    /// Threshold breached. All tool dispatches are denied until the state
    /// is explicitly released (or the registry entry is removed).
    Quarantined {
        /// Human-readable reason, usually the triggering taint context.
        reason: String,
        /// Wall-clock milliseconds at the moment of quarantine. Populated
        /// by the caller (passed from the orchestrator's `now_ms` clock)
        /// so we don't take a `SystemTime` dependency inside the registry.
        since_ms: u64,
    },
}

impl QuarantineState {
    /// Returns `true` iff the agent is in the [`Quarantined`](Self::Quarantined) state.
    ///
    /// `Suspect` is *not* quarantined — the agent can still dispatch tools.
    #[must_use]
    pub fn is_quarantined(&self) -> bool {
        matches!(self, Self::Quarantined { .. })
    }
}

// ---------------------------------------------------------------------------
// AutoQuarantinePolicy
// ---------------------------------------------------------------------------

/// Policy knobs for the auto-quarantine state machine.
///
/// Passed to [`QuarantineRegistry::new_with_policy`]. The default (via
/// [`AutoQuarantinePolicy::default`]) uses [`DEFAULT_QUARANTINE_THRESHOLD`].
#[derive(Debug, Clone)]
pub struct AutoQuarantinePolicy {
    /// Number of *consecutive* `TaintLevel::Tainted` observations required
    /// to escalate to [`QuarantineState::Quarantined`].
    ///
    /// Must be ≥ 1. Panics on construction if set to 0.
    pub threshold: usize,
}

impl Default for AutoQuarantinePolicy {
    fn default() -> Self {
        Self {
            threshold: DEFAULT_QUARANTINE_THRESHOLD,
        }
    }
}

// ---------------------------------------------------------------------------
// Per-agent tracking slot
// ---------------------------------------------------------------------------

/// Internal tracking slot per agent. Kept separate from [`QuarantineState`]
/// so the public state enum stays clean (no `consecutive_tainted` leaking
/// into `Quarantined`).
#[derive(Debug)]
struct AgentSlot {
    /// Current quarantine lifecycle state for this agent.
    state: QuarantineState,
    /// Consecutive `Tainted` count, incremented on each `Tainted`
    /// observation and reset to 0 on a `Clean` observation.
    ///
    /// Mirrors the value stored in `Suspect { consecutive_tainted }` for the
    /// state-machine transitions; kept on the slot so we don't have to
    /// match into the state every time.
    consecutive_tainted: usize,
}

impl AgentSlot {
    fn new() -> Self {
        Self {
            state: QuarantineState::Clean,
            consecutive_tainted: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// QuarantineRegistry
// ---------------------------------------------------------------------------

/// Tracks `AgentId → QuarantineState` and drives auto-quarantine transitions.
///
/// Owned by the orchestrator and shared via `Arc<Mutex<QuarantineRegistry>>`.
/// All operations are `&mut self` so callers must hold the lock; the registry
/// itself does no locking.
#[derive(Debug, Default)]
pub struct QuarantineRegistry {
    slots: HashMap<AgentId, AgentSlot>,
    policy: AutoQuarantinePolicy,
}

impl QuarantineRegistry {
    /// Create a registry with the default policy
    /// ([`DEFAULT_QUARANTINE_THRESHOLD`] = 3 consecutive `Tainted` checks).
    #[must_use]
    pub fn new() -> Self {
        Self::new_with_policy(AutoQuarantinePolicy::default())
    }

    /// Create a registry with an explicit policy. Panics if
    /// `policy.threshold == 0`.
    ///
    /// # Panics
    ///
    /// Panics when `policy.threshold` is 0 — a threshold of zero would
    /// quarantine every agent on first contact, which is almost certainly a
    /// misconfiguration.
    #[must_use]
    pub fn new_with_policy(policy: AutoQuarantinePolicy) -> Self {
        assert!(policy.threshold >= 1, "quarantine threshold must be >= 1");
        Self {
            slots: HashMap::new(),
            policy,
        }
    }

    /// Returns `true` iff the agent identified by `id` is currently in the
    /// [`QuarantineState::Quarantined`] state.
    ///
    /// Returns `false` for unknown agents — agents that have never been
    /// observed are `Clean` by default.
    #[must_use]
    pub fn agent_is_quarantined(&self, id: AgentId) -> bool {
        self.slots
            .get(&id)
            .map(|slot| slot.state.is_quarantined())
            .unwrap_or(false)
    }

    /// Return a snapshot of the current [`QuarantineState`] for `id`.
    ///
    /// Unknown agents are implicitly [`QuarantineState::Clean`].
    #[must_use]
    pub fn state_of(&self, id: AgentId) -> QuarantineState {
        self.slots
            .get(&id)
            .map(|slot| slot.state.clone())
            .unwrap_or(QuarantineState::Clean)
    }

    /// Record a taint observation for `id` and apply the policy state machine.
    ///
    /// ## Transitions
    ///
    /// | Current state  | Observation | Next state                                   |
    /// |----------------|-------------|----------------------------------------------|
    /// | Any            | `Clean`     | `Clean` (counter reset)                      |
    /// | Any            | `Suspect`   | unchanged (soft signal, ignored)             |
    /// | `Clean`        | `Tainted`   | `Suspect { consecutive_tainted: 1 }`         |
    /// | `Suspect { n }`| `Tainted`   | `Suspect { n+1 }` or `Quarantined` if n+1 ≥ threshold |
    /// | `Quarantined`  | `Tainted`   | `Quarantined` (already at maximum)           |
    ///
    /// ## Arguments
    ///
    /// - `id` — the agent being observed.
    /// - `taint` — the [`TaintLevel`] from the dispatch gate.
    /// - `now_ms` — wall-clock milliseconds, stamped into the `Quarantined`
    ///   state on escalation. Callers should pass the orchestrator's
    ///   monotonic-ish clock value (e.g. from `SystemTime`).
    /// - `reason` — short human-readable string embedded in the
    ///   `Quarantined` state. Callers should give context (tool name, etc.).
    ///
    /// ## Returns
    ///
    /// `true` iff this call caused a transition to `Quarantined` (i.e. the
    /// agent just crossed the threshold). Callers can use this to emit a
    /// notification or log the event.
    pub fn check_and_escalate(
        &mut self,
        id: AgentId,
        taint: TaintLevel,
        now_ms: u64,
        reason: impl Into<String>,
    ) -> bool {
        match taint {
            // Clean observation → reset the consecutive counter for this agent.
            // Already-quarantined agents stay quarantined — Clean resets the
            // *counter* but cannot release a quarantine.
            TaintLevel::Clean => {
                if let Some(slot) = self.slots.get_mut(&id) {
                    if !slot.state.is_quarantined() {
                        slot.consecutive_tainted = 0;
                        slot.state = QuarantineState::Clean;
                    }
                }
                // If no slot yet: agent is already Clean by default — no-op.
                false
            }

            // Suspect is a soft signal — ignored for quarantine purposes.
            TaintLevel::Suspect => false,

            // Tainted → advance the consecutive counter; escalate if threshold met.
            TaintLevel::Tainted => {
                let slot = self.slots.entry(id).or_insert_with(AgentSlot::new);

                // Already quarantined: nothing to do.
                if slot.state.is_quarantined() {
                    return false;
                }

                slot.consecutive_tainted += 1;

                if slot.consecutive_tainted >= self.policy.threshold {
                    slot.state = QuarantineState::Quarantined {
                        reason: reason.into(),
                        since_ms: now_ms,
                    };
                    true // newly quarantined
                } else {
                    slot.state = QuarantineState::Suspect {
                        consecutive_tainted: slot.consecutive_tainted,
                    };
                    false
                }
            }
        }
    }

    /// Forcibly release a quarantine for `id`, resetting it to `Clean`.
    ///
    /// The consecutive counter is also reset. Intended for manual operator
    /// intervention or TTL expiry logic sitting above this registry.
    pub fn release(&mut self, id: AgentId) {
        if let Some(slot) = self.slots.get_mut(&id) {
            slot.consecutive_tainted = 0;
            slot.state = QuarantineState::Clean;
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_000_000;

    /// N-1 consecutive Tainted observations must NOT quarantine the agent.
    ///
    /// With the default threshold of 3, two `Tainted` checks put the agent
    /// in `Suspect` but not `Quarantined`.
    #[test]
    fn n_minus_one_taints_do_not_quarantine() {
        let mut reg = QuarantineRegistry::new();
        let agent_id = 1u64;
        let threshold = DEFAULT_QUARANTINE_THRESHOLD; // 3

        // Apply threshold-1 consecutive Tainted observations.
        for i in 0..(threshold - 1) {
            let escalated = reg.check_and_escalate(
                agent_id,
                TaintLevel::Tainted,
                NOW,
                format!("offense {}", i + 1),
            );
            assert!(
                !escalated,
                "check {}/{}: must not escalate before threshold",
                i + 1,
                threshold
            );
        }

        assert!(
            !reg.agent_is_quarantined(agent_id),
            "agent must not be quarantined before threshold"
        );

        // State should be Suspect with consecutive count = threshold-1.
        let state = reg.state_of(agent_id);
        assert!(
            matches!(
                state,
                QuarantineState::Suspect {
                    consecutive_tainted
                } if consecutive_tainted == threshold - 1
            ),
            "expected Suspect {{ consecutive_tainted: {} }}, got {:?}",
            threshold - 1,
            state
        );
    }

    /// Exactly N consecutive Tainted observations MUST quarantine the agent.
    ///
    /// The Nth call to `check_and_escalate` must return `true` (newly
    /// quarantined) and the registry must report the agent as quarantined.
    #[test]
    fn n_taints_quarantine_agent() {
        let mut reg = QuarantineRegistry::new();
        let agent_id = 2u64;
        let threshold = DEFAULT_QUARANTINE_THRESHOLD; // 3

        // Apply threshold-1 checks — should not yet escalate.
        for i in 0..(threshold - 1) {
            let escalated = reg.check_and_escalate(
                agent_id,
                TaintLevel::Tainted,
                NOW,
                format!("offense {}", i + 1),
            );
            assert!(
                !escalated,
                "premature escalation at check {}",
                i + 1,
            );
        }

        // The Nth check must cross the threshold and return true.
        let escalated = reg.check_and_escalate(
            agent_id,
            TaintLevel::Tainted,
            NOW,
            "final offense",
        );
        assert!(escalated, "Nth Tainted check must return true (newly quarantined)");

        // The agent must now be quarantined.
        assert!(
            reg.agent_is_quarantined(agent_id),
            "agent must be quarantined after N Tainted observations"
        );

        // The state must be Quarantined with the correct reason and timestamp.
        let state = reg.state_of(agent_id);
        match state {
            QuarantineState::Quarantined { reason, since_ms } => {
                assert_eq!(since_ms, NOW);
                assert_eq!(reason, "final offense");
            }
            other => panic!("expected Quarantined, got {:?}", other),
        }
    }

    /// A `Clean` taint observation resets the consecutive counter.
    ///
    /// After N-1 Tainted checks, a single Clean check must reset the counter
    /// so that the agent needs another full run of N Tainted checks to
    /// quarantine.
    #[test]
    fn clean_taint_resets_consecutive_counter() {
        let mut reg = QuarantineRegistry::new();
        let agent_id = 3u64;
        let threshold = DEFAULT_QUARANTINE_THRESHOLD; // 3

        // Advance to threshold-1 (Suspect).
        for i in 0..(threshold - 1) {
            reg.check_and_escalate(
                agent_id,
                TaintLevel::Tainted,
                NOW,
                format!("offense {}", i + 1),
            );
        }

        // Confirm we're in Suspect.
        assert!(
            matches!(reg.state_of(agent_id), QuarantineState::Suspect { .. }),
            "expected Suspect before Clean reset"
        );

        // One Clean check resets the counter.
        let escalated = reg.check_and_escalate(agent_id, TaintLevel::Clean, NOW, "clean");
        assert!(!escalated, "Clean check must never return true");

        assert!(
            !reg.agent_is_quarantined(agent_id),
            "agent must not be quarantined after Clean reset"
        );
        assert_eq!(
            reg.state_of(agent_id),
            QuarantineState::Clean,
            "state must be Clean after Clean reset"
        );

        // Now we need another full N Tainted checks to quarantine.
        for i in 0..(threshold - 1) {
            let escalated = reg.check_and_escalate(
                agent_id,
                TaintLevel::Tainted,
                NOW,
                format!("re-offense {}", i + 1),
            );
            assert!(
                !escalated,
                "re-offense {} must not yet escalate (counter reset by Clean)",
                i + 1
            );
        }
        assert!(
            !reg.agent_is_quarantined(agent_id),
            "agent must still need one more Tainted after Clean reset"
        );

        // The final Tainted check quarantines.
        let escalated = reg.check_and_escalate(
            agent_id,
            TaintLevel::Tainted,
            NOW + 1,
            "re-offense final",
        );
        assert!(escalated, "final re-offense must quarantine");
        assert!(reg.agent_is_quarantined(agent_id));
    }

    /// A `Quarantined` agent is reported by `agent_is_quarantined`.
    ///
    /// Explicit verification that the query method returns `true` for
    /// a quarantined agent and `false` for clean / unknown agents.
    #[test]
    fn quarantined_agent_is_reported_by_is_quarantined() {
        let mut reg = QuarantineRegistry::new();
        let quarantined_id = 10u64;
        let clean_id = 20u64;
        let unknown_id = 99u64;
        let threshold = DEFAULT_QUARANTINE_THRESHOLD;

        // Quarantine agent 10.
        for _ in 0..threshold {
            reg.check_and_escalate(quarantined_id, TaintLevel::Tainted, NOW, "offense");
        }

        // Record a Clean observation for agent 20 (no slot → Clean by default).
        reg.check_and_escalate(clean_id, TaintLevel::Clean, NOW, "irrelevant");

        assert!(
            reg.agent_is_quarantined(quarantined_id),
            "quarantined agent must be reported as quarantined"
        );
        assert!(
            !reg.agent_is_quarantined(clean_id),
            "clean agent must not be reported as quarantined"
        );
        assert!(
            !reg.agent_is_quarantined(unknown_id),
            "unknown agent must not be reported as quarantined"
        );
    }

    /// Suspect taint does NOT advance the consecutive counter.
    ///
    /// N-1 Tainted + 1 Suspect + 1 more Tainted = N total but only N-1
    /// consecutive Tainted, so no quarantine yet.
    #[test]
    fn suspect_taint_does_not_advance_counter() {
        let mut reg = QuarantineRegistry::new();
        let agent_id = 5u64;
        let threshold = DEFAULT_QUARANTINE_THRESHOLD; // 3

        // 2 Tainted (threshold-1 = 2).
        for _ in 0..(threshold - 1) {
            reg.check_and_escalate(agent_id, TaintLevel::Tainted, NOW, "offense");
        }

        // 1 Suspect — must not advance the counter.
        let escalated = reg.check_and_escalate(agent_id, TaintLevel::Suspect, NOW, "suspect");
        assert!(!escalated, "Suspect must not escalate");

        // Should still be Suspect with the same counter.
        let state = reg.state_of(agent_id);
        assert!(
            matches!(
                state,
                QuarantineState::Suspect {
                    consecutive_tainted
                } if consecutive_tainted == threshold - 1
            ),
            "counter must not change after Suspect, expected {}, got {:?}",
            threshold - 1,
            state
        );

        assert!(
            !reg.agent_is_quarantined(agent_id),
            "agent must not be quarantined after Suspect (counter unchanged)"
        );
    }

    /// `release` resets a quarantined agent back to Clean.
    #[test]
    fn release_clears_quarantine() {
        let mut reg = QuarantineRegistry::new();
        let agent_id = 7u64;
        let threshold = DEFAULT_QUARANTINE_THRESHOLD;

        for _ in 0..threshold {
            reg.check_and_escalate(agent_id, TaintLevel::Tainted, NOW, "offense");
        }
        assert!(reg.agent_is_quarantined(agent_id), "must be quarantined before release");

        reg.release(agent_id);
        assert!(
            !reg.agent_is_quarantined(agent_id),
            "must not be quarantined after release"
        );
        assert_eq!(
            reg.state_of(agent_id),
            QuarantineState::Clean,
            "must be Clean after release"
        );
    }

    /// Custom threshold of N=1: first Tainted observation quarantines immediately.
    #[test]
    fn custom_threshold_one_quarantines_immediately() {
        let policy = AutoQuarantinePolicy { threshold: 1 };
        let mut reg = QuarantineRegistry::new_with_policy(policy);
        let agent_id = 8u64;

        let escalated = reg.check_and_escalate(agent_id, TaintLevel::Tainted, NOW, "instant");
        assert!(escalated, "threshold=1: first Tainted must quarantine");
        assert!(reg.agent_is_quarantined(agent_id));
    }

    /// Already-quarantined agent: additional Tainted checks do NOT return true
    /// (the agent is already at maximum restriction).
    #[test]
    fn additional_taints_on_quarantined_agent_return_false() {
        let mut reg = QuarantineRegistry::new();
        let agent_id = 9u64;
        let threshold = DEFAULT_QUARANTINE_THRESHOLD;

        // Quarantine the agent.
        for _ in 0..threshold {
            reg.check_and_escalate(agent_id, TaintLevel::Tainted, NOW, "offense");
        }
        assert!(reg.agent_is_quarantined(agent_id));

        // Additional Tainted checks on an already-quarantined agent.
        for _ in 0..5 {
            let escalated = reg.check_and_escalate(
                agent_id,
                TaintLevel::Tainted,
                NOW + 100,
                "more offenses",
            );
            assert!(
                !escalated,
                "already-quarantined agent must not trigger another escalation"
            );
        }

        // Still quarantined with original state.
        match reg.state_of(agent_id) {
            QuarantineState::Quarantined { reason, since_ms } => {
                assert_eq!(since_ms, NOW, "since_ms must not change after further Tainted checks");
                // Reason was set on the Nth (threshold) check, which was "offense".
                assert_eq!(reason, "offense");
            }
            other => panic!("must remain Quarantined, got {:?}", other),
        }
    }
}
