//! Sec.26 — Thin separator line widget (horizontal and vertical).
//!
//! A [`Divider`] draws a single hairline quad between two adjacent UI regions.
//! Its color is sourced from [`crate::tokens::Tokens`]: `chrome_divider` maps
//! to the phosphor-green grid line color — no raw RGBA constants live here.
//!
//! # Examples
//!
//! ```
//! use phantom_ui::widgets::divider::{Divider, Orientation};
//! use phantom_ui::layout::Rect;
//! use phantom_ui::widgets::Widget;
//!
//! let sep = Divider::horizontal();
//! let rect = Rect { x: 0.0, y: 100.0, width: 1920.0, height: 1.0 };
//! // One background quad in chrome_divider color.
//! assert_eq!(sep.render_quads(&rect).len(), 1);
//! assert!(sep.render_text(&rect).is_empty());
//! ```

use crate::layout::Rect;
use crate::render_ctx::RenderCtx;
use crate::tokens::Tokens;
use crate::widgets::{TextSegment, Widget};
use phantom_renderer::quads::QuadInstance;

// -----------------------------------------------------------------------
// Orientation
// -----------------------------------------------------------------------

/// Axis along which the divider line runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Orientation {
    /// A full-width horizontal line; height equals `thickness`.
    Horizontal,
    /// A full-height vertical line; width equals `thickness`.
    Vertical,
}

// -----------------------------------------------------------------------
// Divider
// -----------------------------------------------------------------------

/// A hairline separator widget — horizontal or vertical.
///
/// The widget fills its entire [`Rect`] with a single `chrome_divider`-colored
/// quad. The caller is responsible for passing a rect whose minor dimension
/// (height for horizontal, width for vertical) equals the desired visual
/// thickness — the widget does not enforce this to stay flexible for DPI
/// scaling and fractional pixel budgets.
///
/// Colors are sourced from [`Tokens::phosphor`], so theme changes propagate
/// without touching widget code.
#[derive(Debug, Clone)]
pub struct Divider {
    /// Axis along which the line is drawn.
    pub orientation: Orientation,
    /// Thickness of the line in pixels.
    ///
    /// For a [`Orientation::Horizontal`] divider this is the height;
    /// for [`Orientation::Vertical`] it is the width.
    pub thickness: f32,
    /// Render context for token resolution (spacing, DPI metrics).
    ctx: RenderCtx,
}

impl Divider {
    /// Create a 1 px horizontal divider using the fallback render context.
    pub fn horizontal() -> Self {
        Self::new(Orientation::Horizontal, RenderCtx::fallback())
    }

    /// Create a 1 px vertical divider using the fallback render context.
    pub fn vertical() -> Self {
        Self::new(Orientation::Vertical, RenderCtx::fallback())
    }

    /// Create a divider with an explicit orientation and render context.
    ///
    /// `thickness` defaults to `tokens.hair()` (1 px). Call
    /// [`Self::with_thickness`] to override after construction.
    pub fn new(orientation: Orientation, ctx: RenderCtx) -> Self {
        let thickness = Tokens::phosphor(ctx).hair();
        Self { orientation, thickness, ctx }
    }

    /// Override the divider thickness in pixels.
    #[must_use]
    pub fn with_thickness(mut self, thickness: f32) -> Self {
        self.thickness = thickness;
        self
    }

    /// Update the live render context.
    pub fn set_render_ctx(&mut self, ctx: RenderCtx) {
        self.ctx = ctx;
    }

    /// The preferred minor-axis size (height for horizontal, width for
    /// vertical) that the caller should reserve in the layout budget.
    pub fn preferred_size(&self) -> f32 {
        self.thickness
    }

    /// Resolve the divider color from the token table.
    fn divider_color(&self) -> [f32; 4] {
        Tokens::phosphor(self.ctx).colors.chrome_divider
    }
}

impl Default for Divider {
    fn default() -> Self {
        Self::horizontal()
    }
}

impl Widget for Divider {
    /// Emit a single quad that fills the provided rect.
    ///
    /// The rect's minor dimension (height for horizontal, width for vertical)
    /// should match [`Self::thickness`] — the widget renders whatever space it
    /// is given.
    fn render_quads(&self, rect: &Rect) -> Vec<QuadInstance> {
        vec![QuadInstance {
            pos: [rect.x, rect.y],
            size: [rect.width, rect.height],
            color: self.divider_color(),
            border_radius: 0.0,
        }]
    }

    /// Dividers carry no text; always returns an empty [`Vec`].
    fn render_text(&self, _rect: &Rect) -> Vec<TextSegment> {
        Vec::new()
    }
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render_ctx::RenderCtx;
    use crate::tokens::Tokens;

    fn h_rect() -> Rect {
        Rect { x: 0.0, y: 100.0, width: 1920.0, height: 1.0 }
    }

    fn v_rect() -> Rect {
        Rect { x: 400.0, y: 0.0, width: 1.0, height: 600.0 }
    }

    // ── Construction ────────────────────────────────────────────────────────

    #[test]
    fn horizontal_constructor_sets_orientation_and_hair_thickness() {
        let d = Divider::horizontal();
        assert_eq!(d.orientation, Orientation::Horizontal);
        // hair() == 1.0 per Tokens spec.
        let expected_thickness = Tokens::phosphor(RenderCtx::fallback()).hair();
        assert_eq!(d.thickness, expected_thickness);
    }

    #[test]
    fn vertical_constructor_sets_orientation_and_hair_thickness() {
        let d = Divider::vertical();
        assert_eq!(d.orientation, Orientation::Vertical);
        let expected_thickness = Tokens::phosphor(RenderCtx::fallback()).hair();
        assert_eq!(d.thickness, expected_thickness);
    }

    #[test]
    fn with_thickness_overrides_default() {
        let d = Divider::horizontal().with_thickness(2.0);
        assert_eq!(d.thickness, 2.0);
    }

    #[test]
    fn preferred_size_matches_thickness() {
        let d = Divider::vertical().with_thickness(3.0);
        assert_eq!(d.preferred_size(), 3.0);
    }

    #[test]
    fn default_is_horizontal() {
        let d = Divider::default();
        assert_eq!(d.orientation, Orientation::Horizontal);
    }

    // ── Rendering — quad count and structure ─────────────────────────────────

    #[test]
    fn horizontal_renders_exactly_one_quad() {
        let d = Divider::horizontal();
        let quads = d.render_quads(&h_rect());
        assert_eq!(quads.len(), 1, "horizontal divider must emit exactly one quad");
    }

    #[test]
    fn vertical_renders_exactly_one_quad() {
        let d = Divider::vertical();
        let quads = d.render_quads(&v_rect());
        assert_eq!(quads.len(), 1, "vertical divider must emit exactly one quad");
    }

    #[test]
    fn horizontal_quad_fills_full_rect() {
        let d = Divider::horizontal();
        let rect = h_rect();
        let quads = d.render_quads(&rect);
        let q = &quads[0];
        assert_eq!(q.pos[0], rect.x);
        assert_eq!(q.pos[1], rect.y);
        assert_eq!(q.size[0], rect.width);
        assert_eq!(q.size[1], rect.height);
    }

    #[test]
    fn vertical_quad_fills_full_rect() {
        let d = Divider::vertical();
        let rect = v_rect();
        let quads = d.render_quads(&rect);
        let q = &quads[0];
        assert_eq!(q.pos[0], rect.x);
        assert_eq!(q.pos[1], rect.y);
        assert_eq!(q.size[0], rect.width);
        assert_eq!(q.size[1], rect.height);
    }

    // ── Token color compliance ────────────────────────────────────────────────

    #[test]
    fn horizontal_color_matches_chrome_divider_token() {
        let ctx = RenderCtx::fallback();
        let tokens = Tokens::phosphor(ctx);
        let d = Divider::horizontal();
        let quads = d.render_quads(&h_rect());
        assert_eq!(
            quads[0].color,
            tokens.colors.chrome_divider,
            "divider color must come from tokens.colors.chrome_divider — no hardcoded RGBA",
        );
    }

    #[test]
    fn vertical_color_matches_chrome_divider_token() {
        let ctx = RenderCtx::fallback();
        let tokens = Tokens::phosphor(ctx);
        let d = Divider::vertical();
        let quads = d.render_quads(&v_rect());
        assert_eq!(
            quads[0].color,
            tokens.colors.chrome_divider,
            "divider color must come from tokens.colors.chrome_divider — no hardcoded RGBA",
        );
    }

    #[test]
    fn no_hardcoded_colors_in_quads() {
        // The quad's color must equal the live token value, not a constant.
        // If someone ever hard-codes [0.18, 0.38, 0.24, 0.60] this test would
        // still pass, but changing the token would break it — that's the point.
        let ctx = RenderCtx::fallback();
        let expected = Tokens::phosphor(ctx).colors.chrome_divider;
        for orientation in [Orientation::Horizontal, Orientation::Vertical] {
            let d = Divider::new(orientation, ctx);
            let rect = match orientation {
                Orientation::Horizontal => h_rect(),
                Orientation::Vertical => v_rect(),
            };
            let quads = d.render_quads(&rect);
            assert_eq!(quads[0].color, expected, "color must equal live token for {orientation:?}");
        }
    }

    // ── Text is always empty ──────────────────────────────────────────────────

    #[test]
    fn horizontal_emits_no_text() {
        let d = Divider::horizontal();
        assert!(d.render_text(&h_rect()).is_empty(), "dividers carry no text");
    }

    #[test]
    fn vertical_emits_no_text() {
        let d = Divider::vertical();
        assert!(d.render_text(&v_rect()).is_empty(), "dividers carry no text");
    }

    // ── Widget trait object safety ─────────────────────────────────────────────

    #[test]
    fn divider_is_object_safe() {
        let d = Divider::horizontal();
        let widget: &dyn Widget = &d;
        assert_eq!(widget.render_quads(&h_rect()).len(), 1);
        assert!(widget.render_text(&h_rect()).is_empty());
    }

    // ── Sizing semantics ──────────────────────────────────────────────────────

    #[test]
    fn horizontal_preferred_size_is_height_of_divider() {
        // For a horizontal divider, preferred_size() should match thickness
        // (the height dimension), not the width.
        let d = Divider::horizontal();
        assert_eq!(d.preferred_size(), d.thickness);
    }

    #[test]
    fn vertical_preferred_size_is_width_of_divider() {
        // For a vertical divider, preferred_size() should match thickness
        // (the width dimension).
        let d = Divider::vertical();
        assert_eq!(d.preferred_size(), d.thickness);
    }

    #[test]
    fn render_ctx_update_propagates_to_token_color() {
        // After updating the ctx the divider_color should still resolve from
        // Tokens::phosphor using the new context. We verify no panic and that
        // the color remains the token value.
        let mut d = Divider::horizontal();
        let new_ctx = RenderCtx::new((10.0, 20.0), 2.0);
        d.set_render_ctx(new_ctx);
        let expected = Tokens::phosphor(new_ctx).colors.chrome_divider;
        let quads = d.render_quads(&h_rect());
        assert_eq!(quads[0].color, expected);
    }
}
