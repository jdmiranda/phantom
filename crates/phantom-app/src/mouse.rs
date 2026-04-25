//! Mouse event handling for Phantom.
//!
//! Converts winit mouse events (clicks, scroll, motion) into SGR 1006
//! escape sequences and writes them to the PTY when the running terminal
//! program has requested mouse tracking.

use log::{trace, warn};
use winit::event::{ElementState, MouseButton, MouseScrollDelta};

use phantom_terminal::input::{
    encode_mouse_motion_sgr, encode_mouse_sgr, MouseButton as TermMouseButton, MouseMode,
};

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
    max_col: u16,
    max_row: u16,
) -> (u16, u16) {
    let rel_x = (px as f32 - inner_x).max(0.0);
    let rel_y = (py as f32 - inner_y).max(0.0);

    let col = (rel_x / cell_w).floor() as u16;
    let row = (rel_y / cell_h).floor() as u16;

    let col = col.min(max_col);
    let row = row.min(max_row);

    (col, row)
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
    /// Handle a mouse click (press or release) event from winit.
    pub fn handle_mouse_click(&mut self, state: ElementState, button: MouseButton) {
        let Some(pane) = self.panes.get_mut(self.focused_pane) else {
            return;
        };

        let mouse_mode = pane.terminal.mouse_mode();

        // When mouse tracking is NOT active, ignore release events entirely.
        if mouse_mode == MouseMode::None {
            if state == ElementState::Released {
                return;
            }
            // No mouse tracking — nothing more to do for clicks.
            return;
        }

        // Convert winit button to terminal button.
        let Some(term_button) = winit_to_term_button(button) else {
            return;
        };

        // Track held button for drag-mode motion events.
        match state {
            ElementState::Pressed => self.mouse_button_held = Some(term_button),
            ElementState::Released => self.mouse_button_held = None,
        }

        // Compute cell coordinates from cursor position relative to the pane.
        let layout_rect = match self.layout.get_pane_rect(pane.pane_id) {
            Ok(r) => r,
            Err(_) => return,
        };
        let outer = container_rect(layout_rect, self.cell_size);
        let inner = pane_inner_rect(self.cell_size, outer);
        let size = pane.terminal.size();
        let max_col = size.cols.saturating_sub(1);
        let max_row = size.rows.saturating_sub(1);

        let (col, row) = pixel_to_cell(
            self.cursor_position.0,
            self.cursor_position.1,
            inner.x,
            inner.y,
            self.cell_size.0,
            self.cell_size.1,
            max_col,
            max_row,
        );

        let pressed = state == ElementState::Pressed;
        let bytes = encode_mouse_sgr(term_button, col, row, pressed);
        trace!("mouse click: btn={term_button:?} col={col} row={row} pressed={pressed}");

        if let Err(e) = pane.terminal.pty_write(&bytes) {
            warn!("PTY write (mouse click) failed: {e}");
        }
    }

    /// Handle a mouse scroll event from winit.
    pub fn handle_mouse_scroll(&mut self, delta: MouseScrollDelta) {
        let Some(pane) = self.panes.get_mut(self.focused_pane) else {
            return;
        };

        // Determine scroll direction and magnitude.
        let lines = match delta {
            MouseScrollDelta::LineDelta(_x, y) => y,
            MouseScrollDelta::PixelDelta(pos) => {
                // Convert pixel delta to approximate line count.
                (pos.y as f32) / self.cell_size.1
            }
        };

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
            let max_col = size.cols.saturating_sub(1);
            let max_row = size.rows.saturating_sub(1);

            let (col, row) = pixel_to_cell(
                self.cursor_position.0,
                self.cursor_position.1,
                inner.x,
                inner.y,
                self.cell_size.0,
                self.cell_size.1,
                max_col,
                max_row,
            );

            // Each scroll notch generates one event. Send multiple for larger deltas.
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
            trace!("mouse scroll: btn={term_button:?} col={col} row={row} count={count}");
        }
        // When mouse tracking is NOT active, scrolling is a no-op for now
        // (scrollback support will be added later).
    }

    /// Handle a cursor-moved event from winit.
    pub fn handle_cursor_moved(&mut self, x: f64, y: f64) {
        self.cursor_position = (x, y);

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

        // Use the held button for drag, or Left as the default for any-motion mode.
        let term_button = self
            .mouse_button_held
            .unwrap_or(TermMouseButton::Left);

        let layout_rect = match self.layout.get_pane_rect(pane.pane_id) {
            Ok(r) => r,
            Err(_) => return,
        };
        let outer = container_rect(layout_rect, self.cell_size);
        let inner = pane_inner_rect(self.cell_size, outer);
        let size = pane.terminal.size();
        let max_col = size.cols.saturating_sub(1);
        let max_row = size.rows.saturating_sub(1);

        let (col, row) = pixel_to_cell(
            x,
            y,
            inner.x,
            inner.y,
            self.cell_size.0,
            self.cell_size.1,
            max_col,
            max_row,
        );

        let bytes = encode_mouse_motion_sgr(term_button, col, row);
        trace!("mouse motion: btn={term_button:?} col={col} row={row}");

        if let Err(e) = pane.terminal.pty_write(&bytes) {
            warn!("PTY write (mouse motion) failed: {e}");
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
        let (col, row) = pixel_to_cell(10.0, 20.0, 10.0, 20.0, 8.0, 16.0, 79, 23);
        assert_eq!((col, row), (0, 0));
    }

    #[test]
    fn pixel_to_cell_middle() {
        // 50px from inner_x=10 with cell_w=8 → col = floor(40/8) = 5
        // 84px from inner_y=20 with cell_h=16 → row = floor(64/16) = 4
        let (col, row) = pixel_to_cell(50.0, 84.0, 10.0, 20.0, 8.0, 16.0, 79, 23);
        assert_eq!((col, row), (5, 4));
    }

    #[test]
    fn pixel_to_cell_clamp_max() {
        // Very large coordinates should clamp to max_col, max_row.
        let (col, row) = pixel_to_cell(9999.0, 9999.0, 0.0, 0.0, 8.0, 16.0, 79, 23);
        assert_eq!((col, row), (79, 23));
    }

    #[test]
    fn pixel_to_cell_before_inner_rect() {
        // Cursor is before the inner rect origin — should clamp to (0, 0).
        let (col, row) = pixel_to_cell(5.0, 5.0, 10.0, 20.0, 8.0, 16.0, 79, 23);
        assert_eq!((col, row), (0, 0));
    }

    #[test]
    fn pixel_to_cell_exact_boundary() {
        // Exactly at the start of cell (1, 1).
        let (col, row) = pixel_to_cell(18.0, 36.0, 10.0, 20.0, 8.0, 16.0, 79, 23);
        assert_eq!((col, row), (1, 1));
    }

    #[test]
    fn pixel_to_cell_fractional() {
        // Halfway through a cell should still floor to the cell.
        let (col, row) = pixel_to_cell(14.5, 28.5, 10.0, 20.0, 8.0, 16.0, 79, 23);
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
