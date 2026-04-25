//! Mouse input handling for Phantom panes.
//!
//! Converts winit mouse events (clicks, scroll, motion) into either
//! internal scrollback operations or SGR 1006 escape sequences written
//! to the PTY when the running terminal program requests mouse tracking.
//! Also handles scrollbar click-to-jump.

use log::debug;
use winit::event::{ElementState, MouseButton, MouseScrollDelta};

#[allow(unused_imports)] // Used when SGR mouse forwarding is wired through adapters
use phantom_terminal::input::{
    encode_mouse_motion_sgr, encode_mouse_sgr, MouseButton as TermMouseButton,
};

use crate::app::App;
use crate::pane::{
    container_rect, pane_inner_rect, point_in_rect, scrollbar_track_rect, scrollbar_y_to_offset,
};

// ---------------------------------------------------------------------------
// Helper: pixel coordinates to terminal cell
// ---------------------------------------------------------------------------

/// Convert pixel coordinates to terminal cell (col, row).
#[allow(dead_code)] // Used when SGR mouse tracking is wired through adapters
pub(crate) fn pixel_to_cell(
    px: f64,
    py: f64,
    inner_x: f32,
    inner_y: f32,
    cell_w: f32,
    cell_h: f32,
    max_col: usize,
    max_row: usize,
) -> (usize, usize) {
    let rel_x = (px as f32 - inner_x).max(0.0);
    let rel_y = (py as f32 - inner_y).max(0.0);

    let col = (rel_x / cell_w).floor() as usize;
    let row = (rel_y / cell_h).floor() as usize;

    (col.min(max_col), row.min(max_row))
}

// ---------------------------------------------------------------------------
// Winit button conversion
// ---------------------------------------------------------------------------

#[allow(dead_code)] // Used when SGR mouse tracking is wired through adapters
fn winit_to_term_button(button: MouseButton) -> Option<TermMouseButton> {
    match button {
        MouseButton::Left => Some(TermMouseButton::Left),
        MouseButton::Right => Some(TermMouseButton::Right),
        MouseButton::Middle => Some(TermMouseButton::Middle),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Mouse event methods on App
// ---------------------------------------------------------------------------

impl App {
    /// Handle cursor movement -- update position and hit-test panes.
    pub fn handle_cursor_moved(&mut self, x: f64, y: f64) {
        self.cursor_position = (x, y);

        // Hit-test coordinator-managed adapters.
        self.cursor_over_pane = None;
        for app_id in self.coordinator.all_app_ids() {
            if let Some(pane_id) = self.coordinator.pane_id_for(app_id) {
                if let Ok(layout_rect) = self.layout.get_pane_rect(pane_id) {
                    let cr = container_rect(layout_rect, self.cell_size);
                    let inner = pane_inner_rect(self.cell_size, cr);
                    if x as f32 >= inner.x
                        && x as f32 <= inner.x + inner.width
                        && y as f32 >= inner.y
                        && y as f32 <= inner.y + inner.height
                    {
                        // Store the AppId in cursor_over_pane for focus-click.
                        self.cursor_over_pane = Some(app_id);
                        break;
                    }
                }
            }
        }

        // If left button is held, update selection (drag-to-select).
        if self.mouse_button_held == Some(phantom_terminal::input::MouseButton::Left) {
            if let Some(focused) = self.coordinator.focused() {
                if let Some((col, row)) = self.cursor_to_cell(focused) {
                    let _ = self.coordinator.send_command(
                        focused,
                        "select_update",
                        &serde_json::json!({"col": col, "row": row}),
                    );
                }
            }
        }
    }

    /// Convert cursor pixel position to terminal cell (col, row) for the given adapter.
    fn cursor_to_cell(&self, app_id: u32) -> Option<(usize, usize)> {
        let pane_id = self.coordinator.pane_id_for(app_id)?;
        let layout_rect = self.layout.get_pane_rect(pane_id).ok()?;
        let cr = container_rect(layout_rect, self.cell_size);
        let inner = pane_inner_rect(self.cell_size, cr);
        let (px, py) = self.cursor_position;
        let col = ((px as f32 - inner.x).max(0.0) / self.cell_size.0).floor() as usize;
        let row = ((py as f32 - inner.y).max(0.0) / self.cell_size.1).floor() as usize;
        Some((col, row))
    }

    /// Handle mouse button press/release.
    pub fn handle_mouse_click(&mut self, state: ElementState, button: MouseButton) {
        // Track button state for drag selection.
        if button == MouseButton::Left {
            match state {
                ElementState::Pressed => {
                    self.mouse_button_held = Some(phantom_terminal::input::MouseButton::Left);
                }
                ElementState::Released => {
                    self.mouse_button_held = None;
                }
            }
        }

        // Only handle press events for non-SGR operations.
        if state != ElementState::Pressed {
            return;
        }

        // Left click — check scrollbar hit on coordinator panes, then focus.
        if button == MouseButton::Left {
            // Clear any existing selection first.
            if let Some(focused) = self.coordinator.focused() {
                let _ = self.coordinator.send_command(
                    focused,
                    "select_clear",
                    &serde_json::json!({}),
                );
            }
            let (mx, my) = (self.cursor_position.0 as f32, self.cursor_position.1 as f32);

            // Check scrollbar hit on each coordinator-managed pane.
            for app_id in self.coordinator.all_app_ids() {
                let Some(pane_id) = self.coordinator.pane_id_for(app_id) else { continue };
                let Ok(layout_rect) = self.layout.get_pane_rect(pane_id) else { continue };
                let outer = container_rect(layout_rect, self.cell_size);
                let inner = pane_inner_rect(self.cell_size, outer);
                let track = scrollbar_track_rect(inner);

                if point_in_rect(mx, my, track) {
                    // Scrollbar click-to-jump via scroll command.
                    let target_offset = scrollbar_y_to_offset(track, my, 1000);
                    let _ = self.coordinator.send_command(
                        app_id,
                        "scroll_to_offset",
                        &serde_json::json!({"offset": target_offset}),
                    );
                    debug!("Scrollbar click: adapter {app_id}, target_offset={target_offset}");
                    return;
                }
            }

            // Click-to-focus: set coordinator focus to the pane under cursor.
            if let Some(app_id) = self.cursor_over_pane {
                if self.coordinator.focused() != Some(app_id) {
                    debug!("Mouse focus: adapter {app_id}");
                    self.coordinator.set_focus(app_id);
                }
            }

            // Start text selection at the clicked cell.
            if let Some(focused) = self.coordinator.focused() {
                if let Some((col, row)) = self.cursor_to_cell(focused) {
                    let _ = self.coordinator.send_command(
                        focused,
                        "select_start",
                        &serde_json::json!({"col": col, "row": row}),
                    );
                }
            }
        }
    }

    /// Handle mouse scroll wheel.
    pub fn handle_mouse_scroll(&mut self, delta: MouseScrollDelta) {
        let lines = match delta {
            MouseScrollDelta::LineDelta(_, y) => y,
            MouseScrollDelta::PixelDelta(pos) => {
                let line_height = self.cell_size.1;
                if line_height > 0.0 {
                    (pos.y as f32) / line_height
                } else {
                    0.0
                }
            }
        };

        if lines.abs() < 0.01 {
            return;
        }

        // Route scroll to focused adapter via command.
        let int_lines = lines.round().abs().max(1.0) as u64;
        let direction = if lines > 0.0 { "up" } else { "down" };
        let _ = self.coordinator.send_command_to_focused(
            "scroll",
            &serde_json::json!({"direction": direction, "lines": int_lines}),
        );
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pixel_to_cell_origin() {
        let (col, row) = pixel_to_cell(10.0, 20.0, 10.0, 20.0, 8.0, 16.0, 79usize, 23usize);
        assert_eq!((col, row), (0, 0));
    }

    #[test]
    fn pixel_to_cell_middle() {
        let (col, row) = pixel_to_cell(50.0, 84.0, 10.0, 20.0, 8.0, 16.0, 79usize, 23usize);
        assert_eq!((col, row), (5, 4));
    }

    #[test]
    fn pixel_to_cell_clamp_max() {
        let (col, row) = pixel_to_cell(9999.0, 9999.0, 0.0, 0.0, 8.0, 16.0, 79usize, 23usize);
        assert_eq!((col, row), (79, 23));
    }

    #[test]
    fn pixel_to_cell_before_inner_rect() {
        let (col, row) = pixel_to_cell(5.0, 5.0, 10.0, 20.0, 8.0, 16.0, 79usize, 23usize);
        assert_eq!((col, row), (0, 0));
    }

    #[test]
    fn pixel_to_cell_exact_boundary() {
        let (col, row) = pixel_to_cell(18.0, 36.0, 10.0, 20.0, 8.0, 16.0, 79usize, 23usize);
        assert_eq!((col, row), (1, 1));
    }

    #[test]
    fn pixel_to_cell_fractional() {
        let (col, row) = pixel_to_cell(14.5, 28.5, 10.0, 20.0, 8.0, 16.0, 79usize, 23usize);
        assert_eq!((col, row), (0, 0));
    }

    #[test]
    fn winit_button_conversion() {
        assert_eq!(winit_to_term_button(MouseButton::Left), Some(TermMouseButton::Left));
        assert_eq!(winit_to_term_button(MouseButton::Right), Some(TermMouseButton::Right));
        assert_eq!(winit_to_term_button(MouseButton::Middle), Some(TermMouseButton::Middle));
        assert_eq!(winit_to_term_button(MouseButton::Back), None);
        assert_eq!(winit_to_term_button(MouseButton::Forward), None);
    }
}
