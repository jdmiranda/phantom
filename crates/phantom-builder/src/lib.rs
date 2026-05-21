//! Phantom builder — a higher-level orchestration layer on top of the
//! [`phantom_loop`] + [`phantom_brain`] infrastructure that lets the user
//! point Phantom at any GitHub repository and have it work through all the
//! open issues autonomously.
//!
//! # Topology
//!
//! ```text
//!   phantom builder run <owner>/<repo>
//!       │
//!       ▼
//!   ensure_local_checkout(slug, override) ──► <path>
//!       │  git clone OR `git fetch && git checkout origin/main`
//!       ▼
//!   write_default_specs(path, slug)
//!       │  drops pr_finder_review.toml, pr_finder_impl.toml,
//!       │  reviewer.toml, implementer.toml into <path>/.phantom/loops/
//!       │  with `repo = "<slug>"` substituted on every gh_pr/gh_issues source
//!       ▼
//!   build_loop_runtime(...)
//!       │  reuses phantom-loop's LoopRegistry + LoopQueueRegistry
//!       │  + SubstrateAgentDispatcher + SubstrateDriver
//!       ▼
//!   build_brain_with_self_improvement(slug, safety, trust_band)
//!       │  reuses phantom-brain's BrainConfig + SelfImprovementState
//!       │  + GhIssueGoalSource (label_filter = None → ALL open issues)
//!       │  + GhCiFailureGoalSource
//!       ▼
//!   forward brain actions → LoopQueueActionHandler → LoopQueueRegistry
//! ```
//!
//! # Safety
//!
//! Three knobs in [`BuilderSafetyConfig`] limit autonomy:
//!
//! - `dry_run`: when true, the brain emits scoring decisions but the
//!   action forwarder substitutes a [`safety::DryRunActionHandler`] that
//!   logs intent without enqueueing. The substrate driver never sees a
//!   request.
//! - `max_prs_per_hour`: clamps the brain's per-hour rate limiter cap.
//! - `max_concurrent_agents`: the loop spec writer caps every spec's
//!   `max_concurrent` field at this value.
//!
//! # Trust band
//!
//! The brain's [`TrustBudget`][phantom_brain::self_improvement::TrustBudget]
//! defaults to band 1 (`Conservative`) in the builder — the per-hour cap is
//! halved and the score threshold raised to 0.85. Operators ramp upward
//! explicitly via [`BuilderConfig::trust_band`]. The builder never spawns
//! at band 3 (`Aggressive`) by default — that is the operator's call.

pub mod clone;
pub mod orchestrate;
pub mod safety;
pub mod templates;

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use phantom_brain::self_improvement::TrustBand;

pub use orchestrate::{Builder, BuilderHooks, BuilderResult, RunArtifacts};
pub use safety::{BuilderSafetyConfig, DryRunActionHandler};

// ---------------------------------------------------------------------------
// BuilderConfig — the public knob surface
// ---------------------------------------------------------------------------

/// Top-level config for [`Builder::run`].
///
/// Plain data so callers can construct one from CLI flags (the production
/// path) or deserialize from TOML (a future `phantom-builder.toml`). Every
/// field has a documented default; the CLI surfaces only the knobs that
/// commonly diverge from the default.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuilderConfig {
    /// Target repository slug in `owner/repo` form, e.g. `"jdmiranda/phantom"`.
    pub target_slug: String,

    /// When set, use this directory as the working copy instead of cloning to
    /// the default `~/.phantom/builds/<owner>-<repo>` path. The directory must
    /// already be a git checkout of `target_slug`; the builder does not
    /// validate the remote URL because in practice `phantom builder run`
    /// against an existing local clone is a power-user path.
    #[serde(default)]
    pub repo_path: Option<PathBuf>,

    /// Trust band the brain operates at. Defaults to [`TrustBand::Conservative`]
    /// (band 1) — active but rate-limited. The operator opts up to standard
    /// (band 2) or aggressive (band 3) by passing the `--trust-band` flag.
    #[serde(default = "default_trust_band")]
    pub trust_band: TrustBandConfig,

    /// Optional label filter for the brain's `GhIssueGoalSource`. The default
    /// (`None`) means **all open issues** become candidates — this is the
    /// "builder eats every issue" mode that differentiates the builder from
    /// `phantom loop run`'s opinionated `priority:*`-filtered default.
    #[serde(default)]
    pub label_filter: Option<Vec<String>>,

    /// Safety rails: rate caps, concurrency caps, and dry-run mode.
    #[serde(default)]
    pub safety: BuilderSafetyConfig,

    /// Which loop specs to start. Defaults to the canonical four-loop pipeline:
    /// `["pr_finder_review", "pr_finder_impl", "reviewer", "implementer"]`.
    #[serde(default = "default_loops")]
    pub loops: Vec<String>,
}

impl BuilderConfig {
    /// Build the minimum-viable config for a target slug. Every other field
    /// is filled with the documented default.
    #[must_use]
    pub fn new(target_slug: impl Into<String>) -> Self {
        Self {
            target_slug: target_slug.into(),
            repo_path: None,
            trust_band: default_trust_band(),
            label_filter: None,
            safety: BuilderSafetyConfig::default(),
            loops: default_loops(),
        }
    }
}

fn default_loops() -> Vec<String> {
    vec![
        "pr_finder_review".to_string(),
        "pr_finder_impl".to_string(),
        "reviewer".to_string(),
        "implementer".to_string(),
    ]
}

fn default_trust_band() -> TrustBandConfig {
    TrustBandConfig::Conservative
}

/// Serializable mirror of [`phantom_brain::self_improvement::TrustBand`].
///
/// `TrustBand` upstream is a plain enum with no `Serialize` impl — we shadow
/// it here so `BuilderConfig` can round-trip through TOML / JSON without
/// requiring a third-party feature on the brain crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustBandConfig {
    /// Band 0: suggestion-only. The brain never enqueues; useful for shadow runs.
    SuggestionOnly,
    /// Band 1: conservative. Default for the builder — score threshold 0.85,
    /// per-hour cap halved.
    Conservative,
    /// Band 2: standard. Score threshold 0.75, per-hour cap at default.
    Standard,
    /// Band 3: aggressive. Score threshold 0.65, per-hour cap doubled.
    Aggressive,
}

impl TrustBandConfig {
    /// Map the symbolic band to the starting trust-budget integer.
    ///
    /// See `phantom_brain::self_improvement::TrustBudget::band` for the band
    /// boundaries.
    #[must_use]
    pub fn starting_budget(self) -> u32 {
        match self {
            Self::SuggestionOnly => 0,
            Self::Conservative => 2,
            Self::Standard => 6,
            Self::Aggressive => 15,
        }
    }
}

impl From<TrustBandConfig> for TrustBand {
    fn from(value: TrustBandConfig) -> Self {
        match value {
            TrustBandConfig::SuggestionOnly => Self::SuggestionOnly,
            TrustBandConfig::Conservative => Self::Conservative,
            TrustBandConfig::Standard => Self::Standard,
            TrustBandConfig::Aggressive => Self::Aggressive,
        }
    }
}

// ---------------------------------------------------------------------------
// BuilderError
// ---------------------------------------------------------------------------

/// Errors returned by the builder.
///
/// Variants split along the orchestration phases so the CLI can map each one
/// to a precise stderr line.
#[derive(Debug, Error)]
pub enum BuilderError {
    /// The target slug did not parse as `owner/repo`.
    #[error("invalid slug `{0}` — expected `owner/repo`")]
    InvalidSlug(String),

    /// `git clone` (or the refresh path) failed.
    #[error("git operation failed: {0}")]
    Git(String),

    /// Filesystem I/O failed while seeding specs or resolving the clone path.
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// The user's home directory could not be resolved (needed for the default
    /// `~/.phantom/builds` location). Pass `--repo-path` to bypass.
    #[error("could not resolve $HOME — pass --repo-path to override")]
    NoHomeDirectory,

    /// Loop spec parsing or seeding failed.
    #[error("loop spec error: {0}")]
    Spec(String),

    /// Pre-flight gate (gh binary missing, gh auth, runlock) refused to run.
    #[error("preflight failed: {0}")]
    Preflight(String),

    /// A miscellaneous error surfaced through `anyhow` from a delegated call
    /// (e.g. the runtime building or the loop discovery walk).
    #[error("orchestrate failed: {0}")]
    Other(String),
}

impl From<anyhow::Error> for BuilderError {
    fn from(value: anyhow::Error) -> Self {
        Self::Other(value.to_string())
    }
}

// ---------------------------------------------------------------------------
// Slug helpers
// ---------------------------------------------------------------------------

/// Split a `owner/repo` slug into `(owner, repo)`.
///
/// # Errors
///
/// Returns [`BuilderError::InvalidSlug`] when `slug` does not contain exactly
/// one `/` or either side is empty.
pub fn parse_slug(slug: &str) -> Result<(&str, &str), BuilderError> {
    let mut parts = slug.split('/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(owner), Some(repo), None) if !owner.is_empty() && !repo.is_empty() => {
            Ok((owner, repo))
        }
        _ => Err(BuilderError::InvalidSlug(slug.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_slug_accepts_owner_repo() {
        let (owner, repo) = parse_slug("jdmiranda/phantom").unwrap();
        assert_eq!(owner, "jdmiranda");
        assert_eq!(repo, "phantom");
    }

    #[test]
    fn parse_slug_rejects_missing_slash() {
        assert!(matches!(
            parse_slug("nothingatall"),
            Err(BuilderError::InvalidSlug(_))
        ));
    }

    #[test]
    fn parse_slug_rejects_empty_owner() {
        assert!(matches!(
            parse_slug("/repo"),
            Err(BuilderError::InvalidSlug(_))
        ));
    }

    #[test]
    fn parse_slug_rejects_empty_repo() {
        assert!(matches!(
            parse_slug("owner/"),
            Err(BuilderError::InvalidSlug(_))
        ));
    }

    #[test]
    fn parse_slug_rejects_extra_slash() {
        assert!(matches!(
            parse_slug("owner/repo/extra"),
            Err(BuilderError::InvalidSlug(_))
        ));
    }

    #[test]
    fn default_loops_are_the_canonical_four() {
        let loops = default_loops();
        assert_eq!(loops.len(), 4);
        assert!(loops.contains(&"pr_finder_review".to_string()));
        assert!(loops.contains(&"pr_finder_impl".to_string()));
        assert!(loops.contains(&"reviewer".to_string()));
        assert!(loops.contains(&"implementer".to_string()));
    }

    #[test]
    fn trust_band_config_maps_to_brain_band() {
        let band: TrustBand = TrustBandConfig::Conservative.into();
        assert_eq!(band, TrustBand::Conservative);
        let band: TrustBand = TrustBandConfig::Aggressive.into();
        assert_eq!(band, TrustBand::Aggressive);
    }

    #[test]
    fn builder_config_new_uses_documented_defaults() {
        let cfg = BuilderConfig::new("foo/bar");
        assert_eq!(cfg.target_slug, "foo/bar");
        assert!(cfg.repo_path.is_none());
        assert_eq!(cfg.trust_band, TrustBandConfig::Conservative);
        assert!(cfg.label_filter.is_none());
        assert_eq!(cfg.loops.len(), 4);
    }
}
