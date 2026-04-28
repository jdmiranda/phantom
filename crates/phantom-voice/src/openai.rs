//! OpenAI text-to-speech backend (`tts-1` / `tts-1-hd`).
//!
//! Streams synthesized audio from `POST /v1/audio/speech` with
//! `response_format: "pcm"`. The OpenAI PCM format is raw little-endian
//! signed 16-bit mono at 24 kHz, which we decode on the fly into
//! [`SynthAudioChunk`]s of f32 samples in `[-1.0, 1.0]`.
//!
//! Chunks are emitted at roughly 20 ms granularity (480 samples at 24 kHz)
//! so downstream consumers see steady streaming progress rather than a
//! single trailing buffer.

use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures_core::Stream;
use futures_util::StreamExt;

use crate::{
    BoxedVoiceStream, SynthAudioChunk, VoiceError, VoiceProfile, VoiceStreamItem, VoiceStyle,
    VoiceSynth,
};

/// Sample rate of OpenAI's PCM response format.
const OPENAI_TTS_SAMPLE_RATE: u32 = 24_000;

/// Approximate number of mono samples per emitted chunk (~20 ms @ 24 kHz).
const CHUNK_SAMPLES: usize = 480;

/// Default OpenAI API base URL. Overridable via [`OpenAiTtsBackend`] for
/// tests / proxies.
const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// OpenAI TTS backend.
///
/// Construct with [`OpenAiTtsBackend::from_env`] (reads `OPENAI_API_KEY`)
/// or [`OpenAiTtsBackend::new`] for explicit keys. Call [`Self::with_hd`]
/// to upgrade the model from `tts-1` to `tts-1-hd`.
#[derive(Debug, Clone)]
pub struct OpenAiTtsBackend {
    api_key: String,
    model: String,
    base_url: String,
}

impl OpenAiTtsBackend {
    /// Build a backend, reading the API key from `OPENAI_API_KEY`.
    ///
    /// # Errors
    ///
    /// Returns [`VoiceError::NotConfigured`] if the env var is missing or
    /// empty.
    pub fn from_env() -> Result<Self, VoiceError> {
        let api_key = std::env::var("OPENAI_API_KEY").map_err(|_| {
            VoiceError::NotConfigured("OPENAI_API_KEY env var not set".to_string())
        })?;
        if api_key.trim().is_empty() {
            return Err(VoiceError::NotConfigured(
                "OPENAI_API_KEY env var is empty".to_string(),
            ));
        }
        Ok(Self::new(api_key))
    }

    /// Build a backend with an explicit API key. Defaults to the `tts-1`
    /// model and the production OpenAI base URL.
    #[must_use]
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            model: "tts-1".to_string(),
            base_url: DEFAULT_BASE_URL.to_string(),
        }
    }

    /// Upgrade this backend to use the higher-quality `tts-1-hd` model.
    #[must_use]
    pub fn with_hd(mut self) -> Self {
        self.model = "tts-1-hd".to_string();
        self
    }

    /// Override the base URL (e.g. for proxies or tests).
    #[must_use]
    pub fn with_base_url(mut self, base_url: String) -> Self {
        self.base_url = base_url;
        self
    }

    /// Currently configured model id (`"tts-1"` or `"tts-1-hd"`).
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }
}

#[async_trait::async_trait]
impl VoiceSynth for OpenAiTtsBackend {
    fn name(&self) -> &'static str {
        "openai-tts"
    }

    async fn list_voices(&self) -> Result<Vec<VoiceProfile>, VoiceError> {
        Ok(static_voices())
    }

    async fn synthesize(
        &self,
        text: String,
        voice: &VoiceProfile,
    ) -> Result<BoxedVoiceStream, VoiceError> {
        let url = format!("{}/audio/speech", self.base_url.trim_end_matches('/'));
        let body = serde_json::json!({
            "model": self.model,
            "voice": voice.voice_id,
            "input": text,
            "response_format": "pcm",
        });

        let client = reqwest::Client::new();
        let response = client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| VoiceError::Backend(format!("openai request failed: {e}")))?;

        if !response.status().is_success() {
            let status = response.status();
            let detail = response
                .text()
                .await
                .unwrap_or_else(|_| "<no body>".to_string());
            return Err(VoiceError::Backend(format!(
                "openai tts http {status}: {detail}"
            )));
        }

        let byte_stream = response.bytes_stream();
        let chunker = PcmChunkStream::new(byte_stream);
        Ok(Box::pin(chunker))
    }
}

/// The static list of voices OpenAI exposes for `tts-1` / `tts-1-hd`.
fn static_voices() -> Vec<VoiceProfile> {
    const NAMES: [&str; 6] = ["alloy", "echo", "fable", "onyx", "nova", "shimmer"];
    NAMES
        .iter()
        .map(|name| VoiceProfile {
            voice_id: (*name).to_string(),
            label: (*name).to_string(),
            language: "en-US".to_string(),
            style: VoiceStyle::Neutral,
        })
        .collect()
}

/// Adapts a stream of HTTP byte chunks into `SynthAudioChunk`s of decoded
/// f32 samples.
///
/// The underlying byte stream may slice the PCM stream at arbitrary
/// boundaries (including in the middle of a 16-bit sample), so we buffer
/// any leftover odd byte across polls and only decode complete samples.
struct PcmChunkStream<S> {
    inner: S,
    /// Buffered f32 samples waiting to be drained into chunks.
    pending_samples: Vec<f32>,
    /// Leftover low byte when an inbound HTTP chunk had an odd length.
    leftover_byte: Option<u8>,
    /// Total samples emitted so far, for `timestamp_ms` accounting.
    emitted_samples: u64,
    /// True once the underlying byte stream has signalled completion.
    upstream_done: bool,
    /// True once we've emitted the final chunk with `is_final = true`.
    finished: bool,
}

impl<S> PcmChunkStream<S> {
    fn new(inner: S) -> Self {
        Self {
            inner,
            pending_samples: Vec::with_capacity(CHUNK_SAMPLES * 2),
            leftover_byte: None,
            emitted_samples: 0,
            upstream_done: false,
            finished: false,
        }
    }

    /// Decode an inbound byte slice into `pending_samples`, accounting for
    /// any odd byte buffered from a prior chunk.
    fn ingest(&mut self, bytes: &[u8]) {
        let mut idx = 0;
        if let Some(low) = self.leftover_byte.take()
            && let Some(&high) = bytes.first()
        {
            let s16 = i16::from_le_bytes([low, high]);
            self.pending_samples.push(i16_to_f32(s16));
            idx = 1;
        }
        let remaining = &bytes[idx..];
        let mut i = 0;
        while i + 1 < remaining.len() {
            let s16 = i16::from_le_bytes([remaining[i], remaining[i + 1]]);
            self.pending_samples.push(i16_to_f32(s16));
            i += 2;
        }
        if i < remaining.len() {
            self.leftover_byte = Some(remaining[i]);
        }
    }

    /// Pull up to `CHUNK_SAMPLES` samples off the front of the buffer.
    fn drain_chunk(&mut self, force_all: bool) -> Option<Vec<f32>> {
        if self.pending_samples.is_empty() {
            return None;
        }
        let take = if force_all {
            self.pending_samples.len()
        } else if self.pending_samples.len() >= CHUNK_SAMPLES {
            CHUNK_SAMPLES
        } else {
            return None;
        };
        Some(self.pending_samples.drain(..take).collect())
    }

    fn make_chunk(&mut self, samples: Vec<f32>, is_final: bool) -> SynthAudioChunk {
        let timestamp_ms = self.emitted_samples * 1000 / u64::from(OPENAI_TTS_SAMPLE_RATE);
        self.emitted_samples += samples.len() as u64;
        SynthAudioChunk {
            samples,
            sample_rate: OPENAI_TTS_SAMPLE_RATE,
            timestamp_ms,
            is_final,
        }
    }
}

impl<S> Stream for PcmChunkStream<S>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Send + Unpin,
{
    type Item = VoiceStreamItem;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.finished {
            return Poll::Ready(None);
        }

        loop {
            // Try to emit a full chunk from already-buffered samples.
            if !self.upstream_done
                && let Some(samples) = self.drain_chunk(false)
            {
                let chunk = self.make_chunk(samples, false);
                return Poll::Ready(Some(Ok(chunk)));
            }

            if self.upstream_done {
                // Drain remainder; mark final when the buffer empties.
                if let Some(samples) = self.drain_chunk(true) {
                    let is_final = self.pending_samples.is_empty();
                    let chunk = self.make_chunk(samples, is_final);
                    if is_final {
                        self.finished = true;
                    }
                    return Poll::Ready(Some(Ok(chunk)));
                }
                // Buffer empty after upstream end. Emit a synthetic empty
                // final chunk if we never sent one (e.g. zero-byte body).
                self.finished = true;
                let chunk = self.make_chunk(Vec::new(), true);
                return Poll::Ready(Some(Ok(chunk)));
            }

            match self.inner.poll_next_unpin(cx) {
                Poll::Ready(Some(Ok(bytes))) => {
                    self.ingest(&bytes);
                    continue;
                }
                Poll::Ready(Some(Err(e))) => {
                    self.finished = true;
                    return Poll::Ready(Some(Err(VoiceError::Backend(format!(
                        "openai stream error: {e}"
                    )))));
                }
                Poll::Ready(None) => {
                    self.upstream_done = true;
                    continue;
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// Convert a signed 16-bit PCM sample into an f32 in `[-1.0, 1.0]`.
fn i16_to_f32(s: i16) -> f32 {
    // Divide by 32768.0 (i16::MIN.abs()) so -32768 maps to exactly -1.0
    // and +32767 maps to ~0.99997.
    f32::from(s) / 32_768.0
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, MutexGuard, OnceLock};

    use super::*;

    /// Serializes tests that mutate the process-wide `OPENAI_API_KEY` env
    /// var. Cargo runs unit tests in parallel by default, so we need a
    /// mutex to keep the two env-driven tests from racing each other.
    fn env_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        // Recover from poisoning — the env var is a single bit of state and
        // a panicking test doesn't leave it in an inconsistent shape.
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    fn clear_env() {
        // SAFETY: callers hold `env_lock()`, and no other code in this
        // process touches `OPENAI_API_KEY` during tests.
        unsafe {
            std::env::remove_var("OPENAI_API_KEY");
        }
    }

    fn set_env(value: &str) {
        // SAFETY: see `clear_env`.
        unsafe {
            std::env::set_var("OPENAI_API_KEY", value);
        }
    }

    #[test]
    fn from_env_returns_not_configured_when_missing() {
        let _guard = env_lock();
        clear_env();
        let err = OpenAiTtsBackend::from_env().expect_err("must error when env missing");
        assert!(
            matches!(err, VoiceError::NotConfigured(_)),
            "expected NotConfigured, got {err:?}"
        );
    }

    #[test]
    fn from_env_succeeds_with_key() {
        let _guard = env_lock();
        set_env("sk-test-key-abc");
        let backend = OpenAiTtsBackend::from_env().expect("must succeed when env set");
        assert_eq!(backend.name(), "openai-tts");
        assert_eq!(backend.model(), "tts-1");
        clear_env();
    }

    #[tokio::test]
    async fn list_voices_returns_six_openai_voices() {
        let backend = OpenAiTtsBackend::new("sk-fake".to_string());
        let voices = backend.list_voices().await.expect("list_voices");
        assert_eq!(voices.len(), 6, "OpenAI exposes exactly six TTS voices");

        let ids: Vec<&str> = voices.iter().map(|v| v.voice_id.as_str()).collect();
        for expected in ["alloy", "echo", "fable", "onyx", "nova", "shimmer"] {
            assert!(ids.contains(&expected), "missing voice: {expected}");
        }
        for v in &voices {
            assert_eq!(v.language, "en-US");
            assert_eq!(v.style, VoiceStyle::Neutral);
            assert_eq!(v.voice_id, v.label);
        }
    }

    #[test]
    fn with_hd_uses_tts1_hd_model() {
        let backend = OpenAiTtsBackend::new("sk-fake".to_string()).with_hd();
        assert_eq!(backend.model(), "tts-1-hd");
    }

    #[test]
    fn i16_to_f32_maps_extremes() {
        assert!((i16_to_f32(0) - 0.0).abs() < 1e-6);
        assert!((i16_to_f32(i16::MIN) - -1.0).abs() < 1e-6);
        assert!(i16_to_f32(i16::MAX) < 1.0 && i16_to_f32(i16::MAX) > 0.999);
    }

    #[tokio::test]
    #[ignore = "requires network + OPENAI_API_KEY"]
    async fn integration_synthesize_hello_emits_final_chunk() {
        use futures_util::StreamExt;

        let backend = OpenAiTtsBackend::from_env().expect("OPENAI_API_KEY required");
        let voices = backend.list_voices().await.expect("list_voices");
        let voice = voices.first().expect("at least one voice").clone();

        let mut stream = backend
            .synthesize("hello".to_string(), &voice)
            .await
            .expect("synthesize");

        let mut chunks = Vec::new();
        while let Some(item) = stream.next().await {
            chunks.push(item.expect("chunk ok"));
        }
        assert!(!chunks.is_empty(), "expected ≥1 chunk");
        let last = chunks.last().expect("last chunk");
        assert!(last.is_final, "last chunk must be marked final");
        for c in &chunks[..chunks.len() - 1] {
            assert!(!c.is_final, "interim chunks must not be final");
        }
        for c in &chunks {
            assert_eq!(c.sample_rate, 24_000, "must be 24kHz");
        }
    }
}
