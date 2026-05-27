//! File-watch adapter — render a tail of recently-modified files.
//!
//! Adapter owns a ring buffer of `FileChange` events. A real consumer wires
//! `notify`/`FSEvents` callbacks to push events here; the adapter is the
//! visible surface.

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

const MAX_HISTORY: usize = 200;
const VISIBLE_ROWS: usize = 16;

/// Type of filesystem change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileChangeKind {
    Modified,
    Created,
    Deleted,
}

impl FileChangeKind {
    /// Single-glyph marker rendered in the left column.
    pub fn marker(self) -> &'static str {
        match self {
            Self::Modified => "M",
            Self::Created => "+",
            Self::Deleted => "D",
        }
    }
    /// Resolve the marker color from the active token palette.
    fn color(self, t: &Tokens) -> [f32; 4] {
        match self {
            Self::Modified => t.colors.status_ok,
            Self::Created => t.colors.status_info,
            Self::Deleted => t.colors.status_danger,
        }
    }
}

/// Single change event.
#[derive(Debug, Clone)]
pub struct FileChange {
    pub kind: FileChangeKind,
    pub path: String,
    /// Wall clock label (`HH:MM` is fine).
    pub stamp: String,
}

impl FileChange {
    /// Convenience constructor with an empty stamp.
    #[must_use]
    pub fn new(kind: FileChangeKind, path: impl Into<String>) -> Self {
        Self {
            kind,
            path: path.into(),
            stamp: String::new(),
        }
    }

    /// Builder: attach a timestamp string.
    #[must_use]
    pub fn with_stamp(mut self, stamp: impl Into<String>) -> Self {
        self.stamp = stamp.into();
        self
    }
}

/// The file-watch pane.
pub struct FilesWatchAdapter {
    history: VecDeque<FileChange>,
    tokens: Tokens,
    app_id: u32,
}

impl FilesWatchAdapter {
    /// Build empty.
    #[must_use]
    pub fn new() -> Self {
        Self {
            history: VecDeque::with_capacity(MAX_HISTORY),
            tokens: Tokens::phosphor(RenderCtx::fallback()),
            app_id: 0,
        }
    }

    /// Update the live color palette. The host App calls this on theme switch.
    pub fn set_tokens(&mut self, tokens: Tokens) {
        self.tokens = tokens;
    }

    /// Append a change.
    pub fn push(&mut self, change: FileChange) {
        if self.history.len() == MAX_HISTORY {
            self.history.pop_front();
        }
        self.history.push_back(change);
    }

    /// Count.
    #[must_use]
    pub fn len(&self) -> usize {
        self.history.len()
    }

    /// `true` when no changes recorded yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.history.is_empty()
    }
}

impl Default for FilesWatchAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl AppCore for FilesWatchAdapter {
    fn app_type(&self) -> &str {
        "files-watch"
    }

    fn is_alive(&self) -> bool {
        true
    }

    fn update(&mut self, _dt: f32) {}

    fn get_state(&self) -> serde_json::Value {
        json!({
            "type": "files-watch",
            "changes": self.history.len(),
        })
    }

    fn title(&self) -> &str {
        "Watch"
    }
}

impl Renderable for FilesWatchAdapter {
    fn render(&self, rect: &Rect) -> RenderOutput {
        let mut quads: Vec<QuadData> = Vec::new();
        let mut text_segments: Vec<TextData> = Vec::new();
        let t = self.tokens;

        let head = AppHead::new("WATCH", "file changes")
            .with_icon("◫")
            .with_meta(format!("{}", self.history.len()))
            .with_tokens(t);
        head.render_into_adapter(rect, &mut quads, &mut text_segments);

        let body = head.body_rect_adapter(rect);
        let cell_w = if rect.cell_size.0 > 0.0 { rect.cell_size.0 } else { 8.0 };
        let cell_h = if rect.cell_size.1 > 0.0 { rect.cell_size.1 } else { 16.0 };
        let mut y = body.y + cell_h * 0.3;

        let path_color = t.colors.text_primary;
        let stamp_color = t.colors.text_dim;

        // Render oldest→newest of the most-recent `VISIBLE_ROWS`. The slice
        // is computed from the front of the ring so we don't double-allocate.
        let total = self.history.len();
        let skip = total.saturating_sub(VISIBLE_ROWS);
        for change in self.history.iter().skip(skip) {
            if y + cell_h > body.y + body.height {
                break;
            }
            text_segments.push(TextData {
                text: change.kind.marker().to_string(),
                x: body.x + cell_w,
                y,
                color: change.kind.color(&t),
            });
            text_segments.push(TextData {
                text: change.path.clone(),
                x: body.x + cell_w * 3.0,
                y,
                color: path_color,
            });
            if !change.stamp.is_empty() {
                let stamp_x = body.x + body.width - cell_w * 8.0;
                text_segments.push(TextData {
                    text: format!("· {}", change.stamp),
                    x: stamp_x,
                    y,
                    color: stamp_color,
                });
            }
            y += cell_h;
        }

        if self.history.is_empty() {
            text_segments.push(TextData {
                text: "  (no changes recorded)".to_string(),
                x: body.x + cell_w,
                y,
                color: stamp_color,
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
            min_size: (40, 8),
            preferred_size: (60, 20),
            max_size: None,
            aspect_ratio: None,
            internal_panes: 1,
            internal_layout: InternalLayout::Single,
            priority: 2.0,
        })
    }
}

impl InputHandler for FilesWatchAdapter {
    fn handle_input(&mut self, _key: &str) -> bool {
        false
    }

    fn accepts_input(&self) -> bool {
        false
    }
}

impl Commandable for FilesWatchAdapter {
    fn accept_command(&mut self, cmd: &str, args: &serde_json::Value) -> anyhow::Result<String> {
        match cmd {
            "set_theme_name" => {
                let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("");
                if let Some(tokens) = Tokens::for_theme_name(name, RenderCtx::fallback()) {
                    self.set_tokens(tokens);
                }
                Ok(json!({ "status": "ok" }).to_string())
            }
            "push" => {
                let path = args
                    .get("path")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("missing field: path"))?;
                let kind = match args.get("kind").and_then(|v| v.as_str()) {
                    Some("created") | Some("+") => FileChangeKind::Created,
                    Some("deleted") | Some("D") => FileChangeKind::Deleted,
                    _ => FileChangeKind::Modified,
                };
                let stamp = args
                    .get("stamp")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                self.push(FileChange::new(kind, path).with_stamp(stamp));
                Ok(json!({ "status": "ok" }).to_string())
            }
            "clear" => {
                self.history.clear();
                Ok(json!({ "status": "ok" }).to_string())
            }
            "snapshot" => Ok(self.get_state().to_string()),
            other => Err(anyhow::anyhow!("unknown command: {other}")),
        }
    }
}

impl BusParticipant for FilesWatchAdapter {}

impl Lifecycled for FilesWatchAdapter {
    fn set_app_id(&mut self, id: u32) {
        self.app_id = id;
    }
}

impl Permissioned for FilesWatchAdapter {}

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
    fn app_type_is_files_watch() {
        assert_eq!(FilesWatchAdapter::new().app_type(), "files-watch");
    }

    #[test]
    fn push_appends_and_caps() {
        let mut a = FilesWatchAdapter::new();
        for i in 0..(MAX_HISTORY + 5) {
            a.push(FileChange::new(FileChangeKind::Modified, format!("p{i}")));
        }
        assert_eq!(a.len(), MAX_HISTORY);
    }

    #[test]
    fn renders_marker_and_path_per_row() {
        let mut a = FilesWatchAdapter::new();
        a.push(FileChange::new(FileChangeKind::Modified, "a.rs"));
        a.push(FileChange::new(FileChangeKind::Created, "b.rs"));
        a.push(FileChange::new(FileChangeKind::Deleted, "c.rs"));
        let out = a.render(&rect());
        assert!(out.text_segments.iter().any(|t| t.text == "a.rs"));
        assert!(out.text_segments.iter().any(|t| t.text == "M"));
        assert!(out.text_segments.iter().any(|t| t.text == "+"));
        assert!(out.text_segments.iter().any(|t| t.text == "D"));
    }

    #[test]
    fn empty_renders_hint() {
        let a = FilesWatchAdapter::new();
        let out = a.render(&rect());
        assert!(out.text_segments.iter().any(|t| t.text.contains("no changes")));
    }

    #[test]
    fn push_command_accepts_kind_alias() {
        let mut a = FilesWatchAdapter::new();
        a.accept_command("push", &json!({ "path": "x.rs", "kind": "+" }))
            .unwrap();
        assert_eq!(a.len(), 1);
    }

    #[test]
    fn set_app_id_stores_id() {
        let mut a = FilesWatchAdapter::new();
        a.set_app_id(42);
        assert_eq!(a.app_id, 42);
    }

    #[test]
    fn theme_swap_propagates_to_marker_color() {
        use phantom_ui::tokens::ColorRoles;
        let mut a = FilesWatchAdapter::new();
        a.push(FileChange::new(FileChangeKind::Modified, "x.rs"));
        let out_p = a.render(&rect());
        let marker_p = out_p
            .text_segments
            .iter()
            .find(|t| t.text == "M")
            .map(|t| t.color)
            .expect("M marker must render");

        let mut roles = ColorRoles::phosphor();
        roles.status_ok = [0.0, 0.0, 1.0, 1.0];
        a.set_tokens(Tokens::new(roles, RenderCtx::fallback()));
        let out_b = a.render(&rect());
        let marker_b = out_b
            .text_segments
            .iter()
            .find(|t| t.text == "M")
            .map(|t| t.color)
            .expect("M marker must render");

        assert_ne!(marker_p, marker_b);
        assert!(marker_b[2] > 0.9);
    }
}
