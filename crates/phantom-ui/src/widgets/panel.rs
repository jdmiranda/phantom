//! Generic bordered panel widget with an optional title bar.
//!
//! [`Panel`] is the foundational container chrome for Phantom's UI overlays,
//! agent panes, and inspector drawers. It renders a framed surface with:
//!
//! - A 1-sided border (all four edges) using `chrome_frame` tokens.
//! - An optional 18px title bar — token-driven color (`surface_raised` bg,
//!   `text_primary` label). When `title` is `None` the header is omitted
//!   and the full rect is body area.
//! - A body region clipped to the content area so children cannot draw
//!   outside the panel bounds.
//!
//! All colors and widths come from [`crate::tokens::Tokens::phosphor`]; no raw
//! RGBA literals appear in this file. Swap the theme and the panel recolors.
//!
//! # Layout anatomy
//!
//! ```text
//! ┌──────────────────────────────┐ ← chrome_frame border (frame() thick)
//! │ Title text                   │ ← title bar, TITLE_BAR_HEIGHT px
//! ├──────────────────────────────┤ ← divider, hair() thick
//! │                              │
//! │  body (clipped)              │
//! │                              │
//! └──────────────────────────────┘
//! ```
//!
//! # Usage
//!
//! ```rust,ignore
//! use phantom_ui::widgets::panel::Panel;
//! use phantom_ui::layout::Rect;
//!
//! let panel = Panel::new(Some("Agent Output".to_owned()));
//! let quads = panel.render_quads(&rect);
//! let texts = panel.render_text(&rect);
//! ```

use crate::layout::Rect;
use crate::render_ctx::RenderCtx;
use crate::tokens::Tokens;
use crate::widgets::{TextSegment, Widget};
use phantom_renderer::quads::QuadInstance;

/// Height of the title bar region in pixels.
///
/// Token-driven typography for panels: 18px fits a single monospace line
/// with 1px top + 1px bottom internal padding at the default 16px cell height.
pub const TITLE_BAR_HEIGHT: f32 = 18.0;

/// A bordered container widget with an optional title bar.
///
/// Construct via [`Panel::new`] or [`Panel::untitled`]. Call
/// [`Widget::render_quads`] and [`Widget::render_text`] to obtain the
/// rendering primitives the compositor should draw.
///
/// All color and sizing decisions flow through [`Tokens::phosphor`]; the
/// panel adapts automatically when `RenderCtx` changes font metrics.
#[derive(Debug, Clone)]
pub struct Panel {
    /// Optional title displayed in the header bar. `None` → no title bar.
    title: Option<String>,
    /// Live render context for text measurement and spacing.
    ctx: RenderCtx,
}

impl Panel {
    /// Create a panel, optionally with a title bar.
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// let with_title = Panel::new(Some("Inspector".to_owned()));
    /// let bare       = Panel::new(None);
    /// ```
    pub fn new(title: Option<String>) -> Self {
        Self {
            title,
            ctx: RenderCtx::fallback(),
        }
    }

    /// Convenience constructor for a panel without a title bar.
    pub fn untitled() -> Self {
        Self::new(None)
    }

    /// Update the live render context (typically called once per frame before
    /// `render_quads` / `render_text`).
    pub fn set_render_ctx(&mut self, ctx: RenderCtx) {
        self.ctx = ctx;
    }

    /// Update the title. `None` removes the title bar entirely.
    pub fn set_title(&mut self, title: Option<String>) {
        self.title = title;
    }

    /// `true` when a title bar is present.
    pub fn has_title(&self) -> bool {
        self.title.is_some()
    }

    /// Height (px) consumed by the title bar. Returns `0.0` when untitled.
    pub fn title_bar_height(&self) -> f32 {
        if self.has_title() {
            TITLE_BAR_HEIGHT
        } else {
            0.0
        }
    }

    /// The clipped body rectangle — the area inside the border and below the
    /// (optional) title bar that is safe for child content.
    ///
    /// Returns [`Rect::ZERO`] for degenerate rects where the borders consume
    /// all available space.
    pub fn body_rect(&self, outer: &Rect) -> Rect {
        let t = Tokens::phosphor(self.ctx);
        let border = t.frame(); // 2.0 px
        let divider = if self.has_title() { t.hair() } else { 0.0 };
        let header_h = self.title_bar_height() + divider;

        let x = outer.x + border;
        let y = outer.y + border + header_h;
        let width = (outer.width - border * 2.0).max(0.0);
        let height = (outer.height - border * 2.0 - header_h).max(0.0);

        Rect {
            x,
            y,
            width,
            height,
        }
    }

    // ── Private helpers ──────────────────────────────────────────────────────

    /// Resolve the title-bar background rect (positioned inside the outer border).
    fn title_bar_rect(&self, outer: &Rect) -> Rect {
        let t = Tokens::phosphor(self.ctx);
        let border = t.frame();
        Rect {
            x: outer.x + border,
            y: outer.y + border,
            width: (outer.width - border * 2.0).max(0.0),
            height: TITLE_BAR_HEIGHT,
        }
    }

    /// Resolve the horizontal divider rect (sits between title bar and body).
    fn divider_rect(&self, outer: &Rect) -> Rect {
        let t = Tokens::phosphor(self.ctx);
        let border = t.frame();
        let div_h = t.hair();
        Rect {
            x: outer.x + border,
            y: outer.y + border + TITLE_BAR_HEIGHT,
            width: (outer.width - border * 2.0).max(0.0),
            height: div_h,
        }
    }
}

impl Default for Panel {
    fn default() -> Self {
        Self::untitled()
    }
}

impl Widget for Panel {
    /// Emit quads for:
    /// 1. Full outer background (`surface_raised`) — the visible "card" face.
    /// 2. Four border quads (`chrome_frame`) at `frame()` thickness.
    /// 3. Title bar background (`surface_recessed`) when a title is present.
    /// 4. Divider line (`chrome_divider`) separating title from body.
    fn render_quads(&self, rect: &Rect) -> Vec<QuadInstance> {
        let t = Tokens::phosphor(self.ctx);
        let border = t.frame();

        // Capacity: 1 bg + 4 borders + 2 optional (title bg + divider).
        let cap = if self.has_title() { 7 } else { 5 };
        let mut quads = Vec::with_capacity(cap);

        // 1. Panel surface background.
        quads.push(QuadInstance {
            pos: [rect.x, rect.y],
            size: [rect.width, rect.height],
            color: t.colors.surface_raised,
            border_radius: 0.0,
        });

        // 2. Four border edges (frame color).
        // Top
        quads.push(QuadInstance {
            pos: [rect.x, rect.y],
            size: [rect.width, border],
            color: t.colors.chrome_frame,
            border_radius: 0.0,
        });
        // Bottom
        quads.push(QuadInstance {
            pos: [rect.x, rect.y + rect.height - border],
            size: [rect.width, border],
            color: t.colors.chrome_frame,
            border_radius: 0.0,
        });
        // Left
        quads.push(QuadInstance {
            pos: [rect.x, rect.y],
            size: [border, rect.height],
            color: t.colors.chrome_frame,
            border_radius: 0.0,
        });
        // Right
        quads.push(QuadInstance {
            pos: [rect.x + rect.width - border, rect.y],
            size: [border, rect.height],
            color: t.colors.chrome_frame,
            border_radius: 0.0,
        });

        if self.has_title() {
            // 3. Title bar background.
            let tb = self.title_bar_rect(rect);
            quads.push(QuadInstance {
                pos: [tb.x, tb.y],
                size: [tb.width, tb.height],
                color: t.colors.surface_recessed,
                border_radius: 0.0,
            });

            // 4. Divider line.
            let dv = self.divider_rect(rect);
            quads.push(QuadInstance {
                pos: [dv.x, dv.y],
                size: [dv.width, dv.height],
                color: t.colors.chrome_divider,
                border_radius: 0.0,
            });
        }

        quads
    }

    /// Emit one [`TextSegment`] for the title when present.
    ///
    /// The title is left-aligned with `space_3()` horizontal padding and
    /// vertically centered in the title bar, matching the [`NotificationBanner`]
    /// text-placement idiom.
    fn render_text(&self, rect: &Rect) -> Vec<TextSegment> {
        let Some(ref title) = self.title else {
            return Vec::new();
        };

        let t = Tokens::phosphor(self.ctx);
        let tb = self.title_bar_rect(rect);
        let pad_x = t.space_3();
        // Vertically center: top-of-glyph ≈ midline − half cell height.
        let text_y = tb.y + (tb.height * 0.5) - (self.ctx.cell_h() * 0.5);

        vec![TextSegment {
            text: title.clone(),
            x: tb.x + pad_x,
            y: text_y,
            color: t.colors.text_primary,
        }]
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Standard test rect — 400×300 at (10, 20) so we can verify absolute
    /// positions rather than asserting == 0 for everything.
    fn panel_rect() -> Rect {
        Rect {
            x: 10.0,
            y: 20.0,
            width: 400.0,
            height: 300.0,
        }
    }

    fn tokens() -> Tokens {
        Tokens::phosphor(RenderCtx::fallback())
    }

    // ── Without title bar ─────────────────────────────────────────────────

    #[test]
    fn untitled_renders_five_quads() {
        // Arrange
        let panel = Panel::untitled();
        // Act
        let quads = panel.render_quads(&panel_rect());
        // Assert: 1 surface + 4 border edges
        assert_eq!(quads.len(), 5, "untitled panel must emit exactly 5 quads");
    }

    #[test]
    fn untitled_renders_no_text() {
        let panel = Panel::untitled();
        assert!(
            panel.render_text(&panel_rect()).is_empty(),
            "untitled panel must not emit any text segments"
        );
    }

    #[test]
    fn untitled_has_no_title_bar() {
        let panel = Panel::untitled();
        assert!(!panel.has_title());
        assert_eq!(panel.title_bar_height(), 0.0);
    }

    #[test]
    fn untitled_body_rect_inset_by_border_only() {
        let panel = Panel::untitled();
        let outer = panel_rect();
        let body = panel.body_rect(&outer);
        let t = tokens();
        let border = t.frame();

        // Body starts immediately after the border on all sides.
        assert!((body.x - (outer.x + border)).abs() < 0.01, "body.x wrong");
        assert!((body.y - (outer.y + border)).abs() < 0.01, "body.y wrong");
        assert!(
            (body.width - (outer.width - border * 2.0)).abs() < 0.01,
            "body.width wrong"
        );
        assert!(
            (body.height - (outer.height - border * 2.0)).abs() < 0.01,
            "body.height wrong"
        );
    }

    // ── With title bar ────────────────────────────────────────────────────

    #[test]
    fn titled_renders_seven_quads() {
        // Arrange
        let panel = Panel::new(Some("Inspector".to_owned()));
        // Act
        let quads = panel.render_quads(&panel_rect());
        // Assert: 1 surface + 4 borders + 1 title bg + 1 divider
        assert_eq!(quads.len(), 7, "titled panel must emit exactly 7 quads");
    }

    #[test]
    fn titled_renders_one_text_segment() {
        let panel = Panel::new(Some("Agent Log".to_owned()));
        let texts = panel.render_text(&panel_rect());
        assert_eq!(
            texts.len(),
            1,
            "titled panel must emit exactly one text segment"
        );
        assert_eq!(texts[0].text, "Agent Log");
    }

    #[test]
    fn titled_has_title_bar_flag() {
        let panel = Panel::new(Some("X".to_owned()));
        assert!(panel.has_title());
        assert_eq!(panel.title_bar_height(), TITLE_BAR_HEIGHT);
    }

    #[test]
    fn titled_body_rect_inset_by_border_and_title_bar() {
        let panel = Panel::new(Some("Title".to_owned()));
        let outer = panel_rect();
        let body = panel.body_rect(&outer);
        let t = tokens();
        let border = t.frame();
        let divider = t.hair();
        let expected_y = outer.y + border + TITLE_BAR_HEIGHT + divider;
        let expected_h = outer.height - border * 2.0 - TITLE_BAR_HEIGHT - divider;

        assert!((body.x - (outer.x + border)).abs() < 0.01, "body.x wrong");
        assert!(
            (body.y - expected_y).abs() < 0.01,
            "body.y wrong: got {}, expected {}",
            body.y,
            expected_y
        );
        assert!(
            (body.width - (outer.width - border * 2.0)).abs() < 0.01,
            "body.width wrong"
        );
        assert!((body.height - expected_h).abs() < 0.01, "body.height wrong");
    }

    // ── Token compliance ──────────────────────────────────────────────────

    #[test]
    fn surface_background_uses_token_color() {
        let t = tokens();
        let panel = Panel::untitled();
        let quads = panel.render_quads(&panel_rect());
        // First quad is the panel surface.
        assert_eq!(
            quads[0].color, t.colors.surface_raised,
            "panel background must use surface_raised token"
        );
    }

    #[test]
    fn border_quads_use_chrome_frame_token() {
        let t = tokens();
        let panel = Panel::untitled();
        let quads = panel.render_quads(&panel_rect());
        // Quads 1-4 are the four border edges.
        for quad in &quads[1..5] {
            assert_eq!(
                quad.color, t.colors.chrome_frame,
                "border quad must use chrome_frame token, got {:?}",
                quad.color
            );
        }
    }

    #[test]
    fn title_bar_background_uses_surface_recessed_token() {
        let t = tokens();
        let panel = Panel::new(Some("Panel".to_owned()));
        let quads = panel.render_quads(&panel_rect());
        // Quad 5 (index 5) is the title bar background.
        assert_eq!(
            quads[5].color, t.colors.surface_recessed,
            "title bar must use surface_recessed token"
        );
    }

    #[test]
    fn divider_uses_chrome_divider_token() {
        let t = tokens();
        let panel = Panel::new(Some("Panel".to_owned()));
        let quads = panel.render_quads(&panel_rect());
        // Quad 6 (index 6) is the divider.
        assert_eq!(
            quads[6].color, t.colors.chrome_divider,
            "divider must use chrome_divider token"
        );
    }

    #[test]
    fn title_text_uses_text_primary_token() {
        let t = tokens();
        let panel = Panel::new(Some("My Panel".to_owned()));
        let texts = panel.render_text(&panel_rect());
        assert_eq!(
            texts[0].color, t.colors.text_primary,
            "title text must use text_primary token"
        );
    }

    // ── Border geometry ───────────────────────────────────────────────────

    #[test]
    fn border_thickness_equals_frame_token() {
        let t = tokens();
        let border = t.frame();
        let panel = Panel::untitled();
        let outer = panel_rect();
        let quads = panel.render_quads(&outer);

        // Top border: size[1] == border thickness
        assert!(
            (quads[1].size[1] - border).abs() < 0.01,
            "top border thickness mismatch"
        );
        // Bottom border: size[1] == border thickness
        assert!(
            (quads[2].size[1] - border).abs() < 0.01,
            "bottom border thickness mismatch"
        );
        // Left border: size[0] == border thickness
        assert!(
            (quads[3].size[0] - border).abs() < 0.01,
            "left border thickness mismatch"
        );
        // Right border: size[0] == border thickness
        assert!(
            (quads[4].size[0] - border).abs() < 0.01,
            "right border thickness mismatch"
        );
    }

    #[test]
    fn top_border_positioned_at_outer_top() {
        let panel = Panel::untitled();
        let outer = panel_rect();
        let quads = panel.render_quads(&outer);
        let top_border = &quads[1];
        assert!(
            (top_border.pos[0] - outer.x).abs() < 0.01,
            "top border x wrong"
        );
        assert!(
            (top_border.pos[1] - outer.y).abs() < 0.01,
            "top border y wrong"
        );
        assert!(
            (top_border.size[0] - outer.width).abs() < 0.01,
            "top border width wrong"
        );
    }

    #[test]
    fn bottom_border_positioned_at_outer_bottom() {
        let t = tokens();
        let panel = Panel::untitled();
        let outer = panel_rect();
        let quads = panel.render_quads(&outer);
        let bottom_border = &quads[2];
        let expected_y = outer.y + outer.height - t.frame();
        assert!(
            (bottom_border.pos[1] - expected_y).abs() < 0.01,
            "bottom border y wrong"
        );
    }

    // ── Body is inside border ─────────────────────────────────────────────

    #[test]
    fn body_rect_is_strictly_inside_outer_rect_untitled() {
        let panel = Panel::untitled();
        let outer = panel_rect();
        let body = panel.body_rect(&outer);
        assert!(body.x > outer.x, "body.x must be inside left border");
        assert!(body.y > outer.y, "body.y must be inside top border");
        assert!(
            body.x + body.width < outer.x + outer.width,
            "body right must be inside right border"
        );
        assert!(
            body.y + body.height < outer.y + outer.height,
            "body bottom must be inside bottom border"
        );
    }

    #[test]
    fn body_rect_is_strictly_inside_outer_rect_titled() {
        let panel = Panel::new(Some("T".to_owned()));
        let outer = panel_rect();
        let body = panel.body_rect(&outer);
        assert!(body.x > outer.x, "body.x must be inside left border");
        assert!(
            body.y > outer.y + TITLE_BAR_HEIGHT,
            "body.y must be below title bar"
        );
        assert!(
            body.x + body.width < outer.x + outer.width,
            "body right inside border"
        );
        assert!(
            body.y + body.height < outer.y + outer.height,
            "body bottom inside border"
        );
    }

    // ── Dynamic mutation ──────────────────────────────────────────────────

    #[test]
    fn set_title_changes_render_output() {
        let mut panel = Panel::untitled();
        assert_eq!(panel.render_quads(&panel_rect()).len(), 5);
        assert!(panel.render_text(&panel_rect()).is_empty());

        panel.set_title(Some("Now has title".to_owned()));
        assert_eq!(panel.render_quads(&panel_rect()).len(), 7);
        assert_eq!(panel.render_text(&panel_rect()).len(), 1);
    }

    #[test]
    fn removing_title_collapses_to_untitled_output() {
        let mut panel = Panel::new(Some("Remove me".to_owned()));
        panel.set_title(None);
        assert_eq!(panel.render_quads(&panel_rect()).len(), 5);
        assert!(panel.render_text(&panel_rect()).is_empty());
    }

    #[test]
    fn default_is_untitled() {
        let panel = Panel::default();
        assert!(!panel.has_title());
        assert_eq!(panel.render_quads(&panel_rect()).len(), 5);
    }

    // ── Widget trait object safety ────────────────────────────────────────

    #[test]
    fn panel_is_object_safe() {
        let panel = Panel::new(Some("Dyn".to_owned()));
        let widget: &dyn Widget = &panel;
        let quads = widget.render_quads(&panel_rect());
        assert!(!quads.is_empty());
    }

    // ── Degenerate rect ───────────────────────────────────────────────────

    #[test]
    fn degenerate_rect_body_clamped_to_zero() {
        let panel = Panel::new(Some("Small".to_owned()));
        let tiny = Rect {
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 1.0,
        };
        let body = panel.body_rect(&tiny);
        // Dimensions must never go negative.
        assert!(body.width >= 0.0, "body.width must not be negative");
        assert!(body.height >= 0.0, "body.height must not be negative");
    }
}
