//! Intent-anchored retrieval API.
//!
//! Surface only — no embedding model, no ANN backend, no LLM client. The
//! traits here lock the contract so search backends and LLM rewriters can
//! land in parallel later.
//!
//! ## Vector query execution
//!
//! [`engine`] provides the concrete `InMemoryRecallEngine` which implements
//! cosine-similarity search over an in-memory corpus using a deterministic
//! mock embedder (384 dims). Use it for tests and offline development.
//!
//! ## Pipeline
//!
//! 1. [`QueryRewriter`] turns a natural-language phrase like
//!    `"the meeting where we argued about pricing"` into a structured
//!    [`RecallQuery`] (intent, tags, time window, modality hint).
//! 2. A modality router (caller's responsibility) selects which embedding
//!    tables to search using [`RecallQuery::modality_hint`].
//! 3. ANN backend produces raw similarity hits.
//! 4. [`fuse_score`] composes the final score from similarity, recency, and
//!    importance.
//! 5. An LLM reranker (caller's responsibility) reorders the top-K
//!    [`RecallHit`]s.
//!
//! ## Score fusion
//!
//! Hits are scored as `alpha * similarity + beta * recency + gamma * importance`.
//! Weights default to Park et al.'s generative-agent retrieval defaults
//! (0.6 / 0.2 / 0.2) and are tunable per deployment via [`ScoreWeights`].
//!
//! ## Recency
//!
//! Recency uses an exponential half-life. A bundle whose event timestamp
//! equals "now" scores 1.0; one whose event is `half_life_days` in the past
//! scores 0.5; events in the future or with a non-positive half-life clamp
//! to 1.0.

pub mod engine;

use async_trait::async_trait;
use phantom_bundles::BundleId;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A modality the retrieval pipeline can search over. Used as a hint to the
/// modality router so it knows which embedding tables to consult.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Modality {
    /// Transcript / OCR / structured text.
    Text,
    /// Captured frames or screenshots.
    Image,
    /// Raw audio (separate from transcript).
    Audio,
    /// Sealed bundle intent tags.
    Intent,
}

/// A structured retrieval query. Produced either directly by the caller or
/// by a [`QueryRewriter`] from a natural-language phrase.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecallQuery {
    /// Original natural-language query, preserved for reranking and audit.
    pub natural_language: String,
    /// Optional intent label extracted by the rewriter (e.g. `"meeting"`).
    pub intent_hint: Option<String>,
    /// Tags pulled from the rewriter (e.g. `["pricing", "argument"]`).
    pub tags: Vec<String>,
    /// Inclusive `(start, end)` window in unix milliseconds.
    pub time_window_unix_ms: Option<(i64, i64)>,
    /// Modality the rewriter believes is most likely to contain the answer.
    pub modality_hint: Option<Modality>,
    /// Maximum number of hits to return after fusion + rerank.
    pub limit: usize,
}

/// A single retrieval hit with both raw signals and the fused score.
///
/// Raw signals are kept on the hit so callers can re-fuse with different
/// weights or display per-axis explanations without re-running ANN.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecallHit {
    /// Bundle this hit refers to.
    pub bundle_id: BundleId,
    /// Final composite score (output of [`fuse_score`]).
    pub score: f32,
    /// Raw ANN cosine similarity in `[-1.0, 1.0]` (typically `[0.0, 1.0]`).
    pub similarity: f32,
    /// Recency score in `[0.0, 1.0]`; `1.0` means "now".
    pub recency: f32,
    /// Importance pulled from the bundle, in `[0.0, 1.0]`.
    pub importance: f32,
    /// Top transcript words that matched, if the backend produced any.
    pub matched_words: Vec<String>,
}

/// Errors surfaced by the retrieval pipeline.
#[derive(Debug, Error)]
pub enum RecallError {
    /// The ANN / vector store layer failed.
    #[error("index error: {0}")]
    Index(String),
    /// The query rewriter failed (bad model response, network, parse error).
    #[error("rewriter error: {0}")]
    Rewriter(String),
}

/// Rewrite a natural-language query into a structured [`RecallQuery`].
///
/// Implementations are expected to call out to an LLM (Claude, etc.).
/// [`MockRewriter`] is provided for tests and local development.
#[async_trait]
pub trait QueryRewriter: Send + Sync {
    /// Rewrite `natural` into a structured query. The returned
    /// `natural_language` should equal `natural` so downstream rerankers
    /// retain the original phrasing.
    async fn rewrite(&self, natural: &str) -> Result<RecallQuery, RecallError>;
}

/// Run the full retrieval pipeline.
///
/// Implementations own modality routing, ANN, score fusion, and rerank.
/// Callers may pre-rewrite the query themselves or pass through whatever
/// natural-language string they have — implementations decide how to handle
/// missing structure.
#[async_trait]
pub trait Recall: Send + Sync {
    /// Stable name for logging and metrics.
    fn name(&self) -> &'static str;

    /// Return up to `query.limit` hits, sorted by `score` descending.
    async fn search(&self, query: RecallQuery) -> Result<Vec<RecallHit>, RecallError>;
}

/// Linear-combination weights for [`fuse_score`].
///
/// Defaults track Park et al.'s generative-agent retrieval defaults:
/// `alpha = 0.6`, `beta = 0.2`, `gamma = 0.2`. Tune per deployment.
#[derive(Debug, Clone, Copy)]
pub struct ScoreWeights {
    /// Weight on raw ANN similarity.
    pub alpha: f32,
    /// Weight on recency.
    pub beta: f32,
    /// Weight on importance.
    pub gamma: f32,
}

impl Default for ScoreWeights {
    fn default() -> Self {
        // Park et al. defaults; tunable later.
        Self { alpha: 0.6, beta: 0.2, gamma: 0.2 }
    }
}

/// Fuse the three retrieval axes into a single composite score.
///
/// Pure linear combination: `alpha*similarity + beta*recency + gamma*importance`.
/// No clamping or normalization — callers that want a `[0, 1]` output
/// should ensure their weights sum to `1.0` and inputs are in `[0, 1]`.
#[must_use]
pub fn fuse_score(similarity: f32, recency: f32, importance: f32, weights: ScoreWeights) -> f32 {
    weights.alpha * similarity + weights.beta * recency + weights.gamma * importance
}

/// Compute a recency score in `[0.0, 1.0]` using an exponential half-life.
///
/// `event_unix_ms == now_unix_ms` returns `1.0`. An event `half_life_days`
/// in the past returns `0.5`. Future events and non-positive half-lives
/// clamp to `1.0`; very old events asymptote toward `0.0`.
#[must_use]
pub fn recency_score(event_unix_ms: i64, now_unix_ms: i64, half_life_days: f32) -> f32 {
    if half_life_days <= 0.0 {
        return 1.0;
    }
    let delta_ms = now_unix_ms - event_unix_ms;
    if delta_ms <= 0 {
        return 1.0;
    }
    let ms_per_day = 86_400_000.0_f32;
    let delta_days = (delta_ms as f32) / ms_per_day;
    let raw = (0.5_f32).powf(delta_days / half_life_days);
    raw.clamp(0.0, 1.0)
}

/// In-memory mock rewriter for tests.
///
/// Parses a hardcoded grammar: any tokens after the literal word `about` are
/// captured as tags; the literal word `meeting` sets `intent_hint`. Limit
/// defaults to `10`.
#[derive(Debug, Default, Clone)]
pub struct MockRewriter;

#[async_trait]
impl QueryRewriter for MockRewriter {
    async fn rewrite(&self, natural: &str) -> Result<RecallQuery, RecallError> {
        let lower = natural.to_lowercase();
        let intent_hint = lower
            .split_whitespace()
            .find(|w| *w == "meeting")
            .map(|s| s.to_string());

        let tags: Vec<String> = match lower.split_once("about") {
            Some((_, after)) => after
                .split_whitespace()
                .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()).to_string())
                .filter(|w| !w.is_empty())
                .collect(),
            None => Vec::new(),
        };

        Ok(RecallQuery {
            natural_language: natural.to_string(),
            intent_hint,
            tags,
            time_window_unix_ms: None,
            modality_hint: Some(Modality::Text),
            limit: 10,
        })
    }
}

/// In-memory mock recall backend for tests.
///
/// Holds a fixed set of [`RecallHit`]s and returns them sorted by `score`
/// descending, truncated to `query.limit`. Ignores the query body otherwise.
#[derive(Debug, Default, Clone)]
pub struct MockRecall {
    hits: Vec<RecallHit>,
}

impl MockRecall {
    /// Build a new mock from a fixed set of hits.
    #[must_use]
    pub fn new(hits: Vec<RecallHit>) -> Self {
        Self { hits }
    }
}

#[async_trait]
impl Recall for MockRecall {
    fn name(&self) -> &'static str {
        "mock"
    }

    async fn search(&self, query: RecallQuery) -> Result<Vec<RecallHit>, RecallError> {
        let mut hits = self.hits.clone();
        hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        hits.truncate(query.limit);
        Ok(hits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(score: f32) -> RecallHit {
        RecallHit {
            bundle_id: BundleId::new_v4(),
            score,
            similarity: score,
            recency: 0.5,
            importance: 0.5,
            matched_words: vec![],
        }
    }

    #[test]
    fn recall_query_round_trips_through_json() {
        let q = RecallQuery {
            natural_language: "the meeting where we argued about pricing".into(),
            intent_hint: Some("meeting".into()),
            tags: vec!["pricing".into(), "argument".into()],
            time_window_unix_ms: Some((1_700_000_000_000, 1_710_000_000_000)),
            modality_hint: Some(Modality::Text),
            limit: 25,
        };
        let json = serde_json::to_string(&q).expect("serialize");
        let restored: RecallQuery = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored.natural_language, q.natural_language);
        assert_eq!(restored.intent_hint, q.intent_hint);
        assert_eq!(restored.tags, q.tags);
        assert_eq!(restored.time_window_unix_ms, q.time_window_unix_ms);
        assert!(matches!(restored.modality_hint, Some(Modality::Text)));
        assert_eq!(restored.limit, q.limit);
    }

    #[test]
    fn recall_hit_round_trips_through_json() {
        let h = hit(0.8);
        let json = serde_json::to_string(&h).expect("serialize");
        let restored: RecallHit = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored.bundle_id, h.bundle_id);
        assert!((restored.score - h.score).abs() < f32::EPSILON);
    }

    #[test]
    fn score_weights_default_matches_park_et_al() {
        let w = ScoreWeights::default();
        assert!((w.alpha - 0.6).abs() < f32::EPSILON);
        assert!((w.beta - 0.2).abs() < f32::EPSILON);
        assert!((w.gamma - 0.2).abs() < f32::EPSILON);
        assert!((w.alpha + w.beta + w.gamma - 1.0).abs() < 1e-6);
    }

    #[test]
    fn fuse_score_is_linear_combination() {
        let w = ScoreWeights { alpha: 0.5, beta: 0.3, gamma: 0.2 };
        let s = fuse_score(1.0, 0.5, 0.25, w);
        // 0.5*1.0 + 0.3*0.5 + 0.2*0.25 = 0.5 + 0.15 + 0.05 = 0.70
        assert!((s - 0.70).abs() < 1e-6, "expected 0.70, got {s}");
    }

    #[test]
    fn fuse_score_zero_weights_yield_zero() {
        let w = ScoreWeights { alpha: 0.0, beta: 0.0, gamma: 0.0 };
        let s = fuse_score(0.99, 0.99, 0.99, w);
        assert!(s.abs() < f32::EPSILON);
    }

    #[test]
    fn recency_is_one_when_event_is_now() {
        let now = 1_700_000_000_000;
        let r = recency_score(now, now, 7.0);
        assert!((r - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn recency_is_half_at_one_half_life() {
        let now = 1_700_000_000_000;
        let half_life_days = 7.0;
        let one_half_life_ago = now - (half_life_days as i64) * 86_400_000;
        let r = recency_score(one_half_life_ago, now, half_life_days);
        assert!((r - 0.5).abs() < 1e-4, "expected ~0.5, got {r}");
    }

    #[test]
    fn recency_decays_toward_zero_for_ancient_events() {
        let now = 1_700_000_000_000;
        let ten_half_lives_ago = now - 10 * 7 * 86_400_000;
        let r = recency_score(ten_half_lives_ago, now, 7.0);
        // 0.5^10 ~= 0.000976
        assert!(r < 0.01, "expected near zero, got {r}");
        assert!(r >= 0.0);
    }

    #[test]
    fn recency_clamps_to_one_for_future_events() {
        let now = 1_700_000_000_000;
        let future = now + 86_400_000;
        let r = recency_score(future, now, 7.0);
        assert!((r - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn recency_clamps_to_one_for_nonpositive_half_life() {
        let now = 1_700_000_000_000;
        let r = recency_score(now - 86_400_000, now, 0.0);
        assert!((r - 1.0).abs() < f32::EPSILON);
        let r = recency_score(now - 86_400_000, now, -3.0);
        assert!((r - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn recency_stays_in_unit_interval_for_arbitrary_inputs() {
        let now = 1_700_000_000_000;
        for &delta_days in &[-1000.0_f32, -1.0, 0.0, 1.0, 100.0, 10_000.0] {
            let event = now - (delta_days as i64) * 86_400_000;
            let r = recency_score(event, now, 14.0);
            assert!((0.0..=1.0).contains(&r), "out of range at delta={delta_days}: {r}");
        }
    }

    #[tokio::test]
    async fn mock_rewriter_extracts_intent_and_tags() {
        let r = MockRewriter;
        let q = r
            .rewrite("the meeting where we argued about pricing tiers")
            .await
            .expect("rewrite");
        assert_eq!(q.natural_language, "the meeting where we argued about pricing tiers");
        assert_eq!(q.intent_hint.as_deref(), Some("meeting"));
        assert_eq!(q.tags, vec!["pricing".to_string(), "tiers".to_string()]);
        assert!(matches!(q.modality_hint, Some(Modality::Text)));
        assert_eq!(q.limit, 10);
    }

    #[tokio::test]
    async fn mock_rewriter_handles_no_about_clause() {
        let r = MockRewriter;
        let q = r.rewrite("standup yesterday").await.expect("rewrite");
        assert!(q.tags.is_empty());
        assert!(q.intent_hint.is_none());
    }

    #[tokio::test]
    async fn mock_recall_returns_hits_sorted_descending() {
        let backend = MockRecall::new(vec![hit(0.3), hit(0.9), hit(0.5), hit(0.7)]);
        let query = RecallQuery {
            natural_language: "anything".into(),
            intent_hint: None,
            tags: vec![],
            time_window_unix_ms: None,
            modality_hint: None,
            limit: 10,
        };
        let results = backend.search(query).await.expect("search");
        let scores: Vec<f32> = results.iter().map(|h| h.score).collect();
        assert_eq!(scores, vec![0.9_f32, 0.7, 0.5, 0.3]);
    }

    #[tokio::test]
    async fn mock_recall_respects_limit() {
        let backend = MockRecall::new(vec![hit(0.3), hit(0.9), hit(0.5), hit(0.7)]);
        let query = RecallQuery {
            natural_language: "anything".into(),
            intent_hint: None,
            tags: vec![],
            time_window_unix_ms: None,
            modality_hint: None,
            limit: 2,
        };
        let results = backend.search(query).await.expect("search");
        assert_eq!(results.len(), 2);
        assert!((results[0].score - 0.9).abs() < f32::EPSILON);
        assert!((results[1].score - 0.7).abs() < f32::EPSILON);
    }

    #[test]
    fn recall_error_renders_messages() {
        let e = RecallError::Index("hnsw down".into());
        assert_eq!(e.to_string(), "index error: hnsw down");
        let e = RecallError::Rewriter("bad json".into());
        assert_eq!(e.to_string(), "rewriter error: bad json");
    }
}
