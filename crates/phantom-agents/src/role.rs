//! Agent roles, capability classes, and role manifests.
//!
//! Each agent has a fixed role at spawn. The role declares which **capability
//! classes** it has (Sense, Reflect, Compute, Act, Coordinate) and which
//! specific tool IDs it can invoke. Default-deny: a tool not in the manifest
//! cannot be called by an agent of that role.
//!
//! Static-at-spawn-time. Escalation requires respawn under a different role
//! with explicit user consent. Compromised or misbehaving agents are bounded
//! by their manifest — they cannot grant themselves more capability.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Capability classes
// ---------------------------------------------------------------------------

/// One of five orthogonal capability axes. Tools are tagged with one. Roles
/// declare which they hold.
///
/// - `Sense` — read-only observation of environment (read files, subscribe
///   to pane streams, take screenshots). No side effects on the user's world.
/// - `Reflect` — write to substrate-internal state (memory, event log,
///   embeddings). Not visible to the user's filesystem or the world.
/// - `Compute` — call an LLM, run an embedding model, run a transformation.
///   Costs money/cycles, no side effects.
/// - `Act` — mutate the user's world (write files, run commands, send keys,
///   modify panes, commit git). Requires consent gating.
/// - `Coordinate` — spawn or steer other agents. Meta-capability that
///   composes others; carries effective Act if its children do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CapabilityClass {
    Sense,
    Reflect,
    Compute,
    Act,
    Coordinate,
}

// ---------------------------------------------------------------------------
// Roles
// ---------------------------------------------------------------------------

/// Agent role / archetype. Determines manifest, lifecycle, and spawn path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AgentRole {
    /// Talks to the user. Turn-based loop, full LLM, gated Act via Actor delegation.
    Conversational,
    /// Long-lived stream observer. Subscribes, optionally reasons, never acts.
    Watcher,
    /// Pure I/O. Captures pane frames / audio without LLM.
    Capturer,
    /// Audio chunk in, transcript words out. Single-purpose transform.
    Transcriber,
    /// Periodic memory-stream summarizer. Emits day-notes.
    Reflector,
    /// Substrate plumbing. Maintains vector indexes, derived views.
    Indexer,
    /// Short-lived, scoped Act executor. Spawned with explicit user consent.
    Actor,
    /// Plans + delegates. Spawned by Conversational for multi-step work.
    Composer,
    /// Short-lived, scoped fixer. Spawned when another agent blocks. Reads
    /// the blockage context, proposes a fix, writes a memory note, dies.
    /// Cannot mutate the user's world directly — the original blocked agent
    /// (or an Actor) applies the fix after consent.
    Fixer,
}

impl AgentRole {
    /// Human-readable role name, used in UI badges and the system prompt.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Conversational => "Conversational",
            Self::Watcher => "Watcher",
            Self::Capturer => "Capturer",
            Self::Transcriber => "Transcriber",
            Self::Reflector => "Reflector",
            Self::Indexer => "Indexer",
            Self::Actor => "Actor",
            Self::Composer => "Composer",
            Self::Fixer => "Fixer",
        }
    }

    /// The static manifest declaring this role's capability classes.
    pub fn manifest(&self) -> RoleManifest {
        match self {
            Self::Conversational => RoleManifest {
                role: *self,
                classes: &[
                    CapabilityClass::Sense,
                    CapabilityClass::Reflect,
                    CapabilityClass::Compute,
                    CapabilityClass::Coordinate,
                ],
                description: "Talks to the user. Reads, remembers, plans. Cannot directly mutate \
                              the user's world; spawns Actors with consent for that.",
            },
            Self::Watcher => RoleManifest {
                role: *self,
                classes: &[CapabilityClass::Sense, CapabilityClass::Reflect, CapabilityClass::Compute],
                description: "Long-lived ambient observer. Reads streams, writes memory and \
                              event log. Cannot act on the user's world.",
            },
            Self::Capturer => RoleManifest {
                role: *self,
                classes: &[CapabilityClass::Sense, CapabilityClass::Reflect],
                description: "Pure capture. Screenshots, audio frames. No LLM, no acting.",
            },
            Self::Transcriber => RoleManifest {
                role: *self,
                classes: &[CapabilityClass::Compute, CapabilityClass::Reflect],
                description: "Audio-to-transcript transform. Writes transcripts to substrate.",
            },
            Self::Reflector => RoleManifest {
                role: *self,
                classes: &[CapabilityClass::Sense, CapabilityClass::Reflect, CapabilityClass::Compute],
                description: "Periodically summarizes the memory stream into day-notes.",
            },
            Self::Indexer => RoleManifest {
                role: *self,
                classes: &[CapabilityClass::Sense, CapabilityClass::Reflect],
                description: "Maintains vector indexes over the bundle store. No LLM.",
            },
            Self::Actor => RoleManifest {
                role: *self,
                classes: &[
                    CapabilityClass::Sense,
                    CapabilityClass::Reflect,
                    CapabilityClass::Compute,
                    CapabilityClass::Act,
                ],
                description: "Short-lived mutator. Writes files, runs commands. Spawned only \
                              with explicit user consent in scope.",
            },
            Self::Composer => RoleManifest {
                role: *self,
                classes: &[
                    CapabilityClass::Sense,
                    CapabilityClass::Reflect,
                    CapabilityClass::Compute,
                    CapabilityClass::Coordinate,
                ],
                description: "Plans multi-step work and steers Actors / Watchers. No direct Act.",
            },
            Self::Fixer => RoleManifest {
                role: *self,
                classes: &[
                    CapabilityClass::Sense,
                    CapabilityClass::Reflect,
                    CapabilityClass::Compute,
                ],
                description: "Short-lived, scoped fixer. Spawned when another agent blocks. \
                              Reads the blockage context, proposes a fix, writes a memory note, \
                              dies. Cannot mutate the user's world directly — the original \
                              blocked agent (or an Actor) applies the fix after consent.",
            },
        }
    }

    /// Whether this role declares the given capability class.
    pub fn has(&self, class: CapabilityClass) -> bool {
        self.manifest().classes.contains(&class)
    }
}

// ---------------------------------------------------------------------------
// Role manifest
// ---------------------------------------------------------------------------

/// What a given role can do. Declared statically at compile time.
///
/// The `classes` array is the role's capability allowance. Tools are gated
/// at invocation by intersecting the called tool's class with this slice.
#[derive(Debug, Clone, Copy)]
pub struct RoleManifest {
    pub role: AgentRole,
    pub classes: &'static [CapabilityClass],
    pub description: &'static str,
}

impl RoleManifest {
    /// Format the manifest as a paragraph injectable into the agent's
    /// system prompt. Communicates the role's identity and capability
    /// scope so the model doesn't hallucinate ("I don't have access to…").
    pub fn system_prompt_paragraph(&self, label: &str, id: AgentId) -> String {
        let classes: Vec<&'static str> = self
            .classes
            .iter()
            .map(|c| match c {
                CapabilityClass::Sense => "Sense (observe environment)",
                CapabilityClass::Reflect => "Reflect (write to memory/log)",
                CapabilityClass::Compute => "Compute (call LLM/embed)",
                CapabilityClass::Act => "Act (mutate user's world)",
                CapabilityClass::Coordinate => "Coordinate (spawn/steer other agents)",
            })
            .collect();
        format!(
            "You are agent `{label}` (id={id}, role={role}). \
             {description} \
             Your capabilities: {classes}. \
             If you need a capability you don't have, say so and request \
             escalation — do NOT invent tool calls or claim limits you can't verify.",
            role = self.role.label(),
            description = self.description,
            classes = classes.join(", "),
        )
    }
}

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

/// Stable per-agent identifier assigned at spawn. Never reused.
pub type AgentId = u64;

/// Where an agent came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnSource {
    /// Substrate auto-spawned this agent on a lifecycle event.
    Substrate,
    /// User opened the agent pane / explicitly requested a spawn.
    User,
    /// Another agent (typically Conversational or Composer) delegated to it.
    Agent(AgentId),
}

/// The user-visible reference to an agent. Carried on every emission so the
/// UI can attribute every line, badge every message, and route addressed
/// messages back to the right inbox.
#[derive(Debug, Clone)]
pub struct AgentRef {
    pub id: AgentId,
    pub role: AgentRole,
    pub label: String,
    pub spawned_at_unix_ms: u64,
    pub spawned_by: SpawnSource,
}

impl AgentRef {
    /// Convenience constructor that timestamps now.
    pub fn new(id: AgentId, role: AgentRole, label: impl Into<String>, spawned_by: SpawnSource) -> Self {
        let spawned_at_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        Self {
            id,
            role,
            label: label.into(),
            spawned_at_unix_ms,
            spawned_by,
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
    fn every_role_declares_at_least_one_capability_class() {
        for role in [
            AgentRole::Conversational, AgentRole::Watcher, AgentRole::Capturer,
            AgentRole::Transcriber, AgentRole::Reflector, AgentRole::Indexer,
            AgentRole::Actor, AgentRole::Composer, AgentRole::Fixer,
        ] {
            assert!(
                !role.manifest().classes.is_empty(),
                "{role:?} declares no capabilities"
            );
        }
    }

    #[test]
    fn watcher_cannot_act() {
        // Load-bearing security property: a watcher cannot mutate the user's
        // world, regardless of what the LLM tries to do.
        assert!(!AgentRole::Watcher.has(CapabilityClass::Act));
    }

    #[test]
    fn conversational_cannot_act_directly() {
        // Conversational delegates Act to Actor. Direct Act would let it
        // bypass consent.
        assert!(!AgentRole::Conversational.has(CapabilityClass::Act));
    }

    #[test]
    fn capturer_has_no_compute() {
        // Pure I/O. No LLM access, no API key required, can't be prompt-injected.
        assert!(!AgentRole::Capturer.has(CapabilityClass::Compute));
    }

    #[test]
    fn only_actor_has_act() {
        // The Actor role is the *only* one declaring Act capability.
        let acting = [
            AgentRole::Conversational, AgentRole::Watcher, AgentRole::Capturer,
            AgentRole::Transcriber, AgentRole::Reflector, AgentRole::Indexer,
            AgentRole::Actor, AgentRole::Composer, AgentRole::Fixer,
        ]
        .into_iter()
        .filter(|r| r.has(CapabilityClass::Act))
        .collect::<Vec<_>>();
        assert_eq!(acting, vec![AgentRole::Actor]);
    }

    #[test]
    fn label_is_unique_per_role() {
        let mut seen = std::collections::HashSet::new();
        for role in [
            AgentRole::Conversational, AgentRole::Watcher, AgentRole::Capturer,
            AgentRole::Transcriber, AgentRole::Reflector, AgentRole::Indexer,
            AgentRole::Actor, AgentRole::Composer, AgentRole::Fixer,
        ] {
            assert!(seen.insert(role.label()), "duplicate label for {role:?}");
        }
    }

    #[test]
    fn manifest_classes_have_no_duplicates() {
        for role in [
            AgentRole::Conversational, AgentRole::Watcher, AgentRole::Capturer,
            AgentRole::Transcriber, AgentRole::Reflector, AgentRole::Indexer,
            AgentRole::Actor, AgentRole::Composer, AgentRole::Fixer,
        ] {
            let classes = role.manifest().classes;
            let unique: std::collections::HashSet<_> = classes.iter().collect();
            assert_eq!(
                unique.len(),
                classes.len(),
                "{role:?} has duplicate capability classes: {classes:?}"
            );
        }
    }

    #[test]
    fn system_prompt_paragraph_mentions_role_id_and_classes() {
        let manifest = AgentRole::Watcher.manifest();
        let prompt = manifest.system_prompt_paragraph("contradiction-finder", 42);
        assert!(prompt.contains("contradiction-finder"));
        assert!(prompt.contains("42"));
        assert!(prompt.contains("Watcher"));
        assert!(prompt.contains("Sense"));
    }

    #[test]
    fn agent_ref_constructor_timestamps_now() {
        let r = AgentRef::new(7, AgentRole::Watcher, "test", SpawnSource::Substrate);
        assert_eq!(r.id, 7);
        assert_eq!(r.role, AgentRole::Watcher);
        assert_eq!(r.label, "test");
        assert!(matches!(r.spawned_by, SpawnSource::Substrate));
        assert!(r.spawned_at_unix_ms > 0);
    }

    #[test]
    fn spawn_source_records_parent() {
        let parent: AgentId = 100;
        let r = AgentRef::new(101, AgentRole::Actor, "child", SpawnSource::Agent(parent));
        match r.spawned_by {
            SpawnSource::Agent(p) => assert_eq!(p, parent),
            other => panic!("expected Agent({parent}), got {other:?}"),
        }
    }
}
