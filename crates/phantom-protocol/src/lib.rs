//! Phantom two-process supervisor protocol.
//!
//! Defines the line-based communication protocol between the supervisor process
//! and the main app process over a Unix domain socket at `/tmp/phantom-{pid}.sock`.
//!
//! Wire format: newline-delimited UTF-8 text. Each message is a single line.

use std::io::Read as _;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

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
// Socket path helpers
// ---------------------------------------------------------------------------

/// Returns the canonical socket path for a supervisor with the given PID.
pub fn socket_path(supervisor_pid: u32) -> PathBuf {
    PathBuf::from(format!("/tmp/phantom-{supervisor_pid}.sock"))
}

/// Scans `/tmp` for an existing `phantom-*.sock` file and returns the first match.
pub fn find_socket() -> Option<PathBuf> {
    let Ok(entries) = std::fs::read_dir("/tmp") else {
        return None;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with("phantom-") && name.ends_with(".sock") {
            return Some(entry.path());
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
}

impl AppMessage {
    pub fn to_line(&self) -> String {
        match self {
            Self::Heartbeat => "HEARTBEAT".into(),
            Self::Ready => "READY".into(),
            Self::Log(msg) => format!("LOG:{msg}"),
            Self::Pong => "PONG".into(),
        }
    }

    pub fn from_line(s: &str) -> Option<Self> {
        if let Some(msg) = s.strip_prefix("LOG:") {
            Some(Self::Log(msg.to_owned()))
        } else {
            match s {
                "HEARTBEAT" => Some(Self::Heartbeat),
                "READY" => Some(Self::Ready),
                "PONG" => Some(Self::Pong),
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
    pub fn to_line(&self) -> String {
        match self {
            Self::Ok(None) => "OK".into(),
            Self::Ok(Some(data)) => format!("OK:{data}"),
            Self::Err(msg) => format!("ERR:{msg}"),
        }
    }

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
}
