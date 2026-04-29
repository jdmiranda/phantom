//! Correlation ID — causality token for end-to-end tracing.
//!
//! A [`CorrelationId`] is a `v4` UUID wrapped in a newtype so it is
//! type-safe and cannot be confused with any other `Uuid`-based identifier in
//! the system (agent ids, event ids, session ids, …).
//!
//! ## Semantics
//!
//! Every user action that can fan out across multiple agents — spawning an
//! agent pipeline, running a multi-step task, issuing a natural-language
//! command — should be tagged with a single [`CorrelationId`] at origin.
//! Every agent spawned in the same pipeline inherits the same id, every tool
//! call carries it in [`crate::dispatch::DispatchContext::correlation_id`],
//! and every log/tracing span attaches it so post-mortem queries can answer
//! "show me all agents in this pipeline run" with `WHERE correlation_id = ?`.
//!
//! ## Optional everywhere
//!
//! The id is always `Option<CorrelationId>` at the boundary. `None` means
//! "no tracing context" — the correct value for legacy test paths and any
//! dispatch that was not initiated by a tracked user action. No correlation
//! context must never be treated as an error.

use std::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// CorrelationId
// ---------------------------------------------------------------------------

/// Causality token that groups all events, tool calls, and log entries
/// belonging to the same user-initiated pipeline run.
///
/// Internally a v4 UUID; exposed as a newtype so call-sites cannot
/// accidentally pass an arbitrary [`uuid::Uuid`] where a correlation id is
/// expected.
///
/// # Construction
///
/// ```
/// use phantom_agents::correlation::CorrelationId;
///
/// let id = CorrelationId::new();         // fresh random id
/// let same = id;                         // Copy
/// assert_eq!(id, same);
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CorrelationId(Uuid);

impl CorrelationId {
    /// Generate a fresh, random correlation id.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Return the underlying [`Uuid`].
    #[must_use]
    pub fn as_uuid(&self) -> Uuid {
        self.0
    }
}

impl fmt::Display for CorrelationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl fmt::Debug for CorrelationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CorrelationId({})", self.0)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    // ---- Construction ----------------------------------------------------------

    #[test]
    fn new_returns_distinct_ids() {
        // Two calls to `new()` must produce different ids — v4 UUIDs have
        // overwhelming probability of uniqueness.
        let a = CorrelationId::new();
        let b = CorrelationId::new();
        assert_ne!(a, b, "two fresh CorrelationIds must be distinct");
    }

    // ---- Copy / Clone ----------------------------------------------------------

    #[test]
    fn copy_produces_equal_id() {
        let original = CorrelationId::new();
        let copied = original; // Copy, not move
        assert_eq!(original, copied, "copied id must equal original");
    }

    #[test]
    fn clone_produces_equal_id() {
        let original = CorrelationId::new();
        let cloned = original.clone();
        assert_eq!(original, cloned, "cloned id must equal original");
    }

    // ---- PartialEq / Eq / Hash -------------------------------------------------

    #[test]
    fn eq_is_reflexive() {
        let id = CorrelationId::new();
        assert_eq!(id, id);
    }

    #[test]
    fn hash_is_stable_for_equal_ids() {
        // Equal ids must produce the same hash — required by the Hash contract.
        let mut set = HashSet::new();
        let id = CorrelationId::new();
        set.insert(id);
        set.insert(id); // same id inserted twice
        assert_eq!(set.len(), 1, "identical ids must hash to the same bucket");
    }

    #[test]
    fn distinct_ids_can_coexist_in_hash_set() {
        let mut set = HashSet::new();
        for _ in 0..10 {
            set.insert(CorrelationId::new());
        }
        assert_eq!(set.len(), 10, "each distinct id must occupy its own bucket");
    }

    // ---- Display / Debug -------------------------------------------------------

    #[test]
    fn display_emits_hyphenated_uuid_format() {
        let id = CorrelationId::new();
        let s = id.to_string();
        // A standard hyphenated UUID has the form xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx
        // (8-4-4-4-12 hex chars separated by hyphens = 36 total chars).
        assert_eq!(s.len(), 36, "Display must emit a 36-char hyphenated UUID");
        let parts: Vec<&str> = s.split('-').collect();
        assert_eq!(parts.len(), 5, "Display must produce 5 hyphen-separated groups");
        assert!(
            s.chars().all(|c| c.is_ascii_hexdigit() || c == '-'),
            "Display must contain only hex digits and hyphens, got: {s}",
        );
    }

    #[test]
    fn debug_includes_correlation_id_wrapper_name() {
        let id = CorrelationId::new();
        let dbg = format!("{id:?}");
        assert!(
            dbg.starts_with("CorrelationId("),
            "Debug must start with 'CorrelationId(', got: {dbg}",
        );
        assert!(dbg.ends_with(')'), "Debug must end with ')', got: {dbg}");
    }

    // ---- as_uuid accessor -------------------------------------------------------

    #[test]
    fn as_uuid_round_trips() {
        let id = CorrelationId::new();
        let uuid = id.as_uuid();
        // Wrapping the same UUID back must compare equal.
        let reconstructed = CorrelationId(uuid);
        assert_eq!(id, reconstructed, "as_uuid round-trip must preserve identity");
    }

    // ---- Serde -----------------------------------------------------------------

    #[test]
    fn serializes_and_deserializes_roundtrip() {
        let id = CorrelationId::new();
        let json = serde_json::to_string(&id).expect("serialize must not fail");
        let restored: CorrelationId =
            serde_json::from_str(&json).expect("deserialize must not fail");
        assert_eq!(id, restored, "serde round-trip must preserve identity");
    }

    #[test]
    fn option_none_serializes_to_null() {
        let opt: Option<CorrelationId> = None;
        let json = serde_json::to_string(&opt).expect("serialize must not fail");
        assert_eq!(json, "null");
    }

    #[test]
    fn option_some_round_trips() {
        let id = CorrelationId::new();
        let opt = Some(id);
        let json = serde_json::to_string(&opt).expect("serialize must not fail");
        let restored: Option<CorrelationId> =
            serde_json::from_str(&json).expect("deserialize must not fail");
        assert_eq!(opt, restored, "Option<CorrelationId> serde round-trip failed");
    }
}
