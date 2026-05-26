//! Vector query execution backend for intent-anchored retrieval.
//!
//! Provides:
//! * [`VectorQuery`] — structured query with text, top-k, min-score, and
//!   optional session filter.
//! * [`RecallEngine`] — async trait that search backends implement.
//! * [`RecallResult`] — a single retrieval hit with private fields and public
//!   accessors.
//! * [`InMemoryRecallEngine`] — cosine-similarity engine over an in-memory
//!   corpus of `(text, embedding)` pairs. Uses a mock embedder (pad / truncate
//!   to 384 dims) to embed the query text before comparison.
//!
//! ## Embed contract
//!
//! Query text is converted to a 384-dimensional vector by [`mock_embed`].
//! Corpus entries whose stored embedding is shorter than 384 dims are
//! zero-padded; those that are longer are truncated. This matches the
//! padding/truncation applied to the query so that cosine similarity is
//! always computed over the same dimensionality.

use std::collections::HashMap;

use async_trait::async_trait;
use phantom_embeddings::{EmbedItem, EmbedRequest, EmbeddingBackend, Modality as EmbedModality};

use crate::RecallError;

/// Opaque session identifier. Matches the wire type used in `phantom-protocol`
/// (`u32`) without introducing a heavyweight dependency.
pub type SessionId = u32;

// ── VectorQuery ──────────────────────────────────────────────────────────────

/// A structured vector retrieval query.
#[derive(Debug, Clone)]
pub struct VectorQuery {
    text: String,
    top_k: usize,
    min_score: f32,
    session_filter: Option<SessionId>,
}

impl VectorQuery {
    /// Create a new query.
    ///
    /// * `text` — natural-language or keyword query to embed and search.
    /// * `top_k` — maximum number of results to return.
    /// * `min_score` — minimum cosine similarity score (inclusive) a result
    ///   must reach; results below this threshold are excluded.
    /// * `session_filter` — when `Some(id)`, only corpus entries tagged with
    ///   that session id are considered.
    #[must_use]
    pub fn new(
        text: impl Into<String>,
        top_k: usize,
        min_score: f32,
        session_filter: Option<SessionId>,
    ) -> Self {
        Self {
            text: text.into(),
            top_k,
            min_score,
            session_filter,
        }
    }

    /// The query text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Maximum number of results to return.
    #[must_use]
    pub fn top_k(&self) -> usize {
        self.top_k
    }

    /// Minimum score threshold.
    #[must_use]
    pub fn min_score(&self) -> f32 {
        self.min_score
    }

    /// Optional session filter.
    #[must_use]
    pub fn session_filter(&self) -> Option<SessionId> {
        self.session_filter
    }
}

// ── RecallResult ─────────────────────────────────────────────────────────────

/// A single retrieval hit returned by [`RecallEngine::query`].
#[derive(Debug, Clone)]
pub struct RecallResult {
    text: String,
    score: f32,
    metadata: HashMap<String, String>,
}

impl RecallResult {
    /// The corpus text this result corresponds to.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Cosine similarity score in `[-1.0, 1.0]` (typically `[0.0, 1.0]`).
    #[must_use]
    pub fn score(&self) -> f32 {
        self.score
    }

    /// Arbitrary metadata attached to this corpus entry.
    #[must_use]
    pub fn metadata(&self) -> &HashMap<String, String> {
        &self.metadata
    }
}

// ── RecallEngine trait ────────────────────────────────────────────────────────

/// Async trait implemented by vector query backends.
#[async_trait]
pub trait RecallEngine: Send + Sync {
    /// Search the corpus for entries similar to `q.text()`, respecting
    /// `q.top_k()`, `q.min_score()`, and `q.session_filter()`.
    ///
    /// Results are returned sorted by score descending. Ties (same score) are
    /// broken by insertion order (earlier entries rank higher).
    async fn query(&self, q: VectorQuery) -> Result<Vec<RecallResult>, RecallError>;
}

// ── mock_embed ────────────────────────────────────────────────────────────────

const EMBED_DIM: usize = 384;

/// Convert a text string into a 384-dimensional mock embedding.
///
/// Uses a tiny FNV-1a hash spread across 384 floats so that equal strings
/// always produce equal vectors (deterministic) without an external model.
/// The output is normalised to unit length so cosine similarity is equivalent
/// to dot product.
///
/// Corpus embeddings shorter than 384 dims are zero-padded; longer ones are
/// truncated before comparison — both operations are applied at call sites, not
/// here.
#[must_use]
pub fn mock_embed(text: &str) -> Vec<f32> {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    let mut out = Vec::with_capacity(EMBED_DIM);
    for i in 0..EMBED_DIM {
        let mixed = hash.wrapping_add((i as u64).wrapping_mul(0x9e3779b97f4a7c15));
        let v = ((mixed >> 32) as i32 as f32) / (i32::MAX as f32);
        out.push(v);
    }
    normalise(&mut out);
    out
}

/// Normalise `v` to unit length in-place. No-op if the vector has zero norm.
fn normalise(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

/// Cosine similarity between two equal-length slices.
///
/// Returns `0.0` for empty slices or either zero-magnitude vector.
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0_f32;
    let mut na = 0.0_f32;
    let mut nb = 0.0_f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// Pad or truncate `embedding` to exactly `EMBED_DIM` elements.
fn normalise_dims(embedding: &[f32]) -> Vec<f32> {
    let mut out = embedding.to_vec();
    out.resize(EMBED_DIM, 0.0);
    out
}

// ── Corpus entry ──────────────────────────────────────────────────────────────

/// An entry in the [`InMemoryRecallEngine`] corpus.
#[derive(Debug, Clone)]
struct CorpusEntry {
    text: String,
    embedding: Vec<f32>,
    session_id: Option<SessionId>,
    metadata: HashMap<String, String>,
}

// ── InMemoryRecallEngine ──────────────────────────────────────────────────────

/// In-memory vector query engine.
///
/// Stores a `Vec` of corpus entries, each consisting of a text string and a
/// pre-computed embedding. When `query` is called:
///
/// 1. The query text is embedded with [`mock_embed`].
/// 2. Each corpus entry's embedding is pad/truncated to 384 dims.
/// 3. Cosine similarity is computed between the query vector and each entry.
/// 4. Entries that don't match `session_filter` or fall below `min_score` are
///    dropped.
/// 5. Results are sorted by score descending; ties are broken by insertion
///    order (stable sort).
/// 6. The top `top_k` results are returned.
#[derive(Debug, Default)]
pub struct InMemoryRecallEngine {
    corpus: Vec<CorpusEntry>,
}

impl InMemoryRecallEngine {
    /// Create a new, empty engine.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a corpus entry with a pre-computed embedding.
    ///
    /// The embedding may be any length; it will be pad/truncated to 384 dims
    /// at query time. Pass `session_id = None` if the entry does not belong
    /// to a specific session.
    pub fn add(
        &mut self,
        text: impl Into<String>,
        embedding: Vec<f32>,
        session_id: Option<SessionId>,
        metadata: HashMap<String, String>,
    ) {
        self.corpus.push(CorpusEntry {
            text: text.into(),
            embedding,
            session_id,
            metadata,
        });
    }

    /// Add a corpus entry using the mock embedder to generate the vector.
    ///
    /// Convenience wrapper around [`add`][Self::add] that calls
    /// [`mock_embed`] internally.
    pub fn add_text(
        &mut self,
        text: impl Into<String>,
        session_id: Option<SessionId>,
        metadata: HashMap<String, String>,
    ) {
        let s: String = text.into();
        let embedding = mock_embed(&s);
        self.add(s, embedding, session_id, metadata);
    }
}

#[async_trait]
impl RecallEngine for InMemoryRecallEngine {
    async fn query(&self, q: VectorQuery) -> Result<Vec<RecallResult>, RecallError> {
        let query_vec = mock_embed(q.text());

        // Score every entry, collecting (insertion_index, score, entry).
        let mut scored: Vec<(usize, f32, &CorpusEntry)> = self
            .corpus
            .iter()
            .enumerate()
            .filter_map(|(idx, entry)| {
                // Apply session filter.
                if let Some(filter_id) = q.session_filter() {
                    match entry.session_id {
                        Some(sid) if sid == filter_id => {}
                        _ => return None,
                    }
                }

                let corpus_vec = normalise_dims(&entry.embedding);
                let score = cosine(&query_vec, &corpus_vec);

                if score < q.min_score() {
                    return None;
                }

                Some((idx, score, entry))
            })
            .collect();

        // Stable sort: higher score first, tie-break by insertion index ascending.
        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });

        scored.truncate(q.top_k());

        let results = scored
            .into_iter()
            .map(|(_, score, entry)| RecallResult {
                text: entry.text.clone(),
                score,
                metadata: entry.metadata.clone(),
            })
            .collect();

        Ok(results)
    }
}

// ── RecallEntry ───────────────────────────────────────────────────────────────

/// A document entry to be indexed by [`EmbeddedRecallEngine`].
#[derive(Debug, Clone)]
pub struct RecallEntry {
    /// The text content to embed and retrieve.
    pub text: String,
    /// Optional session id used by `VectorQuery::session_filter`. When
    /// `None`, the entry is matched only by queries that pass no filter.
    pub session_id: Option<SessionId>,
    /// Arbitrary caller-supplied metadata.
    pub metadata: HashMap<String, String>,
}

impl RecallEntry {
    /// Create a new entry with the given text, no session, and no metadata.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into(), session_id: None, metadata: HashMap::new() }
    }

    /// Create a new entry with text and metadata (no session id).
    #[must_use]
    pub fn with_metadata(text: impl Into<String>, metadata: HashMap<String, String>) -> Self {
        Self { text: text.into(), session_id: None, metadata }
    }

    /// Create a new entry with text, an optional session id, and metadata.
    #[must_use]
    pub fn with_session(
        text: impl Into<String>,
        session_id: Option<SessionId>,
        metadata: HashMap<String, String>,
    ) -> Self {
        Self { text: text.into(), session_id, metadata }
    }
}

// ── EmbeddedRecallEngine ──────────────────────────────────────────────────────

/// Vector recall engine backed by a real [`EmbeddingBackend`].
///
/// Unlike [`InMemoryRecallEngine`] (which uses a deterministic FNV mock
/// embedder), `EmbeddedRecallEngine` calls out to the supplied backend — e.g.
/// the OpenAI `text-embedding-3-small` model — so that retrieved entries are
/// semantically ranked rather than hash-ranked.
///
/// The store is entirely in-memory; persistence is the caller's responsibility.
///
/// # Complexity
///
/// `query` performs a linear scan over the in-memory store and is therefore
/// O(n) in the number of indexed entries (plus one network round-trip to the
/// embedding backend for the query vector). There is no eviction or cap on
/// store size; callers placing this engine on a hot path are expected to
/// either bound the number of indexed entries themselves or replace this
/// backend with an ANN index once the corpus grows large.
///
/// # Concurrency
///
/// `query` takes `&self` and may be invoked concurrently from multiple tasks.
/// The cosine-similarity scan is purely local, but each call invokes
/// `backend.embed(...)` to vectorise the query text; concurrent callers
/// share the underlying `Box<dyn EmbeddingBackend + Send + Sync>`. The
/// `Send + Sync` bound means the backend itself is safe to share, but
/// stateful backends (rate limiters, connection pools) may serialise calls
/// internally. `index` takes `&mut self` and therefore excludes concurrent
/// `query` calls for the duration of the write.
///
/// # Example
///
/// ```rust,no_run
/// use phantom_embeddings::MockEmbeddingBackend;
/// use phantom_recall::engine::{EmbeddedRecallEngine, RecallEntry};
///
/// async fn demo() {
///     let backend = Box::new(MockEmbeddingBackend::new());
///     let mut engine = EmbeddedRecallEngine::new(backend);
///     engine.index(RecallEntry::new("rust ownership rules")).await.unwrap();
///     let _results = engine.query_text("ownership", 5, 0.0).await.unwrap();
/// }
/// ```
pub struct EmbeddedRecallEngine {
    backend: Box<dyn EmbeddingBackend + Send + Sync>,
    store: Vec<(RecallEntry, Vec<f32>)>,
}

impl EmbeddedRecallEngine {
    /// Create an engine backed by the given embedding provider.
    #[must_use]
    pub fn new(backend: Box<dyn EmbeddingBackend + Send + Sync>) -> Self {
        Self { backend, store: Vec::new() }
    }

    /// Embed `entry.text` and add it to the in-memory index.
    ///
    /// Returns an error if the backend call fails.
    pub async fn index(&mut self, entry: RecallEntry) -> Result<(), RecallError> {
        let embedding = self.embed_text(&entry.text).await?;
        self.store.push((entry, embedding));
        Ok(())
    }

    /// Query the index using a [`VectorQuery`] and return matching results.
    ///
    /// The query text is embedded, then cosine similarity is computed against
    /// every indexed entry. Entries that don't match `q.session_filter()` are
    /// dropped before scoring; results that fall below `q.min_score()` or
    /// produce a non-finite score are discarded; the remainder are sorted by
    /// score descending (ties broken by insertion order ascending), truncated
    /// to `q.top_k()`, and returned as [`RecallResult`]s.
    pub async fn query(&self, q: VectorQuery) -> Result<Vec<RecallResult>, RecallError> {
        let query_vec = self.embed_text(q.text()).await?;

        let mut scored: Vec<(usize, f32, &RecallEntry)> = self
            .store
            .iter()
            .enumerate()
            .filter_map(|(idx, (entry, emb))| {
                // Apply session filter.
                if let Some(filter_id) = q.session_filter() {
                    match entry.session_id {
                        Some(sid) if sid == filter_id => {}
                        _ => return None,
                    }
                }

                let score = Self::cosine_similarity(&query_vec, emb);
                // Drop non-finite scores defensively (NaN can sneak in if a
                // backend returns degenerate vectors); they would otherwise
                // poison the sort.
                if !score.is_finite() {
                    return None;
                }
                if score < q.min_score() { None } else { Some((idx, score, entry)) }
            })
            .collect();

        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        scored.truncate(q.top_k());

        let results = scored
            .into_iter()
            .map(|(_, score, entry)| RecallResult {
                text: entry.text.clone(),
                score,
                metadata: entry.metadata.clone(),
            })
            .collect();

        Ok(results)
    }

    /// Shorthand query: embed `text`, search with `top_k` and `min_score`.
    pub async fn query_text(
        &self,
        text: &str,
        top_k: usize,
        min_score: f32,
    ) -> Result<Vec<RecallResult>, RecallError> {
        let q = VectorQuery::new(text, top_k, min_score, None);
        self.query(q).await
    }

    /// Cosine similarity between two equal-length slices in `[-1.0, 1.0]`.
    ///
    /// Returns `0.0` for empty or differently-sized slices, and for
    /// zero-magnitude vectors.
    #[must_use]
    pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
        if a.len() != b.len() || a.is_empty() {
            return 0.0;
        }
        let mut dot = 0.0_f32;
        let mut na = 0.0_f32;
        let mut nb = 0.0_f32;
        for (x, y) in a.iter().zip(b.iter()) {
            dot += x * y;
            na += x * x;
            nb += y * y;
        }
        if na == 0.0 || nb == 0.0 { 0.0 } else { dot / (na.sqrt() * nb.sqrt()) }
    }

    /// Call the backend to embed a single text string.
    async fn embed_text(&self, text: &str) -> Result<Vec<f32>, RecallError> {
        let request = EmbedRequest {
            modality: EmbedModality::Text,
            items: vec![EmbedItem::Text(text.to_string())],
        };
        let mut embeddings = self
            .backend
            .embed(request)
            .await
            .map_err(|e| RecallError::Index(e.to_string()))?;
        embeddings
            .pop()
            .map(|e| e.vec)
            .ok_or_else(|| RecallError::Index("backend returned no embeddings".into()))
    }
}

#[async_trait]
impl RecallEngine for EmbeddedRecallEngine {
    async fn query(&self, q: VectorQuery) -> Result<Vec<RecallResult>, RecallError> {
        // Delegate to the inherent method so callers using `dyn RecallEngine`
        // observe identical filtering, scoring, and ordering semantics as the
        // direct `EmbeddedRecallEngine::query` call.
        EmbeddedRecallEngine::query(self, q).await
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn no_meta() -> HashMap<String, String> {
        HashMap::new()
    }

    fn meta(k: &str, v: &str) -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert(k.to_string(), v.to_string());
        m
    }

    /// Build an engine with a synthetic corpus where each entry has an
    /// embedding of all the same float value so cosine similarity is
    /// predictable (identical non-zero vectors → similarity = 1.0).
    fn uniform_vec(val: f32) -> Vec<f32> {
        vec![val; EMBED_DIM]
    }

    // ── 1. Empty corpus ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn empty_corpus_returns_empty_results() {
        let engine = InMemoryRecallEngine::new();
        let q = VectorQuery::new("anything", 10, 0.0, None);
        let results = engine.query(q).await.expect("query should not fail");
        assert!(results.is_empty(), "expected empty results, got {}", results.len());
    }

    // ── 2. Single result ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn single_entry_is_returned_when_above_min_score() {
        let mut engine = InMemoryRecallEngine::new();
        // Use mock_embed so query and corpus embedding are identical → score 1.0.
        engine.add_text("hello world", None, no_meta());

        let q = VectorQuery::new("hello world", 10, 0.0, None);
        let results = engine.query(q).await.expect("query");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].text(), "hello world");
        // Identical embeddings → cosine similarity = 1.0.
        assert!(
            (results[0].score() - 1.0).abs() < 1e-5,
            "expected score ~1.0, got {}",
            results[0].score()
        );
    }

    // ── 3. top_k limit ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn top_k_limits_result_count() {
        let mut engine = InMemoryRecallEngine::new();
        // Add five entries with distinct uniform embeddings.
        // Use min_score = -1.0 so all entries survive filtering; the assertion
        // checks that top_k caps the output at 3, not that scoring drops entries.
        for i in 0..5usize {
            engine.add(
                format!("entry-{i}"),
                uniform_vec((i + 1) as f32),
                None,
                no_meta(),
            );
        }

        let q = VectorQuery::new("anything", 3, -1.0, None);
        let results = engine.query(q).await.expect("query");
        assert_eq!(results.len(), 3, "expected exactly 3 results due to top_k");
    }

    // ── 4. min_score filter ───────────────────────────────────────────────────

    #[tokio::test]
    async fn min_score_filters_low_similarity_entries() {
        let mut engine = InMemoryRecallEngine::new();
        // Entry A: embedding identical to the query → score = 1.0.
        let query_text = "the quick brown fox";
        engine.add_text(query_text, None, meta("label", "A"));
        // Entry B: completely different embedding (all 0.5) → low score.
        engine.add("unrelated document", uniform_vec(0.5), None, meta("label", "B"));

        let q = VectorQuery::new(query_text, 10, 0.9, None);
        let results = engine.query(q).await.expect("query");
        // Only entry A (score ≈ 1.0) should survive the 0.9 threshold.
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].metadata()["label"], "A");
    }

    // ── 5. session filter ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn session_filter_excludes_other_sessions() {
        let mut engine = InMemoryRecallEngine::new();
        engine.add_text("session one doc", Some(1), meta("session", "1"));
        engine.add_text("session two doc", Some(2), meta("session", "2"));
        engine.add_text("no session doc", None, meta("session", "none"));

        // Filter to session 1.
        let q = VectorQuery::new("doc", 10, 0.0, Some(1));
        let results = engine.query(q).await.expect("query");
        assert_eq!(results.len(), 1, "only session-1 entry should be returned");
        assert_eq!(results[0].metadata()["session"], "1");
    }

    #[tokio::test]
    async fn session_filter_none_returns_all_sessions() {
        let mut engine = InMemoryRecallEngine::new();
        engine.add_text("doc alpha", Some(1), no_meta());
        engine.add_text("doc beta", Some(2), no_meta());
        engine.add_text("doc gamma", None, no_meta());

        // Use min_score = -1.0 so cosine scores anywhere in [-1, 1] pass the
        // threshold — this test is about session scoping, not score filtering.
        let q = VectorQuery::new("doc", 10, -1.0, None);
        let results = engine.query(q).await.expect("query");
        assert_eq!(results.len(), 3, "no filter should include all entries");
    }

    // ── 6. Tie-breaking by insertion order ────────────────────────────────────

    #[tokio::test]
    async fn ties_broken_by_insertion_order() {
        let mut engine = InMemoryRecallEngine::new();
        // All three entries share the same uniform embedding → identical
        // cosine similarity against any query. Tie should be broken by
        // insertion order: first inserted ranks highest.
        // Use min_score = -1.0 so all three entries survive regardless of the
        // cosine sign (the test is about tie-breaking, not min-score behaviour).
        engine.add("first", uniform_vec(1.0), None, meta("rank", "0"));
        engine.add("second", uniform_vec(1.0), None, meta("rank", "1"));
        engine.add("third", uniform_vec(1.0), None, meta("rank", "2"));

        let q = VectorQuery::new("query", 3, -1.0, None);
        let results = engine.query(q).await.expect("query");
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].text(), "first");
        assert_eq!(results[1].text(), "second");
        assert_eq!(results[2].text(), "third");
    }

    // ── 7. Score ordering (descending) ────────────────────────────────────────

    #[tokio::test]
    async fn results_ordered_by_score_descending() {
        let mut engine = InMemoryRecallEngine::new();
        // Query text "alpha". The corpus entry for "alpha" uses mock_embed("alpha")
        // so its cosine similarity to the query is exactly 1.0. The other two
        // use embeddings of different texts — cosine may be anywhere in [-1, 1].
        // Use min_score = -1.0 so all three entries are always included; the test
        // is about ordering, not min-score filtering.
        engine.add_text("alpha", None, meta("label", "exact"));
        engine.add("beta content", mock_embed("beta"), None, meta("label", "beta"));
        engine.add("gamma content", mock_embed("gamma"), None, meta("label", "gamma"));

        let q = VectorQuery::new("alpha", 10, -1.0, None);
        let results = engine.query(q).await.expect("query");
        assert_eq!(results.len(), 3);
        // The first result should be the exact match (score = 1.0).
        assert_eq!(results[0].metadata()["label"], "exact");
        assert!(
            (results[0].score() - 1.0).abs() < 1e-5,
            "exact match should score 1.0, got {}",
            results[0].score()
        );
        // Verify all scores are non-increasing.
        for window in results.windows(2) {
            assert!(
                window[0].score() >= window[1].score() - 1e-5,
                "score ordering violated: {} < {}",
                window[0].score(),
                window[1].score()
            );
        }
    }

    // ── 8. min_score at boundary ──────────────────────────────────────────────

    #[tokio::test]
    async fn min_score_boundary_inclusive() {
        let mut engine = InMemoryRecallEngine::new();
        // Identical embedding → score exactly 1.0; set min_score = 1.0.
        engine.add_text("exact match", None, no_meta());

        let q = VectorQuery::new("exact match", 10, 1.0, None);
        let results = engine.query(q).await.expect("query");
        assert_eq!(results.len(), 1, "exact min_score boundary should be inclusive");
    }

    // ── 9. RecallResult accessors ─────────────────────────────────────────────

    #[tokio::test]
    async fn recall_result_accessors_expose_all_fields() {
        let mut engine = InMemoryRecallEngine::new();
        engine.add_text("documented entry", None, meta("author", "alice"));

        let q = VectorQuery::new("documented entry", 1, 0.0, None);
        let results = engine.query(q).await.expect("query");
        assert_eq!(results.len(), 1);
        let r = &results[0];
        // text()
        assert_eq!(r.text(), "documented entry");
        // score() in plausible range
        assert!(r.score() >= 0.0 && r.score() <= 1.0 + 1e-5);
        // metadata()
        assert_eq!(r.metadata().get("author").map(String::as_str), Some("alice"));
    }

    // ── 10. VectorQuery accessors ─────────────────────────────────────────────

    #[test]
    fn recall_query_accessors_round_trip() {
        let q = VectorQuery::new("some text", 5, 0.3, Some(42));
        assert_eq!(q.text(), "some text");
        assert_eq!(q.top_k(), 5);
        assert!((q.min_score() - 0.3).abs() < f32::EPSILON);
        assert_eq!(q.session_filter(), Some(42));

        let q2 = VectorQuery::new("other", 1, 0.0, None);
        assert_eq!(q2.session_filter(), None);
    }

    // ── 11. mock_embed is deterministic and produces 384 dims ─────────────────

    #[test]
    fn mock_embed_is_deterministic_and_correct_dim() {
        let a = mock_embed("test phrase");
        let b = mock_embed("test phrase");
        assert_eq!(a.len(), EMBED_DIM);
        assert_eq!(a, b, "mock_embed must be deterministic");
    }

    #[test]
    fn mock_embed_different_texts_differ() {
        let a = mock_embed("hello");
        let b = mock_embed("world");
        assert_ne!(a, b, "different texts should produce different embeddings");
    }

    // ── 12. pad/truncate corpus embeddings ────────────────────────────────────

    #[tokio::test]
    async fn short_corpus_embedding_is_zero_padded() {
        let mut engine = InMemoryRecallEngine::new();
        // Store a 10-dim embedding; query should still work (padded to 384).
        // Use min_score = -1.0 so the entry is returned regardless of cosine sign.
        engine.add("short dim entry", vec![1.0_f32; 10], None, no_meta());

        let q = VectorQuery::new("query", 10, -1.0, None);
        let results = engine.query(q).await.expect("query");
        assert_eq!(results.len(), 1, "short embedding should still be queried");
        // Score should be finite and in range.
        assert!(results[0].score().is_finite());
        assert!(results[0].score() >= -1.0 && results[0].score() <= 1.0 + 1e-5);
    }

    #[tokio::test]
    async fn long_corpus_embedding_is_truncated() {
        let mut engine = InMemoryRecallEngine::new();
        // Store a 1000-dim embedding; should be truncated gracefully.
        // Use min_score = -1.0 so the entry is returned regardless of cosine sign.
        engine.add("long dim entry", vec![0.5_f32; 1000], None, no_meta());

        let q = VectorQuery::new("query", 10, -1.0, None);
        let results = engine.query(q).await.expect("query");
        assert_eq!(results.len(), 1, "long embedding should still be queried");
        assert!(results[0].score().is_finite());
    }

    // ── EmbeddedRecallEngine tests ────────────────────────────────────────────

    use phantom_embeddings::MockEmbeddingBackend;

    fn mock_backend() -> Box<dyn phantom_embeddings::EmbeddingBackend + Send + Sync> {
        // 16-dim mock — consistent dim for tests.
        Box::new(MockEmbeddingBackend::with_dim(16))
    }

    #[tokio::test]
    async fn embedded_recall_engine_indexes_and_queries() {
        let mut engine = EmbeddedRecallEngine::new(mock_backend());
        engine
            .index(RecallEntry::new("rust ownership and borrowing"))
            .await
            .expect("index");
        engine
            .index(RecallEntry::new("async await futures in rust"))
            .await
            .expect("index");

        // min_score = -1.0 so all entries are returned regardless of cosine sign.
        let results = engine
            .query_text("rust ownership", 10, -1.0)
            .await
            .expect("query");
        assert_eq!(results.len(), 2, "both indexed entries should be returned");
    }

    #[test]
    fn cosine_similarity_returns_one_for_identical_vectors() {
        let v = vec![0.6_f32, 0.8_f32]; // already unit length: 0.36+0.64=1.0
        let sim = EmbeddedRecallEngine::cosine_similarity(&v, &v);
        assert!((sim - 1.0).abs() < 1e-6, "identical vectors must give 1.0, got {sim}");
    }

    #[tokio::test]
    async fn embedded_recall_engine_respects_top_k() {
        let mut engine = EmbeddedRecallEngine::new(mock_backend());
        for i in 0..5u32 {
            engine.index(RecallEntry::new(format!("entry {i}"))).await.expect("index");
        }
        let results = engine.query_text("entry", 3, -1.0).await.expect("query");
        assert_eq!(results.len(), 3, "top_k should cap results at 3");
    }

    #[tokio::test]
    async fn embedded_recall_engine_min_score_filters() {
        let mut engine = EmbeddedRecallEngine::new(mock_backend());
        // Both entries get embeddings from the mock; the exact match scores 1.0.
        engine.index(RecallEntry::new("phantom terminal emulator")).await.expect("index");
        engine.index(RecallEntry::new("completely different topic xyz")).await.expect("index");

        // With min_score = 1.0 only the exact-match entry passes (cosine = 1.0).
        let results = engine
            .query_text("phantom terminal emulator", 10, 1.0)
            .await
            .expect("query");
        assert_eq!(results.len(), 1, "only the exact-match entry should survive min_score 1.0");
        assert_eq!(results[0].text(), "phantom terminal emulator");
        assert!((results[0].score() - 1.0).abs() < 1e-5, "score should be 1.0 for exact match");
    }

    #[test]
    fn cosine_similarity_zero_for_mismatched_lengths() {
        let a = vec![1.0_f32, 0.0];
        let b = vec![1.0_f32, 0.0, 0.0];
        assert_eq!(EmbeddedRecallEngine::cosine_similarity(&a, &b), 0.0);
    }

    #[test]
    fn cosine_similarity_zero_for_zero_magnitude() {
        let a = vec![0.0_f32, 0.0, 0.0];
        let b = vec![1.0_f32, 2.0, 3.0];
        assert_eq!(EmbeddedRecallEngine::cosine_similarity(&a, &b), 0.0);
    }

    #[tokio::test]
    async fn embedded_recall_engine_session_filter_excludes_other_sessions() {
        let mut engine = EmbeddedRecallEngine::new(mock_backend());
        engine
            .index(RecallEntry::with_session("session one doc", Some(1), no_meta()))
            .await
            .expect("index");
        engine
            .index(RecallEntry::with_session("session two doc", Some(2), no_meta()))
            .await
            .expect("index");
        engine
            .index(RecallEntry::with_session("no session doc", None, no_meta()))
            .await
            .expect("index");

        let q = VectorQuery::new("doc", 10, -1.0, Some(1));
        let results = engine.query(q).await.expect("query");
        assert_eq!(results.len(), 1, "only the session-1 entry should be returned");
        assert_eq!(results[0].text(), "session one doc");
    }

    #[tokio::test]
    async fn embedded_recall_engine_no_session_filter_returns_all() {
        let mut engine = EmbeddedRecallEngine::new(mock_backend());
        engine
            .index(RecallEntry::with_session("a", Some(1), no_meta()))
            .await
            .expect("index");
        engine
            .index(RecallEntry::with_session("b", Some(2), no_meta()))
            .await
            .expect("index");
        engine
            .index(RecallEntry::with_session("c", None, no_meta()))
            .await
            .expect("index");

        let q = VectorQuery::new("any", 10, -1.0, None);
        let results = engine.query(q).await.expect("query");
        assert_eq!(results.len(), 3, "no filter should include all sessions plus None");
    }

    #[tokio::test]
    async fn embedded_recall_engine_implements_recall_engine_trait() {
        // Confirm `EmbeddedRecallEngine` is usable as `dyn RecallEngine`.
        let mut engine = EmbeddedRecallEngine::new(mock_backend());
        engine.index(RecallEntry::new("polymorphic call")).await.expect("index");

        let dyn_engine: &dyn RecallEngine = &engine;
        let q = VectorQuery::new("polymorphic call", 10, -1.0, None);
        let results = dyn_engine.query(q).await.expect("query");
        assert_eq!(results.len(), 1, "dyn dispatch should produce the indexed entry");
    }
}
