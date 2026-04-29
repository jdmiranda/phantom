//! JSON persistence for [`CodeDag`] — the `.planning/dag.json` wire format.
//!
//! Schema (version 1):
//! ```json
//! {
//!   "version": 1,
//!   "generated_at": "<ISO 8601 UTC timestamp>",
//!   "nodes": [ { "id": "...", "kind": "function", "file": "...", "line": 1 } ],
//!   "edges": [ { "from": "...", "to": "...", "kind": "calls" } ]
//! }
//! ```

use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::{CodeDag, DagEdge, DagNode};

/// Current schema version written to `dag.json`.
const SCHEMA_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// Top-level JSON envelope for `.planning/dag.json`.
#[derive(Serialize, Deserialize)]
struct DagFile {
    version: u32,
    generated_at: String,
    nodes: Vec<DagNode>,
    edges: Vec<DagEdge>,
}

// ---------------------------------------------------------------------------
// Public helpers (crate-private)
// ---------------------------------------------------------------------------

/// Serialise a [`CodeDag`] into the `.planning/dag.json` JSON format.
pub(crate) fn to_json(dag: &CodeDag) -> Result<String> {
    let file = DagFile {
        version: SCHEMA_VERSION,
        generated_at: Utc::now().to_rfc3339(),
        nodes: dag.nodes().cloned().collect(),
        edges: dag.edges().cloned().collect(),
    };
    serde_json::to_string_pretty(&file).context("failed to serialise CodeDag to JSON")
}

/// Deserialise a [`CodeDag`] from the `.planning/dag.json` JSON format.
pub(crate) fn from_json(json: &str) -> Result<CodeDag> {
    let file: DagFile =
        serde_json::from_str(json).context("failed to parse dag.json — invalid JSON or schema")?;

    if file.version != SCHEMA_VERSION {
        bail!(
            "unsupported dag.json schema version {} (expected {})",
            file.version,
            SCHEMA_VERSION
        );
    }

    let mut dag = CodeDag::new();
    for node in file.nodes {
        dag.add_node(node);
    }
    for edge in file.edges {
        dag.add_edge(edge);
    }
    Ok(dag)
}
