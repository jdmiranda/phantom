//! Versioned schema for the `phantom-memory` event log.
//!
//! Defines the canonical layout of an [`EventLogEntry`], a schema-version
//! constant, and forward-compatible migration stubs.  The design follows
//! skip-unknown semantics: a v1 reader silently ignores fields that were not
//! present in v1, so a v2 log is readable by a v1 library (with data loss for
//! the new fields, but no parse errors).
//!
//! # Schema versions
//!
//! | Version | Changes |
//! |---------|---------|
//! | 1       | Initial: `id`, `timestamp_ms`, `kind`, `payload`, `source_chain` |
//!
//! # Kind validation
//!
//! Every `kind` string must either:
//!
//! 1. Match a known dotted-path kind from [`KnownKind`], or
//! 2. Begin with the `unknown.` prefix (for forward-compat with future kinds).
//!
//! Any other string is rejected by [`EventLogEntry::validate`].
//!
//! # Example
//!
//! ```rust
//! use phantom_memory::schema::{EventLogEntry, SCHEMA_VERSION};
//!
//! assert_eq!(SCHEMA_VERSION, 1);
//!
//! let entry = EventLogEntry::new(
//!     1,
//!     1_700_000_000_000,
//!     "agent.spawn",
//!     serde_json::json!({"agent_id": 42}),
//!     vec![],
//! ).unwrap();
//!
//! assert_eq!(entry.id(), 1);
//! assert!(entry.validate().is_ok());
//! ```

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The current schema version.
///
/// Increment this constant (and add a migration stub) whenever the
/// [`EventLogEntry`] layout changes in a backward-incompatible way.
pub const SCHEMA_VERSION: u32 = 1;

// ── Known kind set ────────────────────────────────────────────────────────────

/// All event kind strings that are valid under the current schema version.
///
/// Each variant maps to a dotted-path string that appears in the JSONL log.
/// Extend this list when a new event kind is introduced; retired kinds should
/// be moved to a `#[deprecated]` variant rather than removed, to preserve
/// read-compatibility with historical logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KnownKind {
    // ── Agent lifecycle ──────────────────────────────────────────────────────
    AgentSpawn,
    AgentComplete,
    AgentFlatline,
    AgentQuarantine,

    // ── Tool calls ───────────────────────────────────────────────────────────
    ToolInvoked,
    ToolSucceeded,
    ToolFailed,

    // ── Pipeline ─────────────────────────────────────────────────────────────
    PipelineStarted,
    PipelineBlocked,
    PipelineCompleted,

    // ── Brain / OODA ─────────────────────────────────────────────────────────
    BrainObserve,
    BrainAct,

    // ── User ─────────────────────────────────────────────────────────────────
    UserCommand,
}

impl KnownKind {
    /// The canonical dotted-path string for this kind.
    pub fn as_str(self) -> &'static str {
        match self {
            KnownKind::AgentSpawn => "agent.spawn",
            KnownKind::AgentComplete => "agent.complete",
            KnownKind::AgentFlatline => "agent.flatline",
            KnownKind::AgentQuarantine => "agent.quarantine",
            KnownKind::ToolInvoked => "tool.invoked",
            KnownKind::ToolSucceeded => "tool.succeeded",
            KnownKind::ToolFailed => "tool.failed",
            KnownKind::PipelineStarted => "pipeline.started",
            KnownKind::PipelineBlocked => "pipeline.blocked",
            KnownKind::PipelineCompleted => "pipeline.completed",
            KnownKind::BrainObserve => "brain.observe",
            KnownKind::BrainAct => "brain.act",
            KnownKind::UserCommand => "user.command",
        }
    }

    /// Try to match a string against the full known-kind set.
    ///
    /// Returns `None` if the string is not a recognized kind (and not an
    /// `unknown.*` prefix — callers must check that separately).
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "agent.spawn" => Some(KnownKind::AgentSpawn),
            "agent.complete" => Some(KnownKind::AgentComplete),
            "agent.flatline" => Some(KnownKind::AgentFlatline),
            "agent.quarantine" => Some(KnownKind::AgentQuarantine),
            "tool.invoked" => Some(KnownKind::ToolInvoked),
            "tool.succeeded" => Some(KnownKind::ToolSucceeded),
            "tool.failed" => Some(KnownKind::ToolFailed),
            "pipeline.started" => Some(KnownKind::PipelineStarted),
            "pipeline.blocked" => Some(KnownKind::PipelineBlocked),
            "pipeline.completed" => Some(KnownKind::PipelineCompleted),
            "brain.observe" => Some(KnownKind::BrainObserve),
            "brain.act" => Some(KnownKind::BrainAct),
            "user.command" => Some(KnownKind::UserCommand),
            _ => None,
        }
    }

    /// All known kinds, in declaration order.  Useful for exhaustive tests.
    pub fn all() -> &'static [KnownKind] {
        &[
            KnownKind::AgentSpawn,
            KnownKind::AgentComplete,
            KnownKind::AgentFlatline,
            KnownKind::AgentQuarantine,
            KnownKind::ToolInvoked,
            KnownKind::ToolSucceeded,
            KnownKind::ToolFailed,
            KnownKind::PipelineStarted,
            KnownKind::PipelineBlocked,
            KnownKind::PipelineCompleted,
            KnownKind::BrainObserve,
            KnownKind::BrainAct,
            KnownKind::UserCommand,
        ]
    }
}

impl std::fmt::Display for KnownKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ── EventLogEntry ─────────────────────────────────────────────────────────────

/// A single entry in the versioned event log.
///
/// All fields are private; construct with [`EventLogEntry::new`] and access
/// them through the provided getters.
///
/// # Schema stability
///
/// The JSON representation of this struct is the on-disk format.  Fields must
/// not be renamed or removed in a patch release — use `#[serde(rename)]` or
/// `#[serde(skip_serializing_if)]` to maintain backward compatibility.
///
/// Optional fields added in future versions should be `Option<T>` and
/// `#[serde(default)]` so that v1 entries remain decodable by v2+ readers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventLogEntry {
    /// Monotonically increasing identifier assigned by the log writer.
    id: u64,
    /// Wall-clock time in milliseconds since the Unix epoch.
    timestamp_ms: u64,
    /// Dotted-path event kind (e.g. `"agent.spawn"`).
    ///
    /// Must be a known kind OR start with `unknown.` — enforced by
    /// [`EventLogEntry::validate`].
    kind: String,
    /// Free-form structured payload specific to `kind`.
    payload: serde_json::Value,
    /// Ordered list of ancestor event ids for causal tracing.
    ///
    /// Empty for root events (no causal parent).
    source_chain: Vec<u64>,
    // ── v2 extension point ────────────────────────────────────────────────
    /// Causality token linking this entry to the pipeline run that triggered
    /// it.  The string form of a v4 UUID (hyphenated, 36 chars) matches the
    /// `CorrelationId` wire representation from `phantom-agents`.
    ///
    /// `None` for entries that were not produced in a tracked pipeline run
    /// (legacy / test paths).  Skip-serializing when absent keeps v1 logs
    /// readable by v1 readers without a parse error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    correlation_id: Option<String>,
}

/// Errors produced when constructing or validating an [`EventLogEntry`].
#[derive(Debug, Error)]
pub enum SchemaError {
    /// The `kind` string is neither a known kind nor `unknown.*`-namespaced.
    #[error("invalid kind string {kind:?}: must be a known kind or start with 'unknown.'")]
    InvalidKind { kind: String },

    /// A migration function detected an unrecognised source version.
    #[error("unsupported schema version {version}: cannot migrate")]
    UnsupportedVersion { version: u32 },
}

impl EventLogEntry {
    /// Construct a new [`EventLogEntry`], validating `kind` immediately.
    ///
    /// # Errors
    ///
    /// Returns [`SchemaError::InvalidKind`] if `kind` is neither a known
    /// dotted-path kind nor prefixed with `unknown.`.
    pub fn new(
        id: u64,
        timestamp_ms: u64,
        kind: impl Into<String>,
        payload: serde_json::Value,
        source_chain: Vec<u64>,
    ) -> Result<Self, SchemaError> {
        let kind = kind.into();
        validate_kind(&kind)?;
        Ok(Self {
            id,
            timestamp_ms,
            kind,
            payload,
            source_chain,
            correlation_id: None,
        })
    }

    /// Attach a correlation id to this entry, consuming `self` and returning
    /// the updated entry.
    ///
    /// The id is stored as a `String` (hyphenated UUID) for serde
    /// compatibility with the on-disk JSONL format.
    #[must_use]
    pub fn with_correlation_id(mut self, id: impl Into<String>) -> Self {
        self.correlation_id = Some(id.into());
        self
    }

    /// Monotonically increasing log id.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Creation time in milliseconds since the Unix epoch.
    pub fn timestamp_ms(&self) -> u64 {
        self.timestamp_ms
    }

    /// Dotted-path event kind string.
    pub fn kind(&self) -> &str {
        &self.kind
    }

    /// Structured payload.
    pub fn payload(&self) -> &serde_json::Value {
        &self.payload
    }

    /// Ordered list of causal ancestor event ids.
    pub fn source_chain(&self) -> &[u64] {
        &self.source_chain
    }

    /// Causality token linking this entry to its pipeline run, if present.
    ///
    /// Returns the string form of the UUID (hyphenated, 36 chars) or `None`
    /// for entries that were not produced in a tracked pipeline run.
    pub fn correlation_id(&self) -> Option<&str> {
        self.correlation_id.as_deref()
    }

    /// Validate this entry against the current schema.
    ///
    /// The primary check is kind-string validation (known OR `unknown.*`).
    /// Additional field-level invariants may be added here without breaking
    /// the constructor API.
    ///
    /// # Errors
    ///
    /// Returns [`SchemaError::InvalidKind`] if the kind string is invalid.
    pub fn validate(&self) -> Result<(), SchemaError> {
        validate_kind(&self.kind)
    }

    /// Deserialize an [`EventLogEntry`] from a JSON value using
    /// skip-unknown semantics.
    ///
    /// Fields present in `value` that are not part of the current schema
    /// are silently ignored.  Missing optional fields (introduced in later
    /// schema versions) are filled with their defaults.
    ///
    /// This enables a v1 reader to decode a v2 entry without error.
    pub fn from_json(value: serde_json::Value) -> Result<Self, serde_json::Error> {
        serde_json::from_value(value)
    }
}

// ── Kind validation ───────────────────────────────────────────────────────────

/// Validate a kind string: must be a known kind OR start with `unknown.`.
fn validate_kind(kind: &str) -> Result<(), SchemaError> {
    if KnownKind::from_str(kind).is_some() || kind.starts_with("unknown.") {
        Ok(())
    } else {
        Err(SchemaError::InvalidKind {
            kind: kind.to_owned(),
        })
    }
}

// ── Schema migration stubs ────────────────────────────────────────────────────

/// Migrate a raw JSON value from `from_version` to [`SCHEMA_VERSION`].
///
/// This function is a stub: it accepts v1 entries unchanged (since
/// `SCHEMA_VERSION == 1`).  When schema v2 is introduced, add a
/// `migrate_v1_to_v2` inner function and call it here.
///
/// # Errors
///
/// Returns [`SchemaError::UnsupportedVersion`] for any version other than
/// those the current codebase knows how to handle.
pub fn migrate(
    value: serde_json::Value,
    from_version: u32,
) -> Result<serde_json::Value, SchemaError> {
    match from_version {
        1 => {
            // v1 → current (v1): nothing to do — field set is identical.
            Ok(value)
        }
        v => Err(SchemaError::UnsupportedVersion { version: v }),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── SCHEMA_VERSION ────────────────────────────────────────────────────────

    #[test]
    fn schema_version_is_one() {
        assert_eq!(SCHEMA_VERSION, 1);
    }

    // ── KnownKind ─────────────────────────────────────────────────────────────

    #[test]
    fn all_known_kinds_round_trip_via_from_str() {
        for &kind in KnownKind::all() {
            let s = kind.as_str();
            let back =
                KnownKind::from_str(s).unwrap_or_else(|| panic!("from_str({s:?}) returned None"));
            assert_eq!(back, kind, "round-trip failed for {s:?}");
        }
    }

    #[test]
    fn known_kind_from_str_covers_all_13_variants() {
        // Guard: if a variant is added to KnownKind::all() without updating
        // from_str, this test catches the gap.
        assert_eq!(KnownKind::all().len(), 13);
    }

    #[test]
    fn unknown_kind_from_str_returns_none() {
        assert!(KnownKind::from_str("not.a.real.kind").is_none());
        assert!(KnownKind::from_str("unknown.future_field").is_none());
        assert!(KnownKind::from_str("").is_none());
    }

    #[test]
    fn known_kind_display_matches_as_str() {
        for &kind in KnownKind::all() {
            assert_eq!(kind.to_string(), kind.as_str());
        }
    }

    // ── EventLogEntry construction ─────────────────────────────────────────────

    #[test]
    fn new_with_known_kind_succeeds() {
        for &k in KnownKind::all() {
            let entry = EventLogEntry::new(1, 1_000_000, k.as_str(), json!({}), vec![])
                .unwrap_or_else(|e| panic!("new failed for {}: {e}", k.as_str()));
            assert_eq!(entry.kind(), k.as_str());
        }
    }

    #[test]
    fn new_with_unknown_prefix_succeeds() {
        let entry = EventLogEntry::new(
            42,
            2_000,
            "unknown.future_field",
            json!({"x": 1}),
            vec![1, 2],
        )
        .unwrap();
        assert_eq!(entry.id(), 42);
        assert_eq!(entry.kind(), "unknown.future_field");
        assert_eq!(entry.source_chain(), &[1, 2]);
    }

    #[test]
    fn new_with_invalid_kind_returns_error() {
        let err = EventLogEntry::new(1, 0, "bad-kind-string", json!({}), vec![]).unwrap_err();
        assert!(matches!(err, SchemaError::InvalidKind { .. }));
    }

    #[test]
    fn new_with_empty_kind_returns_error() {
        let err = EventLogEntry::new(1, 0, "", json!({}), vec![]).unwrap_err();
        assert!(matches!(err, SchemaError::InvalidKind { .. }));
    }

    // ── Getters ───────────────────────────────────────────────────────────────

    #[test]
    fn getters_return_constructor_values() {
        let chain = vec![10u64, 20, 30];
        let payload = json!({"agent_id": 7, "task": "test"});
        let entry = EventLogEntry::new(
            99,
            1_234_567_890,
            "agent.spawn",
            payload.clone(),
            chain.clone(),
        )
        .unwrap();

        assert_eq!(entry.id(), 99);
        assert_eq!(entry.timestamp_ms(), 1_234_567_890);
        assert_eq!(entry.kind(), "agent.spawn");
        assert_eq!(entry.payload(), &payload);
        assert_eq!(entry.source_chain(), chain.as_slice());
    }

    // ── validate ──────────────────────────────────────────────────────────────

    #[test]
    fn validate_known_kind_ok() {
        let entry = EventLogEntry::new(1, 0, "tool.invoked", json!({}), vec![]).unwrap();
        assert!(entry.validate().is_ok());
    }

    #[test]
    fn validate_unknown_prefix_ok() {
        let entry = EventLogEntry::new(1, 0, "unknown.v3_feature", json!({}), vec![]).unwrap();
        assert!(entry.validate().is_ok());
    }

    // ── Serde round-trip for all known kinds ──────────────────────────────────

    #[test]
    fn serde_round_trip_for_all_known_kinds() {
        for &kind in KnownKind::all() {
            let entry = EventLogEntry::new(
                1,
                1_700_000_000_000,
                kind.as_str(),
                json!({"test": true}),
                vec![],
            )
            .unwrap();

            let serialized = serde_json::to_string(&entry)
                .unwrap_or_else(|e| panic!("serialize failed for {}: {e}", kind.as_str()));
            let back: EventLogEntry = serde_json::from_str(&serialized)
                .unwrap_or_else(|e| panic!("deserialize failed for {}: {e}", kind.as_str()));

            assert_eq!(back.id(), entry.id());
            assert_eq!(back.kind(), entry.kind());
            assert_eq!(back.payload(), entry.payload());
            assert!(back.validate().is_ok());
        }
    }

    // ── from_json (skip-unknown semantics) ────────────────────────────────────

    #[test]
    fn from_json_v1_entry_decodes_ok() {
        let raw = json!({
            "id": 1,
            "timestamp_ms": 1_700_000_000_000u64,
            "kind": "agent.spawn",
            "payload": {"agent_id": 1},
            "source_chain": []
        });
        let entry = EventLogEntry::from_json(raw).unwrap();
        assert_eq!(entry.id(), 1);
        assert_eq!(entry.kind(), "agent.spawn");
    }

    #[test]
    fn from_json_ignores_unknown_fields() {
        // Simulates a v2 entry being read by v1 code.
        let raw = json!({
            "id": 5,
            "timestamp_ms": 9_000u64,
            "kind": "tool.invoked",
            "payload": {},
            "source_chain": [1, 2],
            // v2-only field — should be silently ignored
            "correlation_id": "abc-123"
        });
        let entry = EventLogEntry::from_json(raw).unwrap();
        assert_eq!(entry.id(), 5);
        assert_eq!(entry.source_chain(), &[1, 2]);
    }

    // ── source_chain ──────────────────────────────────────────────────────────

    #[test]
    fn source_chain_empty_for_root_events() {
        let entry = EventLogEntry::new(1, 0, "user.command", json!({}), vec![]).unwrap();
        assert!(entry.source_chain().is_empty());
    }

    #[test]
    fn source_chain_preserves_causal_order() {
        let chain = vec![1u64, 5, 9];
        let entry = EventLogEntry::new(10, 0, "agent.spawn", json!({}), chain.clone()).unwrap();
        assert_eq!(entry.source_chain(), chain.as_slice());
    }

    // ── migrate ───────────────────────────────────────────────────────────────

    #[test]
    fn migrate_v1_is_identity() {
        let raw = json!({
            "id": 1,
            "timestamp_ms": 0u64,
            "kind": "agent.spawn",
            "payload": {"x": 1},
            "source_chain": []
        });
        let migrated = migrate(raw.clone(), 1).unwrap();
        assert_eq!(migrated, raw);
    }

    #[test]
    fn migrate_unsupported_version_returns_error() {
        let raw = json!({});
        let err = migrate(raw, 99).unwrap_err();
        assert!(matches!(
            err,
            SchemaError::UnsupportedVersion { version: 99 }
        ));
    }

    #[test]
    fn migrate_then_decode_round_trips_v1() {
        let original = json!({
            "id": 7,
            "timestamp_ms": 1_000_000u64,
            "kind": "brain.observe",
            "payload": {"score": 0.9},
            "source_chain": [1, 2, 3]
        });

        let migrated = migrate(original, 1).unwrap();
        let entry = EventLogEntry::from_json(migrated).unwrap();

        assert_eq!(entry.id(), 7);
        assert_eq!(entry.kind(), "brain.observe");
        assert_eq!(entry.source_chain(), &[1, 2, 3]);
    }

    // ── kind validation edge cases ────────────────────────────────────────────

    #[test]
    fn kind_with_no_dot_and_no_unknown_prefix_rejected() {
        let kinds = [
            "agentspawn",
            "AGENT.SPAWN",
            "agent",
            " agent.spawn",
            "agent.spawn ",
        ];
        for bad in kinds {
            let err = EventLogEntry::new(1, 0, bad, json!({}), vec![]).unwrap_err();
            assert!(
                matches!(err, SchemaError::InvalidKind { .. }),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn unknown_dot_prefix_accepted_with_any_suffix() {
        let kinds = [
            "unknown.v2_field",
            "unknown.really.long.dotted.path",
            "unknown.12345",
        ];
        for kind in kinds {
            EventLogEntry::new(1, 0, kind, json!({}), vec![])
                .unwrap_or_else(|e| panic!("{kind:?} should be accepted: {e}"));
        }
    }

    // ── SchemaError display ────────────────────────────────────────────────────

    #[test]
    fn schema_error_display_is_human_readable() {
        let err = SchemaError::InvalidKind {
            kind: "bad".to_owned(),
        };
        let msg = err.to_string();
        assert!(msg.contains("bad"), "message: {msg}");
        assert!(msg.contains("unknown."), "message: {msg}");

        let err2 = SchemaError::UnsupportedVersion { version: 42 };
        let msg2 = err2.to_string();
        assert!(msg2.contains("42"), "message: {msg2}");
    }
}
