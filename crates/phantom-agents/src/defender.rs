//! Defender-on-denial substrate hook (Sec.4).
//!
//! When the Layer-2 dispatch gate refuses a tool call and emits
//! [`crate::spawn_rules::EventKind::CapabilityDenied`], the substrate
//! auto-spawns a short-lived [`AgentRole::Defender`] whose job is to observe
//! the denial, gather the source chain, and prepare to challenge the
//! offending agent. The challenge tool itself lands in a separate wave
//! (Sec.5); for now the Defender is a passive Sense-only observer that
//! proves the spawn-on-denial wiring is end-to-end.
//!
//! ## Lifecycle
//!
//! 1. Agent A invokes a tool whose [`crate::role::CapabilityClass`] is not
//!    in A's role manifest.
//! 2. The Layer-2 dispatch gate rejects the call and emits
//!    `CapabilityDenied { agent_id: A, role, attempted_class,
//!    attempted_tool, source_chain }`.
//! 3. The spawn-rule registry, holding [`defender_spawn_rule`], fires
//!    [`SpawnAction::SpawnIfNotRunning`] for [`AgentRole::Defender`].
//! 4. The Defender records the denial, follows the source chain, and (in
//!    the next wave) challenges A.
//!
//! ## Why `SpawnIfNotRunning`?
//!
//! A misbehaving or compromised agent can emit many `CapabilityDenied`
//! events in a short window — we want at most one Defender per offender at
//! a time. The idempotent spawn variant is the cheapest enforcement
//! mechanism without the rule layer needing to track agent identity.
//!
//! Mirrors the Fixer's design ([`crate::fixer::fixer_spawn_rule`]) — the
//! two security hooks are deliberately symmetric: Fixer responds to an
//! agent that *can't* progress; Defender responds to an agent that *tried
//! to overstep*.

use crate::role::AgentRole;
use crate::spawn_rules::{KindPattern, SpawnRule};

/// Default deadline (in seconds) for a Defender agent. Like the Fixer, the
/// Defender is meant to be short-lived: observe, gather, challenge, die.
/// Five minutes is generous for an LLM round-trip with tool calls.
pub const DEFAULT_DEFENDER_TTL_SECS: u64 = 5 * 60;

/// Build the canonical spawn rule that fires a [`AgentRole::Defender`] whenever
/// any agent triggers a [`crate::spawn_rules::EventKind::CapabilityDenied`].
///
/// Uses [`SpawnAction::SpawnIfNotRunning`] so a denial-storm from a single
/// offender doesn't multiply Defenders. The label template is
/// `"defender-on-denial"`; substitute at spawn time if the substrate wants
/// per-offender labels.
///
/// The default deadline is [`DEFAULT_DEFENDER_TTL_SECS`]; callers wanting a
/// different TTL should override the returned rule's `spawn.params` payload
/// before registration.
pub fn defender_spawn_rule() -> SpawnRule {
    SpawnRule::on_any(KindPattern::CapabilityDenied)
        .spawn_if_not_running(AgentRole::Defender, "defender-on-denial")
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
    fn defender_role_manifest_has_only_sense_capability() {
        // Load-bearing security property: at this stage a Defender can only
        // observe. The challenge tool (and any matching capability class)
        // lands in Sec.5; until then the Defender must not be able to act,
        // compute, reflect, or coordinate. If this test starts failing it
        // means someone widened the manifest without going through the
        // Sec.5 review.
        let manifest = AgentRole::Defender.manifest();
        assert_eq!(
            manifest.classes,
            &[CapabilityClass::Sense],
            "Defender must be Sense-only until Sec.5 lands the challenge tool"
        );
        assert!(AgentRole::Defender.has(CapabilityClass::Sense));
        assert!(!AgentRole::Defender.has(CapabilityClass::Act));
        assert!(!AgentRole::Defender.has(CapabilityClass::Compute));
        assert!(!AgentRole::Defender.has(CapabilityClass::Reflect));
        assert!(!AgentRole::Defender.has(CapabilityClass::Coordinate));
    }

    #[test]
    fn defender_spawn_rule_fires_on_capability_denied() {
        let reg = SpawnRuleRegistry::new().add(defender_spawn_rule());
        let actions = reg.evaluate(&ev(EventKind::CapabilityDenied {
            agent_id: 42,
            role: AgentRole::Watcher,
            attempted_class: CapabilityClass::Act,
            attempted_tool: "run_command".to_string(),
            source_chain: vec![1, 2, 3],
        }));
        assert_eq!(actions.len(), 1);
        match actions[0] {
            SpawnAction::SpawnIfNotRunning { role, label_template, .. } => {
                assert_eq!(*role, AgentRole::Defender);
                assert_eq!(label_template, "defender-on-denial");
            }
            other => panic!("expected SpawnIfNotRunning(Defender), got {other:?}"),
        }
    }

    #[test]
    fn defender_spawn_rule_does_not_fire_on_agent_blocked() {
        // Negative: the Fixer hook fires on AgentBlocked. The Defender hook
        // must not — the two security paths must remain disjoint or we
        // pile up duplicate agents on every blockage.
        let reg = SpawnRuleRegistry::new().add(defender_spawn_rule());
        let actions = reg.evaluate(&ev(EventKind::AgentBlocked {
            agent_id: 42,
            reason: "stuck".to_string(),
        }));
        assert!(actions.is_empty());
    }

    #[test]
    fn defender_spawn_rule_does_not_fire_on_pane_opened() {
        // Negative: an unrelated event must not spawn a Defender.
        let reg = SpawnRuleRegistry::new().add(defender_spawn_rule());
        let actions = reg.evaluate(&ev(EventKind::PaneOpened {
            app_type: "terminal".to_string(),
        }));
        assert!(actions.is_empty());
    }

    #[test]
    fn defender_spawn_rule_uses_spawn_if_not_running() {
        // Document the policy: only one Defender per offender. A misbehaving
        // agent can hammer denied tool calls; SpawnIfNotRunning is the
        // substrate-level idempotency guarantee that prevents N Defenders
        // from piling up.
        let rule = defender_spawn_rule();
        match rule.spawn {
            SpawnAction::SpawnIfNotRunning { role, .. } => {
                assert_eq!(role, AgentRole::Defender);
            }
            SpawnAction::Spawn { .. } => {
                panic!(
                    "defender_spawn_rule must use SpawnIfNotRunning to avoid \
                     piling up duplicate Defenders on denial storms"
                );
            }
        }
    }
}
