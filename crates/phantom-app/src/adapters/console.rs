//! Console adapter — backtick drop-down REPL.
//!
//! Captures keystrokes into a current-line buffer, accepts `Enter` to commit
//! a line into the scrollback, and surfaces output rows that other code
//! pushes via `accept_command "out"`. The actual command dispatch lives at
//! the App level — the adapter just owns the buffer and the visual surface.

use std::collections::VecDeque;

use serde_json::json;

use phantom_adapter::adapter::{QuadData, Rect, RenderOutput, TextData};
use phantom_adapter::spatial::{InternalLayout, SpatialPreference};
use phantom_adapter::{
    AppCore, BusParticipant, Commandable, InputHandler, Lifecycled, Permissioned, Renderable,
};
use phantom_ui::tokens::Tokens;
use phantom_ui::widgets::AppHead;
use phantom_ui::RenderCtx;

const MAX_SCROLLBACK: usize = 200;
const VISIBLE_ROWS: usize = 16;

/// Kind of a scrollback row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleLineKind {
    /// User-typed input echoed back with the prompt prefix.
    Input,
    /// Output written by the dispatch layer.
    Out,
    /// Error / denial.
    Err,
}

/// One row in the console scrollback.
#[derive(Debug, Clone)]
pub struct ConsoleLine {
    pub kind: ConsoleLineKind,
    pub text: String,
}

/// The console pane.
pub struct ConsoleAdapter {
    scrollback: VecDeque<ConsoleLine>,
    /// Pending submissions — drained by the App so it can dispatch them.
    pending_input: Vec<String>,
    current: String,
    tokens: Tokens,
    app_id: u32,
}

impl ConsoleAdapter {
    /// Build an empty console.
    #[must_use]
    pub fn new() -> Self {
        Self {
            scrollback: VecDeque::with_capacity(MAX_SCROLLBACK),
            pending_input: Vec::new(),
            current: String::new(),
            tokens: Tokens::phosphor(RenderCtx::fallback()),
            app_id: 0,
        }
    }

    /// Update the live color palette. The host App calls this on theme switch.
    pub fn set_tokens(&mut self, tokens: Tokens) {
        self.tokens = tokens;
    }

    fn push_line(&mut self, line: ConsoleLine) {
        if self.scrollback.len() == MAX_SCROLLBACK {
            self.scrollback.pop_front();
        }
        self.scrollback.push_back(line);
    }

    /// Drain pending input lines (called by the App after dispatch).
    pub fn drain_pending_input(&mut self) -> Vec<String> {
        std::mem::take(&mut self.pending_input)
    }

    /// Current edit buffer (for tests / inspectors).
    #[must_use]
    pub fn current(&self) -> &str {
        &self.current
    }

    /// Row count of the scrollback ring.
    #[must_use]
    pub fn line_count(&self) -> usize {
        self.scrollback.len()
    }
}

impl Default for ConsoleAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl AppCore for ConsoleAdapter {
    fn app_type(&self) -> &str {
        "console"
    }

    fn is_alive(&self) -> bool {
        true
    }

    fn update(&mut self, _dt: f32) {}

    fn get_state(&self) -> serde_json::Value {
        json!({
            "type": "console",
            "scrollback": self.scrollback.len(),
            "current_len": self.current.len(),
            "pending_input": self.pending_input.len(),
        })
    }

    fn title(&self) -> &str {
        "Console"
    }
}

impl Renderable for ConsoleAdapter {
    fn render(&self, rect: &Rect) -> RenderOutput {
        let mut quads: Vec<QuadData> = Vec::new();
        let mut text_segments: Vec<TextData> = Vec::new();
        let t = self.tokens;

        let head = AppHead::new("CONSOLE", "drop-down repl")
            .with_icon("›")
            .with_meta("`")
            .with_tokens(t);
        head.render_into_adapter(rect, &mut quads, &mut text_segments);

        let body = head.body_rect_adapter(rect);
        let cell_w = if rect.cell_size.0 > 0.0 { rect.cell_size.0 } else { 8.0 };
        let cell_h = if rect.cell_size.1 > 0.0 { rect.cell_size.1 } else { 16.0 };

        let body_pad_x = cell_w;
        let mut y = body.y + cell_h * 0.5;

        // Scrollback — newest at the bottom; render oldest-first into the
        // available space, then the input row at the bottom.
        let total = self.scrollback.len();
        let take = total.min(VISIBLE_ROWS - 1); // reserve one row for the prompt
        let skip = total.saturating_sub(take);

        let prompt_color = t.colors.text_accent;
        let out_color = t.colors.text_secondary;
        let err_color = t.colors.status_danger;

        for line in self.scrollback.iter().skip(skip) {
            if y + cell_h > body.y + body.height - cell_h {
                break;
            }
            let color = match line.kind {
                ConsoleLineKind::Input => prompt_color,
                ConsoleLineKind::Out => out_color,
                ConsoleLineKind::Err => err_color,
            };
            text_segments.push(TextData {
                text: line.text.clone(),
                x: body.x + body_pad_x,
                y,
                color,
            });
            y += cell_h;
        }

        // Live prompt at the bottom.
        let prompt_y = body.y + body.height - cell_h * 1.2;
        text_segments.push(TextData {
            text: format!("phantom> {}_", self.current),
            x: body.x + body_pad_x,
            y: prompt_y,
            color: prompt_color,
        });

        RenderOutput {
            quads,
            text_segments,
            grid: None,
            scroll: None,
            selection: None,
        }
    }

    fn is_visual(&self) -> bool {
        true
    }

    fn spatial_preference(&self) -> Option<SpatialPreference> {
        Some(SpatialPreference {
            min_size: (40, 8),
            preferred_size: (80, 16),
            max_size: None,
            aspect_ratio: None,
            internal_panes: 1,
            internal_layout: InternalLayout::Single,
            priority: 4.0,
        })
    }
}

impl InputHandler for ConsoleAdapter {
    fn handle_input(&mut self, key: &str) -> bool {
        match key {
            "Enter" => {
                if !self.current.is_empty() {
                    let echo = format!("phantom> {}", self.current);
                    let line = std::mem::take(&mut self.current);
                    self.push_line(ConsoleLine {
                        kind: ConsoleLineKind::Input,
                        text: echo,
                    });
                    self.pending_input.push(line);
                }
                true
            }
            "Backspace" => {
                self.current.pop();
                true
            }
            "Escape" => {
                self.current.clear();
                true
            }
            other => {
                // Accept single-char keys and multi-char paste payloads.
                // Any string containing a control character is treated as
                // a named key we don't recognise and not appended.
                if !other.is_empty()
                    && !other.chars().any(|c| c.is_control())
                {
                    self.current.push_str(other);
                    true
                } else {
                    false
                }
            }
        }
    }

    fn accepts_input(&self) -> bool {
        true
    }
}

impl Commandable for ConsoleAdapter {
    fn accept_command(&mut self, cmd: &str, args: &serde_json::Value) -> anyhow::Result<String> {
        match cmd {
            "set_theme_name" => {
                let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("");
                if let Some(tokens) = Tokens::for_theme_name(name, RenderCtx::fallback()) {
                    self.set_tokens(tokens);
                }
                Ok(json!({ "status": "ok" }).to_string())
            }
            "drain_pending" => {
                let lines = self.drain_pending_input();
                Ok(serde_json::to_string(&lines).unwrap_or_else(|_| "[]".to_string()))
            }
            "out" => {
                let text = args
                    .get("text")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("missing field: text"))?;
                self.push_line(ConsoleLine {
                    kind: ConsoleLineKind::Out,
                    text: text.to_string(),
                });
                Ok(json!({ "status": "ok" }).to_string())
            }
            "err" => {
                let text = args
                    .get("text")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("missing field: text"))?;
                self.push_line(ConsoleLine {
                    kind: ConsoleLineKind::Err,
                    text: text.to_string(),
                });
                Ok(json!({ "status": "ok" }).to_string())
            }
            "clear" => {
                self.scrollback.clear();
                self.current.clear();
                Ok(json!({ "status": "ok" }).to_string())
            }
            "snapshot" => Ok(self.get_state().to_string()),
            other => Err(anyhow::anyhow!("unknown command: {other}")),
        }
    }
}

impl BusParticipant for ConsoleAdapter {}

impl Lifecycled for ConsoleAdapter {
    fn set_app_id(&mut self, id: u32) {
        self.app_id = id;
    }
}

impl Permissioned for ConsoleAdapter {}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect() -> Rect {
        Rect {
            x: 0.0,
            y: 0.0,
            width: 800.0,
            height: 400.0,
            cell_size: (8.0, 16.0),
        }
    }

    #[test]
    fn app_type_is_console() {
        assert_eq!(ConsoleAdapter::new().app_type(), "console");
    }

    #[test]
    fn typing_keys_appends_to_current_buffer() {
        let mut c = ConsoleAdapter::new();
        c.handle_input("a");
        c.handle_input("b");
        c.handle_input("c");
        assert_eq!(c.current(), "abc");
    }

    #[test]
    fn backspace_drops_last_char() {
        let mut c = ConsoleAdapter::new();
        c.handle_input("h");
        c.handle_input("i");
        c.handle_input("Backspace");
        assert_eq!(c.current(), "h");
    }

    #[test]
    fn enter_submits_line_and_queues_pending() {
        let mut c = ConsoleAdapter::new();
        for ch in "theme amber".chars() {
            c.handle_input(&ch.to_string());
        }
        c.handle_input("Enter");
        assert_eq!(c.current(), "");
        let pending = c.drain_pending_input();
        assert_eq!(pending, vec!["theme amber".to_string()]);
        assert_eq!(c.line_count(), 1);
    }

    #[test]
    fn out_command_appends_output_row() {
        let mut c = ConsoleAdapter::new();
        c.accept_command("out", &json!({ "text": "hello" })).unwrap();
        assert_eq!(c.line_count(), 1);
    }

    #[test]
    fn clear_drains_scrollback_and_buffer() {
        let mut c = ConsoleAdapter::new();
        c.handle_input("x");
        c.accept_command("out", &json!({ "text": "y" })).unwrap();
        c.accept_command("clear", &json!({})).unwrap();
        assert_eq!(c.line_count(), 0);
        assert_eq!(c.current(), "");
    }

    #[test]
    fn renders_prompt_row() {
        let c = ConsoleAdapter::new();
        let out = c.render(&rect());
        assert!(out.text_segments.iter().any(|t| t.text.starts_with("phantom>")));
    }

    #[test]
    fn paste_multi_char_string_is_appended() {
        let mut c = ConsoleAdapter::new();
        // Mimic a paste event that delivers the whole string in one call.
        assert!(c.handle_input("theme amber"));
        assert_eq!(c.current(), "theme amber");
    }

    #[test]
    fn control_chars_are_not_appended() {
        let mut c = ConsoleAdapter::new();
        assert!(!c.handle_input("\t")); // tab is a control char
        assert_eq!(c.current(), "");
    }

    #[test]
    fn set_app_id_stores_id() {
        let mut c = ConsoleAdapter::new();
        c.set_app_id(42);
        assert_eq!(c.app_id, 42);
    }

    #[test]
    fn theme_swap_propagates_to_prompt_color() {
        use phantom_ui::tokens::ColorRoles;
        let mut c = ConsoleAdapter::new();
        let out_p = c.render(&rect());
        let prompt_p = out_p
            .text_segments
            .iter()
            .find(|t| t.text.starts_with("phantom>"))
            .map(|t| t.color)
            .expect("prompt must render");

        let mut roles = ColorRoles::phosphor();
        roles.text_accent = [0.0, 0.0, 1.0, 1.0];
        c.set_tokens(Tokens::new(roles, RenderCtx::fallback()));
        let out_b = c.render(&rect());
        let prompt_b = out_b
            .text_segments
            .iter()
            .find(|t| t.text.starts_with("phantom>"))
            .map(|t| t.color)
            .expect("prompt must render");

        assert_ne!(prompt_p, prompt_b);
        assert!(prompt_b[2] > 0.9);
    }
}
