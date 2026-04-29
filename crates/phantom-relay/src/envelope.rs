//! Wire-format envelope type.
//!
//! Once `phantom-net` is published this module can be replaced with a
//! re-export.  Until then we define a compatible local type that matches the
//! spec from issue #5.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Opaque identifier for a connected peer.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PeerId(pub String);

impl std::fmt::Display for PeerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A routable message between two peers.
///
/// The relay treats `payload` as opaque bytes (Base64-encoded JSON or binary).
/// It only inspects the routing fields (`from`, `to`) and the `sig` / `nonce`
/// pair for replay-protection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    /// Originating peer.
    pub from: PeerId,
    /// Destination peer.
    pub to: PeerId,
    /// Opaque application payload (Base64 or raw JSON string).
    pub payload: serde_json::Value,
    /// Ed25519 signature over `nonce || from || to || payload` (hex-encoded).
    pub sig: String,
    /// Monotonic nonce (UUIDv4) for replay prevention.
    pub nonce: Uuid,
}

/// Messages the relay sends back to a client.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RelayMessage {
    /// Relay successfully forwarded the envelope.
    Delivered { nonce: Uuid },
    /// Sender has exceeded the per-peer rate limit.
    RateLimitExceeded { peer_id: PeerId, retry_after_ms: u64 },
    /// The target peer is not connected.
    PeerNotFound { peer_id: PeerId },
    /// General relay error.
    Error { code: String, message: String },
    /// Server-initiated keepalive.
    Ping,
}

/// Messages a client sends to the relay after the handshake.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// Route an envelope to another peer.
    Send(Envelope),
    /// Client-initiated keepalive acknowledgement.
    Pong,
}
