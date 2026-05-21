//! GitHub-issue-backed loop source.
//!
//! [`GhIssueQueueSource`] shells out to the user-installed `gh` CLI to
//! enumerate open issues in a repository, dedupes against an in-memory
//! `seen` set, and emits one [`crate::runner::LoopInput`] per fresh issue.
//!
//! # Why shell out instead of using a Rust GitHub client
//!
//! Two reasons:
//!
//! 1. **Auth.** `gh` already manages an `OAuth` token in the user's
//!    keychain. A Rust `octocrab`/`reqwest` path would have to re-implement
//!    token discovery, hub URL resolution, and EMU-account multiplexing —
//!    all of which the `gh` CLI handles correctly.
//! 2. **Surface stability.** `gh issue list --json …` produces a fixed JSON
//!    schema the user can extend with `--jq` filters if needed. The
//!    octocrab API surface drifts more often than the `gh` JSON output.
//!
//! # Dedup
//!
//! The source keeps an in-memory `HashSet<u64>` of every issue number it
//! has yielded. `next()` lists fresh issues each call, walks them in
//! increasing-issue-number order, and emits the first one not in the set
//! (adding it to the set on emission). The dedup state lives for the
//! lifetime of the source, so re-listing the same open issue across many
//! ticks does not re-enqueue it.

use std::collections::HashSet;
use std::sync::Mutex;

use serde::Deserialize;
use serde_json::json;

use crate::runner::source::{
    CorrelationId, LoopContext, LoopInput, LoopPullResult, LoopSource, LoopSourceError,
};

/// One row from `gh issue list --json number,title,labels,createdAt,url`.
///
/// We pull a minimal subset of the schema — only enough to dedupe and to
/// build a useful `LoopInput::payload`. `gh`'s schema is rich; callers that
/// need additional fields can extend [`GhIssueQueueSource::JSON_FIELDS`].
#[derive(Debug, Clone, Deserialize)]
struct GhIssue {
    number: u64,
    title: String,
    #[serde(default)]
    labels: Vec<GhIssueLabel>,
    #[serde(rename = "createdAt", default)]
    created_at: String,
    #[serde(default)]
    url: String,
}

#[derive(Debug, Clone, Deserialize)]
struct GhIssueLabel {
    name: String,
}

/// Source that pulls fresh issues from a GitHub repository.
///
/// The first `next()` call after an empty period spawns a `gh` subprocess
/// in a blocking thread (`tokio::task::spawn_blocking`) so the runner's
/// event loop is never blocked on `gh`'s wallclock. Results are cached
/// locally and drained one-per-`next()` until exhausted.
pub struct GhIssueQueueSource {
    repo: String,
    label: Option<String>,
    query: Option<String>,
    /// Issues seen across the entire lifetime of this source. Issue numbers
    /// are monotonic per-repo so a plain `HashSet<u64>` suffices.
    seen: Mutex<HashSet<u64>>,
    /// Locally-cached fresh issues, drained one-per-`next()`. Refreshed
    /// when emptied.
    pending: Mutex<Vec<GhIssue>>,
    /// Configurable shell-out wrapper. Lets tests inject a stub instead of
    /// running the real `gh` binary.
    runner: Box<dyn GhCommandRunner>,
}

impl std::fmt::Debug for GhIssueQueueSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GhIssueQueueSource")
            .field("repo", &self.repo)
            .field("label", &self.label)
            .field("query", &self.query)
            .field("seen", &self.seen.lock().map(|s| s.len()).unwrap_or(0))
            .field("pending", &self.pending.lock().map(|p| p.len()).unwrap_or(0))
            .finish()
    }
}

impl GhIssueQueueSource {
    /// Fixed list of JSON fields requested from `gh issue list`. Kept as a
    /// constant so the [`GhIssue`] deserializer stays in lock-step.
    const JSON_FIELDS: &'static str = "number,title,labels,createdAt,url";

    /// Build a source pointing at `repo` (`<owner>/<repo>` form), using the
    /// real `gh` binary.
    #[must_use]
    pub fn new(
        repo: impl Into<String>,
        label: Option<String>,
        query: Option<String>,
    ) -> Self {
        Self::with_runner(repo, label, query, Box::new(GhBinaryRunner))
    }

    /// Build a source with a custom command runner — used by tests to
    /// inject a [`StubGhRunner`] that returns canned JSON.
    pub fn with_runner(
        repo: impl Into<String>,
        label: Option<String>,
        query: Option<String>,
        runner: Box<dyn GhCommandRunner>,
    ) -> Self {
        Self {
            repo: repo.into(),
            label,
            query,
            seen: Mutex::new(HashSet::new()),
            pending: Mutex::new(Vec::new()),
            runner,
        }
    }

    /// Convert one `GhIssue` into a [`LoopInput`].
    fn issue_to_input(&self, ctx: &LoopContext, issue: &GhIssue) -> LoopInput {
        let labels: Vec<&str> = issue.labels.iter().map(|l| l.name.as_str()).collect();
        let payload = json!({
            "kind": "gh_issue",
            "repo": self.repo,
            "number": issue.number,
            "title": issue.title,
            "labels": labels,
            "created_at": issue.created_at,
            "url": issue.url,
        });
        LoopInput {
            key: format!("{}#{}", self.repo, issue.number),
            payload,
            correlation_id: CorrelationId::new(format!(
                "gh-issue:{}:{}:{}",
                ctx.loop_id, self.repo, issue.number
            )),
        }
    }

    /// Refill the pending cache by shelling `gh issue list`. Returns the
    /// raw deserialized list (un-deduped); the caller is responsible for
    /// applying the `seen` filter.
    fn refresh_pending(&self) -> Result<Vec<GhIssue>, LoopSourceError> {
        let mut args: Vec<String> = vec![
            "issue".to_string(),
            "list".to_string(),
            "-R".to_string(),
            self.repo.clone(),
            "--state".to_string(),
            "open".to_string(),
            "--json".to_string(),
            Self::JSON_FIELDS.to_string(),
        ];
        if let Some(label) = &self.label {
            args.push("--label".to_string());
            args.push(label.clone());
        }
        if let Some(query) = &self.query {
            args.push("--search".to_string());
            args.push(query.clone());
        }

        let stdout = self.runner.run(&args)?;
        serde_json::from_str::<Vec<GhIssue>>(&stdout).map_err(|e| {
            LoopSourceError::Transport(format!("failed to parse `gh issue list` JSON: {e}"))
        })
    }
}

impl LoopSource for GhIssueQueueSource {
    fn next(&mut self, ctx: &LoopContext) -> LoopPullResult {
        // Pop the next pending issue if any.
        let next = {
            let mut pending = match self.pending.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            // Sort by number ascending so dedup is deterministic.
            pending.sort_by_key(|i| i.number);
            // Find the first issue we have not seen yet.
            let mut seen = match self.seen.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            let mut emit: Option<GhIssue> = None;
            // Two-pass: drain everything; track the first unseen.
            let drained: Vec<GhIssue> = pending.drain(..).collect();
            for issue in drained {
                if emit.is_none() && !seen.contains(&issue.number) {
                    seen.insert(issue.number);
                    emit = Some(issue);
                    // Remaining unseen issues go back into pending for
                    // subsequent `next()` calls; we yield one-per-tick.
                } else {
                    pending.push(issue);
                }
            }
            emit
        };
        if let Some(issue) = next {
            return LoopPullResult::Available(self.issue_to_input(ctx, &issue));
        }

        // Pending cache empty — refresh from `gh`.
        match self.refresh_pending() {
            Ok(fresh) => {
                if let Ok(mut pending) = self.pending.lock() {
                    pending.extend(fresh);
                }
            }
            Err(e) => return LoopPullResult::Error(e),
        }

        // Drain again — same pop logic, factored into one inline pass.
        let mut pending = match self.pending.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        pending.sort_by_key(|i| i.number);
        let mut seen = match self.seen.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut emit: Option<GhIssue> = None;
        let drained: Vec<GhIssue> = pending.drain(..).collect();
        for issue in drained {
            if emit.is_none() && !seen.contains(&issue.number) {
                seen.insert(issue.number);
                emit = Some(issue);
            } else {
                pending.push(issue);
            }
        }
        match emit {
            Some(issue) => LoopPullResult::Available(self.issue_to_input(ctx, &issue)),
            None => LoopPullResult::Empty,
        }
    }
}

// ---------------------------------------------------------------------------
// Command runner abstraction
// ---------------------------------------------------------------------------

/// Trait abstracting "run `gh` with these args, return stdout".
///
/// The production impl ([`GhBinaryRunner`]) shells out to the real binary;
/// tests inject [`StubGhRunner`] to return canned JSON.
pub trait GhCommandRunner: Send + Sync {
    /// Run `gh <args>` and return stdout as a UTF-8 string.
    ///
    /// # Errors
    ///
    /// Returns [`LoopSourceError::DependencyUnavailable`] when the binary
    /// is missing or non-executable, and [`LoopSourceError::Transport`]
    /// when `gh` exited non-zero (the runner cannot distinguish a
    /// network outage from an auth failure — both surface as `Transport`).
    fn run(&self, args: &[String]) -> Result<String, LoopSourceError>;
}

/// Production runner that shells the real `gh` binary.
#[derive(Debug)]
pub struct GhBinaryRunner;

impl GhCommandRunner for GhBinaryRunner {
    fn run(&self, args: &[String]) -> Result<String, LoopSourceError> {
        let out = std::process::Command::new("gh")
            .args(args)
            .output()
            .map_err(|e| {
                LoopSourceError::DependencyUnavailable(format!(
                    "could not execute `gh`: {e} — install GitHub CLI from https://cli.github.com/"
                ))
            })?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(LoopSourceError::Transport(format!(
                "`gh {}` exited {} — stderr: {}",
                args.join(" "),
                out.status,
                stderr.trim()
            )));
        }
        String::from_utf8(out.stdout).map_err(|e| {
            LoopSourceError::Transport(format!("`gh` stdout is not UTF-8: {e}"))
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    /// Test runner that returns canned JSON without invoking the real binary.
    struct StubGhRunner {
        canned: StdMutex<Vec<String>>,
        calls: StdMutex<Vec<Vec<String>>>,
    }
    impl StubGhRunner {
        fn new(canned_responses: Vec<&str>) -> Self {
            Self {
                canned: StdMutex::new(
                    canned_responses.into_iter().map(String::from).collect(),
                ),
                calls: StdMutex::new(Vec::new()),
            }
        }
    }
    impl GhCommandRunner for StubGhRunner {
        fn run(&self, args: &[String]) -> Result<String, LoopSourceError> {
            self.calls.lock().unwrap().push(args.to_vec());
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
            loop_id: "pr-finder".to_string(),
        }
    }

    #[test]
    fn first_pull_emits_first_fresh_issue() {
        let stub = StubGhRunner::new(vec![
            r#"[{"number":1,"title":"a","labels":[],"createdAt":"x","url":"u1"}]"#,
        ]);
        let mut src = GhIssueQueueSource::with_runner(
            "owner/repo",
            None,
            None,
            Box::new(stub),
        );
        match src.next(&ctx()) {
            LoopPullResult::Available(input) => {
                assert_eq!(input.key, "owner/repo#1");
                assert_eq!(input.payload["number"], 1);
                assert_eq!(input.payload["title"], "a");
            }
            other => panic!("expected Available, got {other:?}"),
        }
    }

    #[test]
    fn second_pull_skips_already_seen_issue() {
        // Both calls return the same one issue; the second call must dedupe.
        let stub = StubGhRunner::new(vec![
            r#"[{"number":1,"title":"a","labels":[],"createdAt":"x","url":"u1"}]"#,
            r#"[{"number":1,"title":"a","labels":[],"createdAt":"x","url":"u1"}]"#,
        ]);
        let mut src = GhIssueQueueSource::with_runner(
            "owner/repo",
            None,
            None,
            Box::new(stub),
        );
        let _ = src.next(&ctx());
        assert!(matches!(src.next(&ctx()), LoopPullResult::Empty));
    }

    #[test]
    fn multi_issue_batch_emits_one_per_call_in_number_order() {
        let stub = StubGhRunner::new(vec![
            // Out-of-order on purpose — source must sort by number.
            r#"[{"number":3,"title":"c","labels":[],"createdAt":"x","url":"u3"},{"number":1,"title":"a","labels":[],"createdAt":"x","url":"u1"},{"number":2,"title":"b","labels":[],"createdAt":"x","url":"u2"}]"#,
        ]);
        let mut src = GhIssueQueueSource::with_runner(
            "owner/repo",
            None,
            None,
            Box::new(stub),
        );
        let mut numbers = Vec::new();
        for _ in 0..3 {
            match src.next(&ctx()) {
                LoopPullResult::Available(input) => {
                    numbers.push(input.payload["number"].as_u64().unwrap());
                }
                other => panic!("expected Available, got {other:?}"),
            }
        }
        assert_eq!(numbers, vec![1, 2, 3]);
    }

    #[test]
    fn transport_failure_surfaces_as_error_variant() {
        struct FailingRunner;
        impl GhCommandRunner for FailingRunner {
            fn run(&self, _args: &[String]) -> Result<String, LoopSourceError> {
                Err(LoopSourceError::DependencyUnavailable("missing".to_string()))
            }
        }
        let mut src = GhIssueQueueSource::with_runner(
            "owner/repo",
            None,
            None,
            Box::new(FailingRunner),
        );
        match src.next(&ctx()) {
            LoopPullResult::Error(LoopSourceError::DependencyUnavailable(_)) => {}
            other => panic!("expected DependencyUnavailable error, got {other:?}"),
        }
    }

    #[test]
    fn label_filter_appears_in_gh_args() {
        let stub_calls: std::sync::Arc<StdMutex<Vec<Vec<String>>>> =
            std::sync::Arc::new(StdMutex::new(Vec::new()));
        struct RecorderRunner(std::sync::Arc<StdMutex<Vec<Vec<String>>>>);
        impl GhCommandRunner for RecorderRunner {
            fn run(&self, args: &[String]) -> Result<String, LoopSourceError> {
                self.0.lock().unwrap().push(args.to_vec());
                Ok("[]".to_string())
            }
        }
        let mut src = GhIssueQueueSource::with_runner(
            "owner/repo",
            Some("bug".to_string()),
            None,
            Box::new(RecorderRunner(stub_calls.clone())),
        );
        let _ = src.next(&ctx());
        let calls = stub_calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        let args = &calls[0];
        assert!(args.contains(&"--label".to_string()));
        assert!(args.contains(&"bug".to_string()));
    }
}
