//! Pane types and geometry helpers for the app container layout.
//!
//! Constants, the [`Pane`] struct, free geometry functions, and pane
//! management methods (`split_focused_pane`, `close_focused_pane`) all
//! live here so every resize / split / render path uses the same math.

use log::{info, warn};

use phantom_terminal::terminal::PhantomTerminal;

use crate::app::App;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Horizontal padding inside the app container, in multiples of cell width.
pub(crate) const CONTAINER_PAD_X_CELLS: f32 = 0.6;

/// Title-strip height, in multiples of cell height.
pub(crate) const CONTAINER_TITLE_H_CELLS: f32 = 1.2;

/// Bottom padding inside the app container, in multiples of cell height.
pub(crate) const CONTAINER_PAD_B_CELLS: f32 = 0.3;

/// Outer margin around each container, in pixels.
pub(crate) const CONTAINER_MARGIN: f32 = 12.0;

// ---------------------------------------------------------------------------
// Geometry helpers
// ---------------------------------------------------------------------------

/// Translate an MCP `phantom.send_key` argument into terminal input bytes.
pub(crate) fn key_name_to_bytes(key: &str) -> Vec<u8> {
    match key {
        "Enter" | "Return" => b"\r".to_vec(),
        "Tab" => b"\t".to_vec(),
        "Escape" | "Esc" => b"\x1b".to_vec(),
        "Space" => b" ".to_vec(),
        "Backspace" => b"\x7f".to_vec(),
        "Up" => b"\x1b[A".to_vec(),
        "Down" => b"\x1b[B".to_vec(),
        "Right" => b"\x1b[C".to_vec(),
        "Left" => b"\x1b[D".to_vec(),
        other => other.as_bytes().to_vec(),
    }
}

/// Apply the outer margin to a layout rect, producing the container rect.
pub(crate) fn container_rect(layout_rect: phantom_ui::layout::Rect, cell_size: (f32, f32)) -> phantom_ui::layout::Rect {
    let m = CONTAINER_MARGIN;
    phantom_ui::layout::Rect {
        x: layout_rect.x + m,
        y: layout_rect.y + m * 0.5,
        width: (layout_rect.width - m * 2.0).max(cell_size.0),
        height: (layout_rect.height - m).max(cell_size.1),
    }
}

/// Compute terminal cols/rows from a layout rect, accounting for container
/// margin and inner chrome padding.
pub(crate) fn pane_cols_rows(cell_size: (f32, f32), layout_rect: phantom_ui::layout::Rect) -> (u16, u16) {
    let inner = pane_inner_rect(cell_size, container_rect(layout_rect, cell_size));
    let cols = (inner.width / cell_size.0).floor().max(1.0) as u16;
    let rows = (inner.height / cell_size.1).floor().max(1.0) as u16;
    (cols, rows)
}

/// Compute the terminal-grid area inside a container rect.
pub(crate) fn pane_inner_rect(cell_size: (f32, f32), outer: phantom_ui::layout::Rect) -> phantom_ui::layout::Rect {
    let pad_x = cell_size.0 * CONTAINER_PAD_X_CELLS;
    let title_h = cell_size.1 * CONTAINER_TITLE_H_CELLS;
    let pad_b = cell_size.1 * CONTAINER_PAD_B_CELLS;
    let w = (outer.width - pad_x * 2.0).max(cell_size.0);
    let h = (outer.height - title_h - pad_b).max(cell_size.1);
    phantom_ui::layout::Rect {
        x: outer.x + pad_x,
        y: outer.y + title_h,
        width: w,
        height: h,
    }
}

// ---------------------------------------------------------------------------
// Scrollbar geometry
// ---------------------------------------------------------------------------

/// Width of the scrollbar track in pixels.
pub(crate) const SCROLLBAR_WIDTH: f32 = 6.0;

/// Compute the scrollbar track rectangle, anchored to the right edge of the
/// terminal inner rect with a small inset.
pub(crate) fn scrollbar_track_rect(inner: phantom_ui::layout::Rect) -> phantom_ui::layout::Rect {
    let margin = 2.0;
    phantom_ui::layout::Rect {
        x: inner.x + inner.width - SCROLLBAR_WIDTH - margin,
        y: inner.y + margin,
        width: SCROLLBAR_WIDTH,
        height: inner.height - margin * 2.0,
    }
}

/// Compute the scrollbar thumb rectangle within the track.
pub(crate) fn scrollbar_thumb_rect(
    track: phantom_ui::layout::Rect,
    display_offset: usize,
    history_size: usize,
    visible_rows: usize,
) -> Option<phantom_ui::layout::Rect> {
    let total = history_size + visible_rows;
    if total == 0 || history_size == 0 {
        return None;
    }

    let ratio = visible_rows as f32 / total as f32;
    let thumb_h = (track.height * ratio).max(20.0);

    if thumb_h >= track.height {
        return None;
    }

    let scroll_fraction = display_offset as f32 / history_size as f32;
    let max_y_offset = track.height - thumb_h;
    let thumb_y = track.y + max_y_offset * (1.0 - scroll_fraction);

    Some(phantom_ui::layout::Rect {
        x: track.x,
        y: thumb_y,
        width: track.width,
        height: thumb_h,
    })
}

/// Convert a Y pixel coordinate within the scrollbar track to a display_offset.
pub(crate) fn scrollbar_y_to_offset(
    track: phantom_ui::layout::Rect,
    click_y: f32,
    history_size: usize,
) -> usize {
    if track.height <= 0.0 || history_size == 0 {
        return 0;
    }
    let frac = ((click_y - track.y) / track.height).clamp(0.0, 1.0);
    let offset = ((1.0 - frac) * history_size as f32).round() as usize;
    offset.min(history_size)
}

/// Check if a point (x, y) is inside a rect.
pub(crate) fn point_in_rect(x: f32, y: f32, rect: phantom_ui::layout::Rect) -> bool {
    x >= rect.x && x <= rect.x + rect.width && y >= rect.y && y <= rect.y + rect.height
}

// ---------------------------------------------------------------------------
// Pane management methods on App (coordinator-based)
// ---------------------------------------------------------------------------

impl App {
    /// Split the focused pane. `horizontal` = left|right, otherwise top|bottom.
    pub(crate) fn split_focused_pane(&mut self, horizontal: bool) {
        // Get the focused adapter's PaneId.
        let Some(focused_app_id) = self.coordinator.focused() else {
            warn!("Split: no focused adapter");
            return;
        };
        let Some(current_pane_id) = self.coordinator.pane_id_for(focused_app_id) else {
            warn!("Split: focused adapter has no layout pane");
            return;
        };

        // Ask the layout engine to split.
        let split_result = if horizontal {
            self.layout.split_horizontal(current_pane_id)
        } else {
            self.layout.split_vertical(current_pane_id)
        };

        let (existing_child, new_child) = match split_result {
            Ok(ids) => ids,
            Err(e) => {
                warn!("Split failed: {e}");
                return;
            }
        };

        let width = self.gpu.surface_config.width;
        let height = self.gpu.surface_config.height;
        if let Err(e) = self.layout.resize(width as f32, height as f32) {
            warn!("Layout resize after split failed: {e}");
        }

        // Update the existing adapter's PaneId mapping (split replaced the old PaneId).
        self.coordinator.remap_pane(focused_app_id, current_pane_id, existing_child);

        // Resize the existing adapter's terminal to fit the new (smaller) pane.
        if let Ok(rect) = self.layout.get_pane_rect(existing_child) {
            let (cols, rows) = pane_cols_rows(self.cell_size, rect);
            let _ = self.coordinator.send_command(
                focused_app_id,
                "resize",
                &serde_json::json!({"cols": cols, "rows": rows}),
            );
        }

        // Create a new terminal for the new pane.
        let new_rect = self.layout.get_pane_rect(new_child).unwrap_or_else(|e| {
            warn!("Layout missing for new split pane {new_child:?}: {e}");
            phantom_ui::layout::Rect {
                x: 0.0, y: 30.0,
                width: width as f32 / 2.0,
                height: height as f32 - 54.0,
            }
        });
        let (cols, rows) = pane_cols_rows(self.cell_size, new_rect);

        match PhantomTerminal::new(cols, rows) {
            Ok(terminal) => {
                use crate::adapters::terminal::TerminalAdapter;
                use phantom_terminal::output::TerminalThemeColors;
                use phantom_scene::clock::Cadence;

                let theme_colors = TerminalThemeColors {
                    foreground: self.theme.colors.foreground,
                    background: self.theme.colors.background,
                    cursor: self.theme.colors.cursor,
                    ansi: Some(self.theme.colors.ansi),
                };

                let scene_node = self.scene.add_node(
                    self.scene_content_node,
                    phantom_scene::node::NodeKind::Pane,
                );

                let adapter = TerminalAdapter::with_theme(terminal, theme_colors);
                let new_app_id = self.coordinator.register_adapter_at_pane(
                    Box::new(adapter),
                    new_child,
                    scene_node,
                    Cadence::unlimited(),
                );
                self.coordinator.set_focus(new_app_id);
                info!("Split: new adapter {new_app_id} ({cols}x{rows})");
            }
            Err(e) => {
                warn!("Failed to spawn terminal for new pane: {e}");
                let _ = self.layout.remove_pane(new_child);
            }
        }
    }

    /// Close the focused pane and its terminal.
    pub(crate) fn close_focused_pane(&mut self) {
        let Some(focused_app_id) = self.coordinator.focused() else {
            return;
        };

        // Last adapter — quit the app.
        if self.coordinator.adapter_count() <= 1 {
            info!("Last pane closed, quitting");
            self.quit_requested = true;
            return;
        }

        // Remove the adapter (handles layout, scene, focus shift).
        self.coordinator.remove_adapter(focused_app_id, &mut self.layout, &mut self.scene);

        let width = self.gpu.surface_config.width;
        let height = self.gpu.surface_config.height;
        let _ = self.layout.resize(width as f32, height as f32);

        // Resize remaining adapters to fit the reclaimed space.
        for app_id in self.coordinator.all_app_ids() {
            if let Some(pane_id) = self.coordinator.pane_id_for(app_id) {
                if let Ok(rect) = self.layout.get_pane_rect(pane_id) {
                    let (cols, rows) = pane_cols_rows(self.cell_size, rect);
                    let _ = self.coordinator.send_command(
                        app_id,
                        "resize",
                        &serde_json::json!({"cols": cols, "rows": rows}),
                    );
                }
            }
        }

        info!("Pane closed, focused: {:?}", self.coordinator.focused());
    }
}

#[cfg(test)]
mod scrollbar_tests {
    use super::*;
    use phantom_ui::layout::Rect;

    fn test_inner() -> Rect {
        Rect { x: 100.0, y: 50.0, width: 400.0, height: 300.0 }
    }

    #[test]
    fn track_rect_positioned_at_right_edge() {
        let track = scrollbar_track_rect(test_inner());
        assert!((track.x - (100.0 + 400.0 - SCROLLBAR_WIDTH - 2.0)).abs() < 0.01);
        assert_eq!(track.width, SCROLLBAR_WIDTH);
    }

    #[test]
    fn thumb_none_when_no_history() {
        let track = scrollbar_track_rect(test_inner());
        assert!(scrollbar_thumb_rect(track, 0, 0, 24).is_none());
    }

    #[test]
    fn thumb_at_bottom_when_offset_zero() {
        let track = scrollbar_track_rect(test_inner());
        let thumb = scrollbar_thumb_rect(track, 0, 1000, 24).unwrap();
        let expected_bottom = track.y + track.height;
        let actual_bottom = thumb.y + thumb.height;
        assert!((actual_bottom - expected_bottom).abs() < 1.0);
    }

    #[test]
    fn thumb_at_top_when_fully_scrolled() {
        let track = scrollbar_track_rect(test_inner());
        let thumb = scrollbar_thumb_rect(track, 1000, 1000, 24).unwrap();
        assert!((thumb.y - track.y).abs() < 1.0);
    }

    // -- scrollbar_y_to_offset tests --

    fn make_track() -> Rect {
        Rect { x: 100.0, y: 50.0, width: 6.0, height: 400.0 }
    }

    #[test]
    fn y_to_offset_top_gives_max() {
        let track = make_track();
        let offset = scrollbar_y_to_offset(track, track.y, 500);
        assert_eq!(offset, 500);
    }

    #[test]
    fn y_to_offset_bottom_gives_zero() {
        let track = make_track();
        let offset = scrollbar_y_to_offset(track, track.y + track.height, 500);
        assert_eq!(offset, 0);
    }

    #[test]
    fn y_to_offset_middle_gives_half() {
        let track = make_track();
        let mid_y = track.y + track.height / 2.0;
        let offset = scrollbar_y_to_offset(track, mid_y, 500);
        assert_eq!(offset, 250);
    }

    #[test]
    fn y_to_offset_zero_history() {
        let track = make_track();
        let offset = scrollbar_y_to_offset(track, track.y + 100.0, 0);
        assert_eq!(offset, 0);
    }

    #[test]
    fn y_to_offset_zero_height() {
        let track = Rect { x: 0.0, y: 0.0, width: 6.0, height: 0.0 };
        let offset = scrollbar_y_to_offset(track, 50.0, 100);
        assert_eq!(offset, 0);
    }

    #[test]
    fn y_to_offset_clamps_above_track() {
        let track = make_track();
        let offset = scrollbar_y_to_offset(track, track.y - 100.0, 500);
        assert_eq!(offset, 500);
    }

    #[test]
    fn y_to_offset_clamps_below_track() {
        let track = make_track();
        let offset = scrollbar_y_to_offset(track, track.y + track.height + 100.0, 500);
        assert_eq!(offset, 0);
    }

    // -- point_in_rect tests --

    #[test]
    fn point_inside_rect() {
        let rect = Rect { x: 10.0, y: 20.0, width: 100.0, height: 50.0 };
        assert!(point_in_rect(50.0, 40.0, rect));
    }

    #[test]
    fn point_on_edge() {
        let rect = Rect { x: 10.0, y: 20.0, width: 100.0, height: 50.0 };
        assert!(point_in_rect(10.0, 20.0, rect)); // top-left corner
        assert!(point_in_rect(110.0, 70.0, rect)); // bottom-right corner
    }

    #[test]
    fn point_outside_rect() {
        let rect = Rect { x: 10.0, y: 20.0, width: 100.0, height: 50.0 };
        assert!(!point_in_rect(5.0, 40.0, rect)); // left of rect
        assert!(!point_in_rect(50.0, 15.0, rect)); // above rect
        assert!(!point_in_rect(111.0, 40.0, rect)); // right of rect
        assert!(!point_in_rect(50.0, 71.0, rect)); // below rect
    }
}

// ---------------------------------------------------------------------------
// Issue #173 — Scroll position: 1000-line buffer, 40-row viewport
// ---------------------------------------------------------------------------

#[cfg(test)]
mod scroll_position_tests {
    use super::*;
    use phantom_ui::layout::Rect;

    const HISTORY: usize = 1000;
    const VIEWPORT: usize = 40;

    fn track() -> Rect {
        Rect { x: 494.0, y: 50.0, width: 6.0, height: 400.0 }
    }

    // scrollbar_y_to_offset maps a click y position to a display_offset.
    // At mid-track the result must be history_size / 2 = 500.
    #[test]
    fn scroll_to_500_gives_offset_500() {
        let t = track();
        let mid_y = t.y + t.height / 2.0;
        let offset = scrollbar_y_to_offset(t, mid_y, HISTORY);
        assert_eq!(offset, 500);
    }

    // Clicking at the top of the track means oldest content → max offset.
    #[test]
    fn scroll_to_top_gives_max_offset() {
        let t = track();
        let offset = scrollbar_y_to_offset(t, t.y, HISTORY);
        assert_eq!(offset, HISTORY);
    }

    // Clicking at the bottom of the track means live end → offset 0.
    #[test]
    fn scroll_to_end_gives_offset_zero() {
        let t = track();
        let offset = scrollbar_y_to_offset(t, t.y + t.height, HISTORY);
        assert_eq!(offset, 0);
    }

    // Clicking above the track must clamp to history_size without panicking.
    #[test]
    fn scroll_past_top_clamps_no_panic() {
        let t = track();
        let offset = scrollbar_y_to_offset(t, t.y - 100.0, HISTORY);
        assert_eq!(offset, HISTORY, "clamped at history_size");
    }

    // Clicking below the track must clamp to 0 without panicking.
    #[test]
    fn scroll_past_end_clamps_no_panic() {
        let t = track();
        let offset = scrollbar_y_to_offset(t, t.y + t.height + 100.0, HISTORY);
        assert_eq!(offset, 0, "clamped at zero");
    }

    // Zero-history edge case must not panic and must return 0.
    #[test]
    fn scroll_zero_history_returns_zero_no_panic() {
        let t = track();
        let offset = scrollbar_y_to_offset(t, t.y + 100.0, 0);
        assert_eq!(offset, 0);
    }

    // With a 1000-line history and 40-row viewport the thumb must be present
    // (history > visible_rows means there is something to scroll).
    #[test]
    fn thumb_is_present_with_1000_line_history() {
        let t = track();
        let thumb = scrollbar_thumb_rect(t, 0, HISTORY, VIEWPORT);
        assert!(thumb.is_some(), "thumb must exist when history > viewport");
    }

    // When history_size == 0 there is nothing to scroll; no thumb.
    #[test]
    fn thumb_absent_when_no_history() {
        let t = track();
        let thumb = scrollbar_thumb_rect(t, 0, 0, VIEWPORT);
        assert!(thumb.is_none(), "no thumb when history is empty");
    }

    // At live end (offset = 0) the thumb should be near the bottom of the track.
    #[test]
    fn thumb_at_bottom_when_at_live_end() {
        let t = track();
        let thumb = scrollbar_thumb_rect(t, 0, HISTORY, VIEWPORT).unwrap();
        // The thumb's top edge must be in the lower half of the track.
        assert!(
            thumb.y > t.y + t.height / 2.0,
            "thumb must sit in lower half when at live end (offset=0)"
        );
    }

    // At the oldest content (offset = history_size) the thumb must be near the top.
    #[test]
    fn thumb_at_top_when_at_oldest_end() {
        let t = track();
        let thumb = scrollbar_thumb_rect(t, HISTORY, HISTORY, VIEWPORT).unwrap();
        // The thumb's top edge must be in the upper half of the track.
        assert!(
            thumb.y <= t.y + t.height / 2.0,
            "thumb must sit in upper half when at oldest end (offset=history_size)"
        );
    }
}
