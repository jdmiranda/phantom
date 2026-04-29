//! Relay client — connect, send, receive, and heartbeat.
//!
//! [`RelayClient`] is the primary entry point for Phantom federation.  It
//! manages a single WebSocket connection to a relay server, wraps outgoing
//! messages in signed [`Envelope`]s, and surfaces incoming envelopes to the
//! caller.
//!
//! # Handshake
//! On connect, the client immediately sends a `HELLO` envelope addressed to
//! the relay's own peer-id (`"relay"`).  The relay is expected to echo back a
//! `HELLO_ACK`.  Until the ack is received, [`RelayClient::is_connected`]
//! returns `false`.
//!
//! # Example
//! ```rust,no_run
//! use phantom_net::{identity::Identity, client::RelayClient};
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let alice = Identity::load_or_generate("phantom")?;
//!     let bob   = Identity::load_or_generate("phantom-bob")?;
//!
//!     let mut client = RelayClient::connect("wss://relay.example.com", alice).await?;
//!     client.send(&bob.peer_id, b"hello peer".to_vec()).await?;
//!     let env = client.recv().await?;
//!     println!("got message from {}", env.from);
//!     Ok(())
//! }
//! ```

use anyhow::{Context, Result};
use log::{debug, warn};
use tokio::time::{sleep_until, Instant as TokioInstant};

use crate::{
    envelope::Envelope,
    heartbeat::{Heartbeat, HeartbeatAction},
    identity::{Identity, PeerId},
    transport::WsTransport,
};

// ---------------------------------------------------------------------------
// Nonce counter
// ---------------------------------------------------------------------------

/// A simple monotonic nonce counter per client session.
struct NonceCounter(u64);

impl NonceCounter {
    fn next(&mut self) -> u64 {
        let n = self.0;
        self.0 = self.0.wrapping_add(1);
        n
    }
}

// ---------------------------------------------------------------------------
// RelayClient
// ---------------------------------------------------------------------------

/// Authenticated relay client for Phantom peer-to-peer messaging.
pub struct RelayClient {
    identity: Identity,
    transport: WsTransport,
    relay_url: String,
    nonce: NonceCounter,
    heartbeat: Heartbeat,
    handshake_complete: bool,
}

/// Well-known peer-id string that the relay server registers under.
const RELAY_PEER_ID: &str = "relay";

impl RelayClient {
    /// Connect to the relay at `relay_url` and complete the handshake.
    ///
    /// Sends a `HELLO` control envelope and waits for a `HELLO_ACK` response.
    pub async fn connect(relay_url: &str, identity: Identity) -> Result<Self> {
        let transport = WsTransport::connect(relay_url)
            .await
            .with_context(|| format!("failed to connect to relay: {relay_url}"))?;

        let mut client = Self {
            identity,
            transport,
            relay_url: relay_url.to_owned(),
            nonce: NonceCounter(0),
            heartbeat: Heartbeat::new(),
            handshake_complete: false,
        };

        client.do_handshake().await?;
        Ok(client)
    }

    /// Send `payload` bytes to `to`.
    ///
    /// The payload is wrapped in a signed [`Envelope`] before transmission.
    pub async fn send(&mut self, to: &PeerId, payload: Vec<u8>) -> Result<()> {
        let nonce = self.nonce.next();
        let env = Envelope::new(&self.identity, to, payload, nonce);
        let wire = env.to_wire()?;
        self.transport
            .send_bytes(wire)
            .await
            .context("relay send failed")?;
        Ok(())
    }

    /// Receive the next incoming [`Envelope`] from the relay.
    ///
    /// Also drives the heartbeat state machine: if a ping is due, it is sent
    /// transparently before the next application message is returned.
    pub async fn recv(&mut self) -> Result<Envelope> {
        loop {
            // Drive heartbeat before blocking on recv.
            match self.heartbeat.poll() {
                HeartbeatAction::SendPing => {
                    debug!("heartbeat: sending ping");
                    self.send_control("PING").await?;
                }
                HeartbeatAction::Reconnect => {
                    warn!("heartbeat: pong timeout — reconnecting");
                    self.reconnect().await?;
                }
                HeartbeatAction::WaitUntil(deadline) => {
                    // Convert std Instant to tokio Instant for sleep_until.
                    let now_std = std::time::Instant::now();
                    let now_tok = TokioInstant::now();
                    let delta = deadline
                        .checked_duration_since(now_std)
                        .unwrap_or_default();
                    let tokio_deadline = now_tok + delta;

                    // Race between deadline and incoming data.
                    tokio::select! {
                        _ = sleep_until(tokio_deadline) => {
                            // Deadline elapsed; loop again to poll heartbeat.
                            continue;
                        }
                        result = self.transport.recv_bytes() => {
                            let bytes = result?.ok_or_else(|| anyhow::anyhow!("relay closed connection"))?;
                            if let Some(env) = self.handle_incoming(bytes)? {
                                return Ok(env);
                            }
                            // Control message handled internally; loop for next.
                        }
                    }
                    continue;
                }
            }
        }
    }

    /// Returns `true` once the handshake has completed successfully.
    pub fn is_connected(&self) -> bool {
        self.handshake_complete && self.transport.is_connected()
    }

    // -- Private helpers -----------------------------------------------------

    async fn do_handshake(&mut self) -> Result<()> {
        // Send HELLO.
        self.send_control("HELLO").await?;

        // Wait for HELLO_ACK (with a generous timeout).
        let deadline = TokioInstant::now() + std::time::Duration::from_secs(10);
        loop {
            tokio::select! {
                _ = sleep_until(deadline) => {
                    anyhow::bail!("relay handshake timed out");
                }
                result = self.transport.recv_bytes() => {
                    let bytes = result?.ok_or_else(|| anyhow::anyhow!("relay closed during handshake"))?;
                    let env = Envelope::from_wire(&bytes)?;
                    if env.payload == b"HELLO_ACK" {
                        self.handshake_complete = true;
                        debug!("relay handshake complete");
                        return Ok(());
                    }
                    // Any other message before ack is unexpected but tolerated.
                }
            }
        }
    }

    async fn reconnect(&mut self) -> Result<()> {
        self.handshake_complete = false;
        self.heartbeat.on_reconnect_attempt();

        let transport = WsTransport::connect(&self.relay_url.clone())
            .await
            .context("reconnect failed")?;
        self.transport = transport;
        self.do_handshake().await?;
        self.heartbeat.on_reconnect_success();
        Ok(())
    }

    /// Send a control message to the relay itself.
    async fn send_control(&mut self, tag: &str) -> Result<()> {
        let relay_peer = PeerId::from(RELAY_PEER_ID.to_owned());
        let nonce = self.nonce.next();
        let env = Envelope::new(&self.identity, &relay_peer, tag.as_bytes().to_vec(), nonce);
        let wire = env.to_wire()?;
        self.transport.send_bytes(wire).await
    }

    /// Process a raw incoming frame; returns `Some(Envelope)` for application
    /// messages, `None` for control messages handled internally.
    fn handle_incoming(&mut self, bytes: Vec<u8>) -> Result<Option<Envelope>> {
        let env = Envelope::from_wire(&bytes)?;

        // Pong from relay — reset heartbeat.
        if env.payload == b"PONG" || env.payload == b"HELLO_ACK" {
            self.heartbeat.on_pong();
            return Ok(None);
        }

        Ok(Some(env))
    }
}

impl From<String> for PeerId {
    fn from(s: String) -> Self {
        crate::identity::PeerId::from_raw(s)
    }
}
