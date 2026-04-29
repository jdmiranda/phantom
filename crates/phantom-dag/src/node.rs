//! [`DagNode`] and [`NodeKind`] — code symbol vertices.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// NodeKind
// ---------------------------------------------------------------------------

/// The syntactic category of a code symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    /// A free function or method.
    Function,
    /// A struct definition.
    Struct,
    /// A trait definition.
    Trait,
    /// A module.
    Module,
    /// A test function (annotated with `#[test]`).
    Test,
}

// ---------------------------------------------------------------------------
// DagNode
// ---------------------------------------------------------------------------

/// A vertex in the code dependency graph, representing a single code symbol.
///
/// All fields are private; use the accessor methods to read them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DagNode {
    /// Fully-qualified symbol path, e.g. `phantom_agents::dispatch::dispatch_tool`.
    id: String,
    /// The syntactic category of this symbol.
    kind: NodeKind,
    /// Source file in which the symbol is defined.
    file: PathBuf,
    /// 1-based line number of the symbol's definition.
    line: u32,
}

impl DagNode {
    /// Construct a new [`DagNode`].
    ///
    /// # Arguments
    ///
    /// * `id`   — Fully-qualified symbol id (e.g. `phantom_agents::dispatch::dispatch_tool`).
    /// * `kind` — Syntactic category.
    /// * `file` — Source file path.
    /// * `line` — 1-based line number.
    #[must_use]
    pub fn new(id: String, kind: NodeKind, file: PathBuf, line: u32) -> Self {
        Self { id, kind, file, line }
    }

    /// The fully-qualified symbol id.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The syntactic category.
    #[must_use]
    pub fn kind(&self) -> &NodeKind {
        &self.kind
    }

    /// Source file in which the symbol is defined.
    #[must_use]
    pub fn file(&self) -> &PathBuf {
        &self.file
    }

    /// 1-based line number of the definition.
    #[must_use]
    pub fn line(&self) -> u32 {
        self.line
    }
}
