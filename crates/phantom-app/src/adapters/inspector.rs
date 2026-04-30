//! Inspector adapter — wraps an `InspectorView` snapshot as an `AppAdapter`.
//!
//! Bridges the substrate's agent registry and event log into a live,
//! read-only pane. The pane is fed by the App: each `update()` cycle the App
//! pushes a fresh [`InspectorView`] into the adapter's shared snapshot,
//! and `render()` lays the snapshot out into quads + text segments.
//!
//! ## Snapshot ownership
//!
//! The adapter holds an `Arc<RwLock<InspectorView>>` rather than a clone, so
//! the App can push new snapshots without cloning the whole view structure
//! through the coordinator's mutable adapter API. The adapter takes a read
//! lock during `render()` (cheap, no contention with the writer).
//!
//! ## Live design tokens
//!
//! The adapter holds an `Arc<RwLock<Tokens>>` shared with the App. Any theme
//! change the App writes into the lock is picked up at the next `render()`
//! call — no adapter restart needed. `render()` takes a short read lock to
//! snapshot the current token values and releases it before laying out quads.
//!
//! ## Read-only by design
//!
//! `accepts_input` is `false` and `accepts_commands` is `false` — the
//! inspector is a window into substrate state, not a control surface. Click
//! handling and "kill agent" actions belong in a separate phase.

use std::sync::{Arc, RwLock};

use phantom_adapter::adapter::{QuadData, Rect, RenderOutput, TextData};
use phantom_adapter::spatial::{InternalLayout, SpatialPreference};
use phantom_adapter::{
    AppCore, BusParticipant, Commandable, InputHandler, Lifecycled, Permissioned, Renderable,
};

use phantom_agents::inspector::InspectorView;
use phantom_dag::CodeDag;
use phantom_ui::tokens::Tokens;

use crate::adapters::dag_viewer::DagViewerState;

/// Number of events shown at the bottom of the pane (fixed cap, separate from
/// the snapshot's `recent_events` cap).
const VISIBLE_EVENT_ROWS: usize = 20;
/// Number of denials shown in the Denials section. The snapshot is already
/// capped at `MAX_RECENT_DENIALS = 20`; this fixed cap is independent so the
/// renderer can stop laying out rows once the pane runs out of vertical room.
const VISIBLE_DENIAL_ROWS: usize = 20;

// ---------------------------------------------------------------------------
// Tab enum
// ---------------------------------------------------------------------------

/// Which inspector tab is currently visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InspectorTab {
    /// Agents / events / denials overview (default).
    #[default]
    Overview,
    /// Live crate dependency graph with ticket-instability colours.
    Dag,
}

impl InspectorTab {
    /// Cycle to the next tab in declaration order.
    fn next(self) -> Self {
        match self {
            Self::Overview => Self::Dag,
            Self::Dag => Self::Overview,
        }
    }

    /// Human-readable tab name for future tab-bar rendering.
    #[allow(dead_code)]
    fn label(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Dag => "DAG",
        }
    }
}

// ---------------------------------------------------------------------------
// Adapter
// ---------------------------------------------------------------------------

/// Live inspector pane for the substrate runtime.
///
/// Holds an `Arc<RwLock<InspectorView>>` whose contents are pushed by the
/// App at the end of each `update()` cycle. `render()` reads the snapshot
/// without copying.
///
/// Also holds an `Arc<RwLock<Tokens>>` so that theme changes propagate to
/// the inspector UI in real time: any write to the shared token lock is
/// visible at the next `render()` call without restarting the adapter.
///
/// The pane has two tabs ([`InspectorTab`]):
/// - **Overview** — agents, events, denials (original view).
/// - **DAG** — force-directed crate dependency graph with instability colours.
///
/// Press `Tab` to cycle between tabs.
pub struct InspectorAdapter {
    snapshot: Arc<RwLock<InspectorView>>,
    tokens: Arc<RwLock<Tokens>>,
    app_id: u32,
    /// Currently visible tab.
    active_tab: InspectorTab,
    /// Force-directed layout state for the DAG tab.
    dag_viewer: DagViewerState,
    /// The DAG being visualised. `None` until one is loaded.
    dag: Option<CodeDag>,
}

impl InspectorAdapter {
    /// Build an adapter sharing `snapshot` and `tokens` with the App.
    ///
    /// The App should hold its own `Arc` clones of both locks so it can
    /// push fresh snapshots and update the active theme each frame.
    #[allow(dead_code)] // Phase 2.G+: spawn-inspector-pane wiring is staged.
    pub(crate) fn new(snapshot: Arc<RwLock<InspectorView>>, tokens: Arc<RwLock<Tokens>>) -> Self {
        Self {
            snapshot,
            tokens,
            app_id: 0,
            active_tab: InspectorTab::Overview,
            dag_viewer: DagViewerState::new(),
            dag: None,
        }
    }

    /// Load a [`CodeDag`] into the DAG tab and compute the initial
    /// force-directed layout. Call this after acquiring a DAG from disk or
    /// from the planning pipeline.
    pub fn load_dag(&mut self, dag: CodeDag) {
        self.dag_viewer.compute_layout(&dag);
        self.dag = Some(dag);
    }

    /// Test-only constructor that wraps an existing view with phosphor tokens.
    #[cfg(test)]
    pub(crate) fn with_view(view: InspectorView) -> Self {
        use phantom_ui::RenderCtx;
        let tokens = Tokens::phosphor(RenderCtx::fallback());
        Self::new(Arc::new(RwLock::new(view)), Arc::new(RwLock::new(tokens)))
    }

    /// Test-only constructor that wraps an existing view with custom tokens.
    #[cfg(test)]
    pub(crate) fn with_view_and_tokens(view: InspectorView, tokens: Tokens) -> Self {
        Self::new(Arc::new(RwLock::new(view)), Arc::new(RwLock::new(tokens)))
    }

    // -----------------------------------------------------------------------
    // DAG tab rendering
    // -----------------------------------------------------------------------

    /// Render the DAG tab into `quads` and `text_segments` vectors.
    ///
    /// When no DAG has been loaded, renders a centered "No DAG loaded" hint
    /// rather than panicking.
    fn render_dag_tab(
        &self,
        rect: &Rect,
        quads: &mut Vec<QuadData>,
        text_segments: &mut Vec<TextData>,
        colors: phantom_ui::tokens::ColorRoles,
    ) {
        let cell_w = if rect.cell_size.0 > 0.0 { rect.cell_size.0 } else { 8.0 };
        let cell_h = if rect.cell_size.1 > 0.0 { rect.cell_size.1 } else { 16.0 };
        let pad_x = cell_w;
        let pad_y = cell_h * 0.4;

        // Header bar.
        let header_h = cell_h * 1.6;
        quads.push(QuadData {
            x: rect.x,
            y: rect.y,
            w: rect.width,
            h: header_h,
            color: colors.surface_recessed,
        });
        text_segments.push(TextData {
            text: "INSPECTOR — DAG  [Tab] switch tab".to_string(),
            x: rect.x + pad_x,
            y: rect.y + pad_y,
            color: colors.text_accent,
        });

        let content_y = rect.y + header_h + pad_y;

        match &self.dag {
            None => {
                // Empty state — no DAG loaded yet.
                text_segments.push(TextData {
                    text: "No DAG loaded".to_string(),
                    x: rect.x + pad_x,
                    y: content_y,
                    color: colors.text_secondary,
                });
                text_segments.push(TextData {
                    text: "Load a CodeDag via InspectorAdapter::load_dag()".to_string(),
                    x: rect.x + pad_x,
                    y: content_y + cell_h,
                    color: colors.text_dim,
                });
            }
            Some(dag) => {
                // Render node/edge quads from the force-directed layout.
                // The layout uses world-space coordinates; offset them by
                // the pane origin so they land inside the pane rectangle.
                //
                // We translate world positions by (rect.x, content_y) so
                // the DAG renders within the pane bounds.
                let origin_x = rect.x + rect.width * 0.5;
                let origin_y = content_y + (rect.height - header_h) * 0.5;

                for mut qi in self.dag_viewer.render_quads(dag) {
                    // Centre the DAG within the pane.
                    qi.pos[0] += origin_x;
                    qi.pos[1] += origin_y;

                    // Skip quads that fall entirely outside the pane.
                    if qi.pos[0] + qi.size[0] < rect.x
                        || qi.pos[0] > rect.x + rect.width
                        || qi.pos[1] + qi.size[1] < content_y
                        || qi.pos[1] > rect.y + rect.height
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

                // Node label overlays.
                for node in dag.nodes() {
                    let Some(&[wx, wy]) = self.dag_viewer.positions.get(node.id()) else { continue };

                    let sx = wx * self.dag_viewer.zoom
                        + self.dag_viewer.viewport_offset[0]
                        + origin_x;
                    let sy = wy * self.dag_viewer.zoom
                        + self.dag_viewer.viewport_offset[1]
                        + origin_y;

                    if sx < rect.x || sx > rect.x + rect.width
                        || sy < content_y || sy > rect.y + rect.height
                    {
                        continue;
                    }

                    // Shorten the label to the crate/module leaf.
                    let label = node.id().split("::").last().unwrap_or(node.id());
                    text_segments.push(TextData {
                        text: label.to_string(),
                        x: sx + cell_w * 0.3,
                        y: sy + cell_h * 0.2,
                        color: colors.text_primary,
                    });
                }

                // Node count summary line below the graph.
                text_segments.push(TextData {
                    text: format!(
                        "{} nodes  {} edges",
                        dag.node_count(),
                        dag.edge_count()
                    ),
                    x: rect.x + pad_x,
                    y: rect.y + rect.height - cell_h * 1.5,
                    color: colors.text_dim,
                });
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Sub-trait implementations
// ---------------------------------------------------------------------------

impl AppCore for InspectorAdapter {
    fn app_type(&self) -> &str {
        "inspector"
    }

    fn is_alive(&self) -> bool {
        true
    }

    fn update(&mut self, _dt: f32) {
        // No-op: the App is the snapshot producer and writes through the
        // shared `Arc<RwLock<InspectorView>>` outside of `update()`. The
        // adapter only reads.
    }

    fn get_state(&self) -> serde_json::Value {
        let view = self.snapshot.read().expect("inspector snapshot lock");
        serde_json::json!({
            "type": "inspector",
            "agents": view.agents.len(),
            "spawned_total": view.spawned_total,
            "running_count": view.running_count,
            "recent_events": view.recent_events.len(),
            "denials": view.denials.len(),
        })
    }
}

impl Renderable for InspectorAdapter {
    fn render(&self, rect: &Rect) -> RenderOutput {
        let mut quads = Vec::new();
        let mut text_segments = Vec::new();

        // Snapshot the current design tokens. The read lock is released
        // immediately after the copy so it cannot block theme writes.
        let colors = {
            let tok = self.tokens.read().expect("inspector tokens lock");
            tok.colors
        };

        // ── Dispatch to active tab ─────────────────────────────────────
        if self.active_tab == InspectorTab::Dag {
            // Pane background.
            quads.push(QuadData {
                x: rect.x,
                y: rect.y,
                w: rect.width,
                h: rect.height,
                color: [0.0_f32, 0.0, 0.0, 0.0],
            });
            self.render_dag_tab(rect, &mut quads, &mut text_segments, colors);
            return RenderOutput {
                quads,
                text_segments,
                grid: None,
                scroll: None,
                selection: None,
            };
        }

        // Derive role-specific colors from the live token palette.
        let header_bg = colors.surface_recessed;
        let header_color = colors.text_accent;
        let section_color = colors.text_primary;
        let agent_color = colors.text_primary;
        let event_color = colors.text_secondary;
        let stamp_color = colors.text_dim;
        let pane_bg = [0.0_f32, 0.0, 0.0, 0.0];
        // Denial colors are drawn from the `status_danger` token so that
        // theme changes (e.g. amber → blood) propagate to the security UI.
        let denial_header = colors.status_danger;
        // Row body: same hue but slightly softened — reduce alpha slightly.
        let denial_row = {
            let [r, g, b, _] = colors.status_danger;
            [r * 0.95, g * 1.5_f32.min(1.0), b * 1.6_f32.min(1.0), 0.95]
        };
        // Chain sub-row: mix toward text_dim to push it into the background.
        let denial_chain = {
            let [r, g, b, _] = colors.status_danger;
            let [dr, dg, db, _] = colors.text_dim;
            [(r + dr) * 0.5, (g + dg) * 0.5, (b + db) * 0.5, 0.80]
        };

        // Pull live cell metrics from the rect rather than baking constants;
        // doubling the font size doubles every spacing/positioning value.
        // `Rect::default()` carries `(0.0, 0.0)` as a "not provided" sentinel
        // so we fall back to legacy 8.0 / 16.0 for callers that don't pass
        // cell metrics through.
        let cell_w = if rect.cell_size.0 > 0.0 {
            rect.cell_size.0
        } else {
            8.0
        };
        let cell_h = if rect.cell_size.1 > 0.0 {
            rect.cell_size.1
        } else {
            16.0
        };
        let pad_x = cell_w; // 1 cell of left padding.
        let pad_y = cell_h * 0.4; // ~half a line of top padding.

        // ── Pane background ────────────────────────────────────────────
        quads.push(QuadData {
            x: rect.x,
            y: rect.y,
            w: rect.width,
            h: rect.height,
            color: pane_bg,
        });

        let view = self.snapshot.read().expect("inspector snapshot lock");

        // ── Header bar ─────────────────────────────────────────────────
        let header_h = cell_h * 1.6;
        quads.push(QuadData {
            x: rect.x,
            y: rect.y,
            w: rect.width,
            h: header_h,
            color: header_bg,
        });

        let header_text = format!(
            "INSPECTOR — {} agents running ({} total)",
            view.running_count, view.spawned_total,
        );
        text_segments.push(TextData {
            text: header_text,
            x: rect.x + pad_x,
            y: rect.y + pad_y,
            color: header_color,
        });

        // Refresh stamp at the right edge of the header bar.
        let stamp_text = format!("events: {}", view.recent_events.len());
        // Right-align by columns (string length × cell width). Best-effort:
        // very long stamps still draw inside the pane because pad_x bounds
        // the left side.
        let stamp_x = rect.x + rect.width - pad_x - (stamp_text.chars().count() as f32) * cell_w;
        text_segments.push(TextData {
            text: stamp_text,
            x: stamp_x.max(rect.x + pad_x),
            y: rect.y + pad_y,
            color: stamp_color,
        });

        // Cursor advances down the pane as we lay out sections.
        let mut cursor_y = rect.y + header_h + pad_y;

        // ── Agents section title ───────────────────────────────────────
        text_segments.push(TextData {
            text: "AGENTS".to_string(),
            x: rect.x + pad_x,
            y: cursor_y,
            color: section_color,
        });
        cursor_y += cell_h * 1.2;

        // ── One row per agent ──────────────────────────────────────────
        // Format: `[role] label │ status │ N min ago`. We use ASCII pipes
        // so the renderer doesn't need any wide-glyph handling.
        for row in &view.agents {
            let line = format!(
                "[{role}] {label:<24} | {status:<14} | {age} min ago",
                role = row.agent_ref.role.label(),
                label = truncate_label(&row.agent_ref.label, 24),
                status = row.status,
                age = row.spawned_minutes_ago,
            );
            text_segments.push(TextData {
                text: line,
                x: rect.x + pad_x * 2.0, // indent agent rows one cell deeper.
                y: cursor_y,
                color: agent_color,
            });
            cursor_y += cell_h;

            // Stop laying out agent rows if we run out of pane height.
            if cursor_y > rect.y + rect.height - cell_h * 4.0 {
                break;
            }
        }

        // Empty-state hint when no agents are running.
        if view.agents.is_empty() {
            text_segments.push(TextData {
                text: "  (no agents)".to_string(),
                x: rect.x + pad_x * 2.0,
                y: cursor_y,
                color: event_color,
            });
            cursor_y += cell_h;
        }

        // ── Recent events section ──────────────────────────────────────
        cursor_y += cell_h * 0.5;
        text_segments.push(TextData {
            text: "RECENT EVENTS".to_string(),
            x: rect.x + pad_x,
            y: cursor_y,
            color: section_color,
        });
        cursor_y += cell_h * 1.2;

        // Show the newest events first, capped at VISIBLE_EVENT_ROWS. The
        // snapshot stores newest-last in `recent_events`, so iterate
        // backwards and clamp.
        let total = view.recent_events.len();
        let take = total.min(VISIBLE_EVENT_ROWS);
        for ev in view.recent_events.iter().rev().take(take) {
            // Truncate very long summaries to roughly the pane width.
            let max_chars = ((rect.width / cell_w).floor() as usize).saturating_sub(4);
            let summary = if ev.summary.chars().count() > max_chars {
                let cut: String = ev
                    .summary
                    .chars()
                    .take(max_chars.saturating_sub(1))
                    .collect();
                format!("{cut}…")
            } else {
                ev.summary.clone()
            };
            text_segments.push(TextData {
                text: summary,
                x: rect.x + pad_x * 2.0,
                y: cursor_y,
                color: event_color,
            });
            cursor_y += cell_h;
            if cursor_y > rect.y + rect.height - cell_h {
                break;
            }
        }

        if view.recent_events.is_empty() {
            text_segments.push(TextData {
                text: "  (no recent events)".to_string(),
                x: rect.x + pad_x * 2.0,
                y: cursor_y,
                color: event_color,
            });
            cursor_y += cell_h;
        }

        // ── Denials section (Sec.3) ───────────────────────────────────
        // Surface `EventKind::CapabilityDenied` events the runtime has
        // recorded since the inspector pane was opened. This is the
        // user-facing window into the substrate's security boundary:
        // the rows show *which* agent tried *which* tool under *which*
        // capability class, with the source chain underneath as
        // provenance.
        cursor_y += cell_h * 0.5;
        text_segments.push(TextData {
            text: "DENIALS".to_string(),
            x: rect.x + pad_x,
            y: cursor_y,
            color: denial_header,
        });
        cursor_y += cell_h * 1.2;

        if view.denials.is_empty() {
            text_segments.push(TextData {
                text: "  (no denials)".to_string(),
                x: rect.x + pad_x * 2.0,
                y: cursor_y,
                color: event_color,
            });
        } else {
            // Newest-on-top: snapshot pushes oldest-first, so iterate
            // in reverse and clamp to VISIBLE_DENIAL_ROWS.
            let total = view.denials.len();
            let take = total.min(VISIBLE_DENIAL_ROWS);
            for entry in view.denials.iter().rev().take(take) {
                // Stop laying out denial rows when we run out of vertical
                // pane room. Each entry consumes two cell-height rows
                // (primary + chain), so reserve at least 2 cells.
                if cursor_y > rect.y + rect.height - cell_h * 2.0 {
                    break;
                }

                let primary = format!(
                    "{role} \u{2192} {tool} ({class})",
                    role = entry.role,
                    tool = entry.attempted_tool,
                    class = entry.attempted_class,
                );
                text_segments.push(TextData {
                    text: primary,
                    x: rect.x + pad_x * 2.0,
                    y: cursor_y,
                    color: denial_row,
                });
                cursor_y += cell_h;

                // Source chain sub-row. When the chain is empty (Sec.2
                // hasn't filled it yet), we still render the prefix so
                // the user knows the chain field exists.
                let chain_text = format_source_chain(&entry.source_chain);
                text_segments.push(TextData {
                    text: chain_text,
                    x: rect.x + pad_x * 3.0, // double-indent under the primary row
                    y: cursor_y,
                    color: denial_chain,
                });
                cursor_y += cell_h;
            }
        }

        // ── Peers section (connected peers + grants) ───────────────────────
        cursor_y += cell_h * 0.5;
        text_segments.push(TextData {
            text: "PEERS".to_string(),
            x: rect.x + pad_x,
            y: cursor_y,
            color: section_color,
        });
        cursor_y += cell_h * 1.2;

        // Local node identity header.
        text_segments.push(TextData {
            text: format!("Local: {}", view.local_node_id),
            x: rect.x + pad_x * 2.0,
            y: cursor_y,
            color: agent_color,
        });
        cursor_y += cell_h;

        // Peer rows: peer_id + granted capabilities.
        if view.peers.is_empty() {
            text_segments.push(TextData {
                text: "  (no connected peers)".to_string(),
                x: rect.x + pad_x * 2.0,
                y: cursor_y,
                color: event_color,
            });
        } else {
            for peer in &view.peers {
                if cursor_y > rect.y + rect.height - cell_h {
                    break;
                }

                // Format peer line: truncated peer_id, display name, and capability badges.
                let truncated_peer_id = truncate_label(&peer.peer_id.to_string(), 12);
                let caps_str = if peer.granted_capabilities.is_empty() {
                    "(no caps)".to_string()
                } else {
                    peer.granted_capabilities
                        .iter()
                        .map(|cap| format!("{:?}", cap).chars().next().unwrap_or('?').to_string())
                        .collect::<Vec<_>>()
                        .join("")
                };
                let peer_line = format!(
                    "{:<14} {:<20} [{}]",
                    truncated_peer_id,
                    truncate_label(&peer.display_name, 20),
                    caps_str
                );
                text_segments.push(TextData {
                    text: peer_line,
                    x: rect.x + pad_x * 2.0,
                    y: cursor_y,
                    color: agent_color,
                });
                cursor_y += cell_h;
            }
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
            min_size: (40, 12),
            preferred_size: (60, 24),
            max_size: Some((120, 60)),
            aspect_ratio: None,
            internal_panes: 1,
            internal_layout: InternalLayout::Single,
            priority: 3.0,
        })
    }
}

impl InputHandler for InspectorAdapter {
    fn handle_input(&mut self, key: &str) -> bool {
        // Tab key cycles between inspector tabs.
        if key == "Tab" {
            self.active_tab = self.active_tab.next();
            return true;
        }
        // All other keys are not consumed — the inspector remains read-only.
        false
    }

    fn accepts_input(&self) -> bool {
        // Accept Tab key for switching tabs; all other input is rejected at
        // the caller by checking `handle_input`'s return value.
        true
    }
}

impl Commandable for InspectorAdapter {
    fn accept_command(&mut self, cmd: &str, _args: &serde_json::Value) -> anyhow::Result<String> {
        Err(anyhow::anyhow!(
            "inspector adapter does not accept commands: {cmd}"
        ))
    }

    fn accepts_commands(&self) -> bool {
        false
    }
}

impl BusParticipant for InspectorAdapter {}

impl Lifecycled for InspectorAdapter {
    fn set_app_id(&mut self, id: u32) {
        self.app_id = id;
    }
}

impl Permissioned for InspectorAdapter {}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Truncate a label to `max_chars` Unicode scalar values, padding with `…`
/// if truncated. Returns the original label unchanged when it already fits.
fn truncate_label(label: &str, max_chars: usize) -> String {
    if label.chars().count() <= max_chars {
        return label.to_string();
    }
    let cut: String = label.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{cut}…")
}

/// Render a source chain as the spec'd `chain: 123\u{2192}456\u{2192}here`
/// string. An empty chain renders as `chain: (empty)` so the user can tell
/// the chain field exists but provenance hasn't been wired yet (Sec.2).
fn format_source_chain(chain: &[u64]) -> String {
    if chain.is_empty() {
        return "chain: (empty)".to_string();
    }
    let mut s = String::from("chain: ");
    for id in chain {
        s.push_str(&id.to_string());
        s.push('\u{2192}'); // unicode RIGHTWARDS ARROW
    }
    s.push_str("here");
    s
}

// ---------------------------------------------------------------------------
// Compile-time Send assert
// ---------------------------------------------------------------------------

fn _assert_send() {
    fn _check<T: Send>() {}
    _check::<InspectorAdapter>();
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use phantom_agents::inspector::{AgentRow, EventRow, InspectorBuilder};
    use phantom_agents::role::{AgentRef, AgentRole, SpawnSource};

    fn agent_ref(id: u64, role: AgentRole, label: &str, spawned_ms: u64) -> AgentRef {
        AgentRef {
            id,
            role,
            label: label.to_string(),
            spawned_at_unix_ms: spawned_ms,
            spawned_by: SpawnSource::Substrate,
        }
    }

    fn make_rect(cell_w: f32) -> Rect {
        Rect {
            x: 0.0,
            y: 0.0,
            width: 1200.0,
            height: 800.0,
            cell_size: (cell_w, cell_w * 2.0),
        }
    }

    fn build_view_with_agents(n: usize) -> InspectorView {
        let mut b = InspectorBuilder::new();
        for i in 0..n {
            let label = format!("agent-{i}");
            let r = agent_ref(i as u64, AgentRole::Watcher, &label, 0);
            b = b.with_agent(AgentRow::new(r, "Idle", None, None, 0, 0));
        }
        b.build()
    }

    /// Header text must announce the running-agent count.
    #[test]
    fn inspector_adapter_renders_header_with_agent_count() {
        let view = build_view_with_agents(3);
        let adapter = InspectorAdapter::with_view(view);
        let output = adapter.render(&make_rect(8.0));

        let header = output
            .text_segments
            .iter()
            .find(|t| t.text.starts_with("INSPECTOR"))
            .expect("header text must be present");
        assert!(
            header.text.contains("3 agents"),
            "header should mention running count of 3, got: {}",
            header.text,
        );
    }

    /// Each agent row must surface the agent's label as a distinct
    /// rendered text segment.
    #[test]
    fn inspector_adapter_renders_one_row_per_agent() {
        let view = build_view_with_agents(4);
        let adapter = InspectorAdapter::with_view(view);
        let output = adapter.render(&make_rect(8.0));

        for i in 0..4 {
            let label = format!("agent-{i}");
            let found = output.text_segments.iter().any(|t| t.text.contains(&label));
            assert!(found, "expected text segment containing label {label}");
        }
    }

    /// Doubling `cell_size.0` (cell width) must shift the rendered text x
    /// positions by 2x. This is the contract that proves the adapter pulls
    /// metrics off the rect rather than baking a constant.
    #[test]
    fn inspector_adapter_uses_render_ctx_cell_metrics() {
        let view = build_view_with_agents(2);
        let adapter_a = InspectorAdapter::with_view(view.clone());
        let adapter_b = InspectorAdapter::with_view(view);

        let small = adapter_a.render(&make_rect(8.0));
        let big = adapter_b.render(&make_rect(16.0));

        // Pluck the agent-row x coordinate (the row containing "agent-0").
        let small_x = small
            .text_segments
            .iter()
            .find(|t| t.text.contains("agent-0"))
            .map(|t| t.x)
            .expect("small render: agent-0 row must exist");
        let big_x = big
            .text_segments
            .iter()
            .find(|t| t.text.contains("agent-0"))
            .map(|t| t.x)
            .expect("big render: agent-0 row must exist");

        // Agent rows are at `pad_x * 2.0 = cell_w * 2.0`. Doubling cell_w
        // doubles the x position.
        assert!(
            (small_x - 16.0).abs() < 0.001,
            "small cell_w=8 -> x=16, got {small_x}",
        );
        assert!(
            (big_x - 32.0).abs() < 0.001,
            "big cell_w=16 -> x=32, got {big_x}",
        );
        assert!(
            (big_x - 2.0 * small_x).abs() < 0.001,
            "big x must be 2x small (small={small_x}, big={big_x})",
        );
    }

    /// Empty snapshot must still render without panic and surface a hint.
    #[test]
    fn inspector_adapter_handles_empty_view() {
        let adapter = InspectorAdapter::with_view(InspectorView::empty());
        let output = adapter.render(&make_rect(8.0));
        assert!(
            output
                .text_segments
                .iter()
                .any(|t| t.text.contains("no agents"))
        );
        assert!(
            output
                .text_segments
                .iter()
                .any(|t| t.text.contains("no recent events"))
        );
    }

    /// Recent events must appear in the rendered text segments, formatted
    /// via `summarize_event` upstream (we just assert pass-through).
    #[test]
    fn inspector_adapter_renders_recent_events() {
        let view = InspectorBuilder::new()
            .with_event(EventRow {
                id: 1,
                ts_ms: 100,
                source_label: "Substrate".into(),
                kind: "agent.spawn".into(),
                summary: "Spawned Watcher 'scout' (id=1)".into(),
            })
            .build();
        let adapter = InspectorAdapter::with_view(view);
        let output = adapter.render(&make_rect(8.0));
        let found = output
            .text_segments
            .iter()
            .any(|t| t.text.contains("Spawned Watcher"));
        assert!(found, "expected event summary text in rendered output");
    }

    #[test]
    fn inspector_adapter_app_type_is_inspector() {
        let adapter = InspectorAdapter::with_view(InspectorView::empty());
        assert_eq!(adapter.app_type(), "inspector");
    }

    #[test]
    fn inspector_adapter_tab_key_cycles_tabs() {
        let mut adapter = InspectorAdapter::with_view(InspectorView::empty());
        // Tab key is accepted and cycles between tabs.
        assert!(adapter.accepts_input());
        assert_eq!(adapter.active_tab, InspectorTab::Overview);
        assert!(adapter.handle_input("Tab"));
        assert_eq!(adapter.active_tab, InspectorTab::Dag);
        assert!(adapter.handle_input("Tab"));
        assert_eq!(adapter.active_tab, InspectorTab::Overview);
        // Other keys are not consumed.
        assert!(!adapter.handle_input("q"));
        assert!(!adapter.handle_input("Enter"));
    }

    #[test]
    fn inspector_adapter_does_not_accept_commands() {
        let mut adapter = InspectorAdapter::with_view(InspectorView::empty());
        assert!(!adapter.accepts_commands());
        let res = adapter.accept_command("anything", &serde_json::json!({}));
        assert!(res.is_err());
    }

    #[test]
    fn inspector_adapter_get_state_reports_counts() {
        let view = build_view_with_agents(2);
        let adapter = InspectorAdapter::with_view(view);
        let state = adapter.get_state();
        assert_eq!(state["agents"], 2);
        assert_eq!(state["type"], "inspector");
    }

    #[test]
    fn inspector_adapter_send_assert() {
        fn _check<T: Send>() {}
        _check::<InspectorAdapter>();
    }

    #[test]
    fn truncate_label_short_passes_through() {
        assert_eq!(truncate_label("hi", 10), "hi");
    }

    #[test]
    fn truncate_label_long_is_clipped() {
        let s = truncate_label("a-really-long-agent-label", 8);
        assert_eq!(s.chars().count(), 8);
        assert!(s.ends_with('…'));
    }

    // ---- Sec.3: Denials section ------------------------------------------

    #[test]
    fn format_source_chain_empty_renders_marker() {
        assert_eq!(format_source_chain(&[]), "chain: (empty)");
    }

    #[test]
    fn format_source_chain_renders_arrow_separated_with_here_terminator() {
        // Spec: `chain: 123\u{2192}456\u{2192}here`
        let s = format_source_chain(&[123, 456]);
        assert_eq!(s, "chain: 123\u{2192}456\u{2192}here");
    }

    #[test]
    fn inspector_adapter_renders_no_denials_hint_when_empty() {
        let adapter = InspectorAdapter::with_view(InspectorView::empty());
        let output = adapter.render(&make_rect(8.0));
        assert!(
            output.text_segments.iter().any(|t| t.text == "DENIALS"),
            "DENIALS section header must render",
        );
        assert!(
            output
                .text_segments
                .iter()
                .any(|t| t.text.contains("no denials")),
            "empty denials list must surface a hint",
        );
    }

    #[test]
    fn inspector_adapter_renders_denial_row_with_role_tool_class() {
        use phantom_agents::inspector::DenialEntry;
        let view = phantom_agents::inspector::InspectorBuilder::new()
            .with_denial(DenialEntry {
                role: "Watcher".to_string(),
                attempted_tool: "run_command".to_string(),
                attempted_class: "Act".to_string(),
                source_chain: vec![123, 456],
                timestamp_ms: 1_700_000_000_000,
            })
            .build();
        let adapter = InspectorAdapter::with_view(view);
        let output = adapter.render(&make_rect(8.0));

        // Primary row carries `role -> tool (class)`.
        let primary = output
            .text_segments
            .iter()
            .find(|t| t.text.starts_with("Watcher"))
            .expect("denial primary row must be present");
        assert!(primary.text.contains("run_command"));
        assert!(primary.text.contains("(Act)"));
        assert!(primary.text.contains('\u{2192}'));

        // Source-chain sub-row: `chain: 123→456→here`.
        let chain = output
            .text_segments
            .iter()
            .find(|t| t.text.starts_with("chain:") && t.text.contains("here"))
            .expect("source-chain sub-row must be present");
        assert!(chain.text.contains("123"));
        assert!(chain.text.contains("456"));

        // Header color must match the live token's status_danger value.
        // With default phosphor tokens: status_danger = [1.00, 0.30, 0.25, 1.00].
        let header = output
            .text_segments
            .iter()
            .find(|t| t.text == "DENIALS")
            .expect("DENIALS header must render");
        // The DENIALS header reads from tokens.colors.status_danger, so its
        // red channel must be dominant (> 0.8) to distinguish it from regular
        // text colors.
        assert!(
            header.color[0] > 0.8,
            "DENIALS header must be red-dominant (status_danger), got {:?}",
            header.color,
        );
    }

    #[test]
    fn inspector_adapter_renders_empty_chain_marker_when_provenance_missing() {
        // Sec.2 hasn't filled source_chain on every dispatch yet; the row
        // must still render with a `(empty)` marker so the chain field is
        // visible to the user.
        use phantom_agents::inspector::DenialEntry;
        let view = phantom_agents::inspector::InspectorBuilder::new()
            .with_denial(DenialEntry {
                role: "Actor".to_string(),
                attempted_tool: "phantom.spawn_agent".to_string(),
                attempted_class: "Coordinate".to_string(),
                source_chain: Vec::new(),
                timestamp_ms: 0,
            })
            .build();
        let adapter = InspectorAdapter::with_view(view);
        let output = adapter.render(&make_rect(8.0));
        assert!(
            output
                .text_segments
                .iter()
                .any(|t| t.text == "chain: (empty)")
        );
    }

    // ---- Issue #31: live Tokens propagation --------------------------------

    /// Verify that `InspectorAdapter::new` accepts a `Tokens` arc and that
    /// the constructor compiles without the test helper wrapper.
    #[test]
    fn inspector_adapter_new_accepts_tokens_arc() {
        use phantom_ui::RenderCtx;
        let snapshot = Arc::new(RwLock::new(InspectorView::empty()));
        let tokens = Arc::new(RwLock::new(Tokens::phosphor(RenderCtx::fallback())));
        let adapter = InspectorAdapter::new(snapshot, tokens);
        assert_eq!(adapter.app_type(), "inspector");
    }

    /// The DENIALS header must read its color from `tokens.colors.status_danger`
    /// rather than a baked-in constant. This test builds two adapters with
    /// contrasting token palettes and asserts that their DENIALS header colors
    /// differ — proving the live plumbing is in place.
    #[test]
    fn denials_header_color_changes_with_tokens() {
        use phantom_ui::RenderCtx;
        use phantom_ui::tokens::{ColorRoles, Tokens};

        // Phosphor tokens: status_danger is red-dominant (r ≈ 1.0).
        let phosphor_tokens = Tokens::phosphor(RenderCtx::fallback());

        // Custom "safe" palette: make status_danger blue-dominant (b = 1.0, r ≈ 0).
        let mut blue_roles = ColorRoles::phosphor();
        blue_roles.status_danger = [0.0, 0.2, 1.0, 1.0];
        let blue_tokens = Tokens::new(blue_roles, RenderCtx::fallback());

        let view = InspectorView::empty();
        let adapter_phosphor =
            InspectorAdapter::with_view_and_tokens(view.clone(), phosphor_tokens);
        let adapter_blue = InspectorAdapter::with_view_and_tokens(view, blue_tokens);

        let out_p = adapter_phosphor.render(&make_rect(8.0));
        let out_b = adapter_blue.render(&make_rect(8.0));

        let header_p = out_p
            .text_segments
            .iter()
            .find(|t| t.text == "DENIALS")
            .expect("phosphor: DENIALS header must render");
        let header_b = out_b
            .text_segments
            .iter()
            .find(|t| t.text == "DENIALS")
            .expect("blue: DENIALS header must render");

        // Phosphor: red channel > 0.8, blue < 0.5.
        assert!(
            header_p.color[0] > 0.8,
            "phosphor DENIALS should be red-dominant, got {:?}",
            header_p.color,
        );
        // Blue palette: red channel < 0.2, blue channel > 0.8.
        assert!(
            header_b.color[0] < 0.2,
            "blue DENIALS red channel should be near 0, got {:?}",
            header_b.color,
        );
        assert!(
            header_b.color[2] > 0.8,
            "blue DENIALS should be blue-dominant, got {:?}",
            header_b.color,
        );
        // The two headers must have different colors.
        assert_ne!(
            header_p.color, header_b.color,
            "DENIALS header color must change when Tokens changes",
        );
    }

    /// Mutating the shared `Arc<RwLock<Tokens>>` after construction must
    /// propagate to the next `render()` call without rebuilding the adapter.
    /// This is the core contract of the live-tokens feature.
    #[test]
    fn live_tokens_propagate_without_adapter_restart() {
        use phantom_ui::RenderCtx;
        use phantom_ui::tokens::{ColorRoles, Tokens};

        let phosphor_tokens = Tokens::phosphor(RenderCtx::fallback());
        let tokens_arc = Arc::new(RwLock::new(phosphor_tokens));
        let snapshot = Arc::new(RwLock::new(InspectorView::empty()));

        let adapter = InspectorAdapter::new(Arc::clone(&snapshot), Arc::clone(&tokens_arc));

        // First render: phosphor — DENIALS header is red-dominant.
        let out1 = adapter.render(&make_rect(8.0));
        let header1 = out1
            .text_segments
            .iter()
            .find(|t| t.text == "DENIALS")
            .expect("first render: DENIALS header");
        let color1 = header1.color;
        assert!(color1[0] > 0.8, "first render should be red: {color1:?}");

        // Mutate tokens in-place — simulates a theme switch.
        {
            let mut tok = tokens_arc.write().expect("tokens write lock");
            let mut blue_roles = ColorRoles::phosphor();
            blue_roles.status_danger = [0.0, 0.2, 1.0, 1.0];
            *tok = Tokens::new(blue_roles, RenderCtx::fallback());
        }

        // Second render with same adapter instance: must pick up the new color.
        let out2 = adapter.render(&make_rect(8.0));
        let header2 = out2
            .text_segments
            .iter()
            .find(|t| t.text == "DENIALS")
            .expect("second render: DENIALS header");
        let color2 = header2.color;

        assert!(
            color2[0] < 0.2,
            "second render should have near-zero red after theme switch: {color2:?}",
        );
        assert!(
            color2[2] > 0.8,
            "second render should be blue-dominant after theme switch: {color2:?}",
        );
        assert_ne!(
            color1, color2,
            "color must change after live token update (got same value: {color1:?})",
        );
    }

    /// Header text color is sourced from `tokens.colors.text_accent`. Switching
    /// to a custom palette with a clearly distinct accent must change the header
    /// text color emitted by `render()`.
    #[test]
    fn header_text_color_changes_with_tokens() {
        use phantom_ui::RenderCtx;
        use phantom_ui::tokens::{ColorRoles, Tokens};

        let phosphor_tokens = Tokens::phosphor(RenderCtx::fallback());

        let mut alt_roles = ColorRoles::phosphor();
        // Override text_accent to pure blue so it's clearly different from phosphor green.
        alt_roles.text_accent = [0.0, 0.0, 1.0, 1.0];
        let alt_tokens = Tokens::new(alt_roles, RenderCtx::fallback());

        let adapter_p =
            InspectorAdapter::with_view_and_tokens(InspectorView::empty(), phosphor_tokens);
        let adapter_a = InspectorAdapter::with_view_and_tokens(InspectorView::empty(), alt_tokens);

        let out_p = adapter_p.render(&make_rect(8.0));
        let out_a = adapter_a.render(&make_rect(8.0));

        let hdr_p = out_p
            .text_segments
            .iter()
            .find(|t| t.text.starts_with("INSPECTOR"))
            .expect("phosphor: INSPECTOR header");
        let hdr_a = out_a
            .text_segments
            .iter()
            .find(|t| t.text.starts_with("INSPECTOR"))
            .expect("alt: INSPECTOR header");

        assert_ne!(
            hdr_p.color, hdr_a.color,
            "INSPECTOR header color must change when text_accent changes",
        );
        // Alt palette has blue accent — blue channel must dominate.
        assert!(
            hdr_a.color[2] > 0.8,
            "alt adapter: INSPECTOR header should be blue-dominant, got {:?}",
            hdr_a.color,
        );
    }
}
