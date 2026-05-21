//! Append-only JSONL history store.
//!
//! One JSONL file lives at `~/.local/share/phantom/history/<session_id>.jsonl`.
//! Each line is a serialised [`HistoryEntry`].  The store maintains two
//! in-memory indices:
//!
//! - `index: HashMap<Uuid, u64>` — entry ID → byte offset (always current).
//! - `agent_index: Option<HashMap<String, Vec<u64>>>` — agent ID → sorted
//!   list of byte offsets; built lazily on the first [`HistoryStore::by_agent`]
//!   call and invalidated whenever [`HistoryStore::append`] is called.
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
// Rotation constants
// ---------------------------------------------------------------------------

/// Default maximum number of entries before the JSONL file is rotated.
const DEFAULT_MAX_HISTORY_ENTRIES: usize = 100_000;

/// Maximum number of rotated backup files kept (`.1`, `.2`, `.3`).
const MAX_ROTATED_FILES: u32 = 3;

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
///
/// ## Rotation
///
/// When the number of entries reaches `max_entries`, [`HistoryStore::append`]
/// rotates the current file: the existing file is renamed to `<path>.1` (bumping
/// older rotations up to `.2`, `.3`), and the oldest file beyond
/// [`MAX_ROTATED_FILES`] is deleted.  The active store then starts with a fresh,
/// empty file.
pub struct HistoryStore {
    /// Path to the `.jsonl` file.
    path: PathBuf,
    /// Maps entry id → byte offset of the start of its line in the file.
    index: HashMap<Uuid, u64>,
    /// Maps agent_id → byte offsets for every entry tagged with that agent.
    ///
    /// `None` means the index has not been built yet (lazy, built on first
    /// [`by_agent`] call).  Set back to `None` by [`append`] so that newly
    /// appended entries are always included.
    agent_index: Option<HashMap<String, Vec<u64>>>,
    /// Byte offset where the next append will begin.
    next_offset: u64,
    /// Number of valid entries currently in the active file.
    entry_count: usize,
    /// Maximum entries before the active file is rotated.
    max_entries: usize,
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
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).with_context(|| {
                    format!("cannot create history dir: {}", parent.display())
                })?;
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
            agent_index: None,
            next_offset: 0,
            entry_count: 0,
            max_entries: DEFAULT_MAX_HISTORY_ENTRIES,
            _lock_file: lock_file,
        };

        store.rebuild_index()?;
        Ok(store)
    }

    /// Override the rotation threshold (number of entries before rotation).
    ///
    /// Must be called immediately after [`open_at`] / [`open`] and before
    /// the first [`append`].  The entry count accumulated during `open_at`
    /// is preserved; rotation will trigger on the next [`append`] if the
    /// existing file already exceeds `max_entries`.
    #[must_use]
    pub fn with_max_entries(mut self, max_entries: usize) -> Self {
        self.max_entries = max_entries;
        self
    }

    // -----------------------------------------------------------------------
    // Writes
    // -----------------------------------------------------------------------

    /// Append `entry` to the JSONL file.
    ///
    /// The in-memory index is updated so subsequent id-lookups reflect the new
    /// entry without a file rescan.
    ///
    /// ## Rotation
    ///
    /// After writing, if `entry_count` reaches `max_entries` the store rotates:
    /// the current file is renamed to `<path>.1`, older rotations are bumped
    /// (`.1` → `.2`, etc.), and any rotation beyond [`MAX_ROTATED_FILES`] is
    /// deleted.  The in-memory index, offsets, and entry count are then reset
    /// to reflect the new empty file.
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
        self.entry_count += 1;
        // Invalidate the lazy agent index so the next by_agent call will
        // rebuild it and include this new entry.
        self.agent_index = None;

        // Rotate if the file has grown beyond the limit.
        if self.entry_count >= self.max_entries {
            self.rotate()?;
        }

        Ok(())
    }

    /// Rotate the active JSONL file.
    ///
    /// Renames rotated files upward (`.2` → `.3`, `.1` → `.2`), deletes the
    /// oldest if it would exceed [`MAX_ROTATED_FILES`], then renames the active
    /// file to `.1`.  The in-memory state is reset to represent a new, empty
    /// active file.
    fn rotate(&mut self) -> Result<()> {
        // Walk backwards so we don't clobber files still needing to be renamed.
        // Delete the oldest rotation if it exists to make room.
        let overflow_path = rotated_path(&self.path, MAX_ROTATED_FILES);
        if overflow_path.exists() {
            fs::remove_file(&overflow_path).with_context(|| {
                format!("cannot remove oldest rotation: {}", overflow_path.display())
            })?;
        }

        // Bump .2 → .3, .1 → .2  (iterate from MAX_ROTATED_FILES-1 down to 1)
        for n in (1..MAX_ROTATED_FILES).rev() {
            let src = rotated_path(&self.path, n);
            let dst = rotated_path(&self.path, n + 1);
            if src.exists() {
                fs::rename(&src, &dst).with_context(|| {
                    format!(
                        "cannot rename rotation {} → {}",
                        src.display(),
                        dst.display()
                    )
                })?;
            }
        }

        // Rename the active file to .1
        let backup = rotated_path(&self.path, 1);
        if self.path.exists() {
            fs::rename(&self.path, &backup).with_context(|| {
                format!(
                    "cannot rotate history file: {} → {}",
                    self.path.display(),
                    backup.display()
                )
            })?;
        }

        // Reset in-memory state — the active file is now empty.
        self.index.clear();
        self.agent_index = None;
        self.next_offset = 0;
        self.entry_count = 0;

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

    /// Return up to `limit` entries tagged with `agent_id`, in chronological
    /// order (oldest first).
    ///
    /// Uses the lazy agent index: the JSONL file is scanned at most once per
    /// invalidation cycle; subsequent calls seek directly to the relevant lines.
    pub fn by_agent(&mut self, agent_id: &str, limit: usize) -> Result<Vec<HistoryEntry>> {
        if self.agent_index.is_none() {
            self.build_agent_index()?;
        }

        let offsets = match self.agent_index.as_ref().and_then(|idx| idx.get(agent_id)) {
            Some(v) => v.clone(),
            None => return Ok(Vec::new()),
        };

        let take_from = offsets.len().saturating_sub(limit);
        let offsets = &offsets[take_from..];

        if !self.path.exists() {
            return Ok(Vec::new());
        }

        let mut file = File::open(&self.path)
            .with_context(|| format!("cannot open history file: {}", self.path.display()))?;

        let mut entries = Vec::with_capacity(offsets.len());
        for &offset in offsets {
            file.seek(SeekFrom::Start(offset)).context("seek failed")?;
            let mut reader = BufReader::new(&mut file);
            let mut line = String::new();
            reader.read_line(&mut line).context("read_line failed")?;
            match HistoryEntry::from_jsonl_line(line.trim()) {
                Ok(e) => entries.push(e),
                Err(e) => log::warn!("skipping corrupt history line at offset {offset}: {e}"),
            }
        }

        Ok(entries)
    }

    /// Search entries whose command string contains `query` (case-insensitive),
    /// returning up to `limit` results in chronological order.
    ///
    /// This is a full O(n) scan over all entries.
    // TODO: FTS — replace with SQLite FTS5 when the store is migrated to SQLite.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<HistoryEntry>> {
        let lower = query.to_lowercase();
        let all = self.read_all()?;
        let results: Vec<HistoryEntry> = all
            .into_iter()
            .filter(|e| e.command().to_lowercase().contains(&lower))
            .collect();
        let start = results.len().saturating_sub(limit);
        Ok(results[start..].to_vec())
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

    /// Build (or rebuild) the agent index by scanning the JSONL file once.
    ///
    /// For each line, records the byte offset before the line and the
    /// `agent_id` field value.  Lines without an `agent_id` are skipped.
    /// After this call `self.agent_index` is `Some(…)`.
    fn build_agent_index(&mut self) -> Result<()> {
        let mut idx: HashMap<String, Vec<u64>> = HashMap::new();

        if self.path.exists() {
            let file = File::open(&self.path).with_context(|| {
                format!(
                    "cannot open history file for agent indexing: {}",
                    self.path.display()
                )
            })?;
            let reader = BufReader::new(file);

            let mut offset: u64 = 0;
            for line in reader.lines() {
                let line = line.context("read error while building agent index")?;
                let line_len = line.len() as u64 + 1; // +1 for '\n'
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    if let Ok(entry) = HistoryEntry::from_jsonl_line(trimmed) {
                        if let Some(aid) = entry.agent_id() {
                            idx.entry(aid.to_owned()).or_default().push(offset);
                        }
                    }
                }
                offset += line_len;
            }
        }

        self.agent_index = Some(idx);
        Ok(())
    }

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

    /// Scan the file to populate `index`, `next_offset`, and `entry_count`.
    fn rebuild_index(&mut self) -> Result<()> {
        if !self.path.exists() {
            self.next_offset = 0;
            self.entry_count = 0;
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
        self.entry_count = self.index.len();
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

/// Return the path for a rotated backup of `path`.
///
/// For example, `rotated_path("/data/history.jsonl", 1)` returns
/// `/data/history.jsonl.1`.
fn rotated_path(path: &Path, n: u32) -> PathBuf {
    let mut p = path.as_os_str().to_os_string();
    p.push(format!(".{n}"));
    PathBuf::from(p)
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
    // 15. git status is auto-classified as Git on append
    // -----------------------------------------------------------------------

    #[test]
    fn git_status_auto_classified_on_append() {
        use phantom_semantic::GitCommand;

        let (mut store, _dir) = temp_store();
        let session = Uuid::new_v4();

        // Build an entry without explicitly setting semantic_type —
        // the builder must call SemanticParser::classify_command internally.
        let e = HistoryEntry::builder("git status", "/repo", session).build();
        assert_eq!(
            e.semantic_type(),
            &CommandType::Git(GitCommand::Status),
            "builder should auto-classify 'git status' as Git(Status)"
        );

        let id = e.id();
        store.append(&e).unwrap();

        // Verify the classification survives the JSONL round-trip.
        let restored = store.get_by_id(id).unwrap().unwrap();
        assert_eq!(
            restored.semantic_type(),
            &CommandType::Git(GitCommand::Status),
            "semantic_type should survive JSONL round-trip"
        );
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

    // -----------------------------------------------------------------------
    // 16. by_agent_uses_index_and_returns_correct_entries
    //
    //     Write 1000 entries across 3 agent IDs, then verify that by_agent
    //     returns only entries for the requested agent and that the result
    //     commands match what was written.
    // -----------------------------------------------------------------------

    #[test]
    fn by_agent_uses_index_and_returns_correct_entries() {
        let (mut store, _dir) = temp_store();
        let session = Uuid::new_v4();
        let agents = ["alpha", "beta", "gamma"];

        for i in 0..1000u32 {
            let agent = agents[(i as usize) % agents.len()];
            let e = HistoryEntry::builder(&format!("cmd-{i}-{agent}"), "/", session)
                .agent_id(agent)
                .build();
            store.append(&e).unwrap();
        }

        // Verify each agent slice
        for agent in &agents {
            let results = store.by_agent(agent, 1000).unwrap();
            // Each agent owns 1/3 of 1000 entries (333 or 334)
            assert!(
                results.len() >= 333 && results.len() <= 334,
                "agent {agent} expected ~333 entries, got {}",
                results.len()
            );
            // Every returned entry must belong to this agent
            for e in &results {
                assert_eq!(
                    e.agent_id(),
                    Some(*agent),
                    "entry {:?} should have agent_id={agent}",
                    e.command()
                );
            }
        }
    }

    // -----------------------------------------------------------------------
    // 17. append_invalidates_agent_index
    //
    //     Build the index via by_agent, append a new entry, then verify the
    //     subsequent by_agent call includes the new entry.
    // -----------------------------------------------------------------------

    #[test]
    fn append_invalidates_agent_index() {
        let (mut store, _dir) = temp_store();
        let session = Uuid::new_v4();

        // Seed three entries for "agent-x"
        for i in 0..3u32 {
            let e = HistoryEntry::builder(&format!("old-cmd-{i}"), "/", session)
                .agent_id("agent-x")
                .build();
            store.append(&e).unwrap();
        }

        // Prime the index
        let before = store.by_agent("agent-x", 100).unwrap();
        assert_eq!(before.len(), 3);

        // Append a new entry — this must invalidate the agent index
        let new_entry = HistoryEntry::builder("new-cmd", "/", session)
            .agent_id("agent-x")
            .build();
        store.append(&new_entry).unwrap();

        // The index must have been invalidated; by_agent must rebuild and
        // return all 4 entries including the newly appended one.
        let after = store.by_agent("agent-x", 100).unwrap();
        assert_eq!(after.len(), 4, "expected 4 entries after append, got {}", after.len());
        assert!(
            after.iter().any(|e| e.command() == "new-cmd"),
            "newly appended entry should be found by by_agent"
        );
    }

    // -----------------------------------------------------------------------
    // 18. rotation_triggered_at_max_entries
    //
    //     When entry_count reaches max_entries the active file is rotated:
    //     a `.1` backup appears and the active file is fresh / empty.
    // -----------------------------------------------------------------------

    #[test]
    fn rotation_triggered_at_max_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.jsonl");
        let session = Uuid::new_v4();

        // Use a tiny limit so we don't have to write 100k entries.
        let mut store = HistoryStore::open_at(&path).unwrap().with_max_entries(3);

        // Write exactly max_entries entries; rotation happens on the 3rd append.
        for i in 0..3u32 {
            store.append(&entry(&format!("cmd-{i}"), session)).unwrap();
        }

        // After rotation the active file must not exist yet (it was renamed to
        // `.1` and a fresh empty file hasn't been created until the next append).
        let backup = rotated_path(&path, 1);
        assert!(backup.exists(), "history.jsonl.1 must exist after rotation");

        // The in-memory state must reflect an empty active store.
        assert_eq!(store.count(), 0, "entry_count should be reset after rotation");

        // A subsequent append goes to the new (empty) active file.
        store.append(&entry("cmd-after-rotation", session)).unwrap();
        assert_eq!(store.count(), 1);
        assert!(path.exists(), "new active file must exist after first post-rotation append");
    }

    // -----------------------------------------------------------------------
    // 19. rotated_files_capped_at_three
    //
    //     After 4 rotations only .1, .2, .3 remain; the overflow (.4) is
    //     deleted.
    // -----------------------------------------------------------------------

    #[test]
    fn rotated_files_capped_at_three() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.jsonl");
        let session = Uuid::new_v4();

        // max_entries = 1 so every single append triggers a rotation.
        let mut store = HistoryStore::open_at(&path).unwrap().with_max_entries(1);

        // 4 appends → 4 rotations attempted; only .1 .2 .3 should survive.
        for i in 0..4u32 {
            store.append(&entry(&format!("cmd-{i}"), session)).unwrap();
        }

        // .1, .2, .3 must exist
        for n in 1..=3u32 {
            let rp = rotated_path(&path, n);
            assert!(rp.exists(), "rotation file .{n} should exist after 4 appends");
        }

        // .4 must NOT exist (deleted during the 4th rotation)
        let rp4 = rotated_path(&path, 4);
        assert!(!rp4.exists(), "rotation file .4 must be deleted (cap is 3)");
    }
}
