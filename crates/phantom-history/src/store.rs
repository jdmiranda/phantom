use std::collections::hash_map::DefaultHasher;
use std::fs::{self, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use phantom_semantic::ParsedOutput;
use serde::{Deserialize, Serialize};

/// A history entry with timestamp and metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// Unix epoch seconds.
    pub timestamp: u64,
    /// Working directory where the command was executed.
    pub working_dir: String,
    /// Semantic parse result for the command.
    pub parsed: ParsedOutput,
}

/// Append-only command history backed by a `.jsonl` file.
///
/// One file per project directory, stored at `~/.config/phantom/history/`.
/// Each line is a JSON-serialized [`HistoryEntry`].
pub struct HistoryStore {
    path: PathBuf,
}

impl HistoryStore {
    /// Open or create a history store for the given project directory.
    ///
    /// The project directory is hashed to produce a stable filename under
    /// `~/.config/phantom/history/{hash}.jsonl`.
    pub fn open(project_dir: &str) -> Result<Self> {
        let base = dirs_base();
        fs::create_dir_all(&base)
            .with_context(|| format!("failed to create history dir: {}", base.display()))?;

        let hash = hash_project_dir(project_dir);
        let path = base.join(format!("{hash}.jsonl"));

        Ok(Self { path })
    }

    /// Open a store at an explicit path (used in tests).
    #[cfg(test)]
    fn open_at(path: PathBuf) -> Self {
        Self { path }
    }

    /// Append a command execution to history.
    pub fn append(&self, entry: &HistoryEntry) -> Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("failed to open history file: {}", self.path.display()))?;

        let json =
            serde_json::to_string(entry).context("failed to serialize history entry")?;

        writeln!(file, "{json}").context("failed to write history entry")?;

        Ok(())
    }

    /// Search history by text query (case-insensitive substring match on
    /// command, raw_output, and error messages). Returns the most recent
    /// matches first, up to `limit`.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<HistoryEntry>> {
        let lower_query = query.to_lowercase();
        let entries = self.read_all()?;

        let mut matches: Vec<HistoryEntry> = entries
            .into_iter()
            .filter(|e| entry_matches(e, &lower_query))
            .collect();

        // Most recent first.
        matches.reverse();
        matches.truncate(limit);

        Ok(matches)
    }

    /// Get the last N entries (most recent last in the returned vec, matching
    /// chronological order).
    pub fn recent(&self, limit: usize) -> Result<Vec<HistoryEntry>> {
        let entries = self.read_all()?;
        let start = entries.len().saturating_sub(limit);

        Ok(entries[start..].to_vec())
    }

    /// Search for entries with errors, most recent first.
    pub fn errors_recent(&self, limit: usize) -> Result<Vec<HistoryEntry>> {
        let entries = self.read_all()?;

        let mut with_errors: Vec<HistoryEntry> = entries
            .into_iter()
            .filter(|e| !e.parsed.errors.is_empty())
            .collect();

        with_errors.reverse();
        with_errors.truncate(limit);

        Ok(with_errors)
    }

    /// Get total entry count (without deserializing every line).
    pub fn count(&self) -> Result<usize> {
        if !self.path.exists() {
            return Ok(0);
        }

        let file = fs::File::open(&self.path)
            .with_context(|| format!("failed to open history file: {}", self.path.display()))?;
        let reader = BufReader::new(file);

        let count = reader
            .lines()
            .filter_map(|l| l.ok())
            .filter(|l| !l.trim().is_empty())
            .count();

        Ok(count)
    }

    /// Get the history file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    /// Read all entries from the backing file.
    fn read_all(&self) -> Result<Vec<HistoryEntry>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }

        let file = fs::File::open(&self.path)
            .with_context(|| format!("failed to open history file: {}", self.path.display()))?;
        let reader = BufReader::new(file);

        let mut entries = Vec::new();
        for line in reader.lines() {
            let line = line.context("failed to read line from history file")?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            match serde_json::from_str::<HistoryEntry>(trimmed) {
                Ok(entry) => entries.push(entry),
                Err(e) => {
                    // Log but skip corrupt lines so one bad entry doesn't brick
                    // the entire history.
                    log::warn!("skipping corrupt history entry: {e}");
                }
            }
        }

        Ok(entries)
    }
}

/// Case-insensitive substring match across the searchable fields of an entry.
fn entry_matches(entry: &HistoryEntry, lower_query: &str) -> bool {
    let p = &entry.parsed;

    if p.command.to_lowercase().contains(lower_query) {
        return true;
    }
    if p.raw_output.to_lowercase().contains(lower_query) {
        return true;
    }
    for err in &p.errors {
        if err.message.to_lowercase().contains(lower_query) {
            return true;
        }
        if err.raw_line.to_lowercase().contains(lower_query) {
            return true;
        }
    }
    for warn in &p.warnings {
        if warn.message.to_lowercase().contains(lower_query) {
            return true;
        }
    }

    false
}

/// Default base directory for history files.
fn dirs_base() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home)
        .join(".config")
        .join("phantom")
        .join("history")
}

/// Deterministic hash of a project directory path to a hex string.
fn hash_project_dir(project_dir: &str) -> String {
    let mut hasher = DefaultHasher::new();
    project_dir.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use phantom_semantic::{
        CommandType, ContentType, DetectedError, ErrorType, ParsedOutput, Severity,
    };
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Helper: create a store backed by a temp file.
    fn temp_store() -> (HistoryStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        let store = HistoryStore::open_at(path);
        (store, dir)
    }

    /// Helper: build a basic history entry.
    fn make_entry(command: &str, output: &str, errors: Vec<DetectedError>) -> HistoryEntry {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        HistoryEntry {
            timestamp: now,
            working_dir: "/home/dev/project".to_string(),
            parsed: ParsedOutput {
                command: command.to_string(),
                command_type: CommandType::Shell,
                exit_code: Some(0),
                content_type: ContentType::PlainText,
                errors,
                warnings: vec![],
                duration_ms: Some(42),
                raw_output: output.to_string(),
            },
        }
    }

    fn make_error(message: &str) -> DetectedError {
        DetectedError {
            message: message.to_string(),
            error_type: ErrorType::Compiler,
            file: Some("src/main.rs".to_string()),
            line: Some(10),
            column: Some(5),
            code: Some("E0308".to_string()),
            severity: Severity::Error,
            raw_line: format!("error[E0308]: {message}"),
            suggestion: None,
        }
    }

    // -----------------------------------------------------------------------
    // 1. Append and count
    // -----------------------------------------------------------------------

    #[test]
    fn append_increments_count() {
        let (store, _dir) = temp_store();

        assert_eq!(store.count().unwrap(), 0);

        store.append(&make_entry("ls -la", "file1\nfile2", vec![])).unwrap();
        assert_eq!(store.count().unwrap(), 1);

        store.append(&make_entry("pwd", "/home/dev/project", vec![])).unwrap();
        assert_eq!(store.count().unwrap(), 2);
    }

    // -----------------------------------------------------------------------
    // 2. Recent entries
    // -----------------------------------------------------------------------

    #[test]
    fn recent_returns_last_n_in_order() {
        let (store, _dir) = temp_store();

        for i in 0..5 {
            store
                .append(&make_entry(&format!("cmd-{i}"), "", vec![]))
                .unwrap();
        }

        let recent = store.recent(3).unwrap();
        assert_eq!(recent.len(), 3);
        assert_eq!(recent[0].parsed.command, "cmd-2");
        assert_eq!(recent[1].parsed.command, "cmd-3");
        assert_eq!(recent[2].parsed.command, "cmd-4");
    }

    // -----------------------------------------------------------------------
    // 3. Recent with limit larger than total
    // -----------------------------------------------------------------------

    #[test]
    fn recent_limit_exceeds_total() {
        let (store, _dir) = temp_store();

        store.append(&make_entry("only-one", "", vec![])).unwrap();

        let recent = store.recent(100).unwrap();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].parsed.command, "only-one");
    }

    // -----------------------------------------------------------------------
    // 4. Search by command text
    // -----------------------------------------------------------------------

    #[test]
    fn search_matches_command_text() {
        let (store, _dir) = temp_store();

        store.append(&make_entry("cargo build", "Compiling...", vec![])).unwrap();
        store.append(&make_entry("git status", "On branch main", vec![])).unwrap();
        store.append(&make_entry("cargo test", "test result: ok", vec![])).unwrap();

        let results = store.search("cargo", 10).unwrap();
        assert_eq!(results.len(), 2);
        // Most recent first.
        assert_eq!(results[0].parsed.command, "cargo test");
        assert_eq!(results[1].parsed.command, "cargo build");
    }

    // -----------------------------------------------------------------------
    // 5. Search is case-insensitive
    // -----------------------------------------------------------------------

    #[test]
    fn search_is_case_insensitive() {
        let (store, _dir) = temp_store();

        store
            .append(&make_entry("CARGO BUILD", "COMPILING", vec![]))
            .unwrap();

        let results = store.search("cargo", 10).unwrap();
        assert_eq!(results.len(), 1);

        let results_upper = store.search("CARGO", 10).unwrap();
        assert_eq!(results_upper.len(), 1);
    }

    // -----------------------------------------------------------------------
    // 6. Search matches raw output
    // -----------------------------------------------------------------------

    #[test]
    fn search_matches_raw_output() {
        let (store, _dir) = temp_store();

        store
            .append(&make_entry("curl api.example.com", "404 Not Found", vec![]))
            .unwrap();
        store
            .append(&make_entry("echo hello", "hello", vec![]))
            .unwrap();

        let results = store.search("not found", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].parsed.command, "curl api.example.com");
    }

    // -----------------------------------------------------------------------
    // 7. Search matches error messages
    // -----------------------------------------------------------------------

    #[test]
    fn search_matches_error_messages() {
        let (store, _dir) = temp_store();

        let err = make_error("mismatched types");
        store
            .append(&make_entry("cargo build", "", vec![err]))
            .unwrap();
        store
            .append(&make_entry("ls", "", vec![]))
            .unwrap();

        let results = store.search("mismatched", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].parsed.command, "cargo build");
    }

    // -----------------------------------------------------------------------
    // 8. errors_recent filters correctly
    // -----------------------------------------------------------------------

    #[test]
    fn errors_recent_filters_entries_with_errors() {
        let (store, _dir) = temp_store();

        store
            .append(&make_entry("cargo build", "ok", vec![]))
            .unwrap();

        let err1 = make_error("mismatched types");
        store
            .append(&make_entry("cargo check", "", vec![err1]))
            .unwrap();

        store
            .append(&make_entry("ls", "file1", vec![]))
            .unwrap();

        let err2 = make_error("unresolved import");
        store
            .append(&make_entry("cargo test", "", vec![err2]))
            .unwrap();

        let errors = store.errors_recent(10).unwrap();
        assert_eq!(errors.len(), 2);
        // Most recent first.
        assert_eq!(errors[0].parsed.command, "cargo test");
        assert_eq!(errors[1].parsed.command, "cargo check");
    }

    // -----------------------------------------------------------------------
    // 9. errors_recent respects limit
    // -----------------------------------------------------------------------

    #[test]
    fn errors_recent_respects_limit() {
        let (store, _dir) = temp_store();

        for i in 0..5 {
            let err = make_error(&format!("error-{i}"));
            store
                .append(&make_entry(&format!("cmd-{i}"), "", vec![err]))
                .unwrap();
        }

        let errors = store.errors_recent(2).unwrap();
        assert_eq!(errors.len(), 2);
        assert_eq!(errors[0].parsed.command, "cmd-4");
        assert_eq!(errors[1].parsed.command, "cmd-3");
    }

    // -----------------------------------------------------------------------
    // 10. Empty store returns empty results
    // -----------------------------------------------------------------------

    #[test]
    fn empty_store_returns_empty() {
        let (store, _dir) = temp_store();

        assert_eq!(store.count().unwrap(), 0);
        assert!(store.recent(10).unwrap().is_empty());
        assert!(store.search("anything", 10).unwrap().is_empty());
        assert!(store.errors_recent(10).unwrap().is_empty());
    }

    // -----------------------------------------------------------------------
    // 11. Corrupt lines are skipped gracefully
    // -----------------------------------------------------------------------

    #[test]
    fn corrupt_lines_skipped() {
        let (store, _dir) = temp_store();

        // Write a valid entry.
        store
            .append(&make_entry("valid", "output", vec![]))
            .unwrap();

        // Manually append garbage.
        let mut file = OpenOptions::new()
            .append(true)
            .open(store.path())
            .unwrap();
        writeln!(file, "{{not valid json at all").unwrap();

        // Write another valid entry.
        store
            .append(&make_entry("also-valid", "output2", vec![]))
            .unwrap();

        let recent = store.recent(10).unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].parsed.command, "valid");
        assert_eq!(recent[1].parsed.command, "also-valid");
    }

    // -----------------------------------------------------------------------
    // 12. Serialization round-trip
    // -----------------------------------------------------------------------

    #[test]
    fn entry_round_trips_through_json() {
        let err = make_error("type mismatch");
        let entry = make_entry("cargo build --release", "Compiling phantom...", vec![err]);

        let json = serde_json::to_string(&entry).unwrap();
        let deser: HistoryEntry = serde_json::from_str(&json).unwrap();

        assert_eq!(deser.parsed.command, entry.parsed.command);
        assert_eq!(deser.parsed.errors.len(), 1);
        assert_eq!(deser.parsed.errors[0].message, "type mismatch");
        assert_eq!(deser.working_dir, entry.working_dir);
    }
}
