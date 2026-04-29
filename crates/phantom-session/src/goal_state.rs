//! Goal / TaskLedger persistence for phantom-session (Issue #77).
//!
//! Saves the active `TaskLedger` from `phantom-brain` to a sidecar JSON file
//! so the reconciler can resume from where it left off after a restart.
//!
//! # Conflict policy
//!
//! Steps that were `Active` (in-progress) at shutdown are demoted to `Pending`
//! on restore so they will be retried cleanly.  `Done`, `Failed`, and
//! `Skipped` steps are preserved as-is so completed work is not re-executed.
//!
//! # File layout
//!
//! ```text
//! ~/.local/share/phantom/goals.json
//! ```
//!
//! The file is rewritten atomically on every save via a temp-file rename.
//! A second sidecar alongside the session file is also supported for
//! project-scoped saves (see `GoalStatePersister::sidecar_path`).

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{fs, io};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use phantom_agents::agent::AgentTask;

// ---------------------------------------------------------------------------
// Saved step status
// ---------------------------------------------------------------------------

/// Serialisable mirror of `phantom_brain::orchestrator::StepStatus`.
///
/// We cannot depend on `phantom-brain` from `phantom-session` (it would create
/// a circular dependency) so we replicate the small enum here.  A conversion
/// function in `phantom-brain` will map between the two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SavedStepStatus {
    Pending,
    /// Active steps are demoted to Pending on restore.
    Active,
    Done,
    Failed,
    Skipped,
}

impl SavedStepStatus {
    /// The status that should be used when restoring this step.
    ///
    /// `Active` → `Pending` (retry policy from the issue spec).
    /// All other statuses are restored as-is.
    pub fn restore_as(self) -> Self {
        match self {
            Self::Active => Self::Pending,
            other => other,
        }
    }
}

// ---------------------------------------------------------------------------
// SavedPlanStep
// ---------------------------------------------------------------------------

/// Serialisable snapshot of a single plan step.
///
/// All fields are private; use the constructor and accessor methods.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedPlanStep {
    description: String,
    assigned_task: AgentTask,
    status: SavedStepStatus,
    /// The agent id that was handling this step, if any.
    agent_id: Option<u32>,
    attempts: u32,
    max_attempts: u32,
    result_summary: Option<String>,
}

impl SavedPlanStep {
    // -- Constructor ---------------------------------------------------------

    /// Build a saved step, applying the restore conflict policy.
    ///
    /// This is not called directly — use `SavedPlanStep::from_raw` when you
    /// have the raw field values from the ledger.
    fn from_raw(
        description: String,
        assigned_task: AgentTask,
        status: SavedStepStatus,
        agent_id: Option<u32>,
        attempts: u32,
        max_attempts: u32,
        result_summary: Option<String>,
    ) -> Self {
        Self {
            description,
            assigned_task,
            status,
            agent_id,
            attempts,
            max_attempts,
            result_summary,
        }
    }

    // -- Accessors -----------------------------------------------------------

    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn assigned_task(&self) -> &AgentTask {
        &self.assigned_task
    }

    pub fn status(&self) -> SavedStepStatus {
        self.status
    }

    pub fn agent_id(&self) -> Option<u32> {
        self.agent_id
    }

    pub fn attempts(&self) -> u32 {
        self.attempts
    }

    pub fn max_attempts(&self) -> u32 {
        self.max_attempts
    }

    pub fn result_summary(&self) -> Option<&str> {
        self.result_summary.as_deref()
    }

    /// The status that should be used when reconstructing a live `PlanStep`.
    ///
    /// `Active` → `Pending` (conflict policy: retry in-progress work).
    pub fn restore_status(&self) -> SavedStepStatus {
        self.status.restore_as()
    }
}

// ---------------------------------------------------------------------------
// SavedFact — knowledge base entry
// ---------------------------------------------------------------------------

/// Serialisable mirror of `phantom_brain::orchestrator::FactConfidence`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SavedFactConfidence {
    Verified,
    ToLookUp,
    ToDerive,
    Guess,
}

/// A fact from the ledger's knowledge base.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedFact {
    content: String,
    confidence: SavedFactConfidence,
    source: String,
}

impl SavedFact {
    /// Construct a new fact with the given content, confidence, and source.
    ///
    /// This is the public constructor; the struct fields are kept private to
    /// maintain the encapsulation contract (all mutation is via methods).
    pub fn new(
        content: impl Into<String>,
        confidence: SavedFactConfidence,
        source: impl Into<String>,
    ) -> Self {
        Self {
            content: content.into(),
            confidence,
            source: source.into(),
        }
    }

    pub fn content(&self) -> &str {
        &self.content
    }

    pub fn confidence(&self) -> SavedFactConfidence {
        self.confidence
    }

    pub fn source(&self) -> &str {
        &self.source
    }
}

// ---------------------------------------------------------------------------
// GoalSnapshot — the serialisable TaskLedger
// ---------------------------------------------------------------------------

/// Serialisable snapshot of a `TaskLedger`.
///
/// `Instant`-based timestamps are not serialisable; we record unix epoch
/// seconds for created_at / last_replan_at instead.
///
/// All fields are private; use the constructor and accessor methods.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalSnapshot {
    goal: String,
    facts: Vec<SavedFact>,
    plan: Vec<SavedPlanStep>,
    /// Plan history is preserved so replan context remains available.
    plan_history: Vec<Vec<SavedPlanStep>>,
    stall_counter: u32,
    stall_threshold: u32,
    replan_count: u32,
    max_replans: u32,
    created_at_secs: u64,
    last_replan_at_secs: Option<u64>,
}

impl GoalSnapshot {
    // -- Constructor ---------------------------------------------------------

    /// Build a snapshot from the raw ledger fields.
    ///
    /// Callers in `phantom-brain` can call this directly after extracting
    /// the values they need.  The `active → pending` demotion is applied
    /// at restore time (see `restore_status()`), not at save time, so the
    /// saved record faithfully records what was running.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        goal: String,
        facts: Vec<SavedFact>,
        plan: Vec<SavedPlanStep>,
        plan_history: Vec<Vec<SavedPlanStep>>,
        stall_counter: u32,
        stall_threshold: u32,
        replan_count: u32,
        max_replans: u32,
        created_at_secs: u64,
        last_replan_at_secs: Option<u64>,
    ) -> Self {
        Self {
            goal,
            facts,
            plan,
            plan_history,
            stall_counter,
            stall_threshold,
            replan_count,
            max_replans,
            created_at_secs,
            last_replan_at_secs,
        }
    }

    // -- Accessors -----------------------------------------------------------

    pub fn goal(&self) -> &str {
        &self.goal
    }

    pub fn facts(&self) -> &[SavedFact] {
        &self.facts
    }

    pub fn plan(&self) -> &[SavedPlanStep] {
        &self.plan
    }

    pub fn plan_history(&self) -> &[Vec<SavedPlanStep>] {
        &self.plan_history
    }

    pub fn stall_counter(&self) -> u32 {
        self.stall_counter
    }

    pub fn stall_threshold(&self) -> u32 {
        self.stall_threshold
    }

    pub fn replan_count(&self) -> u32 {
        self.replan_count
    }

    pub fn max_replans(&self) -> u32 {
        self.max_replans
    }

    pub fn created_at_secs(&self) -> u64 {
        self.created_at_secs
    }

    pub fn last_replan_at_secs(&self) -> Option<u64> {
        self.last_replan_at_secs
    }

    // -- Derived helpers -----------------------------------------------------

    /// Count steps that are `Done` in the saved plan.
    ///
    /// The reconciler uses this to detect whether completed steps would be
    /// re-executed after restore (they should not).
    pub fn done_step_count(&self) -> usize {
        self.plan
            .iter()
            .filter(|s| s.status == SavedStepStatus::Done)
            .count()
    }

    /// Count steps that would be `Pending` after restore (includes `Active`
    /// steps that are demoted).
    pub fn pending_after_restore(&self) -> usize {
        self.plan
            .iter()
            .filter(|s| {
                matches!(
                    s.restore_status(),
                    SavedStepStatus::Pending
                )
            })
            .count()
    }
}

// ---------------------------------------------------------------------------
// GoalStateFile — the envelope saved to disk
// ---------------------------------------------------------------------------

/// Top-level JSON file holding all active goal snapshots.
///
/// Multiple goals can be active simultaneously (though the current brain only
/// supports one), so we store a `Vec<GoalSnapshot>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalStateFile {
    version: u32,
    saved_at: u64,
    goals: Vec<GoalSnapshot>,
}

impl GoalStateFile {
    /// Create a new file from a collection of goal snapshots.
    pub fn new(goals: Vec<GoalSnapshot>) -> Self {
        let saved_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Self {
            version: 1,
            saved_at,
            goals,
        }
    }

    // -- Accessors -----------------------------------------------------------

    pub fn version(&self) -> u32 {
        self.version
    }

    pub fn saved_at(&self) -> u64 {
        self.saved_at
    }

    pub fn goals(&self) -> &[GoalSnapshot] {
        &self.goals
    }

    pub fn goal_count(&self) -> usize {
        self.goals.len()
    }

    // -- I/O -----------------------------------------------------------------

    /// Write to `path` atomically (write temp file, rename).
    pub fn save(&self, path: &Path) -> Result<()> {
        let json =
            serde_json::to_string_pretty(self).context("failed to serialize goal state")?;

        // Atomic write: temp file in same directory, then rename.
        let parent = path.parent().unwrap_or(Path::new("."));
        let tmp = parent.join(format!(
            ".goals_tmp_{}.json",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));

        fs::write(&tmp, &json)
            .with_context(|| format!("failed to write goal state temp: {}", tmp.display()))?;

        fs::rename(&tmp, path)
            .with_context(|| format!("failed to rename goal state to: {}", path.display()))
    }

    /// Load from `path`.  Returns `None` if the file does not exist.
    pub fn load(path: &Path) -> Result<Option<Self>> {
        match fs::read_to_string(path) {
            Ok(contents) => {
                let file: Self = serde_json::from_str(&contents)
                    .with_context(|| format!("failed to parse goal state: {}", path.display()))?;
                Ok(Some(file))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => {
                Err(e).with_context(|| format!("failed to read goal state: {}", path.display()))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// GoalStatePersister
// ---------------------------------------------------------------------------

/// High-level persistence helper for goal state.
///
/// Writes to the canonical `goals.json` path, or to a project-scoped sidecar
/// derived from the session file path.
pub struct GoalStatePersister {
    path: PathBuf,
}

impl GoalStatePersister {
    /// Return the canonical global goal-state path:
    /// `~/.local/share/phantom/goals.json`.
    pub fn default_path() -> PathBuf {
        if let Ok(home) = std::env::var("HOME") {
            PathBuf::from(home)
                .join(".local")
                .join("share")
                .join("phantom")
                .join("goals.json")
        } else {
            PathBuf::from("goals.json")
        }
    }

    /// Derive a project-scoped sidecar path from a session file path.
    ///
    /// Given `{hash}_{ts}.json`, returns `{hash}_{ts}_goals.json`.
    pub fn sidecar_path(session_path: &Path) -> PathBuf {
        let stem = session_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("session");
        let name = format!("{stem}_goals.json");
        session_path
            .parent()
            .unwrap_or(Path::new("."))
            .join(name)
    }

    /// Create a persister targeting `path`.
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Save a collection of goal snapshots.
    pub fn save_goals(&self, goals: Vec<GoalSnapshot>) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create goal state directory: {}", parent.display())
            })?;
        }
        let file = GoalStateFile::new(goals);
        file.save(&self.path)
    }

    /// Load the saved goal state.  Returns `None` if no file exists.
    pub fn load(&self) -> Result<Option<GoalStateFile>> {
        GoalStateFile::load(&self.path)
    }

    /// Delete the file (called when user declines restore or reconciler
    /// finishes all goals).
    pub fn discard(&self) -> Result<()> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| {
                format!("failed to delete goal state: {}", self.path.display())
            }),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

// ---------------------------------------------------------------------------
// SavedPlanStep builder — convenient constructor
// ---------------------------------------------------------------------------

/// Builder for `SavedPlanStep` used in tests and integration code.
pub struct PlanStepBuilder {
    description: String,
    task: AgentTask,
    status: SavedStepStatus,
    agent_id: Option<u32>,
    attempts: u32,
    max_attempts: u32,
    result_summary: Option<String>,
}

impl PlanStepBuilder {
    pub fn new(description: impl Into<String>, task: AgentTask) -> Self {
        Self {
            description: description.into(),
            task,
            status: SavedStepStatus::Pending,
            agent_id: None,
            attempts: 0,
            max_attempts: 3,
            result_summary: None,
        }
    }

    pub fn status(mut self, status: SavedStepStatus) -> Self {
        self.status = status;
        self
    }

    pub fn agent_id(mut self, id: u32) -> Self {
        self.agent_id = Some(id);
        self
    }

    pub fn attempts(mut self, n: u32) -> Self {
        self.attempts = n;
        self
    }

    pub fn max_attempts(mut self, n: u32) -> Self {
        self.max_attempts = n;
        self
    }

    pub fn result_summary(mut self, s: impl Into<String>) -> Self {
        self.result_summary = Some(s.into());
        self
    }

    pub fn build(self) -> SavedPlanStep {
        SavedPlanStep::from_raw(
            self.description,
            self.task,
            self.status,
            self.agent_id,
            self.attempts,
            self.max_attempts,
            self.result_summary,
        )
    }
}

// ---------------------------------------------------------------------------
// Partial-restore helpers
// ---------------------------------------------------------------------------

/// Outcome of attempting to restore a single goal from a snapshot.
#[derive(Debug)]
pub enum GoalRestoreOutcome {
    Ok(GoalSnapshot),
    /// The snapshot could not be used; reason explains why.
    Skipped { goal: String, reason: String },
    /// The whole file is corrupt.
    Corrupt { reason: String },
}

/// Attempt to restore goals from a saved file, tolerating per-goal failures.
pub fn partial_restore_goals(file: &GoalStateFile) -> Vec<GoalRestoreOutcome> {
    if file.version() < 1 {
        return vec![GoalRestoreOutcome::Corrupt {
            reason: format!("unsupported version {}", file.version()),
        }];
    }

    file.goals()
        .iter()
        .map(|snap| {
            // Validate: each step's task must round-trip through serde.
            let bad_step = snap.plan().iter().find(|s| {
                serde_json::to_string(&s.assigned_task).is_err()
            });

            if let Some(step) = bad_step {
                return GoalRestoreOutcome::Skipped {
                    goal: snap.goal().to_owned(),
                    reason: format!("step '{}' has unserializable task", step.description()),
                };
            }

            GoalRestoreOutcome::Ok(snap.clone())
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use phantom_agents::agent::AgentTask;
    use tempfile::TempDir;

    fn free_task(prompt: &str) -> AgentTask {
        AgentTask::FreeForm { prompt: prompt.into() }
    }

    fn pending_step(desc: &str) -> SavedPlanStep {
        PlanStepBuilder::new(desc, free_task(desc)).build()
    }

    fn done_step(desc: &str) -> SavedPlanStep {
        PlanStepBuilder::new(desc, free_task(desc))
            .status(SavedStepStatus::Done)
            .result_summary("completed")
            .build()
    }

    fn active_step(desc: &str) -> SavedPlanStep {
        PlanStepBuilder::new(desc, free_task(desc))
            .status(SavedStepStatus::Active)
            .agent_id(100)
            .attempts(1)
            .build()
    }

    fn failed_step(desc: &str) -> SavedPlanStep {
        PlanStepBuilder::new(desc, free_task(desc))
            .status(SavedStepStatus::Failed)
            .attempts(3)
            .max_attempts(3)
            .result_summary("exhausted retries")
            .build()
    }

    fn sample_snapshot(goal: &str) -> GoalSnapshot {
        GoalSnapshot::new(
            goal.to_owned(),
            vec![SavedFact::new(
                "error in main.rs",
                SavedFactConfidence::Verified,
                "agent-1",
            )],
            vec![
                done_step("read the file"),
                active_step("fix the error"),
                pending_step("run tests"),
            ],
            vec![],
            0,
            2,
            0,
            5,
            1_700_000_000,
            None,
        )
    }

    // -- SavedStepStatus conflict policy ------------------------------------

    #[test]
    fn active_is_demoted_to_pending_on_restore() {
        let step = active_step("running step");
        assert_eq!(step.status(), SavedStepStatus::Active);
        assert_eq!(step.restore_status(), SavedStepStatus::Pending,
            "Active must become Pending on restore");
    }

    #[test]
    fn done_is_preserved_on_restore() {
        let step = done_step("finished step");
        assert_eq!(step.restore_status(), SavedStepStatus::Done);
    }

    #[test]
    fn failed_is_preserved_on_restore() {
        let step = failed_step("failed step");
        assert_eq!(step.restore_status(), SavedStepStatus::Failed);
    }

    #[test]
    fn pending_is_preserved_on_restore() {
        let step = pending_step("pending step");
        assert_eq!(step.restore_status(), SavedStepStatus::Pending);
    }

    // -- GoalSnapshot ----------------------------------------------------------

    #[test]
    fn done_step_count_correct() {
        let snap = sample_snapshot("fix build");
        assert_eq!(snap.done_step_count(), 1);
    }

    #[test]
    fn pending_after_restore_includes_demoted_active() {
        let snap = sample_snapshot("fix build");
        // done_step → Done, active_step → Pending (demoted), pending_step → Pending
        // So pending_after_restore should be 2.
        assert_eq!(snap.pending_after_restore(), 2);
    }

    #[test]
    fn snapshot_accessors_correct() {
        let snap = sample_snapshot("the goal");
        assert_eq!(snap.goal(), "the goal");
        assert_eq!(snap.plan().len(), 3);
        assert_eq!(snap.facts().len(), 1);
        assert_eq!(snap.stall_threshold(), 2);
        assert_eq!(snap.max_replans(), 5);
    }

    // -- GoalStateFile save/load round-trip -----------------------------------

    #[test]
    fn save_and_load_round_trip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("goals.json");

        let snap1 = sample_snapshot("fix build errors");
        let snap2 = sample_snapshot("add new feature");
        let file = GoalStateFile::new(vec![snap1, snap2]);
        file.save(&path).unwrap();

        let loaded = GoalStateFile::load(&path).unwrap().unwrap();
        assert_eq!(loaded.version(), 1);
        assert_eq!(loaded.goal_count(), 2);
        assert_eq!(loaded.goals()[0].goal(), "fix build errors");
        assert_eq!(loaded.goals()[1].goal(), "add new feature");
    }

    #[test]
    fn load_missing_returns_none() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("goals.json");
        let result = GoalStateFile::load(&path).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn load_corrupt_returns_error() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("goals.json");
        fs::write(&path, "{{ not json").unwrap();
        assert!(GoalStateFile::load(&path).is_err());
    }

    #[test]
    fn atomic_write_creates_no_temp_file_after_success() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("goals.json");
        let file = GoalStateFile::new(vec![sample_snapshot("g")]);
        file.save(&path).unwrap();

        // The final file must exist.
        assert!(path.exists());
        // No `.goals_tmp_*.json` should remain.
        let temps: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.starts_with(".goals_tmp_"))
            })
            .collect();
        assert!(temps.is_empty(), "temp files must be cleaned up");
    }

    // -- GoalStatePersister ---------------------------------------------------

    #[test]
    fn sidecar_path_derived_from_session_path() {
        let session = PathBuf::from("/sessions/abc123_1700000000.json");
        let sidecar = GoalStatePersister::sidecar_path(&session);
        assert_eq!(
            sidecar,
            PathBuf::from("/sessions/abc123_1700000000_goals.json")
        );
    }

    #[test]
    fn persister_save_and_load() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("goals.json");
        let persister = GoalStatePersister::new(path.clone());

        persister.save_goals(vec![sample_snapshot("test goal")]).unwrap();

        let loaded = persister.load().unwrap().unwrap();
        assert_eq!(loaded.goal_count(), 1);
        assert_eq!(loaded.goals()[0].goal(), "test goal");
    }

    #[test]
    fn persister_discard_removes_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("goals.json");
        fs::write(&path, "{}").unwrap();
        let persister = GoalStatePersister::new(path.clone());
        persister.discard().unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn persister_discard_nonexistent_is_ok() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nonexistent_goals.json");
        let persister = GoalStatePersister::new(path);
        persister.discard().unwrap();
    }

    // -- partial_restore_goals ------------------------------------------------

    #[test]
    fn partial_restore_all_valid() {
        let file = GoalStateFile::new(vec![
            sample_snapshot("goal 1"),
            sample_snapshot("goal 2"),
        ]);
        let outcomes = partial_restore_goals(&file);
        assert_eq!(outcomes.len(), 2);
        assert!(outcomes.iter().all(|o| matches!(o, GoalRestoreOutcome::Ok(_))));
    }

    #[test]
    fn partial_restore_version_zero_is_corrupt() {
        let file = GoalStateFile {
            version: 0,
            saved_at: 0,
            goals: vec![sample_snapshot("g")],
        };
        let outcomes = partial_restore_goals(&file);
        assert_eq!(outcomes.len(), 1);
        assert!(matches!(outcomes[0], GoalRestoreOutcome::Corrupt { .. }));
    }

    #[test]
    fn partial_restore_empty_file_returns_empty() {
        let file = GoalStateFile::new(vec![]);
        let outcomes = partial_restore_goals(&file);
        assert!(outcomes.is_empty());
    }

    // -- Done steps not re-executed -------------------------------------------

    #[test]
    fn done_steps_remain_done_after_restore() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("goals.json");

        let snap = GoalSnapshot::new(
            "ship feature".into(),
            vec![],
            vec![
                done_step("write tests"),
                done_step("implement feature"),
                pending_step("deploy"),
            ],
            vec![],
            0, 2, 0, 5,
            1_700_000_000,
            None,
        );

        let file = GoalStateFile::new(vec![snap]);
        file.save(&path).unwrap();

        let loaded = GoalStateFile::load(&path).unwrap().unwrap();
        let restored_snap = &loaded.goals()[0];

        // Done steps must remain Done (not re-executed).
        assert_eq!(restored_snap.plan()[0].restore_status(), SavedStepStatus::Done);
        assert_eq!(restored_snap.plan()[1].restore_status(), SavedStepStatus::Done);
        // Pending step is still Pending.
        assert_eq!(restored_snap.plan()[2].restore_status(), SavedStepStatus::Pending);
    }

    #[test]
    fn active_step_demoted_on_restore() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("goals.json");

        let snap = GoalSnapshot::new(
            "mid-execution goal".into(),
            vec![],
            vec![
                done_step("step 1"),
                active_step("step 2 — was running at shutdown"),
                pending_step("step 3"),
            ],
            vec![],
            0, 2, 1, 5,
            1_700_000_000,
            Some(1_700_001_000),
        );

        let file = GoalStateFile::new(vec![snap]);
        file.save(&path).unwrap();

        let loaded = GoalStateFile::load(&path).unwrap().unwrap();
        let step = &loaded.goals()[0].plan()[1];

        assert_eq!(step.status(), SavedStepStatus::Active,
            "saved status must faithfully record Active");
        assert_eq!(step.restore_status(), SavedStepStatus::Pending,
            "but restore must demote Active → Pending (retry policy)");
    }

    // -- Plan history preservation --------------------------------------------

    #[test]
    fn plan_history_is_preserved() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("goals.json");

        let old_plan = vec![failed_step("old approach")];
        let snap = GoalSnapshot::new(
            "replan goal".into(),
            vec![],
            vec![pending_step("new approach")],
            vec![old_plan],
            0, 2, 1, 5,
            1_700_000_000,
            None,
        );

        let file = GoalStateFile::new(vec![snap]);
        file.save(&path).unwrap();

        let loaded = GoalStateFile::load(&path).unwrap().unwrap();
        let restored = &loaded.goals()[0];

        assert_eq!(restored.replan_count(), 1);
        assert_eq!(restored.plan_history().len(), 1);
        assert_eq!(restored.plan_history()[0][0].description(), "old approach");
        assert_eq!(
            restored.plan_history()[0][0].status(),
            SavedStepStatus::Failed
        );
    }
}
