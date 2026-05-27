//! Plugins adapter — list installed plugins with their lifecycle state.

use serde_json::json;

use phantom_adapter::adapter::{QuadData, Rect, RenderOutput, TextData};
use phantom_adapter::spatial::{InternalLayout, SpatialPreference};
use phantom_adapter::{
    AppCore, BusParticipant, Commandable, InputHandler, Lifecycled, Permissioned, Renderable,
};
use phantom_ui::tokens::Tokens;
use phantom_ui::widgets::AppHead;
use phantom_ui::RenderCtx;

/// Plugin lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginState {
    Active,
    Idle,
    Disabled,
    Errored,
}

impl PluginState {
    /// Short label rendered in the right-aligned pill.
    pub fn label(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Idle => "idle",
            Self::Disabled => "disabled",
            Self::Errored => "error",
        }
    }
    /// Resolve the pill text color from the active token palette.
    fn color(self, t: &Tokens) -> [f32; 4] {
        match self {
            Self::Active => t.colors.status_ok,
            Self::Idle => t.colors.text_secondary,
            Self::Disabled => t.colors.text_dim,
            Self::Errored => t.colors.status_danger,
        }
    }
}

/// One plugin row.
#[derive(Debug, Clone)]
pub struct PluginEntry {
    pub name: String,
    pub version: String,
    pub state: PluginState,
}

impl PluginEntry {
    /// Convenience constructor.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        version: impl Into<String>,
        state: PluginState,
    ) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
            state,
        }
    }
}

/// Plugins pane.
pub struct PluginsAdapter {
    plugins: Vec<PluginEntry>,
    tokens: Tokens,
    app_id: u32,
}

impl PluginsAdapter {
    /// Build empty.
    #[must_use]
    pub fn new() -> Self {
        Self {
            plugins: Vec::new(),
            tokens: Tokens::phosphor(RenderCtx::fallback()),
            app_id: 0,
        }
    }

    /// Update the live color palette. The host App calls this on theme switch.
    pub fn set_tokens(&mut self, tokens: Tokens) {
        self.tokens = tokens;
    }

    /// Replace plugin list.
    pub fn set_plugins(&mut self, plugins: Vec<PluginEntry>) {
        self.plugins = plugins;
    }

    /// Plugin count.
    #[must_use]
    pub fn len(&self) -> usize {
        self.plugins.len()
    }

    /// True when no plugins installed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }
}

impl Default for PluginsAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl AppCore for PluginsAdapter {
    fn app_type(&self) -> &str {
        "plugins"
    }

    fn is_alive(&self) -> bool {
        true
    }

    fn update(&mut self, _dt: f32) {}

    fn get_state(&self) -> serde_json::Value {
        json!({
            "type": "plugins",
            "plugins": self.plugins.len(),
        })
    }

    fn title(&self) -> &str {
        "Plugins"
    }
}

impl Renderable for PluginsAdapter {
    fn render(&self, rect: &Rect) -> RenderOutput {
        let mut quads: Vec<QuadData> = Vec::new();
        let mut text_segments: Vec<TextData> = Vec::new();
        let t = self.tokens;

        let head = AppHead::new("PLUGINS", "wasm sandbox")
            .with_icon("⊕")
            .with_meta(format!("{}", self.plugins.len()))
            .with_tokens(t);
        head.render_into_adapter(rect, &mut quads, &mut text_segments);

        let body = head.body_rect_adapter(rect);
        let cell_w = if rect.cell_size.0 > 0.0 { rect.cell_size.0 } else { 8.0 };
        let cell_h = if rect.cell_size.1 > 0.0 { rect.cell_size.1 } else { 16.0 };
        let mut y = body.y + cell_h * 0.3;

        let name_color = t.colors.text_primary;
        let ver_color = t.colors.text_dim;

        for p in &self.plugins {
            if y + cell_h > body.y + body.height {
                break;
            }
            text_segments.push(TextData {
                text: p.name.clone(),
                x: body.x + cell_w,
                y,
                color: name_color,
            });
            text_segments.push(TextData {
                text: p.version.clone(),
                x: body.x + cell_w * 18.0,
                y,
                color: ver_color,
            });
            // Right-aligned state pill — small background + label.
            let pill_label = p.state.label();
            let pill_w = (pill_label.chars().count() as f32 + 2.0) * cell_w;
            let pill_x = body.x + body.width - pill_w - cell_w;
            let label_color = p.state.color(&t);
            quads.push(QuadData {
                x: pill_x,
                y: y + 2.0,
                w: pill_w,
                h: cell_h - 4.0,
                color: [label_color[0] * 0.35, label_color[1] * 0.35, label_color[2] * 0.35, 0.45],
            });
            text_segments.push(TextData {
                text: pill_label.to_string(),
                x: pill_x + cell_w,
                y,
                color: label_color,
            });
            y += cell_h;
        }

        if self.plugins.is_empty() {
            text_segments.push(TextData {
                text: "  (no plugins installed)".to_string(),
                x: body.x + cell_w,
                y,
                color: ver_color,
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
            preferred_size: (60, 18),
            max_size: None,
            aspect_ratio: None,
            internal_panes: 1,
            internal_layout: InternalLayout::Single,
            priority: 2.0,
        })
    }
}

impl InputHandler for PluginsAdapter {
    fn handle_input(&mut self, _key: &str) -> bool {
        false
    }

    fn accepts_input(&self) -> bool {
        false
    }
}

impl Commandable for PluginsAdapter {
    fn accept_command(&mut self, cmd: &str, args: &serde_json::Value) -> anyhow::Result<String> {
        match cmd {
            "set_theme_name" => {
                let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("");
                if let Some(tokens) = Tokens::for_theme_name(name, RenderCtx::fallback()) {
                    self.set_tokens(tokens);
                }
                Ok(json!({ "status": "ok" }).to_string())
            }
            "load" => {
                let arr = args
                    .get("plugins")
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| anyhow::anyhow!("missing field: plugins"))?;
                self.plugins = arr
                    .iter()
                    .filter_map(|item| {
                        let name = item.get("name")?.as_str()?.to_string();
                        let version =
                            item.get("version").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let state = match item.get("state").and_then(|v| v.as_str()) {
                            Some("idle") => PluginState::Idle,
                            Some("disabled") => PluginState::Disabled,
                            Some("error") | Some("errored") => PluginState::Errored,
                            _ => PluginState::Active,
                        };
                        Some(PluginEntry {
                            name,
                            version,
                            state,
                        })
                    })
                    .collect();
                Ok(json!({ "status": "ok", "loaded": self.plugins.len() }).to_string())
            }
            "clear" => {
                self.plugins.clear();
                Ok(json!({ "status": "ok" }).to_string())
            }
            "snapshot" => Ok(self.get_state().to_string()),
            other => Err(anyhow::anyhow!("unknown command: {other}")),
        }
    }
}

impl BusParticipant for PluginsAdapter {}

impl Lifecycled for PluginsAdapter {
    fn set_app_id(&mut self, id: u32) {
        self.app_id = id;
    }
}

impl Permissioned for PluginsAdapter {}

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
    fn app_type_is_plugins() {
        assert_eq!(PluginsAdapter::new().app_type(), "plugins");
    }

    #[test]
    fn load_command_parses_states() {
        let mut a = PluginsAdapter::new();
        a.accept_command(
            "load",
            &json!({
                "plugins": [
                    { "name": "git-blame", "version": "0.3.1", "state": "active" },
                    { "name": "graphql-runner", "version": "1.0.0", "state": "idle" },
                    { "name": "jira-bridge", "version": "0.1.0-rc", "state": "disabled" },
                ]
            }),
        )
        .unwrap();
        assert_eq!(a.len(), 3);
        assert_eq!(a.plugins[0].state, PluginState::Active);
        assert_eq!(a.plugins[2].state, PluginState::Disabled);
    }

    #[test]
    fn renders_name_and_version_and_state_label() {
        let mut a = PluginsAdapter::new();
        a.set_plugins(vec![PluginEntry::new("foo", "1.2.3", PluginState::Active)]);
        let out = a.render(&rect());
        assert!(out.text_segments.iter().any(|t| t.text == "foo"));
        assert!(out.text_segments.iter().any(|t| t.text == "1.2.3"));
        assert!(out.text_segments.iter().any(|t| t.text == "active"));
    }

    #[test]
    fn empty_renders_hint() {
        let a = PluginsAdapter::new();
        let out = a.render(&rect());
        assert!(out.text_segments.iter().any(|t| t.text.contains("no plugins")));
    }

    #[test]
    fn set_app_id_stores_id() {
        let mut a = PluginsAdapter::new();
        a.set_app_id(42);
        assert_eq!(a.app_id, 42);
    }

    #[test]
    fn theme_swap_propagates_to_active_pill() {
        use phantom_ui::tokens::ColorRoles;
        let mut a = PluginsAdapter::new();
        a.set_plugins(vec![PluginEntry::new("foo", "1.0", PluginState::Active)]);
        let out_p = a.render(&rect());
        let pill_p = out_p
            .text_segments
            .iter()
            .find(|t| t.text == "active")
            .map(|t| t.color)
            .expect("pill must render");

        let mut roles = ColorRoles::phosphor();
        roles.status_ok = [0.0, 0.0, 1.0, 1.0];
        a.set_tokens(Tokens::new(roles, RenderCtx::fallback()));
        let out_b = a.render(&rect());
        let pill_b = out_b
            .text_segments
            .iter()
            .find(|t| t.text == "active")
            .map(|t| t.color)
            .expect("pill must render");

        assert_ne!(pill_p, pill_b);
        assert!(pill_b[2] > 0.9);
    }
}
