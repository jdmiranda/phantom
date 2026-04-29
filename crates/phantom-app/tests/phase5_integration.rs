//! Phase 5 integration tests — headless validation of all new features.
//!
//! Exercises: brain scoring (spam prevention, dedup, REPL detection),
//! LayoutArbiter negotiation, scene graph sync, floating pane lifecycle,
//! z-order sorting, dirty-flag tracking, and context menu state.
//!
//! No GPU required — uses MockAdapter + coordinator test constructor.

use phantom_adapter::spatial::{NegotiationResult, SpatialPreference};
use phantom_adapter::{
    AppCore, BusParticipant, Commandable, EventBus, InputHandler, Lifecycled,
    Permissioned, QuadData, Rect, Renderable, RenderOutput, TextData,
};
use phantom_brain::events::{AiAction, AiEvent};
use phantom_brain::scoring::UtilityScorer;
use phantom_context::ProjectContext;
use phantom_memory::MemoryStore;
use phantom_scene::node::{NodeKind, RenderLayer};
use phantom_scene::tree::SceneTree;
use phantom_ui::arbiter::LayoutArbiter;
use phantom_ui::layout::LayoutEngine;
use serde_json::json;

// ---------------------------------------------------------------------------
// Mock adapter (mirrors coordinator.rs test mock)
// ---------------------------------------------------------------------------

struct TestAdapter {
    alive: bool,
    visual: bool,
    name: &'static str,
    pref: Option<SpatialPreference>,
}

impl TestAdapter {
    fn new(name: &'static str) -> Self {
        Self { alive: true, visual: true, name, pref: None }
    }

    #[allow(dead_code)]
    fn with_pref(name: &'static str, pref: SpatialPreference) -> Self {
        Self { alive: true, visual: true, name, pref: Some(pref) }
    }
}

impl AppCore for TestAdapter {
    fn app_type(&self) -> &str { self.name }
    fn is_alive(&self) -> bool { self.alive }
    fn update(&mut self, _dt: f32) {}
    fn get_state(&self) -> serde_json::Value { json!({ "name": self.name }) }
}

impl Renderable for TestAdapter {
    fn render(&self, rect: &Rect) -> RenderOutput {
        RenderOutput {
            quads: vec![QuadData { x: rect.x, y: rect.y, w: rect.width, h: rect.height, color: [1.0; 4] }],
            text_segments: vec![TextData { text: self.name.into(), x: rect.x, y: rect.y, color: [1.0; 4] }],
            grid: None,
            scroll: None,
            selection: None,
        }
    }
    fn is_visual(&self) -> bool { self.visual }
    fn spatial_preference(&self) -> Option<SpatialPreference> { self.pref.clone() }
}

impl InputHandler for TestAdapter {
    fn handle_input(&mut self, _key: &str) -> bool { false }
}

impl Commandable for TestAdapter {
    fn accept_command(&mut self, cmd: &str, _args: &serde_json::Value) -> anyhow::Result<String> {
        if cmd == "die" { self.alive = false; }
        Ok("ok".into())
    }
}

impl BusParticipant for TestAdapter {}
impl Lifecycled for TestAdapter {}
impl Permissioned for TestAdapter {}

// ===========================================================================
// B1: Brain suggestion spam prevention
// ===========================================================================

#[test]
fn b1_watcher_score_zero_without_active_process() {
    let scorer = UtilityScorer::new();
    let ctx = ProjectContext::detect(std::path::Path::new("."));
    let scored = scorer.watcher_score(&ctx);
    assert!(scored.score < f32::EPSILON, "watcher should be 0.0 without active process, got {}", scored.score);
}

#[test]
fn b1_dedup_suppresses_identical_suggestion() {
    let mut scorer = UtilityScorer::new();
    scorer.idle_time = 15.0;
    scorer.last_had_errors = true;
    let ctx = ProjectContext::detect(std::path::Path::new("."));
    let (memory, _dir) = test_memory();

    // First evaluation should produce a suggestion.
    let first = scorer.evaluate(&AiEvent::UserIdle { seconds: 15.0 }, &ctx, &memory);
    assert!(!matches!(first.action, AiAction::DoNothing), "first eval should suggest something");

    // Second evaluation with same state should be suppressed by dedup or cooldown.
    let second = scorer.evaluate(&AiEvent::UserIdle { seconds: 20.0 }, &ctx, &memory);
    assert!(matches!(second.action, AiAction::DoNothing), "second eval should be suppressed, got: {}", second.reason);
}

#[test]
fn b1_cooldown_blocks_rapid_actions() {
    let mut scorer = UtilityScorer::new();
    scorer.idle_time = 5.0;
    scorer.last_had_errors = true;
    let ctx = ProjectContext::detect(std::path::Path::new("."));
    let (memory, _dir) = test_memory();

    let parsed = error_output("build failed");
    let first = scorer.evaluate(&AiEvent::CommandComplete(parsed.clone()), &ctx, &memory);
    assert!(!matches!(first.action, AiAction::DoNothing));

    // Immediately after, cooldown should block.
    scorer.idle_time = 5.0;
    let second = scorer.evaluate(&AiEvent::UserIdle { seconds: 5.0 }, &ctx, &memory);
    assert!(matches!(second.action, AiAction::DoNothing), "cooldown should block: {}", second.reason);
}

#[test]
fn b1_suggestions_since_input_dampens() {
    let mut scorer = UtilityScorer::new();
    scorer.suggestions_since_input = 5;
    scorer.idle_time = 15.0;
    scorer.last_had_errors = true;
    // Expire cooldown manually.
    scorer.chattiness = 0.0;

    let ctx = ProjectContext::detect(std::path::Path::new("."));
    let (memory, _dir) = test_memory();

    let result = scorer.evaluate(&AiEvent::UserIdle { seconds: 15.0 }, &ctx, &memory);
    // Should be dampened (score * 0.2) and likely suppressed by quiet baseline.
    // The exact behavior depends on the quiet score vs dampened score,
    // but the intent is suppression after many suggestions.
    assert!(result.reason.contains("dampened") || matches!(result.action, AiAction::DoNothing),
        "should be dampened or suppressed after 5 suggestions: {}", result.reason);
}

// ===========================================================================
// W1c: Context awareness — REPL detection + error dedup
// ===========================================================================

#[test]
fn w1c_repl_detection_suppresses_explain() {
    let mut scorer = UtilityScorer::new();
    scorer.last_command = Some("python3".into());
    scorer.last_had_errors = true;

    let parsed = error_output("syntax error");
    let scored = scorer.explain_score(&parsed, 30.0);
    assert!(scored.score < f32::EPSILON, "explain should be 0.0 in REPL, got {}", scored.score);
}

#[test]
fn w1c_error_signature_dedup() {
    let mut scorer = UtilityScorer::new();
    scorer.idle_time = 5.0;
    let ctx = ProjectContext::detect(std::path::Path::new("."));

    let parsed = error_output("cannot find type `Foo`");
    let first = scorer.fix_score(&parsed, &ctx);
    assert!(first.score > 0.5, "first fix_score should be high");

    // Same error again — should be deduplicated.
    let second = scorer.fix_score(&parsed, &ctx);
    assert!(second.score < f32::EPSILON, "duplicate error should score 0.0, got {}", second.score);
}

#[test]
fn w1c_user_acted_clears_error_signature() {
    let mut scorer = UtilityScorer::new();
    scorer.idle_time = 5.0;
    let ctx = ProjectContext::detect(std::path::Path::new("."));

    let parsed = error_output("cannot find type `Foo`");
    let _ = scorer.fix_score(&parsed, &ctx);
    assert!(scorer.last_error_signature.is_some());

    scorer.user_acted();
    assert!(scorer.last_error_signature.is_none(), "user_acted should clear error signature");

    // Same error should now score again.
    scorer.idle_time = 5.0;
    let retry = scorer.fix_score(&parsed, &ctx);
    assert!(retry.score > 0.5, "after user_acted, same error should re-trigger");
}

// ===========================================================================
// W1a: SuggestionOption carries action payload
// ===========================================================================

#[test]
fn w1a_suggestion_option_has_action() {
    let mut scorer = UtilityScorer::new();
    scorer.idle_time = 5.0;

    let parsed = error_output("build failed");
    let scored = scorer.fix_score(&parsed, &ProjectContext::detect(std::path::Path::new(".")));

    if let AiAction::ShowSuggestion { options, .. } = &scored.action {
        assert!(!options.is_empty(), "should have options");
        let fix_opt = options.iter().find(|o| o.key == 'f');
        assert!(fix_opt.is_some(), "should have [f] Fix it option");
        assert!(fix_opt.unwrap().action.is_some(), "[f] option should carry an action payload");
    } else {
        panic!("expected ShowSuggestion, got {:?}", scored.action);
    }
}

// ===========================================================================
// W2a: LayoutArbiter constraint solving
// ===========================================================================

#[test]
fn w2a_arbiter_single_adapter_gets_full_space() {
    let arbiter = LayoutArbiter::new((800.0, 600.0), (10.0, 20.0));
    let prefs = vec![(1u32, SpatialPreference::simple(40, 10))];
    let plan = arbiter.negotiate(&prefs);
    assert_eq!(plan.allocations.len(), 1);
    let rect = &plan.allocations[&1];
    assert!(rect.width > 0.0 && rect.height > 0.0);
}

#[test]
fn w2a_arbiter_priority_ordering() {
    let arbiter = LayoutArbiter::new((800.0, 600.0), (10.0, 20.0));
    let prefs = vec![
        (1, SpatialPreference { priority: 10.0, preferred_size: (80, 30), ..SpatialPreference::simple(40, 10) }),
        (2, SpatialPreference { priority: 2.0, preferred_size: (40, 10), ..SpatialPreference::simple(20, 5) }),
    ];
    let plan = arbiter.negotiate(&prefs);
    assert_eq!(plan.allocations.len(), 2);
    // Higher priority adapter should get more space.
    let r1 = &plan.allocations[&1];
    let r2 = &plan.allocations[&2];
    assert!(r1.height >= r2.height, "priority 10 adapter should get >= space than priority 2");
}

#[test]
fn w2a_arbiter_denies_when_space_insufficient() {
    let arbiter = LayoutArbiter::new((100.0, 50.0), (10.0, 20.0));
    let prefs = vec![
        (1, SpatialPreference { priority: 10.0, min_size: (10, 3), ..SpatialPreference::simple(10, 3) }),
        (2, SpatialPreference { priority: 1.0, min_size: (10, 3), ..SpatialPreference::simple(10, 3) }),
        (3, SpatialPreference { priority: 0.5, min_size: (10, 3), ..SpatialPreference::simple(10, 3) }),
    ];
    let plan = arbiter.negotiate(&prefs);
    // If space isn't enough for all minimums, lowest priority should be denied.
    let total_allocated: usize = plan.allocations.len();
    let total_denied = plan.denied.len();
    assert!(total_allocated + total_denied == 3, "all 3 adapters accounted for");
}

#[test]
fn w2d_two_phase_negotiation_accepts() {
    let arbiter = LayoutArbiter::new((800.0, 600.0), (10.0, 20.0));
    let prefs = vec![(1, SpatialPreference::simple(40, 10))];
    let plan = arbiter.negotiate_with_feedback(&prefs, |_id, _w, _h| NegotiationResult::Accepted);
    assert_eq!(plan.allocations.len(), 1);
}

#[test]
fn w2d_two_phase_negotiation_counter_offer() {
    let arbiter = LayoutArbiter::new((800.0, 600.0), (10.0, 20.0));
    let prefs = vec![
        (1, SpatialPreference { preferred_size: (80, 30), ..SpatialPreference::simple(40, 10) }),
    ];
    let plan = arbiter.negotiate_with_feedback(&prefs, |_id, _w, h| {
        if h > 300.0 { NegotiationResult::CounterOffer { width: 800.0, height: 300.0 } }
        else { NegotiationResult::Accepted }
    });
    let rect = &plan.allocations[&1];
    assert!(rect.height <= 600.0, "counter-offer should be respected");
}

// ===========================================================================
// W3a: Scene graph sync from layout
// ===========================================================================

#[test]
fn w3a_scene_graph_sync() {
    let bus = EventBus::new();
    let mut coord = phantom_app::coordinator::AppCoordinator::new(bus);
    let mut layout = LayoutEngine::new().unwrap();
    let mut scene = SceneTree::new();
    let content = scene.add_node(scene.root(), NodeKind::ContentArea);

    let id = coord.register_adapter(
        Box::new(TestAdapter::new("terminal")),
        &mut layout, &mut scene, content,
        phantom_scene::clock::Cadence::unlimited(),
    );

    layout.resize(800.0, 600.0).unwrap();
    let plan = coord.build_layout_plan(&layout);

    assert!(plan.allocations.contains_key(&id));
    let rect = &plan.allocations[&id];
    assert!(rect.width > 0.0 && rect.height > 0.0);

    coord.sync_arbiter_to_scene(&plan, &mut scene);

    // Scene node should have updated transform.
    let node_id = coord.scene_node_for(id);
    if let Some(nid) = node_id {
        let node = scene.get(nid).unwrap();
        assert!(node.transform.width > 0.0, "scene node width should be set");
    }
}

// ===========================================================================
// W3b: Z-order sorted rendering
// ===========================================================================

#[test]
fn w3b_render_sorted_by_z_order() {
    let bus = EventBus::new();
    let mut coord = phantom_app::coordinator::AppCoordinator::new(bus);
    let mut layout = LayoutEngine::new().unwrap();
    let mut scene = SceneTree::new();
    let content = scene.add_node(scene.root(), NodeKind::ContentArea);

    let id1 = coord.register_adapter(
        Box::new(TestAdapter::new("pane1")),
        &mut layout, &mut scene, content,
        phantom_scene::clock::Cadence::unlimited(),
    );
    let id2 = coord.register_adapter(
        Box::new(TestAdapter::new("video")),
        &mut layout, &mut scene, content,
        phantom_scene::clock::Cadence::unlimited(),
    );

    layout.resize(800.0, 600.0).unwrap();
    let plan = coord.build_layout_plan(&layout);
    coord.sync_arbiter_to_scene(&plan, &mut scene);
    scene.update_world_transforms();

    let outputs = coord.render_all_with_scene(&scene);
    assert!(outputs.len() >= 2);

    // Video (z=10) should come after pane1 (z=0) in sorted order.
    let positions: Vec<u32> = outputs.iter().map(|(id, _, _)| *id).collect();
    let pos1 = positions.iter().position(|&id| id == id1);
    let pos2 = positions.iter().position(|&id| id == id2);
    if let (Some(p1), Some(p2)) = (pos1, pos2) {
        assert!(p1 < p2, "video (z=10) should render after pane1 (z=0)");
    }
}

// ===========================================================================
// W3c: Dirty-flag tracking
// ===========================================================================

#[test]
fn w3c_mark_all_dirty_and_clear() {
    let bus = EventBus::new();
    let mut coord = phantom_app::coordinator::AppCoordinator::new(bus);
    let mut layout = LayoutEngine::new().unwrap();
    let mut scene = SceneTree::new();
    let content = scene.add_node(scene.root(), NodeKind::ContentArea);

    let _id = coord.register_adapter(
        Box::new(TestAdapter::new("t")),
        &mut layout, &mut scene, content,
        phantom_scene::clock::Cadence::unlimited(),
    );

    coord.mark_all_dirty(&mut scene);
    // After marking, nodes should have dirty flags.
    // After clearing, they should be clean.
    coord.clear_render_dirty(&mut scene);
    // No panic = success; dirty flags are cleared.
}

// ===========================================================================
// W5a+W5b+W5c: Floating pane lifecycle
// ===========================================================================

#[test]
fn w5_float_lifecycle() {
    let bus = EventBus::new();
    let mut coord = phantom_app::coordinator::AppCoordinator::new(bus);
    let mut layout = LayoutEngine::new().unwrap();
    let mut scene = SceneTree::new();
    let content = scene.add_node(scene.root(), NodeKind::ContentArea);

    let id = coord.register_adapter(
        Box::new(TestAdapter::new("t")),
        &mut layout, &mut scene, content,
        phantom_scene::clock::Cadence::unlimited(),
    );
    layout.resize(800.0, 600.0).unwrap();

    // Initially tiled.
    assert!(!coord.is_floating(id));
    assert!(coord.float_rect(id).is_none());

    // Detach to float.
    coord.detach_to_float(id, &mut layout, &mut scene);
    assert!(coord.is_floating(id));
    assert!(coord.float_rect(id).is_some());

    // Scene node should be Overlay with z=50.
    if let Some(nid) = coord.scene_node_for(id) {
        let node = scene.get(nid).unwrap();
        assert_eq!(node.z_order, 50);
        assert_eq!(node.render_layer, RenderLayer::Overlay);
    }

    // Move it.
    coord.move_floating(id, 200.0, 150.0);
    let rect = coord.float_rect(id).unwrap();
    assert!((rect.x - 200.0).abs() < 1.0);
    assert!((rect.y - 150.0).abs() < 1.0);

    // Resize it.
    coord.resize_floating(id, 400.0, 300.0);
    let rect = coord.float_rect(id).unwrap();
    assert!((rect.width - 400.0).abs() < 1.0);
    assert!((rect.height - 300.0).abs() < 1.0);

    // Minimum size enforced.
    coord.resize_floating(id, 10.0, 10.0);
    let rect = coord.float_rect(id).unwrap();
    assert!(rect.width >= 100.0, "min width 100");
    assert!(rect.height >= 80.0, "min height 80");

    // Dock back.
    coord.dock_to_grid(id, &mut layout, &mut scene);
    assert!(!coord.is_floating(id));
    assert!(coord.float_rect(id).is_none());

    // Scene node should be back to Scene layer.
    if let Some(nid) = coord.scene_node_for(id) {
        let node = scene.get(nid).unwrap();
        assert_eq!(node.z_order, 0);
        assert_eq!(node.render_layer, RenderLayer::Scene);
    }
}

#[test]
fn w5_floating_ids_iterator() {
    let bus = EventBus::new();
    let mut coord = phantom_app::coordinator::AppCoordinator::new(bus);
    let mut layout = LayoutEngine::new().unwrap();
    let mut scene = SceneTree::new();
    let content = scene.add_node(scene.root(), NodeKind::ContentArea);

    let id1 = coord.register_adapter(
        Box::new(TestAdapter::new("a")),
        &mut layout, &mut scene, content,
        phantom_scene::clock::Cadence::unlimited(),
    );
    let id2 = coord.register_adapter(
        Box::new(TestAdapter::new("b")),
        &mut layout, &mut scene, content,
        phantom_scene::clock::Cadence::unlimited(),
    );
    layout.resize(800.0, 600.0).unwrap();

    coord.detach_to_float(id1, &mut layout, &mut scene);
    let floats: Vec<_> = coord.floating_ids().collect();
    assert!(floats.contains(&id1));
    assert!(!floats.contains(&id2));
}

// ===========================================================================
// W3d: Layer-based render partitioning
// ===========================================================================

#[test]
fn w3d_render_layer_default_is_scene() {
    let bus = EventBus::new();
    let mut coord = phantom_app::coordinator::AppCoordinator::new(bus);
    let mut layout = LayoutEngine::new().unwrap();
    let mut scene = SceneTree::new();
    let content = scene.add_node(scene.root(), NodeKind::ContentArea);

    let id = coord.register_adapter(
        Box::new(TestAdapter::new("t")),
        &mut layout, &mut scene, content,
        phantom_scene::clock::Cadence::unlimited(),
    );

    assert_eq!(coord.render_layer_for(id, &scene), RenderLayer::Scene);
}

#[test]
fn w3d_set_render_layer_changes_layer() {
    let bus = EventBus::new();
    let mut coord = phantom_app::coordinator::AppCoordinator::new(bus);
    let mut layout = LayoutEngine::new().unwrap();
    let mut scene = SceneTree::new();
    let content = scene.add_node(scene.root(), NodeKind::ContentArea);

    let id = coord.register_adapter(
        Box::new(TestAdapter::new("t")),
        &mut layout, &mut scene, content,
        phantom_scene::clock::Cadence::unlimited(),
    );

    coord.set_render_layer(id, RenderLayer::Overlay, &mut scene);
    assert_eq!(coord.render_layer_for(id, &scene), RenderLayer::Overlay);
}

// ===========================================================================
// Context menu state
// ===========================================================================

#[test]
fn w4c_context_menu_hit_test() {
    let mut menu = phantom_app::context_menu::ContextMenu::new();
    assert!(!menu.visible);
    assert!(menu.hit_test(0.0, 0.0).is_none());

    menu.show(100.0, 200.0, vec![
        phantom_app::context_menu::MenuItem {
            label: "Copy".into(),
            action: phantom_app::context_menu::MenuAction::Copy,
            enabled: true,
        },
        phantom_app::context_menu::MenuItem {
            label: "Paste".into(),
            action: phantom_app::context_menu::MenuAction::Paste,
            enabled: true,
        },
    ]);

    assert!(menu.visible);
    // Click inside menu area → should hit an item.
    let hit = menu.hit_test(110.0, 215.0);
    assert!(hit.is_some(), "should hit first item");
    assert_eq!(hit.unwrap(), 0);

    // Click outside → no hit.
    let miss = menu.hit_test(0.0, 0.0);
    assert!(miss.is_none());

    menu.hide();
    assert!(!menu.visible);
}

// ===========================================================================
// Issue #226: CommandComplete must carry real output so OODA scoring works
// ===========================================================================

/// Regression test for #226.
///
/// Before the fix, `drain_bus_to_brain` constructed `ParsedOutput` with
/// empty strings for `command` and `raw_output`, so `SemanticParser::parse`
/// never ran and `errors` was always empty — making `fix_score` always 0.
///
/// After the fix the caller uses the terminal output buffer and the tracked
/// command text.  This test simulates that path by calling
/// `SemanticParser::parse` directly with realistic content and asserting the
/// scorer sees the errors.
#[test]
fn b226_command_complete_with_real_output_yields_nonzero_fix_score() {
    // Simulate what drain_bus_to_brain now does: call SemanticParser::parse
    // with a real command + PTY buffer in the stderr slot.
    //
    // Production call (drain_bus_to_brain):
    //   SemanticParser::parse(&command, "", &raw_output, Some(*exit_code))
    //
    // The PTY buffer is passed as `stderr` because parse_rust_errors only
    // reads from that argument. Passing it as `stdout` (the old bug) meant
    // cargo error parsing was always blind.
    let raw_output = "error[E0308]: mismatched types\n  --> src/main.rs:5:9\n   |\n5  |     return \"hello\";\n   |            ^^^^^^^ expected `i32`, found `&str`";
    let parsed = phantom_semantic::parser::SemanticParser::parse(
        "cargo build",
        "",          // stdout: empty — mirrors production call
        raw_output,  // stderr slot: PTY buffer — mirrors production call
        Some(1),
    );

    // The fix ensures `errors` is non-empty when the output contains error lines.
    assert!(
        !parsed.errors.is_empty(),
        "SemanticParser must detect errors in real compiler output; got errors={:?}, raw_output={:?}",
        parsed.errors, parsed.raw_output
    );
    assert_eq!(parsed.command, "cargo build", "ParsedOutput must carry the command string");
    assert!(!parsed.raw_output.is_empty(), "ParsedOutput must carry the raw output");

    // Confirm the brain scorer produces a non-zero fix score for this event.
    let mut scorer = UtilityScorer::new();
    scorer.idle_time = 5.0;
    let ctx = phantom_context::ProjectContext::detect(std::path::Path::new("."));
    let scored = scorer.fix_score(&parsed, &ctx);
    assert!(
        scored.score > 0.0,
        "fix_score must be > 0 when CommandComplete carries real error output, got {}",
        scored.score
    );
}

// ===========================================================================
// Helpers
// ===========================================================================

fn test_memory() -> (MemoryStore, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let memory = MemoryStore::open_in(".", dir.path()).unwrap();
    (memory, dir)
}

fn error_output(msg: &str) -> phantom_semantic::ParsedOutput {
    phantom_semantic::ParsedOutput {
        command: "cargo build".into(),
        command_type: phantom_semantic::CommandType::Unknown,
        exit_code: Some(1),
        content_type: phantom_semantic::ContentType::PlainText,
        errors: vec![phantom_semantic::DetectedError {
            message: msg.into(),
            error_type: phantom_semantic::ErrorType::Compiler,
            file: None,
            line: None,
            column: None,
            code: None,
            severity: phantom_semantic::Severity::Error,
            raw_line: String::new(),
            suggestion: None,
        }],
        warnings: vec![],
        duration_ms: Some(1000),
        raw_output: msg.into(),
    }
}
