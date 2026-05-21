//! Recall context integration for the AI brain.
//!
//! [`BrainRecallContext`] wraps an [`InMemoryRecallEngine`] and provides a
//! simple interface for indexing command/output pairs and querying for
//! relevant context to inject into agent prompts.

use std::collections::HashMap;

use phantom_recall::engine::{InMemoryRecallEngine, RecallEngine, RecallResult, VectorQuery};

// ---------------------------------------------------------------------------
// BrainRecallContext
// ---------------------------------------------------------------------------

/// In-memory recall index for the AI brain.
///
/// Indexes command+output pairs as they are observed and answers natural-
/// language queries by returning the most similar corpus entries. The top-K
/// hits can be formatted as a Markdown prompt section and injected into agent
/// system prompts.
pub struct BrainRecallContext {
    engine: InMemoryRecallEngine,
}

impl BrainRecallContext {
    /// Create a new, empty recall context.
    #[must_use]
    pub fn new() -> Self {
        Self { engine: InMemoryRecallEngine::new() }
    }

    /// Add a command+output pair to the recall index.
    ///
    /// `tags` are stored in the entry's metadata under the key `"tags"` as a
    /// comma-separated list so they survive the untyped `HashMap<String,String>`
    /// metadata layer.
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

        self.engine.add_text(text, None, metadata);
    }

    /// Query for relevant context given a natural-language question.
    ///
    /// Returns up to `limit` results sorted by cosine-similarity score
    /// descending. Applies a minimum score of `0.0` so all matches that beat
    /// cosine-0 are returned (negative-cosine entries are excluded).
    pub fn query_relevant(&self, question: &str, limit: usize) -> Vec<RecallResult> {
        let q = VectorQuery::new(question, limit, 0.0, None);
        // Block on the async query using a single-use tokio runtime.
        // The in-memory engine never awaits real I/O so this is cheap.
        match build_rt().block_on(self.engine.query(q)) {
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

// ---------------------------------------------------------------------------
// Sync bridge — the in-memory engine is async but the brain runs synchronously.
// ---------------------------------------------------------------------------

/// Build a minimal single-use tokio runtime for blocking on in-memory async ops.
fn build_rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("BrainRecallContext: failed to build single-use tokio runtime")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // recall_query_compiles_with_enriched_query_field
    // -----------------------------------------------------------------------

    /// Verify that RecallQuery can be constructed with enriched_query = Some("test").
    ///
    /// This is the canonical smoke test for Fix 1: before the fix, this literal
    /// did not compile because the field was missing from the struct.
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

    // -----------------------------------------------------------------------
    // brain_recall_context_indexes_and_queries
    // -----------------------------------------------------------------------

    /// Index 3 commands and verify that a relevant query returns at least one hit.
    #[test]
    fn brain_recall_context_indexes_and_queries() {
        let mut ctx = BrainRecallContext::new();

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

    // -----------------------------------------------------------------------
    // format_for_prompt_returns_empty_on_no_hits
    // -----------------------------------------------------------------------

    /// An empty engine returns an empty string from format_for_prompt.
    #[test]
    fn format_for_prompt_returns_empty_on_no_hits() {
        let ctx = BrainRecallContext::new();
        let result = ctx.format_for_prompt("what happened yesterday", 5);
        assert!(result.is_empty(), "empty engine should produce empty prompt section");
    }

    // -----------------------------------------------------------------------
    // format_for_prompt_includes_hits
    // -----------------------------------------------------------------------

    /// After indexing, format_for_prompt includes the relevant context section.
    #[test]
    fn format_for_prompt_includes_hits() {
        let mut ctx = BrainRecallContext::new();
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
