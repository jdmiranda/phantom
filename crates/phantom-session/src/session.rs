use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Saved state of a single terminal pane.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneState {
    pub working_dir: String,
    pub is_focused: bool,
    pub cols: u16,
    pub rows: u16,
    pub title: String,
    /// How the pane was split relative to its parent.
    pub split: Option<SplitDirection>,
}

/// Direction a pane was split from its parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SplitDirection {
    Horizontal,
    Vertical,
}

/// Saved CRT shader parameters — persisted so debug HUD tuning survives restarts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedShaderParams {
    pub scanline_intensity: f32,
    pub bloom_intensity: f32,
    pub chromatic_aberration: f32,
    pub curvature: f32,
    pub vignette_intensity: f32,
    pub noise_intensity: f32,
}

/// Full saved session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionState {
    /// Schema version for forward compatibility.
    pub version: u32,
    /// When saved (unix epoch seconds).
    pub timestamp: u64,
    pub project_dir: String,
    pub project_name: String,
    pub git_branch: Option<String>,
    pub panes: Vec<PaneState>,
    pub theme_name: String,
    pub font_size: f32,
    /// Short note about what the user was doing.
    pub activity: Option<String>,
    /// CRT shader params at the time of save (debug HUD tuning etc.).
    #[serde(default)]
    pub shader_params: Option<SavedShaderParams>,
}

/// Summary for listing sessions without loading full state.
#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub path: PathBuf,
    pub project_name: String,
    pub timestamp: u64,
    pub pane_count: usize,
    pub activity: Option<String>,
}

// ---------------------------------------------------------------------------
// SessionManager
// ---------------------------------------------------------------------------

/// Session persistence manager.
///
/// Sessions are stored as JSON files under `~/.config/phantom/sessions/`
/// with the naming scheme `{project_hash}_{timestamp}.json`.
pub struct SessionManager {
    session_dir: PathBuf,
}

impl SessionManager {
    /// Return the default session directory path without creating it.
    ///
    /// Falls back to `"."` as an absolute last resort when `HOME` is unset.
    pub fn session_dir_path() -> PathBuf {
        if let Ok(home) = std::env::var("HOME") {
            PathBuf::from(home)
                .join(".config")
                .join("phantom")
                .join("sessions")
        } else {
            PathBuf::from(".")
        }
    }

    /// Create a session manager using the default session directory
    /// (`~/.config/phantom/sessions/`).
    ///
    /// The directory is determined by [`session_dir_path`], which is the single
    /// source of truth for that path. Adding XDG support or any other path
    /// change only needs to happen in that one function.
    pub fn new() -> Result<Self> {
        let session_dir = session_dir_path()?;
        fs::create_dir_all(&session_dir)
            .with_context(|| format!("failed to create session dir: {}", session_dir.display()))?;
        Ok(Self { session_dir })
    }

    /// Create a session manager rooted at a custom directory (useful for tests).
    pub fn with_dir(session_dir: PathBuf) -> Result<Self> {
        fs::create_dir_all(&session_dir)
            .with_context(|| format!("failed to create session dir: {}", session_dir.display()))?;
        Ok(Self { session_dir })
    }

    /// Save a session state to disk.
    ///
    /// Returns the path to the written file.
    pub fn save(&self, state: &SessionState) -> Result<PathBuf> {
        let hash = project_hash(&state.project_dir);
        let filename = format!("{hash}_{}.json", state.timestamp);
        let path = self.session_dir.join(&filename);
        let json =
            serde_json::to_string_pretty(state).context("failed to serialize session state")?;
        fs::write(&path, json)
            .with_context(|| format!("failed to write session file: {}", path.display()))?;
        log::info!("session saved: {}", path.display());
        Ok(path)
    }

    /// Load the most recent session for a project directory.
    ///
    /// Returns `None` if no session exists for the given project.
    pub fn load_latest(&self, project_dir: &str) -> Result<Option<SessionState>> {
        let hash = project_hash(project_dir);
        let prefix = format!("{hash}_");

        let mut matching: Vec<PathBuf> = self.session_files()?.into_iter().filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(&prefix))
        }).collect();

        // Sort by timestamp descending (encoded in filename).
        matching.sort_by(|a, b| {
            let ts_a = timestamp_from_filename(a);
            let ts_b = timestamp_from_filename(b);
            ts_b.cmp(&ts_a)
        });

        match matching.first() {
            Some(path) => {
                let state = self.load(path)?;
                Ok(Some(state))
            }
            None => Ok(None),
        }
    }

    /// Load a specific session by file path.
    pub fn load(&self, path: &Path) -> Result<SessionState> {
        let contents = fs::read_to_string(path)
            .with_context(|| format!("failed to read session file: {}", path.display()))?;
        let state: SessionState = serde_json::from_str(&contents)
            .with_context(|| format!("failed to parse session file: {}", path.display()))?;
        Ok(state)
    }

    /// List all saved sessions, most recent first.
    pub fn list_sessions(&self) -> Result<Vec<SessionSummary>> {
        let files = self.session_files()?;
        let mut summaries = Vec::new();

        for path in &files {
            match self.load(path) {
                Ok(state) => {
                    summaries.push(SessionSummary {
                        path: path.clone(),
                        project_name: state.project_name,
                        timestamp: state.timestamp,
                        pane_count: state.panes.len(),
                        activity: state.activity,
                    });
                }
                Err(e) => {
                    log::warn!("skipping corrupt session file {}: {e}", path.display());
                }
            }
        }

        // Most recent first.
        summaries.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        Ok(summaries)
    }

    /// Delete sessions older than `max_age_days`.
    ///
    /// Returns the number of files deleted.
    pub fn cleanup(&self, max_age_days: u32) -> Result<u32> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let cutoff = now.saturating_sub(u64::from(max_age_days) * 86_400);
        let mut deleted = 0u32;

        for path in self.session_files()? {
            let ts = timestamp_from_filename(&path);
            if ts < cutoff {
                if let Err(e) = fs::remove_file(&path) {
                    log::warn!("failed to delete {}: {e}", path.display());
                } else {
                    deleted += 1;
                }
            }
        }

        log::info!("session cleanup: deleted {deleted} file(s) older than {max_age_days} days");
        Ok(deleted)
    }

    /// Build a welcome-back message from a saved session.
    pub fn welcome_message(state: &SessionState) -> String {
        let mut parts = vec![format!(
            "Welcome back. You were working on {}",
            state.project_name
        )];

        if let Some(ref branch) = state.git_branch {
            parts.push(format!("on branch {branch}"));
        }

        let mut msg = parts.join(" ");
        msg.push('.');

        let pane_count = state.panes.len();
        if pane_count > 0 {
            msg.push_str(&format!(
                " {} pane{} open.",
                pane_count,
                if pane_count == 1 { "" } else { "s" }
            ));
        }

        if let Some(ref activity) = state.activity {
            msg.push(' ');
            msg.push_str(activity);
        }

        msg
    }

    // -----------------------------------------------------------------------
    // Test-only accessors
    // -----------------------------------------------------------------------

    /// Expose the resolved session directory for unit tests.
    #[cfg(test)]
    pub(crate) fn session_dir(&self) -> &PathBuf {
        &self.session_dir
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    /// Collect all `.json` session files in the session directory.
    fn session_files(&self) -> Result<Vec<PathBuf>> {
        let mut files = Vec::new();
        let entries = fs::read_dir(&self.session_dir).with_context(|| {
            format!(
                "failed to read session dir: {}",
                self.session_dir.display()
            )
        })?;
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                files.push(path);
            }
        }
        Ok(files)
    }
}

// ---------------------------------------------------------------------------
// Free helpers
// ---------------------------------------------------------------------------

/// Return the canonical session directory path: `$HOME/.config/phantom/sessions/`.
///
/// This is the single source of truth for the session directory location.
/// Both [`SessionManager::new`] and any caller that needs to know where sessions
/// live (e.g. `is_session_restore` checks) must call this function rather than
/// re-deriving the path independently.
///
/// # Errors
/// Returns an error if the `HOME` environment variable is not set.
pub fn session_dir_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home)
        .join(".config")
        .join("phantom")
        .join("sessions"))
}

/// Deterministic hash of a project directory path, used as a filename prefix.
fn project_hash(project_dir: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    project_dir.hash(&mut hasher);
    hasher.finish()
}

/// Extract the unix timestamp from a session filename like `{hash}_{timestamp}.json`.
fn timestamp_from_filename(path: &Path) -> u64 {
    path.file_stem()
        .and_then(|s| s.to_str())
        .and_then(|s| s.rsplit('_').next())
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0)
}

/// Return the current unix epoch in seconds.
#[cfg(test)]
fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(std::time::Duration::ZERO)
        .as_secs()
}

// ---------------------------------------------------------------------------
// Session-restore detection
// ---------------------------------------------------------------------------

/// Returns `true` when this launch should skip the boot animation.
///
/// Two signals trigger a restore:
/// 1. `PHANTOM_RESTORING=1` environment variable — set by the supervisor or a
///    launch script when restarting after a crash or explicit restore.
/// 2. A `{hash}_{timestamp}.json` session file exists in `session_dir` for
///    `project_dir` — the normal case after a clean shutdown + re-launch.
///
/// The check is cheap: only filenames are inspected, no JSON is parsed.
/// Returns `false` on any I/O error so the caller never has to handle an
/// error just to decide whether to show the boot animation.
pub fn is_session_restore(session_dir: &Path, project_dir: &str) -> bool {
    is_session_restore_with_env(
        session_dir,
        project_dir,
        std::env::var("PHANTOM_RESTORING").ok().as_deref(),
    )
}

/// Inner implementation that accepts the env-var value as a parameter.
///
/// Separate from [`is_session_restore`] so tests can inject the value without
/// mutating the process environment (which is unsafe in Rust 2024 and racy
/// across parallel test threads).
fn is_session_restore_with_env(
    session_dir: &Path,
    project_dir: &str,
    phantom_restoring: Option<&str>,
) -> bool {
    if phantom_restoring == Some("1") {
        return true;
    }

    let hash = project_hash(project_dir);
    let prefix = format!("{hash}_");

    let Ok(entries) = fs::read_dir(session_dir) else {
        return false;
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(&prefix) && name.ends_with(".json") {
            return true;
        }
    }

    false
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Helper: create a `SessionManager` backed by a temp directory.
    fn test_manager() -> (SessionManager, TempDir) {
        let dir = TempDir::new().unwrap();
        let mgr = SessionManager::with_dir(dir.path().to_path_buf()).unwrap();
        (mgr, dir)
    }

    /// Helper: build a minimal valid session state.
    fn sample_state(project_dir: &str, project_name: &str, timestamp: u64) -> SessionState {
        SessionState {
            version: 1,
            timestamp,
            project_dir: project_dir.into(),
            project_name: project_name.into(),
            git_branch: Some("main".into()),
            panes: vec![
                PaneState {
                    working_dir: project_dir.into(),
                    is_focused: true,
                    cols: 120,
                    rows: 40,
                    title: "zsh".into(),
                    split: None,
                },
            ],
            theme_name: "dracula".into(),
            font_size: 14.0,
            activity: Some("Implementing session save".into()),
            shader_params: None,
        }
    }

    #[test]
    fn save_and_load_round_trip() {
        let (mgr, _dir) = test_manager();
        let state = sample_state("/home/dev/phantom", "phantom", 1_700_000_000);

        let path = mgr.save(&state).unwrap();
        let loaded = mgr.load(&path).unwrap();

        assert_eq!(loaded.version, state.version);
        assert_eq!(loaded.timestamp, state.timestamp);
        assert_eq!(loaded.project_dir, state.project_dir);
        assert_eq!(loaded.project_name, state.project_name);
        assert_eq!(loaded.git_branch, state.git_branch);
        assert_eq!(loaded.panes.len(), 1);
        assert_eq!(loaded.panes[0].cols, 120);
        assert_eq!(loaded.theme_name, "dracula");
        assert_eq!(loaded.font_size, 14.0);
        assert_eq!(loaded.activity, state.activity);
    }

    #[test]
    fn save_creates_json_file() {
        let (mgr, dir) = test_manager();
        let state = sample_state("/tmp/proj", "proj", 1_700_000_001);

        let path = mgr.save(&state).unwrap();

        assert!(path.exists());
        assert_eq!(path.extension().unwrap(), "json");
        assert!(path.starts_with(dir.path()));
    }

    #[test]
    fn load_latest_picks_newest() {
        let (mgr, _dir) = test_manager();
        let project_dir = "/home/dev/phantom";

        let old = sample_state(project_dir, "phantom", 1_700_000_000);
        let mid = sample_state(project_dir, "phantom", 1_700_000_100);
        let new = sample_state(project_dir, "phantom", 1_700_000_200);

        // Save out of order to verify sorting.
        mgr.save(&mid).unwrap();
        mgr.save(&old).unwrap();
        mgr.save(&new).unwrap();

        let latest = mgr.load_latest(project_dir).unwrap().unwrap();
        assert_eq!(latest.timestamp, 1_700_000_200);
    }

    #[test]
    fn load_latest_returns_none_for_unknown_project() {
        let (mgr, _dir) = test_manager();
        let state = sample_state("/home/dev/phantom", "phantom", 1_700_000_000);
        mgr.save(&state).unwrap();

        let result = mgr.load_latest("/home/dev/other").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn list_sessions_returns_all_sorted() {
        let (mgr, _dir) = test_manager();

        mgr.save(&sample_state("/proj/a", "alpha", 1_700_000_100))
            .unwrap();
        mgr.save(&sample_state("/proj/b", "bravo", 1_700_000_300))
            .unwrap();
        mgr.save(&sample_state("/proj/a", "alpha", 1_700_000_200))
            .unwrap();

        let sessions = mgr.list_sessions().unwrap();
        assert_eq!(sessions.len(), 3);
        // Most recent first.
        assert_eq!(sessions[0].timestamp, 1_700_000_300);
        assert_eq!(sessions[0].project_name, "bravo");
        assert_eq!(sessions[1].timestamp, 1_700_000_200);
        assert_eq!(sessions[2].timestamp, 1_700_000_100);
    }

    #[test]
    fn list_sessions_includes_pane_count_and_activity() {
        let (mgr, _dir) = test_manager();
        let mut state = sample_state("/proj/x", "xray", 1_700_000_000);
        state.panes.push(PaneState {
            working_dir: "/proj/x/tests".into(),
            is_focused: false,
            cols: 80,
            rows: 24,
            title: "tests".into(),
            split: Some(SplitDirection::Vertical),
        });
        state.activity = Some("Running tests".into());
        mgr.save(&state).unwrap();

        let sessions = mgr.list_sessions().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].pane_count, 2);
        assert_eq!(sessions[0].activity.as_deref(), Some("Running tests"));
    }

    #[test]
    fn cleanup_deletes_old_sessions() {
        let (mgr, _dir) = test_manager();

        let now = now_epoch();
        let old_ts = now - 100 * 86_400; // 100 days ago
        let recent_ts = now - 2 * 86_400; // 2 days ago

        mgr.save(&sample_state("/proj/a", "alpha", old_ts))
            .unwrap();
        mgr.save(&sample_state("/proj/b", "bravo", recent_ts))
            .unwrap();

        let deleted = mgr.cleanup(30).unwrap();
        assert_eq!(deleted, 1);

        let remaining = mgr.list_sessions().unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].project_name, "bravo");
    }

    #[test]
    fn cleanup_returns_zero_when_nothing_to_delete() {
        let (mgr, _dir) = test_manager();

        let now = now_epoch();
        mgr.save(&sample_state("/proj/a", "alpha", now)).unwrap();

        let deleted = mgr.cleanup(30).unwrap();
        assert_eq!(deleted, 0);
    }

    #[test]
    fn welcome_message_with_branch_and_activity() {
        let state = sample_state("/home/dev/phantom", "phantom", 1_700_000_000);
        let msg = SessionManager::welcome_message(&state);

        assert!(msg.contains("Welcome back"));
        assert!(msg.contains("phantom"));
        assert!(msg.contains("branch main"));
        assert!(msg.contains("1 pane open."));
        assert!(msg.contains("Implementing session save"));
    }

    #[test]
    fn welcome_message_without_branch() {
        let mut state = sample_state("/proj", "myproject", 1_700_000_000);
        state.git_branch = None;
        state.activity = None;

        let msg = SessionManager::welcome_message(&state);

        assert_eq!(
            msg,
            "Welcome back. You were working on myproject. 1 pane open."
        );
    }

    #[test]
    fn welcome_message_multiple_panes() {
        let mut state = sample_state("/proj", "multi", 1_700_000_000);
        state.panes.push(PaneState {
            working_dir: "/proj".into(),
            is_focused: false,
            cols: 80,
            rows: 24,
            title: "htop".into(),
            split: Some(SplitDirection::Horizontal),
        });
        state.git_branch = Some("feature/auth".into());
        state.activity = None;

        let msg = SessionManager::welcome_message(&state);

        assert!(msg.contains("branch feature/auth"));
        assert!(msg.contains("2 panes open."));
    }

    #[test]
    fn welcome_message_no_panes() {
        let mut state = sample_state("/proj", "empty", 1_700_000_000);
        state.panes.clear();
        state.git_branch = None;
        state.activity = None;

        let msg = SessionManager::welcome_message(&state);
        assert_eq!(msg, "Welcome back. You were working on empty.");
    }

    // =======================================================================
    // Issue #135 — single source of truth for session directory
    // =======================================================================

    /// `SessionManager::new()` must agree with `session_dir_path()` on the
    /// directory it uses.  If either is updated (e.g. XDG_DATA_HOME support)
    /// the other must be updated too — this test prevents silent divergence.
    #[test]
    fn session_manager_new_uses_session_dir_path() {
        let expected = session_dir_path().expect("HOME must be set for this test");
        let mgr = SessionManager::new().expect("SessionManager::new() failed");
        assert_eq!(
            mgr.session_dir(), &expected,
            "SessionManager::new() chose a different directory than session_dir_path()"
        );
    }

    #[test]
    fn project_hash_is_deterministic() {
        let h1 = project_hash("/home/dev/phantom");
        let h2 = project_hash("/home/dev/phantom");
        assert_eq!(h1, h2);
    }

    #[test]
    fn different_projects_get_different_hashes() {
        let h1 = project_hash("/home/dev/phantom");
        let h2 = project_hash("/home/dev/other");
        assert_ne!(h1, h2);
    }

    #[test]
    fn pane_state_serialization_round_trip() {
        let pane = PaneState {
            working_dir: "/home/dev".into(),
            is_focused: true,
            cols: 200,
            rows: 50,
            title: "nvim".into(),
            split: Some(SplitDirection::Vertical),
        };

        let json = serde_json::to_string(&pane).unwrap();
        let restored: PaneState = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.working_dir, pane.working_dir);
        assert_eq!(restored.is_focused, pane.is_focused);
        assert_eq!(restored.cols, pane.cols);
        assert_eq!(restored.rows, pane.rows);
        assert_eq!(restored.title, pane.title);
        assert_eq!(restored.split, Some(SplitDirection::Vertical));
    }

    #[test]
    fn load_returns_error_for_missing_file() {
        let (mgr, dir) = test_manager();
        let bogus = dir.path().join("nonexistent.json");
        let result = mgr.load(&bogus);
        assert!(result.is_err());
    }

    #[test]
    fn load_returns_error_for_corrupt_json() {
        let (mgr, dir) = test_manager();
        let bad_file = dir.path().join("bad_123.json");
        fs::write(&bad_file, "not valid json {{{").unwrap();

        let result = mgr.load(&bad_file);
        assert!(result.is_err());
    }

    // =======================================================================
    // Session restore round-trip tests
    // =======================================================================

    #[test]
    fn save_and_restore_preserves_theme_name() {
        let (mgr, _dir) = test_manager();
        let project_dir = "/home/dev/project";
        let state = SessionState {
            version: 1,
            timestamp: 1_700_000_500,
            project_dir: project_dir.into(),
            project_name: "project".into(),
            git_branch: Some("main".into()),
            panes: vec![PaneState {
                working_dir: project_dir.into(),
                is_focused: true,
                cols: 120,
                rows: 40,
                title: "zsh".into(),
                split: None,
            }],
            theme_name: "pipboy".into(),
            font_size: 16.0,
            activity: Some("testing session restore".into()),
            shader_params: None,
        };

        mgr.save(&state).unwrap();
        let restored = mgr.load_latest(project_dir).unwrap().unwrap();

        assert_eq!(restored.theme_name, "pipboy");
        assert_eq!(restored.font_size, 16.0);
        assert_eq!(restored.activity, Some("testing session restore".into()));
    }

    #[test]
    fn restore_multi_pane_session() {
        let (mgr, _dir) = test_manager();
        let project_dir = "/home/dev/multi";
        let state = SessionState {
            version: 1,
            timestamp: 1_700_000_600,
            project_dir: project_dir.into(),
            project_name: "multi".into(),
            git_branch: Some("feature".into()),
            panes: vec![
                PaneState {
                    working_dir: project_dir.into(),
                    is_focused: true,
                    cols: 120,
                    rows: 40,
                    title: "editor".into(),
                    split: None,
                },
                PaneState {
                    working_dir: project_dir.into(),
                    is_focused: false,
                    cols: 80,
                    rows: 24,
                    title: "tests".into(),
                    split: Some(SplitDirection::Vertical),
                },
                PaneState {
                    working_dir: project_dir.into(),
                    is_focused: false,
                    cols: 60,
                    rows: 20,
                    title: "logs".into(),
                    split: Some(SplitDirection::Horizontal),
                },
            ],
            theme_name: "dracula".into(),
            font_size: 14.0,
            activity: None,
            shader_params: None,
        };

        mgr.save(&state).unwrap();
        let restored = mgr.load_latest(project_dir).unwrap().unwrap();

        assert_eq!(restored.panes.len(), 3);
        assert_eq!(restored.panes[0].title, "editor");
        assert!(restored.panes[0].is_focused);
        assert_eq!(restored.panes[1].split, Some(SplitDirection::Vertical));
        assert_eq!(restored.panes[2].split, Some(SplitDirection::Horizontal));
    }

    #[test]
    fn welcome_message_from_restored_session() {
        let state = SessionState {
            version: 1,
            timestamp: 1_700_000_700,
            project_dir: "/proj".into(),
            project_name: "phantom".into(),
            git_branch: Some("feature/agents".into()),
            panes: vec![
                PaneState {
                    working_dir: "/proj".into(),
                    is_focused: true,
                    cols: 120,
                    rows: 40,
                    title: "zsh".into(),
                    split: None,
                },
                PaneState {
                    working_dir: "/proj".into(),
                    is_focused: false,
                    cols: 80,
                    rows: 24,
                    title: "htop".into(),
                    split: Some(SplitDirection::Horizontal),
                },
            ],
            theme_name: "pipboy".into(),
            font_size: 18.0,
            activity: Some("Wiring the scene graph".into()),
            shader_params: None,
        };

        let msg = SessionManager::welcome_message(&state);
        assert!(msg.contains("phantom"));
        assert!(msg.contains("feature/agents"));
        assert!(msg.contains("2 panes"));
        assert!(msg.contains("Wiring the scene graph"));
    }

    // =======================================================================
    // is_session_restore — boot-skip detection
    // =======================================================================

    #[test]
    fn session_restore_cold_launch_no_files() {
        let dir = TempDir::new().unwrap();
        assert!(
            !is_session_restore_with_env(dir.path(), "/home/dev/phantom", None),
            "empty session dir must not be detected as a restore"
        );
    }

    #[test]
    fn session_restore_detects_existing_session_file() {
        let dir = TempDir::new().unwrap();
        let project_dir = "/home/dev/phantom";
        let hash = project_hash(project_dir);
        let filename = format!("{hash}_1700000000.json");
        fs::write(dir.path().join(&filename), r#"{"version":1}"#).unwrap();
        assert!(
            is_session_restore_with_env(dir.path(), project_dir, None),
            "session file for this project must trigger restore detection"
        );
    }

    #[test]
    fn session_restore_ignores_other_project_session_files() {
        let dir = TempDir::new().unwrap();
        let other_dir = "/home/dev/other-project";
        let hash = project_hash(other_dir);
        let filename = format!("{hash}_1700000000.json");
        fs::write(dir.path().join(&filename), r#"{"version":1}"#).unwrap();
        assert!(
            !is_session_restore_with_env(dir.path(), "/home/dev/phantom", None),
            "session file for another project must not trigger restore for the current project"
        );
    }

    #[test]
    fn session_restore_matches_only_own_project() {
        let dir = TempDir::new().unwrap();
        let our_dir = "/home/dev/phantom";
        let other_dir = "/home/dev/other";
        let our_hash = project_hash(our_dir);
        let other_hash = project_hash(other_dir);
        fs::write(
            dir.path().join(format!("{our_hash}_1700000001.json")),
            r#"{"version":1}"#,
        )
        .unwrap();
        fs::write(
            dir.path().join(format!("{other_hash}_1700000002.json")),
            r#"{"version":1}"#,
        )
        .unwrap();
        assert!(is_session_restore_with_env(dir.path(), our_dir, None));
        assert!(is_session_restore_with_env(dir.path(), other_dir, None));
    }

    #[test]
    fn session_restore_env_var_overrides() {
        let dir = TempDir::new().unwrap();
        assert!(
            is_session_restore_with_env(dir.path(), "/home/dev/phantom", Some("1")),
            "PHANTOM_RESTORING=1 must signal restore even with an empty session directory"
        );
    }

    #[test]
    fn session_restore_env_var_must_be_exactly_one() {
        let dir = TempDir::new().unwrap();
        assert!(
            !is_session_restore_with_env(dir.path(), "/home/dev/phantom", Some("true")),
            "PHANTOM_RESTORING=true must not trigger restore"
        );
        assert!(
            !is_session_restore_with_env(dir.path(), "/home/dev/phantom", Some("")),
            "PHANTOM_RESTORING= must not trigger restore"
        );
    }

    #[test]
    fn session_restore_missing_directory_returns_false() {
        let nonexistent =
            std::path::Path::new("/tmp/phantom-sessions-nonexistent-a3f9b2c1d4e5f607");
        assert!(
            !is_session_restore_with_env(nonexistent, "/home/dev/phantom", None),
            "missing session dir must return false without panicking"
        );
    }

    // =======================================================================
    // Issue #171 — Session restore: crash & restart recovers pane layout
    // =======================================================================

    fn pane(title: &str, focused: bool, split: Option<SplitDirection>) -> PaneState {
        PaneState {
            working_dir: "/home/dev/phantom".into(),
            is_focused: focused,
            cols: 120,
            rows: 40,
            title: title.into(),
            split,
        }
    }

    #[test]
    fn restore_preserves_pane_count() {
        let (mgr, _dir) = test_manager();
        let project_dir = "/home/dev/phantom";
        let mut state = sample_state(project_dir, "phantom", 1_700_001_000);
        state.panes = vec![
            pane("editor", true, None),
            pane("tests", false, Some(SplitDirection::Vertical)),
            pane("logs", false, Some(SplitDirection::Horizontal)),
        ];
        mgr.save(&state).unwrap();
        let restored = mgr.load_latest(project_dir).unwrap().unwrap();
        assert_eq!(restored.panes.len(), 3, "pane count must survive save/restore");
    }

    #[test]
    fn restore_preserves_pane_titles_as_ids() {
        let (mgr, _dir) = test_manager();
        let project_dir = "/home/dev/phantom";
        let mut state = sample_state(project_dir, "phantom", 1_700_001_001);
        state.panes = vec![
            pane("alpha", true, None),
            pane("beta", false, Some(SplitDirection::Vertical)),
            pane("gamma", false, Some(SplitDirection::Horizontal)),
        ];
        mgr.save(&state).unwrap();
        let restored = mgr.load_latest(project_dir).unwrap().unwrap();
        assert_eq!(restored.panes[0].title, "alpha");
        assert_eq!(restored.panes[1].title, "beta");
        assert_eq!(restored.panes[2].title, "gamma");
    }

    #[test]
    fn restore_preserves_focus_state() {
        let (mgr, _dir) = test_manager();
        let project_dir = "/home/dev/phantom";
        let mut state = sample_state(project_dir, "phantom", 1_700_001_002);
        state.panes = vec![
            pane("editor", false, None),
            pane("tests", true, Some(SplitDirection::Vertical)),
            pane("logs", false, Some(SplitDirection::Horizontal)),
        ];
        mgr.save(&state).unwrap();
        let restored = mgr.load_latest(project_dir).unwrap().unwrap();
        let focused_count = restored.panes.iter().filter(|p| p.is_focused).count();
        assert_eq!(focused_count, 1, "exactly one pane must be focused after restore");
        assert!(restored.panes[1].is_focused, "pane 'tests' (index 1) must retain focus");
        assert!(!restored.panes[0].is_focused, "pane 'editor' must not be focused");
        assert!(!restored.panes[2].is_focused, "pane 'logs' must not be focused");
    }

    #[test]
    fn restore_preserves_split_directions() {
        let (mgr, _dir) = test_manager();
        let project_dir = "/home/dev/phantom";
        let mut state = sample_state(project_dir, "phantom", 1_700_001_003);
        state.panes = vec![
            pane("root", true, None),
            pane("right", false, Some(SplitDirection::Vertical)),
            pane("bottom", false, Some(SplitDirection::Horizontal)),
        ];
        mgr.save(&state).unwrap();
        let restored = mgr.load_latest(project_dir).unwrap().unwrap();
        assert_eq!(restored.panes[0].split, None);
        assert_eq!(restored.panes[1].split, Some(SplitDirection::Vertical));
        assert_eq!(restored.panes[2].split, Some(SplitDirection::Horizontal));
    }

    #[test]
    fn restore_preserves_pane_terminal_size() {
        let (mgr, _dir) = test_manager();
        let project_dir = "/home/dev/phantom";
        let mut state = sample_state(project_dir, "phantom", 1_700_001_004);
        state.panes = vec![
            PaneState { working_dir: project_dir.into(), is_focused: true, cols: 220, rows: 55, title: "wide".into(), split: None },
            PaneState { working_dir: project_dir.into(), is_focused: false, cols: 80, rows: 24, title: "narrow".into(), split: Some(SplitDirection::Vertical) },
        ];
        mgr.save(&state).unwrap();
        let restored = mgr.load_latest(project_dir).unwrap().unwrap();
        assert_eq!(restored.panes[0].cols, 220);
        assert_eq!(restored.panes[0].rows, 55);
        assert_eq!(restored.panes[1].cols, 80);
        assert_eq!(restored.panes[1].rows, 24);
    }

    #[test]
    fn crash_and_restart_recovers_last_saved_layout() {
        let (mgr, _dir) = test_manager();
        let project_dir = "/home/dev/crash-test";
        let mut before_crash = sample_state(project_dir, "crash-test", 1_700_002_000);
        before_crash.panes = vec![
            pane("main", true, None),
            pane("build", false, Some(SplitDirection::Vertical)),
        ];
        mgr.save(&before_crash).unwrap();
        let recovered = mgr.load_latest(project_dir).unwrap().unwrap();
        assert_eq!(recovered.panes.len(), 2, "crash recovery must restore both panes");
        assert_eq!(recovered.panes[0].title, "main");
        assert_eq!(recovered.panes[1].title, "build");
        assert!(recovered.panes[0].is_focused, "'main' pane must be focused after recovery");
    }
}
