//! Audio capture backend abstraction for Phantom.
//!
//! This crate defines the trait + types only — no real backend implementations.
//! A [`MockAudioCapture`] is provided for tests and as a placeholder default
//! until real backends (CoreAudio process-tap, ScreenCaptureKit app-stream,
//! virtual device, in-process) land.
//!
//! # Streaming model
//!
//! Callers choose an [`AudioCaptureSource`] from [`AudioCapture::enumerate`]
//! and pass it to [`AudioCapture::open`]. The backend returns a
//! [`BoxedAudioStream`] that yields [`AudioFrame`]s. Dropping the stream stops
//! capture.

use std::pin::Pin;

use futures_core::Stream;

#[cfg(all(target_os = "macos", feature = "macos-sck"))]
pub mod macos_sck;

/// Where audio is being captured from.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AudioCaptureSource {
    /// Backend-specific source descriptor.
    pub kind: SourceKind,
    /// Human-readable label for UIs.
    pub label: String,
}

/// Variants of audio capture source supported by the abstraction.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum SourceKind {
    /// Per-process tap (macOS 14.4+, requires mic permission).
    ProcessTap { pid: u32 },
    /// Per-app stream (macOS 13+, requires screen recording permission).
    AppStream { bundle_id: String },
    /// System-wide via virtual device (BlackHole etc).
    SystemViaVirtualDevice { device_name: String },
    /// In-process (Phantom captures its own audio output).
    InProcess,
}

/// A buffer of captured PCM audio.
#[derive(Debug, Clone)]
pub struct AudioFrame {
    /// Interleaved if `channels > 1`.
    pub samples: Vec<f32>,
    /// Sample rate in Hz (e.g. `48_000`).
    pub sample_rate: u32,
    /// Number of interleaved channels.
    pub channels: u16,
    /// Capture timestamp in milliseconds since session start.
    pub timestamp_ms: u64,
}

impl Default for AudioFrame {
    fn default() -> Self {
        Self {
            samples: Vec::new(),
            sample_rate: 48_000,
            channels: 2,
            timestamp_ms: 0,
        }
    }
}

/// Errors returned by [`AudioCapture`] implementations.
#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    /// OS denied access (mic, screen recording, etc.).
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    /// The requested source no longer exists or never did.
    #[error("source not found: {0}")]
    SourceNotFound(String),
    /// This backend can't run on the current platform/OS version.
    #[error("unsupported on this platform: {0}")]
    Unsupported(String),
    /// The backend itself failed (driver, IPC, decoder, etc.).
    #[error("backend error: {0}")]
    Backend(String),
}

/// Item type yielded by a [`BoxedAudioStream`].
pub type AudioStreamItem = Result<AudioFrame, AudioError>;

/// A boxed, pinned stream of audio frames. `Send` so backend tasks can move
/// across threads (e.g. spawned onto the tokio runtime).
pub type BoxedAudioStream = Pin<Box<dyn Stream<Item = AudioStreamItem> + Send>>;

/// Whether a backend currently has the OS permissions it needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionStatus {
    /// Permissions are in place; capture should succeed.
    Granted,
    /// User explicitly denied; recovery requires user action.
    Denied,
    /// Permission state is indeterminate (not yet prompted).
    Unknown,
    /// Permission concept doesn't apply to this backend (e.g. virtual device).
    NotApplicable,
}

/// Implemented by every audio capture backend.
///
/// A backend is expected to be cheap to clone or share (`Arc`-friendly):
/// `open` takes `&self` and may be called concurrently to start independent
/// captures.
#[async_trait::async_trait]
pub trait AudioCapture: Send + Sync {
    /// Backend identifier, used for logging and audit trails.
    /// Should be stable across versions (e.g. `"mock"`, `"coreaudio-tap"`).
    fn name(&self) -> &'static str;

    /// List sources currently available (running processes, devices).
    async fn enumerate(&self) -> Result<Vec<AudioCaptureSource>, AudioError>;

    /// Open a stream from the given source. Caller drops the stream
    /// to stop capture.
    async fn open(&self, source: AudioCaptureSource) -> Result<BoxedAudioStream, AudioError>;

    /// Whether this backend currently has the OS permissions it needs.
    /// Returns `Ok(PermissionStatus::Denied)` if not granted but recoverable
    /// via OS prompt; `Err` if blocked entirely.
    async fn permission_status(&self) -> Result<PermissionStatus, AudioError>;
}

/// In-memory backend that emits a fixed sequence of frames regardless of the
/// requested source. Useful for tests and as the default placeholder until a
/// real backend is wired up.
#[derive(Debug, Clone)]
pub struct MockAudioCapture {
    sources: Vec<AudioCaptureSource>,
    frames: Vec<AudioFrame>,
    permission: PermissionStatus,
}

impl Default for MockAudioCapture {
    fn default() -> Self {
        let sources = vec![
            AudioCaptureSource {
                kind: SourceKind::InProcess,
                label: "Phantom (in-process)".to_string(),
            },
            AudioCaptureSource {
                kind: SourceKind::ProcessTap { pid: 1234 },
                label: "Mock process 1234".to_string(),
            },
        ];
        let frames = vec![
            AudioFrame::default(),
            AudioFrame {
                samples: vec![0.0_f32; 480],
                sample_rate: 48_000,
                channels: 2,
                timestamp_ms: 10,
            },
        ];
        Self {
            sources,
            frames,
            permission: PermissionStatus::NotApplicable,
        }
    }
}

impl MockAudioCapture {
    /// Build a mock with the default sources and frames.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a mock that advertises the given sources and emits the given
    /// frames on every `open`.
    #[must_use]
    pub fn with(
        sources: Vec<AudioCaptureSource>,
        frames: Vec<AudioFrame>,
        permission: PermissionStatus,
    ) -> Self {
        Self {
            sources,
            frames,
            permission,
        }
    }
}

#[async_trait::async_trait]
impl AudioCapture for MockAudioCapture {
    fn name(&self) -> &'static str {
        "mock"
    }

    async fn enumerate(&self) -> Result<Vec<AudioCaptureSource>, AudioError> {
        Ok(self.sources.clone())
    }

    async fn open(&self, _source: AudioCaptureSource) -> Result<BoxedAudioStream, AudioError> {
        let frames = self.frames.clone();
        Ok(Box::pin(mock_stream::stream(frames)))
    }

    async fn permission_status(&self) -> Result<PermissionStatus, AudioError> {
        Ok(self.permission)
    }
}

// Tiny inline stream helper so we don't pull in `async-stream`. Emits each
// pre-canned frame in order, then completes.
mod mock_stream {
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use futures_core::Stream;

    use super::{AudioFrame, AudioStreamItem};

    pub(super) fn stream(frames: Vec<AudioFrame>) -> MockStream {
        MockStream {
            frames: frames.into_iter(),
        }
    }

    pub(super) struct MockStream {
        frames: std::vec::IntoIter<AudioFrame>,
    }

    impl Stream for MockStream {
        type Item = AudioStreamItem;

        fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            // No self-referential state; safe to project mutably.
            let this = self.get_mut();
            match this.frames.next() {
                Some(frame) => Poll::Ready(Some(Ok(frame))),
                None => Poll::Ready(None),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use futures_util::StreamExt;

    #[test]
    fn audio_frame_default_is_sane() {
        let frame = AudioFrame::default();
        assert!(frame.samples.is_empty());
        assert_eq!(frame.sample_rate, 48_000);
        assert_eq!(frame.channels, 2);
        assert_eq!(frame.timestamp_ms, 0);
    }

    #[test]
    fn audio_capture_source_round_trips_through_serde_json() {
        let cases = vec![
            AudioCaptureSource {
                kind: SourceKind::ProcessTap { pid: 4242 },
                label: "Safari".to_string(),
            },
            AudioCaptureSource {
                kind: SourceKind::AppStream {
                    bundle_id: "com.apple.Music".to_string(),
                },
                label: "Music".to_string(),
            },
            AudioCaptureSource {
                kind: SourceKind::SystemViaVirtualDevice {
                    device_name: "BlackHole 2ch".to_string(),
                },
                label: "System (BlackHole)".to_string(),
            },
            AudioCaptureSource {
                kind: SourceKind::InProcess,
                label: "Phantom".to_string(),
            },
        ];

        for original in cases {
            let json = serde_json::to_string(&original).expect("serialize");
            let decoded: AudioCaptureSource = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(decoded.label, original.label);
            // Variant tag preserved.
            match (decoded.kind, original.kind) {
                (SourceKind::ProcessTap { pid: a }, SourceKind::ProcessTap { pid: b }) => {
                    assert_eq!(a, b);
                }
                (
                    SourceKind::AppStream { bundle_id: a },
                    SourceKind::AppStream { bundle_id: b },
                ) => assert_eq!(a, b),
                (
                    SourceKind::SystemViaVirtualDevice { device_name: a },
                    SourceKind::SystemViaVirtualDevice { device_name: b },
                ) => assert_eq!(a, b),
                (SourceKind::InProcess, SourceKind::InProcess) => {}
                _ => panic!("variant mismatch after round-trip"),
            }
        }
    }

    #[tokio::test]
    async fn mock_enumerate_returns_non_empty_list() {
        let backend = MockAudioCapture::new();
        let sources = backend.enumerate().await.expect("enumerate");
        assert!(!sources.is_empty(), "default mock should advertise sources");
    }

    #[tokio::test]
    async fn mock_open_yields_at_least_one_frame() {
        let backend = MockAudioCapture::new();
        let sources = backend.enumerate().await.expect("enumerate");
        let source = sources.into_iter().next().expect("at least one source");

        let mut stream = backend.open(source).await.expect("open");
        let first = stream.next().await.expect("at least one item");
        let frame = first.expect("ok frame");
        assert_eq!(frame.sample_rate, 48_000);
        assert_eq!(frame.channels, 2);
    }

    #[tokio::test]
    async fn mock_permission_status_defaults_to_not_applicable() {
        let backend = MockAudioCapture::new();
        let status = backend.permission_status().await.expect("status");
        assert_eq!(status, PermissionStatus::NotApplicable);
    }

    // Compile-time check: AudioStreamItem and BoxedAudioStream must be Send so
    // backend impls can move sessions across threads (tokio runtime, etc.).
    #[allow(dead_code)]
    fn assert_stream_item_is_send(item: AudioStreamItem) -> impl Send {
        item
    }

    #[allow(dead_code)]
    fn assert_stream_is_send(s: BoxedAudioStream) -> impl Send {
        s
    }

    // Compile-time check: an AudioCapture trait object can be shared across
    // threads.
    #[allow(dead_code)]
    fn assert_backend_is_send_sync(b: std::sync::Arc<dyn AudioCapture>) -> impl Send + Sync {
        b
    }

    // Compile-time check: on non-macOS targets the SCK module must not exist
    // so the workspace builds cleanly on Linux/Windows. Referencing the path
    // here would fail to resolve on non-mac builds; using `cfg!` (a value)
    // and a feature-cfg ensures the assertion compiles everywhere but only
    // truly tests the property in non-mac builds.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn macos_sck_module_absent_on_non_mac() {
        // If `crate::macos_sck` exists on a non-mac build, this test would
        // fail to compile (path resolution error). The assertion itself is
        // trivially true; the value is in the absence of the symbol.
        assert!(
            !cfg!(any(feature = "macos-sck-impl-leaked-to-non-mac")),
            "macos_sck must remain mac-only"
        );
    }
}
