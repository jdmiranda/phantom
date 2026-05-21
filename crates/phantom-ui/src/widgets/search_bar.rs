//! Find-in-terminal search bar widget.
//!
//! A floating search bar that appears at the top-right of the terminal pane
//! when the user presses Cmd+F. It renders as:
//!
//! ```text
//! [ query____] [1/5] [^] [v] [X]
//! ```
//!
//! The widget is self-contained: it handles text input, emits
//! [`SearchBarAction`] on each key, and renders its own quads and text
//! segments. The app layer is responsible for wiring actions to the
//! scrollback index and feeding match counts back via
//! [`SearchBar::set_match_info`].

use crate::layout::Rect;
use phantom_renderer::quads::QuadInstance;

use super::TextSegment;

// ─────────────────────────────────────────────────────────────────────────────
// Constants
// ─────────────────────────────────────────────────────────────────────────────

/// Fixed height of the search bar in pixels.
pub const SEARCH_BAR_HEIGHT: f32 = 32.0;

/// Approximate character width used for layout math (same value as the rest of
/// the widget layer — the renderer does precise shaping independently).
const CHAR_W: f32 = 8.0;

/// Horizontal padding inside the input area.
const H_PAD: f32 = 8.0;

/// Colors (RGBA, linear sRGB).
const BG: [f32; 4] = [0.05, 0.07, 0.10, 0.95];
const BORDER: [f32; 4] = [0.0, 0.75, 0.9, 0.8];
const TEXT_ACTIVE: [f32; 4] = [0.85, 0.95, 0.85, 1.0];
const TEXT_DIM: [f32; 4] = [0.45, 0.55, 0.5, 0.85];
const CURSOR_COLOR: [f32; 4] = [0.2, 1.0, 0.55, 1.0];
const MATCH_COLOR: [f32; 4] = [0.4, 0.9, 0.4, 1.0];
const NO_MATCH_COLOR: [f32; 4] = [0.9, 0.35, 0.3, 1.0];

// ─────────────────────────────────────────────────────────────────────────────
// SearchKey — winit-free key enum for the widget layer
// ─────────────────────────────────────────────────────────────────────────────

/// Logical key events delivered to [`SearchBar::handle_key`].
///
/// The app layer maps winit `NamedKey` values to these variants so that
/// `phantom-ui` does not need to depend on `winit` directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchKey {
    /// Close the bar (Escape).
    Escape,
    /// Confirm / navigate to next match (Enter).
    Enter,
    /// Delete character before cursor.
    Backspace,
    /// Delete character at cursor.
    Delete,
    /// Move cursor left.
    Left,
    /// Move cursor right.
    Right,
    /// Navigate to previous match.
    Up,
    /// Navigate to next match.
    Down,
    /// Move cursor to start.
    Home,
    /// Move cursor to end.
    End,
}

// ─────────────────────────────────────────────────────────────────────────────
// SearchBarAction
// ─────────────────────────────────────────────────────────────────────────────

/// Actions emitted by [`SearchBar::handle_key`] and [`SearchBar::handle_char`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchBarAction {
    /// Key did not produce a notable action.
    None,
    /// Query string changed; the caller should rebuild the scrollback index.
    QueryChanged(String),
    /// Navigate to the next match.
    Next,
    /// Navigate to the previous match.
    Prev,
    /// Close the search bar.
    Close,
}

// ─────────────────────────────────────────────────────────────────────────────
// SearchBar
// ─────────────────────────────────────────────────────────────────────────────

/// Floating find-in-terminal search bar.
///
/// Owned by `App` (or the terminal adapter layer). Toggled open/closed by
/// `Cmd+F`. When visible, keyboard events should be routed here *before*
/// they reach the terminal PTY.
#[derive(Debug, Clone)]
pub struct SearchBar {
    /// Whether the bar is currently displayed.
    pub visible: bool,
    /// Current query string (chars).
    query: Vec<char>,
    /// Cursor position as a char index into `query`.
    cursor_pos: usize,
    /// Total number of matches in the current scrollback index.
    match_count: usize,
    /// The 1-indexed currently highlighted match (0 when no matches).
    current_match: usize,
}

impl SearchBar {
    /// Construct a hidden search bar with an empty query.
    #[must_use]
    pub fn new() -> Self {
        Self {
            visible: false,
            query: Vec::new(),
            cursor_pos: 0,
            match_count: 0,
            current_match: 0,
        }
    }

    /// Show the search bar and focus the query input.
    pub fn show(&mut self) {
        self.visible = true;
    }

    /// Hide the search bar without clearing the query.
    pub fn hide(&mut self) {
        self.visible = false;
    }

    /// Toggle visibility.
    pub fn toggle(&mut self) {
        self.visible = !self.visible;
    }

    /// Return the current query as an owned `String`.
    #[must_use]
    pub fn query(&self) -> String {
        self.query.iter().collect()
    }

    /// Total number of matches in the current index.
    #[must_use]
    pub fn match_count(&self) -> usize {
        self.match_count
    }

    /// The 1-indexed currently highlighted match (0 when there are no matches).
    #[must_use]
    pub fn current_match(&self) -> usize {
        self.current_match
    }

    /// Update the match counters (called by the app after re-indexing).
    pub fn set_match_info(&mut self, total: usize, current: usize) {
        self.match_count = total;
        self.current_match = if total > 0 { current.clamp(1, total) } else { 0 };
    }

    /// Handle a logical key event and return the resulting action.
    ///
    /// Only call this when the bar is visible; the caller is responsible for
    /// the gate.
    pub fn handle_key(&mut self, key: SearchKey) -> SearchBarAction {
        match key {
            SearchKey::Escape => {
                self.hide();
                SearchBarAction::Close
            }
            SearchKey::Enter => SearchBarAction::Next,
            SearchKey::Backspace => {
                if self.cursor_pos > 0 {
                    self.cursor_pos -= 1;
                    self.query.remove(self.cursor_pos);
                    SearchBarAction::QueryChanged(self.query())
                } else {
                    SearchBarAction::None
                }
            }
            SearchKey::Delete => {
                if self.cursor_pos < self.query.len() {
                    self.query.remove(self.cursor_pos);
                    SearchBarAction::QueryChanged(self.query())
                } else {
                    SearchBarAction::None
                }
            }
            SearchKey::Left => {
                if self.cursor_pos > 0 {
                    self.cursor_pos -= 1;
                }
                SearchBarAction::None
            }
            SearchKey::Right => {
                if self.cursor_pos < self.query.len() {
                    self.cursor_pos += 1;
                }
                SearchBarAction::None
            }
            SearchKey::Up => SearchBarAction::Prev,
            SearchKey::Down => SearchBarAction::Next,
            SearchKey::Home => {
                self.cursor_pos = 0;
                SearchBarAction::None
            }
            SearchKey::End => {
                self.cursor_pos = self.query.len();
                SearchBarAction::None
            }
        }
    }

    /// Handle a printable character input and return the resulting action.
    pub fn handle_char(&mut self, ch: char) -> SearchBarAction {
        self.query.insert(self.cursor_pos, ch);
        self.cursor_pos += 1;
        SearchBarAction::QueryChanged(self.query())
    }

    /// Produce the background and border quads for the search bar.
    ///
    /// The bar is anchored to the top-right of `pane_rect`.
    #[must_use]
    pub fn render_quads(&self, pane_rect: &Rect) -> Vec<QuadInstance> {
        if !self.visible {
            return Vec::new();
        }

        let bar_w = 340.0_f32.min(pane_rect.width * 0.55);
        let bar_h = SEARCH_BAR_HEIGHT;
        let bar_x = pane_rect.x + pane_rect.width - bar_w - 8.0;
        let bar_y = pane_rect.y + 8.0;

        let mut quads = Vec::with_capacity(6);

        // Drop shadow for depth.
        quads.push(QuadInstance {
            pos: [bar_x + 2.0, bar_y + 2.0],
            size: [bar_w, bar_h],
            color: [0.0, 0.0, 0.0, 0.25],
            border_radius: 4.0,
        });

        // Background panel.
        quads.push(QuadInstance {
            pos: [bar_x, bar_y],
            size: [bar_w, bar_h],
            color: BG,
            border_radius: 4.0,
        });

        // Border lines (top, bottom, left, right).
        let t = 1.0;
        for &(pos, size) in &[
            ([bar_x, bar_y], [bar_w, t]),
            ([bar_x, bar_y + bar_h - t], [bar_w, t]),
            ([bar_x, bar_y], [t, bar_h]),
            ([bar_x + bar_w - t, bar_y], [t, bar_h]),
        ] {
            quads.push(QuadInstance {
                pos,
                size,
                color: BORDER,
                border_radius: 0.0,
            });
        }

        quads
    }

    /// Produce text segments for the search bar contents.
    #[must_use]
    pub fn render_text(&self, pane_rect: &Rect) -> Vec<TextSegment> {
        if !self.visible {
            return Vec::new();
        }

        let bar_w = 340.0_f32.min(pane_rect.width * 0.55);
        let bar_h = SEARCH_BAR_HEIGHT;
        let bar_x = pane_rect.x + pane_rect.width - bar_w - 8.0;
        let bar_y = pane_rect.y + 8.0;

        let text_y = bar_y + (bar_h - 14.0) * 0.5;
        let mut segments = Vec::with_capacity(4);

        // Match counter  "3/12"  or  "0/0"
        let counter_text = if self.match_count == 0 {
            "0/0".to_owned()
        } else {
            format!("{}/{}", self.current_match, self.match_count)
        };
        let counter_color = if self.match_count == 0 {
            NO_MATCH_COLOR
        } else {
            MATCH_COLOR
        };

        // Navigation buttons and close — anchored right.
        let controls = format!(" {} [^][v][X]", counter_text);
        let controls_w = controls.len() as f32 * CHAR_W;
        let controls_x = bar_x + bar_w - controls_w - H_PAD;
        segments.push(TextSegment {
            text: controls,
            x: controls_x,
            y: text_y,
            color: counter_color,
        });

        // Search icon.
        let icon = "/ ";
        let icon_w = icon.len() as f32 * CHAR_W;
        segments.push(TextSegment {
            text: icon.to_owned(),
            x: bar_x + H_PAD,
            y: text_y,
            color: TEXT_DIM,
        });

        // Query text.
        let query_str: String = self.query.iter().collect();
        let query_x = bar_x + H_PAD + icon_w;

        if !query_str.is_empty() {
            segments.push(TextSegment {
                text: query_str,
                x: query_x,
                y: text_y,
                color: TEXT_ACTIVE,
            });
        }

        // Cursor block at the insertion point.
        let before_cursor: String = self.query[..self.cursor_pos].iter().collect();
        let cursor_x = query_x + before_cursor.len() as f32 * CHAR_W;
        segments.push(TextSegment {
            text: "_".to_owned(),
            x: cursor_x,
            y: text_y,
            color: CURSOR_COLOR,
        });

        segments
    }
}

impl Default for SearchBar {
    fn default() -> Self {
        Self::new()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_bar_query_changed_on_char_input() {
        let mut bar = SearchBar::new();
        bar.show();
        let action = bar.handle_char('h');
        assert_eq!(action, SearchBarAction::QueryChanged("h".to_owned()));
        let action2 = bar.handle_char('i');
        assert_eq!(action2, SearchBarAction::QueryChanged("hi".to_owned()));
        assert_eq!(bar.query(), "hi");
    }

    #[test]
    fn search_bar_closes_on_escape() {
        let mut bar = SearchBar::new();
        bar.show();
        assert!(bar.visible);
        let action = bar.handle_key(SearchKey::Escape);
        assert_eq!(action, SearchBarAction::Close);
        assert!(!bar.visible);
    }

    #[test]
    fn search_bar_backspace_removes_char() {
        let mut bar = SearchBar::new();
        bar.show();
        bar.handle_char('a');
        bar.handle_char('b');
        assert_eq!(bar.query(), "ab");
        let action = bar.handle_key(SearchKey::Backspace);
        assert_eq!(action, SearchBarAction::QueryChanged("a".to_owned()));
        assert_eq!(bar.query(), "a");
    }

    #[test]
    fn search_bar_toggle_visibility() {
        let mut bar = SearchBar::new();
        assert!(!bar.visible);
        bar.toggle();
        assert!(bar.visible);
        bar.toggle();
        assert!(!bar.visible);
    }

    #[test]
    fn search_bar_set_match_info() {
        let mut bar = SearchBar::new();
        bar.set_match_info(5, 2);
        assert_eq!(bar.match_count(), 5);
        assert_eq!(bar.current_match(), 2);
    }

    #[test]
    fn search_bar_set_match_info_no_matches() {
        let mut bar = SearchBar::new();
        bar.set_match_info(0, 0);
        assert_eq!(bar.match_count(), 0);
        assert_eq!(bar.current_match(), 0);
    }

    #[test]
    fn search_bar_enter_returns_next() {
        let mut bar = SearchBar::new();
        bar.show();
        let action = bar.handle_key(SearchKey::Enter);
        assert_eq!(action, SearchBarAction::Next);
    }

    #[test]
    fn search_bar_up_returns_prev() {
        let mut bar = SearchBar::new();
        bar.show();
        let action = bar.handle_key(SearchKey::Up);
        assert_eq!(action, SearchBarAction::Prev);
    }

    #[test]
    fn search_bar_down_returns_next() {
        let mut bar = SearchBar::new();
        bar.show();
        let action = bar.handle_key(SearchKey::Down);
        assert_eq!(action, SearchBarAction::Next);
    }
}
