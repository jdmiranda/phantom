//! Durable notification store for the agent runtime.
//!
//! Every significant lifecycle transition — agent spawn, pipeline completion,
//! denial detected — is recorded as a [`Notification`] that persists across
//! process restarts.  Notifications are **not** ephemeral toasts; they are
//! queryable, mark-as-read records backed by an append-write JSON file.
//!
//! # Storage format
//!
//! Notifications are stored as a JSON array in
//! `~/.config/phantom/notifications/{project_hash}.json`.  Writes are atomic
//! (write-then-rename).
//!
//! # Example
//!
//! ```rust,no_run
//! use phantom_memory::notifications::{NotificationStore, NotificationKind};
//! use std::path::Path;
//!
//! let mut store = NotificationStore::open_in("/my/project", Path::new("/tmp/notif")).unwrap();
//! store.push(NotificationKind::AgentRunning, "Agent started", "Agent #1 is running", None).unwrap();
//! let unread = store.unread();
//! assert_eq!(unread.len(), 1);
//! ```

use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Opaque identifier for a notification.
///
/// Assigned monotonically by [`NotificationStore::push`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct NotificationId(u64);

impl NotificationId {
    /// The underlying integer value.
    pub fn value(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for NotificationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "notification#{}", self.0)
    }
}

/// Semantic classification of a notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationKind {
    /// An AI plan is ready for review or execution.
    PlanReady,
    /// An agent has started and is actively running.
    AgentRunning,
    /// An agent completed and its memory/context is synced.
    AgentSynced,
    /// An agent died unexpectedly (no clean completion path).
    AgentFlatlined,
    /// A multi-step pipeline finished successfully.
    PipelineCompleted,
    /// A pipeline cannot proceed due to a blocked dependency or denial.
    PipelineBlocked,
}

impl std::fmt::Display for NotificationKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            NotificationKind::PlanReady => "plan_ready",
            NotificationKind::AgentRunning => "agent_running",
            NotificationKind::AgentSynced => "agent_synced",
            NotificationKind::AgentFlatlined => "agent_flatlined",
            NotificationKind::PipelineCompleted => "pipeline_completed",
            NotificationKind::PipelineBlocked => "pipeline_blocked",
        };
        write!(f, "{s}")
    }
}

/// A single durable notification.
///
/// All fields are private; access them through the provided getters.
/// Use [`NotificationStore::push`] to create and persist a new notification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    id: NotificationId,
    kind: NotificationKind,
    title: String,
    message: String,
    /// Optional agent that produced this notification.
    agent_id: Option<u64>,
    read: bool,
    /// Wall-clock time of creation in milliseconds since the Unix epoch.
    created_at_ms: u64,
}

impl Notification {
    /// The notification's unique, store-assigned identifier.
    pub fn id(&self) -> NotificationId {
        self.id
    }

    /// Semantic kind of this notification.
    pub fn kind(&self) -> NotificationKind {
        self.kind
    }

    /// Short human-readable title (suitable for a badge or toast header).
    pub fn title(&self) -> &str {
        &self.title
    }

    /// Full human-readable description of the event.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The agent that produced this notification, if applicable.
    pub fn agent_id(&self) -> Option<u64> {
        self.agent_id
    }

    /// Whether this notification has been marked as read.
    pub fn is_read(&self) -> bool {
        self.read
    }

    /// Creation time in milliseconds since the Unix epoch.
    pub fn created_at_ms(&self) -> u64 {
        self.created_at_ms
    }
}

/// Persistent notification store for a single project.
///
/// Backed by a JSON file at `<base_dir>/<project_hash>.json`.  All writes
/// are atomic (write-tmp-then-rename).
pub struct NotificationStore {
    notifications: Vec<Notification>,
    next_id: u64,
    path: PathBuf,
}

// ── Internal helpers ──────────────────────────────────────────────────────────

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn project_hash(project_dir: &str) -> String {
    let mut hasher = DefaultHasher::new();
    project_dir.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

// ── NotificationStore ─────────────────────────────────────────────────────────

impl NotificationStore {
    /// Open or create the notification store for a project.
    ///
    /// The backing file is located at
    /// `~/.config/phantom/notifications/<hash>.json` where `<hash>` is derived
    /// from `project_dir`.  If the file already exists it is loaded; otherwise
    /// the store starts empty.
    pub fn open(project_dir: &str) -> Result<Self> {
        let home = std::env::var("HOME").context("HOME not set")?;
        let dir = PathBuf::from(home).join(".config/phantom/notifications");
        Self::open_in(project_dir, &dir)
    }

    /// Open with an explicit base directory (useful for testing).
    pub fn open_in(project_dir: &str, base_dir: &Path) -> Result<Self> {
        fs::create_dir_all(base_dir).with_context(|| {
            format!("failed to create notification dir: {}", base_dir.display())
        })?;

        let hash = project_hash(project_dir);
        let path = base_dir.join(format!("{hash}.json"));

        let notifications: Vec<Notification> = if path.exists() {
            let data = fs::read_to_string(&path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            serde_json::from_str(&data)
                .with_context(|| format!("failed to parse {}", path.display()))?
        } else {
            Vec::new()
        };

        // Recover next_id from the highest existing id + 1.
        let next_id = notifications
            .iter()
            .map(|n| n.id.0)
            .max()
            .map(|max| max + 1)
            .unwrap_or(1);

        Ok(Self {
            notifications,
            next_id,
            path,
        })
    }

    /// Append a new notification and persist it to disk.
    ///
    /// # Arguments
    ///
    /// - `kind` — the semantic kind of this notification.
    /// - `title` — short display title (e.g. `"Agent started"`).
    /// - `message` — full description of the event.
    /// - `agent_id` — the agent that triggered the notification, if any.
    ///
    /// Returns the freshly persisted [`Notification`].
    pub fn push(
        &mut self,
        kind: NotificationKind,
        title: impl Into<String>,
        message: impl Into<String>,
        agent_id: Option<u64>,
    ) -> Result<&Notification> {
        let n = Notification {
            id: NotificationId(self.next_id),
            kind,
            title: title.into(),
            message: message.into(),
            agent_id,
            read: false,
            created_at_ms: now_unix_ms(),
        };
        self.next_id += 1;
        self.notifications.push(n);
        self.save()?;
        Ok(self.notifications.last().expect("just pushed"))
    }

    /// Mark a notification as read.
    ///
    /// Returns `true` if the notification was found and updated.
    /// If it was already read or does not exist, returns `false`.
    pub fn mark_read(&mut self, id: NotificationId) -> Result<bool> {
        let Some(n) = self.notifications.iter_mut().find(|n| n.id == id) else {
            return Ok(false);
        };
        if n.read {
            return Ok(false);
        }
        n.read = true;
        self.save()?;
        Ok(true)
    }

    /// Mark all unread notifications as read.
    ///
    /// Returns the number of notifications that were marked read.
    pub fn mark_all_read(&mut self) -> Result<usize> {
        let count = self
            .notifications
            .iter_mut()
            .filter(|n| !n.read)
            .fold(0usize, |acc, n| {
                n.read = true;
                acc + 1
            });
        if count > 0 {
            self.save()?;
        }
        Ok(count)
    }

    /// All notifications in insertion order (oldest first).
    pub fn all(&self) -> &[Notification] {
        &self.notifications
    }

    /// Unread notifications in insertion order (oldest first).
    pub fn unread(&self) -> Vec<&Notification> {
        self.notifications.iter().filter(|n| !n.read).collect()
    }

    /// Count of unread notifications.
    pub fn unread_count(&self) -> usize {
        self.notifications.iter().filter(|n| !n.read).count()
    }

    /// Notifications filtered by [`NotificationKind`], in insertion order.
    pub fn by_kind(&self, kind: NotificationKind) -> Vec<&Notification> {
        self.notifications
            .iter()
            .filter(|n| n.kind == kind)
            .collect()
    }

    /// Look up a notification by id.
    pub fn get(&self, id: NotificationId) -> Option<&Notification> {
        self.notifications.iter().find(|n| n.id == id)
    }

    /// Total number of notifications (read and unread).
    pub fn count(&self) -> usize {
        self.notifications.len()
    }

    /// Soft-delete: remove a notification by id.
    ///
    /// Returns `true` if the notification existed and was removed.
    pub fn remove(&mut self, id: NotificationId) -> Result<bool> {
        let before = self.notifications.len();
        self.notifications.retain(|n| n.id != id);
        let removed = self.notifications.len() < before;
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    // ── Private ───────────────────────────────────────────────────────────────

    /// Persist to disk atomically (write tmp, then rename).
    fn save(&self) -> Result<()> {
        let data = serde_json::to_string_pretty(&self.notifications)
            .context("failed to serialize notifications")?;
        let tmp = self.path.with_extension("json.tmp");
        fs::write(&tmp, &data)
            .with_context(|| format!("failed to write {}", tmp.display()))?;
        fs::rename(&tmp, &self.path).with_context(|| {
            format!(
                "failed to rename {} -> {}",
                tmp.display(),
                self.path.display()
            )
        })?;
        Ok(())
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_store(project: &str) -> (NotificationStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = NotificationStore::open_in(project, dir.path()).expect("open_in");
        (store, dir)
    }

    // ── push / basics ────────────────────────────────────────────────────────

    #[test]
    fn push_assigns_monotonic_ids_from_one() {
        let (mut store, _dir) = tmp_store("/proj");

        let n1 = store
            .push(NotificationKind::AgentRunning, "A", "msg", None)
            .unwrap();
        assert_eq!(n1.id().value(), 1);

        let n2 = store
            .push(NotificationKind::PlanReady, "B", "msg", None)
            .unwrap();
        assert_eq!(n2.id().value(), 2);

        let n3 = store
            .push(NotificationKind::PipelineCompleted, "C", "msg", Some(42))
            .unwrap();
        assert_eq!(n3.id().value(), 3);
    }

    #[test]
    fn pushed_notification_starts_unread() {
        let (mut store, _dir) = tmp_store("/proj");
        let n = store
            .push(NotificationKind::AgentRunning, "title", "message", None)
            .unwrap();
        assert!(!n.is_read());
    }

    #[test]
    fn all_fields_accessible_via_getters() {
        let (mut store, _dir) = tmp_store("/proj");
        let n = store
            .push(NotificationKind::AgentFlatlined, "Flatline", "Agent died", Some(7))
            .unwrap();

        assert_eq!(n.kind(), NotificationKind::AgentFlatlined);
        assert_eq!(n.title(), "Flatline");
        assert_eq!(n.message(), "Agent died");
        assert_eq!(n.agent_id(), Some(7));
        assert!(!n.is_read());
        assert!(n.created_at_ms() > 0);
    }

    // ── mark_read ────────────────────────────────────────────────────────────

    #[test]
    fn mark_read_flips_read_flag() {
        let (mut store, _dir) = tmp_store("/proj");
        store
            .push(NotificationKind::AgentRunning, "t", "m", None)
            .unwrap();
        let id = NotificationId(1);

        assert_eq!(store.unread_count(), 1);
        assert!(store.mark_read(id).unwrap());
        assert_eq!(store.unread_count(), 0);
        assert!(store.get(id).unwrap().is_read());
    }

    #[test]
    fn mark_read_already_read_returns_false() {
        let (mut store, _dir) = tmp_store("/proj");
        store
            .push(NotificationKind::PlanReady, "t", "m", None)
            .unwrap();
        let id = NotificationId(1);

        store.mark_read(id).unwrap();
        let second = store.mark_read(id).unwrap();
        assert!(!second, "already-read returns false");
    }

    #[test]
    fn mark_read_missing_id_returns_false() {
        let (mut store, _dir) = tmp_store("/proj");
        let ghost = NotificationId(999);
        assert!(!store.mark_read(ghost).unwrap());
    }

    // ── mark_all_read ────────────────────────────────────────────────────────

    #[test]
    fn mark_all_read_clears_all_unread() {
        let (mut store, _dir) = tmp_store("/proj");
        store
            .push(NotificationKind::AgentRunning, "a", "m", None)
            .unwrap();
        store
            .push(NotificationKind::PlanReady, "b", "m", None)
            .unwrap();
        store
            .push(NotificationKind::AgentFlatlined, "c", "m", None)
            .unwrap();

        assert_eq!(store.unread_count(), 3);
        let n = store.mark_all_read().unwrap();
        assert_eq!(n, 3);
        assert_eq!(store.unread_count(), 0);
    }

    #[test]
    fn mark_all_read_skips_already_read() {
        let (mut store, _dir) = tmp_store("/proj");
        store
            .push(NotificationKind::AgentRunning, "a", "m", None)
            .unwrap();
        store
            .push(NotificationKind::PlanReady, "b", "m", None)
            .unwrap();

        store.mark_read(NotificationId(1)).unwrap();
        let count = store.mark_all_read().unwrap();
        assert_eq!(count, 1, "only one was still unread");
        assert_eq!(store.unread_count(), 0);
    }

    #[test]
    fn mark_all_read_on_empty_store_returns_zero() {
        let (mut store, _dir) = tmp_store("/proj");
        assert_eq!(store.mark_all_read().unwrap(), 0);
    }

    // ── unread / unread_count ─────────────────────────────────────────────────

    #[test]
    fn unread_returns_only_unread_in_order() {
        let (mut store, _dir) = tmp_store("/proj");
        store
            .push(NotificationKind::AgentRunning, "a", "m", None)
            .unwrap();
        store
            .push(NotificationKind::PlanReady, "b", "m", None)
            .unwrap();
        store
            .push(NotificationKind::AgentSynced, "c", "m", None)
            .unwrap();

        store.mark_read(NotificationId(2)).unwrap();

        let unread = store.unread();
        assert_eq!(unread.len(), 2);
        assert_eq!(unread[0].id().value(), 1);
        assert_eq!(unread[1].id().value(), 3);
    }

    // ── by_kind ───────────────────────────────────────────────────────────────

    #[test]
    fn by_kind_filters_correctly() {
        let (mut store, _dir) = tmp_store("/proj");
        store
            .push(NotificationKind::AgentRunning, "a", "m", Some(1))
            .unwrap();
        store
            .push(NotificationKind::AgentRunning, "b", "m", Some(2))
            .unwrap();
        store
            .push(NotificationKind::PipelineBlocked, "c", "m", None)
            .unwrap();
        store
            .push(NotificationKind::PlanReady, "d", "m", None)
            .unwrap();

        let running = store.by_kind(NotificationKind::AgentRunning);
        assert_eq!(running.len(), 2);
        assert!(running.iter().all(|n| n.kind() == NotificationKind::AgentRunning));

        let blocked = store.by_kind(NotificationKind::PipelineBlocked);
        assert_eq!(blocked.len(), 1);

        let synced = store.by_kind(NotificationKind::AgentSynced);
        assert!(synced.is_empty());
    }

    // ── get ───────────────────────────────────────────────────────────────────

    #[test]
    fn get_existing_id_returns_some() {
        let (mut store, _dir) = tmp_store("/proj");
        store
            .push(NotificationKind::PlanReady, "t", "m", None)
            .unwrap();
        assert!(store.get(NotificationId(1)).is_some());
    }

    #[test]
    fn get_missing_id_returns_none() {
        let (store, _dir) = tmp_store("/proj");
        assert!(store.get(NotificationId(99)).is_none());
    }

    // ── remove ────────────────────────────────────────────────────────────────

    #[test]
    fn remove_existing_notification() {
        let (mut store, _dir) = tmp_store("/proj");
        store
            .push(NotificationKind::AgentRunning, "t", "m", None)
            .unwrap();
        assert_eq!(store.count(), 1);

        let removed = store.remove(NotificationId(1)).unwrap();
        assert!(removed);
        assert_eq!(store.count(), 0);
        assert!(store.get(NotificationId(1)).is_none());
    }

    #[test]
    fn remove_missing_returns_false() {
        let (mut store, _dir) = tmp_store("/proj");
        assert!(!store.remove(NotificationId(99)).unwrap());
    }

    // ── all / count ───────────────────────────────────────────────────────────

    #[test]
    fn all_returns_every_notification_in_insertion_order() {
        let (mut store, _dir) = tmp_store("/proj");
        store
            .push(NotificationKind::PlanReady, "a", "m1", None)
            .unwrap();
        store
            .push(NotificationKind::AgentRunning, "b", "m2", Some(1))
            .unwrap();
        store
            .push(NotificationKind::PipelineCompleted, "c", "m3", None)
            .unwrap();

        let all = store.all();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].title(), "a");
        assert_eq!(all[1].title(), "b");
        assert_eq!(all[2].title(), "c");
    }

    #[test]
    fn count_tracks_push_and_remove() {
        let (mut store, _dir) = tmp_store("/proj");
        assert_eq!(store.count(), 0);

        store
            .push(NotificationKind::PlanReady, "a", "m", None)
            .unwrap();
        assert_eq!(store.count(), 1);

        store
            .push(NotificationKind::AgentRunning, "b", "m", None)
            .unwrap();
        assert_eq!(store.count(), 2);

        store.remove(NotificationId(1)).unwrap();
        assert_eq!(store.count(), 1);
    }

    // ── persistence ───────────────────────────────────────────────────────────

    #[test]
    fn persistence_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let project = "/home/user/myproject";

        {
            let mut store = NotificationStore::open_in(project, dir.path()).unwrap();
            store
                .push(NotificationKind::PlanReady, "Plan", "Ready to execute", None)
                .unwrap();
            store
                .push(NotificationKind::AgentRunning, "Agent", "Running", Some(42))
                .unwrap();
            store.mark_read(NotificationId(1)).unwrap();
        }

        {
            let store = NotificationStore::open_in(project, dir.path()).unwrap();
            assert_eq!(store.count(), 2);

            let n1 = store.get(NotificationId(1)).unwrap();
            assert!(n1.is_read());
            assert_eq!(n1.kind(), NotificationKind::PlanReady);
            assert_eq!(n1.title(), "Plan");

            let n2 = store.get(NotificationId(2)).unwrap();
            assert!(!n2.is_read());
            assert_eq!(n2.agent_id(), Some(42));
        }
    }

    #[test]
    fn next_id_recovers_after_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let project = "/proj";

        {
            let mut store = NotificationStore::open_in(project, dir.path()).unwrap();
            store
                .push(NotificationKind::PlanReady, "a", "m", None)
                .unwrap();
            store
                .push(NotificationKind::AgentRunning, "b", "m", None)
                .unwrap();
        }

        {
            let mut store = NotificationStore::open_in(project, dir.path()).unwrap();
            // Next id should continue from 3, not restart at 1.
            let n = store
                .push(NotificationKind::AgentSynced, "c", "m", None)
                .unwrap();
            assert_eq!(n.id().value(), 3);
        }
    }

    #[test]
    fn different_projects_are_isolated() {
        let dir = tempfile::tempdir().unwrap();

        let mut store_a = NotificationStore::open_in("/project-a", dir.path()).unwrap();
        store_a
            .push(NotificationKind::PlanReady, "alpha", "m", None)
            .unwrap();

        let mut store_b = NotificationStore::open_in("/project-b", dir.path()).unwrap();
        store_b
            .push(NotificationKind::AgentRunning, "beta", "m", None)
            .unwrap();

        let store_a2 = NotificationStore::open_in("/project-a", dir.path()).unwrap();
        assert_eq!(store_a2.count(), 1);
        assert_eq!(store_a2.all()[0].title(), "alpha");

        let store_b2 = NotificationStore::open_in("/project-b", dir.path()).unwrap();
        assert_eq!(store_b2.count(), 1);
        assert_eq!(store_b2.all()[0].title(), "beta");
    }

    // ── NotificationId display ────────────────────────────────────────────────

    #[test]
    fn notification_id_display() {
        let id = NotificationId(42);
        assert_eq!(format!("{id}"), "notification#42");
    }

    // ── NotificationKind display ──────────────────────────────────────────────

    #[test]
    fn notification_kind_display_variants() {
        assert_eq!(NotificationKind::PlanReady.to_string(), "plan_ready");
        assert_eq!(NotificationKind::AgentRunning.to_string(), "agent_running");
        assert_eq!(NotificationKind::AgentSynced.to_string(), "agent_synced");
        assert_eq!(NotificationKind::AgentFlatlined.to_string(), "agent_flatlined");
        assert_eq!(
            NotificationKind::PipelineCompleted.to_string(),
            "pipeline_completed"
        );
        assert_eq!(
            NotificationKind::PipelineBlocked.to_string(),
            "pipeline_blocked"
        );
    }

    // ── all six NotificationKind variants round-trip through serde ────────────

    #[test]
    fn all_kinds_serde_round_trip() {
        let kinds = [
            NotificationKind::PlanReady,
            NotificationKind::AgentRunning,
            NotificationKind::AgentSynced,
            NotificationKind::AgentFlatlined,
            NotificationKind::PipelineCompleted,
            NotificationKind::PipelineBlocked,
        ];
        for kind in kinds {
            let json = serde_json::to_string(&kind).unwrap();
            let back: NotificationKind = serde_json::from_str(&json).unwrap();
            assert_eq!(back, kind);
        }
    }
}
