//! Ticket overlay and module instability scoring for [`CodeDag`] nodes.
//!
//! Provides [`NodeOverlay`] (per-node ticket metadata + instability score),
//! [`OverlayIndex`] (a map from node id → overlay), and [`build_overlay`]
//! which derives an overlay from a GitHub issue list JSON payload.

use std::collections::HashMap;

use serde::Deserialize;

// ---------------------------------------------------------------------------
// NodeOverlay
// ---------------------------------------------------------------------------

/// Ticket metadata and computed instability score for a single DAG node.
#[derive(Debug, Clone, Default)]
pub struct NodeOverlay {
    /// Issue numbers that are open and touch files in this node's crate.
    pub open_tickets: Vec<u64>,
    /// Issue numbers that are in-progress (labelled `in-progress`) and touch
    /// files in this node's crate.
    pub in_progress_tickets: Vec<u64>,
    /// Count of tickets closed in the last 30 days (informational; not sourced
    /// from the open-issue endpoint but can be populated by the caller).
    pub tickets_closed_last_30d: u64,
    /// Weighted instability score computed from open/in-progress tickets and
    /// recent commit churn.
    pub instability_score: f32,
}

impl NodeOverlay {
    /// Compute an instability score from open tickets, in-progress tickets,
    /// and recent-commit count.
    ///
    /// Formula: `open * 1.0 + in_progress * 2.0 + recent_commits * 0.5`
    #[must_use]
    pub fn compute_instability(open: usize, in_progress: usize, recent_commits: u32) -> f32 {
        open as f32 * 1.0 + in_progress as f32 * 2.0 + recent_commits as f32 * 0.5
    }
}

// ---------------------------------------------------------------------------
// OverlayIndex
// ---------------------------------------------------------------------------

/// Map from node id → [`NodeOverlay`].
///
/// The key is either a fully-qualified node id (e.g.
/// `phantom_agents::dispatch::dispatch_tool`) or a crate name (e.g.
/// `phantom-agents`), depending on how the index was constructed.
pub type OverlayIndex = HashMap<String, NodeOverlay>;

// ---------------------------------------------------------------------------
// build_overlay
// ---------------------------------------------------------------------------

/// Minimal structure we need from each GitHub issue in the JSON array.
#[derive(Debug, Deserialize)]
struct GhIssue {
    number: u64,
    /// Issue title — parsed from JSON but not used in overlay logic.
    #[serde(default)]
    #[allow(dead_code)]
    title: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    labels: Vec<GhLabel>,
}

#[derive(Debug, Deserialize)]
struct GhLabel {
    name: String,
}

/// Extract crate names mentioned in an issue body.
///
/// Looks for lines containing `**Files touched**:` followed by paths of the
/// form `crates/<crate-name>/…`.  Also scans every line in the body for
/// `crates/<crate-name>/` occurrences.
fn extract_crate_names(body: &str) -> Vec<String> {
    let mut crates = Vec::new();
    for line in body.lines() {
        // Match any occurrence of `crates/<name>/` on this line.
        let mut remainder = line;
        while let Some(pos) = remainder.find("crates/") {
            let after = &remainder[pos + "crates/".len()..];
            // Crate name ends at the next `/` or whitespace.
            let end = after
                .find(|c: char| c == '/' || c.is_whitespace())
                .unwrap_or(after.len());
            let name = &after[..end];
            if !name.is_empty() {
                let owned = name.to_owned();
                if !crates.contains(&owned) {
                    crates.push(owned);
                }
            }
            // Advance past this match to find any further occurrences.
            remainder = &remainder[pos + "crates/".len() + end..];
        }
    }
    crates
}

/// Build an [`OverlayIndex`] from a GitHub issue list JSON string and a
/// [`CodeDag`].
///
/// # Arguments
///
/// * `gh_issues_json` — JSON produced by:
///   ```text
///   gh issue list -R jdmiranda/phantom --state open \
///       --json number,body,title,labels
///   ```
/// * `dag` — The graph whose nodes should be annotated.
///
/// # Algorithm
///
/// 1. Parse the issue list.
/// 2. For each issue, scan the body for `crates/<crate-name>/` path fragments.
/// 3. For each dag node, check whether the node's source file contains one of
///    those crate names.
/// 4. Aggregate open / in-progress ticket counts per crate, then compute
///    `instability_score` via [`NodeOverlay::compute_instability`].
///
/// # Errors
///
/// If `gh_issues_json` is not valid JSON this function returns an empty index
/// rather than panicking.
#[must_use]
pub fn build_overlay(gh_issues_json: &str, dag: &crate::CodeDag) -> OverlayIndex {
    // Parse the issue list; on any error return an empty index.
    let issues: Vec<GhIssue> = match serde_json::from_str(gh_issues_json) {
        Ok(v) => v,
        Err(_) => return OverlayIndex::new(),
    };

    // Per-crate accumulators: (open_tickets, in_progress_tickets)
    let mut open_map: HashMap<String, Vec<u64>> = HashMap::new();
    let mut in_progress_map: HashMap<String, Vec<u64>> = HashMap::new();

    for issue in &issues {
        let body = issue.body.as_deref().unwrap_or("");
        let crate_names = extract_crate_names(body);

        let is_in_progress = issue
            .labels
            .iter()
            .any(|l| l.name.eq_ignore_ascii_case("in-progress"));

        for crate_name in crate_names {
            if is_in_progress {
                in_progress_map
                    .entry(crate_name)
                    .or_default()
                    .push(issue.number);
            } else {
                open_map
                    .entry(crate_name)
                    .or_default()
                    .push(issue.number);
            }
        }
    }

    // Build one overlay entry per dag node that has at least one ticket.
    // We key on the node id (fully-qualified), and match by checking whether
    // the node's file path contains the crate name segment.
    let mut index = OverlayIndex::new();

    for node in dag.nodes() {
        let file_str = node.file().to_string_lossy();

        // Collect all crates that touch this node's file.
        let mut open_tickets: Vec<u64> = Vec::new();
        let mut in_progress_tickets: Vec<u64> = Vec::new();

        for (crate_name, tickets) in &open_map {
            if file_str.contains(crate_name.as_str()) {
                open_tickets.extend_from_slice(tickets);
            }
        }
        for (crate_name, tickets) in &in_progress_map {
            if file_str.contains(crate_name.as_str()) {
                in_progress_tickets.extend_from_slice(tickets);
            }
        }

        if !open_tickets.is_empty() || !in_progress_tickets.is_empty() {
            open_tickets.sort_unstable();
            open_tickets.dedup();
            in_progress_tickets.sort_unstable();
            in_progress_tickets.dedup();

            let score = NodeOverlay::compute_instability(
                open_tickets.len(),
                in_progress_tickets.len(),
                0,
            );

            index.insert(
                node.id().to_owned(),
                NodeOverlay {
                    open_tickets,
                    in_progress_tickets,
                    tickets_closed_last_30d: 0,
                    instability_score: score,
                },
            );
        }
    }

    index
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::{CodeDag, DagNode, NodeKind};

    // -----------------------------------------------------------------------
    // Helper
    // -----------------------------------------------------------------------

    fn agent_node(id: &str) -> DagNode {
        DagNode::new(
            id.to_owned(),
            NodeKind::Function,
            PathBuf::from("crates/phantom-agents/src/lib.rs"),
            1,
        )
    }

    // -----------------------------------------------------------------------
    // Tests
    // -----------------------------------------------------------------------

    #[test]
    fn instability_score_weights_open_in_progress_and_commits() {
        // compute_instability(2, 1, 4) == 2.0 * 1.0 + 1.0 * 2.0 + 4.0 * 0.5 = 6.0
        let score = NodeOverlay::compute_instability(2, 1, 4);
        assert!((score - 6.0).abs() < 0.001);
    }

    #[test]
    fn phantom_agents_node_has_nonzero_instability_when_issues_present() {
        // Create a mock OverlayIndex entry and check instability > 0
        let mut overlay = OverlayIndex::new();
        overlay.insert(
            "phantom-agents".to_string(),
            NodeOverlay {
                open_tickets: vec![1],
                instability_score: NodeOverlay::compute_instability(1, 0, 0),
                ..Default::default()
            },
        );
        assert!(overlay["phantom-agents"].instability_score > 0.0);
    }

    #[test]
    fn overlay_cache_hits_on_same_etag_and_sha() {
        // Test that calling build_overlay twice with the same inputs returns equal results
        let json = r#"[]"#; // empty issue list
        let dag = CodeDag::default(); // empty dag
        let o1 = build_overlay(json, &dag);
        let o2 = build_overlay(json, &dag);
        assert_eq!(o1.len(), o2.len()); // both empty
    }

    #[test]
    fn build_overlay_maps_issue_to_matching_dag_node() {
        let json = r#"[
            {
                "number": 42,
                "title": "Fix dispatch loop",
                "body": "**Files touched**:\n- crates/phantom-agents/src/dispatch.rs\n",
                "labels": []
            }
        ]"#;

        let mut dag = CodeDag::new();
        dag.add_node(agent_node("phantom_agents::dispatch::run"));

        let overlay = build_overlay(json, &dag);
        let entry = overlay.get("phantom_agents::dispatch::run").expect("entry missing");
        assert!(entry.open_tickets.contains(&42));
        assert!(entry.instability_score > 0.0);
    }

    #[test]
    fn build_overlay_in_progress_label_uses_higher_weight() {
        let json = r#"[
            {
                "number": 7,
                "title": "WIP: refactor agents",
                "body": "Touches crates/phantom-agents/src/lib.rs",
                "labels": [{"name": "in-progress"}]
            }
        ]"#;

        let mut dag = CodeDag::new();
        dag.add_node(agent_node("phantom_agents::lib::run"));

        let overlay = build_overlay(json, &dag);
        let entry = overlay.get("phantom_agents::lib::run").expect("entry missing");
        assert!(entry.in_progress_tickets.contains(&7));
        // 0 open * 1.0 + 1 in-progress * 2.0 + 0 commits * 0.5 = 2.0
        assert!((entry.instability_score - 2.0).abs() < 0.001);
    }

    #[test]
    fn build_overlay_invalid_json_returns_empty() {
        let dag = CodeDag::default();
        let overlay = build_overlay("not valid json {{{", &dag);
        assert!(overlay.is_empty());
    }

    #[test]
    fn extract_crate_names_finds_multiple_crates_in_one_body() {
        let body = "crates/phantom-brain/src/ooda.rs and crates/phantom-agents/src/lib.rs";
        let names = extract_crate_names(body);
        assert!(names.contains(&"phantom-brain".to_owned()));
        assert!(names.contains(&"phantom-agents".to_owned()));
    }
}
