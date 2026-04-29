//! Serialization round-trip tests for `phantom-bundles`.
//!
//! # Purpose
//!
//! These tests lock in the wire format so that:
//!
//! 1. Every [`Bundle`] variant survives a JSON serialize → deserialize cycle
//!    with field-level equality.
//! 2. Known-good fixture files checked into `tests/fixtures/` can still be
//!    decoded by the current schema — forward-compatibility guarantees.
//! 3. Adding a new *optional* field (with `#[serde(default)]`) does not break
//!    existing fixtures.
//!
//! Run with:
//!
//! ```sh
//! cargo test -p phantom-bundles serialization
//! ```

use phantom_bundles::{AudioRef, Bundle, BundleError, FrameRef, SCHEMA_VERSION, TranscriptWord};

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Path helper — resolves a fixture filename relative to this test file's
/// directory at *compile time* so the test works from any working directory.
macro_rules! fixture {
    ($name:expr) => {
        include_str!(concat!("fixtures/", $name))
    };
}

fn frame(t: u64) -> FrameRef {
    FrameRef {
        t_offset_ns: t,
        sha: format!("sha{t:016x}"),
        blob_path: format!("frames/{t}.png"),
        dhash: t ^ 0xDEAD_BEEF,
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
        confidence: 0.93,
    }
}

/// Construct a rich, fully-populated bundle for use across multiple tests.
fn rich_bundle() -> Bundle {
    let mut b = Bundle::new(42);
    b.t_start_ns = 1_000_000_000;
    b.t_wall_unix_ms = 1_700_000_000_000;
    b.add_frame(frame(0));
    b.add_frame(frame(33_000_000));
    b.add_frame(frame(66_000_000));
    b.add_audio(audio(0, 20_000_000));
    b.add_audio(audio(20_000_001, 20_000_000));
    b.add_word(word(0, 500_000_000, "cargo"));
    b.add_word(word(500_000_001, 1_000_000_000, "test"));
    b.add_word(word(1_000_000_001, 1_500_000_000, "ok"));
    b.seal(Some("ci-pass".into()), vec!["green".into(), "rust".into()], 0.9);
    b
}

// ─── 1. JSON round-trips for every "shape" of Bundle ─────────────────────────

#[test]
fn json_round_trip_empty_unsealed_bundle() {
    let original = Bundle::new(1);
    let json = serde_json::to_string(&original).expect("serialize");
    let restored: Bundle = serde_json::from_str(&json).expect("deserialize");

    assert_eq!(restored.id, original.id, "id");
    assert_eq!(restored.source_pane_id, original.source_pane_id, "pane");
    assert_eq!(restored.t_start_ns, 0, "t_start_ns");
    assert_eq!(restored.t_wall_unix_ms, 0, "t_wall_unix_ms");
    assert!(restored.frames.is_empty(), "frames");
    assert!(restored.audio_chunks.is_empty(), "audio");
    assert!(restored.transcript_words.is_empty(), "words");
    assert!(restored.intent.is_none(), "intent");
    assert!(restored.tags.is_empty(), "tags");
    assert!((restored.importance - 0.0).abs() < f32::EPSILON, "importance");
    assert!(!restored.sealed, "sealed");
    assert_eq!(restored.schema_version, SCHEMA_VERSION, "schema_version");
}

#[test]
fn json_round_trip_frames_only_bundle() {
    let mut original = Bundle::new(7);
    original.t_start_ns = 1_000_000_000;
    original.t_wall_unix_ms = 1_700_000_000_000;
    original.add_frame(frame(0));
    original.add_frame(frame(33_000_000));
    original.seal(None, vec!["auto".into()], 0.3);

    let json = serde_json::to_string(&original).expect("serialize");
    let restored: Bundle = serde_json::from_str(&json).expect("deserialize");

    assert_eq!(restored.id, original.id, "id");
    assert_eq!(restored.frames.len(), 2, "frame count");
    assert_eq!(restored.frames[0].t_offset_ns, 0, "frame[0].t");
    assert_eq!(restored.frames[1].t_offset_ns, 33_000_000, "frame[1].t");
    assert_eq!(restored.frames[0].sha, original.frames[0].sha, "sha");
    assert_eq!(restored.frames[0].dhash, original.frames[0].dhash, "dhash");
    assert_eq!(restored.frames[0].width, 1920, "width");
    assert_eq!(restored.frames[0].height, 1080, "height");
    assert!(restored.audio_chunks.is_empty(), "audio empty");
    assert!(restored.sealed, "sealed");
    assert!(restored.intent.is_none(), "intent none");
    assert_eq!(restored.tags, vec!["auto"], "tags");
    assert!((restored.importance - 0.3).abs() < f32::EPSILON, "importance");
}

#[test]
fn json_round_trip_audio_only_bundle() {
    let mut original = Bundle::new(3);
    original.add_audio(audio(0, 5_000_000_000));
    original.seal(Some("microphone-test".into()), vec!["audio".into()], 0.4);

    let json = serde_json::to_string(&original).expect("serialize");
    let restored: Bundle = serde_json::from_str(&json).expect("deserialize");

    assert_eq!(restored.id, original.id);
    assert!(restored.frames.is_empty());
    assert_eq!(restored.audio_chunks.len(), 1);
    assert_eq!(restored.audio_chunks[0].t_offset_ns, 0);
    assert_eq!(restored.audio_chunks[0].duration_ns, 5_000_000_000);
    assert_eq!(restored.audio_chunks[0].sample_rate, 48_000);
    assert_eq!(restored.audio_chunks[0].channels, 2);
    assert!(restored.sealed);
}

#[test]
fn json_round_trip_transcript_only_bundle() {
    let mut original = Bundle::new(9);
    original.add_word(word(0, 300_000_000, "hello"));
    original.add_word(word(350_000_000, 700_000_000, "world"));
    original.seal(Some("speech-demo".into()), vec!["stt".into()], 0.5);

    let json = serde_json::to_string(&original).expect("serialize");
    let restored: Bundle = serde_json::from_str(&json).expect("deserialize");

    assert_eq!(restored.id, original.id);
    assert!(restored.frames.is_empty());
    assert!(restored.audio_chunks.is_empty());
    assert_eq!(restored.transcript_words.len(), 2);
    assert_eq!(restored.transcript_words[0].text, "hello");
    assert_eq!(restored.transcript_words[1].text, "world");
    assert_eq!(restored.transcript_words[0].speaker.as_deref(), Some("user"));
    assert!((restored.transcript_words[0].confidence - 0.93).abs() < f32::EPSILON);
    assert!(restored.sealed);
}

#[test]
fn json_round_trip_all_modalities_bundle() {
    let original = rich_bundle();

    let json = serde_json::to_string(&original).expect("serialize");
    let restored: Bundle = serde_json::from_str(&json).expect("deserialize");

    assert_eq!(restored.id, original.id, "id");
    assert_eq!(restored.t_start_ns, 1_000_000_000, "t_start_ns");
    assert_eq!(restored.t_wall_unix_ms, 1_700_000_000_000, "t_wall_unix_ms");
    assert_eq!(restored.source_pane_id, 42, "source_pane_id");
    assert_eq!(restored.frames.len(), 3, "3 frames");
    assert_eq!(restored.frames[2].t_offset_ns, 66_000_000, "frame[2].t");
    assert_eq!(restored.audio_chunks.len(), 2, "2 audio chunks");
    assert_eq!(restored.transcript_words.len(), 3, "3 words");
    assert_eq!(restored.transcript_words[2].text, "ok", "word[2]");
    assert_eq!(restored.intent.as_deref(), Some("ci-pass"), "intent");
    assert_eq!(restored.tags, vec!["green", "rust"], "tags");
    assert!((restored.importance - 0.9).abs() < f32::EPSILON, "importance");
    assert!(restored.sealed, "sealed");
    assert_eq!(restored.schema_version, 1, "schema_version");
}

// ─── 2. Pretty-JSON round-trips (stable format check) ────────────────────────

#[test]
fn pretty_json_round_trip_is_stable() {
    let original = rich_bundle();

    let pretty = serde_json::to_string_pretty(&original).expect("pretty serialize");
    // Verify the pretty output contains recognisable field names.
    assert!(pretty.contains("\"t_start_ns\""), "missing t_start_ns");
    assert!(pretty.contains("\"schema_version\""), "missing schema_version");
    assert!(pretty.contains("\"importance\""), "missing importance");

    let restored: Bundle = serde_json::from_str(&pretty).expect("deserialize pretty");
    assert_eq!(restored.id, original.id);
    assert_eq!(restored.frames.len(), original.frames.len());
}

// ─── 3. schema_version mismatch is detectable post-deserialize ───────────────

#[test]
fn schema_version_mismatch_is_detectable() {
    // Tamper with the version field to simulate a bundle produced by a newer
    // writer.
    let bundle = Bundle::new(1);
    let mut value: serde_json::Value = serde_json::to_value(&bundle).expect("to_value");
    value["schema_version"] = serde_json::json!(99);

    let tampered: Bundle =
        serde_json::from_value(value).expect("serde allows any u32 schema_version");

    assert_ne!(tampered.schema_version, SCHEMA_VERSION);

    // Callers detect the mismatch by checking the version field.
    let err_opt: Option<BundleError> = if tampered.schema_version != SCHEMA_VERSION {
        Some(BundleError::SchemaVersionMismatch {
            expected: SCHEMA_VERSION,
            found: tampered.schema_version,
        })
    } else {
        None
    };

    assert!(
        matches!(err_opt, Some(BundleError::SchemaVersionMismatch { expected: 1, found: 99 })),
        "mismatch must be detectable"
    );
}

// ─── 4. Fixture files: forward-compat (v1 fixtures load without error) ────────

/// Load each known-good fixture and assert it deserializes without error.  The
/// fixtures are embedded at compile time via `include_str!` so the test is
/// hermetic — no filesystem access at runtime.

#[test]
fn fixture_bundle_empty_unsealed_loads() {
    let json = fixture!("bundle_empty_unsealed.json");
    let bundle: Bundle = serde_json::from_str(json).expect("fixture must deserialize");
    assert_eq!(bundle.schema_version, 1, "schema_version");
    assert!(!bundle.sealed, "not sealed");
    assert!(bundle.frames.is_empty(), "no frames");
    assert!(bundle.audio_chunks.is_empty(), "no audio");
    assert!(bundle.transcript_words.is_empty(), "no words");
}

#[test]
fn fixture_bundle_frames_only_loads() {
    let json = fixture!("bundle_frames_only.json");
    let bundle: Bundle = serde_json::from_str(json).expect("fixture must deserialize");
    assert_eq!(bundle.schema_version, 1);
    assert!(bundle.sealed);
    assert_eq!(bundle.frames.len(), 2);
    assert!(bundle.audio_chunks.is_empty());
    assert!(bundle.transcript_words.is_empty());
    assert_eq!(bundle.frames[0].width, 1920);
    assert_eq!(bundle.frames[1].t_offset_ns, 33_333_333);
}

#[test]
fn fixture_bundle_all_modalities_loads() {
    let json = fixture!("bundle_all_modalities.json");
    let bundle: Bundle = serde_json::from_str(json).expect("fixture must deserialize");
    assert_eq!(bundle.schema_version, 1);
    assert!(bundle.sealed);
    assert_eq!(bundle.frames.len(), 1);
    assert_eq!(bundle.audio_chunks.len(), 1);
    assert_eq!(bundle.transcript_words.len(), 2);
    assert_eq!(bundle.transcript_words[0].text, "cargo");
    assert_eq!(bundle.transcript_words[1].text, "test");
    assert_eq!(bundle.intent.as_deref(), Some("ci-check"));
    assert_eq!(bundle.tags, vec!["rust", "test", "green"]);
    assert!((bundle.importance - 0.85).abs() < 1e-5, "importance {}", bundle.importance);
}

#[test]
fn fixture_bundle_transcript_only_loads() {
    let json = fixture!("bundle_transcript_only.json");
    let bundle: Bundle = serde_json::from_str(json).expect("fixture must deserialize");
    assert_eq!(bundle.schema_version, 1);
    assert!(bundle.sealed);
    assert!(bundle.frames.is_empty());
    assert!(bundle.audio_chunks.is_empty());
    assert_eq!(bundle.transcript_words.len(), 2);
    assert_eq!(bundle.transcript_words[0].text, "hello");
    // speaker: null in fixture → None
    assert!(bundle.transcript_words[0].speaker.is_none());
}

#[test]
fn fixture_bundle_high_importance_loads() {
    let json = fixture!("bundle_high_importance.json");
    let bundle: Bundle = serde_json::from_str(json).expect("fixture must deserialize");
    assert_eq!(bundle.schema_version, 1);
    assert!(bundle.sealed);
    assert!((bundle.importance - 1.0).abs() < 1e-5);
    assert_eq!(bundle.frames.len(), 1);
    assert_eq!(bundle.frames[0].width, 3840);
    assert_eq!(bundle.frames[0].height, 2160);
    assert_eq!(bundle.transcript_words[0].speaker.as_deref(), Some("agent"));
}

// ─── 5. Forward-compat: adding an optional field doesn't break old fixtures ──

/// A hypothetical v2 reader gains a new optional `session_id` field on
/// `Bundle`.  A v1 fixture (which lacks that field) must still deserialize
/// successfully, with `session_id` defaulting to `None`.
#[test]
fn forward_compat_extra_optional_field_in_reader_accepts_v1_fixture() {
    /// A v2 reader struct that adds an optional `session_id` field.
    #[derive(Debug, serde::Deserialize)]
    struct BundleV2 {
        #[allow(dead_code)]
        id: uuid::Uuid,
        #[allow(dead_code)]
        source_pane_id: u64,
        schema_version: u32,
        sealed: bool,
        // New field in v2 — must default to None when absent.
        #[serde(default)]
        session_id: Option<String>,
    }

    let v1_fixture = fixture!("bundle_all_modalities.json");
    let decoded: BundleV2 =
        serde_json::from_str(v1_fixture).expect("v2 reader must accept v1 fixture");
    assert_eq!(decoded.schema_version, 1, "schema_version");
    assert!(decoded.sealed, "sealed");
    assert!(
        decoded.session_id.is_none(),
        "new optional field defaults to None for v1 payload"
    );
}

/// A v2 payload that contains an unknown field must still deserialize for a v1
/// reader via `#[serde(deny_unknown_fields)]` being *absent* (the default).
/// serde's default behavior is to ignore unknown fields, so old readers are
/// not broken by new writers that add extra fields.
#[test]
fn forward_compat_v2_payload_with_extra_field_is_ignored_by_v1_reader() {
    // Inject a synthetic extra field into a known fixture.
    let v1_json = fixture!("bundle_frames_only.json");
    let mut value: serde_json::Value =
        serde_json::from_str(v1_json).expect("parse fixture as Value");
    value["extra_field_from_v2"] = serde_json::json!("some-new-data");

    let v2_json = serde_json::to_string(&value).expect("re-serialize");

    // The v1 Bundle struct must still deserialize — it silently ignores the
    // unknown field.
    let decoded: Bundle =
        serde_json::from_str(&v2_json).expect("v1 reader ignores unknown fields");
    assert_eq!(decoded.schema_version, 1);
    assert_eq!(decoded.frames.len(), 2);
}

// ─── 6. Component types round-trip independently ─────────────────────────────

#[test]
fn frame_ref_json_round_trip() {
    let original = frame(12_345_678);
    let json = serde_json::to_string(&original).expect("serialize");
    let restored: FrameRef = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(restored.t_offset_ns, original.t_offset_ns);
    assert_eq!(restored.sha, original.sha);
    assert_eq!(restored.blob_path, original.blob_path);
    assert_eq!(restored.dhash, original.dhash);
    assert_eq!(restored.width, original.width);
    assert_eq!(restored.height, original.height);
}

#[test]
fn audio_ref_json_round_trip() {
    let original = audio(9_999_999, 3_000_000_000);
    let json = serde_json::to_string(&original).expect("serialize");
    let restored: AudioRef = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(restored.t_offset_ns, original.t_offset_ns);
    assert_eq!(restored.duration_ns, original.duration_ns);
    assert_eq!(restored.blob_path, original.blob_path);
    assert_eq!(restored.sample_rate, 48_000);
    assert_eq!(restored.channels, 2);
}

#[test]
fn transcript_word_json_round_trip_with_speaker() {
    let original = word(100, 200, "phantom");
    let json = serde_json::to_string(&original).expect("serialize");
    let restored: TranscriptWord = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(restored.t_offset_ns, 100);
    assert_eq!(restored.t_end_ns, 200);
    assert_eq!(restored.text, "phantom");
    assert_eq!(restored.speaker.as_deref(), Some("user"));
    assert!((restored.confidence - 0.93).abs() < f32::EPSILON);
}

#[test]
fn transcript_word_json_round_trip_without_speaker() {
    let original = TranscriptWord {
        t_offset_ns: 0,
        t_end_ns: 100,
        text: "nospeaker".into(),
        speaker: None,
        confidence: 0.5,
    };
    let json = serde_json::to_string(&original).expect("serialize");
    let restored: TranscriptWord = serde_json::from_str(&json).expect("deserialize");
    assert!(restored.speaker.is_none());
    assert_eq!(restored.text, "nospeaker");
}

// ─── 7. Importance clamping survives round-trip ───────────────────────────────

#[test]
fn importance_clamped_above_one_survives_round_trip() {
    let mut b = Bundle::new(1);
    b.add_frame(frame(0));
    b.seal(None, vec![], 9999.0); // seal clamps to 1.0

    let json = serde_json::to_string(&b).expect("serialize");
    let restored: Bundle = serde_json::from_str(&json).expect("deserialize");
    assert!((restored.importance - 1.0).abs() < f32::EPSILON, "got {}", restored.importance);
}

#[test]
fn importance_clamped_below_zero_survives_round_trip() {
    let mut b = Bundle::new(1);
    b.add_frame(frame(0));
    b.seal(None, vec![], -5.0); // seal clamps to 0.0

    let json = serde_json::to_string(&b).expect("serialize");
    let restored: Bundle = serde_json::from_str(&json).expect("deserialize");
    assert!((restored.importance - 0.0).abs() < f32::EPSILON, "got {}", restored.importance);
}

// ─── 8. UUID stability: id survives serialization ────────────────────────────

#[test]
fn bundle_id_survives_json_round_trip() {
    let original = Bundle::new(1);
    let id = original.id;
    let json = serde_json::to_string(&original).expect("serialize");
    // Confirm the UUID appears literally in the JSON (not as bytes).
    assert!(json.contains(&id.to_string()), "UUID must be in JSON");
    let restored: Bundle = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(restored.id, id, "UUID must survive round-trip");
}

// ─── 9. Fixture files: re-serialize and re-deserialize (double round-trip) ───

/// Each fixture survives: fixture_str → Bundle → JSON string → Bundle.
/// This confirms the fixture content is normalized (no ordering surprises).
#[test]
fn fixture_double_round_trip_all_modalities() {
    let json1 = fixture!("bundle_all_modalities.json");
    let b1: Bundle = serde_json::from_str(json1).expect("first deserialize");
    let json2 = serde_json::to_string(&b1).expect("re-serialize");
    let b2: Bundle = serde_json::from_str(&json2).expect("second deserialize");

    assert_eq!(b1.id, b2.id);
    assert_eq!(b1.frames.len(), b2.frames.len());
    assert_eq!(b1.audio_chunks.len(), b2.audio_chunks.len());
    assert_eq!(b1.transcript_words.len(), b2.transcript_words.len());
    assert_eq!(b1.intent, b2.intent);
    assert_eq!(b1.tags, b2.tags);
    assert!((b1.importance - b2.importance).abs() < 1e-5);
    assert_eq!(b1.sealed, b2.sealed);
    assert_eq!(b1.schema_version, b2.schema_version);
}
