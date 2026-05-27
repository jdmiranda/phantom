//! Background filesystem watcher for the `FilesWatchAdapter` pane.
//!
//! Wraps `notify::RecommendedWatcher` against a project root directory and
//! emits `FileChangeEvent`s on a channel. The receiver drains the channel
//! each frame from `App::update` and routes events into the pane via
//! `accept_command "push"`.
//!
//! Dropping the watcher stops the background watch thread (notify's
//! internal contract). The App holds an `Option<FilesWatcher>` that is
//! `Some` only while the pane is spawned.

use std::path::Path;
use std::sync::mpsc;

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher, recommended_watcher};

/// One filesystem change event projected into the adapter's vocabulary.
#[derive(Debug, Clone)]
pub struct FileChangeEvent {
    /// Path that changed, relative to the project root if possible,
    /// otherwise absolute.
    pub path: String,
    /// `"M"` for modify, `"+"` for create, `"D"` for delete. Falls back to
    /// `"M"` for ambiguous events.
    pub kind: &'static str,
}

/// Background filesystem watcher.
pub struct FilesWatcher {
    rx: mpsc::Receiver<FileChangeEvent>,
    root: std::path::PathBuf,
    _watcher: RecommendedWatcher,
}

impl FilesWatcher {
    /// Start watching `root` recursively. Returns `None` when notify setup
    /// fails so callers can degrade gracefully.
    pub fn new(root: &Path) -> Option<Self> {
        let (tx, rx) = mpsc::channel::<FileChangeEvent>();
        let root_owned = root.to_path_buf();
        let root_for_cb = root_owned.clone();

        let mut watcher = recommended_watcher(move |result: notify::Result<notify::Event>| {
            let Ok(event) = result else { return };
            let kind = match event.kind {
                EventKind::Create(_) => "+",
                EventKind::Remove(_) => "D",
                _ => "M",
            };
            for path in event.paths {
                // Skip noisy directories (target/, node_modules/, .git/).
                let p = path.to_string_lossy();
                if p.contains("/target/")
                    || p.contains("/node_modules/")
                    || p.contains("/.git/")
                    || p.contains("/.cargo/")
                {
                    continue;
                }
                let relative = path
                    .strip_prefix(&root_for_cb)
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|_| p.into_owned());
                let _ = tx.send(FileChangeEvent {
                    path: relative,
                    kind,
                });
            }
        })
        .ok()?;

        watcher.watch(root, RecursiveMode::Recursive).ok()?;

        log::info!("files_watcher: watching {root:?} recursively");

        Some(Self {
            rx,
            root: root_owned,
            _watcher: watcher,
        })
    }

    /// Drain all pending events; returns at most `limit` to bound work.
    pub fn drain(&self, limit: usize) -> Vec<FileChangeEvent> {
        let mut out = Vec::new();
        while out.len() < limit {
            match self.rx.try_recv() {
                Ok(ev) => out.push(ev),
                Err(_) => break,
            }
        }
        out
    }

    /// Root path being watched.
    #[allow(dead_code)]
    pub fn root(&self) -> &Path {
        &self.root
    }
}
