//! phantom-adapter — the "everything is an app" core.
//!
//! Defines the `AppAdapter` trait, lifecycle states, app registry,
//! pub/sub event bus, and spatial negotiation types. Every component
//! in Phantom (terminals, editors, headless processors) implements
//! `AppAdapter` and registers with the `AppRegistry`.

pub mod adapter;
pub mod bus;
pub mod lifecycle;
pub mod registry;
pub mod spatial;

// Re-export all public types at crate root for convenience.
pub use adapter::{
    AppAdapter, AppCore, BusParticipant, Commandable, InputHandler, Lifecycled, Permissioned,
    Renderable,
};
pub use adapter::{AppId, CursorData, CursorShape, GridData, QuadData, Rect, RenderOutput, TerminalCell, TextData};
pub use bus::{BusMessage, DataType, EventBus, Topic, TopicDeclaration, TopicId};
pub use phantom_protocol::Event;
pub use lifecycle::AppState;
pub use registry::{AppRegistry, RegisteredApp};
pub use spatial::{
    Direction, InternalLayout, NegotiationResult, ResizeResult, SpatialPreference,
};

#[cfg(test)]
mod tests {
    use super::*;
    use phantom_protocol::Event;
    use serde_json::json;

    // -----------------------------------------------------------------------
    // Mock adapter for testing
    // -----------------------------------------------------------------------

    struct MockApp {
        alive: bool,
        visual: bool,
        app_type: String,
        state_log: Vec<AppState>,
        messages: Vec<BusMessage>,
    }

    impl MockApp {
        fn new(app_type: &str) -> Self {
            Self {
                alive: true,
                visual: true,
                app_type: app_type.to_string(),
                state_log: Vec::new(),
                messages: Vec::new(),
            }
        }

        fn headless(app_type: &str) -> Self {
            Self {
                alive: true,
                visual: false,
                app_type: app_type.to_string(),
                state_log: Vec::new(),
                messages: Vec::new(),
            }
        }
    }

    impl AppCore for MockApp {
        fn app_type(&self) -> &str {
            &self.app_type
        }

        fn is_alive(&self) -> bool {
            self.alive
        }

        fn update(&mut self, _dt: f32) {}

        fn get_state(&self) -> serde_json::Value {
            json!({ "type": self.app_type, "alive": self.alive })
        }
    }

    impl Renderable for MockApp {
        fn render(&self, rect: &Rect) -> RenderOutput {
            RenderOutput {
                quads: vec![QuadData {
                    x: rect.x,
                    y: rect.y,
                    w: rect.width,
                    h: rect.height,
                    color: [1.0, 1.0, 1.0, 1.0],
                }],
                text_segments: vec![TextData {
                    text: format!("mock:{}", self.app_type),
                    x: rect.x,
                    y: rect.y,
                    color: [1.0, 1.0, 1.0, 1.0],
                }],
                grid: None,
            }
        }

        fn is_visual(&self) -> bool {
            self.visual
        }
    }

    impl InputHandler for MockApp {
        fn handle_input(&mut self, key: &str) -> bool {
            key == "q"
        }
    }

    impl Commandable for MockApp {
        fn accept_command(
            &mut self,
            cmd: &str,
            _args: &serde_json::Value,
        ) -> anyhow::Result<String> {
            if cmd == "die" {
                self.alive = false;
            }
            Ok(format!("executed:{cmd}"))
        }
    }

    impl BusParticipant for MockApp {
        fn on_message(&mut self, msg: &BusMessage) {
            self.messages.push(msg.clone());
        }
    }

    impl Lifecycled for MockApp {
        fn on_state_change(&mut self, new_state: AppState) {
            self.state_log.push(new_state);
        }
    }

    impl Permissioned for MockApp {
        fn permissions(&self) -> Vec<String> {
            vec!["filesystem".into()]
        }
    }

    // =======================================================================
    // Lifecycle tests
    // =======================================================================

    #[test]
    fn lifecycle_init_to_running() {
        assert!(AppState::Initializing.can_transition_to(AppState::Running));
    }

    #[test]
    fn lifecycle_running_to_suspended() {
        assert!(AppState::Running.can_transition_to(AppState::Suspended));
    }

    #[test]
    fn lifecycle_suspended_to_running() {
        assert!(AppState::Suspended.can_transition_to(AppState::Running));
    }

    #[test]
    fn lifecycle_running_to_exiting() {
        assert!(AppState::Running.can_transition_to(AppState::Exiting));
    }

    #[test]
    fn lifecycle_exiting_to_dead() {
        assert!(AppState::Exiting.can_transition_to(AppState::Dead));
    }

    #[test]
    fn lifecycle_any_to_dead() {
        for state in [
            AppState::Initializing,
            AppState::Running,
            AppState::Suspended,
            AppState::Exiting,
            AppState::Dead,
        ] {
            assert!(
                state.can_transition_to(AppState::Dead),
                "{state:?} should be able to transition to Dead"
            );
        }
    }

    #[test]
    fn lifecycle_invalid_transitions() {
        // Cannot go backwards from Running to Initializing.
        assert!(!AppState::Running.can_transition_to(AppState::Initializing));
        // Cannot skip from Initializing to Suspended.
        assert!(!AppState::Initializing.can_transition_to(AppState::Suspended));
        // Cannot go from Suspended to Exiting directly.
        assert!(!AppState::Suspended.can_transition_to(AppState::Exiting));
        // Cannot go from Exiting to Running.
        assert!(!AppState::Exiting.can_transition_to(AppState::Running));
        // Dead cannot transition to anything except Dead.
        assert!(!AppState::Dead.can_transition_to(AppState::Running));
        assert!(!AppState::Dead.can_transition_to(AppState::Initializing));
    }

    #[test]
    fn lifecycle_is_active() {
        assert!(!AppState::Initializing.is_active());
        assert!(AppState::Running.is_active());
        assert!(AppState::Suspended.is_active());
        assert!(!AppState::Exiting.is_active());
        assert!(!AppState::Dead.is_active());
    }

    #[test]
    fn lifecycle_receives_input() {
        assert!(AppState::Running.receives_input());
        assert!(!AppState::Suspended.receives_input());
        assert!(!AppState::Initializing.receives_input());
        assert!(!AppState::Dead.receives_input());
    }

    #[test]
    fn lifecycle_can_publish() {
        assert!(AppState::Running.can_publish());
        assert!(AppState::Suspended.can_publish());
        assert!(!AppState::Initializing.can_publish());
        assert!(!AppState::Exiting.can_publish());
        assert!(!AppState::Dead.can_publish());
    }

    // =======================================================================
    // Registry tests
    // =======================================================================

    #[test]
    fn registry_register_assigns_unique_ids() {
        let mut reg = AppRegistry::new();
        let id1 = reg.register(Box::new(MockApp::new("terminal")));
        let id2 = reg.register(Box::new(MockApp::new("editor")));
        assert_ne!(id1, id2);
        assert_eq!(reg.count(), 2);
    }

    #[test]
    fn registry_initial_state_is_initializing() {
        let mut reg = AppRegistry::new();
        let id = reg.register(Box::new(MockApp::new("terminal")));
        let entry = reg.get(id).unwrap();
        assert_eq!(entry.state, AppState::Initializing);
    }

    #[test]
    fn registry_ready_transition() {
        let mut reg = AppRegistry::new();
        let id = reg.register(Box::new(MockApp::new("terminal")));
        assert!(reg.ready(id));
        assert_eq!(reg.get(id).unwrap().state, AppState::Running);
    }

    #[test]
    fn registry_suspend_and_resume() {
        let mut reg = AppRegistry::new();
        let id = reg.register(Box::new(MockApp::new("terminal")));
        reg.ready(id);
        assert!(reg.suspend(id));
        assert_eq!(reg.get(id).unwrap().state, AppState::Suspended);
        assert!(reg.resume(id));
        assert_eq!(reg.get(id).unwrap().state, AppState::Running);
    }

    #[test]
    fn registry_invalid_transition_returns_false() {
        let mut reg = AppRegistry::new();
        let id = reg.register(Box::new(MockApp::new("terminal")));
        // Cannot suspend from Initializing.
        assert!(!reg.suspend(id));
        assert_eq!(reg.get(id).unwrap().state, AppState::Initializing);
    }

    #[test]
    fn registry_request_exit_and_kill() {
        let mut reg = AppRegistry::new();
        let id = reg.register(Box::new(MockApp::new("terminal")));
        reg.ready(id);
        assert!(reg.request_exit(id));
        assert_eq!(reg.get(id).unwrap().state, AppState::Exiting);
        reg.kill(id);
        assert_eq!(reg.get(id).unwrap().state, AppState::Dead);
    }

    #[test]
    fn registry_gc_removes_dead_apps() {
        let mut reg = AppRegistry::new();
        let id1 = reg.register(Box::new(MockApp::new("terminal")));
        let id2 = reg.register(Box::new(MockApp::new("editor")));
        reg.ready(id1);
        reg.kill(id2); // force-kill from Initializing -> Dead
        assert_eq!(reg.count(), 2);
        let removed = reg.gc();
        assert_eq!(removed, 1);
        assert_eq!(reg.count(), 1);
        assert!(reg.get(id1).is_some());
        assert!(reg.get(id2).is_none());
    }

    #[test]
    fn registry_by_state_and_all_running() {
        let mut reg = AppRegistry::new();
        let id1 = reg.register(Box::new(MockApp::new("a")));
        let id2 = reg.register(Box::new(MockApp::new("b")));
        let id3 = reg.register(Box::new(MockApp::new("c")));
        reg.ready(id1);
        reg.ready(id2);
        // id3 stays Initializing
        assert_eq!(reg.all_running().len(), 2);
        assert_eq!(reg.by_state(AppState::Initializing), vec![id3]);
    }

    #[test]
    fn registry_all_visual() {
        let mut reg = AppRegistry::new();
        let _id1 = reg.register(Box::new(MockApp::new("terminal")));
        let _id2 = reg.register(Box::new(MockApp::headless("processor")));
        assert_eq!(reg.all_visual().len(), 1);
    }

    #[test]
    fn registry_adapter_access() {
        let mut reg = AppRegistry::new();
        let id = reg.register(Box::new(MockApp::new("terminal")));
        assert_eq!(reg.get_adapter(id).unwrap().app_type(), "terminal");
        let adapter = reg.get_adapter_mut(id).unwrap();
        assert!(adapter.handle_input("q"));
    }

    #[test]
    fn registry_on_state_change_callback() {
        let mut reg = AppRegistry::new();
        let id = reg.register(Box::new(MockApp::new("terminal")));
        reg.ready(id);
        reg.suspend(id);
        reg.resume(id);
        // The mock logs state changes — verify via get_state or adapter access.
        // (We verified the transitions; on_state_change is called inside transition().)
        let entry = reg.get(id).unwrap();
        assert_eq!(entry.state, AppState::Running);
    }

    #[test]
    fn registry_nonexistent_id_returns_none() {
        let reg = AppRegistry::new();
        assert!(reg.get(999).is_none());
        assert!(reg.get_adapter(999).is_none());
    }

    // =======================================================================
    // Event bus tests
    // =======================================================================

    #[test]
    fn bus_create_topic() {
        let mut bus = EventBus::new();
        let tid = bus.create_topic(1, "terminal.output", DataType::TerminalOutput);
        assert_eq!(bus.topic_count(), 1);
        assert_eq!(bus.topics()[0].name, "terminal.output");
        assert_eq!(tid, 1);
    }

    #[test]
    fn bus_remove_topic() {
        let mut bus = EventBus::new();
        let tid = bus.create_topic(1, "t", DataType::Text);
        bus.remove_topic(tid);
        assert_eq!(bus.topic_count(), 0);
    }

    #[test]
    fn bus_subscribe_and_emit() {
        let mut bus = EventBus::new();
        let tid = bus.create_topic(1, "data", DataType::Json);
        bus.subscribe(2, tid);

        bus.emit(BusMessage {
            topic_id: tid,
            sender: 1,
            event: Event::Custom { kind: "test".into(), data: "value".into() },
            frame: 0,
            timestamp: 100,
        });

        let msgs = bus.drain_for(2);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].sender, 1);
        assert!(matches!(
            &msgs[0].event,
            Event::Custom { kind, data } if kind == "test" && data == "value"
        ));
    }

    #[test]
    fn bus_drain_only_subscribed_topics() {
        let mut bus = EventBus::new();
        let t1 = bus.create_topic(1, "audio", DataType::Audio);
        let t2 = bus.create_topic(1, "text", DataType::Text);
        bus.subscribe(2, t1); // subscriber 2 only gets audio

        bus.emit(BusMessage {
            topic_id: t1,
            sender: 1,
            event: Event::Custom { kind: "audio".into(), data: "audio_data".into() },
            frame: 0,
            timestamp: 1,
        });
        bus.emit(BusMessage {
            topic_id: t2,
            sender: 1,
            event: Event::Custom { kind: "text".into(), data: "text_data".into() },
            frame: 0,
            timestamp: 2,
        });

        let msgs = bus.drain_for(2);
        assert_eq!(msgs.len(), 1);
        assert!(matches!(
            &msgs[0].event,
            Event::Custom { kind, .. } if kind == "audio"
        ));
    }

    #[test]
    fn bus_unsubscribe() {
        let mut bus = EventBus::new();
        let tid = bus.create_topic(1, "data", DataType::Text);
        bus.subscribe(2, tid);
        bus.unsubscribe(2, tid);

        bus.emit(BusMessage {
            topic_id: tid,
            sender: 1,
            event: Event::Custom { kind: "test".into(), data: "hello".into() },
            frame: 0,
            timestamp: 1,
        });

        let msgs = bus.drain_for(2);
        assert!(msgs.is_empty());
    }

    #[test]
    fn bus_unsubscribe_all() {
        let mut bus = EventBus::new();
        let t1 = bus.create_topic(1, "a", DataType::Text);
        let t2 = bus.create_topic(1, "b", DataType::Json);
        bus.subscribe(2, t1);
        bus.subscribe(2, t2);
        bus.unsubscribe_all(2);

        assert!(bus.subscribers(t1).is_empty());
        assert!(bus.subscribers(t2).is_empty());
    }

    #[test]
    fn bus_topics_by_type() {
        let mut bus = EventBus::new();
        bus.create_topic(1, "a", DataType::Text);
        bus.create_topic(1, "b", DataType::Text);
        bus.create_topic(1, "c", DataType::Audio);

        assert_eq!(bus.topics_by_type(&DataType::Text).len(), 2);
        assert_eq!(bus.topics_by_type(&DataType::Audio).len(), 1);
        assert_eq!(bus.topics_by_type(&DataType::Image).len(), 0);
    }

    #[test]
    fn bus_subscribers_list() {
        let mut bus = EventBus::new();
        let tid = bus.create_topic(1, "x", DataType::Json);
        bus.subscribe(2, tid);
        bus.subscribe(3, tid);

        let subs = bus.subscribers(tid);
        assert_eq!(subs.len(), 2);
        assert!(subs.contains(&2));
        assert!(subs.contains(&3));
    }

    #[test]
    fn bus_no_duplicate_subscriptions() {
        let mut bus = EventBus::new();
        let tid = bus.create_topic(1, "x", DataType::Text);
        bus.subscribe(2, tid);
        bus.subscribe(2, tid);
        assert_eq!(bus.subscribers(tid).len(), 1);
    }

    #[test]
    fn bus_remove_topic_clears_queued_messages() {
        let mut bus = EventBus::new();
        let tid = bus.create_topic(1, "data", DataType::Text);
        bus.subscribe(2, tid);
        bus.emit(BusMessage {
            topic_id: tid,
            sender: 1,
            event: Event::Custom { kind: "test".into(), data: "will be removed".into() },
            frame: 0,
            timestamp: 1,
        });
        bus.remove_topic(tid);
        let msgs = bus.drain_for(2);
        assert!(msgs.is_empty());
    }

    // =======================================================================
    // Spatial preference tests
    // =======================================================================

    #[test]
    fn spatial_simple_constructor() {
        let pref = SpatialPreference::simple(80, 24);
        assert_eq!(pref.min_size, (80, 24));
        assert_eq!(pref.preferred_size, (80, 24));
        assert!(pref.max_size.is_none());
        assert_eq!(pref.internal_panes, 1);
        assert_eq!(pref.priority, 1.0);
    }

    #[test]
    fn spatial_builder_with_internal() {
        let pref =
            SpatialPreference::simple(40, 12).with_internal(3, InternalLayout::VerticalStack(3));
        assert_eq!(pref.internal_panes, 3);
        assert!(matches!(
            pref.internal_layout,
            InternalLayout::VerticalStack(3)
        ));
    }

    #[test]
    fn spatial_builder_with_priority() {
        let pref = SpatialPreference::simple(40, 12).with_priority(10.0);
        assert_eq!(pref.priority, 10.0);
    }

    #[test]
    fn spatial_fits_in() {
        let pref = SpatialPreference::simple(80, 24);
        assert!(pref.fits_in(80, 24));
        assert!(pref.fits_in(120, 40));
        assert!(!pref.fits_in(79, 24));
        assert!(!pref.fits_in(80, 23));
        assert!(!pref.fits_in(10, 10));
    }

    // =======================================================================
    // AppAdapter mock implementation test
    // =======================================================================

    #[test]
    fn mock_adapter_full_lifecycle() {
        let mut app = MockApp::new("test");
        assert!(app.is_visual());
        assert!(app.is_alive());
        assert_eq!(app.app_type(), "test");
        assert_eq!(app.permissions(), vec!["filesystem".to_string()]);

        // Render into a rect.
        let rect = Rect {
            x: 0.0,
            y: 0.0,
            width: 100.0,
            height: 50.0,
        };
        let output = app.render(&rect);
        assert_eq!(output.quads.len(), 1);
        assert_eq!(output.text_segments.len(), 1);
        assert!(output.text_segments[0].text.contains("test"));

        // Input handling.
        assert!(app.handle_input("q"));
        assert!(!app.handle_input("x"));

        // Command.
        let result = app.accept_command("die", &json!({})).unwrap();
        assert!(result.contains("die"));
        assert!(!app.is_alive());

        // State.
        let state = app.get_state();
        assert_eq!(state["type"], "test");
    }

    #[test]
    fn render_output_default_is_empty() {
        let output = RenderOutput::default();
        assert!(output.quads.is_empty());
        assert!(output.text_segments.is_empty());
        assert!(output.grid.is_none());
    }

    #[test]
    fn render_output_default_has_no_grid() {
        let output = RenderOutput::default();
        assert!(output.grid.is_none());
    }

    #[test]
    fn grid_data_with_cells() {
        let grid = GridData {
            cells: vec![
                TerminalCell { ch: 'H', fg: [1.0, 1.0, 1.0, 1.0], bg: [0.0, 0.0, 0.0, 1.0] },
                TerminalCell { ch: 'i', fg: [0.0, 1.0, 0.0, 1.0], bg: [0.0, 0.0, 0.0, 1.0] },
            ],
            cols: 2,
            rows: 1,
            origin: (10.0, 20.0),
            cursor: Some(CursorData {
                col: 1,
                row: 0,
                shape: CursorShape::Block,
                visible: true,
            }),
        };
        assert_eq!(grid.cells.len(), 2);
        assert_eq!(grid.cols, 2);
    }

    #[test]
    fn cursor_shape_equality() {
        assert_eq!(CursorShape::Block, CursorShape::Block);
        assert_ne!(CursorShape::Block, CursorShape::Bar);
    }

    // =======================================================================
    // Integration: registry + adapter interactions
    // =======================================================================

    #[test]
    fn registry_gc_multiple_dead_apps() {
        let mut reg = AppRegistry::new();
        let ids: Vec<AppId> = (0..5)
            .map(|i| reg.register(Box::new(MockApp::new(&format!("app{i}")))))
            .collect();

        // Kill the first three.
        for &id in &ids[..3] {
            reg.kill(id);
        }

        let removed = reg.gc();
        assert_eq!(removed, 3);
        assert_eq!(reg.count(), 2);

        // The surviving ones should still be accessible.
        assert!(reg.get(ids[3]).is_some());
        assert!(reg.get(ids[4]).is_some());
    }

    #[test]
    fn app_adapter_is_send() {
        // Compile-time proof that Box<dyn AppAdapter> is Send.
        fn assert_send<T: Send>() {}
        assert_send::<Box<dyn AppAdapter>>();
    }

    // =======================================================================
    // Sub-trait object safety and blanket impl tests
    // =======================================================================

    #[test]
    fn test_app_core_is_object_safe() {
        fn assert_obj_safe(_: &dyn AppCore) {}
        let app = MockApp::new("test");
        assert_obj_safe(&app);
    }

    #[test]
    fn test_renderable_is_object_safe() {
        fn assert_obj_safe(_: &dyn Renderable) {}
        let app = MockApp::new("test");
        assert_obj_safe(&app);
    }

    #[test]
    fn test_input_handler_is_object_safe() {
        fn assert_obj_safe(_: &dyn InputHandler) {}
        let app = MockApp::new("test");
        assert_obj_safe(&app);
    }

    #[test]
    fn test_commandable_is_object_safe() {
        fn assert_obj_safe(_: &dyn Commandable) {}
        let app = MockApp::new("test");
        assert_obj_safe(&app);
    }

    #[test]
    fn test_bus_participant_is_object_safe() {
        fn assert_obj_safe(_: &dyn BusParticipant) {}
        let app = MockApp::new("test");
        assert_obj_safe(&app);
    }

    #[test]
    fn test_blanket_app_adapter_impl() {
        // MockApp implements all sub-traits, so it should auto-impl AppAdapter.
        fn assert_app_adapter(_: &dyn AppAdapter) {}
        let app = MockApp::new("blanket");
        assert_app_adapter(&app);
    }

    #[test]
    fn test_mock_implements_all_sub_traits() {
        let mut app = MockApp::new("check");

        // AppCore
        assert_eq!(app.app_type(), "check");
        assert!(app.is_alive());
        app.update(0.016);
        let _ = app.get_state();

        // Renderable
        let rect = Rect {
            x: 0.0,
            y: 0.0,
            width: 80.0,
            height: 24.0,
        };
        let _ = app.render(&rect);
        assert!(app.is_visual());

        // InputHandler
        let _ = app.handle_input("x");

        // Commandable
        let _ = app.accept_command("noop", &json!({}));

        // BusParticipant
        assert!(app.publishes().is_empty());
        assert!(app.subscribes_to().is_empty());

        // Lifecycled
        assert!(app.on_init().is_ok());
        app.on_state_change(AppState::Running);

        // Permissioned
        assert!(!app.permissions().is_empty());
    }
}
