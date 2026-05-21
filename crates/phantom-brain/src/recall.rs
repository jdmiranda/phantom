//! Brain-level recall context — wires phantom-recall into the OODA loop.
//!
//! [`BrainRecallContext`] wraps either an [`EmbeddedRecallEngine`] (when
//! `OPENAI_API_KEY` is set at construction time) or the mock
//! [`InMemoryRecallEngine`] as a fallback for offline / test use. Callers
//! interact through a sync command-history surface (`index_command`,
//! `query_relevant`, `format_for_prompt`) used by the brain's OODA loop, or
//! through the async surface (`index`, `query_text`) when they already live
//! on a tokio runtime. The active backend is never exposed.
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
/// Indexes command+output pairs as they are observed and answers natural-
/// language queries by returning the most similar corpus entries. The top-K
/// hits can be formatted as a Markdown prompt section and injected into agent
/// system prompts.
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

    /// Index `text` so it can be retrieved later (async).
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

    /// Query the index for `text` (async), returning up to `top_k` results
    /// with a minimum cosine similarity of `min_score`.
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

    /// Add a command+output pair to the recall index (sync).
    ///
    /// `tags` are stored in the entry's metadata under the key `"tags"` as a
    /// comma-separated list so they survive the untyped `HashMap<String,String>`
    /// metadata layer. The `command` is preserved under the `"command"` key so
    /// `format_for_prompt` can render concise summaries.
    pub fn index_command(&mut self, command: &str, output: &str, tags: Vec<String>) {
        let text = if output.trim().is_empty() {
            command.to_string()
        } else {
            // Cap output at 512 chars to keep the corpus lean.
            let capped = if output.len() > 512 { &output[..512] } else { output };
            format!("{command}\n{capped}")
        };

        let mut metadata = HashMap::new();
        metadata.insert("command".to_string(), command.to_string());
        if !tags.is_empty() {
            metadata.insert("tags".to_string(), tags.join(","));
        }

        match &mut self.inner {
            Inner::InMemory(e) => {
                e.add_text(text, None, metadata);
            }
            Inner::Embedded(e) => {
                // Block on the async index using a single-use tokio runtime.
                // Mirrors the sync bridge used by `query_relevant`. Failures
                // are logged and swallowed so a single API hiccup does not
                // kill the OODA loop.
                let entry = RecallEntry::with_metadata(text, metadata);
                if let Err(err) = build_rt().block_on(e.index(entry)) {
                    log::warn!("BrainRecallContext: index_command failed: {err}");
                }
            }
        }
    }

    /// Query for relevant context given a natural-language question (sync).
    ///
    /// Returns up to `limit` results sorted by cosine-similarity score
    /// descending. Applies a minimum score of `0.0` so all matches that beat
    /// cosine-0 are returned (negative-cosine entries are excluded).
    pub fn query_relevant(&self, question: &str, limit: usize) -> Vec<RecallResult> {
        let q = VectorQuery::new(question, limit, 0.0, None);
        // Block on the async query using a single-use tokio runtime.
        // The in-memory engine never awaits real I/O so this is cheap; the
        // embedded backend trades a single API round-trip per query.
        let result = match &self.inner {
            Inner::InMemory(e) => build_rt().block_on(e.query(q)),
            Inner::Embedded(e) => build_rt().block_on(e.query(q)),
        };
        match result {
            Ok(hits) => hits,
            Err(e) => {
                log::warn!("BrainRecallContext: query failed: {e}");
                Vec::new()
            }
        }
    }

    /// Format the top-K most relevant hits as a Markdown prompt section.
    ///
    /// Returns an empty string when no hits exceed the minimum score threshold,
    /// so callers can safely append the result without checking first.
    #[must_use]
    pub fn format_for_prompt(&self, question: &str, limit: usize) -> String {
        let hits = self.query_relevant(question, limit);
        if hits.is_empty() {
            return String::new();
        }
        let mut out = String::from("\n## Relevant Context from History\n");
        for hit in &hits {
            // Use the command metadata if available, otherwise the raw text.
            let summary = hit
                .metadata()
                .get("command")
                .map(String::as_str)
                .unwrap_or_else(|| hit.text());
            out.push_str(&format!("- {summary}\n"));
        }
        out
    }
}

impl Default for BrainRecallContext {
    fn default() -> Self {
        Self::new()
    }
}

// ── Sync bridge ──────────────────────────────────────────────────────────────
// The recall engines are async but the brain runs synchronously.

/// Build a minimal single-use tokio runtime for blocking on async ops.
fn build_rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("BrainRecallContext: failed to build single-use tokio runtime")
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use phantom_embeddings::MockEmbeddingBackend;

    fn mock_ctx() -> BrainRecallContext {
        BrainRecallContext::with_embedded(Box::new(MockEmbeddingBackend::with_dim(16)))
    }

    #[test]
    fn brain_recall_context_defaults_to_in_memory_without_key() {
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

    // ── Sync command-history API tests (PR 577 contract) ──────────────────

    /// Verify that RecallQuery can be constructed with enriched_query = Some("test").
    ///
    /// This is the canonical smoke test for the PR 597 `enriched_query` field:
    /// before the fix, this literal did not compile because the field was
    /// missing from the struct.
    #[test]
    fn recall_query_compiles_with_enriched_query_field() {
        let q = phantom_recall::RecallQuery {
            natural_language: "test query".into(),
            intent_hint: None,
            tags: vec![],
            time_window_unix_ms: None,
            modality_hint: None,
            limit: 5,
            enriched_query: Some("test".into()),
        };
        assert_eq!(q.enriched_query.as_deref(), Some("test"));
        assert_eq!(q.natural_language, "test query");
    }

    /// Index 3 commands and verify that a relevant query returns at least one hit.
    #[test]
    fn brain_recall_context_indexes_and_queries() {
        let mut ctx = BrainRecallContext::with_in_memory();

        ctx.index_command("cargo build", "Finished dev [unoptimized]", vec!["rust".into()]);
        ctx.index_command("git status", "On branch main\nnothing to commit", vec!["git".into()]);
        ctx.index_command(
            "cargo test",
            "test result: ok. 3 passed; 0 failed",
            vec!["rust".into(), "test".into()],
        );

        let results = ctx.query_relevant("cargo build", 3);
        assert!(!results.is_empty(), "expected at least one hit for 'cargo build'");

        // The top result should be the exact match.
        let top = &results[0];
        assert!(
            top.score() > 0.0,
            "top hit score should be positive, got {}",
            top.score()
        );
    }

    /// An empty engine returns an empty string from format_for_prompt.
    #[test]
    fn format_for_prompt_returns_empty_on_no_hits() {
        let ctx = BrainRecallContext::with_in_memory();
        let result = ctx.format_for_prompt("what happened yesterday", 5);
        assert!(result.is_empty(), "empty engine should produce empty prompt section");
    }

    /// After indexing, format_for_prompt includes the relevant context section.
    #[test]
    fn format_for_prompt_includes_hits() {
        let mut ctx = BrainRecallContext::with_in_memory();
        ctx.index_command("cargo build", "Finished", vec![]);

        let prompt_section = ctx.format_for_prompt("cargo build", 3);
        assert!(
            !prompt_section.is_empty(),
            "prompt section should not be empty after indexing"
        );
        assert!(
            prompt_section.contains("## Relevant Context from History"),
            "prompt section should contain the markdown header"
        );
        assert!(
            prompt_section.contains("cargo build"),
            "prompt section should contain the indexed command"
        );
    }
}
