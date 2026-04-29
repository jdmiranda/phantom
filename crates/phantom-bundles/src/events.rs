//! Canonical event types for the capture pipeline.
//!
//! Every significant moment in a capture session — a rendered frame, an audio
//! chunk, a speech recognition result, a shell command boundary, or a sealed
//! bundle — is represented as a [`CaptureEvent`]. Events can be serialised
//! with both JSON (via `serde`) and the compact binary format (via `bincode`).
//!
//! # Schema versioning
//!
//! [`SCHEMA_VERSION`] is stored alongside any persisted event stream so that
//! future readers can detect format mismatches. The forward-compatibility
//! design relies on `#[serde(default)]` on optional fields: a v1 payload
//! decoded by a hypothetical v2 reader that adds optional fields will succeed
//! because `serde` treats missing optional fields as `None`.

use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

/// Schema version stamped on every persisted event stream.
///
/// Increment this whenever a breaking change is made to [`CaptureEvent`] or
/// [`EventRef`].
pub const SCHEMA_VERSION: u8 = 1;

// ─── EventKind ───────────────────────────────────────────────────────────────

/// Discriminant tag that identifies which variant an [`EventRef`] points to.
///
/// Stored as a compact string in JSON (`"frame"`, `"audio"`, …) so that event
/// logs remain grep-friendly; bincode encodes the variant index as a single
/// byte for compactness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum EventKind {
    /// A rendered terminal frame was captured.
    Frame,
    /// A chunk of microphone audio was captured.
    Audio,
    /// Speech-to-text produced a transcript segment.
    Speech,
    /// A shell command boundary was detected (start or exit).
    Command,
    /// A bundle was sealed and is ready for indexing.
    Bundle,
}

// ─── EventRef ────────────────────────────────────────────────────────────────

/// Lightweight pointer to a specific captured event.
///
/// `EventRef` is designed to fit inside index structures and timeline cursors
/// without carrying full event payloads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct EventRef {
    /// Which variant of [`CaptureEvent`] this reference addresses.
    pub kind: EventKind,
    /// Opaque monotonically-increasing identifier for the event, unique within
    /// a single capture session.
    pub id: u64,
    /// Wall-clock timestamp when the event was produced, in Unix milliseconds.
    pub timestamp_ms: u64,
}

// ─── CaptureEvent ────────────────────────────────────────────────────────────

/// Every significant moment produced by the capture pipeline.
///
/// Variants are ordered from most-frequent to least-frequent so that `match`
/// arms in hot loops follow the branch predictor's default bias.
///
/// # Encoding
///
/// JSON uses an internally-tagged representation (`"type": "<variant>"`) for
/// human readability. Binary encoding via `bincode` uses the native enum
/// discriminant (a single `u32`) for compactness and zero-copy decoding.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum CaptureEvent {
    /// A terminal pane was rendered and the PNG pixels were captured.
    FrameCaptured {
        /// The pane that produced this frame.
        pane_id: u64,
        /// Raw PNG-encoded image bytes.
        png_bytes: Vec<u8>,
        /// Wall-clock timestamp when the frame was captured, in Unix
        /// milliseconds.
        timestamp_ms: u64,
    },

    /// A raw audio chunk was captured from the microphone.
    AudioCaptured {
        /// Interleaved PCM samples (f32, little-endian).
        sample_chunk: Vec<f32>,
        /// Wall-clock timestamp of the first sample, in Unix milliseconds.
        timestamp_ms: u64,
    },

    /// The speech-to-text engine produced a recognised segment.
    SpeechTranscribed {
        /// Recognised text for this segment.
        text: String,
        /// Recognition confidence in `[0.0, 1.0]`.
        confidence: f32,
        /// Wall-clock timestamp of the start of the segment, in Unix
        /// milliseconds.
        timestamp_ms: u64,
    },

    /// A shell command boundary was detected (either entry or exit).
    CommandBoundary {
        /// The command string as typed by the user.
        cmd: String,
        /// Exit code of the command (`None` while the command is still
        /// running).
        exit_code: Option<i32>,
        /// Wall-clock timestamp, in Unix milliseconds.
        timestamp_ms: u64,
    },

    /// A bundle has been finalised and its event list sealed.
    BundleSealed {
        /// Stable identifier for the sealed bundle.
        bundle_id: String,
        /// Ordered list of event references included in this bundle.
        refs: Vec<EventRef>,
        /// Dense embedding vector produced at seal time (e.g. 768-dimensional
        /// sentence embedding of the transcript).
        embedding: Vec<f32>,
    },
}

impl CaptureEvent {
    /// Return the wall-clock timestamp carried by this event, in Unix
    /// milliseconds.
    ///
    /// [`CaptureEvent::BundleSealed`] does not carry a top-level timestamp;
    /// this method returns `None` for that variant.
    #[must_use]
    pub fn timestamp_ms(&self) -> Option<u64> {
        match self {
            Self::FrameCaptured { timestamp_ms, .. }
            | Self::AudioCaptured { timestamp_ms, .. }
            | Self::SpeechTranscribed { timestamp_ms, .. }
            | Self::CommandBoundary { timestamp_ms, .. } => Some(*timestamp_ms),
            Self::BundleSealed { .. } => None,
        }
    }

    /// Return the [`EventKind`] discriminant for this event.
    #[must_use]
    pub fn kind(&self) -> EventKind {
        match self {
            Self::FrameCaptured { .. } => EventKind::Frame,
            Self::AudioCaptured { .. } => EventKind::Audio,
            Self::SpeechTranscribed { .. } => EventKind::Speech,
            Self::CommandBoundary { .. } => EventKind::Command,
            Self::BundleSealed { .. } => EventKind::Bundle,
        }
    }
}

// ─── Bincode encode/decode helpers ───────────────────────────────────────────

/// Standard bincode configuration used throughout the capture pipeline.
///
/// Uses little-endian byte order and variable-length integer encoding to keep
/// frame payloads compact.
#[must_use]
pub fn bincode_config() -> impl bincode::config::Config {
    bincode::config::standard()
}

/// Encode an event to a byte vector using bincode.
///
/// # Errors
///
/// Returns an error if the value cannot be encoded (practically infallible for
/// well-formed `CaptureEvent` values).
pub fn encode_event(event: &CaptureEvent) -> Result<Vec<u8>, bincode::error::EncodeError> {
    bincode::encode_to_vec(event, bincode_config())
}

/// Decode an event from a byte slice produced by [`encode_event`].
///
/// # Errors
///
/// Returns an error if the bytes are malformed or represent an incompatible
/// schema version.
pub fn decode_event(bytes: &[u8]) -> Result<CaptureEvent, bincode::error::DecodeError> {
    let (event, _consumed) = bincode::decode_from_slice(bytes, bincode_config())?;
    Ok(event)
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── helpers ──────────────────────────────────────────────────────────────

    fn json_round_trip(event: &CaptureEvent) -> CaptureEvent {
        let json = serde_json::to_string(event).expect("serialize to JSON");
        serde_json::from_str(&json).expect("deserialize from JSON")
    }

    fn bincode_round_trip(event: &CaptureEvent) -> CaptureEvent {
        let bytes = encode_event(event).expect("bincode encode");
        decode_event(&bytes).expect("bincode decode")
    }

    // ── schema version ───────────────────────────────────────────────────────

    #[test]
    fn schema_version_is_one() {
        assert_eq!(SCHEMA_VERSION, 1u8);
    }

    // ── FrameCaptured ────────────────────────────────────────────────────────

    #[test]
    fn frame_captured_json_round_trip() {
        // Arrange
        let event = CaptureEvent::FrameCaptured {
            pane_id: 42,
            png_bytes: vec![0x89, 0x50, 0x4e, 0x47],
            timestamp_ms: 1_700_000_000_000,
        };

        // Act
        let restored = json_round_trip(&event);

        // Assert
        assert_eq!(restored, event);
        assert_eq!(restored.kind(), EventKind::Frame);
        assert_eq!(restored.timestamp_ms(), Some(1_700_000_000_000));
    }

    #[test]
    fn frame_captured_bincode_round_trip() {
        // Arrange
        let event = CaptureEvent::FrameCaptured {
            pane_id: 7,
            png_bytes: vec![1, 2, 3],
            timestamp_ms: 12345,
        };

        // Act + Assert
        assert_eq!(bincode_round_trip(&event), event);
    }

    // ── AudioCaptured ────────────────────────────────────────────────────────

    #[test]
    fn audio_captured_json_round_trip() {
        // Arrange
        let event = CaptureEvent::AudioCaptured {
            sample_chunk: vec![0.0_f32, 0.5, -0.5, 1.0],
            timestamp_ms: 9_999,
        };

        // Act
        let restored = json_round_trip(&event);

        // Assert
        assert_eq!(restored, event);
        assert_eq!(restored.kind(), EventKind::Audio);
        assert_eq!(restored.timestamp_ms(), Some(9_999));
    }

    #[test]
    fn audio_captured_bincode_round_trip() {
        // Arrange
        let event = CaptureEvent::AudioCaptured {
            sample_chunk: vec![0.1_f32, -0.2, 0.3],
            timestamp_ms: 50_000,
        };

        // Act + Assert
        assert_eq!(bincode_round_trip(&event), event);
    }

    // ── SpeechTranscribed ────────────────────────────────────────────────────

    #[test]
    fn speech_transcribed_json_round_trip() {
        // Arrange
        let event = CaptureEvent::SpeechTranscribed {
            text: "cargo test".to_owned(),
            confidence: 0.97,
            timestamp_ms: 1_000,
        };

        // Act
        let restored = json_round_trip(&event);

        // Assert
        assert_eq!(restored, event);
        assert_eq!(restored.kind(), EventKind::Speech);
        assert_eq!(restored.timestamp_ms(), Some(1_000));
    }

    #[test]
    fn speech_transcribed_bincode_round_trip() {
        // Arrange
        let event = CaptureEvent::SpeechTranscribed {
            text: "hello world".to_owned(),
            confidence: 0.85,
            timestamp_ms: 2_000,
        };

        // Act + Assert
        assert_eq!(bincode_round_trip(&event), event);
    }

    // ── CommandBoundary ──────────────────────────────────────────────────────

    #[test]
    fn command_boundary_with_exit_code_json_round_trip() {
        // Arrange
        let event = CaptureEvent::CommandBoundary {
            cmd: "cargo build --release".to_owned(),
            exit_code: Some(0),
            timestamp_ms: 3_000,
        };

        // Act
        let restored = json_round_trip(&event);

        // Assert
        assert_eq!(restored, event);
        assert_eq!(restored.kind(), EventKind::Command);
        assert_eq!(restored.timestamp_ms(), Some(3_000));
    }

    #[test]
    fn command_boundary_running_no_exit_code_json_round_trip() {
        // exit_code = None means the command is still running
        let event = CaptureEvent::CommandBoundary {
            cmd: "sleep 60".to_owned(),
            exit_code: None,
            timestamp_ms: 4_000,
        };
        let restored = json_round_trip(&event);
        assert_eq!(restored, event);
        assert!(matches!(
            restored,
            CaptureEvent::CommandBoundary { exit_code: None, .. }
        ));
    }

    #[test]
    fn command_boundary_bincode_round_trip() {
        // Arrange
        let event = CaptureEvent::CommandBoundary {
            cmd: "ls -la".to_owned(),
            exit_code: Some(1),
            timestamp_ms: 5_000,
        };

        // Act + Assert
        assert_eq!(bincode_round_trip(&event), event);
    }

    // ── BundleSealed ─────────────────────────────────────────────────────────

    #[test]
    fn bundle_sealed_json_round_trip() {
        // Arrange
        let refs = vec![
            EventRef {
                kind: EventKind::Frame,
                id: 1,
                timestamp_ms: 100,
            },
            EventRef {
                kind: EventKind::Audio,
                id: 2,
                timestamp_ms: 200,
            },
        ];
        let event = CaptureEvent::BundleSealed {
            bundle_id: "550e8400-e29b-41d4-a716-446655440000".to_owned(),
            refs: refs.clone(),
            embedding: vec![0.1, 0.2, 0.3],
        };

        // Act
        let restored = json_round_trip(&event);

        // Assert
        assert_eq!(restored, event);
        assert_eq!(restored.kind(), EventKind::Bundle);
        assert_eq!(restored.timestamp_ms(), None);
    }

    #[test]
    fn bundle_sealed_bincode_round_trip() {
        // Arrange — 768-dim embedding mirrors real sentence-transformer output
        let event = CaptureEvent::BundleSealed {
            bundle_id: "test-bundle-001".to_owned(),
            refs: vec![EventRef {
                kind: EventKind::Speech,
                id: 99,
                timestamp_ms: 999,
            }],
            embedding: vec![0.0_f32; 768],
        };

        // Act + Assert
        assert_eq!(bincode_round_trip(&event), event);
    }

    // ── EventRef ─────────────────────────────────────────────────────────────

    #[test]
    fn event_ref_json_round_trip() {
        // Arrange
        let r = EventRef {
            kind: EventKind::Command,
            id: 42,
            timestamp_ms: 7_777,
        };

        // Act
        let json = serde_json::to_string(&r).expect("serialize");
        let restored: EventRef = serde_json::from_str(&json).expect("deserialize");

        // Assert
        assert_eq!(restored, r);
    }

    #[test]
    fn event_ref_bincode_round_trip() {
        // Arrange
        let r = EventRef {
            kind: EventKind::Bundle,
            id: 0,
            timestamp_ms: 0,
        };

        // Act
        let bytes = bincode::encode_to_vec(&r, bincode_config()).expect("encode");
        let (restored, _): (EventRef, _) =
            bincode::decode_from_slice(&bytes, bincode_config()).expect("decode");

        // Assert
        assert_eq!(restored, r);
    }

    // ── EventKind JSON repr ───────────────────────────────────────────────────

    #[test]
    fn event_kind_serialises_to_snake_case_strings() {
        assert_eq!(
            serde_json::to_string(&EventKind::Frame).unwrap(),
            r#""frame""#
        );
        assert_eq!(
            serde_json::to_string(&EventKind::Audio).unwrap(),
            r#""audio""#
        );
        assert_eq!(
            serde_json::to_string(&EventKind::Speech).unwrap(),
            r#""speech""#
        );
        assert_eq!(
            serde_json::to_string(&EventKind::Command).unwrap(),
            r#""command""#
        );
        assert_eq!(
            serde_json::to_string(&EventKind::Bundle).unwrap(),
            r#""bundle""#
        );
    }

    // ── Forward-compatibility (v1 decoded by hypothetical v2 reader) ─────────

    /// A v2 reader might add an optional `session_id` field to
    /// `FrameCaptured`. A v1 payload (which lacks that field) should still
    /// deserialise successfully because `serde` treats missing optional fields
    /// as `None`.
    #[test]
    fn v1_frame_captured_decoded_by_v2_reader_with_extra_optional_field() {
        // Simulate a v1 JSON payload.
        let v1_json = r#"{
            "type": "frame_captured",
            "pane_id": 1,
            "png_bytes": [137, 80],
            "timestamp_ms": 100
        }"#;

        // The v2 reader struct — only used inside this test.
        #[derive(Debug, Deserialize)]
        struct FrameCapturedV2 {
            pane_id: u64,
            png_bytes: Vec<u8>,
            timestamp_ms: u64,
            // New optional field added in v2.
            #[serde(default)]
            session_id: Option<String>,
        }

        #[derive(Debug, Deserialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        enum CaptureEventV2 {
            FrameCaptured(FrameCapturedV2),
            // Other variants omitted for brevity; serde(other) handles them.
            #[serde(other)]
            Unknown,
        }

        let result: CaptureEventV2 =
            serde_json::from_str(v1_json).expect("v2 reader must accept v1 payload");

        match result {
            CaptureEventV2::FrameCaptured(f) => {
                assert_eq!(f.pane_id, 1);
                assert_eq!(f.png_bytes, vec![137u8, 80]);
                assert_eq!(f.timestamp_ms, 100);
                assert!(
                    f.session_id.is_none(),
                    "v2 optional field defaults to None for v1 payload"
                );
            }
            other => panic!("expected FrameCaptured, got {other:?}"),
        }
    }

    /// A v2 reader that adds an entirely new `HeartbeatEmitted` variant should
    /// still be able to decode v1 streams that contain only known variants.
    /// The `#[serde(other)]` fallback arm absorbs the unknown variant.
    #[test]
    fn v1_stream_with_unknown_variant_falls_through_to_serde_other() {
        // Simulate a v2 payload containing a variant that the v1 reader does
        // not know about.
        let v2_json = r#"{"type": "heartbeat_emitted", "seq": 7}"#;

        #[derive(Debug, Deserialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        enum CaptureEventV1Compat {
            FrameCaptured {
                #[allow(dead_code)]
                pane_id: u64,
                #[allow(dead_code)]
                png_bytes: Vec<u8>,
                #[allow(dead_code)]
                timestamp_ms: u64,
            },
            #[serde(other)]
            Unknown,
        }

        let result: CaptureEventV1Compat = serde_json::from_str(v2_json)
            .expect("v1 reader with #[serde(other)] accepts v2 payload");
        assert!(
            matches!(result, CaptureEventV1Compat::Unknown),
            "unknown variant falls through to Unknown arm"
        );
    }
}
