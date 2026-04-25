//! Mouse input handling for Phantom panes.

use winit::event::{ElementState, MouseButton, MouseScrollDelta};

use crate::app::App;

impl App {
    /// Handle cursor movement -- update position and hit-test panes.
    pub fn handle_cursor_moved(&mut self, x: f64, y: f64) {
        self.cursor_position = (x, y);
    }

    /// Handle mouse button press/release.
    pub fn handle_mouse_click(&mut self, _state: ElementState, _button: MouseButton) {
        // Will be implemented in T6
    }

    /// Handle mouse scroll wheel.
    pub fn handle_mouse_scroll(&mut self, _delta: MouseScrollDelta) {
        // Will be implemented in T6
    }
}
