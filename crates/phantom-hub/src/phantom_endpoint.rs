//! `GET /phantom/connect` — Phantom-side WSS dial-in.
//!
//! # Protocol
//!
//! This endpoint speaks the **binary relay-envelope protocol** used by
//! `phantom-net::RelayClient`.  All frames are binary WebSocket messages
//! containing a JSON-serialised [`phantom_net::Envelope`] (the envelope
//! itself is JSON; the outer WebSocket frame is binary).
//!
//! ## Handshake sequence
//!
//! ```text
//! Phantom → Hub: Binary(Envelope { from: peer_id, to: "relay",
//!                                  payload: b"HELLO", ... })
//! Hub → Phantom: Binary(Envelope { from: "hub", to: peer_id,
//!                                  payload: b"HELLO_ACK", ... })
//! Phantom → Hub: Binary(Envelope { from: peer_id, to: "hub",
//!                                  payload: JSON({ type: "register",
//!                                                  phantom_id, device_token,
//!                                                  version, host }) })
//! ```
//!
//! 1. The client upgrades HTTP → WebSocket.
//!    The `Authorization: Bearer <jwt>` header must be present; the hub
//!    verifies the JWT before accepting the upgrade.  A missing or invalid
//!    JWT results in a `401 Unauthorized` HTTP response before the WebSocket
//!    handshake completes.
//!
//! 2. The first binary frame must be a HELLO envelope
//!    (`Envelope.payload == b"HELLO"`).  Non-binary frames and envelopes with
//!    any other payload are rejected with a `4400` close code.
//!
//! 3. The hub replies with a HELLO_ACK binary envelope addressed to the
//!    Phantom's `from` peer-id.
//!
//! 4. The second binary frame must be a registration envelope whose `payload`
//!    is UTF-8 JSON:
//!    ```json
//!    {
//!      "type":         "register",
//!      "phantom_id":   "<string>",
//!      "device_token": "<signed JWT>",
//!      "version":      "<semver string>",
//!      "host":         "<optional hostname>"
//!    }
//!    ```
//!    The `device_token` is a JWT issued by `POST /auth/register`.  It is
//!    verified by [`crate::auth::JwtAuthority`].  An invalid or expired JWT
//!    closes the connection with close code `4401`.
//!
//! 5. Subsequent inbound frames are binary envelopes whose `payload` bytes are
//!    a JSON-serialised `JsonRpcResponse`.  They are dispatched to waiting
//!    [`crate::router::forward`] callers via [`crate::router::deliver_response`].
//!
//! 6. Outbound frames (hub → Phantom) are binary envelopes wrapping
//!    `JsonRpcRequest` payload bytes.
//!
//! 7. On WebSocket close the Phantom is removed from the registry and all
//!    in-flight pending oneshots are dropped (triggering
//!    [`crate::router::RouteError::Disconnected`] for any waiting callers).
//!
//! # Relation to hub_listener (phantom-mcp)
//!
//! `phantom-mcp::hub_listener` connects using `RelayClient::connect`, which
//! sends binary relay-envelope HELLO frames and expects a binary HELLO_ACK.
//! This endpoint is the server-side implementation of exactly that protocol.
//! The two are designed to interoperate directly.
//!
//! # `POST /auth/register`
//!
//! The challenge–response registration endpoint (issue #398) that issues JWTs
//! is defined in this module alongside the WSS handler.
//!
//! ```text
//! 1. Phantom generates a random nonce locally.
//! 2. Phantom signs (nonce_bytes || peer_id_bytes) with its Ed25519 key.
//! 3. Phantom POSTs { peer_id, public_key_hex, nonce_hex, signature_hex }.
//! 4. Hub verifies the signature.
//! 5. Hub calls NonceCache::try_claim — returns 409 Conflict if nonce was
//!    already used (replay protection, issue #398).
//! 6. Hub issues a JWT bound to peer_id.
//! 7. Phantom stores the JWT via `phantom_net::DeviceCredentials`.
//! ```

use axum::{
    Json,
    extract::{
        ConnectInfo, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use tracing::{info, warn};

use crate::{
    AppState,
    auth::{self, AuthError},
    registry::{PhantomId, OUTBOUND_CHANNEL_CAPACITY},
    router::{JsonRpcResponse, deliver_response, JsonRpcRequest},
};

// ---------------------------------------------------------------------------
// POST /auth/register
// ---------------------------------------------------------------------------

/// Request body for `POST /auth/register`.
#[derive(Debug, Serialize, Deserialize)]
pub struct RegisterRequest {
    /// The Phantom's stable peer_id (base58 SHA-256 of public key).
    pub peer_id: String,
    /// The Phantom's Ed25519 public key, hex-encoded (64 hex chars = 32 bytes).
    pub public_key_hex: String,
    /// Nonce hex-encoded by the client.
    ///
    /// v1: the client generates the nonce itself and includes it in the
    /// request.  The hub verifies the signature over (nonce || peer_id) which
    /// proves key ownership, then records the nonce in [`auth::NonceCache`] to
    /// prevent replay attacks.
    pub nonce_hex: String,
    /// Ed25519 signature over `(nonce_bytes || peer_id_bytes)`, hex-encoded
    /// (128 hex chars = 64 bytes).
    pub signature_hex: String,
}

/// Response body for a successful `POST /auth/register`.
#[derive(Debug, Serialize, Deserialize)]
pub struct RegisterResponse {
    /// The signed JWT.  Do not log this value.
    pub device_token: String,
    /// Token expiry as a Unix timestamp (seconds).
    pub exp: u64,
    /// The `peer_id` echoed back for the caller's convenience.
    pub phantom_id: String,
}

/// Handler for `POST /auth/register`.
///
/// Verifies the Ed25519 registration signature, enforces nonce single-use via
/// [`auth::NonceCache`], and issues a JWT device token.
///
/// Returns `409 Conflict` when the nonce has already been claimed — this is the
/// replay-rejection path.
pub async fn register(
    State(state): State<AppState>,
    Json(body): Json<RegisterRequest>,
) -> impl IntoResponse {
    let pubkey_bytes = match hex_decode_exact::<32>(&body.public_key_hex) {
        Ok(b) => b,
        Err(()) => {
            warn!(phantom_id = %body.peer_id, "register: invalid public_key_hex");
            return (
                StatusCode::BAD_REQUEST,
                "public_key_hex must be 64 hex chars (32 bytes)",
            )
                .into_response();
        }
    };

    let nonce_bytes_raw = match hex_decode_vec(&body.nonce_hex) {
        Ok(b) => b,
        Err(()) => {
            warn!(phantom_id = %body.peer_id, "register: invalid nonce_hex");
            return (StatusCode::BAD_REQUEST, "nonce_hex is not valid hex").into_response();
        }
    };
    let nonce_str = match String::from_utf8(nonce_bytes_raw) {
        Ok(s) => s,
        Err(_) => {
            warn!(phantom_id = %body.peer_id, "register: nonce_hex does not decode to UTF-8");
            return (StatusCode::BAD_REQUEST, "nonce must be a UTF-8 string").into_response();
        }
    };

    let sig_bytes = match hex_decode_exact::<64>(&body.signature_hex) {
        Ok(b) => b,
        Err(()) => {
            warn!(phantom_id = %body.peer_id, "register: invalid signature_hex");
            return (
                StatusCode::BAD_REQUEST,
                "signature_hex must be 128 hex chars (64 bytes)",
            )
                .into_response();
        }
    };

    // Verify the registration signature BEFORE claiming the nonce so that an
    // attacker cannot burn nonces without possessing a valid signing key.
    if let Err(e) = auth::verify_registration_signature(
        &body.peer_id,
        &nonce_str,
        &pubkey_bytes,
        &sig_bytes,
    ) {
        warn!(phantom_id = %body.peer_id, "register: {e} — auth_failure");
        return (StatusCode::UNAUTHORIZED, "signature verification failed").into_response();
    }

    // Claim the nonce.  try_claim is atomic (single Mutex acquisition —
    // check + insert happen together with no window between them).
    // Returns false when the nonce was already used within the TTL window.
    if !state.nonce_cache.try_claim(&nonce_str) {
        warn!(phantom_id = %body.peer_id, "register: nonce already used — replay_rejected");
        return (
            StatusCode::CONFLICT,
            "nonce already used — registration request must not be replayed",
        )
            .into_response();
    }

    // Issue the JWT.
    let token = match state.jwt.issue(&body.peer_id) {
        Ok(t) => t,
        Err(e) => {
            warn!(phantom_id = %body.peer_id, "register: JWT issuance failed: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to issue device token",
            )
                .into_response();
        }
    };

    let exp = state.jwt.verify(&token).map(|c| c.exp).unwrap_or(0);

    // Persist the verifying key to the on-disk peer-key registry (issue #527).
    // We do this after JWT issuance so a write failure does not strand a
    // peer with no token, but before responding so the registry is always
    // at least as up-to-date as the most recently issued token.
    //
    // pubkey_bytes already round-tripped through verify_registration_signature
    // above, so VerifyingKey::from_bytes here cannot fail — but we surface
    // the error rather than unwrap, defending against future refactors.
    let vk = match ed25519_dalek::VerifyingKey::from_bytes(&pubkey_bytes) {
        Ok(vk) => vk,
        Err(e) => {
            warn!(phantom_id = %body.peer_id, "register: pubkey decoded twice but failed canonicalisation: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to canonicalise public key",
            )
                .into_response();
        }
    };
    {
        let reg = state.registry.read().await;
        if let Err(e) = reg.insert_peer_key(PhantomId::new(body.peer_id.clone()), vk) {
            // Non-fatal: a disk-write failure should not block JWT issuance,
            // but we log loudly so operators can investigate.  The peer can
            // still connect with the JWT we just issued; signature-checked
            // operations that depend on the persisted key will fail until
            // the underlying disk issue is resolved.
            warn!(
                phantom_id = %body.peer_id,
                "register: persisting peer key failed: {e}"
            );
        }
    }

    info!(phantom_id = %body.peer_id, "register: device token issued");
    Json(RegisterResponse {
        device_token: token,
        exp,
        phantom_id: body.peer_id,
    })
    .into_response()
}

// ---------------------------------------------------------------------------
// GET /phantom/connect
// ---------------------------------------------------------------------------

/// Handler for `GET /phantom/connect`.
///
/// Validates the device JWT from `Authorization: Bearer <jwt>` before
/// accepting the WebSocket upgrade, then hands off to [`handle_phantom_ws`]
/// for the binary relay-envelope handshake.
pub async fn connect(
    ws: WebSocketUpgrade,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    // Validate the JWT before accepting the WebSocket upgrade.
    let token = match auth::extract_bearer(&headers) {
        Some(t) => t,
        None => {
            warn!("phantom/connect: missing Authorization header — auth_failure");
            return (
                StatusCode::UNAUTHORIZED,
                "Authorization: Bearer <device_token> required",
            )
                .into_response();
        }
    };

    let claims = match state.jwt.verify(&token) {
        Ok(c) => c,
        Err(AuthError::Expired) => {
            warn!("phantom/connect: expired JWT — auth_failure");
            return (StatusCode::UNAUTHORIZED, "device token expired").into_response();
        }
        Err(_) => {
            warn!("phantom/connect: invalid JWT — auth_failure");
            return (StatusCode::UNAUTHORIZED, "invalid device token").into_response();
        }
    };

    let jwt_peer_id = claims.sub.clone();
    info!(
        phantom_id = %jwt_peer_id,
        "phantom/connect: JWT valid — accepting WebSocket upgrade"
    );

    ws.on_upgrade(move |socket| {
        handle_phantom_ws(socket, peer_addr, state, jwt_peer_id)
    })
}

// ---------------------------------------------------------------------------
// Relay-envelope wire helpers
// ---------------------------------------------------------------------------

/// Peer-id the hub uses as the envelope sender for HELLO_ACK and outbound frames.
const HUB_PEER_ID: &str = "hub";

/// Decode a binary WebSocket frame as a relay-envelope JSON blob.
///
/// Returns the inner `from`, `to`, and `payload` fields only.
/// The hub does not verify the Ed25519 signature in Phase 1 — signature
/// verification is deferred to issue #399.
fn decode_envelope(bytes: &[u8]) -> Option<DecodedEnvelope> {
    #[derive(serde::Deserialize)]
    struct RawEnvelope {
        from: String,
        to: String,
        #[serde(with = "base64_bytes")]
        payload: Vec<u8>,
    }

    serde_json::from_slice::<RawEnvelope>(bytes)
        .ok()
        .map(|e| DecodedEnvelope {
            from: e.from,
            to: e.to,
            payload: e.payload,
        })
}

struct DecodedEnvelope {
    from: String,
    #[allow(dead_code)]
    to: String,
    payload: Vec<u8>,
}

/// Encode a binary relay-envelope frame (hub → Phantom).
///
/// The hub synthesises envelopes using a zero nonce and an empty signature —
/// Phase 1 only; real signing deferred to issue #399.
fn encode_envelope(from: &str, to: &str, payload: &[u8]) -> Vec<u8> {
    #[derive(serde::Serialize)]
    struct WireEnvelope<'a> {
        from: &'a str,
        to: &'a str,
        #[serde(with = "base64_bytes")]
        payload: &'a [u8],
        #[serde(with = "base64_bytes")]
        sig: &'a [u8],
        nonce: u64,
    }

    serde_json::to_vec(&WireEnvelope {
        from,
        to,
        payload,
        sig: &[],
        nonce: 0,
    })
    .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Base64 serde helper (matches phantom-net's serde_bytes_base64 impl)
// ---------------------------------------------------------------------------

mod base64_bytes {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    static CHARS: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    fn b64_encode(input: &[u8]) -> String {
        let mut out = Vec::with_capacity(input.len().div_ceil(3) * 4);
        for chunk in input.chunks(3) {
            let b0 = chunk[0];
            let b1 = if chunk.len() > 1 { chunk[1] } else { 0 };
            let b2 = if chunk.len() > 2 { chunk[2] } else { 0 };
            out.push(CHARS[(b0 >> 2) as usize]);
            out.push(CHARS[((b0 & 3) << 4 | b1 >> 4) as usize]);
            if chunk.len() > 1 {
                out.push(CHARS[((b1 & 15) << 2 | b2 >> 6) as usize]);
            } else {
                out.push(b'=');
            }
            if chunk.len() > 2 {
                out.push(CHARS[(b2 & 63) as usize]);
            } else {
                out.push(b'=');
            }
        }
        String::from_utf8(out).unwrap()
    }

    fn b64_decode(input: &[u8]) -> Result<Vec<u8>, &'static str> {
        fn val(c: u8) -> Result<u8, &'static str> {
            match c {
                b'A'..=b'Z' => Ok(c - b'A'),
                b'a'..=b'z' => Ok(c - b'a' + 26),
                b'0'..=b'9' => Ok(c - b'0' + 52),
                b'+' => Ok(62),
                b'/' => Ok(63),
                b'=' => Ok(0),
                _ => Err("invalid base64 character"),
            }
        }
        let clean: Vec<u8> = input
            .iter()
            .copied()
            .filter(|&c| c != b'\n' && c != b'\r')
            .collect();
        if !clean.len().is_multiple_of(4) {
            return Err("base64 input length not a multiple of 4");
        }
        let mut out = Vec::with_capacity(clean.len() / 4 * 3);
        for chunk in clean.chunks(4) {
            let v0 = val(chunk[0])?;
            let v1 = val(chunk[1])?;
            let v2 = val(chunk[2])?;
            let v3 = val(chunk[3])?;
            out.push(v0 << 2 | v1 >> 4);
            if chunk[2] != b'=' {
                out.push((v1 & 15) << 4 | v2 >> 2);
            }
            if chunk[3] != b'=' {
                out.push((v2 & 3) << 6 | v3);
            }
        }
        Ok(out)
    }

    pub fn serialize<S: Serializer>(bytes: &[u8], ser: S) -> Result<S::Ok, S::Error> {
        b64_encode(bytes).serialize(ser)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(de)?;
        b64_decode(s.as_bytes()).map_err(serde::de::Error::custom)
    }
}

// ---------------------------------------------------------------------------
// Registration frame (second envelope, from hub_listener)
// ---------------------------------------------------------------------------

/// Payload of the registration envelope sent by `hub_listener` after HELLO_ACK.
#[derive(Debug, Deserialize)]
pub struct RegistrationFrame {
    /// Message type — must be `"register"`.
    #[serde(rename = "type")]
    #[allow(dead_code)]
    pub msg_type: Option<String>,
    /// Stable identity of this Phantom instance (matches JWT `sub` claim).
    pub phantom_id: String,
    /// Device JWT issued by `POST /auth/register`.
    pub device_token: String,
    /// Phantom client version string.
    #[serde(default)]
    pub version: String,
    /// Optional human-readable hostname for diagnostics.
    #[serde(default)]
    pub host: String,
}

// ---------------------------------------------------------------------------
// Per-connection WebSocket driver
// ---------------------------------------------------------------------------

async fn handle_phantom_ws(
    mut socket: WebSocket,
    peer_addr: SocketAddr,
    state: AppState,
    jwt_peer_id: String,
) {
    let host = peer_addr.to_string();
    info!("phantom_endpoint: new binary-envelope connection from {host} (jwt_peer_id={jwt_peer_id})");

    // ── 1. HELLO handshake ───────────────────────────────────────────────────
    let hello_env = match receive_binary_envelope(&mut socket, &host).await {
        Ok(e) => e,
        Err(reason) => {
            warn!("phantom_endpoint: rejected {host} at HELLO: {reason}");
            let _ = socket
                .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                    code: 4400,
                    reason: reason.into(),
                })))
                .await;
            return;
        }
    };

    if hello_env.payload != b"HELLO" {
        warn!(
            "phantom_endpoint: expected HELLO payload from {host}, got {:?}",
            &hello_env.payload[..hello_env.payload.len().min(32)]
        );
        let _ = socket
            .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                code: 4400,
                reason: "expected HELLO payload as first frame".into(),
            })))
            .await;
        return;
    }

    let peer_id_from_hello = hello_env.from.clone();

    // Send HELLO_ACK.
    let ack_wire = encode_envelope(HUB_PEER_ID, &peer_id_from_hello, b"HELLO_ACK");
    if socket.send(Message::Binary(ack_wire)).await.is_err() {
        warn!("phantom_endpoint: failed to send HELLO_ACK to {host}");
        return;
    }

    // ── 2. Registration frame ────────────────────────────────────────────────
    let reg_env = match receive_binary_envelope(&mut socket, &host).await {
        Ok(e) => e,
        Err(reason) => {
            warn!("phantom_endpoint: rejected {host} at registration: {reason}");
            let _ = socket
                .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                    code: 4400,
                    reason: reason.into(),
                })))
                .await;
            return;
        }
    };

    let reg_frame: RegistrationFrame =
        match serde_json::from_slice::<RegistrationFrame>(&reg_env.payload) {
            Ok(f) => f,
            Err(e) => {
                warn!(
                    "phantom_endpoint: malformed registration payload from {host}: {e}"
                );
                let _ = socket
                    .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                        code: 4400,
                        reason: "malformed registration payload".into(),
                    })))
                    .await;
                return;
            }
        };

    // ── 3. JWT verification ──────────────────────────────────────────────────
    let claims = match state.jwt.verify(&reg_frame.device_token) {
        Ok(c) => c,
        Err(AuthError::Expired) => {
            warn!(
                "phantom_endpoint: expired device JWT from {host} (phantom_id={})",
                reg_frame.phantom_id
            );
            let _ = socket
                .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                    code: 4401,
                    reason: "device token expired".into(),
                })))
                .await;
            return;
        }
        Err(_) => {
            warn!(
                "phantom_endpoint: invalid device JWT from {host} (phantom_id={})",
                reg_frame.phantom_id
            );
            let _ = socket
                .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                    code: 4401,
                    reason: "invalid device token".into(),
                })))
                .await;
            return;
        }
    };

    // The JWT sub claim must match the phantom_id in the registration frame.
    if claims.sub != reg_frame.phantom_id {
        warn!(
            "phantom_endpoint: JWT sub ({}) does not match phantom_id ({}) from {host}",
            claims.sub, reg_frame.phantom_id
        );
        let _ = socket
            .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                code: 4401,
                reason: "device token peer_id mismatch".into(),
            })))
            .await;
        return;
    }

    let phantom_id = PhantomId::new(&reg_frame.phantom_id);
    let effective_host = if reg_frame.host.is_empty() {
        host.clone()
    } else {
        reg_frame.host.clone()
    };

    // ── 4. Register in the connection registry ───────────────────────────────
    let (outbound_tx, mut outbound_rx) =
        tokio::sync::mpsc::channel::<JsonRpcRequest>(OUTBOUND_CHANNEL_CAPACITY);

    {
        let mut reg = state.registry.write().await;
        match reg.register(
            phantom_id.clone(),
            outbound_tx,
            effective_host.clone(),
            reg_frame.version.clone(),
        ) {
            Ok(()) => {}
            Err(e) => {
                warn!("phantom_endpoint: register failed for {host}: {e}");
                let _ = socket
                    .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                        code: 4409,
                        reason: e.to_string().into(),
                    })))
                    .await;
                return;
            }
        }
    }

    info!(
        "phantom_endpoint: registered phantom_id={phantom_id} from {host} (jwt_sub={})",
        claims.sub
    );

    // ── 5. Message loop ───────────────────────────────────────────────────────
    loop {
        tokio::select! {
            // Inbound: Phantom → hub (binary envelopes wrapping JsonRpcResponse)
            incoming = socket.recv() => {
                match incoming {
                    None => break,
                    Some(Err(e)) => {
                        warn!("phantom_endpoint: ws error from {phantom_id}: {e}");
                        break;
                    }
                    Some(Ok(Message::Binary(bytes))) => {
                        let bytes = bytes.to_vec();
                        let env = match decode_envelope(&bytes) {
                            Some(e) => e,
                            None => {
                                warn!(
                                    "phantom_endpoint: malformed envelope from {phantom_id}"
                                );
                                continue;
                            }
                        };
                        // Skip control messages (PING/PONG handled by tungstenite).
                        if env.payload == b"PING" || env.payload == b"PONG" {
                            state.registry.write().await.touch(&phantom_id);
                            continue;
                        }
                        match serde_json::from_slice::<JsonRpcResponse>(&env.payload) {
                            Ok(resp) => {
                                deliver_response(&state.registry, &phantom_id, resp).await;
                            }
                            Err(e) => {
                                warn!(
                                    "phantom_endpoint: malformed JsonRpcResponse from {phantom_id}: {e}"
                                );
                            }
                        }
                    }
                    Some(Ok(Message::Ping(data))) => {
                        let _ = socket.send(Message::Pong(data)).await;
                        state.registry.write().await.touch(&phantom_id);
                    }
                    Some(Ok(Message::Close(_))) => break,
                    Some(Ok(_)) => {
                        // Text frames not expected in binary-envelope protocol; skip.
                    }
                }
            }

            // Outbound: hub → Phantom (JsonRpcRequest wrapped in binary envelope)
            outbound = outbound_rx.recv() => {
                match outbound {
                    None => break,
                    Some(req) => {
                        match serde_json::to_vec(&req) {
                            Ok(payload) => {
                                let wire = encode_envelope(
                                    HUB_PEER_ID,
                                    &phantom_id.to_string(),
                                    &payload,
                                );
                                if socket.send(Message::Binary(wire)).await.is_err() {
                                    break;
                                }
                            }
                            Err(e) => {
                                tracing::error!(
                                    "phantom_endpoint: serialisation error for {phantom_id}: {e}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    // ── 6. Cleanup ────────────────────────────────────────────────────────────
    info!("phantom_endpoint: phantom_id={phantom_id} disconnected");
    let state_opt = state.registry.write().await.unregister(&phantom_id);
    drop(state_opt);
}

// ---------------------------------------------------------------------------
// Helper: receive and decode a binary envelope frame
// ---------------------------------------------------------------------------

async fn receive_binary_envelope(
    socket: &mut WebSocket,
    host: &str,
) -> Result<DecodedEnvelope, String> {
    loop {
        match socket.recv().await {
            None => return Err(format!("{host} closed before sending envelope")),
            Some(Err(e)) => {
                return Err(format!("{host} ws error during handshake: {e}"));
            }
            Some(Ok(Message::Binary(bytes))) => {
                return decode_envelope(&bytes).ok_or_else(|| {
                    format!("malformed relay-envelope from {host}")
                });
            }
            Some(Ok(Message::Ping(data))) => {
                // tungstenite auto-pong; continue waiting.
                let _ = socket.send(Message::Pong(data)).await;
            }
            Some(Ok(Message::Close(_))) => {
                return Err(format!("{host} closed during handshake"));
            }
            Some(Ok(_)) => {
                // Text frames are not part of the binary-envelope protocol.
                return Err(format!(
                    "unexpected text/non-binary frame from {host}; binary relay-envelope required"
                ));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Hex decode helpers (carried from auth PR; used by register handler)
// ---------------------------------------------------------------------------

fn hex_decode_exact<const N: usize>(s: &str) -> Result<[u8; N], ()> {
    if s.len() != N * 2 {
        return Err(());
    }
    let mut out = [0u8; N];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        let hi = from_hex_digit(chunk[0]).ok_or(())?;
        let lo = from_hex_digit(chunk[1]).ok_or(())?;
        out[i] = (hi << 4) | lo;
    }
    Ok(out)
}

fn hex_decode_vec(s: &str) -> Result<Vec<u8>, ()> {
    if !s.len().is_multiple_of(2) {
        return Err(());
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    for chunk in s.as_bytes().chunks(2) {
        let hi = from_hex_digit(chunk[0]).ok_or(())?;
        let lo = from_hex_digit(chunk[1]).ok_or(())?;
        out.push((hi << 4) | lo);
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{ApiKeyStore, JwtAuthority, NonceCache};
    use axum::body::Body;
    use axum::http::{Method, Request};
    use std::sync::Arc;
    use tower::ServiceExt;

    const TEST_SECRET: &[u8] = b"phantom-hub-test-secret-for-endpoint-tests";

    fn test_state() -> AppState {
        AppState {
            jwt: Arc::new(JwtAuthority::from_secret(TEST_SECRET)),
            api_keys: Arc::new(ApiKeyStore::default()),
            nonce_cache: Arc::new(NonceCache::new()),
            registry: crate::registry::new_shared_for_tests(),
        }
    }

    /// Build a valid RegisterRequest for `peer_id` using a freshly generated
    /// Ed25519 keypair and the provided `nonce`.
    fn make_register_body_with_nonce(peer_id: &str, nonce: &str) -> RegisterRequest {
        use ed25519_dalek::{Signer, SigningKey};
        use rand::rngs::OsRng;

        let signing_key = SigningKey::generate(&mut OsRng);

        let mut msg = Vec::new();
        msg.extend_from_slice(nonce.as_bytes());
        msg.extend_from_slice(peer_id.as_bytes());
        let sig = signing_key.sign(&msg);

        let pubkey_hex: String = signing_key
            .verifying_key()
            .as_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let nonce_hex: String = nonce.as_bytes().iter().map(|b| format!("{b:02x}")).collect();
        let sig_hex: String = sig.to_bytes().iter().map(|b| format!("{b:02x}")).collect();

        RegisterRequest {
            peer_id: peer_id.to_owned(),
            public_key_hex: pubkey_hex,
            nonce_hex,
            signature_hex: sig_hex,
        }
    }

    fn make_register_body(peer_id: &str) -> RegisterRequest {
        make_register_body_with_nonce(peer_id, "test-nonce-12345")
    }

    // -----------------------------------------------------------------------
    // POST /auth/register — valid signature → 200 + JWT
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn register_valid_signature_returns_jwt() {
        let state = test_state();
        let app = crate::build_router(state);
        let body = make_register_body("test-peer-valid");

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/auth/register")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let r: RegisterResponse = serde_json::from_slice(&bytes).unwrap();
        assert!(!r.device_token.is_empty());
        assert_eq!(r.phantom_id, "test-peer-valid");
        assert!(r.exp > 0);
    }

    #[tokio::test]
    async fn register_tampered_signature_returns_401() {
        let state = test_state();
        let app = crate::build_router(state);
        let mut body = make_register_body("test-peer-tampered");
        body.signature_hex = "aa".repeat(64);

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/auth/register")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    // -----------------------------------------------------------------------
    // P0 regression: replayed nonce returns 409 Conflict
    //
    // Both requests share the same AppState (same NonceCache).  The first
    // succeeds (200); the second is rejected (409) because the nonce was
    // already claimed.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn register_replayed_nonce_returns_409() {
        let state = test_state();
        // Use separate apps but the same state so both share the NonceCache.
        let body = make_register_body_with_nonce("replay-peer", "replay-nonce-xyz");

        let first_resp = crate::build_router(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/auth/register")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        // Replay the identical request (same nonce, same signature).
        let second_resp = crate::build_router(state)
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/auth/register")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            first_resp.status(),
            StatusCode::OK,
            "first registration must succeed"
        );
        assert_eq!(
            second_resp.status(),
            StatusCode::CONFLICT,
            "replayed nonce must return 409 Conflict"
        );
    }

    // -----------------------------------------------------------------------
    // P0 regression: two distinct nonces both succeed
    //
    // Two different valid nonces signed by two different keypairs must both
    // receive 200 — the cache must not block legitimate concurrent registrations.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn register_distinct_nonces_both_succeed() {
        let state = test_state();

        let body_a = make_register_body_with_nonce("peer-alpha", "nonce-alpha-unique-001");
        let body_b = make_register_body_with_nonce("peer-beta", "nonce-beta-unique-002");

        let resp_a = crate::build_router(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/auth/register")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body_a).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        let resp_b = crate::build_router(state)
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/auth/register")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body_b).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            resp_a.status(),
            StatusCode::OK,
            "first distinct nonce registration must succeed"
        );
        assert_eq!(
            resp_b.status(),
            StatusCode::OK,
            "second distinct nonce registration must also succeed"
        );
    }

    // -----------------------------------------------------------------------
    // P0 regression: LRU eviction — oldest entry is evicted when cache is full
    //
    // Uses NonceCache::with_capacity_and_ttl directly (unit-level) to verify
    // that capacity-driven eviction makes the oldest nonce re-claimable while
    // more-recently-used entries remain blocked.
    //
    // Note: This test operates on NonceCache directly (not via HTTP) because
    // driving LRU eviction via the register handler would require
    // NONCE_CACHE_CAPACITY (10 000) round-trips through axum.  The handler
    // integration path is covered by register_replayed_nonce_returns_409.
    //
    // Two-stage design: re-inserting the evicted nonce in the same cache would
    // cascade-evict the next-oldest entry.  Stage 1 verifies the remaining
    // entries are blocked; Stage 2 (fresh cache) verifies the evicted entry is
    // re-claimable.
    // -----------------------------------------------------------------------

    #[test]
    fn nonce_cache_eviction_lru_oldest() {
        use std::time::Duration;
        // ---- Stage 1: middle entries blocked after one eviction ----
        let cache = NonceCache::with_capacity_and_ttl(4, Duration::from_secs(3600));

        // Fill: A(LRU) → B → C → D(MRU).
        assert!(cache.try_claim("evict-nonce-A"), "A: initial claim (1/4)");
        assert!(cache.try_claim("evict-nonce-B"), "B: initial claim (2/4)");
        assert!(cache.try_claim("evict-nonce-C"), "C: initial claim (3/4)");
        assert!(cache.try_claim("evict-nonce-D"), "D: initial claim (4/4)");

        // E evicts A (the LRU).  Cache: B(LRU), C, D, E(MRU).
        assert!(cache.try_claim("evict-nonce-E"), "E: claim evicts A");

        // B, C, D, E still present — must be replay-rejected.
        assert!(!cache.try_claim("evict-nonce-B"), "B still cached — replay");
        assert!(!cache.try_claim("evict-nonce-C"), "C still cached — replay");
        assert!(!cache.try_claim("evict-nonce-D"), "D still cached — replay");
        assert!(!cache.try_claim("evict-nonce-E"), "E still cached — replay");

        // ---- Stage 2: evicted entry is re-claimable (fresh cache) ----
        let cache2 = NonceCache::with_capacity_and_ttl(4, Duration::from_secs(3600));
        cache2.try_claim("evict-nonce-A");
        cache2.try_claim("evict-nonce-B");
        cache2.try_claim("evict-nonce-C");
        cache2.try_claim("evict-nonce-D");
        cache2.try_claim("evict-nonce-E"); // evicts A

        assert!(
            cache2.try_claim("evict-nonce-A"),
            "evicted nonce-A must be re-claimable after LRU eviction"
        );
    }

    // -----------------------------------------------------------------------
    // GET /phantom/connect — no JWT → 401
    // -----------------------------------------------------------------------

    /// Verify that a plain HTTP GET to /phantom/connect (no WebSocket upgrade
    /// headers) is rejected.  The hub returns either 400 (WebSocket upgrade
    /// required — axum rejects the extractor) or 401 (JWT absent/invalid).
    /// Either is a correct rejection for a non-WS request.
    ///
    /// End-to-end 401 behaviour (valid WS headers + bad JWT → 401) is tested
    /// in `tests/registration_handshake.rs` using a real tokio-tungstenite
    /// client that supplies proper WebSocket upgrade headers.
    #[tokio::test]
    async fn connect_without_jwt_rejected() {
        let state = test_state();
        let app = crate::build_router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/phantom/connect")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // 400 = axum rejects the WS upgrade extractor (no Upgrade header);
        // 401 = JWT check runs first and rejects.  Both are correct rejections.
        assert!(
            resp.status() == StatusCode::BAD_REQUEST
                || resp.status() == StatusCode::UNAUTHORIZED,
            "expected 400 or 401, got: {}",
            resp.status()
        );
    }

    #[tokio::test]
    async fn connect_with_invalid_jwt_but_no_ws_headers_rejected() {
        let state = test_state();
        let app = crate::build_router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/phantom/connect")
                    .header("Authorization", "Bearer not.a.valid.jwt")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // Same as above: 400 or 401 is the correct response for a plain HTTP
        // request without WebSocket upgrade headers.
        assert!(
            resp.status() == StatusCode::BAD_REQUEST
                || resp.status() == StatusCode::UNAUTHORIZED,
            "expected 400 or 401, got: {}",
            resp.status()
        );
    }

    #[test]
    fn hex_decode_exact_correct_length() {
        let hex = "deadbeef".repeat(8);
        assert!(hex_decode_exact::<32>(&hex).is_ok());
    }

    #[test]
    fn hex_decode_exact_wrong_length_errors() {
        assert!(hex_decode_exact::<32>("deadbeef").is_err());
    }

    #[test]
    fn encode_decode_envelope_roundtrip() {
        let wire = encode_envelope("hub", "peer-abc", b"HELLO_ACK");
        let decoded = decode_envelope(&wire).expect("round-trip must succeed");
        assert_eq!(decoded.from, "hub");
        assert_eq!(decoded.payload, b"HELLO_ACK");
    }
}
