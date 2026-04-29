//! Cartographer-only DAG navigation tool surface.
//!
//! The [`AgentRole::Cartographer`] is spawned in response to user queries that
//! require understanding graph topology — "what's blocking X?", "show me the
//! critical path to Y". It holds `Sense` (read the DAG state) and `Coordinate`
//! (send commands to the DAG viewer surface) but NO `Act` — it cannot write
//! files, run commands, or mutate the user's world.
//!
//! ## Tool catalog
//!
//! | Tool | Class | Description |
//! |------|-------|-------------|
//! | `dag_list_nodes` | Sense | List all nodes in the execution DAG |
//! | `dag_get_node` | Sense | Fetch details for a single node |
//! | `dag_list_edges` | Sense | List all dependency edges |
//! | `dag_find_blocking` | Sense | Find nodes blocking a given target |
//! | `dag_critical_path` | Sense | Compute the longest path to completion |
//! | `dag_mark_complete` | Coordinate | Mark a node complete |
//! | `dag_mark_failed` | Coordinate | Mark a node failed |
//! | `dag_mark_skipped` | Coordinate | Mark a node skipped |
//! | `dag_add_child` | Coordinate | Add a new child goal under a node |
//! | `dag_annotate` | Coordinate | Attach a temporary annotation to a node |
//! | `dag_clear_annotations` | Coordinate | Clear all temporary annotations |
//!
//! ## Capability gating
//!
//! `Sense`-class tools (`dag_list_nodes`, `dag_get_node`, `dag_list_edges`,
//! `dag_find_blocking`, `dag_critical_path`) are read-only: they observe the
//! DAG state and have no side effects. Any role holding `Sense` can call them.
//!
//! `Coordinate`-class tools (`dag_mark_complete`, `dag_mark_failed`,
//! `dag_mark_skipped`, `dag_add_child`, `dag_annotate`, `dag_clear_annotations`)
//! steer the DAG viewer surface. Only roles holding `Coordinate` can call them.
//! The Cartographer's manifest declares both `Sense` and `Coordinate` — and
//! nothing else.
//!
//! ## Issue
//!
//! Implements GitHub issue #67.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::role::CapabilityClass;

// ---------------------------------------------------------------------------
// NodeStatus
// ---------------------------------------------------------------------------

/// Execution status of a single DAG node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    /// Not yet started.
    Pending,
    /// Actively being worked on.
    Active,
    /// Completed successfully.
    Complete,
    /// Failed after exhausting attempts.
    Failed,
    /// Skipped (rendered irrelevant by a re-plan or user request).
    Skipped,
}

impl NodeStatus {
    /// Whether this status is terminal (no further state transitions possible).
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Complete | Self::Failed | Self::Skipped)
    }
}

// ---------------------------------------------------------------------------
// DagNode
// ---------------------------------------------------------------------------

/// A single node in the execution DAG.
///
/// Nodes are uniquely identified by a `u64` id. Dependencies are represented
/// as a vec of parent node ids — a node is eligible to run once all its
/// dependencies reach a terminal status.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DagNode {
    /// Stable node identifier.
    pub id: u64,
    /// Human-readable description of what this node does.
    pub description: String,
    /// Current execution status.
    pub status: NodeStatus,
    /// Ids of nodes that must complete before this one can run.
    pub dependencies: Vec<u64>,
    /// Optional GitHub issue number this node corresponds to.
    pub issue_number: Option<u64>,
    /// Temporary annotations attached by the Cartographer.
    pub annotations: Vec<String>,
}

impl DagNode {
    /// Create a pending node with no dependencies.
    pub fn new(id: u64, description: impl Into<String>) -> Self {
        Self {
            id,
            description: description.into(),
            status: NodeStatus::Pending,
            dependencies: Vec::new(),
            issue_number: None,
            annotations: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// DagStore — shared, mutable DAG state
// ---------------------------------------------------------------------------

/// Thread-safe DAG store.
///
/// All mutation goes through the methods on this type, which hold the lock
/// for the minimum duration required. Cartographer tools receive an
/// `Arc<Mutex<DagStore>>` via [`DagExplorerContext`] and use the helper
/// methods to read or mutate the graph.
#[derive(Debug, Default)]
pub struct DagStore {
    nodes: HashMap<u64, DagNode>,
    next_id: u64,
}

impl DagStore {
    /// Create a new, empty store.
    pub fn new() -> Self {
        Self { nodes: HashMap::new(), next_id: 1 }
    }

    /// Insert `node` into the store. Replaces any existing node with the same id.
    pub fn insert(&mut self, node: DagNode) {
        let id = node.id;
        if id >= self.next_id {
            self.next_id = id + 1;
        }
        self.nodes.insert(id, node);
    }

    /// Allocate a fresh node id and insert a new pending node.
    ///
    /// Returns the new node's id.
    pub fn add_node(&mut self, description: impl Into<String>, dependencies: Vec<u64>) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let mut node = DagNode::new(id, description);
        node.dependencies = dependencies;
        self.nodes.insert(id, node);
        id
    }

    /// Get an immutable reference to a node by id.
    pub fn get(&self, id: u64) -> Option<&DagNode> {
        self.nodes.get(&id)
    }

    /// Get a mutable reference to a node by id.
    pub fn get_mut(&mut self, id: u64) -> Option<&mut DagNode> {
        self.nodes.get_mut(&id)
    }

    /// Collect all nodes, sorted by id.
    pub fn all_nodes(&self) -> Vec<&DagNode> {
        let mut nodes: Vec<&DagNode> = self.nodes.values().collect();
        nodes.sort_by_key(|n| n.id);
        nodes
    }

    /// All edges as `(parent_id, child_id)` pairs where `child` depends on `parent`.
    pub fn all_edges(&self) -> Vec<(u64, u64)> {
        let mut edges: Vec<(u64, u64)> = self
            .nodes
            .values()
            .flat_map(|node| node.dependencies.iter().map(move |&dep| (dep, node.id)))
            .collect();
        edges.sort();
        edges
    }

    /// Find nodes that are *directly or transitively* blocking `target_id`.
    ///
    /// A node `B` blocks `target_id` iff `target_id` has a transitive dependency
    /// on `B` AND `B` has not yet reached a terminal status.
    ///
    /// Returns ids in BFS order (closest blockers first).
    pub fn find_blocking(&self, target_id: u64) -> Vec<u64> {
        let mut blockers: Vec<u64> = Vec::new();
        let mut visited: HashSet<u64> = HashSet::new();
        let mut queue: VecDeque<u64> = VecDeque::new();

        // Seed the queue with the target's direct dependencies.
        if let Some(node) = self.nodes.get(&target_id) {
            for &dep in &node.dependencies {
                if visited.insert(dep) {
                    queue.push_back(dep);
                }
            }
        }

        while let Some(id) = queue.pop_front() {
            let Some(node) = self.nodes.get(&id) else { continue };

            if !node.status.is_terminal() {
                blockers.push(id);
            }

            // Recurse into this node's own dependencies.
            for &dep in &node.dependencies {
                if visited.insert(dep) {
                    queue.push_back(dep);
                }
            }
        }

        blockers
    }

    /// Compute the longest path (critical path) from any root to `target_id`.
    ///
    /// Returns the sequence of node ids on the critical path, starting from the
    /// earliest ancestor and ending at `target_id`. Returns just `[target_id]`
    /// when the target has no dependencies.
    ///
    /// Uses a simple BFS-based longest-path search; valid because DAGs are
    /// acyclic. A cycle (invalid input) causes the walk to terminate early via
    /// the visited guard.
    pub fn critical_path(&self, target_id: u64) -> Vec<u64> {
        // dist[id] = (max depth from target, predecessor id)
        let mut dist: HashMap<u64, (usize, Option<u64>)> = HashMap::new();
        dist.insert(target_id, (0, None));

        let mut stack: Vec<u64> = vec![target_id];
        let mut visited: HashSet<u64> = HashSet::new();

        while let Some(id) = stack.pop() {
            if !visited.insert(id) {
                continue;
            }
            let current_depth = dist.get(&id).map(|(d, _)| *d).unwrap_or(0);
            let Some(node) = self.nodes.get(&id) else { continue };
            for &dep in &node.dependencies {
                let new_depth = current_depth + 1;
                let entry = dist.entry(dep).or_insert((0, None));
                if new_depth > entry.0 {
                    *entry = (new_depth, Some(id));
                }
                stack.push(dep);
            }
        }

        // Find the ancestor with the maximum depth.
        let Some((&root_id, _)) = dist.iter().max_by_key(|(_, (d, _))| d) else {
            return vec![target_id];
        };

        // Walk forward from root to target via the predecessor links.
        // Build reverse predecessor: child → parent for the critical path.
        let mut path: Vec<u64> = Vec::new();
        let mut cursor = root_id;

        // Build child-to-parent map for forward traversal.
        let child_of: HashMap<u64, u64> = dist
            .iter()
            .filter_map(|(&id, (_, pred))| pred.map(|p| (id, p)))
            .collect();

        path.push(cursor);
        while let Some(&next) = child_of.get(&cursor) {
            path.push(next);
            if next == target_id {
                break;
            }
            cursor = next;
        }

        if path.last() != Some(&target_id) {
            path.push(target_id);
        }

        path
    }
}

// ---------------------------------------------------------------------------
// CartographerTool catalog
// ---------------------------------------------------------------------------

/// Tool ids exposed by the Cartographer role (issue #67).
///
/// The tool is the unit of dispatch gating: every variant declares its
/// [`CapabilityClass`] so [`crate::dispatch::dispatch_tool`] can default-deny
/// calls from agents whose manifest lacks that class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CartographerTool {
    // ---- Sense-class (read-only) ----

    /// List all nodes in the DAG. Sense.
    DagListNodes,
    /// Fetch details for a single node by id. Sense.
    DagGetNode,
    /// List all dependency edges as `(parent_id, child_id)` pairs. Sense.
    DagListEdges,
    /// Find nodes blocking a given target (BFS, non-terminal ancestors). Sense.
    DagFindBlocking,
    /// Compute the critical (longest) path to a target node. Sense.
    DagCriticalPath,

    // ---- Coordinate-class (mutate DAG state / viewer) ----

    /// Mark a node complete. Coordinate.
    DagMarkComplete,
    /// Mark a node failed. Coordinate.
    DagMarkFailed,
    /// Mark a node skipped. Coordinate.
    DagMarkSkipped,
    /// Add a new child goal node under an existing parent. Coordinate.
    DagAddChild,
    /// Attach a temporary annotation string to a node. Coordinate.
    DagAnnotate,
    /// Clear all temporary annotations from all nodes. Coordinate.
    DagClearAnnotations,
}

impl CartographerTool {
    /// Wire name used in tool definitions and JSON dispatch.
    #[must_use]
    pub fn api_name(self) -> &'static str {
        match self {
            Self::DagListNodes => "dag_list_nodes",
            Self::DagGetNode => "dag_get_node",
            Self::DagListEdges => "dag_list_edges",
            Self::DagFindBlocking => "dag_find_blocking",
            Self::DagCriticalPath => "dag_critical_path",
            Self::DagMarkComplete => "dag_mark_complete",
            Self::DagMarkFailed => "dag_mark_failed",
            Self::DagMarkSkipped => "dag_mark_skipped",
            Self::DagAddChild => "dag_add_child",
            Self::DagAnnotate => "dag_annotate",
            Self::DagClearAnnotations => "dag_clear_annotations",
        }
    }

    /// Parse from a wire name. Returns `None` for unknown names.
    #[must_use]
    pub fn from_api_name(name: &str) -> Option<Self> {
        match name {
            "dag_list_nodes" => Some(Self::DagListNodes),
            "dag_get_node" => Some(Self::DagGetNode),
            "dag_list_edges" => Some(Self::DagListEdges),
            "dag_find_blocking" => Some(Self::DagFindBlocking),
            "dag_critical_path" => Some(Self::DagCriticalPath),
            "dag_mark_complete" => Some(Self::DagMarkComplete),
            "dag_mark_failed" => Some(Self::DagMarkFailed),
            "dag_mark_skipped" => Some(Self::DagMarkSkipped),
            "dag_add_child" => Some(Self::DagAddChild),
            "dag_annotate" => Some(Self::DagAnnotate),
            "dag_clear_annotations" => Some(Self::DagClearAnnotations),
            _ => None,
        }
    }

    /// The capability class this tool requires.
    ///
    /// Sense-class tools are read-only; Coordinate-class tools mutate DAG state.
    /// The Cartographer holds both; no other role currently uses these tools.
    #[must_use]
    pub fn class(self) -> CapabilityClass {
        match self {
            Self::DagListNodes
            | Self::DagGetNode
            | Self::DagListEdges
            | Self::DagFindBlocking
            | Self::DagCriticalPath => CapabilityClass::Sense,

            Self::DagMarkComplete
            | Self::DagMarkFailed
            | Self::DagMarkSkipped
            | Self::DagAddChild
            | Self::DagAnnotate
            | Self::DagClearAnnotations => CapabilityClass::Coordinate,
        }
    }
}

// ---------------------------------------------------------------------------
// DagExplorerContext
// ---------------------------------------------------------------------------

/// Context passed to every [`CartographerTool`] handler.
///
/// The DAG store is the single mutable state shared across tool calls within
/// a Cartographer's session. Wrap it in `Arc<Mutex<…>>` so future multi-turn
/// sessions can share state across turns.
#[derive(Clone)]
pub struct DagExplorerContext {
    pub dag: Arc<Mutex<DagStore>>,
}

impl DagExplorerContext {
    /// Construct a context backed by `dag`.
    #[must_use]
    pub fn new(dag: Arc<Mutex<DagStore>>) -> Self {
        Self { dag }
    }

    /// Construct a context backed by a fresh, empty [`DagStore`].
    #[must_use]
    pub fn empty() -> Self {
        Self::new(Arc::new(Mutex::new(DagStore::new())))
    }
}

// ---------------------------------------------------------------------------
// Argument decoders (private helpers)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct NodeIdArgs {
    node_id: u64,
}

#[derive(Debug, Deserialize)]
struct AddChildArgs {
    parent_id: u64,
    description: String,
}

#[derive(Debug, Deserialize)]
struct AnnotateArgs {
    node_id: u64,
    note: String,
}

// ---------------------------------------------------------------------------
// Handler functions
// ---------------------------------------------------------------------------

/// List all nodes in the DAG, sorted by id.
///
/// Returns a JSON array of [`DagNode`] objects.
pub fn dag_list_nodes(ctx: &DagExplorerContext) -> Result<String, String> {
    let guard = ctx
        .dag
        .lock()
        .map_err(|_| "DAG store mutex poisoned".to_string())?;
    let nodes: Vec<&DagNode> = guard.all_nodes();
    serde_json::to_string(&nodes).map_err(|e| format!("serialize error: {e}"))
}

/// Fetch a single node by id.
///
/// `args` must contain `node_id: u64`.
/// Returns `Err("node not found: <id>")` when the id is absent.
pub fn dag_get_node(args: &serde_json::Value, ctx: &DagExplorerContext) -> Result<String, String> {
    let parsed: NodeIdArgs = serde_json::from_value(args.clone())
        .map_err(|e| format!("invalid dag_get_node args: {e}"))?;

    let guard = ctx
        .dag
        .lock()
        .map_err(|_| "DAG store mutex poisoned".to_string())?;

    let node = guard
        .get(parsed.node_id)
        .ok_or_else(|| format!("node not found: {}", parsed.node_id))?;

    serde_json::to_string(node).map_err(|e| format!("serialize error: {e}"))
}

/// List all dependency edges as `(parent_id, child_id)` pairs.
///
/// Returns a JSON array of two-element arrays `[parent_id, child_id]`.
pub fn dag_list_edges(ctx: &DagExplorerContext) -> Result<String, String> {
    let guard = ctx
        .dag
        .lock()
        .map_err(|_| "DAG store mutex poisoned".to_string())?;
    let edges = guard.all_edges();
    serde_json::to_string(&edges).map_err(|e| format!("serialize error: {e}"))
}

/// Find non-terminal nodes that are blocking `target_id` (direct + transitive).
///
/// `args` must contain `node_id: u64` — the target whose blockers to find.
/// Returns a JSON array of blocker node ids in BFS order (closest first).
pub fn dag_find_blocking(
    args: &serde_json::Value,
    ctx: &DagExplorerContext,
) -> Result<String, String> {
    let parsed: NodeIdArgs = serde_json::from_value(args.clone())
        .map_err(|e| format!("invalid dag_find_blocking args: {e}"))?;

    let guard = ctx
        .dag
        .lock()
        .map_err(|_| "DAG store mutex poisoned".to_string())?;

    let blockers = guard.find_blocking(parsed.node_id);
    serde_json::to_string(&blockers).map_err(|e| format!("serialize error: {e}"))
}

/// Compute the critical (longest) path to `target_id`.
///
/// `args` must contain `node_id: u64`.
/// Returns a JSON array of node ids from earliest ancestor to `target_id`.
pub fn dag_critical_path(
    args: &serde_json::Value,
    ctx: &DagExplorerContext,
) -> Result<String, String> {
    let parsed: NodeIdArgs = serde_json::from_value(args.clone())
        .map_err(|e| format!("invalid dag_critical_path args: {e}"))?;

    let guard = ctx
        .dag
        .lock()
        .map_err(|_| "DAG store mutex poisoned".to_string())?;

    let path = guard.critical_path(parsed.node_id);
    serde_json::to_string(&path).map_err(|e| format!("serialize error: {e}"))
}

/// Mark a node complete.
///
/// `args` must contain `node_id: u64`.
/// Returns `"marked node <id> complete"` on success.
pub fn dag_mark_complete(
    args: &serde_json::Value,
    ctx: &DagExplorerContext,
) -> Result<String, String> {
    set_node_status(args, ctx, NodeStatus::Complete, "complete")
}

/// Mark a node failed.
///
/// `args` must contain `node_id: u64`.
/// Returns `"marked node <id> failed"` on success.
pub fn dag_mark_failed(
    args: &serde_json::Value,
    ctx: &DagExplorerContext,
) -> Result<String, String> {
    set_node_status(args, ctx, NodeStatus::Failed, "failed")
}

/// Mark a node skipped.
///
/// `args` must contain `node_id: u64`.
/// Returns `"marked node <id> skipped"` on success.
pub fn dag_mark_skipped(
    args: &serde_json::Value,
    ctx: &DagExplorerContext,
) -> Result<String, String> {
    set_node_status(args, ctx, NodeStatus::Skipped, "skipped")
}

fn set_node_status(
    args: &serde_json::Value,
    ctx: &DagExplorerContext,
    status: NodeStatus,
    label: &str,
) -> Result<String, String> {
    let parsed: NodeIdArgs = serde_json::from_value(args.clone())
        .map_err(|e| format!("invalid node_id args: {e}"))?;

    let mut guard = ctx
        .dag
        .lock()
        .map_err(|_| "DAG store mutex poisoned".to_string())?;

    let node = guard
        .get_mut(parsed.node_id)
        .ok_or_else(|| format!("node not found: {}", parsed.node_id))?;

    node.status = status;
    Ok(format!("marked node {} {label}", parsed.node_id))
}

/// Add a new child goal under an existing parent node.
///
/// `args` must contain:
/// - `parent_id: u64` — the node this child depends on.
/// - `description: String` — human-readable description of the new node.
///
/// Returns `"added child node <new_id> under parent <parent_id>"`.
pub fn dag_add_child(
    args: &serde_json::Value,
    ctx: &DagExplorerContext,
) -> Result<String, String> {
    let parsed: AddChildArgs = serde_json::from_value(args.clone())
        .map_err(|e| format!("invalid dag_add_child args: {e}"))?;

    let mut guard = ctx
        .dag
        .lock()
        .map_err(|_| "DAG store mutex poisoned".to_string())?;

    // Verify the parent exists.
    if guard.get(parsed.parent_id).is_none() {
        return Err(format!("parent node not found: {}", parsed.parent_id));
    }

    let new_id = guard.add_node(parsed.description, vec![parsed.parent_id]);
    Ok(format!("added child node {new_id} under parent {}", parsed.parent_id))
}

/// Attach a temporary annotation note to a node.
///
/// `args` must contain:
/// - `node_id: u64`
/// - `note: String`
///
/// Annotations are ephemeral: they persist in memory only and are cleared by
/// [`dag_clear_annotations`].
///
/// Returns `"annotated node <id>"`.
pub fn dag_annotate(
    args: &serde_json::Value,
    ctx: &DagExplorerContext,
) -> Result<String, String> {
    let parsed: AnnotateArgs = serde_json::from_value(args.clone())
        .map_err(|e| format!("invalid dag_annotate args: {e}"))?;

    let mut guard = ctx
        .dag
        .lock()
        .map_err(|_| "DAG store mutex poisoned".to_string())?;

    let node = guard
        .get_mut(parsed.node_id)
        .ok_or_else(|| format!("node not found: {}", parsed.node_id))?;

    node.annotations.push(parsed.note);
    Ok(format!("annotated node {}", parsed.node_id))
}

/// Clear all temporary annotations from every node in the DAG.
///
/// Returns `"cleared annotations from <n> nodes"`.
pub fn dag_clear_annotations(ctx: &DagExplorerContext) -> Result<String, String> {
    let mut guard = ctx
        .dag
        .lock()
        .map_err(|_| "DAG store mutex poisoned".to_string())?;

    let mut count = 0usize;
    for node in guard.nodes.values_mut() {
        if !node.annotations.is_empty() {
            node.annotations.clear();
            count += 1;
        }
    }
    Ok(format!("cleared annotations from {count} nodes"))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn ctx_with(nodes: Vec<DagNode>) -> DagExplorerContext {
        let mut store = DagStore::new();
        for n in nodes {
            store.insert(n);
        }
        DagExplorerContext::new(Arc::new(Mutex::new(store)))
    }

    fn node(id: u64, desc: &str, deps: Vec<u64>, status: NodeStatus) -> DagNode {
        DagNode {
            id,
            description: desc.into(),
            status,
            dependencies: deps,
            issue_number: None,
            annotations: Vec::new(),
        }
    }

    // -----------------------------------------------------------------------
    // CartographerTool catalog
    // -----------------------------------------------------------------------

    #[test]
    fn all_tools_api_name_round_trip() {
        let tools = [
            CartographerTool::DagListNodes,
            CartographerTool::DagGetNode,
            CartographerTool::DagListEdges,
            CartographerTool::DagFindBlocking,
            CartographerTool::DagCriticalPath,
            CartographerTool::DagMarkComplete,
            CartographerTool::DagMarkFailed,
            CartographerTool::DagMarkSkipped,
            CartographerTool::DagAddChild,
            CartographerTool::DagAnnotate,
            CartographerTool::DagClearAnnotations,
        ];
        for t in tools {
            let parsed = CartographerTool::from_api_name(t.api_name());
            assert_eq!(parsed, Some(t), "round-trip failed for {t:?}");
        }
    }

    #[test]
    fn unknown_tool_name_returns_none() {
        assert_eq!(CartographerTool::from_api_name("not_a_dag_tool"), None);
    }

    #[test]
    fn sense_tools_have_sense_class() {
        let sense_tools = [
            CartographerTool::DagListNodes,
            CartographerTool::DagGetNode,
            CartographerTool::DagListEdges,
            CartographerTool::DagFindBlocking,
            CartographerTool::DagCriticalPath,
        ];
        for t in sense_tools {
            assert_eq!(
                t.class(),
                CapabilityClass::Sense,
                "{t:?} must be Sense-class"
            );
        }
    }

    #[test]
    fn coordinate_tools_have_coordinate_class() {
        let coord_tools = [
            CartographerTool::DagMarkComplete,
            CartographerTool::DagMarkFailed,
            CartographerTool::DagMarkSkipped,
            CartographerTool::DagAddChild,
            CartographerTool::DagAnnotate,
            CartographerTool::DagClearAnnotations,
        ];
        for t in coord_tools {
            assert_eq!(
                t.class(),
                CapabilityClass::Coordinate,
                "{t:?} must be Coordinate-class"
            );
        }
    }

    // -----------------------------------------------------------------------
    // NodeStatus
    // -----------------------------------------------------------------------

    #[test]
    fn terminal_statuses_are_terminal() {
        assert!(NodeStatus::Complete.is_terminal());
        assert!(NodeStatus::Failed.is_terminal());
        assert!(NodeStatus::Skipped.is_terminal());
    }

    #[test]
    fn non_terminal_statuses_are_not_terminal() {
        assert!(!NodeStatus::Pending.is_terminal());
        assert!(!NodeStatus::Active.is_terminal());
    }

    // -----------------------------------------------------------------------
    // DagStore
    // -----------------------------------------------------------------------

    #[test]
    fn dag_store_add_node_assigns_unique_ids() {
        let mut store = DagStore::new();
        let id1 = store.add_node("first", vec![]);
        let id2 = store.add_node("second", vec![]);
        assert_ne!(id1, id2);
    }

    #[test]
    fn dag_store_insert_preserves_id() {
        let mut store = DagStore::new();
        let n = DagNode::new(42, "the answer");
        store.insert(n);
        assert_eq!(store.get(42).unwrap().id, 42);
    }

    #[test]
    fn all_nodes_returns_sorted_by_id() {
        let mut store = DagStore::new();
        store.insert(DagNode::new(3, "c"));
        store.insert(DagNode::new(1, "a"));
        store.insert(DagNode::new(2, "b"));
        let ids: Vec<u64> = store.all_nodes().iter().map(|n| n.id).collect();
        assert_eq!(ids, vec![1, 2, 3]);
    }

    #[test]
    fn all_edges_returns_parent_child_pairs() {
        let mut store = DagStore::new();
        store.insert(DagNode::new(1, "root"));
        let mut child = DagNode::new(2, "child");
        child.dependencies = vec![1];
        store.insert(child);
        assert_eq!(store.all_edges(), vec![(1, 2)]);
    }

    // -----------------------------------------------------------------------
    // find_blocking
    // -----------------------------------------------------------------------

    #[test]
    fn find_blocking_returns_pending_ancestors() {
        // #1 (pending) → #2 (pending) → #3 (target)
        let n1 = node(1, "a", vec![], NodeStatus::Pending);
        let n2 = node(2, "b", vec![1], NodeStatus::Pending);
        let n3 = node(3, "target", vec![2], NodeStatus::Pending);
        let ctx = ctx_with(vec![n1, n2, n3]);
        let guard = ctx.dag.lock().unwrap();
        let blockers = guard.find_blocking(3);
        assert!(blockers.contains(&1), "node 1 must block node 3");
        assert!(blockers.contains(&2), "node 2 must block node 3");
        assert!(!blockers.contains(&3), "target must not block itself");
    }

    #[test]
    fn find_blocking_skips_terminal_ancestors() {
        // #1 (complete) → #2 (pending) → #3 (target)
        let n1 = node(1, "done", vec![], NodeStatus::Complete);
        let n2 = node(2, "b", vec![1], NodeStatus::Pending);
        let n3 = node(3, "target", vec![2], NodeStatus::Pending);
        let ctx = ctx_with(vec![n1, n2, n3]);
        let guard = ctx.dag.lock().unwrap();
        let blockers = guard.find_blocking(3);
        // Node 1 is complete — not a blocker.
        assert!(!blockers.contains(&1), "complete node must not appear in blockers");
        assert!(blockers.contains(&2));
    }

    #[test]
    fn find_blocking_empty_when_no_deps() {
        let n1 = node(1, "root", vec![], NodeStatus::Pending);
        let ctx = ctx_with(vec![n1]);
        let guard = ctx.dag.lock().unwrap();
        let blockers = guard.find_blocking(1);
        assert!(blockers.is_empty(), "node with no deps has no blockers");
    }

    // -----------------------------------------------------------------------
    // critical_path
    // -----------------------------------------------------------------------

    #[test]
    fn critical_path_single_node() {
        let n1 = node(1, "solo", vec![], NodeStatus::Pending);
        let ctx = ctx_with(vec![n1]);
        let guard = ctx.dag.lock().unwrap();
        let path = guard.critical_path(1);
        assert_eq!(path, vec![1]);
    }

    #[test]
    fn critical_path_linear_chain() {
        // 1 → 2 → 3 — critical path should include all three
        let n1 = node(1, "a", vec![], NodeStatus::Pending);
        let n2 = node(2, "b", vec![1], NodeStatus::Pending);
        let n3 = node(3, "c", vec![2], NodeStatus::Pending);
        let ctx = ctx_with(vec![n1, n2, n3]);
        let guard = ctx.dag.lock().unwrap();
        let path = guard.critical_path(3);
        assert!(path.contains(&1), "node 1 on critical path");
        assert!(path.contains(&2), "node 2 on critical path");
        assert!(path.contains(&3), "node 3 on critical path");
        assert_eq!(*path.last().unwrap(), 3, "target must be last");
    }

    // -----------------------------------------------------------------------
    // dag_list_nodes handler
    // -----------------------------------------------------------------------

    #[test]
    fn dag_list_nodes_returns_json_array() {
        let n1 = node(1, "alpha", vec![], NodeStatus::Pending);
        let n2 = node(2, "beta", vec![1], NodeStatus::Active);
        let ctx = ctx_with(vec![n1, n2]);
        let json_str = dag_list_nodes(&ctx).expect("list must succeed");
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert!(parsed.is_array(), "must be a JSON array");
        assert_eq!(parsed.as_array().unwrap().len(), 2);
    }

    // -----------------------------------------------------------------------
    // dag_get_node handler
    // -----------------------------------------------------------------------

    #[test]
    fn dag_get_node_returns_node_details() {
        let n1 = node(7, "spec", vec![], NodeStatus::Complete);
        let ctx = ctx_with(vec![n1]);
        let result = dag_get_node(&json!({"node_id": 7}), &ctx).expect("get must succeed");
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["id"].as_u64(), Some(7));
        assert_eq!(parsed["description"].as_str(), Some("spec"));
    }

    #[test]
    fn dag_get_node_missing_returns_err() {
        let ctx = DagExplorerContext::empty();
        let err = dag_get_node(&json!({"node_id": 99}), &ctx).unwrap_err();
        assert!(err.contains("not found"), "error must mention not found: {err}");
        assert!(err.contains("99"), "error must mention the id: {err}");
    }

    // -----------------------------------------------------------------------
    // dag_list_edges handler
    // -----------------------------------------------------------------------

    #[test]
    fn dag_list_edges_returns_pairs() {
        let n1 = node(1, "parent", vec![], NodeStatus::Pending);
        let n2 = node(2, "child", vec![1], NodeStatus::Pending);
        let ctx = ctx_with(vec![n1, n2]);
        let json_str = dag_list_edges(&ctx).expect("list edges must succeed");
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        let arr = parsed.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0][0].as_u64(), Some(1)); // parent
        assert_eq!(arr[0][1].as_u64(), Some(2)); // child
    }

    // -----------------------------------------------------------------------
    // dag_find_blocking handler
    // -----------------------------------------------------------------------

    #[test]
    fn dag_find_blocking_handler_returns_json_array() {
        let n1 = node(1, "a", vec![], NodeStatus::Pending);
        let n2 = node(2, "target", vec![1], NodeStatus::Pending);
        let ctx = ctx_with(vec![n1, n2]);
        let json_str = dag_find_blocking(&json!({"node_id": 2}), &ctx)
            .expect("find_blocking must succeed");
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        let arr = parsed.as_array().unwrap();
        assert!(arr.iter().any(|v| v.as_u64() == Some(1)));
    }

    // -----------------------------------------------------------------------
    // dag_critical_path handler
    // -----------------------------------------------------------------------

    #[test]
    fn dag_critical_path_handler_returns_json_array() {
        let n1 = node(1, "a", vec![], NodeStatus::Pending);
        let n2 = node(2, "b", vec![1], NodeStatus::Pending);
        let ctx = ctx_with(vec![n1, n2]);
        let json_str = dag_critical_path(&json!({"node_id": 2}), &ctx)
            .expect("critical_path must succeed");
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert!(parsed.is_array());
    }

    // -----------------------------------------------------------------------
    // dag_mark_* handlers
    // -----------------------------------------------------------------------

    #[test]
    fn dag_mark_complete_changes_status() {
        let n1 = node(1, "work", vec![], NodeStatus::Active);
        let ctx = ctx_with(vec![n1]);
        dag_mark_complete(&json!({"node_id": 1}), &ctx).expect("mark complete must succeed");
        let guard = ctx.dag.lock().unwrap();
        assert_eq!(guard.get(1).unwrap().status, NodeStatus::Complete);
    }

    #[test]
    fn dag_mark_failed_changes_status() {
        let n1 = node(1, "work", vec![], NodeStatus::Active);
        let ctx = ctx_with(vec![n1]);
        dag_mark_failed(&json!({"node_id": 1}), &ctx).expect("mark failed must succeed");
        let guard = ctx.dag.lock().unwrap();
        assert_eq!(guard.get(1).unwrap().status, NodeStatus::Failed);
    }

    #[test]
    fn dag_mark_skipped_changes_status() {
        let n1 = node(1, "work", vec![], NodeStatus::Pending);
        let ctx = ctx_with(vec![n1]);
        dag_mark_skipped(&json!({"node_id": 1}), &ctx).expect("mark skipped must succeed");
        let guard = ctx.dag.lock().unwrap();
        assert_eq!(guard.get(1).unwrap().status, NodeStatus::Skipped);
    }

    #[test]
    fn dag_mark_complete_missing_node_returns_err() {
        let ctx = DagExplorerContext::empty();
        let err = dag_mark_complete(&json!({"node_id": 99}), &ctx).unwrap_err();
        assert!(err.contains("not found"));
    }

    // -----------------------------------------------------------------------
    // dag_add_child handler
    // -----------------------------------------------------------------------

    #[test]
    fn dag_add_child_creates_new_node_with_dep() {
        let n1 = node(1, "parent", vec![], NodeStatus::Pending);
        let ctx = ctx_with(vec![n1]);
        let msg = dag_add_child(
            &json!({"parent_id": 1, "description": "child task"}),
            &ctx,
        )
        .expect("add_child must succeed");
        assert!(msg.contains("added child node"), "msg: {msg}");
        assert!(msg.contains("1"), "must mention parent: {msg}");

        // The new child must be in the store with dependency on node 1.
        let guard = ctx.dag.lock().unwrap();
        let all: Vec<&DagNode> = guard.all_nodes();
        let child = all.iter().find(|n| n.id != 1).expect("child must exist");
        assert_eq!(child.dependencies, vec![1]);
        assert_eq!(child.description, "child task");
    }

    #[test]
    fn dag_add_child_missing_parent_returns_err() {
        let ctx = DagExplorerContext::empty();
        let err = dag_add_child(
            &json!({"parent_id": 99, "description": "orphan"}),
            &ctx,
        )
        .unwrap_err();
        assert!(err.contains("not found"));
        assert!(err.contains("99"));
    }

    // -----------------------------------------------------------------------
    // dag_annotate handler
    // -----------------------------------------------------------------------

    #[test]
    fn dag_annotate_appends_note() {
        let n1 = node(1, "task", vec![], NodeStatus::Pending);
        let ctx = ctx_with(vec![n1]);
        dag_annotate(&json!({"node_id": 1, "note": "hello"}), &ctx)
            .expect("annotate must succeed");
        dag_annotate(&json!({"node_id": 1, "note": "world"}), &ctx)
            .expect("second annotate must succeed");
        let guard = ctx.dag.lock().unwrap();
        let annotations = &guard.get(1).unwrap().annotations;
        assert_eq!(annotations, &["hello", "world"]);
    }

    #[test]
    fn dag_annotate_missing_node_returns_err() {
        let ctx = DagExplorerContext::empty();
        let err =
            dag_annotate(&json!({"node_id": 42, "note": "foo"}), &ctx).unwrap_err();
        assert!(err.contains("not found"));
    }

    // -----------------------------------------------------------------------
    // dag_clear_annotations handler
    // -----------------------------------------------------------------------

    #[test]
    fn dag_clear_annotations_removes_all_notes() {
        let mut n1 = node(1, "a", vec![], NodeStatus::Pending);
        n1.annotations = vec!["x".into(), "y".into()];
        let mut n2 = node(2, "b", vec![], NodeStatus::Pending);
        n2.annotations = vec!["z".into()];
        let ctx = ctx_with(vec![n1, n2]);

        let msg = dag_clear_annotations(&ctx).expect("clear must succeed");
        assert!(msg.contains("2"), "must report 2 nodes cleared: {msg}");

        let guard = ctx.dag.lock().unwrap();
        for n in guard.all_nodes() {
            assert!(n.annotations.is_empty(), "node {} still has annotations", n.id);
        }
    }

    #[test]
    fn dag_clear_annotations_on_empty_dag_succeeds() {
        let ctx = DagExplorerContext::empty();
        let msg = dag_clear_annotations(&ctx).expect("clear on empty must succeed");
        assert!(msg.contains("0"), "must report 0 nodes cleared: {msg}");
    }

    // -----------------------------------------------------------------------
    // Issue #67 acceptance: capability gate enforces no Act
    // -----------------------------------------------------------------------

    #[test]
    fn cartographer_has_no_act_tools() {
        let all_tools = [
            CartographerTool::DagListNodes,
            CartographerTool::DagGetNode,
            CartographerTool::DagListEdges,
            CartographerTool::DagFindBlocking,
            CartographerTool::DagCriticalPath,
            CartographerTool::DagMarkComplete,
            CartographerTool::DagMarkFailed,
            CartographerTool::DagMarkSkipped,
            CartographerTool::DagAddChild,
            CartographerTool::DagAnnotate,
            CartographerTool::DagClearAnnotations,
        ];
        for t in all_tools {
            assert_ne!(
                t.class(),
                CapabilityClass::Act,
                "Cartographer tool {t:?} must not be Act-class — Cartographer cannot mutate the user's world",
            );
        }
    }
}
