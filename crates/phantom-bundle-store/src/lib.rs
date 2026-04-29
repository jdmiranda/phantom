//! Unified bundle persistence layer.
//!
//! `phantom-bundle-store` glues together three concerns that have to agree
//! about the same bundle ids:
//!
//! 1. **Structured metadata** in an encrypted SQLite (SQLCipher) database
//!    with an FTS5 index over transcripts, intents, and tags.
//! 2. **Embedding vectors** in a LanceDB columnar store (or, behind the
//!    [`vector_search_disabled`](crate#cargo-features) feature, an
//!    in-memory deterministic stub).
//! 3. **Encrypted frame and audio blobs** sealed with XChaCha20-Poly1305
//!    using a per-bundle data-encryption key derived via HKDF-SHA256 from a
//!    master key stored in the OS keychain.
//!
//! Writes go through a two-phase protocol: SQLite first inside a
//! transaction, then LanceDB. If the LanceDB step fails we roll the SQLite
//! transaction back. If we crash *between* the two, the next start runs
//! [`recovery::sweep_leaked_rows`] which finds bundle ids that are present
//! in SQLite but missing from the vector store and rewrites them.
//!
//! ## Cargo features
//!
//! - `vector_search_disabled` (default): LanceDB is not compiled in; the
//!   in-memory stub is used. The `vector_search_orders_by_cosine` test runs
//!   against the stub. This is the working configuration today.
//! - `vector_search_lancedb`: enables the real LanceDB backend. Currently a
//!   wire-up scaffold — the lance 4.0 + arrow 57 ecosystem is changing
//!   fast, so the search test is `#[ignore]`d under this feature.
//!
//! ## Schema versioning
//!
//! [`STORE_SCHEMA_VERSION`] is bumped any time the SQLite schema changes
//! incompatibly. On open we read the version row from `meta` and compare;
//! a mismatch surfaces as [`StoreError::SchemaVersionMismatch`].

#![allow(clippy::result_large_err)]

use std::path::PathBuf;
#[cfg(any(test, feature = "testing"))]
use std::path::Path;
use std::sync::Mutex;

use phantom_bundles::{Bundle, BundleId};
use phantom_embeddings::Embedding;
use serde::{Deserialize, Serialize};
use thiserror::Error;

mod crypto;
mod lance;
pub mod recovery;
mod sqlite;

pub use crypto::{BlobEnvelope, MasterKey};
pub use lance::{InMemoryVectorIndex, VectorHit, VectorIndex};

/// Schema version of the unified store. Independent of
/// [`phantom_bundles::SCHEMA_VERSION`] — the bundle payload schema and the
/// store's table layout evolve at different cadences.
pub const STORE_SCHEMA_VERSION: u32 = 1;

/// Errors produced by the bundle store.
#[derive(Debug, Error)]
pub enum StoreError {
    /// SQLite / SQLCipher failure.
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// JSON encode/decode failure for stored columns.
    #[error("serde_json: {0}")]
    Serde(#[from] serde_json::Error),

    /// I/O error (paths, blob files).
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// Crypto error during AEAD wrap / unwrap or HKDF derivation.
    #[error("crypto: {0}")]
    Crypto(String),

    /// Master key could not be loaded from the keychain.
    #[error("keyring: {0}")]
    Keyring(String),

    /// Vector index error (LanceDB or stub).
    #[error("vector index: {0}")]
    Vector(String),

    /// SQLite schema version did not match what the build understands.
    #[error("schema version mismatch: expected {expected}, found {found}")]
    SchemaVersionMismatch {
        /// What this build understands.
        expected: u32,
        /// What was actually stored.
        found: u32,
    },

    /// Tried to read a bundle that does not exist.
    #[error("bundle not found: {0}")]
    NotFound(BundleId),

    /// Internal invariant violated (bug / corruption).
    #[error("invariant: {0}")]
    Invariant(String),
}

/// Vector embeddings paired with the modality they were produced under, ready
/// to be indexed alongside a bundle.
#[derive(Debug, Clone)]
pub struct BundleEmbeddings {
    /// Modality string (e.g. `"text"`, `"intent"`). Stored verbatim in the
    /// vector table so a single [`VectorIndex`] can hold mixed modalities.
    pub modality: String,
    /// One embedding per bundle. Multiple modalities can be passed as
    /// separate entries in the same `&[BundleEmbeddings]` slice — they all
    /// commit or roll back atomically with the SQLite write.
    pub embedding: Embedding,
}

/// Search query used by [`BundleStore::search_vectors`].
#[derive(Debug, Clone)]
pub struct VectorQuery {
    /// Modality to search; matched verbatim against [`BundleEmbeddings::modality`].
    pub modality: String,
    /// The query vector. Must match the dimension of stored vectors.
    pub vector: Vec<f32>,
    /// Maximum number of hits to return.
    pub limit: usize,
}

/// Hit produced by FTS5 search over transcripts/intent/tags.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FtsHit {
    /// Bundle id this hit refers to.
    pub bundle_id: BundleId,
    /// Snippet of the matching content (FTS5 `snippet()`).
    pub snippet: String,
    /// FTS5 `bm25` rank — lower is better.
    pub rank: f64,
}

/// Configuration used to open a [`BundleStore`].
#[derive(Debug, Clone)]
pub struct StoreConfig {
    /// Directory that will hold `bundles.db` (SQLite) and `vectors/`
    /// (LanceDB), plus an `objects/` directory for encrypted blob files.
    pub root: PathBuf,
    /// Master key. In production this is loaded from the OS keychain via
    /// [`MasterKey::from_keyring`]. In tests we pass an explicit key.
    pub master_key: MasterKey,
}

/// Top-level handle. Wraps a SQLite (SQLCipher) connection and a vector
/// index behind a single mutex so the public API can be `Send + Sync`.
///
/// The mutex is intentionally coarse: bundle writes are infrequent (one per
/// capture window) and the value of having an unambiguously serial write
/// path here outweighs any throughput we'd gain from finer locking. Tests
/// that exercise concurrent writers rely on this serial behavior.
pub struct BundleStore {
    inner: Mutex<Inner>,
}

impl std::fmt::Debug for BundleStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Avoid touching the mutex (and the master key) so this is safe to
        // log even from inside an error path that already holds the guard.
        f.debug_struct("BundleStore").finish_non_exhaustive()
    }
}

struct Inner {
    sqlite: sqlite::Connection,
    vectors: InMemoryVectorIndex,
    root: PathBuf,
    master_key: MasterKey,
}

impl BundleStore {
    /// Open or create a store at `config.root`.
    ///
    /// On open, the SQLite schema is created if missing and the schema
    /// version row is checked. On a stale leaked-rows trail (bundles in
    /// SQLite without their vector counterparts), [`recovery::sweep_leaked_rows`]
    /// is run synchronously.
    pub fn open(config: StoreConfig) -> Result<Self, StoreError> {
        std::fs::create_dir_all(&config.root)?;
        std::fs::create_dir_all(config.root.join("objects"))?;

        let db_path = config.root.join("bundles.db");
        let sqlite = sqlite::Connection::open_encrypted(&db_path, config.master_key.bytes())?;
        sqlite::init_schema(&sqlite)?;
        sqlite::check_schema_version(&sqlite)?;

        let vectors = InMemoryVectorIndex::default();
        let inner = Inner {
            sqlite,
            vectors,
            root: config.root.clone(),
            master_key: config.master_key.clone(),
        };

        // Best-effort recovery: surface but don't fail open if recovery hits
        // a bundle we can't reseat. The next write will retry.
        let leaked = recovery::sweep_leaked_rows(&inner.sqlite, &inner.vectors);
        if let Err(err) = leaked {
            tracing::warn!(?err, "leaked-rows sweep failed");
        }

        Ok(Self { inner: Mutex::new(inner) })
    }

    /// Persist a bundle and its embeddings atomically.
    ///
    /// Two-phase protocol:
    /// 1. Begin SQLite transaction; insert into `bundles`, `frames`,
    ///    `audio_chunks`, `transcript_words`, `tags`, plus the FTS5 row.
    /// 2. Insert vectors into the vector index.
    /// 3. Commit SQLite if step 2 succeeded; otherwise roll back.
    ///
    /// Frame and audio blob *paths* are stored in clear (relative paths
    /// under the `objects/` dir); the blob *contents* are sealed with
    /// XChaCha20-Poly1305 by [`Self::write_frame_blob`] /
    /// [`Self::write_audio_blob`].
    pub fn write_bundle(
        &self,
        bundle: &Bundle,
        embeddings: &[BundleEmbeddings],
    ) -> Result<(), StoreError> {
        let mut guard = self.inner.lock().expect("bundle store poisoned");
        let inner = &mut *guard;

        let mut tx = inner.sqlite.transaction()?;
        sqlite::insert_bundle(&mut tx, bundle)?;

        let mut indexed = Vec::with_capacity(embeddings.len());
        let mut vector_failed: Option<StoreError> = None;
        for emb in embeddings {
            match inner.vectors.upsert(bundle.id, &emb.modality, &emb.embedding.vec) {
                Ok(()) => indexed.push(emb.modality.clone()),
                Err(err) => {
                    vector_failed = Some(err);
                    break;
                }
            }
        }

        if let Some(err) = vector_failed {
            // Roll back vectors we already wrote, then drop the SQLite tx
            // (which auto-rolls-back on `Drop`).
            for modality in &indexed {
                let _ = inner.vectors.remove(bundle.id, modality);
            }
            return Err(err);
        }

        tx.commit()?;
        Ok(())
    }

    /// Read a bundle back. Returns `NotFound` if the id is unknown.
    pub fn read_bundle(&self, id: BundleId) -> Result<Bundle, StoreError> {
        let guard = self.inner.lock().expect("bundle store poisoned");
        sqlite::read_bundle(&guard.sqlite, id)
    }

    /// Vector similarity search. Backed by [`InMemoryVectorIndex`] under the
    /// default feature, or LanceDB when `vector_search_lancedb` is enabled
    /// (currently scaffolded only — see crate-level docs).
    pub fn search_vectors(&self, query: &VectorQuery) -> Result<Vec<VectorHit>, StoreError> {
        let guard = self.inner.lock().expect("bundle store poisoned");
        guard.vectors.search(&query.modality, &query.vector, query.limit)
    }

    /// FTS5 search over the `transcripts_fts` virtual table.
    ///
    /// `query` is passed verbatim to `MATCH`. Callers can use FTS5 operators
    /// (`AND`, `OR`, `NEAR`) and column filters (`column:term`).
    pub fn search_fts(&self, query: &str, limit: usize) -> Result<Vec<FtsHit>, StoreError> {
        let guard = self.inner.lock().expect("bundle store poisoned");
        sqlite::search_fts(&guard.sqlite, query, limit)
    }

    /// Encrypt and write a frame blob, returning the relative path that
    /// should be stored in [`phantom_bundles::FrameRef::blob_path`].
    pub fn write_frame_blob(
        &self,
        bundle_id: BundleId,
        name: &str,
        plaintext: &[u8],
    ) -> Result<String, StoreError> {
        self.write_blob("frames", bundle_id, name, plaintext)
    }

    /// Encrypt and write an audio blob, returning the relative path.
    pub fn write_audio_blob(
        &self,
        bundle_id: BundleId,
        name: &str,
        plaintext: &[u8],
    ) -> Result<String, StoreError> {
        self.write_blob("audio", bundle_id, name, plaintext)
    }

    /// Read and decrypt a previously-written blob using its relative path.
    pub fn read_blob(&self, bundle_id: BundleId, relpath: &str) -> Result<Vec<u8>, StoreError> {
        let guard = self.inner.lock().expect("bundle store poisoned");
        let abs = guard.root.join(relpath);
        let envelope_bytes = std::fs::read(&abs)?;
        let envelope = BlobEnvelope::from_bytes(&envelope_bytes)
            .map_err(|e| StoreError::Crypto(format!("envelope decode: {e}")))?;
        let dek = guard.master_key.derive_bundle_dek(bundle_id)?;
        let plaintext = crypto::open_blob(&dek, &envelope)
            .map_err(|e| StoreError::Crypto(format!("open: {e}")))?;
        Ok(plaintext)
    }

    /// Returns the path to the SQLite (SQLCipher) database file. Used by
    /// the `encryption_at_rest_is_gibberish` test to scan raw bytes.
    #[must_use]
    pub fn database_path(&self) -> PathBuf {
        let guard = self.inner.lock().expect("bundle store poisoned");
        guard.root.join("bundles.db")
    }

    fn write_blob(
        &self,
        bucket: &str,
        bundle_id: BundleId,
        name: &str,
        plaintext: &[u8],
    ) -> Result<String, StoreError> {
        let guard = self.inner.lock().expect("bundle store poisoned");
        let dek = guard.master_key.derive_bundle_dek(bundle_id)?;
        let envelope = crypto::seal_blob(&dek, plaintext)
            .map_err(|e| StoreError::Crypto(format!("seal: {e}")))?;
        let dir = guard.root.join("objects").join(bucket);
        std::fs::create_dir_all(&dir)?;
        let abs = dir.join(name);
        std::fs::write(&abs, envelope.to_bytes())?;
        let rel = format!("objects/{bucket}/{name}");
        Ok(rel)
    }
}

/// Helpers used in tests and by the recovery module.
#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use super::*;

    /// Construct a master key from raw bytes. Production code should use
    /// [`MasterKey::from_keyring`].
    #[must_use]
    pub fn fixed_master_key(bytes: [u8; 32]) -> MasterKey {
        MasterKey::from_bytes(bytes)
    }

    /// Convenience — a deterministic 32-byte master key seeded from an
    /// integer for tests that don't care about the specific bytes.
    #[must_use]
    pub fn deterministic_master_key(seed: u8) -> MasterKey {
        let mut buf = [0_u8; 32];
        for (i, b) in buf.iter_mut().enumerate() {
            *b = seed.wrapping_add(i as u8);
        }
        MasterKey::from_bytes(buf)
    }

    /// Open a fresh store at a tempdir-style root using `master`.
    pub fn open_at(root: &Path, master: MasterKey) -> Result<BundleStore, StoreError> {
        BundleStore::open(StoreConfig { root: root.to_path_buf(), master_key: master })
    }

    /// Run [`recovery::sweep_leaked_rows`] against the SQLite database at
    /// `db_path` (keyed by `master`) and a caller-supplied in-memory vector
    /// index. Intended for integration tests that need to exercise the sweep
    /// path without going through a full `BundleStore::open`.
    pub fn run_sweep(
        db_path: &std::path::Path,
        master: &MasterKey,
        vectors: &InMemoryVectorIndex,
    ) -> Result<usize, StoreError> {
        let conn = sqlite::Connection::open_encrypted(db_path, master.bytes())?;
        recovery::sweep_leaked_rows(&conn, vectors)
    }

    /// Inject a row into the `leaked_rows` scratchpad table. Used by tests to
    /// simulate the state after a process crash between the vector and SQLite
    /// write phases.
    pub fn inject_leaked_row(
        db_path: &std::path::Path,
        master: &MasterKey,
        bundle_id: BundleId,
        modality: &str,
    ) -> Result<(), StoreError> {
        let conn = sqlite::Connection::open_encrypted(db_path, master.bytes())?;
        sqlite::record_leaked_row(&conn, bundle_id, modality)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use phantom_bundles::{AudioRef, FrameRef, TranscriptWord};
    use phantom_embeddings::Embedding;
    use tempfile::TempDir;

    fn embedding(vec: Vec<f32>) -> Embedding {
        let dim = vec.len();
        Embedding { vec, dim, model: "test".into() }
    }

    fn open_tmp() -> (TempDir, BundleStore) {
        let tmp = TempDir::new().expect("tempdir");
        let key = testing::deterministic_master_key(0xAB);
        let store = testing::open_at(tmp.path(), key).expect("open store");
        (tmp, store)
    }

    fn rich_bundle() -> Bundle {
        let mut b = Bundle::new(42);
        b.t_start_ns = 1_000_000;
        b.t_wall_unix_ms = 1_700_000_000_000;
        b.add_frame(FrameRef {
            t_offset_ns: 0,
            sha: "sha-0".into(),
            blob_path: "frames/0.png".into(),
            dhash: 0,
            width: 1920,
            height: 1080,
        });
        b.add_frame(FrameRef {
            t_offset_ns: 33_000_000,
            sha: "sha-1".into(),
            blob_path: "frames/1.png".into(),
            dhash: 1,
            width: 1920,
            height: 1080,
        });
        b.add_audio(AudioRef {
            t_offset_ns: 0,
            duration_ns: 20_000_000,
            blob_path: "audio/0.opus".into(),
            sample_rate: 48_000,
            channels: 2,
        });
        b.add_word(TranscriptWord {
            t_offset_ns: 0,
            t_end_ns: 1_000_000,
            text: "build".into(),
            speaker: Some("user".into()),
            confidence: 0.9,
        });
        b.add_word(TranscriptWord {
            t_offset_ns: 1_000_001,
            t_end_ns: 2_000_000,
            text: "passing".into(),
            speaker: Some("user".into()),
            confidence: 0.95,
        });
        b.seal(
            Some("ci-success".into()),
            vec!["green".into(), "rust".into()],
            0.85,
        );
        b
    }

    #[test]
    fn roundtrip_bundle_write_read() {
        let (_tmp, store) = open_tmp();
        let original = rich_bundle();
        let emb = BundleEmbeddings {
            modality: "text".into(),
            embedding: embedding(vec![1.0, 0.0, 0.0, 0.0]),
        };
        store.write_bundle(&original, &[emb]).expect("write");
        let restored = store.read_bundle(original.id).expect("read");

        assert_eq!(restored.id, original.id);
        assert_eq!(restored.t_start_ns, original.t_start_ns);
        assert_eq!(restored.t_wall_unix_ms, original.t_wall_unix_ms);
        assert_eq!(restored.source_pane_id, original.source_pane_id);
        assert_eq!(restored.frames.len(), 2);
        assert_eq!(restored.frames[1].t_offset_ns, 33_000_000);
        assert_eq!(restored.audio_chunks.len(), 1);
        assert_eq!(restored.transcript_words.len(), 2);
        assert_eq!(restored.transcript_words[1].text, "passing");
        assert_eq!(restored.intent.as_deref(), Some("ci-success"));
        assert_eq!(restored.tags, vec!["green".to_string(), "rust".to_string()]);
        assert!(restored.sealed);
        assert!((restored.importance - 0.85).abs() < 1e-6);
    }

    #[test]
    fn vector_search_orders_by_cosine() {
        let (_tmp, store) = open_tmp();
        // Three bundles, each with a text vector pointing in a different
        // direction. The query vector points at bundle B; we expect B first.
        let mut b_a = Bundle::new(1);
        b_a.seal(Some("a".into()), vec![], 0.5);
        let mut b_b = Bundle::new(1);
        b_b.seal(Some("b".into()), vec![], 0.5);
        let mut b_c = Bundle::new(1);
        b_c.seal(Some("c".into()), vec![], 0.5);

        store
            .write_bundle(&b_a, &[BundleEmbeddings {
                modality: "text".into(),
                embedding: embedding(vec![1.0, 0.0, 0.0]),
            }])
            .unwrap();
        store
            .write_bundle(&b_b, &[BundleEmbeddings {
                modality: "text".into(),
                embedding: embedding(vec![0.0, 1.0, 0.0]),
            }])
            .unwrap();
        store
            .write_bundle(&b_c, &[BundleEmbeddings {
                modality: "text".into(),
                embedding: embedding(vec![0.0, 0.0, 1.0]),
            }])
            .unwrap();

        let hits = store
            .search_vectors(&VectorQuery {
                modality: "text".into(),
                vector: vec![0.0, 1.0, 0.0],
                limit: 3,
            })
            .expect("search");

        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].bundle_id, b_b.id, "B should be the closest");
        assert!(hits[0].similarity >= hits[1].similarity);
        assert!(hits[1].similarity >= hits[2].similarity);
        assert!((hits[0].similarity - 1.0).abs() < 1e-6);
    }

    #[test]
    fn fts5_finds_transcript_word() {
        let (_tmp, store) = open_tmp();
        let bundle = rich_bundle();
        let emb = BundleEmbeddings {
            modality: "text".into(),
            embedding: embedding(vec![1.0, 0.0]),
        };
        store.write_bundle(&bundle, &[emb]).unwrap();

        let hits = store.search_fts("passing", 10).expect("fts");
        assert!(!hits.is_empty(), "expected at least one fts hit");
        assert!(hits.iter().any(|h| h.bundle_id == bundle.id));
    }

    #[test]
    fn fts5_finds_intent_and_tags() {
        let (_tmp, store) = open_tmp();
        let bundle = rich_bundle();
        let emb = BundleEmbeddings {
            modality: "text".into(),
            embedding: embedding(vec![1.0]),
        };
        store.write_bundle(&bundle, &[emb]).unwrap();

        let intent_hits = store.search_fts("ci-success", 10).expect("fts");
        assert!(intent_hits.iter().any(|h| h.bundle_id == bundle.id), "intent");

        let tag_hits = store.search_fts("green", 10).expect("fts");
        assert!(tag_hits.iter().any(|h| h.bundle_id == bundle.id), "tag");
    }

    #[test]
    fn encryption_at_rest_is_gibberish() {
        // Write a bundle with a recognizable plaintext token; then read the
        // raw on-disk bytes of the SQLite file and confirm the token does
        // not appear anywhere. This is the SQLCipher contract.
        let tmp = TempDir::new().expect("tempdir");
        let key = testing::deterministic_master_key(0xAB);
        {
            let store = testing::open_at(tmp.path(), key).expect("open");
            let mut bundle = Bundle::new(7);
            bundle.add_word(TranscriptWord {
                t_offset_ns: 0,
                t_end_ns: 1,
                text: "MAGIC_PLAINTEXT_TOKEN_YDZQ".into(),
                speaker: None,
                confidence: 1.0,
            });
            bundle.seal(Some("enc-test".into()), vec!["scan".into()], 0.5);
            let emb = BundleEmbeddings {
                modality: "text".into(),
                embedding: embedding(vec![1.0]),
            };
            store.write_bundle(&bundle, &[emb]).unwrap();
        } // drop store -> close db -> flush

        let db_bytes = std::fs::read(tmp.path().join("bundles.db")).expect("read db");
        let needle = b"MAGIC_PLAINTEXT_TOKEN_YDZQ";
        let found = db_bytes.windows(needle.len()).any(|w| w == needle);
        assert!(!found, "plaintext token leaked into encrypted SQLite file");
        assert!(db_bytes.len() > needle.len() * 2);
    }

    #[test]
    fn atomicity_sqlite_failure_rolls_back_lance() {
        // We exercise the inverse direction: a failed vector upsert must
        // roll back the SQLite insert. We force a vector failure by writing
        // a vector with a mismatched dim against an existing entry for the
        // same modality.
        let (_tmp, store) = open_tmp();
        let mut a = Bundle::new(1);
        a.seal(Some("first".into()), vec![], 0.1);
        store
            .write_bundle(&a, &[BundleEmbeddings {
                modality: "text".into(),
                embedding: embedding(vec![1.0, 2.0, 3.0]),
            }])
            .expect("first write ok");

        let mut b = Bundle::new(1);
        b.seal(Some("second".into()), vec![], 0.2);
        let err = store
            .write_bundle(
                &b,
                &[BundleEmbeddings {
                    modality: "text".into(),
                    embedding: embedding(vec![1.0, 2.0]),
                }],
            )
            .expect_err("second write must fail dim check");
        assert!(matches!(err, StoreError::Vector(_)));

        // Bundle B must NOT be in SQLite — the transaction was rolled back.
        let read_err = store.read_bundle(b.id).expect_err("must be NotFound");
        assert!(matches!(read_err, StoreError::NotFound(_)));

        // Bundle A is still readable.
        let read_a = store.read_bundle(a.id).expect("first still there");
        assert_eq!(read_a.id, a.id);
    }

    #[test]
    fn concurrent_writers_no_corruption() {
        use std::sync::Arc;
        use std::thread;

        let tmp = TempDir::new().expect("tempdir");
        let key = testing::deterministic_master_key(0xAB);
        let store = Arc::new(testing::open_at(tmp.path(), key.clone()).expect("open"));

        let mut joins = Vec::new();
        for n in 0..8_u32 {
            let store = Arc::clone(&store);
            joins.push(thread::spawn(move || {
                let mut bundle = Bundle::new(u64::from(n));
                bundle.t_wall_unix_ms = 1_700_000_000_000 + i64::from(n);
                bundle.add_word(TranscriptWord {
                    t_offset_ns: 0,
                    t_end_ns: 1,
                    text: format!("worker_{n}"),
                    speaker: None,
                    confidence: 1.0,
                });
                bundle.seal(Some(format!("intent-{n}")), vec![format!("tag-{n}")], 0.5);
                let emb = BundleEmbeddings {
                    modality: "text".into(),
                    embedding: Embedding {
                        vec: vec![f32::from(i16::from(n as i16)) / 8.0, 0.5, 0.25],
                        dim: 3,
                        model: "test".into(),
                    },
                };
                store.write_bundle(&bundle, &[emb]).expect("concurrent write");
                bundle.id
            }));
        }
        let ids: Vec<BundleId> = joins.into_iter().map(|h| h.join().expect("join")).collect();

        // Drop the store and re-open from disk to confirm everything was
        // committed durably.
        drop(store);
        let reopened = testing::open_at(tmp.path(), key).expect("reopen");
        for id in ids {
            let _ = reopened.read_bundle(id).expect("each id must round-trip");
        }
    }

    #[test]
    fn schema_version_mismatch_surfaces() {
        // Open once to create the schema, then tamper with the meta row and
        // confirm the next open surfaces SchemaVersionMismatch.
        let tmp = TempDir::new().unwrap();
        let key = testing::deterministic_master_key(0x11);
        {
            let _store = testing::open_at(tmp.path(), key.clone()).expect("first open");
        }

        let db_path = tmp.path().join("bundles.db");
        let conn = sqlite::Connection::open_encrypted(&db_path, key.bytes()).unwrap();
        conn.exec("UPDATE meta SET value = '999' WHERE key = 'schema_version'")
            .unwrap();
        drop(conn);

        let err = testing::open_at(tmp.path(), key).expect_err("must mismatch");
        assert!(matches!(
            err,
            StoreError::SchemaVersionMismatch { expected: 1, found: 999 }
        ));
    }
}
