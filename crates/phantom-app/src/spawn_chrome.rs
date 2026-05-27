//! Shared spawn helpers for chrome adapters reachable via keybinds.
//!
//! Two flavours live in this module:
//!
//! * **Free `toggle_*_pane(app: &mut App)` functions** for the mockup chrome
//!   row (Settings / Notifications / Console / KeybindsHelp / Logs /
//!   FilesWatch / Diff / Memory / Fleet / Plugins / Database / VoiceStt).
//!   Each tracks its open pane via `App::<adapter>_pane_id`. Mirrors the
//!   `spawn_inspector_pane` pattern: split the focused pane vertically,
//!   register the new adapter in the new child, resize, focus.
//! * **Methods on `impl App`** for system / capture panes
//!   (`toggle_monitor_pane`, `toggle_video_pane`, `toggle_dag_viewer_pane`,
//!   `auto_open_boot_pane`, `despawn_boot_pane`, `ensure_first_layout`).
//!   These look up an existing instance by `app_type` rather than a
//!   dedicated `Option<AppId>` field on `App`.
//!
//! In both shapes the lifecycle is identical:
//!
//! 1. Look for an existing instance. If found, close it (toggle off).
//! 2. Otherwise, split the focused pane vertically (50/50) and register
//!    the new adapter in the new child pane. Focus the new pane.
//! 3. Return `false` only when there is no focused pane to split from.

use log::{info, warn};

use phantom_adapter::AppAdapter;
use phantom_ui::tokens::Tokens;
use phantom_ui::RenderCtx;

use phantom_scene::clock::Cadence;

use crate::adapters::{
    BootAdapter, ConsoleAdapter, DagViewerAdapter, DatabaseAdapter, DiffAdapter, FilesWatchAdapter,
    FleetAdapter, KeybindsHelpAdapter, LogsAdapter, MemoryInspectorAdapter, MonitorAdapter,
    NotificationsAdapter, PluginsAdapter, SettingsAdapter, VideoAdapter, VoiceSttAdapter,
};
use crate::adapters::settings::{Slider01, SettingsView};
use crate::app::App;
use crate::sysmon::spawn_sysmon;

/// Build a `Tokens` snapshot from the App's currently active theme.
///
/// For now every theme uses the phosphor ColorRoles palette; the per-theme
/// `ColorRoles` table lands as part of the theme-cycle broadcast pass.
fn current_tokens(app: &App) -> Tokens {
    let ctx = RenderCtx::new(app.cell_size, 1.0);
    Tokens::phosphor(ctx)
}

/// Split the focused pane vertically (50/50) and register `adapter` in the
/// new child. Returns the new `AppId` on success, `None` if no pane is
/// focused or the split fails.
fn spawn_chrome_pane(
    app: &mut App,
    adapter: Box<dyn AppAdapter>,
    adapter_label: &str,
) -> Option<u32> {
    let focused_app_id = match app.coordinator.focused() {
        Some(id) => id,
        None => {
            warn!("Cannot spawn {adapter_label}: no focused adapter");
            return None;
        }
    };
    let current_pane_id = match app.coordinator.pane_id_for(focused_app_id) {
        Some(id) => id,
        None => {
            warn!("Cannot spawn {adapter_label}: focused adapter has no layout pane");
            return None;
        }
    };

    let (existing_child, new_child) = match app.layout.split_vertical(current_pane_id) {
        Ok(ids) => ids,
        Err(e) => {
            warn!("Spawn {adapter_label} split failed: {e}");
            return None;
        }
    };

    let _ = app.layout.set_flex_grow(existing_child, 1.0);
    let _ = app.layout.set_flex_grow(new_child, 1.0);

    let width = app.gpu.surface_config.width;
    let height = app.gpu.surface_config.height;
    let _ = app.layout.resize(width as f32, height as f32);

    app.coordinator
        .remap_pane(focused_app_id, current_pane_id, existing_child);

    if let Ok(rect) = app.layout.get_pane_rect(existing_child) {
        let (cols, rows) = crate::pane::pane_cols_rows(app.cell_size, rect);
        let _ = app.coordinator.send_command(
            focused_app_id,
            "resize",
            &serde_json::json!({ "cols": cols, "rows": rows }),
        );
    }

    let scene_node = app
        .scene
        .add_node(app.scene_content_node, phantom_scene::node::NodeKind::Pane);

    let app_id = app.coordinator.register_adapter_at_pane(
        adapter,
        new_child,
        scene_node,
        phantom_scene::clock::Cadence::unlimited(),
        &mut app.layout,
    );

    app.coordinator.set_focus(app_id);

    info!("{adapter_label} adapter registered (AppId {app_id}) in split pane");
    Some(app_id)
}

/// Despawn a previously-spawned chrome pane by its AppId.
fn despawn_chrome_pane(app: &mut App, app_id: u32, label: &str) {
    app.coordinator
        .remove_adapter(app_id, &mut app.layout, &mut app.scene);
    info!("{label} adapter despawned (AppId {app_id})");
}

// ---------------------------------------------------------------------------
// Toggle helpers — one per chrome adapter
// ---------------------------------------------------------------------------

pub(crate) fn toggle_settings_pane(app: &mut App) {
    if let Some(app_id) = app.settings_pane_id.take() {
        despawn_chrome_pane(app, app_id, "Settings");
        return;
    }
    let tokens = current_tokens(app);
    let view = SettingsView {
        theme: app.theme.name.to_lowercase(),
        scanlines: Slider01::new(app.theme.shader_params.scanline_intensity),
        bloom: Slider01::new(app.theme.shader_params.bloom_intensity),
        curvature: Slider01::new(app.theme.shader_params.curvature),
        ..SettingsView::default()
    };
    let mut adapter = SettingsAdapter::with_view(view);
    adapter.set_tokens(tokens);
    if let Some(new_id) = spawn_chrome_pane(app, Box::new(adapter), "Settings") {
        app.settings_pane_id = Some(new_id);
    }
}

pub(crate) fn toggle_notifications_pane(app: &mut App) {
    if let Some(app_id) = app.notifications_pane_id.take() {
        despawn_chrome_pane(app, app_id, "Notifications");
        return;
    }
    let tokens = current_tokens(app);
    let mut adapter = NotificationsAdapter::new();
    adapter.set_tokens(tokens);
    if let Some(new_id) = spawn_chrome_pane(app, Box::new(adapter), "Notifications") {
        app.notifications_pane_id = Some(new_id);
    }
}

pub(crate) fn toggle_console_pane(app: &mut App) {
    if let Some(app_id) = app.console_pane_id.take() {
        despawn_chrome_pane(app, app_id, "Console");
        return;
    }
    let tokens = current_tokens(app);
    let mut adapter = ConsoleAdapter::new();
    adapter.set_tokens(tokens);
    if let Some(new_id) = spawn_chrome_pane(app, Box::new(adapter), "Console") {
        app.console_pane_id = Some(new_id);
    }
}

pub(crate) fn toggle_keybinds_help_pane(app: &mut App) {
    if let Some(app_id) = app.keybinds_help_pane_id.take() {
        despawn_chrome_pane(app, app_id, "KeybindsHelp");
        return;
    }
    let tokens = current_tokens(app);
    let mut adapter = KeybindsHelpAdapter::with_defaults();
    adapter.set_tokens(tokens);
    if let Some(new_id) = spawn_chrome_pane(app, Box::new(adapter), "KeybindsHelp") {
        app.keybinds_help_pane_id = Some(new_id);
    }
}

pub(crate) fn toggle_logs_pane(app: &mut App) {
    if let Some(app_id) = app.logs_pane_id.take() {
        despawn_chrome_pane(app, app_id, "Logs");
        return;
    }
    let tokens = current_tokens(app);
    let mut adapter = LogsAdapter::new();
    adapter.set_tokens(tokens);

    // Seed with the in-memory log ring's tail so the pane shows recent
    // activity immediately. Per-frame appends land in
    // `App::refresh_logs_pane`.
    use crate::adapters::logs::{LogLevel, LogRow};
    let ring = crate::logging::log_ring();
    let watermark = if let Ok(buf) = ring.lock() {
        for entry in buf.iter() {
            let level = match entry.level {
                log::Level::Error => LogLevel::Error,
                log::Level::Warn => LogLevel::Warn,
                log::Level::Info => LogLevel::Info,
                log::Level::Debug => LogLevel::Debug,
                log::Level::Trace => LogLevel::Trace,
            };
            adapter.push(LogRow::new(level, entry.target.clone(), entry.message.clone()));
        }
        buf.len()
    } else {
        0
    };
    app.logs_watermark = watermark;

    if let Some(new_id) = spawn_chrome_pane(app, Box::new(adapter), "Logs") {
        app.logs_pane_id = Some(new_id);
    }
}

/// Ensure the background filesystem watcher is running on the current
/// working directory.  Multiple panes (FilesWatch, Diff) share the single
/// watcher.  Called from the toggle helpers of every pane that depends on
/// file-write events.  Calling when the watcher is already up is a no-op.
fn ensure_files_watcher(app: &mut App) {
    if app.files_watcher.is_some() {
        return;
    }
    if let Ok(cwd) = std::env::current_dir() {
        app.files_watcher = crate::files_watcher::FilesWatcher::new(&cwd);
        if app.files_watcher.is_none() {
            log::warn!("files_watcher setup failed; dependent panes will not auto-refresh");
        }
    }
}

/// Tear the background filesystem watcher down when no pane needs it.
/// Called from the closing branch of every dependent pane.
fn maybe_stop_files_watcher(app: &mut App) {
    let still_needed = app.files_watch_pane_id.is_some() || app.diff_pane_id.is_some();
    if !still_needed {
        app.files_watcher = None;
    }
}

pub(crate) fn toggle_files_watch_pane(app: &mut App) {
    if let Some(app_id) = app.files_watch_pane_id.take() {
        despawn_chrome_pane(app, app_id, "FilesWatch");
        // Stop the background watcher only when no other dependent pane
        // (today: Diff) needs it.
        maybe_stop_files_watcher(app);
        return;
    }
    let tokens = current_tokens(app);
    let mut adapter = FilesWatchAdapter::new();
    adapter.set_tokens(tokens);
    if let Some(new_id) = spawn_chrome_pane(app, Box::new(adapter), "FilesWatch") {
        app.files_watch_pane_id = Some(new_id);
        ensure_files_watcher(app);
    }
}

pub(crate) fn toggle_diff_pane(app: &mut App) {
    if let Some(app_id) = app.diff_pane_id.take() {
        despawn_chrome_pane(app, app_id, "Diff");
        // The Diff pane shares the FilesWatcher with FilesWatch; release
        // the watcher only when neither pane is still open.
        maybe_stop_files_watcher(app);
        return;
    }
    let tokens = current_tokens(app);
    let mut adapter = DiffAdapter::empty();
    adapter.set_tokens(tokens);

    // Seed the adapter with `git diff` output for the current working tree.
    // Hard-wires the `git` binary because phantom-context's git plumbing
    // doesn't expose a unified-diff string today. Falls back silently to
    // an empty view if the command fails (no repo, no git, etc.).
    if let Ok(cwd) = std::env::current_dir()
        && let Ok(output) = std::process::Command::new("git")
            .arg("diff")
            .arg("HEAD")
            .current_dir(&cwd)
            .output()
        && output.status.success()
    {
        let body = String::from_utf8_lossy(&output.stdout);
        if !body.trim().is_empty() {
            let file_label = cwd
                .file_name()
                .and_then(|s| s.to_str())
                .map(|s| format!("{s} · git diff HEAD"))
                .unwrap_or_else(|| "git diff HEAD".to_string());
            adapter.set_view(crate::adapters::diff::DiffView::parse(file_label, &body));
        }
    }

    if let Some(new_id) = spawn_chrome_pane(app, Box::new(adapter), "Diff") {
        app.diff_pane_id = Some(new_id);
        // Diff auto-refreshes from FilesWatcher events drained in
        // `App::refresh_files_watch_pane`.  Start the watcher if it isn't
        // already running for another pane.
        ensure_files_watcher(app);
    }
}

pub(crate) fn toggle_memory_pane(app: &mut App) {
    if let Some(app_id) = app.memory_pane_id.take() {
        despawn_chrome_pane(app, app_id, "Memory");
        return;
    }
    let tokens = current_tokens(app);
    let mut adapter = MemoryInspectorAdapter::new();
    adapter.set_tokens(tokens);

    let mut rows: Vec<crate::adapters::memory_inspector::MemoryEntry> = Vec::new();

    // Source 1: Claude Code per-project auto-memory dir.
    // Each .md file has a YAML frontmatter with `name` and `description`; we
    // surface those as key/value rows. MEMORY.md (the index) is skipped
    // because its content is one-liners that double-up with the individual files.
    let project_dir = std::env::current_dir().ok();
    let project_slug = project_dir.as_ref().and_then(|p| {
        p.to_str().map(|s| s.replace('/', "-"))
    });
    if let Some(slug) = project_slug {
        let memory_dir = std::env::var("HOME")
            .ok()
            .map(|h| std::path::PathBuf::from(h).join(".claude/projects").join(slug).join("memory"));
        if let Some(dir) = memory_dir
            && let Ok(entries) = std::fs::read_dir(&dir)
        {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) != Some("md") {
                    continue;
                }
                let file_name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
                if file_name == "MEMORY.md" {
                    continue;
                }
                let Ok(body) = std::fs::read_to_string(&path) else { continue };
                let name = parse_frontmatter_field(&body, "name").unwrap_or_else(|| file_name.trim_end_matches(".md").to_string());
                let description = parse_frontmatter_field(&body, "description").unwrap_or_else(|| "(no description)".to_string());
                rows.push(crate::adapters::memory_inspector::MemoryEntry::new(name, description));
            }
        }
    }

    // Source 2: wolf session journals — operator-facing session notes
    // captured by the `/journal` skill. Each .md file is a structured
    // session report; we surface the filename stem (date + slug) and the
    // first heading line as a key/value pair.
    let journal_dir = std::env::var("HOME")
        .ok()
        .map(|h| std::path::PathBuf::from(h).join(".wolf/sessions/journal"));
    if let Some(dir) = journal_dir {
        for entry in scan_wolf_journal(&dir, 30) {
            rows.push(entry);
        }
    }

    // Sort project-memory rows alphabetically by key, but keep the journal
    // rows (already newest-first) at the bottom of the list so the pane
    // surfaces the auto-memory front-matter first.
    let split_idx = rows.iter().position(|r| r.key.starts_with("journal · ")).unwrap_or(rows.len());
    rows[..split_idx].sort_by(|a, b| a.key.cmp(&b.key));
    adapter.set_entries(rows);

    if let Some(name) = project_dir
        .as_ref()
        .and_then(|p| p.file_name().and_then(|s| s.to_str()))
    {
        adapter.set_project(name);
    }

    if let Some(new_id) = spawn_chrome_pane(app, Box::new(adapter), "Memory") {
        app.memory_pane_id = Some(new_id);
    }
}

/// Pull a single YAML-frontmatter field from a markdown file (very loose
/// parsing — looks for `<key>:` at the start of a line within the leading
/// `---` block). Strips surrounding quotes.
fn parse_frontmatter_field(body: &str, key: &str) -> Option<String> {
    let mut in_fm = false;
    for line in body.lines() {
        if line.trim() == "---" {
            if in_fm {
                return None;
            }
            in_fm = true;
            continue;
        }
        if !in_fm {
            continue;
        }
        if let Some(rest) = line.strip_prefix(&format!("{key}:")) {
            let v = rest.trim().trim_matches(|c| c == '"' || c == '\'');
            if v.is_empty() {
                return None;
            }
            return Some(v.to_string());
        }
    }
    None
}

/// Scan `dir` for `.md` files and return up to `cap` memory-inspector entries,
/// newest-first by mtime, with key `"journal · <stem>"` and value set to the
/// first non-empty heading line of the file.
///
/// Returns an empty `Vec` when `dir` does not exist or cannot be read so the
/// caller never has to wrap this in extra option-handling.  Extracted into a
/// free function so unit tests can target a temp directory.
pub(crate) fn scan_wolf_journal(
    dir: &std::path::Path,
    cap: usize,
) -> Vec<crate::adapters::memory_inspector::MemoryEntry> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };

    let mut journal_rows: Vec<(std::time::SystemTime, String, String)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        let file_name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        let stem = file_name.trim_end_matches(".md").to_string();
        let Ok(body) = std::fs::read_to_string(&path) else { continue };
        let first_line = body
            .lines()
            .find(|l| !l.trim().is_empty())
            .map(|l| l.trim_start_matches('#').trim().to_string())
            .unwrap_or_else(|| "(empty)".to_string());
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        journal_rows.push((mtime, format!("journal · {stem}"), first_line));
    }
    journal_rows.sort_by(|a, b| b.0.cmp(&a.0));
    journal_rows
        .into_iter()
        .take(cap)
        .map(|(_, key, val)| crate::adapters::memory_inspector::MemoryEntry::new(key, val))
        .collect()
}

pub(crate) fn toggle_fleet_pane(app: &mut App) {
    if let Some(app_id) = app.fleet_pane_id.take() {
        despawn_chrome_pane(app, app_id, "Fleet");
        return;
    }
    let tokens = current_tokens(app);
    let mut adapter = FleetAdapter::new();
    adapter.set_tokens(tokens);

    // Seed with the local node's identity; phantom-fleet's FleetRunner
    // lives in a separate binary today, so a live remote-node feed lands
    // when the App hosts a FleetRunner directly.
    use crate::adapters::fleet::{FleetNode, FleetNodeState};
    let host = std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "localhost".to_string());
    let meta = std::env::consts::OS.to_string();
    adapter.set_nodes(vec![FleetNode::new(host, FleetNodeState::Self_, meta)]);

    if let Some(new_id) = spawn_chrome_pane(app, Box::new(adapter), "Fleet") {
        app.fleet_pane_id = Some(new_id);
    }
}

pub(crate) fn toggle_plugins_pane(app: &mut App) {
    if let Some(app_id) = app.plugins_pane_id.take() {
        despawn_chrome_pane(app, app_id, "Plugins");
        return;
    }
    let tokens = current_tokens(app);
    let mut adapter = PluginsAdapter::new();
    adapter.set_tokens(tokens);

    // Pull live plugin info from the App's PluginRegistry.
    use crate::adapters::plugins::{PluginEntry, PluginState};
    let entries: Vec<PluginEntry> = app
        .plugin_registry
        .list()
        .into_iter()
        .map(|p| {
            let state = if !p.enabled {
                PluginState::Disabled
            } else if p.hooks == 0 && p.commands == 0 {
                PluginState::Idle
            } else {
                PluginState::Active
            };
            PluginEntry::new(p.name, p.version, state)
        })
        .collect();
    adapter.set_plugins(entries);

    if let Some(new_id) = spawn_chrome_pane(app, Box::new(adapter), "Plugins") {
        app.plugins_pane_id = Some(new_id);
    }
}

pub(crate) fn toggle_database_pane(app: &mut App) {
    if let Some(app_id) = app.database_pane_id.take() {
        despawn_chrome_pane(app, app_id, "Database");
        return;
    }
    let tokens = current_tokens(app);
    let mut adapter = DatabaseAdapter::new();
    adapter.set_tokens(tokens);

    // Seed with the bundle-store schema. The store is structured (frames,
    // bundles, vectors), not a flat SQLite table the user can browse, so
    // we present the high-level shape and the last query the runtime has
    // recorded if any.
    use crate::adapters::database::DbColumn;
    let backend = if app.bundle_store.is_some() { "sqlite" } else { "(disabled)" };
    let columns = vec![
        DbColumn::new("bundles", "table", "capture bundles"),
        DbColumn::new("frames", "table", "per-bundle PNG frames"),
        DbColumn::new("vectors", "table", "embedding vectors (LanceDB)"),
        DbColumn::new("events", "table", "JSONL event log"),
    ];
    adapter.set_schema("phantom-bundle-store", columns.len() as u64, columns);
    adapter.set_last_query(format!("backend: {backend}"));

    if let Some(new_id) = spawn_chrome_pane(app, Box::new(adapter), "Database") {
        app.database_pane_id = Some(new_id);
    }
}

pub(crate) fn toggle_voice_stt_pane(app: &mut App) {
    if let Some(app_id) = app.voice_stt_pane_id.take() {
        despawn_chrome_pane(app, app_id, "VoiceStt");
        return;
    }
    let tokens = current_tokens(app);
    let mut adapter = VoiceSttAdapter::new();
    adapter.set_tokens(tokens);
    // Drop a baseline of synthetic levels so the visualiser isn't empty
    // before the real STT backend wires in (phantom-stt is 🔧 still).
    // Per-frame animation lands in `App::refresh_voice_stt_pane`.
    for &lvl in &[0.2, 0.5, 0.4, 0.7, 0.6, 0.3, 0.5, 0.8, 0.7, 0.4, 0.6, 0.5] {
        adapter.push_level(lvl);
    }
    if let Some(new_id) = spawn_chrome_pane(app, Box::new(adapter), "VoiceStt") {
        app.voice_stt_pane_id = Some(new_id);
    }
}

/// Spawn the boot pane at startup. Called by `App::new` before the agent
/// pane lands. Returns the new `AppId` so the App can advance phases via
/// `accept_command "advance"` and finally despawn via `remove_adapter`.
#[allow(dead_code)] // Wired by Task 32 (BootAdapter startup integration).
pub(crate) fn spawn_boot_pane(app: &mut App) -> Option<u32> {
    let tokens = current_tokens(app);
    let mut adapter = BootAdapter::new();
    adapter.set_tokens(tokens);
    spawn_chrome_pane(app, Box::new(adapter), "Boot")
}

// ---------------------------------------------------------------------------
// System / capture pane toggles (impl on App)
// ---------------------------------------------------------------------------

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

    // ── Tier 3a: memory/journal helpers ───────────────────────────────────
    use std::fs;
    use std::time::{Duration, SystemTime};

    #[test]
    fn parse_frontmatter_field_reads_quoted_value() {
        let body = "---\nname: \"Stack\"\ndescription: 'rust 2024'\n---\nbody\n";
        assert_eq!(parse_frontmatter_field(body, "name").as_deref(), Some("Stack"));
        assert_eq!(
            parse_frontmatter_field(body, "description").as_deref(),
            Some("rust 2024")
        );
    }

    #[test]
    fn scan_wolf_journal_returns_entries_with_journal_prefix() {
        // Build a temp dir mimicking the layout of ~/.wolf/sessions/journal.
        let tmp = std::env::temp_dir().join(format!(
            "phantom-journal-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).expect("temp dir");

        let alpha = tmp.join("2026-01-01-alpha.md");
        let beta = tmp.join("2026-02-02-beta.md");
        fs::write(&alpha, "# Session: alpha — first\nbody\n").unwrap();
        fs::write(&beta, "# Session: beta — second\nbody\n").unwrap();

        let rows = scan_wolf_journal(&tmp, 10);
        assert_eq!(rows.len(), 2, "must surface two journal entries");

        // Key carries the "journal · " prefix so the row is distinguishable
        // from project-memory rows in the unified list.
        assert!(rows.iter().all(|r| r.key.starts_with("journal · ")));
        // Both entries surface their stem in the key and their heading in
        // the value.
        let keys: Vec<_> = rows.iter().map(|r| r.key.clone()).collect();
        assert!(keys.iter().any(|k| k.contains("alpha")), "keys: {keys:?}");
        assert!(keys.iter().any(|k| k.contains("beta")), "keys: {keys:?}");
        let vals: Vec<_> = rows.iter().map(|r| r.value.clone()).collect();
        assert!(vals.iter().any(|v| v.contains("alpha")), "vals: {vals:?}");
        assert!(vals.iter().any(|v| v.contains("beta")), "vals: {vals:?}");

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn scan_wolf_journal_sorts_newest_first_when_mtime_differs() {
        // Two files written with a sleep in between so the OS records
        // distinct mtimes. The newer file must appear first.
        let tmp = std::env::temp_dir().join(format!(
            "phantom-journal-order-test-{}-{:?}",
            std::process::id(),
            SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos(),
        ));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).expect("temp dir");

        fs::write(tmp.join("a.md"), "# A\n").unwrap();
        std::thread::sleep(Duration::from_millis(50));
        fs::write(tmp.join("b.md"), "# B\n").unwrap();

        let rows = scan_wolf_journal(&tmp, 10);
        assert_eq!(rows.len(), 2);
        // `b.md` was written second, so it must surface first.
        assert!(
            rows[0].key.contains(" b"),
            "newer entry must be first; got {}",
            rows[0].key
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn scan_wolf_journal_caps_at_limit() {
        let tmp = std::env::temp_dir().join(format!(
            "phantom-journal-cap-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).expect("temp dir");

        for i in 0..50 {
            let path = tmp.join(format!("entry-{i:02}.md"));
            fs::write(&path, format!("# Session {i}\n")).unwrap();
        }

        let rows = scan_wolf_journal(&tmp, 7);
        assert_eq!(rows.len(), 7, "cap must clamp the result");

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn scan_wolf_journal_returns_empty_when_dir_missing() {
        let path = std::path::PathBuf::from("/nonexistent/phantom-no-such-dir");
        let rows = scan_wolf_journal(&path, 10);
        assert!(rows.is_empty());
    }
}
