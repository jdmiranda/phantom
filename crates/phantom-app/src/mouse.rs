//! Mouse input handling for Phantom panes.
//!
//! Converts winit mouse events (clicks, scroll, motion) into either
//! internal scrollback operations or SGR 1006 escape sequences written
//! to the PTY when the running terminal program requests mouse tracking.
//! Also handles scrollbar click-to-jump.

use std::time::{Duration, Instant};

use log::{debug, warn};
use winit::event::{ElementState, MouseButton, MouseScrollDelta};

use phantom_terminal::input::{
    MouseButton as TermMouseButton, encode_mouse_motion_sgr, encode_mouse_sgr,
};

use crate::app::{App, FloatInteraction, ResizeEdge};
use crate::context_menu::{MenuAction, MenuItem};
use crate::pane::{
    container_rect, pane_inner_rect, point_in_rect, scrollbar_track_rect, scrollbar_y_to_offset,
};

// ---------------------------------------------------------------------------
// Helper: pixel coordinates to terminal cell
// ---------------------------------------------------------------------------

/// Convert pixel coordinates to terminal cell (col, row).
#[allow(clippy::too_many_arguments)]
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
// Hyperlink scheme allowlist
// ---------------------------------------------------------------------------

/// Returns `true` when a URI is safe to forward to the system's default URL
/// handler.
///
/// OSC 8 hyperlinks are emitted by an arbitrary terminal program — including
/// processes the user does not fully trust (remote SSH sessions, container
/// shells, build output piped through `less`, etc.). The OSC 8 spec puts no
/// restriction on the URI scheme, so a hostile process can stamp a cell with
/// `javascript:...`, `file:///etc/passwd`, `vscode://...`, or any custom
/// scheme an installed app has registered, and trigger that handler on a
/// click. We restrict click-to-open to `http://` and `https://`.
///
/// The scheme check is case-insensitive per RFC 3986 §3.1.
pub(crate) fn is_safe_hyperlink_scheme(uri: &str) -> bool {
    let Some(colon) = uri.find(':') else { return false };
    let scheme = &uri[..colon];
    // Reject empty or non-ASCII-alpha schemes (a real scheme is alpha then
    // alnum/+/-/. per RFC 3986).
    if scheme.is_empty() || !scheme.bytes().all(|b| b.is_ascii()) {
        return false;
    }
    scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")
}

/// Truncate a string for inclusion in a log line. Long URLs in attacker-
/// controlled escape sequences could spam the log; cap at 200 chars.
fn truncate_for_log(s: &str) -> String {
    const MAX: usize = 200;
    if s.len() <= MAX {
        s.to_owned()
    } else {
        let mut out = s.chars().take(MAX).collect::<String>();
        out.push_str("...");
        out
    }
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

        // Handle active floating pane drag/resize.
        if let Some(ref interaction) = self.float_interaction {
            match interaction {
                FloatInteraction::Dragging {
                    app_id,
                    offset_x,
                    offset_y,
                } => {
                    let new_x = (x as f32 - offset_x).max(0.0);
                    let new_y = (y as f32 - offset_y).max(0.0);
                    self.coordinator.move_floating(*app_id, new_x, new_y);
                }
                FloatInteraction::Resizing {
                    app_id,
                    edge,
                    initial_rect,
                } => {
                    let (new_w, new_h) = match edge {
                        ResizeEdge::Right => {
                            ((x as f32 - initial_rect.x).max(100.0), initial_rect.height)
                        }
                        ResizeEdge::Bottom => {
                            (initial_rect.width, (y as f32 - initial_rect.y).max(80.0))
                        }
                        ResizeEdge::BottomRight => (
                            (x as f32 - initial_rect.x).max(100.0),
                            (y as f32 - initial_rect.y).max(80.0),
                        ),
                    };
                    self.coordinator.resize_floating(*app_id, new_w, new_h);
                }
            }
            return;
        }

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
            if let Some(rect) = self.coordinator.float_rect(fid)
                && fx >= rect.x
                    && fx <= rect.x + rect.width
                    && fy >= rect.y
                    && fy <= rect.y + rect.height
                {
                    self.cursor_over_pane = Some(fid);
                    return;
                }
        }

        // Hit-test tiled coordinator-managed adapters.
        for app_id in self.coordinator.all_app_ids() {
            if let Some(pane_id) = self.coordinator.pane_id_for(app_id)
                && let Ok(layout_rect) = self.layout.get_pane_rect(pane_id) {
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

        // SGR 1006 motion forwarding: if the focused adapter has mouse tracking
        // enabled, encode cursor movement as an SGR motion sequence and forward
        // to the PTY. Motion mode (1003) sends all moves; Drag mode (1002)
        // only sends while a button is held.
        if let Some(focused) = self.coordinator.focused()
            && self.coordinator.adapter_wants_mouse(focused) {
                let state = self.coordinator.get_state(focused);
                let mouse_mode = state
                    .as_ref()
                    .and_then(|s| s.get("mouse_mode"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("none");
                let forward = match mouse_mode {
                    "motion" => true,
                    "drag" => self.mouse_button_held.is_some(),
                    _ => false,
                };
                if forward
                    && let Some((col, row)) = self.cursor_to_cell(focused) {
                        let btn = self.mouse_button_held.unwrap_or(TermMouseButton::Left);
                        let sgr = encode_mouse_motion_sgr(btn, col, row);
                        let _ = self.coordinator.route_bytes_to(focused, &sgr);
                    }
            }

        // If left button is held, update selection (drag-to-select).
        // Skip when SGR mouse tracking is active — the PTY handles its own selection.
        if self.mouse_button_held == Some(phantom_terminal::input::MouseButton::Left) {
            let sgr_active = self
                .coordinator
                .focused()
                .is_some_and(|id| self.coordinator.adapter_wants_mouse(id));
            if !sgr_active
                && let Some(focused) = self.coordinator.focused()
                    && let Some((col, row)) = self.cursor_to_cell(focused) {
                        let _ = self.coordinator.send_command(
                            focused,
                            "select_update",
                            &serde_json::json!({"col": col, "row": row}),
                        );
                    }
        }
    }

    /// Convert cursor pixel position to terminal cell (col, row) for the given adapter.
    ///
    /// In single-pane mode the renderer skips all container chrome (outer margin,
    /// title strip, side padding), so we must map the cursor directly against the
    /// raw layout rect.  The check mirrors the `tiled_count <= 1` guard that
    /// suppresses chrome drawing in `render.rs` (see PR #259).
    fn cursor_to_cell(&self, app_id: u32) -> Option<(usize, usize)> {
        let pane_id = self.coordinator.pane_id_for(app_id)?;
        let layout_rect = self.layout.get_pane_rect(pane_id).ok()?;

        // Single-pane: no chrome drawn -> use the full layout rect directly.
        let inner = if self.coordinator.tiled_visual_count() <= 1 {
            layout_rect
        } else {
            let cr = container_rect(layout_rect, self.cell_size);
            pane_inner_rect(self.cell_size, cr)
        };

        let (px, py) = self.cursor_position;
        // Compute grid dimensions from the rect for clamping.
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
            px,
            py,
            inner.x,
            inner.y,
            self.cell_size.0,
            self.cell_size.1,
            max_col.saturating_sub(1),
            max_row.saturating_sub(1),
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
        if let Some(term_btn) = winit_to_term_button(button)
            && let Some(focused) = self.coordinator.focused()
                && self.coordinator.adapter_wants_mouse(focused) {
                    let pressed = state == ElementState::Pressed;
                    if let Some((col, row)) = self.cursor_to_cell(focused) {
                        let sgr = encode_mouse_sgr(term_btn, col, row, pressed);
                        let _ = self.coordinator.route_bytes_to(focused, &sgr);
                        return;
                    }
                }

        // End float interaction on release.
        if state == ElementState::Released && self.float_interaction.is_some() {
            self.float_interaction = None;
            return;
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

        // Top theme strip — handle left clicks on swatches / CRT toggle before
        // any other hit-test so the picker is always reachable above the
        // pane chrome.
        if button == MouseButton::Left
            && let Ok(strip_rect) = self.layout.get_theme_strip_rect() {
                let ui_rect = phantom_ui::layout::Rect {
                    x: strip_rect.x,
                    y: strip_rect.y,
                    width: strip_rect.width,
                    height: strip_rect.height,
                };
                use phantom_ui::widgets::ThemeStripAction;
                match self.theme_strip.hit_test(&ui_rect, mx, my) {
                    ThemeStripAction::SetTheme(name) => {
                        self.apply_theme(&name);
                        return;
                    }
                    ThemeStripAction::ToggleCrt => {
                        self.toggle_crt();
                        return;
                    }
                    ThemeStripAction::None => {}
                }
            }

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

        // Left-click on the app launcher bar: toggle the clicked pane via
        // the existing `spawn_chrome::toggle_*_pane` helpers. The launcher
        // sits above the tab strip so we check it before everything else
        // chrome-layout-related.
        if button == MouseButton::Left
            && let Ok(launcher_rect) = self.layout.get_launcher_bar_rect()
        {
            use phantom_ui::widgets::LauncherAction;
            let action = self.app_launcher.hit_test(&launcher_rect, mx, my);
            if let LauncherAction::OpenPane(kind) = action {
                self.handle_launcher_action(kind);
                return;
            }
            // If the click landed inside the launcher rect but on padding
            // between chips, swallow it so it doesn't fall through to pane
            // focus / hyperlink handling underneath.
            if mx >= launcher_rect.x
                && mx <= launcher_rect.x + launcher_rect.width
                && my >= launcher_rect.y
                && my <= launcher_rect.y + launcher_rect.height
            {
                return;
            }
        }

        // Click-to-focus for floating panes (check first, highest z).
        if button == MouseButton::Left {
            let (fmx, fmy) = (self.cursor_position.0 as f32, self.cursor_position.1 as f32);
            let float_ids: Vec<_> = self.coordinator.floating_ids().collect();
            let mut float_focus = None;
            for fid in &float_ids {
                if let Some(rect) = self.coordinator.float_rect(*fid)
                    && fmx >= rect.x
                        && fmx <= rect.x + rect.width
                        && fmy >= rect.y
                        && fmy <= rect.y + rect.height
                    {
                        float_focus = Some(*fid);
                        break;
                    }
            }
            if let Some(fid) = float_focus {
                if self.coordinator.focused() != Some(fid) {
                    self.coordinator.set_focus(fid);
                }
                // Check title bar (top 24px) → drag, or edges → resize.
                if let Some(rect) = self.coordinator.float_rect(fid).cloned() {
                    if fmy < rect.y + 24.0 {
                        self.float_interaction = Some(FloatInteraction::Dragging {
                            app_id: fid,
                            offset_x: fmx - rect.x,
                            offset_y: fmy - rect.y,
                        });
                    } else {
                        let right = fmx > rect.x + rect.width - 8.0;
                        let bottom = fmy > rect.y + rect.height - 8.0;
                        if right && bottom {
                            self.float_interaction = Some(FloatInteraction::Resizing {
                                app_id: fid,
                                edge: ResizeEdge::BottomRight,
                                initial_rect: rect,
                            });
                        } else if right {
                            self.float_interaction = Some(FloatInteraction::Resizing {
                                app_id: fid,
                                edge: ResizeEdge::Right,
                                initial_rect: rect,
                            });
                        } else if bottom {
                            self.float_interaction = Some(FloatInteraction::Resizing {
                                app_id: fid,
                                edge: ResizeEdge::Bottom,
                                initial_rect: rect,
                            });
                        }
                    }
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

            let rapid = self.last_click_time.is_some_and(|t| {
                now.duration_since(t) < Duration::from_millis(400)
            });

            if rapid && near {
                self.click_count = (self.click_count + 1).min(3);
            } else {
                self.click_count = 1;
            }
            self.last_click_time = Some(now);
            self.last_click_pos = (px, py);

            // Clear any existing selection first (single click resets).
            if self.click_count == 1
                && let Some(focused) = self.coordinator.focused() {
                    let _ = self.coordinator.send_command(
                        focused,
                        "select_clear",
                        &serde_json::json!({}),
                    );
                }

            let (mx, my) = (self.cursor_position.0 as f32, self.cursor_position.1 as f32);

            // Check scrollbar hit on each coordinator-managed pane.
            // The track rect must match exactly what the renderer draws:
            //   • single-pane  → track anchored to the raw layout_rect
            //   • multi-pane   → track anchored to the chrome-inset inner_rect
            let tiled_count = self.coordinator.all_app_ids().len();
            for app_id in self.coordinator.all_app_ids() {
                let Some(pane_id) = self.coordinator.pane_id_for(app_id) else {
                    continue;
                };
                let Ok(layout_rect) = self.layout.get_pane_rect(pane_id) else {
                    continue;
                };
                let track = if tiled_count <= 1 {
                    scrollbar_track_rect(phantom_ui::layout::Rect {
                        x: layout_rect.x,
                        y: layout_rect.y,
                        width: layout_rect.width,
                        height: layout_rect.height,
                    })
                } else {
                    let outer = container_rect(layout_rect, self.cell_size);
                    let inner = pane_inner_rect(self.cell_size, outer);
                    scrollbar_track_rect(inner)
                };

                if point_in_rect(mx, my, track) {
                    // Query actual history size from adapter state for accurate jump.
                    let history_size = self
                        .coordinator
                        .get_state(app_id)
                        .and_then(|s| s.get("history_size").and_then(|v| v.as_u64()))
                        .unwrap_or(0) as usize;
                    if history_size > 0 {
                        let target_offset = scrollbar_y_to_offset(track, my, history_size);
                        let _ = self.coordinator.send_command(
                            app_id,
                            "scroll_to_offset",
                            &serde_json::json!({"offset": target_offset}),
                        );
                        debug!(
                            "Scrollbar click: adapter {app_id}, target_offset={target_offset}, history={history_size}"
                        );
                    }
                    return;
                }
            }

            // Click-to-focus: set coordinator focus to the pane under cursor.
            if let Some(app_id) = self.cursor_over_pane
                && self.coordinator.focused() != Some(app_id) {
                    debug!("Mouse focus: adapter {app_id}");
                    self.coordinator.set_focus(app_id);
                }

            // Hyperlink hit-test: on single click, check if the clicked cell
            // carries an OSC 8 hyperlink and open it in the default browser.
            //
            // Security: the URI is emitted by an arbitrary terminal program,
            // so we must NOT pass it to `open::that` unconditionally — that
            // would let a hostile or compromised process invoke any URL
            // handler installed on the host (e.g. `file://`, `vscode://`,
            // `slack://`, or a registered custom scheme). We allowlist
            // `http`/`https` only; everything else is logged and dropped.
            if self.click_count == 1
                && let Some(focused) = self.coordinator.focused()
                && let Some((col, row)) = self.cursor_to_cell(focused)
                && let Some(url) = self.coordinator.hyperlink_at(focused, col, row)
            {
                if is_safe_hyperlink_scheme(&url) {
                    debug!("Hyperlink click: {url}");
                    if let Err(e) = open::that(&url) {
                        warn!("Failed to open hyperlink {url}: {e}");
                    }
                } else {
                    warn!(
                        "Refusing to open hyperlink with disallowed scheme: {}",
                        truncate_for_log(&url)
                    );
                }
                return;
            }

            // Dispatch selection based on click count.
            if let Some(focused) = self.coordinator.focused()
                && let Some((col, row)) = self.cursor_to_cell(focused) {
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
        if let Some(focused) = self.coordinator.focused()
            && self.coordinator.adapter_wants_mouse(focused) {
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
            MenuItem {
                label: "Copy".into(),
                action: MenuAction::Copy,
                enabled: true,
            },
            MenuItem {
                label: "Paste".into(),
                action: MenuAction::Paste,
                enabled: true,
            },
            MenuItem {
                label: "Select All".into(),
                action: MenuAction::SelectAll,
                enabled: true,
            },
            MenuItem {
                label: "Split Horizontal".into(),
                action: MenuAction::SplitHorizontal,
                enabled: true,
            },
            MenuItem {
                label: "Split Vertical".into(),
                action: MenuAction::SplitVertical,
                enabled: true,
            },
            MenuItem {
                label: "Fullscreen".into(),
                action: MenuAction::Fullscreen,
                enabled: true,
            },
        ]
    }

    fn execute_menu_action(&mut self, action: MenuAction) {
        match action {
            MenuAction::Copy => {
                if let Some(focused) = self.coordinator.focused()
                    && let Ok(text) = self.coordinator.send_command(
                        focused,
                        "select_copy",
                        &serde_json::json!({}),
                    )
                        && !text.is_empty() {
                            if let Ok(mut clipboard) = arboard::Clipboard::new() {
                                let _ = clipboard.set_text(&text);
                            }
                            debug!("Context menu: copied {} chars", text.len());
                        }
            }
            MenuAction::Paste => {
                if let Ok(mut clipboard) = arboard::Clipboard::new()
                    && let Ok(text) = clipboard.get_text() {
                        self.coordinator.route_bytes(text.as_bytes());
                        debug!("Context menu: pasted {} bytes", text.len());
                    }
            }
            MenuAction::SelectAll => {
                if let Some(focused) = self.coordinator.focused() {
                    let _ = self.coordinator.send_command(
                        focused,
                        "select_all",
                        &serde_json::json!({}),
                    );
                }
            }
            MenuAction::SplitHorizontal => {
                self.split_focused_pane(true);
            }
            MenuAction::SplitVertical => {
                self.split_focused_pane(false);
            }
            MenuAction::Fullscreen => {
                if let Some(focused) = self.coordinator.focused() {
                    self.fullscreen_pane = Some(focused);
                }
            }
            MenuAction::Close => {
                self.close_focused_pane();
            }
        }
    }

    /// Route a click on the app-launcher bar to the matching chrome-pane
    /// spawner. Mirrors the keybind handler in `input.rs` — clicking a chip
    /// is equivalent to pressing the chip's keybind.
    pub(crate) fn handle_launcher_action(&mut self, kind: phantom_ui::widgets::LauncherPaneKind) {
        use phantom_ui::widgets::LauncherPaneKind as K;
        match kind {
            K::Inspector => {
                // Inspector still uses the legacy `spawn_inspector_pane` rather
                // than a `toggle_*` helper. We mirror the Cmd+I path here.
                if self.spawn_inspector_pane() {
                    self.console.system("Inspector pane opened.");
                }
            }
            K::Memory => crate::spawn_chrome::toggle_memory_pane(self),
            K::Settings => crate::spawn_chrome::toggle_settings_pane(self),
            K::Logs => crate::spawn_chrome::toggle_logs_pane(self),
            K::Notifications => crate::spawn_chrome::toggle_notifications_pane(self),
            K::FilesWatch => crate::spawn_chrome::toggle_files_watch_pane(self),
            K::Diff => crate::spawn_chrome::toggle_diff_pane(self),
            K::Fleet => crate::spawn_chrome::toggle_fleet_pane(self),
            K::Plugins => crate::spawn_chrome::toggle_plugins_pane(self),
            K::Database => crate::spawn_chrome::toggle_database_pane(self),
            K::VoiceStt => crate::spawn_chrome::toggle_voice_stt_pane(self),
            K::KeybindsHelp => crate::spawn_chrome::toggle_keybinds_help_pane(self),
            K::Console => crate::spawn_chrome::toggle_console_pane(self),
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
        assert_eq!(
            winit_to_term_button(MouseButton::Left),
            Some(TermMouseButton::Left)
        );
        assert_eq!(
            winit_to_term_button(MouseButton::Right),
            Some(TermMouseButton::Right)
        );
        assert_eq!(
            winit_to_term_button(MouseButton::Middle),
            Some(TermMouseButton::Middle)
        );
        assert_eq!(winit_to_term_button(MouseButton::Back), None);
        assert_eq!(winit_to_term_button(MouseButton::Forward), None);
    }

    // -- Bug #2: SGR motion encoding regression tests --

    /// Motion events must add 32 to the button code (SGR 1006 spec).
    /// Left (0+32=32), Right (2+32=34).
    #[test]
    fn sgr_motion_adds_32_to_button_code() {
        let left_motion = encode_mouse_motion_sgr(TermMouseButton::Left, 0, 0);
        // Left = 0, motion adds 32 → code 32; coords 1-based → col=1 row=1; always M
        assert_eq!(left_motion, b"\x1b[<32;1;1M");

        let right_motion = encode_mouse_motion_sgr(TermMouseButton::Right, 5, 10);
        // Right = 2, motion adds 32 → code 34; col=6, row=11
        assert_eq!(right_motion, b"\x1b[<34;6;11M");
    }

    /// No-button motion (cursor move without any held button) uses Left (code 32).
    #[test]
    fn sgr_motion_no_button_defaults_to_left_code() {
        let motion = encode_mouse_motion_sgr(TermMouseButton::Left, 79, 23);
        // col=80, row=24
        assert_eq!(motion, b"\x1b[<32;80;24M");
    }

    // -- Bug #1: single-pane scrollbar geometry regression tests --
    // These test the helper functions used by the renderer and click handler.

    /// scrollbar_track_rect should be anchored to the rect origin, not require
    /// chrome-inset correction in single-pane mode.
    #[test]
    fn scrollbar_track_rect_single_pane_anchoring() {
        use crate::pane::{SCROLLBAR_WIDTH, scrollbar_track_rect};

        // Simulate a full-screen layout rect (no chrome insets applied).
        let layout_rect = phantom_ui::layout::Rect {
            x: 0.0,
            y: 0.0,
            width: 1280.0,
            height: 800.0,
        };

        let track = scrollbar_track_rect(layout_rect);
        let margin = 2.0;

        // Track x must be at the right edge minus scrollbar width and margin.
        let expected_x = layout_rect.x + layout_rect.width - SCROLLBAR_WIDTH - margin;
        assert!(
            (track.x - expected_x).abs() < 0.01,
            "track.x={} expected {}",
            track.x,
            expected_x
        );

        // Track y starts at layout_rect.y + margin.
        assert!(
            (track.y - (layout_rect.y + margin)).abs() < 0.01,
            "track.y={} expected {}",
            track.y,
            layout_rect.y + margin
        );

        // Track height is inset by margin on both ends.
        let expected_h = layout_rect.height - margin * 2.0;
        assert!(
            (track.height - expected_h).abs() < 0.01,
            "track.height={} expected {}",
            track.height,
            expected_h
        );

        assert_eq!(track.width, SCROLLBAR_WIDTH);
    }

    /// scrollbar_thumb_rect returns None when no history exists.
    #[test]
    fn scrollbar_thumb_absent_without_history() {
        use crate::pane::{scrollbar_thumb_rect, scrollbar_track_rect};

        let rect = phantom_ui::layout::Rect {
            x: 0.0,
            y: 0.0,
            width: 800.0,
            height: 600.0,
        };
        let track = scrollbar_track_rect(rect);
        assert!(scrollbar_thumb_rect(track, 0, 0, 24).is_none());
    }

    /// scrollbar_thumb_rect returns Some when history is non-zero.
    #[test]
    fn scrollbar_thumb_present_with_history() {
        use crate::pane::{scrollbar_thumb_rect, scrollbar_track_rect};

        let rect = phantom_ui::layout::Rect {
            x: 0.0,
            y: 0.0,
            width: 800.0,
            height: 600.0,
        };
        let track = scrollbar_track_rect(rect);
        let thumb = scrollbar_thumb_rect(track, 0, 500, 24);
        assert!(thumb.is_some(), "thumb should exist when history_size > 0");
        let t = thumb.unwrap();
        assert!(t.height > 0.0);
        assert!(t.y >= track.y);
        assert!(t.y + t.height <= track.y + track.height + 1.0); // +1 for float rounding
    }

    // -----------------------------------------------------------------------
    // Issue #261 -- single-pane chrome inset regression
    // -----------------------------------------------------------------------
    //
    // Regression guard: `cursor_to_cell` must skip container/pane-inner chrome
    // insets when only one tiled pane is present (no chrome is drawn).
    //
    // Verifies:
    //  1. On the single-pane path, cursor at (0,0) maps to cell (0,0).
    //  2. The multi-pane chrome insets produce a strictly positive inner origin,
    //     which would shift the coordinate mapping -- proving the bug was real.

    #[test]
    fn cursor_to_cell_single_pane_no_chrome_offset() {
        use crate::pane::{CONTAINER_TITLE_H_CELLS, container_rect, pane_inner_rect};
        use phantom_ui::layout::Rect;

        let cell_w = 8.0_f32;
        let cell_h = 16.0_f32;
        let cell_size = (cell_w, cell_h);

        // Full-screen layout rect (single-pane: no margin/chrome applied).
        let layout_rect = Rect {
            x: 0.0,
            y: 0.0,
            width: 1280.0,
            height: 800.0,
        };

        // -- Single-pane path (the fix) -------------------------------------
        // inner == layout_rect, so cursor (0,0) -> cell (0,0).
        let inner_single = layout_rect;
        let max_col = (inner_single.width / cell_w).floor() as usize;
        let max_row = (inner_single.height / cell_h).floor() as usize;
        let (col, row) = pixel_to_cell(
            0.0,
            0.0,
            inner_single.x,
            inner_single.y,
            cell_w,
            cell_h,
            max_col.saturating_sub(1),
            max_row.saturating_sub(1),
        );
        assert_eq!(
            (col, row),
            (0, 0),
            "single-pane: cursor at (0,0) must map to cell (0,0); got ({col},{row})",
        );

        // -- Multi-pane / pre-fix path --------------------------------------
        // Chrome insets produce a non-zero inner origin, shifting coordinates.
        let cr = container_rect(layout_rect, cell_size);
        let inner_multi = pane_inner_rect(cell_size, cr);

        assert!(
            inner_multi.x > 0.0 || inner_multi.y > 0.0,
            "multi-pane inner rect must have a non-zero origin due to chrome insets; \
             got ({}, {})",
            inner_multi.x,
            inner_multi.y,
        );

        // inner.y must be at least one title-strip height from the window top,
        // documenting the ~12px + 1.2-cell offset that #261 reported.
        let title_h = cell_h * CONTAINER_TITLE_H_CELLS;
        assert!(
            inner_multi.y >= title_h - 0.5,
            "multi-pane inner.y must be at least one title-strip height ({title_h}px) \
             from the window top; got {}",
            inner_multi.y,
        );
    }

    // -- OSC 8 hyperlink scheme allowlist ---------------------------------

    #[test]
    fn safe_hyperlink_scheme_accepts_http_https() {
        assert!(is_safe_hyperlink_scheme("http://example.com"));
        assert!(is_safe_hyperlink_scheme("https://example.com/path?q=1"));
    }

    #[test]
    fn safe_hyperlink_scheme_is_case_insensitive() {
        assert!(is_safe_hyperlink_scheme("HTTP://example.com"));
        assert!(is_safe_hyperlink_scheme("HtTpS://example.com"));
    }

    #[test]
    fn safe_hyperlink_scheme_rejects_javascript() {
        // The classic exploit: clicking opens a JS execution context in the
        // default browser. Must be refused.
        assert!(!is_safe_hyperlink_scheme("javascript:alert(1)"));
        assert!(!is_safe_hyperlink_scheme("JavaScript:alert(1)"));
    }

    #[test]
    fn safe_hyperlink_scheme_rejects_file() {
        // file:// would let a terminal program point the browser at any
        // local resource the user can read.
        assert!(!is_safe_hyperlink_scheme("file:///etc/passwd"));
    }

    #[test]
    fn safe_hyperlink_scheme_rejects_app_handlers() {
        // Custom-scheme handlers registered by other apps would otherwise be
        // hijackable. Vscode, Slack, Zoom, etc.
        assert!(!is_safe_hyperlink_scheme("vscode://file/etc/passwd"));
        assert!(!is_safe_hyperlink_scheme("slack://channel/foo"));
        assert!(!is_safe_hyperlink_scheme("zoommtg://zoom.us/join?confno=1"));
        assert!(!is_safe_hyperlink_scheme("ftp://example.com"));
        assert!(!is_safe_hyperlink_scheme("mailto:a@b.c"));
        assert!(!is_safe_hyperlink_scheme("data:text/html,<script>alert(1)</script>"));
    }

    #[test]
    fn safe_hyperlink_scheme_rejects_malformed() {
        // No colon, no scheme.
        assert!(!is_safe_hyperlink_scheme("example.com"));
        assert!(!is_safe_hyperlink_scheme(""));
        // Empty scheme.
        assert!(!is_safe_hyperlink_scheme(":foo"));
        // Non-ASCII bytes in scheme position.
        assert!(!is_safe_hyperlink_scheme("ht\u{00e9}p://example.com"));
    }

    #[test]
    fn truncate_for_log_passes_short_strings() {
        assert_eq!(truncate_for_log("hello"), "hello");
    }

    #[test]
    fn truncate_for_log_caps_long_strings() {
        let long = "a".repeat(500);
        let out = truncate_for_log(&long);
        assert!(out.ends_with("..."));
        assert!(out.len() <= 210);
    }
}
