//! Taffy-based flexbox layout engine for positioning terminal panes,
//! tab bar, and status bar within the Phantom window.

use anyhow::{Context, Result};
use taffy::prelude::*;

/// Logical height of the theme strip (top-of-window chrome) in points.
///
/// Matches the mockup's `.controls` row but with terminal-chrome density.
const THEME_STRIP_HEIGHT_LOGICAL: f32 = 28.0;

/// Logical height of the launcher bar in points (before DPI scaling).
///
/// Mirrors `phantom_ui::widgets::APP_LAUNCHER_BAR_HEIGHT`. Duplicated here
/// to keep `layout` from depending on `widgets` (mutual import would create
/// a cycle in the `phantom-ui` crate root); the widget module's constant is
/// the source of truth and any future change should bump both.
const APP_LAUNCHER_BAR_HEIGHT_LOGICAL: f32 = 48.0;

/// Logical height of the tab bar in points (before DPI scaling).
const TAB_BAR_HEIGHT_LOGICAL: f32 = 30.0;

/// Logical height of the status bar in points (before DPI scaling).
const STATUS_BAR_HEIGHT_LOGICAL: f32 = 28.0;

/// Maximum allowed nesting depth for split panes.
///
/// Attempting to split a pane that would push any leaf beyond this depth
/// returns [`LayoutError::MaxDepthExceeded`] so the caller can surface a
/// notification instead of allowing unbounded recursion.
const MAX_SPLIT_DEPTH: usize = 20;

/// Errors that can be returned by the layout engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutError {
    /// A split was requested but the target pane is already at the maximum
    /// allowed nesting depth. Show a notification to the user instead of
    /// creating an unboundedly deep tree.
    MaxDepthExceeded,
}

impl std::fmt::Display for LayoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MaxDepthExceeded => write!(f, "maximum split depth ({MAX_SPLIT_DEPTH}) exceeded"),
        }
    }
}

impl std::error::Error for LayoutError {}

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
    pub const ZERO: Self = Self {
        x: 0.0,
        y: 0.0,
        width: 0.0,
        height: 0.0,
    };
}

/// Opaque handle to a terminal pane within the layout tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PaneId(NodeId);

impl PaneId {
    /// Returns the underlying taffy `NodeId`.
    #[must_use]
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
/// |   App Launcher (48px)     |
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
    /// Top-of-window app launcher bar with discoverable chrome pane chips.
    /// Sits at the very top of the window above the theme strip.
    launcher_bar: NodeId,
    /// Top-of-window theme picker + CRT toggle strip (matches the mockup
    /// `.controls` row). Sits above the tab bar.
    theme_strip: NodeId,
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
        Self::with_scale(1.0)
    }

    /// Create a new layout engine with DPI scale factor applied to chrome heights.
    pub fn with_scale(scale: f32) -> Result<Self> {
        let theme_h = THEME_STRIP_HEIGHT_LOGICAL * scale;
        let launcher_h = APP_LAUNCHER_BAR_HEIGHT_LOGICAL * scale;
        let tab_h = TAB_BAR_HEIGHT_LOGICAL * scale;
        let status_h = STATUS_BAR_HEIGHT_LOGICAL * scale;

        let mut tree = TaffyTree::new();

        let launcher_bar = tree
            .new_leaf(Style {
                size: Size {
                    width: Dimension::Auto,
                    height: Dimension::Length(launcher_h),
                },
                flex_shrink: 0.0,
                ..Style::default()
            })
            .context("failed to create launcher_bar node")?;

        let theme_strip = tree
            .new_leaf(Style {
                size: Size {
                    width: Dimension::Auto,
                    height: Dimension::Length(theme_h),
                },
                flex_shrink: 0.0,
                ..Style::default()
            })
            .context("failed to create theme_strip node")?;

        let tab_bar = tree
            .new_leaf(Style {
                size: Size {
                    width: Dimension::Auto,
                    height: Dimension::Length(tab_h),
                },
                flex_shrink: 0.0,
                ..Style::default()
            })
            .context("failed to create tab_bar node")?;

        let content = tree
            .new_leaf(Style {
                flex_grow: 1.0,
                flex_shrink: 1.0,
                size: Size {
                    width: Dimension::Auto,
                    height: Dimension::Auto,
                },
                ..Style::default()
            })
            .context("failed to create content node")?;

        let status_bar = tree
            .new_leaf(Style {
                size: Size {
                    width: Dimension::Auto,
                    height: Dimension::Length(status_h),
                },
                flex_shrink: 0.0,
                ..Style::default()
            })
            .context("failed to create status_bar node")?;

        let bottom_pad = 8.0 * scale;

        let root = tree
            .new_with_children(
                Style {
                    display: Display::Flex,
                    padding: taffy::geometry::Rect {
                        left: LengthPercentage::Length(0.0),
                        right: LengthPercentage::Length(0.0),
                        top: LengthPercentage::Length(0.0),
                        bottom: LengthPercentage::Length(bottom_pad),
                    },
                    flex_direction: FlexDirection::Column,
                    size: Size {
                        width: Dimension::Percent(1.0),
                        height: Dimension::Percent(1.0),
                    },
                    ..Style::default()
                },
                &[launcher_bar, theme_strip, tab_bar, content, status_bar],
            )
            .context("failed to create root node")?;

        Ok(Self {
            tree,
            root,
            launcher_bar,
            theme_strip,
            tab_bar,
            content,
            status_bar,
        })
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
        let node = self
            .tree
            .new_leaf(Style {
                flex_grow: 1.0,
                flex_shrink: 1.0,
                size: Size {
                    width: Dimension::Auto,
                    height: Dimension::Percent(1.0),
                },
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
    /// If the pane is a split container (i.e. it has children from a prior
    /// split), the entire sub-tree is removed.
    ///
    /// After removing the target node this method also prunes any ancestor
    /// split-container nodes that are now empty. A split container is created
    /// by [`split_horizontal`](Self::split_horizontal) or
    /// [`split_vertical`](Self::split_vertical): the original pane node is
    /// promoted to a flex container and two child leaves are added. If both
    /// children are subsequently removed the container itself becomes an
    /// orphaned node that would otherwise remain in the Taffy tree forever.
    /// This pruning step prevents that leak.
    pub fn remove_pane(&mut self, id: PaneId) -> Result<()> {
        // Record the parent *before* removing the subtree so we can walk
        // upward afterward and prune any now-empty container ancestors.
        let parent_before = self.tree.parent(id.0);

        self.remove_subtree(id.0)?;

        // Prune empty non-chrome ancestors (split containers left behind
        // when both halves of a split have been closed).
        self.prune_empty_containers(parent_before)?;

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

    /// Set the flex_grow weight of a pane (controls how much space it gets).
    pub fn set_flex_grow(&mut self, pane: PaneId, grow: f32) -> Result<()> {
        let mut style = self
            .tree
            .style(pane.0)
            .map_err(|e| anyhow::anyhow!("cannot read style: {e}"))?
            .clone();
        style.flex_grow = grow;
        self.tree
            .set_style(pane.0, style)
            .map_err(|e| anyhow::anyhow!("cannot set style: {e}"))
    }

    /// Get the computed pixel rectangle for a pane.
    pub fn get_pane_rect(&self, id: PaneId) -> Result<Rect> {
        self.absolute_rect(id.0)
    }

    /// Get the computed pixel rectangle for the top-of-window theme strip.
    pub fn get_theme_strip_rect(&self) -> Result<Rect> {
        self.absolute_rect(self.theme_strip)
    }

    /// Get the computed pixel rectangle for the app launcher bar (above tabs).
    pub fn get_launcher_bar_rect(&self) -> Result<Rect> {
        self.absolute_rect(self.launcher_bar)
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
    #[must_use]
    pub fn pane_count(&self) -> usize {
        self.tree.child_count(self.content)
    }

    /// Set `min_size` and `max_size` constraints on an existing pane node.
    ///
    /// Both dimensions are expressed in pixels and are mapped to Taffy's
    /// `min_size` / `max_size` style fields. Pass `None` for either bound
    /// to clear that constraint (resets to `auto`).
    ///
    /// Returns an error if the `PaneId` is not found in the tree.
    pub fn set_pane_size_constraints(
        &mut self,
        pane: PaneId,
        min_size: Option<(f32, f32)>,
        max_size: Option<(f32, f32)>,
    ) -> Result<()> {
        let mut style = self
            .tree
            .style(pane.0)
            .map_err(|e| anyhow::anyhow!("set_pane_size_constraints: cannot read style: {e}"))?
            .clone();

        style.min_size = match min_size {
            Some((w, h)) => Size {
                width: Dimension::Length(w),
                height: Dimension::Length(h),
            },
            None => Size {
                width: Dimension::Auto,
                height: Dimension::Auto,
            },
        };

        style.max_size = match max_size {
            Some((w, h)) => Size {
                width: Dimension::Length(w),
                height: Dimension::Length(h),
            },
            None => Size {
                width: Dimension::Auto,
                height: Dimension::Auto,
            },
        };

        self.tree
            .set_style(pane.0, style)
            .map_err(|e| anyhow::anyhow!("set_pane_size_constraints: cannot set style: {e}"))
    }

    /// Return the total number of nodes in the underlying Taffy tree.
    ///
    /// This includes the fixed chrome nodes (root, tab bar, content area,
    /// status bar) as well as all live pane nodes. Use this to assert that
    /// spawn-close cycles do not permanently grow the tree.
    #[cfg(any(test, feature = "test-utils"))]
    #[must_use]
    pub fn total_node_count(&self) -> usize {
        self.tree.total_node_count()
    }

    /// Return the root `NodeId` (useful for debugging / printing).
    #[must_use]
    pub fn root(&self) -> NodeId {
        self.root
    }

    // ----------------------------------------------------------------
    // Internal helpers
    // ----------------------------------------------------------------

    /// Walk up the Taffy parent chain from `node` and return the depth (number
    /// of ancestors) not counting `node` itself.  Returns 0 when `node` has no
    /// parent (i.e. it is the root).
    fn node_depth(&self, node: NodeId) -> usize {
        let mut depth = 0usize;
        let mut current = node;
        while let Some(parent) = self.tree.parent(current) {
            depth += 1;
            current = parent;
        }
        depth
    }

    /// Perform a split on an existing pane, converting it into a flex container
    /// in the given direction with two equally-sized children.
    ///
    /// Returns [`LayoutError::MaxDepthExceeded`] (wrapped in an `anyhow` error)
    /// if the pane is already at [`MAX_SPLIT_DEPTH`] and creating children would
    /// push them past the limit.
    fn split(&mut self, pane: PaneId, direction: FlexDirection) -> Result<(PaneId, PaneId)> {
        let pane_node = pane.0;

        // Guard: children of `pane` will sit one level deeper than `pane`.
        let depth = self.node_depth(pane_node);
        if depth >= MAX_SPLIT_DEPTH {
            return Err(anyhow::Error::new(LayoutError::MaxDepthExceeded));
        }

        // Create the two child panes that will live inside the split container.
        let child_style = Style {
            flex_grow: 1.0,
            flex_shrink: 1.0,
            size: Size {
                width: Dimension::Auto,
                height: Dimension::Auto,
            },
            ..Style::default()
        };

        let left = self
            .tree
            .new_leaf(child_style.clone())
            .context("failed to create left split pane")?;
        let right = self
            .tree
            .new_leaf(child_style)
            .context("failed to create right split pane")?;

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
                    size: Size {
                        width: Dimension::Auto,
                        height: Dimension::Auto,
                    },
                    ..Style::default()
                },
            )
            .context("failed to restyle pane as split container")?;

        self.tree
            .add_child(pane_node, left)
            .context("failed to add left child")?;
        self.tree
            .add_child(pane_node, right)
            .context("failed to add right child")?;

        Ok((PaneId(left), PaneId(right)))
    }

    /// Iteratively remove a node and all its descendants from the tree.
    ///
    /// Uses an explicit stack instead of recursion so that a deeply nested
    /// split tree (up to [`MAX_SPLIT_DEPTH`] levels) cannot overflow the call
    /// stack.  Nodes are removed in a post-order fashion: each node's children
    /// are enqueued before the node itself so the Taffy tree remains
    /// structurally valid throughout.
    fn remove_subtree(&mut self, node: NodeId) -> Result<()> {
        // Build the full visit order iteratively (post-order: children first).
        let mut to_visit = vec![node];
        let mut removal_order: Vec<NodeId> = Vec::new();

        while let Some(current) = to_visit.pop() {
            removal_order.push(current);
            let children = self.tree.children(current).unwrap_or_default();
            to_visit.extend(children);
        }

        // Remove in reverse order so leaves are removed before their parents.
        for id in removal_order.into_iter().rev() {
            self.tree
                .remove(id)
                .map_err(|e| anyhow::anyhow!("failed to remove node: {e}"))?;
        }

        Ok(())
    }

    /// Walk up the ancestor chain from `start` and remove any non-chrome
    /// containers that have become empty after a child was removed.
    ///
    /// A split operation promotes an existing pane node to a flex container
    /// and adds two leaf children. If both children are later removed the
    /// container node would otherwise be orphaned in the Taffy tree. This
    /// method prunes those empty containers bottom-up so the tree stays
    /// compact across many split-close cycles.
    ///
    /// Chrome nodes (root, tab_bar, content, status_bar) are never pruned —
    /// they must remain even when empty to preserve the chrome structure.
    fn prune_empty_containers(&mut self, start: Option<NodeId>) -> Result<()> {
        let chrome = [
            self.root,
            self.launcher_bar,
            self.theme_strip,
            self.tab_bar,
            self.content,
            self.status_bar,
        ];
        let mut cursor = start;
        while let Some(node) = cursor {
            // Never prune fixed chrome nodes.
            if chrome.contains(&node) {
                break;
            }
            // Only prune if the node is now truly empty.
            if self.tree.child_count(node) > 0 {
                break;
            }
            // Record the grandparent before removing so we can continue upward.
            let grandparent = self.tree.parent(node);
            self.tree.remove(node).map_err(|e| {
                anyhow::anyhow!("prune_empty_containers: failed to remove orphaned container: {e}")
            })?;
            cursor = grandparent;
        }
        Ok(())
    }

    /// Compute the absolute pixel rectangle for a node by walking up the
    /// parent chain and accumulating offsets.
    fn absolute_rect(&self, node: NodeId) -> Result<Rect> {
        let layout = self
            .tree
            .layout(node)
            .map_err(|e| anyhow::anyhow!("layout query failed: {e}"))?;

        let mut x = layout.location.x;
        let mut y = layout.location.y;

        // Walk ancestors to accumulate absolute position.
        let mut current = node;
        while let Some(parent) = self.tree.parent(current) {
            let parent_layout = self
                .tree
                .layout(parent)
                .map_err(|e| anyhow::anyhow!("parent layout query failed: {e}"))?;
            x += parent_layout.location.x;
            y += parent_layout.location.y;
            current = parent;
        }

        Ok(Rect {
            x,
            y,
            width: layout.size.width,
            height: layout.size.height,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW_W: f32 = 1920.0;
    const WINDOW_H: f32 = 1080.0;
    const EPSILON: f32 = 1.0; // rounding tolerance
    const BOTTOM_PAD: f32 = 8.0; // matches root padding at scale=1.0

    fn approx_eq(a: f32, b: f32) -> bool {
        (a - b).abs() < EPSILON
    }

    #[test]
    fn chrome_regions_fill_window() {
        let mut engine = LayoutEngine::new().unwrap();
        engine.resize(WINDOW_W, WINDOW_H).unwrap();

        let launcher = engine.get_launcher_bar_rect().unwrap();
        let theme = engine.get_theme_strip_rect().unwrap();
        let tab = engine.get_tab_bar_rect().unwrap();
        let status = engine.get_status_bar_rect().unwrap();

        // Launcher pinned to the very top.
        assert!(
            approx_eq(launcher.y, 0.0),
            "launcher bar should start at top: got {}",
            launcher.y
        );
        assert!(
            approx_eq(launcher.height, APP_LAUNCHER_BAR_HEIGHT_LOGICAL),
            "launcher bar height: got {}",
            launcher.height
        );

        // Theme strip sits directly beneath the launcher.
        assert!(
            approx_eq(theme.y, APP_LAUNCHER_BAR_HEIGHT_LOGICAL),
            "theme strip should sit beneath launcher: got {}",
            theme.y
        );
        assert!(
            approx_eq(theme.height, THEME_STRIP_HEIGHT_LOGICAL),
            "theme strip height: got {}",
            theme.height
        );

        // Tab strip sits beneath the theme strip.
        assert!(
            approx_eq(
                tab.y,
                APP_LAUNCHER_BAR_HEIGHT_LOGICAL + THEME_STRIP_HEIGHT_LOGICAL
            ),
            "tab bar should sit beneath theme strip: got y={}",
            tab.y
        );
        assert!(
            approx_eq(tab.height, TAB_BAR_HEIGHT_LOGICAL),
            "tab bar height: got {}",
            tab.height
        );
        assert!(
            approx_eq(tab.width, WINDOW_W),
            "tab bar width: got {}",
            tab.width
        );

        assert!(
            approx_eq(status.y + status.height + BOTTOM_PAD, WINDOW_H),
            "status bar should end at bottom minus padding: got y={} h={}",
            status.y,
            status.height,
        );
        assert!(
            approx_eq(status.height, STATUS_BAR_HEIGHT_LOGICAL),
            "status bar height: got {}",
            status.height
        );
    }

    #[test]
    fn single_pane_fills_content_area() {
        let mut engine = LayoutEngine::new().unwrap();
        let pane = engine.add_pane().unwrap();
        engine.resize(WINDOW_W, WINDOW_H).unwrap();

        let rect = engine.get_pane_rect(pane).unwrap();
        let expected_height = WINDOW_H
            - APP_LAUNCHER_BAR_HEIGHT_LOGICAL
            - THEME_STRIP_HEIGHT_LOGICAL
            - TAB_BAR_HEIGHT_LOGICAL
            - STATUS_BAR_HEIGHT_LOGICAL
            - BOTTOM_PAD;

        // Pane must clear the launcher, theme strip and tab bar.
        assert!(
            approx_eq(
                rect.y,
                APP_LAUNCHER_BAR_HEIGHT_LOGICAL
                    + THEME_STRIP_HEIGHT_LOGICAL
                    + TAB_BAR_HEIGHT_LOGICAL
            ),
            "pane y: got {}",
            rect.y
        );
        assert!(
            approx_eq(rect.height, expected_height),
            "pane height: got {}",
            rect.height
        );
        assert!(
            approx_eq(rect.width, WINDOW_W),
            "pane width: got {}",
            rect.width
        );
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
        assert!(
            approx_eq(r1.width + r2.width, WINDOW_W),
            "widths should sum to window"
        );
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
        assert!(
            r_existing.width > 0.0,
            "existing pane should have positive width"
        );
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
        assert!(
            r_existing.width > 0.0,
            "existing pane should have positive width"
        );
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

    // ── Layout memory-leak regression (Issue #15) ──────────────────────────
    //
    // When `split_horizontal/vertical` promotes a pane node to a flex
    // container and adds two child leaves, removing both children must also
    // remove the now-empty container node. Otherwise every split+close cycle
    // permanently grows the Taffy tree by one orphaned container node.

    /// Splitting a pane and then closing both halves must leave the node
    /// count at the chrome-only baseline (no orphaned container node).
    ///
    /// Arrange: record the chrome-only baseline, add a pane, split it.
    /// Act:     remove both split children (left then right).
    /// Assert:  total node count returns to the chrome-only baseline
    ///          (the original pane node is the split container and must
    ///           also be pruned when it becomes empty, so we end up with
    ///           just the 4 chrome nodes again).
    #[test]
    fn taffy_node_count_stable_after_split_then_close_both_halves() {
        let mut engine = LayoutEngine::new().unwrap();
        engine.resize(WINDOW_W, WINDOW_H).unwrap();

        // Baseline: chrome nodes only (root + launcher_bar + tab_bar + content + status_bar = 5).
        let chrome_baseline = engine.total_node_count();

        let pane = engine.add_pane().unwrap();
        engine.resize(WINDOW_W, WINDOW_H).unwrap();

        // Split promotes `pane` to a container and creates 2 leaf children.
        let (left, right) = engine.split_horizontal(pane).unwrap();
        engine.resize(WINDOW_W, WINDOW_H).unwrap();

        // Close the right half — the container still has one child.
        engine.remove_pane(right).unwrap();

        // Close the left half — container is now empty and the pruner must
        // also remove it, returning us to the chrome-only baseline.
        engine.remove_pane(left).unwrap();
        engine.resize(WINDOW_W, WINDOW_H).unwrap();

        let after = engine.total_node_count();
        assert_eq!(
            after, chrome_baseline,
            "orphaned split container leaked: expected {chrome_baseline} nodes, got {after}",
        );
    }

    /// 1 000 split-then-close cycles must not grow the Taffy tree.
    #[test]
    fn taffy_node_count_stable_across_1000_split_close_cycles() {
        let mut engine = LayoutEngine::new().unwrap();
        let _pane = engine.add_pane().unwrap();
        engine.resize(WINDOW_W, WINDOW_H).unwrap();

        // Baseline: chrome (5) + 1 pane = 6.
        let baseline = engine.total_node_count();

        for cycle in 0..1_000 {
            // Each add_pane + split + close-both-halves must be net-zero.
            let p = engine.add_pane().unwrap();
            let (l, r) = engine.split_horizontal(p).unwrap();
            engine.remove_pane(r).unwrap();
            engine.remove_pane(l).unwrap();

            let after = engine.total_node_count();
            assert_eq!(
                after, baseline,
                "cycle {cycle}: node count grew from {baseline} to {after}",
            );
        }
    }

    /// Nested splits (split a leaf that is itself a split child) must not leak
    /// intermediate container nodes when all three leaves are closed.
    ///
    /// Arrange:
    ///   1. `add_pane` → 1 leaf (A)
    ///   2. `split_horizontal(A)` → container A promoted, leaves L and R
    ///   3. `split_vertical(L)` → L promoted to container, leaves T and B
    ///
    ///   Result: 3 leaves (T, B, R) and 2 container nodes (A, L)
    ///
    /// Act: close all three leaves (T, B, R).
    ///
    /// Assert: `total_node_count()` returns to the chrome-only baseline.
    ///   `prune_empty_containers` must walk the full ancestor chain — after
    ///   removing T the inner container (L) becomes empty and must be pruned,
    ///   which then makes the outer container (A) empty so it too must be
    ///   pruned. A single-level walk would miss the second prune step.
    #[test]
    fn taffy_node_count_stable_after_nested_split_then_close_all() {
        let mut engine = LayoutEngine::new().unwrap();
        engine.resize(WINDOW_W, WINDOW_H).unwrap();

        // Baseline: chrome nodes only (root + launcher_bar + tab_bar + content + status_bar = 5).
        let chrome_baseline = engine.total_node_count();

        // Step 1: single leaf A added to the content area.
        let a = engine.add_pane().unwrap();

        // Step 2: split A horizontally → A becomes a row container, L and R are leaves.
        let (l, r) = engine.split_horizontal(a).unwrap();

        // Step 3: split L vertically → L becomes a column container, T and B are leaves.
        // Tree is now: content → [A(container) → [L(container) → [T, B], R]]
        let (t, b) = engine.split_vertical(l).unwrap();

        engine.resize(WINDOW_W, WINDOW_H).unwrap();

        // All three leaves must have positive area.
        assert!(
            engine.get_pane_rect(t).unwrap().height > 0.0,
            "T must have positive height"
        );
        assert!(
            engine.get_pane_rect(b).unwrap().height > 0.0,
            "B must have positive height"
        );
        assert!(
            engine.get_pane_rect(r).unwrap().width > 0.0,
            "R must have positive width"
        );

        // Close T: L still has B, nothing pruned yet.
        engine.remove_pane(t).unwrap();

        // Close B: L is now empty → L is pruned; A still has R, not pruned.
        engine.remove_pane(b).unwrap();

        // Close R: A is now empty → A is pruned; content is a chrome node, stop.
        engine.remove_pane(r).unwrap();

        engine.resize(WINDOW_W, WINDOW_H).unwrap();

        let after = engine.total_node_count();
        assert_eq!(
            after, chrome_baseline,
            "nested split leaked container nodes: expected {chrome_baseline} nodes, got {after}",
        );
    }

    // ── set_pane_size_constraints ─────────────────────────────────────────────

    /// Setting min/max constraints on a pane node must be reflected in the
    /// Taffy style after the call and survive a subsequent layout computation.
    #[test]
    fn set_pane_size_constraints_updates_taffy_style() {
        let mut engine = LayoutEngine::new().unwrap();
        let pane = engine.add_pane().unwrap();

        // Apply a concrete min/max constraint.
        engine
            .set_pane_size_constraints(pane, Some((200.0, 100.0)), Some((400.0, 300.0)))
            .expect("set_pane_size_constraints should not fail for a valid pane");

        // Taffy must reflect the constraint after recompute.
        engine.resize(WINDOW_W, WINDOW_H).unwrap();
        let rect = engine.get_pane_rect(pane).unwrap();

        // The pane has both a min and a max set to the same value; Taffy
        // should clamp the allocation to the max (400 wide, 300 tall) at most.
        assert!(
            rect.width <= 400.0 + EPSILON,
            "width {} must not exceed max 400",
            rect.width,
        );
        assert!(
            rect.height <= 300.0 + EPSILON,
            "height {} must not exceed max 300",
            rect.height,
        );
    }

    /// Clearing constraints (passing `None`) must restore default auto sizing.
    #[test]
    fn set_pane_size_constraints_clear_restores_auto() {
        let mut engine = LayoutEngine::new().unwrap();
        let pane = engine.add_pane().unwrap();

        // Constrain, then clear.
        engine
            .set_pane_size_constraints(pane, Some((50.0, 50.0)), Some((50.0, 50.0)))
            .unwrap();
        engine.set_pane_size_constraints(pane, None, None).unwrap();

        // After clearing, the pane should fill the available space again.
        engine.resize(WINDOW_W, WINDOW_H).unwrap();
        let rect = engine.get_pane_rect(pane).unwrap();
        let expected_height = WINDOW_H
            - APP_LAUNCHER_BAR_HEIGHT_LOGICAL
            - THEME_STRIP_HEIGHT_LOGICAL
            - TAB_BAR_HEIGHT_LOGICAL
            - STATUS_BAR_HEIGHT_LOGICAL
            - BOTTOM_PAD;

        assert!(
            approx_eq(rect.width, WINDOW_W),
            "cleared pane should fill full width; got {}",
            rect.width,
        );
        assert!(
            approx_eq(rect.height, expected_height),
            "cleared pane should fill content height; got {}",
            rect.height,
        );
    }

    // ── Bug-fix: MAX_SPLIT_DEPTH guard ────────────────────────────────────────

    /// Splitting a pane at the maximum allowed nesting depth must return
    /// `LayoutError::MaxDepthExceeded` instead of creating a deeper tree.
    #[test]
    fn split_at_max_depth_returns_error() {
        let mut engine = LayoutEngine::new().unwrap();

        // Build a chain of single-child splits up to the depth limit.
        // Each split_horizontal call promotes the current leaf and returns
        // two children; we keep the left child and recurse into it.
        let mut current = engine.add_pane().unwrap();
        engine.resize(WINDOW_W, WINDOW_H).unwrap();

        // Drive the nesting to MAX_SPLIT_DEPTH.
        //
        // `add_pane` places the leaf at absolute depth 2 (root → content →
        // pane).  Each `split_horizontal` promotes the current leaf and returns
        // children one level deeper.  The guard fires when the current node's
        // depth reaches MAX_SPLIT_DEPTH (≥ 20).  Starting from depth 2, we
        // need MAX_SPLIT_DEPTH − 2 successful splits before hitting the limit.
        for _ in 0..(MAX_SPLIT_DEPTH - 2) {
            let (left, _right) = engine.split_horizontal(current).unwrap();
            current = left;
        }

        // The next split must be rejected — `current` is now at depth MAX_SPLIT_DEPTH.
        let result = engine.split_horizontal(current);
        assert!(
            result.is_err(),
            "split beyond MAX_SPLIT_DEPTH must return an error"
        );
        let err = result.unwrap_err();
        assert!(
            err.downcast_ref::<LayoutError>()
                .is_some_and(|e| *e == LayoutError::MaxDepthExceeded),
            "error must be LayoutError::MaxDepthExceeded, got: {err}"
        );
    }

    // ── Bug-fix: iterative remove_subtree ─────────────────────────────────────

    /// Removing a deeply nested split subtree must succeed without stack
    /// overflow.  We build a MAX_SPLIT_DEPTH-deep chain and then close the
    /// deepest leaf, which triggers `remove_subtree` on the entire sub-chain.
    #[test]
    fn remove_subtree_iterative_no_stack_overflow() {
        let mut engine = LayoutEngine::new().unwrap();
        let baseline = engine.total_node_count();

        let mut current = engine.add_pane().unwrap();
        engine.resize(WINDOW_W, WINDOW_H).unwrap();

        // Build a chain split as deep as allowed.
        // Same depth math as above: MAX_SPLIT_DEPTH − 2 successful splits.
        let mut right_leaves: Vec<PaneId> = Vec::new();
        for _ in 0..(MAX_SPLIT_DEPTH - 2) {
            let (left, right) = engine.split_horizontal(current).unwrap();
            right_leaves.push(right);
            current = left;
        }

        // Close the right leaves from deepest to shallowest, then the deepest left leaf.
        for r in right_leaves.into_iter().rev() {
            engine.remove_pane(r).unwrap();
        }
        engine.remove_pane(current).unwrap();

        engine.resize(WINDOW_W, WINDOW_H).unwrap();

        // All nodes must be pruned; tree returns to chrome-only baseline.
        assert_eq!(
            engine.total_node_count(),
            baseline,
            "deep split chain must not leak nodes"
        );
    }
}
