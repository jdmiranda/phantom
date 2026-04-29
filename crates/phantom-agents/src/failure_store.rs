//! Failure preservation for agent retries.
//!
//! When an agent task fails — due to a tool error, API error, or timeout — the
//! context of that failure is normally discarded. This module preserves failure
//! context so the brain can construct informed retry prompts and detect patterns
//! of repeated failures.
//!
//! ## Types
//!
//! - [`FailureRecord`] — a snapshot of one failed execution: which agent, which
//!   task, which tool call (if any), what error, when, and how many attempts.
//! - [`FailureStore`] — a ring buffer of up to 100 [`FailureRecord`]s with
//!   per-agent and recency query methods.
//!
//! ## Design
//!
//! All fields on [`FailureRecord`] are private; access goes through named
//! accessors so the shape can evolve without breaking call sites.
//!
//! [`FailureStore`] is a plain `struct` with no `Arc`/`Mutex` — callers
//! own the wrapping if they need shared access.

use std::collections::VecDeque;
use std::time::SystemTime;

use crate::agent::{AgentId, AgentTask};
use crate::tools::ToolCall;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Maximum number of failure records the store retains. Oldest entry is
/// evicted when the store exceeds this limit.
const MAX_RECORDS: usize = 100;

// ---------------------------------------------------------------------------
// FailureRecord
// ---------------------------------------------------------------------------

/// A snapshot of one failed agent execution.
///
/// All fields are private; use the named accessors to read them.
pub struct FailureRecord {
    agent_id: AgentId,
    task: AgentTask,
    tool_call: Option<ToolCall>,
    error: String,
    timestamp: SystemTime,
    attempt_count: u32,
}

impl FailureRecord {
    /// Construct a new failure record.
    ///
    /// `attempt_count` should be 1 for the first failure, 2 for the first
    /// retry, and so on.
    pub fn new(
        agent_id: AgentId,
        task: AgentTask,
        tool_call: Option<ToolCall>,
        error: impl Into<String>,
        attempt_count: u32,
    ) -> Self {
        Self {
            agent_id,
            task,
            tool_call,
            error: error.into(),
            timestamp: SystemTime::now(),
            attempt_count,
        }
    }

    /// The id of the agent that failed.
    pub fn agent_id(&self) -> AgentId {
        self.agent_id
    }

    /// The task the agent was executing when it failed.
    pub fn task(&self) -> &AgentTask {
        &self.task
    }

    /// The tool call that triggered the failure, if the failure originated
    /// from a tool dispatch.
    pub fn tool_call(&self) -> Option<&ToolCall> {
        self.tool_call.as_ref()
    }

    /// Human-readable error string.
    pub fn error(&self) -> &str {
        &self.error
    }

    /// Wall-clock time of the failure.
    pub fn timestamp(&self) -> SystemTime {
        self.timestamp
    }

    /// How many times this agent has attempted the task (including this failure).
    pub fn attempt_count(&self) -> u32 {
        self.attempt_count
    }
}

// ---------------------------------------------------------------------------
// FailureStore
// ---------------------------------------------------------------------------

/// Ring-buffer store for [`FailureRecord`]s, capped at [`MAX_RECORDS`].
///
/// Records are appended with [`push`][Self::push]. The oldest record is
/// evicted when the cap is reached. Query with [`recent`][Self::recent] or
/// [`by_agent`][Self::by_agent]. Use [`clear_agent`][Self::clear_agent] when
/// a successful retry resolves an agent's failure streak.
pub struct FailureStore {
    records: VecDeque<FailureRecord>,
}

impl FailureStore {
    /// Create an empty store.
    pub fn new() -> Self {
        Self {
            records: VecDeque::new(),
        }
    }

    /// Push a failure record, evicting the oldest entry when the store is full.
    pub fn push(&mut self, record: FailureRecord) {
        if self.records.len() >= MAX_RECORDS {
            self.records.pop_front();
        }
        self.records.push_back(record);
    }

    /// Return references to the last `n` failure records, most-recent-last.
    ///
    /// If there are fewer than `n` records, all records are returned.
    pub fn recent(&self, n: usize) -> Vec<&FailureRecord> {
        let skip = self.records.len().saturating_sub(n);
        self.records.iter().skip(skip).collect()
    }

    /// Return all failure records for `agent_id`, in insertion order.
    pub fn by_agent(&self, agent_id: AgentId) -> Vec<&FailureRecord> {
        self.records
            .iter()
            .filter(|r| r.agent_id == agent_id)
            .collect()
    }

    /// Remove all failure records for `agent_id`.
    ///
    /// Call this when a successful retry resolves the agent's failure streak.
    pub fn clear_agent(&mut self, agent_id: AgentId) {
        self.records.retain(|r| r.agent_id != agent_id);
    }

    /// Total number of records currently stored.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Returns `true` when the store contains no records.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

impl Default for FailureStore {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentTask;
    use crate::tools::{ToolCall, ToolType};

    // Helpers -----------------------------------------------------------------

    fn free_form_task(s: &str) -> AgentTask {
        AgentTask::FreeForm {
            prompt: s.to_owned(),
        }
    }

    fn make_record(agent_id: AgentId, error: &str, attempt: u32) -> FailureRecord {
        FailureRecord::new(agent_id, free_form_task("test"), None, error, attempt)
    }

    fn make_record_with_tool(
        agent_id: AgentId,
        error: &str,
        attempt: u32,
    ) -> FailureRecord {
        let tool_call = ToolCall {
            tool: ToolType::RunCommand,
            args: serde_json::json!({"command": "cargo test"}),
        };
        FailureRecord::new(
            agent_id,
            free_form_task("test"),
            Some(tool_call),
            error,
            attempt,
        )
    }

    // Test 1: push and retrieve -----------------------------------------------

    #[test]
    fn push_and_retrieve_single_record() {
        let mut store = FailureStore::new();
        store.push(make_record(1, "timeout", 1));

        assert_eq!(store.len(), 1);
        let recents = store.recent(10);
        assert_eq!(recents.len(), 1);
        assert_eq!(recents[0].agent_id(), 1);
        assert_eq!(recents[0].error(), "timeout");
        assert_eq!(recents[0].attempt_count(), 1);
    }

    // Test 2: cap at 100 (oldest evicted) -------------------------------------

    #[test]
    fn store_caps_at_100_records_evicting_oldest() {
        let mut store = FailureStore::new();

        // Push 101 records; the first one (agent_id=0, error="first") must
        // be evicted so the total stays at 100.
        store.push(FailureRecord::new(
            0,
            free_form_task("sentinel"),
            None,
            "first",
            1,
        ));

        for i in 1u64..=100 {
            store.push(make_record(i, "bulk", 1));
        }

        assert_eq!(store.len(), 100, "store must be capped at 100");

        // The first record (error="first") must have been evicted.
        let all: Vec<_> = store.recent(100);
        let has_first = all.iter().any(|r| r.error() == "first");
        assert!(!has_first, "oldest record must have been evicted");

        // The last pushed record (agent_id=100) must still be present.
        let has_last = all.iter().any(|r| r.agent_id() == 100);
        assert!(has_last, "most-recent record must still be present");
    }

    // Test 3: by_agent filter -------------------------------------------------

    #[test]
    fn by_agent_returns_only_matching_records() {
        let mut store = FailureStore::new();
        store.push(make_record(1, "err-a", 1));
        store.push(make_record(2, "err-b", 1));
        store.push(make_record(1, "err-c", 2));
        store.push(make_record(3, "err-d", 1));

        let agent1 = store.by_agent(1);
        assert_eq!(agent1.len(), 2);
        assert_eq!(agent1[0].error(), "err-a");
        assert_eq!(agent1[1].error(), "err-c");

        let agent2 = store.by_agent(2);
        assert_eq!(agent2.len(), 1);
        assert_eq!(agent2[0].error(), "err-b");

        let agent99 = store.by_agent(99);
        assert!(agent99.is_empty());
    }

    // Test 4: clear_agent on successful retry ---------------------------------

    #[test]
    fn clear_agent_removes_failures_for_that_agent() {
        let mut store = FailureStore::new();
        store.push(make_record(1, "first-fail", 1));
        store.push(make_record(2, "other-fail", 1));
        store.push(make_record(1, "second-fail", 2));

        assert_eq!(store.len(), 3);

        // Successful retry for agent 1 — clear its history.
        store.clear_agent(1);

        assert_eq!(store.len(), 1, "only agent-2 record should remain");
        let remaining = store.by_agent(1);
        assert!(remaining.is_empty(), "agent-1 must have no records after clear");

        // Agent 2's record must be untouched.
        let agent2 = store.by_agent(2);
        assert_eq!(agent2.len(), 1);
        assert_eq!(agent2[0].error(), "other-fail");
    }

    // Test 5: multi-agent isolation -------------------------------------------

    #[test]
    fn agents_do_not_bleed_into_each_other() {
        let mut store = FailureStore::new();

        // Interleave records for three agents.
        for i in 0..5u32 {
            store.push(make_record(10, &format!("a-{i}"), i + 1));
            store.push(make_record(20, &format!("b-{i}"), i + 1));
            store.push(make_record(30, &format!("c-{i}"), i + 1));
        }

        // Each agent must have exactly 5 records.
        assert_eq!(store.by_agent(10).len(), 5);
        assert_eq!(store.by_agent(20).len(), 5);
        assert_eq!(store.by_agent(30).len(), 5);

        // Clearing agent 20 must not affect 10 or 30.
        store.clear_agent(20);

        assert_eq!(store.by_agent(10).len(), 5);
        assert!(store.by_agent(20).is_empty());
        assert_eq!(store.by_agent(30).len(), 5);
    }

    // Test 6: oldest evicted first (ordering invariant) -----------------------

    #[test]
    fn oldest_record_evicted_first_not_newest() {
        let mut store = FailureStore::new();

        // Fill to capacity.
        for i in 0u64..100 {
            store.push(make_record(i, &format!("err-{i}"), 1));
        }

        // Now push one more — error="err-100", agent_id=100.
        store.push(make_record(100, "err-100", 1));

        assert_eq!(store.len(), 100);

        // Record 0 (the oldest) must be gone.
        let all = store.recent(100);
        let has_0 = all.iter().any(|r| r.error() == "err-0");
        assert!(!has_0, "record 0 (oldest) must be evicted, not record 100");

        // Record 100 (newest) must be present.
        let has_100 = all.iter().any(|r| r.error() == "err-100");
        assert!(has_100, "record 100 (newest) must survive eviction");

        // Record 1 (second-oldest after eviction of 0) must still be there.
        let has_1 = all.iter().any(|r| r.error() == "err-1");
        assert!(has_1, "record 1 must survive (only oldest evicted)");
    }

    // Additional: accessor coverage -------------------------------------------

    #[test]
    fn accessors_expose_all_fields() {
        let record = make_record_with_tool(42, "dispatch error", 3);

        assert_eq!(record.agent_id(), 42);
        assert_eq!(record.error(), "dispatch error");
        assert_eq!(record.attempt_count(), 3);
        assert!(record.tool_call().is_some());
        assert_eq!(record.tool_call().unwrap().tool, ToolType::RunCommand);

        // timestamp should be recent (within the last few seconds).
        let elapsed = record
            .timestamp()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("timestamp must be valid")
            .as_secs();
        assert!(elapsed > 0);
    }

    #[test]
    fn recent_with_n_larger_than_store_returns_all() {
        let mut store = FailureStore::new();
        store.push(make_record(1, "a", 1));
        store.push(make_record(2, "b", 1));

        let r = store.recent(100);
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn recent_zero_returns_empty() {
        let mut store = FailureStore::new();
        store.push(make_record(1, "a", 1));

        let r = store.recent(0);
        assert!(r.is_empty());
    }

    #[test]
    fn is_empty_and_len_track_correctly() {
        let mut store = FailureStore::new();
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);

        store.push(make_record(1, "err", 1));
        assert!(!store.is_empty());
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn default_produces_empty_store() {
        let store: FailureStore = Default::default();
        assert!(store.is_empty());
    }
}
