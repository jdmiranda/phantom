//! Speech-to-text backend abstraction for Phantom.
//!
//! This crate defines the trait + types only — no real backend implementations.
//! A [`MockBackend`] is provided for tests and as a placeholder default until
//! a real backend (Whisper, Deepgram, etc.) lands.
//!
//! # Streaming model
//!
//! Callers push [`AudioChunk`]s through a `tokio::sync::mpsc` channel. The
//! backend returns a [`BoxedTranscriptStream`] that yields [`TranscriptEvent`]s
//! as transcription progresses. Closing the audio channel signals end-of-input.

use std::pin::Pin;

use futures_core::Stream;

pub mod openai;
pub mod stream;

/// A single transcription event emitted by a backend.
///
/// Backends may emit interim events (`is_final == false`) followed by a final
/// event for the same word once they're confident.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TranscriptEvent {
    /// The transcribed word or token.
    pub word: String,
    /// Start of the word in the audio stream, in milliseconds.
    pub start_ms: u64,
    /// End of the word in the audio stream, in milliseconds.
    pub end_ms: u64,
    /// Backend-reported confidence in `[0.0, 1.0]`.
    pub confidence: f32,
    /// `true` if this is the backend's final answer for this span;
    /// `false` if it may be revised.
    pub is_final: bool,
}

/// A chunk of PCM audio fed to a backend.
#[derive(Debug, Clone)]
pub struct AudioChunk {
    /// Interleaved f32 PCM samples in `[-1.0, 1.0]`.
    pub samples: Vec<f32>,
    /// Sample rate in Hz (e.g. `16_000`).
    pub sample_rate: u32,
    /// Number of interleaved channels.
    pub channels: u16,
    /// Capture timestamp in milliseconds since session start.
    pub timestamp_ms: u64,
}

impl Default for AudioChunk {
    fn default() -> Self {
        Self {
            samples: Vec::new(),
            sample_rate: 16_000,
            channels: 1,
            timestamp_ms: 0,
        }
    }
}

/// Errors returned by [`TranscriptBackend`] implementations.
#[derive(Debug, thiserror::Error)]
pub enum SttError {
    /// The backend exists but is missing required configuration
    /// (API key, model file, etc.).
    #[error("backend not configured: {0}")]
    NotConfigured(String),
    /// The backend itself failed (network, model crash, etc.).
    #[error("backend error: {0}")]
    Backend(String),
    /// Audio format (sample rate, channel count, encoding) unsupported.
    #[error("audio format not supported: {0}")]
    UnsupportedFormat(String),
}

/// Item type yielded by a [`BoxedTranscriptStream`].
pub type TranscriptStreamItem = Result<TranscriptEvent, SttError>;

/// A boxed, pinned stream of transcript events. `Send` so backend tasks can
/// move across threads (e.g. spawned onto the tokio runtime).
pub type BoxedTranscriptStream = Pin<Box<dyn Stream<Item = TranscriptStreamItem> + Send>>;

/// Implemented by every speech-to-text backend.
///
/// A backend is expected to be cheap to clone or share (`Arc`-friendly):
/// `transcribe` takes `&self` and may be called concurrently to start
/// independent sessions.
#[async_trait::async_trait]
pub trait TranscriptBackend: Send + Sync {
    /// Backend identifier, used for logging and audit trails.
    /// Should be stable across versions (e.g. `"mock"`, `"whisper-local"`).
    fn name(&self) -> &'static str;

    /// Begin a transcription session.
    ///
    /// The caller pushes [`AudioChunk`]s through `audio_rx` and drops the
    /// sender to signal end-of-input. The returned stream yields events as
    /// they arrive and completes once the backend is done.
    async fn transcribe(
        &self,
        audio_rx: tokio::sync::mpsc::Receiver<AudioChunk>,
    ) -> Result<BoxedTranscriptStream, SttError>;
}

/// In-memory backend that emits a fixed transcript regardless of audio
/// content. Useful for tests and as the default placeholder until a real
/// backend is wired up.
#[cfg(test)]
#[derive(Debug, Clone, Default)]
pub struct MockBackend {
    transcript: Vec<TranscriptEvent>,
}

#[cfg(test)]
impl MockBackend {
    /// Build a backend that emits no events (drains audio and completes).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a backend that emits the given transcript once audio input
    /// closes.
    #[must_use]
    pub fn with_transcript(transcript: Vec<TranscriptEvent>) -> Self {
        Self { transcript }
    }
}

#[cfg(test)]
#[async_trait::async_trait]
impl TranscriptBackend for MockBackend {
    fn name(&self) -> &'static str {
        "mock"
    }

    async fn transcribe(
        &self,
        mut audio_rx: tokio::sync::mpsc::Receiver<AudioChunk>,
    ) -> Result<BoxedTranscriptStream, SttError> {
        let transcript = self.transcript.clone();
        let stream = async_stream::stream(transcript, async move {
            // Drain audio so the producer can complete cleanly.
            while audio_rx.recv().await.is_some() {}
        });
        Ok(Box::pin(stream))
    }
}

// Tiny inline stream helper so we don't pull in `async-stream`. Drains the
// audio channel, then emits each pre-canned event in order.
#[cfg(test)]
mod async_stream {
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use futures_core::Stream;

    use super::{TranscriptEvent, TranscriptStreamItem};

    pub(super) fn stream<F>(events: Vec<TranscriptEvent>, drain: F) -> MockStream<F>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        MockStream {
            drain: Some(Box::pin(drain)),
            events: events.into_iter(),
        }
    }

    pub(super) struct MockStream<F> {
        drain: Option<Pin<Box<F>>>,
        events: std::vec::IntoIter<TranscriptEvent>,
    }

    impl<F> Stream for MockStream<F>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        type Item = TranscriptStreamItem;

        fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            // Safety: we only project `drain` and `events` and never move out.
            let this = unsafe { self.get_unchecked_mut() };

            if let Some(drain) = this.drain.as_mut() {
                match drain.as_mut().poll(cx) {
                    Poll::Ready(()) => {
                        this.drain = None;
                    }
                    Poll::Pending => return Poll::Pending,
                }
            }

            match this.events.next() {
                Some(event) => Poll::Ready(Some(Ok(event))),
                None => Poll::Ready(None),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use futures_util::StreamExt;

    fn sample_event() -> TranscriptEvent {
        TranscriptEvent {
            word: "hello".to_string(),
            start_ms: 100,
            end_ms: 350,
            confidence: 0.92,
            is_final: true,
        }
    }

    #[test]
    fn transcript_event_round_trips_through_serde_json() {
        let original = sample_event();
        let json = serde_json::to_string(&original).expect("serialize");
        let decoded: TranscriptEvent = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(decoded.word, original.word);
        assert_eq!(decoded.start_ms, original.start_ms);
        assert_eq!(decoded.end_ms, original.end_ms);
        assert!((decoded.confidence - original.confidence).abs() < f32::EPSILON);
        assert_eq!(decoded.is_final, original.is_final);
    }

    #[test]
    fn audio_chunk_default_values_match_spec() {
        let chunk = AudioChunk::default();
        assert!(chunk.samples.is_empty());
        assert_eq!(chunk.sample_rate, 16_000);
        assert_eq!(chunk.channels, 1);
        assert_eq!(chunk.timestamp_ms, 0);
    }

    #[tokio::test]
    async fn mock_backend_yields_configured_transcript() {
        let events = vec![
            sample_event(),
            TranscriptEvent {
                word: "world".to_string(),
                start_ms: 400,
                end_ms: 700,
                confidence: 0.88,
                is_final: true,
            },
        ];
        let backend = MockBackend::with_transcript(events.clone());
        assert_eq!(backend.name(), "mock");

        let (tx, rx) = tokio::sync::mpsc::channel(4);
        // Feed one chunk then close.
        tx.send(AudioChunk::default()).await.expect("send");
        drop(tx);

        let mut stream = backend.transcribe(rx).await.expect("transcribe");
        let mut collected = Vec::new();
        while let Some(item) = stream.next().await {
            collected.push(item.expect("ok event"));
        }

        assert_eq!(collected.len(), events.len());
        for (got, want) in collected.iter().zip(events.iter()) {
            assert_eq!(got.word, want.word);
            assert_eq!(got.start_ms, want.start_ms);
        }
    }

    #[tokio::test]
    async fn mock_backend_with_stream_of_three_chunks_collects_all_events() {
        // Three transcript events — one per audio chunk conceptually.
        let events = vec![
            TranscriptEvent {
                word: "one".to_string(),
                start_ms: 0,
                end_ms: 200,
                confidence: 0.95,
                is_final: true,
            },
            TranscriptEvent {
                word: "two".to_string(),
                start_ms: 200,
                end_ms: 400,
                confidence: 0.90,
                is_final: true,
            },
            TranscriptEvent {
                word: "three".to_string(),
                start_ms: 400,
                end_ms: 600,
                confidence: 0.85,
                is_final: true,
            },
        ];
        let backend = MockBackend::with_transcript(events.clone());

        let (tx, rx) = tokio::sync::mpsc::channel(8);
        // Send exactly 3 chunks then close the channel.
        for i in 0u64..3 {
            tx.send(AudioChunk {
                samples: vec![0.0; 160],
                sample_rate: 16_000,
                channels: 1,
                timestamp_ms: i * 200,
            })
            .await
            .expect("send chunk");
        }
        drop(tx);

        let mut stream = backend.transcribe(rx).await.expect("transcribe");
        let mut collected = Vec::new();
        while let Some(item) = stream.next().await {
            collected.push(item.expect("ok event"));
        }

        assert_eq!(collected.len(), 3, "expected exactly 3 transcript events");
        assert_eq!(collected[0].word, "one");
        assert_eq!(collected[1].word, "two");
        assert_eq!(collected[2].word, "three");
    }

    #[tokio::test]
    async fn mock_backend_with_empty_audio_returns_no_events() {
        // MockBackend with no pre-configured transcript + immediately-closed channel.
        let backend = MockBackend::new();
        let (_tx, rx) = tokio::sync::mpsc::channel::<AudioChunk>(1);
        drop(_tx);

        let mut stream = backend.transcribe(rx).await.expect("transcribe");
        assert!(
            stream.next().await.is_none(),
            "empty mock backend should yield no events"
        );
    }

    // Compile-time check: BoxedTranscriptStream must be Send so backend impls
    // can move sessions across threads (tokio runtime, etc.).
    #[allow(dead_code)]
    fn assert_stream_is_send(s: BoxedTranscriptStream) -> impl Send {
        s
    }

    // Compile-time check: a TranscriptBackend trait object can be shared
    // across threads.
    #[allow(dead_code)]
    fn assert_backend_is_send_sync(b: std::sync::Arc<dyn TranscriptBackend>) -> impl Send + Sync {
        b
    }
}
