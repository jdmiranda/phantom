//! Ticket-driven instability overlay for DAG nodes.
//!
//! Each [`NodeOverlay`] carries an instability score (0.0 = stable, 1.0 = highly
//! unstable) and the list of open GitHub issue numbers linked to that node.
//! The [`OverlayIndex`] is a thin `HashMap` alias used throughout the
//! `phantom-app` rendering layer.

use std::collections::HashMap;

// ---------------------------------------------------------------------------
// NodeOverlay
// ---------------------------------------------------------------------------

/// Per-node instability metadata derived from open tickets.
///
/// `instability_score` is a normalised float in `[0.0, 1.0]`:
/// - `0.0` — no open tickets; node is considered stable.
/// - `1.0` — maximum instability; many open tickets linked.
#[derive(Debug, Clone, Default)]
pub struct NodeOverlay {
    /// Normalised instability score in `[0.0, 1.0]`.
    pub instability_score: f32,
    /// Open GitHub issue numbers linked to this node.
    pub open_tickets: Vec<u64>,
}

// ---------------------------------------------------------------------------
// OverlayIndex
// ---------------------------------------------------------------------------

/// Map from fully-qualified node id to its [`NodeOverlay`].
pub type OverlayIndex = HashMap<String, NodeOverlay>;
