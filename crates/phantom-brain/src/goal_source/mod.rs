//! Goal-source abstraction for autonomous discovery of work the brain can pursue.
//!
//! The brain self-improvement design (`docs/design/brain-self-improvement.md`,
//! §2) introduces a pluggable source-of-goals trait so the brain can poll for
//! candidate goals (open GitHub issues, failing CI runs, etc.) without bolting
//! that knowledge into the OODA loop itself. Each implementation owns its own
//! dedup state and cache; the self-improvement reconciler calls `poll()` on a
//! fixed cadence and feeds the results through scoring.
//!
//! # Implementations
//!
//! - [`GhIssueGoalSource`] (`gh_issues` submodule) — `gh issue list` adapter.
//! - [`GhCiFailureGoalSource`] (`gh_ci` submodule) — `gh run list` adapter.
//!
//! Both implementations are abstracted over [`GhCommandRunner`] so tests can
//! inject canned JSON without invoking the real `gh` binary. The same pattern
//! is used in `phantom-loop/src/sources/gh_issues.rs` so the audit story is
//! identical across crates.

pub mod gh_ci;
pub mod gh_issues;

use std::time::SystemTime;

pub use gh_ci::GhCiFailureGoalSource;
pub use gh_issues::GhIssueGoalSource;

// ---------------------------------------------------------------------------
// Shared ISO 8601 parser
// ---------------------------------------------------------------------------

/// Best-effort RFC 3339 parser used by both gh_issues and gh_ci.
///
/// Accepts the fixed GitHub API shape `"YYYY-MM-DDTHH:MM:SSZ"`. On parse
/// failure returns the current system time so downstream code can still
/// proceed; the `age_hours` signal simply reads zero in that case.
///
/// Hand-rolled rather than pulling chrono / time to keep the brain crate's
/// dependency surface unchanged.
pub(crate) fn gh_parse_iso8601(s: &str) -> SystemTime {
    if s.len() < 20 {
        return SystemTime::now();
    }
    let bytes = s.as_bytes();
    let year: i64 = std::str::from_utf8(&bytes[0..4])
        .ok()
        .and_then(|x| x.parse().ok())
        .unwrap_or(1970);
    let month: i64 = std::str::from_utf8(&bytes[5..7])
        .ok()
        .and_then(|x| x.parse().ok())
        .unwrap_or(1);
    let day: i64 = std::str::from_utf8(&bytes[8..10])
        .ok()
        .and_then(|x| x.parse().ok())
        .unwrap_or(1);
    let hour: i64 = std::str::from_utf8(&bytes[11..13])
        .ok()
        .and_then(|x| x.parse().ok())
        .unwrap_or(0);
    let minute: i64 = std::str::from_utf8(&bytes[14..16])
        .ok()
        .and_then(|x| x.parse().ok())
        .unwrap_or(0);
    let second: i64 = std::str::from_utf8(&bytes[17..19])
        .ok()
        .and_then(|x| x.parse().ok())
        .unwrap_or(0);

    // Proleptic Gregorian formula (Howard Hinnant's "date" library, public domain).
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y / 400 } else { (y - 399) / 400 };
    let yoe = y - era * 400;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days_since_epoch = era * 146097 + doe - 719468;
    let secs = days_since_epoch * 86400 + hour * 3600 + minute * 60 + second;
    if secs < 0 {
        SystemTime::UNIX_EPOCH
    } else {
        SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs as u64)
    }
}

/// Errors a [`GoalSource`] may surface from `poll()`.
///
/// Production callers should log and continue — a single failed poll never
/// crashes the brain thread. The variants distinguish *recoverable* outages
/// (network down, `gh` not authenticated) from *terminal* misconfigurations
/// (bad JSON schema, malformed input) so future telemetry can rate-limit
/// the noisy ones independently.
#[derive(Debug)]
pub enum GoalSourceError {
    /// The upstream dependency (`gh` CLI, network) is unavailable. The brain
    /// should retry on the next tick. Carries a human-readable hint.
    DependencyUnavailable(String),
    /// Transport-level failure: process exited non-zero, network timeout, or
    /// stdout was non-UTF-8. Carries a human-readable hint.
    Transport(String),
    /// The upstream payload could not be parsed against the expected schema.
    /// Carries a human-readable hint that includes the parse error.
    SchemaMismatch(String),
}

impl std::fmt::Display for GoalSourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DependencyUnavailable(msg) => write!(f, "dependency unavailable: {msg}"),
            Self::Transport(msg) => write!(f, "transport error: {msg}"),
            Self::SchemaMismatch(msg) => write!(f, "schema mismatch: {msg}"),
        }
    }
}

impl std::error::Error for GoalSourceError {}

/// A pluggable source of candidate goals the brain can pursue autonomously.
///
/// Implementations poll an external surface (GitHub issues, failing CI runs,
/// memory store, etc.) and yield zero or more [`GoalCandidate`]s on each
/// `poll()`. The brain's self-improvement reconciler calls `poll()` on a
/// fixed cadence and feeds the results through scoring before deciding
/// whether to auto-enqueue.
///
/// # Threading
///
/// Implementations must be `Send + Sync`. The brain thread owns the source
/// and serializes calls; the `Send + Sync` bound only matters because the
/// source is stored inside a `Vec<Box<dyn GoalSource>>` that may move across
/// threads during reconfiguration.
///
/// # Cadence
///
/// `poll()` should be **cheap** — the caller controls cadence (default 60s
/// per source). Implementations that wrap a slow upstream call SHOULD cache
/// results locally and refresh in the background; the brain thread must not
/// block waiting on `gh`.
pub trait GoalSource: Send + Sync {
    /// Stable identifier for the source (e.g. `"gh-issues"`, `"gh-ci-failures"`).
    ///
    /// Used as the `source` field on emitted [`GoalCandidate`]s and as the
    /// audit-log dedup key. MUST be globally unique within a brain instance.
    fn name(&self) -> &str;

    /// Pull the current set of candidate goals.
    ///
    /// # Errors
    ///
    /// Returns [`GoalSourceError`] on transport / schema / dependency failures.
    /// Implementations SHOULD dedupe against already-yielded `external_id`s so
    /// the same upstream issue does not re-enqueue across ticks.
    fn poll(&mut self) -> Result<Vec<GoalCandidate>, GoalSourceError>;
}

/// A goal candidate surfaced by a [`GoalSource`].
///
/// Not yet scored. The brain's scoring path
/// ([`crate::self_improvement::score_candidate`]) produces a utility score in
/// `[0.0, 1.0]`; only candidates above the configured `auto_enqueue_threshold`
/// are auto-enqueued.
#[derive(Debug, Clone)]
pub struct GoalCandidate {
    /// Source identifier — equals the producing [`GoalSource::name`].
    pub source: String,
    /// Stable upstream identifier — e.g. `"gh-issue:649"` or `"gh-run:1234567"`.
    /// Used for dedup so the same issue does not enqueue twice.
    pub external_id: String,
    /// Human-readable title (mapped onto the implementer payload's `title`).
    pub title: String,
    /// Long-form description. `None` when the upstream surface does not
    /// provide a body (e.g. a CI failure run has only metadata).
    pub body: Option<String>,
    /// Upstream labels attached to the candidate.
    pub labels: Vec<String>,
    /// Wall-clock creation time of the upstream surface.
    pub created_at: SystemTime,
    /// Free-form numeric signals the scorer reads, keyed by name.
    ///
    /// Keys are stable per source — e.g. `gh-issues` populates
    /// `"priority_rank"`, `"age_hours"`, `"activity_count"`, `"blocked_by_count"`.
    /// Unknown keys are treated as missing by the scorer.
    pub signals: std::collections::HashMap<String, f64>,
    /// Optional URL pointing back to the upstream surface (issue / run page).
    pub url: Option<String>,
    /// Optional author handle (used by hard-exclusion checks).
    pub author: Option<String>,
}

impl GoalCandidate {
    /// Read a signal by name, returning `0.0` if absent. Convenience wrapper
    /// around `signals.get(name).copied().unwrap_or(0.0)`.
    #[must_use]
    pub fn signal(&self, name: &str) -> f64 {
        self.signals.get(name).copied().unwrap_or(0.0)
    }

    /// Check whether `label` is present (case-sensitive). Convenience wrapper.
    #[must_use]
    pub fn has_label(&self, label: &str) -> bool {
        self.labels.iter().any(|l| l == label)
    }
}

// ---------------------------------------------------------------------------
// GhCommandRunner — shared between gh_issues and gh_ci
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
    /// Returns [`GoalSourceError::DependencyUnavailable`] when the binary is
    /// missing or non-executable, and [`GoalSourceError::Transport`] when
    /// `gh` exited non-zero. The runner cannot distinguish a network outage
    /// from an auth failure — both surface as `Transport`.
    fn run(&self, args: &[String]) -> Result<String, GoalSourceError>;
}

/// Production runner that shells the real `gh` binary.
#[derive(Debug, Default)]
pub struct GhBinaryRunner;

impl GhCommandRunner for GhBinaryRunner {
    fn run(&self, args: &[String]) -> Result<String, GoalSourceError> {
        let out = std::process::Command::new("gh")
            .args(args)
            .output()
            .map_err(|e| {
                GoalSourceError::DependencyUnavailable(format!(
                    "could not execute `gh`: {e} — install GitHub CLI from https://cli.github.com/"
                ))
            })?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(GoalSourceError::Transport(format!(
                "`gh {}` exited {} — stderr: {}",
                args.join(" "),
                out.status,
                stderr.trim()
            )));
        }
        String::from_utf8(out.stdout)
            .map_err(|e| GoalSourceError::Transport(format!("`gh` stdout is not UTF-8: {e}")))
    }
}

// ---------------------------------------------------------------------------
// Test stub runner — public so integration tests can use it
// ---------------------------------------------------------------------------

/// In-test stub runner that returns canned JSON without invoking the real
/// binary. Tests push canned responses in order; each `run()` call pops the
/// front of the queue. When the queue is empty `run()` returns `"[]"`.
///
/// This is `pub` (not `cfg(test)`-gated) so integration tests in the sibling
/// `tests/` directory can use it without enabling a dev-only feature on
/// every consumer.
#[derive(Debug, Default)]
pub struct StubGhRunner {
    canned: std::sync::Mutex<Vec<String>>,
    calls: std::sync::Mutex<Vec<Vec<String>>>,
}

impl StubGhRunner {
    /// Build a stub primed with the given canned responses (in order).
    #[must_use]
    pub fn new(responses: Vec<String>) -> Self {
        Self {
            canned: std::sync::Mutex::new(responses),
            calls: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Return a clone of every set of args this stub has been called with.
    /// Tests use this to assert that the source built the right `gh` command
    /// line (e.g. that `--label priority:high` was passed through).
    pub fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().map(|c| c.clone()).unwrap_or_default()
    }
}

impl GhCommandRunner for StubGhRunner {
    fn run(&self, args: &[String]) -> Result<String, GoalSourceError> {
        if let Ok(mut calls) = self.calls.lock() {
            calls.push(args.to_vec());
        }
        let mut canned = self
            .canned
            .lock()
            .map_err(|e| GoalSourceError::Transport(format!("stub mutex poisoned: {e}")))?;
        Ok(if canned.is_empty() {
            "[]".to_string()
        } else {
            canned.remove(0)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn goal_source_error_display() {
        let e = GoalSourceError::DependencyUnavailable("missing".into());
        assert!(e.to_string().contains("missing"));
        let e = GoalSourceError::Transport("timeout".into());
        assert!(e.to_string().contains("timeout"));
        let e = GoalSourceError::SchemaMismatch("bad json".into());
        assert!(e.to_string().contains("bad json"));
    }

    #[test]
    fn candidate_signal_returns_zero_when_absent() {
        let c = GoalCandidate {
            source: "gh-issues".into(),
            external_id: "gh-issue:1".into(),
            title: "t".into(),
            body: None,
            labels: vec![],
            created_at: SystemTime::now(),
            signals: std::collections::HashMap::new(),
            url: None,
            author: None,
        };
        assert!((c.signal("missing")).abs() < f64::EPSILON);
    }

    #[test]
    fn candidate_has_label_is_case_sensitive() {
        let c = GoalCandidate {
            source: "gh-issues".into(),
            external_id: "gh-issue:1".into(),
            title: "t".into(),
            body: None,
            labels: vec!["priority:high".into(), "good-first-issue".into()],
            created_at: SystemTime::now(),
            signals: std::collections::HashMap::new(),
            url: None,
            author: None,
        };
        assert!(c.has_label("priority:high"));
        assert!(c.has_label("good-first-issue"));
        assert!(!c.has_label("Priority:High"));
        assert!(!c.has_label("missing"));
    }

    #[test]
    fn stub_runner_returns_canned_in_order() {
        let stub = StubGhRunner::new(vec!["[]".into(), r#"[{"n":1}]"#.into()]);
        assert_eq!(stub.run(&[]).unwrap(), "[]");
        assert_eq!(stub.run(&[]).unwrap(), r#"[{"n":1}]"#);
        assert_eq!(stub.run(&[]).unwrap(), "[]"); // default when exhausted
    }

    #[test]
    fn stub_runner_records_args() {
        let stub = StubGhRunner::new(vec![]);
        let _ = stub.run(&["issue".into(), "list".into()]);
        let _ = stub.run(&["run".into(), "list".into()]);
        let calls = stub.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0], vec!["issue", "list"]);
        assert_eq!(calls[1], vec!["run", "list"]);
    }
}
