//! Ollama HTTP client for the brain's local model dispatch.
//!
//! Talks to a running Ollama instance at `localhost:11434`. The brain thread
//! calls these functions synchronously (blocking) — this is intentional since
//! the brain runs on its own dedicated thread.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

const DEFAULT_URL: &str = "http://localhost:11434";
const GENERATE_TIMEOUT_SECS: u64 = 30;

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
pub fn generate(
    model: &str,
    prompt: &str,
    max_tokens: u32,
) -> Result<(String, f32), String> {
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

    let resp = agent.post(&url)
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
        let errors = vec![
            phantom_semantic::DetectedError {
                message: "mismatched types".into(),
                error_type: phantom_semantic::ErrorType::Compiler,
                file: Some("src/main.rs".into()),
                line: Some(42),
                column: Some(5),
                code: Some("E0308".into()),
                severity: phantom_semantic::Severity::Error,
                raw_line: String::new(),
                suggestion: None,
            },
        ];

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
}
