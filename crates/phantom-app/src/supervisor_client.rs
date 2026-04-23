//! Non-blocking client for communicating with the Phantom supervisor process.
//!
//! When Phantom is launched by the supervisor, the env var
//! `PHANTOM_SUPERVISOR_SOCK` contains the Unix socket path. The client
//! connects, sends periodic heartbeats, and receives live-config commands.
//!
//! Heartbeats run on a dedicated background thread so they are never blocked
//! by slow frames, GPU syncs, or heavy PTY processing on the main thread.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use log::{debug, warn};

use phantom_protocol::{
    set_nonblocking, try_read_line, AppMessage, SupervisorCommand, HEARTBEAT_INTERVAL_MS,
};

// ---------------------------------------------------------------------------
// SupervisorClient
// ---------------------------------------------------------------------------

/// A connection to the supervisor process.
///
/// Heartbeats are sent by a background thread (decoupled from frame rate).
/// The main thread polls for incoming commands via [`try_recv`].
pub struct SupervisorClient {
    /// Main-thread stream for reading commands and sending one-off messages.
    stream: UnixStream,
    read_buf: String,
    /// Signal the heartbeat thread to stop on shutdown.
    alive: Arc<AtomicBool>,
    /// Join handle for the heartbeat thread.
    heartbeat_thread: Option<std::thread::JoinHandle<()>>,
}

impl SupervisorClient {
    /// Connect to the supervisor socket at `socket_path`.
    ///
    /// Spawns a background thread that sends heartbeats every
    /// `HEARTBEAT_INTERVAL_MS` milliseconds, independent of the frame loop.
    pub fn connect(socket_path: &Path) -> Result<Self> {
        debug!("Connecting to supervisor at {}", socket_path.display());
        let stream = UnixStream::connect(socket_path)?;
        set_nonblocking(&stream)?;
        debug!("Connected to supervisor (non-blocking)");

        // Clone the stream for the heartbeat thread. The cloned fd shares
        // the same underlying socket — writes from either side are atomic
        // at the line level (each heartbeat is one small write).
        let hb_stream = stream.try_clone()?;
        let alive = Arc::new(AtomicBool::new(true));
        let alive_clone = Arc::clone(&alive);

        let heartbeat_thread = std::thread::Builder::new()
            .name("supervisor-heartbeat".into())
            .spawn(move || {
                Self::heartbeat_loop(hb_stream, alive_clone);
            })?;

        Ok(Self {
            stream,
            read_buf: String::with_capacity(256),
            alive,
            heartbeat_thread: Some(heartbeat_thread),
        })
    }

    /// Background heartbeat loop. Sends `HEARTBEAT` at a fixed interval
    /// regardless of what the main thread is doing.
    ///
    /// IMPORTANT: the cloned fd shares the same file description as the
    /// main thread's stream, so we must NOT call set_nonblocking — that
    /// would flip the main thread to blocking too. Instead we keep
    /// non-blocking mode and retry writes on WouldBlock.
    fn heartbeat_loop(mut stream: UnixStream, alive: Arc<AtomicBool>) {
        let interval = Duration::from_millis(HEARTBEAT_INTERVAL_MS);
        let msg = format!("{}\n", AppMessage::Heartbeat.to_line());
        let msg_bytes = msg.as_bytes();

        while alive.load(Ordering::Relaxed) {
            // Non-blocking write — retry once on WouldBlock.
            match stream.write_all(msg_bytes) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // Socket buffer full; skip this heartbeat, try next interval.
                }
                Err(e) => {
                    warn!("Heartbeat write failed: {e}");
                    break;
                }
            }
            std::thread::sleep(interval);
        }
        debug!("Heartbeat thread exiting");
    }

    /// Send an [`AppMessage`] to the supervisor (from the main thread).
    pub fn send(&mut self, msg: &AppMessage) {
        let line = format!("{}\n", msg.to_line());
        if let Err(e) = self.stream.write_all(line.as_bytes()) {
            warn!("Failed to send to supervisor: {e}");
        }
    }

    /// Send the `Ready` message, indicating initialisation is complete.
    pub fn send_ready(&mut self) {
        debug!("Sending READY to supervisor");
        self.send(&AppMessage::Ready);
    }

    /// Non-blocking attempt to read a command from the supervisor.
    pub fn try_recv(&mut self) -> Option<SupervisorCommand> {
        match try_read_line(&self.stream, &mut self.read_buf) {
            Ok(Some(line)) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    return None;
                }
                let cmd = SupervisorCommand::from_line(trimmed);
                if cmd.is_none() {
                    warn!("Unknown supervisor command: {trimmed}");
                }
                cmd
            }
            Ok(None) => None,
            Err(e) => {
                warn!("Supervisor read error: {e}");
                None
            }
        }
    }
}

impl Drop for SupervisorClient {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Relaxed);
        if let Some(handle) = self.heartbeat_thread.take() {
            let _ = handle.join();
        }
    }
}
