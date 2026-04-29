//! Vector query execution backend for intent-anchored retrieval.
//!
//! Provides:
//! * [`RecallQuery`] — structured query with text, top-k, min-score, and
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

use crate::RecallError;

/// Opaque session identifier. Matches the wire type used in `phantom-protocol`
/// (`u32`) without introducing a heavyweight dependency.
pub type SessionId = u32;

// ── RecallQuery ──────────────────────────────────────────────────────────────

/// A structured vector retrieval query.
#[derive(Debug, Clone)]
pub struct RecallQuery {
    text: String,
    top_k: usize,
    min_score: f32,
    session_filter: Option<SessionId>,
}

impl RecallQuery {
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
    async fn query(&self, q: RecallQuery) -> Result<Vec<RecallResult>, RecallError>;
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
    async fn query(&self, q: RecallQuery) -> Result<Vec<RecallResult>, RecallError> {
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
        let q = RecallQuery::new("anything", 10, 0.0, None);
        let results = engine.query(q).await.expect("query should not fail");
        assert!(results.is_empty(), "expected empty results, got {}", results.len());
    }

    // ── 2. Single result ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn single_entry_is_returned_when_above_min_score() {
        let mut engine = InMemoryRecallEngine::new();
        // Use mock_embed so query and corpus embedding are identical → score 1.0.
        engine.add_text("hello world", None, no_meta());

        let q = RecallQuery::new("hello world", 10, 0.0, None);
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

        let q = RecallQuery::new("anything", 3, -1.0, None);
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

        let q = RecallQuery::new(query_text, 10, 0.9, None);
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
        let q = RecallQuery::new("doc", 10, 0.0, Some(1));
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
        let q = RecallQuery::new("doc", 10, -1.0, None);
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

        let q = RecallQuery::new("query", 3, -1.0, None);
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

        let q = RecallQuery::new("alpha", 10, -1.0, None);
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

        let q = RecallQuery::new("exact match", 10, 1.0, None);
        let results = engine.query(q).await.expect("query");
        assert_eq!(results.len(), 1, "exact min_score boundary should be inclusive");
    }

    // ── 9. RecallResult accessors ─────────────────────────────────────────────

    #[tokio::test]
    async fn recall_result_accessors_expose_all_fields() {
        let mut engine = InMemoryRecallEngine::new();
        engine.add_text("documented entry", None, meta("author", "alice"));

        let q = RecallQuery::new("documented entry", 1, 0.0, None);
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

    // ── 10. RecallQuery accessors ─────────────────────────────────────────────

    #[test]
    fn recall_query_accessors_round_trip() {
        let q = RecallQuery::new("some text", 5, 0.3, Some(42));
        assert_eq!(q.text(), "some text");
        assert_eq!(q.top_k(), 5);
        assert!((q.min_score() - 0.3).abs() < f32::EPSILON);
        assert_eq!(q.session_filter(), Some(42));

        let q2 = RecallQuery::new("other", 1, 0.0, None);
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

        let q = RecallQuery::new("query", 10, -1.0, None);
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

        let q = RecallQuery::new("query", 10, -1.0, None);
        let results = engine.query(q).await.expect("query");
        assert_eq!(results.len(), 1, "long embedding should still be queried");
        assert!(results[0].score().is_finite());
    }
}
