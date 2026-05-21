//! Stable identifiers for a running [`crate::LoopSpec`] instance.
//!
//! A [`LoopId`] is the runtime handle the (future) loop runner assigns when it
//! materialises a spec — it is distinct from the user-chosen string id on
//! [`crate::LoopSpec::id`], which exists at TOML-edit time. Future slices
//! (C2 — runner) will mint these ids monotonically.

use serde::{Deserialize, Serialize};

/// Runtime handle for a single loop instance. Opaque to callers; future
/// slices mint these via a monotonic counter inside the loop registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LoopId(pub u64);

impl std::fmt::Display for LoopId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "loop#{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loop_id_roundtrips_through_json() {
        let id = LoopId(42);
        let json = serde_json::to_string(&id).expect("serialize");
        let back: LoopId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, id);
    }

    #[test]
    fn loop_id_display_uses_pound_prefix() {
        assert_eq!(LoopId(7).to_string(), "loop#7");
    }
}
