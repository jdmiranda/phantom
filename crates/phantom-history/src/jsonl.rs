//! Core `HistoryEntry` type — one per command execution, serialised as a JSONL line.

use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use phantom_semantic::{CommandType, SemanticParser};

// ---------------------------------------------------------------------------
// Schema versioning
// ---------------------------------------------------------------------------

/// Current schema version written into every new entry.
///
/// Increment this constant whenever the `HistoryEntry` schema changes in a
/// backward-incompatible way, and add a migration arm in
/// [`HistoryEntry::from_jsonl_line`].
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

pub(crate) fn schema_v1() -> u32 {
    1
}

// ---------------------------------------------------------------------------
// HistoryEntry
// ---------------------------------------------------------------------------

/// A single recorded command execution.
///
/// All fields are private; use [`HistoryEntry::builder`] to construct and the
/// provided getters to read.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// Schema version — defaults to 1 when deserialising pre-versioned records.
    ///
    /// Forward compatibility: entries with a version higher than
    /// [`CURRENT_SCHEMA_VERSION`] are rejected by [`HistoryEntry::from_jsonl_line`].
    #[serde(default = "schema_v1")]
    pub schema_version: u32,
    id: Uuid,
    /// ISO-8601 timestamp of when the command was submitted.
    timestamp: DateTime<Utc>,
    command: String,
    exit_code: Option<i32>,
    duration_ms: Option<u64>,
    cwd: PathBuf,
    session_id: Uuid,
    /// Semantic classification of the command (from phantom-semantic).
    semantic_type: CommandType,
    /// Optional agent that generated or requested this command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent_id: Option<String>,
}

impl HistoryEntry {
    /// Return a builder to construct an entry.
    #[must_use]
    pub fn builder(
        command: impl Into<String>,
        cwd: impl Into<PathBuf>,
        session_id: Uuid,
    ) -> HistoryEntryBuilder {
        HistoryEntryBuilder::new(command.into(), cwd.into(), session_id)
    }

    // -----------------------------------------------------------------------
    // Getters
    // -----------------------------------------------------------------------

    /// Unique entry identifier.
    #[must_use]
    pub fn id(&self) -> Uuid { self.id }

    /// Submission timestamp (UTC).
    #[must_use]
    pub fn timestamp(&self) -> DateTime<Utc> { self.timestamp }

    /// The command string as typed by the user.
    #[must_use]
    pub fn command(&self) -> &str { &self.command }

    /// Process exit code, if the command completed.
    #[must_use]
    pub fn exit_code(&self) -> Option<i32> { self.exit_code }

    /// Wall-clock duration in milliseconds, if measured.
    #[must_use]
    pub fn duration_ms(&self) -> Option<u64> { self.duration_ms }

    /// Working directory in which the command was executed.
    #[must_use]
    pub fn cwd(&self) -> &PathBuf { &self.cwd }

    /// Session that owns this entry.
    #[must_use]
    pub fn session_id(&self) -> Uuid { self.session_id }

    /// Semantic classification (git/cargo/shell/…).
    #[must_use]
    pub fn semantic_type(&self) -> &CommandType { &self.semantic_type }

    /// Agent that generated or requested this command, if any.
    #[must_use]
    pub fn agent_id(&self) -> Option<&str> { self.agent_id.as_deref() }

    // -----------------------------------------------------------------------
    // (De)serialisation helpers
    // -----------------------------------------------------------------------

    /// Serialise to a compact JSON string suitable for a JSONL line.
    pub fn to_jsonl_line(&self) -> Result<String> {
        serde_json::to_string(self).context("failed to serialise HistoryEntry")
    }

    /// Deserialise from a single JSONL line.
    ///
    /// Returns an error if the record's `schema_version` is greater than
    /// [`CURRENT_SCHEMA_VERSION`] (forward-compat guard).  Records with a
    /// version below the current one are accepted as-is — the missing field
    /// defaults applied by serde are sufficient for the v0 → v1 migration.
    pub fn from_jsonl_line(line: &str) -> Result<Self> {
        let entry: Self =
            serde_json::from_str(line).context("failed to deserialise HistoryEntry")?;
        if entry.schema_version > CURRENT_SCHEMA_VERSION {
            anyhow::bail!(
                "unsupported schema_version {} (max supported: {})",
                entry.schema_version,
                CURRENT_SCHEMA_VERSION,
            );
        }
        Ok(entry)
    }
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

/// Fluent builder for [`HistoryEntry`].
pub struct HistoryEntryBuilder {
    command: String,
    cwd: PathBuf,
    session_id: Uuid,
    id: Uuid,
    timestamp: DateTime<Utc>,
    exit_code: Option<i32>,
    duration_ms: Option<u64>,
    semantic_type: CommandType,
    agent_id: Option<String>,
}

impl HistoryEntryBuilder {
    fn new(command: String, cwd: PathBuf, session_id: Uuid) -> Self {
        let semantic_type = SemanticParser::classify_command(&command);
        Self {
            command,
            cwd,
            session_id,
            id: Uuid::new_v4(),
            timestamp: Utc::now(),
            exit_code: None,
            duration_ms: None,
            semantic_type,
            agent_id: None,
        }
    }


    /// Override the auto-generated UUID (useful in tests).
    #[must_use]
    pub fn id(mut self, id: Uuid) -> Self { self.id = id; self }

    /// Override the auto-generated timestamp (useful in tests).
    #[must_use]
    pub fn timestamp(mut self, ts: DateTime<Utc>) -> Self { self.timestamp = ts; self }

    /// Set the exit code.
    #[must_use]
    pub fn exit_code(mut self, code: i32) -> Self { self.exit_code = Some(code); self }

    /// Set the wall-clock duration.
    #[must_use]
    pub fn duration_ms(mut self, ms: u64) -> Self { self.duration_ms = Some(ms); self }

    /// Set the semantic classification.
    #[must_use]
    pub fn semantic_type(mut self, t: CommandType) -> Self { self.semantic_type = t; self }

    /// Tag this entry with the agent that produced it.
    #[must_use]
    pub fn agent_id(mut self, id: impl Into<String>) -> Self {
        self.agent_id = Some(id.into());
        self
    }

    /// Finalise and return the entry.
    #[must_use]
    pub fn build(self) -> HistoryEntry {
        HistoryEntry {
            schema_version: CURRENT_SCHEMA_VERSION,
            id: self.id,
            timestamp: self.timestamp,
            command: self.command,
            exit_code: self.exit_code,
            duration_ms: self.duration_ms,
            cwd: self.cwd,
            session_id: self.session_id,
            semantic_type: self.semantic_type,
            agent_id: self.agent_id,
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use phantom_semantic::CommandType;

    fn make_entry(cmd: &str) -> HistoryEntry {
        HistoryEntry::builder(cmd, "/home/dev/project", Uuid::new_v4()).build()
    }

    // -----------------------------------------------------------------------
    // 1. Builder defaults are sensible
    // -----------------------------------------------------------------------

    #[test]
    fn builder_defaults() {
        let e = make_entry("ls -la");
        assert_eq!(e.command(), "ls -la");
        assert_eq!(e.exit_code(), None);
        assert_eq!(e.duration_ms(), None);
        assert_eq!(e.cwd(), &PathBuf::from("/home/dev/project"));
        // "ls" is a known shell builtin — auto-classified at construction time.
        assert_eq!(e.semantic_type(), &CommandType::Shell);
    }

    // -----------------------------------------------------------------------
    // 2. Builder setters work
    // -----------------------------------------------------------------------

    #[test]
    fn builder_setters() {
        let session = Uuid::new_v4();
        let e = HistoryEntry::builder("cargo build", "/tmp", session)
            .exit_code(0)
            .duration_ms(1234)
            .semantic_type(CommandType::Shell)
            .build();

        assert_eq!(e.exit_code(), Some(0));
        assert_eq!(e.duration_ms(), Some(1234));
        assert_eq!(e.session_id(), session);
        assert_eq!(e.semantic_type(), &CommandType::Shell);
    }

    // -----------------------------------------------------------------------
    // 3. JSONL round-trip
    // -----------------------------------------------------------------------

    #[test]
    fn jsonl_round_trip() {
        let session = Uuid::new_v4();
        let original = HistoryEntry::builder("git status", "/repo", session)
            .exit_code(0)
            .duration_ms(42)
            .build();

        let line = original.to_jsonl_line().unwrap();
        // Must be a single line (no embedded newlines)
        assert!(!line.contains('\n'));

        let restored = HistoryEntry::from_jsonl_line(&line).unwrap();
        assert_eq!(restored.id(), original.id());
        assert_eq!(restored.command(), original.command());
        assert_eq!(restored.exit_code(), original.exit_code());
        assert_eq!(restored.duration_ms(), original.duration_ms());
        assert_eq!(restored.cwd(), original.cwd());
        assert_eq!(restored.session_id(), original.session_id());
    }

    // -----------------------------------------------------------------------
    // 4. Timestamp survives ISO-8601 round-trip
    // -----------------------------------------------------------------------

    #[test]
    fn timestamp_iso8601_round_trip() {
        let ts = Utc::now();
        let e = HistoryEntry::builder("echo hi", "/tmp", Uuid::new_v4())
            .timestamp(ts)
            .build();

        let line = e.to_jsonl_line().unwrap();
        let restored = HistoryEntry::from_jsonl_line(&line).unwrap();

        // Truncate sub-microsecond precision for reliable equality
        assert_eq!(
            restored.timestamp().timestamp_millis(),
            ts.timestamp_millis()
        );
    }

    // -----------------------------------------------------------------------
    // 5. Each entry gets a distinct UUID
    // -----------------------------------------------------------------------

    #[test]
    fn unique_ids() {
        let session = Uuid::new_v4();
        let a = HistoryEntry::builder("a", "/", session).build();
        let b = HistoryEntry::builder("b", "/", session).build();
        assert_ne!(a.id(), b.id());
    }

    // -----------------------------------------------------------------------
    // 6. Corrupt JSONL line returns an Err (no panic)
    // -----------------------------------------------------------------------

    #[test]
    fn corrupt_line_returns_err() {
        let result = HistoryEntry::from_jsonl_line("{not valid json");
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // 7. schema_version_defaults_to_one_on_old_entries
    //
    //    A record written without `schema_version` (simulating a pre-v1 entry)
    //    must deserialise successfully and report schema_version == 1.
    // -----------------------------------------------------------------------

    #[test]
    fn schema_version_defaults_to_one_on_old_entries() {
        // Craft a minimal valid JSON that omits schema_version entirely.
        let session = Uuid::new_v4();
        let id = Uuid::new_v4();
        let line = format!(
            r#"{{"id":"{id}","timestamp":"2024-01-01T00:00:00Z","command":"ls","exit_code":null,"duration_ms":null,"cwd":"/","session_id":"{session}","semantic_type":"Shell"}}"#,
        );

        let entry = HistoryEntry::from_jsonl_line(&line).unwrap();
        assert_eq!(
            entry.schema_version, 1,
            "missing schema_version should default to 1"
        );
        assert_eq!(entry.command(), "ls");
    }

    // -----------------------------------------------------------------------
    // 8. entry_with_future_schema_version_is_skipped_with_warning
    //
    //    A record with schema_version > CURRENT_SCHEMA_VERSION must be rejected.
    // -----------------------------------------------------------------------

    #[test]
    fn entry_with_future_schema_version_is_skipped_with_warning() {
        let session = Uuid::new_v4();
        let id = Uuid::new_v4();
        let future_version = CURRENT_SCHEMA_VERSION + 1;
        let line = format!(
            r#"{{"schema_version":{future_version},"id":"{id}","timestamp":"2024-01-01T00:00:00Z","command":"ls","exit_code":null,"duration_ms":null,"cwd":"/","session_id":"{session}","semantic_type":"Shell"}}"#,
        );

        let result = HistoryEntry::from_jsonl_line(&line);
        assert!(
            result.is_err(),
            "entry with future schema_version must be rejected"
        );
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("unsupported schema_version"),
            "error message should mention unsupported schema_version, got: {msg}"
        );
    }

    // -----------------------------------------------------------------------
    // 9. round_trip_preserves_schema_version
    //
    //    A new entry serialised and deserialised must have schema_version == 1.
    // -----------------------------------------------------------------------

    #[test]
    fn round_trip_preserves_schema_version() {
        let e = make_entry("cargo test");
        assert_eq!(e.schema_version, CURRENT_SCHEMA_VERSION);

        let line = e.to_jsonl_line().unwrap();
        let restored = HistoryEntry::from_jsonl_line(&line).unwrap();
        assert_eq!(restored.schema_version, CURRENT_SCHEMA_VERSION);
    }
}
