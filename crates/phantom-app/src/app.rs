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

use phantom_protocol::AppMessage;
use phantom_renderer::atlas::GlyphAtlas;
use phantom_renderer::gpu::GpuContext;
use phantom_renderer::grid::GridRenderer;
use phantom_renderer::postfx::PostFxPipeline;
use phantom_renderer::quads::{QuadInstance, QuadRenderer};
use phantom_renderer::text::TextRenderer;

use phantom_terminal::terminal::PhantomTerminal;

use phantom_ui::keybinds::KeybindRegistry;
use phantom_ui::layout::LayoutEngine;
use phantom_ui::themes::Theme;
use phantom_ui::widgets::{StatusBar, TabBar};

use phantom_adapter::{EventBus, TopicId, DataType};
use phantom_brain::brain::{BrainConfig, BrainHandle, spawn_brain};
use phantom_brain::events::AiEvent;
use phantom_context::ProjectContext;
use phantom_memory::MemoryStore;
use phantom_plugins::PluginRegistry;
use phantom_scene::node::{NodeKind, RenderLayer};
use phantom_scene::tree::SceneTree;
use phantom_session::session::{SessionManager, SessionState, PaneState};
use phantom_mcp::{spawn_listener, AppCommand, McpListener};

use crate::boot::BootSequence;
use crate::config::PhantomConfig;
use crate::pane::{Pane, pane_cols_rows};
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
    pub(crate) scene_content_node: phantom_scene::node::NodeId,

    // -- MCP listener (Unix socket) and inbound command channel --
    pub(crate) mcp_cmd_rx: mpsc::Receiver<AppCommand>,
    pub(crate) _mcp_listener: Option<McpListener>,

    // -- Render pools (reused each frame via clear() to avoid per-frame allocs) --
    pub(crate) pool_quads: Vec<QuadInstance>,
    pub(crate) pool_glyphs: Vec<phantom_renderer::text::GlyphInstance>,
    pub(crate) pool_chrome_quads: Vec<QuadInstance>,
    pub(crate) pool_chrome_glyphs: Vec<phantom_renderer::text::GlyphInstance>,

    // -- Agent panes (AI workers running in visible panes) --
    pub(crate) agent_panes: Vec<crate::agent_pane::AgentPane>,

    // -- Event bus (pub/sub between subsystems) --
    pub(crate) event_bus: EventBus,
    pub(crate) topic_terminal_output: TopicId,
    pub(crate) topic_terminal_error: TopicId,
    pub(crate) topic_agent_event: TopicId,

    // -- Plugin registry --
    pub(crate) plugin_registry: PluginRegistry,

    // -- System resource monitor --
    pub(crate) sysmon: crate::sysmon::SysmonHandle,
    pub(crate) sysmon_visible: bool,
    pub(crate) appmon_visible: bool,

    // -- Mouse state --
    pub(crate) cursor_position: (f64, f64),
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
        // scene_node will be set after the scene graph is created below.
        let panes = vec![Pane {
            terminal,
            pane_id,
            scene_node: 0, // placeholder; set after scene graph init
            was_alt_screen: false,
            is_detached: false,
            detached_label: String::new(),
            output_buf: String::new(),
            error_notified: false,
        }];

        // -- Keybinds --
        let keybinds = KeybindRegistry::new();

        // -- Theme (from config, with shader overrides) --
        let mut theme = config.resolve_theme();

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
        // First pane under content area — linked to panes[0].
        let first_pane_node = scene.add_node(content_node, NodeKind::Pane);
        // Overlay nodes (rendered after CRT post-fx).
        let cmd_bar_node = scene.add_node(root, NodeKind::CommandBar);
        if let Some(n) = scene.get_mut(cmd_bar_node) { n.render_layer = RenderLayer::Overlay; }
        let debug_hud_node = scene.add_node(root, NodeKind::DebugHud);
        if let Some(n) = scene.get_mut(debug_hud_node) { n.render_layer = RenderLayer::Overlay; }
        let suggestion_node = scene.add_node(root, NodeKind::AgentSuggestion);
        if let Some(n) = scene.get_mut(suggestion_node) { n.render_layer = RenderLayer::Overlay; }
        let _ = (cmd_bar_node, debug_hud_node, suggestion_node); // suppress unused
        // Set initial transforms from layout.
        scene.set_transform(root, 0.0, 0.0, width as f32, height as f32);
        // Sync first pane's scene transform from the layout engine.
        if let Ok(rect) = layout.get_pane_rect(pane_id) {
            scene.set_transform(first_pane_node, rect.x, rect.y, rect.width, rect.height);
        }
        scene.update_world_transforms();
        info!("Scene graph initialized: {} nodes", scene.node_count());

        // Link scene graph pane node back to the first pane.
        let panes = {
            let mut p = panes;
            p[0].scene_node = first_pane_node;
            p
        };

        // -- Session manager + restore --
        let session_manager = match SessionManager::new() {
            Ok(sm) => Some(sm),
            Err(e) => {
                warn!("Failed to create session manager: {e}");
                None
            }
        };

        // Try to restore the most recent session for this project.
        if let Some(ref sm) = session_manager {
            match sm.load_latest(&project_dir) {
                Ok(Some(prev)) => {
                    // Restore theme from previous session, preserving config
                    // shader overrides by re-resolving through the config path.
                    if prev.theme_name != config.theme_name {
                        let mut restore_config = config.clone();
                        restore_config.theme_name = prev.theme_name.to_ascii_lowercase();
                        theme = restore_config.resolve_theme();
                        info!("Session restored theme: {}", prev.theme_name);
                    }
                    // Restore CRT shader params from session (debug HUD tuning etc.)
                    if let Some(ref sp) = prev.shader_params {
                        theme.shader_params.scanline_intensity = sp.scanline_intensity;
                        theme.shader_params.bloom_intensity = sp.bloom_intensity;
                        theme.shader_params.chromatic_aberration = sp.chromatic_aberration;
                        theme.shader_params.curvature = sp.curvature;
                        theme.shader_params.vignette_intensity = sp.vignette_intensity;
                        theme.shader_params.noise_intensity = sp.noise_intensity;
                        info!(
                            "Session restored CRT: scanlines={:.2} bloom={:.2} curve={:.2}",
                            sp.scanline_intensity, sp.bloom_intensity, sp.curvature
                        );
                    }
                    let welcome = SessionManager::welcome_message(&prev);
                    info!("{welcome}");
                    status_bar.set_activity(&welcome);
                }
                Ok(None) => {
                    info!("No previous session for this project");
                }
                Err(e) => {
                    warn!("Failed to load session: {e}");
                }
            }
        }

        // -- Event bus --
        let mut event_bus = EventBus::new();
        let topic_terminal_output = event_bus.create_topic(0, "terminal.output", DataType::TerminalOutput);
        let topic_terminal_error = event_bus.create_topic(0, "terminal.error", DataType::Text);
        let topic_agent_event = event_bus.create_topic(0, "agent.event", DataType::Json);
        info!("Event bus initialized: {} topics", event_bus.topic_count());

        // -- Plugin registry --
        let plugin_registry = match PluginRegistry::new() {
            Ok(reg) => reg,
            Err(e) => {
                warn!("Failed to create plugin registry: {e}");
                match PluginRegistry::with_dir(std::env::temp_dir().join("phantom-plugins-fallback")) {
                    Ok(reg) => reg,
                    Err(e2) => {
                        warn!("Failed to create fallback plugin registry: {e2}");
                        // Return empty registry — plugins disabled but app works.
                        PluginRegistry::with_dir(std::env::temp_dir())
                            .unwrap_or_else(|_| {
                                // Last resort — should never fail on /tmp.
                                panic!("cannot create plugin registry in /tmp");
                            })
                    }
                }
            }
        };

        // -- System monitor --
        let sysmon = crate::sysmon::spawn_sysmon();

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
            scene_content_node: content_node,
            mcp_cmd_rx,
            _mcp_listener: mcp_listener,
            pool_quads: Vec::with_capacity(256),
            pool_glyphs: Vec::with_capacity(4096),
            pool_chrome_quads: Vec::with_capacity(32),
            pool_chrome_glyphs: Vec::with_capacity(256),
            agent_panes: Vec::new(),
            event_bus,
            topic_terminal_output,
            topic_terminal_error,
            topic_agent_event,
            plugin_registry,
            sysmon,
            sysmon_visible: false,
            appmon_visible: false,
            cursor_position: (0.0, 0.0),
        })
    }

    /// Returns `true` if the app has requested to quit.
    pub fn should_quit(&self) -> bool {
        self.quit_requested
    }

    /// Graceful shutdown: save session, dispatch plugin hooks, shut down brain.
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

        // Dispatch shutdown hooks to plugins.
        let wd = self.context.as_ref()
            .map(|c| c.root.clone())
            .unwrap_or_else(|| ".".into());
        let ctx = phantom_plugins::HookContext::shutdown(&wd);
        let responses = self.plugin_registry.dispatch_hook(
            &phantom_plugins::HookType::OnShutdown,
            &ctx,
        );
        for resp in &responses {
            info!("[plugin shutdown]: {resp:?}");
        }
        self.plugin_registry.shutdown_all();

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

        let sp = &self.theme.shader_params;
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
            shader_params: Some(phantom_session::session::SavedShaderParams {
                scanline_intensity: sp.scanline_intensity,
                bloom_intensity: sp.bloom_intensity,
                chromatic_aberration: sp.chromatic_aberration,
                curvature: sp.curvature,
                vignette_intensity: sp.vignette_intensity,
                noise_intensity: sp.noise_intensity,
            }),
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
    // Update
    // -----------------------------------------------------------------------
    // Render (see render.rs for the main rendering pipeline)
    // Update (see update.rs for the per-frame update loop)
    // Input (see input.rs for keyboard handling)
    // Commands (see commands.rs for user command execution)
    // Pane management (see pane.rs for split/close)
    // -----------------------------------------------------------------------
}
