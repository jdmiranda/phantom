# Provider Trait Audit — `ChatBackend` capability matrix

> Issue #19 · audited 2026-04-28 against `crates/phantom-agents/src/chat.rs`

---

## 1. Trait surface (as-shipped)

```rust
pub trait ChatBackend: Send + Sync {
    fn name(&self) -> &'static str;
    fn complete(&self, request: ChatRequest<'_>) -> Result<ChatResponse, ChatError>;
}

pub struct ChatRequest<'a> {
    pub agent:        &'a Agent,       // carries system prompt + message history
    pub tools:        &'a [ToolDefinition],
    pub tool_use_ids: &'a [String],    // correlate prior tool calls / results
    pub max_tokens:   u32,
}
```

`ChatResponse` wraps `ApiHandle` — a non-blocking `mpsc::Receiver<ApiEvent>` polled
each render frame. The canonical event vocabulary is:

| Event | Meaning |
|---|---|
| `TextDelta(String)` | assistant text chunk |
| `ToolUse { id, call }` | model invoked a tool |
| `Done` | stream finished cleanly |
| `Error(String)` | transport or API error |

---

## 2. Capability audit

### 2.1 Dimensions checked

| # | Capability | Claude | OpenAI | LocalBackend (planned) | Notes |
|---|---|:---:|:---:|:---:|---|
| 1 | **Streaming responses** | partial | partial | partial | Both impls use blocking HTTP + full-body parse; events drip via mpsc but the wire is not truly streamed. `ApiEvent::TextDelta` is the right slot — wire streaming is a backend-internal concern and does not require a trait change. |
| 2 | **Tool use (structured)** | implemented | implemented | polyfillable | Anthropic: `content[].tool_use` blocks. OpenAI: `message.tool_calls[]`. Both translate to `ApiEvent::ToolUse`. Local models (llama.cpp, Ollama): most expose an OpenAI-compatible `/v1/chat/completions`; an `OllamaBackend` can reuse the OpenAI shaping logic verbatim. |
| 3 | **System prompt** | implemented | implemented | transparent | `agent.system_prompt()` is passed as the top-level Anthropic `system` field (Claude) and a `role:"system"` message (OpenAI). A local backend receives it identically via `request.agent`. No trait change needed. |
| 4 | **`max_tokens`** | implemented | implemented | transparent | Carried in `ChatRequest::max_tokens`, applied per-request. Local models honour the same field in the OpenAI wire format. |
| 5 | **Temperature / sampling params** | not exposed | not exposed | not applicable | Neither existing backend exposes temperature. Phantom agents are task-oriented; determinism is preferred. If needed: add `Option<f32>` to `ChatRequest` (non-breaking — see §3). |
| 6 | **JSON mode / structured output** | not exposed | not exposed | not applicable | OpenAI supports `response_format: { type: "json_object" }`. Claude has no equivalent. Not needed by the current agent loop. If added: a `response_format: Option<ResponseFormat>` field on `ChatRequest`. |
| 7 | **Vision input (image data)** | not exposed | not exposed | not applicable | Both APIs accept base-64 image content blocks. The agent message history (`AgentMessage`) carries only `String` content; no image variant exists. Not needed today; would require extending `AgentMessage` and the trait together. |
| 8 | **Audio input** | unsupported | unsupported | unsupported | OpenAI Whisper/Realtime is a separate API surface. Claude has no audio input at present. Out of scope for Phantom agents. |
| 9 | **Token usage / cost tracking** | not exposed | not exposed | not applicable | Both response bodies contain usage counters (`usage.input_tokens`, etc.). Not parsed today. Could be added as an optional field on a future `ChatMetadata` without changing `ChatBackend`. |
| 10 | **Retry / rate-limit handling** | not implemented | not implemented | not applicable | HTTP errors are forwarded as `ApiEvent::Error`. Back-off and retry are the caller's responsibility. Could be a wrapper backend (`RetryBackend<B: ChatBackend>`) — no trait change needed. |
| 11 | **Model ID override at request time** | not exposed | not exposed | not applicable | Model is baked into the backend at construction (`with_model()`). Per-request model selection would require a field on `ChatRequest`. Not a gap for current use. |
| 12 | **Context window / token budget** | not exposed | not exposed | not applicable | `max_tokens` controls output budget only. Input context is not enforced by the trait; the backend or caller is responsible for truncating message history. |

### 2.2 Summary verdict

**The trait is sufficient for all current functionality and for a `LocalBackend`.**

- The only genuinely missing fields are `temperature` and `response_format`, both optional and additive — they do not require a breaking change.
- The `ChatRequest` struct is `#[non_exhaustive]` in spirit (all callers construct it inline), so adding `Option<f32>` fields is a one-line non-breaking extension.
- Vision and audio are out of scope for the agent use-case today.

---

## 3. What a `LocalBackend` needs

A backend targeting **llama.cpp server** or **Ollama** can reuse the existing OpenAI
request/response shape almost unchanged.

### Minimum viable `LocalBackend`

```rust
pub struct LocalBackend {
    /// Base URL, e.g. "http://localhost:11434/v1" (Ollama)
    /// or "http://localhost:8080/v1" (llama.cpp server).
    base_url: String,
    model:    String,
}

impl ChatBackend for LocalBackend {
    fn name(&self) -> &'static str { "local" }

    fn complete(&self, request: ChatRequest<'_>) -> Result<ChatResponse, ChatError> {
        // Reuse build_openai_request_body (already pub(crate)).
        // POST to {base_url}/chat/completions with no auth header.
        // Parse with parse_openai_response — Ollama and llama.cpp both
        // speak the OpenAI wire format.
        todo!("LocalBackend::complete")
    }
}
```

### Differences from OpenAI

| Concern | Ollama | llama.cpp server | Mitigation |
|---|---|---|---|
| Auth header | none required | none required | omit `Authorization` header |
| TLS | plain HTTP (localhost) | plain HTTP (localhost) | platform verifier still works; or skip TLS entirely |
| Tool use | models vary (llama3, mistral support it; older don't) | same | caller should pass `tools: &[]` for models without tool support |
| `stream` field | supported | supported | not needed — both return full body if omitted |
| Rate limits / 429 | none | none | existing error path is sufficient |
| Model id format | `"llama3:8b"` etc. | `"default"` or path | pass through as-is |

### `ChatModel` extension (when `LocalBackend` ships)

```rust
pub enum ChatModel {
    Claude(String),
    OpenAi(String),
    Local { base_url: String, model: String },  // new
}
```

And `build_backend` gains a third arm. **No changes to `ChatBackend` trait.**

---

## 4. Optional future extensions (non-breaking)

These can be added to `ChatRequest` as `Option<T>` fields at any time without
breaking existing callers because all callers construct the struct by name.

| Field | Type | When useful |
|---|---|---|
| `temperature` | `Option<f32>` | creative / exploratory agents |
| `response_format` | `Option<ResponseFormat>` | structured JSON output agents |
| `stop_sequences` | `Option<Vec<String>>` | output boundary control |
| `seed` | `Option<u64>` | reproducible test runs |

---

## 5. Trait change decision

**No trait changes made.**

All identified gaps are either:
- Out of scope for current Phantom agents (vision, audio, JSON mode), or
- Addable as `Option<T>` fields on `ChatRequest` without touching the trait, or
- Backend-internal concerns (streaming wire, retry, TLS).

The existing trait is stable and correct.

---

## 6. Provider × Capability matrix (quick reference)

Legend: ✅ implemented · 🟡 polyfillable/planned · ❌ unsupported · — not applicable

| Capability | Claude | OpenAI | LocalBackend (planned) |
|---|:---:|:---:|:---:|
| Streaming (event model) | ✅ | ✅ | 🟡 |
| Tool use | ✅ | ✅ | 🟡 (model-dependent) |
| System prompt | ✅ | ✅ | 🟡 |
| max_tokens | ✅ | ✅ | 🟡 |
| Temperature | — | — | — |
| JSON mode | — | — | — |
| Vision input | ❌ | ❌ | ❌ |
| Audio input | ❌ | ❌ | ❌ |
| Token usage reporting | — | — | — |
| Per-request model id | — | — | — |
| Retry / back-off | — | — | — |

---

## 7. Test coverage (no new tests required)

The existing test suite in `chat.rs` covers:
- `ClaudeBackend` and `OpenAiChatBackend` construct and name correctly
- `build_openai_request_body` shape (system, user, tools, tool_choice)
- `parse_openai_response` for text-only, tool-use, API error, unknown tool
- Tool-use round-trip (call → result → next request `tool_call_id` propagation)
- `ChatModel::from_env_str` parser

A `LocalBackend` will add unit tests following the same pattern as the OpenAI
backend (inject a synthetic `mpsc::Receiver<ApiEvent>` without hitting the network).
