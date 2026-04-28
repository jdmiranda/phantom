//! Typed spawn-rule registry for ambient agents.
//!
//! Declarative "when SubstrateEvent X (with role Y), spawn AgentRole Z" rules.
//! The model is Bevy-inspired: events flow through a registry of predicates,
//! and each matching rule emits a [`SpawnAction`] without performing any
//! mutation itself. The caller decides whether to honor the action (e.g. by
//! consulting the agent manager for already-running instances when the action
//! is `SpawnIfNotRunning`).
//!
//! ## Design
//!
//! - Rules are declared up-front via the fluent builder ([`SpawnRule::on`]).
//! - Matching is split into two stages: a coarse [`KindPattern`] check, then
//!   an optional fine-grained predicate. This keeps the hot path cheap.
//! - Predicates are stored as plain function pointers (`fn(&SubstrateEvent)
//!   -> bool`) rather than `Box<dyn Fn>`. Rationale:
//!     1. **`Send + Sync` for free.** Function pointers are unconditionally
//!        thread-safe, so the registry can be shared across the Watcher /
//!        Composer threads without `Arc<Mutex<...>>` ceremony.
//!     2. **No heap allocation per rule.** Important when the substrate
//!        registers dozens of ambient rules at startup.
//!     3. **Spawn rules are static policy, not closures over runtime state.**
//!        If a rule needs runtime context, that belongs in the event payload
//!        (`SubstrateEvent::payload`), not captured in the predicate.
//!     4. **Cheap `Clone` and `Debug`.** Function pointers are `Copy`, so the
//!        whole [`EventMatcher`] is trivially cloneable.
//!   The trade-off is no captured state in predicates — which is by design.

use serde::{Deserialize, Serialize};

use crate::role::{AgentRole, CapabilityClass};

// ---------------------------------------------------------------------------
// Event types
// ---------------------------------------------------------------------------

/// A taxonomy of substrate-level events that spawn rules can react to.
///
/// `Custom(String)` is the escape hatch for plugin-defined events that don't
/// fit one of the well-known kinds.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EventKind {
    PaneOpened { app_type: String },
    PaneClosed { app_type: String },
    AgentSpawned { role: AgentRole },
    AgentExited { role: AgentRole, success: bool },
    /// An agent has signalled that it cannot make forward progress and needs
    /// help. The substrate's canonical reaction is to spawn a [`AgentRole::Fixer`]
    /// to triage the blockage. `agent_id` identifies the blocked agent;
    /// `reason` is a short, human-readable summary suitable for the Fixer's
    /// system prompt.
    AgentBlocked { agent_id: u64, reason: String },
    /// The Layer-2 dispatch gate refused a tool call because the calling
    /// agent's role manifest does not include the tool's
    /// [`CapabilityClass`]. The model already saw the
    /// [`crate::tools::DispatchError::CapabilityDenied`] in its
    /// `tool_result` block; this event is the parallel substrate-side
    /// signal so the runtime can record, surface, and react (e.g. spawn a
    /// Defender). `source_chain` is the chain of substrate event ids that
    /// led to this dispatch; empty until Sec.2 wires provenance.
    CapabilityDenied {
        agent_id: u64,
        role: AgentRole,
        attempted_class: CapabilityClass,
        attempted_tool: String,
        source_chain: Vec<u64>,
    },
    AudioStreamAvailable,
    VideoStreamAvailable,
    UserCommandSubmitted,
    Custom(String),
}

/// A concrete event flowing through the substrate.
///
/// `payload` carries event-specific data (e.g. the pane id, the command
/// string) and is the place to put data that predicates need at match time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubstrateEvent {
    pub kind: EventKind,
    pub payload: serde_json::Value,
    pub source: EventSource,
}

/// Who emitted the event. Useful for filtering "ignore events I caused".
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum EventSource {
    Substrate,
    Agent { role: AgentRole },
    User,
}

// ---------------------------------------------------------------------------
// Spawn actions
// ---------------------------------------------------------------------------

/// What a matching rule asks the agent manager to do.
///
/// `SpawnIfNotRunning` is the idempotent variant — useful for ambient
/// watchers where you want at most one instance per role.
#[derive(Debug, Clone)]
pub enum SpawnAction {
    Spawn {
        role: AgentRole,
        label_template: String,
        params: serde_json::Value,
    },
    SpawnIfNotRunning {
        role: AgentRole,
        label_template: String,
        params: serde_json::Value,
    },
}

// ---------------------------------------------------------------------------
// Matchers
// ---------------------------------------------------------------------------

/// A coarse-grained event-shape filter, used when an exact `EventKind` would
/// be too narrow (e.g. "any pane opened, regardless of `app_type`").
#[derive(Debug, Clone)]
pub enum KindPattern {
    Exact(EventKind),
    AnyPaneOpened,
    AnyAgentExited,
    /// Matches any [`EventKind::AgentBlocked`], regardless of the blocked
    /// agent's id or the reason string. The canonical pattern for binding the
    /// Fixer spawn rule.
    AnyAgentBlocked,
    /// Matches any [`EventKind::CapabilityDenied`], regardless of the
    /// agent id, role, attempted class, or tool name. The canonical pattern
    /// for binding the Defender spawn rule (Sec.4).
    CapabilityDenied,
    Any,
}

impl KindPattern {
    /// Whether this pattern admits the given event kind.
    fn matches(&self, kind: &EventKind) -> bool {
        match (self, kind) {
            (KindPattern::Exact(want), got) => want == got,
            (KindPattern::AnyPaneOpened, EventKind::PaneOpened { .. }) => true,
            (KindPattern::AnyAgentExited, EventKind::AgentExited { .. }) => true,
            (KindPattern::AnyAgentBlocked, EventKind::AgentBlocked { .. }) => true,
            (KindPattern::CapabilityDenied, EventKind::CapabilityDenied { .. }) => true,
            (KindPattern::Any, _) => true,
            _ => false,
        }
    }
}

/// Public helper that returns the [`KindPattern::CapabilityDenied`] pattern.
///
/// Sec.4 (Defender) registers its spawn rule via:
/// `SpawnRule::on_any(capability_denied_pattern()).spawn_if_not_running(Defender, "defender-on-denial")`
///
/// Exposing this as a function (rather than asking callers to write
/// `KindPattern::CapabilityDenied` directly) keeps the substrate's public
/// API symmetric with the existing pattern helpers and makes the binding
/// site self-documenting.
#[must_use]
pub fn capability_denied_pattern() -> KindPattern {
    KindPattern::CapabilityDenied
}

/// Encapsulates the match logic for a single rule.
///
/// Variants:
/// - `OfKind` — exact `EventKind` equality (the common case).
/// - `KindAndPredicate` — coarse pattern plus a fine-grained predicate.
/// - `Always` — pattern only, no predicate.
#[derive(Clone)]
pub enum EventMatcher {
    OfKind(EventKind),
    KindAndPredicate {
        kind_pattern: KindPattern,
        predicate: fn(&SubstrateEvent) -> bool,
    },
    Always(KindPattern),
}

impl EventMatcher {
    fn matches(&self, ev: &SubstrateEvent) -> bool {
        match self {
            EventMatcher::OfKind(want) => &ev.kind == want,
            EventMatcher::KindAndPredicate { kind_pattern, predicate } => {
                kind_pattern.matches(&ev.kind) && predicate(ev)
            }
            EventMatcher::Always(pattern) => pattern.matches(&ev.kind),
        }
    }
}

// ---------------------------------------------------------------------------
// Rules and registry
// ---------------------------------------------------------------------------

/// One declarative rule: a matcher and the action to take when it fires.
pub struct SpawnRule {
    pub when: EventMatcher,
    pub spawn: SpawnAction,
}

impl SpawnRule {
    /// Begin building a rule that fires on an exact `EventKind`.
    pub fn on(kind: EventKind) -> SpawnRuleBuilder {
        SpawnRuleBuilder {
            matcher_seed: MatcherSeed::Exact(kind),
            predicate: None,
        }
    }

    /// Begin building a rule that fires on any event matching the pattern.
    pub fn on_any(pattern: KindPattern) -> SpawnRuleBuilder {
        SpawnRuleBuilder {
            matcher_seed: MatcherSeed::Pattern(pattern),
            predicate: None,
        }
    }
}

/// Fluent builder for [`SpawnRule`]. Constructed via `SpawnRule::on(...)` or
/// `SpawnRule::on_any(...)`, optionally narrowed with `with_predicate`, then
/// terminated with `spawn` / `spawn_if_not_running`.
pub struct SpawnRuleBuilder {
    matcher_seed: MatcherSeed,
    predicate: Option<fn(&SubstrateEvent) -> bool>,
}

/// Internal: tracks how the builder was opened so we can pick the right
/// `EventMatcher` variant at finalization.
enum MatcherSeed {
    Exact(EventKind),
    Pattern(KindPattern),
}

impl SpawnRuleBuilder {
    /// Add a fine-grained predicate. Calling this on an `on(...)` builder
    /// promotes the matcher from `OfKind` to `KindAndPredicate` (with an
    /// `Exact` pattern preserving the original kind).
    pub fn with_predicate(mut self, pred: fn(&SubstrateEvent) -> bool) -> Self {
        self.predicate = Some(pred);
        self
    }

    /// Finish building with a `Spawn` action.
    pub fn spawn(self, role: AgentRole, label_template: impl Into<String>) -> SpawnRule {
        SpawnRule {
            when: self.build_matcher(),
            spawn: SpawnAction::Spawn {
                role,
                label_template: label_template.into(),
                params: serde_json::Value::Null,
            },
        }
    }

    /// Finish building with a `SpawnIfNotRunning` action.
    pub fn spawn_if_not_running(self, role: AgentRole, label_template: impl Into<String>) -> SpawnRule {
        SpawnRule {
            when: self.build_matcher(),
            spawn: SpawnAction::SpawnIfNotRunning {
                role,
                label_template: label_template.into(),
                params: serde_json::Value::Null,
            },
        }
    }

    fn build_matcher(self) -> EventMatcher {
        match (self.matcher_seed, self.predicate) {
            (MatcherSeed::Exact(kind), None) => EventMatcher::OfKind(kind),
            (MatcherSeed::Exact(kind), Some(pred)) => EventMatcher::KindAndPredicate {
                kind_pattern: KindPattern::Exact(kind),
                predicate: pred,
            },
            (MatcherSeed::Pattern(pat), None) => EventMatcher::Always(pat),
            (MatcherSeed::Pattern(pat), Some(pred)) => EventMatcher::KindAndPredicate {
                kind_pattern: pat,
                predicate: pred,
            },
        }
    }
}

/// Holds a set of [`SpawnRule`]s and evaluates events against them.
///
/// Construction is builder-style so the substrate can declare its rule set
/// in a single expression at startup.
pub struct SpawnRuleRegistry {
    rules: Vec<SpawnRule>,
}

impl SpawnRuleRegistry {
    /// Empty registry. No rules, no spawns.
    pub fn new() -> Self {
        Self { rules: Vec::new() }
    }

    /// Append a rule, returning `self` for fluent composition.
    pub fn add(mut self, rule: SpawnRule) -> Self {
        self.rules.push(rule);
        self
    }

    /// Evaluate `ev` against every registered rule, returning references to
    /// every matching action in declaration order. Pure — never mutates the
    /// registry.
    pub fn evaluate(&self, ev: &SubstrateEvent) -> Vec<&SpawnAction> {
        let mut out = Vec::new();
        for rule in &self.rules {
            if rule.when.matches(ev) {
                out.push(&rule.spawn);
            }
        }
        out
    }

    /// Number of registered rules. Mostly useful for tests and diagnostics.
    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }
}

impl Default for SpawnRuleRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(kind: EventKind) -> SubstrateEvent {
        SubstrateEvent {
            kind,
            payload: serde_json::Value::Null,
            source: EventSource::Substrate,
        }
    }

    #[test]
    fn empty_registry_evaluates_to_no_spawns() {
        let reg = SpawnRuleRegistry::new();
        let actions = reg.evaluate(&ev(EventKind::AudioStreamAvailable));
        assert!(actions.is_empty());
        assert_eq!(reg.rule_count(), 0);
    }

    #[test]
    fn exact_kind_match_triggers_spawn() {
        let trigger = EventKind::PaneOpened { app_type: "video".to_string() };
        let reg = SpawnRuleRegistry::new().add(SpawnRule {
            when: EventMatcher::OfKind(trigger.clone()),
            spawn: SpawnAction::Spawn {
                role: AgentRole::Watcher,
                label_template: "video-watch".to_string(),
                params: serde_json::Value::Null,
            },
        });

        let actions = reg.evaluate(&ev(trigger));
        assert_eq!(actions.len(), 1);
        match actions[0] {
            SpawnAction::Spawn { role, label_template, .. } => {
                assert_eq!(*role, AgentRole::Watcher);
                assert_eq!(label_template, "video-watch");
            }
            other => panic!("expected Spawn, got {other:?}"),
        }
    }

    #[test]
    fn mismatched_kind_does_not_trigger() {
        let reg = SpawnRuleRegistry::new().add(SpawnRule {
            when: EventMatcher::OfKind(EventKind::PaneOpened {
                app_type: "video".to_string(),
            }),
            spawn: SpawnAction::Spawn {
                role: AgentRole::Watcher,
                label_template: "x".to_string(),
                params: serde_json::Value::Null,
            },
        });

        // Different app_type — should not match an exact kind.
        let actions = reg.evaluate(&ev(EventKind::PaneOpened {
            app_type: "terminal".to_string(),
        }));
        assert!(actions.is_empty());

        // Entirely different kind — also no match.
        let actions = reg.evaluate(&ev(EventKind::AudioStreamAvailable));
        assert!(actions.is_empty());
    }

    #[test]
    fn pattern_any_pane_opened_matches_terminal_and_video() {
        let reg = SpawnRuleRegistry::new().add(SpawnRule {
            when: EventMatcher::Always(KindPattern::AnyPaneOpened),
            spawn: SpawnAction::Spawn {
                role: AgentRole::Watcher,
                label_template: "pane-watch".to_string(),
                params: serde_json::Value::Null,
            },
        });

        let term = reg.evaluate(&ev(EventKind::PaneOpened {
            app_type: "terminal".to_string(),
        }));
        let video = reg.evaluate(&ev(EventKind::PaneOpened {
            app_type: "video".to_string(),
        }));
        let closed = reg.evaluate(&ev(EventKind::PaneClosed {
            app_type: "terminal".to_string(),
        }));

        assert_eq!(term.len(), 1);
        assert_eq!(video.len(), 1);
        // PaneClosed must NOT be matched by the AnyPaneOpened pattern.
        assert!(closed.is_empty());
    }

    #[test]
    fn predicate_filter_narrows_match() {
        // Only fire on PaneOpened where source is the User (not the substrate).
        fn is_user_initiated(ev: &SubstrateEvent) -> bool {
            matches!(ev.source, EventSource::User)
        }

        let reg = SpawnRuleRegistry::new().add(SpawnRule {
            when: EventMatcher::KindAndPredicate {
                kind_pattern: KindPattern::AnyPaneOpened,
                predicate: is_user_initiated,
            },
            spawn: SpawnAction::Spawn {
                role: AgentRole::Watcher,
                label_template: "user-pane".to_string(),
                params: serde_json::Value::Null,
            },
        });

        let from_substrate = SubstrateEvent {
            kind: EventKind::PaneOpened { app_type: "terminal".to_string() },
            payload: serde_json::Value::Null,
            source: EventSource::Substrate,
        };
        assert!(reg.evaluate(&from_substrate).is_empty());

        let from_user = SubstrateEvent {
            kind: EventKind::PaneOpened { app_type: "terminal".to_string() },
            payload: serde_json::Value::Null,
            source: EventSource::User,
        };
        assert_eq!(reg.evaluate(&from_user).len(), 1);
    }

    #[test]
    fn multiple_rules_accumulate() {
        let trigger = EventKind::AudioStreamAvailable;
        let reg = SpawnRuleRegistry::new()
            .add(SpawnRule {
                when: EventMatcher::OfKind(trigger.clone()),
                spawn: SpawnAction::Spawn {
                    role: AgentRole::Capturer,
                    label_template: "audio-capture".to_string(),
                    params: serde_json::Value::Null,
                },
            })
            .add(SpawnRule {
                when: EventMatcher::OfKind(trigger.clone()),
                spawn: SpawnAction::Spawn {
                    role: AgentRole::Transcriber,
                    label_template: "audio-transcribe".to_string(),
                    params: serde_json::Value::Null,
                },
            });

        let actions = reg.evaluate(&ev(trigger));
        assert_eq!(actions.len(), 2);

        let roles: Vec<AgentRole> = actions
            .iter()
            .map(|a| match a {
                SpawnAction::Spawn { role, .. } | SpawnAction::SpawnIfNotRunning { role, .. } => *role,
            })
            .collect();
        assert_eq!(roles, vec![AgentRole::Capturer, AgentRole::Transcriber]);
    }

    #[test]
    fn evaluate_does_not_mutate_registry() {
        let reg = SpawnRuleRegistry::new().add(SpawnRule {
            when: EventMatcher::Always(KindPattern::Any),
            spawn: SpawnAction::Spawn {
                role: AgentRole::Watcher,
                label_template: "anything".to_string(),
                params: serde_json::Value::Null,
            },
        });

        let event = ev(EventKind::UserCommandSubmitted);
        let first = reg.evaluate(&event);
        let second = reg.evaluate(&event);

        assert_eq!(first.len(), second.len());
        assert_eq!(first.len(), 1);
        // Rule count stable (registry is immutable from evaluate's POV).
        assert_eq!(reg.rule_count(), 1);

        // Same action both times.
        match (first[0], second[0]) {
            (
                SpawnAction::Spawn { role: r1, label_template: l1, .. },
                SpawnAction::Spawn { role: r2, label_template: l2, .. },
            ) => {
                assert_eq!(r1, r2);
                assert_eq!(l1, l2);
            }
            _ => panic!("expected matching Spawn actions"),
        }
    }

    #[test]
    fn builder_produces_registered_rule() {
        // End-to-end fluent API: on(...) -> with_predicate(...) -> spawn(...).
        fn always_true(_: &SubstrateEvent) -> bool { true }

        let rule_a = SpawnRule::on(EventKind::VideoStreamAvailable)
            .spawn(AgentRole::Capturer, "video-cap");

        let rule_b = SpawnRule::on_any(KindPattern::AnyAgentExited)
            .with_predicate(always_true)
            .spawn_if_not_running(AgentRole::Reflector, "reflect-on-exit");

        let reg = SpawnRuleRegistry::new().add(rule_a).add(rule_b);
        assert_eq!(reg.rule_count(), 2);

        // First rule fires on its exact kind.
        let video_actions = reg.evaluate(&ev(EventKind::VideoStreamAvailable));
        assert_eq!(video_actions.len(), 1);
        assert!(matches!(
            video_actions[0],
            SpawnAction::Spawn { role: AgentRole::Capturer, .. }
        ));

        // Second rule fires on any AgentExited.
        let exit_actions = reg.evaluate(&ev(EventKind::AgentExited {
            role: AgentRole::Watcher,
            success: true,
        }));
        assert_eq!(exit_actions.len(), 1);
        assert!(matches!(
            exit_actions[0],
            SpawnAction::SpawnIfNotRunning { role: AgentRole::Reflector, .. }
        ));

        // Unrelated event matches neither rule.
        assert!(reg.evaluate(&ev(EventKind::AudioStreamAvailable)).is_empty());
    }

    // ---- Sec.1: CapabilityDenied event variant ------------------------------

    /// The `EventKind::CapabilityDenied` variant must round-trip through
    /// serde with all its payload (agent_id, role, attempted_class,
    /// attempted_tool, source_chain) preserved. Pinning this property
    /// keeps the on-disk `events.jsonl` and any over-the-wire transport
    /// stable across runs.
    #[test]
    fn event_kind_capability_denied_serializes_with_payload() {
        use crate::role::CapabilityClass;

        let original = EventKind::CapabilityDenied {
            agent_id: 42,
            role: AgentRole::Watcher,
            attempted_class: CapabilityClass::Act,
            attempted_tool: "run_command".to_string(),
            source_chain: vec![100, 101, 102],
        };

        let json = serde_json::to_string(&original).expect("serialize");
        let round_tripped: EventKind =
            serde_json::from_str(&json).expect("deserialize");

        assert_eq!(round_tripped, original, "round-trip must preserve all fields");

        match round_tripped {
            EventKind::CapabilityDenied {
                agent_id,
                role,
                attempted_class,
                attempted_tool,
                source_chain,
            } => {
                assert_eq!(agent_id, 42);
                assert_eq!(role, AgentRole::Watcher);
                assert_eq!(attempted_class, CapabilityClass::Act);
                assert_eq!(attempted_tool, "run_command");
                assert_eq!(source_chain, vec![100, 101, 102]);
            }
            other => panic!("expected CapabilityDenied, got {other:?}"),
        }
    }

    /// `KindPattern::CapabilityDenied` matches any
    /// `EventKind::CapabilityDenied` regardless of its inner fields, and
    /// does NOT match unrelated kinds. The `capability_denied_pattern()`
    /// helper returns the same variant — Sec.4 (Defender) will register a
    /// spawn rule via that helper.
    #[test]
    fn kind_pattern_matches_capability_denied() {
        use crate::role::CapabilityClass;

        let pattern = KindPattern::CapabilityDenied;
        // The helper returns the same variant.
        assert!(matches!(
            capability_denied_pattern(),
            KindPattern::CapabilityDenied
        ));

        // Positive: a real CapabilityDenied event matches.
        let denied = EventKind::CapabilityDenied {
            agent_id: 7,
            role: AgentRole::Watcher,
            attempted_class: CapabilityClass::Act,
            attempted_tool: "run_command".to_string(),
            source_chain: Vec::new(),
        };
        assert!(pattern.matches(&denied), "pattern should match denied event");

        // Negative: an unrelated kind must not match.
        let pane_opened = EventKind::PaneOpened {
            app_type: "terminal".to_string(),
        };
        assert!(!pattern.matches(&pane_opened), "pattern must not match PaneOpened");

        // Negative: AgentBlocked is similarly shaped (carries agent_id) but
        // the pattern must distinguish it.
        let blocked = EventKind::AgentBlocked {
            agent_id: 7,
            reason: "stuck".to_string(),
        };
        assert!(!pattern.matches(&blocked), "pattern must not match AgentBlocked");

        // Registry-level integration: a rule built via on_any(pattern) fires
        // exactly once on a CapabilityDenied event.
        let reg = SpawnRuleRegistry::new().add(
            SpawnRule::on_any(capability_denied_pattern())
                .spawn_if_not_running(AgentRole::Watcher, "defender-on-denial"),
        );
        let actions = reg.evaluate(&ev(denied));
        assert_eq!(actions.len(), 1, "rule must fire on CapabilityDenied");
    }
}
