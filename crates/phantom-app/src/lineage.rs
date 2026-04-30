//! Pane lineage model — parent/child relationships for terminal panes.
//!
//! This module implements the pane tree required by Phase 4 process-detach work
//! (issue #365). Every pane in the system starts as a root (no parent, no
//! children). When a subprocess is detached into its own pane the caller
//! records the relationship here; downstream issues (#367, #368, #369) consume
//! the tree to drive tether rendering, lifecycle harding, and nested detach.
//!
//! # Degenerate case
//!
//! Panes that have never been attached or detached have no entry in this tree
//! and behave identically to how they did before this module existed. All
//! queries on an unknown pane return sensible empty results rather than errors.
//!
//! # Tree invariants
//!
//! * Every pane has at most one parent.
//! * Children are ordered by registration time (insertion order).
//! * Removing a pane automatically unlinks it from its parent's child list and
//!   orphans its children (their `parent` becomes `None`).  Callers that need a
//!   different orphan policy can reparent children before removal.

use phantom_adapter::AppId;
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// PaneLineage
// ---------------------------------------------------------------------------

/// The pane lineage registry.
///
/// Stored on [`AppCoordinator`](crate::coordinator::AppCoordinator) and
/// updated via its `lineage` accessor methods rather than directly.
#[derive(Debug, Default)]
pub struct PaneLineage {
    /// Maps each pane to its parent, if any.
    parent: HashMap<AppId, AppId>,
    /// Maps each pane to its ordered list of children.
    children: HashMap<AppId, Vec<AppId>>,
}

impl PaneLineage {
    /// Create an empty lineage registry.
    pub fn new() -> Self {
        Self::default()
    }

    // -----------------------------------------------------------------------
    // Mutation
    // -----------------------------------------------------------------------

    /// Attach `child` as a child of `parent`.
    ///
    /// If `child` already has a parent, the old relationship is removed before
    /// the new one is established — a pane can only have one parent at a time.
    ///
    /// No-op when `child == parent`.
    pub fn attach(&mut self, parent: AppId, child: AppId) {
        if parent == child {
            return;
        }

        // Remove any prior parent link for `child`.
        self.detach_from_parent(child);

        self.parent.insert(child, parent);
        self.children.entry(parent).or_default().push(child);
    }

    /// Remove `pane` from the registry and clean up all lineage references.
    ///
    /// * Unlinks `pane` from its parent's children list.
    /// * Orphans all of `pane`'s children (their parent pointer is cleared, but
    ///   they remain in the registry as roots).
    /// * Removes `pane`'s own entries.
    pub fn remove(&mut self, pane: AppId) {
        // Unlink from parent.
        self.detach_from_parent(pane);

        // Orphan children.
        let children = self.children.remove(&pane).unwrap_or_default();
        for child in &children {
            self.parent.remove(child);
        }
    }

    // -----------------------------------------------------------------------
    // Query
    // -----------------------------------------------------------------------

    /// The parent of `pane`, if any.
    #[must_use]
    pub fn parent_of(&self, pane: AppId) -> Option<AppId> {
        self.parent.get(&pane).copied()
    }

    /// The ordered children of `pane` (insertion order).
    ///
    /// Returns an empty slice when `pane` has no children.
    #[must_use]
    pub fn children_of(&self, pane: AppId) -> &[AppId] {
        self.children.get(&pane).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Whether `pane` is a root (has no parent).
    #[must_use]
    pub fn is_root(&self, pane: AppId) -> bool {
        !self.parent.contains_key(&pane)
    }

    /// The full ancestry chain from the tree root down to `pane` (inclusive).
    ///
    /// Returns `vec![pane]` when `pane` is a root.  The walk is bounded by the
    /// number of registered panes; a cycle (which the `attach` logic prevents)
    /// would terminate at the cycle-detection limit anyway.
    #[must_use]
    pub fn lineage(&self, pane: AppId) -> Vec<AppId> {
        let mut chain = Vec::new();
        let mut cursor = pane;
        loop {
            chain.push(cursor);
            match self.parent.get(&cursor).copied() {
                Some(p) => cursor = p,
                None => break,
            }
        }
        chain.reverse();
        chain
    }

    /// The subtree rooted at `pane`: `pane` itself followed by all descendants
    /// in depth-first pre-order.
    #[must_use]
    pub fn subtree(&self, pane: AppId) -> Vec<AppId> {
        let mut out = Vec::new();
        self.collect_subtree(pane, &mut out);
        out
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    fn detach_from_parent(&mut self, pane: AppId) {
        if let Some(old_parent) = self.parent.remove(&pane) {
            if let Some(siblings) = self.children.get_mut(&old_parent) {
                siblings.retain(|&c| c != pane);
            }
        }
    }

    fn collect_subtree(&self, pane: AppId, out: &mut Vec<AppId>) {
        out.push(pane);
        for &child in self.children_of(pane) {
            self.collect_subtree(child, out);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u32) -> AppId {
        n
    }

    // -----------------------------------------------------------------------
    // attach / parent-child basics
    // -----------------------------------------------------------------------

    #[test]
    fn attach_records_parent_and_child() {
        let mut lin = PaneLineage::new();
        lin.attach(id(1), id(2));

        assert_eq!(lin.parent_of(id(2)), Some(id(1)));
        assert_eq!(lin.children_of(id(1)), &[id(2)]);
        assert!(lin.is_root(id(1)));
        assert!(!lin.is_root(id(2)));
    }

    #[test]
    fn attach_multiple_children_preserves_insertion_order() {
        let mut lin = PaneLineage::new();
        lin.attach(id(0), id(1));
        lin.attach(id(0), id(2));
        lin.attach(id(0), id(3));

        assert_eq!(lin.children_of(id(0)), &[id(1), id(2), id(3)]);
    }

    #[test]
    fn attach_reparents_child() {
        let mut lin = PaneLineage::new();
        lin.attach(id(1), id(3));
        lin.attach(id(2), id(3)); // reparent 3 under 2

        assert_eq!(lin.parent_of(id(3)), Some(id(2)));
        assert!(lin.children_of(id(1)).is_empty(), "old parent must drop child");
        assert_eq!(lin.children_of(id(2)), &[id(3)]);
    }

    #[test]
    fn attach_noop_when_same_as_parent() {
        let mut lin = PaneLineage::new();
        lin.attach(id(1), id(1));
        assert!(lin.is_root(id(1)));
        assert!(lin.children_of(id(1)).is_empty());
    }

    // -----------------------------------------------------------------------
    // remove
    // -----------------------------------------------------------------------

    #[test]
    fn remove_unlinks_from_parent() {
        let mut lin = PaneLineage::new();
        lin.attach(id(1), id(2));
        lin.remove(id(2));

        assert_eq!(lin.parent_of(id(2)), None);
        assert!(lin.children_of(id(1)).is_empty());
        assert!(lin.is_root(id(1)));
    }

    #[test]
    fn remove_orphans_children() {
        let mut lin = PaneLineage::new();
        lin.attach(id(0), id(1));
        lin.attach(id(1), id(2));

        lin.remove(id(1));

        // 1 is gone; 2 is now a root.
        assert_eq!(lin.parent_of(id(1)), None);
        assert_eq!(lin.parent_of(id(2)), None);
        assert!(lin.is_root(id(2)));
        // 0 no longer lists 1 as a child.
        assert!(lin.children_of(id(0)).is_empty());
    }

    #[test]
    fn remove_unknown_pane_is_noop() {
        let mut lin = PaneLineage::new();
        lin.remove(id(99)); // must not panic
        assert_eq!(lin.parent_of(id(99)), None);
        assert!(lin.children_of(id(99)).is_empty());
    }

    // -----------------------------------------------------------------------
    // lineage walk
    // -----------------------------------------------------------------------

    #[test]
    fn lineage_root_returns_self() {
        let lin = PaneLineage::new();
        assert_eq!(lin.lineage(id(5)), vec![id(5)]);
    }

    #[test]
    fn lineage_walks_root_to_leaf() {
        let mut lin = PaneLineage::new();
        lin.attach(id(1), id(2));
        lin.attach(id(2), id(3));

        assert_eq!(lin.lineage(id(3)), vec![id(1), id(2), id(3)]);
    }

    #[test]
    fn lineage_mid_chain() {
        let mut lin = PaneLineage::new();
        lin.attach(id(1), id(2));
        lin.attach(id(2), id(3));
        lin.attach(id(3), id(4));

        assert_eq!(lin.lineage(id(3)), vec![id(1), id(2), id(3)]);
    }

    // -----------------------------------------------------------------------
    // subtree
    // -----------------------------------------------------------------------

    #[test]
    fn subtree_leaf_returns_self() {
        let lin = PaneLineage::new();
        assert_eq!(lin.subtree(id(7)), vec![id(7)]);
    }

    #[test]
    fn subtree_depth_first_preorder() {
        let mut lin = PaneLineage::new();
        // Tree: 1 → {2, 3}, 2 → {4}
        lin.attach(id(1), id(2));
        lin.attach(id(1), id(3));
        lin.attach(id(2), id(4));

        assert_eq!(lin.subtree(id(1)), vec![id(1), id(2), id(4), id(3)]);
    }

    // -----------------------------------------------------------------------
    // children_of on unknown pane
    // -----------------------------------------------------------------------

    #[test]
    fn children_of_unknown_returns_empty_slice() {
        let lin = PaneLineage::new();
        assert!(lin.children_of(id(42)).is_empty());
    }
}
