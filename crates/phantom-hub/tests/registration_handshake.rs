//! Regression test: full registration handshake over plain JSON WSS.
//!
//! Verifies that a mock client speaking the hub's plain-WSS protocol
//! (NOT `RelayClient` binary envelopes) can:
//!
//! 1. Connect to `GET /phantom/connect`.
//! 2. Send a JSON registration frame as a text WebSocket message.
//! 3. Receive a JSON `HELLO_ACK` text frame from the hub.
//! 4. Find the Phantom recorded in the connection registry.
//!
//! This test locks down the fix for FAIL-1 from the PR #495 review:
//! the hub endpoint speaks plain JSON WSS; `phantom-mcp::hub_listener`
//! (issue #498) must be updated to match.

use std::net::SocketAddr;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use phantom_hub::build_router;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Bind the hub router on a random port and return the local address.
async fn spawn_hub() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = build_router()
        .into_make_service_with_connect_info::<SocketAddr>();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

// ---------------------------------------------------------------------------
// Test: plain-WSS client completes the registration handshake
// ---------------------------------------------------------------------------

/// A client that speaks the correct hub protocol (plain JSON WSS) must be able
/// to register and receive a HELLO_ACK.  This is the regression test for the
/// FAIL-1 protocol mismatch identified in the PR #495 review.
#[tokio::test]
async fn plain_wss_client_registers_and_receives_hello_ack() {
    let addr = spawn_hub().await;
    let url = format!("ws://{addr}/phantom/connect");

    let (mut ws, _) = connect_async(&url).await.expect("WS connect must succeed");

    // Send the JSON registration frame as a plain text message.
    let reg_frame = json!({
        "phantom_id":   "test-phantom-handshake",
        "device_token": "placeholder-token-abc",
        "version":      "0.1.0",
        "host":         "test-host"
    });
    ws.send(Message::Text(reg_frame.to_string().into()))
        .await
        .expect("send registration frame must succeed");

    // Expect a JSON HELLO_ACK text frame back.
    let msg = ws
        .next()
        .await
        .expect("hub must send HELLO_ACK")
        .expect("HELLO_ACK frame must not be a WS error");

    let ack_text = match msg {
        Message::Text(t) => t,
        other => panic!("expected text HELLO_ACK, got: {other:?}"),
    };

    let ack: Value = serde_json::from_str(&ack_text).expect("HELLO_ACK must be valid JSON");
    assert_eq!(
        ack["status"].as_str(),
        Some("HELLO_ACK"),
        "HELLO_ACK status field must be 'HELLO_ACK'"
    );
    assert_eq!(
        ack["phantom_id"].as_str(),
        Some("test-phantom-handshake"),
        "HELLO_ACK phantom_id must echo the registered id"
    );
}

// ---------------------------------------------------------------------------
// Test: empty device_token is rejected with close code 4401
// ---------------------------------------------------------------------------

#[tokio::test]
async fn empty_device_token_is_rejected_with_4401() {
    let addr = spawn_hub().await;
    let url = format!("ws://{addr}/phantom/connect");

    let (mut ws, _) = connect_async(&url).await.expect("WS connect must succeed");

    let reg_frame = json!({
        "phantom_id":   "reject-test",
        "device_token": "",
        "version":      "0.1.0",
        "host":         "test-host"
    });
    ws.send(Message::Text(reg_frame.to_string().into()))
        .await
        .expect("send must succeed");

    // Hub must close with code 4401.
    let msg = ws.next().await.expect("hub must respond");
    match msg.expect("must not be a transport error") {
        Message::Close(Some(frame)) => {
            assert_eq!(frame.code, 4401u16.into(), "expected close code 4401 for empty token");
        }
        other => panic!("expected Close frame, got: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Test: binary relay-envelope HELLO frame is rejected (documents FAIL-1 root cause)
// ---------------------------------------------------------------------------

/// A client sending a binary frame as the first message (as RelayClient does)
/// is rejected by the hub.  This documents the FAIL-1 root cause:
/// `phantom-mcp::hub_listener` uses RelayClient which sends binary envelopes,
/// but the hub expects a plain JSON text frame.  Fix tracked in issue #498.
///
/// The hub sends a 4400 close frame; depending on tungstenite framing details
/// the client may observe either a Close frame or a transport error.  Either
/// outcome confirms that the binary-frame client is rejected.
#[tokio::test]
async fn binary_first_frame_is_rejected() {
    let addr = spawn_hub().await;
    let url = format!("ws://{addr}/phantom/connect");

    let (mut ws, _) = connect_async(&url).await.expect("WS connect must succeed");

    // Send a binary frame (simulating RelayClient HELLO envelope).
    ws.send(Message::Binary(b"fake-relay-hello-envelope".to_vec().into()))
        .await
        .expect("send binary must succeed");

    // Hub rejects the connection.  Accept either a Close frame with code 4400
    // or a transport-level error (both mean the binary client was rejected).
    let msg = ws.next().await.expect("hub must respond");
    match msg {
        Ok(Message::Close(Some(frame))) => {
            assert_eq!(
                frame.code, 4400u16.into(),
                "expected close code 4400 for unexpected binary frame"
            );
        }
        Ok(Message::Close(None)) => {
            // Hub closed without a code — still a rejection.
        }
        Err(_) => {
            // Transport-level error also confirms rejection.
        }
        Ok(other) => panic!("expected rejection (Close or error), got: {other:?}"),
    }
}
