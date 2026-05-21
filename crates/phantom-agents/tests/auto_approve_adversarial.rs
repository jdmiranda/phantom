//! Adversarial integration tests for `Agent::try_auto_approve_with_audit`
//! (issue #648).
//!
//! Covers behavior under conditions the happy-path tests don't exercise:
//!
//! 1. Poisoned event-log mutex — the helper's contract says the FSM
//!    transition still happens; the log append is best-effort. Verify the
//!    helper does NOT panic on a poisoned lock and the FSM outcome is
//!    preserved.
//! 2. Non-auto-approvable disposition — the envelope must be appended with
//!    `approved: false` (per the helper docs: "the envelope is written even
//!    when the fast path is *refused*"). This is the audit-completeness
//!    invariant the issue #648 fix was designed to provide.
//! 3. Repeated calls accumulate envelopes — three calls produce three
//!    envelopes in the log.
//! 4. FSM-refused (non-Queued) auto-approval — the envelope still appears
//!    with `approved: false` and the reason explains why.

use std::panic;
use std::sync::{Arc, Mutex};

use phantom_agents::agent::{
    AUTO_APPROVE_EVENT_KIND, Agent, AgentStatus, AgentTask,
};
use phantom_agents::dispatch::Disposition;
use phantom_memory::event_log::EventLog;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn open_event_log() -> (Arc<Mutex<EventLog>>, TempDir) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("events.jsonl");
    let log = EventLog::open(&path).expect("open");
    (Arc::new(Mutex::new(log)), tmp)
}

// ---------------------------------------------------------------------------
// E1. Poisoned event-log mutex — FSM outcome preserved, no panic
// ---------------------------------------------------------------------------

#[test]
fn try_auto_approve_with_audit_under_poisoned_mutex_preserves_fsm_outcome() {
    // Per the helper's docs: "A poisoned or full event log is best-effort:
    // the FSM transition still happens and the outcome is still returned."
    //
    // Build a log, poison its Mutex by panicking inside `lock()`, then
    // invoke the helper. Verify:
    //  - the call does NOT panic (the helper swallows the lock-poison),
    //  - the FSM transitioned (status moved Queued -> Working for Chat),
    //  - the returned outcome reports `approved = true`.
    let (log, _tmp) = open_event_log();
    let mut agent = Agent::with_disposition(
        1,
        AgentTask::FreeForm { prompt: "summarise".into() },
        Disposition::Chat,
    );

    let log_for_poison = Arc::clone(&log);
    let _ = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        let _g = log_for_poison.lock().expect("first lock must succeed");
        panic!("intentional panic to poison the event-log mutex");
    }));
    assert!(
        log.is_poisoned(),
        "the mutex must be poisoned before the test continues"
    );

    // The helper must not panic. The audit envelope is dropped silently
    // (the helper's documented contract), but the FSM outcome is honored.
    let outcome = agent.try_auto_approve_with_audit(&log);

    assert!(
        outcome.approved,
        "Chat disposition must auto-approve even when the audit log is poisoned"
    );
    assert_eq!(
        outcome.disposition,
        Disposition::Chat,
        "outcome must echo back the disposition"
    );
    assert_eq!(
        agent.status(),
        AgentStatus::Working,
        "FSM transition must run even when the audit append is dropped"
    );
}

// ---------------------------------------------------------------------------
// E2. Non-auto-approvable disposition — refusal envelope appended
// ---------------------------------------------------------------------------

#[test]
fn try_auto_approve_with_audit_appends_envelope_on_non_auto_approvable_disposition() {
    // Per the helper docs and the issue #648 issue body: "the envelope is
    // written even when the fast path is refused so an auditor can see
    // attempted bypasses, not just successful ones."
    //
    // Use `Disposition::Feature` (one of the non-auto-approvable variants).
    // The agent stays Queued and the envelope appears with approved=false.
    let (log, _tmp) = open_event_log();
    let mut agent = Agent::with_disposition(
        42,
        AgentTask::FreeForm { prompt: "implement X".into() },
        Disposition::Feature,
    );

    let outcome = agent.try_auto_approve_with_audit(&log);

    // Outcome reports a refusal.
    assert!(!outcome.approved);
    assert_eq!(outcome.disposition, Disposition::Feature);
    assert_eq!(outcome.agent_id, 42);
    assert_eq!(agent.status(), AgentStatus::Queued);

    // The envelope IS appended even on refusal — this is the audit
    // completeness invariant the issue was built to provide.
    let g = log.lock().expect("lock event log");
    let envs: Vec<_> = g
        .tail(16)
        .into_iter()
        .filter(|env| env.kind == AUTO_APPROVE_EVENT_KIND)
        .collect();
    assert_eq!(envs.len(), 1, "exactly one audit envelope per call");
    assert_eq!(envs[0].payload["agent_id"], 42);
    assert_eq!(envs[0].payload["approved"], false);
    assert_eq!(envs[0].payload["disposition"], "Feature");
    let reason = envs[0].payload["reason"].as_str().unwrap_or("");
    assert!(
        reason.contains("not auto-approvable"),
        "the refusal reason must explain WHY the fast path was refused; got {reason:?}"
    );
}

// ---------------------------------------------------------------------------
// E3. Repeated calls accumulate envelopes
// ---------------------------------------------------------------------------

#[test]
fn try_auto_approve_with_audit_repeated_calls_accumulate_envelopes() {
    // Three calls — one Chat (approves), two Feature (refused). The log
    // must contain three audit envelopes in order.
    let (log, _tmp) = open_event_log();
    let mut a1 = Agent::with_disposition(
        1,
        AgentTask::FreeForm { prompt: "summarise".into() },
        Disposition::Chat,
    );
    let mut a2 = Agent::with_disposition(
        2,
        AgentTask::FreeForm { prompt: "feature 2".into() },
        Disposition::Feature,
    );
    let mut a3 = Agent::with_disposition(
        3,
        AgentTask::FreeForm { prompt: "feature 3".into() },
        Disposition::Feature,
    );

    let _ = a1.try_auto_approve_with_audit(&log);
    let _ = a2.try_auto_approve_with_audit(&log);
    let _ = a3.try_auto_approve_with_audit(&log);

    let g = log.lock().expect("lock event log");
    let envs: Vec<_> = g
        .tail(16)
        .into_iter()
        .filter(|env| env.kind == AUTO_APPROVE_EVENT_KIND)
        .collect();
    assert_eq!(
        envs.len(),
        3,
        "three calls must produce three audit envelopes; got {envs:?}"
    );

    // Order is chronological (oldest first).
    assert_eq!(envs[0].payload["agent_id"], 1);
    assert_eq!(envs[0].payload["approved"], true);
    assert_eq!(envs[1].payload["agent_id"], 2);
    assert_eq!(envs[1].payload["approved"], false);
    assert_eq!(envs[2].payload["agent_id"], 3);
    assert_eq!(envs[2].payload["approved"], false);
}

// ---------------------------------------------------------------------------
// E4. FSM-refused — envelope still appears with `approved: false`
// ---------------------------------------------------------------------------

#[test]
fn try_auto_approve_with_audit_fsm_refused_still_emits_audit_envelope() {
    // Auto-approvable disposition (Chat) BUT the agent is not in Queued —
    // force the FSM into a state the no-gate fast path cannot transition
    // from. The helper must still emit the audit envelope with
    // `approved: false` and a reason explaining the FSM refusal.
    let (log, _tmp) = open_event_log();
    let mut agent = Agent::with_disposition(
        9,
        AgentTask::FreeForm { prompt: "summarise".into() },
        Disposition::Chat,
    );
    // Force the agent into a non-Queued state. `Done` is terminal and the
    // fast path cannot transition from Done.
    agent.force_status_for_test(AgentStatus::Done);
    assert_eq!(agent.status(), AgentStatus::Done);

    let outcome = agent.try_auto_approve_with_audit(&log);

    // Auto-approvable disposition, but FSM refuses the transition from a
    // non-Queued state.
    assert!(
        !outcome.approved,
        "FSM must refuse the transition from a non-Queued state"
    );
    assert_eq!(outcome.disposition, Disposition::Chat);
    assert_eq!(
        agent.status(),
        AgentStatus::Done,
        "FSM-refused transition must not mutate the status"
    );

    // The audit envelope must still appear.
    let g = log.lock().expect("lock event log");
    let envs: Vec<_> = g
        .tail(16)
        .into_iter()
        .filter(|env| env.kind == AUTO_APPROVE_EVENT_KIND)
        .collect();
    assert_eq!(envs.len(), 1);
    assert_eq!(envs[0].payload["approved"], false);
    // The reason for THIS refusal must distinguish from the
    // "not auto-approvable" case — i.e. it must mention the FSM, the
    // status, or refusal-by-FSM in some form.
    let reason = envs[0].payload["reason"].as_str().unwrap_or("");
    assert!(
        reason.contains("FSM refused") || reason.contains("current status"),
        "FSM-refusal reason must distinguish from disposition-refusal; got {reason:?}"
    );
}

// ---------------------------------------------------------------------------
// E5. Approved envelope payload shape — agent_id and reason present
// ---------------------------------------------------------------------------

#[test]
fn try_auto_approve_with_audit_approved_envelope_carries_expected_payload() {
    // Pin the envelope's payload shape so downstream consumers (inspector
    // pane, brain reconciler, future policy auditors) can rely on the
    // schema. Spec: { agent_id, approved, disposition, reason }.
    let (log, _tmp) = open_event_log();
    let mut agent = Agent::with_disposition(
        100,
        AgentTask::FreeForm { prompt: "summarise".into() },
        Disposition::Synthesize,
    );

    let _ = agent.try_auto_approve_with_audit(&log);

    let g = log.lock().expect("lock");
    let env = g
        .tail(16)
        .into_iter()
        .find(|env| env.kind == AUTO_APPROVE_EVENT_KIND)
        .expect("an audit envelope must be appended");

    // Schema check. Every field must be present and well-typed.
    assert!(env.payload["agent_id"].is_u64());
    assert_eq!(env.payload["agent_id"], 100);
    assert!(env.payload["approved"].is_boolean());
    assert_eq!(env.payload["approved"], true);
    assert!(env.payload["disposition"].is_string());
    assert_eq!(env.payload["disposition"], "Synthesize");
    assert!(env.payload["reason"].is_string());
    assert!(
        !env.payload["reason"].as_str().unwrap().is_empty(),
        "the reason must be a non-empty human-readable string"
    );
}
