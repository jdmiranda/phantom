# ARD-003: App Lifecycle, Pub/Sub Event Bus, and Spatial Negotiation

**Status**: Accepted
**Date**: 2026-04-21
**Authors**: Jeremy Miranda, Claude

---

## Decision

Phantom introduces a unified **App** abstraction with:
1. **Lifecycle states** — governed enum, auto-cleanup on death
2. **Discovery + registration** — apps self-register, auto-remove on exit
3. **Pub/sub event bus** — apps publish/subscribe to typed data streams
4. **Headless apps** — apps that process data without rendering
5. **Spatial negotiation** — apps declare layout preferences, arbiter resolves

## Context

Phantom needs to evolve from "terminal with panes" to "app platform where the terminal is one app among many." Third-party apps (database browsers, log viewers, transcription services) need to participate equally with built-in components. The AI brain needs uniform visibility into all apps. Data needs to flow between apps like Unix pipes but for structured data.

Inspiration: Yahoo Pipes (visual data flow programming), Unix philosophy (small tools that compose), Emacs (everything is a buffer), Wayland (negotiated layout).

---

## 1. App Lifecycle States

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppState {
    /// App is loading/initializing. Not yet ready for events.
    Initializing,
    /// App is running normally. Receives events, renders (if visual).
    Running,
    /// App is paused/backgrounded. No updates, retains state.
    Suspended,
    /// App is shutting down gracefully. Finishing work, saving state.
    Exiting,
    /// App is dead. Will be cleaned up by the registry.
    Dead,
}
```

**State transitions**:
```
Initializing → Running       (init complete)
Running → Suspended          (user backgrounds, pane hidden)
Suspended → Running          (user foregrounds)
Running → Exiting            (user closes, app requests exit)
Exiting → Dead               (cleanup complete)
ANY → Dead                   (crash, kill, timeout)
```

**Governance rules**:
- Only `Running` apps receive input events
- Only `Running` or `Suspended` apps can publish to the event bus
- `Dead` apps are garbage-collected on the next frame
- Transition to `Dead` triggers: unsubscribe all, release spatial claim, notify dependents
- Apps get a `SIGTERM`-equivalent (state → Exiting) before force-kill (state → Dead)
- Timeout: if Exiting doesn't reach Dead within 5 seconds, force-kill

## 2. App Discovery + Auto-Registration

```rust
pub struct AppRegistry {
    apps: Vec<RegisteredApp>,
    next_id: AppId,
    event_bus: EventBus,
    arbiter: LayoutArbiter,
}

struct RegisteredApp {
    id: AppId,
    adapter: Box<dyn AppAdapter>,
    state: AppState,
    node_id: Option<NodeId>,    // scene graph node (None for headless)
    subscriptions: Vec<TopicId>,
    publications: Vec<TopicId>,
    spatial_pref: Option<SpatialPreference>,
    registered_at: Instant,
}

impl AppRegistry {
    /// Register a new app. Assigns ID, sets state to Initializing.
    pub fn register(&mut self, app: Box<dyn AppAdapter>) -> AppId;
    
    /// App signals it's ready. State → Running.
    pub fn ready(&mut self, id: AppId);
    
    /// Request app exit. State → Exiting.
    pub fn request_exit(&mut self, id: AppId);
    
    /// Force kill. State → Dead immediately.
    pub fn kill(&mut self, id: AppId);
    
    /// Garbage collect dead apps.
    pub fn gc(&mut self);
    
    /// Get all apps in a given state.
    pub fn by_state(&self, state: AppState) -> Vec<AppId>;
    
    /// Get app by ID.
    pub fn get(&self, id: AppId) -> Option<&dyn AppAdapter>;
}
```

**Auto-cleanup**: the render loop calls `gc()` each frame. Dead apps are:
- Removed from the scene graph
- Unsubscribed from all topics
- Spatial claims released
- Dependents notified via `AppDied(AppId)` event

## 3. Pub/Sub Event Bus

Inspired by Yahoo Pipes: apps are nodes, data flows between them via typed topics.

```rust
pub type TopicId = u32;

/// A typed data stream between apps.
pub struct Topic {
    pub id: TopicId,
    pub name: String,
    pub data_type: DataType,
    pub publisher: AppId,
}

#[derive(Debug, Clone)]
pub enum DataType {
    Text,              // plain text stream
    Json,              // structured JSON objects
    Audio,             // audio stream (for speech-to-text)
    Image,             // image frames
    TerminalOutput,    // parsed terminal output
    Custom(String),    // plugin-defined
}

/// A message on the event bus.
#[derive(Debug, Clone)]
pub struct BusMessage {
    pub topic: TopicId,
    pub sender: AppId,
    pub payload: serde_json::Value,
    pub timestamp: u64,
}

pub struct EventBus {
    topics: HashMap<TopicId, Topic>,
    subscriptions: HashMap<TopicId, Vec<AppId>>,
    queue: VecDeque<BusMessage>,
}

impl EventBus {
    /// App publishes a new topic. Returns TopicId.
    pub fn publish_topic(&mut self, publisher: AppId, name: &str, data_type: DataType) -> TopicId;
    
    /// App subscribes to a topic.
    pub fn subscribe(&mut self, subscriber: AppId, topic: TopicId);
    
    /// App unsubscribes.
    pub fn unsubscribe(&mut self, subscriber: AppId, topic: TopicId);
    
    /// Publish a message to a topic.
    pub fn emit(&mut self, message: BusMessage);
    
    /// Drain messages for a specific subscriber.
    pub fn drain_for(&mut self, subscriber: AppId) -> Vec<BusMessage>;
    
    /// List all available topics.
    pub fn topics(&self) -> Vec<&Topic>;
    
    /// Find topics by data type.
    pub fn topics_by_type(&self, data_type: &DataType) -> Vec<&Topic>;
}
```

**Example pipeline** (user's speech-to-text scenario):

```
BrowserApp                SpeechToTextApp          FileWriterApp
(renders page)            (headless)               (headless)
    |                          |                        |
    | publishes:               | subscribes to:         | subscribes to:
    | "browser.audio"          | "browser.audio"        | "transcript.text"
    | (DataType::Audio)        |                        |
    |                          | publishes:             |
    |                          | "transcript.text"      | writes to:
    |                          | (DataType::Text)       | ~/transcripts/
    |                          |                        |
    v                          v                        v
  [audio stream] ────────► [transcription] ────────► [file output]
```

Each app declares what it publishes and subscribes to. The event bus routes messages. Apps auto-discover available topics. The AI brain can read the entire topology.

## 4. Headless Apps

Not all apps need a pane. Some are pure data processors:

```rust
trait AppAdapter: Send {
    // ... existing methods ...
    
    /// Does this app render visually?
    fn is_visual(&self) -> bool { true }  // default: yes
    
    /// For headless apps: called each update tick instead of render.
    fn process(&mut self) {}
}
```

Headless apps:
- Have `is_visual() = false`
- No scene graph node, no spatial claim
- Still registered in AppRegistry
- Still participate in pub/sub
- Still visible to AI brain via `get_state()`
- Shown in an "Apps" panel (like Activity Monitor) but not in the main workspace

Examples: speech-to-text, file watcher, CI poller, log aggregator, clipboard history.

## 5. Spatial Negotiation

See [spatial-negotiation.md](research/spatial-negotiation.md) for full research.

**Summary**: apps declare `SpatialPreference` (min/max/preferred size, internal panes, priority). The `LayoutArbiter` resolves conflicts using:
- Priority ordering (higher priority gets first pick)
- Cassowary-style constraint solving for complex layouts
- Wayland-style two-phase negotiation (suggest → ack)
- Neighbor queries (apps ask neighbors to resize)

## 6. Updated AppAdapter Trait

```rust
pub trait AppAdapter: Send {
    /// Unique app type identifier.
    fn app_type(&self) -> &str;
    
    /// Permissions this app requires.
    fn permissions(&self) -> &[Permission];
    
    /// Does this app render visually?
    fn is_visual(&self) -> bool { true }
    
    /// Spatial layout preferences.
    fn spatial_preference(&self) -> Option<SpatialPreference> { None }
    
    /// Lifecycle: called once when app starts.
    fn on_init(&mut self) -> Result<()> { Ok(()) }
    
    /// Lifecycle: called when state transitions.
    fn on_state_change(&mut self, new_state: AppState) {}
    
    /// Render into a quad + glyph buffer.
    fn render(&self, rect: &Rect) -> (Vec<QuadInstance>, Vec<GlyphInstance>);
    
    /// Handle keyboard input. Returns true if consumed.
    fn handle_input(&mut self, key: &KeyEvent) -> bool;
    
    /// Get current state as structured data (AI reads this).
    fn get_state(&self) -> serde_json::Value;
    
    /// Accept a command from AI or another app.
    fn accept_command(&mut self, cmd: &str, args: &serde_json::Value) -> Result<String>;
    
    /// Update tick (called every frame for visual, periodically for headless).
    fn update(&mut self, dt: f32);
    
    /// Process tick (headless apps only).
    fn process(&mut self) {}
    
    /// Is this app still alive?
    fn is_alive(&self) -> bool;
    
    /// What topics does this app publish?
    fn publishes(&self) -> Vec<(String, DataType)> { vec![] }
    
    /// What topics does this app want to subscribe to?
    fn subscribes_to(&self) -> Vec<String> { vec![] }
    
    /// Receive a message from the event bus.
    fn on_message(&mut self, msg: &BusMessage) {}
}
```

## Implementation Order

1. `phantom-adapter` crate — trait + registry + lifecycle + event bus
2. ADR this document
3. `TerminalApp` — wrap existing PhantomTerminal
4. `AgentApp` — wrap existing Agent
5. Refactor `app.rs` panes to `Vec<RegisteredApp>`
6. Spatial negotiation in scene graph
7. wasmtime runtime for third-party WASM apps
8. Example headless app (file watcher)
9. Example pipeline (terminal output → semantic parser → memory updater)

## References

- [Yahoo Pipes](https://en.wikipedia.org/wiki/Yahoo!_Pipes) — visual data flow programming (2007-2015)
- [Unix Philosophy](https://en.wikipedia.org/wiki/Unix_philosophy) — small tools that compose via pipes
- [Wayland xdg-shell](https://wayland-book.com/xdg-shell-in-depth/configuration.html) — negotiated layout
- [Cassowary Constraint Solver](https://cassowary.readthedocs.io/en/latest/topics/theory.html) — linear constraint solving
- [Emacs Buffers](https://www.gnu.org/software/emacs/) — everything is a buffer
- [ROS Topics](http://wiki.ros.org/Topics) — pub/sub in robotics (closest analogue to our event bus)
