//! 8px vertical scrollbar widget with token-driven colors.
//!
//! Renders a fixed-width track with a proportional thumb that reflects the
//! current scroll position. The widget is intentionally thin and low-contrast
//! so it does not compete with terminal content; it fades to alpha=0 when
//! there is nothing to scroll (thumb fills the whole track).
//!
//! # Layout
//!
//! The caller supplies the scrollbar's bounding [`Rect`]. The widget fills
//! the entire rect with the track background and places the thumb inside it.
//!
//! # Thumb position math
//!
//! ```text
//! total_lines = history_size + visible_rows
//! thumb_ratio  = visible_rows / total_lines          (0..1)
//! thumb_h      = max(MIN_THUMB_PX, track_h * thumb_ratio)
//! scroll_frac  = display_offset / history_size       (0 = bottom, 1 = top)
//! max_travel   = track_h - thumb_h
//! thumb_y      = track_y + max_travel * (1 - scroll_frac)
//! ```
//!
//! When `history_size == 0` or `thumb_h >= track_h` the thumb is hidden and
//! no quads are emitted.

use crate::layout::Rect;
use crate::tokens::ColorRoles;
use phantom_renderer::quads::QuadInstance;

use super::{TextSegment, Widget};

// ---------------------------------------------------------------------------
// Geometry constants
// ---------------------------------------------------------------------------

/// Fixed width of the scrollbar in pixels.
pub const SCROLLBAR_WIDTH: f32 = 8.0;

/// Minimum thumb height in pixels so it remains click-able on long histories.
const MIN_THUMB_PX: f32 = 20.0;

// ---------------------------------------------------------------------------
// Token-driven colors
// ---------------------------------------------------------------------------

/// Scrollbar track: nearly invisible — only shows up to indicate "there is a
/// bar here". Users discover it by seeing the thumb.
const TRACK_ALPHA: f32 = 0.08;

/// Scrollbar thumb: dim but clearly visible against the track.
const THUMB_ALPHA: f32 = 0.45;

/// Thumb when hovered (not yet wired to winit hover; kept for future use).
#[allow(dead_code)]
const THUMB_HOVER_ALPHA: f32 = 0.70;

// ---------------------------------------------------------------------------
// ScrollState — the data the widget needs
// ---------------------------------------------------------------------------

/// The minimum scroll state required to render the bar.
///
/// All values are in *line* units (matching `display_offset` / `history_size`
/// from alacritty_terminal and the agent pane line counter).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollState {
    /// Lines of history above the visible viewport (0 = scrolled to bottom).
    display_offset: usize,
    /// Total scrollback lines available above the viewport.
    history_size: usize,
    /// Number of lines currently visible in the pane.
    visible_rows: usize,
}

impl ScrollState {
    /// Create a scroll state.
    pub fn new(display_offset: usize, history_size: usize, visible_rows: usize) -> Self {
        Self { display_offset, history_size, visible_rows }
    }

    /// Lines of history above the visible viewport (0 = scrolled to bottom).
    pub fn display_offset(&self) -> usize {
        self.display_offset
    }

    /// Total scrollback lines available above the viewport.
    pub fn history_size(&self) -> usize {
        self.history_size
    }

    /// Number of lines currently visible in the pane.
    pub fn visible_rows(&self) -> usize {
        self.visible_rows
    }

    /// Whether the scrollbar has anything to show.
    ///
    /// Returns `false` when the content fits entirely on screen (no history)
    /// or when the values are all zero.
    pub fn is_scrollable(&self) -> bool {
        self.history_size > 0 && self.visible_rows > 0
    }

    /// Thumb height as a fraction of the track (clamped to `[0, 1]`).
    pub fn thumb_ratio(&self) -> f32 {
        let total = self.history_size + self.visible_rows;
        if total == 0 {
            return 1.0;
        }
        (self.visible_rows as f32 / total as f32).clamp(0.0, 1.0)
    }

    /// Scroll fraction: 0.0 = scrolled to bottom, 1.0 = scrolled to top.
    pub fn scroll_fraction(&self) -> f32 {
        if self.history_size == 0 {
            return 0.0;
        }
        (self.display_offset as f32 / self.history_size as f32).clamp(0.0, 1.0)
    }

    /// Compute the thumb rectangle within the given track rect.
    ///
    /// Returns `None` when there is nothing to scroll or the thumb fills the
    /// entire track (content fits on screen).
    #[must_use]
    pub fn thumb_rect(&self, track: Rect) -> Option<Rect> {
        if !self.is_scrollable() {
            return None;
        }
        let thumb_h = (track.height * self.thumb_ratio()).max(MIN_THUMB_PX);
        if thumb_h >= track.height {
            return None;
        }
        let max_travel = track.height - thumb_h;
        let thumb_y = track.y + max_travel * (1.0 - self.scroll_fraction());
        Some(Rect { x: track.x, y: thumb_y, width: track.width, height: thumb_h })
    }
}

// ---------------------------------------------------------------------------
// Scrollbar widget
// ---------------------------------------------------------------------------

/// Vertical scrollbar widget.
///
/// Stateless: all scroll state is provided via [`ScrollState`] at render time.
/// Colors are driven by the `ColorRoles` token table so every theme can
/// override track / thumb appearance without touching widget code.
pub struct Scrollbar {
    colors: ColorRoles,
    state: ScrollState,
}

impl Scrollbar {
    /// Create a scrollbar with Phosphor-default colors.
    pub fn new(state: ScrollState) -> Self {
        Self {
            colors: ColorRoles::phosphor(),
            state,
        }
    }

    /// Create a scrollbar with explicit color roles (e.g. from the active theme).
    pub fn with_colors(state: ScrollState, colors: ColorRoles) -> Self {
        Self { colors, state }
    }

    /// Update scroll state (e.g. after a new frame's output arrives).
    pub fn set_state(&mut self, state: ScrollState) {
        self.state = state;
    }

    /// The current scroll state.
    pub fn state(&self) -> ScrollState {
        self.state
    }
}

impl Widget for Scrollbar {
    fn render_quads(&self, rect: &Rect) -> Vec<QuadInstance> {
        if !self.state.is_scrollable() {
            return Vec::new();
        }

        let mut quads = Vec::with_capacity(2);

        // Track background — nearly invisible.
        let mut track_color = self.colors.surface_recessed;
        track_color[3] = TRACK_ALPHA;
        quads.push(QuadInstance {
            pos: [rect.x, rect.y],
            size: [rect.width, rect.height],
            color: track_color,
            border_radius: rect.width * 0.5, // pill shape
        });

        // Thumb — visible indicator of current position.
        if let Some(thumb) = self.state.thumb_rect(*rect) {
            let mut thumb_color = self.colors.chrome_frame;
            thumb_color[3] = THUMB_ALPHA;
            quads.push(QuadInstance {
                pos: [thumb.x, thumb.y],
                size: [thumb.width, thumb.height],
                color: thumb_color,
                border_radius: thumb.width * 0.5,
            });
        }

        quads
    }

    fn render_text(&self, _rect: &Rect) -> Vec<TextSegment> {
        // Scrollbar is purely graphical — no text.
        Vec::new()
    }
}

// ---------------------------------------------------------------------------
// Geometry helpers (public for mouse.rs / render.rs)
// ---------------------------------------------------------------------------

/// Convert a Y pixel coordinate inside the track to a `display_offset`.
///
/// Used for click-to-jump: clicking at `click_y` in the scrollbar track
/// should scroll to the proportional position in history.
///
/// - `click_y == track.y`               → offset = `history_size` (top of history)
/// - `click_y == track.y + track.height` → offset = 0 (live / bottom)
#[must_use]
pub fn track_y_to_offset(track: Rect, click_y: f32, history_size: usize) -> usize {
    if track.height <= 0.0 || history_size == 0 {
        return 0;
    }
    let frac = ((click_y - track.y) / track.height).clamp(0.0, 1.0);
    let offset = ((1.0 - frac) * history_size as f32).round() as usize;
    offset.min(history_size)
}

// ---------------------------------------------------------------------------
// Tests (TDD — written before the impl was finalized)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- Helpers --

    fn track() -> Rect {
        Rect { x: 392.0, y: 50.0, width: 8.0, height: 300.0 }
    }

    fn state(offset: usize, history: usize, visible: usize) -> ScrollState {
        ScrollState::new(offset, history, visible)
    }

    // -----------------------------------------------------------------------
    // ScrollState::thumb_ratio
    // -----------------------------------------------------------------------

    #[test]
    fn thumb_ratio_full_page_no_history() {
        let s = state(0, 0, 24);
        // No history → effectively full page → ratio = 1
        assert!((s.thumb_ratio() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn thumb_ratio_half_page() {
        // 24 visible / 48 total = 0.5
        let s = state(0, 24, 24);
        assert!((s.thumb_ratio() - 0.5).abs() < 1e-6);
    }

    #[test]
    fn thumb_ratio_tiny_viewport() {
        // 1 visible / 101 total ≈ 0.0099
        let s = state(0, 100, 1);
        let r = s.thumb_ratio();
        assert!(r > 0.0 && r < 0.02);
    }

    // -----------------------------------------------------------------------
    // ScrollState::scroll_fraction
    // -----------------------------------------------------------------------

    #[test]
    fn scroll_fraction_at_bottom() {
        let s = state(0, 500, 24);
        assert!((s.scroll_fraction() - 0.0).abs() < 1e-6);
    }

    #[test]
    fn scroll_fraction_at_top() {
        let s = state(500, 500, 24);
        assert!((s.scroll_fraction() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn scroll_fraction_midway() {
        let s = state(250, 500, 24);
        assert!((s.scroll_fraction() - 0.5).abs() < 1e-6);
    }

    // -----------------------------------------------------------------------
    // ScrollState::thumb_rect — position math
    // -----------------------------------------------------------------------

    #[test]
    fn thumb_none_when_no_history() {
        let s = state(0, 0, 24);
        assert!(s.thumb_rect(track()).is_none());
    }

    #[test]
    fn thumb_at_bottom_when_offset_zero() {
        let t = track();
        let s = state(0, 1000, 24);
        let thumb = s.thumb_rect(t).expect("should have thumb");
        let expected_bottom = t.y + t.height;
        let actual_bottom = thumb.y + thumb.height;
        assert!(
            (actual_bottom - expected_bottom).abs() < 1.0,
            "thumb bottom {actual_bottom} should be near track bottom {expected_bottom}",
        );
    }

    #[test]
    fn thumb_at_top_when_fully_scrolled() {
        let t = track();
        let s = state(1000, 1000, 24);
        let thumb = s.thumb_rect(t).expect("should have thumb");
        assert!(
            (thumb.y - t.y).abs() < 1.0,
            "thumb top {} should be at track top {}",
            thumb.y,
            t.y,
        );
    }

    #[test]
    fn thumb_midpoint_when_half_scrolled() {
        let t = track();
        let s = state(500, 1000, 24);
        let thumb = s.thumb_rect(t).expect("should have thumb");
        // At 0.5 scroll_fraction: thumb_y = track_y + max_travel * 0.5
        let thumb_ratio = s.thumb_ratio();
        let thumb_h = (t.height * thumb_ratio).max(MIN_THUMB_PX);
        let max_travel = t.height - thumb_h;
        let expected_y = t.y + max_travel * 0.5;
        assert!(
            (thumb.y - expected_y).abs() < 2.0,
            "thumb y {:.1} should be near {expected_y:.1}",
            thumb.y,
        );
    }

    #[test]
    fn thumb_width_matches_track_width() {
        let t = track();
        let s = state(0, 100, 24);
        let thumb = s.thumb_rect(t).expect("should have thumb");
        assert!((thumb.width - t.width).abs() < 0.01);
    }

    #[test]
    fn thumb_respects_min_height() {
        // Very long history → ratio tiny → thumb must still be MIN_THUMB_PX
        let t = track();
        let s = state(0, 1_000_000, 1);
        let thumb = s.thumb_rect(t).expect("should have thumb");
        assert!(
            thumb.height >= MIN_THUMB_PX - 0.01,
            "thumb height {} must be >= MIN_THUMB_PX {}",
            thumb.height,
            MIN_THUMB_PX,
        );
    }

    // -----------------------------------------------------------------------
    // track_y_to_offset — click-to-jump math
    // -----------------------------------------------------------------------

    #[test]
    fn click_top_gives_max_offset() {
        let t = track();
        let offset = track_y_to_offset(t, t.y, 500);
        assert_eq!(offset, 500);
    }

    #[test]
    fn click_bottom_gives_zero_offset() {
        let t = track();
        let offset = track_y_to_offset(t, t.y + t.height, 500);
        assert_eq!(offset, 0);
    }

    #[test]
    fn click_middle_gives_half_offset() {
        let t = track();
        let offset = track_y_to_offset(t, t.y + t.height / 2.0, 500);
        // Expected: round(0.5 * 500) = 250, direction is (1-frac) so:
        // frac = 0.5 → (1-0.5)*500 = 250
        assert_eq!(offset, 250);
    }

    #[test]
    fn click_above_track_clamps_to_max() {
        let t = track();
        let offset = track_y_to_offset(t, t.y - 100.0, 500);
        assert_eq!(offset, 500);
    }

    #[test]
    fn click_below_track_clamps_to_zero() {
        let t = track();
        let offset = track_y_to_offset(t, t.y + t.height + 100.0, 500);
        assert_eq!(offset, 0);
    }

    #[test]
    fn click_zero_history_always_zero() {
        let t = track();
        let offset = track_y_to_offset(t, t.y, 0);
        assert_eq!(offset, 0);
    }

    #[test]
    fn click_zero_height_always_zero() {
        let t = Rect { x: 0.0, y: 0.0, width: 8.0, height: 0.0 };
        let offset = track_y_to_offset(t, 50.0, 500);
        assert_eq!(offset, 0);
    }

    // -----------------------------------------------------------------------
    // Scrollbar widget rendering
    // -----------------------------------------------------------------------

    #[test]
    fn no_quads_when_not_scrollable() {
        let bar = Scrollbar::new(state(0, 0, 24));
        let quads = bar.render_quads(&track());
        assert!(quads.is_empty(), "no quads when nothing to scroll");
    }

    #[test]
    fn two_quads_when_scrollable_with_offset() {
        // Enough history and offset that a thumb appears.
        let bar = Scrollbar::new(state(100, 500, 24));
        let quads = bar.render_quads(&track());
        assert_eq!(
            quads.len(),
            2,
            "should emit track + thumb quads; got {}",
            quads.len(),
        );
    }

    #[test]
    fn track_quad_covers_full_rect() {
        let t = track();
        let bar = Scrollbar::new(state(0, 100, 24));
        let quads = bar.render_quads(&t);
        assert!(!quads.is_empty());
        let track_quad = &quads[0];
        assert!((track_quad.pos[0] - t.x).abs() < 0.01, "track x");
        assert!((track_quad.pos[1] - t.y).abs() < 0.01, "track y");
        assert!((track_quad.size[0] - t.width).abs() < 0.01, "track w");
        assert!((track_quad.size[1] - t.height).abs() < 0.01, "track h");
    }

    #[test]
    fn render_text_always_empty() {
        let bar = Scrollbar::new(state(100, 500, 24));
        assert!(bar.render_text(&track()).is_empty());
    }

    #[test]
    fn track_alpha_is_low() {
        let bar = Scrollbar::new(state(0, 100, 24));
        let quads = bar.render_quads(&track());
        let track_quad = &quads[0];
        assert!(
            track_quad.color[3] < 0.2,
            "track alpha {} should be nearly invisible",
            track_quad.color[3],
        );
    }

    #[test]
    fn thumb_alpha_is_visible() {
        let bar = Scrollbar::new(state(50, 500, 24));
        let quads = bar.render_quads(&track());
        assert_eq!(quads.len(), 2);
        let thumb_quad = &quads[1];
        assert!(
            thumb_quad.color[3] >= 0.3,
            "thumb alpha {} should be visible",
            thumb_quad.color[3],
        );
    }

    #[test]
    fn scrollbar_is_widget_object_safe() {
        let bar = Scrollbar::new(state(0, 0, 0));
        let widget: &dyn Widget = &bar;
        let _quads = widget.render_quads(&track());
    }

    // -----------------------------------------------------------------------
    // ScrollState helpers
    // -----------------------------------------------------------------------

    #[test]
    fn scroll_state_is_scrollable() {
        assert!(!state(0, 0, 24).is_scrollable());
        assert!(!state(0, 10, 0).is_scrollable());
        assert!(state(0, 10, 24).is_scrollable());
    }

    #[test]
    fn scroll_state_set_state() {
        let mut bar = Scrollbar::new(state(0, 0, 0));
        bar.set_state(state(10, 100, 24));
        assert!(bar.state().is_scrollable());
    }
}
