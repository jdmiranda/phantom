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

use std::collections::VecDeque;
use std::time::Instant;

use crate::curves::{LogisticCurve, UtilityCurve};

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
    AgentCompleted {
        success: bool,
        summary: String,
    },
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
        while self.signals.front().is_some_and(|s| {
            now.duration_since(s.at).as_secs_f32() > self.window_duration_secs
        }) {
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
            SignalKind::Env(EnvSignal::CommandFailed { is_novel_error: true, .. })
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

        let recent_changes = self.signals.iter().filter(|s| {
            now.duration_since(s.at).as_secs() < 15
                && matches!(
                    s.kind,
                    SignalKind::Env(EnvSignal::FileChanged { .. })
                        | SignalKind::Env(EnvSignal::GitChanged)
                )
        }).count();

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

        (base + dismissal_penalty + saturation_penalty + cooldown_penalty)
            .clamp(0.1, 0.95)
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn engine_with_expired_cooldown() -> InterventionEngine {
        let mut e = InterventionEngine::new();
        // Set last_action_at far enough in the past that cooldown is expired.
        e.last_action_at =
            Some(Instant::now() - std::time::Duration::from_secs(60));
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
        assert!(threshold <= 0.95, "threshold capped at 0.95, got {}", threshold);
        assert!(threshold >= 0.1, "threshold floored at 0.1, got {}", threshold);
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
}
