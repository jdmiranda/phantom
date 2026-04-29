//! Append-only handoff log — durable JSONL record of agent-to-agent task
//! transfers.
//!
//! [`HandoffLog`] mirrors the layout of [`crate::journal::AgentJournal`]:
//! every entry is persisted as one JSON line in a JSONL file via
//! [`crate::event_log::EventLog`] and the most-recent entries are kept in the
//! in-memory tail for fast reads.
//!
//! # Example
//!
//! ```rust,no_run
//! use phantom_memory::handoff_log::{HandoffEntry, HandoffLog};
//! use std::path::Path;
//!
//! let mut log = HandoffLog::open(Path::new("/tmp/handoffs.jsonl")).unwrap();
//!
//! let entry = HandoffEntry::new(
//!     1, 2,
//!     "task-abc",
//!     "phase 1 complete",
//!     vec!["curl 403".into()],
//!     vec!["mem-xyz".into()],
//!     None,
//!     0,
//! );
//!
//! log.record(&entry).unwrap();
//! let recent = log.recent(10);
//! assert_eq!(recent.len(), 1);
//! ```

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::event_log::{EventLog, EventSource};

// ---------------------------------------------------------------------------
// HandoffEntry
// ---------------------------------------------------------------------------

/// One agent-to-agent task transfer recorded in the log.
///
/// Fields are private; use [`HandoffEntry::new`] to construct and the typed
/// accessors to read. Serialisation is handled by serde (Serialize/Deserialize).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffEntry {
    /// Agent id that transferred the task.
    from_agent: u64,
    /// Agent id that received the task.
    to_agent: u64,
    /// Application-level task identifier. Free-form string; may be an agent
    /// id, a ticket id, or any other discriminator the caller finds useful.
    task_id: String,
    /// Brief summary of what the from-agent accomplished.
    summary: String,
    /// Descriptions of already-tried, already-failed approaches.
    failed_attempts: Vec<String>,
    /// Memory-block identifiers the receiving agent should consult.
    memory_refs: Vec<String>,
    /// Optional causality token linking this handoff to a pipeline run.
    correlation_id: Option<String>,
    /// Wall-clock time in milliseconds since the Unix epoch.
    ts_unix_ms: i64,
}

impl HandoffEntry {
    /// Construct a `HandoffEntry`.
    ///
    /// `ts_unix_ms` is accepted from the caller so tests can supply
    /// deterministic timestamps. Use [`new_entry`] as a convenience wrapper
    /// that stamps the current wall clock.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        from_agent: u64,
        to_agent: u64,
        task_id: impl Into<String>,
        summary: impl Into<String>,
        failed_attempts: Vec<String>,
        memory_refs: Vec<String>,
        correlation_id: Option<String>,
        ts_unix_ms: i64,
    ) -> Self {
        Self {
            from_agent,
            to_agent,
            task_id: task_id.into(),
            summary: summary.into(),
            failed_attempts,
            memory_refs,
            correlation_id,
            ts_unix_ms,
        }
    }

    /// The agent that transferred the task.
    pub fn from_agent(&self) -> u64 {
        self.from_agent
    }
    /// The agent that received the task.
    pub fn to_agent(&self) -> u64 {
        self.to_agent
    }
    /// Application-level task identifier.
    pub fn task_id(&self) -> &str {
        &self.task_id
    }
    /// Brief summary of what the from-agent accomplished.
    pub fn summary(&self) -> &str {
        &self.summary
    }
    /// Descriptions of already-tried, already-failed approaches.
    pub fn failed_attempts(&self) -> &[String] {
        &self.failed_attempts
    }
    /// Memory-block identifiers the receiving agent should consult.
    pub fn memory_refs(&self) -> &[String] {
        &self.memory_refs
    }
    /// Optional causality token linking this handoff to a pipeline run.
    pub fn correlation_id(&self) -> Option<&str> {
        self.correlation_id.as_deref()
    }
    /// Wall-clock time in milliseconds since the Unix epoch.
    pub fn ts_unix_ms(&self) -> i64 {
        self.ts_unix_ms
    }
}

// ---------------------------------------------------------------------------
// HandoffError
// ---------------------------------------------------------------------------

/// Errors produced by [`HandoffLog`].
#[derive(Debug, Error)]
pub enum HandoffError {
    /// A filesystem or I/O error from the backing [`EventLog`].
    #[error("handoff log I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// A JSON payload could not be serialised.
    #[error("handoff log serialization error: {0}")]
    Serialize(#[from] serde_json::Error),
}

// ---------------------------------------------------------------------------
// HandoffLog
// ---------------------------------------------------------------------------

/// Append-only, JSONL-backed log of agent handoffs.
///
/// Every call to [`record`](HandoffLog::record) appends one
/// [`HandoffEntry`] to the backing file and pushes it onto the in-memory
/// tail. [`recent`](HandoffLog::recent) and
/// [`by_task`](HandoffLog::by_task) read from the tail without
/// performing file I/O.
pub struct HandoffLog {
    log: EventLog,
    path: PathBuf,
}

/// Event kind string used when writing handoff envelopes to the backing log.
const KIND: &str = "agent.handoff";

impl HandoffLog {
    /// Open or create the log at `path`.
    ///
    /// The directory is created if it does not exist. Existing contents are
    /// preserved (append-only).
    pub fn open(path: &Path) -> Result<Self, HandoffError> {
        let log = EventLog::open(path)?;
        Ok(Self {
            log,
            path: path.to_path_buf(),
        })
    }

    /// Append a handoff entry to the log.
    ///
    /// The `ts_unix_ms` field on `entry` is used as-is; the backing
    /// [`EventLog`] stamps its own wall-clock timestamp on the outer
    /// envelope, but the inner payload preserves the caller's value for
    /// independent clock sources or test determinism.
    pub fn record(&mut self, entry: &HandoffEntry) -> Result<(), HandoffError> {
        let payload = serde_json::to_value(entry)?;
        self.log.append(
            EventSource::Agent {
                id: entry.from_agent,
            },
            KIND,
            payload,
        )?;
        Ok(())
    }

    /// Most-recent `n` handoff entries, in chronological order (oldest first).
    ///
    /// Reads from the in-memory tail; performs no file I/O.
    #[must_use]
    pub fn recent(&self, n: usize) -> Vec<HandoffEntry> {
        self.log
            .tail(n)
            .into_iter()
            .filter(|env| env.kind == KIND)
            .filter_map(|env| serde_json::from_value(env.payload).ok())
            .collect()
    }

    /// All in-memory handoff entries whose `task_id` matches `task_id`.
    ///
    /// Reads from the in-memory tail; performs no file I/O. The result is
    /// in chronological order (oldest first).
    #[must_use]
    pub fn by_task(&self, task_id: &str) -> Vec<HandoffEntry> {
        self.log
            .tail(usize::MAX)
            .into_iter()
            .filter(|env| env.kind == KIND)
            .filter_map(|env| serde_json::from_value::<HandoffEntry>(env.payload).ok())
            .filter(|e| e.task_id() == task_id)
            .collect()
    }

    /// Force a flush of the buffered writer.
    pub fn flush(&mut self) -> Result<(), HandoffError> {
        self.log.flush().map_err(HandoffError::Io)
    }

    /// Path of the backing JSONL file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn now_unix_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    dur.as_millis() as i64
}

/// Construct a [`HandoffEntry`] with `ts_unix_ms` set to now.
#[must_use]
pub fn new_entry(
    from_agent: u64,
    to_agent: u64,
    task_id: impl Into<String>,
    summary: impl Into<String>,
    failed_attempts: Vec<String>,
    memory_refs: Vec<String>,
    correlation_id: Option<String>,
) -> HandoffEntry {
    HandoffEntry::new(
        from_agent,
        to_agent,
        task_id,
        summary,
        failed_attempts,
        memory_refs,
        correlation_id,
        now_unix_ms(),
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    // ── helpers ──────────────────────────────────────────────────────────────

    fn mk_log(name: &str) -> (HandoffLog, PathBuf, TempDir) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(name);
        let log = HandoffLog::open(&path).unwrap();
        (log, path, dir)
    }

    fn simple_entry(from: u64, to: u64, task_id: &str) -> HandoffEntry {
        HandoffEntry::new(from, to, task_id, "phase 1 done", vec![], vec![], None, 0)
    }

    // ── record + retrieve ─────────────────────────────────────────────────────

    #[test]
    fn record_and_recent_round_trip() {
        let (mut log, _, _dir) = mk_log("basic.jsonl");

        let entry = simple_entry(1, 2, "task-001");
        log.record(&entry).unwrap();

        let tail = log.recent(10);
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].from_agent(), 1);
        assert_eq!(tail[0].to_agent(), 2);
        assert_eq!(tail[0].task_id(), "task-001");
        assert_eq!(tail[0].summary(), "phase 1 done");
    }

    #[test]
    fn recent_returns_last_n_chronological() {
        let (mut log, _, _dir) = mk_log("recent_n.jsonl");

        for i in 0..5u64 {
            let e = simple_entry(i, i + 1, &format!("task-{i:03}"));
            log.record(&e).unwrap();
        }

        let tail = log.recent(3);
        assert_eq!(tail.len(), 3);
        // Chronological order, oldest first.
        assert_eq!(tail[0].task_id(), "task-002");
        assert_eq!(tail[1].task_id(), "task-003");
        assert_eq!(tail[2].task_id(), "task-004");

        assert_eq!(log.recent(100).len(), 5);
        assert!(log.recent(0).is_empty());
    }

    // ── by_task filter ────────────────────────────────────────────────────────

    #[test]
    fn by_task_filters_by_task_id() {
        let (mut log, _, _dir) = mk_log("by_task.jsonl");

        log.record(&simple_entry(1, 2, "alpha")).unwrap();
        log.record(&simple_entry(2, 3, "beta")).unwrap();
        log.record(&simple_entry(3, 4, "alpha")).unwrap();
        log.record(&simple_entry(4, 5, "gamma")).unwrap();

        let alpha = log.by_task("alpha");
        assert_eq!(alpha.len(), 2, "two handoffs for 'alpha'");
        assert!(alpha.iter().all(|e| e.task_id() == "alpha"));

        let beta = log.by_task("beta");
        assert_eq!(beta.len(), 1);

        let missing = log.by_task("delta");
        assert!(missing.is_empty(), "unknown task must return empty vec");
    }

    // ── JSONL round-trip ──────────────────────────────────────────────────────

    #[test]
    fn jsonl_round_trip_survives_reopen() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("persist.jsonl");

        {
            let mut log = HandoffLog::open(&path).unwrap();
            log.record(&HandoffEntry::new(
                10,
                20,
                "t-persist",
                "wrote the thing",
                vec!["approach-A".into()],
                vec!["mem-1".into(), "mem-2".into()],
                Some("corr-xyz".into()),
                1_700_000_000_000,
            ))
            .unwrap();
            log.flush().unwrap();
        }

        // Re-read the JSONL file directly and verify the payload.
        let contents = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 1, "one line per record");

        let env: crate::event_log::EventEnvelope = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(env.kind, "agent.handoff");

        let entry: HandoffEntry = serde_json::from_value(env.payload).unwrap();
        assert_eq!(entry.from_agent(), 10);
        assert_eq!(entry.to_agent(), 20);
        assert_eq!(entry.task_id(), "t-persist");
        assert_eq!(entry.summary(), "wrote the thing");
        assert_eq!(entry.failed_attempts(), &["approach-A".to_string()]);
        assert_eq!(
            entry.memory_refs(),
            &["mem-1".to_string(), "mem-2".to_string()]
        );
        assert_eq!(entry.correlation_id(), Some("corr-xyz"));
        assert_eq!(entry.ts_unix_ms(), 1_700_000_000_000);
    }

    // ── correlation_id propagation ────────────────────────────────────────────

    #[test]
    fn record_with_correlation_id_preserved() {
        let (mut log, _, _dir) = mk_log("corr.jsonl");

        let entry = HandoffEntry::new(
            1,
            2,
            "t1",
            "done",
            vec![],
            vec![],
            Some("cid-abc-123".into()),
            0,
        );
        log.record(&entry).unwrap();

        let tail = log.recent(1);
        assert_eq!(tail[0].correlation_id(), Some("cid-abc-123"));
    }

    // ── empty failed_attempts ─────────────────────────────────────────────────

    #[test]
    fn record_with_empty_failed_attempts() {
        let (mut log, _, _dir) = mk_log("no_fails.jsonl");

        let entry = HandoffEntry::new(1, 2, "clean", "all good", vec![], vec![], None, 0);
        log.record(&entry).unwrap();

        let tail = log.recent(1);
        assert!(
            tail[0].failed_attempts().is_empty(),
            "empty failed_attempts must round-trip as empty",
        );
    }

    // ── multi-agent chain ─────────────────────────────────────────────────────

    #[test]
    fn multi_agent_chain_all_under_same_task_id() {
        let (mut log, _, _dir) = mk_log("chain.jsonl");

        // Orchestrator → Specialist A → Specialist B, same pipeline.
        let cid = "corr-pipeline-42".to_string();

        log.record(&HandoffEntry::new(
            1,
            2,
            "pipeline-42",
            "scoped requirements",
            vec![],
            vec!["mem-req".into()],
            Some(cid.clone()),
            1_000,
        ))
        .unwrap();

        log.record(&HandoffEntry::new(
            2,
            3,
            "pipeline-42",
            "implemented core logic",
            vec!["mutex deadlock on naive impl".into()],
            vec!["mem-req".into(), "mem-design".into()],
            Some(cid.clone()),
            2_000,
        ))
        .unwrap();

        let chain = log.by_task("pipeline-42");
        assert_eq!(
            chain.len(),
            2,
            "both hops must appear under the same task id"
        );

        // Verify the chain is in chronological order.
        assert_eq!(chain[0].from_agent(), 1);
        assert_eq!(chain[1].from_agent(), 2);

        // Every hop in the chain shares the correlation id.
        assert!(
            chain
                .iter()
                .all(|e| e.correlation_id() == Some("corr-pipeline-42"))
        );
    }
}
