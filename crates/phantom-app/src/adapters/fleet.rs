//! Fleet adapter — render the list of connected/known fleet nodes.

use serde_json::json;

use phantom_adapter::adapter::{QuadData, Rect, RenderOutput, TextData};
use phantom_adapter::spatial::{InternalLayout, SpatialPreference};
use phantom_adapter::{
    AppCore, BusParticipant, Commandable, InputHandler, Lifecycled, Permissioned, Renderable,
};
use phantom_ui::tokens::Tokens;
use phantom_ui::widgets::AppHead;
use phantom_ui::RenderCtx;

/// Cap on `set_nodes` / `load` to prevent OOM if a host caller misbehaves.
pub const MAX_NODES: usize = 1000;

/// Reachability state for a fleet node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FleetNodeState {
    /// Connected and healthy.
    Live,
    /// Connected but degraded or busy.
    Busy,
    /// Idle / not currently reachable.
    Idle,
    /// Self (the local node).
    Self_,
}

impl FleetNodeState {
    /// Resolve the status-dot color from the active token palette.
    fn color(self, t: &Tokens) -> [f32; 4] {
        match self {
            Self::Live | Self::Self_ => t.colors.status_ok,
            Self::Busy => t.colors.status_info,
            Self::Idle => t.colors.status_warn,
        }
    }
}

/// One fleet node row.
#[derive(Debug, Clone)]
pub struct FleetNode {
    pub name: String,
    pub state: FleetNodeState,
    pub meta: String,
}

impl FleetNode {
    /// Convenience constructor.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        state: FleetNodeState,
        meta: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            state,
            meta: meta.into(),
        }
    }
}

/// Fleet pane.
pub struct FleetAdapter {
    nodes: Vec<FleetNode>,
    tokens: Tokens,
    app_id: u32,
}

impl FleetAdapter {
    /// Build with no nodes.
    #[must_use]
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            tokens: Tokens::phosphor(RenderCtx::fallback()),
            app_id: 0,
        }
    }

    /// Update the live color palette. The host App calls this on theme switch.
    pub fn set_tokens(&mut self, tokens: Tokens) {
        self.tokens = tokens;
    }

    /// Replace node list. Truncates at `MAX_NODES` to bound the adapter's memory.
    pub fn set_nodes(&mut self, mut nodes: Vec<FleetNode>) {
        if nodes.len() > MAX_NODES {
            nodes.truncate(MAX_NODES);
        }
        self.nodes = nodes;
    }

    /// Node count.
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// True when no nodes are known.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}

impl Default for FleetAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl AppCore for FleetAdapter {
    fn app_type(&self) -> &str {
        "fleet"
    }

    fn is_alive(&self) -> bool {
        true
    }

    fn update(&mut self, _dt: f32) {}

    fn get_state(&self) -> serde_json::Value {
        json!({
            "type": "fleet",
            "nodes": self.nodes.len(),
        })
    }

    fn title(&self) -> &str {
        "Fleet"
    }
}

impl Renderable for FleetAdapter {
    fn render(&self, rect: &Rect) -> RenderOutput {
        let mut quads: Vec<QuadData> = Vec::new();
        let mut text_segments: Vec<TextData> = Vec::new();
        let t = self.tokens;

        let live_count = self
            .nodes
            .iter()
            .filter(|n| matches!(n.state, FleetNodeState::Live | FleetNodeState::Self_))
            .count();
        let head = AppHead::new("FLEET", "connected nodes")
            .with_icon("⊞")
            .with_meta(format!("{live_count} live"))
            .with_tokens(t)
            .focused(rect.focused);
        head.render_into_adapter(rect, &mut quads, &mut text_segments);

        let body = head.body_rect_adapter(rect);
        let cell_w = if rect.cell_size.0 > 0.0 { rect.cell_size.0 } else { 8.0 };
        let cell_h = if rect.cell_size.1 > 0.0 { rect.cell_size.1 } else { 16.0 };
        let mut y = body.y + cell_h * 0.3;

        let name_color = t.colors.text_primary;
        let meta_color = t.colors.text_dim;

        for node in &self.nodes {
            if y + cell_h > body.y + body.height {
                break;
            }
            // Status dot
            let dot = 6.0;
            quads.push(QuadData {
                x: body.x + cell_w,
                y: y + (cell_h - dot) * 0.5,
                w: dot,
                h: dot,
                color: node.state.color(&t),
            });
            // Name
            text_segments.push(TextData {
                text: node.name.clone(),
                x: body.x + cell_w * 3.0,
                y,
                color: name_color,
            });
            // Meta (right side)
            if !node.meta.is_empty() {
                let meta_x = body.x + body.width - cell_w * (node.meta.chars().count() as f32 + 1.0);
                text_segments.push(TextData {
                    text: node.meta.clone(),
                    x: meta_x.max(body.x + cell_w * 20.0),
                    y,
                    color: meta_color,
                });
            }
            y += cell_h;
        }

        if self.nodes.is_empty() {
            text_segments.push(TextData {
                text: "  (no nodes connected)".to_string(),
                x: body.x + cell_w,
                y,
                color: meta_color,
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

impl InputHandler for FleetAdapter {
    fn handle_input(&mut self, _key: &str) -> bool {
        false
    }

    fn accepts_input(&self) -> bool {
        false
    }
}

impl Commandable for FleetAdapter {
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
                    .get("nodes")
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| anyhow::anyhow!("missing field: nodes"))?;
                let parsed: Vec<FleetNode> = arr
                    .iter()
                    .take(MAX_NODES)
                    .filter_map(|item| {
                        let name = item.get("name")?.as_str()?.to_string();
                        let state = match item.get("state").and_then(|v| v.as_str())? {
                            "self" | "self_" => FleetNodeState::Self_,
                            "live" => FleetNodeState::Live,
                            "busy" => FleetNodeState::Busy,
                            _ => FleetNodeState::Idle,
                        };
                        let meta = item
                            .get("meta")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        Some(FleetNode {
                            name,
                            state,
                            meta,
                        })
                    })
                    .collect();
                self.set_nodes(parsed);
                Ok(json!({ "status": "ok", "loaded": self.nodes.len() }).to_string())
            }
            "clear" => {
                self.nodes.clear();
                Ok(json!({ "status": "ok" }).to_string())
            }
            "snapshot" => Ok(self.get_state().to_string()),
            other => Err(anyhow::anyhow!("unknown command: {other}")),
        }
    }
}

impl BusParticipant for FleetAdapter {}

impl Lifecycled for FleetAdapter {
    fn set_app_id(&mut self, id: u32) {
        self.app_id = id;
    }
}

impl Permissioned for FleetAdapter {}

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
            ..Default::default()
        }
    }

    #[test]
    fn app_type_is_fleet() {
        assert_eq!(FleetAdapter::new().app_type(), "fleet");
    }

    #[test]
    fn load_parses_nodes() {
        let mut a = FleetAdapter::new();
        a.accept_command(
            "load",
            &json!({
                "nodes": [
                    { "name": "max.local", "state": "self", "meta": "M3 Max" },
                    { "name": "ci-runner-01", "state": "live", "meta": "linux" },
                    { "name": "deploy-staging", "state": "idle", "meta": "4h" },
                ]
            }),
        )
        .unwrap();
        assert_eq!(a.len(), 3);
    }

    #[test]
    fn renders_node_names() {
        let mut a = FleetAdapter::new();
        a.set_nodes(vec![
            FleetNode::new("alpha", FleetNodeState::Live, "x"),
            FleetNode::new("beta", FleetNodeState::Idle, ""),
        ]);
        let out = a.render(&rect());
        assert!(out.text_segments.iter().any(|t| t.text == "alpha"));
        assert!(out.text_segments.iter().any(|t| t.text == "beta"));
    }

    #[test]
    fn empty_renders_hint() {
        let a = FleetAdapter::new();
        let out = a.render(&rect());
        assert!(out.text_segments.iter().any(|t| t.text.contains("no nodes")));
    }

    #[test]
    fn live_count_appears_in_meta() {
        let mut a = FleetAdapter::new();
        a.set_nodes(vec![
            FleetNode::new("a", FleetNodeState::Live, ""),
            FleetNode::new("b", FleetNodeState::Self_, ""),
            FleetNode::new("c", FleetNodeState::Idle, ""),
        ]);
        let out = a.render(&rect());
        assert!(out.text_segments.iter().any(|t| t.text == "2 live"));
    }

    #[test]
    fn set_nodes_caps_at_max() {
        let mut a = FleetAdapter::new();
        let many: Vec<FleetNode> = (0..(MAX_NODES + 100))
            .map(|i| FleetNode::new(format!("n{i}"), FleetNodeState::Idle, ""))
            .collect();
        a.set_nodes(many);
        assert_eq!(a.len(), MAX_NODES);
    }

    #[test]
    fn set_app_id_stores_id() {
        let mut a = FleetAdapter::new();
        a.set_app_id(42);
        assert_eq!(a.app_id, 42);
    }

    #[test]
    fn theme_swap_propagates_to_status_dot() {
        use phantom_ui::tokens::ColorRoles;
        let mut a = FleetAdapter::new();
        a.set_nodes(vec![FleetNode::new("a", FleetNodeState::Live, "")]);
        let out_p = a.render(&rect());
        let dot_p = out_p
            .quads
            .iter()
            .find(|q| (q.w - 6.0).abs() < 0.01 && (q.h - 6.0).abs() < 0.01)
            .expect("dot must render");

        let mut roles = ColorRoles::phosphor();
        roles.status_ok = [0.0, 0.0, 1.0, 1.0];
        a.set_tokens(Tokens::new(roles, RenderCtx::fallback()));
        let out_b = a.render(&rect());
        let dot_b = out_b
            .quads
            .iter()
            .find(|q| (q.w - 6.0).abs() < 0.01 && (q.h - 6.0).abs() < 0.01)
            .expect("dot must render");

        assert_ne!(dot_p.color, dot_b.color);
        assert!(dot_b.color[2] > 0.9);
    }
}
