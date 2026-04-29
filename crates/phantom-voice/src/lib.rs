//! Text-to-speech (voice synthesis) backend abstraction for Phantom.
//!
//! Counterpart to `phantom-stt`. This crate defines the [`VoiceSynth`] trait,
//! shared types, a [`MockVoiceSynth`] for tests, and a [`CachingVoiceSynth`]
//! wrapper that memoises synthesis results in an LRU cache (max 100 entries).
//!
//! # Backends
//!
//! | Backend | Module |
//! |---------|--------|
//! | OpenAI (`tts-1` / `tts-1-hd`) | [`openai`] |
//! | Mock (silent 100 ms chunk) | [`MockVoiceSynth`] |
//!
//! # Streaming model
//!
//! Callers pass a complete `text` plus a chosen [`VoiceProfile`] to
//! [`VoiceSynth::synthesize`]. The backend returns a [`BoxedVoiceStream`] of
//! [`SynthAudioChunk`]s. Streaming-style backends emit interim chunks as
//! synthesis progresses; the final chunk has `is_final == true`. Batch-style
//! backends (and the cache replay path) emit a single chunk with
//! `is_final == true`.

use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::Mutex;

use futures_core::Stream;
use lru::LruCache;

pub mod openai;

// ── OpenAiVoice enum ──────────────────────────────────────────────────────────

/// Identifies an OpenAI TTS voice by name.
///
/// OpenAI's `tts-1` and `tts-1-hd` models expose exactly these six voices.
/// Convert to the API slug with [`OpenAiVoice::as_voice_id`]; convert to a
/// generic [`VoiceProfile`] with [`OpenAiVoice::into_profile`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OpenAiVoice {
    /// Alloy — balanced, versatile.
    Alloy,
    /// Echo — warm, conversational.
    Echo,
    /// Fable — expressive, British accent.
    Fable,
    /// Onyx — deep, authoritative.
    Onyx,
    /// Nova — energetic, bright.
    Nova,
    /// Shimmer — soft, clear.
    Shimmer,
}

impl OpenAiVoice {
    /// All six OpenAI TTS voices in definition order.
    pub const ALL: [Self; 6] = [
        Self::Alloy,
        Self::Echo,
        Self::Fable,
        Self::Onyx,
        Self::Nova,
        Self::Shimmer,
    ];

    /// The API slug used in `POST /v1/audio/speech` requests.
    #[must_use]
    pub fn as_voice_id(self) -> &'static str {
        match self {
            Self::Alloy => "alloy",
            Self::Echo => "echo",
            Self::Fable => "fable",
            Self::Onyx => "onyx",
            Self::Nova => "nova",
            Self::Shimmer => "shimmer",
        }
    }

    /// Build the [`VoiceProfile`] descriptor for this voice.
    #[must_use]
    pub fn into_profile(self) -> VoiceProfile {
        VoiceProfile {
            voice_id: self.as_voice_id().to_string(),
            label: self.as_voice_id().to_string(),
            language: "en-US".to_string(),
            style: VoiceStyle::Neutral,
        }
    }
}

impl std::fmt::Display for OpenAiVoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_voice_id())
    }
}

// ── VoiceProfile ──────────────────────────────────────────────────────────────

/// Identifies a voice the backend can synthesize with.
///
/// `voice_id` is opaque to Phantom — backends interpret it however they like
/// (model path, cloud voice slug, etc.). `label` is a human-readable name
/// surfaced in UI and config.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct VoiceProfile {
    /// Backend-specific identifier (e.g. OpenAI voice slug, Piper model file).
    pub voice_id: String,
    /// Human-readable label shown in UI / config (e.g. `"nova"`).
    pub label: String,
    /// BCP-47 language tag (e.g. `"en-US"`, `"ja-JP"`).
    pub language: String,
    /// Default delivery style for this voice.
    pub style: VoiceStyle,
}

// ── VoiceStyle ────────────────────────────────────────────────────────────────

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

// ── Audio types ───────────────────────────────────────────────────────────────

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

// ── Error ─────────────────────────────────────────────────────────────────────

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

// ── Stream types ──────────────────────────────────────────────────────────────

/// Item type yielded by a [`BoxedVoiceStream`].
pub type VoiceStreamItem = Result<SynthAudioChunk, VoiceError>;

/// A boxed, pinned stream of synthesized audio chunks. `Send` so backend tasks
/// can move across threads (e.g. spawned onto the tokio runtime).
pub type BoxedVoiceStream = Pin<Box<dyn Stream<Item = VoiceStreamItem> + Send>>;

// ── VoiceSynth trait ──────────────────────────────────────────────────────────

/// Implemented by every text-to-speech backend.
///
/// A backend is expected to be cheap to clone or share (`Arc`-friendly):
/// `synthesize` takes `&self` and may be called concurrently to start
/// independent synthesis sessions.
#[async_trait::async_trait]
pub trait VoiceSynth: Send + Sync {
    /// Backend identifier, used for logging and audit trails.
    /// Should be stable across versions (e.g. `"mock"`, `"openai-tts"`).
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

// ── MockVoiceSynth ────────────────────────────────────────────────────────────

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
        // 100 ms of silence at 24 kHz is a reasonable stand-in for "real"
        // audio so downstream consumers can exercise their pipeline.
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

// ── CachingVoiceSynth ─────────────────────────────────────────────────────────

/// Maximum number of (text, voice_id) pairs retained in the synthesis cache.
const CACHE_CAPACITY: usize = 100;

/// Cache key: verbatim text paired with the backend-specific voice id.
type CacheKey = (String, String);

/// Wrapper that adds an LRU cache in front of any [`VoiceSynth`] backend.
///
/// When the same `(text, voice.voice_id)` pair is requested again, the
/// wrapper returns the previously-synthesised PCM bytes as a single final
/// chunk without calling the upstream backend.
///
/// The cache is bounded to [`CACHE_CAPACITY`] entries; the least-recently-used
/// entry is evicted when the limit is reached.
///
/// # Concurrency
///
/// The internal cache is protected by a `Mutex`. Synthesis calls release the
/// lock before awaiting the upstream backend, so concurrent requests for
/// *different* keys proceed in parallel. Two concurrent requests for the
/// *same* uncached key will both hit the upstream, but only one result is
/// ultimately written to the cache (last write wins, which is harmless).
pub struct CachingVoiceSynth<B> {
    inner: B,
    cache: Mutex<LruCache<CacheKey, Vec<f32>>>,
    sample_rate: u32,
}

impl<B: VoiceSynth> CachingVoiceSynth<B> {
    /// Wrap `inner` with an LRU cache that stores up to [`CACHE_CAPACITY`]
    /// synthesis results.
    ///
    /// `sample_rate` is used when replaying a cached result as a
    /// [`SynthAudioChunk`]; it should match the backend's output rate.
    #[must_use]
    pub fn new(inner: B, sample_rate: u32) -> Self {
        let capacity =
            NonZeroUsize::new(CACHE_CAPACITY).expect("CACHE_CAPACITY is non-zero by definition");
        Self {
            inner,
            cache: Mutex::new(LruCache::new(capacity)),
            sample_rate,
        }
    }
}

#[async_trait::async_trait]
impl<B: VoiceSynth> VoiceSynth for CachingVoiceSynth<B> {
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    async fn list_voices(&self) -> Result<Vec<VoiceProfile>, VoiceError> {
        self.inner.list_voices().await
    }

    async fn synthesize(
        &self,
        text: String,
        voice: &VoiceProfile,
    ) -> Result<BoxedVoiceStream, VoiceError> {
        let key: CacheKey = (text.clone(), voice.voice_id.clone());

        // Fast path: cache hit — return a single-chunk stream from memory.
        {
            let mut guard = self
                .cache
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(samples) = guard.get(&key) {
                let chunk = SynthAudioChunk {
                    samples: samples.clone(),
                    sample_rate: self.sample_rate,
                    timestamp_ms: 0,
                    is_final: true,
                };
                drop(guard);
                return Ok(Box::pin(single_chunk_stream(chunk)));
            }
        }

        // Slow path: synthesize upstream, drain into memory, populate cache.
        use futures_util::StreamExt as _;

        let mut stream = self.inner.synthesize(text, voice).await?;
        let mut all_samples: Vec<f32> = Vec::new();
        while let Some(item) = stream.next().await {
            let chunk = item?;
            all_samples.extend_from_slice(&chunk.samples);
        }

        {
            let mut guard = self
                .cache
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.put(key, all_samples.clone());
        }

        let chunk = SynthAudioChunk {
            samples: all_samples,
            sample_rate: self.sample_rate,
            timestamp_ms: 0,
            is_final: true,
        };
        Ok(Box::pin(single_chunk_stream(chunk)))
    }
}

// ── Internal stream helpers ───────────────────────────────────────────────────

/// Emits exactly one chunk, then completes. Avoids pulling in `async-stream`
/// for this simple case.
fn single_chunk_stream(chunk: SynthAudioChunk) -> SingleChunkStream {
    SingleChunkStream { chunk: Some(chunk) }
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

// ── Tests ─────────────────────────────────────────────────────────────────────

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

    /// Drain a [`BoxedVoiceStream`] to completion, returning all chunks.
    async fn collect_stream(mut stream: BoxedVoiceStream) -> Vec<SynthAudioChunk> {
        let mut chunks = Vec::new();
        while let Some(item) = stream.next().await {
            chunks.push(item.expect("chunk must be Ok in tests"));
        }
        chunks
    }

    // ── VoiceProfile serde ────────────────────────────────────────────────────

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

    // ── OpenAiVoice enum ──────────────────────────────────────────────────────

    #[test]
    fn openai_voice_all_has_six_variants() {
        assert_eq!(OpenAiVoice::ALL.len(), 6);
    }

    #[test]
    fn openai_voice_slugs_match_api_names() {
        let expected = [
            (OpenAiVoice::Alloy, "alloy"),
            (OpenAiVoice::Echo, "echo"),
            (OpenAiVoice::Fable, "fable"),
            (OpenAiVoice::Onyx, "onyx"),
            (OpenAiVoice::Nova, "nova"),
            (OpenAiVoice::Shimmer, "shimmer"),
        ];
        for (voice, slug) in expected {
            assert_eq!(voice.as_voice_id(), slug);
            assert_eq!(voice.to_string(), slug);
        }
    }

    #[test]
    fn openai_voice_into_profile_roundtrips() {
        for voice in OpenAiVoice::ALL {
            let profile = voice.into_profile();
            assert_eq!(profile.voice_id, voice.as_voice_id());
            assert_eq!(profile.language, "en-US");
            assert_eq!(profile.style, VoiceStyle::Neutral);
        }
    }

    #[test]
    fn openai_voice_serde_roundtrip() {
        let voice = OpenAiVoice::Nova;
        let json = serde_json::to_string(&voice).expect("serialize");
        let decoded: OpenAiVoice = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, voice);
        // The serialised form must be the lowercase slug, not a variant name.
        assert_eq!(json, "\"nova\"");
    }

    // ── MockVoiceSynth ────────────────────────────────────────────────────────

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

        let stream = synth
            .synthesize("hello phantom".to_string(), &voice)
            .await
            .expect("synthesize");
        let chunks = collect_stream(stream).await;

        assert!(!chunks.is_empty(), "synthesize must emit ≥1 chunk");
        let last = chunks.last().expect("at least one chunk");
        assert!(last.is_final, "last chunk must have is_final == true");
        for chunk in &chunks[..chunks.len() - 1] {
            assert!(!chunk.is_final, "interim chunks must not be marked final");
        }
    }

    // ── CachingVoiceSynth ─────────────────────────────────────────────────────

    /// A mock that counts synthesis invocations so we can verify cache hits.
    #[derive(Debug, Default, Clone)]
    struct CountingMock {
        call_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl CountingMock {
        fn calls(&self) -> usize {
            self.call_count
                .load(std::sync::atomic::Ordering::Relaxed)
        }
    }

    #[async_trait::async_trait]
    impl VoiceSynth for CountingMock {
        fn name(&self) -> &'static str {
            "counting-mock"
        }

        async fn list_voices(&self) -> Result<Vec<VoiceProfile>, VoiceError> {
            Ok(vec![MockVoiceSynth::default_voice()])
        }

        async fn synthesize(
            &self,
            _text: String,
            _voice: &VoiceProfile,
        ) -> Result<BoxedVoiceStream, VoiceError> {
            self.call_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let chunk = SynthAudioChunk {
                samples: vec![0.1_f32, 0.2_f32],
                sample_rate: 24_000,
                timestamp_ms: 0,
                is_final: true,
            };
            Ok(Box::pin(single_chunk_stream(chunk)))
        }
    }

    #[tokio::test]
    async fn caching_synth_deduplicates_identical_requests() {
        let mock = CountingMock::default();
        let caching = CachingVoiceSynth::new(mock.clone(), 24_000);
        let voice = MockVoiceSynth::default_voice();

        // First call → upstream is invoked.
        collect_stream(
            caching
                .synthesize("hello".to_string(), &voice)
                .await
                .expect("first call"),
        )
        .await;
        assert_eq!(mock.calls(), 1, "first call must hit upstream");

        // Second call with same text+voice → cache hit, no upstream call.
        collect_stream(
            caching
                .synthesize("hello".to_string(), &voice)
                .await
                .expect("second call"),
        )
        .await;
        assert_eq!(mock.calls(), 1, "second call must return cached result");
    }

    #[tokio::test]
    async fn caching_synth_treats_different_texts_as_distinct_keys() {
        let mock = CountingMock::default();
        let caching = CachingVoiceSynth::new(mock.clone(), 24_000);
        let voice = MockVoiceSynth::default_voice();

        collect_stream(
            caching
                .synthesize("hello".to_string(), &voice)
                .await
                .expect("hello"),
        )
        .await;
        collect_stream(
            caching
                .synthesize("world".to_string(), &voice)
                .await
                .expect("world"),
        )
        .await;

        assert_eq!(mock.calls(), 2, "different texts must each hit upstream");
    }

    #[tokio::test]
    async fn caching_synth_treats_different_voices_as_distinct_keys() {
        let mock = CountingMock::default();
        let caching = CachingVoiceSynth::new(mock.clone(), 24_000);

        let voice_a = VoiceProfile {
            voice_id: "voice-a".to_string(),
            label: "A".to_string(),
            language: "en-US".to_string(),
            style: VoiceStyle::Neutral,
        };
        let voice_b = VoiceProfile {
            voice_id: "voice-b".to_string(),
            label: "B".to_string(),
            language: "en-US".to_string(),
            style: VoiceStyle::Neutral,
        };

        collect_stream(
            caching
                .synthesize("hello".to_string(), &voice_a)
                .await
                .expect("voice-a"),
        )
        .await;
        collect_stream(
            caching
                .synthesize("hello".to_string(), &voice_b)
                .await
                .expect("voice-b"),
        )
        .await;

        assert_eq!(mock.calls(), 2, "different voices must each hit upstream");
    }

    #[tokio::test]
    async fn caching_synth_cached_result_is_single_final_chunk() {
        let mock = CountingMock::default();
        let caching = CachingVoiceSynth::new(mock, 24_000);
        let voice = MockVoiceSynth::default_voice();

        // Prime the cache.
        collect_stream(
            caching
                .synthesize("cached".to_string(), &voice)
                .await
                .expect("prime"),
        )
        .await;

        // Replay from cache.
        let chunks = collect_stream(
            caching
                .synthesize("cached".to_string(), &voice)
                .await
                .expect("replay"),
        )
        .await;

        assert_eq!(chunks.len(), 1, "cache replay emits exactly one chunk");
        assert!(chunks[0].is_final, "replayed chunk must be marked final");
    }

    #[tokio::test]
    async fn caching_synth_name_delegates_to_inner() {
        let caching = CachingVoiceSynth::new(MockVoiceSynth::new(), 24_000);
        assert_eq!(caching.name(), "mock");
    }

    #[tokio::test]
    async fn caching_synth_list_voices_delegates_to_inner() {
        let caching = CachingVoiceSynth::new(MockVoiceSynth::new(), 24_000);
        let voices = caching.list_voices().await.expect("list_voices");
        assert!(!voices.is_empty());
    }

    // ── Compile-time assertions ───────────────────────────────────────────────

    // BoxedVoiceStream must be Send so backend tasks can move sessions across
    // threads (tokio runtime, etc.).
    #[allow(dead_code)]
    fn assert_stream_is_send(s: BoxedVoiceStream) -> impl Send {
        s
    }

    // A VoiceSynth trait object can be shared across threads.
    #[allow(dead_code)]
    fn assert_synth_is_send_sync(b: std::sync::Arc<dyn VoiceSynth>) -> impl Send + Sync {
        b
    }

    // CachingVoiceSynth<MockVoiceSynth> is itself Send + Sync.
    #[allow(dead_code)]
    fn assert_caching_synth_is_send_sync(
        c: CachingVoiceSynth<MockVoiceSynth>,
    ) -> impl Send + Sync {
        c
    }
}
