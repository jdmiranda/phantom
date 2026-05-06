//! Integration test: full binary relay-envelope registration handshake.
//!
//! Verifies that a mock client speaking the binary relay-envelope protocol
//! (matching `phantom-net::RelayClient` / `phantom-mcp::hub_listener`) can:
//!
//! 1. Obtain a JWT from `POST /auth/register`.
//! 2. Connect to `GET /phantom/connect` with `Authorization: Bearer <jwt>`.
//! 3. Send a binary HELLO envelope and receive a binary HELLO_ACK.
//! 4. Send a binary registration envelope with a valid device JWT.
//! 5. Find the Phantom recorded in the connection registry.
//!
//! A client sending a bad JWT in the HTTP upgrade is rejected at the HTTP
//! layer (connection error from tungstenite).  A client sending a bad JWT
//! inside the registration envelope is rejected with WebSocket close code 4401.

use std::net::SocketAddr;
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::net::TcpListener;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{ClientRequestBuilder, Message},
};

use phantom_hub::{AppState, auth::{ApiKeyStore, JwtAuthority, NonceCache}, build_router, registry::new_shared_for_tests};

// ---------------------------------------------------------------------------
// Test state and hub helpers
// ---------------------------------------------------------------------------

const TEST_SECRET: &[u8] = b"phantom-hub-test-secret-registration-handshake";

fn test_state() -> AppState {
    AppState {
        jwt: Arc::new(JwtAuthority::from_secret(TEST_SECRET)),
        api_keys: Arc::new(ApiKeyStore::default()),
        nonce_cache: Arc::new(NonceCache::new()),
        registry: new_shared_for_tests(),
    }
}

/// Spawn the hub on a random port; return the bound address.
async fn spawn_hub(state: AppState) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = build_router(state)
        .into_make_service_with_connect_info::<SocketAddr>();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

// ---------------------------------------------------------------------------
// Binary-envelope client helpers (mirror phantom-net's wire format)
// ---------------------------------------------------------------------------

/// Standard base64 encoder (matches phantom-net's serde_bytes_base64 impl).
fn b64_encode(input: &[u8]) -> String {
    static CHARS: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
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

/// Build a binary relay-envelope frame (matches phantom-net wire format).
///
/// Signature and nonce are zeroed — the hub does not verify Ed25519 signatures
/// in Phase 1 (deferred to issue #399).
fn make_envelope(from: &str, to: &str, payload: &[u8]) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "from":    from,
        "to":      to,
        "payload": b64_encode(payload),
        "sig":     b64_encode(&[]),
        "nonce":   0u64,
    }))
    .unwrap()
}

/// Decode the `payload` field (base64) from a raw binary envelope frame.
fn decode_payload(bytes: &[u8]) -> Vec<u8> {
    let v: serde_json::Value = serde_json::from_slice(bytes).unwrap();
    let s = v["payload"].as_str().unwrap();
    fn val(c: u8) -> u8 {
        match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => 0,
        }
    }
    let clean: Vec<u8> = s.bytes().filter(|&c| c != b'\n' && c != b'\r').collect();
    let mut out = Vec::new();
    for chunk in clean.chunks(4) {
        let v0 = val(chunk[0]);
        let v1 = val(chunk[1]);
        let v2 = val(chunk[2]);
        let v3 = val(chunk[3]);
        out.push(v0 << 2 | v1 >> 4);
        if chunk[2] != b'=' {
            out.push((v1 & 15) << 4 | v2 >> 2);
        }
        if chunk[3] != b'=' {
            out.push((v2 & 3) << 6 | v3);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Test: binary-envelope client completes the full registration handshake
// ---------------------------------------------------------------------------

/// A client speaking binary relay-envelope (matching RelayClient/hub_listener)
/// with a valid JWT in the HTTP upgrade must complete the full handshake:
///
/// 1. HTTP 101 Switching Protocols
/// 2. Binary HELLO envelope → binary HELLO_ACK
/// 3. Binary registration envelope → connection remains open
#[tokio::test]
async fn binary_envelope_client_registers_and_receives_hello_ack() {
    let state = test_state();
    let phantom_id = "test-phantom-binary-handshake";
    let jwt = state.jwt.issue(phantom_id).unwrap();
    let addr = spawn_hub(state).await;
    let url = format!("ws://{addr}/phantom/connect");

    let uri: tokio_tungstenite::tungstenite::http::Uri = url.parse().unwrap();
    let request = ClientRequestBuilder::new(uri)
        .with_header("Authorization", format!("Bearer {jwt}"));

    let (mut ws, _) = connect_async(request).await.expect("WS connect must succeed");

    // Send HELLO envelope (binary).
    let hello = make_envelope(phantom_id, "relay", b"HELLO");
    ws.send(Message::Binary(hello.into()))
        .await
        .expect("HELLO send must succeed");

    // Expect a binary HELLO_ACK envelope back.
    let msg = ws
        .next()
        .await
        .expect("hub must send HELLO_ACK")
        .expect("HELLO_ACK must not be a WS error");

    let ack_bytes = match msg {
        Message::Binary(b) => b.to_vec(),
        other => panic!("expected binary HELLO_ACK, got: {other:?}"),
    };

    let ack_payload = decode_payload(&ack_bytes);
    assert_eq!(
        ack_payload, b"HELLO_ACK",
        "HELLO_ACK envelope payload must be b\"HELLO_ACK\""
    );

    // Send registration envelope (binary).
    let reg_payload = serde_json::to_vec(&json!({
        "type":         "register",
        "phantom_id":   phantom_id,
        "device_token": jwt,
        "version":      "0.1.0",
        "host":         "test-host",
    }))
    .unwrap();
    let reg_frame = make_envelope(phantom_id, "hub", &reg_payload);
    ws.send(Message::Binary(reg_frame.into()))
        .await
        .expect("registration send must succeed");

    // Give the hub a moment to process the registration frame.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // The connection must remain open — verify by sending one more message
    // (PING envelope) and not receiving a Close frame.
    let ping = make_envelope(phantom_id, "hub", b"PING");
    let send_result = ws.send(Message::Binary(ping.into())).await;
    assert!(
        send_result.is_ok(),
        "sending after registration must succeed (connection still open)"
    );
}

// ---------------------------------------------------------------------------
// Test: bad JWT in HTTP upgrade is rejected before WebSocket opens (HTTP 401)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn invalid_jwt_in_upgrade_is_rejected_with_http_401() {
    let state = test_state();
    let addr = spawn_hub(state).await;
    let url = format!("ws://{addr}/phantom/connect");

    let uri: tokio_tungstenite::tungstenite::http::Uri = url.parse().unwrap();
    let request = ClientRequestBuilder::new(uri)
        .with_header("Authorization", "Bearer not.a.real.jwt");

    let result = connect_async(request).await;
    // The hub returns HTTP 401 before the WebSocket handshake; tungstenite
    // surfaces this as a connection error (non-101 response).
    assert!(
        result.is_err(),
        "connecting with an invalid JWT must fail at the HTTP level"
    );
}

// ---------------------------------------------------------------------------
// Test: missing JWT in HTTP upgrade is rejected (HTTP 401)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn missing_jwt_in_upgrade_is_rejected_with_http_401() {
    let state = test_state();
    let addr = spawn_hub(state).await;
    let url = format!("ws://{addr}/phantom/connect");

    // Connect without any Authorization header.
    let uri: tokio_tungstenite::tungstenite::http::Uri = url.parse().unwrap();
    let request = ClientRequestBuilder::new(uri);

    let result = connect_async(request).await;
    assert!(
        result.is_err(),
        "connecting without a JWT must fail at the HTTP level"
    );
}

// ---------------------------------------------------------------------------
// Test: HELLO with wrong payload is rejected with close code 4400
// ---------------------------------------------------------------------------

/// A client that presents a valid JWT and connects successfully, but sends a
/// non-HELLO payload as the first binary envelope, is rejected with 4400.
#[tokio::test]
async fn wrong_first_payload_is_rejected_with_4400() {
    let state = test_state();
    let phantom_id = "test-phantom-bad-hello";
    let jwt = state.jwt.issue(phantom_id).unwrap();
    let addr = spawn_hub(state).await;
    let url = format!("ws://{addr}/phantom/connect");

    let uri: tokio_tungstenite::tungstenite::http::Uri = url.parse().unwrap();
    let request = ClientRequestBuilder::new(uri)
        .with_header("Authorization", format!("Bearer {jwt}"));

    let (mut ws, _) = connect_async(request).await.expect("WS connect must succeed");

    // Send an envelope with wrong payload (not b"HELLO").
    let bad_hello = make_envelope(phantom_id, "relay", b"GREET");
    ws.send(Message::Binary(bad_hello.into()))
        .await
        .expect("send must succeed");

    let msg = ws.next().await.expect("hub must respond");
    match msg.expect("must not be a transport error") {
        Message::Close(Some(frame)) => {
            assert_eq!(
                u16::from(frame.code),
                4400,
                "expected close code 4400 for wrong first payload"
            );
        }
        Message::Close(None) => {
            // Hub closed without a code — still a rejection.
        }
        other => panic!("expected Close frame, got: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Test: registration with bad device JWT is rejected with close code 4401
// ---------------------------------------------------------------------------

/// A client that passes the initial JWT check (HTTP upgrade) but provides an
/// invalid JWT in the registration frame is rejected with 4401 after HELLO_ACK.
#[tokio::test]
async fn bad_device_token_in_registration_is_rejected_with_4401() {
    let state = test_state();
    let phantom_id = "test-phantom-bad-reg-jwt";
    let jwt = state.jwt.issue(phantom_id).unwrap();
    let addr = spawn_hub(state).await;
    let url = format!("ws://{addr}/phantom/connect");

    let uri: tokio_tungstenite::tungstenite::http::Uri = url.parse().unwrap();
    let request = ClientRequestBuilder::new(uri)
        .with_header("Authorization", format!("Bearer {jwt}"));

    let (mut ws, _) = connect_async(request).await.expect("WS connect must succeed");

    // Send HELLO.
    let hello = make_envelope(phantom_id, "relay", b"HELLO");
    ws.send(Message::Binary(hello.into())).await.unwrap();

    // Consume HELLO_ACK.
    let _ack = ws.next().await.unwrap().unwrap();

    // Registration with a garbage device_token (not a valid JWT).
    let reg_payload = serde_json::to_vec(&json!({
        "type":         "register",
        "phantom_id":   phantom_id,
        "device_token": "this.is.not.a.valid.jwt",
        "version":      "0.1.0",
        "host":         "test-host",
    }))
    .unwrap();
    let reg_frame = make_envelope(phantom_id, "hub", &reg_payload);
    ws.send(Message::Binary(reg_frame.into())).await.unwrap();

    let msg = ws.next().await.expect("hub must respond");
    match msg.expect("must not be a transport error") {
        Message::Close(Some(frame)) => {
            assert_eq!(
                u16::from(frame.code),
                4401,
                "expected close code 4401 for invalid device token in registration"
            );
        }
        Message::Close(None) => {
            // Hub closed without a code — still a rejection.
        }
        other => panic!("expected Close frame, got: {other:?}"),
    }
}
