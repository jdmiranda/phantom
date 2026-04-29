//! Schema types for phantom capture bundles.
//!
//! A bundle groups a temporal slice of pane activity — frames, audio, and
//! transcript — under a stable id so storage and retrieval layers can be
//! implemented independently.  The crate provides:
//!
//! * **Schema types** ([`Bundle`], [`FrameRef`], [`AudioRef`], [`TranscriptWord`])
//!   — all derive `serde::{Serialize, Deserialize}` so they can be persisted as
//!   JSON or any serde-compatible format.
//! * **[`assembler::BundleAssembler`]** — staged builder that collects raw
//!   capture events and seals them into a [`Bundle`] at a command boundary.
//! * **[`events`]** — canonical event types for the capture pipeline, with both
//!   JSON and `bincode` serialization.

pub mod assembler;
pub mod events;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Stable identifier for a bundle.
pub type BundleId = uuid::Uuid;

/// Identifier for the originating pane.
pub type PaneId = u64;

/// Current schema version. Persisted bundles store their version so future
/// readers can detect mismatches.
pub const SCHEMA_VERSION: u32 = 1;

/// Errors produced when validating a bundle.
#[derive(Debug, Error)]
pub enum BundleError {
    /// The bundle was produced by an incompatible schema version.
    #[error("schema version mismatch: expected {expected}, found {found}")]
    SchemaVersionMismatch {
        /// The version this build understands.
        expected: u32,
        /// The version stored in the bundle.
        found: u32,
    },
}

/// A reference to a captured frame blob.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrameRef {
    /// Offset from the bundle start, in nanoseconds.
    pub t_offset_ns: u64,
    /// Content hash of the blob (e.g. sha256 hex).
    pub sha: String,
    /// Path to the blob, relative to the bundle directory.
    pub blob_path: String,
    /// Perceptual hash used for near-duplicate detection.
    pub dhash: u64,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
}

/// A reference to a captured audio chunk blob.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioRef {
    /// Offset from the bundle start, in nanoseconds.
    pub t_offset_ns: u64,
    /// Length of the audio chunk, in nanoseconds.
    pub duration_ns: u64,
    /// Path to the blob, relative to the bundle directory.
    pub blob_path: String,
    /// Sample rate in hertz.
    pub sample_rate: u32,
    /// Channel count.
    pub channels: u16,
}

/// A single transcribed word with timing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptWord {
    /// Start offset from the bundle start, in nanoseconds.
    pub t_offset_ns: u64,
    /// End offset from the bundle start, in nanoseconds.
    pub t_end_ns: u64,
    /// The transcribed text.
    pub text: String,
    /// Speaker label, if diarized.
    pub speaker: Option<String>,
    /// Confidence score in [0.0, 1.0].
    pub confidence: f32,
}

/// A capture bundle: temporal slice of pane activity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bundle {
    /// Stable bundle id.
    pub id: BundleId,
    /// Monotonic clock at capture start, in nanoseconds.
    pub t_start_ns: u64,
    /// Wall-clock at capture start, in unix milliseconds.
    pub t_wall_unix_ms: i64,
    /// The pane that produced this bundle.
    pub source_pane_id: PaneId,
    /// Frame references in insertion order.
    pub frames: Vec<FrameRef>,
    /// Audio chunk references in insertion order.
    pub audio_chunks: Vec<AudioRef>,
    /// Transcript words in insertion order.
    pub transcript_words: Vec<TranscriptWord>,
    /// Optional intent tag assigned at seal time.
    pub intent: Option<String>,
    /// Free-form tags assigned at seal time.
    pub tags: Vec<String>,
    /// Importance score in [0.0, 1.0]; clamped at seal time.
    pub importance: f32,
    /// Whether the bundle has been finalized.
    pub sealed: bool,
    /// Schema version of this bundle.
    pub schema_version: u32,
}

impl Bundle {
    /// Construct a fresh, unsealed bundle for the given pane.
    ///
    /// `t_start_ns` and `t_wall_unix_ms` default to zero — callers populate
    /// them at capture time. The id is a freshly generated v4 UUID.
    #[must_use]
    pub fn new(source_pane_id: PaneId) -> Self {
        Self {
            id: uuid::Uuid::new_v4(),
            t_start_ns: 0,
            t_wall_unix_ms: 0,
            source_pane_id,
            frames: Vec::new(),
            audio_chunks: Vec::new(),
            transcript_words: Vec::new(),
            intent: None,
            tags: Vec::new(),
            importance: 0.0,
            sealed: false,
            schema_version: SCHEMA_VERSION,
        }
    }

    /// Append a frame reference.
    pub fn add_frame(&mut self, frame: FrameRef) {
        self.frames.push(frame);
    }

    /// Append an audio chunk reference.
    pub fn add_audio(&mut self, chunk: AudioRef) {
        self.audio_chunks.push(chunk);
    }

    /// Append a transcript word.
    pub fn add_word(&mut self, word: TranscriptWord) {
        self.transcript_words.push(word);
    }

    /// Finalize the bundle. Importance is clamped to `[0.0, 1.0]`.
    pub fn seal(&mut self, intent: Option<String>, tags: Vec<String>, importance: f32) {
        self.intent = intent;
        self.tags = tags;
        self.importance = importance.clamp(0.0, 1.0);
        self.sealed = true;
    }

    /// Duration covered by the bundle, derived from the latest frame offset.
    /// Returns `0` if no frames have been recorded.
    #[must_use]
    pub fn duration_ns(&self) -> u64 {
        self.frames.iter().map(|f| f.t_offset_ns).max().unwrap_or(0)
    }

    /// Find the transcript word whose `[t_offset_ns, t_end_ns]` range
    /// contains `t_offset_ns`. Both endpoints are treated as inclusive so
    /// boundary samples remain hittable. When ranges overlap, the first
    /// match in insertion order is returned.
    #[must_use]
    pub fn word_at(&self, t_offset_ns: u64) -> Option<&TranscriptWord> {
        self.transcript_words
            .iter()
            .find(|w| t_offset_ns >= w.t_offset_ns && t_offset_ns <= w.t_end_ns)
    }

    /// Find the frame whose `t_offset_ns <= t_offset_ns`, choosing the
    /// latest such frame. This gives "the frame currently on screen at
    /// this time". Returns `None` if no frame's offset is `<=` the query
    /// (including the empty case).
    #[must_use]
    pub fn frame_at(&self, t_offset_ns: u64) -> Option<&FrameRef> {
        self.frames
            .iter()
            .filter(|f| f.t_offset_ns <= t_offset_ns)
            .max_by_key(|f| f.t_offset_ns)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(t: u64) -> FrameRef {
        FrameRef {
            t_offset_ns: t,
            sha: format!("sha-{t}"),
            blob_path: format!("frames/{t}.png"),
            dhash: t,
            width: 1920,
            height: 1080,
        }
    }

    fn audio(t: u64, dur: u64) -> AudioRef {
        AudioRef {
            t_offset_ns: t,
            duration_ns: dur,
            blob_path: format!("audio/{t}.opus"),
            sample_rate: 48_000,
            channels: 2,
        }
    }

    fn word(start: u64, end: u64, text: &str) -> TranscriptWord {
        TranscriptWord {
            t_offset_ns: start,
            t_end_ns: end,
            text: text.into(),
            speaker: Some("user".into()),
            confidence: 0.92,
        }
    }

    #[test]
    fn new_bundle_has_fresh_uuid_unsealed_v1() {
        let a = Bundle::new(7);
        let b = Bundle::new(7);
        assert_ne!(a.id, b.id, "each new bundle should get a fresh UUID");
        assert!(!a.sealed);
        assert_eq!(a.schema_version, 1);
        assert_eq!(a.source_pane_id, 7);
        assert!(a.frames.is_empty());
        assert!(a.audio_chunks.is_empty());
        assert!(a.transcript_words.is_empty());
        assert!(a.intent.is_none());
        assert!(a.tags.is_empty());
        assert_eq!(a.importance, 0.0);
    }

    #[test]
    fn add_methods_preserve_insertion_order() {
        let mut b = Bundle::new(1);
        b.add_frame(frame(30));
        b.add_frame(frame(10));
        b.add_frame(frame(20));
        b.add_audio(audio(5, 100));
        b.add_audio(audio(1, 50));
        b.add_word(word(0, 10, "hi"));
        b.add_word(word(11, 20, "there"));

        let frame_offsets: Vec<u64> = b.frames.iter().map(|f| f.t_offset_ns).collect();
        assert_eq!(frame_offsets, vec![30, 10, 20]);

        let audio_offsets: Vec<u64> = b.audio_chunks.iter().map(|a| a.t_offset_ns).collect();
        assert_eq!(audio_offsets, vec![5, 1]);

        let words: Vec<&str> = b.transcript_words.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(words, vec!["hi", "there"]);
    }

    #[test]
    fn seal_flips_state_and_stores_metadata() {
        let mut b = Bundle::new(3);
        b.seal(
            Some("debug-build".into()),
            vec!["rust".into(), "test".into()],
            0.7,
        );
        assert!(b.sealed);
        assert_eq!(b.intent.as_deref(), Some("debug-build"));
        assert_eq!(b.tags, vec!["rust", "test"]);
        assert!((b.importance - 0.7).abs() < f32::EPSILON);
    }

    #[test]
    fn seal_clamps_importance_above_one() {
        let mut b = Bundle::new(1);
        b.seal(None, vec![], 5.5);
        assert!((b.importance - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn seal_clamps_importance_below_zero() {
        let mut b = Bundle::new(1);
        b.seal(None, vec![], -0.5);
        assert!((b.importance - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn duration_ns_is_max_frame_offset() {
        let mut b = Bundle::new(1);
        assert_eq!(b.duration_ns(), 0, "empty bundle has zero duration");
        b.add_frame(frame(100));
        b.add_frame(frame(500));
        b.add_frame(frame(250));
        assert_eq!(b.duration_ns(), 500);
    }

    #[test]
    fn word_at_returns_word_containing_query() {
        let mut b = Bundle::new(1);
        b.add_word(word(0, 100, "hello"));
        b.add_word(word(150, 250, "world"));

        assert_eq!(b.word_at(50).unwrap().text, "hello");
        assert_eq!(b.word_at(0).unwrap().text, "hello", "start boundary inclusive");
        assert_eq!(b.word_at(100).unwrap().text, "hello", "end boundary inclusive");
        assert_eq!(b.word_at(200).unwrap().text, "world");
        assert!(b.word_at(125).is_none(), "gap between words returns None");
        assert!(b.word_at(1_000).is_none(), "past end returns None");
    }

    #[test]
    fn word_at_empty_returns_none() {
        let b = Bundle::new(1);
        assert!(b.word_at(0).is_none());
    }

    #[test]
    fn frame_at_returns_latest_frame_at_or_before_query() {
        let mut b = Bundle::new(1);
        // Insert out of order to confirm it's not insertion-order based.
        b.add_frame(frame(300));
        b.add_frame(frame(100));
        b.add_frame(frame(200));

        assert!(b.frame_at(50).is_none(), "before first frame returns None");
        assert_eq!(b.frame_at(100).unwrap().t_offset_ns, 100, "exact match");
        assert_eq!(b.frame_at(150).unwrap().t_offset_ns, 100);
        assert_eq!(b.frame_at(250).unwrap().t_offset_ns, 200);
        assert_eq!(b.frame_at(300).unwrap().t_offset_ns, 300, "exact at last");
        assert_eq!(b.frame_at(99_999).unwrap().t_offset_ns, 300, "past last clamps to last");
    }

    #[test]
    fn frame_at_empty_returns_none() {
        let b = Bundle::new(1);
        assert!(b.frame_at(0).is_none());
        assert!(b.frame_at(1_000).is_none());
    }

    #[test]
    fn populated_bundle_round_trips_through_json() {
        let mut original = Bundle::new(42);
        original.t_start_ns = 1_000_000;
        original.t_wall_unix_ms = 1_700_000_000_000;
        original.add_frame(frame(0));
        original.add_frame(frame(33_000_000));
        original.add_audio(audio(0, 20_000_000));
        original.add_word(word(0, 1_000_000, "build"));
        original.add_word(word(1_000_001, 2_000_000, "passing"));
        original.seal(Some("ci-success".into()), vec!["green".into()], 0.85);

        let json = serde_json::to_string(&original).expect("serialize");
        let restored: Bundle = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(restored.id, original.id);
        assert_eq!(restored.t_start_ns, original.t_start_ns);
        assert_eq!(restored.t_wall_unix_ms, original.t_wall_unix_ms);
        assert_eq!(restored.source_pane_id, original.source_pane_id);
        assert_eq!(restored.frames.len(), original.frames.len());
        assert_eq!(restored.frames[1].t_offset_ns, 33_000_000);
        assert_eq!(restored.audio_chunks.len(), 1);
        assert_eq!(restored.transcript_words.len(), 2);
        assert_eq!(restored.transcript_words[1].text, "passing");
        assert_eq!(restored.intent.as_deref(), Some("ci-success"));
        assert_eq!(restored.tags, vec!["green"]);
        assert!((restored.importance - 0.85).abs() < f32::EPSILON);
        assert!(restored.sealed);
        assert_eq!(restored.schema_version, 1);
    }

    #[test]
    fn schema_version_mismatch_is_detectable_after_deserialize() {
        let bundle = Bundle::new(1);
        let mut value: serde_json::Value = serde_json::to_value(&bundle).expect("to value");
        value["schema_version"] = serde_json::json!(99);
        let tampered_json = serde_json::to_string(&value).expect("re-serialize");

        let restored: Bundle = serde_json::from_str(&tampered_json).expect("deserialize");
        assert_ne!(
            restored.schema_version, SCHEMA_VERSION,
            "tampered version should round-trip and be detectable"
        );

        let err = match restored.schema_version == SCHEMA_VERSION {
            true => None,
            false => Some(BundleError::SchemaVersionMismatch {
                expected: SCHEMA_VERSION,
                found: restored.schema_version,
            }),
        };
        assert!(matches!(
            err,
            Some(BundleError::SchemaVersionMismatch { expected: 1, found: 99 })
        ));
    }
}
