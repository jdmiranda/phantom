//! Text-to-speech (voice synthesis) backend abstraction for Phantom.
//!
//! Counterpart to `phantom-stt`. This crate defines the trait + types only —
//! no real backend implementations. A [`MockVoiceSynth`] is provided for tests
//! and as a placeholder default until a real backend (ElevenLabs, Piper, system
//! TTS, etc.) lands.
//!
//! # Streaming model
//!
//! Callers pass a complete `text` plus a chosen [`VoiceProfile`] to
//! [`VoiceSynth::synthesize`]. The backend returns a [`BoxedVoiceStream`] of
//! [`SynthAudioChunk`]s. Streaming-style backends emit interim chunks as
//! synthesis progresses; the final chunk has `is_final == true`. Batch-style
//! backends emit a single chunk with `is_final == true`.

use std::pin::Pin;

use futures_core::Stream;

pub mod openai;

/// Identifies a voice the backend can synthesize with.
///
/// `voice_id` is opaque to Phantom — backends interpret it however they like
/// (model path, cloud voice slug, etc.). `label` is a human-readable name
/// surfaced in UI and config.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct VoiceProfile {
    /// Backend-specific identifier (e.g. ElevenLabs voice id, Piper model file).
    pub voice_id: String,
    /// Human-readable label shown in UI / config (e.g. `"phantom-narrator"`).
    pub label: String,
    /// BCP-47 language tag (e.g. `"en-US"`, `"ja-JP"`).
    pub language: String,
    /// Default delivery style for this voice.
    pub style: VoiceStyle,
}

/// High-level delivery style hint passed to the backend. Backends may map
/// these onto their own emotion/prosody knobs or ignore them.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub enum VoiceStyle {
    /// Default, unaffected delivery.
    Neutral,
    /// Slow, steady, low-arousal delivery.
    Calm,
    /// Fast, high-arousal delivery for alerts and errors.
    Urgent,
    /// Warm, upbeat delivery for confirmations and successes.
    Cheerful,
}

/// A chunk of synthesized PCM audio emitted by a backend.
#[derive(Debug, Clone)]
pub struct SynthAudioChunk {
    /// Mono f32 PCM samples in `[-1.0, 1.0]`.
    pub samples: Vec<f32>,
    /// Sample rate in Hz (e.g. `22_050`, `24_000`).
    pub sample_rate: u32,
    /// Offset of this chunk's first sample from the start of synthesis,
    /// in milliseconds.
    pub timestamp_ms: u64,
    /// `true` if this is the last chunk for the current `synthesize` call.
    pub is_final: bool,
}

/// Errors returned by [`VoiceSynth`] implementations.
#[derive(Debug, thiserror::Error)]
pub enum VoiceError {
    /// The backend exists but is missing required configuration
    /// (API key, model file, etc.).
    #[error("backend not configured: {0}")]
    NotConfigured(String),
    /// The requested voice id is not known to this backend.
    #[error("voice not available: {0}")]
    VoiceNotFound(String),
    /// The backend itself failed (network, model crash, etc.).
    #[error("backend error: {0}")]
    Backend(String),
}

/// Item type yielded by a [`BoxedVoiceStream`].
pub type VoiceStreamItem = Result<SynthAudioChunk, VoiceError>;

/// A boxed, pinned stream of synthesized audio chunks. `Send` so backend tasks
/// can move across threads (e.g. spawned onto the tokio runtime).
pub type BoxedVoiceStream = Pin<Box<dyn Stream<Item = VoiceStreamItem> + Send>>;

/// Implemented by every text-to-speech backend.
///
/// A backend is expected to be cheap to clone or share (`Arc`-friendly):
/// `synthesize` takes `&self` and may be called concurrently to start
/// independent synthesis sessions.
#[async_trait::async_trait]
pub trait VoiceSynth: Send + Sync {
    /// Backend identifier, used for logging and audit trails.
    /// Should be stable across versions (e.g. `"mock"`, `"elevenlabs"`).
    fn name(&self) -> &'static str;

    /// Enumerate voices this backend can render with. May be empty if the
    /// backend is unconfigured.
    async fn list_voices(&self) -> Result<Vec<VoiceProfile>, VoiceError>;

    /// Begin synthesizing `text`. Returns a stream of audio chunks.
    ///
    /// Streaming-style backends emit interim chunks as synthesis progresses;
    /// the final chunk has `is_final == true`. Batch-style backends emit a
    /// single chunk with `is_final == true`.
    async fn synthesize(
        &self,
        text: String,
        voice: &VoiceProfile,
    ) -> Result<BoxedVoiceStream, VoiceError>;
}

/// In-memory backend that emits a single fixed synthetic chunk regardless of
/// input. Useful for tests and as the default placeholder until a real
/// backend is wired up.
#[derive(Debug, Clone, Default)]
pub struct MockVoiceSynth;

impl MockVoiceSynth {
    /// Build a new mock backend.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Voice profile this mock claims to support. Exposed so tests can pass
    /// the same profile to `synthesize` without hard-coding strings.
    #[must_use]
    pub fn default_voice() -> VoiceProfile {
        VoiceProfile {
            voice_id: "mock-voice-0".to_string(),
            label: "phantom-narrator".to_string(),
            language: "en-US".to_string(),
            style: VoiceStyle::Neutral,
        }
    }
}

#[async_trait::async_trait]
impl VoiceSynth for MockVoiceSynth {
    fn name(&self) -> &'static str {
        "mock"
    }

    async fn list_voices(&self) -> Result<Vec<VoiceProfile>, VoiceError> {
        Ok(vec![Self::default_voice()])
    }

    async fn synthesize(
        &self,
        _text: String,
        _voice: &VoiceProfile,
    ) -> Result<BoxedVoiceStream, VoiceError> {
        // 100ms of silence at 24kHz is a reasonable stand-in for "real" audio
        // so downstream consumers can exercise their pipeline.
        let sample_rate: u32 = 24_000;
        let samples = vec![0.0_f32; (sample_rate as usize) / 10];
        let chunk = SynthAudioChunk {
            samples,
            sample_rate,
            timestamp_ms: 0,
            is_final: true,
        };
        let stream = single_chunk_stream(chunk);
        Ok(Box::pin(stream))
    }
}

// Tiny inline stream helper so we don't pull in `async-stream`. Emits exactly
// one chunk, then completes.
fn single_chunk_stream(chunk: SynthAudioChunk) -> SingleChunkStream {
    SingleChunkStream {
        chunk: Some(chunk),
    }
}

struct SingleChunkStream {
    chunk: Option<SynthAudioChunk>,
}

impl Stream for SingleChunkStream {
    type Item = VoiceStreamItem;

    fn poll_next(
        mut self: Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        match self.chunk.take() {
            Some(chunk) => std::task::Poll::Ready(Some(Ok(chunk))),
            None => std::task::Poll::Ready(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use futures_util::StreamExt;

    fn sample_profile() -> VoiceProfile {
        VoiceProfile {
            voice_id: "vox-001".to_string(),
            label: "phantom-narrator".to_string(),
            language: "en-US".to_string(),
            style: VoiceStyle::Calm,
        }
    }

    #[test]
    fn voice_profile_round_trips_through_serde_json() {
        let original = sample_profile();
        let json = serde_json::to_string(&original).expect("serialize");
        let decoded: VoiceProfile = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(decoded.voice_id, original.voice_id);
        assert_eq!(decoded.label, original.label);
        assert_eq!(decoded.language, original.language);
        assert_eq!(decoded.style, original.style);
    }

    // Forces the compiler to error if a new VoiceStyle variant is added
    // without updating callers that depend on the full set.
    #[test]
    fn voice_style_exhaustive_match_covers_every_variant() {
        for style in [
            VoiceStyle::Neutral,
            VoiceStyle::Calm,
            VoiceStyle::Urgent,
            VoiceStyle::Cheerful,
        ] {
            let label: &'static str = match style {
                VoiceStyle::Neutral => "neutral",
                VoiceStyle::Calm => "calm",
                VoiceStyle::Urgent => "urgent",
                VoiceStyle::Cheerful => "cheerful",
            };
            assert!(!label.is_empty());
        }
    }

    #[tokio::test]
    async fn mock_voice_synth_list_voices_is_non_empty() {
        let synth = MockVoiceSynth::new();
        assert_eq!(synth.name(), "mock");

        let voices = synth.list_voices().await.expect("list_voices");
        assert!(!voices.is_empty(), "mock backend must advertise ≥1 voice");
    }

    #[tokio::test]
    async fn mock_voice_synth_synthesize_emits_final_chunk_last() {
        let synth = MockVoiceSynth::new();
        let voice = MockVoiceSynth::default_voice();

        let mut stream = synth
            .synthesize("hello phantom".to_string(), &voice)
            .await
            .expect("synthesize");

        let mut chunks = Vec::new();
        while let Some(item) = stream.next().await {
            chunks.push(item.expect("ok chunk"));
        }

        assert!(!chunks.is_empty(), "synthesize must emit ≥1 chunk");
        let last = chunks.last().expect("at least one chunk");
        assert!(last.is_final, "last chunk must have is_final == true");
        // No interim chunk should claim to be final.
        for chunk in &chunks[..chunks.len() - 1] {
            assert!(!chunk.is_final, "interim chunks must not be marked final");
        }
    }

    // Compile-time check: BoxedVoiceStream must be Send so backend tasks can
    // move sessions across threads (tokio runtime, etc.).
    #[allow(dead_code)]
    fn assert_stream_is_send(s: BoxedVoiceStream) -> impl Send {
        s
    }

    // Compile-time check: a VoiceSynth trait object can be shared across
    // threads.
    #[allow(dead_code)]
    fn assert_synth_is_send_sync(b: std::sync::Arc<dyn VoiceSynth>) -> impl Send + Sync {
        b
    }
}
