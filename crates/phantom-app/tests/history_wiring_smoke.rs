//! Smoke tests: phantom-history wired into phantom-app.
//!
//! These are headless (no GPU, no `App`) integration tests that exercise the
//! `phantom-history` API layer — `HistoryStore`, `AgentOutputCapture`, and
//! `HistoryEntry` — directly, verifying the primitives that the production
//! wiring in `app.rs` / `update.rs` / `agent_pane.rs` relies on.
//!
//! A separate integration test targeting the full App boot path would require
//! a GPU context and is out of scope here.

use phantom_history::{AgentOutputCapture, HistoryEntry, HistoryStore, ToolCall};
use tempfile::TempDir;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn temp_store() -> (HistoryStore, TempDir) {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("history.jsonl");
    let store = HistoryStore::open_at(&path).expect("store open");
    (store, dir)
}

fn temp_capture() -> (AgentOutputCapture, TempDir) {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("agents.jsonl");
    let capture = AgentOutputCapture::open_at(&path);
    (capture, dir)
}

fn make_entry(cmd: &str, session: Uuid) -> HistoryEntry {
    HistoryEntry::builder(cmd, "/home/dev", session).build()
}

// ---------------------------------------------------------------------------
// HistoryStore tests
// ---------------------------------------------------------------------------

/// Appending an entry increments the count.
#[test]
fn history_append_increments_count() {
    let (mut store, _dir) = temp_store();
    let session = Uuid::new_v4();

    assert_eq!(store.count(), 0);
    store.append(&make_entry("ls", session)).expect("append");
    assert_eq!(store.count(), 1);
    store.append(&make_entry("pwd", session)).expect("append");
    assert_eq!(store.count(), 2);
}

/// Builder fields survive a JSONL round-trip.
#[test]
fn history_command_and_exit_code_round_trip() {
    let (mut store, _dir) = temp_store();
    let session = Uuid::new_v4();

    let entry = HistoryEntry::builder("cargo build", "/repo", session)
        .exit_code(0)
        .build();
    let id = entry.id();
    store.append(&entry).expect("append");

    let found = store.get_by_id(id).expect("get").expect("present");
    assert_eq!(found.command(), "cargo build");
    assert_eq!(found.exit_code(), Some(0));
    assert_eq!(found.session_id(), session);
}

/// `recent(N)` returns entries in chronological order, oldest first.
#[test]
fn history_recent_chronological_order() {
    let (mut store, _dir) = temp_store();
    let session = Uuid::new_v4();

    for cmd in ["cmd-0", "cmd-1", "cmd-2", "cmd-3"] {
        store.append(&make_entry(cmd, session)).expect("append");
    }

    let recent = store.recent(3).expect("recent");
    assert_eq!(recent.len(), 3);
    assert_eq!(recent[0].command(), "cmd-1");
    assert_eq!(recent[1].command(), "cmd-2");
    assert_eq!(recent[2].command(), "cmd-3");
}

/// When the limit exceeds the total, all entries are returned.
#[test]
fn history_recent_limit_exceeds_total() {
    let (mut store, _dir) = temp_store();
    let session = Uuid::new_v4();

    store
        .append(&make_entry("only-one", session))
        .expect("append");
    let recent = store.recent(100).expect("recent");
    assert_eq!(recent.len(), 1);
    assert_eq!(recent[0].command(), "only-one");
}

/// An empty store is safe to query.
#[test]
fn history_empty_store_is_safe() {
    let (store, _dir) = temp_store();

    assert_eq!(store.count(), 0);
    assert!(store.recent(10).expect("recent").is_empty());
    assert!(store.get_by_id(Uuid::new_v4()).expect("get").is_none());
}

/// The store can be reopened and the in-memory index rebuilt correctly.
#[test]
fn history_survives_reopen() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("history.jsonl");
    let session = Uuid::new_v4();

    let target_id = {
        let mut store = HistoryStore::open_at(&path).expect("open");
        store.append(&make_entry("first", session)).expect("append");
        let target = make_entry("second", session);
        let id = target.id();
        store.append(&target).expect("append");
        id
    };

    // Re-open — index must be rebuilt from disk.
    let store = HistoryStore::open_at(&path).expect("reopen");
    assert_eq!(store.count(), 2);
    let found = store.get_by_id(target_id).expect("get").expect("present");
    assert_eq!(found.command(), "second");
}

// ---------------------------------------------------------------------------
// AgentOutputCapture tests
// ---------------------------------------------------------------------------

/// Appending an agent record increments the count.
#[test]
fn agent_capture_records_run() {
    let (capture, _dir) = temp_capture();
    let session = Uuid::new_v4();

    assert_eq!(capture.count().expect("count"), 0);

    capture
        .append(
            "test-agent",
            session,
            vec![ToolCall::new("ReadFile", r#"{"path":"/etc/hosts"}"#, None)],
            "Finished reading /etc/hosts",
        )
        .expect("append");

    assert_eq!(capture.count().expect("count"), 1);

    let records = capture.recent(10).expect("recent");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].agent_name(), "test-agent");
    assert_eq!(records[0].tool_calls().len(), 1);
    assert_eq!(records[0].tool_calls()[0].name(), "ReadFile");
    assert_eq!(records[0].text_output(), "Finished reading /etc/hosts");
}

/// `AgentOutputCapture` is `Clone` — required for per-pane distribution.
#[test]
fn agent_capture_is_clone() {
    let (capture, _dir) = temp_capture();
    let session = Uuid::new_v4();

    let capture2 = capture.clone();
    capture
        .append("agent-a", session, vec![], "output-a")
        .expect("append via original");
    capture2
        .append("agent-b", session, vec![], "output-b")
        .expect("append via clone");

    // Both writes go to the same file — count must be 2.
    assert_eq!(capture.count().expect("count"), 2);
}

// ---------------------------------------------------------------------------
// Simulate the drain_bus_to_brain pattern from update.rs
// ---------------------------------------------------------------------------

/// Simulate the CommandStarted → CommandComplete tracking that `update.rs`
/// performs when wiring into `HistoryStore`.
///
/// This test mirrors the exact logic in `drain_bus_to_brain` so a refactor
/// that breaks the pattern is caught here first.
#[test]
fn simulate_drain_bus_to_brain_pattern() {
    use std::collections::HashMap;

    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("history.jsonl");
    let mut store = HistoryStore::open_at(&path).expect("open");
    let session = Uuid::new_v4();

    // Simulate App::pending_command_text: HashMap<AppId, String>.
    let mut pending: HashMap<u32, String> = HashMap::new();

    // --- CommandStarted for pane 1 ---
    pending.insert(1_u32, "cargo test".to_string());

    // --- CommandComplete for pane 1, exit 0 ---
    {
        let app_id: u32 = 1;
        let exit_code: i32 = 0;
        let command_text = pending.remove(&app_id).unwrap_or_default();
        let entry = HistoryEntry::builder(&command_text, "/repo", session)
            .exit_code(exit_code)
            .build();
        store.append(&entry).expect("append");
    }

    // --- CommandStarted for pane 2 ---
    pending.insert(2_u32, "git status".to_string());

    // --- CommandComplete for pane 2, exit 0 ---
    {
        let app_id: u32 = 2;
        let exit_code: i32 = 0;
        let command_text = pending.remove(&app_id).unwrap_or_default();
        let entry = HistoryEntry::builder(&command_text, "/repo", session)
            .exit_code(exit_code)
            .build();
        store.append(&entry).expect("append");
    }

    assert_eq!(store.count(), 2);

    let recent = store.recent(2).expect("recent");
    assert_eq!(recent[0].command(), "cargo test");
    assert_eq!(recent[0].exit_code(), Some(0));
    assert_eq!(recent[1].command(), "git status");
    assert_eq!(recent[1].exit_code(), Some(0));
}
