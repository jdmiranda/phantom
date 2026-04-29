//! Issue #30 — `FocusRing` overlay widget.
//!
//! Draws a 2 px outline around the focused pane using `colors.accent_focus`
//! from the token table. The ring fades in/out over 100 ms when focus changes
//! to avoid jarring visual pops.
//!
//! The widget is associated with a specific pane via [`phantom_adapter::AppId`]
//! so the caller can cycle focus across panes by calling [`FocusRing::set_focused`].
//! When `focused` is `None` the ring is invisible (alpha = 0).
//!
//! Corner radius matches `tokens.radius_sm()` (2 px by default).
//!
//! # Animation model
//!
//! [`FocusRing`] tracks a `[0.0, 1.0]` opacity that lerps toward the target
//! each frame:
//! - Focus gained → target = 1.0, ramps up over [`FADE_DURATION_MS`].
//! - Focus lost  → target = 0.0, ramps down over [`FADE_DURATION_MS`].
//!
//! Call [`FocusRing::tick`] once per frame with the elapsed milliseconds.
//!
//! # Examples
//!
//! ```rust,ignore
//! use phantom_ui::widgets::focus_ring::FocusRing;
//!
//! let mut ring = FocusRing::new();
//! ring.set_focused(Some(42)); // pane with AppId 42 gains focus
//! ring.tick(50);              // advance 50 ms → half-way through fade-in
//! let quads = ring.render_quads(&rect);
//! ```

use phantom_adapter::AppId;

use crate::layout::Rect;
use crate::render_ctx::RenderCtx;
use crate::tokens::Tokens;
use crate::widgets::{TextSegment, Widget};
use phantom_renderer::quads::QuadInstance;

// ─────────────────────────────────────────────────────────────────────────────
// Constants
// ─────────────────────────────────────────────────────────────────────────────

/// Fade-in / fade-out duration in milliseconds.
pub const FADE_DURATION_MS: f32 = 100.0;

/// Thickness of the focus outline in pixels (2 px per spec).
const RING_THICKNESS: f32 = 2.0;

// ─────────────────────────────────────────────────────────────────────────────
// FocusRing
// ─────────────────────────────────────────────────────────────────────────────

/// Animated focus-ring overlay.
///
/// Wraps an optional [`AppId`] (the ID assigned by the adapter registry) so
/// the application layer can cycle focus without this widget knowing anything
/// about pane layout.
///
/// Derive `Debug` manually because `AppId = u32` is `Debug`-able.
#[derive(Debug, Clone)]
pub struct FocusRing {
    /// The pane that currently has focus, or `None` when no pane is focused.
    focused: Option<AppId>,
    /// Animation opacity — 0.0 (invisible) to 1.0 (fully opaque).
    opacity: f32,
    /// Previous focus for detecting transitions.
    prev_focused: Option<AppId>,
    /// Live render metrics.
    ctx: RenderCtx,
}

impl FocusRing {
    /// Create a [`FocusRing`] with no pane focused (fully invisible).
    pub fn new() -> Self {
        Self {
            focused: None,
            opacity: 0.0,
            prev_focused: None,
            ctx: RenderCtx::fallback(),
        }
    }

    /// Update the focused pane. If the new value differs from the current one
    /// the animation immediately starts transitioning toward the new target.
    pub fn set_focused(&mut self, id: Option<AppId>) {
        self.focused = id;
    }

    /// Update the live render context.
    pub fn set_render_ctx(&mut self, ctx: RenderCtx) {
        self.ctx = ctx;
    }

    /// The pane that currently has focus, or `None` when no pane is focused.
    pub fn focused(&self) -> Option<AppId> {
        self.focused
    }

    /// Return the current animated opacity in `[0.0, 1.0]`.
    pub fn opacity(&self) -> f32 {
        self.opacity
    }

    /// Advance the animation by `dt_ms` milliseconds.
    ///
    /// The opacity moves linearly toward `1.0` when a pane is focused and
    /// toward `0.0` when no pane is focused. One full transition (0 → 1 or
    /// 1 → 0) takes [`FADE_DURATION_MS`] milliseconds.
    pub fn tick(&mut self, dt_ms: f32) {
        // Detect a focus change and reset the transition direction.
        if self.focused != self.prev_focused {
            self.prev_focused = self.focused;
        }

        let target = if self.focused.is_some() {
            1.0_f32
        } else {
            0.0_f32
        };
        let step = dt_ms / FADE_DURATION_MS;

        self.opacity = if target > self.opacity {
            (self.opacity + step).min(1.0)
        } else {
            (self.opacity - step).max(0.0)
        };
    }

    // ── Private ───────────────────────────────────────────────────────────────

    /// Modulate the `accent_focus` token color by the current animated opacity.
    fn ring_color(&self, tokens: &Tokens) -> [f32; 4] {
        let c = tokens.colors.accent_focus;
        [c[0], c[1], c[2], c[3] * self.opacity]
    }
}

impl Default for FocusRing {
    fn default() -> Self {
        Self::new()
    }
}

impl Widget for FocusRing {
    /// Emit four border quads forming the ring outline.
    ///
    /// Each quad is `RING_THICKNESS` pixels wide (top / bottom) or tall
    /// (left / right), with the same corner radius as `tokens.radius_sm()`.
    /// When `opacity == 0.0` (no pane focused or fade-out complete) the quads
    /// have zero alpha and are invisible — but are still emitted so the
    /// renderer pipeline stays constant.
    fn render_quads(&self, rect: &Rect) -> Vec<QuadInstance> {
        let t = Tokens::phosphor(self.ctx);
        let color = self.ring_color(&t);
        let r = t.radius_sm();

        // Four border quads (top, bottom, left, right).
        vec![
            // Top
            QuadInstance {
                pos: [rect.x, rect.y],
                size: [rect.width, RING_THICKNESS],
                color,
                border_radius: r,
            },
            // Bottom
            QuadInstance {
                pos: [rect.x, rect.y + rect.height - RING_THICKNESS],
                size: [rect.width, RING_THICKNESS],
                color,
                border_radius: r,
            },
            // Left
            QuadInstance {
                pos: [rect.x, rect.y],
                size: [RING_THICKNESS, rect.height],
                color,
                border_radius: r,
            },
            // Right
            QuadInstance {
                pos: [rect.x + rect.width - RING_THICKNESS, rect.y],
                size: [RING_THICKNESS, rect.height],
                color,
                border_radius: r,
            },
        ]
    }

    /// The focus ring has no text content.
    fn render_text(&self, _rect: &Rect) -> Vec<TextSegment> {
        Vec::new()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render_ctx::RenderCtx;
    use crate::tokens::Tokens;

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn ring_rect() -> Rect {
        Rect {
            x: 10.0,
            y: 20.0,
            width: 400.0,
            height: 300.0,
        }
    }

    // ── Construction ──────────────────────────────────────────────────────────

    #[test]
    fn new_is_unfocused_and_invisible() {
        let ring = FocusRing::new();
        assert!(ring.focused().is_none());
        assert_eq!(ring.opacity(), 0.0);
    }

    #[test]
    fn default_matches_new() {
        let a = FocusRing::new();
        let b = FocusRing::default();
        assert_eq!(a.focused(), b.focused());
        assert_eq!(a.opacity(), b.opacity());
    }

    // ── Focus state ───────────────────────────────────────────────────────────

    #[test]
    fn set_focused_updates_id() {
        let mut ring = FocusRing::new();
        ring.set_focused(Some(7));
        assert_eq!(ring.focused(), Some(7));
    }

    #[test]
    fn clear_focused_sets_none() {
        let mut ring = FocusRing::new();
        ring.set_focused(Some(1));
        ring.set_focused(None);
        assert!(ring.focused().is_none());
    }

    // ── Animation ────────────────────────────────────────────────────────────

    #[test]
    fn tick_with_focus_increases_opacity() {
        let mut ring = FocusRing::new();
        ring.set_focused(Some(1));
        ring.tick(50.0); // 50 ms → 50 % of 100 ms = 0.5
        assert!(
            (ring.opacity() - 0.5).abs() < 0.01,
            "expected ~0.5, got {}",
            ring.opacity()
        );
    }

    #[test]
    fn tick_full_duration_reaches_full_opacity() {
        let mut ring = FocusRing::new();
        ring.set_focused(Some(1));
        ring.tick(FADE_DURATION_MS);
        assert_eq!(ring.opacity(), 1.0);
    }

    #[test]
    fn tick_over_full_duration_clamps_at_one() {
        let mut ring = FocusRing::new();
        ring.set_focused(Some(1));
        ring.tick(FADE_DURATION_MS * 5.0);
        assert_eq!(ring.opacity(), 1.0);
    }

    #[test]
    fn tick_without_focus_decreases_opacity() {
        let mut ring = FocusRing::new();
        ring.set_focused(Some(1));
        ring.tick(FADE_DURATION_MS); // fully visible
        ring.set_focused(None);
        ring.tick(50.0); // fade out 50 ms → 0.5
        assert!(
            (ring.opacity() - 0.5).abs() < 0.01,
            "expected ~0.5, got {}",
            ring.opacity()
        );
    }

    #[test]
    fn tick_fade_out_reaches_zero() {
        let mut ring = FocusRing::new();
        ring.set_focused(Some(1));
        ring.tick(FADE_DURATION_MS);
        ring.set_focused(None);
        ring.tick(FADE_DURATION_MS);
        assert_eq!(ring.opacity(), 0.0);
    }

    // ── Quad rendering ────────────────────────────────────────────────────────

    #[test]
    fn always_emits_four_quads() {
        // Invisible ring still emits quads (constant pipeline).
        let ring = FocusRing::new();
        let quads = ring.render_quads(&ring_rect());
        assert_eq!(
            quads.len(),
            4,
            "focus ring must emit exactly 4 border quads"
        );
    }

    #[test]
    fn unfocused_ring_has_zero_alpha() {
        let ring = FocusRing::new(); // opacity = 0.0
        let quads = ring.render_quads(&ring_rect());
        for quad in &quads {
            assert_eq!(quad.color[3], 0.0, "unfocused ring alpha must be 0");
        }
    }

    #[test]
    fn fully_focused_ring_uses_accent_focus_color() {
        let ctx = RenderCtx::fallback();
        let t = Tokens::phosphor(ctx);
        let mut ring = FocusRing::new();
        ring.set_focused(Some(1));
        ring.tick(FADE_DURATION_MS); // opacity = 1.0
        let quads = ring.render_quads(&ring_rect());
        // All quads should have the accent_focus color (rgb channels match).
        for quad in &quads {
            assert!((quad.color[0] - t.colors.accent_focus[0]).abs() < 0.001);
            assert!((quad.color[1] - t.colors.accent_focus[1]).abs() < 0.001);
            assert!((quad.color[2] - t.colors.accent_focus[2]).abs() < 0.001);
            assert_eq!(quad.color[3], 1.0, "fully focused ring alpha must be 1.0");
        }
    }

    #[test]
    fn ring_border_radius_matches_radius_sm_token() {
        let ctx = RenderCtx::fallback();
        let t = Tokens::phosphor(ctx);
        let ring = FocusRing::new();
        let quads = ring.render_quads(&ring_rect());
        for quad in &quads {
            assert_eq!(quad.border_radius, t.radius_sm());
        }
    }

    #[test]
    fn ring_thickness_is_two_pixels() {
        let ring = FocusRing::new();
        let rect = ring_rect();
        let quads = ring.render_quads(&rect);
        // Top and bottom quads: height == RING_THICKNESS.
        assert!(
            (quads[0].size[1] - RING_THICKNESS).abs() < 0.01,
            "top ring height"
        );
        assert!(
            (quads[1].size[1] - RING_THICKNESS).abs() < 0.01,
            "bottom ring height"
        );
        // Left and right quads: width == RING_THICKNESS.
        assert!(
            (quads[2].size[0] - RING_THICKNESS).abs() < 0.01,
            "left ring width"
        );
        assert!(
            (quads[3].size[0] - RING_THICKNESS).abs() < 0.01,
            "right ring width"
        );
    }

    #[test]
    fn ring_encloses_rect() {
        let ring = FocusRing::new();
        let rect = ring_rect();
        let quads = ring.render_quads(&rect);

        let top = &quads[0];
        let bottom = &quads[1];
        let left = &quads[2];
        let right = &quads[3];

        // Top edge at rect.y.
        assert!((top.pos[1] - rect.y).abs() < 0.01);
        // Bottom edge at rect.y + rect.height - RING_THICKNESS.
        assert!((bottom.pos[1] - (rect.y + rect.height - RING_THICKNESS)).abs() < 0.01);
        // Left edge at rect.x.
        assert!((left.pos[0] - rect.x).abs() < 0.01);
        // Right edge at rect.x + rect.width - RING_THICKNESS.
        assert!((right.pos[0] - (rect.x + rect.width - RING_THICKNESS)).abs() < 0.01);
    }

    // ── Text rendering ────────────────────────────────────────────────────────

    #[test]
    fn render_text_is_always_empty() {
        let ring = FocusRing::new();
        assert!(ring.render_text(&ring_rect()).is_empty());
    }

    // ── Widget object safety ──────────────────────────────────────────────────

    #[test]
    fn focus_ring_is_object_safe() {
        let ring = FocusRing::new();
        let widget: &dyn Widget = &ring;
        let quads = widget.render_quads(&ring_rect());
        assert_eq!(quads.len(), 4);
    }

    // ── AppId association ─────────────────────────────────────────────────────

    #[test]
    fn cycling_focus_between_panes() {
        let mut ring = FocusRing::new();
        ring.set_focused(Some(1));
        ring.tick(FADE_DURATION_MS);
        assert_eq!(ring.focused(), Some(1));
        assert_eq!(ring.opacity(), 1.0);

        ring.set_focused(Some(2));
        // Opacity stays at 1.0 (still focused, just different pane).
        ring.tick(0.0);
        assert_eq!(ring.focused(), Some(2));
        assert_eq!(ring.opacity(), 1.0);
    }
}

// ---------------------------------------------------------------------------
// Issue #178 — Focus ring: single-focus invariant across 3 panes
// ---------------------------------------------------------------------------

#[cfg(test)]
mod pane_focus_state_tests {
    use super::*;

    const PANE_A: AppId = 1;
    const PANE_B: AppId = 2;
    const PANE_C: AppId = 3;

    // Focusing PANE_B must be reported by focused().
    #[test]
    fn focus_pane_b_is_reported() {
        let mut ring = FocusRing::new();
        ring.set_focused(Some(PANE_B));
        assert_eq!(ring.focused(), Some(PANE_B));
    }

    // After focusing A then C, only C must be the focused pane.
    #[test]
    fn focus_transfers_from_a_to_c() {
        let mut ring = FocusRing::new();
        ring.set_focused(Some(PANE_A));
        ring.set_focused(Some(PANE_C));
        assert_eq!(ring.focused(), Some(PANE_C));
        assert_ne!(ring.focused(), Some(PANE_A));
    }

    // Cycling through A → B → C: at each step exactly one pane is focused.
    #[test]
    fn only_one_pane_is_focused_at_a_time() {
        let mut ring = FocusRing::new();

        ring.set_focused(Some(PANE_A));
        assert_eq!(ring.focused(), Some(PANE_A));

        ring.set_focused(Some(PANE_B));
        assert_eq!(ring.focused(), Some(PANE_B));
        assert_ne!(ring.focused(), Some(PANE_A));
        assert_ne!(ring.focused(), Some(PANE_C));

        ring.set_focused(Some(PANE_C));
        assert_eq!(ring.focused(), Some(PANE_C));
        assert_ne!(ring.focused(), Some(PANE_A));
        assert_ne!(ring.focused(), Some(PANE_B));
    }

    // A → B → C navigation: each step yields the correct pane.
    #[test]
    fn navigate_focus_a_to_b_to_c() {
        let mut ring = FocusRing::new();
        let panes = [PANE_A, PANE_B, PANE_C];
        for &pane in &panes {
            ring.set_focused(Some(pane));
            assert_eq!(ring.focused(), Some(pane), "after focusing {pane}");
        }
    }

    // Clearing focus (None) leaves no pane focused.
    #[test]
    fn clear_focus_leaves_no_pane_focused() {
        let mut ring = FocusRing::new();
        ring.set_focused(Some(PANE_A));
        ring.set_focused(None);
        assert_eq!(ring.focused(), None);
    }

    // Re-focusing the same pane is idempotent.
    #[test]
    fn refocus_same_pane_is_idempotent() {
        let mut ring = FocusRing::new();
        ring.set_focused(Some(PANE_B));
        ring.tick(FADE_DURATION_MS); // fully opaque
        ring.set_focused(Some(PANE_B)); // same pane again
        ring.tick(0.0);
        assert_eq!(ring.focused(), Some(PANE_B));
        assert_eq!(ring.opacity(), 1.0);
    }

    // When focus moves from one pane to another, opacity stays at 1.0 because
    // a pane is still focused — only the identity changes.
    #[test]
    fn opacity_stays_high_when_focus_moves_between_panes() {
        let mut ring = FocusRing::new();
        ring.set_focused(Some(PANE_A));
        ring.tick(FADE_DURATION_MS); // fade fully in
        assert_eq!(ring.opacity(), 1.0);

        ring.set_focused(Some(PANE_B)); // different pane
        ring.tick(0.0);                 // one zero-dt tick
        // Should not drop opacity since something is still focused.
        assert_eq!(ring.opacity(), 1.0, "opacity must not fade when focus merely moves");
    }

    // When focus is cleared, ticking must gradually reduce opacity toward 0.
    #[test]
    fn opacity_fades_when_focus_cleared() {
        let mut ring = FocusRing::new();
        ring.set_focused(Some(PANE_A));
        ring.tick(FADE_DURATION_MS); // fully opaque

        ring.set_focused(None);
        ring.tick(FADE_DURATION_MS / 2.0); // half fade-out
        assert!(ring.opacity() < 1.0, "opacity must decrease after focus cleared");
        assert!(ring.opacity() >= 0.0, "opacity must be non-negative");

        ring.tick(FADE_DURATION_MS * 2.0); // complete fade-out
        assert_eq!(ring.opacity(), 0.0, "opacity must reach zero after full fade-out");
    }
}
