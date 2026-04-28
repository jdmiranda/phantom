//! Substrate runtime — the cognitive plumbing wired into [`App`](crate::app::App).
//!
//! [`AgentRuntime`] owns the long-lived Phase 1/2 substrate primitives:
//!
//! - [`Supervisor`] — restart-policy-driven host for ambient agent tasks.
//! - [`EventLog`] — append-only Park-style memory stream on disk
//!   (`~/.config/phantom/events.jsonl` by default).
//! - [`AgentRegistry`] — id → handle directory used to route inbox messages.
//! - [`MemoryStore`] — Letta-inspired shared K/V blocks for multi-agent
//!   coordination (distinct from the project-scoped JSON
//!   [`phantom_memory::MemoryStore`] kept on `App`).
//! - [`SpawnRuleRegistry`] — declarative "when X event, spawn role Y" rules
//!   evaluated each tick.
//!
//! The `App::update()` loop calls [`AgentRuntime::tick`] once per frame:
//!
//! 1. Reaps the supervisor (restart-policy enforcement on dead children).
//! 2. Drains pending [`SubstrateEvent`]s into the [`EventLog`].
//! 3. For each event, evaluates spawn rules and queues matching
//!    [`SpawnAction`]s. Execution of those actions is the next phase
//!    (Watcher implementation); for now we collect them so the wiring is
//!    end-to-end observable in tests.
//!
//! ## Threading
//!
//! `AgentRuntime` itself is not `Sync`. External producers push events via
//! [`AgentRuntime::push_event`], which uses a mutex-guarded queue, so the
//! same runtime can be fed from multiple threads (e.g. the brain thread or
//! an agent task) while the main loop drains on the render thread.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use phantom_agents::inbox::AgentRegistry;
use phantom_agents::inspector::{EventRow, InspectorBuilder, InspectorView, summarize_event};
use phantom_agents::spawn_rules::{
    EventKind, EventSource as SubstrateEventSource, SpawnAction, SpawnRule, SpawnRuleRegistry,
    SubstrateEvent,
};
use phantom_agents::supervisor::Supervisor;
use phantom_memory::event_log::{EventLog, EventSource as LogEventSource};
use phantom_memory::memory_blocks::MemoryStore;

// Re-export so downstream callers can construct envelopes without a deep
// import. [`LoggedEvent`] is the on-disk shape of an event log line.
pub use phantom_memory::event_log::EventEnvelope as LoggedEvent;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Paths and tunables governing runtime construction.
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    /// On-disk path for the append-only event log.
    pub event_log_path: PathBuf,
}

impl RuntimeConfig {
    /// Default config. The event log lives at `$HOME/.config/phantom/events.jsonl`.
    /// If `$HOME` is unset (CI / sandbox), falls back to `./phantom-events.jsonl`
    /// in the current working directory.
    #[must_use]
    pub fn default_paths() -> Self {
        let home = std::env::var("HOME").ok();
        let event_log_path = match home {
            Some(h) => PathBuf::from(h)
                .join(".config")
                .join("phantom")
                .join("events.jsonl"),
            None => PathBuf::from("phantom-events.jsonl"),
        };
        Self { event_log_path }
    }

    /// Build a config rooted at `dir`. Useful for tests and isolated runs.
    #[must_use]
    pub fn under_dir(dir: &Path) -> Self {
        Self {
            event_log_path: dir.join("events.jsonl"),
        }
    }
}

// ---------------------------------------------------------------------------
// AgentRuntime
// ---------------------------------------------------------------------------

/// The cognitive substrate's runtime, ticked once per frame from
/// [`App::update`](crate::app::App::update).
pub struct AgentRuntime {
    /// Long-lived supervisor for ambient agent tasks. Currently idle; the
    /// next phase wires Watchers and Reflectors as supervised children.
    supervisor: Supervisor,
    /// Append-only on-disk event log, behind an `Arc<Mutex<…>>` so dispatch
    /// contexts handed to running agents (see
    /// [`phantom_agents::dispatch::DispatchContext`]) can share ownership
    /// without copying the log. Tick / shutdown / accessor paths lock briefly
    /// — the log itself is small and append-only.
    event_log: Arc<Mutex<EventLog>>,
    /// Live agent directory for inbox routing, also behind `Arc<Mutex<…>>`
    /// so chat-tool dispatchers (`send_to_agent`, `broadcast_to_role`) and
    /// composer-tool dispatchers (`request_critique`) can co-own it with the
    /// runtime.
    registry: Arc<Mutex<AgentRegistry>>,
    /// Shared K/V blocks for multi-agent coordination.
    memory: MemoryStore,
    /// Declarative spawn rules evaluated each tick.
    rules: SpawnRuleRegistry,
    /// Queue of events awaiting drain to the log. Mutex so external
    /// producers (e.g. background threads) can push without holding the
    /// runtime lock.
    pending: Mutex<Vec<SubstrateEvent>>,
    /// Most recent batch of spawn actions evaluated during the last tick.
    /// Exposed for tests and diagnostics; the next phase consumes these to
    /// drive [`Supervisor::spawn`].
    last_actions: Vec<QueuedAction>,
}

/// A spawn action queued for the next phase to consume.
///
/// We keep both the matched event and the action so the eventual executor
/// can stamp lineage (parent event id, source) onto the spawned agent.
#[derive(Debug, Clone)]
pub struct QueuedAction {
    pub event: SubstrateEvent,
    pub action: SpawnAction,
}

impl AgentRuntime {
    /// Build a runtime with the given config and seed spawn rules.
    ///
    /// The seed rule list is appended to whatever rules `register_default_rules`
    /// installs, so callers can add project-specific behavior without losing
    /// the substrate's ambient defaults.
    pub fn new(config: RuntimeConfig, extra_rules: Vec<SpawnRule>) -> std::io::Result<Self> {
        let event_log = EventLog::open(&config.event_log_path)?;
        let mut rules = SpawnRuleRegistry::new();
        for rule in default_seed_rules() {
            rules = rules.add(rule);
        }
        for rule in extra_rules {
            rules = rules.add(rule);
        }
        Ok(Self {
            supervisor: Supervisor::new(),
            event_log: Arc::new(Mutex::new(event_log)),
            registry: Arc::new(Mutex::new(AgentRegistry::new())),
            memory: MemoryStore::new(),
            rules,
            pending: Mutex::new(Vec::new()),
            last_actions: Vec::new(),
        })
    }

    /// Construct a runtime under `RuntimeConfig::default_paths`.
    pub fn with_default_paths() -> std::io::Result<Self> {
        Self::new(RuntimeConfig::default_paths(), Vec::new())
    }

    /// Push an event into the pending queue. Cheap and lock-bounded;
    /// safe to call from any thread.
    pub fn push_event(&self, event: SubstrateEvent) {
        if let Ok(mut q) = self.pending.lock() {
            q.push(event);
        }
    }

    /// Drive one tick of the substrate.
    ///
    /// Order of operations matters: we reap *first* so any restart-policy
    /// effects from the previous frame are applied before this frame's
    /// rules look at the registry. Then we drain pending events into the
    /// log, and finally evaluate rules for each event.
    pub fn tick(&mut self) {
        // 1. Reap dead supervisor children.
        self.supervisor.reap();

        // 2. Drain the pending queue (snapshot, then drop the lock).
        let drained: Vec<SubstrateEvent> = match self.pending.lock() {
            Ok(mut q) => std::mem::take(&mut *q),
            Err(_) => Vec::new(),
        };

        self.last_actions.clear();

        // 3. Append each event to the log and evaluate spawn rules.
        for ev in &drained {
            // Best-effort log append. We log, but we don't drop the event
            // from rule evaluation if the file write fails — observability
            // beats consistency here.
            if let Ok(mut log) = self.event_log.lock() {
                if let Err(e) = log.append(
                    log_source_for(&ev.source),
                    kind_dotted_name(&ev.kind),
                    ev.payload.clone(),
                ) {
                    log::warn!("event_log append failed: {e}");
                }
            }

            for action in self.rules.evaluate(ev) {
                self.last_actions.push(QueuedAction {
                    event: ev.clone(),
                    action: action.clone(),
                });
            }
        }
    }

    // -----------------------------------------------------------------------
    // Accessors (test/diagnostic surface)
    // -----------------------------------------------------------------------

    /// Lock the event log and return a guard. Used by callers (and tests)
    /// that want to drive `tail`, `path`, etc. directly.
    pub fn event_log(&self) -> std::sync::MutexGuard<'_, EventLog> {
        self.event_log.lock().expect("event log mutex poisoned")
    }

    /// Clone the event-log handle for sharing with a
    /// [`phantom_agents::dispatch::DispatchContext`] or other co-owner. The
    /// underlying `Arc<Mutex<…>>` is the same instance the runtime ticks
    /// against, so writes through any handle are immediately visible to all.
    #[must_use]
    pub fn event_log_handle(&self) -> Arc<Mutex<EventLog>> {
        self.event_log.clone()
    }

    /// Lock the registry and return a guard. Read-only consumers (e.g.
    /// `commands.rs::pair`) use this to look up agents by label without
    /// taking ownership.
    pub fn registry(&self) -> std::sync::MutexGuard<'_, AgentRegistry> {
        self.registry.lock().expect("registry mutex poisoned")
    }

    /// Clone the registry handle for sharing with dispatch contexts. Same
    /// semantics as [`Self::event_log_handle`]: the inner `Arc<Mutex<…>>`
    /// is shared, so chat-tool dispatchers see live agent registrations.
    #[must_use]
    pub fn registry_handle(&self) -> Arc<Mutex<AgentRegistry>> {
        self.registry.clone()
    }

    /// Borrow the spawn-rule registry.
    #[must_use]
    pub fn rules(&self) -> &SpawnRuleRegistry {
        &self.rules
    }

    /// Borrow the memory store.
    #[must_use]
    pub fn memory(&self) -> &MemoryStore {
        &self.memory
    }

    /// Number of running supervised children.
    #[must_use]
    pub fn supervisor_running_count(&self) -> usize {
        self.supervisor.running_count()
    }

    /// Spawn actions queued by the most recent [`Self::tick`].
    #[must_use]
    pub fn last_actions(&self) -> &[QueuedAction] {
        &self.last_actions
    }

    /// Force a flush of the underlying event log buffer. Useful before
    /// shutdown or in tests asserting on-disk state.
    pub fn flush(&mut self) -> std::io::Result<()> {
        let mut log = self.event_log.lock().expect("event log mutex poisoned");
        log.flush()
    }

    /// Build an [`InspectorView`] snapshot from the runtime's current state.
    ///
    /// The view is a pure value: it owns no live handles back into the
    /// runtime, so it's safe to push into a renderer's Arc<RwLock<…>> or
    /// serialize over a socket.
    ///
    /// Phase 8.B scope: agent rows are derived from the [`AgentRegistry`]
    /// (live agents only). The registry's [`phantom_agents::inbox::AgentHandle`]
    /// carries an [`tokio::sync::watch::Receiver<AgentStatus>`] but reading it
    /// inside the snapshot would require a runtime context we don't have
    /// during the GUI's synchronous update loop, so we report a coarse
    /// `"Idle"` placeholder for now. Recent events come from the event log's
    /// in-memory tail, formatted via [`summarize_event`] so the renderer
    /// gets one-liner strings, not raw envelopes.
    #[must_use]
    pub fn snapshot(&self) -> InspectorView {
        let mut builder = InspectorBuilder::new();

        // Pull the most recent events from the in-memory tail and convert
        // them into formatted EventRows. We push newest-last so the renderer
        // gets newest-on-bottom; the inspector adapter is free to reverse if
        // it prefers newest-on-top.
        let envelopes = {
            let log = self.event_log.lock().expect("event log mutex poisoned");
            log.tail(phantom_agents::inspector::MAX_RECENT_EVENTS)
        };
        for env in &envelopes {
            let source_label = match &env.source {
                LogEventSource::Substrate => "Substrate".to_string(),
                LogEventSource::User => "User".to_string(),
                LogEventSource::Agent { id } => format!("Agent: {id}"),
            };
            let summary = summarize_event(&env.kind, &env.payload);
            builder = builder.with_event(EventRow {
                id: env.id,
                ts_ms: env.ts_unix_ms,
                source_label,
                kind: env.kind.clone(),
                summary,
            });
        }

        // Agent rows: project the live registry. We don't have a synchronous
        // way to read the watch::Receiver<AgentStatus> here without entering
        // a Tokio runtime, so we stamp "Idle" as a coarse placeholder. A
        // later phase can plumb a non-async status accessor through
        // AgentHandle and replace this projection.
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let agent_refs: Vec<_> = {
            let reg = self.registry.lock().expect("registry mutex poisoned");
            reg.list().into_iter().cloned().collect()
        };
        for agent_ref in agent_refs {
            let row = phantom_agents::inspector::AgentRow::new(
                agent_ref,
                "Idle",
                None,
                None,
                0,
                now_ms,
            );
            builder = builder.with_agent(row);
        }

        builder.build()
    }
}

// ---------------------------------------------------------------------------
// Seed rules
// ---------------------------------------------------------------------------

/// The default ambient spawn rules installed at startup.
///
/// Two rules ship today:
/// 1. **Observability** — when an agent pane is opened, we want a record in
///    the event log so substrate test code can assert that the wiring is
///    end-to-end. Fires `SpawnIfNotRunning(Watcher, "agent-pane-watch")`.
/// 2. **Fixer-on-blockage** (Phase 2.E/2.G consumer) — when any agent emits
///    `EventKind::AgentBlocked` after its consecutive-failure streak crosses
///    `TOOL_BLOCK_THRESHOLD`, fire `SpawnIfNotRunning(Fixer, "fixer-on-blockage")`.
///    The actual Fixer agent spawn is the next phase; today the queued
///    action is observable via [`AgentRuntime::last_actions`] and is what
///    closes the producer→consumer loop in tests.
///
/// Both rules are passive (no actual agent is spawned yet) — their presence
/// is verified via the event log and `last_actions`.
fn default_seed_rules() -> Vec<SpawnRule> {
    use phantom_agents::role::AgentRole;

    vec![
        SpawnRule::on(EventKind::PaneOpened {
            app_type: "agent".to_string(),
        })
        .spawn_if_not_running(AgentRole::Watcher, "agent-pane-watch"),
        // Fixer-on-blockage (Phase 2.E/2.G): when any agent emits
        // `EventKind::AgentBlocked` (after its consecutive-failure streak
        // crosses `TOOL_BLOCK_THRESHOLD`), queue a `SpawnIfNotRunning(Fixer)`
        // action. The actual Fixer spawn is the next phase; today the
        // queued action is observable via [`AgentRuntime::last_actions`] and
        // is what closes the producer→consumer loop in tests.
        phantom_agents::fixer::fixer_spawn_rule(),
        // Defender-on-denial (Sec.4): when the Layer-2 dispatch gate emits
        // `EventKind::CapabilityDenied` for any agent, queue a
        // `SpawnIfNotRunning(Defender)` action. The Defender's challenge
        // tool lands in Sec.5; today the queued action proves the
        // spawn-on-denial wiring is end-to-end and is observable via
        // [`AgentRuntime::last_actions`].
        phantom_agents::defender::defender_spawn_rule(),
    ]
}

// ---------------------------------------------------------------------------
// SubstrateEvent ↔ EventLog plumbing
// ---------------------------------------------------------------------------

/// Translate the runtime's [`SubstrateEventSource`] into the
/// [`LogEventSource`] consumed by the on-disk log.
fn log_source_for(src: &SubstrateEventSource) -> LogEventSource {
    match src {
        SubstrateEventSource::Substrate => LogEventSource::Substrate,
        // We don't have agent ids on `SubstrateEventSource` (it carries a
        // role, not an id) — record `id: 0` as a sentinel until the next
        // phase ties spawned-agent ids back into the source.
        SubstrateEventSource::Agent { .. } => LogEventSource::Agent { id: 0 },
        SubstrateEventSource::User => LogEventSource::User,
    }
}

/// Best-effort dotted-name encoding of [`EventKind`] for the log's `kind`
/// field. Watchers/Reflectors filter by these strings.
fn kind_dotted_name(kind: &EventKind) -> String {
    match kind {
        EventKind::PaneOpened { app_type } => format!("pane.opened.{app_type}"),
        EventKind::PaneClosed { app_type } => format!("pane.closed.{app_type}"),
        EventKind::AgentSpawned { role } => {
            format!("agent.spawned.{}", role.label().to_ascii_lowercase())
        }
        EventKind::AgentExited { role, success } => format!(
            "agent.exited.{}.{}",
            role.label().to_ascii_lowercase(),
            if *success { "ok" } else { "fail" }
        ),
        EventKind::AgentBlocked { agent_id, .. } => {
            format!("agent.blocked.{agent_id}")
        }
        // `agent.capability_denied.<id>` so log scrapers can grep by agent
        // and downstream Defender hooks can filter the on-disk log
        // (Sec.4 reads this prefix).
        EventKind::CapabilityDenied { agent_id, .. } => {
            format!("agent.capability_denied.{agent_id}")
        }
        EventKind::AudioStreamAvailable => "stream.audio.available".to_string(),
        EventKind::VideoStreamAvailable => "stream.video.available".to_string(),
        EventKind::UserCommandSubmitted => "user.command.submitted".to_string(),
        EventKind::Custom(s) => format!("custom.{s}"),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use phantom_agents::role::AgentRole;
    use serde_json::json;
    use tempfile::tempdir;

    fn make_runtime() -> (AgentRuntime, tempfile::TempDir) {
        let dir = tempdir().expect("tempdir");
        let cfg = RuntimeConfig::under_dir(dir.path());
        let rt = AgentRuntime::new(cfg, Vec::new()).expect("runtime open");
        (rt, dir)
    }

    fn pane_opened(app_type: &str) -> SubstrateEvent {
        SubstrateEvent {
            kind: EventKind::PaneOpened {
                app_type: app_type.to_string(),
            },
            payload: json!({"app_type": app_type}),
            source: SubstrateEventSource::User,
        }
    }

    /// Pushing a synthetic event then ticking must result in exactly one
    /// log entry visible via `tail(1)`.
    #[test]
    fn runtime_tick_drains_events_to_log() {
        let (mut rt, _dir) = make_runtime();

        rt.push_event(pane_opened("terminal"));
        rt.tick();

        let tail = rt.event_log().tail(1);
        assert_eq!(tail.len(), 1, "exactly one event should land in the log");
        assert_eq!(tail[0].kind, "pane.opened.terminal");
    }

    /// The seed `PaneOpened { app_type: "agent" }` rule must fire when a
    /// matching event is pushed, queueing one [`SpawnAction`].
    #[test]
    fn spawn_rule_fires_for_matching_event() {
        let (mut rt, _dir) = make_runtime();

        rt.push_event(pane_opened("agent"));
        rt.tick();

        let actions = rt.last_actions();
        assert_eq!(actions.len(), 1, "agent pane should trigger one rule");
        match &actions[0].action {
            SpawnAction::SpawnIfNotRunning {
                role,
                label_template,
                ..
            } => {
                assert_eq!(*role, AgentRole::Watcher);
                assert_eq!(label_template, "agent-pane-watch");
            }
            other => panic!("expected SpawnIfNotRunning, got {other:?}"),
        }
    }

    /// Pushing an event whose kind doesn't match any rule must yield zero
    /// queued actions — the registry stays unmutated, the log still gets
    /// the entry, but `last_actions` is empty.
    #[test]
    fn unmatched_event_does_not_fire_rule() {
        let (mut rt, _dir) = make_runtime();

        // The seed rule only matches PaneOpened { app_type: "agent" }.
        rt.push_event(pane_opened("video"));
        rt.tick();

        assert!(rt.last_actions().is_empty(), "non-matching event must not queue actions");
        // But the event still hit the log.
        assert_eq!(rt.event_log().tail(1)[0].kind, "pane.opened.video");
    }

    /// After the runtime is dropped (which flushes the writer), the on-disk
    /// log file must exist and contain at least one JSON line.
    #[test]
    fn event_log_persists_to_disk() {
        let dir = tempdir().expect("tempdir");
        let cfg = RuntimeConfig::under_dir(dir.path());
        let path = cfg.event_log_path.clone();

        {
            let mut rt = AgentRuntime::new(cfg, Vec::new()).expect("open");
            rt.push_event(pane_opened("agent"));
            rt.tick();
            // Drop here flushes via EventLog::Drop.
        }

        assert!(path.exists(), "log file should exist on disk: {}", path.display());
        let contents = std::fs::read_to_string(&path).expect("read log");
        let lines: Vec<&str> = contents.lines().filter(|l| !l.is_empty()).collect();
        assert!(!lines.is_empty(), "log should have at least one line");
        // The first non-empty line must be valid JSON.
        let _: serde_json::Value =
            serde_json::from_str(lines[0]).expect("valid JSON line");
    }

    /// Custom spawn rules supplied via `extra_rules` must be additive on
    /// top of the seed rules.
    #[test]
    fn extra_rules_compose_with_seed_rules() {
        let dir = tempdir().expect("tempdir");
        let cfg = RuntimeConfig::under_dir(dir.path());
        let extra = vec![SpawnRule::on(EventKind::AudioStreamAvailable)
            .spawn(AgentRole::Capturer, "audio-cap")];
        let mut rt = AgentRuntime::new(cfg, extra).expect("open");

        rt.push_event(SubstrateEvent {
            kind: EventKind::AudioStreamAvailable,
            payload: json!({}),
            source: SubstrateEventSource::Substrate,
        });
        rt.tick();

        let actions = rt.last_actions();
        assert_eq!(actions.len(), 1);
        match &actions[0].action {
            SpawnAction::Spawn { role, .. } => assert_eq!(*role, AgentRole::Capturer),
            other => panic!("expected Spawn, got {other:?}"),
        }
    }

    /// A fresh runtime owns no children.
    #[test]
    fn supervisor_starts_with_no_children() {
        let (rt, _dir) = make_runtime();
        assert_eq!(rt.supervisor_running_count(), 0);
    }

    /// Pushing N events and ticking should produce a snapshot whose
    /// `recent_events` reflects all N entries (in event-log order).
    #[test]
    fn runtime_snapshot_returns_current_state() {
        let (mut rt, _dir) = make_runtime();

        rt.push_event(pane_opened("agent"));
        rt.push_event(pane_opened("terminal"));
        rt.push_event(pane_opened("video"));
        rt.tick();

        let view = rt.snapshot();
        assert_eq!(view.recent_events.len(), 3, "snapshot should carry all 3 events");
        // Kinds round-tripped through dotted name + summarize_event.
        let kinds: Vec<&str> = view.recent_events.iter().map(|e| e.kind.as_str()).collect();
        assert!(kinds.contains(&"pane.opened.agent"));
        assert!(kinds.contains(&"pane.opened.terminal"));
        assert!(kinds.contains(&"pane.opened.video"));
        // No agents are registered yet.
        assert!(view.agents.is_empty());
        assert_eq!(view.spawned_total, 0);
        assert_eq!(view.running_count, 0);
    }

    /// The seed `fixer_spawn_rule` must fire when an `AgentBlocked` event
    /// is pushed to a runtime built via `default_seed_rules`. This pins the
    /// substrate-level guarantee that the consumer wiring in
    /// `update.rs::update` (drain sink → runtime.push_event) lands a Fixer
    /// `SpawnAction` in `last_actions()` on the next tick.
    #[test]
    fn fixer_rule_fires_on_agent_blocked() {
        let (mut rt, _dir) = make_runtime();

        rt.push_event(SubstrateEvent {
            kind: EventKind::AgentBlocked {
                agent_id: 7,
                reason: "2+ consecutive tool failures".to_string(),
            },
            payload: json!({
                "agent_id": 7,
                "agent_role": "Conversational",
                "reason": "2+ consecutive tool failures",
            }),
            source: SubstrateEventSource::Agent {
                role: AgentRole::Conversational,
            },
        });
        rt.tick();

        let actions = rt.last_actions();
        assert_eq!(actions.len(), 1, "fixer rule must fire exactly once");
        match &actions[0].action {
            SpawnAction::SpawnIfNotRunning {
                role,
                label_template,
                ..
            } => {
                assert_eq!(*role, AgentRole::Fixer);
                assert_eq!(label_template, "fixer-on-blockage");
            }
            other => panic!("expected SpawnIfNotRunning(Fixer), got {other:?}"),
        }
    }

    /// Sec.1: pushing a `CapabilityDenied` event into the runtime, ticking,
    /// then flushing must produce an `agent.capability_denied.<id>` line in
    /// the on-disk `events.jsonl`. This is the substrate-level guarantee
    /// that an emitted denial is durably recorded — Sec.4 (Defender) reads
    /// these lines from the log.
    #[test]
    fn runtime_drain_capability_denied_writes_to_event_log() {
        use phantom_agents::role::CapabilityClass;

        let dir = tempdir().expect("tempdir");
        let cfg = RuntimeConfig::under_dir(dir.path());
        let path = cfg.event_log_path.clone();

        {
            let mut rt = AgentRuntime::new(cfg, Vec::new()).expect("open");

            rt.push_event(SubstrateEvent {
                kind: EventKind::CapabilityDenied {
                    agent_id: 99,
                    role: AgentRole::Watcher,
                    attempted_class: CapabilityClass::Act,
                    attempted_tool: "run_command".to_string(),
                    source_chain: Vec::new(),
                },
                payload: json!({
                    "agent_id": 99,
                    "agent_role": "Watcher",
                    "attempted_class": "Act",
                    "attempted_tool": "run_command",
                    "source_chain": [],
                }),
                source: SubstrateEventSource::Agent {
                    role: AgentRole::Watcher,
                },
            });
            rt.tick();
            rt.flush().expect("flush");
            // Drop the runtime here so EventLog::Drop guarantees the buffer
            // hits disk before we read.
        }

        // Read the on-disk log. The first non-empty line must carry the
        // `agent.capability_denied.<id>` dotted name, which `kind_dotted_name`
        // produces for `EventKind::CapabilityDenied`.
        let contents = std::fs::read_to_string(&path).expect("read log");
        let needle = "agent.capability_denied.99";
        assert!(
            contents.contains(needle),
            "expected '{needle}' in events.jsonl; got:\n{contents}",
        );

        // The line must be valid JSON with the expected payload fields.
        let line = contents
            .lines()
            .find(|l| l.contains(needle))
            .expect("find line");
        let parsed: serde_json::Value =
            serde_json::from_str(line).expect("valid JSON");
        assert_eq!(
            parsed.get("kind").and_then(|v| v.as_str()),
            Some(needle),
        );
        // payload.attempted_tool must be the wire name the model called.
        let payload = parsed.get("payload").expect("payload field");
        assert_eq!(
            payload.get("attempted_tool").and_then(|v| v.as_str()),
            Some("run_command"),
        );
        assert_eq!(
            payload.get("attempted_class").and_then(|v| v.as_str()),
            Some("Act"),
        );
    }
}
