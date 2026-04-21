# ARD-001: Phantom Architecture Decision Record

**Status**: Accepted
**Date**: 2026-04-21
**Authors**: Jeremy Miranda, Claude

---

## 1. Supervisor Architecture: Two-Process Model

### Decision

Phantom uses a **two-process supervisor model** inspired by Erlang/OTP. A lightweight `phantom-supervisor` process spawns, monitors, and controls the main `phantom` app process. Communication happens over a Unix domain socket with a heartbeat protocol.

### Context

We need an "unkillable" control plane — a way to always interact with the system even if the GPU hangs, the PTY deadlocks, or the app OOMs. The user presses `!` and enters system commands (`! restart`, `! kill`, `! set curvature 0.2`) that must always work.

### Options Considered

| Option | Pros | Cons |
|--------|------|------|
| **Supervisor thread** | Simple, no IPC needed | Dies with the process. Can't survive panic/OOM/GPU hang. Shares address space — memory corruption kills it too. |
| **Supervisor process (chosen)** | Separate address space. Survives app crashes. True isolation. Erlang-proven pattern. | IPC overhead (~microseconds, negligible). Slightly more complex deployment (two binaries). |
| **systemd/launchd watchdog** | OS-level supervision | Not portable. User must configure it. No in-app command interface. |

### Why Two Processes

- **Erlang/OTP proved this pattern** over 30 years. The supervisor is always a separate process with its own heap. `one_for_one` restart strategy: if the child dies, only the child restarts. The supervisor's state (restart count, uptime) survives.
- **Memory isolation via `fork()`**: On Unix, child processes get a copy-on-write address space. If the app corrupts its heap, the supervisor is physically untouched.
- **systemd uses the same heartbeat model**: `WatchdogSec` + `sd_notify("WATCHDOG=1")`. If the service stops pinging, systemd kills and restarts it. Our supervisor does the same thing but in-process.
- **The `!` key** is intercepted at the lowest level (winit, before PTY) and routes commands over the Unix socket. Even if the terminal is frozen, the supervisor can `kill -9` and respawn.

### Protocol

Line-based over Unix domain socket at `/tmp/phantom-{pid}.sock`:
- App → Supervisor: `HEARTBEAT`, `LOG:message`
- Supervisor → App: `CMD:set:key:value`, `CMD:theme:name`, `CMD:reload`
- User → Supervisor (via `!` key): `restart`, `kill`, `status`, `set key value`, `theme name`

### References

- [Erlang/OTP Supervisor Behaviour](https://www.erlang.org/doc/system/sup_princ.html) — one_for_one, one_for_all, rest_for_one restart strategies
- [Who Supervises The Supervisors?](https://learnyousomeerlang.com/supervisors) — deep dive on OTP supervision trees
- [rust_supervisor crate](https://docs.rs/rust_supervisor) — Erlang-inspired process supervision in Rust
- [supertrees crate](https://docs.rs/supertrees) — process isolation via fork with restart policies
- [systemd Watchdog for Administrators](http://0pointer.de/blog/projects/watchdog.html) — heartbeat + kill + restart pattern
- [Configure systemd WatchdogSec](https://oneuptime.com/blog/post/2026-03-02-configure-systemd-restartsec-watchdogsec-ubuntu/view) — production watchdog configuration
- [Dealing with process termination in Linux](https://iximiuz.com/en/posts/dealing-with-processes-termination-in-Linux/) — kill/wait patterns, zombie reaping
- [Process spawning performance in Rust](https://kobzol.github.io/rust/2024/01/28/process-spawning-performance-in-rust.html) — spawn vs fork performance analysis

---

## 2. Terminal Emulation: alacritty_terminal Crate

### Decision

Use `alacritty_terminal` (v0.26) as the terminal emulation core rather than building VT100/xterm parsing from scratch.

### Context

Terminal emulation is a solved problem but an enormous one — VT100, xterm, ECMA-48, Unicode, BiDi, sixel, selection, scrollback. Building this from scratch would consume months before we could write a single line of AI code.

### Rationale

- **Production-proven**: Powers Alacritty, one of the most popular terminal emulators. Battle-tested against real-world terminal applications.
- **Complete VTE parsing**: Full escape sequence handling via the `vte` crate. Cursor movement, screen clearing, text attributes, bracketed paste, application cursor mode.
- **Optimized grid**: `Term<T>` provides an efficient 2D cell grid with configurable scrollback, selection handling, and renderable content extraction.
- **Clean separation**: The crate is a library, not an application. We own the rendering, input, and UI — alacritty_terminal owns the state machine.
- **Alternative (building from scratch)**: Estimated 6-12 months just for VT100 compat. This is the tarpit the PHANTOM.md spec explicitly warns about.

### References

- [Alacritty GitHub](https://github.com/alacritty/alacritty) — GPU-accelerated terminal emulator
- [alacritty/vte parser](https://github.com/alacritty/vte) — VT100/xterm escape sequence parser
- [Zutty comparison](https://tomscii.sig7.se/2020/12/A-totally-biased-comparison-of-Zutty) — terminal emulator architectural comparison

---

## 3. GPU Rendering: wgpu

### Decision

Use `wgpu` for all GPU rendering instead of raw OpenGL, Metal, or Vulkan.

### Context

Phantom needs GPU-accelerated rendering for smooth terminal text, instanced quad drawing, and CRT post-processing shaders. We need cross-platform support (macOS Metal, Linux Vulkan, Windows D3D12, and eventually WebGPU in browser).

### Rationale

- **One API, all backends**: wgpu abstracts Vulkan, Metal, DirectX 12, OpenGL ES, and WebGPU behind a single safe Rust API. Write shaders once in WGSL.
- **Safety**: No raw pointers, no undefined behavior. The Rust type system enforces correct resource lifetimes.
- **Modern architecture**: Matches modern GPU hardware better than OpenGL. Explicit command encoding, bind groups, pipeline state objects.
- **WebGPU future**: When Phantom targets the browser, wgpu compiles to WebGPU/WebGL2 via wasm. No rendering rewrite needed.
- **Alternative (OpenGL)**: WezTerm uses OpenGL. More resources/tutorials, but deprecated on macOS, higher CPU overhead, no path to WebGPU.
- **Alternative (raw Vulkan/Metal)**: Maximum performance but enormous API surface. Not worth the complexity for a terminal emulator.

### References

- [wgpu — portable graphics library for Rust](https://wgpu.rs/)
- [wgpu GitHub](https://github.com/gfx-rs/wgpu) — cross-platform safe Rust graphics API
- [Cross-Platform Rust Graphics with wgpu](https://www.blog.brightcoding.dev/2025/09/30/cross-platform-rust-graphics-with-wgpu-one-api-to-rule-vulkan-metal-d3d12-opengl-webgpu/)
- [OpenGL vs Rust wgpu comparison](https://thisvsthat.io/opengl-vs-rust-wgpu)

---

## 4. Text Rendering: cosmic-text + swash

### Decision

Use `cosmic-text` for font loading, text shaping, and layout. Use its built-in `SwashCache` for glyph rasterization.

### Context

Terminal text rendering needs: monospace font loading, Unicode shaping (ligatures, combining characters), glyph rasterization to alpha bitmaps, and font fallback for emoji/CJK.

### Rationale

- **Full text pipeline**: cosmic-text provides FontSystem (system font discovery), Buffer (shaping + layout), and SwashCache (rasterization) in one package. It's the most complete pure-Rust text solution.
- **Font fallback**: Automatic fallback chain for missing glyphs. Critical for terminal emulators that encounter arbitrary Unicode.
- **System font integration**: Discovers and loads system fonts automatically. No bundled font requirement.
- **Alternative (fontdue)**: Fastest rasterizer but no text shaping. The author explicitly recommends cosmic-text for users needing shaping.
- **Alternative (ab_glyph)**: Similar to fontdue — rasterization only, no shaping. Superseded by cosmic-text for our use case.

### References

- [cosmic-text GitHub](https://github.com/pop-os/cosmic-text) — pure Rust multi-line text handling
- [cosmic-text docs](https://pop-os.github.io/cosmic-text/cosmic_text/)
- [fontdue GitHub](https://github.com/mooman219/fontdue) — fastest font renderer, no shaping
- [State of fonts in Rust discussion](https://users.rust-lang.org/t/the-state-of-fonts-parsers-glyph-shaping-and-text-layout-in-rust/32064)

---

## 5. Glyph Atlas: etagere (Shelf Packing)

### Decision

Use `etagere` for GPU glyph atlas bin-packing.

### Context

Rasterized glyphs are uploaded to a single GPU texture atlas. We need an allocator that efficiently packs variable-sized rectangles (glyph bitmaps) into fixed atlas space.

### Rationale

- **Optimized for glyph workloads**: Shelf packing separates the 2D problem into 1D vertical (shelves) + 1D horizontal (within shelf). Works exceptionally well when items have similar heights — exactly the case for monospace terminal glyphs.
- **Battle-tested**: Created by Nicolas Silva for Mozilla WebRender's texture atlas allocation. Used in production Firefox.
- **Used by glyphon**: The standard wgpu text renderer also uses etagere, validating the choice.
- **Alternative (guillotiere)**: Same author, guillotine algorithm. Better for mixed-size workloads but more complex. Overkill for terminal glyphs which are nearly uniform in size.

### References

- [Eight million pixels and counting — etagere design](https://nical.github.io/posts/etagere.html) — detailed shelf-packing algorithm explanation
- [Improving texture atlas allocation in WebRender](https://mozillagfx.wordpress.com/2021/02/04/improving-texture-atlas-allocation-in-webrender/) — Mozilla's production usage
- [etagere crate](https://crates.io/crates/etagere)
- [glyphon — wgpu text renderer using etagere](https://github.com/grovesNL/glyphon)

---

## 6. Layout Engine: taffy (Flexbox)

### Decision

Use `taffy` for UI layout (pane positioning, tab bar, status bar, splits).

### Context

Phantom needs a layout system for positioning terminal panes, tab/status bars, and split views within the window. Must handle resize, nested splits, and fixed-height chrome.

### Rationale

- **CSS Flexbox + Grid**: taffy implements the full CSS Block, Flexbox, and Grid algorithms. Flexbox is a natural fit for terminal pane layouts (grow, shrink, split).
- **Actively maintained**: Used by Dioxus and Bevy. Forked from Stretch (abandoned) and significantly improved.
- **Performance**: High-performance layout computation. Handles hundreds of nodes without frame drops.
- **Alternative (Stretch)**: Original library, but abandoned — no commits in 3+ years.
- **Alternative (Morphorm)**: Simpler one-pass algorithm (~1000 LoC vs taffy's ~1700 for flexbox alone). But less capable, smaller community, fewer features.

### References

- [taffy GitHub](https://github.com/DioxusLabs/taffy) — high-performance Rust layout library
- [taffy docs](https://docs.rs/taffy)
- [Morphorm GitHub](https://github.com/vizia/morphorm) — alternative one-pass layout engine
- [Morphorm integration discussion](https://github.com/DioxusLabs/taffy/issues/308)

---

## 7. CRT Post-Processing: Custom WGSL Shader Pipeline

### Decision

Implement CRT effects as a custom full-screen WGSL post-processing shader rather than using a third-party effects library.

### Context

The retro CRT aesthetic is Phantom's visual signature. We need scanlines, phosphor bloom, chromatic aberration, barrel distortion, vignette, and film grain — all configurable per-theme.

### Rationale

- **Full creative control**: CRT effects are Phantom's brand. We need pixel-level control over every parameter, not a generic filter.
- **Single-pass efficiency**: All six effects run in one fragment shader invocation. No multi-pass overhead.
- **Theme integration**: Each theme defines its own ShaderParams (scanline intensity, bloom, curvature, etc.). The shader reads these from a uniform buffer — zero code changes to add new themes.
- **WGSL native**: Shaders written in WGSL (WebGPU Shading Language) compile on all wgpu backends. No GLSL→SPIRV translation step.
- **No dependency**: Post-processing is ~200 lines of WGSL. Not worth pulling in a library for.

---

## Summary

| Component | Choice | Key Reason |
|-----------|--------|------------|
| Supervisor | Two-process (Erlang model) | Survives app crashes, true isolation |
| Terminal emulation | alacritty_terminal | Solved problem, don't rebuild |
| GPU rendering | wgpu | One API, all backends, safe Rust |
| Text rendering | cosmic-text | Only full pipeline (shape + rasterize + fallback) |
| Glyph atlas | etagere | Optimized for uniform-height glyphs |
| Layout | taffy | CSS Flexbox, actively maintained |
| CRT shaders | Custom WGSL | Creative control, single-pass, no deps |
