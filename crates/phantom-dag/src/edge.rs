//! [`DagEdge`] and [`EdgeKind`] — dependency arcs.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// EdgeKind
// ---------------------------------------------------------------------------

/// The relationship kind encoded by a directed edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    /// The source symbol calls the target (function call or method dispatch).
    Calls,
    /// The source type implements the target trait.
    Implements,
    /// The source symbol references the target type / value.
    Uses,
    /// The source module/struct contains the target symbol.
    Contains,
}

// ---------------------------------------------------------------------------
// DagEdge
// ---------------------------------------------------------------------------

/// A directed arc in the code dependency graph.
///
/// All fields are private; use the accessor methods to read them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DagEdge {
    /// Fully-qualified id of the source symbol.
    from: String,
    /// Fully-qualified id of the target symbol.
    to: String,
    /// The relationship kind.
    kind: EdgeKind,
}

impl DagEdge {
    /// Construct a new [`DagEdge`].
    ///
    /// # Arguments
    ///
    /// * `from` — Fully-qualified id of the source symbol.
    /// * `to`   — Fully-qualified id of the target symbol.
    /// * `kind` — The relationship kind.
    #[must_use]
    pub fn new(from: String, to: String, kind: EdgeKind) -> Self {
        Self { from, to, kind }
    }

    /// Fully-qualified id of the source symbol.
    #[must_use]
    pub fn from(&self) -> &str {
        &self.from
    }

    /// Fully-qualified id of the target symbol.
    #[must_use]
    pub fn to(&self) -> &str {
        &self.to
    }

    /// The relationship kind.
    #[must_use]
    pub fn kind(&self) -> &EdgeKind {
        &self.kind
    }
}
