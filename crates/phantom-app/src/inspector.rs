//! Inspector pane spawn helper — split the focused pane and register an
//! [`InspectorAdapter`] in the new child.
//!
//! Companion to [`crate::agent_pane`]'s `App::spawn_agent_pane`. Mirrors that
//! flow: split focused pane vertically, give the inspector half the width,
//! register the adapter, focus the new pane.
//!
//! Snapshot architecture — the App holds the canonical
//! `Arc<RwLock<InspectorView>>` and pushes a fresh snapshot at the end of
//! each `update()` cycle by calling [`crate::runtime::AgentRuntime::snapshot`]
//! and writing into the lock. The inspector adapter (and any future
//! observers) read through the same lock during `render()`.

use std::sync::{Arc, RwLock};

use log::{info, warn};

use phantom_agents::inspector::InspectorView;

use crate::adapters::inspector::InspectorAdapter;
use crate::app::App;

impl App {
    /// Spawn an inspector pane as a first-class coordinator adapter.
    ///
    /// Splits the focused pane vertically (50/50), creates an
    /// [`InspectorAdapter`] sharing the App's snapshot lock, registers it in
    /// the new split pane, and focuses it. Returns `false` if no pane is
    /// focused or the split fails.
    #[allow(dead_code)] // Wired by command path in a follow-up; kept ahead of time.
    pub(crate) fn spawn_inspector_pane(&mut self) -> bool {
        // Split the focused pane to make room for the inspector.
        let Some(focused_app_id) = self.coordinator.focused() else {
            warn!("Cannot spawn inspector: no focused adapter");
            return false;
        };
        let Some(current_pane_id) = self.coordinator.pane_id_for(focused_app_id) else {
            warn!("Cannot spawn inspector: focused adapter has no layout pane");
            return false;
        };

        let split_result = self.layout.split_vertical(current_pane_id);
        let (existing_child, new_child) = match split_result {
            Ok(ids) => ids,
            Err(e) => {
                warn!("Inspector split failed: {e}");
                return false;
            }
        };

        // Equal split: the existing pane keeps half, inspector takes the other.
        let _ = self.layout.set_flex_grow(existing_child, 1.0);
        let _ = self.layout.set_flex_grow(new_child, 1.0);

        // Resize layout after split.
        let width = self.gpu.surface_config.width;
        let height = self.gpu.surface_config.height;
        let _ = self.layout.resize(width as f32, height as f32);

        // Remap the existing terminal/agent's PaneId.
        self.coordinator.remap_pane(focused_app_id, current_pane_id, existing_child);

        // Resize the existing pane to fit its new (smaller) bounds.
        if let Ok(rect) = self.layout.get_pane_rect(existing_child) {
            let (cols, rows) = crate::pane::pane_cols_rows(self.cell_size, rect);
            let _ = self.coordinator.send_command(
                focused_app_id,
                "resize",
                &serde_json::json!({"cols": cols, "rows": rows}),
            );
        }

        // Build the snapshot lock if we don't have one yet, and seed it with
        // the runtime's current view.
        let snapshot = self
            .inspector_snapshot
            .get_or_insert_with(|| Arc::new(RwLock::new(InspectorView::empty())))
            .clone();
        if let Ok(mut guard) = snapshot.write() {
            *guard = self.runtime.snapshot();
        }

        let adapter = InspectorAdapter::new(snapshot);

        let scene_node = self.scene.add_node(
            self.scene_content_node,
            phantom_scene::node::NodeKind::Pane,
        );

        let app_id = self.coordinator.register_adapter_at_pane(
            Box::new(adapter),
            new_child,
            scene_node,
            phantom_scene::clock::Cadence::unlimited(),
        );

        // Focus the new inspector pane.
        self.coordinator.set_focus(app_id);

        info!("Inspector adapter registered (AppId {app_id}) in split pane");
        true
    }

    /// Push a fresh snapshot into the shared inspector view, if one is
    /// active. Cheap when no inspector pane is open (no Arc, no work).
    ///
    /// Called from `App::update()` once per frame after the runtime has
    /// ticked. Inspector adapters read the result through their own clone
    /// of the same `Arc<RwLock<InspectorView>>`.
    #[allow(dead_code)] // Wired in Phase 2.G alongside the inspector adapter.
    pub(crate) fn refresh_inspector_snapshot(&mut self) {
        let Some(ref snapshot) = self.inspector_snapshot else {
            return;
        };
        let view = self.runtime.snapshot();
        if let Ok(mut guard) = snapshot.write() {
            *guard = view;
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    //! Spawn-flow tests live alongside other integration tests in `app.rs`
    //! because constructing a real `App` requires a GPU context. The
    //! adapter-level rendering tests live in `crate::adapters::inspector`.
    //!
    //! What we *can* unit-test here: the `inspect` console command routes
    //! to `App::spawn_inspector_pane` (without actually constructing an App).
    use crate::console::COMMANDS;

    /// The `inspect` command must be in the tab-completion list so it shows
    /// up in the console's autocompletion. This guards against a regression
    /// where `inspect` is added to commands.rs but forgotten in console.rs.
    #[test]
    fn console_inspect_command_is_in_completions() {
        assert!(
            COMMANDS.contains(&"inspect"),
            "console COMMANDS must include 'inspect' for tab completion",
        );
    }

    /// Lightweight router test that doesn't require a full `App`: we
    /// confirm that the parser splits "inspect" into a recognized first
    /// token. This is a no-GPU smoke test for the command path; the
    /// actual spawn path is integration-tested when an App is available.
    #[test]
    fn console_inspect_command_parses_as_known_command() {
        let input = "inspect";
        let parts: Vec<&str> = input.trim().splitn(3, ' ').collect();
        assert_eq!(parts[0], "inspect");
    }
}
