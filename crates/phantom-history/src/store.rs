//! Append-only JSONL history store.
//!
//! One JSONL file lives at `~/.local/share/phantom/history/<session_id>.jsonl`.
//! Each line is a serialised [`HistoryEntry`].  The store also maintains an
//! in-memory index (`HashMap<Uuid, u64>`) that maps entry ID → byte offset so
//! that id-lookup stays O(1) without a full scan after the store is opened.
//!
//! ## Concurrency
//!
//! `HistoryStore::open_at` acquires an **advisory exclusive `flock`** on a
//! companion lock file (`<path>.lock`) via [`fs2::FileExt::lock_exclusive`].
//! The lock is advisory — it protects processes that also use `HistoryStore`
//! but does nothing against processes that write to the JSONL file directly.
//!
//! If the lock is already held by another `HistoryStore` instance,
//! `open_at` returns an `Err` immediately (non-blocking try-lock).  The
//! caller is expected to retry or use a session-specific path.
//!
//! The lock file is released automatically when the `HistoryStore` is dropped.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use fs2::FileExt as _;
use uuid::Uuid;

use crate::jsonl::HistoryEntry;

// ---------------------------------------------------------------------------
// HistoryStore
// ---------------------------------------------------------------------------

/// Append-only JSONL history store for a session.
///
/// All public methods return [`anyhow::Result`] — no `.unwrap()` in production
/// paths.
///
/// The store holds an exclusive advisory flock on `<path>.lock` for its
/// entire lifetime.  A second `HistoryStore` opened on the same path will
/// fail with a "locked" error until the first is dropped.
pub struct HistoryStore {
    /// Path to the `.jsonl` file.
    path: PathBuf,
    /// Maps entry id → byte offset of the start of its line in the file.
    index: HashMap<Uuid, u64>,
    /// Byte offset where the next append will begin.
    next_offset: u64,
    /// Holds the exclusive flock on `<path>.lock` for the store's lifetime.
    /// Dropping this field releases the lock.
    _lock_file: File,
}

impl HistoryStore {
    // -----------------------------------------------------------------------
    // Construction
    // -----------------------------------------------------------------------

    /// Open (or create) a store for `session_id` under the default data dir:
    /// `~/.local/share/phantom/history/<session_id>.jsonl`.
    pub fn open(session_id: Uuid) -> Result<Self> {
        let dir = default_data_dir();
        fs::create_dir_all(&dir)
            .with_context(|| format!("cannot create history dir: {}", dir.display()))?;
        let path = dir.join(format!("{session_id}.jsonl"));
        Self::open_at(path)
    }

    /// Open (or create) a store at an explicit path.
    ///
    /// Acquires an exclusive advisory flock on `<path>.lock` before scanning
    /// the existing file to rebuild the in-memory index.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - the lock file cannot be created or opened, or
    /// - the lock is already held by another [`HistoryStore`] (or any other
    ///   process using the same companion lock file).
    pub fn open_at(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();

        // Create the JSONL file's parent directory if it doesn't exist yet.
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).with_context(|| {
                    format!("cannot create history dir: {}", parent.display())
                })?;
            }
        }

        // Acquire an exclusive advisory lock via a companion `.lock` sidecar.
        // Using a separate sidecar avoids locking the JSONL file itself, which
        // would block concurrent *readers* that don't go through HistoryStore.
        let lock_path = lock_path_for(&path);
        let lock_file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .with_context(|| format!("cannot open lock file: {}", lock_path.display()))?;

        lock_file.try_lock_exclusive().with_context(|| {
            format!(
                "history file is locked by another process: {}",
                path.display()
            )
        })?;

        let mut store = Self {
            path,
            index: HashMap::new(),
            next_offset: 0,
            _lock_file: lock_file,
        };

        store.rebuild_index()?;
        Ok(store)
    }

    // -----------------------------------------------------------------------
    // Writes
    // -----------------------------------------------------------------------

    /// Append `entry` to the JSONL file.
    ///
    /// The in-memory index is updated so subsequent id-lookups reflect the new
    /// entry without a file rescan.
    pub fn append(&mut self, entry: &HistoryEntry) -> Result<()> {
        let line = entry.to_jsonl_line()?;

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("cannot open history file: {}", self.path.display()))?;

        let offset = self.next_offset;
        writeln!(file, "{line}")
            .with_context(|| format!("cannot write to history file: {}", self.path.display()))?;

        // line + '\n'
        self.next_offset += line.len() as u64 + 1;
        self.index.insert(entry.id(), offset);

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Reads
    // -----------------------------------------------------------------------

    /// Return the most-recent `limit` entries in chronological order
    /// (oldest first, newest last).
    pub fn recent(&self, limit: usize) -> Result<Vec<HistoryEntry>> {
        let all = self.read_all()?;
        let start = all.len().saturating_sub(limit);
        Ok(all[start..].to_vec())
    }

    /// Fetch a single entry by its UUID in O(1) (index lookup + one seek).
    pub fn get_by_id(&self, id: Uuid) -> Result<Option<HistoryEntry>> {
        let Some(&offset) = self.index.get(&id) else {
            return Ok(None);
        };

        let mut file = File::open(&self.path)
            .with_context(|| format!("cannot open history file: {}", self.path.display()))?;
        file.seek(SeekFrom::Start(offset)).context("seek failed")?;

        let mut reader = BufReader::new(file);
        let mut line = String::new();
        reader.read_line(&mut line).context("read_line failed")?;

        let entry = HistoryEntry::from_jsonl_line(line.trim())
            .context("index pointed at a corrupt line")?;
        Ok(Some(entry))
    }

    /// Return all entries for `session_id` in chronological order.
    pub fn by_session(&self, session_id: Uuid) -> Result<Vec<HistoryEntry>> {
        let all = self.read_all()?;
        Ok(all
            .into_iter()
            .filter(|e| e.session_id() == session_id)
            .collect())
    }

    /// Return entries whose timestamp falls within `[from, to]` (inclusive),
    /// in chronological order.
    pub fn by_time_range(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<HistoryEntry>> {
        let all = self.read_all()?;
        Ok(all
            .into_iter()
            .filter(|e| e.timestamp() >= from && e.timestamp() <= to)
            .collect())
    }

    /// Total number of (non-corrupt) entries recorded in the index.
    #[must_use]
    pub fn count(&self) -> usize {
        self.index.len()
    }

    /// Path to the backing JSONL file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    /// Read and deserialise all entries, skipping corrupt lines with a warning.
    fn read_all(&self) -> Result<Vec<HistoryEntry>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }

        let file = File::open(&self.path)
            .with_context(|| format!("cannot open history file: {}", self.path.display()))?;
        let reader = BufReader::new(file);

        let mut entries = Vec::new();
        for line in reader.lines() {
            let line = line.context("read error in history file")?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            match HistoryEntry::from_jsonl_line(trimmed) {
                Ok(e) => entries.push(e),
                Err(e) => log::warn!("skipping corrupt history line: {e}"),
            }
        }
        Ok(entries)
    }

    /// Scan the file to populate `index` and `next_offset`.
    fn rebuild_index(&mut self) -> Result<()> {
        if !self.path.exists() {
            self.next_offset = 0;
            return Ok(());
        }

        let file = File::open(&self.path).with_context(|| {
            format!(
                "cannot open history file for indexing: {}",
                self.path.display()
            )
        })?;
        let reader = BufReader::new(file);

        let mut offset: u64 = 0;
        for line in reader.lines() {
            let line = line.context("read error while rebuilding index")?;
            // +1 for the '\n' that writeln! appended
            let line_len = line.len() as u64 + 1;
            let trimmed = line.trim();
            if !trimmed.is_empty()
                && let Ok(entry) = HistoryEntry::from_jsonl_line(trimmed)
            {
                self.index.insert(entry.id(), offset);
            }
            offset += line_len;
        }
        self.next_offset = offset;
        Ok(())
    }
}

impl Drop for HistoryStore {
    /// Dropping the store releases the advisory lock automatically because
    /// the OS closes the `_lock_file` file descriptor.
    fn drop(&mut self) {
        // Explicitly unlock for clarity and portability (NFS, etc.).
        // Errors here are silently ignored — the fd close still releases the
        // lock on Linux/macOS regardless.
        let _ = self._lock_file.unlock();
    }
}

/// Derive the companion lock file path for a JSONL store path.
fn lock_path_for(path: &Path) -> PathBuf {
    let mut lock = path.to_path_buf().into_os_string();
    lock.push(".lock");
    PathBuf::from(lock)
}

/// Default data directory: `~/.local/share/phantom/history/`.
fn default_data_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home)
        .join(".local")
        .join("share")
        .join("phantom")
        .join("history")
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use phantom_semantic::CommandType;
    use std::io::Write;
    use tempfile::TempDir;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn temp_store() -> (HistoryStore, TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        let store = HistoryStore::open_at(&path).unwrap();
        (store, dir)
    }

    fn entry(cmd: &str, session: Uuid) -> HistoryEntry {
        HistoryEntry::builder(cmd, "/home/dev", session).build()
    }

    fn entry_with_exit(cmd: &str, session: Uuid, code: i32) -> HistoryEntry {
        HistoryEntry::builder(cmd, "/home/dev", session)
            .exit_code(code)
            .build()
    }

    // -----------------------------------------------------------------------
    // 1. Append increments count
    // -----------------------------------------------------------------------

    #[test]
    fn append_increments_count() {
        let (mut store, _dir) = temp_store();
        let session = Uuid::new_v4();

        assert_eq!(store.count(), 0);
        store.append(&entry("ls", session)).unwrap();
        assert_eq!(store.count(), 1);
        store.append(&entry("pwd", session)).unwrap();
        assert_eq!(store.count(), 2);
    }

    // -----------------------------------------------------------------------
    // 2. Recent returns last N in chronological order
    // -----------------------------------------------------------------------

    #[test]
    fn recent_returns_last_n_in_order() {
        let (mut store, _dir) = temp_store();
        let session = Uuid::new_v4();

        for i in 0..5 {
            store.append(&entry(&format!("cmd-{i}"), session)).unwrap();
        }

        let recent = store.recent(3).unwrap();
        assert_eq!(recent.len(), 3);
        assert_eq!(recent[0].command(), "cmd-2");
        assert_eq!(recent[1].command(), "cmd-3");
        assert_eq!(recent[2].command(), "cmd-4");
    }

    // -----------------------------------------------------------------------
    // 3. Recent with limit larger than total
    // -----------------------------------------------------------------------

    #[test]
    fn recent_limit_exceeds_total() {
        let (mut store, _dir) = temp_store();
        let session = Uuid::new_v4();
        store.append(&entry("only-one", session)).unwrap();

        let recent = store.recent(100).unwrap();
        assert_eq!(recent.len(), 1);
    }

    // -----------------------------------------------------------------------
    // 4. get_by_id returns correct entry (O(1) path)
    // -----------------------------------------------------------------------

    #[test]
    fn get_by_id_returns_correct_entry() {
        let (mut store, _dir) = temp_store();
        let session = Uuid::new_v4();

        store.append(&entry("first", session)).unwrap();
        let target = entry("target-command", session);
        let target_id = target.id();
        store.append(&target).unwrap();
        store.append(&entry("third", session)).unwrap();

        let found = store.get_by_id(target_id).unwrap().unwrap();
        assert_eq!(found.command(), "target-command");
        assert_eq!(found.id(), target_id);
    }

    // -----------------------------------------------------------------------
    // 5. get_by_id returns None for unknown UUID
    // -----------------------------------------------------------------------

    #[test]
    fn get_by_id_unknown_returns_none() {
        let (mut store, _dir) = temp_store();
        let session = Uuid::new_v4();
        store.append(&entry("ls", session)).unwrap();

        let result = store.get_by_id(Uuid::new_v4()).unwrap();
        assert!(result.is_none());
    }

    // -----------------------------------------------------------------------
    // 6. by_session filters correctly
    // -----------------------------------------------------------------------

    #[test]
    fn by_session_filters_entries() {
        let (mut store, _dir) = temp_store();
        let session_a = Uuid::new_v4();
        let session_b = Uuid::new_v4();

        store.append(&entry("a1", session_a)).unwrap();
        store.append(&entry("b1", session_b)).unwrap();
        store.append(&entry("a2", session_a)).unwrap();
        store.append(&entry("b2", session_b)).unwrap();

        let a_entries = store.by_session(session_a).unwrap();
        assert_eq!(a_entries.len(), 2);
        assert!(a_entries.iter().all(|e| e.session_id() == session_a));

        let b_entries = store.by_session(session_b).unwrap();
        assert_eq!(b_entries.len(), 2);
    }

    // -----------------------------------------------------------------------
    // 7. by_time_range returns entries within the range
    // -----------------------------------------------------------------------

    #[test]
    fn by_time_range_filters_correctly() {
        let (mut store, _dir) = temp_store();
        let session = Uuid::new_v4();
        let now = Utc::now();

        let past = HistoryEntry::builder("past", "/", session)
            .timestamp(now - Duration::hours(2))
            .build();
        let in_range = HistoryEntry::builder("in-range", "/", session)
            .timestamp(now - Duration::minutes(30))
            .build();
        let future = HistoryEntry::builder("future", "/", session)
            .timestamp(now + Duration::hours(1))
            .build();

        store.append(&past).unwrap();
        store.append(&in_range).unwrap();
        store.append(&future).unwrap();

        let from = now - Duration::hours(1);
        let to = now;
        let results = store.by_time_range(from, to).unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].command(), "in-range");
    }

    // -----------------------------------------------------------------------
    // 8. Index survives reopen (rebuild_index)
    // -----------------------------------------------------------------------

    #[test]
    fn index_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.jsonl");
        let session = Uuid::new_v4();

        let target_id = {
            let mut store = HistoryStore::open_at(&path).unwrap();
            store.append(&entry("first", session)).unwrap();
            let target = entry("second", session);
            let id = target.id();
            store.append(&target).unwrap();
            id
        };

        // Re-open — index must be rebuilt from file
        let store = HistoryStore::open_at(&path).unwrap();
        assert_eq!(store.count(), 2);

        let found = store.get_by_id(target_id).unwrap().unwrap();
        assert_eq!(found.command(), "second");
    }

    // -----------------------------------------------------------------------
    // 9. Corrupt lines are skipped gracefully
    // -----------------------------------------------------------------------

    #[test]
    fn corrupt_lines_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.jsonl");
        let session = Uuid::new_v4();

        {
            let mut store = HistoryStore::open_at(&path).unwrap();
            store.append(&entry("good-first", session)).unwrap();
        }

        // Inject a corrupt line directly (bypasses the store lock intentionally).
        {
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            writeln!(f, "{{not valid json").unwrap();
        }

        {
            let mut store = HistoryStore::open_at(&path).unwrap();
            store.append(&entry("good-second", session)).unwrap();
        }

        let store = HistoryStore::open_at(&path).unwrap();
        let recent = store.recent(10).unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].command(), "good-first");
        assert_eq!(recent[1].command(), "good-second");
    }

    // -----------------------------------------------------------------------
    // 10. Empty store is safe
    // -----------------------------------------------------------------------

    #[test]
    fn empty_store_is_safe() {
        let (store, _dir) = temp_store();
        assert_eq!(store.count(), 0);
        assert!(store.recent(10).unwrap().is_empty());
        assert!(store.get_by_id(Uuid::new_v4()).unwrap().is_none());
    }

    // -----------------------------------------------------------------------
    // 11. exit_code survives round-trip
    // -----------------------------------------------------------------------

    #[test]
    fn exit_code_round_trip() {
        let (mut store, _dir) = temp_store();
        let session = Uuid::new_v4();
        let e = entry_with_exit("failing-cmd", session, 127);
        let id = e.id();
        store.append(&e).unwrap();

        let restored = store.get_by_id(id).unwrap().unwrap();
        assert_eq!(restored.exit_code(), Some(127));
    }

    // -----------------------------------------------------------------------
    // 12. Semantic type survives round-trip
    // -----------------------------------------------------------------------

    #[test]
    fn semantic_type_round_trip() {
        let (mut store, _dir) = temp_store();
        let session = Uuid::new_v4();
        let e = HistoryEntry::builder("cargo build", "/project", session)
            .semantic_type(CommandType::Shell)
            .build();
        let id = e.id();
        store.append(&e).unwrap();

        let restored = store.get_by_id(id).unwrap().unwrap();
        assert_eq!(restored.semantic_type(), &CommandType::Shell);
    }

    // -----------------------------------------------------------------------
    // 13. Concurrent open: second store errors cleanly (lock is held)
    //
    //     This test exercises the advisory exclusive-lock guarantee from #211:
    //     two HistoryStore instances opened on the same path must not both
    //     succeed — the second must return an error that contains the word
    //     "locked" or similar, preventing silent index corruption.
    // -----------------------------------------------------------------------

    #[test]
    fn second_open_on_same_path_errors_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shared.jsonl");
        let session = Uuid::new_v4();

        // First store opens successfully and appends an entry.
        let mut store_a = HistoryStore::open_at(&path).unwrap();
        store_a.append(&entry("from-a", session)).unwrap();

        // Second open on the same path must fail because store_a holds the lock.
        let err = HistoryStore::open_at(&path)
            .err()
            .expect("expected an error opening a locked store, got Ok");
        let err_msg = format!("{err}");
        assert!(
            err_msg.contains("locked") || err_msg.contains("lock"),
            "expected error message to mention lock, got: {err_msg}"
        );

        // After store_a is dropped the lock is released and a new open succeeds.
        drop(store_a);
        let store_b = HistoryStore::open_at(&path).unwrap();
        assert_eq!(store_b.count(), 1);
        let entries = store_b.recent(10).unwrap();
        assert_eq!(entries[0].command(), "from-a");
    }

    // -----------------------------------------------------------------------
    // 14. Sequential stores write valid JSONL and every id resolves correctly
    //
    //     Verifies the "no index corruption" guarantee from #211 across the
    //     full write → close → reopen cycle.
    // -----------------------------------------------------------------------

    #[test]
    fn sequential_stores_produce_valid_index() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sequential.jsonl");
        let session = Uuid::new_v4();

        // Wave 1
        let mut ids_a = Vec::new();
        {
            let mut store = HistoryStore::open_at(&path).unwrap();
            for i in 0..10u32 {
                let e = entry(&format!("wave1-cmd-{i}"), session);
                ids_a.push(e.id());
                store.append(&e).unwrap();
            }
        }

        // Wave 2
        let mut ids_b = Vec::new();
        {
            let mut store = HistoryStore::open_at(&path).unwrap();
            for i in 0..10u32 {
                let e = entry(&format!("wave2-cmd-{i}"), session);
                ids_b.push(e.id());
                store.append(&e).unwrap();
            }
        }

        // Final read: all 20 entries must be present and resolvable.
        let reader = HistoryStore::open_at(&path).unwrap();
        assert_eq!(reader.count(), 20);

        for (i, id) in ids_a.iter().enumerate() {
            let found = reader.get_by_id(*id).unwrap().unwrap();
            assert_eq!(found.command(), format!("wave1-cmd-{i}"));
        }
        for (i, id) in ids_b.iter().enumerate() {
            let found = reader.get_by_id(*id).unwrap().unwrap();
            assert_eq!(found.command(), format!("wave2-cmd-{i}"));
        }
    }
}
