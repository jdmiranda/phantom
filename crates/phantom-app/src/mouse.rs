//! Mouse input handling for Phantom panes.
//!
//! Converts winit mouse events (clicks, scroll, motion) into either
//! internal scrollback operations or SGR 1006 escape sequences written
//! to the PTY when the running terminal program requests mouse tracking.

use log::{debug, trace, warn};
use winit::event::{ElementState, MouseButton, MouseScrollDelta};

use phantom_terminal::input::{
    encode_mouse_motion_sgr, encode_mouse_sgr, MouseButton as TermMouseButton,
};
use phantom_terminal::terminal::MouseMode;

use crate::app::App;
use crate::pane::{container_rect, pane_inner_rect};

// ---------------------------------------------------------------------------
// Helper: pixel coordinates to terminal cell
// ---------------------------------------------------------------------------

/// Convert pixel coordinates to terminal cell (col, row).
///
/// `px` / `py` are absolute window coordinates. The pane's inner rect and
/// `cell_size` determine which cell the pixel falls into. The result is
/// clamped to `0..max_col` and `0..max_row` (inclusive).
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

        // Hit-test: which pane is the cursor over?
        self.cursor_over_pane = None;
        for (i, pane) in self.panes.iter().enumerate() {
            if let Ok(layout_rect) = self.layout.get_pane_rect(pane.pane_id) {
                let cr = container_rect(layout_rect, self.cell_size);
                let inner = pane_inner_rect(self.cell_size, cr);
                if x as f32 >= inner.x
                    && x as f32 <= inner.x + inner.width
                    && y as f32 >= inner.y
                    && y as f32 <= inner.y + inner.height
                {
                    self.cursor_over_pane = Some(i);
                    break;
                }
            }
        }

        // Forward motion to PTY when the terminal program is tracking the mouse.
        let Some(pane) = self.panes.get_mut(self.focused_pane) else {
            return;
        };
        let mouse_mode = pane.terminal.mouse_mode();
        let should_send = match mouse_mode {
            MouseMode::Motion => true,
            MouseMode::Drag => self.mouse_button_held.is_some(),
            _ => false,
        };
        if !should_send {
            return;
        }

        let term_button = self.mouse_button_held.unwrap_or(TermMouseButton::Left);
        let layout_rect = match self.layout.get_pane_rect(pane.pane_id) {
            Ok(r) => r,
            Err(_) => return,
        };
        let outer = container_rect(layout_rect, self.cell_size);
        let inner = pane_inner_rect(self.cell_size, outer);
        let size = pane.terminal.size();
        let (col, row) = pixel_to_cell(
            x, y, inner.x, inner.y,
            self.cell_size.0, self.cell_size.1,
            size.cols.saturating_sub(1) as usize, size.rows.saturating_sub(1) as usize,
        );

        let bytes = encode_mouse_motion_sgr(term_button, col, row);
        trace!("mouse motion: btn={term_button:?} col={col} row={row}");
        if let Err(e) = pane.terminal.pty_write(&bytes) {
            warn!("PTY write (mouse motion) failed: {e}");
        }
    }

    /// Handle mouse button press/release.
    pub fn handle_mouse_click(&mut self, state: ElementState, button: MouseButton) {
        let Some(pane) = self.panes.get_mut(self.focused_pane) else {
            return;
        };
        let mouse_mode = pane.terminal.mouse_mode();

        if mouse_mode != MouseMode::None {
            // Terminal is tracking mouse — encode as SGR and write to PTY.
            let Some(term_button) = winit_to_term_button(button) else {
                return;
            };

            // Track held button for drag-mode motion events.
            match state {
                ElementState::Pressed => self.mouse_button_held = Some(term_button),
                ElementState::Released => self.mouse_button_held = None,
            }

            let layout_rect = match self.layout.get_pane_rect(pane.pane_id) {
                Ok(r) => r,
                Err(_) => return,
            };
            let outer = container_rect(layout_rect, self.cell_size);
            let inner = pane_inner_rect(self.cell_size, outer);
            let size = pane.terminal.size();
            let (col, row) = pixel_to_cell(
                self.cursor_position.0, self.cursor_position.1,
                inner.x, inner.y,
                self.cell_size.0, self.cell_size.1,
                size.cols.saturating_sub(1) as usize, size.rows.saturating_sub(1) as usize,
            );

            let pressed = state == ElementState::Pressed;
            let bytes = encode_mouse_sgr(term_button, col, row, pressed);
            trace!("mouse click SGR: btn={term_button:?} col={col} row={row} pressed={pressed}");
            if let Err(e) = pane.terminal.pty_write(&bytes) {
                warn!("PTY write (mouse click) failed: {e}");
            }
            return;
        }

        // No mouse tracking — only handle press events.
        if state != ElementState::Pressed {
            return;
        }

        // Left click on a pane focuses it.
        if button == MouseButton::Left {
            if let Some(pane_idx) = self.cursor_over_pane {
                if pane_idx != self.focused_pane {
                    debug!("Mouse focus: pane {pane_idx}");
                    self.focused_pane = pane_idx;
                }
            }
        }
    }

    /// Handle mouse scroll wheel.
    pub fn handle_mouse_scroll(&mut self, delta: MouseScrollDelta) {
        let Some(pane) = self.panes.get_mut(self.focused_pane) else {
            return;
        };

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

        let mouse_mode = pane.terminal.mouse_mode();

        if mouse_mode != MouseMode::None {
            // Encode scroll as SGR mouse events.
            let layout_rect = match self.layout.get_pane_rect(pane.pane_id) {
                Ok(r) => r,
                Err(_) => return,
            };
            let outer = container_rect(layout_rect, self.cell_size);
            let inner = pane_inner_rect(self.cell_size, outer);
            let size = pane.terminal.size();
            let (col, row) = pixel_to_cell(
                self.cursor_position.0, self.cursor_position.1,
                inner.x, inner.y,
                self.cell_size.0, self.cell_size.1,
                size.cols.saturating_sub(1) as usize, size.rows.saturating_sub(1) as usize,
            );

            let count = lines.abs().ceil().max(1.0) as u32;
            let term_button = if lines > 0.0 {
                TermMouseButton::ScrollUp
            } else {
                TermMouseButton::ScrollDown
            };

            for _ in 0..count {
                let bytes = encode_mouse_sgr(term_button, col, row, true);
                if let Err(e) = pane.terminal.pty_write(&bytes) {
                    warn!("PTY write (mouse scroll) failed: {e}");
                    break;
                }
            }
            trace!("mouse scroll SGR: btn={term_button:?} col={col} row={row} count={count}");
            return;
        }

        // No mouse tracking — use scrollback.
        let int_lines = lines.round() as i32;
        if int_lines > 0 {
            pane.terminal.scroll_up(int_lines.unsigned_abs() as usize);
        } else if int_lines < 0 {
            pane.terminal.scroll_down(int_lines.unsigned_abs() as usize);
        }
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
