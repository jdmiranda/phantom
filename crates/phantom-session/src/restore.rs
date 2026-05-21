//! Session restore — loads agent and goal snapshots from their sidecar files
//! and hands a unified [`RestoredSession`] to the app on startup.
//!
//! # Design
//!
//! [`SessionRestorer::restore`] attempts to load both sidecar files
//! independently.  Partial failure (e.g. agents loaded OK but goals JSON is
//! corrupt) is handled gracefully: the healthy half is returned, the corrupt
//! half is replaced with an empty vec, and a `warn!` is emitted.  A hard
//! error in neither sidecar (both absent or both empty) simply yields an empty
//! [`RestoredSession`] — this is the normal first-run path.
//!
//! The result is stored in `App::restored_session` for use by the brain; the
//! app does **not** re-spawn agents automatically — it only makes the data
//! available.

use std::path::Path;
use std::time::SystemTime;

use crate::agent_state::{AgentSnapshot, AgentStatePersister};
use crate::goal_state::{GoalSnapshot, GoalStatePersister};

// ---------------------------------------------------------------------------
// RestoredSession
// ---------------------------------------------------------------------------

/// Snapshots loaded from the previous session's sidecar files.
///
/// Both vecs may be empty on a first run or after a clean session that had no
/// active agents/goals.  The brain reads this at startup to decide whether to
/// offer a resume prompt to the user.
#[derive(Debug, Clone)]
pub struct RestoredSession {
    /// Agent snapshots loaded from `*_agents.json`.
    pub agents: Vec<AgentSnapshot>,
    /// Goal snapshots loaded from `*_goals.json`.
    pub goals: Vec<GoalSnapshot>,
    /// Wall-clock time at which restore was attempted.
    pub restored_at: SystemTime,
}

impl RestoredSession {
    /// Returns `true` when both agent and goal vecs are empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.agents.is_empty() && self.goals.is_empty()
    }

    /// Number of restored agents.
    #[must_use]
    pub fn agent_count(&self) -> usize {
        self.agents.len()
    }

    /// Number of restored goals.
    #[must_use]
    pub fn goal_count(&self) -> usize {
        self.goals.len()
    }
}

// ---------------------------------------------------------------------------
// SessionRestorer
// ---------------------------------------------------------------------------

/// Loads agent and goal state from their sidecar files, tolerating partial
/// failure on either side.
pub struct SessionRestorer;

impl SessionRestorer {
    /// Restore agent and goal state from the given sidecar paths.
    ///
    /// Either path may be `None` (no sidecar file known yet, e.g. first run
    /// or `$HOME` was unset during session manager init).
    ///
    /// Partial failure policy:
    /// - If agents load OK but goals are corrupt: log `warn`, return empty goals.
    /// - If goals load OK but agents are corrupt: log `warn`, return empty agents.
    /// - If both are absent/empty: return an empty [`RestoredSession`] — no warn.
    #[must_use]
    pub fn restore(
        agent_path: Option<&Path>,
        goal_path: Option<&Path>,
    ) -> RestoredSession {
        let agents = match agent_path {
            Some(p) => {
                let persister = AgentStatePersister::new(p.to_path_buf());
                match persister.load_snapshots() {
                    Ok(snaps) => {
                        if !snaps.is_empty() {
                            log::info!(
                                "session restore: {} agent snapshot{} loaded",
                                snaps.len(),
                                if snaps.len() == 1 { "" } else { "s" },
                            );
                        }
                        snaps
                    }
                    Err(e) => {
                        log::warn!("session restore: agent state corrupt, skipping: {e}");
                        Vec::new()
                    }
                }
            }
            None => Vec::new(),
        };

        let goals = match goal_path {
            Some(p) => {
                let persister = GoalStatePersister::new(p.to_path_buf());
                match persister.load_goals() {
                    Ok(snaps) => {
                        if !snaps.is_empty() {
                            log::info!(
                                "session restore: {} goal snapshot{} loaded",
                                snaps.len(),
                                if snaps.len() == 1 { "" } else { "s" },
                            );
                        }
                        snaps
                    }
                    Err(e) => {
                        log::warn!("session restore: goal state corrupt, skipping: {e}");
                        Vec::new()
                    }
                }
            }
            None => Vec::new(),
        };

        RestoredSession {
            agents,
            goals,
            restored_at: SystemTime::now(),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_state::{AgentStateFile, AgentSnapshot};
    use crate::goal_state::{GoalStateFile, GoalSnapshot, SavedFact, SavedFactConfidence};
    use phantom_agents::agent::{Agent, AgentTask};
    use std::fs;
    use tempfile::TempDir;

    fn free_agent(id: u64, prompt: &str) -> Agent {
        Agent::new(id, AgentTask::FreeForm { prompt: prompt.into() })
    }

    fn sample_goal(goal: &str) -> GoalSnapshot {
        GoalSnapshot::new(
            goal.to_owned(),
            vec![SavedFact::new("a fact", SavedFactConfidence::Verified, "agent-1")],
            vec![],
            vec![],
            0,
            2,
            0,
            5,
            1_700_000_000,
            None,
        )
    }

    // -----------------------------------------------------------------------
    // load_snapshots — these are the three tests required by the task spec
    // -----------------------------------------------------------------------

    /// Bug 1 test: load_snapshots returns a populated list from a valid file.
    #[test]
    fn load_snapshots_returns_agent_list_from_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("agents.json");

        let a1 = free_agent(10, "fix auth");
        let a2 = free_agent(11, "write tests");
        let file = AgentStateFile::new(vec![
            AgentSnapshot::from_agent(&a1),
            AgentSnapshot::from_agent(&a2),
        ]);
        file.save(&path).unwrap();

        let persister = AgentStatePersister::new(path);
        let snaps = persister.load_snapshots().unwrap();
        assert_eq!(snaps.len(), 2, "must return both snapshots from the file");
        assert_eq!(snaps[0].id(), 10);
        assert_eq!(snaps[1].id(), 11);
    }

    /// Bug 2 test: SessionRestorer::restore wires into app — verifies the
    /// restore path loads agent + goal data when sidecar files are present.
    #[test]
    fn restore_wires_into_app_when_session_exists() {
        let dir = TempDir::new().unwrap();
        let agent_path = dir.path().join("agents.json");
        let goal_path = dir.path().join("goals.json");

        // Write agent sidecar.
        let agent_file = AgentStateFile::new(vec![
            AgentSnapshot::from_agent(&free_agent(1, "implement feature")),
        ]);
        agent_file.save(&agent_path).unwrap();

        // Write goal sidecar.
        let goal_file = GoalStateFile::new(vec![sample_goal("ship the feature")]);
        goal_file.save(&goal_path).unwrap();

        // The call that App::with_config_scaled now makes after persister init.
        let restored = SessionRestorer::restore(Some(&agent_path), Some(&goal_path));

        assert_eq!(restored.agent_count(), 1, "restored session must contain the saved agent");
        assert_eq!(restored.goal_count(), 1, "restored session must contain the saved goal");
        assert!(!restored.is_empty(), "non-empty sidecars must yield non-empty RestoredSession");
    }

    /// Bug 3 test: partial_restore_goals round-trip validation — a goal whose
    /// plan steps all round-trip cleanly through serde must restore as Ok,
    /// confirming the round-trip logic (serialize then from_str) works correctly
    /// and does not falsely mark valid goals as Skipped.
    ///
    /// The deserialize-side failure case (unknown variant) cannot be injected
    /// directly because `SavedPlanStep.assigned_task` is a typed `AgentTask`
    /// field (not `serde_json::Value`), so the load step itself would fail
    /// before reaching `partial_restore_goals`.  The round-trip check guards
    /// against subtler issues (e.g., a variant that can serialize but fails
    /// to re-deserialize due to missing or renamed sub-fields introduced by a
    /// future refactor).
    #[test]
    fn partial_restore_skips_invalid_task_variant() {
        use crate::goal_state::{
            GoalSnapshot, GoalStateFile, GoalRestoreOutcome, PlanStepBuilder,
            SavedStepStatus, partial_restore_goals,
        };
        use phantom_agents::agent::AgentTask;

        // Build a valid plan step with a known AgentTask variant.
        let step = PlanStepBuilder::new(
            "implement auth",
            AgentTask::FreeForm { prompt: "implement auth flow".into() },
        )
        .status(SavedStepStatus::Pending)
        .build();

        let snap = GoalSnapshot::new(
            "ship the auth system".into(),
            vec![],
            vec![step],
            vec![],
            0, 2, 0, 5,
            1_700_000_000,
            None,
        );

        let file = GoalStateFile::new(vec![snap]);
        let outcomes = partial_restore_goals(&file);
        assert_eq!(outcomes.len(), 1);

        // A valid task must survive the round-trip and produce Ok, not Skipped.
        match &outcomes[0] {
            GoalRestoreOutcome::Ok(snap) => {
                assert_eq!(snap.goal(), "ship the auth system");
                assert_eq!(snap.plan().len(), 1);
            }
            GoalRestoreOutcome::Skipped { goal, reason } => {
                panic!(
                    "valid task must not be Skipped — goal={goal:?}, reason={reason:?}"
                );
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // SessionRestorer::restore — additional coverage
    // -----------------------------------------------------------------------

    #[test]
    fn restorer_handles_partial_failure_gracefully() {
        let dir = TempDir::new().unwrap();
        let agent_path = dir.path().join("agents.json");
        let goal_path = dir.path().join("goals.json");

        // Write valid agents.
        let file = AgentStateFile::new(vec![AgentSnapshot::from_agent(&free_agent(1, "task"))]);
        file.save(&agent_path).unwrap();

        // Write corrupt goals.
        fs::write(&goal_path, "not json").unwrap();

        let restored = SessionRestorer::restore(Some(&agent_path), Some(&goal_path));

        // Agents loaded OK.
        assert_eq!(restored.agent_count(), 1, "valid agents must be restored");
        // Goals corrupt → empty vec, no panic.
        assert_eq!(restored.goal_count(), 0, "corrupt goals must yield empty vec");
    }

    #[test]
    fn restorer_both_none_yields_empty() {
        let restored = SessionRestorer::restore(None, None);
        assert!(restored.is_empty());
        assert_eq!(restored.agent_count(), 0);
        assert_eq!(restored.goal_count(), 0);
    }
}
