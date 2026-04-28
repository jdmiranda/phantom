//! OpenAI embeddings backend.
//!
//! Wraps the `/embeddings` endpoint for the `text-embedding-3-large` (3072-dim)
//! and `text-embedding-3-small` (1536-dim) models. Text and intent only —
//! image/audio modalities are rejected.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{EmbedError, EmbedItem, EmbedRequest, Embedding, EmbeddingBackend, Modality};

/// Default model identifier.
const MODEL_LARGE: &str = "text-embedding-3-large";
/// Smaller, cheaper model.
const MODEL_SMALL: &str = "text-embedding-3-small";
/// Vector dimension for `text-embedding-3-large`.
const DIM_LARGE: usize = 3072;
/// Vector dimension for `text-embedding-3-small`.
const DIM_SMALL: usize = 1536;
/// Default OpenAI API base URL.
const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// Embedding backend backed by the OpenAI HTTP API.
///
/// Construct via [`OpenAiEmbeddingBackend::from_env`] (reads `OPENAI_API_KEY`)
/// or [`OpenAiEmbeddingBackend::new`]. Defaults to `text-embedding-3-large`;
/// call [`OpenAiEmbeddingBackend::with_small`] to switch to the small model.
pub struct OpenAiEmbeddingBackend {
    api_key: String,
    model: String,
    base_url: String,
    dim: usize,
}

impl std::fmt::Debug for OpenAiEmbeddingBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Redact the API key — never let it print into logs or test output.
        f.debug_struct("OpenAiEmbeddingBackend")
            .field("api_key", &"<redacted>")
            .field("model", &self.model)
            .field("base_url", &self.base_url)
            .field("dim", &self.dim)
            .finish()
    }
}

impl OpenAiEmbeddingBackend {
    /// Build a backend from `OPENAI_API_KEY`.
    ///
    /// # Errors
    ///
    /// Returns [`EmbedError::NotConfigured`] if `OPENAI_API_KEY` is unset
    /// or empty.
    pub fn from_env() -> Result<Self, EmbedError> {
        let key = std::env::var("OPENAI_API_KEY").map_err(|_| {
            EmbedError::NotConfigured("OPENAI_API_KEY environment variable not set".into())
        })?;
        if key.is_empty() {
            return Err(EmbedError::NotConfigured(
                "OPENAI_API_KEY environment variable is empty".into(),
            ));
        }
        Ok(Self::new(key))
    }

    /// Build a backend with an explicit API key. Defaults to the large model.
    #[must_use]
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            model: MODEL_LARGE.to_string(),
            base_url: DEFAULT_BASE_URL.to_string(),
            dim: DIM_LARGE,
        }
    }

    /// Switch this backend to the smaller (1536-dim) model.
    #[must_use]
    pub fn with_small(mut self) -> Self {
        self.model = MODEL_SMALL.to_string();
        self.dim = DIM_SMALL;
        self
    }

    /// Override the base URL (used by tests against a mock server).
    #[must_use]
    pub fn with_base_url(mut self, base_url: String) -> Self {
        self.base_url = base_url;
        self
    }
}

#[derive(Serialize)]
struct EmbeddingsRequest<'a> {
    model: &'a str,
    input: Vec<&'a str>,
}

#[derive(Deserialize)]
struct EmbeddingsResponse {
    data: Vec<EmbeddingData>,
}

#[derive(Deserialize)]
struct EmbeddingData {
    embedding: Vec<f32>,
    index: usize,
}

#[async_trait]
impl EmbeddingBackend for OpenAiEmbeddingBackend {
    fn name(&self) -> &'static str {
        "openai-embedding"
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

        // All items must be Text. Reject image/audio payloads up front.
        let mut texts: Vec<&str> = Vec::with_capacity(request.items.len());
        for item in &request.items {
            match item {
                EmbedItem::Text(s) => texts.push(s.as_str()),
                EmbedItem::Image { .. } => {
                    return Err(EmbedError::UnsupportedModality(Modality::Image));
                }
                EmbedItem::Audio { .. } => {
                    return Err(EmbedError::UnsupportedModality(Modality::Audio));
                }
            }
        }

        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let body = EmbeddingsRequest {
            model: &self.model,
            input: texts,
        };
        let url = format!("{}/embeddings", self.base_url);

        let client = reqwest::Client::new();
        let resp = client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| EmbedError::Backend(format!("request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(EmbedError::Backend(format!(
                "OpenAI returned {status}: {text}"
            )));
        }

        let parsed: EmbeddingsResponse = resp
            .json()
            .await
            .map_err(|e| EmbedError::Backend(format!("decode failed: {e}")))?;

        // Sort by `index` so output order matches input order regardless of
        // server-side reordering. OpenAI guarantees ordering today, but the
        // field exists for a reason — be defensive.
        let mut data = parsed.data;
        data.sort_by_key(|d| d.index);

        let model = self.model.clone();
        let out = data
            .into_iter()
            .map(|d| Embedding {
                dim: d.embedding.len(),
                vec: d.embedding,
                model: model.clone(),
            })
            .collect();
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guard against parallel env mutation inside the test process.
    fn with_env_var<F: FnOnce()>(key: &str, value: Option<&str>, f: F) {
        // Cargo runs tests in parallel by default; using a Mutex keeps these
        // env-var tests honest without forcing `--test-threads=1`.
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let prior = std::env::var(key).ok();
        // SAFETY: tests run inside this serialized region, so the process-wide
        // env table isn't being mutated concurrently here.
        unsafe {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
        f();
        unsafe {
            match prior {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }

    #[test]
    fn from_env_returns_not_configured_when_missing() {
        with_env_var("OPENAI_API_KEY", None, || {
            let err = OpenAiEmbeddingBackend::from_env()
                .expect_err("missing key should fail");
            assert!(matches!(err, EmbedError::NotConfigured(_)));
        });
    }

    #[test]
    fn from_env_succeeds_with_key() {
        with_env_var("OPENAI_API_KEY", Some("sk-test-fixture"), || {
            let backend = OpenAiEmbeddingBackend::from_env()
                .expect("present key should succeed");
            assert_eq!(backend.model, MODEL_LARGE);
            assert_eq!(backend.dim, DIM_LARGE);
            assert_eq!(backend.api_key, "sk-test-fixture");
        });
    }

    #[test]
    fn with_small_uses_small_model_and_1536_dim() {
        let backend = OpenAiEmbeddingBackend::new("sk-x".into()).with_small();
        assert_eq!(backend.model, MODEL_SMALL);
        assert_eq!(backend.dim, DIM_SMALL);
        assert_eq!(backend.dimension_for(Modality::Text), Some(1536));
    }

    #[test]
    fn supports_text_and_intent_only() {
        let backend = OpenAiEmbeddingBackend::new("sk-x".into());
        assert!(backend.supports(Modality::Text));
        assert!(backend.supports(Modality::Intent));
        assert!(!backend.supports(Modality::Image));
        assert!(!backend.supports(Modality::Audio));
    }

    #[tokio::test]
    async fn embed_image_returns_unsupported_modality() {
        let backend = OpenAiEmbeddingBackend::new("sk-x".into());
        let request = EmbedRequest {
            modality: Modality::Image,
            items: vec![EmbedItem::Image {
                bytes: vec![0, 1, 2],
                mime: "image/png".into(),
            }],
        };
        let err = backend
            .embed(request)
            .await
            .expect_err("image modality should be rejected");
        assert!(matches!(err, EmbedError::UnsupportedModality(Modality::Image)));
    }

    #[test]
    fn dimension_for_text_returns_default_3072() {
        let backend = OpenAiEmbeddingBackend::new("sk-x".into());
        assert_eq!(backend.dimension_for(Modality::Text), Some(3072));
        assert_eq!(backend.dimension_for(Modality::Intent), Some(3072));
        assert_eq!(backend.dimension_for(Modality::Image), None);
        assert_eq!(backend.dimension_for(Modality::Audio), None);
    }

    /// Live integration test — requires a real `OPENAI_API_KEY` and network.
    /// Run with: `cargo test -p phantom-embeddings -- --ignored`.
    #[tokio::test]
    #[ignore = "requires live OpenAI API key + network"]
    async fn embed_hello_world_returns_two_3072_dim_vectors() {
        let backend = OpenAiEmbeddingBackend::from_env()
            .expect("set OPENAI_API_KEY for live test");
        let request = EmbedRequest {
            modality: Modality::Text,
            items: vec![
                EmbedItem::Text("hello".into()),
                EmbedItem::Text("world".into()),
            ],
        };
        let out = backend.embed(request).await.expect("live call should succeed");
        assert_eq!(out.len(), 2);
        for e in &out {
            assert_eq!(e.dim, 3072);
            assert_eq!(e.vec.len(), 3072);
            assert_eq!(e.model, MODEL_LARGE);
        }
    }
}
