//! Permission-based agent sandboxing.
//!
//! Each agent operates under a `PermissionSet` that gates which tools it can
//! invoke. This prevents a read-only code-review agent from writing files, or
//! a documentation agent from running arbitrary shell commands.

use std::collections::HashSet;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::ToolType;

// ---------------------------------------------------------------------------
// Permission enum
// ---------------------------------------------------------------------------

/// Permissions an agent can request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Permission {
    ReadFiles,
    WriteFiles,
    RunCommands,
    Network,
    GitAccess,
}

impl Permission {
    /// All permission variants.
    const ALL: &[Permission] = &[
        Permission::ReadFiles,
        Permission::WriteFiles,
        Permission::RunCommands,
        Permission::Network,
        Permission::GitAccess,
    ];
}

// ---------------------------------------------------------------------------
// PermissionSet
// ---------------------------------------------------------------------------

/// A permission set for an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionSet {
    granted: HashSet<Permission>,
}

impl PermissionSet {
    /// Create a permission set from a slice of permissions.
    pub fn new(perms: &[Permission]) -> Self {
        Self {
            granted: perms.iter().copied().collect(),
        }
    }

    /// All permissions granted.
    pub fn all() -> Self {
        Self {
            granted: Permission::ALL.iter().copied().collect(),
        }
    }

    /// Read-only: `ReadFiles` + `GitAccess` only.
    pub fn read_only() -> Self {
        Self::new(&[Permission::ReadFiles, Permission::GitAccess])
    }

    /// Check whether a specific permission is granted.
    pub fn has(&self, perm: Permission) -> bool {
        self.granted.contains(&perm)
    }

    /// Check if a tool call is allowed under this permission set.
    ///
    /// Returns `Ok(())` if permitted, or `Err(PermissionDenied)` with details.
    pub fn check_tool(&self, tool: &ToolType) -> Result<(), PermissionDenied> {
        let required = required_permission(tool);
        if self.has(required) {
            Ok(())
        } else {
            Err(PermissionDenied {
                tool: *tool,
                required,
                message: format!(
                    "tool `{}` requires {:?} permission, which is not granted",
                    tool.api_name(),
                    required,
                ),
            })
        }
    }
}

// ---------------------------------------------------------------------------
// PermissionDenied
// ---------------------------------------------------------------------------

/// Error returned when a tool call is denied by the permission set.
#[derive(Debug, Clone)]
pub struct PermissionDenied {
    pub tool: ToolType,
    pub required: Permission,
    pub message: String,
}

impl fmt::Display for PermissionDenied {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for PermissionDenied {}

// ---------------------------------------------------------------------------
// Tool -> Permission mapping
// ---------------------------------------------------------------------------

/// Map a tool type to the permission required to execute it.
pub fn required_permission(tool: &ToolType) -> Permission {
    match tool {
        ToolType::ReadFile | ToolType::SearchFiles | ToolType::ListFiles => Permission::ReadFiles,
        ToolType::WriteFile => Permission::WriteFiles,
        ToolType::RunCommand => Permission::RunCommands,
        ToolType::GitStatus | ToolType::GitDiff => Permission::GitAccess,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_grants_specified_permissions() {
        let set = PermissionSet::new(&[Permission::ReadFiles, Permission::Network]);
        assert!(set.has(Permission::ReadFiles));
        assert!(set.has(Permission::Network));
        assert!(!set.has(Permission::WriteFiles));
        assert!(!set.has(Permission::RunCommands));
        assert!(!set.has(Permission::GitAccess));
    }

    #[test]
    fn all_grants_every_permission() {
        let set = PermissionSet::all();
        for &perm in Permission::ALL {
            assert!(set.has(perm), "all() should grant {perm:?}");
        }
    }

    #[test]
    fn read_only_grants_read_and_git() {
        let set = PermissionSet::read_only();
        assert!(set.has(Permission::ReadFiles));
        assert!(set.has(Permission::GitAccess));
        assert!(!set.has(Permission::WriteFiles));
        assert!(!set.has(Permission::RunCommands));
        assert!(!set.has(Permission::Network));
    }

    #[test]
    fn check_tool_read_file_allowed() {
        let set = PermissionSet::new(&[Permission::ReadFiles]);
        assert!(set.check_tool(&ToolType::ReadFile).is_ok());
    }

    #[test]
    fn check_tool_write_file_denied() {
        let set = PermissionSet::read_only();
        let err = set.check_tool(&ToolType::WriteFile).unwrap_err();
        assert_eq!(err.tool, ToolType::WriteFile);
        assert_eq!(err.required, Permission::WriteFiles);
        assert!(err.message.contains("write_file"));
    }

    #[test]
    fn check_tool_run_command_denied() {
        let set = PermissionSet::new(&[Permission::ReadFiles]);
        let err = set.check_tool(&ToolType::RunCommand).unwrap_err();
        assert_eq!(err.required, Permission::RunCommands);
    }

    #[test]
    fn check_tool_search_files_uses_read_permission() {
        let set = PermissionSet::new(&[Permission::ReadFiles]);
        assert!(set.check_tool(&ToolType::SearchFiles).is_ok());
    }

    #[test]
    fn check_tool_list_files_uses_read_permission() {
        let set = PermissionSet::new(&[Permission::ReadFiles]);
        assert!(set.check_tool(&ToolType::ListFiles).is_ok());
    }

    #[test]
    fn check_tool_git_status_and_diff() {
        let set = PermissionSet::new(&[Permission::GitAccess]);
        assert!(set.check_tool(&ToolType::GitStatus).is_ok());
        assert!(set.check_tool(&ToolType::GitDiff).is_ok());
    }

    #[test]
    fn all_tools_allowed_with_all_permissions() {
        let set = PermissionSet::all();
        let tools = [
            ToolType::ReadFile,
            ToolType::WriteFile,
            ToolType::RunCommand,
            ToolType::SearchFiles,
            ToolType::GitStatus,
            ToolType::GitDiff,
            ToolType::ListFiles,
        ];
        for tool in &tools {
            assert!(
                set.check_tool(tool).is_ok(),
                "all() should allow {:?}",
                tool,
            );
        }
    }
}
