//! Ollama HTTP client for the brain's local model dispatch.
//!
//! Talks to a running Ollama instance at `localhost:11434`. The brain thread
//! calls these functions synchronously (blocking) — this is intentional since
//! the brain runs on its own dedicated thread.
//!
//! # `OllamaBackend`
//!
//! The [`OllamaBackend`] struct implements [`crate::goal::ChatBackend`] so it
//! can be used wherever a `ChatBackend` is required (e.g., goal decomposition).
//! It uses Ollama's `/api/chat` endpoint for multi-turn conversation history.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

const DEFAULT_URL: &str = "http://localhost:11434";
const GENERATE_TIMEOUT_SECS: u64 = 30;
const CHAT_TIMEOUT_SECS: u64 = 60;

// ---------------------------------------------------------------------------
// OllamaBackend — implements ChatBackend (multi-turn /api/chat)
// ---------------------------------------------------------------------------

/// A [`crate::goal::ChatBackend`] that dispatches to Ollama's `/api/chat`
/// endpoint for single-turn and multi-turn conversations.
///
/// # Construction
///
/// ```rust
/// use phantom_brain::ollama::OllamaBackend;
///
/// // Default: phi3.5:latest @ http://localhost:11434
/// let backend = OllamaBackend::default_model();
///
/// // Custom model:
/// let backend = OllamaBackend::new("llama3:latest", "http://localhost:11434");
/// ```
pub struct OllamaBackend {
    /// Ollama base URL (default: `http://localhost:11434`).
    base_url: String,
    /// Ollama model name (e.g. `"phi3.5:latest"`).
    model: String,
}

impl OllamaBackend {
    /// Create a backend for `model` at `base_url`.
    pub fn new(model: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            model: model.into(),
        }
    }

    /// Create a backend using `phi3.5:latest` at the default local URL.
    pub fn default_model() -> Self {
        Self::new("phi3.5:latest", DEFAULT_URL)
    }

    /// Returns `true` when Ollama is running and reachable.
    ///
    /// Pings `/api/tags` which responds quickly without loading a model.
    pub fn is_available(&self) -> bool {
        let url = format!("{}/api/tags", self.base_url);
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(std::time::Duration::from_secs(2)))
            .build()
            .new_agent();
        match agent.get(&url).call() {
            Ok(resp) => resp.status() == 200,
            Err(_) => false,
        }
    }

    /// Return the model name in use.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Return the base URL in use.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }
}

// Wire types for /api/chat

/// A single message in the Ollama `/api/chat` format.
#[derive(Serialize, Deserialize, Clone)]
struct ChatMessage {
    role: String,
    content: String,
}

/// Request body for Ollama's `/api/chat` endpoint.
#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage>,
    stream: bool,
    options: ChatRequestOptions,
}

#[derive(Serialize)]
struct ChatRequestOptions {
    num_predict: u32,
    temperature: f32,
}

/// Response from Ollama's `/api/chat` (non-streaming).
#[derive(Deserialize)]
struct ChatResponse {
    message: ChatMessage,
    #[serde(default)]
    done: bool,
}

impl crate::goal::ChatBackend for OllamaBackend {
    fn chat(&self, prompt: &str) -> Result<String, String> {
        let url = format!("{}/api/chat", self.base_url);

        let request = ChatRequest {
            model: &self.model,
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: prompt.to_string(),
            }],
            stream: false,
            options: ChatRequestOptions {
                num_predict: 512,
                temperature: 0.3,
            },
        };

        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(std::time::Duration::from_secs(CHAT_TIMEOUT_SECS)))
            .build()
            .new_agent();

        let start = std::time::Instant::now();

        let resp = agent
            .post(&url)
            .send_json(&request)
            .map_err(|e| format!("ollama /api/chat request failed: {e}"))?;

        let latency_ms = start.elapsed().as_secs_f32() * 1000.0;

        if resp.status() != 200 {
            return Err(format!(
                "ollama /api/chat returned status {}",
                resp.status()
            ));
        }

        let chat_resp: ChatResponse = resp
            .into_body()
            .read_json()
            .map_err(|e| format!("ollama /api/chat response parse failed: {e}"))?;

        let text = chat_resp.message.content.trim().to_string();

        log::debug!(
            "ollama chat: model={}, latency={latency_ms:.0}ms, done={}, len={}",
            self.model,
            chat_resp.done,
            text.len()
        );

        Ok(text)
    }
}

/// Request body for Ollama's `/api/generate` endpoint.
#[derive(Serialize)]
struct GenerateRequest<'a> {
    model: &'a str,
    prompt: &'a str,
    stream: bool,
    options: GenerateOptions,
}

#[derive(Serialize)]
struct GenerateOptions {
    /// Max tokens to generate.
    num_predict: u32,
    /// Temperature (0.0 = deterministic, 1.0 = creative).
    temperature: f32,
}

/// Response from Ollama's `/api/generate` (non-streaming).
#[derive(Deserialize)]
struct GenerateResponse {
    response: String,
    #[serde(default)]
    done: bool,
    #[serde(default)]
    #[allow(dead_code)]
    total_duration: u64,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Check if Ollama is running and reachable.
///
/// Pings the `/api/tags` endpoint which returns quickly.
/// Returns `true` if the server responds with 200.
pub fn is_available() -> bool {
    let url = format!("{DEFAULT_URL}/api/tags");
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(2)))
        .build()
        .new_agent();
    match agent.get(&url).call() {
        Ok(resp) => resp.status() == 200,
        Err(_) => false,
    }
}

/// Generate a short text response from the local model.
///
/// `model` is the Ollama model name (e.g. "phi3.5:latest").
/// `prompt` is the full prompt text.
/// `max_tokens` caps the response length.
///
/// Returns `(response_text, latency_ms)` on success.
pub fn generate(model: &str, prompt: &str, max_tokens: u32) -> Result<(String, f32), String> {
    let url = format!("{DEFAULT_URL}/api/generate");
    let start = std::time::Instant::now();

    let body = GenerateRequest {
        model,
        prompt,
        stream: false,
        options: GenerateOptions {
            num_predict: max_tokens,
            temperature: 0.3,
        },
    };

    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(GENERATE_TIMEOUT_SECS)))
        .build()
        .new_agent();

    let resp = agent
        .post(&url)
        .send_json(&body)
        .map_err(|e| format!("ollama request failed: {e}"))?;

    let latency_ms = start.elapsed().as_secs_f32() * 1000.0;

    if resp.status() != 200 {
        return Err(format!("ollama returned status {}", resp.status()));
    }

    let gen_resp: GenerateResponse = resp
        .into_body()
        .read_json()
        .map_err(|e| format!("ollama response parse failed: {e}"))?;

    let text = gen_resp.response.trim().to_string();

    log::debug!(
        "ollama generate: model={model}, tokens≈{}, latency={latency_ms:.0}ms, done={}",
        text.split_whitespace().count(),
        gen_resp.done
    );

    Ok((text, latency_ms))
}

/// Build a prompt for error triage from a parsed command output.
///
/// The local model receives a concise prompt asking it to summarize the error
/// and suggest a fix in 1-2 sentences. Keeps token count low for fast inference.
pub fn build_error_triage_prompt(
    command: &str,
    errors: &[phantom_semantic::DetectedError],
    project_type: &str,
) -> String {
    let error_summary: String = errors
        .iter()
        .take(3) // cap at 3 errors to keep prompt short
        .map(|e| {
            let location = match (&e.file, e.line) {
                (Some(f), Some(l)) => format!(" at {f}:{l}"),
                _ => String::new(),
            };
            format!("- {}{location}", e.message)
        })
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "You are a terminal assistant for a {project_type} project.\n\
         The command `{command}` failed with these errors:\n\
         {error_summary}\n\n\
         In 1-2 sentences, explain what went wrong and suggest a fix. Be concise."
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_error_triage_prompt_formats_correctly() {
        let errors = vec![phantom_semantic::DetectedError {
            message: "mismatched types".into(),
            error_type: phantom_semantic::ErrorType::Compiler,
            file: Some("src/main.rs".into()),
            line: Some(42),
            column: Some(5),
            code: Some("E0308".into()),
            severity: phantom_semantic::Severity::Error,
            raw_line: String::new(),
            suggestion: None,
        }];

        let prompt = build_error_triage_prompt("cargo build", &errors, "Rust");
        assert!(prompt.contains("Rust project"));
        assert!(prompt.contains("cargo build"));
        assert!(prompt.contains("mismatched types"));
        assert!(prompt.contains("src/main.rs:42"));
        assert!(prompt.contains("1-2 sentences"));
    }

    #[test]
    fn build_error_triage_prompt_caps_at_3_errors() {
        let errors: Vec<phantom_semantic::DetectedError> = (0..10)
            .map(|i| phantom_semantic::DetectedError {
                message: format!("error {i}"),
                error_type: phantom_semantic::ErrorType::Compiler,
                file: None,
                line: None,
                column: None,
                code: None,
                severity: phantom_semantic::Severity::Error,
                raw_line: String::new(),
                suggestion: None,
            })
            .collect();

        let prompt = build_error_triage_prompt("cargo build", &errors, "Rust");
        // Should only contain errors 0, 1, 2 (capped at 3)
        assert!(prompt.contains("error 0"));
        assert!(prompt.contains("error 2"));
        assert!(!prompt.contains("error 3"));
    }

    #[test]
    fn is_available_returns_false_when_no_server() {
        // No Ollama running in CI — should return false, not panic.
        // (If Ollama IS running locally, this will return true — both are fine.)
        let _ = is_available();
    }

    // ---------------------------------------------------------------------------
    // OllamaBackend tests
    // ---------------------------------------------------------------------------

    #[test]
    fn ollama_backend_default_model_fields() {
        let backend = OllamaBackend::default_model();
        assert_eq!(backend.model(), "phi3.5:latest");
        assert_eq!(backend.base_url(), "http://localhost:11434");
    }

    #[test]
    fn ollama_backend_new_stores_fields() {
        let backend = OllamaBackend::new("llama3:latest", "http://localhost:11434");
        assert_eq!(backend.model(), "llama3:latest");
        assert_eq!(backend.base_url(), "http://localhost:11434");
    }

    #[test]
    fn ollama_backend_is_available_does_not_panic() {
        // No Ollama running in CI — should return false, not panic.
        let backend = OllamaBackend::default_model();
        let _ = backend.is_available();
    }

    #[test]
    fn ollama_backend_chat_fails_gracefully_without_server() {
        use crate::goal::ChatBackend;

        // Point to a port that's almost certainly not listening.
        let backend = OllamaBackend::new("phi3.5:latest", "http://localhost:19999");
        let result = backend.chat("Hello, are you there?");
        assert!(
            result.is_err(),
            "chat must return Err when Ollama is not reachable"
        );
    }

    #[test]
    fn ollama_backend_implements_chat_backend_trait() {
        use crate::goal::ChatBackend;

        // Compile-time proof: OllamaBackend satisfies the ChatBackend bound.
        fn accepts_chat_backend<B: ChatBackend>(_: &B) {}
        let backend = OllamaBackend::default_model();
        accepts_chat_backend(&backend);
    }
}
