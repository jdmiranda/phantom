# PHANTOM

```
 ██████╗ ██╗  ██╗ █████╗ ███╗   ██╗████████╗ ██████╗ ███╗   ███╗
 ██╔══██╗██║  ██║██╔══██╗████╗  ██║╚══██╔══╝██╔═══██╗████╗ ████║
 ██████╔╝███████║███████║██╔██╗ ██║   ██║   ██║   ██║██╔████╔██║
 ██╔═══╝ ██╔══██║██╔══██║██║╚██╗██║   ██║   ██║   ██║██║╚██╔╝██║
 ██║     ██║  ██║██║  ██║██║ ╚████║   ██║   ╚██████╔╝██║ ╚═╝ ██║
 ╚═╝     ╚═╝  ╚═╝╚═╝  ╚═╝╚═╝  ╚═══╝   ╚═╝    ╚═════╝ ╚═╝     ╚═╝
```

**An AI-native terminal emulator.** Intelligence isn't a feature. It's the substrate.

```
18 crates | 27,220 lines of Rust | 629 tests | MIT licensed
```

---

## What Is Phantom?

Phantom is not a terminal with AI bolted on. It's an AI system with a terminal built in.

Every command you run is semantically parsed. Every error is detected and analyzed. AI agents live in their own panes and work alongside you. The terminal remembers your projects across sessions. And it renders everything through a GPU-accelerated CRT shader pipeline that makes it look like the future imagined by the past.

## Quick Start

```bash
# Clone and build
git clone https://github.com/jdmiranda/phantom.git
cd phantom
cargo build --release

# Run standalone
cargo run --bin phantom

# Run with supervisor (heartbeat monitoring, auto-restart, ! commands)
cargo run --bin phantom-supervisor

# See all options
cargo run --bin phantom -- --help
```

## Features

### GPU-Rendered Terminal
- **wgpu** backend (Metal/Vulkan/DX12/WebGPU)
- Full VT100/xterm terminal emulation via `alacritty_terminal`
- GPU-accelerated text rendering with `cosmic-text` shaping + glyph atlas
- Instanced quad rendering for cell backgrounds, cursors, selections
- Retina/HiDPI display scaling

### CRT Post-Processing Shaders
- Scanlines, phosphor bloom, chromatic aberration
- Barrel distortion (CRT screen curvature)
- Vignette, film grain
- All effects configurable per-theme and live-tweakable via debug HUD
- 5 built-in themes: phosphor (green), amber, ice (cyan), blood (red), vapor (retrowave)

### Cinematic Boot Sequence
- ASCII noise fills the screen, clears from center outward
- Horizontal scan beam sweeps top to bottom
- Laughing skull glitch-reveals above the PHANTOM logo
- Logo characters scramble through random glyphs before locking in
- Animated progress bars for system checks
- Pauses at "SYSTEM READY" until keypress

### Tmux-Style Pane Splitting
- `Cmd+D` split horizontal, `Cmd+Shift+D` split vertical
- `Cmd+]`/`Cmd+[` cycle focus between panes
- `Cmd+W` close focused pane
- Each pane runs its own independent shell (PTY)
- Focused pane highlighted, unfocused dimmed
- Taffy flexbox layout engine

### Process Detach
- Detects when interactive programs enter alternate screen (vim, htop, ssh)
- Animated cyan pulsing border on detached panes
- Process name detection via `TIOCGPGRP` (macOS/Linux)
- Collapse animation when program exits

### Semantic Layer
- Parses command output into structured data (git status, cargo build errors, test results, HTTP responses, JSON)
- Error detection with file:line:col references across languages (Rust, Python, Node, Go, Java)
- Auto-detects JSON and tabular output for rich rendering
- Structured command history searchable across sessions

### AI Agent System
- Agents run in their own sandboxed panes with animated borders
- 7 sandboxed tools: ReadFile, WriteFile, RunCommand, SearchFiles, GitStatus, GitDiff, ListFiles
- Path traversal protection, 30s command timeout
- Permission-based access control (ReadFiles, WriteFiles, RunCommands, Network, GitAccess)
- Claude API integration with tool use (background thread, non-blocking)
- Agent CLI: `` ` agent "fix the failing tests" ``
- Error-to-agent pipeline: build fails -> Phantom offers to fix it
- Agent queue with capacity limits and automatic promotion

### AI Brain (Ambient Intelligence)
- Dedicated thread running event-driven OODA loop
- Utility AI scoring: every possible action scored 0.0-1.0
- Quiet baseline (0.5) — AI only acts when MORE useful than silence
- Chattiness dampener prevents annoying repeated suggestions
- Observes all terminal output, errors, git state, file changes
- Proactive: doesn't wait to be asked

### Project Context Engine
- Auto-detects project type (Rust, Node, Python, Go, Java, Ruby, Elixir, C++, C#, Swift)
- Package manager detection (npm/yarn/pnpm/bun/poetry/uv/cargo/etc)
- Framework detection (React, Next.js, Vue, Actix, Axum, Django, Flask, Rails, etc)
- Git info: branch, remote, dirty state, ahead/behind, last commit
- Command inference: "build" -> `cargo build` or `npm run build` based on project

### Persistent Memory
- Per-project key-value store across sessions
- Categories: ProjectConfig, Convention, Warning, Context, UserNote
- Sources: Auto-detected, Agent-written, User-set
- Atomic saves, search by text

### Session Save/Restore
- Saves pane states, working dirs, theme, git branch, activity
- "Welcome back. You were working on feature/auth in phantom."
- Cleanup of old sessions

### Natural Language Commands
- Type "build" instead of `cargo build`
- "what changed today" -> `git log --oneline -10`
- "fix it" -> spawns an AI agent
- "deploy staging" -> resolves or asks for clarification
- 7-stage NLP pipeline with 100+ known binary passthrough

### MCP Protocol
- **MCP Server**: Claude Code and other tools can drive Phantom
  - Tools: `phantom.run_command`, `phantom.read_output`, `phantom.screenshot`, `phantom.split_pane`, `phantom.get_context`, `phantom.get_memory`, `phantom.set_memory`
  - Resources: `phantom://terminal/state`, `phantom://project/context`, `phantom://history/recent`
- **MCP Client**: Phantom's agents can use external MCP servers

### Plugin System (WASM)
- Trait-based plugin runtime (wasmtime pluggable)
- TOML/JSON plugin manifests with declared permissions
- Hook system: react to commands, output, errors, timers
- 5 official plugins: git-enhanced, docker-dashboard, api-inspector, spotify-controls, github-notifications
- Marketplace with search, install, uninstall

### Supervisor Architecture
- Two-process model inspired by Erlang/OTP
- Heartbeat monitoring (10s timeout)
- Auto-restart with rate limiting (max 5 restarts/60s)
- Unix domain socket for inter-process commands
- `!` system commands always responsive

### System Overlay (Post-CRT)
- Command mode (`` ` ``): system commands rendered crisp, above CRT effects
- Debug shader HUD: live-tune all CRT parameters with arrow keys
- `plain` command: disable all CRT effects instantly
- Screenshot capture: GPU frame readback to PNG with metadata

### Scene Graph
- Retained-mode scene tree with arena storage
- Dirty flag propagation (TRANSFORM, CONTENT, CHILDREN, VISIBILITY)
- Only re-upload dirty subtrees to GPU
- Per-node render layer (Scene vs Overlay)
- Z-order sorting, visibility culling

### Configuration
- `~/.config/phantom/config.toml`
- CLI args: `--theme`, `--font-size`, `--scanlines`, `--bloom`, `--curvature`, `--no-boot`
- Live config: `` ` set curvature 0.1 `` — changes apply instantly
- Theme hot-swap: `` ` theme amber ``

## Architecture

```
phantom                  # main binary (winit event loop)
phantom-supervisor       # Erlang/OTP-style process monitor
phantom-app              # app orchestrator (owns all subsystems)
phantom-renderer         # GPU: wgpu context, glyph atlas, quads, grid, post-fx, images, screenshots
phantom-terminal         # PTY: alacritty_terminal wrapper, input encoding, output extraction, kitty protocol
phantom-ui               # UI: themes, layout (taffy), keybinds, widgets
phantom-semantic         # brain: command parser, error detection, highlighting
phantom-agents           # AI: agent runtime, tools, permissions, Claude API, CLI, suggestions, pane rendering
phantom-brain            # AI: ambient OODA loop, utility scoring, event-driven control
phantom-context          # awareness: project detection, framework, git, commands
phantom-memory           # persistence: per-project key-value memory
phantom-history          # persistence: structured command history (JSONL)
phantom-session          # persistence: session save/restore
phantom-nlp              # language: natural language command interpreter
phantom-plugins          # ecosystem: WASM plugin host, manifests, registry, marketplace, builtins
phantom-mcp              # protocol: MCP server + client (JSON-RPC 2.0)
phantom-protocol         # protocol: supervisor socket communication
phantom-scene            # performance: retained scene graph, dirty tracking
```

## Keybinds

| Key | Action |
|-----|--------|
| `` ` `` | Command mode (system commands) |
| `Cmd+D` | Split pane horizontal |
| `Cmd+Shift+D` | Split pane vertical |
| `Cmd+]` / `Cmd+[` | Focus next/prev pane |
| `Cmd+W` | Close focused pane |
| `Cmd+=` / `Cmd+-` | Zoom in/out |
| `Cmd+C` / `Cmd+V` | Copy/paste |
| `Cmd+T` | New tab |
| `Cmd+Q` | Quit |

## Command Mode

Press `` ` `` to enter command mode:

| Command | Effect |
|---------|--------|
| `set curvature 0.1` | Live-tweak shader params |
| `theme amber` | Hot-swap theme |
| `debug` | Toggle shader debug HUD |
| `plain` | Disable all CRT effects |
| `agent "fix tests"` | Spawn AI agent |
| `agents` | List running agents |
| `boot` | Replay boot sequence |
| `reload` | Re-read config from disk |
| `quit` | Exit |

## Research

- [ARD-001: Architecture Decision Record](docs/ARD-001-architecture-decisions.md) — all major technical decisions with rationale
- [Scene Graph Research](docs/research/scene-graph.md) — FrankenTUI, Bevy, dirty flag patterns
- [Supervisor Research](docs/research/supervisor-architecture.md) — Erlang/OTP, systemd watchdog
- [AI Control Loop Research](docs/research/ai-control-loop.md) — OODA, utility AI, ambient agents, Claude Code internals

## License

MIT

## Credits

Built by [Jeremy Miranda](https://github.com/jdmiranda) with [Claude Code](https://claude.ai/claude-code).
