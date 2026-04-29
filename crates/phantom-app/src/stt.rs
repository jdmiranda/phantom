//! STT pipeline integration for phantom-app.
//!
//! This module wires [`phantom_stt::stream::SttStream`] into the capture
//! pipeline. It owns the audio sender half and drives a background tokio task
//! that reads [`PartialTranscript`] results and converts them into
//! [`CaptureEvent::SpeechTranscribed`] events on the application event bus.
//!
//! # Lifecycle
//!
//! 1. At startup (or when the user enables voice input), call
//!    [`SttPipeline::start`] to launch the background tasks.
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
    audio_tx: mpsc::Sender<AudioChunk>,
}

impl SttPipeline {
    /// Start the STT pipeline with the given backend and config.
    ///
    /// Spawns two tokio tasks:
    /// * The [`SttStream::run`] loop — reads audio, emits partials.
    /// * The event-forwarding loop — converts final partials to
    ///   [`CaptureEvent::SpeechTranscribed`] and sends them on `event_tx`.
    ///
    /// `audio_channel_capacity` controls the depth of the audio mpsc channel.
    /// 64 is a reasonable default for 10 ms chunks at 16 kHz.
    #[must_use]
    pub fn start(
        backend: Arc<dyn TranscriptBackend>,
        config: SttStreamConfig,
        event_tx: mpsc::Sender<CaptureEvent>,
        audio_channel_capacity: usize,
    ) -> Self {
        let (audio_tx, audio_rx) = mpsc::channel::<AudioChunk>(audio_channel_capacity);
        let (partial_tx, partial_rx) = mpsc::channel::<PartialTranscript>(64);

        let stream = SttStream::new(backend, config);
        tokio::spawn(async move {
            stream.run(audio_rx, partial_tx).await;
        });

        tokio::spawn(forward_partials(partial_rx, event_tx));

        Self { audio_tx }
    }

    /// Push an [`AudioChunk`] into the pipeline.
    ///
    /// Returns `true` on success, `false` if the pipeline has shut down.
    pub fn push_chunk(&self, chunk: AudioChunk) -> bool {
        self.audio_tx.try_send(chunk).is_ok()
    }

    /// Returns `true` if the pipeline is still accepting audio.
    #[must_use]
    pub fn is_running(&self) -> bool {
        !self.audio_tx.is_closed()
    }
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

        let (event_tx, mut event_rx) = mpsc::channel::<CaptureEvent>(64);
        let pipeline = SttPipeline::start(backend, config, event_tx, 64);

        for _ in 0..5 {
            assert!(pipeline.push_chunk(voice_chunk()), "pipeline should accept audio");
        }
        for _ in 0..5 {
            assert!(pipeline.push_chunk(silence_chunk()), "pipeline should accept silence");
        }

        drop(pipeline);

        let events = collect_events_with_timeout(&mut event_rx, 2).await;

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

        let (event_tx, mut event_rx) = mpsc::channel::<CaptureEvent>(8);
        let pipeline = SttPipeline::start(backend, config, event_tx, 8);
        drop(pipeline);

        let events = collect_events_with_timeout(&mut event_rx, 1).await;
        assert!(events.is_empty(), "empty pipeline should produce no events");
    }

    /// `push_chunk` returns `false` when the pipeline's receiver is closed.
    #[test]
    fn push_chunk_returns_false_when_receiver_closed() {
        let (tx, rx) = mpsc::channel::<AudioChunk>(1);
        drop(rx);
        let pipeline = SttPipeline { audio_tx: tx };
        assert!(!pipeline.is_running(), "closed receiver means not running");
        assert!(
            !pipeline.push_chunk(silence_chunk()),
            "push_chunk returns false when receiver closed"
        );
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    async fn collect_events_with_timeout(
        rx: &mut mpsc::Receiver<CaptureEvent>,
        limit: usize,
    ) -> Vec<CaptureEvent> {
        let mut out = Vec::new();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(event)) => {
                    out.push(event);
                    if out.len() >= limit {
                        break;
                    }
                }
                Ok(None) | Err(_) => break,
            }
        }
        out
    }
}
