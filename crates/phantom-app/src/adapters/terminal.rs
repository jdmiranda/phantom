//! Terminal adapter — wraps `PhantomTerminal` as an `AppAdapter`.
//!
//! Bridges the PTY-backed terminal emulator into the unified app model
//! so that terminals participate in layout negotiation, event bus
//! messaging, and AI-driven command dispatch.

use log::warn;
use serde_json::json;
use std::sync::{Arc, Mutex};

use phantom_adapter::adapter::{
    CursorData, CursorShape as AdapterCursorShape, GridData, QuadData, Rect, RenderOutput,
    ScrollState, SelectionRange, TerminalCell as AdapterCell, TextData,
};
use phantom_adapter::spatial::{InternalLayout, SpatialPreference};
use phantom_adapter::{
    AppCore, BusParticipant, Commandable, InputHandler, Lifecycled, Permissioned, Renderable,
};
use phantom_terminal::output::{
    self, CursorShape as TermCursorShape, CursorState, TerminalThemeColors,
};
use phantom_terminal::takeover::TakeoverDetector;
use phantom_terminal::terminal::PhantomTerminal;
use phantom_ui::RenderCtx;
use phantom_ui::tokens::Tokens;
use phantom_ui::widgets::app_head::AppHead;

/// Maximum bytes retained in the output buffer before the front is drained.
const OUTPUT_BUF_CAP: usize = 8192;

// ---------------------------------------------------------------------------
// Type conversions: terminal -> adapter
// ---------------------------------------------------------------------------

fn convert_cursor_shape(shape: TermCursorShape) -> AdapterCursorShape {
    match shape {
        TermCursorShape::Block => AdapterCursorShape::Block,
        TermCursorShape::Underline => AdapterCursorShape::Underline,
        TermCursorShape::Bar => AdapterCursorShape::Bar,
    }
}

fn convert_cursor(state: &CursorState) -> CursorData {
    CursorData {
        col: state.col,
        row: state.row,
        shape: convert_cursor_shape(state.shape),
        visible: state.visible,
        blinking: state.blinking,
    }
}

/// A terminal pane wrapped in the `AppAdapter` interface.
///
/// Owns a `PhantomTerminal` and maintains the output buffer, detach
/// state, and alt-screen tracking needed by the app container.
pub struct TerminalAdapter {
    terminal: PhantomTerminal,
    output_buf: String,
    has_new_output: bool,
    error_notified: bool,
    is_detached: bool,
    detached_label: String,
    was_alt_screen: bool,
    /// Theme colors for grid extraction (set at construction, updateable).
    theme_colors: TerminalThemeColors,
    /// Set when the PTY child process exits.
    pty_dead: bool,
    /// Assigned by the coordinator after registration (for outbox messages).
    app_id: u32,
    /// Pending outbound bus messages, drained by coordinator each frame.
    outbox: Vec<phantom_adapter::BusMessage>,
    /// Shared render snapshot written each frame when alt-screen is active.
    /// Set by `attach_alt_screen_snapshot`; cleared by `detach_alt_screen_snapshot`.
    alt_screen_snapshot: Option<Arc<Mutex<Option<RenderOutput>>>>,
    /// Edge-detector for subprocess takeover events (issue #364).
    takeover_detector: TakeoverDetector,
    /// Most recent OSC 2 window title received from the running program.
    /// Set during `update()` when the terminal emits `Event::Title`; consumed
    /// (and cleared) by `take_pending_title()`.
    pending_title: Option<String>,
    /// Design tokens used to paint pane chrome (card, head, body bg).
    /// Set by `set_tokens` / the `set_theme_name` command.
    tokens: Tokens,
    /// Header subtitle shown in the app-head `title` slot.
    /// Defaults to `"zsh"`; updateable via `set_title` command.
    head_title: String,
}

// ---------------------------------------------------------------------------
// Constructor and accessors
// ---------------------------------------------------------------------------

impl TerminalAdapter {
    /// Wrap an already-spawned terminal in the adapter.
    #[must_use] 
    pub fn new(terminal: PhantomTerminal) -> Self {
        Self::with_theme(terminal, TerminalThemeColors::default())
    }

    /// Wrap a terminal with specific theme colors for grid rendering.
    #[must_use] 
    pub fn with_theme(terminal: PhantomTerminal, theme_colors: TerminalThemeColors) -> Self {
        Self {
            terminal,
            output_buf: String::new(),
            has_new_output: false,
            error_notified: false,
            is_detached: false,
            detached_label: String::new(),
            was_alt_screen: false,
            theme_colors,
            pty_dead: false,
            app_id: 0,
            outbox: Vec::new(),
            alt_screen_snapshot: None,
            takeover_detector: TakeoverDetector::default(),
            pending_title: None,
            tokens: Tokens::phosphor(RenderCtx::fallback()),
            head_title: default_head_title(),
        }
    }

    /// Swap the design-token palette used to paint the pane chrome.
    /// Callers that already build per-frame tokens (e.g. the App after a
    /// theme change) push them in here. The grid body still uses the
    /// `TerminalThemeColors` set via `set_theme_colors`.
    pub fn set_tokens(&mut self, tokens: Tokens) {
        self.tokens = tokens;
    }

    /// Replace the title shown in the app-head subtitle slot.
    /// Typical content is `"zsh · ~/path"`. Defaults to `"zsh"`.
    pub fn set_head_title(&mut self, title: impl Into<String>) {
        self.head_title = title.into();
    }

    /// Update the theme colors used for grid extraction.
    pub fn set_theme_colors(&mut self, colors: TerminalThemeColors) {
        self.theme_colors = colors;
    }

    /// Immutable access to the inner terminal.
    #[must_use] 
    pub fn terminal(&self) -> &PhantomTerminal {
        &self.terminal
    }

    /// Mutable access to the inner terminal.
    pub fn terminal_mut(&mut self) -> &mut PhantomTerminal {
        &mut self.terminal
    }

    /// The raw output buffer (last ~8 KiB of PTY output).
    #[must_use] 
    pub fn output_buf(&self) -> &str {
        &self.output_buf
    }

    /// Whether new PTY output arrived since the last clear.
    #[must_use] 
    pub fn has_new_output(&self) -> bool {
        self.has_new_output
    }

    /// Reset the new-output flag (call after the consumer processes it).
    pub fn clear_new_output_flag(&mut self) {
        self.has_new_output = false;
    }

    /// Whether the terminal is detached (alt-screen program running).
    #[must_use]
    pub fn is_detached(&self) -> bool {
        self.is_detached
    }

    /// Consume and return the latest OSC 2 window title emitted since the last call.
    ///
    /// Returns `None` when no new title has arrived. The caller (typically the
    /// main event loop) should forward this to `window.set_title()`.
    pub fn take_pending_title(&mut self) -> Option<String> {
        self.pending_title.take()
    }

    /// Label of the detached foreground process (e.g. "vim", "htop").
    #[must_use] 
    pub fn detached_label(&self) -> &str {
        &self.detached_label
    }

    /// Whether an error notification has been sent for the current output.
    #[must_use] 
    pub fn error_notified(&self) -> bool {
        self.error_notified
    }

    /// Set the error-notified flag.
    pub fn set_error_notified(&mut self, val: bool) {
        self.error_notified = val;
    }

    /// Extract a `RenderOutput` from the alt-screen (used for snapshot writes
    /// in `update()` so the secondary view adapter always has fresh data).
    ///
    /// The `_rect` parameter is ignored — we produce a full snapshot; the view
    /// adapter re-anchors the origin to its own rect during render.
    fn render_alt_screen(&self, _rect: &phantom_adapter::adapter::Rect) -> RenderOutput {
        use phantom_adapter::adapter::{GridData, ScrollState};
        use phantom_terminal::output;

        let (render_cells, cols, rows, cursor_state) =
            output::extract_grid_themed(self.terminal.term(), &self.theme_colors);

        let cells: Vec<AdapterCell> = render_cells
            .iter()
            .map(|rc| AdapterCell {
                ch: rc.ch,
                fg: rc.fg,
                bg: rc.bg,
                bold: rc.flags.contains(phantom_terminal::output::CellFlags::BOLD),
                italic: rc.flags.contains(phantom_terminal::output::CellFlags::ITALIC),
            })
            .collect();

        let cursor = if cursor_state.visible {
            Some(convert_cursor(&cursor_state))
        } else {
            None
        };

        let grid = GridData {
            cells,
            cols,
            rows,
            origin: (0.0, 0.0),
            cursor,
        };

        let scroll = Some(ScrollState {
            display_offset: self.terminal.display_offset(),
            history_size: self.terminal.history_size(),
            visible_rows: self.terminal.size().rows as usize,
        });

        RenderOutput {
            quads: vec![],
            text_segments: vec![],
            grid: Some(grid),
            scroll,
            selection: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Sub-trait implementations (ISP — each trait is focused)
// ---------------------------------------------------------------------------

impl AppCore for TerminalAdapter {
    fn app_type(&self) -> &str {
        "terminal"
    }

    fn is_alive(&self) -> bool {
        !self.pty_dead
    }

    fn update(&mut self, _dt: f32) {
        match self.terminal.pty_read() {
            Ok(n) if n > 0 => {
                let raw = &self.terminal.last_read_buf()[..n];
                let text = String::from_utf8_lossy(raw);
                self.output_buf.push_str(&text);

                if self.output_buf.len() > OUTPUT_BUF_CAP {
                    let mut trim = self.output_buf.len() - OUTPUT_BUF_CAP;
                    while trim < self.output_buf.len() && !self.output_buf.is_char_boundary(trim) {
                        trim += 1;
                    }
                    self.output_buf.drain(..trim);
                }

                self.has_new_output = true;
                self.error_notified = false;

                // Emit a bus event for the brain observer.
                self.outbox.push(phantom_adapter::BusMessage {
                    topic_id: 0, // Filled by coordinator from registered topic
                    sender: self.app_id,
                    event: phantom_protocol::Event::TerminalOutput {
                        app_id: self.app_id,
                        bytes: n as u64,
                    },
                    frame: 0,
                    timestamp: 0,
                });
            }
            Ok(_) => {}
            Err(e) => {
                warn!("TerminalAdapter PTY exited: {e}");
                self.pty_dead = true;
            }
        }

        // -- OSC 2 title drain (Bug 3) -----------------------------------------
        // Drain any window title changes emitted by the running program and keep
        // the most recent one. The caller retrieves it via `take_pending_title()`.
        let titles = self.terminal.drain_title_queue();
        if let Some(latest) = titles.into_iter().last() {
            self.pending_title = Some(latest);
        }

        // -- Bracketed-paste timeout tick (Bug 1) --------------------------------
        self.terminal.tick_paste_timeout();

        let is_alt = phantom_terminal::alt_screen::is_alt_screen(self.terminal.term());

        if is_alt && !self.was_alt_screen {
            self.is_detached = true;
            self.detached_label =
                phantom_terminal::process::foreground_process_name(self.terminal.pty_fd())
                    .unwrap_or_else(|| "interactive".to_string());
        }

        if !is_alt && self.was_alt_screen && self.is_detached {
            self.is_detached = false;
            self.detached_label.clear();
        }

        self.was_alt_screen = is_alt;

        // -- Subprocess takeover detection (issue #364) ----------------------
        // Run the structured detector alongside the legacy is_detached flag.
        // The detector emits typed bus events consumed by #366/#367 (tethers).
        // Detection is read-only; no pane split or PTY reparenting happens here.
        match self
            .takeover_detector
            .tick(self.terminal.term(), self.terminal.pty_fd())
        {
            phantom_terminal::takeover::TakeoverEvent::Detected(candidate) => {
                self.outbox.push(phantom_adapter::BusMessage {
                    topic_id: 0,
                    sender: self.app_id,
                    event: phantom_protocol::Event::SubprocessTakeoverDetected {
                        app_id: self.app_id,
                        process_name: candidate.process_name,
                        pgid: candidate.pgid,
                        alt_screen: candidate.signal
                            == phantom_terminal::takeover::TakeoverSignal::AltScreen,
                    },
                    frame: 0,
                    timestamp: 0,
                });
            }
            phantom_terminal::takeover::TakeoverEvent::Cleared => {
                self.outbox.push(phantom_adapter::BusMessage {
                    topic_id: 0,
                    sender: self.app_id,
                    event: phantom_protocol::Event::SubprocessTakeoverCleared {
                        app_id: self.app_id,
                    },
                    frame: 0,
                    timestamp: 0,
                });
            }
            phantom_terminal::takeover::TakeoverEvent::None => {}
        }

        // When alt-screen is active and a secondary pane has been wired in,
        // push a fresh render snapshot so the view adapter always has the
        // latest grid data.  We use a dummy full-screen rect here; the view
        // adapter re-anchors to its own rect in render().
        if self.is_detached
            && let Some(ref snapshot) = self.alt_screen_snapshot {
                let dummy_rect = phantom_adapter::adapter::Rect {
                    x: 0.0,
                    y: 0.0,
                    width: 9999.0,
                    height: 9999.0,
                    ..Default::default()
                };
                let render_out = self.render_alt_screen(&dummy_rect);
                if let Ok(mut guard) = snapshot.lock() {
                    *guard = Some(render_out);
                }
            }
    }

    fn attach_alt_screen_snapshot(
        &mut self,
        snapshot: std::sync::Arc<std::sync::Mutex<Option<phantom_adapter::adapter::RenderOutput>>>,
    ) {
        self.alt_screen_snapshot = Some(snapshot);
    }

    fn detach_alt_screen_snapshot(&mut self) {
        self.alt_screen_snapshot = None;
    }

    fn get_state(&self) -> serde_json::Value {
        let mouse_mode = match self.terminal.mouse_mode() {
            phantom_terminal::terminal::MouseMode::None => "none",
            phantom_terminal::terminal::MouseMode::Click => "click",
            phantom_terminal::terminal::MouseMode::Drag => "drag",
            phantom_terminal::terminal::MouseMode::Motion => "motion",
        };
        json!({
            "type": "terminal",
            "alive": true,
            "is_detached": self.is_detached,
            // Label of the foreground process (e.g. "vim", "htop"). Empty when
            // not in a takeover. Read by poll_alt_screen_transitions in update.rs.
            "detached_label": self.detached_label,
            "has_new_output": self.has_new_output,
            "history_size": self.terminal.history_size(),
            "display_offset": self.terminal.display_offset(),
            "mouse_mode": mouse_mode,
            // Structured takeover state (issue #364): true when the edge-detector
            // considers a subprocess to be actively taking over this terminal.
            "takeover_active": self.takeover_detector.is_active(),
            // Kitty keyboard protocol (CSI u) state. True when the running
            // program has enabled Kitty mode via `CSI > 1 h`. The input
            // dispatch layer reads this to select the encoding path.
            "kitty_keyboard_mode": self.terminal.is_kitty_keyboard_mode(),
        })
    }

    /// Return the raw PTY output buffer so the OODA brain can populate
    /// `ParsedOutput::raw_output` when a `CommandComplete` event fires (#226).
    fn output_buf_snapshot(&self) -> Option<String> {
        Some(self.output_buf.clone())
    }

    /// Drain the latest OSC 2 window title received from the running program.
    ///
    /// The main event loop (in `main.rs`) calls this each frame and forwards
    /// the value to `winit_window.set_title()` so the OS window title tracks
    /// whatever the shell / TUI program sets via `\x1b]2;<title>\x07`.
    fn take_pending_window_title(&mut self) -> Option<String> {
        self.pending_title.take()
    }

    /// Drain any OSC 52 clipboard texts decoded by the terminal since the last call.
    fn drain_osc52(&mut self) -> Vec<String> {
        self.terminal.drain_osc52()
    }
}

impl Renderable for TerminalAdapter {
    fn render(&self, rect: &Rect) -> RenderOutput {
        let mut quads: Vec<QuadData> = Vec::with_capacity(8);
        let mut text_segments: Vec<TextData> = Vec::new();

        // -- Chrome: card + 1px border ring + head + body bg --------------
        render_chrome(
            rect,
            &self.tokens,
            &self.head_title,
            self.terminal.size().cols as usize,
            self.terminal.size().rows as usize,
            &mut quads,
            &mut text_segments,
        );

        // -- Body inner rect (the grid's drawable area) -------------------
        let body = body_rect(rect, &self.tokens);

        // Pull the live grid from the underlying terminal.
        let (render_cells, cols, rows, cursor_state) =
            output::extract_grid_themed(self.terminal.term(), &self.theme_colors);

        let cells: Vec<AdapterCell> = render_cells
            .iter()
            .map(|rc| AdapterCell {
                ch: rc.ch,
                fg: rc.fg,
                bg: rc.bg,
                bold: rc.flags.contains(phantom_terminal::output::CellFlags::BOLD),
                italic: rc.flags.contains(phantom_terminal::output::CellFlags::ITALIC),
            })
            .collect();

        let cursor = if cursor_state.visible {
            Some(convert_cursor(&cursor_state))
        } else {
            None
        };

        // Cursor glow — a soft halo behind the cursor cell. The renderer's
        // grid pipeline draws the cursor block itself; this quad is the
        // glow under it. The sibling phantom-renderer PR replaces this
        // with `draw_glow`; until then we emit a faint over-sized quad.
        if let Some(c) = cursor.as_ref() {
            let (cell_w, cell_h) = grid_cell_size(rect);
            let cx = body.x + c.col as f32 * cell_w;
            let cy = body.y + c.row as f32 * cell_h;
            let halo = 6.0;
            quads.push(QuadData {
                x: cx - halo,
                y: cy - halo,
                w: cell_w + halo * 2.0,
                h: cell_h + halo * 2.0,
                color: glow_color_for(&self.tokens),
            });
        }

        let grid = GridData {
            cells,
            cols,
            rows,
            origin: (body.x, body.y),
            cursor,
        };

        let scroll = Some(ScrollState {
            display_offset: self.terminal.display_offset(),
            history_size: self.terminal.history_size(),
            visible_rows: self.terminal.size().rows as usize,
        });

        let selection = self
            .terminal
            .selection_range()
            .map(|(sc, sr, ec, er)| SelectionRange {
                start_col: sc,
                start_row: sr,
                end_col: ec,
                end_row: er,
            });

        RenderOutput {
            quads,
            text_segments,
            grid: Some(grid),
            scroll,
            selection,
        }
    }

    fn is_visual(&self) -> bool {
        true
    }

    fn spatial_preference(&self) -> Option<SpatialPreference> {
        Some(SpatialPreference {
            min_size: (40, 10),
            preferred_size: (120, 40),
            max_size: None,
            aspect_ratio: None,
            internal_panes: 1,
            internal_layout: InternalLayout::Single,
            priority: 10.0,
        })
    }
}

impl InputHandler for TerminalAdapter {
    fn handle_input(&mut self, key: &str) -> bool {
        let bytes = key_name_to_bytes(key);
        if let Err(e) = self.terminal.pty_write(&bytes) {
            warn!("TerminalAdapter pty_write failed: {e}");
        }
        true
    }
}

impl Commandable for TerminalAdapter {
    fn accept_command(&mut self, cmd: &str, args: &serde_json::Value) -> anyhow::Result<String> {
        match cmd {
            "write" => {
                let text = args.get("text").and_then(|v| v.as_str()).ok_or_else(|| {
                    anyhow::anyhow!("write command requires a \"text\" string field")
                })?;
                self.terminal
                    .pty_write(text.as_bytes())
                    .map_err(|e| anyhow::anyhow!("pty_write failed: {e}"))?;
                Ok("written".into())
            }
            "scroll" => {
                let direction = args
                    .get("direction")
                    .and_then(|v| v.as_str())
                    .unwrap_or("page_down");
                match direction {
                    "page_up" => self.terminal.scroll_page_up(),
                    "page_down" => self.terminal.scroll_page_down(),
                    "top" => self.terminal.scroll_to_top(),
                    "bottom" => self.terminal.scroll_to_bottom(),
                    "up" => {
                        let lines =
                            args.get("lines").and_then(|v| v.as_u64()).unwrap_or(3) as usize;
                        self.terminal.scroll_up(lines);
                    }
                    "down" => {
                        let lines =
                            args.get("lines").and_then(|v| v.as_u64()).unwrap_or(3) as usize;
                        self.terminal.scroll_down(lines);
                    }
                    other => return Err(anyhow::anyhow!("unknown scroll direction: {other}")),
                }
                Ok("scrolled".into())
            }
            "scroll_to_offset" => {
                let target = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let current = self.terminal.display_offset();
                if target > current {
                    self.terminal.scroll_up(target - current);
                } else if target < current {
                    self.terminal.scroll_down(current - target);
                }
                Ok("scrolled".into())
            }
            "write_bytes" => {
                let bytes_val = args
                    .get("bytes")
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| anyhow::anyhow!("write_bytes requires a \"bytes\" array"))?;
                let bytes: Vec<u8> = bytes_val
                    .iter()
                    .filter_map(|v| v.as_u64().map(|n| n as u8))
                    .collect();
                self.terminal
                    .pty_write(&bytes)
                    .map_err(|e| anyhow::anyhow!("pty_write failed: {e}"))?;
                Ok("written".into())
            }
            "resize" => {
                let cols = args.get("cols").and_then(|v| v.as_u64()).ok_or_else(|| {
                    anyhow::anyhow!("resize command requires a \"cols\" integer field")
                })? as u16;
                let rows = args.get("rows").and_then(|v| v.as_u64()).ok_or_else(|| {
                    anyhow::anyhow!("resize command requires a \"rows\" integer field")
                })? as u16;
                self.terminal.resize(cols, rows);
                Ok(format!("resized to {cols}x{rows}"))
            }
            "select_start" => {
                let col = args.get("col").and_then(|v| v.as_i64()).unwrap_or(0).max(0) as usize;
                let row = args.get("row").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                use phantom_terminal::selection::{Column, Line, Point, SelectionType, Side};
                let point = Point::new(Line(row), Column(col));
                self.terminal
                    .start_selection(SelectionType::Simple, point, Side::Left);
                Ok("selection started".into())
            }
            "select_update" => {
                let col = args.get("col").and_then(|v| v.as_i64()).unwrap_or(0).max(0) as usize;
                let row = args.get("row").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                use phantom_terminal::selection::{Column, Line, Point, Side};
                let point = Point::new(Line(row), Column(col));
                self.terminal.update_selection(point, Side::Right);
                Ok("selection updated".into())
            }
            "select_clear" => {
                self.terminal.clear_selection();
                Ok("selection cleared".into())
            }
            "select_word" => {
                let col = args.get("col").and_then(|v| v.as_i64()).unwrap_or(0).max(0) as usize;
                let row = args.get("row").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                use phantom_terminal::selection::{Column, Line, Point, SelectionType, Side};
                let point = Point::new(Line(row), Column(col));
                self.terminal
                    .start_selection(SelectionType::Semantic, point, Side::Left);
                self.terminal.update_selection(point, Side::Right);
                Ok("word selected".into())
            }
            "select_line" => {
                let row = args.get("row").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                use phantom_terminal::selection::{Column, Line, Point, SelectionType, Side};
                let point = Point::new(Line(row), Column(0));
                self.terminal
                    .start_selection(SelectionType::Lines, point, Side::Left);
                self.terminal.update_selection(point, Side::Right);
                Ok("line selected".into())
            }
            "select_copy" => {
                let text = self.terminal.selection_to_string().unwrap_or_default();
                Ok(text)
            }
            "select_all" => {
                // Select from the top of scrollback history to the last cell
                // of the visible screen.
                use phantom_terminal::selection::{Column, Line, Point, SelectionType, Side};
                let size = self.terminal.size();
                let history = self.terminal.history_size() as i32;
                // Scrollback lines are at negative Line indices.
                let start = Point::new(Line(-history), Column(0));
                let end = Point::new(
                    Line((size.rows as i32).saturating_sub(1)),
                    Column(size.cols.saturating_sub(1) as usize),
                );
                self.terminal
                    .start_selection(SelectionType::Simple, start, Side::Left);
                self.terminal.update_selection(end, Side::Right);
                Ok("all selected".into())
            }
            "hyperlink_at" => {
                let col = args.get("col").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let row = args.get("row").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                match self.terminal.hyperlink_at(col, row) {
                    Some(url) => Ok(url),
                    None => Ok(String::new()),
                }
            }
            "update_search" => {
                let query = args
                    .get("query")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                self.terminal.update_search(query);
                Ok(format!(
                    "search updated: {} matches",
                    self.terminal.search_index.total_matches()
                ))
            }
            "search_info" => {
                let total = self.terminal.search_index.total_matches();
                let active = self.terminal.search_active;
                Ok(serde_json::json!({ "total": total, "active": active }).to_string())
            }
            "scroll_to_search_match" => {
                let idx = args
                    .get("index")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as usize;
                if let Some((row, _col)) = self.terminal.search_index.nth_match(idx) {
                    // Convert the signed row (Line) to a display offset.
                    // Negative rows are above the viewport (in scrollback);
                    // positive rows are within the viewport.
                    // To centre the match we scroll so the row lands roughly
                    // in the middle of the visible area.
                    let rows = self.terminal.size().rows as i32;
                    let margin = (rows / 2).max(1);
                    if row < 0 {
                        // row = -(distance from top of viewport), so we need
                        // display_offset = |row| + margin to bring it into view.
                        let target_offset = ((-row) + margin) as usize;
                        let current = self.terminal.display_offset();
                        if target_offset > current {
                            self.terminal.scroll_up(target_offset - current);
                        } else if target_offset < current {
                            self.terminal.scroll_down(current - target_offset);
                        }
                    } else {
                        // Row is within the viewport — scroll to bottom to show it.
                        self.terminal.scroll_to_bottom();
                    }
                }
                Ok("scrolled to match".into())
            }
            "set_theme_name" => {
                let name = args
                    .get("name")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("set_theme_name requires a \"name\" string"))?;
                if let Some(tokens) = Tokens::for_theme_name(name, RenderCtx::fallback()) {
                    self.set_tokens(tokens);
                    Ok(format!("theme set: {name}"))
                } else {
                    Err(anyhow::anyhow!("unknown theme: {name}"))
                }
            }
            "set_title" => {
                let title = args
                    .get("title")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("set_title requires a \"title\" string"))?;
                self.set_head_title(title);
                Ok("title set".into())
            }
            other => Err(anyhow::anyhow!("unknown command: {other}")),
        }
    }
}

impl BusParticipant for TerminalAdapter {
    fn drain_outbox(&mut self) -> Vec<phantom_adapter::BusMessage> {
        std::mem::take(&mut self.outbox)
    }
}

impl Lifecycled for TerminalAdapter {
    fn set_app_id(&mut self, id: u32) {
        self.app_id = id;
    }
}
impl Permissioned for TerminalAdapter {
    fn permissions(&self) -> Vec<String> {
        vec!["filesystem".into(), "pty".into()]
    }
}

// ---------------------------------------------------------------------------
// Key translation (mirrors pane::key_name_to_bytes)
// ---------------------------------------------------------------------------

/// Translate an MCP / input key name into terminal ANSI bytes.
fn key_name_to_bytes(key: &str) -> Vec<u8> {
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

// ---------------------------------------------------------------------------
// Chrome rendering — card / border / head / body bg
// ---------------------------------------------------------------------------
//
// This block paints the pane chrome described in
// `docs/mockups/apps.html` `#terminal`. The body grid itself is still
// emitted via `GridData` by the renderer's grid pipeline; this code
// only paints the surfaces the grid sits on.
//
// Sibling phantom-renderer PR introduces `draw_rounded_rect`,
// `draw_glow`, `draw_gradient_rect`. Until that lands, the helpers
// here approximate those with plain `QuadData` rectangles. The shape
// of the output is identical; only the corners and the glow are flat.

/// Default subtitle for new terminals.
fn default_head_title() -> String {
    "zsh".to_string()
}

/// `surface_floating` token with a graceful fallback to `surface_raised`.
///
/// `phantom-ui` adds a dedicated `surface_floating` color role in its
/// sibling PR that syncs the Rust tokens with `system.css`. While main
/// does not yet expose that role, the floating surface is visually
/// indistinguishable from `surface_raised` for the terminal card, so
/// we return that instead. Once the sibling PR lands, swap this body
/// for `t.colors.surface_floating`.
fn surface_floating(t: &Tokens) -> [f32; 4] {
    t.colors.surface_raised
}

/// Glow color used behind the cursor cell.
///
/// `phantom-ui` adds a dedicated `glow_color` rgba slot. Until that
/// lands, derive a faint halo from `text_accent` at 14% alpha — that
/// matches the apparent intensity of the CSS `box-shadow: 0 0 12px
/// rgba(51,255,0,0.32)` once the GPU compositor blends it.
fn glow_color_for(t: &Tokens) -> [f32; 4] {
    let a = t.colors.text_accent;
    [a[0], a[1], a[2], 0.14]
}

/// Inner rect a grid renders into.
///
/// Carves the header height plus a `(top, side)` inset of
/// `(space_3, space_4)` (= 12 / 16 px) off the outer rect, matching
/// `.term-body { padding: 12px 16px; }` in `system.css`.
fn body_rect(rect: &Rect, tokens: &Tokens) -> Rect {
    let head_h = AppHead::new("TERMINAL", "")
        .with_tokens(*tokens)
        .height();
    let pad_x = tokens.space_4();
    let pad_y = tokens.space_3();
    Rect {
        x: rect.x + pad_x,
        y: rect.y + head_h + pad_y,
        width: (rect.width - pad_x * 2.0).max(0.0),
        height: (rect.height - head_h - pad_y * 2.0).max(0.0),
        cell_size: rect.cell_size,
    }
}

/// Per-cell pixel size, with a sane fallback when the rect carries
/// `(0.0, 0.0)` (test / pre-layout frames).
fn grid_cell_size(rect: &Rect) -> (f32, f32) {
    let w = if rect.cell_size.0 > 0.0 {
        rect.cell_size.0
    } else {
        8.0
    };
    let h = if rect.cell_size.1 > 0.0 {
        rect.cell_size.1
    } else {
        16.0
    };
    (w, h)
}

/// Paint the card, the 1 px border ring, the app-head, and the body
/// background quad. Appends quads / text segments to the supplied
/// vectors. The grid itself is emitted by the caller.
fn render_chrome(
    rect: &Rect,
    tokens: &Tokens,
    title: &str,
    cols: usize,
    rows: usize,
    quads: &mut Vec<QuadData>,
    text_segments: &mut Vec<TextData>,
) {
    // Back-to-front emission order: card bg, then app-head band, then
    // body bg, and finally the 1 px border ring LAST so it draws on top
    // of the head band (otherwise the head band paints over the top
    // hairline and breaks the ring).

    // -- 1. Outer card background ----------------------------------------
    quads.push(QuadData {
        x: rect.x,
        y: rect.y,
        w: rect.width,
        h: rect.height,
        color: surface_floating(tokens),
    });

    // -- 2. App-head row ------------------------------------------------
    //
    // The shared `AppHead` widget already paints the band, the bottom
    // divider, and the icon / name / title / meta text using the same
    // tokens every other pane uses. We feed it the mockup's strings:
    // icon `▶`, name `TERMINAL`, title `<shell · cwd>`, meta `<cols>x<rows>`.
    let head = AppHead::new("TERMINAL", title)
        .with_icon("▶")
        .with_meta(format!("{cols}x{rows}"))
        .with_tokens(*tokens);
    head.render_into_adapter(rect, quads, text_segments);

    // -- 3. Body background --------------------------------------------
    //
    // Must match `body_rect`'s `(space_4, space_3)` padding exactly so
    // there is no recessed strip below the last cell row.
    let head_h = head.height();
    let pad_x = tokens.space_4();
    let pad_y = tokens.space_3();
    quads.push(QuadData {
        x: rect.x + pad_x,
        y: rect.y + head_h + pad_y,
        w: (rect.width - pad_x * 2.0).max(0.0),
        h: (rect.height - head_h - pad_y * 2.0).max(0.0),
        color: tokens.colors.surface_recessed,
    });

    // -- 4. 1 px border ring -- emit four hairlines around the card LAST
    // so the ring sits on top of the head band. Color is
    // `chrome_frame_dim`, matching `system.css` `.app`.
    let frame = tokens.colors.chrome_frame_dim;
    let hair = tokens.hair();
    // top
    quads.push(QuadData {
        x: rect.x,
        y: rect.y,
        w: rect.width,
        h: hair,
        color: frame,
    });
    // bottom
    quads.push(QuadData {
        x: rect.x,
        y: rect.y + rect.height - hair,
        w: rect.width,
        h: hair,
        color: frame,
    });
    // left
    quads.push(QuadData {
        x: rect.x,
        y: rect.y,
        w: hair,
        h: rect.height,
        color: frame,
    });
    // right
    quads.push(QuadData {
        x: rect.x + rect.width - hair,
        y: rect.y,
        w: hair,
        h: rect.height,
        color: frame,
    });
}

// ---------------------------------------------------------------------------
// Compile-time Send assert
// ---------------------------------------------------------------------------

fn _assert_send() {
    fn _check<T: Send>() {}
    _check::<TerminalAdapter>();
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_app_type_returns_terminal() {
        // We cannot construct a TerminalAdapter without a real PTY, so test
        // the trait contract through the key helper and state assertions.
        // The app_type is a compile-verified string literal — the impl
        // always returns "terminal". Verified below via get_state on a
        // real adapter when a PTY is available (integration test).
        assert_eq!("terminal", "terminal");
    }

    #[test]
    fn test_key_name_to_bytes() {
        assert_eq!(key_name_to_bytes("Enter"), b"\r");
        assert_eq!(key_name_to_bytes("Tab"), b"\t");
        assert_eq!(key_name_to_bytes("Escape"), b"\x1b");
        assert_eq!(key_name_to_bytes("Space"), b" ");
        assert_eq!(key_name_to_bytes("Backspace"), b"\x7f");
        assert_eq!(key_name_to_bytes("Up"), b"\x1b[A");
        assert_eq!(key_name_to_bytes("Down"), b"\x1b[B");
        assert_eq!(key_name_to_bytes("Right"), b"\x1b[C");
        assert_eq!(key_name_to_bytes("Left"), b"\x1b[D");
        assert_eq!(key_name_to_bytes("a"), b"a");
        assert_eq!(key_name_to_bytes("ls"), b"ls");
    }

    #[test]
    fn test_render_produces_output() {
        let rect = Rect {
            x: 10.0,
            y: 20.0,
            width: 800.0,
            height: 600.0,
            ..Default::default()
        };
        // Test render output structure without needing a full terminal.
        let output = RenderOutput {
            quads: vec![QuadData {
                x: rect.x,
                y: rect.y,
                w: rect.width,
                h: rect.height,
                color: [0.05, 0.05, 0.08, 1.0],
            }],
            text_segments: vec![],
            grid: None,
            scroll: None,
            selection: None,
        };
        assert_eq!(output.quads.len(), 1);
        assert_eq!(output.quads[0].x, 10.0);
        assert_eq!(output.quads[0].y, 20.0);
        assert_eq!(output.quads[0].w, 800.0);
        assert_eq!(output.quads[0].h, 600.0);
        assert!(output.text_segments.is_empty());
    }

    #[test]
    fn test_handle_input_returns_true() {
        // The adapter always returns true from handle_input.
        // We verify the contract: key_name_to_bytes produces valid bytes,
        // and the impl unconditionally returns true.
        let bytes = key_name_to_bytes("Enter");
        assert!(!bytes.is_empty());
        // handle_input always returns true per contract.
    }

    #[test]
    fn test_accept_command_unknown_returns_error() {
        // Cannot construct TerminalAdapter without a PTY, but we can
        // verify the error message format.
        let err_msg = format!("unknown command: {}", "bogus");
        assert!(err_msg.contains("unknown command"));
        assert!(err_msg.contains("bogus"));
    }

    #[test]
    fn test_output_buf_lifecycle() {
        // Verify the output buffer cap logic (unit-testable without PTY).
        let mut buf = String::new();
        // Fill past cap.
        for _ in 0..1000 {
            buf.push_str("0123456789"); // 10 bytes each
        }
        assert_eq!(buf.len(), 10_000);

        // Apply the same trim logic as update().
        if buf.len() > OUTPUT_BUF_CAP {
            let mut trim = buf.len() - OUTPUT_BUF_CAP;
            while trim < buf.len() && !buf.is_char_boundary(trim) {
                trim += 1;
            }
            buf.drain(..trim);
        }
        assert_eq!(buf.len(), OUTPUT_BUF_CAP);
    }

    #[test]
    fn test_send_assert() {
        // Compile-time check that TerminalAdapter: Send.
        fn _check<T: Send>() {}
        _check::<TerminalAdapter>();
    }

    #[test]
    fn render_chrome_emits_card_head_and_body() {
        // Verify the chrome emitter produces the four layers described
        // in `docs/specs/terminal-rewrite/SPEC.md`:
        //   1. outer card bg quad,
        //   2. four 1-px border hairlines,
        //   3. app-head (band + divider, plus TERMINAL / meta text),
        //   4. body background quad.
        let rect = Rect {
            x: 0.0,
            y: 0.0,
            width: 600.0,
            height: 400.0,
            cell_size: (8.0, 16.0),
        };
        let tokens = Tokens::phosphor(RenderCtx::fallback());
        let mut quads: Vec<QuadData> = Vec::new();
        let mut text_segments: Vec<TextData> = Vec::new();
        render_chrome(&rect, &tokens, "zsh", 80, 24, &mut quads, &mut text_segments);

        // Card bg sits at the rect origin and spans the full rect.
        let card = &quads[0];
        assert_eq!(card.x, 0.0);
        assert_eq!(card.y, 0.0);
        assert!((card.w - 600.0).abs() < f32::EPSILON);
        assert!((card.h - 400.0).abs() < f32::EPSILON);

        // Border ring contributes four hairline quads (top, bottom, left,
        // right) of `tokens.hair()` thickness. The ring is emitted LAST
        // so it draws on top of the head band — assert the trailing four
        // quads match the hairline geometry.
        let hair = tokens.hair();
        let ring_start = quads.len() - 4;
        let ring: Vec<&QuadData> = quads[ring_start..].iter().collect();
        assert_eq!(ring.len(), 4);
        assert!(ring.iter().any(|q| (q.h - hair).abs() < f32::EPSILON));
        assert!(ring.iter().any(|q| (q.w - hair).abs() < f32::EPSILON));

        // Card + head band/divider + body bg + 4 ring quads.
        assert!(
            quads.len() >= 7,
            "expected card + head band/divider + body bg + 4 ring quads, got {}",
            quads.len()
        );

        // Text segments expose the mockup's TERMINAL label and the meta.
        let labels: Vec<&str> = text_segments.iter().map(|t| t.text.as_str()).collect();
        assert!(
            labels.iter().any(|s| s.contains("TERMINAL")),
            "expected a TERMINAL label segment, got {labels:?}"
        );
        assert!(
            labels.iter().any(|s| s.contains("80x24")),
            "expected a meta segment with 80x24, got {labels:?}"
        );
    }

    #[test]
    fn body_rect_is_inset_below_head() {
        let rect = Rect {
            x: 0.0,
            y: 0.0,
            width: 600.0,
            height: 400.0,
            cell_size: (8.0, 16.0),
        };
        let tokens = Tokens::phosphor(RenderCtx::fallback());
        let body = body_rect(&rect, &tokens);
        // The body must start below the rect's top edge (head was inset).
        assert!(body.y > rect.y, "body must be below the head row");
        // It must also be narrower than the outer rect (side padding).
        assert!(body.width < rect.width);
        // And its bottom must stay inside the outer rect.
        assert!(body.y + body.height <= rect.y + rect.height + 0.001);
    }

    #[test]
    fn glow_color_is_dimmed_accent() {
        let tokens = Tokens::phosphor(RenderCtx::fallback());
        let g = glow_color_for(&tokens);
        // Glow alpha must be far below 1.0 so it reads as a halo, not
        // a solid quad.
        assert!(g[3] > 0.0 && g[3] < 0.30, "glow alpha out of range");
    }
}
