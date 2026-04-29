//! Integration tests: mock relay handshake and two-client message exchange.
//!
//! A lightweight in-process WebSocket "relay" server is started on a random
//! OS-assigned port for each test.  The relay:
//!
//! 1. Receives a `HELLO` envelope from a connecting client.
//! 2. Responds with a `HELLO_ACK` envelope.
//! 3. Forwards all subsequent messages to the registered peers.
//!
//! This exercises the full handshake and send/recv path without a real relay
//! server.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio_tungstenite::{accept_async, tungstenite::Message};

use crate::{
    envelope::Envelope,
    identity::Identity,
};

// ---------------------------------------------------------------------------
// Mock relay server
// ---------------------------------------------------------------------------

type Sender = tokio::sync::mpsc::UnboundedSender<Vec<u8>>;

/// Spawn an in-process mock relay on a random port and return the bound address.
///
/// The relay performs the HELLO/HELLO_ACK handshake, then routes binary frames
/// to all other registered peers by peer_id (from the envelope's `to` field).
async fn spawn_mock_relay() -> Result<SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;

    let peers: Arc<Mutex<HashMap<String, Sender>>> = Arc::new(Mutex::new(HashMap::new()));

    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let peers = Arc::clone(&peers);
            tokio::spawn(handle_connection(stream, peers));
        }
    });

    Ok(addr)
}

async fn handle_connection(
    stream: TcpStream,
    peers: Arc<Mutex<HashMap<String, Sender>>>,
) {
    let mut ws = match accept_async(stream).await {
        Ok(ws) => ws,
        Err(_) => return,
    };

    // Step 1: receive HELLO.
    let hello_bytes = loop {
        match ws.next().await {
            Some(Ok(Message::Binary(b))) => break b.to_vec(),
            Some(Ok(_)) => continue,
            _ => return,
        }
    };

    let hello_env = match Envelope::from_wire(&hello_bytes) {
        Ok(e) => e,
        Err(_) => return,
    };

    if hello_env.payload != b"HELLO" {
        return;
    }

    let peer_id = hello_env.from.clone();

    // Step 2: send HELLO_ACK back to the client.
    // We fabricate the relay's response — the relay has no real identity in
    // this mock, so we reuse a throwaway identity.
    let relay_id = Identity::generate_ephemeral();
    let client_peer = crate::identity::PeerId::from_raw(peer_id.clone());
    let ack = Envelope::new(&relay_id, &client_peer, b"HELLO_ACK".to_vec(), 0);
    let ack_wire = match ack.to_wire() {
        Ok(b) => b,
        Err(_) => return,
    };
    if ws.send(Message::Binary(ack_wire.into())).await.is_err() {
        return;
    }

    // Step 3: register peer and forward messages.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    peers.lock().await.insert(peer_id.clone(), tx);

    // Split WS into sender/receiver.
    let (mut ws_tx, mut ws_rx) = ws.split();

    let peers_clone = Arc::clone(&peers);
    tokio::spawn(async move {
        // Forward outbound messages from the relay queue to this connection.
        while let Some(msg) = rx.recv().await {
            let _ = ws_tx.send(Message::Binary(msg.into())).await;
        }
    });

    // Read incoming frames and route them.
    while let Some(Ok(Message::Binary(bytes))) = ws_rx.next().await {
        let bytes = bytes.to_vec();
        let env = match Envelope::from_wire(&bytes) {
            Ok(e) => e,
            Err(_) => continue,
        };

        // PING → PONG (relay ↔ client keepalive).
        if env.payload == b"PING" {
            let pong = Envelope::new(
                &relay_id,
                &crate::identity::PeerId::from_raw(env.from.clone()),
                b"PONG".to_vec(),
                0,
            );
            if let Ok(wire) = pong.to_wire() {
                if let Some(tx) = peers_clone.lock().await.get(&env.from) {
                    let _ = tx.send(wire);
                }
            }
            continue;
        }

        // Route to recipient.
        if let Some(tx) = peers_clone.lock().await.get(&env.to) {
            let _ = tx.send(bytes);
        }
    }

    peers.lock().await.remove(&peer_id);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mock_relay_handshake_completes() {
    let addr = spawn_mock_relay().await.unwrap();
    let id = Identity::generate_ephemeral();
    let url = format!("ws://{addr}");

    let client = crate::client::RelayClient::connect(&url, id).await.unwrap();
    assert!(
        client.is_connected(),
        "client should be connected after successful handshake"
    );
}

#[tokio::test]
async fn two_clients_exchange_one_message() {
    let addr = spawn_mock_relay().await.unwrap();

    let alice = Identity::generate_ephemeral();
    let bob = Identity::generate_ephemeral();

    let alice_peer = alice.peer_id.clone();
    let bob_peer = bob.peer_id.clone();

    let url = format!("ws://{addr}");

    let mut alice_client =
        crate::client::RelayClient::connect(&url, alice).await.unwrap();
    let mut bob_client =
        crate::client::RelayClient::connect(&url, bob).await.unwrap();

    // Give the relay a moment to register both peers before routing.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let payload = b"hello from alice to bob".to_vec();
    alice_client
        .send(&bob_peer, payload.clone())
        .await
        .unwrap();

    // Bob receives the envelope.
    let received = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        bob_client.recv(),
    )
    .await
    .expect("recv timed out")
    .expect("recv failed");

    assert_eq!(received.from, alice_peer.to_string());
    assert_eq!(received.payload, payload);
}
