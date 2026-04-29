//! Streaming STT processor: VAD-based segmentation over a live audio channel.
//!
//! [`SttStream`] takes a continuous `mpsc::Receiver<AudioChunk>` and emits
//! [`PartialTranscript`] values through a `mpsc::Sender`. The stream performs
//! voice-activity detection (VAD) by measuring RMS energy against a configurable
//! silence threshold. When silence exceeds the configured boundary duration the
//! accumulated segment is handed to the underlying [`TranscriptBackend`] and the
//! result is emitted as a final transcript. Interim partials are also emitted
//! every N milliseconds while audio is accumulating so the UX can show
//! in-progress text.
//!
//! # Streaming pipeline
//!
//! ```text
//! AudioChunk rx → accumulator → VAD gate → silence boundary?
//!                                              ├─ yes → transcribe(segment) → final partial
//!                                              └─ no  → every N ms emit interim partial
//! ```
//!
//! # Design notes
//!
//! * No network calls are on the hot path: the backend is only invoked at
//!   segment boundaries, so typical chunk-to-interim latency is deterministic
//!   and bounded by `interim_interval_ms`.
//! * `SttStream` is a value type: calling [`SttStream::run`] consumes it.
//!   Construct a new one for each recording session.
//! * All errors inside `run` are logged and short-circuit the current segment —
//!   the loop continues so transient failures don't kill the session.

use std::sync::Arc;

use tokio::sync::mpsc;

use crate::{AudioChunk, TranscriptBackend, TranscriptEvent};

// ── Default tuning constants ──────────────────────────────────────────────────

/// Default silence boundary: 500 ms of audio below the energy threshold
/// triggers a segment flush.
const DEFAULT_SILENCE_BOUNDARY_MS: u64 = 500;

/// Default interim emission interval: every 2 s while accumulating audio we
/// emit a partial so the UI shows that transcription is in-progress.
const DEFAULT_INTERIM_INTERVAL_MS: u64 = 2_000;

/// Default VAD energy threshold: RMS below this value is considered silence.
/// The f32 range is `[0.0, 1.0]`; 0.01 ≈ −40 dBFS.
const DEFAULT_SILENCE_THRESHOLD: f32 = 0.01;

/// Minimum voice-segment duration before we bother calling the backend.
/// Avoids wasting API quota on sub-50 ms clicks or noise bursts.
const MIN_SEGMENT_MS: u64 = 50;

// ── Public types ─────────────────────────────────────────────────────────────

/// A transcript result emitted by [`SttStream`].
///
/// Both interim (in-progress) and final results share this type. Callers
/// should check [`PartialTranscript::is_final`] to distinguish them.
#[derive(Debug, Clone)]
pub struct PartialTranscript {
    /// Accumulated text for this segment, space-joined from received words.
    ///
    /// For interim events this is empty — the segment is still accumulating.
    /// For final events this is the fully transcribed text.
    pub text: String,

    /// Average confidence across all final-flagged [`TranscriptEvent`]s in this
    /// segment. `1.0` for interim placeholders (no real confidence yet).
    pub confidence: f32,

    /// Wall-clock time when this partial was produced, in Unix milliseconds.
    pub timestamp_ms: u64,

    /// `true` when this partial represents a completed, committed transcript
    /// segment. `false` for in-progress hints.
    pub is_final: bool,
}

/// Configuration knobs for [`SttStream`].
///
/// All durations are in milliseconds to stay consistent with [`AudioChunk`]'s
/// `timestamp_ms` field.
#[derive(Debug, Clone)]
pub struct SttStreamConfig {
    silence_threshold: f32,
    silence_boundary_ms: u64,
    interim_interval_ms: u64,
}

impl Default for SttStreamConfig {
    fn default() -> Self {
        Self {
            silence_threshold: DEFAULT_SILENCE_THRESHOLD,
            silence_boundary_ms: DEFAULT_SILENCE_BOUNDARY_MS,
            interim_interval_ms: DEFAULT_INTERIM_INTERVAL_MS,
        }
    }
}

impl SttStreamConfig {
    /// Construct a default config.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Override the silence energy threshold (RMS, `[0.0, 1.0]`).
    #[must_use]
    pub fn with_silence_threshold(mut self, threshold: f32) -> Self {
        self.silence_threshold = threshold.clamp(0.0, 1.0);
        self
    }

    /// Override the silence boundary in milliseconds.
    #[must_use]
    pub fn with_silence_boundary_ms(mut self, ms: u64) -> Self {
        self.silence_boundary_ms = ms;
        self
    }

    /// Override the interim emission interval in milliseconds.
    #[must_use]
    pub fn with_interim_interval_ms(mut self, ms: u64) -> Self {
        self.interim_interval_ms = ms;
        self
    }

    /// Read the configured silence threshold.
    #[must_use]
    pub fn silence_threshold(&self) -> f32 {
        self.silence_threshold
    }

    /// Read the configured silence boundary.
    #[must_use]
    pub fn silence_boundary_ms(&self) -> u64 {
        self.silence_boundary_ms
    }

    /// Read the configured interim interval.
    #[must_use]
    pub fn interim_interval_ms(&self) -> u64 {
        self.interim_interval_ms
    }
}

/// Continuous STT processor that segments audio by voice-activity detection.
///
/// Construct with [`SttStream::new`], then call [`SttStream::run`] to drive
/// the loop. The loop exits when `audio_rx` is closed (sender dropped).
///
/// ```no_run
/// # async fn example() {
/// use std::sync::Arc;
/// use tokio::sync::mpsc;
/// use phantom_stt::{AudioChunk, MockBackend};
/// use phantom_stt::stream::{SttStream, SttStreamConfig};
///
/// let (audio_tx, audio_rx) = mpsc::channel::<AudioChunk>(64);
/// let (partial_tx, mut partial_rx) = mpsc::channel(64);
/// let backend = Arc::new(MockBackend::new());
/// let stream = SttStream::new(backend, SttStreamConfig::default());
///
/// // Drive in a background task.
/// tokio::spawn(async move {
///     stream.run(audio_rx, partial_tx).await;
/// });
///
/// // Send audio then close.
/// drop(audio_tx);
///
/// while let Some(partial) = partial_rx.recv().await {
///     println!("{}: {:?}", if partial.is_final { "FINAL" } else { "interim" }, partial.text);
/// }
/// # }
/// ```
pub struct SttStream {
    backend: Arc<dyn TranscriptBackend>,
    config: SttStreamConfig,
}

impl SttStream {
    /// Create a new [`SttStream`] bound to `backend` with the given `config`.
    #[must_use]
    pub fn new(backend: Arc<dyn TranscriptBackend>, config: SttStreamConfig) -> Self {
        Self { backend, config }
    }

    /// Drive the continuous transcription loop.
    ///
    /// Reads from `audio_rx` until closed, segments audio at silence
    /// boundaries, and emits [`PartialTranscript`] values through
    /// `partial_tx`. Drops `partial_tx` when the loop exits.
    ///
    /// Timing is based on **audio content time** (sample count ÷ sample rate)
    /// rather than wall-clock time so the loop is both accurate for real audio
    /// and deterministic in tests that send chunks in tight loops.
    ///
    /// This method is `async` and should be spawned onto a tokio runtime.
    pub async fn run(
        self,
        mut audio_rx: mpsc::Receiver<AudioChunk>,
        partial_tx: mpsc::Sender<PartialTranscript>,
    ) {
        let silence_threshold = self.config.silence_threshold;
        let silence_boundary_ms = self.config.silence_boundary_ms;
        let interim_interval_ms = self.config.interim_interval_ms;

        // Accumulated chunks for the current voice segment.
        let mut segment: Vec<AudioChunk> = Vec::new();
        // Total audio-content milliseconds accumulated in the current segment.
        let mut segment_audio_ms: u64 = 0;
        // Audio-content milliseconds of consecutive silence since the last
        // voice chunk.
        let mut silence_audio_ms: u64 = 0;
        // Audio-content milliseconds of voice since the last interim emit.
        let mut since_interim_ms: u64 = 0;

        while let Some(chunk) = audio_rx.recv().await {
            let energy = rms_energy(&chunk.samples);
            let is_voice = energy >= silence_threshold;

            // Compute the audio duration of this chunk in milliseconds.
            let sample_rate = chunk.sample_rate.max(1) as u64;
            let channels = chunk.channels.max(1) as u64;
            let total_samples = chunk.samples.len() as u64;
            // Interleaved samples: frame count = total_samples / channels.
            let chunk_ms = total_samples.saturating_div(channels) * 1_000 / sample_rate;

            if is_voice {
                silence_audio_ms = 0;
                segment.push(chunk);
                segment_audio_ms += chunk_ms;
                since_interim_ms += chunk_ms;

                // Emit interim partial once per interim_interval_ms of voice.
                if since_interim_ms >= interim_interval_ms {
                    since_interim_ms = 0;
                    let partial = PartialTranscript {
                        text: String::new(),
                        confidence: 1.0,
                        timestamp_ms: unix_ms_now(),
                        is_final: false,
                    };
                    if partial_tx.send(partial).await.is_err() {
                        return;
                    }
                }
            } else {
                // Silence chunk.
                silence_audio_ms += chunk_ms;

                if silence_audio_ms >= silence_boundary_ms && !segment.is_empty() {
                    let segment_chunks = std::mem::take(&mut segment);
                    let seg_ms = segment_audio_ms;
                    segment_audio_ms = 0;
                    silence_audio_ms = 0;
                    since_interim_ms = 0;

                    if seg_ms >= MIN_SEGMENT_MS {
                        if let Some(p) =
                            transcribe_segment(&*self.backend, segment_chunks).await
                        {
                            if partial_tx.send(p).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            }
        }

        // Audio channel closed — flush any remaining segment.
        if !segment.is_empty() {
            if let Some(p) = transcribe_segment(&*self.backend, segment).await {
                let _ = partial_tx.send(p).await;
            }
        }
    }
}

// ── Internal helpers ─────────────────────────────────────────────────────────

/// Compute the root-mean-square energy of a sample slice.
///
/// Returns `0.0` for an empty slice. Result is in `[0.0, 1.0]` when all
/// samples are within the f32 PCM range.
fn rms_energy(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f32 = samples.iter().map(|s| s * s).sum();
    (sum_sq / samples.len() as f32).sqrt()
}

/// Current wall-clock time in Unix milliseconds.
fn unix_ms_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Hand a collected segment to the backend and return a final [`PartialTranscript`].
///
/// Returns `None` if the backend produced no events.
async fn transcribe_segment(
    backend: &dyn TranscriptBackend,
    chunks: Vec<AudioChunk>,
) -> Option<PartialTranscript> {
    use futures_util::StreamExt;

    let cap = chunks.len().max(1);
    let (seg_tx, seg_rx) = mpsc::channel::<AudioChunk>(cap);
    for c in chunks {
        if seg_tx.send(c).await.is_err() {
            break;
        }
    }
    drop(seg_tx);

    let mut event_stream = match backend.transcribe(seg_rx).await {
        Ok(s) => s,
        Err(e) => {
            log::warn!("stt backend transcribe failed: {e}");
            return None;
        }
    };

    let mut words: Vec<TranscriptEvent> = Vec::new();
    while let Some(item) = event_stream.next().await {
        match item {
            Ok(ev) if ev.is_final => words.push(ev),
            Ok(_) => {}
            Err(e) => log::warn!("stt stream error: {e}"),
        }
    }

    if words.is_empty() {
        return None;
    }

    let text = words
        .iter()
        .map(|w| w.word.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let confidence = {
        let sum: f32 = words.iter().map(|w| w.confidence).sum();
        sum / words.len() as f32
    };

    Some(PartialTranscript {
        text,
        confidence,
        timestamp_ms: unix_ms_now(),
        is_final: true,
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MockBackend, TranscriptEvent};

    fn word(text: &str, start_ms: u64, end_ms: u64) -> TranscriptEvent {
        TranscriptEvent {
            word: text.to_string(),
            start_ms,
            end_ms,
            confidence: 0.9,
            is_final: true,
        }
    }

    fn voice_chunk(amplitude: f32, num_samples: usize) -> AudioChunk {
        AudioChunk {
            samples: vec![amplitude; num_samples],
            sample_rate: 16_000,
            channels: 1,
            timestamp_ms: 0,
        }
    }

    fn silence_chunk(num_samples: usize) -> AudioChunk {
        AudioChunk {
            samples: vec![0.0; num_samples],
            sample_rate: 16_000,
            channels: 1,
            timestamp_ms: 0,
        }
    }

    // ── rms_energy ────────────────────────────────────────────────────────────

    #[test]
    fn rms_energy_empty_slice_returns_zero() {
        assert_eq!(rms_energy(&[]), 0.0);
    }

    #[test]
    fn rms_energy_silence_returns_zero() {
        assert_eq!(rms_energy(&[0.0, 0.0, 0.0]), 0.0);
    }

    #[test]
    fn rms_energy_full_scale_returns_one() {
        assert!((rms_energy(&[1.0, 1.0, 1.0]) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn rms_energy_half_scale_returns_half() {
        assert!((rms_energy(&[0.5, 0.5, 0.5]) - 0.5).abs() < 1e-5);
    }

    // ── SttStreamConfig ───────────────────────────────────────────────────────

    #[test]
    fn config_default_values_match_constants() {
        let cfg = SttStreamConfig::default();
        assert!((cfg.silence_threshold() - DEFAULT_SILENCE_THRESHOLD).abs() < f32::EPSILON);
        assert_eq!(cfg.silence_boundary_ms(), DEFAULT_SILENCE_BOUNDARY_MS);
        assert_eq!(cfg.interim_interval_ms(), DEFAULT_INTERIM_INTERVAL_MS);
    }

    #[test]
    fn config_builder_overrides_all_fields() {
        let cfg = SttStreamConfig::new()
            .with_silence_threshold(0.05)
            .with_silence_boundary_ms(200)
            .with_interim_interval_ms(500);
        assert!((cfg.silence_threshold() - 0.05).abs() < f32::EPSILON);
        assert_eq!(cfg.silence_boundary_ms(), 200);
        assert_eq!(cfg.interim_interval_ms(), 500);
    }

    #[test]
    fn config_threshold_clamps_to_unit_range() {
        let cfg = SttStreamConfig::new().with_silence_threshold(5.0);
        assert!((cfg.silence_threshold() - 1.0).abs() < f32::EPSILON);
        let cfg2 = SttStreamConfig::new().with_silence_threshold(-1.0);
        assert_eq!(cfg2.silence_threshold(), 0.0);
    }

    // ── SttStream end-to-end ──────────────────────────────────────────────────

    /// A single audio segment terminated by silence emits one final partial
    /// with the mock transcript text.
    #[tokio::test]
    async fn streaming_single_segment_emits_final_transcript() {
        let transcript = vec![word("hello", 0, 300), word("world", 350, 600)];
        let backend = Arc::new(MockBackend::with_transcript(transcript));

        let config = SttStreamConfig::new()
            .with_silence_boundary_ms(50)
            .with_silence_threshold(0.01)
            .with_interim_interval_ms(9999); // suppress interims

        let (audio_tx, audio_rx) = mpsc::channel::<AudioChunk>(16);
        let (partial_tx, mut partial_rx) = mpsc::channel::<PartialTranscript>(16);

        let stream = SttStream::new(backend, config);
        let run_handle = tokio::spawn(async move {
            stream.run(audio_rx, partial_tx).await;
        });

        // Voice: amplitude well above threshold.
        for _ in 0..5 {
            audio_tx
                .send(voice_chunk(0.5, 160))
                .await
                .expect("send voice");
        }
        // Silence to trigger segment flush (silence_boundary = 50 ms,
        // 160 samples at 16 kHz ≈ 10 ms per chunk → 5 chunks ≈ 50 ms).
        for _ in 0..5 {
            audio_tx
                .send(silence_chunk(160))
                .await
                .expect("send silence");
        }
        drop(audio_tx);
        run_handle.await.expect("runner didn't panic");

        let mut finals: Vec<PartialTranscript> = Vec::new();
        while let Some(p) = partial_rx.recv().await {
            if p.is_final {
                finals.push(p);
            }
        }

        assert!(!finals.is_empty(), "expected at least one final partial");
        assert_eq!(finals[0].text, "hello world");
        assert!(finals[0].confidence > 0.0 && finals[0].confidence <= 1.0);
    }

    /// Closing the audio channel without sending any audio drains cleanly.
    #[tokio::test]
    async fn streaming_empty_audio_channel_closes_cleanly() {
        let backend = Arc::new(MockBackend::new());
        let config = SttStreamConfig::new()
            .with_silence_boundary_ms(50)
            .with_interim_interval_ms(9999);

        let (_audio_tx, audio_rx) = mpsc::channel::<AudioChunk>(4);
        let (partial_tx, mut partial_rx) = mpsc::channel::<PartialTranscript>(4);

        drop(_audio_tx);

        let stream = SttStream::new(backend, config);
        stream.run(audio_rx, partial_tx).await;

        assert!(partial_rx.recv().await.is_none(), "no partials from empty input");
    }

    /// Three consecutive voice segments (separated by silence) each produce
    /// their own final partial — verifying the multi-segment path.
    #[tokio::test]
    async fn streaming_multiple_segments_yield_multiple_finals() {
        let words = vec![word("one", 0, 100)];
        let backend = Arc::new(MockBackend::with_transcript(words));

        let config = SttStreamConfig::new()
            .with_silence_boundary_ms(50)
            .with_silence_threshold(0.01)
            .with_interim_interval_ms(9999);

        let (audio_tx, audio_rx) = mpsc::channel::<AudioChunk>(64);
        let (partial_tx, mut partial_rx) = mpsc::channel::<PartialTranscript>(64);

        let stream = SttStream::new(backend, config);
        let handle = tokio::spawn(async move {
            stream.run(audio_rx, partial_tx).await;
        });

        for _ in 0..3 {
            for _ in 0..5 {
                audio_tx
                    .send(voice_chunk(0.5, 160))
                    .await
                    .expect("send voice");
            }
            for _ in 0..5 {
                audio_tx
                    .send(silence_chunk(160))
                    .await
                    .expect("send silence");
            }
        }
        drop(audio_tx);
        handle.await.expect("runner didn't panic");

        let mut finals = Vec::new();
        while let Some(p) = partial_rx.recv().await {
            if p.is_final {
                finals.push(p);
            }
        }
        assert_eq!(
            finals.len(),
            3,
            "expected 3 final partials, got {}",
            finals.len()
        );
    }

    /// Interim partials are emitted while voice is accumulating.
    #[tokio::test]
    async fn streaming_emits_interim_partials_while_accumulating() {
        let backend = Arc::new(MockBackend::with_transcript(vec![word("test", 0, 100)]));

        let config = SttStreamConfig::new()
            .with_silence_boundary_ms(50)
            .with_silence_threshold(0.01)
            .with_interim_interval_ms(1); // fire after every tick

        let (audio_tx, audio_rx) = mpsc::channel::<AudioChunk>(128);
        let (partial_tx, mut partial_rx) = mpsc::channel::<PartialTranscript>(128);

        let stream = SttStream::new(backend, config);
        let handle = tokio::spawn(async move {
            stream.run(audio_rx, partial_tx).await;
        });

        for _ in 0..20 {
            audio_tx
                .send(voice_chunk(0.5, 160))
                .await
                .expect("send voice");
            tokio::task::yield_now().await;
        }
        for _ in 0..5 {
            audio_tx
                .send(silence_chunk(160))
                .await
                .expect("send silence");
        }
        drop(audio_tx);
        handle.await.expect("runner didn't panic");

        let mut interims = 0usize;
        let mut finals = 0usize;
        while let Some(p) = partial_rx.recv().await {
            if p.is_final {
                finals += 1;
            } else {
                interims += 1;
            }
        }
        assert!(interims > 0, "expected at least one interim partial");
        assert!(finals > 0, "expected at least one final partial");
    }

    // ── Compile-time API checks ───────────────────────────────────────────────

    #[allow(dead_code)]
    fn assert_stt_stream_accepts_arc_dyn(b: Arc<dyn TranscriptBackend>) {
        let _ = SttStream::new(b, SttStreamConfig::default());
    }

    #[allow(dead_code)]
    fn assert_partial_transcript_is_send(p: PartialTranscript) -> impl Send {
        p
    }

    // ── Issue #68 acceptance test ─────────────────────────────────────────────

    /// Simulates 30 s of audio at 16 kHz with 3 voice segments. Acceptance
    /// criterion from issue #68: ≥ 3 final transcripts.
    #[tokio::test]
    async fn streaming_30s_clip_yields_at_least_3_segments() {
        let words = vec![word("segment", 0, 500)];
        let backend = Arc::new(MockBackend::with_transcript(words));

        let config = SttStreamConfig::new()
            .with_silence_boundary_ms(50)
            .with_silence_threshold(0.01)
            .with_interim_interval_ms(9999);

        let (audio_tx, audio_rx) = mpsc::channel::<AudioChunk>(512);
        let (partial_tx, mut partial_rx) = mpsc::channel::<PartialTranscript>(256);

        let stream = SttStream::new(backend, config);
        let handle = tokio::spawn(async move {
            stream.run(audio_rx, partial_tx).await;
        });

        // Three 10-second voice segments (16_000 samples/s) + 1 s silence each.
        let voice_samples = 16_000usize * 10;
        let silence_samples = 16_000usize;

        for _ in 0..3 {
            let mut rem = voice_samples;
            while rem > 0 {
                let n = rem.min(160);
                audio_tx
                    .send(AudioChunk {
                        samples: vec![0.5; n],
                        sample_rate: 16_000,
                        channels: 1,
                        timestamp_ms: 0,
                    })
                    .await
                    .expect("send voice");
                rem -= n;
            }
            let mut rem = silence_samples;
            while rem > 0 {
                let n = rem.min(160);
                audio_tx
                    .send(AudioChunk {
                        samples: vec![0.0; n],
                        sample_rate: 16_000,
                        channels: 1,
                        timestamp_ms: 0,
                    })
                    .await
                    .expect("send silence");
                rem -= n;
            }
        }
        drop(audio_tx);
        handle.await.expect("runner didn't panic");

        let mut finals: Vec<PartialTranscript> = Vec::new();
        while let Some(p) = partial_rx.recv().await {
            if p.is_final {
                finals.push(p);
            }
        }
        assert!(
            finals.len() >= 3,
            "expected ≥ 3 final partials from 30 s clip, got {}",
            finals.len()
        );
    }
}
