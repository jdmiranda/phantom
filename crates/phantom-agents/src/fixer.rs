//! Fixer-on-blockage substrate hook (Phase 2.E).
//!
//! When an agent emits [`EventKind::AgentBlocked`], the substrate auto-spawns
//! a short-lived [`AgentRole::Fixer`] whose job is to read the blockage
//! context, propose a fix, write a memory note, and die. The original blocked
//! agent (or a consent-gated [`AgentRole::Actor`]) is responsible for actually
//! applying the fix to the user's world.
//!
//! ## Lifecycle
//!
//! 1. Agent A hits a wall (missing dep, unparseable error, ambiguous spec).
//! 2. A emits `AgentBlocked { agent_id: A, reason: "..." }`.
//! 3. The spawn-rule registry, holding [`fixer_spawn_rule`], fires
//!    [`SpawnAction::SpawnIfNotRunning`] for [`AgentRole::Fixer`].
//! 4. The Fixer triages the blockage, writes a memory note, exits.
//! 5. A consults the memory note and resumes (or escalates to the user).
//!
//! ## Why `SpawnIfNotRunning`?
//!
//! An agent can emit multiple `AgentBlocked` events for the same blockage as
//! it retries — we want at most one Fixer per blockage at a time. The
//! idempotent spawn variant is the cheapest way to enforce that without the
//! rule layer needing to know about agent identity.
//!
//! ## Suggested `payload` shape for `AgentBlocked`
//!
//! Producers of the event SHOULD populate `SubstrateEvent.payload` with:
//!
//! ```json
//! {
//!   "agent_id": 42,
//!   "agent_role": "Composer",
//!   "reason": "missing tool: web_search",
//!   "blocked_at_unix_ms": 1714200000000,
//!   "context_excerpt": "...last 200 chars of the agent's transcript...",
//!   "suggested_capability": "Sense"
//! }
//! ```
//!
//! Only `reason` is mandatory at the rule layer; the rest is convention so
//! downstream Fixer instances see a consistent input shape.

use crate::role::AgentRole;
use crate::spawn_rules::{KindPattern, SpawnRule};

/// Default deadline (in seconds) for a Fixer agent. The Fixer is meant to be
/// short-lived: triage, propose, write memory, die. Five minutes is generous
/// for an LLM round-trip with tool calls.
pub const DEFAULT_FIXER_TTL_SECS: u64 = 5 * 60;

/// Build the canonical spawn rule that fires a [`AgentRole::Fixer`] whenever
/// any other agent emits [`crate::spawn_rules::EventKind::AgentBlocked`].
///
/// Uses [`SpawnAction::SpawnIfNotRunning`] so repeated `AgentBlocked` events
/// for the same agent don't multiply Fixers. The label template is
/// `"fixer-on-blockage"`; substitute at spawn time if the substrate wants
/// per-agent labels.
///
/// The default deadline is [`DEFAULT_FIXER_TTL_SECS`]; callers wanting a
/// different TTL should override the returned rule's `spawn.params` payload
/// before registration.
pub fn fixer_spawn_rule() -> SpawnRule {
    SpawnRule::on_any(KindPattern::AnyAgentBlocked)
        .spawn_if_not_running(AgentRole::Fixer, "fixer-on-blockage")
}

/// Build a deadline payload for a Fixer spawn.
///
/// The returned JSON object carries `expires_at_unix_ms`, computed as
/// `now_unix_ms + ttl_secs * 1000`. Consumers (the agent manager, the Fixer's
/// own watchdog) should self-terminate when wall-clock time exceeds that
/// instant.
pub fn fixer_deadline_payload(now_unix_ms: u64, ttl_secs: u64) -> serde_json::Value {
    let expires_at_unix_ms = now_unix_ms.saturating_add(ttl_secs.saturating_mul(1_000));
    serde_json::json!({
        "expires_at_unix_ms": expires_at_unix_ms,
        "ttl_secs": ttl_secs,
        "issued_at_unix_ms": now_unix_ms,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::role::CapabilityClass;
    use crate::spawn_rules::{
        EventKind, EventSource, SpawnAction, SpawnRuleRegistry, SubstrateEvent,
    };

    fn ev(kind: EventKind) -> SubstrateEvent {
        SubstrateEvent {
            kind,
            payload: serde_json::Value::Null,
            source: EventSource::Substrate,
        }
    }

    #[test]
    fn fixer_role_manifest_has_no_act_capability() {
        // Load-bearing security property: a Fixer cannot mutate the user's
        // world. It proposes; the original blocked agent (or a consent-gated
        // Actor) applies.
        assert!(!AgentRole::Fixer.has(CapabilityClass::Act));
    }

    #[test]
    fn fixer_role_manifest_has_compute_for_llm_use() {
        // Sanity: the Fixer needs an LLM round-trip to actually reason about
        // the blockage and propose a fix.
        assert!(AgentRole::Fixer.has(CapabilityClass::Compute));
    }

    #[test]
    fn fixer_spawn_rule_fires_on_agent_blocked() {
        let reg = SpawnRuleRegistry::new().add(fixer_spawn_rule());
        let actions = reg.evaluate(&ev(EventKind::AgentBlocked {
            agent_id: 42,
            reason: "missing tool: web_search".to_string(),
        }));
        assert_eq!(actions.len(), 1);
        match actions[0] {
            SpawnAction::SpawnIfNotRunning { role, label_template, .. } => {
                assert_eq!(*role, AgentRole::Fixer);
                assert_eq!(label_template, "fixer-on-blockage");
            }
            other => panic!("expected SpawnIfNotRunning(Fixer), got {other:?}"),
        }
    }

    #[test]
    fn fixer_spawn_rule_does_not_fire_on_pane_opened() {
        // Negative: an unrelated event must not spawn a Fixer.
        let reg = SpawnRuleRegistry::new().add(fixer_spawn_rule());
        let actions = reg.evaluate(&ev(EventKind::PaneOpened {
            app_type: "terminal".to_string(),
        }));
        assert!(actions.is_empty());
    }

    #[test]
    fn fixer_spawn_rule_uses_spawn_if_not_running() {
        // Document the policy: only one Fixer per blockage. If an agent emits
        // multiple `AgentBlocked` events while retrying, we don't want N
        // Fixers piling up. `SpawnIfNotRunning` is the substrate-level
        // idempotency guarantee that enforces this.
        let rule = fixer_spawn_rule();
        match rule.spawn {
            SpawnAction::SpawnIfNotRunning { role, .. } => {
                assert_eq!(role, AgentRole::Fixer);
            }
            SpawnAction::Spawn { .. } => {
                panic!(
                    "fixer_spawn_rule must use SpawnIfNotRunning to avoid \
                     piling up duplicate Fixers on retry storms"
                );
            }
        }
    }

    #[test]
    fn fixer_deadline_payload_serializes_unix_seconds() {
        let now = 1_714_200_000_000_u64; // arbitrary plausible epoch ms
        let ttl = 300_u64;
        let payload = fixer_deadline_payload(now, ttl);

        let expires = payload
            .get("expires_at_unix_ms")
            .and_then(|v| v.as_u64())
            .expect("expires_at_unix_ms must be a u64");
        assert!(
            expires > now,
            "expires_at_unix_ms ({expires}) must be strictly greater than now_unix_ms ({now})"
        );
        assert_eq!(expires, now + ttl * 1_000);

        // The other fields are convention but worth pinning so consumers can
        // rely on them.
        assert_eq!(payload.get("ttl_secs").and_then(|v| v.as_u64()), Some(ttl));
        assert_eq!(
            payload.get("issued_at_unix_ms").and_then(|v| v.as_u64()),
            Some(now)
        );
    }
}
