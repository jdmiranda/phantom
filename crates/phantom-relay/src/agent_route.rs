//! Agent-addressed routing layer on top of the peer-level relay.
//!
//! The relay's core [`crate::router::Router`] routes opaque [`crate::envelope::Envelope`]s
//! between peers by [`crate::envelope::PeerId`]. This module adds the
//! agent-addressing layer: given an `AnyAgentRef` (borrowed from the wire
//! format — reproduced here as [`AgentTarget`] to avoid a hard dependency on
//! `phantom-agents`) the [`AgentEnvelope`] type bundles the target identity
//! with a JSON payload.
//!
//! # Wire format
//!
//! An `AgentEnvelope` is serialized as JSON and placed into the `payload`
//! field of a [`crate::envelope::Envelope`]. The relay broker forwards the
//! outer envelope transparently; the receiving peer deserializes the inner
//! `AgentEnvelope` and delivers it to the addressed agent.
//!
//! # Graceful degradation
//!
//! [`route_agent_envelope`] returns [`AgentRouteError::LocalDelivery`] for
//! `Local` targets — the relay layer never touches local delivery; the caller
//! is responsible for that. For `Remote` targets it delegates to the relay
//! [`crate::router::Router`].

use ed25519_dalek::SigningKey;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::envelope::{ClientMessage, Envelope, PeerId, RelayMessage};
use crate::grant::CapabilityClass;
use crate::router::Router;
use crate::signing;

// ---------------------------------------------------------------------------
// AgentTarget — wire-compatible with phantom-agents AnyAgentRef
// ---------------------------------------------------------------------------

/// A peer-portable agent address. Wire-compatible with
/// `phantom_agents::AnyAgentRef` (same serde tag layout) so both sides can
/// deserialize without sharing a crate dependency.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentTarget {
    /// The agent lives on the local instance.
    Local(u64),
    /// The agent lives on a remote peer instance.
    Remote {
        /// The peer that hosts the agent.
        peer_id: PeerId,
        /// The agent's id on that peer.
        agent_id: u64,
    },
}

impl AgentTarget {
    /// Returns `true` if this target is local.
    #[must_use]
    pub fn is_local(&self) -> bool {
        matches!(self, AgentTarget::Local(_))
    }

    /// Returns `true` if this target is on a remote peer.
    #[must_use]
    pub fn is_remote(&self) -> bool {
        matches!(self, AgentTarget::Remote { .. })
    }

    /// Extract the destination peer id for a remote target.
    #[must_use]
    pub fn destination_peer(&self) -> Option<&PeerId> {
        match self {
            AgentTarget::Local(_) => None,
            AgentTarget::Remote { peer_id, .. } => Some(peer_id),
        }
    }
}

// ---------------------------------------------------------------------------
// AgentEnvelope — typed payload for agent-addressed messages
// ---------------------------------------------------------------------------

/// A message addressed to a specific agent, carried inside a relay
/// [`Envelope`] payload.
///
/// The relay broker forwards the outer `Envelope` transparently by `PeerId`.
/// The receiving peer unpacks the `AgentEnvelope` from the payload and
/// dispatches to the local agent runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentEnvelope {
    /// Who the message is addressed to.
    pub target: AgentTarget,
    /// Opaque JSON payload (the serialized `RemoteInboxMessage` from
    /// `phantom-agents`).
    pub payload: serde_json::Value,
}

// ---------------------------------------------------------------------------
// AgentRouteError
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// GrantDenied
// ---------------------------------------------------------------------------

/// Failure modes for [`route_agent_envelope`].
#[derive(Debug, thiserror::Error)]
pub enum AgentRouteError {
    /// The target is local — the relay layer must not handle it.
    #[error("target is local; local delivery is the caller's responsibility")]
    LocalDelivery,
    /// The target peer is not connected to this relay.
    #[error("remote peer {0} is not connected")]
    PeerNotFound(PeerId),
    /// The relay rejected the message (rate limit, serialization error, etc.).
    #[error("relay error: {0}")]
    RelayError(String),
    /// The sender peer's capability grant does not permit this action.
    ///
    /// The peer must be explicitly granted the required capability before
    /// routing will succeed. Unknown peers are deny-all by default.
    #[error("peer {0} lacks capability grant for relay agent forward")]
    GrantDenied(PeerId),
}

// ---------------------------------------------------------------------------
// GrantChecker type alias
// ---------------------------------------------------------------------------

/// Function type for per-peer capability grant checks.
///
/// Returns `true` iff `peer_id` is allowed to use `class`. Callers wire this
/// to [`crate::grant::PeerGrantRegistry::check`] or a test double.
pub type GrantChecker<'a> = Option<&'a dyn Fn(&PeerId, CapabilityClass) -> bool>;

// ---------------------------------------------------------------------------
// route_agent_envelope
// ---------------------------------------------------------------------------

/// Route an [`AgentEnvelope`] using a [`Router`], with an optional capability
/// grant check.
///
/// - `Local` targets: returns [`AgentRouteError::LocalDelivery`]. The caller
///   must dispatch locally through the in-process agent registry.
/// - `Remote` targets: serializes the envelope into a relay [`Envelope`] and
///   forwards it via the [`Router`] on behalf of `from_peer`.
///
/// # Grant check
///
/// When `grant_check` is `Some(f)`, the function `f(peer_id, capability)` is
/// called before routing. Forwarding an agent envelope requires at least the
/// `Relay` capability grant. If the check returns `false`, this function
/// returns [`AgentRouteError::GrantDenied`].
///
/// Pass `None` to skip the check (for relay-internal or trusted local paths).
///
/// # Signing (issue #457)
///
/// When `signer` is `Some(key)`, the constructed relay [`Envelope`] is signed
/// with `key` before being handed to the router. The signature covers the
/// canonical bytes: `from || NUL || to || NUL || nonce_le16 || payload_json`.
/// Recipients verify the signature using the sender's published Ed25519
/// verifying key via [`crate::signing::verify_envelope`].
///
/// Pass `None` for relay-internal or test paths that do not require a
/// cryptographic signature.
///
/// # Errors
///
/// See [`AgentRouteError`].
pub fn route_agent_envelope(
    router: &mut Router,
    from_peer: &PeerId,
    env: AgentEnvelope,
    grant_check: GrantChecker<'_>,
    signer: Option<&SigningKey>,
) -> Result<(), AgentRouteError> {
    match &env.target {
        // Local delivery — no grant check; the caller dispatches in-process.
        AgentTarget::Local(_) => Err(AgentRouteError::LocalDelivery),
        AgentTarget::Remote {
            peer_id: to_peer, ..
        } => {
            // --- capability grant check ---
            //
            // Forwarding a message to a remote agent requires the `Relay`
            // grant. Unknown / unregistered peers are deny-all by default.
            // Local targets bypass this check because the local dispatch path
            // is governed by the existing role/taint model, not the peer grant
            // registry. `Relay` is the canonical capability class after the
            // consolidation of the two former enums (issue #492).
            if grant_check.is_some_and(|check| !check(from_peer, CapabilityClass::Relay)) {
                return Err(AgentRouteError::GrantDenied(from_peer.clone()));
            }

            let payload = serde_json::to_value(&env).unwrap_or(serde_json::Value::Null);
            let mut wire = Envelope {
                from: from_peer.clone(),
                to: to_peer.clone(),
                payload,
                sig: String::new(),
                nonce: Uuid::new_v4(),
            };

            // Sign the envelope when the caller provides a signing key (issue #457).
            // The `sig` field is left empty only on relay-internal trusted paths
            // where no signing key is available; external callers must always pass
            // a signer so that recipients can verify the sender's identity.
            if let Some(key) = signer {
                signing::sign_envelope(&mut wire, key);
            }

            let reply = router.route(from_peer, ClientMessage::Send(wire));
            match reply {
                RelayMessage::Delivered { .. } => Ok(()),
                RelayMessage::PeerNotFound { peer_id } => {
                    Err(AgentRouteError::PeerNotFound(peer_id))
                }
                RelayMessage::RateLimitExceeded {
                    peer_id,
                    retry_after_ms,
                } => Err(AgentRouteError::RelayError(format!(
                    "rate limit for peer {peer_id}; retry in {retry_after_ms}ms"
                ))),
                other => Err(AgentRouteError::RelayError(format!(
                    "unexpected relay reply: {other:?}"
                ))),
            }
        }
    }
}

/// Decode an [`AgentEnvelope`] from the JSON payload of an inbound relay
/// [`Envelope`].
///
/// Returns an error if the payload cannot be deserialized as an
/// [`AgentEnvelope`].
///
/// # Errors
///
/// Returns a [`serde_json::Error`] if deserialization fails.
pub fn decode_agent_envelope(envelope: &Envelope) -> Result<AgentEnvelope, serde_json::Error> {
    serde_json::from_value(envelope.payload.clone())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grant::Grant;
    use crate::session::Session;

    fn make_session(id: &str) -> (Session, crate::session::SessionHandle) {
        let session = Session::new(PeerId(id.into()));
        let handle = session.handle();
        (session, handle)
    }

    // -- AgentTarget ----------------------------------------------------------

    #[test]
    fn agent_target_local_is_local() {
        let t = AgentTarget::Local(42);
        assert!(t.is_local());
        assert!(!t.is_remote());
        assert!(t.destination_peer().is_none());
    }

    #[test]
    fn agent_target_remote_is_remote() {
        let peer = PeerId("peer-A".into());
        let t = AgentTarget::Remote {
            peer_id: peer.clone(),
            agent_id: 7,
        };
        assert!(!t.is_local());
        assert!(t.is_remote());
        assert_eq!(t.destination_peer(), Some(&peer));
    }

    #[test]
    fn agent_target_serde_round_trip_local() {
        let t = AgentTarget::Local(99);
        let json = serde_json::to_string(&t).unwrap();
        let decoded: AgentTarget = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, t);
    }

    #[test]
    fn agent_target_serde_round_trip_remote() {
        let t = AgentTarget::Remote {
            peer_id: PeerId("peer-B".into()),
            agent_id: 3,
        };
        let json = serde_json::to_string(&t).unwrap();
        let decoded: AgentTarget = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, t);
    }

    // -- AgentEnvelope --------------------------------------------------------

    #[test]
    fn agent_envelope_serde_round_trip() {
        let env = AgentEnvelope {
            target: AgentTarget::Remote {
                peer_id: PeerId("peer-C".into()),
                agent_id: 1,
            },
            payload: serde_json::json!({"agent_id": 1, "content": {"UserSpeak": "hello"}}),
        };
        let json = serde_json::to_string(&env).unwrap();
        let decoded: AgentEnvelope = serde_json::from_str(&json).unwrap();
        assert!(decoded.target.is_remote());
    }

    // -- route_agent_envelope -------------------------------------------------

    #[test]
    fn route_local_target_returns_local_delivery_error() {
        let mut router = Router::new(100, 10);
        let env = AgentEnvelope {
            target: AgentTarget::Local(1),
            payload: serde_json::json!(null),
        };
        // Grant check not reached for local targets; pass None for both.
        let result = route_agent_envelope(&mut router, &PeerId("alice".into()), env, None, None);
        assert!(matches!(result, Err(AgentRouteError::LocalDelivery)));
    }

    #[tokio::test]
    async fn route_remote_target_delivers_to_peer() {
        let mut router = Router::new(100, 10);
        let (mut bob_session, bob_handle) = make_session("bob");
        let (_, alice_handle) = make_session("alice");
        router.register(alice_handle).unwrap();
        router.register(bob_handle).unwrap();
        // Alice needs a Relay grant so router.route() lets the envelope through.
        router.grant(&PeerId("alice".into()), Grant::permanent(CapabilityClass::Relay));

        let env = AgentEnvelope {
            target: AgentTarget::Remote {
                peer_id: PeerId("bob".into()),
                agent_id: 5,
            },
            payload: serde_json::json!({"agent_id": 5, "content": {"UserSpeak": "hi"}}),
        };

        // No agent-level grant check and no signer — trusted internal path.
        let result = route_agent_envelope(&mut router, &PeerId("alice".into()), env, None, None);
        assert!(result.is_ok(), "routing should succeed: {result:?}");

        // Bob's session channel should have received the forwarded envelope.
        let raw = bob_session
            .rx
            .try_recv()
            .expect("message not delivered to bob");
        assert!(raw.contains("bob"), "envelope should address bob");
    }

    #[test]
    fn route_remote_target_unknown_peer_returns_peer_not_found() {
        let mut router = Router::new(100, 10);
        let (_, alice_handle) = make_session("alice");
        router.register(alice_handle).unwrap();
        // Alice holds a Relay grant so she passes the router grant check; the
        // failure must come from "ghost" not being registered.
        router.grant(&PeerId("alice".into()), Grant::permanent(CapabilityClass::Relay));

        let env = AgentEnvelope {
            target: AgentTarget::Remote {
                peer_id: PeerId("ghost".into()),
                agent_id: 1,
            },
            payload: serde_json::json!(null),
        };

        let result = route_agent_envelope(&mut router, &PeerId("alice".into()), env, None, None);
        assert!(
            matches!(result, Err(AgentRouteError::PeerNotFound(ref p)) if p.0 == "ghost"),
            "expected PeerNotFound(ghost), got {result:?}"
        );
    }

    // ── Issue #8 — per-peer capability grant enforcement ─────────────────────

    /// An unknown peer (no entry in the grant check) is denied by default.
    #[test]
    fn grant_denied_blocks_routing_for_unknown_peer() {
        let mut router = Router::new(100, 10);
        let (_, alice_handle) = make_session("alice");
        let (_, bob_handle) = make_session("bob");
        router.register(alice_handle).unwrap();
        router.register(bob_handle).unwrap();

        let env = AgentEnvelope {
            target: AgentTarget::Remote {
                peer_id: PeerId("bob".into()),
                agent_id: 1,
            },
            payload: serde_json::json!(null),
        };

        // Deny-all grant check simulating an empty PeerGrantRegistry.
        let deny_all = |_peer: &PeerId, _class: CapabilityClass| false;
        let result = route_agent_envelope(
            &mut router,
            &PeerId("unknown-peer".into()),
            env,
            Some(&deny_all),
            None,
        );
        assert!(
            matches!(result, Err(AgentRouteError::GrantDenied(_))),
            "expected GrantDenied, got {result:?}"
        );
    }

    /// A peer with an explicit Relay grant is allowed through.
    #[test]
    fn granted_peer_routes_successfully() {
        let mut router = Router::new(100, 10);
        let (_alice_session, alice_handle) = make_session("alice");
        let (_bob_session, bob_handle) = make_session("bob"); // keep alive so channel stays open
        router.register(alice_handle).unwrap();
        router.register(bob_handle).unwrap();
        // Alice needs a router-level Relay grant so router.route() passes.
        router.grant(&PeerId("alice".into()), Grant::permanent(CapabilityClass::Relay));

        let env = AgentEnvelope {
            target: AgentTarget::Remote {
                peer_id: PeerId("bob".into()),
                agent_id: 2,
            },
            payload: serde_json::json!(null),
        };

        // Grant check that accepts the canonical Relay class.
        let allow_relay = |_peer: &PeerId, class: CapabilityClass| {
            matches!(class, CapabilityClass::Relay)
        };
        let result = route_agent_envelope(
            &mut router,
            &PeerId("alice".into()),
            env,
            Some(&allow_relay),
            None,
        );
        assert!(result.is_ok(), "granted peer should route: {result:?}");
    }

    /// Expired grant (simulated by deny returning false) is enforced.
    #[test]
    fn expired_grant_blocks_routing() {
        let mut router = Router::new(100, 10);
        let (_, src_handle) = make_session("src");
        let (_, dst_handle) = make_session("dst");
        router.register(src_handle).unwrap();
        router.register(dst_handle).unwrap();

        let env = AgentEnvelope {
            target: AgentTarget::Remote {
                peer_id: PeerId("dst".into()),
                agent_id: 3,
            },
            payload: serde_json::json!(null),
        };

        // Expired grant returns false (as PeerGrantRegistry::check does after expiry).
        let expired = |_peer: &PeerId, _class: CapabilityClass| false;
        let result =
            route_agent_envelope(&mut router, &PeerId("src".into()), env, Some(&expired), None);
        assert!(
            matches!(result, Err(AgentRouteError::GrantDenied(_))),
            "expired grant must be denied, got {result:?}"
        );
    }

    /// Revoked grant is denied after revocation.
    #[test]
    fn revoked_grant_blocks_routing() {
        let mut router = Router::new(100, 10);
        let (_alice_session, alice_handle) = make_session("alice");
        let (_bob_session, bob_handle) = make_session("bob");
        router.register(alice_handle).unwrap();
        router.register(bob_handle).unwrap();

        let env = AgentEnvelope {
            target: AgentTarget::Remote {
                peer_id: PeerId("bob".into()),
                agent_id: 4,
            },
            payload: serde_json::json!(null),
        };

        // Post-revocation the checker returns false.
        let after_revoke = |_peer: &PeerId, _class: CapabilityClass| false;
        let result = route_agent_envelope(
            &mut router,
            &PeerId("alice".into()),
            env,
            Some(&after_revoke),
            None,
        );
        assert!(
            matches!(result, Err(AgentRouteError::GrantDenied(_))),
            "revoked grant must be denied, got {result:?}"
        );
    }

    /// Local agents bypass the grant check — the registry is never consulted.
    ///
    /// The grant check closure runs only for remote peers. If the target is
    /// Local, `LocalDelivery` is returned before the check fires.
    #[test]
    fn local_target_bypasses_grant_check() {
        let mut router = Router::new(100, 10);
        let env = AgentEnvelope {
            target: AgentTarget::Local(9),
            payload: serde_json::json!(null),
        };
        // A deny-all checker that should never be called for local targets.
        let should_not_be_called = |_peer: &PeerId, _class: CapabilityClass| {
            panic!("grant check must not fire for local targets");
        };
        // The result must be LocalDelivery, not GrantDenied.
        let result = route_agent_envelope(
            &mut router,
            &PeerId("alice".into()),
            env,
            Some(&should_not_be_called),
            None,
        );
        assert!(
            matches!(result, Err(AgentRouteError::LocalDelivery)),
            "expected LocalDelivery for local target, got {result:?}"
        );
    }

    // -- decode_agent_envelope ------------------------------------------------

    #[test]
    fn decode_agent_envelope_from_relay_envelope() {
        let agent_env = AgentEnvelope {
            target: AgentTarget::Remote {
                peer_id: PeerId("bob".into()),
                agent_id: 2,
            },
            payload: serde_json::json!({"agent_id": 2, "content": {"UserSpeak": "decoded"}}),
        };
        let payload = serde_json::to_value(&agent_env).unwrap();
        let relay_env = Envelope {
            from: PeerId("alice".into()),
            to: PeerId("bob".into()),
            payload,
            sig: "sig".into(),
            nonce: Uuid::new_v4(),
        };

        let decoded = decode_agent_envelope(&relay_env).unwrap();
        assert!(decoded.target.is_remote());
        assert_eq!(
            decoded.target.destination_peer(),
            Some(&PeerId("bob".into()))
        );
    }

    #[test]
    fn decode_agent_envelope_invalid_payload_returns_error() {
        let relay_env = Envelope {
            from: PeerId("alice".into()),
            to: PeerId("bob".into()),
            payload: serde_json::json!("not-an-agent-envelope"),
            sig: "sig".into(),
            nonce: Uuid::new_v4(),
        };
        let result = decode_agent_envelope(&relay_env);
        assert!(result.is_err());
    }

    // -- Issue #457 — envelope signing ----------------------------------------

    /// Routing with a signer populates `sig` and the delivered envelope
    /// verifies against the sender's verifying key.
    #[tokio::test]
    async fn signed_envelope_verifies_on_delivery() {
        use crate::signing;
        use crate::signing::tests::make_signing_key;

        let key = make_signing_key();
        let vk = key.verifying_key();

        let mut router = Router::new(100, 10);
        let (mut bob_session, bob_handle) = make_session("bob");
        let (_, alice_handle) = make_session("alice");
        router.register(alice_handle).unwrap();
        router.register(bob_handle).unwrap();
        router.grant(&PeerId("alice".into()), Grant::permanent(CapabilityClass::Relay));

        let env = AgentEnvelope {
            target: AgentTarget::Remote {
                peer_id: PeerId("bob".into()),
                agent_id: 10,
            },
            payload: serde_json::json!({"agent_id": 10, "content": {"UserSpeak": "signed"}}),
        };

        let result = route_agent_envelope(
            &mut router,
            &PeerId("alice".into()),
            env,
            None,
            Some(&key),
        );
        assert!(result.is_ok(), "signed routing must succeed: {result:?}");

        // Deserialize the wire envelope that arrived in Bob's channel.
        let raw = bob_session
            .rx
            .try_recv()
            .expect("message not delivered to bob");
        let client_msg: crate::envelope::ClientMessage =
            serde_json::from_str(&raw).expect("invalid JSON in channel");
        let crate::envelope::ClientMessage::Send(wire_env) = client_msg else {
            panic!("expected Send variant");
        };

        // The sig field must be non-empty (128 hex chars).
        assert_eq!(wire_env.sig.len(), 128, "sig must be 128 hex chars");

        // Verification must succeed with Alice's verifying key.
        assert!(
            signing::verify_envelope(&wire_env, &vk).is_ok(),
            "delivered envelope signature must verify"
        );
    }

    /// A tampered envelope must fail signature verification.
    #[test]
    fn tampered_envelope_fails_verification() {
        use crate::signing;
        use crate::signing::tests::make_signing_key;

        let key = make_signing_key();
        let vk = key.verifying_key();

        // Build and sign a relay envelope directly (not via route_agent_envelope
        // — we just need a signed Envelope to tamper with).
        let mut env = Envelope {
            from: PeerId("alice".into()),
            to: PeerId("bob".into()),
            payload: serde_json::json!({"msg": "original"}),
            sig: String::new(),
            nonce: Uuid::new_v4(),
        };
        signing::sign_envelope(&mut env, &key);
        assert!(signing::verify_envelope(&env, &vk).is_ok());

        // Tamper with the payload after signing.
        env.payload = serde_json::json!({"msg": "tampered"});
        assert!(
            signing::verify_envelope(&env, &vk).is_err(),
            "tampered envelope must fail verification"
        );
    }
}
