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

Phantom is structured as a Rust workspace with **23 active crates** (4 additional crates exist on disk and compile independently but are not yet wired into the workspace — see "Future / not yet in workspace" below).

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
- **phantom-agents** ✅ — Agent lifecycle, tool execution, Claude API chat, role system (Defender/Inspector), permission model, taint levels ([#60](https://github.com/jdmiranda/phantom/issues/60), [#87](https://github.com/jdmiranda/phantom/issues/87), [#93](https://github.com/jdmiranda/phantom/issues/93)–[#96](https://github.com/jdmiranda/phantom/issues/96), [#103](https://github.com/jdmiranda/phantom/issues/103)–[#105](https://github.com/jdmiranda/phantom/issues/105))
- **phantom-brain** ✅ — Ambient OODA loop on a dedicated thread: event scoring, utility AI, action dispatch, autonomous reconciler ([#32](https://github.com/jdmiranda/phantom/issues/32), [#36](https://github.com/jdmiranda/phantom/issues/36)–[#40](https://github.com/jdmiranda/phantom/issues/40), [#45](https://github.com/jdmiranda/phantom/issues/45)–[#47](https://github.com/jdmiranda/phantom/issues/47), [#61](https://github.com/jdmiranda/phantom/issues/61), [#98](https://github.com/jdmiranda/phantom/issues/98)–[#99](https://github.com/jdmiranda/phantom/issues/99))
- **phantom-nlp** 🔧 — Natural-language command interpreter; LLM call routing is a stub ([#55](https://github.com/jdmiranda/phantom/issues/55))
- **phantom-context** ✅ — Project/git/environment detection and context assembly for agent prompts
- **phantom-memory** 🔧 — Per-project knowledge store with event log and memory blocks; schema and event log pending ([#28](https://github.com/jdmiranda/phantom/issues/28), [#33](https://github.com/jdmiranda/phantom/issues/33), [#62](https://github.com/jdmiranda/phantom/issues/62), [#78](https://github.com/jdmiranda/phantom/issues/78))

### Persistence & History
- **phantom-history** 🔧 — Structured JSONL command history store; read/write and agent output capture pending ([#75](https://github.com/jdmiranda/phantom/issues/75))
- **phantom-session** 🔧 — Session save/restore; agent and goal/task state restore pending ([#76](https://github.com/jdmiranda/phantom/issues/76), [#77](https://github.com/jdmiranda/phantom/issues/77))

### Extensibility
- **phantom-plugins** 🔧 — Plugin lifecycle (manifest, host, registry, marketplace); WASM host is a mock, real wasmtime pending ([#48](https://github.com/jdmiranda/phantom/issues/48))
- **phantom-mcp** 🔧 — Model Context Protocol server (exposes Phantom to external AI) and client (consumes external tools); client impl and registry pending ([#52](https://github.com/jdmiranda/phantom/issues/52), [#54](https://github.com/jdmiranda/phantom/issues/54))
- **phantom-adapter** ✅ — `AppAdapter` trait, app registry, pub/sub event bus, spatial layout negotiation; the "everything is an app" abstraction layer ([#17](https://github.com/jdmiranda/phantom/issues/17))

### Capture Pipeline (Phase 2.G)
- **phantom-vision** 🔧 — Perceptual-hash dedup (dHash + SAD gate) for frame deduplication; GPT-4V analysis pipeline pending ([#70](https://github.com/jdmiranda/phantom/issues/70), [#71](https://github.com/jdmiranda/phantom/issues/71), [#79](https://github.com/jdmiranda/phantom/issues/79))
- **phantom-bundles** 🔧 — Schema-only types for capture bundles (frames, audio, transcript); serialization and capture pipeline integration pending ([#80](https://github.com/jdmiranda/phantom/issues/80), [#81](https://github.com/jdmiranda/phantom/issues/81), [#91](https://github.com/jdmiranda/phantom/issues/91))
- **phantom-bundle-store** 🔧 — Unified persistence: SQLite/FTS5 metadata + LanceDB vectors + XChaCha20 encrypted blobs; recovery path tests pending ([#10](https://github.com/jdmiranda/phantom/issues/10), [#88](https://github.com/jdmiranda/phantom/issues/88))
- **phantom-embeddings** 🔧 — Multi-modal embedding trait + OpenAI backend + mock; persistent storage and vector query pending ([#72](https://github.com/jdmiranda/phantom/issues/72), [#73](https://github.com/jdmiranda/phantom/issues/73))

### Future / Not yet in workspace
These crates have `Cargo.toml` and compile independently but are not yet members of the workspace `Cargo.toml`:

- **phantom-audio** 🔧 — Audio capture backend abstraction (CoreAudio/ScreenCaptureKit traits + mock); real backends pending ([#9](https://github.com/jdmiranda/phantom/issues/9))
- **phantom-recall** 🔧 — Intent-anchored retrieval API: query rewriting, score fusion, ANN routing; backend wiring pending ([#72](https://github.com/jdmiranda/phantom/issues/72))
- **phantom-stt** 🔧 — Speech-to-text backend abstraction (Whisper/Deepgram traits + mock); streaming Whisper and OpenAI integration pending ([#56](https://github.com/jdmiranda/phantom/issues/56), [#68](https://github.com/jdmiranda/phantom/issues/68))
- **phantom-voice** 🔧 — Text-to-speech backend abstraction (ElevenLabs/Piper/system TTS traits + mock); real TTS pending ([#69](https://github.com/jdmiranda/phantom/issues/69))

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