//! In-memory embedding store with cosine-similarity vector query.
//!
//! [`EmbeddingStore`] is the stable trait callers depend on. The initial
//! backend is [`InMemoryStore`], which satisfies the persistence contract
//! within a session. A SQLite/LanceDB backend can drop in behind the same
//! trait later (tracked in phantom issue #10).
//!
//! # Design notes
//!
//! * All struct fields are private; construction is through named constructors.
//! * No `.unwrap()` in production code paths.
//! * `insert` is idempotent: re-inserting the same `id` overwrites the record.
//! * `query` returns at most `k` results (capped at [`MAX_QUERY_RESULTS`]).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::cosine_similarity;

/// Hard ceiling on the number of hits returned by [`EmbeddingStore::query`].
pub const MAX_QUERY_RESULTS: usize = 100;

// ── Error ─────────────────────────────────────────────────────────────────────

/// Errors produced by [`EmbeddingStore`] implementations.
#[derive(Debug, Error)]
pub enum StoreError {
    /// A vector with a different dimensionality was inserted into a store that
    /// has already fixed its expected dimensionality.
    #[error("dimension mismatch: store expects {expected}, got {actual}")]
    DimensionMismatch { expected: usize, actual: usize },

    /// The requested record does not exist.
    #[error("record not found: {0}")]
    NotFound(Uuid),

    /// Any backend-level I/O or serialization error.
    #[error("store backend error: {0}")]
    Backend(String),
}

// ── Types ─────────────────────────────────────────────────────────────────────

/// Arbitrary key-value metadata attached to a stored embedding.
pub type Metadata = HashMap<String, serde_json::Value>;

/// A single stored record.
///
/// All fields are private; use the constructor and accessor methods.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingRecord {
    id: Uuid,
    vector: Vec<f32>,
    metadata: Metadata,
}

impl EmbeddingRecord {
    /// Create a new record.
    #[must_use]
    pub fn new(id: Uuid, vector: Vec<f32>, metadata: Metadata) -> Self {
        Self {
            id,
            vector,
            metadata,
        }
    }

    /// The unique identifier for this record.
    #[must_use]
    pub fn id(&self) -> Uuid {
        self.id
    }

    /// The raw embedding vector.
    #[must_use]
    pub fn vector(&self) -> &[f32] {
        &self.vector
    }

    /// Metadata key-value pairs.
    #[must_use]
    pub fn metadata(&self) -> &Metadata {
        &self.metadata
    }
}

/// A single hit returned by [`EmbeddingStore::query`].
///
/// Scores are cosine similarities in `[-1.0, 1.0]`, ordered highest first.
#[derive(Debug, Clone)]
pub struct QueryHit {
    id: Uuid,
    score: f32,
    metadata: Metadata,
}

impl QueryHit {
    /// The id of the matching record.
    #[must_use]
    pub fn id(&self) -> Uuid {
        self.id
    }

    /// Cosine similarity score (`-1.0 .. 1.0`).
    #[must_use]
    pub fn score(&self) -> f32 {
        self.score
    }

    /// Metadata attached to the matching record.
    #[must_use]
    pub fn metadata(&self) -> &Metadata {
        &self.metadata
    }
}

/// Filter applied during [`EmbeddingStore::query`].
///
/// All fields are optional — omitting a field means "no constraint on this
/// dimension". Fields that are present must ALL match (logical AND).
#[derive(Debug, Clone, Default)]
pub struct QueryFilter {
    /// Only return records whose metadata contains `key` equal to `value`.
    pub metadata_eq: Option<(String, serde_json::Value)>,
}

// ── Trait ─────────────────────────────────────────────────────────────────────

/// Persistent embedding store.
///
/// The trait is intentionally minimal so SQLite and LanceDB backends can
/// implement it without exposing backend-specific concerns to callers.
pub trait EmbeddingStore: Send + Sync {
    /// Insert or overwrite the record with the given `id`.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::DimensionMismatch`] if the store has already fixed
    /// a dimensionality different from `vector.len()`.
    fn insert(
        &mut self,
        id: Uuid,
        vector: Vec<f32>,
        metadata: Metadata,
    ) -> Result<(), StoreError>;

    /// Retrieve the record with the given `id`.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::NotFound`] if no record with that id exists.
    fn get(&self, id: Uuid) -> Result<EmbeddingRecord, StoreError>;

    /// Return all records that match `filter`.
    ///
    /// Returns an empty `Vec` (not an error) when nothing matches.
    fn scan_filter(&self, filter: &QueryFilter) -> Vec<EmbeddingRecord>;

    /// Return the `k` nearest records to `query_vec` by cosine similarity,
    /// optionally narrowed by `filter`.
    ///
    /// `k` is silently clamped to [`MAX_QUERY_RESULTS`].
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::DimensionMismatch`] if `query_vec.len()` differs
    /// from the store's fixed dimensionality.
    fn query(
        &self,
        query_vec: &[f32],
        k: usize,
        filter: Option<&QueryFilter>,
    ) -> Result<Vec<QueryHit>, StoreError>;

    /// Number of records currently in the store.
    fn len(&self) -> usize;

    /// Whether the store is empty.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

// ── In-memory backend ─────────────────────────────────────────────────────────

/// Pure in-memory [`EmbeddingStore`] with O(n) cosine-similarity scan.
///
/// Suitable for unit tests and single-session use. Satisfies the persistence
/// contract _within_ a session; data is lost when the process exits. Migrate
/// to the SQLite or LanceDB backend (issue #10) for cross-session recall.
///
/// Dimensionality is fixed on the first `insert` call; subsequent inserts with
/// a different length return [`StoreError::DimensionMismatch`].
pub struct InMemoryStore {
    records: HashMap<Uuid, EmbeddingRecord>,
    fixed_dim: Option<usize>,
}

impl InMemoryStore {
    /// Create an empty store with no fixed dimensionality yet.
    #[must_use]
    pub fn new() -> Self {
        Self {
            records: HashMap::new(),
            fixed_dim: None,
        }
    }

    /// Create an empty store with a pre-declared dimensionality.
    ///
    /// All inserts must supply vectors of exactly `dim` elements.
    #[must_use]
    pub fn with_dim(dim: usize) -> Self {
        Self {
            records: HashMap::new(),
            fixed_dim: Some(dim),
        }
    }
}

impl Default for InMemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

impl EmbeddingStore for InMemoryStore {
    fn insert(
        &mut self,
        id: Uuid,
        vector: Vec<f32>,
        metadata: Metadata,
    ) -> Result<(), StoreError> {
        match self.fixed_dim {
            Some(expected) if vector.len() != expected => {
                return Err(StoreError::DimensionMismatch {
                    expected,
                    actual: vector.len(),
                });
            }
            None => {
                // Fix the dimensionality on the first insert.
                self.fixed_dim = Some(vector.len());
            }
            Some(_) => {}
        }

        self.records
            .insert(id, EmbeddingRecord::new(id, vector, metadata));
        Ok(())
    }

    fn get(&self, id: Uuid) -> Result<EmbeddingRecord, StoreError> {
        self.records
            .get(&id)
            .cloned()
            .ok_or(StoreError::NotFound(id))
    }

    fn scan_filter(&self, filter: &QueryFilter) -> Vec<EmbeddingRecord> {
        self.records
            .values()
            .filter(|r| record_matches(r, filter))
            .cloned()
            .collect()
    }

    fn query(
        &self,
        query_vec: &[f32],
        k: usize,
        filter: Option<&QueryFilter>,
    ) -> Result<Vec<QueryHit>, StoreError> {
        if let Some(expected) = self.fixed_dim {
            if query_vec.len() != expected {
                return Err(StoreError::DimensionMismatch {
                    expected,
                    actual: query_vec.len(),
                });
            }
        }

        let k = k.min(MAX_QUERY_RESULTS);

        let mut hits: Vec<QueryHit> = self
            .records
            .values()
            .filter(|r| {
                filter
                    .map(|f| record_matches(r, f))
                    .unwrap_or(true)
            })
            .map(|r| {
                let score = cosine_similarity(query_vec, &r.vector);
                QueryHit {
                    id: r.id,
                    score,
                    metadata: r.metadata.clone(),
                }
            })
            .collect();

        // Sort descending by score; break ties by id for determinism.
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.id.cmp(&b.id))
        });

        hits.truncate(k);
        Ok(hits)
    }

    fn len(&self) -> usize {
        self.records.len()
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn record_matches(record: &EmbeddingRecord, filter: &QueryFilter) -> bool {
    if let Some((ref key, ref value)) = filter.metadata_eq {
        match record.metadata.get(key) {
            Some(v) => v == value,
            None => false,
        }
    } else {
        true
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn uuid(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    fn meta(key: &str, val: &str) -> Metadata {
        [(key.to_string(), json!(val))].into_iter().collect()
    }

    fn unit_vec(dim: usize, hot: usize) -> Vec<f32> {
        let mut v = vec![0.0f32; dim];
        v[hot] = 1.0;
        v
    }

    // ── insert / get round-trip ────────────────────────────────────────────

    #[test]
    fn insert_get_round_trip() {
        let mut store = InMemoryStore::new();
        let id = uuid(1);
        let vec = vec![1.0, 2.0, 3.0];
        let meta = meta("kind", "text");

        store.insert(id, vec.clone(), meta.clone()).expect("insert");
        let rec = store.get(id).expect("get");

        assert_eq!(rec.id(), id);
        assert_eq!(rec.vector(), vec.as_slice());
        assert_eq!(rec.metadata()["kind"], json!("text"));
    }

    #[test]
    fn insert_is_idempotent_overwrites() {
        let mut store = InMemoryStore::new();
        let id = uuid(2);

        store
            .insert(id, vec![1.0, 0.0], meta("v", "first"))
            .expect("first");
        store
            .insert(id, vec![0.0, 1.0], meta("v", "second"))
            .expect("second");

        let rec = store.get(id).expect("get");
        assert_eq!(rec.vector(), &[0.0_f32, 1.0]);
        assert_eq!(rec.metadata()["v"], json!("second"));
        assert_eq!(store.len(), 1, "idempotent insert must not add a new row");
    }

    #[test]
    fn get_missing_returns_not_found() {
        let store = InMemoryStore::new();
        let err = store.get(uuid(99)).expect_err("should be missing");
        assert!(matches!(err, StoreError::NotFound(_)));
    }

    // ── dimension enforcement ──────────────────────────────────────────────

    #[test]
    fn dimension_mismatch_on_second_insert_is_rejected() {
        let mut store = InMemoryStore::new();
        store.insert(uuid(1), vec![1.0, 2.0], Metadata::new()).expect("first");
        let err = store
            .insert(uuid(2), vec![1.0, 2.0, 3.0], Metadata::new())
            .expect_err("mismatch should fail");
        assert!(matches!(
            err,
            StoreError::DimensionMismatch {
                expected: 2,
                actual: 3
            }
        ));
    }

    #[test]
    fn with_dim_rejects_wrong_length_on_first_insert() {
        let mut store = InMemoryStore::with_dim(4);
        let err = store
            .insert(uuid(1), vec![1.0, 2.0], Metadata::new())
            .expect_err("wrong dim should fail");
        assert!(matches!(
            err,
            StoreError::DimensionMismatch {
                expected: 4,
                actual: 2
            }
        ));
    }

    // ── query ──────────────────────────────────────────────────────────────

    #[test]
    fn query_returns_results_ordered_by_score_descending() {
        let mut store = InMemoryStore::new();
        let dim = 4;
        // Insert four axis-aligned unit vectors.
        for i in 0..dim {
            store
                .insert(uuid(i as u128), unit_vec(dim, i), Metadata::new())
                .expect("insert");
        }

        // Query with the axis-0 vector → id(0) should score 1.0, others 0.0.
        let hits = store.query(&unit_vec(dim, 0), 4, None).expect("query");

        assert_eq!(hits.len(), 4);
        assert_eq!(hits[0].id(), uuid(0));
        assert!((hits[0].score() - 1.0).abs() < 1e-6);
        // All others are orthogonal → score 0.0
        for h in &hits[1..] {
            assert!((h.score()).abs() < 1e-6);
        }
    }

    #[test]
    fn query_respects_k_cap() {
        let mut store = InMemoryStore::new();
        for i in 0..10u128 {
            store
                .insert(uuid(i), vec![1.0, 0.0], Metadata::new())
                .expect("insert");
        }
        let hits = store.query(&[1.0, 0.0], 3, None).expect("query");
        assert_eq!(hits.len(), 3);
    }

    #[test]
    fn query_clamps_k_to_max_query_results() {
        let mut store = InMemoryStore::new();
        for i in 0..10u128 {
            store
                .insert(uuid(i), vec![1.0, 0.0], Metadata::new())
                .expect("insert");
        }
        // Request more than MAX_QUERY_RESULTS; only MAX or store.len() should come back.
        let hits = store
            .query(&[1.0, 0.0], MAX_QUERY_RESULTS + 9999, None)
            .expect("query");
        assert!(hits.len() <= MAX_QUERY_RESULTS);
    }

    #[test]
    fn query_dimension_mismatch_returns_error() {
        let mut store = InMemoryStore::with_dim(4);
        store
            .insert(uuid(1), unit_vec(4, 0), Metadata::new())
            .expect("insert");
        let err = store
            .query(&[1.0, 0.0], 10, None)
            .expect_err("dim mismatch should error");
        assert!(matches!(
            err,
            StoreError::DimensionMismatch {
                expected: 4,
                actual: 2
            }
        ));
    }

    // ── scan_filter ────────────────────────────────────────────────────────

    #[test]
    fn scan_filter_narrows_by_metadata_eq() {
        let mut store = InMemoryStore::new();

        store
            .insert(uuid(1), vec![1.0, 0.0], meta("kind", "text"))
            .expect("a");
        store
            .insert(uuid(2), vec![0.0, 1.0], meta("kind", "image"))
            .expect("b");
        store
            .insert(uuid(3), vec![1.0, 1.0], meta("kind", "text"))
            .expect("c");

        let filter = QueryFilter {
            metadata_eq: Some(("kind".into(), json!("text"))),
        };
        let results = store.scan_filter(&filter);
        assert_eq!(results.len(), 2);
        for r in &results {
            assert_eq!(r.metadata()["kind"], json!("text"));
        }
    }

    #[test]
    fn scan_filter_with_no_matches_returns_empty_vec() {
        let mut store = InMemoryStore::new();
        store
            .insert(uuid(1), vec![1.0], meta("kind", "text"))
            .expect("insert");

        let filter = QueryFilter {
            metadata_eq: Some(("kind".into(), json!("audio"))),
        };
        assert!(store.scan_filter(&filter).is_empty());
    }

    #[test]
    fn query_with_metadata_filter_narrows_candidates() {
        let mut store = InMemoryStore::new();
        let dim = 2;

        // Insert a "text" record aligned with axis 0.
        store
            .insert(uuid(1), unit_vec(dim, 0), meta("kind", "text"))
            .expect("text");
        // Insert an "image" record also aligned with axis 0 (perfect score).
        store
            .insert(uuid(2), unit_vec(dim, 0), meta("kind", "image"))
            .expect("image");

        let filter = QueryFilter {
            metadata_eq: Some(("kind".into(), json!("text"))),
        };

        let hits = store
            .query(&unit_vec(dim, 0), 10, Some(&filter))
            .expect("query");

        // Only the "text" record should appear despite "image" having equal score.
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id(), uuid(1));
    }

    // ── performance smoke test ─────────────────────────────────────────────

    #[test]
    fn query_10k_records_k10_under_50ms() {
        use std::time::Instant;

        let n = 10_000usize;
        let dim = 128usize;
        let mut store = InMemoryStore::with_dim(dim);

        // Build a simple deterministic set of unit vectors (cycling axes).
        for i in 0..n {
            let mut v = vec![0.0f32; dim];
            v[i % dim] = 1.0;
            store
                .insert(uuid(i as u128), v, Metadata::new())
                .expect("insert");
        }

        let query_vec = unit_vec(dim, 0);
        let start = Instant::now();
        let hits = store.query(&query_vec, 10, None).expect("query");
        let elapsed = start.elapsed();

        assert_eq!(hits.len(), 10);
        assert!(
            elapsed.as_millis() < 50,
            "query took {}ms, expected <50ms",
            elapsed.as_millis()
        );
    }

    // ── len / is_empty ─────────────────────────────────────────────────────

    #[test]
    fn len_and_is_empty() {
        let mut store = InMemoryStore::new();
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);

        store
            .insert(uuid(1), vec![1.0], Metadata::new())
            .expect("insert");
        assert!(!store.is_empty());
        assert_eq!(store.len(), 1);
    }
}
