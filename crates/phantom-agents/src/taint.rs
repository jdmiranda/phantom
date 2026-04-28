//! Taint tracking for tool results.
//!
//! Each [`crate::tools::ToolResult`] carries a [`TaintLevel`] (via its
//! [`crate::tools::ToolProvenance`]) so future quarantine logic can decide
//! which agents are repeat offenders. Taint propagates: a tool result derived
//! from a tainted source is itself tainted.
//!
//! This module is foundation only — it defines the type, ordering, and merge
//! semantics. Wiring elevation logic (e.g. on a `CapabilityDenied` event) and
//! the dispatch-time taint inspection that drives quarantine are intentionally
//! left to follow-up tasks.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// TaintLevel
// ---------------------------------------------------------------------------

/// How "trusted" a tool result is, relative to upstream denials.
///
/// `Clean` is the baseline. `Suspect` is a soft signal: somewhere upstream a
/// denial fired, but the result itself didn't trip a capability check.
/// `Tainted` is a hard signal: the result derives from a denied or
/// quarantined agent, and any further use should be treated with suspicion.
///
/// `TaintLevel` is totally ordered by severity (`Clean < Suspect < Tainted`),
/// so [`TaintLevel::merge`] returns the worst of two sources — the right
/// behaviour for propagation when a result is built from multiple inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum TaintLevel {
    /// Untainted. The default and most common case.
    #[default]
    Clean,
    /// Upstream had a denial, but this result itself didn't trip.
    Suspect,
    /// Upstream came from a denied or quarantined agent.
    Tainted,
}

impl TaintLevel {
    /// Merge two taint levels by taking the higher severity.
    ///
    /// `Clean < Suspect < Tainted`. Used when a result is derived from
    /// multiple sources and should inherit the worst of them.
    #[must_use]
    pub fn merge(self, other: Self) -> Self {
        if self.severity() >= other.severity() {
            self
        } else {
            other
        }
    }

    /// Numeric severity, used by [`Self::merge`] and ordering helpers.
    #[must_use]
    fn severity(self) -> u8 {
        match self {
            Self::Clean => 0,
            Self::Suspect => 1,
            Self::Tainted => 2,
        }
    }

    /// Returns `true` iff this level is [`TaintLevel::Clean`].
    #[must_use]
    pub fn is_clean(self) -> bool {
        matches!(self, Self::Clean)
    }

    /// Returns `true` iff this level is [`TaintLevel::Tainted`].
    ///
    /// `Suspect` is *not* considered tainted — it's a soft signal. Callers
    /// that want "anything non-clean" should compare against
    /// [`TaintLevel::Clean`] directly.
    #[must_use]
    pub fn is_tainted(self) -> bool {
        matches!(self, Self::Tainted)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_clean() {
        assert_eq!(TaintLevel::default(), TaintLevel::Clean);
    }

    #[test]
    fn merge_is_idempotent() {
        for level in [TaintLevel::Clean, TaintLevel::Suspect, TaintLevel::Tainted] {
            assert_eq!(level.merge(level), level);
        }
    }

    #[test]
    fn merge_picks_max_severity() {
        // Clean is the identity element.
        assert_eq!(
            TaintLevel::Clean.merge(TaintLevel::Suspect),
            TaintLevel::Suspect
        );
        assert_eq!(
            TaintLevel::Clean.merge(TaintLevel::Tainted),
            TaintLevel::Tainted
        );
        // Tainted dominates everything.
        assert_eq!(
            TaintLevel::Tainted.merge(TaintLevel::Suspect),
            TaintLevel::Tainted
        );
        assert_eq!(
            TaintLevel::Tainted.merge(TaintLevel::Clean),
            TaintLevel::Tainted
        );
        // Suspect beats Clean but loses to Tainted.
        assert_eq!(
            TaintLevel::Suspect.merge(TaintLevel::Clean),
            TaintLevel::Suspect
        );
        assert_eq!(
            TaintLevel::Suspect.merge(TaintLevel::Tainted),
            TaintLevel::Tainted
        );
    }

    #[test]
    fn merge_is_commutative() {
        let levels = [TaintLevel::Clean, TaintLevel::Suspect, TaintLevel::Tainted];
        for a in levels {
            for b in levels {
                assert_eq!(a.merge(b), b.merge(a), "merge not commutative for {a:?},{b:?}");
            }
        }
    }

    #[test]
    fn merge_is_associative() {
        let levels = [TaintLevel::Clean, TaintLevel::Suspect, TaintLevel::Tainted];
        for a in levels {
            for b in levels {
                for c in levels {
                    assert_eq!(
                        a.merge(b).merge(c),
                        a.merge(b.merge(c)),
                        "merge not associative for {a:?},{b:?},{c:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn is_clean_only_for_clean() {
        assert!(TaintLevel::Clean.is_clean());
        assert!(!TaintLevel::Suspect.is_clean());
        assert!(!TaintLevel::Tainted.is_clean());
    }

    #[test]
    fn is_tainted_only_for_tainted() {
        // Suspect is a soft signal — not the same as Tainted.
        assert!(!TaintLevel::Clean.is_tainted());
        assert!(!TaintLevel::Suspect.is_tainted());
        assert!(TaintLevel::Tainted.is_tainted());
    }
}
