//! Memory inspector — render the per-project memory store as key/value rows.
//!
//! Holds a `Vec<(key, value)>`. The App supplies the data via the `load`
//! command from whatever backing store is current (phantom-memory once it
//! lands, or the auto-memory directory in the meantime).

use serde_json::json;

use phantom_adapter::adapter::{QuadData, Rect, RenderOutput, TextData};
use phantom_adapter::spatial::{InternalLayout, SpatialPreference};
use phantom_adapter::{
    AppCore, BusParticipant, Commandable, InputHandler, Lifecycled, Permissioned, Renderable,
};
use phantom_ui::tokens::Tokens;
use phantom_ui::widgets::AppHead;
use phantom_ui::RenderCtx;

/// Cap on `set_entries` / `load` to prevent OOM if a host caller misbehaves.
pub const MAX_ENTRIES: usize = 1000;

/// One memory entry.
#[derive(Debug, Clone)]
pub struct MemoryEntry {
    pub key: String,
    pub value: String,
}

impl MemoryEntry {
    /// Convenience constructor.
    #[must_use]
    pub fn new(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            value: value.into(),
        }
    }
}

/// Memory pane.
pub struct MemoryInspectorAdapter {
    entries: Vec<MemoryEntry>,
    project: String,
    tokens: Tokens,
    app_id: u32,
}

impl MemoryInspectorAdapter {
    /// Build empty.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            project: "project".into(),
            tokens: Tokens::phosphor(RenderCtx::fallback()),
            app_id: 0,
        }
    }

    /// Update the live color palette. The host App calls this on theme switch.
    pub fn set_tokens(&mut self, tokens: Tokens) {
        self.tokens = tokens;
    }

    /// Replace entries wholesale. Truncates at `MAX_ENTRIES` to bound the
    /// adapter's memory footprint.
    pub fn set_entries(&mut self, mut entries: Vec<MemoryEntry>) {
        if entries.len() > MAX_ENTRIES {
            entries.truncate(MAX_ENTRIES);
        }
        self.entries = entries;
    }

    /// Set the project label rendered in the header title.
    pub fn set_project(&mut self, project: impl Into<String>) {
        self.project = project.into();
    }

    /// Entry count.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when no entries are loaded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl Default for MemoryInspectorAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl AppCore for MemoryInspectorAdapter {
    fn app_type(&self) -> &str {
        "memory-inspector"
    }

    fn is_alive(&self) -> bool {
        true
    }

    fn update(&mut self, _dt: f32) {}

    fn get_state(&self) -> serde_json::Value {
        json!({
            "type": "memory-inspector",
            "project": self.project,
            "entries": self.entries.len(),
        })
    }

    fn title(&self) -> &str {
        "Memory"
    }
}

impl Renderable for MemoryInspectorAdapter {
    fn render(&self, rect: &Rect) -> RenderOutput {
        let mut quads: Vec<QuadData> = Vec::new();
        let mut text_segments: Vec<TextData> = Vec::new();
        let t = self.tokens;

        let head = AppHead::new("MEMORY", format!("project · {}", self.project))
            .with_icon("◐")
            .with_meta(format!("{} entries", self.entries.len()))
            .with_tokens(t);
        head.render_into_adapter(rect, &mut quads, &mut text_segments);

        let body = head.body_rect_adapter(rect);
        let cell_w = if rect.cell_size.0 > 0.0 { rect.cell_size.0 } else { 8.0 };
        let cell_h = if rect.cell_size.1 > 0.0 { rect.cell_size.1 } else { 16.0 };
        let mut y = body.y + cell_h * 0.3;

        let key_color = t.colors.text_accent;
        let val_color = t.colors.text_primary;
        let sep_color = t.colors.text_dim;

        for e in &self.entries {
            if y + cell_h > body.y + body.height {
                break;
            }
            text_segments.push(TextData {
                text: e.key.clone(),
                x: body.x + cell_w,
                y,
                color: key_color,
            });
            text_segments.push(TextData {
                text: "·".to_string(),
                x: body.x + cell_w * 14.0,
                y,
                color: sep_color,
            });
            text_segments.push(TextData {
                text: e.value.clone(),
                x: body.x + cell_w * 16.0,
                y,
                color: val_color,
            });
            y += cell_h;
        }

        if self.entries.is_empty() {
            text_segments.push(TextData {
                text: "  (no memory entries)".to_string(),
                x: body.x + cell_w,
                y,
                color: sep_color,
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

impl InputHandler for MemoryInspectorAdapter {
    fn handle_input(&mut self, _key: &str) -> bool {
        false
    }

    fn accepts_input(&self) -> bool {
        false
    }
}

impl Commandable for MemoryInspectorAdapter {
    fn accept_command(&mut self, cmd: &str, args: &serde_json::Value) -> anyhow::Result<String> {
        match cmd {
            "set_project" => {
                let name = args
                    .get("name")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("missing field: name"))?;
                self.project = name.to_string();
                Ok(json!({ "status": "ok" }).to_string())
            }
            "load" => {
                let arr = args
                    .get("entries")
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| anyhow::anyhow!("missing field: entries"))?;
                let parsed: Vec<MemoryEntry> = arr
                    .iter()
                    .take(MAX_ENTRIES)
                    .filter_map(|item| {
                        let k = item.get("key")?.as_str()?.to_string();
                        let v = item.get("value")?.as_str()?.to_string();
                        Some(MemoryEntry::new(k, v))
                    })
                    .collect();
                self.set_entries(parsed);
                Ok(json!({ "status": "ok", "loaded": self.entries.len() }).to_string())
            }
            "clear" => {
                self.entries.clear();
                Ok(json!({ "status": "ok" }).to_string())
            }
            "snapshot" => Ok(self.get_state().to_string()),
            other => Err(anyhow::anyhow!("unknown command: {other}")),
        }
    }
}

impl BusParticipant for MemoryInspectorAdapter {}

impl Lifecycled for MemoryInspectorAdapter {
    fn set_app_id(&mut self, id: u32) {
        self.app_id = id;
    }
}

impl Permissioned for MemoryInspectorAdapter {}

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
    fn app_type_is_memory_inspector() {
        assert_eq!(MemoryInspectorAdapter::new().app_type(), "memory-inspector");
    }

    #[test]
    fn load_command_replaces_entries() {
        let mut a = MemoryInspectorAdapter::new();
        a.accept_command(
            "load",
            &json!({
                "entries": [
                    { "key": "stack", "value": "rust" },
                    { "key": "deploy", "value": "macOS" },
                ]
            }),
        )
        .unwrap();
        assert_eq!(a.len(), 2);
    }

    #[test]
    fn renders_entries_in_body() {
        let mut a = MemoryInspectorAdapter::new();
        a.set_entries(vec![MemoryEntry::new("k1", "v1"), MemoryEntry::new("k2", "v2")]);
        let out = a.render(&rect());
        assert!(out.text_segments.iter().any(|t| t.text == "k1"));
        assert!(out.text_segments.iter().any(|t| t.text == "v1"));
        assert!(out.text_segments.iter().any(|t| t.text == "k2"));
    }

    #[test]
    fn empty_renders_hint() {
        let a = MemoryInspectorAdapter::new();
        let out = a.render(&rect());
        assert!(out.text_segments.iter().any(|t| t.text.contains("no memory")));
    }

    #[test]
    fn set_project_updates_title() {
        let mut a = MemoryInspectorAdapter::new();
        a.accept_command("set_project", &json!({ "name": "badass-cli" }))
            .unwrap();
        let out = a.render(&rect());
        assert!(out.text_segments.iter().any(|t| t.text.contains("badass-cli")));
    }

    #[test]
    fn set_entries_caps_at_max() {
        let mut a = MemoryInspectorAdapter::new();
        let many: Vec<MemoryEntry> = (0..(MAX_ENTRIES + 100))
            .map(|i| MemoryEntry::new(format!("k{i}"), format!("v{i}")))
            .collect();
        a.set_entries(many);
        assert_eq!(a.len(), MAX_ENTRIES);
    }

    #[test]
    fn set_app_id_stores_id() {
        let mut a = MemoryInspectorAdapter::new();
        a.set_app_id(42);
        assert_eq!(a.app_id, 42);
    }

    #[test]
    fn theme_swap_propagates_to_key_color() {
        use phantom_ui::tokens::ColorRoles;
        let mut a = MemoryInspectorAdapter::new();
        a.set_entries(vec![MemoryEntry::new("k1", "v1")]);
        let out_p = a.render(&rect());
        let key_p = out_p
            .text_segments
            .iter()
            .find(|t| t.text == "k1")
            .map(|t| t.color)
            .expect("key must render");

        let mut roles = ColorRoles::phosphor();
        roles.text_accent = [0.0, 0.0, 1.0, 1.0];
        a.set_tokens(Tokens::new(roles, RenderCtx::fallback()));
        let out_b = a.render(&rect());
        let key_b = out_b
            .text_segments
            .iter()
            .find(|t| t.text == "k1")
            .map(|t| t.color)
            .expect("key must render");

        assert_ne!(key_p, key_b);
        assert!(key_b[2] > 0.9);
    }
}
