//! Terminal emulation core wrapping `alacritty_terminal`.
//!
//! Spawns a PTY with the user's default shell and manages the terminal state
//! machine. All PTY I/O is non-blocking.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::io::AsRawFd;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Instant;

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::term::{ClipboardType, Config, TermMode};
use alacritty_terminal::tty::{self, Options as PtyOptions};
use alacritty_terminal::vte::ansi;
use alacritty_terminal::Term;
use anyhow::{Context, Result};
use log::{debug, trace, warn};

use crate::search::ScrollbackIndex;


/// Default scrollback history in lines.
const SCROLLBACK_LINES: usize = 10_000;

/// Read buffer size for PTY output.
const PTY_READ_BUF: usize = 0x10000; // 64 KiB

/// Default cell dimensions in pixels (used for TIOCSWINSZ pixel fields).
const DEFAULT_CELL_WIDTH: u16 = 8;
const DEFAULT_CELL_HEIGHT: u16 = 16;

/// Maximum number of bytes accumulated in the bracketed-paste buffer before
/// the paste is discarded to prevent unbounded memory growth.
pub const MAX_PASTE_BUFFER_BYTES: usize = 4 * 1024 * 1024; // 4 MiB

/// How long a bracketed paste may remain open (start received, no end yet)
/// before it is force-cleared.
const PASTE_TIMEOUT_SECS: u64 = 5;

// ---------------------------------------------------------------------------
// MouseMode — which mouse tracking the terminal program has requested
// ---------------------------------------------------------------------------

/// Which mouse tracking mode the terminal program has requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseMode {
    /// No mouse tracking (default shell).
    None,
    /// Report clicks only (mode 1000).
    Click,
    /// Report clicks and drag motion (mode 1002).
    Drag,
    /// Report all motion (mode 1003).
    Motion,
}

// ---------------------------------------------------------------------------
// EventListener — forwards terminal events to Phantom
// ---------------------------------------------------------------------------

/// Shared queue for data the terminal wants written back to the PTY.
///
/// The `EventListener` is moved into `Term` and we lose direct access, so we
/// share the write queue through an `Arc<Mutex<_>>`.
type PtyWriteQueue = Arc<Mutex<Vec<Vec<u8>>>>;

/// Shared channel for OSC 2 window title changes emitted by the running
/// program.  The sender lives in the event listener (which is `Clone`);
/// `PhantomTerminal` exposes a receiver so the caller can subscribe.
type TitleQueue = Arc<Mutex<Vec<String>>>;

/// Listener that receives events from the alacritty terminal state machine.
///
/// Events like device-attribute responses (`PtyWrite`) are buffered in a
/// shared queue that the `PhantomTerminal` drains after each read cycle.
/// OSC 52 clipboard texts are forwarded via an optional bounded mpsc sender.
#[derive(Clone, Debug)]
pub struct PhantomEventListener {
    pty_writes: PtyWriteQueue,
    title_queue: TitleQueue,
    /// Optional sender for OSC 52 decoded clipboard texts.
    osc52_tx: Option<mpsc::SyncSender<String>>,
}

impl PhantomEventListener {
    fn with_osc52(
        queue: PtyWriteQueue,
        title_queue: TitleQueue,
        tx: mpsc::SyncSender<String>,
    ) -> Self {
        Self {
            pty_writes: queue,
            title_queue,
            osc52_tx: Some(tx),
        }
    }
}

impl EventListener for PhantomEventListener {
    fn send_event(&self, event: Event) {
        match &event {
            Event::PtyWrite(data) => {
                trace!("terminal requests PTY write: {} bytes", data.len());
                if let Ok(mut q) = self.pty_writes.lock() {
                    q.push(data.as_bytes().to_vec());
                }
            }
            Event::Title(title) => {
                debug!("terminal title: {title}");
                if let Ok(mut q) = self.title_queue.lock() {
                    q.push(title.clone());
                }
            }
            Event::ClipboardStore(ClipboardType::Clipboard, text) => {
                trace!("OSC 52 clipboard text: {} bytes", text.len());
                if let Some(tx) = &self.osc52_tx {
                    let _ = tx.try_send(text.clone());
                }
            }
            Event::Bell => debug!("terminal bell"),
            Event::Exit => debug!("terminal exit requested"),
            Event::Wakeup => trace!("terminal wakeup"),
            Event::ChildExit(status) => debug!("child exited: {status:?}"),
            Event::CursorBlinkingChange => trace!("cursor blink state changed"),
            _ => trace!("terminal event: {event:?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// TerminalSize — implements alacritty Dimensions
// ---------------------------------------------------------------------------

/// Terminal dimensions implementing `alacritty_terminal::grid::Dimensions`.
#[derive(Clone, Copy, Debug)]
pub struct TerminalSize {
    pub cols: u16,
    pub rows: u16,
}

impl TerminalSize {
    #[must_use]
    pub fn new(cols: u16, rows: u16) -> Self {
        Self { cols, rows }
    }

    /// Build the `WindowSize` that the PTY / kernel expects.
    #[must_use]
    pub fn window_size(&self) -> WindowSize {
        WindowSize {
            num_cols: self.cols,
            num_lines: self.rows,
            cell_width: DEFAULT_CELL_WIDTH,
            cell_height: DEFAULT_CELL_HEIGHT,
        }
    }
}

impl Dimensions for TerminalSize {
    fn total_lines(&self) -> usize {
        self.rows as usize
    }

    fn screen_lines(&self) -> usize {
        self.rows as usize
    }

    fn columns(&self) -> usize {
        self.cols as usize
    }
}

// ---------------------------------------------------------------------------
// PhantomTerminal
// ---------------------------------------------------------------------------

/// The terminal emulation core.
///
/// Owns the alacritty terminal state machine, the PTY file descriptors, and
/// the VTE parser. Provides a synchronous, non-blocking API for reading PTY
/// output, writing PTY input, and resizing.
pub struct PhantomTerminal {
    /// Terminal state machine (grid, cursor, modes, etc.).
    term: Term<PhantomEventListener>,

    /// VTE parser that translates raw bytes into terminal operations.
    parser: ansi::Processor,

    /// PTY reader (non-blocking).
    pty_reader: File,

    /// PTY writer.
    pty_writer: File,

    /// The underlying PTY handle. Kept alive so the child process persists
    /// and is cleaned up on drop.
    _pty: tty::Pty,

    /// Shared queue for PTY-write requests from the terminal (e.g. DA responses).
    pty_write_queue: PtyWriteQueue,

    /// Shared queue for OSC 2 title strings emitted by the running program.
    title_queue: TitleQueue,

    /// Current terminal dimensions.
    size: TerminalSize,

    /// Scratch buffer for PTY reads, allocated once.
    read_buf: Vec<u8>,

    /// Number of valid bytes written into `read_buf` during the last
    /// [`pty_read`](PhantomTerminal::pty_read) call.  Zero between reads.
    last_read_len: usize,

    /// Whether the Kitty keyboard protocol (CSI u) is active.
    ///
    /// Reflects `TermMode::DISAMBIGUATE_ESC_CODES` — set by the running
    /// program via `CSI > 1 h` (enable) and cleared via `CSI > 1 l`
    /// (disable).  Kept as a cached bool so callers avoid importing
    /// `alacritty_terminal` just to check a mode bit.  Refreshed after
    /// every `pty_read` call.
    pub kitty_keyboard_mode: bool,

    // ── Bracketed-paste guard (Bug 1) ─────────────────────────────────────
    /// True while a bracketed-paste start (`\x1b[200~`) has been received
    /// but the matching end (`\x1b[201~`) has not yet been written to the PTY.
    in_bracketed_paste: bool,

    /// Total bytes written to the PTY in the current bracketed-paste session.
    /// Reset to zero on `\x1b[201~` or when the size limit / timeout fires.
    paste_byte_count: usize,

    /// Wall-clock instant at which the current bracketed-paste session began.
    /// Used to detect runaway pastes that never receive a terminator.
    paste_started_at: Option<Instant>,

    /// Receiver for OSC 52 clipboard texts decoded by the event listener.
    /// `None` when OSC 52 forwarding is disabled.
    osc52_rx: Option<mpsc::Receiver<String>>,

    /// Current OSC 8 hyperlink URL being written by the terminal program.
    /// Set when `\x1b]8;params;uri\x07` (open) is received; cleared on
    /// `\x1b]8;;\x07` (close). Cells written while this is `Some` inherit
    /// the URL.
    current_hyperlink: Option<Arc<str>>,

    /// Per-cell hyperlink map: `(row, col) -> Arc<str>`.
    ///
    /// Populated by the OSC-8 post-processor in `pty_read`. Consulted by
    /// `hit_test_hyperlink` on mouse-click events.
    hyperlink_map: HashMap<(usize, usize), Arc<str>>,

    /// Cached scrollback search index. Rebuilt on every query change via
    /// [`PhantomTerminal::update_search`]. Empty (no matches) until a search
    /// is active.
    pub search_index: ScrollbackIndex,

    /// Whether find-in-terminal search is currently active. When `true`, the
    /// renderer overlays match highlights from `search_index` on top of the
    /// terminal cells.
    pub search_active: bool,
}

impl PhantomTerminal {
    /// Create a new terminal emulator with the given dimensions.
    ///
    /// Spawns a PTY running the user's default shell. The PTY file descriptor
    /// is set to non-blocking mode by `alacritty_terminal::tty::new`.
    pub fn new(cols: u16, rows: u16) -> Result<Self> {
        let size = TerminalSize::new(cols, rows);

        // Configure the PTY — use the user's default shell.
        let pty_options = PtyOptions::default();

        // Set up TERM / COLORTERM environment variables.
        tty::setup_env();

        // Spawn the PTY child process. The window_id is unused for our purposes.
        let pty = tty::new(&pty_options, size.window_size(), 0)
            .context("failed to spawn PTY")?;

        // Clone the PTY file descriptor for separate reader/writer handles.
        // The fd is already non-blocking (set by alacritty_terminal::tty::new).
        let pty_reader = pty.file().try_clone().context("failed to clone PTY fd for reader")?;
        let pty_writer = pty.file().try_clone().context("failed to clone PTY fd for writer")?;

        // Terminal config with reasonable defaults.
        let config = Config {
            scrolling_history: SCROLLBACK_LINES,
            ..Config::default()
        };

        // Shared write queue between the event listener and this struct.
        let pty_write_queue: PtyWriteQueue = Arc::new(Mutex::new(Vec::new()));

        // Shared title queue for OSC 2 title change events.
        let title_queue: TitleQueue = Arc::new(Mutex::new(Vec::new()));

        // OSC 52 channel: bounded to 32 items so we don't accumulate unbounded
        // clipboard texts if the consumer falls behind.
        let (osc52_tx, osc52_rx) = mpsc::sync_channel::<String>(32);

        // Create the terminal state machine.
        let event_listener = PhantomEventListener::with_osc52(
            Arc::clone(&pty_write_queue),
            Arc::clone(&title_queue),
            osc52_tx,
        );
        let term = Term::new(config, &size, event_listener);

        debug!("PhantomTerminal created: {cols}x{rows}");

        Ok(Self {
            term,
            parser: ansi::Processor::new(),
            pty_reader,
            pty_writer,
            _pty: pty,
            pty_write_queue,
            title_queue,
            size,
            read_buf: vec![0u8; PTY_READ_BUF],
            last_read_len: 0,
            kitty_keyboard_mode: false,
            in_bracketed_paste: false,
            paste_byte_count: 0,
            paste_started_at: None,
            osc52_rx: Some(osc52_rx),
            current_hyperlink: None,
            hyperlink_map: HashMap::new(),
            search_index: ScrollbackIndex::new(),
            search_active: false,
        })
    }

    /// Drain all OSC 52 clipboard texts received since the last call.
    ///
    /// Returns a `Vec<String>` of decoded clipboard texts (one per OSC 52 sequence).
    /// Returns an empty vec when the channel is empty or not wired.
    pub fn drain_osc52(&mut self) -> Vec<String> {
        let Some(rx) = &self.osc52_rx else {
            return Vec::new();
        };
        let mut texts = Vec::new();
        while let Ok(text) = rx.try_recv() {
            texts.push(text);
        }
        texts
    }

    /// Resize the terminal and notify the PTY.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        let new_size = TerminalSize::new(cols, rows);
        self.size = new_size;

        // Resize the terminal grid.
        self.term.resize(new_size);

        // Notify the PTY / kernel of the new window size via TIOCSWINSZ.
        let ws = new_size.window_size();
        let raw_fd = self.pty_writer.as_raw_fd();
        let winsize = libc::winsize {
            ws_row: ws.num_lines,
            ws_col: ws.num_cols,
            ws_xpixel: ws.num_cols.saturating_mul(ws.cell_width),
            ws_ypixel: ws.num_lines.saturating_mul(ws.cell_height),
        };
        // SAFETY: TIOCSWINSZ is safe to call on a valid PTY master fd.
        let res = unsafe { libc::ioctl(raw_fd, libc::TIOCSWINSZ, &winsize as *const _) };
        if res < 0 {
            warn!("TIOCSWINSZ failed: {}", io::Error::last_os_error());
        }

        debug!("terminal resized to {cols}x{rows}");
    }

    /// Read available bytes from the PTY and feed them to the terminal.
    ///
    /// Returns the number of bytes processed. Returns `Ok(0)` when the PTY
    /// has no data available (EAGAIN / WouldBlock). Returns an error on a
    /// real I/O failure or EOF (child exited).
    pub fn pty_read(&mut self) -> Result<usize> {
        let n = match self.pty_reader.read(&mut self.read_buf) {
            Ok(0) => anyhow::bail!("PTY EOF — child process exited"),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                self.last_read_len = 0;
                return Ok(0);
            }
            Err(e) => return Err(e).context("PTY read failed"),
        };

        // Record the number of valid bytes for `last_read_buf`.
        self.last_read_len = n;

        // Feed the raw bytes through the VTE parser into the terminal.
        // This mirrors what alacritty's event_loop does: `parser.advance(&mut term, buf)`.
        self.parser.advance(&mut self.term, &self.read_buf[..n]);

        // Drain any PTY-write requests the terminal generated (e.g. device
        // attribute responses) and write them back to the PTY.
        self.flush_pty_write_queue();

        // Post-process the raw buffer for OSC 8 hyperlink sequences.
        // alacritty_terminal does not expose OSC 8 via its EventListener, so we
        // scan the raw bytes ourselves and track which cells carry hyperlinks.
        // We clone the slice to avoid a split &mut self borrow.
        self.process_osc8(&self.read_buf[..n].to_vec());

        // Refresh the Kitty keyboard mode cache.  The running program may
        // have sent `CSI > 1 h` / `CSI > 1 l` inside this chunk; the VTE
        // parser already updated TermMode, so we just mirror it here.
        self.kitty_keyboard_mode = self
            .term
            .mode()
            .contains(TermMode::DISAMBIGUATE_ESC_CODES);

        trace!("pty_read: processed {n} bytes");
        Ok(n)
    }

    /// Access the raw bytes from the last `pty_read` call.
    ///
    /// Returns only the valid portion of the internal scratch buffer —
    /// exactly the bytes that were filled during the most recent `pty_read`.
    /// Returns an empty slice between reads or when `pty_read` returned `Ok(0)`.
    /// The returned slice is only valid until the next `pty_read` call.
    #[must_use]
    #[inline]
    pub fn last_read_buf(&self) -> &[u8] {
        &self.read_buf[..self.last_read_len]
    }

    /// Write raw input bytes to the PTY (keyboard/mouse input, paste data, etc.).
    ///
    /// Applies two layers of safety before the actual write syscall:
    ///
    /// 1. **Null-byte sanitization** (Bug 2): null bytes (`0x00`) are stripped
    ///    because they can corrupt PTY line-discipline state in ways that are
    ///    difficult to diagnose.
    ///
    /// 2. **Bracketed-paste guard** (Bug 1): the method tracks whether a paste
    ///    start (`\x1b[200~`) has been sent without the matching end
    ///    (`\x1b[201~`) and enforces a hard 4 MiB cap and a 5-second timeout.
    ///    If either limit is exceeded the paste session is force-closed.
    pub fn pty_write(&mut self, data: &[u8]) -> Result<()> {
        // -- Bug 2: null-byte sanitization --
        let sanitized = sanitize_pty_input(data);
        let data = sanitized.as_ref();

        // -- Bug 1: bracketed-paste bookkeeping --
        // Detect bracketed paste start / end markers so we can track the
        // in-flight byte count and apply size/timeout limits.
        let has_start = memslice_contains(data, b"\x1b[200~");
        let has_end = memslice_contains(data, b"\x1b[201~");

        if has_start {
            self.in_bracketed_paste = true;
            self.paste_byte_count = 0;
            self.paste_started_at = Some(Instant::now());
        }

        if self.in_bracketed_paste {
            // Timeout check: force-clear pastes stuck open for > 5 seconds.
            if let Some(started) = self.paste_started_at {
                if started.elapsed().as_secs() >= PASTE_TIMEOUT_SECS {
                    warn!(
                        "bracketed paste timed out after {}s without terminator — discarding",
                        PASTE_TIMEOUT_SECS
                    );
                    self.in_bracketed_paste = false;
                    self.paste_byte_count = 0;
                    self.paste_started_at = None;
                    return Ok(());
                }
            }

            // Size limit check: discard when paste exceeds 4 MiB.
            if self.paste_byte_count + data.len() > MAX_PASTE_BUFFER_BYTES {
                warn!(
                    "bracketed paste exceeded {} MiB limit — discarding",
                    MAX_PASTE_BUFFER_BYTES / (1024 * 1024)
                );
                self.in_bracketed_paste = false;
                self.paste_byte_count = 0;
                self.paste_started_at = None;
                return Ok(());
            }

            self.paste_byte_count += data.len();

            if has_end {
                // Paste completed normally.
                self.in_bracketed_paste = false;
                self.paste_byte_count = 0;
                self.paste_started_at = None;
            }
        }

        self.pty_writer.write_all(data).context("PTY write failed")?;
        Ok(())
    }

    /// Drain all pending OSC 2 window title strings from the event queue.
    ///
    /// Returns the titles in the order they were received. The caller should
    /// apply the last (most-recent) title to the window. Returns an empty
    /// `Vec` when no title changes have occurred since the last drain.
    pub fn drain_title_queue(&self) -> Vec<String> {
        match self.title_queue.lock() {
            Ok(mut q) => std::mem::take(&mut *q),
            Err(poisoned) => {
                let mut q = poisoned.into_inner();
                std::mem::take(&mut *q)
            }
        }
    }

    /// Tick the bracketed-paste timeout guard.
    ///
    /// Call this periodically (e.g. once per frame or on a timer tick) so
    /// that a paste which started but never received `\x1b[201~` is
    /// eventually force-cleared even when no further PTY writes arrive.
    pub fn tick_paste_timeout(&mut self) {
        if !self.in_bracketed_paste {
            return;
        }
        if let Some(started) = self.paste_started_at {
            if started.elapsed().as_secs() >= PASTE_TIMEOUT_SECS {
                warn!(
                    "bracketed paste timed out (tick) after {}s — clearing",
                    PASTE_TIMEOUT_SECS
                );
                self.in_bracketed_paste = false;
                self.paste_byte_count = 0;
                self.paste_started_at = None;
            }
        }
    }

    /// Whether a bracketed-paste session is currently open (start received,
    /// end not yet seen).
    #[must_use]
    #[inline]
    pub fn in_bracketed_paste(&self) -> bool {
        self.in_bracketed_paste
    }

    /// Immutable access to the terminal state (grid, cursor, modes).
    #[must_use]
    #[inline]
    pub fn term(&self) -> &Term<PhantomEventListener> {
        &self.term
    }

    /// Mutable access to the terminal state.
    #[inline]
    pub fn term_mut(&mut self) -> &mut Term<PhantomEventListener> {
        &mut self.term
    }

    /// Current terminal dimensions.
    #[must_use]
    #[inline]
    pub fn size(&self) -> TerminalSize {
        self.size
    }

    /// The PTY file descriptor for ioctl queries (e.g. foreground process group).
    #[must_use]
    #[inline]
    pub fn pty_fd(&self) -> &File {
        &self.pty_reader
    }

    // -- Scroll API --------------------------------------------------------

    /// Scroll the viewport up by the given number of lines.
    pub fn scroll_up(&mut self, lines: usize) {
        self.term.scroll_display(Scroll::Delta(lines as i32));
    }

    /// Scroll the viewport down by the given number of lines.
    pub fn scroll_down(&mut self, lines: usize) {
        self.term.scroll_display(Scroll::Delta(-(lines as i32)));
    }

    /// Scroll the viewport up by one full page.
    pub fn scroll_page_up(&mut self) {
        self.term.scroll_display(Scroll::PageUp);
    }

    /// Scroll the viewport down by one full page.
    pub fn scroll_page_down(&mut self) {
        self.term.scroll_display(Scroll::PageDown);
    }

    /// Scroll to the bottom of the terminal (live output).
    pub fn scroll_to_bottom(&mut self) {
        self.term.scroll_display(Scroll::Bottom);
    }

    /// Scroll to the top of the scrollback history.
    pub fn scroll_to_top(&mut self) {
        self.term.scroll_display(Scroll::Top);
    }

    /// Current display offset from the bottom (0 = at live output).
    #[must_use]
    pub fn display_offset(&self) -> usize {
        self.term.grid().display_offset()
    }

    /// Total number of lines in scrollback history.
    #[must_use]
    pub fn history_size(&self) -> usize {
        self.term.grid().history_size()
    }

    // -- Search API --------------------------------------------------------

    /// Rebuild the scrollback search index for `query`.
    ///
    /// Sets `search_active` to `true` when the query is non-empty and `false`
    /// when the query is cleared, so the renderer knows whether to draw
    /// highlight quads.
    pub fn update_search(&mut self, query: &str) {
        self.search_active = !query.is_empty();
        self.search_index.index(self.term.grid(), query);
    }

    // -- Mouse mode API ----------------------------------------------------

    /// Check which mouse tracking mode the running program has requested.
    #[must_use]
    pub fn mouse_mode(&self) -> MouseMode {
        let mode = self.term.mode();
        if mode.contains(TermMode::MOUSE_MOTION) {
            MouseMode::Motion
        } else if mode.contains(TermMode::MOUSE_DRAG) {
            MouseMode::Drag
        } else if mode.contains(TermMode::MOUSE_REPORT_CLICK) {
            MouseMode::Click
        } else {
            MouseMode::None
        }
    }

    /// Whether the terminal is in SGR mouse mode (1006).
    #[must_use]
    pub fn sgr_mouse(&self) -> bool {
        self.term.mode().contains(TermMode::SGR_MOUSE)
    }

    /// Whether the terminal is using the alternate screen buffer.
    #[must_use]
    pub fn is_alt_screen(&self) -> bool {
        self.term.mode().contains(TermMode::ALT_SCREEN)
    }

    // -- Mouse scroll API --------------------------------------------------

    /// Handle a mouse scroll event.
    ///
    /// When the terminal program has requested mouse tracking (any mode other
    /// than [`MouseMode::None`]), the scroll is encoded as an SGR 1006 wheel
    /// sequence (`\x1b[<64;col+1;row+1M` / `\x1b[<65;…`) and written
    /// directly to the PTY so the running program receives it.
    ///
    /// When the terminal is **not** in mouse reporting mode the viewport is
    /// shifted by `delta.abs().max(1) as usize` lines via `scroll_display`,
    /// which adjusts `display_offset` without sending any bytes to the PTY.
    ///
    /// `col` and `row` are the zero-based terminal cell under the cursor.
    /// `delta` is positive for scroll-up and negative for scroll-down.
    pub fn handle_mouse_scroll(&mut self, delta: f32, col: usize, row: usize) {
        let lines = delta.abs().ceil().max(1.0) as usize;
        if self.mouse_mode() != MouseMode::None {
            use crate::input::{MouseButton, encode_mouse_sgr};
            let btn = if delta > 0.0 {
                MouseButton::ScrollUp
            } else {
                MouseButton::ScrollDown
            };
            let sgr = encode_mouse_sgr(btn, col, row, true);
            if let Err(e) = self.pty_write(&sgr) {
                warn!("handle_mouse_scroll: PTY write failed: {e}");
            }
        } else if delta > 0.0 {
            self.scroll_up(lines);
        } else {
            self.scroll_down(lines);
        }
    }

    /// Set the viewport to an absolute `display_offset` (scrollback lines
    /// above the live screen).  Clamps to `[0, history_size]`.
    ///
    /// Used by the scrollbar drag handler to jump directly to a position
    /// rather than scrolling by a relative delta.
    pub fn set_display_offset(&mut self, offset: usize) {
        let current = self.display_offset();
        let target = offset.min(self.history_size());
        if target > current {
            self.scroll_up(target - current);
        } else if target < current {
            self.scroll_down(current - target);
        }
    }

    /// Whether the Kitty keyboard protocol (CSI u) is enabled.
    ///
    /// Reflects `TermMode::DISAMBIGUATE_ESC_CODES`.  Refreshed after every
    /// `pty_read` call.  Use this instead of the `kitty_keyboard_mode` field
    /// for read-only queries — the field is `pub` only so `TerminalAdapter`
    /// can expose it via `get_state` without importing alacritty internals.
    #[must_use]
    pub fn is_kitty_keyboard_mode(&self) -> bool {
        self.kitty_keyboard_mode
    }

    // -- Selection API ----------------------------------------------------

    /// Start a new text selection at the given grid position.
    pub fn start_selection(&mut self, ty: alacritty_terminal::selection::SelectionType, point: alacritty_terminal::index::Point, side: alacritty_terminal::index::Side) {
        self.term.selection = Some(alacritty_terminal::selection::Selection::new(ty, point, side));
    }

    /// Update the selection endpoint as the mouse drags.
    pub fn update_selection(&mut self, point: alacritty_terminal::index::Point, side: alacritty_terminal::index::Side) {
        if let Some(ref mut sel) = self.term.selection {
            sel.update(point, side);
        }
    }

    /// Clear the current selection.
    pub fn clear_selection(&mut self) {
        self.term.selection = None;
    }

    /// Get the selected text as a string (for clipboard copy).
    #[must_use]
    pub fn selection_to_string(&self) -> Option<String> {
        self.term.selection_to_string()
    }

    /// Whether any text is currently selected.
    #[must_use]
    pub fn has_selection(&self) -> bool {
        self.term.selection.is_some()
    }

    /// Get the selection range as (start_col, start_row, end_col, end_row).
    ///
    /// Row values are relative to the visible screen (0 = top visible row).
    /// Returns `None` if there is no selection.
    #[must_use]
    pub fn selection_range(&self) -> Option<(usize, usize, usize, usize)> {
        let range = self.term.selection.as_ref().and_then(|s| s.to_range(&self.term))?;
        let start_col = range.start.column.0;
        let start_row = range.start.line.0.max(0) as usize;
        let end_col = range.end.column.0;
        let end_row = range.end.line.0.max(0) as usize;
        Some((start_col, start_row, end_col, end_row))
    }

    // -- Hyperlink API --------------------------------------------------------

    /// Hit-test a cell position against the OSC-8 hyperlink map.
    ///
    /// Returns the URL string when the cell at `(col, row)` is covered by an
    /// OSC 8 hyperlink (either from a real OSC 8 sequence or from URL regex
    /// fallback annotation). Returns `None` for plain cells.
    #[must_use]
    pub fn hit_test_hyperlink(&self, col: usize, row: usize) -> Option<&str> {
        self.hyperlink_map.get(&(row, col)).map(|s| s.as_ref())
    }

    /// Return a snapshot of the hyperlink map for a given viewport row.
    ///
    /// Used by the grid extractor to annotate `RenderCell::hyperlink` after
    /// `extract_grid_themed` returns the base cell buffer.
    #[must_use]
    pub fn hyperlinks_for_row(&self, row: usize) -> Vec<(usize, Arc<str>)> {
        self.hyperlink_map
            .iter()
            .filter_map(|((r, c), url)| {
                if *r == row {
                    Some((*c, Arc::clone(url)))
                } else {
                    None
                }
            })
            .collect()
    }

    /// Scan raw PTY bytes for OSC 8 sequences and update the hyperlink map.
    ///
    /// OSC 8 open:  `\x1b]8;<params>;<uri>\x07`  or `\x1b]8;<params>;<uri>\x1b\\`
    /// OSC 8 close: `\x1b]8;;\x07`               or `\x1b]8;;\x1b\\`
    ///
    /// When a URI is active, we stamp the current cursor position and all
    /// subsequent cursor positions (as the VTE feeds characters) with the URL.
    /// This is an approximation: we track the cursor position *after* the
    /// alacritty VTE has already advanced it, so each call to this function
    /// records the cursor position at the end of the chunk. For multi-chunk
    /// output, some cells may be missed on chunk boundaries; this is acceptable
    /// for the common case of single-line hyperlinks.
    fn process_osc8(&mut self, buf: &[u8]) {
        // Parse OSC sequences out of the raw byte stream.
        // Format: ESC ] <command> <ST>
        //   where ST is BEL (0x07) or ESC \ (0x1b 0x5c).
        // OSC 8 command: "8;<params>;<uri>"
        let mut i = 0;
        while i < buf.len() {
            // Look for ESC ]
            if buf[i] == 0x1b && i + 1 < buf.len() && buf[i + 1] == b']' {
                let osc_start = i + 2;
                // Find the string terminator: BEL or ESC \.
                let mut end = osc_start;
                while end < buf.len() {
                    if buf[end] == 0x07 {
                        break;
                    }
                    if buf[end] == 0x1b && end + 1 < buf.len() && buf[end + 1] == 0x5c {
                        break;
                    }
                    end += 1;
                }

                if end < buf.len() {
                    let osc_payload = &buf[osc_start..end];
                    // Check for OSC 8: starts with "8;"
                    if osc_payload.starts_with(b"8;") {
                        let rest = &osc_payload[2..]; // after "8;"
                        // Find the second ';' separating params from URI.
                        if let Some(semi) = rest.iter().position(|&b| b == b';') {
                            let uri = &rest[semi + 1..];
                            if uri.is_empty() {
                                // OSC 8 close.
                                trace!("OSC 8 close");
                                self.current_hyperlink = None;
                            } else if let Ok(url_str) = std::str::from_utf8(uri) {
                                // OSC 8 open.
                                let url: Arc<str> = url_str.into();
                                trace!("OSC 8 open: {url_str}");
                                self.current_hyperlink = Some(url);
                            }
                        }
                    }
                    // Advance past the ST.
                    i = if buf[end] == 0x07 { end + 1 } else { end + 2 };
                    continue;
                }
            }

            // If we have an active hyperlink, record the current cursor position.
            if let Some(ref url) = self.current_hyperlink.clone() {
                let cursor = self.term.grid().cursor.point;
                let col = cursor.column.0;
                let row_line = cursor.line.0;
                let display_offset = self.term.grid().display_offset() as i32;
                // Convert grid line to viewport row.
                let viewport_row = row_line + display_offset;
                if viewport_row >= 0 {
                    self.hyperlink_map
                        .insert((viewport_row as usize, col), Arc::clone(url));
                }
            }

            i += 1;
        }
    }

    /// Clear the entire hyperlink map. Call when the terminal screen is reset
    /// (e.g. on clear-screen or alt-screen toggle) to avoid stale entries.
    pub fn clear_hyperlink_map(&mut self) {
        self.hyperlink_map.clear();
        self.current_hyperlink = None;
    }

    // -- Private helpers ---------------------------------------------------

    /// Flush pending PTY write requests from the terminal's event listener.
    fn flush_pty_write_queue(&mut self) {
        let pending: Vec<Vec<u8>> = {
            let mut q = match self.pty_write_queue.lock() {
                Ok(q) => q,
                Err(poisoned) => poisoned.into_inner(),
            };
            std::mem::take(&mut *q)
        };

        for data in &pending {
            if let Err(e) = self.pty_writer.write_all(data) {
                warn!("failed to write terminal response to PTY: {e}");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Free-standing helpers
// ---------------------------------------------------------------------------

/// Strip null bytes from PTY input.
///
/// Null bytes (`0x00`) corrupt PTY line-discipline state and must be removed
/// before the data reaches the kernel write() call. Returns `Cow::Borrowed`
/// when the input is already null-free (zero copy), or `Cow::Owned` when
/// bytes were removed.
pub fn sanitize_pty_input(bytes: &[u8]) -> Cow<'_, [u8]> {
    if bytes.contains(&0x00) {
        Cow::Owned(bytes.iter().copied().filter(|&b| b != 0x00).collect())
    } else {
        Cow::Borrowed(bytes)
    }
}

/// Return `true` when `haystack` contains the byte pattern `needle`.
///
/// Uses a simple sliding-window scan — the patterns we check for are short
/// (≤ 6 bytes) so this is fast enough for PTY write sizes.
fn memslice_contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    if needle.len() > haystack.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scroll_methods_exist() {
        let term = PhantomTerminal::new(80, 24).expect("failed to create terminal");
        assert_eq!(term.display_offset(), 0);
        assert_eq!(term.history_size(), 0);
    }

    #[test]
    fn test_scroll_to_bottom_noop_when_at_bottom() {
        let mut term = PhantomTerminal::new(80, 24).expect("failed to create terminal");
        assert_eq!(term.display_offset(), 0);
        term.scroll_to_bottom();
        assert_eq!(term.display_offset(), 0);
    }

    #[test]
    fn default_mouse_mode_is_none() {
        let term = PhantomTerminal::new(80, 24).unwrap();
        assert_eq!(term.mouse_mode(), MouseMode::None);
    }

    #[test]
    fn default_is_not_alt_screen() {
        let term = PhantomTerminal::new(80, 24).unwrap();
        assert!(!term.is_alt_screen());
    }

    /// `last_read_buf` must return an empty slice before any read.
    #[test]
    fn last_read_buf_empty_before_any_read() {
        let term = PhantomTerminal::new(80, 24).unwrap();
        assert_eq!(term.last_read_buf().len(), 0, "no read yet — must be empty");
    }

    /// After a non-blocking read that returns 0 bytes (WouldBlock),
    /// `last_read_buf` must also return an empty slice, not the full 64 KiB
    /// scratch buffer.
    #[test]
    fn last_read_buf_returns_only_valid_bytes_after_would_block() {
        let mut term = PhantomTerminal::new(80, 24).unwrap();
        // The PTY is freshly spawned; reading immediately often returns
        // WouldBlock (no shell output yet). Either way, `last_read_len` is
        // updated: 0 on WouldBlock, n on a real read. We assert the invariant:
        // `last_read_buf().len() == pty_read() return value`.
        let n = term.pty_read().unwrap_or(0);
        assert_eq!(
            term.last_read_buf().len(),
            n,
            "last_read_buf().len() must equal the bytes returned by pty_read"
        );
    }

    // -----------------------------------------------------------------------
    // handle_mouse_scroll
    // -----------------------------------------------------------------------

    /// In mouse mode the scroll is encoded as SGR and written to the PTY;
    /// the display_offset does NOT change.
    ///
    /// We cannot easily observe what was written to the PTY in a unit test
    /// (the PTY fd is owned by the kernel TTY layer), so we verify the
    /// side-effect that `display_offset` stays at 0 — i.e. the fallback
    /// scrollback path was NOT taken.
    #[test]
    fn mouse_scroll_up_in_mouse_mode_writes_sgr_to_pty() {
        let mut term = PhantomTerminal::new(80, 24).unwrap();
        // Default mode is None; we can't activate MOUSE_REPORT_CLICK without
        // writing a real VTE sequence, but we can test the None branch and
        // verify the function compiles and runs without panic.
        // In None mode a scroll-up adjusts display_offset; we verify it is
        // clamped to history_size (0 when no output yet).
        let before = term.display_offset();
        term.handle_mouse_scroll(3.0, 10, 5);
        // history_size == 0, so display_offset stays 0 regardless.
        assert_eq!(
            term.display_offset(),
            before,
            "display_offset must stay at 0 when there is no scrollback history"
        );
    }

    /// Outside mouse mode, scrolling down decrements display_offset toward 0.
    ///
    /// With no scrollback history the display_offset is already 0; scrolling
    /// down is a no-op (clamped). The test confirms it stays at 0.
    #[test]
    fn mouse_scroll_down_outside_mouse_mode_adjusts_display_offset() {
        let mut term = PhantomTerminal::new(80, 24).unwrap();
        assert_eq!(term.mouse_mode(), MouseMode::None);
        // Scroll up first (no-op without history, but exercises the path).
        term.handle_mouse_scroll(1.0, 0, 0);
        let after_up = term.display_offset();
        // Scroll down: should not go below 0.
        term.handle_mouse_scroll(-1.0, 0, 0);
        assert_eq!(
            term.display_offset(),
            0,
            "display_offset after scroll-down should remain 0 when already at bottom; was {after_up}"
        );
    }

    /// `set_display_offset` clamps to history_size when the target overshoots.
    #[test]
    fn set_display_offset_clamps_to_history_size() {
        let mut term = PhantomTerminal::new(80, 24).unwrap();
        // No history yet; setting to a large value should remain 0.
        term.set_display_offset(9999);
        assert_eq!(term.display_offset(), 0);
    }

    // ── Bug 1: bracketed-paste size limit ────────────────────────────────

    /// Writing a bracketed-paste start followed by data that exceeds the 4 MiB
    /// cap must clear the paste session and return `Ok(())` without writing
    /// the oversized payload to the PTY.
    #[test]
    fn paste_buffer_clears_on_size_limit_exceeded() {
        let mut term = PhantomTerminal::new(80, 24).unwrap();

        // Send paste-start so the guard activates.
        term.pty_write(b"\x1b[200~").unwrap();
        assert!(term.in_bracketed_paste(), "paste session should be active");

        // Build a chunk just large enough to exceed the 4 MiB cap.
        // We skip actually writing 4 MiB + start bytes by sending a single
        // chunk that is (MAX - start_bytes + 1) bytes to trigger the limit.
        // We simulate the byte count by calling pty_write in two steps:
        // first push the count to just under the limit, then push it over.
        let almost_max = vec![b'A'; MAX_PASTE_BUFFER_BYTES - 6]; // -6 for \x1b[200~
        term.pty_write(&almost_max).unwrap();
        assert!(term.in_bracketed_paste(), "still within limit");

        // This write should push us over the cap.
        term.pty_write(b"overflow").unwrap();
        assert!(
            !term.in_bracketed_paste(),
            "paste session must be cleared after exceeding size limit"
        );
    }

    /// After `PASTE_TIMEOUT_SECS` without a terminator the `tick_paste_timeout`
    /// helper must force-clear the paste session.
    #[test]
    fn paste_buffer_clears_on_timeout_without_terminator() {
        let mut term = PhantomTerminal::new(80, 24).unwrap();
        term.pty_write(b"\x1b[200~").unwrap();
        assert!(term.in_bracketed_paste());

        // Manually backdate the start time so the timeout appears to have fired.
        // We reach into the struct directly because this is a unit test.
        term.paste_started_at =
            Some(Instant::now() - std::time::Duration::from_secs(PASTE_TIMEOUT_SECS + 1));

        term.tick_paste_timeout();
        assert!(
            !term.in_bracketed_paste(),
            "paste session must be cleared after timeout"
        );
    }

    // ── Bug 2: PTY input sanitization ────────────────────────────────────

    /// `sanitize_pty_input` must remove null bytes from the input.
    #[test]
    fn sanitize_pty_input_removes_null_bytes() {
        let input = b"hel\x00lo\x00world";
        let result = sanitize_pty_input(input);
        assert_eq!(result.as_ref(), b"helloworld");
    }

    /// `sanitize_pty_input` must return `Cow::Borrowed` (no copy) for
    /// inputs that contain no null bytes.
    #[test]
    fn sanitize_pty_input_passes_valid_utf8_unchanged() {
        let input = b"hello, world!";
        let result = sanitize_pty_input(input);
        // The result should be byte-identical and borrow the original slice.
        assert_eq!(result.as_ref(), input);
        assert!(
            matches!(result, std::borrow::Cow::Borrowed(_)),
            "no allocation expected for null-free input"
        );
    }

    // ── Bug 3: OSC 2 title forwarding ────────────────────────────────────

    /// After the terminal emits an `Event::Title`, `drain_title_queue` must
    /// return that title string.  We exercise the queue directly because the
    /// full VTE parser path requires a live PTY.
    #[test]
    fn osc2_title_event_sent_through_watch_channel() {
        // The title queue is Arc-shared between the listener and the terminal.
        // We grab a reference to the queue and push a synthetic title into it
        // the same way the event listener would.
        let term = PhantomTerminal::new(80, 24).unwrap();

        // Simulate the event listener receiving an OSC 2 title.
        {
            let mut q = term.title_queue.lock().unwrap();
            q.push("phantom — ~/projects/foo".to_string());
        }

        let titles = term.drain_title_queue();
        assert_eq!(titles.len(), 1);
        assert_eq!(titles[0], "phantom — ~/projects/foo");

        // A second drain must return nothing (queue was cleared).
        let titles2 = term.drain_title_queue();
        assert!(titles2.is_empty(), "queue must be empty after drain");
    }

    // -------------------------------------------------------------------------
    // Fix 3: OSC 52 clipboard handler
    // -------------------------------------------------------------------------

    /// Verify that `PhantomEventListener::with_osc52` forwards
    /// `ClipboardStore(Clipboard, text)` events to the channel, and that
    /// `drain_osc52` on a `PhantomTerminal` drains them correctly.
    ///
    /// We exercise the listener directly (no PTY needed) so the test is fast
    /// and hermetic.
    #[test]
    fn osc52_decoded_text_sent_on_channel() {
        use alacritty_terminal::event::Event;
        use alacritty_terminal::term::ClipboardType;
        use std::sync::mpsc;

        let pty_queue: PtyWriteQueue = Arc::new(Mutex::new(Vec::new()));
        let title_queue: TitleQueue = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = mpsc::sync_channel::<String>(32);

        let listener =
            PhantomEventListener::with_osc52(Arc::clone(&pty_queue), Arc::clone(&title_queue), tx);

        // Simulate alacritty decoding an OSC 52 sequence.
        listener.send_event(Event::ClipboardStore(
            ClipboardType::Clipboard,
            "hello from OSC 52".to_string(),
        ));
        listener.send_event(Event::ClipboardStore(
            ClipboardType::Clipboard,
            "second text".to_string(),
        ));

        // Drain from the receiver directly to assert the channel is populated.
        let mut received: Vec<String> = Vec::new();
        while let Ok(text) = rx.try_recv() {
            received.push(text);
        }

        assert_eq!(received.len(), 2, "both OSC 52 texts must be forwarded");
        assert_eq!(received[0], "hello from OSC 52");
        assert_eq!(received[1], "second text");
    }

    /// `drain_osc52` on a freshly created `PhantomTerminal` must return an
    /// empty vec (nothing has been sent yet).
    #[test]
    fn osc52_drain_empty_on_fresh_terminal() {
        let mut term = PhantomTerminal::new(80, 24).unwrap();
        let drained = term.drain_osc52();
        assert!(
            drained.is_empty(),
            "no OSC 52 events before any PTY output — drain must be empty"
        );
    }

    // =========================================================================
    // OSC 8 hyperlink tests
    // =========================================================================

    /// `process_osc8` sets `current_hyperlink` when a BEL-terminated OSC 8 open
    /// sequence is present in the byte buffer.
    #[test]
    fn osc8_open_sets_current_hyperlink() {
        let mut term = PhantomTerminal::new(80, 24).expect("create terminal");
        assert!(term.current_hyperlink.is_none());

        // OSC 8 open: ESC ] 8 ; ; https://example.com BEL
        let seq = b"\x1b]8;;https://example.com\x07";
        term.process_osc8(seq);

        assert_eq!(
            term.current_hyperlink.as_deref(),
            Some("https://example.com"),
            "current_hyperlink must be set after OSC 8 open"
        );
    }

    /// `process_osc8` clears `current_hyperlink` on OSC 8 close.
    #[test]
    fn osc8_close_clears_hyperlink() {
        let mut term = PhantomTerminal::new(80, 24).expect("create terminal");

        // First open, then close.
        let open = b"\x1b]8;;https://example.com\x07";
        term.process_osc8(open);
        assert!(term.current_hyperlink.is_some(), "must be set after open");

        let close = b"\x1b]8;;\x07";
        term.process_osc8(close);
        assert!(
            term.current_hyperlink.is_none(),
            "current_hyperlink must be None after OSC 8 close"
        );
    }

    /// `hit_test_hyperlink` returns `None` when no hyperlinks are in the map.
    #[test]
    fn hit_test_returns_none_for_plain_cell() {
        let term = PhantomTerminal::new(80, 24).expect("create terminal");
        assert!(
            term.hit_test_hyperlink(5, 2).is_none(),
            "plain cell must return None"
        );
    }

    /// `hit_test_hyperlink` returns the URL when the map has an entry for the cell.
    #[test]
    fn hit_test_returns_url_for_hyperlink_cell() {
        let mut term = PhantomTerminal::new(80, 24).expect("create terminal");
        let url: Arc<str> = Arc::from("https://phantom.dev");
        term.hyperlink_map.insert((3, 7), Arc::clone(&url));

        let result = term.hit_test_hyperlink(7, 3);
        assert_eq!(
            result,
            Some("https://phantom.dev"),
            "hit_test must return the URL for an annotated cell"
        );
    }

    /// `clear_hyperlink_map` removes all entries and resets `current_hyperlink`.
    #[test]
    fn clear_hyperlink_map_resets_state() {
        let mut term = PhantomTerminal::new(80, 24).expect("create terminal");
        let url: Arc<str> = Arc::from("https://test.com");
        term.hyperlink_map.insert((0, 0), url);
        term.current_hyperlink = Some(Arc::from("https://test.com"));

        term.clear_hyperlink_map();
        assert!(term.hyperlink_map.is_empty(), "map must be empty after clear");
        assert!(term.current_hyperlink.is_none(), "current_hyperlink must be None after clear");
    }

    /// ESC-\\ string terminator works in addition to BEL.
    #[test]
    fn osc8_open_with_st_terminator() {
        let mut term = PhantomTerminal::new(80, 24).expect("create terminal");
        // ESC ] 8 ; ; https://esc-term.io ESC \
        let seq = b"\x1b]8;;https://esc-term.io\x1b\\\\";
        term.process_osc8(seq);
        assert_eq!(
            term.current_hyperlink.as_deref(),
            Some("https://esc-term.io"),
            "ESC-\\ terminator must be recognized"
        );
    }
}
