//! Agent output capture — records an agent's tool calls and text output as a
//! JSONL sidecar file next to the main history file.
//!
//! Layout on disk:
//! ```text
//! ~/.local/share/phantom/history/
//!   <session_id>.jsonl            ← HistoryStore
//!   <session_id>-agents.jsonl     ← AgentOutputCapture (sidecar)
//! ```

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// ToolCall
// ---------------------------------------------------------------------------

/// A record of a single tool invocation made by an agent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    /// Name of the tool (e.g. `"ReadFile"`, `"RunCommand"`).
    name: String,
    /// JSON-encoded input arguments, stored as a raw string for flexibility.
    input_json: String,
    /// JSON-encoded output / result, or an error message.
    output_json: Option<String>,
    /// When the call was made.
    called_at: DateTime<Utc>,
}

impl ToolCall {
    /// Construct a new tool call record.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        input_json: impl Into<String>,
        output_json: Option<String>,
    ) -> Self {
        Self {
            name: name.into(),
            input_json: input_json.into(),
            output_json,
            called_at: Utc::now(),
        }
    }

    // Getters

    /// Tool name.
    #[must_use]
    pub fn name(&self) -> &str { &self.name }

    /// Raw JSON of the input arguments.
    #[must_use]
    pub fn input_json(&self) -> &str { &self.input_json }

    /// Raw JSON of the result, if the call completed.
    #[must_use]
    pub fn output_json(&self) -> Option<&str> { self.output_json.as_deref() }

    /// Timestamp of the invocation.
    #[must_use]
    pub fn called_at(&self) -> DateTime<Utc> { self.called_at }
}

// ---------------------------------------------------------------------------
// AgentRecord  (one record per append)
// ---------------------------------------------------------------------------

/// A single captured record from an agent run.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct AgentRecord {
    /// UUID of this record.
    id: Uuid,
    /// Which agent produced this record.
    agent_name: String,
    /// Session the agent belongs to.
    session_id: Uuid,
    /// Tool calls made during this record, in order.
    tool_calls: Vec<ToolCall>,
    /// Free-form text output produced by the agent.
    text_output: String,
    /// When this record was captured.
    captured_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// AgentOutputCapture
// ---------------------------------------------------------------------------

/// Append-only JSONL sidecar that records agent activity.
///
/// All public methods return [`anyhow::Result`] — no `.unwrap()` in
/// production paths.
///
/// `Clone` is derived so callers can hand a copy to each spawned
/// `AgentPane` at launch time — the underlying `PathBuf` is cheap to clone
/// and all writes go through `OpenOptions::append(true)` so concurrent
/// clones are safe at the OS level.
#[derive(Clone)]
pub struct AgentOutputCapture {
    path: PathBuf,
}

impl AgentOutputCapture {
    // -----------------------------------------------------------------------
    // Construction
    // -----------------------------------------------------------------------

    /// Open (or create) the sidecar for `session_id` next to the main history
    /// file under `~/.local/share/phantom/history/`.
    pub fn open(session_id: Uuid) -> Result<Self> {
        let dir = default_data_dir();
        fs::create_dir_all(&dir)
            .with_context(|| format!("cannot create history dir: {}", dir.display()))?;
        let path = dir.join(format!("{session_id}-agents.jsonl"));
        Ok(Self { path })
    }

    /// Open a sidecar at an explicit path (used in tests).
    #[must_use]
    pub fn open_at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    // -----------------------------------------------------------------------
    // Write
    // -----------------------------------------------------------------------

    /// Append one agent activity record to the sidecar.
    ///
    /// # Parameters
    /// * `agent_name` — human-readable agent identifier.
    /// * `session_id` — session UUID.
    /// * `tool_calls` — ordered list of tool invocations.
    /// * `text_output` — agent's free-form text.
    pub fn append(
        &self,
        agent_name: impl Into<String>,
        session_id: Uuid,
        tool_calls: Vec<ToolCall>,
        text_output: impl Into<String>,
    ) -> Result<()> {
        let record = AgentRecord {
            id: Uuid::new_v4(),
            agent_name: agent_name.into(),
            session_id,
            tool_calls,
            text_output: text_output.into(),
            captured_at: Utc::now(),
        };

        let line =
            serde_json::to_string(&record).context("failed to serialise AgentRecord")?;

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("cannot open agent sidecar: {}", self.path.display()))?;

        writeln!(file, "{line}")
            .with_context(|| format!("cannot write to agent sidecar: {}", self.path.display()))?;

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Read
    // -----------------------------------------------------------------------

    /// Return the most-recent `limit` records in chronological order.
    pub fn recent(&self, limit: usize) -> Result<Vec<CapturedAgent>> {
        let all = self.read_all()?;
        let start = all.len().saturating_sub(limit);
        Ok(all[start..].to_vec())
    }

    /// Return all records for a given agent name, in chronological order.
    pub fn by_agent(&self, agent_name: &str) -> Result<Vec<CapturedAgent>> {
        let all = self.read_all()?;
        Ok(all
            .into_iter()
            .filter(|r| r.agent_name() == agent_name)
            .collect())
    }

    /// Total number of non-corrupt records in the sidecar.
    pub fn count(&self) -> Result<usize> {
        if !self.path.exists() {
            return Ok(0);
        }
        let file = fs::File::open(&self.path)
            .with_context(|| format!("cannot open agent sidecar: {}", self.path.display()))?;
        let reader = BufReader::new(file);
        let mut count = 0_usize;
        for line in reader.lines() {
            let line = line.context("read error counting agent sidecar lines")?;
            let t = line.trim();
            if !t.is_empty() && serde_json::from_str::<AgentRecord>(t).is_ok() {
                count += 1;
            }
        }
        Ok(count)
    }

    /// Path to the sidecar file.
    #[must_use]
    pub fn path(&self) -> &Path { &self.path }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    fn read_all(&self) -> Result<Vec<CapturedAgent>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let file = fs::File::open(&self.path)
            .with_context(|| format!("cannot open agent sidecar: {}", self.path.display()))?;
        let reader = BufReader::new(file);
        let mut records = Vec::new();
        for line in reader.lines() {
            let line = line.context("read error in agent sidecar")?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            match serde_json::from_str::<AgentRecord>(trimmed) {
                Ok(r) => records.push(CapturedAgent::from(r)),
                Err(e) => log::warn!("skipping corrupt agent sidecar line: {e}"),
            }
        }
        Ok(records)
    }
}

// ---------------------------------------------------------------------------
// CapturedAgent — the public-facing view of an AgentRecord
// ---------------------------------------------------------------------------

/// Public read-only view of a captured agent run.
#[derive(Debug, Clone)]
pub struct CapturedAgent {
    id: Uuid,
    agent_name: String,
    session_id: Uuid,
    tool_calls: Vec<ToolCall>,
    text_output: String,
    captured_at: DateTime<Utc>,
}

impl CapturedAgent {
    /// Record UUID.
    #[must_use]
    pub fn id(&self) -> Uuid { self.id }

    /// Agent name.
    #[must_use]
    pub fn agent_name(&self) -> &str { &self.agent_name }

    /// Session UUID.
    #[must_use]
    pub fn session_id(&self) -> Uuid { self.session_id }

    /// Tool calls in invocation order.
    #[must_use]
    pub fn tool_calls(&self) -> &[ToolCall] { &self.tool_calls }

    /// Free-form text output.
    #[must_use]
    pub fn text_output(&self) -> &str { &self.text_output }

    /// Capture timestamp.
    #[must_use]
    pub fn captured_at(&self) -> DateTime<Utc> { self.captured_at }
}

impl From<AgentRecord> for CapturedAgent {
    fn from(r: AgentRecord) -> Self {
        Self {
            id: r.id,
            agent_name: r.agent_name,
            session_id: r.session_id,
            tool_calls: r.tool_calls,
            text_output: r.text_output,
            captured_at: r.captured_at,
        }
    }
}

/// Default data directory: `~/.local/share/phantom/history/`.
fn default_data_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home)
        .join(".local")
        .join("share")
        .join("phantom")
        .join("history")
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_capture() -> (AgentOutputCapture, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agents.jsonl");
        let cap = AgentOutputCapture::open_at(&path);
        (cap, dir)
    }

    fn tool(name: &str, input: &str) -> ToolCall {
        ToolCall::new(name, input, Some(r#"{"ok":true}"#.to_string()))
    }

    // -----------------------------------------------------------------------
    // 1. Append increments count
    // -----------------------------------------------------------------------

    #[test]
    fn append_increments_count() {
        let (cap, _dir) = temp_capture();
        let session = Uuid::new_v4();

        assert_eq!(cap.count().unwrap(), 0);

        cap.append("defender", session, vec![], "Hello from defender")
            .unwrap();
        assert_eq!(cap.count().unwrap(), 1);

        cap.append("inspector", session, vec![], "Audit done")
            .unwrap();
        assert_eq!(cap.count().unwrap(), 2);
    }

    // -----------------------------------------------------------------------
    // 2. Tool calls survive round-trip
    // -----------------------------------------------------------------------

    #[test]
    fn tool_calls_round_trip() {
        let (cap, _dir) = temp_capture();
        let session = Uuid::new_v4();

        let calls = vec![
            tool("ReadFile", r#"{"path": "/etc/hosts"}"#),
            tool("RunCommand", r#"{"cmd": "ls"}"#),
        ];

        cap.append("agent-x", session, calls.clone(), "done").unwrap();

        let recent = cap.recent(1).unwrap();
        assert_eq!(recent.len(), 1);

        let recorded = &recent[0];
        assert_eq!(recorded.tool_calls().len(), 2);
        assert_eq!(recorded.tool_calls()[0].name(), "ReadFile");
        assert_eq!(recorded.tool_calls()[1].name(), "RunCommand");
    }

    // -----------------------------------------------------------------------
    // 3. Text output survives round-trip
    // -----------------------------------------------------------------------

    #[test]
    fn text_output_round_trip() {
        let (cap, _dir) = temp_capture();
        let session = Uuid::new_v4();

        cap.append("agent-y", session, vec![], "The answer is 42")
            .unwrap();

        let recent = cap.recent(1).unwrap();
        assert_eq!(recent[0].text_output(), "The answer is 42");
    }

    // -----------------------------------------------------------------------
    // 4. by_agent filters correctly
    // -----------------------------------------------------------------------

    #[test]
    fn by_agent_filters_correctly() {
        let (cap, _dir) = temp_capture();
        let session = Uuid::new_v4();

        cap.append("alpha", session, vec![], "alpha-1").unwrap();
        cap.append("beta", session, vec![], "beta-1").unwrap();
        cap.append("alpha", session, vec![], "alpha-2").unwrap();

        let alpha_records = cap.by_agent("alpha").unwrap();
        assert_eq!(alpha_records.len(), 2);
        assert!(alpha_records.iter().all(|r| r.agent_name() == "alpha"));

        let beta_records = cap.by_agent("beta").unwrap();
        assert_eq!(beta_records.len(), 1);
    }

    // -----------------------------------------------------------------------
    // 5. Empty capture is safe
    // -----------------------------------------------------------------------

    #[test]
    fn empty_capture_is_safe() {
        let (cap, _dir) = temp_capture();
        assert_eq!(cap.count().unwrap(), 0);
        assert!(cap.recent(10).unwrap().is_empty());
        assert!(cap.by_agent("nobody").unwrap().is_empty());
    }

    // -----------------------------------------------------------------------
    // 6. Corrupt lines are skipped gracefully
    // -----------------------------------------------------------------------

    #[test]
    fn corrupt_lines_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agents.jsonl");
        let session = Uuid::new_v4();

        let cap = AgentOutputCapture::open_at(&path);
        cap.append("good-agent", session, vec![], "first").unwrap();

        // Inject garbage
        {
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            writeln!(f, "{{garbage line").unwrap();
        }

        cap.append("good-agent", session, vec![], "second").unwrap();

        let records = cap.recent(10).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].text_output(), "first");
        assert_eq!(records[1].text_output(), "second");
    }

    // -----------------------------------------------------------------------
    // 7. ToolCall getters work
    // -----------------------------------------------------------------------

    #[test]
    fn tool_call_getters() {
        let tc = ToolCall::new("WriteFile", r#"{"path":"/tmp/x","content":"y"}"#, None);
        assert_eq!(tc.name(), "WriteFile");
        assert!(tc.input_json().contains("/tmp/x"));
        assert!(tc.output_json().is_none());
    }
}
