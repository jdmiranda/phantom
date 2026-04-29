//! SQLite (SQLCipher) connection wrapper, schema, and CRUD.
//!
//! Why a wrapper: the public `BundleStore` interface needs a tiny, opinionated
//! surface — open with a passphrase, run a transaction, read a bundle, run an
//! FTS query. Wrapping `rusqlite::Connection` keeps that surface in one place
//! and gives us a single hook to add encryption-at-rest pragmas.
//!
//! Schema (v1):
//!
//! - `meta(key TEXT PRIMARY KEY, value TEXT)` — schema version + bookkeeping.
//! - `bundles` — one row per bundle. Stores `tags` as a JSON array.
//! - `frames`, `audio_chunks`, `transcript_words` — child rows by bundle id.
//! - `transcripts_fts` (FTS5, contentless) — full-text over transcript words,
//!   intent, and tags. Rebuilt synchronously in [`insert_bundle`].
//! - `leaked_rows(bundle_id, modality)` — recovery scratchpad. Inserted when
//!   a vector upsert fails after the SQLite tx already committed (only
//!   relevant if the protocol changes; today the SQLite tx is committed
//!   *after* the vector upsert succeeds).

use phantom_bundles::{AudioRef, Bundle, BundleId, FrameRef, TranscriptWord};
use rusqlite::OptionalExtension;
use rusqlite::params;

use crate::{FtsHit, STORE_SCHEMA_VERSION, StoreError};

/// Wrapped connection. `Send` because `rusqlite::Connection` is `Send`.
/// The wrapper is intentionally not `Sync`; thread-safety is provided by the
/// outer `Mutex<Inner>` in `lib.rs`.
pub struct Connection {
    inner: rusqlite::Connection,
}

impl Connection {
    /// Open a SQLCipher-encrypted database at `path` keyed by `key`.
    pub fn open_encrypted(path: &std::path::Path, key: &[u8; 32]) -> Result<Self, StoreError> {
        let inner = rusqlite::Connection::open(path)?;
        // SQLCipher takes the raw key as a hex literal of the form
        // `x'aabbcc...'`. This avoids any UTF-8 sensitivity in `PRAGMA key`.
        let hex = hex_encode(key);
        let pragma = format!("PRAGMA key = \"x'{hex}'\";");
        inner.execute_batch(&pragma)?;
        // Sensible defaults: WAL for crash safety + concurrent reads,
        // foreign keys for child-table consistency.
        inner.execute_batch(
            "PRAGMA journal_mode = WAL; \
             PRAGMA synchronous = NORMAL; \
             PRAGMA foreign_keys = ON;",
        )?;
        Ok(Self { inner })
    }

    /// Start a write transaction. Auto-rolls-back on `Drop` unless
    /// [`Transaction::commit`] is called.
    pub fn transaction(&mut self) -> Result<Transaction<'_>, StoreError> {
        let tx = self.inner.transaction()?;
        Ok(Transaction { inner: tx })
    }

    /// Execute a single DML statement. Used by tests to tamper with the meta
    /// row when forcing a schema-version mismatch.
    pub fn exec(&self, sql: &str) -> Result<(), StoreError> {
        self.inner.execute_batch(sql)?;
        Ok(())
    }

    fn raw(&self) -> &rusqlite::Connection {
        &self.inner
    }
}

/// Active write transaction.
pub struct Transaction<'a> {
    inner: rusqlite::Transaction<'a>,
}

impl<'a> Transaction<'a> {
    /// Commit the transaction.
    pub fn commit(self) -> Result<(), StoreError> {
        self.inner.commit()?;
        Ok(())
    }

    fn raw(&self) -> &rusqlite::Connection {
        &self.inner
    }
}

// ---------------------------------------------------------------------------
// Schema management
// ---------------------------------------------------------------------------

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS bundles (
    id              TEXT PRIMARY KEY,
    t_start_ns      INTEGER NOT NULL,
    t_wall_unix_ms  INTEGER NOT NULL,
    source_pane_id  INTEGER NOT NULL,
    intent          TEXT,
    tags_json       TEXT NOT NULL DEFAULT '[]',
    importance      REAL NOT NULL DEFAULT 0.0,
    sealed          INTEGER NOT NULL DEFAULT 0,
    schema_version  INTEGER NOT NULL DEFAULT 1
);

CREATE INDEX IF NOT EXISTS idx_bundles_pane ON bundles(source_pane_id);
CREATE INDEX IF NOT EXISTS idx_bundles_t_wall ON bundles(t_wall_unix_ms);

CREATE TABLE IF NOT EXISTS frames (
    bundle_id    TEXT NOT NULL REFERENCES bundles(id) ON DELETE CASCADE,
    seq          INTEGER NOT NULL,
    t_offset_ns  INTEGER NOT NULL,
    sha          TEXT NOT NULL,
    blob_path    TEXT NOT NULL,
    dhash        INTEGER NOT NULL,
    width        INTEGER NOT NULL,
    height       INTEGER NOT NULL,
    PRIMARY KEY (bundle_id, seq)
);

CREATE TABLE IF NOT EXISTS audio_chunks (
    bundle_id    TEXT NOT NULL REFERENCES bundles(id) ON DELETE CASCADE,
    seq          INTEGER NOT NULL,
    t_offset_ns  INTEGER NOT NULL,
    duration_ns  INTEGER NOT NULL,
    blob_path    TEXT NOT NULL,
    sample_rate  INTEGER NOT NULL,
    channels     INTEGER NOT NULL,
    PRIMARY KEY (bundle_id, seq)
);

CREATE TABLE IF NOT EXISTS transcript_words (
    bundle_id    TEXT NOT NULL REFERENCES bundles(id) ON DELETE CASCADE,
    seq          INTEGER NOT NULL,
    t_offset_ns  INTEGER NOT NULL,
    t_end_ns     INTEGER NOT NULL,
    text         TEXT NOT NULL,
    speaker      TEXT,
    confidence   REAL NOT NULL,
    PRIMARY KEY (bundle_id, seq)
);

CREATE VIRTUAL TABLE IF NOT EXISTS transcripts_fts USING fts5(
    bundle_id UNINDEXED,
    body,
    intent,
    tags,
    tokenize = 'unicode61'
);

CREATE TABLE IF NOT EXISTS leaked_rows (
    bundle_id  TEXT NOT NULL,
    modality   TEXT NOT NULL,
    logged_at  INTEGER NOT NULL,
    PRIMARY KEY (bundle_id, modality)
);
"#;

/// Create tables if missing, and stamp the schema version on first run.
pub fn init_schema(conn: &Connection) -> Result<(), StoreError> {
    conn.raw().execute_batch(SCHEMA_SQL)?;
    conn.raw().execute(
        "INSERT OR IGNORE INTO meta (key, value) VALUES ('schema_version', ?1)",
        params![STORE_SCHEMA_VERSION.to_string()],
    )?;
    Ok(())
}

/// Verify the stored schema version matches what this build understands.
pub fn check_schema_version(conn: &Connection) -> Result<(), StoreError> {
    let raw: Option<String> = conn
        .raw()
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let Some(s) = raw else {
        return Err(StoreError::Invariant(
            "meta.schema_version row missing".into(),
        ));
    };
    let found: u32 = s
        .parse()
        .map_err(|e| StoreError::Invariant(format!("bad schema_version {s:?}: {e}")))?;
    if found != STORE_SCHEMA_VERSION {
        return Err(StoreError::SchemaVersionMismatch {
            expected: STORE_SCHEMA_VERSION,
            found,
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Bundle CRUD
// ---------------------------------------------------------------------------

/// Insert a complete bundle (parent + all children + FTS row) inside `tx`.
pub fn insert_bundle(tx: &mut Transaction<'_>, bundle: &Bundle) -> Result<(), StoreError> {
    let conn = tx.raw();
    let id_str = bundle.id.to_string();
    let tags_json = serde_json::to_string(&bundle.tags)?;

    conn.execute(
        "INSERT OR REPLACE INTO bundles \
            (id, t_start_ns, t_wall_unix_ms, source_pane_id, intent, tags_json, importance, sealed, schema_version) \
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            id_str,
            i64_from_u64(bundle.t_start_ns),
            bundle.t_wall_unix_ms,
            i64_from_u64(bundle.source_pane_id),
            bundle.intent,
            tags_json,
            f64::from(bundle.importance),
            i32::from(bundle.sealed),
            bundle.schema_version,
        ],
    )?;

    // Re-insert children: delete-then-insert guarantees idempotence on
    // `INSERT OR REPLACE` of the parent.
    conn.execute("DELETE FROM frames WHERE bundle_id = ?1", params![id_str])?;
    for (seq, frame) in bundle.frames.iter().enumerate() {
        insert_frame(conn, &id_str, seq as i64, frame)?;
    }

    conn.execute(
        "DELETE FROM audio_chunks WHERE bundle_id = ?1",
        params![id_str],
    )?;
    for (seq, audio) in bundle.audio_chunks.iter().enumerate() {
        insert_audio(conn, &id_str, seq as i64, audio)?;
    }

    conn.execute(
        "DELETE FROM transcript_words WHERE bundle_id = ?1",
        params![id_str],
    )?;
    for (seq, word) in bundle.transcript_words.iter().enumerate() {
        insert_word(conn, &id_str, seq as i64, word)?;
    }

    // Refresh the FTS row.
    conn.execute(
        "DELETE FROM transcripts_fts WHERE bundle_id = ?1",
        params![id_str],
    )?;
    let body = bundle
        .transcript_words
        .iter()
        .map(|w| w.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let tags_text = bundle.tags.join(" ");
    conn.execute(
        "INSERT INTO transcripts_fts (bundle_id, body, intent, tags) VALUES (?1, ?2, ?3, ?4)",
        params![
            id_str,
            body,
            bundle.intent.as_deref().unwrap_or(""),
            tags_text
        ],
    )?;

    Ok(())
}

fn insert_frame(
    conn: &rusqlite::Connection,
    id: &str,
    seq: i64,
    f: &FrameRef,
) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO frames (bundle_id, seq, t_offset_ns, sha, blob_path, dhash, width, height) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            id,
            seq,
            i64_from_u64(f.t_offset_ns),
            f.sha,
            f.blob_path,
            i64_from_u64(f.dhash),
            f.width,
            f.height,
        ],
    )?;
    Ok(())
}

fn insert_audio(
    conn: &rusqlite::Connection,
    id: &str,
    seq: i64,
    a: &AudioRef,
) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO audio_chunks (bundle_id, seq, t_offset_ns, duration_ns, blob_path, sample_rate, channels) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            id,
            seq,
            i64_from_u64(a.t_offset_ns),
            i64_from_u64(a.duration_ns),
            a.blob_path,
            a.sample_rate,
            a.channels,
        ],
    )?;
    Ok(())
}

fn insert_word(
    conn: &rusqlite::Connection,
    id: &str,
    seq: i64,
    w: &TranscriptWord,
) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO transcript_words (bundle_id, seq, t_offset_ns, t_end_ns, text, speaker, confidence) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            id,
            seq,
            i64_from_u64(w.t_offset_ns),
            i64_from_u64(w.t_end_ns),
            w.text,
            w.speaker,
            f64::from(w.confidence),
        ],
    )?;
    Ok(())
}

/// Fetch a bundle by id, reconstructing children in stored insertion order.
pub fn read_bundle(conn: &Connection, id: BundleId) -> Result<Bundle, StoreError> {
    let id_str = id.to_string();
    let raw = conn.raw();

    let row = raw
        .query_row(
            "SELECT t_start_ns, t_wall_unix_ms, source_pane_id, intent, tags_json, importance, sealed, schema_version \
             FROM bundles WHERE id = ?1",
            params![id_str],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, f64>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, i64>(7)?,
                ))
            },
        )
        .optional()?;
    let Some((
        t_start_ns,
        t_wall_unix_ms,
        source_pane_id,
        intent,
        tags_json,
        importance,
        sealed,
        schema_version,
    )) = row
    else {
        return Err(StoreError::NotFound(id));
    };

    let tags: Vec<String> = serde_json::from_str(&tags_json)?;

    let mut bundle = Bundle::new(u64_from_i64(source_pane_id));
    bundle.id = id;
    bundle.t_start_ns = u64_from_i64(t_start_ns);
    bundle.t_wall_unix_ms = t_wall_unix_ms;
    bundle.intent = intent;
    bundle.tags = tags;
    bundle.importance = importance as f32;
    bundle.sealed = sealed != 0;
    bundle.schema_version = u32::try_from(schema_version)
        .map_err(|_| StoreError::Invariant(format!("bad schema_version {schema_version}")))?;

    // Children
    let mut frame_stmt = raw.prepare(
        "SELECT t_offset_ns, sha, blob_path, dhash, width, height FROM frames \
         WHERE bundle_id = ?1 ORDER BY seq ASC",
    )?;
    let frame_rows = frame_stmt
        .query_map(params![id_str], |row| {
            Ok(FrameRef {
                t_offset_ns: u64_from_i64(row.get::<_, i64>(0)?),
                sha: row.get::<_, String>(1)?,
                blob_path: row.get::<_, String>(2)?,
                dhash: u64_from_i64(row.get::<_, i64>(3)?),
                width: row.get::<_, u32>(4)?,
                height: row.get::<_, u32>(5)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    bundle.frames = frame_rows;

    let mut audio_stmt = raw.prepare(
        "SELECT t_offset_ns, duration_ns, blob_path, sample_rate, channels FROM audio_chunks \
         WHERE bundle_id = ?1 ORDER BY seq ASC",
    )?;
    let audio_rows = audio_stmt
        .query_map(params![id_str], |row| {
            Ok(AudioRef {
                t_offset_ns: u64_from_i64(row.get::<_, i64>(0)?),
                duration_ns: u64_from_i64(row.get::<_, i64>(1)?),
                blob_path: row.get::<_, String>(2)?,
                sample_rate: row.get::<_, u32>(3)?,
                channels: row.get::<_, u16>(4)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    bundle.audio_chunks = audio_rows;

    let mut word_stmt = raw.prepare(
        "SELECT t_offset_ns, t_end_ns, text, speaker, confidence FROM transcript_words \
         WHERE bundle_id = ?1 ORDER BY seq ASC",
    )?;
    let word_rows = word_stmt
        .query_map(params![id_str], |row| {
            Ok(TranscriptWord {
                t_offset_ns: u64_from_i64(row.get::<_, i64>(0)?),
                t_end_ns: u64_from_i64(row.get::<_, i64>(1)?),
                text: row.get::<_, String>(2)?,
                speaker: row.get::<_, Option<String>>(3)?,
                confidence: row.get::<_, f64>(4)? as f32,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    bundle.transcript_words = word_rows;

    Ok(bundle)
}

/// FTS5 search over `transcripts_fts`. Returns the bundle id, an FTS5
/// snippet, and the bm25 rank (lower = more relevant).
///
/// Simple queries (those with no FTS5 metacharacters) are auto-wrapped in
/// double quotes so callers can pass tokens like `"ci-success"` literally
/// without colliding with FTS5's `-` (NOT) or `:` (column filter) operators.
/// Queries containing any of `"():^*` are passed through verbatim so power
/// users can still write FTS5 expressions.
pub fn search_fts(conn: &Connection, query: &str, limit: usize) -> Result<Vec<FtsHit>, StoreError> {
    let prepared = prepare_fts_query(query);
    let raw = conn.raw();
    let mut stmt = raw.prepare(
        "SELECT bundle_id, snippet(transcripts_fts, -1, '<', '>', '...', 16), bm25(transcripts_fts) \
         FROM transcripts_fts WHERE transcripts_fts MATCH ?1 \
         ORDER BY bm25(transcripts_fts) LIMIT ?2",
    )?;
    let rows = stmt
        .query_map(
            params![prepared, i64::try_from(limit).unwrap_or(i64::MAX)],
            |row| {
                let id_str: String = row.get(0)?;
                let snippet: String = row.get(1)?;
                let rank: f64 = row.get(2)?;
                Ok((id_str, snippet, rank))
            },
        )?
        .collect::<Result<Vec<_>, _>>()?;
    let mut out = Vec::with_capacity(rows.len());
    for (id_str, snippet, rank) in rows {
        let bundle_id = uuid::Uuid::parse_str(&id_str)
            .map_err(|e| StoreError::Invariant(format!("bad uuid {id_str:?}: {e}")))?;
        out.push(FtsHit {
            bundle_id,
            snippet,
            rank,
        });
    }
    Ok(out)
}

/// Wrap a single-token-or-phrase query in FTS5 phrase quoting if it does
/// not already contain any FTS5 syntax characters. Phrase quoting makes
/// the query safe for tokens like `"ci-success"` that would otherwise be
/// chopped on the dash by `unicode61`.
fn prepare_fts_query(query: &str) -> String {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let has_meta = trimmed
        .chars()
        .any(|c| matches!(c, '"' | '(' | ')' | ':' | '^' | '*'));
    let has_operator_word = trimmed
        .split_whitespace()
        .any(|w| matches!(w, "AND" | "OR" | "NOT" | "NEAR"));
    if has_meta || has_operator_word {
        return trimmed.to_string();
    }
    // Phrase-quote each whitespace-delimited token so internal punctuation
    // (e.g. `-`) doesn't get reinterpreted as an FTS5 operator. This keeps
    // multi-word queries working as implicit-AND.
    let parts: Vec<String> = trimmed
        .split_whitespace()
        .map(|w| {
            // Escape any embedded double-quotes per FTS5 rules: doubled.
            let escaped = w.replace('"', "\"\"");
            format!("\"{escaped}\"")
        })
        .collect();
    parts.join(" ")
}

/// Iterate over all bundle ids currently stored. Used by recovery.
pub fn all_bundle_ids(conn: &Connection) -> Result<Vec<BundleId>, StoreError> {
    let raw = conn.raw();
    let mut stmt = raw.prepare("SELECT id FROM bundles")?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut out = Vec::with_capacity(rows.len());
    for s in rows {
        out.push(
            uuid::Uuid::parse_str(&s)
                .map_err(|e| StoreError::Invariant(format!("bad uuid {s:?}: {e}")))?,
        );
    }
    Ok(out)
}

/// Append a leaked-row record for the recovery sweep to consume next start.
///
/// Unused under the in-memory vector backend (the sweep clears the table
/// wholesale on open) but kept on the public surface so a future durable
/// vector backend has somewhere to record the half-state.
#[allow(dead_code)]
pub fn record_leaked_row(
    conn: &Connection,
    bundle_id: BundleId,
    modality: &str,
) -> Result<(), StoreError> {
    let raw = conn.raw();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    raw.execute(
        "INSERT OR REPLACE INTO leaked_rows (bundle_id, modality, logged_at) VALUES (?1, ?2, ?3)",
        params![bundle_id.to_string(), modality, now],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0F) as usize] as char);
    }
    out
}

/// Bit-cast u64 to i64 (SQLite has no unsigned). Lossless and reversible.
fn i64_from_u64(v: u64) -> i64 {
    v as i64
}

fn u64_from_i64(v: i64) -> u64 {
    v as u64
}
