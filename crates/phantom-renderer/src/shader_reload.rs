// shader_reload.rs — live WGSL shader reload for debug builds (issue #84).
//
// # Overview
//
// In debug builds only, a background thread watches `shaders/*.wgsl` on disk
// for modifications. When a change is detected the caller is notified via a
// non-blocking channel so it can re-read the file, validate the WGSL with
// `naga`, and rebuild only the affected GPU pipeline.
//
// Release builds compile this file but the watcher thread is never spawned
// and every method is a cheap no-op with zero allocations. The feature-flag
// `live-reload` must be enabled to pull in the `notify` crate; without it
// only the types and stubs are available.
//
// # Atomic swap contract
//
// `ShaderWatcher::try_recv_reload()` returns queued `ShaderKind` values.
// The caller is responsible for:
//   1. Reading the new source from disk.
//   2. Calling `validate_wgsl()` to pre-flight it with naga.
//   3. On success: rebuilding the pipeline and atomically swapping it in.
//   4. On failure: logging the error and keeping the existing pipeline.
//
// Step 4 is the "never crash on bad shader" guarantee — the old pipeline
// keeps running while the developer fixes the syntax error.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};

// ---------------------------------------------------------------------------
// ShaderKind
// ---------------------------------------------------------------------------

/// Which GPU pipeline is associated with a given shader file.
///
/// A [`ShaderWatcher`] emits one of these values each time the corresponding
/// `.wgsl` file changes on disk. The caller uses the variant to decide which
/// pipeline to rebuild.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShaderKind {
    /// `shaders/crt.wgsl` — the CRT post-processing full-screen triangle.
    Crt,
    /// `shaders/text.wgsl` — the glyph instanced-rendering pipeline.
    Text,
}

impl ShaderKind {
    /// Return the relative path (from the workspace root) of the shader file.
    pub fn relative_path(self) -> &'static str {
        match self {
            ShaderKind::Crt => "shaders/crt.wgsl",
            ShaderKind::Text => "shaders/text.wgsl",
        }
    }

    /// Try to identify a `ShaderKind` from an absolute or relative path.
    ///
    /// Returns `None` if the path doesn't correspond to a known shader.
    pub fn from_path(path: &std::path::Path) -> Option<Self> {
        let file_name = path.file_name()?.to_str()?;
        match file_name {
            "crt.wgsl" => Some(ShaderKind::Crt),
            "text.wgsl" => Some(ShaderKind::Text),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// WGSL validation (naga — CPU-only, no GPU needed)
// ---------------------------------------------------------------------------

/// Validate a WGSL source string with naga's full IR validator.
///
/// Returns `Ok(())` if the shader parses and validates correctly, or an
/// `Err(String)` with the human-readable parse/validation error. No GPU
/// device is required.
///
/// This is called before handing a reloaded shader source to
/// `wgpu::Device::create_shader_module` so that a syntax error surfaces as a
/// log message rather than a GPU driver panic or a silent black screen.
///
/// Only available when the `live-reload` feature is enabled.
#[cfg(feature = "live-reload")]
pub fn validate_wgsl(source: &str) -> Result<(), String> {
    let module = naga::front::wgsl::parse_str(source).map_err(|e| e.emit_to_string(source))?;

    let mut validator = naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::empty(),
    );

    validator
        .validate(&module)
        .map_err(|e| format!("{e:?}"))?;

    Ok(())
}

// ---------------------------------------------------------------------------
// ShaderWatcher
// ---------------------------------------------------------------------------

/// Watches `shaders/*.wgsl` for on-disk modifications in debug builds.
///
/// Create one instance at startup with [`ShaderWatcher::new`]; call
/// [`try_recv_reload`](Self::try_recv_reload) once per frame (or event loop
/// iteration) to drain any pending reload notifications.
///
/// In release builds, or when the `live-reload` cargo feature is not enabled,
/// `new()` returns a no-op watcher and `try_recv_reload()` always returns
/// `None`.
pub struct ShaderWatcher {
    receiver: Receiver<ShaderKind>,
    /// Keeps the watcher alive for the lifetime of this struct.
    /// The field is never read — it exists only so the watcher thread is
    /// joined when `ShaderWatcher` is dropped.
    #[allow(dead_code)]
    _handle: Option<WatcherHandle>,
}

/// RAII handle that keeps a `notify` watcher alive.
///
/// When dropped the watcher is stopped and the background thread exits.
struct WatcherHandle {
    #[cfg(all(debug_assertions, feature = "live-reload"))]
    _watcher: notify::RecommendedWatcher,
}

impl ShaderWatcher {
    /// Create a new shader watcher.
    ///
    /// In **debug builds** with the `live-reload` feature:
    /// - Spawns a [`notify::RecommendedWatcher`] that monitors `shaders/`.
    /// - Changes to `crt.wgsl` or `text.wgsl` are forwarded through an
    ///   internal channel and surfaced via [`try_recv_reload`](Self::try_recv_reload).
    ///
    /// In **release builds** or without the feature:
    /// - Returns immediately with no background thread and no allocations
    ///   beyond the single channel endpoint pair.
    ///
    /// `shaders_dir` should be an absolute path to the directory containing
    /// the `.wgsl` files (typically `<workspace-root>/shaders`).
    pub fn new(shaders_dir: PathBuf) -> Self {
        let (sender, receiver) = mpsc::channel::<ShaderKind>();

        let handle = Self::spawn_watcher(shaders_dir, sender);

        Self {
            receiver,
            _handle: handle,
        }
    }

    /// Pop the next pending reload notification, if any.
    ///
    /// Returns the next queued [`ShaderKind`], or `None` if nothing changed.
    /// Call in a loop once per render frame to drain all FS events that
    /// arrived since the last frame:
    ///
    /// ```ignore
    /// while let Some(kind) = watcher.try_recv_reload() {
    ///     // rebuild pipeline for `kind`
    /// }
    /// ```
    ///
    /// In release builds or without `live-reload` this always returns `None`.
    pub fn try_recv_reload(&self) -> Option<ShaderKind> {
        self.receiver.try_recv().ok()
    }

    /// Rebuild the pipeline for `kind` if the new source validates, or log an
    /// error and return `None` to signal the caller should keep the old pipeline.
    ///
    /// # Arguments
    /// * `kind`       — which pipeline needs rebuilding.
    /// * `shaders_dir` — absolute path to the `shaders/` directory.
    ///
    /// # Returns
    /// * `Some(source)` — validated WGSL source; the caller should rebuild the
    ///   corresponding pipeline with this string.
    /// * `None`          — the file couldn't be read or failed naga validation;
    ///   the old pipeline should remain in use.
    ///
    /// Only available when the `live-reload` feature is enabled.
    #[cfg(feature = "live-reload")]
    pub fn try_reload_source(kind: ShaderKind, shaders_dir: &std::path::Path) -> Option<String> {
        let path = shaders_dir.join(kind.relative_path().trim_start_matches("shaders/"));
        let source = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                log::error!(
                    "[shader reload] failed to read {:?}: {}",
                    path.file_name().unwrap_or_default(),
                    e
                );
                return None;
            }
        };

        match validate_wgsl(&source) {
            Ok(()) => {
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("unknown");
                log::info!("[shader reload] {} reloaded", name);
                Some(source)
            }
            Err(msg) => {
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("unknown");
                log::error!("[shader reload] {} compile error — keeping old pipeline:\n{}", name, msg);
                None
            }
        }
    }

    // -----------------------------------------------------------------------
    // Internal: spawn the notify watcher (debug + live-reload only)
    // -----------------------------------------------------------------------

    #[cfg(all(debug_assertions, feature = "live-reload"))]
    fn spawn_watcher(shaders_dir: PathBuf, sender: Sender<ShaderKind>) -> Option<WatcherHandle> {
        use notify::{EventKind, RecursiveMode, Watcher};

        let mut watcher = match notify::RecommendedWatcher::new(
            move |result: notify::Result<notify::Event>| {
                let event = match result {
                    Ok(e) => e,
                    Err(e) => {
                        log::warn!("[shader reload] watcher error: {}", e);
                        return;
                    }
                };

                // Only react to actual content-modification events.
                let is_modify = matches!(
                    event.kind,
                    EventKind::Modify(_) | EventKind::Create(_)
                );
                if !is_modify {
                    return;
                }

                for path in &event.paths {
                    if let Some(kind) = ShaderKind::from_path(path) {
                        // Ignore send errors — the receiver may have been dropped.
                        let _ = sender.send(kind);
                    }
                }
            },
            notify::Config::default(),
        ) {
            Ok(w) => w,
            Err(e) => {
                log::warn!("[shader reload] could not create file watcher: {}", e);
                return None;
            }
        };

        if let Err(e) = watcher.watch(&shaders_dir, RecursiveMode::NonRecursive) {
            log::warn!("[shader reload] could not watch {:?}: {}", shaders_dir, e);
            return None;
        }

        log::debug!(
            "[shader reload] watching {:?} for WGSL changes",
            shaders_dir
        );

        Some(WatcherHandle { _watcher: watcher })
    }

    /// No-op stub for release builds or when `live-reload` is not enabled.
    #[cfg(not(all(debug_assertions, feature = "live-reload")))]
    fn spawn_watcher(_shaders_dir: PathBuf, _sender: Sender<ShaderKind>) -> Option<WatcherHandle> {
        None
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    // -----------------------------------------------------------------------
    // (a) Watcher fires on file change (mock the FS event via the raw channel)
    // -----------------------------------------------------------------------

    /// Test that `ShaderKind::from_path` correctly classifies known shader paths,
    /// and that a mock FS-event delivered directly to the channel surfaces
    /// through `try_recv_reload`.
    ///
    /// We mock the file-system event by constructing the internal channel
    /// ourselves and sending a `ShaderKind` directly — this isolates the
    /// notification plumbing from the real OS file-watcher (which requires
    /// privileged FS access and is nondeterministic in tests).
    #[test]
    fn watcher_fires_on_mock_fs_event() {
        // Build the channel pair the same way ShaderWatcher does internally.
        let (tx, rx) = mpsc::channel::<ShaderKind>();

        // Simulate what the notify callback does when it sees a modify event
        // for shaders/crt.wgsl.
        let mock_path = std::path::Path::new("shaders/crt.wgsl");
        let kind = ShaderKind::from_path(mock_path).expect("crt.wgsl must map to ShaderKind::Crt");
        assert_eq!(kind, ShaderKind::Crt);
        tx.send(kind).unwrap();

        // Construct a watcher with the pre-seeded receiver.
        let watcher = ShaderWatcher {
            receiver: rx,
            _handle: None,
        };

        // The first call must yield the queued event.
        let received = watcher.try_recv_reload();
        assert_eq!(received, Some(ShaderKind::Crt));

        // Subsequent call must be empty — channel is drained.
        assert_eq!(watcher.try_recv_reload(), None);
    }

    /// Same test for the text shader path.
    #[test]
    fn watcher_fires_for_text_wgsl() {
        let (tx, rx) = mpsc::channel::<ShaderKind>();
        let mock_path = std::path::Path::new("/abs/shaders/text.wgsl");
        let kind = ShaderKind::from_path(mock_path).unwrap();
        assert_eq!(kind, ShaderKind::Text);
        tx.send(kind).unwrap();

        let watcher = ShaderWatcher {
            receiver: rx,
            _handle: None,
        };
        assert_eq!(watcher.try_recv_reload(), Some(ShaderKind::Text));
    }

    /// Unrecognised `.wgsl` files must not produce a `ShaderKind`.
    #[test]
    fn from_path_returns_none_for_unknown_shader() {
        let path = std::path::Path::new("shaders/unknown_effect.wgsl");
        assert!(ShaderKind::from_path(path).is_none());
    }

    /// Non-wgsl paths also return `None`.
    #[test]
    fn from_path_returns_none_for_non_wgsl() {
        assert!(ShaderKind::from_path(std::path::Path::new("src/main.rs")).is_none());
    }

    // -----------------------------------------------------------------------
    // (b) Compile failure falls back gracefully — never crashes
    // -----------------------------------------------------------------------

    /// `try_reload_source` must return `None` and NOT panic when given a
    /// directory containing a syntactically invalid WGSL file.
    #[cfg(feature = "live-reload")]
    #[test]
    fn compile_failure_falls_back_gracefully() {
        use std::io::Write;

        let dir = tempfile::tempdir().unwrap();
        let shader_path = dir.path().join("crt.wgsl");

        // Write deliberately broken WGSL (missing semicolon, invalid token).
        let broken_wgsl = r#"
            @vertex
            fn vs_main() -> @builtin(position) vec4<f32> {
                // This line intentionally does not compile
                let x: i32 = "not a number"
                return vec4<f32>(0.0, 0.0, 0.0, 1.0);
            }
        "#;
        std::fs::File::create(&shader_path)
            .unwrap()
            .write_all(broken_wgsl.as_bytes())
            .unwrap();

        // Must return None (no crash, no panic, no GPU pipeline submission).
        let result = ShaderWatcher::try_reload_source(ShaderKind::Crt, dir.path());
        assert!(
            result.is_none(),
            "broken WGSL must return None, not panic or succeed"
        );
    }

    /// `try_reload_source` must return `None` when the file does not exist,
    /// rather than panicking on an I/O error.
    #[cfg(feature = "live-reload")]
    #[test]
    fn missing_file_falls_back_gracefully() {
        let dir = tempfile::tempdir().unwrap();
        // No crt.wgsl written — file is absent.
        let result = ShaderWatcher::try_reload_source(ShaderKind::Crt, dir.path());
        assert!(
            result.is_none(),
            "missing file must return None, not panic"
        );
    }

    /// `validate_wgsl` accepts valid WGSL and rejects invalid WGSL correctly.
    #[cfg(feature = "live-reload")]
    #[test]
    fn validate_wgsl_accepts_valid_and_rejects_invalid() {
        // Minimal valid WGSL vertex shader.
        let valid = r#"
            @vertex
            fn vs_main(@builtin(vertex_index) vi: u32) -> @builtin(position) vec4<f32> {
                return vec4<f32>(0.0, 0.0, 0.0, 1.0);
            }
        "#;
        assert!(
            validate_wgsl(valid).is_ok(),
            "valid WGSL must pass validation"
        );

        // Invalid WGSL — undeclared identifier.
        let invalid = "fn broken() { let x = totally_nonexistent_function(); }";
        assert!(
            validate_wgsl(invalid).is_err(),
            "invalid WGSL must fail validation"
        );
    }

    /// `ShaderKind::relative_path` returns stable values — if these change, the
    /// watcher will silently stop mapping paths to kinds.
    #[test]
    fn shader_kind_relative_paths_are_stable() {
        assert_eq!(ShaderKind::Crt.relative_path(), "shaders/crt.wgsl");
        assert_eq!(ShaderKind::Text.relative_path(), "shaders/text.wgsl");
    }

    /// Multiple events in the channel are drained in order.
    #[test]
    fn multiple_events_drained_in_order() {
        let (tx, rx) = mpsc::channel::<ShaderKind>();
        tx.send(ShaderKind::Crt).unwrap();
        tx.send(ShaderKind::Text).unwrap();

        let watcher = ShaderWatcher {
            receiver: rx,
            _handle: None,
        };

        assert_eq!(watcher.try_recv_reload(), Some(ShaderKind::Crt));
        assert_eq!(watcher.try_recv_reload(), Some(ShaderKind::Text));
        assert_eq!(watcher.try_recv_reload(), None);
    }

    /// `validate_wgsl` on the real embedded CRT shader must pass — guards
    /// against the embedded shader going stale relative to naga's validator.
    #[cfg(feature = "live-reload")]
    #[test]
    fn embedded_crt_shader_passes_validation() {
        use crate::postfx::CRT_WGSL;
        assert!(
            validate_wgsl(CRT_WGSL).is_ok(),
            "embedded crt.wgsl must pass naga validation"
        );
    }
}
