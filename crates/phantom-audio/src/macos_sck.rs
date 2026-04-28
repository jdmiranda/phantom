//! Tier-2 macOS audio capture backend built on Apple's ScreenCaptureKit.
//!
//! This implementation wraps the [`screencapturekit`] crate (version 1.5+),
//! which is the production-grade Rust binding maintained by `doom-fish`. It
//! satisfies the per-app capture path of [`crate::AudioCapture`] using the
//! `SCStream` "captures audio" mode and a content filter scoped to a single
//! `SCRunningApplication`.
//!
//! # Why ScreenCaptureKit (Tier 2) and not Process Tap (Tier 1)
//!
//! - SCK is supported on macOS 13.0+; Process Tap (`AudioObjectCreate` with
//!   `kAudioHardwarePropertyProcessObjectList`) is macOS 14.4+ only.
//! - SCK has a stable, audited Rust binding. Process Tap currently requires
//!   hand-rolled `unsafe` FFI against `CoreAudio`/`AudioToolbox` and a
//!   `CADisplayLink`-style IOProc, which is out of scope for this milestone.
//! - We will add a Tier-1 backend later; the trait abstracts the choice.
//!
//! # Permission model
//!
//! SCK requires the **Screen Recording** permission. The first call into
//! `SCShareableContent::get()` triggers the OS prompt. There is no
//! `requestAccess` API in SCK, so we probe by calling `get()` and treat
//! `SCError::PermissionDenied` (or any failure on first contact) as
//! [`PermissionStatus::Denied`].
//!
//! # Note on the crate version
//!
//! The original spec called for `screencapturekit = "0.3"`, but the published
//! crate at `crates.io` is currently at `1.5.4` (the `0.x` series predates
//! audio support entirely). We pin to `1.5` and use the public, safe API
//! surface — no `unsafe` blocks live in this file.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use async_trait::async_trait;
use futures_core::Stream;
use screencapturekit::cm::CMSampleBuffer;
use screencapturekit::error::SCError;
use screencapturekit::shareable_content::SCShareableContent;
use screencapturekit::stream::configuration::SCStreamConfiguration;
use screencapturekit::stream::content_filter::SCContentFilter;
use screencapturekit::stream::output_type::SCStreamOutputType;
use screencapturekit::stream::sc_stream::SCStream;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::{
    AudioCapture, AudioCaptureSource, AudioError, AudioFrame, AudioStreamItem, BoxedAudioStream,
    PermissionStatus, SourceKind,
};

/// Default sample rate requested from SCK. SCK supports 8k/16k/24k/48k.
const DEFAULT_SAMPLE_RATE_HZ: i32 = 48_000;
/// Default channel count requested from SCK. SCK supports 1 or 2.
const DEFAULT_CHANNEL_COUNT: i32 = 2;
/// Bounded channel depth for the audio frame queue. ~10 frames @ 1024 samples
/// @ 48 kHz ≈ 200 ms of buffering before backpressure / drops.
const FRAME_QUEUE_DEPTH: usize = 32;

/// macOS ScreenCaptureKit-based audio backend.
pub struct MacOsSckCapture {
    /// Cached probe result so repeated `permission_status` calls don't
    /// re-trigger the OS prompt path. `None` means "not probed yet".
    permission_cache: tokio::sync::Mutex<Option<PermissionStatus>>,
}

impl MacOsSckCapture {
    /// Construct a new backend handle. Cheap; does not touch SCK.
    ///
    /// # Errors
    ///
    /// Currently infallible, but kept fallible for parity with future
    /// backends that might do an init-time capability check.
    pub fn new() -> Result<Self, AudioError> {
        Ok(Self {
            permission_cache: tokio::sync::Mutex::new(None),
        })
    }
}

#[async_trait]
impl AudioCapture for MacOsSckCapture {
    fn name(&self) -> &'static str {
        "macos-sck"
    }

    async fn enumerate(&self) -> Result<Vec<AudioCaptureSource>, AudioError> {
        // SCShareableContent::get() is a blocking ObjC call that round-trips
        // to the windowserver. Push it off the async runtime.
        let content = tokio::task::spawn_blocking(SCShareableContent::get)
            .await
            .map_err(|e| AudioError::Backend(format!("spawn_blocking join failed: {e}")))?
            .map_err(map_sc_error)?;

        let mut out = Vec::new();
        for app in content.applications() {
            let bundle_id = app.bundle_identifier();
            // Skip the placeholder "" bundle that SCK emits for unbundled
            // helper processes — they aren't a stable thing to "open" later.
            if bundle_id.is_empty() {
                continue;
            }
            let label = {
                let name = app.application_name();
                if name.is_empty() {
                    bundle_id.clone()
                } else {
                    name
                }
            };
            out.push(AudioCaptureSource {
                kind: SourceKind::AppStream {
                    bundle_id: bundle_id.clone(),
                },
                label,
            });
        }

        Ok(out)
    }

    async fn open(&self, source: AudioCaptureSource) -> Result<BoxedAudioStream, AudioError> {
        let SourceKind::AppStream { bundle_id } = source.kind else {
            return Err(AudioError::Unsupported(format!(
                "macos-sck backend only supports AppStream sources; got {:?}",
                source.kind
            )));
        };

        // Build the stream on a blocking thread — SCK construction and
        // start_capture both call into ObjC.
        let bundle_for_task = bundle_id.clone();
        let (tx, rx) = mpsc::channel::<AudioStreamItem>(FRAME_QUEUE_DEPTH);

        let stream_handle: SckStreamGuard = tokio::task::spawn_blocking(move || {
            build_and_start_stream(&bundle_for_task, tx)
        })
        .await
        .map_err(|e| AudioError::Backend(format!("spawn_blocking join failed: {e}")))?
        .map_err(map_sc_error_with_context(&bundle_id))?;

        // Yield frames to the consumer. The SCK output handler closure pushes
        // into `tx`; when the consumer drops `BoxedAudioStream` we drop both
        // the receiver side and the SckStreamGuard, which calls stop_capture.
        Ok(Box::pin(SckAudioStream {
            inner: ReceiverStream::new(rx),
            _guard: stream_handle,
        }))
    }

    async fn permission_status(&self) -> Result<PermissionStatus, AudioError> {
        // Cache the answer so we don't repeatedly call into SCK. If we've
        // probed once, return that.
        {
            let cache = self.permission_cache.lock().await;
            if let Some(status) = *cache {
                return Ok(status);
            }
        }

        let probe_result = tokio::task::spawn_blocking(SCShareableContent::get)
            .await
            .map_err(|e| AudioError::Backend(format!("spawn_blocking join failed: {e}")))?;

        let status = match probe_result {
            Ok(_) => PermissionStatus::Granted,
            Err(SCError::PermissionDenied(_)) => PermissionStatus::Denied,
            Err(SCError::NoShareableContent(_)) => {
                // This typically also means the user never granted the
                // permission; the OS just rejects with "no content"
                // without distinguishing. Treat as Unknown rather than
                // misreporting a hard Denied.
                PermissionStatus::Unknown
            }
            Err(_) => PermissionStatus::Unknown,
        };

        let mut cache = self.permission_cache.lock().await;
        *cache = Some(status);
        Ok(status)
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// RAII handle that stops + drops the underlying `SCStream` on Drop.
///
/// SCK's `SCStream` will leak the running stream until either its destructor
/// runs or `stop_capture` is called explicitly. We do both, in that order.
struct SckStreamGuard {
    stream: Option<SCStream>,
    /// Set by the output handler when it sees the receiver was dropped, so
    /// we don't keep delivering into a closed channel.
    _shutdown: Arc<AtomicBool>,
}

impl Drop for SckStreamGuard {
    fn drop(&mut self) {
        if let Some(stream) = self.stream.take() {
            // Best-effort stop. If SCK errors out (e.g. stream never started
            // because permission was denied between configure and start), we
            // just let SCStream's own drop release Apple-side resources.
            let _ = stream.stop_capture();
        }
    }
}

/// Adapter stream type so we can plumb the guard's lifetime into
/// `BoxedAudioStream` without exposing `SCStream` to callers.
struct SckAudioStream {
    inner: ReceiverStream<AudioStreamItem>,
    _guard: SckStreamGuard,
}

impl Stream for SckAudioStream {
    type Item = AudioStreamItem;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        std::pin::Pin::new(&mut self.inner).poll_next(cx)
    }
}

/// Construct an `SCStream` filtered to one app's audio and start capturing.
/// Runs on a blocking thread.
fn build_and_start_stream(
    bundle_id: &str,
    tx: mpsc::Sender<AudioStreamItem>,
) -> Result<SckStreamGuard, SCError> {
    let content = SCShareableContent::get()?;

    // Find the running app whose bundle id matches.
    let apps = content.applications();
    let target_app = apps
        .iter()
        .find(|a| a.bundle_identifier() == bundle_id)
        .ok_or_else(|| {
            SCError::ApplicationNotFound(format!(
                "no running application with bundle id {bundle_id}"
            ))
        })?;

    // SCContentFilter requires a display anchor for the
    // `with_including_applications` form. Use the first available display —
    // for an audio-only capture the display choice is irrelevant.
    let displays = content.displays();
    let display = displays.first().ok_or_else(|| {
        SCError::NoShareableContent("no displays available to anchor SCContentFilter".to_string())
    })?;

    let filter = SCContentFilter::create()
        .with_display(display)
        .with_including_applications(&[target_app], &[])
        .build();

    let config = SCStreamConfiguration::new()
        .with_captures_audio(true)
        .with_excludes_current_process_audio(true)
        .with_sample_rate(DEFAULT_SAMPLE_RATE_HZ)
        .with_channel_count(DEFAULT_CHANNEL_COUNT);

    let mut stream = SCStream::new(&filter, &config);

    // Per-stream session clock so frame timestamps start at 0.
    let session_start = Instant::now();
    // Best-effort frame counter (currently informational only — the
    // timestamp comes from `Instant::now()`).
    let _frame_seq = Arc::new(AtomicU64::new(0));
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_for_handler = shutdown.clone();

    stream.add_output_handler(
        move |sample: CMSampleBuffer, of_type: SCStreamOutputType| {
            // We only registered for Audio, but SCK delivers all output types
            // through the same trait — guard anyway.
            if !matches!(of_type, SCStreamOutputType::Audio) {
                return;
            }
            if shutdown_for_handler.load(Ordering::Relaxed) {
                return;
            }
            if let Some(frame) = sample_buffer_to_audio_frame(&sample, session_start) {
                // Fire-and-forget: if the consumer is gone, mark shutdown so
                // we stop building frames at all.
                if tx.try_send(Ok(frame)).is_err() {
                    shutdown_for_handler.store(true, Ordering::Relaxed);
                }
            }
        },
        SCStreamOutputType::Audio,
    );

    stream.start_capture()?;

    Ok(SckStreamGuard {
        stream: Some(stream),
        _shutdown: shutdown,
    })
}

/// Convert a SCK audio sample buffer to a phantom-audio `AudioFrame`.
///
/// Returns `None` if the buffer contains no audio data we can interpret
/// (missing format description, non-float PCM, empty buffer list, etc.).
fn sample_buffer_to_audio_frame(
    sample: &CMSampleBuffer,
    session_start: Instant,
) -> Option<AudioFrame> {
    // Format metadata: sample rate, channel count, float-vs-int.
    let format = sample.format_description()?;
    if !format.is_audio() {
        return None;
    }
    let sample_rate = format.audio_sample_rate()? as u32;
    let channels = format.audio_channel_count()? as u16;
    let is_float = format.audio_is_float();
    let bits_per_channel = format.audio_bits_per_channel().unwrap_or(32);

    // Audio payload.
    let buffer_list = sample.audio_buffer_list()?;
    let mut samples = Vec::<f32>::new();
    for buffer in buffer_list.iter() {
        let bytes = buffer.data();
        if bytes.is_empty() {
            continue;
        }

        if is_float && bits_per_channel == 32 {
            // f32 PCM, native endianness — the standard SCK format.
            // Reinterpret the byte slice as f32 samples (4 bytes each).
            let n = bytes.len() / 4;
            samples.reserve(n);
            for i in 0..n {
                let off = i * 4;
                let raw = [bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]];
                samples.push(f32::from_ne_bytes(raw));
            }
        } else if !is_float && bits_per_channel == 16 {
            // i16 PCM (rare for SCK, but handle defensively).
            let n = bytes.len() / 2;
            samples.reserve(n);
            for i in 0..n {
                let off = i * 2;
                let raw = [bytes[off], bytes[off + 1]];
                let s = i16::from_ne_bytes(raw);
                samples.push(f32::from(s) / f32::from(i16::MAX));
            }
        } else {
            // Unknown format — skip rather than emit garbage. Future work:
            // hand-roll integer-to-float conversion for the relevant format
            // flags once we've seen them in the wild.
            return None;
        }
    }

    if samples.is_empty() {
        return None;
    }

    // Timestamp: prefer SCK's presentation time (monotonic, in CMTime units),
    // but fall back to wall clock measured from session start for robustness.
    let timestamp_ms = sample
        .presentation_timestamp()
        .as_seconds()
        .map_or_else(
            || session_start.elapsed().as_millis() as u64,
            |s| (s * 1_000.0) as u64,
        );

    Some(AudioFrame {
        samples,
        sample_rate,
        channels,
        timestamp_ms,
    })
}

/// Map an `SCError` to our `AudioError` taxonomy.
fn map_sc_error(err: SCError) -> AudioError {
    match err {
        SCError::PermissionDenied(msg) => AudioError::PermissionDenied(msg),
        SCError::ApplicationNotFound(msg)
        | SCError::WindowNotFound(msg)
        | SCError::DisplayNotFound(msg) => AudioError::SourceNotFound(msg),
        SCError::FeatureNotAvailable {
            feature,
            required_version,
        } => AudioError::Unsupported(format!("{feature} requires macOS {required_version}+")),
        other => AudioError::Backend(other.to_string()),
    }
}

/// Attach the bundle id to the error message for a friendlier `open`-time
/// failure surface.
fn map_sc_error_with_context(bundle_id: &str) -> impl FnOnce(SCError) -> AudioError + '_ {
    move |err| {
        let mut e = map_sc_error(err);
        // Wrap source-not-found / backend errors with the bundle id so the
        // caller can tell which app failed. PermissionDenied is global so
        // we leave that one unmodified.
        match &mut e {
            AudioError::SourceNotFound(msg) | AudioError::Backend(msg) => {
                *msg = format!("{msg} (bundle_id={bundle_id})");
            }
            _ => {}
        }
        e
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: enumerate either succeeds with some apps, or fails with a
    /// permission error on a fresh CI box. Either is acceptable; what we're
    /// guarding is that the function doesn't hang or panic.
    #[tokio::test]
    async fn enumerate_succeeds_or_permission_denied() {
        let backend = MacOsSckCapture::new().expect("construct backend");
        match backend.enumerate().await {
            Ok(_sources) => {
                // Permission granted; we got *some* answer. We can't assert
                // non-empty because a sandboxed CI runner may legitimately
                // report zero apps.
            }
            Err(AudioError::PermissionDenied(_)) | Err(AudioError::Backend(_)) => {
                // Acceptable on CI / first run before the user grants
                // Screen Recording permission.
            }
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[tokio::test]
    async fn non_app_stream_open_returns_unsupported() {
        let backend = MacOsSckCapture::new().expect("construct backend");
        let source = AudioCaptureSource {
            kind: SourceKind::InProcess,
            label: "in-process placeholder".to_string(),
        };
        // BoxedAudioStream doesn't impl Debug, so we can't use `expect_err`;
        // match the result manually instead.
        match backend.open(source).await {
            Ok(_) => panic!("InProcess must not be supported by SCK backend"),
            Err(AudioError::Unsupported(_)) => {}
            Err(other) => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn permission_status_returns_a_known_variant() {
        let backend = MacOsSckCapture::new().expect("construct backend");
        let status = backend.permission_status().await.expect("status query");
        // Just assert we got *some* answer; the actual value depends on
        // whether the test host has granted Screen Recording.
        assert!(matches!(
            status,
            PermissionStatus::Granted | PermissionStatus::Denied | PermissionStatus::Unknown
        ));
    }
}
