//! Main application orchestrator for Phantom.
//!
//! The [`App`] struct owns every subsystem -- GPU, terminal, layout, theming,
//! widgets, and the boot sequence -- and drives the per-frame update/render
//! loop. It is created after the window and GPU context are established and
//! handed control for the lifetime of the application.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Instant;

use anyhow::Result;
use log::{debug, info, trace, warn};
use wgpu::CommandEncoderDescriptor;
use winit::keyboard::{Key, NamedKey};

use phantom_protocol::{AppMessage, SupervisorCommand};
use phantom_renderer::atlas::GlyphAtlas;
use phantom_renderer::gpu::GpuContext;
use phantom_renderer::grid::{GridCell, GridRenderData, GridRenderer};
use phantom_renderer::postfx::{PostFxParams, PostFxPipeline};
use phantom_renderer::quads::{QuadInstance, QuadRenderer};
use phantom_renderer::text::TextRenderer;

use phantom_terminal::input::{self, KeyEvent, PhantomKey, PhantomModifiers};
use phantom_terminal::output::{self, CursorShape, TerminalThemeColors};
use phantom_terminal::terminal::PhantomTerminal;

use phantom_ui::keybinds::{Action, KeyCombo, KeybindRegistry};
use phantom_ui::keybinds::Key as UiKey;
use phantom_ui::layout::{LayoutEngine, PaneId};
use phantom_ui::themes::{self, Theme};
use phantom_ui::widgets::{StatusBar, TabBar, Widget};

use phantom_brain::brain::{BrainConfig, BrainHandle, spawn_brain};
use phantom_brain::events::{AiAction, AiEvent};
use phantom_context::ProjectContext;
use phantom_memory::MemoryStore;
use phantom_nlp::NlpInterpreter;
use phantom_nlp::interpreter::ResolvedAction;
use phantom_scene::node::{NodeKind, RenderLayer};
use phantom_scene::tree::SceneTree;
use phantom_session::session::{SessionManager, SessionState, PaneState};
use phantom_mcp::{spawn_listener, AppCommand, McpListener, ScreenshotReply};

use crate::boot::BootSequence;
use crate::config::PhantomConfig;
use crate::pane::{Pane, pane_cols_rows, pane_inner_rect, container_rect, key_name_to_bytes,
    CONTAINER_PAD_X_CELLS, CONTAINER_TITLE_H_CELLS, CONTAINER_PAD_B_CELLS, CONTAINER_MARGIN};
use crate::supervisor_client::SupervisorClient;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Default font size in points for the terminal text renderer.
const DEFAULT_FONT_SIZE: f32 = 18.0;


// ---------------------------------------------------------------------------
// AppState
// ---------------------------------------------------------------------------

/// Top-level application mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppState {
    /// Playing the cinematic boot sequence.
    Boot,
    /// Normal terminal operation.
    Terminal,
}


// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

/// The heart of Phantom: owns all subsystems and orchestrates the frame loop.
///
/// Created after the winit window and wgpu device are established. The caller
/// (typically the `ApplicationHandler` in main.rs) forwards resize, keyboard,
/// and redraw events here.
pub struct App {
    // -- GPU subsystems --
    pub gpu: GpuContext,
    pub(crate) atlas: GlyphAtlas,
    pub(crate) text_renderer: TextRenderer,
    pub(crate) quad_renderer: QuadRenderer,
    pub(crate) grid_renderer: GridRenderer,
    pub(crate) postfx: PostFxPipeline,

    // -- Terminal panes --
    pub(crate) panes: Vec<Pane>,
    pub(crate) focused_pane: usize,

    // -- UI --
    pub(crate) layout: LayoutEngine,
    pub(crate) keybinds: KeybindRegistry,
    pub(crate) theme: Theme,
    pub(crate) status_bar: StatusBar,
    pub(crate) tab_bar: TabBar,

    // -- Boot sequence --
    pub(crate) boot: BootSequence,
    pub(crate) state: AppState,

    // -- Timing --
    pub(crate) start_time: Instant,
    pub(crate) last_frame: Instant,

    // -- Cached metrics --
    pub(crate) cell_size: (f32, f32),

    // -- Whether a quit has been requested --
    pub(crate) quit_requested: bool,

    // -- Supervisor connection (None when running standalone) --
    pub(crate) supervisor: Option<SupervisorClient>,

    // -- Command mode (backtick key) --
    pub(crate) command_mode: bool,
    pub(crate) command_input: Option<String>,

    // -- Debug shader HUD --
    pub(crate) debug_hud: bool,
    pub(crate) debug_hud_selected: usize,

    // -- AI Brain (OODA loop on dedicated thread) --
    pub(crate) brain: Option<BrainHandle>,

    // -- Project context (auto-detected) --
    pub(crate) context: Option<ProjectContext>,

    // -- Memory store (persistent per-project) --
    pub(crate) memory: Option<MemoryStore>,

    // -- Session manager --
    pub(crate) session_manager: Option<SessionManager>,

    // -- Idle tracking (seconds since last user keypress) --
    pub(crate) last_input_time: Instant,

    // -- Suggestion overlay (from brain) --
    pub(crate) suggestion: Option<SuggestionOverlay>,

    // -- Scene graph (retained, dirty-tracked) --
    pub(crate) scene: SceneTree,

    // -- MCP listener (Unix socket) and inbound command channel --
    pub(crate) mcp_cmd_rx: mpsc::Receiver<AppCommand>,
    pub(crate) _mcp_listener: Option<McpListener>,

    // -- Render pools (reused each frame via clear() to avoid per-frame allocs) --
    pub(crate) pool_quads: Vec<QuadInstance>,
    pub(crate) pool_glyphs: Vec<phantom_renderer::text::GlyphInstance>,
    pub(crate) pool_chrome_quads: Vec<QuadInstance>,
    pub(crate) pool_chrome_glyphs: Vec<phantom_renderer::text::GlyphInstance>,
}

/// An active suggestion from the AI brain.
pub(crate) struct SuggestionOverlay {
    pub(crate) text: String,
    pub(crate) options: Vec<(char, String)>,
    pub(crate) shown_at: Instant,
}

impl App {
    /// Create the application, initializing all subsystems.
    ///
    /// The `gpu` context must already be fully initialized with a configured
    /// surface. This constructor creates the terminal (spawning a PTY),
    /// renderers, layout engine, and boot sequence.
    pub fn new(gpu: GpuContext) -> Result<Self> {
        Self::with_config(gpu, PhantomConfig::load(), None)
    }

    /// Create the application with an explicit config (for CLI overrides).
    pub fn with_config(
        gpu: GpuContext,
        config: PhantomConfig,
        supervisor_socket: Option<&Path>,
    ) -> Result<Self> {
        Self::with_config_scaled(gpu, config, supervisor_socket, 1.0)
    }

    /// Create the application with config and display scale factor.
    ///
    /// `scale_factor` is the display's DPI scale (e.g. 2.0 on Retina Macs).
    /// Font size is multiplied by this to render at the correct visual size.
    pub fn with_config_scaled(
        gpu: GpuContext,
        config: PhantomConfig,
        supervisor_socket: Option<&Path>,
        scale_factor: f32,
    ) -> Result<Self> {
        let width = gpu.surface_config.width;
        let height = gpu.surface_config.height;
        let format = gpu.format;

        // -- Font / text (scaled for HiDPI) --
        let scaled_font_size = config.font_size * scale_factor;
        info!("Font: {:.0}pt logical × {:.1}x scale = {:.0}pt physical", config.font_size, scale_factor, scaled_font_size);
        let mut text_renderer = TextRenderer::new(scaled_font_size);
        let cell_size = text_renderer.measure_cell();
        info!(
            "Cell size: {:.1}x{:.1} at {:.0}pt",
            cell_size.0, cell_size.1, DEFAULT_FONT_SIZE
        );

        // -- Atlas --
        let atlas = GlyphAtlas::new(&gpu.device, &gpu.queue);

        // -- Renderers --
        let quad_renderer = QuadRenderer::new(&gpu.device, format);
        let grid_renderer =
            GridRenderer::new(&gpu.device, format, atlas.bind_group_layout());
        let postfx = PostFxPipeline::new(&gpu.device, format, width, height);

        // -- Terminal dimensions from window size --
        // Reserve space for the tab bar (30px), status bar (28px), and the
        // app-container chrome (padding + title strip) inside the pane.
        let chrome_height = (30.0 + 28.0) * scale_factor;
        let content_height = (height as f32 - chrome_height).max(cell_size.1);
        let initial_outer = phantom_ui::layout::Rect {
            x: 0.0,
            y: 0.0,
            width: width as f32,
            height: content_height,
        };
        let (cols, rows) = pane_cols_rows(cell_size, initial_outer);

        info!("Terminal: {cols}x{rows} (window {width}x{height})");

        let terminal = PhantomTerminal::new(cols, rows)?;

        // -- Layout --
        let mut layout = LayoutEngine::with_scale(scale_factor)?;
        let pane_id = layout.add_pane()?;
        layout.resize(width as f32, height as f32)?;

        // -- Panes --
        let panes = vec![Pane {
            terminal,
            pane_id,
            was_alt_screen: false,
            is_detached: false,
            detached_label: String::new(),
            output_buf: String::new(),
            error_notified: false,
        }];

        // -- Keybinds --
        let keybinds = KeybindRegistry::new();

        // -- Theme (from config, with shader overrides) --
        let theme = config.resolve_theme();

        // -- Widgets --
        let mut tab_bar = TabBar::new();
        tab_bar.add_tab("shell");

        let mut status_bar = StatusBar::new();

        // -- Project context (auto-detect language, git, etc.) --
        let project_dir = std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| ".".into());
        let context = ProjectContext::detect(Path::new(&project_dir));
        info!(
            "Project detected: {} [{:?}]",
            context.name, context.project_type
        );

        // Wire context into status bar.
        status_bar.set_cwd(&context.name);
        if let Some(ref git) = context.git {
            status_bar.set_branch(&git.branch);
        }

        // -- Memory store (persistent per-project) --
        let memory = match MemoryStore::open(&project_dir) {
            Ok(m) => Some(m),
            Err(e) => {
                warn!("Failed to open memory store: {e}");
                None
            }
        };

        // -- AI Brain thread --
        let brain = spawn_brain(BrainConfig {
            project_dir: project_dir.clone(),
            enable_suggestions: true,
            enable_memory: true,
            quiet_threshold: 0.5,
            router: None,
        });
        info!("AI brain spawned");

        // -- Scene graph --
        let mut scene = SceneTree::new();
        let root = scene.root();
        // Structural nodes for the UI hierarchy.
        let _tab_bar_node = scene.add_node(root, NodeKind::TabBar);
        let content_node = scene.add_node(root, NodeKind::ContentArea);
        let _status_bar_node = scene.add_node(root, NodeKind::StatusBar);
        // First pane under content area.
        let _pane_node = scene.add_node(content_node, NodeKind::Pane);
        // Overlay nodes (rendered after CRT post-fx).
        let cmd_bar_node = scene.add_node(root, NodeKind::CommandBar);
        if let Some(n) = scene.get_mut(cmd_bar_node) { n.render_layer = RenderLayer::Overlay; }
        let debug_hud_node = scene.add_node(root, NodeKind::DebugHud);
        if let Some(n) = scene.get_mut(debug_hud_node) { n.render_layer = RenderLayer::Overlay; }
        let suggestion_node = scene.add_node(root, NodeKind::AgentSuggestion);
        if let Some(n) = scene.get_mut(suggestion_node) { n.render_layer = RenderLayer::Overlay; }
        let _ = (cmd_bar_node, debug_hud_node, suggestion_node); // suppress unused
        // Set initial transforms.
        scene.set_transform(root, 0.0, 0.0, width as f32, height as f32);
        scene.update_world_transforms();
        info!("Scene graph initialized: {} nodes", scene.node_count());

        // -- Session manager --
        let session_manager = match SessionManager::new() {
            Ok(sm) => Some(sm),
            Err(e) => {
                warn!("Failed to create session manager: {e}");
                None
            }
        };

        // -- Boot --
        // Boot sequence sized to the full window (in character cells).
        let boot_cols = (width as f32 / cell_size.0).floor().max(40.0) as usize;
        let boot_rows = (height as f32 / cell_size.1).floor().max(10.0) as usize;
        let mut boot = BootSequence::with_size(boot_cols, boot_rows);
        let initial_state = if config.skip_boot {
            boot.skip_immediate();
            AppState::Terminal
        } else {
            AppState::Boot
        };

        // -- Supervisor connection --
        let supervisor = if let Some(sock) = supervisor_socket {
            match SupervisorClient::connect(sock) {
                Ok(mut client) => {
                    client.send_ready();
                    Some(client)
                }
                Err(e) => {
                    warn!("Failed to connect to supervisor: {e}");
                    None
                }
            }
        } else {
            None
        };

        // -- MCP listener --
        // Bind a Unix socket so external MCP clients (Claude Code, `nc -U`, etc.)
        // can drive Phantom live. Path honors $PHANTOM_MCP_SOCK; otherwise
        // falls back to /tmp/phantom-mcp-<pid>.sock.
        let mcp_socket_path = std::env::var("PHANTOM_MCP_SOCK")
            .ok()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(format!("/tmp/phantom-mcp-{}.sock", std::process::id())));
        let (mcp_cmd_tx, mcp_cmd_rx) = mpsc::channel::<AppCommand>();
        let mcp_listener = match spawn_listener(mcp_socket_path.clone(), mcp_cmd_tx) {
            Ok(l) => {
                info!("MCP listener ready: {}", mcp_socket_path.display());
                Some(l)
            }
            Err(e) => {
                warn!("Failed to start MCP listener at {}: {e}", mcp_socket_path.display());
                None
            }
        };

        let now = Instant::now();

        Ok(Self {
            gpu,
            atlas,
            text_renderer,
            quad_renderer,
            grid_renderer,
            postfx,
            panes,
            focused_pane: 0,
            layout,
            keybinds,
            theme,
            status_bar,
            tab_bar,
            boot,
            state: initial_state,
            start_time: now,
            last_frame: now,
            cell_size,
            quit_requested: false,
            supervisor,
            command_mode: false,
            command_input: None,
            debug_hud: false,
            debug_hud_selected: 0,
            brain: Some(brain),
            context: Some(context),
            memory,
            session_manager,
            last_input_time: now,
            suggestion: None,
            scene,
            mcp_cmd_rx,
            _mcp_listener: mcp_listener,
            pool_quads: Vec::with_capacity(256),
            pool_glyphs: Vec::with_capacity(4096),
            pool_chrome_quads: Vec::with_capacity(32),
            pool_chrome_glyphs: Vec::with_capacity(256),
        })
    }

    /// Returns `true` if the app has requested to quit.
    pub fn should_quit(&self) -> bool {
        self.quit_requested
    }

    /// Graceful shutdown: save session, shut down brain thread.
    pub fn shutdown(&mut self) {
        // Tell the supervisor we're exiting on purpose — don't restart.
        if let Some(ref mut sv) = self.supervisor {
            sv.send(&AppMessage::ExitClean);
        }

        // Save session state.
        if let Some(ref sm) = self.session_manager {
            let state = self.build_session_state();
            match sm.save(&state) {
                Ok(path) => info!("Session saved to {}", path.display()),
                Err(e) => warn!("Failed to save session: {e}"),
            }
        }

        // Shut down the brain thread.
        if let Some(ref brain) = self.brain {
            let _ = brain.send_event(AiEvent::Shutdown);
        }
    }

    /// Build a SessionState snapshot from current app state.
    fn build_session_state(&self) -> SessionState {
        use std::time::{SystemTime, UNIX_EPOCH};

        let project_dir = self.context.as_ref()
            .map(|c| c.root.clone())
            .unwrap_or_else(|| ".".into());
        let project_name = self.context.as_ref()
            .map(|c| c.name.clone())
            .unwrap_or_else(|| "unknown".into());
        let git_branch = self.context.as_ref()
            .and_then(|c| c.git.as_ref().map(|g| g.branch.clone()));

        let panes: Vec<PaneState> = self.panes.iter().enumerate().map(|(i, pane)| {
            let term_size = pane.terminal.size();
            PaneState {
                working_dir: project_dir.clone(),
                is_focused: i == self.focused_pane,
                cols: term_size.cols,
                rows: term_size.rows,
                title: if pane.is_detached {
                    pane.detached_label.clone()
                } else {
                    "shell".into()
                },
                split: None,
            }
        }).collect();

        SessionState {
            version: 1,
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            project_dir,
            project_name,
            git_branch,
            panes,
            theme_name: self.theme.name.clone(),
            font_size: self.text_renderer.font_size(),
            activity: None,
        }
    }

    // -----------------------------------------------------------------------
    // Resize
    // -----------------------------------------------------------------------

    /// Handle a window resize event.
    ///
    /// Propagates the new dimensions to the GPU surface, post-fx pipeline,
    /// layout engine, and terminal PTY.
    pub fn handle_resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }

        debug!("Resize: {width}x{height}");

        // GPU surface
        self.gpu.resize(width, height);

        // Post-FX offscreen texture
        self.postfx.resize(&self.gpu.device, width, height);

        // Layout
        if let Err(e) = self.layout.resize(width as f32, height as f32) {
            warn!("Layout resize failed: {e}");
        }

        // Recompute terminal dimensions for every pane. Cols/rows come from
        // the pane's *inner* rect (inside container chrome) so the shell sees
        // the same area we draw into.
        for pane in &mut self.panes {
            let layout_r = self.layout.get_pane_rect(pane.pane_id).unwrap_or_else(|e| {
                warn!("Layout missing for pane {:?} on resize: {e}", pane.pane_id);
                phantom_ui::layout::Rect { x: 0.0, y: 30.0, width: width as f32, height: height as f32 - 54.0 }
            });
            let (cols, rows) = pane_cols_rows(self.cell_size, layout_r);
            pane.terminal.resize(cols, rows);
            trace!("Pane resized to {cols}x{rows}");
        }

        // Update scene graph root transform.
        let root = self.scene.root();
        self.scene.set_transform(root, 0.0, 0.0, width as f32, height as f32);
        self.scene.update_world_transforms();
    }

    // -----------------------------------------------------------------------
    // Keyboard input
    // -----------------------------------------------------------------------

    /// Handle a winit keyboard event.
    ///
    /// First checks the keybind registry for an application-level action.
    /// If no binding matches, encodes the key event as terminal input bytes
    /// and writes them to the PTY.
    pub fn handle_key(&mut self, event: winit::event::KeyEvent) {
        // Only process key presses, not releases.
        if !event.state.is_pressed() {
            return;
        }

        // Track last input for idle detection.
        self.last_input_time = Instant::now();

        // Dismiss active suggestion on any keypress.
        if self.suggestion.is_some() {
            // Check if the user pressed a shortcut key for a suggestion option.
            if let Key::Character(ref s) = event.logical_key {
                if let Some(ch) = s.chars().next() {
                    let ch_lower = ch.to_ascii_lowercase();
                    let matched = self.suggestion.as_ref()
                        .and_then(|s| s.options.iter().find(|(k, _)| *k == ch_lower).map(|(_, v)| v.clone()));
                    if let Some(action_text) = matched {
                        info!("[PHANTOM]: User chose: {action_text}");
                        self.suggestion = None;
                        return;
                    }
                }
            }
            // Any other key dismisses the suggestion.
            self.suggestion = None;
        }

        // -- Debug HUD input handling --
        if self.debug_hud {
            self.handle_debug_hud_key(&event);
            return;
        }

        // -- Command mode input handling --
        if self.command_mode {
            self.handle_command_mode_key(&event);
            return;
        }

        // Backtick (`) toggles command mode on.
        if matches!(&event.logical_key, Key::Character(s) if s.as_str() == "`") {
            self.command_mode = true;
            self.command_input = Some(String::new());
            debug!("Command mode activated");
            return;
        }

        // Convert winit logical key to our combo for keybind lookup.
        if let Some(combo) = winit_key_to_combo(&event) {
            if let Some(action) = self.keybinds.lookup(&combo) {
                self.dispatch_action(*action);
                return;
            }
        }

        // During boot, keypress dismisses the boot screen (if paused) or skips ahead.
        if self.state == AppState::Boot {
            if self.boot.is_waiting() {
                self.boot.dismiss();
            } else {
                self.boot.skip();
            }
            return;
        }

        // Convert to terminal key event and write to focused pane's PTY.
        if let Some(terminal_event) = winit_key_to_terminal(&event) {
            let bytes = input::encode_key(&terminal_event);
            if !bytes.is_empty() {
                if let Some(pane) = self.panes.get_mut(self.focused_pane) {
                    if let Err(e) = pane.terminal.pty_write(&bytes) {
                        warn!("PTY write failed: {e}");
                    }
                }
            }
        }
    }

    /// Handle modifier state changes from winit.
    ///
    /// This is called from `WindowEvent::ModifiersChanged` in the event loop.
    /// We store the current modifier state for use in key event handling.
    pub fn handle_modifiers(&mut self, modifiers: winit::event::Modifiers) {
        // Stored for future use with key event dispatch if needed.
        // Currently winit_key_to_combo derives modifiers from the event itself.
        let _ = modifiers;
    }

    /// Dispatch an application-level action from the keybind registry.
    fn dispatch_action(&mut self, action: Action) {
        match action {
            Action::Quit => {
                info!("Quit requested via keybind");
                self.quit_requested = true;
            }
            Action::Copy => {
                // TODO: implement clipboard copy from terminal selection
                debug!("Action: Copy (not yet implemented)");
            }
            Action::Paste => {
                // TODO: read from clipboard and write to PTY via encode_paste
                debug!("Action: Paste (not yet implemented)");
            }
            Action::NewTab => {
                debug!("Action: NewTab (not yet implemented)");
            }
            Action::CloseTab => {
                debug!("Action: CloseTab (not yet implemented)");
            }
            Action::SplitHorizontal => {
                self.split_focused_pane(true);
            }
            Action::SplitVertical => {
                self.split_focused_pane(false);
            }
            Action::FocusNext => {
                if !self.panes.is_empty() {
                    self.focused_pane = (self.focused_pane + 1) % self.panes.len();
                    debug!("Focus next: pane {}", self.focused_pane);
                }
            }
            Action::FocusPrev => {
                if !self.panes.is_empty() {
                    self.focused_pane = (self.focused_pane + self.panes.len() - 1) % self.panes.len();
                    debug!("Focus prev: pane {}", self.focused_pane);
                }
            }
            Action::CloseFocused => {
                self.close_focused_pane();
            }
            Action::ZoomIn => {
                let new_size = self.text_renderer.font_size() + 2.0;
                info!("Zoom in: {new_size}pt");
                self.text_renderer.set_font_size(new_size);
                self.cell_size = self.text_renderer.measure_cell();
                self.atlas.clear();
            }
            Action::ZoomOut => {
                let new_size = (self.text_renderer.font_size() - 2.0).max(8.0);
                info!("Zoom out: {new_size}pt");
                self.text_renderer.set_font_size(new_size);
                self.cell_size = self.text_renderer.measure_cell();
                self.atlas.clear();
            }
            _ => {
                debug!("Action: {action} (not yet implemented)");
            }
        }
    }


    // -----------------------------------------------------------------------
    // Update
    // -----------------------------------------------------------------------

    /// Per-frame update: read PTY data, advance boot sequence, update widgets.
    ///
    /// Call this once per frame before [`render`](Self::render).
    pub fn update(&mut self) {
        let now = Instant::now();
        let dt = now.duration_since(self.last_frame).as_secs_f32();
        self.last_frame = now;

        // Read from all panes' PTYs (non-blocking). Collect indices of exited panes.
        let mut exited: Vec<usize> = Vec::new();
        for (i, pane) in self.panes.iter_mut().enumerate() {
            match pane.terminal.pty_read() {
                Ok(n) => {
                    if n > 0 {
                        trace!("Pane {i} PTY read: {n} bytes");

                        // Capture raw output for semantic scanning.
                        let raw = &pane.terminal.last_read_buf()[..n];
                        if let Ok(text) = std::str::from_utf8(raw) {
                            pane.output_buf.push_str(text);
                            // Rolling window: keep last 8KB.
                            if pane.output_buf.len() > 8192 {
                                let trim = pane.output_buf.len() - 8192;
                                pane.output_buf.drain(..trim);
                            }
                            // Reset error notification when new output arrives.
                            pane.error_notified = false;
                        }
                    }
                }
                Err(e) => {
                    // PTY EOF means the shell exited in this pane.
                    warn!("Pane {i} PTY read error (shell may have exited): {e}");
                    exited.push(i);
                }
            }
        }

        // Semantic scan: detect errors in PTY output and notify brain.
        if let Some(ref brain) = self.brain {
            for pane in self.panes.iter_mut() {
                if pane.error_notified || pane.is_detached || pane.output_buf.is_empty() {
                    continue;
                }
                // Quick heuristic scan for error patterns.
                let buf = &pane.output_buf;
                let has_error = buf.contains("error[E")        // Rust
                    || buf.contains("Error:")                   // generic
                    || buf.contains("FAILED")                   // test failures
                    || buf.contains("error: ")                  // cargo/generic
                    || buf.contains("npm ERR!")                 // npm
                    || buf.contains("Traceback (most recent")   // Python
                    || buf.contains("SyntaxError")              // JS/Python
                    || buf.contains("TypeError")                // JS/Python
                    || buf.contains("panic at");                // Rust panic

                if has_error {
                    // Parse the buffer through the semantic parser.
                    let parsed = phantom_semantic::SemanticParser::parse(
                        "",
                        &pane.output_buf,
                        &pane.output_buf,
                        Some(1), // nonzero = error
                    );
                    if !parsed.errors.is_empty() {
                        let _ = brain.send_event(AiEvent::CommandComplete(parsed));
                        pane.error_notified = true;
                    }
                }

                // Detect prompt return (shell ready) — clear the buffer.
                // Simple heuristic: buffer ends with common prompt suffixes.
                let trimmed = buf.trim_end();
                if trimmed.ends_with("$ ") || trimmed.ends_with("% ") || trimmed.ends_with("> ") || trimmed.ends_with("# ") {
                    pane.output_buf.clear();
                }
            }
        }

        // -- Alt-screen detection: detect interactive program attach/detach --
        for pane in self.panes.iter_mut() {
            let is_alt = phantom_terminal::alt_screen::is_alt_screen(pane.terminal.term());

            if is_alt && !pane.was_alt_screen {
                // Just entered alt screen -- interactive program started.
                pane.is_detached = true;
                pane.detached_label = phantom_terminal::process::foreground_process_name(
                    pane.terminal.pty_fd(),
                )
                .unwrap_or_else(|| "interactive".to_string());
                info!("Pane detached: process \"{}\"", pane.detached_label);
            }

            if !is_alt && pane.was_alt_screen && pane.is_detached {
                // Left alt screen -- interactive program exited.
                info!("Pane reattached (was \"{}\")", pane.detached_label);
                pane.is_detached = false;
                pane.detached_label.clear();
            }

            pane.was_alt_screen = is_alt;
        }

        // Remove exited panes in reverse order so indices stay valid.
        for &i in exited.iter().rev() {
            let pane = self.panes.remove(i);
            if let Err(e) = self.layout.remove_pane(pane.pane_id) {
                warn!("Failed to remove exited pane from layout: {e}");
            }
            // Adjust focused_pane index.
            if self.focused_pane >= self.panes.len() && !self.panes.is_empty() {
                self.focused_pane = self.panes.len() - 1;
            }
        }

        if !exited.is_empty() {
            // Recompute layout after removals.
            let width = self.gpu.surface_config.width;
            let height = self.gpu.surface_config.height;
            let _ = self.layout.resize(width as f32, height as f32);

            // Resize remaining panes.
            for pane in &mut self.panes {
                if let Ok(rect) = self.layout.get_pane_rect(pane.pane_id) {
                    let (cols, rows) = pane_cols_rows(self.cell_size, rect);
                    pane.terminal.resize(cols, rows);
                }
            }
        }

        // If all panes are gone, quit.
        if self.panes.is_empty() {
            info!("All panes exited, quitting");
            self.quit_requested = true;
        }

        // Boot sequence state machine.
        if self.state == AppState::Boot {
            self.boot.update(dt);
            if self.boot.is_done() {
                info!("Boot sequence complete, transitioning to terminal");
                self.state = AppState::Terminal;
            }
        }

        // -- Supervisor heartbeat & command polling --
        if let Some(ref mut sv) = self.supervisor {
            sv.send_heartbeat();
        }
        // Drain supervisor commands (separate borrow to avoid alias issues).
        let cmd = self.supervisor.as_mut().and_then(|sv| sv.try_recv());
        if let Some(cmd) = cmd {
            self.handle_supervisor_command(cmd);
        }

        // -- AI Brain: send idle events + drain actions --
        if let Some(ref brain) = self.brain {
            let idle_secs = now.duration_since(self.last_input_time).as_secs_f32();
            // Send idle pulse every ~5 seconds (approximated by frame tick).
            if idle_secs > 5.0 && (idle_secs % 5.0) < dt {
                let _ = brain.send_event(AiEvent::UserIdle { seconds: idle_secs });
            }

            // Drain brain actions.
            while let Some(action) = brain.try_recv_action() {
                match action {
                    AiAction::ShowSuggestion { text, options } => {
                        info!("[PHANTOM]: {text}");
                        self.suggestion = Some(SuggestionOverlay {
                            text,
                            options,
                            shown_at: now,
                        });
                    }
                    AiAction::ShowNotification(msg) => {
                        info!("[PHANTOM]: {msg}");
                    }
                    AiAction::UpdateMemory { key, value } => {
                        if let Some(ref mut mem) = self.memory {
                            let _ = mem.set(
                                &key,
                                &value,
                                phantom_memory::MemoryCategory::Context,
                                phantom_memory::MemorySource::Auto,
                            );
                        }
                    }
                    AiAction::SpawnAgent(_task) => {
                        // Agent pane creation will be wired in a follow-up.
                        debug!("Brain requested agent spawn (not yet wired to GUI panes)");
                    }
                    AiAction::RunCommand(cmd) => {
                        debug!("Brain suggested command: {cmd}");
                    }
                    AiAction::DoNothing => {}
                }
            }
        }

        // Expire stale suggestions after 10 seconds.
        if let Some(ref s) = self.suggestion {
            if now.duration_since(s.shown_at).as_secs() > 10 {
                self.suggestion = None;
            }
        }

        // -- Refresh git context periodically (every ~30s, off main thread) --
        if let Some(ref ctx) = self.context {
            let elapsed = now.duration_since(self.start_time).as_secs();
            if elapsed > 0 && elapsed % 30 == 0 && dt > 0.0 {
                // Spawn a background thread so git commands don't block rendering.
                let project_dir = ctx.root.clone();
                let brain_tx = self.brain.as_ref().map(|b| b.event_sender());
                std::thread::spawn(move || {
                    let mut fresh = ProjectContext::detect(std::path::Path::new(&project_dir));
                    fresh.refresh_git();
                    // Notify the brain of git state change.
                    if let Some(tx) = brain_tx {
                        let _ = tx.send(AiEvent::GitStateChanged);
                    }
                });
            }
            // Update status bar from cached context (non-blocking).
            if let Some(ref git) = ctx.git {
                self.status_bar.set_branch(&git.branch);
            }
        }

        // -- Drain MCP commands from external clients --
        // Each command carries a reply channel; handler runs to completion
        // (including GPU readback for screenshots) before we reply.
        loop {
            match self.mcp_cmd_rx.try_recv() {
                Ok(cmd) => self.handle_mcp_command(cmd),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    warn!("MCP command channel disconnected");
                    break;
                }
            }
        }

        // Update status bar clock.
        let now_wall = chrono_time_string();
        self.status_bar.set_time(&now_wall);
    }

    // -----------------------------------------------------------------------
    // MCP command handlers
    // -----------------------------------------------------------------------

    /// Handle one inbound MCP command by running the relevant app-side work
    /// and sending the result back on the command's reply channel.
    fn handle_mcp_command(&mut self, cmd: AppCommand) {
        match cmd {
            AppCommand::Screenshot { path, reply } => {
                let result = self.mcp_capture_screenshot(&path);
                let _ = reply.send(result);
            }
            AppCommand::RunCommand { command, reply } => {
                let result = self.mcp_send_to_pty(&command);
                let _ = reply.send(result);
            }
            AppCommand::SendKey { key, reply } => {
                let result = self.mcp_send_key(&key);
                let _ = reply.send(result);
            }
            AppCommand::ReadTerminalState { reply } => {
                let text = self.mcp_read_terminal_state();
                let _ = reply.send(Ok(text));
            }
            AppCommand::GetContext { reply } => {
                let json = self.mcp_get_context_json();
                let _ = reply.send(Ok(json));
            }
        }
    }

    /// Capture the current pre-CRT scene texture and save it as a PNG at `path`.
    ///
    /// Uses `phantom_renderer::screenshot::capture_frame` against the PostFx
    /// offscreen texture (same pixels that get the CRT shader applied). The
    /// texture holds the last rendered frame, so this is "current state" from
    /// the user's POV with one frame of latency.
    fn mcp_capture_screenshot(&mut self, path: &Path) -> Result<ScreenshotReply, String> {
        use phantom_renderer::screenshot::{capture_frame, ScreenshotMetadata, save_screenshot};
        use std::time::{SystemTime, UNIX_EPOCH};

        let texture = self.postfx.scene_texture();
        let width = texture.width();
        let height = texture.height();

        let pixels = capture_frame(&self.gpu.device, &self.gpu.queue, texture, width, height);

        // Swizzle BGRA -> RGBA if the surface format is BGRA (macOS default).
        let pixels_rgba = match self.gpu.format {
            wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb => {
                let mut out = pixels.clone();
                for px in out.chunks_exact_mut(4) {
                    px.swap(0, 2); // B <-> R
                }
                out
            }
            _ => pixels,
        };

        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let metadata = ScreenshotMetadata {
            timestamp,
            width,
            height,
            theme: self.theme.name.clone(),
            pane_count: self.panes.len(),
            project: self.context.as_ref().map(|c| c.name.clone()),
            branch: self.context.as_ref().and_then(|c| c.git.as_ref().map(|g| g.branch.clone())),
        };

        save_screenshot(&pixels_rgba, width, height, &metadata, path)
            .map_err(|e| format!("save failed: {e}"))?;

        info!("Screenshot saved via MCP: {} ({}x{})", path.display(), width, height);

        Ok(ScreenshotReply {
            path: path.to_path_buf(),
            width,
            height,
        })
    }

    /// Handle an external key event over MCP.
    ///
    /// State-aware: during boot, any key dismisses the current pause or
    /// skips the sequence. During normal operation, the key name is
    /// translated to terminal input bytes and written to the focused
    /// pane's PTY.
    ///
    /// Returns a short note describing what happened (dismissed boot,
    /// skipped boot, or wrote N bytes to PTY) for debugging over the wire.
    fn mcp_send_key(&mut self, key: &str) -> Result<String, String> {
        if self.state == AppState::Boot {
            if self.boot.is_waiting() {
                self.boot.dismiss();
                return Ok("dismissed boot pause".into());
            }
            self.boot.skip();
            return Ok("skipped boot sequence".into());
        }

        let bytes = key_name_to_bytes(key);
        let pane = self.panes.get_mut(self.focused_pane)
            .ok_or_else(|| "no focused pane".to_string())?;
        pane.terminal.pty_write(&bytes)
            .map_err(|e| format!("pty_write failed: {e}"))?;
        // Reset idle timer — an MCP key counts as user input for brain purposes.
        self.last_input_time = Instant::now();
        Ok(format!("wrote {} bytes to pty", bytes.len()))
    }

    /// Write a string to the focused pane's PTY. Appends a newline so the
    /// shell executes it.
    fn mcp_send_to_pty(&mut self, command: &str) -> Result<(), String> {
        let pane = self.panes.get_mut(self.focused_pane)
            .ok_or_else(|| "no focused pane".to_string())?;

        let mut bytes = command.as_bytes().to_vec();
        if !bytes.ends_with(b"\n") {
            bytes.push(b'\n');
        }
        pane.terminal.pty_write(&bytes)
            .map_err(|e| format!("pty_write failed: {e}"))?;

        Ok(())
    }

    /// Extract visible terminal grid from the focused pane as plain text.
    fn mcp_read_terminal_state(&self) -> String {
        let Some(pane) = self.panes.get(self.focused_pane) else {
            return String::new();
        };
        let (cells, cols, rows, _cursor) = output::extract_grid(pane.terminal.term());

        let mut out = String::with_capacity(cells.len() + rows);
        for row in 0..rows {
            for col in 0..cols {
                let idx = row * cols + col;
                if let Some(cell) = cells.get(idx) {
                    out.push(cell.ch);
                }
            }
            out.push('\n');
        }
        // Trim trailing whitespace rows for readability.
        while out.ends_with("\n\n") {
            out.pop();
        }
        out
    }

    /// Serialize project context as JSON.
    fn mcp_get_context_json(&self) -> serde_json::Value {
        use serde_json::json;
        let Some(ctx) = &self.context else {
            return json!({});
        };
        json!({
            "name": ctx.name,
            "root": ctx.root,
            "project_type": format!("{:?}", ctx.project_type),
            "git": ctx.git.as_ref().map(|g| json!({
                "branch": g.branch,
                "dirty": g.is_dirty,
                "ahead": g.ahead,
                "behind": g.behind,
            })),
            "pane_count": self.panes.len(),
            "focused_pane": self.focused_pane,
            "theme": self.theme.name,
        })
    }

    // -----------------------------------------------------------------------
    // Render
    // -----------------------------------------------------------------------

    /// Render one frame.
    ///
    /// This is the main render path, called every `RedrawRequested`. It:
    /// 1. Renders the scene (boot or terminal) into the PostFx offscreen texture.
    /// 2. Composites CRT effects onto the final surface texture.
    pub fn render(&mut self) -> Result<()> {
        let output = self.gpu.surface.get_current_texture()?;
        let surface_view = output.texture.create_view(&Default::default());

        let width = self.gpu.surface_config.width;
        let height = self.gpu.surface_config.height;
        let screen_size = [width as f32, height as f32];

        let mut encoder = self
            .gpu
            .device
            .create_command_encoder(&CommandEncoderDescriptor {
                label: Some("phantom-frame-encoder"),
            });

        // -----------------------------------------------------------------
        // Collect scene data (quads + glyphs) — must happen before
        // borrowing self.postfx.scene_view() to avoid borrow conflicts.
        // -----------------------------------------------------------------
        // Reuse pooled Vecs (clear + retain capacity instead of allocating).
        let mut all_quads = std::mem::take(&mut self.pool_quads);
        let mut all_glyphs = std::mem::take(&mut self.pool_glyphs);
        let mut chrome_quads = std::mem::take(&mut self.pool_chrome_quads);
        let mut chrome_glyphs = std::mem::take(&mut self.pool_chrome_glyphs);
        all_quads.clear();
        all_glyphs.clear();
        chrome_quads.clear();
        chrome_glyphs.clear();

        match self.state {
            AppState::Boot => {
                self.render_boot(
                    screen_size,
                    &mut all_quads,
                    &mut all_glyphs,
                );
            }
            AppState::Terminal => {
                self.render_terminal(
                    screen_size,
                    &mut all_quads,
                    &mut all_glyphs,
                    &mut chrome_quads,
                    &mut chrome_glyphs,
                );
            }
        }

        // Upload to GPU.
        self.quad_renderer
            .prepare(&self.gpu.device, &self.gpu.queue, &all_quads, screen_size);
        self.grid_renderer
            .prepare(&self.gpu.device, &self.gpu.queue, &all_glyphs, screen_size);

        // -----------------------------------------------------------------
        // Scene pass: render into the PostFx offscreen texture
        // -----------------------------------------------------------------
        {
            let bg = self.theme.colors.background;
            let scene_view = self.postfx.scene_view();

            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("scene-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: scene_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: bg[0] as f64,
                            g: bg[1] as f64,
                            b: bg[2] as f64,
                            a: bg[3] as f64,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });

            // Draw quads (backgrounds, cursor, UI chrome).
            self.quad_renderer.render(&mut pass);

            // Draw glyphs (text).
            self.grid_renderer
                .render(&mut pass, self.atlas.bind_group());
        }

        // -----------------------------------------------------------------
        // Post-FX pass: composite CRT effects onto the surface
        // -----------------------------------------------------------------
        {
            let sp = &self.theme.shader_params;
            let elapsed = self.start_time.elapsed().as_secs_f32();

            // During boot, scale CRT effect intensities by the boot warmup ramp.
            let crt_scale = if self.state == AppState::Boot {
                self.boot.crt_intensity()
            } else {
                1.0
            };

            let params = PostFxParams::from_theme(
                sp.scanline_intensity * crt_scale,
                sp.bloom_intensity * crt_scale,
                sp.chromatic_aberration * crt_scale,
                sp.curvature * crt_scale,
                sp.vignette_intensity * crt_scale,
                sp.noise_intensity * crt_scale,
                sp.glow_color,
                elapsed,
                width,
                height,
            );

            self.postfx
                .render(&mut encoder, &surface_view, &self.gpu.queue, &params);
        }

        // -----------------------------------------------------------------
        // Pass 3: System overlay — rendered AFTER CRT, directly on surface.
        // No post-processing. Crisp, clean, always readable.
        // Includes: container chrome (tint, border, title) + command bar +
        // debug HUD + AI suggestion overlay.
        // -----------------------------------------------------------------
        {
            // Chrome vecs already contain container quads/glyphs from render_terminal.
            // Append command mode / debug HUD / suggestion overlays on top.

            // -- Command input bar --
            if self.command_mode {
                self.build_command_overlay(screen_size, &mut chrome_quads, &mut chrome_glyphs);
            }

            // -- Debug shader HUD --
            if self.debug_hud {
                self.build_debug_hud(screen_size, &mut chrome_quads, &mut chrome_glyphs);
            }

            // -- AI Suggestion overlay --
            if self.suggestion.is_some() {
                self.build_suggestion_overlay(screen_size, &mut chrome_quads, &mut chrome_glyphs);
            }

            if !chrome_quads.is_empty() || !chrome_glyphs.is_empty() {
                // Upload + render overlay in its own pass on the surface.
                self.quad_renderer.prepare(
                    &self.gpu.device,
                    &self.gpu.queue,
                    &chrome_quads,
                    screen_size,
                );
                self.grid_renderer.prepare(
                    &self.gpu.device,
                    &self.gpu.queue,
                    &chrome_glyphs,
                    screen_size,
                );

                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("system-overlay-pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &surface_view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Load, // preserve the CRT output
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                });

                self.quad_renderer.render(&mut pass);
                self.grid_renderer.render(&mut pass, self.atlas.bind_group());
            }
        }

        // Submit and present.
        self.gpu.queue.submit(std::iter::once(encoder.finish()));
        output.present();

        // Return pooled Vecs for reuse next frame (retains capacity).
        self.pool_quads = all_quads;
        self.pool_glyphs = all_glyphs;
        self.pool_chrome_quads = chrome_quads;
        self.pool_chrome_glyphs = chrome_glyphs;

        Ok(())
    }

    /// Build the command input overlay (post-CRT, crisp).
    fn build_command_overlay(
        &mut self,
        screen_size: [f32; 2],
        quads: &mut Vec<QuadInstance>,
        glyphs: &mut Vec<phantom_renderer::text::GlyphInstance>,
    ) {
        let bar_height = 28.0;
        let y = screen_size[1] - bar_height;
        let cmd_text = self.command_input.as_deref().unwrap_or("");
        let display = format!("> {cmd_text}_");

        // Dark background bar.
        quads.push(QuadInstance {
            pos: [0.0, y],
            size: [screen_size[0], bar_height],
            color: [0.02, 0.02, 0.04, 0.95],
            border_radius: 0.0,
        });

        // Command text.
        let color = [0.2, 1.0, 0.5, 1.0]; // bright green
        let cells: Vec<phantom_renderer::text::TerminalCell> = display
            .chars()
            .map(|ch| phantom_renderer::text::TerminalCell { ch, fg: color })
            .collect();

        if !cells.is_empty() {
            let cols = cells.len();
            let origin = (8.0, y + 4.0);
            let mut g = self.text_renderer.prepare_glyphs(
                &mut self.atlas,
                &self.gpu.queue,
                &cells,
                cols,
                origin,
            );
            glyphs.append(&mut g);
        }
    }

    /// Build the AI suggestion overlay (post-CRT, crisp).
    fn build_suggestion_overlay(
        &mut self,
        screen_size: [f32; 2],
        quads: &mut Vec<QuadInstance>,
        glyphs: &mut Vec<phantom_renderer::text::GlyphInstance>,
    ) {
        let suggestion = match self.suggestion {
            Some(ref s) => s,
            None => return,
        };

        let line_height = 22.0;
        let padding = 12.0;
        let option_lines = suggestion.options.len();
        let total_lines = 1 + option_lines; // text line + option lines
        let box_height = (total_lines as f32) * line_height + padding * 2.0;
        let box_width = 500.0_f32.min(screen_size[0] - 32.0);
        let box_x = (screen_size[0] - box_width) / 2.0;
        let box_y = screen_size[1] - box_height - 60.0; // above status bar

        // Background panel.
        quads.push(QuadInstance {
            pos: [box_x, box_y],
            size: [box_width, box_height],
            color: [0.02, 0.04, 0.08, 0.92],
            border_radius: 4.0,
        });

        // Cyan border.
        for &(pos, size) in &[
            ([box_x, box_y], [box_width, 1.0]),
            ([box_x, box_y + box_height - 1.0], [box_width, 1.0]),
            ([box_x, box_y], [1.0, box_height]),
            ([box_x + box_width - 1.0, box_y], [1.0, box_height]),
        ] {
            quads.push(QuadInstance {
                pos,
                size,
                color: [0.0, 0.8, 0.9, 0.7],
                border_radius: 0.0,
            });
        }

        // Suggestion text.
        let prefix = "[PHANTOM]: ";
        let display = format!("{prefix}{}", suggestion.text);
        let text_color = [0.1, 0.9, 0.6, 1.0];
        let cells: Vec<phantom_renderer::text::TerminalCell> = display
            .chars()
            .map(|ch| phantom_renderer::text::TerminalCell { ch, fg: text_color })
            .collect();

        if !cells.is_empty() {
            let cols = cells.len();
            let origin = (box_x + padding, box_y + padding);
            let mut g = self.text_renderer.prepare_glyphs(
                &mut self.atlas,
                &self.gpu.queue,
                &cells,
                cols,
                origin,
            );
            glyphs.append(&mut g);
        }

        // Option labels.
        let opt_color = [0.6, 0.8, 0.4, 1.0];
        for (i, (key, label)) in suggestion.options.iter().enumerate() {
            let opt_text = format!("  [{key}] {label}");
            let opt_cells: Vec<phantom_renderer::text::TerminalCell> = opt_text
                .chars()
                .map(|ch| phantom_renderer::text::TerminalCell { ch, fg: opt_color })
                .collect();

            if !opt_cells.is_empty() {
                let cols = opt_cells.len();
                let origin = (box_x + padding, box_y + padding + (i as f32 + 1.0) * line_height);
                let mut g = self.text_renderer.prepare_glyphs(
                    &mut self.atlas,
                    &self.gpu.queue,
                    &opt_cells,
                    cols,
                    origin,
                );
                glyphs.append(&mut g);
            }
        }
    }

    /// Build the debug shader HUD overlay (post-CRT, crisp).
    fn build_debug_hud(
        &mut self,
        screen_size: [f32; 2],
        quads: &mut Vec<QuadInstance>,
        glyphs: &mut Vec<phantom_renderer::text::GlyphInstance>,
    ) {
        let sp = &self.theme.shader_params;
        let params: &[(&str, f32)] = &[
            ("scanlines", sp.scanline_intensity),
            ("bloom", sp.bloom_intensity),
            ("aberration", sp.chromatic_aberration),
            ("curvature", sp.curvature),
            ("vignette", sp.vignette_intensity),
            ("noise", sp.noise_intensity),
        ];

        let hud_width = 340.0;
        let line_height = 20.0;
        let hud_height = (params.len() as f32 + 3.0) * line_height;
        let hud_x = screen_size[0] - hud_width - 16.0;
        let hud_y = 16.0;

        // Background panel.
        quads.push(QuadInstance {
            pos: [hud_x, hud_y],
            size: [hud_width, hud_height],
            color: [0.02, 0.02, 0.04, 0.90],
            border_radius: 4.0,
        });

        // Border.
        let border_color = [0.15, 0.4, 0.2, 0.8];
        for &(pos, size) in &[
            ([hud_x, hud_y], [hud_width, 1.0]),
            ([hud_x, hud_y + hud_height - 1.0], [hud_width, 1.0]),
            ([hud_x, hud_y], [1.0, hud_height]),
            ([hud_x + hud_width - 1.0, hud_y], [1.0, hud_height]),
        ] {
            quads.push(QuadInstance {
                pos,
                size,
                color: border_color,
                border_radius: 0.0,
            });
        }

        let mut text_y = hud_y + 6.0;
        let text_x = hud_x + 12.0;

        // Title
        let title = "SHADER DEBUG";
        self.render_overlay_text(title, text_x, text_y, [0.55, 1.0, 0.72, 1.0], glyphs);
        text_y += line_height;

        // Separator
        let sep = "────────────────────────────────────";
        self.render_overlay_text(sep, text_x, text_y, [0.15, 0.4, 0.2, 0.5], glyphs);
        text_y += line_height;

        // Param lines
        for (i, &(name, value)) in params.iter().enumerate() {
            let selected = i == self.debug_hud_selected;
            let bar_len = 20;
            let filled = ((value * bar_len as f32).round() as usize).min(bar_len);
            let empty = bar_len - filled;
            let bar: String = "█".repeat(filled) + &"░".repeat(empty);
            let marker = if selected { "▶" } else { " " };
            let line = format!("{marker} {name:<14} {bar} {value:.2}");

            let color = if selected {
                [0.2, 1.0, 0.5, 1.0]
            } else {
                [0.5, 0.7, 0.5, 0.8]
            };
            self.render_overlay_text(&line, text_x, text_y, color, glyphs);
            text_y += line_height;
        }

        // Help line
        text_y += 4.0;
        let help = "[Tab] next  [↑↓] adjust  [Esc] close";
        self.render_overlay_text(help, text_x, text_y, [0.3, 0.5, 0.3, 0.6], glyphs);
    }

    /// Helper: render a text string directly into the overlay glyph buffer.
    fn render_overlay_text(
        &mut self,
        text: &str,
        x: f32,
        y: f32,
        color: [f32; 4],
        glyphs: &mut Vec<phantom_renderer::text::GlyphInstance>,
    ) {
        let cells: Vec<phantom_renderer::text::TerminalCell> = text
            .chars()
            .map(|ch| phantom_renderer::text::TerminalCell { ch, fg: color })
            .collect();
        if !cells.is_empty() {
            let cols = cells.len();
            let mut g = self.text_renderer.prepare_glyphs(
                &mut self.atlas,
                &self.gpu.queue,
                &cells,
                cols,
                (x, y),
            );
            glyphs.append(&mut g);
        }
    }

    // -----------------------------------------------------------------------
    // Boot rendering
    // -----------------------------------------------------------------------

    /// Build quads and glyphs for the boot sequence.
    fn render_boot(
        &mut self,
        _screen_size: [f32; 2],
        _quads: &mut Vec<QuadInstance>,
        glyphs: &mut Vec<phantom_renderer::text::GlyphInstance>,
    ) {
        let boot_lines = self.boot.visible_text();
        let opacity = self.boot.screen_opacity();

        // Convert boot text lines to grid cells and render them.
        // Each BootTextLine has a row, text, color, and chars_visible.
        // We render them as positioned text using the text renderer.
        for line in &boot_lines {
            if line.chars_visible == 0 {
                continue;
            }

            // Truncate text to chars_visible for the typewriter effect.
            let visible_text: String = line.text.chars().take(line.chars_visible).collect();

            // Convert to TerminalCells for the text renderer.
            let cells: Vec<phantom_renderer::text::TerminalCell> = visible_text
                .chars()
                .map(|ch| phantom_renderer::text::TerminalCell {
                    ch,
                    fg: [
                        line.color[0],
                        line.color[1],
                        line.color[2],
                        line.color[3] * opacity,
                    ],
                })
                .collect();

            if cells.is_empty() {
                continue;
            }

            let cols = cells.len();
            let origin = (
                self.cell_size.0 * 2.0, // left margin
                self.cell_size.1 * line.row as f32,
            );

            let mut line_glyphs = self.text_renderer.prepare_glyphs(
                &mut self.atlas,
                &self.gpu.queue,
                &cells,
                cols,
                origin,
            );

            glyphs.append(&mut line_glyphs);
        }
    }

    // -----------------------------------------------------------------------
    // Terminal rendering
    // -----------------------------------------------------------------------

    /// Build quads and glyphs for all terminal panes plus chrome.
    fn render_terminal(
        &mut self,
        screen_size: [f32; 2],
        quads: &mut Vec<QuadInstance>,
        glyphs: &mut Vec<phantom_renderer::text::GlyphInstance>,
        chrome_quads: &mut Vec<QuadInstance>,
        chrome_glyphs: &mut Vec<phantom_renderer::text::GlyphInstance>,
    ) {
        let _has_multiple = self.panes.len() > 1;
        let mut detached_labels: Vec<(String, f32, f32, [f32; 4])> = Vec::new();
        let mut container_titles: Vec<(String, f32, f32, [f32; 4])> = Vec::new();

        // Build theme-aware color mapping for terminal grid extraction.
        let theme_colors = TerminalThemeColors {
            foreground: self.theme.colors.foreground,
            background: self.theme.colors.background,
            cursor: self.theme.colors.cursor,
            ansi: Some(self.theme.colors.ansi),
        };

        for (pane_index, pane) in self.panes.iter().enumerate() {
            let is_focused = pane_index == self.focused_pane;

            // -- Extract terminal grid with theme colors --
            let (render_cells, cols, rows, cursor) =
                output::extract_grid_themed(pane.terminal.term(), &theme_colors);

            // -- Get pane rectangle from layout --
            let layout_rect = self.layout.get_pane_rect(pane.pane_id).unwrap_or_else(|e| {
                warn!("Layout missing for pane {:?} in render: {e}", pane.pane_id);
                phantom_ui::layout::Rect {
                    x: 0.0,
                    y: 30.0,
                    width: screen_size[0],
                    height: screen_size[1] - 54.0,
                }
            });

            // Inset by outer margin to create the "floating container" look.
            let pane_rect = container_rect(layout_rect, self.cell_size);

            // -- Inner rect: area inside container chrome where the grid draws --
            let inner_rect = pane_inner_rect(self.cell_size, pane_rect);
            let origin = (inner_rect.x, inner_rect.y);

            // -- App-container chrome (routed to overlay pass — post-CRT) ----
            // Non-detached panes only; detached panes have their own (cyan) chrome.
            if !pane.is_detached {
                let bg = self.theme.colors.background;
                // Noticeably lighter than theme bg — container must read as
                // a distinct surface sitting on the window backdrop.
                let cont_bg = [
                    (bg[0] + 0.06).min(1.0),
                    (bg[1] + 0.10).min(1.0),
                    (bg[2] + 0.06).min(1.0),
                    1.0,
                ];

                // Drop shadow: slightly darker quad offset behind the container.
                chrome_quads.push(QuadInstance {
                    pos: [pane_rect.x + 3.0, pane_rect.y + 3.0],
                    size: [pane_rect.width, pane_rect.height],
                    color: [0.0, 0.0, 0.0, 0.35],
                    border_radius: 6.0,
                });

                // Container background.
                chrome_quads.push(QuadInstance {
                    pos: [pane_rect.x, pane_rect.y],
                    size: [pane_rect.width, pane_rect.height],
                    color: cont_bg,
                    border_radius: 6.0,
                });

                // Title strip across the top of the container.
                let title_h = self.cell_size.1 * CONTAINER_TITLE_H_CELLS;
                let title_bg = if is_focused {
                    [bg[0] * 1.6 + 0.04, bg[1] * 2.0 + 0.06, bg[2] * 1.6 + 0.04, 1.0]
                } else {
                    [bg[0] * 1.3 + 0.02, bg[1] * 1.5 + 0.03, bg[2] * 1.3 + 0.02, 1.0]
                };
                chrome_quads.push(QuadInstance {
                    pos: [pane_rect.x, pane_rect.y],
                    size: [pane_rect.width, title_h],
                    color: title_bg,
                    border_radius: 6.0,
                });

                // Focus-aware border around the entire container.
                let border_color = if is_focused {
                    [0.2, 1.0, 0.5, 0.85]
                } else {
                    [0.15, 0.25, 0.18, 0.60]
                };
                let t = 1.0;
                // top
                chrome_quads.push(QuadInstance { pos: [pane_rect.x, pane_rect.y], size: [pane_rect.width, t], color: border_color, border_radius: 0.0 });
                // bottom
                chrome_quads.push(QuadInstance { pos: [pane_rect.x, pane_rect.y + pane_rect.height - t], size: [pane_rect.width, t], color: border_color, border_radius: 0.0 });
                // left
                chrome_quads.push(QuadInstance { pos: [pane_rect.x, pane_rect.y], size: [t, pane_rect.height], color: border_color, border_radius: 0.0 });
                // right
                chrome_quads.push(QuadInstance { pos: [pane_rect.x + pane_rect.width - t, pane_rect.y], size: [t, pane_rect.height], color: border_color, border_radius: 0.0 });

                // Title text: "● shell · {cols}×{rows}"
                let dot_color = if is_focused {
                    [0.2, 1.0, 0.5, 1.0]
                } else {
                    [0.4, 0.5, 0.4, 0.7]
                };
                let title_text = format!("● shell  {}×{}", cols, rows);
                let title_x = pane_rect.x + self.cell_size.0 * CONTAINER_PAD_X_CELLS;
                let title_y = pane_rect.y + (title_h - self.cell_size.1) * 0.5;
                container_titles.push((title_text, title_x, title_y, dot_color));
            }

            // -- Convert RenderCells to GridCells --
            let grid_cells: Vec<GridCell> = render_cells
                .iter()
                .map(|rc| GridCell {
                    ch: rc.ch,
                    fg: rc.fg,
                    bg: rc.bg,
                })
                .collect();

            // -- Prepare background quads + glyph instances via GridRenderData --
            let (mut bg_quads, mut glyph_instances) = GridRenderData::prepare(
                &grid_cells,
                cols,
                rows,
                &mut self.text_renderer,
                &mut self.atlas,
                &self.gpu.queue,
                origin,
                self.cell_size,
            );

            quads.append(&mut bg_quads);
            glyphs.append(&mut glyph_instances);

            // -- Cursor quad (only for focused pane, or dim for unfocused) --
            if cursor.visible {
                let cursor_x = inner_rect.x + cursor.col as f32 * self.cell_size.0;
                let cursor_y = inner_rect.y + cursor.row as f32 * self.cell_size.1;
                let cursor_color = if is_focused {
                    self.theme.colors.cursor
                } else {
                    // Dim cursor for unfocused panes.
                    let c = self.theme.colors.cursor;
                    [c[0] * 0.3, c[1] * 0.3, c[2] * 0.3, c[3] * 0.4]
                };

                let cursor_quad = match cursor.shape {
                    CursorShape::Block => QuadInstance {
                        pos: [cursor_x, cursor_y],
                        size: [self.cell_size.0, self.cell_size.1],
                        color: [cursor_color[0], cursor_color[1], cursor_color[2], if is_focused { 0.7 } else { 0.3 }],
                        border_radius: 0.0,
                    },
                    CursorShape::Underline => QuadInstance {
                        pos: [cursor_x, cursor_y + self.cell_size.1 - 2.0],
                        size: [self.cell_size.0, 2.0],
                        color: cursor_color,
                        border_radius: 0.0,
                    },
                    CursorShape::Bar => QuadInstance {
                        pos: [cursor_x, cursor_y],
                        size: [2.0, self.cell_size.1],
                        color: cursor_color,
                        border_radius: 0.0,
                    },
                };
                quads.push(cursor_quad);
            }

            // -- Pane border --
            // Detached panes always get an animated cyan tether border + header.
            // Non-detached panes get the standard border when multiple panes exist.
            if pane.is_detached {
                let elapsed = self.start_time.elapsed().as_secs_f32();
                let pulse = (elapsed * 2.0).sin() * 0.15 + 0.85;
                let border_color = [0.0, pulse, pulse * 0.8, 0.9];
                let border_thickness = 2.0;

                // Top edge
                quads.push(QuadInstance {
                    pos: [pane_rect.x, pane_rect.y],
                    size: [pane_rect.width, border_thickness],
                    color: border_color,
                    border_radius: 0.0,
                });
                // Bottom edge
                quads.push(QuadInstance {
                    pos: [pane_rect.x, pane_rect.y + pane_rect.height - border_thickness],
                    size: [pane_rect.width, border_thickness],
                    color: border_color,
                    border_radius: 0.0,
                });
                // Left edge
                quads.push(QuadInstance {
                    pos: [pane_rect.x, pane_rect.y],
                    size: [border_thickness, pane_rect.height],
                    color: border_color,
                    border_radius: 0.0,
                });
                // Right edge
                quads.push(QuadInstance {
                    pos: [pane_rect.x + pane_rect.width - border_thickness, pane_rect.y],
                    size: [border_thickness, pane_rect.height],
                    color: border_color,
                    border_radius: 0.0,
                });

                // -- Detach header bar --
                let header_height = self.cell_size.1 + 4.0;
                // Dark semi-transparent header background.
                quads.push(QuadInstance {
                    pos: [pane_rect.x + border_thickness, pane_rect.y + border_thickness],
                    size: [pane_rect.width - border_thickness * 2.0, header_height],
                    color: [0.02, 0.06, 0.08, 0.85],
                    border_radius: 0.0,
                });

                // Tether indicator dot (animated).
                let dot_size = 6.0;
                let dot_x = pane_rect.x + border_thickness + 6.0;
                let dot_y = pane_rect.y + border_thickness + (header_height - dot_size) / 2.0;
                quads.push(QuadInstance {
                    pos: [dot_x, dot_y],
                    size: [dot_size, dot_size],
                    color: [0.0, pulse, pulse * 0.8, 1.0],
                    border_radius: 3.0,
                });

                // Process name label.
                let label = format!("  {} ", &pane.detached_label);
                let label_x = dot_x + dot_size + 4.0;
                let label_y = pane_rect.y + border_thickness + 2.0;
                let label_color = [0.0, pulse, pulse * 0.8, 1.0];

                detached_labels.push((label, label_x, label_y, label_color));
            }
            // Note: non-detached pane borders are now drawn as part of the
            // app-container chrome at the top of the pane loop.
        }

        // -- Detached pane labels (rendered after the pane loop to avoid borrow issues) --
        for (label, x, y, color) in &detached_labels {
            self.render_overlay_text(label, *x, *y, *color, glyphs);
        }

        // -- App-container title text (routed to chrome overlay — post-CRT) --
        for (label, x, y, color) in &container_titles {
            self.render_overlay_text(label, *x, *y, *color, chrome_glyphs);
        }

        // -- Tab bar --
        if let Ok(tab_rect) = self.layout.get_tab_bar_rect() {
            let tab_quads = self.tab_bar.render_quads(&tab_rect);
            quads.extend(tab_quads);

            // Render tab bar text as glyphs.
            let tab_texts = self.tab_bar.render_text(&tab_rect);
            self.render_text_segments(&tab_texts, glyphs);
        }

        // -- Status bar --
        if let Ok(status_rect) = self.layout.get_status_bar_rect() {
            let status_quads = self.status_bar.render_quads(&status_rect);
            quads.extend(status_quads);

            // Render status bar text as glyphs.
            let status_texts = self.status_bar.render_text(&status_rect);
            self.render_text_segments(&status_texts, glyphs);

            // Command overlay moved to system overlay pass (post-CRT).
        }
    }

    /// Convert widget TextSegments into GlyphInstances via the text renderer.
    fn render_text_segments(
        &mut self,
        segments: &[phantom_ui::widgets::TextSegment],
        glyphs: &mut Vec<phantom_renderer::text::GlyphInstance>,
    ) {
        for seg in segments {
            let cells: Vec<phantom_renderer::text::TerminalCell> = seg
                .text
                .chars()
                .map(|ch| phantom_renderer::text::TerminalCell {
                    ch,
                    fg: seg.color,
                })
                .collect();

            if cells.is_empty() {
                continue;
            }

            let cols = cells.len();
            let origin = (seg.x, seg.y);

            let mut seg_glyphs = self.text_renderer.prepare_glyphs(
                &mut self.atlas,
                &self.gpu.queue,
                &cells,
                cols,
                origin,
            );

            glyphs.append(&mut seg_glyphs);
        }
    }
}

// ---------------------------------------------------------------------------
// Supervisor integration & command mode
// ---------------------------------------------------------------------------

impl App {
    /// Handle a key press while in command mode.
    ///
    /// Printable chars are appended to the command buffer. Enter executes,
    /// Escape cancels, Backspace deletes the last char.
    fn handle_command_mode_key(&mut self, event: &winit::event::KeyEvent) {
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => {
                debug!("Command mode cancelled");
                self.command_mode = false;
                self.command_input = None;
            }
            Key::Named(NamedKey::Enter) => {
                let input = self.command_input.take().unwrap_or_default();
                self.command_mode = false;
                if !input.is_empty() {
                    self.execute_user_command(&input);
                }
            }
            Key::Named(NamedKey::Backspace) => {
                if let Some(ref mut buf) = self.command_input {
                    buf.pop();
                }
            }
            Key::Character(s) => {
                if let Some(ref mut buf) = self.command_input {
                    buf.push_str(s.as_str());
                }
            }
            _ => {}
        }
    }

    /// Parse and execute a user command string entered via command mode.
    /// Handle keys when the debug shader HUD is open.
    fn handle_debug_hud_key(&mut self, event: &winit::event::KeyEvent) {
        const PARAM_COUNT: usize = 6;
        const STEP: f32 = 0.01;

        match &event.logical_key {
            Key::Named(NamedKey::Escape) => {
                self.debug_hud = false;
            }
            Key::Named(NamedKey::Tab) => {
                self.debug_hud_selected = (self.debug_hud_selected + 1) % PARAM_COUNT;
            }
            Key::Named(NamedKey::ArrowUp) => {
                self.adjust_debug_param(STEP);
            }
            Key::Named(NamedKey::ArrowDown) => {
                self.adjust_debug_param(-STEP);
            }
            Key::Named(NamedKey::ArrowRight) => {
                self.adjust_debug_param(STEP * 5.0);
            }
            Key::Named(NamedKey::ArrowLeft) => {
                self.adjust_debug_param(-STEP * 5.0);
            }
            _ => {}
        }
    }

    /// Adjust the currently selected debug HUD shader parameter.
    fn adjust_debug_param(&mut self, delta: f32) {
        let sp = &mut self.theme.shader_params;
        let val = match self.debug_hud_selected {
            0 => &mut sp.scanline_intensity,
            1 => &mut sp.bloom_intensity,
            2 => &mut sp.chromatic_aberration,
            3 => &mut sp.curvature,
            4 => &mut sp.vignette_intensity,
            5 => &mut sp.noise_intensity,
            _ => return,
        };
        *val = (*val + delta).clamp(0.0, 1.0);
    }

}

// ---------------------------------------------------------------------------
// Winit key conversion
// ---------------------------------------------------------------------------

/// Convert a winit `KeyEvent` to a `KeyCombo` for keybind registry lookup.
///
/// Returns `None` if the key event cannot be mapped to a combo (e.g. unknown
/// keys or modifier-only presses).
fn winit_key_to_combo(event: &winit::event::KeyEvent) -> Option<KeyCombo> {
    let ui_key = winit_logical_to_ui_key(&event.logical_key)?;

    // Extract modifier state from the event. In winit 0.30+, modifiers are
    // tracked separately via ModifiersChanged. We check the key text to
    // detect Shift (uppercase letter), but Ctrl/Alt/Logo come from the
    // event_loop's tracked state. For now we derive what we can.
    //
    // The actual modifier state is embedded in the logical key for character
    // keys (e.g. Shift+A produces Key::Character("A")), so we need the
    // raw physical modifiers. We encode Ctrl/Alt/Logo from Named keys and
    // rely on the caller to improve this with tracked modifier state.
    //
    // For correctness with Cmd+<key> bindings (the primary use case), winit
    // strips the logo modifier from the logical key text, so Cmd+C shows
    // as Key::Character("c") with the logo modifier active. We need the
    // raw modifier state which isn't on KeyEvent directly. Instead, we
    // check if the logical key matches a character while tracking modifiers.
    //
    // Workaround: check if character key came with no text (dead key) or
    // with modified text. For now, return bare combos and let the event loop
    // forward tracked modifiers. This is improved in handle_key_with_mods.

    Some(KeyCombo::bare(ui_key))
}

/// Improved key combo extraction that incorporates externally tracked modifiers.
pub fn winit_key_to_combo_with_mods(
    event: &winit::event::KeyEvent,
    modifiers: winit::event::Modifiers,
) -> Option<KeyCombo> {
    let ui_key = winit_logical_to_ui_key(&event.logical_key)?;

    let state = modifiers.state();
    Some(KeyCombo {
        key: ui_key,
        ctrl: state.control_key(),
        alt: state.alt_key(),
        shift: state.shift_key(),
        logo: state.super_key(),
    })
}

/// Map a winit logical key to our UI key enum.
fn winit_logical_to_ui_key(key: &Key) -> Option<UiKey> {
    match key {
        Key::Character(s) => {
            let ch = s.chars().next()?;
            // Normalize to lowercase for keybind matching.
            let ch_lower = ch.to_ascii_lowercase();
            Some(UiKey::Char(ch_lower))
        }
        Key::Named(named) => match named {
            NamedKey::Enter => Some(UiKey::Enter),
            NamedKey::Tab => Some(UiKey::Tab),
            NamedKey::Space => Some(UiKey::Space),
            NamedKey::Backspace => Some(UiKey::Backspace),
            NamedKey::Delete => Some(UiKey::Delete),
            NamedKey::Insert => Some(UiKey::Insert),
            NamedKey::Escape => Some(UiKey::Escape),
            NamedKey::ArrowUp => Some(UiKey::Up),
            NamedKey::ArrowDown => Some(UiKey::Down),
            NamedKey::ArrowLeft => Some(UiKey::Left),
            NamedKey::ArrowRight => Some(UiKey::Right),
            NamedKey::Home => Some(UiKey::Home),
            NamedKey::End => Some(UiKey::End),
            NamedKey::PageUp => Some(UiKey::PageUp),
            NamedKey::PageDown => Some(UiKey::PageDown),
            NamedKey::F1 => Some(UiKey::F(1)),
            NamedKey::F2 => Some(UiKey::F(2)),
            NamedKey::F3 => Some(UiKey::F(3)),
            NamedKey::F4 => Some(UiKey::F(4)),
            NamedKey::F5 => Some(UiKey::F(5)),
            NamedKey::F6 => Some(UiKey::F(6)),
            NamedKey::F7 => Some(UiKey::F(7)),
            NamedKey::F8 => Some(UiKey::F(8)),
            NamedKey::F9 => Some(UiKey::F(9)),
            NamedKey::F10 => Some(UiKey::F(10)),
            NamedKey::F11 => Some(UiKey::F(11)),
            NamedKey::F12 => Some(UiKey::F(12)),
            _ => None,
        },
        _ => None,
    }
}

/// Convert a winit `KeyEvent` to a terminal `KeyEvent` for PTY encoding.
///
/// Returns `None` for keys that have no terminal encoding (e.g. bare
/// modifier presses, media keys).
fn winit_key_to_terminal(event: &winit::event::KeyEvent) -> Option<KeyEvent> {
    let phantom_key = match &event.logical_key {
        Key::Character(s) => {
            let ch = s.chars().next()?;
            PhantomKey::Char(ch)
        }
        Key::Named(named) => match named {
            NamedKey::Enter => PhantomKey::Enter,
            NamedKey::Backspace => PhantomKey::Backspace,
            NamedKey::Tab => PhantomKey::Tab,
            NamedKey::Escape => PhantomKey::Escape,
            NamedKey::ArrowUp => PhantomKey::Up,
            NamedKey::ArrowDown => PhantomKey::Down,
            NamedKey::ArrowLeft => PhantomKey::Left,
            NamedKey::ArrowRight => PhantomKey::Right,
            NamedKey::Home => PhantomKey::Home,
            NamedKey::End => PhantomKey::End,
            NamedKey::PageUp => PhantomKey::PageUp,
            NamedKey::PageDown => PhantomKey::PageDown,
            NamedKey::Delete => PhantomKey::Delete,
            NamedKey::Insert => PhantomKey::Insert,
            NamedKey::F1 => PhantomKey::F(1),
            NamedKey::F2 => PhantomKey::F(2),
            NamedKey::F3 => PhantomKey::F(3),
            NamedKey::F4 => PhantomKey::F(4),
            NamedKey::F5 => PhantomKey::F(5),
            NamedKey::F6 => PhantomKey::F(6),
            NamedKey::F7 => PhantomKey::F(7),
            NamedKey::F8 => PhantomKey::F(8),
            NamedKey::F9 => PhantomKey::F(9),
            NamedKey::F10 => PhantomKey::F(10),
            NamedKey::F11 => PhantomKey::F(11),
            NamedKey::F12 => PhantomKey::F(12),
            NamedKey::Space => PhantomKey::Char(' '),
            _ => return None,
        },
        _ => return None,
    };

    // Derive modifiers from the key event text. In winit, when Ctrl is held,
    // the logical key text is modified (e.g. Ctrl+C produces '\x03' as text).
    // We need to check the physical key to detect modifiers properly.
    // For now, we use a heuristic based on the text field.
    let mods = PhantomModifiers::NONE;

    Some(KeyEvent {
        key: phantom_key,
        mods,
    })
}

// ---------------------------------------------------------------------------
// Extended key handling with full modifier support
// ---------------------------------------------------------------------------

impl App {
    /// Handle a keyboard event with externally tracked modifier state.
    ///
    /// This is the preferred entry point for keyboard input when the caller
    /// tracks modifier state via `WindowEvent::ModifiersChanged`.
    pub fn handle_key_with_mods(
        &mut self,
        event: winit::event::KeyEvent,
        modifiers: winit::event::Modifiers,
    ) {
        if !event.state.is_pressed() {
            return;
        }

        // Track last input for idle detection.
        self.last_input_time = Instant::now();

        // Dismiss active suggestion on any keypress.
        if self.suggestion.is_some() {
            self.suggestion = None;
        }

        // -- Debug HUD input handling --
        if self.debug_hud {
            self.handle_debug_hud_key(&event);
            return;
        }

        // -- Command mode input handling --
        if self.command_mode {
            self.handle_command_mode_key(&event);
            return;
        }

        // Backtick (`) toggles command mode on (only without modifiers).
        if !modifiers.state().control_key()
            && !modifiers.state().alt_key()
            && !modifiers.state().super_key()
        {
            if matches!(&event.logical_key, Key::Character(s) if s.as_str() == "`") {
                self.command_mode = true;
                self.command_input = Some(String::new());
                debug!("Command mode activated");
                return;
            }
        }

        // Check keybind registry with full modifier state.
        if let Some(combo) = winit_key_to_combo_with_mods(&event, modifiers) {
            if let Some(action) = self.keybinds.lookup(&combo) {
                self.dispatch_action(*action);
                return;
            }
        }

        // During boot, any key press skips.
        if self.state == AppState::Boot {
            self.boot.skip();
            return;
        }

        // Build terminal key event with proper modifiers and write to focused pane.
        if let Some(mut terminal_event) = winit_key_to_terminal(&event) {
            let state = modifiers.state();
            terminal_event.mods = PhantomModifiers {
                ctrl: state.control_key(),
                alt: state.alt_key(),
                shift: state.shift_key(),
                logo: state.super_key(),
            };

            let bytes = input::encode_key(&terminal_event);
            if !bytes.is_empty() {
                if let Some(pane) = self.panes.get_mut(self.focused_pane) {
                    if let Err(e) = pane.terminal.pty_write(&bytes) {
                        warn!("PTY write failed: {e}");
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Utility
// ---------------------------------------------------------------------------

/// Get the current wall clock time as a `HH:MM` string.
///
/// Uses basic system time rather than pulling in a full datetime crate.
fn chrono_time_string() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    // Convert to hours:minutes in local time (rough UTC offset not applied --
    // a proper implementation would use libc::localtime_r, but this is enough
    // for the status bar until we pull in chrono or time).
    let day_secs = secs % 86400;
    let hours = day_secs / 3600;
    let minutes = (day_secs % 3600) / 60;

    format!("{hours:02}:{minutes:02}")
}
