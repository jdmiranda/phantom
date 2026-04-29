//! LLM-backed natural-language → [`Intent`] translation.
//!
//! # Architecture
//!
//! The public surface is a single function [`translate`] that accepts raw text
//! and a [`ProjectContext`], sends a few-shot prompt to an [`LlmBackend`], and
//! parses the JSON response into a typed [`Intent`].
//!
//! The [`LlmBackend`] trait is the seam for testing: production code uses
//! [`ClaudeLlmBackend`] (reads `ANTHROPIC_API_KEY` from the environment),
//! while tests inject [`MockLlmBackend`].
//!
//! # Prompt design
//!
//! The system prompt explains the four intent types and provides three few-shot
//! examples (one per category) before asking the model to classify the user's
//! input. The model is instructed to output *only* valid JSON of the shape
//! `{"intent": "...", ...}` — no markdown, no prose. Low-confidence cases
//! produce `{"intent": "Clarify", "question": "..."}`.
//!
//! # Fallback
//!
//! If the model's response cannot be parsed as valid JSON, or if the `intent`
//! field is unrecognised, [`translate`] returns
//! `Intent::clarify("Could not parse response from model")` rather than an
//! error. Network / auth failures still propagate as [`TranslateError`].

use serde::Deserialize;

use phantom_context::ProjectContext;

// ---------------------------------------------------------------------------
// Public error type
// ---------------------------------------------------------------------------

/// Errors that can occur during LLM-backed translation.
///
/// Does **not** cover low-confidence classification — that case returns
/// [`Intent::Clarify`] rather than an error.
#[derive(Debug, thiserror::Error)]
pub enum TranslateError {
    /// The backend is not configured (e.g. missing API key).
    #[error("backend not configured: {0}")]
    NotConfigured(String),
    /// The HTTP request to the LLM API failed.
    #[error("transport error: {0}")]
    Transport(String),
    /// Any other error.
    #[error("{0}")]
    Other(String),
}

// ---------------------------------------------------------------------------
// Intent
// ---------------------------------------------------------------------------

/// The structured result of translating a natural-language command.
#[derive(Debug, Clone, PartialEq)]
pub enum Intent {
    /// Execute a concrete shell command.
    RunCommand {
        /// The shell command to run (e.g. `"cargo build"`).
        cmd: String,
    },
    /// Search command history for recent activity.
    SearchHistory {
        /// What to search for (e.g. `"recent git commits"`).
        query: String,
    },
    /// Spawn an AI agent with a high-level goal.
    SpawnAgent {
        /// The agent's task description (e.g. `"fix the last error"`).
        goal: String,
    },
    /// The input is ambiguous — ask the user for clarification.
    Clarify {
        /// The clarifying question to show the user.
        question: String,
    },
}

impl Intent {
    /// Convenience constructor for a [`Intent::RunCommand`].
    pub fn run_command(cmd: impl Into<String>) -> Self {
        Self::RunCommand { cmd: cmd.into() }
    }

    /// Convenience constructor for a [`Intent::SearchHistory`].
    pub fn search_history(query: impl Into<String>) -> Self {
        Self::SearchHistory {
            query: query.into(),
        }
    }

    /// Convenience constructor for a [`Intent::SpawnAgent`].
    pub fn spawn_agent(goal: impl Into<String>) -> Self {
        Self::SpawnAgent { goal: goal.into() }
    }

    /// Convenience constructor for a [`Intent::Clarify`].
    pub fn clarify(question: impl Into<String>) -> Self {
        Self::Clarify {
            question: question.into(),
        }
    }

    // -- field accessors -------------------------------------------------------

    /// Return the `cmd` field if this is a [`Intent::RunCommand`], else `None`.
    #[must_use]
    pub fn cmd(&self) -> Option<&str> {
        match self {
            Self::RunCommand { cmd } => Some(cmd),
            _ => None,
        }
    }

    /// Return the `query` field if this is a [`Intent::SearchHistory`], else `None`.
    #[must_use]
    pub fn query(&self) -> Option<&str> {
        match self {
            Self::SearchHistory { query } => Some(query),
            _ => None,
        }
    }

    /// Return the `goal` field if this is a [`Intent::SpawnAgent`], else `None`.
    #[must_use]
    pub fn goal(&self) -> Option<&str> {
        match self {
            Self::SpawnAgent { goal } => Some(goal),
            _ => None,
        }
    }

    /// Return the `question` field if this is a [`Intent::Clarify`], else `None`.
    #[must_use]
    pub fn question(&self) -> Option<&str> {
        match self {
            Self::Clarify { question } => Some(question),
            _ => None,
        }
    }

    /// Human-readable name of this intent variant (for logging).
    #[must_use]
    pub fn variant_name(&self) -> &'static str {
        match self {
            Self::RunCommand { .. } => "RunCommand",
            Self::SearchHistory { .. } => "SearchHistory",
            Self::SpawnAgent { .. } => "SpawnAgent",
            Self::Clarify { .. } => "Clarify",
        }
    }
}

// ---------------------------------------------------------------------------
// LlmBackend trait
// ---------------------------------------------------------------------------

/// A minimal text-completion backend used by [`translate`].
///
/// The trait is intentionally narrow: it sends a system prompt + user message
/// and returns the assistant's reply as a `String`. This makes mock
/// implementations trivial and avoids coupling `phantom-nlp` to the full
/// `phantom-agents` chat stack.
pub trait LlmBackend: Send + Sync {
    /// Stable name for logging.
    fn name(&self) -> &'static str;

    /// Send `user_message` with `system_prompt` context and return the reply.
    ///
    /// Implementations **must not** return an empty string on success — callers
    /// treat an empty reply as a parse failure and return [`Intent::Clarify`].
    fn complete(&self, system_prompt: &str, user_message: &str) -> Result<String, TranslateError>;
}

// ---------------------------------------------------------------------------
// ClaudeLlmBackend — real production backend
// ---------------------------------------------------------------------------

const CLAUDE_API_URL: &str = "https://api.anthropic.com/v1/messages";
const CLAUDE_API_VERSION: &str = "2023-06-01";
const CLAUDE_DEFAULT_MODEL: &str = "claude-sonnet-4-20250514";

/// Anthropic Claude backend for [`translate`].
///
/// Reads `ANTHROPIC_API_KEY` from the environment.  Enabled in production;
/// tests use [`MockLlmBackend`] instead.
#[derive(Debug)]
pub struct ClaudeLlmBackend {
    api_key: String,
    model: String,
}

impl ClaudeLlmBackend {
    /// Construct from the `ANTHROPIC_API_KEY` environment variable.
    ///
    /// # Errors
    ///
    /// Returns [`TranslateError::NotConfigured`] when the variable is absent
    /// or empty.
    pub fn from_env() -> Result<Self, TranslateError> {
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .map_err(|_| TranslateError::NotConfigured("ANTHROPIC_API_KEY is not set".into()))?;
        if api_key.is_empty() {
            return Err(TranslateError::NotConfigured(
                "ANTHROPIC_API_KEY is empty".into(),
            ));
        }
        Ok(Self {
            api_key,
            model: CLAUDE_DEFAULT_MODEL.to_owned(),
        })
    }

    /// Override the model identifier.
    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    /// Return the model identifier in use.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }
}

impl LlmBackend for ClaudeLlmBackend {
    fn name(&self) -> &'static str {
        "claude"
    }

    fn complete(&self, system_prompt: &str, user_message: &str) -> Result<String, TranslateError> {
        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": 256,
            "system": system_prompt,
            "messages": [
                {"role": "user", "content": user_message}
            ]
        });

        // Use the OS certificate store to avoid `UnknownIssuer` errors on
        // some platforms (same pattern as the OpenAI backend in phantom-agents).
        let tls = ureq::tls::TlsConfig::builder()
            .root_certs(ureq::tls::RootCerts::PlatformVerifier)
            .build();
        let agent = ureq::Agent::new_with_config(
            ureq::config::Config::builder()
                .tls_config(tls)
                .timeout_global(Some(std::time::Duration::from_secs(30)))
                .build(),
        );

        let body_str = serde_json::to_string(&body)
            .map_err(|e| TranslateError::Other(format!("serialise error: {e}")))?;

        let mut response = agent
            .post(CLAUDE_API_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", CLAUDE_API_VERSION)
            .header("content-type", "application/json")
            .send(body_str.as_bytes())
            .map_err(|e| TranslateError::Transport(e.to_string()))?;

        let text = response
            .body_mut()
            .read_to_string()
            .map_err(|e| TranslateError::Transport(format!("read body: {e}")))?;

        let json: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| TranslateError::Other(format!("parse response: {e}")))?;

        // Surface API-level errors (e.g. invalid API key, rate limit).
        if let Some(err) = json.get("error") {
            let msg = err
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown API error");
            return Err(TranslateError::Transport(msg.to_owned()));
        }

        // Extract the first text block from `content[]`.
        let content = json
            .get("content")
            .and_then(|c| c.as_array())
            .and_then(|arr| {
                arr.iter()
                    .find(|block| block.get("type").and_then(|t| t.as_str()) == Some("text"))
            })
            .and_then(|block| block.get("text"))
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_owned();

        Ok(content)
    }
}

// ---------------------------------------------------------------------------
// MockLlmBackend — for tests only
// ---------------------------------------------------------------------------

/// A scripted [`LlmBackend`] that returns a predetermined reply.
///
/// Used in unit tests and integration tests (via the `testing` feature) so
/// they never hit the network.
#[cfg(any(test, feature = "testing"))]
pub struct MockLlmBackend {
    reply: String,
}

#[cfg(any(test, feature = "testing"))]
impl MockLlmBackend {
    /// Construct a mock that always returns `reply`.
    pub fn new(reply: impl Into<String>) -> Self {
        Self {
            reply: reply.into(),
        }
    }
}

#[cfg(any(test, feature = "testing"))]
impl LlmBackend for MockLlmBackend {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn complete(
        &self,
        _system_prompt: &str,
        _user_message: &str,
    ) -> Result<String, TranslateError> {
        Ok(self.reply.clone())
    }
}

// ---------------------------------------------------------------------------
// Prompt construction
// ---------------------------------------------------------------------------

/// Build the system prompt with few-shot examples and project context.
fn build_system_prompt(ctx: &ProjectContext) -> String {
    // The project context block gives the model enough signal to resolve
    // "build the project" → the right command without hallucinating.
    let project_block = ctx.agent_context();

    format!(
        r#"You are a natural-language command interpreter embedded in a terminal emulator.
Your job: classify the user's input into exactly one of four intents and respond with
a single JSON object — no markdown, no prose, no explanation.

## Intent schema

RunCommand   — run a shell command directly.
  {{"intent": "RunCommand", "cmd": "<shell command>"}}

SearchHistory — search recent command/git history for information.
  {{"intent": "SearchHistory", "query": "<search topic>"}}

SpawnAgent   — delegate to an AI agent for open-ended tasks.
  {{"intent": "SpawnAgent", "goal": "<task description>"}}

Clarify      — the input is ambiguous or unrecognisable; ask for clarification.
  {{"intent": "Clarify", "question": "<clarifying question>"}}

## Rules

1. Prefer RunCommand when the request maps cleanly to a single known shell invocation.
2. Use SearchHistory when the user asks "what changed", "recent commits", history queries.
3. Use SpawnAgent for open-ended tasks that require reasoning (fix, explain, analyse).
4. Use Clarify when confidence is low, the input is ambiguous, or no intent matches.
5. Output ONLY the JSON object. No other text.

## Few-shot examples

User: build the project
{{"intent": "RunCommand", "cmd": "cargo build"}}

User: what changed today
{{"intent": "SearchHistory", "query": "recent git commits today"}}

User: fix the failing tests
{{"intent": "SpawnAgent", "goal": "fix the failing tests"}}

User: zork
{{"intent": "Clarify", "question": "I'm not sure what you mean by 'zork'. Could you be more specific?"}}

## Project context

{project_block}

Respond with a single JSON object matching one of the schemas above."#
    )
}

// ---------------------------------------------------------------------------
// JSON response parsing
// ---------------------------------------------------------------------------

/// Wire shape of the model's response — we only decode the fields we need.
#[derive(Deserialize)]
struct IntentResponse {
    intent: String,
    // Variant-specific payloads — all optional so serde tolerates missing keys.
    cmd: Option<String>,
    query: Option<String>,
    goal: Option<String>,
    question: Option<String>,
}

/// Parse the model's raw JSON reply into an [`Intent`].
///
/// Returns [`Intent::Clarify`] (not an error) when the reply is unparseable,
/// so that callers always receive an actionable result.
fn parse_reply(raw: &str) -> Intent {
    // Strip markdown code fences in case the model wraps the JSON despite
    // being asked not to.
    let trimmed = raw.trim();
    let stripped = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .map(|s| s.trim_end_matches("```").trim())
        .unwrap_or(trimmed);

    let parsed: IntentResponse = match serde_json::from_str(stripped) {
        Ok(r) => r,
        Err(_) => {
            return Intent::clarify(
                "I couldn't understand that — could you rephrase your request?",
            );
        }
    };

    match parsed.intent.as_str() {
        "RunCommand" => {
            let cmd = parsed.cmd.unwrap_or_default();
            if cmd.is_empty() {
                Intent::clarify(
                    "I understood the intent but couldn't determine which command to run.",
                )
            } else {
                Intent::run_command(cmd)
            }
        }
        "SearchHistory" => {
            let query = parsed.query.unwrap_or_default();
            if query.is_empty() {
                Intent::search_history("recent activity")
            } else {
                Intent::search_history(query)
            }
        }
        "SpawnAgent" => {
            let goal = parsed.goal.unwrap_or_default();
            if goal.is_empty() {
                Intent::clarify("I understood the intent but couldn't determine the agent's goal.")
            } else {
                Intent::spawn_agent(goal)
            }
        }
        "Clarify" => {
            let question = parsed
                .question
                .unwrap_or_else(|| "Could you clarify your request?".to_owned());
            Intent::clarify(question)
        }
        other => {
            log::warn!("nlp: unrecognised intent variant from model: {other:?}");
            Intent::clarify("I received an unexpected response. Could you rephrase?")
        }
    }
}

// ---------------------------------------------------------------------------
// Public translate function
// ---------------------------------------------------------------------------

/// Translate `input` into a typed [`Intent`] using `backend` for LLM routing.
///
/// # Errors
///
/// Returns [`TranslateError`] only on network or configuration failures.
/// Low-confidence classifications return [`Intent::Clarify`], not an error.
///
/// # Examples
///
/// ```no_run
/// use phantom_nlp::translate::{ClaudeLlmBackend, translate};
/// use phantom_context::ProjectContext;
/// use std::path::Path;
///
/// let ctx = ProjectContext::detect(Path::new("."));
/// let backend = ClaudeLlmBackend::from_env().expect("ANTHROPIC_API_KEY must be set");
/// let intent = translate("build the project", &ctx, &backend).expect("translate failed");
/// println!("{}", intent.variant_name());
/// ```
pub fn translate(
    input: &str,
    ctx: &ProjectContext,
    backend: &dyn LlmBackend,
) -> Result<Intent, TranslateError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(Intent::clarify("Please enter a command or question."));
    }

    let system_prompt = build_system_prompt(ctx);
    let reply = backend.complete(&system_prompt, trimmed)?;

    if reply.is_empty() {
        log::warn!("nlp: backend '{}' returned empty reply", backend.name());
        return Ok(Intent::clarify(
            "I received an empty response. Could you rephrase?",
        ));
    }

    Ok(parse_reply(&reply))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use phantom_context::{
        Framework, GitInfo, PackageManager, ProjectCommands, ProjectContext, ProjectType,
    };

    // -------------------------------------------------------------------------
    // Test fixtures
    // -------------------------------------------------------------------------

    fn rust_ctx() -> ProjectContext {
        ProjectContext {
            root: "/tmp/my-rust-project".into(),
            name: "my-crate".into(),
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
            git: Some(GitInfo {
                branch: "main".into(),
                remote_url: None,
                is_dirty: false,
                ahead: 0,
                behind: 0,
                last_commit_message: None,
                last_commit_age: None,
            }),
            rust_version: Some("1.79.0".into()),
            node_version: None,
            python_version: None,
        }
    }

    fn node_ctx() -> ProjectContext {
        ProjectContext {
            root: "/tmp/my-node-app".into(),
            name: "my-app".into(),
            project_type: ProjectType::Node,
            package_manager: PackageManager::Pnpm,
            framework: Framework::NextJs,
            commands: ProjectCommands {
                build: Some("pnpm build".into()),
                test: Some("pnpm test".into()),
                run: Some("pnpm dev".into()),
                lint: Some("pnpm lint".into()),
                format: Some("pnpm format".into()),
            },
            git: None,
            rust_version: None,
            node_version: Some("20.11.0".into()),
            python_version: None,
        }
    }

    // -------------------------------------------------------------------------
    // MockLlmBackend — construction and name
    // -------------------------------------------------------------------------

    #[test]
    fn mock_backend_name_is_mock() {
        let b = MockLlmBackend::new("{}");
        assert_eq!(b.name(), "mock");
    }

    #[test]
    fn mock_backend_returns_scripted_reply() {
        let b = MockLlmBackend::new(r#"{"intent":"RunCommand","cmd":"echo hi"}"#);
        let reply = b.complete("sys", "user").unwrap();
        assert_eq!(reply, r#"{"intent":"RunCommand","cmd":"echo hi"}"#);
    }

    // -------------------------------------------------------------------------
    // parse_reply unit tests
    // -------------------------------------------------------------------------

    #[test]
    fn parse_run_command_variant() {
        let raw = r#"{"intent":"RunCommand","cmd":"cargo build"}"#;
        let intent = parse_reply(raw);
        assert_eq!(intent, Intent::run_command("cargo build"));
    }

    #[test]
    fn parse_search_history_variant() {
        let raw = r#"{"intent":"SearchHistory","query":"recent git commits today"}"#;
        let intent = parse_reply(raw);
        assert_eq!(intent, Intent::search_history("recent git commits today"));
    }

    #[test]
    fn parse_spawn_agent_variant() {
        let raw = r#"{"intent":"SpawnAgent","goal":"fix the failing tests"}"#;
        let intent = parse_reply(raw);
        assert_eq!(intent, Intent::spawn_agent("fix the failing tests"));
    }

    #[test]
    fn parse_clarify_variant() {
        let raw = r#"{"intent":"Clarify","question":"What do you mean?"}"#;
        let intent = parse_reply(raw);
        assert_eq!(intent, Intent::clarify("What do you mean?"));
    }

    #[test]
    fn parse_invalid_json_falls_back_to_clarify() {
        let raw = "this is not json";
        let intent = parse_reply(raw);
        assert!(matches!(intent, Intent::Clarify { .. }));
    }

    #[test]
    fn parse_unknown_intent_falls_back_to_clarify() {
        let raw = r#"{"intent":"Explode","payload":"kaboom"}"#;
        let intent = parse_reply(raw);
        assert!(matches!(intent, Intent::Clarify { .. }));
    }

    #[test]
    fn parse_strips_markdown_code_fences() {
        let raw = "```json\n{\"intent\":\"RunCommand\",\"cmd\":\"ls -la\"}\n```";
        let intent = parse_reply(raw);
        assert_eq!(intent, Intent::run_command("ls -la"));
    }

    #[test]
    fn parse_run_command_missing_cmd_field_clarifies() {
        let raw = r#"{"intent":"RunCommand"}"#;
        let intent = parse_reply(raw);
        assert!(matches!(intent, Intent::Clarify { .. }));
    }

    #[test]
    fn parse_spawn_agent_missing_goal_field_clarifies() {
        let raw = r#"{"intent":"SpawnAgent"}"#;
        let intent = parse_reply(raw);
        assert!(matches!(intent, Intent::Clarify { .. }));
    }

    #[test]
    fn parse_clarify_missing_question_uses_default() {
        let raw = r#"{"intent":"Clarify"}"#;
        let intent = parse_reply(raw);
        assert!(matches!(intent, Intent::Clarify { .. }));
        // Should still be a valid Clarify, not a crash.
        assert!(intent.question().is_some());
    }

    // -------------------------------------------------------------------------
    // translate() — acceptance criteria from issue #55
    // -------------------------------------------------------------------------

    /// "build the project" → RunCommand("cargo build") for a Rust project.
    #[test]
    fn build_the_project_runs_cargo_build() {
        let ctx = rust_ctx();
        let backend = MockLlmBackend::new(r#"{"intent":"RunCommand","cmd":"cargo build"}"#);
        let intent = translate("build the project", &ctx, &backend).unwrap();
        assert_eq!(intent, Intent::run_command("cargo build"));
    }

    /// "what changed today" → SearchHistory.
    #[test]
    fn what_changed_today_searches_history() {
        let ctx = rust_ctx();
        let backend =
            MockLlmBackend::new(r#"{"intent":"SearchHistory","query":"recent git commits today"}"#);
        let intent = translate("what changed today", &ctx, &backend).unwrap();
        assert!(matches!(intent, Intent::SearchHistory { .. }));
        assert!(
            intent.query().unwrap().contains("git")
                || intent.query().unwrap().contains("commit")
                || intent.query().unwrap().contains("recent")
        );
    }

    /// Ambiguous input returns Clarify.
    #[test]
    fn ambiguous_input_clarifies() {
        let ctx = rust_ctx();
        let backend = MockLlmBackend::new(
            r#"{"intent":"Clarify","question":"What exactly do you want to do?"}"#,
        );
        let intent = translate("xyzzy frobnicate", &ctx, &backend).unwrap();
        assert!(matches!(intent, Intent::Clarify { .. }));
    }

    /// Empty input short-circuits before hitting the backend.
    #[test]
    fn empty_input_returns_clarify_without_calling_backend() {
        let ctx = rust_ctx();
        // Backend will panic if called — proves we short-circuit.
        struct PanicBackend;
        impl LlmBackend for PanicBackend {
            fn name(&self) -> &'static str {
                "panic"
            }
            fn complete(&self, _: &str, _: &str) -> Result<String, TranslateError> {
                panic!("backend should not be called for empty input");
            }
        }
        let intent = translate("", &ctx, &PanicBackend).unwrap();
        assert!(matches!(intent, Intent::Clarify { .. }));
    }

    /// Whitespace-only input is treated the same as empty.
    #[test]
    fn whitespace_only_input_clarifies() {
        let ctx = rust_ctx();
        struct PanicBackend;
        impl LlmBackend for PanicBackend {
            fn name(&self) -> &'static str {
                "panic"
            }
            fn complete(&self, _: &str, _: &str) -> Result<String, TranslateError> {
                panic!("backend should not be called for whitespace input");
            }
        }
        let intent = translate("   \t\n  ", &ctx, &PanicBackend).unwrap();
        assert!(matches!(intent, Intent::Clarify { .. }));
    }

    /// Backend returning empty string maps to Clarify (not a hard error).
    #[test]
    fn empty_backend_reply_maps_to_clarify() {
        let ctx = rust_ctx();
        let backend = MockLlmBackend::new("");
        let intent = translate("build", &ctx, &backend).unwrap();
        assert!(matches!(intent, Intent::Clarify { .. }));
    }

    /// SpawnAgent: open-ended fix task.
    #[test]
    fn fix_error_spawns_agent() {
        let ctx = rust_ctx();
        let backend = MockLlmBackend::new(r#"{"intent":"SpawnAgent","goal":"fix the last error"}"#);
        let intent = translate("fix the error", &ctx, &backend).unwrap();
        assert_eq!(intent, Intent::spawn_agent("fix the last error"));
    }

    /// RunCommand for Node project: "build" → "pnpm build".
    #[test]
    fn build_maps_to_node_command() {
        let ctx = node_ctx();
        let backend = MockLlmBackend::new(r#"{"intent":"RunCommand","cmd":"pnpm build"}"#);
        let intent = translate("build", &ctx, &backend).unwrap();
        assert_eq!(intent, Intent::run_command("pnpm build"));
    }

    // -------------------------------------------------------------------------
    // Intent field accessors
    // -------------------------------------------------------------------------

    #[test]
    fn run_command_accessors() {
        let i = Intent::run_command("git status");
        assert_eq!(i.cmd(), Some("git status"));
        assert!(i.query().is_none());
        assert!(i.goal().is_none());
        assert!(i.question().is_none());
        assert_eq!(i.variant_name(), "RunCommand");
    }

    #[test]
    fn search_history_accessors() {
        let i = Intent::search_history("recent commits");
        assert!(i.cmd().is_none());
        assert_eq!(i.query(), Some("recent commits"));
        assert!(i.goal().is_none());
        assert!(i.question().is_none());
        assert_eq!(i.variant_name(), "SearchHistory");
    }

    #[test]
    fn spawn_agent_accessors() {
        let i = Intent::spawn_agent("explain the error");
        assert!(i.cmd().is_none());
        assert!(i.query().is_none());
        assert_eq!(i.goal(), Some("explain the error"));
        assert!(i.question().is_none());
        assert_eq!(i.variant_name(), "SpawnAgent");
    }

    #[test]
    fn clarify_accessors() {
        let i = Intent::clarify("what do you mean?");
        assert!(i.cmd().is_none());
        assert!(i.query().is_none());
        assert!(i.goal().is_none());
        assert_eq!(i.question(), Some("what do you mean?"));
        assert_eq!(i.variant_name(), "Clarify");
    }

    // -------------------------------------------------------------------------
    // ClaudeLlmBackend — configuration
    // -------------------------------------------------------------------------

    #[test]
    fn claude_backend_name_is_claude() {
        // We can only verify name() here — from_env() needs the actual key.
        // Simulate by constructing directly (private field test via env path).
        // If ANTHROPIC_API_KEY is absent, verify the error shape.
        if std::env::var("ANTHROPIC_API_KEY").is_err() {
            let err = ClaudeLlmBackend::from_env().unwrap_err();
            assert!(matches!(err, TranslateError::NotConfigured(_)));
        } else {
            let b = ClaudeLlmBackend::from_env().unwrap();
            assert_eq!(b.name(), "claude");
            assert_eq!(b.model(), CLAUDE_DEFAULT_MODEL);
        }
    }

    #[test]
    fn claude_backend_with_model_overrides_default() {
        // Build with a fake key — we won't call complete().
        // Bypass from_env() by temporarily setting the var to any non-empty value.
        // We can only run this if we can set env vars (or if the key is already set).
        // Use a workaround: test the with_model chain on a config derived from a
        // known-present key, or skip gracefully.
        let key = std::env::var("ANTHROPIC_API_KEY").unwrap_or_else(|_| "sk-test".to_owned());
        let b = ClaudeLlmBackend {
            api_key: key,
            model: "claude-3-5-haiku-20241022".to_owned(),
        };
        assert_eq!(b.model(), "claude-3-5-haiku-20241022");
        assert_eq!(b.name(), "claude");
    }

    // -------------------------------------------------------------------------
    // TranslateError — propagation on backend failure
    // -------------------------------------------------------------------------

    #[test]
    fn transport_error_propagates() {
        struct ErrBackend;
        impl LlmBackend for ErrBackend {
            fn name(&self) -> &'static str {
                "err"
            }
            fn complete(&self, _: &str, _: &str) -> Result<String, TranslateError> {
                Err(TranslateError::Transport("connection refused".into()))
            }
        }
        let ctx = rust_ctx();
        let err = translate("build", &ctx, &ErrBackend).unwrap_err();
        assert!(matches!(err, TranslateError::Transport(_)));
    }

    #[test]
    fn not_configured_error_propagates() {
        struct ErrBackend;
        impl LlmBackend for ErrBackend {
            fn name(&self) -> &'static str {
                "err"
            }
            fn complete(&self, _: &str, _: &str) -> Result<String, TranslateError> {
                Err(TranslateError::NotConfigured("no key".into()))
            }
        }
        let ctx = rust_ctx();
        let err = translate("build", &ctx, &ErrBackend).unwrap_err();
        assert!(matches!(err, TranslateError::NotConfigured(_)));
    }

    // -------------------------------------------------------------------------
    // System prompt content — smoke tests
    // -------------------------------------------------------------------------

    #[test]
    fn system_prompt_contains_project_name() {
        let ctx = rust_ctx();
        let prompt = build_system_prompt(&ctx);
        assert!(prompt.contains("my-crate"));
    }

    #[test]
    fn system_prompt_contains_build_command() {
        let ctx = rust_ctx();
        let prompt = build_system_prompt(&ctx);
        assert!(prompt.contains("cargo build"));
    }

    #[test]
    fn system_prompt_contains_few_shot_examples() {
        let ctx = rust_ctx();
        let prompt = build_system_prompt(&ctx);
        assert!(prompt.contains("RunCommand"));
        assert!(prompt.contains("SearchHistory"));
        assert!(prompt.contains("SpawnAgent"));
        assert!(prompt.contains("Clarify"));
    }

    /// Live integration test — only runs when ANTHROPIC_API_KEY is present.
    #[test]
    #[ignore]
    fn live_translate_build_the_project() {
        let ctx = rust_ctx();
        let backend = match ClaudeLlmBackend::from_env() {
            Ok(b) => b,
            Err(e) => {
                eprintln!("skipping live test: {e}");
                return;
            }
        };
        let intent = translate("build the project", &ctx, &backend).unwrap();
        println!("live intent: {intent:?}");
        // Must be RunCommand for a Rust context.
        assert!(
            matches!(intent, Intent::RunCommand { .. }),
            "expected RunCommand, got {intent:?}"
        );
        assert!(
            intent.cmd().map(|c| c.contains("cargo")).unwrap_or(false),
            "expected cargo command, got {:?}",
            intent.cmd()
        );
    }
}
