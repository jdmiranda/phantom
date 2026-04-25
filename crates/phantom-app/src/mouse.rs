//! Mouse input handling for Phantom.
//!
//! Provides `handle_mouse_click` and `handle_cursor_moved` on [`App`].

use log::debug;
use winit::event::MouseButton;

use crate::app::App;
use crate::pane::{
    container_rect, pane_inner_rect, point_in_rect, scrollbar_track_rect, scrollbar_y_to_offset,
};

impl App {
    /// Update the cached cursor position when the pointer moves.
    pub fn handle_cursor_moved(&mut self, x: f64, y: f64) {
        self.cursor_position = (x, y);
    }

    /// Handle a mouse button press.
    ///
    /// Left-click on a scrollbar track jumps the scroll position. Otherwise
    /// the click focuses the pane under the cursor.
    pub fn handle_mouse_click(&mut self, button: MouseButton) {
        if button != MouseButton::Left {
            return;
        }

        let (mx, my) = (self.cursor_position.0 as f32, self.cursor_position.1 as f32);

        // Check each pane's scrollbar track for a hit.
        for (pane_idx, pane) in self.panes.iter().enumerate() {
            // Skip panes where the terminal is tracking the mouse -- the
            // click should go to the program, not the scrollbar.
            if pane.terminal.mouse_tracking_active() {
                continue;
            }

            let layout_rect = match self.layout.get_pane_rect(pane.pane_id) {
                Ok(r) => r,
                Err(_) => continue,
            };

            let outer = container_rect(layout_rect, self.cell_size);
            let inner = pane_inner_rect(self.cell_size, outer);
            let track = scrollbar_track_rect(inner);

            if point_in_rect(mx, my, track) {
                let history_size = pane.terminal.history_size();
                if history_size == 0 {
                    // Nothing to scroll.
                    return;
                }

                let target_offset = scrollbar_y_to_offset(track, my, history_size);
                let current_offset = pane.terminal.display_offset();

                let delta = target_offset as i32 - current_offset as i32;
                if delta != 0 {
                    debug!(
                        "Scrollbar click: pane {pane_idx}, target_offset={target_offset}, \
                         current={current_offset}, delta={delta}"
                    );
                    // We need mutable access to the specific pane.
                    self.panes[pane_idx].terminal.scroll_delta(delta);
                }
                return;
            }
        }

        // No scrollbar hit -- focus the pane under the cursor.
        for (pane_idx, pane) in self.panes.iter().enumerate() {
            let layout_rect = match self.layout.get_pane_rect(pane.pane_id) {
                Ok(r) => r,
                Err(_) => continue,
            };

            let outer = container_rect(layout_rect, self.cell_size);
            if point_in_rect(mx, my, outer) {
                if self.focused_pane != pane_idx {
                    self.focused_pane = pane_idx;
                    debug!("Focus pane {pane_idx} via mouse click");
                }
                return;
            }
        }
    }
}
