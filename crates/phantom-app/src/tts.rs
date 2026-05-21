//! TTS pipeline integration for phantom-app.
//!
//! Mirrors the [`crate::stt`] module but in the outbound direction: text goes
//! in, synthesized speech comes out through the system audio device.
//!
//! # Architecture
//!
//! The pipeline has two layers:
//!
//! 1. **Tokio worker** (`run_tts_worker`) — waits for text on an async mpsc
//!    channel, calls the [`VoiceSynth`] backend to get f32 PCM samples, and
//!    forwards the fully-collected samples to a blocking thread via a
//!    `std::sync::mpsc` channel.
//! 2. **Audio thread** (`run_audio_thread`) — a plain `std::thread::spawn`
//!    that owns the `rodio` `OutputStream` (which is `!Send`) and plays each
//!    received sample buffer sequentially.
//!
//! Splitting the two layers keeps the `rodio` audio objects off the async
//! executor (they are `!Send`) while still driving synthesis through the
//! async [`VoiceSynth`] API.
//!
//! # Lifecycle
//!
//! 1. At startup, call [`TtsPipeline::start`] with a backend.
//! 2. Whenever a full agent assistant message arrives, call
//!    [`TtsPipeline::speak`].
//! 3. Drop [`TtsPipeline`] to stop the worker gracefully.
//!
//! # Error handling
//!
//! All errors inside the worker and audio thread are logged; they never
//! surface to callers. [`TtsPipeline::speak`] returns `false` only when the
//! channel has closed (worker exited).

use std::sync::Arc;

use log::{debug, warn};
use tokio::sync::mpsc as async_mpsc;

use phantom_voice::{
    VoiceSynth,
    openai::OpenAiTtsBackend,
};

// ── TtsPipeline ───────────────────────────────────────────────────────────────

/// Handle to a running TTS worker task.
///
/// Drop this value to stop the worker (the `text_tx` drop closes the channel,
/// which terminates the worker's `recv()` loop).
pub struct TtsPipeline {
    /// Send text to the background synthesizer.  `None` when TTS is disabled.
    pub tts_tx: Option<async_mpsc::Sender<String>>,
}

/// Join handles for background tasks spawned by [`TtsPipeline::start`].
pub struct TtsTaskHandles {
    /// The Tokio task that drives synthesis (async).
    pub worker: tokio::task::JoinHandle<()>,
    /// The OS thread that owns the rodio output stream (sync).
    #[allow(dead_code)]
    audio_thread: std::thread::JoinHandle<()>,
}

impl TtsPipeline {
    /// Start the TTS pipeline with the given backend.
    ///
    /// Spawns:
    /// * A Tokio task that receives text, synthesizes PCM, and forwards it.
    /// * A dedicated OS thread that owns `rodio::OutputStream` (which is
    ///   `!Send`) and plays each buffer sequentially.
    ///
    /// Returns the pipeline handle and the task/thread join handles.
    pub fn start(
        backend: Arc<dyn VoiceSynth + Send + Sync>,
    ) -> (Self, TtsTaskHandles) {
        let (tts_tx, tts_rx) = async_mpsc::channel::<String>(32);

        // Channel from the async worker to the audio thread.
        // Each message is a fully-collected Vec<f32> of mono samples plus the
        // sample rate the backend reported.
        let (audio_tx, audio_rx) = std::sync::mpsc::sync_channel::<(Vec<f32>, u32)>(4);

        let audio_thread = std::thread::spawn(move || {
            run_audio_thread(audio_rx);
        });

        let worker = tokio::spawn(run_tts_worker(backend, tts_rx, audio_tx));

        let pipeline = Self {
            tts_tx: Some(tts_tx),
        };
        let handles = TtsTaskHandles {
            worker,
            audio_thread,
        };
        (pipeline, handles)
    }

    /// Create a no-op pipeline (TTS disabled).
    ///
    /// All [`TtsPipeline::speak`] calls return `false` immediately.
    #[must_use]
    pub fn disabled() -> Self {
        Self { tts_tx: None }
    }

    /// Send `text` to the background synthesizer.
    ///
    /// Returns `true` on success, `false` if TTS is disabled or the worker
    /// has already exited.
    #[must_use]
    pub fn speak(&self, text: String) -> bool {
        match &self.tts_tx {
            Some(tx) => tx.try_send(text).is_ok(),
            None => false,
        }
    }

    /// Returns `true` if the pipeline is active and accepting text.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.tts_tx
            .as_ref()
            .map(|tx| !tx.is_closed())
            .unwrap_or(false)
    }
}

// ── Audio thread (owns rodio — is !Send) ──────────────────────────────────────

/// Dedicated OS thread that owns the `rodio` output stream and plays each
/// (samples, sample_rate) pair it receives from the async worker.
///
/// Blocks until the sender is dropped.
fn run_audio_thread(rx: std::sync::mpsc::Receiver<(Vec<f32>, u32)>) {
    use phantom_voice::player::AudioPlayer;

    let player = match AudioPlayer::new() {
        Ok(p) => p,
        Err(e) => {
            warn!("[tts-audio-thread] failed to open audio device: {e} — audio playback disabled");
            return;
        }
    };

    while let Ok((samples, sample_rate)) = rx.recv() {
        if samples.is_empty() {
            continue;
        }

        // Encode the f32 samples as a WAV buffer and play via the player's
        // synchronous play_bytes path.
        let wav = encode_wav_f32(&samples, sample_rate);
        if let Err(e) = player.play_bytes(&wav) {
            warn!("[tts-audio-thread] play_bytes failed: {e}");
        }
    }

    debug!("[tts-audio-thread] channel closed — exiting");
}

/// Encode mono f32 samples as a 32-bit IEEE floating-point WAV buffer.
///
/// This is a minimal in-memory encoder that avoids pulling in a WAV library.
/// The format is:
///   - RIFF/WAVE header
///   - `fmt ` chunk with AudioFormat = 3 (IEEE float), 1 channel, given rate
///   - `data` chunk with raw little-endian f32 samples
fn encode_wav_f32(samples: &[f32], sample_rate: u32) -> Vec<u8> {
    let num_channels: u16 = 1;
    let bits_per_sample: u16 = 32;
    let byte_rate = sample_rate * u32::from(num_channels) * u32::from(bits_per_sample / 8);
    let block_align = num_channels * (bits_per_sample / 8);
    let data_size = (samples.len() * 4) as u32; // 4 bytes per f32
    let chunk_size = 36 + data_size;

    let mut wav = Vec::with_capacity(44 + samples.len() * 4);
    // RIFF header
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&chunk_size.to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    // fmt sub-chunk
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes()); // PCM fmt chunk size
    wav.extend_from_slice(&3u16.to_le_bytes()); // AudioFormat = IEEE float
    wav.extend_from_slice(&num_channels.to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&byte_rate.to_le_bytes());
    wav.extend_from_slice(&block_align.to_le_bytes());
    wav.extend_from_slice(&bits_per_sample.to_le_bytes());
    // data sub-chunk
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_size.to_le_bytes());
    for &s in samples {
        wav.extend_from_slice(&s.to_le_bytes());
    }
    wav
}

// ── Tokio worker ──────────────────────────────────────────────────────────────

/// Async worker: receive text, synthesize, collect samples, forward to audio thread.
async fn run_tts_worker(
    backend: Arc<dyn VoiceSynth + Send + Sync>,
    mut rx: async_mpsc::Receiver<String>,
    audio_tx: std::sync::mpsc::SyncSender<(Vec<f32>, u32)>,
) {
    // Resolve a voice profile once from the backend.
    let voice = match backend.list_voices().await {
        Ok(voices) if !voices.is_empty() => voices.into_iter().next().unwrap(),
        Ok(_) => {
            warn!("[tts-worker] backend returned no voices — TTS disabled");
            return;
        }
        Err(e) => {
            warn!("[tts-worker] list_voices failed: {e} — TTS disabled");
            return;
        }
    };

    while let Some(text) = rx.recv().await {
        if text.trim().is_empty() {
            continue;
        }

        debug!("[tts-worker] synthesizing: {} chars", text.len());

        let stream = match backend.synthesize(text, &voice).await {
            Ok(s) => s,
            Err(e) => {
                warn!("[tts-worker] synthesize failed: {e}");
                continue;
            }
        };

        // Collect all PCM chunks into a single buffer.
        let (all_samples, sample_rate) = collect_stream(stream).await;
        if all_samples.is_empty() {
            continue;
        }

        // Forward to the audio thread (non-blocking try_send: skip if full).
        if audio_tx.try_send((all_samples, sample_rate)).is_err() {
            warn!("[tts-worker] audio queue full — dropping utterance");
        }
    }

    debug!("[tts-worker] channel closed — exiting");
}

/// Drain a [`BoxedVoiceStream`] into a flat `Vec<f32>` and return the sample
/// rate from the last chunk (or 24_000 as a sensible default).
///
/// Uses `std::future::poll_fn` with `futures_core::Stream::poll_next` so we
/// don't need `futures-util` in phantom-app's dependency tree.
async fn collect_stream(
    stream: phantom_voice::BoxedVoiceStream,
) -> (Vec<f32>, u32) {
    use std::future::poll_fn;
    use std::pin::Pin;
    use futures_core::Stream as _;

    let mut pinned = stream;
    let mut all_samples: Vec<f32> = Vec::new();
    let mut sample_rate: u32 = 24_000;

    loop {
        let item = poll_fn(|cx| Pin::new(&mut pinned).poll_next(cx)).await;

        match item {
            Some(Ok(chunk)) => {
                sample_rate = chunk.sample_rate;
                all_samples.extend_from_slice(&chunk.samples);
            }
            Some(Err(e)) => {
                warn!("[tts-worker] stream error while collecting: {e}");
                break;
            }
            None => break,
        }
    }

    (all_samples, sample_rate)
}

// ── Startup helper ────────────────────────────────────────────────────────────

/// Try to build a [`TtsPipeline`] from environment variables.
///
/// * Checks `OPENAI_API_KEY`.  If set, constructs [`OpenAiTtsBackend`] and
///   starts the pipeline.
/// * Otherwise returns [`None`] (caller should store `tts: None` in `App`).
///
/// All failures are logged; this function never panics.
#[must_use]
pub fn build_tts_pipeline_from_env() -> Option<(TtsPipeline, TtsTaskHandles)> {
    match OpenAiTtsBackend::from_env() {
        Ok(backend) => {
            let arc: Arc<dyn VoiceSynth + Send + Sync> = Arc::new(backend);
            let (pipeline, handles) = TtsPipeline::start(arc);
            log::info!("TTS pipeline started (OpenAI backend)");
            Some((pipeline, handles))
        }
        Err(e) => {
            debug!("TTS pipeline disabled: {e}");
            None
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use phantom_voice::MockVoiceSynth;

    /// A [`TtsPipeline`] built from [`MockVoiceSynth`] must accept text without
    /// panicking and report `is_active() == true` before the channel closes.
    #[tokio::test]
    async fn tts_pipeline_sends_text_to_backend() {
        let backend: Arc<dyn VoiceSynth + Send + Sync> =
            Arc::new(MockVoiceSynth::new());

        let (pipeline, handles) = TtsPipeline::start(backend);

        assert!(pipeline.is_active(), "pipeline must be active after start");

        // Sending text should succeed.
        let sent = pipeline.speak("Hello, Phantom.".to_string());
        assert!(sent, "speak() must succeed while pipeline is active");

        // Drop the pipeline to close the channel so the worker exits.
        drop(pipeline);

        // Worker should exit cleanly (no panic).
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            handles.worker,
        )
        .await;
    }

    /// A disabled pipeline (no backend) must never panic and always return
    /// `false` from [`TtsPipeline::speak`].
    #[test]
    fn tts_pipeline_skips_when_no_backend() {
        let pipeline = TtsPipeline::disabled();
        assert!(
            !pipeline.is_active(),
            "disabled pipeline must not be active"
        );
        let sent = pipeline.speak("should be ignored".to_string());
        assert!(!sent, "speak() on disabled pipeline must return false");
    }

    /// When `OPENAI_API_KEY` is absent, [`build_tts_pipeline_from_env`] must
    /// return `None` without panicking.
    #[test]
    fn build_tts_pipeline_returns_none_without_api_key() {
        // Snapshot current env state.
        let prior = std::env::var("OPENAI_API_KEY").ok();
        // SAFETY: only this test touches the var; no parallel env mutation.
        unsafe { std::env::remove_var("OPENAI_API_KEY") };

        let result = build_tts_pipeline_from_env();

        unsafe {
            match prior {
                Some(v) => std::env::set_var("OPENAI_API_KEY", v),
                None => std::env::remove_var("OPENAI_API_KEY"),
            }
        }

        assert!(
            result.is_none(),
            "must return None when OPENAI_API_KEY is absent"
        );
    }
}
