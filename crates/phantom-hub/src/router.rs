//! JSON-RPC frame router.
//!
//! [`forward`] is the main entry point. It looks up the target Phantom in the
//! [`crate::registry::ConnectionRegistry`], rewrites the request's `id` field
//! to a hub-local [`crate::registry::HubId`], enqueues the frame on the
//! Phantom's outbound channel, and awaits the matching response on a
//! [`tokio::sync::oneshot`] channel.
//!
//! # ID rewriting
//!
//! Claude and Phantom instances manage their own independent ID spaces.
//! Passing Claude's ID through to Phantom would create collisions when two
//! Claude sessions concurrently target the same Phantom. The hub therefore:
//!
//! 1. Saves the original `req.id`.
//! 2. Replaces `req.id` with a monotonic hub-local counter value.
//! 3. Registers a `oneshot::Sender` keyed by that counter value in the
//!    connection's pending table.
//! 4. On response, looks up by hub ID, restores the original ID, and
//!    completes the oneshot.
//!
//! # Idempotency
//!
//! Idempotency dedup (deduplicating concurrent retried calls by an optional
//! caller-supplied key) is deferred to issue #397. The `idempotency_key`
//! parameter on [`forward`] is accepted but ignored in Phase 1. A correct
//! implementation requires a shared broadcast channel per in-flight key and
//! bounded map cleanup; the broken Phase 1 stub was removed in PR #495 review.
//!
//! # Timeout
//!
//! Each call is subject to a configurable deadline (default 30 s, overrideable
//! via the `HUB_FORWARD_TIMEOUT_SECS` environment variable).  On expiry the
//! pending entry is removed and [`RouteError::Timeout`] is returned.
//!
//! # Backpressure
//!
//! The outbound mpsc has bounded capacity ([`crate::registry::OUTBOUND_CHANNEL_CAPACITY`]).
//! When the channel is full, [`RouteError::Backpressure`] is returned
//! immediately rather than blocking the caller.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use crate::registry::{HubId, PhantomId, SharedRegistry};

// ---------------------------------------------------------------------------
// JSON-RPC types (hub-local definitions)
// ---------------------------------------------------------------------------

/// A JSON-RPC 2.0 request frame as forwarded by the hub.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<serde_json::Value>,
    pub method: String,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub params: serde_json::Value,
}

/// A JSON-RPC 2.0 response frame returned by Phantom.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

/// A JSON-RPC 2.0 error object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl JsonRpcError {
    /// Standard "method not found" error (code -32601).
    #[must_use]
    pub fn method_not_found(method: &str) -> Self {
        Self {
            code: -32601,
            message: format!("Method not found: {method}"),
            data: None,
        }
    }

    /// Standard "not implemented yet" error for scaffold stubs.
    #[must_use]
    pub fn not_implemented(issue: &str) -> Self {
        Self {
            code: -32000,
            message: format!("Not implemented (see issue {issue})"),
            data: None,
        }
    }

    /// Routing failure error.
    #[must_use]
    pub fn routing_error(msg: &str) -> Self {
        Self {
            code: -32001,
            message: msg.to_owned(),
            data: None,
        }
    }
}

impl JsonRpcResponse {
    /// Build a JSON-RPC 2.0 error response for the given original ID.
    #[must_use]
    pub fn error_response(id: Option<serde_json::Value>, err: JsonRpcError) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(err),
        }
    }
}

// ---------------------------------------------------------------------------
// RouteError
// ---------------------------------------------------------------------------

/// Errors that can occur when forwarding a JSON-RPC request to a Phantom.
#[derive(Debug, thiserror::Error)]
pub enum RouteError {
    /// No Phantom with the given id is currently registered.
    #[error("phantom {0} is not connected")]
    NotFound(PhantomId),

    /// The Phantom's outbound channel is full.
    #[error("phantom {0} outbound channel is at capacity")]
    Backpressure(PhantomId),

    /// The request timed out waiting for a response.
    #[error("request to phantom {0} timed out")]
    Timeout(PhantomId),

    /// The WebSocket connection was closed before a response arrived.
    #[error("phantom {0} disconnected while request was in flight")]
    Disconnected(PhantomId),
}

// ---------------------------------------------------------------------------
// Idempotency tracking (deferred — see module doc)
// ---------------------------------------------------------------------------

/// Placeholder for the idempotency dedup map deferred to issue #397.
///
/// The `forward` function accepts this type so callers can be wired up today;
/// passing `None` to `idempotency_key` is always safe. A real implementation
/// will replace this with a bounded broadcast-channel map once #397 lands.
pub type SharedIdempotencyMap = ();

/// Create a no-op idempotency placeholder.
#[must_use]
#[allow(clippy::let_unit_value)]
pub fn new_idempotency_map() -> SharedIdempotencyMap {}

// ---------------------------------------------------------------------------
// forward
// ---------------------------------------------------------------------------

/// Read the forward timeout from the environment.
///
/// Defaults to 30 seconds; overrideable via `HUB_FORWARD_TIMEOUT_SECS`.
#[must_use]
fn forward_timeout() -> Duration {
    std::env::var("HUB_FORWARD_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(30))
}

/// Forward `req` to the Phantom identified by `phantom_id`.
///
/// The function:
/// 1. Acquires a write lock on the registry, looks up the connection, rewrites
///    the request id, enqueues the frame, and registers a oneshot reply channel
///    — all under a single write lock acquisition.
/// 2. Drops the write lock before `.await`-ing the reply.
/// 3. Races the reply oneshot against the timeout.
/// 4. On success, rewrites the response id back to the original Claude id.
///
/// `idempotency_key` is accepted for API compatibility but **ignored in Phase
/// 1**. Idempotency dedup is deferred to issue #397 — the broken stub that was
/// here (dead oneshot receiver + unbounded map growth) was removed during PR
/// #495 review.
///
/// # Errors
///
/// See [`RouteError`].
pub async fn forward(
    registry: &SharedRegistry,
    phantom_id: &PhantomId,
    req: JsonRpcRequest,
    idempotency_key: Option<&str>,
    idem_map: &SharedIdempotencyMap,
) -> Result<JsonRpcResponse, RouteError> {
    forward_with_timeout(registry, phantom_id, req, idempotency_key, idem_map, forward_timeout()).await
}

/// Forward `req` to the Phantom identified by `phantom_id` with an explicit `timeout`.
///
/// This is the underlying implementation used by [`forward`].  Callers that need
/// to inject a specific deadline (e.g. tests that want an instant timeout without
/// mutating environment variables) should call this function directly.
///
/// # Errors
///
/// See [`RouteError`].
pub async fn forward_with_timeout(
    registry: &SharedRegistry,
    phantom_id: &PhantomId,
    mut req: JsonRpcRequest,
    _idempotency_key: Option<&str>,
    _idem_map: &SharedIdempotencyMap,
    timeout: Duration,
) -> Result<JsonRpcResponse, RouteError> {
    // Idempotency dedup deferred to issue #397 — see module-level doc.

    // --- Save the original caller-supplied id ---
    let original_id = req.id.clone();

    // --- Acquire write lock, rewrite id, enqueue, register oneshot ---
    let (reply_rx, hub_id) = {
        let mut reg = registry.write().await;
        let state = reg
            .get_mut(phantom_id)
            .ok_or_else(|| RouteError::NotFound(phantom_id.clone()))?;

        let hub_id = state.alloc_hub_id();
        req.id = Some(serde_json::Value::Number(hub_id.0.into()));

        // Enqueue — non-blocking; returns Backpressure if the channel is full.
        state
            .tx
            .try_send(req)
            .map_err(|_| RouteError::Backpressure(phantom_id.clone()))?;

        let (reply_tx, reply_rx) = oneshot::channel::<JsonRpcResponse>();
        state.pending.insert(hub_id, reply_tx);

        (reply_rx, hub_id)
    };

    // --- Await the reply, racing against timeout ---
    let result = tokio::time::timeout(timeout, reply_rx).await;

    // Clean up the pending entry on timeout (disconnect cleanup happens via
    // ConnState drop in unregister).
    match result {
        Ok(Ok(mut response)) => {
            // Rewrite the hub id back to the original Claude id.
            response.id = original_id;
            Ok(response)
        }
        Ok(Err(_)) => {
            // Oneshot sender was dropped — the connection was removed.
            Err(RouteError::Disconnected(phantom_id.clone()))
        }
        Err(_elapsed) => {
            // Timeout: remove the pending entry so we don't accumulate orphans.
            let mut reg = registry.write().await;
            if let Some(state) = reg.get_mut(phantom_id) {
                state.pending.remove(&hub_id);
            }
            Err(RouteError::Timeout(phantom_id.clone()))
        }
    }
}

/// Deliver a response from a Phantom back to the waiting [`forward`] caller.
///
/// Called by the inbound WebSocket read task for each JSON-RPC response frame.
/// The function acquires a write lock on the registry, looks up the pending
/// oneshot sender by hub id, removes it, and sends the response.
///
/// Logs a warning when the hub id is not found (already timed out or
/// duplicate delivery) rather than returning an error, since the WebSocket
/// task should not crash on stale responses.
pub async fn deliver_response(registry: &SharedRegistry, phantom_id: &PhantomId, resp: JsonRpcResponse) {
    // Extract the hub id from the response's `id` field.
    let hub_id = match &resp.id {
        Some(serde_json::Value::Number(n)) => {
            if let Some(u) = n.as_u64() {
                HubId(u)
            } else {
                tracing::warn!(
                    "deliver_response: non-u64 response id from {}: {:?}",
                    phantom_id,
                    n
                );
                return;
            }
        }
        other => {
            tracing::warn!(
                "deliver_response: unexpected response id from {}: {:?}",
                phantom_id,
                other
            );
            return;
        }
    };

    let mut reg = registry.write().await;
    if let Some(state) = reg.get_mut(phantom_id) {
        state.last_seen = std::time::Instant::now();
        if let Some(tx) = state.pending.remove(&hub_id) {
            let _ = tx.send(resp);
        } else {
            tracing::warn!(
                "deliver_response: hub_id {} not in pending table for {}",
                hub_id.0,
                phantom_id
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::let_unit_value)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::registry::{new_shared_for_tests, OUTBOUND_CHANNEL_CAPACITY};

    fn phantom_id(s: &str) -> PhantomId {
        PhantomId::new(s)
    }

    async fn register_mock(
        registry: &SharedRegistry,
        id: &str,
    ) -> tokio::sync::mpsc::Receiver<JsonRpcRequest> {
        let (tx, rx) = tokio::sync::mpsc::channel(OUTBOUND_CHANNEL_CAPACITY);
        registry
            .write()
            .await
            .register(phantom_id(id), tx, "localhost".into(), "0.1.0".into())
            .unwrap();
        rx
    }

    fn make_request(method: &str) -> JsonRpcRequest {
        JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(serde_json::Value::Number(42.into())),
            method: method.to_owned(),
            params: serde_json::Value::Null,
        }
    }

    fn make_response(hub_id: u64) -> JsonRpcResponse {
        JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: Some(serde_json::Value::Number(hub_id.into())),
            result: Some(serde_json::json!({"ok": true})),
            error: None,
        }
    }

    // -------------------------------------------------------------------------
    // Frame routing: request arrives at mock Phantom, response comes back
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn forward_delivers_frame_and_receives_response() {
        let registry = new_shared_for_tests();
        let idem_map = new_idempotency_map();
        let mut phantom_rx = register_mock(&registry, "phantom-a").await;

        // Spawn a task that acts as the Phantom: reads the request and replies.
        let reg_clone = Arc::clone(&registry);
        tokio::spawn(async move {
            let req = phantom_rx.recv().await.expect("should receive request");
            // The request's id was rewritten to hub id 0.
            let hub_id = req.id.unwrap().as_u64().unwrap();
            let resp = make_response(hub_id);
            deliver_response(&reg_clone, &phantom_id("phantom-a"), resp).await;
        });

        let req = make_request("tools/call");
        let resp = forward(&registry, &phantom_id("phantom-a"), req, None, &idem_map)
            .await
            .expect("forward should succeed");

        // The response id should be restored to the original Claude id (42).
        assert_eq!(resp.id, Some(serde_json::Value::Number(42.into())));
        assert!(resp.error.is_none());
    }

    // -------------------------------------------------------------------------
    // Disconnect: connection removed mid-request → RouteError::Disconnected
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn forward_returns_disconnected_when_registry_entry_dropped() {
        let registry = new_shared_for_tests();
        let idem_map = new_idempotency_map();
        let _phantom_rx = register_mock(&registry, "phantom-b").await;

        // Spawn a task that unregisters the Phantom immediately after the
        // forward call enqueues the frame (giving it time to start waiting).
        let reg_clone = Arc::clone(&registry);
        tokio::spawn(async move {
            // Brief yield so forward() can acquire the lock first.
            tokio::task::yield_now().await;
            let state = reg_clone.write().await.unregister(&phantom_id("phantom-b"));
            // Dropping ConnState drops all pending oneshot senders.
            drop(state);
        });

        let req = make_request("tools/call");
        let result = forward(&registry, &phantom_id("phantom-b"), req, None, &idem_map).await;
        // Either Disconnected (oneshot dropped) or NotFound (unregistered before
        // forward could insert) — both are acceptable.
        assert!(
            matches!(
                result,
                Err(RouteError::Disconnected(_)) | Err(RouteError::NotFound(_))
            ),
            "unexpected result: {:?}",
            result
        );
    }

    // -------------------------------------------------------------------------
    // Not found: phantom_id not registered → RouteError::NotFound
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn forward_not_found_for_unregistered_phantom() {
        let registry = new_shared_for_tests();
        let idem_map = new_idempotency_map();

        let req = make_request("tools/call");
        let result = forward(&registry, &phantom_id("ghost"), req, None, &idem_map).await;
        assert!(matches!(result, Err(RouteError::NotFound(_))));
    }

    // -------------------------------------------------------------------------
    // Timeout: Phantom never replies → RouteError::Timeout
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn forward_times_out_when_phantom_does_not_reply() {
        let registry = new_shared_for_tests();
        let idem_map = new_idempotency_map();
        let _phantom_rx = register_mock(&registry, "phantom-slow").await;
        // Keep _phantom_rx alive so the channel is not closed (we want Timeout, not Disconnected).

        let req = make_request("tools/call");
        // Inject a zero-duration timeout directly — no env mutation required.
        let result = forward_with_timeout(
            &registry,
            &phantom_id("phantom-slow"),
            req,
            None,
            &idem_map,
            Duration::ZERO,
        )
        .await;

        assert!(
            matches!(result, Err(RouteError::Timeout(_))),
            "expected Timeout, got: {:?}",
            result
        );
    }

    // -------------------------------------------------------------------------
    // Backpressure: full channel → RouteError::Backpressure
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn forward_returns_backpressure_when_channel_full() {
        let registry = new_shared_for_tests();
        let idem_map = new_idempotency_map();
        // Capacity-1 channel so we can fill it easily.
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        registry
            .write()
            .await
            .register(phantom_id("slow"), tx, "h".into(), "v".into())
            .unwrap();

        // Fill the channel.
        let req1 = make_request("first");
        let req2 = make_request("second");

        let _ = forward(&registry, &phantom_id("slow"), req1, None, &idem_map).await;
        let result = forward(&registry, &phantom_id("slow"), req2, None, &idem_map).await;

        assert!(
            matches!(result, Err(RouteError::Backpressure(_))),
            "expected Backpressure, got: {:?}",
            result
        );
    }

    // -------------------------------------------------------------------------
    // Multi-peer: frame addressed to phantom-b is NOT delivered to phantom-a
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn forward_routes_to_correct_phantom() {
        let registry = new_shared_for_tests();
        let idem_map = new_idempotency_map();
        let mut rx_a = register_mock(&registry, "pa").await;
        let mut rx_b = register_mock(&registry, "pb").await;

        // phantom-b replies; phantom-a should not see the request.
        let reg_clone = Arc::clone(&registry);
        tokio::spawn(async move {
            let req = rx_b.recv().await.expect("pb should receive");
            let hub_id = req.id.unwrap().as_u64().unwrap();
            deliver_response(&reg_clone, &phantom_id("pb"), make_response(hub_id)).await;
        });

        let req = make_request("tools/list");
        let resp = forward(&registry, &phantom_id("pb"), req, None, &idem_map)
            .await
            .unwrap();
        assert!(resp.error.is_none());

        // phantom-a's channel should be empty.
        assert!(rx_a.try_recv().is_err());
    }

    // -------------------------------------------------------------------------
    // ID rewriting: hub id used on wire, original id restored in response
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn forward_rewrites_id_on_wire_and_restores_on_response() {
        let registry = new_shared_for_tests();
        let idem_map = new_idempotency_map();
        let mut rx = register_mock(&registry, "id-test").await;

        let reg_clone = Arc::clone(&registry);
        tokio::spawn(async move {
            let req = rx.recv().await.unwrap();
            // The wire id must be a hub-local random u64, not Claude's 99.
            let wire_id = req.id.clone().unwrap().as_u64().unwrap();
            assert_ne!(wire_id, 99, "hub must rewrite Claude's id on the wire");
            deliver_response(&reg_clone, &phantom_id("id-test"), make_response(wire_id)).await;
        });

        let mut req = make_request("ping");
        req.id = Some(serde_json::Value::Number(99.into()));
        let resp = forward(&registry, &phantom_id("id-test"), req, None, &idem_map)
            .await
            .unwrap();
        assert_eq!(resp.id, Some(serde_json::Value::Number(99.into())));
    }

    // -------------------------------------------------------------------------
    // Empty registration token rejected
    // -------------------------------------------------------------------------

    #[test]
    fn phantom_id_display() {
        let id = PhantomId::new("test-abc");
        assert_eq!(id.to_string(), "test-abc");
    }
}
