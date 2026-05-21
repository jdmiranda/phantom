//! Config-file watcher — hot-reloads `settings.toml` whenever it changes on
//! disk.
//!
//! Follows the same pattern as [`phantom_renderer::shader_loader::ShaderReloader`]:
//! a background `notify` watcher posts a unit signal onto an `mpsc` channel;
//! [`ConfigWatcher::drain_changes`] is called once per frame from `App::update`
//! and returns `true` when at least one change landed since the last poll.
//!
//! The watcher is created with [`ConfigWatcher::new`].  If `notify` setup fails
//! (permissions, unsupported OS path, etc.) `None` is returned and the app
//! continues without live-reload — config changes take effect on the next
//! restart.

use std::path::Path;
use std::sync::mpsc;

use notify::{RecommendedWatcher, RecursiveMode, Watcher, recommended_watcher};

/// Background settings-file watcher.
///
/// Created with [`ConfigWatcher::new`]. The internal watcher thread exits
/// automatically when this struct is dropped (the `_watcher` field is dropped,
/// which signals the background thread to stop).
pub struct ConfigWatcher {
    /// Receives a `()` unit whenever the watched file changes.
    rx: mpsc::Receiver<()>,
    /// Keeps the watcher alive — dropping stops the OS watch.
    _watcher: RecommendedWatcher,
}

impl ConfigWatcher {
    /// Start watching `config_path` for changes.
    ///
    /// Returns `None` when `notify` setup fails so callers can degrade
    /// gracefully to "no live reload" without a panic.
    pub fn new(config_path: &Path) -> Option<Self> {
        let (tx, rx) = mpsc::channel::<()>();

        let mut watcher = recommended_watcher(move |_result| {
            // Ignore the event detail — all that matters is "something changed".
            let _ = tx.send(());
        })
        .ok()?;

        watcher
            .watch(config_path, RecursiveMode::NonRecursive)
            .ok()?;

        log::info!(
            "config_watcher: watching {:?} for changes",
            config_path
        );

        Some(Self {
            rx,
            _watcher: watcher,
        })
    }

    /// Drain all pending change notifications and return whether any arrived.
    ///
    /// Call this once per frame from `App::update`. When it returns `true` the
    /// caller should reload settings from disk and apply them.
    pub fn drain_changes(&self) -> bool {
        let mut changed = false;
        while self.rx.try_recv().is_ok() {
            changed = true;
        }
        changed
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// A freshly-created `ConfigWatcher` that has not seen any changes must
    /// return `false` on the first `drain_changes()` call.
    #[test]
    fn drain_changes_returns_false_when_no_change() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");
        std::fs::write(&path, b"# phantom settings\n").unwrap();

        let watcher = ConfigWatcher::new(&path).expect("watcher must start");
        // No file-system event has occurred yet.
        assert!(
            !watcher.drain_changes(),
            "drain_changes must return false before any file-system event"
        );
    }

    /// Writing to the watched file must cause `drain_changes()` to return `true`
    /// after the OS delivers the event.
    #[test]
    fn config_watcher_detects_file_change() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");
        std::fs::write(&path, b"theme = \"phosphor\"\n").unwrap();

        let watcher = ConfigWatcher::new(&path).expect("watcher must start");

        // Modify the file — this should trigger a notify event.
        {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(&path)
                .expect("open settings file");
            f.write_all(b"theme = \"amber\"\n").expect("write settings");
            f.flush().expect("flush");
        }

        // Give the OS watcher time to deliver the event (notify is async).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut detected = false;
        while std::time::Instant::now() < deadline {
            if watcher.drain_changes() {
                detected = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        assert!(
            detected,
            "config_watcher must detect a file change within 3 seconds"
        );
    }

    /// `drain_changes` must be idempotent: calling it a second time after it
    /// returned `true` must return `false` (events are consumed).
    #[test]
    fn drain_changes_consumes_events() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");
        std::fs::write(&path, b"theme = \"phosphor\"\n").unwrap();

        let watcher = ConfigWatcher::new(&path).expect("watcher must start");

        // Modify the file.
        std::fs::write(&path, b"theme = \"ice\"\n").unwrap();

        // Wait for the event.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            if watcher.drain_changes() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        // A second drain immediately after must return false.
        assert!(
            !watcher.drain_changes(),
            "drain_changes must return false when no new events have arrived"
        );
    }
}
