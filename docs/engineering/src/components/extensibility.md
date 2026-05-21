# Extensibility + Capture

[← back to components index](README.md)

> Adding capabilities + frame capture + vectors.

## Status

<span class="chip ok">shipping</span> · `phantom-mcp` (production
listener + MCP discovery)  ·
<span class="chip warn">stubbed</span> · `phantom-plugins`,
`phantom-skill-host`, `phantom-vision`, `phantom-bundles`,
`phantom-bundle-store`, `phantom-embeddings`

## What it does

Two adjacent concerns:

1. **Extensibility** — how external (3P) capabilities plug into Phantom.
   MCP servers ship today; WASM plugin sandbox is the Phase 2+ pattern
   (ADR-002).
2. **Capture** — the screen/audio/transcript capture pipeline that feeds
   the brain's perceptual surface (vision, embeddings, bundles).

## Crates

### `phantom-mcp` <span class="chip ok">shipping</span>

The MCP listener + client. ~5k LOC.

- `Listener` — Unix-socket server at `/tmp/phantom-mcp-{pid}.sock` (or
  `$PHANTOM_MCP_SOCK`). External clients (Claude Code, `nc -U`,
  remote AI systems) dial in and dispatch `AppCommand`s.
- `spawn_listener(path, cmd_tx)` — bootstrap. Called from
  `App::with_config_scaled` Step 8 of Flow 1.
- `spawn_hub(hub_url, cmd_tx)` — outbound registration to
  `phantom-hub` for remote Phantom-on-Phantom federation.
- `McpToolRegistry` — external tool surface aggregated across all
  connected MCP servers. Wired into `AgentPane::set_mcp_registry` so
  agents see the union of built-in + external tools.
- `mcp_discovery` — async task that polls `$PHANTOM_MCP_SERVERS` URLs
  on startup; populates the registry. Gated by the 500ms barrier in
  `spawn_agent_pane_with_opts` so cold-launched agents see the full
  tool list.

### `phantom-skill-host` <span class="chip warn">stubbed</span>

Runtime dylib loader + hot-module host for "phantom skill" crates. Phase
1 dylib loading exists (issue #382); the production sandboxing isn't
yet wired.

- `LlmHost` — the NLP backend abstraction. `ClaudeLlmBackend` and
  `OllamaLlmBackend` ship; routes through `phantom-nlp` for intent
  extraction.

### `phantom-plugins` <span class="chip warn">stubbed</span>

Plugin lifecycle (manifest, host, registry, marketplace). WASM host is
a mock — real `wasmtime` integration pending (issue #48).

- `PluginRegistry` — discovers + loads plugins from
  `~/.local/share/phantom/plugins/`.
- `Plugin` trait — `on_command(cmd, output)`, `on_event(topic, data)`.

### `phantom-vision` <span class="chip warn">stubbed</span>

Perceptual-hash dedup (dHash + SAD gate) for frame deduplication.
GPT-4V analysis pipeline pending (issues #70, #71, #79).

- `DedupGate` — drops near-duplicate frames before they hit the
  embedding pipeline.
- `Frame` — typed capture frame with timestamp + hash.

### `phantom-bundles` <span class="chip warn">stubbed</span>

Schema-only types for capture bundles (frames, audio, transcript).
Serialization + capture pipeline integration pending (issues #80, #81,
#91).

- `CaptureBundle` — typed envelope.
- `BundleMeta` — projection metadata.

### `phantom-bundle-store` <span class="chip warn">stubbed</span>

Unified persistence: SQLite (encrypted via SQLCipher) + LanceDB vectors
with two-phase writes. Recovery path tests pending (issues #10, #88).

- `BundleStore::open(path)` — opens
  `~/.config/phantom/bundles/` (encrypted SQLite via
  `rusqlite` + `bundled-sqlcipher-vendored-openssl`).
- LanceDB integration is wired but vector queries are stubbed.

### `phantom-embeddings` <span class="chip warn">stubbed</span>

Multi-modal embedding trait + OpenAI backend + mock. Persistent storage
+ vector query pending (issues #72, #73).

- `Embedder` trait — `embed(text) -> Vec<f32>`.
- `OpenAiEmbedder` — `text-embedding-3-large` backend.
- `MockEmbedder` — for tests.

## Owns

- MCP listener socket + dispatch route table
- MCP tool registry (live, drained on agent spawn)
- Hub registration state (when `$PHANTOM_HUB_URL` is set)
- Plugin registry (when populated)
- Capture pipeline schema
- Embeddings backend abstraction

## Reads from

| Source | What |
|---|---|
| MCP server URLs (`$PHANTOM_MCP_SERVERS`) | external tool surfaces |
| `$PHANTOM_HUB_URL` | hub endpoint for federation |
| Plugin directory | WASM plugin manifests |
| Capture frames (when wired) | screen captures from `phantom-renderer` |
| OpenAI API | embedding vectors |

## Writes to / publishes

| Target | What |
|---|---|
| `AppCommand` channel | external commands dispatched into the app loop |
| `MessageBlock` rows in agent panes | MCP tool call results |
| Bundle store SQLite | capture bundles (when wired) |
| Embedding store | vector rows (when wired) |

## Decisions honoured

- [ADR-002 · WASM app adapter](../decisions/002-wasm-adapter.md) — the
  long-term sandbox plan; phantom-plugins + phantom-skill-host implement it.

## Open gaps

(no flow-surfaced gaps yet — these crates' stubs are known
incomplete implementations rather than flow gaps)

## Source files

| Concept | File |
|---|---|
| MCP listener | [`crates/phantom-mcp/src/listener.rs`](../../../../crates/phantom-mcp/src/listener.rs) |
| MCP server impl | [`crates/phantom-mcp/src/server.rs`](../../../../crates/phantom-mcp/src/server.rs) |
| MCP discovery | [`crates/phantom-app/src/mcp_discovery.rs`](../../../../crates/phantom-app/src/mcp_discovery.rs) |
| Skill host (LLM) | [`crates/phantom-skill-host/src/llm_host.rs`](../../../../crates/phantom-skill-host/src/llm_host.rs) |
| Plugins registry | [`crates/phantom-plugins/src/lib.rs`](../../../../crates/phantom-plugins/src/lib.rs) |
| Vision dedup | [`crates/phantom-vision/src/lib.rs`](../../../../crates/phantom-vision/src/lib.rs) |
| Bundles | [`crates/phantom-bundles/src/lib.rs`](../../../../crates/phantom-bundles/src/lib.rs) |
| Bundle store | [`crates/phantom-bundle-store/src/lib.rs`](../../../../crates/phantom-bundle-store/src/lib.rs) |
| Embeddings | [`crates/phantom-embeddings/src/lib.rs`](../../../../crates/phantom-embeddings/src/lib.rs) |
