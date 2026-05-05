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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Result;
use log::{debug, warn};

use phantom_protocol::{
    AppMessage, HEARTBEAT_INTERVAL_MS, SupervisorCommand, set_nonblocking, try_read_line,
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

    /// Notify the supervisor that the render loop has escalated past the
    /// consecutive-panic threshold and is forcing an exit.
    ///
    /// This allows the supervisor to distinguish a panic-escalation crash from
    /// a silent heartbeat-timeout crash (GPU hang, SIGKILL, etc.) and record
    /// the cause in its log before restarting.
    pub fn notify_render_panic(&mut self, count: u32, last_message: &str) {
        warn!(
            "Notifying supervisor of render-panic escalation (count={count}, msg={last_message})"
        );
        self.send(&AppMessage::RenderPanic {
            count,
            last_message: last_message.to_owned(),
        });
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;

    use phantom_protocol::AppMessage;

    use super::SupervisorClient;

    fn temp_sock_path(suffix: &str) -> PathBuf {
        std::env::temp_dir().join(format!("phantom-test-{suffix}.sock"))
    }

    fn bind_listener(path: &PathBuf) -> UnixListener {
        let _ = std::fs::remove_file(path);
        UnixListener::bind(path).expect("bind listener")
    }

    /// `notify_render_panic` writes a `RENDER_PANIC:<count>:<msg>` line to the
    /// supervisor socket.  Spins up a minimal Unix socket server, connects a
    /// `SupervisorClient`, triggers panic escalation, and verifies the server
    /// received the expected wire message.
    #[test]
    fn render_panic_sends_supervisor_notification() {
        let path = temp_sock_path("render-panic");
        let listener = bind_listener(&path);

        let listener_thread = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream);
            let mut lines = Vec::new();
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => lines.push(line.trim().to_owned()),
                    Err(_) => break,
                }
            }
            lines
        });

        let mut client = SupervisorClient::connect(&path).expect("connect");
        client.notify_render_panic(11, "index out of bounds: the len is 0");
        drop(client);

        let received = listener_thread.join().expect("listener thread panicked");

        let panic_line = received
            .iter()
            .find(|l| l.starts_with("RENDER_PANIC:"))
            .expect("no RENDER_PANIC line received");

        let parsed =
            AppMessage::from_line(panic_line).expect("RENDER_PANIC line must parse as AppMessage");

        match parsed {
            AppMessage::RenderPanic { count, last_message } => {
                assert_eq!(count, 11);
                assert_eq!(last_message, "index out of bounds: the len is 0");
            }
            other => panic!("expected RenderPanic, got {other:?}"),
        }

        let _ = std::fs::remove_file(temp_sock_path("render-panic"));
    }
}
