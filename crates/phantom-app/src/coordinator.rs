//! App coordinator — orchestrates adapters, layout, scene, and event bus.
//!
//! The `AppCoordinator` is the central loop driver. It owns the
//! `AppRegistry` and `EventBus`, maps adapters to layout panes and
//! scene nodes, and governs per-adapter tick cadences.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use phantom_adapter::{AppAdapter, AppId, AppRegistry, EventBus, Rect, RenderOutput};
use phantom_adapter::spatial::SpatialPreference;
use phantom_scene::clock::Cadence;
use phantom_scene::dirty::DirtyFlags;
use phantom_scene::node::{NodeId, NodeKind, RenderLayer};
use phantom_scene::tree::SceneTree;
use phantom_ui::arbiter::LayoutArbiter;
use phantom_ui::layout::{LayoutEngine, PaneId};

/// A set of position allocations produced by the layout arbiter or Taffy.
pub struct LayoutPlan {
    pub allocations: HashMap<AppId, Rect>,
}

/// Render outputs partitioned by scene layer.
#[allow(dead_code)]
pub struct LayeredRenderOutputs {
    pub scene: Vec<(AppId, Rect, RenderOutput)>,
    pub overlay: Vec<(AppId, Rect, RenderOutput)>,
}

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
    arbiter: LayoutArbiter,
    render_cache: HashMap<AppId, RenderOutput>,
    dirty_adapters: HashSet<AppId>,
    /// Adapters detached from the tiled grid into floating mode.
    floating: HashSet<AppId>,
    /// Pixel-space rects for floating panes.
    float_rects: HashMap<AppId, Rect>,
}

impl AppCoordinator {
    /// Create a new coordinator with the given event bus.
    ///
    /// The arbiter is initialized with zero size; call
    /// `set_arbiter_size()` once the window content area and cell size
    /// are known (typically right after window creation).
    pub fn new(bus: EventBus) -> Self {
        Self {
            registry: AppRegistry::new(),
            bus,
            pane_map: HashMap::new(),
            app_pane_map: HashMap::new(),
            scene_map: HashMap::new(),
            cadences: HashMap::new(),
            focused: None,
            arbiter: LayoutArbiter::new((0.0, 0.0), (1.0, 1.0)),
            render_cache: HashMap::new(),
            dirty_adapters: HashSet::new(),
            floating: HashSet::new(),
            float_rects: HashMap::new(),
        }
    }

    /// Configure the arbiter with the actual window content area and cell
    /// size. Should be called once during app initialization and again if
    /// `cell_size` changes (e.g. font size change).
    pub fn set_arbiter_size(&mut self, available: (f32, f32), cell_size: (f32, f32)) {
        self.arbiter = LayoutArbiter::new(available, cell_size);
    }

    /// Handle a window resize: update the arbiter's available area and
    /// re-negotiate spatial allocations.
    pub fn on_window_resize(&mut self, available: (f32, f32)) {
        self.arbiter.set_available(available);
        self.run_arbiter_negotiation();
    }

    /// Collect spatial preferences from all running adapters, run the
    /// arbiter, and log the resulting plan.
    ///
    /// TODO: Apply the plan's allocations as Taffy min/max constraints
    /// on the corresponding pane nodes. For now we just log.
    fn run_arbiter_negotiation(&self) {
        let default_pref = SpatialPreference::simple(40, 10);

        let participants: Vec<(AppId, SpatialPreference)> = self
            .registry
            .all_running()
            .into_iter()
            .map(|id| {
                let pref = self
                    .registry
                    .get_adapter(id)
                    .and_then(|a| a.spatial_preference())
                    .unwrap_or_else(|| default_pref.clone());
                (id, pref)
            })
            .collect();

        let plan = self.arbiter.negotiate(&participants);

        log::debug!(
            "Arbiter negotiation: {} participants -> {} allocations",
            participants.len(),
            plan.allocations.len(),
        );
        for (id, rect) in &plan.allocations {
            log::trace!(
                "  AppId {id}: ({:.0}, {:.0}) {:.0}x{:.0}",
                rect.x,
                rect.y,
                rect.width,
                rect.height,
            );
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

        // Inform the adapter of its assigned ID and invoke lifecycle init.
        if let Some(adapter) = self.registry.get_adapter_mut(id) {
            adapter.set_app_id(id);
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
        if let Some(node) = scene.get_mut(node_id) {
            let app_type = self.registry.get(id).map(|e| e.app_type.as_str()).unwrap_or("unknown");
            node.z_order = match app_type { "video" => 10, _ => 0 };
            node.render_layer = RenderLayer::Scene;
        }
        self.scene_map.insert(id, node_id);

        self.cadences.insert(id, cadence);

        if is_first {
            self.focused = Some(id);
        }

        // Re-negotiate spatial allocations with the new participant.
        self.run_arbiter_negotiation();
        self.dirty_adapters.insert(id);

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
            adapter.set_app_id(id);
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

        // Re-negotiate spatial allocations with the new participant.
        self.run_arbiter_negotiation();
        self.dirty_adapters.insert(id);

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

        // Re-negotiate so remaining adapters can claim freed space.
        self.run_arbiter_negotiation();
        self.render_cache.remove(&app_id);
        self.dirty_adapters.remove(&app_id);
        self.floating.remove(&app_id);
        self.float_rects.remove(&app_id);
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

        // Collect outbound messages from adapters and emit them on the bus.
        // Adapters emit with topic_id=0; we resolve the correct ID from the
        // event's topic category.
        for id in &running {
            let Some(adapter) = self.registry.get_adapter_mut(*id) else {
                continue;
            };
            let outbox = adapter.drain_outbox();
            for mut msg in outbox {
                if msg.topic_id == 0 {
                    // Resolve topic ID from the event's topic name.
                    let topic_name = match msg.event.topic() {
                        phantom_protocol::events::EventTopic::Terminal => "terminal.output",
                        phantom_protocol::events::EventTopic::Agents => "agent.event",
                        phantom_protocol::events::EventTopic::Video => "video",
                        phantom_protocol::events::EventTopic::System => "system",
                        _ => "custom",
                    };
                    if let Some(tid) = self.bus.topic_id_by_name(topic_name) {
                        msg.topic_id = tid;
                    } else {
                        log::warn!("Outbox event dropped: no topic '{topic_name}' registered");
                        continue;
                    }
                }
                self.bus.emit(msg);
            }
        }

        // Deliver bus messages to all running subscribers (not gated by cadence —
        // messages should never be silently delayed).
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
    ///
    /// The rect handed to each adapter is the **inner content rect** —
    /// chrome insets (container margin + title strip + bottom padding)
    /// are subtracted **before** the rect ever reaches the adapter, when
    /// chrome will be drawn (i.e. when there are 2+ visual adapters).
    /// This makes overlap with the chrome's title strip mathematically
    /// impossible: the adapter never sees the outer rect at all.
    ///
    /// `cell_size` is required because chrome insets are expressed in
    /// multiples of cell metrics (see [`crate::pane::pane_inner_rect`]).
    pub fn render_all(
        &self,
        layout: &LayoutEngine,
        cell_size: (f32, f32),
    ) -> Vec<(AppId, Rect, RenderOutput)> {
        // Count visual adapters first; chrome is suppressed when there is
        // only one tiled pane (see render.rs `tiled_count <= 1`). When
        // chrome is suppressed, the adapter should occupy the full layout
        // rect — there is no title strip to avoid.
        let visual_count = self
            .registry
            .all_running()
            .into_iter()
            .filter_map(|id| self.registry.get(id))
            .filter(|e| e.visual)
            .count();
        let chrome_active = visual_count > 1;

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
            let layout_rect = match layout.get_pane_rect(*pane_id) {
                Ok(r) => r,
                Err(_) => continue,
            };

            // Apply the same chrome math the renderer uses: outer margin
            // (`container_rect`) then chrome insets (`pane_inner_rect`).
            // When chrome is suppressed the adapter sees the raw layout
            // rect so single-pane mode does not lose ~1.5 cells of height.
            let adapter_rect_ui = if chrome_active {
                let outer = crate::pane::container_rect(layout_rect, cell_size);
                crate::pane::pane_inner_rect(cell_size, outer)
            } else {
                layout_rect
            };

            let rect = Rect {
                x: adapter_rect_ui.x,
                y: adapter_rect_ui.y,
                width: adapter_rect_ui.width,
                height: adapter_rect_ui.height,
                cell_size,
            };

            let Some(adapter) = self.registry.get_adapter(id) else {
                continue;
            };
            let output = adapter.render(&rect);
            outputs.push((id, rect, output));
        }

        outputs
    }

    /// Sync positions into the scene graph from a layout plan.
    pub fn sync_arbiter_to_scene(&self, plan: &LayoutPlan, scene: &mut SceneTree) {
        for (&app_id, rect) in &plan.allocations {
            if let Some(&node_id) = self.scene_map.get(&app_id) {
                if let Some(node) = scene.get_mut(node_id) {
                    node.transform.x = rect.x;
                    node.transform.y = rect.y;
                    node.transform.width = rect.width;
                    node.transform.height = rect.height;
                    node.dirty |= DirtyFlags::TRANSFORM;
                    log::debug!(
                        "Scene sync: adapter {app_id} -> node {node_id} at ({:.0}, {:.0}, {:.0}x{:.0})",
                        rect.x, rect.y, rect.width, rect.height,
                    );
                }
            }
        }
    }

    /// Build a `LayoutPlan` from the current Taffy layout.
    pub fn build_layout_plan(&self, layout: &LayoutEngine) -> LayoutPlan {
        let mut allocations = HashMap::new();
        for id in self.registry.all_running() {
            if let Some(pane_id) = self.app_pane_map.get(&id) {
                if let Ok(r) = layout.get_pane_rect(*pane_id) {
                    allocations.insert(id, Rect { x: r.x, y: r.y, width: r.width, height: r.height, ..Default::default() });
                }
            }
        }
        LayoutPlan { allocations }
    }

    /// Scene-graph-aware render: reads positions from world_transform,
    /// sorted by z_order (lowest first = drawn first = behind).
    /// Focused adapter gets +1 z_order bonus.
    pub fn render_all_with_scene(&self, scene: &SceneTree) -> Vec<(AppId, Rect, RenderOutput)> {
        let mut outputs = Vec::new();
        for id in self.registry.all_running() {
            let Some(entry) = self.registry.get(id) else { continue };
            if !entry.visual { continue; }
            let Some(&node_id) = self.scene_map.get(&id) else { continue };
            let Some(node) = scene.get(node_id) else { continue };
            let wt = node.world_transform;
            let rect = Rect { x: wt.x, y: wt.y, width: wt.width, height: wt.height, ..Default::default() };
            let Some(adapter) = self.registry.get_adapter(id) else { continue };
            let output = adapter.render(&rect);
            outputs.push((id, rect, output));
        }

        // Sort by z_order; focused adapter gets +1 bonus.
        outputs.sort_by_key(|(app_id, _, _)| {
            let base = self.scene_map.get(app_id)
                .and_then(|&nid| scene.get(nid))
                .map(|n| n.z_order)
                .unwrap_or(0);
            if self.focused == Some(*app_id) { base + 1 } else { base }
        });

        outputs
    }

    /// Query which render layer an adapter's scene node belongs to.
    pub fn render_layer_for(&self, app_id: AppId, scene: &SceneTree) -> RenderLayer {
        self.scene_map.get(&app_id)
            .and_then(|&nid| scene.get(nid))
            .map(|n| n.render_layer)
            .unwrap_or(RenderLayer::Scene)
    }

    /// Collect render outputs partitioned by RenderLayer.
    #[allow(dead_code)]
    pub fn render_all_layered(
        &self,
        layout: &LayoutEngine,
        scene: &SceneTree,
        cell_size: (f32, f32),
    ) -> LayeredRenderOutputs {
        let all = self.render_all(layout, cell_size);
        let mut result = LayeredRenderOutputs { scene: Vec::new(), overlay: Vec::new() };
        for item in all {
            let (app_id, _, _) = &item;
            match self.render_layer_for(*app_id, scene) {
                RenderLayer::Scene => result.scene.push(item),
                RenderLayer::Overlay => result.overlay.push(item),
            }
        }
        result
    }

    /// Detach a pane from the tiled grid into floating mode.
    pub fn detach_to_float(&mut self, app_id: AppId, layout: &mut LayoutEngine, scene: &mut SceneTree) {
        let rect = self.app_pane_map.get(&app_id)
            .and_then(|pid| layout.get_pane_rect(*pid).ok())
            .map(|r| Rect { x: r.x, y: r.y, width: r.width, height: r.height, ..Default::default() })
            .unwrap_or(Rect { x: 100.0, y: 100.0, width: 600.0, height: 400.0, ..Default::default() });

        if let Some(pane_id) = self.app_pane_map.remove(&app_id) {
            self.pane_map.remove(&pane_id);
            let _ = layout.remove_pane(pane_id);
        }

        self.floating.insert(app_id);
        self.float_rects.insert(app_id, rect.clone());

        if let Some(&node_id) = self.scene_map.get(&app_id) {
            if let Some(node) = scene.get_mut(node_id) {
                node.z_order = 50;
                node.render_layer = RenderLayer::Overlay;
                node.transform.x = rect.x;
                node.transform.y = rect.y;
                node.transform.width = rect.width;
                node.transform.height = rect.height;
                node.dirty |= DirtyFlags::TRANSFORM;
            }
        }

        self.run_arbiter_negotiation();
        log::info!("Detached adapter {app_id} to floating at ({:.0},{:.0} {:.0}x{:.0})", rect.x, rect.y, rect.width, rect.height);
    }

    /// Get the scene node ID for an adapter, if it exists.
    pub fn scene_node_for(&self, app_id: AppId) -> Option<phantom_scene::node::NodeId> {
        self.scene_map.get(&app_id).copied()
    }

    pub fn is_floating(&self, app_id: AppId) -> bool { self.floating.contains(&app_id) }
    pub fn float_rect(&self, app_id: AppId) -> Option<&Rect> { self.float_rects.get(&app_id) }
    pub fn floating_ids(&self) -> impl Iterator<Item = AppId> + '_ { self.floating.iter().copied() }

    pub fn move_floating(&mut self, app_id: AppId, x: f32, y: f32) {
        if let Some(rect) = self.float_rects.get_mut(&app_id) {
            rect.x = x;
            rect.y = y;
        }
    }

    pub fn resize_floating(&mut self, app_id: AppId, width: f32, height: f32) {
        if let Some(rect) = self.float_rects.get_mut(&app_id) {
            rect.width = width.max(100.0);
            rect.height = height.max(80.0);
        }
    }

    /// Dock a floating pane back into the tiled grid.
    pub fn dock_to_grid(&mut self, app_id: AppId, layout: &mut LayoutEngine, scene: &mut SceneTree) {
        if !self.floating.remove(&app_id) { return; }
        self.float_rects.remove(&app_id);

        match layout.add_pane() {
            Ok(pane_id) => {
                self.pane_map.insert(pane_id, app_id);
                self.app_pane_map.insert(app_id, pane_id);
            }
            Err(e) => {
                log::warn!("Failed to dock adapter {app_id}: {e}");
                return;
            }
        }

        if let Some(&node_id) = self.scene_map.get(&app_id) {
            if let Some(node) = scene.get_mut(node_id) {
                node.z_order = 0;
                node.render_layer = RenderLayer::Scene;
                node.dirty |= DirtyFlags::TRANSFORM;
            }
        }

        self.run_arbiter_negotiation();
        log::info!("Docked adapter {app_id} back to grid");
    }

    /// Switch an adapter's scene node to a different render layer.
    #[allow(dead_code)]
    pub fn set_render_layer(&self, app_id: AppId, layer: RenderLayer, scene: &mut SceneTree) {
        if let Some(&node_id) = self.scene_map.get(&app_id) {
            if let Some(node) = scene.get_mut(node_id) {
                node.render_layer = layer;
                node.dirty |= DirtyFlags::VISIBILITY;
            }
        }
    }

    /// Mark all adapter scene nodes as dirty (call on window resize).
    pub fn mark_all_dirty(&mut self, scene: &mut SceneTree) {
        for (&app_id, &node_id) in &self.scene_map {
            if let Some(node) = scene.get_mut(node_id) {
                node.dirty |= DirtyFlags::TRANSFORM;
            }
            self.dirty_adapters.insert(app_id);
        }
    }

    /// Clear dirty flags on all adapter scene nodes (call after render).
    pub fn clear_render_dirty(&mut self, scene: &mut SceneTree) {
        for &node_id in self.scene_map.values() {
            if let Some(node) = scene.get_mut(node_id) {
                node.dirty = DirtyFlags::empty();
            }
        }
        self.dirty_adapters.clear();
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

    /// Write raw bytes to the focused adapter via `accept_command("write_bytes", ...)`.
    ///
    /// Returns `true` if the bytes were consumed. Used for keyboard input
    /// where bytes are pre-encoded by `encode_key()`.
    pub fn route_bytes(&mut self, bytes: &[u8]) -> bool {
        let Some(id) = self.focused else {
            return false;
        };
        self.route_bytes_to(id, bytes)
    }

    /// Write raw bytes to a specific adapter via `accept_command("write_bytes", ...)`.
    ///
    /// Returns `true` if the bytes were consumed. Used for SGR mouse
    /// forwarding where the target may differ from the focused adapter.
    pub fn route_bytes_to(&mut self, app_id: AppId, bytes: &[u8]) -> bool {
        let Some(entry) = self.registry.get(app_id) else {
            return false;
        };
        if !entry.accepts_input {
            return false;
        }
        let Some(adapter) = self.registry.get_adapter_mut(app_id) else {
            return false;
        };
        // Encode bytes as a JSON array for the command interface.
        let args = serde_json::json!({ "bytes": bytes });
        match adapter.accept_command("write_bytes", &args) {
            Ok(_) => true,
            Err(e) => {
                log::warn!("route_bytes to adapter {app_id} failed: {e}");
                false
            }
        }
    }

    /// Whether the adapter at `app_id` has requested mouse event forwarding.
    ///
    /// Checks the adapter's state JSON for a `mouse_mode` field that is
    /// not `"none"`. Terminal adapters report this based on DEC mode
    /// tracking (modes 1000/1002/1003).
    pub fn adapter_wants_mouse(&self, app_id: AppId) -> bool {
        self.registry
            .get_adapter(app_id)
            .map(|a| a.get_state())
            .and_then(|s| s.get("mouse_mode").and_then(|v| v.as_str().map(String::from)))
            .is_some_and(|m| m != "none")
    }

    /// Send a command to the currently focused adapter.
    ///
    /// Returns `Ok` with the response, or `Err` if no adapter is focused
    /// or the command fails.
    pub fn send_command_to_focused(
        &mut self,
        cmd: &str,
        args: &serde_json::Value,
    ) -> anyhow::Result<String> {
        let id = self.focused.ok_or_else(|| anyhow::anyhow!("no focused adapter"))?;
        self.send_command(id, cmd, args)
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

    /// Remap an adapter's PaneId (e.g. after a layout split replaces the old PaneId).
    pub fn remap_pane(&mut self, app_id: AppId, old_pane_id: PaneId, new_pane_id: PaneId) {
        self.pane_map.remove(&old_pane_id);
        self.pane_map.insert(new_pane_id, app_id);
        self.app_pane_map.insert(app_id, new_pane_id);
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
                scroll: None,
                selection: None,
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

        let outputs = coord.render_all(&layout, (8.0, 16.0));

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
        let outputs = coord.render_all(&layout, (8.0, 16.0));

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
        let outputs = coord.render_all(&layout, (8.0, 16.0));
        assert_eq!(outputs.len(), 1);

        // Remove.
        coord.remove_adapter(id, &mut layout, &mut scene);
        assert_eq!(coord.registry().get(id).unwrap().state, AppState::Dead);
        assert_eq!(coord.focused(), None);
    }

    // ── Chrome-overlap regression: pane_inner_rect transformation ────────
    //
    // The chrome's title strip occupies (pane_rect.x, pane_rect.y,
    // pane_rect.width, title_h). Adapters must never see the outer rect;
    // the coordinator transforms it via `pane_inner_rect` before calling
    // `render()`, making overlap mathematically impossible.

    /// When chrome is active (2+ visual adapters), `render_all` must hand
    /// each adapter the inner rect — the outer layout rect minus container
    /// margins, title strip, and bottom padding.
    #[test]
    fn coordinator_passes_inner_rect_to_adapter() {
        use crate::pane::{container_rect, pane_inner_rect};

        let cell_size = (8.0_f32, 16.0_f32);
        let mut coord = AppCoordinator::new_test(EventBus::new());
        let mut layout = LayoutEngine::new().unwrap();
        let mut scene = SceneTree::new();
        let content = scene.add_node(scene.root(), NodeKind::ContentArea);

        // Two visual adapters → chrome is active.
        let id_a = register_mock(&mut coord, &mut layout, &mut scene, content, "a");
        let _id_b = register_mock(&mut coord, &mut layout, &mut scene, content, "b");

        layout.resize(800.0, 600.0).unwrap();
        let outputs = coord.render_all(&layout, cell_size);

        // MockAdapter::render emits a quad at (rect.x, rect.y, rect.w, rect.h).
        // We use that quad to inspect what rect the adapter received.
        let (_, rect_a, ro_a) = outputs.iter().find(|(id, _, _)| *id == id_a).unwrap();
        let q = ro_a.quads.first().expect("mock emits a rect-sized quad");

        // The rect passed to the adapter must equal the inner rect derived
        // from the layout rect via the same chrome math used in render.rs.
        let pane_id = coord.pane_id_for(id_a).unwrap();
        let layout_rect = layout.get_pane_rect(pane_id).unwrap();
        let expected = pane_inner_rect(cell_size, container_rect(layout_rect, cell_size));

        assert!((rect_a.x - expected.x).abs() < 0.01, "x: got {} expected {}", rect_a.x, expected.x);
        assert!((rect_a.y - expected.y).abs() < 0.01, "y: got {} expected {}", rect_a.y, expected.y);
        assert!((rect_a.width - expected.width).abs() < 0.01, "w: got {} expected {}", rect_a.width, expected.width);
        assert!((rect_a.height - expected.height).abs() < 0.01, "h: got {} expected {}", rect_a.height, expected.height);

        // And the quad the adapter emitted is at that same rect (sanity).
        assert!((q.x - expected.x).abs() < 0.01);
        assert!((q.y - expected.y).abs() < 0.01);
    }

    /// The adapter's first text segment must start at a Y >= the title
    /// strip's bottom edge. The title strip occupies the top of the
    /// **container** rect (post-margin), so the bound is
    /// `container_rect.y + title_h`.
    #[test]
    fn adapter_render_y_starts_below_title_strip() {
        use crate::pane::{
            CONTAINER_TITLE_H_CELLS, container_rect,
        };

        let cell_size = (8.0_f32, 16.0_f32);
        let mut coord = AppCoordinator::new_test(EventBus::new());
        let mut layout = LayoutEngine::new().unwrap();
        let mut scene = SceneTree::new();
        let content = scene.add_node(scene.root(), NodeKind::ContentArea);

        let id_a = register_mock(&mut coord, &mut layout, &mut scene, content, "a");
        let _id_b = register_mock(&mut coord, &mut layout, &mut scene, content, "b");

        layout.resize(800.0, 600.0).unwrap();
        let outputs = coord.render_all(&layout, cell_size);

        let (_, _rect_a, ro_a) = outputs.iter().find(|(id, _, _)| *id == id_a).unwrap();
        let first_text = ro_a
            .text_segments
            .first()
            .expect("mock emits at least one text segment");

        let pane_id = coord.pane_id_for(id_a).unwrap();
        let layout_rect = layout.get_pane_rect(pane_id).unwrap();
        let outer = container_rect(layout_rect, cell_size);
        let title_strip_bottom = outer.y + cell_size.1 * CONTAINER_TITLE_H_CELLS;

        assert!(
            first_text.y >= title_strip_bottom - 0.01,
            "adapter text Y {} must be at or below title strip bottom {}",
            first_text.y,
            title_strip_bottom,
        );
    }

    /// The adapter's last possible text Y + line_height must not exceed
    /// the body's bottom (container_rect minus title strip and bottom pad).
    /// Combined with the previous test, this proves the adapter cannot
    /// paint outside the chrome's body region.
    #[test]
    fn adapter_render_height_excludes_title_strip() {
        use crate::pane::{
            CONTAINER_PAD_B_CELLS, CONTAINER_TITLE_H_CELLS, container_rect,
        };

        let cell_size = (8.0_f32, 16.0_f32);
        let mut coord = AppCoordinator::new_test(EventBus::new());
        let mut layout = LayoutEngine::new().unwrap();
        let mut scene = SceneTree::new();
        let content = scene.add_node(scene.root(), NodeKind::ContentArea);

        let id_a = register_mock(&mut coord, &mut layout, &mut scene, content, "a");
        let _id_b = register_mock(&mut coord, &mut layout, &mut scene, content, "b");

        layout.resize(800.0, 600.0).unwrap();
        let outputs = coord.render_all(&layout, cell_size);

        let (_, rect_a, _ro_a) = outputs.iter().find(|(id, _, _)| *id == id_a).unwrap();

        let pane_id = coord.pane_id_for(id_a).unwrap();
        let layout_rect = layout.get_pane_rect(pane_id).unwrap();
        let outer = container_rect(layout_rect, cell_size);
        let title_h = cell_size.1 * CONTAINER_TITLE_H_CELLS;
        let pad_b = cell_size.1 * CONTAINER_PAD_B_CELLS;
        let body_bottom = outer.y + outer.height - pad_b;

        // The adapter's rect bottom must fit within the chrome body.
        let adapter_bottom = rect_a.y + rect_a.height;
        assert!(
            adapter_bottom <= body_bottom + 0.01,
            "adapter bottom {} must not exceed body bottom {}",
            adapter_bottom,
            body_bottom,
        );

        // And the rect must start at the title strip's bottom.
        let title_bottom = outer.y + title_h;
        assert!(
            rect_a.y >= title_bottom - 0.01,
            "adapter rect.y {} must start at or below title strip bottom {}",
            rect_a.y,
            title_bottom,
        );
    }

    /// Single-pane mode skips chrome, so the adapter receives the raw
    /// layout rect (no insets). This guards against an over-eager fix
    /// that would always inset and waste height in single-pane mode.
    #[test]
    fn single_pane_skips_chrome_insets() {
        let cell_size = (8.0_f32, 16.0_f32);
        let mut coord = AppCoordinator::new_test(EventBus::new());
        let mut layout = LayoutEngine::new().unwrap();
        let mut scene = SceneTree::new();
        let content = scene.add_node(scene.root(), NodeKind::ContentArea);

        let id_a = register_mock(&mut coord, &mut layout, &mut scene, content, "solo");

        layout.resize(800.0, 600.0).unwrap();
        let outputs = coord.render_all(&layout, cell_size);

        let (_, rect_a, _) = outputs.iter().find(|(id, _, _)| *id == id_a).unwrap();
        let pane_id = coord.pane_id_for(id_a).unwrap();
        let layout_rect = layout.get_pane_rect(pane_id).unwrap();

        // Single pane → adapter sees the raw layout rect.
        assert!((rect_a.x - layout_rect.x).abs() < 0.01);
        assert!((rect_a.y - layout_rect.y).abs() < 0.01);
        assert!((rect_a.width - layout_rect.width).abs() < 0.01);
        assert!((rect_a.height - layout_rect.height).abs() < 0.01);
    }

    // ── Layout memory-leak regression (Issue #15) ──────────────────────────
    //
    // Every `register_adapter` call allocates a new Taffy node; the matching
    // `remove_adapter` call must free it.  If the two are not perfectly
    // balanced the Taffy tree grows without bound, causing heap pressure
    // proportional to the number of spawn-close cycles.
    //
    // This test drives 1 000 spawn-close cycles and asserts that the total
    // Taffy node count returns to the pre-cycle baseline after each removal,
    // proving that no orphaned nodes accumulate.

    /// 1 000 spawn-close cycles must not grow the Taffy tree.
    ///
    /// Arrange: record the node count after the layout engine is created
    ///          (chrome nodes only: root, tab_bar, content, status_bar = 4).
    /// Act:     register an adapter (creates 1 pane node) then immediately
    ///          remove it (must free that pane node) — repeat 1 000 times.
    /// Assert:  after every removal the total node count equals the baseline.
    #[test]
    fn taffy_node_count_stable_across_spawn_close_cycles() {
        let mut coord = AppCoordinator::new_test(EventBus::new());
        let mut layout = LayoutEngine::new().unwrap();
        let mut scene = SceneTree::new();
        let content = scene.add_node(scene.root(), NodeKind::ContentArea);

        // Baseline: chrome nodes only (root + tab_bar + content + status_bar).
        let baseline = layout.total_node_count();

        for cycle in 0..1_000 {
            // Register creates one new Taffy node.
            let id = coord.register_adapter(
                Box::new(MockAdapter::new("cycle")),
                &mut layout,
                &mut scene,
                content,
                Cadence::unlimited(),
            );

            // Remove must free that node — tree must shrink back to baseline.
            coord.remove_adapter(id, &mut layout, &mut scene);

            let after = layout.total_node_count();
            assert_eq!(
                after,
                baseline,
                "cycle {cycle}: Taffy node count leaked — expected {baseline}, got {after}",
            );
        }
    }
}
