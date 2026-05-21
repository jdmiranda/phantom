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
use uuid::Uuid;

pub mod openai;
pub mod store;

use store::{EmbeddingStore, Metadata, StoreError};

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
    /// Returned when a required environment variable or external key is missing.
    #[error("not configured: {0}")]
    MissingConfig(String),
    /// Returned when a modality is a planned feature but not yet wired in the
    /// current environment. Distinguishes "will never work" from "not yet set
    /// up here".
    #[error("modality {modality:?} not configured: {reason}")]
    NotConfigured {
        modality: Modality,
        reason: &'static str,
    },
    /// Wraps a [`StoreError`] that occurs while persisting embeddings.
    #[error("store error: {0}")]
    Store(#[from] StoreError),
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

/// Combinator that wires an [`EmbeddingBackend`] to an [`EmbeddingStore`].
///
/// Every callsite that needs to embed and then persist the results can use this
/// type instead of manually threading the output of `embed()` into a store.
/// The coupling is enforced at construction time.
pub struct EmbeddingPipeline {
    backend: Box<dyn EmbeddingBackend + Send + Sync>,
    store: Box<dyn EmbeddingStore + Send + Sync>,
}

impl EmbeddingPipeline {
    /// Create a new pipeline from a backend and a store.
    #[must_use]
    pub fn new(
        backend: Box<dyn EmbeddingBackend + Send + Sync>,
        store: Box<dyn EmbeddingStore + Send + Sync>,
    ) -> Self {
        Self { backend, store }
    }

    /// Embed `items` and write the resulting vectors into the store.
    ///
    /// Each embedding is stored with a UUID derived from `bundle_id` and the
    /// item's index within the batch, along with a `bundle_id` metadata key.
    /// Returns the produced [`Embedding`] slice in input order.
    ///
    /// # Errors
    ///
    /// Propagates [`EmbedError`] from the backend or [`EmbedError::Store`] from
    /// the persistence layer.
    pub async fn embed_and_store(
        &mut self,
        modality: Modality,
        items: Vec<EmbedItem>,
        bundle_id: u64,
    ) -> Result<Vec<Embedding>, EmbedError> {
        let result = self
            .backend
            .embed(EmbedRequest { modality, items })
            .await?;

        for (i, embedding) in result.iter().enumerate() {
            // Derive a stable UUID from the bundle_id + item index so that
            // re-embedding the same bundle overwrites rather than duplicates.
            let uuid = Uuid::from_u128(u128::from(bundle_id) << 32 | i as u128);
            let mut meta = Metadata::new();
            meta.insert(
                "bundle_id".to_string(),
                serde_json::Value::Number(bundle_id.into()),
            );
            self.store.insert(uuid, embedding.vec.clone(), meta)?;
        }

        Ok(result)
    }
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
            return Err(EmbedError::NotConfigured {
                modality: request.modality,
                reason: "Image and Audio embedding backends are not yet configured. \
                         Set CLIP_API_KEY or IMAGEBIND_ENDPOINT to enable.",
            });
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
    async fn mock_embed_rejects_unsupported_modality_as_not_configured() {
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
        assert!(
            matches!(err, EmbedError::NotConfigured { modality: Modality::Image, .. }),
            "expected NotConfigured for Image, got {err:?}",
        );
    }

    /// Verifies that Image and Audio modalities return `NotConfigured` rather
    /// than the generic `UnsupportedModality`, surfacing the actionable message.
    #[tokio::test]
    async fn image_modality_returns_not_configured_not_unsupported() {
        let mock = MockEmbeddingBackend::new();
        let request = EmbedRequest {
            modality: Modality::Image,
            items: vec![EmbedItem::Image {
                bytes: vec![1, 2, 3],
                mime: "image/jpeg".into(),
            }],
        };
        let err = mock
            .embed(request)
            .await
            .expect_err("Image should be rejected");

        // Must be NotConfigured, not UnsupportedModality.
        assert!(
            matches!(err, EmbedError::NotConfigured { modality: Modality::Image, .. }),
            "expected NotConfigured {{Image, ..}}, got {err:?}",
        );
        assert!(
            !matches!(err, EmbedError::UnsupportedModality(_)),
            "must not be UnsupportedModality",
        );
    }

    /// Verifies that `EmbeddingPipeline::embed_and_store` stores every returned
    /// embedding into the backing store.
    #[tokio::test]
    async fn embedding_pipeline_stores_result_on_success() {
        use crate::store::InMemoryStore;

        let backend = Box::new(MockEmbeddingBackend::with_dim(16));
        let store = Box::new(InMemoryStore::new());
        let mut pipeline = EmbeddingPipeline::new(backend, store);

        let items = vec![
            EmbedItem::Text("alpha".into()),
            EmbedItem::Text("beta".into()),
        ];
        let result = pipeline
            .embed_and_store(Modality::Text, items, 42)
            .await
            .expect("embed_and_store should succeed");

        assert_eq!(result.len(), 2, "should return two embeddings");
        assert_eq!(result[0].dim, 16);
        assert_eq!(result[1].dim, 16);

        // Verify that the store received both records.
        assert_eq!(pipeline.store.len(), 2, "store should contain two records");
    }

    /// Verifies that a backend failure is propagated and the store is not written.
    #[tokio::test]
    async fn embedding_pipeline_propagates_backend_error() {
        use crate::store::InMemoryStore;

        let backend = Box::new(MockEmbeddingBackend::new());
        let store = Box::new(InMemoryStore::new());
        let mut pipeline = EmbeddingPipeline::new(backend, store);

        // Audio modality is not supported by MockEmbeddingBackend.
        let items = vec![EmbedItem::Audio {
            samples: vec![0.0, 1.0],
            sample_rate: 16_000,
        }];
        let err = pipeline
            .embed_and_store(Modality::Audio, items, 99)
            .await
            .expect_err("audio backend should fail");

        assert!(
            matches!(err, EmbedError::NotConfigured { modality: Modality::Audio, .. }),
            "expected NotConfigured for Audio, got {err:?}",
        );
        // Store must remain untouched because the backend failed.
        assert_eq!(pipeline.store.len(), 0, "store must be empty after backend error");
    }
}
