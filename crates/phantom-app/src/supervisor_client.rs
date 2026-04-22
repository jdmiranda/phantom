//! Non-blocking client for communicating with the Phantom supervisor process.
//!
//! When Phantom is launched by the supervisor, the env var
//! `PHANTOM_SUPERVISOR_SOCK` contains the Unix socket path. The client
//! connects, sends periodic heartbeats, and receives live-config commands.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Result;
use log::{debug, warn};

use phantom_protocol::{
    set_nonblocking, try_read_line, AppMessage, SupervisorCommand, HEARTBEAT_INTERVAL_MS,
};

// ---------------------------------------------------------------------------
// SupervisorClient
// ---------------------------------------------------------------------------

/// A non-blocking connection to the supervisor process.
pub struct SupervisorClient {
    stream: UnixStream,
    read_buf: String,
    last_heartbeat: Instant,
}

impl SupervisorClient {
    /// Connect to the supervisor socket at `socket_path` and set non-blocking.
    pub fn connect(socket_path: &Path) -> Result<Self> {
        debug!("Connecting to supervisor at {}", socket_path.display());
        let stream = UnixStream::connect(socket_path)?;
        set_nonblocking(&stream)?;
        debug!("Connected to supervisor (non-blocking)");

        Ok(Self {
            stream,
            read_buf: String::with_capacity(256),
            last_heartbeat: Instant::now(),
        })
    }

    /// Send an [`AppMessage`] to the supervisor.
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

    /// Send a heartbeat if at least `HEARTBEAT_INTERVAL_MS` has elapsed.
    pub fn send_heartbeat(&mut self) {
        let interval = Duration::from_millis(HEARTBEAT_INTERVAL_MS);
        if self.last_heartbeat.elapsed() >= interval {
            debug!("Sending HEARTBEAT to supervisor");
            self.send(&AppMessage::Heartbeat);
            self.last_heartbeat = Instant::now();
        }
    }

    /// Non-blocking attempt to read a command from the supervisor.
    ///
    /// Returns `Some(cmd)` if a complete line was received and parsed,
    /// `None` if no data is available or the line was not a valid command.
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
