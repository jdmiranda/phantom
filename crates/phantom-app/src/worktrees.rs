//! First-class worktree primitive API.
//!
//! Wraps `git worktree` via `std::process::Command` so Phantom orchestration
//! tooling can create, list, and remove worktrees through a typed Rust API
//! instead of shell-outs. Worktrees are placed under
//! `<repo_root>/.phantom/worktrees/<branch-sanitized>`, mirroring the shape
//! Claude Code uses for `.claude/worktrees/`.
//!
//! This module is intentionally a small, self-contained slice. The forever
//! script migration is follow-up work and is not part of this change.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A handle to a worktree this API created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeHandle {
    pub branch: String,
    pub path: PathBuf,
    pub base_ref: String,
}

/// One row parsed from `git worktree list --porcelain`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeInfo {
    pub branch: Option<String>,
    pub path: PathBuf,
    pub head: Option<String>,
    pub is_bare: bool,
    pub is_detached: bool,
}

/// Errors produced by the worktree primitive.
#[derive(Debug)]
pub enum WorktreeError {
    InvalidBranchName(String),
    GitCommandFailed {
        command: String,
        status: Option<i32>,
        stderr: String,
    },
    IoError(io::Error),
    ParseError(String),
}

impl fmt::Display for WorktreeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WorktreeError::InvalidBranchName(b) => {
                write!(f, "invalid branch name: {b:?}")
            }
            WorktreeError::GitCommandFailed { command, status, stderr } => {
                let code = status
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "<no exit code>".to_string());
                write!(
                    f,
                    "git command failed (status {code}): {command}\nstderr: {stderr}"
                )
            }
            WorktreeError::IoError(e) => write!(f, "io error: {e}"),
            WorktreeError::ParseError(msg) => write!(f, "parse error: {msg}"),
        }
    }
}

impl std::error::Error for WorktreeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            WorktreeError::IoError(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for WorktreeError {
    fn from(e: io::Error) -> Self {
        WorktreeError::IoError(e)
    }
}

/// Validate a branch name and return the on-disk directory component.
///
/// Rejects empty, leading `-`, `..` components, NUL bytes, and characters
/// outside the `[A-Za-z0-9._/+@-]` allowlist. Replaces `/` with `-` on the
/// returned directory component so the worktree fits in a single subdir.
fn sanitize_branch(branch: &str) -> Result<String, WorktreeError> {
    let trimmed = branch.trim();
    if trimmed.is_empty() {
        return Err(WorktreeError::InvalidBranchName(branch.to_string()));
    }
    if trimmed.starts_with('-') {
        return Err(WorktreeError::InvalidBranchName(branch.to_string()));
    }
    if trimmed.contains('\0') {
        return Err(WorktreeError::InvalidBranchName(branch.to_string()));
    }
    for component in trimmed.split('/') {
        if component == ".." {
            return Err(WorktreeError::InvalidBranchName(branch.to_string()));
        }
    }
    for c in trimmed.chars() {
        let ok = c.is_ascii_alphanumeric()
            || matches!(c, '.' | '_' | '/' | '+' | '@' | '-');
        if !ok {
            return Err(WorktreeError::InvalidBranchName(branch.to_string()));
        }
    }
    Ok(trimmed.replace('/', "-"))
}

/// Run `git -C <repo_root> <args...>` and return stdout on success.
fn run_git(repo_root: &Path, args: &[&str]) -> Result<String, WorktreeError> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(repo_root);
    cmd.args(args);
    let output = cmd.output().map_err(WorktreeError::IoError)?;
    if !output.status.success() {
        let rendered_args = args
            .iter()
            .map(|a| format!("{a:?}"))
            .collect::<Vec<_>>()
            .join(" ");
        return Err(WorktreeError::GitCommandFailed {
            command: format!("git -C {} {}", repo_root.display(), rendered_args),
            status: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Create a worktree at `<repo_root>/.phantom/worktrees/<sanitized>`.
pub fn create_worktree(
    repo_root: &Path,
    branch: &str,
    base_ref: &str,
) -> Result<WorktreeHandle, WorktreeError> {
    let dir_name = sanitize_branch(branch)?;
    let worktrees_root = repo_root.join(".phantom").join("worktrees");
    std::fs::create_dir_all(&worktrees_root)?;
    let path = worktrees_root.join(&dir_name);

    let path_str = path
        .to_str()
        .ok_or_else(|| WorktreeError::ParseError("non-utf8 worktree path".into()))?;
    run_git(
        repo_root,
        &["worktree", "add", "-b", branch, path_str, base_ref],
    )?;

    Ok(WorktreeHandle {
        branch: branch.to_string(),
        path,
        base_ref: base_ref.to_string(),
    })
}

/// List all worktrees known to `git -C <repo_root>`.
pub fn list_worktrees(repo_root: &Path) -> Result<Vec<WorktreeInfo>, WorktreeError> {
    let stdout = run_git(repo_root, &["worktree", "list", "--porcelain"])?;
    parse_porcelain(&stdout)
}

/// Parse the output of `git worktree list --porcelain`.
fn parse_porcelain(stdout: &str) -> Result<Vec<WorktreeInfo>, WorktreeError> {
    let mut out = Vec::new();
    let mut cur_path: Option<PathBuf> = None;
    let mut cur_head: Option<String> = None;
    let mut cur_branch: Option<String> = None;
    let mut cur_bare = false;
    let mut cur_detached = false;

    let flush = |out: &mut Vec<WorktreeInfo>,
                 cur_path: &mut Option<PathBuf>,
                 cur_head: &mut Option<String>,
                 cur_branch: &mut Option<String>,
                 cur_bare: &mut bool,
                 cur_detached: &mut bool| {
        if let Some(p) = cur_path.take() {
            out.push(WorktreeInfo {
                branch: cur_branch.take(),
                path: p,
                head: cur_head.take(),
                is_bare: *cur_bare,
                is_detached: *cur_detached,
            });
        }
        *cur_bare = false;
        *cur_detached = false;
    };

    for line in stdout.lines() {
        if line.is_empty() {
            flush(
                &mut out,
                &mut cur_path,
                &mut cur_head,
                &mut cur_branch,
                &mut cur_bare,
                &mut cur_detached,
            );
            continue;
        }
        if let Some(rest) = line.strip_prefix("worktree ") {
            cur_path = Some(PathBuf::from(rest));
        } else if let Some(rest) = line.strip_prefix("HEAD ") {
            cur_head = Some(rest.to_string());
        } else if let Some(rest) = line.strip_prefix("branch ") {
            // Porcelain emits e.g. `branch refs/heads/feature`. Strip the prefix
            // when present so callers see a plain branch name.
            let short = rest.strip_prefix("refs/heads/").unwrap_or(rest);
            cur_branch = Some(short.to_string());
        } else if line == "bare" {
            cur_bare = true;
        } else if line == "detached" {
            cur_detached = true;
        }
        // Unknown lines are ignored so future porcelain additions do not
        // crash the parser.
    }
    flush(
        &mut out,
        &mut cur_path,
        &mut cur_head,
        &mut cur_branch,
        &mut cur_bare,
        &mut cur_detached,
    );
    Ok(out)
}

/// Remove a worktree previously created by `create_worktree`.
pub fn remove_worktree(
    repo_root: &Path,
    handle: WorktreeHandle,
    force: bool,
) -> Result<(), WorktreeError> {
    let path_str = handle
        .path
        .to_str()
        .ok_or_else(|| WorktreeError::ParseError("non-utf8 worktree path".into()))?;
    let mut args: Vec<&str> = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(path_str);
    run_git(repo_root, &args)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    fn has_git() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn setup_repo() -> TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path();
        let run = |args: &[&str]| {
            let out = Command::new("git")
                .arg("-C")
                .arg(p)
                .args(args)
                .output()
                .expect("git invocation");
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&["init", "-b", "main"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test"]);
        run(&["config", "commit.gpgsign", "false"]);
        std::fs::write(p.join("seed.txt"), "seed\n").unwrap();
        run(&["add", "seed.txt"]);
        run(&["commit", "-m", "seed"]);
        dir
    }

    fn canon(p: &Path) -> PathBuf {
        std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
    }

    #[test]
    fn creates_worktree_at_dot_phantom_path() {
        if !has_git() {
            return;
        }
        let repo = setup_repo();
        let handle =
            create_worktree(repo.path(), "feature-one", "HEAD").expect("create_worktree");
        assert_eq!(handle.branch, "feature-one");
        assert_eq!(handle.base_ref, "HEAD");
        assert!(handle.path.exists(), "worktree dir should exist");
        let expected = repo.path().join(".phantom").join("worktrees").join("feature-one");
        assert_eq!(canon(&handle.path), canon(&expected));
    }

    #[test]
    fn lists_created_worktree() {
        if !has_git() {
            return;
        }
        let repo = setup_repo();
        let handle =
            create_worktree(repo.path(), "feature-list", "HEAD").expect("create_worktree");
        let entries = list_worktrees(repo.path()).expect("list_worktrees");
        let want = canon(&handle.path);
        let found = entries.iter().any(|e| canon(&e.path) == want);
        assert!(found, "newly created worktree not in list: {entries:?}");
        let entry = entries.iter().find(|e| canon(&e.path) == want).unwrap();
        assert_eq!(entry.branch.as_deref(), Some("feature-list"));
    }

    #[test]
    fn removes_worktree_cleans_up() {
        if !has_git() {
            return;
        }
        let repo = setup_repo();
        let handle =
            create_worktree(repo.path(), "feature-remove", "HEAD").expect("create_worktree");
        let path = handle.path.clone();
        remove_worktree(repo.path(), handle, false).expect("remove_worktree");
        assert!(!path.exists(), "worktree dir should be gone");
        let entries = list_worktrees(repo.path()).expect("list_worktrees");
        let want = canon(&path);
        assert!(
            !entries.iter().any(|e| canon(&e.path) == want),
            "removed worktree still listed: {entries:?}"
        );
    }

    #[test]
    fn sanitization_rejects_bad_names() {
        // These checks are pure and do not need git on PATH.
        let cases = ["", "   ", "-flagish", "../escape", "a/../b", "a\0b", "weird name", "bad*char"];
        for case in cases {
            let r = sanitize_branch(case);
            assert!(matches!(r, Err(WorktreeError::InvalidBranchName(_))), "expected reject for {case:?}, got {r:?}");
        }
        // Allowed: slashes become dashes on the path component.
        assert_eq!(sanitize_branch("feat/x").unwrap(), "feat-x");
        assert_eq!(sanitize_branch("v1.2.3+meta").unwrap(), "v1.2.3+meta");
    }

    #[test]
    fn bad_base_ref_returns_git_command_failed() {
        if !has_git() {
            return;
        }
        let repo = setup_repo();
        let err = create_worktree(repo.path(), "feature-bad", "definitely-not-a-ref")
            .expect_err("should fail with bad base ref");
        match err {
            WorktreeError::GitCommandFailed { status, stderr, .. } => {
                assert!(status.map(|c| c != 0).unwrap_or(true));
                assert!(!stderr.is_empty(), "expected non-empty stderr");
            }
            other => panic!("expected GitCommandFailed, got {other:?}"),
        }
    }

    #[test]
    fn porcelain_parser_handles_blank_separated_records() {
        let sample = "\
worktree /tmp/repo
HEAD abc123
branch refs/heads/main

worktree /tmp/repo/.phantom/worktrees/feature
HEAD def456
branch refs/heads/feature

worktree /tmp/repo/.phantom/worktrees/detached
HEAD 000111
detached
";
        let parsed = parse_porcelain(sample).expect("parse");
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].branch.as_deref(), Some("main"));
        assert_eq!(parsed[1].branch.as_deref(), Some("feature"));
        assert!(parsed[2].is_detached);
        assert_eq!(parsed[2].branch, None);
    }
}
