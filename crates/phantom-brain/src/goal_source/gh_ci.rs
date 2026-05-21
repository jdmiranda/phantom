//! CI-failure [`GoalSource`][super::GoalSource] implementation.
//!
//! Polls `gh run list --status failure` for recent failing workflow runs in
//! a repository. Each run that is younger than 24 hours emits one
//! [`GoalCandidate`][super::GoalCandidate] with `priority_rank = 3` (high) by
//! default and `priority_rank = 4` (critical) when the failed event was a
//! `push` to `main` — a regression-on-main signal per §2.3 of the design doc.
//!
//! # Dedupe
//!
//! Tracks `databaseId` in an in-memory `HashSet<u64>`. The same failed run
//! is emitted exactly once across the lifetime of the source.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, SystemTime};

use serde::Deserialize;

use super::{
    gh_parse_iso8601, GhBinaryRunner, GhCommandRunner, GoalCandidate, GoalSource, GoalSourceError,
};

/// One row from `gh run list --json …`. Fields kept to the minimum the
/// scorer needs.
#[derive(Debug, Clone, Deserialize)]
struct GhRun {
    #[serde(rename = "databaseId")]
    database_id: u64,
    #[serde(rename = "displayTitle", default)]
    display_title: String,
    #[serde(rename = "headSha", default)]
    head_sha: String,
    #[serde(rename = "workflowName", default)]
    workflow_name: String,
    #[serde(rename = "createdAt", default)]
    created_at: String,
    #[serde(default)]
    event: String,
    #[serde(default)]
    conclusion: String,
    #[serde(rename = "headBranch", default)]
    head_branch: String,
    #[serde(default)]
    url: String,
}

/// [`GoalSource`] that polls `gh run list` for failing CI runs.
pub struct GhCiFailureGoalSource {
    repo: String,
    workflow_filter: Option<String>,
    poll_interval: Duration,
    last_polled: Option<std::time::Instant>,
    cache: Vec<GhRun>,
    seen: HashSet<u64>,
    runner: Box<dyn GhCommandRunner>,
    /// Maximum age (hours) of a run that the source still surfaces.
    /// Runs older than this are skipped — stale failures are not actionable.
    max_age_hours: f64,
}

impl std::fmt::Debug for GhCiFailureGoalSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GhCiFailureGoalSource")
            .field("repo", &self.repo)
            .field("workflow_filter", &self.workflow_filter)
            .field("poll_interval", &self.poll_interval)
            .field("max_age_hours", &self.max_age_hours)
            .field("seen_count", &self.seen.len())
            .field("cached_count", &self.cache.len())
            .finish()
    }
}

impl GhCiFailureGoalSource {
    /// JSON fields requested from `gh run list`.
    const JSON_FIELDS: &'static str =
        "databaseId,displayTitle,headSha,workflowName,createdAt,event,conclusion,headBranch,url";

    /// Default poll interval (60 s).
    pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(60);
    /// Default max age for surfaced CI failures (24 h, per design doc §2.3).
    pub const DEFAULT_MAX_AGE_HOURS: f64 = 24.0;

    /// Construct a source pointing at `repo`, using the real `gh` binary.
    #[must_use]
    pub fn new(repo: impl Into<String>, workflow_filter: Option<String>) -> Self {
        Self::with_runner(
            repo,
            workflow_filter,
            Self::DEFAULT_POLL_INTERVAL,
            Self::DEFAULT_MAX_AGE_HOURS,
            Box::new(GhBinaryRunner),
        )
    }

    /// Test-injection constructor.
    pub fn with_runner(
        repo: impl Into<String>,
        workflow_filter: Option<String>,
        poll_interval: Duration,
        max_age_hours: f64,
        runner: Box<dyn GhCommandRunner>,
    ) -> Self {
        Self {
            repo: repo.into(),
            workflow_filter,
            poll_interval,
            last_polled: None,
            cache: Vec::new(),
            seen: HashSet::new(),
            runner,
            max_age_hours,
        }
    }

    fn build_args(&self) -> Vec<String> {
        let mut args: Vec<String> = vec![
            "run".into(),
            "list".into(),
            "-R".into(),
            self.repo.clone(),
            "--status".into(),
            "failure".into(),
            "--json".into(),
            Self::JSON_FIELDS.into(),
            "--limit".into(),
            "25".into(),
        ];
        if let Some(wf) = &self.workflow_filter {
            args.push("--workflow".into());
            args.push(wf.clone());
        }
        args
    }

    fn refresh(&self) -> Result<Vec<GhRun>, GoalSourceError> {
        let args = self.build_args();
        let stdout = self.runner.run(&args)?;
        serde_json::from_str::<Vec<GhRun>>(&stdout)
            .map_err(|e| GoalSourceError::SchemaMismatch(format!("`gh run list` JSON: {e}")))
    }

    fn run_to_candidate(&self, run: &GhRun, age_hours: f64) -> GoalCandidate {
        let mut signals: HashMap<String, f64> = HashMap::new();

        // Priority: high (3) by default; critical (4) when push-on-main.
        let is_main = matches!(
            run.head_branch.as_str(),
            "main" | "master" | "trunk" | "develop",
        );
        let is_push = run.event == "push";
        let priority = if is_push && is_main { 4u8 } else { 3u8 };
        signals.insert("priority_rank".into(), f64::from(priority));
        signals.insert("age_hours".into(), age_hours);
        signals.insert("activity_count".into(), 0.0);
        signals.insert("blocked_by_count".into(), 0.0);
        signals.insert("recent_ci_failure_count".into(), 1.0);
        signals.insert("is_security".into(), 0.0);
        signals.insert("has_good_first_issue".into(), 0.0);
        signals.insert("has_needs_spec".into(), 0.0);
        signals.insert("has_do_not_auto_implement".into(), 0.0);
        signals.insert("has_needs_discussion".into(), 0.0);
        signals.insert(
            "is_blocker".into(),
            if is_push && is_main { 1.0 } else { 0.0 },
        );
        signals.insert(
            "has_regression".into(),
            if is_push && is_main { 1.0 } else { 0.0 },
        );

        let labels = vec!["ci-failure".to_string(), run.workflow_name.clone()];
        let body = format!(
            "CI failure on workflow `{}` for `{}` (sha {}, branch {}, event {}).\n\nSee {}.",
            run.workflow_name, run.display_title, run.head_sha, run.head_branch, run.event, run.url,
        );

        GoalCandidate {
            source: self.name().to_string(),
            external_id: format!("gh-run:{}", run.database_id),
            title: format!("Fix CI: {}", run.display_title),
            body: Some(body),
            labels,
            created_at: gh_parse_iso8601(&run.created_at),
            signals,
            url: if run.url.is_empty() {
                None
            } else {
                Some(run.url.clone())
            },
            author: None,
        }
    }
}

impl GoalSource for GhCiFailureGoalSource {
    fn name(&self) -> &str {
        "gh-ci-failures"
    }

    fn poll(&mut self) -> Result<Vec<GoalCandidate>, GoalSourceError> {
        let now = std::time::Instant::now();
        let fresh = match self.last_polled {
            Some(last) => now.duration_since(last) < self.poll_interval && !self.cache.is_empty(),
            None => false,
        };

        if !fresh {
            match self.refresh() {
                Ok(runs) => {
                    self.cache = runs;
                    self.last_polled = Some(now);
                }
                Err(e) => return Err(e),
            }
        }

        let mut candidates = Vec::new();
        for run in &self.cache {
            if self.seen.contains(&run.database_id) {
                continue;
            }
            // Skip runs older than max_age_hours.
            let created = gh_parse_iso8601(&run.created_at);
            let age_hours = SystemTime::now()
                .duration_since(created)
                .map(|d| d.as_secs_f64() / 3600.0)
                .unwrap_or(0.0);
            if age_hours > self.max_age_hours {
                continue;
            }
            // Only failed conclusion is interesting.
            if !run.conclusion.is_empty() && run.conclusion != "failure" {
                continue;
            }

            self.seen.insert(run.database_id);
            candidates.push(self.run_to_candidate(run, age_hours));
        }

        Ok(candidates)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::goal_source::StubGhRunner;

    fn one_run_json(
        db_id: u64,
        title: &str,
        event: &str,
        head_branch: &str,
        conclusion: &str,
    ) -> String {
        format!(
            r#"[{{"databaseId":{db_id},"displayTitle":"{title}","headSha":"abc","workflowName":"ci","createdAt":"2099-01-01T00:00:00Z","event":"{event}","conclusion":"{conclusion}","headBranch":"{head_branch}","url":"http://x"}}]"#
        )
    }

    #[test]
    fn push_on_main_is_critical_priority() {
        // Future timestamp = age_hours will be negative-clamped to 0 (well within window).
        let stub = StubGhRunner::new(vec![one_run_json(1, "fix bug", "push", "main", "failure")]);
        let mut src = GhCiFailureGoalSource::with_runner(
            "owner/repo",
            None,
            Duration::ZERO,
            f64::INFINITY,
            Box::new(stub),
        );
        let cands = src.poll().unwrap();
        assert_eq!(cands.len(), 1);
        assert!((cands[0].signal("priority_rank") - 4.0).abs() < f64::EPSILON);
        assert!(cands[0].has_label("ci-failure"));
        assert_eq!(cands[0].external_id, "gh-run:1");
    }

    #[test]
    fn non_main_event_is_high_priority_not_critical() {
        let stub = StubGhRunner::new(vec![one_run_json(
            2,
            "fix bug",
            "pull_request",
            "feat/foo",
            "failure",
        )]);
        let mut src = GhCiFailureGoalSource::with_runner(
            "owner/repo",
            None,
            Duration::ZERO,
            f64::INFINITY,
            Box::new(stub),
        );
        let cands = src.poll().unwrap();
        assert!((cands[0].signal("priority_rank") - 3.0).abs() < f64::EPSILON);
    }

    #[test]
    fn dedupe_skips_already_seen_database_id() {
        let stub = StubGhRunner::new(vec![
            one_run_json(99, "t", "push", "main", "failure"),
            one_run_json(99, "t", "push", "main", "failure"),
        ]);
        let mut src = GhCiFailureGoalSource::with_runner(
            "owner/repo",
            None,
            Duration::ZERO,
            f64::INFINITY,
            Box::new(stub),
        );
        let first = src.poll().unwrap();
        let second = src.poll().unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 0);
    }

    #[test]
    fn stale_run_outside_age_window_is_dropped() {
        // age_hours = current_year - 1970 ≈ many millions of hours; default max is 24.
        let stub = StubGhRunner::new(vec![
            r#"[{"databaseId":1,"displayTitle":"old","headSha":"x","workflowName":"ci","createdAt":"1971-01-01T00:00:00Z","event":"push","conclusion":"failure","headBranch":"main","url":"http://x"}]"#
                .into(),
        ]);
        let mut src = GhCiFailureGoalSource::with_runner(
            "owner/repo",
            None,
            Duration::ZERO,
            24.0,
            Box::new(stub),
        );
        let cands = src.poll().unwrap();
        assert_eq!(cands.len(), 0); // dropped — too old
    }

    #[test]
    fn workflow_filter_passes_through() {
        let calls: std::sync::Arc<std::sync::Mutex<Vec<Vec<String>>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        struct R(std::sync::Arc<std::sync::Mutex<Vec<Vec<String>>>>);
        impl GhCommandRunner for R {
            fn run(&self, args: &[String]) -> Result<String, GoalSourceError> {
                if let Ok(mut c) = self.0.lock() {
                    c.push(args.to_vec());
                }
                Ok("[]".into())
            }
        }
        let mut src = GhCiFailureGoalSource::with_runner(
            "owner/repo",
            Some("ci.yml".into()),
            Duration::ZERO,
            24.0,
            Box::new(R(calls.clone())),
        );
        let _ = src.poll();
        let recorded = calls.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert!(recorded[0].contains(&"--workflow".into()));
        assert!(recorded[0].contains(&"ci.yml".into()));
    }
}
