//! Quake-style drop-down console.
//!
//! Press backtick (`) to toggle. Drops from the top of the screen, shows
//! command history with outputs, and captures all input while open.

/// A single line in the console scrollback buffer.
#[derive(Debug, Clone)]
pub(crate) enum ConsoleLine {
    /// User-entered command (rendered with `> ` prefix).
    Command(String),
    /// Normal output from a command.
    Output(String),
    /// Error message.
    Error(String),
    /// System/info message.
    System(String),
}

/// Known commands for tab completion.
pub(crate) const COMMANDS: &[&str] = &[
    "agent", "appmon", "boot", "clear", "debug", "exit", "help",
    "goal", "inspect", "plain", "plugins", "quit", "reload", "selfheal", "selftest", "set", "suggestions", "sysmon", "theme", "video",
];

/// Quake-style drop-down console state.
pub(crate) struct Console {
    /// Whether the console is visible (or animating).
    pub open: bool,
    /// Current input buffer.
    pub input: String,
    /// Scrollback history (commands + their outputs).
    pub history: Vec<ConsoleLine>,
    /// Scroll offset from the bottom (0 = viewing latest).
    pub scroll_offset: usize,
    /// Previous commands for Up/Down recall.
    pub command_history: Vec<String>,
    /// Index into command_history for recall.
    pub history_index: Option<usize>,
    /// Saved input when browsing command history.
    pub saved_input: String,
    /// Max scrollback lines before oldest are dropped.
    pub max_scrollback: usize,
    /// Slide animation progress: 0.0 = fully closed, 1.0 = fully open.
    pub slide: f32,
    /// Tab completion: index into current matches (-1 = none).
    pub tab_index: Option<usize>,
    /// Tab completion: cached matches for the current prefix.
    pub tab_matches: Vec<String>,
}

impl Console {
    pub fn new() -> Self {
        Self {
            open: false,
            input: String::new(),
            history: Vec::with_capacity(256),
            scroll_offset: 0,
            command_history: Vec::with_capacity(64),
            history_index: None,
            saved_input: String::new(),
            max_scrollback: 2000,
            slide: 0.0,
            tab_index: None,
            tab_matches: Vec::new(),
        }
    }

    /// Toggle the console open/closed.
    pub fn toggle(&mut self) {
        self.open = !self.open;
        if self.open {
            self.input.clear();
            self.scroll_offset = 0;
            self.history_index = None;
            self.clear_tab();
        }
    }

    /// Returns true if the console is visible (open or still animating closed).
    pub fn visible(&self) -> bool {
        self.open || self.slide > 0.001
    }

    /// Advance the slide animation toward the target. Call once per frame.
    /// `dt` is seconds since last frame. Returns true if still animating.
    pub fn animate(&mut self, dt: f32) -> bool {
        let target = if self.open { 1.0 } else { 0.0 };
        if (self.slide - target).abs() < 0.001 {
            self.slide = target;
            return false;
        }
        // Smooth lerp: ~200ms open, ~150ms close (close is snappier).
        let speed = if self.open { 8.0 } else { 10.0 };
        self.slide += (target - self.slide) * (speed * dt).min(1.0);
        true
    }

    /// Submit the current input. Returns the command string if non-empty.
    pub fn submit(&mut self) -> Option<String> {
        self.clear_tab();
        let cmd = self.input.trim().to_string();
        if cmd.is_empty() {
            return None;
        }
        self.input.clear();
        self.scroll_offset = 0;
        self.history_index = None;

        if self.command_history.last().map_or(true, |last| last != &cmd) {
            self.command_history.push(cmd.clone());
        }

        self.push(ConsoleLine::Command(cmd.clone()));
        Some(cmd)
    }

    /// Push a line to the scrollback, trimming if over max.
    pub fn push(&mut self, line: ConsoleLine) {
        self.history.push(line);
        if self.history.len() > self.max_scrollback {
            let excess = self.history.len() - self.max_scrollback;
            self.history.drain(..excess);
        }
    }

    pub fn output(&mut self, text: impl Into<String>) {
        self.push(ConsoleLine::Output(text.into()));
    }

    pub fn error(&mut self, text: impl Into<String>) {
        self.push(ConsoleLine::Error(text.into()));
    }

    pub fn system(&mut self, text: impl Into<String>) {
        self.push(ConsoleLine::System(text.into()));
    }

    /// Navigate command history upward (older).
    pub fn history_up(&mut self) {
        self.clear_tab();
        if self.command_history.is_empty() {
            return;
        }
        match self.history_index {
            None => {
                self.saved_input = self.input.clone();
                let idx = self.command_history.len() - 1;
                self.history_index = Some(idx);
                self.input = self.command_history[idx].clone();
            }
            Some(idx) if idx > 0 => {
                let new_idx = idx - 1;
                self.history_index = Some(new_idx);
                self.input = self.command_history[new_idx].clone();
            }
            _ => {}
        }
    }

    /// Navigate command history downward (newer).
    pub fn history_down(&mut self) {
        self.clear_tab();
        match self.history_index {
            Some(idx) => {
                if idx + 1 < self.command_history.len() {
                    let new_idx = idx + 1;
                    self.history_index = Some(new_idx);
                    self.input = self.command_history[new_idx].clone();
                } else {
                    self.history_index = None;
                    self.input = self.saved_input.clone();
                }
            }
            None => {}
        }
    }

    /// Scroll the view up by N lines.
    pub fn scroll_up(&mut self, lines: usize) {
        let max = self.history.len().saturating_sub(1);
        self.scroll_offset = (self.scroll_offset + lines).min(max);
    }

    /// Scroll the view down by N lines.
    pub fn scroll_down(&mut self, lines: usize) {
        self.scroll_offset = self.scroll_offset.saturating_sub(lines);
    }

    // -- Tab completion -------------------------------------------------------

    /// Cycle to the next tab completion match.
    pub fn tab_complete(&mut self) {
        let prefix = self.input.split_whitespace().next().unwrap_or("").to_lowercase();

        // If we have no matches or the prefix changed, rebuild.
        if self.tab_matches.is_empty() || self.tab_index.is_none() {
            self.tab_matches = COMMANDS
                .iter()
                .filter(|cmd| cmd.starts_with(&prefix) && **cmd != prefix)
                .map(|s| s.to_string())
                .collect();

            // Also match against command history.
            for cmd in self.command_history.iter().rev() {
                let first_word = cmd.split_whitespace().next().unwrap_or("");
                if first_word.starts_with(&prefix) && !self.tab_matches.contains(&first_word.to_string()) {
                    self.tab_matches.push(first_word.to_string());
                }
            }

            if self.tab_matches.is_empty() {
                return;
            }
            self.tab_index = Some(0);
        } else if let Some(idx) = self.tab_index {
            self.tab_index = Some((idx + 1) % self.tab_matches.len());
        }

        if let Some(idx) = self.tab_index {
            // Replace the first word in input with the match.
            let rest: String = self.input
                .split_whitespace()
                .skip(1)
                .collect::<Vec<_>>()
                .join(" ");
            if rest.is_empty() {
                self.input = format!("{} ", self.tab_matches[idx]);
            } else {
                self.input = format!("{} {}", self.tab_matches[idx], rest);
            }
        }
    }

    /// Clear tab completion state (called on any non-Tab input).
    pub fn clear_tab(&mut self) {
        self.tab_index = None;
        self.tab_matches.clear();
    }
}

// ---------------------------------------------------------------------------
// Issue #176 — Command history: Up/Down recall with LIFO ordering
// ---------------------------------------------------------------------------

#[cfg(test)]
mod history_tests {
    use super::*;

    /// Build a console that has already submitted "first", "second", "third".
    fn console_with_3_commands() -> Console {
        let mut c = Console::new();
        c.input = "first".into();
        c.submit();
        c.input = "second".into();
        c.submit();
        c.input = "third".into();
        c.submit();
        c
    }

    // Up once → most recent command.
    #[test]
    fn history_up_once_returns_last_command() {
        let mut c = console_with_3_commands();
        c.history_up();
        assert_eq!(c.input, "third");
    }

    // Up twice → second-most recent.
    #[test]
    fn history_up_twice_returns_second_command() {
        let mut c = console_with_3_commands();
        c.history_up();
        c.history_up();
        assert_eq!(c.input, "second");
    }

    // Up three times → oldest command.
    #[test]
    fn history_up_three_times_returns_first_command() {
        let mut c = console_with_3_commands();
        c.history_up();
        c.history_up();
        c.history_up();
        assert_eq!(c.input, "first");
    }

    // Up past the oldest entry must stay at the oldest without panicking.
    #[test]
    fn history_up_past_oldest_stays_at_first_no_panic() {
        let mut c = console_with_3_commands();
        for _ in 0..10 {
            c.history_up();
        }
        assert_eq!(c.input, "first", "must clamp at oldest entry");
    }

    // Up then Down restores the saved draft.
    #[test]
    fn history_up_then_down_restores_input() {
        let mut c = console_with_3_commands();
        c.input = "draft".into();
        c.history_up();
        assert_eq!(c.input, "third");
        c.history_down();
        assert_eq!(c.input, "draft", "Down from newest must restore draft");
    }

    // Up×3 Down×1 → "second".
    #[test]
    fn history_down_moves_forward() {
        let mut c = console_with_3_commands();
        c.history_up();
        c.history_up();
        c.history_up();
        c.history_down();
        assert_eq!(c.input, "second");
    }

    // Down all the way back restores the draft.
    #[test]
    fn history_down_to_end_restores_draft() {
        let mut c = console_with_3_commands();
        c.input = "new draft".into();
        c.history_up();
        c.history_up();
        c.history_up();
        c.history_down();
        c.history_down();
        c.history_down();
        assert_eq!(c.input, "new draft");
    }

    // history_up on empty history is a no-op, no panic.
    #[test]
    fn history_up_on_empty_history_is_no_op_no_panic() {
        let mut c = Console::new();
        c.history_up(); // must not panic
        assert!(c.command_history.is_empty());
    }

    // history_down when not navigating must not panic or change input.
    #[test]
    fn history_down_when_not_navigating_is_no_op() {
        let mut c = console_with_3_commands();
        c.input = "current".into();
        c.history_down(); // not in navigation mode — no-op
        assert_eq!(c.input, "current");
    }

    // Duplicate adjacent commands are deduplicated.
    #[test]
    fn duplicate_adjacent_commands_deduplicated() {
        let mut c = Console::new();
        c.input = "build".into();
        c.submit();
        c.input = "build".into();
        c.submit();
        assert_eq!(c.command_history.len(), 1, "identical adjacent commands must not be duplicated");
    }

    // submit() returns the trimmed command string.
    #[test]
    fn submit_returns_trimmed_command() {
        let mut c = Console::new();
        c.input = "  cargo test  ".into();
        let result = c.submit();
        assert_eq!(result, Some("cargo test".into()));
    }

    // submit() clears the input buffer.
    #[test]
    fn submit_clears_input() {
        let mut c = Console::new();
        c.input = "something".into();
        c.submit();
        assert!(c.input.is_empty(), "input must be empty after submit");
    }

    // submit() with only whitespace returns None and does not add to history.
    #[test]
    fn submit_empty_returns_none() {
        let mut c = Console::new();
        c.input = "   ".into();
        let result = c.submit();
        assert_eq!(result, None);
        assert!(c.command_history.is_empty());
    }
}
