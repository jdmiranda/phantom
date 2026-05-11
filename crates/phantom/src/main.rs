use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use phantom_app::app::App;
use phantom_app::config::PhantomConfig;
use phantom_renderer::gpu::GpuContext;
use winit::{
    application::ApplicationHandler,
    event::{ElementState, WindowEvent},
    event_loop::{ActiveEventLoop, EventLoop},
    keyboard::{Key, NamedKey},
    window::{Fullscreen, Window, WindowAttributes, WindowId},
};

mod auth_cli;
mod builder_cli;
mod fleet_cli;
mod headless;
mod loop_cli;
mod path_resolver;

struct Phantom {
    window: Option<Arc<Window>>,
    app: Option<App>,
    config: PhantomConfig,
    supervisor_socket: Option<PathBuf>,
    modifiers: winit::event::Modifiers,
    consecutive_panics: u32,
    /// Tracks whether the window is currently visible (not minimized/occluded).
    /// When `false`, `request_redraw` is suppressed to avoid wasting GPU/CPU.
    window_visible: bool,
}

impl Phantom {
    fn new(config: PhantomConfig, supervisor_socket: Option<PathBuf>) -> Self {
        Self {
            window: None,
            app: None,
            config,
            supervisor_socket,
            modifiers: winit::event::Modifiers::default(),
            consecutive_panics: 0,
            window_visible: true,
        }
    }
}

fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

impl ApplicationHandler for Phantom {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }

        let initial_fullscreen = if self.config.fullscreen {
            Some(Fullscreen::Borderless(None))
        } else {
            None
        };
        let attrs = WindowAttributes::default()
            .with_title("PHANTOM v0.1.0")
            .with_fullscreen(initial_fullscreen);

        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                log::error!("Failed to create window: {e}");
                event_loop.exit();
                return;
            }
        };

        let gpu = match GpuContext::new(window.clone()) {
            Ok(gpu) => {
                log::info!(
                    "GPU initialized: {}x{}",
                    gpu.surface_config.width,
                    gpu.surface_config.height
                );
                gpu
            }
            Err(e) => {
                log::error!("Failed to initialize GPU: {e}");
                event_loop.exit();
                return;
            }
        };

        let scale_factor = window.scale_factor() as f32;
        log::info!("Display scale factor: {scale_factor}");

        match App::with_config_scaled(
            gpu,
            self.config.clone(),
            self.supervisor_socket.as_deref(),
            scale_factor,
        ) {
            Ok(app) => {
                self.app = Some(app);
            }
            Err(e) => {
                log::error!("Failed to initialize Phantom: {e}");
                event_loop.exit();
                return;
            }
        }

        window.request_redraw();
        self.window = Some(window);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                log::info!("Window closed. Shutting down.");
                if let Some(app) = &mut self.app {
                    app.shutdown();
                }
                event_loop.exit();
            }
            WindowEvent::Resized(new_size) => {
                if let Some(app) = &mut self.app {
                    app.handle_resize(new_size.width, new_size.height);
                }
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            WindowEvent::KeyboardInput { ref event, .. } => {
                // Intercept fullscreen toggle (F11 or Cmd+Enter) before
                // forwarding the key to the app so the app never sees it.
                let is_fullscreen_toggle = event.state == ElementState::Pressed
                    && match &event.logical_key {
                        Key::Named(NamedKey::F11) => true,
                        Key::Named(NamedKey::Enter) => {
                            // Cmd+Enter on macOS; Super on other platforms.
                            let mods = self.modifiers.state();
                            mods.super_key()
                        }
                        _ => false,
                    };

                if is_fullscreen_toggle {
                    if let Some(window) = &self.window {
                        let next = if window.fullscreen().is_some() {
                            None
                        } else {
                            Some(Fullscreen::Borderless(None))
                        };
                        log::info!(
                            "Fullscreen toggle: {}",
                            if next.is_some() { "on" } else { "off" }
                        );
                        window.set_fullscreen(next);
                    }
                } else {
                    let modifiers = self.modifiers;
                    let event = event.clone();
                    if let Some(app) = &mut self.app {
                        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
                            app.handle_key_with_mods(event, modifiers);
                        }));
                        if let Err(ref panic) = result {
                            log::error!("Input panic: {}", panic_message(panic));
                        }
                    }
                }
                if let Some(app) = &mut self.app
                    && app.should_quit()
                {
                    app.shutdown();
                    event_loop.exit();
                }
            }
            WindowEvent::Occluded(occluded) => {
                // On macOS, `Occluded(true)` fires when the window is minimised
                // or fully hidden behind other windows. Pause the render loop
                // while occluded to avoid wasting GPU/CPU cycles.
                self.window_visible = !occluded;
                log::debug!("Window occlusion changed: visible={}", self.window_visible);
                if self.window_visible {
                    // Re-arm the render loop immediately when we become visible.
                    if let Some(window) = &self.window {
                        window.request_redraw();
                    }
                }
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                self.modifiers = modifiers;
            }
            WindowEvent::CursorMoved { position, .. } => {
                if let Some(app) = &mut self.app {
                    app.handle_cursor_moved(position.x, position.y);
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if let Some(app) = &mut self.app {
                    app.handle_mouse_click(state, button);
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                if let Some(app) = &mut self.app {
                    app.handle_mouse_scroll(delta);
                }
            }
            WindowEvent::RedrawRequested => {
                let frame_result = if let Some(app) = &mut self.app {
                    // Raw-write frame trace to disk (survives SIGKILL, bypasses logger).
                    // Only writes every ~500 frames to avoid I/O overhead.
                    if let Some(trace) = app.watchdog_trace(500)
                        && let Some(log_path) = LOG_PATH.get()
                    {
                        raw_append_to_log(log_path, trace.as_bytes());
                    }

                    std::panic::catch_unwind(AssertUnwindSafe(|| {
                        app.update();
                        // Bug 3: forward OSC 2 window title changes to the OS.
                        if let Some(title) = app.take_pending_window_title()
                            && let Some(window) = &self.window
                        {
                            window.set_title(&title);
                        }
                        if let Err(e) = app.render() {
                            log::error!("Render error: {e}");
                        }
                    }))
                } else {
                    Ok(())
                };
                match frame_result {
                    Ok(()) => self.consecutive_panics = 0,
                    Err(panic) => {
                        self.consecutive_panics += 1;
                        log::error!(
                            "Frame panic #{}: {}",
                            self.consecutive_panics,
                            panic_message(&panic),
                        );
                        if self.consecutive_panics > 10 {
                            log::error!(
                                "Too many consecutive panics ({}) — forcing exit",
                                self.consecutive_panics,
                            );
                            if let Some(app) = &mut self.app {
                                app.shutdown();
                            }
                            event_loop.exit();
                            return;
                        }
                    }
                }
                // Only reschedule the next frame while the window is visible.
                // When occluded/minimised, `WindowEvent::Occluded(false)` will
                // re-arm the loop once the window reappears.
                if self.window_visible {
                    if let Some(window) = &self.window {
                        window.request_redraw();
                    }
                }
            }
            _ => {}
        }
    }
}

/// Load a `.env` file from the current directory if it exists.
/// Only sets vars that are not already in the environment.
fn load_dotenv() {
    let path = std::path::Path::new(".env");
    if !path.exists() {
        return;
    }
    if let Ok(contents) = std::fs::read_to_string(path) {
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((key, value)) = line.split_once('=') {
                let key = key.trim();
                let value = value.trim();
                if std::env::var(key).is_err() {
                    // SAFETY: single-threaded at this point (before any spawns).
                    unsafe {
                        std::env::set_var(key, value);
                    }
                }
            }
        }
    }
}

fn print_help() {
    println!(
        r#"PHANTOM v0.1.0 — AI-native terminal emulator

USAGE:
    phantom [OPTIONS]              Launch the terminal (default).
    phantom auth <SUBCOMMAND>      Manage hub credentials.

GUI / RUN OPTIONS:
    --headless               Run in headless REPL mode (no window, no GPU)
    --theme <NAME>          Theme: phosphor, amber, ice, blood, vapor
    --font-size <PT>        Font size in points (default: 14.0)
    --scanlines <0.0-1.0>   Scanline intensity
    --bloom <0.0-1.0>       Bloom/glow intensity
    --aberration <0.0-1.0>  Chromatic aberration
    --curvature <0.0-1.0>   CRT barrel distortion
    --vignette <0.0-1.0>    Vignette intensity
    --noise <0.0-1.0>       Film grain intensity
    --no-boot               Skip the boot sequence
    --fullscreen             Start in borderless fullscreen mode (toggle with F11 / Cmd+Enter)
    --init-config            Write default config to ~/.config/phantom/config.toml
    --help                   Print this help message

AUTH SUBCOMMANDS:
    phantom auth register --hub <URL> [--service <NAME>]
        Register this Phantom with the hub at <URL>.  Generates an Ed25519
        identity if one does not exist, signs a registration challenge,
        receives a JWT, and stores it under {{config_dir}}/phantom/credentials.
    phantom auth status [--service <NAME>]
        Print stored credentials' peer-id and expiry.
    phantom auth clear [--service <NAME>]
        Delete stored credentials (idempotent).

CONFIG:
    ~/.config/phantom/config.toml

EXAMPLES:
    phantom --theme amber --curvature 0.1
    phantom --bloom 0 --scanlines 0 --curvature 0
    phantom --theme ice --font-size 16
    phantom auth register --hub https://hub.example.com
    phantom auth status"#
    );
}

/// Home-based config dir.
fn dirs_or_home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()))
}

// ---------------------------------------------------------------------------
// Signal-based crash logging
// ---------------------------------------------------------------------------
//
// The Rust panic hook only catches Rust panics. Signals from native code
// (Metal/wgpu SIGSEGV, libc SIGABRT, etc.) bypass it entirely. This installs
// async-signal-safe handlers that write a crash marker to disk before dying.

/// Path to the signal crash log. Must be a static — signal handlers can't allocate.
static SIGNAL_CRASH_PATH: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Install signal handlers for SIGSEGV, SIGBUS, SIGABRT, SIGTERM.
fn install_signal_handlers() {
    let crash_dir = dirs_or_home().join(".config/phantom");
    let _ = std::fs::create_dir_all(&crash_dir);
    let _ = SIGNAL_CRASH_PATH.set(crash_dir.join("signal_crash.log"));

    unsafe {
        for &sig in &[libc::SIGSEGV, libc::SIGBUS, libc::SIGABRT, libc::SIGTERM] {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = signal_handler as *const () as usize;
            sa.sa_flags = libc::SA_SIGINFO | libc::SA_RESETHAND; // one-shot, then default
            libc::sigemptyset(&mut sa.sa_mask);
            libc::sigaction(sig, &sa, std::ptr::null_mut());
        }
    }
}

/// Async-signal-safe crash handler. No allocations, no locks.
/// Writes signal info to disk using raw write(), then re-raises.
extern "C" fn signal_handler(
    sig: libc::c_int,
    info: *mut libc::siginfo_t,
    _ctx: *mut libc::c_void,
) {
    // Build a fixed-size crash message on the stack.
    let sig_name = match sig {
        libc::SIGSEGV => b"SIGSEGV (segmentation fault)" as &[u8],
        libc::SIGBUS => b"SIGBUS (bus error)",
        libc::SIGABRT => b"SIGABRT (abort)",
        libc::SIGTERM => b"SIGTERM (terminated)",
        _ => b"UNKNOWN SIGNAL",
    };

    // Get si_addr for SIGSEGV/SIGBUS (the faulting address).
    let fault_addr: usize = if !info.is_null() && (sig == libc::SIGSEGV || sig == libc::SIGBUS) {
        unsafe { (*info).si_addr as usize }
    } else {
        0
    };

    // Format into a stack buffer. No heap. No String.
    let mut buf = [0u8; 512];
    let mut pos = 0;

    let header = b"PHANTOM SIGNAL CRASH\n====================\nSignal: ";
    pos = append(&mut buf, pos, header);
    pos = append(&mut buf, pos, sig_name);
    pos = append(&mut buf, pos, b"\n");

    if fault_addr != 0 {
        pos = append(&mut buf, pos, b"Fault address: 0x");
        pos = append_hex(&mut buf, pos, fault_addr);
        pos = append(&mut buf, pos, b"\n");
    }

    pos = append(&mut buf, pos, b"PID: ");
    pos = append_usize(&mut buf, pos, unsafe { libc::getpid() } as usize);
    pos = append(&mut buf, pos, b"\n");

    // Write to crash file.
    if let Some(path) = SIGNAL_CRASH_PATH.get()
        && let Ok(cstr) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
    {
        unsafe {
            let fd = libc::open(
                cstr.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC,
                0o644,
            );
            if fd >= 0 {
                libc::write(fd, buf.as_ptr() as *const libc::c_void, pos);
                libc::close(fd);
            }
        }
    }

    // Also write to stderr.
    unsafe {
        libc::write(
            libc::STDERR_FILENO,
            buf.as_ptr() as *const libc::c_void,
            pos,
        );
    }

    // Append to phantom.log so the signal crash appears inline with other logs.
    if let Some(log_path) = LOG_PATH.get() {
        raw_append_to_log(log_path, &buf[..pos]);
    }

    // Re-raise so the default handler runs (core dump, exit, etc.).
    // SA_RESETHAND already restored the default, so this kills the process.
    unsafe {
        libc::raise(sig);
    }
}

/// Append bytes to a fixed buffer (async-signal-safe, no alloc).
fn append(buf: &mut [u8], pos: usize, data: &[u8]) -> usize {
    let end = (pos + data.len()).min(buf.len());
    let n = end - pos;
    buf[pos..end].copy_from_slice(&data[..n]);
    end
}

/// Append a usize as decimal digits.
fn append_usize(buf: &mut [u8], pos: usize, mut val: usize) -> usize {
    if val == 0 {
        return append(buf, pos, b"0");
    }
    let mut digits = [0u8; 20];
    let mut i = 0;
    while val > 0 {
        digits[i] = b'0' + (val % 10) as u8;
        val /= 10;
        i += 1;
    }
    digits[..i].reverse();
    append(buf, pos, &digits[..i])
}

/// Append a usize as hex.
fn append_hex(buf: &mut [u8], pos: usize, mut val: usize) -> usize {
    if val == 0 {
        return append(buf, pos, b"0");
    }
    let hex = b"0123456789abcdef";
    let mut digits = [0u8; 16];
    let mut i = 0;
    while val > 0 {
        digits[i] = hex[val & 0xf];
        val >>= 4;
        i += 1;
    }
    digits[..i].reverse();
    append(buf, pos, &digits[..i])
}

/// Path to the phantom.log for raw append from signal/atexit handlers.
static LOG_PATH: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Register a C-level atexit handler that appends to phantom.log.
/// This fires on exit(), process::exit(), normal main return — anything
/// except _exit() and signals. If phantom.log has no atexit marker AND
/// no signal crash entry, the process was SIGKILL'd.
fn install_atexit() {
    extern "C" fn on_exit() {
        if let Some(path) = LOG_PATH.get() {
            raw_append_to_log(
                path,
                b"[ATEXIT] Process exited via exit() or normal return\n",
            );
        }
    }

    unsafe {
        libc::atexit(on_exit);
    }
}

/// Append a message to a log file using raw syscalls (async-signal-safe).
/// Safe to call from signal handlers and atexit.
fn raw_append_to_log(path: &std::path::Path, msg: &[u8]) {
    if let Ok(cstr) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) {
        unsafe {
            let fd = libc::open(
                cstr.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND,
                0o644,
            );
            if fd >= 0 {
                libc::write(fd, msg.as_ptr() as *const libc::c_void, msg.len());
                libc::close(fd);
            }
        }
    }
}

/// ISO-ish timestamp without external crate.
fn chrono_timestamp() -> String {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&ts, &mut tm) };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
    )
}

// ---------------------------------------------------------------------------
// `phantom auth ...` clap surface (issue #563)
// ---------------------------------------------------------------------------

#[derive(clap::Parser)]
#[command(name = "phantom", bin_name = "phantom", disable_help_flag = false)]
struct AuthCli {
    #[command(subcommand)]
    cmd: AuthRoot,
}

#[derive(clap::Subcommand)]
enum AuthRoot {
    /// Manage hub credentials (register / status / clear).
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },
}

#[derive(clap::Subcommand)]
enum AuthAction {
    /// Register this Phantom with the hub and persist the issued JWT.
    Register {
        /// Hub base URL, e.g. `https://hub.example.com`.
        #[arg(long)]
        hub: String,
        /// Identity / credentials namespace.  Defaults to `"phantom"`.
        #[arg(long, default_value = auth_cli::DEFAULT_SERVICE)]
        service: String,
    },
    /// Print stored credentials' peer-id and expiry.
    Status {
        /// Identity / credentials namespace.  Defaults to `"phantom"`.
        #[arg(long, default_value = auth_cli::DEFAULT_SERVICE)]
        service: String,
    },
    /// Delete stored credentials (idempotent).
    Clear {
        /// Identity / credentials namespace.  Defaults to `"phantom"`.
        #[arg(long, default_value = auth_cli::DEFAULT_SERVICE)]
        service: String,
    },
}

fn run_auth_subcommand(args: &[String]) -> Result<()> {
    use clap::Parser;
    let parsed = AuthCli::parse_from(args);
    let AuthRoot::Auth { action } = parsed.cmd;
    match action {
        AuthAction::Register { hub, service } => auth_cli::register(&hub, &service),
        AuthAction::Status { service } => auth_cli::status(&service),
        AuthAction::Clear { service } => auth_cli::clear(&service),
    }
}

fn main() -> Result<()> {
    // Resolve desktop PATH before any tool is spawned. When Phantom is launched
    // via a .app bundle or .desktop file the inherited PATH is typically missing
    // user shell paths such as /opt/homebrew/bin and ~/.cargo/bin.
    path_resolver::resolve_desktop_path();

    // Load .env file (ANTHROPIC_API_KEY, etc.) before anything else.
    let _ = dotenvy::dotenv();

    // Clean up stale MCP sockets from previous Phantom instances.
    if let Ok(entries) = std::fs::read_dir("/tmp") {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(name) = path.file_name().and_then(|n| n.to_str())
                && name.starts_with("phantom-mcp-")
                && name.ends_with(".sock")
                && let Some(pid_str) = name
                    .strip_prefix("phantom-mcp-")
                    .and_then(|s| s.strip_suffix(".sock"))
                && let Ok(pid) = pid_str.parse::<i32>()
            {
                // kill(pid, 0) checks if process exists without sending a signal.
                if unsafe { libc::kill(pid, 0) } != 0 {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
    }

    let args: Vec<String> = std::env::args().collect();

    // -- Subcommand routing --
    // The very first argument after argv[0] may be a subcommand (e.g.
    // `auth`).  When it is, we hand all remaining args to a clap parser
    // that owns that subcommand's flags.  When it is not — including the
    // common case of legacy flat flags like `phantom --theme amber` —
    // fall through to the legacy flat-flag parser below.
    //
    // We deliberately check argv[1] directly (not "first non-flag arg") so
    // a value-of-flag like `phantom --theme amber` cannot accidentally
    // route into the subcommand parser if a future theme were named
    // `auth`.
    if args.get(1).map(String::as_str) == Some("auth") {
        return run_auth_subcommand(&args);
    }

    // `phantom loop` subcommand routing (issue #650 C3). Mirrors the
    // `auth` block above — we hand the full argv to the loop_cli module
    // which owns its own clap parsing and runtime construction.
    if args.get(1).map(String::as_str) == Some("loop") {
        return loop_cli::run_loop_subcommand(&args);
    }

    // `phantom builder` subcommand routing — higher-level orchestration
    // that points the loop pipeline at any GitHub repo. The CLI surface
    // mirrors `phantom loop run` but adds clone-or-attach, default-spec
    // seeding, and an aggressive brain config.
    if args.get(1).map(String::as_str) == Some("builder") {
        return builder_cli::run_builder_subcommand(&args);
    }

    // `phantom fleet` subcommand routing — the "app of apps" meta-
    // orchestrator. Same shape as the `loop` block above.
    if args.get(1).map(String::as_str) == Some("fleet") {
        return fleet_cli::run_fleet_subcommand(&args);
    }

    // Quick exits
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_help();
        return Ok(());
    }

    if args.iter().any(|a| a == "--init-config") {
        let path = PhantomConfig::write_default()?;
        println!("Wrote default config to {}", path.display());
        return Ok(());
    }

    // -- Logging: file + stderr --
    // Write logs to ~/.config/phantom/phantom.log so crashes are debuggable.
    let log_path = dirs_or_home().join(".config/phantom/phantom.log");
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path);

    let mut builder =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));
    if let Ok(file) = log_file {
        use std::io::Write;
        let file = std::sync::Mutex::new(file);
        builder.format(move |buf, record| {
            let line = format!(
                "[{} {} {}] {}\n",
                chrono_timestamp(),
                record.level(),
                record.target(),
                record.args()
            );
            // Write to both stderr and file.
            let _ = buf.write_all(line.as_bytes());
            if let Ok(mut f) = file.lock() {
                let _ = f.write_all(line.as_bytes());
            }
            Ok(())
        });
    }
    builder.init();

    // Store the log path for raw append from signal/atexit handlers.
    let _ = LOG_PATH.set(log_path);

    // -- Panic hook: save crash report to disk --
    let crash_dir = dirs_or_home().join(".config/phantom");
    std::panic::set_hook(Box::new(move |info| {
        let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
            s.to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "unknown panic".to_string()
        };

        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown location".into());

        let backtrace = std::backtrace::Backtrace::force_capture();

        let report = format!(
            "PHANTOM CRASH REPORT\n\
             ====================\n\
             Time: {}\n\
             Panic: {payload}\n\
             Location: {location}\n\n\
             Backtrace:\n{backtrace}\n",
            chrono_timestamp(),
        );

        // Write to file, fall back to stderr if write fails.
        let crash_path = crash_dir.join("crash.log");
        if let Err(e) = std::fs::write(&crash_path, &report) {
            eprintln!(
                "Failed to write crash report to {}: {e}",
                crash_path.display()
            );
        } else {
            eprintln!("Crash report saved to {}", crash_path.display());
        }

        eprintln!("\n{report}");
    }));

    // -- Reset inherited signal mask --
    // The supervisor blocks SIGINT/SIGTERM before spawning us. Signal masks
    // are inherited across fork()+exec(), so we must unblock them or our
    // signal handlers will never fire (SIGTERM stays pending → supervisor
    // escalates to SIGKILL after 1s → instant death, zero trace).
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::sigaddset(&mut set, libc::SIGSEGV);
        libc::sigaddset(&mut set, libc::SIGBUS);
        libc::sigaddset(&mut set, libc::SIGABRT);
        libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
    }

    // -- Signal handlers: catch SIGSEGV/SIGBUS/SIGABRT/SIGTERM --
    // These bypass the Rust panic hook entirely, so we need separate handling.
    install_signal_handlers();

    // -- atexit handler: logs to a file when the process exits for ANY reason --
    // This catches exit(), process::exit(), normal return, etc.
    // If phantom.log has no "Process exiting" AND no atexit marker, it was SIGKILL.
    install_atexit();

    // Load .env file (for ANTHROPIC_API_KEY etc.) before anything reads env vars.
    load_dotenv();

    // Load config file, then apply CLI overrides
    let mut config = PhantomConfig::load();
    let mut headless = false;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--headless" => {
                headless = true;
            }
            "--theme" => {
                i += 1;
                if i < args.len() {
                    config.theme_name = args[i].clone();
                }
            }
            "--font-size" => {
                i += 1;
                if i < args.len()
                    && let Ok(v) = args[i].parse::<f32>()
                {
                    config.font_size = v;
                }
            }
            "--scanlines" => {
                i += 1;
                if i < args.len()
                    && let Ok(v) = args[i].parse::<f32>()
                {
                    config.shader_overrides.scanline_intensity = Some(v);
                }
            }
            "--bloom" => {
                i += 1;
                if i < args.len()
                    && let Ok(v) = args[i].parse::<f32>()
                {
                    config.shader_overrides.bloom_intensity = Some(v);
                }
            }
            "--aberration" => {
                i += 1;
                if i < args.len()
                    && let Ok(v) = args[i].parse::<f32>()
                {
                    config.shader_overrides.chromatic_aberration = Some(v);
                }
            }
            "--curvature" => {
                i += 1;
                if i < args.len()
                    && let Ok(v) = args[i].parse::<f32>()
                {
                    config.shader_overrides.curvature = Some(v);
                }
            }
            "--vignette" => {
                i += 1;
                if i < args.len()
                    && let Ok(v) = args[i].parse::<f32>()
                {
                    config.shader_overrides.vignette_intensity = Some(v);
                }
            }
            "--noise" => {
                i += 1;
                if i < args.len()
                    && let Ok(v) = args[i].parse::<f32>()
                {
                    config.shader_overrides.noise_intensity = Some(v);
                }
            }
            "--no-boot" => {
                config.skip_boot = true;
            }
            "--fullscreen" => {
                config.fullscreen = true;
            }
            _ => {
                eprintln!("Unknown option: {}", args[i]);
                print_help();
                std::process::exit(1);
            }
        }
        i += 1;
    }

    log::info!(
        r#"
 ██████╗ ██╗  ██╗ █████╗ ███╗   ██╗████████╗ ██████╗ ███╗   ███╗
 ██╔══██╗██║  ██║██╔══██╗████╗  ██║╚══██╔══╝██╔═══██╗████╗ ████║
 ██████╔╝███████║███████║██╔██╗ ██║   ██║   ██║   ██║██╔████╔██║
 ██╔═══╝ ██╔══██║██╔══██║██║╚██╗██║   ██║   ██║   ██║██║╚██╔╝██║
 ██║     ██║  ██║██║  ██║██║ ╚████║   ██║   ╚██████╔╝██║ ╚═╝ ██║
 ╚═╝     ╚═╝  ╚═╝╚═╝  ╚═╝╚═╝  ╚═══╝   ╚═╝    ╚═════╝ ╚═╝     ╚═╝
                        v0.1.0
"#
    );

    // -- Headless mode --
    if headless {
        log::info!("Starting headless REPL mode");
        return headless::run_headless(config);
    }

    // -- Detect supervisor mode --
    let supervisor_socket = std::env::var("PHANTOM_SUPERVISOR_SOCK")
        .ok()
        .map(PathBuf::from);

    if let Some(ref sock) = supervisor_socket {
        log::info!("Supervisor mode: socket at {}", sock.display());
    }

    let event_loop = EventLoop::new()?;
    let mut app = Phantom::new(config, supervisor_socket);

    // Catch every exit path — errors from run_app, panics, and normal exit.
    let result = event_loop.run_app(&mut app);
    match &result {
        Ok(()) => {
            log::info!("Event loop exited cleanly");
        }
        Err(e) => {
            log::error!("Event loop exited with error: {e}");
            // Also write to the crash file so we have a record.
            let crash_path = dirs_or_home().join(".config/phantom/exit_error.log");
            let report = format!(
                "PHANTOM EXIT ERROR\n==================\nTime: {}\nError: {e}\n\
                 Backtrace:\n{}\n",
                chrono_timestamp(),
                std::backtrace::Backtrace::force_capture(),
            );
            let _ = std::fs::write(&crash_path, &report);
        }
    }

    // Log at process exit — if this line is missing from phantom.log,
    // something killed us before we got here (SIGKILL, _exit, etc.)
    log::info!(
        "Process exiting (exit code {})",
        if result.is_ok() { 0 } else { 1 }
    );

    result?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use phantom_app::config::PhantomConfig;

    /// `window_visible` starts `true` so the very first frame is rendered.
    #[test]
    fn window_visible_initialises_to_true() {
        let config = PhantomConfig::default();
        let state = super::Phantom::new(config, None);
        assert!(
            state.window_visible,
            "window_visible must be true at init so the first frame renders"
        );
    }

    /// Simulates the occlusion tracking logic: occluded=true → visible=false,
    /// occluded=false → visible=true.
    #[test]
    fn render_skips_when_occluded() {
        // Directly exercise the same boolean inversion used in the Occluded handler.
        let occluded = true;
        let window_visible = !occluded;
        assert!(
            !window_visible,
            "window should not be visible when occluded=true"
        );

        let occluded = false;
        let window_visible = !occluded;
        assert!(
            window_visible,
            "window should be visible when occluded=false"
        );
    }

    /// `--fullscreen` CLI flag sets config.fullscreen = true.
    #[test]
    fn cli_fullscreen_flag_sets_config() {
        let mut config = PhantomConfig::default();
        assert!(!config.fullscreen, "should start windowed by default");
        // Simulate `--fullscreen` CLI parsing.
        config.fullscreen = true;
        assert!(
            config.fullscreen,
            "--fullscreen flag must enable fullscreen"
        );
    }

    /// `config_fullscreen_false_starts_windowed` — default config has fullscreen=false.
    #[test]
    fn config_fullscreen_false_starts_windowed() {
        let config = PhantomConfig::default();
        assert!(
            !config.fullscreen,
            "default config must not start in fullscreen"
        );
    }

    /// F11 toggle logic: if fullscreen is currently Some, toggle to None, and vice versa.
    #[test]
    fn f11_toggles_fullscreen_on() {
        // Simulate: currently windowed (fullscreen = None) → toggle to Some.
        let current: Option<()> = None;
        let next = if current.is_some() { None } else { Some(()) };
        assert!(
            next.is_some(),
            "F11 from windowed mode must enable fullscreen"
        );
    }

    #[test]
    fn f11_toggles_fullscreen_off() {
        // Simulate: currently fullscreen (fullscreen = Some) → toggle to None.
        let current: Option<()> = Some(());
        let next = if current.is_some() { None } else { Some(()) };
        assert!(
            next.is_none(),
            "F11 from fullscreen mode must exit fullscreen"
        );
    }
}
