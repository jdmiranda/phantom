//! Background dylib watcher — debounced filesystem events trigger reload.
//!
//! Mirrors the shape of `phantom-renderer/src/shader_loader.rs:155-235`.
//! Active only when `hot-modules` feature + debug build.

#![cfg(all(debug_assertions, feature = "hot-modules"))]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use notify::{EventKind, RecursiveMode, Watcher, event::ModifyKind, recommended_watcher};

use crate::host::SemanticSkill;
use crate::loader::load_from_path;

/// Debounce window: ignore events closer than this to the previous one.
const DEBOUNCE: Duration = Duration::from_millis(150);

/// Event posted by the watcher thread to the main thread.
pub enum SkillReloadEvent {
    /// A new skill was loaded successfully.
    Reloaded(Arc<dyn SemanticSkill>),
    /// The dylib changed but could not be loaded.
    Error(anyhow::Error),
}

/// Background watcher for a skill dylib.
///
/// Drop to stop the watcher thread.
pub struct SkillWatcher {
    /// Events produced by the background thread.
    rx: std::sync::mpsc::Receiver<SkillReloadEvent>,
    /// Keep the watcher alive — dropping stops watching.
    _watcher: Box<dyn notify::Watcher + Send>,
}

impl SkillWatcher {
    /// Start watching `path` for changes.  Returns `None` (with a warning) if
    /// `notify` setup fails — the caller should continue with the static skill.
    pub fn start(path: PathBuf) -> Option<Self> {
        match Self::try_start(path) {
            Ok(w) => Some(w),
            Err(e) => {
                log::warn!("skill-host: watcher setup failed: {e} — hot reload disabled");
                None
            }
        }
    }

    fn try_start(path: PathBuf) -> anyhow::Result<Self> {
        let (tx, rx) = std::sync::mpsc::channel::<SkillReloadEvent>();
        let watched_dir = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("dylib path has no parent dir"))?
            .to_path_buf();

        // Track last modification time and content hash to avoid spurious
        // reloads from `cargo`'s touch-without-write footgun.
        let last_mtime: Arc<Mutex<Option<SystemTime>>> = Arc::new(Mutex::new(None));
        let last_hash: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));

        let tx_clone = tx;
        let path_clone = path.clone();
        let last_mtime_clone = Arc::clone(&last_mtime);
        let last_hash_clone = Arc::clone(&last_hash);

        let mut watcher =
            recommended_watcher(move |result: notify::Result<notify::Event>| {
                let event = match result {
                    Ok(e) => e,
                    Err(e) => {
                        log::warn!("skill-host: notify error: {e}");
                        return;
                    }
                };

                let is_modify = matches!(
                    event.kind,
                    EventKind::Modify(ModifyKind::Data(_))
                        | EventKind::Modify(ModifyKind::Any)
                        | EventKind::Create(_)
                );
                if !is_modify {
                    return;
                }

                // Only care about the specific dylib, not other files.
                let is_our_file = event.paths.iter().any(|p| p == &path_clone);
                if !is_our_file {
                    return;
                }

                // Stale-rebuild safety: check mtime.
                let mtime = std::fs::metadata(&path_clone)
                    .and_then(|m| m.modified())
                    .ok();

                {
                    let mut last = last_mtime_clone.lock().unwrap();
                    if *last == mtime && mtime.is_some() {
                        log::debug!("skill-host: mtime unchanged — skipping reload");
                        return;
                    }
                    // Debounce: if a previous event was very recent, skip.
                    if let (Some(prev), Some(curr)) = (*last, mtime) {
                        if curr.duration_since(prev).unwrap_or(DEBOUNCE) < DEBOUNCE {
                            log::debug!("skill-host: debounced reload");
                            return;
                        }
                    }
                    *last = mtime;
                }

                // Content-hash check: avoid reload if bytes haven't changed.
                if let Ok(bytes) = std::fs::read(&path_clone) {
                    let hash = simple_hash(&bytes);
                    let mut last_h = last_hash_clone.lock().unwrap();
                    if *last_h == Some(hash) {
                        log::debug!("skill-host: content hash unchanged — skipping reload");
                        return;
                    }
                    *last_h = Some(hash);
                }

                log::info!(
                    "skill-host: {:?} changed — attempting reload",
                    path_clone.file_name().unwrap_or_default()
                );

                let event = match load_from_path(&path_clone) {
                    Ok(skill) => SkillReloadEvent::Reloaded(skill),
                    Err(e) => {
                        log::warn!("skill-host: reload failed: {e}");
                        SkillReloadEvent::Error(e)
                    }
                };
                let _ = tx_clone.send(event);
            })?;

        watcher.watch(&watched_dir, RecursiveMode::NonRecursive)?;
        log::info!(
            "skill-host: watching {:?} for dylib changes (PHANTOM_HOT_MODULES=1)",
            watched_dir
        );

        Ok(Self {
            rx,
            _watcher: Box::new(watcher),
        })
    }

    /// Poll for the latest reload event without blocking.
    ///
    /// Returns `None` if no change has occurred.
    #[must_use]
    pub fn poll(&self) -> Option<SkillReloadEvent> {
        match self.rx.try_recv() {
            Ok(e) => Some(e),
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                log::warn!("skill-host: watcher channel disconnected");
                None
            }
        }
    }
}

/// Fast non-cryptographic hash used for change detection.
fn simple_hash(bytes: &[u8]) -> u64 {
    // FNV-1a 64-bit
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}
