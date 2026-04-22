//! The universal app adapter trait.
//!
//! Every component in Phantom — terminals, editors, database browsers,
//! headless transcription services — implements `AppAdapter`.

use crate::bus::{BusMessage, TopicDeclaration};
use crate::lifecycle::AppState;
use crate::spatial::SpatialPreference;

/// Opaque app identifier assigned by the registry.
pub type AppId = u32;

/// The universal app interface. Everything in Phantom implements this.
pub trait AppAdapter: Send {
    /// Unique type name for this kind of app (e.g. "terminal", "browser").
    fn app_type(&self) -> &str;

    /// Permissions this app requires (filesystem, network, etc.).
    fn permissions(&self) -> Vec<String>;

    /// Whether this app renders visually. Headless apps return `false`.
    fn is_visual(&self) -> bool {
        true
    }

    /// Spatial layout preferences (size constraints, priority).
    fn spatial_preference(&self) -> Option<SpatialPreference> {
        None
    }

    /// Called once when the app starts (state = Initializing).
    fn on_init(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    /// Called whenever the app's lifecycle state changes.
    fn on_state_change(&mut self, _new_state: AppState) {}

    /// Render into simplified quad + text buffers.
    /// The actual GPU types live in phantom-renderer; these are
    /// intermediate representations.
    fn render(&self, rect: &Rect) -> RenderOutput;

    /// Handle keyboard input. Returns `true` if consumed.
    fn handle_input(&mut self, key: &str) -> bool;

    /// Current state as structured JSON (the AI brain reads this).
    fn get_state(&self) -> serde_json::Value;

    /// Accept a command from the AI or another app.
    fn accept_command(&mut self, cmd: &str, args: &serde_json::Value) -> anyhow::Result<String>;

    /// Per-frame update tick.
    fn update(&mut self, dt: f32);

    /// Processing tick for headless apps.
    fn process(&mut self) {}

    /// Whether this app is still alive.
    fn is_alive(&self) -> bool;

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
}

// ---------------------------------------------------------------------------
// Simplified render primitives
// ---------------------------------------------------------------------------

/// Simplified render output. Actual GPU types are in phantom-renderer.
#[derive(Debug, Clone, Default)]
pub struct RenderOutput {
    pub quads: Vec<QuadData>,
    pub text_segments: Vec<TextData>,
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
#[derive(Debug, Clone)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}
