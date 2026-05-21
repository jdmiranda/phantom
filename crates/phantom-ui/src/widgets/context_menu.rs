//! Right-click context menu widget.
//!
//! [`ContextMenu`] renders a floating popup list anchored to a screen-space
//! click position. It supports:
//!
//! - Keyboard navigation (ArrowDown/ArrowUp skip disabled rows and separators;
//!   Enter activates; Escape hides).
//! - Token-driven colors: `surface_raised` background, `chrome_frame` border,
//!   `accent_focus` row highlight, `text_primary`/`text_disabled` item text.
//! - Optional separator lines drawn below items flagged with `separator_after`.
//! - Viewport-overflow clamping: the menu shifts up/left when it would extend
//!   past `bounds`.
//!
//! # Rendering primitives
//!
//! `render_quads` and `render_text` accept a `bounds` rect that represents the
//! full usable screen area (e.g. the window minus chrome). The menu computes its
//! own screen rect from `anchor`, `item_row_height`, and `item count`, then
//! clamps it so it stays inside `bounds`. Callers can pass the window rect as
//! `bounds` and rely on the widget to position itself correctly.
//!
//! # Usage
//!
//! ```rust,ignore
//! use phantom_ui::widgets::{ContextMenu, ContextMenuItem};
//! use phantom_ui::keybinds::Key;
//!
//! let mut menu = ContextMenu::new(vec![
//!     ContextMenuItem { label: "Copy".into(),  action: "copy".into(),  disabled: false, separator_after: false },
//!     ContextMenuItem { label: "Paste".into(), action: "paste".into(), disabled: false, separator_after: false },
//! ]);
//! menu.show_at(200.0, 300.0);
//!
//! if let Some(action) = menu.handle_key(&Key::Enter) {
//!     // dispatch `action`
//! }
//! ```

use crate::keybinds::Key;
use crate::layout::Rect;
use crate::render_ctx::RenderCtx;
use crate::tokens::Tokens;
use crate::widgets::{TextSegment, Widget};
use phantom_renderer::quads::QuadInstance;

// ─────────────────────────────────────────────────────────────────────────────
// Geometry constants
// ─────────────────────────────────────────────────────────────────────────────

/// Height of each item row in pixels.
pub const ITEM_ROW_HEIGHT: f32 = 22.0;

/// Height of a separator line in pixels.
const SEPARATOR_HEIGHT: f32 = 1.0;

/// Horizontal padding inside the menu, in pixels.
const H_PAD: f32 = 12.0;

/// Minimum menu width so narrow labels still look presentable.
const MIN_MENU_WIDTH: f32 = 160.0;

/// Border thickness (matches `Tokens::frame()`).
const BORDER: f32 = 2.0;

/// Approximate monospace char width used for label measurement.
/// Menus don't usually carry a live `RenderCtx`, so we fall back to this.
const CHAR_W: f32 = 8.0;

// ─────────────────────────────────────────────────────────────────────────────
// ContextMenuItem
// ─────────────────────────────────────────────────────────────────────────────

/// A single entry in a [`ContextMenu`].
#[derive(Debug, Clone, PartialEq)]
pub struct ContextMenuItem {
    /// Display label shown to the user.
    pub label: String,
    /// Opaque action tag returned from [`ContextMenu::handle_key`] when this
    /// item is activated. Callers interpret the string however they like.
    pub action: String,
    /// When `true` the item is rendered in `text_disabled` and cannot be
    /// focused or activated via keyboard.
    pub disabled: bool,
    /// When `true` a thin `chrome_frame` separator line is drawn below this
    /// item, visually grouping it with the items above.
    pub separator_after: bool,
}

// ─────────────────────────────────────────────────────────────────────────────
// ContextMenu
// ─────────────────────────────────────────────────────────────────────────────

/// Floating context menu widget.
///
/// The menu is hidden by default. Call [`show_at`](ContextMenu::show_at) to
/// position and reveal it, and [`hide`](ContextMenu::hide) to dismiss it.
/// Drive keyboard interaction via [`handle_key`](ContextMenu::handle_key).
#[derive(Debug, Clone)]
pub struct ContextMenu {
    /// The list of menu items.
    pub items: Vec<ContextMenuItem>,
    /// Index of the currently focused (highlighted) item.
    pub focused: usize,
    /// Whether the menu is currently visible.
    pub visible: bool,
    /// Screen-space position of the click that opened the menu.
    pub anchor: (f32, f32),
    /// Live render context for text metrics.
    ctx: RenderCtx,
}

impl ContextMenu {
    /// Create a new, hidden menu with the given items.
    ///
    /// Focus starts at the first non-disabled, non-separator item. If all items
    /// are disabled `focused` stays at 0.
    #[must_use]
    pub fn new(items: Vec<ContextMenuItem>) -> Self {
        let mut menu = Self {
            items,
            focused: 0,
            visible: false,
            anchor: (0.0, 0.0),
            ctx: RenderCtx::fallback(),
        };
        // Position focus on the first selectable item.
        menu.focused = menu.first_selectable().unwrap_or(0);
        menu
    }

    /// Update the live render context (call once per frame before rendering).
    pub fn set_render_ctx(&mut self, ctx: RenderCtx) {
        self.ctx = ctx;
    }

    /// Position the menu at `(x, y)` and make it visible.
    pub fn show_at(&mut self, x: f32, y: f32) {
        self.anchor = (x, y);
        self.visible = true;
        self.focused = self.first_selectable().unwrap_or(0);
    }

    /// Hide the menu without firing any action.
    pub fn hide(&mut self) {
        self.visible = false;
    }

    /// Handle a logical key event.
    ///
    /// - `ArrowDown`: advance focus, wrapping around, skipping disabled items.
    /// - `ArrowUp`: retreat focus, wrapping around, skipping disabled items.
    /// - `Enter`: return `Some(action)` for the focused item (no-op if disabled).
    /// - `Escape`: hide the menu and return `None`.
    /// - All other keys: return `None` without changing state.
    pub fn handle_key(&mut self, key: &Key) -> Option<String> {
        if !self.visible {
            return None;
        }

        match key {
            Key::Down => {
                self.advance_focus(1);
                None
            }
            Key::Up => {
                self.advance_focus(-1);
                None
            }
            Key::Enter => {
                let item = self.items.get(self.focused)?;
                if item.disabled {
                    return None;
                }
                Some(item.action.clone())
            }
            Key::Escape => {
                self.hide();
                None
            }
            _ => None,
        }
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    /// Return the index of the first selectable (non-disabled) item, or `None`
    /// if all items are disabled.
    fn first_selectable(&self) -> Option<usize> {
        self.items.iter().position(|it| !it.disabled)
    }

    /// Move focus by `delta` (+1 or -1), skipping disabled items.
    ///
    /// Wraps around. If there are no selectable items the focus does not change.
    fn advance_focus(&mut self, delta: i32) {
        let n = self.items.len();
        if n == 0 {
            return;
        }
        let selectables: Vec<usize> = (0..n)
            .filter(|&i| !self.items[i].disabled)
            .collect();
        if selectables.is_empty() {
            return;
        }

        // Find the position of current focus in the selectable list.
        let pos = selectables
            .iter()
            .position(|&i| i == self.focused)
            .unwrap_or(0);

        let new_pos = if delta > 0 {
            (pos + 1) % selectables.len()
        } else {
            (pos + selectables.len() - 1) % selectables.len()
        };
        self.focused = selectables[new_pos];
    }

    /// Compute the total pixel height of the menu including borders.
    fn menu_height(&self) -> f32 {
        let rows_h: f32 = self
            .items
            .iter()
            .map(|it| {
                ITEM_ROW_HEIGHT + if it.separator_after { SEPARATOR_HEIGHT } else { 0.0 }
            })
            .sum();
        rows_h + BORDER * 2.0
    }

    /// Compute the total pixel width of the menu including borders and padding.
    fn menu_width(&self) -> f32 {
        let max_label_px = self
            .items
            .iter()
            .map(|it| it.label.chars().count() as f32 * CHAR_W + H_PAD * 2.0)
            .fold(0.0_f32, f32::max);
        max_label_px.max(MIN_MENU_WIDTH) + BORDER * 2.0
    }

    /// Compute the clamped top-left corner of the menu so it stays inside
    /// `bounds`. The menu prefers to open below-right of the anchor; if that
    /// would overflow it shifts up and/or left.
    fn clamped_origin(&self, bounds: &Rect) -> (f32, f32) {
        let w = self.menu_width();
        let h = self.menu_height();

        let x = if self.anchor.0 + w > bounds.x + bounds.width {
            (bounds.x + bounds.width - w).max(bounds.x)
        } else {
            self.anchor.0
        };

        let y = if self.anchor.1 + h > bounds.y + bounds.height {
            (bounds.y + bounds.height - h).max(bounds.y)
        } else {
            self.anchor.1
        };

        (x, y)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Widget impl
// ─────────────────────────────────────────────────────────────────────────────

impl Widget for ContextMenu {
    /// Emit quads when visible.
    ///
    /// The `rect` parameter is treated as the **usable screen bounds** for
    /// overflow clamping; it is NOT the menu's own rect. The menu positions
    /// itself from `anchor` inside `rect`.
    ///
    /// Quads emitted (in order):
    /// 1. Background fill (`surface_raised`).
    /// 2. Four border edges (`chrome_frame`).
    /// 3. Per-item: optional focus highlight (`accent_focus`).
    /// 4. Per-item: optional separator line (`chrome_frame`) when `separator_after`.
    fn render_quads(&self, rect: &Rect) -> Vec<QuadInstance> {
        if !self.visible || self.items.is_empty() {
            return Vec::new();
        }

        let t = Tokens::phosphor(self.ctx);
        let (mx, my) = self.clamped_origin(rect);
        let mw = self.menu_width();
        let mh = self.menu_height();

        let mut quads = Vec::new();

        // 1. Background.
        quads.push(QuadInstance {
            pos: [mx, my],
            size: [mw, mh],
            color: t.colors.surface_raised,
            border_radius: 0.0,
        });

        // 2. Border edges.
        // Top
        quads.push(QuadInstance {
            pos: [mx, my],
            size: [mw, BORDER],
            color: t.colors.chrome_frame,
            border_radius: 0.0,
        });
        // Bottom
        quads.push(QuadInstance {
            pos: [mx, my + mh - BORDER],
            size: [mw, BORDER],
            color: t.colors.chrome_frame,
            border_radius: 0.0,
        });
        // Left
        quads.push(QuadInstance {
            pos: [mx, my],
            size: [BORDER, mh],
            color: t.colors.chrome_frame,
            border_radius: 0.0,
        });
        // Right
        quads.push(QuadInstance {
            pos: [mx + mw - BORDER, my],
            size: [BORDER, mh],
            color: t.colors.chrome_frame,
            border_radius: 0.0,
        });

        // 3 & 4. Per-item quads.
        let mut cursor_y = my + BORDER;
        for (idx, item) in self.items.iter().enumerate() {
            // Focus highlight.
            if idx == self.focused && !item.disabled {
                quads.push(QuadInstance {
                    pos: [mx + BORDER, cursor_y],
                    size: [mw - BORDER * 2.0, ITEM_ROW_HEIGHT],
                    color: t.colors.accent_focus,
                    border_radius: 0.0,
                });
            }

            cursor_y += ITEM_ROW_HEIGHT;

            // Separator line.
            if item.separator_after {
                quads.push(QuadInstance {
                    pos: [mx + BORDER, cursor_y],
                    size: [mw - BORDER * 2.0, SEPARATOR_HEIGHT],
                    color: t.colors.chrome_frame,
                    border_radius: 0.0,
                });
                cursor_y += SEPARATOR_HEIGHT;
            }
        }

        quads
    }

    /// Emit one [`TextSegment`] per item when visible.
    ///
    /// Enabled items use `text_primary`; disabled items use `text_disabled`
    /// (the `text_dim` token at 50 % alpha).
    fn render_text(&self, rect: &Rect) -> Vec<TextSegment> {
        if !self.visible || self.items.is_empty() {
            return Vec::new();
        }

        let t = Tokens::phosphor(self.ctx);
        let (mx, my) = self.clamped_origin(rect);

        // text_disabled = text_dim at 50% alpha.
        let text_disabled = {
            let mut c = t.colors.text_dim;
            c[3] *= 0.5;
            c
        };

        let mut segments = Vec::with_capacity(self.items.len());
        let mut cursor_y = my + BORDER;

        for item in &self.items {
            let color = if item.disabled {
                text_disabled
            } else {
                t.colors.text_primary
            };

            // Vertically center the text in the row.
            let text_y = cursor_y + (ITEM_ROW_HEIGHT - self.ctx.cell_h()) * 0.5;

            segments.push(TextSegment {
                text: item.label.clone(),
                x: mx + BORDER + H_PAD,
                y: text_y,
                color,
            });

            cursor_y += ITEM_ROW_HEIGHT;
            if item.separator_after {
                cursor_y += SEPARATOR_HEIGHT;
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
    use crate::keybinds::Key;
    use crate::render_ctx::RenderCtx;
    use crate::tokens::Tokens;

    // ── Helpers ───────────────────────────────────────────────────────────────

    /// A generous screen bounds rect used for most rendering tests.
    fn screen_rect() -> Rect {
        Rect {
            x: 0.0,
            y: 0.0,
            width: 1920.0,
            height: 1080.0,
        }
    }

    fn item(label: &str, action: &str) -> ContextMenuItem {
        ContextMenuItem {
            label: label.into(),
            action: action.into(),
            disabled: false,
            separator_after: false,
        }
    }

    fn disabled_item(label: &str, action: &str) -> ContextMenuItem {
        ContextMenuItem {
            label: label.into(),
            action: action.into(),
            disabled: true,
            separator_after: false,
        }
    }

    fn sep_item(label: &str, action: &str) -> ContextMenuItem {
        ContextMenuItem {
            label: label.into(),
            action: action.into(),
            disabled: false,
            separator_after: true,
        }
    }

    fn three_items() -> Vec<ContextMenuItem> {
        vec![item("Copy", "copy"), item("Paste", "paste"), item("Delete", "delete")]
    }

    // ── Construction ──────────────────────────────────────────────────────────

    #[test]
    fn new_creates_empty_focused_hidden_menu() {
        let menu = ContextMenu::new(three_items());
        assert!(!menu.visible, "new menu must be hidden");
        assert_eq!(menu.focused, 0, "focus starts at first item");
        assert_eq!(menu.anchor, (0.0, 0.0), "anchor defaults to origin");
    }

    #[test]
    fn new_skips_to_first_selectable_when_first_item_disabled() {
        let items = vec![
            disabled_item("No", "no"),
            item("Yes", "yes"),
        ];
        let menu = ContextMenu::new(items);
        assert_eq!(menu.focused, 1, "focus must skip leading disabled item");
    }

    #[test]
    fn new_all_disabled_focus_stays_at_zero() {
        let items = vec![
            disabled_item("A", "a"),
            disabled_item("B", "b"),
        ];
        let menu = ContextMenu::new(items);
        assert_eq!(menu.focused, 0, "focus stays at 0 when all items disabled");
    }

    // ── show_at / hide ────────────────────────────────────────────────────────

    #[test]
    fn show_at_sets_anchor_and_visible() {
        let mut menu = ContextMenu::new(three_items());
        menu.show_at(100.0, 200.0);
        assert!(menu.visible);
        assert_eq!(menu.anchor, (100.0, 200.0));
    }

    #[test]
    fn hide_sets_not_visible() {
        let mut menu = ContextMenu::new(three_items());
        menu.show_at(50.0, 50.0);
        menu.hide();
        assert!(!menu.visible);
    }

    #[test]
    fn show_at_resets_focus_to_first_selectable() {
        let items = vec![
            disabled_item("X", "x"),
            item("Open", "open"),
            item("Close", "close"),
        ];
        let mut menu = ContextMenu::new(items);
        // Manually corrupt focus.
        menu.focused = 2;
        menu.show_at(0.0, 0.0);
        assert_eq!(menu.focused, 1, "show_at must reset focus to first selectable");
    }

    // ── Keyboard navigation ───────────────────────────────────────────────────

    #[test]
    fn arrow_down_advances_focus_skipping_disabled() {
        let items = vec![
            item("A", "a"),
            disabled_item("B", "b"),
            item("C", "c"),
        ];
        let mut menu = ContextMenu::new(items);
        menu.show_at(0.0, 0.0);
        assert_eq!(menu.focused, 0);

        let result = menu.handle_key(&Key::Down);
        assert!(result.is_none(), "ArrowDown must not return an action");
        assert_eq!(menu.focused, 2, "focus must jump over disabled item B");
    }

    #[test]
    fn arrow_up_retreats_focus_wrapping() {
        let mut menu = ContextMenu::new(three_items());
        menu.show_at(0.0, 0.0);
        assert_eq!(menu.focused, 0);

        let result = menu.handle_key(&Key::Up);
        assert!(result.is_none());
        assert_eq!(menu.focused, 2, "ArrowUp from first item wraps to last");
    }

    #[test]
    fn arrow_down_wraps_from_last_to_first() {
        let mut menu = ContextMenu::new(three_items());
        menu.show_at(0.0, 0.0);
        menu.focused = 2;

        menu.handle_key(&Key::Down);
        assert_eq!(menu.focused, 0, "ArrowDown from last item wraps to first");
    }

    #[test]
    fn arrow_up_retreats_focus_one_step() {
        let mut menu = ContextMenu::new(three_items());
        menu.show_at(0.0, 0.0);
        menu.focused = 2;

        menu.handle_key(&Key::Up);
        assert_eq!(menu.focused, 1, "ArrowUp should retreat by one selectable step");
    }

    // ── Enter / Escape ────────────────────────────────────────────────────────

    #[test]
    fn enter_returns_action_string() {
        let mut menu = ContextMenu::new(three_items());
        menu.show_at(0.0, 0.0);
        menu.focused = 1;

        let action = menu.handle_key(&Key::Enter);
        assert_eq!(action, Some("paste".into()));
    }

    #[test]
    fn enter_on_disabled_item_returns_none() {
        let items = vec![
            disabled_item("No", "no"),
            item("Yes", "yes"),
        ];
        let mut menu = ContextMenu::new(items);
        menu.show_at(0.0, 0.0);
        // Force focus onto the disabled item.
        menu.focused = 0;

        let action = menu.handle_key(&Key::Enter);
        assert!(action.is_none(), "Enter on disabled item must return None");
    }

    #[test]
    fn escape_hides_and_returns_none() {
        let mut menu = ContextMenu::new(three_items());
        menu.show_at(0.0, 0.0);

        let action = menu.handle_key(&Key::Escape);
        assert!(action.is_none());
        assert!(!menu.visible, "Escape must hide the menu");
    }

    #[test]
    fn unhandled_key_returns_none_and_no_state_change() {
        let mut menu = ContextMenu::new(three_items());
        menu.show_at(0.0, 0.0);
        let focused_before = menu.focused;

        let result = menu.handle_key(&Key::Tab);
        assert!(result.is_none());
        assert_eq!(menu.focused, focused_before, "unhandled key must not change focus");
        assert!(menu.visible, "unhandled key must not hide the menu");
    }

    #[test]
    fn hidden_menu_handle_key_returns_none() {
        let mut menu = ContextMenu::new(three_items());
        // Menu is hidden by default.
        let result = menu.handle_key(&Key::Enter);
        assert!(result.is_none(), "hidden menu handle_key must always return None");
    }

    // ── All-disabled edge cases ───────────────────────────────────────────────

    #[test]
    fn all_disabled_items_no_focus_change_on_arrow() {
        let items = vec![
            disabled_item("A", "a"),
            disabled_item("B", "b"),
            disabled_item("C", "c"),
        ];
        let mut menu = ContextMenu::new(items);
        menu.show_at(0.0, 0.0);
        let focused_before = menu.focused;

        menu.handle_key(&Key::Down);
        assert_eq!(menu.focused, focused_before, "focus must not change when all items are disabled");

        menu.handle_key(&Key::Up);
        assert_eq!(menu.focused, focused_before, "focus must not change when all items are disabled");
    }

    // ── Rendering — quads ────────────────────────────────────────────────────

    #[test]
    fn hidden_menu_emits_no_quads() {
        let menu = ContextMenu::new(three_items());
        // Not shown — render_quads must be empty.
        let quads = menu.render_quads(&screen_rect());
        assert!(quads.is_empty(), "hidden menu must emit no quads");
    }

    #[test]
    fn visible_menu_emits_background_and_border_quads() {
        let mut menu = ContextMenu::new(three_items());
        menu.show_at(100.0, 100.0);
        let quads = menu.render_quads(&screen_rect());
        // Minimum: 1 background + 4 border edges = 5.
        assert!(quads.len() >= 5, "must emit at least background + 4 border quads; got {}", quads.len());
    }

    #[test]
    fn background_quad_uses_surface_raised_token() {
        let ctx = RenderCtx::fallback();
        let t = Tokens::phosphor(ctx);
        let mut menu = ContextMenu::new(three_items());
        menu.show_at(0.0, 0.0);
        let quads = menu.render_quads(&screen_rect());
        assert_eq!(quads[0].color, t.colors.surface_raised, "background must use surface_raised token");
    }

    #[test]
    fn border_quads_use_chrome_frame_token() {
        let ctx = RenderCtx::fallback();
        let t = Tokens::phosphor(ctx);
        let mut menu = ContextMenu::new(three_items());
        menu.show_at(0.0, 0.0);
        let quads = menu.render_quads(&screen_rect());
        // Quads 1-4 are the border edges.
        for q in &quads[1..5] {
            assert_eq!(q.color, t.colors.chrome_frame, "border quads must use chrome_frame token");
        }
    }

    #[test]
    fn focused_row_gets_accent_focus_highlight() {
        let ctx = RenderCtx::fallback();
        let t = Tokens::phosphor(ctx);
        let mut menu = ContextMenu::new(three_items());
        menu.show_at(0.0, 0.0);
        menu.focused = 1;
        let quads = menu.render_quads(&screen_rect());
        let has_focus_quad = quads.iter().any(|q| q.color == t.colors.accent_focus);
        assert!(has_focus_quad, "a quad with accent_focus color must be emitted for the focused row");
    }

    #[test]
    fn separator_after_emits_extra_quad() {
        let items = vec![
            sep_item("Cut", "cut"),
            item("Paste", "paste"),
        ];
        let mut menu = ContextMenu::new(items);
        menu.show_at(0.0, 0.0);
        let quads_no_sep = {
            let mut m2 = ContextMenu::new(vec![item("Cut", "cut"), item("Paste", "paste")]);
            m2.show_at(0.0, 0.0);
            m2.render_quads(&screen_rect()).len()
        };
        let quads_with_sep = menu.render_quads(&screen_rect()).len();
        assert!(
            quads_with_sep > quads_no_sep,
            "separator_after must emit an extra quad; got {quads_with_sep} vs {quads_no_sep}"
        );
    }

    // ── Rendering — text ─────────────────────────────────────────────────────

    #[test]
    fn hidden_menu_emits_no_text() {
        let menu = ContextMenu::new(three_items());
        assert!(menu.render_text(&screen_rect()).is_empty());
    }

    #[test]
    fn visible_menu_emits_one_text_segment_per_item() {
        let mut menu = ContextMenu::new(three_items());
        menu.show_at(0.0, 0.0);
        let texts = menu.render_text(&screen_rect());
        assert_eq!(texts.len(), 3, "one TextSegment per item");
    }

    #[test]
    fn enabled_item_uses_text_primary() {
        let ctx = RenderCtx::fallback();
        let t = Tokens::phosphor(ctx);
        let mut menu = ContextMenu::new(vec![item("Copy", "copy")]);
        menu.show_at(0.0, 0.0);
        let texts = menu.render_text(&screen_rect());
        assert_eq!(texts[0].color, t.colors.text_primary, "enabled item must use text_primary token");
    }

    #[test]
    fn disabled_item_uses_text_disabled_fifty_percent_alpha() {
        let ctx = RenderCtx::fallback();
        let t = Tokens::phosphor(ctx);
        let mut menu = ContextMenu::new(vec![disabled_item("Grayed", "grayed")]);
        menu.show_at(0.0, 0.0);
        let texts = menu.render_text(&screen_rect());
        // Alpha must be half of text_dim's alpha.
        let expected_alpha = t.colors.text_dim[3] * 0.5;
        assert!(
            (texts[0].color[3] - expected_alpha).abs() < 0.001,
            "disabled item alpha must be 50% of text_dim alpha; got {}",
            texts[0].color[3]
        );
    }

    // ── Overflow clamping ─────────────────────────────────────────────────────

    #[test]
    fn render_shifts_when_near_right_edge() {
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            width: 200.0,
            height: 600.0,
        };
        let mut menu = ContextMenu::new(three_items());
        // Anchor near the right edge so it would overflow.
        menu.show_at(190.0, 10.0);
        let quads = menu.render_quads(&bounds);
        // The background quad (index 0) x position must be <= (200 - menu_width).
        let bg = &quads[0];
        assert!(
            bg.pos[0] + bg.size[0] <= bounds.x + bounds.width + 0.01,
            "menu right edge must not exceed bounds; bg.pos[0]={}, bg.size[0]={}",
            bg.pos[0],
            bg.size[0]
        );
    }

    #[test]
    fn render_shifts_when_near_bottom_edge() {
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            width: 1920.0,
            height: 120.0,
        };
        let mut menu = ContextMenu::new(three_items());
        menu.show_at(10.0, 110.0);
        let quads = menu.render_quads(&bounds);
        let bg = &quads[0];
        assert!(
            bg.pos[1] + bg.size[1] <= bounds.y + bounds.height + 0.01,
            "menu bottom must not exceed bounds; bg.pos[1]={}, bg.size[1]={}",
            bg.pos[1],
            bg.size[1]
        );
    }

    // ── Separator geometry ────────────────────────────────────────────────────

    #[test]
    fn menu_height_includes_separator_pixels() {
        let items_no_sep = vec![item("A", "a"), item("B", "b")];
        let items_with_sep = vec![sep_item("A", "a"), item("B", "b")];

        let h_no_sep = ContextMenu::new(items_no_sep).menu_height();
        let h_with_sep = ContextMenu::new(items_with_sep).menu_height();

        assert!(
            h_with_sep > h_no_sep,
            "menu_height must be larger when separator_after is set; got {h_with_sep} vs {h_no_sep}"
        );
        assert!(
            (h_with_sep - h_no_sep - SEPARATOR_HEIGHT).abs() < 0.01,
            "height difference must equal SEPARATOR_HEIGHT"
        );
    }

    // ── Empty menu ────────────────────────────────────────────────────────────

    #[test]
    fn empty_menu_emits_nothing_even_when_visible() {
        let mut menu = ContextMenu::new(vec![]);
        menu.show_at(0.0, 0.0);
        assert!(menu.render_quads(&screen_rect()).is_empty());
        assert!(menu.render_text(&screen_rect()).is_empty());
    }
}
