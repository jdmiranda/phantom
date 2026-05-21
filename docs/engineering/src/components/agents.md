# Agents

[← back to components index](README.md)

> LLM-driven actors + project context + NLP intent.

## Status

<span class="chip ok">shipping</span> · `phantom-agents`,
`phantom-context` · <span class="chip warn">stubbed</span> · `phantom-nlp`

## What it does

The agent ring is the layer of LLM-driven actors. Owns the chat protocol
to Claude + OpenAI-compat backends, the per-role tool dispatch surface,
the capability gate, and the lifecycle protocol (`complete_task`,
`abort_task`, validation_failure flatline).

## Crates

### `phantom-agents` <span class="chip ok">shipping</span>

The big one. ~25k LOC. Owns:

- `AgentRole` — 10-variant role enum (Conversational, Watcher, Capturer,
  Transcriber, Reflector, Indexer, Actor, Composer, Fixer, Defender) +
  Cartographer / Dispatcher as recent additions. See
  [`crates/phantom-agents/src/role.rs`](../../../../crates/phantom-agents/src/role.rs).
- `CapabilityClass` — 5-variant class enum (Sense, Reflect, Compute,
  Act, Coordinate) used by the gate.
- `dispatch::dispatch_tool(ctx, tool)` — the entry point. Calls
  `check_capability`, runs `try_auto_approve_with_audit`, routes to
  the handler. See Flow 2's [walkthrough](../flows/02-agent-spawn.md).
- `dispatch::capability::check_capability(role, class)` — the gate.
- `AgentSpawnOpts` — the builder threaded through `spawn_agent_pane`.
  Supports `with_role`, `with_label`, `with_chat_model`,
  `with_requires_complete_task`.
- `ChatModel` enum — `Claude(model_id)` + `OpenAi(model_id)`.
- `complete_task` lifecycle tool — emitted by `tools::lifecycle_tools()`
  when `requires_complete_task=true`.
- `QuarantineRegistry` — typed quarantine state per agent
  (`Healthy` / `Probation { since_ms }` / `Quarantined { since_ms, reason }`).
- `TaskLedger` (lives in [phantom-brain](brain.md), but the typed
  enums `StepFailureCause`, `QuarantinePolicy`, `DispatchBlocked` cross
  the boundary).
- `AgentMessage` — typed message rows for an `AgentPane`'s history.

### `phantom-context` <span class="chip ok">shipping</span>

Project + git + environment detection.

- `ProjectContext::detect(path)` — sniffs `Cargo.toml`, `package.json`,
  `.git/`, etc. to determine the project name + type + language.
- `GitContext` — current branch, HEAD sha, uncommitted file count.
- `EnvContext` — relevant env vars (`ANTHROPIC_API_KEY` presence,
  `GITHUB_TOKEN`, etc.).
- Threaded into agent system prompts so the model "knows" what it's
  working on.

### `phantom-nlp` <span class="chip warn">stubbed</span>

Natural-language command interpreter. LLM call routing is a stub today.
Intent extraction skeleton + tests exist; the live model call is wired
to a mock. When complete:

- `NlpInterpreter::translate(text, context)` — returns an
  `AiAction` (spawn agent, run command, etc.).
- The interpreter is the bridge from "talk to the AI in English" to
  "phantom dispatches the right action."

## Owns

- The `AgentRole` + `CapabilityClass` matrix.
- The capability gate.
- The tool dispatch surface.
- The lifecycle protocol (`complete_task`).
- `QuarantineRegistry` + the quarantine state machine.
- The chat protocol to Claude + OpenAI-compat backends.
- The audit envelope format for capability denials + fast-path takes.

## Reads from

| Source | What |
|---|---|
| Claude API / OpenAI-compat backends | streamed completions |
| `phantom-context` | project + git context for system prompts |
| `phantom-mcp` (registry) | external MCP tool surface |
| `phantom-recall` (when wired) | retrieval-augmented context |

## Writes to / publishes

| Target | What |
|---|---|
| `agent.*` bus topics | `AgentSpawned`, `AgentProgress`, `AgentTaskComplete`, `AgentError`, `FastPathTaken` |
| `phantom-history` agent capture sidecar | tool calls + outputs |
| `phantom-app` `BlockedEventSink` | `EventKind::AgentBlocked` events after consecutive failures |
| `phantom-app` `pending_spawn_subagent` queue | child agent spawn requests |
| audit log | capability denials + fast-path takes |

## Decisions honoured

- [ADR-001 · Architecture decisions](../decisions/001-architecture.md) — the
  per-role tool whitelist, the capability gate at the dispatch boundary.
- [ADR-003 · App lifecycle + pub-sub](../decisions/003-pubsub.md) — typed
  `agent.*` event topics; the bus is the only inter-component
  communication channel.

## Open gaps

- [gap-capability-class-propagation](../gaps.md#gap-capability-class-propagation)
  — three independent `CapabilityClass` enums in the workspace.
- [gap-fast-path-audit-trail](../gaps.md#gap-fast-path-audit-trail) —
  fast-path events emit but Inspector has no dedicated view.

## Source files

| Concept | File |
|---|---|
| Tool dispatch entry | [`crates/phantom-agents/src/dispatch/mod.rs`](../../../../crates/phantom-agents/src/dispatch/mod.rs) |
| Capability gate | [`crates/phantom-agents/src/dispatch/capability.rs`](../../../../crates/phantom-agents/src/dispatch/capability.rs) |
| Role + class | [`crates/phantom-agents/src/role.rs`](../../../../crates/phantom-agents/src/role.rs) |
| Lifecycle tools (complete_task) | [`crates/phantom-agents/src/tools.rs`](../../../../crates/phantom-agents/src/tools.rs) |
| Quarantine registry | [`crates/phantom-agents/src/quarantine.rs`](../../../../crates/phantom-agents/src/quarantine.rs) |
| Project context | [`crates/phantom-context/src/lib.rs`](../../../../crates/phantom-context/src/lib.rs) |
| NLP interpreter (stub) | [`crates/phantom-nlp/src/lib.rs`](../../../../crates/phantom-nlp/src/lib.rs) |
