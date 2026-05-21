//! Process-global directory of running loops.
//!
//! [`LoopRegistry`] is the in-memory list of every [`crate::LoopRunner`] the
//! CLI subcommand has spawned. It owns each runner's tokio
//! [`tokio::task::JoinHandle`] so `phantom loop stop` can abort one by id
//! and `phantom loop status` can snapshot every running loop's last-known
//! state.
//!
//! # Why a separate type rather than just a `HashMap<LoopId, JoinHandle>`
//!
//! Three reasons:
//!
//! 1. **Status snapshots.** The runner's [`crate::LoopState`] lives behind a
//!    `&mut runner` and is not safe to share. The registry instead holds a
//!    shared `Arc<Mutex<LoopStatus>>` that the runner task updates on every
//!    transition; the registry's `list()` reads it without contending with
//!    the runner.
//! 2. **Friendly ids.** [`crate::LoopId`] is the durable handle used in
//!    `phantom loop stop --loop <id>` and on log lines, but the runner only
//!    sees the user-chosen spec id (`"reviewer"`, `"pr-finder"`). The
//!    registry bridges the two.
//! 3. **Abort discipline.** Tokio `JoinHandle::abort` is fire-and-forget;
//!    we want the registry to drop the entry only after `join` confirms the
//!    task has actually exited. Embedding that lifecycle as an explicit
//!    method on the registry keeps the CLI side trivial.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use tokio::task::JoinHandle;

use crate::id::LoopId;

// ---------------------------------------------------------------------------
// LoopStatus
// ---------------------------------------------------------------------------

/// A coarse-grained mirror of [`crate::LoopState`] suitable for sharing across
/// threads via `Arc<Mutex<_>>`.
///
/// The runner FSM's [`crate::LoopState`] cannot be `Clone` because it owns
/// the in-flight [`crate::LoopInput`]. [`LoopStatus`] strips the input field
/// and keeps only the discriminant + the terminal stop reason, which is all
/// `phantom loop status` ever needs to display.
#[derive(Debug, Clone)]
pub enum LoopStatus {
    /// Fresh runner, has not yet pulled.
    Idle,
    /// Polling the source.
    Pulling,
    /// Holding an input, about to dispatch.
    Dispatching,
    /// Dispatched; waiting for the agent's `complete_task` result.
    Awaiting,
    /// Got a result; validating against the schema.
    Validating,
    /// Terminal. Carries the stop reason for the CLI to display.
    Stopped { reason: String },
}

impl LoopStatus {
    /// `true` if this status is terminal.
    #[must_use]
    pub fn is_stopped(&self) -> bool {
        matches!(self, Self::Stopped { .. })
    }
}

// ---------------------------------------------------------------------------
// LoopHandle
// ---------------------------------------------------------------------------

/// One registered runner — the tuple of (spec id, tokio task handle, shared
/// status mirror).
///
/// The CLI's `list`, `status`, and `stop` subcommands all operate on these.
/// The `JoinHandle` is owned by the registry so that `stop()` can `abort()`
/// it and then drop the entry; callers should never hold a long-lived
/// reference to it.
#[derive(Debug)]
pub struct LoopHandle {
    /// The user-chosen `id` from [`crate::LoopSpec::id`] (e.g. `"reviewer"`).
    pub spec_id: String,
    /// Last-known status, updated by the runner task on every transition via
    /// [`LoopRegistry::install_status_writer`].
    pub status: Arc<Mutex<LoopStatus>>,
    /// Wall-clock time at registration. Used by `phantom loop status` to
    /// print uptime.
    pub started_at: SystemTime,
    /// Tokio task handle. `None` after `stop()` has aborted it so a
    /// subsequent `stop()` is a no-op.
    pub join_handle: Option<JoinHandle<()>>,
}

// ---------------------------------------------------------------------------
// LoopRegistry
// ---------------------------------------------------------------------------

/// In-memory directory of running loops.
///
/// `Arc<LoopRegistry>` is the canonical sharing shape — the CLI subcommand
/// constructs one at startup, clones the `Arc` into each runner task's
/// status-writer closure, and keeps the original for the `list` / `status` /
/// `stop` command handlers.
#[derive(Debug, Default)]
pub struct LoopRegistry {
    loops: Mutex<HashMap<LoopId, LoopHandle>>,
    /// Monotonic id allocator for fresh [`LoopId`]s.
    next_id: AtomicU64,
}

impl LoopRegistry {
    /// Construct an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            loops: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        }
    }

    /// Allocate the next monotonic [`LoopId`].
    pub fn alloc_id(&self) -> LoopId {
        LoopId(self.next_id.fetch_add(1, Ordering::SeqCst))
    }

    /// Register `handle` under `id`. Replaces any prior entry with the same id.
    pub fn register(&self, id: LoopId, handle: LoopHandle) {
        let mut map = match self.loops.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        map.insert(id, handle);
    }

    /// Snapshot every registered loop's (id, spec_id, status, started_at).
    ///
    /// Returns the data in a form `phantom loop status` can print directly
    /// without holding the registry lock across the formatting step.
    #[must_use]
    pub fn list(&self) -> Vec<LoopSnapshot> {
        let map = match self.loops.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        map.iter()
            .map(|(id, h)| LoopSnapshot {
                id: *id,
                spec_id: h.spec_id.clone(),
                status: h
                    .status
                    .lock()
                    .map(|s| s.clone())
                    .unwrap_or(LoopStatus::Idle),
                started_at: h.started_at,
                is_aborted: h.join_handle.is_none(),
            })
            .collect()
    }

    /// Look up one loop by id. Returns the spec id + last-known status.
    #[must_use]
    pub fn get(&self, id: LoopId) -> Option<LoopSnapshot> {
        let map = match self.loops.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        map.get(&id).map(|h| LoopSnapshot {
            id,
            spec_id: h.spec_id.clone(),
            status: h
                .status
                .lock()
                .map(|s| s.clone())
                .unwrap_or(LoopStatus::Idle),
            started_at: h.started_at,
            is_aborted: h.join_handle.is_none(),
        })
    }

    /// Find one loop by spec id. Returns the first match (the registry
    /// permits duplicate spec ids; the CLI does not enforce uniqueness).
    #[must_use]
    pub fn find_by_spec_id(&self, spec_id: &str) -> Option<LoopId> {
        let map = match self.loops.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        map.iter()
            .find(|(_, h)| h.spec_id == spec_id)
            .map(|(id, _)| *id)
    }

    /// Abort the loop's tokio task. Returns `Err` if the id is unknown.
    ///
    /// The registry keeps the [`LoopHandle`] entry so a follow-up `status`
    /// call still surfaces the stop. Callers that want full removal should
    /// call [`Self::unregister`] afterwards.
    pub fn stop(&self, id: LoopId) -> Result<(), LoopRegistryError> {
        let mut map = match self.loops.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let Some(handle) = map.get_mut(&id) else {
            return Err(LoopRegistryError::UnknownLoop { id });
        };
        if let Some(jh) = handle.join_handle.take() {
            jh.abort();
            if let Ok(mut status) = handle.status.lock() {
                *status = LoopStatus::Stopped {
                    reason: "aborted via phantom loop stop".to_string(),
                };
            }
        }
        Ok(())
    }

    /// Remove an entry from the registry. Returns `Err` if the id is unknown.
    pub fn unregister(&self, id: LoopId) -> Result<(), LoopRegistryError> {
        let mut map = match self.loops.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if map.remove(&id).is_some() {
            Ok(())
        } else {
            Err(LoopRegistryError::UnknownLoop { id })
        }
    }

    /// Number of registered loops (including stopped ones still in the map).
    #[must_use]
    pub fn len(&self) -> usize {
        let map = match self.loops.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        map.len()
    }

    /// `true` when no loops are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

// ---------------------------------------------------------------------------
// LoopSnapshot — value-only view returned from `list` / `get`
// ---------------------------------------------------------------------------

/// One row in a [`LoopRegistry::list`] snapshot.
#[derive(Debug, Clone)]
pub struct LoopSnapshot {
    pub id: LoopId,
    pub spec_id: String,
    pub status: LoopStatus,
    pub started_at: SystemTime,
    /// `true` if the registry has already aborted the task (the join handle
    /// has been taken). The runner may still be in the middle of unwinding.
    pub is_aborted: bool,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors the registry can produce.
#[derive(Debug, thiserror::Error)]
pub enum LoopRegistryError {
    #[error("no loop registered under id {id}")]
    UnknownLoop { id: LoopId },
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal handle with a never-completing task for unit testing.
    fn dummy_handle(spec_id: &str) -> LoopHandle {
        // A task that just sleeps forever — abort() must work without panic.
        let jh: JoinHandle<()> = tokio::spawn(async {
            std::future::pending::<()>().await;
        });
        LoopHandle {
            spec_id: spec_id.to_owned(),
            status: Arc::new(Mutex::new(LoopStatus::Idle)),
            started_at: SystemTime::now(),
            join_handle: Some(jh),
        }
    }

    #[tokio::test]
    async fn alloc_id_returns_monotonic_ids() {
        let reg = LoopRegistry::new();
        let a = reg.alloc_id();
        let b = reg.alloc_id();
        assert_ne!(a, b);
        assert!(b.0 > a.0);
    }

    #[tokio::test]
    async fn register_then_list_finds_the_entry() {
        let reg = LoopRegistry::new();
        let id = reg.alloc_id();
        reg.register(id, dummy_handle("reviewer"));
        let snaps = reg.list();
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].spec_id, "reviewer");
        assert!(matches!(snaps[0].status, LoopStatus::Idle));
    }

    #[tokio::test]
    async fn stop_then_status_reports_aborted_reason() {
        let reg = LoopRegistry::new();
        let id = reg.alloc_id();
        reg.register(id, dummy_handle("reviewer"));
        reg.stop(id).expect("stop should succeed");
        let snap = reg.get(id).expect("entry still present after stop");
        match snap.status {
            LoopStatus::Stopped { reason } => assert!(reason.contains("aborted")),
            other => panic!("expected Stopped, got {other:?}"),
        }
        assert!(snap.is_aborted);
    }

    #[tokio::test]
    async fn unregister_removes_the_entry() {
        let reg = LoopRegistry::new();
        let id = reg.alloc_id();
        reg.register(id, dummy_handle("x"));
        assert_eq!(reg.len(), 1);
        reg.unregister(id).expect("unregister should succeed");
        assert_eq!(reg.len(), 0);
    }

    #[tokio::test]
    async fn unknown_id_returns_unknown_error() {
        let reg = LoopRegistry::new();
        let err = reg.stop(LoopId(9999)).expect_err("should fail");
        assert!(matches!(err, LoopRegistryError::UnknownLoop { .. }));
    }

    #[tokio::test]
    async fn find_by_spec_id_returns_the_first_match() {
        let reg = LoopRegistry::new();
        let id1 = reg.alloc_id();
        let id2 = reg.alloc_id();
        reg.register(id1, dummy_handle("reviewer"));
        reg.register(id2, dummy_handle("implementer"));
        let found = reg.find_by_spec_id("implementer").expect("must find");
        assert_eq!(found, id2);
        assert!(reg.find_by_spec_id("nope").is_none());
    }
}
