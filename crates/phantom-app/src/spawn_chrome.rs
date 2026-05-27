//! Shared spawn helper for the mockup's chrome adapters
//! (Settings / Notifications / Console / KeybindsHelp / Logs / FilesWatch /
//! Diff / Memory / Fleet / Plugins / Database / VoiceStt).
//!
//! Mirrors the `spawn_inspector_pane` pattern: split the focused pane
//! vertically, register the new adapter in the new child, resize, focus.
//!
//! Every chrome-pane keybind in `App` calls one of the `toggle_*_pane`
//! helpers below: the helper spawns a fresh adapter if the pane is closed
//! and despawns it if it's already open.

use log::{info, warn};

use phantom_adapter::AppAdapter;
use phantom_ui::tokens::Tokens;
use phantom_ui::RenderCtx;

use crate::adapters::{
    BootAdapter, ConsoleAdapter, DatabaseAdapter, DiffAdapter, FilesWatchAdapter, FleetAdapter,
    KeybindsHelpAdapter, LogsAdapter, MemoryInspectorAdapter, NotificationsAdapter, PluginsAdapter,
    SettingsAdapter, VoiceSttAdapter,
};
use crate::adapters::settings::{Slider01, SettingsView};
use crate::app::App;

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

#[cfg(test)]
mod tests {
    use super::*;
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
