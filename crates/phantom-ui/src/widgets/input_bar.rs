//! Issue #23 — `InputBar` widget.
//!
//! A single-line text input with:
//! - An optional prompt prefix (e.g. `"> "` or `"$ "`).
//! - A cursor maintained at `cursor_pos` (byte index into `buffer`).
//! - Cursor blink via [`crate::cursor::CursorBlink`].
//! - Token-driven colors: input text = `text_primary`, prompt = `text_dim`,
//!   cursor block = `text_primary` (modulated by blink state), background =
//!   `surface_recessed`.
//! - Keyboard event dispatch: `handle_key` interprets common keys and calls
//!   `on_submit` on Enter.
//! - A simple command-history ring accessible via `history_prev` / `history_next`.
//!
//! The widget does **not** depend on the agent runtime — it is usable
//! standalone wherever a text input is needed.
//!
//! # Examples
//!
//! ```rust,ignore
//! use phantom_ui::widgets::input_bar::InputBar;
//!
//! let mut bar = InputBar::new(Some("> ".into()), |text| println!("submit: {text}"));
//! bar.handle_key(InputKey::Char('h'));
//! bar.handle_key(InputKey::Char('i'));
//! bar.handle_key(InputKey::Enter);
//! ```

use crate::cursor::CursorBlink;
use crate::layout::Rect;
use crate::render_ctx::RenderCtx;
use crate::tokens::Tokens;
use crate::widgets::{TextSegment, Widget};
use phantom_renderer::quads::QuadInstance;

// ─────────────────────────────────────────────────────────────────────────────
// Constants
// ─────────────────────────────────────────────────────────────────────────────

/// Fixed height of the input bar in pixels.
pub const INPUT_BAR_HEIGHT: f32 = 28.0;

/// Horizontal padding between the rect edge and the prompt / input text.
const H_PAD: f32 = 8.0;

// ─────────────────────────────────────────────────────────────────────────────
// InputKey
// ─────────────────────────────────────────────────────────────────────────────

/// Logical key events delivered to [`InputBar::handle_key`].
///
/// The caller maps hardware key codes to these variants before dispatching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputKey {
    /// A printable character.
    Char(char),
    /// Backspace — delete the character before the cursor.
    Backspace,
    /// Delete — delete the character at the cursor.
    Delete,
    /// Move cursor left one character.
    Left,
    /// Move cursor right one character.
    Right,
    /// Move cursor to the beginning of the buffer.
    Home,
    /// Move cursor to the end of the buffer.
    End,
    /// Submit the buffer and fire `on_submit`.
    Enter,
    /// Scroll backwards in command history.
    HistoryPrev,
    /// Scroll forwards in command history.
    HistoryNext,
}

// ─────────────────────────────────────────────────────────────────────────────
// InputBar
// ─────────────────────────────────────────────────────────────────────────────

/// Single-line text input widget with prompt, cursor, blink, and history.
///
/// The widget is non-`Clone` because `on_submit` is a heap-allocated closure.
/// Snapshot `buffer()` and `cursor_pos()` individually if you need serialisable
/// state.
pub struct InputBar {
    /// Current text content of the input field.
    buffer: String,
    /// Cursor position as a **char** index into `buffer` (not byte offset).
    cursor_pos: usize,
    /// Optional prompt rendered before the user input (e.g. `"> "`).
    prompt: Option<String>,
    /// Called when the user presses Enter. Receives the buffer contents.
    on_submit: Box<dyn FnMut(&str)>,
    /// Cursor blink state.
    blink: CursorBlink,
    /// Command history ring (oldest → newest).
    history: Vec<String>,
    /// Current position in the history ring (`None` = live input, not in history).
    history_pos: Option<usize>,
    /// Saved live buffer when the user navigates into history.
    live_buffer: String,
    /// Live render metrics.
    ctx: RenderCtx,
}

impl InputBar {
    /// Create an [`InputBar`] with an optional prompt and submit callback.
    ///
    /// The buffer starts empty and the cursor is at position 0.
    pub fn new(prompt: Option<String>, on_submit: impl FnMut(&str) + 'static) -> Self {
        Self {
            buffer: String::new(),
            cursor_pos: 0,
            prompt,
            on_submit: Box::new(on_submit),
            blink: CursorBlink::default(),
            history: Vec::new(),
            history_pos: None,
            live_buffer: String::new(),
            ctx: RenderCtx::fallback(),
        }
    }

    /// Update the live render context.
    pub fn set_render_ctx(&mut self, ctx: RenderCtx) {
        self.ctx = ctx;
    }

    /// Current text content of the input field.
    pub fn buffer(&self) -> &str {
        &self.buffer
    }

    /// Cursor position as a char index into the buffer (not byte offset).
    pub fn cursor_pos(&self) -> usize {
        self.cursor_pos
    }

    /// Optional prompt string rendered before the user input.
    pub fn prompt(&self) -> Option<&str> {
        self.prompt.as_deref()
    }

    /// Advance the cursor-blink timer. Call once per frame with the current
    /// wall-clock time in milliseconds.
    pub fn tick(&mut self, now_ms: u64) {
        self.blink.tick(now_ms);
    }

    /// Push a string onto the history ring (e.g. after a successful submit).
    ///
    /// Consecutive duplicates are deduplicated: if the new entry is identical
    /// to the most-recent history entry it is silently dropped.
    pub fn push_history(&mut self, entry: &str) {
        if entry.is_empty() {
            return;
        }
        if self.history.last().map(|s| s.as_str()) == Some(entry) {
            return;
        }
        self.history.push(entry.to_owned());
    }

    /// Handle a logical key event, updating `buffer` and `cursor_pos`.
    ///
    /// Returns `true` when the key was consumed (always, for now).
    pub fn handle_key(&mut self, key: InputKey) -> bool {
        match key {
            InputKey::Char(c) => {
                self.buffer.insert(self.byte_offset(self.cursor_pos), c);
                self.cursor_pos += 1;
                self.blink.reset(0);
                self.history_pos = None;
            }
            InputKey::Backspace => {
                if self.cursor_pos > 0 {
                    let byte = self.byte_offset(self.cursor_pos - 1);
                    self.buffer.remove(byte);
                    self.cursor_pos -= 1;
                    self.blink.reset(0);
                    self.history_pos = None;
                }
            }
            InputKey::Delete => {
                if self.cursor_pos < self.char_count() {
                    let byte = self.byte_offset(self.cursor_pos);
                    self.buffer.remove(byte);
                    self.blink.reset(0);
                    self.history_pos = None;
                }
            }
            InputKey::Left => {
                if self.cursor_pos > 0 {
                    self.cursor_pos -= 1;
                }
            }
            InputKey::Right => {
                if self.cursor_pos < self.char_count() {
                    self.cursor_pos += 1;
                }
            }
            InputKey::Home => {
                self.cursor_pos = 0;
            }
            InputKey::End => {
                self.cursor_pos = self.char_count();
            }
            InputKey::Enter => {
                let text = self.buffer.clone();
                self.push_history(&text);
                (self.on_submit)(&text);
                self.buffer.clear();
                self.cursor_pos = 0;
                self.history_pos = None;
                self.blink.reset(0);
            }
            InputKey::HistoryPrev => {
                self.history_prev();
            }
            InputKey::HistoryNext => {
                self.history_next();
            }
        }
        true
    }

    // ── Private helpers ──────────────────────────────────────────────────────

    /// Number of chars in the current buffer.
    fn char_count(&self) -> usize {
        self.buffer.chars().count()
    }

    /// Convert a char-index `pos` to a byte offset in `buffer`.
    ///
    /// Clamps to `buffer.len()` so it is always valid for slicing.
    fn byte_offset(&self, pos: usize) -> usize {
        self.buffer
            .char_indices()
            .nth(pos)
            .map(|(b, _)| b)
            .unwrap_or(self.buffer.len())
    }

    /// Navigate backwards in history (older commands).
    fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let new_pos = match self.history_pos {
            None => {
                // Save the live buffer before entering history.
                self.live_buffer = self.buffer.clone();
                self.history.len() - 1
            }
            Some(0) => 0, // Already at the oldest entry.
            Some(p) => p - 1,
        };
        self.history_pos = Some(new_pos);
        self.buffer = self.history[new_pos].clone();
        self.cursor_pos = self.char_count();
    }

    /// Navigate forwards in history (newer commands, back to live input).
    fn history_next(&mut self) {
        match self.history_pos {
            None => {} // Already at live input.
            Some(p) if p + 1 >= self.history.len() => {
                // Past the newest entry: return to live buffer.
                self.buffer = self.live_buffer.clone();
                self.history_pos = None;
                self.cursor_pos = self.char_count();
            }
            Some(p) => {
                let new_pos = p + 1;
                self.history_pos = Some(new_pos);
                self.buffer = self.history[new_pos].clone();
                self.cursor_pos = self.char_count();
            }
        }
    }

    /// Width in chars of the prompt (0 if absent).
    fn prompt_char_width(&self) -> usize {
        self.prompt.as_deref().map(|p| p.chars().count()).unwrap_or(0)
    }
}

impl Widget for InputBar {
    /// Emit:
    /// 1. Full-width background quad (`surface_recessed`).
    /// 2. Cursor block quad (`text_primary`, modulated by blink alpha) positioned
    ///    at the character cell where the cursor currently sits.
    fn render_quads(&self, rect: &Rect) -> Vec<QuadInstance> {
        let t = Tokens::phosphor(self.ctx);
        let char_w = self.ctx.cell_w();
        let char_h = self.ctx.cell_h();

        let mut quads = Vec::with_capacity(2);

        // 1. Background.
        quads.push(QuadInstance {
            pos: [rect.x, rect.y],
            size: [rect.width, rect.height],
            color: t.colors.surface_recessed,
            border_radius: 0.0,
        });

        // 2. Cursor block.
        let prompt_w = self.prompt_char_width() as f32 * char_w;
        let cursor_x = rect.x + H_PAD + prompt_w + self.cursor_pos as f32 * char_w;
        let cursor_y = rect.y + (rect.height - char_h) * 0.5;

        let cursor_color = self.blink.color(t.colors.text_primary);

        quads.push(QuadInstance {
            pos: [cursor_x, cursor_y],
            size: [char_w, char_h],
            color: cursor_color,
            border_radius: 0.0,
        });

        quads
    }

    /// Emit text segments:
    /// - Prompt (if any) in `text_dim`.
    /// - Buffer text in `text_primary`.
    fn render_text(&self, rect: &Rect) -> Vec<TextSegment> {
        let t = Tokens::phosphor(self.ctx);
        let char_w = self.ctx.cell_w();
        let char_h = self.ctx.cell_h();
        let text_y = rect.y + (rect.height - char_h) * 0.5;

        let mut segments = Vec::with_capacity(2);

        match &self.prompt {
            Some(prompt) => {
                // Prompt segment.
                segments.push(TextSegment {
                    text: prompt.clone(),
                    x: rect.x + H_PAD,
                    y: text_y,
                    color: t.colors.text_dim,
                });

                // Buffer segment (offset by prompt width).
                if !self.buffer.is_empty() {
                    let prompt_w = prompt.chars().count() as f32 * char_w;
                    segments.push(TextSegment {
                        text: self.buffer.clone(),
                        x: rect.x + H_PAD + prompt_w,
                        y: text_y,
                        color: t.colors.text_primary,
                    });
                }
            }
            None => {
                if !self.buffer.is_empty() {
                    segments.push(TextSegment {
                        text: self.buffer.clone(),
                        x: rect.x + H_PAD,
                        y: text_y,
                        color: t.colors.text_primary,
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

    fn input_rect() -> Rect {
        Rect { x: 0.0, y: 0.0, width: 800.0, height: INPUT_BAR_HEIGHT }
    }

    fn bare_bar() -> InputBar {
        InputBar::new(None, |_| {})
    }

    fn prompted_bar() -> InputBar {
        InputBar::new(Some("> ".into()), |_| {})
    }

    // ── Construction ──────────────────────────────────────────────────────────

    #[test]
    fn new_buffer_is_empty() {
        let bar = bare_bar();
        assert!(bar.buffer().is_empty());
        assert_eq!(bar.cursor_pos(), 0);
    }

    #[test]
    fn prompt_stored_correctly() {
        let bar = InputBar::new(Some("$ ".into()), |_| {});
        assert_eq!(bar.prompt(), Some("$ "));
    }

    // ── Char input ────────────────────────────────────────────────────────────

    #[test]
    fn typing_chars_appends_to_buffer() {
        let mut bar = bare_bar();
        bar.handle_key(InputKey::Char('h'));
        bar.handle_key(InputKey::Char('i'));
        assert_eq!(bar.buffer(), "hi");
        assert_eq!(bar.cursor_pos(), 2);
    }

    #[test]
    fn cursor_in_middle_inserts_at_cursor() {
        let mut bar = bare_bar();
        bar.handle_key(InputKey::Char('a'));
        bar.handle_key(InputKey::Char('c'));
        bar.handle_key(InputKey::Left);
        bar.handle_key(InputKey::Char('b'));
        assert_eq!(bar.buffer(), "abc");
        assert_eq!(bar.cursor_pos(), 2);
    }

    // ── Backspace / Delete ────────────────────────────────────────────────────

    #[test]
    fn backspace_removes_char_before_cursor() {
        let mut bar = bare_bar();
        bar.handle_key(InputKey::Char('a'));
        bar.handle_key(InputKey::Char('b'));
        bar.handle_key(InputKey::Backspace);
        assert_eq!(bar.buffer(), "a");
        assert_eq!(bar.cursor_pos(), 1);
    }

    #[test]
    fn backspace_at_start_is_no_op() {
        let mut bar = bare_bar();
        bar.handle_key(InputKey::Backspace); // must not panic
        assert!(bar.buffer().is_empty());
    }

    #[test]
    fn delete_removes_char_at_cursor() {
        let mut bar = bare_bar();
        bar.handle_key(InputKey::Char('a'));
        bar.handle_key(InputKey::Char('b'));
        bar.handle_key(InputKey::Left); // cursor before 'b'
        bar.handle_key(InputKey::Delete);
        assert_eq!(bar.buffer(), "a");
        assert_eq!(bar.cursor_pos(), 1);
    }

    #[test]
    fn delete_at_end_is_no_op() {
        let mut bar = bare_bar();
        bar.handle_key(InputKey::Char('x'));
        bar.handle_key(InputKey::Delete); // cursor at end — no-op
        assert_eq!(bar.buffer(), "x");
    }

    // ── Navigation ────────────────────────────────────────────────────────────

    #[test]
    fn left_decrements_cursor() {
        let mut bar = bare_bar();
        bar.handle_key(InputKey::Char('a'));
        bar.handle_key(InputKey::Left);
        assert_eq!(bar.cursor_pos(), 0);
    }

    #[test]
    fn left_at_start_is_clamped() {
        let mut bar = bare_bar();
        bar.handle_key(InputKey::Left); // must not underflow
        assert_eq!(bar.cursor_pos(), 0);
    }

    #[test]
    fn right_increments_cursor() {
        let mut bar = bare_bar();
        bar.handle_key(InputKey::Char('a'));
        bar.handle_key(InputKey::Left);
        bar.handle_key(InputKey::Right);
        assert_eq!(bar.cursor_pos(), 1);
    }

    #[test]
    fn right_at_end_is_clamped() {
        let mut bar = bare_bar();
        bar.handle_key(InputKey::Char('a'));
        bar.handle_key(InputKey::Right); // already at end
        assert_eq!(bar.cursor_pos(), 1);
    }

    #[test]
    fn home_moves_cursor_to_zero() {
        let mut bar = bare_bar();
        bar.handle_key(InputKey::Char('a'));
        bar.handle_key(InputKey::Char('b'));
        bar.handle_key(InputKey::Home);
        assert_eq!(bar.cursor_pos(), 0);
    }

    #[test]
    fn end_moves_cursor_to_buffer_end() {
        let mut bar = bare_bar();
        bar.handle_key(InputKey::Char('a'));
        bar.handle_key(InputKey::Char('b'));
        bar.handle_key(InputKey::Home);
        bar.handle_key(InputKey::End);
        assert_eq!(bar.cursor_pos(), 2);
    }

    // ── Enter / submit ────────────────────────────────────────────────────────

    #[test]
    fn enter_fires_callback_with_buffer() {
        let submitted = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let submitted_clone = submitted.clone();
        let mut bar = InputBar::new(None, move |text| {
            *submitted_clone.lock().unwrap() = text.to_owned();
        });
        bar.handle_key(InputKey::Char('o'));
        bar.handle_key(InputKey::Char('k'));
        bar.handle_key(InputKey::Enter);
        assert_eq!(submitted.lock().unwrap().as_str(), "ok");
    }

    #[test]
    fn enter_clears_buffer_and_resets_cursor() {
        let mut bar = bare_bar();
        bar.handle_key(InputKey::Char('x'));
        bar.handle_key(InputKey::Enter);
        assert!(bar.buffer().is_empty());
        assert_eq!(bar.cursor_pos(), 0);
    }

    // ── History ───────────────────────────────────────────────────────────────

    #[test]
    fn history_prev_loads_last_entry() {
        let mut bar = bare_bar();
        bar.push_history("first");
        bar.push_history("second");
        bar.handle_key(InputKey::HistoryPrev);
        assert_eq!(bar.buffer(), "second");
    }

    #[test]
    fn history_prev_twice_loads_earlier_entry() {
        let mut bar = bare_bar();
        bar.push_history("first");
        bar.push_history("second");
        bar.handle_key(InputKey::HistoryPrev);
        bar.handle_key(InputKey::HistoryPrev);
        assert_eq!(bar.buffer(), "first");
    }

    #[test]
    fn history_next_returns_to_live_buffer() {
        let mut bar = bare_bar();
        bar.push_history("cmd");
        // Set a live buffer first.
        bar.handle_key(InputKey::Char('l'));
        bar.handle_key(InputKey::Char('i'));
        bar.handle_key(InputKey::Char('v'));
        bar.handle_key(InputKey::Char('e'));
        bar.handle_key(InputKey::HistoryPrev); // → "cmd"
        bar.handle_key(InputKey::HistoryNext); // → "live"
        assert_eq!(bar.buffer(), "live");
        assert!(bar.history_pos.is_none());
    }

    #[test]
    fn history_prev_on_empty_is_no_op() {
        let mut bar = bare_bar();
        bar.handle_key(InputKey::HistoryPrev); // must not panic
        assert!(bar.buffer().is_empty());
    }

    #[test]
    fn push_history_deduplicates_consecutive() {
        let mut bar = bare_bar();
        bar.push_history("dup");
        bar.push_history("dup");
        assert_eq!(bar.history.len(), 1);
    }

    #[test]
    fn enter_pushes_to_history() {
        let mut bar = bare_bar();
        bar.handle_key(InputKey::Char('g'));
        bar.handle_key(InputKey::Enter);
        assert_eq!(bar.history, vec!["g"]);
    }

    // ── Quad rendering ────────────────────────────────────────────────────────

    #[test]
    fn render_quads_emits_two_quads() {
        let bar = bare_bar();
        let quads = bar.render_quads(&input_rect());
        assert_eq!(quads.len(), 2, "background + cursor block");
    }

    #[test]
    fn background_quad_uses_surface_recessed() {
        let ctx = RenderCtx::fallback();
        let t = Tokens::phosphor(ctx);
        let bar = bare_bar();
        let quads = bar.render_quads(&input_rect());
        assert_eq!(quads[0].color, t.colors.surface_recessed);
    }

    #[test]
    fn cursor_quad_uses_text_primary_when_visible() {
        // Blink starts visible (phase = 0).
        let ctx = RenderCtx::fallback();
        let t = Tokens::phosphor(ctx);
        let bar = bare_bar();
        let quads = bar.render_quads(&input_rect());
        assert_eq!(quads[1].color, t.colors.text_primary);
    }

    // ── Text rendering ────────────────────────────────────────────────────────

    #[test]
    fn empty_bar_without_prompt_emits_no_text() {
        let bar = bare_bar();
        assert!(bar.render_text(&input_rect()).is_empty());
    }

    #[test]
    fn prompt_only_emits_one_segment() {
        let bar = prompted_bar();
        let texts = bar.render_text(&input_rect());
        assert_eq!(texts.len(), 1);
        assert_eq!(texts[0].text, "> ");
    }

    #[test]
    fn prompt_uses_text_dim_color() {
        let ctx = RenderCtx::fallback();
        let t = Tokens::phosphor(ctx);
        let bar = prompted_bar();
        let texts = bar.render_text(&input_rect());
        assert_eq!(texts[0].color, t.colors.text_dim);
    }

    #[test]
    fn buffer_text_uses_text_primary() {
        let ctx = RenderCtx::fallback();
        let t = Tokens::phosphor(ctx);
        let mut bar = bare_bar();
        bar.handle_key(InputKey::Char('x'));
        let texts = bar.render_text(&input_rect());
        assert_eq!(texts.len(), 1);
        assert_eq!(texts[0].color, t.colors.text_primary);
    }

    #[test]
    fn prompted_bar_with_text_emits_two_segments() {
        let mut bar = prompted_bar();
        bar.handle_key(InputKey::Char('a'));
        let texts = bar.render_text(&input_rect());
        assert_eq!(texts.len(), 2);
        assert!(texts.iter().any(|s| s.text == "> "));
        assert!(texts.iter().any(|s| s.text == "a"));
    }

    // ── Height constant ───────────────────────────────────────────────────────

    #[test]
    fn height_constant_defined() {
        assert!(INPUT_BAR_HEIGHT > 0.0);
    }
}
