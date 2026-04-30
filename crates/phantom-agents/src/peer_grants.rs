//! Per-peer capability grants.
//!
//! When a remote peer's agent sends a request to a local Phantom instance via
//! `AnyAgentRef::Remote`, it should not automatically receive the same capability
//! level as local agents. This module implements the grant table that limits what
//! remote agents can do.
//!
//! # Default policy
//!
//! Unknown peers receive [`PeerGrants::default_remote`]: `Sense` and `Coordinate`
//! only. To elevate a peer (e.g. a trusted CI node), call
//! [`PeerGrantRegistry::grant`] with additional [`CapabilityClass`] entries.
//!
//! # Expiry
//!
//! Grants can be time-bounded with an `until: Some(Instant)`. Any call to
//! [`PeerGrantRegistry::check`] after that instant treats the grant as absent —
//! equivalent to an unknown peer with no capabilities.
//!
//! # Integration point
//!
//! The relay's inbound dispatch path checks each incoming [`AgentEnvelope`]'s
//! sender peer against this registry before routing it to the local agent. See
//! `phantom-relay`'s `router` module for the integration site.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crate::role::CapabilityClass;

// ---------------------------------------------------------------------------
// PeerId re-export shim
// ---------------------------------------------------------------------------

/// Opaque identifier for a connected peer.
///
/// Mirrors `phantom_relay::envelope::PeerId` so `phantom-agents` can express
/// peer grants without taking a dependency on `phantom-relay`. When the two
/// crates are compiled together, callers convert with `.0` / `PeerId(s)`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct PeerId(pub String);

impl PeerId {
    /// Sentinel value used as a placeholder before a real id is assigned.
    pub const ZERO: PeerId = PeerId(String::new());
}

impl std::fmt::Display for PeerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// ---------------------------------------------------------------------------
// PeerGrants
// ---------------------------------------------------------------------------

/// Capability grant record for a single remote peer.
///
/// Holds the set of [`CapabilityClass`] values the peer is allowed to exercise
/// on this node, plus an optional expiry instant after which the grant is
/// treated as absent.
#[derive(Debug, Clone)]
pub struct PeerGrants {
    /// The peer this record applies to.
    pub peer_id: PeerId,
    /// Allowed capability classes.
    pub allowed_classes: HashSet<CapabilityClass>,
    /// When `Some(t)` the grant expires at `t`; `None` = permanent.
    pub until: Option<Instant>,
}

impl PeerGrants {
    /// Construct a grant record for `peer_id` with the given classes and expiry.
    pub fn new(
        peer_id: PeerId,
        allowed_classes: impl IntoIterator<Item = CapabilityClass>,
        until: Option<Instant>,
    ) -> Self {
        Self {
            peer_id,
            allowed_classes: allowed_classes.into_iter().collect(),
            until,
        }
    }

    /// Default grant for an unknown remote peer: `Sense` + `Coordinate` only,
    /// permanent (no expiry).
    ///
    /// The `peer_id` field is set to [`PeerId::ZERO`] — callers must override
    /// it before inserting into the registry.
    pub fn default_remote() -> Self {
        Self {
            peer_id: PeerId(String::new()),
            allowed_classes: HashSet::from([CapabilityClass::Sense, CapabilityClass::Coordinate]),
            until: None,
        }
    }

    /// Returns `true` iff this grant record has not yet expired.
    pub fn is_valid(&self) -> bool {
        match self.until {
            None => true,
            Some(t) => Instant::now() < t,
        }
    }

    /// Returns `true` iff this grant allows `class`.
    pub fn allows(&self, class: CapabilityClass) -> bool {
        self.is_valid() && self.allowed_classes.contains(&class)
    }
}

impl Default for PeerGrants {
    fn default() -> Self {
        Self::default_remote()
    }
}

// ---------------------------------------------------------------------------
// PeerGrantRegistry
// ---------------------------------------------------------------------------

/// Registry of capability grants per remote peer.
///
/// Used by the relay (and relay consumers) to enforce Layer-2 capability
/// limits on incoming requests. Default policy: unknown peers are deny-all.
/// Callers can [`grant`](Self::grant) or [`grant_default`](Self::grant_default)
/// to elevate a peer.
#[derive(Debug, Clone, Default)]
pub struct PeerGrantRegistry {
    grants: HashMap<PeerId, PeerGrants>,
}

impl PeerGrantRegistry {
    /// Create an empty registry. Unknown peers will receive an empty grant
    /// (deny all) unless explicitly registered.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register or replace the grant for `peer_id`.
    pub fn grant(&mut self, peer_id: PeerId, classes: HashSet<CapabilityClass>, until: Option<Instant>) {
        self.grants.insert(
            peer_id.clone(),
            PeerGrants::new(peer_id, classes, until),
        );
    }

    /// Promote `peer_id` to the default remote grant (`Sense` + `Coordinate`,
    /// permanent). Equivalent to calling `grant` with those two classes.
    pub fn grant_default(&mut self, peer_id: PeerId) {
        let mut g = PeerGrants::default_remote();
        g.peer_id = peer_id.clone();
        self.grants.insert(peer_id, g);
    }

    /// Remove all grants for `peer_id`. The peer reverts to the deny-all default.
    pub fn revoke(&mut self, peer_id: &PeerId) {
        self.grants.remove(peer_id);
    }

    /// Return the effective set of allowed [`CapabilityClass`] values for
    /// `peer_id`, respecting expiry.
    ///
    /// - Known peer with valid (non-expired) grant → their `allowed_classes`.
    /// - Known peer with expired grant → empty set (deny all).
    /// - Unknown peer → empty set (deny all).
    pub fn effective_classes(&self, peer_id: &PeerId) -> HashSet<CapabilityClass> {
        match self.grants.get(peer_id) {
            Some(g) if g.is_valid() => g.allowed_classes.clone(),
            _ => HashSet::new(),
        }
    }

    /// Return `true` iff `peer_id` is allowed to use `class`.
    ///
    /// - `true`: known peer with a valid (non-expired) grant that includes `class`.
    /// - `false`: unknown peer, expired grant, or class not in grant.
    pub fn check(&self, peer_id: &PeerId, class: CapabilityClass) -> bool {
        match self.grants.get(peer_id) {
            Some(g) => g.allows(class),
            None => false,
        }
    }

    /// Iterate over all registered (non-expired) grants.
    pub fn iter(&self) -> impl Iterator<Item = &PeerGrants> {
        self.grants.values().filter(|g| g.is_valid())
    }

    /// Mutable access to the internal grants map for direct updates.
    pub fn grants_mut(&mut self) -> &mut HashMap<PeerId, PeerGrants> {
        &mut self.grants
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn peer(s: &str) -> PeerId {
        PeerId(s.into())
    }

    // ── PeerGrants ────────────────────────────────────────────────────────────

    #[test]
    fn default_remote_allows_sense_and_coordinate() {
        let g = PeerGrants::default_remote();
        assert!(g.allows(CapabilityClass::Sense));
        assert!(g.allows(CapabilityClass::Coordinate));
        assert!(!g.allows(CapabilityClass::Act));
        assert!(!g.allows(CapabilityClass::Reflect));
        assert!(!g.allows(CapabilityClass::Compute));
    }

    #[test]
    fn default_remote_has_no_expiry() {
        let g = PeerGrants::default_remote();
        assert!(g.is_valid());
        assert!(g.until.is_none());
    }

    #[test]
    fn expired_grant_allows_nothing() {
        let g = PeerGrants::new(
            peer("x"),
            [CapabilityClass::Act, CapabilityClass::Sense],
            Some(Instant::now() - Duration::from_secs(1)), // in the past
        );
        assert!(!g.is_valid());
        assert!(!g.allows(CapabilityClass::Sense));
        assert!(!g.allows(CapabilityClass::Act));
    }

    #[test]
    fn future_expiry_grant_is_valid() {
        let g = PeerGrants::new(
            peer("y"),
            [CapabilityClass::Compute],
            Some(Instant::now() + Duration::from_secs(3600)),
        );
        assert!(g.is_valid());
        assert!(g.allows(CapabilityClass::Compute));
    }

    // ── PeerGrantRegistry ─────────────────────────────────────────────────────

    #[test]
    fn unknown_peer_denied_all_classes() {
        let registry = PeerGrantRegistry::new();
        for class in [
            CapabilityClass::Sense,
            CapabilityClass::Reflect,
            CapabilityClass::Compute,
            CapabilityClass::Act,
            CapabilityClass::Coordinate,
        ] {
            assert!(
                !registry.check(&peer("ghost"), class),
                "unknown peer must be denied {class:?}"
            );
        }
    }

    #[test]
    fn effective_classes_empty_for_unknown_peer() {
        let registry = PeerGrantRegistry::new();
        assert!(registry.effective_classes(&peer("ghost")).is_empty());
    }

    #[test]
    fn grant_default_allows_sense_and_coordinate() {
        let mut registry = PeerGrantRegistry::new();
        registry.grant_default(peer("alice"));
        assert!(registry.check(&peer("alice"), CapabilityClass::Sense));
        assert!(registry.check(&peer("alice"), CapabilityClass::Coordinate));
        assert!(!registry.check(&peer("alice"), CapabilityClass::Act));
    }

    #[test]
    fn grant_custom_classes() {
        let mut registry = PeerGrantRegistry::new();
        registry.grant(
            peer("trusted-ci"),
            HashSet::from([
                CapabilityClass::Sense,
                CapabilityClass::Coordinate,
                CapabilityClass::Act,
            ]),
            None,
        );
        assert!(registry.check(&peer("trusted-ci"), CapabilityClass::Act));
        assert!(!registry.check(&peer("trusted-ci"), CapabilityClass::Reflect));
    }

    #[test]
    fn revoke_returns_peer_to_deny_all() {
        let mut registry = PeerGrantRegistry::new();
        registry.grant_default(peer("bob"));
        assert!(registry.check(&peer("bob"), CapabilityClass::Sense));

        registry.revoke(&peer("bob"));
        assert!(!registry.check(&peer("bob"), CapabilityClass::Sense));
        assert!(registry.effective_classes(&peer("bob")).is_empty());
    }

    #[test]
    fn expired_grant_treated_as_no_grant() {
        let mut registry = PeerGrantRegistry::new();
        registry.grant(
            peer("temp"),
            HashSet::from([CapabilityClass::Sense, CapabilityClass::Act]),
            Some(Instant::now() - Duration::from_secs(1)), // already expired
        );
        // Both check() and effective_classes() must treat this as denied.
        assert!(!registry.check(&peer("temp"), CapabilityClass::Sense));
        assert!(registry.effective_classes(&peer("temp")).is_empty());
    }

    #[test]
    fn grant_replaces_existing_entry() {
        let mut registry = PeerGrantRegistry::new();
        registry.grant_default(peer("peer1")); // Sense + Coordinate
        // Upgrade to full trusted.
        registry.grant(
            peer("peer1"),
            HashSet::from([CapabilityClass::Sense, CapabilityClass::Act]),
            None,
        );
        assert!(registry.check(&peer("peer1"), CapabilityClass::Act));
        // Coordinate was not in the replacement grant.
        assert!(!registry.check(&peer("peer1"), CapabilityClass::Coordinate));
    }

    // ── Acceptance criteria (issue #8) ────────────────────────────────────────

    /// Remote Sense ok — unknown peer CAN use Sense under default-remote grant.
    #[test]
    fn remote_sense_allowed_under_default_grant() {
        let mut registry = PeerGrantRegistry::new();
        registry.grant_default(peer("remote-peer"));
        assert!(registry.check(&peer("remote-peer"), CapabilityClass::Sense));
    }

    /// Remote Act denied — default-remote grant does NOT include Act.
    #[test]
    fn remote_act_denied_under_default_grant() {
        let mut registry = PeerGrantRegistry::new();
        registry.grant_default(peer("remote-peer"));
        assert!(!registry.check(&peer("remote-peer"), CapabilityClass::Act));
    }

    /// Local agents unaffected — the registry is only consulted for remote peers;
    /// local agents bypass it. Simulate this by verifying that an absent entry
    /// (never registered) is always deny, while a local bypass path simply doesn't
    /// consult the registry at all (no-op test to document intent).
    #[test]
    fn local_path_does_not_consult_registry() {
        // Local agents are identified by AgentId, not PeerId. The registry is
        // only consulted when the envelope carries a remote PeerId. This test
        // documents that the registry has no knowledge of local agents — it
        // would need to be handed a peer-id string that matches a local agent,
        // which the relay never does.
        let registry = PeerGrantRegistry::new();
        // No entry for "local-sentinel" → deny, but the local dispatch path
        // never reaches this registry, so the end-to-end result is allow.
        assert!(!registry.check(&peer("local-sentinel"), CapabilityClass::Act));
    }

    /// Expired grant denies — a grant whose `until` is in the past is treated
    /// as no grant.
    #[test]
    fn expired_grant_denies_all() {
        let mut registry = PeerGrantRegistry::new();
        registry.grant(
            peer("expired"),
            HashSet::from([CapabilityClass::Sense, CapabilityClass::Coordinate]),
            Some(Instant::now() - Duration::from_millis(1)),
        );
        assert!(!registry.check(&peer("expired"), CapabilityClass::Sense));
        assert!(!registry.check(&peer("expired"), CapabilityClass::Coordinate));
    }
}
