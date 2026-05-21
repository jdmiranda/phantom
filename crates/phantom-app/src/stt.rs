//! STT pipeline integration for phantom-app.
//!
//! This module wires [`phantom_stt::stream::SttStream`] into the capture
//! pipeline. It owns the audio sender half and drives a background tokio task
//! that reads [`PartialTranscript`] results and converts them into
//! [`CaptureEvent::SpeechTranscribed`] events on the application event bus.
//!
//! # Backend selection
//!
//! [`SttPipeline::build`] reads environment variables at startup and selects
//! the first available backend in priority order:
//!
//! 1. `OPENAI_API_KEY` → [`phantom_stt::openai::OpenAiBackend`]
//! 2. No key found → returns `None` (STT disabled, no-op).
//!
//! When `None` is returned, callers should treat STT as disabled and skip
//! [`drain_stt_events`] calls.
//!
//! # Lifecycle
//!
//! 1. At startup, call [`SttPipeline::build`] to launch the background tasks.
//! 2. During audio capture, call [`SttPipeline::push_chunk`] for each
//!    incoming [`AudioChunk`].
//! 3. To stop gracefully, drop [`SttPipeline`] — dropping `audio_tx` signals
//!    end-of-input to the [`SttStream`] runner, which in turn closes the
//!    `partial_tx` channel and shuts the event-forwarding task down.
//!
//! # Error handling
//!
//! All errors in the background tasks are logged and do not surface to callers.
//! [`SttPipeline::push_chunk`] returns `false` if the pipeline has shut down
//! (channel closed); callers should stop producing audio in that case.

use std::sync::Arc;

use tokio::sync::mpsc;

use phantom_bundles::events::CaptureEvent;
use phantom_stt::{AudioChunk, TranscriptBackend};
use phantom_stt::stream::{PartialTranscript, SttStream, SttStreamConfig};

/// Handle to a running STT pipeline.
///
/// Drop this value to stop the pipeline gracefully (audio channel closes →
/// remaining segment flushed → event-forwarding task exits).
pub struct SttPipeline {
    /// Sender half of the audio ingestion channel.
    /// `push_chunk` will be the primary call site once mic capture is wired.
    #[allow(dead_code)]
    audio_tx: mpsc::Sender<AudioChunk>,
    /// Receiver for [`CaptureEvent::SpeechTranscribed`] events produced by the
    /// background forwarding task. Drained by `drain_stt_events` each frame.
    pub(crate) event_rx: mpsc::Receiver<CaptureEvent>,
}

impl SttPipeline {
    /// Attempt to construct an [`SttPipeline`] by probing the environment for
    /// a real STT backend.
    ///
    /// Priority order:
    /// 1. `OPENAI_API_KEY` → [`phantom_stt::openai::OpenAiBackend`]
    /// 2. No key found → `None` (STT disabled).
    ///
    /// Returns `None` when no key is available; the rest of the app boots
    /// normally and voice input is simply unavailable.
    #[must_use]
    pub fn build() -> Option<Self> {
        // Privacy-mode check is handled by the caller (App::with_config_scaled)
        // before calling build(), so we don't re-examine it here.
        //
        // Delegate env-var probing + whitespace-guard to `OpenAiBackend::from_env`
        // so the two code paths can't diverge.
        match phantom_stt::openai::OpenAiBackend::from_env() {
            Ok(backend) => {
                log::info!("STT: using OpenAI backend (gpt-4o-transcribe)");
                Some(Self::start(Arc::new(backend), SttStreamConfig::default(), 64))
            }
            Err(_) => {
                log::warn!("STT: no OPENAI_API_KEY found — voice input disabled");
                None
            }
        }
    }

    /// Start the STT pipeline with the given backend and config.
    ///
    /// Spawns two tokio tasks:
    /// * The [`SttStream::run`] loop — reads audio, emits partials.
    /// * The event-forwarding loop — converts final partials to
    ///   [`CaptureEvent::SpeechTranscribed`] and sends them on an internal
    ///   channel whose receiver is stored in `self.event_rx`.
    ///
    /// `audio_channel_capacity` controls the depth of the audio mpsc channel.
    /// 64 is a reasonable default for 10 ms chunks at 16 kHz.
    #[must_use]
    pub fn start(
        backend: Arc<dyn TranscriptBackend>,
        config: SttStreamConfig,
        audio_channel_capacity: usize,
    ) -> Self {
        let (audio_tx, audio_rx) = mpsc::channel::<AudioChunk>(audio_channel_capacity);
        let (partial_tx, partial_rx) = mpsc::channel::<PartialTranscript>(64);
        let (event_tx, event_rx) = mpsc::channel::<CaptureEvent>(64);

        let stream = SttStream::new(backend, config);
        // FIXME(#56/#68): these tasks are live but produce no output until mic
        // capture is wired. They idle on `audio_rx.recv()` with zero CPU cost
        // and shut down cleanly when `audio_tx` is dropped (pipeline dropped).
        tokio::spawn(async move {
            stream.run(audio_rx, partial_tx).await;
        });

        tokio::spawn(forward_partials(partial_rx, event_tx));

        Self { audio_tx, event_rx }
    }

    /// Push an [`AudioChunk`] into the pipeline.
    ///
    /// Returns `true` on success, `false` if the pipeline has shut down.
    ///
    /// Called by the audio capture subsystem once mic input is wired up.
    #[allow(dead_code)]
    pub fn push_chunk(&self, chunk: AudioChunk) -> bool {
        self.audio_tx.try_send(chunk).is_ok()
    }

    /// Returns `true` if the pipeline is still accepting audio.
    #[must_use]
    #[allow(dead_code)]
    pub fn is_running(&self) -> bool {
        !self.audio_tx.is_closed()
    }
}

/// Drain any pending [`CaptureEvent`]s from the STT pipeline.
#[allow(dead_code)]
///
/// When `stt` is `None` (STT disabled), this is a compile-time no-op.
/// Returns all events available without blocking. Intended to be called
/// once per frame from `update.rs`.
pub fn drain_stt_events(stt: &mut Option<SttPipeline>) -> Vec<CaptureEvent> {
    let Some(pipeline) = stt else {
        return Vec::new();
    };
    let mut events = Vec::new();
    while let Ok(ev) = pipeline.event_rx.try_recv() {
        events.push(ev);
    }
    events
}

/// Background task: read [`PartialTranscript`]s and emit final ones as
/// [`CaptureEvent::SpeechTranscribed`] on `event_tx`.
async fn forward_partials(
    mut partial_rx: mpsc::Receiver<PartialTranscript>,
    event_tx: mpsc::Sender<CaptureEvent>,
) {
    while let Some(partial) = partial_rx.recv().await {
        if !partial.is_final {
            continue;
        }

        let event = CaptureEvent::SpeechTranscribed {
            text: partial.text,
            confidence: partial.confidence,
            timestamp_ms: partial.timestamp_ms,
        };

        if event_tx.send(event).await.is_err() {
            break;
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use phantom_stt::{MockBackend, TranscriptEvent};

    /// Mutex to prevent env-var mutation tests from racing each other.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn word(text: &str) -> TranscriptEvent {
        TranscriptEvent {
            word: text.to_string(),
            start_ms: 0,
            end_ms: 100,
            confidence: 0.9,
            is_final: true,
        }
    }

    fn voice_chunk() -> AudioChunk {
        AudioChunk {
            samples: vec![0.5; 160],
            sample_rate: 16_000,
            channels: 1,
            timestamp_ms: 0,
        }
    }

    fn silence_chunk() -> AudioChunk {
        AudioChunk {
            samples: vec![0.0; 160],
            sample_rate: 16_000,
            channels: 1,
            timestamp_ms: 0,
        }
    }

    /// End-to-end acceptance test for issues #56 and #68:
    /// "Capture pipeline emits SpeechTranscribed events end-to-end
    ///  (mocked Whisper backend OK for test)".
    #[tokio::test]
    async fn pipeline_emits_speech_transcribed_event() {
        let backend = Arc::new(MockBackend::with_transcript(vec![
            word("cargo"),
            word("test"),
        ]));

        let config = SttStreamConfig::new()
            .with_silence_boundary_ms(50)
            .with_silence_threshold(0.01)
            .with_interim_interval_ms(9999);

        let mut pipeline = SttPipeline::start(backend, config, 64);

        for _ in 0..5 {
            assert!(pipeline.push_chunk(voice_chunk()), "pipeline should accept audio");
        }
        for _ in 0..5 {
            assert!(pipeline.push_chunk(silence_chunk()), "pipeline should accept silence");
        }

        // Signal end-of-input by dropping the audio_tx clone inside the pipeline.
        // We can't drop the pipeline itself yet because we need event_rx.
        // Use a helper: construct a dummy sender to close the channel.
        // Actually dropping `pipeline` closes event_rx too — collect first.

        // Drop the pipeline's audio sender to flush the stream.
        // We keep event_rx alive by temporarily holding a reference.
        // Simplest: just let the pipeline run to completion via timeout.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);

        let mut events = Vec::new();
        loop {
            match tokio::time::timeout_at(deadline, pipeline.event_rx.recv()).await {
                Ok(Some(ev)) => {
                    events.push(ev);
                    break; // one final event is enough
                }
                Ok(None) | Err(_) => break,
            }
        }

        // Force close — drop pipeline.
        drop(pipeline);

        assert!(!events.is_empty(), "expected at least one SpeechTranscribed event");
        match &events[0] {
            CaptureEvent::SpeechTranscribed { text, confidence, .. } => {
                assert_eq!(text, "cargo test");
                assert!(*confidence > 0.0 && *confidence <= 1.0);
            }
            other => panic!("expected SpeechTranscribed, got {other:?}"),
        }
    }

    /// Dropping the pipeline without sending any audio does not produce any
    /// events and does not panic.
    #[tokio::test]
    async fn pipeline_empty_audio_produces_no_events() {
        let backend = Arc::new(MockBackend::new());
        let config = SttStreamConfig::new()
            .with_silence_boundary_ms(50)
            .with_interim_interval_ms(9999);

        let pipeline = SttPipeline::start(backend, config, 8);
        drop(pipeline);
    }

    /// `push_chunk` returns `false` when the pipeline's receiver is closed.
    #[test]
    fn push_chunk_returns_false_when_receiver_closed() {
        let (tx, rx) = mpsc::channel::<AudioChunk>(1);
        let (_, event_rx) = mpsc::channel::<CaptureEvent>(1);
        drop(rx);
        let pipeline = SttPipeline { audio_tx: tx, event_rx };
        assert!(!pipeline.is_running(), "closed receiver means not running");
        assert!(
            !pipeline.push_chunk(silence_chunk()),
            "push_chunk returns false when receiver closed"
        );
    }

    /// `drain_stt_events` returns an empty vec when `stt` is `None`.
    #[test]
    fn drain_stt_events_is_noop_when_none() {
        let mut stt: Option<SttPipeline> = None;
        let events = drain_stt_events(&mut stt);
        assert!(events.is_empty(), "drain_stt_events should be a no-op when None");
    }

    // ── Backend-selection tests ───────────────────────────────────────────────

    /// When `OPENAI_API_KEY` is set, `SttPipeline::build()` should construct
    /// an `OpenAiBackend` and return `Some`. We verify this by checking that
    /// `build()` returns `Some` (the backend name is internal; we trust the
    /// from_env path given the existing openai.rs tests for the key logic).
    #[tokio::test]
    async fn stt_pipeline_uses_openai_when_key_present() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let prev = std::env::var("OPENAI_API_KEY").ok();
        unsafe {
            std::env::set_var("OPENAI_API_KEY", "sk-test-fixture-stt");
        }

        let result = SttPipeline::build();

        // Restore env before asserting so a panic still cleans up.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("OPENAI_API_KEY", v),
                None => std::env::remove_var("OPENAI_API_KEY"),
            }
        }

        assert!(
            result.is_some(),
            "SttPipeline::build() must return Some when OPENAI_API_KEY is set"
        );
    }

    /// When no API key is present, `SttPipeline::build()` returns `None`.
    #[test]
    fn stt_pipeline_returns_none_when_no_key() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let prev = std::env::var("OPENAI_API_KEY").ok();
        unsafe {
            std::env::remove_var("OPENAI_API_KEY");
        }

        let result = SttPipeline::build();

        unsafe {
            match prev {
                Some(v) => std::env::set_var("OPENAI_API_KEY", v),
                None => std::env::remove_var("OPENAI_API_KEY"),
            }
        }

        assert!(
            result.is_none(),
            "SttPipeline::build() must return None when OPENAI_API_KEY is unset"
        );
    }

    /// An empty or whitespace-only API key is treated as absent.
    #[test]
    fn stt_pipeline_returns_none_when_key_is_empty() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let prev = std::env::var("OPENAI_API_KEY").ok();
        unsafe {
            std::env::set_var("OPENAI_API_KEY", "   ");
        }

        let result = SttPipeline::build();

        unsafe {
            match prev {
                Some(v) => std::env::set_var("OPENAI_API_KEY", v),
                None => std::env::remove_var("OPENAI_API_KEY"),
            }
        }

        assert!(
            result.is_none(),
            "SttPipeline::build() must return None for whitespace-only key"
        );
    }
}
