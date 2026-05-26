//! Keybinds help adapter — F1 overlay listing every active key binding.
//!
//! Reads from a shared `KeybindRegistry` (the same one the App uses to
//! dispatch input) so the help pane is always in sync with reality. Each
//! `render()` call snapshots the current bindings, sorts them, and lays
//! them out as a two-column table under the shared `AppHead` chrome.
//!
//! ## Dismissal
//!
//! The adapter does NOT consume key input — `accepts_input()` is `false`.
//! The host App owns the F1 toggle: pressing F1 should despawn the pane,
//! not route the key into the adapter. The `with_meta("F1")` chrome
//! label is informational only.

use std::sync::{Arc, RwLock};

use serde_json::json;

use phantom_adapter::adapter::{Rect, RenderOutput, TextData};
use phantom_adapter::spatial::{InternalLayout, SpatialPreference};
use phantom_adapter::{
    AppCore, BusParticipant, Commandable, InputHandler, Lifecycled, Permissioned, Renderable,
};
use phantom_ui::keybinds::KeybindRegistry;
use phantom_ui::tokens::Tokens;
use phantom_ui::widgets::AppHead;
use phantom_ui::RenderCtx;

/// Maximum number of rows rendered per pane height.
const MAX_VISIBLE_ROWS: usize = 64;

/// Adapter that renders the active keybind table.
pub struct KeybindsHelpAdapter {
    registry: Arc<RwLock<KeybindRegistry>>,
    tokens: Tokens,
    app_id: u32,
}

impl KeybindsHelpAdapter {
    /// Build an adapter bound to a shared keybind registry.
    #[must_use]
    pub fn new(registry: Arc<RwLock<KeybindRegistry>>) -> Self {
        Self {
            registry,
            tokens: Tokens::phosphor(RenderCtx::fallback()),
            app_id: 0,
        }
    }

    /// Convenience: build with a fresh default registry (useful for tests
    /// and stand-alone instantiation).
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(Arc::new(RwLock::new(KeybindRegistry::new())))
    }

    /// Update the live color palette. The host App calls this on theme switch
    /// so the next render picks up the new colors.
    pub fn set_tokens(&mut self, tokens: Tokens) {
        self.tokens = tokens;
    }

    /// Sorted snapshot of `(combo, action)` pairs from the registry.
    fn rows(&self) -> Vec<(String, String)> {
        let reg = self.registry.read().expect("keybinds registry lock");
        let mut rows: Vec<(String, String)> = reg
            .iter()
            .map(|(combo, action)| (format!("{combo}"), format!("{action}")))
            .collect();
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        rows
    }
}

impl AppCore for KeybindsHelpAdapter {
    fn app_type(&self) -> &str {
        "keybinds-help"
    }

    fn is_alive(&self) -> bool {
        true
    }

    fn update(&mut self, _dt: f32) {}

    fn get_state(&self) -> serde_json::Value {
        let count = self.rows().len();
        json!({
            "type": "keybinds-help",
            "binding_count": count,
        })
    }

    fn title(&self) -> &str {
        "Keybinds"
    }
}

impl Renderable for KeybindsHelpAdapter {
    fn render(&self, rect: &Rect) -> RenderOutput {
        let mut quads = Vec::new();
        let mut text_segments: Vec<TextData> = Vec::new();
        let t = self.tokens;

        let rows = self.rows();
        let head = AppHead::new("KEYBINDS", "F1")
            .with_icon("⌨")
            .with_meta(format!("{} bindings", rows.len()))
            .with_tokens(t);
        head.render_into_adapter(rect, &mut quads, &mut text_segments);

        let body = head.body_rect_adapter(rect);
        let cell_w = if rect.cell_size.0 > 0.0 { rect.cell_size.0 } else { 8.0 };
        let cell_h = if rect.cell_size.1 > 0.0 { rect.cell_size.1 } else { 16.0 };

        let pad_x = cell_w;
        let mut y = body.y + cell_h * 0.5;

        // Two-column layout: combo (left 40 %), action (right 60 %).
        let combo_col_x = body.x + pad_x;
        let action_col_x = body.x + body.width * 0.4;

        let key_color = t.colors.text_primary;
        let action_color = t.colors.text_secondary;

        for (combo, action) in rows.iter().take(MAX_VISIBLE_ROWS) {
            if y + cell_h > body.y + body.height {
                break;
            }
            text_segments.push(TextData {
                text: combo.clone(),
                x: combo_col_x,
                y,
                color: key_color,
            });
            text_segments.push(TextData {
                text: action.clone(),
                x: action_col_x,
                y,
                color: action_color,
            });
            y += cell_h;
        }

        if rows.is_empty() {
            text_segments.push(TextData {
                text: "  (no bindings)".to_string(),
                x: combo_col_x,
                y,
                color: action_color,
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
            min_size: (40, 10),
            preferred_size: (60, 24),
            max_size: Some((100, 48)),
            aspect_ratio: None,
            internal_panes: 1,
            internal_layout: InternalLayout::Single,
            priority: 2.0,
        })
    }
}

impl InputHandler for KeybindsHelpAdapter {
    fn handle_input(&mut self, _key: &str) -> bool {
        false
    }

    fn accepts_input(&self) -> bool {
        false
    }
}

impl Commandable for KeybindsHelpAdapter {
    fn accept_command(&mut self, cmd: &str, _args: &serde_json::Value) -> anyhow::Result<String> {
        match cmd {
            "snapshot" => {
                let rows: Vec<serde_json::Value> = self
                    .rows()
                    .into_iter()
                    .map(|(k, a)| json!({ "combo": k, "action": a }))
                    .collect();
                Ok(serde_json::Value::Array(rows).to_string())
            }
            other => Err(anyhow::anyhow!("unknown command: {other}")),
        }
    }
}

impl BusParticipant for KeybindsHelpAdapter {}

impl Lifecycled for KeybindsHelpAdapter {
    fn set_app_id(&mut self, id: u32) {
        self.app_id = id;
    }
}

impl Permissioned for KeybindsHelpAdapter {}

#[cfg(test)]
mod tests {
    use super::*;
    use phantom_ui::tokens::ColorRoles;

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
    fn app_type_is_keybinds_help() {
        let a = KeybindsHelpAdapter::with_defaults();
        assert_eq!(a.app_type(), "keybinds-help");
    }

    #[test]
    fn renders_app_head_chrome() {
        let a = KeybindsHelpAdapter::with_defaults();
        let out = a.render(&rect());
        assert!(out.quads.len() >= 2);
        assert!(out.text_segments.iter().any(|t| t.text == "KEYBINDS"));
    }

    #[test]
    fn renders_default_bindings() {
        let a = KeybindsHelpAdapter::with_defaults();
        let out = a.render(&rect());
        let action_present = out
            .text_segments
            .iter()
            .any(|t| t.text == "Copy" || t.text == "Paste" || t.text == "Quit");
        assert!(action_present, "expected at least one default action label");
    }

    #[test]
    fn empty_registry_renders_hint() {
        let reg = Arc::new(RwLock::new(KeybindRegistry::new()));
        {
            let combos: Vec<_> = {
                let r = reg.read().unwrap();
                r.iter().map(|(c, _)| *c).collect()
            };
            let mut w = reg.write().unwrap();
            for c in combos {
                w.unbind(&c);
            }
        }
        let a = KeybindsHelpAdapter::new(reg);
        let out = a.render(&rect());
        assert!(out.text_segments.iter().any(|t| t.text.contains("no bindings")));
    }

    #[test]
    fn snapshot_command_returns_json_array() {
        let mut a = KeybindsHelpAdapter::with_defaults();
        let resp = a.accept_command("snapshot", &json!({})).unwrap();
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
        assert!(v.is_array());
        assert!(!v.as_array().unwrap().is_empty());
    }

    #[test]
    fn get_state_reports_count() {
        let a = KeybindsHelpAdapter::with_defaults();
        let st = a.get_state();
        assert_eq!(st["type"], "keybinds-help");
        assert!(st["binding_count"].as_u64().unwrap() > 0);
    }

    #[test]
    fn set_app_id_stores_id() {
        let mut a = KeybindsHelpAdapter::with_defaults();
        a.set_app_id(42);
        assert_eq!(a.app_id, 42);
    }

    #[test]
    fn theme_swap_propagates_to_row_color() {
        let mut a = KeybindsHelpAdapter::with_defaults();
        // First render under phosphor.
        let out_p = a.render(&rect());
        let combo_p = out_p
            .text_segments
            .iter()
            .find(|t| !t.text.is_empty() && t.x > 0.0 && t.text != "KEYBINDS")
            .map(|t| t.color)
            .expect("phosphor row must render");

        // Swap to a contrasting palette where text_primary is pure blue.
        let mut roles = ColorRoles::phosphor();
        roles.text_primary = [0.0, 0.0, 1.0, 1.0];
        a.set_tokens(Tokens::new(roles, RenderCtx::fallback()));

        let out_b = a.render(&rect());
        let combo_b = out_b
            .text_segments
            .iter()
            .find(|t| !t.text.is_empty() && t.x > 0.0 && t.text != "KEYBINDS")
            .map(|t| t.color)
            .expect("blue row must render");

        assert_ne!(combo_p, combo_b, "row colors must change with theme");
    }
}
