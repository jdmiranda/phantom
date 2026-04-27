//! Claude API integration.
//!
//! Sends agent conversations to the Anthropic Messages API on a background
//! thread and exposes a non-blocking polling handle for the synchronous main
//! loop. Uses the **blocking** `reqwest` client -- no async runtime required.

use std::sync::mpsc;

use serde_json::Value;

use crate::agent::{Agent, AgentMessage};
use crate::tools::{ToolCall, ToolDefinition, ToolType};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const API_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_MODEL: &str = "claude-sonnet-4-20250514";
const DEFAULT_MAX_TOKENS: u32 = 4096;

// ---------------------------------------------------------------------------
// ClaudeConfig
// ---------------------------------------------------------------------------

/// Configuration for Claude API access.
#[derive(Debug, Clone)]
pub struct ClaudeConfig {
    pub api_key: String,
    pub model: String,
    pub max_tokens: u32,
}

impl ClaudeConfig {
    /// Load from the `ANTHROPIC_API_KEY` environment variable.
    ///
    /// Returns `None` if the variable is unset or empty.
    pub fn from_env() -> Option<Self> {
        let api_key = std::env::var("ANTHROPIC_API_KEY").ok()?;
        if api_key.is_empty() {
            return None;
        }
        Some(Self {
            api_key,
            model: DEFAULT_MODEL.to_owned(),
            max_tokens: DEFAULT_MAX_TOKENS,
        })
    }

    /// Create a config with explicit values.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            model: DEFAULT_MODEL.to_owned(),
            max_tokens: DEFAULT_MAX_TOKENS,
        }
    }

    /// Override the model.
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    /// Override max tokens.
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }
}

// ---------------------------------------------------------------------------
// ApiEvent
// ---------------------------------------------------------------------------

/// An event from the API response, delivered over an mpsc channel.
#[derive(Debug, Clone)]
pub enum ApiEvent {
    /// Text response from the assistant.
    TextDelta(String),
    /// The assistant wants to call a tool.
    ToolUse {
        /// API-assigned ID for this tool use (needed for the tool_result).
        id: String,
        call: ToolCall,
    },
    /// The response is complete.
    Done,
    /// An error occurred.
    Error(String),
}

// ---------------------------------------------------------------------------
// ApiHandle
// ---------------------------------------------------------------------------

/// Handle for a background API request.
///
/// The main thread calls [`try_recv`](Self::try_recv) each frame to poll for
/// events without blocking.
pub struct ApiHandle {
    rx: mpsc::Receiver<ApiEvent>,
    done: bool,
}

impl ApiHandle {
    /// Non-blocking poll for the next event.
    ///
    /// Returns `None` when no event is available yet.
    pub fn try_recv(&mut self) -> Option<ApiEvent> {
        match self.rx.try_recv() {
            Ok(event) => {
                if matches!(&event, ApiEvent::Done | ApiEvent::Error(_)) {
                    self.done = true;
                }
                Some(event)
            }
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => {
                if !self.done {
                    self.done = true;
                    Some(ApiEvent::Error("background thread disconnected".into()))
                } else {
                    None
                }
            }
        }
    }

    /// Whether the request has completed (either successfully or with an error).
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Create a handle from a raw receiver. Used by test harnesses to inject
    /// synthetic API events without hitting the network.
    pub fn from_receiver(rx: mpsc::Receiver<ApiEvent>) -> Self {
        Self { rx, done: false }
    }
}

// ---------------------------------------------------------------------------
// Message format conversion
// ---------------------------------------------------------------------------

/// Convenience wrapper: convert messages with auto-generated placeholder IDs.
#[cfg(test)]
fn build_messages(messages: &[AgentMessage]) -> Vec<Value> {
    build_messages_with_ids(messages, &[])
}

/// Like [`build_messages`] but accepts an external list of tool-use IDs to
/// pair with `ToolCall` / `ToolResult` messages.
///
/// `tool_use_ids` maps positionally: the *n*-th tool call in `messages`
/// corresponds to `tool_use_ids[n]`. If the slice is too short, a generated
/// placeholder is used.
fn build_messages_with_ids(messages: &[AgentMessage], tool_use_ids: &[String]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut tool_idx: usize = 0;
    let mut result_idx: usize = 0;

    let mut i = 0;
    while i < messages.len() {
        match &messages[i] {
            AgentMessage::System(_) => {
                // Handled by the system field, skip.
                i += 1;
            }
            AgentMessage::User(text) => {
                out.push(serde_json::json!({
                    "role": "user",
                    "content": text,
                }));
                i += 1;
            }
            AgentMessage::Assistant(text) => {
                out.push(serde_json::json!({
                    "role": "assistant",
                    "content": text,
                }));
                i += 1;
            }
            AgentMessage::ToolCall(_) => {
                // Collect consecutive tool calls into a single assistant message.
                let mut content_blocks = Vec::new();
                while i < messages.len() {
                    if let AgentMessage::ToolCall(tc) = &messages[i] {
                        let id = tool_use_ids
                            .get(tool_idx)
                            .cloned()
                            .unwrap_or_else(|| format!("toolu_{tool_idx}"));
                        content_blocks.push(serde_json::json!({
                            "type": "tool_use",
                            "id": id,
                            "name": tc.tool.api_name(),
                            "input": tc.args,
                        }));
                        tool_idx += 1;
                        i += 1;
                    } else {
                        break;
                    }
                }
                out.push(serde_json::json!({
                    "role": "assistant",
                    "content": content_blocks,
                }));
            }
            AgentMessage::ToolResult(_) => {
                // Collect consecutive tool results into a single user message.
                let mut content_blocks = Vec::new();
                while i < messages.len() {
                    if let AgentMessage::ToolResult(tr) = &messages[i] {
                        let id = tool_use_ids
                            .get(result_idx)
                            .cloned()
                            .unwrap_or_else(|| format!("toolu_{result_idx}"));
                        let mut block = serde_json::json!({
                            "type": "tool_result",
                            "tool_use_id": id,
                            "content": tr.output,
                        });
                        if !tr.success {
                            block["is_error"] = serde_json::json!(true);
                        }
                        content_blocks.push(block);
                        result_idx += 1;
                        i += 1;
                    } else {
                        break;
                    }
                }
                out.push(serde_json::json!({
                    "role": "user",
                    "content": content_blocks,
                }));
            }
        }
    }

    out
}

/// Convert tool definitions into the `tools` array for the API.
fn build_tools(tools: &[ToolDefinition]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            serde_json::json!({
                "name": t.name,
                "description": t.description,
                "input_schema": t.parameters,
            })
        })
        .collect()
}

/// Build the complete request body.
fn build_request_body(
    config: &ClaudeConfig,
    agent: &Agent,
    tools: &[ToolDefinition],
    tool_use_ids: &[String],
) -> Value {
    let system = agent.system_prompt();
    let mut body = serde_json::json!({
        "model": config.model,
        "max_tokens": config.max_tokens,
        "system": system,
        "messages": build_messages_with_ids(&agent.messages, tool_use_ids),
    });

    let tool_defs = build_tools(tools);
    if !tool_defs.is_empty() {
        body["tools"] = Value::Array(tool_defs);
    }

    body
}

// ---------------------------------------------------------------------------
// Response parsing
// ---------------------------------------------------------------------------

/// Parse the API response JSON and send events over the channel.
fn parse_response(body: &Value, tx: &mpsc::Sender<ApiEvent>) {
    // Check for API-level error.
    if let Some(err) = body.get("error") {
        let msg = err["message"]
            .as_str()
            .unwrap_or("unknown API error");
        let _ = tx.send(ApiEvent::Error(msg.to_owned()));
        return;
    }

    // Parse content blocks.
    let Some(content) = body.get("content").and_then(|c| c.as_array()) else {
        let _ = tx.send(ApiEvent::Error("response missing content array".into()));
        return;
    };

    for block in content {
        match block.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                    let _ = tx.send(ApiEvent::TextDelta(text.to_owned()));
                }
            }
            Some("tool_use") => {
                let id = block
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned();
                let name = block
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let input = block
                    .get("input")
                    .cloned()
                    .unwrap_or(Value::Object(serde_json::Map::new()));

                if let Some(tool) = ToolType::from_api_name(name) {
                    let _ = tx.send(ApiEvent::ToolUse {
                        id,
                        call: ToolCall { tool, args: input },
                    });
                } else {
                    let _ = tx.send(ApiEvent::Error(format!(
                        "unknown tool in response: {name}"
                    )));
                }
            }
            _ => {
                // Unknown block type -- skip silently.
            }
        }
    }

    let _ = tx.send(ApiEvent::Done);
}

// ---------------------------------------------------------------------------
// send_message
// ---------------------------------------------------------------------------

/// Send the agent's conversation to the Claude API on a background thread.
///
/// `tool_use_ids` provides the API-assigned IDs for tool calls already in the
/// conversation history. Pass an empty slice if this is the first request.
///
/// Returns an [`ApiHandle`] that the main thread polls each frame via
/// [`ApiHandle::try_recv`]. The background thread uses `reqwest::blocking`
/// so no async runtime is needed.
pub fn send_message(
    config: &ClaudeConfig,
    agent: &Agent,
    tools: &[ToolDefinition],
    tool_use_ids: &[String],
) -> ApiHandle {
    let (tx, rx) = mpsc::channel();

    let request_body = build_request_body(config, agent, tools, tool_use_ids);
    let api_key = config.api_key.clone();

    std::thread::spawn(move || {
        // Use ureq instead of reqwest — reqwest::blocking has TLS issues
        // on macOS that cause "error sending request" failures. ureq works
        // reliably (the brain's claude.rs uses it successfully).
        let agent = ureq::Agent::new_with_config(
            ureq::config::Config::builder()
                .timeout_global(Some(std::time::Duration::from_secs(120)))
                .build()
        );

        let body_str = serde_json::to_string(&request_body).unwrap_or_default();

        let result = agent
            .post(API_URL)
            .header("x-api-key", &api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .send(body_str.as_bytes());

        match result {
            Ok(mut response) => {
                match response.body_mut().read_to_string() {
                    Ok(text) => {
                        match serde_json::from_str::<Value>(&text) {
                            Ok(json) => parse_response(&json, &tx),
                            Err(e) => {
                                let _ = tx.send(ApiEvent::Error(format!(
                                    "failed to parse response: {e}"
                                )));
                            }
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(ApiEvent::Error(format!(
                            "failed to read response body: {e}"
                        )));
                    }
                }
            }
            Err(e) => {
                // ureq returns HTTP errors (4xx, 5xx) as Err variants.
                let _ = tx.send(ApiEvent::Error(format!("request failed: {e}")));
            }
        }
    });

    ApiHandle { rx, done: false }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Agent, AgentMessage, AgentTask};
    use crate::tools::{available_tools, ToolCall, ToolResult, ToolType};

    // -- Config tests -------------------------------------------------------

    #[test]
    fn config_new_defaults() {
        let config = ClaudeConfig::new("sk-test-key");
        assert_eq!(config.api_key, "sk-test-key");
        assert_eq!(config.model, DEFAULT_MODEL);
        assert_eq!(config.max_tokens, DEFAULT_MAX_TOKENS);
    }

    #[test]
    fn config_builder() {
        let config = ClaudeConfig::new("sk-key")
            .with_model("claude-opus-4-20250514")
            .with_max_tokens(8192);
        assert_eq!(config.api_key, "sk-key");
        assert_eq!(config.model, "claude-opus-4-20250514");
        assert_eq!(config.max_tokens, 8192);
    }

    #[test]
    fn config_from_env_returns_none_without_var() {
        // Rust 2024 makes set_var/remove_var unsafe, so we test indirectly.
        // In CI without ANTHROPIC_API_KEY set, this returns None.
        if std::env::var("ANTHROPIC_API_KEY").is_err() {
            assert!(ClaudeConfig::from_env().is_none());
        }
    }

    // -- Message format conversion ------------------------------------------

    fn make_agent() -> Agent {
        Agent::new(
            1,
            AgentTask::FreeForm {
                prompt: "test task".into(),
            },
        )
    }

    #[test]
    fn build_messages_user_assistant() {
        let messages = vec![
            AgentMessage::User("Hello".into()),
            AgentMessage::Assistant("Hi there".into()),
        ];
        let result = build_messages(&messages);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0]["role"], "user");
        assert_eq!(result[0]["content"], "Hello");
        assert_eq!(result[1]["role"], "assistant");
        assert_eq!(result[1]["content"], "Hi there");
    }

    #[test]
    fn build_messages_skips_system() {
        let messages = vec![
            AgentMessage::System("system prompt".into()),
            AgentMessage::User("Hello".into()),
        ];
        let result = build_messages(&messages);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["role"], "user");
    }

    #[test]
    fn build_messages_tool_call_format() {
        let messages = vec![AgentMessage::ToolCall(ToolCall {
            tool: ToolType::ReadFile,
            args: serde_json::json!({"path": "/etc/hosts"}),
        })];
        let ids = vec!["toolu_abc123".to_owned()];
        let result = build_messages_with_ids(&messages, &ids);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["role"], "assistant");

        let content = result[0]["content"].as_array().expect("content array");
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "tool_use");
        assert_eq!(content[0]["id"], "toolu_abc123");
        assert_eq!(content[0]["name"], "read_file");
        assert_eq!(content[0]["input"]["path"], "/etc/hosts");
    }

    #[test]
    fn build_messages_tool_call_generates_placeholder_id() {
        let messages = vec![AgentMessage::ToolCall(ToolCall {
            tool: ToolType::ReadFile,
            args: serde_json::json!({"path": "test.txt"}),
        })];
        let result = build_messages(&messages);
        let content = result[0]["content"].as_array().unwrap();
        assert_eq!(content[0]["id"], "toolu_0");
    }

    #[test]
    fn build_messages_consecutive_tool_calls_grouped() {
        let messages = vec![
            AgentMessage::ToolCall(ToolCall {
                tool: ToolType::ReadFile,
                args: serde_json::json!({"path": "a.txt"}),
            }),
            AgentMessage::ToolCall(ToolCall {
                tool: ToolType::ListFiles,
                args: serde_json::json!({"path": "/tmp"}),
            }),
        ];
        let ids = vec!["tc_1".to_owned(), "tc_2".to_owned()];
        let result = build_messages_with_ids(&messages, &ids);
        // Should be one assistant message with two content blocks.
        assert_eq!(result.len(), 1);
        let content = result[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["id"], "tc_1");
        assert_eq!(content[1]["id"], "tc_2");
    }

    #[test]
    fn build_messages_tool_result_format() {
        let messages = vec![AgentMessage::ToolResult(ToolResult {
            tool: ToolType::ReadFile,
            success: true,
            output: "127.0.0.1 localhost".into(),
        })];
        let ids = vec!["toolu_abc123".to_owned()];
        let result = build_messages_with_ids(&messages, &ids);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["role"], "user");

        let content = result[0]["content"].as_array().expect("content array");
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "tool_result");
        assert_eq!(content[0]["tool_use_id"], "toolu_abc123");
        assert_eq!(content[0]["content"], "127.0.0.1 localhost");
        // is_error should not be present when success is true.
        assert!(content[0].get("is_error").is_none());
    }

    #[test]
    fn build_messages_tool_result_error() {
        let messages = vec![AgentMessage::ToolResult(ToolResult {
            tool: ToolType::ReadFile,
            success: false,
            output: "permission denied".into(),
        })];
        let result = build_messages(&messages);
        let content = result[0]["content"].as_array().unwrap();
        assert_eq!(content[0]["is_error"], true);
    }

    #[test]
    fn build_messages_consecutive_tool_results_grouped() {
        let messages = vec![
            AgentMessage::ToolResult(ToolResult {
                tool: ToolType::ReadFile,
                success: true,
                output: "ok".into(),
            }),
            AgentMessage::ToolResult(ToolResult {
                tool: ToolType::ListFiles,
                success: true,
                output: "also ok".into(),
            }),
        ];
        let result = build_messages(&messages);
        assert_eq!(result.len(), 1);
        let content = result[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
    }

    #[test]
    fn build_messages_full_conversation() {
        let messages = vec![
            AgentMessage::User("Read the config file".into()),
            AgentMessage::ToolCall(ToolCall {
                tool: ToolType::ReadFile,
                args: serde_json::json!({"path": "config.toml"}),
            }),
            AgentMessage::ToolResult(ToolResult {
                tool: ToolType::ReadFile,
                success: true,
                output: "[server]\nport = 8080".into(),
            }),
            AgentMessage::Assistant("The config file sets the server port to 8080.".into()),
        ];
        let ids = vec!["tc_1".to_owned()];
        let result = build_messages_with_ids(&messages, &ids);
        assert_eq!(result.len(), 4);
        assert_eq!(result[0]["role"], "user");
        assert_eq!(result[1]["role"], "assistant");
        assert_eq!(result[2]["role"], "user");
        assert_eq!(result[3]["role"], "assistant");

        // Verify the tool_use_id matches.
        let tc_content = result[1]["content"].as_array().unwrap();
        assert_eq!(tc_content[0]["id"], "tc_1");
        let tr_content = result[2]["content"].as_array().unwrap();
        assert_eq!(tr_content[0]["tool_use_id"], "tc_1");
    }

    // -- Tool definition generation -----------------------------------------

    #[test]
    fn build_tools_produces_valid_api_format() {
        let tools = available_tools();
        let api_tools = build_tools(&tools);

        assert_eq!(api_tools.len(), 8);
        for tool in &api_tools {
            assert!(tool["name"].is_string());
            assert!(tool["description"].is_string());
            // The API field is `input_schema`, mapped from ToolDefinition.parameters.
            assert!(tool["input_schema"]["type"].as_str() == Some("object"));
        }
    }

    // -- Request body -------------------------------------------------------

    #[test]
    fn build_request_body_structure() {
        let config = ClaudeConfig::new("sk-test");
        let mut agent = make_agent();
        agent.push_message(AgentMessage::User("What time is it?".into()));

        let body = build_request_body(&config, &agent, &available_tools(), &[]);

        assert_eq!(body["model"], DEFAULT_MODEL);
        assert_eq!(body["max_tokens"], DEFAULT_MAX_TOKENS);
        assert!(body["system"].is_string());
        assert!(body["messages"].is_array());
        assert!(body["tools"].is_array());
    }

    #[test]
    fn build_request_body_no_tools_omits_field() {
        let config = ClaudeConfig::new("sk-test");
        let mut agent = make_agent();
        agent.push_message(AgentMessage::User("Hello".into()));

        let body = build_request_body(&config, &agent, &[], &[]);

        assert!(body.get("tools").is_none());
    }

    // -- Response parsing ---------------------------------------------------

    #[test]
    fn parse_response_text_block() {
        let response = serde_json::json!({
            "id": "msg_123",
            "type": "message",
            "role": "assistant",
            "content": [
                {
                    "type": "text",
                    "text": "Hello! How can I help?"
                }
            ],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 8}
        });

        let (tx, rx) = mpsc::channel();
        parse_response(&response, &tx);

        let events: Vec<_> = rx.try_iter().collect();
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], ApiEvent::TextDelta(t) if t == "Hello! How can I help?"));
        assert!(matches!(&events[1], ApiEvent::Done));
    }

    #[test]
    fn parse_response_tool_use_block() {
        let response = serde_json::json!({
            "id": "msg_456",
            "type": "message",
            "role": "assistant",
            "content": [
                {
                    "type": "tool_use",
                    "id": "toolu_abc",
                    "name": "read_file",
                    "input": {"path": "/etc/hosts"}
                }
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 10, "output_tokens": 20}
        });

        let (tx, rx) = mpsc::channel();
        parse_response(&response, &tx);

        let events: Vec<_> = rx.try_iter().collect();
        assert_eq!(events.len(), 2);
        match &events[0] {
            ApiEvent::ToolUse { id, call } => {
                assert_eq!(id, "toolu_abc");
                assert_eq!(call.tool, ToolType::ReadFile);
                assert_eq!(call.args["path"], "/etc/hosts");
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
        assert!(matches!(&events[1], ApiEvent::Done));
    }

    #[test]
    fn parse_response_mixed_blocks() {
        let response = serde_json::json!({
            "id": "msg_789",
            "type": "message",
            "role": "assistant",
            "content": [
                {
                    "type": "text",
                    "text": "Let me read that file."
                },
                {
                    "type": "tool_use",
                    "id": "toolu_xyz",
                    "name": "read_file",
                    "input": {"path": "Cargo.toml"}
                }
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 15, "output_tokens": 25}
        });

        let (tx, rx) = mpsc::channel();
        parse_response(&response, &tx);

        let events: Vec<_> = rx.try_iter().collect();
        assert_eq!(events.len(), 3);
        assert!(matches!(&events[0], ApiEvent::TextDelta(_)));
        assert!(matches!(&events[1], ApiEvent::ToolUse { .. }));
        assert!(matches!(&events[2], ApiEvent::Done));
    }

    #[test]
    fn parse_response_api_error() {
        let response = serde_json::json!({
            "type": "error",
            "error": {
                "type": "invalid_request_error",
                "message": "max_tokens must be a positive integer"
            }
        });

        let (tx, rx) = mpsc::channel();
        parse_response(&response, &tx);

        let events: Vec<_> = rx.try_iter().collect();
        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0], ApiEvent::Error(msg) if msg.contains("max_tokens"))
        );
    }

    #[test]
    fn parse_response_unknown_tool() {
        let response = serde_json::json!({
            "id": "msg_bad",
            "type": "message",
            "role": "assistant",
            "content": [
                {
                    "type": "tool_use",
                    "id": "toolu_bad",
                    "name": "nonexistent_tool",
                    "input": {}
                }
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 5, "output_tokens": 10}
        });

        let (tx, rx) = mpsc::channel();
        parse_response(&response, &tx);

        let events: Vec<_> = rx.try_iter().collect();
        // Should get an error for the unknown tool, then Done.
        assert!(events
            .iter()
            .any(|e| matches!(e, ApiEvent::Error(msg) if msg.contains("nonexistent_tool"))));
    }

    #[test]
    fn parse_response_missing_content() {
        let response = serde_json::json!({
            "id": "msg_empty",
            "type": "message",
            "role": "assistant",
        });

        let (tx, rx) = mpsc::channel();
        parse_response(&response, &tx);

        let events: Vec<_> = rx.try_iter().collect();
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], ApiEvent::Error(msg) if msg.contains("content")));
    }

    // -- ApiHandle ----------------------------------------------------------

    #[test]
    fn api_handle_try_recv_empty() {
        let (_tx, rx) = mpsc::channel::<ApiEvent>();
        let mut handle = ApiHandle { rx, done: false };
        assert!(handle.try_recv().is_none());
        assert!(!handle.is_done());
    }

    #[test]
    fn api_handle_marks_done_on_done_event() {
        let (tx, rx) = mpsc::channel();
        tx.send(ApiEvent::Done).unwrap();
        drop(tx);

        let mut handle = ApiHandle { rx, done: false };
        let event = handle.try_recv();
        assert!(matches!(event, Some(ApiEvent::Done)));
        assert!(handle.is_done());
    }

    #[test]
    fn api_handle_marks_done_on_error_event() {
        let (tx, rx) = mpsc::channel();
        tx.send(ApiEvent::Error("something broke".into())).unwrap();
        drop(tx);

        let mut handle = ApiHandle { rx, done: false };
        let event = handle.try_recv();
        assert!(matches!(event, Some(ApiEvent::Error(_))));
        assert!(handle.is_done());
    }

    #[test]
    fn api_handle_detects_disconnected_sender() {
        let (tx, rx) = mpsc::channel::<ApiEvent>();
        drop(tx); // Disconnect immediately.

        let mut handle = ApiHandle { rx, done: false };
        let event = handle.try_recv();
        assert!(
            matches!(&event, Some(ApiEvent::Error(msg)) if msg.contains("disconnected"))
        );
        assert!(handle.is_done());
    }

    #[test]
    fn api_handle_returns_none_after_done() {
        let (tx, rx) = mpsc::channel();
        tx.send(ApiEvent::Done).unwrap();
        drop(tx);

        let mut handle = ApiHandle { rx, done: false };
        handle.try_recv(); // consume Done
        assert!(handle.is_done());
        // Subsequent calls return None (not a spurious disconnect error).
        assert!(handle.try_recv().is_none());
    }
}
