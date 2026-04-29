//! Integration tests for `phantom-relay`.
//!
//! Spins up a real in-process relay server over an ephemeral TCP port and
//! exercises the full WebSocket wire protocol using mock `phantom-net` clients.
//!
//! Tests:
//!   1. Two peers rendezvous and exchange a round-trip message.
//!   2. The rate limiter trips on a burst sender without dropping the connection.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use uuid::Uuid;

use phantom_relay::router::Router;
use phantom_relay::server::run_with_listener;

// ── Test helpers ──────────────────────────────────────────────────────────────

/// Bind to an ephemeral port, spawn the relay, and return the bound address.
async fn spawn_relay(rate_limit_per_sec: u32) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = Arc::new(Mutex::new(Router::new(rate_limit_per_sec, 100)));
    tokio::spawn(run_with_listener(listener, router));
    addr
}

type WsSink = futures_util::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    Message,
>;
type WsStream = futures_util::stream::SplitStream<
    tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
>;

/// Connect a WebSocket client, perform the relay handshake, return sink/stream.
async fn handshake(addr: SocketAddr, peer_id: &str) -> (WsSink, WsStream) {
    let url = format!("ws://{}", addr);
    let (ws, _) = connect_async(url).await.expect("ws connect failed");
    let (mut sink, mut stream) = ws.split();

    sink.send(Message::from(
        json!({ "peer_id": peer_id, "proof": "test-proof" }).to_string(),
    ))
    .await
    .unwrap();

    let ack_raw = stream.next().await.unwrap().unwrap();
    let ack: Value = match ack_raw {
        Message::Text(t) => serde_json::from_str(&t).unwrap(),
        other => panic!("expected handshake ack, got {:?}", other),
    };
    assert_eq!(
        ack["peer_id"].as_str().unwrap(),
        peer_id,
        "handshake ack peer_id mismatch"
    );
    assert!(
        ack["session_token"].is_string(),
        "missing session_token in ack"
    );

    (sink, stream)
}

/// Receive the next text frame and parse as JSON; skip non-text frames.
async fn recv_json(stream: &mut WsStream) -> Value {
    loop {
        let raw = tokio::time::timeout(Duration::from_secs(3), stream.next())
            .await
            .expect("timeout waiting for message")
            .unwrap()
            .unwrap();
        if let Message::Text(t) = raw {
            return serde_json::from_str(&t).expect("invalid JSON in text frame");
        }
    }
}

/// Construct a `send` envelope JSON in the relay wire format.
///
/// `PeerId` is a newtype wrapping `String`; serde serialises it as a plain
/// JSON string, not an object.
fn make_send(from: &str, to: &str, payload: &str) -> String {
    json!({
        "type": "send",
        "from": from,
        "to":   to,
        "payload": payload,
        "sig":   "test-sig",
        "nonce": Uuid::new_v4().to_string()
    })
    .to_string()
}

// ── Test 1 — round-trip rendezvous ────────────────────────────────────────────

#[tokio::test]
async fn two_peers_rendezvous_round_trip() {
    let addr = spawn_relay(100).await;
    tokio::time::sleep(Duration::from_millis(30)).await;

    let (mut alice_sink, mut alice_stream) = handshake(addr, "alice").await;
    let (mut bob_sink, mut bob_stream) = handshake(addr, "bob").await;

    // Alice → Bob
    let nonce_ab = Uuid::new_v4().to_string();
    let env_ab = json!({
        "type": "send",
        "from": "alice",
        "to":   "bob",
        "payload": "hello bob",
        "sig":   "sig",
        "nonce": nonce_ab
    })
    .to_string();

    alice_sink
        .send(Message::from(env_ab))
        .await
        .unwrap();

    // Alice receives Delivered.
    let alice_reply = recv_json(&mut alice_stream).await;
    assert_eq!(alice_reply["type"], "delivered", "alice: {}", alice_reply);
    assert_eq!(alice_reply["nonce"], nonce_ab, "delivered nonce mismatch");

    // Bob receives the forwarded envelope.
    let bob_recv = recv_json(&mut bob_stream).await;
    assert_eq!(bob_recv["type"], "send", "bob: {}", bob_recv);
    assert_eq!(bob_recv["payload"], "hello bob");

    // Bob → Alice (return leg)
    let nonce_ba = Uuid::new_v4().to_string();
    let env_ba = json!({
        "type": "send",
        "from": "bob",
        "to":   "alice",
        "payload": "hello alice",
        "sig":   "sig",
        "nonce": nonce_ba
    })
    .to_string();

    bob_sink
        .send(Message::from(env_ba))
        .await
        .unwrap();

    let bob_reply = recv_json(&mut bob_stream).await;
    assert_eq!(bob_reply["type"], "delivered", "bob return: {}", bob_reply);

    let alice_recv = recv_json(&mut alice_stream).await;
    assert_eq!(alice_recv["type"], "send", "alice recv: {}", alice_recv);
    assert_eq!(alice_recv["payload"], "hello alice");
}

// ── Test 2 — rate limiter trips, connection survives ─────────────────────────

#[tokio::test]
async fn rate_limiter_trips_on_burst_without_disconnect() {
    // 3 messages per second so we exhaust the bucket quickly.
    let addr = spawn_relay(3).await;
    tokio::time::sleep(Duration::from_millis(30)).await;

    let (mut alice_sink, mut alice_stream) = handshake(addr, "burst-alice").await;
    let (_bob_sink, _bob_stream) = handshake(addr, "burst-bob").await;

    let mut delivered = 0usize;
    let mut rate_limited = 0usize;

    for _ in 0..6 {
        alice_sink
            .send(Message::from(make_send("burst-alice", "burst-bob", "spam")))
            .await
            .unwrap();

        let reply = recv_json(&mut alice_stream).await;
        match reply["type"].as_str().unwrap() {
            "delivered" => delivered += 1,
            "rate_limit_exceeded" => {
                let retry_ms = reply["retry_after_ms"]
                    .as_u64()
                    .expect("retry_after_ms must be present");
                assert!(retry_ms > 0, "retry_after_ms must be positive");
                rate_limited += 1;
            }
            other => panic!("unexpected reply type '{}': {}", other, reply),
        }
    }

    assert!(
        delivered >= 3,
        "expected at least 3 delivered, got {}",
        delivered
    );
    assert!(
        rate_limited >= 1,
        "expected at least 1 rate_limit_exceeded, got {}",
        rate_limited
    );

    // The connection must still be alive after being rate-limited.
    alice_sink
        .send(Message::from(make_send(
            "burst-alice",
            "burst-bob",
            "still-alive",
        )))
        .await
        .expect("connection should remain open after rate limiting");
}
