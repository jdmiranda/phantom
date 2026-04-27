//! Claude API client for the brain's frontier model dispatch.
//!
//! Called synchronously from the brain thread when a task is classified as
//! Complex and either Ollama is unavailable or its response was low-quality.
//! Uses the Messages API via ureq (blocking HTTP).

use serde::{Deserialize, Serialize};

const DEFAULT_URL: &str = "https://api.anthropic.com/v1/messages";
const TIMEOUT_SECS: u64 = 30;

/// Request body for the Claude Messages API.
#[derive(Serialize)]
struct MessagesRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    messages: Vec<Message<'a>>,
}

#[derive(Serialize)]
struct Message<'a> {
    role: &'a str,
    content: &'a str,
}

/// Simplified response from the Claude Messages API.
#[derive(Deserialize)]
struct MessagesResponse {
    content: Vec<ContentBlock>,
}

#[derive(Deserialize)]
struct ContentBlock {
    text: Option<String>,
}

/// Check if the Claude API is reachable (API key is set).
pub fn is_available() -> bool {
    std::env::var("ANTHROPIC_API_KEY").is_ok()
}

/// Generate a response from Claude.
///
/// Returns `(response_text, latency_ms)` on success.
pub fn generate(
    model: &str,
    prompt: &str,
    max_tokens: u32,
) -> Result<(String, f32), String> {
    let api_key = std::env::var("ANTHROPIC_API_KEY")
        .map_err(|_| "ANTHROPIC_API_KEY not set".to_string())?;

    let start = std::time::Instant::now();

    let body = MessagesRequest {
        model,
        max_tokens,
        messages: vec![Message {
            role: "user",
            content: prompt,
        }],
    };

    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(TIMEOUT_SECS)))
        .build()
        .new_agent();

    let resp = agent
        .post(DEFAULT_URL)
        .header("x-api-key", &api_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .send_json(&body)
        .map_err(|e| format!("claude request failed: {e}"))?;

    let latency_ms = start.elapsed().as_secs_f32() * 1000.0;

    if resp.status() != 200 {
        return Err(format!("claude returned status {}", resp.status()));
    }

    let msg_resp: MessagesResponse = resp
        .into_body()
        .read_json()
        .map_err(|e| format!("claude response parse failed: {e}"))?;

    let text = msg_resp
        .content
        .into_iter()
        .filter_map(|b| b.text)
        .collect::<Vec<_>>()
        .join("");

    let text = text.trim().to_string();

    log::debug!(
        "claude generate: model={model}, latency={latency_ms:.0}ms, len={}",
        text.len()
    );

    Ok((text, latency_ms))
}

/// Build a prompt for Claude error analysis (deeper than Ollama's triage).
pub fn build_error_analysis_prompt(
    command: &str,
    errors: &[phantom_semantic::DetectedError],
    project_type: &str,
) -> String {
    let error_summary: String = errors
        .iter()
        .take(5)
        .map(|e| {
            let location = match (&e.file, e.line) {
                (Some(f), Some(l)) => format!(" at {f}:{l}"),
                _ => String::new(),
            };
            let suggestion = e
                .suggestion
                .as_deref()
                .map(|s| format!(" (hint: {s})"))
                .unwrap_or_default();
            format!("- [{:?}] {}{location}{suggestion}", e.error_type, e.message)
        })
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "You are an expert terminal assistant for a {project_type} project.\n\
         The command `{command}` failed with {} error(s):\n\
         {error_summary}\n\n\
         Analyze the root cause and provide a specific fix. \
         If multiple errors share a root cause, explain that. \
         Be concise — 2-3 sentences max.",
        errors.len()
    )
}

/// Run a tool-augmented investigation on the brain thread.
///
/// Blocks until done — the brain thread is dedicated and doesn't share
/// work with the render loop.
pub fn investigate(
    prompt: &str,
    working_dir: &str,
    max_rounds: u32,
) -> Result<String, String> {
    use phantom_agents::api::{ApiEvent, ClaudeConfig, send_message};
    use phantom_agents::agent::{Agent, AgentMessage, AgentTask};
    use phantom_agents::tools::{available_tools, execute_tool};

    let config = ClaudeConfig::from_env()
        .ok_or("ANTHROPIC_API_KEY not set")?;

    let task = AgentTask::FreeForm { prompt: prompt.to_string() };
    let mut agent = Agent::new(0, task);
    let sys = agent.system_prompt();
    agent.push_message(AgentMessage::System(sys));
    agent.push_message(AgentMessage::User(prompt.to_string()));

    let tools = available_tools();
    let mut tool_use_ids: Vec<String> = Vec::new();
    let mut final_text = String::new();

    for _round in 0..max_rounds {
        let mut handle = send_message(&config, &agent, &tools, &tool_use_ids);
        let mut assistant_text = String::new();
        let mut pending: Vec<(String, phantom_agents::tools::ToolCall)> = Vec::new();

        loop {
            match handle.try_recv() {
                Some(ApiEvent::TextDelta(text)) => assistant_text.push_str(&text),
                Some(ApiEvent::ToolUse { id, call }) => {
                    tool_use_ids.push(id.clone());
                    pending.push((id, call));
                }
                Some(ApiEvent::Done) => break,
                Some(ApiEvent::Error(e)) => return Err(e),
                None => std::thread::sleep(std::time::Duration::from_millis(50)),
            }
        }

        if !assistant_text.is_empty() {
            agent.push_message(AgentMessage::Assistant(assistant_text.clone()));
        }
        if pending.is_empty() {
            final_text = assistant_text;
            break;
        }
        for (_, call) in &pending {
            agent.push_message(AgentMessage::ToolCall(call.clone()));
        }
        for (_, call) in pending {
            let result = execute_tool(call.tool, &call.args, working_dir);
            agent.push_message(AgentMessage::ToolResult(result));
        }
    }

    if final_text.is_empty() {
        Ok("(investigation completed with tool use, no final text)".into())
    } else {
        Ok(final_text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_error_analysis_prompt_formats_correctly() {
        let errors = vec![phantom_semantic::DetectedError {
            message: "mismatched types".into(),
            error_type: phantom_semantic::ErrorType::Compiler,
            file: Some("src/main.rs".into()),
            line: Some(42),
            column: Some(5),
            code: Some("E0308".into()),
            severity: phantom_semantic::Severity::Error,
            raw_line: String::new(),
            suggestion: Some("try `.to_string()`".into()),
        }];

        let prompt = build_error_analysis_prompt("cargo build", &errors, "Rust");
        assert!(prompt.contains("Rust project"));
        assert!(prompt.contains("cargo build"));
        assert!(prompt.contains("mismatched types"));
        assert!(prompt.contains("src/main.rs:42"));
        assert!(prompt.contains("try `.to_string()`"));
    }

    #[test]
    fn build_error_analysis_prompt_caps_at_5_errors() {
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

        let prompt = build_error_analysis_prompt("cargo build", &errors, "Rust");
        assert!(prompt.contains("error 0"));
        assert!(prompt.contains("error 4"));
        assert!(!prompt.contains("error 5"));
    }

    #[test]
    fn is_available_does_not_panic() {
        let _ = is_available();
    }
}
