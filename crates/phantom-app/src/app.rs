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
use phantom_renderer::atlas::{ColorGlyphAtlas, GlyphAtlas};
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
use phantom_ui::widgets::{KeybindHelp, SearchBar, StatusBar, TabBar};

use phantom_adapter::{DataType, EventBus, TopicId};
use phantom_brain::brain::{BrainConfig, BrainHandle, spawn_brain};
use phantom_brain::capability_audit::{AuditConfig, CapabilityReport};
use phantom_brain::events::AiEvent;
use phantom_brain::ooda::{OodaConfig, OodaLoop};
use phantom_context::{ContextAssembler, ProjectContext};
use phantom_history::{AgentOutputCapture, HistoryStore};
use phantom_mcp::{AppCommand, McpListener, spawn_listener};
use phantom_memory::MemoryStore;
use phantom_skill_host::{LlmHost, LlmSkill};
use phantom_plugins::PluginRegistry;
use phantom_scene::node::{NodeKind, RenderLayer};
use phantom_scene::tree::SceneTree;
use phantom_session::session::{PaneState, SessionManager, SessionState, is_session_restore};
use phantom_session::{AgentStatePersister, GoalStatePersister};
use phantom_session::{RestoredSession, SessionRestorer};

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
    /// Color emoji atlas (Rgba8UnormSrgb). Stores SwashContent::Color bitmaps
    /// un-tinted so emoji render in full color across all themes (fixes #356).
    pub(crate) color_atlas: ColorGlyphAtlas,
    pub(crate) text_renderer: TextRenderer,
    pub(crate) quad_renderer: QuadRenderer,
    pub(crate) grid_renderer: GridRenderer,
    /// Grid renderer for full-color emoji glyphs (text_color.wgsl pipeline).
    /// Draws instances from the Rgba8UnormSrgb color atlas without FG tinting.
    pub(crate) color_grid_renderer: GridRenderer,
    pub(crate) postfx: PostFxPipeline,

    // -- UI --
    pub(crate) layout: LayoutEngine,
    pub(crate) keybinds: KeybindRegistry,
    pub(crate) theme: Theme,
    pub(crate) status_bar: StatusBar,
    pub(crate) tab_bar: TabBar,
    /// Full-screen keybind help overlay (F1 / ?).
    pub(crate) keybind_help: KeybindHelp,

    // -- Find-in-terminal search bar (Cmd+F) --
    pub(crate) search_bar: SearchBar,

    // -- Boot sequence --
    pub(crate) boot: BootSequence,
    pub(crate) state: AppState,

    // -- Demo mode --
    pub(crate) demo_mode: bool,
    pub(crate) demo_post_boot_done: bool,

    // -- Post-boot agent spawn --
    /// Set to `true` the first time we enter AppState::Terminal so we spawn
    /// one agent pane as the default view instead of leaving the user staring
    /// at a raw terminal.
    pub(crate) post_boot_agent_spawned: bool,

    /// Shared flag between the live `SetupAdapter` (when present) and the
    /// App's update loop. `SetupAdapter::update` flips this to `true` on a
    /// `NONE → SOME` `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` transition.
    /// `App::update` drains the flag each tick and, when set, calls
    /// `spawn_agent_pane(...)` whose `adapter_count() == 1` replace-focused
    /// path swaps the SetupAdapter out for the real agent at the same pane
    /// slot. This avoids any cross-thread `AppCommand` plumbing.
    pub(crate) post_setup_upgrade: std::sync::Arc<std::sync::atomic::AtomicBool>,

    // -- Timing --
    pub(crate) start_time: Instant,
    pub(crate) last_frame: Instant,
    /// Frame-dt safety clamp: prevents animation explosions caused by
    /// abnormally long frames (debugger pauses, OS suspends, GC spikes).
    /// When measured dt exceeds `max_dt` (100 ms), the clamp substitutes
    /// `target_dt` (16.6 ms) so downstream animation math stays bounded.
    pub(crate) dt_clamp: phantom_scene::DtClamp,
    /// Centralized monotonic game clock (scene time).
    /// Ticked once per frame with the clamped dt so all subsystems share a
    /// single advancing time base that cannot jump on pause/resume.
    pub(crate) scene_clock: phantom_scene::Clock,

    // -- Clock-driven terminal cursor blink timer.
    //    Ticked once per frame with wall-clock milliseconds.  The terminal
    //    renderer consults `cursor_blink.is_visible()` instead of drawing the
    //    cursor on every repaint, so rapidly-repainting TUIs (gemini, htop,
    //    lazygit) no longer cause the cursor quad to strobe.
    pub(crate) cursor_blink: phantom_ui::CursorBlink,

    // -- Cached metrics --
    pub(crate) cell_size: (f32, f32),

    // -- Whether a quit has been requested --
    pub(crate) quit_requested: bool,

    // -- Force-redraw latch set by external events (key/mouse/resize/focus).
    //    Cleared after every GPU submit. Ensures at least one repaint happens
    //    even when the scene graph is fully clean.
    pub(crate) force_redraw: bool,

    // -- Supervisor connection (None when running standalone) --
    pub(crate) supervisor: Option<SupervisorClient>,

    // -- Quake drop-down console --
    pub(crate) console: crate::console::Console,

    // -- Debug shader HUD --
    pub(crate) debug_hud: bool,
    pub(crate) debug_hud_selected: usize,

    // -- AI Brain (OODA loop on dedicated thread) --
    pub(crate) brain: Option<BrainHandle>,

    // -- Semantic skill (SkillHost-routed parser used by the CommandComplete handler).
    //    Built once at startup via SkillHost::build(): static path in release and by
    //    default in debug; dylib hot-reload path when PHANTOM_HOT_MODULES=1.
    //    Stored as Arc<dyn SemanticSkill> so the dispatch site is backend-agnostic.
    pub(crate) semantic_skill: std::sync::Arc<dyn phantom_skill_host::SemanticSkill>,

    // -- Per-frame OODA loop (Observe/Orient/Decide/Act driven by render clock).
    //    Runs synchronously in update() — see ooda.rs and issue #45.
    pub(crate) ooda_loop: OodaLoop,

    // -- Substrate runtime (Phase 1/2 primitives: supervisor, event log,
    //    agent registry, memory blocks, spawn rules). Ticked once per frame
    //    from `update.rs`.
    pub(crate) runtime: crate::runtime::AgentRuntime,

    // -- Project context (auto-detected) --
    pub(crate) context: Option<ProjectContext>,

    // -- Context assembler (caches DAG topology so agent invocations call
    //    assembler.assemble() instead of bare ProjectContext::detect()).
    //    Wrapped in Arc<Mutex> so agent pane threads can share it without
    //    holding a reference back to App. --
    pub(crate) context_assembler: std::sync::Arc<std::sync::Mutex<ContextAssembler>>,

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

    // -- Snapshots loaded from the previous session's sidecar files.
    //    Populated by `SessionRestorer::restore()` during `with_config_scaled`
    //    and consumed (`.take()`'d) by `update.rs` on the first Terminal tick
    //    to emit an `AiEvent::Interrupt` resume prompt when non-empty.
    //    `None` after the prompt has been fired or when there is nothing to restore. --
    pub(crate) restored_session: Option<RestoredSession>,

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
    /// `None` when `nlp_llm_enabled = false` in config or no LLM backend is reachable.
    ///
    /// The backend is routed through [`phantom_skill_host::LlmHost`] so that
    /// hot-module reload can swap the `phantom-nlp` dylib at runtime when
    /// `PHANTOM_HOT_MODULES=1` is set (closes #383).
    pub(crate) nlp_backend: Option<std::sync::Arc<dyn LlmSkill>>,

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
    // -- Hub registration listener (outbound WSS to phantom-hub, issue #395) --
    pub(crate) _hub_listener: Option<phantom_mcp::HubListener>,

    // -- Federation relay task handle (kept alive for the process lifetime).
    //    `Some` when PHANTOM_RELAY_URL is set and the relay task was spawned.
    //    The relay task owns the tokio runtime and drives the relay WebSocket
    //    connection; this JoinHandle prevents the thread from being silently
    //    detached while the App is alive.
    pub(crate) _relay_task: Option<std::thread::JoinHandle<()>>,

    // -- One-shot relay handshake notification channel.
    //    The relay task sends a RelayHandshakeInfo on this when the WebSocket
    //    handshake completes. update.rs drains it and emits
    //    AiEvent::RelayConnected to the brain. Set to None after the
    //    first message is received (one-shot semantics).
    pub(crate) relay_connected_rx:
        Option<std::sync::mpsc::Receiver<crate::app::RelayHandshakeInfo>>,

    // -- MCP external-server discovery barrier.
    //    Set to `true` by the async discovery task once all servers listed in
    //    `$PHANTOM_MCP_SERVERS` have been connected and their `tools/list`
    //    responses indexed into the tool registry.  Agent spawn waits up to
    //    500 ms for this flag before proceeding (non-blocking: if discovery
    //    is still running the agent gets an empty external tool list and can
    //    still function).
    pub(crate) mcp_discovery_complete: std::sync::Arc<std::sync::atomic::AtomicBool>,

    // -- Render pools (reused each frame via clear() to avoid per-frame allocs) --
    pub(crate) pool_quads: Vec<QuadInstance>,
    pub(crate) pool_glyphs: Vec<phantom_renderer::text::GlyphInstance>,
    /// Color-glyph instances (Rgba8UnormSrgb atlas, no FG tint). Fixes #356.
    pub(crate) pool_color_glyphs: Vec<phantom_renderer::text::GlyphInstance>,
    pub(crate) pool_chrome_quads: Vec<QuadInstance>,
    pub(crate) pool_chrome_glyphs: Vec<phantom_renderer::text::GlyphInstance>,

    // -- Fullscreen pane toggle (stores AppId of the fullscreen adapter) --
    pub(crate) fullscreen_pane: Option<u32>,

    // -- Issue #235: shared ticket dispatcher (constructed at startup if
    //    GITHUB_TOKEN is set; None otherwise). Handed to Dispatcher-role agent
    //    panes so they can call request_next_ticket / mark_ticket_in_progress /
    //    mark_ticket_done via the gh CLI.
    pub(crate) ticket_dispatcher:
        Option<std::sync::Arc<phantom_agents::dispatcher::GhTicketDispatcher>>,

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

    // -- Per-pane handles for the mockup's chrome adapters
    //    (Settings / Notifications / Console / KeybindsHelp / Logs /
    //    FilesWatch / Diff / Memory / Fleet / Plugins / Database / VoiceStt).
    //
    //    `Some(app_id)` while the pane is open, `None` after despawn. The
    //    keybind handler toggles: spawn if `None`, despawn if `Some`.
    pub(crate) settings_pane_id: Option<u32>,
    pub(crate) notifications_pane_id: Option<u32>,
    pub(crate) console_pane_id: Option<u32>,
    pub(crate) keybinds_help_pane_id: Option<u32>,
    pub(crate) logs_pane_id: Option<u32>,
    pub(crate) files_watch_pane_id: Option<u32>,
    pub(crate) diff_pane_id: Option<u32>,
    pub(crate) memory_pane_id: Option<u32>,
    pub(crate) fleet_pane_id: Option<u32>,
    pub(crate) plugins_pane_id: Option<u32>,
    pub(crate) database_pane_id: Option<u32>,
    pub(crate) voice_stt_pane_id: Option<u32>,

    /// Background filesystem watcher kept alive while the FilesWatch pane
    /// is open. Dropped on despawn to stop the OS watch.
    pub(crate) files_watcher: Option<crate::files_watcher::FilesWatcher>,

    /// Last log-ring offset pushed into the Logs pane. Advances each frame
    /// so the adapter only sees new entries; reset on pane spawn.
    pub(crate) logs_watermark: usize,

    /// Last observed revision of the Settings pane. When the pane mutates
    /// any value its revision bumps; the App detects the change each
    /// frame, persists to ~/.config/phantom/settings.toml, and triggers
    /// a live reload.
    pub(crate) settings_pane_revision: u64,

    /// CRT post-fx master switch. When `false` the renderer scales every
    /// shader intensity by zero regardless of theme/per-slider values.
    /// Mutated by `SettingsAdapter::set_crt_enabled`.
    pub(crate) crt_enabled: bool,

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
    pub(crate) quarantine_registry:
        std::sync::Arc<std::sync::Mutex<phantom_agents::quarantine::QuarantineRegistry>>,

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
    /// Cooperative cancellation flag for the running git-refresh thread.
    ///
    /// Set to `true` on timeout so the thread can exit early rather than
    /// sending a stale GitStateChanged event after the handle is dropped.
    pub(crate) git_refresh_cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,

    // -- STT task handles (for abort on shutdown) --
    /// JoinHandles for the two background tokio tasks spawned by the STT
    /// pipeline.  Stored here so `shutdown()` can abort them immediately.
    pub(crate) stt_task_handles: Option<crate::stt::SttTaskHandles>,

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
    /// GPT-4V analyzer. `None` when `OPENAI_API_KEY` is absent or the
    /// capture pipeline is disabled. When present, frames that pass the
    /// dHash+SAD dedup gate are forwarded to GPT-4V asynchronously.
    ///
    /// Stored behind `Arc` so the capture loop can clone a cheap handle
    /// into each spawned tokio task without blocking the render thread.
    pub(crate) vision_analyzer: Option<std::sync::Arc<phantom_vision::VisionAnalyzer>>,

    // -- Embedding backend (optional, initialized from OPENAI_API_KEY).
    //    `None` when `OPENAI_API_KEY` is absent or empty — the capture
    //    pipeline persists bundles without vector indexing in that case.
    //    Stored as `Arc<dyn EmbeddingBackend>` so it can be shared with
    //    off-thread persistence jobs via `Arc::clone` without holding a
    //    reference back to the `App`.
    pub(crate) embedding_backend:
        Option<std::sync::Arc<dyn phantom_embeddings::EmbeddingBackend>>,

    // -- STT pipeline (None when no API key is configured or privacy mode is on) --
    //    Constructed at boot via `SttPipeline::build()`. Holds the audio sender
    //    half of the capture pipeline; drop to shut down gracefully.
    //    Pending mic-capture integration (issue #56/#68): `push_chunk` and
    //    `drain_stt_events` will be called here once ScreenCaptureKit audio is wired.
    #[allow(dead_code)]
    pub(crate) stt: Option<crate::stt::SttPipeline>,

    // -- Per-pane last-command tracking (issue #226).
    //    Populated from `Event::CommandStarted` so that the subsequent
    //    `Event::CommandComplete` handler in `drain_bus_to_brain` can feed
    //    a real command string into `ParsedOutput::command` instead of the
    //    empty string that made OODA fix/explain scoring always return 0.
    pub(crate) pane_last_command: std::collections::HashMap<u32, String>,

    // -- Live shader reloader (debug + `live-reload` feature only).
    //    No-op stub in release builds; zero overhead on the hot path.
    pub(crate) shader_reloader: phantom_renderer::shader_loader::ShaderReloader,

    // -- Config-file watcher: monitors `settings.toml` for on-disk changes
    //    and triggers `apply_config_reload` in `App::update`. `None` when
    //    `notify` setup fails (unsupported path, permissions); the app boots
    //    normally without live-reload in that case.
    pub(crate) config_watcher: Option<crate::config_watcher::ConfigWatcher>,

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

    // -- Issue #323: alt-screen split-pane state --
    //
    // Maps primary adapter AppId → secondary (view) adapter AppId.  An entry
    // exists for the lifetime of a split pane, from the moment a terminal
    // enters alt-screen mode until the secondary pane is fully collapsed.
    pub(crate) alt_screen_secondaries:
        std::collections::HashMap<phantom_adapter::AppId, phantom_adapter::AppId>,

    // Previous `is_detached` state per adapter, used for edge detection in
    // `poll_alt_screen_transitions` (rising edge → split, falling edge → collapse).
    pub(crate) prev_detached: std::collections::HashMap<phantom_adapter::AppId, bool>,

    // Per secondary-pane collapse animation progress (0.0 → 1.0 over 300 ms).
    // Keyed by secondary adapter AppId.  Entries are created on collapse trigger
    // and removed once the fade completes and the pane is removed.
    pub(crate) alt_screen_fade: std::collections::HashMap<phantom_adapter::AppId, f32>,

    // Queue of secondary adapter AppIds whose collapse animations have
    // completed and are ready for `remove_adapter`.  Drained by
    // `tick_alt_screen_fade` into `collapse_alt_screen_pane`.
    pub(crate) alt_screen_pending_collapses: Vec<phantom_adapter::AppId>,

    // -- TTS pipeline (optional: None when OPENAI_API_KEY is absent or TTS
    //    is otherwise unavailable). Receives full assistant messages from
    //    agent panes and speaks them aloud via the system audio device.
    pub(crate) tts: Option<crate::tts::TtsPipeline>,
    // Keeps the background worker task alive for the process lifetime.
    #[allow(dead_code)]
    pub(crate) _tts_handles: Option<crate::tts::TtsTaskHandles>,

    // -- Privacy mode: hard block on cloud API calls.
    //    Mirrors `PhantomConfig::privacy_mode`. Toggled at runtime by the
    //    `ghost privacy on/off` command. When `true`:
    //    - `phantom_agents::PrivacyGuard` returns `ChatError::PrivacyModeViolation`
    //      on every cloud-backend call.
    //    - The brain router skips cloud backends (Claude, OpenAI).
    //    - The status strip shows a lock indicator.
    pub(crate) privacy_mode: bool,

    // -- Per-peer capability grant registry (issue #8).
    //    Governs what remote peers are allowed to do when their agent envelopes
    //    arrive via the relay. Unknown peers default to deny-all; explicit
    //    grants are added via `ghost grant <peer> <capability>`.
    //    Persisted to `~/.config/phantom/peer_grants.json` on every mutation
    //    and reloaded on boot so grants survive restarts.
    pub(crate) peer_grant_registry: phantom_agents::PeerGrantRegistry,

    // -- OODA signal cache (#358) -------------------------------------------
    //
    // Lightweight cache updated from bus events so `build_world_state()` can
    // assemble a fully populated `WorldState` in O(1) without scanning the PTY
    // buffer on every frame.
    //
    // `ooda_last_parsed` — most recent `ParsedOutput` from a `CommandComplete`
    //   event; `None` before the first command finishes.
    pub(crate) ooda_last_parsed: Option<phantom_semantic::ParsedOutput>,
    // `ooda_agent_just_completed` — set to `true` when an `AgentComplete`
    //   event arrives; cleared to `false` at the end of `build_world_state()`
    //   so it fires for exactly one OODA tick.
    pub(crate) ooda_agent_just_completed: bool,
    // `ooda_git_changed` — set to `true` when `GitStateChanged` is received;
    //   cleared after one OODA tick (same single-frame pulse pattern).
    pub(crate) ooda_git_changed: bool,

    // -- MCP tool registry: shared across all agent panes for external tool
    //    fallback. Populated by `mcp_discovery::discover_and_connect` running
    //    in a background tokio task at startup. Handed to each new AgentPane
    //    via `AgentPane::set_mcp_registry` so dispatch can route unknown tool
    //    names to connected MCP servers.
    pub(crate) mcp_registry:
        std::sync::Arc<tokio::sync::RwLock<phantom_mcp::McpToolRegistry>>,

    // -- Background MCP discovery task (kept alive for the App lifetime).
    //    `None` when `config.mcp_servers` is empty (no work to do).
    #[allow(dead_code)]
    pub(crate) _mcp_discovery_task: Option<tokio::task::JoinHandle<()>>,
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
        let mut text_renderer = TextRenderer::with_font_family(scaled_font_size, config.font_family.clone());
        let cell_size = text_renderer.measure_cell();
        info!(
            "Cell size: {:.1}x{:.1} at {:.0}pt",
            cell_size.0, cell_size.1, DEFAULT_FONT_SIZE
        );

        // -- Atlas --
        let atlas = GlyphAtlas::new(&gpu.device, &gpu.queue);
        let color_atlas = ColorGlyphAtlas::new(&gpu.device, &gpu.queue);

        // -- Renderers --
        let quad_renderer = QuadRenderer::new(&gpu.device, format);
        let grid_renderer = GridRenderer::new(&gpu.device, format, atlas.bind_group_layout());
        let color_grid_renderer =
            GridRenderer::new_color(&gpu.device, format, color_atlas.bind_group_layout());
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
        // Use ContextAssembler so subsequent agent invocations benefit from
        // DAG caching rather than calling ProjectContext::detect() on every spawn.
        let mut context_assembler = ContextAssembler::new();
        let context = context_assembler.assemble(Path::new(&project_dir));
        let context_assembler =
            std::sync::Arc::new(std::sync::Mutex::new(context_assembler));
        info!(
            "Project detected: {} [{:?}]",
            context.name, context.project_type
        );

        // Wire context into status bar.
        status_bar.set_cwd(&context.name);
        if let Some(ref git) = context.git {
            status_bar.set_branch(&git.branch);
        }
        // Reflect initial privacy mode in the status bar indicator.
        if config.privacy_mode {
            status_bar.set_privacy_mode(true);
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
        let notification_store =
            match phantom_memory::notifications::NotificationStore::open(&project_dir) {
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

        // -- Capability audit (offline-readiness report, Issue #362) --
        // Run before the brain thread starts so the report is in the log
        // before any AI dispatch happens.  The audit is synchronous and
        // short (≤ 2 s Ollama ping).  We run it on this thread because
        // `with_config_scaled` is already called off the render thread.
        {
            // Mirror the user's `PhantomConfig::privacy_mode` so the audit
            // report correctly classifies cloud subsystems as
            // `BlockedByPolicy` when privacy mode is on. See issue #446.
            let audit_config = AuditConfig {
                privacy_mode: config.privacy_mode,
            };
            let report = CapabilityReport::compute(&audit_config);
            report.log_report();
            if !report.all_online_or_blocked() {
                warn!(
                    "[capability-audit] {} subsystem(s) unavailable — some AI features will not work",
                    report.unavailable_entries().len()
                );
            }
        }

        // -- Federation relay task --
        // When PHANTOM_RELAY_URL (or PHANTOM_HUB_URL as a convenience alias)
        // is set, create an inbound channel and spawn a background OS thread
        // that connects to the relay WebSocket and forwards inbound frames to
        // the brain.  relay_inbound_rx is `None` when no relay URL is
        // configured, keeping the federation path fully disabled in standalone
        // mode.
        let (relay_inbound_rx, relay_connected_rx, relay_task) = build_relay_channels();

        // -- AI Brain thread --
        // Build a RouterConfig that respects the user's preferred_provider setting.
        // When preferred_provider is set, the named backend is promoted to the front
        // of the cascade so it is tried first. When absent, the default order is used.
        let brain_router_config = match config.preferred_provider() {
            Some(id) => {
                info!("AI brain: preferred_provider = '{id}' — promoting backend in cascade");
                phantom_brain::router::RouterConfig::with_preferred_provider(id)
            }
            None => phantom_brain::router::RouterConfig::default(),
        };
        // Always apply privacy/offline mode from the application config into the
        // router config so the router enforces the policy from the very first event.
        let brain_router_config = phantom_brain::router::RouterConfig {
            privacy_mode: config.privacy_mode,
            offline_mode: config.offline_mode,
            ..brain_router_config
        };
        // Seed the brain's initial history snapshot (up to 20 most recent commands).
        let initial_history_context: Vec<phantom_history::HistoryEntry> = history
            .as_ref()
            .and_then(|s| s.recent(20).ok())
            .unwrap_or_default();
        let brain = spawn_brain(BrainConfig {
            project_dir: project_dir.clone(),
            enable_suggestions: true,
            enable_memory: true,
            quiet_threshold: 0.35, // Tuned: high enough to suppress noise, low enough for real events
            router: Some(brain_router_config),
            catalog: Some(phantom_brain::provider_catalog::ProviderCatalog::with_builtins()),
            privacy_mode: config.privacy_mode,
            // relay_inbound_rx is Some when PHANTOM_RELAY_URL is configured
            // (wired by build_relay_channels above), None in standalone mode.
            relay_inbound_rx,
            recall_context: None,
            history_context: initial_history_context,
            // Self-improvement defaults to OFF per design doc §5.1; the
            // operator opts in by setting `BrainConfig::self_improvement` and
            // `BrainConfig::goal_sources` (typically via a future PR that
            // adds the `ghost self-improve on` command).
            self_improvement: None,
            goal_sources: Vec::new(),
        });
        if config.privacy_mode {
            info!("Privacy mode enabled — cloud API calls blocked");
        }
        info!("AI brain spawned");

        // -- Semantic skill (SkillHost-routed — static or dylib, with fallback).
        //    SkillHost::build() tries the dylib when PHANTOM_HOT_MODULES=1 and the
        //    hot-modules feature is active; on any failure it falls back to the
        //    static SemanticParser path so boot is never blocked.
        let semantic_skill = phantom_skill_host::SkillHost::build();
        info!("Semantic skill host initialized");

        // -- NLP translate channel (bounded at 8 outstanding requests) --
        let (nlp_translate_tx, nlp_translate_rx) =
            std::sync::mpsc::sync_channel::<NlpTranslateResult>(8);

        // Build the LLM backend via LlmHost (phantom-skill-host), which routes
        // through the phantom-nlp dylib when PHANTOM_HOT_MODULES=1 (hot-reload,
        // closes #383) or falls back to the static Claude → Ollama path.
        let nlp_backend: Option<std::sync::Arc<dyn LlmSkill>> = if config.nlp_llm_enabled {
            match LlmHost::build() {
                Some(skill) => {
                    info!("NLP LLM backend: {} (via LlmHost)", skill.name());
                    Some(skill)
                }
                None => {
                    debug!("NLP LLM backend: disabled (no backend reachable)");
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

        // -- STT pipeline (best-effort: None when no key or privacy mode) --
        // WARNING: `SttPipeline::start` calls `tokio::spawn` internally.
        // `with_config_scaled` runs on the winit `resumed()` callback where
        // no tokio runtime is in scope — so this WILL panic at startup
        // whenever `OPENAI_API_KEY` is set (same failure mode diagnosed
        // earlier for the TTS pipeline). The fix is to move `SttPipeline::build`
        // onto a dedicated OS thread that owns a `new_current_thread()`
        // runtime, mirroring the `mcp-discovery` pattern at line ~1311.
        // Tracked as a follow-up to PR #594.
        let stt = if config.privacy_mode {
            log::info!("STT: disabled — privacy mode is on");
            None
        } else {
            crate::stt::SttPipeline::build()
        };

        // -- Embedding backend (optional). Constructed from OPENAI_API_KEY when
        //    present so the capture pipeline can vector-index sealed bundles.
        //    When the key is absent we store None and log at debug level — bundles
        //    still persist with metadata; only vector search is unavailable.
        //    The client is cached inside the backend (not per-request) so the
        //    Arc<dyn EmbeddingBackend> can be shared cheaply with job workers.
        let embedding_backend: Option<std::sync::Arc<dyn phantom_embeddings::EmbeddingBackend>> =
            match phantom_embeddings::openai::OpenAiEmbeddingBackend::from_env() {
                Ok(backend) => {
                    info!("Embedding backend ready (OpenAI text-embedding-3-large)");
                    Some(std::sync::Arc::new(backend))
                }
                Err(phantom_embeddings::EmbedError::NotConfigured { .. }) => {
                    debug!("OPENAI_API_KEY not set — embedding backend disabled");
                    None
                }
                Err(e) => {
                    warn!("Embedding backend init failed: {e} — vector indexing disabled");
                    None
                }
            };

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
                        let ap = AgentStatePersister::new(AgentStatePersister::sidecar_path(
                            &session_path,
                        ));
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

                        let gp = GoalStatePersister::new(GoalStatePersister::sidecar_path(
                            &session_path,
                        ));
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

        // -- Session restore: load agent + goal snapshots from sidecar files --
        // Called after both persisters are initialised so the sidecar paths are
        // known.  Partial failure (corrupt sidecar on one side) is handled
        // gracefully inside `SessionRestorer::restore`.  The result is stored on
        // `App` and consumed on the first Terminal-state OODA tick in `update.rs`.
        let restored_session: Option<RestoredSession> = {
            let agent_path = agent_persister.as_ref().map(|p| p.path().to_path_buf());
            let goal_path = goal_persister.as_ref().map(|p| p.path().to_path_buf());
            let session = SessionRestorer::restore(
                agent_path.as_deref(),
                goal_path.as_deref(),
            );
            if session.is_empty() {
                None
            } else {
                info!(
                    "RestoredSession ready: {} agent(s), {} goal(s)",
                    session.agent_count(),
                    session.goal_count(),
                );
                Some(session)
            }
        };

        // -- Event bus (single instance, will be handed to AppCoordinator) --
        let mut event_bus = EventBus::new();
        let topic_terminal_output =
            event_bus.create_topic(0, "terminal.output", DataType::TerminalOutput);
        let topic_terminal_error = event_bus.create_topic(0, "terminal.error", DataType::Text);
        let topic_agent_event = event_bus.create_topic(0, "agent.event", DataType::Json);
        // Issue #79 item 7: topic for post-dedup frame notifications.
        let topic_capture_frame = event_bus.create_topic(0, "capture.frame", DataType::Json);

        // Chrome-pane notification topics — registered at boot so the
        // NotificationsAdapter's `subscribes_to()` resolves to live topic
        // IDs when the coordinator wires the subscription.  Without these
        // the registration logs a warn and the adapter silently never
        // receives anything.
        //
        // Publishers:
        // - `agent.denied`  — the capability-denial drain in `update.rs`
        //   forwards every `EventKind::CapabilityDenied` as a typed
        //   `Event::Custom { kind: "agent.denied", .. }`.
        // - `brain.suggestion` — reserved for the brain reconciler's
        //   advice surface (publisher lands with #647).
        // - `system.warn` — reserved for the global logger to forward
        //   warn/error log records.
        event_bus.create_topic(0, "agent.denied", DataType::Json);
        event_bus.create_topic(0, "brain.suggestion", DataType::Json);
        event_bus.create_topic(0, "system.warn", DataType::Json);

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
        let mut plugin_registry = match PluginRegistry::new() {
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

        // Scan for installed plugins and auto-load them; errors per-plugin are
        // logged as warnings inside scan() so one bad plugin cannot block boot.
        match plugin_registry.scan() {
            Ok(manifests) => {
                info!(
                    "Plugin scan complete: {} manifest(s) found, {} plugin(s) loaded",
                    manifests.len(),
                    plugin_registry.len()
                );
            }
            Err(e) => {
                warn!("Plugin scan failed: {e}");
            }
        }

        // Dispatch OnStartup to all loaded plugins now that the registry is ready.
        //
        // TODO(#48): Phase-1 limitation — responses are logged but not acted on.
        // `HookResponse::RunCommand`, `ModifyOutput`, and `Notification` returned
        // from `OnStartup` are intentionally dropped here because the agent /
        // terminal / notification dispatch surfaces are not wired into this boot
        // path. When the plugin host gains a typed response router, route the
        // matched arms (`RunCommand` -> command bus, `Notification` -> notifier,
        // etc.) instead of unconditionally logging.
        {
            let startup_ctx = phantom_plugins::HookContext::startup(&project_dir);
            let responses = plugin_registry
                .dispatch_hook(&phantom_plugins::HookType::OnStartup, &startup_ctx);
            for resp in &responses {
                info!("[plugin startup]: {resp:?}");
            }
        }

        // -- Job pool (4 workers for async brain queries, resource loading, etc.) --
        let job_pool = crate::jobs::JobPool::start_up(4);
        info!("Job pool initialized: 4 workers");

        // -- App coordinator (owns all adapters, dispatches update/render/input) --
        let mut coordinator = AppCoordinator::new(event_bus);

        // -- Register initial SetupAdapter as the sole pane --
        //
        // The cold-launch first impression is "Phantom IS the AI", not "Phantom
        // is a terminal" (see `feedback_agent_is_primary` memory).  We
        // register a dependency-free `SetupAdapter` here.  On the first
        // `update.rs` tick the post-boot agent-spawn code runs; if an API key
        // is available the existing `adapter_count() == 1 → kill_keeping_pane`
        // replace-focused path swaps SetupAdapter out for the real agent at
        // the SAME pane slot — no split, no half-window agent.  If no key is
        // available SetupAdapter stays put with a "needs API key" message.
        //
        // The early `terminal: PhantomTerminal` built above at line ~679 is
        // now unused at init; we drop it.  Cmd+T constructs a fresh terminal
        // via `PhantomTerminal::new` in `pane.rs` when the user actually wants
        // one.  Letting `terminal` go out of scope releases the PTY cleanly
        // via `Drop`.
        drop(terminal);

        let post_setup_upgrade = std::sync::Arc::new(
            std::sync::atomic::AtomicBool::new(false),
        );
        {
            use crate::adapters::setup::SetupAdapter;
            use phantom_scene::clock::Cadence;

            let adapter = SetupAdapter::new(std::sync::Arc::clone(&post_setup_upgrade));
            let _app_id = coordinator.register_adapter_at_pane(
                Box::new(adapter),
                pane_id,
                first_pane_node,
                Cadence::unlimited(),
                &mut layout,
            );
            info!("Initial SetupAdapter registered (AppId {_app_id}) — agent is king");
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
        // Clone sender before moving into spawn_listener; the hub listener
        // shares the same mpsc channel so both transports funnel to one receiver.
        let hub_cmd_tx = mcp_cmd_tx.clone();
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

        // -- Hub registration listener (issues #395, #398) --
        // When PHANTOM_HUB_URL is set, dial out to phantom-hub and register
        // this Phantom instance so Claude can reach it remotely.  If the env
        // var is absent this is a graceful no-op.
        //
        // The identity and JWT are loaded from the on-disk identity and
        // credentials files inside spawn_hub on each connection attempt
        // (issue #398, file-backed per #539).  Run `phantom auth register
        // --hub <url>` before launching to populate the credentials file;
        // without it the hub will reject the connection.
        let hub_listener = {
            let hub_url = std::env::var("PHANTOM_HUB_URL").unwrap_or_default();
            match phantom_mcp::spawn_hub(&hub_url, hub_cmd_tx) {
                Ok(Some(hl)) => {
                    info!("Hub listener started: {}", hl.hub_url());
                    Some(hl)
                }
                Ok(None) => {
                    debug!("Hub URL not configured — hub registration skipped");
                    None
                }
                Err(e) => {
                    warn!("Failed to start hub listener: {e}");
                    None
                }
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

        // -- MCP external-server discovery (Bug 3 fix, env-var path).
        //
        // Spawn an async task that connects to each URL listed in
        // `$PHANTOM_MCP_SERVERS` (comma-separated), calls `tools/list` on each,
        // and sets `mcp_discovery_complete` to `true` when done.
        //
        // Agent spawn checks this flag and waits up to 500 ms so that agents
        // started immediately after boot have access to the full external tool
        // list. If discovery is still running after 500 ms, the agent proceeds
        // with whatever tools are available (empty external list is acceptable).
        let mcp_discovery_complete =
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        {
            let flag = std::sync::Arc::clone(&mcp_discovery_complete);
            let server_urls: Vec<String> = std::env::var("PHANTOM_MCP_SERVERS")
                .unwrap_or_default()
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect();

            if server_urls.is_empty() {
                // No external servers configured — mark discovery done immediately.
                flag.store(true, std::sync::atomic::Ordering::Release);
                debug!("MCP discovery: no PHANTOM_MCP_SERVERS configured, skipping");
            } else {
                // Spin up a one-shot tokio runtime on a background thread so we
                // don't block the winit event loop.
                std::thread::Builder::new()
                    .name("mcp-discovery".into())
                    .spawn(move || {
                        let rt = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .expect("mcp-discovery: tokio runtime");
                        rt.block_on(async move {
                            let mut registry = phantom_mcp::McpToolRegistry::new();
                            for url in &server_urls {
                                match phantom_mcp::McpClient::connect(url).await {
                                    Ok(mut client) => {
                                        if let Err(e) = client.list_tools().await {
                                            warn!("MCP discovery: list_tools failed for {url}: {e}");
                                        }
                                        let server_name = url
                                            .trim_start_matches("ws://")
                                            .trim_start_matches("wss://")
                                            .to_owned();
                                        registry.register_server(&server_name, client);
                                        info!("MCP discovery: registered server {url} ({} tools)",
                                            registry.tool_count());
                                    }
                                    Err(e) => {
                                        warn!("MCP discovery: connect failed for {url}: {e}");
                                    }
                                }
                            }
                            // Mark discovery done so agents can proceed.
                            flag.store(true, std::sync::atomic::Ordering::Release);
                            info!("MCP discovery complete ({} tools indexed)", registry.tool_count());
                        });
                    })
                    .expect("mcp-discovery thread spawn");
            }
        }

        // -- TTS pipeline (best-effort; no-op when OPENAI_API_KEY is absent) --
        // Privacy mode blocks cloud API calls, so we skip TTS init to avoid
        // a live network call on first synthesis.
        let (tts, _tts_handles) = if config.privacy_mode {
            debug!("TTS pipeline disabled (privacy mode)");
            (None, None)
        } else {
            match crate::tts::build_tts_pipeline_from_env() {
                Some((p, h)) => (Some(p), Some(h)),
                None => (None, None),
            }
        };

        // -- MCP config-driven tool registry (shared across agent panes).
        // `discover_and_connect` is spawned as a background task so it does
        // not block the GPU / render thread during startup. This complements
        // the env-var discovery above; both populate registries used by agents.
        let mcp_registry = std::sync::Arc::new(tokio::sync::RwLock::new(
            phantom_mcp::McpToolRegistry::new(),
        ));
        let mcp_discovery_task: Option<tokio::task::JoinHandle<()>> =
            if config.mcp_servers.is_empty() {
                None
            } else {
                let servers = config.mcp_servers.clone();
                let registry = std::sync::Arc::clone(&mcp_registry);
                let handle = tokio::spawn(async move {
                    crate::mcp_discovery::discover_and_connect(&servers, registry).await;
                });
                Some(handle)
            };

        let now = Instant::now();

        Ok(Self {
            gpu,
            atlas,
            color_atlas,
            text_renderer,
            quad_renderer,
            grid_renderer,
            color_grid_renderer,
            postfx,
            layout,
            keybinds,
            theme,
            status_bar,
            tab_bar,
            keybind_help: KeybindHelp::new(),
            search_bar: SearchBar::new(),
            boot,
            state: initial_state,
            demo_mode: config.demo_mode,
            demo_post_boot_done: false,
            post_boot_agent_spawned: false,
            post_setup_upgrade,
            start_time: now,
            last_frame: now,
            dt_clamp: phantom_scene::DtClamp::default_60fps(),
            scene_clock: phantom_scene::Clock::new(),
            cursor_blink: phantom_ui::CursorBlink::default(),
            cell_size,
            quit_requested: false,
            force_redraw: true,
            supervisor,
            console: crate::console::Console::new(),
            debug_hud: false,
            debug_hud_selected: 0,
            brain: Some(brain),
            semantic_skill,
            ooda_loop: OodaLoop::new(OodaConfig::default()),
            runtime,
            context: Some(context),
            context_assembler,
            memory,
            notification_store,
            session_manager,
            agent_persister,
            goal_persister,
            restored_session,
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
            _hub_listener: hub_listener,
            _relay_task: relay_task,
            relay_connected_rx,
            mcp_discovery_complete,
            pool_quads: Vec::with_capacity(256),
            pool_glyphs: Vec::with_capacity(4096),
            pool_color_glyphs: Vec::with_capacity(64),
            pool_chrome_quads: Vec::with_capacity(32),
            pool_chrome_glyphs: Vec::with_capacity(256),
            fullscreen_pane: None,
            inspector_snapshot: None,
            inspector_tokens: None,
            settings_pane_id: None,
            notifications_pane_id: None,
            console_pane_id: None,
            keybinds_help_pane_id: None,
            logs_pane_id: None,
            files_watch_pane_id: None,
            diff_pane_id: None,
            memory_pane_id: None,
            fleet_pane_id: None,
            plugins_pane_id: None,
            database_pane_id: None,
            voice_stt_pane_id: None,
            files_watcher: None,
            logs_watermark: 0,
            settings_pane_revision: 0,
            crt_enabled: true,
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
            git_refresh_cancel: None,
            stt_task_handles: None,
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
            stt,
            bundle_store,
            capture_state: crate::capture::CaptureState::new(),
            vision_analyzer: phantom_vision::VisionAnalyzer::from_env()
                .ok()
                .map(std::sync::Arc::new),
            embedding_backend,
            ticket_dispatcher,
            pane_last_command: std::collections::HashMap::new(),
            shader_reloader: phantom_renderer::shader_loader::ShaderReloader::new(),
            config_watcher: crate::config_watcher::ConfigWatcher::new(
                &crate::settings::PhantomSettings::default_path(),
            ),
            history,
            agent_capture,
            session_uuid,
            pending_command_text: std::collections::HashMap::new(),
            alt_screen_secondaries: std::collections::HashMap::new(),
            prev_detached: std::collections::HashMap::new(),
            alt_screen_fade: std::collections::HashMap::new(),
            alt_screen_pending_collapses: Vec::new(),
            privacy_mode: config.privacy_mode,
            peer_grant_registry: crate::peer_grants::load_peer_grant_registry(),
            tts,
            _tts_handles,
            // OODA signal cache — all start zeroed; populated by drain_bus_to_brain.
            ooda_last_parsed: None,
            ooda_agent_just_completed: false,
            ooda_git_changed: false,
            mcp_registry,
            _mcp_discovery_task: mcp_discovery_task,
        })
    }

    /// Returns `true` if the app has requested to quit.
    pub fn should_quit(&self) -> bool {
        self.quit_requested
    }

    /// Returns `true` when the scene graph has at least one dirty node,
    /// meaning the GPU pipeline has pending work to upload this frame.
    pub fn scene_is_dirty(&self) -> bool {
        self.scene.has_dirty_nodes()
    }

    /// Returns `true` when a visual animation is in progress that requires
    /// the render loop to keep running even if the scene graph is clean.
    pub fn has_active_animation(&self) -> bool {
        // Boot sequence is animating.
        if self.state == AppState::Boot && !self.boot.is_done() {
            return true;
        }
        // Terminal mode: cursor blinking requires continuous repaints.
        if self.state == AppState::Terminal {
            return true;
        }
        // Quake console is visible (slide animation or idle — keeps animating).
        if self.console.visible() {
            return true;
        }
        // Per-keystroke glitch effect is running.
        if self.keystroke_fx.is_active() {
            return true;
        }
        // Alt-screen fade-in/out.
        if !self.alt_screen_fade.is_empty() {
            return true;
        }
        false
    }

    /// Set the force-redraw latch so at least one render happens this frame
    /// even if the scene graph is clean.
    ///
    /// Call this from event handlers (key, mouse, resize, focus, OODA action)
    /// to ensure the frame loop re-arms itself.
    pub fn request_redraw(&mut self) {
        self.force_redraw = true;
    }

    /// Returns the current value of the force-redraw latch.
    ///
    /// Used by the winit event loop to decide whether to re-arm
    /// `window.request_redraw()` after a static frame.
    pub fn needs_force_redraw(&self) -> bool {
        self.force_redraw
    }

    /// Drain the latest OSC 2 window title from the focused terminal adapter.
    ///
    /// Returns the title string when the running program has emitted a new
    /// title since the last call.  The caller should forward this to
    /// `winit_window.set_title()`.  Returns `None` when no change arrived.
    pub fn take_pending_window_title(&mut self) -> Option<String> {
        self.coordinator.take_focused_window_title()
    }

    /// Watchdog trace: returns a log line every `interval` frames.
    /// Written directly to disk by the event loop (bypasses Rust logger,
    /// survives SIGKILL mid-frame).
    pub fn watchdog_trace(&mut self, interval: u64) -> Option<String> {
        self.watchdog_frame += 1;
        if !self.watchdog_frame.is_multiple_of(interval) {
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

    /// Notify the supervisor that the render loop is escalating past the
    /// consecutive-panic threshold and is about to force-exit.
    ///
    /// Best-effort: if the socket is broken the error is silently ignored.
    pub fn notify_render_panic(&mut self, count: u32, last_message: &str) {
        if let Some(ref mut sv) = self.supervisor {
            sv.notify_render_panic(count, last_message);
        }
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
                            Ok(()) => {
                                info!("Agent state saved ({} snapshot(s))", file.agent_count())
                            }
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

        // Abort STT background tasks immediately so the pipeline does not
        // block shutdown waiting for the channel-close cascade to propagate
        // through a long STT backend call.
        if let Some(handles) = self.stt_task_handles.take() {
            handles.abort();
        }

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
            GoalSnapshot, PlanStepBuilder, SavedFact, SavedFactConfidence, SavedStepStatus,
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
            phantom_agents::AgentTask::FreeForm {
                prompt: objective.to_owned(),
            },
        )
        .status(SavedStepStatus::Pending)
        .build();

        let snapshot = GoalSnapshot::new(
            objective.to_owned(),
            vec![SavedFact::new(
                "goal set by user",
                SavedFactConfidence::Verified,
                "commands",
            )],
            vec![initial_step],
            vec![], // plan_history: no prior plans yet
            0,      // stall_counter
            2,      // stall_threshold (matches brain default)
            0,      // replan_count
            5,      // max_replans (matches brain default)
            created_at_secs,
            None, // last_replan_at_secs
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
            if let Some(pane_id) = self.coordinator.pane_id_for(app_id)
                && let Ok(rect) = self.layout.get_pane_rect(pane_id) {
                    let (cols, rows) = pane_cols_rows(self.cell_size, rect);
                    let _ = self.coordinator.send_command(
                        app_id,
                        "resize",
                        &serde_json::json!({"cols": cols, "rows": rows}),
                    );
                    trace!("Adapter {app_id} resized to {cols}x{rows}");
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

    // -----------------------------------------------------------------------
    // Config live-reload
    // -----------------------------------------------------------------------

    /// Apply a freshly-loaded [`PhantomSettings`] to the live app state.
    ///
    /// Called from `App::update` when [`ConfigWatcher::drain_changes`] returns
    /// `true`. Updates the theme name, shader params, and font size from the
    /// new settings so changes written to `settings.toml` from an external
    /// editor (or via the settings panel) are reflected immediately.
    pub(crate) fn apply_config_reload(&mut self, settings: &crate::settings::PhantomSettings) {
        use phantom_ui::themes;

        // Theme: resolve the named built-in and swap if it changed.
        if !settings.theme.eq_ignore_ascii_case(&self.theme.name) {
            if let Some(new_theme) = themes::builtin_by_name(&settings.theme) {
                self.theme = new_theme;
                info!("Config live-reload: theme → {}", settings.theme);
            } else {
                warn!(
                    "Config live-reload: unknown theme '{}', keeping current",
                    settings.theme
                );
            }
        }

        // CRT shader params.
        let sp = &mut self.theme.shader_params;
        let crt = &settings.crt;
        sp.scanline_intensity = crt.scanline_intensity;
        sp.bloom_intensity = crt.bloom_intensity;
        sp.chromatic_aberration = crt.chromatic_aberration;
        sp.curvature = crt.curvature;
        sp.vignette_intensity = crt.vignette_intensity;
        sp.noise_intensity = crt.noise_intensity;

        // Font size: only change if it actually differs (avoid atlas churn).
        let current_size = self.text_renderer.font_size();
        if (settings.font_size - current_size).abs() > 0.5 {
            self.text_renderer.set_font_size(settings.font_size);
            self.cell_size = self.text_renderer.measure_cell();
            self.atlas.clear();
            info!(
                "Config live-reload: font_size → {:.0}pt",
                settings.font_size
            );
        }

    }
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
fn build_ticket_dispatcher()
-> Option<std::sync::Arc<phantom_agents::dispatcher::GhTicketDispatcher>> {
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

    // Allow operators to fully skip bundle-store init (and its keychain
    // touch) via env var. Used in dev / CI where capture/persistence is
    // not needed and the keychain prompt is a friction point.
    if std::env::var("PHANTOM_DISABLE_BUNDLE_STORE")
        .ok()
        .as_deref()
        .map(|s| matches!(s, "1" | "true" | "TRUE"))
        .unwrap_or(false)
    {
        info!(
            "Bundle store disabled via PHANTOM_DISABLE_BUNDLE_STORE — capture pipeline will not initialize"
        );
        return None;
    }

    // Resolve master key from the on-disk store under `dirs::config_dir()`.
    // First run generates a fresh key and persists it at mode 0600; later
    // runs read it back. See `phantom-bundle-store::crypto` for the file
    // layout and the `PHANTOM_BUNDLE_STORE_MASTER_KEY_FILE` test override.
    let master_key = match MasterKey::load_or_generate() {
        Ok(k) => k,
        Err(e) => {
            warn!("Bundle store master-key load failed ({e}); skipping capture init");
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

// ---------------------------------------------------------------------------
// build_relay_channels — federation relay wiring
// ---------------------------------------------------------------------------

/// Handshake notification payload: sent by the relay task to the update loop
/// once the WebSocket handshake is complete.  The update loop uses this to
/// emit [`phantom_brain::events::AiEvent::RelayConnected`] to the brain.
pub(crate) struct RelayHandshakeInfo {
    /// Outbound channel: the brain's `AgentRouter` sends `(peer_id, json)`
    /// tuples here; the relay task's outbound leg forwards them to the relay
    /// WebSocket.
    ///
    /// # TODO — outbound forwarding leg
    /// The relay task currently only wires the inbound leg.  The outbound leg
    /// (brain → relay) requires phantom-net to expose a `select!`-friendly
    /// recv so the relay loop can simultaneously poll `outbound_rx` and
    /// `client.recv()` without blocking.  Track in phantom-net:
    /// "expose select-friendly recv on RelayClient so relay_task can drive
    /// inbound and outbound concurrently without blocking".
    pub outbound_tx: tokio::sync::mpsc::Sender<(String, String)>,
    /// This Phantom instance's peer id string (base58, derived from Ed25519 key).
    pub local_peer_id: String,
}

/// Set up the inbound relay channel and (optionally) spawn the relay task.
///
/// Returns `(relay_inbound_rx, relay_connected_rx, relay_task_handle)`:
/// - `relay_inbound_rx` goes to [`BrainConfig::relay_inbound_rx`] so the
///   brain drains inbound relay frames on its proactive tick.
/// - `relay_connected_rx` is a one-shot receiver.  The relay task sends a
///   [`RelayHandshakeInfo`] on it after the WebSocket handshake completes.
///   The update loop drains this and emits `AiEvent::RelayConnected` to the
///   brain to wire the `AgentRouter`.
/// - `relay_task_handle` keeps the OS thread alive.  All three are `None`
///   when neither `PHANTOM_RELAY_URL` nor `PHANTOM_HUB_URL` is set.
pub(crate) fn build_relay_channels() -> (
    Option<tokio::sync::mpsc::Receiver<String>>,
    Option<std::sync::mpsc::Receiver<RelayHandshakeInfo>>,
    Option<std::thread::JoinHandle<()>>,
) {
    // Accept PHANTOM_RELAY_URL as the canonical var; fall back to
    // PHANTOM_HUB_URL as a convenience alias so the same env var that
    // controls hub registration also enables the relay path.
    let relay_url = std::env::var("PHANTOM_RELAY_URL")
        .or_else(|_| std::env::var("PHANTOM_HUB_URL"))
        .ok()
        .filter(|s| !s.is_empty());

    let Some(relay_url) = relay_url else {
        debug!("PHANTOM_RELAY_URL not set — federation relay disabled");
        return (None, None, None);
    };

    info!("Federation relay URL configured: {relay_url}");

    // Inbound channel: relay task pushes raw JSON frames; brain drains them.
    let (inbound_tx, inbound_rx) = tokio::sync::mpsc::channel::<String>(256);

    // One-shot handshake notification: relay task → update loop.
    let (connected_tx, connected_rx) = std::sync::mpsc::sync_channel::<RelayHandshakeInfo>(1);

    let handle = std::thread::Builder::new()
        .name("phantom-relay-task".into())
        .spawn(move || {
            relay_task_body(&relay_url, inbound_tx, connected_tx);
        })
        .unwrap_or_else(|e| {
            warn!("Failed to spawn relay task: {e}; federation disabled");
            std::thread::Builder::new()
                .name("phantom-relay-noop".into())
                .spawn(|| {})
                .expect("noop thread")
        });

    (Some(inbound_rx), Some(connected_rx), Some(handle))
}

/// Body of the relay task OS thread.
///
/// Runs a single-threaded tokio runtime that:
/// 1. Loads the on-disk identity and (optional) device JWT.
/// 2. Connects to the relay WebSocket and completes the handshake.
/// 3. Notifies the update loop via `connected_tx` so it can emit
///    `AiEvent::RelayConnected` to the brain.
/// 4. Forwards inbound frames to `inbound_tx`.
///
/// Logs on error and returns without panicking so the App continues
/// without federation.
fn relay_task_body(
    relay_url: &str,
    inbound_tx: tokio::sync::mpsc::Sender<String>,
    connected_tx: std::sync::mpsc::SyncSender<RelayHandshakeInfo>,
) {
    use phantom_net::{DeviceCredentials, Identity};

    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            warn!("relay task: failed to build tokio runtime: {e}");
            return;
        }
    };

    rt.block_on(async move {
        // Load or generate the on-disk Ed25519 identity.
        let identity = match Identity::load_or_generate("phantom") {
            Ok(id) => id,
            Err(e) => {
                warn!("relay task: failed to load identity: {e}; federation disabled");
                return;
            }
        };

        let local_peer_id = identity.peer_id.to_string();
        info!("relay task: local peer id = {local_peer_id}");

        // Optionally load a device JWT for authenticated relay connections.
        let device_token: Option<String> = match DeviceCredentials::load("phantom") {
            Ok(Some(creds)) => {
                info!(
                    "relay task: device credentials loaded (hub={})",
                    creds.hub_url
                );
                Some(creds.jwt)
            }
            Ok(None) => {
                debug!(
                    "relay task: no device credentials — connecting without Authorization header; \
                     run `phantom auth register --hub <url>` to populate credentials"
                );
                None
            }
            Err(e) => {
                warn!(
                    "relay task: failed to load device credentials: {e}; connecting unauthenticated"
                );
                None
            }
        };

        // Connect to the relay.
        let token_ref = device_token.as_deref();
        let mut client = match phantom_net::client::RelayClient::connect_with_token(
            relay_url,
            identity,
            token_ref,
        )
        .await
        {
            Ok(c) => {
                info!("relay task: connected and handshake complete");
                c
            }
            Err(e) => {
                warn!(
                    "relay task: connection failed: {e}; federation disabled for this session"
                );
                return;
            }
        };

        // Notify the update loop that the handshake is done.  Create the
        // outbound channel now so the brain's AgentRouter can send messages
        // to the relay.  The outbound_rx end is held here; the sender is
        // handed to the brain via AiEvent::RelayConnected.
        //
        // TODO: drive the outbound_rx in the loop below once phantom-net
        // exposes a select!-friendly recv — see RelayHandshakeInfo TODO.
        let (outbound_tx, _outbound_rx) =
            tokio::sync::mpsc::channel::<(String, String)>(64);
        let _ = connected_tx.try_send(RelayHandshakeInfo {
            outbound_tx,
            local_peer_id,
        });

        // Forward inbound frames to the brain's relay_inbound_rx.
        loop {
            match client.recv().await {
                Ok(envelope) => match String::from_utf8(envelope.payload) {
                    Ok(json) => {
                        if inbound_tx.send(json).await.is_err() {
                            debug!("relay task: brain inbound channel closed; shutting down");
                            break;
                        }
                    }
                    Err(e) => {
                        warn!("relay task: non-UTF-8 envelope payload ignored: {e}");
                    }
                },
                Err(e) => {
                    warn!("relay task: recv error: {e}; closing relay connection");
                    break;
                }
            }
        }

        info!("relay task: exiting");
    });
}

#[cfg(test)]
mod tests {
    use super::open_bundle_store;

    // -- Helpers for env-var mutation in tests --------------------------------

    /// Serialize env-var mutations so parallel tests in the same binary
    /// don't stomp on each other.
    fn with_env_var<F: FnOnce()>(key: &str, value: Option<&str>, f: F) {
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var(key).ok();
        // SAFETY: serialized by LOCK.
        unsafe {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
        f();
        unsafe {
            match prior {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }

    // -- Embedding backend construction tests --------------------------------

    /// `embedding_backend_created_when_key_present`
    ///
    /// When `OPENAI_API_KEY` is set to a non-empty value,
    /// `OpenAiEmbeddingBackend::from_env()` must succeed and produce a
    /// backend whose `name()` is `"openai-embedding"`.
    #[test]
    fn embedding_backend_created_when_key_present() {
        use phantom_embeddings::EmbeddingBackend;
        with_env_var("OPENAI_API_KEY", Some("sk-test-fixture"), || {
            let backend = phantom_embeddings::openai::OpenAiEmbeddingBackend::from_env()
                .expect("from_env should succeed when key is set");
            assert_eq!(backend.name(), "openai-embedding");
        });
    }

    /// `embedding_backend_none_when_no_key`
    ///
    /// When `OPENAI_API_KEY` is absent, `OpenAiEmbeddingBackend::from_env()`
    /// must return `Err(EmbedError::NotConfigured(_))` — the App then stores
    /// `None` for `embedding_backend`.
    #[test]
    fn embedding_backend_none_when_no_key() {
        with_env_var("OPENAI_API_KEY", None, || {
            let result = phantom_embeddings::openai::OpenAiEmbeddingBackend::from_env();
            assert!(
                matches!(
                    result,
                    Err(phantom_embeddings::EmbedError::NotConfigured { .. })
                ),
                "expected NotConfigured, got {result:?}"
            );
        });
    }

    // -- Bundle store env guard test -----------------------------------------

    /// Setting `PHANTOM_DISABLE_BUNDLE_STORE=1` must short-circuit
    /// `open_bundle_store()` and return `None` WITHOUT touching the
    /// OS keychain. Verifies the dev-friction hot-fix is wired.
    #[test]
    fn bundle_store_disabled_via_env_returns_none() {
        // Snapshot prior value so concurrent tests in the same binary
        // do not lose state. We restore on exit.
        let prior = std::env::var("PHANTOM_DISABLE_BUNDLE_STORE").ok();
        // SAFETY: tests in this crate are not currently parallel readers
        // of this env var. The restore below puts the value back.
        unsafe {
            std::env::set_var("PHANTOM_DISABLE_BUNDLE_STORE", "1");
        }

        let result = open_bundle_store();

        // Restore env var BEFORE asserting so a panic still leaves a
        // clean environment for subsequent tests in the same process.
        unsafe {
            match prior {
                Some(v) => std::env::set_var("PHANTOM_DISABLE_BUNDLE_STORE", v),
                None => std::env::remove_var("PHANTOM_DISABLE_BUNDLE_STORE"),
            }
        }

        assert!(
            result.is_none(),
            "open_bundle_store must return None when PHANTOM_DISABLE_BUNDLE_STORE=1"
        );
    }

    // -----------------------------------------------------------------------
    // Federation relay channel tests
    // -----------------------------------------------------------------------

    /// When `PHANTOM_HUB_URL` and `PHANTOM_RELAY_URL` are both absent,
    /// `build_relay_channels` must return `None` for the inbound receiver so
    /// `BrainConfig::relay_inbound_rx` stays `None` in standalone mode.
    #[test]
    fn relay_inbound_rx_is_none_without_env_var() {
        use super::build_relay_channels;
        use std::sync::Mutex;

        // Coarse serial lock so parallel test threads do not race on env vars.
        static ENV_LOCK: Mutex<()> = Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());

        // Snapshot and clear both relay env vars.
        let prior_relay = std::env::var("PHANTOM_RELAY_URL").ok();
        let prior_hub = std::env::var("PHANTOM_HUB_URL").ok();
        unsafe {
            std::env::remove_var("PHANTOM_RELAY_URL");
            std::env::remove_var("PHANTOM_HUB_URL");
        }

        let (rx, connected_rx, handle) = build_relay_channels();

        // Restore env vars before asserting.
        unsafe {
            match prior_relay {
                Some(v) => std::env::set_var("PHANTOM_RELAY_URL", v),
                None => std::env::remove_var("PHANTOM_RELAY_URL"),
            }
            match prior_hub {
                Some(v) => std::env::set_var("PHANTOM_HUB_URL", v),
                None => std::env::remove_var("PHANTOM_HUB_URL"),
            }
        }

        assert!(
            rx.is_none(),
            "relay_inbound_rx must be None when no relay URL is configured"
        );
        assert!(
            connected_rx.is_none(),
            "relay_connected_rx must be None when no relay URL is configured"
        );
        assert!(
            handle.is_none(),
            "relay task handle must be None when no relay URL is configured"
        );
    }

    /// When `PHANTOM_HUB_URL` is set to a non-empty value,
    /// `build_relay_channels` must return `Some` for the inbound receiver so
    /// `BrainConfig::relay_inbound_rx` is wired.  The relay task will attempt
    /// to connect and log an error if the URL is unreachable — that is
    /// acceptable; this test only validates channel creation, not network
    /// connectivity.
    #[test]
    fn relay_inbound_rx_is_some_with_env_var() {
        use super::build_relay_channels;
        use std::sync::Mutex;

        static ENV_LOCK: Mutex<()> = Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());

        let prior_relay = std::env::var("PHANTOM_RELAY_URL").ok();
        let prior_hub = std::env::var("PHANTOM_HUB_URL").ok();
        unsafe {
            std::env::remove_var("PHANTOM_RELAY_URL");
            std::env::set_var("PHANTOM_HUB_URL", "wss://relay.phantom.test");
        }

        let (rx, connected_rx, handle) = build_relay_channels();

        // Restore env vars before asserting.
        unsafe {
            match prior_relay {
                Some(v) => std::env::set_var("PHANTOM_RELAY_URL", v),
                None => std::env::remove_var("PHANTOM_RELAY_URL"),
            }
            match prior_hub {
                Some(v) => std::env::set_var("PHANTOM_HUB_URL", v),
                None => std::env::remove_var("PHANTOM_HUB_URL"),
            }
        }

        // Drop handle before asserting so the relay thread is not left
        // dangling across tests (it will fail to connect and exit cleanly).
        drop(handle);
        drop(connected_rx);

        assert!(
            rx.is_some(),
            "relay_inbound_rx must be Some when PHANTOM_HUB_URL is set"
        );
    }
}
