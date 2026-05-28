# TASKS

- [ ] T1. Add `pub mod worktrees;` to `crates/phantom-app/src/lib.rs`.
- [ ] T2. Create `crates/phantom-app/src/worktrees.rs` with `WorktreeHandle`, `WorktreeInfo`, and `WorktreeError`.
- [ ] T3. Implement `sanitize_branch` per SPEC: reject empty, leading `-`, `..`, NUL, out-of-charset; slash-to-dash on path component.
- [ ] T4. Implement `run_git` helper around `std::process::Command`.
- [ ] T5. Implement `create_worktree(repo_root, branch, base_ref)` returning `WorktreeHandle`.
- [ ] T6. Implement `list_worktrees(repo_root)` with a porcelain parser.
- [ ] T7. Implement `remove_worktree(repo_root, handle, force)`.
- [ ] T8. Add `#[cfg(test)] mod tests` covering create, list, remove, sanitization, bad base ref.
- [ ] T9. Run `cargo build -p phantom-app` and confirm it compiles.
- [ ] T10. Run `cargo test -p phantom-app worktrees` and confirm all new tests pass.
- [ ] T11. Commit on the preconfigured branch with a clear message.
- [ ] T12. Push and open a draft PR with the required title and HEREDOC body.
