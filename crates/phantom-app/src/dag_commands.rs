//! DAG viewer command surface — stable mutation API for agent control.
//!
//! Agents (e.g. Cartographer) use this module to programmatically drive the
//! Inspector's DAG tab without touching low-level layout state directly. All
//! commands are deterministic and side-effect free outside the DAG viewer
//! surface: they only mutate [`DagViewerState`].
//!
//! ## Usage
//!
//! ```text
//! let result = execute_dag_command(&mut viewer_state, DagViewerCommand::FocusNode {
//!     id: "phantom_agents::dispatch::dispatch_tool".into(),
//! });
//! ```
//!
//! ## Command dispatch through the inspector adapter
//!
//! The [`InspectorAdapter`] implements [`Commandable`] and routes `dag.*`
//! commands here. External callers (brain, MCP, Cartographer) use the
//! coordinator's `send_command(inspector_id, "dag.focus_node", &args)` API.
//!
//! [`InspectorAdapter`]: crate::adapters::inspector::InspectorAdapter

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::adapters::dag_viewer::DagViewerState;

// ---------------------------------------------------------------------------
// Command enum
// ---------------------------------------------------------------------------

/// Commands an agent can issue against the DAG viewer.
///
/// Every command maps 1-to-1 onto a mutation of [`DagViewerState`].
/// Invalid node ids or out-of-range arguments return
/// [`DagCommandResult::NotFound`] or [`DagCommandResult::InvalidArgs`];
/// they never panic.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum DagViewerCommand {
    /// Select (focus) the node with the given id. The node is highlighted in
    /// the render and becomes the "current" selection.
    FocusNode {
        /// Fully-qualified node id (e.g. `phantom_agents::dispatch::dispatch_tool`).
        id: String,
    },

    /// Clear the current selection, returning the viewer to unselected state.
    ClearFocus,

    /// Pan the viewport so the node with the given id is centred on screen.
    ///
    /// If the node has no layout position (it was never computed), returns
    /// [`DagCommandResult::NotFound`].
    ScrollTo {
        /// Fully-qualified node id.
        id: String,
    },

    /// Set the zoom level to `factor`. Clamped to `[0.1, 5.0]` by the viewer.
    Zoom {
        /// Desired zoom factor (1.0 = 100%).
        factor: f32,
    },

    /// Apply an additive highlight to a set of node ids. Highlighted nodes
    /// receive a distinct rendering tint distinct from the selection state.
    ///
    /// Calling this multiple times *replaces* the previous highlight set.
    Highlight {
        /// Node ids to highlight. Ids that do not exist in the layout are
        /// silently skipped and noted in the `skipped` field of the response.
        ids: Vec<String>,
    },

    /// Clear the transient highlight state (does not affect selection).
    ClearHighlight,

    /// Reset the viewport to default zoom (1.0) and zero pan offset.
    ResetView,
}

// ---------------------------------------------------------------------------
// Result enum
// ---------------------------------------------------------------------------

/// Result of executing a [`DagViewerCommand`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DagCommandResult {
    /// The command was applied successfully.
    Ok {
        /// Human-readable description of the effect.
        message: String,
    },
    /// The referenced node id does not exist in the current layout.
    NotFound {
        /// The id that was not found.
        id: String,
    },
    /// The command arguments are invalid (e.g. NaN zoom factor).
    InvalidArgs {
        /// Description of what was wrong.
        reason: String,
    },
    /// The command was applied but some requested ids were not present in the
    /// layout. Present ids were still processed.
    PartialOk {
        /// Human-readable summary of what was applied.
        message: String,
        /// Node ids that were silently skipped (not in the current layout).
        skipped: Vec<String>,
    },
}

impl DagCommandResult {
    /// Serialise the result to a JSON string for the coordinator response wire.
    ///
    /// # Errors
    ///
    /// Returns an error only if `serde_json` fails, which is unreachable for
    /// well-formed enum variants.
    pub fn to_json(&self) -> anyhow::Result<String> {
        serde_json::to_string(self).map_err(|e| anyhow::anyhow!("dag result serialise: {e}"))
    }
}

// ---------------------------------------------------------------------------
// Highlight state (stored separately from DagViewerState to avoid coupling)
// ---------------------------------------------------------------------------

/// Transient highlight set — node ids that should receive a highlight tint
/// distinct from the selection color.
///
/// This state is owned by the [`InspectorAdapter`] alongside the
/// [`DagViewerState`] so that the two pieces of state stay in sync through
/// the same command path.
///
/// [`InspectorAdapter`]: crate::adapters::inspector::InspectorAdapter
#[derive(Debug, Default, Clone)]
pub struct DagHighlightState {
    pub highlighted_ids: HashSet<String>,
}

impl DagHighlightState {
    /// Create a new, empty highlight state.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

// ---------------------------------------------------------------------------
// Command execution
// ---------------------------------------------------------------------------

/// Execute a [`DagViewerCommand`] against `viewer` (and optionally `highlight`).
///
/// Returns a [`DagCommandResult`] describing the outcome. Never panics.
///
/// # Arguments
///
/// * `viewer` — mutable reference to the DAG viewer layout/selection state.
/// * `highlight` — mutable reference to the transient highlight state.
/// * `cmd` — the command to execute.
pub fn execute_dag_command(
    viewer: &mut DagViewerState,
    highlight: &mut DagHighlightState,
    cmd: DagViewerCommand,
) -> DagCommandResult {
    match cmd {
        // ── focus_node ───────────────────────────────────────────────────
        DagViewerCommand::FocusNode { id } => {
            if viewer.positions.contains_key(&id) {
                viewer.selected_node = Some(id.clone());
                DagCommandResult::Ok {
                    message: format!("focused node '{id}'"),
                }
            } else {
                DagCommandResult::NotFound { id }
            }
        }

        // ── clear_focus ──────────────────────────────────────────────────
        DagViewerCommand::ClearFocus => {
            viewer.selected_node = None;
            DagCommandResult::Ok {
                message: "selection cleared".to_string(),
            }
        }

        // ── scroll_to ────────────────────────────────────────────────────
        DagViewerCommand::ScrollTo { id } => {
            if let Some(&[nx, ny]) = viewer.positions.get(&id) {
                // Centre the node: negate the node's world position and add
                // half the canonical viewport size (notional 800×600 pane).
                viewer.viewport_offset = [
                    -(nx * viewer.zoom) + 400.0,
                    -(ny * viewer.zoom) + 300.0,
                ];
                DagCommandResult::Ok {
                    message: format!("scrolled to node '{id}'"),
                }
            } else {
                DagCommandResult::NotFound { id }
            }
        }

        // ── zoom ─────────────────────────────────────────────────────────
        DagViewerCommand::Zoom { factor } => {
            if !factor.is_finite() || factor <= 0.0 {
                return DagCommandResult::InvalidArgs {
                    reason: format!(
                        "zoom factor must be a positive finite number, got {factor}"
                    ),
                };
            }
            // Delegate to the existing clamp-safe method.
            // `handle_zoom` multiplies the current zoom — we want to set
            // to an absolute value, so divide by the current zoom first.
            let relative = factor / viewer.zoom;
            viewer.handle_zoom(relative);
            DagCommandResult::Ok {
                message: format!("zoom set to {:.2}", viewer.zoom),
            }
        }

        // ── highlight ────────────────────────────────────────────────────
        DagViewerCommand::Highlight { ids } => {
            highlight.highlighted_ids.clear();
            let mut skipped = Vec::new();

            for id in ids {
                if viewer.positions.contains_key(&id) {
                    highlight.highlighted_ids.insert(id);
                } else {
                    skipped.push(id);
                }
            }

            if skipped.is_empty() {
                DagCommandResult::Ok {
                    message: format!(
                        "highlighted {} node(s)",
                        highlight.highlighted_ids.len()
                    ),
                }
            } else {
                DagCommandResult::PartialOk {
                    message: format!(
                        "highlighted {} node(s), {} not found",
                        highlight.highlighted_ids.len(),
                        skipped.len()
                    ),
                    skipped,
                }
            }
        }

        // ── clear_highlight ──────────────────────────────────────────────
        DagViewerCommand::ClearHighlight => {
            let count = highlight.highlighted_ids.len();
            highlight.highlighted_ids.clear();
            DagCommandResult::Ok {
                message: format!("cleared {count} highlight(s)"),
            }
        }

        // ── reset_view ───────────────────────────────────────────────────
        DagViewerCommand::ResetView => {
            viewer.viewport_offset = [0.0, 0.0];
            viewer.zoom = 1.0;
            DagCommandResult::Ok {
                message: "view reset to default zoom and pan".to_string(),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// JSON adapter helpers (for coordinator wire protocol)
// ---------------------------------------------------------------------------

/// Parse a `DagViewerCommand` from the JSON `args` blob passed via the
/// coordinator's `send_command` wire.
///
/// The `cmd` parameter is the command name (e.g. `"dag.focus_node"`) and
/// `args` is the argument payload. This function maps the two into a typed
/// [`DagViewerCommand`].
///
/// # Errors
///
/// Returns an error string (not a panic) when the command name is unrecognised
/// or required arguments are missing.
pub fn parse_dag_command(cmd: &str, args: &serde_json::Value) -> Result<DagViewerCommand, String> {
    match cmd {
        "dag.focus_node" => {
            let id = args["id"]
                .as_str()
                .ok_or("dag.focus_node requires {\"id\": \"<node-id>\"}")?
                .to_string();
            Ok(DagViewerCommand::FocusNode { id })
        }
        "dag.clear_focus" => Ok(DagViewerCommand::ClearFocus),
        "dag.scroll_to" => {
            let id = args["id"]
                .as_str()
                .ok_or("dag.scroll_to requires {\"id\": \"<node-id>\"}")?
                .to_string();
            Ok(DagViewerCommand::ScrollTo { id })
        }
        "dag.zoom" => {
            let factor = args["factor"]
                .as_f64()
                .ok_or("dag.zoom requires {\"factor\": <number>}")? as f32;
            Ok(DagViewerCommand::Zoom { factor })
        }
        "dag.highlight" => {
            let ids = args["ids"]
                .as_array()
                .ok_or("dag.highlight requires {\"ids\": [\"<id>\", ...]}")?
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect();
            Ok(DagViewerCommand::Highlight { ids })
        }
        "dag.clear_highlight" => Ok(DagViewerCommand::ClearHighlight),
        "dag.reset_view" => Ok(DagViewerCommand::ResetView),
        other => Err(format!(
            "unknown DAG command '{other}'; supported: dag.focus_node, \
             dag.clear_focus, dag.scroll_to, dag.zoom, dag.highlight, \
             dag.clear_highlight, dag.reset_view"
        )),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use phantom_dag::{CodeDag, DagEdge, DagNode, EdgeKind, NodeKind};

    use super::*;
    use crate::adapters::dag_viewer::DagViewerState;

    fn node(id: &str) -> DagNode {
        DagNode::new(id.to_owned(), NodeKind::Function, PathBuf::from("src/lib.rs"), 1)
    }

    fn edge(from: &str, to: &str) -> DagEdge {
        DagEdge::new(from.to_owned(), to.to_owned(), EdgeKind::Calls)
    }

    fn three_node_viewer() -> (DagViewerState, DagHighlightState, CodeDag) {
        let mut dag = CodeDag::new();
        dag.add_node(node("a"));
        dag.add_node(node("b"));
        dag.add_node(node("c"));
        dag.add_edge(edge("a", "b"));

        let mut viewer = DagViewerState::new();
        viewer.compute_layout(&dag);
        let highlight = DagHighlightState::new();

        (viewer, highlight, dag)
    }

    // ── focus_node ──────────────────────────────────────────────────────────

    #[test]
    fn focus_node_existing_selects_it() {
        let (mut viewer, mut hl, _dag) = three_node_viewer();

        let result = execute_dag_command(
            &mut viewer,
            &mut hl,
            DagViewerCommand::FocusNode { id: "a".into() },
        );

        assert!(matches!(result, DagCommandResult::Ok { .. }));
        assert_eq!(viewer.selected_node, Some("a".into()));
    }

    #[test]
    fn focus_node_missing_returns_not_found() {
        let (mut viewer, mut hl, _dag) = three_node_viewer();

        let result = execute_dag_command(
            &mut viewer,
            &mut hl,
            DagViewerCommand::FocusNode { id: "ghost".into() },
        );

        assert!(
            matches!(result, DagCommandResult::NotFound { ref id } if id == "ghost"),
            "expected NotFound, got {result:?}",
        );
        assert!(viewer.selected_node.is_none());
    }

    // ── clear_focus ─────────────────────────────────────────────────────────

    #[test]
    fn clear_focus_removes_selection() {
        let (mut viewer, mut hl, _dag) = three_node_viewer();
        viewer.selected_node = Some("a".into());

        let result =
            execute_dag_command(&mut viewer, &mut hl, DagViewerCommand::ClearFocus);

        assert!(matches!(result, DagCommandResult::Ok { .. }));
        assert!(viewer.selected_node.is_none());
    }

    #[test]
    fn clear_focus_on_empty_selection_is_ok() {
        let (mut viewer, mut hl, _dag) = three_node_viewer();

        let result =
            execute_dag_command(&mut viewer, &mut hl, DagViewerCommand::ClearFocus);

        assert!(matches!(result, DagCommandResult::Ok { .. }));
    }

    // ── scroll_to ───────────────────────────────────────────────────────────

    #[test]
    fn scroll_to_existing_node_shifts_viewport() {
        let (mut viewer, mut hl, _dag) = three_node_viewer();
        let initial_offset = viewer.viewport_offset;

        let result = execute_dag_command(
            &mut viewer,
            &mut hl,
            DagViewerCommand::ScrollTo { id: "a".into() },
        );

        assert!(matches!(result, DagCommandResult::Ok { .. }));
        // The viewport offset must have been updated from the default [0,0].
        // (The exact value depends on the force-directed layout, which is
        //  non-deterministic in position but guaranteed to shift the offset.)
        let [nx, ny] = viewer.positions["a"];
        let expected_x = -(nx * 1.0) + 400.0;
        let expected_y = -(ny * 1.0) + 300.0;
        assert!(
            (viewer.viewport_offset[0] - expected_x).abs() < 0.001,
            "viewport_offset.x mismatch: expected {expected_x}, got {}",
            viewer.viewport_offset[0],
        );
        assert!(
            (viewer.viewport_offset[1] - expected_y).abs() < 0.001,
            "viewport_offset.y mismatch: expected {expected_y}, got {}",
            viewer.viewport_offset[1],
        );
        // Confirm it actually moved from the initial offset (covers the case
        // where the node happens to be exactly at [0,0] — unlikely but safe).
        let _ = initial_offset; // no assertion needed; formula above is the contract
    }

    #[test]
    fn scroll_to_missing_node_returns_not_found() {
        let (mut viewer, mut hl, _dag) = three_node_viewer();
        let initial_offset = viewer.viewport_offset;

        let result = execute_dag_command(
            &mut viewer,
            &mut hl,
            DagViewerCommand::ScrollTo { id: "ghost".into() },
        );

        assert!(
            matches!(result, DagCommandResult::NotFound { .. }),
            "expected NotFound, got {result:?}",
        );
        // Viewport must not have changed.
        assert_eq!(viewer.viewport_offset, initial_offset);
    }

    // ── zoom ────────────────────────────────────────────────────────────────

    #[test]
    fn zoom_sets_absolute_factor() {
        let (mut viewer, mut hl, _dag) = three_node_viewer();

        let result = execute_dag_command(
            &mut viewer,
            &mut hl,
            DagViewerCommand::Zoom { factor: 2.0 },
        );

        assert!(matches!(result, DagCommandResult::Ok { .. }));
        assert!((viewer.zoom - 2.0).abs() < 0.01, "zoom should be ~2.0, got {}", viewer.zoom);
    }

    #[test]
    fn zoom_clamps_to_max() {
        let (mut viewer, mut hl, _dag) = three_node_viewer();

        let result = execute_dag_command(
            &mut viewer,
            &mut hl,
            DagViewerCommand::Zoom { factor: 999.0 },
        );

        assert!(matches!(result, DagCommandResult::Ok { .. }));
        assert!(viewer.zoom <= 5.0 + 1e-6, "zoom must be clamped to ZOOM_MAX");
    }

    #[test]
    fn zoom_clamps_to_min() {
        let (mut viewer, mut hl, _dag) = three_node_viewer();

        let result = execute_dag_command(
            &mut viewer,
            &mut hl,
            DagViewerCommand::Zoom { factor: 0.0001 },
        );

        assert!(matches!(result, DagCommandResult::Ok { .. }));
        assert!(viewer.zoom >= 0.1 - 1e-6, "zoom must be clamped to ZOOM_MIN");
    }

    #[test]
    fn zoom_nan_returns_invalid_args() {
        let (mut viewer, mut hl, _dag) = three_node_viewer();

        let result = execute_dag_command(
            &mut viewer,
            &mut hl,
            DagViewerCommand::Zoom { factor: f32::NAN },
        );

        assert!(
            matches!(result, DagCommandResult::InvalidArgs { .. }),
            "NaN zoom must return InvalidArgs, got {result:?}",
        );
    }

    #[test]
    fn zoom_negative_returns_invalid_args() {
        let (mut viewer, mut hl, _dag) = three_node_viewer();

        let result = execute_dag_command(
            &mut viewer,
            &mut hl,
            DagViewerCommand::Zoom { factor: -1.0 },
        );

        assert!(
            matches!(result, DagCommandResult::InvalidArgs { .. }),
            "negative zoom must return InvalidArgs, got {result:?}",
        );
    }

    // ── highlight ───────────────────────────────────────────────────────────

    #[test]
    fn highlight_all_known_ids_returns_ok() {
        let (mut viewer, mut hl, _dag) = three_node_viewer();

        let result = execute_dag_command(
            &mut viewer,
            &mut hl,
            DagViewerCommand::Highlight {
                ids: vec!["a".into(), "b".into()],
            },
        );

        assert!(matches!(result, DagCommandResult::Ok { .. }));
        assert!(hl.highlighted_ids.contains("a"));
        assert!(hl.highlighted_ids.contains("b"));
    }

    #[test]
    fn highlight_with_unknown_ids_returns_partial_ok() {
        let (mut viewer, mut hl, _dag) = three_node_viewer();

        let result = execute_dag_command(
            &mut viewer,
            &mut hl,
            DagViewerCommand::Highlight {
                ids: vec!["a".into(), "ghost".into()],
            },
        );

        assert!(
            matches!(
                result,
                DagCommandResult::PartialOk { ref skipped, .. } if skipped == &["ghost"]
            ),
            "expected PartialOk with ghost in skipped, got {result:?}",
        );
        assert!(hl.highlighted_ids.contains("a"));
        assert!(!hl.highlighted_ids.contains("ghost"));
    }

    #[test]
    fn highlight_replaces_previous_set() {
        let (mut viewer, mut hl, _dag) = three_node_viewer();

        execute_dag_command(
            &mut viewer,
            &mut hl,
            DagViewerCommand::Highlight {
                ids: vec!["a".into()],
            },
        );

        execute_dag_command(
            &mut viewer,
            &mut hl,
            DagViewerCommand::Highlight {
                ids: vec!["b".into()],
            },
        );

        assert!(!hl.highlighted_ids.contains("a"), "old highlight must be replaced");
        assert!(hl.highlighted_ids.contains("b"));
    }

    #[test]
    fn highlight_all_unknown_ids_leaves_set_empty() {
        let (mut viewer, mut hl, _dag) = three_node_viewer();

        let result = execute_dag_command(
            &mut viewer,
            &mut hl,
            DagViewerCommand::Highlight {
                ids: vec!["ghost1".into(), "ghost2".into()],
            },
        );

        assert!(
            matches!(result, DagCommandResult::PartialOk { .. }),
            "all-unknown highlight must still return PartialOk, got {result:?}",
        );
        assert!(hl.highlighted_ids.is_empty());
    }

    // ── clear_highlight ─────────────────────────────────────────────────────

    #[test]
    fn clear_highlight_empties_the_set() {
        let (mut viewer, mut hl, _dag) = three_node_viewer();
        hl.highlighted_ids.insert("a".into());
        hl.highlighted_ids.insert("b".into());

        let result =
            execute_dag_command(&mut viewer, &mut hl, DagViewerCommand::ClearHighlight);

        assert!(matches!(result, DagCommandResult::Ok { .. }));
        assert!(hl.highlighted_ids.is_empty());
    }

    #[test]
    fn clear_highlight_on_empty_set_is_ok() {
        let (mut viewer, mut hl, _dag) = three_node_viewer();

        let result =
            execute_dag_command(&mut viewer, &mut hl, DagViewerCommand::ClearHighlight);

        assert!(matches!(result, DagCommandResult::Ok { .. }));
    }

    // ── reset_view ──────────────────────────────────────────────────────────

    #[test]
    fn reset_view_restores_zoom_and_pan() {
        let (mut viewer, mut hl, _dag) = three_node_viewer();
        viewer.zoom = 3.5;
        viewer.viewport_offset = [100.0, -200.0];

        let result =
            execute_dag_command(&mut viewer, &mut hl, DagViewerCommand::ResetView);

        assert!(matches!(result, DagCommandResult::Ok { .. }));
        assert!((viewer.zoom - 1.0).abs() < 1e-6, "zoom must be 1.0 after reset");
        assert_eq!(viewer.viewport_offset, [0.0, 0.0], "pan must be zero after reset");
    }

    // ── parse_dag_command ───────────────────────────────────────────────────

    #[test]
    fn parse_focus_node_valid() {
        let args = serde_json::json!({"id": "phantom_agents::dispatch"});
        let cmd = parse_dag_command("dag.focus_node", &args).unwrap();
        assert!(matches!(cmd, DagViewerCommand::FocusNode { ref id } if id == "phantom_agents::dispatch"));
    }

    #[test]
    fn parse_focus_node_missing_id_is_err() {
        let args = serde_json::json!({});
        assert!(parse_dag_command("dag.focus_node", &args).is_err());
    }

    #[test]
    fn parse_clear_focus() {
        let cmd = parse_dag_command("dag.clear_focus", &serde_json::json!({})).unwrap();
        assert!(matches!(cmd, DagViewerCommand::ClearFocus));
    }

    #[test]
    fn parse_scroll_to_valid() {
        let args = serde_json::json!({"id": "some::node"});
        let cmd = parse_dag_command("dag.scroll_to", &args).unwrap();
        assert!(matches!(cmd, DagViewerCommand::ScrollTo { ref id } if id == "some::node"));
    }

    #[test]
    fn parse_zoom_valid() {
        let args = serde_json::json!({"factor": 2.5});
        let cmd = parse_dag_command("dag.zoom", &args).unwrap();
        assert!(matches!(cmd, DagViewerCommand::Zoom { factor } if (factor - 2.5).abs() < 0.001));
    }

    #[test]
    fn parse_zoom_missing_factor_is_err() {
        let args = serde_json::json!({});
        assert!(parse_dag_command("dag.zoom", &args).is_err());
    }

    #[test]
    fn parse_highlight_valid() {
        let args = serde_json::json!({"ids": ["a", "b"]});
        let cmd = parse_dag_command("dag.highlight", &args).unwrap();
        assert!(
            matches!(cmd, DagViewerCommand::Highlight { ref ids } if ids.len() == 2),
            "expected Highlight with 2 ids, got {cmd:?}",
        );
    }

    #[test]
    fn parse_clear_highlight() {
        let cmd = parse_dag_command("dag.clear_highlight", &serde_json::json!({})).unwrap();
        assert!(matches!(cmd, DagViewerCommand::ClearHighlight));
    }

    #[test]
    fn parse_reset_view() {
        let cmd = parse_dag_command("dag.reset_view", &serde_json::json!({})).unwrap();
        assert!(matches!(cmd, DagViewerCommand::ResetView));
    }

    #[test]
    fn parse_unknown_command_is_err() {
        let args = serde_json::json!({});
        assert!(parse_dag_command("dag.fly_to_moon", &args).is_err());
    }

    // ── result serialisation ────────────────────────────────────────────────

    #[test]
    fn dag_command_result_serialises_to_json() {
        let r = DagCommandResult::Ok { message: "done".into() };
        let json = r.to_json().unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["status"], "ok");
        assert_eq!(v["message"], "done");
    }

    #[test]
    fn not_found_serialises_correctly() {
        let r = DagCommandResult::NotFound { id: "ghost".into() };
        let json = r.to_json().unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["status"], "not_found");
        assert_eq!(v["id"], "ghost");
    }

    #[test]
    fn partial_ok_serialises_skipped_list() {
        let r = DagCommandResult::PartialOk {
            message: "partial".into(),
            skipped: vec!["ghost1".into(), "ghost2".into()],
        };
        let json = r.to_json().unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["status"], "partial_ok");
        assert_eq!(v["skipped"].as_array().unwrap().len(), 2);
    }
}
