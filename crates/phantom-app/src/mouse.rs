//! Mouse input handling for Phantom panes.
//!
//! Converts winit mouse events (clicks, scroll, motion) into either
//! internal scrollback operations or SGR 1006 escape sequences written
//! to the PTY when the running terminal program requests mouse tracking.
//! Also handles scrollbar click-to-jump.

use std::time::{Duration, Instant};

use log::debug;
use winit::event::{ElementState, MouseButton, MouseScrollDelta};

use phantom_terminal::input::{
    encode_mouse_sgr, MouseButton as TermMouseButton,
};

use crate::app::App;
use crate::context_menu::{MenuAction, MenuItem};
use crate::pane::{
    container_rect, pane_inner_rect, point_in_rect, scrollbar_track_rect, scrollbar_y_to_offset,
};

// ---------------------------------------------------------------------------
// Helper: pixel coordinates to terminal cell
// ---------------------------------------------------------------------------

/// Convert pixel coordinates to terminal cell (col, row).
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
    /// Returns true if the cursor is inside the visible console overlay region.
    fn cursor_in_console(&self) -> bool {
        if !self.console.visible() {
            return false;
        }
        let screen_h = self.gpu.surface_config.height as f32;
        let full_height = (screen_h * 0.40).max(120.0);
        let console_height = full_height * self.console.slide;
        let (_, cy) = self.cursor_position;
        cy < console_height as f64
    }

    /// Handle cursor movement -- update position and hit-test panes.
    pub fn handle_cursor_moved(&mut self, x: f64, y: f64) {
        self.cursor_position = (x, y);

        // Update context menu hover tracking.
        if self.context_menu.visible {
            self.context_menu.update_hover(x as f32, y as f32);
        }

        // Console overlay captures all mouse interaction when visible.
        if self.cursor_in_console() {
            self.cursor_over_pane = None;
            return;
        }

        // Hit-test floating panes first (highest z-order).
        self.cursor_over_pane = None;
        let fx = x as f32;
        let fy = y as f32;
        for fid in self.coordinator.floating_ids() {
            if let Some(rect) = self.coordinator.float_rect(fid) {
                if fx >= rect.x && fx <= rect.x + rect.width
                    && fy >= rect.y && fy <= rect.y + rect.height
                {
                    self.cursor_over_pane = Some(fid);
                    return;
                }
            }
        }

        // Hit-test tiled coordinator-managed adapters.
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
        // Compute grid dimensions from the pane inner rect for clamping.
        let max_col = if self.cell_size.0 > 0.0 {
            (inner.width / self.cell_size.0).floor() as usize
        } else {
            0
        };
        let max_row = if self.cell_size.1 > 0.0 {
            (inner.height / self.cell_size.1).floor() as usize
        } else {
            0
        };
        Some(pixel_to_cell(
            px, py, inner.x, inner.y,
            self.cell_size.0, self.cell_size.1,
            max_col.saturating_sub(1), max_row.saturating_sub(1),
        ))
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

        // SGR mouse forwarding: if the focused adapter has mouse tracking
        // enabled, encode the click/release as SGR 1006 and send to the PTY.
        if let Some(term_btn) = winit_to_term_button(button) {
            if let Some(focused) = self.coordinator.focused() {
                if self.coordinator.adapter_wants_mouse(focused) {
                    let pressed = state == ElementState::Pressed;
                    if let Some((col, row)) = self.cursor_to_cell(focused) {
                        let sgr = encode_mouse_sgr(term_btn, col, row, pressed);
                        let _ = self.coordinator.route_bytes_to(focused, &sgr);
                        return;
                    }
                }
            }
        }

        // Only handle press events for non-SGR operations.
        if state != ElementState::Pressed {
            return;
        }

        // Console overlay captures mouse clicks when visible.
        if self.cursor_in_console() {
            return;
        }

        let (mx, my) = (self.cursor_position.0 as f32, self.cursor_position.1 as f32);

        // Right-click opens context menu.
        if button == MouseButton::Right {
            let items = self.build_context_menu_items();
            self.context_menu.show(mx, my, items);
            return;
        }

        // Left-click on context menu dispatches action or dismisses.
        if button == MouseButton::Left && self.context_menu.visible {
            if let Some(idx) = self.context_menu.hit_test(mx, my) {
                let action = self.context_menu.items[idx].action.clone();
                self.context_menu.hide();
                self.execute_menu_action(action);
                return;
            }
            self.context_menu.hide();
            return;
        }

        // Click-to-focus for floating panes (check first, highest z).
        if button == MouseButton::Left {
            let (fmx, fmy) = (self.cursor_position.0 as f32, self.cursor_position.1 as f32);
            let float_ids: Vec<_> = self.coordinator.floating_ids().collect();
            let mut float_focus = None;
            for fid in &float_ids {
                if let Some(rect) = self.coordinator.float_rect(*fid) {
                    if fmx >= rect.x && fmx <= rect.x + rect.width
                        && fmy >= rect.y && fmy <= rect.y + rect.height
                    {
                        float_focus = Some(*fid);
                        break;
                    }
                }
            }
            if let Some(fid) = float_focus {
                if self.coordinator.focused() != Some(fid) {
                    self.coordinator.set_focus(fid);
                }
                return;
            }
        }

        // Left click — check scrollbar hit on coordinator panes, then focus.
        if button == MouseButton::Left {
            // -- Multi-click detection (double = word, triple = line) --
            let now = Instant::now();
            let (px, py) = self.cursor_position;
            let max_dist = 5.0; // pixels — clicks must be near each other
            let near = {
                let dx = px - self.last_click_pos.0;
                let dy = py - self.last_click_pos.1;
                (dx * dx + dy * dy).sqrt() < max_dist
            };

            let rapid = self.last_click_time
                .map_or(false, |t| now.duration_since(t) < Duration::from_millis(400));

            if rapid && near {
                self.click_count = (self.click_count + 1).min(3);
            } else {
                self.click_count = 1;
            }
            self.last_click_time = Some(now);
            self.last_click_pos = (px, py);

            // Clear any existing selection first (single click resets).
            if self.click_count == 1 {
                if let Some(focused) = self.coordinator.focused() {
                    let _ = self.coordinator.send_command(
                        focused,
                        "select_clear",
                        &serde_json::json!({}),
                    );
                }
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
                    // Query actual history size from adapter state for accurate jump.
                    let history_size = self.coordinator.get_state(app_id)
                        .and_then(|s| s.get("history_size").and_then(|v| v.as_u64()))
                        .unwrap_or(0) as usize;
                    if history_size > 0 {
                        let target_offset = scrollbar_y_to_offset(track, my, history_size);
                        let _ = self.coordinator.send_command(
                            app_id,
                            "scroll_to_offset",
                            &serde_json::json!({"offset": target_offset}),
                        );
                        debug!("Scrollbar click: adapter {app_id}, target_offset={target_offset}, history={history_size}");
                    }
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

            // Dispatch selection based on click count.
            if let Some(focused) = self.coordinator.focused() {
                if let Some((col, row)) = self.cursor_to_cell(focused) {
                    match self.click_count {
                        2 => {
                            // Double-click: select word.
                            let _ = self.coordinator.send_command(
                                focused,
                                "select_word",
                                &serde_json::json!({"col": col, "row": row}),
                            );
                            debug!("Double-click word select at ({col}, {row})");
                        }
                        3 => {
                            // Triple-click: select line.
                            let _ = self.coordinator.send_command(
                                focused,
                                "select_line",
                                &serde_json::json!({"row": row}),
                            );
                            debug!("Triple-click line select at row {row}");
                        }
                        _ => {
                            // Single click: start simple selection.
                            let _ = self.coordinator.send_command(
                                focused,
                                "select_start",
                                &serde_json::json!({"col": col, "row": row}),
                            );
                        }
                    }
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

        // Console overlay captures scroll when visible — scroll console history.
        if self.cursor_in_console() {
            let int_lines = lines.round().abs().max(1.0) as usize;
            if lines > 0.0 {
                self.console.scroll_up(int_lines);
            } else {
                self.console.scroll_down(int_lines);
            }
            return;
        }

        // SGR mouse forwarding: if the focused adapter has mouse tracking
        // enabled, encode scroll as SGR 1006 wheel events.
        if let Some(focused) = self.coordinator.focused() {
            if self.coordinator.adapter_wants_mouse(focused) {
                let btn = if lines > 0.0 {
                    TermMouseButton::ScrollUp
                } else {
                    TermMouseButton::ScrollDown
                };
                if let Some((col, row)) = self.cursor_to_cell(focused) {
                    let sgr = encode_mouse_sgr(btn, col, row, true);
                    let _ = self.coordinator.route_bytes_to(focused, &sgr);
                    return;
                }
            }
        }

        // Route scroll to focused adapter via command.
        let int_lines = lines.round().abs().max(1.0) as u64;
        let direction = if lines > 0.0 { "up" } else { "down" };
        let _ = self.coordinator.send_command_to_focused(
            "scroll",
            &serde_json::json!({"direction": direction, "lines": int_lines}),
        );
    }
    fn build_context_menu_items(&self) -> Vec<MenuItem> {
        vec![
            MenuItem { label: "Copy".into(), action: MenuAction::Copy, enabled: true },
            MenuItem { label: "Paste".into(), action: MenuAction::Paste, enabled: true },
            MenuItem { label: "Select All".into(), action: MenuAction::SelectAll, enabled: true },
            MenuItem { label: "Split Horizontal".into(), action: MenuAction::SplitHorizontal, enabled: true },
            MenuItem { label: "Split Vertical".into(), action: MenuAction::SplitVertical, enabled: true },
            MenuItem { label: "Fullscreen".into(), action: MenuAction::Fullscreen, enabled: true },
        ]
    }

    fn execute_menu_action(&mut self, action: MenuAction) {
        match action {
            MenuAction::Copy => {
                if let Some(focused) = self.coordinator.focused() {
                    if let Ok(text) = self.coordinator.send_command(focused, "select_copy", &serde_json::json!({})) {
                        if !text.is_empty() {
                            if let Ok(mut clipboard) = arboard::Clipboard::new() {
                                let _ = clipboard.set_text(&text);
                            }
                            debug!("Context menu: copied {} chars", text.len());
                        }
                    }
                }
            }
            MenuAction::Paste => {
                if let Ok(mut clipboard) = arboard::Clipboard::new() {
                    if let Ok(text) = clipboard.get_text() {
                        self.coordinator.route_bytes(text.as_bytes());
                        debug!("Context menu: pasted {} bytes", text.len());
                    }
                }
            }
            MenuAction::SelectAll => {
                if let Some(focused) = self.coordinator.focused() {
                    let _ = self.coordinator.send_command(focused, "select_all", &serde_json::json!({}));
                }
            }
            MenuAction::SplitHorizontal => { self.split_focused_pane(true); }
            MenuAction::SplitVertical => { self.split_focused_pane(false); }
            MenuAction::Fullscreen => {
                if let Some(focused) = self.coordinator.focused() {
                    self.fullscreen_pane = Some(focused);
                }
            }
            MenuAction::Close => { self.close_focused_pane(); }
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
