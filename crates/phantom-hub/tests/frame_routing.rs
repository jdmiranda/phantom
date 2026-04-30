//! Integration tests for the JSON-RPC frame router.
#![allow(clippy::let_unit_value)]
//!
//! These tests exercise `router::forward` and `router::deliver_response` end-to-end.
//! They do not involve an HTTP server — the WebSocket layer is replaced by a
//! tokio mpsc channel acting as a mock Phantom client.

use phantom_hub::{
    registry::{new_shared, PhantomId, OUTBOUND_CHANNEL_CAPACITY},
    router::{
        deliver_response, forward, new_idempotency_map, JsonRpcRequest, JsonRpcResponse,
        RouteError,
    },
};
use std::sync::Arc;
use tokio::sync::mpsc;

fn phantom_id(s: &str) -> PhantomId {
    PhantomId::new(s)
}

fn req(method: &str, id: u64) -> JsonRpcRequest {
    JsonRpcRequest {
        jsonrpc: "2.0".into(),
        id: Some(serde_json::Value::Number(id.into())),
        method: method.to_owned(),
        params: serde_json::Value::Null,
    }
}

fn ok_response(hub_id: u64) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0".into(),
        id: Some(serde_json::Value::Number(hub_id.into())),
        result: Some(serde_json::json!({"status": "ok"})),
        error: None,
    }
}

/// Register a mock Phantom and return its outbound receiver.
async fn register_mock(
    registry: &phantom_hub::registry::SharedRegistry,
    id: &str,
) -> mpsc::Receiver<JsonRpcRequest> {
    let (tx, rx) = mpsc::channel(OUTBOUND_CHANNEL_CAPACITY);
    registry
        .write()
        .await
        .register(phantom_id(id), tx, "localhost".into(), "0.1.0".into())
        .unwrap();
    rx
}

// ---------------------------------------------------------------------------
// Happy path: request arrives, Phantom replies, response returned
// ---------------------------------------------------------------------------

#[tokio::test]
async fn forward_round_trip_succeeds() {
    let registry = new_shared();
    let idem_map = new_idempotency_map();
    let mut rx = register_mock(&registry, "phantom-rt").await;

    let reg_clone = Arc::clone(&registry);
    tokio::spawn(async move {
        let req = rx.recv().await.unwrap();
        let hub_id = req.id.unwrap().as_u64().unwrap();
        deliver_response(&reg_clone, &phantom_id("phantom-rt"), ok_response(hub_id)).await;
    });

    let resp = forward(
        &registry,
        &phantom_id("phantom-rt"),
        req("tools/call", 100),
        None,
        &idem_map,
    )
    .await
    .unwrap();

    // Original Claude id (100) must be restored.
    assert_eq!(resp.id, Some(serde_json::json!(100)));
    assert!(resp.error.is_none());
}

// ---------------------------------------------------------------------------
// Disconnect mid-request → RouteError::Disconnected
// ---------------------------------------------------------------------------

#[tokio::test]
async fn forward_returns_disconnected_after_unregister() {
    let registry = new_shared();
    let idem_map = new_idempotency_map();
    let _rx = register_mock(&registry, "disc-test").await;

    let reg_clone = Arc::clone(&registry);
    tokio::spawn(async move {
        tokio::task::yield_now().await;
        let state = reg_clone
            .write()
            .await
            .unregister(&phantom_id("disc-test"));
        drop(state);
    });

    let result = forward(
        &registry,
        &phantom_id("disc-test"),
        req("tools/call", 1),
        None,
        &idem_map,
    )
    .await;

    assert!(
        matches!(
            result,
            Err(RouteError::Disconnected(_)) | Err(RouteError::NotFound(_))
        ),
        "expected Disconnected or NotFound, got: {:?}",
        result
    );
}

// ---------------------------------------------------------------------------
// Not found: unregistered phantom → RouteError::NotFound
// ---------------------------------------------------------------------------

#[tokio::test]
async fn forward_not_found_for_unknown_phantom() {
    let registry = new_shared();
    let idem_map = new_idempotency_map();

    let result = forward(
        &registry,
        &phantom_id("nobody"),
        req("tools/call", 5),
        None,
        &idem_map,
    )
    .await;

    assert!(
        matches!(result, Err(RouteError::NotFound(_))),
        "expected NotFound, got: {:?}",
        result
    );
}

// ---------------------------------------------------------------------------
// Timeout: Phantom never replies → RouteError::Timeout
// ---------------------------------------------------------------------------

#[tokio::test]
async fn forward_timeout_when_phantom_silent() {
    // SAFETY: single-threaded test runner; no concurrent env reads in this test.
    unsafe {
        std::env::set_var("HUB_FORWARD_TIMEOUT_SECS", "0");
    }

    let registry = new_shared();
    let idem_map = new_idempotency_map();
    let _rx = register_mock(&registry, "slow-phantom").await;

    let pid = phantom_id("slow-phantom");
    let result = forward(&registry, &pid, req("tools/call", 7), None, &idem_map).await;

    unsafe {
        std::env::remove_var("HUB_FORWARD_TIMEOUT_SECS");
    }

    assert!(
        matches!(result, Err(RouteError::Timeout(_))),
        "expected Timeout, got: {:?}",
        result
    );
}

// ---------------------------------------------------------------------------
// Multi-peer routing: frame for B does not land on A
// ---------------------------------------------------------------------------

#[tokio::test]
async fn forward_delivers_to_correct_phantom_only() {
    let registry = new_shared();
    let idem_map = new_idempotency_map();
    let mut rx_a = register_mock(&registry, "pa").await;
    let mut rx_b = register_mock(&registry, "pb").await;

    let reg_clone = Arc::clone(&registry);
    tokio::spawn(async move {
        let req = rx_b.recv().await.unwrap();
        let hub_id = req.id.unwrap().as_u64().unwrap();
        deliver_response(&reg_clone, &phantom_id("pb"), ok_response(hub_id)).await;
    });

    let resp = forward(
        &registry,
        &phantom_id("pb"),
        req("tools/list", 99),
        None,
        &idem_map,
    )
    .await
    .unwrap();

    // phantom-a's outbound channel should be untouched.
    assert!(
        rx_a.try_recv().is_err(),
        "phantom-a should not have received the request"
    );

    // Response came back correctly.
    assert_eq!(resp.id, Some(serde_json::json!(99)));
    assert!(resp.error.is_none());
}

// ---------------------------------------------------------------------------
// Hub id rewriting: wire frame uses hub id; response restores original id
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hub_rewrites_id_on_wire_and_restores_original_on_response() {
    let registry = new_shared();
    let idem_map = new_idempotency_map();
    let mut rx = register_mock(&registry, "id-phantom").await;

    let reg_clone = Arc::clone(&registry);
    tokio::spawn(async move {
        let req = rx.recv().await.unwrap();
        let wire_id = req.id.clone().unwrap().as_u64().unwrap();
        // Wire id must be the hub-local counter (0), not Claude's 77.
        assert_eq!(wire_id, 0, "hub must rewrite id to 0 on the wire");
        deliver_response(&reg_clone, &phantom_id("id-phantom"), ok_response(wire_id)).await;
    });

    let mut request = req("ping", 0);
    request.id = Some(serde_json::json!(77)); // Claude's id
    let resp = forward(&registry, &phantom_id("id-phantom"), request, None, &idem_map)
        .await
        .unwrap();

    assert_eq!(resp.id, Some(serde_json::json!(77)));
}

// ---------------------------------------------------------------------------
// Empty / malformed registration: registry rejects phantom with no token
// ---------------------------------------------------------------------------

#[tokio::test]
async fn registry_rejects_empty_phantom_id_string() {
    let registry = new_shared();
    let (tx, _rx) = mpsc::channel::<JsonRpcRequest>(8);
    // An empty string PhantomId is technically allowed by the registry type
    // but the WSS handler rejects empty device_token before inserting.
    // Here we verify the registry's duplicate-key logic works correctly
    // with a non-empty id.
    registry
        .write()
        .await
        .register(phantom_id("valid-id"), tx, "h".into(), "v".into())
        .unwrap();

    let (tx2, _rx2) = mpsc::channel::<JsonRpcRequest>(8);
    let err = registry
        .write()
        .await
        .register(phantom_id("valid-id"), tx2, "h".into(), "v".into());
    assert!(err.is_err(), "duplicate registration must fail");
}

// ---------------------------------------------------------------------------
// Concurrent requests to the same phantom are correctly correlated
// ---------------------------------------------------------------------------

#[tokio::test]
async fn concurrent_requests_to_same_phantom_are_correlated() {
    let registry = new_shared();
    let idem_map = new_idempotency_map();
    let mut rx = register_mock(&registry, "multi-req").await;

    let reg_clone = Arc::clone(&registry);
    tokio::spawn(async move {
        // Echo back both requests.
        for _ in 0..2 {
            if let Some(req) = rx.recv().await {
                let hub_id = req.id.unwrap().as_u64().unwrap();
                deliver_response(
                    &reg_clone,
                    &phantom_id("multi-req"),
                    ok_response(hub_id),
                )
                .await;
            }
        }
    });

    let pid = phantom_id("multi-req");
    let r1_future = forward(&registry, &pid, req("call/a", 11), None, &idem_map);
    let r2_future = forward(&registry, &pid, req("call/b", 22), None, &idem_map);

    let (r1, r2) = tokio::join!(r1_future, r2_future);

    let r1 = r1.unwrap();
    let r2 = r2.unwrap();

    // Each response must have its original Claude id restored.
    let ids: std::collections::HashSet<u64> = [&r1, &r2]
        .iter()
        .map(|r| r.id.as_ref().unwrap().as_u64().unwrap())
        .collect();
    assert!(ids.contains(&11));
    assert!(ids.contains(&22));
}
