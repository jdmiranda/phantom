# PLAN

## Step 1 — Scaffold the module

Create `crates/phantom-app/src/worktrees.rs` and declare it as `pub mod worktrees;` from `crates/phantom-app/src/lib.rs`. No other workspace edits.

## Step 2 — Define types

- `WorktreeHandle` (branch, path, base_ref).
- `WorktreeInfo` (branch, path, head, is_bare, is_detached).
- `WorktreeError` enum with `Display` + `std::error::Error` impls and a `From<io::Error>`.

## Step 3 — Branch sanitization

Internal `fn sanitize_branch(branch: &str) -> Result<String, WorktreeError>`:
1. Trim and validate emptiness.
2. Reject `..` components, leading `-`, NUL, and out-of-charset characters.
3. Replace `/` with `-` for the on-disk component.

## Step 4 — Git wrappers

Internal `fn run_git(repo_root: &Path, args: &[&str]) -> Result<String, WorktreeError>`:
- Spawn `git -C <repo_root> <args...>` via `std::process::Command`.
- On non-zero exit, return `GitCommandFailed { command, status, stderr }`.
- On success, return stdout as a `String`.

## Step 5 — Public functions

- `create_worktree`: sanitize, ensure `.phantom/worktrees/` exists, run `git worktree add -b <branch> <path> <base_ref>`.
- `list_worktrees`: run `git worktree list --porcelain`, parse blank-line-delimited records, map each to `WorktreeInfo`.
- `remove_worktree`: run `git worktree remove [--force] <path>`.

## Step 6 — Tests

`#[cfg(test)] mod tests` at the bottom of `worktrees.rs`. A `setup_repo()` helper creates a tempdir, runs `git init`, `git config user.email/name`, writes a file, and commits so HEAD exists. A `has_git()` guard returns false when `git --version` cannot run; every test starts with `if !has_git() { return; }`.

Tests:
- `creates_worktree_at_dot_phantom_path`
- `lists_created_worktree`
- `removes_worktree_cleans_up`
- `sanitization_rejects_bad_names`
- `bad_base_ref_returns_git_command_failed`

## Step 7 — Verification

- `cargo build -p phantom-app`
- `cargo test -p phantom-app worktrees`

## Step 8 — PR

Open a draft PR titled `feat(phantom-app): first-class worktree primitive API` with a 3-bullet Summary and a Test plan checklist.

## Risks and mitigations

- **Tests flake on machines without git**: each test guards on `has_git()` and returns early.
- **macOS tempdir under `/var` vs `/private/var` symlink mismatch**: tests canonicalize paths before comparing.
- **`git worktree list --porcelain` format drift**: parser handles unknown lines by ignoring them rather than failing.
