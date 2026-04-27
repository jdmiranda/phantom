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

Phantom is structured as a Rust workspace with 18 specialized crates:

### Core System
- **phantom**: Main binary and application orchestrator
- **phantom-supervisor**: Erlang/OTP-inspired process monitor with heartbeat
- **phantom-app**: Application lifecycle and coordination

### Rendering Stack
- **phantom-renderer**: GPU pipeline (wgpu), text atlas, CRT shaders
- **phantom-ui**: Themes, layout (taffy), widgets, keybinds
- **phantom-scene**: Retained scene graph with dirty tracking

### Terminal Emulation
- **phantom-terminal**: PTY management, alacritty_terminal wrapper
- **phantom-semantic**: Command output parsing and understanding

### AI & Intelligence
- **phantom-agents**: AI agent runtime, tools, Claude API integration
- **phantom-brain**: Ambient OODA loop, utility AI scoring
- **phantom-nlp**: Natural language command interpretation
- **phantom-context**: Project/git/environment awareness
- **phantom-memory**: Persistent per-project knowledge storage

### Persistence & History
- **phantom-history**: Structured command history (JSONL)
- **phantom-session**: Session save/restore

### Extensibility
- **phantom-plugins**: WASM plugin host and marketplace
- **phantom-mcp**: Model Context Protocol server/client
- **phantom-protocol**: Supervisor IPC communication
- **phantom-adapter**: WASM app adapter framework

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