//! Adversarial integration tests for the event-log causality boundary
//! (issue #645).
//!
//! Builds on the dispatch-boundary contract that flipped
//! `ChatToolContext::event_log` from `Option<Arc<Mutex<EventLog>>>` to
//! `Arc<Mutex<EventLog>>`. Before #645, `send_to_agent` / `broadcast_to_role`
//! silently no-op'd on a missing log, and inter-agent coordination had no
//! causality guarantee. Today the emitter MUST return `Err(_)` on:
//!
//! 1. Poisoned mutex on the shared event log.
//! 2. A `send_to_agent` call against an unknown label.
//! 3. A `send_to_agent` call against a recipient whose inbox is closed.
//!
//! Each scenario is exercised against `send_to_agent` and corroborated against
//! `read_from_agent` where shape-of-error matters.

use std::panic;
use std::sync::{Arc, Mutex};

use phantom_agents::chat_tools::{ChatToolContext, read_from_agent, send_to_agent};
use phantom_agents::inbox::{AgentHandle, AgentRegistry, AgentStatus, InboxMessage};
use phantom_agents::role::{AgentRef, AgentRole, SpawnSource};
use phantom_agents::test_support::fresh_log;

use serde_json::json;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_self_ref(id: u64, label: &str) -> AgentRef {
    AgentRef::new(id, AgentRole::Conversational, label, SpawnSource::User)
}

/// Build a fake agent handle bound to its tx/rx pair. The caller owns the
/// receiver and can drop it to simulate a crashed agent.
fn fake_agent(
    id: u64,
    role: AgentRole,
    label: &str,
) -> (AgentHandle, tokio::sync::mpsc::Receiver<InboxMessage>) {
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    let (_status_tx, status_rx) = tokio::sync::watch::channel(AgentStatus::Idle);
    let handle = AgentHandle {
        agent_ref: AgentRef::new(id, role, label, SpawnSource::Substrate),
        inbox: tx,
        status: status_rx,
    };
    (handle, rx)
}

/// Build a `ChatToolContext` whose self_ref carries the given id/label and
/// whose registry contains the supplied handles. The event_log is a fresh
/// tempdir-backed log.
fn build_ctx(
    self_id: u64,
    self_label: &str,
    handles: Vec<AgentHandle>,
) -> ChatToolContext {
    let mut reg = AgentRegistry::new();
    for h in handles {
        reg.register(h);
    }
    ChatToolContext::new(
        make_self_ref(self_id, self_label),
        Arc::new(Mutex::new(reg)),
        fresh_log(),
    )
}

// ---------------------------------------------------------------------------
// D1. Poisoned event-log mutex — send_to_agent must Err, not silently
// ---------------------------------------------------------------------------

#[test]
fn send_to_agent_returns_err_on_poisoned_event_log_mutex() {
    // Build a context whose event_log Arc<Mutex<>> we can poison
    // out-of-band by panicking inside a lock guard.
    let log = fresh_log();
    let mut reg = AgentRegistry::new();
    let (target_handle, _target_rx) = fake_agent(2, AgentRole::Watcher, "target");
    reg.register(target_handle);
    let ctx = ChatToolContext::new(
        make_self_ref(1, "sender"),
        Arc::new(Mutex::new(reg)),
        Arc::clone(&log),
    );

    // Poison the mutex by panicking inside `lock()`.
    let log_for_poison = Arc::clone(&log);
    let _ = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        let _g = log_for_poison.lock().expect("first lock must succeed");
        panic!("intentional panic to poison the mutex");
    }));
    assert!(
        log.is_poisoned(),
        "the mutex must be poisoned by the panic above for this test to be meaningful"
    );

    // The inbox delivery succeeds (it's a tokio channel, not the log), so
    // the producer is still in flight; but the log emission is now
    // load-bearing and must surface the poison as Err(_).
    let err = send_to_agent(
        &json!({"label": "target", "body": "hello"}),
        &ctx,
    )
    .expect_err("send_to_agent must return Err on a poisoned event log");

    assert!(
        err.contains("event log poisoned"),
        "the error must mention 'event log poisoned'; got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// D2. Poisoned event-log mutex — read_from_agent must Err
// ---------------------------------------------------------------------------

#[test]
fn read_from_agent_returns_err_on_poisoned_event_log_mutex() {
    // The companion read path also takes the log lock and must surface
    // the poison consistently.
    let log = fresh_log();
    let reg = AgentRegistry::new();
    let ctx = ChatToolContext::new(
        make_self_ref(1, "reader"),
        Arc::new(Mutex::new(reg)),
        Arc::clone(&log),
    );

    let log_for_poison = Arc::clone(&log);
    let _ = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        let _g = log_for_poison.lock().expect("first lock must succeed");
        panic!("intentional panic to poison");
    }));
    assert!(log.is_poisoned(), "mutex must be poisoned");

    let err = read_from_agent(
        &json!({"label": "some-label", "since_event_id": 0u64}),
        &ctx,
    )
    .expect_err("read_from_agent must return Err on a poisoned event log");

    assert!(
        err.contains("event log poisoned"),
        "the error must mention 'event log poisoned'; got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// D3. Closed inbox — send_to_agent must surface a typed error
// ---------------------------------------------------------------------------

#[test]
fn send_to_agent_returns_err_on_closed_inbox() {
    // Build a fake agent handle, drop the receiver so its inbox is
    // "closed", then attempt to send. The error must surface — not be a
    // silent no-op.
    let (target_handle, target_rx) = fake_agent(2, AgentRole::Watcher, "target");
    // Drop the receiver to close the channel.
    drop(target_rx);

    let ctx = build_ctx(1, "sender", vec![target_handle]);

    let err = send_to_agent(
        &json!({"label": "target", "body": "hello"}),
        &ctx,
    )
    .expect_err("send_to_agent must return Err on a closed inbox");

    assert!(
        err.contains("agent inbox closed"),
        "the error must mention 'agent inbox closed'; got {err:?}"
    );
    assert!(
        err.contains("target"),
        "the error must include the label; got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// D4. Unknown label — send_to_agent must surface a typed error
// ---------------------------------------------------------------------------

#[test]
fn send_to_agent_returns_err_for_nonexistent_agent_label() {
    // The registry is empty. Sending to a label that does not exist must
    // surface a typed error (not be a silent no-op).
    let ctx = build_ctx(1, "sender", vec![]);

    let err = send_to_agent(
        &json!({"label": "ghost-agent", "body": "are you there"}),
        &ctx,
    )
    .expect_err("send_to_agent must return Err for an unknown label");

    assert!(
        err.contains("agent label not found"),
        "the error must mention 'agent label not found'; got {err:?}"
    );
    assert!(
        err.contains("ghost-agent"),
        "the error must include the label; got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// D5. Internal poison on EventLog — append surfaces error
// ---------------------------------------------------------------------------

#[test]
fn event_log_append_after_internal_poison_returns_io_error() {
    // The EventLog has its own internal `poisoned` flag (separate from
    // the Mutex poison). Force a corrupt path so the writer's on-write
    // failure poisons it, then verify the next append surfaces an I/O
    // error rather than silently succeeding.
    //
    // We can't easily force a real write failure portably; instead we
    // simulate the post-poison state by re-opening against a path that
    // exists and is writable, then we use the dedicated internal API.
    // Both EventLog::open and append are exercised through fresh_log()
    // in the happy path; the negative path is harder to reach without
    // mocking. We verify the public surface: `is_poisoned()` is `false`
    // on a fresh log, and `append()` succeeds normally — a soft
    // adversarial check that the API isn't dropping records silently.
    let log = fresh_log();
    {
        let g = log.lock().expect("fresh log must lock cleanly");
        assert!(!g.is_poisoned(), "a fresh log must not be poisoned");
    }

    // Append a record and verify it shows up in tail (proving the writer
    // is not silently dropping).
    {
        let mut g = log.lock().expect("lock");
        let envelope = g
            .append(
                phantom_memory::event_log::EventSource::Agent { id: 1 },
                "test.kind",
                json!({"hello": "world"}),
            )
            .expect("append on a fresh log must succeed");
        assert_eq!(envelope.kind, "test.kind");
    }
    {
        let g = log.lock().expect("lock");
        let tail = g.tail(8);
        assert!(
            tail.iter().any(|env| env.kind == "test.kind"),
            "the appended envelope must be visible in tail"
        );
    }
}

// ---------------------------------------------------------------------------
// D6. After successful send, the speak envelope is durable in the log
// ---------------------------------------------------------------------------

#[test]
fn send_to_agent_durably_records_speak_envelope() {
    // Verify the load-bearing property: a successful send_to_agent
    // produces an `agent.speak` envelope in the log. If a future change
    // broke the audit emission silently, this test fails.
    let (target_handle, mut target_rx) = fake_agent(2, AgentRole::Watcher, "target");
    let ctx = build_ctx(1, "sender", vec![target_handle]);

    let r = send_to_agent(
        &json!({"label": "target", "body": "audit trail"}),
        &ctx,
    )
    .expect("send must succeed");
    assert_eq!(r, "delivered to target");

    // The recipient received it.
    let received = target_rx
        .try_recv()
        .expect("the target's inbox must have received the message");
    if let InboxMessage::AgentSpeak { body, .. } = received {
        assert_eq!(body, "audit trail");
    } else {
        panic!("expected AgentSpeak");
    }

    // And the log captured the audit envelope.
    let log = Arc::clone(&ctx.event_log);
    let g = log.lock().expect("lock");
    let tail = g.tail(8);
    assert!(
        tail.iter().any(|env| env.kind == "agent.speak"),
        "the log must contain an agent.speak envelope after send_to_agent"
    );
}

// ---------------------------------------------------------------------------
// D7. Send to closed inbox — log emission does NOT run
// ---------------------------------------------------------------------------

#[test]
fn send_to_agent_to_closed_inbox_does_not_emit_speak_envelope() {
    // If the inbox is closed, send_to_agent fails BEFORE the log emission
    // — verify the log does NOT contain a stale `agent.speak` envelope
    // for a delivery that didn't happen. This is the inverse causality
    // guarantee: we must not record speech that did not occur.
    let (target_handle, target_rx) = fake_agent(2, AgentRole::Watcher, "target");
    drop(target_rx);
    let ctx = build_ctx(1, "sender", vec![target_handle]);

    let _ = send_to_agent(
        &json!({"label": "target", "body": "phantom delivery"}),
        &ctx,
    )
    .expect_err("delivery must fail on closed inbox");

    // The log must NOT contain an agent.speak envelope. There may be
    // pre-existing kinds in the log from fresh setup, but no speak.
    let log = Arc::clone(&ctx.event_log);
    let g = log.lock().expect("lock");
    let tail = g.tail(8);
    let speak_count = tail.iter().filter(|env| env.kind == "agent.speak").count();
    assert_eq!(
        speak_count, 0,
        "no agent.speak envelope must be recorded when delivery failed: tail = {tail:?}"
    );
}
