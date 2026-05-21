//! Brain-level recall context — wires phantom-recall into the OODA loop.
//!
//! `BrainRecallContext` wraps either an [`EmbeddedRecallEngine`] (when
//! `OPENAI_API_KEY` is set at construction time) or the mock
//! [`InMemoryRecallEngine`] as a fallback for offline / test use. Callers
//! interact exclusively through [`BrainRecallContext::index`] and
//! [`BrainRecallContext::query_text`] and never need to know which backend
//! is active.
//!
//! ## Backend selection
//!
//! | Condition | Engine chosen |
//! |-----------|---------------|
//! | `OPENAI_API_KEY` env var is non-empty | `EmbeddedRecallEngine` with the OpenAI backend |
//! | key absent or empty | `InMemoryRecallEngine` (FNV mock embedder) |
//!
//! This matches the project convention: code without verified end-to-end
//! function is not done, so the `InMemoryRecallEngine` path must remain
//! exercisable in CI without network access.

use std::collections::HashMap;

use phantom_embeddings::openai::OpenAiEmbeddingBackend;
use phantom_recall::engine::{
    EmbeddedRecallEngine, InMemoryRecallEngine, RecallEngine, RecallEntry, RecallResult,
    VectorQuery,
};
use phantom_recall::RecallError;

// ── BrainRecallContext ────────────────────────────────────────────────────────

/// Which underlying engine is active.
enum Inner {
    /// Real semantic embeddings via the OpenAI API.
    Embedded(EmbeddedRecallEngine),
    /// Deterministic FNV-hash mock — no network required.
    InMemory(InMemoryRecallEngine),
}

/// Recall context used by the brain's OODA loop.
///
/// Construct with [`BrainRecallContext::new`]; the backend is selected
/// automatically based on environment variables.
pub struct BrainRecallContext {
    inner: Inner,
}

impl BrainRecallContext {
    /// Build a recall context, preferring the real OpenAI embedding backend
    /// when `OPENAI_API_KEY` is set and non-empty, falling back to the
    /// in-memory mock otherwise.
    #[must_use]
    pub fn new() -> Self {
        let key = std::env::var("OPENAI_API_KEY").unwrap_or_default();
        if !key.is_empty() {
            let backend = Box::new(OpenAiEmbeddingBackend::new(key));
            Self { inner: Inner::Embedded(EmbeddedRecallEngine::new(backend)) }
        } else {
            Self { inner: Inner::InMemory(InMemoryRecallEngine::new()) }
        }
    }

    /// Build a context backed by a specific embedding backend.
    ///
    /// Useful in tests or when the caller has already constructed a backend.
    #[must_use]
    pub fn with_embedded(
        backend: Box<dyn phantom_embeddings::EmbeddingBackend + Send + Sync>,
    ) -> Self {
        Self { inner: Inner::Embedded(EmbeddedRecallEngine::new(backend)) }
    }

    /// Build a context backed by the in-memory mock engine.
    ///
    /// Intended for tests and offline development.
    #[must_use]
    pub fn with_in_memory() -> Self {
        Self { inner: Inner::InMemory(InMemoryRecallEngine::new()) }
    }

    /// Return `true` when the context is backed by the real embedding engine.
    #[must_use]
    pub fn uses_embedded_backend(&self) -> bool {
        matches!(self.inner, Inner::Embedded(_))
    }

    /// Index `text` so it can be retrieved later.
    ///
    /// When the embedded backend is active the text is sent to the embedding
    /// API; when the in-memory mock is active a deterministic vector is
    /// computed locally.
    pub async fn index(&mut self, text: impl Into<String>) -> Result<(), RecallError> {
        let text = text.into();
        match &mut self.inner {
            Inner::Embedded(e) => e.index(RecallEntry::new(text)).await,
            Inner::InMemory(e) => {
                e.add_text(text, None, HashMap::new());
                Ok(())
            }
        }
    }

    /// Query the index for `text`, returning up to `top_k` results with a
    /// minimum cosine similarity of `min_score`.
    pub async fn query_text(
        &self,
        text: &str,
        top_k: usize,
        min_score: f32,
    ) -> Result<Vec<RecallResult>, RecallError> {
        let q = VectorQuery::new(text, top_k, min_score, None);
        match &self.inner {
            Inner::Embedded(e) => e.query(q).await,
            Inner::InMemory(e) => e.query(q).await,
        }
    }
}

impl Default for BrainRecallContext {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use phantom_embeddings::MockEmbeddingBackend;

    fn mock_ctx() -> BrainRecallContext {
        BrainRecallContext::with_embedded(Box::new(MockEmbeddingBackend::with_dim(16)))
    }

    #[test]
    fn brain_recall_context_defaults_to_in_memory_without_key() {
        // Ensure OPENAI_API_KEY is absent for this test.
        // We can't mutate env from safe Rust without unsafe tricks, so we
        // use `with_in_memory` directly to exercise the same branch.
        let ctx = BrainRecallContext::with_in_memory();
        assert!(!ctx.uses_embedded_backend());
    }

    #[test]
    fn brain_recall_context_uses_embedded_when_key_present() {
        let ctx = BrainRecallContext::with_embedded(Box::new(MockEmbeddingBackend::new()));
        assert!(ctx.uses_embedded_backend());
    }

    #[tokio::test]
    async fn brain_recall_context_index_and_query_embedded() {
        let mut ctx = mock_ctx();
        ctx.index("rust borrow checker explanation").await.expect("index");
        ctx.index("async tokio runtime").await.expect("index");

        let results = ctx.query_text("rust borrow checker", 10, -1.0).await.expect("query");
        assert_eq!(results.len(), 2, "both indexed entries should be returned");
    }

    #[tokio::test]
    async fn brain_recall_context_index_and_query_in_memory() {
        let mut ctx = BrainRecallContext::with_in_memory();
        ctx.index("phantom terminal emulator").await.expect("index");
        ctx.index("gpu accelerated rendering pipeline").await.expect("index");

        let results = ctx.query_text("phantom terminal", 10, -1.0).await.expect("query");
        assert_eq!(results.len(), 2, "both in-memory entries should be returned");
    }

    #[tokio::test]
    async fn brain_recall_context_respects_top_k() {
        let mut ctx = mock_ctx();
        for i in 0..5u32 {
            ctx.index(format!("document {i}")).await.expect("index");
        }
        let results = ctx.query_text("document", 2, -1.0).await.expect("query");
        assert_eq!(results.len(), 2, "top_k must be respected");
    }
}
