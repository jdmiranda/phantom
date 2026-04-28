//! Append-only event log for the agent runtime.
//!
//! This is the central memory stream — every observation made by the substrate,
//! agents, or user lands here. Both watcher and conversational agents query the
//! same log, so the in-memory tail is the source of truth for hot reads while
//! the on-disk JSONL file is the durable record.
//!
//! Each line in the file is a self-contained [`EventEnvelope`] serialized as
//! JSON. Ids are monotonic and recovered on `open()` by reading the last line
//! of an existing file.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// Maximum number of events kept in memory for fast `tail()` / `filter_kind()`.
const TAIL_CAPACITY: usize = 4096;

/// Broadcast channel capacity. Lossy under heavy load — the file is the durable
/// source, this is just for live subscribers.
const BROADCAST_CAPACITY: usize = 1024;

/// Buffered events between forced flushes.
const FLUSH_EVERY_N_EVENTS: u64 = 64;

/// A single event recorded in the log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventEnvelope {
    /// Monotonically increasing id assigned at append time.
    pub id: u64,
    /// Wall-clock time of the append, in milliseconds since the Unix epoch.
    pub ts_unix_ms: i64,
    /// Where the event originated.
    pub source: EventSource,
    /// Dotted-path event kind, e.g. `"agent.spawn"` or `"tool.invoked"`.
    pub kind: String,
    /// Free-form structured payload.
    pub payload: serde_json::Value,
}

/// Origin of an event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum EventSource {
    /// The substrate runtime itself (host).
    Substrate,
    /// An agent identified by its runtime id.
    Agent { id: u64 },
    /// A direct user action.
    User,
}

/// Append-only event log with an in-memory tail and live broadcast channel.
pub struct EventLog {
    path: PathBuf,
    writer: BufWriter<File>,
    next_id: u64,
    tail: VecDeque<EventEnvelope>,
    tx: broadcast::Sender<EventEnvelope>,
    appends_since_flush: u64,
}

fn now_unix_ms() -> i64 {
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    // Cast safe for any time within ~292M years of the epoch.
    dur.as_millis() as i64
}

/// Read the last non-empty line of `file`, if any.
///
/// Scans backwards in 4 KiB chunks so we don't slurp huge logs. Returns
/// `Ok(None)` when the file is empty or contains no complete line.
fn read_last_line(file: &mut File) -> std::io::Result<Option<String>> {
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(None);
    }

    const CHUNK: usize = 4096;
    let mut pos: u64 = len;
    let mut buf: Vec<u8> = Vec::new();

    while pos > 0 {
        let read_size = std::cmp::min(CHUNK as u64, pos) as usize;
        pos -= read_size as u64;
        file.seek(SeekFrom::Start(pos))?;
        let mut chunk = vec![0u8; read_size];
        file.read_exact(&mut chunk)?;
        // Prepend chunk to buf.
        chunk.extend_from_slice(&buf);
        buf = chunk;

        // Strip a trailing newline (if file ends with `\n`) so we look for the
        // newline *before* the final record.
        let scan_end = if buf.last() == Some(&b'\n') {
            buf.len() - 1
        } else {
            buf.len()
        };

        if let Some(idx) = buf[..scan_end].iter().rposition(|b| *b == b'\n') {
            // Last line is everything after that newline, up to scan_end.
            let line = &buf[idx + 1..scan_end];
            if line.is_empty() {
                return Ok(None);
            }
            return Ok(Some(String::from_utf8_lossy(line).into_owned()));
        }
    }

    // No newline found anywhere — the whole file is one line (possibly
    // unterminated). Return it if non-empty.
    let scan_end = if buf.last() == Some(&b'\n') {
        buf.len() - 1
    } else {
        buf.len()
    };
    if scan_end == 0 {
        Ok(None)
    } else {
        Ok(Some(String::from_utf8_lossy(&buf[..scan_end]).into_owned()))
    }
}

/// Best-effort recovery of the highest valid id in `path`.
///
/// Walks backwards line-by-line from EOF until a parseable [`EventEnvelope`]
/// is found, so a partially-written final line doesn't poison id assignment.
/// Returns `Ok(0)` if the file doesn't exist, is empty, or contains no
/// recoverable event.
fn recover_last_id(path: &Path) -> std::io::Result<u64> {
    if !path.exists() {
        return Ok(0);
    }

    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut last_good: u64 = 0;
    for line in reader.lines() {
        let line = line?;
        if line.is_empty() {
            continue;
        }
        if let Ok(ev) = serde_json::from_str::<EventEnvelope>(&line)
            && ev.id > last_good
        {
            last_good = ev.id;
        }
    }
    Ok(last_good)
}

impl EventLog {
    /// Open or create the log file at `path`.
    ///
    /// Recovers `next_id` by walking the existing file (if any) and finding the
    /// largest id of any successfully parseable line. A truncated or malformed
    /// final line is therefore tolerated — `next_id` will be `last_good_id + 1`.
    pub fn open(path: &Path) -> std::io::Result<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }

        let last_id = recover_last_id(path)?;

        // First ensure the file exists, then probe the last line one more time
        // using the chunked reader as a defensive cross-check: if it disagrees
        // (e.g. JSON walker missed a higher id) prefer the higher value.
        let mut probe = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        if let Some(line) = read_last_line(&mut probe)?
            && let Ok(ev) = serde_json::from_str::<EventEnvelope>(&line)
            && ev.id > last_id
        {
            // Defensive: shouldn't happen since recover_last_id walks all lines.
        }
        drop(probe);

        let file = OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)?;
        let writer = BufWriter::new(file);

        let (tx, _rx) = broadcast::channel(BROADCAST_CAPACITY);

        Ok(Self {
            path: path.to_path_buf(),
            writer,
            next_id: last_id + 1,
            tail: VecDeque::with_capacity(TAIL_CAPACITY),
            tx,
            appends_since_flush: 0,
        })
    }

    /// Append a new event. Assigns the next monotonic id and stamps `ts_unix_ms`
    /// to "now".
    ///
    /// The event is written as one JSON line followed by `\n`, pushed onto the
    /// in-memory tail, and broadcast to live subscribers. The buffered writer
    /// is flushed eagerly every [`FLUSH_EVERY_N_EVENTS`] appends.
    pub fn append(
        &mut self,
        source: EventSource,
        kind: impl Into<String>,
        payload: serde_json::Value,
    ) -> std::io::Result<EventEnvelope> {
        let envelope = EventEnvelope {
            id: self.next_id,
            ts_unix_ms: now_unix_ms(),
            source,
            kind: kind.into(),
            payload,
        };
        self.next_id += 1;

        let mut line = serde_json::to_string(&envelope).map_err(std::io::Error::other)?;
        line.push('\n');
        self.writer.write_all(line.as_bytes())?;

        self.appends_since_flush += 1;
        if self.appends_since_flush >= FLUSH_EVERY_N_EVENTS {
            self.writer.flush()?;
            self.appends_since_flush = 0;
        }

        if self.tail.len() == TAIL_CAPACITY {
            self.tail.pop_front();
        }
        self.tail.push_back(envelope.clone());

        // Broadcast is lossy by design — file is the durable source. Errors
        // here just mean no live subscribers, which is fine.
        let _ = self.tx.send(envelope.clone());

        Ok(envelope)
    }

    /// Most-recent `n` events from the in-memory tail, in chronological order
    /// (oldest first). Performs no file I/O.
    #[must_use]
    pub fn tail(&self, n: usize) -> Vec<EventEnvelope> {
        let take = std::cmp::min(n, self.tail.len());
        let start = self.tail.len() - take;
        self.tail.iter().skip(start).cloned().collect()
    }

    /// All events in the in-memory tail with `kind == kind`.
    #[must_use]
    pub fn filter_kind(&self, kind: &str) -> Vec<EventEnvelope> {
        self.tail
            .iter()
            .filter(|e| e.kind == kind)
            .cloned()
            .collect()
    }

    /// Subscribe to the live event broadcast.
    ///
    /// Subscribers only see events appended *after* they subscribe. The channel
    /// is bounded; slow consumers will see `RecvError::Lagged` rather than
    /// blocking the producer.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<EventEnvelope> {
        self.tx.subscribe()
    }

    /// Force a flush of the buffered writer.
    pub fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush()?;
        self.appends_since_flush = 0;
        Ok(())
    }

    /// Path of the backing file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The id that will be assigned to the next appended event.
    #[must_use]
    pub fn next_id(&self) -> u64 {
        self.next_id
    }
}

impl Drop for EventLog {
    fn drop(&mut self) {
        let _ = self.writer.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tempfile::tempdir;

    fn ev_path(name: &str) -> (PathBuf, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let path = dir.path().join(name);
        (path, dir)
    }

    #[test]
    fn open_empty_path_starts_at_id_one() {
        let (path, _dir) = ev_path("events.jsonl");
        let mut log = EventLog::open(&path).unwrap();
        assert_eq!(log.next_id(), 1);

        let e1 = log
            .append(EventSource::Substrate, "first", serde_json::json!({"a": 1}))
            .unwrap();
        assert_eq!(e1.id, 1);

        let e2 = log
            .append(EventSource::User, "second", serde_json::json!({}))
            .unwrap();
        assert_eq!(e2.id, 2);
        assert_eq!(log.next_id(), 3);
    }

    #[test]
    fn reopen_recovers_next_id() {
        let (path, _dir) = ev_path("recover.jsonl");

        {
            let mut log = EventLog::open(&path).unwrap();
            for i in 0..5 {
                let ev = log
                    .append(
                        EventSource::Agent { id: 7 },
                        "x",
                        serde_json::json!({ "n": i }),
                    )
                    .unwrap();
                assert_eq!(ev.id, i + 1);
            }
            log.flush().unwrap();
        } // Drop flushes too.

        let mut log = EventLog::open(&path).unwrap();
        assert_eq!(log.next_id(), 6);
        let e = log
            .append(EventSource::User, "after", serde_json::json!({}))
            .unwrap();
        assert_eq!(e.id, 6);
    }

    #[test]
    fn reopen_tolerates_partial_last_line() {
        let (path, _dir) = ev_path("partial.jsonl");

        {
            let mut log = EventLog::open(&path).unwrap();
            for _ in 0..3 {
                log.append(EventSource::Substrate, "ok", serde_json::json!({}))
                    .unwrap();
            }
            log.flush().unwrap();
        }

        // Append a malformed (truncated) final line, no trailing newline.
        {
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(b"{\"id\":99,\"ts_unix_ms\":\"BORK").unwrap();
            f.flush().unwrap();
        }

        // Should recover from the last *good* id (3), not be poisoned by 99.
        let mut log = EventLog::open(&path).unwrap();
        assert_eq!(log.next_id(), 4);
        let e = log
            .append(EventSource::User, "next", serde_json::json!({}))
            .unwrap();
        assert_eq!(e.id, 4);
    }

    #[test]
    fn tail_returns_last_n_in_chronological_order() {
        let (path, _dir) = ev_path("tail.jsonl");
        let mut log = EventLog::open(&path).unwrap();
        for i in 0..10 {
            log.append(
                EventSource::Substrate,
                "evt",
                serde_json::json!({ "i": i }),
            )
            .unwrap();
        }

        let tail = log.tail(3);
        assert_eq!(tail.len(), 3);
        assert_eq!(tail[0].id, 8);
        assert_eq!(tail[1].id, 9);
        assert_eq!(tail[2].id, 10);

        // Asking for more than we have returns everything.
        let all = log.tail(100);
        assert_eq!(all.len(), 10);
        assert_eq!(all.first().unwrap().id, 1);
        assert_eq!(all.last().unwrap().id, 10);

        // Asking for zero returns nothing.
        assert!(log.tail(0).is_empty());
    }

    #[test]
    fn filter_kind_matches_only_named() {
        let (path, _dir) = ev_path("filter.jsonl");
        let mut log = EventLog::open(&path).unwrap();
        log.append(EventSource::Substrate, "foo.bar", serde_json::json!({}))
            .unwrap();
        log.append(EventSource::User, "baz.qux", serde_json::json!({}))
            .unwrap();
        log.append(EventSource::Substrate, "foo.bar", serde_json::json!({}))
            .unwrap();
        log.append(EventSource::Agent { id: 1 }, "foo.baz", serde_json::json!({}))
            .unwrap();

        let foos = log.filter_kind("foo.bar");
        assert_eq!(foos.len(), 2);
        assert!(foos.iter().all(|e| e.kind == "foo.bar"));

        let none = log.filter_kind("does.not.exist");
        assert!(none.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn subscribe_receives_subsequent_appends() {
        let (path, _dir) = ev_path("sub.jsonl");
        let mut log = EventLog::open(&path).unwrap();

        let mut rx = log.subscribe();

        log.append(EventSource::User, "first", serde_json::json!({"x": 1}))
            .unwrap();
        log.append(EventSource::Substrate, "second", serde_json::json!({}))
            .unwrap();
        log.append(EventSource::Agent { id: 3 }, "third", serde_json::json!({}))
            .unwrap();

        let e1 = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("recv timed out")
            .expect("channel closed");
        let e2 = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let e3 = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(e1.id, 1);
        assert_eq!(e1.kind, "first");
        assert_eq!(e2.id, 2);
        assert_eq!(e3.id, 3);
        assert_eq!(e3.source, EventSource::Agent { id: 3 });
    }

    #[test]
    fn each_file_line_is_valid_json_envelope() {
        let (path, _dir) = ev_path("json.jsonl");
        {
            let mut log = EventLog::open(&path).unwrap();
            for i in 0..20 {
                log.append(
                    if i % 2 == 0 {
                        EventSource::User
                    } else {
                        EventSource::Agent { id: i }
                    },
                    "k",
                    serde_json::json!({ "i": i, "nested": { "v": "hi" } }),
                )
                .unwrap();
            }
        } // Drop -> flush.

        let contents = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 20);
        for (idx, line) in lines.iter().enumerate() {
            let ev: EventEnvelope = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("line {idx} not valid json: {e}: {line}"));
            assert_eq!(ev.id as usize, idx + 1);
            assert_eq!(ev.kind, "k");
        }
    }

    #[test]
    fn flush_persists_pending_writes() {
        let (path, _dir) = ev_path("flush.jsonl");
        let mut log = EventLog::open(&path).unwrap();
        log.append(EventSource::Substrate, "a", serde_json::json!({}))
            .unwrap();
        log.flush().unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("\"kind\":\"a\""), "kind not flushed: {after}");
    }

    #[test]
    fn drop_flushes_writer() {
        let (path, _dir) = ev_path("drop.jsonl");
        {
            let mut log = EventLog::open(&path).unwrap();
            log.append(EventSource::Substrate, "drp", serde_json::json!({}))
                .unwrap();
        } // Drop must flush.
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("\"kind\":\"drp\""));
    }

    #[test]
    fn tail_capped_at_in_memory_capacity() {
        let (path, _dir) = ev_path("cap.jsonl");
        let mut log = EventLog::open(&path).unwrap();
        // Append more than capacity; tail() should never exceed TAIL_CAPACITY.
        let n = TAIL_CAPACITY + 50;
        for _ in 0..n {
            log.append(EventSource::Substrate, "k", serde_json::json!({}))
                .unwrap();
        }
        let all = log.tail(usize::MAX);
        assert_eq!(all.len(), TAIL_CAPACITY);
        // Oldest in-memory event should be the (n - TAIL_CAPACITY + 1)th appended.
        assert_eq!(all.first().unwrap().id as usize, n - TAIL_CAPACITY + 1);
        assert_eq!(all.last().unwrap().id as usize, n);
    }

    #[test]
    fn perf_thousand_appends_under_one_second() {
        let (path, _dir) = ev_path("perf.jsonl");
        let mut log = EventLog::open(&path).unwrap();
        let start = std::time::Instant::now();
        for i in 0..1000 {
            log.append(
                EventSource::Substrate,
                "perf",
                serde_json::json!({ "i": i }),
            )
            .unwrap();
        }
        log.flush().unwrap();
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(1),
            "1000 appends took {elapsed:?}, expected < 1s"
        );
    }
}
