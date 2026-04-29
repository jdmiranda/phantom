//! [`CaptureSession`] — accumulates frames and transcript segments during a
//! live capture window, then seals them into a [`CaptureBundle`] via
//! [`BundleAssembler`].
//!
//! # Lifecycle
//!
//! ```text
//! CaptureSession::new(session_id)
//!     │
//!     ├── add_frame(frame)          // zero or more times
//!     ├── add_transcript(segment)   // zero or more times
//!     │
//!     └── finalize(self) -> Result<CaptureBundle, BundleError>
//! ```
//!
//! `finalize` returns [`BundleError::NoFrames`] when no frames have been
//! added.  Consecutive frames that share the same perceptual hash are silently
//! deduplicated; only the first of a run of identical hashes is retained.

use crate::{
    BundleError, FrameRef, TranscriptWord,
    assembler::{AssemblerError, BundleAssembler},
};

/// Re-exported for callers that only depend on `phantom_bundles`.
pub use crate::Bundle as CaptureBundle;

// ─── SessionId ───────────────────────────────────────────────────────────────

/// Stable identifier for a capture session.
pub type SessionId = uuid::Uuid;

// ─── CaptureFrame ────────────────────────────────────────────────────────────

/// A raw captured frame: pixel data, wall-clock timestamp, and perceptual hash.
///
/// All fields are private.  Use the accessor methods to read them.
#[derive(Debug, Clone)]
pub struct CaptureFrame {
    /// Wall-clock time when the frame was captured, in Unix milliseconds.
    timestamp_ms: u64,
    /// Perceptual hash (dHash) used for near-duplicate detection.
    phash: u64,
    /// Raw pixel bytes (e.g. PNG-encoded).
    pixels: Vec<u8>,
}

impl CaptureFrame {
    /// Construct a new `CaptureFrame`.
    ///
    /// - `timestamp_ms` — wall-clock capture time in Unix milliseconds.
    /// - `phash`        — perceptual hash for deduplication.
    /// - `pixels`       — raw pixel bytes (e.g. PNG-encoded image data).
    #[must_use]
    pub fn new(timestamp_ms: u64, phash: u64, pixels: Vec<u8>) -> Self {
        Self {
            timestamp_ms,
            phash,
            pixels,
        }
    }

    /// Wall-clock capture time in Unix milliseconds.
    #[must_use]
    pub fn timestamp_ms(&self) -> u64 {
        self.timestamp_ms
    }

    /// Perceptual hash used for near-duplicate detection.
    #[must_use]
    pub fn phash(&self) -> u64 {
        self.phash
    }

    /// Raw pixel bytes.
    #[must_use]
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }
}

// ─── TranscriptSegment ───────────────────────────────────────────────────────

/// A recognised speech segment with timing information.
///
/// All fields are private.  Use the accessor methods to read them.
#[derive(Debug, Clone)]
pub struct TranscriptSegment {
    /// Wall-clock time of the start of the segment, in Unix milliseconds.
    timestamp_ms: u64,
    /// Recognised text for this segment.
    text: String,
    /// Recognition confidence in `[0.0, 1.0]`.
    confidence: f32,
}

impl TranscriptSegment {
    /// Construct a new `TranscriptSegment`.
    ///
    /// - `timestamp_ms` — wall-clock start time in Unix milliseconds.
    /// - `text`         — recognised text.
    /// - `confidence`   — recognition confidence in `[0.0, 1.0]`.
    #[must_use]
    pub fn new(timestamp_ms: u64, text: impl Into<String>, confidence: f32) -> Self {
        Self {
            timestamp_ms,
            text: text.into(),
            confidence,
        }
    }

    /// Wall-clock start time in Unix milliseconds.
    #[must_use]
    pub fn timestamp_ms(&self) -> u64 {
        self.timestamp_ms
    }

    /// Recognised text for this segment.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Recognition confidence.
    #[must_use]
    pub fn confidence(&self) -> f32 {
        self.confidence
    }
}

// ─── CaptureSession ──────────────────────────────────────────────────────────

/// Accumulates frames and transcript segments during a live capture window.
///
/// Create one session per logical capture interval (e.g. per command boundary),
/// push events as they arrive, then call [`finalize`](CaptureSession::finalize)
/// to obtain a sealed, ready-to-persist [`CaptureBundle`].
///
/// # Deduplication
///
/// Consecutive frames whose perceptual hash matches the immediately preceding
/// accepted frame are silently dropped.  Non-consecutive duplicate hashes are
/// retained because a frame may reappear after content changes in between.
pub struct CaptureSession {
    /// Stable session id; also used as the pane id in the assembled bundle.
    session_id: SessionId,
    /// Accumulated frames in push order (after deduplication).
    frames: Vec<CaptureFrame>,
    /// Perceptual hash of the last accepted frame, for run-length dedup.
    last_phash: Option<u64>,
    /// Accumulated transcript segments in push order.
    transcripts: Vec<TranscriptSegment>,
}

impl CaptureSession {
    /// Open a new capture session identified by `session_id`.
    ///
    /// The session starts empty: no frames and no transcript segments.
    #[must_use]
    pub fn new(session_id: SessionId) -> Self {
        Self {
            session_id,
            frames: Vec::new(),
            last_phash: None,
            transcripts: Vec::new(),
        }
    }

    /// The [`SessionId`] this session was opened with.
    #[must_use]
    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    /// The number of frames accepted so far (after deduplication).
    #[must_use]
    pub fn frame_count(&self) -> usize {
        self.frames.len()
    }

    /// The number of transcript segments accumulated so far.
    #[must_use]
    pub fn transcript_count(&self) -> usize {
        self.transcripts.len()
    }

    /// Append a captured frame.
    ///
    /// If the frame's perceptual hash is identical to the immediately preceding
    /// accepted frame, the frame is silently dropped (run-length deduplication).
    pub fn add_frame(&mut self, frame: CaptureFrame) {
        if self.last_phash == Some(frame.phash) {
            return; // consecutive duplicate — discard
        }
        self.last_phash = Some(frame.phash);
        self.frames.push(frame);
    }

    /// Append a transcript segment.
    pub fn add_transcript(&mut self, segment: TranscriptSegment) {
        self.transcripts.push(segment);
    }

    /// Consume the session, assemble, seal, and return the [`CaptureBundle`].
    ///
    /// Frames are fed into a [`BundleAssembler`] in push order; transcript
    /// segments are converted to [`TranscriptWord`] values and also pushed.
    /// The assembler's [`finish`](crate::assembler::BundleAssembler::finish)
    /// method produces the sealed bundle.
    ///
    /// # Errors
    ///
    /// Returns [`BundleError::NoFrames`] if no frames were added.  An empty
    /// bundle with only transcripts is not valid because there is no visual
    /// anchor for storage or retrieval.
    pub fn finalize(self) -> Result<CaptureBundle, BundleError> {
        if self.frames.is_empty() {
            return Err(BundleError::NoFrames);
        }

        // Use the lower 64 bits of the session UUID as a stable pane id so the
        // bundle's source_pane_id is deterministic for a given session.
        let pane_id: u64 = self.session_id.as_u64_pair().0;
        let mut assembler = BundleAssembler::new(pane_id);

        for frame in self.frames {
            // Convert CaptureFrame → FrameRef.  The pixel bytes are not stored
            // in the FrameRef schema (they belong to the blob layer); we embed a
            // minimal sha stub derived from the phash so round-trip tests remain
            // deterministic without a real hashing dependency.
            let frame_ref = FrameRef {
                t_offset_ns: frame.timestamp_ms.saturating_mul(1_000_000),
                sha: format!("{:016x}", frame.phash),
                blob_path: format!("frames/{:016x}.bin", frame.phash),
                dhash: frame.phash,
                width: 0,
                height: 0,
            };
            assembler.push_frame(frame_ref);
        }

        for segment in self.transcripts {
            let duration_ns: u64 = 1_000_000; // 1 ms minimum duration
            let start_ns = segment.timestamp_ms.saturating_mul(1_000_000);
            let end_ns = start_ns.saturating_add(duration_ns);
            let word = TranscriptWord {
                t_offset_ns: start_ns,
                t_end_ns: end_ns,
                text: segment.text,
                speaker: None,
                confidence: segment.confidence,
            };
            assembler.push_word(word);
        }

        assembler
            .finish(None, Vec::new(), 0.5)
            .map_err(|e| match e {
                AssemblerError::EmptyBundle => BundleError::NoFrames,
                AssemblerError::ClockError(msg) => BundleError::ClockError(msg),
            })
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BundleError;

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn frame(timestamp_ms: u64, phash: u64) -> CaptureFrame {
        CaptureFrame::new(timestamp_ms, phash, vec![0xDE, 0xAD, 0xBE, 0xEF])
    }

    fn segment(timestamp_ms: u64, text: &str) -> TranscriptSegment {
        TranscriptSegment::new(timestamp_ms, text, 0.95)
    }

    fn fresh_id() -> SessionId {
        uuid::Uuid::new_v4()
    }

    // ── Test 1: empty session errors ─────────────────────────────────────────

    #[test]
    fn finalize_on_empty_session_returns_no_frames_error() {
        let session = CaptureSession::new(fresh_id());
        let err = session.finalize().expect_err("empty session must fail");
        assert!(
            matches!(err, BundleError::NoFrames),
            "expected NoFrames, got {err:?}"
        );
    }

    #[test]
    fn finalize_transcript_only_session_returns_no_frames_error() {
        let mut session = CaptureSession::new(fresh_id());
        session.add_transcript(segment(1_000, "hello"));
        session.add_transcript(segment(2_000, "world"));
        let err = session.finalize().expect_err("transcript-only session must fail");
        assert!(matches!(err, BundleError::NoFrames));
    }

    // ── Test 2: single frame ─────────────────────────────────────────────────

    #[test]
    fn finalize_single_frame_produces_sealed_bundle() {
        let mut session = CaptureSession::new(fresh_id());
        session.add_frame(frame(100, 0xABCD_1234));
        let bundle = session.finalize().expect("single frame must succeed");
        assert!(bundle.sealed, "bundle must be sealed");
        assert_eq!(bundle.frames.len(), 1, "exactly one frame");
        assert!(bundle.transcript_words.is_empty(), "no words");
        // dhash must survive the round-trip through FrameRef
        assert_eq!(bundle.frames[0].dhash, 0xABCD_1234);
        // t_offset_ns should be timestamp_ms * 1_000_000
        assert_eq!(bundle.frames[0].t_offset_ns, 100 * 1_000_000);
    }

    // ── Test 3: multi-frame ordering ─────────────────────────────────────────

    #[test]
    fn finalize_multi_frame_preserves_insertion_order() {
        let mut session = CaptureSession::new(fresh_id());
        // Push frames with distinct phashes to avoid dedup
        session.add_frame(frame(10, 0x01));
        session.add_frame(frame(20, 0x02));
        session.add_frame(frame(30, 0x03));
        session.add_frame(frame(40, 0x04));

        let bundle = session.finalize().expect("multi-frame");
        assert_eq!(bundle.frames.len(), 4);

        // Verify timestamps appear in insertion order (10, 20, 30, 40 ms → ns)
        let offsets: Vec<u64> = bundle.frames.iter().map(|f| f.t_offset_ns).collect();
        assert_eq!(
            offsets,
            vec![10_000_000, 20_000_000, 30_000_000, 40_000_000],
            "frames must be in push order"
        );
    }

    // ── Test 4: duplicate frame dedup (same phash) ────────────────────────────

    #[test]
    fn consecutive_duplicate_phash_frames_are_deduplicated() {
        let mut session = CaptureSession::new(fresh_id());
        session.add_frame(frame(10, 0xFF)); // accepted
        session.add_frame(frame(20, 0xFF)); // duplicate → dropped
        session.add_frame(frame(30, 0xFF)); // duplicate → dropped
        session.add_frame(frame(40, 0xAA)); // different hash → accepted
        session.add_frame(frame(50, 0xFF)); // phash reappears but not consecutive → accepted
        session.add_frame(frame(60, 0xFF)); // duplicate of previous → dropped

        assert_eq!(session.frame_count(), 3, "only 3 unique-run frames accepted");

        let bundle = session.finalize().expect("deduped bundle");
        assert_eq!(bundle.frames.len(), 3);

        let hashes: Vec<u64> = bundle.frames.iter().map(|f| f.dhash).collect();
        assert_eq!(hashes, vec![0xFF, 0xAA, 0xFF], "retained hashes in order");
    }

    #[test]
    fn non_consecutive_identical_phash_is_retained() {
        let mut session = CaptureSession::new(fresh_id());
        session.add_frame(frame(0, 0x11));
        session.add_frame(frame(1, 0x22)); // different — breaks the run
        session.add_frame(frame(2, 0x11)); // same as first but not consecutive → keep

        let bundle = session.finalize().expect("bundle");
        assert_eq!(bundle.frames.len(), 3, "all three retained");
    }

    // ── Test 5: transcript included in output ────────────────────────────────

    #[test]
    fn transcript_segments_appear_in_bundle_words() {
        let mut session = CaptureSession::new(fresh_id());
        session.add_frame(frame(0, 0x01));
        session.add_transcript(segment(10, "cargo"));
        session.add_transcript(segment(20, "test"));
        session.add_transcript(segment(30, "ok"));

        let bundle = session.finalize().expect("bundle with transcript");
        assert_eq!(bundle.frames.len(), 1);
        assert_eq!(bundle.transcript_words.len(), 3, "three transcript words");

        let texts: Vec<&str> = bundle.transcript_words.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(texts, vec!["cargo", "test", "ok"], "words in push order");

        // Verify t_offset_ns mapping: timestamp_ms * 1_000_000
        assert_eq!(bundle.transcript_words[0].t_offset_ns, 10 * 1_000_000);
        assert_eq!(bundle.transcript_words[1].t_offset_ns, 20 * 1_000_000);
    }

    // ── Test 6: round-trip serialize/deserialize ──────────────────────────────

    #[test]
    fn finalized_bundle_round_trips_through_json() {
        let id = fresh_id();
        let mut session = CaptureSession::new(id);
        session.add_frame(frame(0, 0xDEAD));
        session.add_frame(frame(16, 0xBEEF));
        session.add_transcript(segment(8, "phantom"));

        let original = session.finalize().expect("finalize");
        assert!(original.sealed);
        assert_eq!(original.frames.len(), 2);
        assert_eq!(original.transcript_words.len(), 1);

        let json = serde_json::to_string(&original).expect("serialize");
        let restored: CaptureBundle = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(restored.id, original.id, "id survives round-trip");
        assert_eq!(restored.frames.len(), 2, "frame count");
        assert_eq!(restored.frames[0].dhash, 0xDEAD, "phash survives as dhash");
        assert_eq!(restored.frames[1].dhash, 0xBEEF);
        assert_eq!(restored.transcript_words.len(), 1);
        assert_eq!(restored.transcript_words[0].text, "phantom");
        assert!(restored.sealed, "sealed flag survives round-trip");
        assert_eq!(restored.schema_version, crate::SCHEMA_VERSION);
    }

    // ── Extra: accessors return correct values ────────────────────────────────

    #[test]
    fn capture_frame_accessors_return_correct_values() {
        let pixels = vec![1u8, 2, 3, 4];
        let f = CaptureFrame::new(999, 0xCAFE, pixels.clone());
        assert_eq!(f.timestamp_ms(), 999);
        assert_eq!(f.phash(), 0xCAFE);
        assert_eq!(f.pixels(), pixels.as_slice());
    }

    #[test]
    fn transcript_segment_accessors_return_correct_values() {
        let s = TranscriptSegment::new(500, "hello world", 0.88);
        assert_eq!(s.timestamp_ms(), 500);
        assert_eq!(s.text(), "hello world");
        assert!((s.confidence() - 0.88).abs() < f32::EPSILON);
    }

    #[test]
    fn session_id_accessor_matches_construction_id() {
        let id = fresh_id();
        let session = CaptureSession::new(id);
        assert_eq!(session.session_id(), id);
    }
}
