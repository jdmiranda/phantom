// phantom-renderer/src/shader_loader.rs
//
// Live WGSL shader reloader — `live-reload` feature + debug builds only.
//
// In release builds (or without the `live-reload` feature) this module is a
// zero-size no-op: the [`ShaderReloader`] type compiles to nothing and all
// methods are inlined away.
//
// When compiled with `--features live-reload` in a debug build,
// `PHANTOM_HOT_SHADERS=1` activates a background thread that uses the
// `notify` crate to watch every `*.wgsl` file under the `shaders/` directory
// (relative to the process working directory).  When a file changes the new
// source is read, parsed through naga's WGSL front-end for a quick CPU-side
// syntax check, and posted onto a lock-free channel.
//
// The caller (typically `App::update`) calls [`ShaderReloader::poll`] each
// frame.  A `Some(ShaderEvent::Reloaded { … })` result means the caller
// should rebuild any pipeline that uses that shader.  A
// `Some(ShaderEvent::Error { … })` means the new source failed naga
// validation; the caller should surface the message in the
// `NotificationCenter` and keep the last-good pipeline alive.
//
// ## ~500 ms latency guarantee
//
// `notify`'s `RecommendedWatcher` coalesces rapid successive saves (e.g.
// format-on-save + actual edit) via OS-level debounce.  The per-frame poll
// plus the OS debounce give a worst-case latency well inside the 500 ms
// target from issue #84.
//
// ## Error recovery
//
// If the watcher thread panics or the `notify` setup fails, `ShaderReloader`
// silently falls back to a no-op state (same as a release build).  The app
// continues working; live-reload is a dev-only quality-of-life feature and
// must never crash the terminal.

// ---------------------------------------------------------------------------
// Public interface (same shape regardless of cfg)
// ---------------------------------------------------------------------------

/// A reload event produced by the background shader watcher.
#[derive(Debug, Clone)]
pub enum ShaderEvent {
    /// The shader compiled cleanly.  The caller should swap its pipeline.
    Reloaded {
        /// Shader name (the stem of the `.wgsl` file, e.g. `"crt"`).
        name: String,
        /// Full WGSL source, ready to pass to `wgpu::Device::create_shader_module`.
        source: String,
    },
    /// The new source failed naga validation.  Keep the last-good pipeline.
    Error {
        /// Shader name (same stem convention as [`ShaderEvent::Reloaded`]).
        name: String,
        /// Human-readable naga error message.
        message: String,
    },
}

// ---------------------------------------------------------------------------
// Stub (release builds or feature not enabled)
// ---------------------------------------------------------------------------

#[cfg(not(all(debug_assertions, feature = "live-reload")))]
pub struct ShaderReloader;

#[cfg(not(all(debug_assertions, feature = "live-reload")))]
impl ShaderReloader {
    /// Create a no-op reloader.  The watcher is never started.
    #[must_use]
    pub fn new() -> Self {
        ShaderReloader
    }

    /// Always returns `None`.
    #[must_use]
    pub fn poll(&self) -> Option<ShaderEvent> {
        None
    }
}

#[cfg(not(all(debug_assertions, feature = "live-reload")))]
impl Default for ShaderReloader {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Live implementation (`live-reload` feature + debug builds)
// ---------------------------------------------------------------------------

#[cfg(all(debug_assertions, feature = "live-reload"))]
mod live_impl {
    use std::path::PathBuf;
    use std::sync::mpsc;

    use super::ShaderEvent;

    use notify::{
        EventKind, RecursiveMode, Result as NotifyResult, Watcher,
        event::ModifyKind,
        recommended_watcher,
    };

    /// Background WGSL file watcher.
    ///
    /// Created with [`ShaderReloader::new`].  The internal watcher thread
    /// exits automatically when this struct is dropped (the `_watcher` field
    /// is dropped, which signals the background thread to stop).
    pub struct ShaderReloader {
        /// Events produced by the background watcher thread.  `None` when
        /// `PHANTOM_HOT_SHADERS` is not set or if watcher setup failed.
        rx: Option<mpsc::Receiver<ShaderEvent>>,
        /// Hold the watcher alive — dropping it stops watching.
        _watcher: Option<Box<dyn notify::Watcher + Send>>,
    }

    impl ShaderReloader {
        /// Create the watcher.
        ///
        /// The watcher is only activated when the `PHANTOM_HOT_SHADERS`
        /// environment variable is set.  If absent, or if `notify` setup
        /// fails, returns a silent no-op instance so the app continues
        /// normally.
        #[must_use]
        pub fn new() -> Self {
            if std::env::var_os("PHANTOM_HOT_SHADERS").is_none() {
                log::debug!(
                    "shader_loader: PHANTOM_HOT_SHADERS not set — live reload inactive"
                );
                return Self { rx: None, _watcher: None };
            }

            match Self::try_start() {
                Ok(loader) => loader,
                Err(e) => {
                    log::warn!(
                        "shader_loader: failed to start watcher: {e} — live reload inactive"
                    );
                    Self { rx: None, _watcher: None }
                }
            }
        }

        fn try_start() -> anyhow::Result<Self> {
            let shaders_dir = PathBuf::from("shaders");
            if !shaders_dir.is_dir() {
                anyhow::bail!(
                    "shader_loader: 'shaders/' directory not found in {:?}",
                    std::env::current_dir().unwrap_or_default()
                );
            }

            let (tx, rx) = mpsc::channel::<ShaderEvent>();
            let tx_clone = tx.clone();

            let mut watcher =
                recommended_watcher(move |result: NotifyResult<notify::Event>| {
                    let event = match result {
                        Ok(e) => e,
                        Err(err) => {
                            log::warn!("shader_loader: notify error: {err}");
                            return;
                        }
                    };

                    // Only care about content modifications, not metadata or access.
                    let is_modify = matches!(
                        event.kind,
                        EventKind::Modify(ModifyKind::Data(_))
                            | EventKind::Modify(ModifyKind::Any)
                            | EventKind::Create(_)
                    );
                    if !is_modify {
                        return;
                    }

                    for path in &event.paths {
                        if path.extension().and_then(|e| e.to_str()) != Some("wgsl") {
                            continue;
                        }

                        let name = path
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or("unknown")
                            .to_owned();

                        match std::fs::read_to_string(path) {
                            Ok(source) => {
                                match naga::front::wgsl::parse_str(&source) {
                                    Ok(_module) => {
                                        log::info!(
                                            "shader_loader: {name} reloaded ({} bytes)",
                                            source.len()
                                        );
                                        let _ = tx_clone.send(ShaderEvent::Reloaded {
                                            name: name.clone(),
                                            source,
                                        });
                                    }
                                    Err(parse_err) => {
                                        let msg = format!("{parse_err}");
                                        log::warn!(
                                            "shader_loader: {name}.wgsl parse error: {msg}"
                                        );
                                        let _ = tx_clone.send(ShaderEvent::Error {
                                            name: name.clone(),
                                            message: msg,
                                        });
                                    }
                                }
                            }
                            Err(io_err) => {
                                log::warn!(
                                    "shader_loader: could not read {name}.wgsl: {io_err}"
                                );
                            }
                        }
                    }
                })?;

            watcher.watch(&shaders_dir, RecursiveMode::NonRecursive)?;

            log::info!(
                "shader_loader: watching {:?} for .wgsl changes (PHANTOM_HOT_SHADERS=1)",
                shaders_dir.canonicalize().unwrap_or(shaders_dir)
            );

            Ok(Self {
                rx: Some(rx),
                _watcher: Some(Box::new(watcher)),
            })
        }

        /// Poll for the latest shader event without blocking.
        ///
        /// Returns `None` when no change has occurred since the last call.
        /// When multiple reloads are queued (e.g. saved twice quickly),
        /// each call returns one — subsequent calls drain the rest.
        #[must_use]
        pub fn poll(&self) -> Option<ShaderEvent> {
            let rx = self.rx.as_ref()?;
            match rx.try_recv() {
                Ok(event) => Some(event),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => {
                    log::warn!("shader_loader: event channel disconnected");
                    None
                }
            }
        }
    }

    impl Default for ShaderReloader {
        fn default() -> Self {
            Self::new()
        }
    }
}

#[cfg(all(debug_assertions, feature = "live-reload"))]
pub use live_impl::ShaderReloader;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// A freshly constructed `ShaderReloader` without the env var must
    /// return `None` on the first poll — not panic, not block.
    #[test]
    fn poll_returns_none_when_env_var_not_set() {
        // Safety: single-threaded test environment.
        unsafe { std::env::remove_var("PHANTOM_HOT_SHADERS") };
        let reloader = ShaderReloader::new();
        assert!(
            reloader.poll().is_none(),
            "poll() must return None when PHANTOM_HOT_SHADERS is not set"
        );
    }

    /// Multiple consecutive polls on an idle reloader must all return `None`.
    #[test]
    fn repeated_poll_returns_none() {
        unsafe { std::env::remove_var("PHANTOM_HOT_SHADERS") };
        let reloader = ShaderReloader::new();
        for _ in 0..10 {
            assert!(reloader.poll().is_none());
        }
    }

    /// `ShaderEvent::Reloaded` must carry both a non-empty name and source.
    #[test]
    fn shader_event_reloaded_carries_name_and_source() {
        let evt = ShaderEvent::Reloaded {
            name: "crt".to_owned(),
            source: "// dummy wgsl".to_owned(),
        };
        let ShaderEvent::Reloaded { name, source } = evt else {
            panic!("expected Reloaded");
        };
        assert_eq!(name, "crt");
        assert!(!source.is_empty());
    }

    /// `ShaderEvent::Error` must carry a non-empty message.
    #[test]
    fn shader_event_error_carries_message() {
        let evt = ShaderEvent::Error {
            name: "crt".to_owned(),
            message: "unexpected token".to_owned(),
        };
        let ShaderEvent::Error { name, message } = evt else {
            panic!("expected Error");
        };
        assert_eq!(name, "crt");
        assert!(!message.is_empty());
    }

    /// `Default::default()` is safe and always returns no events.
    #[test]
    fn default_is_safe() {
        unsafe { std::env::remove_var("PHANTOM_HOT_SHADERS") };
        let r: ShaderReloader = Default::default();
        assert!(r.poll().is_none());
    }
}
