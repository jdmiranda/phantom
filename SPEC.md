# SPEC: First-class worktree primitive API

## Problem

Phantom currently uses git worktrees ad-hoc via shell-outs in the forever script and orchestration tooling. There is no first-class Rust API. Claude Code exposes `.claude/worktrees/` as a primitive for parallel branches; Phantom should expose the same shape under `.phantom/worktrees/`.

## Goals

- Introduce a typed, testable Rust API in `phantom-app` for managing git worktrees.
- Standardize worktree placement at `<repo_root>/.phantom/worktrees/<branch-sanitized>`.
- Cover the three operations Phantom orchestration actually needs today: create, list, remove.
- Provide a structured error type (`WorktreeError`) so callers can pattern-match instead of parsing stderr.
- Sanitize branch names so they cannot escape the worktrees directory.

## Non-goals (this slice)

- Migrating the forever script to call the new API. That is follow-up work.
- Adding any new crate. The module lives inside `phantom-app`.
- Pulling in `git2`/`libgit2`. We shell out via `std::process::Command`.
- Lock/concurrency primitives beyond what `git worktree` itself provides.

## Surface

Module: `crates/phantom-app/src/worktrees.rs`

Types:
- `pub struct WorktreeHandle { pub branch: String, pub path: PathBuf, pub base_ref: String }`
- `pub struct WorktreeInfo { pub branch: Option<String>, pub path: PathBuf, pub head: Option<String>, pub is_bare: bool, pub is_detached: bool }`
- `pub enum WorktreeError { InvalidBranchName(String), GitCommandFailed { command: String, status: Option<i32>, stderr: String }, IoError(io::Error), ParseError(String) }`

Functions:
- `pub fn create_worktree(repo_root: &Path, branch: &str, base_ref: &str) -> Result<WorktreeHandle, WorktreeError>`
- `pub fn list_worktrees(repo_root: &Path) -> Result<Vec<WorktreeInfo>, WorktreeError>`
- `pub fn remove_worktree(repo_root: &Path, handle: WorktreeHandle, force: bool) -> Result<(), WorktreeError>`

## Branch-name sanitization

A branch name is rejected (returns `InvalidBranchName`) when any of these hold:
- empty after trimming
- contains `..` as a path component
- starts with `-` (would be parsed as a flag by `git`)
- contains a NUL byte
- contains characters outside `[A-Za-z0-9._/+@-]`

When constructing the on-disk path, `/` is replaced with `-` so the worktree fits in a single directory under `.phantom/worktrees/`.

## Behavior

- `create_worktree` ensures `<repo_root>/.phantom/worktrees/` exists, then runs `git -C <repo_root> worktree add -b <branch> <path> <base_ref>`.
- `list_worktrees` runs `git -C <repo_root> worktree list --porcelain` and parses the porcelain stream.
- `remove_worktree` runs `git -C <repo_root> worktree remove [--force] <path>`.

## Testing

Unit tests use the `tempfile` dev-dep (already present) and shell out to the real `git` binary. Each test gracefully skips when `git` is unavailable on `PATH`, so the suite runs on workstations and in CI without special setup.

Coverage:
- create succeeds and returns a handle pointing at an existing directory.
- list shows the newly created worktree.
- remove cleans up the directory and removes the entry from `git worktree list`.
- sanitization rejects empty, leading-dash, `..`, NUL, and out-of-charset names.
- a bad base ref produces `GitCommandFailed` with a non-zero status and non-empty stderr.
