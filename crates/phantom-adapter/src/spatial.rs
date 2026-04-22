//! Spatial preferences and negotiation types.
//!
//! Apps declare `SpatialPreference` to request layout space. The layout
//! arbiter (outside this crate) resolves conflicts using priority ordering
//! and constraint-based negotiation.

use serde::{Deserialize, Serialize};

/// An app's spatial requirements and hints.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpatialPreference {
    /// Minimum size in (cols, rows). Below this the app cannot function.
    pub min_size: (u32, u32),
    /// Preferred size in (cols, rows).
    pub preferred_size: (u32, u32),
    /// Maximum useful size. `None` means unbounded.
    pub max_size: Option<(u32, u32)>,
    /// Desired aspect ratio (width / height). `None` means unconstrained.
    pub aspect_ratio: Option<f32>,
    /// Number of internal panes the app manages itself.
    pub internal_panes: u32,
    /// How those internal panes are arranged.
    pub internal_layout: InternalLayout,
    /// Priority weight for the layout arbiter (higher = first pick).
    pub priority: f32,
}

/// How an app arranges its own internal sub-panes.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum InternalLayout {
    /// Single content area.
    Single,
    /// Vertically stacked regions.
    VerticalStack(u32),
    /// Horizontally stacked regions.
    HorizontalStack(u32),
    /// Grid with (cols, rows).
    Grid(u32, u32),
    /// App manages layout entirely itself.
    Custom,
}

/// Cardinal direction for neighbor resize requests.
#[derive(Debug, Clone, Copy)]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
}

/// Outcome of a resize request from the layout arbiter.
#[derive(Debug, Clone)]
pub enum ResizeResult {
    /// The full requested size was granted.
    Granted { width: f32, height: f32 },
    /// A partial allocation was given.
    Partial { width: f32, height: f32 },
    /// The request was denied.
    Denied { reason: String },
}

/// Outcome of a two-phase spatial negotiation (Wayland-style suggest/ack).
#[derive(Debug, Clone)]
pub enum NegotiationResult {
    /// The app accepted the proposed size.
    Accepted,
    /// The app proposes a different size.
    CounterOffer { width: f32, height: f32 },
    /// The app rejected the proposal.
    Rejected { reason: String },
}

impl SpatialPreference {
    /// Create a minimal preference with just a minimum size.
    /// Preferred size defaults to the minimum, single-pane layout, priority 1.0.
    pub fn simple(min_cols: u32, min_rows: u32) -> Self {
        Self {
            min_size: (min_cols, min_rows),
            preferred_size: (min_cols, min_rows),
            max_size: None,
            aspect_ratio: None,
            internal_panes: 1,
            internal_layout: InternalLayout::Single,
            priority: 1.0,
        }
    }

    /// Set the internal pane count and layout (builder pattern).
    pub fn with_internal(mut self, panes: u32, layout: InternalLayout) -> Self {
        self.internal_panes = panes;
        self.internal_layout = layout;
        self
    }

    /// Set the priority weight (builder pattern).
    pub fn with_priority(mut self, priority: f32) -> Self {
        self.priority = priority;
        self
    }

    /// Returns `true` if the minimum size fits within the given dimensions.
    pub fn fits_in(&self, width: u32, height: u32) -> bool {
        self.min_size.0 <= width && self.min_size.1 <= height
    }
}
