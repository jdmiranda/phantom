//! Channel-tagged logging with file mirror and panic flush.
//!
//! Each Phantom subsystem logs to a named channel. Channels can be
//! toggled on/off at runtime. All messages mirror to a log file.
//! On panic, the file is flushed and a crash report is written.

use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};
use std::sync::Mutex;

use bitflags::bitflags;

bitflags! {
    /// Per-subsystem channel bitmask.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Channels: u32 {
        const RENDERER    = 1 << 0;
        const SHADER      = 1 << 1;
        const TERMINAL    = 1 << 2;
        const ADAPTER     = 1 << 3;
        const COORDINATOR = 1 << 4;
        const SCENE       = 1 << 5;
        const SEMANTIC    = 1 << 6;
        const NLP         = 1 << 7;
        const BRAIN       = 1 << 8;
        const SUPERVISOR  = 1 << 9;
        const AGENTS      = 1 << 10;
        const MCP         = 1 << 11;
        const PLUGINS     = 1 << 12;
        const MEMORY      = 1 << 13;
        const CONTEXT     = 1 << 14;
        const SESSION     = 1 << 15;
        const BOOT        = 1 << 16;
        const INPUT       = 1 << 17;
        const FX          = 1 << 18;
        const PROFILER    = 1 << 19;
        const ALL         = u32::MAX;
    }
}

/// Maps a log target string to a channel bit.
pub fn channel_for_target(target: &str) -> Channels {
    // Extract the subsystem name from targets like "phantom::brain" or "phantom_brain"
    let name = target
        .strip_prefix("phantom::")
        .or_else(|| target.strip_prefix("phantom_"))
        .unwrap_or(target);

    match name {
        "renderer" => Channels::RENDERER,
        "shader" | "shaders" => Channels::SHADER,
        "terminal" => Channels::TERMINAL,
        "adapter" => Channels::ADAPTER,
        "coordinator" | "coord" => Channels::COORDINATOR,
        "scene" => Channels::SCENE,
        "semantic" => Channels::SEMANTIC,
        "nlp" => Channels::NLP,
        "brain" => Channels::BRAIN,
        "supervisor" => Channels::SUPERVISOR,
        "agents" | "agent" => Channels::AGENTS,
        "mcp" => Channels::MCP,
        "plugins" | "plugin" => Channels::PLUGINS,
        "memory" => Channels::MEMORY,
        "context" => Channels::CONTEXT,
        "session" => Channels::SESSION,
        "boot" => Channels::BOOT,
        "input" => Channels::INPUT,
        "fx" | "effects" => Channels::FX,
        "profiler" | "profile" => Channels::PROFILER,
        _ => Channels::BOOT, // unknown targets route to BOOT (general/uncategorized)
    }
}

/// Verbosity levels (0=error only ... 4=trace).
pub const VERBOSITY_ERROR: u8 = 0;
pub const VERBOSITY_WARN: u8 = 1;
pub const VERBOSITY_INFO: u8 = 2;
pub const VERBOSITY_DEBUG: u8 = 3;
pub const VERBOSITY_TRACE: u8 = 4;

/// The Phantom logger.
pub struct PhantomLogger {
    active_channels: AtomicU32,
    verbosity: AtomicU8,
    file: Mutex<Option<File>>,
    stderr: bool,
    file_write_errors: AtomicU32,
}

impl PhantomLogger {
    /// Create a new logger, optionally mirroring to a file in `log_dir`.
    pub fn new(log_dir: Option<PathBuf>, stderr: bool) -> Self {
        let file = log_dir.and_then(|dir| {
            fs::create_dir_all(&dir).ok()?;
            let timestamp = chrono_free_timestamp();
            let path = dir.join(format!("phantom-{timestamp}.log"));
            File::create(path).ok()
        });

        Self {
            active_channels: AtomicU32::new(Channels::ALL.bits()),
            verbosity: AtomicU8::new(VERBOSITY_INFO),
            file: Mutex::new(file),
            stderr,
            file_write_errors: AtomicU32::new(0),
        }
    }

    /// Enable/disable a specific channel at runtime.
    pub fn set_channel(&self, channel: Channels, enabled: bool) {
        if enabled {
            self.active_channels
                .fetch_or(channel.bits(), Ordering::Relaxed);
        } else {
            self.active_channels
                .fetch_and(!channel.bits(), Ordering::Relaxed);
        }
    }

    /// Set verbosity level (0-4).
    pub fn set_verbosity(&self, level: u8) {
        self.verbosity
            .store(level.min(VERBOSITY_TRACE), Ordering::Relaxed);
    }

    /// Get current active channels bitmask.
    pub fn active_channels(&self) -> Channels {
        Channels::from_bits_truncate(self.active_channels.load(Ordering::Relaxed))
    }

    /// Get current verbosity level.
    pub fn verbosity(&self) -> u8 {
        self.verbosity.load(Ordering::Relaxed)
    }

    /// Flush the log file. Recovers from poisoned mutex so that panic
    /// hooks can still flush even if the panic occurred while holding the lock.
    pub fn flush(&self) {
        let mut guard = match self.file.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(f) = guard.as_mut() {
            let _ = f.flush();
        }
    }
}

fn chrono_free_timestamp() -> String {
    use std::time::SystemTime;
    let d = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}", d.as_secs())
}

fn level_to_verbosity(level: log::Level) -> u8 {
    match level {
        log::Level::Error => VERBOSITY_ERROR,
        log::Level::Warn => VERBOSITY_WARN,
        log::Level::Info => VERBOSITY_INFO,
        log::Level::Debug => VERBOSITY_DEBUG,
        log::Level::Trace => VERBOSITY_TRACE,
    }
}

impl log::Log for PhantomLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        let verbosity = self.verbosity.load(Ordering::Relaxed);
        if level_to_verbosity(metadata.level()) > verbosity {
            return false;
        }
        let channel = channel_for_target(metadata.target());
        let active = Channels::from_bits_truncate(self.active_channels.load(Ordering::Relaxed));
        active.intersects(channel)
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }

        let msg = format!(
            "[{level}][{target}] {msg}",
            level = record.level(),
            target = record.target(),
            msg = record.args(),
        );

        // Write to file
        if let Ok(mut file) = self.file.lock() {
            if let Some(f) = file.as_mut() {
                if let Err(_e) = writeln!(f, "{msg}") {
                    let prev = self.file_write_errors.fetch_add(1, Ordering::Relaxed);
                    if prev == 0 {
                        eprintln!("[phantom-logger] log file write failed — subsequent errors will be silent");
                    }
                }
            }
        }

        // Write to stderr if enabled
        if self.stderr {
            eprintln!("{msg}");
        }
    }

    fn flush(&self) {
        PhantomLogger::flush(self);
    }
}

/// Install the PhantomLogger as the global logger.
///
/// Returns a reference for runtime channel/verbosity control, or an error
/// if a logger has already been set.
pub fn install(
    log_dir: Option<PathBuf>,
    stderr: bool,
) -> Result<&'static PhantomLogger, log::SetLoggerError> {
    let logger = Box::new(PhantomLogger::new(log_dir, stderr));
    let logger_ref: &'static PhantomLogger = Box::leak(logger);
    log::set_logger(logger_ref)?;
    log::set_max_level(log::LevelFilter::Trace);
    Ok(logger_ref)
}

/// Install a panic hook that flushes the log file.
pub fn install_panic_hook(logger: &'static PhantomLogger) {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let msg = if let Some(s) = info.payload().downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "unknown panic".to_string()
        };

        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_default();

        log::error!(target: "phantom::boot", "PANIC at {location}: {msg}");
        logger.flush();

        original(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use log::Log;

    #[test]
    fn channel_for_known_targets() {
        assert_eq!(channel_for_target("phantom::brain"), Channels::BRAIN);
        assert_eq!(channel_for_target("phantom_renderer"), Channels::RENDERER);
        assert_eq!(channel_for_target("phantom::mcp"), Channels::MCP);
    }

    #[test]
    fn channel_for_unknown_target_passes() {
        assert_eq!(channel_for_target("unknown_crate"), Channels::BOOT);
    }

    #[test]
    fn verbosity_filtering() {
        let logger = PhantomLogger::new(None, false);
        logger.set_verbosity(VERBOSITY_WARN);

        // Info should be filtered out (verbosity 2 > threshold 1)
        let meta = log::MetadataBuilder::new()
            .level(log::Level::Info)
            .target("phantom::brain")
            .build();
        assert!(!logger.enabled(&meta));

        // Warn should pass
        let meta = log::MetadataBuilder::new()
            .level(log::Level::Warn)
            .target("phantom::brain")
            .build();
        assert!(logger.enabled(&meta));
    }

    #[test]
    fn channel_filtering() {
        let logger = PhantomLogger::new(None, false);
        logger.set_verbosity(VERBOSITY_TRACE);
        logger.set_channel(Channels::BRAIN, false);

        let meta = log::MetadataBuilder::new()
            .level(log::Level::Info)
            .target("phantom::brain")
            .build();
        assert!(!logger.enabled(&meta));

        // Renderer should still pass
        let meta = log::MetadataBuilder::new()
            .level(log::Level::Info)
            .target("phantom::renderer")
            .build();
        assert!(logger.enabled(&meta));
    }

    #[test]
    fn runtime_channel_toggle() {
        let logger = PhantomLogger::new(None, false);
        logger.set_channel(Channels::MCP, false);
        assert!(!logger.active_channels().contains(Channels::MCP));
        logger.set_channel(Channels::MCP, true);
        assert!(logger.active_channels().contains(Channels::MCP));
    }

    #[test]
    fn file_mirror_to_tempdir() {
        let dir = std::env::temp_dir().join("phantom-test-log");
        let _ = std::fs::remove_dir_all(&dir);

        let logger = PhantomLogger::new(Some(dir.clone()), false);
        logger.set_verbosity(VERBOSITY_TRACE);

        // Use the log trait directly
        let record = log::RecordBuilder::new()
            .level(log::Level::Info)
            .target("phantom::boot")
            .args(format_args!("test message"))
            .build();
        log::Log::log(&logger, &record);
        logger.flush();

        // Check a file was created
        let entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(entries.len(), 1);

        let content = std::fs::read_to_string(entries[0].path()).unwrap();
        assert!(content.contains("test message"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
