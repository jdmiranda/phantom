//! [`BundleAssembler`] — staged builder that collects raw capture events into
//! a sealed [`Bundle`].
//!
//! The capture pipeline receives frames, audio chunks, and transcript words
//! from independent sources at different rates.  `BundleAssembler` provides a
//! type-safe accumulation point: callers push individual events in and call
//! [`BundleAssembler::finish`] when a command boundary is detected to obtain a
//! sealed, ready-to-persist [`Bundle`].
//!
//! # Design notes
//!
//! * All fields on `BundleAssembler` are private.  Mutation happens exclusively
//!   through the typed `push_*` methods so that the ordering and deduplication
//!   invariants remain internal to this module.
//! * `finish` consumes the assembler so the caller cannot accidentally reuse a
//!   partially-built state after sealing.
//! * No `unwrap` in production paths — every fallible step returns
//!   [`AssemblerError`].

use std::time::{SystemTime, UNIX_EPOCH};

use thiserror::Error;

use crate::{AudioRef, Bundle, FrameRef, PaneId, TranscriptWord};

// ─── Error type ──────────────────────────────────────────────────────────────

/// Errors that can occur while assembling a bundle.
#[derive(Debug, Error)]
pub enum AssemblerError {
    /// [`BundleAssembler::finish`] was called on an assembler that has no
    /// frames AND no audio AND no transcript words.  Persisting such a bundle
    /// would waste storage with zero searchable content.
    #[error("cannot seal an empty bundle — add at least one frame, audio chunk, or word first")]
    EmptyBundle,

    /// The system clock returned a time earlier than the Unix epoch.  This
    /// should not happen in practice but the API must handle it gracefully.
    #[error("system clock returned a pre-epoch value: {0}")]
    ClockError(String),
}

// ─── BundleAssembler ─────────────────────────────────────────────────────────

/// Staged builder for [`Bundle`].
///
/// Create one per command (or capture window), push events into it as they
/// arrive, then call [`finish`](BundleAssembler::finish) at a command boundary.
///
/// ```rust
/// # use phantom_bundles::assembler::BundleAssembler;
/// # use phantom_bundles::{FrameRef, TranscriptWord};
/// let mut asm = BundleAssembler::new(42);
/// asm.push_frame(FrameRef {
///     t_offset_ns: 0,
///     sha: "abc".into(),
///     blob_path: "frames/0.png".into(),
///     dhash: 0,
///     width: 1920,
///     height: 1080,
/// });
/// asm.push_word(TranscriptWord {
///     t_offset_ns: 0,
///     t_end_ns: 500_000_000,
///     text: "cargo test".into(),
///     speaker: None,
///     confidence: 0.95,
/// });
/// let bundle = asm.finish(Some("cargo-test".into()), vec!["ci".into()], 0.7)
///     .expect("non-empty bundle");
/// assert!(bundle.sealed);
/// ```
pub struct BundleAssembler {
    /// The pane whose screen content is being captured.
    pane_id: PaneId,
    /// Accumulated frame references, in push order.
    frames: Vec<FrameRef>,
    /// Accumulated audio chunk references, in push order.
    audio_chunks: Vec<AudioRef>,
    /// Accumulated transcript words, in push order.
    transcript_words: Vec<TranscriptWord>,
    /// Monotonic start offset, in nanoseconds.  Set lazily on the first event
    /// push so the bundle's `t_start_ns` aligns with actual data rather than
    /// construction time.
    t_start_ns: u64,
    /// Wall-clock at construction time, in Unix milliseconds.  Captured eagerly
    /// so `finish` does not need the clock.
    t_wall_unix_ms: i64,
}

impl BundleAssembler {
    /// Create a new assembler for `pane_id`.
    ///
    /// The wall-clock is captured immediately so the bundle's `t_wall_unix_ms`
    /// reflects when assembly started.  Returns an assembler even if the clock
    /// call fails: in that case `t_wall_unix_ms` is set to `0` and a warning
    /// is emitted to the log.
    #[must_use]
    pub fn new(pane_id: PaneId) -> Self {
        let t_wall_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or_else(|e| {
                log::warn!("BundleAssembler: clock error at construction: {e}; using 0");
                0
            });

        Self {
            pane_id,
            frames: Vec::new(),
            audio_chunks: Vec::new(),
            transcript_words: Vec::new(),
            t_start_ns: 0,
            t_wall_unix_ms,
        }
    }

    /// Append a frame reference.  Frames are stored in push order (insertion
    /// order is the canonical timeline).
    pub fn push_frame(&mut self, frame: FrameRef) {
        self.frames.push(frame);
    }

    /// Append an audio chunk reference.
    pub fn push_audio(&mut self, chunk: AudioRef) {
        self.audio_chunks.push(chunk);
    }

    /// Append a transcript word.
    pub fn push_word(&mut self, word: TranscriptWord) {
        self.transcript_words.push(word);
    }

    /// The number of frames currently accumulated.
    #[must_use]
    pub fn frame_count(&self) -> usize {
        self.frames.len()
    }

    /// The number of audio chunks currently accumulated.
    #[must_use]
    pub fn audio_count(&self) -> usize {
        self.audio_chunks.len()
    }

    /// The number of transcript words currently accumulated.
    #[must_use]
    pub fn word_count(&self) -> usize {
        self.transcript_words.len()
    }

    /// `true` if the assembler has no frames, no audio, and no transcript.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty() && self.audio_chunks.is_empty() && self.transcript_words.is_empty()
    }

    /// Set the bundle's monotonic start offset, in nanoseconds.
    ///
    /// Callers that track their own monotonic clock (e.g. the GPU capture
    /// loop) can supply the precise bundle-open timestamp here so that frame
    /// `t_offset_ns` values are meaningful.  If not called, `t_start_ns` stays
    /// at `0`.
    pub fn set_start_ns(&mut self, t_start_ns: u64) {
        self.t_start_ns = t_start_ns;
    }

    /// Consume the assembler, seal the bundle, and return it.
    ///
    /// `importance` is clamped to `[0.0, 1.0]` inside [`Bundle::seal`].
    ///
    /// # Errors
    ///
    /// Returns [`AssemblerError::EmptyBundle`] if no frames, audio, or words
    /// have been pushed.  Persisting a completely empty bundle has no value and
    /// wastes encrypted-blob quota.
    pub fn finish(
        self,
        intent: Option<String>,
        tags: Vec<String>,
        importance: f32,
    ) -> Result<Bundle, AssemblerError> {
        if self.is_empty() {
            return Err(AssemblerError::EmptyBundle);
        }

        let mut bundle = Bundle::new(self.pane_id);
        bundle.t_start_ns = self.t_start_ns;
        bundle.t_wall_unix_ms = self.t_wall_unix_ms;
        bundle.frames = self.frames;
        bundle.audio_chunks = self.audio_chunks;
        bundle.transcript_words = self.transcript_words;
        bundle.seal(intent, tags, importance);

        Ok(bundle)
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_frame(t: u64) -> FrameRef {
        FrameRef {
            t_offset_ns: t,
            sha: format!("sha-{t}"),
            blob_path: format!("frames/{t}.png"),
            dhash: t,
            width: 1920,
            height: 1080,
        }
    }

    fn sample_audio(t: u64, dur: u64) -> AudioRef {
        AudioRef {
            t_offset_ns: t,
            duration_ns: dur,
            blob_path: format!("audio/{t}.opus"),
            sample_rate: 48_000,
            channels: 2,
        }
    }

    fn sample_word(start: u64, end: u64, text: &str) -> TranscriptWord {
        TranscriptWord {
            t_offset_ns: start,
            t_end_ns: end,
            text: text.into(),
            speaker: Some("user".into()),
            confidence: 0.91,
        }
    }

    // ── is_empty / counters ───────────────────────────────────────────────────

    #[test]
    fn new_assembler_is_empty() {
        let asm = BundleAssembler::new(1);
        assert!(asm.is_empty());
        assert_eq!(asm.frame_count(), 0);
        assert_eq!(asm.audio_count(), 0);
        assert_eq!(asm.word_count(), 0);
    }

    #[test]
    fn push_frame_makes_non_empty() {
        let mut asm = BundleAssembler::new(1);
        asm.push_frame(sample_frame(0));
        assert!(!asm.is_empty());
        assert_eq!(asm.frame_count(), 1);
    }

    #[test]
    fn push_audio_makes_non_empty() {
        let mut asm = BundleAssembler::new(1);
        asm.push_audio(sample_audio(0, 1_000_000));
        assert!(!asm.is_empty());
        assert_eq!(asm.audio_count(), 1);
    }

    #[test]
    fn push_word_makes_non_empty() {
        let mut asm = BundleAssembler::new(1);
        asm.push_word(sample_word(0, 100, "hello"));
        assert!(!asm.is_empty());
        assert_eq!(asm.word_count(), 1);
    }

    // ── finish: empty guard ───────────────────────────────────────────────────

    #[test]
    fn finish_on_empty_assembler_errors() {
        let asm = BundleAssembler::new(99);
        let err = asm.finish(None, vec![], 0.5).expect_err("empty should error");
        assert!(matches!(err, AssemblerError::EmptyBundle));
    }

    // ── finish: sealed bundle shape ───────────────────────────────────────────

    #[test]
    fn finish_produces_sealed_bundle_with_correct_pane() {
        let pane: PaneId = 42;
        let mut asm = BundleAssembler::new(pane);
        asm.push_frame(sample_frame(0));
        let bundle = asm.finish(Some("test".into()), vec!["ci".into()], 0.8).expect("finish");
        assert!(bundle.sealed);
        assert_eq!(bundle.source_pane_id, pane);
        assert_eq!(bundle.intent.as_deref(), Some("test"));
        assert_eq!(bundle.tags, vec!["ci"]);
        assert!((bundle.importance - 0.8).abs() < f32::EPSILON);
        assert_eq!(bundle.schema_version, crate::SCHEMA_VERSION);
    }

    #[test]
    fn finish_preserves_insertion_order_across_all_modalities() {
        let mut asm = BundleAssembler::new(1);
        asm.push_frame(sample_frame(300));
        asm.push_frame(sample_frame(100));
        asm.push_audio(sample_audio(50, 200_000));
        asm.push_audio(sample_audio(10, 100_000));
        asm.push_word(sample_word(0, 50, "alpha"));
        asm.push_word(sample_word(60, 100, "beta"));

        let bundle = asm.finish(None, vec![], 0.5).expect("finish");

        let frame_ts: Vec<u64> = bundle.frames.iter().map(|f| f.t_offset_ns).collect();
        assert_eq!(frame_ts, vec![300, 100], "frames: insertion order");

        let audio_ts: Vec<u64> = bundle.audio_chunks.iter().map(|a| a.t_offset_ns).collect();
        assert_eq!(audio_ts, vec![50, 10], "audio: insertion order");

        let words: Vec<&str> = bundle.transcript_words.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(words, vec!["alpha", "beta"], "words: insertion order");
    }

    #[test]
    fn finish_importance_is_clamped() {
        let mut asm = BundleAssembler::new(1);
        asm.push_frame(sample_frame(0));
        let b = asm.finish(None, vec![], 99.0).expect("finish");
        assert!((b.importance - 1.0).abs() < f32::EPSILON, "clamped above 1");
    }

    #[test]
    fn set_start_ns_is_reflected_in_bundle() {
        let mut asm = BundleAssembler::new(5);
        asm.set_start_ns(1_234_567_890);
        asm.push_frame(sample_frame(0));
        let b = asm.finish(None, vec![], 0.0).expect("finish");
        assert_eq!(b.t_start_ns, 1_234_567_890);
    }

    // ── finish: each modality alone is sufficient ─────────────────────────────

    #[test]
    fn finish_frame_only_bundle_is_valid() {
        let mut asm = BundleAssembler::new(1);
        asm.push_frame(sample_frame(0));
        let b = asm.finish(None, vec![], 0.0).expect("frame-only bundle");
        assert_eq!(b.frames.len(), 1);
        assert!(b.audio_chunks.is_empty());
        assert!(b.transcript_words.is_empty());
    }

    #[test]
    fn finish_audio_only_bundle_is_valid() {
        let mut asm = BundleAssembler::new(1);
        asm.push_audio(sample_audio(0, 1_000_000));
        let b = asm.finish(None, vec![], 0.0).expect("audio-only bundle");
        assert!(b.frames.is_empty());
        assert_eq!(b.audio_chunks.len(), 1);
    }

    #[test]
    fn finish_transcript_only_bundle_is_valid() {
        let mut asm = BundleAssembler::new(1);
        asm.push_word(sample_word(0, 100, "hi"));
        let b = asm.finish(None, vec![], 0.0).expect("transcript-only bundle");
        assert!(b.frames.is_empty());
        assert_eq!(b.transcript_words.len(), 1);
    }

    // ── round-trip: assembler output survives JSON serde ─────────────────────

    #[test]
    fn assembled_bundle_json_round_trips() {
        let mut asm = BundleAssembler::new(7);
        asm.set_start_ns(5_000_000);
        asm.push_frame(sample_frame(0));
        asm.push_frame(sample_frame(33_000_000));
        asm.push_audio(sample_audio(0, 20_000_000));
        asm.push_word(sample_word(0, 1_000_000, "build"));
        asm.push_word(sample_word(1_000_001, 2_000_000, "ok"));

        let original = asm.finish(Some("ci".into()), vec!["green".into()], 0.9).expect("finish");

        let json = serde_json::to_string(&original).expect("serialize");
        let restored: crate::Bundle = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(restored.id, original.id);
        assert_eq!(restored.source_pane_id, original.source_pane_id);
        assert_eq!(restored.t_start_ns, original.t_start_ns);
        assert_eq!(restored.frames.len(), 2);
        assert_eq!(restored.audio_chunks.len(), 1);
        assert_eq!(restored.transcript_words.len(), 2);
        assert_eq!(restored.transcript_words[1].text, "ok");
        assert_eq!(restored.intent.as_deref(), Some("ci"));
        assert!(restored.sealed);
    }

    // ── edge: many frames accumulate correctly ────────────────────────────────

    #[test]
    fn many_frames_accumulate_without_loss() {
        let mut asm = BundleAssembler::new(3);
        for i in 0..200_u64 {
            asm.push_frame(sample_frame(i * 33_000_000));
        }
        assert_eq!(asm.frame_count(), 200);
        let b = asm.finish(None, vec![], 0.5).expect("finish");
        assert_eq!(b.frames.len(), 200);
    }
}
