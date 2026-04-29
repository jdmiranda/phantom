//! Issue #25 — `TabStrip` widget.
//!
//! A horizontal tab strip with:
//! - Active tab uses `text_primary` token color.
//! - Inactive tabs use `text_secondary` token color.
//! - Optional numeric badge on each tab (unread count, error count, etc.).
//! - Click selection via `on_click(x)` pixel-coordinate dispatch.
//! - Keyboard navigation: `next()` / `prev()` for Tab / Shift-Tab.
//! - Selection callback fired via [`TabStrip::on_click`] and the nav methods.
//!
//! # Layout
//!
//! ```text
//! ┌────────────────┬────────────────┬────────────────┐
//! │  Alpha         │  Beta [3]      │  Gamma         │
//! └────────────────┴────────────────┴────────────────┘
//! ```
//!
//! Each tab occupies an equal share of the strip width (minimum [`TAB_MIN_W`]
//! px). The active tab receives a background highlight (`surface_raised`) and
//! a bottom accent bar (`accent_focus`). Badges render as `[N]` appended to
//! the label in `status_warn` color.
//!
//! # Examples
//!
//! ```rust,ignore
//! use phantom_ui::widgets::tab_strip::{Tab, TabStrip};
//!
//! let mut strip = TabStrip::new(
//!     vec![
//!         Tab::new("Terminal", None),
//!         Tab::new("Agent", Some(2)),
//!     ],
//!     0,
//!     |idx| println!("selected tab {idx}"),
//! );
//! ```

use crate::layout::Rect;
use crate::render_ctx::RenderCtx;
use crate::tokens::Tokens;
use crate::widgets::{TextSegment, Widget};
use phantom_renderer::quads::QuadInstance;

// ─────────────────────────────────────────────────────────────────────────────
// Constants
// ─────────────────────────────────────────────────────────────────────────────

/// Minimum pixel width of a single tab button.
pub const TAB_MIN_W: f32 = 80.0;

/// Horizontal padding inside each tab cell.
const H_PAD: f32 = 12.0;

/// Thickness of the active-tab bottom accent bar (pixels).
const ACCENT_BAR_H: f32 = 2.0;

// ─────────────────────────────────────────────────────────────────────────────
// Tab
// ─────────────────────────────────────────────────────────────────────────────

/// A single entry in a [`TabStrip`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tab {
    /// Display label rendered inside the tab cell.
    label: String,
    /// Optional badge count (unread messages, error count, etc.).
    /// Rendered as `[N]` appended to the label in `status_warn` color.
    badge: Option<u32>,
}

impl Tab {
    /// Create a [`Tab`] with the given label and optional badge count.
    pub fn new(label: impl Into<String>, badge: Option<u32>) -> Self {
        Self { label: label.into(), badge }
    }

    /// The display label of this tab.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The optional badge count for this tab.
    pub fn badge(&self) -> Option<u32> {
        self.badge
    }

    /// Compose the rendered label string: `"Label"` or `"Label [N]"`.
    ///
    /// Used in tests to verify the full display string without going through
    /// the render pipeline.
    #[cfg(test)]
    fn display_label(&self) -> String {
        match self.badge {
            Some(n) => format!("{} [{}]", self.label, n),
            None => self.label.clone(),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// TabStrip
// ─────────────────────────────────────────────────────────────────────────────

/// Horizontal tab strip widget.
///
/// Owns the tab list and the active index. Selection changes are reported via
/// `on_select` callback. The widget is non-`Clone` because `on_select` is a
/// heap-allocated closure; clone individual [`Tab`]s if you need to snapshot
/// state.
///
/// Use [`TabStrip::on_click`] to route mouse-click pixel coordinates to tab
/// selection, and [`TabStrip::next`] / [`TabStrip::prev`] for keyboard nav.
pub struct TabStrip {
    /// Ordered list of tabs.
    tabs: Vec<Tab>,
    /// Index of the currently active tab. Always `< tabs.len()` when
    /// `!tabs.is_empty()`, and `0` when empty.
    active: usize,
    /// Called whenever the active tab changes. Receives the new index.
    on_select: Box<dyn FnMut(usize)>,
    /// Live render metrics (cell width drives truncation math).
    ctx: RenderCtx,
}

impl TabStrip {
    /// Construct a [`TabStrip`] with the given tabs, initial active index, and
    /// selection callback.
    ///
    /// `active` is clamped to `tabs.len().saturating_sub(1)` so passing an
    /// out-of-range index on an empty list is safe.
    pub fn new(tabs: Vec<Tab>, active: usize, on_select: impl FnMut(usize) + 'static) -> Self {
        let clamped = if tabs.is_empty() { 0 } else { active.min(tabs.len() - 1) };
        Self {
            tabs,
            active: clamped,
            on_select: Box::new(on_select),
            ctx: RenderCtx::fallback(),
        }
    }

    /// Update the live render context (call once per frame before rendering).
    pub fn set_render_ctx(&mut self, ctx: RenderCtx) {
        self.ctx = ctx;
    }

    /// Index of the currently active tab.
    pub fn active(&self) -> usize {
        self.active
    }

    /// Number of tabs in the strip.
    pub fn tab_count(&self) -> usize {
        self.tabs.len()
    }

    /// Route a mouse click at `click_x` pixels (relative to the strip rect
    /// origin) to the appropriate tab.
    ///
    /// Does nothing if `tabs` is empty.
    pub fn on_click(&mut self, click_x: f32, rect_width: f32) {
        if self.tabs.is_empty() {
            return;
        }
        let tab_w = self.tab_width(rect_width);
        let idx = ((click_x / tab_w) as usize).min(self.tabs.len() - 1);
        self.select(idx);
    }

    /// Advance to the next tab (wraps around). Returns the new active index.
    pub fn next(&mut self) -> usize {
        if self.tabs.is_empty() {
            return 0;
        }
        let idx = (self.active + 1) % self.tabs.len();
        self.select(idx);
        idx
    }

    /// Move to the previous tab (wraps around). Returns the new active index.
    pub fn prev(&mut self) -> usize {
        if self.tabs.is_empty() {
            return 0;
        }
        let idx = if self.active == 0 {
            self.tabs.len() - 1
        } else {
            self.active - 1
        };
        self.select(idx);
        idx
    }

    // ── Private ───────────────────────────────────────────────────────────────

    /// Set `active` to `idx` and fire the `on_select` callback.
    fn select(&mut self, idx: usize) {
        self.active = idx;
        (self.on_select)(idx);
    }

    /// Compute the pixel width of one tab cell given the total strip width.
    fn tab_width(&self, strip_width: f32) -> f32 {
        if self.tabs.is_empty() {
            return TAB_MIN_W;
        }
        (strip_width / self.tabs.len() as f32).max(TAB_MIN_W)
    }
}

impl Widget for TabStrip {
    /// Produce quads for:
    /// 1. Full-width strip background (`surface_recessed`).
    /// 2. Active-tab background highlight (`surface_raised`).
    /// 3. Active-tab bottom accent bar (`accent_focus`).
    fn render_quads(&self, rect: &Rect) -> Vec<QuadInstance> {
        let t = Tokens::phosphor(self.ctx);
        let mut quads = Vec::with_capacity(3);

        // 1. Strip background.
        quads.push(QuadInstance {
            pos: [rect.x, rect.y],
            size: [rect.width, rect.height],
            color: t.colors.surface_recessed,
            border_radius: 0.0,
        });

        if self.tabs.is_empty() {
            return quads;
        }

        let tab_w = self.tab_width(rect.width);
        let active_x = rect.x + self.active as f32 * tab_w;

        // 2. Active tab background.
        quads.push(QuadInstance {
            pos: [active_x, rect.y],
            size: [tab_w, rect.height],
            color: t.colors.surface_raised,
            border_radius: 0.0,
        });

        // 3. Active tab accent bar along the bottom edge.
        quads.push(QuadInstance {
            pos: [active_x, rect.y + rect.height - ACCENT_BAR_H],
            size: [tab_w, ACCENT_BAR_H],
            color: t.colors.accent_focus,
            border_radius: 0.0,
        });

        quads
    }

    /// Produce text segments for every tab label.
    ///
    /// Active tab labels use `text_primary`; inactive labels use
    /// `text_secondary`. When a tab carries a badge the badge suffix (`[N]`) is
    /// rendered with `status_warn` color by emitting a second segment positioned
    /// immediately after the base label.
    fn render_text(&self, rect: &Rect) -> Vec<TextSegment> {
        if self.tabs.is_empty() {
            return Vec::new();
        }

        let t = Tokens::phosphor(self.ctx);
        let tab_w = self.tab_width(rect.width);
        let text_y = rect.y + (rect.height * 0.5) - (self.ctx.cell_h() * 0.5);
        let char_w = self.ctx.cell_w();

        let mut segments = Vec::with_capacity(self.tabs.len() * 2);

        for (i, tab) in self.tabs.iter().enumerate() {
            let tab_x = rect.x + i as f32 * tab_w;
            let is_active = i == self.active;
            let label_color = if is_active {
                t.colors.text_primary
            } else {
                t.colors.text_secondary
            };

            // Available chars inside the tab cell (after padding both sides).
            let avail_px = (tab_w - H_PAD * 2.0).max(0.0);
            let max_chars = (avail_px / char_w) as usize;

            // Base label (truncated to fit).
            let label: String = tab.label.chars().take(max_chars).collect();
            if label.is_empty() {
                continue;
            }

            let label_w = label.chars().count() as f32 * char_w;

            match tab.badge {
                None => {
                    // Center the label within the tab cell.
                    let text_x = tab_x + (tab_w - label_w) * 0.5;
                    segments.push(TextSegment {
                        text: label,
                        x: text_x,
                        y: text_y,
                        color: label_color,
                    });
                }
                Some(n) => {
                    // Render label + badge as two adjacent segments.
                    let badge_text = format!(" [{}]", n);
                    let badge_w = badge_text.chars().count() as f32 * char_w;
                    let total_w = label_w + badge_w;
                    let start_x = tab_x + (tab_w - total_w).max(0.0) * 0.5;

                    segments.push(TextSegment {
                        text: label,
                        x: start_x,
                        y: text_y,
                        color: label_color,
                    });
                    segments.push(TextSegment {
                        text: badge_text,
                        x: start_x + label_w,
                        y: text_y,
                        color: t.colors.status_warn,
                    });
                }
            }
        }

        segments
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

    fn strip_rect() -> Rect {
        Rect { x: 0.0, y: 0.0, width: 960.0, height: 30.0 }
    }

    fn make_tabs(n: usize) -> Vec<Tab> {
        (0..n)
            .map(|i| Tab::new(format!("Tab{i}"), None))
            .collect()
    }

    // ── Construction ──────────────────────────────────────────────────────────

    #[test]
    fn new_stores_tabs_and_active() {
        let strip = TabStrip::new(make_tabs(3), 1, |_| {});
        assert_eq!(strip.tab_count(), 3);
        assert_eq!(strip.active(), 1);
    }

    #[test]
    fn new_clamps_active_to_last_tab() {
        let strip = TabStrip::new(make_tabs(2), 99, |_| {});
        assert_eq!(strip.active(), 1);
    }

    #[test]
    fn new_empty_tabs_active_is_zero() {
        let strip = TabStrip::new(vec![], 5, |_| {});
        assert_eq!(strip.active(), 0);
        assert_eq!(strip.tab_count(), 0);
    }

    // ── Keyboard nav ──────────────────────────────────────────────────────────

    #[test]
    fn next_advances_active() {
        let mut strip = TabStrip::new(make_tabs(3), 0, |_| {});
        assert_eq!(strip.next(), 1);
        assert_eq!(strip.active(), 1);
    }

    #[test]
    fn next_wraps_around() {
        let mut strip = TabStrip::new(make_tabs(3), 2, |_| {});
        assert_eq!(strip.next(), 0);
    }

    #[test]
    fn prev_decrements_active() {
        let mut strip = TabStrip::new(make_tabs(3), 2, |_| {});
        assert_eq!(strip.prev(), 1);
    }

    #[test]
    fn prev_wraps_around() {
        let mut strip = TabStrip::new(make_tabs(3), 0, |_| {});
        assert_eq!(strip.prev(), 2);
    }

    #[test]
    fn next_on_empty_returns_zero() {
        let mut strip = TabStrip::new(vec![], 0, |_| {});
        assert_eq!(strip.next(), 0);
    }

    // ── Selection callback ────────────────────────────────────────────────────

    #[test]
    fn selection_callback_fires_on_next() {
        let fired = std::sync::Arc::new(std::sync::Mutex::new(None::<usize>));
        let fired_clone = fired.clone();
        let mut strip = TabStrip::new(make_tabs(3), 0, move |idx| {
            *fired_clone.lock().unwrap() = Some(idx);
        });
        strip.next();
        assert_eq!(*fired.lock().unwrap(), Some(1));
    }

    #[test]
    fn selection_callback_fires_on_prev() {
        let fired = std::sync::Arc::new(std::sync::Mutex::new(None::<usize>));
        let fired_clone = fired.clone();
        let mut strip = TabStrip::new(make_tabs(3), 1, move |idx| {
            *fired_clone.lock().unwrap() = Some(idx);
        });
        strip.prev();
        assert_eq!(*fired.lock().unwrap(), Some(0));
    }

    #[test]
    fn selection_callback_fires_on_click() {
        let fired = std::sync::Arc::new(std::sync::Mutex::new(None::<usize>));
        let fired_clone = fired.clone();
        let mut strip = TabStrip::new(make_tabs(3), 0, move |idx| {
            *fired_clone.lock().unwrap() = Some(idx);
        });
        // Click in the middle third (tab index 1).
        strip.on_click(350.0, 960.0);
        assert_eq!(*fired.lock().unwrap(), Some(1));
    }

    // ── Click routing ────────────────────────────────────────────────────────

    #[test]
    fn click_on_first_tab_selects_zero() {
        let mut strip = TabStrip::new(make_tabs(3), 0, |_| {});
        strip.on_click(10.0, 960.0);
        assert_eq!(strip.active(), 0);
    }

    #[test]
    fn click_on_last_tab_selects_last() {
        let mut strip = TabStrip::new(make_tabs(3), 0, |_| {});
        strip.on_click(900.0, 960.0);
        assert_eq!(strip.active(), 2);
    }

    #[test]
    fn click_on_empty_strip_is_no_op() {
        let mut strip = TabStrip::new(vec![], 0, |_| {});
        strip.on_click(0.0, 960.0); // must not panic
    }

    // ── Quad rendering ────────────────────────────────────────────────────────

    #[test]
    fn empty_strip_renders_one_background_quad() {
        let strip = TabStrip::new(vec![], 0, |_| {});
        let quads = strip.render_quads(&strip_rect());
        assert_eq!(quads.len(), 1);
    }

    #[test]
    fn non_empty_strip_renders_three_quads() {
        let strip = TabStrip::new(make_tabs(3), 0, |_| {});
        let quads = strip.render_quads(&strip_rect());
        // 1 background + 1 active bg + 1 accent bar
        assert_eq!(quads.len(), 3);
    }

    #[test]
    fn strip_background_color_is_surface_recessed() {
        let ctx = RenderCtx::fallback();
        let t = Tokens::phosphor(ctx);
        let strip = TabStrip::new(make_tabs(2), 0, |_| {});
        let quads = strip.render_quads(&strip_rect());
        assert_eq!(quads[0].color, t.colors.surface_recessed);
    }

    #[test]
    fn active_tab_bg_uses_surface_raised() {
        let ctx = RenderCtx::fallback();
        let t = Tokens::phosphor(ctx);
        let strip = TabStrip::new(make_tabs(2), 0, |_| {});
        let quads = strip.render_quads(&strip_rect());
        assert_eq!(quads[1].color, t.colors.surface_raised);
    }

    #[test]
    fn accent_bar_uses_accent_focus_token() {
        let ctx = RenderCtx::fallback();
        let t = Tokens::phosphor(ctx);
        let strip = TabStrip::new(make_tabs(2), 0, |_| {});
        let quads = strip.render_quads(&strip_rect());
        // Third quad is the accent bar.
        assert_eq!(quads[2].color, t.colors.accent_focus);
    }

    #[test]
    fn accent_bar_is_at_strip_bottom() {
        let strip = TabStrip::new(make_tabs(2), 0, |_| {});
        let rect = strip_rect();
        let quads = strip.render_quads(&rect);
        let bar = &quads[2];
        assert!((bar.pos[1] + bar.size[1] - (rect.y + rect.height)).abs() < 0.01);
        assert!((bar.size[1] - ACCENT_BAR_H).abs() < 0.01);
    }

    // ── Text rendering ────────────────────────────────────────────────────────

    #[test]
    fn active_tab_text_uses_text_primary() {
        let ctx = RenderCtx::fallback();
        let t = Tokens::phosphor(ctx);
        let strip = TabStrip::new(
            vec![Tab::new("Alpha", None)],
            0,
            |_| {},
        );
        let texts = strip.render_text(&strip_rect());
        assert!(!texts.is_empty());
        assert_eq!(texts[0].color, t.colors.text_primary);
    }

    #[test]
    fn inactive_tab_text_uses_text_secondary() {
        let ctx = RenderCtx::fallback();
        let t = Tokens::phosphor(ctx);
        let strip = TabStrip::new(
            vec![
                Tab::new("A", None),
                Tab::new("B", None),
            ],
            0,
            |_| {},
        );
        let texts = strip.render_text(&strip_rect());
        // Second tab (inactive) should use text_secondary.
        let b_seg = texts.iter().find(|s| s.text == "B").expect("B not found");
        assert_eq!(b_seg.color, t.colors.text_secondary);
    }

    #[test]
    fn badge_renders_as_additional_segment_in_status_warn() {
        let ctx = RenderCtx::fallback();
        let t = Tokens::phosphor(ctx);
        let strip = TabStrip::new(
            vec![Tab::new("Errors", Some(5))],
            0,
            |_| {},
        );
        let texts = strip.render_text(&strip_rect());
        // Expect label segment + badge segment.
        assert_eq!(texts.len(), 2, "badge tab should emit 2 text segments");
        let badge_seg = texts.iter().find(|s| s.text.contains("[5]")).expect("badge missing");
        assert_eq!(badge_seg.color, t.colors.status_warn);
    }

    #[test]
    fn empty_strip_has_no_text_segments() {
        let strip = TabStrip::new(vec![], 0, |_| {});
        assert!(strip.render_text(&strip_rect()).is_empty());
    }

    // ── Tab display label ─────────────────────────────────────────────────────

    #[test]
    fn tab_display_label_no_badge() {
        let tab = Tab::new("Inspector", None);
        assert_eq!(tab.display_label(), "Inspector");
    }

    #[test]
    fn tab_display_label_with_badge() {
        let tab = Tab::new("Alerts", Some(3));
        assert_eq!(tab.display_label(), "Alerts [3]");
    }
}
