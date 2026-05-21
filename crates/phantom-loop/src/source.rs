//! Where a loop pulls work from.
//!
//! A [`LoopSourceSpec`] describes the *input side* of a loop: GitHub issues
//! or PRs, an internal cross-loop queue, or a cron timer. The future runner
//! (C2) will implement a `LoopSource` trait keyed on this enum's discriminant
//! and produce a stream of typed `LoopMessage`s for each variant.

use serde::{Deserialize, Serialize};

/// Tagged union over the available loop input sources.
///
/// Encoded in TOML as:
///
/// ```toml
/// [source]
/// kind = "gh_pr"
/// repo = "jdmiranda/phantom"
///
/// [source.predicate]
/// state = "open"
/// review_required = true
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LoopSourceSpec {
    /// Poll `gh issue list` against a repository.
    ///
    /// `query` accepts the same string the `gh` CLI accepts for `--search`;
    /// `label` is a convenience filter equivalent to `--label`. When both are
    /// `None` the source returns *all* open issues — narrowing is the
    /// caller's responsibility.
    GhIssues {
        repo: String,
        #[serde(default)]
        label: Option<String>,
        #[serde(default)]
        query: Option<String>,
    },

    /// Poll `gh pr list` against a repository with a structured predicate.
    GhPr {
        repo: String,
        #[serde(default)]
        predicate: GhPrPredicate,
    },

    /// Pull from an in-process named queue. The PR-finder loop publishes
    /// `LoopMessage`s into a named queue and the Reviewer loop drains it.
    Queue { name: String },

    /// Fire on a fixed interval. Used by agentless poll loops like PR-finder.
    Cron { interval_seconds: u64 },
}

/// Structured predicate for filtering pull-request results.
///
/// Each `Some` field tightens the filter; `None` means "don't constrain".
/// The boolean fields default to `false` because they describe additive
/// constraints — `review_required = true` means *also* require that the
/// PR is awaiting review, not the inverse.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct GhPrPredicate {
    /// Restrict to PRs in this state. Defaults to [`GhPrState::Open`].
    pub state: GhPrState,
    /// Require the PR to be awaiting review.
    pub review_required: bool,
    /// Require the PR to have at least one failing CI check.
    pub failing_ci: bool,
    /// Restrict to PRs by this author.
    pub author: Option<String>,
    /// Restrict to PRs carrying this label.
    pub label: Option<String>,
}

/// Whether to scope a [`LoopSourceSpec::GhPr`] query to open, closed, or all PRs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum GhPrState {
    /// Only open PRs. The default — almost every loop wants this.
    #[default]
    Open,
    /// Only closed (merged or rejected) PRs.
    Closed,
    /// Every PR, regardless of state.
    All,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gh_pr_state_defaults_to_open() {
        assert_eq!(GhPrState::default(), GhPrState::Open);
    }

    #[test]
    fn gh_pr_predicate_defaults_are_permissive() {
        let p = GhPrPredicate::default();
        assert_eq!(p.state, GhPrState::Open);
        assert!(!p.review_required);
        assert!(!p.failing_ci);
        assert!(p.author.is_none());
        assert!(p.label.is_none());
    }

    #[test]
    fn loop_source_spec_deserializes_gh_pr_kind() {
        let src: LoopSourceSpec = toml::from_str(
            r#"
            kind = "gh_pr"
            repo = "jdmiranda/phantom"

            [predicate]
            state = "open"
            review_required = true
            "#,
        )
        .expect("parse gh_pr source");
        match src {
            LoopSourceSpec::GhPr { repo, predicate } => {
                assert_eq!(repo, "jdmiranda/phantom");
                assert_eq!(predicate.state, GhPrState::Open);
                assert!(predicate.review_required);
            }
            other => panic!("expected GhPr, got {other:?}"),
        }
    }

    #[test]
    fn loop_source_spec_deserializes_cron_kind() {
        let src: LoopSourceSpec = toml::from_str(
            r#"
            kind = "cron"
            interval_seconds = 300
            "#,
        )
        .expect("parse cron source");
        match src {
            LoopSourceSpec::Cron { interval_seconds } => {
                assert_eq!(interval_seconds, 300);
            }
            other => panic!("expected Cron, got {other:?}"),
        }
    }

    #[test]
    fn loop_source_spec_deserializes_queue_kind() {
        let src: LoopSourceSpec = toml::from_str(
            r#"
            kind = "queue"
            name = "review-queue"
            "#,
        )
        .expect("parse queue source");
        match src {
            LoopSourceSpec::Queue { name } => assert_eq!(name, "review-queue"),
            other => panic!("expected Queue, got {other:?}"),
        }
    }

    #[test]
    fn loop_source_spec_deserializes_gh_issues_kind() {
        let src: LoopSourceSpec = toml::from_str(
            r#"
            kind = "gh_issues"
            repo = "jdmiranda/phantom"
            label = "bug"
            "#,
        )
        .expect("parse gh_issues source");
        match src {
            LoopSourceSpec::GhIssues { repo, label, query } => {
                assert_eq!(repo, "jdmiranda/phantom");
                assert_eq!(label.as_deref(), Some("bug"));
                assert!(query.is_none());
            }
            other => panic!("expected GhIssues, got {other:?}"),
        }
    }
}
