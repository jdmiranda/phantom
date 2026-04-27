//! Agent tool definitions and execution.
//!
//! Tools are the bridge between AI reasoning and real-world effects.
//! Every tool call is sandboxed to a `working_dir` — no path traversal,
//! no absolute paths, no escape.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

// ---------------------------------------------------------------------------
// ToolType
// ---------------------------------------------------------------------------

/// Tools available to agents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ToolType {
    ReadFile,
    WriteFile,
    EditFile,
    RunCommand,
    SearchFiles,
    GitStatus,
    GitDiff,
    ListFiles,
}

impl ToolType {
    /// The wire name sent to the Claude API.
    pub fn api_name(&self) -> &'static str {
        match self {
            Self::ReadFile => "read_file",
            Self::WriteFile => "write_file",
            Self::EditFile => "edit_file",
            Self::RunCommand => "run_command",
            Self::SearchFiles => "search_files",
            Self::GitStatus => "git_status",
            Self::GitDiff => "git_diff",
            Self::ListFiles => "list_files",
        }
    }

    /// Parse from the wire name returned by the Claude API.
    pub fn from_api_name(name: &str) -> Option<Self> {
        match name {
            "read_file" => Some(Self::ReadFile),
            "write_file" => Some(Self::WriteFile),
            "edit_file" => Some(Self::EditFile),
            "run_command" => Some(Self::RunCommand),
            "search_files" => Some(Self::SearchFiles),
            "git_status" => Some(Self::GitStatus),
            "git_diff" => Some(Self::GitDiff),
            "list_files" => Some(Self::ListFiles),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// ToolCall / ToolResult
// ---------------------------------------------------------------------------

/// A tool the agent wants to invoke.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub tool: ToolType,
    pub args: serde_json::Value,
}

/// Result of a tool execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub tool: ToolType,
    pub success: bool,
    pub output: String,
}

// ---------------------------------------------------------------------------
// ToolDefinition (sent to AI model in API requests)
// ---------------------------------------------------------------------------

/// Tool definition for the AI model (included in API requests).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Get all available tool definitions for the AI model.
pub fn available_tools() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "read_file".into(),
            description: "Read the contents of a file relative to the project root.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Relative path to the file to read."
                    }
                },
                "required": ["path"]
            }),
        },
        ToolDefinition {
            name: "write_file".into(),
            description: "Write content to a file relative to the project root.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Relative path to the file to write."
                    },
                    "content": {
                        "type": "string",
                        "description": "Content to write to the file."
                    }
                },
                "required": ["path", "content"]
            }),
        },
        ToolDefinition {
            name: "edit_file".into(),
            description: "Replace a specific text string in a file. The old_text must match exactly one location in the file. Use this for surgical edits instead of rewriting entire files.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Relative path to the file to edit."
                    },
                    "old_text": {
                        "type": "string",
                        "description": "The exact text to find and replace. Must match exactly one location."
                    },
                    "new_text": {
                        "type": "string",
                        "description": "The replacement text."
                    }
                },
                "required": ["path", "old_text", "new_text"]
            }),
        },
        ToolDefinition {
            name: "run_command".into(),
            description: "Execute a shell command in the project directory.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command to execute."
                    }
                },
                "required": ["command"]
            }),
        },
        ToolDefinition {
            name: "search_files".into(),
            description: "Search for files matching a glob pattern in the project.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Glob pattern to match (e.g. '**/*.rs')."
                    }
                },
                "required": ["pattern"]
            }),
        },
        ToolDefinition {
            name: "git_status".into(),
            description: "Run `git status --porcelain` in the project directory.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
        },
        ToolDefinition {
            name: "git_diff".into(),
            description: "Run `git diff` in the project directory.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
        },
        ToolDefinition {
            name: "list_files".into(),
            description: "List directory contents relative to the project root.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Relative path to the directory. Defaults to '.' (project root)."
                    }
                }
            }),
        },
    ]
}

// ---------------------------------------------------------------------------
// Path sandboxing
// ---------------------------------------------------------------------------

/// Maximum file size we will read (50 KB).
const MAX_READ_SIZE: u64 = 50 * 1024;

/// Command execution timeout.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

/// Resolve a relative path within `working_dir`, rejecting path traversal.
///
/// Returns `Err` if the path contains `..`, is absolute, or escapes the sandbox.
fn sandbox_path(working_dir: &Path, relative: &str) -> Result<PathBuf, String> {
    let relative = relative.trim();

    // Reject absolute paths.
    if Path::new(relative).is_absolute() {
        return Err(format!("absolute paths are not allowed: {relative}"));
    }

    // Reject path traversal components.
    if relative.contains("..") {
        return Err(format!("path traversal is not allowed: {relative}"));
    }

    let resolved = working_dir.join(relative);

    // Belt-and-suspenders: canonicalize both paths and verify containment.
    let canon_root = working_dir
        .canonicalize()
        .map_err(|e| format!("cannot canonicalize working dir: {e}"))?;

    // For the resolved path, try canonicalizing it directly first. If that
    // fails (file doesn't exist yet), canonicalize the parent and append the
    // file name.
    let canon_resolved = if resolved.exists() {
        resolved
            .canonicalize()
            .map_err(|e| format!("cannot canonicalize path: {e}"))?
    } else {
        let parent = resolved
            .parent()
            .ok_or_else(|| "invalid path: no parent".to_string())?;
        if !parent.exists() {
            return Err(format!(
                "parent directory does not exist: {}",
                parent.display()
            ));
        }
        let canon_parent = parent
            .canonicalize()
            .map_err(|e| format!("cannot canonicalize parent: {e}"))?;
        let file_name = resolved
            .file_name()
            .ok_or_else(|| "invalid path: no file name".to_string())?;
        canon_parent.join(file_name)
    };

    if !canon_resolved.starts_with(&canon_root) {
        return Err(format!(
            "path escapes sandbox: {}",
            canon_resolved.display()
        ));
    }

    Ok(canon_resolved)
}

// ---------------------------------------------------------------------------
// Tool execution
// ---------------------------------------------------------------------------

/// Execute a tool call and return the result.
///
/// `working_dir` is the project root; all file operations are sandboxed to it.
pub fn execute_tool(tool: ToolType, args: &serde_json::Value, working_dir: &str) -> ToolResult {
    let root = Path::new(working_dir);

    match tool {
        ToolType::ReadFile => execute_read_file(root, args),
        ToolType::WriteFile => execute_write_file(root, args),
        ToolType::EditFile => execute_edit_file(root, args),
        ToolType::RunCommand => execute_run_command(root, args),
        ToolType::SearchFiles => execute_search_files(root, args),
        ToolType::GitStatus => execute_git_status(root),
        ToolType::GitDiff => execute_git_diff(root),
        ToolType::ListFiles => execute_list_files(root, args),
    }
}

fn tool_err(tool: ToolType, msg: String) -> ToolResult {
    ToolResult {
        tool,
        success: false,
        output: msg,
    }
}

fn tool_ok(tool: ToolType, output: String) -> ToolResult {
    ToolResult {
        tool,
        success: true,
        output,
    }
}

// ---------------------------------------------------------------------------
// Individual tool implementations
// ---------------------------------------------------------------------------

fn execute_read_file(root: &Path, args: &serde_json::Value) -> ToolResult {
    let tool = ToolType::ReadFile;

    let Some(path_str) = args.get("path").and_then(|v| v.as_str()) else {
        return tool_err(tool, "missing required parameter: path".into());
    };

    let resolved = match sandbox_path(root, path_str) {
        Ok(p) => p,
        Err(e) => return tool_err(tool, e),
    };

    match fs::metadata(&resolved) {
        Ok(meta) if meta.len() > MAX_READ_SIZE => {
            return tool_err(
                tool,
                format!(
                    "file too large: {} bytes (max {})",
                    meta.len(),
                    MAX_READ_SIZE
                ),
            );
        }
        Err(e) => return tool_err(tool, format!("cannot stat file: {e}")),
        _ => {}
    }

    let mut file = match fs::File::open(&resolved) {
        Ok(f) => f,
        Err(e) => return tool_err(tool, format!("cannot open file: {e}")),
    };

    let mut contents = String::new();
    match file.read_to_string(&mut contents) {
        Ok(_) => tool_ok(tool, contents),
        Err(e) => tool_err(tool, format!("cannot read file: {e}")),
    }
}

fn execute_write_file(root: &Path, args: &serde_json::Value) -> ToolResult {
    let tool = ToolType::WriteFile;

    let Some(path_str) = args.get("path").and_then(|v| v.as_str()) else {
        return tool_err(tool, "missing required parameter: path".into());
    };
    let Some(content) = args.get("content").and_then(|v| v.as_str()) else {
        return tool_err(tool, "missing required parameter: content".into());
    };

    // For write, the file may not exist. We need the parent to exist for
    // sandbox_path canonicalization. Create parent dirs first if needed,
    // but only after validating the path doesn't traverse.
    if Path::new(path_str).is_absolute() {
        return tool_err(tool, format!("absolute paths are not allowed: {path_str}"));
    }
    if path_str.contains("..") {
        return tool_err(tool, format!("path traversal is not allowed: {path_str}"));
    }

    let target = root.join(path_str);

    // Ensure parent directory exists.
    if let Some(parent) = target.parent() {
        if !parent.exists() {
            if let Err(e) = fs::create_dir_all(parent) {
                return tool_err(tool, format!("cannot create directories: {e}"));
            }
        }
    }

    // Now sandbox_path will succeed since parent exists.
    let resolved = match sandbox_path(root, path_str) {
        Ok(p) => p,
        Err(e) => return tool_err(tool, e),
    };

    match fs::write(&resolved, content) {
        Ok(()) => tool_ok(
            tool,
            format!("wrote {} bytes to {path_str}", content.len()),
        ),
        Err(e) => tool_err(tool, format!("cannot write file: {e}")),
    }
}

fn execute_edit_file(root: &Path, args: &serde_json::Value) -> ToolResult {
    let tool = ToolType::EditFile;

    let Some(path_str) = args.get("path").and_then(|v| v.as_str()) else {
        return tool_err(tool, "missing required parameter: path".into());
    };
    let Some(old_text) = args.get("old_text").and_then(|v| v.as_str()) else {
        return tool_err(tool, "missing required parameter: old_text".into());
    };
    let Some(new_text) = args.get("new_text").and_then(|v| v.as_str()) else {
        return tool_err(tool, "missing required parameter: new_text".into());
    };

    let resolved = match sandbox_path(root, path_str) {
        Ok(p) => p,
        Err(e) => return tool_err(tool, e),
    };

    let content = match fs::read_to_string(&resolved) {
        Ok(c) => c,
        Err(e) => return tool_err(tool, format!("cannot read file: {e}")),
    };

    let count = content.matches(old_text).count();
    if count == 0 {
        return tool_err(
            tool,
            format!("old_text not found in {path_str}. The text to replace must match exactly."),
        );
    }
    if count > 1 {
        return tool_err(
            tool,
            format!(
                "old_text matches {count} locations in {path_str}. Be more specific to match exactly one."
            ),
        );
    }

    let replaced = content.replacen(old_text, new_text, 1);
    match fs::write(&resolved, &replaced) {
        Ok(()) => tool_ok(
            tool,
            format!(
                "edited {path_str}: replaced {} bytes with {} bytes",
                old_text.len(),
                new_text.len()
            ),
        ),
        Err(e) => tool_err(tool, format!("cannot write file: {e}")),
    }
}

fn execute_run_command(root: &Path, args: &serde_json::Value) -> ToolResult {
    let tool = ToolType::RunCommand;

    let Some(command_str) = args.get("command").and_then(|v| v.as_str()) else {
        return tool_err(tool, "missing required parameter: command".into());
    };

    let child = Command::new("sh")
        .arg("-c")
        .arg(command_str)
        .current_dir(root)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn();

    let mut child = match child {
        Ok(c) => c,
        Err(e) => return tool_err(tool, format!("cannot spawn command: {e}")),
    };

    match child.wait_timeout(COMMAND_TIMEOUT) {
        Ok(Some(status)) => {
            let stdout = child
                .stdout
                .take()
                .map(|mut s| {
                    let mut buf = String::new();
                    let _ = s.read_to_string(&mut buf);
                    buf
                })
                .unwrap_or_default();

            let stderr = child
                .stderr
                .take()
                .map(|mut s| {
                    let mut buf = String::new();
                    let _ = s.read_to_string(&mut buf);
                    buf
                })
                .unwrap_or_default();

            let mut output = String::new();
            if !stdout.is_empty() {
                output.push_str(&stdout);
            }
            if !stderr.is_empty() {
                if !output.is_empty() {
                    output.push('\n');
                }
                output.push_str("STDERR:\n");
                output.push_str(&stderr);
            }

            if status.success() {
                tool_ok(tool, output)
            } else {
                tool_err(tool, format!("command exited with {status}\n{output}"))
            }
        }
        Ok(None) => {
            let _ = child.kill();
            tool_err(tool, format!("command timed out after {COMMAND_TIMEOUT:?}"))
        }
        Err(e) => tool_err(tool, format!("error waiting for command: {e}")),
    }
}

fn execute_search_files(root: &Path, args: &serde_json::Value) -> ToolResult {
    let tool = ToolType::SearchFiles;

    let Some(pattern_str) = args.get("pattern").and_then(|v| v.as_str()) else {
        return tool_err(tool, "missing required parameter: pattern".into());
    };

    let full_pattern = format!("{}/{pattern_str}", root.display());

    let matches: Vec<String> = match glob::glob(&full_pattern) {
        Ok(paths) => paths
            .filter_map(|entry| entry.ok())
            .filter_map(|path| {
                path.strip_prefix(root)
                    .ok()
                    .map(|rel| rel.display().to_string())
            })
            .collect(),
        Err(e) => return tool_err(tool, format!("invalid glob pattern: {e}")),
    };

    if matches.is_empty() {
        tool_ok(tool, "no files matched".into())
    } else {
        tool_ok(tool, matches.join("\n"))
    }
}

fn execute_git_status(root: &Path) -> ToolResult {
    let tool = ToolType::GitStatus;

    match Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(root)
        .output()
    {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            if output.status.success() {
                tool_ok(tool, stdout)
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
                tool_err(tool, format!("git status failed: {stderr}"))
            }
        }
        Err(e) => tool_err(tool, format!("cannot run git: {e}")),
    }
}

fn execute_git_diff(root: &Path) -> ToolResult {
    let tool = ToolType::GitDiff;

    match Command::new("git")
        .arg("diff")
        .current_dir(root)
        .output()
    {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            if output.status.success() {
                tool_ok(tool, stdout)
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
                tool_err(tool, format!("git diff failed: {stderr}"))
            }
        }
        Err(e) => tool_err(tool, format!("cannot run git: {e}")),
    }
}

fn execute_list_files(root: &Path, args: &serde_json::Value) -> ToolResult {
    let tool = ToolType::ListFiles;

    let path_str = args
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or(".");

    let resolved = match sandbox_path(root, path_str) {
        Ok(p) => p,
        Err(e) => return tool_err(tool, e),
    };

    if !resolved.is_dir() {
        return tool_err(tool, format!("not a directory: {path_str}"));
    }

    let entries: Vec<String> = match fs::read_dir(&resolved) {
        Ok(iter) => iter
            .filter_map(|entry| entry.ok())
            .map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                let meta = entry.metadata().ok();
                let suffix = if meta.as_ref().is_some_and(|m| m.is_dir()) {
                    "/"
                } else {
                    ""
                };
                format!("{name}{suffix}")
            })
            .collect(),
        Err(e) => return tool_err(tool, format!("cannot read directory: {e}")),
    };

    let mut sorted = entries;
    sorted.sort();
    tool_ok(tool, sorted.join("\n"))
}

// ---------------------------------------------------------------------------
// Wait-with-timeout helper for std::process::Child
// ---------------------------------------------------------------------------

trait ChildExt {
    fn wait_timeout(
        &mut self,
        timeout: Duration,
    ) -> std::io::Result<Option<std::process::ExitStatus>>;
}

impl ChildExt for std::process::Child {
    fn wait_timeout(
        &mut self,
        timeout: Duration,
    ) -> std::io::Result<Option<std::process::ExitStatus>> {
        let start = std::time::Instant::now();
        let poll_interval = Duration::from_millis(50);

        loop {
            match self.try_wait()? {
                Some(status) => return Ok(Some(status)),
                None => {
                    if start.elapsed() >= timeout {
                        return Ok(None);
                    }
                    std::thread::sleep(poll_interval);
                }
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
    use tempfile::TempDir;

    // -- ToolType API name round-trips --------------------------------------

    #[test]
    fn tool_type_api_name_round_trip() {
        let types = [
            ToolType::ReadFile,
            ToolType::WriteFile,
            ToolType::EditFile,
            ToolType::RunCommand,
            ToolType::SearchFiles,
            ToolType::GitStatus,
            ToolType::GitDiff,
            ToolType::ListFiles,
        ];
        for t in &types {
            let name = t.api_name();
            let parsed = ToolType::from_api_name(name);
            assert_eq!(parsed.as_ref(), Some(t), "round-trip failed for {name}");
        }
    }

    #[test]
    fn tool_type_unknown_returns_none() {
        assert_eq!(ToolType::from_api_name("nonexistent_tool"), None);
    }

    // -- Sandboxing tests ---------------------------------------------------

    #[test]
    fn sandbox_rejects_absolute_path() {
        let tmp = TempDir::new().unwrap();
        let result = sandbox_path(tmp.path(), "/etc/passwd");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("absolute paths"));
    }

    #[test]
    fn sandbox_rejects_path_traversal() {
        let tmp = TempDir::new().unwrap();
        let result = sandbox_path(tmp.path(), "../../../etc/passwd");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("path traversal"));
    }

    #[test]
    fn sandbox_rejects_hidden_traversal() {
        let tmp = TempDir::new().unwrap();
        let result = sandbox_path(tmp.path(), "foo/../../bar");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("path traversal"));
    }

    #[test]
    fn sandbox_accepts_valid_relative_path() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("hello.txt"), "world").unwrap();
        let result = sandbox_path(tmp.path(), "hello.txt");
        assert!(result.is_ok());
    }

    #[test]
    fn sandbox_accepts_nested_path() {
        let tmp = TempDir::new().unwrap();
        let sub = tmp.path().join("a/b");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("c.txt"), "deep").unwrap();
        let result = sandbox_path(tmp.path(), "a/b/c.txt");
        assert!(result.is_ok());
    }

    // -- ReadFile tests -----------------------------------------------------

    #[test]
    fn read_file_succeeds() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("test.txt"), "hello phantom").unwrap();

        let result = execute_tool(
            ToolType::ReadFile,
            &serde_json::json!({ "path": "test.txt" }),
            tmp.path().to_str().unwrap(),
        );
        assert!(result.success);
        assert_eq!(result.output, "hello phantom");
    }

    #[test]
    fn read_file_rejects_traversal() {
        let tmp = TempDir::new().unwrap();
        let result = execute_tool(
            ToolType::ReadFile,
            &serde_json::json!({ "path": "../secret.txt" }),
            tmp.path().to_str().unwrap(),
        );
        assert!(!result.success);
        assert!(result.output.contains("path traversal"));
    }

    #[test]
    fn read_file_rejects_missing_param() {
        let tmp = TempDir::new().unwrap();
        let result = execute_tool(
            ToolType::ReadFile,
            &serde_json::json!({}),
            tmp.path().to_str().unwrap(),
        );
        assert!(!result.success);
        assert!(result.output.contains("missing"));
    }

    // -- WriteFile tests ----------------------------------------------------

    #[test]
    fn write_file_creates_and_writes() {
        let tmp = TempDir::new().unwrap();
        let result = execute_tool(
            ToolType::WriteFile,
            &serde_json::json!({ "path": "out.txt", "content": "written" }),
            tmp.path().to_str().unwrap(),
        );
        assert!(result.success, "write failed: {}", result.output);

        let contents = fs::read_to_string(tmp.path().join("out.txt")).unwrap();
        assert_eq!(contents, "written");
    }

    #[test]
    fn write_file_creates_parent_dirs() {
        let tmp = TempDir::new().unwrap();
        let result = execute_tool(
            ToolType::WriteFile,
            &serde_json::json!({ "path": "sub/dir/file.txt", "content": "nested" }),
            tmp.path().to_str().unwrap(),
        );
        assert!(result.success, "write failed: {}", result.output);

        let contents = fs::read_to_string(tmp.path().join("sub/dir/file.txt")).unwrap();
        assert_eq!(contents, "nested");
    }

    #[test]
    fn write_file_rejects_traversal() {
        let tmp = TempDir::new().unwrap();
        let result = execute_tool(
            ToolType::WriteFile,
            &serde_json::json!({ "path": "../evil.txt", "content": "pwned" }),
            tmp.path().to_str().unwrap(),
        );
        assert!(!result.success);
    }

    // -- RunCommand tests ---------------------------------------------------

    #[test]
    fn run_command_captures_stdout() {
        let tmp = TempDir::new().unwrap();
        let result = execute_tool(
            ToolType::RunCommand,
            &serde_json::json!({ "command": "echo hello" }),
            tmp.path().to_str().unwrap(),
        );
        assert!(result.success);
        assert!(result.output.contains("hello"));
    }

    #[test]
    fn run_command_reports_failure() {
        let tmp = TempDir::new().unwrap();
        let result = execute_tool(
            ToolType::RunCommand,
            &serde_json::json!({ "command": "false" }),
            tmp.path().to_str().unwrap(),
        );
        assert!(!result.success);
    }

    // -- SearchFiles tests --------------------------------------------------

    #[test]
    fn search_files_finds_matches() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("a.rs"), "").unwrap();
        fs::write(tmp.path().join("b.rs"), "").unwrap();
        fs::write(tmp.path().join("c.txt"), "").unwrap();

        let result = execute_tool(
            ToolType::SearchFiles,
            &serde_json::json!({ "pattern": "*.rs" }),
            tmp.path().to_str().unwrap(),
        );
        assert!(result.success);
        assert!(result.output.contains("a.rs"));
        assert!(result.output.contains("b.rs"));
        assert!(!result.output.contains("c.txt"));
    }

    // -- ListFiles tests ----------------------------------------------------

    #[test]
    fn list_files_shows_directory_contents() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("alpha.txt"), "").unwrap();
        fs::create_dir(tmp.path().join("beta")).unwrap();

        let result = execute_tool(
            ToolType::ListFiles,
            &serde_json::json!({ "path": "." }),
            tmp.path().to_str().unwrap(),
        );
        assert!(result.success);
        assert!(result.output.contains("alpha.txt"));
        assert!(result.output.contains("beta/"));
    }

    // -- GitStatus tests ----------------------------------------------------

    #[test]
    fn git_status_in_non_repo_fails() {
        let tmp = TempDir::new().unwrap();
        let result = execute_tool(
            ToolType::GitStatus,
            &serde_json::json!({}),
            tmp.path().to_str().unwrap(),
        );
        assert!(!result.success);
    }

    // -- ToolDefinition tests -----------------------------------------------

    #[test]
    fn available_tools_returns_all_eight() {
        let tools = available_tools();
        assert_eq!(tools.len(), 8);

        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"write_file"));
        assert!(names.contains(&"edit_file"));
        assert!(names.contains(&"run_command"));
        assert!(names.contains(&"search_files"));
        assert!(names.contains(&"git_status"));
        assert!(names.contains(&"git_diff"));
        assert!(names.contains(&"list_files"));
    }

    // -- EditFile tests ------------------------------------------------------

    #[test]
    fn edit_file_replaces_unique_match() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("test.rs"),
            "fn main() {\n    println!(\"hello\");\n}\n",
        )
        .unwrap();

        let result = execute_tool(
            ToolType::EditFile,
            &serde_json::json!({
                "path": "test.rs",
                "old_text": "println!(\"hello\")",
                "new_text": "println!(\"world\")"
            }),
            tmp.path().to_str().unwrap(),
        );
        assert!(result.success, "edit failed: {}", result.output);

        let content = fs::read_to_string(tmp.path().join("test.rs")).unwrap();
        assert!(content.contains("println!(\"world\")"));
        assert!(!content.contains("println!(\"hello\")"));
    }

    #[test]
    fn edit_file_fails_on_no_match() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("test.rs"), "fn main() {}").unwrap();

        let result = execute_tool(
            ToolType::EditFile,
            &serde_json::json!({
                "path": "test.rs",
                "old_text": "nonexistent text",
                "new_text": "replacement"
            }),
            tmp.path().to_str().unwrap(),
        );
        assert!(!result.success);
        assert!(result.output.contains("not found"));
    }

    #[test]
    fn edit_file_fails_on_multiple_matches() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("test.rs"), "aaa\naaa\naaa\n").unwrap();

        let result = execute_tool(
            ToolType::EditFile,
            &serde_json::json!({
                "path": "test.rs",
                "old_text": "aaa",
                "new_text": "bbb"
            }),
            tmp.path().to_str().unwrap(),
        );
        assert!(!result.success);
        assert!(result.output.contains("matches"));
    }

    #[test]
    fn edit_file_rejects_traversal() {
        let tmp = TempDir::new().unwrap();
        let result = execute_tool(
            ToolType::EditFile,
            &serde_json::json!({
                "path": "../evil.rs",
                "old_text": "x",
                "new_text": "y"
            }),
            tmp.path().to_str().unwrap(),
        );
        assert!(!result.success);
    }

    // -- ToolDefinition tests -----------------------------------------------

    #[test]
    fn tool_definitions_have_valid_json_schema() {
        for tool in available_tools() {
            assert!(
                tool.parameters.is_object(),
                "tool {} has non-object params",
                tool.name
            );
            assert!(
                tool.parameters.get("type").is_some(),
                "tool {} params missing 'type' field",
                tool.name
            );
        }
    }
}
