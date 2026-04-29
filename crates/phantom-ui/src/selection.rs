//! Selection primitives for terminal text selection.

/// A rectangle in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PixelRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// Text selection mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionMode {
    /// Character-by-character selection.
    Normal,
    /// Whole-line selection.
    Line,
    /// Rectangular block selection.
    Block,
}

/// A text selection region with a mode and bounding rectangle.
#[derive(Debug, Clone, PartialEq)]
pub struct SelectionRect {
    pub rect: PixelRect,
    pub mode: SelectionMode,
}
