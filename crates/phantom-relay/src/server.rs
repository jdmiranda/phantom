//! WebSocket server: accepts connections and drives per-peer tasks.
//!
//! # Handshake (proof-of-possession, issue #526)
//!
//! The relay rejects trust-on-first-use. On every new connection:
//!
//! 1. Server sends a [`Challenge`] frame containing 32 bytes of fresh entropy
//!    (hex-encoded).
//! 2. Client replies with an [`IdentityProof`] containing:
//!    - `peer_id` — claimed identity string
//!    - `public_key` — 32-byte Ed25519 verifying key (64 hex chars)
//!    - `signature` — 64-byte Ed25519 signature (128 hex chars) over the
//!      canonical proof-of-possession bytes (see [`pop_canonical_bytes`])
//! 3. Server verifies the signature with the supplied public key. If it does
//!    not bind `peer_id` to the challenge under the claimed key, the
//!    connection is rejected with an `auth_failed` error.
//!
//! Only after a verified signature does the server register the peer and
//! grant the [`CapabilityClass::Relay`] capability. This replaces the
//! previous TOFU model where the client self-asserted its key.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use futures_util::{SinkExt, StreamExt};
use log::{error, info, warn};
use rand::rngs::OsRng;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio_tungstenite::{
    accept_async,
    tungstenite::{
        protocol::{frame::coding::CloseCode, CloseFrame},
        Message,
    },
};

use crate::envelope::{ClientMessage, PeerId, RelayMessage};
use crate::grant::{CapabilityClass, Grant};
use crate::router::{Router, MAX_CONNECTIONS};
use crate::session::Session;

// ── Handshake types ──────────────────────────────────────────────────────────

/// Length of the proof-of-possession nonce in bytes.
const POP_CHALLENGE_LEN: usize = 32;

/// Domain-separation prefix for the proof-of-possession message.
///
/// Mixed into the canonical bytes so a signature minted for some other
/// protocol can never be replayed against the relay handshake.
const POP_DOMAIN_TAG: &[u8] = b"phantom-relay-pop-v1\0";

/// Server-issued challenge sent before [`IdentityProof`].
#[derive(Debug, Serialize)]
struct Challenge<'a> {
    /// Message tag for clients that dispatch on `type`.
    #[serde(rename = "type")]
    kind: &'static str,
    /// 32 random bytes, hex-encoded (64 chars).
    challenge: &'a str,
}

/// First message a connecting client must send after receiving the challenge.
///
/// All fields are required. The legacy `proof` field is no longer accepted —
/// trust-on-first-use was removed in issue #526.
#[derive(Debug, Deserialize)]
struct IdentityProof {
    /// Requested peer identity string.
    peer_id: String,
    /// 32-byte Ed25519 public key, lowercase hex (64 chars).
    public_key: String,
    /// 64-byte Ed25519 signature over [`pop_canonical_bytes`], lowercase hex
    /// (128 chars).
    signature: String,
}

/// Relay's response to a successful handshake.
#[derive(Debug, Serialize)]
struct HandshakeAck {
    session_token: String,
    peer_id: String,
}

// ── Proof-of-possession helpers ──────────────────────────────────────────────

/// Canonical bytes the client signs and the server verifies.
///
/// Layout:
///
/// ```text
/// POP_DOMAIN_TAG || peer_id_utf8 || 0x00 || challenge_bytes
/// ```
///
/// The domain tag prevents cross-protocol signature reuse; the trailing NUL
/// byte separates the variable-length `peer_id` from the fixed-length
/// challenge so two distinct (peer_id, challenge) pairs cannot produce the
/// same byte string.
pub(crate) fn pop_canonical_bytes(peer_id: &str, challenge: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(POP_DOMAIN_TAG.len() + peer_id.len() + 1 + challenge.len());
    buf.extend_from_slice(POP_DOMAIN_TAG);
    buf.extend_from_slice(peer_id.as_bytes());
    buf.push(0);
    buf.extend_from_slice(challenge);
    buf
}

/// Decode a hex string of exactly `N` bytes (`2*N` hex chars).
fn decode_hex<const N: usize>(s: &str) -> Result<[u8; N], &'static str> {
    if s.len() != N * 2 {
        return Err("unexpected hex length");
    }
    let mut out = [0u8; N];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        let hi = from_hex_digit(chunk[0]).ok_or("invalid hex digit")?;
        let lo = from_hex_digit(chunk[1]).ok_or("invalid hex digit")?;
        out[i] = (hi << 4) | lo;
    }
    Ok(out)
}

fn from_hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Encode bytes as lowercase hex.
fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Generate a fresh proof-of-possession challenge.
fn fresh_challenge() -> [u8; POP_CHALLENGE_LEN] {
    let mut buf = [0u8; POP_CHALLENGE_LEN];
    OsRng.fill_bytes(&mut buf);
    buf
}

/// Verify that `proof` cryptographically binds `peer_id` to `challenge`.
///
/// On success returns the verified [`VerifyingKey`]. On failure returns a
/// short stable reason string suitable for an `auth_failed` error message.
fn verify_pop(proof: &IdentityProof, challenge: &[u8]) -> Result<VerifyingKey, &'static str> {
    let pk_bytes: [u8; 32] = decode_hex::<32>(&proof.public_key)
        .map_err(|_| "public_key must be 64 lowercase hex chars")?;
    let sig_bytes: [u8; 64] = decode_hex::<64>(&proof.signature)
        .map_err(|_| "signature must be 128 lowercase hex chars")?;

    let vk = VerifyingKey::from_bytes(&pk_bytes).map_err(|_| "public_key is not a valid Ed25519 point")?;
    let sig = Signature::from_bytes(&sig_bytes);
    let canonical = pop_canonical_bytes(&proof.peer_id, challenge);

    vk.verify(&canonical, &sig).map_err(|_| "signature did not verify")?;
    Ok(vk)
}

// ── Server ───────────────────────────────────────────────────────────────────

/// Run the relay WebSocket server until the process is killed.
pub async fn run(addr: SocketAddr, router: Arc<Mutex<Router>>) -> Result<()> {
    let listener = TcpListener::bind(addr).await?;
    run_with_listener(listener, router).await
}

/// Run the relay WebSocket server using an already-bound listener.
///
/// This variant is useful in tests where you need to discover the ephemeral
/// port *before* handing control to the server loop.
pub async fn run_with_listener(listener: TcpListener, router: Arc<Mutex<Router>>) -> Result<()> {
    info!(
        "phantom-relay listening on {}",
        listener.local_addr().unwrap_or_else(|_| "unknown".parse().unwrap())
    );

    loop {
        match listener.accept().await {
            Ok((stream, peer_addr)) => {
                let router_clone = Arc::clone(&router);
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(stream, peer_addr, router_clone).await {
                        warn!("connection error from {}: {}", peer_addr, e);
                    }
                });
            }
            Err(e) => {
                error!("accept error: {}", e);
            }
        }
    }
}

// ── Per-connection task ───────────────────────────────────────────────────────

async fn handle_connection(
    stream: TcpStream,
    peer_addr: SocketAddr,
    router: Arc<Mutex<Router>>,
) -> Result<()> {
    let ws_stream = accept_async(stream).await?;
    let (mut ws_sink, mut ws_source) = ws_stream.split();

    info!("new connection from {}", peer_addr);

    // ── 1. Handshake (proof-of-possession, issue #526) ───────────────────────
    //
    // Server sends a fresh 32-byte challenge BEFORE expecting an
    // IdentityProof. Client must sign the canonical bytes (see
    // `pop_canonical_bytes`) with the private half of the public key it
    // claims. This replaces the prior trust-on-first-use scheme.
    let challenge_bytes = fresh_challenge();
    let challenge_hex = encode_hex(&challenge_bytes);
    let challenge_msg = Challenge {
        kind: "challenge",
        challenge: &challenge_hex,
    };
    if let Err(e) = ws_sink
        .send(Message::from(serde_json::to_string(&challenge_msg)?))
        .await
    {
        warn!("failed to send challenge to {}: {}", peer_addr, e);
        return Ok(());
    }

    let raw = match ws_source.next().await {
        Some(Ok(msg)) => msg,
        Some(Err(e)) => return Err(e.into()),
        None => {
            warn!("peer {} closed before handshake", peer_addr);
            return Ok(());
        }
    };

    let proof: IdentityProof = match raw {
        Message::Text(text) => match serde_json::from_str(&text) {
            Ok(p) => p,
            Err(e) => {
                warn!("malformed IdentityProof from {}: {}", peer_addr, e);
                let err = RelayMessage::Error {
                    code: "auth_failed".into(),
                    message: format!("malformed identity proof: {e}"),
                };
                let _ = ws_sink
                    .send(Message::from(serde_json::to_string(&err)?))
                    .await;
                return Ok(());
            }
        },
        other => {
            warn!("unexpected handshake message type from {}: {:?}", peer_addr, other);
            return Ok(());
        }
    };

    if proof.peer_id.is_empty() {
        warn!("empty peer_id from {}; rejecting", peer_addr);
        let err = RelayMessage::Error {
            code: "auth_failed".into(),
            message: "peer_id must not be empty".into(),
        };
        let _ = ws_sink
            .send(Message::from(serde_json::to_string(&err)?))
            .await;
        return Ok(());
    }

    // Verify proof-of-possession: the supplied signature must bind the
    // claimed peer_id to the server-issued challenge under the supplied
    // public key. Reject the connection on any failure.
    if let Err(reason) = verify_pop(&proof, &challenge_bytes) {
        warn!(
            "proof-of-possession failed for peer_id={} from {}: {}",
            proof.peer_id, peer_addr, reason
        );
        let err = RelayMessage::Error {
            code: "auth_failed".into(),
            message: format!("proof-of-possession failed: {reason}"),
        };
        let _ = ws_sink
            .send(Message::from(serde_json::to_string(&err)?))
            .await;
        return Ok(());
    }

    let peer_id = PeerId(proof.peer_id.clone());
    let session = Session::new(peer_id.clone());
    let token = session.token;
    let handle = session.handle();
    let mut outbound_rx = session.rx;

    {
        let mut guard = router.lock().await;

        // Check the connection cap before registering. When the active session
        // count equals MAX_CONNECTIONS we close immediately with WS 1013 (Try
        // Again Later) without burning any session state.
        if guard.peer_count() >= MAX_CONNECTIONS {
            warn!(
                "relay: connection limit ({}) reached; rejecting peer {} with 1013",
                MAX_CONNECTIONS, peer_id
            );
            let body = serde_json::to_string(&RelayMessage::TryAgainLater)?;
            let _ = ws_sink.send(Message::from(body)).await;
            let _ = ws_sink
                .send(Message::Close(Some(CloseFrame {
                    code: CloseCode::Again,
                    reason: "connection limit reached".into(),
                })))
                .await;
            return Ok(());
        }

        if let Err(e) = guard.register(handle) {
            let err = RelayMessage::Error {
                code: "register_failed".into(),
                message: e.to_string(),
            };
            ws_sink
                .send(Message::from(serde_json::to_string(&err)?))
                .await?;
            return Ok(());
        }
        // Issue a permanent `Relay` grant on successful authentication.
        // The peer proved its identity via `IdentityProof`; it is now allowed
        // to forward envelopes through the relay until it disconnects.
        guard.grant(&peer_id, Grant::permanent(CapabilityClass::Relay));
    }

    let ack = HandshakeAck {
        session_token: token.to_string(),
        peer_id: peer_id.to_string(),
    };
    ws_sink
        .send(Message::from(serde_json::to_string(&ack)?))
        .await?;
    info!(
        "peer {} registered (token {})",
        peer_id,
        &token.to_string()[..8]
    );

    // ── 2. Message loop ───────────────────────────────────────────────────────
    loop {
        tokio::select! {
            // Inbound: client → relay
            incoming = ws_source.next() => {
                match incoming {
                    None => break,
                    Some(Err(e)) => {
                        warn!("ws error from {}: {}", peer_id, e);
                        break;
                    }
                    Some(Ok(Message::Text(text))) => {
                        let client_msg: ClientMessage = match serde_json::from_str(&text) {
                            Ok(m) => m,
                            Err(e) => {
                                warn!("bad message from {}: {}", peer_id, e);
                                let err = RelayMessage::Error {
                                    code: "parse_error".into(),
                                    message: e.to_string(),
                                };
                                let _ = ws_sink
                                    .send(Message::from(serde_json::to_string(&err)?))
                                    .await;
                                continue;
                            }
                        };

                        let reply = {
                            let mut guard = router.lock().await;
                            guard.route(&peer_id, client_msg)
                        };

                        // Don't echo anything back for Pong — just touch heartbeat.
                        if matches!(reply, RelayMessage::Ping) {
                            continue;
                        }

                        // Determine whether this reply also requires closing
                        // the connection with a specific WS close code.
                        let close_frame: Option<CloseFrame> = match &reply {
                            RelayMessage::MessageTooLarge { .. } => {
                                warn!("relay: closing peer {} with 1009 (message too large)", peer_id);
                                Some(CloseFrame {
                                    code: CloseCode::Size,
                                    reason: "message too large".into(),
                                })
                            }
                            RelayMessage::SlidingWindowExceeded { .. } => {
                                warn!("relay: closing peer {} with 1008 (policy violation)", peer_id);
                                Some(CloseFrame {
                                    code: CloseCode::Policy,
                                    reason: "rate limit exceeded".into(),
                                })
                            }
                            _ => None,
                        };

                        let _ = ws_sink
                            .send(Message::from(serde_json::to_string(&reply)?))
                            .await;

                        if let Some(frame) = close_frame {
                            let _ = ws_sink.send(Message::Close(Some(frame))).await;
                            break;
                        }
                    }
                    Some(Ok(Message::Ping(data))) => {
                        let _ = ws_sink.send(Message::Pong(data)).await;
                    }
                    Some(Ok(Message::Close(_))) => break,
                    Some(Ok(_)) => {}
                }
            }

            // Outbound: router → client (forwarded envelopes)
            outbound = outbound_rx.recv() => {
                match outbound {
                    None => break,
                    Some(json) => {
                        if ws_sink.send(Message::from(json)).await.is_err() {
                            break;
                        }
                    }
                }
            }
        }
    }

    // ── 3. Cleanup ────────────────────────────────────────────────────────────
    info!("peer {} disconnected", peer_id);
    let mut guard = router.lock().await;
    guard.unregister(&peer_id);
    // Revoke all capability grants so a reconnecting peer with the same id
    // cannot inherit a stale grant. Grants are re-issued on the next
    // successful handshake (see step 1 above).
    guard.revoke_all(&peer_id);
    Ok(())
}
