//! Alt-screen view adapter — read-only mirror of a primary terminal's
//! alt-screen render output.
//!
//! Used by issue #323: when a terminal enters alt-screen mode (vim, htop,
//! less), the primary pane renders the normal-screen history while a sibling
//! `AltScreenViewAdapter` renders the alt-screen program. Both share the same
//! `Arc<Mutex<Option<RenderOutput>>>` snapshot that the primary writes each
//! frame via its `update()` override.
//!
//! The view adapter is intentionally minimal: no PTY, no input, no commands.
//! It exists only to give the coordinator a second pane slot so the layout
//! engine creates a visual split.

use std::sync::{Arc, Mutex};

use phantom_adapter::adapter::{Rect, RenderOutput};
use phantom_adapter::spatial::{InternalLayout, SpatialPreference};
use phantom_adapter::{
    AppCore, BusParticipant, Commandable, InputHandler, Lifecycled, Permissioned, Renderable,
};
use serde_json::json;

/// A shared render-snapshot handle passed between the primary terminal adapter
/// and its alt-screen view sibling.
///
/// The primary calls `write()` each frame; the view adapter calls `read()`
/// in its `render()` implementation.
pub type AltScreenSnapshot = Arc<Mutex<Option<RenderOutput>>>;

/// Create a new empty snapshot handle.
pub fn new_snapshot() -> AltScreenSnapshot {
    Arc::new(Mutex::new(None))
}

/// Read-only adapter that mirrors an alt-screen program's grid into a sibling pane.
pub struct AltScreenViewAdapter {
    snapshot: AltScreenSnapshot,
    app_id: u32,
    /// Human-readable label of the foreground program (e.g. "vim", "htop").
    label: String,
}

impl AltScreenViewAdapter {
    /// Create a new view adapter backed by `snapshot`.
    pub fn new(snapshot: AltScreenSnapshot, label: String) -> Self {
        Self {
            snapshot,
            app_id: 0,
            label,
        }
    }

    /// The shared snapshot handle (hand this to the primary adapter).
    pub fn snapshot_arc(&self) -> AltScreenSnapshot {
        Arc::clone(&self.snapshot)
    }
}

// ---------------------------------------------------------------------------
// Sub-trait implementations
// ---------------------------------------------------------------------------

impl AppCore for AltScreenViewAdapter {
    fn app_type(&self) -> &str {
        "alt_screen_view"
    }

    fn is_alive(&self) -> bool {
        true
    }

    fn update(&mut self, _dt: f32) {}

    fn get_state(&self) -> serde_json::Value {
        json!({
            "type": "alt_screen_view",
            "label": self.label,
        })
    }

    fn title(&self) -> &str {
        &self.label
    }
}

impl Renderable for AltScreenViewAdapter {
    fn render(&self, rect: &Rect) -> RenderOutput {
        // Attempt to read the latest snapshot pushed by the primary terminal.
        if let Ok(guard) = self.snapshot.lock() {
            if let Some(mut output) = guard.clone() {
                // Re-anchor the grid origin to this adapter's rect, since the
                // primary may have been positioned differently.
                if let Some(ref mut grid) = output.grid {
                    grid.origin = (rect.x, rect.y);
                }
                return output;
            }
        }
        // No snapshot yet — return empty output.
        RenderOutput::default()
    }

    fn is_visual(&self) -> bool {
        true
    }

    fn spatial_preference(&self) -> Option<SpatialPreference> {
        Some(SpatialPreference {
            min_size: (40, 10),
            preferred_size: (120, 40),
            max_size: None,
            aspect_ratio: None,
            internal_panes: 1,
            internal_layout: InternalLayout::Single,
            priority: 10.0,
        })
    }
}

impl InputHandler for AltScreenViewAdapter {
    fn handle_input(&mut self, _key: &str) -> bool {
        // View-only — never consumes input.
        false
    }

    fn accepts_input(&self) -> bool {
        false
    }
}

impl Commandable for AltScreenViewAdapter {
    fn accept_command(&mut self, cmd: &str, _args: &serde_json::Value) -> anyhow::Result<String> {
        Err(anyhow::anyhow!(
            "alt_screen_view adapter does not accept commands: {cmd}"
        ))
    }

    fn accepts_commands(&self) -> bool {
        false
    }
}

impl BusParticipant for AltScreenViewAdapter {}

impl Lifecycled for AltScreenViewAdapter {
    fn set_app_id(&mut self, id: u32) {
        self.app_id = id;
    }
}

impl Permissioned for AltScreenViewAdapter {
    fn permissions(&self) -> Vec<String> {
        vec![]
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alt_screen_view_adapter_type_is_alt_screen_view() {
        let snap = new_snapshot();
        let adapter = AltScreenViewAdapter::new(snap, "vim".into());
        assert_eq!(adapter.app_type(), "alt_screen_view");
    }

    #[test]
    fn alt_screen_view_adapter_is_always_alive() {
        let snap = new_snapshot();
        let adapter = AltScreenViewAdapter::new(snap, "htop".into());
        assert!(adapter.is_alive());
    }

    #[test]
    fn alt_screen_view_adapter_does_not_accept_input() {
        let snap = new_snapshot();
        let mut adapter = AltScreenViewAdapter::new(snap, "less".into());
        assert!(!adapter.accepts_input());
        assert!(!adapter.handle_input("q"));
    }

    #[test]
    fn alt_screen_view_adapter_does_not_accept_commands() {
        let snap = new_snapshot();
        let mut adapter = AltScreenViewAdapter::new(snap, "vim".into());
        assert!(!adapter.accepts_commands());
        let result = adapter.accept_command("resize", &serde_json::json!({}));
        assert!(result.is_err());
    }

    #[test]
    fn render_returns_empty_when_no_snapshot() {
        let snap = new_snapshot();
        let adapter = AltScreenViewAdapter::new(snap, "vim".into());
        let rect = Rect {
            x: 0.0,
            y: 0.0,
            width: 800.0,
            height: 600.0,
            ..Default::default()
        };
        let output = adapter.render(&rect);
        assert!(output.grid.is_none());
        assert!(output.quads.is_empty());
    }

    #[test]
    fn render_uses_snapshot_and_reanchors_grid_origin() {
        use phantom_adapter::adapter::{GridData, TerminalCell};

        let snap = new_snapshot();
        let adapter = AltScreenViewAdapter::new(Arc::clone(&snap), "vim".into());

        // Push a grid snapshot with an arbitrary origin.
        let cells = vec![TerminalCell {
            ch: 'A',
            fg: [1.0; 4],
            bg: [0.0; 4],
        }];
        let grid = GridData {
            cells,
            cols: 1,
            rows: 1,
            origin: (0.0, 0.0),
            cursor: None,
        };
        {
            let mut guard = snap.lock().unwrap();
            *guard = Some(RenderOutput {
                grid: Some(grid),
                ..Default::default()
            });
        }

        let rect = Rect {
            x: 400.0,
            y: 100.0,
            width: 400.0,
            height: 300.0,
            ..Default::default()
        };
        let output = adapter.render(&rect);
        let grid_out = output.grid.unwrap();
        // Origin should be re-anchored to the adapter's rect.
        assert!((grid_out.origin.0 - 400.0).abs() < 0.01);
        assert!((grid_out.origin.1 - 100.0).abs() < 0.01);
    }

    #[test]
    fn snapshot_arc_returns_same_arc() {
        let snap = new_snapshot();
        let adapter = AltScreenViewAdapter::new(Arc::clone(&snap), "vim".into());
        let arc = adapter.snapshot_arc();
        // Both arcs point to the same allocation.
        assert!(Arc::ptr_eq(&snap, &arc));
    }
}
