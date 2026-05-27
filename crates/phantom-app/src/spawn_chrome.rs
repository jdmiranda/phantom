//! `toggle_*_pane` helpers for chrome adapters reachable via keybinds.
//!
//! Each helper follows the same lifecycle:
//!
//! 1. Look for an existing instance of the chrome adapter (by `app_type`).
//!    If found, close it (toggle off) and return `true`.
//! 2. Otherwise, split the focused pane vertically (50/50) and register
//!    the new adapter in the new child pane. Focus the new pane and
//!    return `true`.
//! 3. Return `false` only when there is no focused pane to split from.
//!
//! This mirrors the inspector spawn flow in `inspector.rs` but factors
//! the per-adapter setup out so the App's `update` / input modules don't
//! grow unbounded.

use log::{info, warn};

use phantom_adapter::AppAdapter;
use phantom_scene::clock::Cadence;

use crate::adapters::{DagViewerAdapter, MonitorAdapter, VideoAdapter};
use crate::app::App;
use crate::sysmon::spawn_sysmon;

impl App {
    /// Find an existing chrome adapter by `app_type`. Returns the `AppId`
    /// of the first matching adapter, or `None` if no such adapter is
    /// registered. Used by all toggle helpers to flip an existing pane
    /// off instead of opening a duplicate.
    pub(crate) fn find_first_adapter_by_type(
        &self,
        target_type: &str,
    ) -> Option<phantom_adapter::AppId> {
        for id in self.coordinator.all_app_ids() {
            if let Some(adapter) = self.coordinator.registry().get_adapter(id)
                && adapter.app_type() == target_type
            {
                return Some(id);
            }
        }
        None
    }

    /// Close an existing chrome adapter (the standard "toggle off" path).
    /// Returns `true` on a clean close, `false` if removal failed. Resizes
    /// the layout afterwards so remaining panes reclaim the freed space.
    fn close_chrome_pane(&mut self, target_app_id: phantom_adapter::AppId) -> bool {
        self.coordinator
            .remove_adapter(target_app_id, &mut self.layout, &mut self.scene);
        let width = self.gpu.surface_config.width;
        let height = self.gpu.surface_config.height;
        let _ = self.layout.resize(width as f32, height as f32);
        // Resize remaining adapters to fit the reclaimed space.
        for app_id in self.coordinator.all_app_ids() {
            if let Some(pane_id) = self.coordinator.pane_id_for(app_id)
                && let Ok(rect) = self.layout.get_pane_rect(pane_id)
            {
                let (cols, rows) = crate::pane::pane_cols_rows(self.cell_size, rect);
                let _ = self.coordinator.send_command(
                    app_id,
                    "resize",
                    &serde_json::json!({"cols": cols, "rows": rows}),
                );
            }
        }
        true
    }

    /// Split the focused pane vertically and register `adapter` in the new
    /// child pane. Returns the new `AppId` on success.
    ///
    /// Shared by every `toggle_*_pane` helper.
    fn spawn_chrome_pane(
        &mut self,
        adapter: Box<dyn AppAdapter>,
        log_label: &str,
    ) -> Option<phantom_adapter::AppId> {
        let Some(focused_app_id) = self.coordinator.focused() else {
            warn!("{log_label}: no focused adapter to split from");
            return None;
        };
        let Some(current_pane_id) = self.coordinator.pane_id_for(focused_app_id) else {
            warn!("{log_label}: focused adapter has no layout pane");
            return None;
        };

        let split_result = self.layout.split_vertical(current_pane_id);
        let (existing_child, new_child) = match split_result {
            Ok(ids) => ids,
            Err(e) => {
                warn!("{log_label}: split failed: {e}");
                return None;
            }
        };
        let _ = self.layout.set_flex_grow(existing_child, 1.0);
        let _ = self.layout.set_flex_grow(new_child, 1.0);

        let width = self.gpu.surface_config.width;
        let height = self.gpu.surface_config.height;
        let _ = self.layout.resize(width as f32, height as f32);

        self.coordinator
            .remap_pane(focused_app_id, current_pane_id, existing_child);

        if let Ok(rect) = self.layout.get_pane_rect(existing_child) {
            let (cols, rows) = crate::pane::pane_cols_rows(self.cell_size, rect);
            let _ = self.coordinator.send_command(
                focused_app_id,
                "resize",
                &serde_json::json!({"cols": cols, "rows": rows}),
            );
        }

        let scene_node = self
            .scene
            .add_node(self.scene_content_node, phantom_scene::node::NodeKind::Pane);

        let app_id = self.coordinator.register_adapter_at_pane(
            adapter,
            new_child,
            scene_node,
            Cadence::unlimited(),
            &mut self.layout,
        );

        self.coordinator.set_focus(app_id);
        info!("{log_label}: registered AppId {app_id} in split pane");
        Some(app_id)
    }

    // -----------------------------------------------------------------------
    // Public toggle helpers
    // -----------------------------------------------------------------------

    /// Toggle the system-resource monitor pane (`Cmd+Shift+M`).
    ///
    /// Spawns a fresh `MonitorAdapter` backed by a dedicated `SysmonHandle`,
    /// activates the underlying sysmon thread so metrics start flowing, and
    /// focuses the new pane. If a monitor pane is already open, closes it.
    pub(crate) fn toggle_monitor_pane(&mut self) -> bool {
        if let Some(existing) = self.find_first_adapter_by_type("monitor") {
            info!("Cmd+Shift+M: closing existing monitor pane {existing}");
            return self.close_chrome_pane(existing);
        }
        let handle = spawn_sysmon();
        let mut adapter = MonitorAdapter::new(handle);
        // Eagerly activate so the background sysmon thread starts polling.
        let _ = adapter.accept_command_activate();
        self.spawn_chrome_pane(Box::new(adapter), "Cmd+Shift+M").is_some()
    }

    /// Toggle the video pane (`Cmd+Shift+W`, W=watch).
    ///
    /// Opens a placeholder `VideoAdapter`. Real playback is wired by
    /// downstream paths that call `App::start_video` with a file path.
    pub(crate) fn toggle_video_pane(&mut self) -> bool {
        if let Some(existing) = self.find_first_adapter_by_type("video") {
            info!("Cmd+Shift+W: closing existing video pane {existing}");
            return self.close_chrome_pane(existing);
        }
        // Build a placeholder playback. The dimensions are nominal; real
        // playback overwrites them when a file is loaded.
        let playback = crate::video::VideoPlayback::placeholder();
        let adapter = VideoAdapter::new(playback);
        self.spawn_chrome_pane(Box::new(adapter), "Cmd+Shift+W").is_some()
    }

    /// Toggle the DAG viewer pane (`Cmd+Shift+A`, A=architecture).
    ///
    /// Loads `.planning/dag.json` from the current working directory when
    /// present; otherwise opens an empty pane with a hint.
    pub(crate) fn toggle_dag_viewer_pane(&mut self) -> bool {
        if let Some(existing) = self.find_first_adapter_by_type("dag_viewer") {
            info!("Cmd+Shift+A: closing existing DAG viewer pane {existing}");
            return self.close_chrome_pane(existing);
        }
        let planning_dir = std::env::current_dir()
            .unwrap_or_else(|_| std::path::PathBuf::from("."))
            .join(".planning");
        let adapter = DagViewerAdapter::from_planning_dir(&planning_dir);
        self.spawn_chrome_pane(Box::new(adapter), "Cmd+Shift+A").is_some()
    }

    /// Toggle the BootAdapter splash pane.
    ///
    /// Returns `true` if a pane was opened or closed. The App auto-spawns
    /// this once at startup when `--boot` was passed (see
    /// [`App::auto_open_boot_pane`]); the toggle helper is also reachable
    /// from tests and the console `boot` command.
    #[allow(dead_code)]
    pub(crate) fn toggle_boot_pane(&mut self) -> bool {
        if let Some(existing) = self.find_first_adapter_by_type("boot") {
            info!("toggle_boot: closing existing boot pane {existing}");
            return self.close_chrome_pane(existing);
        }
        let adapter = crate::adapters::BootAdapter::new();
        self.spawn_chrome_pane(Box::new(adapter), "toggle_boot").is_some()
    }

    /// Auto-open a one-shot BootAdapter pane during startup, then close it
    /// when `AppState::Terminal` is reached. Called from the App constructor
    /// only when `config.skip_boot == false` (i.e. the user opted in via
    /// `--boot`).
    ///
    /// Replaces the focused pane in-place (typically the SetupAdapter at
    /// cold-launch) so the boot screen owns the full window. The despawn
    /// path (`despawn_boot_pane`) restores a fresh SetupAdapter at the
    /// same pane slot.
    pub(crate) fn auto_open_boot_pane(&mut self) -> bool {
        // If a boot pane already exists, leave it alone.
        if self.find_first_adapter_by_type("boot").is_some() {
            return false;
        }
        let Some(focused_app_id) = self.coordinator.focused() else {
            warn!("auto_open_boot_pane: no focused adapter to replace");
            return false;
        };
        // Replace the focused pane (typically the SetupAdapter) with the
        // BootAdapter so the splash owns the full window during boot.
        let Some((target_pane_id, target_scene_node)) =
            self.coordinator.kill_keeping_pane(focused_app_id)
        else {
            warn!("auto_open_boot_pane: kill_keeping_pane returned None");
            return false;
        };
        let adapter = crate::adapters::BootAdapter::new();
        let app_id = self.coordinator.register_adapter_at_pane(
            Box::new(adapter),
            target_pane_id,
            target_scene_node,
            Cadence::unlimited(),
            &mut self.layout,
        );
        self.coordinator.set_focus(app_id);
        info!("auto_open_boot_pane: BootAdapter {app_id} owns full window during boot");
        true
    }

    /// Despawn the BootAdapter (if any) on the first `AppState::Terminal`
    /// transition. Called from `update.rs` once per frame; cheap when no
    /// boot pane is open.
    ///
    /// Replaces the BootAdapter in-place with a fresh SetupAdapter so the
    /// cold-launch agent-king flow resumes. The post-boot agent spawn in
    /// `update.rs` will then upgrade SetupAdapter → Agent when an API key
    /// is available.
    pub(crate) fn despawn_boot_pane(&mut self) -> bool {
        let Some(boot_app_id) = self.find_first_adapter_by_type("boot") else {
            return false;
        };
        info!("despawn_boot: replacing boot pane {boot_app_id} with SetupAdapter on terminal entry");
        let Some((target_pane_id, target_scene_node)) =
            self.coordinator.kill_keeping_pane(boot_app_id)
        else {
            warn!("despawn_boot: kill_keeping_pane returned None");
            return false;
        };
        // Re-register a fresh SetupAdapter sharing the App's existing
        // upgrade flag so the App's update loop swaps it for a real agent
        // when an API key becomes available.
        let adapter = crate::adapters::SetupAdapter::new(
            std::sync::Arc::clone(&self.post_setup_upgrade),
        );
        let app_id = self.coordinator.register_adapter_at_pane(
            Box::new(adapter),
            target_pane_id,
            target_scene_node,
            Cadence::unlimited(),
            &mut self.layout,
        );
        self.coordinator.set_focus(app_id);
        info!("despawn_boot: SetupAdapter {app_id} took over the full window");
        true
    }

    /// Ensure the default mockup-row-1 two-up layout (Agent + Terminal).
    ///
    /// Called once per process lifetime by `update.rs` on the first
    /// `AppState::Terminal` transition AFTER the post-boot agent has been
    /// spawned. Splits the focused pane (the agent) horizontally and
    /// places a fresh `TerminalAdapter` on the right.
    ///
    /// No-op when:
    /// - `self.first_layout_done` is already `true` (called twice).
    /// - The focused adapter is not an agent (e.g. the API-key SetupAdapter
    ///   is still on screen; the user has not provisioned a key yet).
    /// - `coordinator.adapter_count()` is already > 1 (the user / Composer
    ///   has already opened additional panes).
    pub(crate) fn ensure_first_layout(&mut self) -> bool {
        if self.first_layout_done {
            return false;
        }
        // Skip when the user has already created additional panes since
        // boot; this is a default, not a forced override.
        if self.coordinator.adapter_count() != 1 {
            self.first_layout_done = true;
            return false;
        }
        // Only fire when the sole pane is an agent. Setup/boot/etc. should
        // wait until the user upgrades to an agent before we split.
        let Some(focused_app_id) = self.coordinator.focused() else {
            return false;
        };
        let focused_is_agent = self
            .coordinator
            .registry()
            .get_adapter(focused_app_id)
            .map(|a| a.app_type() == "agent")
            .unwrap_or(false);
        if !focused_is_agent {
            return false;
        }
        let Some(current_pane_id) = self.coordinator.pane_id_for(focused_app_id) else {
            return false;
        };

        // Split horizontally: agent stays on the left, new terminal goes
        // to the right. Matches mockups/apps.html row 1.
        let split_result = self.layout.split_horizontal(current_pane_id);
        let (existing_child, new_child) = match split_result {
            Ok(ids) => ids,
            Err(e) => {
                warn!("ensure_first_layout: horizontal split failed: {e}");
                return false;
            }
        };
        let _ = self.layout.set_flex_grow(existing_child, 1.0);
        let _ = self.layout.set_flex_grow(new_child, 1.0);

        let width = self.gpu.surface_config.width;
        let height = self.gpu.surface_config.height;
        let _ = self.layout.resize(width as f32, height as f32);

        // Remap the existing agent's PaneId onto the existing child pane.
        self.coordinator
            .remap_pane(focused_app_id, current_pane_id, existing_child);

        // Resize the existing agent to its new (smaller) pane.
        if let Ok(rect) = self.layout.get_pane_rect(existing_child) {
            let (cols, rows) = crate::pane::pane_cols_rows(self.cell_size, rect);
            let _ = self.coordinator.send_command(
                focused_app_id,
                "resize",
                &serde_json::json!({"cols": cols, "rows": rows}),
            );
        }

        // Spawn a fresh terminal in the new pane.
        let new_rect = self.layout.get_pane_rect(new_child).ok();
        let (cols, rows) = new_rect
            .map(|r| crate::pane::pane_cols_rows(self.cell_size, r))
            .unwrap_or((80, 24));

        let terminal = match phantom_terminal::terminal::PhantomTerminal::new(cols, rows) {
            Ok(t) => t,
            Err(e) => {
                warn!("ensure_first_layout: failed to spawn terminal: {e}");
                // Roll back the split: drop the new pane.
                let _ = self.layout.remove_pane(new_child);
                return false;
            }
        };

        use crate::adapters::terminal::TerminalAdapter;
        use phantom_terminal::output::TerminalThemeColors;
        let theme_colors = TerminalThemeColors {
            foreground: self.theme.colors.foreground,
            background: self.theme.colors.background,
            cursor: self.theme.colors.cursor,
            ansi: Some(self.theme.colors.ansi),
        };
        let term_adapter = TerminalAdapter::with_theme(terminal, theme_colors);

        let scene_node = self
            .scene
            .add_node(self.scene_content_node, phantom_scene::node::NodeKind::Pane);

        let new_app_id = self.coordinator.register_adapter_at_pane(
            Box::new(term_adapter),
            new_child,
            scene_node,
            Cadence::unlimited(),
            &mut self.layout,
        );

        // Keep focus on the agent — the agent is primary, terminal is
        // secondary chrome (see `feedback_agent_is_primary`). Only the
        // pane registration changes focus; restore it here.
        self.coordinator.set_focus(focused_app_id);

        info!(
            "ensure_first_layout: agent {focused_app_id} on left, terminal {new_app_id} on right ({cols}x{rows})"
        );
        self.first_layout_done = true;
        true
    }
}

// ---------------------------------------------------------------------------
// Helper trait impls
// ---------------------------------------------------------------------------

impl MonitorAdapter {
    /// Convenience: activate without going through the trait method (used
    /// by `toggle_monitor_pane` so we don't have to import the trait there).
    pub(crate) fn accept_command_activate(&mut self) -> anyhow::Result<String> {
        use phantom_adapter::Commandable;
        self.accept_command("activate", &serde_json::json!({}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sysmon::SysmonHandle;
    use std::sync::mpsc;

    fn fake_handle() -> SysmonHandle {
        let (_tx, rx) = mpsc::channel();
        SysmonHandle::for_test(rx)
    }

    /// Smoke test: the convenience activate path is reachable and returns
    /// the expected literal.
    #[test]
    fn monitor_adapter_accept_command_activate_returns_activated() {
        let mut adapter = MonitorAdapter::new(fake_handle());
        let res = adapter.accept_command_activate().unwrap();
        assert_eq!(res, "activated");
    }
}
