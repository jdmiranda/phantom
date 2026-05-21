//! Process-wide registry of running fleet apps.
//!
//! Distinct from [`phantom_adapter::AppRegistry`]: that registry stores
//! `Box<dyn AppAdapter>` and is owned by the GUI app for pane management.
//! The fleet's registry tracks **headless** apps spawned as tokio tasks,
//! holding the `JoinHandle` plus a cheap status snapshot for `phantom fleet
//! list` output.
//!
//! The shared `LoopQueueRegistry`, brain handle, and substrate dispatcher all
//! live elsewhere (in [`crate::run::FleetRunner`]). This module is just the
//! handle-storage side of the orchestrator.

use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use tokio::task::JoinHandle;

/// Lifecycle status of a hosted fleet app.
#[derive(Debug, Clone)]
pub enum FleetAppStatus {
    /// The app's lifecycle task is spawned and running.
    Running,

    /// The app exited cleanly with the carried reason.
    Stopped(String),

    /// The app encountered a fatal error during boot or lifecycle.
    Errored(String),
}

/// Opaque numeric handle assigned by [`FleetRegistry::alloc_id`].
pub type FleetAppId = u32;

/// One entry per hosted app. Stored in [`FleetRegistry`].
pub struct FleetAppHandle {
    /// User-visible identifier (e.g. `"builder:jdmiranda/phantom"`).
    pub label: String,

    /// Coarse status snapshot. Hot-path code (the lifecycle task) writes
    /// here; `phantom fleet list` reads here.
    pub status: Arc<Mutex<FleetAppStatus>>,

    /// When this app was registered. Useful for the `list` subcommand.
    pub registered_at: SystemTime,

    /// The tokio task driving this app. Aborted on shutdown.
    pub join_handle: Option<JoinHandle<()>>,
}

/// Process-wide directory of hosted fleet apps.
///
/// Hands out monotonic [`FleetAppId`]s and stores [`FleetAppHandle`]s under
/// a `Mutex` because the multi-threaded runtime needs `Send + Sync` access
/// from both the fleet's main task (registration) and the Ctrl-C shutdown
/// path (iteration + abort).
pub struct FleetRegistry {
    inner: Mutex<FleetRegistryInner>,
}

struct FleetRegistryInner {
    next_id: FleetAppId,
    entries: Vec<(FleetAppId, FleetAppHandle)>,
}

impl FleetRegistry {
    /// Construct an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(FleetRegistryInner {
                next_id: 1,
                entries: Vec::new(),
            }),
        }
    }

    /// Allocate a fresh monotonic id without registering anything yet.
    /// Useful when callers want to thread an id into a status `Arc` *before*
    /// they finish building the join handle.
    pub fn alloc_id(&self) -> FleetAppId {
        let mut g = self.lock();
        let id = g.next_id;
        g.next_id += 1;
        id
    }

    /// Insert an entry under `id`. Overwrites any existing entry with the
    /// same id (callers should always pair `alloc_id` with one `register`).
    pub fn register(&self, id: FleetAppId, handle: FleetAppHandle) {
        let mut g = self.lock();
        g.entries.retain(|(existing, _)| *existing != id);
        g.entries.push((id, handle));
    }

    /// Snapshot every entry's (id, label, status). Order is registration
    /// order. Used by `phantom fleet list` (running case).
    #[must_use]
    pub fn snapshot(&self) -> Vec<FleetAppSnapshot> {
        let g = self.lock();
        g.entries
            .iter()
            .map(|(id, h)| FleetAppSnapshot {
                id: *id,
                label: h.label.clone(),
                status: h
                    .status
                    .lock()
                    .map(|s| s.clone())
                    .unwrap_or(FleetAppStatus::Errored("status mutex poisoned".to_string())),
            })
            .collect()
    }

    /// Abort every registered tokio task and clear the registry. Called on
    /// Ctrl-C. After this returns the registry is empty; further registers
    /// would race against the runtime drop, which the caller avoids.
    pub fn abort_all(&self) {
        let mut g = self.lock();
        for (_, handle) in g.entries.iter_mut() {
            if let Some(jh) = handle.join_handle.take() {
                jh.abort();
            }
        }
        g.entries.clear();
    }

    /// Number of currently-registered entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().entries.len()
    }

    /// Whether the registry has zero entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FleetRegistryInner> {
        match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

impl Default for FleetRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Read-only view of a registered app.
#[derive(Debug, Clone)]
pub struct FleetAppSnapshot {
    /// Unique id within this registry.
    pub id: FleetAppId,
    /// Human-readable label.
    pub label: String,
    /// Current status snapshot.
    pub status: FleetAppStatus,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_id_is_monotonic_starting_from_one() {
        let r = FleetRegistry::new();
        assert_eq!(r.alloc_id(), 1);
        assert_eq!(r.alloc_id(), 2);
        assert_eq!(r.alloc_id(), 3);
    }

    #[test]
    fn register_and_snapshot_round_trip() {
        let r = FleetRegistry::new();
        let id = r.alloc_id();
        let status = Arc::new(Mutex::new(FleetAppStatus::Running));
        r.register(
            id,
            FleetAppHandle {
                label: "test".to_string(),
                status: Arc::clone(&status),
                registered_at: SystemTime::now(),
                join_handle: None,
            },
        );
        let snaps = r.snapshot();
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].id, id);
        assert_eq!(snaps[0].label, "test");
        assert!(matches!(snaps[0].status, FleetAppStatus::Running));
    }

    #[test]
    fn abort_all_clears_entries() {
        let r = FleetRegistry::new();
        let id = r.alloc_id();
        let status = Arc::new(Mutex::new(FleetAppStatus::Running));
        r.register(
            id,
            FleetAppHandle {
                label: "x".to_string(),
                status,
                registered_at: SystemTime::now(),
                join_handle: None,
            },
        );
        assert!(!r.is_empty());
        r.abort_all();
        assert!(r.is_empty());
    }

    #[test]
    fn snapshot_reflects_status_updates() {
        let r = FleetRegistry::new();
        let id = r.alloc_id();
        let status = Arc::new(Mutex::new(FleetAppStatus::Running));
        r.register(
            id,
            FleetAppHandle {
                label: "z".to_string(),
                status: Arc::clone(&status),
                registered_at: SystemTime::now(),
                join_handle: None,
            },
        );
        // Flip the status from outside.
        *status.lock().unwrap() = FleetAppStatus::Stopped("ok".to_string());
        let snaps = r.snapshot();
        match &snaps[0].status {
            FleetAppStatus::Stopped(m) => assert_eq!(m, "ok"),
            other => panic!("expected stopped, got {other:?}"),
        }
    }
}
