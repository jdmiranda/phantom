//! Sec.8 — User-visible notification center.
//!
//! Surfaces **patterns of denial** loudly. The Layer-2 dispatch gate already
//! emits `EventKind::CapabilityDenied` for individual refusals, and Sec.3 will
//! feed those into the inspector. But individual denials are easy to miss —
//! the user only learns "this agent keeps trying things it can't do" if the
//! pattern is summarized somewhere persistent.
//!
//! The [`NotificationCenter`] watches denial timestamps **per agent** and,
//! when the same agent crosses a threshold inside a sliding window
//! (default: 3 denials in 60 seconds), pushes a `Severity::Danger` banner
//! that the renderer surfaces at the top of the screen for 30 seconds.
//!
//! ## Wiring
//!
//! Producer side: `App::update` drains the per-frame `DeniedEventSink`,
//! pushes each event into the substrate runtime (Sec.4 Defender consumer),
//! **and** calls [`NotificationCenter::record_denial`] with the agent id +
//! current wall-clock millis. Then it calls [`NotificationCenter::tick`] so
//! expired banners drop off.
//!
//! Consumer side: [`NotificationCenter::current_banner`] returns the
//! highest-severity active banner, which the
//! `phantom_ui::widgets::NotificationBanner` widget renders if `Some`.
//!
//! ## Keep it cheap
//!
//! - Per-agent timestamp queues are `VecDeque<u64>` (one `u64` per recent
//!   denial). On every `record_denial` we prune anything older than the
//!   window — bounded work, since the queue can never grow past `threshold`
//!   entries before triggering and clearing.
//! - `tick` walks the banner list once and drops expired ones. There are
//!   never more than a handful of banners (we only emit one per agent per
//!   denial window).
//! - No allocations on the steady state — `prune` uses `pop_front`, banners
//!   are pushed onto a `Vec` we never grow past ~tens of entries.
//!
//! Observability is best-effort: every method is infallible. A poisoned
//! mutex around an upstream sink is logged elsewhere; this module never
//! takes a lock.

use std::collections::{HashMap, VecDeque};

// ---------------------------------------------------------------------------
// Defaults
// ---------------------------------------------------------------------------

/// Number of denials inside [`DEFAULT_WINDOW_MS`] that surfaces a banner.
///
/// Three is "noisy enough to mean a pattern, not noise." A well-behaved
/// agent should never even hit one denial; three within a minute means the
/// model is repeatedly trying capabilities it doesn't have.
pub const DEFAULT_DENIAL_THRESHOLD: usize = 3;

/// Sliding-window width (milliseconds) for counting denials.
///
/// Sixty seconds matches the Defender spawn rule's natural beat: if a
/// Defender pops in and the offending agent is still trying things at the
/// next minute boundary, that's worth a banner.
pub const DEFAULT_WINDOW_MS: u64 = 60_000;

/// Banner display lifetime (milliseconds) before automatic dismissal.
///
/// 30 seconds is enough for the user to read it without becoming
/// permanent UI clutter. Repeat patterns will re-trigger.
pub const DEFAULT_BANNER_TTL_MS: u64 = 30_000;

// ---------------------------------------------------------------------------
// Severity
// ---------------------------------------------------------------------------

/// Banner severity, mapped 1:1 onto `Tokens::status_*` colors by the renderer.
///
/// The order is meaningful: [`Severity::Danger`] outranks [`Severity::Warn`]
/// outranks [`Severity::Info`]. `current_banner` uses this ordering so the
/// loudest active banner wins screen real estate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Info,
    Warn,
    Danger,
}

// ---------------------------------------------------------------------------
// Banner
// ---------------------------------------------------------------------------

/// A single notification banner pinned to the top of the app.
///
/// `expires_at_ms` is wall-clock millis (matches the time domain
/// [`NotificationCenter::record_denial`] expects). The banner is
/// considered active iff `now_ms < expires_at_ms`.
#[derive(Debug, Clone)]
pub struct Banner {
    pub message: String,
    pub severity: Severity,
    pub expires_at_ms: u64,
}

// ---------------------------------------------------------------------------
// NotificationCenter
// ---------------------------------------------------------------------------

/// Tracks denial patterns and produces user-visible banners.
///
/// Owned by `App` and ticked once per frame from `update.rs`. See module
/// docs for the producer/consumer wiring.
pub struct NotificationCenter {
    /// Active banners. Pruned by [`Self::tick`].
    banners: Vec<Banner>,
    /// Per-agent ring of recent denial timestamps (wall-clock millis).
    ///
    /// Keys are `u64` to match the `agent_id` field on
    /// `EventKind::CapabilityDenied`. The deque is pruned of stale entries
    /// on every push, so its length is bounded by `threshold`.
    denial_history: HashMap<u64, VecDeque<u64>>,
    /// Number of denials within `window_ms` that triggers a banner.
    threshold: usize,
    /// Sliding-window width in milliseconds.
    window_ms: u64,
    /// Per-banner TTL in milliseconds.
    banner_ttl_ms: u64,
}

impl Default for NotificationCenter {
    fn default() -> Self {
        Self::new()
    }
}

impl NotificationCenter {
    /// Construct a center with the documented defaults
    /// (3 denials in 60s → a 30s Danger banner).
    pub fn new() -> Self {
        Self {
            banners: Vec::new(),
            denial_history: HashMap::new(),
            threshold: DEFAULT_DENIAL_THRESHOLD,
            window_ms: DEFAULT_WINDOW_MS,
            banner_ttl_ms: DEFAULT_BANNER_TTL_MS,
        }
    }

    /// Construct a center with explicit thresholds. Reserved for tests so
    /// they don't have to wait minutes of wall clock to exercise expiry.
    #[cfg(test)]
    pub fn with_config(threshold: usize, window_ms: u64, banner_ttl_ms: u64) -> Self {
        Self {
            banners: Vec::new(),
            denial_history: HashMap::new(),
            threshold,
            window_ms,
            banner_ttl_ms,
        }
    }

    /// Record a single capability denial for `agent_id` at wall-clock
    /// `timestamp_ms`. If this push completes a pattern (≥`threshold`
    /// timestamps inside the sliding window), a `Severity::Danger` banner
    /// is enqueued and the agent's history is cleared so the same pattern
    /// doesn't re-trigger every frame for the same offender.
    ///
    /// Cheap on the steady state: bounded by `threshold` prunes per call.
    pub fn record_denial(&mut self, agent_id: u64, timestamp_ms: u64) {
        let window_ms = self.window_ms;
        let threshold = self.threshold;
        let banner_ttl = self.banner_ttl_ms;
        let cutoff = timestamp_ms.saturating_sub(window_ms);

        let history = self.denial_history.entry(agent_id).or_default();
        // Prune stale timestamps from the front (deque is time-ordered).
        while let Some(&front) = history.front() {
            if front < cutoff {
                history.pop_front();
            } else {
                break;
            }
        }
        history.push_back(timestamp_ms);

        if history.len() >= threshold {
            // Reset so the next pattern needs a fresh `threshold` denials.
            history.clear();
            // Drop the entry entirely to keep the map small for one-shot
            // offenders — recreated lazily on the next denial.
            self.denial_history.remove(&agent_id);

            self.banners.push(Banner {
                message: format!(
                    "Agent #{agent_id} hit {threshold}+ capability denials in {}s — \
                     check the inspector for the source chain.",
                    window_ms / 1000,
                ),
                severity: Severity::Danger,
                expires_at_ms: timestamp_ms.saturating_add(banner_ttl),
            });
        }
    }

    /// Directly push a pre-formed banner.
    ///
    /// Use this for non-denial banner sources (e.g. live shader reload errors)
    /// that need to surface a Severity::Warn message without going through the
    /// denial-pattern counter.
    pub fn push_banner(&mut self, banner: Banner) {
        self.banners.push(banner);
    }

    /// Drop banners whose `expires_at_ms` has passed.
    ///
    /// Called once per frame from `App::update`. Linear in the banner count,
    /// which is tiny (we cap one per offender per window).
    pub fn tick(&mut self, now_ms: u64) {
        self.banners.retain(|b| now_ms < b.expires_at_ms);
    }

    /// The highest-severity active banner, if any.
    ///
    /// Severity is ordered Info < Warn < Danger; ties broken by *most
    /// recently inserted* (later in `banners` wins). The renderer reads
    /// this each frame to decide whether to draw the banner widget.
    pub fn current_banner(&self) -> Option<&Banner> {
        self.banners
            .iter()
            .max_by_key(|b| (b.severity, b.expires_at_ms))
    }

    /// Active banner count. Test/debug helper.
    #[cfg(test)]
    pub fn banner_count(&self) -> usize {
        self.banners.len()
    }

    /// Recent-denial count for `agent_id`. Test helper to confirm pruning.
    #[cfg(test)]
    pub fn denial_count(&self, agent_id: u64) -> usize {
        self.denial_history
            .get(&agent_id)
            .map(|q| q.len())
            .unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Reaching the threshold inside the window must enqueue exactly one
    /// `Severity::Danger` banner with a message that names the offending
    /// agent. This is the primary success path for Sec.8.
    #[test]
    fn triggers_banner_at_threshold() {
        let mut nc = NotificationCenter::with_config(3, 60_000, 30_000);
        // Two denials — under threshold, no banner yet.
        nc.record_denial(7, 1_000);
        nc.record_denial(7, 2_000);
        assert_eq!(nc.banner_count(), 0, "no banner before threshold");

        // The third denial completes the pattern.
        nc.record_denial(7, 3_000);

        assert_eq!(nc.banner_count(), 1, "exactly one banner at threshold");
        let b = nc.current_banner().expect("banner must be active");
        assert_eq!(b.severity, Severity::Danger);
        assert!(
            b.message.contains("#7"),
            "banner message must name the agent: {:?}",
            b.message
        );
        // TTL math: enqueued at t=3000, ttl=30000 → expires at 33000.
        assert_eq!(b.expires_at_ms, 33_000);
    }

    /// Below-threshold denial counts must NOT enqueue a banner. Two
    /// hits in the window is "noise," not a pattern.
    #[test]
    fn does_not_trigger_below_threshold() {
        let mut nc = NotificationCenter::with_config(3, 60_000, 30_000);
        nc.record_denial(7, 1_000);
        nc.record_denial(7, 2_000);

        assert_eq!(nc.banner_count(), 0, "below threshold, no banner");
        assert!(nc.current_banner().is_none());
        assert_eq!(
            nc.denial_count(7),
            2,
            "history retains both pre-threshold timestamps"
        );
    }

    /// Timestamps that fall outside the sliding window must be pruned and
    /// must NOT count toward the threshold. Two old hits + one new hit
    /// is one effective hit, no banner.
    #[test]
    fn prunes_old_timestamps_outside_window() {
        let mut nc = NotificationCenter::with_config(3, 60_000, 30_000);

        // Two ancient denials, well outside the 60s window.
        nc.record_denial(9, 0);
        nc.record_denial(9, 5_000);
        assert_eq!(nc.denial_count(9), 2);

        // Big jump — both old entries should be evicted before the new
        // timestamp lands. Effective count: 1.
        nc.record_denial(9, 200_000);

        assert_eq!(
            nc.denial_count(9),
            1,
            "old entries must be pruned outside window_ms"
        );
        assert_eq!(
            nc.banner_count(),
            0,
            "pruned timestamps must not count toward threshold"
        );
    }
}
