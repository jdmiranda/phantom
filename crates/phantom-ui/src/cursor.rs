//! Cursor blink primitive — shared timer so InputBar and the terminal cursor
//! never each re-implement timing.
//!
//! # Usage
//!
//! ```
//! use phantom_ui::cursor::CursorBlink;
//!
//! let mut blink = CursorBlink::default();
//!
//! // Advance the timer with the current wall-clock millisecond timestamp.
//! let visible = blink.tick(530);   // exactly one period → toggles to false
//! assert!(!visible);
//! ```
//!
//! The default period is **530 ms**, matching macOS Terminal and most
//! system-level cursor blink rates.
//!
//! # Token color helper
//!
//! [`CursorBlink::color`] accepts a `[f32; 4]` base color (e.g.
//! `tokens.colors.text_primary`) and returns it unmodified when the cursor is
//! visible, or fully transparent when it is hidden, so the caller only needs
//! one code path regardless of blink state.

use crate::tokens::Tokens;

/// Default blink period in milliseconds — matches macOS Terminal.
pub const DEFAULT_PERIOD_MS: u64 = 530;

/// Shared cursor blink timer.
///
/// Call [`tick`](CursorBlink::tick) once per frame with the current
/// wall-clock time in milliseconds. The returned `bool` is the new
/// visibility state; the caller should use it to decide whether to draw the
/// cursor.
///
/// # Example
///
/// ```
/// use phantom_ui::cursor::CursorBlink;
///
/// let mut blink = CursorBlink::default();
/// assert!(blink.is_visible());       // starts visible
/// assert!(blink.visible());          // alias also works
///
/// let v = blink.tick(530);
/// assert!(!v);                       // first period elapsed → hidden
///
/// let v = blink.tick(1060);
/// assert!(v);                        // second period → visible again
/// ```
#[derive(Debug, Clone)]
pub struct CursorBlink {
    /// Duration of one on/off phase, in milliseconds.
    period_ms: u64,
    /// Timestamp (ms) of the last visibility toggle.
    last_toggle_ms: u64,
    /// Current visibility state.
    visible: bool,
}

impl CursorBlink {
    /// Construct a blink timer with a custom period, anchored to `t = 0`.
    ///
    /// The timer always starts at timestamp zero so that callers can simply
    /// pass wall-clock milliseconds to [`tick`](CursorBlink::tick) without
    /// needing to supply the construction time.
    ///
    /// # Panics
    ///
    /// Panics if `period_ms` is zero, as a zero period would cause a
    /// divide-by-zero in [`tick`](CursorBlink::tick).
    pub fn new(period_ms: u64) -> Self {
        assert!(period_ms > 0, "period_ms must be greater than zero");
        Self {
            period_ms,
            last_toggle_ms: 0,
            visible: true,
        }
    }

    /// Construct a blink timer with a custom period, anchored to `now_ms`.
    ///
    /// `now_ms` is the current wall-clock timestamp in milliseconds; it anchors
    /// the first toggle so the cursor is never immediately hidden on creation.
    ///
    /// # Panics
    ///
    /// Panics if `period_ms` is zero.
    pub fn new_at(period_ms: u64, now_ms: u64) -> Self {
        assert!(period_ms > 0, "period_ms must be greater than zero");
        Self {
            period_ms,
            last_toggle_ms: now_ms,
            visible: true,
        }
    }

    /// Return the configured blink period in milliseconds.
    pub fn period_ms(&self) -> u64 {
        self.period_ms
    }

    /// Return the current visibility without advancing the timer.
    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// Return the current visibility without advancing the timer.
    ///
    /// Alias for [`is_visible`](CursorBlink::is_visible) kept for
    /// backward compatibility with call sites not yet updated.
    pub fn visible(&self) -> bool {
        self.visible
    }

    /// Advance the timer to `now_ms` and return the current visibility.
    ///
    /// Toggles the internal visible state each time a full `period_ms`
    /// has elapsed since the last toggle. Multiple periods elapsed in one call
    /// are folded correctly — only the parity of elapsed periods matters.
    ///
    /// Returns the new visibility state after applying any toggles.
    pub fn tick(&mut self, now_ms: u64) -> bool {
        // Guard against backward time (e.g. clock wrap or test ordering).
        if now_ms <= self.last_toggle_ms {
            return self.visible;
        }

        let elapsed = now_ms - self.last_toggle_ms;
        let periods = elapsed / self.period_ms;

        if periods > 0 {
            // Odd number of periods flips visibility; even leaves it unchanged.
            if periods % 2 == 1 {
                self.visible = !self.visible;
            }
            // Advance last_toggle to the most-recent period boundary so that
            // sub-period carry-over is preserved correctly.
            self.last_toggle_ms += periods * self.period_ms;
        }

        self.visible
    }

    /// Reset the timer, anchoring the next toggle to `now_ms` and restoring
    /// full visibility. Useful when the user types — the cursor should restart
    /// its blink cycle from the "on" state.
    pub fn reset(&mut self, now_ms: u64) {
        self.last_toggle_ms = now_ms;
        self.visible = true;
    }

    /// Token color helper.
    ///
    /// Returns `base_color` when the cursor is visible and a fully-transparent
    /// copy when it is hidden. The caller passes `tokens.colors.text_primary`
    /// (or any role) and gets back the correct RGBA without needing to branch.
    ///
    /// ```
    /// use phantom_ui::cursor::CursorBlink;
    /// use phantom_ui::tokens::{ColorRoles, Tokens};
    /// use phantom_ui::RenderCtx;
    ///
    /// let blink = CursorBlink::default();
    /// let tokens = Tokens::phosphor(RenderCtx::fallback());
    /// let color = blink.color(tokens.colors.text_primary);
    /// // Cursor starts visible, so color is unchanged.
    /// assert_eq!(color, tokens.colors.text_primary);
    /// ```
    pub fn color(&self, base_color: [f32; 4]) -> [f32; 4] {
        if self.visible {
            base_color
        } else {
            [base_color[0], base_color[1], base_color[2], 0.0]
        }
    }

    /// Convenience wrapper: resolve `tokens.colors.text_primary`, modulate by
    /// visibility, and return the resulting RGBA.
    pub fn text_primary_color(&self, tokens: &Tokens) -> [f32; 4] {
        self.color(tokens.colors.text_primary)
    }
}

impl Default for CursorBlink {
    /// Creates a blink timer anchored to `t = 0` with the default 530 ms period.
    fn default() -> Self {
        Self::new(DEFAULT_PERIOD_MS)
    }
}

// -------------------------------------------------------------------------
// Tests
// -------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RenderCtx;
    use crate::tokens::Tokens;

    // -- Default construction --

    #[test]
    fn default_period_is_530ms() {
        let b = CursorBlink::default();
        assert_eq!(b.period_ms(), 530);
    }

    #[test]
    fn default_starts_visible() {
        let b = CursorBlink::default();
        assert!(b.is_visible());
        assert!(b.visible());
    }

    // -- Zero-period guard --

    #[test]
    #[should_panic(expected = "period_ms must be greater than zero")]
    fn new_with_zero_period_panics() {
        let _ = CursorBlink::new(0);
    }

    // -- Toggle at period boundaries --

    #[test]
    fn tick_before_period_does_not_toggle() {
        let mut b = CursorBlink::default();
        // 529 ms — one ms short of the period.
        let vis = b.tick(529);
        assert!(vis, "cursor must stay visible before period elapses");
    }

    #[test]
    fn tick_at_exact_period_toggles() {
        let mut b = CursorBlink::default();
        let vis = b.tick(530);
        assert!(!vis, "cursor must go hidden exactly at period boundary");
    }

    #[test]
    fn tick_just_past_period_toggles() {
        let mut b = CursorBlink::default();
        let vis = b.tick(531);
        assert!(!vis);
    }

    #[test]
    fn tick_two_periods_returns_to_visible() {
        let mut b = CursorBlink::default();
        b.tick(530); // → hidden
        let vis = b.tick(1060); // → visible again
        assert!(vis);
    }

    #[test]
    fn tick_three_periods_is_hidden() {
        let mut b = CursorBlink::default();
        b.tick(530); // period 1 → hidden
        b.tick(1060); // period 2 → visible
        let vis = b.tick(1590); // period 3 → hidden
        assert!(!vis);
    }

    #[test]
    fn tick_skipping_many_periods_uses_parity() {
        let mut b = CursorBlink::default();
        // Jump 7 periods at once from t=0.  7 is odd, so visible flips once.
        let vis = b.tick(7 * 530);
        assert!(!vis, "odd number of periods must flip to hidden");

        // Jump another 8 periods (even). State should not change.
        let vis2 = b.tick(7 * 530 + 8 * 530);
        assert!(
            !vis2,
            "even number of additional periods must leave state unchanged"
        );
    }

    // -- Sub-period carry-over --

    #[test]
    fn carry_over_is_preserved_after_toggle() {
        let mut b = CursorBlink::default();
        // Advance 600 ms: 1 full period (530) + 70 ms carry.
        b.tick(600);
        assert!(!b.is_visible());

        // Now 460 ms later: carry(70) + 460 = 530 → another full period.
        let vis = b.tick(1060);
        assert!(vis, "carry-over should count toward the next period");
    }

    // -- Backward time guard --

    #[test]
    fn backward_time_does_not_toggle() {
        let mut b = CursorBlink::default();
        b.tick(1000); // somewhere past several periods
        let state_before = b.is_visible();
        let vis = b.tick(500); // earlier timestamp
        assert_eq!(
            vis, state_before,
            "backward tick must not change visibility"
        );
    }

    #[test]
    fn same_timestamp_does_not_toggle() {
        let mut b = CursorBlink::default();
        b.tick(530); // → hidden
        let vis = b.tick(530); // same ts
        assert!(!vis, "same timestamp must not re-toggle");
    }

    // -- Reset --

    #[test]
    fn reset_restores_visible_and_anchors_timer() {
        let mut b = CursorBlink::default();
        b.tick(530); // → hidden
        b.reset(1000);
        assert!(b.is_visible(), "reset must restore visible");
        // 529 ms after reset — should still be visible.
        let vis = b.tick(1529);
        assert!(vis);
        // One full period after reset → hidden.
        let vis2 = b.tick(1530);
        assert!(!vis2);
    }

    // -- Color helper --

    #[test]
    fn color_passes_through_base_when_visible() {
        let b = CursorBlink::default(); // visible = true
        let base = [0.5, 1.0, 0.7, 1.0];
        assert_eq!(b.color(base), base);
    }

    #[test]
    fn color_zeroes_alpha_when_hidden() {
        let mut b = CursorBlink::default();
        b.tick(530); // → hidden
        let base = [0.5, 1.0, 0.7, 1.0];
        let result = b.color(base);
        assert_eq!(result[0], 0.5);
        assert_eq!(result[1], 1.0);
        assert_eq!(result[2], 0.7);
        assert_eq!(result[3], 0.0, "alpha must be zero when cursor is hidden");
    }

    #[test]
    fn text_primary_color_uses_token_role() {
        let b = CursorBlink::default(); // visible
        let tokens = Tokens::phosphor(RenderCtx::fallback());
        let c = b.text_primary_color(&tokens);
        assert_eq!(c, tokens.colors.text_primary);
    }

    #[test]
    fn text_primary_color_transparent_when_hidden() {
        let mut b = CursorBlink::default();
        b.tick(530); // → hidden
        let tokens = Tokens::phosphor(RenderCtx::fallback());
        let c = b.text_primary_color(&tokens);
        assert_eq!(c[3], 0.0);
        // RGB preserved.
        assert_eq!(c[0], tokens.colors.text_primary[0]);
    }

    // -- Custom period --

    #[test]
    fn custom_period_toggles_at_custom_boundary() {
        let mut b = CursorBlink::new(200);
        assert!(b.tick(199)); // just before → visible
        assert!(!b.tick(200)); // at boundary → hidden
        assert!(b.tick(400)); // two periods → visible
    }

    // -- new() anchoring --

    #[test]
    fn new_anchors_to_zero() {
        // Timer always starts at t=0; a tick at t=529 is sub-period → no toggle.
        let mut b = CursorBlink::new(DEFAULT_PERIOD_MS);
        assert!(b.tick(529));
        // Tick at t=530 (exactly one period from anchor) → hidden.
        assert!(!b.tick(530));
    }

    // -- new_at() anchoring --

    #[test]
    fn new_at_anchors_to_provided_timestamp() {
        // Timer starts at t=1000; a tick at t=1529 is sub-period → no toggle.
        let mut b = CursorBlink::new_at(DEFAULT_PERIOD_MS, 1000);
        assert!(b.tick(1529));
        // Tick at t=1530 (exactly one period from anchor) → hidden.
        assert!(!b.tick(1530));
    }

    // -- period_ms accessor --

    #[test]
    fn period_ms_accessor_returns_configured_period() {
        let b = CursorBlink::new(200);
        assert_eq!(b.period_ms(), 200);
    }
}
