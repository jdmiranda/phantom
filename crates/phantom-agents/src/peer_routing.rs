//! Cross-peer agent routing — `AnyAgentRef`, `PeerId`, `RemoteAgentInfo`, and
//! outbound-routing helpers that serialize [`InboxMessage`]s for forwarding to
//! peer Phantom instances over the relay.
//!
//! # Address space
//!
//! An agent can live on the local instance or on any connected peer.
//! [`AnyAgentRef`] captures both cases in one type so callers never need to
//! branch on locality:
//!
//! ```text
//! AnyAgentRef::Local(42)                         → agent 42 on this instance
//! AnyAgentRef::Remote { peer_id, agent_id: 7 }  → agent 7 on peer P
//! ```
//!
//! # Routing
//!
//! [`AgentRouter`] owns the outbound relay channel. When a `Remote` ref is
//! addressed the router serializes a [`RemoteInboxMessage`] as JSON and writes
//! it to the outbound sender. The caller is responsible for forwarding that
//! JSON over the WebSocket connection to the relay server.
//!
//! Inbound relay messages are decoded by [`decode_inbound`] and delivered to
//! the local [`crate::inbox::AgentRegistry`] via its `route()` call.
//!
//! # Graceful degradation
//!
//! [`AgentRouter::send_to_remote`] returns [`RemoteRouteError::NoRelay`] when
//! no relay connection is configured. Local agents are never affected — they
//! communicate in-process regardless of relay availability.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::role::AgentId;

// ---------------------------------------------------------------------------
// PeerId
// ---------------------------------------------------------------------------

/// Opaque, stable identifier for a connected Phantom peer.
///
/// A `PeerId` is a free-form string that survives serialization. In production
/// it is a UUIDv4 string; in tests any ASCII identifier is fine.
///
/// [`phantom-relay`] uses an identically shaped type — the relay is the source
/// of truth for routing. We keep our own copy here so `phantom-agents` has no
/// compile-time dependency on `phantom-relay`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PeerId(pub String);

impl PeerId {
    /// Construct from any string value.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

impl std::fmt::Display for PeerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// ---------------------------------------------------------------------------
// AnyAgentRef
// ---------------------------------------------------------------------------

/// An agent address that is valid regardless of which Phantom instance hosts
/// the agent.
///
/// - `Local(id)` — the agent lives on this instance; deliver via the
///   in-process [`crate::inbox::AgentRegistry`].
/// - `Remote { peer_id, agent_id }` — the agent lives on a peer instance;
///   serialize and forward via the relay.
///
/// [`AgentRegistry::resolve`] accepts an `AnyAgentRef` and returns the local
/// handle when the ref is local, or `None` when it is remote (the caller must
/// use [`AgentRouter`] for remote delivery).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AnyAgentRef {
    /// An agent that lives on this Phantom instance.
    Local(AgentId),
    /// An agent that lives on a remote peer instance.
    Remote {
        /// The peer that hosts the agent.
        peer_id: PeerId,
        /// The agent's id on that peer.
        agent_id: AgentId,
    },
}

impl AnyAgentRef {
    /// Returns `true` if this ref points to a local agent.
    #[must_use]
    pub fn is_local(&self) -> bool {
        matches!(self, AnyAgentRef::Local(_))
    }

    /// Returns `true` if this ref points to an agent on a remote peer.
    #[must_use]
    pub fn is_remote(&self) -> bool {
        matches!(self, AnyAgentRef::Remote { .. })
    }

    /// Extract the local [`AgentId`], if this is a local ref.
    #[must_use]
    pub fn local_id(&self) -> Option<AgentId> {
        match self {
            AnyAgentRef::Local(id) => Some(*id),
            AnyAgentRef::Remote { .. } => None,
        }
    }

    /// Extract the peer id, if this is a remote ref.
    #[must_use]
    pub fn peer_id(&self) -> Option<&PeerId> {
        match self {
            AnyAgentRef::Local(_) => None,
            AnyAgentRef::Remote { peer_id, .. } => Some(peer_id),
        }
    }

    /// The agent id, regardless of locality.
    #[must_use]
    pub fn agent_id(&self) -> AgentId {
        match self {
            AnyAgentRef::Local(id) => *id,
            AnyAgentRef::Remote { agent_id, .. } => *agent_id,
        }
    }
}

// ---------------------------------------------------------------------------
// RemoteAgentInfo
// ---------------------------------------------------------------------------

/// A brief advertisement of an agent visible on a connected peer.
///
/// Populated by the relay-listener background task when a peer broadcasts its
/// agent roster, or when an inbound envelope carries agent metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteAgentInfo {
    /// The peer that hosts this agent.
    pub peer_id: PeerId,
    /// The agent's id on its host peer.
    pub agent_id: AgentId,
    /// Human-readable summary of what the agent is doing.
    pub task_summary: String,
}

// ---------------------------------------------------------------------------
// RemoteInboxMessage — wire format for cross-peer delivery
// ---------------------------------------------------------------------------

/// The payload serialized into a relay envelope when routing a message to a
/// remote agent.
///
/// Only [`RemoteMessageContent::UserSpeak`] and
/// [`RemoteMessageContent::AgentSpeak`] are forwarded over the relay.
/// `Stop` and `Reconfigure` are local-only control messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteInboxMessage {
    /// The target agent on the recipient peer.
    pub agent_id: AgentId,
    /// The serializable message body.
    pub content: RemoteMessageContent,
}

/// Serializable subset of [`crate::inbox::InboxMessage`] that can be
/// transmitted over the relay.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RemoteMessageContent {
    /// A user-originated message forwarded to a remote agent.
    UserSpeak(String),
    /// An agent-originated message forwarded to a remote agent.
    AgentSpeak {
        /// String form of the originating peer's [`PeerId`].
        from_peer: String,
        /// The originating agent's id.
        from_agent: AgentId,
        /// The message body.
        body: String,
    },
}

// ---------------------------------------------------------------------------
// AgentRouter — outbound relay delivery
// ---------------------------------------------------------------------------

/// Delivers [`RemoteInboxMessage`]s to peer agents via the relay channel.
///
/// The router is intentionally optional: when no relay connection is configured
/// (the common case for a standalone Phantom instance) all remote-addressed
/// sends return [`RemoteRouteError::NoRelay`]. Local agents are never affected.
///
/// The relay is modelled as a `mpsc::Sender<(PeerId, String)>` so the router
/// has no compile-time dependency on the WebSocket stack. The brain's relay
/// task owns the receiver and forwards each `(to_peer, json)` pair over the
/// live WebSocket connection.
pub struct AgentRouter {
    /// Outbound channel: `(destination_peer_id, json_payload)`.
    relay_tx: Option<tokio::sync::mpsc::Sender<(PeerId, String)>>,
    /// This instance's peer id, used as the `from` address on outbound messages.
    local_peer_id: Option<PeerId>,
    /// Cache of remote agents advertised by connected peers.
    remote_agents: HashMap<(PeerId, AgentId), RemoteAgentInfo>,
}

impl AgentRouter {
    /// Create a router with no relay connection (local-only mode).
    #[must_use]
    pub fn new() -> Self {
        Self {
            relay_tx: None,
            local_peer_id: None,
            remote_agents: HashMap::new(),
        }
    }

    /// Attach a live relay sender. Call once the relay handshake completes.
    ///
    /// The relay task owns the corresponding `Receiver<(PeerId, String)>` and
    /// forwards each frame over the WebSocket to the relay server.
    pub fn set_relay(
        &mut self,
        tx: tokio::sync::mpsc::Sender<(PeerId, String)>,
        local_peer_id: PeerId,
    ) {
        self.relay_tx = Some(tx);
        self.local_peer_id = Some(local_peer_id);
    }

    /// Returns `true` if a relay sender is wired up.
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.relay_tx.is_some()
    }

    /// This instance's peer id, or `None` if no relay is configured.
    #[must_use]
    pub fn local_peer_id(&self) -> Option<&PeerId> {
        self.local_peer_id.as_ref()
    }

    /// Register a remote agent advertisement (from an inbound relay message).
    pub fn register_remote_agent(&mut self, info: RemoteAgentInfo) {
        self.remote_agents
            .insert((info.peer_id.clone(), info.agent_id), info);
    }

    /// Remove all remote agents associated with a peer (on disconnect).
    pub fn unregister_peer(&mut self, peer_id: &PeerId) {
        self.remote_agents.retain(|(pid, _), _| pid != peer_id);
    }

    /// Snapshot of all remote agents currently visible from connected peers.
    #[must_use]
    pub fn remote_agents(&self) -> Vec<&RemoteAgentInfo> {
        self.remote_agents.values().collect()
    }

    /// Route `msg` to `peer_id` over the relay channel.
    ///
    /// Serializes `msg` as JSON and enqueues it on the relay sender. The relay
    /// background task reads from that sender and writes the JSON to the live
    /// WebSocket connection.
    ///
    /// # Errors
    ///
    /// - [`RemoteRouteError::NoRelay`] if no relay sender has been set.
    /// - [`RemoteRouteError::Serialize`] if JSON serialization fails.
    /// - [`RemoteRouteError::Send`] if the relay channel is closed.
    pub async fn send_to_remote(
        &self,
        peer_id: &PeerId,
        msg: RemoteInboxMessage,
    ) -> Result<(), RemoteRouteError> {
        let tx = self.relay_tx.as_ref().ok_or(RemoteRouteError::NoRelay)?;

        let json =
            serde_json::to_string(&msg).map_err(|e| RemoteRouteError::Serialize(e.to_string()))?;

        tx.send((peer_id.clone(), json))
            .await
            .map_err(|_| RemoteRouteError::Send("relay channel closed".into()))
    }
}

impl Default for AgentRouter {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// RemoteRouteError
// ---------------------------------------------------------------------------

/// Failure modes when routing to a remote peer.
#[derive(Debug, thiserror::Error)]
pub enum RemoteRouteError {
    /// No relay connection is configured — cannot route to remote peers.
    #[error("no relay connection — cannot route to remote peer")]
    NoRelay,
    /// JSON serialization of the message failed.
    #[error("serialization error: {0}")]
    Serialize(String),
    /// The relay sender channel is closed.
    #[error("relay send error: {0}")]
    Send(String),
}

// ---------------------------------------------------------------------------
// decode_inbound — decode an incoming relay JSON payload
// ---------------------------------------------------------------------------

/// Deserialize an inbound relay payload (JSON string) as a
/// [`RemoteInboxMessage`].
///
/// Called by the brain's relay-listener task when an envelope arrives from the
/// relay server. The caller then routes the decoded message to the local
/// [`crate::inbox::AgentRegistry`] via [`crate::inbox::AgentRegistry::route`].
///
/// # Errors
///
/// Returns an error if the JSON cannot be deserialized as [`RemoteInboxMessage`].
pub fn decode_inbound(json: &str) -> Result<RemoteInboxMessage, serde_json::Error> {
    serde_json::from_str(json)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_peer(seed: &str) -> PeerId {
        PeerId::new(format!("peer-{seed}"))
    }

    // -- AnyAgentRef ----------------------------------------------------------

    #[test]
    fn any_agent_ref_local_is_local() {
        let r = AnyAgentRef::Local(42);
        assert!(r.is_local());
        assert!(!r.is_remote());
        assert_eq!(r.local_id(), Some(42));
        assert!(r.peer_id().is_none());
        assert_eq!(r.agent_id(), 42);
    }

    #[test]
    fn any_agent_ref_remote() {
        let peer = make_peer("A");
        let r = AnyAgentRef::Remote {
            peer_id: peer.clone(),
            agent_id: 7,
        };
        assert!(!r.is_local());
        assert!(r.is_remote());
        assert_eq!(r.local_id(), None);
        assert_eq!(r.peer_id(), Some(&peer));
        assert_eq!(r.agent_id(), 7);
    }

    #[test]
    fn any_agent_ref_serde_round_trip_remote() {
        let peer = make_peer("B");
        let r = AnyAgentRef::Remote {
            peer_id: peer.clone(),
            agent_id: 99,
        };
        let json = serde_json::to_string(&r).unwrap();
        let decoded: AnyAgentRef = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, r);
    }

    #[test]
    fn any_agent_ref_serde_round_trip_local() {
        let r = AnyAgentRef::Local(123);
        let json = serde_json::to_string(&r).unwrap();
        let decoded: AnyAgentRef = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, r);
    }

    // -- RemoteInboxMessage ---------------------------------------------------

    #[test]
    fn remote_inbox_message_json_round_trip_user_speak() {
        let msg = RemoteInboxMessage {
            agent_id: 5,
            content: RemoteMessageContent::UserSpeak("hello".into()),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: RemoteInboxMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.agent_id, 5);
        assert!(matches!(decoded.content, RemoteMessageContent::UserSpeak(s) if s == "hello"));
    }

    #[test]
    fn remote_inbox_message_json_round_trip_agent_speak() {
        let msg = RemoteInboxMessage {
            agent_id: 3,
            content: RemoteMessageContent::AgentSpeak {
                from_peer: "peer-X".into(),
                from_agent: 1,
                body: "Hey".into(),
            },
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: RemoteInboxMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.agent_id, 3);
        match decoded.content {
            RemoteMessageContent::AgentSpeak { body, .. } => assert_eq!(body, "Hey"),
            other => panic!("unexpected content: {other:?}"),
        }
    }

    // -- decode_inbound -------------------------------------------------------

    #[test]
    fn decode_inbound_roundtrip() {
        let msg = RemoteInboxMessage {
            agent_id: 10,
            content: RemoteMessageContent::UserSpeak("ping".into()),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded = decode_inbound(&json).unwrap();
        assert_eq!(decoded.agent_id, 10);
        assert!(matches!(decoded.content, RemoteMessageContent::UserSpeak(s) if s == "ping"));
    }

    #[test]
    fn decode_inbound_invalid_json_returns_error() {
        let result = decode_inbound("not-valid-json");
        assert!(result.is_err());
    }

    // -- AgentRouter ----------------------------------------------------------

    #[tokio::test]
    async fn agent_router_no_relay_returns_no_relay_error() {
        let router = AgentRouter::new();
        let peer = make_peer("C");
        let msg = RemoteInboxMessage {
            agent_id: 1,
            content: RemoteMessageContent::UserSpeak("hi".into()),
        };
        let result = router.send_to_remote(&peer, msg).await;
        assert!(
            matches!(result, Err(RemoteRouteError::NoRelay)),
            "expected NoRelay, got {result:?}"
        );
    }

    #[tokio::test]
    async fn agent_router_with_relay_delivers_json() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<(PeerId, String)>(8);
        let mut router = AgentRouter::new();
        router.set_relay(tx, make_peer("local"));

        assert!(router.is_connected());
        assert_eq!(router.local_peer_id(), Some(&make_peer("local")));

        let peer = make_peer("D");
        let msg = RemoteInboxMessage {
            agent_id: 7,
            content: RemoteMessageContent::UserSpeak("world".into()),
        };
        router
            .send_to_remote(&peer, msg)
            .await
            .expect("send should succeed");

        let (dest, json) = rx.recv().await.expect("should receive");
        assert_eq!(dest, peer);
        let decoded: RemoteInboxMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.agent_id, 7);
        assert!(matches!(decoded.content, RemoteMessageContent::UserSpeak(s) if s == "world"));
    }

    #[tokio::test]
    async fn agent_router_closed_channel_returns_send_error() {
        let (tx, rx) = tokio::sync::mpsc::channel::<(PeerId, String)>(1);
        drop(rx); // close the receiver so send fails
        let mut router = AgentRouter::new();
        router.set_relay(tx, make_peer("local"));

        let result = router
            .send_to_remote(
                &make_peer("E"),
                RemoteInboxMessage {
                    agent_id: 2,
                    content: RemoteMessageContent::UserSpeak("x".into()),
                },
            )
            .await;
        assert!(matches!(result, Err(RemoteRouteError::Send(_))));
    }

    // -- Remote agent registry ------------------------------------------------

    #[test]
    fn register_and_unregister_remote_agents() {
        let mut router = AgentRouter::new();
        let peer_a = make_peer("A");
        let peer_b = make_peer("B");

        router.register_remote_agent(RemoteAgentInfo {
            peer_id: peer_a.clone(),
            agent_id: 1,
            task_summary: "task A1".into(),
        });
        router.register_remote_agent(RemoteAgentInfo {
            peer_id: peer_a.clone(),
            agent_id: 2,
            task_summary: "task A2".into(),
        });
        router.register_remote_agent(RemoteAgentInfo {
            peer_id: peer_b.clone(),
            agent_id: 3,
            task_summary: "task B3".into(),
        });

        assert_eq!(router.remote_agents().len(), 3);

        router.unregister_peer(&peer_a);
        assert_eq!(router.remote_agents().len(), 1);
        assert_eq!(router.remote_agents()[0].peer_id, peer_b);
    }

    // -- RemoteAgentInfo fields -----------------------------------------------

    #[test]
    fn remote_agent_info_fields() {
        let peer = make_peer("D");
        let info = RemoteAgentInfo {
            peer_id: peer.clone(),
            agent_id: 42,
            task_summary: "doing stuff".into(),
        };
        assert_eq!(info.agent_id, 42);
        assert_eq!(info.task_summary, "doing stuff");
        assert_eq!(info.peer_id, peer);
    }

    // -- PeerId ---------------------------------------------------------------

    #[test]
    fn peer_id_display() {
        let p = PeerId::new("abc-123");
        assert_eq!(p.to_string(), "abc-123");
    }

    #[test]
    fn peer_id_serde_round_trip() {
        let p = PeerId::new("test-peer");
        let json = serde_json::to_string(&p).unwrap();
        let decoded: PeerId = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, p);
    }
}
