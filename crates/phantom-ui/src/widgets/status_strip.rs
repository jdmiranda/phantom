//! Issue #21 — Bottom-of-pane status strip.
//!
//! A fixed-height horizontal bar divided into three slots — left, center, and
//! right — that each hold a single string. Content that overflows its slot is
//! truncated with a trailing ellipsis (`…`) so the strip never wraps or
//! expands beyond its 22 px height.
//!
//! ## Layout
//!
//! ```text
//! ┌───────────────────────────────────────────────────────────────────┐
//! │  left text           center text                     right text   │
//! └───────────────────────────────────────────────────────────────────┘
//! ```
//!
//! The available width is divided into three equal thirds (each ~33 % of the
//! total rect width). Padding of [`TOKEN_PAD`] pixels is subtracted from each
//! slot's inner budget before truncation. Slot content is positioned as
//! follows:
//! - **Left**: flush to the left edge of the left slot (+ padding).
//! - **Center**: horizontally centered within the center slot.
//! - **Right**: flush to the right edge of the right slot (- padding).
//!
//! ## Token compliance
//!
//! All colors flow through [`Tokens`] / [`ColorRoles`]:
//! - Background: `tokens.colors.surface_recessed`
//! - Foreground text: `tokens.colors.text_secondary`
//!
//! No raw RGBA constants appear outside the `#[cfg(test)]` block where they
//! are pulled from `Tokens::phosphor` for assertion purposes.

use crate::layout::Rect;
use crate::render_ctx::RenderCtx;
use crate::tokens::Tokens;
use crate::widgets::{TextSegment, Widget};
use phantom_renderer::quads::QuadInstance;

/// Fixed pixel height of the [`StatusStrip`] widget.
pub const STATUS_STRIP_HEIGHT: f32 = 22.0;

/// Horizontal padding applied at each slot edge (pixels).
///
/// Keeps text away from the slot boundaries and from the CRT barrel-distortion
/// zone at the very edges of the screen.
const TOKEN_PAD: f32 = 8.0;

// ────────────────────────────────────────────────────────────────────────────
// StatusStrip
// ────────────────────────────────────────────────────────────────────────────

/// A horizontal, fixed-height status strip positioned at the bottom of a pane.
///
/// Content is split into three independent slots — left, center, and right.
/// Each slot truncates with an ellipsis (`…`) when the text is wider than the
/// slot allows. All colors are resolved from the live [`Tokens`] table so a
/// theme change recolors the strip without touching this widget.
///
/// # Examples
///
/// ```
/// use phantom_ui::widgets::status_strip::StatusStrip;
///
/// let strip = StatusStrip::new("NORMAL", "src/main.rs", "Ln 42 Col 8");
/// ```
#[derive(Debug, Clone)]
pub struct StatusStrip {
    /// Content for the left slot.
    left: String,
    /// Content for the center slot.
    center: String,
    /// Content for the right slot.
    right: String,
    /// Render context for cell metrics (cell width drives truncation math).
    ctx: RenderCtx,
}

impl StatusStrip {
    /// Construct a [`StatusStrip`] with explicit slot content.
    ///
    /// Uses [`RenderCtx::fallback`] for metrics; call [`Self::set_render_ctx`]
    /// to provide live font metrics once available.
    pub fn new(left: impl Into<String>, center: impl Into<String>, right: impl Into<String>) -> Self {
        Self {
            left: left.into(),
            center: center.into(),
            right: right.into(),
            ctx: RenderCtx::fallback(),
        }
    }

    /// Update the render context with live font metrics.
    pub fn set_render_ctx(&mut self, ctx: RenderCtx) {
        self.ctx = ctx;
    }

    /// Replace the left slot text.
    pub fn set_left(&mut self, text: impl Into<String>) {
        self.left = text.into();
    }

    /// Replace the center slot text.
    pub fn set_center(&mut self, text: impl Into<String>) {
        self.center = text.into();
    }

    /// Replace the right slot text.
    pub fn set_right(&mut self, text: impl Into<String>) {
        self.right = text.into();
    }

    // ── Internal helpers ───────────────────────────────────────────────────

    /// Compute the pixel width of one slot given the total rect width.
    ///
    /// The strip is divided into three equal thirds.
    fn slot_width(rect_width: f32) -> f32 {
        rect_width / 3.0
    }

    /// Truncate `text` so that it fits within `max_chars` characters, appending
    /// `…` when truncation occurs.
    ///
    /// Returns an empty string when `max_chars` is 0.
    fn truncate(text: &str, max_chars: usize) -> String {
        if max_chars == 0 {
            return String::new();
        }
        let chars: Vec<char> = text.chars().collect();
        if chars.len() <= max_chars {
            text.to_owned()
        } else {
            // Reserve one char for the ellipsis.
            let keep = max_chars.saturating_sub(1);
            let truncated: String = chars[..keep].iter().collect();
            format!("{truncated}…")
        }
    }

    /// Pixel budget (in chars) available inside a slot after padding.
    fn chars_in_slot(&self, slot_w: f32) -> usize {
        let inner = (slot_w - TOKEN_PAD * 2.0).max(0.0);
        (inner / self.ctx.cell_w()) as usize
    }
}

impl Default for StatusStrip {
    fn default() -> Self {
        Self::new("", "", "")
    }
}

impl Widget for StatusStrip {
    /// Emits a single full-width background quad in `surface_recessed`.
    fn render_quads(&self, rect: &Rect) -> Vec<QuadInstance> {
        let t = Tokens::phosphor(self.ctx);
        vec![QuadInstance {
            pos: [rect.x, rect.y],
            size: [rect.width, rect.height],
            color: t.colors.surface_recessed,
            border_radius: 0.0,
        }]
    }

    /// Emits up to three text segments (one per slot), each truncated to fit.
    ///
    /// Slots with an empty string after truncation are omitted from the output
    /// so the renderer's append pattern is a no-op for blank content.
    fn render_text(&self, rect: &Rect) -> Vec<TextSegment> {
        let t = Tokens::phosphor(self.ctx);
        let fg = t.colors.text_secondary;
        let slot_w = Self::slot_width(rect.width);
        let text_y = rect.y + (rect.height * 0.5) - (self.ctx.cell_h() * 0.5);
        let char_w = self.ctx.cell_w();
        let max_chars = self.chars_in_slot(slot_w);

        let mut segments = Vec::with_capacity(3);

        // ── Left slot: flush-left ──────────────────────────────────────────
        let left_text = Self::truncate(&self.left, max_chars);
        if !left_text.is_empty() {
            segments.push(TextSegment {
                text: left_text,
                x: rect.x + TOKEN_PAD,
                y: text_y,
                color: fg,
            });
        }

        // ── Center slot: horizontally centered within its third ───────────
        let center_text = Self::truncate(&self.center, max_chars);
        if !center_text.is_empty() {
            let center_slot_x = rect.x + slot_w;
            let text_w = center_text.chars().count() as f32 * char_w;
            let text_x = center_slot_x + (slot_w - text_w) * 0.5;
            segments.push(TextSegment {
                text: center_text,
                x: text_x,
                y: text_y,
                color: fg,
            });
        }

        // ── Right slot: flush-right ────────────────────────────────────────
        let right_text = Self::truncate(&self.right, max_chars);
        if !right_text.is_empty() {
            let right_slot_x = rect.x + slot_w * 2.0;
            let text_w = right_text.chars().count() as f32 * char_w;
            let text_x = right_slot_x + slot_w - TOKEN_PAD - text_w;
            segments.push(TextSegment {
                text: right_text,
                x: text_x,
                y: text_y,
                color: fg,
            });
        }

        segments
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokens::ColorRoles;

    // ── Helpers ──────────────────────────────────────────────────────────────

    fn wide_rect() -> Rect {
        Rect { x: 0.0, y: 980.0, width: 1200.0, height: STATUS_STRIP_HEIGHT }
    }

    fn narrow_rect(width: f32) -> Rect {
        Rect { x: 0.0, y: 980.0, width, height: STATUS_STRIP_HEIGHT }
    }

    fn strip_with_ctx(left: &str, center: &str, right: &str, ctx: RenderCtx) -> StatusStrip {
        let mut s = StatusStrip::new(left, center, right);
        s.set_render_ctx(ctx);
        s
    }

    // ── Construction & defaults ───────────────────────────────────────────────

    #[test]
    fn default_has_empty_slots() {
        let s = StatusStrip::default();
        assert!(s.left.is_empty());
        assert!(s.center.is_empty());
        assert!(s.right.is_empty());
    }

    #[test]
    fn new_stores_slot_content() {
        let s = StatusStrip::new("L", "C", "R");
        assert_eq!(s.left, "L");
        assert_eq!(s.center, "C");
        assert_eq!(s.right, "R");
    }

    #[test]
    fn setters_update_slots() {
        let mut s = StatusStrip::default();
        s.set_left("left");
        s.set_center("mid");
        s.set_right("right");
        assert_eq!(s.left, "left");
        assert_eq!(s.center, "mid");
        assert_eq!(s.right, "right");
    }

    // ── Quad rendering ───────────────────────────────────────────────────────

    #[test]
    fn render_quads_emits_single_background() {
        let s = StatusStrip::new("L", "C", "R");
        let quads = s.render_quads(&wide_rect());
        assert_eq!(quads.len(), 1, "exactly one background quad");
        assert_eq!(quads[0].pos, [0.0, 980.0]);
        assert_eq!(quads[0].size, [1200.0, STATUS_STRIP_HEIGHT]);
        assert_eq!(quads[0].border_radius, 0.0);
    }

    /// Token compliance: background color must equal `surface_recessed` from
    /// the Phosphor token set — no hardcoded RGBA values allowed.
    #[test]
    fn background_color_is_surface_recessed_token() {
        let ctx = RenderCtx::fallback();
        let expected = Tokens::phosphor(ctx).colors.surface_recessed;

        let s = StatusStrip::new("X", "Y", "Z");
        let quads = s.render_quads(&wide_rect());
        assert_eq!(quads[0].color, expected, "background must use surface_recessed token");
    }

    /// Cross-check: `surface_recessed` must not equal `surface_base` (ensures
    /// we are using the correct depth token).
    #[test]
    fn surface_recessed_differs_from_surface_base() {
        let roles = ColorRoles::phosphor();
        assert_ne!(
            roles.surface_recessed, roles.surface_base,
            "surface_recessed and surface_base should differ for visual depth"
        );
    }

    // ── Text rendering — normal width ────────────────────────────────────────

    #[test]
    fn all_three_slots_render_when_content_fits() {
        let s = StatusStrip::new("LEFT", "CENTER", "RIGHT");
        let texts = s.render_text(&wide_rect());
        assert_eq!(texts.len(), 3);
        assert!(texts.iter().any(|t| t.text == "LEFT"), "left slot missing");
        assert!(texts.iter().any(|t| t.text == "CENTER"), "center slot missing");
        assert!(texts.iter().any(|t| t.text == "RIGHT"), "right slot missing");
    }

    /// Token compliance: text color must equal `text_secondary`.
    #[test]
    fn text_color_is_text_secondary_token() {
        let ctx = RenderCtx::fallback();
        let expected = Tokens::phosphor(ctx).colors.text_secondary;

        let s = strip_with_ctx("L", "C", "R", ctx);
        let texts = s.render_text(&wide_rect());
        for seg in &texts {
            assert_eq!(
                seg.color, expected,
                "segment '{}' must use text_secondary token, got {:?}",
                seg.text, seg.color
            );
        }
    }

    #[test]
    fn empty_slots_are_omitted() {
        let s = StatusStrip::new("", "CENTER", "");
        let texts = s.render_text(&wide_rect());
        assert_eq!(texts.len(), 1);
        assert_eq!(texts[0].text, "CENTER");
    }

    #[test]
    fn all_empty_slots_produce_no_text() {
        let s = StatusStrip::default();
        let texts = s.render_text(&wide_rect());
        assert!(texts.is_empty(), "all-empty strip should emit no text segments");
    }

    // ── Truncation behavior ───────────────────────────────────────────────────

    /// Core truncation unit test: long text must end with `…` and fit within
    /// `max_chars` when the text length exceeds the budget.
    #[test]
    fn truncate_long_text_ends_with_ellipsis() {
        let result = StatusStrip::truncate("abcdefghijklmnop", 8);
        assert!(result.ends_with('…'), "truncated text should end with ellipsis");
        assert!(
            result.chars().count() <= 8,
            "truncated result must fit within max_chars: got {}",
            result.chars().count()
        );
    }

    #[test]
    fn truncate_short_text_unchanged() {
        let result = StatusStrip::truncate("hello", 20);
        assert_eq!(result, "hello", "text shorter than budget must not be modified");
    }

    #[test]
    fn truncate_exact_fit_unchanged() {
        let result = StatusStrip::truncate("exact", 5);
        assert_eq!(result, "exact", "text that exactly fits must not be truncated");
    }

    #[test]
    fn truncate_zero_budget_returns_empty() {
        let result = StatusStrip::truncate("anything", 0);
        assert!(result.is_empty(), "zero budget must produce empty string");
    }

    #[test]
    fn truncate_budget_of_one_gives_ellipsis_only() {
        let result = StatusStrip::truncate("hello", 1);
        assert_eq!(result, "…", "budget=1 should give just the ellipsis");
    }

    /// Slot truncation under a 60 px wide rect with the fallback cell width of
    /// 8 px. Each slot = 20 px; inner budget = 20 - 16 (2×8 pad) = 4 px →
    /// max_chars = 0 (floor division), so even a short label is suppressed.
    #[test]
    fn very_narrow_strip_suppresses_all_text() {
        // 60 px total → 20 px per slot → inner = 4 px → 0 chars
        let ctx = RenderCtx::fallback(); // cell_w = 8.0
        let s = strip_with_ctx("ABC", "DEF", "GHI", ctx);
        let texts = s.render_text(&narrow_rect(60.0));
        assert!(
            texts.is_empty(),
            "60 px strip should produce no text at 8 px cell width"
        );
    }

    /// At 300 px total with an 8 px cell: each slot = 100 px, inner = 84 px,
    /// max_chars = 10. A 15-char string must be truncated to ≤10 chars and
    /// end with `…`.
    #[test]
    fn moderate_narrow_strip_truncates_long_labels() {
        let ctx = RenderCtx::fallback(); // cell_w = 8.0
        // slot_w = 100 px; inner = 100 - 16 = 84 px; max_chars = floor(84/8) = 10
        let long = "123456789ABCDEF"; // 15 chars — too long
        let s = strip_with_ctx(long, long, long, ctx);
        let texts = s.render_text(&narrow_rect(300.0));

        assert_eq!(texts.len(), 3, "all slots should still emit text at 300 px");
        for seg in &texts {
            let n = seg.text.chars().count();
            assert!(
                n <= 10,
                "segment '{}' has {} chars, expected ≤10",
                seg.text, n
            );
            assert!(
                seg.text.ends_with('…'),
                "truncated segment '{}' should end with ellipsis",
                seg.text
            );
        }
    }

    /// At 480 px total with an 8 px cell: each slot = 160 px, inner = 144 px,
    /// max_chars = 18. A 10-char string fits without truncation.
    #[test]
    fn medium_width_short_labels_not_truncated() {
        let ctx = RenderCtx::fallback();
        // slot_w = 160; inner = 144; max_chars = 18
        let s = strip_with_ctx("NORMAL", "file.rs", "Ln 1 Col 1", ctx);
        let texts = s.render_text(&narrow_rect(480.0));
        assert_eq!(texts.len(), 3);
        assert!(texts.iter().any(|t| t.text == "NORMAL"));
        assert!(texts.iter().any(|t| t.text == "file.rs"));
        assert!(texts.iter().any(|t| t.text == "Ln 1 Col 1"));
    }

    // ── Slot positioning ─────────────────────────────────────────────────────

    /// Left text must start at (rect.x + TOKEN_PAD).
    #[test]
    fn left_slot_x_position() {
        let ctx = RenderCtx::fallback();
        let s = strip_with_ctx("LEFT", "", "", ctx);
        let texts = s.render_text(&wide_rect());
        assert_eq!(texts.len(), 1);
        assert_eq!(texts[0].x, 0.0 + TOKEN_PAD, "left slot must start at rect.x + TOKEN_PAD");
    }

    /// Right text must end at (rect.x + rect.width - TOKEN_PAD).
    #[test]
    fn right_slot_x_position() {
        let ctx = RenderCtx::fallback();
        let s = strip_with_ctx("", "", "RIGHT", ctx);
        let texts = s.render_text(&wide_rect());
        assert_eq!(texts.len(), 1);

        let text_w = "RIGHT".chars().count() as f32 * ctx.cell_w();
        let slot_w = wide_rect().width / 3.0;
        let right_slot_x = slot_w * 2.0;
        let actual_expected = right_slot_x + slot_w - TOKEN_PAD - text_w;

        assert!(
            (texts[0].x - actual_expected).abs() < 0.01,
            "right text x={} expected={actual_expected}",
            texts[0].x
        );
        // Right edge must not exceed rect boundary minus padding.
        let right_edge = texts[0].x + text_w;
        assert!(
            right_edge <= wide_rect().width + 0.01,
            "right text bleeds past rect boundary: right_edge={right_edge}"
        );
    }

    /// Center text must be within the center slot (second third).
    #[test]
    fn center_slot_x_is_within_center_third() {
        let ctx = RenderCtx::fallback();
        let s = strip_with_ctx("", "CENTER", "", ctx);
        let texts = s.render_text(&wide_rect());
        assert_eq!(texts.len(), 1);

        let slot_w = wide_rect().width / 3.0;
        let center_slot_start = slot_w;
        let center_slot_end = slot_w * 2.0;
        let text_w = "CENTER".chars().count() as f32 * ctx.cell_w();

        assert!(
            texts[0].x >= center_slot_start,
            "center text starts before center slot: x={}",
            texts[0].x
        );
        assert!(
            texts[0].x + text_w <= center_slot_end + 0.01,
            "center text exceeds center slot: x+w={}",
            texts[0].x + text_w
        );
    }

    // ── Widget object-safety ─────────────────────────────────────────────────

    #[test]
    fn status_strip_is_object_safe() {
        let s = StatusStrip::new("A", "B", "C");
        let widget: &dyn Widget = &s;
        let quads = widget.render_quads(&wide_rect());
        assert_eq!(quads.len(), 1);
    }

    // ── Height constant ──────────────────────────────────────────────────────

    #[test]
    fn height_constant_is_22() {
        assert_eq!(STATUS_STRIP_HEIGHT, 22.0);
    }
}
