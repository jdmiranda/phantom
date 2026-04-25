//! Terminal emulation core wrapping `alacritty_terminal`.
//!
//! Spawns a PTY with the user's default shell and manages the terminal state
//! machine. All PTY I/O is non-blocking.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::io::AsRawFd;
use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::term::Config;
use alacritty_terminal::tty::{self, Options as PtyOptions};
use alacritty_terminal::vte::ansi;
use alacritty_terminal::Term;
use anyhow::{Context, Result};
use log::{debug, trace, warn};

/// Default scrollback history in lines.
const SCROLLBACK_LINES: usize = 10_000;

/// Read buffer size for PTY output.
const PTY_READ_BUF: usize = 0x10000; // 64 KiB

/// Default cell dimensions in pixels (used for TIOCSWINSZ pixel fields).
const DEFAULT_CELL_WIDTH: u16 = 8;
const DEFAULT_CELL_HEIGHT: u16 = 16;

// ---------------------------------------------------------------------------
// EventListener — forwards terminal events to Phantom
// ---------------------------------------------------------------------------

/// Shared queue for data the terminal wants written back to the PTY.
///
/// The `EventListener` is moved into `Term` and we lose direct access, so we
/// share the write queue through an `Arc<Mutex<_>>`.
type PtyWriteQueue = Arc<Mutex<Vec<Vec<u8>>>>;

/// Listener that receives events from the alacritty terminal state machine.
///
/// Events like device-attribute responses (`PtyWrite`) are buffered in a
/// shared queue that the `PhantomTerminal` drains after each read cycle.
#[derive(Clone, Debug)]
pub struct PhantomEventListener {
    pty_writes: PtyWriteQueue,
}

impl PhantomEventListener {
    fn new(queue: PtyWriteQueue) -> Self {
        Self { pty_writes: queue }
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
            Event::Title(title) => debug!("terminal title: {title}"),
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
    pub fn new(cols: u16, rows: u16) -> Self {
        Self { cols, rows }
    }

    /// Build the `WindowSize` that the PTY / kernel expects.
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

    /// Current terminal dimensions.
    size: TerminalSize,

    /// Scratch buffer for PTY reads, allocated once.
    read_buf: Vec<u8>,
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

        // Create the terminal state machine.
        let event_listener = PhantomEventListener::new(Arc::clone(&pty_write_queue));
        let term = Term::new(config, &size, event_listener);

        debug!("PhantomTerminal created: {cols}x{rows}");

        Ok(Self {
            term,
            parser: ansi::Processor::new(),
            pty_reader,
            pty_writer,
            _pty: pty,
            pty_write_queue,
            size,
            read_buf: vec![0u8; PTY_READ_BUF],
        })
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
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(0),
            Err(e) => return Err(e).context("PTY read failed"),
        };

        // Feed the raw bytes through the VTE parser into the terminal.
        // This mirrors what alacritty's event_loop does: `parser.advance(&mut term, buf)`.
        self.parser.advance(&mut self.term, &self.read_buf[..n]);

        // Drain any PTY-write requests the terminal generated (e.g. device
        // attribute responses) and write them back to the PTY.
        self.flush_pty_write_queue();

        trace!("pty_read: processed {n} bytes");
        Ok(n)
    }

    /// Access the raw bytes from the last `pty_read` call.
    ///
    /// Returns the slice of the internal read buffer that was filled during
    /// the most recent read. The returned slice is only valid until the next
    /// `pty_read` call.
    #[inline]
    pub fn last_read_buf(&self) -> &[u8] {
        // After pty_read, self.read_buf contains the last-read data.
        // The length is not tracked separately, but callers can use the
        // return value of pty_read to slice this.
        &self.read_buf
    }

    /// Write raw input bytes to the PTY (keyboard/mouse input, paste data, etc.).
    pub fn pty_write(&mut self, data: &[u8]) -> Result<()> {
        self.pty_writer
            .write_all(data)
            .context("PTY write failed")?;
        Ok(())
    }

    /// Immutable access to the terminal state (grid, cursor, modes).
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
    #[inline]
    pub fn size(&self) -> TerminalSize {
        self.size
    }

    /// The PTY file descriptor for ioctl queries (e.g. foreground process group).
    #[inline]
    pub fn pty_fd(&self) -> &File {
        &self.pty_reader
    }

    /// Scroll the viewport one page up in the scrollback buffer.
    pub fn scroll_page_up(&mut self) {
        self.term.scroll_display(Scroll::PageUp);
    }

    /// Scroll the viewport one page down toward the latest output.
    pub fn scroll_page_down(&mut self) {
        self.term.scroll_display(Scroll::PageDown);
    }

    /// Scroll the viewport to the very top of the scrollback buffer.
    pub fn scroll_to_top(&mut self) {
        self.term.scroll_display(Scroll::Top);
    }

    /// Scroll the viewport to the bottom (latest output).
    pub fn scroll_to_bottom(&mut self) {
        self.term.scroll_display(Scroll::Bottom);
    }

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
