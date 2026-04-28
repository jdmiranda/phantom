//! Multi-backend chat abstraction.
//!
//! This module introduces [`ChatBackend`], a vendor-agnostic trait that the
//! agent loop drives. Implementations encapsulate the request/response shape
//! for a particular chat-completion API (Anthropic, OpenAI, and so on). The
//! agent loop sees one canonical event stream — the same [`ApiEvent`] enum
//! the existing Anthropic path already produces — so adding a backend is a
//! matter of mapping that vendor's tool-use shape to/from Anthropic's.
//!
//! ## Why [`ApiEvent`] is the canonical shape
//!
//! The existing Claude integration in [`crate::api`] is wired into three
//! callers (the GUI agent pane, the headless REPL, and the brain
//! investigator). Reshaping that public surface would ripple through every
//! caller. Instead we treat [`ApiEvent`] as the canonical event vocabulary
//! and translate the OpenAI Chat Completions response into the same events.
//!
//! ## Tool-use mapping
//!
//! Anthropic returns tool calls as `content` blocks of type `tool_use` with
//! `{ id, name, input }`. OpenAI returns tool calls in `message.tool_calls`
//! as `{ id, type: "function", function: { name, arguments } }` where
//! `arguments` is a JSON-encoded string. The OpenAI backend parses
//! `arguments` and re-emits the call as an `ApiEvent::ToolUse { id, call }`
//! with the same `id` the agent loop will use to correlate the eventual
//! `tool_result`. On the request side, OpenAI expects tool results as
//! messages with `role: "tool"` and `tool_call_id`, while Anthropic embeds
//! them as `tool_result` content blocks inside a user message — the OpenAI
//! backend rewrites the conversation as it serializes.

use std::sync::mpsc;

use serde_json::Value;

use crate::agent::{Agent, AgentMessage};
use crate::api::{self, ApiEvent, ApiHandle, ClaudeConfig};
use crate::tools::{ToolCall, ToolDefinition, ToolType};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors that can occur when constructing or invoking a [`ChatBackend`].
#[derive(Debug, thiserror::Error)]
pub enum ChatError {
    /// The required API key is unset or empty.
    #[error("backend not configured: {0}")]
    NotConfigured(String),
    /// The HTTP transport failed.
    #[error("transport error: {0}")]
    Transport(String),
    /// Other error.
    #[error("{0}")]
    Other(String),
}

// ---------------------------------------------------------------------------
// ChatRequest / ChatResponse
// ---------------------------------------------------------------------------

/// One round of a chat conversation, vendor-agnostic.
///
/// This wraps the same inputs the Anthropic path has always used: an [`Agent`]
/// (carrying system prompt + message history), a tool list, and the running
/// list of tool-use IDs needed to correlate calls and results across turns.
pub struct ChatRequest<'a> {
    /// The agent whose conversation we're advancing.
    pub agent: &'a Agent,
    /// Tool definitions exposed to the assistant for this round.
    pub tools: &'a [ToolDefinition],
    /// API-assigned IDs for prior tool calls (positionally aligned with
    /// `AgentMessage::ToolCall` entries in `agent.messages`).
    pub tool_use_ids: &'a [String],
    /// Maximum tokens to generate (vendor-specific clamping may apply).
    pub max_tokens: u32,
}

/// A poll-able handle to an in-flight chat completion.
///
/// This is structurally identical to [`ApiHandle`] — both backends
/// stream [`ApiEvent`]s over an mpsc channel — but exposing it as
/// `ChatResponse` keeps the trait signature backend-agnostic.
pub struct ChatResponse {
    handle: ApiHandle,
}

impl ChatResponse {
    /// Wrap an existing [`ApiHandle`].
    pub fn from_handle(handle: ApiHandle) -> Self {
        Self { handle }
    }

    /// Wrap a raw receiver. Useful for backends that drive the channel
    /// directly without going through [`api::send_message`].
    pub fn from_receiver(rx: mpsc::Receiver<ApiEvent>) -> Self {
        Self {
            handle: ApiHandle::from_receiver(rx),
        }
    }

    /// Non-blocking poll for the next event.
    pub fn try_recv(&mut self) -> Option<ApiEvent> {
        self.handle.try_recv()
    }

    /// Whether the request has completed.
    pub fn is_done(&self) -> bool {
        self.handle.is_done()
    }

    /// Consume into the underlying [`ApiHandle`] for callers that need the
    /// existing concrete type (e.g. the agent pane caches it directly).
    pub fn into_handle(self) -> ApiHandle {
        self.handle
    }
}

// ---------------------------------------------------------------------------
// ChatBackend trait
// ---------------------------------------------------------------------------

/// A pluggable chat-completion provider.
///
/// One round of conversation: send messages + tool defs, get back a
/// streaming response. Implementations encapsulate the per-vendor request
/// and response shape; the agent loop sees one canonical event stream.
pub trait ChatBackend: Send + Sync {
    /// Stable name used for logging and tests.
    fn name(&self) -> &'static str;

    /// Issue one round of chat completion.
    fn complete(&self, request: ChatRequest<'_>) -> Result<ChatResponse, ChatError>;
}

// ---------------------------------------------------------------------------
// ChatModel — selector enum
// ---------------------------------------------------------------------------

/// Which chat backend a particular agent should use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatModel {
    /// Anthropic Claude with the given model id (e.g. `"claude-opus-4-7"`).
    Claude(String),
    /// OpenAI Chat Completions with the given model id (e.g. `"gpt-4o"`).
    OpenAi(String),
}

impl ChatModel {
    /// The default Claude model used today by every existing caller.
    pub fn default_claude() -> Self {
        Self::Claude("claude-opus-4-7".to_owned())
    }

    /// The default OpenAI model.
    pub fn default_openai() -> Self {
        Self::OpenAi("gpt-4o".to_owned())
    }

    /// Returns the backend's stable name (`"claude"` or `"openai"`).
    pub fn backend_name(&self) -> &'static str {
        match self {
            Self::Claude(_) => "claude",
            Self::OpenAi(_) => "openai",
        }
    }

    /// Parse a `PHANTOM_AGENT_MODEL` env-var value.
    ///
    /// Recognized shapes:
    /// - `"claude"` / `"anthropic"` → [`Self::default_claude`]
    /// - `"openai"` / `"gpt"` → [`Self::default_openai`]
    /// - `"claude:<model-id>"` or `"anthropic:<model-id>"`
    /// - `"openai:<model-id>"` or `"gpt:<model-id>"`
    ///
    /// Anything else returns `None` so the caller can fall through to a
    /// default. Whitespace and case are ignored.
    pub fn from_env_str(raw: &str) -> Option<Self> {
        let s = raw.trim().to_ascii_lowercase();
        if let Some((backend, model)) = s.split_once(':') {
            return match backend.trim() {
                "claude" | "anthropic" => Some(Self::Claude(model.trim().to_owned())),
                "openai" | "gpt" => Some(Self::OpenAi(model.trim().to_owned())),
                _ => None,
            };
        }
        match s.as_str() {
            "claude" | "anthropic" => Some(Self::default_claude()),
            "openai" | "gpt" => Some(Self::default_openai()),
            _ => None,
        }
    }
}

impl Default for ChatModel {
    /// Defaults to [`Self::default_claude`] — every legacy caller used Claude
    /// before the chat-backend trait existed, and we preserve that behavior.
    fn default() -> Self {
        Self::default_claude()
    }
}

/// Build a backend instance from a [`ChatModel`] selector.
///
/// Reads credentials from the environment (`ANTHROPIC_API_KEY` for Claude,
/// `OPENAI_API_KEY` for OpenAI). Returns [`ChatError::NotConfigured`] when
/// the required key is missing.
pub fn build_backend(model: &ChatModel) -> Result<Box<dyn ChatBackend>, ChatError> {
    match model {
        ChatModel::Claude(model_id) => {
            let backend = ClaudeBackend::from_env()?.with_model(model_id.clone());
            Ok(Box::new(backend))
        }
        ChatModel::OpenAi(model_id) => {
            let backend = OpenAiChatBackend::from_env()?.with_model(model_id.clone());
            Ok(Box::new(backend))
        }
    }
}

// ---------------------------------------------------------------------------
// ClaudeBackend
// ---------------------------------------------------------------------------

/// Anthropic Claude implementation of [`ChatBackend`].
///
/// This is a thin wrapper over the existing [`api::send_message`]; the
/// existing public [`ClaudeConfig`] and `send_message` continue to work
/// byte-for-byte.
pub struct ClaudeBackend {
    config: ClaudeConfig,
}

impl ClaudeBackend {
    /// Load credentials from `ANTHROPIC_API_KEY`.
    pub fn from_env() -> Result<Self, ChatError> {
        let config = ClaudeConfig::from_env().ok_or_else(|| {
            ChatError::NotConfigured("ANTHROPIC_API_KEY missing or empty".into())
        })?;
        Ok(Self { config })
    }

    /// Construct from an explicit config.
    pub fn from_config(config: ClaudeConfig) -> Self {
        Self { config }
    }

    /// Override the model id.
    pub fn with_model(mut self, model: String) -> Self {
        self.config.model = model;
        self
    }

    /// Borrow the underlying [`ClaudeConfig`].
    pub fn config(&self) -> &ClaudeConfig {
        &self.config
    }
}

impl ChatBackend for ClaudeBackend {
    fn name(&self) -> &'static str {
        "claude"
    }

    fn complete(&self, request: ChatRequest<'_>) -> Result<ChatResponse, ChatError> {
        // Honour the per-request max_tokens override.
        let mut config = self.config.clone();
        config.max_tokens = request.max_tokens;
        let handle = api::send_message(
            &config,
            request.agent,
            request.tools,
            request.tool_use_ids,
        );
        Ok(ChatResponse::from_handle(handle))
    }
}

// ---------------------------------------------------------------------------
// OpenAiChatBackend
// ---------------------------------------------------------------------------

const OPENAI_API_URL: &str = "https://api.openai.com/v1/chat/completions";
const OPENAI_DEFAULT_MODEL: &str = "gpt-4o";

/// OpenAI Chat Completions implementation of [`ChatBackend`].
pub struct OpenAiChatBackend {
    api_key: String,
    model: String,
}

impl OpenAiChatBackend {
    /// Load credentials from `OPENAI_API_KEY`.
    pub fn from_env() -> Result<Self, ChatError> {
        let api_key = std::env::var("OPENAI_API_KEY").map_err(|_| {
            ChatError::NotConfigured("OPENAI_API_KEY missing or empty".into())
        })?;
        if api_key.is_empty() {
            return Err(ChatError::NotConfigured(
                "OPENAI_API_KEY missing or empty".into(),
            ));
        }
        Ok(Self {
            api_key,
            model: OPENAI_DEFAULT_MODEL.to_owned(),
        })
    }

    /// Construct from an explicit API key (for tests).
    pub fn from_api_key(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            model: OPENAI_DEFAULT_MODEL.to_owned(),
        }
    }

    /// Override the model id.
    pub fn with_model(mut self, model: String) -> Self {
        self.model = model;
        self
    }
}

impl ChatBackend for OpenAiChatBackend {
    fn name(&self) -> &'static str {
        "openai"
    }

    fn complete(&self, request: ChatRequest<'_>) -> Result<ChatResponse, ChatError> {
        let body = build_openai_request_body(
            &self.model,
            request.max_tokens,
            request.agent,
            request.tools,
            request.tool_use_ids,
        );

        let (tx, rx) = mpsc::channel();
        let api_key = self.api_key.clone();
        let body_str = serde_json::to_string(&body)
            .map_err(|e| ChatError::Other(format!("failed to serialise request: {e}")))?;

        std::thread::spawn(move || {
            // Use the OS trust store for cert validation. The default
            // WebPki/Mozilla root bundle that ships with `webpki-roots`
            // can miss intermediates on some endpoints (notably OpenAI),
            // producing `UnknownIssuer` errors. The macOS Keychain /
            // Windows CertStore / Linux system store recognise them.
            let tls = ureq::tls::TlsConfig::builder()
                .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                .build();
            let agent = ureq::Agent::new_with_config(
                ureq::config::Config::builder()
                    .tls_config(tls)
                    .timeout_global(Some(std::time::Duration::from_secs(120)))
                    .build(),
            );

            let result = agent
                .post(OPENAI_API_URL)
                .header("authorization", &format!("Bearer {api_key}"))
                .header("content-type", "application/json")
                .send(body_str.as_bytes());

            match result {
                Ok(mut response) => match response.body_mut().read_to_string() {
                    Ok(text) => match serde_json::from_str::<Value>(&text) {
                        Ok(json) => parse_openai_response(&json, &tx),
                        Err(e) => {
                            let _ = tx.send(ApiEvent::Error(format!(
                                "failed to parse response: {e}"
                            )));
                        }
                    },
                    Err(e) => {
                        let _ = tx.send(ApiEvent::Error(format!(
                            "failed to read response body: {e}"
                        )));
                    }
                },
                Err(e) => {
                    let _ = tx.send(ApiEvent::Error(format!("request failed: {e}")));
                }
            }
        });

        Ok(ChatResponse::from_receiver(rx))
    }
}

// ---------------------------------------------------------------------------
// OpenAI request shaping
// ---------------------------------------------------------------------------

/// Build the OpenAI request body, mapping Anthropic-shaped messages and
/// tools into OpenAI shape.
fn build_openai_request_body(
    model: &str,
    max_tokens: u32,
    agent: &Agent,
    tools: &[ToolDefinition],
    tool_use_ids: &[String],
) -> Value {
    let mut body = serde_json::json!({
        "model": model,
        "max_tokens": max_tokens,
        "messages": build_openai_messages(agent, tool_use_ids),
    });

    if !tools.is_empty() {
        body["tools"] = Value::Array(build_openai_tools(tools));
        // Let the model decide whether to call a tool.
        body["tool_choice"] = Value::String("auto".into());
    }

    body
}

/// Map the agent's conversation into OpenAI message shape.
///
/// Differences from Anthropic:
/// * The system prompt rides on a `role: "system"` message at the head, not
///   a top-level `system` field.
/// * `tool_use` content blocks become an assistant message with a
///   `tool_calls` array; the function arguments are JSON-encoded as a string.
/// * `tool_result` blocks become messages with `role: "tool"` and a
///   `tool_call_id` (one message per result, no grouping).
fn build_openai_messages(agent: &Agent, tool_use_ids: &[String]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();

    // Lead with a system message.
    out.push(serde_json::json!({
        "role": "system",
        "content": agent.system_prompt(),
    }));

    let mut tool_idx: usize = 0;
    let mut result_idx: usize = 0;
    let mut i = 0;
    while i < agent.messages.len() {
        match &agent.messages[i] {
            AgentMessage::System(_) => {
                // Already handled at the head.
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
                // Group consecutive tool calls into a single assistant message
                // with multiple tool_calls entries.
                let mut tool_calls = Vec::new();
                while i < agent.messages.len() {
                    if let AgentMessage::ToolCall(tc) = &agent.messages[i] {
                        let id = tool_use_ids
                            .get(tool_idx)
                            .cloned()
                            .unwrap_or_else(|| format!("call_{tool_idx}"));
                        let arguments = serde_json::to_string(&tc.args)
                            .unwrap_or_else(|_| "{}".to_owned());
                        tool_calls.push(serde_json::json!({
                            "id": id,
                            "type": "function",
                            "function": {
                                "name": tc.tool.api_name(),
                                "arguments": arguments,
                            }
                        }));
                        tool_idx += 1;
                        i += 1;
                    } else {
                        break;
                    }
                }
                out.push(serde_json::json!({
                    "role": "assistant",
                    "content": Value::Null,
                    "tool_calls": tool_calls,
                }));
            }
            AgentMessage::ToolResult(tr) => {
                let id = tool_use_ids
                    .get(result_idx)
                    .cloned()
                    .unwrap_or_else(|| format!("call_{result_idx}"));
                let mut content = tr.output.clone();
                if !tr.success {
                    content = format!("ERROR: {content}");
                }
                out.push(serde_json::json!({
                    "role": "tool",
                    "tool_call_id": id,
                    "content": content,
                }));
                result_idx += 1;
                i += 1;
            }
        }
    }

    out
}

/// Map [`ToolDefinition`] entries into the OpenAI `tools[]` shape.
fn build_openai_tools(tools: &[ToolDefinition]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                }
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// OpenAI response parsing
// ---------------------------------------------------------------------------

/// Translate an OpenAI Chat Completions response into [`ApiEvent`]s.
fn parse_openai_response(body: &Value, tx: &mpsc::Sender<ApiEvent>) {
    if let Some(err) = body.get("error") {
        let msg = err
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown OpenAI API error");
        let _ = tx.send(ApiEvent::Error(msg.to_owned()));
        return;
    }

    let Some(choices) = body.get("choices").and_then(|c| c.as_array()) else {
        let _ = tx.send(ApiEvent::Error(
            "OpenAI response missing choices array".into(),
        ));
        return;
    };

    let Some(choice) = choices.first() else {
        let _ = tx.send(ApiEvent::Error(
            "OpenAI response had empty choices array".into(),
        ));
        return;
    };

    let Some(message) = choice.get("message") else {
        let _ = tx.send(ApiEvent::Error(
            "OpenAI response missing message".into(),
        ));
        return;
    };

    // Assistant text (may be null when only tool calls are returned).
    if let Some(content) = message.get("content").and_then(|v| v.as_str()) {
        if !content.is_empty() {
            let _ = tx.send(ApiEvent::TextDelta(content.to_owned()));
        }
    }

    // Tool calls.
    if let Some(tool_calls) = message.get("tool_calls").and_then(|v| v.as_array()) {
        for call in tool_calls {
            let id = call
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            let function = match call.get("function") {
                Some(f) => f,
                None => continue,
            };
            let name = function
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let arguments_str = function
                .get("arguments")
                .and_then(|v| v.as_str())
                .unwrap_or("{}");
            let args: Value = serde_json::from_str(arguments_str)
                .unwrap_or(Value::Object(serde_json::Map::new()));

            if let Some(tool) = ToolType::from_api_name(name) {
                let _ = tx.send(ApiEvent::ToolUse {
                    id,
                    call: ToolCall { tool, args },
                });
            } else {
                let _ = tx.send(ApiEvent::Error(format!(
                    "unknown tool in response: {name}"
                )));
            }
        }
    }

    let _ = tx.send(ApiEvent::Done);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Agent, AgentMessage, AgentTask};
    use crate::tools::{available_tools, ToolCall, ToolResult, ToolType};

    fn make_agent() -> Agent {
        Agent::new(
            1,
            AgentTask::FreeForm {
                prompt: "test".into(),
            },
        )
    }

    // -- from_env / NotConfigured ------------------------------------------

    #[test]
    fn claude_backend_from_env_returns_not_configured_when_missing() {
        // We can't reliably set/unset env vars in Rust 2024 (unsafe), so this
        // test only runs when the var is genuinely absent.
        if std::env::var("ANTHROPIC_API_KEY").is_err() {
            let result = ClaudeBackend::from_env();
            assert!(matches!(result, Err(ChatError::NotConfigured(_))));
        }
    }

    #[test]
    fn openai_backend_from_env_returns_not_configured_when_missing() {
        if std::env::var("OPENAI_API_KEY").is_err() {
            let result = OpenAiChatBackend::from_env();
            assert!(matches!(result, Err(ChatError::NotConfigured(_))));
        }
    }

    // -- ChatModel / build_backend -----------------------------------------

    #[test]
    fn chat_model_defaults_use_expected_ids() {
        match ChatModel::default_claude() {
            ChatModel::Claude(m) => assert_eq!(m, "claude-opus-4-7"),
            other => panic!("expected Claude, got {other:?}"),
        }
        match ChatModel::default_openai() {
            ChatModel::OpenAi(m) => assert_eq!(m, "gpt-4o"),
            other => panic!("expected OpenAi, got {other:?}"),
        }
    }

    #[test]
    fn chat_model_backend_name_distinguishes_variants() {
        assert_eq!(ChatModel::Claude("x".into()).backend_name(), "claude");
        assert_eq!(ChatModel::OpenAi("y".into()).backend_name(), "openai");
    }

    #[test]
    fn build_backend_returns_correct_impl() {
        // Claude path: only succeeds if ANTHROPIC_API_KEY is set; if not,
        // we still verify that the error is NotConfigured (not a panic).
        let claude_result = build_backend(&ChatModel::Claude("claude-opus-4-7".into()));
        match claude_result {
            Ok(backend) => assert_eq!(backend.name(), "claude"),
            Err(ChatError::NotConfigured(_)) => {
                assert!(std::env::var("ANTHROPIC_API_KEY").is_err());
            }
            Err(other) => panic!("unexpected error: {other}"),
        }

        let openai_result = build_backend(&ChatModel::OpenAi("gpt-4o".into()));
        match openai_result {
            Ok(backend) => assert_eq!(backend.name(), "openai"),
            Err(ChatError::NotConfigured(_)) => {
                assert!(std::env::var("OPENAI_API_KEY").is_err());
            }
            Err(other) => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn claude_backend_name() {
        let backend = ClaudeBackend::from_config(ClaudeConfig::new("sk-test"));
        assert_eq!(backend.name(), "claude");
    }

    #[test]
    fn openai_backend_name() {
        let backend = OpenAiChatBackend::from_api_key("sk-test");
        assert_eq!(backend.name(), "openai");
    }

    // -- Claude format round-trip (sanity-check the existing path) ---------

    #[test]
    fn chat_request_round_trips_through_claude_format() {
        // The Claude backend delegates to api::send_message, whose request
        // shape we've previously asserted in api::tests. Here we just verify
        // that wrapping a ClaudeConfig in a ClaudeBackend preserves model id
        // and max_tokens overrides as the trait would receive them.
        let mut backend = ClaudeBackend::from_config(ClaudeConfig::new("sk-test"));
        backend = backend.with_model("claude-3-7-sonnet-20250219".into());
        assert_eq!(backend.config().model, "claude-3-7-sonnet-20250219");
        assert_eq!(backend.name(), "claude");

        // Verify that ChatRequest fields are usable via the trait without
        // needing to hit the network: we construct one and ensure it borrows
        // cleanly.
        let mut agent = make_agent();
        agent.push_message(AgentMessage::User("hello".into()));
        let tools = available_tools();
        let ids: Vec<String> = Vec::new();
        let request = ChatRequest {
            agent: &agent,
            tools: &tools,
            tool_use_ids: &ids,
            max_tokens: 1024,
        };
        // Trait object compatibility check:
        let dyn_backend: &dyn ChatBackend = &backend;
        assert_eq!(dyn_backend.name(), "claude");
        // We don't call complete() here — that hits the network. The mapping
        // logic is exercised in the api::tests module.
        let _ = request.tools.len();
    }

    // -- OpenAI request shape ---------------------------------------------

    #[test]
    fn chat_request_round_trips_through_openai_format() {
        let mut agent = make_agent();
        agent.push_message(AgentMessage::User("Read Cargo.toml".into()));

        let tools = available_tools();
        let ids: Vec<String> = Vec::new();
        let body = build_openai_request_body("gpt-4o", 2048, &agent, &tools, &ids);

        // Top-level shape.
        assert_eq!(body["model"], "gpt-4o");
        assert_eq!(body["max_tokens"], 2048);
        assert!(body["messages"].is_array());
        assert!(body["tools"].is_array());
        assert_eq!(body["tool_choice"], "auto");

        // Messages: head is system, then the user message.
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "system");
        assert!(
            messages[0]["content"]
                .as_str()
                .unwrap()
                .contains("AI assistant agent")
        );
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"], "Read Cargo.toml");

        // Tool shape: type=function, with name/description/parameters.
        let tool_array = body["tools"].as_array().unwrap();
        let first = &tool_array[0];
        assert_eq!(first["type"], "function");
        assert!(first["function"]["name"].is_string());
        assert!(first["function"]["description"].is_string());
        assert_eq!(first["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn openai_request_omits_tools_when_empty() {
        let mut agent = make_agent();
        agent.push_message(AgentMessage::User("hi".into()));
        let body = build_openai_request_body("gpt-4o", 512, &agent, &[], &[]);
        assert!(body.get("tools").is_none());
        assert!(body.get("tool_choice").is_none());
    }

    // -- OpenAI tool-use round-trip ---------------------------------------

    #[test]
    fn tool_use_block_roundtrip_openai() {
        // Stage 1: response contains an assistant tool_call. Verify our
        // parser emits ApiEvent::ToolUse with the right id/tool/args.
        let response = serde_json::json!({
            "id": "chatcmpl-1",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_abc123",
                        "type": "function",
                        "function": {
                            "name": "read_file",
                            "arguments": "{\"path\":\"Cargo.toml\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        });

        let (tx, rx) = mpsc::channel();
        parse_openai_response(&response, &tx);
        let events: Vec<_> = rx.try_iter().collect();
        assert_eq!(events.len(), 2, "events: {events:?}");
        match &events[0] {
            ApiEvent::ToolUse { id, call } => {
                assert_eq!(id, "call_abc123");
                assert_eq!(call.tool, ToolType::ReadFile);
                assert_eq!(call.args["path"], "Cargo.toml");
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
        assert!(matches!(&events[1], ApiEvent::Done));

        // Stage 2: build a follow-up request that echoes the tool_result back.
        let mut agent = make_agent();
        agent.push_message(AgentMessage::User("Read Cargo.toml".into()));
        agent.push_message(AgentMessage::ToolCall(ToolCall {
            tool: ToolType::ReadFile,
            args: serde_json::json!({"path": "Cargo.toml"}),
        }));
        agent.push_message(AgentMessage::ToolResult(ToolResult {
            tool: ToolType::ReadFile,
            success: true,
            output: "[package]\nname = \"phantom\"".into(),
            tool_name: String::new(),
            args_hash: String::new(),
            source_event_id: None,
        }));

        let ids = vec!["call_abc123".to_owned()];
        let body = build_openai_request_body("gpt-4o", 1024, &agent, &available_tools(), &ids);
        let messages = body["messages"].as_array().unwrap();

        // Head: system, user, assistant-with-tool_calls, tool-result.
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[1]["role"], "user");

        // Assistant message carrying the tool_call.
        assert_eq!(messages[2]["role"], "assistant");
        assert!(messages[2]["content"].is_null());
        let tool_calls = messages[2]["tool_calls"].as_array().unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0]["id"], "call_abc123");
        assert_eq!(tool_calls[0]["type"], "function");
        assert_eq!(tool_calls[0]["function"]["name"], "read_file");
        // arguments must be a JSON-encoded string, not an object.
        let args_str = tool_calls[0]["function"]["arguments"].as_str().unwrap();
        let parsed: Value = serde_json::from_str(args_str).unwrap();
        assert_eq!(parsed["path"], "Cargo.toml");

        // Tool result message with matching id.
        assert_eq!(messages[3]["role"], "tool");
        assert_eq!(messages[3]["tool_call_id"], "call_abc123");
        assert!(
            messages[3]["content"]
                .as_str()
                .unwrap()
                .contains("[package]")
        );
    }

    #[test]
    fn openai_tool_result_marks_errors() {
        let mut agent = make_agent();
        agent.push_message(AgentMessage::User("read it".into()));
        agent.push_message(AgentMessage::ToolCall(ToolCall {
            tool: ToolType::ReadFile,
            args: serde_json::json!({"path": "missing.txt"}),
        }));
        agent.push_message(AgentMessage::ToolResult(ToolResult {
            tool: ToolType::ReadFile,
            success: false,
            output: "no such file".into(),
            tool_name: String::new(),
            args_hash: String::new(),
            source_event_id: None,
        }));
        let ids = vec!["call_xyz".to_owned()];
        let body = build_openai_request_body("gpt-4o", 256, &agent, &[], &ids);
        let messages = body["messages"].as_array().unwrap();
        let tool_msg = messages.iter().find(|m| m["role"] == "tool").unwrap();
        assert!(
            tool_msg["content"]
                .as_str()
                .unwrap()
                .starts_with("ERROR:")
        );
    }

    #[test]
    fn openai_response_text_only() {
        let response = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "hello world"
                },
                "finish_reason": "stop"
            }]
        });
        let (tx, rx) = mpsc::channel();
        parse_openai_response(&response, &tx);
        let events: Vec<_> = rx.try_iter().collect();
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], ApiEvent::TextDelta(t) if t == "hello world"));
        assert!(matches!(&events[1], ApiEvent::Done));
    }

    #[test]
    fn openai_response_api_error() {
        let response = serde_json::json!({
            "error": {
                "type": "invalid_request_error",
                "message": "bad model id"
            }
        });
        let (tx, rx) = mpsc::channel();
        parse_openai_response(&response, &tx);
        let events: Vec<_> = rx.try_iter().collect();
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], ApiEvent::Error(m) if m.contains("bad model id")));
    }

    #[test]
    fn openai_response_unknown_tool_emits_error() {
        let response = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "nonexistent_tool",
                            "arguments": "{}"
                        }
                    }]
                }
            }]
        });
        let (tx, rx) = mpsc::channel();
        parse_openai_response(&response, &tx);
        let events: Vec<_> = rx.try_iter().collect();
        assert!(events.iter().any(
            |e| matches!(e, ApiEvent::Error(m) if m.contains("nonexistent_tool"))
        ));
    }

    // -- Live integration test (requires OPENAI_API_KEY) -------------------

    #[test]
    #[ignore]
    fn openai_live_say_hello() {
        let backend = match OpenAiChatBackend::from_env() {
            Ok(b) => b,
            Err(e) => {
                eprintln!("skipping live test: {e}");
                return;
            }
        };
        let mut agent = make_agent();
        agent.push_message(AgentMessage::User("say hello".into()));
        let request = ChatRequest {
            agent: &agent,
            tools: &[],
            tool_use_ids: &[],
            max_tokens: 64,
        };

        let mut response = backend.complete(request).expect("complete failed");
        let mut text = String::new();
        loop {
            match response.try_recv() {
                Some(ApiEvent::TextDelta(t)) => text.push_str(&t),
                Some(ApiEvent::ToolUse { .. }) => {
                    panic!("unexpected tool use in live hello test");
                }
                Some(ApiEvent::Done) => break,
                Some(ApiEvent::Error(e)) => panic!("live test error: {e}"),
                None => std::thread::sleep(std::time::Duration::from_millis(50)),
            }
        }
        assert!(!text.trim().is_empty(), "expected non-empty response");
    }
}
