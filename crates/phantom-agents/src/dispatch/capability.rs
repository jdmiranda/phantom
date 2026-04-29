//! Capability gating helpers for the dispatch layer.

use crate::role::{AgentRole, CapabilityClass};
use crate::tools::ToolType;

/// Default-deny gate. Returns `Ok(())` iff `role`'s manifest declares
/// `class`; otherwise returns the canonical
/// `"capability denied: <Class> not in <Role> manifest"` message the model
/// sees in its next `tool_result` block. Pinning the wording here keeps the
/// API contract stable across all three dispatch forks.
pub(super) fn check_capability(role: AgentRole, class: CapabilityClass) -> Result<(), String> {
    if role.has(class) {
        Ok(())
    } else {
        Err(format!(
            "capability denied: {class:?} not in {role:?} manifest"
        ))
    }
}

/// Map a [`ToolType`] to its [`CapabilityClass`].
///
/// Read-only inspectors (file reads, listings, git status/diff) are
/// `Sense`. Mutators (file writes, edits, shell commands) are `Act`. Local
/// to this module so the `tools` crate doesn't need a per-tool class
/// declaration — the dispatch surface is the only place that intersects
/// against role manifests.
pub(super) fn class_for(tool: ToolType) -> CapabilityClass {
    match tool {
        ToolType::ReadFile
        | ToolType::SearchFiles
        | ToolType::ListFiles
        | ToolType::GitStatus
        | ToolType::GitDiff => CapabilityClass::Sense,
        ToolType::WriteFile | ToolType::EditFile | ToolType::RunCommand => CapabilityClass::Act,
    }
}
