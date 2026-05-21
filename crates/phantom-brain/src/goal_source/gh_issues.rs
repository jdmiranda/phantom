//! GitHub-issue [`GoalSource`][super::GoalSource] implementation.
//!
//! Shells out to the user-installed `gh` CLI to enumerate open issues in a
//! repository, populates the standard `signals` bundle from §2.2 of the
//! design doc, and dedupes against an in-memory `seen` set so the same
//! issue does not re-emit across polls.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, SystemTime};

use serde::Deserialize;

use super::{
    gh_parse_iso8601, GhBinaryRunner, GhCommandRunner, GoalCandidate, GoalSource, GoalSourceError,
};

/// One row from `gh issue list --json number,title,body,labels,createdAt,url,author,comments`.
#[derive(Debug, Clone, Deserialize)]
struct GhIssue {
    number: u64,
    title: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    labels: Vec<GhIssueLabel>,
    #[serde(rename = "createdAt", default)]
    created_at: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    author: Option<GhIssueAuthor>,
    /// `gh` returns the comment list itself; we only need the count for the
    /// activity signal so this collects the array length.
    #[serde(default)]
    comments: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
struct GhIssueLabel {
    name: String,
}

#[derive(Debug, Clone, Deserialize)]
struct GhIssueAuthor {
    #[serde(default)]
    login: String,
}

/// [`GoalSource`] that polls `gh issue list` for autonomous goal discovery.
///
/// Dedupes against an in-memory `HashSet<u64>` of yielded issue numbers; the
/// same open issue is emitted exactly once across the lifetime of the source.
/// Caching is opt-in via the `poll_interval` — set it to `Duration::ZERO` to
/// force a fresh shell-out on every `poll()` (the integration tests use this
/// mode).
pub struct GhIssueGoalSource {
    repo: String,
    label_filter: Option<String>,
    poll_interval: Duration,
    last_polled: Option<std::time::Instant>,
    cache: Vec<GhIssue>,
    seen: HashSet<u64>,
    runner: Box<dyn GhCommandRunner>,
}

impl std::fmt::Debug for GhIssueGoalSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GhIssueGoalSource")
            .field("repo", &self.repo)
            .field("label_filter", &self.label_filter)
            .field("poll_interval", &self.poll_interval)
            .field("seen_count", &self.seen.len())
            .field("cached_count", &self.cache.len())
            .finish()
    }
}

impl GhIssueGoalSource {
    /// JSON fields requested from `gh issue list`. Kept in lock-step with the
    /// [`GhIssue`] deserializer above.
    const JSON_FIELDS: &'static str = "number,title,body,labels,createdAt,url,author,comments";

    /// Default poll interval (60 s). Matches the §4.2 default in the design doc.
    pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(60);

    /// Construct a source pointing at `repo` (e.g. `"jdmiranda/phantom"`),
    /// using the real `gh` binary.
    #[must_use]
    pub fn new(repo: impl Into<String>, label_filter: Option<String>) -> Self {
        Self::with_runner(
            repo,
            label_filter,
            Self::DEFAULT_POLL_INTERVAL,
            Box::new(GhBinaryRunner),
        )
    }

    /// Construct a source with an injected command runner — used by tests to
    /// supply canned JSON without invoking the real `gh` binary.
    pub fn with_runner(
        repo: impl Into<String>,
        label_filter: Option<String>,
        poll_interval: Duration,
        runner: Box<dyn GhCommandRunner>,
    ) -> Self {
        Self {
            repo: repo.into(),
            label_filter,
            poll_interval,
            last_polled: None,
            cache: Vec::new(),
            seen: HashSet::new(),
            runner,
        }
    }

    /// Build the `gh` argument vector for the configured filters.
    fn build_args(&self) -> Vec<String> {
        let mut args: Vec<String> = vec![
            "issue".into(),
            "list".into(),
            "-R".into(),
            self.repo.clone(),
            "--state".into(),
            "open".into(),
            "--json".into(),
            Self::JSON_FIELDS.into(),
            "--limit".into(),
            "100".into(),
        ];
        if let Some(label) = &self.label_filter {
            args.push("--label".into());
            args.push(label.clone());
        }
        args
    }

    /// Map a label name to a `priority_rank` per the design doc §2.2 table.
    fn priority_rank_for(label: &str) -> u8 {
        match label {
            "priority:critical" | "P0" | "critical" => 4,
            "priority:high" | "P1" | "high" => 3,
            "priority:medium" | "P2" | "medium" => 2,
            "priority:low" | "P3" | "low" => 1,
            "wontfix" | "discussion" => 0,
            _ => 0,
        }
    }

    /// Build the `signals` bundle for one issue.
    fn signals_from_issue(issue: &GhIssue) -> HashMap<String, f64> {
        let mut s = HashMap::new();

        // priority_rank: take the max across all labels.
        let mut priority: u8 = 0;
        let mut is_security = false;
        let mut is_blocker = false;
        let mut has_good_first = false;
        let mut has_needs_spec = false;
        let mut has_do_not_implement = false;
        let mut has_needs_discussion = false;
        let mut has_regression = false;
        for label in &issue.labels {
            priority = priority.max(Self::priority_rank_for(&label.name));
            if label.name.eq_ignore_ascii_case("security") || label.name.contains("security") {
                is_security = true;
            }
            if label.name.eq_ignore_ascii_case("blocker") {
                is_blocker = true;
            }
            if label.name == "good-first-issue" || label.name == "good first issue" {
                has_good_first = true;
            }
            if label.name == "needs-spec" {
                has_needs_spec = true;
            }
            if label.name == "do-not-auto-implement" {
                has_do_not_implement = true;
            }
            if label.name == "needs-discussion" {
                has_needs_discussion = true;
            }
            if label.name == "regression" {
                has_regression = true;
            }
        }
        // Default rank when no priority label is present: 2 (medium) per §2.2.
        if priority == 0
            && !issue.labels.iter().any(|l| {
                matches!(
                    l.name.as_str(),
                    "wontfix" | "discussion" | "do-not-auto-implement" | "needs-discussion",
                )
            })
        {
            priority = 2;
        }
        s.insert("priority_rank".into(), f64::from(priority));

        // age_hours: now - created_at. If `created_at` is in the future the
        // duration_since() Err branch yields 0.0 — fine for fresh-created records.
        let created = gh_parse_iso8601(&issue.created_at);
        let age_hours = SystemTime::now()
            .duration_since(created)
            .map(|d| d.as_secs_f64() / 3600.0)
            .unwrap_or(0.0);
        s.insert("age_hours".into(), age_hours);

        // activity_count: number of comments.
        s.insert("activity_count".into(), issue.comments.len() as f64);

        // blocked_by_count: scan body for "blocked by #N" references.
        let blocked = count_blocked_by_refs(&issue.body);
        s.insert("blocked_by_count".into(), f64::from(blocked));

        // recent_ci_failure_count: initialized to 0; cross-source enrichment
        // can set this from the CI source.
        s.insert("recent_ci_failure_count".into(), 0.0);

        // Hard-exclusion flags surfaced as 0/1 signals for the audit log.
        s.insert("is_security".into(), if is_security { 1.0 } else { 0.0 });
        s.insert("is_blocker".into(), if is_blocker { 1.0 } else { 0.0 });
        s.insert(
            "has_good_first_issue".into(),
            if has_good_first { 1.0 } else { 0.0 },
        );
        s.insert(
            "has_needs_spec".into(),
            if has_needs_spec { 1.0 } else { 0.0 },
        );
        s.insert(
            "has_do_not_auto_implement".into(),
            if has_do_not_implement { 1.0 } else { 0.0 },
        );
        s.insert(
            "has_needs_discussion".into(),
            if has_needs_discussion { 1.0 } else { 0.0 },
        );
        s.insert(
            "has_regression".into(),
            if has_regression { 1.0 } else { 0.0 },
        );

        s
    }

    /// Refresh the cache by shelling out. Returns the un-deduped issue list.
    fn refresh(&self) -> Result<Vec<GhIssue>, GoalSourceError> {
        let args = self.build_args();
        let stdout = self.runner.run(&args)?;
        serde_json::from_str::<Vec<GhIssue>>(&stdout)
            .map_err(|e| GoalSourceError::SchemaMismatch(format!("`gh issue list` JSON: {e}")))
    }
}

impl GoalSource for GhIssueGoalSource {
    fn name(&self) -> &str {
        "gh-issues"
    }

    fn poll(&mut self) -> Result<Vec<GoalCandidate>, GoalSourceError> {
        // Cache check: if we polled recently and the cache is non-empty,
        // reuse the cached issues (still applies dedup).
        let now = std::time::Instant::now();
        let fresh = match self.last_polled {
            Some(last) => now.duration_since(last) < self.poll_interval && !self.cache.is_empty(),
            None => false,
        };

        if !fresh {
            match self.refresh() {
                Ok(issues) => {
                    self.cache = issues;
                    self.last_polled = Some(now);
                }
                Err(e) => return Err(e),
            }
        }

        let mut candidates = Vec::new();
        for issue in &self.cache {
            if self.seen.contains(&issue.number) {
                continue;
            }
            self.seen.insert(issue.number);

            let signals = Self::signals_from_issue(issue);
            let labels: Vec<String> = issue.labels.iter().map(|l| l.name.clone()).collect();
            let author = issue.author.as_ref().map(|a| a.login.clone());
            let created_at = gh_parse_iso8601(&issue.created_at);

            candidates.push(GoalCandidate {
                source: self.name().to_string(),
                external_id: format!("gh-issue:{}", issue.number),
                title: issue.title.clone(),
                body: if issue.body.is_empty() {
                    None
                } else {
                    Some(issue.body.clone())
                },
                labels,
                created_at,
                signals,
                url: if issue.url.is_empty() {
                    None
                } else {
                    Some(issue.url.clone())
                },
                author,
            });
        }

        Ok(candidates)
    }
}

/// Scan an issue body for "blocked by #N" / "depends on #N" markdown
/// references. Returns the count (capped at `u32::MAX`).
fn count_blocked_by_refs(body: &str) -> u32 {
    let lower = body.to_ascii_lowercase();
    let mut count: u32 = 0;
    for needle in ["blocked by #", "depends on #", "blocked-by #", "blockedby #"] {
        count = count.saturating_add(lower.matches(needle).count() as u32);
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::goal_source::StubGhRunner;

    fn one_issue_json(
        number: u64,
        labels: &[&str],
        body: &str,
        author: &str,
        comments: usize,
    ) -> String {
        let labels_json: Vec<String> = labels
            .iter()
            .map(|n| format!(r#"{{"name":"{n}"}}"#))
            .collect();
        let comments_json = std::iter::repeat("{}")
            .take(comments)
            .collect::<Vec<_>>()
            .join(",");
        format!(
            r#"[{{"number":{number},"title":"t{number}","body":"{body}","labels":[{}],"createdAt":"2026-01-01T00:00:00Z","url":"http://example/{number}","author":{{"login":"{author}"}},"comments":[{comments_json}]}}]"#,
            labels_json.join(",")
        )
    }

    #[test]
    fn parses_minimal_issue_json() {
        let stub =
            StubGhRunner::new(vec![one_issue_json(1, &["priority:high"], "", "user", 0)]);
        let mut src = GhIssueGoalSource::with_runner(
            "owner/repo",
            None,
            Duration::ZERO,
            Box::new(stub),
        );
        let cands = src.poll().unwrap();
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].external_id, "gh-issue:1");
        assert_eq!(cands[0].title, "t1");
        assert!(cands[0].labels.contains(&"priority:high".to_string()));
        assert!((cands[0].signal("priority_rank") - 3.0).abs() < f64::EPSILON);
    }

    #[test]
    fn dedupe_skips_already_seen_external_id() {
        let stub = StubGhRunner::new(vec![
            one_issue_json(1, &["priority:medium"], "", "user", 0),
            one_issue_json(1, &["priority:medium"], "", "user", 0),
        ]);
        let mut src = GhIssueGoalSource::with_runner(
            "owner/repo",
            None,
            Duration::ZERO,
            Box::new(stub),
        );
        let first = src.poll().unwrap();
        let second = src.poll().unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 0); // dedup'd
    }

    #[test]
    fn label_filter_appears_in_gh_args() {
        let calls: std::sync::Arc<std::sync::Mutex<Vec<Vec<String>>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        struct Recorder(std::sync::Arc<std::sync::Mutex<Vec<Vec<String>>>>);
        impl GhCommandRunner for Recorder {
            fn run(&self, args: &[String]) -> Result<String, GoalSourceError> {
                if let Ok(mut c) = self.0.lock() {
                    c.push(args.to_vec());
                }
                Ok("[]".into())
            }
        }
        let recorder = Recorder(calls.clone());
        let mut src = GhIssueGoalSource::with_runner(
            "owner/repo",
            Some("priority:high".into()),
            Duration::ZERO,
            Box::new(recorder),
        );
        let _ = src.poll();
        let recorded = calls.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        let args = &recorded[0];
        assert!(args.contains(&"--label".to_string()));
        assert!(args.contains(&"priority:high".to_string()));
    }

    #[test]
    fn transport_failure_surfaces_as_error() {
        struct Failing;
        impl GhCommandRunner for Failing {
            fn run(&self, _: &[String]) -> Result<String, GoalSourceError> {
                Err(GoalSourceError::DependencyUnavailable("no gh".into()))
            }
        }
        let mut src = GhIssueGoalSource::with_runner(
            "owner/repo",
            None,
            Duration::ZERO,
            Box::new(Failing),
        );
        let err = src.poll().unwrap_err();
        assert!(matches!(err, GoalSourceError::DependencyUnavailable(_)));
    }

    #[test]
    fn security_label_sets_is_security_signal() {
        let stub = StubGhRunner::new(vec![one_issue_json(42, &["security"], "", "user", 0)]);
        let mut src = GhIssueGoalSource::with_runner(
            "owner/repo",
            None,
            Duration::ZERO,
            Box::new(stub),
        );
        let cands = src.poll().unwrap();
        assert_eq!(cands.len(), 1);
        assert!((cands[0].signal("is_security") - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn blocked_by_count_scans_body() {
        let body = "This is blocked by #100 and depends on #101.";
        assert_eq!(count_blocked_by_refs(body), 2);
        assert_eq!(count_blocked_by_refs("no deps"), 0);
    }

    #[test]
    fn priority_rank_unknown_label_defaults_to_medium() {
        let stub = StubGhRunner::new(vec![one_issue_json(7, &["bug"], "", "user", 0)]);
        let mut src = GhIssueGoalSource::with_runner(
            "owner/repo",
            None,
            Duration::ZERO,
            Box::new(stub),
        );
        let cands = src.poll().unwrap();
        assert_eq!(cands.len(), 1);
        // unknown label → default rank 2 (medium).
        assert!((cands[0].signal("priority_rank") - 2.0).abs() < f64::EPSILON);
    }
}
