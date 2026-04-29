//! Attention head — pane-of-interest selector.
//!
//! When the brain has compute budget for one observation, which pane should it
//! watch? This module answers that question. [`Attention::rank`] scores every
//! live pane by four salience signals and returns them sorted highest-first so
//! the caller can observe the most interesting pane first.
//!
//! # Signals (additive, each contributes a weight in \[0, 1\])
//!
//! | Signal | Max weight | Rationale |
//! |---|---|---|
//! | Error tokens | 0.4 | Compiler errors, panics, stderr — highest salience |
//! | User focus | 0.3 | The pane the user most recently interacted with |
//! | Recent activity | 0.2 | Events/sec in the last observation window |
//! | Agent in distress | 0.1 | An agent running in this pane emitted a warning |
//!
//! # Design constraints
//!
//! - **Deterministic**: the same `PaneSnapshot` inputs always produce the same
//!   ranking. No random tie-breaking.
//! - **Cheap**: no heap allocation per pane, no LLM call, O(n log n) sort.
//! - **Recency decay**: the activity signal decays exponentially with time so
//!   a burst of activity 60 seconds ago doesn't permanently dominate a quiet
//!   pane that just saw an error.
//!
//! # Integration
//!
//! [`Attention`] is constructed once and held by the brain loop. On every
//! proactive tick the brain calls [`Attention::rank`] to choose which pane to
//! observe. The resulting `(AdapterId, score)` pairs are used to pick the
//! observation target; the rest of the panes are skipped this tick.
//!
//! # Issue
//!
//! Implements GitHub issue #46.

use phantom_adapter::protocol::AdapterId;

// ---------------------------------------------------------------------------
// PaneSnapshot — caller-supplied view of a live pane
// ---------------------------------------------------------------------------

/// A lightweight, caller-supplied snapshot of a pane's current state.
///
/// The caller (typically the brain loop) assembles this from data it already
/// holds — terminal output, agent state, focus tracker — and passes it to
/// [`Attention::rank`] without any filesystem or network access.
///
/// All fields are optional; missing signals default to the neutral baseline
/// (no salience contribution).
#[derive(Debug, Clone)]
pub struct PaneSnapshot {
    /// The stable identifier of this pane's adapter.
    pub id: AdapterId,

    /// Number of error tokens detected in the pane's recent output.
    ///
    /// "Error tokens" are keywords like `error`, `panic`, `FAILED`, `fatal`,
    /// `stderr` — incremented by the caller for each match. Higher counts
    /// produce higher salience.
    pub error_token_count: u32,

    /// Whether this pane holds the user's current keyboard focus.
    pub is_user_focused: bool,

    /// Observed events per second in the pane's recent activity window.
    ///
    /// Typically: output lines / elapsed seconds since last observation.
    /// The brain clips this to `[0.0, ∞)` before passing it in; the
    /// attention head normalises internally.
    pub events_per_sec: f32,

    /// Whether an agent running inside this pane emitted a distress signal
    /// (e.g., the agent's confidence dropped, it hit a tool error, or it
    /// reported that it is stuck).
    pub agent_in_distress: bool,

    /// Seconds since this pane last had any activity.
    ///
    /// Used for recency decay. `0.0` means activity is happening right now.
    /// `None` means the pane has never had any activity (treated as very stale).
    pub secs_since_last_activity: Option<f32>,
}

// ---------------------------------------------------------------------------
// Attention
// ---------------------------------------------------------------------------

/// Attention head — ranks panes by salience so the brain observes the most
/// interesting pane first.
///
/// The struct carries no mutable state; it is a bag of tuning constants. Hold
/// one in the brain loop and call [`Attention::rank`] on every proactive tick.
pub struct Attention {
    /// Maximum salience contribution from error tokens.
    error_weight: f32,
    /// Maximum salience contribution from user focus.
    focus_weight: f32,
    /// Maximum salience contribution from recent activity.
    activity_weight: f32,
    /// Maximum salience contribution from agent-in-distress flag.
    distress_weight: f32,
    /// Activity events/sec that saturates the activity signal at its max weight.
    activity_saturation: f32,
    /// Decay half-life in seconds for the activity signal.
    decay_half_life_secs: f32,
}

impl Attention {
    /// Create an [`Attention`] head with the default tuning constants.
    ///
    /// Weights: error=0.4, focus=0.3, activity=0.2, distress=0.1.
    pub fn new() -> Self {
        Self {
            error_weight: 0.4,
            focus_weight: 0.3,
            activity_weight: 0.2,
            distress_weight: 0.1,
            activity_saturation: 5.0,   // 5 events/sec saturates the signal
            decay_half_life_secs: 30.0, // halve every 30 s
        }
    }

    /// Rank panes by salience, highest first.
    ///
    /// Returns a `Vec<(AdapterId, f32)>` sorted descending by score.
    /// The score is in `[0.0, 1.0]`.
    ///
    /// Empty input returns an empty vec. A single pane always scores whatever
    /// its signals dictate (not necessarily 1.0).
    pub fn rank(&self, panes: &[PaneSnapshot]) -> Vec<(AdapterId, f32)> {
        let mut scores: Vec<(AdapterId, f32)> =
            panes.iter().map(|p| (p.id, self.score(p))).collect();

        // Sort descending by score; break ties by AdapterId (lower id first)
        // so the ranking is fully deterministic.
        scores.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });

        scores
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    /// Compute the composite salience score for a single pane.
    ///
    /// Score = Σ(signal_i × weight_i), clamped to [0.0, 1.0].
    fn score(&self, pane: &PaneSnapshot) -> f32 {
        let error_score = self.error_signal(pane.error_token_count);
        let focus_score = if pane.is_user_focused {
            self.focus_weight
        } else {
            0.0
        };
        let activity_score = self.activity_signal(pane);
        let distress_score = if pane.agent_in_distress {
            self.distress_weight
        } else {
            0.0
        };

        let total = error_score + focus_score + activity_score + distress_score;
        total.clamp(0.0, 1.0)
    }

    /// Error signal: saturates at `error_weight` for >= 3 error tokens,
    /// scales linearly below that.
    fn error_signal(&self, count: u32) -> f32 {
        if count == 0 {
            return 0.0;
        }
        // Saturate at 3 tokens; scale linearly below.
        let ratio = (count as f32 / 3.0).min(1.0);
        ratio * self.error_weight
    }

    /// Activity signal with exponential recency decay.
    ///
    /// Raw activity contribution = `(events_per_sec / saturation).min(1.0) × activity_weight`.
    /// Multiplied by the recency decay factor so old activity doesn't dominate.
    fn activity_signal(&self, pane: &PaneSnapshot) -> f32 {
        if pane.events_per_sec <= 0.0 {
            return 0.0;
        }

        let raw = (pane.events_per_sec / self.activity_saturation).min(1.0) * self.activity_weight;

        // Apply exponential decay based on time since last activity.
        let decay = match pane.secs_since_last_activity {
            None => 0.0,                      // never active — no contribution
            Some(secs) if secs <= 0.0 => 1.0, // active right now
            Some(secs) => {
                // decay = 0.5 ^ (secs / half_life)
                let half_lives = secs / self.decay_half_life_secs;
                (0.5_f32).powf(half_lives)
            }
        };

        raw * decay
    }
}

impl Default for Attention {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_idle(id: u64) -> PaneSnapshot {
        PaneSnapshot {
            id: AdapterId::new(id),
            error_token_count: 0,
            is_user_focused: false,
            events_per_sec: 0.0,
            agent_in_distress: false,
            secs_since_last_activity: Some(120.0), // very stale
        }
    }

    fn make_erroring(id: u64) -> PaneSnapshot {
        PaneSnapshot {
            id: AdapterId::new(id),
            error_token_count: 5, // plenty of errors
            is_user_focused: false,
            events_per_sec: 0.0,
            agent_in_distress: false,
            secs_since_last_activity: Some(2.0),
        }
    }

    fn make_focused(id: u64) -> PaneSnapshot {
        PaneSnapshot {
            id: AdapterId::new(id),
            error_token_count: 0,
            is_user_focused: true,
            events_per_sec: 0.0,
            agent_in_distress: false,
            secs_since_last_activity: Some(1.0),
        }
    }

    fn make_active(id: u64, eps: f32) -> PaneSnapshot {
        PaneSnapshot {
            id: AdapterId::new(id),
            error_token_count: 0,
            is_user_focused: false,
            events_per_sec: eps,
            agent_in_distress: false,
            secs_since_last_activity: Some(0.5), // very recent
        }
    }

    fn make_distressed(id: u64) -> PaneSnapshot {
        PaneSnapshot {
            id: AdapterId::new(id),
            error_token_count: 0,
            is_user_focused: false,
            events_per_sec: 0.0,
            agent_in_distress: true,
            secs_since_last_activity: Some(1.0),
        }
    }

    // =======================================================================
    // Issue #46 acceptance: error pane outranks idle pane
    // =======================================================================

    #[test]
    fn error_pane_outranks_idle_pane() {
        let attention = Attention::new();
        let panes = vec![make_idle(1), make_erroring(2)];

        let ranked = attention.rank(&panes);

        assert_eq!(ranked.len(), 2);
        assert_eq!(
            ranked[0].0,
            AdapterId::new(2),
            "erroring pane must rank first"
        );
        assert_eq!(ranked[1].0, AdapterId::new(1), "idle pane must rank last");
        assert!(
            ranked[0].1 > ranked[1].1,
            "erroring score ({}) must exceed idle score ({})",
            ranked[0].1,
            ranked[1].1
        );
    }

    // =======================================================================
    // Issue #46 acceptance: user-focused pane gets bonus
    // =======================================================================

    #[test]
    fn user_focused_pane_gets_bonus() {
        let attention = Attention::new();
        let focused = make_focused(1);
        let idle = make_idle(2);

        let ranked = attention.rank(&[focused, idle]);

        assert_eq!(
            ranked[0].0,
            AdapterId::new(1),
            "focused pane must rank above idle"
        );
        assert!(ranked[0].1 > ranked[1].1);
        // The focused pane's score should include the focus_weight contribution.
        assert!(
            ranked[0].1 >= attention.focus_weight,
            "focused pane score ({}) must be at least focus_weight ({})",
            ranked[0].1,
            attention.focus_weight
        );
    }

    // =======================================================================
    // Issue #46 acceptance: recency decays
    // =======================================================================

    #[test]
    fn recency_decays_over_time() {
        let attention = Attention::new();

        let fresh = PaneSnapshot {
            id: AdapterId::new(1),
            error_token_count: 0,
            is_user_focused: false,
            events_per_sec: 5.0,
            agent_in_distress: false,
            secs_since_last_activity: Some(0.0), // just now
        };

        let stale = PaneSnapshot {
            id: AdapterId::new(2),
            error_token_count: 0,
            is_user_focused: false,
            events_per_sec: 5.0, // same activity rate…
            agent_in_distress: false,
            secs_since_last_activity: Some(300.0), // …but 5 minutes ago
        };

        let ranked = attention.rank(&[stale.clone(), fresh.clone()]);

        assert_eq!(
            ranked[0].0,
            AdapterId::new(1),
            "fresh pane must outrank stale pane"
        );
        assert!(
            ranked[0].1 > ranked[1].1,
            "fresh activity score ({}) must exceed stale activity score ({})",
            ranked[0].1,
            ranked[1].1
        );
    }

    // =======================================================================
    // Additional correctness tests
    // =======================================================================

    #[test]
    fn rank_empty_input_returns_empty() {
        let attention = Attention::new();
        let ranked = attention.rank(&[]);
        assert!(ranked.is_empty());
    }

    #[test]
    fn rank_single_pane_returns_single_entry() {
        let attention = Attention::new();
        let ranked = attention.rank(&[make_idle(42)]);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].0, AdapterId::new(42));
    }

    #[test]
    fn score_never_exceeds_one() {
        let attention = Attention::new();
        // Max out every signal simultaneously.
        let pane = PaneSnapshot {
            id: AdapterId::new(1),
            error_token_count: 100,
            is_user_focused: true,
            events_per_sec: 100.0,
            agent_in_distress: true,
            secs_since_last_activity: Some(0.0),
        };
        let ranked = attention.rank(&[pane]);
        assert!(
            ranked[0].1 <= 1.0,
            "score must not exceed 1.0, got {}",
            ranked[0].1
        );
    }

    #[test]
    fn score_is_non_negative() {
        let attention = Attention::new();
        let pane = make_idle(1);
        let ranked = attention.rank(&[pane]);
        assert!(
            ranked[0].1 >= 0.0,
            "score must not be negative, got {}",
            ranked[0].1
        );
    }

    #[test]
    fn agent_distress_adds_to_score() {
        let attention = Attention::new();
        let distressed = make_distressed(1);
        let idle = make_idle(2);

        let ranked = attention.rank(&[distressed, idle]);
        assert_eq!(
            ranked[0].0,
            AdapterId::new(1),
            "distressed pane should rank above idle"
        );
        assert!(ranked[0].1 > ranked[1].1);
    }

    #[test]
    fn deterministic_tie_breaking_by_adapter_id() {
        let attention = Attention::new();
        // Two identical panes — tie should be broken by lower adapter id first.
        let pane_a = make_idle(5);
        let pane_b = make_idle(3);

        let ranked = attention.rank(&[pane_a, pane_b]);
        assert_eq!(
            ranked[0].0,
            AdapterId::new(3),
            "tie should be broken by lower adapter id"
        );
    }

    #[test]
    fn activity_never_active_pane_scores_zero_activity() {
        let attention = Attention::new();
        let pane = PaneSnapshot {
            id: AdapterId::new(1),
            error_token_count: 0,
            is_user_focused: false,
            events_per_sec: 10.0, // claims activity…
            agent_in_distress: false,
            secs_since_last_activity: None, // …but never actually active
        };
        // Activity signal must be zero when secs_since_last_activity is None.
        let ranked = attention.rank(&[pane]);
        // Score should be 0.0 since all other signals are off and activity decays to 0.
        assert!(
            ranked[0].1 < f32::EPSILON,
            "never-active pane must score ~0, got {}",
            ranked[0].1
        );
    }

    #[test]
    fn active_pane_outranks_idle_pane() {
        let attention = Attention::new();
        let active = make_active(1, 3.0);
        let idle = make_idle(2);

        let ranked = attention.rank(&[idle, active]);
        // The active pane with recent events should outrank the stale idle pane.
        assert_eq!(
            ranked[0].0,
            AdapterId::new(1),
            "active pane should rank above idle"
        );
        assert!(ranked[0].1 > ranked[1].1);
    }

    #[test]
    fn error_signal_saturates_at_three_tokens() {
        let attention = Attention::new();
        let three_errors = PaneSnapshot {
            id: AdapterId::new(1),
            error_token_count: 3,
            is_user_focused: false,
            events_per_sec: 0.0,
            agent_in_distress: false,
            secs_since_last_activity: Some(0.0),
        };
        let many_errors = PaneSnapshot {
            id: AdapterId::new(2),
            error_token_count: 100,
            is_user_focused: false,
            events_per_sec: 0.0,
            agent_in_distress: false,
            secs_since_last_activity: Some(0.0),
        };
        // Both should reach the same saturated error score (0.4).
        let ranked = attention.rank(&[three_errors, many_errors]);
        let score_three = ranked
            .iter()
            .find(|(id, _)| *id == AdapterId::new(1))
            .unwrap()
            .1;
        let score_many = ranked
            .iter()
            .find(|(id, _)| *id == AdapterId::new(2))
            .unwrap()
            .1;
        assert!(
            (score_three - score_many).abs() < f32::EPSILON,
            "error signal must saturate: 3-error score ({score_three}) and many-error score ({score_many}) must be equal"
        );
    }
}
