//! phantom-supervisor -- Erlang/OTP-inspired supervisor for the Phantom terminal.
//!
//! Spawns `phantom` as a child process, monitors heartbeats over a Unix domain
//! socket, and restarts it on failure.  Accepts user commands from stdin and
//! relayed `UserCommand` messages over the socket.

mod orphan;

use std::collections::VecDeque;
use std::env;
use std::io::{self, BufRead, BufReader, Write as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use log::{error, info, warn};
use phantom_protocol::{
    AppMessage, HEARTBEAT_TIMEOUT_MS, MAX_RESTARTS, RESTART_WINDOW_SECS, SupervisorCommand,
    UserCommand, socket_path, try_read_line,
};

// ---------------------------------------------------------------------------
// Supervisor
// ---------------------------------------------------------------------------

struct Supervisor {
    /// The phantom child process, if running.
    child: Option<Child>,
    /// Listener for incoming connections from phantom.
    listener: UnixListener,
    /// Filesystem path of the socket (for cleanup).
    socket_path: PathBuf,
    /// The active connection to the phantom app.
    app_stream: Option<UnixStream>,
    /// Partial-line buffer for non-blocking reads from the app stream.
    app_read_buf: String,
    /// When the last heartbeat was received (or when we last spawned).
    last_heartbeat: Instant,
    /// Lifetime restart counter.
    restart_count: u32,
    /// Recent restart timestamps for rate limiting.
    restart_timestamps: VecDeque<Instant>,
    /// When the supervisor was created.
    start_time: Instant,
    /// Path to the phantom binary.
    phantom_binary: PathBuf,
    /// Receiver end of the stdin-reader thread channel.
    stdin_rx: mpsc::Receiver<String>,
    /// Shared flag to request graceful shutdown.
    shutdown: Arc<AtomicBool>,
}

impl Supervisor {
    // ----- construction -----------------------------------------------------

    fn new(shutdown: Arc<AtomicBool>) -> Result<Self> {
        let phantom_binary = Self::resolve_phantom_binary()?;
        info!("phantom binary: {}", phantom_binary.display());

        let pid = std::process::id();
        let sock = socket_path(pid);

        // Remove stale socket if present.
        if sock.exists() {
            std::fs::remove_file(&sock)
                .with_context(|| format!("failed to remove stale socket {}", sock.display()))?;
        }

        let listener = UnixListener::bind(&sock)
            .with_context(|| format!("failed to bind socket {}", sock.display()))?;
        listener.set_nonblocking(true)?;
        info!("listening on {}", sock.display());

        // Spawn a background thread to read stdin line-by-line so the main
        // loop never blocks on user input.
        let (tx, rx) = mpsc::channel::<String>();
        thread::spawn(move || {
            let stdin = io::stdin();
            let reader = BufReader::new(stdin.lock());
            for line in reader.lines() {
                match line {
                    Ok(l) => {
                        if tx.send(l).is_err() {
                            break; // receiver dropped
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(Self {
            child: None,
            listener,
            socket_path: sock,
            app_stream: None,
            app_read_buf: String::new(),
            last_heartbeat: Instant::now(),
            restart_count: 0,
            restart_timestamps: VecDeque::new(),
            start_time: Instant::now(),
            phantom_binary,
            stdin_rx: rx,
            shutdown,
        })
    }

    /// Locate the `phantom` binary.  Checks `PHANTOM_BIN` env var first, then
    /// looks in the same directory as the currently-running supervisor binary.
    fn resolve_phantom_binary() -> Result<PathBuf> {
        if let Ok(p) = env::var("PHANTOM_BIN") {
            return Ok(PathBuf::from(p));
        }

        let self_exe = env::current_exe().context("cannot determine own binary path")?;
        let dir = self_exe
            .parent()
            .context("supervisor binary has no parent directory")?;
        Ok(dir.join("phantom"))
    }

    // ----- child management -------------------------------------------------

    fn spawn_phantom(&mut self) -> Result<()> {
        info!("spawning phantom: {}", self.phantom_binary.display());

        let child = Command::new(&self.phantom_binary)
            .env("PHANTOM_SUPERVISOR_SOCK", &self.socket_path)
            .spawn()
            .with_context(|| {
                format!(
                    "failed to spawn phantom at {}",
                    self.phantom_binary.display()
                )
            })?;

        info!("phantom spawned -- pid {}", child.id());
        self.child = Some(child);
        self.last_heartbeat = Instant::now();
        self.app_stream = None;
        self.app_read_buf.clear();
        Ok(())
    }

    fn kill_phantom(&mut self) {
        if let Some(ref mut child) = self.child {
            let pid = child.id();
            info!("sending SIGTERM to phantom (pid {})", pid);

            // SIGTERM first.
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGTERM);
            }

            // Wait up to 1 second for graceful exit.
            let deadline = Instant::now() + Duration::from_secs(1);
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        info!("phantom (pid {pid}) exited: {status}");
                        break;
                    }
                    Ok(None) if Instant::now() >= deadline => {
                        warn!("phantom (pid {pid}) did not exit in time -- SIGKILL");
                        unsafe {
                            libc::kill(pid as libc::pid_t, libc::SIGKILL);
                        }
                        let _ = child.wait(); // reap
                        break;
                    }
                    Ok(None) => {
                        thread::sleep(Duration::from_millis(50));
                    }
                    Err(e) => {
                        error!("error waiting for phantom: {e}");
                        break;
                    }
                }
            }
        }

        self.child = None;
        self.app_stream = None;
        self.app_read_buf.clear();
    }

    fn restart_phantom(&mut self) -> Result<()> {
        // Rate-limit: only keep timestamps within the sliding window.
        let window = Duration::from_secs(RESTART_WINDOW_SECS);
        let now = Instant::now();
        while self
            .restart_timestamps
            .front()
            .is_some_and(|&t| now.duration_since(t) > window)
        {
            self.restart_timestamps.pop_front();
        }

        if self.restart_timestamps.len() as u32 >= MAX_RESTARTS {
            error!(
                "phantom restarted {MAX_RESTARTS} times within {RESTART_WINDOW_SECS}s -- giving up"
            );
            self.kill_phantom();
            bail!("restart rate limit exceeded");
        }

        self.kill_phantom();
        self.restart_count += 1;
        self.restart_timestamps.push_back(now);
        info!(
            "restarting phantom (total restarts: {})",
            self.restart_count
        );
        self.spawn_phantom()
    }

    // ----- send to app ------------------------------------------------------

    fn send_to_app(&mut self, cmd: &SupervisorCommand) {
        if let Some(ref mut stream) = self.app_stream {
            let line = format!("{}\n", cmd.to_line());
            if let Err(e) = stream.write_all(line.as_bytes()) {
                warn!("failed to send command to phantom: {e}");
                self.app_stream = None;
                self.app_read_buf.clear();
            }
        } else {
            warn!("no active connection to phantom -- command dropped");
        }
    }

    // ----- main loop --------------------------------------------------------

    fn run(&mut self) -> Result<()> {
        info!("supervisor main loop starting");

        while !self.shutdown.load(Ordering::Relaxed) {
            // 1. Accept new connections (non-blocking).
            self.accept_connections();

            // 2. Read messages from the app stream.
            self.read_app_messages();

            // 3. Heartbeat watchdog.
            if self.child.is_some() && self.heartbeat_expired() {
                warn!("heartbeat timeout -- restarting phantom");
                if let Err(e) = self.restart_phantom() {
                    error!("{e}");
                    break;
                }
            }

            // 4. Check if child exited unexpectedly.
            self.check_child_exit()?;

            // 5. Read and handle stdin commands.
            self.read_stdin_commands();

            // 6. Brief sleep to avoid busy-waiting.
            thread::sleep(Duration::from_millis(10));
        }

        info!("supervisor shutting down");
        self.cleanup();
        Ok(())
    }

    fn accept_connections(&mut self) {
        match self.listener.accept() {
            Ok((stream, _addr)) => {
                info!("phantom connected on socket");
                if let Err(e) = stream.set_nonblocking(true) {
                    error!("failed to set stream non-blocking: {e}");
                    return;
                }
                self.app_stream = Some(stream);
                self.app_read_buf.clear();
                self.last_heartbeat = Instant::now();
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => warn!("accept error: {e}"),
        }
    }

    fn read_app_messages(&mut self) {
        let Some(ref stream) = self.app_stream else {
            return;
        };

        // Collect complete lines first to avoid borrow conflicts with
        // dispatch_app_line (which needs &mut self).
        let mut lines: Vec<String> = Vec::new();
        let mut disconnected = false;

        loop {
            match try_read_line(stream, &mut self.app_read_buf) {
                Ok(Some(line)) => lines.push(line),
                Ok(None) => break, // would block, no complete line yet
                Err(e) => {
                    info!("app stream error: {e}");
                    disconnected = true;
                    break;
                }
            }
        }

        if disconnected {
            self.app_stream = None;
            self.app_read_buf.clear();
        }

        for line in lines {
            self.dispatch_app_line(&line);
        }
    }

    /// Dispatch a single line received from the app socket.
    ///
    /// Lines may be `AppMessage`s (HEARTBEAT, READY, LOG:..., PONG) or
    /// `UserCommand`s (USER:RESTART, USER:SET:k:v, ...) relayed from the
    /// app when the user types `! <command>` inside phantom.
    fn dispatch_app_line(&mut self, line: &str) {
        let trimmed = line.trim();

        // Try AppMessage first.
        if let Some(msg) = AppMessage::from_line(trimmed) {
            self.handle_app_message(msg);
            return;
        }

        // Try UserCommand (relayed from the app).
        if let Some(cmd) = UserCommand::from_line(trimmed) {
            self.handle_user_command_proto(cmd);
            return;
        }

        warn!("unrecognised message from app: {trimmed}");
    }

    fn handle_app_message(&mut self, msg: AppMessage) {
        match msg {
            AppMessage::Heartbeat => {
                self.last_heartbeat = Instant::now();
            }
            AppMessage::Ready => {
                info!("phantom reports READY");
                self.last_heartbeat = Instant::now();
            }
            AppMessage::Pong => {
                info!("phantom PONG");
            }
            AppMessage::Log(text) => {
                info!("[phantom] {text}");
            }
            AppMessage::ExitClean => {
                info!("phantom requested clean exit — supervisor standing down");
                self.shutdown.store(true, Ordering::Relaxed);
            }
        }
    }

    fn heartbeat_expired(&self) -> bool {
        let timeout = Duration::from_millis(HEARTBEAT_TIMEOUT_MS);
        if self.app_stream.is_none() {
            // Be more lenient while waiting for the app to connect.
            return self.last_heartbeat.elapsed() > timeout * 2;
        }
        self.last_heartbeat.elapsed() > timeout
    }

    fn check_child_exit(&mut self) -> Result<()> {
        let exited = if let Some(ref mut child) = self.child {
            match child.try_wait() {
                Ok(Some(status)) => {
                    warn!("phantom exited unexpectedly: {status}");
                    Some(())
                }
                Ok(None) => None,
                Err(e) => {
                    error!("error polling child: {e}");
                    None
                }
            }
        } else {
            None
        };

        if exited.is_some() {
            self.child = None;
            self.app_stream = None;
            self.app_read_buf.clear();
            if self.shutdown.load(Ordering::Relaxed) {
                info!("phantom exited cleanly — not restarting");
            } else {
                self.restart_phantom()?;
            }
        }
        Ok(())
    }

    // ----- stdin commands ---------------------------------------------------

    fn read_stdin_commands(&mut self) {
        while let Ok(line) = self.stdin_rx.try_recv() {
            let trimmed = line.trim().to_string();
            if trimmed.is_empty() {
                continue;
            }
            self.handle_stdin_command(&trimmed);
        }
    }

    /// Parse and execute a command typed directly into the supervisor's stdin.
    fn handle_stdin_command(&mut self, input: &str) {
        let parts: Vec<&str> = input.splitn(3, ' ').collect();
        match parts[0] {
            "restart" => {
                println!("[supervisor] restarting phantom...");
                if let Err(e) = self.restart_phantom() {
                    eprintln!("[supervisor] restart failed: {e}");
                }
            }
            "kill" => {
                println!("[supervisor] killing phantom and exiting");
                self.shutdown.store(true, Ordering::Relaxed);
            }
            "status" => self.print_status(),
            "set" if parts.len() >= 3 => {
                let cmd = SupervisorCommand::Set {
                    key: parts[1].to_string(),
                    value: parts[2].to_string(),
                };
                self.send_to_app(&cmd);
                println!("[supervisor] sent SET command");
            }
            "set" => println!("[supervisor] usage: set <key> <value>"),
            "theme" if parts.len() >= 2 => {
                self.send_to_app(&SupervisorCommand::Theme(parts[1].to_string()));
                println!("[supervisor] sent THEME command");
            }
            "theme" => println!("[supervisor] usage: theme <name>"),
            "reload" => {
                self.send_to_app(&SupervisorCommand::Reload);
                println!("[supervisor] sent RELOAD");
            }
            "ping" => {
                self.send_to_app(&SupervisorCommand::Ping);
                println!("[supervisor] sent PING");
            }
            "shutdown" => {
                self.send_to_app(&SupervisorCommand::Shutdown);
                println!("[supervisor] sent SHUTDOWN to phantom");
            }
            "help" => Self::print_help(),
            other => {
                println!("[supervisor] unknown command: {other}  (type 'help' for commands)");
            }
        }
    }

    /// Handle a `UserCommand` relayed from phantom over the socket.
    fn handle_user_command_proto(&mut self, cmd: UserCommand) {
        match cmd {
            UserCommand::Restart => {
                info!("user requested restart via app");
                if let Err(e) = self.restart_phantom() {
                    error!("restart failed: {e}");
                }
            }
            UserCommand::Kill => {
                info!("user requested kill via app");
                self.shutdown.store(true, Ordering::Relaxed);
            }
            UserCommand::Status => self.print_status(),
            UserCommand::Set { key, value } => {
                let cmd = SupervisorCommand::Set { key, value };
                self.send_to_app(&cmd);
            }
            UserCommand::Theme(name) => {
                self.send_to_app(&SupervisorCommand::Theme(name));
            }
            UserCommand::Reload => {
                self.send_to_app(&SupervisorCommand::Reload);
            }
            UserCommand::Boot => {
                // The protocol doesn't have CMD:BOOT; we can send it raw
                // or handle it supervisor-side. For now, log it.
                info!("user requested boot replay");
                // Forward as a raw line if the app understands it.
                if let Some(ref mut stream) = self.app_stream {
                    let _ = stream.write_all(b"CMD:BOOT\n");
                }
            }
            UserCommand::Help => Self::print_help(),
        }
    }

    // ----- display ----------------------------------------------------------

    fn print_status(&self) {
        let uptime = self.start_time.elapsed();
        let heartbeat_age = self.last_heartbeat.elapsed();
        let child_pid = self.child.as_ref().map(|c| c.id());
        let connected = self.app_stream.is_some();

        println!("+-  phantom-supervisor status  ----------------+");
        println!(
            "|  uptime          {:>8.1}s                   |",
            uptime.as_secs_f64()
        );
        println!(
            "|  restarts        {:<6}                       |",
            self.restart_count
        );
        println!(
            "|  heartbeat age   {:>6}ms                     |",
            heartbeat_age.as_millis()
        );
        println!(
            "|  child pid       {:<10}                   |",
            child_pid
                .map(|p| p.to_string())
                .unwrap_or_else(|| "none".into())
        );
        println!(
            "|  connected       {:<6}                       |",
            if connected { "yes" } else { "no" }
        );
        println!("+-----------------------------------------------+");
    }

    fn print_help() {
        println!("+-- supervisor commands ------------------------+");
        println!("|  restart     kill & respawn phantom            |");
        println!("|  kill        kill phantom and exit supervisor  |");
        println!("|  status      show uptime, pid, heartbeat      |");
        println!("|  set K V     forward CMD:SET:K:V to app       |");
        println!("|  theme NAME  forward CMD:THEME:NAME to app    |");
        println!("|  reload      forward CMD:RELOAD to app        |");
        println!("|  ping        forward CMD:PING to app          |");
        println!("|  shutdown    forward CMD:SHUTDOWN to app       |");
        println!("|  help        show this message                 |");
        println!("+-----------------------------------------------+");
    }

    // ----- cleanup ----------------------------------------------------------

    fn cleanup(&mut self) {
        self.kill_phantom();
        if self.socket_path.exists() {
            let _ = std::fs::remove_file(&self.socket_path);
            info!("removed socket {}", self.socket_path.display());
        }
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        self.cleanup();
    }
}

// ---------------------------------------------------------------------------
// Signal handling
// ---------------------------------------------------------------------------

/// Block SIGINT and SIGTERM in the calling thread and spawn a dedicated
/// signal-waiter thread that sets `shutdown` when either signal arrives.
fn install_signal_handlers(shutdown: Arc<AtomicBool>) {
    // Block the signals in the main thread first so they propagate to the
    // signal-waiter thread via sigwait.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
    }

    thread::spawn(move || unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::sigaddset(&mut set, libc::SIGTERM);

        let mut sig: libc::c_int = 0;
        loop {
            let rc = libc::sigwait(&set, &mut sig);
            if rc == 0 {
                eprintln!("\n[supervisor] received signal {sig}, shutting down...");
                shutdown.store(true, Ordering::Relaxed);
                break;
            }
        }
    });
}

// ---------------------------------------------------------------------------
// macOS: DYLD_FALLBACK_LIBRARY_PATH injection
// ---------------------------------------------------------------------------

/// On macOS, `DYLD_FALLBACK_LIBRARY_PATH` must be set before the dynamic
/// linker resolves Swift runtime libraries used by the audio-capture backend.
/// When Phantom is launched via `cargo run --bin phantom` (without `run.sh`)
/// this variable may be absent, causing `dlopen` failures at runtime.
///
/// This function injects a suitable path early in the supervisor — before the
/// phantom child process is spawned — so the child inherits it automatically.
/// It is a no-op if the variable is already set.
#[cfg(target_os = "macos")]
fn inject_dyld_fallback() {
    if std::env::var("DYLD_FALLBACK_LIBRARY_PATH").is_ok() {
        // Already set (e.g. caller exported it, or run.sh set it). Respect it.
        return;
    }

    match find_swift_runtime_path() {
        Some(path) => {
            info!("injecting DYLD_FALLBACK_LIBRARY_PATH={path}");
            // SAFETY: single-threaded at this point in main(); no other threads
            // have been spawned yet when we call this.
            #[allow(unused_unsafe)]
            unsafe {
                std::env::set_var("DYLD_FALLBACK_LIBRARY_PATH", &path);
            }
        }
        None => {
            warn!(
                "Swift runtime not found — audio capture will be unavailable \
                 (set DYLD_FALLBACK_LIBRARY_PATH manually if needed)"
            );
        }
    }
}

/// Probe for the Swift standard-library directory on the host macOS system.
///
/// Strategy (in order):
/// 1. Ask `xcrun --show-sdk-path` and derive `<sdk>/usr/lib/swift`.
/// 2. Fall back to the well-known system path `/usr/lib/swift`.
#[cfg(target_os = "macos")]
fn find_swift_runtime_path() -> Option<String> {
    // --- Strategy 1: xcrun SDK path -------------------------------------------
    if let Ok(output) = std::process::Command::new("xcrun")
        .args(["--show-sdk-path"])
        .output()
    {
        if output.status.success() {
            let sdk = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !sdk.is_empty() {
                let candidate = format!("{sdk}/usr/lib/swift");
                if std::path::Path::new(&candidate).exists() {
                    return Some(candidate);
                }
            }
        }
    }

    // --- Strategy 2: well-known system path ------------------------------------
    let fallback = "/usr/lib/swift";
    if std::path::Path::new(fallback).exists() {
        return Some(fallback.to_string());
    }

    None
}

// ---------------------------------------------------------------------------
// Entrypoint
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    // Inject DYLD_FALLBACK_LIBRARY_PATH on macOS so Swift-backed audio capture
    // works when launched directly via `cargo run --bin phantom` without run.sh.
    #[cfg(target_os = "macos")]
    inject_dyld_fallback();

    print_banner();

    // Recover orphaned child processes from any previous crashed phantom instance
    // before we spawn a new one.
    match orphan::pid_file_path() {
        Ok(pid_path) => {
            if let Err(e) = orphan::recover_orphans(&pid_path) {
                warn!("orphan recovery encountered an error: {e}");
            }
        }
        Err(e) => warn!("could not determine PID file path: {e}"),
    }

    let shutdown = Arc::new(AtomicBool::new(false));
    install_signal_handlers(Arc::clone(&shutdown));

    let mut supervisor = Supervisor::new(Arc::clone(&shutdown))?;
    supervisor.spawn_phantom()?;
    supervisor.run()?;

    Ok(())
}

fn print_banner() {
    let pid = std::process::id();
    eprintln!("+=======================================+");
    eprintln!("|   PHANTOM SUPERVISOR                  |");
    eprintln!("|   Erlang/OTP-style process monitor    |");
    eprintln!("|   pid {pid:<6}                          |");
    eprintln!("+=======================================+");
}
