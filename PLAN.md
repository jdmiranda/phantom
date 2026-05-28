# PLAN: Token-Budgeted Context Injection

## Steps

1. Create `crates/phantom-context/src/context_budget.rs` containing:
   - `ContextBudget` struct with three usize fields and a `Default` impl
     matching Claude Code's 5/5K/25K reference values.
   - `TokenCounter` trait with `count(&self, s: &str) -> usize`.
   - `WordCounter` zero-sized type implementing `TokenCounter` via
     `s.split_whitespace().count()`.
   - `BudgetedItem` trait with associated type `Recency: Ord`, `recency_key`,
     `payload`, and `identifier` methods.
   - `SelectedItem` struct with `identifier`, `payload`, `token_cost`.
   - `ContextBudget::select` implementing the algorithm in SPEC.md.

2. Export the module from `crates/phantom-context/src/lib.rs` with a single
   `pub mod context_budget;` line. Do not re-export via glob to avoid name
   collisions with `context::*`.

3. Add unit tests inside the new module under a `#[cfg(test)] mod tests`
   block. Cover all six cases listed in TASKS.md.

4. Build and test only the `phantom-context` crate to stay within scope:
   - `cargo build -p phantom-context`
   - `cargo test -p phantom-context`

5. Commit the change on the current worktree branch with a Conventional
   Commits message: `feat(phantom-context): token-budgeted context injection
   primitive`.

6. Push the branch and open a draft PR via `gh pr create --draft` with the
   prescribed title and HEREDOC body.

## Risks

- Truncation by token (word) count rather than byte count means the payload
  string must be reconstructed from the kept words. Use
  `s.split_whitespace().take(cap).collect::<Vec<_>>().join(" ")`.
- The `total_token_cap` mid-iteration drop rule means a small item further
  down the list might still fit after a large one is dropped. The test
  `total_cap_hit_mid_iteration` pins that behavior.

## Out of scope

- BPE tokenizer integration.
- Real skill-file discovery or filesystem walks.
- Wiring into `phantom-agents::spawn` or any caller in this PR.
