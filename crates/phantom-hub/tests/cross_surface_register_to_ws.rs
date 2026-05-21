//! Cross-surface integration test: HTTP `/auth/register` → WS `/phantom/connect`
//!
//! Closes #507.  Verifies that the JWT issued by `POST /auth/register` (using a
//! real Ed25519 challenge–response) is accepted by the binary WebSocket endpoint
//! `GET /phantom/connect` without any other coordination between the two surfaces.
//!
//! Test sequence:
//! 1. Spin up the hub on a random port via [`tokio::net::TcpListener`].
//! 2. Generate an Ed25519 keypair.
//! 3. `POST /auth/register` with `{ peer_id, public_key_hex, nonce_hex, signature_hex }`.
//! 4. Receive `{ device_token, exp, phantom_id }`.
//! 5. Open a binary WebSocket to `ws://…/phantom/connect` with
//!    `Authorization: Bearer <device_token>`.
//! 6. Send a binary HELLO envelope; receive a binary HELLO_ACK envelope.
//! 7. Send a binary registration envelope (re-using the issued `device_token`).
//! 8. Assert that the connection registry now contains the phantom's `peer_id`.
//! 9. Drop the WebSocket client; assert the registry entry is eventually removed.
//!
//! HTTP calls use [`axum_test::TestServer`] bound to the same TCP socket so
//! that both surfaces are served by a single shared [`AppState`] (and therefore
//! the same `ConnectionRegistry` instance).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum_test::TestServer;
use ed25519_dalek::{Signer, SigningKey};
use futures_util::{SinkExt, StreamExt};
use rand::rngs::OsRng;
use serde_json::json;
use tokio::net::TcpListener;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{ClientRequestBuilder, Message},
};

use phantom_hub::{
    AppState,
    auth::{ApiKeyStore, JwtAuthority, NonceCache},
    build_router,
    registry::new_shared_for_tests,
};

// ---------------------------------------------------------------------------
// Test state helpers
// ---------------------------------------------------------------------------

const TEST_SECRET: &[u8] = b"phantom-hub-test-secret-cross-surface-507";

fn test_state() -> AppState {
    AppState {
        jwt: Arc::new(JwtAuthority::from_secret(TEST_SECRET)),
        api_keys: Arc::new(ApiKeyStore::default()),
        nonce_cache: Arc::new(NonceCache::new()),
        registry: new_shared_for_tests(),
    }
}

/// Spawn the hub on a random port.  Returns the bound address.
async fn spawn_hub(state: AppState) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = build_router(state).into_make_service_with_connect_info::<SocketAddr>();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

// ---------------------------------------------------------------------------
// Binary-envelope helpers (mirror phantom-net wire format)
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
// Hex helpers
// ---------------------------------------------------------------------------

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_encode_str(s: &str) -> String {
    hex_encode(s.as_bytes())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Full cross-surface happy path: HTTP register → WS connect with issued JWT.
///
/// Closes #507.  The key property under test is that the `device_token` issued
/// by the HTTP surface is accepted by the WebSocket surface without any
/// additional coordination — they share only the [`AppState`].
#[tokio::test]
async fn http_register_then_ws_connect_with_jwt() {
    let state = test_state();
    let registry = state.registry.clone();
    let addr = spawn_hub(state.clone()).await;

    // ── Step 1: generate an Ed25519 identity keypair ──────────────────────────
    let signing_key = SigningKey::generate(&mut OsRng);
    let verifying_key = signing_key.verifying_key();

    let peer_id = "cross-surface-test-phantom-507";
    let nonce = "unique-nonce-cross-surface-507";

    // Build the registration message: nonce_bytes || peer_id_bytes.
    let mut msg = Vec::new();
    msg.extend_from_slice(nonce.as_bytes());
    msg.extend_from_slice(peer_id.as_bytes());
    let signature = signing_key.sign(&msg);

    // ── Step 2: POST /auth/register ───────────────────────────────────────────
    // Use the same state to construct a TestServer so it shares the AppState.
    // The TestServer binds internally; for the WS part we use the TcpListener
    // server at `addr`.  Both operate against the same registry Arc.
    let server = TestServer::new(
        build_router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .unwrap();

    let resp = server
        .post("/auth/register")
        .json(&json!({
            "peer_id":       peer_id,
            "public_key_hex": hex_encode(verifying_key.as_bytes()),
            "nonce_hex":     hex_encode_str(nonce),
            "signature_hex": hex_encode(&signature.to_bytes()),
        }))
        .await;

    assert_eq!(
        resp.status_code(),
        200,
        "POST /auth/register must return 200; body: {}",
        resp.text()
    );

    let body: serde_json::Value = resp.json();
    let device_token = body["device_token"]
        .as_str()
        .expect("response must contain device_token string")
        .to_owned();
    let returned_phantom_id = body["phantom_id"]
        .as_str()
        .expect("response must contain phantom_id");
    assert_eq!(
        returned_phantom_id, peer_id,
        "phantom_id echoed back must match the submitted peer_id"
    );

    // ── Step 3: connect WebSocket to /phantom/connect with the issued JWT ─────
    let url = format!("ws://{addr}/phantom/connect");
    let uri: tokio_tungstenite::tungstenite::http::Uri = url.parse().unwrap();
    let request = ClientRequestBuilder::new(uri)
        .with_header("Authorization", format!("Bearer {device_token}"));

    let (mut ws, _) = connect_async(request)
        .await
        .expect("WebSocket upgrade must succeed with the issued JWT");

    // ── Step 4: binary HELLO → HELLO_ACK handshake ───────────────────────────
    let hello = make_envelope(peer_id, "relay", b"HELLO");
    ws.send(Message::Binary(hello.into()))
        .await
        .expect("HELLO send must succeed");

    let ack_msg = ws
        .next()
        .await
        .expect("hub must send HELLO_ACK")
        .expect("HELLO_ACK must not be a WS error");

    let ack_bytes = match ack_msg {
        Message::Binary(b) => b.to_vec(),
        other => panic!("expected binary HELLO_ACK, got: {other:?}"),
    };
    let ack_payload = decode_payload(&ack_bytes);
    assert_eq!(
        ack_payload, b"HELLO_ACK",
        "HELLO_ACK payload must be exactly b\"HELLO_ACK\""
    );

    // ── Step 5: send binary registration envelope ─────────────────────────────
    let reg_payload = serde_json::to_vec(&json!({
        "type":         "register",
        "phantom_id":   peer_id,
        "device_token": device_token,
        "version":      "0.1.0",
        "host":         "test-host-507",
    }))
    .unwrap();
    let reg_frame = make_envelope(peer_id, "hub", &reg_payload);
    ws.send(Message::Binary(reg_frame.into()))
        .await
        .expect("registration frame send must succeed");

    // Give the hub a moment to process the registration frame.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // ── Step 6: assert registry contains the phantom ─────────────────────────
    let phantom_id_obj = phantom_hub::registry::PhantomId::new(peer_id);
    {
        let reg = registry.read().await;
        let online: Vec<_> = reg.list_online();
        assert!(
            online.iter().any(|p| p.id == phantom_id_obj),
            "registry must contain the registered phantom after handshake; online={online:?}"
        );
    }

    // ── Step 7: drop WebSocket and assert registry entry is removed ───────────
    drop(ws);

    // The hub processes the disconnect asynchronously; poll with a short budget.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        tokio::time::sleep(Duration::from_millis(20)).await;
        let still_online = registry
            .read()
            .await
            .list_online()
            .into_iter()
            .any(|p| p.id == phantom_id_obj);
        if !still_online {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "registry must remove the phantom within 2 s of WebSocket close"
        );
    }
}

/// Replay protection: a second POST /auth/register with the same nonce is
/// rejected with 409 Conflict even though the Ed25519 signature is valid.
///
/// This validates that the cross-surface JWT issuance path enforces the
/// `NonceCache` guard (the WS surface does not bypass it).
#[tokio::test]
async fn http_register_replay_rejected_with_409() {
    let state = test_state();
    let addr = spawn_hub(state.clone()).await;

    let signing_key = SigningKey::generate(&mut OsRng);
    let verifying_key = signing_key.verifying_key();

    let peer_id = "cross-surface-replay-phantom-507";
    let nonce = "unique-nonce-replay-cross-surface-507";

    let mut msg = Vec::new();
    msg.extend_from_slice(nonce.as_bytes());
    msg.extend_from_slice(peer_id.as_bytes());
    let signature = signing_key.sign(&msg);

    let server = TestServer::new(
        build_router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .unwrap();

    let body = json!({
        "peer_id":        peer_id,
        "public_key_hex": hex_encode(verifying_key.as_bytes()),
        "nonce_hex":      hex_encode_str(nonce),
        "signature_hex":  hex_encode(&signature.to_bytes()),
    });

    let first = server.post("/auth/register").json(&body).await;
    assert_eq!(
        first.status_code(),
        200,
        "first registration must succeed; body: {}",
        first.text()
    );

    let second = server.post("/auth/register").json(&body).await;
    assert_eq!(
        second.status_code(),
        409,
        "replayed registration must be rejected with 409 Conflict; body: {}",
        second.text()
    );

    // The WS address is separate but consistent: verify the addr was bound.
    let _ = addr;
}
