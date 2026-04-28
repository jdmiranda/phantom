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

// ---------------------------------------------------------------------------
// Visual constants
// ---------------------------------------------------------------------------

/// Header bar background — a thin dark strip.
const HEADER_BG: [f32; 4] = [0.05, 0.07, 0.08, 1.0];
/// Header text color — bright phosphor green.
const HEADER_COLOR: [f32; 4] = [0.3, 1.0, 0.5, 1.0];
/// Section title color — slightly dimmer.
const SECTION_COLOR: [f32; 4] = [0.5, 0.9, 0.6, 0.95];
/// Agent row text color — neutral phosphor.
const AGENT_COLOR: [f32; 4] = [0.7, 0.95, 0.75, 0.95];
/// Event row text color — dimmer than agents to push focus to the live agents.
const EVENT_COLOR: [f32; 4] = [0.5, 0.75, 0.55, 0.85];
/// Refresh-time stamp color — dim grey-green.
const STAMP_COLOR: [f32; 4] = [0.35, 0.55, 0.4, 0.6];
/// Inspector pane background — near-transparent.
const PANE_BG: [f32; 4] = [0.0, 0.0, 0.0, 0.0];

/// Number of events shown at the bottom of the pane (fixed cap, separate from
/// the snapshot's `recent_events` cap).
const VISIBLE_EVENT_ROWS: usize = 20;

// ---------------------------------------------------------------------------
// Adapter
// ---------------------------------------------------------------------------

/// Live inspector pane for the substrate runtime.
///
/// Holds an `Arc<RwLock<InspectorView>>` whose contents are pushed by the
/// App at the end of each `update()` cycle. `render()` reads the snapshot
/// without copying.
pub struct InspectorAdapter {
    snapshot: Arc<RwLock<InspectorView>>,
    app_id: u32,
}

impl InspectorAdapter {
    /// Build an adapter sharing `snapshot` with the producer (the App).
    ///
    /// The App should hold its own `Arc` to the same `RwLock` so it can
    /// write fresh snapshots each frame.
    #[allow(dead_code)] // Phase 2.G+: spawn-inspector-pane wiring is staged.
    pub(crate) fn new(snapshot: Arc<RwLock<InspectorView>>) -> Self {
        Self {
            snapshot,
            app_id: 0,
        }
    }

    /// Test-only constructor that wraps an existing view.
    #[cfg(test)]
    pub(crate) fn with_view(view: InspectorView) -> Self {
        Self::new(Arc::new(RwLock::new(view)))
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
        })
    }
}

impl Renderable for InspectorAdapter {
    fn render(&self, rect: &Rect) -> RenderOutput {
        let mut quads = Vec::new();
        let mut text_segments = Vec::new();

        // Pull live cell metrics from the rect rather than baking constants;
        // doubling the font size doubles every spacing/positioning value.
        // `Rect::default()` carries `(0.0, 0.0)` as a "not provided" sentinel
        // so we fall back to legacy 8.0 / 16.0 for callers that don't pass
        // cell metrics through.
        let cell_w = if rect.cell_size.0 > 0.0 { rect.cell_size.0 } else { 8.0 };
        let cell_h = if rect.cell_size.1 > 0.0 { rect.cell_size.1 } else { 16.0 };
        let pad_x = cell_w; // 1 cell of left padding.
        let pad_y = cell_h * 0.4; // ~half a line of top padding.

        // ── Pane background ────────────────────────────────────────────
        quads.push(QuadData {
            x: rect.x,
            y: rect.y,
            w: rect.width,
            h: rect.height,
            color: PANE_BG,
        });

        let view = self.snapshot.read().expect("inspector snapshot lock");

        // ── Header bar ─────────────────────────────────────────────────
        let header_h = cell_h * 1.6;
        quads.push(QuadData {
            x: rect.x,
            y: rect.y,
            w: rect.width,
            h: header_h,
            color: HEADER_BG,
        });

        let header_text = format!(
            "INSPECTOR — {} agents running ({} total)",
            view.running_count, view.spawned_total,
        );
        text_segments.push(TextData {
            text: header_text,
            x: rect.x + pad_x,
            y: rect.y + pad_y,
            color: HEADER_COLOR,
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
            color: STAMP_COLOR,
        });

        // Cursor advances down the pane as we lay out sections.
        let mut cursor_y = rect.y + header_h + pad_y;

        // ── Agents section title ───────────────────────────────────────
        text_segments.push(TextData {
            text: "AGENTS".to_string(),
            x: rect.x + pad_x,
            y: cursor_y,
            color: SECTION_COLOR,
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
                color: AGENT_COLOR,
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
                color: EVENT_COLOR,
            });
            cursor_y += cell_h;
        }

        // ── Recent events section ──────────────────────────────────────
        cursor_y += cell_h * 0.5;
        text_segments.push(TextData {
            text: "RECENT EVENTS".to_string(),
            x: rect.x + pad_x,
            y: cursor_y,
            color: SECTION_COLOR,
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
                let cut: String = ev.summary.chars().take(max_chars.saturating_sub(1)).collect();
                format!("{cut}…")
            } else {
                ev.summary.clone()
            };
            text_segments.push(TextData {
                text: summary,
                x: rect.x + pad_x * 2.0,
                y: cursor_y,
                color: EVENT_COLOR,
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
                color: EVENT_COLOR,
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
    fn handle_input(&mut self, _key: &str) -> bool {
        // Read-only pane. A future phase can add scroll or "kill agent"
        // actions; for now no key is consumed.
        false
    }

    fn accepts_input(&self) -> bool {
        false
    }
}

impl Commandable for InspectorAdapter {
    fn accept_command(
        &mut self,
        cmd: &str,
        _args: &serde_json::Value,
    ) -> anyhow::Result<String> {
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
            let found = output
                .text_segments
                .iter()
                .any(|t| t.text.contains(&label));
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
        assert!(output.text_segments.iter().any(|t| t.text.contains("no agents")));
        assert!(output
            .text_segments
            .iter()
            .any(|t| t.text.contains("no recent events")));
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
        let found = output.text_segments.iter().any(|t| t.text.contains("Spawned Watcher"));
        assert!(found, "expected event summary text in rendered output");
    }

    #[test]
    fn inspector_adapter_app_type_is_inspector() {
        let adapter = InspectorAdapter::with_view(InspectorView::empty());
        assert_eq!(adapter.app_type(), "inspector");
    }

    #[test]
    fn inspector_adapter_does_not_accept_input() {
        let mut adapter = InspectorAdapter::with_view(InspectorView::empty());
        assert!(!adapter.accepts_input());
        assert!(!adapter.handle_input("q"));
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
}
