//! The universal app adapter trait, split into focused sub-traits.
//!
//! Every component in Phantom — terminals, editors, database browsers,
//! headless transcription services — implements the relevant sub-traits.
//! Implementing all required sub-traits grants `AppAdapter` via blanket impl.

use crate::bus::{BusMessage, TopicDeclaration};
use crate::lifecycle::AppState;
use crate::protocol::AdapterId;
use crate::spatial::{NegotiationResult, SpatialPreference};

/// Opaque app identifier assigned by the registry.
pub type AppId = u32;

// ---------------------------------------------------------------------------
// Sub-traits (Interface Segregation Principle)
// ---------------------------------------------------------------------------

/// Required by all adapters. The coordinator stores `Box<dyn AppCore>`.
pub trait AppCore: Send {
    /// Unique type name for this kind of app (e.g. "terminal", "browser").
    fn app_type(&self) -> &str;

    /// Whether this app is still alive.
    fn is_alive(&self) -> bool;

    /// Per-frame update tick.
    fn update(&mut self, dt: f32);

    /// Current state as structured JSON (the AI brain reads this).
    fn get_state(&self) -> serde_json::Value;

    /// Human-readable title for the pane chrome (e.g. tab label, window title).
    ///
    /// The default implementation returns the `app_type()` string so existing
    /// adapters do not need to implement this immediately.
    fn title(&self) -> &str {
        self.app_type()
    }

    /// The stable [`AdapterId`] for this adapter instance.
    ///
    /// The coordinator calls [`Lifecycled::set_adapter_id`] immediately after
    /// registration; the default returns [`AdapterId::new(0)`] (sentinel) until
    /// the adapter stores the assigned id.
    fn adapter_id(&self) -> AdapterId {
        AdapterId::new(0)
    }

    /// Fine-grained time update called by the frame loop.
    ///
    /// `dt_ms` is the elapsed time in milliseconds since the previous tick.
    /// Unlike `update(dt_f32)`, this tick uses integer milliseconds so
    /// animation state machines can use cheap integer arithmetic.
    ///
    /// The default is a no-op so existing adapters compile without changes.
    fn tick(&mut self, _dt_ms: u64) {}

    /// Snapshot of the adapter's raw PTY/stdout output buffer (last ~8 KiB).
    ///
    /// Terminal adapters return `Some(text)` so the OODA brain can populate
    /// `ParsedOutput::raw_output` when a `CommandComplete` event fires.
    /// Non-terminal adapters return `None` (the default) and are silently
    /// skipped.
    ///
    /// The returned string may contain ANSI escape sequences.
    fn output_buf_snapshot(&self) -> Option<String> {
        None
    }

    /// Wire a shared render-snapshot Arc into this adapter.
    ///
    /// Called by `App::split_for_alt_screen` when the layout is split so that
    /// the alt-screen view sibling can receive rendered grid data each frame.
    /// Terminal adapters store the Arc and write into it during `update()`.
    ///
    /// The default no-op means all non-terminal adapters compile unchanged.
    fn attach_alt_screen_snapshot(
        &mut self,
        _snapshot: std::sync::Arc<std::sync::Mutex<Option<RenderOutput>>>,
    ) {
    }

    /// Remove the shared render-snapshot Arc previously set by
    /// `attach_alt_screen_snapshot`.  Called when the secondary pane is
    /// collapsed.  The default is a no-op.
    fn detach_alt_screen_snapshot(&mut self) {}
}

/// Visual adapters that render into a rect.
pub trait Renderable {
    /// Render into simplified quad + text buffers.
    fn render(&self, rect: &Rect) -> RenderOutput;

    /// Whether this app renders visually. Headless apps return `false`.
    fn is_visual(&self) -> bool {
        true
    }

    /// Spatial layout preferences (size constraints, priority).
    fn spatial_preference(&self) -> Option<SpatialPreference> {
        None
    }

    /// Respond to a proposed resize from the layout arbiter.
    ///
    /// The arbiter calls this during two-phase negotiation. The default
    /// accepts any proposal.
    fn on_resize_propose(&mut self, width: f32, height: f32) -> NegotiationResult {
        let _ = (width, height);
        NegotiationResult::Accepted
    }
}

/// Adapters that accept keyboard input.
pub trait InputHandler {
    /// Handle keyboard input. Returns `true` if consumed.
    fn handle_input(&mut self, key: &str) -> bool;

    /// Whether this adapter accepts keyboard input dispatch.
    /// Non-interactive adapters (headless processors, monitors) return `false`.
    fn accepts_input(&self) -> bool {
        true
    }
}

/// Adapters that accept commands from AI or other apps.
pub trait Commandable {
    /// Accept a command from the AI or another app.
    fn accept_command(&mut self, cmd: &str, args: &serde_json::Value) -> anyhow::Result<String>;

    /// Whether this adapter accepts command dispatch.
    /// Override to `false` for adapters that are purely observational.
    fn accepts_commands(&self) -> bool {
        true
    }
}

/// Adapters that participate in the event bus.
pub trait BusParticipant {
    /// Topics this app publishes.
    fn publishes(&self) -> Vec<TopicDeclaration> {
        vec![]
    }

    /// Topic names this app wants to subscribe to.
    fn subscribes_to(&self) -> Vec<String> {
        vec![]
    }

    /// Receive a message from the event bus.
    fn on_message(&mut self, _msg: &BusMessage) {}

    /// Drain pending outbound messages queued during `update()`.
    ///
    /// Called by the coordinator after the update pass and before message
    /// delivery, so adapters can emit bus events without needing direct
    /// bus access (which would cause borrow conflicts).
    fn drain_outbox(&mut self) -> Vec<BusMessage> {
        vec![]
    }
}

/// Adapters with lifecycle hooks.
pub trait Lifecycled {
    /// Called once when the app starts (state = Initializing).
    fn on_init(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    /// Called whenever the app's lifecycle state changes.
    fn on_state_change(&mut self, _new_state: AppState) {}

    /// Called by the coordinator immediately after registration to inform
    /// the adapter of its assigned AppId. Override to store the ID for
    /// use in outbox messages.
    fn set_app_id(&mut self, _id: AppId) {}

    /// Called by the coordinator immediately after registration to inform
    /// the adapter of its stable [`AdapterId`].
    ///
    /// Adapters that want to expose `adapter_id()` from [`AppCore`] should
    /// store this value and return it from that method.
    fn set_adapter_id(&mut self, _id: AdapterId) {}
}

/// Permission declarations (WASM sandbox boundary).
pub trait Permissioned {
    /// Permissions this app requires (filesystem, network, etc.).
    fn permissions(&self) -> Vec<String> {
        vec![]
    }
}

// ---------------------------------------------------------------------------
// Convenience super-trait with blanket impl
// ---------------------------------------------------------------------------

/// Convenience: implement all sub-traits and get `AppAdapter` for free.
///
/// The `AppRegistry` stores `Box<dyn AppAdapter>`, so this remains the
/// primary trait object used throughout the system.
pub trait AppAdapter:
    AppCore + Renderable + InputHandler + Commandable + BusParticipant + Lifecycled + Permissioned
{
}

impl<T> AppAdapter for T where
    T: AppCore
        + Renderable
        + InputHandler
        + Commandable
        + BusParticipant
        + Lifecycled
        + Permissioned
{
}

// ---------------------------------------------------------------------------
// Simplified render primitives
// ---------------------------------------------------------------------------

/// Simplified render output. Actual GPU types are in phantom-renderer.
#[derive(Debug, Clone, Default)]
pub struct RenderOutput {
    pub quads: Vec<QuadData>,
    pub text_segments: Vec<TextData>,
    pub grid: Option<GridData>,
    pub scroll: Option<ScrollState>,
    pub selection: Option<SelectionRange>,
}

/// Active text selection range in grid coordinates.
#[derive(Debug, Clone, Copy)]
pub struct SelectionRange {
    pub start_col: usize,
    pub start_row: usize,
    pub end_col: usize,
    pub end_row: usize,
}

/// Scroll position data for scrollbar rendering.
#[derive(Debug, Clone, Copy)]
pub struct ScrollState {
    pub display_offset: usize,
    pub history_size: usize,
    pub visible_rows: usize,
}

/// Terminal cell grid data for GPU rendering.
#[derive(Debug, Clone)]
pub struct GridData {
    pub cells: Vec<TerminalCell>,
    pub cols: usize,
    pub rows: usize,
    pub origin: (f32, f32),
    pub cursor: Option<CursorData>,
}

/// Terminal cell for text rendering.
#[derive(Debug, Clone, Copy)]
pub struct TerminalCell {
    pub ch: char,
    pub fg: [f32; 4],
    pub bg: [f32; 4],
}

/// Cursor position and appearance.
#[derive(Debug, Clone, Copy)]
pub struct CursorData {
    pub col: usize,
    pub row: usize,
    pub shape: CursorShape,
    pub visible: bool,
}

/// Cursor visual style.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorShape {
    Block,
    Underline,
    Bar,
}

/// A colored rectangle.
#[derive(Debug, Clone)]
pub struct QuadData {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub color: [f32; 4],
}

/// A positioned text segment.
#[derive(Debug, Clone)]
pub struct TextData {
    pub text: String,
    pub x: f32,
    pub y: f32,
    pub color: [f32; 4],
}

/// Axis-aligned rectangle used for layout allocation.
///
/// `cell_size` carries the (cell_w, cell_h) the renderer derived for this
/// frame. Adapters that need to translate between pixel-space rects and
/// cell-space rects (e.g. inspector lists, terminal grids) read it instead
/// of guessing. `(0.0, 0.0)` is the default and signals "unknown / not
/// computed" — adapters should fall back to a sensible constant.
#[derive(Debug, Clone, Default)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub cell_size: (f32, f32),
}
