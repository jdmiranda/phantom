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
}
