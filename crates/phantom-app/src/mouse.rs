//! Mouse input handling for Phantom panes.

use log::debug;
use winit::event::{ElementState, MouseButton, MouseScrollDelta};

use crate::app::App;
use crate::pane::{container_rect, pane_inner_rect};

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
    }

    /// Handle mouse button press/release.
    pub fn handle_mouse_click(&mut self, state: ElementState, button: MouseButton) {
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
        let lines = match delta {
            MouseScrollDelta::LineDelta(_, y) => y as i32,
            MouseScrollDelta::PixelDelta(pos) => {
                let line_height = self.cell_size.1 as f64;
                if line_height > 0.0 {
                    (pos.y / line_height).round() as i32
                } else {
                    0
                }
            }
        };

        if lines == 0 {
            return;
        }

        if let Some(pane) = self.panes.get_mut(self.focused_pane) {
            if lines > 0 {
                pane.terminal.scroll_up(lines.unsigned_abs() as usize);
            } else {
                pane.terminal.scroll_down(lines.unsigned_abs() as usize);
            }
        }
    }
}
