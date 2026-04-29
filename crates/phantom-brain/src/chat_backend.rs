//! Unified `ChatBackend` trait and concrete provider implementations.
//!
//! This module defines [`ChatBackend`], the single interface through which the
//! brain dispatches chat completions to any LLM provider — cloud or local.
//! All providers (Claude, Ollama, OpenAI-compatible) implement the same trait,
//! so routing, fallback, and mocking all operate on `dyn ChatBackend`.
//!
//! # Providers
//!
//! | Struct              | Provider          | Cloud? | Requires             |
//! |---------------------|-------------------|--------|----------------------|
//! | [`ClaudeBackend`]   | Anthropic Claude  | yes    | `ANTHROPIC_API_KEY`  |
//! | [`OllamaBackend`]   | Ollama (local)    | no     | Ollama running       |
//! | [`OpenAiBackend`]   | OpenAI-compatible | yes    | `OPENAI_API_KEY`     |
//!
//! # Fallback routing
//!
//! [`RoutingCatalog`] wraps a primary and optional secondary backend.
//! [`RoutingCatalog::route`] returns a reference to whichever backend is
//! available right now — primary first, secondary as fallback.
//!
//! # Issue
//!
//! Implements GitHub issue #319.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Message / ChatOptions / ChatResponse
// ---------------------------------------------------------------------------

/// A single message in a chat conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    /// Either `"user"` or `"assistant"`.
    pub role: String,
    /// The message text.
    pub content: String,
}

impl Message {
    /// Construct a user message.
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: content.into(),
        }
    }

    /// Construct an assistant message.
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".into(),
            content: content.into(),
        }
    }
}

/// Options that tune a single chat completion request.
#[derive(Debug, Clone)]
pub struct ChatOptions {
    /// Maximum tokens to generate.
    pub max_tokens: u32,
    /// Optional model override. `None` means use the backend's default.
    pub model: Option<String>,
    /// Sampling temperature (0.0 = deterministic).
    pub temperature: f32,
}

impl Default for ChatOptions {
    fn default() -> Self {
        Self {
            max_tokens: 1024,
            model: None,
            temperature: 0.3,
        }
    }
}

/// The response from a [`ChatBackend::chat`] call.
#[derive(Debug, Clone)]
pub struct ChatResponse {
    /// The generated text.
    pub text: String,
    /// Round-trip latency in milliseconds.
    pub latency_ms: f32,
    /// Which provider produced this response.
    pub provider: String,
}

// ---------------------------------------------------------------------------
// ChatBackend trait
// ---------------------------------------------------------------------------

/// A synchronous chat interface that any LLM backend can implement.
///
/// All methods are synchronous (blocking). The brain runs on its own dedicated
/// thread, so blocking is safe and avoids async complexity.
///
/// # Thread safety
///
/// Implementations must be [`Send`] + [`Sync`] so they can be held behind
/// an `Arc<dyn ChatBackend>` and shared across threads.
pub trait ChatBackend: Send + Sync {
    /// Send `messages` to the model and return a [`ChatResponse`].
    ///
    /// Returns `Err` on network failure, auth error, or rate limit.
    fn chat(&self, messages: &[Message], opts: &ChatOptions) -> Result<ChatResponse, String>;

    /// Returns `true` if this backend appears to be reachable without making a
    /// network call.
    ///
    /// For cloud providers this is an env-var check (`ANTHROPIC_API_KEY` is
    /// set). For local providers it is a lightweight connectivity probe or a
    /// cached liveness flag. Implementations must not block for more than a
    /// few milliseconds.
    fn is_available(&self) -> bool;

    /// Short, stable name for this provider (e.g. `"claude"`, `"ollama"`).
    fn provider_name(&self) -> &'static str;

    /// Returns `true` for cloud providers (Claude, OpenAI) and `false` for
    /// local providers (Ollama, llama.cpp).
    ///
    /// The routing layer uses this to prefer local backends when privacy
    /// matters, or cloud backends when quality is critical.
    fn is_cloud_provider(&self) -> bool;
}

// ---------------------------------------------------------------------------
// ClaudeBackend
// ---------------------------------------------------------------------------

/// [`ChatBackend`] implementation that calls the Anthropic Messages API.
///
/// Availability is determined by checking whether the `ANTHROPIC_API_KEY`
/// environment variable is set — no network call is made during the check.
pub struct ClaudeBackend {
    model: String,
}

impl ClaudeBackend {
    /// Create a backend using the given Claude model.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
        }
    }

    /// Create a backend using `claude-sonnet-4-20250514`.
    pub fn default_model() -> Self {
        Self::new("claude-sonnet-4-20250514")
    }
}

impl ChatBackend for ClaudeBackend {
    fn chat(&self, messages: &[Message], opts: &ChatOptions) -> Result<ChatResponse, String> {
        let model = opts.model.as_deref().unwrap_or(&self.model);
        // Build a single user prompt by joining all user messages.
        // The brain's `claude::generate` only handles single-turn prompts for
        // now — multi-turn is handled by `phantom_agents::api::send_message`.
        let prompt = messages
            .iter()
            .filter(|m| m.role == "user")
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");

        let (text, latency_ms) = crate::claude::generate(model, &prompt, opts.max_tokens)?;

        Ok(ChatResponse {
            text,
            latency_ms,
            provider: "claude".into(),
        })
    }

    fn is_available(&self) -> bool {
        crate::claude::is_available()
    }

    fn provider_name(&self) -> &'static str {
        "claude"
    }

    fn is_cloud_provider(&self) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// OllamaBackend
// ---------------------------------------------------------------------------

/// [`ChatBackend`] implementation that calls a local Ollama instance.
///
/// Availability is checked by pinging Ollama's `/api/tags` endpoint (the same
/// call used by [`crate::ollama::is_available`]). The first call may take up
/// to 2 seconds; subsequent calls reuse cached state if within the TTL.
pub struct OllamaBackend {
    model: String,
}

impl OllamaBackend {
    /// Create a backend that uses the given Ollama model.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
        }
    }

    /// Create a backend using `phi3.5:latest`.
    pub fn default_model() -> Self {
        Self::new("phi3.5:latest")
    }
}

impl ChatBackend for OllamaBackend {
    fn chat(&self, messages: &[Message], opts: &ChatOptions) -> Result<ChatResponse, String> {
        let model = opts.model.as_deref().unwrap_or(&self.model);
        let prompt = messages
            .iter()
            .filter(|m| m.role == "user")
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");

        let (text, latency_ms) = crate::ollama::generate(model, &prompt, opts.max_tokens)?;

        Ok(ChatResponse {
            text,
            latency_ms,
            provider: "ollama".into(),
        })
    }

    fn is_available(&self) -> bool {
        crate::ollama::is_available()
    }

    fn provider_name(&self) -> &'static str {
        "ollama"
    }

    fn is_cloud_provider(&self) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// OpenAiBackend
// ---------------------------------------------------------------------------

/// [`ChatBackend`] stub for OpenAI-compatible endpoints.
///
/// Availability is determined by checking the `OPENAI_API_KEY` environment
/// variable. The actual HTTP call hits whatever `base_url` is configured.
pub struct OpenAiBackend {
    model: String,
    base_url: String,
}

impl OpenAiBackend {
    /// Create a backend targeting an OpenAI-compatible endpoint.
    pub fn new(model: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            base_url: base_url.into(),
        }
    }

    /// Create a backend using the canonical OpenAI API.
    pub fn openai_default() -> Self {
        Self::new("gpt-4o", "https://api.openai.com/v1")
    }
}

/// Response body from an OpenAI-compatible `/v1/chat/completions` endpoint.
#[derive(Deserialize)]
struct OpenAiResponse {
    choices: Vec<OpenAiChoice>,
}

#[derive(Deserialize)]
struct OpenAiChoice {
    message: OpenAiMessage,
}

#[derive(Deserialize)]
struct OpenAiMessage {
    content: String,
}

/// Request body for an OpenAI-compatible `/v1/chat/completions` endpoint.
#[derive(Serialize)]
struct OpenAiRequest<'a> {
    model: &'a str,
    messages: Vec<OpenAiRequestMessage<'a>>,
    max_tokens: u32,
    temperature: f32,
}

#[derive(Serialize)]
struct OpenAiRequestMessage<'a> {
    role: &'a str,
    content: &'a str,
}

impl ChatBackend for OpenAiBackend {
    fn chat(&self, messages: &[Message], opts: &ChatOptions) -> Result<ChatResponse, String> {
        let api_key =
            std::env::var("OPENAI_API_KEY").map_err(|_| "OPENAI_API_KEY not set".to_string())?;

        let model = opts.model.as_deref().unwrap_or(&self.model);
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));

        let start = std::time::Instant::now();

        let req_messages: Vec<OpenAiRequestMessage<'_>> = messages
            .iter()
            .map(|m| OpenAiRequestMessage {
                role: &m.role,
                content: &m.content,
            })
            .collect();

        let body = OpenAiRequest {
            model,
            messages: req_messages,
            max_tokens: opts.max_tokens,
            temperature: opts.temperature,
        };

        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(std::time::Duration::from_secs(30)))
            .build()
            .new_agent();

        let resp = agent
            .post(&url)
            .header("Authorization", &format!("Bearer {api_key}"))
            .header("content-type", "application/json")
            .send_json(&body)
            .map_err(|e| format!("openai request failed: {e}"))?;

        let latency_ms = start.elapsed().as_secs_f32() * 1000.0;

        if resp.status() != 200 {
            return Err(format!("openai returned status {}", resp.status()));
        }

        let openai_resp: OpenAiResponse = resp
            .into_body()
            .read_json()
            .map_err(|e| format!("openai response parse failed: {e}"))?;

        let text = openai_resp
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .unwrap_or_default();

        Ok(ChatResponse {
            text,
            latency_ms,
            provider: "openai".into(),
        })
    }

    fn is_available(&self) -> bool {
        std::env::var("OPENAI_API_KEY").is_ok()
    }

    fn provider_name(&self) -> &'static str {
        "openai"
    }

    fn is_cloud_provider(&self) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// RoutingCatalog — primary + fallback
// ---------------------------------------------------------------------------

/// A two-level fallback catalog of [`ChatBackend`]s.
///
/// [`RoutingCatalog::route`] returns a reference to the best available
/// backend without making any network calls:
///
/// 1. If `primary.is_available()` returns `true`, return primary.
/// 2. If `fallback` is set and `fallback.is_available()` returns `true`,
///    log a warning and return fallback.
/// 3. Otherwise return primary anyway — the caller will receive an `Err`
///    from the subsequent `chat()` call and can handle it.
pub struct RoutingCatalog {
    primary: Box<dyn ChatBackend>,
    fallback: Option<Box<dyn ChatBackend>>,
}

impl RoutingCatalog {
    /// Create a catalog with only a primary backend.
    pub fn primary_only(primary: Box<dyn ChatBackend>) -> Self {
        Self {
            primary,
            fallback: None,
        }
    }

    /// Create a catalog with a primary and a fallback backend.
    pub fn with_fallback(primary: Box<dyn ChatBackend>, fallback: Box<dyn ChatBackend>) -> Self {
        Self {
            primary,
            fallback: Some(fallback),
        }
    }

    /// Create the default production catalog: Claude primary, Ollama fallback.
    ///
    /// If neither is available the caller gets an `Err` from `chat()`.
    pub fn default_production() -> Self {
        Self::with_fallback(
            Box::new(ClaudeBackend::default_model()),
            Box::new(OllamaBackend::default_model()),
        )
    }

    /// Return a reference to the best available backend.
    ///
    /// This method never blocks for more than a few milliseconds.
    pub fn route(&self) -> &dyn ChatBackend {
        if self.primary.is_available() {
            return self.primary.as_ref();
        }
        if let Some(fb) = &self.fallback {
            if fb.is_available() {
                log::warn!(
                    "primary backend '{}' unavailable, using fallback '{}'",
                    self.primary.provider_name(),
                    fb.provider_name()
                );
                return fb.as_ref();
            }
        }
        // Neither is available — return primary so the caller gets a
        // descriptive error from chat() rather than a silent failure.
        self.primary.as_ref()
    }

    /// The primary backend.
    pub fn primary(&self) -> &dyn ChatBackend {
        self.primary.as_ref()
    }

    /// The fallback backend, if any.
    pub fn fallback(&self) -> Option<&dyn ChatBackend> {
        self.fallback.as_deref()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Mock backends
    // -----------------------------------------------------------------------

    struct AlwaysAvailableBackend {
        name: &'static str,
        cloud: bool,
    }

    impl ChatBackend for AlwaysAvailableBackend {
        fn chat(&self, _messages: &[Message], _opts: &ChatOptions) -> Result<ChatResponse, String> {
            Ok(ChatResponse {
                text: format!("response from {}", self.name),
                latency_ms: 1.0,
                provider: self.name.into(),
            })
        }

        fn is_available(&self) -> bool {
            true
        }

        fn provider_name(&self) -> &'static str {
            self.name
        }

        fn is_cloud_provider(&self) -> bool {
            self.cloud
        }
    }

    struct NeverAvailableBackend {
        name: &'static str,
    }

    impl ChatBackend for NeverAvailableBackend {
        fn chat(&self, _messages: &[Message], _opts: &ChatOptions) -> Result<ChatResponse, String> {
            Err(format!("{} is unavailable", self.name))
        }

        fn is_available(&self) -> bool {
            false
        }

        fn provider_name(&self) -> &'static str {
            self.name
        }

        fn is_cloud_provider(&self) -> bool {
            true
        }
    }

    // -----------------------------------------------------------------------
    // Message helpers
    // -----------------------------------------------------------------------

    #[test]
    fn message_user_and_assistant_constructors() {
        let u = Message::user("hello");
        assert_eq!(u.role, "user");
        assert_eq!(u.content, "hello");

        let a = Message::assistant("hi");
        assert_eq!(a.role, "assistant");
        assert_eq!(a.content, "hi");
    }

    // -----------------------------------------------------------------------
    // ChatOptions defaults
    // -----------------------------------------------------------------------

    #[test]
    fn chat_options_default_values() {
        let opts = ChatOptions::default();
        assert_eq!(opts.max_tokens, 1024);
        assert!(opts.model.is_none());
        assert!((opts.temperature - 0.3).abs() < f32::EPSILON);
    }

    // -----------------------------------------------------------------------
    // ClaudeBackend
    // -----------------------------------------------------------------------

    #[test]
    fn claude_backend_provider_name() {
        let b = ClaudeBackend::default_model();
        assert_eq!(b.provider_name(), "claude");
    }

    #[test]
    fn claude_backend_is_cloud() {
        let b = ClaudeBackend::default_model();
        assert!(b.is_cloud_provider());
    }

    #[test]
    fn claude_backend_availability_reflects_env_var() {
        let b = ClaudeBackend::default_model();
        let key_set = std::env::var("ANTHROPIC_API_KEY").is_ok();
        assert_eq!(b.is_available(), key_set);
    }

    // -----------------------------------------------------------------------
    // OllamaBackend
    // -----------------------------------------------------------------------

    #[test]
    fn ollama_backend_provider_name() {
        let b = OllamaBackend::default_model();
        assert_eq!(b.provider_name(), "ollama");
    }

    #[test]
    fn ollama_backend_is_not_cloud() {
        let b = OllamaBackend::default_model();
        assert!(!b.is_cloud_provider());
    }

    #[test]
    fn ollama_backend_is_available_does_not_panic() {
        // May return true or false depending on whether Ollama is running.
        let _ = OllamaBackend::default_model().is_available();
    }

    // -----------------------------------------------------------------------
    // OpenAiBackend
    // -----------------------------------------------------------------------

    #[test]
    fn openai_backend_provider_name() {
        let b = OpenAiBackend::openai_default();
        assert_eq!(b.provider_name(), "openai");
    }

    #[test]
    fn openai_backend_is_cloud() {
        let b = OpenAiBackend::openai_default();
        assert!(b.is_cloud_provider());
    }

    #[test]
    fn openai_backend_availability_reflects_env_var() {
        let b = OpenAiBackend::openai_default();
        let key_set = std::env::var("OPENAI_API_KEY").is_ok();
        assert_eq!(b.is_available(), key_set);
    }

    // -----------------------------------------------------------------------
    // RoutingCatalog — Issue #319 acceptance test
    // -----------------------------------------------------------------------

    /// When the primary backend is unavailable, `route()` must return the
    /// fallback backend without panicking.
    #[test]
    fn provider_catalog_falls_back_when_primary_unavailable() {
        let catalog = RoutingCatalog::with_fallback(
            Box::new(NeverAvailableBackend {
                name: "primary-down",
            }),
            Box::new(AlwaysAvailableBackend {
                name: "fallback-up",
                cloud: false,
            }),
        );

        let backend = catalog.route();
        assert_eq!(
            backend.provider_name(),
            "fallback-up",
            "route() must return the fallback when the primary is unavailable"
        );

        // The fallback must actually respond.
        let resp = backend
            .chat(&[Message::user("hello")], &ChatOptions::default())
            .expect("fallback chat must succeed");
        assert!(resp.text.contains("fallback-up"));
    }

    /// When the primary is available, `route()` must return it regardless of
    /// whether the fallback is also available.
    #[test]
    fn provider_catalog_uses_primary_when_available() {
        let catalog = RoutingCatalog::with_fallback(
            Box::new(AlwaysAvailableBackend {
                name: "primary-up",
                cloud: true,
            }),
            Box::new(AlwaysAvailableBackend {
                name: "fallback-up",
                cloud: false,
            }),
        );

        let backend = catalog.route();
        assert_eq!(
            backend.provider_name(),
            "primary-up",
            "route() must return the primary when it is available"
        );
    }

    /// When both primary and fallback are unavailable, `route()` returns the
    /// primary so the caller gets a descriptive error from `chat()`.
    #[test]
    fn provider_catalog_returns_primary_when_both_unavailable() {
        let catalog = RoutingCatalog::with_fallback(
            Box::new(NeverAvailableBackend {
                name: "primary-down",
            }),
            Box::new(NeverAvailableBackend {
                name: "fallback-down",
            }),
        );

        // route() must not panic — it returns primary.
        let backend = catalog.route();
        assert_eq!(backend.provider_name(), "primary-down");

        // chat() must return Err, not panic.
        let result = backend.chat(&[Message::user("hello")], &ChatOptions::default());
        assert!(result.is_err(), "unavailable backend must return Err");
    }

    /// `primary_only` catalog returns primary when asked.
    #[test]
    fn primary_only_catalog_routes_to_primary() {
        let catalog = RoutingCatalog::primary_only(Box::new(AlwaysAvailableBackend {
            name: "solo",
            cloud: true,
        }));

        assert_eq!(catalog.route().provider_name(), "solo");
        assert!(catalog.fallback().is_none());
    }
}
