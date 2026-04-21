//! Taffy-based flexbox layout engine for positioning terminal panes,
//! tab bar, and status bar within the Phantom window.

use anyhow::{Context, Result};
use taffy::prelude::*;

/// Fixed height of the tab bar in pixels.
const TAB_BAR_HEIGHT: f32 = 30.0;

/// Fixed height of the status bar in pixels.
const STATUS_BAR_HEIGHT: f32 = 24.0;

/// A rectangle in pixel coordinates, representing the computed position
/// and size of a layout region.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    /// X offset from the left edge of the window.
    pub x: f32,
    /// Y offset from the top edge of the window.
    pub y: f32,
    /// Width in pixels.
    pub width: f32,
    /// Height in pixels.
    pub height: f32,
}

impl Rect {
    /// A zero-sized rect at the origin.
    pub const ZERO: Self = Self { x: 0.0, y: 0.0, width: 0.0, height: 0.0 };
}

/// Opaque handle to a terminal pane within the layout tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PaneId(NodeId);

impl PaneId {
    /// Returns the underlying taffy `NodeId`.
    pub fn node_id(self) -> NodeId {
        self.0
    }
}

/// Flexbox layout engine that manages the spatial arrangement of the tab bar,
/// terminal panes, and status bar within the window.
///
/// The vertical structure is:
/// ```text
/// +---------------------------+
/// |        Tab Bar (30px)     |
/// +---------------------------+
/// |                           |
/// |   Content Area (flex: 1)  |
/// |   [pane] [pane] [pane]    |
/// |                           |
/// +---------------------------+
/// |      Status Bar (24px)    |
/// +---------------------------+
/// ```
pub struct LayoutEngine {
    tree: TaffyTree,
    root: NodeId,
    tab_bar: NodeId,
    content: NodeId,
    status_bar: NodeId,
}

impl LayoutEngine {
    /// Create a new layout engine with the default chrome structure.
    ///
    /// The tree is initialized with a root column container holding
    /// the tab bar, content area, and status bar. No panes are created
    /// -- call `add_pane` to populate the content area.
    pub fn new() -> Result<Self> {
        let mut tree = TaffyTree::new();

        let tab_bar = tree
            .new_leaf(Style {
                size: Size { width: Dimension::Auto, height: Dimension::Length(TAB_BAR_HEIGHT) },
                flex_shrink: 0.0,
                ..Style::default()
            })
            .context("failed to create tab_bar node")?;

        let content = tree
            .new_leaf(Style {
                flex_grow: 1.0,
                flex_shrink: 1.0,
                size: Size { width: Dimension::Auto, height: Dimension::Auto },
                ..Style::default()
            })
            .context("failed to create content node")?;

        let status_bar = tree
            .new_leaf(Style {
                size: Size { width: Dimension::Auto, height: Dimension::Length(STATUS_BAR_HEIGHT) },
                flex_shrink: 0.0,
                ..Style::default()
            })
            .context("failed to create status_bar node")?;

        let root = tree
            .new_with_children(
                Style {
                    display: Display::Flex,
                    flex_direction: FlexDirection::Column,
                    size: Size { width: Dimension::Percent(1.0), height: Dimension::Percent(1.0) },
                    ..Style::default()
                },
                &[tab_bar, content, status_bar],
            )
            .context("failed to create root node")?;

        Ok(Self { tree, root, tab_bar, content, status_bar })
    }

    /// Update the root dimensions and recompute the entire layout.
    pub fn resize(&mut self, width: f32, height: f32) -> Result<()> {
        self.tree
            .compute_layout(
                self.root,
                Size {
                    width: AvailableSpace::Definite(width),
                    height: AvailableSpace::Definite(height),
                },
            )
            .map_err(|e| anyhow::anyhow!("layout computation failed: {e}"))?;
        Ok(())
    }

    /// Add a new terminal pane to the content area.
    ///
    /// The pane is created with `flex_grow: 1.0` so that all panes in the
    /// content area share space equally. Returns a `PaneId` handle that can
    /// be used for splitting, removal, and rect queries.
    pub fn add_pane(&mut self) -> Result<PaneId> {
        let node = self.tree
            .new_leaf(Style {
                flex_grow: 1.0,
                flex_shrink: 1.0,
                size: Size { width: Dimension::Auto, height: Dimension::Percent(1.0) },
                ..Style::default()
            })
            .context("failed to create pane node")?;

        self.tree
            .add_child(self.content, node)
            .context("failed to attach pane to content area")?;

        Ok(PaneId(node))
    }

    /// Remove a terminal pane from the layout tree.
    ///
    /// If the pane is a split container (i.e. it has children from a prior split),
    /// the entire sub-tree is removed.
    pub fn remove_pane(&mut self, id: PaneId) -> Result<()> {
        self.remove_subtree(id.0)?;
        Ok(())
    }

    /// Split an existing pane horizontally (left | right).
    ///
    /// The original pane becomes a row container holding two child panes that
    /// share the space equally. Returns `(existing_child, new_child)` -- the
    /// existing pane's content should migrate to `existing_child`.
    pub fn split_horizontal(&mut self, pane: PaneId) -> Result<(PaneId, PaneId)> {
        self.split(pane, FlexDirection::Row)
    }

    /// Split an existing pane vertically (top / bottom).
    ///
    /// The original pane becomes a column container holding two child panes that
    /// share the space equally. Returns `(existing_child, new_child)` -- the
    /// existing pane's content should migrate to `existing_child`.
    pub fn split_vertical(&mut self, pane: PaneId) -> Result<(PaneId, PaneId)> {
        self.split(pane, FlexDirection::Column)
    }

    /// Get the computed pixel rectangle for a pane.
    pub fn get_pane_rect(&self, id: PaneId) -> Result<Rect> {
        self.absolute_rect(id.0)
    }

    /// Get the computed pixel rectangle for the tab bar.
    pub fn get_tab_bar_rect(&self) -> Result<Rect> {
        self.absolute_rect(self.tab_bar)
    }

    /// Get the computed pixel rectangle for the status bar.
    pub fn get_status_bar_rect(&self) -> Result<Rect> {
        self.absolute_rect(self.status_bar)
    }

    /// Return the number of direct children of the content area.
    pub fn pane_count(&self) -> usize {
        self.tree.child_count(self.content)
    }

    /// Return the root `NodeId` (useful for debugging / printing).
    pub fn root(&self) -> NodeId {
        self.root
    }

    // ----------------------------------------------------------------
    // Internal helpers
    // ----------------------------------------------------------------

    /// Perform a split on an existing pane, converting it into a flex container
    /// in the given direction with two equally-sized children.
    fn split(&mut self, pane: PaneId, direction: FlexDirection) -> Result<(PaneId, PaneId)> {
        let pane_node = pane.0;

        // Create the two child panes that will live inside the split container.
        let child_style = Style {
            flex_grow: 1.0,
            flex_shrink: 1.0,
            size: Size { width: Dimension::Auto, height: Dimension::Auto },
            ..Style::default()
        };

        let left = self.tree.new_leaf(child_style.clone()).context("failed to create left split pane")?;
        let right = self.tree.new_leaf(child_style).context("failed to create right split pane")?;

        // Convert the existing pane node into a flex container by updating its style
        // and attaching the two new children.
        self.tree
            .set_style(
                pane_node,
                Style {
                    display: Display::Flex,
                    flex_direction: direction,
                    flex_grow: 1.0,
                    flex_shrink: 1.0,
                    size: Size { width: Dimension::Auto, height: Dimension::Auto },
                    ..Style::default()
                },
            )
            .context("failed to restyle pane as split container")?;

        self.tree.add_child(pane_node, left).context("failed to add left child")?;
        self.tree.add_child(pane_node, right).context("failed to add right child")?;

        Ok((PaneId(left), PaneId(right)))
    }

    /// Recursively remove a node and all its descendants from the tree.
    fn remove_subtree(&mut self, node: NodeId) -> Result<()> {
        // Collect children first to avoid borrow issues.
        let children: Vec<NodeId> = self.tree.children(node).unwrap_or_default();
        for child in children {
            self.remove_subtree(child)?;
        }
        self.tree.remove(node).map_err(|e| anyhow::anyhow!("failed to remove node: {e}"))?;
        Ok(())
    }

    /// Compute the absolute pixel rectangle for a node by walking up the
    /// parent chain and accumulating offsets.
    fn absolute_rect(&self, node: NodeId) -> Result<Rect> {
        let layout = self.tree.layout(node).map_err(|e| anyhow::anyhow!("layout query failed: {e}"))?;

        let mut x = layout.location.x;
        let mut y = layout.location.y;

        // Walk ancestors to accumulate absolute position.
        let mut current = node;
        while let Some(parent) = self.tree.parent(current) {
            let parent_layout =
                self.tree.layout(parent).map_err(|e| anyhow::anyhow!("parent layout query failed: {e}"))?;
            x += parent_layout.location.x;
            y += parent_layout.location.y;
            current = parent;
        }

        Ok(Rect { x, y, width: layout.size.width, height: layout.size.height })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW_W: f32 = 1920.0;
    const WINDOW_H: f32 = 1080.0;
    const EPSILON: f32 = 1.0; // rounding tolerance

    fn approx_eq(a: f32, b: f32) -> bool {
        (a - b).abs() < EPSILON
    }

    #[test]
    fn chrome_regions_fill_window() {
        let mut engine = LayoutEngine::new().unwrap();
        engine.resize(WINDOW_W, WINDOW_H).unwrap();

        let tab = engine.get_tab_bar_rect().unwrap();
        let status = engine.get_status_bar_rect().unwrap();

        assert!(approx_eq(tab.y, 0.0), "tab bar should start at top: got {}", tab.y);
        assert!(approx_eq(tab.height, TAB_BAR_HEIGHT), "tab bar height: got {}", tab.height);
        assert!(approx_eq(tab.width, WINDOW_W), "tab bar width: got {}", tab.width);

        assert!(
            approx_eq(status.y + status.height, WINDOW_H),
            "status bar should end at bottom: got y={} h={}",
            status.y,
            status.height,
        );
        assert!(approx_eq(status.height, STATUS_BAR_HEIGHT), "status bar height: got {}", status.height);
    }

    #[test]
    fn single_pane_fills_content_area() {
        let mut engine = LayoutEngine::new().unwrap();
        let pane = engine.add_pane().unwrap();
        engine.resize(WINDOW_W, WINDOW_H).unwrap();

        let rect = engine.get_pane_rect(pane).unwrap();
        let expected_height = WINDOW_H - TAB_BAR_HEIGHT - STATUS_BAR_HEIGHT;

        assert!(approx_eq(rect.y, TAB_BAR_HEIGHT), "pane y: got {}", rect.y);
        assert!(approx_eq(rect.height, expected_height), "pane height: got {}", rect.height);
        assert!(approx_eq(rect.width, WINDOW_W), "pane width: got {}", rect.width);
    }

    #[test]
    fn two_panes_share_space() {
        let mut engine = LayoutEngine::new().unwrap();
        let p1 = engine.add_pane().unwrap();
        let p2 = engine.add_pane().unwrap();
        engine.resize(WINDOW_W, WINDOW_H).unwrap();

        let r1 = engine.get_pane_rect(p1).unwrap();
        let r2 = engine.get_pane_rect(p2).unwrap();

        // Both panes should be side-by-side (content defaults to row).
        assert!(approx_eq(r1.width + r2.width, WINDOW_W), "widths should sum to window");
        assert!(approx_eq(r1.width, r2.width), "widths should be equal");
    }

    #[test]
    fn split_horizontal_creates_two_children() {
        let mut engine = LayoutEngine::new().unwrap();
        let pane = engine.add_pane().unwrap();
        let (existing, new_pane) = engine.split_horizontal(pane).unwrap();
        engine.resize(WINDOW_W, WINDOW_H).unwrap();

        let r_existing = engine.get_pane_rect(existing).unwrap();
        let r_new = engine.get_pane_rect(new_pane).unwrap();
        assert!(r_existing.width > 0.0, "existing pane should have positive width");
        assert!(r_new.width > 0.0, "new pane should have positive width");
        assert!(r_new.height > 0.0, "new pane should have positive height");
    }

    #[test]
    fn split_vertical_creates_two_children() {
        let mut engine = LayoutEngine::new().unwrap();
        let pane = engine.add_pane().unwrap();
        let (existing, new_pane) = engine.split_vertical(pane).unwrap();
        engine.resize(WINDOW_W, WINDOW_H).unwrap();

        let r_existing = engine.get_pane_rect(existing).unwrap();
        let r_new = engine.get_pane_rect(new_pane).unwrap();
        assert!(r_existing.width > 0.0, "existing pane should have positive width");
        assert!(r_new.width > 0.0, "new pane should have positive width");
        assert!(r_new.height > 0.0, "new pane should have positive height");
    }

    #[test]
    fn remove_pane_decreases_count() {
        let mut engine = LayoutEngine::new().unwrap();
        let p1 = engine.add_pane().unwrap();
        let _p2 = engine.add_pane().unwrap();
        assert_eq!(engine.pane_count(), 2);

        engine.remove_pane(p1).unwrap();
        assert_eq!(engine.pane_count(), 1);
    }

    #[test]
    fn resize_updates_layout() {
        let mut engine = LayoutEngine::new().unwrap();
        let pane = engine.add_pane().unwrap();

        engine.resize(800.0, 600.0).unwrap();
        let r1 = engine.get_pane_rect(pane).unwrap();

        engine.resize(1600.0, 900.0).unwrap();
        let r2 = engine.get_pane_rect(pane).unwrap();

        assert!(r2.width > r1.width, "pane should be wider after resize");
        assert!(r2.height > r1.height, "pane should be taller after resize");
    }
}
