//! GitHub-pull-request-backed loop source.
//!
//! [`GhPrReviewQueueSource`] is the PR-side mirror of
//! [`super::gh_issues::GhIssueQueueSource`]: it shells `gh pr list` against a
//! repo, applies a structured [`crate::source::GhPrPredicate`] filter, dedupes
//! against an in-memory `seen` set, and emits one PR per `next()` call.
//!
//! The dedup contract matches the issues source exactly — see that module's
//! header for the rationale.

use std::collections::HashSet;
use std::sync::Mutex;

use serde::Deserialize;
use serde_json::json;

use crate::runner::source::{
    CorrelationId, LoopContext, LoopInput, LoopPullResult, LoopSource, LoopSourceError,
};
use crate::source::{GhPrPredicate, GhPrState};
use crate::sources::gh_issues::{GhBinaryRunner, GhCommandRunner};

/// One row from `gh pr list --json …`.
///
/// The JSON shape is fixed by `gh`'s schema. We pull just enough to dedupe
/// and to build a useful [`LoopInput`] payload; downstream consumers that
/// need more fields can switch to a richer schema by extending
/// [`GhPrReviewQueueSource::JSON_FIELDS`].
#[derive(Debug, Clone, Deserialize)]
struct GhPr {
    number: u64,
    title: String,
    state: String,
    #[serde(default)]
    author: Option<GhPrAuthor>,
    #[serde(default)]
    labels: Vec<GhPrLabel>,
    #[serde(rename = "createdAt", default)]
    created_at: String,
    #[serde(default)]
    url: String,
}

#[derive(Debug, Clone, Deserialize)]
struct GhPrAuthor {
    #[serde(default)]
    login: String,
}

#[derive(Debug, Clone, Deserialize)]
struct GhPrLabel {
    name: String,
}

/// Source that pulls fresh PRs from a GitHub repository.
pub struct GhPrReviewQueueSource {
    repo: String,
    predicate: GhPrPredicate,
    seen: Mutex<HashSet<u64>>,
    pending: Mutex<Vec<GhPr>>,
    runner: Box<dyn GhCommandRunner>,
}

impl std::fmt::Debug for GhPrReviewQueueSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GhPrReviewQueueSource")
            .field("repo", &self.repo)
            .field("predicate", &self.predicate)
            .field("seen", &self.seen.lock().map(|s| s.len()).unwrap_or(0))
            .field("pending", &self.pending.lock().map(|p| p.len()).unwrap_or(0))
            .finish()
    }
}

impl GhPrReviewQueueSource {
    const JSON_FIELDS: &'static str = "number,title,state,author,labels,createdAt,url";

    /// Build a source against the real `gh` binary.
    #[must_use]
    pub fn new(repo: impl Into<String>, predicate: GhPrPredicate) -> Self {
        Self::with_runner(repo, predicate, Box::new(GhBinaryRunner))
    }

    /// Build a source with a custom command runner — used by tests.
    pub fn with_runner(
        repo: impl Into<String>,
        predicate: GhPrPredicate,
        runner: Box<dyn GhCommandRunner>,
    ) -> Self {
        Self {
            repo: repo.into(),
            predicate,
            seen: Mutex::new(HashSet::new()),
            pending: Mutex::new(Vec::new()),
            runner,
        }
    }

    fn pr_to_input(&self, ctx: &LoopContext, pr: &GhPr) -> LoopInput {
        let labels: Vec<&str> = pr.labels.iter().map(|l| l.name.as_str()).collect();
        let author = pr
            .author
            .as_ref()
            .map(|a| a.login.clone())
            .unwrap_or_default();
        let payload = json!({
            "kind": "gh_pr",
            "repo": self.repo,
            "number": pr.number,
            "title": pr.title,
            "state": pr.state,
            "author": author,
            "labels": labels,
            "created_at": pr.created_at,
            "url": pr.url,
        });
        LoopInput {
            key: format!("{}#pr-{}", self.repo, pr.number),
            payload,
            correlation_id: CorrelationId::new(format!(
                "gh-pr:{}:{}:{}",
                ctx.loop_id, self.repo, pr.number
            )),
        }
    }

    /// Apply the structured predicate. Returns `true` if the PR should be
    /// emitted (passes every constraint).
    fn predicate_matches(&self, pr: &GhPr) -> bool {
        // Author filter.
        if let Some(want_author) = self.predicate.author.as_deref() {
            let got = pr.author.as_ref().map(|a| a.login.as_str()).unwrap_or("");
            if !got.eq_ignore_ascii_case(want_author) {
                return false;
            }
        }
        // Label filter.
        if let Some(want_label) = self.predicate.label.as_deref() {
            let has = pr.labels.iter().any(|l| l.name == want_label);
            if !has {
                return false;
            }
        }
        // `review_required` and `failing_ci` would need additional `gh`
        // calls per-PR (e.g. `gh pr view <n> --json reviewRequests`); we
        // accept the predicate but do not enforce it client-side in MVP
        // beyond what the `--search` qualifiers below already encode.
        true
    }

    fn refresh_pending(&self) -> Result<Vec<GhPr>, LoopSourceError> {
        let state_arg = match self.predicate.state {
            GhPrState::Open => "open",
            GhPrState::Closed => "closed",
            GhPrState::All => "all",
        };
        let mut args: Vec<String> = vec![
            "pr".to_string(),
            "list".to_string(),
            "-R".to_string(),
            self.repo.clone(),
            "--state".to_string(),
            state_arg.to_string(),
            "--json".to_string(),
            Self::JSON_FIELDS.to_string(),
        ];
        // `review_required` and `failing_ci` are best expressed as gh's
        // `--search` qualifiers. Even if `gh` rejects an unknown qualifier
        // we will surface the error cleanly via Transport.
        let mut search_terms: Vec<String> = Vec::new();
        if self.predicate.review_required {
            search_terms.push("review:required".to_string());
        }
        if self.predicate.failing_ci {
            search_terms.push("status:failure".to_string());
        }
        if !search_terms.is_empty() {
            args.push("--search".to_string());
            args.push(search_terms.join(" "));
        }

        let stdout = self.runner.run(&args)?;
        serde_json::from_str::<Vec<GhPr>>(&stdout).map_err(|e| {
            LoopSourceError::Transport(format!("failed to parse `gh pr list` JSON: {e}"))
        })
    }

    /// Drain pending in number-ascending order, returning the first unseen +
    /// predicate-passing PR. Re-queues everything else.
    fn drain_next(&self) -> Option<GhPr> {
        let mut pending = match self.pending.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        pending.sort_by_key(|p| p.number);
        let mut seen = match self.seen.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut emit: Option<GhPr> = None;
        let drained: Vec<GhPr> = pending.drain(..).collect();
        for pr in drained {
            if emit.is_none() && !seen.contains(&pr.number) && self.predicate_matches(&pr) {
                seen.insert(pr.number);
                emit = Some(pr);
            } else {
                pending.push(pr);
            }
        }
        emit
    }
}

impl LoopSource for GhPrReviewQueueSource {
    fn next(&mut self, ctx: &LoopContext) -> LoopPullResult {
        if let Some(pr) = self.drain_next() {
            return LoopPullResult::Available(self.pr_to_input(ctx, &pr));
        }
        match self.refresh_pending() {
            Ok(fresh) => {
                if let Ok(mut pending) = self.pending.lock() {
                    pending.extend(fresh);
                }
            }
            Err(e) => return LoopPullResult::Error(e),
        }
        match self.drain_next() {
            Some(pr) => LoopPullResult::Available(self.pr_to_input(ctx, &pr)),
            None => LoopPullResult::Empty,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    struct StubGhRunner {
        canned: StdMutex<Vec<String>>,
    }
    impl StubGhRunner {
        fn new(canned: Vec<&str>) -> Self {
            Self {
                canned: StdMutex::new(canned.into_iter().map(String::from).collect()),
            }
        }
    }
    impl GhCommandRunner for StubGhRunner {
        fn run(&self, _args: &[String]) -> Result<String, LoopSourceError> {
            let mut canned = self.canned.lock().unwrap();
            if canned.is_empty() {
                Ok("[]".to_string())
            } else {
                Ok(canned.remove(0))
            }
        }
    }

    fn ctx() -> LoopContext {
        LoopContext {
            loop_id: "reviewer".to_string(),
        }
    }

    #[test]
    fn first_pull_emits_first_pr() {
        let stub = StubGhRunner::new(vec![
            r#"[{"number":42,"title":"feat: stuff","state":"OPEN","author":{"login":"alice"},"labels":[],"createdAt":"x","url":"u"}]"#,
        ]);
        let mut src = GhPrReviewQueueSource::with_runner(
            "owner/repo",
            GhPrPredicate::default(),
            Box::new(stub),
        );
        match src.next(&ctx()) {
            LoopPullResult::Available(input) => {
                assert_eq!(input.key, "owner/repo#pr-42");
                assert_eq!(input.payload["number"], 42);
                assert_eq!(input.payload["author"], "alice");
            }
            other => panic!("expected Available, got {other:?}"),
        }
    }

    #[test]
    fn already_seen_pr_is_deduped() {
        let stub = StubGhRunner::new(vec![
            r#"[{"number":1,"title":"a","state":"OPEN","author":null,"labels":[],"createdAt":"","url":""}]"#,
            r#"[{"number":1,"title":"a","state":"OPEN","author":null,"labels":[],"createdAt":"","url":""}]"#,
        ]);
        let mut src = GhPrReviewQueueSource::with_runner(
            "owner/repo",
            GhPrPredicate::default(),
            Box::new(stub),
        );
        let _ = src.next(&ctx());
        assert!(matches!(src.next(&ctx()), LoopPullResult::Empty));
    }

    #[test]
    fn author_predicate_filters_out_other_authors() {
        let stub = StubGhRunner::new(vec![
            r#"[{"number":1,"title":"a","state":"OPEN","author":{"login":"alice"},"labels":[],"createdAt":"","url":""},{"number":2,"title":"b","state":"OPEN","author":{"login":"bob"},"labels":[],"createdAt":"","url":""}]"#,
        ]);
        let predicate = GhPrPredicate {
            author: Some("bob".to_string()),
            ..Default::default()
        };
        let mut src = GhPrReviewQueueSource::with_runner("o/r", predicate, Box::new(stub));
        match src.next(&ctx()) {
            LoopPullResult::Available(input) => {
                assert_eq!(input.payload["number"], 2);
                assert_eq!(input.payload["author"], "bob");
            }
            other => panic!("expected Available, got {other:?}"),
        }
    }

    #[test]
    fn label_predicate_filters_by_label() {
        let stub = StubGhRunner::new(vec![
            r#"[{"number":1,"title":"a","state":"OPEN","author":null,"labels":[{"name":"bug"}],"createdAt":"","url":""},{"number":2,"title":"b","state":"OPEN","author":null,"labels":[{"name":"feature"}],"createdAt":"","url":""}]"#,
        ]);
        let predicate = GhPrPredicate {
            label: Some("feature".to_string()),
            ..Default::default()
        };
        let mut src = GhPrReviewQueueSource::with_runner("o/r", predicate, Box::new(stub));
        match src.next(&ctx()) {
            LoopPullResult::Available(input) => {
                assert_eq!(input.payload["number"], 2);
            }
            other => panic!("expected Available, got {other:?}"),
        }
    }

    #[test]
    fn empty_pr_list_returns_empty() {
        let stub = StubGhRunner::new(vec!["[]"]);
        let mut src = GhPrReviewQueueSource::with_runner(
            "o/r",
            GhPrPredicate::default(),
            Box::new(stub),
        );
        assert!(matches!(src.next(&ctx()), LoopPullResult::Empty));
    }
}
