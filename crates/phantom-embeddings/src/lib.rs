//! Multi-modal embedding API surface.
//!
//! This crate defines the trait + types for embedding backends. Concrete
//! backends (text models, image encoders, audio encoders, intent rankers)
//! implement [`EmbeddingBackend`] and can be wired in independently.
//!
//! Scope is intentionally narrow: types, the async trait, a cosine helper,
//! and a deterministic [`MockEmbeddingBackend`] for tests. Real model loading,
//! batching strategy, and routing live elsewhere.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub mod openai;
pub mod store;

/// What kind of input an embedding represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Modality {
    Text,
    Image,
    Audio,
    Intent,
}

/// A batch of items to embed under a single modality.
#[derive(Debug, Clone)]
pub struct EmbedRequest {
    pub modality: Modality,
    pub items: Vec<EmbedItem>,
}

/// A single payload to be embedded.
#[derive(Debug, Clone)]
pub enum EmbedItem {
    Text(String),
    Image { bytes: Vec<u8>, mime: String },
    Audio { samples: Vec<f32>, sample_rate: u32 },
}

/// A produced embedding vector plus identifying metadata.
#[derive(Debug, Clone)]
pub struct Embedding {
    pub vec: Vec<f32>,
    pub dim: usize,
    pub model: String,
}

#[derive(Debug, Error)]
pub enum EmbedError {
    #[error("modality {0:?} not supported by this backend")]
    UnsupportedModality(Modality),
    #[error("backend error: {0}")]
    Backend(String),
    #[error("not configured: {0}")]
    NotConfigured(String),
}

/// Implemented by concrete embedding providers (Ollama, ONNX, OpenAI, mocks).
#[async_trait]
pub trait EmbeddingBackend: Send + Sync {
    /// Stable identifier, e.g. `"mock"`, `"ollama-nomic"`.
    fn name(&self) -> &'static str;

    /// Whether this backend can embed `modality`.
    fn supports(&self, modality: Modality) -> bool;

    /// Vector dimension for `modality`, or `None` if unsupported.
    fn dimension_for(&self, modality: Modality) -> Option<usize>;

    /// Embed every item in `request`. Returns one [`Embedding`] per item,
    /// in the same order.
    async fn embed(&self, request: EmbedRequest) -> Result<Vec<Embedding>, EmbedError>;
}

/// Cosine similarity in `[-1.0, 1.0]`.
///
/// Returns `0.0` if the vectors have different lengths or if either has
/// zero magnitude. Documented and tested behavior — callers should
/// validate dims up-front when that matters.
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
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// Deterministic in-memory backend for tests and offline development.
///
/// Supports [`Modality::Text`] and [`Modality::Intent`] only. Vectors are
/// derived from a stable hash of the input so two calls with the same item
/// yield identical embeddings.
pub struct MockEmbeddingBackend {
    dim: usize,
}

impl MockEmbeddingBackend {
    #[must_use]
    pub fn new() -> Self {
        Self { dim: 16 }
    }

    #[must_use]
    pub fn with_dim(dim: usize) -> Self {
        Self { dim }
    }

    fn embed_text(&self, text: &str) -> Vec<f32> {
        // Tiny FNV-1a so output is stable across runs without bringing in a hasher dep.
        let mut hash: u64 = 0xcbf29ce484222325;
        for byte in text.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        let mut out = Vec::with_capacity(self.dim);
        for i in 0..self.dim {
            let mixed = hash.wrapping_add((i as u64).wrapping_mul(0x9e3779b97f4a7c15));
            // Map to roughly [-1, 1].
            let v = ((mixed >> 32) as i32 as f32) / (i32::MAX as f32);
            out.push(v);
        }
        out
    }
}

impl Default for MockEmbeddingBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl EmbeddingBackend for MockEmbeddingBackend {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn supports(&self, modality: Modality) -> bool {
        matches!(modality, Modality::Text | Modality::Intent)
    }

    fn dimension_for(&self, modality: Modality) -> Option<usize> {
        if self.supports(modality) {
            Some(self.dim)
        } else {
            None
        }
    }

    async fn embed(&self, request: EmbedRequest) -> Result<Vec<Embedding>, EmbedError> {
        if !self.supports(request.modality) {
            return Err(EmbedError::UnsupportedModality(request.modality));
        }
        let mut out = Vec::with_capacity(request.items.len());
        for item in &request.items {
            let text = match item {
                EmbedItem::Text(s) => s.as_str(),
                // Intent flows through as text in the mock; real backends
                // may treat it differently.
                EmbedItem::Image { .. } | EmbedItem::Audio { .. } => {
                    return Err(EmbedError::Backend(
                        "mock backend only accepts text payloads".into(),
                    ));
                }
            };
            let vec = self.embed_text(text);
            out.push(Embedding {
                dim: vec.len(),
                vec,
                model: self.name().to_string(),
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_identical_vectors_is_one() {
        let a = vec![1.0, 2.0, 3.0, 4.0];
        let sim = cosine_similarity(&a, &a);
        assert!((sim - 1.0).abs() < 1e-6, "expected 1.0, got {sim}");
    }

    #[test]
    fn cosine_orthogonal_vectors_is_zero() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        let sim = cosine_similarity(&a, &b);
        assert!(sim.abs() < 1e-6, "expected 0.0, got {sim}");
    }

    #[test]
    fn cosine_opposite_vectors_is_negative_one() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![-1.0, -2.0, -3.0];
        let sim = cosine_similarity(&a, &b);
        assert!((sim + 1.0).abs() < 1e-6, "expected -1.0, got {sim}");
    }

    #[test]
    fn cosine_mismatched_length_returns_zero() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![1.0, 2.0];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }

    #[test]
    fn cosine_zero_magnitude_returns_zero() {
        let a = vec![0.0, 0.0, 0.0];
        let b = vec![1.0, 2.0, 3.0];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }

    #[test]
    fn modality_serde_round_trips() {
        for m in [
            Modality::Text,
            Modality::Image,
            Modality::Audio,
            Modality::Intent,
        ] {
            let json = serde_json::to_string(&m).expect("serialize");
            let back: Modality = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(m, back, "round-trip failed for {m:?}");
        }
    }

    #[test]
    fn mock_supports_only_text_and_intent() {
        let mock = MockEmbeddingBackend::new();
        assert!(mock.supports(Modality::Text));
        assert!(mock.supports(Modality::Intent));
        assert!(!mock.supports(Modality::Image));
        assert!(!mock.supports(Modality::Audio));
    }

    #[test]
    fn mock_dimension_honors_supported_modalities() {
        let mock = MockEmbeddingBackend::with_dim(32);
        assert_eq!(mock.dimension_for(Modality::Text), Some(32));
        assert_eq!(mock.dimension_for(Modality::Intent), Some(32));
        // NotApplicable scenario: unsupported modalities have no dimension.
        assert_eq!(mock.dimension_for(Modality::Image), None);
        assert_eq!(mock.dimension_for(Modality::Audio), None);
    }

    #[tokio::test]
    async fn mock_embed_returns_n_embeddings_of_declared_dim() {
        let mock = MockEmbeddingBackend::with_dim(24);
        let request = EmbedRequest {
            modality: Modality::Text,
            items: vec![
                EmbedItem::Text("hello".into()),
                EmbedItem::Text("world".into()),
                EmbedItem::Text("phantom".into()),
            ],
        };
        let out = mock.embed(request).await.expect("embed should succeed");
        assert_eq!(out.len(), 3);
        for e in &out {
            assert_eq!(e.dim, 24);
            assert_eq!(e.vec.len(), 24);
            assert_eq!(e.model, "mock");
        }
    }

    #[tokio::test]
    async fn mock_embed_is_deterministic() {
        let mock = MockEmbeddingBackend::new();
        let req = || EmbedRequest {
            modality: Modality::Text,
            items: vec![EmbedItem::Text("stable".into())],
        };
        let a = mock.embed(req()).await.expect("first");
        let b = mock.embed(req()).await.expect("second");
        assert_eq!(a[0].vec, b[0].vec);
    }

    #[tokio::test]
    async fn mock_embed_rejects_unsupported_modality() {
        let mock = MockEmbeddingBackend::new();
        let request = EmbedRequest {
            modality: Modality::Image,
            items: vec![EmbedItem::Image {
                bytes: vec![0, 1, 2],
                mime: "image/png".into(),
            }],
        };
        let err = mock
            .embed(request)
            .await
            .expect_err("image modality should be rejected");
        assert!(matches!(err, EmbedError::UnsupportedModality(Modality::Image)));
    }
}
