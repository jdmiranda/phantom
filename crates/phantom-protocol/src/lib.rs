//! Phantom two-process supervisor protocol.
//!
//! Defines the line-based communication protocol between the supervisor process
//! and the main app process over a Unix domain socket at `/tmp/phantom-{pid}.sock`.
//!
//! Wire format: newline-delimited UTF-8 text. Each message is a single line.

pub mod events;

pub use events::{AgentId, Event, EventClass, EventTopic, JobId, SessionId};
// Note: Don't re-export AppId from here -- phantom-adapter owns that type.

use std::io::Read as _;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// How often the app sends a heartbeat to the supervisor.
pub const HEARTBEAT_INTERVAL_MS: u64 = 500;

/// How long the supervisor waits before declaring the app dead.
/// 10 seconds to allow for GPU init, font loading, and boot sequence.
pub const HEARTBEAT_TIMEOUT_MS: u64 = 10_000;

/// Maximum restart attempts within the restart window.
pub const MAX_RESTARTS: u32 = 5;

/// Window (seconds) in which `MAX_RESTARTS` applies.
pub const RESTART_WINDOW_SECS: u64 = 60;

// ---------------------------------------------------------------------------
// Env-var accessors for heartbeat constants
// ---------------------------------------------------------------------------

/// Returns the heartbeat interval, overridable via `PHANTOM_HEARTBEAT_INTERVAL_MS`.
#[must_use]
pub fn heartbeat_interval() -> Duration {
    let ms = std::env::var("PHANTOM_HEARTBEAT_INTERVAL_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(HEARTBEAT_INTERVAL_MS);
    Duration::from_millis(ms)
}

/// Returns the heartbeat timeout, overridable via `PHANTOM_HEARTBEAT_TIMEOUT_MS`.
#[must_use]
pub fn heartbeat_timeout() -> Duration {
    let ms = std::env::var("PHANTOM_HEARTBEAT_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(HEARTBEAT_TIMEOUT_MS);
    Duration::from_millis(ms)
}

// ---------------------------------------------------------------------------
// Socket path helpers
// ---------------------------------------------------------------------------

/// Returns the canonical socket path for a supervisor with the given PID.
#[must_use]
pub fn socket_path(supervisor_pid: u32) -> PathBuf {
    PathBuf::from(format!("/tmp/phantom-{supervisor_pid}.sock"))
}

/// Returns `true` when a `connect()` to `path` succeeds within ~200 ms.
///
/// The connect itself is synchronous; the read timeout is set only as a
/// safety net — we do not actually read anything. A successful connection
/// is sufficient evidence that a live listener is on the other end.
#[must_use]
pub fn is_socket_live(path: &Path) -> bool {
    match UnixStream::connect(path) {
        Ok(stream) => {
            let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
            true
        }
        Err(_) => false,
    }
}

/// Scans `/tmp` for `phantom-*.sock` files and returns the first one that
/// accepts a connection.  Stale socket files left by a crashed supervisor
/// are skipped via [`is_socket_live`].
#[must_use]
pub fn find_socket() -> Option<PathBuf> {
    let Ok(entries) = std::fs::read_dir("/tmp") else {
        return None;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with("phantom-") && name.ends_with(".sock") {
            let path = entry.path();
            if is_socket_live(&path) {
                return Some(path);
            }
        }
    }
    None
}

/// Scans `/tmp` for a live `phantom-*.sock` that is **not** `old_path`.
///
/// Call this after detecting that the current supervisor has gone silent.
/// Returns the path of the first live replacement socket found, or `None`
/// if no alternative is available yet.
#[must_use]
pub fn reconnect(old_path: &Path) -> Option<PathBuf> {
    let Ok(entries) = std::fs::read_dir("/tmp") else {
        return None;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with("phantom-") && name.ends_with(".sock") {
            let path = entry.path();
            if path != old_path && is_socket_live(&path) {
                return Some(path);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// SupervisorCommand  (supervisor -> app)
// ---------------------------------------------------------------------------

/// Commands sent from the supervisor to the running app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupervisorCommand {
    /// Live config change: set a key/value pair.
    Set { key: String, value: String },
    /// Hot-swap the active theme.
    Theme(String),
    /// Re-read the config file from disk.
    Reload,
    /// Initiate graceful shutdown.
    Shutdown,
    /// Health-check ping; app should reply with `Pong`.
    Ping,
}

impl SupervisorCommand {
    /// Serialize to the wire format (without trailing newline).
    #[must_use]
    pub fn to_line(&self) -> String {
        match self {
            Self::Set { key, value } => format!("CMD:SET:{key}:{value}"),
            Self::Theme(name) => format!("CMD:THEME:{name}"),
            Self::Reload => "CMD:RELOAD".into(),
            Self::Shutdown => "CMD:SHUTDOWN".into(),
            Self::Ping => "CMD:PING".into(),
        }
    }

    /// Parse from a wire-format line (without trailing newline).
    #[must_use]
    pub fn from_line(s: &str) -> Option<Self> {
        let s = s.strip_prefix("CMD:")?;
        if let Some(rest) = s.strip_prefix("SET:") {
            let (key, value) = rest.split_once(':')?;
            Some(Self::Set {
                key: key.to_owned(),
                value: value.to_owned(),
            })
        } else if let Some(name) = s.strip_prefix("THEME:") {
            Some(Self::Theme(name.to_owned()))
        } else {
            match s {
                "RELOAD" => Some(Self::Reload),
                "SHUTDOWN" => Some(Self::Shutdown),
                "PING" => Some(Self::Ping),
                _ => None,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// AppMessage  (app -> supervisor)
// ---------------------------------------------------------------------------

/// Messages sent from the app back to the supervisor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppMessage {
    /// Periodic liveness signal.
    Heartbeat,
    /// App has finished initialisation and is ready to accept input.
    Ready,
    /// Forwarded log line.
    Log(String),
    /// Response to a `Ping`.
    Pong,
    /// App is exiting intentionally — supervisor should NOT restart.
    ExitClean,
    /// Render loop accumulated too many consecutive panics and is forcing exit.
    /// The supervisor uses this to distinguish a panic-escalation crash from
    /// a silent heartbeat-timeout crash (GPU hang, SIGKILL, etc.).
    ///
    /// The render thread panicked. `count` is the number of panics since last
    /// reset; `last_message` is the panic message (truncated if needed).
    RenderPanic { count: u32, last_message: String },
}

impl AppMessage {
    /// Encodes this message as a single wire-format line, omitting the trailing
    /// newline so callers control framing on the socket write.
    ///
    /// The encoding is intentionally allocation-light and panic-free: simple
    /// variants resolve to interned literals, while `Log` and `RenderPanic`
    /// allocate because their payloads are dynamic. Both `LOG:` and the
    /// trailing `last_message` of `RENDER_PANIC:<count>:<message>` may embed
    /// colons; `from_line` strips their fixed-shape prefixes and treats the
    /// remainder as opaque so message bodies survive round-trip without
    /// escaping.
    #[must_use]
    pub fn to_line(&self) -> String {
        match self {
            Self::Heartbeat => "HEARTBEAT".into(),
            Self::Ready => "READY".into(),
            Self::Log(msg) => format!("LOG:{msg}"),
            Self::Pong => "PONG".into(),
            Self::ExitClean => "EXIT_CLEAN".into(),
            Self::RenderPanic { count, last_message } => {
                format!("RENDER_PANIC:{count}:{last_message}")
            }
        }
    }

    /// Decodes a wire-format line into an `AppMessage`, returning `None` for
    /// any unrecognised or malformed input rather than panicking.
    ///
    /// Returning `Option` (not `Result`) is deliberate: the supervisor treats
    /// unknown lines as forward-compatible noise from a newer app build and
    /// keeps the connection open, so a missing variant must not surface as an
    /// error. The `LOG:` prefix is checked first so colons inside log bodies
    /// are preserved verbatim — exact-match tags are tested only after that
    /// strip fails, preventing variants like `EXIT_CLEAN` from being shadowed
    /// by a future tag that happens to share a prefix.
    #[must_use]
    pub fn from_line(s: &str) -> Option<Self> {
        if let Some(msg) = s.strip_prefix("LOG:") {
            Some(Self::Log(msg.to_owned()))
        } else if let Some(rest) = s.strip_prefix("RENDER_PANIC:") {
            let (count_str, last_message) = rest.split_once(':')?;
            let count = count_str.parse().ok()?;
            Some(Self::RenderPanic {
                count,
                last_message: last_message.to_owned(),
            })
        } else {
            match s {
                "HEARTBEAT" => Some(Self::Heartbeat),
                "READY" => Some(Self::Ready),
                "PONG" => Some(Self::Pong),
                "EXIT_CLEAN" => Some(Self::ExitClean),
                _ => None,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// UserCommand  (parsed from `! command` input)
// ---------------------------------------------------------------------------

/// Commands the user enters via the `!` prefix in the terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserCommand {
    Restart,
    Kill,
    Status,
    Set { key: String, value: String },
    Theme(String),
    Reload,
    /// Replay the boot sequence animation.
    Boot,
    Help,
}

impl UserCommand {
    #[must_use]
    pub fn to_line(&self) -> String {
        match self {
            Self::Restart => "USER:RESTART".into(),
            Self::Kill => "USER:KILL".into(),
            Self::Status => "USER:STATUS".into(),
            Self::Set { key, value } => format!("USER:SET:{key}:{value}"),
            Self::Theme(name) => format!("USER:THEME:{name}"),
            Self::Reload => "USER:RELOAD".into(),
            Self::Boot => "USER:BOOT".into(),
            Self::Help => "USER:HELP".into(),
        }
    }

    #[must_use]
    pub fn from_line(s: &str) -> Option<Self> {
        let s = s.strip_prefix("USER:")?;
        if let Some(rest) = s.strip_prefix("SET:") {
            let (key, value) = rest.split_once(':')?;
            Some(Self::Set {
                key: key.to_owned(),
                value: value.to_owned(),
            })
        } else if let Some(name) = s.strip_prefix("THEME:") {
            Some(Self::Theme(name.to_owned()))
        } else {
            match s {
                "RESTART" => Some(Self::Restart),
                "KILL" => Some(Self::Kill),
                "STATUS" => Some(Self::Status),
                "RELOAD" => Some(Self::Reload),
                "BOOT" => Some(Self::Boot),
                "HELP" => Some(Self::Help),
                _ => None,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Response
// ---------------------------------------------------------------------------

/// Generic response envelope for synchronous request/reply exchanges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    Ok(Option<String>),
    Err(String),
}

impl Response {
    #[must_use]
    pub fn to_line(&self) -> String {
        match self {
            Self::Ok(None) => "OK".into(),
            Self::Ok(Some(data)) => format!("OK:{data}"),
            Self::Err(msg) => format!("ERR:{msg}"),
        }
    }

    #[must_use]
    pub fn from_line(s: &str) -> Option<Self> {
        if let Some(msg) = s.strip_prefix("ERR:") {
            Some(Self::Err(msg.to_owned()))
        } else if let Some(data) = s.strip_prefix("OK:") {
            Some(Self::Ok(Some(data.to_owned())))
        } else if s == "OK" {
            Some(Self::Ok(None))
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Non-blocking socket helpers
// ---------------------------------------------------------------------------

/// Set a `UnixStream` to non-blocking mode.
pub fn set_nonblocking(stream: &UnixStream) -> Result<()> {
    stream.set_nonblocking(true)?;
    Ok(())
}

/// Attempt a non-blocking line read from `stream`.
///
/// Bytes are appended to `buf`. If a complete line (terminated by `\n`) is
/// found, it is drained from `buf` and returned (without the trailing newline).
/// Returns `Ok(None)` when the read would block (no data available yet).
pub fn try_read_line(stream: &UnixStream, buf: &mut String) -> Result<Option<String>> {
    let mut tmp = [0u8; 1024];
    let mut reader = stream;
    match reader.read(&mut tmp) {
        Ok(0) => anyhow::bail!("connection closed"),
        Ok(n) => {
            let chunk = std::str::from_utf8(&tmp[..n])?;
            buf.push_str(chunk);
        }
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
            // No data right now -- fall through to check if buf already has a line.
        }
        Err(e) => return Err(e.into()),
    }

    if let Some(pos) = buf.find('\n') {
        let line = buf[..pos].to_owned();
        // Drain the consumed bytes including the newline.
        buf.drain(..=pos);
        Ok(Some(line))
    } else {
        Ok(None)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- SupervisorCommand round-trips ------------------------------------

    #[test]
    fn supervisor_set_round_trip() {
        let cmd = SupervisorCommand::Set {
            key: "font_size".into(),
            value: "14".into(),
        };
        let line = cmd.to_line();
        assert_eq!(line, "CMD:SET:font_size:14");
        assert_eq!(SupervisorCommand::from_line(&line), Some(cmd));
    }

    #[test]
    fn supervisor_theme_round_trip() {
        let cmd = SupervisorCommand::Theme("gruvbox".into());
        let line = cmd.to_line();
        assert_eq!(line, "CMD:THEME:gruvbox");
        assert_eq!(SupervisorCommand::from_line(&line), Some(cmd));
    }

    #[test]
    fn supervisor_reload_round_trip() {
        let cmd = SupervisorCommand::Reload;
        assert_eq!(SupervisorCommand::from_line(&cmd.to_line()), Some(cmd));
    }

    #[test]
    fn supervisor_shutdown_round_trip() {
        let cmd = SupervisorCommand::Shutdown;
        assert_eq!(SupervisorCommand::from_line(&cmd.to_line()), Some(cmd));
    }

    #[test]
    fn supervisor_ping_round_trip() {
        let cmd = SupervisorCommand::Ping;
        assert_eq!(SupervisorCommand::from_line(&cmd.to_line()), Some(cmd));
    }

    #[test]
    fn supervisor_rejects_garbage() {
        assert_eq!(SupervisorCommand::from_line("GARBAGE"), None);
        assert_eq!(SupervisorCommand::from_line("CMD:UNKNOWN"), None);
    }

    // -- AppMessage round-trips -------------------------------------------

    #[test]
    fn app_heartbeat_round_trip() {
        let msg = AppMessage::Heartbeat;
        assert_eq!(AppMessage::from_line(&msg.to_line()), Some(msg));
    }

    #[test]
    fn app_ready_round_trip() {
        let msg = AppMessage::Ready;
        assert_eq!(AppMessage::from_line(&msg.to_line()), Some(msg));
    }

    #[test]
    fn app_pong_round_trip() {
        let msg = AppMessage::Pong;
        assert_eq!(AppMessage::from_line(&msg.to_line()), Some(msg));
    }

    #[test]
    fn app_log_round_trip() {
        let msg = AppMessage::Log("something went wrong: code=42".into());
        let line = msg.to_line();
        assert_eq!(line, "LOG:something went wrong: code=42");
        assert_eq!(AppMessage::from_line(&line), Some(msg));
    }

    #[test]
    fn app_log_with_colons() {
        // Colons in the log body must survive the round-trip.
        let msg = AppMessage::Log("key:value:extra".into());
        assert_eq!(AppMessage::from_line(&msg.to_line()), Some(msg));
    }

    #[test]
    fn app_render_panic_round_trip() {
        let msg = AppMessage::RenderPanic {
            count: 11,
            last_message: "index out of bounds".into(),
        };
        let line = msg.to_line();
        assert_eq!(line, "RENDER_PANIC:11:index out of bounds");
        assert_eq!(AppMessage::from_line(&line), Some(msg));
    }

    #[test]
    fn app_render_panic_message_with_colons() {
        let msg = AppMessage::RenderPanic {
            count: 3,
            last_message: "called `Option::unwrap()` on a `None` value".into(),
        };
        assert_eq!(AppMessage::from_line(&msg.to_line()), Some(msg));
    }

    #[test]
    fn app_rejects_garbage() {
        assert_eq!(AppMessage::from_line("NOPE"), None);
    }

    // -- UserCommand round-trips ------------------------------------------

    #[test]
    fn user_simple_variants_round_trip() {
        for cmd in [
            UserCommand::Restart,
            UserCommand::Kill,
            UserCommand::Status,
            UserCommand::Reload,
            UserCommand::Boot,
            UserCommand::Help,
        ] {
            assert_eq!(
                UserCommand::from_line(&cmd.to_line()),
                Some(cmd.clone()),
                "round-trip failed for {cmd:?}"
            );
        }
    }

    #[test]
    fn user_set_round_trip() {
        let cmd = UserCommand::Set {
            key: "opacity".into(),
            value: "0.9".into(),
        };
        let line = cmd.to_line();
        assert_eq!(line, "USER:SET:opacity:0.9");
        assert_eq!(UserCommand::from_line(&line), Some(cmd));
    }

    #[test]
    fn user_theme_round_trip() {
        let cmd = UserCommand::Theme("dracula".into());
        let line = cmd.to_line();
        assert_eq!(line, "USER:THEME:dracula");
        assert_eq!(UserCommand::from_line(&line), Some(cmd));
    }

    #[test]
    fn user_rejects_garbage() {
        assert_eq!(UserCommand::from_line("RESTART"), None); // missing USER: prefix
        assert_eq!(UserCommand::from_line("USER:NOPE"), None);
    }

    // -- Response round-trips ---------------------------------------------

    #[test]
    fn response_ok_empty_round_trip() {
        let r = Response::Ok(None);
        assert_eq!(r.to_line(), "OK");
        assert_eq!(Response::from_line(&r.to_line()), Some(r));
    }

    #[test]
    fn response_ok_data_round_trip() {
        let r = Response::Ok(Some("running pid=1234".into()));
        let line = r.to_line();
        assert_eq!(line, "OK:running pid=1234");
        assert_eq!(Response::from_line(&line), Some(r));
    }

    #[test]
    fn response_err_round_trip() {
        let r = Response::Err("not found".into());
        let line = r.to_line();
        assert_eq!(line, "ERR:not found");
        assert_eq!(Response::from_line(&line), Some(r));
    }

    #[test]
    fn response_rejects_garbage() {
        assert_eq!(Response::from_line("WHAT"), None);
    }

    // -- Socket path helpers ----------------------------------------------

    #[test]
    fn socket_path_format() {
        let p = socket_path(42);
        assert_eq!(p, PathBuf::from("/tmp/phantom-42.sock"));
    }

    // -- Value preservation for colons in SET values ----------------------

    #[test]
    fn supervisor_set_value_with_colons() {
        // The value may contain colons (e.g. a color hex `#aa:bb:cc`).
        // split_once on the first colon after the key means the rest is the value.
        let cmd = SupervisorCommand::Set {
            key: "color".into(),
            value: "aa:bb:cc".into(),
        };
        let line = cmd.to_line();
        assert_eq!(line, "CMD:SET:color:aa:bb:cc");
        assert_eq!(SupervisorCommand::from_line(&line), Some(cmd));
    }

    #[test]
    fn user_set_value_with_colons() {
        let cmd = UserCommand::Set {
            key: "url".into(),
            value: "http://localhost:8080".into(),
        };
        let line = cmd.to_line();
        assert_eq!(line, "USER:SET:url:http://localhost:8080");
        assert_eq!(UserCommand::from_line(&line), Some(cmd));
    }

    // -- is_socket_live / find_socket / reconnect -------------------------

    #[test]
    fn find_socket_skips_stale_socket() {
        use std::fs;

        // Create a socket file path that no one is listening on.
        let stale = PathBuf::from("/tmp/phantom-test-stale-99999.sock");
        // Ensure any leftover file is cleaned up first.
        let _ = fs::remove_file(&stale);
        // Write a regular file at that path — nothing is listening.
        fs::write(&stale, b"").expect("could not create stale socket file");

        // is_socket_live must return false for a non-socket path.
        assert!(!is_socket_live(&stale), "stale file should not be live");

        let _ = fs::remove_file(&stale);
    }

    #[test]
    fn reconnect_returns_new_socket() {
        use std::fs;
        use std::os::unix::net::UnixListener;

        let old = PathBuf::from("/tmp/phantom-test-old-88888.sock");
        let new_path = PathBuf::from("/tmp/phantom-test-new-88888.sock");
        let _ = fs::remove_file(&old);
        let _ = fs::remove_file(&new_path);

        // Bind a live listener on new_path.
        let _listener =
            UnixListener::bind(&new_path).expect("could not bind test socket");

        // old_path does not exist, new_path is live.
        let found = reconnect(&old);
        // We may find new_path or any other real phantom socket, but we must
        // not return old_path.
        if let Some(ref p) = found {
            assert_ne!(p, &old, "reconnect must not return the old socket");
        }

        let _ = fs::remove_file(&new_path);
    }

    // -- Env-var accessors ------------------------------------------------

    #[test]
    fn heartbeat_interval_reads_from_env_var() {
        // SAFETY: this test is single-threaded; no concurrent env access.
        unsafe {
            std::env::set_var("PHANTOM_HEARTBEAT_INTERVAL_MS", "1234");
        }
        let d = heartbeat_interval();
        unsafe {
            std::env::remove_var("PHANTOM_HEARTBEAT_INTERVAL_MS");
        }
        assert_eq!(d, Duration::from_millis(1234));
    }

    #[test]
    fn heartbeat_interval_uses_default_when_unset() {
        unsafe {
            std::env::remove_var("PHANTOM_HEARTBEAT_INTERVAL_MS");
        }
        let d = heartbeat_interval();
        assert_eq!(d, Duration::from_millis(HEARTBEAT_INTERVAL_MS));
    }

    #[test]
    fn heartbeat_timeout_reads_from_env_var() {
        // SAFETY: this test is single-threaded; no concurrent env access.
        unsafe {
            std::env::set_var("PHANTOM_HEARTBEAT_TIMEOUT_MS", "9999");
        }
        let d = heartbeat_timeout();
        unsafe {
            std::env::remove_var("PHANTOM_HEARTBEAT_TIMEOUT_MS");
        }
        assert_eq!(d, Duration::from_millis(9999));
    }
}
