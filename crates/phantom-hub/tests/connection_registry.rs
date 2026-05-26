//! Integration tests for the connection registry.
//!
//! These tests drive `ConnectionRegistry` directly (no HTTP stack) and verify
//! the register → lookup → unregister lifecycle, including the idempotency and
//! stale-entry filtering behaviour.

use phantom_hub::{
    registry::{new_shared_for_tests, PhantomId, OUTBOUND_CHANNEL_CAPACITY},
    router::JsonRpcRequest,
};
use tokio::sync::mpsc;

fn make_id(s: &str) -> PhantomId {
    PhantomId::new(s)
}

fn make_tx() -> mpsc::Sender<JsonRpcRequest> {
    mpsc::channel(OUTBOUND_CHANNEL_CAPACITY).0
}

// ---------------------------------------------------------------------------
// Register → len → unregister
// ---------------------------------------------------------------------------

#[tokio::test]
async fn register_single_phantom_appears_in_list_online() {
    let reg = new_shared_for_tests();
    reg.write()
        .await
        .register(make_id("phantom-1"), make_tx(), "host".into(), "0.1.0".into())
        .unwrap();

    let online = reg.read().await.list_online();
    assert_eq!(online.len(), 1);
    assert_eq!(online[0].id, make_id("phantom-1"));
}

#[tokio::test]
async fn unregister_removes_phantom_from_list_online() {
    let reg = new_shared_for_tests();
    reg.write()
        .await
        .register(make_id("p1"), make_tx(), "h".into(), "v".into())
        .unwrap();

    reg.write().await.unregister(&make_id("p1"));

    let online = reg.read().await.list_online();
    assert_eq!(online.len(), 0);
}

#[tokio::test]
async fn duplicate_register_returns_error() {
    let reg = new_shared_for_tests();
    reg.write()
        .await
        .register(make_id("dup"), make_tx(), "h".into(), "v".into())
        .unwrap();

    let err = reg
        .write()
        .await
        .register(make_id("dup"), make_tx(), "h".into(), "v".into());
    assert!(err.is_err(), "expected error on duplicate registration");
}

#[tokio::test]
async fn unregister_unknown_id_returns_none() {
    let reg = new_shared_for_tests();
    let result = reg.write().await.unregister(&make_id("ghost"));
    assert!(result.is_none());
}

#[tokio::test]
async fn multiple_phantoms_all_appear_in_list_online() {
    let reg = new_shared_for_tests();
    {
        let mut w = reg.write().await;
        for i in 0..5 {
            w.register(
                make_id(&format!("phantom-{i}")),
                make_tx(),
                format!("host-{i}"),
                "1.0.0".into(),
            )
            .unwrap();
        }
    }

    let online = reg.read().await.list_online();
    assert_eq!(online.len(), 5);
}

#[tokio::test]
async fn unregister_drops_pending_oneshots_signalling_disconnected() {
    use phantom_hub::registry::HubId;
    use tokio::sync::oneshot;

    let reg = new_shared_for_tests();
    let (tx, _rx) = mpsc::channel(OUTBOUND_CHANNEL_CAPACITY);
    reg.write()
        .await
        .register(make_id("disc"), tx, "h".into(), "v".into())
        .unwrap();

    // Insert a pending oneshot via the test accessor (issue #500: pending is
    // pub(crate) — external crates must not write to it directly).
    let (reply_tx, reply_rx) = oneshot::channel::<phantom_hub::router::JsonRpcResponse>();
    {
        let mut w = reg.write().await;
        let state = w.get_mut(&make_id("disc")).unwrap();
        state.insert_pending_for_test(HubId(0), reply_tx);
    }

    // Unregister — this should drop the pending sender.
    let conn_state = reg.write().await.unregister(&make_id("disc"));
    // Explicitly drop to trigger oneshot cancellation.
    drop(conn_state);

    // The oneshot receiver should see the sender was dropped.
    let result = reply_rx.await;
    assert!(result.is_err(), "oneshot should be cancelled on disconnect");
}

// ---------------------------------------------------------------------------
// Registry is empty check
// ---------------------------------------------------------------------------

#[tokio::test]
async fn empty_registry_reports_is_empty() {
    let reg = new_shared_for_tests();
    assert!(reg.read().await.is_empty());
    reg.write()
        .await
        .register(make_id("x"), make_tx(), "h".into(), "v".into())
        .unwrap();
    assert!(!reg.read().await.is_empty());
}
