# TASKS

## Implementation

- [ ] Add `crates/phantom-context/src/context_budget.rs` with the module body.
- [ ] Define `ContextBudget` struct with `max_items`, `per_item_token_cap`,
      `total_token_cap` (all `usize`) plus a `Default` impl returning
      `{ 5, 5_000, 25_000 }`.
- [ ] Define `TokenCounter` trait with `fn count(&self, s: &str) -> usize`.
- [ ] Provide `WordCounter` zero-sized struct implementing `TokenCounter` via
      `s.split_whitespace().count()`.
- [ ] Define `BudgetedItem` trait with associated type `Recency: Ord` and the
      `recency_key`, `payload`, `identifier` methods.
- [ ] Define `SelectedItem` struct `{ identifier: String, payload: String,
      token_cost: usize }`.
- [ ] Implement `ContextBudget::select` per the SPEC.md algorithm.
- [ ] Add `pub mod context_budget;` to `crates/phantom-context/src/lib.rs`.

## Tests (all inside the module)

- [ ] `cap_respecting_selection`: 10 items, `max_items = 3`, only 3 returned.
- [ ] `recency_ordering`: items shuffled, output is sorted newest first.
- [ ] `per_item_truncation`: one item over `per_item_token_cap` is truncated
      to the cap and `token_cost == per_item_token_cap`.
- [ ] `empty_input`: empty iterator returns empty vec.
- [ ] `all_items_fit`: total tokens well under cap, all items returned with
      original payloads.
- [ ] `total_cap_hit_mid_iteration`: first big item consumes most of the
      budget; subsequent oversize item is dropped; later small item still
      fits and is included.

## Verification

- [ ] `cargo build -p phantom-context` exits 0.
- [ ] `cargo test -p phantom-context` exits 0 with all new tests passing.

## Delivery

- [ ] Commit on the current worktree branch.
- [ ] Push branch to origin.
- [ ] Open draft PR titled
      `feat(phantom-context): token-budgeted context injection primitive`.
- [ ] Reply with the PR URL.

## Constraints check

- [ ] Edition 2024, no new heavy deps.
- [ ] Zero question-mark sentences in any committed file.
- [ ] No modifications to existing public APIs in `phantom-context`.
- [ ] Total diff under 600 lines.
