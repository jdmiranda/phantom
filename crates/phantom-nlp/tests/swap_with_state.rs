//! Integration test: swap two mock `LlmBackend` implementations and verify the
//! second response after swap matches the new backend — without touching the
//! network (closes #383 acceptance criterion).
//!
//! # Running
//!
//! ```sh
//! cargo test -p phantom-nlp --features testing --test swap_with_state
//! ```
//!
//! The test validates:
//! 1. The static `LlmSkill` path via `phantom_skill_host::LlmHost::from_backend`.
//! 2. Swapping the `Arc<dyn LlmSkill>` pointer changes the reply.
//! 3. State (the `reply` field inside the first mock) is isolated from the second.

use std::sync::Arc;

use phantom_context::{Framework, PackageManager, ProjectCommands, ProjectContext, ProjectType};
use phantom_nlp::{Intent, LlmBackend, TranslateError, translate};
use phantom_skill_host::{LlmHost, LlmSkill, LlmSkillAdapter};

// ---------------------------------------------------------------------------
// Local scripted backend (avoids depending on MockLlmBackend feature gating)
// ---------------------------------------------------------------------------

/// A scripted backend that always returns a fixed `reply`.
struct ScriptedBackend {
    reply: String,
}

impl ScriptedBackend {
    fn new(reply: impl Into<String>) -> Self {
        Self { reply: reply.into() }
    }
}

impl LlmBackend for ScriptedBackend {
    fn name(&self) -> &'static str {
        "scripted"
    }

    fn complete(&self, _system_prompt: &str, _user_message: &str) -> Result<String, TranslateError> {
        Ok(self.reply.clone())
    }
}

// SAFETY: ScriptedBackend holds only a `String` — trivially Send + Sync.
unsafe impl Send for ScriptedBackend {}
// SAFETY: ScriptedBackend holds only a `String` — trivially Send + Sync.
unsafe impl Sync for ScriptedBackend {}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn minimal_ctx() -> ProjectContext {
    ProjectContext {
        root: "/tmp/swap-test".into(),
        name: "swap-test".into(),
        project_type: ProjectType::Rust,
        package_manager: PackageManager::Cargo,
        framework: Framework::None,
        commands: ProjectCommands {
            build: Some("cargo build".into()),
            test: Some("cargo test".into()),
            run: Some("cargo run".into()),
            lint: Some("cargo clippy".into()),
            format: Some("cargo fmt".into()),
        },
        git: None,
        rust_version: Some("1.80.0".into()),
        node_version: None,
        python_version: None,
    }
}

fn make_skill(reply: &str) -> Arc<dyn LlmSkill> {
    let backend: Arc<dyn LlmBackend + Send + Sync> =
        Arc::new(ScriptedBackend::new(reply));
    LlmHost::from_backend(backend)
}

// ---------------------------------------------------------------------------
// Test 1: Two consecutive backends return different responses
// ---------------------------------------------------------------------------

/// Instantiate an `LlmSkillAdapter` backed by a scripted backend, then swap
/// the `Arc` to a second scripted backend with a different reply and assert
/// the second call returns the new canned string.
#[test]
fn swap_mock_backend_changes_reply() {
    let ctx = minimal_ctx();

    // First backend: returns RunCommand JSON.
    let first_skill = make_skill(r#"{"intent":"RunCommand","cmd":"cargo build"}"#);
    let first_adapter = LlmSkillAdapter::new(Arc::clone(&first_skill));

    let intent1 = translate("build the project", &ctx, &first_adapter).unwrap();
    assert_eq!(
        intent1.cmd(),
        Some("cargo build"),
        "first backend should return RunCommand(cargo build)"
    );

    // Second backend: returns SpawnAgent JSON.
    let second_skill = make_skill(r#"{"intent":"SpawnAgent","goal":"fix the error"}"#);
    let second_adapter = LlmSkillAdapter::new(second_skill);

    // Same input, now routed through the second backend.
    let intent2 = translate("build the project", &ctx, &second_adapter).unwrap();
    assert_eq!(
        intent2.goal(),
        Some("fix the error"),
        "second backend should return SpawnAgent(fix the error)"
    );
}

// ---------------------------------------------------------------------------
// Test 2: First backend state does not bleed into second
// ---------------------------------------------------------------------------

#[test]
fn first_backend_state_isolated_from_second() {
    let ctx = minimal_ctx();

    let first_skill = make_skill(r#"{"intent":"Clarify","question":"First?"}"#);
    let first_adapter = LlmSkillAdapter::new(Arc::clone(&first_skill));

    let intent1 = translate("xyzzy", &ctx, &first_adapter).unwrap();
    assert!(matches!(intent1, Intent::Clarify { .. }));

    // Drop first skill explicitly to emphasise lifecycle.
    drop(first_skill);

    let second_skill =
        make_skill(r#"{"intent":"SearchHistory","query":"recent commits"}"#);
    let second_adapter = LlmSkillAdapter::new(second_skill);

    let intent2 = translate("xyzzy", &ctx, &second_adapter).unwrap();
    assert!(
        matches!(intent2, Intent::SearchHistory { .. }),
        "second backend should return SearchHistory, not Clarify from first"
    );
}

// ---------------------------------------------------------------------------
// Test 3: LlmSkill::name is forwarded correctly
// ---------------------------------------------------------------------------

#[test]
fn llm_skill_name_forwarded() {
    let skill = make_skill("{}");
    // StaticLlmSkill forwards to ScriptedBackend::name() which returns "scripted".
    assert_eq!(skill.name(), "scripted");
}

// ---------------------------------------------------------------------------
// Test 4: LlmSkillAdapter::complete maps through correctly
// ---------------------------------------------------------------------------

#[test]
fn llm_skill_adapter_complete_roundtrip() {
    let skill = make_skill("hello adapter");
    let adapter = LlmSkillAdapter::new(skill);
    // adapter implements LlmBackend — call complete() directly.
    let reply = adapter.complete("system", "user").unwrap();
    assert_eq!(reply, "hello adapter");
}

// ---------------------------------------------------------------------------
// Test 5: Error path propagates through adapter
// ---------------------------------------------------------------------------

struct ErrBackend;
impl LlmBackend for ErrBackend {
    fn name(&self) -> &'static str {
        "err"
    }
    fn complete(&self, _: &str, _: &str) -> Result<String, TranslateError> {
        Err(TranslateError::NotConfigured("no key".into()))
    }
}
// SAFETY: ErrBackend holds no data — trivially Send + Sync.
unsafe impl Send for ErrBackend {}
// SAFETY: ErrBackend holds no data — trivially Send + Sync.
unsafe impl Sync for ErrBackend {}

#[test]
fn llm_skill_adapter_error_propagates() {
    let backend: Arc<dyn LlmBackend + Send + Sync> = Arc::new(ErrBackend);
    let skill = LlmHost::from_backend(backend);
    let adapter = LlmSkillAdapter::new(skill);
    let err = adapter.complete("system", "user").unwrap_err();
    assert!(matches!(err, TranslateError::NotConfigured(_)));
}
