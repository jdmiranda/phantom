//! OpenAI `gpt-4o-transcribe` backend for [`crate::TranscriptBackend`].
//!
//! This is a request/response backend (not streaming): it drains the audio
//! channel, converts the f32 PCM to 16 kHz mono PCM16 little-endian, wraps it
//! in a minimal WAV container, and POSTs it to
//! `${base_url}/audio/transcriptions` as multipart form-data. The response is
//! parsed into a batch of [`crate::TranscriptEvent`]s.
//!
//! Callers using a watcher / push-to-talk pattern accept the latency tradeoff
//! (one round-trip after end-of-input).

use std::time::Duration;

use futures_util::stream;
use serde::Deserialize;
use tokio::sync::mpsc;

use crate::{AudioChunk, BoxedTranscriptStream, SttError, TranscriptBackend, TranscriptEvent};

/// Default OpenAI model id for this backend.
pub const DEFAULT_MODEL: &str = "gpt-4o-transcribe";

/// Default OpenAI REST base URL.
pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// Target sample rate the backend resamples / expects from upstream.
/// We don't resample here — we trust the audio pipeline to deliver 16 kHz mono.
const TARGET_SAMPLE_RATE: u32 = 16_000;

/// Backend that calls OpenAI's `gpt-4o-transcribe` endpoint.
///
/// Cheap to clone; an internal [`reqwest::Client`] handles connection pooling.
#[derive(Debug, Clone)]
pub struct OpenAiBackend {
    api_key: String,
    model: String,
    base_url: String,
    http: reqwest::Client,
}

impl OpenAiBackend {
    /// Build a backend from the `OPENAI_API_KEY` environment variable.
    ///
    /// # Errors
    ///
    /// Returns [`SttError::NotConfigured`] if `OPENAI_API_KEY` is unset or
    /// empty.
    pub fn from_env() -> Result<Self, SttError> {
        let api_key = std::env::var("OPENAI_API_KEY").map_err(|_| {
            SttError::NotConfigured("OPENAI_API_KEY environment variable not set".to_string())
        })?;
        if api_key.trim().is_empty() {
            return Err(SttError::NotConfigured(
                "OPENAI_API_KEY environment variable is empty".to_string(),
            ));
        }
        Ok(Self::new(api_key))
    }

    /// Build a backend with an explicit API key. Uses the default model and
    /// base URL.
    #[must_use]
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            model: DEFAULT_MODEL.to_string(),
            base_url: DEFAULT_BASE_URL.to_string(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(60))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }

    /// Override the model id (e.g. `"whisper-1"`, `"gpt-4o-mini-transcribe"`).
    #[must_use]
    pub fn with_model(mut self, model: String) -> Self {
        self.model = model;
        self
    }

    /// Override the API base URL (useful for proxies or test servers).
    #[must_use]
    pub fn with_base_url(mut self, base_url: String) -> Self {
        self.base_url = base_url;
        self
    }

    /// Read-only accessor for the configured model.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Read-only accessor for the configured base URL.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }
}

#[async_trait::async_trait]
impl TranscriptBackend for OpenAiBackend {
    fn name(&self) -> &'static str {
        "openai-gpt4o-transcribe"
    }

    async fn transcribe(
        &self,
        mut audio_rx: mpsc::Receiver<AudioChunk>,
    ) -> Result<BoxedTranscriptStream, SttError> {
        // 1. Drain audio, downmix to mono PCM16, validate sample rate.
        let mut pcm16: Vec<i16> = Vec::new();
        let mut sample_rate: Option<u32> = None;
        while let Some(chunk) = audio_rx.recv().await {
            let chunk_rate = chunk.sample_rate;
            match sample_rate {
                None => sample_rate = Some(chunk_rate),
                Some(r) if r == chunk_rate => {}
                Some(r) => {
                    return Err(SttError::UnsupportedFormat(format!(
                        "mixed sample rates in stream: {r} then {chunk_rate}"
                    )));
                }
            }

            // Downmix interleaved channels to mono by averaging.
            let channels = chunk.channels.max(1) as usize;
            if channels == 1 {
                for &s in &chunk.samples {
                    pcm16.push(f32_to_pcm16(s));
                }
            } else {
                for frame in chunk.samples.chunks_exact(channels) {
                    let avg: f32 = frame.iter().sum::<f32>() / channels as f32;
                    pcm16.push(f32_to_pcm16(avg));
                }
            }
        }

        let sample_rate = sample_rate.unwrap_or(TARGET_SAMPLE_RATE);
        if pcm16.is_empty() {
            // Empty audio — return empty stream rather than 400 from OpenAI.
            return Ok(Box::pin(stream::iter(Vec::<
                Result<TranscriptEvent, SttError>,
            >::new())));
        }

        // 2. Wrap PCM16 in a WAV container. OpenAI accepts wav, mp3, mp4, etc.
        let wav_bytes = pcm16_to_wav(&pcm16, sample_rate, 1);

        // 3. POST as multipart form-data.
        let url = format!("{}/audio/transcriptions", self.base_url.trim_end_matches('/'));
        let part = reqwest::multipart::Part::bytes(wav_bytes)
            .file_name("audio.wav")
            .mime_str("audio/wav")
            .map_err(|e| SttError::Backend(format!("multipart mime: {e}")))?;
        let form = reqwest::multipart::Form::new()
            .text("model", self.model.clone())
            .text("response_format", "json")
            .text("timestamp_granularities[]", "word")
            .part("file", part);

        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.api_key)
            .multipart(form)
            .send()
            .await
            .map_err(|e| SttError::Backend(format!("openai request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(SttError::Backend(format!(
                "openai responded {status}: {body}"
            )));
        }

        let parsed: OpenAiTranscriptionResponse = resp
            .json()
            .await
            .map_err(|e| SttError::Backend(format!("openai json parse: {e}")))?;

        let events = parsed.into_events();

        Ok(Box::pin(stream::iter(
            events.into_iter().map(Ok).collect::<Vec<_>>(),
        )))
    }
}

/// Convert a single f32 sample in `[-1.0, 1.0]` to PCM16, with clamping.
///
/// Out-of-range inputs saturate at `i16::MIN` / `i16::MAX`. NaN maps to 0.
#[inline]
#[must_use]
pub fn f32_to_pcm16(sample: f32) -> i16 {
    if !sample.is_finite() {
        return 0;
    }
    let clamped = sample.clamp(-1.0, 1.0);
    // Use 32767 for symmetric scaling; matches what most encoders do.
    (clamped * 32767.0).round() as i16
}

/// Build a minimal RIFF/WAVE container around interleaved PCM16 samples.
fn pcm16_to_wav(samples: &[i16], sample_rate: u32, channels: u16) -> Vec<u8> {
    let bits_per_sample: u16 = 16;
    let byte_rate = sample_rate * u32::from(channels) * u32::from(bits_per_sample) / 8;
    let block_align = channels * bits_per_sample / 8;
    let data_size = (samples.len() * 2) as u32;
    let riff_size = 36 + data_size;

    let mut buf = Vec::with_capacity(44 + data_size as usize);
    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&riff_size.to_le_bytes());
    buf.extend_from_slice(b"WAVE");
    // fmt sub-chunk
    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&16u32.to_le_bytes()); // PCM fmt chunk size
    buf.extend_from_slice(&1u16.to_le_bytes()); // audio format = PCM
    buf.extend_from_slice(&channels.to_le_bytes());
    buf.extend_from_slice(&sample_rate.to_le_bytes());
    buf.extend_from_slice(&byte_rate.to_le_bytes());
    buf.extend_from_slice(&block_align.to_le_bytes());
    buf.extend_from_slice(&bits_per_sample.to_le_bytes());
    // data sub-chunk
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&data_size.to_le_bytes());
    for s in samples {
        buf.extend_from_slice(&s.to_le_bytes());
    }
    buf
}

#[derive(Debug, Deserialize)]
struct OpenAiTranscriptionResponse {
    /// Full concatenated transcript. Always present.
    #[serde(default)]
    text: String,
    /// Per-word timestamps. Present when `timestamp_granularities[]=word`.
    #[serde(default)]
    words: Vec<OpenAiWord>,
}

#[derive(Debug, Deserialize)]
struct OpenAiWord {
    word: String,
    /// Seconds since start of audio.
    start: f64,
    /// Seconds since start of audio.
    end: f64,
}

impl OpenAiTranscriptionResponse {
    /// Convert the response into a batch of [`TranscriptEvent`]s.
    ///
    /// If word-level timestamps are present, one event per word. Otherwise we
    /// fall back to a single event covering the full transcript with zero
    /// timing — the caller can still surface the text to the user.
    fn into_events(self) -> Vec<TranscriptEvent> {
        if !self.words.is_empty() {
            return self
                .words
                .into_iter()
                .map(|w| TranscriptEvent {
                    word: w.word,
                    start_ms: (w.start * 1000.0).round().max(0.0) as u64,
                    end_ms: (w.end * 1000.0).round().max(0.0) as u64,
                    // OpenAI's transcription API does not expose per-word
                    // confidence. We surface 1.0 as a neutral "trust this"
                    // value rather than 0.0, which downstream rankers might
                    // interpret as "discard". Documented in the module docs.
                    confidence: 1.0,
                    is_final: true,
                })
                .collect();
        }

        if self.text.trim().is_empty() {
            return Vec::new();
        }

        vec![TranscriptEvent {
            word: self.text,
            start_ms: 0,
            end_ms: 0,
            confidence: 1.0,
            is_final: true,
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use futures_util::StreamExt;

    /// Mutex to prevent env-var tests from racing each other.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn from_env_returns_not_configured_when_missing() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: protected by ENV_LOCK; std::env::remove_var requires unsafe in 2024 edition.
        let prev = std::env::var("OPENAI_API_KEY").ok();
        unsafe {
            std::env::remove_var("OPENAI_API_KEY");
        }

        let err = OpenAiBackend::from_env().expect_err("should fail without env var");
        assert!(matches!(err, SttError::NotConfigured(_)));

        if let Some(v) = prev {
            unsafe {
                std::env::set_var("OPENAI_API_KEY", v);
            }
        }
    }

    #[test]
    fn from_env_succeeds_with_key() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("OPENAI_API_KEY").ok();
        unsafe {
            std::env::set_var("OPENAI_API_KEY", "sk-test-fixture");
        }

        let backend = OpenAiBackend::from_env().expect("should construct");
        assert_eq!(backend.name(), "openai-gpt4o-transcribe");
        assert_eq!(backend.model(), DEFAULT_MODEL);
        assert_eq!(backend.base_url(), DEFAULT_BASE_URL);

        match prev {
            Some(v) => unsafe { std::env::set_var("OPENAI_API_KEY", v) },
            None => unsafe { std::env::remove_var("OPENAI_API_KEY") },
        }
    }

    #[test]
    fn from_env_rejects_empty_key() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("OPENAI_API_KEY").ok();
        unsafe {
            std::env::set_var("OPENAI_API_KEY", "   ");
        }

        let err = OpenAiBackend::from_env().expect_err("should reject empty");
        assert!(matches!(err, SttError::NotConfigured(_)));

        match prev {
            Some(v) => unsafe { std::env::set_var("OPENAI_API_KEY", v) },
            None => unsafe { std::env::remove_var("OPENAI_API_KEY") },
        }
    }

    #[test]
    fn with_model_overrides_default() {
        let backend = OpenAiBackend::new("sk-test".to_string()).with_model("whisper-1".to_string());
        assert_eq!(backend.model(), "whisper-1");
    }

    #[test]
    fn with_base_url_overrides_default() {
        let backend = OpenAiBackend::new("sk-test".to_string())
            .with_base_url("https://proxy.example.com/v1".to_string());
        assert_eq!(backend.base_url(), "https://proxy.example.com/v1");
    }

    #[test]
    fn f32_to_pcm16_round_trip_known_samples() {
        // Silence
        assert_eq!(f32_to_pcm16(0.0), 0);
        // Positive full scale
        assert_eq!(f32_to_pcm16(1.0), 32767);
        // Negative full scale (clamped at -32768 by cast — but 1.0 * -32767 = -32767)
        assert_eq!(f32_to_pcm16(-1.0), -32767);
        // Half scale, positive
        assert_eq!(f32_to_pcm16(0.5), (0.5_f32 * 32767.0).round() as i16);
        // Half scale, negative
        assert_eq!(f32_to_pcm16(-0.5), (-0.5_f32 * 32767.0).round() as i16);
        // Out-of-range saturates
        assert_eq!(f32_to_pcm16(2.0), 32767);
        assert_eq!(f32_to_pcm16(-2.0), -32767);
        // NaN safely maps to zero
        assert_eq!(f32_to_pcm16(f32::NAN), 0);
        // Infinity safely maps to zero
        assert_eq!(f32_to_pcm16(f32::INFINITY), 0);
    }

    #[test]
    fn pcm16_to_wav_has_riff_header_and_data() {
        let samples: Vec<i16> = vec![0, 1, -1, 32767, -32767];
        let wav = pcm16_to_wav(&samples, 16_000, 1);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[12..16], b"fmt ");
        assert_eq!(&wav[36..40], b"data");
        // 44-byte header + 5 samples * 2 bytes
        assert_eq!(wav.len(), 44 + samples.len() * 2);
    }

    #[test]
    fn response_into_events_uses_word_timestamps_when_present() {
        let body = serde_json::json!({
            "text": "hello world",
            "words": [
                {"word": "hello", "start": 0.0, "end": 0.5},
                {"word": "world", "start": 0.5, "end": 1.0},
            ],
        });
        let parsed: OpenAiTranscriptionResponse = serde_json::from_value(body).unwrap();
        let events = parsed.into_events();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].word, "hello");
        assert_eq!(events[0].start_ms, 0);
        assert_eq!(events[0].end_ms, 500);
        assert!((events[0].confidence - 1.0).abs() < f32::EPSILON);
        assert!(events[0].is_final);
        assert_eq!(events[1].word, "world");
        assert_eq!(events[1].start_ms, 500);
        assert_eq!(events[1].end_ms, 1000);
    }

    #[test]
    fn response_into_events_falls_back_to_text_when_no_words() {
        let body = serde_json::json!({"text": "fallback transcript"});
        let parsed: OpenAiTranscriptionResponse = serde_json::from_value(body).unwrap();
        let events = parsed.into_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].word, "fallback transcript");
        assert_eq!(events[0].start_ms, 0);
        assert_eq!(events[0].end_ms, 0);
    }

    #[test]
    fn response_into_events_empty_when_no_text_and_no_words() {
        let body = serde_json::json!({"text": ""});
        let parsed: OpenAiTranscriptionResponse = serde_json::from_value(body).unwrap();
        assert!(parsed.into_events().is_empty());
    }

    #[tokio::test]
    async fn transcribe_with_empty_audio_returns_empty_stream() {
        let backend = OpenAiBackend::new("sk-test".to_string());
        let (_tx, rx) = mpsc::channel::<AudioChunk>(1);
        // Drop tx immediately by shadowing -> actually we already have _tx; drop it.
        drop(_tx);
        let mut stream = backend.transcribe(rx).await.expect("transcribe");
        assert!(stream.next().await.is_none());
    }

    /// Live HTTP integration test. Run manually with:
    ///   `OPENAI_API_KEY=sk-... cargo test -p phantom-stt -- --ignored openai_live`
    #[tokio::test]
    #[ignore = "requires OPENAI_API_KEY and network"]
    async fn openai_live_one_second_silence_round_trip() {
        let backend = match OpenAiBackend::from_env() {
            Ok(b) => b,
            Err(_) => {
                eprintln!("skipping: OPENAI_API_KEY not set");
                return;
            }
        };

        let (tx, rx) = mpsc::channel::<AudioChunk>(4);
        // 1 second of silence at 16 kHz mono.
        let chunk = AudioChunk {
            samples: vec![0.0_f32; 16_000],
            sample_rate: 16_000,
            channels: 1,
            timestamp_ms: 0,
        };
        tx.send(chunk).await.expect("send");
        drop(tx);

        let mut stream = backend.transcribe(rx).await.expect("transcribe call");
        // We don't assert on the content of the response — silent audio may
        // produce zero or hallucinated words depending on the model. We only
        // assert the call succeeded (no SttError) and the stream completes.
        let mut count = 0usize;
        while let Some(item) = stream.next().await {
            item.expect("event ok");
            count += 1;
        }
        eprintln!("openai live test produced {count} events");
    }
}
