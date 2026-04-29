//! Principled proactive intervention engine (Proactive Agent patterns).
//!
//! Replaces the ad-hoc `quiet_threshold`, `ACTION_COOLDOWN_SECS`,
//! `suggestions_since_input`, and `chattiness` parameters in [`UtilityScorer`]
//! with a structured decision framework from "Proactive Agent: Shifting LLM
//! Agents from Reactive Responses to Active Assistance" (Liao et al., 2024).
//!
//! # Key concepts from the paper
//!
//! - **P(t) = f_theta(E_t, A_t, S_t)**: predicted task at time t, given
//!   environmental events, user activities, and environment state.
//! - **R(t) = g(P_t, A_t, S_t)**: user acceptance judgment (accept/reject).
//!   R=1 when (P!=null AND user accepts) OR (P=null AND N=0, no need).
//! - **N(t)**: auxiliary variable indicating whether user *actually* needs help.
//! - **Reward model**: binary classifier trained on (event, prediction, judgment)
//!   triples. In Phantom, we approximate this with a rule-based scorer since we
//!   don't have annotated training data yet -- but the signal taxonomy is the same.
//!
//! # Architecture
//!
//! The [`InterventionEngine`] observes three signal streams (matching the paper):
//! 1. **Environmental events** (`E_t`): command output, file changes, git state
//! 2. **User activities** (`A_t`): keystrokes, commands, idle time
//! 3. **Environment state** (`S_t`): error state, active processes, project context
//!
//! It maintains a rolling window of signals and computes a principled
//! `should_act()` score that replaces the scattered ad-hoc checks.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::time::Instant;

use crate::curves::{LogisticCurve, UtilityCurve};
use crate::events::{AiAction, AiEvent};

// ---------------------------------------------------------------------------
// Signal taxonomy (paper's E_t, A_t, S_t decomposition)
// ---------------------------------------------------------------------------

/// An environmental event observed by the brain.
///
/// Maps to the paper's E_t: "ranging from receiving a new email to an
/// application closed." In Phantom, these are terminal-specific events.
#[derive(Debug, Clone)]
pub enum EnvSignal {
    /// A command completed with errors.
    CommandFailed {
        command: String,
        error_count: usize,
        is_novel_error: bool,
    },
    /// A command completed successfully.
    CommandSucceeded { command: String },
    /// A watched file changed.
    FileChanged { path: String },
    /// Git state changed.
    GitChanged,
    /// An agent completed work.
    AgentCompleted { success: bool, summary: String },
    /// A long-running process started or is still running.
    ProcessRunning,
}

/// A user activity signal.
///
/// Maps to the paper's A_t: "user's interactions with the environment and
/// the agent, like keyboard input or chatting."
#[derive(Debug, Clone)]
pub enum UserSignal {
    /// User ran a command.
    RanCommand { command: String },
    /// User typed something (keystroke activity).
    Keystroke,
    /// User explicitly asked the brain something.
    AskedQuestion { query: String },
    /// User dismissed a previous suggestion.
    DismissedSuggestion,
    /// User accepted a previous suggestion.
    AcceptedSuggestion,
    /// User has been idle for this many seconds.
    Idle { seconds: f32 },
}

/// Snapshot of the environment state.
///
/// Maps to the paper's S_t: "the state of the current environment, like
/// the file system state or opened web pages."
#[derive(Debug, Clone)]
pub struct EnvState {
    /// Whether the last command had errors.
    pub has_errors: bool,
    /// Whether a long-running process is active.
    pub has_active_process: bool,
    /// Current idle time in seconds.
    pub idle_seconds: f32,
    /// Whether the user is in a REPL session.
    pub in_repl: bool,
    /// Number of open agent panes.
    pub active_agents: usize,
}

impl Default for EnvState {
    fn default() -> Self {
        Self {
            has_errors: false,
            has_active_process: false,
            idle_seconds: 0.0,
            in_repl: false,
            active_agents: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Timestamped signal for the rolling window
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct TimestampedSignal {
    kind: SignalKind,
    at: Instant,
}

#[derive(Debug, Clone)]
enum SignalKind {
    Env(EnvSignal),
    User(UserSignal),
}

// ---------------------------------------------------------------------------
// InterventionDecision
// ---------------------------------------------------------------------------

/// The engine's output: whether to act, what kind, and why.
#[derive(Debug, Clone)]
pub struct InterventionDecision {
    /// Should the brain intervene?
    pub should_act: bool,
    /// Confidence in this decision (0.0 = uncertain, 1.0 = certain).
    pub confidence: f32,
    /// What kind of intervention is appropriate.
    pub kind: InterventionKind,
    /// Human-readable reasoning (for debug logging).
    pub reason: String,
}

/// What kind of proactive intervention to offer.
///
/// The paper's taxonomy: the system can predict a task (proactive) or
/// predict nothing (stay quiet). We add granularity for Phantom's UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterventionKind {
    /// Reactive: user explicitly asked, always respond.
    ReactiveResponse,
    /// Proactive: error detected, offer to fix.
    ErrorAssistance,
    /// Proactive: user appears stuck, offer help.
    StuckAssistance,
    /// Proactive: environment change worth noting.
    EnvironmentNotice,
    /// Proactive: agent completed, report results.
    CompletionReport,
    /// Stay quiet: no intervention needed (P_t = null).
    Silence,
}

// ---------------------------------------------------------------------------
// InterventionEngine
// ---------------------------------------------------------------------------

/// Principled proactive intervention engine.
///
/// Replaces ad-hoc `quiet_threshold` + `chattiness` + `suggestions_since_input`
/// + `ACTION_COOLDOWN_SECS` with the paper's structured framework:
///
/// 1. Observe signals (E_t, A_t, S_t)
/// 2. Compute need score N_hat (estimated probability user needs help)
/// 3. Compute annoyance risk (false positive penalty)
/// 4. Output: act iff need > annoyance_threshold, adjusted by user feedback
pub struct InterventionEngine {
    /// Rolling window of recent signals.
    signals: VecDeque<TimestampedSignal>,
    /// Maximum signals to retain.
    window_size: usize,
    /// Window duration: only consider signals from the last N seconds.
    window_duration_secs: f32,

    // -- User feedback model (approximates the paper's reward model) -------
    /// Acceptance rate: fraction of suggestions the user accepted.
    /// Tracks the paper's R_t = g(P_t, A_t, S_t) over time.
    pub acceptance_rate: f32,
    /// Total suggestions offered.
    pub total_offered: u32,
    /// Total suggestions accepted.
    pub total_accepted: u32,
    /// Total suggestions dismissed.
    pub total_dismissed: u32,
    /// Consecutive dismissals (recent streak).
    pub consecutive_dismissals: u32,

    // -- Per-session state --------------------------------------------------
    /// Actions taken since last user input.
    pub actions_since_input: u32,
    /// When the last action was emitted.
    pub last_action_at: Option<Instant>,
    /// When the user last provided input.
    pub last_user_input_at: Option<Instant>,
    /// Current environment state snapshot.
    pub state: EnvState,
}

impl InterventionEngine {
    /// Create a new engine with default parameters.
    pub fn new() -> Self {
        Self {
            signals: VecDeque::with_capacity(50),
            window_size: 50,
            window_duration_secs: 120.0, // 2-minute sliding window

            acceptance_rate: 0.5, // prior: neutral
            total_offered: 0,
            total_accepted: 0,
            total_dismissed: 0,
            consecutive_dismissals: 0,

            actions_since_input: 0,
            last_action_at: None,
            last_user_input_at: None,
            state: EnvState::default(),
        }
    }

    // -- Signal ingestion ---------------------------------------------------

    /// Record an environmental event.
    pub fn observe_env(&mut self, signal: EnvSignal) {
        // Update state from the signal.
        match &signal {
            EnvSignal::CommandFailed { error_count, .. } => {
                self.state.has_errors = *error_count > 0;
            }
            EnvSignal::CommandSucceeded { .. } => {
                self.state.has_errors = false;
            }
            EnvSignal::ProcessRunning => {
                self.state.has_active_process = true;
            }
            _ => {}
        }

        self.push_signal(SignalKind::Env(signal));
    }

    /// Record a user activity.
    pub fn observe_user(&mut self, signal: UserSignal) {
        match &signal {
            UserSignal::RanCommand { .. }
            | UserSignal::Keystroke
            | UserSignal::AskedQuestion { .. } => {
                self.last_user_input_at = Some(Instant::now());
                self.actions_since_input = 0;
                self.consecutive_dismissals = 0;
                self.state.idle_seconds = 0.0;
            }
            UserSignal::DismissedSuggestion => {
                self.total_dismissed += 1;
                self.consecutive_dismissals += 1;
                self.update_acceptance_rate(false);
            }
            UserSignal::AcceptedSuggestion => {
                self.total_accepted += 1;
                self.consecutive_dismissals = 0;
                self.update_acceptance_rate(true);
            }
            UserSignal::Idle { seconds } => {
                self.state.idle_seconds = *seconds;
            }
        }

        self.push_signal(SignalKind::User(signal));
    }

    /// Update the environment state snapshot.
    pub fn update_state(&mut self, state: EnvState) {
        self.state = state;
    }

    fn push_signal(&mut self, kind: SignalKind) {
        let now = Instant::now();

        // Evict expired signals.
        while self
            .signals
            .front()
            .is_some_and(|s| now.duration_since(s.at).as_secs_f32() > self.window_duration_secs)
        {
            self.signals.pop_front();
        }

        // Cap at window size.
        if self.signals.len() >= self.window_size {
            self.signals.pop_front();
        }

        self.signals.push_back(TimestampedSignal { kind, at: now });
    }

    // -- Acceptance rate (paper's reward model approximation) ---------------

    /// Update the rolling acceptance rate using exponential moving average.
    ///
    /// The paper trains a reward model (LLaMA 8B, 91.8% F1) to predict
    /// accept/reject. We approximate this online with EMA over actual user
    /// feedback, which converges to the same signal.
    fn update_acceptance_rate(&mut self, accepted: bool) {
        self.total_offered += 1;
        let observed = if accepted { 1.0 } else { 0.0 };
        // EMA with alpha=0.15 (weight recent feedback more).
        self.acceptance_rate = self.acceptance_rate * 0.85 + observed * 0.15;
    }

    // -- The core decision function ----------------------------------------

    /// **The principled `should_act()` replacement.**
    ///
    /// Implements the paper's decision framework:
    ///
    /// ```text
    /// P_t = f(E_t, A_t, S_t)    // predict whether user needs help
    /// R_t = g(P_t, A_t, S_t)    // predict whether user would accept
    ///
    /// Act iff:
    ///   need_score > 0  AND
    ///   need_score * acceptance_modifier > annoyance_threshold
    /// ```
    ///
    /// The annoyance threshold adapts based on:
    /// - User feedback history (acceptance_rate)
    /// - Consecutive dismissals (back-off)
    /// - Actions since last user input (saturation)
    /// - Time since last action (cooldown)
    pub fn should_act(&self) -> InterventionDecision {
        // Phase 1: Detect explicit user request (always reactive).
        if self.has_recent_question() {
            return InterventionDecision {
                should_act: true,
                confidence: 1.0,
                kind: InterventionKind::ReactiveResponse,
                reason: "user asked explicitly".into(),
            };
        }

        // Phase 2: Compute need score N_hat from signals.
        let need = self.compute_need_score();

        // Phase 3: If no need detected, stay quiet (P_t = null).
        if need.score <= 0.0 {
            return InterventionDecision {
                should_act: false,
                confidence: 0.9,
                kind: InterventionKind::Silence,
                reason: need.reason,
            };
        }

        // Phase 4: Compute annoyance threshold (false positive prevention).
        let threshold = self.compute_annoyance_threshold();

        // Phase 5: Apply acceptance modifier from user feedback history.
        // The paper's optimization objective: max E[R_t].
        // If the user has been dismissing, we need higher need to act.
        let acceptance_modifier = self.acceptance_rate.max(0.1); // floor at 0.1
        let effective_score = need.score * acceptance_modifier;

        // Phase 6: Decision.
        let should_act = effective_score > threshold;

        // Map the margin between effective_score and threshold through a
        // logistic curve to get a smooth, well-calibrated confidence value.
        // A positive margin means we're acting; negative means we're silent.
        // We normalize the margin into [0,1] and feed it through the sigmoid
        // so confidence saturates gracefully rather than jumping to extremes.
        let margin = effective_score - threshold; // [-1.0, 1.0]
        let normalized_margin = (margin + 1.0) / 2.0; // map to [0, 1]
        let confidence_curve = LogisticCurve::new(0.5, 12.0);
        let confidence = confidence_curve.score(normalized_margin);

        InterventionDecision {
            should_act,
            confidence,
            kind: need.kind,
            reason: format!(
                "{} (need={:.2} x accept={:.2} = {:.2} vs threshold={:.2})",
                need.reason, need.score, acceptance_modifier, effective_score, threshold
            ),
        }
    }

    // -- Need score computation (P_t prediction) ---------------------------

    /// Compute the estimated need score from recent signals.
    ///
    /// This is the paper's `P_t = f_theta(E_t, A_t, S_t)` -- predicting
    /// whether the user needs assistance right now. We decompose it into
    /// independent signal contributions that sum.
    fn compute_need_score(&self) -> NeedEstimate {
        let mut best = NeedEstimate {
            score: 0.0,
            kind: InterventionKind::Silence,
            reason: "no actionable signals".into(),
        };

        // Signal 1: Fresh errors (highest priority proactive trigger).
        if let Some(est) = self.need_from_errors() {
            if est.score > best.score {
                best = est;
            }
        }

        // Signal 2: User appears stuck (idle after error).
        if let Some(est) = self.need_from_stuck() {
            if est.score > best.score {
                best = est;
            }
        }

        // Signal 3: Agent completion (always worth reporting).
        if let Some(est) = self.need_from_agent_completion() {
            if est.score > best.score {
                best = est;
            }
        }

        // Signal 4: Environment changes worth noting.
        if let Some(est) = self.need_from_env_changes() {
            if est.score > best.score {
                best = est;
            }
        }

        // Gate: user is actively typing -- suppress all proactive signals.
        // From the paper: user activities A_t modulate the prediction.
        if self.state.idle_seconds < 2.0 && best.kind != InterventionKind::CompletionReport {
            best.score = 0.0;
            best.reason = "user is actively typing".into();
            best.kind = InterventionKind::Silence;
        }

        // Gate: user is in a REPL -- don't interrupt.
        if self.state.in_repl
            && !matches!(
                best.kind,
                InterventionKind::CompletionReport | InterventionKind::ReactiveResponse
            )
        {
            best.score *= 0.1;
            best.reason = format!("{} (dampened: REPL session)", best.reason);
        }

        best
    }

    fn need_from_errors(&self) -> Option<NeedEstimate> {
        let now = Instant::now();

        // Look for recent CommandFailed signals.
        let recent_failures: Vec<&TimestampedSignal> = self
            .signals
            .iter()
            .filter(|s| {
                now.duration_since(s.at).as_secs() < 30
                    && matches!(s.kind, SignalKind::Env(EnvSignal::CommandFailed { .. }))
            })
            .collect();

        if recent_failures.is_empty() {
            return None;
        }

        let latest = recent_failures.last().unwrap();
        let age_secs = now.duration_since(latest.at).as_secs_f32();

        // Freshness decay: highest value right after the error, decays over 30s.
        let freshness = 1.0 - (age_secs / 30.0).min(1.0);

        // Novelty boost: first-time errors get higher score.
        let is_novel = matches!(
            &latest.kind,
            SignalKind::Env(EnvSignal::CommandFailed {
                is_novel_error: true,
                ..
            })
        );
        let novelty_bonus = if is_novel { 0.15 } else { 0.0 };

        let score = 0.7 * freshness + novelty_bonus;

        if score > 0.0 {
            Some(NeedEstimate {
                score,
                kind: InterventionKind::ErrorAssistance,
                reason: format!(
                    "error detected {:.0}s ago (freshness={:.2})",
                    age_secs, freshness
                ),
            })
        } else {
            None
        }
    }

    fn need_from_stuck(&self) -> Option<NeedEstimate> {
        // User is stuck if: errors exist AND user has been idle for a while.
        if !self.state.has_errors {
            return None;
        }

        let idle = self.state.idle_seconds;

        // Ramp: starts at 0 when idle=5s, reaches 0.6 at idle=30s.
        if idle < 5.0 {
            return None;
        }

        let ramp = ((idle - 5.0) / 25.0).min(1.0);
        let score = 0.6 * ramp;

        Some(NeedEstimate {
            score,
            kind: InterventionKind::StuckAssistance,
            reason: format!("user idle {:.0}s after error (ramp={:.2})", idle, ramp),
        })
    }

    fn need_from_agent_completion(&self) -> Option<NeedEstimate> {
        let now = Instant::now();

        let recent_completions = self.signals.iter().any(|s| {
            now.duration_since(s.at).as_secs() < 10
                && matches!(s.kind, SignalKind::Env(EnvSignal::AgentCompleted { .. }))
        });

        if recent_completions {
            Some(NeedEstimate {
                score: 0.8, // high -- user should know their agent finished
                kind: InterventionKind::CompletionReport,
                reason: "agent completed recently".into(),
            })
        } else {
            None
        }
    }

    fn need_from_env_changes(&self) -> Option<NeedEstimate> {
        let now = Instant::now();

        let recent_changes = self
            .signals
            .iter()
            .filter(|s| {
                now.duration_since(s.at).as_secs() < 15
                    && matches!(
                        s.kind,
                        SignalKind::Env(EnvSignal::FileChanged { .. })
                            | SignalKind::Env(EnvSignal::GitChanged)
                    )
            })
            .count();

        if recent_changes > 0 {
            Some(NeedEstimate {
                score: 0.35,
                kind: InterventionKind::EnvironmentNotice,
                reason: format!("{} env changes in last 15s", recent_changes),
            })
        } else {
            None
        }
    }

    // -- Annoyance threshold (false positive prevention) --------------------

    /// Compute the dynamic annoyance threshold.
    ///
    /// The paper notes models "frequently offer unnecessary help" with "high
    /// false alarm ratio." This threshold adapts to prevent that:
    ///
    /// ```text
    /// base_threshold = 0.3
    ///   + dismissal_penalty     (consecutive dismissals raise the bar)
    ///   + saturation_penalty    (too many actions without input)
    ///   + cooldown_penalty      (too soon after last action)
    /// ```
    ///
    /// Clamped to [0.1, 0.95] so the brain can still act on truly urgent
    /// signals even when the user has been dismissing, and never becomes
    /// completely mute.
    fn compute_annoyance_threshold(&self) -> f32 {
        let base = 0.3;

        // Penalty 1: Consecutive dismissals.
        // Each dismissal raises the bar by 0.12 (fast back-off).
        // Paper: "models generally reduce their false alarm ratio...but drop
        // dramatically in terms of recall" -- we want moderate back-off.
        let dismissal_penalty = (self.consecutive_dismissals as f32) * 0.12;

        // Penalty 2: Saturation (too many actions without user input).
        // Paper's auxiliary variable N_t: if we've already offered help
        // multiple times without the user asking, need is lower.
        let saturation_penalty = match self.actions_since_input {
            0 => 0.0,
            1 => 0.08,
            2 => 0.18,
            _ => 0.30, // hard dampen after 3+ unrequested actions
        };

        // Penalty 3: Cooldown (minimum gap between actions).
        // Replaces the hard ACTION_COOLDOWN_SECS=15.0 with a soft penalty
        // that decays over time.
        let cooldown_penalty = if let Some(last) = self.last_action_at {
            let elapsed = last.elapsed().as_secs_f32();
            if elapsed < 5.0 {
                0.30 // very recent: strong penalty
            } else if elapsed < 15.0 {
                0.15 * (1.0 - (elapsed - 5.0) / 10.0) // decay from 0.15 to 0
            } else {
                0.0
            }
        } else {
            0.0
        };

        (base + dismissal_penalty + saturation_penalty + cooldown_penalty).clamp(0.1, 0.95)
    }

    // -- Action recording --------------------------------------------------

    /// Call after the brain emits a non-quiet action.
    pub fn record_action(&mut self) {
        self.actions_since_input += 1;
        self.last_action_at = Some(Instant::now());
        self.total_offered += 1;
    }

    /// Call when the user provides any input (resets per-session counters).
    pub fn user_acted(&mut self) {
        self.actions_since_input = 0;
        self.consecutive_dismissals = 0;
        self.state.idle_seconds = 0.0;
        self.last_user_input_at = Some(Instant::now());
    }

    // -- Query helpers ------------------------------------------------------

    /// Whether the user recently asked a direct question.
    fn has_recent_question(&self) -> bool {
        let now = Instant::now();
        self.signals.iter().any(|s| {
            now.duration_since(s.at).as_secs() < 5
                && matches!(s.kind, SignalKind::User(UserSignal::AskedQuestion { .. }))
        })
    }

    /// Get a diagnostic summary of the engine's state (for logging).
    pub fn diagnostic(&self) -> String {
        format!(
            "InterventionEngine {{ accept_rate={:.2}, offered={}, accepted={}, \
             dismissed={}, consec_dismiss={}, actions_since_input={}, \
             idle={:.1}s, signals={} }}",
            self.acceptance_rate,
            self.total_offered,
            self.total_accepted,
            self.total_dismissed,
            self.consecutive_dismissals,
            self.actions_since_input,
            self.state.idle_seconds,
            self.signals.len(),
        )
    }
}

impl Default for InterventionEngine {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Internal types
// ---------------------------------------------------------------------------

struct NeedEstimate {
    score: f32,
    kind: InterventionKind,
    reason: String,
}

// ===========================================================================
// ProactiveSuggester
// ===========================================================================
//
// Structured trigger-based suggester that maps AiEvent inputs to
// AiAction::Suggest outputs. Each trigger kind maintains its own per-kind
// cooldown so that no single trigger kind can spam the user.
//
// The four canonical triggers from issue #37:
//   - TestFailed      — emitted when `cargo test` / test runner exits non-zero.
//   - BuildError      — emitted when a build command exits non-zero.
//   - IdleAfterQuestion — emitted when the user is idle after asking a question.
//   - ContextChange   — emitted when git branch or cwd changes.

// ---------------------------------------------------------------------------
// TriggerKind
// ---------------------------------------------------------------------------

/// The category of pattern that the suggester observed.
///
/// Each variant is tracked separately for cooldown purposes so that, for
/// example, a `BuildError` cannot be suppressed by an unrelated
/// `ContextChange` that fired moments before.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TriggerKind {
    /// A test run exited with a non-zero status code.
    TestFailed,
    /// A build command exited with a non-zero status code.
    BuildError,
    /// The user asked a question and is now idle (may need follow-up).
    IdleAfterQuestion,
    /// The git branch or working directory changed.
    ContextChange,
}

// ---------------------------------------------------------------------------
// Trigger
// ---------------------------------------------------------------------------

/// A single trigger rule: maps a pattern to a suggested action + rationale.
///
/// When the pattern fires and the per-kind cooldown has elapsed, the
/// [`ProactiveSuggester`] emits an [`AiAction::Suggest`] with the trigger's
/// canned `action` and `rationale` strings and a `confidence` value.
pub struct Trigger {
    /// Which trigger category this rule belongs to (used for cooldown tracking).
    pub(crate) kind: TriggerKind,
    /// Short description of the next action to propose to the user.
    pub(crate) action: String,
    /// One-sentence explanation of why this action is being suggested.
    pub(crate) rationale: String,
    /// Confidence score in `[0.0, 1.0]`.
    pub(crate) confidence: f32,
}

impl Trigger {
    /// Create a new trigger rule.
    pub fn new(
        kind: TriggerKind,
        action: impl Into<String>,
        rationale: impl Into<String>,
        confidence: f32,
    ) -> Self {
        Self {
            kind,
            action: action.into(),
            rationale: rationale.into(),
            confidence: confidence.clamp(0.0, 1.0),
        }
    }
}

// ---------------------------------------------------------------------------
// ProactiveSuggester
// ---------------------------------------------------------------------------

/// Trigger-based proactive suggestion engine (issue #37).
///
/// Observes [`AiEvent`]s and emits an [`AiAction::Suggest`] when a registered
/// [`Trigger`] fires and the per-kind cooldown has elapsed.
///
/// # Cooldown semantics
///
/// Each [`TriggerKind`] has its own independent cooldown clock. Firing one
/// trigger does **not** reset the cooldown for other kinds.  The global
/// `cooldown_ms` is the default; individual triggers share the same value
/// (per-kind, not per-rule — a single kind may have at most one registered
/// rule in the default configuration).
pub struct ProactiveSuggester {
    /// Registered trigger rules.
    triggers: Vec<Trigger>,
    /// Cooldown in milliseconds per trigger kind (shared default).
    cooldown_ms: u64,
    /// When each trigger kind last fired (for cooldown enforcement).
    last_fired: HashMap<TriggerKind, Instant>,
    /// Whether a question was recently asked (for IdleAfterQuestion tracking).
    pending_question: bool,
}

impl ProactiveSuggester {
    /// Create a new suggester with the given triggers and per-kind cooldown.
    ///
    /// `cooldown_ms` is the minimum number of milliseconds that must elapse
    /// between two suggestions of the same [`TriggerKind`].  The default
    /// recommended value is `60_000` (60 seconds).
    pub fn new(triggers: Vec<Trigger>, cooldown_ms: u64) -> Self {
        Self {
            triggers,
            cooldown_ms,
            last_fired: HashMap::new(),
            pending_question: false,
        }
    }

    /// Build a [`ProactiveSuggester`] with the canonical set of triggers for
    /// Phantom (issue #37 defaults).
    pub fn default_triggers() -> Self {
        Self::new(
            vec![
                Trigger::new(
                    TriggerKind::TestFailed,
                    "Re-run failing tests with --nocapture for more output",
                    "Tests just failed — capturing output may reveal the root cause",
                    0.80,
                ),
                Trigger::new(
                    TriggerKind::BuildError,
                    "Run cargo fix or inspect the error with cargo check",
                    "Build failed — a quick diagnosis could unblock you",
                    0.85,
                ),
                Trigger::new(
                    TriggerKind::IdleAfterQuestion,
                    "Ask the brain for help with this error",
                    "You asked a question and seem to still be thinking — need more info?",
                    0.65,
                ),
                Trigger::new(
                    TriggerKind::ContextChange,
                    "Review recent changes on this branch",
                    "Context changed — git log or git diff may be useful",
                    0.55,
                ),
            ],
            60_000,
        )
    }

    /// Observe an [`AiEvent`] and optionally emit an [`AiAction::Suggest`].
    ///
    /// Returns `Some(AiAction::Suggest { .. })` if a trigger fires and its
    /// cooldown has elapsed, otherwise `None`.
    pub fn observe(&mut self, event: &AiEvent) -> Option<AiAction> {
        let kind = self.classify(event)?;
        self.maybe_emit(kind)
    }

    /// Classify an event into a [`TriggerKind`], or return `None` if the
    /// event does not match any registered trigger.
    fn classify(&mut self, event: &AiEvent) -> Option<TriggerKind> {
        match event {
            AiEvent::CommandComplete(parsed) => {
                let has_errors = !parsed.errors.is_empty();
                let exit_failed = parsed.exit_code.map_or(false, |c| c != 0);

                if !has_errors && !exit_failed {
                    return None;
                }

                // Distinguish test failures from generic build errors by
                // inspecting the command text.  This mirrors the issue spec's
                // TestFailed / BuildError split.
                let cmd = parsed.command.trim();
                let is_test = is_test_command(cmd);
                let is_build = !is_test && is_build_command(cmd);

                if is_test {
                    Some(TriggerKind::TestFailed)
                } else if is_build {
                    Some(TriggerKind::BuildError)
                } else if has_errors {
                    // Generic command that produced structured errors →
                    // treat as a build-class failure.
                    Some(TriggerKind::BuildError)
                } else {
                    None
                }
            }

            // IdleAfterQuestion: user asked something and then went idle.
            AiEvent::Interrupt(query) if !query.is_empty() => {
                self.pending_question = true;
                None // Don't emit immediately — wait for an idle event.
            }
            AiEvent::UserIdle { seconds } if self.pending_question && *seconds >= 10.0 => {
                Some(TriggerKind::IdleAfterQuestion)
            }

            // ContextChange: branch switch or cwd change.
            AiEvent::GitStateChanged => Some(TriggerKind::ContextChange),

            _ => None,
        }
    }

    /// Emit a `Suggest` action for `kind` if the cooldown has elapsed.
    ///
    /// Updates the per-kind last-fired timestamp on success.
    fn maybe_emit(&mut self, kind: TriggerKind) -> Option<AiAction> {
        // Cooldown check.
        if let Some(last) = self.last_fired.get(&kind) {
            let elapsed_ms = last.elapsed().as_millis() as u64;
            if elapsed_ms < self.cooldown_ms {
                return None;
            }
        }

        // Find the first trigger rule that matches this kind.
        let trigger = self.triggers.iter().find(|t| t.kind == kind)?;

        let action_str = trigger.action.clone();
        let rationale = trigger.rationale.clone();
        let confidence = trigger.confidence;

        // Record the fire time before returning.
        self.last_fired.insert(kind, Instant::now());

        // Reset pending-question state once we've emitted for it.
        if kind == TriggerKind::IdleAfterQuestion {
            self.pending_question = false;
        }

        Some(AiAction::Suggest {
            action: action_str,
            rationale,
            confidence,
        })
    }

    /// Return the elapsed time since `kind` last fired, or `None` if it
    /// has never fired.  Useful for testing and diagnostics.
    pub fn elapsed_since_fired(&self, kind: TriggerKind) -> Option<std::time::Duration> {
        self.last_fired.get(&kind).map(|t| t.elapsed())
    }
}

// ---------------------------------------------------------------------------
// Command-classification helpers (private)
// ---------------------------------------------------------------------------

/// Return `true` if the command string looks like a test runner invocation.
fn is_test_command(cmd: &str) -> bool {
    let first = cmd.split_whitespace().next().unwrap_or("");
    // Primary: first token is a well-known test runner.
    let known_runners = [
        "cargo", "pytest", "jest", "mocha", "go", "rspec", "vitest", "npm", "yarn",
    ];
    if !known_runners.iter().any(|r| first.ends_with(r)) {
        return false;
    }
    // Secondary: subcommand or flags mention "test".
    cmd.contains(" test") || cmd.contains(" spec") || cmd.contains("--test")
}

/// Return `true` if the command string looks like a build invocation.
fn is_build_command(cmd: &str) -> bool {
    let first = cmd.split_whitespace().next().unwrap_or("");
    let known_builders = [
        "cargo", "make", "cmake", "gradle", "mvn", "bazel", "buck", "npm", "yarn",
    ];
    if !known_builders.iter().any(|r| first.ends_with(r)) {
        return false;
    }
    cmd.contains(" build")
        || cmd.contains(" compile")
        || cmd.contains(" check")
        || cmd.contains(" install")
        || cmd.contains("--build")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn engine_with_expired_cooldown() -> InterventionEngine {
        let mut e = InterventionEngine::new();
        // Set last_action_at far enough in the past that cooldown is expired.
        e.last_action_at = Some(Instant::now() - std::time::Duration::from_secs(60));
        e
    }

    // -- Basic should_act behavior -----------------------------------------

    #[test]
    fn silence_when_no_signals() {
        let engine = InterventionEngine::new();
        let decision = engine.should_act();
        assert!(!decision.should_act);
        assert_eq!(decision.kind, InterventionKind::Silence);
    }

    #[test]
    fn reactive_on_explicit_question() {
        let mut engine = InterventionEngine::new();
        engine.observe_user(UserSignal::AskedQuestion {
            query: "what is this error?".into(),
        });
        let decision = engine.should_act();
        assert!(decision.should_act);
        assert_eq!(decision.kind, InterventionKind::ReactiveResponse);
        assert_eq!(decision.confidence, 1.0);
    }

    // -- Error assistance --------------------------------------------------

    #[test]
    fn proactive_on_fresh_error() {
        let mut engine = engine_with_expired_cooldown();
        engine.state.idle_seconds = 5.0; // not typing

        engine.observe_env(EnvSignal::CommandFailed {
            command: "cargo build".into(),
            error_count: 3,
            is_novel_error: true,
        });

        let decision = engine.should_act();
        assert!(decision.should_act, "reason: {}", decision.reason);
        assert_eq!(decision.kind, InterventionKind::ErrorAssistance);
    }

    #[test]
    fn no_act_when_user_typing() {
        let mut engine = engine_with_expired_cooldown();
        engine.state.idle_seconds = 0.5; // actively typing

        engine.observe_env(EnvSignal::CommandFailed {
            command: "cargo build".into(),
            error_count: 3,
            is_novel_error: true,
        });

        let decision = engine.should_act();
        assert!(!decision.should_act, "should not interrupt typing");
    }

    // -- Stuck detection ---------------------------------------------------

    #[test]
    fn stuck_detection_after_idle() {
        let mut engine = engine_with_expired_cooldown();
        engine.state.has_errors = true;
        engine.state.idle_seconds = 25.0;

        // Need a recent error signal in the window for the error scorer.
        engine.observe_env(EnvSignal::CommandFailed {
            command: "cargo build".into(),
            error_count: 1,
            is_novel_error: false,
        });
        // Simulate time passing by adjusting idle.
        engine.state.idle_seconds = 25.0;

        let decision = engine.should_act();
        assert!(
            decision.should_act,
            "should detect stuck user: {}",
            decision.reason
        );
    }

    // -- Agent completion --------------------------------------------------

    #[test]
    fn reports_agent_completion() {
        let mut engine = engine_with_expired_cooldown();
        engine.state.idle_seconds = 3.0;

        engine.observe_env(EnvSignal::AgentCompleted {
            success: true,
            summary: "fixed the bug".into(),
        });

        let decision = engine.should_act();
        assert!(decision.should_act, "reason: {}", decision.reason);
        assert_eq!(decision.kind, InterventionKind::CompletionReport);
    }

    // -- Annoyance threshold adaptation ------------------------------------

    #[test]
    fn threshold_increases_with_dismissals() {
        let mut engine = InterventionEngine::new();
        let base = engine.compute_annoyance_threshold();

        // Simulate 3 dismissals.
        engine.consecutive_dismissals = 3;
        let after_dismissals = engine.compute_annoyance_threshold();

        assert!(
            after_dismissals > base,
            "threshold should increase: {} > {}",
            after_dismissals,
            base
        );
    }

    #[test]
    fn threshold_increases_with_saturation() {
        let mut engine = InterventionEngine::new();
        let base = engine.compute_annoyance_threshold();

        engine.actions_since_input = 3;
        let after_saturation = engine.compute_annoyance_threshold();

        assert!(
            after_saturation > base,
            "threshold should increase: {} > {}",
            after_saturation,
            base
        );
    }

    #[test]
    fn threshold_clamped_to_range() {
        let mut engine = InterventionEngine::new();
        engine.consecutive_dismissals = 100; // extreme
        engine.actions_since_input = 100;

        let threshold = engine.compute_annoyance_threshold();
        assert!(
            threshold <= 0.95,
            "threshold capped at 0.95, got {}",
            threshold
        );
        assert!(
            threshold >= 0.1,
            "threshold floored at 0.1, got {}",
            threshold
        );
    }

    // -- Acceptance rate tracking ------------------------------------------

    #[test]
    fn acceptance_rate_tracks_feedback() {
        let mut engine = InterventionEngine::new();
        assert!((engine.acceptance_rate - 0.5).abs() < f32::EPSILON);

        // 5 accepts.
        for _ in 0..5 {
            engine.observe_user(UserSignal::AcceptedSuggestion);
        }
        assert!(
            engine.acceptance_rate > 0.5,
            "rate should increase: {}",
            engine.acceptance_rate
        );

        // 10 dismissals.
        for _ in 0..10 {
            engine.observe_user(UserSignal::DismissedSuggestion);
        }
        assert!(
            engine.acceptance_rate < 0.5,
            "rate should decrease: {}",
            engine.acceptance_rate
        );
    }

    // -- User input resets -------------------------------------------------

    #[test]
    fn user_acted_resets_counters() {
        let mut engine = InterventionEngine::new();
        engine.actions_since_input = 5;
        engine.consecutive_dismissals = 3;
        engine.state.idle_seconds = 30.0;

        engine.user_acted();

        assert_eq!(engine.actions_since_input, 0);
        assert_eq!(engine.consecutive_dismissals, 0);
        assert_eq!(engine.state.idle_seconds, 0.0);
    }

    // -- REPL dampening ----------------------------------------------------

    #[test]
    fn repl_dampens_proactive_signals() {
        let mut engine = engine_with_expired_cooldown();
        engine.state.in_repl = true;
        engine.state.idle_seconds = 5.0;

        engine.observe_env(EnvSignal::CommandFailed {
            command: "python3".into(),
            error_count: 1,
            is_novel_error: true,
        });

        let decision = engine.should_act();
        // REPL dampening should reduce the score significantly.
        assert!(
            !decision.should_act || decision.confidence < 0.3,
            "REPL should dampen: {:?}",
            decision
        );
    }

    // -- Cooldown penalty --------------------------------------------------

    #[test]
    fn cooldown_penalty_prevents_rapid_fire() {
        let mut engine = InterventionEngine::new();
        engine.last_action_at = Some(Instant::now()); // just acted
        engine.state.idle_seconds = 5.0;

        engine.observe_env(EnvSignal::CommandFailed {
            command: "cargo build".into(),
            error_count: 2,
            is_novel_error: true,
        });

        let decision = engine.should_act();
        // The cooldown penalty should make the threshold too high.
        assert!(
            !decision.should_act,
            "cooldown should prevent rapid fire: {}",
            decision.reason
        );
    }

    // -- Diagnostic string -------------------------------------------------

    #[test]
    fn diagnostic_contains_key_fields() {
        let engine = InterventionEngine::new();
        let diag = engine.diagnostic();
        assert!(diag.contains("accept_rate"));
        assert!(diag.contains("offered"));
        assert!(diag.contains("signals"));
    }

    // -- Edge: high acceptance rate lowers barrier -------------------------

    #[test]
    fn high_acceptance_lowers_effective_threshold() {
        let mut engine_high = engine_with_expired_cooldown();
        engine_high.acceptance_rate = 0.9; // user loves suggestions
        engine_high.state.idle_seconds = 10.0;

        let mut engine_low = engine_with_expired_cooldown();
        engine_low.acceptance_rate = 0.15; // user hates suggestions
        engine_low.state.idle_seconds = 10.0;

        // Give both the same moderate signal.
        let signal = EnvSignal::CommandFailed {
            command: "cargo test".into(),
            error_count: 1,
            is_novel_error: false,
        };
        engine_high.observe_env(signal.clone());
        engine_low.observe_env(signal);

        let dec_high = engine_high.should_act();
        let dec_low = engine_low.should_act();

        // The high-acceptance engine should be more willing to act.
        // We check that at least the confidence/willingness differs.
        assert!(
            dec_high.confidence >= dec_low.confidence
                || (dec_high.should_act && !dec_low.should_act),
            "high acceptance should lower effective threshold: \
             high={:?}, low={:?}",
            dec_high,
            dec_low
        );
    }

    // =======================================================================
    // ProactiveSuggester tests (issue #37)
    // =======================================================================

    use super::{ProactiveSuggester, Trigger, TriggerKind};
    use phantom_semantic::{
        CargoCommand, CommandType, ContentType, DetectedError, ErrorType, ParsedOutput, Severity,
    };

    /// Build a minimal ParsedOutput with one error and a non-zero exit code.
    fn failed_output(command: &str) -> ParsedOutput {
        ParsedOutput {
            command: command.into(),
            command_type: CommandType::Cargo(CargoCommand::Build),
            exit_code: Some(1),
            content_type: ContentType::CompilerOutput,
            errors: vec![DetectedError {
                message: "mismatched types".into(),
                error_type: ErrorType::Compiler,
                file: Some("src/main.rs".into()),
                line: Some(10),
                column: Some(1),
                code: Some("E0308".into()),
                severity: Severity::Error,
                raw_line: "error[E0308]: mismatched types".into(),
                suggestion: None,
            }],
            warnings: vec![],
            duration_ms: Some(800),
            raw_output: "error[E0308]: mismatched types".into(),
        }
    }

    /// Build a minimal ParsedOutput with no errors and exit code 0.
    fn success_output(command: &str) -> ParsedOutput {
        ParsedOutput {
            command: command.into(),
            command_type: CommandType::Cargo(CargoCommand::Build),
            exit_code: Some(0),
            content_type: ContentType::PlainText,
            errors: vec![],
            warnings: vec![],
            duration_ms: Some(400),
            raw_output: "Finished".into(),
        }
    }

    /// Create a fresh suggester using `default_triggers()`.
    fn default_suggester() -> ProactiveSuggester {
        ProactiveSuggester::default_triggers()
    }

    // -----------------------------------------------------------------------
    // TriggerKind::BuildError
    // -----------------------------------------------------------------------

    #[test]
    fn build_error_fires_on_cargo_build_failure() {
        let mut s = default_suggester();
        let event = AiEvent::CommandComplete(failed_output("cargo build"));
        let result = s.observe(&event);
        assert!(result.is_some(), "expected Suggest for cargo build failure");
        if let Some(AiAction::Suggest { confidence, .. }) = result {
            assert!(confidence > 0.0);
        } else {
            panic!("expected AiAction::Suggest");
        }
    }

    #[test]
    fn build_error_does_not_fire_on_success() {
        let mut s = default_suggester();
        let event = AiEvent::CommandComplete(success_output("cargo build"));
        let result = s.observe(&event);
        assert!(result.is_none(), "should not suggest when build succeeds");
    }

    #[test]
    fn build_error_suggest_contains_action_and_rationale() {
        let mut s = default_suggester();
        let event = AiEvent::CommandComplete(failed_output("cargo build"));
        let result = s.observe(&event);
        if let Some(AiAction::Suggest {
            action,
            rationale,
            confidence,
        }) = result
        {
            assert!(!action.is_empty(), "action must not be empty");
            assert!(!rationale.is_empty(), "rationale must not be empty");
            assert!(
                (0.0..=1.0).contains(&confidence),
                "confidence must be in [0,1]"
            );
        } else {
            panic!("expected AiAction::Suggest");
        }
    }

    // -----------------------------------------------------------------------
    // TriggerKind::TestFailed
    // -----------------------------------------------------------------------

    #[test]
    fn test_failed_fires_on_cargo_test_failure() {
        let mut s = default_suggester();
        let event = AiEvent::CommandComplete(failed_output("cargo test"));
        let result = s.observe(&event);
        assert!(result.is_some(), "expected Suggest for cargo test failure");
    }

    #[test]
    fn test_failed_does_not_fire_on_success() {
        let mut s = default_suggester();
        let event = AiEvent::CommandComplete(success_output("cargo test"));
        let result = s.observe(&event);
        assert!(result.is_none(), "should not suggest when tests pass");
    }

    #[test]
    fn test_failed_is_distinct_from_build_error() {
        // Each TriggerKind has its own cooldown; firing TestFailed should not
        // consume BuildError's cooldown.
        let mut s = default_suggester();

        // Fire TestFailed.
        let test_event = AiEvent::CommandComplete(failed_output("cargo test"));
        let r1 = s.observe(&test_event);
        assert!(r1.is_some(), "TestFailed should fire first time");

        // BuildError is a separate kind — its cooldown is untouched.
        let build_event = AiEvent::CommandComplete(failed_output("cargo build"));
        let r2 = s.observe(&build_event);
        assert!(
            r2.is_some(),
            "BuildError should fire independently of TestFailed cooldown"
        );
    }

    // -----------------------------------------------------------------------
    // TriggerKind::IdleAfterQuestion
    // -----------------------------------------------------------------------

    #[test]
    fn idle_after_question_fires_when_question_then_idle() {
        let mut s = default_suggester();

        // Step 1: user asks a question.
        let q = AiEvent::Interrupt("how do I fix this borrow error?".into());
        let r1 = s.observe(&q);
        assert!(r1.is_none(), "question alone should not emit immediately");

        // Step 2: user goes idle for > 10 s.
        let idle = AiEvent::UserIdle { seconds: 15.0 };
        let r2 = s.observe(&idle);
        assert!(r2.is_some(), "should suggest after question + idle");
    }

    #[test]
    fn idle_after_question_does_not_fire_without_prior_question() {
        let mut s = default_suggester();

        // Idle without a prior question.
        let idle = AiEvent::UserIdle { seconds: 30.0 };
        let result = s.observe(&idle);
        assert!(
            result.is_none(),
            "idle without a question should not trigger"
        );
    }

    #[test]
    fn idle_after_question_does_not_fire_if_idle_too_short() {
        let mut s = default_suggester();

        let q = AiEvent::Interrupt("how do I fix this?".into());
        s.observe(&q);

        // Idle for only 5 s — below the 10 s threshold.
        let idle = AiEvent::UserIdle { seconds: 5.0 };
        let result = s.observe(&idle);
        assert!(
            result.is_none(),
            "idle < 10 s should not trigger IdleAfterQuestion"
        );
    }

    // -----------------------------------------------------------------------
    // TriggerKind::ContextChange
    // -----------------------------------------------------------------------

    #[test]
    fn context_change_fires_on_git_state_changed() {
        let mut s = default_suggester();
        let event = AiEvent::GitStateChanged;
        let result = s.observe(&event);
        assert!(
            result.is_some(),
            "ContextChange should fire on GitStateChanged"
        );
    }

    #[test]
    fn context_change_does_not_fire_on_file_changed() {
        // FileChanged is not a ContextChange in the current classifier, so
        // this test verifies it returns None (keeps trigger taxonomy clean).
        let mut s = default_suggester();
        let event = AiEvent::FileChanged("src/main.rs".into());
        let result = s.observe(&event);
        // FileChanged is not mapped to ContextChange in the issue spec.
        // This is intentional — GitStateChanged is the ContextChange signal.
        assert!(
            result.is_none(),
            "FileChanged alone should not trigger ContextChange"
        );
    }

    // -----------------------------------------------------------------------
    // Cooldown enforcement
    // -----------------------------------------------------------------------

    #[test]
    fn cooldown_suppresses_rapid_refires_of_same_kind() {
        let mut s = default_suggester();

        let event = AiEvent::CommandComplete(failed_output("cargo build"));

        // First fire: should succeed.
        let r1 = s.observe(&event);
        assert!(r1.is_some(), "first BuildError should fire");

        // Immediate second fire: cooldown should suppress.
        let r2 = s.observe(&event);
        assert!(
            r2.is_none(),
            "second immediate BuildError should be suppressed by cooldown"
        );
    }

    #[test]
    fn cooldown_expires_after_configured_ms() {
        // Use a very short cooldown so the test does not have to sleep 60 s.
        let mut s = ProactiveSuggester::new(
            vec![Trigger::new(
                TriggerKind::BuildError,
                "run cargo fix",
                "build failed",
                0.8,
            )],
            1, // 1 ms cooldown
        );

        let event = AiEvent::CommandComplete(failed_output("cargo build"));

        // Fire once.
        let r1 = s.observe(&event);
        assert!(r1.is_some(), "first fire should succeed");

        // Spin long enough for 1 ms to elapse.
        std::thread::sleep(std::time::Duration::from_millis(5));

        // Fire again — cooldown should have expired.
        let r2 = s.observe(&event);
        assert!(r2.is_some(), "should fire again after cooldown expires");
    }

    #[test]
    fn different_kinds_have_independent_cooldowns() {
        // A short cooldown for both kinds.
        let mut s = ProactiveSuggester::new(
            vec![
                Trigger::new(TriggerKind::BuildError, "fix build", "build failed", 0.8),
                Trigger::new(
                    TriggerKind::ContextChange,
                    "review changes",
                    "context changed",
                    0.6,
                ),
            ],
            60_000, // 60 s
        );

        // Fire BuildError.
        let build_event = AiEvent::CommandComplete(failed_output("cargo build"));
        let r1 = s.observe(&build_event);
        assert!(r1.is_some(), "BuildError first fire");

        // ContextChange has an independent cooldown — should still fire.
        let ctx_event = AiEvent::GitStateChanged;
        let r2 = s.observe(&ctx_event);
        assert!(
            r2.is_some(),
            "ContextChange should fire independently of BuildError cooldown"
        );

        // BuildError again immediately — should be suppressed.
        let r3 = s.observe(&build_event);
        assert!(r3.is_none(), "BuildError should be on cooldown");
    }

    // -----------------------------------------------------------------------
    // Confidence bounds
    // -----------------------------------------------------------------------

    #[test]
    fn confidence_is_clamped_to_unit_range() {
        // Clamp > 1.0.
        let t = Trigger::new(TriggerKind::BuildError, "a", "b", 2.5);
        assert!(
            (t.confidence - 1.0).abs() < f32::EPSILON,
            "confidence should clamp to 1.0"
        );

        // Clamp < 0.0.
        let t = Trigger::new(TriggerKind::TestFailed, "a", "b", -0.5);
        assert!(
            t.confidence.abs() < f32::EPSILON,
            "confidence should clamp to 0.0"
        );
    }

    // -----------------------------------------------------------------------
    // elapsed_since_fired helper
    // -----------------------------------------------------------------------

    #[test]
    fn elapsed_since_fired_returns_none_before_first_fire() {
        let s = default_suggester();
        assert!(
            s.elapsed_since_fired(TriggerKind::BuildError).is_none(),
            "elapsed_since_fired should be None before the trigger fires"
        );
    }

    #[test]
    fn elapsed_since_fired_returns_some_after_fire() {
        let mut s = default_suggester();
        s.observe(&AiEvent::CommandComplete(failed_output("cargo build")));
        assert!(
            s.elapsed_since_fired(TriggerKind::BuildError).is_some(),
            "elapsed_since_fired should return Some after the trigger fires"
        );
    }
}
