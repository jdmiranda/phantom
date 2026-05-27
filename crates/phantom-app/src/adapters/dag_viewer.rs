//! Force-directed layout state for the DAG viewer tab.
//!
//! [`DagViewerState`] holds the spring-simulation layout positions for all
//! nodes in a [`phantom_dag::CodeDag`], viewport pan/zoom, and the per-node
//! instability overlay. It is designed to be embedded inside the
//! [`InspectorAdapter`] when the user switches to the DAG tab.
//!
//! ## Layout algorithm
//!
//! A simple spring/repulsion simulation is run for a fixed number of
//! iterations when [`DagViewerState::compute_layout`] is called:
//! - All node pairs repel each other (Coulomb-style).
//! - Connected node pairs attract each other (Hooke-style).
//! - Initial positions are placed on a circle so nodes don't start on top
//!   of each other.
//!
//! 50 iterations is enough for stable initial placement; the result does
//! not need to be pixel-perfect at this stage.
//!
//! ## Rendering
//!
//! [`DagViewerState::render_quads`] returns a `Vec<phantom_renderer::quads::QuadInstance>`
//! that can be fed directly to the quad renderer. Node color interpolates
//! between a stable green and a danger red based on the `instability_score`
//! from the overlay.

use std::collections::HashMap;
use std::f32::consts::PI;

use phantom_dag::{CodeDag, OverlayIndex};
use phantom_renderer::quads::QuadInstance;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Repulsion constant (higher = nodes push apart more).
const K_REPEL: f32 = 5_000.0;
/// Attraction constant (spring strength between connected nodes).
const K_ATTRACT: f32 = 0.05;
/// Natural spring length in pixels (desired edge length).
const SPRING_LEN: f32 = 120.0;
/// Damping factor applied to displacement each iteration.
const DAMPING: f32 = 0.85;
/// Number of spring-simulation iterations on initial layout.
const LAYOUT_ITERATIONS: usize = 50;
/// Radius of the initial circle placement.
const INITIAL_RADIUS: f32 = 200.0;
/// Rendered node box size in pixels.
const NODE_W: f32 = 80.0;
const NODE_H: f32 = 24.0;
/// Minimum zoom level.
const ZOOM_MIN: f32 = 0.1;
/// Maximum zoom level.
const ZOOM_MAX: f32 = 5.0;

// ---------------------------------------------------------------------------
// DagViewerState
// ---------------------------------------------------------------------------

/// Runtime layout and interaction state for the Inspector's DAG tab.
///
/// Create with [`DagViewerState::new`], then call [`compute_layout`] once
/// after a DAG becomes available. Each frame, call [`render_quads`] to get
/// the draw list.
///
/// [`compute_layout`]: DagViewerState::compute_layout
pub struct DagViewerState {
    /// Per-node positions in world-space pixels (node_id → [x, y]).
    pub positions: HashMap<String, [f32; 2]>,
    /// Current pan offset applied before rendering.
    pub viewport_offset: [f32; 2],
    /// Current zoom level (1.0 = 100%).
    pub zoom: f32,
    /// The node id that is currently selected (highlighted), if any.
    pub selected_node: Option<String>,
    /// Per-node ticket overlay for instability colouring.
    pub overlay: OverlayIndex,
}

impl DagViewerState {
    /// Create a new viewer state with default zoom and no selection.
    #[must_use]
    pub fn new() -> Self {
        Self {
            positions: HashMap::new(),
            viewport_offset: [0.0, 0.0],
            zoom: 1.0,
            selected_node: None,
            overlay: OverlayIndex::new(),
        }
    }

    /// Run [`LAYOUT_ITERATIONS`] spring-simulation steps to compute initial
    /// node positions from a [`CodeDag`].
    ///
    /// Nodes are first placed on a circle of radius [`INITIAL_RADIUS`] so
    /// that no two nodes start at the same position. If the DAG is empty
    /// this is a no-op.
    pub fn compute_layout(&mut self, dag: &CodeDag) {
        let ids: Vec<String> = dag.nodes().map(|n| n.id().to_owned()).collect();
        let n = ids.len();
        if n == 0 {
            return;
        }

        // Place nodes on an evenly-spaced circle.
        for (i, id) in ids.iter().enumerate() {
            let angle = 2.0 * PI * (i as f32) / (n as f32);
            let x = INITIAL_RADIUS * angle.cos();
            let y = INITIAL_RADIUS * angle.sin();
            self.positions.insert(id.clone(), [x, y]);
        }

        // Build adjacency list (bidirectional for layout purposes).
        let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
        for edge in dag.edges() {
            adj.entry(edge.from()).or_default().push(edge.to());
            adj.entry(edge.to()).or_default().push(edge.from());
        }

        // Iterative force-directed layout.
        for _ in 0..LAYOUT_ITERATIONS {
            let mut displacements: HashMap<String, [f32; 2]> =
                ids.iter().map(|id| (id.clone(), [0.0_f32, 0.0_f32])).collect();

            // Repulsion: all pairs.
            for i in 0..n {
                for j in (i + 1)..n {
                    let [xi, yi] = self.positions[&ids[i]];
                    let [xj, yj] = self.positions[&ids[j]];
                    let dx = xi - xj;
                    let dy = yi - yj;
                    let dist_sq = (dx * dx + dy * dy).max(0.01);
                    let dist = dist_sq.sqrt();
                    let force = K_REPEL / dist_sq;
                    let fx = force * dx / dist;
                    let fy = force * dy / dist;

                    let di = displacements.get_mut(&ids[i]).unwrap();
                    di[0] += fx;
                    di[1] += fy;
                    let dj = displacements.get_mut(&ids[j]).unwrap();
                    dj[0] -= fx;
                    dj[1] -= fy;
                }
            }

            // Attraction: connected pairs.
            for id in &ids {
                if let Some(neighbors) = adj.get(id.as_str()) {
                    for &nb in neighbors {
                        if !self.positions.contains_key(nb) {
                            continue;
                        }
                        let [xi, yi] = self.positions[id];
                        let [xj, yj] = self.positions[nb];
                        let dx = xj - xi;
                        let dy = yj - yi;
                        let dist = (dx * dx + dy * dy).sqrt().max(0.01);
                        let displacement = dist - SPRING_LEN;
                        let fx = K_ATTRACT * displacement * dx / dist;
                        let fy = K_ATTRACT * displacement * dy / dist;
                        let di = displacements.get_mut(id).unwrap();
                        di[0] += fx;
                        di[1] += fy;
                    }
                }
            }

            // Apply displacements with damping.
            for id in &ids {
                let [dx, dy] = displacements[id];
                let pos = self.positions.get_mut(id).unwrap();
                pos[0] += dx * DAMPING;
                pos[1] += dy * DAMPING;
            }
        }
    }

    /// Build a `Vec<QuadInstance>` for all nodes and edges in `dag`.
    ///
    /// Node quads are coloured by instability score: fully stable nodes are
    /// green (`[0.2, 0.8, 0.2, 1.0]`), fully unstable nodes are red
    /// (`[0.9, 0.2, 0.15, 1.0]`). The selected node gets a bright white
    /// highlight. Edge "lines" are rendered as thin rectangles between node
    /// centres.
    ///
    /// Returns an empty `Vec` if the DAG has no nodes. Never panics on an
    /// empty graph.
    #[must_use]
    pub fn render_quads(&self, dag: &CodeDag) -> Vec<QuadInstance> {
        if dag.node_count() == 0 {
            return vec![];
        }

        let mut quads = Vec::with_capacity(dag.node_count() * 2 + dag.edge_count());
        let [off_x, off_y] = self.viewport_offset;

        // ── Edges (drawn beneath nodes) ───────────────────────────────────
        for edge in dag.edges() {
            let Some(&[x0, y0]) = self.positions.get(edge.from()) else { continue };
            let Some(&[x1, y1]) = self.positions.get(edge.to()) else { continue };

            // Draw a thin rectangle as a proxy for a line.
            let cx = (x0 + x1) * 0.5;
            let cy = (y0 + y1) * 0.5;
            let dx = x1 - x0;
            let dy = y1 - y0;
            let len = (dx * dx + dy * dy).sqrt().max(1.0);

            // We can't rotate quads without shader support, so we draw a
            // horizontal or vertical proxy based on the dominant axis.
            let (qx, qy, qw, qh) = if dx.abs() >= dy.abs() {
                // Horizontal-ish edge.
                let x = x0.min(x1) * self.zoom + off_x;
                let y = (cy - 1.0) * self.zoom + off_y;
                let w = len * self.zoom;
                let h = 2.0 * self.zoom;
                (x, y, w, h)
            } else {
                // Vertical-ish edge.
                let x = (cx - 1.0) * self.zoom + off_x;
                let y = y0.min(y1) * self.zoom + off_y;
                let w = 2.0 * self.zoom;
                let h = len * self.zoom;
                (x, y, w, h)
            };

            quads.push(QuadInstance {
                pos: [qx, qy],
                size: [qw.max(1.0), qh.max(1.0)],
                color: [0.35, 0.45, 0.45, 0.7],
                border_radius: 0.0,
            });
        }

        // ── Nodes ─────────────────────────────────────────────────────────
        for node in dag.nodes() {
            let Some(&[wx, wy]) = self.positions.get(node.id()) else { continue };

            let sx = wx * self.zoom + off_x;
            let sy = wy * self.zoom + off_y;
            let sw = NODE_W * self.zoom;
            let sh = NODE_H * self.zoom;

            let color = if self.selected_node.as_deref() == Some(node.id()) {
                // Selected: bright white outline.
                [0.95, 0.95, 0.95, 1.0]
            } else {
                // Interpolate between stable green and unstable red.
                let score = self
                    .overlay
                    .get(node.id())
                    .map(|o| o.instability_score.clamp(0.0, 1.0))
                    .unwrap_or(0.0);
                let r = 0.2 + 0.7 * score;
                let g = 0.8 - 0.6 * score;
                let b = 0.2 - 0.05 * score;
                [r, g, b.max(0.0), 1.0]
            };

            quads.push(QuadInstance {
                pos: [sx, sy],
                size: [sw, sh],
                color,
                border_radius: 3.0 * self.zoom,
            });
        }

        quads
    }

    /// Apply a pan delta (in screen pixels) to the viewport offset.
    pub fn handle_pan(&mut self, delta: [f32; 2]) {
        self.viewport_offset[0] += delta[0];
        self.viewport_offset[1] += delta[1];
    }

    /// Multiply the current zoom by `factor`, clamped to `[ZOOM_MIN, ZOOM_MAX]`.
    pub fn handle_zoom(&mut self, factor: f32) {
        self.zoom = (self.zoom * factor).clamp(ZOOM_MIN, ZOOM_MAX);
    }

    /// Test whether `world_pos` hits any node box and, if so, select it.
    ///
    /// Returns the id of the newly-selected node, or `None` if the click
    /// missed all nodes. The selection is also stored in
    /// [`DagViewerState::selected_node`].
    pub fn handle_click(&mut self, world_pos: [f32; 2]) -> Option<String> {
        let [wx, wy] = world_pos;
        // Invert viewport transform: world → layout space.
        let [off_x, off_y] = self.viewport_offset;
        let lx = (wx - off_x) / self.zoom;
        let ly = (wy - off_y) / self.zoom;

        for (id, &[nx, ny]) in &self.positions {
            if lx >= nx && lx <= nx + NODE_W && ly >= ny && ly <= ny + NODE_H {
                self.selected_node = Some(id.clone());
                return Some(id.clone());
            }
        }
        self.selected_node = None;
        None
    }
}

impl Default for DagViewerState {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// DagViewerAdapter — standalone, Cmd+Shift+A reachable pane
// ---------------------------------------------------------------------------

use phantom_adapter::adapter::{QuadData, Rect, RenderOutput, TextData};
use phantom_adapter::spatial::{InternalLayout, SpatialPreference};
use phantom_adapter::{
    AppCore, BusParticipant, Commandable, InputHandler, Lifecycled, Permissioned, Renderable,
};
use phantom_ui::tokens::Tokens;
use phantom_ui::widgets::AppHead;
use phantom_ui::RenderCtx;
use serde_json::json;

/// Standalone DAG-viewer pane wrapping a [`DagViewerState`].
///
/// Reachable via `Cmd+Shift+A`. Loads its data from `.planning/dag.json`
/// at construction time when available; the App can also push a `CodeDag`
/// via the `load_dag` setter or the `set_dag` command.
pub struct DagViewerAdapter {
    state: DagViewerState,
    dag: Option<CodeDag>,
    app_id: u32,
    tokens: Tokens,
}

impl DagViewerAdapter {
    /// Build an empty viewer. Call [`Self::load_dag`] to populate it.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: DagViewerState::new(),
            dag: None,
            app_id: 0,
            tokens: Tokens::phosphor(RenderCtx::fallback()),
        }
    }

    /// Build a viewer pre-populated from `path` (typically `.planning/dag.json`).
    /// Returns the viewer in its empty state if the file is missing or invalid.
    #[must_use]
    pub fn from_planning_dir<P: AsRef<std::path::Path>>(planning_dir: P) -> Self {
        let mut adapter = Self::new();
        let path = planning_dir.as_ref().join("dag.json");
        if let Ok(contents) = std::fs::read_to_string(&path)
            && let Ok(dag) = CodeDag::from_json(&contents)
        {
            adapter.load_dag(dag);
        }
        adapter
    }

    /// Replace the DAG and recompute the force-directed layout.
    pub fn load_dag(&mut self, dag: CodeDag) {
        self.state.compute_layout(&dag);
        self.dag = Some(dag);
    }

    /// Update the live color palette. The host App calls this on theme switch.
    #[allow(dead_code)]
    pub fn set_tokens(&mut self, tokens: Tokens) {
        self.tokens = tokens;
    }

    /// Reference to the inner viewer state (for tests).
    #[cfg(test)]
    pub(crate) fn state(&self) -> &DagViewerState {
        &self.state
    }
}

impl Default for DagViewerAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl AppCore for DagViewerAdapter {
    fn app_type(&self) -> &str {
        "dag_viewer"
    }

    fn is_alive(&self) -> bool {
        true
    }

    fn update(&mut self, _dt: f32) {}

    fn get_state(&self) -> serde_json::Value {
        json!({
            "type": "dag_viewer",
            "has_dag": self.dag.is_some(),
            "node_count": self.dag.as_ref().map(|d| d.node_count()).unwrap_or(0),
        })
    }

    fn title(&self) -> &str {
        "dag"
    }
}

impl Renderable for DagViewerAdapter {
    fn render(&self, rect: &Rect) -> RenderOutput {
        let t = self.tokens;
        let mut quads: Vec<QuadData> = Vec::new();
        let mut text_segments: Vec<TextData> = Vec::new();

        let meta = match &self.dag {
            Some(dag) => format!(
                "{} node{}  ·  {} edge{}",
                dag.node_count(),
                if dag.node_count() == 1 { "" } else { "s" },
                dag.edge_count(),
                if dag.edge_count() == 1 { "" } else { "s" },
            ),
            None => "no .planning/dag.json".to_string(),
        };
        let head = AppHead::new("DAG", "architecture")
            .with_icon("◯")
            .with_meta(meta)
            .with_tokens(t);
        head.render_into_adapter(rect, &mut quads, &mut text_segments);

        let body = head.body_rect_adapter(rect);
        let cell_w = if rect.cell_size.0 > 0.0 { rect.cell_size.0 } else { 8.0 };
        let cell_h = if rect.cell_size.1 > 0.0 { rect.cell_size.1 } else { 16.0 };

        let Some(ref dag) = self.dag else {
            text_segments.push(TextData {
                text: "No DAG loaded.".to_string(),
                x: body.x + cell_w,
                y: body.y + cell_h,
                color: t.colors.text_secondary,
            });
            text_segments.push(TextData {
                text: "Run `cargo metadata` then phantom dag rebuild to populate .planning/dag.json.".into(),
                x: body.x + cell_w,
                y: body.y + cell_h * 2.5,
                color: t.colors.text_dim,
            });
            return RenderOutput { quads, text_segments, grid: None, scroll: None, selection: None };
        };

        // Force-directed quads, anchored to the centre of the body rect.
        let origin_x = body.x + body.width * 0.5;
        let origin_y = body.y + body.height * 0.5;

        for mut qi in self.state.render_quads(dag) {
            qi.pos[0] += origin_x;
            qi.pos[1] += origin_y;

            // Clip out-of-body quads to keep the head untouched.
            if qi.pos[0] + qi.size[0] < body.x
                || qi.pos[0] > body.x + body.width
                || qi.pos[1] + qi.size[1] < body.y
                || qi.pos[1] > body.y + body.height
            {
                continue;
            }

            quads.push(QuadData {
                x: qi.pos[0],
                y: qi.pos[1],
                w: qi.size[0],
                h: qi.size[1],
                color: qi.color,
            });
        }

        // Label overlays — one short label per node, centred on the box.
        for node in dag.nodes() {
            let Some(&[wx, wy]) = self.state.positions.get(node.id()) else { continue };
            let sx = wx * self.state.zoom + self.state.viewport_offset[0] + origin_x;
            let sy = wy * self.state.zoom + self.state.viewport_offset[1] + origin_y;
            if sx < body.x || sx > body.x + body.width || sy < body.y || sy > body.y + body.height {
                continue;
            }
            let label: String = node.id().chars().take(20).collect();
            text_segments.push(TextData {
                text: label,
                x: sx + cell_w * 0.25,
                y: sy + cell_h * 0.3,
                color: t.colors.text_primary,
            });
        }

        RenderOutput { quads, text_segments, grid: None, scroll: None, selection: None }
    }

    fn is_visual(&self) -> bool {
        true
    }

    fn spatial_preference(&self) -> Option<SpatialPreference> {
        Some(SpatialPreference {
            min_size: (40, 12),
            preferred_size: (80, 30),
            max_size: None,
            aspect_ratio: None,
            internal_panes: 1,
            internal_layout: InternalLayout::Single,
            priority: 4.0,
        })
    }
}

impl InputHandler for DagViewerAdapter {
    fn handle_input(&mut self, _key: &str) -> bool {
        false
    }

    fn accepts_input(&self) -> bool {
        false
    }
}

impl Commandable for DagViewerAdapter {
    fn accept_command(&mut self, cmd: &str, args: &serde_json::Value) -> anyhow::Result<String> {
        match cmd {
            "load_path" => {
                let path = args
                    .get("path")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("load_path requires a 'path' string"))?;
                let contents = std::fs::read_to_string(path)?;
                let dag = CodeDag::from_json(&contents)?;
                self.load_dag(dag);
                Ok("loaded".into())
            }
            "set_dag_json" => {
                let json_str = args
                    .get("json")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("set_dag_json requires a 'json' string"))?;
                let dag = CodeDag::from_json(json_str)?;
                self.load_dag(dag);
                Ok("loaded".into())
            }
            other => Err(anyhow::anyhow!("unknown command: {other}")),
        }
    }
}

impl BusParticipant for DagViewerAdapter {}

impl Lifecycled for DagViewerAdapter {
    fn set_app_id(&mut self, id: u32) {
        self.app_id = id;
    }
}

impl Permissioned for DagViewerAdapter {}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use phantom_dag::{CodeDag, DagEdge, DagNode, EdgeKind, NodeKind};

    use super::*;

    fn node(id: &str) -> DagNode {
        DagNode::new(id.to_owned(), NodeKind::Function, PathBuf::from("src/lib.rs"), 1)
    }

    fn edge(from: &str, to: &str) -> DagEdge {
        DagEdge::new(from.to_owned(), to.to_owned(), EdgeKind::Calls)
    }

    fn three_node_dag() -> CodeDag {
        let mut dag = CodeDag::new();
        dag.add_node(node("a"));
        dag.add_node(node("b"));
        dag.add_node(node("c"));
        dag.add_edge(edge("a", "b"));
        dag.add_edge(edge("b", "c"));
        dag
    }

    #[test]
    fn new_has_defaults() {
        let s = DagViewerState::new();
        assert!((s.zoom - 1.0).abs() < 1e-6);
        assert_eq!(s.viewport_offset, [0.0, 0.0]);
        assert!(s.selected_node.is_none());
        assert!(s.positions.is_empty());
    }

    #[test]
    fn compute_layout_places_all_nodes() {
        let dag = three_node_dag();
        let mut s = DagViewerState::new();
        s.compute_layout(&dag);
        assert_eq!(s.positions.len(), 3);
        assert!(s.positions.contains_key("a"));
        assert!(s.positions.contains_key("b"));
        assert!(s.positions.contains_key("c"));
    }

    #[test]
    fn compute_layout_on_empty_dag_is_noop() {
        let dag = CodeDag::new();
        let mut s = DagViewerState::new();
        s.compute_layout(&dag);
        assert!(s.positions.is_empty());
    }

    #[test]
    fn render_quads_empty_dag_returns_empty_vec() {
        let dag = CodeDag::new();
        let s = DagViewerState::new();
        assert!(s.render_quads(&dag).is_empty());
    }

    #[test]
    fn render_quads_non_empty_dag_returns_quads() {
        let dag = three_node_dag();
        let mut s = DagViewerState::new();
        s.compute_layout(&dag);
        let quads = s.render_quads(&dag);
        // At minimum one quad per node (3 nodes), plus edge quads.
        assert!(quads.len() >= 3);
    }

    #[test]
    fn handle_pan_shifts_offset() {
        let mut s = DagViewerState::new();
        s.handle_pan([10.0, -20.0]);
        assert!((s.viewport_offset[0] - 10.0).abs() < 1e-6);
        assert!((s.viewport_offset[1] - (-20.0)).abs() < 1e-6);
    }

    #[test]
    fn handle_zoom_scales_and_clamps() {
        let mut s = DagViewerState::new();
        s.handle_zoom(2.0);
        assert!((s.zoom - 2.0).abs() < 1e-6);

        // Clamp at ZOOM_MAX.
        for _ in 0..20 {
            s.handle_zoom(10.0);
        }
        assert!(s.zoom <= ZOOM_MAX + 1e-6);

        // Clamp at ZOOM_MIN.
        s.zoom = 1.0;
        for _ in 0..20 {
            s.handle_zoom(0.01);
        }
        assert!(s.zoom >= ZOOM_MIN - 1e-6);
    }

    #[test]
    fn handle_click_selects_node() {
        let dag = three_node_dag();
        let mut s = DagViewerState::new();
        s.compute_layout(&dag);

        // Pick the first node and click in its centre.
        let (id, &[nx, ny]) = s.positions.iter().next().unwrap();
        let id = id.clone();
        let cx = nx + NODE_W * 0.5 + s.viewport_offset[0];
        let cy = ny + NODE_H * 0.5 + s.viewport_offset[1];

        let selected = s.handle_click([cx, cy]);
        assert_eq!(selected, Some(id.clone()));
        assert_eq!(s.selected_node, Some(id));
    }

    #[test]
    fn handle_click_miss_clears_selection() {
        let dag = three_node_dag();
        let mut s = DagViewerState::new();
        s.compute_layout(&dag);
        // Click far outside any node.
        let result = s.handle_click([1_000_000.0, 1_000_000.0]);
        assert!(result.is_none());
        assert!(s.selected_node.is_none());
    }

    #[test]
    fn selected_node_gets_white_quad() {
        let dag = three_node_dag();
        let mut s = DagViewerState::new();
        s.compute_layout(&dag);

        let first_id = s.positions.keys().next().unwrap().clone();
        s.selected_node = Some(first_id.clone());

        let quads = s.render_quads(&dag);
        // The selected node quad must be white-ish.
        let has_white = quads
            .iter()
            .any(|q| q.color[0] > 0.9 && q.color[1] > 0.9 && q.color[2] > 0.9);
        assert!(has_white, "selected node must render as white-ish quad");
    }

    #[test]
    fn instability_score_shifts_color_toward_red() {
        use phantom_dag::NodeOverlay;

        let mut dag = CodeDag::new();
        dag.add_node(node("hot"));

        let mut s = DagViewerState::new();
        s.compute_layout(&dag);

        // First render: no overlay → stable green.
        let stable_quads = s.render_quads(&dag);
        let stable_color = stable_quads.iter().find(|q| q.size[0] > 1.0).unwrap().color;

        // Add high instability overlay.
        s.overlay.insert("hot".to_owned(), NodeOverlay {
            instability_score: 1.0,
            open_tickets: vec![42],
            in_progress_tickets: vec![],
            tickets_closed_last_30d: 0,
        });

        let unstable_quads = s.render_quads(&dag);
        let unstable_color = unstable_quads.iter().find(|q| q.size[0] > 1.0).unwrap().color;

        // Unstable node must be more red and less green than stable node.
        assert!(
            unstable_color[0] > stable_color[0],
            "red channel must increase with instability",
        );
        assert!(
            unstable_color[1] < stable_color[1],
            "green channel must decrease with instability",
        );
    }

    // ---------------------------------------------------------------------
    // DagViewerAdapter tests
    // ---------------------------------------------------------------------

    use super::DagViewerAdapter;
    use phantom_adapter::adapter::Rect;
    use phantom_adapter::{AppCore, Commandable, Renderable};

    #[test]
    fn dag_viewer_adapter_app_type_is_dag_viewer() {
        let a = DagViewerAdapter::new();
        assert_eq!(a.app_type(), "dag_viewer");
    }

    #[test]
    fn dag_viewer_adapter_is_visual() {
        let a = DagViewerAdapter::new();
        assert!(a.is_visual());
    }

    #[test]
    fn dag_viewer_adapter_renders_empty_state_without_panic() {
        let a = DagViewerAdapter::new();
        let rect = Rect {
            x: 0.0,
            y: 0.0,
            width: 800.0,
            height: 600.0,
            cell_size: (8.0, 16.0),
            focused: false,
            elapsed_secs: 0.0,
        };
        let out = a.render(&rect);
        // The empty-state message must appear.
        assert!(
            out.text_segments
                .iter()
                .any(|t| t.text.contains("No DAG loaded")),
            "empty-state hint must render",
        );
    }

    #[test]
    fn dag_viewer_adapter_load_dag_populates_state() {
        let dag = three_node_dag();
        let mut a = DagViewerAdapter::new();
        a.load_dag(dag);
        assert_eq!(a.state().positions.len(), 3);
    }

    #[test]
    fn dag_viewer_adapter_renders_nodes_when_dag_loaded() {
        let dag = three_node_dag();
        let mut a = DagViewerAdapter::new();
        a.load_dag(dag);
        let rect = Rect {
            x: 0.0,
            y: 0.0,
            width: 1200.0,
            height: 800.0,
            cell_size: (8.0, 16.0),
            focused: false,
            elapsed_secs: 0.0,
        };
        let out = a.render(&rect);
        // At least the head quads plus some node quads should render.
        assert!(out.quads.len() > 2, "should render >2 quads (head + nodes)");
    }

    #[test]
    fn dag_viewer_adapter_set_dag_json_command_loads_dag() {
        let dag = three_node_dag();
        let json_str = dag.to_json().unwrap();
        let mut a = DagViewerAdapter::new();
        let result = a
            .accept_command("set_dag_json", &serde_json::json!({"json": json_str}))
            .unwrap();
        assert_eq!(result, "loaded");
        assert!(a.dag.is_some());
        assert_eq!(a.dag.as_ref().unwrap().node_count(), 3);
    }

    #[test]
    fn dag_viewer_adapter_set_dag_json_command_rejects_invalid() {
        let mut a = DagViewerAdapter::new();
        let result =
            a.accept_command("set_dag_json", &serde_json::json!({"json": "not json"}));
        assert!(result.is_err());
    }

    #[test]
    fn dag_viewer_adapter_from_planning_dir_handles_missing_file() {
        // Pass a non-existent directory; we must get an empty adapter,
        // not a panic.
        let a = DagViewerAdapter::from_planning_dir("/tmp/nonexistent-path-xyz123");
        assert!(a.dag.is_none());
    }
}
