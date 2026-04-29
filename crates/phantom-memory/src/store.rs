use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::clock::SequenceClock;

/// A single memory entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub key: String,
    pub value: String,
    pub category: MemoryCategory,
    pub created_at: u64,
    pub updated_at: u64,
    pub source: MemorySource,
    /// Monotonically-increasing sequence number assigned at insertion time.
    ///
    /// Enables total ordering of entries independent of wall-clock timestamps.
    /// Entries loaded from a pre-existing JSON file that pre-dates this field
    /// default to `0` via `serde(default)`.
    #[serde(default)]
    pub seq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemoryCategory {
    ProjectConfig,
    Convention,
    Warning,
    Context,
    UserNote,
}

impl std::fmt::Display for MemoryCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ProjectConfig => write!(f, "config"),
            Self::Convention => write!(f, "convention"),
            Self::Warning => write!(f, "warning"),
            Self::Context => write!(f, "context"),
            Self::UserNote => write!(f, "note"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemorySource {
    Auto,
    Agent,
    User,
}

/// Persistent per-project memory store.
///
/// Backed by a JSON file at `~/.config/phantom/memory/{project_hash}.json`.
///
/// Each new entry is stamped with a monotonically-increasing sequence number
/// via the embedded [`SequenceClock`], providing unambiguous insertion order
/// even if multiple entries share the same wall-clock second.
pub struct MemoryStore {
    entries: Vec<MemoryEntry>,
    path: PathBuf,
    clock: Arc<SequenceClock>,
}

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_secs()
}

fn project_hash(project_dir: &str) -> String {
    let mut hasher = DefaultHasher::new();
    project_dir.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

impl MemoryStore {
    /// Open or create memory for a project directory.
    ///
    /// The backing file lives at `~/.config/phantom/memory/{hash}.json` where hash
    /// is derived from `project_dir`. If the file already exists it is loaded;
    /// otherwise the store starts empty.
    pub fn open(project_dir: &str) -> Result<Self> {
        let home = std::env::var("HOME").context("HOME not set")?;
        let dir = PathBuf::from(home).join(".config/phantom/memory");
        Self::open_in(project_dir, &dir)
    }

    /// Open with an explicit base directory (useful for testing).
    pub fn open_in(project_dir: &str, base_dir: &std::path::Path) -> Result<Self> {
        fs::create_dir_all(base_dir)
            .with_context(|| format!("failed to create memory dir: {}", base_dir.display()))?;

        let hash = project_hash(project_dir);
        let path = base_dir.join(format!("{hash}.json"));

        let entries: Vec<MemoryEntry> = if path.exists() {
            let data = fs::read_to_string(&path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            serde_json::from_str(&data)
                .with_context(|| format!("failed to parse {}", path.display()))?
        } else {
            Vec::new()
        };

        // Seed the clock past the highest seq already on disk so that new
        // entries are always strictly greater than any persisted value.
        let max_seq = entries.iter().map(|e| e.seq).max().unwrap_or(0);
        let clock = Arc::new(SequenceClock::new());
        // Consume `max_seq` ticks so the next `clock.next()` yields max_seq + 1.
        for _ in 0..=max_seq {
            clock.next();
        }

        Ok(Self {
            entries,
            path,
            clock,
        })
    }

    /// Set a memory entry (insert or update by key).
    ///
    /// New entries are stamped with the next sequence number from the store's
    /// [`SequenceClock`].  Updates to existing entries do *not* change `seq`
    /// (the insertion order is immutable).
    pub fn set(
        &mut self,
        key: &str,
        value: &str,
        category: MemoryCategory,
        source: MemorySource,
    ) -> Result<()> {
        let now = now_epoch();

        if let Some(entry) = self.entries.iter_mut().find(|e| e.key == key) {
            entry.value = value.to_owned();
            entry.category = category;
            entry.source = source;
            entry.updated_at = now;
        } else {
            let seq = self.clock.next();
            self.entries.push(MemoryEntry {
                key: key.to_owned(),
                value: value.to_owned(),
                category,
                created_at: now,
                updated_at: now,
                source,
                seq,
            });
        }

        self.save()
    }

    /// Expose the store's [`SequenceClock`] for external callers that need to
    /// stamp their own events with the same monotonic sequence.
    #[must_use]
    pub fn clock(&self) -> &Arc<SequenceClock> {
        &self.clock
    }

    /// Get a memory entry by key.
    pub fn get(&self, key: &str) -> Option<&MemoryEntry> {
        self.entries.iter().find(|e| e.key == key)
    }

    /// Remove a memory entry. Returns `true` if the key existed.
    pub fn remove(&mut self, key: &str) -> Result<bool> {
        let before = self.entries.len();
        self.entries.retain(|e| e.key != key);
        let removed = self.entries.len() < before;
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    /// Get all entries.
    pub fn all(&self) -> &[MemoryEntry] {
        &self.entries
    }

    /// Get entries by category.
    pub fn by_category(&self, category: MemoryCategory) -> Vec<&MemoryEntry> {
        self.entries
            .iter()
            .filter(|e| e.category == category)
            .collect()
    }

    /// Search entries where `query` appears in the key or value (case-insensitive).
    pub fn search(&self, query: &str) -> Vec<&MemoryEntry> {
        let q = query.to_lowercase();
        self.entries
            .iter()
            .filter(|e| e.key.to_lowercase().contains(&q) || e.value.to_lowercase().contains(&q))
            .collect()
    }

    /// Get total entry count.
    pub fn count(&self) -> usize {
        self.entries.len()
    }

    /// Persist to disk atomically (write tmp, then rename).
    fn save(&self) -> Result<()> {
        let data = serde_json::to_string_pretty(&self.entries)
            .context("failed to serialize memory entries")?;

        let tmp = self.path.with_extension("json.tmp");
        fs::write(&tmp, &data).with_context(|| format!("failed to write {}", tmp.display()))?;
        fs::rename(&tmp, &self.path).with_context(|| {
            format!(
                "failed to rename {} -> {}",
                tmp.display(),
                self.path.display()
            )
        })?;

        Ok(())
    }

    /// Format all memories as context for an AI agent.
    ///
    /// Returns a bulleted list: `- [category] key: value`
    pub fn agent_context(&self) -> String {
        if self.entries.is_empty() {
            return String::from("No project memories stored.");
        }

        self.entries
            .iter()
            .map(|e| format!("- [{}] {}: {}", e.category, e.key, e.value))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_store(project: &str) -> (MemoryStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open_in(project, dir.path()).unwrap();
        (store, dir)
    }

    #[test]
    fn set_and_get() {
        let (mut store, _dir) = tmp_store("/home/user/project");
        store
            .set(
                "pkg_manager",
                "pnpm",
                MemoryCategory::ProjectConfig,
                MemorySource::Auto,
            )
            .unwrap();

        let entry = store.get("pkg_manager").unwrap();
        assert_eq!(entry.value, "pnpm");
        assert_eq!(entry.category, MemoryCategory::ProjectConfig);
        assert_eq!(entry.source, MemorySource::Auto);
    }

    #[test]
    fn update_existing_key() {
        let (mut store, _dir) = tmp_store("/proj");
        store
            .set(
                "port",
                "3000",
                MemoryCategory::ProjectConfig,
                MemorySource::Auto,
            )
            .unwrap();
        let original_created = store.get("port").unwrap().created_at;

        store
            .set(
                "port",
                "3001",
                MemoryCategory::ProjectConfig,
                MemorySource::User,
            )
            .unwrap();

        let entry = store.get("port").unwrap();
        assert_eq!(entry.value, "3001");
        assert_eq!(entry.source, MemorySource::User);
        assert_eq!(
            entry.created_at, original_created,
            "created_at must not change on update"
        );
        assert_eq!(store.count(), 1, "should still be one entry, not two");
    }

    #[test]
    fn get_missing_key_returns_none() {
        let (store, _dir) = tmp_store("/proj");
        assert!(store.get("nonexistent").is_none());
    }

    #[test]
    fn remove_existing_key() {
        let (mut store, _dir) = tmp_store("/proj");
        store
            .set("tmp", "val", MemoryCategory::Context, MemorySource::Agent)
            .unwrap();
        assert_eq!(store.count(), 1);

        let removed = store.remove("tmp").unwrap();
        assert!(removed);
        assert_eq!(store.count(), 0);
        assert!(store.get("tmp").is_none());
    }

    #[test]
    fn remove_missing_key_returns_false() {
        let (mut store, _dir) = tmp_store("/proj");
        let removed = store.remove("ghost").unwrap();
        assert!(!removed);
    }

    #[test]
    fn all_returns_every_entry() {
        let (mut store, _dir) = tmp_store("/proj");
        store
            .set("a", "1", MemoryCategory::Convention, MemorySource::User)
            .unwrap();
        store
            .set("b", "2", MemoryCategory::Warning, MemorySource::Agent)
            .unwrap();
        store
            .set("c", "3", MemoryCategory::UserNote, MemorySource::User)
            .unwrap();

        assert_eq!(store.all().len(), 3);
    }

    #[test]
    fn by_category_filters_correctly() {
        let (mut store, _dir) = tmp_store("/proj");
        store
            .set("a", "1", MemoryCategory::Convention, MemorySource::Auto)
            .unwrap();
        store
            .set("b", "2", MemoryCategory::Warning, MemorySource::Auto)
            .unwrap();
        store
            .set("c", "3", MemoryCategory::Convention, MemorySource::Auto)
            .unwrap();
        store
            .set("d", "4", MemoryCategory::Context, MemorySource::Auto)
            .unwrap();

        let conventions = store.by_category(MemoryCategory::Convention);
        assert_eq!(conventions.len(), 2);
        assert!(
            conventions
                .iter()
                .all(|e| e.category == MemoryCategory::Convention)
        );

        let warnings = store.by_category(MemoryCategory::Warning);
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].key, "b");

        let notes = store.by_category(MemoryCategory::UserNote);
        assert!(notes.is_empty());
    }

    #[test]
    fn search_matches_key_and_value_case_insensitive() {
        let (mut store, _dir) = tmp_store("/proj");
        store
            .set(
                "pkg_manager",
                "pnpm",
                MemoryCategory::ProjectConfig,
                MemorySource::Auto,
            )
            .unwrap();
        store
            .set(
                "dev_port",
                "3001",
                MemoryCategory::ProjectConfig,
                MemorySource::Auto,
            )
            .unwrap();
        store
            .set(
                "warning",
                "Don't touch legacy/",
                MemoryCategory::Warning,
                MemorySource::Agent,
            )
            .unwrap();

        // match on value
        let results = store.search("pnpm");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].key, "pkg_manager");

        // match on key
        let results = store.search("port");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].key, "dev_port");

        // case insensitive
        let results = store.search("LEGACY");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].key, "warning");

        // no match
        let results = store.search("zzz_nothing");
        assert!(results.is_empty());
    }

    #[test]
    fn count_tracks_correctly() {
        let (mut store, _dir) = tmp_store("/proj");
        assert_eq!(store.count(), 0);

        store
            .set("a", "1", MemoryCategory::Context, MemorySource::Auto)
            .unwrap();
        assert_eq!(store.count(), 1);

        store
            .set("b", "2", MemoryCategory::Context, MemorySource::Auto)
            .unwrap();
        assert_eq!(store.count(), 2);

        // update doesn't change count
        store
            .set("a", "updated", MemoryCategory::Context, MemorySource::Auto)
            .unwrap();
        assert_eq!(store.count(), 2);

        store.remove("a").unwrap();
        assert_eq!(store.count(), 1);
    }

    #[test]
    fn persistence_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let project = "/home/user/myproject";

        // Write data
        {
            let mut store = MemoryStore::open_in(project, dir.path()).unwrap();
            store
                .set(
                    "pkg",
                    "pnpm",
                    MemoryCategory::ProjectConfig,
                    MemorySource::Auto,
                )
                .unwrap();
            store
                .set(
                    "style",
                    "tabs",
                    MemoryCategory::Convention,
                    MemorySource::User,
                )
                .unwrap();
            store
                .set(
                    "warn",
                    "legacy is frozen",
                    MemoryCategory::Warning,
                    MemorySource::Agent,
                )
                .unwrap();
        }

        // Re-open and verify
        {
            let store = MemoryStore::open_in(project, dir.path()).unwrap();
            assert_eq!(store.count(), 3);

            let pkg = store.get("pkg").unwrap();
            assert_eq!(pkg.value, "pnpm");
            assert_eq!(pkg.category, MemoryCategory::ProjectConfig);

            let style = store.get("style").unwrap();
            assert_eq!(style.value, "tabs");
            assert_eq!(style.source, MemorySource::User);

            let warn = store.get("warn").unwrap();
            assert_eq!(warn.value, "legacy is frozen");
        }
    }

    #[test]
    fn persistence_after_remove() {
        let dir = tempfile::tempdir().unwrap();
        let project = "/proj";

        {
            let mut store = MemoryStore::open_in(project, dir.path()).unwrap();
            store
                .set("a", "1", MemoryCategory::Context, MemorySource::Auto)
                .unwrap();
            store
                .set("b", "2", MemoryCategory::Context, MemorySource::Auto)
                .unwrap();
            store.remove("a").unwrap();
        }

        {
            let store = MemoryStore::open_in(project, dir.path()).unwrap();
            assert_eq!(store.count(), 1);
            assert!(store.get("a").is_none());
            assert!(store.get("b").is_some());
        }
    }

    #[test]
    fn agent_context_empty() {
        let (store, _dir) = tmp_store("/proj");
        assert_eq!(store.agent_context(), "No project memories stored.");
    }

    #[test]
    fn agent_context_formatting() {
        let (mut store, _dir) = tmp_store("/proj");
        store
            .set(
                "pkg_manager",
                "pnpm",
                MemoryCategory::ProjectConfig,
                MemorySource::Auto,
            )
            .unwrap();
        store
            .set(
                "style",
                "snake_case",
                MemoryCategory::Convention,
                MemorySource::User,
            )
            .unwrap();
        store
            .set(
                "auth_refactor",
                "don't touch legacy/",
                MemoryCategory::Warning,
                MemorySource::Agent,
            )
            .unwrap();

        let ctx = store.agent_context();
        let lines: Vec<&str> = ctx.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "- [config] pkg_manager: pnpm");
        assert_eq!(lines[1], "- [convention] style: snake_case");
        assert_eq!(lines[2], "- [warning] auth_refactor: don't touch legacy/");
    }

    #[test]
    fn memory_block_seq_stamps_increase_on_each_add() {
        let (mut store, _dir) = tmp_store("/proj");

        store
            .set("a", "1", MemoryCategory::Context, MemorySource::Auto)
            .unwrap();
        store
            .set("b", "2", MemoryCategory::Context, MemorySource::Auto)
            .unwrap();
        store
            .set("c", "3", MemoryCategory::Context, MemorySource::Auto)
            .unwrap();

        let seq_a = store.get("a").unwrap().seq;
        let seq_b = store.get("b").unwrap().seq;
        let seq_c = store.get("c").unwrap().seq;

        assert!(
            seq_a < seq_b && seq_b < seq_c,
            "seq stamps must be strictly increasing: a={seq_a} b={seq_b} c={seq_c}"
        );
    }

    #[test]
    fn update_does_not_change_seq() {
        let (mut store, _dir) = tmp_store("/proj");

        store
            .set("x", "original", MemoryCategory::Context, MemorySource::Auto)
            .unwrap();
        let seq_before = store.get("x").unwrap().seq;

        store
            .set("x", "updated", MemoryCategory::Context, MemorySource::User)
            .unwrap();
        let seq_after = store.get("x").unwrap().seq;

        assert_eq!(
            seq_before, seq_after,
            "update must not change the seq stamp"
        );
    }

    #[test]
    fn different_projects_get_different_stores() {
        let dir = tempfile::tempdir().unwrap();

        let mut store_a = MemoryStore::open_in("/project-a", dir.path()).unwrap();
        store_a
            .set("name", "alpha", MemoryCategory::Context, MemorySource::Auto)
            .unwrap();

        let mut store_b = MemoryStore::open_in("/project-b", dir.path()).unwrap();
        store_b
            .set("name", "beta", MemoryCategory::Context, MemorySource::Auto)
            .unwrap();

        // Re-open A and confirm isolation
        let store_a2 = MemoryStore::open_in("/project-a", dir.path()).unwrap();
        assert_eq!(store_a2.get("name").unwrap().value, "alpha");

        let store_b2 = MemoryStore::open_in("/project-b", dir.path()).unwrap();
        assert_eq!(store_b2.get("name").unwrap().value, "beta");
    }
}
