//! Agent-state persistence for phantom-session (Issue #76).
//!
//! Captures every live agent's conversation history and task description so
//! the user can resume agents after a restart.  Tool calls that were
//! *in-flight* at shutdown time are stripped — the agent resumes with stale
//! tool state cleared, so the next turn is a clean LLM round-trip.
//!
//! # File layout
//!
//! Agents are written to a sidecar JSON file alongside the main session:
//!
//! ```text
//! ~/.config/phantom/sessions/{hash}_{timestamp}_agents.json
//! ```
//!
//! # Restore flow
//!
//! On startup, if `agents.json` exists, the caller prompts "Resume N agents
//! from previous session?" and, if accepted, reconstructs `AgentSnapshot`
//! values that the app can use to re-spawn agents with their histories intact.
//!
//! # In-flight tool calls
//!
//! A tool call is *in-flight* when the preceding `AgentMessage::ToolCall`
//! does **not** yet have a matching `AgentMessage::ToolResult`.  Such calls
//! are stripped from the tail of the message history before serialization.
//! Completed tool call+result pairs are preserved.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{fs, io};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use phantom_agents::agent::{AgentMessage, AgentStatus, AgentTask};

// ---------------------------------------------------------------------------
// Saved message representation
// ---------------------------------------------------------------------------

/// A single message in a serialized agent conversation.
///
/// We cannot use `AgentMessage` directly because `ToolCall` and `ToolResult`
/// contain types that live in `phantom-agents` and serialise differently.
/// This intermediate type is a flat, stable serialization contract.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SavedMessage {
    System(String),
    User(String),
    Assistant(String),
    /// A completed tool-call/result pair, stored together.
    CompletedToolUse {
        tool_name: String,
        args: serde_json::Value,
        success: bool,
        output: String,
    },
}

impl SavedMessage {
    /// Convert a slice of `AgentMessage` values to `SavedMessage`, stripping
    /// any in-flight (unmatched) tool calls from the tail.
    ///
    /// The algorithm walks the slice in order:
    /// - `System`, `User`, and `Assistant` messages are emitted as-is.
    /// - `ToolCall` messages are buffered; they are only emitted once the
    ///   following `ToolResult` arrives.
    /// - `ToolResult` matches the most-recently-buffered call and emits a
    ///   `CompletedToolUse` entry.
    /// - Any buffered (unmatched) calls at the end are dropped — these are
    ///   the in-flight calls the issue asks us to discard.
    pub fn from_agent_messages(messages: &[AgentMessage]) -> Vec<Self> {
        use phantom_agents::tools::ToolCall;

        let mut out: Vec<SavedMessage> = Vec::with_capacity(messages.len());
        // Pending call waiting for its result.
        let mut pending_call: Option<&ToolCall> = None;

        for msg in messages {
            match msg {
                AgentMessage::System(s) => {
                    // Flush any pending (in-flight) call first — it's now
                    // superseded by a non-tool message, so discard it.
                    pending_call = None;
                    out.push(SavedMessage::System(s.clone()));
                }
                AgentMessage::User(s) => {
                    pending_call = None;
                    out.push(SavedMessage::User(s.clone()));
                }
                AgentMessage::Assistant(s) => {
                    pending_call = None;
                    out.push(SavedMessage::Assistant(s.clone()));
                }
                AgentMessage::ToolCall(tc) => {
                    // Discard previous unmatched call (shouldn't happen in a
                    // well-formed history but be defensive).
                    pending_call = Some(tc);
                }
                AgentMessage::ToolResult(tr) => {
                    if let Some(call) = pending_call.take() {
                        out.push(SavedMessage::CompletedToolUse {
                            tool_name: call.tool.api_name().to_owned(),
                            args: call.args.clone(),
                            success: tr.success,
                            output: tr.output.clone(),
                        });
                    }
                    // If there's no pending call (shouldn't happen), drop the
                    // orphan result rather than panic.
                }
            }
        }
        // Any leftover `pending_call` is intentionally dropped — it was
        // in-flight at shutdown.
        out
    }
}

// ---------------------------------------------------------------------------
// AgentSnapshot — the per-agent saved record
// ---------------------------------------------------------------------------

/// Saved state of a single agent.
///
/// All fields are private; use the constructor and accessor methods.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSnapshot {
    id: u32,
    task: AgentTask,
    /// `Queued`, `Done`, `Failed`, or `Flatline` — the saved status.
    /// In-progress states (`Working`, `WaitingForTool`) are normalised to
    /// `Queued` so the agent can restart cleanly.
    status: AgentStatus,
    messages: Vec<SavedMessage>,
    /// Unix epoch seconds at agent creation (approximated from saved session).
    created_at_secs: u64,
    /// Reason the agent entered Flatline, if applicable.
    flatline_reason: Option<String>,
    /// Visible output log snapshot.
    output_log: Vec<String>,
}

impl AgentSnapshot {
    // -- Constructor ---------------------------------------------------------

    /// Build a snapshot from a live agent.
    ///
    /// In-flight tool calls are stripped.  In-progress status is normalised to
    /// `Queued` so the agent can restart from a clean state.
    pub fn from_agent(agent: &phantom_agents::Agent) -> Self {
        let status = match agent.status {
            AgentStatus::Working | AgentStatus::WaitingForTool => AgentStatus::Queued,
            other => other,
        };

        let messages = SavedMessage::from_agent_messages(&agent.messages);

        let created_at_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        Self {
            id: agent.id,
            task: agent.task.clone(),
            status,
            messages,
            created_at_secs,
            flatline_reason: agent.flatline_reason.clone(),
            output_log: agent.output_log.clone(),
        }
    }

    // -- Accessors -----------------------------------------------------------

    /// The agent's numeric identifier.
    pub fn id(&self) -> u32 {
        self.id
    }

    /// The task the agent was performing.
    pub fn task(&self) -> &AgentTask {
        &self.task
    }

    /// The normalised lifecycle status (never `Working` or `WaitingForTool`).
    pub fn status(&self) -> AgentStatus {
        self.status
    }

    /// Saved conversation messages (in-flight tool calls stripped).
    pub fn messages(&self) -> &[SavedMessage] {
        &self.messages
    }

    /// Approximate creation timestamp (unix epoch seconds).
    pub fn created_at_secs(&self) -> u64 {
        self.created_at_secs
    }

    /// Flatline reason if the agent was in Flatline state at shutdown.
    pub fn flatline_reason(&self) -> Option<&str> {
        self.flatline_reason.as_deref()
    }

    /// Visible output lines at the time of save.
    pub fn output_log(&self) -> &[String] {
        &self.output_log
    }

    /// Number of non-system messages in the saved history.
    pub fn conversation_depth(&self) -> usize {
        self.messages
            .iter()
            .filter(|m| !matches!(m, SavedMessage::System(_)))
            .count()
    }
}

// ---------------------------------------------------------------------------
// AgentStateFile — the envelope saved to disk
// ---------------------------------------------------------------------------

/// The top-level JSON file that holds all saved agents for a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentStateFile {
    /// Schema version — bump when the format changes incompatibly.
    version: u32,
    /// Unix epoch seconds when the file was written.
    saved_at: u64,
    /// All saved agents.
    agents: Vec<AgentSnapshot>,
}

impl AgentStateFile {
    /// Create a new agent state file from a collection of snapshots.
    pub fn new(agents: Vec<AgentSnapshot>) -> Self {
        let saved_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Self {
            version: 1,
            saved_at,
            agents,
        }
    }

    // -- Accessors -----------------------------------------------------------

    /// Schema version.
    pub fn version(&self) -> u32 {
        self.version
    }

    /// When this file was saved (unix epoch seconds).
    pub fn saved_at(&self) -> u64 {
        self.saved_at
    }

    /// All saved agent snapshots.
    pub fn agents(&self) -> &[AgentSnapshot] {
        &self.agents
    }

    /// Number of saved agents.
    pub fn agent_count(&self) -> usize {
        self.agents.len()
    }

    // -- I/O -----------------------------------------------------------------

    /// Write this file to `path` atomically (write temp file, then rename).
    pub fn save(&self, path: &Path) -> Result<()> {
        let json =
            serde_json::to_string_pretty(self).context("failed to serialize agent state")?;

        // Atomic write: temp file in same directory, then rename.
        let parent = path.parent().unwrap_or(Path::new("."));
        let tmp = parent.join(format!(
            ".agents_tmp_{}.json",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));

        fs::write(&tmp, &json)
            .with_context(|| format!("failed to write agent state temp: {}", tmp.display()))?;

        fs::rename(&tmp, path)
            .with_context(|| format!("failed to rename agent state to: {}", path.display()))
    }

    /// Load from `path`, returning `None` if the file does not exist.
    ///
    /// Returns an error if the file exists but cannot be parsed — callers
    /// should treat this as a corrupt file and offer to discard.
    pub fn load(path: &Path) -> Result<Option<Self>> {
        match fs::read_to_string(path) {
            Ok(contents) => {
                let file: Self = serde_json::from_str(&contents)
                    .with_context(|| format!("failed to parse agent state: {}", path.display()))?;
                Ok(Some(file))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| {
                format!("failed to read agent state: {}", path.display())
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// AgentStatePersister — convenience wrapper used by SessionManager
// ---------------------------------------------------------------------------

/// High-level persistence helper for agent state.
///
/// Derives the sidecar file path from the same session-file path used by
/// `SessionManager`, keeping agent state co-located with the session.
pub struct AgentStatePersister {
    path: PathBuf,
}

impl AgentStatePersister {
    /// Derive the agent-state sidecar path from a session file path.
    ///
    /// Given `{hash}_{ts}.json`, returns `{hash}_{ts}_agents.json`.
    pub fn sidecar_path(session_path: &Path) -> PathBuf {
        let stem = session_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("session");
        let sidecar_name = format!("{stem}_agents.json");
        session_path
            .parent()
            .unwrap_or(Path::new("."))
            .join(sidecar_name)
    }

    /// Create a persister for the given sidecar path.
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Snapshot and save a collection of live agents.
    ///
    /// Only agents whose status is not `Done` are snapshotted by default —
    /// completed agents don't need to be resumed.  Pass `include_done = true`
    /// to include them anyway (useful for history / audit).
    pub fn save_agents(
        &self,
        agents: &[&phantom_agents::Agent],
        include_done: bool,
    ) -> Result<usize> {
        let snapshots: Vec<AgentSnapshot> = agents
            .iter()
            .filter(|a| include_done || a.status != AgentStatus::Done)
            .map(|a| AgentSnapshot::from_agent(a))
            .collect();

        let count = snapshots.len();
        let file = AgentStateFile::new(snapshots);
        file.save(&self.path)?;
        Ok(count)
    }

    /// Load the saved agent state.
    ///
    /// Returns `None` if no sidecar file exists yet.
    pub fn load(&self) -> Result<Option<AgentStateFile>> {
        AgentStateFile::load(&self.path)
    }

    /// Delete the sidecar file (e.g., after the user declines to restore).
    pub fn discard(&self) -> Result<()> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e)
                .with_context(|| format!("failed to delete agent state: {}", self.path.display())),
        }
    }

    /// The path this persister writes to.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

// ---------------------------------------------------------------------------
// Partial-restore helpers
// ---------------------------------------------------------------------------

/// Outcome of attempting to restore a single agent from a snapshot.
#[derive(Debug)]
pub enum RestoreOutcome<T> {
    /// Successfully reconstructed the value.
    Ok(T),
    /// The snapshot was present but could not be reconstructed.
    /// The caller should skip this agent but continue with others.
    Skipped {
        agent_id: u32,
        reason: String,
    },
    /// The snapshot was corrupt enough that we couldn't even extract an id.
    Corrupt { reason: String },
}

/// Attempt to reconstruct all agents from a saved file, tolerating failures
/// on individual agents (graceful degradation).
///
/// Returns a vec of `RestoreOutcome`, one per saved snapshot.  Callers can
/// count `Ok` entries to decide whether to show the "resume" prompt and can
/// log `Skipped`/`Corrupt` entries for diagnostics.
pub fn partial_restore(file: &AgentStateFile) -> Vec<RestoreOutcome<AgentSnapshot>> {
    file.agents()
        .iter()
        .map(|snap| {
            // Validate the snapshot is usable:
            // 1. The task must be representable (it always is — it's an enum
            //    we own — but the serialization round-trip could theoretically
            //    fail if the format drifted).
            // 2. We require at least version 1.
            if file.version() < 1 {
                return RestoreOutcome::Corrupt {
                    reason: format!("unsupported version {}", file.version()),
                };
            }

            // Re-serialize the task to catch any deserialization drift.
            match serde_json::to_string(&snap.task) {
                Ok(_) => RestoreOutcome::Ok(snap.clone()),
                Err(e) => RestoreOutcome::Skipped {
                    agent_id: snap.id(),
                    reason: format!("task re-serialization failed: {e}"),
                },
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use phantom_agents::agent::{Agent, AgentMessage, AgentStatus, AgentTask};
    use phantom_agents::tools::{ToolCall, ToolResult, ToolType};
    use tempfile::TempDir;

    fn free_agent(id: u32, prompt: &str) -> Agent {
        Agent::new(id, AgentTask::FreeForm { prompt: prompt.into() })
    }

    fn tool_call(tool: ToolType) -> AgentMessage {
        AgentMessage::ToolCall(ToolCall {
            tool,
            args: serde_json::json!({"path": "test.rs"}),
        })
    }

    fn tool_result(tool: ToolType, success: bool) -> AgentMessage {
        AgentMessage::ToolResult(ToolResult {
            tool,
            success,
            output: "file contents".into(),
            ..Default::default()
        })
    }

    // -- SavedMessage conversion ------------------------------------------

    #[test]
    fn completed_tool_pair_is_preserved() {
        let msgs = vec![
            AgentMessage::User("do something".into()),
            tool_call(ToolType::ReadFile),
            tool_result(ToolType::ReadFile, true),
        ];
        let saved = SavedMessage::from_agent_messages(&msgs);
        assert_eq!(saved.len(), 2); // User + CompletedToolUse
        assert!(matches!(saved[1], SavedMessage::CompletedToolUse { .. }));
    }

    #[test]
    fn inflight_tool_call_is_stripped() {
        let msgs = vec![
            AgentMessage::User("start".into()),
            AgentMessage::Assistant("calling tool".into()),
            tool_call(ToolType::WriteFile), // no following ToolResult
        ];
        let saved = SavedMessage::from_agent_messages(&msgs);
        // The in-flight ToolCall must be stripped.
        assert_eq!(saved.len(), 2);
        assert!(matches!(saved[0], SavedMessage::User(_)));
        assert!(matches!(saved[1], SavedMessage::Assistant(_)));
    }

    #[test]
    fn multiple_completed_pairs_all_preserved() {
        let msgs = vec![
            AgentMessage::User("task".into()),
            tool_call(ToolType::ReadFile),
            tool_result(ToolType::ReadFile, true),
            AgentMessage::Assistant("got it".into()),
            tool_call(ToolType::GitStatus),
            tool_result(ToolType::GitStatus, true),
        ];
        let saved = SavedMessage::from_agent_messages(&msgs);
        assert_eq!(saved.len(), 4); // User + Completed + Assistant + Completed
    }

    #[test]
    fn inflight_at_end_stripped_completed_pair_at_start_preserved() {
        let msgs = vec![
            tool_call(ToolType::ReadFile),
            tool_result(ToolType::ReadFile, false), // completed
            tool_call(ToolType::WriteFile),          // in-flight
        ];
        let saved = SavedMessage::from_agent_messages(&msgs);
        assert_eq!(saved.len(), 1);
        assert!(matches!(
            &saved[0],
            SavedMessage::CompletedToolUse { success, .. } if !success
        ));
    }

    #[test]
    fn empty_messages_produces_empty_saved() {
        let saved = SavedMessage::from_agent_messages(&[]);
        assert!(saved.is_empty());
    }

    // -- AgentSnapshot --------------------------------------------------------

    #[test]
    fn snapshot_from_queued_agent() {
        let agent = free_agent(1, "fix the tests");
        let snap = AgentSnapshot::from_agent(&agent);
        assert_eq!(snap.id(), 1);
        assert_eq!(snap.status(), AgentStatus::Queued);
        assert!(snap.messages().is_empty());
    }

    #[test]
    fn working_status_normalised_to_queued() {
        let mut agent = free_agent(2, "working agent");
        agent.status = AgentStatus::Working;
        let snap = AgentSnapshot::from_agent(&agent);
        assert_eq!(snap.status(), AgentStatus::Queued,
            "Working must normalise to Queued for clean restart");
    }

    #[test]
    fn waiting_for_tool_normalised_to_queued() {
        let mut agent = free_agent(3, "waiting agent");
        agent.status = AgentStatus::WaitingForTool;
        let snap = AgentSnapshot::from_agent(&agent);
        assert_eq!(snap.status(), AgentStatus::Queued);
    }

    #[test]
    fn done_status_preserved() {
        let mut agent = free_agent(4, "done agent");
        agent.status = AgentStatus::Done;
        let snap = AgentSnapshot::from_agent(&agent);
        assert_eq!(snap.status(), AgentStatus::Done);
    }

    #[test]
    fn flatline_reason_preserved() {
        let mut agent = free_agent(5, "flatline agent");
        agent.flatline("exceeded retries");
        let snap = AgentSnapshot::from_agent(&agent);
        assert_eq!(snap.status(), AgentStatus::Flatline);
        assert_eq!(snap.flatline_reason(), Some("exceeded retries"));
    }

    #[test]
    fn output_log_preserved() {
        let mut agent = free_agent(6, "noisy agent");
        agent.log("line 1");
        agent.log("line 2");
        let snap = AgentSnapshot::from_agent(&agent);
        assert_eq!(snap.output_log(), &["line 1", "line 2"]);
    }

    #[test]
    fn conversation_depth_excludes_system_messages() {
        let mut agent = free_agent(7, "chat agent");
        agent.push_message(AgentMessage::System("system prompt".into()));
        agent.push_message(AgentMessage::User("hello".into()));
        agent.push_message(AgentMessage::Assistant("hi".into()));
        // one in-flight tool call — will be stripped
        agent.push_message(tool_call(ToolType::ReadFile));

        let snap = AgentSnapshot::from_agent(&agent);
        // System is stripped from depth count; in-flight tool call is stripped from messages.
        assert_eq!(snap.conversation_depth(), 2); // User + Assistant
    }

    // -- AgentStateFile save/load round-trip ---------------------------------

    #[test]
    fn save_and_load_round_trip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("agents.json");

        let agent1 = free_agent(1, "task one");
        let agent2 = free_agent(2, "task two");
        let snaps = vec![
            AgentSnapshot::from_agent(&agent1),
            AgentSnapshot::from_agent(&agent2),
        ];
        let file = AgentStateFile::new(snaps);
        file.save(&path).unwrap();

        let loaded = AgentStateFile::load(&path).unwrap().unwrap();
        assert_eq!(loaded.version(), 1);
        assert_eq!(loaded.agent_count(), 2);
        assert_eq!(loaded.agents()[0].id(), 1);
        assert_eq!(loaded.agents()[1].id(), 2);
    }

    #[test]
    fn atomic_write_creates_no_temp_file_after_success() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("agents.json");
        let agent = free_agent(1, "atomic test");
        let file = AgentStateFile::new(vec![AgentSnapshot::from_agent(&agent)]);
        file.save(&path).unwrap();

        // The final file must exist.
        assert!(path.exists());
        // No `.agents_tmp_*.json` should remain.
        let temps: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.starts_with(".agents_tmp_"))
            })
            .collect();
        assert!(temps.is_empty(), "temp files must be cleaned up");
    }

    #[test]
    fn load_missing_file_returns_none() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nonexistent_agents.json");
        let result = AgentStateFile::load(&path).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn load_corrupt_file_returns_error() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("bad_agents.json");
        fs::write(&path, "not valid json {{{").unwrap();
        let result = AgentStateFile::load(&path);
        assert!(result.is_err());
    }

    // -- AgentStatePersister -------------------------------------------------

    #[test]
    fn sidecar_path_derived_from_session_path() {
        let session = PathBuf::from("/sessions/abc123_1700000000.json");
        let sidecar = AgentStatePersister::sidecar_path(&session);
        assert_eq!(
            sidecar,
            PathBuf::from("/sessions/abc123_1700000000_agents.json")
        );
    }

    #[test]
    fn save_agents_skips_done_by_default() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("agents.json");
        let persister = AgentStatePersister::new(path.clone());

        let agent1 = free_agent(1, "ongoing");
        let mut agent2 = free_agent(2, "done agent");
        agent2.status = AgentStatus::Done;

        persister.save_agents(&[&agent1, &agent2], false).unwrap();

        let loaded = AgentStateFile::load(&path).unwrap().unwrap();
        assert_eq!(loaded.agent_count(), 1, "Done agent must be skipped");
        assert_eq!(loaded.agents()[0].id(), 1);
    }

    #[test]
    fn save_agents_includes_done_when_requested() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("agents.json");
        let persister = AgentStatePersister::new(path.clone());

        let agent1 = free_agent(1, "ongoing");
        let mut agent2 = free_agent(2, "done agent");
        agent2.status = AgentStatus::Done;

        persister.save_agents(&[&agent1, &agent2], true).unwrap();

        let loaded = AgentStateFile::load(&path).unwrap().unwrap();
        assert_eq!(loaded.agent_count(), 2);
    }

    #[test]
    fn discard_removes_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("agents.json");
        fs::write(&path, "{}").unwrap();
        let persister = AgentStatePersister::new(path.clone());
        persister.discard().unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn discard_nonexistent_is_ok() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nonexistent_agents.json");
        let persister = AgentStatePersister::new(path);
        // Must not error.
        persister.discard().unwrap();
    }

    // -- partial_restore: graceful degradation --------------------------------

    #[test]
    fn partial_restore_returns_ok_for_valid_snapshots() {
        let agents: Vec<AgentSnapshot> = (1u32..=3)
            .map(|i| AgentSnapshot::from_agent(&free_agent(i, "task")))
            .collect();
        let file = AgentStateFile::new(agents);
        let outcomes = partial_restore(&file);
        assert_eq!(outcomes.len(), 3);
        assert!(outcomes
            .iter()
            .all(|o| matches!(o, RestoreOutcome::Ok(_))));
    }

    #[test]
    fn partial_restore_version_zero_is_corrupt() {
        let file = AgentStateFile {
            version: 0,
            saved_at: 0,
            agents: vec![AgentSnapshot::from_agent(&free_agent(1, "x"))],
        };
        let outcomes = partial_restore(&file);
        assert!(matches!(outcomes[0], RestoreOutcome::Corrupt { .. }));
    }

    #[test]
    fn partial_restore_empty_file_returns_empty() {
        let file = AgentStateFile::new(vec![]);
        let outcomes = partial_restore(&file);
        assert!(outcomes.is_empty());
    }

    // -- Full save → restore integration -------------------------------------

    #[test]
    fn save_restore_preserves_conversation_history() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("agents.json");

        let mut agent = free_agent(42, "implement feature X");
        agent.push_message(AgentMessage::System("You are a code agent".into()));
        agent.push_message(AgentMessage::User("implement feature X".into()));
        agent.push_message(AgentMessage::Assistant("Reading the codebase...".into()));
        agent.push_message(tool_call(ToolType::ReadFile));
        agent.push_message(tool_result(ToolType::ReadFile, true));

        let snap = AgentSnapshot::from_agent(&agent);
        let file = AgentStateFile::new(vec![snap]);
        file.save(&path).unwrap();

        let loaded = AgentStateFile::load(&path).unwrap().unwrap();
        let restored = &loaded.agents()[0];

        assert_eq!(restored.id(), 42);
        assert_eq!(restored.messages().len(), 4); // System + User + Assistant + CompletedToolUse
        assert!(matches!(restored.messages()[3], SavedMessage::CompletedToolUse { .. }));
    }

    #[test]
    fn save_restore_strips_inflight_calls() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("agents.json");

        let mut agent = free_agent(99, "crashed mid-call");
        agent.push_message(AgentMessage::User("do it".into()));
        agent.push_message(tool_call(ToolType::WriteFile)); // in-flight — no ToolResult

        let snap = AgentSnapshot::from_agent(&agent);
        let file = AgentStateFile::new(vec![snap]);
        file.save(&path).unwrap();

        let loaded = AgentStateFile::load(&path).unwrap().unwrap();
        let restored = &loaded.agents()[0];

        assert_eq!(restored.messages().len(), 1, "Only the User message; in-flight call must be stripped");
        assert!(matches!(restored.messages()[0], SavedMessage::User(_)));
    }
}
