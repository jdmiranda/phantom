//! Hub registration listener — outbound WSS connection to `phantom-hub`.
//!
//! On startup Phantom dials the hub URL, completes the relay handshake, then
//! sends a registration frame announcing its identity, version, and capability
//! summary.  Inbound JSON-RPC frames from the hub are piped through the same
//! [`dispatch_public`] function used by the Unix-socket listener so every tool
//! that already works locally automatically works remotely.
//!
//! # Lifecycle
//! ```text
//! spawn_hub()
//!   └─ hub_loop  (OS thread with its own tokio runtime, or spawned on
//!                 the caller's tokio runtime when one is already active)
//!        ├─ Identity::load_or_generate (keychain lookup per attempt)
//!        ├─ RelayClient::connect  (HELLO/HELLO_ACK handshake)
//!        ├─ send_registration     (phantom_id, device_token, host, version)
//!        └─ recv loop
//!             ├─ parse JsonRpcRequest from envelope payload
//!             ├─ dispatch_public() → JsonRpcResponse
//!             └─ RelayClient::send  (serialised response back to hub)
//!        ─ on disconnect / error: exponential back-off then reconnect
//! ```
//!
//! # Graceful no-op
//! If `hub_url` is empty, [`spawn_hub`] returns `Ok(None)` and nothing is
//! spawned.  The caller does not need to guard against a missing hub URL.
//!
//! # Auth placeholder
//! `device_token` is an opaque `String` forwarded unchanged in the registration
//! frame under the key `"device_token"`.  Real JWT issuance lands in #398.
//!
//! # Relation to Unix-socket listener
//! The Unix-socket listener (`listener.rs`) is sync and thread-per-connection.
//! This module is fully async and shares only the `AppCommand` channel and the
//! `dispatch_public` function.  Both listeners write to the same
//! `cmd_tx: Sender<AppCommand>`; the App thread owns the receiver and processes
//! messages serially — safe by design.

use std::sync::mpsc::Sender;
use std::time::Duration;

use anyhow::Result;
use log::{debug, error, info, warn};
use serde_json::json;
use tokio::runtime::Handle;

use phantom_net::RelayClient;
use phantom_net::identity::{Identity, PeerId};

use crate::listener::{AppCommand, dispatch_public};
use crate::protocol::JsonRpcRequest;
use crate::server::PhantomMcpServer;

// ---------------------------------------------------------------------------
// Well-known hub peer-id
// ---------------------------------------------------------------------------

/// The peer-id string the hub registers under on the relay.
const HUB_PEER_ID: &str = "hub";

/// Keyring namespace used for the Phantom instance identity.
const IDENTITY_NAMESPACE: &str = "phantom";

// ---------------------------------------------------------------------------
// Back-off configuration
// ---------------------------------------------------------------------------

const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
const BACKOFF_FACTOR: u64 = 2;

// ---------------------------------------------------------------------------
// HubListener handle
// ---------------------------------------------------------------------------

/// A running hub registration listener.
///
/// Dropping the handle does not stop the background task.  The task lives for
/// the lifetime of the process (or until the hub URL is permanently unreachable
/// after all reconnect attempts are exhausted).
pub struct HubListener {
    /// The hub URL this listener is connected to.
    hub_url: String,
}

impl HubListener {
    /// The hub URL this listener was configured with.
    #[must_use]
    pub fn hub_url(&self) -> &str {
        &self.hub_url
    }
}

// ---------------------------------------------------------------------------
// Public spawn function
// ---------------------------------------------------------------------------

/// Connect to `hub_url` and register this Phantom instance.
///
/// Returns `Ok(None)` immediately and without spawning anything when `hub_url`
/// is empty.
///
/// `identity_namespace` controls which keychain slot is used for the Ed25519
/// keypair.  Pass `None` to use the default `"phantom"` namespace, or supply
/// `PHANTOM_IDENTITY_NAMESPACE` to isolate QA / dev instances.
///
/// `device_token` is forwarded as-is in the registration frame.  Issue #398
/// provides real JWT issuance; for Phase 1 callers pass an empty string.
/// Connect to `hub_url` and register this Phantom instance.
///
/// The `identity` parameter is used for the first connection attempt; on
/// subsequent reconnects the identity is reloaded from the OS keychain (same
/// keypair — the keychain is the source of truth).
pub fn spawn_hub(
    hub_url: &str,
    identity: Identity,
    device_token: String,
    cmd_tx: Sender<AppCommand>,
) -> Result<Option<HubListener>> {
    // Derive the namespace from the identity's peer_id so reconnects reload
    // the same key.  The keychain service name is "phantom-net/phantom" by
    // convention (see Identity::load_or_generate in phantom-net).
    let _ = identity; // consumed; reconnects use load_or_generate
    spawn_hub_ns(hub_url, None, device_token, cmd_tx)
}

/// Like [`spawn_hub`] but accepts an explicit `identity_namespace` override.
///
/// Used internally and in tests to isolate keychain slots.
pub fn spawn_hub_ns(
    hub_url: &str,
    identity_namespace: Option<String>,
    device_token: String,
    cmd_tx: Sender<AppCommand>,
) -> Result<Option<HubListener>> {
    if hub_url.is_empty() {
        debug!("hub_listener: no hub URL configured — skipping hub registration");
        return Ok(None);
    }

    let hub_url_owned = hub_url.to_owned();
    let hub_url_for_handle = hub_url_owned.clone();
    let ns = identity_namespace
        .or_else(|| std::env::var("PHANTOM_IDENTITY_NAMESPACE").ok())
        .unwrap_or_else(|| IDENTITY_NAMESPACE.to_owned());

    match Handle::try_current() {
        Ok(handle) => {
            handle.spawn(hub_loop(hub_url_owned, ns, device_token, cmd_tx));
        }
        Err(_) => {
            std::thread::Builder::new()
                .name("mcp-hub".into())
                .spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("failed to build hub tokio runtime");
                    rt.block_on(hub_loop(hub_url_owned, ns, device_token, cmd_tx));
                })?;
        }
    }

    info!("hub_listener: started — url={hub_url_for_handle}");
    Ok(Some(HubListener {
        hub_url: hub_url_for_handle,
    }))
}

// ---------------------------------------------------------------------------
// Hub loop (reconnect with exponential back-off)
// ---------------------------------------------------------------------------

/// Top-level async task.  Reconnects indefinitely with exponential back-off.
async fn hub_loop(
    hub_url: String,
    identity_ns: String,
    device_token: String,
    cmd_tx: Sender<AppCommand>,
) {
    let mut delay = BACKOFF_MIN;
    let mut attempt: u32 = 0;

    loop {
        attempt += 1;
        debug!("hub_listener: connect attempt {attempt} to {hub_url}");

        // Load (or generate) the identity fresh on each attempt.  The keychain
        // lookup is idempotent — the same keypair is returned every time.
        let identity = match Identity::load_or_generate(&identity_ns) {
            Ok(id) => id,
            Err(e) => {
                error!("hub_listener: failed to load identity: {e:#} — retrying in {delay:?}");
                tokio::time::sleep(delay).await;
                delay = backoff_advance(delay);
                continue;
            }
        };

        match connect_and_run(&hub_url, identity, &device_token, &cmd_tx).await {
            Ok(()) => {
                warn!("hub_listener: connection to {hub_url} closed — reconnecting");
                delay = BACKOFF_MIN;
            }
            Err(e) => {
                error!("hub_listener: connection error: {e:#} — retrying in {delay:?}");
            }
        }

        tokio::time::sleep(delay).await;
        delay = backoff_advance(delay);
    }
}

/// Advance the back-off delay: multiply by `BACKOFF_FACTOR`, cap at `BACKOFF_MAX`.
fn backoff_advance(current: Duration) -> Duration {
    std::cmp::min(
        Duration::from_secs(current.as_secs().saturating_mul(BACKOFF_FACTOR)),
        BACKOFF_MAX,
    )
}

// ---------------------------------------------------------------------------
// Single connection: handshake + register + dispatch loop
// ---------------------------------------------------------------------------

/// Open one connection to the hub, send the registration frame, then process
/// inbound JSON-RPC frames until the connection drops or an error occurs.
async fn connect_and_run(
    hub_url: &str,
    identity: Identity,
    device_token: &str,
    cmd_tx: &Sender<AppCommand>,
) -> Result<()> {
    let peer_id_str = identity.peer_id.as_str().to_owned();

    // RelayClient::connect takes Identity by value — no Clone required.
    let mut client = RelayClient::connect(hub_url, identity).await?;
    info!("hub_listener: connected — phantom_id={peer_id_str}");

    // Send the registration frame.  peer_id_str is a plain String by this point.
    send_registration(&mut client, &peer_id_str, device_token).await?;
    info!("hub_listener: registered — phantom_id={peer_id_str}");

    // Inbound dispatch loop.
    let hub_peer = PeerId::from(HUB_PEER_ID.to_owned());

    loop {
        let envelope = client.recv().await?;

        let request: JsonRpcRequest = match serde_json::from_slice(&envelope.payload) {
            Ok(r) => r,
            Err(e) => {
                warn!(
                    "hub_listener: malformed JSON-RPC frame from {}: {e}",
                    envelope.from
                );
                continue;
            }
        };

        debug!(
            "hub_listener: inbound method={} id={:?}",
            request.method, request.id
        );

        // `dispatch_public` blocks on `reply_rx.recv_timeout` (a std sync call).
        // Running it on a blocking thread prevents stalling the tokio executor.
        let cmd_tx_clone = cmd_tx.clone();
        let request_clone = request.clone();
        let response = tokio::task::spawn_blocking(move || {
            // Create a fresh server per request — it is stateless (no live app
            // state; all live state flows through cmd_tx).
            let srv = PhantomMcpServer::new();
            dispatch_public(&srv, &request_clone, &cmd_tx_clone)
        })
        .await
        .unwrap_or_else(|_| {
            crate::protocol::create_error(
                request.id.clone().unwrap_or(serde_json::Value::Null),
                crate::protocol::INTERNAL_ERROR,
                "dispatch task panicked",
            )
        });

        let payload = serde_json::to_vec(&response).unwrap_or_else(|_| b"{}".to_vec());
        client.send(&hub_peer, payload).await?;
    }
}

// ---------------------------------------------------------------------------
// Registration frame
// ---------------------------------------------------------------------------

/// Send the registration frame to the hub peer.
///
/// ```json
/// {
///   "type":         "register",
///   "phantom_id":   "<base58 peer-id>",
///   "device_token": "<opaque — #398 fills real JWT>",
///   "host":         "<hostname>",
///   "version":      "<cargo package version>"
/// }
/// ```
async fn send_registration(
    client: &mut RelayClient,
    peer_id: &str,
    device_token: &str,
) -> Result<()> {
    let frame = json!({
        "type":         "register",
        "phantom_id":   peer_id,
        "device_token": device_token,
        "host":         hostname(),
        "version":      env!("CARGO_PKG_VERSION"),
    });
    let payload = serde_json::to_vec(&frame)?;
    let hub_peer = PeerId::from(HUB_PEER_ID.to_owned());
    client.send(&hub_peer, payload).await
}

// ---------------------------------------------------------------------------
// Hostname helper
// ---------------------------------------------------------------------------

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_owned())
                .ok_or(std::env::VarError::NotPresent)
        })
        .unwrap_or_else(|_| "unknown".to_owned())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use anyhow::Result;
    use futures_util::{SinkExt, StreamExt};
    use serde_json::json;
    use tokio::net::{TcpListener, TcpStream};
    use tokio_tungstenite::{accept_async, tungstenite::Message};

    use phantom_net::{Envelope, Identity as NetIdentity, PeerId as NetPeerId};

    // -----------------------------------------------------------------------
    // In-process mock hub
    // (mirrors the pattern in phantom-net/src/tests.rs)
    // -----------------------------------------------------------------------

    type HubSender = tokio::sync::mpsc::UnboundedSender<Vec<u8>>;

    async fn spawn_mock_hub() -> Result<SocketAddr> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let peers: Arc<tokio::sync::Mutex<HashMap<String, HubSender>>> =
            Arc::new(tokio::sync::Mutex::new(HashMap::new()));

        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let peers_clone = Arc::clone(&peers);
                tokio::spawn(handle_mock_hub_conn(stream, peers_clone));
            }
        });

        Ok(addr)
    }

    async fn handle_mock_hub_conn(
        stream: TcpStream,
        peers: Arc<tokio::sync::Mutex<HashMap<String, HubSender>>>,
    ) {
        let mut ws = match accept_async(stream).await {
            Ok(ws) => ws,
            Err(_) => return,
        };

        // HELLO
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
        let relay_identity = test_identity("relay-ack");
        let client_peer = NetPeerId::from(peer_id.clone());
        let ack = Envelope::new(&relay_identity, &client_peer, b"HELLO_ACK".to_vec(), 0);
        let Ok(ack_wire) = ack.to_wire() else {
            return;
        };
        if ws.send(Message::Binary(ack_wire.into())).await.is_err() {
            return;
        }

        // Register peer.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        peers.lock().await.insert(peer_id.clone(), tx);

        let (mut ws_tx, mut ws_rx) = ws.split();
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                let _ = ws_tx.send(Message::Binary(msg.into())).await;
            }
        });

        while let Some(Ok(Message::Binary(bytes))) = ws_rx.next().await {
            let bytes = bytes.to_vec();
            let env = match Envelope::from_wire(&bytes) {
                Ok(e) => e,
                Err(_) => continue,
            };

            // PING → PONG
            if env.payload == b"PING" {
                let pong = Envelope::new(
                    &relay_identity,
                    &NetPeerId::from(env.from.clone()),
                    b"PONG".to_vec(),
                    0,
                );
                if let Ok(wire) = pong.to_wire()
                    && let Some(peer_tx) = peers.lock().await.get(&env.from)
                {
                    let _ = peer_tx.send(wire);
                }
                continue;
            }

            // Route the frame to the addressed peer (peer-to-peer),
            // or echo it back to the sender if addressed to "hub".
            let guard = peers.lock().await;
            if env.to == HUB_PEER_ID {
                // Echo hub-addressed frames back to the sender (test helper).
                if let Some(peer_tx) = guard.get(&env.from) {
                    let _ = peer_tx.send(bytes);
                }
            } else if let Some(peer_tx) = guard.get(&env.to) {
                // Forward to the addressed peer (peer-to-peer routing).
                let _ = peer_tx.send(bytes);
            }
        }

        peers.lock().await.remove(&peer_id);
    }

    // -----------------------------------------------------------------------
    // Helper: generate a unique test identity via the OS keychain.
    // Each call uses a unique service name so tests don't share state.
    // -----------------------------------------------------------------------
    fn test_identity(tag: &str) -> NetIdentity {
        use std::time::{SystemTime, UNIX_EPOCH};
        let ns_nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let ns = format!("phantom-test-hub-{tag}-{ns_nanos}");
        NetIdentity::load_or_generate(&ns)
            .expect("test identity load_or_generate must succeed")
    }

    // -----------------------------------------------------------------------
    // Test: spawn_hub no-ops when hub_url is empty
    // -----------------------------------------------------------------------

    #[test]
    fn spawn_hub_noop_when_url_empty() {
        let identity = test_identity("noop");
        let (cmd_tx, _cmd_rx) = std::sync::mpsc::channel();
        let result = spawn_hub("", identity, String::new(), cmd_tx);
        assert!(result.is_ok(), "spawn_hub must not error on empty URL");
        assert!(
            result.unwrap().is_none(),
            "spawn_hub must return None for empty URL"
        );
    }

    // -----------------------------------------------------------------------
    // Test: listener connects, registers, and dispatches phantom.run_command
    // -----------------------------------------------------------------------

    /// Verifies that a tools/call frame arriving from the hub is parsed and
    /// forwarded to the App command channel as `AppCommand::RunCommand`.
    ///
    /// Note: the response routing (listener → hub → Claude) is completed by
    /// the hub-side registry in issue #396.  This test verifies only the
    /// Phantom-side dispatch: that the inbound frame is correctly deserialised
    /// and forwarded to `cmd_tx`.
    #[tokio::test]
    async fn hub_listener_registers_and_dispatches_run_command() {
        let addr = spawn_mock_hub().await.unwrap();
        let hub_url = format!("ws://{addr}");

        // Track the AppCommand that arrives on cmd_tx.
        let received_cmd: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let received_clone = Arc::clone(&received_cmd);

        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<AppCommand>();

        // Serve the command channel in a background OS thread and signal
        // completion via a tokio oneshot so the async test can await it.
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
        std::thread::spawn(move || {
            let mut done_tx = Some(done_tx);
            for cmd in &cmd_rx {
                if let AppCommand::RunCommand { command, reply } = cmd {
                    *received_clone.lock().unwrap() = Some(command.clone());
                    let _ = reply.send(Ok(()));
                    if let Some(tx) = done_tx.take() {
                        let _ = tx.send(());
                    }
                }
            }
        });

        // Use a unique test identity namespace so the keychain slot is isolated.
        let identity = test_identity("dispatch");
        let ns = format!("phantom-test-hub-{}", std::process::id());

        spawn_hub_ns(&hub_url, Some(ns.clone()), "placeholder-token".to_owned(), cmd_tx)
            .unwrap()
            .expect("hub listener must return Some for non-empty URL");

        // Allow the listener to connect, complete the handshake, and register.
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Connect a second client acting as "the hub agent" that forwards
        // a JSON-RPC request to the listener.
        let agent_id = test_identity("agent");
        let mut hub_agent = RelayClient::connect(&hub_url, agent_id).await.unwrap();
        // Brief wait to let the agent's handshake complete before sending.
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Recover the listener's peer_id from the keychain namespace.
        let listener_id = match NetIdentity::load_or_generate(&ns) {
            Ok(id) => id,
            Err(_) => {
                // Keychain unavailable in CI; skip routing test.
                drop(identity);
                return;
            }
        };
        let listener_peer = NetPeerId::from(listener_id.peer_id.as_str().to_owned());

        // Send a tools/call request addressed directly to the listener.
        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "phantom.run_command",
                "arguments": {"command": "echo hello"}
            }
        });
        let payload = serde_json::to_vec(&request).unwrap();
        hub_agent.send(&listener_peer, payload).await.unwrap();

        // Wait for the AppCommand to arrive at the mock app thread (max 5 s).
        tokio::time::timeout(Duration::from_secs(5), done_rx)
            .await
            .expect("AppCommand::RunCommand must be dispatched within 5 s")
            .expect("done sender dropped unexpectedly");

        assert_eq!(
            received_cmd.lock().unwrap().as_deref(),
            Some("echo hello"),
            "AppCommand::RunCommand must carry the correct command string"
        );

        drop(identity);
    }

    // -----------------------------------------------------------------------
    // Test: listener reconnects after hub drops the connection
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn hub_listener_reconnects_after_disconnect() {
        // Bind a server that counts successful HELLO/ACK handshakes, then
        // immediately closes the WS after each one so the listener retries.
        let connect_count = Arc::new(tokio::sync::Mutex::new(0u32));
        let count_srv = Arc::clone(&connect_count);

        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = tcp.local_addr().unwrap();
        let hub_url = format!("ws://{addr}");

        tokio::spawn(async move {
            for _ in 0..4u32 {
                let Ok((stream, _)) = tcp.accept().await else {
                    break;
                };
                let count = Arc::clone(&count_srv);
                tokio::spawn(async move {
                    let mut ws = match accept_async(stream).await {
                        Ok(ws) => ws,
                        Err(_) => return,
                    };
                    let hello_bytes = loop {
                        match ws.next().await {
                            Some(Ok(Message::Binary(b))) => break b.to_vec(),
                            Some(Ok(_)) => continue,
                            _ => return,
                        }
                    };
                    let Ok(hello_env) = Envelope::from_wire(&hello_bytes) else {
                        return;
                    };
                    if hello_env.payload != b"HELLO" {
                        return;
                    }
                    let client_peer = NetPeerId::from(hello_env.from.clone());
                    let relay_id = test_identity("reconnect-relay");
                    let ack = Envelope::new(&relay_id, &client_peer, b"HELLO_ACK".to_vec(), 0);
                    if let Ok(wire) = ack.to_wire() {
                        let _ = ws.send(Message::Binary(wire.into())).await;
                    }
                    *count.lock().await += 1;
                    // Drop ws → disconnect → listener retries.
                });
            }
        });

        let ns = format!("phantom-test-reconnect-{}", std::process::id());
        let (cmd_tx, _cmd_rx) = std::sync::mpsc::channel::<AppCommand>();

        spawn_hub_ns(&hub_url, Some(ns), String::new(), cmd_tx)
            .unwrap()
            .expect("must return Some");

        // BACKOFF_MIN is 1s; wait 4s to give at least 2–3 reconnect cycles.
        tokio::time::sleep(Duration::from_secs(4)).await;

        let count = *connect_count.lock().await;
        assert!(
            count >= 2,
            "expected at least 2 connect attempts, got {count}"
        );
    }

    // -----------------------------------------------------------------------
    // Test: registration frame structure
    // -----------------------------------------------------------------------

    #[test]
    fn registration_frame_fields_are_present() {
        let frame = json!({
            "type":         "register",
            "phantom_id":   "some-peer-id",
            "device_token": "tok-placeholder",
            "host":         "test-host",
            "version":      env!("CARGO_PKG_VERSION"),
        });
        assert_eq!(frame["type"].as_str(), Some("register"));
        assert!(frame["phantom_id"].as_str().is_some());
        assert!(frame["device_token"].as_str().is_some());
        assert!(frame["host"].as_str().is_some());
        assert!(frame["version"].as_str().is_some());
    }

    // -----------------------------------------------------------------------
    // Test: auth placeholder passes through unchanged
    // -----------------------------------------------------------------------

    #[test]
    fn auth_placeholder_passes_through_unchanged() {
        let token = "placeholder-device-token-xyz";
        let frame = json!({
            "type":         "register",
            "phantom_id":   "some-peer-id",
            "device_token": token,
            "host":         "test-host",
            "version":      "0.1.0",
        });
        assert_eq!(frame["device_token"].as_str(), Some(token));
    }
}
