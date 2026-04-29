//! Typed agent-lifecycle journal backed by [`EventLog`].
//!
//! Every significant event produced by an agent — spawn, tool call, output
//! line, completion, or unexpected death ("flatline") — is appended here as a
//! [`JournalEntry`].  Each entry carries a monotonically-increasing `sequence`
//! number (the global sequence clock, Pattern 16), a `phase` tag
//! (`Planning | Execution | Completion | Lifecycle`), and a `level` tag
//! (`Debug | Info | Warn | Error`).
//!
//! Under the hood every `JournalEntry` is stored as one line in the JSONL
//! [`EventLog`], so the file is the durable record and the in-memory tail
//! drives fast reads.
//!
//! # Example
//!
//! ```rust,no_run
//! use phantom_memory::journal::{AgentJournal, Level, Phase};
//! use std::path::Path;
//!
//! let mut journal = AgentJournal::open(Path::new("/tmp/agent.jsonl")).unwrap();
//! journal.record_spawn(1, "fix failing tests").unwrap();
//! journal.record_tool_call(1, "ReadFile", "src/main.rs").unwrap();
//! journal.record_completion(1, true, "all tests pass").unwrap();
//! ```

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::broadcast;

use crate::event_log::{EventEnvelope, EventLog, EventSource};

/// Global sequence clock, shared across all journal instances in a process.
///
/// Sequence numbers are monotonically increasing across *all* agents so that a
/// merge of per-agent journals retains a total ordering of events.
static GLOBAL_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// Runtime identifier for an agent.  Matches the `u64` id used inside
/// [`EventSource::Agent`] so entries can be correlated with the raw event log.
pub type AgentId = u64;

/// High-level phase of the agent state machine at the time of the event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// The agent is formulating a plan or interpreting its task.
    Planning,
    /// The agent is executing its plan (running tools, producing output).
    Execution,
    /// The agent has finished, either successfully or with an error.
    Completion,
    /// Administrative lifecycle event (spawn, flatline, quarantine, etc.).
    Lifecycle,
}

impl Phase {
    /// Dotted-path prefix used in the backing [`EventLog`] kind strings.
    fn kind_prefix(self) -> &'static str {
        match self {
            Phase::Planning => "agent.planning",
            Phase::Execution => "agent.execution",
            Phase::Completion => "agent.completion",
            Phase::Lifecycle => "agent.lifecycle",
        }
    }
}

impl std::fmt::Display for Phase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Phase::Planning => "planning",
            Phase::Execution => "execution",
            Phase::Completion => "completion",
            Phase::Lifecycle => "lifecycle",
        };
        write!(f, "{s}")
    }
}

/// Severity level of a journal entry, analogous to a log level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    /// Verbose diagnostic information.
    Debug,
    /// Normal operational information.
    Info,
    /// Something unexpected but recoverable.
    Warn,
    /// An error that may impair the agent.
    Error,
}

impl std::fmt::Display for Level {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Level::Debug => "debug",
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
        };
        write!(f, "{s}")
    }
}

/// A single agent lifecycle event.
///
/// All fields are private; use the constructor [`JournalEntry::new`] or the
/// typed helpers on [`AgentJournal`], then access data through getters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntry {
    agent_id: AgentId,
    sequence: u64,
    /// Wall-clock time in milliseconds since the Unix epoch.
    ts_unix_ms: i64,
    phase: Phase,
    level: Level,
    message: String,
}

impl JournalEntry {
    /// Construct a `JournalEntry` directly.
    ///
    /// `sequence` must come from the caller; use
    /// [`AgentJournal::next_sequence`] or [`next_global_sequence`] to
    /// obtain a monotonic value.
    pub fn new(
        agent_id: AgentId,
        sequence: u64,
        ts_unix_ms: i64,
        phase: Phase,
        level: Level,
        message: impl Into<String>,
    ) -> Self {
        Self {
            agent_id,
            sequence,
            ts_unix_ms,
            phase,
            level,
            message: message.into(),
        }
    }

    /// The agent that produced this entry.
    pub fn agent_id(&self) -> AgentId {
        self.agent_id
    }

    /// Monotonically increasing global sequence number.
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Wall-clock timestamp in milliseconds since the Unix epoch.
    pub fn ts_unix_ms(&self) -> i64 {
        self.ts_unix_ms
    }

    /// State-machine phase at the time of the event.
    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// Severity level.
    pub fn level(&self) -> Level {
        self.level
    }

    /// Human-readable description of the event.
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// Errors produced by [`AgentJournal`].
#[derive(Debug, Error)]
pub enum JournalError {
    /// A filesystem or serialization error from the backing [`EventLog`].
    #[error("event log I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// A JSON payload for a [`JournalEntry`] could not be serialized.
    #[error("serialization error: {0}")]
    Serialize(#[from] serde_json::Error),
}

/// Typed agent-lifecycle journal.
///
/// Wraps an [`EventLog`] and exposes a higher-level API for appending
/// structured [`JournalEntry`] events.  The in-memory tail supports fast reads
/// (recent N events, filter by phase, filter by agent); time-range queries
/// perform a single sequential scan of the in-memory tail.
pub struct AgentJournal {
    log: EventLog,
}

/// Consume the next value from the process-global sequence clock.
///
/// Exposed for callers that construct [`JournalEntry`] directly.
pub fn next_global_sequence() -> u64 {
    GLOBAL_SEQUENCE.fetch_add(1, Ordering::Relaxed)
}

impl AgentJournal {
    /// Open or create the journal at `path`.
    ///
    /// Uses the global sequence clock — sequence numbers are unique across all
    /// `AgentJournal` instances in the process.
    pub fn open(path: &Path) -> Result<Self, JournalError> {
        let log = EventLog::open(path)?;
        Ok(Self { log })
    }

    /// The next sequence number that will be assigned.
    pub fn next_sequence(&self) -> u64 {
        GLOBAL_SEQUENCE.load(Ordering::Relaxed)
    }

    /// Append a fully-constructed [`JournalEntry`].
    ///
    /// This is the lowest-level append; prefer the typed helpers when possible.
    pub fn append(&mut self, entry: JournalEntry) -> Result<JournalEntry, JournalError> {
        let kind = format!("{}.{}", entry.phase.kind_prefix(), entry.level);
        let payload = serde_json::to_value(&entry)?;
        self.log
            .append(EventSource::Agent { id: entry.agent_id }, kind, payload)?;
        Ok(entry)
    }

    /// Build and append a journal entry, automatically assigning a sequence
    /// number and timestamp.
    pub fn record(
        &mut self,
        agent_id: AgentId,
        phase: Phase,
        level: Level,
        message: impl Into<String>,
    ) -> Result<JournalEntry, JournalError> {
        let seq = next_global_sequence();
        let ts = now_unix_ms();
        let entry = JournalEntry::new(agent_id, seq, ts, phase, level, message);
        self.append(entry)
    }

    // ── Typed lifecycle helpers ────────────────────────────────────────────

    /// Record an agent spawn event.
    pub fn record_spawn(
        &mut self,
        agent_id: AgentId,
        task: impl Into<String>,
    ) -> Result<JournalEntry, JournalError> {
        self.record(
            agent_id,
            Phase::Lifecycle,
            Level::Info,
            format!("spawn: {}", task.into()),
        )
    }

    /// Record a tool invocation.
    pub fn record_tool_call(
        &mut self,
        agent_id: AgentId,
        tool: impl Into<String>,
        args: impl Into<String>,
    ) -> Result<JournalEntry, JournalError> {
        self.record(
            agent_id,
            Phase::Execution,
            Level::Info,
            format!("tool_call: {} args={}", tool.into(), args.into()),
        )
    }

    /// Record a line of agent output.
    pub fn record_output(
        &mut self,
        agent_id: AgentId,
        line: impl Into<String>,
    ) -> Result<JournalEntry, JournalError> {
        self.record(agent_id, Phase::Execution, Level::Debug, line)
    }

    /// Record agent completion.
    ///
    /// `success` is `true` when the agent completed its task normally; `false`
    /// when it terminated with an error or was cancelled.
    pub fn record_completion(
        &mut self,
        agent_id: AgentId,
        success: bool,
        summary: impl Into<String>,
    ) -> Result<JournalEntry, JournalError> {
        let level = if success { Level::Info } else { Level::Error };
        self.record(
            agent_id,
            Phase::Completion,
            level,
            format!(
                "completion: {} summary={}",
                if success { "ok" } else { "err" },
                summary.into()
            ),
        )
    }

    /// Record an unexpected agent death (flatline — no clean completion).
    pub fn record_flatline(
        &mut self,
        agent_id: AgentId,
        reason: impl Into<String>,
    ) -> Result<JournalEntry, JournalError> {
        self.record(
            agent_id,
            Phase::Lifecycle,
            Level::Error,
            format!("flatline: {}", reason.into()),
        )
    }

    /// Record the fully-assembled system prompt that was sent to the agent.
    ///
    /// Called once per agent run, immediately before the first LLM request.
    /// The `prompt_text` is stored verbatim so any run can be replayed exactly
    /// by feeding the same prompt back to the model.
    pub fn record_prompt_snapshot(
        &mut self,
        agent_id: AgentId,
        prompt_text: impl Into<String>,
    ) -> Result<JournalEntry, JournalError> {
        self.record(
            agent_id,
            Phase::Planning,
            Level::Debug,
            format!("prompt_snapshot: {}", prompt_text.into()),
        )
    }

    // ── Query API ─────────────────────────────────────────────────────────

    /// Most-recent `n` journal entries, in chronological order (oldest first).
    ///
    /// Reads from the in-memory tail; performs no file I/O.
    pub fn tail(&self, n: usize) -> Vec<JournalEntry> {
        self.log
            .tail(n)
            .into_iter()
            .filter_map(|env| entry_from_envelope(&env))
            .collect()
    }

    /// All in-memory entries for a specific `phase`.
    pub fn filter_by_phase(&self, phase: Phase) -> Vec<JournalEntry> {
        self.log
            .tail(usize::MAX)
            .into_iter()
            .filter_map(|env| entry_from_envelope(&env))
            .filter(|e| e.phase == phase)
            .collect()
    }

    /// All in-memory entries produced by `agent_id`.
    pub fn filter_by_agent(&self, agent_id: AgentId) -> Vec<JournalEntry> {
        self.log
            .tail(usize::MAX)
            .into_iter()
            .filter_map(|env| entry_from_envelope(&env))
            .filter(|e| e.agent_id == agent_id)
            .collect()
    }

    /// All in-memory entries produced by `agent_id` in the given `phase`.
    pub fn filter_by_agent_and_phase(&self, agent_id: AgentId, phase: Phase) -> Vec<JournalEntry> {
        self.log
            .tail(usize::MAX)
            .into_iter()
            .filter_map(|env| entry_from_envelope(&env))
            .filter(|e| e.agent_id == agent_id && e.phase == phase)
            .collect()
    }

    /// All in-memory entries with `level == level`.
    pub fn filter_by_level(&self, level: Level) -> Vec<JournalEntry> {
        self.log
            .tail(usize::MAX)
            .into_iter()
            .filter_map(|env| entry_from_envelope(&env))
            .filter(|e| e.level == level)
            .collect()
    }

    /// All in-memory entries whose timestamp falls within `[from_ms, to_ms]`
    /// (inclusive, milliseconds since Unix epoch).
    pub fn query_range(&self, from_ms: i64, to_ms: i64) -> Vec<JournalEntry> {
        self.log
            .tail(usize::MAX)
            .into_iter()
            .filter_map(|env| entry_from_envelope(&env))
            .filter(|e| e.ts_unix_ms >= from_ms && e.ts_unix_ms <= to_ms)
            .collect()
    }

    /// Subscribe to the live broadcast channel.
    ///
    /// Only entries appended *after* the subscription point are delivered.  The
    /// channel is bounded and lossy — the file is the durable record.
    pub fn subscribe(&self) -> broadcast::Receiver<EventEnvelope> {
        self.log.subscribe()
    }

    /// Force a flush of the buffered writer.
    pub fn flush(&mut self) -> Result<(), JournalError> {
        self.log.flush().map_err(JournalError::Io)
    }

    /// Path of the backing JSONL file.
    pub fn path(&self) -> &Path {
        self.log.path()
    }
}

// ── Internal helpers ──────────────────────────────────────────────────────────

fn now_unix_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    dur.as_millis() as i64
}

/// Attempt to deserialize a [`JournalEntry`] from a raw [`EventEnvelope`]
/// payload.  Returns `None` for envelopes that were not written by
/// `AgentJournal` (e.g. raw `EventLog` appends from other subsystems).
fn entry_from_envelope(env: &EventEnvelope) -> Option<JournalEntry> {
    serde_json::from_value(env.payload.clone()).ok()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    // ── helpers ──────────────────────────────────────────────────────────────

    fn mk_journal(name: &str) -> (AgentJournal, PathBuf, TempDir) {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join(name);
        let journal = AgentJournal::open(&path).expect("open");
        (journal, path, dir)
    }

    // ── append / tail basics ─────────────────────────────────────────────────

    #[test]
    fn record_appends_and_tail_returns_in_order() {
        let (mut j, _, _dir) = mk_journal("basic.jsonl");

        j.record(1, Phase::Lifecycle, Level::Info, "first").unwrap();
        j.record(2, Phase::Execution, Level::Debug, "second")
            .unwrap();
        j.record(1, Phase::Completion, Level::Info, "third")
            .unwrap();

        let tail = j.tail(10);
        assert_eq!(tail.len(), 3);
        assert_eq!(tail[0].message(), "first");
        assert_eq!(tail[1].message(), "second");
        assert_eq!(tail[2].message(), "third");
    }

    #[test]
    fn tail_n_returns_last_n_chronological() {
        let (mut j, _, _dir) = mk_journal("tail_n.jsonl");

        for i in 0..10u64 {
            j.record(1, Phase::Execution, Level::Info, format!("msg-{i}"))
                .unwrap();
        }

        let tail = j.tail(3);
        assert_eq!(tail.len(), 3);
        assert_eq!(tail[0].message(), "msg-7");
        assert_eq!(tail[1].message(), "msg-8");
        assert_eq!(tail[2].message(), "msg-9");

        // Asking for more than we have returns everything.
        assert_eq!(j.tail(100).len(), 10);

        // Zero returns nothing.
        assert!(j.tail(0).is_empty());
    }

    #[test]
    fn journal_entry_fields_accessible_via_getters() {
        let (mut j, _, _dir) = mk_journal("getters.jsonl");

        let entry = j
            .record(42, Phase::Planning, Level::Warn, "something unexpected")
            .unwrap();

        assert_eq!(entry.agent_id(), 42);
        assert_eq!(entry.phase(), Phase::Planning);
        assert_eq!(entry.level(), Level::Warn);
        assert_eq!(entry.message(), "something unexpected");
        // sequence assigned from global clock — must be ≥ 1
        assert!(entry.sequence() >= 1);
        // timestamp is non-zero
        assert!(entry.ts_unix_ms() > 0);
    }

    // ── typed lifecycle helpers ───────────────────────────────────────────────

    #[test]
    fn record_spawn_lifecycle_info() {
        let (mut j, _, _dir) = mk_journal("spawn.jsonl");
        let e = j.record_spawn(5, "fix failing tests").unwrap();

        assert_eq!(e.agent_id(), 5);
        assert_eq!(e.phase(), Phase::Lifecycle);
        assert_eq!(e.level(), Level::Info);
        assert!(e.message().contains("spawn"));
        assert!(e.message().contains("fix failing tests"));
    }

    #[test]
    fn record_tool_call_execution_info() {
        let (mut j, _, _dir) = mk_journal("tool.jsonl");
        let e = j.record_tool_call(3, "ReadFile", "src/main.rs").unwrap();

        assert_eq!(e.phase(), Phase::Execution);
        assert_eq!(e.level(), Level::Info);
        assert!(e.message().contains("tool_call"));
        assert!(e.message().contains("ReadFile"));
        assert!(e.message().contains("src/main.rs"));
    }

    #[test]
    fn record_output_execution_debug() {
        let (mut j, _, _dir) = mk_journal("output.jsonl");
        let e = j.record_output(7, "compiling...").unwrap();

        assert_eq!(e.phase(), Phase::Execution);
        assert_eq!(e.level(), Level::Debug);
    }

    #[test]
    fn record_completion_success_and_failure() {
        let (mut j, _, _dir) = mk_journal("completion.jsonl");

        let ok = j.record_completion(1, true, "all tests pass").unwrap();
        assert_eq!(ok.phase(), Phase::Completion);
        assert_eq!(ok.level(), Level::Info);
        assert!(ok.message().contains("ok"));

        let err = j.record_completion(2, false, "timeout").unwrap();
        assert_eq!(err.phase(), Phase::Completion);
        assert_eq!(err.level(), Level::Error);
        assert!(err.message().contains("err"));
    }

    #[test]
    fn record_flatline_lifecycle_error() {
        let (mut j, _, _dir) = mk_journal("flatline.jsonl");
        let e = j.record_flatline(9, "killed by OOM").unwrap();

        assert_eq!(e.phase(), Phase::Lifecycle);
        assert_eq!(e.level(), Level::Error);
        assert!(e.message().contains("flatline"));
        assert!(e.message().contains("killed by OOM"));
    }

    // ── filter by phase ───────────────────────────────────────────────────────

    #[test]
    fn filter_by_phase_returns_only_matching() {
        let (mut j, _, _dir) = mk_journal("phase_filter.jsonl");

        j.record_spawn(1, "task-a").unwrap();
        j.record_tool_call(1, "ReadFile", "f.rs").unwrap();
        j.record_output(1, "line 1").unwrap();
        j.record_completion(1, true, "done").unwrap();
        j.record_flatline(2, "oom").unwrap();

        let lifecycle = j.filter_by_phase(Phase::Lifecycle);
        assert_eq!(lifecycle.len(), 2, "spawn + flatline");
        assert!(lifecycle.iter().all(|e| e.phase() == Phase::Lifecycle));

        let execution = j.filter_by_phase(Phase::Execution);
        assert_eq!(execution.len(), 2, "tool_call + output");

        let completion = j.filter_by_phase(Phase::Completion);
        assert_eq!(completion.len(), 1);

        let planning = j.filter_by_phase(Phase::Planning);
        assert!(planning.is_empty());
    }

    // ── filter by agent ───────────────────────────────────────────────────────

    #[test]
    fn filter_by_agent_isolates_per_agent() {
        let (mut j, _, _dir) = mk_journal("agent_filter.jsonl");

        j.record_spawn(1, "task-a").unwrap();
        j.record_spawn(2, "task-b").unwrap();
        j.record_tool_call(1, "WriteFile", "out.txt").unwrap();
        j.record_completion(2, false, "cancelled").unwrap();

        let agent1 = j.filter_by_agent(1);
        assert_eq!(agent1.len(), 2);
        assert!(agent1.iter().all(|e| e.agent_id() == 1));

        let agent2 = j.filter_by_agent(2);
        assert_eq!(agent2.len(), 2);
        assert!(agent2.iter().all(|e| e.agent_id() == 2));

        let agent3 = j.filter_by_agent(3);
        assert!(agent3.is_empty());
    }

    #[test]
    fn filter_by_agent_and_phase_intersection() {
        let (mut j, _, _dir) = mk_journal("agent_phase.jsonl");

        j.record_spawn(1, "task").unwrap();
        j.record_tool_call(1, "RunCommand", "ls").unwrap();
        j.record_tool_call(2, "RunCommand", "pwd").unwrap();
        j.record_completion(1, true, "done").unwrap();

        let result = j.filter_by_agent_and_phase(1, Phase::Execution);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].agent_id(), 1);
        assert_eq!(result[0].phase(), Phase::Execution);

        let empty = j.filter_by_agent_and_phase(1, Phase::Planning);
        assert!(empty.is_empty());
    }

    // ── filter by level ───────────────────────────────────────────────────────

    #[test]
    fn filter_by_level_returns_matching() {
        let (mut j, _, _dir) = mk_journal("level_filter.jsonl");

        j.record(1, Phase::Execution, Level::Debug, "verbose")
            .unwrap();
        j.record(1, Phase::Execution, Level::Info, "normal")
            .unwrap();
        j.record(1, Phase::Lifecycle, Level::Error, "dead").unwrap();
        j.record(2, Phase::Execution, Level::Warn, "odd").unwrap();
        j.record(2, Phase::Completion, Level::Info, "ok").unwrap();

        let errors = j.filter_by_level(Level::Error);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].message(), "dead");

        let infos = j.filter_by_level(Level::Info);
        assert_eq!(infos.len(), 2);

        let warnings = j.filter_by_level(Level::Warn);
        assert_eq!(warnings.len(), 1);

        let debugs = j.filter_by_level(Level::Debug);
        assert_eq!(debugs.len(), 1);
    }

    // ── time-range query ──────────────────────────────────────────────────────

    #[test]
    fn query_range_filters_by_timestamp() {
        let (mut j, _, _dir) = mk_journal("range.jsonl");

        // Manually craft entries at known timestamps.
        let make = |agent_id: u64, ts: i64, msg: &str| {
            JournalEntry::new(
                agent_id,
                next_global_sequence(),
                ts,
                Phase::Execution,
                Level::Info,
                msg,
            )
        };

        j.append(make(1, 1000, "before")).unwrap();
        j.append(make(1, 2000, "start")).unwrap();
        j.append(make(1, 3000, "middle")).unwrap();
        j.append(make(1, 4000, "end")).unwrap();
        j.append(make(1, 5000, "after")).unwrap();

        let range = j.query_range(2000, 4000);
        assert_eq!(range.len(), 3);
        assert_eq!(range[0].message(), "start");
        assert_eq!(range[1].message(), "middle");
        assert_eq!(range[2].message(), "end");

        // Exclusive outer edges.
        let exact = j.query_range(3000, 3000);
        assert_eq!(exact.len(), 1);
        assert_eq!(exact[0].message(), "middle");

        // Empty range.
        let empty = j.query_range(6000, 9000);
        assert!(empty.is_empty());
    }

    // ── sequence monotonicity ─────────────────────────────────────────────────

    #[test]
    fn sequence_numbers_are_strictly_increasing() {
        let (mut j, _, _dir) = mk_journal("seq.jsonl");

        let entries: Vec<_> = (0..10)
            .map(|_| j.record(1, Phase::Execution, Level::Info, "x").unwrap())
            .collect();

        let seqs: Vec<u64> = entries.iter().map(|e| e.sequence()).collect();
        // All sequences must be strictly increasing.
        for pair in seqs.windows(2) {
            assert!(
                pair[1] > pair[0],
                "sequence must be strictly increasing: {pair:?}"
            );
        }
    }

    // ── persistence ───────────────────────────────────────────────────────────

    #[test]
    fn journal_survives_reopen() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("persist.jsonl");

        {
            let mut j = AgentJournal::open(&path).unwrap();
            j.record_spawn(1, "reopen test").unwrap();
            j.record_tool_call(1, "ReadFile", "Cargo.toml").unwrap();
            j.flush().unwrap();
        }

        // Reopen and check that the backing log survived.
        {
            let _j = AgentJournal::open(&path).unwrap();
            // The in-memory tail of a freshly opened log is empty (it reads
            // the sequence id from disk but doesn't hydrate the tail).
            // However we can verify the JSONL file directly.
            let contents = std::fs::read_to_string(&path).unwrap();
            let lines: Vec<&str> = contents.lines().collect();
            assert_eq!(lines.len(), 2, "two events must be on disk");
            // Each line must be valid JSON with a JournalEntry payload.
            for line in &lines {
                let env: crate::event_log::EventEnvelope =
                    serde_json::from_str(line).expect("line not valid JSON");
                let entry: JournalEntry =
                    serde_json::from_value(env.payload).expect("payload not a JournalEntry");
                assert_eq!(entry.agent_id(), 1);
            }
        }
    }

    // ── JSONL file format ─────────────────────────────────────────────────────

    #[test]
    fn each_file_line_is_valid_journal_entry_json() {
        let (mut j, path, _dir) = mk_journal("format.jsonl");

        j.record_spawn(1, "task").unwrap();
        j.record_tool_call(1, "RunCommand", "cargo test").unwrap();
        j.record_output(1, "running 42 tests").unwrap();
        j.record_completion(1, true, "all passed").unwrap();
        j.flush().unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 4);

        for line in lines {
            let env: crate::event_log::EventEnvelope =
                serde_json::from_str(line).expect("not valid JSON");
            let _entry: JournalEntry =
                serde_json::from_value(env.payload).expect("payload is not a JournalEntry");
        }
    }

    // ── subscribe ─────────────────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn subscribe_receives_live_appends() {
        use std::time::Duration;

        let (mut j, _, _dir) = mk_journal("subscribe.jsonl");
        let mut rx = j.subscribe();

        j.record_spawn(1, "task").unwrap();
        j.record_completion(1, true, "done").unwrap();

        let e1 = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("recv timed out")
            .expect("channel closed");
        let e2 = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();

        // Verify the raw envelopes contain JournalEntry payloads.
        let j1: JournalEntry = serde_json::from_value(e1.payload).unwrap();
        let j2: JournalEntry = serde_json::from_value(e2.payload).unwrap();

        assert_eq!(j1.phase(), Phase::Lifecycle);
        assert_eq!(j2.phase(), Phase::Completion);
    }

    // ── append with pre-built JournalEntry ───────────────────────────────────

    #[test]
    fn append_pre_built_entry_preserves_all_fields() {
        let (mut j, _, _dir) = mk_journal("prebuilt.jsonl");

        let ts = 1_700_000_000_000i64;
        let seq = next_global_sequence();
        let entry = JournalEntry::new(99, seq, ts, Phase::Planning, Level::Warn, "custom");
        let returned = j.append(entry).unwrap();

        assert_eq!(returned.agent_id(), 99);
        assert_eq!(returned.ts_unix_ms(), ts);
        assert_eq!(returned.phase(), Phase::Planning);
        assert_eq!(returned.level(), Level::Warn);
        assert_eq!(returned.message(), "custom");
        assert_eq!(returned.sequence(), seq);
    }

    // ── path accessor ─────────────────────────────────────────────────────────

    #[test]
    fn path_returns_backing_file_path() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("path_test.jsonl");
        let j = AgentJournal::open(&path).unwrap();
        assert_eq!(j.path(), path.as_path());
    }

    // ── prompt_snapshot (#42) ─────────────────────────────────────────────────

    #[test]
    fn record_prompt_snapshot_planning_debug() {
        let (mut j, _, _dir) = mk_journal("prompt_snap.jsonl");
        let e = j
            .record_prompt_snapshot(7, "You are an AI assistant.\n\n## Task\ndo the thing")
            .unwrap();

        assert_eq!(e.agent_id(), 7);
        assert_eq!(e.phase(), Phase::Planning);
        assert_eq!(e.level(), Level::Debug);
        assert!(
            e.message().starts_with("prompt_snapshot:"),
            "message must have the prompt_snapshot prefix; got: {}",
            e.message(),
        );
        assert!(
            e.message().contains("do the thing"),
            "message must include prompt content; got: {}",
            e.message(),
        );
    }

    #[test]
    fn record_prompt_snapshot_appears_in_filter_by_phase_planning() {
        let (mut j, _, _dir) = mk_journal("snap_phase.jsonl");
        j.record_spawn(1, "task").unwrap();
        j.record_prompt_snapshot(1, "system prompt text").unwrap();
        j.record_tool_call(1, "ReadFile", "src/main.rs").unwrap();

        let planning = j.filter_by_phase(Phase::Planning);
        assert_eq!(planning.len(), 1, "only the prompt_snapshot should be in Planning");
        assert!(planning[0].message().contains("system prompt text"));
    }
}
