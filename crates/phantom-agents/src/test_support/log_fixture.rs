//! Fresh, isolated [`EventLog`] fixtures for tests.
//!
//! Issue #645 made [`crate::dispatch::DispatchContext::event_log`] (and the
//! matching field on [`crate::chat_tools::ChatToolContext`] /
//! [`crate::defender_tools::DefenderToolContext`]) non-`Option`. Every call
//! site that used to pass `event_log: None` now needs a real
//! `Arc<Mutex<EventLog>>`. [`fresh_log`] is the canonical builder for that
//! handle.
//!
//! The underlying [`EventLog`] is opened against a path inside a freshly-
//! created [`tempfile::TempDir`]. Tests typically only care about the handle
//! ([`fresh_log`]), but [`LogFixture`] is offered for the rare test that
//! wants to read the on-disk file directly or assert against it after the
//! fact.

use std::sync::{Arc, Mutex};

use phantom_memory::event_log::EventLog;
use tempfile::TempDir;

/// Open a fresh [`EventLog`] backed by a private tempdir and return the
/// shared handle.
///
/// The tempdir is intentionally **persisted** (via [`TempDir::keep`]) so
/// the log path remains valid for the entire test process even after the
/// borrowed [`TempDir`] handle goes out of scope. Tests are short-lived; the
/// OS reaps the directory at process exit. For tests that want to read the
/// on-disk file back (or that prefer deterministic cleanup), use
/// [`LogFixture`] directly.
///
/// # Panics
///
/// Panics if the tempdir cannot be created or [`EventLog::open`] fails — a
/// test environment that cannot create files is already broken; pretending
/// otherwise would hide the real failure behind a downstream `unwrap`.
#[must_use]
pub fn fresh_log() -> Arc<Mutex<EventLog>> {
    let fix = LogFixture::new();
    // Persist the directory so the on-disk path stays valid after the
    // [`TempDir`] is dropped. The OS reclaims it at process exit.
    let _persisted = fix.dir.keep();
    fix.log
}

/// Bundle of a [`EventLog`] handle plus the [`TempDir`] backing its on-disk
/// file. Hold onto a `LogFixture` (instead of just the handle from
/// [`fresh_log`]) when a test needs the directory to live exactly as long
/// as the fixture, or wants to read the JSONL file back via [`Self::path`].
///
/// Dropping a `LogFixture` removes the tempdir and any pending writes.
pub struct LogFixture {
    pub log: Arc<Mutex<EventLog>>,
    pub dir: TempDir,
}

impl LogFixture {
    /// Build a `LogFixture` against a freshly-created tempdir.
    ///
    /// # Panics
    ///
    /// Panics if the tempdir cannot be created or [`EventLog::open`] fails.
    #[must_use]
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("test_support: tempdir");
        let path = dir.path().join("events.jsonl");
        let log = EventLog::open(&path).expect("test_support: EventLog::open");
        Self {
            log: Arc::new(Mutex::new(log)),
            dir,
        }
    }

    /// Path of the backing JSONL file. Useful for tests that re-open the
    /// log out-of-band or verify durable on-disk state.
    #[must_use]
    pub fn path(&self) -> std::path::PathBuf {
        self.dir.path().join("events.jsonl")
    }
}

impl Default for LogFixture {
    fn default() -> Self {
        Self::new()
    }
}
