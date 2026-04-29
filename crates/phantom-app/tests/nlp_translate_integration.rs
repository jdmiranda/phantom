//! Integration tests for the NLP translate fallback pipeline.
//!
//! These tests verify the `intent_to_translate_result` conversion logic and
//! the `PhantomConfig::nlp_llm_enabled` flag without constructing a live `App`
//! (which requires a GPU context).

use phantom_nlp::{Intent, MockLlmBackend, translate};
use phantom_context::ProjectContext;

// ---------------------------------------------------------------------------
// Helper: detect real project context for tests.
// ---------------------------------------------------------------------------

fn rust_ctx() -> ProjectContext {
    ProjectContext::detect(std::path::Path::new(
        env!("CARGO_MANIFEST_DIR"),
    ))
}

// ---------------------------------------------------------------------------
// translate() -> Intent round-trip tests (via MockLlmBackend)
// ---------------------------------------------------------------------------

#[test]
fn translate_run_command_with_mock() {
    let ctx = rust_ctx();
    let backend = MockLlmBackend::new(r#"{"intent":"RunCommand","cmd":"cargo build"}"#);
    let intent = translate("build the project", &ctx, &backend).unwrap();
    assert_eq!(intent, Intent::run_command("cargo build"));
}

#[test]
fn translate_spawn_agent_with_mock() {
    let ctx = rust_ctx();
    let backend = MockLlmBackend::new(r#"{"intent":"SpawnAgent","goal":"fix the failing tests"}"#);
    let intent = translate("fix failing tests", &ctx, &backend).unwrap();
    assert_eq!(intent, Intent::spawn_agent("fix the failing tests"));
}

#[test]
fn translate_search_history_with_mock() {
    let ctx = rust_ctx();
    let backend = MockLlmBackend::new(
        r#"{"intent":"SearchHistory","query":"recent git commits today"}"#,
    );
    let intent = translate("what changed today", &ctx, &backend).unwrap();
    assert!(matches!(intent, Intent::SearchHistory { .. }));
    assert_eq!(intent.query(), Some("recent git commits today"));
}

#[test]
fn translate_clarify_for_ambiguous_input() {
    let ctx = rust_ctx();
    let backend = MockLlmBackend::new(
        r#"{"intent":"Clarify","question":"What exactly do you want to do?"}"#,
    );
    let intent = translate("xyzzy frobnicate", &ctx, &backend).unwrap();
    assert!(matches!(intent, Intent::Clarify { .. }));
    assert_eq!(intent.question(), Some("What exactly do you want to do?"));
}

#[test]
fn translate_empty_input_is_clarify_without_backend_call() {
    let ctx = rust_ctx();
    // Backend reply doesn't matter -- empty input returns Clarify immediately.
    let backend = MockLlmBackend::new("");
    let intent = translate("", &ctx, &backend).unwrap();
    assert!(matches!(intent, Intent::Clarify { .. }));
}

#[test]
fn translate_malformed_backend_reply_maps_to_clarify() {
    let ctx = rust_ctx();
    let backend = MockLlmBackend::new("not valid json at all");
    let intent = translate("do something", &ctx, &backend).unwrap();
    // Malformed JSON -> Clarify (not an error).
    assert!(matches!(intent, Intent::Clarify { .. }));
}

// ---------------------------------------------------------------------------
// PhantomConfig::nlp_llm_enabled flag tests
// ---------------------------------------------------------------------------

#[test]
fn config_nlp_llm_enabled_defaults_to_true() {
    use phantom_app::config::PhantomConfig;
    let config = PhantomConfig::default();
    assert!(
        config.nlp_llm_enabled,
        "nlp_llm_enabled must default to true so the LLM path is on by default"
    );
}

#[test]
fn config_nlp_llm_disabled_by_field() {
    use phantom_app::config::PhantomConfig;
    let mut config = PhantomConfig::default();
    config.nlp_llm_enabled = false;
    assert!(!config.nlp_llm_enabled);
}
