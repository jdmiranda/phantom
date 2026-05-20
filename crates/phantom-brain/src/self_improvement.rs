//! Brain self-improvement scoring + auto-enqueue.
//!
//! This module implements §3–§5 of the brain self-improvement design doc:
//!
//! - **Scoring** ([`score_candidate`]): the weighted-sum scorer that maps a
//!   [`GoalCandidate`] to a `[0.0, 1.0]` utility score using the signal
//!   weights from §3 (priority 0.30, age 0.15, activity 0.10, recent CI 0.20,
//!   blocked_by 0.10, label bonus/penalty up to 0.15).
//! - **Hard exclusions** ([`HardExclusions::matches`]): a single predicate
//!   returning `Some(reason)` for any candidate that MUST NOT auto-enqueue
//!   regardless of score (security label, draft, brain-authored, etc.).
//! - **Trust budget** ([`TrustBudget`]): a counter in `[0, 20]` with four
//!   operating bands (suggestion-only / conservative / standard / aggressive).
//!   Drives both the score threshold and the per-hour cap.
//! - **Rate limits** ([`RateLimiter`]): sliding-window per-hour and per-day
//!   counters plus an enqueue cooldown.
//! - **Audit log** ([`AuditEntry`]): every decision (enqueue or skip) appends
//!   one envelope to a JSONL log; rotation is deferred to a follow-up.
//! - **Self-improvement state** ([`SelfImprovementState`]): the orchestrator
//!   struct the brain owns and calls into on each tick.
//!
//! The substrate driver that actually spawns the implementer agent for an
//! enqueued message is a separate follow-up — this PR lands the brain-side
//! decision logic and emits [`AiAction::EnqueueLoopMessage`] for the app
//! handler to forward to a `LoopQueueRegistry`.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use crate::events::AiAction;
use crate::goal_source::{GoalCandidate, GoalSource};

// ---------------------------------------------------------------------------
// Constants & defaults (design doc §3, §4, §5)
// ---------------------------------------------------------------------------

/// Default queue name the brain auto-enqueues to (design doc §4.1).
pub const DEFAULT_IMPLEMENTER_QUEUE: &str = "implementer-queue";

/// Standard-band score threshold for auto-enqueue (§5.4).
pub const STANDARD_THRESHOLD: f64 = 0.75;
/// Conservative-band threshold (§5.4 band 1–3).
pub const CONSERVATIVE_THRESHOLD: f64 = 0.85;
/// Aggressive-band threshold (§5.4 band 10–20).
pub const AGGRESSIVE_THRESHOLD: f64 = 0.65;

/// Critical-label score floor (§7.1 risk mitigation).
pub const CRITICAL_LABEL_FLOOR: f64 = 0.85;

/// Default per-hour cap (§4.2).
pub const DEFAULT_PER_HOUR: u32 = 4;
/// Default per-day cap (§4.2).
pub const DEFAULT_PER_DAY: u32 = 12;
/// Default in-flight cap (§4.2).
pub const DEFAULT_MAX_IN_FLIGHT: u32 = 1;
/// Default cooldown between consecutive auto-enqueues (§4.2).
pub const DEFAULT_COOLDOWN: Duration = Duration::from_secs(600);

/// Maximum body length forwarded onto the implementer payload. Truncates
/// long issue descriptions on a UTF-8 boundary to keep the queue payload
/// bounded.
pub const PAYLOAD_BODY_MAX_BYTES: usize = 8 * 1024;

/// Trust budget starting value (§5.4).
pub const TRUST_BUDGET_START: u32 = 4;
/// Trust budget cap (§5.4).
pub const TRUST_BUDGET_CAP: u32 = 20;

// ---------------------------------------------------------------------------
// SelfImprovementConfig
// ---------------------------------------------------------------------------

/// Tunable knobs for the self-improvement reconciler.
///
/// Construct via [`Self::default()`] for the design-doc defaults, or build
/// piecewise to override individual fields. The config is plain-data so a
/// caller can clone, mutate, and reapply with [`SelfImprovementState::set_config`].
#[derive(Debug, Clone)]
pub struct SelfImprovementConfig {
    /// Target loop queue name (default: `"implementer-queue"`).
    pub queue_name: String,
    /// Per-hour absolute ceiling.
    pub per_hour: u32,
    /// Per-day absolute ceiling.
    pub per_day: u32,
    /// Max simultaneous in-flight implementer items (informational only at
    /// the brain layer; enforced by the app-side handler when it knows
    /// queue depth).
    pub max_in_flight: u32,
    /// Cooldown between successive auto-enqueues.
    pub cooldown: Duration,
    /// Master kill switch — when `false`, every tick is a no-op.
    /// Mirrors `BrainConfig::enable_self_improvement` and the
    /// `PHANTOM_DISABLE_SELF_IMPROVEMENT=1` env var.
    pub enabled: bool,
    /// Path to the JSONL audit log. `None` disables persistence (in-memory
    /// only, accessible via [`SelfImprovementState::recent_audit_entries`]).
    pub audit_log_path: Option<PathBuf>,
}

impl Default for SelfImprovementConfig {
    fn default() -> Self {
        Self {
            queue_name: DEFAULT_IMPLEMENTER_QUEUE.into(),
            per_hour: DEFAULT_PER_HOUR,
            per_day: DEFAULT_PER_DAY,
            max_in_flight: DEFAULT_MAX_IN_FLIGHT,
            cooldown: DEFAULT_COOLDOWN,
            // Default OFF per §5.1 — operator must opt in.
            enabled: false,
            audit_log_path: None,
        }
    }
}

// ---------------------------------------------------------------------------
// TrustBudget (design doc §5.4)
// ---------------------------------------------------------------------------

/// Persistent counter that ramps brain autonomy up on successes and down on
/// failures.
///
/// Four operating bands (§5.4 table):
///
/// | budget | band            | threshold | per-hour       |
/// |--------|-----------------|-----------|----------------|
/// | 0      | suggestion-only | n/a       | 0 (no enqueue) |
/// | 1–3    | conservative    | 0.85      | half default   |
/// | 4–9    | standard        | 0.75      | default        |
/// | 10–20  | aggressive      | 0.65      | doubled        |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrustBudget {
    score: u32,
}

/// Symbolic identifier for one of the four trust bands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustBand {
    /// Budget == 0: suggestion-only; no auto-enqueue regardless of score.
    SuggestionOnly,
    /// Budget 1–3: raised threshold (0.85), halved per-hour ceiling.
    Conservative,
    /// Budget 4–9: design-doc defaults.
    Standard,
    /// Budget 10–20: lowered threshold (0.65), doubled per-hour ceiling.
    Aggressive,
}

impl TrustBudget {
    /// Construct at the design-doc starting value (4 = standard band).
    #[must_use]
    pub fn new() -> Self {
        Self {
            score: TRUST_BUDGET_START,
        }
    }

    /// Construct from a raw score; clamps to `[0, TRUST_BUDGET_CAP]`.
    #[must_use]
    pub fn from_score(score: u32) -> Self {
        Self {
            score: score.min(TRUST_BUDGET_CAP),
        }
    }

    /// Current budget value in `[0, TRUST_BUDGET_CAP]`.
    #[must_use]
    pub fn score(self) -> u32 {
        self.score
    }

    /// Increment on a successful PR-merged feedback signal (capped at 20).
    pub fn record_success(&mut self) {
        self.score = (self.score + 1).min(TRUST_BUDGET_CAP);
    }

    /// Decrement on a failure / revert / abandon (saturates at 0).
    pub fn record_failure(&mut self) {
        self.score = self.score.saturating_sub(1);
    }

    /// Map the current score to a band.
    #[must_use]
    pub fn band(self) -> TrustBand {
        match self.score {
            0 => TrustBand::SuggestionOnly,
            1..=3 => TrustBand::Conservative,
            4..=9 => TrustBand::Standard,
            _ => TrustBand::Aggressive,
        }
    }

    /// Score threshold required for auto-enqueue at the current band.
    #[must_use]
    pub fn threshold(self) -> f64 {
        match self.band() {
            TrustBand::SuggestionOnly => 1.01, // unreachable — no enqueue
            TrustBand::Conservative => CONSERVATIVE_THRESHOLD,
            TrustBand::Standard => STANDARD_THRESHOLD,
            TrustBand::Aggressive => AGGRESSIVE_THRESHOLD,
        }
    }

    /// Per-hour cap given the configured default. Conservative band halves
    /// the cap; aggressive doubles it; suggestion-only zeroes it.
    #[must_use]
    pub fn per_hour_cap(self, default: u32) -> u32 {
        match self.band() {
            TrustBand::SuggestionOnly => 0,
            TrustBand::Conservative => default / 2,
            TrustBand::Standard => default,
            TrustBand::Aggressive => default.saturating_mul(2),
        }
    }
}

impl Default for TrustBudget {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// HardExclusions (design doc §5.3)
// ---------------------------------------------------------------------------

/// Single-predicate hard-exclusion check.
///
/// Returns `Some(reason)` for any candidate that MUST NOT auto-enqueue
/// regardless of computed score. Called BEFORE scoring so the audit log
/// records the exclusion reason precisely.
#[derive(Debug, Clone)]
pub struct HardExclusions {
    /// Author handles that should be excluded — used to break the
    /// "brain enqueues issues authored by the brain" runaway loop (§7.3 #1).
    /// Default: `["phantom-brain", "phantom-brain[bot]", "github-actions[bot]"]`.
    pub excluded_authors: Vec<String>,
}

impl Default for HardExclusions {
    fn default() -> Self {
        Self {
            excluded_authors: vec![
                "phantom-brain".into(),
                "github-actions[bot]".into(),
                "phantom-brain[bot]".into(),
            ],
        }
    }
}

impl HardExclusions {
    /// Check `candidate` against every exclusion rule. Returns the first
    /// matching reason (or `None` if the candidate is eligible to score).
    #[must_use]
    pub fn matches(&self, candidate: &GoalCandidate) -> Option<&'static str> {
        // Security label → never auto-enqueue (§5.3 #1).
        if candidate.signal("is_security") > 0.0 || candidate.has_label("security") {
            return Some("security label");
        }
        // Draft state → not ready by definition (§5.3 #2).
        if candidate.has_label("draft") || candidate.has_label("WIP") || candidate.has_label("wip")
        {
            return Some("draft / WIP");
        }
        // Body contains explicit needs-design markers (§5.3 #3).
        if let Some(body) = &candidate.body {
            let lower = body.to_ascii_lowercase();
            if lower.contains("[ ] design needed")
                || lower.contains("design needed")
                || lower.contains("wip")
            {
                return Some("body marks WIP / design needed");
            }
        }
        // Labels that explicitly opt out (§5.3 #4).
        // `auto-triage-skip` is applied by the triager loop when it decides
        // the issue is not directly actionable (tracking epics, meta-work).
        // Without this exclusion the brain re-enqueues the same epic every
        // daemon restart and the triager bills another agent run to make
        // the same skip decision again.
        if candidate.has_label("do-not-auto-implement")
            || candidate.has_label("needs-discussion")
            || candidate.has_label("needs-spec")
            || candidate.has_label("auto-triage-skip")
            || candidate.has_label("auto-triage-close")
        {
            return Some("opt-out label present");
        }
        // Self-authored issues → runaway-loop guard (§5.3 #5).
        if let Some(author) = &candidate.author
            && self.excluded_authors.iter().any(|a| a == author)
        {
            return Some("self-authored");
        }
        None
    }
}

// ---------------------------------------------------------------------------
// Scoring (design doc §3)
// ---------------------------------------------------------------------------

/// Per-signal contribution to the final score.
///
/// Returned alongside the score itself so the audit log can record exactly
/// what drove a decision. Field names mirror the design-doc table headings.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ScoreBreakdown {
    pub priority_rank: f64,
    pub age_hours: f64,
    pub activity_count: f64,
    pub recent_ci_failure_count: f64,
    pub blocked_by_count: f64,
    pub labels_bonus: f64,
    pub critical_floor_applied: bool,
}

impl ScoreBreakdown {
    /// Sum of all weighted contributions (before the critical-label floor
    /// override).
    #[must_use]
    pub fn weighted_sum(&self) -> f64 {
        self.priority_rank
            + self.age_hours
            + self.activity_count
            + self.recent_ci_failure_count
            + self.blocked_by_count
            + self.labels_bonus
    }
}

/// Score one candidate per §3 weighted-sum rules.
///
/// Returns the final score (clamped to `[0.0, 1.0]`) and the breakdown.
/// A candidate carrying a `critical` / `regression` / `blocker` label is
/// floored at [`CRITICAL_LABEL_FLOOR`] (§7.1).
#[must_use]
pub fn score_candidate(candidate: &GoalCandidate) -> (f64, ScoreBreakdown) {
    let mut b = ScoreBreakdown::default();

    // Priority rank: 0..4 mapped linearly to [0, 1] then weighted by 0.30.
    let priority = (candidate.signal("priority_rank") / 4.0).clamp(0.0, 1.0);
    b.priority_rank = priority * 0.30;

    // Age hours: inverted logistic centered around 24 h.
    // Fresh (0 h) ≈ 0.73; 1 day ≈ 0.5; 1 week ≈ 0.005. Scale by 0.15.
    let age = candidate.signal("age_hours");
    let logistic = 1.0 / (1.0 + (0.04 * (age - 24.0)).exp());
    b.age_hours = logistic.clamp(0.0, 1.0) * 0.15;

    // Activity count: log curve up to ~5 comments saturates; scale by 0.10.
    let activity = candidate.signal("activity_count").max(0.0);
    let normalized = (activity + 1.0).ln() / 6.0_f64.ln();
    b.activity_count = normalized.clamp(0.0, 1.0) * 0.10;

    // Recent CI failures: linear 0..5 clamp, scale by 0.20.
    let ci = candidate.signal("recent_ci_failure_count");
    let ci_norm = (ci / 5.0).clamp(0.0, 1.0);
    b.recent_ci_failure_count = ci_norm * 0.20;

    // Blocked-by: inverted linear — fewer blockers is better; weight 0.10.
    let blocked = candidate.signal("blocked_by_count");
    let blocked_norm = ((5.0 - blocked) / 5.0).clamp(0.0, 1.0);
    b.blocked_by_count = blocked_norm * 0.10;

    // Label bonus / penalty (§3 table tail):
    // good-first-issue → +0.05; needs-spec → -0.10. Clamp final to [0, 1].
    let mut labels = 0.0;
    if candidate.signal("has_good_first_issue") > 0.0 {
        labels += 0.05;
    }
    if candidate.signal("has_needs_spec") > 0.0 {
        labels -= 0.10;
    }
    b.labels_bonus = labels;

    let raw = b.weighted_sum();
    let mut score = raw.clamp(0.0, 1.0);

    // Critical-label score floor (§7.1) — any label "containing critical"
    // (the bare `critical` form OR the namespaced `priority:critical` /
    // `P0` form), or `regression` / `blocker`, forces score >=
    // CRITICAL_LABEL_FLOOR. Implementation walks the label list with
    // `contains("critical")` to keep the rule label-vocabulary-agnostic.
    let force = candidate
        .labels
        .iter()
        .any(|l| l.contains("critical") || l == "regression" || l == "blocker" || l == "P0")
        || candidate.signal("has_regression") > 0.0
        || candidate.signal("is_blocker") > 0.0;
    if force && score < CRITICAL_LABEL_FLOOR {
        score = CRITICAL_LABEL_FLOOR;
        b.critical_floor_applied = true;
    }

    (score, b)
}

// ---------------------------------------------------------------------------
// RateLimiter (design doc §4.2)
// ---------------------------------------------------------------------------

/// Sliding-window per-hour + per-day counters plus a fixed cooldown.
///
/// Times are stored as `Instant` so the limiter is portable across system-clock
/// jumps. Tests can drive synthetic time via [`Self::tick_at`] which is the
/// only entry-point that records an enqueue.
#[derive(Debug, Clone)]
pub struct RateLimiter {
    per_hour: u32,
    per_day: u32,
    cooldown: Duration,
    history: VecDeque<Instant>,
    last: Option<Instant>,
}

impl RateLimiter {
    /// Build a limiter with the design-doc defaults.
    #[must_use]
    pub fn new() -> Self {
        Self::with_caps(DEFAULT_PER_HOUR, DEFAULT_PER_DAY, DEFAULT_COOLDOWN)
    }

    /// Build a limiter with explicit caps. Useful for tests and band-adjusted
    /// caps from [`TrustBudget`].
    #[must_use]
    pub fn with_caps(per_hour: u32, per_day: u32, cooldown: Duration) -> Self {
        Self {
            per_hour,
            per_day,
            cooldown,
            history: VecDeque::new(),
            last: None,
        }
    }

    /// Reason a "should I allow this enqueue" check returned `Some`.
    /// Returns `None` when the enqueue should go through.
    #[must_use]
    pub fn check(&self, now: Instant) -> Option<RateLimitReason> {
        let hour_window = Duration::from_secs(3600);
        let day_window = Duration::from_secs(86_400);

        if let Some(last) = self.last
            && now.duration_since(last) < self.cooldown
        {
            return Some(RateLimitReason::Cooldown);
        }

        let hour_count = self
            .history
            .iter()
            .rev()
            .take_while(|t| now.duration_since(**t) <= hour_window)
            .count() as u32;
        if hour_count >= self.per_hour {
            return Some(RateLimitReason::PerHour { cap: self.per_hour });
        }

        let day_count = self
            .history
            .iter()
            .rev()
            .take_while(|t| now.duration_since(**t) <= day_window)
            .count() as u32;
        if day_count >= self.per_day {
            return Some(RateLimitReason::PerDay { cap: self.per_day });
        }

        None
    }

    /// Record a successful enqueue at `now`. Prunes any entries older than
    /// the day window to bound the deque.
    pub fn tick_at(&mut self, now: Instant) {
        self.history.push_back(now);
        self.last = Some(now);
        let day = Duration::from_secs(86_400);
        while let Some(front) = self.history.front()
            && now.duration_since(*front) > day
        {
            self.history.pop_front();
        }
    }

    /// Adjust the caps in place (used when the trust band changes).
    pub fn set_caps(&mut self, per_hour: u32, per_day: u32, cooldown: Duration) {
        self.per_hour = per_hour;
        self.per_day = per_day;
        self.cooldown = cooldown;
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

/// Reason the rate-limit gate refused an enqueue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateLimitReason {
    /// Cooldown between successive enqueues has not elapsed.
    Cooldown,
    /// Per-hour cap was hit.
    PerHour { cap: u32 },
    /// Per-day cap was hit.
    PerDay { cap: u32 },
}

impl std::fmt::Display for RateLimitReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cooldown => write!(f, "cooldown"),
            Self::PerHour { cap } => write!(f, "per-hour cap ({cap}) exceeded"),
            Self::PerDay { cap } => write!(f, "per-day cap ({cap}) exceeded"),
        }
    }
}

// ---------------------------------------------------------------------------
// Audit log (design doc §5.2)
// ---------------------------------------------------------------------------

/// Outcome of a single self-improvement decision.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditDecision {
    /// Candidate was enqueued.
    Enqueued,
    /// Candidate was rejected by hard-exclusion.
    SkippedExcluded,
    /// Candidate scored below the active threshold.
    SkippedLowScore,
    /// Candidate was suppressed by rate-limit / cooldown / per-hour cap.
    SkippedRateLimited,
    /// Trust band == SuggestionOnly — kill switch on.
    SkippedSuggestionOnly,
    /// Self-improvement feature is disabled.
    SkippedDisabled,
}

/// One audit-log envelope. Serialized JSONL per §5.2.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AuditEntry {
    /// Wall-clock unix-millis.
    pub ts_unix_ms: u64,
    /// Candidate's `external_id`.
    pub external_id: String,
    /// Candidate's source (`gh-issues`, `gh-ci-failures`, …).
    pub source: String,
    /// Score computed (irrespective of decision).
    pub score: f64,
    /// Per-signal breakdown.
    pub score_breakdown: ScoreBreakdown,
    /// Decision taken.
    pub decision: AuditDecision,
    /// Free-form reason — e.g. `"security label"`, `"below threshold (0.42 < 0.75)"`.
    pub reason: String,
    /// Trust budget at the moment of the decision.
    pub trust_budget: u32,
}

// ---------------------------------------------------------------------------
// SelfImprovementState — the orchestrator
// ---------------------------------------------------------------------------

/// Outcome of one `evaluate` call against a single candidate. Tests assert on
/// this shape directly; the brain layer uses `actions` to drive the action
/// channel and uses `decision` for telemetry.
#[derive(Debug)]
pub struct EvaluationOutcome {
    /// The audit entry written for this decision.
    pub audit: AuditEntry,
    /// `Some(action)` when the decision was `Enqueued`; otherwise `None`.
    pub action: Option<AiAction>,
}

/// Stateful reconciler the brain owns and ticks every 60 s.
///
/// Holds the trust budget, rate limiter, hard-exclusion config, and the
/// in-memory audit-log tail. The brain (`brain_loop`) calls [`Self::evaluate`]
/// for each candidate produced by polling its [`GoalSource`]s and forwards
/// any returned [`AiAction`] to its action channel.
#[derive(Debug)]
pub struct SelfImprovementState {
    config: SelfImprovementConfig,
    trust_budget: TrustBudget,
    rate_limit: RateLimiter,
    hard_exclusions: HardExclusions,
    /// Recent decisions in chronological order; bounded by [`Self::AUDIT_TAIL_CAP`].
    audit_tail: VecDeque<AuditEntry>,
    /// Set of `external_id`s the state has already evaluated. Used so the
    /// same candidate is not re-scored on every tick.
    seen: std::collections::HashSet<String>,
}

impl SelfImprovementState {
    /// Cap on the in-memory audit-log tail. JSONL persistence (§5.2) writes
    /// every entry; this bound only governs `recent_audit_entries`.
    pub const AUDIT_TAIL_CAP: usize = 256;

    /// Construct with the given config and a fresh trust budget at
    /// [`TRUST_BUDGET_START`].
    #[must_use]
    pub fn new(config: SelfImprovementConfig) -> Self {
        Self::with_trust_budget(config, TrustBudget::new())
    }

    /// Construct with the given config and an explicit starting trust budget.
    ///
    /// The standard [`Self::new`] always opens at [`TRUST_BUDGET_START`] (4 =
    /// standard band). Callers that need to begin at a different band — most
    /// notably the `phantom-builder` crate, which exposes a `--trust-band`
    /// CLI flag — use this constructor to seed the budget directly. The
    /// per-hour cap on the rate limiter is recomputed against the supplied
    /// budget so the operator's choice takes effect immediately.
    #[must_use]
    pub fn with_trust_budget(config: SelfImprovementConfig, budget: TrustBudget) -> Self {
        let rate_limit = RateLimiter::with_caps(
            budget.per_hour_cap(config.per_hour),
            config.per_day,
            config.cooldown,
        );
        Self {
            config,
            trust_budget: budget,
            rate_limit,
            hard_exclusions: HardExclusions::default(),
            audit_tail: VecDeque::new(),
            seen: std::collections::HashSet::new(),
        }
    }

    /// Replace the config. Updates the rate limiter to match.
    pub fn set_config(&mut self, config: SelfImprovementConfig) {
        self.rate_limit.set_caps(
            self.trust_budget.per_hour_cap(config.per_hour),
            config.per_day,
            config.cooldown,
        );
        self.config = config;
    }

    /// Current trust budget (for telemetry / persistence).
    #[must_use]
    pub fn trust_budget(&self) -> TrustBudget {
        self.trust_budget
    }

    /// Replace the hard-exclusion config (used by tests).
    pub fn set_hard_exclusions(&mut self, excl: HardExclusions) {
        self.hard_exclusions = excl;
    }

    /// Record a successful PR-merged feedback signal. Bumps the trust budget
    /// and recomputes the rate-limit caps for the new band.
    pub fn record_success(&mut self) {
        self.trust_budget.record_success();
        self.rate_limit.set_caps(
            self.trust_budget.per_hour_cap(self.config.per_hour),
            self.config.per_day,
            self.config.cooldown,
        );
    }

    /// Record a failure (CI never green, revert, abandon). Decrements the
    /// trust budget and recomputes caps.
    pub fn record_failure(&mut self) {
        self.trust_budget.record_failure();
        self.rate_limit.set_caps(
            self.trust_budget.per_hour_cap(self.config.per_hour),
            self.config.per_day,
            self.config.cooldown,
        );
    }

    /// Bounded snapshot of the most recent audit entries (chronological).
    #[must_use]
    pub fn recent_audit_entries(&self) -> Vec<AuditEntry> {
        self.audit_tail.iter().cloned().collect()
    }

    /// Evaluate one candidate against the full gate chain. The default
    /// `Instant::now()` is replaced by `now` so tests can drive synthetic
    /// time through hand-crafted offsets.
    pub fn evaluate(&mut self, candidate: &GoalCandidate, now: Instant) -> EvaluationOutcome {
        // Always recompute score for the audit envelope, even when we skip.
        let (score, breakdown) = score_candidate(candidate);

        // 1. Disabled kill switch (§5.1) — short-circuits everything.
        if !self.config.enabled {
            return self.record(
                candidate,
                score,
                breakdown,
                AuditDecision::SkippedDisabled,
                "self-improvement disabled".into(),
                None,
            );
        }

        // 2. Already-evaluated dedup — never re-score the same external_id.
        if self.seen.contains(&candidate.external_id) {
            return self.record(
                candidate,
                score,
                breakdown,
                AuditDecision::SkippedExcluded,
                "duplicate external_id".into(),
                None,
            );
        }
        self.seen.insert(candidate.external_id.clone());

        // 3. Trust band guard (§5.4) — suggestion-only mode never enqueues.
        if matches!(self.trust_budget.band(), TrustBand::SuggestionOnly) {
            return self.record(
                candidate,
                score,
                breakdown,
                AuditDecision::SkippedSuggestionOnly,
                "trust budget = 0 (suggestion-only)".into(),
                None,
            );
        }

        // 4. Hard exclusions (§5.3) — security, draft, self-authored, etc.
        if let Some(reason) = self.hard_exclusions.matches(candidate) {
            return self.record(
                candidate,
                score,
                breakdown,
                AuditDecision::SkippedExcluded,
                reason.to_string(),
                None,
            );
        }

        // 5. Threshold (§5.4 per-band). The env var
        // `PHANTOM_SCORE_THRESHOLD` overrides the band threshold for live
        // testing — useful when phantom's own issue board has no signals
        // that the scorer rewards (no priority labels, no recent CI fails,
        // no comments) and every candidate scores ~0.25, which can't cross
        // any band's default threshold.
        let threshold = std::env::var("PHANTOM_SCORE_THRESHOLD")
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or_else(|| self.trust_budget.threshold());
        if score < threshold {
            return self.record(
                candidate,
                score,
                breakdown,
                AuditDecision::SkippedLowScore,
                format!("below threshold ({score:.3} < {threshold:.3})"),
                None,
            );
        }

        // 6. Rate limits (§4.2).
        if let Some(reason) = self.rate_limit.check(now) {
            return self.record(
                candidate,
                score,
                breakdown,
                AuditDecision::SkippedRateLimited,
                reason.to_string(),
                None,
            );
        }

        // 7. Build the enqueue payload (§4.1).
        let payload = build_payload(candidate, &breakdown, score, self.trust_budget);
        let action = AiAction::EnqueueLoopMessage {
            queue: self.config.queue_name.clone(),
            from_source: candidate.source.clone(),
            payload,
        };

        // Record the enqueue against the rate limiter only on success.
        self.rate_limit.tick_at(now);

        self.record(
            candidate,
            score,
            breakdown,
            AuditDecision::Enqueued,
            format!("enqueued to {}", self.config.queue_name),
            Some(action),
        )
    }

    fn record(
        &mut self,
        candidate: &GoalCandidate,
        score: f64,
        breakdown: ScoreBreakdown,
        decision: AuditDecision,
        reason: String,
        action: Option<AiAction>,
    ) -> EvaluationOutcome {
        let ts_unix_ms = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0);
        let entry = AuditEntry {
            ts_unix_ms,
            external_id: candidate.external_id.clone(),
            source: candidate.source.clone(),
            score,
            score_breakdown: breakdown,
            decision,
            reason,
            trust_budget: self.trust_budget.score(),
        };
        // Persist to JSONL if configured. A persistence error is logged and
        // dropped — the brain MUST NOT block on the audit log.
        if let Some(path) = &self.config.audit_log_path
            && let Err(e) = append_audit_jsonl(path, &entry)
        {
            log::warn!("self-improvement: failed to append audit log: {e}");
        }
        // In-memory tail with cap.
        if self.audit_tail.len() >= Self::AUDIT_TAIL_CAP {
            self.audit_tail.pop_front();
        }
        self.audit_tail.push_back(entry.clone());
        EvaluationOutcome {
            audit: entry,
            action,
        }
    }

    /// Convenience: run [`GoalSource::poll`] on every source in `sources`,
    /// then [`Self::evaluate`] each candidate at the current `Instant`.
    /// Returns the list of `AiAction::EnqueueLoopMessage` actions to forward.
    pub fn tick(&mut self, sources: &mut [Box<dyn GoalSource>]) -> Vec<AiAction> {
        let now = Instant::now();
        let mut actions = Vec::new();
        for src in sources.iter_mut() {
            let candidates = match src.poll() {
                Ok(c) => c,
                Err(e) => {
                    log::warn!(
                        "self-improvement: source `{}` poll failed: {e}",
                        src.name()
                    );
                    continue;
                }
            };
            for c in candidates {
                let outcome = self.evaluate(&c, now);
                if let Some(action) = outcome.action {
                    actions.push(action);
                }
            }
        }
        actions
    }
}

/// Build the JSON payload emitted on [`AiAction::EnqueueLoopMessage::payload`]
/// per design doc §4.1.
fn build_payload(
    candidate: &GoalCandidate,
    breakdown: &ScoreBreakdown,
    score: f64,
    trust_budget: TrustBudget,
) -> serde_json::Value {
    let body = candidate.body.as_deref().unwrap_or("");
    let body_truncated = if body.len() > PAYLOAD_BODY_MAX_BYTES {
        // Truncate on a UTF-8 char boundary by stepping back until valid.
        let mut end = PAYLOAD_BODY_MAX_BYTES;
        while end > 0 && !body.is_char_boundary(end) {
            end -= 1;
        }
        &body[..end]
    } else {
        body
    };
    let ts = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0);
    serde_json::json!({
        "external_id": candidate.external_id,
        "title": candidate.title,
        "body": body_truncated,
        "url": candidate.url,
        "source": candidate.source,
        "labels": candidate.labels,
        "score": score,
        "score_breakdown": breakdown,
        "discovered_at_unix_ms": ts,
        "trust_budget_remaining": trust_budget.score(),
    })
}

/// Append one JSONL entry to `path`, creating parent directories as needed.
fn append_audit_jsonl(path: &std::path::Path, entry: &AuditEntry) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let line = serde_json::to_string(entry)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    writeln!(f, "{line}")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::time::SystemTime;

    fn mk_candidate(
        external_id: &str,
        priority: f64,
        labels: Vec<&str>,
        signals: &[(&str, f64)],
    ) -> GoalCandidate {
        let mut s: HashMap<String, f64> = HashMap::new();
        s.insert("priority_rank".into(), priority);
        // Default zero for every signal the scorer reads.
        for k in [
            "age_hours",
            "activity_count",
            "blocked_by_count",
            "recent_ci_failure_count",
            "is_security",
            "has_good_first_issue",
            "has_needs_spec",
        ] {
            s.insert(k.into(), 0.0);
        }
        for (k, v) in signals {
            s.insert((*k).into(), *v);
        }
        GoalCandidate {
            source: "gh-issues".into(),
            external_id: external_id.into(),
            title: "t".into(),
            body: Some("b".into()),
            labels: labels.iter().map(|l| (*l).into()).collect(),
            created_at: SystemTime::now(),
            signals: s,
            url: None,
            author: None,
        }
    }

    #[test]
    fn score_high_when_priority_critical() {
        let c = mk_candidate("gh-issue:1", 4.0, vec![], &[("age_hours", 1.0)]);
        let (score, _) = score_candidate(&c);
        // priority 4/4 * 0.30 = 0.30 contribution alone; age fresh adds ~0.11.
        assert!(score >= 0.40, "expected >= 0.40, got {score}");
    }

    #[test]
    fn score_low_when_priority_none() {
        let c = mk_candidate("gh-issue:2", 0.0, vec![], &[]);
        let (score, _) = score_candidate(&c);
        assert!(score < 0.30, "expected < 0.30, got {score}");
    }

    #[test]
    fn score_decays_with_age_past_one_week() {
        let fresh = mk_candidate("a", 4.0, vec![], &[("age_hours", 1.0)]);
        let stale = mk_candidate("b", 4.0, vec![], &[("age_hours", 720.0)]); // ~1 month
        let (s_fresh, _) = score_candidate(&fresh);
        let (s_stale, _) = score_candidate(&stale);
        assert!(s_fresh > s_stale, "fresh {s_fresh} should beat stale {s_stale}");
    }

    #[test]
    fn score_boost_when_recent_ci_failures_present() {
        let no_ci = mk_candidate("a", 3.0, vec![], &[]);
        let with_ci = mk_candidate("b", 3.0, vec![], &[("recent_ci_failure_count", 5.0)]);
        let (s_no, _) = score_candidate(&no_ci);
        let (s_with, _) = score_candidate(&with_ci);
        // CI signal contributes up to 0.20.
        assert!(
            s_with - s_no >= 0.19,
            "expected CI boost >= 0.19, got delta {}",
            s_with - s_no
        );
    }

    #[test]
    fn score_penalty_for_blocked_by_count() {
        let no_block = mk_candidate("a", 3.0, vec![], &[]);
        let blocked = mk_candidate("b", 3.0, vec![], &[("blocked_by_count", 5.0)]);
        let (s_no, _) = score_candidate(&no_block);
        let (s_blocked, _) = score_candidate(&blocked);
        assert!(s_no > s_blocked, "blocked should score lower");
    }

    #[test]
    fn score_bonus_for_good_first_issue_label() {
        let plain = mk_candidate("a", 3.0, vec![], &[]);
        let gfi = mk_candidate(
            "b",
            3.0,
            vec!["good-first-issue"],
            &[("has_good_first_issue", 1.0)],
        );
        let (s_plain, _) = score_candidate(&plain);
        let (s_gfi, _) = score_candidate(&gfi);
        assert!(s_gfi > s_plain, "good-first-issue should boost");
    }

    #[test]
    fn score_clamps_to_zero_when_negative_components_dominate() {
        let c = mk_candidate(
            "z",
            0.0,
            vec!["needs-spec"],
            &[("has_needs_spec", 1.0), ("age_hours", 10000.0)],
        );
        let (score, _) = score_candidate(&c);
        assert!(score >= 0.0, "score must be clamped to >= 0, got {score}");
    }

    #[test]
    fn score_floor_for_critical_label_override() {
        let c = mk_candidate(
            "x",
            1.0, // low priority numerically
            vec!["critical"],
            &[("age_hours", 10000.0)], // stale
        );
        let (score, breakdown) = score_candidate(&c);
        assert!(
            score >= CRITICAL_LABEL_FLOOR,
            "critical label must floor the score, got {score}"
        );
        assert!(breakdown.critical_floor_applied);
    }

    // ----- Hard exclusions -----

    #[test]
    fn security_label_is_excluded() {
        let c = mk_candidate("s", 4.0, vec!["security"], &[("is_security", 1.0)]);
        assert_eq!(HardExclusions::default().matches(&c), Some("security label"));
    }

    #[test]
    fn draft_label_is_excluded() {
        let c = mk_candidate("d", 4.0, vec!["draft"], &[]);
        assert_eq!(HardExclusions::default().matches(&c), Some("draft / WIP"));
    }

    #[test]
    fn opt_out_labels_are_excluded() {
        for lbl in ["do-not-auto-implement", "needs-discussion", "needs-spec"] {
            let c = mk_candidate("o", 4.0, vec![lbl], &[]);
            assert_eq!(
                HardExclusions::default().matches(&c),
                Some("opt-out label present"),
                "label {lbl} should be excluded",
            );
        }
    }

    #[test]
    fn self_authored_is_excluded() {
        let mut c = mk_candidate("a", 4.0, vec![], &[]);
        c.author = Some("phantom-brain".into());
        assert_eq!(HardExclusions::default().matches(&c), Some("self-authored"));
    }

    #[test]
    fn human_authored_is_not_excluded_by_author_rule() {
        let mut c = mk_candidate("a", 4.0, vec![], &[]);
        c.author = Some("jdmiranda".into());
        assert_eq!(HardExclusions::default().matches(&c), None);
    }

    // ----- Trust budget -----

    #[test]
    fn trust_budget_starts_in_standard_band() {
        let b = TrustBudget::new();
        assert_eq!(b.band(), TrustBand::Standard);
        assert!((b.threshold() - STANDARD_THRESHOLD).abs() < f64::EPSILON);
    }

    #[test]
    fn trust_budget_record_success_increments_capped() {
        let mut b = TrustBudget::from_score(19);
        b.record_success();
        b.record_success();
        b.record_success();
        assert_eq!(b.score(), TRUST_BUDGET_CAP);
    }

    #[test]
    fn trust_budget_record_failure_saturates_at_zero() {
        let mut b = TrustBudget::from_score(2);
        b.record_failure();
        b.record_failure();
        b.record_failure();
        assert_eq!(b.score(), 0);
        assert_eq!(b.band(), TrustBand::SuggestionOnly);
    }

    #[test]
    fn trust_band_per_hour_cap_scales_correctly() {
        assert_eq!(TrustBudget::from_score(0).per_hour_cap(4), 0);
        assert_eq!(TrustBudget::from_score(2).per_hour_cap(4), 2);
        assert_eq!(TrustBudget::from_score(5).per_hour_cap(4), 4);
        assert_eq!(TrustBudget::from_score(15).per_hour_cap(4), 8);
    }

    // ----- Rate limiter -----

    #[test]
    fn rate_limiter_cooldown_blocks_immediately_after_enqueue() {
        let mut rl = RateLimiter::with_caps(4, 12, Duration::from_secs(600));
        let t0 = Instant::now();
        rl.tick_at(t0);
        assert_eq!(rl.check(t0 + Duration::from_secs(1)), Some(RateLimitReason::Cooldown));
        assert_eq!(rl.check(t0 + Duration::from_secs(599)), Some(RateLimitReason::Cooldown));
        // Beyond cooldown: still under per-hour cap.
        assert_eq!(rl.check(t0 + Duration::from_secs(600)), None);
    }

    #[test]
    fn rate_limiter_enforces_per_hour_cap() {
        let mut rl = RateLimiter::with_caps(2, 12, Duration::ZERO);
        let t0 = Instant::now();
        rl.tick_at(t0);
        rl.tick_at(t0 + Duration::from_secs(1));
        // Third request within the hour window must hit the cap.
        assert_eq!(
            rl.check(t0 + Duration::from_secs(2)),
            Some(RateLimitReason::PerHour { cap: 2 })
        );
        // After an hour, the first entry rolls off — third request OK.
        assert_eq!(rl.check(t0 + Duration::from_secs(3601)), None);
    }

    #[test]
    fn rate_limiter_enforces_per_day_cap() {
        let mut rl = RateLimiter::with_caps(u32::MAX, 2, Duration::ZERO);
        let t0 = Instant::now();
        rl.tick_at(t0);
        rl.tick_at(t0 + Duration::from_secs(7200));
        assert_eq!(
            rl.check(t0 + Duration::from_secs(7300)),
            Some(RateLimitReason::PerDay { cap: 2 })
        );
    }

    // ----- SelfImprovementState end-to-end -----

    fn enabled_state() -> SelfImprovementState {
        let mut s = SelfImprovementState::new(SelfImprovementConfig {
            enabled: true,
            cooldown: Duration::ZERO,
            ..Default::default()
        });
        s.set_hard_exclusions(HardExclusions::default());
        s
    }

    #[test]
    fn evaluate_enqueues_high_priority_candidate() {
        let mut state = enabled_state();
        let c = mk_candidate(
            "gh-issue:100",
            4.0,
            vec!["priority:critical"],
            &[("age_hours", 1.0)],
        );
        let out = state.evaluate(&c, Instant::now());
        assert_eq!(out.audit.decision, AuditDecision::Enqueued);
        match out.action {
            Some(AiAction::EnqueueLoopMessage { ref queue, .. }) => {
                assert_eq!(queue, DEFAULT_IMPLEMENTER_QUEUE);
            }
            other => panic!("expected EnqueueLoopMessage, got {other:?}"),
        }
    }

    #[test]
    fn evaluate_skips_security_label_and_records_audit() {
        let mut state = enabled_state();
        let c = mk_candidate("gh-issue:200", 4.0, vec!["security"], &[("is_security", 1.0)]);
        let out = state.evaluate(&c, Instant::now());
        assert_eq!(out.audit.decision, AuditDecision::SkippedExcluded);
        assert_eq!(out.audit.reason, "security label");
        assert!(out.action.is_none());
    }

    #[test]
    fn evaluate_skips_low_score_candidate() {
        let mut state = enabled_state();
        let c = mk_candidate(
            "gh-issue:300",
            1.0,
            vec![],
            &[("age_hours", 5000.0)],
        );
        let out = state.evaluate(&c, Instant::now());
        assert_eq!(out.audit.decision, AuditDecision::SkippedLowScore);
        assert!(out.audit.reason.contains("below threshold"));
    }

    #[test]
    fn evaluate_skips_when_disabled() {
        let mut state = SelfImprovementState::new(SelfImprovementConfig {
            enabled: false,
            ..Default::default()
        });
        let c = mk_candidate("gh-issue:400", 4.0, vec![], &[]);
        let out = state.evaluate(&c, Instant::now());
        assert_eq!(out.audit.decision, AuditDecision::SkippedDisabled);
    }

    #[test]
    fn evaluate_skips_when_suggestion_only() {
        let mut state = enabled_state();
        // Force budget to 0.
        for _ in 0..TRUST_BUDGET_START + 1 {
            state.record_failure();
        }
        assert_eq!(state.trust_budget().band(), TrustBand::SuggestionOnly);
        let c = mk_candidate("gh-issue:500", 4.0, vec!["priority:critical"], &[]);
        let out = state.evaluate(&c, Instant::now());
        assert_eq!(out.audit.decision, AuditDecision::SkippedSuggestionOnly);
    }

    #[test]
    fn evaluate_respects_per_hour_cap() {
        let mut state = SelfImprovementState::new(SelfImprovementConfig {
            enabled: true,
            per_hour: 1,
            per_day: 12,
            cooldown: Duration::ZERO,
            ..Default::default()
        });
        let c1 = mk_candidate("gh-issue:601", 4.0, vec!["priority:critical"], &[("age_hours", 1.0)]);
        let c2 = mk_candidate("gh-issue:602", 4.0, vec!["priority:critical"], &[("age_hours", 1.0)]);
        let t0 = Instant::now();
        let r1 = state.evaluate(&c1, t0);
        assert_eq!(r1.audit.decision, AuditDecision::Enqueued);
        let r2 = state.evaluate(&c2, t0 + Duration::from_secs(1));
        assert_eq!(r2.audit.decision, AuditDecision::SkippedRateLimited);
    }

    #[test]
    fn dedupe_does_not_rescore_same_external_id() {
        let mut state = enabled_state();
        // Use the `priority:critical` label so the critical-floor (§7.1)
        // pushes the score above the default 0.75 threshold; the dedupe
        // path is what we are testing here, not the scoring weights.
        let c = mk_candidate(
            "gh-issue:777",
            4.0,
            vec!["priority:critical"],
            &[("age_hours", 1.0)],
        );
        let r1 = state.evaluate(&c, Instant::now());
        let r2 = state.evaluate(&c, Instant::now());
        assert_eq!(r1.audit.decision, AuditDecision::Enqueued);
        assert_eq!(r2.audit.decision, AuditDecision::SkippedExcluded);
        assert_eq!(r2.audit.reason, "duplicate external_id");
    }

    #[test]
    fn audit_tail_is_capped() {
        let mut state = enabled_state();
        for i in 0..SelfImprovementState::AUDIT_TAIL_CAP + 10 {
            let c = mk_candidate(&format!("gh-issue:{i}"), 4.0, vec![], &[("age_hours", 1.0)]);
            let _ = state.evaluate(&c, Instant::now());
        }
        assert!(state.audit_tail.len() <= SelfImprovementState::AUDIT_TAIL_CAP);
    }

    #[test]
    fn success_feedback_bumps_trust_budget() {
        let mut state = enabled_state();
        let before = state.trust_budget().score();
        state.record_success();
        assert_eq!(state.trust_budget().score(), before + 1);
    }
}
