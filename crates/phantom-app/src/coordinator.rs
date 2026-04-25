//! App coordinator — orchestrates adapters, layout, scene, and event bus.
//!
//! The `AppCoordinator` is the central loop driver. It owns the
//! `AppRegistry` and `EventBus`, maps adapters to layout panes and
//! scene nodes, and governs per-adapter tick cadences.

use std::collections::HashMap;
use std::time::Duration;

use phantom_adapter::{AppAdapter, AppId, AppRegistry, EventBus, Rect, RenderOutput};
use phantom_scene::clock::Cadence;
use phantom_scene::node::{NodeId, NodeKind};
use phantom_scene::tree::SceneTree;
use phantom_ui::layout::{LayoutEngine, PaneId};

/// Orchestrates all registered adapters, the event bus, layout panes,
/// and scene nodes within a single frame loop.
pub struct AppCoordinator {
    registry: AppRegistry,
    bus: EventBus,
    pane_map: HashMap<PaneId, AppId>,
    app_pane_map: HashMap<AppId, PaneId>,
    scene_map: HashMap<AppId, NodeId>,
    cadences: HashMap<AppId, Cadence>,
    focused: Option<AppId>,
}

impl AppCoordinator {
    /// Create a new coordinator with the given event bus.
    pub fn new(bus: EventBus) -> Self {
        Self {
            registry: AppRegistry::new(),
            bus,
            pane_map: HashMap::new(),
            app_pane_map: HashMap::new(),
            scene_map: HashMap::new(),
            cadences: HashMap::new(),
            focused: None,
        }
    }

    /// Register an adapter, wire it into layout and scene, and transition
    /// it to `Running`. Returns the assigned `AppId`.
    ///
    /// Sets focus to this adapter if it is the first one registered.
    pub fn register_adapter(
        &mut self,
        adapter: Box<dyn AppAdapter>,
        layout: &mut LayoutEngine,
        scene: &mut SceneTree,
        content_node: NodeId,
        cadence: Cadence,
    ) -> AppId {
        let is_first = self.registry.count() == 0;

        let id = self.registry.register(adapter);

        // Invoke the lifecycle init hook before transitioning to Running.
        if let Some(adapter) = self.registry.get_adapter_mut(id) {
            if let Err(e) = adapter.on_init() {
                log::error!("Adapter {id} on_init() failed: {e}");
            }
        }

        self.registry.ready(id);

        // Layout pane — warn if layout can't accommodate a new pane.
        match layout.add_pane() {
            Ok(pane_id) => {
                self.pane_map.insert(pane_id, id);
                self.app_pane_map.insert(id, pane_id);
            }
            Err(e) => {
                log::warn!("Adapter {id} registered without layout pane: {e}");
            }
        }

        // Scene node.
        let node_id = scene.add_node(content_node, NodeKind::Pane);
        self.scene_map.insert(id, node_id);

        self.cadences.insert(id, cadence);

        if is_first {
            self.focused = Some(id);
        }

        id
    }

    /// Register an adapter using an **existing** layout pane and scene node.
    ///
    /// Unlike `register_adapter`, this does NOT create a new layout pane or
    /// scene node — it binds the adapter to IDs that the legacy system
    /// already created. Used during the strangler-fig migration.
    pub fn register_adapter_at_pane(
        &mut self,
        adapter: Box<dyn AppAdapter>,
        pane_id: PaneId,
        scene_node: NodeId,
        cadence: Cadence,
    ) -> AppId {
        let is_first = self.registry.count() == 0;

        let id = self.registry.register(adapter);

        if let Some(adapter) = self.registry.get_adapter_mut(id) {
            if let Err(e) = adapter.on_init() {
                log::error!("Adapter {id} on_init() failed: {e}");
            }
        }

        self.registry.ready(id);

        // Use the provided PaneId (already in the layout).
        self.pane_map.insert(pane_id, id);
        self.app_pane_map.insert(id, pane_id);

        // Use the provided scene node (already in the scene graph).
        self.scene_map.insert(id, scene_node);

        self.cadences.insert(id, cadence);

        if is_first {
            self.focused = Some(id);
        }

        id
    }

    /// Remove an adapter: kill it, strip layout pane and scene node,
    /// shift focus if the removed adapter was focused.
    pub fn remove_adapter(
        &mut self,
        app_id: AppId,
        layout: &mut LayoutEngine,
        scene: &mut SceneTree,
    ) {
        self.registry.kill(app_id);

        if let Some(pane_id) = self.app_pane_map.remove(&app_id) {
            self.pane_map.remove(&pane_id);
            let _ = layout.remove_pane(pane_id);
        }

        if let Some(node_id) = self.scene_map.remove(&app_id) {
            scene.remove_node(node_id);
        }

        self.cadences.remove(&app_id);

        if self.focused == Some(app_id) {
            self.focused = self.registry.all_running().into_iter().next();
        }
    }

    /// Update all running adapters whose cadence fires this frame.
    ///
    /// After updating, drains bus messages and delivers them to subscribers.
    pub fn update_all(&mut self, dt: Duration) {
        let running = self.registry.all_running();
        let dt_secs = dt.as_secs_f32();

        for id in &running {
            let Some(cadence) = self.cadences.get_mut(id) else {
                continue;
            };
            if !cadence.should_tick(dt) {
                continue;
            }
            let Some(adapter) = self.registry.get_adapter_mut(*id) else {
                continue;
            };
            adapter.update(dt_secs);
        }

        // Deliver bus messages to subscribers.
        for id in &running {
            let msgs = self.bus.drain_for(*id);
            if msgs.is_empty() {
                continue;
            }
            let Some(adapter) = self.registry.get_adapter_mut(*id) else {
                continue;
            };
            for msg in &msgs {
                adapter.on_message(msg);
            }
        }
    }

    /// Collect render outputs from all visual, running adapters.
    ///
    /// Returns an owned `Vec` so the caller can use `&mut` GPU resources
    /// after the call without borrow conflicts.
    pub fn render_all(&self, layout: &LayoutEngine) -> Vec<(AppId, Rect, RenderOutput)> {
        let mut outputs = Vec::new();

        for id in self.registry.all_running() {
            let Some(entry) = self.registry.get(id) else {
                continue;
            };
            if !entry.visual {
                continue;
            }
            let Some(pane_id) = self.app_pane_map.get(&id) else {
                continue;
            };
            let rect = match layout.get_pane_rect(*pane_id) {
                Ok(r) => Rect {
                    x: r.x,
                    y: r.y,
                    width: r.width,
                    height: r.height,
                },
                Err(_) => continue,
            };
            let Some(adapter) = self.registry.get_adapter(id) else {
                continue;
            };
            let output = adapter.render(&rect);
            outputs.push((id, rect, output));
        }

        outputs
    }

    /// Route a key event to the focused adapter.
    ///
    /// Returns `true` if the input was consumed. Skips dispatch if the
    /// focused adapter does not accept input (capability guard for Phase 2
    /// partial adapters).
    pub fn route_input(&mut self, key: &str) -> bool {
        let Some(id) = self.focused else {
            return false;
        };
        let Some(entry) = self.registry.get(id) else {
            return false;
        };
        if !entry.accepts_input {
            return false;
        }
        let Some(adapter) = self.registry.get_adapter_mut(id) else {
            return false;
        };
        adapter.handle_input(key)
    }

    /// Set focus to a specific adapter.
    pub fn set_focus(&mut self, app_id: AppId) {
        self.focused = Some(app_id);
    }

    /// The currently focused adapter, if any.
    pub fn focused(&self) -> Option<AppId> {
        self.focused
    }

    /// Get the current state JSON from an adapter.
    pub fn get_state(&self, app_id: AppId) -> Option<serde_json::Value> {
        let adapter = self.registry.get_adapter(app_id)?;
        Some(adapter.get_state())
    }

    /// Send a command to a specific adapter.
    ///
    /// Returns an error if the adapter does not exist or does not accept
    /// commands (capability guard for Phase 2 partial adapters).
    pub fn send_command(
        &mut self,
        app_id: AppId,
        cmd: &str,
        args: &serde_json::Value,
    ) -> anyhow::Result<String> {
        let Some(entry) = self.registry.get(app_id) else {
            return Err(anyhow::anyhow!("adapter not found: {app_id}"));
        };
        if !entry.accepts_commands {
            return Err(anyhow::anyhow!(
                "adapter {app_id} ({}) does not accept commands",
                entry.app_type
            ));
        }
        let Some(adapter) = self.registry.get_adapter_mut(app_id) else {
            return Err(anyhow::anyhow!("adapter not found: {app_id}"));
        };
        adapter.accept_command(cmd, args)
    }

    /// Number of adapters currently registered (including dead, pre-GC).
    pub fn adapter_count(&self) -> usize {
        self.registry.count()
    }

    /// All app IDs that are currently running.
    pub fn all_app_ids(&self) -> Vec<AppId> {
        self.registry.all_running()
    }

    /// The layout pane associated with an adapter.
    pub fn pane_id_for(&self, app_id: AppId) -> Option<PaneId> {
        self.app_pane_map.get(&app_id).copied()
    }

    /// Look up which adapter owns a given layout pane.
    pub fn app_id_for_pane(&self, pane_id: PaneId) -> Option<AppId> {
        self.pane_map.get(&pane_id).copied()
    }

    /// Immutable access to the event bus.
    pub fn bus(&self) -> &EventBus {
        &self.bus
    }

    /// Mutable access to the event bus.
    pub fn bus_mut(&mut self) -> &mut EventBus {
        &mut self.bus
    }

    /// Immutable access to the app registry.
    pub fn registry(&self) -> &AppRegistry {
        &self.registry
    }

    /// Mutable access to the app registry.
    pub fn registry_mut(&mut self) -> &mut AppRegistry {
        &mut self.registry
    }

    /// Test-only constructor that skips layout/scene requirements.
    #[cfg(test)]
    fn new_test(bus: EventBus) -> Self {
        Self::new(bus)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use phantom_adapter::{
        AppState, BusParticipant, Commandable, InputHandler, Lifecycled, Permissioned,
        QuadData, Renderable, TextData,
    };
    use phantom_adapter::AppCore;
    use serde_json::json;
    use std::sync::atomic::{AtomicU32, Ordering};

    // ── Mock adapter ───────────────────────────────────────────────────

    static UPDATE_COUNTER: AtomicU32 = AtomicU32::new(0);

    struct MockAdapter {
        alive: bool,
        visual: bool,
        name: &'static str,
        input_log: Vec<String>,
        command_log: Vec<String>,
    }

    impl MockAdapter {
        fn new(name: &'static str) -> Self {
            Self {
                alive: true,
                visual: true,
                name,
                input_log: Vec::new(),
                command_log: Vec::new(),
            }
        }

        fn headless(name: &'static str) -> Self {
            Self {
                alive: true,
                visual: false,
                name,
                input_log: Vec::new(),
                command_log: Vec::new(),
            }
        }
    }

    impl AppCore for MockAdapter {
        fn app_type(&self) -> &str {
            self.name
        }

        fn is_alive(&self) -> bool {
            self.alive
        }

        fn update(&mut self, _dt: f32) {
            UPDATE_COUNTER.fetch_add(1, Ordering::Relaxed);
        }

        fn get_state(&self) -> serde_json::Value {
            json!({ "name": self.name, "alive": self.alive })
        }
    }

    impl Renderable for MockAdapter {
        fn render(&self, rect: &Rect) -> RenderOutput {
            RenderOutput {
                quads: vec![QuadData {
                    x: rect.x,
                    y: rect.y,
                    w: rect.width,
                    h: rect.height,
                    color: [1.0; 4],
                }],
                text_segments: vec![TextData {
                    text: String::from(self.name),
                    x: rect.x,
                    y: rect.y,
                    color: [1.0; 4],
                }],
                grid: None,
            }
        }

        fn is_visual(&self) -> bool {
            self.visual
        }
    }

    impl InputHandler for MockAdapter {
        fn handle_input(&mut self, key: &str) -> bool {
            self.input_log.push(key.to_string());
            key == "q"
        }
    }

    impl Commandable for MockAdapter {
        fn accept_command(
            &mut self,
            cmd: &str,
            _args: &serde_json::Value,
        ) -> anyhow::Result<String> {
            self.command_log.push(cmd.to_string());
            if cmd == "die" {
                self.alive = false;
            }
            Ok(String::from("ok"))
        }
    }

    impl BusParticipant for MockAdapter {}
    impl Lifecycled for MockAdapter {}
    impl Permissioned for MockAdapter {}

    // ── Helpers ─────────────────────────────────────────────────────────

    /// Register a mock adapter with the coordinator using real layout/scene.
    fn register_mock(
        coord: &mut AppCoordinator,
        layout: &mut LayoutEngine,
        scene: &mut SceneTree,
        content_node: NodeId,
        name: &'static str,
    ) -> AppId {
        coord.register_adapter(
            Box::new(MockAdapter::new(name)),
            layout,
            scene,
            content_node,
            Cadence::unlimited(),
        )
    }

    // ── Tests ──────────────────────────────────────────────────────────

    #[test]
    fn test_register_adapter_assigns_unique_id() {
        let mut coord = AppCoordinator::new_test(EventBus::new());
        let mut layout = LayoutEngine::new().unwrap();
        let mut scene = SceneTree::new();
        let content = scene.add_node(scene.root(), NodeKind::ContentArea);

        let id1 = register_mock(&mut coord, &mut layout, &mut scene, content, "a");
        let id2 = register_mock(&mut coord, &mut layout, &mut scene, content, "b");

        assert_ne!(id1, id2);
    }

    #[test]
    fn test_register_adapter_transitions_to_running() {
        let mut coord = AppCoordinator::new_test(EventBus::new());
        let mut layout = LayoutEngine::new().unwrap();
        let mut scene = SceneTree::new();
        let content = scene.add_node(scene.root(), NodeKind::ContentArea);

        let id = register_mock(&mut coord, &mut layout, &mut scene, content, "term");

        let entry = coord.registry().get(id).unwrap();
        assert_eq!(entry.state, AppState::Running);
    }

    #[test]
    fn test_remove_adapter_transitions_to_dead() {
        let mut coord = AppCoordinator::new_test(EventBus::new());
        let mut layout = LayoutEngine::new().unwrap();
        let mut scene = SceneTree::new();
        let content = scene.add_node(scene.root(), NodeKind::ContentArea);

        let id = register_mock(&mut coord, &mut layout, &mut scene, content, "term");
        coord.remove_adapter(id, &mut layout, &mut scene);

        let entry = coord.registry().get(id).unwrap();
        assert_eq!(entry.state, AppState::Dead);
    }

    #[test]
    fn test_update_all_calls_adapter_update() {
        let mut coord = AppCoordinator::new_test(EventBus::new());
        let mut layout = LayoutEngine::new().unwrap();
        let mut scene = SceneTree::new();
        let content = scene.add_node(scene.root(), NodeKind::ContentArea);

        let before = UPDATE_COUNTER.load(Ordering::Relaxed);
        let _id = register_mock(&mut coord, &mut layout, &mut scene, content, "up");

        coord.update_all(Duration::from_millis(16));

        let after = UPDATE_COUNTER.load(Ordering::Relaxed);
        assert!(after > before, "update should have been called at least once");
    }

    #[test]
    fn test_render_all_returns_outputs_for_visual_adapters() {
        let mut coord = AppCoordinator::new_test(EventBus::new());
        let mut layout = LayoutEngine::new().unwrap();
        let mut scene = SceneTree::new();
        let content = scene.add_node(scene.root(), NodeKind::ContentArea);

        let vis_id = register_mock(&mut coord, &mut layout, &mut scene, content, "vis");

        // Register a headless adapter.
        coord.register_adapter(
            Box::new(MockAdapter::headless("headless")),
            &mut layout,
            &mut scene,
            content,
            Cadence::unlimited(),
        );

        // Must compute layout before querying rects.
        layout.resize(800.0, 600.0).unwrap();

        let outputs = coord.render_all(&layout);

        // Only the visual adapter should produce output.
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].0, vis_id);
        assert!(!outputs[0].2.quads.is_empty());
    }

    #[test]
    fn test_route_input_to_focused_adapter() {
        let mut coord = AppCoordinator::new_test(EventBus::new());
        let mut layout = LayoutEngine::new().unwrap();
        let mut scene = SceneTree::new();
        let content = scene.add_node(scene.root(), NodeKind::ContentArea);

        let _id = register_mock(&mut coord, &mut layout, &mut scene, content, "input");

        // "q" is consumed by the mock.
        assert!(coord.route_input("q"));
        // "x" is not consumed.
        assert!(!coord.route_input("x"));
    }

    #[test]
    fn test_set_focus_changes_target() {
        let mut coord = AppCoordinator::new_test(EventBus::new());
        let mut layout = LayoutEngine::new().unwrap();
        let mut scene = SceneTree::new();
        let content = scene.add_node(scene.root(), NodeKind::ContentArea);

        let id1 = register_mock(&mut coord, &mut layout, &mut scene, content, "a");
        let id2 = register_mock(&mut coord, &mut layout, &mut scene, content, "b");

        // First adapter gets auto-focus.
        assert_eq!(coord.focused(), Some(id1));

        coord.set_focus(id2);
        assert_eq!(coord.focused(), Some(id2));
    }

    #[test]
    fn test_adapter_count_reflects_registrations() {
        let mut coord = AppCoordinator::new_test(EventBus::new());
        let mut layout = LayoutEngine::new().unwrap();
        let mut scene = SceneTree::new();
        let content = scene.add_node(scene.root(), NodeKind::ContentArea);

        assert_eq!(coord.adapter_count(), 0);

        register_mock(&mut coord, &mut layout, &mut scene, content, "a");
        assert_eq!(coord.adapter_count(), 1);

        register_mock(&mut coord, &mut layout, &mut scene, content, "b");
        assert_eq!(coord.adapter_count(), 2);
    }

    #[test]
    fn test_get_state_returns_adapter_state_json() {
        let mut coord = AppCoordinator::new_test(EventBus::new());
        let mut layout = LayoutEngine::new().unwrap();
        let mut scene = SceneTree::new();
        let content = scene.add_node(scene.root(), NodeKind::ContentArea);

        let id = register_mock(&mut coord, &mut layout, &mut scene, content, "state");

        let state = coord.get_state(id);
        assert!(state.is_some());
        let val = state.unwrap();
        assert_eq!(val["name"], "state");
        assert_eq!(val["alive"], true);
    }

    #[test]
    fn test_send_command_proxies_to_adapter() {
        let mut coord = AppCoordinator::new_test(EventBus::new());
        let mut layout = LayoutEngine::new().unwrap();
        let mut scene = SceneTree::new();
        let content = scene.add_node(scene.root(), NodeKind::ContentArea);

        let id = register_mock(&mut coord, &mut layout, &mut scene, content, "cmd");

        let result = coord.send_command(id, "test_cmd", &json!({}));
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "ok");
    }

    #[test]
    fn test_send_command_to_missing_adapter_returns_error() {
        let mut coord = AppCoordinator::new_test(EventBus::new());
        let result = coord.send_command(999, "nope", &json!({}));
        assert!(result.is_err());
    }

    #[test]
    fn test_get_state_missing_returns_none() {
        let coord = AppCoordinator::new_test(EventBus::new());
        assert!(coord.get_state(999).is_none());
    }

    #[test]
    fn test_pane_id_for_returns_pane() {
        let mut coord = AppCoordinator::new_test(EventBus::new());
        let mut layout = LayoutEngine::new().unwrap();
        let mut scene = SceneTree::new();
        let content = scene.add_node(scene.root(), NodeKind::ContentArea);

        let id = register_mock(&mut coord, &mut layout, &mut scene, content, "pane");
        assert!(coord.pane_id_for(id).is_some());
    }

    #[test]
    fn test_remove_shifts_focus() {
        let mut coord = AppCoordinator::new_test(EventBus::new());
        let mut layout = LayoutEngine::new().unwrap();
        let mut scene = SceneTree::new();
        let content = scene.add_node(scene.root(), NodeKind::ContentArea);

        let id1 = register_mock(&mut coord, &mut layout, &mut scene, content, "a");
        let id2 = register_mock(&mut coord, &mut layout, &mut scene, content, "b");

        assert_eq!(coord.focused(), Some(id1));
        coord.remove_adapter(id1, &mut layout, &mut scene);

        // Focus should shift to id2.
        assert_eq!(coord.focused(), Some(id2));
    }

    #[test]
    fn test_remove_last_adapter_clears_focus() {
        let mut coord = AppCoordinator::new_test(EventBus::new());
        let mut layout = LayoutEngine::new().unwrap();
        let mut scene = SceneTree::new();
        let content = scene.add_node(scene.root(), NodeKind::ContentArea);

        let id = register_mock(&mut coord, &mut layout, &mut scene, content, "only");
        coord.remove_adapter(id, &mut layout, &mut scene);

        assert_eq!(coord.focused(), None);
    }

    #[test]
    fn test_all_app_ids_returns_running() {
        let mut coord = AppCoordinator::new_test(EventBus::new());
        let mut layout = LayoutEngine::new().unwrap();
        let mut scene = SceneTree::new();
        let content = scene.add_node(scene.root(), NodeKind::ContentArea);

        let id1 = register_mock(&mut coord, &mut layout, &mut scene, content, "a");
        let id2 = register_mock(&mut coord, &mut layout, &mut scene, content, "b");

        let ids = coord.all_app_ids();
        assert!(ids.contains(&id1));
        assert!(ids.contains(&id2));
        assert_eq!(ids.len(), 2);
    }

    #[test]
    fn test_bus_accessors() {
        let mut coord = AppCoordinator::new_test(EventBus::new());
        assert_eq!(coord.bus().topic_count(), 0);
        let _ = coord.bus_mut().create_topic(0, "test", phantom_adapter::DataType::Text);
        assert_eq!(coord.bus().topic_count(), 1);
    }

    #[test]
    fn test_route_input_no_focus_returns_false() {
        let mut coord = AppCoordinator::new_test(EventBus::new());
        assert!(!coord.route_input("q"));
    }

    // ── Capability guard integration tests ─────────────────────────────

    /// A mock that refuses input and commands (Phase 2 partial adapter).
    struct PassiveAdapter;

    impl AppCore for PassiveAdapter {
        fn app_type(&self) -> &str { "passive" }
        fn is_alive(&self) -> bool { true }
        fn update(&mut self, _dt: f32) {}
        fn get_state(&self) -> serde_json::Value { json!({"type": "passive"}) }
    }

    impl Renderable for PassiveAdapter {
        fn render(&self, _rect: &Rect) -> RenderOutput { RenderOutput::default() }
        fn is_visual(&self) -> bool { false }
    }

    impl InputHandler for PassiveAdapter {
        fn handle_input(&mut self, _key: &str) -> bool { false }
        fn accepts_input(&self) -> bool { false }
    }

    impl Commandable for PassiveAdapter {
        fn accept_command(&mut self, _cmd: &str, _args: &serde_json::Value) -> anyhow::Result<String> {
            Ok("unreachable".into())
        }
        fn accepts_commands(&self) -> bool { false }
    }

    impl BusParticipant for PassiveAdapter {}
    impl Lifecycled for PassiveAdapter {}
    impl Permissioned for PassiveAdapter {}

    #[test]
    fn test_capability_guard_blocks_input_to_passive_adapter() {
        let mut coord = AppCoordinator::new_test(EventBus::new());
        let mut layout = LayoutEngine::new().unwrap();
        let mut scene = SceneTree::new();
        let content = scene.add_node(scene.root(), NodeKind::ContentArea);

        let id = coord.register_adapter(
            Box::new(PassiveAdapter),
            &mut layout, &mut scene, content,
            Cadence::unlimited(),
        );

        // PassiveAdapter is the only adapter, so it gets auto-focus.
        assert_eq!(coord.focused(), Some(id));

        // But route_input should return false because accepts_input is false.
        assert!(!coord.route_input("q"));
    }

    #[test]
    fn test_capability_guard_blocks_command_to_passive_adapter() {
        let mut coord = AppCoordinator::new_test(EventBus::new());
        let mut layout = LayoutEngine::new().unwrap();
        let mut scene = SceneTree::new();
        let content = scene.add_node(scene.root(), NodeKind::ContentArea);

        let id = coord.register_adapter(
            Box::new(PassiveAdapter),
            &mut layout, &mut scene, content,
            Cadence::unlimited(),
        );

        let result = coord.send_command(id, "test", &json!({}));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("does not accept commands"));
    }

    #[test]
    fn test_capability_flags_populated_at_registration() {
        let mut coord = AppCoordinator::new_test(EventBus::new());
        let mut layout = LayoutEngine::new().unwrap();
        let mut scene = SceneTree::new();
        let content = scene.add_node(scene.root(), NodeKind::ContentArea);

        // Normal adapter: all capabilities true.
        let id1 = register_mock(&mut coord, &mut layout, &mut scene, content, "full");
        let entry1 = coord.registry().get(id1).unwrap();
        assert!(entry1.visual);
        assert!(entry1.accepts_input);
        assert!(entry1.accepts_commands);

        // Passive adapter: visual=false, input=false, commands=false.
        let id2 = coord.register_adapter(
            Box::new(PassiveAdapter),
            &mut layout, &mut scene, content,
            Cadence::unlimited(),
        );
        let entry2 = coord.registry().get(id2).unwrap();
        assert!(!entry2.visual);
        assert!(!entry2.accepts_input);
        assert!(!entry2.accepts_commands);
    }

    #[test]
    fn test_headless_adapter_excluded_from_render_all() {
        let mut coord = AppCoordinator::new_test(EventBus::new());
        let mut layout = LayoutEngine::new().unwrap();
        let mut scene = SceneTree::new();
        let content = scene.add_node(scene.root(), NodeKind::ContentArea);

        // Register a visual and a headless adapter.
        let _vis = register_mock(&mut coord, &mut layout, &mut scene, content, "vis");
        coord.register_adapter(
            Box::new(MockAdapter::headless("bg")),
            &mut layout, &mut scene, content,
            Cadence::unlimited(),
        );

        layout.resize(800.0, 600.0).unwrap();
        let outputs = coord.render_all(&layout);

        // Only the visual adapter should produce output.
        assert_eq!(outputs.len(), 1);
    }

    #[test]
    fn test_full_lifecycle_register_update_render_remove() {
        let mut coord = AppCoordinator::new_test(EventBus::new());
        let mut layout = LayoutEngine::new().unwrap();
        let mut scene = SceneTree::new();
        let content = scene.add_node(scene.root(), NodeKind::ContentArea);

        // Register.
        let id = register_mock(&mut coord, &mut layout, &mut scene, content, "lifecycle");
        assert_eq!(coord.adapter_count(), 1);
        assert_eq!(coord.registry().get(id).unwrap().state, AppState::Running);

        // Update.
        coord.update_all(Duration::from_millis(16));

        // Render.
        layout.resize(800.0, 600.0).unwrap();
        let outputs = coord.render_all(&layout);
        assert_eq!(outputs.len(), 1);

        // Remove.
        coord.remove_adapter(id, &mut layout, &mut scene);
        assert_eq!(coord.registry().get(id).unwrap().state, AppState::Dead);
        assert_eq!(coord.focused(), None);
    }
}
