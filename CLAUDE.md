# CLAUDE.md - Phantom Terminal Build Instructions & Architecture

## Overview

Phantom is an AI-native terminal emulator built in Rust with GPU-accelerated rendering, semantic command understanding, and integrated AI agents. It's not a terminal with AI features bolted on—it's an AI system with a terminal built in.

## Quick Build & Run

```bash
# Clone the repository
git clone https://github.com/jdmiranda/phantom.git
cd phantom

# Build all binaries
cargo build --release

# Run with supervisor (recommended)
./run.sh
# OR manually:
cargo run --bin phantom-supervisor

# Run standalone (no supervisor monitoring)
cargo run --bin phantom
```

## System Requirements

- **Rust**: Edition 2024 (nightly required)
- **Graphics**: GPU with OpenGL 3.3+, Vulkan, Metal, or D3D12 support
- **OS**: macOS 10.15+, Linux (X11/Wayland), Windows 10+
- **Memory**: 4GB+ recommended

## Dependencies

Core dependencies are defined in the workspace `Cargo.toml`:

```toml
# GPU rendering
wgpu = "25"                    # Cross-platform GPU API
winit = "0.30"                 # Window management
bytemuck = "1"                 # Zero-copy type casting

# Text rendering  
cosmic-text = "0.12"           # Font shaping & layout

# Terminal emulation
alacritty_terminal = "0.26"    # VT100/xterm emulation

# Layout engine
taffy = "0.7"                  # CSS Flexbox layout

# Async runtime
tokio = { version = "1", features = ["full"] }
```

## Architecture Overview

Phantom is structured as a Rust workspace with **32 crates**, all wired into the top-level `Cargo.toml` workspace.

Legend: ✅ complete/active · 🔧 skeletal/stubbed · 🚧 Phase 2+ (open issues noted)

### Core System
- **phantom** ✅ — Main binary: winit event loop, GPU init, panic recovery, supervisor socket handshake
- **phantom-supervisor** ✅ — Erlang/OTP-style process monitor: heartbeat watch, auto-restart with back-off ([#53](https://github.com/jdmiranda/phantom/issues/53))
- **phantom-app** ✅ — Application lifecycle: pane management, boot sequence, agent coordination, inspector UI ([#97](https://github.com/jdmiranda/phantom/issues/97), [#44](https://github.com/jdmiranda/phantom/issues/44))
- **phantom-protocol** ✅ — Unix-socket IPC wire protocol between supervisor and app (heartbeat, events, commands)

### Rendering Stack
- **phantom-renderer** ✅ — wgpu GPU pipeline: text atlas, quad batcher, CRT post-fx, screenshot, video ([#82](https://github.com/jdmiranda/phantom/issues/82), [#83](https://github.com/jdmiranda/phantom/issues/83), [#84](https://github.com/jdmiranda/phantom/issues/84))
- **phantom-ui** ✅ — Design tokens, themes, Taffy layout, widget primitives, keybinds ([#20](https://github.com/jdmiranda/phantom/issues/20)–[#27](https://github.com/jdmiranda/phantom/issues/27), [#29](https://github.com/jdmiranda/phantom/issues/29)–[#31](https://github.com/jdmiranda/phantom/issues/31))
- **phantom-scene** ✅ — Retained scene graph with dirty-bit tracking, clock/cadence, frame delta clamp

### Terminal Emulation
- **phantom-terminal** ✅ — PTY process management, alacritty_terminal VT100 wrapper, input/output routing, Kitty protocol
- **phantom-semantic** 🔧 — Command classification and output parsing (git, cargo, etc.); stub integration pending ([#74](https://github.com/jdmiranda/phantom/issues/74), [#94](https://github.com/jdmiranda/phantom/issues/94))

### AI & Intelligence
- **phantom-agents** ✅ — Agent lifecycle, tool execution, Claude API chat, 12-variant `AgentRole` enum (Conversational/Watcher/Capturer/Transcriber/Reflector/Indexer/Actor/Composer/Fixer/Defender/Dispatcher/Cartographer), role-based tool whitelisting at the `dispatch/mod.rs` capability gate, `complete_task` lifecycle tool + `AgentSpawnOpts::with_requires_complete_task` builder, `try_auto_approve_with_audit` audit-traced fast-path, permission model, taint levels ([#60](https://github.com/jdmiranda/phantom/issues/60), [#87](https://github.com/jdmiranda/phantom/issues/87), [#93](https://github.com/jdmiranda/phantom/issues/93)–[#96](https://github.com/jdmiranda/phantom/issues/96), [#103](https://github.com/jdmiranda/phantom/issues/103)–[#105](https://github.com/jdmiranda/phantom/issues/105), [#646](https://github.com/jdmiranda/phantom/issues/646), [#648](https://github.com/jdmiranda/phantom/issues/648))
- **phantom-brain** ✅ — Ambient OODA loop on a dedicated thread: event scoring, utility AI, action dispatch, autonomous reconciler, `TaskLedger::try_dispatch` guarded mutator with `DispatchBlocked` enum, `StepFailureCause` + `QuarantinePolicy` cascade semantics on `PlanStep`. Self-improvement reconciler (`crates/phantom-brain/src/self_improvement.rs`): `GoalSource` trait + `GhIssueGoalSource` + `GhCiFailureGoalSource` auto-discover candidate work from `jdmiranda/phantom`, `score_candidate` weighted-sum scorer, `HardExclusions` keyword filter, `TrustBudget` with 4 autonomy bands (SuggestionOnly / Conservative / Standard / Aggressive), `RateLimiter` per-hour / per-day / cooldown windows, `AuditEntry` JSONL persistence, `AiAction::EnqueueLoopMessage` forwarded to the loop overseer ([#32](https://github.com/jdmiranda/phantom/issues/32), [#36](https://github.com/jdmiranda/phantom/issues/36)–[#40](https://github.com/jdmiranda/phantom/issues/40), [#45](https://github.com/jdmiranda/phantom/issues/45)–[#47](https://github.com/jdmiranda/phantom/issues/47), [#61](https://github.com/jdmiranda/phantom/issues/61), [#98](https://github.com/jdmiranda/phantom/issues/98)–[#99](https://github.com/jdmiranda/phantom/issues/99), [#647](https://github.com/jdmiranda/phantom/issues/647), [#649](https://github.com/jdmiranda/phantom/issues/649), [#660](https://github.com/jdmiranda/phantom/issues/660))
- **phantom-loop** ✅ — Loop overseer: durable repo-scoped `LoopRunner` async FSM pulling typed input from a pluggable `LoopSource` trait (`CronSource` for agentless poll loops, `LoopMessageQueueSource` for cross-loop fan-out, `GhIssueQueueSource` + `GhPrReviewQueueSource` for GitHub-backed inputs). Validates each agent's `complete_task` payload against the per-loop `ExitSchema`. Routes typed `LoopMessage`s through `LoopQueueRegistry`. `SubstrateAgentDispatcher` pushes `SpawnSubagentRequest`s onto the shared spawn queue with `requires_complete_task=true`. (PR #670 in flight adds a headless `SubstrateDriver` + `SubstrateBackend` trait — `ChatBackedSubstrateBackend` for production, `MockSubstrateBackend` for tests — so `phantom loop run` drains the queue without the GUI App.) CLI entry-point `phantom loop run --repo <path> --loops <names>` runs `check_gh_binary` / `check_gh_auth` / `check_mcp_collisions` preflight, acquires a `RunLock` at `<repo>/.phantom/loops/.runlock`, and registers each runner in `LoopRegistry` ([#650](https://github.com/jdmiranda/phantom/issues/650), [#665](https://github.com/jdmiranda/phantom/issues/665))
- **phantom-nlp** 🔧 — Natural-language command interpreter; LLM call routing is a stub ([#55](https://github.com/jdmiranda/phantom/issues/55))
- **phantom-context** ✅ — Project/git/environment detection and context assembly for agent prompts
- **phantom-memory** 🔧 — Per-project knowledge store with event log and memory blocks; schema and event log pending ([#28](https://github.com/jdmiranda/phantom/issues/28), [#33](https://github.com/jdmiranda/phantom/issues/33), [#62](https://github.com/jdmiranda/phantom/issues/62), [#78](https://github.com/jdmiranda/phantom/issues/78))
- **phantom-dag** 🔧 — Code dependency graph: DAG extraction and `.planning/dag.json` schema for agent navigation
- **phantom-recall** 🔧 — Intent-anchored retrieval API: query rewriting, score fusion, ANN routing; backend wiring pending ([#72](https://github.com/jdmiranda/phantom/issues/72))

### Persistence & History
- **phantom-history** 🔧 — Structured JSONL command history store; read/write and agent output capture pending ([#75](https://github.com/jdmiranda/phantom/issues/75))
- **phantom-session** 🔧 — Session save/restore; agent and goal/task state restore pending ([#76](https://github.com/jdmiranda/phantom/issues/76), [#77](https://github.com/jdmiranda/phantom/issues/77))

### Extensibility
- **phantom-plugins** 🔧 — Plugin lifecycle (manifest, host, registry, marketplace); WASM host is a mock, real wasmtime pending ([#48](https://github.com/jdmiranda/phantom/issues/48))
- **phantom-mcp** 🔧 — Model Context Protocol server (exposes Phantom to external AI) and client (consumes external tools); client impl and registry pending ([#52](https://github.com/jdmiranda/phantom/issues/52), [#54](https://github.com/jdmiranda/phantom/issues/54))
- **phantom-adapter** ✅ — `AppAdapter` trait, app registry, pub/sub event bus, spatial layout negotiation; the "everything is an app" abstraction layer ([#17](https://github.com/jdmiranda/phantom/issues/17))
- **phantom-skill-host** 🔧 — Runtime dylib loader and hot-module host for Phantom skill crates; Phase 1 dylib loading ([#382](https://github.com/jdmiranda/phantom/issues/382))

### Capture Pipeline (Phase 2.G)
- **phantom-vision** 🔧 — Perceptual-hash dedup (dHash + SAD gate) for frame deduplication; GPT-4V analysis pipeline pending ([#70](https://github.com/jdmiranda/phantom/issues/70), [#71](https://github.com/jdmiranda/phantom/issues/71), [#79](https://github.com/jdmiranda/phantom/issues/79))
- **phantom-bundles** 🔧 — Schema-only types for capture bundles (frames, audio, transcript); serialization and capture pipeline integration pending ([#80](https://github.com/jdmiranda/phantom/issues/80), [#81](https://github.com/jdmiranda/phantom/issues/81), [#91](https://github.com/jdmiranda/phantom/issues/91))
- **phantom-bundle-store** 🔧 — Unified persistence: SQLite (encrypted via SQLCipher) + LanceDB vectors with two-phase writes; recovery path tests pending ([#10](https://github.com/jdmiranda/phantom/issues/10), [#88](https://github.com/jdmiranda/phantom/issues/88))
- **phantom-embeddings** 🔧 — Multi-modal embedding trait + OpenAI backend + mock; persistent storage and vector query pending ([#72](https://github.com/jdmiranda/phantom/issues/72), [#73](https://github.com/jdmiranda/phantom/issues/73))

### Voice & Speech
- **phantom-stt** 🔧 — Speech-to-text backend abstraction (Whisper/Deepgram traits + mock); streaming Whisper and OpenAI integration pending ([#56](https://github.com/jdmiranda/phantom/issues/56), [#68](https://github.com/jdmiranda/phantom/issues/68))
- **phantom-voice** 🔧 — Text-to-speech backend abstraction (ElevenLabs/Piper/system TTS traits + mock); real TTS pending ([#69](https://github.com/jdmiranda/phantom/issues/69))

### Federation & Networking (Phase 9)
- **phantom-net** 🔧 — Identity bootstrap, relay handshake, opaque message envelope, and heartbeat keepalive for Phantom federation ([#5](https://github.com/jdmiranda/phantom/issues/5))
- **phantom-relay** 🔧 — Stateless WebSocket message broker routing opaque envelopes by `PeerId` ([#4](https://github.com/jdmiranda/phantom/issues/4))
- **phantom-hub** 🔧 — Railway-hosted MCP fleet control hub: connection broker, auth, and JSON-RPC router ([#394](https://github.com/jdmiranda/phantom/issues/394))

## Key Architectural Decisions

### Two-Process Model
- **Supervisor Process**: Lightweight monitor that never dies
- **Main Process**: The full terminal application
- **Communication**: Unix domain socket with heartbeat protocol
- **Benefit**: System always responsive, survives GPU hangs/crashes

### GPU-First Rendering
- **wgpu**: Cross-platform GPU API (Vulkan/Metal/D3D12/WebGPU)
- **Custom Shaders**: WGSL post-processing for CRT effects
- **Text Atlas**: Efficient glyph caching with etagere bin-packing
- **Mixed Content**: Terminal text + rich widgets + images

### Semantic Understanding
Every command output is parsed into structured data:
```bash
$ git status
# Becomes:
{
  command: "git status",
  type: "git.status", 
  branch: "main",
  modified: ["src/main.rs"],
  staged: [],
  context: { project: "phantom", language: "rust" }
}
```

### AI Agent System
- **Sandboxed Execution**: Agents run in isolated shell sessions
- **Tool Framework**: 7 core tools (ReadFile, WriteFile, RunCommand, etc.)
- **Permission Model**: Granular access control
- **Visual Integration**: Agents render in dedicated panes

### Harness-Level Workflow Control

All four properties of harness control are now in place on `main`. The 5 audit gaps tracked under issue #650 are closed.

1. **Per-role tool whitelists** — `crates/phantom-agents/src/dispatch/mod.rs` routes every tool-use through `capability::check_capability(ctx.role, tool.class())` before the handler runs. Denials return a canonical `"capability denied: <Class> not in <Role> manifest"` `ToolResult` so the model self-corrects. (Gap closed: PR #655 made `event_log` non-optional at the dispatch boundary so the gate cannot be silently bypassed.)
2. **External state machine** — `phantom-brain::TaskLedger` with the `try_dispatch(idx) -> Result<&PlanStep, DispatchBlocked>` guarded mutator and the 9-state `AgentStatus` FSM (Queued, Planning, AwaitingApproval, Working, WaitingForTool, Paused, Done, Failed, Flatline). (Gaps closed: PR #654 — guarded dispatch with `DispatchBlocked`; PR #657 — `StepFailureCause` + `QuarantinePolicy` typed quarantine recovery.)
3. **Structured exit** — `complete_task` lifecycle tool emitted by `phantom_agents::tools::lifecycle_tools()` when an agent is spawned with `AgentSpawnOpts::with_requires_complete_task(true)`. Result payload is a typed `Option<serde_json::Value>` on `Event::AgentTaskComplete`, validated against the per-loop `ExitSchema` from `phantom-loop`. A 3-strike `validation_failure_count` on `AgentPane` flatlines the pane after three consecutive schema-invalid `complete_task` calls; the legacy "PARTIAL" stringly-typed exit is gone. (Gaps closed: PR #652 — spike delivery path; PR #656 — production tool-list wiring + 3-strike flatline.)
4. **Typed inter-agent messaging** — `phantom-protocol::Event` bus with 21 typed variants (and `EventTopic` routing) plus the new `Event::FastPathTaken { agent_id, kind: FastPathKind, reason }` envelope from PR #653. `phantom-loop::LoopMessage` adds typed inter-loop routing through `LoopQueueRegistry`.

### Self-Improving Phantom

The four properties above sum to a closed feedback loop: phantom-on-phantom. The brain auto-discovers work in its own repository, scores it for utility, and forwards the highest-scoring candidates to the loop overseer for autonomous implementation — no human prompting required. Phase 1 (harness control) is complete; the brain self-improvement layer landed in PR #669; the substrate driver wires the loop CLI to real agent spawning in PR #670; the brain↔queue bridge (`LoopQueueActionHandler` + headless brain boot in `phantom loop run`) is the final hookup.

End-to-end flow once the bridge lands:

```
phantom-brain GoalSource polls jdmiranda/phantom (GhIssueGoalSource + GhCiFailureGoalSource)
    -> phantom-brain self_improvement::score_candidate weighted-sum scorer
    -> HardExclusions keyword filter + TrustBand threshold + RateLimiter window
    -> AiAction::EnqueueLoopMessage emitted onto the brain action channel
    -> LoopQueueActionHandler.enqueue_loop_message  (in-flight, brain<->queue bridge)
    -> LoopQueueRegistry.push(implementer-queue, LoopMessage{...})
    -> LoopRunner pulls via LoopMessageQueueSource
    -> SubstrateAgentDispatcher builds AgentSpawnOpts with requires_complete_task=true
    -> SubstrateDriver.tick drains SpawnSubagentQueue, runs ChatBackedSubstrateBackend
    -> Claude API stream loops through TextDelta / ToolUse until complete_task tool call
    -> Event::AgentTaskComplete fulfils the dispatcher's oneshot
    -> LoopRunner validates the payload against the loop's ExitSchema
    -> on_complete LoopEffects fire (typically EnqueueTo a downstream review-queue)
```

Source paths:
- Brain scorer + state machine: `crates/phantom-brain/src/self_improvement.rs`
- Goal sources: `crates/phantom-brain/src/goal_source/{mod,gh_issues,gh_ci}.rs`
- Dispatch action variant: `crates/phantom-brain/src/events.rs` (`AiAction::EnqueueLoopMessage`)
- Loop FSM: `crates/phantom-loop/src/runner/{fsm,source,dispatcher}.rs`
- Substrate dispatcher: `crates/phantom-loop/src/dispatcher/substrate.rs`
- Substrate driver (PR #670 branch): `crates/phantom-loop/src/dispatcher/driver.rs`
- CLI entry-point: `crates/phantom/src/loop_cli.rs`
- Design doc: `docs/design/brain-self-improvement.md`

### Operating Phantom Self-Improvement

```bash
phantom loop run --repo <path> --loops pr_finder_review,pr_finder_impl,reviewer,implementer
```

Preflight gates (`crates/phantom/src/loop_cli.rs::run_command`, calling `phantom_loop::preflight`):
- `check_gh_binary` — `gh` binary on PATH and executable.
- `check_gh_auth` — `gh auth status` reports a logged-in account.
- `check_mcp_collisions` — MCP tool names do not collide with the reserved `complete_task` / `abort_task` lifecycle names.
- `RunLock::acquire(&repo)` — exclusive runlock at `<repo>/.phantom/loops/.runlock`. Released on Ctrl-C or process exit via `Drop`.

Loop spec discovery: `<repo>/.phantom/loops/*.toml`. The shipped specs are `pr_finder_review.toml`, `pr_finder_impl.toml`, `reviewer.toml`, `implementer.toml` (see `.phantom/loops/` at repo root).

Brain self-improvement config (`SelfImprovementConfig` in `crates/phantom-brain/src/self_improvement.rs`):
- `enabled: bool` — default OFF per design doc §5.1; operator must opt in.
- `queue_name: "implementer-queue"` — the brain's default destination.
- `audit_log_path: Option<PathBuf>` — when set, every decision (enqueue or skip) appends one `AuditEntry` JSONL envelope.
- `per_hour: u32`, `per_day: u32`, `cooldown: Duration` — `RateLimiter` ceilings.
- `max_in_flight: u32` — concurrent in-flight cap.

Trust bands (`TrustBand` in `self_improvement.rs`, ramps automatically on enqueue success / failure):
- **SuggestionOnly** (budget 0) — no auto-enqueue regardless of score.
- **Conservative** (budget 1-3) — raised threshold 0.85, halved per-hour ceiling.
- **Standard** (budget 4-9) — design-doc defaults.
- **Aggressive** (budget 10-20) — lowered threshold 0.65, doubled per-hour ceiling.

A `critical` / `regression` / `blocker` label on a candidate floors the score at `CRITICAL_LABEL_FLOOR = 0.85` per design doc §7.1.

The audit log JSONL file (path passed via `SelfImprovementConfig::audit_log_path`) captures every decision with `external_id`, `source`, `score`, `score_breakdown`, `decision`, and `reason`. Tail it to inspect autonomous activity.

## Build Process Details

### Development Build
```bash
# Build with debug info
cargo build

# Build specific crate
cargo build -p phantom-renderer

# Run tests
cargo test

# Run with logging
RUST_LOG=debug cargo run --bin phantom
```

### Release Build
```bash
# Full optimization
cargo build --release

# Install system-wide
cargo install --path .
```

### Platform-Specific Notes

#### macOS
- Uses Metal backend by default
- Requires Xcode command line tools
- May need to allow app in Security preferences

#### Linux
- Vulkan backend preferred, OpenGL fallback
- Required packages: `libfontconfig-dev`, `libxkbcommon-dev`
- Wayland support via winit

#### Windows  
- D3D12 backend with DXGI
- Requires Windows SDK for development
- MSVC toolchain recommended

## Configuration

Default config location: `~/.config/phantom/config.toml`

```toml
[renderer]
theme = "phosphor"
font_size = 14.0
scanlines = 0.8
bloom = 0.6
curvature = 0.1

[terminal]
shell = "/bin/zsh"
working_directory = "~"

[agents]
claude_api_key = "sk-..."
max_concurrent = 3
timeout_seconds = 30
```

## Running & Usage

### Basic Operation
```bash
# Launch with all features
cargo run --bin phantom-supervisor

# Available themes
cargo run -- --theme amber
cargo run -- --theme ice  
cargo run -- --theme blood
cargo run -- --theme vapor

# Debug mode
cargo run -- --debug

# Disable boot sequence
cargo run -- --no-boot
```

### Key Bindings
- **`Cmd+D`**: Split pane horizontal
- **`Cmd+Shift+D`**: Split pane vertical  
- **`Cmd+[`/`Cmd+]`**: Focus previous/next pane
- **`Cmd+W`**: Close focused pane
- **`` ` ``**: Command mode (system commands)

### Agent System
```bash
# Spawn AI agent
` agent "fix the failing tests"

# List running agents  
` agents

# Natural language commands
build the project
what changed today
deploy staging
```

## Development Workflow

### Adding New Features
1. Create new crate in `crates/` if needed
2. Add to workspace `Cargo.toml` members list
3. Implement using established patterns
4. Add tests and documentation
5. Update this guide

### Shader Development
Shaders are in `shaders/` directory:
- `crt.wgsl`: Main CRT post-processing
- `text.wgsl`: Text rendering pipeline
- Live-reloadable in debug mode

### Plugin Development
```rust
// plugins/example/src/lib.rs
use phantom_plugins::prelude::*;

#[phantom_plugin]
pub struct ExamplePlugin;

impl Plugin for ExamplePlugin {
    fn on_command(&self, cmd: &str, output: &str) -> PluginResult {
        // React to terminal commands
        Ok(())
    }
}
```

## Troubleshooting

### Build Issues
```bash
# Clean rebuild
cargo clean && cargo build

# Update dependencies
cargo update

# Check Rust version
rustc --version  # Should be 1.70+
```

### Runtime Issues
```bash
# Enable debug logging
RUST_LOG=phantom=debug cargo run

# Check GPU capabilities
cargo run -- --gpu-info

# Disable CRT effects
cargo run -- --theme plain
```

### Common Problems
- **GPU driver outdated**: Update graphics drivers
- **Font not found**: Install system fonts or specify font path
- **Permission denied**: Check file/directory permissions
- **Agent not responding**: Verify Claude API key in config

## Performance Tuning

### GPU Performance
- Use dedicated GPU if available
- Increase glyph atlas size for large terminals
- Reduce post-processing effects on older hardware

### Memory Usage
- Limit scrollback buffer size
- Enable agent cleanup policies
- Monitor plugin memory usage

## Testing

```bash
# Run all tests
cargo test

# Integration tests only
cargo test --test integration

# Specific crate
cargo test -p phantom-renderer

# With output
cargo test -- --nocapture
```

## Contributing

1. Read the architecture docs in `docs/`
2. Follow Rust style guidelines
3. Add tests for new features
4. Update documentation
5. Submit PR with clear description

## License

MIT License - See LICENSE file for details.

---

Built with ❤️ by Jeremy Miranda and Claude Code.

## Orchestration Rules

Rules that govern how autonomous agents coordinate within the Phantom multi-agent pipeline.

1. **Post-merge workspace check**: After every PR merge, an agent must run `cargo build --workspace` and `cargo test --workspace --no-run` on main before spawning new implementation agents that touch the same crates. Block spawning only when the post-merge run introduces NEW failures relative to the pre-merge baseline; pre-existing red is not a gate.

2. **Pre-PR self-check**: Every implementation agent must run `./scripts/pre-pr-check.sh <crate-name>` before calling `gh pr create`. A PR must not be opened if the script introduces NEW failures relative to the baseline captured at branch-off; pre-existing failures inherited from main do not block the PR.

3. **Worktree cleanup**: After every 10 merges, a cleanup agent must be spawned to prune merged worktrees.

4. **Long-running agent timeout**: Any implementation agent running for more than 30 minutes without opening a PR should be checked on by sending a status message.

5. **Hot-file tracking**: Before spawning an agent on a crate, check `gh pr list -R jdmiranda/phantom --state open` to confirm no other open PR already modifies the same files.

6. **Branch hygiene**: All agent worktrees MUST branch from the most recent clean baseline — preferring a `v*.baseline` tag, falling back to `origin/main` when no such tag exists. Spawn command: `git checkout $(git describe --tags --match 'v*.baseline' --abbrev=0 2>/dev/null || git rev-parse --short origin/main) -b <branch-name>`. Never: `git checkout main -b <branch>` against local main without first fetching.

7. **Spec gate**: Before spawning any executor agent, a spec agent must first produce SPEC.md, PLAN.md, and TASKS.md in the worktree. The executor receives only TASKS.md — not the raw issue. Any issue scoring ≥7/10 on the MAST rubric may skip the spec agent; lower-scoring issues must pass through it.

8. **Prompt phrasing**: Agent prompts contain no question-mark sentences. See [docs/standards/agent-prompts.md](docs/standards/agent-prompts.md) — questions produce commentary instead of action.

## graphify

This project has a graphify knowledge graph at graphify-out/.

Rules:
- Before answering architecture or codebase questions, read graphify-out/GRAPH_REPORT.md for god nodes and community structure
- If graphify-out/wiki/index.md exists, navigate it instead of reading raw files
- After modifying code files in this session, run `python3 scripts/graphify_rebuild.py` to keep the graph current
