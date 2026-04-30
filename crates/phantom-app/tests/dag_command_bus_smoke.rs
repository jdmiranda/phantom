//! End-to-end smoke test for the DAG viewer command surface (issue #372).
//!
//! These tests exercise the public API of `phantom_app::dag_commands`:
//! - `execute_dag_command` against a `DagViewerState` + `DagHighlightState`
//! - `parse_dag_command` for the JSON wire layer
//! - `DagCommandResult` serialisation
//!
//! InspectorAdapter-level tests (accept_command wiring) live in
//! `crates/phantom-app/src/adapters/inspector.rs` because `with_view` is
//! `pub(crate)`. This integration test covers the pure-Rust public surface.

use std::path::PathBuf;

use phantom_app::dag_commands::{
    DagCommandResult, DagHighlightState, DagViewerCommand, execute_dag_command,
    parse_dag_command,
};
use phantom_app::adapters::dag_viewer::DagViewerState;
use phantom_dag::{CodeDag, DagEdge, DagNode, EdgeKind, NodeKind};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn node(id: &str) -> DagNode {
    DagNode::new(id.to_owned(), NodeKind::Function, PathBuf::from("src/lib.rs"), 1)
}

fn edge(from: &str, to: &str) -> DagEdge {
    DagEdge::new(from.to_owned(), to.to_owned(), EdgeKind::Calls)
}

fn three_node_dag() -> CodeDag {
    let mut dag = CodeDag::new();
    dag.add_node(node("a"));
    dag.add_node(node("b"));
    dag.add_node(node("c"));
    dag.add_edge(edge("a", "b"));
    dag.add_edge(edge("b", "c"));
    dag
}

fn make_viewer(dag: &CodeDag) -> (DagViewerState, DagHighlightState) {
    let mut viewer = DagViewerState::new();
    viewer.compute_layout(dag);
    (viewer, DagHighlightState::new())
}

// ---------------------------------------------------------------------------
// execute_dag_command — full command set
// ---------------------------------------------------------------------------

/// focus_node on an existing node selects it and returns Ok.
#[test]
fn cmd_focus_node_selects_existing_node() {
    let dag = three_node_dag();
    let (mut viewer, mut hl) = make_viewer(&dag);

    let result = execute_dag_command(
        &mut viewer,
        &mut hl,
        DagViewerCommand::FocusNode { id: "a".into() },
    );

    assert!(matches!(result, DagCommandResult::Ok { .. }), "expected Ok, got {result:?}");
    assert_eq!(viewer.selected_node, Some("a".into()));
}

/// focus_node on an unknown node returns NotFound without mutation.
#[test]
fn cmd_focus_node_missing_returns_not_found() {
    let dag = three_node_dag();
    let (mut viewer, mut hl) = make_viewer(&dag);

    let result = execute_dag_command(
        &mut viewer,
        &mut hl,
        DagViewerCommand::FocusNode { id: "ghost".into() },
    );

    assert!(
        matches!(result, DagCommandResult::NotFound { .. }),
        "expected NotFound, got {result:?}",
    );
    assert!(viewer.selected_node.is_none());
}

/// clear_focus removes an active selection.
#[test]
fn cmd_clear_focus_removes_selection() {
    let dag = three_node_dag();
    let (mut viewer, mut hl) = make_viewer(&dag);
    viewer.selected_node = Some("b".into());

    let result = execute_dag_command(&mut viewer, &mut hl, DagViewerCommand::ClearFocus);

    assert!(matches!(result, DagCommandResult::Ok { .. }));
    assert!(viewer.selected_node.is_none());
}

/// scroll_to on an existing node updates the viewport offset according to
/// the centering formula: offset = [-nx * zoom + 400, -ny * zoom + 300].
#[test]
fn cmd_scroll_to_existing_node_updates_viewport() {
    let dag = three_node_dag();
    let (mut viewer, mut hl) = make_viewer(&dag);

    let result = execute_dag_command(
        &mut viewer,
        &mut hl,
        DagViewerCommand::ScrollTo { id: "b".into() },
    );

    assert!(matches!(result, DagCommandResult::Ok { .. }));

    let [nx, ny] = viewer.positions["b"];
    let expected_x = -nx * viewer.zoom + 400.0;
    let expected_y = -ny * viewer.zoom + 300.0;
    assert!(
        (viewer.viewport_offset[0] - expected_x).abs() < 0.01,
        "viewport x: expected {expected_x:.3}, got {:.3}",
        viewer.viewport_offset[0],
    );
    assert!(
        (viewer.viewport_offset[1] - expected_y).abs() < 0.01,
        "viewport y: expected {expected_y:.3}, got {:.3}",
        viewer.viewport_offset[1],
    );
}

/// scroll_to a missing node leaves viewport unchanged and returns NotFound.
#[test]
fn cmd_scroll_to_missing_returns_not_found() {
    let dag = three_node_dag();
    let (mut viewer, mut hl) = make_viewer(&dag);
    let saved = viewer.viewport_offset;

    let result = execute_dag_command(
        &mut viewer,
        &mut hl,
        DagViewerCommand::ScrollTo { id: "ghost".into() },
    );

    assert!(matches!(result, DagCommandResult::NotFound { .. }));
    assert_eq!(viewer.viewport_offset, saved, "viewport must not change on NotFound");
}

/// zoom sets an absolute factor within [0.1, 5.0].
#[test]
fn cmd_zoom_sets_absolute_factor() {
    let dag = three_node_dag();
    let (mut viewer, mut hl) = make_viewer(&dag);

    execute_dag_command(&mut viewer, &mut hl, DagViewerCommand::Zoom { factor: 2.5 });

    assert!(
        (viewer.zoom - 2.5).abs() < 0.05,
        "zoom should be ~2.5, got {}",
        viewer.zoom,
    );
}

/// zoom with NaN returns InvalidArgs without touching zoom.
#[test]
fn cmd_zoom_nan_returns_invalid_args() {
    let dag = three_node_dag();
    let (mut viewer, mut hl) = make_viewer(&dag);
    let saved = viewer.zoom;

    let result = execute_dag_command(
        &mut viewer,
        &mut hl,
        DagViewerCommand::Zoom { factor: f32::NAN },
    );

    assert!(matches!(result, DagCommandResult::InvalidArgs { .. }));
    assert!((viewer.zoom - saved).abs() < 1e-6, "zoom must not change on error");
}

/// zoom with a negative factor returns InvalidArgs.
#[test]
fn cmd_zoom_negative_returns_invalid_args() {
    let dag = three_node_dag();
    let (mut viewer, mut hl) = make_viewer(&dag);

    let result = execute_dag_command(
        &mut viewer,
        &mut hl,
        DagViewerCommand::Zoom { factor: -0.5 },
    );

    assert!(matches!(result, DagCommandResult::InvalidArgs { .. }));
}

/// highlight with all known ids returns Ok and populates the highlight set.
#[test]
fn cmd_highlight_all_known_returns_ok() {
    let dag = three_node_dag();
    let (mut viewer, mut hl) = make_viewer(&dag);

    let result = execute_dag_command(
        &mut viewer,
        &mut hl,
        DagViewerCommand::Highlight { ids: vec!["a".into(), "c".into()] },
    );

    assert!(matches!(result, DagCommandResult::Ok { .. }), "got {result:?}");
    assert!(hl.highlighted_ids.contains("a"));
    assert!(hl.highlighted_ids.contains("c"));
    assert!(!hl.highlighted_ids.contains("b"), "b was not requested");
}

/// highlight with mixed known/unknown returns PartialOk; known ids are stored.
#[test]
fn cmd_highlight_mixed_returns_partial_ok() {
    let dag = three_node_dag();
    let (mut viewer, mut hl) = make_viewer(&dag);

    let result = execute_dag_command(
        &mut viewer,
        &mut hl,
        DagViewerCommand::Highlight {
            ids: vec!["a".into(), "ghost".into()],
        },
    );

    assert!(
        matches!(result, DagCommandResult::PartialOk { .. }),
        "expected PartialOk, got {result:?}",
    );
    assert!(hl.highlighted_ids.contains("a"), "'a' must be highlighted");
    assert!(!hl.highlighted_ids.contains("ghost"), "'ghost' must not be highlighted");
}

/// Calling highlight twice replaces the previous set.
#[test]
fn cmd_highlight_replaces_previous_set() {
    let dag = three_node_dag();
    let (mut viewer, mut hl) = make_viewer(&dag);

    execute_dag_command(
        &mut viewer,
        &mut hl,
        DagViewerCommand::Highlight { ids: vec!["a".into()] },
    );
    execute_dag_command(
        &mut viewer,
        &mut hl,
        DagViewerCommand::Highlight { ids: vec!["b".into()] },
    );

    assert!(!hl.highlighted_ids.contains("a"), "first set must be replaced");
    assert!(hl.highlighted_ids.contains("b"));
}

/// clear_highlight empties the highlight set.
#[test]
fn cmd_clear_highlight_empties_set() {
    let dag = three_node_dag();
    let (mut viewer, mut hl) = make_viewer(&dag);
    hl.highlighted_ids.insert("a".into());

    let result = execute_dag_command(&mut viewer, &mut hl, DagViewerCommand::ClearHighlight);

    assert!(matches!(result, DagCommandResult::Ok { .. }));
    assert!(hl.highlighted_ids.is_empty());
}

/// reset_view restores zoom to 1.0 and pan to [0, 0].
#[test]
fn cmd_reset_view_restores_defaults() {
    let dag = three_node_dag();
    let (mut viewer, mut hl) = make_viewer(&dag);
    viewer.zoom = 3.5;
    viewer.viewport_offset = [100.0, -200.0];

    let result = execute_dag_command(&mut viewer, &mut hl, DagViewerCommand::ResetView);

    assert!(matches!(result, DagCommandResult::Ok { .. }));
    assert!((viewer.zoom - 1.0).abs() < 1e-6, "zoom must be 1.0 after reset");
    assert_eq!(viewer.viewport_offset, [0.0, 0.0]);
}

// ---------------------------------------------------------------------------
// parse_dag_command — JSON wire layer
// ---------------------------------------------------------------------------

#[test]
fn parse_focus_node_valid() {
    let args = serde_json::json!({"id": "phantom_agents::dispatch"});
    let cmd = parse_dag_command("dag.focus_node", &args).unwrap();
    assert!(
        matches!(cmd, DagViewerCommand::FocusNode { ref id } if id == "phantom_agents::dispatch"),
    );
}

#[test]
fn parse_focus_node_missing_id_is_err() {
    let result = parse_dag_command("dag.focus_node", &serde_json::json!({}));
    assert!(result.is_err());
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
    let args = serde_json::json!({"factor": 1.5});
    let cmd = parse_dag_command("dag.zoom", &args).unwrap();
    assert!(matches!(cmd, DagViewerCommand::Zoom { factor } if (factor - 1.5).abs() < 0.001));
}

#[test]
fn parse_zoom_missing_factor_is_err() {
    let result = parse_dag_command("dag.zoom", &serde_json::json!({}));
    assert!(result.is_err());
}

#[test]
fn parse_highlight_valid() {
    let args = serde_json::json!({"ids": ["x", "y", "z"]});
    let cmd = parse_dag_command("dag.highlight", &args).unwrap();
    assert!(
        matches!(cmd, DagViewerCommand::Highlight { ref ids } if ids.len() == 3),
        "expected Highlight with 3 ids",
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
    let result = parse_dag_command("dag.fly_to_moon", &serde_json::json!({}));
    assert!(result.is_err());
}

// ---------------------------------------------------------------------------
// DagCommandResult serialisation
// ---------------------------------------------------------------------------

#[test]
fn result_ok_serialises_to_json() {
    let r = DagCommandResult::Ok { message: "done".into() };
    let json = r.to_json().unwrap();
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["status"], "ok");
    assert_eq!(v["message"], "done");
}

#[test]
fn result_not_found_serialises_to_json() {
    let r = DagCommandResult::NotFound { id: "ghost".into() };
    let json = r.to_json().unwrap();
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["status"], "not_found");
    assert_eq!(v["id"], "ghost");
}

#[test]
fn result_invalid_args_serialises_to_json() {
    let r = DagCommandResult::InvalidArgs { reason: "bad input".into() };
    let json = r.to_json().unwrap();
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["status"], "invalid_args");
    assert!(v["reason"].as_str().unwrap().contains("bad input"));
}

#[test]
fn result_partial_ok_serialises_skipped_list() {
    let r = DagCommandResult::PartialOk {
        message: "partial success".into(),
        skipped: vec!["ghost1".into()],
    };
    let json = r.to_json().unwrap();
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["status"], "partial_ok");
    let skipped = v["skipped"].as_array().unwrap();
    assert_eq!(skipped.len(), 1);
    assert_eq!(skipped[0], "ghost1");
}
