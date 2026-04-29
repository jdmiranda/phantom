//! `phantom-dag` — code dependency graph extraction and persistence.
//!
//! Provides [`CodeDag`], a graph of [`DagNode`]s (code symbols) connected by
//! [`DagEdge`]s (dependency relationships).  Graphs can be serialised to / from
//! the `.planning/dag.json` format and queried for connected components via
//! [`CodeDag::community_ids`].

mod edge;
mod node;
mod persist;
mod union_find;
pub mod overlay;

pub use edge::{DagEdge, EdgeKind};
pub use node::{DagNode, NodeKind};
pub use overlay::{NodeOverlay, OverlayIndex};

use std::collections::HashMap;

use anyhow::Result;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// CodeDag
// ---------------------------------------------------------------------------

/// A directed code-dependency graph.
///
/// Nodes represent code symbols; edges represent relationships between them.
/// The graph is keyed on the fully-qualified symbol id so duplicate insertions
/// are handled gracefully (the later node overwrites the earlier one).
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct CodeDag {
    nodes: HashMap<String, DagNode>,
    edges: Vec<DagEdge>,
}

impl CodeDag {
    /// Create an empty graph.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    // -----------------------------------------------------------------------
    // Mutation
    // -----------------------------------------------------------------------

    /// Insert a node, replacing any existing node with the same id.
    pub fn add_node(&mut self, node: DagNode) {
        self.nodes.insert(node.id().to_owned(), node);
    }

    /// Append a directed edge.  The referenced node ids need not exist at the
    /// time the edge is added; unknown ids are simply ignored during traversal.
    pub fn add_edge(&mut self, edge: DagEdge) {
        self.edges.push(edge);
    }

    // -----------------------------------------------------------------------
    // Accessors
    // -----------------------------------------------------------------------

    /// Number of nodes in the graph.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Number of edges in the graph.
    #[must_use]
    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// Look up a node by its fully-qualified id.
    #[must_use]
    pub fn get_node(&self, id: &str) -> Option<&DagNode> {
        self.nodes.get(id)
    }

    /// Iterate over all nodes.
    pub fn nodes(&self) -> impl Iterator<Item = &DagNode> {
        self.nodes.values()
    }

    /// Iterate over all edges.
    pub fn edges(&self) -> impl Iterator<Item = &DagEdge> {
        self.edges.iter()
    }

    // -----------------------------------------------------------------------
    // Community detection
    // -----------------------------------------------------------------------

    /// Compute connected components using union-find (treating edges as
    /// undirected for the purpose of clustering).
    ///
    /// Returns a `HashMap<node_id, component_id>` where `component_id` is the
    /// canonical root id of the component.
    #[must_use]
    pub fn community_ids(&self) -> HashMap<String, String> {
        let mut uf = union_find::UnionFind::new();

        // Initialise every node as its own component.
        for id in self.nodes.keys() {
            uf.make_set(id);
        }

        // Union nodes connected by any edge.
        for edge in &self.edges {
            // Only union nodes that actually exist in the graph.
            if self.nodes.contains_key(edge.from()) && self.nodes.contains_key(edge.to()) {
                uf.union(edge.from(), edge.to());
            }
        }

        self.nodes
            .keys()
            .map(|id| (id.clone(), uf.find(id)))
            .collect()
    }

    // -----------------------------------------------------------------------
    // Serialisation
    // -----------------------------------------------------------------------

    /// Serialise the graph to a JSON string in `.planning/dag.json` format.
    ///
    /// # Errors
    ///
    /// Returns an error if serialisation fails (practically unreachable for
    /// well-formed data).
    pub fn to_json(&self) -> Result<String> {
        persist::to_json(self)
    }

    /// Deserialise a graph from a JSON string produced by [`to_json`].
    ///
    /// # Errors
    ///
    /// Returns an error if the JSON is malformed or the schema version is
    /// unrecognised.
    ///
    /// [`to_json`]: CodeDag::to_json
    pub fn from_json(json: &str) -> Result<Self> {
        persist::from_json(json)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn node(id: &str) -> DagNode {
        DagNode::new(id.to_owned(), NodeKind::Function, PathBuf::from("src/lib.rs"), 1)
    }

    fn edge(from: &str, to: &str) -> DagEdge {
        DagEdge::new(from.to_owned(), to.to_owned(), EdgeKind::Calls)
    }

    // 1. Add a node and retrieve it.
    #[test]
    fn add_node_stores_and_retrieves() {
        let mut dag = CodeDag::new();
        dag.add_node(node("phantom_agents::dispatch::dispatch_tool"));

        assert_eq!(dag.node_count(), 1);
        let n = dag.get_node("phantom_agents::dispatch::dispatch_tool").unwrap();
        assert_eq!(n.id(), "phantom_agents::dispatch::dispatch_tool");
        assert_eq!(*n.kind(), NodeKind::Function);
    }

    // 2. Add an edge and verify counts.
    #[test]
    fn add_edge_increments_count() {
        let mut dag = CodeDag::new();
        dag.add_node(node("a"));
        dag.add_node(node("b"));
        dag.add_edge(edge("a", "b"));

        assert_eq!(dag.edge_count(), 1);
        let e = dag.edges().next().unwrap();
        assert_eq!(e.from(), "a");
        assert_eq!(e.to(), "b");
        assert_eq!(*e.kind(), EdgeKind::Calls);
    }

    // 3. JSON round-trip preserves all data.
    #[test]
    fn json_round_trip() {
        let mut dag = CodeDag::new();
        dag.add_node(DagNode::new(
            "mod::foo".to_owned(),
            NodeKind::Function,
            PathBuf::from("src/foo.rs"),
            42,
        ));
        dag.add_node(DagNode::new(
            "mod::Bar".to_owned(),
            NodeKind::Struct,
            PathBuf::from("src/bar.rs"),
            10,
        ));
        dag.add_edge(DagEdge::new(
            "mod::foo".to_owned(),
            "mod::Bar".to_owned(),
            EdgeKind::Uses,
        ));

        let json = dag.to_json().unwrap();
        let restored = CodeDag::from_json(&json).unwrap();

        assert_eq!(restored.node_count(), 2);
        assert_eq!(restored.edge_count(), 1);

        let foo = restored.get_node("mod::foo").unwrap();
        assert_eq!(*foo.kind(), NodeKind::Function);
        assert_eq!(foo.line(), 42);

        let e = restored.edges().next().unwrap();
        assert_eq!(e.from(), "mod::foo");
        assert_eq!(e.to(), "mod::Bar");
        assert_eq!(*e.kind(), EdgeKind::Uses);
    }

    // 4. Community detection on a disconnected graph.
    #[test]
    fn community_ids_disconnected_graph() {
        let mut dag = CodeDag::new();
        // Component A: a — b — c
        dag.add_node(node("a"));
        dag.add_node(node("b"));
        dag.add_node(node("c"));
        dag.add_edge(edge("a", "b"));
        dag.add_edge(edge("b", "c"));
        // Component B: x — y
        dag.add_node(node("x"));
        dag.add_node(node("y"));
        dag.add_edge(edge("x", "y"));
        // Isolated node
        dag.add_node(node("z"));

        let ids = dag.community_ids();

        // All nodes in component A must share the same community id.
        let ca = ids["a"].clone();
        assert_eq!(ids["b"], ca);
        assert_eq!(ids["c"], ca);

        // Component B must share a different community id.
        let cb = ids["x"].clone();
        assert_eq!(ids["y"], cb);
        assert_ne!(ca, cb);

        // z is its own component.
        let cz = ids["z"].clone();
        assert_ne!(cz, ca);
        assert_ne!(cz, cb);
    }

    // 5. Empty graph behaves correctly.
    #[test]
    fn empty_graph() {
        let dag = CodeDag::new();
        assert_eq!(dag.node_count(), 0);
        assert_eq!(dag.edge_count(), 0);
        assert!(dag.community_ids().is_empty());

        // JSON round-trip of empty graph.
        let json = dag.to_json().unwrap();
        let restored = CodeDag::from_json(&json).unwrap();
        assert_eq!(restored.node_count(), 0);
        assert_eq!(restored.edge_count(), 0);
    }

    // 6. Duplicate node insertion — later node replaces earlier one.
    #[test]
    fn duplicate_node_replaces_earlier() {
        let mut dag = CodeDag::new();
        dag.add_node(DagNode::new(
            "a".to_owned(),
            NodeKind::Function,
            PathBuf::from("old.rs"),
            1,
        ));
        dag.add_node(DagNode::new(
            "a".to_owned(),
            NodeKind::Struct,
            PathBuf::from("new.rs"),
            99,
        ));

        // Still only one node.
        assert_eq!(dag.node_count(), 1);
        let n = dag.get_node("a").unwrap();
        // The second insertion wins.
        assert_eq!(*n.kind(), NodeKind::Struct);
        assert_eq!(n.line(), 99);
    }

    // 7. JSON envelope contains expected schema version field.
    #[test]
    fn json_contains_schema_version() {
        let dag = CodeDag::new();
        let json = dag.to_json().unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["version"], 1);
        assert!(v["generated_at"].is_string());
    }
}
