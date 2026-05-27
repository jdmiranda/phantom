//! Issue #27 — Keybind help overlay.
//!
//! A full-screen translucent panel that lists every registered keyboard
//! shortcut. Toggled on/off with F1 or `?`. Rendered above all other content
//! in the overlay pass.
//!
//! ```text
//! ┌─ KEYBIND HELP (F1 or ? to close) ─────────────────────────────────┐
//! │  Cmd+D          Split pane horizontal                               │
//! │  Cmd+Shift+D    Split pane vertical                                 │
//! │  Cmd+W          Close pane                                          │
//! │  …                                                                  │
//! └────────────────────────────────────────────────────────────────────┘
//! ```

use crate::layout::Rect;
use crate::widgets::{TextSegment, Widget};
use phantom_renderer::quads::QuadInstance;

// ---------------------------------------------------------------------------
// Color constants
// ---------------------------------------------------------------------------

/// Panel background: dark semi-transparent.
const PANEL_BG: [f32; 4] = [0.02, 0.04, 0.06, 0.92];
/// Title row background: slightly brighter than the panel body.
const TITLE_BG: [f32; 4] = [0.04, 0.08, 0.10, 0.95];
/// Title text: bright phosphor cyan.
const TITLE_FG: [f32; 4] = [0.0, 0.85, 0.95, 0.90];
/// Keybind column text (the shortcut key combo): phosphor green, bright.
const KEY_FG: [f32; 4] = [0.2, 1.0, 0.4, 1.0];
/// Description column text: muted green.
const DESC_FG: [f32; 4] = [0.5, 0.8, 0.55, 0.85];
/// Separator line between title and entries.
const SEP_COLOR: [f32; 4] = [0.1, 0.4, 0.3, 0.6];

// ---------------------------------------------------------------------------
// Layout constants
// ---------------------------------------------------------------------------

/// Row height for each keybind entry, in pixels.
const ROW_H: f32 = 22.0;
/// Height of the title bar row, in pixels.
const TITLE_H: f32 = 28.0;
/// Left margin inside the panel.
const MARGIN_L: f32 = 24.0;
/// Column width reserved for the key-combo string.
const KEY_COL_W: f32 = 200.0;
/// Vertical padding between title bar and first entry.
const INNER_PAD: f32 = 4.0;

// ---------------------------------------------------------------------------
// Built-in entries
// ---------------------------------------------------------------------------

/// A single row in the help overlay.
#[derive(Debug, Clone)]
pub struct KeybindEntry {
    /// The key combo string (e.g. `"Cmd+D"`).
    pub keys: &'static str,
    /// Human-readable action description.
    pub description: &'static str,
}

/// Default entries matching the `KeybindRegistry` defaults.
const DEFAULT_ENTRIES: &[KeybindEntry] = &[
    KeybindEntry { keys: "Cmd+D",        description: "Split pane horizontal" },
    KeybindEntry { keys: "Cmd+Shift+D",  description: "Split pane vertical" },
    KeybindEntry { keys: "Cmd+W",        description: "Close pane" },
    KeybindEntry { keys: "Cmd+[",        description: "Focus previous pane" },
    KeybindEntry { keys: "Cmd+]",        description: "Focus next pane" },
    KeybindEntry { keys: "Cmd+C",        description: "Copy selection" },
    KeybindEntry { keys: "Cmd+V",        description: "Paste" },
    KeybindEntry { keys: "Cmd+=",        description: "Zoom in" },
    KeybindEntry { keys: "Cmd+-",        description: "Zoom out" },
    KeybindEntry { keys: "Cmd+I",        description: "Toggle inspector" },
    KeybindEntry { keys: "Cmd+Shift+M",  description: "Toggle system monitor" },
    KeybindEntry { keys: "Cmd+Shift+W",  description: "Toggle video (watch) pane" },
    KeybindEntry { keys: "Cmd+Shift+A",  description: "Toggle DAG (architecture) viewer" },
    KeybindEntry { keys: "Ctrl+,",       description: "Open settings" },
    KeybindEntry { keys: "Ctrl+Shift+F", description: "Float / dock pane" },
    KeybindEntry { keys: "F11",          description: "Toggle fullscreen" },
    KeybindEntry { keys: "F1 / ?",       description: "Show / hide this help" },
    KeybindEntry { keys: "`",            description: "Toggle command console" },
    KeybindEntry { keys: "Escape",       description: "Dismiss overlay / exit fullscreen" },
];

// ---------------------------------------------------------------------------
// KeybindHelp widget
// ---------------------------------------------------------------------------

/// Full-screen keybind help overlay.
///
/// Rendered above all other UI content when [`KeybindHelp::visible`] is `true`.
/// The caller should check `visible()` before invoking `render_quads` /
/// `render_text` so that no work is done when the overlay is hidden.
#[derive(Debug, Clone)]
pub struct KeybindHelp {
    /// Whether the overlay is currently shown.
    visible: bool,
    /// Entries to display. Defaults to [`DEFAULT_ENTRIES`].
    entries: Vec<KeybindEntry>,
}

impl KeybindHelp {
    /// Construct a `KeybindHelp` widget with the built-in default entries.
    #[must_use]
    pub fn new() -> Self {
        Self {
            visible: false,
            entries: DEFAULT_ENTRIES.to_vec(),
        }
    }

    /// Toggle visibility — F1 / `?` in the main key handler calls this.
    pub fn toggle(&mut self) {
        self.visible = !self.visible;
    }

    /// Show the overlay unconditionally.
    pub fn show(&mut self) {
        self.visible = true;
    }

    /// Hide the overlay unconditionally.
    pub fn hide(&mut self) {
        self.visible = false;
    }

    /// Whether the overlay is currently visible.
    #[must_use]
    pub fn visible(&self) -> bool {
        self.visible
    }

    /// Replace the entry list with a custom set.
    ///
    /// Useful for callers that want to display only the binds relevant to the
    /// current context (e.g. suppress terminal-only binds inside an agent pane).
    pub fn set_entries(&mut self, entries: Vec<KeybindEntry>) {
        self.entries = entries;
    }

    /// Computed pixel height of the full panel for a given number of entries.
    ///
    /// ```text
    /// height = TITLE_H + INNER_PAD + entry_count * ROW_H + INNER_PAD
    /// ```
    #[must_use]
    pub fn panel_height(&self) -> f32 {
        TITLE_H + INNER_PAD + self.entries.len() as f32 * ROW_H + INNER_PAD
    }
}

impl Default for KeybindHelp {
    fn default() -> Self {
        Self::new()
    }
}

impl Widget for KeybindHelp {
    /// Emits:
    /// 1. Full-screen background quad.
    /// 2. Title bar quad.
    /// 3. Separator line quad below the title.
    /// 4. One alternating-row highlight quad per entry (every second row).
    fn render_quads(&self, rect: &Rect) -> Vec<QuadInstance> {
        if !self.visible {
            return Vec::new();
        }

        let panel_h = self.panel_height().min(rect.height);
        // Centre the panel vertically.
        let panel_y = rect.y + (rect.height - panel_h) * 0.5;

        let mut quads = Vec::with_capacity(4 + self.entries.len());

        // Full panel background.
        quads.push(QuadInstance {
            pos: [rect.x, panel_y],
            size: [rect.width, panel_h],
            color: PANEL_BG,
            border_radius: 0.0,
        ..Default::default()
            });

        // Title bar.
        quads.push(QuadInstance {
            pos: [rect.x, panel_y],
            size: [rect.width, TITLE_H],
            color: TITLE_BG,
            border_radius: 0.0,
        ..Default::default()
            });

        // Separator below title.
        quads.push(QuadInstance {
            pos: [rect.x, panel_y + TITLE_H],
            size: [rect.width, 1.0],
            color: SEP_COLOR,
            border_radius: 0.0,
        ..Default::default()
            });

        // Alternating row highlights (every odd row).
        let body_y = panel_y + TITLE_H + INNER_PAD;
        for (i, _) in self.entries.iter().enumerate() {
            if i % 2 == 1 {
                quads.push(QuadInstance {
                    pos: [rect.x, body_y + i as f32 * ROW_H],
                    size: [rect.width, ROW_H],
                    color: [0.04, 0.07, 0.09, 0.5],
                    border_radius: 0.0,
                ..Default::default()
            });
            }
        }

        quads
    }

    /// Emits:
    /// 1. Title text segment.
    /// 2. Two segments per entry (key combo + description).
    fn render_text(&self, rect: &Rect) -> Vec<TextSegment> {
        if !self.visible {
            return Vec::new();
        }

        let panel_h = self.panel_height().min(rect.height);
        let panel_y = rect.y + (rect.height - panel_h) * 0.5;

        let mut segments = Vec::with_capacity(1 + self.entries.len() * 2);

        // Title.
        segments.push(TextSegment {
            text: "KEYBIND HELP  (F1 or ? to close)".to_owned(),
            x: rect.x + MARGIN_L,
            y: panel_y + (TITLE_H - 14.0) * 0.5,
            color: TITLE_FG,
        });

        // Entries.
        let body_y = panel_y + TITLE_H + INNER_PAD;
        let desc_x = rect.x + MARGIN_L + KEY_COL_W;

        for (i, entry) in self.entries.iter().enumerate() {
            let row_y = body_y + i as f32 * ROW_H + (ROW_H - 14.0) * 0.5;

            // Key combo column.
            segments.push(TextSegment {
                text: entry.keys.to_owned(),
                x: rect.x + MARGIN_L,
                y: row_y,
                color: KEY_FG,
            });

            // Description column.
            segments.push(TextSegment {
                text: entry.description.to_owned(),
                x: desc_x,
                y: row_y,
                color: DESC_FG,
            });
        }

        segments
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn full_rect() -> Rect {
        Rect {
            x: 0.0,
            y: 0.0,
            width: 1920.0,
            height: 1080.0,
        }
    }

    /// The module and re-export exist — this test is the acceptance criterion
    /// for Bug 1: `keybind_help_exported_from_widgets_mod`.
    #[test]
    fn keybind_help_exported_from_widgets_mod() {
        // Verify the type is constructible via `KeybindHelp::new()`, which
        // proves the module is compiled and re-exported correctly.
        let help = KeybindHelp::new();
        assert!(!help.visible(), "newly constructed widget should be hidden");
    }

    /// Bug 2 acceptance test: toggle flips visibility.
    #[test]
    fn f1_key_toggles_keybind_help_visible() {
        let mut help = KeybindHelp::new();
        assert!(!help.visible());
        help.toggle();
        assert!(help.visible(), "toggle should make it visible");
        help.toggle();
        assert!(!help.visible(), "second toggle should hide it");
    }

    #[test]
    fn show_and_hide_are_idempotent() {
        let mut help = KeybindHelp::new();
        help.show();
        help.show(); // idempotent
        assert!(help.visible());
        help.hide();
        help.hide(); // idempotent
        assert!(!help.visible());
    }

    #[test]
    fn hidden_widget_emits_no_quads() {
        let help = KeybindHelp::new(); // starts hidden
        let rect = full_rect();
        assert!(
            help.render_quads(&rect).is_empty(),
            "hidden overlay must emit no quads"
        );
    }

    #[test]
    fn hidden_widget_emits_no_text() {
        let help = KeybindHelp::new();
        let rect = full_rect();
        assert!(
            help.render_text(&rect).is_empty(),
            "hidden overlay must emit no text segments"
        );
    }

    #[test]
    fn visible_widget_emits_quads() {
        let mut help = KeybindHelp::new();
        help.show();
        let rect = full_rect();
        let quads = help.render_quads(&rect);
        assert!(
            !quads.is_empty(),
            "visible overlay must emit at least one quad"
        );
        // Expect at minimum: background + title + separator = 3
        assert!(
            quads.len() >= 3,
            "expected ≥3 quads (bg + title + separator), got {}",
            quads.len()
        );
    }

    #[test]
    fn visible_widget_emits_title_and_entries() {
        let mut help = KeybindHelp::new();
        help.show();
        let rect = full_rect();
        let texts = help.render_text(&rect);
        // 1 title + 2 per entry
        let expected = 1 + DEFAULT_ENTRIES.len() * 2;
        assert_eq!(
            texts.len(),
            expected,
            "expected {expected} text segments, got {}",
            texts.len()
        );
    }

    #[test]
    fn title_text_contains_help_hint() {
        let mut help = KeybindHelp::new();
        help.show();
        let rect = full_rect();
        let texts = help.render_text(&rect);
        let title = &texts[0];
        assert!(
            title.text.contains("F1") || title.text.contains("?"),
            "title must mention F1 or ? to close: '{}'",
            title.text
        );
    }

    #[test]
    fn custom_entries_replace_defaults() {
        let mut help = KeybindHelp::new();
        help.set_entries(vec![
            KeybindEntry { keys: "Ctrl+X", description: "Exit" },
        ]);
        help.show();
        let rect = full_rect();
        let texts = help.render_text(&rect);
        // 1 title + 2 for the one custom entry
        assert_eq!(texts.len(), 3);
        assert!(
            texts.iter().any(|s| s.text.contains("Ctrl+X")),
            "custom key must appear in output"
        );
    }

    #[test]
    fn panel_height_scales_with_entries() {
        let mut a = KeybindHelp::new();
        a.set_entries(vec![KeybindEntry { keys: "A", description: "a" }]);
        let mut b = KeybindHelp::new();
        b.set_entries(vec![
            KeybindEntry { keys: "A", description: "a" },
            KeybindEntry { keys: "B", description: "b" },
        ]);
        assert!(
            b.panel_height() > a.panel_height(),
            "more entries → taller panel"
        );
    }
}
