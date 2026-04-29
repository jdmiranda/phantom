//! WebSocket server: accepts connections and drives per-peer tasks.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use log::{error, info, warn};
use serde::{Deserialize, Serialize};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio_tungstenite::{accept_async, tungstenite::Message};

use crate::envelope::{ClientMessage, PeerId, RelayMessage};
use crate::router::Router;
use crate::session::Session;

// ── Handshake types ──────────────────────────────────────────────────────────

/// First message a connecting client must send.
#[derive(Debug, Deserialize)]
struct IdentityProof {
    /// Requested peer identity string.
    peer_id: String,
    /// Client-provided proof (e.g. JWT, bearer token, or Ed25519 sig).
    /// Currently accepted as-is; real auth can be layered here.
    proof: String,
}

/// Relay's response to a successful handshake.
#[derive(Debug, Serialize)]
struct HandshakeAck {
    session_token: String,
    peer_id: String,
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

    // ── 1. Handshake ──────────────────────────────────────────────────────────
    let raw = match ws_source.next().await {
        Some(Ok(msg)) => msg,
        Some(Err(e)) => return Err(e.into()),
        None => {
            warn!("peer {} closed before handshake", peer_addr);
            return Ok(());
        }
    };

    let proof: IdentityProof = match raw {
        Message::Text(text) => serde_json::from_str(&text)?,
        other => {
            warn!("unexpected handshake message type from {}: {:?}", peer_addr, other);
            return Ok(());
        }
    };

    // Minimal proof validation placeholder — real implementations can verify
    // an Ed25519 or JWT here.
    if proof.proof.is_empty() {
        warn!("empty proof from {}; rejecting", peer_addr);
        let err = RelayMessage::Error {
            code: "auth_failed".into(),
            message: "empty identity proof".into(),
        };
        ws_sink
            .send(Message::from(serde_json::to_string(&err)?))
            .await?;
        return Ok(());
    }

    let peer_id = PeerId(proof.peer_id.clone());
    let session = Session::new(peer_id.clone());
    let token = session.token;
    let handle = session.handle();
    let mut outbound_rx = session.rx;

    {
        let mut guard = router.lock().await;
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

                        let _ = ws_sink
                            .send(Message::from(serde_json::to_string(&reply)?))
                            .await;
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
    Ok(())
}
