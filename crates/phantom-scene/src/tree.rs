use std::collections::VecDeque;

use crate::dirty::DirtyFlags;
use crate::node::{NodeId, NodeKind, RenderLayer, SceneNode, Transform, WorldTransform};

/// Arena-backed retained scene tree.
///
/// Nodes are stored in a flat `Vec` indexed by `NodeId`. Removal marks the
/// slot as a tombstone rather than shifting indices, so existing IDs remain
/// stable.
pub struct SceneTree {
    nodes: Vec<SceneNode>,
    root: NodeId,
    next_id: NodeId,
}

impl SceneTree {
    /// Create a new tree with a single `Root` node at index 0.
    pub fn new() -> Self {
        let root_node = SceneNode::new(0, NodeKind::Root);
        Self {
            nodes: vec![root_node],
            root: 0,
            next_id: 1,
        }
    }

    // ── Accessors ───────────────────────────────────────────────────────

    /// Root node ID (always 0).
    pub fn root(&self) -> NodeId {
        self.root
    }

    /// Get a node by ID. Returns `None` for out-of-range or tombstoned slots.
    pub fn get(&self, id: NodeId) -> Option<&SceneNode> {
        self.nodes.get(id as usize).filter(|n| n.alive)
    }

    /// Get a mutable reference to a node by ID.
    pub fn get_mut(&mut self, id: NodeId) -> Option<&mut SceneNode> {
        self.nodes.get_mut(id as usize).filter(|n| n.alive)
    }

    /// Total number of *alive* nodes (excludes tombstones).
    pub fn node_count(&self) -> usize {
        self.nodes.iter().filter(|n| n.alive).count()
    }

    // ── Mutation ────────────────────────────────────────────────────────

    /// Add a new node as a child of `parent`. Returns the new node's ID.
    ///
    /// The node starts with `DirtyFlags::ALL` so it will be picked up by the
    /// first render pass.
    pub fn add_node(&mut self, parent: NodeId, kind: NodeKind) -> NodeId {
        let id = self.next_id;
        self.next_id += 1;

        let mut node = SceneNode::new(id, kind);
        node.parent = Some(parent);

        // Grow the arena if needed (fill gaps with dead nodes).
        while self.nodes.len() <= id as usize {
            let filler_id = self.nodes.len() as NodeId;
            let mut filler = SceneNode::new(filler_id, NodeKind::Root);
            filler.alive = false;
            self.nodes.push(filler);
        }
        self.nodes[id as usize] = node;

        // Register as child of parent.
        if let Some(parent_node) = self.nodes.get_mut(parent as usize) {
            if parent_node.alive {
                parent_node.children.push(id);
            }
        }

        // Propagate CHILDREN dirty flag up.
        self.propagate_children_dirty(parent);

        id
    }

    /// Remove a node and all its descendants (tombstone, don't shift).
    pub fn remove_node(&mut self, id: NodeId) {
        if id == self.root {
            return; // never remove root
        }

        // Collect the entire subtree first.
        let descendants = self.walk_descendants(id);

        // Detach from parent.
        if let Some(node) = self.nodes.get(id as usize) {
            if let Some(parent_id) = node.parent {
                if let Some(parent) = self.nodes.get_mut(parent_id as usize) {
                    parent.children.retain(|&c| c != id);
                }
                self.propagate_children_dirty(parent_id);
            }
        }

        // Tombstone self + all descendants.
        for &desc_id in &descendants {
            if let Some(node) = self.nodes.get_mut(desc_id as usize) {
                node.alive = false;
                node.children.clear();
                node.parent = None;
            }
        }
        // Tombstone self as well.
        if let Some(node) = self.nodes.get_mut(id as usize) {
            node.alive = false;
            node.children.clear();
            node.parent = None;
        }
    }

    // ── Dirty tracking ──────────────────────────────────────────────────

    /// Mark a node with the given dirty flags.
    ///
    /// Automatically propagates `CHILDREN` up the ancestor chain so the
    /// render traversal knows to descend into this subtree.
    pub fn mark_dirty(&mut self, id: NodeId, flags: DirtyFlags) {
        if let Some(node) = self.nodes.get_mut(id as usize) {
            if !node.alive {
                return;
            }
            node.dirty |= flags;
            if let Some(parent_id) = node.parent {
                self.propagate_children_dirty(parent_id);
            }
        }
    }

    /// Clear all dirty flags on a node (call after GPU upload).
    pub fn clear_dirty(&mut self, id: NodeId) {
        if let Some(node) = self.nodes.get_mut(id as usize) {
            node.dirty = DirtyFlags::empty();
        }
    }

    /// Walk upward from `start`, adding `CHILDREN` to every ancestor.
    fn propagate_children_dirty(&mut self, start: NodeId) {
        let mut current = Some(start);
        while let Some(id) = current {
            match self.nodes.get_mut(id as usize) {
                Some(node) if node.alive => {
                    if node.dirty.contains(DirtyFlags::CHILDREN) {
                        // Already propagated — stop early.
                        break;
                    }
                    node.dirty |= DirtyFlags::CHILDREN;
                    current = node.parent;
                }
                _ => break,
            }
        }
    }

    // ── Transform ───────────────────────────────────────────────────────

    /// Update a node's local transform and mark it `TRANSFORM`-dirty.
    pub fn set_transform(&mut self, id: NodeId, x: f32, y: f32, w: f32, h: f32) {
        if let Some(node) = self.get_mut(id) {
            node.transform = Transform { x, y, width: w, height: h };
        }
        self.mark_dirty(id, DirtyFlags::TRANSFORM);
    }

    /// Set a node's visibility and mark it `VISIBILITY`-dirty.
    pub fn set_visible(&mut self, id: NodeId, visible: bool) {
        if let Some(node) = self.get_mut(id) {
            node.visible = visible;
        }
        self.mark_dirty(id, DirtyFlags::VISIBILITY);
    }

    /// Recompute world transforms for all nodes whose `TRANSFORM` flag
    /// (or whose ancestor's `TRANSFORM` flag) is set.
    ///
    /// Performs a top-down BFS starting from root so that parents are
    /// resolved before children.
    pub fn update_world_transforms(&mut self) {
        // BFS order guarantees parent is computed before child.
        let mut queue: VecDeque<NodeId> = VecDeque::new();
        queue.push_back(self.root);

        while let Some(id) = queue.pop_front() {
            let idx = id as usize;
            if idx >= self.nodes.len() || !self.nodes[idx].alive {
                continue;
            }

            // Compute world = parent.world + local.
            let parent_world = self.nodes[idx]
                .parent
                .and_then(|pid| {
                    let p = &self.nodes[pid as usize];
                    if p.alive { Some(p.world_transform) } else { None }
                })
                .unwrap_or(WorldTransform { x: 0.0, y: 0.0, width: 0.0, height: 0.0 });

            let local = self.nodes[idx].transform;
            self.nodes[idx].world_transform = WorldTransform {
                x: parent_world.x + local.x,
                y: parent_world.y + local.y,
                width: local.width,
                height: local.height,
            };

            // Clear the TRANSFORM flag for this node.
            self.nodes[idx].dirty.remove(DirtyFlags::TRANSFORM);

            // Enqueue children.
            let children: Vec<NodeId> = self.nodes[idx].children.clone();
            for child_id in children {
                queue.push_back(child_id);
            }
        }
    }

    // ── Queries ─────────────────────────────────────────────────────────

    /// Return IDs of all nodes that need content re-upload, sorted by z-order.
    pub fn dirty_nodes(&self) -> Vec<NodeId> {
        let mut result: Vec<_> = self
            .nodes
            .iter()
            .filter(|n| n.alive && n.dirty.needs_upload())
            .map(|n| n.id)
            .collect();
        result.sort_by_key(|&id| self.nodes[id as usize].z_order);
        result
    }

    /// Return IDs of all visible nodes in a specific render layer,
    /// sorted by z-order (ascending — lower draws first).
    pub fn visible_nodes(&self, layer: RenderLayer) -> Vec<NodeId> {
        let mut result: Vec<_> = self
            .nodes
            .iter()
            .filter(|n| n.alive && n.visible && n.render_layer == layer)
            .map(|n| n.id)
            .collect();
        result.sort_by_key(|&id| self.nodes[id as usize].z_order);
        result
    }

    /// Walk all descendants of `root_id` in depth-first order.
    /// Does **not** include `root_id` itself.
    pub fn walk_descendants(&self, root_id: NodeId) -> Vec<NodeId> {
        let mut result = Vec::new();
        let mut stack = Vec::new();

        if let Some(node) = self.get(root_id) {
            // Push children in reverse so left-most child is visited first.
            for &child in node.children.iter().rev() {
                stack.push(child);
            }
        }

        while let Some(id) = stack.pop() {
            if let Some(node) = self.get(id) {
                result.push(id);
                for &child in node.children.iter().rev() {
                    stack.push(child);
                }
            }
        }

        result
    }
}

impl Default for SceneTree {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Basic construction ──────────────────────────────────────────

    #[test]
    fn new_tree_has_root() {
        let tree = SceneTree::new();
        assert_eq!(tree.root(), 0);
        assert!(tree.get(0).is_some());
        assert_eq!(tree.get(0).unwrap().kind, NodeKind::Root);
        assert_eq!(tree.node_count(), 1);
    }

    #[test]
    fn add_single_child() {
        let mut tree = SceneTree::new();
        let id = tree.add_node(0, NodeKind::Pane);
        assert_eq!(id, 1);
        assert_eq!(tree.node_count(), 2);

        let node = tree.get(id).unwrap();
        assert_eq!(node.kind, NodeKind::Pane);
        assert_eq!(node.parent, Some(0));

        let root = tree.get(0).unwrap();
        assert!(root.children.contains(&id));
    }

    #[test]
    fn add_multiple_children() {
        let mut tree = SceneTree::new();
        let a = tree.add_node(0, NodeKind::TabBar);
        let b = tree.add_node(0, NodeKind::ContentArea);
        let c = tree.add_node(0, NodeKind::StatusBar);
        assert_eq!(tree.node_count(), 4);

        let root = tree.get(0).unwrap();
        assert_eq!(root.children, vec![a, b, c]);
    }

    #[test]
    fn add_nested_children() {
        let mut tree = SceneTree::new();
        let content = tree.add_node(0, NodeKind::ContentArea);
        let pane = tree.add_node(content, NodeKind::Pane);
        let _img = tree.add_node(pane, NodeKind::Image);

        assert_eq!(tree.node_count(), 4);
        assert_eq!(tree.get(pane).unwrap().parent, Some(content));
    }

    // ── Remove / tombstone ──────────────────────────────────────────

    #[test]
    fn remove_leaf_node() {
        let mut tree = SceneTree::new();
        let id = tree.add_node(0, NodeKind::Pane);
        tree.remove_node(id);

        assert!(tree.get(id).is_none());
        assert_eq!(tree.node_count(), 1);
        assert!(tree.get(0).unwrap().children.is_empty());
    }

    #[test]
    fn remove_subtree() {
        let mut tree = SceneTree::new();
        let content = tree.add_node(0, NodeKind::ContentArea);
        let pane = tree.add_node(content, NodeKind::Pane);
        let _img = tree.add_node(pane, NodeKind::Image);
        assert_eq!(tree.node_count(), 4);

        tree.remove_node(content);
        assert_eq!(tree.node_count(), 1); // only root remains
        assert!(tree.get(content).is_none());
        assert!(tree.get(pane).is_none());
    }

    #[test]
    fn remove_root_is_noop() {
        let mut tree = SceneTree::new();
        tree.remove_node(0);
        assert_eq!(tree.node_count(), 1);
    }

    #[test]
    fn tombstone_ids_are_stable() {
        let mut tree = SceneTree::new();
        let a = tree.add_node(0, NodeKind::Pane);
        let _b = tree.add_node(0, NodeKind::StatusBar);
        tree.remove_node(a);

        // The next node gets a fresh ID, doesn't reuse `a`.
        let c = tree.add_node(0, NodeKind::TabBar);
        assert_ne!(c, a);
        assert!(tree.get(a).is_none()); // tombstone still dead
        assert!(tree.get(c).is_some());
    }

    // ── Dirty propagation ───────────────────────────────────────────

    #[test]
    fn new_node_starts_dirty() {
        let mut tree = SceneTree::new();
        let id = tree.add_node(0, NodeKind::Pane);
        let node = tree.get(id).unwrap();
        assert_eq!(node.dirty, DirtyFlags::ALL);
    }

    #[test]
    fn mark_dirty_sets_flags() {
        let mut tree = SceneTree::new();
        let id = tree.add_node(0, NodeKind::Pane);
        tree.clear_dirty(id);
        assert!(tree.get(id).unwrap().dirty.is_clean());

        tree.mark_dirty(id, DirtyFlags::CONTENT);
        assert!(tree.get(id).unwrap().dirty.needs_upload());
    }

    #[test]
    fn dirty_propagates_children_flag_up() {
        let mut tree = SceneTree::new();
        let content = tree.add_node(0, NodeKind::ContentArea);
        let pane = tree.add_node(content, NodeKind::Pane);

        // Clear everything.
        tree.clear_dirty(0);
        tree.clear_dirty(content);
        tree.clear_dirty(pane);

        // Dirty the leaf.
        tree.mark_dirty(pane, DirtyFlags::CONTENT);

        // Ancestors should have CHILDREN.
        assert!(tree.get(content).unwrap().dirty.contains(DirtyFlags::CHILDREN));
        assert!(tree.get(0).unwrap().dirty.contains(DirtyFlags::CHILDREN));

        // Leaf should NOT have CHILDREN, only CONTENT.
        assert!(!tree.get(pane).unwrap().dirty.contains(DirtyFlags::CHILDREN));
        assert!(tree.get(pane).unwrap().dirty.contains(DirtyFlags::CONTENT));
    }

    #[test]
    fn clear_dirty_resets_flags() {
        let mut tree = SceneTree::new();
        let id = tree.add_node(0, NodeKind::Pane);
        tree.clear_dirty(id);
        assert!(tree.get(id).unwrap().dirty.is_clean());
    }

    // ── Transform ───────────────────────────────────────────────────

    #[test]
    fn set_transform_marks_dirty() {
        let mut tree = SceneTree::new();
        let id = tree.add_node(0, NodeKind::Pane);
        tree.clear_dirty(id);
        tree.clear_dirty(0);

        tree.set_transform(id, 10.0, 20.0, 100.0, 50.0);
        assert!(tree.get(id).unwrap().dirty.needs_layout());
        // Parent should have CHILDREN.
        assert!(tree.get(0).unwrap().dirty.contains(DirtyFlags::CHILDREN));
    }

    #[test]
    fn world_transform_simple() {
        let mut tree = SceneTree::new();
        tree.set_transform(0, 0.0, 0.0, 800.0, 600.0);
        let pane = tree.add_node(0, NodeKind::Pane);
        tree.set_transform(pane, 50.0, 100.0, 300.0, 200.0);

        tree.update_world_transforms();

        let wt = tree.get(pane).unwrap().world_transform;
        assert_eq!(wt.x, 50.0);
        assert_eq!(wt.y, 100.0);
        assert_eq!(wt.width, 300.0);
        assert_eq!(wt.height, 200.0);
    }

    #[test]
    fn world_transform_nested() {
        let mut tree = SceneTree::new();
        tree.set_transform(0, 10.0, 20.0, 800.0, 600.0);

        let content = tree.add_node(0, NodeKind::ContentArea);
        tree.set_transform(content, 5.0, 30.0, 700.0, 500.0);

        let pane = tree.add_node(content, NodeKind::Pane);
        tree.set_transform(pane, 10.0, 10.0, 300.0, 200.0);

        tree.update_world_transforms();

        // root world = (10, 20)
        // content world = (10+5, 20+30) = (15, 50)
        // pane world = (15+10, 50+10) = (25, 60)
        let wt = tree.get(pane).unwrap().world_transform;
        assert_eq!(wt.x, 25.0);
        assert_eq!(wt.y, 60.0);
        assert_eq!(wt.width, 300.0);
        assert_eq!(wt.height, 200.0);
    }

    #[test]
    fn update_world_transforms_clears_transform_flag() {
        let mut tree = SceneTree::new();
        let pane = tree.add_node(0, NodeKind::Pane);
        tree.set_transform(pane, 10.0, 10.0, 100.0, 50.0);
        assert!(tree.get(pane).unwrap().dirty.needs_layout());

        tree.update_world_transforms();
        assert!(!tree.get(pane).unwrap().dirty.needs_layout());
    }

    // ── Visibility ──────────────────────────────────────────────────

    #[test]
    fn set_visible_marks_dirty() {
        let mut tree = SceneTree::new();
        let id = tree.add_node(0, NodeKind::DebugHud);
        tree.clear_dirty(id);
        tree.clear_dirty(0);

        tree.set_visible(id, false);
        assert!(tree.get(id).unwrap().dirty.contains(DirtyFlags::VISIBILITY));
        assert!(!tree.get(id).unwrap().visible);
    }

    #[test]
    fn invisible_nodes_excluded_from_visible_nodes() {
        let mut tree = SceneTree::new();
        let a = tree.add_node(0, NodeKind::Pane);
        let b = tree.add_node(0, NodeKind::Pane);
        tree.set_visible(a, false);

        let visible = tree.visible_nodes(RenderLayer::Scene);
        assert!(!visible.contains(&a));
        assert!(visible.contains(&b));
    }

    // ── Z-order ─────────────────────────────────────────────────────

    #[test]
    fn visible_nodes_sorted_by_z_order() {
        let mut tree = SceneTree::new();

        let high = tree.add_node(0, NodeKind::Pane);
        tree.get_mut(high).unwrap().z_order = 10;

        let low = tree.add_node(0, NodeKind::Pane);
        tree.get_mut(low).unwrap().z_order = 1;

        let mid = tree.add_node(0, NodeKind::Pane);
        tree.get_mut(mid).unwrap().z_order = 5;

        let visible = tree.visible_nodes(RenderLayer::Scene);
        // Root (z=0), low (z=1), mid (z=5), high (z=10)
        let z_orders: Vec<i32> = visible
            .iter()
            .map(|&id| tree.get(id).unwrap().z_order)
            .collect();
        assert_eq!(z_orders, vec![0, 1, 5, 10]);
    }

    #[test]
    fn dirty_nodes_sorted_by_z_order() {
        let mut tree = SceneTree::new();

        let a = tree.add_node(0, NodeKind::Pane);
        tree.get_mut(a).unwrap().z_order = 20;

        let b = tree.add_node(0, NodeKind::Pane);
        tree.get_mut(b).unwrap().z_order = 5;

        // Both start with ALL dirty (includes CONTENT).
        let dirty = tree.dirty_nodes();
        let z_orders: Vec<i32> = dirty
            .iter()
            .map(|&id| tree.get(id).unwrap().z_order)
            .collect();

        // Should be sorted ascending.
        for w in z_orders.windows(2) {
            assert!(w[0] <= w[1]);
        }
    }

    // ── Layer filtering ─────────────────────────────────────────────

    #[test]
    fn visible_nodes_filters_by_layer() {
        let mut tree = SceneTree::new();

        let scene_node = tree.add_node(0, NodeKind::Pane);
        // default layer is Scene

        let overlay_node = tree.add_node(0, NodeKind::DebugHud);
        tree.get_mut(overlay_node).unwrap().render_layer = RenderLayer::Overlay;

        let scene_visible = tree.visible_nodes(RenderLayer::Scene);
        let overlay_visible = tree.visible_nodes(RenderLayer::Overlay);

        assert!(scene_visible.contains(&scene_node));
        assert!(!scene_visible.contains(&overlay_node));

        assert!(overlay_visible.contains(&overlay_node));
        assert!(!overlay_visible.contains(&scene_node));
    }

    // ── Walk descendants ────────────────────────────────────────────

    #[test]
    fn walk_descendants_empty() {
        let tree = SceneTree::new();
        assert!(tree.walk_descendants(0).is_empty());
    }

    #[test]
    fn walk_descendants_depth_first() {
        let mut tree = SceneTree::new();
        let a = tree.add_node(0, NodeKind::ContentArea);
        let b = tree.add_node(a, NodeKind::Pane);
        let c = tree.add_node(a, NodeKind::Pane);
        let d = tree.add_node(b, NodeKind::Image);

        let desc = tree.walk_descendants(0);
        assert_eq!(desc.len(), 4);
        // `a` first, then its subtree before `c`.
        assert_eq!(desc[0], a);
        // `b` before `d` (b is parent of d).
        let b_pos = desc.iter().position(|&x| x == b).unwrap();
        let d_pos = desc.iter().position(|&x| x == d).unwrap();
        assert!(b_pos < d_pos);
        // All IDs present.
        assert!(desc.contains(&c));
    }

    #[test]
    fn walk_descendants_subtree() {
        let mut tree = SceneTree::new();
        let a = tree.add_node(0, NodeKind::ContentArea);
        let b = tree.add_node(a, NodeKind::Pane);
        let _c = tree.add_node(0, NodeKind::StatusBar); // sibling, not descendant of a

        let desc = tree.walk_descendants(a);
        assert_eq!(desc, vec![b]);
    }

    // ── Builder pattern ─────────────────────────────────────────────

    #[test]
    fn node_builder() {
        let node = SceneNode::new(42, NodeKind::CommandBar)
            .with_transform(10.0, 20.0, 300.0, 40.0)
            .with_z_order(100)
            .with_layer(RenderLayer::Overlay);

        assert_eq!(node.transform.x, 10.0);
        assert_eq!(node.transform.y, 20.0);
        assert_eq!(node.transform.width, 300.0);
        assert_eq!(node.transform.height, 40.0);
        assert_eq!(node.z_order, 100);
        assert_eq!(node.render_layer, RenderLayer::Overlay);
    }

    // ── Edge cases ──────────────────────────────────────────────────

    #[test]
    fn get_nonexistent_returns_none() {
        let tree = SceneTree::new();
        assert!(tree.get(999).is_none());
    }

    #[test]
    fn mark_dirty_on_dead_node_is_noop() {
        let mut tree = SceneTree::new();
        let id = tree.add_node(0, NodeKind::Pane);
        tree.remove_node(id);
        // Should not panic.
        tree.mark_dirty(id, DirtyFlags::CONTENT);
    }

    #[test]
    fn remove_middle_of_chain() {
        let mut tree = SceneTree::new();
        let a = tree.add_node(0, NodeKind::ContentArea);
        let b = tree.add_node(a, NodeKind::Pane);
        let c = tree.add_node(b, NodeKind::Image);

        // Remove the middle node — should kill b and c.
        tree.remove_node(b);
        assert!(tree.get(b).is_none());
        assert!(tree.get(c).is_none());
        // `a` should still be alive but with no children.
        assert!(tree.get(a).unwrap().children.is_empty());
        assert_eq!(tree.node_count(), 2); // root + a
    }

    #[test]
    fn dirty_propagation_deep_chain() {
        let mut tree = SceneTree::new();
        let a = tree.add_node(0, NodeKind::ContentArea);
        let b = tree.add_node(a, NodeKind::Pane);
        let c = tree.add_node(b, NodeKind::Image);
        let d = tree.add_node(c, NodeKind::Custom(1));

        // Clear all.
        for id in 0..=d {
            tree.clear_dirty(id);
        }

        tree.mark_dirty(d, DirtyFlags::CONTENT);

        // Every ancestor should have CHILDREN.
        assert!(tree.get(c).unwrap().dirty.contains(DirtyFlags::CHILDREN));
        assert!(tree.get(b).unwrap().dirty.contains(DirtyFlags::CHILDREN));
        assert!(tree.get(a).unwrap().dirty.contains(DirtyFlags::CHILDREN));
        assert!(tree.get(0).unwrap().dirty.contains(DirtyFlags::CHILDREN));
    }

    // ── App pane lifecycle simulation ──────────────────────────────

    /// Simulate the scene graph structure that phantom-app creates:
    /// Root → TabBar, ContentArea, StatusBar, overlays.
    /// Panes are children of ContentArea.
    fn build_app_scene() -> (SceneTree, NodeId /* content_area */) {
        let mut tree = SceneTree::new();
        let root = tree.root();
        tree.set_transform(root, 0.0, 0.0, 1280.0, 720.0);

        let _tab_bar = tree.add_node(root, NodeKind::TabBar);
        let content = tree.add_node(root, NodeKind::ContentArea);
        let _status_bar = tree.add_node(root, NodeKind::StatusBar);

        let cmd_bar = tree.add_node(root, NodeKind::CommandBar);
        tree.get_mut(cmd_bar).unwrap().render_layer = RenderLayer::Overlay;
        let debug_hud = tree.add_node(root, NodeKind::DebugHud);
        tree.get_mut(debug_hud).unwrap().render_layer = RenderLayer::Overlay;

        tree.update_world_transforms();
        (tree, content)
    }

    #[test]
    fn pane_add_creates_scene_node() {
        let (mut tree, content) = build_app_scene();
        let initial_count = tree.node_count();

        let pane1 = tree.add_node(content, NodeKind::Pane);
        assert_eq!(tree.node_count(), initial_count + 1);
        assert_eq!(tree.get(pane1).unwrap().kind, NodeKind::Pane);
        assert_eq!(tree.get(pane1).unwrap().parent, Some(content));
    }

    #[test]
    fn pane_split_adds_second_node() {
        let (mut tree, content) = build_app_scene();

        let pane1 = tree.add_node(content, NodeKind::Pane);
        tree.set_transform(pane1, 0.0, 30.0, 640.0, 660.0);

        let pane2 = tree.add_node(content, NodeKind::Pane);
        tree.set_transform(pane2, 640.0, 30.0, 640.0, 660.0);

        tree.update_world_transforms();

        let content_children = &tree.get(content).unwrap().children;
        assert_eq!(content_children.len(), 2);
        assert!(content_children.contains(&pane1));
        assert!(content_children.contains(&pane2));

        // World transforms should differ.
        let wt1 = tree.get(pane1).unwrap().world_transform;
        let wt2 = tree.get(pane2).unwrap().world_transform;
        assert_eq!(wt1.x, 0.0);
        assert_eq!(wt2.x, 640.0);
    }

    #[test]
    fn pane_close_removes_scene_node() {
        let (mut tree, content) = build_app_scene();

        let pane1 = tree.add_node(content, NodeKind::Pane);
        let pane2 = tree.add_node(content, NodeKind::Pane);
        let count_before = tree.node_count();

        tree.remove_node(pane1);

        assert_eq!(tree.node_count(), count_before - 1);
        assert!(tree.get(pane1).is_none());
        assert!(tree.get(pane2).is_some());

        // Content should only have pane2 as child.
        let children = &tree.get(content).unwrap().children;
        assert_eq!(children, &[pane2]);
    }

    #[test]
    fn pane_visibility_excludes_hidden_panes() {
        let (mut tree, content) = build_app_scene();

        let pane1 = tree.add_node(content, NodeKind::Pane);
        let pane2 = tree.add_node(content, NodeKind::Pane);

        // Hide pane1.
        tree.set_visible(pane1, false);

        let visible = tree.visible_nodes(RenderLayer::Scene);
        assert!(!visible.contains(&pane1), "hidden pane should be excluded");
        assert!(visible.contains(&pane2), "visible pane should be included");
    }

    #[test]
    fn pane_transform_sync_from_layout() {
        let (mut tree, content) = build_app_scene();

        let pane = tree.add_node(content, NodeKind::Pane);

        // Simulate layout engine providing a rect.
        tree.set_transform(pane, 12.0, 42.0, 600.0, 400.0);
        tree.update_world_transforms();

        let wt = tree.get(pane).unwrap().world_transform;
        assert_eq!(wt.x, 12.0);
        assert_eq!(wt.y, 42.0);
        assert_eq!(wt.width, 600.0);
        assert_eq!(wt.height, 400.0);
    }

    #[test]
    fn pane_resize_updates_transforms() {
        let (mut tree, content) = build_app_scene();

        let pane = tree.add_node(content, NodeKind::Pane);
        tree.set_transform(pane, 0.0, 30.0, 1280.0, 660.0);
        tree.update_world_transforms();

        // Simulate window resize.
        let root = tree.root();
        tree.set_transform(root, 0.0, 0.0, 1920.0, 1080.0);
        tree.set_transform(pane, 0.0, 30.0, 1920.0, 1020.0);
        tree.update_world_transforms();

        let wt = tree.get(pane).unwrap().world_transform;
        assert_eq!(wt.width, 1920.0);
        assert_eq!(wt.height, 1020.0);
    }

    #[test]
    fn overlay_nodes_separate_from_scene_panes() {
        let (tree, _content) = build_app_scene();

        // Content area panes are Scene layer.
        // CommandBar and DebugHud are Overlay layer.
        let scene_nodes = tree.visible_nodes(RenderLayer::Scene);
        let overlay_nodes = tree.visible_nodes(RenderLayer::Overlay);

        // Scene layer: root + tab_bar + content + status_bar.
        assert!(
            scene_nodes.iter().all(|&id| {
                let node = tree.get(id).unwrap();
                node.render_layer == RenderLayer::Scene
            }),
            "all scene nodes should be Scene layer"
        );

        // Overlay layer: command_bar + debug_hud.
        assert_eq!(overlay_nodes.len(), 2);
        assert!(overlay_nodes.iter().all(|&id| {
            tree.get(id).unwrap().render_layer == RenderLayer::Overlay
        }));
    }

    #[test]
    fn three_pane_split_then_close_middle() {
        let (mut tree, content) = build_app_scene();

        let p1 = tree.add_node(content, NodeKind::Pane);
        let p2 = tree.add_node(content, NodeKind::Pane);
        let p3 = tree.add_node(content, NodeKind::Pane);

        tree.set_transform(p1, 0.0, 30.0, 426.0, 660.0);
        tree.set_transform(p2, 426.0, 30.0, 426.0, 660.0);
        tree.set_transform(p3, 852.0, 30.0, 428.0, 660.0);
        tree.update_world_transforms();

        // Close middle pane.
        tree.remove_node(p2);

        let children = &tree.get(content).unwrap().children;
        assert_eq!(children.len(), 2);
        assert!(children.contains(&p1));
        assert!(children.contains(&p3));
        assert!(tree.get(p2).is_none());
    }
}
