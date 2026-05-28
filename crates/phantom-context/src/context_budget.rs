//! Token-budgeted context-injection primitive.
//!
//! Mirrors Claude Code's pattern: take the most recent N items, cap each one
//! at a per-item token ceiling, and stop accumulating once the aggregate token
//! ceiling would be exceeded. Items that would overflow are dropped rather
//! than truncated further; later items that still fit are kept.
//!
//! This module is standalone. It does not read from disk and does not depend
//! on any other phantom-context types. Callers supply items via the
//! [`BudgetedItem`] trait and a token counter via [`TokenCounter`].

use std::cmp::Reverse;

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Configuration for the token-budgeted selection algorithm.
///
/// The default values mirror Claude Code's reference pattern: top 5 most
/// recent items, each capped at 5_000 tokens, with a 25_000-token aggregate
/// ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextBudget {
    /// Maximum number of items to consider after recency ordering.
    pub max_items: usize,
    /// Per-item token cap. Items longer than this are truncated.
    pub per_item_token_cap: usize,
    /// Aggregate token ceiling across all selected items.
    pub total_token_cap: usize,
}

impl Default for ContextBudget {
    fn default() -> Self {
        Self {
            max_items: 5,
            per_item_token_cap: 5_000,
            total_token_cap: 25_000,
        }
    }
}

// ---------------------------------------------------------------------------
// Token counter
// ---------------------------------------------------------------------------

/// Counts tokens in a payload string.
///
/// The trait is the seam through which a real BPE tokenizer can be swapped
/// in. For now the only built-in implementation is [`WordCounter`], which
/// counts whitespace-separated words.
pub trait TokenCounter {
    fn count(&self, s: &str) -> usize;
}

/// Zero-sized whitespace token counter.
///
/// Splits on ASCII / Unicode whitespace and counts the resulting non-empty
/// segments. Cheap and deterministic.
#[derive(Debug, Default, Clone, Copy)]
pub struct WordCounter;

impl TokenCounter for WordCounter {
    fn count(&self, s: &str) -> usize {
        s.split_whitespace().count()
    }
}

// ---------------------------------------------------------------------------
// Budgeted item
// ---------------------------------------------------------------------------

/// An item that can be ranked, sized, and identified for budgeted selection.
///
/// Implementors provide a recency key for ordering, the raw payload string,
/// and a stable identifier used for diagnostics and de-duplication by the
/// caller.
pub trait BudgetedItem {
    type Recency: Ord;
    fn recency_key(&self) -> Self::Recency;
    fn payload(&self) -> &str;
    fn identifier(&self) -> &str;
}

/// The result of running [`ContextBudget::select`] over one item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedItem {
    /// Identifier copied from the source [`BudgetedItem`].
    pub identifier: String,
    /// Possibly-truncated payload. Truncation preserves word boundaries.
    pub payload: String,
    /// Token cost of `payload` as measured by the supplied [`TokenCounter`].
    pub token_cost: usize,
}

// ---------------------------------------------------------------------------
// Selection
// ---------------------------------------------------------------------------

impl ContextBudget {
    /// Select up to `max_items` of the most recent items from `items`, with
    /// each truncated to `per_item_token_cap` tokens and the aggregate
    /// bounded by `total_token_cap`.
    ///
    /// The algorithm:
    /// 1. Materialize the iterator.
    /// 2. Sort descending by `recency_key`.
    /// 3. Truncate to at most `max_items`.
    /// 4. For each surviving item, truncate payload to the per-item cap.
    /// 5. Accumulate cost; drop any item whose inclusion would overflow the
    ///    total cap, but continue evaluating subsequent items in case a
    ///    smaller one still fits.
    pub fn select<I, C>(
        &self,
        items: impl IntoIterator<Item = I>,
        counter: &C,
    ) -> Vec<SelectedItem>
    where
        I: BudgetedItem,
        C: TokenCounter,
    {
        let mut collected: Vec<I> = items.into_iter().collect();
        // Sort newest first.
        collected.sort_by_key(|it| Reverse(it.recency_key()));
        collected.truncate(self.max_items);

        let mut out: Vec<SelectedItem> = Vec::with_capacity(collected.len());
        let mut running: usize = 0;

        for it in collected {
            let raw = it.payload();
            let raw_cost = counter.count(raw);

            let (payload, cost) = if raw_cost > self.per_item_token_cap {
                let truncated = truncate_to_tokens(raw, self.per_item_token_cap);
                let cost = counter.count(&truncated);
                (truncated, cost)
            } else {
                (raw.to_string(), raw_cost)
            };

            if running.saturating_add(cost) > self.total_token_cap {
                // Drop this item; keep looking for a smaller one that fits.
                continue;
            }

            running = running.saturating_add(cost);
            out.push(SelectedItem {
                identifier: it.identifier().to_string(),
                payload,
                token_cost: cost,
            });
        }

        out
    }
}

/// Truncate `s` to the first `cap` whitespace-separated tokens, joined with
/// single spaces. The result has token count `min(cap, original_count)`.
fn truncate_to_tokens(s: &str, cap: usize) -> String {
    s.split_whitespace()
        .take(cap)
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal test fixture implementing [`BudgetedItem`].
    struct TestItem {
        id: String,
        recency: u64,
        body: String,
    }

    impl TestItem {
        fn new(id: &str, recency: u64, body: &str) -> Self {
            Self {
                id: id.to_string(),
                recency,
                body: body.to_string(),
            }
        }
    }

    impl BudgetedItem for TestItem {
        type Recency = u64;
        fn recency_key(&self) -> u64 {
            self.recency
        }
        fn payload(&self) -> &str {
            &self.body
        }
        fn identifier(&self) -> &str {
            &self.id
        }
    }

    fn body_of(n: usize) -> String {
        (0..n).map(|i| format!("w{i}")).collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn cap_respecting_selection() {
        let budget = ContextBudget {
            max_items: 3,
            per_item_token_cap: 1_000,
            total_token_cap: 1_000_000,
        };
        let items: Vec<TestItem> = (0..10)
            .map(|i| TestItem::new(&format!("i{i}"), i as u64, "alpha beta"))
            .collect();
        let out = budget.select(items, &WordCounter);
        assert_eq!(out.len(), 3);
        // The three most recent are i9, i8, i7.
        assert_eq!(out[0].identifier, "i9");
        assert_eq!(out[1].identifier, "i8");
        assert_eq!(out[2].identifier, "i7");
    }

    #[test]
    fn recency_ordering() {
        let budget = ContextBudget::default();
        let items = vec![
            TestItem::new("oldest", 1, "a b"),
            TestItem::new("newest", 100, "a b"),
            TestItem::new("middle", 50, "a b"),
        ];
        let out = budget.select(items, &WordCounter);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].identifier, "newest");
        assert_eq!(out[1].identifier, "middle");
        assert_eq!(out[2].identifier, "oldest");
    }

    #[test]
    fn per_item_truncation() {
        let budget = ContextBudget {
            max_items: 5,
            per_item_token_cap: 4,
            total_token_cap: 1_000_000,
        };
        let item = TestItem::new("big", 1, &body_of(20));
        let out = budget.select(vec![item], &WordCounter);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].token_cost, 4);
        assert_eq!(out[0].payload, "w0 w1 w2 w3");
    }

    #[test]
    fn empty_input() {
        let budget = ContextBudget::default();
        let items: Vec<TestItem> = vec![];
        let out = budget.select(items, &WordCounter);
        assert!(out.is_empty());
    }

    #[test]
    fn all_items_fit() {
        let budget = ContextBudget {
            max_items: 10,
            per_item_token_cap: 100,
            total_token_cap: 1_000,
        };
        let items = vec![
            TestItem::new("a", 1, "one two three"),
            TestItem::new("b", 2, "four five"),
            TestItem::new("c", 3, "six"),
        ];
        let out = budget.select(items, &WordCounter);
        assert_eq!(out.len(), 3);
        // Most-recent first.
        assert_eq!(out[0].identifier, "c");
        assert_eq!(out[0].payload, "six");
        assert_eq!(out[0].token_cost, 1);
        assert_eq!(out[1].identifier, "b");
        assert_eq!(out[1].payload, "four five");
        assert_eq!(out[1].token_cost, 2);
        assert_eq!(out[2].identifier, "a");
        assert_eq!(out[2].payload, "one two three");
        assert_eq!(out[2].token_cost, 3);
    }

    #[test]
    fn total_cap_hit_mid_iteration() {
        // Total cap is 10. After the first big item (8 tokens), a 5-token
        // item must be skipped (would overflow to 13) but a later 2-token
        // item still fits.
        let budget = ContextBudget {
            max_items: 10,
            per_item_token_cap: 100,
            total_token_cap: 10,
        };
        let items = vec![
            TestItem::new("big", 30, &body_of(8)),
            TestItem::new("medium", 20, &body_of(5)),
            TestItem::new("small", 10, &body_of(2)),
        ];
        let out = budget.select(items, &WordCounter);
        // big and small fit; medium is dropped.
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].identifier, "big");
        assert_eq!(out[0].token_cost, 8);
        assert_eq!(out[1].identifier, "small");
        assert_eq!(out[1].token_cost, 2);
    }
}
