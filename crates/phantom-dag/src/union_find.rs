//! Weighted union-find (disjoint-set) for connected-component computation.
//!
//! Node ids are arbitrary `String`s.  Rank-based union with path-compression
//! find gives amortised near-linear performance.

use std::collections::HashMap;

/// A union-find data structure keyed on `String` node ids.
pub(crate) struct UnionFind {
    parent: HashMap<String, String>,
    rank: HashMap<String, u32>,
}

impl UnionFind {
    pub(crate) fn new() -> Self {
        Self { parent: HashMap::new(), rank: HashMap::new() }
    }

    /// Register a new singleton set for `id`.  A no-op if `id` is already
    /// present.
    pub(crate) fn make_set(&mut self, id: &str) {
        if !self.parent.contains_key(id) {
            self.parent.insert(id.to_owned(), id.to_owned());
            self.rank.insert(id.to_owned(), 0);
        }
    }

    /// Find the canonical root of the set containing `id`, with path
    /// compression.  Returns the root id as an owned `String`.
    /// Panics if `id` was never registered via [`make_set`].
    pub(crate) fn find(&mut self, id: &str) -> String {
        // Iterative path-compression.
        let mut current = id.to_owned();
        loop {
            let parent = self.parent[&current].clone();
            if parent == current {
                break;
            }
            // Path compression: point directly to grandparent.
            let grandparent = self.parent[&parent].clone();
            self.parent.insert(current.clone(), grandparent.clone());
            current = grandparent;
        }
        current
    }

    /// Merge the sets containing `a` and `b` using union-by-rank.
    pub(crate) fn union(&mut self, a: &str, b: &str) {
        let root_a = self.find(a);
        let root_b = self.find(b);

        if root_a == root_b {
            return;
        }

        let rank_a = self.rank[&root_a];
        let rank_b = self.rank[&root_b];

        // Lower-rank root becomes a child of the higher-rank root.
        match rank_a.cmp(&rank_b) {
            std::cmp::Ordering::Less => {
                self.parent.insert(root_a, root_b);
            }
            std::cmp::Ordering::Greater => {
                self.parent.insert(root_b, root_a);
            }
            std::cmp::Ordering::Equal => {
                self.parent.insert(root_b, root_a.clone());
                *self.rank.entry(root_a).or_insert(0) += 1;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn singleton_is_own_root() {
        let mut uf = UnionFind::new();
        uf.make_set("a");
        assert_eq!(uf.find("a"), "a");
    }

    #[test]
    fn union_merges_components() {
        let mut uf = UnionFind::new();
        for id in ["a", "b", "c"] {
            uf.make_set(id);
        }
        uf.union("a", "b");
        uf.union("b", "c");

        let ra = uf.find("a");
        let rb = uf.find("b");
        let rc = uf.find("c");
        assert_eq!(ra, rb);
        assert_eq!(rb, rc);
    }

    #[test]
    fn disconnected_sets_have_different_roots() {
        let mut uf = UnionFind::new();
        for id in ["a", "b", "x", "y"] {
            uf.make_set(id);
        }
        uf.union("a", "b");
        uf.union("x", "y");

        let rab = uf.find("a");
        let rxy = uf.find("x");
        assert_ne!(rab, rxy);
    }
}
