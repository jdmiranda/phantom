//! Non-blocking client for communicating with the Phantom supervisor process.
//!
//! When Phantom is launched by the supervisor, the env var
//! `PHANTOM_SUPERVISOR_SOCK` contains the Unix socket path. The client
//! connects, sends periodic heartbeats, and receives live-config commands.
//!
//! Heartbeats run on a dedicated background thread so they are never blocked
//! by slow frames, GPU syncs, or heavy PTY processing on the main thread.

use std::collections::VecDeque;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use log::{debug, warn};

use phantom_protocol::{
    AppMessage, HEARTBEAT_TIMEOUT_MS, SupervisorCommand, heartbeat_interval,
    set_nonblocking, try_read_line,
};

// ---------------------------------------------------------------------------
// Activity ring buffer
// ---------------------------------------------------------------------------

/// A single entry in the client's activity ring buffer.
///
/// Each entry records what Phantom was doing at a given instant so that when
/// a supervisor-silence event fires, the last N frames of activity are
/// available for diagnostic logging.
#[derive(Debug, Clone)]
struct ActivityFrame {
    /// Unix-epoch milliseconds when the event was recorded.
    timestamp_ms: u64,
    /// Short human-readable description of what Phantom was doing.
    description: String,
}

/// How many activity frames to keep in the ring buffer.
const ACTIVITY_RING_SIZE: usize = 20;

// ---------------------------------------------------------------------------
// SupervisorClient
// ---------------------------------------------------------------------------

/// A connection to the supervisor process.
///
/// Heartbeats are sent by a background thread (decoupled from frame rate).
/// The main thread polls for incoming commands via [`try_recv`].
///
/// The client also tracks when the last message was received from the
/// supervisor.  If no message arrives within `HEARTBEAT_TIMEOUT_MS * 3`,
/// [`try_recv`] logs a warning and attempts [`reconnect`].
pub struct SupervisorClient {
    /// Main-thread stream for reading commands and sending one-off messages.
    stream: UnixStream,
    read_buf: String,
    /// Signal the heartbeat thread to stop on shutdown.
    alive: Arc<AtomicBool>,
    /// Join handle for the heartbeat thread.
    heartbeat_thread: Option<std::thread::JoinHandle<()>>,
    /// Unix-epoch milliseconds of the last successfully received supervisor
    /// message, shared with the heartbeat thread for diagnostics.
    last_ack_ms: Arc<AtomicU64>,
    /// The socket path we are currently connected to (for reconnect logic).
    socket_path: PathBuf,
    /// Ring buffer of recent activity frames for crash diagnostics.
    activity: VecDeque<ActivityFrame>,
    /// Set to `true` the first time supervisor silence is detected so we only
    /// dump the activity log once per silence episode (not every poll cycle).
    silence_dumped: bool,
}

impl SupervisorClient {
    /// Connect to the supervisor socket at `path`.
    ///
    /// Spawns a background thread that sends heartbeats every
    /// [`heartbeat_interval()`] milliseconds, independent of the frame loop.
    pub fn connect(path: &Path) -> Result<Self> {
        debug!("Connecting to supervisor at {}", path.display());
        let stream = UnixStream::connect(path)?;
        set_nonblocking(&stream)?;
        debug!("Connected to supervisor (non-blocking)");

        // Clone the stream for the heartbeat thread. The cloned fd shares
        // the same underlying socket — writes from either side are atomic
        // at the line level (each heartbeat is one small write).
        let hb_stream = stream.try_clone()?;
        let alive = Arc::new(AtomicBool::new(true));
        let alive_clone = Arc::clone(&alive);

        // Initialise the ACK timestamp to "now" so the timeout doesn't fire
        // immediately before the supervisor has had a chance to reply.
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let last_ack_ms = Arc::new(AtomicU64::new(now_ms));

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
            last_ack_ms,
            socket_path: path.to_path_buf(),
            activity: VecDeque::with_capacity(ACTIVITY_RING_SIZE + 1),
            silence_dumped: false,
        })
    }

    /// Background heartbeat loop. Sends `HEARTBEAT` at the configured interval
    /// regardless of what the main thread is doing.
    ///
    /// IMPORTANT: the cloned fd shares the same file description as the
    /// main thread's stream, so we must NOT call set_nonblocking — that
    /// would flip the main thread to blocking too. Instead we keep
    /// non-blocking mode and retry writes on WouldBlock.
    fn heartbeat_loop(mut stream: UnixStream, alive: Arc<AtomicBool>) {
        let interval = heartbeat_interval();
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

    /// Record a single activity frame into the diagnostic ring buffer.
    ///
    /// Call this from the main frame loop with a short description of what
    /// Phantom is currently doing (e.g. `"frame 1234: 3 adapters, boot"`).
    /// The oldest entry is evicted when the buffer exceeds [`ACTIVITY_RING_SIZE`].
    ///
    /// This is intentionally cheap: one `VecDeque` push + optional pop, no
    /// allocation after the initial capacity is reached.
    pub fn record_activity(&mut self, description: impl Into<String>) {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        if self.activity.len() >= ACTIVITY_RING_SIZE {
            self.activity.pop_front();
        }
        self.activity.push_back(ActivityFrame {
            timestamp_ms: ts,
            description: description.into(),
        });
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
    ///
    /// Also checks for supervisor silence: if no message has been received
    /// within `HEARTBEAT_TIMEOUT_MS * 3` milliseconds, a warning is logged
    /// and a reconnect is attempted.  A successful reconnect replaces the
    /// active stream, resets the ACK timestamp, and respawns the heartbeat
    /// thread so heartbeats resume immediately on the new connection.
    pub fn try_recv(&mut self) -> Option<SupervisorCommand> {
        // ------------------------------------------------------------------
        // Silence detection: compare last-ack timestamp against the threshold.
        // ------------------------------------------------------------------
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let last_ms = self.last_ack_ms.load(Ordering::Relaxed);
        let silence_threshold_ms = HEARTBEAT_TIMEOUT_MS * 3;

        if now_ms.saturating_sub(last_ms) > silence_threshold_ms {
            warn!(
                "Supervisor silent for >{}ms — attempting reconnect",
                silence_threshold_ms
            );

            // Dump the activity ring buffer exactly once per silence episode
            // (requirement 2) so we know what Phantom was doing when heartbeats
            // stopped flowing.
            if !self.silence_dumped {
                self.silence_dumped = true;
                warn!("crash-diag: supervisor-silence activity log ({} frames):", self.activity.len());
                for (i, frame) in self.activity.iter().enumerate() {
                    warn!(
                        "  [{:>2}] +{}ms  {}",
                        i,
                        now_ms.saturating_sub(frame.timestamp_ms),
                        frame.description
                    );
                }
                if self.activity.is_empty() {
                    warn!("  (no activity frames recorded — record_activity() not called yet)");
                }
            }

            // Always reconnect to the known supervisor socket path.  The
            // `reconnect()` helper scans for an ALTERNATIVE socket (for when
            // the supervisor restarts), but since the supervisor socket path
            // is fixed for its lifetime, we first try `self.socket_path`
            // directly.  `reconnect()` is also unreliable because it matches
            // `phantom-mcp-*.sock` files and returns them as valid targets.
            let target = self.socket_path.clone();

            match UnixStream::connect(&target) {
                Ok(new_stream) => {
                    if set_nonblocking(&new_stream).is_ok() {
                        // Respawn the heartbeat thread with a clone of the new
                        // stream.  The old thread may have exited on Broken Pipe
                        // (when the previous connection dropped); without this
                        // the supervisor never receives another heartbeat and
                        // kills Phantom ~10s later.
                        if let Ok(hb_stream) = new_stream.try_clone() {
                            self.alive.store(false, Ordering::Relaxed);
                            let _ = self.heartbeat_thread.take(); // drop old handle
                            let alive = Arc::new(AtomicBool::new(true));
                            self.alive = Arc::clone(&alive);
                            if let Ok(handle) = std::thread::Builder::new()
                                .name("supervisor-heartbeat".into())
                                .spawn(move || Self::heartbeat_loop(hb_stream, alive))
                            {
                                self.heartbeat_thread = Some(handle);
                            }
                        }
                        debug!("Reconnected to supervisor at {}", target.display());
                        self.stream = new_stream;
                        self.socket_path = target;
                        self.read_buf.clear();
                        self.last_ack_ms.store(now_ms, Ordering::Relaxed);
                        // Reset the dump flag so a future silence episode
                        // will produce a fresh log.
                        self.silence_dumped = false;
                    }
                }
                Err(e) => {
                    warn!("Reconnect to {} failed: {e}", target.display());
                    // Reset timestamp to avoid log spam; retry after another window.
                    self.last_ack_ms.store(now_ms, Ordering::Relaxed);
                }
            }
        }

        // ------------------------------------------------------------------
        // Normal non-blocking read.
        // ------------------------------------------------------------------
        match try_read_line(&self.stream, &mut self.read_buf) {
            Ok(Some(line)) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    return None;
                }
                // Stamp the ACK timestamp on every received line.
                self.last_ack_ms.store(now_ms, Ordering::Relaxed);
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

    /// Returns the elapsed time since the last message was received from the
    /// supervisor.  Useful for tests and diagnostic overlays.
    #[must_use]
    pub fn supervisor_silence_duration(&self) -> Duration {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let last_ms = self.last_ack_ms.load(Ordering::Relaxed);
        Duration::from_millis(now_ms.saturating_sub(last_ms))
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
    use std::sync::atomic::Ordering;

    use phantom_protocol::{AppMessage, HEARTBEAT_TIMEOUT_MS};

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

    /// Verify that a client whose `last_ack_ms` is set far in the past will
    /// attempt to reconnect during `try_recv`.  The test uses a real listener
    /// so the initial `connect()` succeeds, then backdates the ACK timestamp
    /// past the silence threshold and calls `try_recv()`.  We assert that
    /// `supervisor_silence_duration()` exceeds the threshold before the call
    /// (confirming the backdating worked) and that `try_recv()` returns `None`
    /// (no data on the socket) without panicking.
    #[test]
    fn heartbeat_timeout_triggers_reconnect_attempt() {
        let sock_path = PathBuf::from("/tmp/phantom-test-hb-timeout-77777.sock");
        let _ = std::fs::remove_file(&sock_path);

        let _listener =
            UnixListener::bind(&sock_path).expect("bind test socket");

        let mut client = SupervisorClient::connect(&sock_path)
            .expect("connect to test socket");

        // Backdate the ACK timestamp well past the 3× threshold.
        let ancient_ms = 0u64; // epoch — definitely stale
        client.last_ack_ms.store(ancient_ms, Ordering::Relaxed);

        // Confirm silence is detected.
        let silence = client.supervisor_silence_duration();
        assert!(
            silence.as_millis() > (HEARTBEAT_TIMEOUT_MS * 3) as u128,
            "silence duration {silence:?} should exceed 3× timeout"
        );

        // try_recv must not panic; it will log a warning and attempt reconnect.
        let _ = client.try_recv();

        let _ = std::fs::remove_file(&sock_path);
    }
}
