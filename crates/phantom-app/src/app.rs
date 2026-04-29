//! Main application orchestrator for Phantom.
//!
//! The [`App`] struct owns every subsystem -- GPU, terminal, layout, theming,
//! widgets, and the boot sequence -- and drives the per-frame update/render
//! loop. It is created after the window and GPU context are established and
//! handed control for the lifetime of the application.

use std::collections::VecDeque;
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
use phantom_renderer::video::VideoRenderer;

use phantom_terminal::terminal::PhantomTerminal;

use phantom_ui::keybinds::KeybindRegistry;
use phantom_ui::layout::LayoutEngine;
use phantom_ui::themes::Theme;
use phantom_ui::widgets::{StatusBar, TabBar};

use phantom_adapter::{DataType, EventBus, TopicId};
use phantom_brain::brain::{BrainConfig, BrainHandle, spawn_brain};
use phantom_brain::events::AiEvent;
use phantom_brain::ooda::{OodaConfig, OodaLoop};
use phantom_context::ProjectContext;
use phantom_history::{AgentOutputCapture, HistoryStore};
use phantom_memory::MemoryStore;
use phantom_plugins::PluginRegistry;
use phantom_scene::node::{NodeKind, RenderLayer};
use phantom_scene::tree::SceneTree;
use phantom_session::session::{is_session_restore, SessionManager, SessionState, PaneState};
use phantom_session::{AgentStatePersister, GoalStatePersister};
use phantom_mcp::{spawn_listener, AppCommand, McpListener};
use phantom_nlp::{ClaudeLlmBackend, LlmBackend};

use crate::boot::BootSequence;
use crate::boot_order::ShutdownGuard;
use crate::config::PhantomConfig;
use crate::coordinator::AppCoordinator;
use crate::pane::pane_cols_rows;
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

/// Which edge a floating pane is being resized from.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ResizeEdge {
    Right,
    Bottom,
    BottomRight,
}

/// State for dragging or resizing a floating pane.
#[derive(Debug, Clone)]
pub(crate) enum FloatInteraction {
    Dragging {
        app_id: u32,
        offset_x: f32,
        offset_y: f32,
    },
    Resizing {
        app_id: u32,
        edge: ResizeEdge,
        initial_rect: phantom_adapter::Rect,
    },
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

    // -- UI --
    pub(crate) layout: LayoutEngine,
    pub(crate) keybinds: KeybindRegistry,
    pub(crate) theme: Theme,
    pub(crate) status_bar: StatusBar,
    pub(crate) tab_bar: TabBar,

    // -- Boot sequence --
    pub(crate) boot: BootSequence,
    pub(crate) state: AppState,

    // -- Demo mode --
    pub(crate) demo_mode: bool,
    pub(crate) demo_post_boot_done: bool,

    // -- Timing --
    pub(crate) start_time: Instant,
    pub(crate) last_frame: Instant,

    // -- Cached metrics --
    pub(crate) cell_size: (f32, f32),

    // -- Whether a quit has been requested --
    pub(crate) quit_requested: bool,

    // -- Supervisor connection (None when running standalone) --
    pub(crate) supervisor: Option<SupervisorClient>,

    // -- Quake drop-down console --
    pub(crate) console: crate::console::Console,

    // -- Debug shader HUD --
    pub(crate) debug_hud: bool,
    pub(crate) debug_hud_selected: usize,

    // -- AI Brain (OODA loop on dedicated thread) --
    pub(crate) brain: Option<BrainHandle>,

    // -- Per-frame OODA loop (Observe/Orient/Decide/Act driven by render clock).
    //    Runs synchronously in update() — see ooda.rs and issue #45.
    pub(crate) ooda_loop: OodaLoop,

    // -- Substrate runtime (Phase 1/2 primitives: supervisor, event log,
    //    agent registry, memory blocks, spawn rules). Ticked once per frame
    //    from `update.rs`.
    pub(crate) runtime: crate::runtime::AgentRuntime,

    // -- Project context (auto-detected) --
    pub(crate) context: Option<ProjectContext>,

    // -- Memory store (persistent per-project) --
    pub(crate) memory: Option<MemoryStore>,

    // -- Notification store (persistent per-project JSONL, survives restarts) --
    pub(crate) notification_store: Option<phantom_memory::notifications::NotificationStore>,

    // -- Session manager --
    pub(crate) session_manager: Option<SessionManager>,

    // -- Agent-state and goal-state persisters (sidecar files derived from the
    //    session file path). Both are initialized alongside `session_manager`
    //    on boot and updated at shutdown / whenever a goal changes. `None`
    //    when no session path could be determined (e.g. $HOME unset). --
    pub(crate) agent_persister: Option<AgentStatePersister>,
    pub(crate) goal_persister: Option<GoalStatePersister>,

    // -- Shared queue that agent panes push an AgentSnapshot into whenever
    //    they reach a terminal state (Done / Failed). The App drains this at
    //    shutdown and persists via AgentStatePersister. Shared via Arc<Mutex>
    //    so each spawned pane gets a clone without holding a reference back
    //    to the App (same pattern as blocked_event_sink).
    pub(crate) agent_snapshot_queue: crate::agent_pane::AgentSnapshotQueue,

    // -- Idle tracking (seconds since last user keypress) --
    pub(crate) last_input_time: Instant,

    // -- Suggestion overlay (from brain) --
    pub(crate) suggestion: Option<SuggestionOverlay>,

    // -- History of dismissed/expired suggestions (most recent at back) --
    pub(crate) suggestion_history: VecDeque<SuggestionOverlay>,

    // -- Pending brain actions queued by suggestion option selection --
    pub(crate) pending_brain_actions: Vec<phantom_brain::events::AiAction>,

    // -- NLP LLM translate pipeline --
    //    When the heuristic NlpInterpreter returns PassThrough, a short-lived
    //    background thread calls `phantom_nlp::translate()` (synchronous ureq)
    //    and sends the result back here.  Bounded at 8 to prevent a backlog of
    //    network calls when the user is typing fast.
    pub(crate) nlp_translate_rx: std::sync::mpsc::Receiver<NlpTranslateResult>,
    pub(crate) nlp_translate_tx: std::sync::mpsc::SyncSender<NlpTranslateResult>,
    /// `None` when `nlp_llm_enabled = false` in config or ANTHROPIC_API_KEY is absent.
    pub(crate) nlp_backend: Option<std::sync::Arc<dyn LlmBackend + Send + Sync>>,

    // -- Pending sub-agent spawn requests from a Composer's `spawn_subagent`
    //    tool. The Composer pushes onto this queue from its tool handler
    //    (via `phantom_agents::dispatch::DispatchContext::pending_spawn`);
    //    `update.rs` drains it each frame and turns each request into an
    //    `App::spawn_agent_pane_with_opts` call. We use the type from
    //    `phantom_agents::composer_tools` directly so the queue keeps the
    //    Composer's chosen role / label / chat_model intact, wrapped in
    //    `Arc<Mutex<…>>` so dispatch contexts handed to running agents can
    //    push without holding a reference back to the App.
    pub(crate) pending_spawn_subagent: phantom_agents::composer_tools::SpawnSubagentQueue,

    // -- Right-click context menu --
    pub(crate) context_menu: crate::context_menu::ContextMenu,

    // -- Floating pane drag/resize interaction --
    pub(crate) float_interaction: Option<FloatInteraction>,

    // -- Self-test runner (brain exercises its own features) --
    pub(crate) selftest: Option<crate::selftest::SelfTestRunner>,

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

    // -- Fullscreen pane toggle (stores AppId of the fullscreen adapter) --
    pub(crate) fullscreen_pane: Option<u32>,

    // -- Issue #235: shared ticket dispatcher (constructed at startup if
    //    GITHUB_TOKEN is set; None otherwise). Handed to Dispatcher-role agent
    //    panes so they can call request_next_ticket / mark_ticket_in_progress /
    //    mark_ticket_done via the gh CLI.
    pub(crate) ticket_dispatcher: Option<std::sync::Arc<phantom_agents::dispatcher::GhTicketDispatcher>>,

    // -- Inspector snapshot (shared with InspectorAdapter when a pane is open).
    //    `None` when no inspector pane has ever been spawned; `Some(Arc<RwLock<…>>)`
    //    once the user runs `inspect` and stays present so the adapter and the
    //    App share the same lock for the rest of the session.
    pub(crate) inspector_snapshot:
        Option<std::sync::Arc<std::sync::RwLock<phantom_agents::inspector::InspectorView>>>,

    // -- Live design tokens shared with the InspectorAdapter (issue #31).
    //    `None` before the first inspector pane is spawned. After spawn the
    //    App and adapter share the same `Arc<RwLock<Tokens>>`; a theme switch
    //    can write a new `Tokens` value into the arc and the inspector picks
    //    it up at the next `render()` without an adapter restart.
    pub(crate) inspector_tokens:
        Option<std::sync::Arc<std::sync::RwLock<phantom_ui::tokens::Tokens>>>,

    // -- Lars fix-thread sink: shared queue of `EventKind::AgentBlocked`
    //    substrate events emitted by agent panes when their consecutive
    //    tool-call failure streak crosses the threshold. Each spawned
    //    `AgentPane` is given a clone; the drain step inline in
    //    `update.rs::update` runs each frame and forwards into the substrate
    //    runtime (Phase 2.G consumer).
    #[allow(dead_code)] // Wired in Phase 2.G consumer; ahead of time.
    pub(crate) blocked_event_sink: crate::agent_pane::BlockedEventSink,

    // -- Sec.1 capability-denial sink: parallel queue of
    //    `EventKind::CapabilityDenied` substrate events emitted by agent
    //    panes whenever the Layer-2 dispatch gate refuses a tool call. Each
    //    spawned `AgentPane` is given a clone; the drain step inline in
    //    `update.rs::update` runs each frame and forwards into the substrate
    //    runtime. The Defender spawn rule (Sec.4 consumer) reads these.
    #[allow(dead_code)] // Wired in Sec.4 consumer; ahead of time.
    pub(crate) denied_event_sink: crate::agent_pane::DeniedEventSink,

    // -- Sec.7.3 quarantine registry (shared across all agent panes).
    //    Tracks per-agent taint observation counts and escalates to
    //    `QuarantineState::Quarantined` after N consecutive `Tainted`
    //    checks. Every agent pane is handed a clone of this Arc at spawn
    //    time via `set_substrate_handles`; the dispatch gate in
    //    `phantom_agents::dispatch::dispatch_tool` checks it before routing
    //    any tool call.
    pub(crate) quarantine_registry: std::sync::Arc<std::sync::Mutex<phantom_agents::quarantine::QuarantineRegistry>>,

    // -- Sec.8 user-visible notification center. Watches denial timestamps
    //    per agent and pushes a top-of-screen `Severity::Danger` banner
    //    whenever the same agent crosses the pattern threshold inside the
    //    sliding window (default: 3 denials in 60s). `update.rs` feeds
    //    drained `EventKind::CapabilityDenied` events into `record_denial`
    //    and ticks expiry every frame; `notification_banner.rs` reads
    //    `current_banner` to draw the chrome at the top of the screen.
    pub(crate) notifications: crate::notifications::NotificationCenter,

    // -- Event bus topic IDs (bus itself lives in coordinator) --
    #[allow(dead_code)]
    pub(crate) topic_terminal_output: TopicId,
    #[allow(dead_code)]
    pub(crate) topic_terminal_error: TopicId,
    #[allow(dead_code)]
    pub(crate) topic_agent_event: TopicId,
    /// Issue #79 item 7: topic for post-dedup frame notifications.
    #[allow(dead_code)]
    pub(crate) topic_capture_frame: TopicId,

    // -- Plugin registry --
    pub(crate) plugin_registry: PluginRegistry,

    // -- System resource monitor --
    pub(crate) sysmon: crate::sysmon::SysmonHandle,
    pub(crate) sysmon_visible: bool,
    pub(crate) appmon_visible: bool,

    // -- Reusable per-frame buffers (avoid allocations in hot loop) --
    #[allow(dead_code)] // Was used for title formatting, now labels are pre-formatted
    pub(crate) title_buf: String,
    pub(crate) text_cell_buf: Vec<phantom_renderer::text::TerminalCell>,
    pub(crate) pool_grid_cells: Vec<phantom_renderer::grid::GridCell>,

    // -- Per-keystroke glitch effect --
    pub(crate) keystroke_fx: crate::keystroke_fx::KeystrokeFx,

    // -- Watchdog: periodic heartbeat for crash diagnostics --
    pub(crate) watchdog_last: Instant,
    pub(crate) watchdog_frame: u64,

    // -- Git refresh tracking (bounded: only one thread at a time) --
    pub(crate) git_refresh_last: Instant,
    pub(crate) git_refresh_handle: Option<std::thread::JoinHandle<()>>,
    /// Wall-clock instant at which the current `git_refresh_handle` was
    /// spawned.  Used to detect hung git threads (#223): if the handle has not
    /// finished within `GIT_REFRESH_TIMEOUT` we log a warning and drop it so
    /// the next 30-second tick can spawn a fresh one.
    pub(crate) git_refresh_spawned_at: Option<Instant>,

    // -- Reusable overlay text buffer (avoids per-frame alloc in console render) --
    pub(crate) overlay_line_buf: Vec<(String, [f32; 4])>,

    // -- Video playback --
    pub(crate) video_renderer: VideoRenderer,
    pub(crate) video_playback: Option<crate::video::VideoPlayback>,

    // -- App coordinator (owns all terminal adapters, layout, and event bus) --
    pub(crate) coordinator: AppCoordinator,

    // -- Job pool (async work: brain queries, resource loading, etc.) --
    pub(crate) job_pool: Option<crate::jobs::JobPool>,

    // -- Resource manager (GUID registry, ref-counting, async loading) --
    #[allow(dead_code)] // Used by adapters via coordinator in later phases
    pub(crate) resources: crate::resources::ResourceManager,

    // -- Shutdown guard (logs reverse-tier teardown, idempotent via Drop) --
    pub(crate) shutdown_guard: ShutdownGuard,

    // -- Mouse state --
    pub(crate) cursor_position: (f64, f64),
    pub(crate) cursor_over_pane: Option<u32>,
    /// Which terminal mouse button is currently held (for drag/selection tracking).
    pub(crate) mouse_button_held: Option<phantom_terminal::input::MouseButton>,
    /// Timestamp of the last left-click (for double/triple-click detection).
    pub(crate) last_click_time: Option<Instant>,
    /// Position of the last left-click (for double/triple-click detection).
    pub(crate) last_click_pos: (f64, f64),
    /// Number of rapid consecutive clicks (1 = single, 2 = double, 3 = triple).
    pub(crate) click_count: u8,

    // -- Settings panel --
    pub(crate) settings_panel: crate::settings_ui::SettingsPanel,

    // -- Per-pane capture pipeline + encrypted bundle store. Both are
    //    Option because the keychain (or even the SQLCipher build) can
    //    fail at startup; in that case the pipeline gracefully no-ops.
    //    `bundle_store` is wrapped in `Arc` so off-thread persistence
    //    jobs can hold their own handle without an extra clone-of-clone.
    pub(crate) bundle_store: Option<std::sync::Arc<phantom_bundle_store::BundleStore>>,
    pub(crate) capture_state: crate::capture::CaptureState,

    // -- Per-pane last-command tracking (issue #226).
    //    Populated from `Event::CommandStarted` so that the subsequent
    //    `Event::CommandComplete` handler in `drain_bus_to_brain` can feed
    //    a real command string into `ParsedOutput::command` instead of the
    //    empty string that made OODA fix/explain scoring always return 0.
    pub(crate) pane_last_command: std::collections::HashMap<u32, String>,

    // -- Live shader reloader (debug + `live-reload` feature only).
    //    No-op stub in release builds; zero overhead on the hot path.
    pub(crate) shader_reloader: phantom_renderer::shader_loader::ShaderReloader,

    // -- Command history store (JSONL, one entry per completed command). --
    //    `None` on open failure; production paths never `.unwrap()`.
    //    File: `~/.local/share/phantom/history/<session_uuid>.jsonl`.
    pub(crate) history: Option<HistoryStore>,

    // -- Agent output capture sidecar (JSONL, one record per agent run). --
    //    `None` on open failure. Each spawned `AgentPane` receives a clone.
    //    File: `~/.local/share/phantom/history/<session_uuid>-agents.jsonl`.
    pub(crate) agent_capture: Option<AgentOutputCapture>,

    // -- Session UUID shared by the history store and agent capture sidecar.
    //    Generated once at startup; stable for the process lifetime.
    pub(crate) session_uuid: uuid::Uuid,

    // -- Per-pane pending command text.
    //    Populated from `Event::CommandStarted` so that the subsequent
    //    `Event::CommandComplete` handler in `drain_bus_to_brain` can write
    //    a `HistoryEntry` with the actual command string and exit code.
    pub(crate) pending_command_text: std::collections::HashMap<phantom_adapter::AppId, String>,
}

/// An active suggestion from the AI brain.
pub(crate) struct SuggestionOverlay {
    pub(crate) text: String,
    pub(crate) options: Vec<phantom_brain::events::SuggestionOption>,
    pub(crate) shown_at: Instant,
}

/// Result delivered back to the main thread after an off-thread LLM NLP call.
///
/// The background thread builds this from the `Intent` the LLM returned;
/// `update.rs` drains the channel each frame and dispatches the action.
pub(crate) struct NlpTranslateResult {
    /// Human-readable text shown in the console (e.g. "Running: git status").
    pub(crate) display: String,
    /// The action to execute (may be `None` when the LLM asked for clarification).
    pub(crate) action: Option<phantom_brain::events::AiAction>,
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
        mut config: PhantomConfig,
        supervisor_socket: Option<&Path>,
        scale_factor: f32,
    ) -> Result<Self> {
        let width = gpu.surface_config.width;
        let height = gpu.surface_config.height;
        let format = gpu.format;

        // -- Font / text (scaled for HiDPI) --
        let scaled_font_size = config.font_size * scale_factor;
        info!(
            "Font: {:.0}pt logical × {:.1}x scale = {:.0}pt physical",
            config.font_size, scale_factor, scaled_font_size
        );
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
        let grid_renderer = GridRenderer::new(&gpu.device, format, atlas.bind_group_layout());
        let postfx = PostFxPipeline::new(&gpu.device, format, width, height);
        let video_renderer = VideoRenderer::new(&gpu.device, format);

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

        // -- Notification store (persistent per-project) --
        let notification_store = match phantom_memory::notifications::NotificationStore::open(&project_dir) {
            Ok(s) => {
                info!(
                    "Notification store ready ({} existing notifications)",
                    s.count()
                );
                Some(s)
            }
            Err(e) => {
                warn!("Failed to open notification store: {e}");
                None
            }
        };

        // -- History store + agent capture sidecar --
        // Both live under `~/.local/share/phantom/history/<session_uuid>[...].jsonl`.
        // Best-effort: on failure we set `None` and the rest of the app boots normally.
        let session_uuid = uuid::Uuid::new_v4();
        let history = match HistoryStore::open(session_uuid) {
            Ok(h) => {
                info!("History store ready → {}", h.path().display());
                Some(h)
            }
            Err(e) => {
                warn!("Failed to open history store: {e}");
                None
            }
        };
        let agent_capture = match AgentOutputCapture::open(session_uuid) {
            Ok(c) => {
                info!("Agent capture sidecar ready → {}", c.path().display());
                Some(c)
            }
            Err(e) => {
                warn!("Failed to open agent capture sidecar: {e}");
                None
            }
        };

        // -- AI Brain thread --
        let brain = spawn_brain(BrainConfig {
            project_dir: project_dir.clone(),
            enable_suggestions: true,
            enable_memory: true,
            quiet_threshold: 0.35, // Tuned: high enough to suppress noise, low enough for real evrs
            router: None,
            catalog: None,
        });
        info!("AI brain spawned");

        // -- NLP translate channel (bounded at 8 outstanding requests) --
        let (nlp_translate_tx, nlp_translate_rx) =
            std::sync::mpsc::sync_channel::<NlpTranslateResult>(8);

        // Build the LLM backend only when the feature is enabled and the key is present.
        let nlp_backend: Option<std::sync::Arc<dyn LlmBackend + Send + Sync>> =
            if config.nlp_llm_enabled {
                match ClaudeLlmBackend::from_env() {
                    Ok(backend) => {
                        info!("NLP LLM backend: ClaudeLlmBackend (key present)");
                        Some(std::sync::Arc::new(backend))
                    }
                    Err(e) => {
                        debug!("NLP LLM backend: disabled ({e})");
                        None
                    }
                }
            } else {
                debug!("NLP LLM backend: disabled by config (nlp_llm = false)");
                None
            };

        // -- Substrate runtime (supervisor, event log, agent registry, memory
        //    blocks, spawn rules). The seed rule list is empty; the runtime
        //    installs its own ambient defaults in `default_seed_rules`. If the
        //    log file fails to open we fall through to a temp-dir fallback so
        //    the rest of the app still boots — observability is best-effort.
        let runtime = match crate::runtime::AgentRuntime::with_default_paths() {
            Ok(rt) => {
                info!(
                    "Substrate runtime ready: {} spawn rules, log → {}",
                    rt.rules().rule_count(),
                    rt.event_log().path().display(),
                );
                rt
            }
            Err(e) => {
                warn!("Substrate event log unavailable on default path: {e}; using temp dir");
                let dir = std::env::temp_dir().join("phantom-runtime");
                let _ = std::fs::create_dir_all(&dir);
                let cfg = crate::runtime::RuntimeConfig::under_dir(&dir);
                crate::runtime::AgentRuntime::new(cfg, Vec::new())
                    .expect("substrate runtime: temp-dir fallback must open event log")
            }
        };

        // -- Bundle store (encrypted SQLCipher + vector index). Best-effort:
        //    on any failure (keychain locked, disk perms, etc.) we set
        //    `bundle_store: None` and the per-pane capture pipeline gracefully
        //    no-ops. Path defaults to `$HOME/.config/phantom/bundles` and
        //    falls back to a temp-dir under the same constraints as the
        //    event log above.
        let bundle_store = open_bundle_store();
        if bundle_store.is_some() {
            info!("Bundle store ready (capture pipeline armed)");
        } else {
            info!("Bundle store unavailable — per-pane capture disabled");
        }

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
        if let Some(n) = scene.get_mut(cmd_bar_node) {
            n.render_layer = RenderLayer::Overlay;
        }
        let debug_hud_node = scene.add_node(root, NodeKind::DebugHud);
        if let Some(n) = scene.get_mut(debug_hud_node) {
            n.render_layer = RenderLayer::Overlay;
        }
        let suggestion_node = scene.add_node(root, NodeKind::AgentSuggestion);
        if let Some(n) = scene.get_mut(suggestion_node) {
            n.render_layer = RenderLayer::Overlay;
        }
        let _ = (cmd_bar_node, debug_hud_node, suggestion_node); // suppress unused
        // Set initial transforms from layout.
        scene.set_transform(root, 0.0, 0.0, width as f32, height as f32);
        // Sync first pane's scene transform from the layout engine.
        if let Ok(rect) = layout.get_pane_rect(pane_id) {
            scene.set_transform(first_pane_node, rect.x, rect.y, rect.width, rect.height);
        }
        scene.update_world_transforms();
        info!("Scene graph initialized: {} nodes", scene.node_count());

        // first_pane_node and pane_id will be registered with the coordinator below.

        // -- Session manager + restore --
        let session_manager = match SessionManager::new() {
            Ok(sm) => Some(sm),
            Err(e) => {
                warn!("Failed to create session manager: {e}");
                None
            }
        };

        // Auto-detect session restore: skip boot animation when a previous
        // session exists for this project or PHANTOM_RESTORING=1 is set.
        // Checked before the boot block below reads `config.skip_boot`.
        if !config.skip_boot {
            let session_dir = SessionManager::session_dir_path();
            if is_session_restore(&session_dir, &project_dir) {
                log::info!("Session restore detected — auto-skipping boot animation");
                config.skip_boot = true;
            }
        }

        // Try to restore the most recent session for this project.
        // Also derive the agent/goal persister sidecar paths from the latest
        // session file — they live next to the session JSON so they are
        // automatically co-located.
        let mut agent_persister: Option<AgentStatePersister> = None;
        let mut goal_persister: Option<GoalStatePersister> = None;

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

                    // Initialise persister pair from the latest session file
                    // path so agent / goal sidecar files land next to it.
                    if let Ok(Some(session_path)) = sm.latest_path(&project_dir) {
                        let ap = AgentStatePersister::new(
                            AgentStatePersister::sidecar_path(&session_path),
                        );
                        match ap.load() {
                            Ok(Some(ref file)) => {
                                let n = file.agent_count();
                                info!(
                                    "Agent state restored: {n} snapshot{} available",
                                    if n == 1 { "" } else { "s" },
                                );
                            }
                            Ok(None) => {}
                            Err(e) => warn!("Failed to load agent state: {e}"),
                        }
                        agent_persister = Some(ap);

                        let gp = GoalStatePersister::new(
                            GoalStatePersister::sidecar_path(&session_path),
                        );
                        match gp.load() {
                            Ok(Some(ref file)) => {
                                let n = file.goal_count();
                                info!(
                                    "Goal state restored: {n} goal{} available",
                                    if n == 1 { "" } else { "s" },
                                );
                            }
                            Ok(None) => {}
                            Err(e) => warn!("Failed to load goal state: {e}"),
                        }
                        goal_persister = Some(gp);
                    }
                }
                Ok(None) => {
                    info!("No previous session for this project");
                }
                Err(e) => {
                    warn!("Failed to load session: {e}");
                }
            }
        }

        // -- Event bus (single instance, will be handed to AppCoordinator) --
        let mut event_bus = EventBus::new();
        let topic_terminal_output =
            event_bus.create_topic(0, "terminal.output", DataType::TerminalOutput);
        let topic_terminal_error = event_bus.create_topic(0, "terminal.error", DataType::Text);
        let topic_agent_event = event_bus.create_topic(0, "agent.event", DataType::Json);
        // Issue #79 item 7: topic for post-dedup frame notifications.
        let topic_capture_frame = event_bus.create_topic(0, "capture.frame", DataType::Json);

        // Subscribe a virtual "brain observer" so the AI brain receives bus events.
        const BRAIN_OBSERVER_ID: u32 = 0xFFFF_FFFE;
        event_bus.subscribe(BRAIN_OBSERVER_ID, topic_terminal_output);
        event_bus.subscribe(BRAIN_OBSERVER_ID, topic_terminal_error);
        event_bus.subscribe(BRAIN_OBSERVER_ID, topic_agent_event);
        event_bus.subscribe(BRAIN_OBSERVER_ID, topic_capture_frame);
        info!(
            "Event bus initialized: {} topics, brain observer subscribed",
            event_bus.topic_count()
        );

        // -- Plugin registry --
        let plugin_registry = match PluginRegistry::new() {
            Ok(reg) => reg,
            Err(e) => {
                warn!("Failed to create plugin registry: {e}");
                match PluginRegistry::with_dir(
                    std::env::temp_dir().join("phantom-plugins-fallback"),
                ) {
                    Ok(reg) => reg,
                    Err(e2) => {
                        warn!("Plugins disabled — all registry paths failed: {e2}");
                        PluginRegistry::with_dir(std::env::temp_dir())
                            .unwrap_or_else(|_| PluginRegistry::empty())
                    }
                }
            }
        };

        // -- Job pool (4 workers for async brain queries, resource loading, etc.) --
        let job_pool = crate::jobs::JobPool::start_up(4);
        info!("Job pool initialized: 4 workers");

        // -- App coordinator (owns all adapters, dispatches update/render/input) --
        let mut coordinator = AppCoordinator::new(event_bus);

        // -- Register initial terminal as adapter (Phase 3 — coordinator-managed) --
        {
            use crate::adapters::terminal::TerminalAdapter;
            use phantom_scene::clock::Cadence;
            use phantom_terminal::output::TerminalThemeColors;

            let theme_colors = TerminalThemeColors {
                foreground: theme.colors.foreground,
                background: theme.colors.background,
                cursor: theme.colors.cursor,
                ansi: Some(theme.colors.ansi),
            };
            let adapter = TerminalAdapter::with_theme(terminal, theme_colors);
            let _app_id = coordinator.register_adapter_at_pane(
                Box::new(adapter),
                pane_id,
                first_pane_node,
                Cadence::unlimited(),
                &mut layout,
            );
            info!("Initial terminal registered as adapter (AppId {_app_id})");
        }

        // Configure the arbiter with window content area and cell metrics.
        {
            let content_area = (width as f32, content_height);
            coordinator.set_arbiter_size(content_area, cell_size);
        }

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
            .unwrap_or_else(|| {
                PathBuf::from(format!("/tmp/phantom-mcp-{}.sock", std::process::id()))
            });
        let (mcp_cmd_tx, mcp_cmd_rx) = mpsc::channel::<AppCommand>();
        let mcp_listener = match spawn_listener(mcp_socket_path.clone(), mcp_cmd_tx) {
            Ok(l) => {
                info!("MCP listener ready: {}", mcp_socket_path.display());
                Some(l)
            }
            Err(e) => {
                warn!(
                    "Failed to start MCP listener at {}: {e}",
                    mcp_socket_path.display()
                );
                None
            }
        };

        // -- Issue #235: ticket dispatcher (best-effort, graceful degradation).
        // GH_REPO is the canonical "owner/repo" slug used by the gh CLI.
        // GITHUB_TOKEN is already consumed by the gh CLI directly, but its
        // presence signals that gh auth is configured so the dispatcher will
        // actually work. Without both vars we log a single warning and leave
        // `ticket_dispatcher: None` — no panic, Dispatcher agents see the
        // "ticket dispatcher not configured" error from the dispatch layer and
        // can self-correct.
        let ticket_dispatcher = build_ticket_dispatcher();

        let now = Instant::now();

        Ok(Self {
            gpu,
            atlas,
            text_renderer,
            quad_renderer,
            grid_renderer,
            postfx,
            layout,
            keybinds,
            theme,
            status_bar,
            tab_bar,
            boot,
            state: initial_state,
            demo_mode: config.demo_mode,
            demo_post_boot_done: false,
            start_time: now,
            last_frame: now,
            cell_size,
            quit_requested: false,
            supervisor,
            console: crate::console::Console::new(),
            debug_hud: false,
            debug_hud_selected: 0,
            brain: Some(brain),
            ooda_loop: OodaLoop::new(OodaConfig::default()),
            runtime,
            context: Some(context),
            memory,
            notification_store,
            session_manager,
            agent_persister,
            goal_persister,
            agent_snapshot_queue: crate::agent_pane::new_agent_snapshot_queue(),
            last_input_time: now,
            suggestion: None,
            suggestion_history: VecDeque::with_capacity(10),
            pending_brain_actions: Vec::new(),
            nlp_translate_rx,
            nlp_translate_tx,
            nlp_backend,
            pending_spawn_subagent: phantom_agents::composer_tools::new_spawn_subagent_queue(),
            context_menu: crate::context_menu::ContextMenu::new(),
            float_interaction: None,
            selftest: None,
            scene,
            scene_content_node: content_node,
            mcp_cmd_rx,
            _mcp_listener: mcp_listener,
            pool_quads: Vec::with_capacity(256),
            pool_glyphs: Vec::with_capacity(4096),
            pool_chrome_quads: Vec::with_capacity(32),
            pool_chrome_glyphs: Vec::with_capacity(256),
            fullscreen_pane: None,
            inspector_snapshot: None,
            inspector_tokens: None,
            blocked_event_sink: crate::agent_pane::new_blocked_event_sink(),
            denied_event_sink: crate::agent_pane::new_denied_event_sink(),
            quarantine_registry: std::sync::Arc::new(std::sync::Mutex::new(
                phantom_agents::quarantine::QuarantineRegistry::new(),
            )),
            notifications: crate::notifications::NotificationCenter::new(),
            topic_terminal_output,
            topic_terminal_error,
            topic_agent_event,
            topic_capture_frame,
            plugin_registry,
            sysmon,
            sysmon_visible: false,
            appmon_visible: false,
            title_buf: String::with_capacity(64),
            text_cell_buf: Vec::with_capacity(256),
            pool_grid_cells: Vec::with_capacity(80 * 24),
            keystroke_fx: crate::keystroke_fx::KeystrokeFx::new(),
            watchdog_last: now,
            watchdog_frame: 0,
            git_refresh_last: now,
            git_refresh_handle: None,
            git_refresh_spawned_at: None,
            overlay_line_buf: Vec::with_capacity(128),
            video_renderer,
            video_playback: None,
            coordinator,
            job_pool: Some(job_pool),
            resources: crate::resources::ResourceManager::new(),
            shutdown_guard: ShutdownGuard::new(),
            cursor_position: (0.0, 0.0),
            cursor_over_pane: None,
            mouse_button_held: None,
            last_click_time: None,
            last_click_pos: (0.0, 0.0),
            click_count: 0,
            settings_panel: crate::settings_ui::SettingsPanel::new(),
            bundle_store,
            capture_state: crate::capture::CaptureState::new(),
            ticket_dispatcher,
            pane_last_command: std::collections::HashMap::new(),
            shader_reloader: phantom_renderer::shader_loader::ShaderReloader::new(),
            history,
            agent_capture,
            session_uuid,
            pending_command_text: std::collections::HashMap::new(),
        })
    }

    /// Returns `true` if the app has requested to quit.
    pub fn should_quit(&self) -> bool {
        self.quit_requested
    }

    /// Watchdog trace: returns a log line every `interval` frames.
    /// Written directly to disk by the event loop (bypasses Rust logger,
    /// survives SIGKILL mid-frame).
    pub fn watchdog_trace(&mut self, interval: u64) -> Option<String> {
        self.watchdog_frame += 1;
        if self.watchdog_frame % interval != 0 {
            return None;
        }
        let state = match self.state {
            AppState::Boot => "boot",
            AppState::Terminal => "term",
        };
        Some(format!(
            "[TRACE] frame={} state={} adapters={} agents={}\n",
            self.watchdog_frame,
            state,
            self.coordinator.adapter_count(),
            self.coordinator
                .registry()
                .all_running()
                .into_iter()
                .filter_map(|id| self.coordinator.registry().get(id))
                .filter(|e| e.app_type == "agent")
                .count(),
        ))
    }

    /// Graceful shutdown: save session, dispatch plugin hooks, shut down brain,
    /// and log reverse-tier teardown via the shutdown guard.
    pub fn shutdown(&mut self) {
        // Begin ordered shutdown (logs tier-by-tier teardown).
        self.shutdown_guard.shut_down();
        // Tell the supervisor we're exiting on purpose — don't restart.
        if let Some(ref mut sv) = self.supervisor {
            sv.send(&AppMessage::ExitClean);
        }

        // Save session state.
        if let Some(ref sm) = self.session_manager {
            let state = self.build_session_state();
            match sm.save(&state) {
                Ok(session_path) => {
                    info!("Session saved to {}", session_path.display());

                    // Persist agent snapshots alongside the session file.
                    // Drain all snapshots collected in the queue during this
                    // session (panes push into it on Done/Failed) and also
                    // snapshot any agents still alive (e.g. long-running tasks
                    // interrupted by the user). De-duplicate by agent id so we
                    // don't write the same agent twice.
                    let agent_sidecar = AgentStatePersister::sidecar_path(&session_path);
                    let ap = AgentStatePersister::new(agent_sidecar);
                    let queue_snaps: Vec<phantom_session::AgentSnapshot> =
                        match self.agent_snapshot_queue.lock() {
                            Ok(mut q) => std::mem::take(&mut *q),
                            Err(_) => {
                                warn!("agent_snapshot_queue mutex poisoned");
                                Vec::new()
                            }
                        };
                    // Build a slice of &Agent references from the queue.
                    // `save_agents` takes `&[&Agent]`; the queue holds `AgentSnapshot`
                    // values so we write them directly via `AgentStateFile`.
                    if !queue_snaps.is_empty() {
                        let file = phantom_session::AgentStateFile::new(queue_snaps);
                        match file.save(ap.path()) {
                            Ok(()) => info!("Agent state saved ({} snapshot(s))", file.agent_count()),
                            Err(e) => warn!("Failed to save agent state: {e}"),
                        }
                    } else {
                        info!("Agent state: no snapshots to persist this session");
                    }
                    self.agent_persister = Some(ap);

                    // Persist goal state alongside the session file.
                    // The goal persister is updated in-place by update.rs whenever
                    // AiEvent::GoalSet fires. On shutdown we just re-point the
                    // persister at the new session sidecar path so the next boot
                    // derives from the correct file.
                    let goal_sidecar = GoalStatePersister::sidecar_path(&session_path);
                    let gp = GoalStatePersister::new(goal_sidecar);
                    // Copy over whatever the existing persister holds, if any.
                    if let Some(ref existing_gp) = self.goal_persister {
                        match existing_gp.load() {
                            Ok(Some(file)) => {
                                let goals: Vec<_> = file.goals().to_vec();
                                let n = goals.len();
                                match gp.save_goals(goals) {
                                    Ok(()) => info!("Goal state flushed at shutdown ({n} goal(s))"),
                                    Err(e) => warn!("Failed to flush goal state: {e}"),
                                }
                            }
                            Ok(None) => {} // Nothing to flush.
                            Err(e) => warn!("Failed to read goal state for flush: {e}"),
                        }
                    }
                    self.goal_persister = Some(gp);
                }
                Err(e) => warn!("Failed to save session: {e}"),
            }
        }

        // Dispatch shutdown hooks to plugins.
        let wd = self
            .context
            .as_ref()
            .map(|c| c.root.clone())
            .unwrap_or_else(|| ".".into());
        let ctx = phantom_plugins::HookContext::shutdown(&wd);
        let responses = self
            .plugin_registry
            .dispatch_hook(&phantom_plugins::HookType::OnShutdown, &ctx);
        for resp in &responses {
            info!("[plugin shutdown]: {resp:?}");
        }
        self.plugin_registry.shutdown_all();

        // Shut down the brain thread.
        if let Some(ref brain) = self.brain {
            let _ = brain.send_event(AiEvent::Shutdown);
        }

        // Shut down the job pool (waits up to 5s for workers to finish).
        if let Some(pool) = self.job_pool.take() {
            pool.shut_down();
        }
    }

    /// Build a SessionState snapshot from current app state.
    fn build_session_state(&self) -> SessionState {
        use std::time::{SystemTime, UNIX_EPOCH};

        let project_dir = self
            .context
            .as_ref()
            .map(|c| c.root.clone())
            .unwrap_or_else(|| ".".into());
        let project_name = self
            .context
            .as_ref()
            .map(|c| c.name.clone())
            .unwrap_or_else(|| "unknown".into());
        let git_branch = self
            .context
            .as_ref()
            .and_then(|c| c.git.as_ref().map(|g| g.branch.clone()));

        // Build pane state from coordinator adapters.
        let focused_app = self.coordinator.focused();
        let panes: Vec<PaneState> = self
            .coordinator
            .all_app_ids()
            .iter()
            .map(|&app_id| PaneState {
                working_dir: project_dir.clone(),
                is_focused: focused_app == Some(app_id),
                cols: 80,
                rows: 24,
                title: "shell".into(),
                split: None,
            })
            .collect();

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
    // Goal persistence (issue #206)
    // -----------------------------------------------------------------------

    /// Snapshot the current goal and persist it via the goal-state persister.
    ///
    /// Called from `commands.rs` whenever the user issues a `goal <objective>`
    /// command.  Builds a minimal [`GoalSnapshot`] from the objective string
    /// and writes it to the sidecar file co-located with the current session.
    ///
    /// No-op when no `goal_persister` is initialised (first boot with no
    /// previous session file) — the goal will be persisted at next shutdown
    /// once a session file exists and the persister has been created.
    pub(crate) fn persist_goal(&mut self, objective: &str) {
        use phantom_session::{
            GoalSnapshot, PlanStepBuilder, SavedFactConfidence, SavedFact, SavedStepStatus,
        };
        use std::time::{SystemTime, UNIX_EPOCH};

        let Some(ref gp) = self.goal_persister else {
            log::debug!("goal_persister not yet initialised; goal will be persisted at shutdown");
            return;
        };

        let created_at_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let initial_step = PlanStepBuilder::new(
            objective,
            phantom_agents::AgentTask::FreeForm { prompt: objective.to_owned() },
        )
        .status(SavedStepStatus::Pending)
        .build();

        let snapshot = GoalSnapshot::new(
            objective.to_owned(),
            vec![SavedFact::new("goal set by user", SavedFactConfidence::Verified, "commands")],
            vec![initial_step],
            vec![],   // plan_history: no prior plans yet
            0,        // stall_counter
            2,        // stall_threshold (matches brain default)
            0,        // replan_count
            5,        // max_replans (matches brain default)
            created_at_secs,
            None,     // last_replan_at_secs
        );

        match gp.save_goals(vec![snapshot]) {
            Ok(()) => info!("Goal state persisted: {objective}"),
            Err(e) => warn!("Failed to persist goal state: {e}"),
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

        // Re-negotiate arbiter allocations with updated content area.
        // Chrome height = tab bar + status bar (same formula as constructor).
        let chrome_h = self
            .layout
            .get_tab_bar_rect()
            .map(|r| r.height)
            .unwrap_or(0.0)
            + self
                .layout
                .get_status_bar_rect()
                .map(|r| r.height)
                .unwrap_or(0.0);
        let content_w = width as f32;
        let content_h = (height as f32 - chrome_h).max(0.0);
        self.coordinator
            .on_window_resize((content_w, content_h), &mut self.layout);

        // Resize coordinator-managed adapters to match new layout dimensions.
        for app_id in self.coordinator.all_app_ids() {
            if let Some(pane_id) = self.coordinator.pane_id_for(app_id) {
                if let Ok(rect) = self.layout.get_pane_rect(pane_id) {
                    let (cols, rows) = pane_cols_rows(self.cell_size, rect);
                    let _ = self.coordinator.send_command(
                        app_id,
                        "resize",
                        &serde_json::json!({"cols": cols, "rows": rows}),
                    );
                    trace!("Adapter {app_id} resized to {cols}x{rows}");
                }
            }
        }

        // Update scene graph root transform.
        let root = self.scene.root();
        self.scene
            .set_transform(root, 0.0, 0.0, width as f32, height as f32);

        // Sync adapter positions from Taffy layout into the scene graph.
        let plan = self.coordinator.build_layout_plan(&self.layout);
        self.coordinator
            .sync_arbiter_to_scene(&plan, &mut self.scene);

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
    // Capture (see capture.rs for per-pane GPU readback + bundle persistence)
    // -----------------------------------------------------------------------
}

/// Construct a [`GhTicketDispatcher`] backed by the real `gh` CLI.
///
/// Returns `Some(Arc<GhTicketDispatcher>)` when both `GITHUB_TOKEN` and
/// `GH_REPO` (format: `"owner/repo"`) are present in the environment,
/// indicating that `gh auth` is configured and the dispatcher will actually
/// work. Returns `None` with a `warn!` log line if either variable is absent
/// — the rest of the app continues to boot and Dispatcher-role agents see
/// `"ticket dispatcher not configured"` from the dispatch layer instead of
/// a panic.
fn build_ticket_dispatcher() -> Option<std::sync::Arc<phantom_agents::dispatcher::GhTicketDispatcher>> {
    let token = std::env::var("GITHUB_TOKEN").ok();
    let repo = std::env::var("GH_REPO").ok();

    match (token, repo) {
        (Some(_), Some(repo)) => {
            let dispatcher = phantom_agents::dispatcher::GhTicketDispatcher::new(repo.clone());
            info!("GhTicketDispatcher ready for repo {repo}");
            Some(dispatcher.shared())
        }
        (None, _) => {
            warn!(
                "GITHUB_TOKEN not set — GhTicketDispatcher unavailable; \
                 Dispatcher-role agents will see \"ticket dispatcher not configured\""
            );
            None
        }
        (Some(_), None) => {
            warn!(
                "GH_REPO not set — GhTicketDispatcher unavailable; \
                 set GH_REPO=owner/repo to enable the Dispatcher role's ticket tools"
            );
            None
        }
    }
}

/// Open the encrypted bundle store at `$HOME/.config/phantom/bundles`,
/// falling back to a temp-dir if `$HOME` is unset. Returns `None` on any
/// failure (keychain locked, disk perms, schema mismatch, etc.) so the
/// rest of the app can boot without the capture pipeline.
fn open_bundle_store() -> Option<std::sync::Arc<phantom_bundle_store::BundleStore>> {
    use phantom_bundle_store::{BundleStore, MasterKey, StoreConfig};

    // Resolve master key from the OS keychain. If the keyring isn't
    // available (CI, sandboxed test runs), we fall back to a deterministic
    // key derived from `$HOME`. The fallback isn't secure — bundles
    // written under it are encrypted but with a key any process on the
    // box can rederive — and we surface a clear log line so the user
    // notices.
    let master_key = match MasterKey::from_keyring() {
        Ok(k) => k,
        Err(e) => {
            warn!("Bundle store keychain unavailable ({e}); skipping capture init");
            return None;
        }
    };

    let root = std::env::var("HOME")
        .ok()
        .map(|h| {
            PathBuf::from(h)
                .join(".config")
                .join("phantom")
                .join("bundles")
        })
        .unwrap_or_else(|| std::env::temp_dir().join("phantom-bundles"));

    let cfg = StoreConfig {
        root: root.clone(),
        master_key,
    };
    match BundleStore::open(cfg) {
        Ok(s) => {
            info!("Bundle store opened at {}", root.display());
            Some(std::sync::Arc::new(s))
        }
        Err(e) => {
            warn!("Bundle store open failed at {}: {e}", root.display());
            None
        }
    }
}
