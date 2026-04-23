//! Pane types and geometry helpers for the app container layout.
//!
//! Constants, the [`Pane`] struct, free geometry functions, and pane
//! management methods (`split_focused_pane`, `close_focused_pane`) all
//! live here so every resize / split / render path uses the same math.

use log::{info, warn};

use phantom_scene::node::NodeId;
use phantom_terminal::terminal::PhantomTerminal;
use phantom_ui::layout::PaneId;

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
// Pane
// ---------------------------------------------------------------------------

/// A terminal pane: owns a PTY-backed terminal emulator and its layout node.
pub(crate) struct Pane {
    pub(crate) terminal: PhantomTerminal,
    pub(crate) pane_id: PaneId,
    pub(crate) scene_node: NodeId,
    pub(crate) was_alt_screen: bool,
    pub(crate) is_detached: bool,
    pub(crate) detached_label: String,
    pub(crate) output_buf: String,
    pub(crate) error_notified: bool,
    /// Set when new PTY data arrives; cleared after semantic scan runs.
    pub(crate) has_new_output: bool,
}

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
// Pane management methods on App
// ---------------------------------------------------------------------------

impl App {
    /// Split the focused pane. `horizontal` = left|right, otherwise top|bottom.
    pub(crate) fn split_focused_pane(&mut self, horizontal: bool) {
        let Some(current) = self.panes.get(self.focused_pane) else { return };
        let current_pane_id = current.pane_id;

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

        self.panes[self.focused_pane].pane_id = existing_child;

        if let Ok(rect) = self.layout.get_pane_rect(existing_child) {
            let (cols, rows) = pane_cols_rows(self.cell_size, rect);
            self.panes[self.focused_pane].terminal.resize(cols, rows);
        }

        let new_rect = self.layout.get_pane_rect(new_child).unwrap_or_else(|e| {
            warn!("Layout missing for new split pane {new_child:?}: {e}");
            phantom_ui::layout::Rect { x: 0.0, y: 30.0, width: width as f32 / 2.0, height: height as f32 - 54.0 }
        });
        let (cols, rows) = pane_cols_rows(self.cell_size, new_rect);

        match PhantomTerminal::new(cols, rows) {
            Ok(terminal) => {
                // Create scene graph node for the new pane.
                let scene_node = self.scene.add_node(
                    self.scene_content_node,
                    phantom_scene::node::NodeKind::Pane,
                );
                let new_index = self.focused_pane + 1;
                self.panes.insert(new_index, Pane {
                    terminal,
                    pane_id: new_child,
                    scene_node,
                    was_alt_screen: false,
                    is_detached: false,
                    detached_label: String::new(),
                    output_buf: String::new(),
                    error_notified: false,
                    has_new_output: false,
                });
                self.focused_pane = new_index;
                info!("Split: new pane {new_index} ({cols}x{rows})");
            }
            Err(e) => {
                warn!("Failed to spawn terminal for new pane: {e}");
                let _ = self.layout.remove_pane(new_child);
            }
        }
    }

    /// Close the focused pane and its terminal.
    pub(crate) fn close_focused_pane(&mut self) {
        if self.panes.is_empty() {
            return;
        }

        if self.panes.len() == 1 {
            info!("Last pane closed, quitting");
            self.quit_requested = true;
            return;
        }

        let pane = self.panes.remove(self.focused_pane);
        if let Err(e) = self.layout.remove_pane(pane.pane_id) {
            warn!("Failed to remove pane from layout: {e}");
        }
        // Remove the corresponding scene graph node.
        self.scene.remove_node(pane.scene_node);
        drop(pane);

        let width = self.gpu.surface_config.width;
        let height = self.gpu.surface_config.height;
        let _ = self.layout.resize(width as f32, height as f32);

        if self.focused_pane >= self.panes.len() {
            self.focused_pane = self.panes.len().saturating_sub(1);
        }

        for pane in &mut self.panes {
            if let Ok(rect) = self.layout.get_pane_rect(pane.pane_id) {
                let (cols, rows) = pane_cols_rows(self.cell_size, rect);
                pane.terminal.resize(cols, rows);
            }
        }

        info!("Pane closed, focused: {}", self.focused_pane);
    }
}
