//! Per-peer capability grant registry for the relay.
//!
//! [`PeerGrantRegistry`] is a default-deny, expiry-aware store that controls
//! which [`CapabilityClass`]es a connected peer is allowed to exercise through
//! the relay. Every inbound relay message is classified into a capability class
//! and checked against this registry before dispatch.
//!
//! # Default-deny
//!
//! A peer that has never been granted any capability, or whose grants have all
//! expired or been revoked, is denied. There is no implicit allow.
//!
//! # Expiry
//!
//! Every grant carries an optional `expires_at` timestamp
//! (`std::time::Instant`). A grant whose `expires_at` is in the past is treated
//! as absent — the check returns [`GrantDenied::Expired`].
//!
//! # Revocation
//!
//! [`PeerGrantRegistry::revoke`] removes a specific `(peer, class)` pair
//! immediately. Revocation is synchronous and takes effect before the next
//! [`PeerGrantRegistry::check`] call on the same peer.

use std::collections::HashMap;
use std::time::Instant;

use crate::envelope::PeerId;

// ── CapabilityClass ───────────────────────────────────────────────────────────

/// What kind of operation a peer is requesting through the relay.
///
/// The relay classifies every inbound [`crate::envelope::Envelope`] into one
/// of these classes before consulting the [`PeerGrantRegistry`].
///
/// - [`Vision`] — requests that invoke a remote vision / GPT-4V analysis
///   pipeline (e.g. screenshot analysis forwarded to a peer running the
///   vision backend).
/// - [`Stt`] — requests that route audio chunks to a remote speech-to-text
///   backend (Whisper, Deepgram, etc.) on another peer.
/// - [`Voice`] — requests that route text to a remote text-to-speech backend
///   (ElevenLabs, OpenAI TTS, etc.) on another peer.
/// - [`Embeddings`] — requests that forward text/image/audio to a remote
///   embedding backend (OpenAI, Ollama, etc.) on another peer.
/// - [`Llm`] — requests that dispatch a prompt to a remote LLM backend
///   (Claude, Ollama, OpenAI-compat) running on another peer. Covers both
///   the `phantom-agents` ChatBackend path and the `phantom-brain` router's
///   cloud-provider path when those calls are relayed rather than local.
/// - [`Relay`] — basic peer-to-peer message forwarding with no additional
///   cloud-backend semantics. Used for control-plane messages
///   (handshake, heartbeat, arbitrary data tunnelling between peers).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CapabilityClass {
    /// Remote vision / GPT-4V analysis pipeline calls forwarded through the
    /// relay to a peer hosting a vision backend.
    Vision,
    /// Remote speech-to-text calls (Whisper / Deepgram / etc.) forwarded
    /// through the relay to a peer hosting an STT backend.
    Stt,
    /// Remote text-to-speech calls (ElevenLabs / OpenAI TTS / etc.) forwarded
    /// through the relay to a peer hosting a voice backend.
    Voice,
    /// Remote embedding calls (OpenAI / Ollama / etc.) forwarded through the
    /// relay to a peer hosting an embedding backend.
    Embeddings,
    /// Remote LLM inference calls (Claude / OpenAI-compat / Ollama) forwarded
    /// through the relay to a peer hosting a language model backend.
    Llm,
    /// Basic peer-to-peer relay forwarding (control plane, arbitrary tunnelling).
    ///
    /// This class covers messages that do not map to a specialised cloud backend.
    /// A peer must hold this grant to send *any* envelope through the relay.
    Relay,
}

// ── Grant ─────────────────────────────────────────────────────────────────────

/// A single capability grant for one `(peer, class)` pair.
#[derive(Debug, Clone)]
pub struct Grant {
    /// The class being granted.
    pub class: CapabilityClass,
    /// When this grant expires, or `None` for a non-expiring grant.
    pub expires_at: Option<Instant>,
}

impl Grant {
    /// Build a non-expiring grant.
    #[must_use]
    pub fn permanent(class: CapabilityClass) -> Self {
        Self { class, expires_at: None }
    }

    /// Build a grant that expires at `expires_at`.
    #[must_use]
    pub fn with_expiry(class: CapabilityClass, expires_at: Instant) -> Self {
        Self { class, expires_at: Some(expires_at) }
    }

    /// `true` if the grant is currently valid (not yet expired).
    #[must_use]
    pub fn is_valid(&self) -> bool {
        match self.expires_at {
            None => true,
            Some(exp) => Instant::now() < exp,
        }
    }
}

// ── GrantDenied ───────────────────────────────────────────────────────────────

/// Reason why [`PeerGrantRegistry::check`] returned a denial.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantDenied {
    /// No grant for this `(peer, class)` pair has ever been issued.
    NoGrant,
    /// A grant existed but its expiry timestamp has passed.
    Expired,
    /// The peer itself is unknown to the registry (never registered).
    UnknownPeer,
}

impl std::fmt::Display for GrantDenied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoGrant => f.write_str("no capability grant"),
            Self::Expired => f.write_str("capability grant expired"),
            Self::UnknownPeer => f.write_str("unknown peer"),
        }
    }
}

// ── PeerGrantRegistry ─────────────────────────────────────────────────────────

/// Default-deny, expiry-aware capability grant store.
///
/// Keyed by `(PeerId, CapabilityClass)`. Each value is the most recently
/// granted [`Grant`] for that pair. Duplicate grants overwrite the previous
/// one (last-write-wins), which lets operators refresh expiry times without
/// revoking and re-granting.
///
/// # Thread safety
///
/// `PeerGrantRegistry` is not `Sync`. Callers must wrap it in a `Mutex` or
/// `RwLock` when sharing across tasks (the router already holds it behind
/// `Arc<Mutex<Router>>`).
#[derive(Debug, Default)]
pub struct PeerGrantRegistry {
    grants: HashMap<(String, CapabilityClass), Grant>,
}

impl PeerGrantRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a grant for `peer_id` / `class`.
    ///
    /// Overwrites any existing grant for the same pair (refreshes expiry).
    pub fn grant(&mut self, peer_id: &PeerId, grant: Grant) {
        self.grants.insert((peer_id.0.clone(), grant.class), grant);
    }

    /// Revoke the grant for `peer_id` / `class` immediately.
    ///
    /// A subsequent [`Self::check`] for the same pair will return
    /// [`GrantDenied::NoGrant`].
    pub fn revoke(&mut self, peer_id: &PeerId, class: CapabilityClass) {
        self.grants.remove(&(peer_id.0.clone(), class));
    }

    /// Revoke all grants for `peer_id` (e.g. on disconnect).
    pub fn revoke_all(&mut self, peer_id: &PeerId) {
        self.grants.retain(|(id, _), _| id != &peer_id.0);
    }

    /// Check whether `peer_id` holds a valid, non-expired grant for `class`.
    ///
    /// Returns `Ok(())` on success, or a [`GrantDenied`] variant describing
    /// the denial reason.
    ///
    /// An expired grant is **not** removed from the registry by this call —
    /// expired entries are left in place to avoid allocation during the hot
    /// path. Use [`Self::gc`] to prune them in bulk.
    ///
    /// # Default-deny
    ///
    /// Any peer or class not present in the registry is denied with
    /// [`GrantDenied::NoGrant`]. There is no implicit allow.
    pub fn check(
        &self,
        peer_id: &PeerId,
        class: CapabilityClass,
    ) -> Result<(), GrantDenied> {
        match self.grants.get(&(peer_id.0.clone(), class)) {
            None => Err(GrantDenied::NoGrant),
            Some(grant) if !grant.is_valid() => Err(GrantDenied::Expired),
            Some(_) => Ok(()),
        }
    }

    /// Remove all expired grants from the registry.
    ///
    /// Call periodically (e.g. once per minute on the server tick) to keep
    /// memory bounded when many short-lived grants are issued.
    pub fn gc(&mut self) {
        self.grants.retain(|_, grant| grant.is_valid());
    }

    /// Number of currently stored grant entries (including expired ones not
    /// yet GC'd).
    #[must_use]
    pub fn len(&self) -> usize {
        self.grants.len()
    }

    /// `true` if no grants are stored.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.grants.is_empty()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn peer(id: &str) -> PeerId {
        PeerId(id.to_string())
    }

    // ── grant + check ─────────────────────────────────────────────────────────

    #[test]
    fn unknown_peer_is_denied_no_grant() {
        let registry = PeerGrantRegistry::new();
        let err = registry
            .check(&peer("alice"), CapabilityClass::Relay)
            .unwrap_err();
        assert_eq!(err, GrantDenied::NoGrant);
    }

    #[test]
    fn granted_peer_passes_check() {
        let mut registry = PeerGrantRegistry::new();
        registry.grant(&peer("alice"), Grant::permanent(CapabilityClass::Relay));
        assert!(registry.check(&peer("alice"), CapabilityClass::Relay).is_ok());
    }

    #[test]
    fn grant_does_not_bleed_to_other_class() {
        let mut registry = PeerGrantRegistry::new();
        registry.grant(&peer("alice"), Grant::permanent(CapabilityClass::Relay));
        let err = registry
            .check(&peer("alice"), CapabilityClass::Vision)
            .unwrap_err();
        assert_eq!(err, GrantDenied::NoGrant);
    }

    #[test]
    fn grant_does_not_bleed_to_other_peer() {
        let mut registry = PeerGrantRegistry::new();
        registry.grant(&peer("alice"), Grant::permanent(CapabilityClass::Relay));
        let err = registry
            .check(&peer("bob"), CapabilityClass::Relay)
            .unwrap_err();
        assert_eq!(err, GrantDenied::NoGrant);
    }

    // ── expiry ────────────────────────────────────────────────────────────────

    #[test]
    fn expired_grant_is_denied() {
        let mut registry = PeerGrantRegistry::new();
        // expires_at is in the past
        let past = Instant::now() - Duration::from_secs(1);
        registry.grant(
            &peer("alice"),
            Grant::with_expiry(CapabilityClass::Relay, past),
        );
        let err = registry
            .check(&peer("alice"), CapabilityClass::Relay)
            .unwrap_err();
        assert_eq!(err, GrantDenied::Expired);
    }

    #[test]
    fn future_expiry_grant_passes_check() {
        let mut registry = PeerGrantRegistry::new();
        let future = Instant::now() + Duration::from_secs(60);
        registry.grant(
            &peer("alice"),
            Grant::with_expiry(CapabilityClass::Relay, future),
        );
        assert!(registry.check(&peer("alice"), CapabilityClass::Relay).is_ok());
    }

    // ── revoke ────────────────────────────────────────────────────────────────

    #[test]
    fn revoke_removes_grant() {
        let mut registry = PeerGrantRegistry::new();
        registry.grant(&peer("alice"), Grant::permanent(CapabilityClass::Relay));
        registry.revoke(&peer("alice"), CapabilityClass::Relay);
        let err = registry
            .check(&peer("alice"), CapabilityClass::Relay)
            .unwrap_err();
        assert_eq!(err, GrantDenied::NoGrant);
    }

    #[test]
    fn revoke_only_removes_specified_class() {
        let mut registry = PeerGrantRegistry::new();
        registry.grant(&peer("alice"), Grant::permanent(CapabilityClass::Relay));
        registry.grant(&peer("alice"), Grant::permanent(CapabilityClass::Vision));
        registry.revoke(&peer("alice"), CapabilityClass::Vision);
        // Relay grant must still be present.
        assert!(registry.check(&peer("alice"), CapabilityClass::Relay).is_ok());
        // Vision grant must be gone.
        assert_eq!(
            registry.check(&peer("alice"), CapabilityClass::Vision).unwrap_err(),
            GrantDenied::NoGrant
        );
    }

    #[test]
    fn revoke_all_removes_every_grant_for_peer() {
        let mut registry = PeerGrantRegistry::new();
        registry.grant(&peer("alice"), Grant::permanent(CapabilityClass::Relay));
        registry.grant(&peer("alice"), Grant::permanent(CapabilityClass::Vision));
        registry.grant(&peer("bob"), Grant::permanent(CapabilityClass::Relay));
        registry.revoke_all(&peer("alice"));
        assert_eq!(
            registry.check(&peer("alice"), CapabilityClass::Relay).unwrap_err(),
            GrantDenied::NoGrant
        );
        assert_eq!(
            registry.check(&peer("alice"), CapabilityClass::Vision).unwrap_err(),
            GrantDenied::NoGrant
        );
        // Bob's grant must be unaffected.
        assert!(registry.check(&peer("bob"), CapabilityClass::Relay).is_ok());
    }

    // ── refresh / overwrite ───────────────────────────────────────────────────

    #[test]
    fn re_grant_overwrites_expired_entry() {
        let mut registry = PeerGrantRegistry::new();
        let past = Instant::now() - Duration::from_secs(1);
        registry.grant(
            &peer("alice"),
            Grant::with_expiry(CapabilityClass::Relay, past),
        );
        // Overwrite with a permanent grant.
        registry.grant(&peer("alice"), Grant::permanent(CapabilityClass::Relay));
        assert!(registry.check(&peer("alice"), CapabilityClass::Relay).is_ok());
    }

    // ── GC ────────────────────────────────────────────────────────────────────

    #[test]
    fn gc_removes_expired_entries() {
        let mut registry = PeerGrantRegistry::new();
        let past = Instant::now() - Duration::from_secs(1);
        registry.grant(
            &peer("alice"),
            Grant::with_expiry(CapabilityClass::Relay, past),
        );
        registry.grant(&peer("bob"), Grant::permanent(CapabilityClass::Relay));
        assert_eq!(registry.len(), 2);
        registry.gc();
        assert_eq!(registry.len(), 1);
        // Bob's permanent grant must survive GC.
        assert!(registry.check(&peer("bob"), CapabilityClass::Relay).is_ok());
    }

    // ── all capability class variants ─────────────────────────────────────────

    #[test]
    fn all_capability_classes_can_be_granted_and_checked() {
        let classes = [
            CapabilityClass::Vision,
            CapabilityClass::Stt,
            CapabilityClass::Voice,
            CapabilityClass::Embeddings,
            CapabilityClass::Llm,
            CapabilityClass::Relay,
        ];
        let mut registry = PeerGrantRegistry::new();
        let p = peer("tester");
        for class in classes {
            registry.grant(&p, Grant::permanent(class));
            assert!(
                registry.check(&p, class).is_ok(),
                "check failed for {class:?}"
            );
        }
    }
}
