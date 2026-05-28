# SPEC: Token-Budgeted Context Injection Primitive

## Problem

Phantom's `phantom-context` crate assembles agent prompt context without any
size ceiling. As session memory, skill files, and recent history grow, the
prompt payload grows without bound. Claude Code's reference pattern caps each
of the top-5 most recent skill files at 5K tokens with a 25K aggregate ceiling.
This SPEC introduces an equivalent primitive in Phantom.

## Scope

Standalone module. No wiring into agent spawn in this slice. Public surface
limited to one new module exported through `phantom-context::lib.rs`.

## Public API

Module: `crates/phantom-context/src/context_budget.rs`.

### `ContextBudget`

```rust
pub struct ContextBudget {
    pub max_items: usize,
    pub per_item_token_cap: usize,
    pub total_token_cap: usize,
}
```

Default values mirror Claude Code: `max_items = 5`, `per_item_token_cap = 5_000`,
`total_token_cap = 25_000`.

### `TokenCounter` trait

```rust
pub trait TokenCounter {
    fn count(&self, s: &str) -> usize;
}
```

Default impl `WordCounter` splits on ASCII whitespace.

### `BudgetedItem` trait

```rust
pub trait BudgetedItem {
    type Recency: Ord;
    fn recency_key(&self) -> Self::Recency;
    fn payload(&self) -> &str;
    fn identifier(&self) -> &str;
}
```

### `SelectedItem`

```rust
pub struct SelectedItem {
    pub identifier: String,
    pub payload: String,
    pub token_cost: usize,
}
```

### Selection method

```rust
impl ContextBudget {
    pub fn select<I, C>(&self, items: impl IntoIterator<Item = I>, counter: &C)
        -> Vec<SelectedItem>
    where
        I: BudgetedItem,
        C: TokenCounter;
}
```

## Algorithm

1. Collect items into a vec.
2. Sort by `recency_key` descending (most-recent first).
3. Take the first `max_items`.
4. For each item: count tokens; if over `per_item_token_cap`, truncate payload
   to the first `per_item_token_cap` tokens.
5. Accumulate total cost. If adding the next item would exceed
   `total_token_cap`, drop that item (do not truncate further) and continue
   evaluating subsequent items in case any are smaller and still fit.

## Non-goals

- Real BPE tokenization (the trait is the seam for that).
- Persistence, caching, or reinjection on compaction.
- Wiring into `phantom-agents` spawn paths.
