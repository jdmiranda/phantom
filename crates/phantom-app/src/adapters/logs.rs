//! Logs adapter — tail-style log viewer with level + source coloring.
//!
//! Holds an in-memory ring of `LogRow` entries. The App is responsible for
//! pushing rows from the actual log sink (file tail, tracing layer, etc.);
//! this adapter just paints them.

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

const MAX_ROWS: usize = 1000;
const VISIBLE_ROWS: usize = 24;

/// Log severity levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    /// Parse a level from its short name (case-insensitive). Unknown → `Info`.
    #[must_use]
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_uppercase().as_str() {
            "TRACE" => Self::Trace,
            "DEBUG" | "DBG" => Self::Debug,
            "WARN" => Self::Warn,
            "ERROR" | "ERR" => Self::Error,
            _ => Self::Info,
        }
    }

    /// Short label rendered in the level chip column.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Trace => "TRC",
            Self::Debug => "DBG",
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERR",
        }
    }

    /// Resolve the level chip color from the active token palette.
    fn color(self, t: &Tokens) -> [f32; 4] {
        match self {
            Self::Trace | Self::Debug => t.colors.text_secondary,
            Self::Info => t.colors.status_info,
            Self::Warn => t.colors.status_warn,
            Self::Error => t.colors.status_danger,
        }
    }
}

/// One log line.
#[derive(Debug, Clone)]
pub struct LogRow {
    pub level: LogLevel,
    pub source: String,
    pub message: String,
}

impl LogRow {
    /// Convenience constructor.
    #[must_use]
    pub fn new(
        level: LogLevel,
        source: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            level,
            source: source.into(),
            message: message.into(),
        }
    }
}

/// Logs pane.
pub struct LogsAdapter {
    rows: VecDeque<LogRow>,
    /// When set, only rows ≥ this level are rendered.
    min_level: LogLevel,
    tokens: Tokens,
    app_id: u32,
}

impl LogsAdapter {
    /// Build an empty log viewer.
    #[must_use]
    pub fn new() -> Self {
        Self {
            rows: VecDeque::with_capacity(MAX_ROWS),
            min_level: LogLevel::Debug,
            tokens: Tokens::phosphor(RenderCtx::fallback()),
            app_id: 0,
        }
    }

    /// Update the live color palette. The host App calls this on theme switch.
    pub fn set_tokens(&mut self, tokens: Tokens) {
        self.tokens = tokens;
    }

    /// Push a log row, dropping the oldest if at capacity.
    pub fn push(&mut self, row: LogRow) {
        if self.rows.len() == MAX_ROWS {
            self.rows.pop_front();
        }
        self.rows.push_back(row);
    }

    /// Row count.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// True when no rows have been pushed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Current minimum displayed level.
    #[must_use]
    pub fn min_level(&self) -> LogLevel {
        self.min_level
    }
}

impl Default for LogsAdapter {
    fn default() -> Self {
        Self::new()
    }
}

fn level_rank(l: LogLevel) -> u8 {
    match l {
        LogLevel::Trace => 0,
        LogLevel::Debug => 1,
        LogLevel::Info => 2,
        LogLevel::Warn => 3,
        LogLevel::Error => 4,
    }
}

impl AppCore for LogsAdapter {
    fn app_type(&self) -> &str {
        "logs"
    }

    fn is_alive(&self) -> bool {
        true
    }

    fn update(&mut self, _dt: f32) {}

    fn get_state(&self) -> serde_json::Value {
        json!({
            "type": "logs",
            "rows": self.rows.len(),
            "min_level": self.min_level.label(),
        })
    }

    fn title(&self) -> &str {
        "Logs"
    }
}

impl Renderable for LogsAdapter {
    fn render(&self, rect: &Rect) -> RenderOutput {
        let mut quads: Vec<QuadData> = Vec::new();
        let mut text_segments: Vec<TextData> = Vec::new();
        let t = self.tokens;

        let head = AppHead::new("LOGS", "phantom.log · tail")
            .with_icon("≡")
            .with_meta(format!("{} rows", self.rows.len()))
            .with_tokens(t);
        head.render_into_adapter(rect, &mut quads, &mut text_segments);

        let body = head.body_rect_adapter(rect);
        let cell_w = if rect.cell_size.0 > 0.0 { rect.cell_size.0 } else { 8.0 };
        let cell_h = if rect.cell_size.1 > 0.0 { rect.cell_size.1 } else { 16.0 };
        let mut y = body.y + cell_h * 0.3;

        let src_color = t.colors.text_secondary;
        let msg_color = t.colors.text_primary;

        let min_rank = level_rank(self.min_level);
        let filtered: Vec<&LogRow> = self
            .rows
            .iter()
            .filter(|r| level_rank(r.level) >= min_rank)
            .collect();

        for row in filtered.iter().rev().take(VISIBLE_ROWS).rev() {
            if y + cell_h > body.y + body.height {
                break;
            }
            // Level chip
            text_segments.push(TextData {
                text: format!("{:<5}", row.level.label()),
                x: body.x + cell_w,
                y,
                color: row.level.color(&t),
            });
            // Source
            text_segments.push(TextData {
                text: format!("{:<10}", truncate(&row.source, 10)),
                x: body.x + cell_w * 7.0,
                y,
                color: src_color,
            });
            // Message
            text_segments.push(TextData {
                text: row.message.clone(),
                x: body.x + cell_w * 18.0,
                y,
                color: msg_color,
            });
            y += cell_h;
        }

        if self.rows.is_empty() {
            text_segments.push(TextData {
                text: "  (no log entries yet)".to_string(),
                x: body.x + cell_w,
                y,
                color: src_color,
            });
        }

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
            min_size: (50, 10),
            preferred_size: (80, 24),
            max_size: None,
            aspect_ratio: None,
            internal_panes: 1,
            internal_layout: InternalLayout::Single,
            priority: 2.0,
        })
    }
}

impl InputHandler for LogsAdapter {
    fn handle_input(&mut self, _key: &str) -> bool {
        false
    }

    fn accepts_input(&self) -> bool {
        false
    }
}

impl Commandable for LogsAdapter {
    fn accept_command(&mut self, cmd: &str, args: &serde_json::Value) -> anyhow::Result<String> {
        match cmd {
            "push" => {
                let level = LogLevel::parse(
                    args.get("level").and_then(|v| v.as_str()).unwrap_or("info"),
                );
                let source = args.get("source").and_then(|v| v.as_str()).unwrap_or("phantom");
                let message = args
                    .get("message")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("missing field: message"))?;
                self.push(LogRow::new(level, source, message));
                Ok(json!({ "status": "ok" }).to_string())
            }
            "set_min_level" => {
                let level = args
                    .get("level")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("missing field: level"))?;
                self.min_level = LogLevel::parse(level);
                Ok(json!({ "status": "ok", "min_level": self.min_level.label() }).to_string())
            }
            "clear" => {
                self.rows.clear();
                Ok(json!({ "status": "ok" }).to_string())
            }
            "snapshot" => Ok(self.get_state().to_string()),
            other => Err(anyhow::anyhow!("unknown command: {other}")),
        }
    }
}

impl BusParticipant for LogsAdapter {}

impl Lifecycled for LogsAdapter {
    fn set_app_id(&mut self, id: u32) {
        self.app_id = id;
    }
}

impl Permissioned for LogsAdapter {}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect() -> Rect {
        Rect {
            x: 0.0,
            y: 0.0,
            width: 1000.0,
            height: 500.0,
            cell_size: (8.0, 16.0),
        }
    }

    #[test]
    fn log_level_parse_is_case_insensitive() {
        assert_eq!(LogLevel::parse("info"), LogLevel::Info);
        assert_eq!(LogLevel::parse("WARN"), LogLevel::Warn);
        assert_eq!(LogLevel::parse("err"), LogLevel::Error);
        assert_eq!(LogLevel::parse("DBG"), LogLevel::Debug);
    }

    #[test]
    fn push_increments_len_and_caps_at_max() {
        let mut a = LogsAdapter::new();
        for i in 0..(MAX_ROWS + 5) {
            a.push(LogRow::new(LogLevel::Info, "src", format!("m{i}")));
        }
        assert_eq!(a.len(), MAX_ROWS);
    }

    #[test]
    fn min_level_filters_rows() {
        let mut a = LogsAdapter::new();
        a.push(LogRow::new(LogLevel::Debug, "x", "d"));
        a.push(LogRow::new(LogLevel::Error, "x", "e"));
        a.accept_command("set_min_level", &json!({ "level": "warn" }))
            .unwrap();
        let out = a.render(&rect());
        // Only the ERR row should show.
        assert!(out.text_segments.iter().any(|t| t.text == "e"));
        assert!(!out.text_segments.iter().any(|t| t.text == "d"));
    }

    #[test]
    fn empty_renders_hint() {
        let a = LogsAdapter::new();
        let out = a.render(&rect());
        assert!(out.text_segments.iter().any(|t| t.text.contains("no log entries")));
    }

    #[test]
    fn renders_app_head_with_log_count() {
        let mut a = LogsAdapter::new();
        a.push(LogRow::new(LogLevel::Info, "app", "boot ok"));
        let out = a.render(&rect());
        assert!(out.text_segments.iter().any(|t| t.text.contains("rows")));
        assert!(out.text_segments.iter().any(|t| t.text == "LOGS"));
    }

    #[test]
    fn set_app_id_stores_id() {
        let mut a = LogsAdapter::new();
        a.set_app_id(42);
        assert_eq!(a.app_id, 42);
    }

    #[test]
    fn theme_swap_propagates_to_level_chip() {
        use phantom_ui::tokens::ColorRoles;
        let mut a = LogsAdapter::new();
        a.push(LogRow::new(LogLevel::Error, "x", "boom"));
        let out_p = a.render(&rect());
        let chip_p = out_p
            .text_segments
            .iter()
            .find(|t| t.text.trim() == "ERR")
            .map(|t| t.color)
            .expect("err chip must render");

        let mut roles = ColorRoles::phosphor();
        roles.status_danger = [0.0, 0.0, 1.0, 1.0];
        a.set_tokens(Tokens::new(roles, RenderCtx::fallback()));
        let out_b = a.render(&rect());
        let chip_b = out_b
            .text_segments
            .iter()
            .find(|t| t.text.trim() == "ERR")
            .map(|t| t.color)
            .expect("err chip must render");

        assert_ne!(chip_p, chip_b);
        assert!(chip_b[2] > 0.9);
    }
}
