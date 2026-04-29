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

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::envelope::{ClientMessage, Envelope, PeerId, RelayMessage};
use crate::router::Router;

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
}

// ---------------------------------------------------------------------------
// route_agent_envelope
// ---------------------------------------------------------------------------

/// Route an [`AgentEnvelope`] using a [`Router`].
///
/// - `Local` targets: returns [`AgentRouteError::LocalDelivery`]. The caller
///   must dispatch locally through the in-process agent registry.
/// - `Remote` targets: serializes the envelope into a relay [`Envelope`] and
///   forwards it via the [`Router`] on behalf of `from_peer`.
///
/// # Errors
///
/// See [`AgentRouteError`].
pub fn route_agent_envelope(
    router: &mut Router,
    from_peer: &PeerId,
    env: AgentEnvelope,
) -> Result<(), AgentRouteError> {
    match &env.target {
        AgentTarget::Local(_) => Err(AgentRouteError::LocalDelivery),
        AgentTarget::Remote {
            peer_id: to_peer, ..
        } => {
            let payload = serde_json::to_value(&env).unwrap_or(serde_json::Value::Null);
            let wire = Envelope {
                from: from_peer.clone(),
                to: to_peer.clone(),
                payload,
                sig: String::new(), // relay-internal call; sig not required
                nonce: Uuid::new_v4(),
            };
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
        let result = route_agent_envelope(&mut router, &PeerId("alice".into()), env);
        assert!(matches!(result, Err(AgentRouteError::LocalDelivery)));
    }

    #[tokio::test]
    async fn route_remote_target_delivers_to_peer() {
        let mut router = Router::new(100, 10);
        let (mut bob_session, bob_handle) = make_session("bob");
        let (_, alice_handle) = make_session("alice");
        router.register(alice_handle).unwrap();
        router.register(bob_handle).unwrap();

        let env = AgentEnvelope {
            target: AgentTarget::Remote {
                peer_id: PeerId("bob".into()),
                agent_id: 5,
            },
            payload: serde_json::json!({"agent_id": 5, "content": {"UserSpeak": "hi"}}),
        };

        let result = route_agent_envelope(&mut router, &PeerId("alice".into()), env);
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

        let env = AgentEnvelope {
            target: AgentTarget::Remote {
                peer_id: PeerId("ghost".into()),
                agent_id: 1,
            },
            payload: serde_json::json!(null),
        };

        let result = route_agent_envelope(&mut router, &PeerId("alice".into()), env);
        assert!(
            matches!(result, Err(AgentRouteError::PeerNotFound(ref p)) if p.0 == "ghost"),
            "expected PeerNotFound(ghost), got {result:?}"
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
}
