//! Gallery-mode adapter spawning: populates a 4-column × 3-row tiled
//! workspace with 12 chrome adapters at cold-launch, mirroring
//! `docs/mockups/apps.html`.
//!
//! Invoked from `App::with_config_scaled` when `PhantomConfig::gallery_mode`
//! is `true` (set by the `--gallery` CLI flag). The SetupAdapter is NOT
//! registered in this path — gallery mode is purely a visual showcase.
//!
//! ## Design
//!
//! The layout engine is switched into gallery mode via
//! [`LayoutEngine::set_grid_mode`] before any panes are added. Every
//! subsequent `add_pane()` call returns a pane node sized to one cell of
//! the 4×3 grid (25% width × 33% height); `flex_wrap: Wrap` then tiles
//! successive panes onto the next row automatically.
//!
//! Per-pane adapter construction reuses the patterns from `spawn_chrome.rs`
//! but bypasses the split-based plumbing: instead of "split the focused
//! pane, mount adapter in the new child", we add a fresh layout pane and
//! mount the adapter on it directly via `register_adapter_at_pane`.
//!
//! ## Tile order (left → right, top → bottom)
//!
//! Row 1: KeybindsHelp · Inspector · Notifications · Memory
//! Row 2: Diff         · Files     · Logs          · Console
//! Row 3: Fleet        · Plugins   · Database      · VoiceStt

use std::sync::{Arc, RwLock};

use log::{info, warn};

use phantom_adapter::AppAdapter;
use phantom_scene::clock::Cadence;
use phantom_ui::layout::PaneId;
use phantom_ui::tokens::Tokens;
use phantom_ui::RenderCtx;

use crate::adapters::{
    ConsoleAdapter, DatabaseAdapter, DiffAdapter, FilesWatchAdapter, FleetAdapter,
    KeybindsHelpAdapter, LogsAdapter, MemoryInspectorAdapter, NotificationsAdapter, PluginsAdapter,
    VoiceSttAdapter,
};
use crate::adapters::inspector::InspectorAdapter;
use crate::app::App;

/// Gallery grid dimensions. 4 columns × 3 rows = 12 tiles. The mockup
/// `apps.html` uses irregular row counts (some `row two`, some `row three`)
/// — we collapse that to a uniform grid because the running app doesn't
/// distinguish "core" vs "chrome" tiles the way the static mockup does.
const GALLERY_COLS: u32 = 4;
const GALLERY_ROWS: u32 = 3;

/// Build a `Tokens` snapshot from the App's currently active theme.
///
/// Mirrors the helper in `spawn_chrome.rs` so each gallery adapter renders
/// with live theme tokens from frame 1. Falls back to phosphor if the
/// theme registry can't resolve the configured name.
fn current_tokens(app: &App) -> Tokens {
    let ctx = RenderCtx::new(app.cell_size, 1.0);
    Tokens::for_theme_name(&app.theme.name.to_lowercase(), ctx.clone())
        .unwrap_or_else(|| Tokens::phosphor(ctx))
}

/// Add a fresh layout pane in gallery mode, mount `adapter` on it, register
/// in the scene tree, and return the new pane id along with the assigned
/// `AppId`. Returns `None` if the layout engine refuses to add another pane.
fn spawn_gallery_tile(
    app: &mut App,
    adapter: Box<dyn AppAdapter>,
    label: &str,
) -> Option<(PaneId, u32)> {
    // Acquire a new pane in the gallery grid. With grid_mode set on the
    // content node every `add_pane` produces a 25% × 33% tile that
    // flex-wraps into the next row once 4 tiles fill a row.
    let pane_id = match app.layout.add_pane() {
        Ok(id) => id,
        Err(e) => {
            warn!("gallery: layout.add_pane failed for {label}: {e}");
            return None;
        }
    };

    let scene_node = app
        .scene
        .add_node(app.scene_content_node, phantom_scene::node::NodeKind::Pane);

    let app_id = app.coordinator.register_adapter_at_pane(
        adapter,
        pane_id,
        scene_node,
        Cadence::unlimited(),
        &mut app.layout,
    );

    info!("gallery: spawned {label} (AppId {app_id}, pane {pane_id:?})");
    Some((pane_id, app_id))
}

/// Spawn the full 12-tile gallery and track the resulting `AppId`s in the
/// matching `App::*_pane_id` slots so toggle-pane keybinds still work.
///
/// Returns the number of tiles that successfully landed in the grid.
/// Best-effort: any adapter constructor or registration failure logs a
/// warning and is skipped without aborting the rest of the gallery.
pub(crate) fn spawn_full_gallery(app: &mut App) -> usize {
    // Engage gallery mode on the layout engine. From this point on every
    // `add_pane` produces a 4×3 grid cell.
    if let Err(e) = app
        .layout
        .set_grid_mode(Some((GALLERY_COLS, GALLERY_ROWS)))
    {
        warn!("gallery: set_grid_mode failed: {e}; aborting auto-spawn");
        return 0;
    }

    let tokens = current_tokens(app);
    let mut spawned: usize = 0;

    // ── Tile 01 — KeybindsHelp (F1 cheat-sheet) ──────────────────────────
    {
        let mut adapter = KeybindsHelpAdapter::with_defaults();
        adapter.set_tokens(tokens.clone());
        if let Some((_pid, app_id)) =
            spawn_gallery_tile(app, Box::new(adapter), "KeybindsHelp")
        {
            app.keybinds_help_pane_id = Some(app_id);
            spawned += 1;
        }
    }

    // ── Tile 02 — Inspector ──────────────────────────────────────────────
    {
        use phantom_agents::inspector::InspectorView;
        // Use or create the shared snapshot and tokens Arcs so a future
        // `inspect` command can populate the same view live.
        let snapshot = app
            .inspector_snapshot
            .get_or_insert_with(|| Arc::new(RwLock::new(InspectorView::empty())))
            .clone();
        let tokens_arc = app
            .inspector_tokens
            .get_or_insert_with(|| Arc::new(RwLock::new(tokens.clone())))
            .clone();
        let adapter = InspectorAdapter::new(snapshot, tokens_arc);
        if let Some((_pid, app_id)) =
            spawn_gallery_tile(app, Box::new(adapter), "Inspector")
        {
            let _ = app_id;
            spawned += 1;
        }
    }

    // ── Tile 03 — Notifications ──────────────────────────────────────────
    {
        let mut adapter = NotificationsAdapter::new();
        adapter.set_tokens(tokens.clone());
        if let Some((_pid, app_id)) =
            spawn_gallery_tile(app, Box::new(adapter), "Notifications")
        {
            app.notifications_pane_id = Some(app_id);
            spawned += 1;
        }
    }

    // ── Tile 04 — Memory ─────────────────────────────────────────────────
    {
        let mut adapter = MemoryInspectorAdapter::new();
        adapter.set_tokens(tokens.clone());
        if let Some((_pid, app_id)) =
            spawn_gallery_tile(app, Box::new(adapter), "Memory")
        {
            app.memory_pane_id = Some(app_id);
            spawned += 1;
        }
    }

    // ── Tile 05 — Diff ───────────────────────────────────────────────────
    {
        let mut adapter = DiffAdapter::empty();
        adapter.set_tokens(tokens.clone());
        // Seed with `git diff HEAD` if we're in a repo. Best-effort.
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
                let label = cwd
                    .file_name()
                    .and_then(|s| s.to_str())
                    .map(|s| format!("{s} · git diff HEAD"))
                    .unwrap_or_else(|| "git diff HEAD".to_string());
                adapter.set_view(crate::adapters::diff::DiffView::parse(label, &body));
            }
        }
        if let Some((_pid, app_id)) =
            spawn_gallery_tile(app, Box::new(adapter), "Diff")
        {
            app.diff_pane_id = Some(app_id);
            spawned += 1;
        }
    }

    // ── Tile 06 — Files (watch) ──────────────────────────────────────────
    {
        let mut adapter = FilesWatchAdapter::new();
        adapter.set_tokens(tokens.clone());
        if let Some((_pid, app_id)) =
            spawn_gallery_tile(app, Box::new(adapter), "FilesWatch")
        {
            app.files_watch_pane_id = Some(app_id);
            if let Ok(cwd) = std::env::current_dir() {
                app.files_watcher = crate::files_watcher::FilesWatcher::new(&cwd);
            }
            spawned += 1;
        }
    }

    // ── Tile 07 — Logs ───────────────────────────────────────────────────
    {
        use crate::adapters::logs::{LogLevel, LogRow};
        let mut adapter = LogsAdapter::new();
        adapter.set_tokens(tokens.clone());
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
        if let Some((_pid, app_id)) =
            spawn_gallery_tile(app, Box::new(adapter), "Logs")
        {
            app.logs_pane_id = Some(app_id);
            spawned += 1;
        }
    }

    // ── Tile 08 — Console ────────────────────────────────────────────────
    {
        let mut adapter = ConsoleAdapter::new();
        adapter.set_tokens(tokens.clone());
        if let Some((_pid, app_id)) =
            spawn_gallery_tile(app, Box::new(adapter), "Console")
        {
            app.console_pane_id = Some(app_id);
            spawned += 1;
        }
    }

    // ── Tile 09 — Fleet ──────────────────────────────────────────────────
    {
        use crate::adapters::fleet::{FleetNode, FleetNodeState};
        let mut adapter = FleetAdapter::new();
        adapter.set_tokens(tokens.clone());
        let host = std::process::Command::new("hostname")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "localhost".to_string());
        let meta = std::env::consts::OS.to_string();
        adapter.set_nodes(vec![FleetNode::new(host, FleetNodeState::Self_, meta)]);
        if let Some((_pid, app_id)) =
            spawn_gallery_tile(app, Box::new(adapter), "Fleet")
        {
            app.fleet_pane_id = Some(app_id);
            spawned += 1;
        }
    }

    // ── Tile 10 — Plugins ────────────────────────────────────────────────
    {
        use crate::adapters::plugins::{PluginEntry, PluginState};
        let mut adapter = PluginsAdapter::new();
        adapter.set_tokens(tokens.clone());
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
        if let Some((_pid, app_id)) =
            spawn_gallery_tile(app, Box::new(adapter), "Plugins")
        {
            app.plugins_pane_id = Some(app_id);
            spawned += 1;
        }
    }

    // ── Tile 11 — Database ───────────────────────────────────────────────
    {
        use crate::adapters::database::DbColumn;
        let mut adapter = DatabaseAdapter::new();
        adapter.set_tokens(tokens.clone());
        let backend = if app.bundle_store.is_some() {
            "sqlite"
        } else {
            "(disabled)"
        };
        let columns = vec![
            DbColumn::new("bundles", "table", "capture bundles"),
            DbColumn::new("frames", "table", "per-bundle PNG frames"),
            DbColumn::new("vectors", "table", "embedding vectors (LanceDB)"),
            DbColumn::new("events", "table", "JSONL event log"),
        ];
        adapter.set_schema("phantom-bundle-store", columns.len() as u64, columns);
        adapter.set_last_query(format!("backend: {backend}"));
        if let Some((_pid, app_id)) =
            spawn_gallery_tile(app, Box::new(adapter), "Database")
        {
            app.database_pane_id = Some(app_id);
            spawned += 1;
        }
    }

    // ── Tile 12 — VoiceStt ───────────────────────────────────────────────
    {
        let mut adapter = VoiceSttAdapter::new();
        adapter.set_tokens(tokens.clone());
        for &lvl in &[0.2, 0.5, 0.4, 0.7, 0.6, 0.3, 0.5, 0.8, 0.7, 0.4, 0.6, 0.5] {
            adapter.push_level(lvl);
        }
        if let Some((_pid, app_id)) =
            spawn_gallery_tile(app, Box::new(adapter), "VoiceStt")
        {
            app.voice_stt_pane_id = Some(app_id);
            spawned += 1;
        }
    }

    // Recompute the layout so every freshly added pane has a real rect by
    // the first render frame.
    let width = app.gpu.surface_config.width;
    let height = app.gpu.surface_config.height;
    let _ = app.layout.resize(width as f32, height as f32);

    info!(
        "gallery: spawned {spawned}/{} tiles in {GALLERY_COLS}x{GALLERY_ROWS} grid",
        GALLERY_COLS * GALLERY_ROWS
    );
    spawned
}
