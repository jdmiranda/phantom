//! Inspector view-model: a pure read over the substrate's agent registry and
//! event log, shaped for a renderer that doesn't yet exist.
//!
//! The Inspector is the user-facing window into the agent runtime. It answers
//! three questions at a glance:
//!
//! 1. **Who's running?** A row per live agent with role, label, status, age,
//!    and a one-line excerpt of the last thing it said.
//! 2. **What just happened?** A capped, time-ordered tail of the most recent
//!    substrate events, rendered as one-line summaries.
//! 3. **What's the population?** Aggregate counters (spawned-total,
//!    running-now) for the header strip.
//!
//! ## Pure read, no mutation
//!
//! [`InspectorView`] is a snapshot value. It owns no `Arc`s, no channels, no
//! handles back into the substrate. Producers (the supervisor, the registry,
//! the event log) project their state into rows via [`InspectorBuilder`] at
//! whatever cadence makes sense. The renderer then consumes the resulting
//! [`InspectorView`] without ever needing a live reference to the runtime.
//!
//! This separation is deliberate. The inspector view-model is small, cloneable,
//! and serializable — it can be sent over a socket, dumped to a debug log,
//! diffed across frames, or mocked in tests. The renderer (Phase 8.B+) can be
//! rebuilt without touching this contract.
//!
//! ## Determinism
//!
//! [`summarize_event`] is a pure function over `(kind, payload)`. Same input,
//! same output. Tests can pin the exact summary string for any envelope.
//! [`AgentRow`] derives `spawned_minutes_ago` from an injected `now_unix_ms`
//! so age computation is reproducible.
//!
//! ## Cap policy
//!
//! `InspectorView::recent_events` is capped at [`MAX_RECENT_EVENTS`] (100).
//! When a builder accumulates more than that, the **oldest** events are
//! dropped — the tail is what the user is watching, and an unbounded tail
//! would balloon the snapshot. Agent rows are not capped; if you have 5,000
//! agents you have bigger problems than the inspector pane.

use serde::ser::{SerializeStruct, Serializer};
use serde::Serialize;
use serde_json::Value;

use crate::peer_grants::PeerId;
use crate::role::{AgentRef, CapabilityClass, SpawnSource};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Hard cap on `InspectorView::recent_events`. The tail is what the user
/// watches; older events live in the full event log, not the snapshot.
pub const MAX_RECENT_EVENTS: usize = 100;

/// Cap on the speech excerpt carried in [`AgentRow::last_speech_excerpt`].
/// Roughly one terminal column-row on a narrow inspector pane.
pub const SPEECH_EXCERPT_CHARS: usize = 80;

// ---------------------------------------------------------------------------
// AgentRow
// ---------------------------------------------------------------------------

/// One row in the "who's running?" table.
///
/// Carries the agent's identity (`agent_ref`), a coarse status string, age,
/// restart count, and a short excerpt of the most recent thing the agent
/// said. The renderer turns this into a row with a status badge, a sparkline,
/// and a truncated one-liner.
///
/// Hand-rolled `Serialize` impl rather than `derive` because `AgentRef` and
/// `SpawnSource` (in `role.rs`) don't derive `Serialize`. We project the
/// AgentRef into stable wire-shape fields here so consumers of the snapshot
/// don't need to reach into `role`.
#[derive(Debug, Clone)]
pub struct AgentRow {
    pub agent_ref: AgentRef,
    /// Coarse lifecycle status. One of `"Spawning" | "Idle" | "Working" |
    /// "EmittingSpeech" | "Stopped" | "Failed"`. Stringly-typed on purpose so
    /// the renderer doesn't need to import the agent crate's enum.
    pub status: String,
    /// Wall-clock timestamp of the last speech token this agent emitted.
    /// `None` if it hasn't spoken yet.
    pub last_speech_at_ms: Option<u64>,
    /// First [`SPEECH_EXCERPT_CHARS`] of the last speech body, with whitespace
    /// trimmed. `None` if the agent hasn't spoken.
    pub last_speech_excerpt: Option<String>,
    /// How many times the supervisor has restarted this agent under the
    /// current id-line. Zero for a freshly-spawned agent.
    pub restart_count: u32,
    /// Whole minutes elapsed between `agent_ref.spawned_at_unix_ms` and the
    /// `now_unix_ms` passed to [`AgentRow::new`]. Saturates at 0 if the clock
    /// went backward.
    pub spawned_minutes_ago: u64,
}

impl AgentRow {
    /// Build a row from its parts, computing `spawned_minutes_ago` from
    /// `agent_ref.spawned_at_unix_ms` and the supplied `now_unix_ms`.
    ///
    /// The excerpt is truncated to [`SPEECH_EXCERPT_CHARS`] characters
    /// (Unicode scalar values, not bytes) and trimmed.
    pub fn new(
        agent_ref: AgentRef,
        status: impl Into<String>,
        last_speech_at_ms: Option<u64>,
        last_speech_body: Option<&str>,
        restart_count: u32,
        now_unix_ms: u64,
    ) -> Self {
        let spawned_minutes_ago = now_unix_ms
            .saturating_sub(agent_ref.spawned_at_unix_ms)
            / 60_000;
        let last_speech_excerpt = last_speech_body.map(excerpt);
        Self {
            agent_ref,
            status: status.into(),
            last_speech_at_ms,
            last_speech_excerpt,
            restart_count,
            spawned_minutes_ago,
        }
    }
}

impl Serialize for AgentRow {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        let mut s = ser.serialize_struct("AgentRow", 9)?;
        s.serialize_field("id", &self.agent_ref.id)?;
        s.serialize_field("role", self.agent_ref.role.label())?;
        s.serialize_field("label", &self.agent_ref.label)?;
        s.serialize_field("spawned_at_unix_ms", &self.agent_ref.spawned_at_unix_ms)?;
        s.serialize_field("spawned_by", &spawn_source_label(self.agent_ref.spawned_by))?;
        s.serialize_field("status", &self.status)?;
        s.serialize_field("last_speech_at_ms", &self.last_speech_at_ms)?;
        s.serialize_field("last_speech_excerpt", &self.last_speech_excerpt)?;
        s.serialize_field("restart_count", &self.restart_count)?;
        s.serialize_field("spawned_minutes_ago", &self.spawned_minutes_ago)?;
        s.end()
    }
}

// ---------------------------------------------------------------------------
// EventRow
// ---------------------------------------------------------------------------

/// One row in the "what just happened?" feed.
///
/// `summary` is the human-readable one-liner produced by [`summarize_event`];
/// `kind` is the raw envelope kind (e.g. `"agent.spawn"`) so the renderer
/// can drive coloring or filtering off the machine-readable type.
#[derive(Debug, Clone, Serialize)]
pub struct EventRow {
    pub id: u64,
    pub ts_ms: i64,
    /// Where the event came from, formatted for display: `"Substrate"`,
    /// `"User"`, or `"Agent: <label>"`.
    pub source_label: String,
    pub kind: String,
    pub summary: String,
}

// ---------------------------------------------------------------------------
// DenialEntry
// ---------------------------------------------------------------------------

/// Hard cap on `InspectorView::denials`. Mirrors the bounded-tail discipline
/// used elsewhere on the snapshot: the view shows the most recent denials,
/// older ones live in the on-disk log.
pub const MAX_RECENT_DENIALS: usize = 20;

/// One row in the "DENIALS" section.
///
/// A denial is the substrate's record of a `CapabilityDenied` event:
/// the Layer-2 dispatch gate refused a tool call because the calling agent's
/// role manifest lacked the tool's capability class. The Inspector surfaces
/// these so the user can see, at a glance, which agents are bumping against
/// the security boundary and which capability axis they tried.
///
/// Fields mirror the `EventKind::CapabilityDenied` payload one-for-one (with
/// the source chain split out for renderer convenience). Stringly-typed
/// `role` and `attempted_class` so the renderer (and downstream consumers
/// like a future export-to-JSON) don't need to import the agent crate's
/// enums.
#[derive(Debug, Clone, Serialize)]
pub struct DenialEntry {
    /// Human-readable role label of the denied agent (e.g. `"Watcher"`).
    pub role: String,
    /// Wire name of the tool the agent attempted to invoke
    /// (e.g. `"run_command"`).
    pub attempted_tool: String,
    /// String form of the [`crate::role::CapabilityClass`] the gate refused
    /// (e.g. `"Act"`). One of `Sense | Reflect | Compute | Act | Coordinate`.
    pub attempted_class: String,
    /// Provenance trail: substrate event ids that led to this dispatch. May
    /// be empty until Sec.2 wires provenance through every dispatch.
    pub source_chain: Vec<u64>,
    /// Wall-clock millis since epoch when the denial was recorded.
    pub timestamp_ms: u64,
}

// ---------------------------------------------------------------------------
// PeerRow
// ---------------------------------------------------------------------------

/// One row in the "connected peers" table.
///
/// Represents a connected peer with its identity and the set of capabilities
/// granted to it. Used in the Peers tab of the Inspector.
#[derive(Debug, Clone, Serialize)]
pub struct PeerRow {
    /// The peer's unique identifier.
    pub peer_id: PeerId,
    /// Human-readable display name for the peer (e.g., "JeremyMBP").
    pub display_name: String,
    /// Allowed capability classes for this peer.
    pub granted_capabilities: Vec<CapabilityClass>,
}

impl PeerRow {
    /// Construct a peer row with the given id, name, and capabilities.
    pub fn new(
        peer_id: PeerId,
        display_name: String,
        granted_capabilities: Vec<CapabilityClass>,
    ) -> Self {
        Self {
            peer_id,
            display_name,
            granted_capabilities,
        }
    }
}

// ---------------------------------------------------------------------------
// InspectorView
// ---------------------------------------------------------------------------

/// A snapshot of the runtime as the inspector pane will render it.
///
/// Built by [`InspectorBuilder`]. Owns no live state — every field is a
/// concrete value cloned out of whatever produced it. Cheap to clone, cheap
/// to serialize, cheap to diff.
#[derive(Debug, Clone, Serialize)]
pub struct InspectorView {
    /// Agents currently known to the registry, sorted alphabetically by label
    /// so the row order is stable across snapshots.
    pub agents: Vec<AgentRow>,
    /// Most-recent-first tail of the event log. Capped at
    /// [`MAX_RECENT_EVENTS`].
    pub recent_events: Vec<EventRow>,
    /// Total agents ever spawned in this session (including ones now stopped
    /// or failed). Drives the "spawned: N" counter.
    pub spawned_total: u32,
    /// Subset of `spawned_total` still in `Spawning | Idle | Working |
    /// EmittingSpeech`. Drives the "live: N" counter.
    pub running_count: u32,
    /// Most-recent-first tail of `EventKind::CapabilityDenied` events.
    /// Capped at [`MAX_RECENT_DENIALS`]. Empty when no agent has hit the
    /// dispatch gate.
    pub denials: Vec<DenialEntry>,
    /// Connected peers with their granted capabilities. Used by the Peers tab.
    pub peers: Vec<PeerRow>,
    /// Local node identity for display in the Peers tab.
    pub local_node_id: String,
}

impl InspectorView {
    /// The view rendered before anything has happened: no agents, no events,
    /// counters at zero. Useful as the renderer's initial state.
    pub fn empty() -> Self {
        Self {
            agents: Vec::new(),
            recent_events: Vec::new(),
            spawned_total: 0,
            running_count: 0,
            denials: Vec::new(),
            peers: Vec::new(),
            local_node_id: String::from("localhost"),
        }
    }
}

// ---------------------------------------------------------------------------
// InspectorBuilder
// ---------------------------------------------------------------------------

/// Accumulator for an [`InspectorView`].
///
/// Insertion order is preserved for events (so the producer chooses
/// most-recent-first or oldest-first); agents are sorted alphabetically by
/// label at `build` time so the renderer never has to.
///
/// Counters (`spawned_total`, `running_count`) default to derived values
/// (`agents.len()` and number of running-state rows) but can be overridden
/// via [`InspectorBuilder::spawned_total`] and
/// [`InspectorBuilder::running_count`] when the producer knows the global
/// truth (e.g. the supervisor has stopped agents that aren't in the snapshot).
#[derive(Debug, Default)]
pub struct InspectorBuilder {
    agents: Vec<AgentRow>,
    events: Vec<EventRow>,
    denials: Vec<DenialEntry>,
    peers: Vec<PeerRow>,
    spawned_total_override: Option<u32>,
    running_count_override: Option<u32>,
    local_node_id: String,
}

impl InspectorBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append an agent row. Insertion order is not preserved — rows are
    /// sorted alphabetically by `agent_ref.label` at [`Self::build`] time.
    pub fn with_agent(mut self, row: AgentRow) -> Self {
        self.agents.push(row);
        self
    }

    /// Append an event row. Insertion order **is** preserved; producers
    /// should push most-recent-first if they want the renderer to display
    /// newest-on-top. When more than [`MAX_RECENT_EVENTS`] are pushed the
    /// **oldest** (front of the vec) are dropped at build time.
    pub fn with_event(mut self, row: EventRow) -> Self {
        self.events.push(row);
        self
    }

    /// Append a denial entry. Insertion order **is** preserved; producers
    /// should push most-recent-first if they want the renderer to display
    /// newest-on-top. When more than [`MAX_RECENT_DENIALS`] are pushed the
    /// **oldest** (front of the vec) are dropped at build time.
    pub fn with_denial(mut self, entry: DenialEntry) -> Self {
        self.denials.push(entry);
        self
    }

    /// Append a peer row. Insertion order is preserved.
    pub fn with_peer(mut self, row: PeerRow) -> Self {
        self.peers.push(row);
        self
    }

    /// Set the local node identity for display in the Peers tab.
    pub fn with_local_node_id(mut self, id: String) -> Self {
        self.local_node_id = id;
        self
    }

    /// Override the derived `spawned_total` counter. Use when the producer
    /// has counted agents that aren't (or no longer are) in the snapshot.
    pub fn spawned_total(mut self, n: u32) -> Self {
        self.spawned_total_override = Some(n);
        self
    }

    /// Override the derived `running_count` counter. Same rationale as
    /// [`Self::spawned_total`].
    pub fn running_count(mut self, n: u32) -> Self {
        self.running_count_override = Some(n);
        self
    }

    pub fn build(mut self) -> InspectorView {
        // Sort agents alphabetically by label for stable rendering. We use
        // `sort_by` (not `sort_by_key`) to avoid cloning the label.
        self.agents
            .sort_by(|a, b| a.agent_ref.label.cmp(&b.agent_ref.label));

        // Cap recent_events at MAX_RECENT_EVENTS by dropping the oldest. The
        // builder accepts events in insertion order; "oldest" is the front
        // of the vec.
        if self.events.len() > MAX_RECENT_EVENTS {
            let drop = self.events.len() - MAX_RECENT_EVENTS;
            self.events.drain(0..drop);
        }

        // Same cap policy for denials: front-drop so the most recent (back)
        // are kept.
        if self.denials.len() > MAX_RECENT_DENIALS {
            let drop = self.denials.len() - MAX_RECENT_DENIALS;
            self.denials.drain(0..drop);
        }

        let spawned_total = self
            .spawned_total_override
            .unwrap_or(self.agents.len() as u32);
        let running_count = self.running_count_override.unwrap_or_else(|| {
            self.agents
                .iter()
                .filter(|r| is_running_status(&r.status))
                .count() as u32
        });

        InspectorView {
            agents: self.agents,
            recent_events: self.events,
            spawned_total,
            running_count,
            denials: self.denials,
            peers: self.peers,
            local_node_id: self.local_node_id,
        }
    }
}

// ---------------------------------------------------------------------------
// summarize_event
// ---------------------------------------------------------------------------

/// Turn a raw `(kind, payload)` pair into a one-line human summary.
///
/// Pure and total: every kind returns *something*. Unknown kinds fall back
/// to `"<kind> {<compact-json>}"` so the renderer can show the user *that*
/// an event happened even if the inspector doesn't have a custom summary.
///
/// The set of kinds with custom summaries:
///
/// | kind             | summary                                             |
/// |------------------|-----------------------------------------------------|
/// | `agent.spawn`    | `Spawned <role> '<label>' (id=<id>)`                |
/// | `agent.stop`     | `Stopped '<label>' (id=<id>)`                       |
/// | `agent.failed`   | `Failed '<label>' (id=<id>): <reason>`              |
/// | `agent.restart`  | `Restarted '<label>' (id=<id>) [#<count>]`          |
/// | `tool.invoked`   | `Tool: <name> (<class>)`                            |
/// | `tool.result`    | `Tool result: <name> (<status>)`                    |
/// | `speech.emit`    | `<label>: <excerpt>`                                |
/// | `user.input`     | `User: <excerpt>`                                   |
/// | `consent.asked`  | `Consent: <action>`                                 |
/// | `consent.granted`| `Granted: <action>`                                 |
/// | `consent.denied` | `Denied: <action>`                                  |
///
/// Missing fields in the payload are rendered as `"?"`. The function never
/// panics, never allocates an unbounded string, and never reaches into the
/// network or filesystem.
pub fn summarize_event(kind: &str, payload: &Value) -> String {
    match kind {
        "agent.spawn" => {
            let role = str_field(payload, "role").unwrap_or("?");
            let label = str_field(payload, "label").unwrap_or("?");
            let id = num_field(payload, "id");
            format!("Spawned {role} '{label}' (id={id})")
        }
        "agent.stop" => {
            let label = str_field(payload, "label").unwrap_or("?");
            let id = num_field(payload, "id");
            format!("Stopped '{label}' (id={id})")
        }
        "agent.failed" => {
            let label = str_field(payload, "label").unwrap_or("?");
            let id = num_field(payload, "id");
            let reason = str_field(payload, "reason").unwrap_or("unknown");
            format!("Failed '{label}' (id={id}): {reason}")
        }
        "agent.restart" => {
            let label = str_field(payload, "label").unwrap_or("?");
            let id = num_field(payload, "id");
            let count = num_field(payload, "count");
            format!("Restarted '{label}' (id={id}) [#{count}]")
        }
        "tool.invoked" => {
            let name = str_field(payload, "name").unwrap_or("?");
            let class = str_field(payload, "class").unwrap_or("?");
            format!("Tool: {name} ({class})")
        }
        "tool.result" => {
            let name = str_field(payload, "name").unwrap_or("?");
            let status = str_field(payload, "status").unwrap_or("?");
            format!("Tool result: {name} ({status})")
        }
        "speech.emit" => {
            let label = str_field(payload, "label").unwrap_or("?");
            let body = str_field(payload, "body").unwrap_or("");
            format!("{label}: {}", excerpt(body))
        }
        "user.input" => {
            let body = str_field(payload, "body").unwrap_or("");
            format!("User: {}", excerpt(body))
        }
        "consent.asked" => {
            let action = str_field(payload, "action").unwrap_or("?");
            format!("Consent: {action}")
        }
        "consent.granted" => {
            let action = str_field(payload, "action").unwrap_or("?");
            format!("Granted: {action}")
        }
        "consent.denied" => {
            let action = str_field(payload, "action").unwrap_or("?");
            format!("Denied: {action}")
        }
        _ => format!("{kind} {payload}"),
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn excerpt(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.chars().count() <= SPEECH_EXCERPT_CHARS {
        return trimmed.to_string();
    }
    trimmed.chars().take(SPEECH_EXCERPT_CHARS).collect()
}

fn str_field<'a>(payload: &'a Value, key: &str) -> Option<&'a str> {
    payload.get(key).and_then(|v| v.as_str())
}

/// Coerce a numeric-ish payload field into a string for display. Accepts
/// integers (preferred), strings (in case the producer stringified the id),
/// and falls back to `"?"`.
fn num_field(payload: &Value, key: &str) -> String {
    match payload.get(key) {
        Some(v) if v.is_u64() => v.as_u64().unwrap().to_string(),
        Some(v) if v.is_i64() => v.as_i64().unwrap().to_string(),
        Some(v) if v.is_string() => v.as_str().unwrap().to_string(),
        _ => "?".to_string(),
    }
}

fn spawn_source_label(src: SpawnSource) -> String {
    match src {
        SpawnSource::Substrate => "substrate".to_string(),
        SpawnSource::User => "user".to_string(),
        SpawnSource::Agent(parent) => format!("agent:{parent}"),
    }
}

fn is_running_status(status: &str) -> bool {
    matches!(status, "Spawning" | "Idle" | "Working" | "EmittingSpeech")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::role::{AgentId, AgentRole};
    use serde_json::json;

    fn agent_ref(id: AgentId, role: AgentRole, label: &str, spawned_ms: u64) -> AgentRef {
        AgentRef {
            id,
            role,
            label: label.to_string(),
            spawned_at_unix_ms: spawned_ms,
            spawned_by: SpawnSource::Substrate,
        }
    }

    // ---- summarize_event --------------------------------------------------

    #[test]
    fn summarize_agent_spawn_formats_role_label_and_id() {
        let payload = json!({ "role": "Watcher", "label": "contradiction-finder", "id": 42 });
        let s = summarize_event("agent.spawn", &payload);
        assert_eq!(s, "Spawned Watcher 'contradiction-finder' (id=42)");
    }

    #[test]
    fn summarize_agent_spawn_handles_missing_fields_without_panic() {
        let s = summarize_event("agent.spawn", &json!({}));
        // Missing fields render as "?" — never empty, never panic.
        assert_eq!(s, "Spawned ? '?' (id=?)");
    }

    #[test]
    fn summarize_tool_invoked_formats_name_and_class() {
        let payload = json!({ "name": "phantom.screenshot", "class": "Sense" });
        let s = summarize_event("tool.invoked", &payload);
        assert_eq!(s, "Tool: phantom.screenshot (Sense)");
    }

    #[test]
    fn summarize_tool_result_formats_name_and_status() {
        let payload = json!({ "name": "read_file", "status": "ok" });
        let s = summarize_event("tool.result", &payload);
        assert_eq!(s, "Tool result: read_file (ok)");
    }

    #[test]
    fn summarize_unknown_kind_falls_back_to_kind_plus_payload() {
        // Unknown kinds must never panic and must include the kind so the
        // renderer can show *something* useful.
        let s = summarize_event("totally.unknown", &json!({}));
        assert_eq!(s, "totally.unknown {}");

        let s2 = summarize_event("totally.unknown", &json!({ "k": "v" }));
        assert!(s2.starts_with("totally.unknown "));
        assert!(s2.contains("\"k\""));
        assert!(s2.contains("\"v\""));
    }

    #[test]
    fn summarize_speech_emit_formats_label_colon_excerpt() {
        let payload = json!({ "label": "scout", "body": "  hello world  " });
        assert_eq!(summarize_event("speech.emit", &payload), "scout: hello world");
    }

    #[test]
    fn summarize_speech_emit_truncates_long_bodies() {
        let long = "x".repeat(500);
        let payload = json!({ "label": "scout", "body": long });
        let s = summarize_event("speech.emit", &payload);
        // "scout: " (7) + 80 truncated chars
        assert_eq!(s.chars().count(), 7 + SPEECH_EXCERPT_CHARS);
    }

    #[test]
    fn summarize_agent_failed_includes_reason() {
        let payload = json!({ "label": "fixer", "id": 7, "reason": "panic in tool" });
        let s = summarize_event("agent.failed", &payload);
        assert_eq!(s, "Failed 'fixer' (id=7): panic in tool");
    }

    #[test]
    fn summarize_consent_kinds_format_action() {
        assert_eq!(
            summarize_event("consent.asked", &json!({ "action": "write file" })),
            "Consent: write file",
        );
        assert_eq!(
            summarize_event("consent.granted", &json!({ "action": "write file" })),
            "Granted: write file",
        );
        assert_eq!(
            summarize_event("consent.denied", &json!({ "action": "write file" })),
            "Denied: write file",
        );
    }

    #[test]
    fn summarize_event_handles_string_id_field() {
        // Defensive: producers sometimes stringify ids. We accept either.
        let payload = json!({ "role": "Actor", "label": "a", "id": "99" });
        assert_eq!(summarize_event("agent.spawn", &payload), "Spawned Actor 'a' (id=99)");
    }

    // ---- AgentRow ---------------------------------------------------------

    #[test]
    fn agent_row_spawned_minutes_ago_uses_injected_now() {
        // 5 minutes = 300_000 ms.
        let r = agent_ref(1, AgentRole::Watcher, "test", 1_000_000);
        let row = AgentRow::new(r, "Idle", None, None, 0, 1_000_000 + 300_000);
        assert_eq!(row.spawned_minutes_ago, 5);
    }

    #[test]
    fn agent_row_spawned_minutes_ago_saturates_when_clock_goes_backward() {
        // now < spawned: should not panic, should produce 0.
        let r = agent_ref(1, AgentRole::Watcher, "test", 2_000_000);
        let row = AgentRow::new(r, "Idle", None, None, 1_000_000, 0);
        assert_eq!(row.spawned_minutes_ago, 0);
    }

    #[test]
    fn agent_row_excerpt_truncates_to_80_chars() {
        let r = agent_ref(1, AgentRole::Watcher, "t", 0);
        let body = "a".repeat(200);
        let row = AgentRow::new(r, "EmittingSpeech", Some(1), Some(&body), 0, 1);
        let excerpt = row.last_speech_excerpt.unwrap();
        assert_eq!(excerpt.chars().count(), SPEECH_EXCERPT_CHARS);
    }

    #[test]
    fn agent_row_serializes_with_flattened_agent_ref_fields() {
        let r = agent_ref(7, AgentRole::Actor, "actor-1", 1000);
        let row = AgentRow::new(r, "Working", Some(2000), Some("hi"), 1, 61_000);
        let v = serde_json::to_value(&row).unwrap();
        assert_eq!(v["id"], 7);
        assert_eq!(v["role"], "Actor");
        assert_eq!(v["label"], "actor-1");
        assert_eq!(v["status"], "Working");
        assert_eq!(v["restart_count"], 1);
        assert_eq!(v["spawned_minutes_ago"], 1);
        assert_eq!(v["last_speech_excerpt"], "hi");
        assert_eq!(v["spawned_by"], "substrate");
    }

    // ---- InspectorBuilder -------------------------------------------------

    #[test]
    fn builder_sorts_agents_alphabetically_by_label() {
        let r_zoo = agent_ref(1, AgentRole::Watcher, "zoo", 0);
        let r_alpha = agent_ref(2, AgentRole::Watcher, "alpha", 0);
        let r_mid = agent_ref(3, AgentRole::Watcher, "mid", 0);

        let view = InspectorBuilder::new()
            .with_agent(AgentRow::new(r_zoo, "Idle", None, None, 0, 0))
            .with_agent(AgentRow::new(r_alpha, "Idle", None, None, 0, 0))
            .with_agent(AgentRow::new(r_mid, "Idle", None, None, 0, 0))
            .build();

        let labels: Vec<&str> = view.agents.iter().map(|r| r.agent_ref.label.as_str()).collect();
        assert_eq!(labels, vec!["alpha", "mid", "zoo"]);
    }

    #[test]
    fn builder_preserves_event_insertion_order() {
        let view = InspectorBuilder::new()
            .with_event(EventRow {
                id: 1,
                ts_ms: 100,
                source_label: "Substrate".into(),
                kind: "x".into(),
                summary: "first".into(),
            })
            .with_event(EventRow {
                id: 2,
                ts_ms: 101,
                source_label: "Substrate".into(),
                kind: "y".into(),
                summary: "second".into(),
            })
            .with_event(EventRow {
                id: 3,
                ts_ms: 102,
                source_label: "Substrate".into(),
                kind: "z".into(),
                summary: "third".into(),
            })
            .build();

        let summaries: Vec<&str> = view.recent_events.iter().map(|e| e.summary.as_str()).collect();
        assert_eq!(summaries, vec!["first", "second", "third"]);
    }

    #[test]
    fn builder_caps_recent_events_at_100_dropping_oldest() {
        let mut b = InspectorBuilder::new();
        for i in 0..150u64 {
            b = b.with_event(EventRow {
                id: i,
                ts_ms: i as i64,
                source_label: "Substrate".into(),
                kind: "x".into(),
                summary: format!("evt-{i}"),
            });
        }
        let view = b.build();
        assert_eq!(view.recent_events.len(), MAX_RECENT_EVENTS);
        // Oldest dropped: the first surviving event should be id=50 (150 - 100).
        assert_eq!(view.recent_events.first().unwrap().id, 50);
        assert_eq!(view.recent_events.last().unwrap().id, 149);
    }

    #[test]
    fn builder_derives_running_count_from_status() {
        let r1 = agent_ref(1, AgentRole::Watcher, "a", 0);
        let r2 = agent_ref(2, AgentRole::Watcher, "b", 0);
        let r3 = agent_ref(3, AgentRole::Watcher, "c", 0);
        let r4 = agent_ref(4, AgentRole::Watcher, "d", 0);

        let view = InspectorBuilder::new()
            .with_agent(AgentRow::new(r1, "Idle", None, None, 0, 0))
            .with_agent(AgentRow::new(r2, "Working", None, None, 0, 0))
            .with_agent(AgentRow::new(r3, "Stopped", None, None, 0, 0))
            .with_agent(AgentRow::new(r4, "Failed", None, None, 0, 0))
            .build();

        assert_eq!(view.spawned_total, 4);
        assert_eq!(view.running_count, 2); // Idle + Working
    }

    #[test]
    fn builder_overrides_take_precedence_over_derived_counters() {
        let r = agent_ref(1, AgentRole::Watcher, "a", 0);
        let view = InspectorBuilder::new()
            .with_agent(AgentRow::new(r, "Idle", None, None, 0, 0))
            .spawned_total(99)
            .running_count(42)
            .build();
        assert_eq!(view.spawned_total, 99);
        assert_eq!(view.running_count, 42);
    }

    // ---- InspectorView ----------------------------------------------------

    #[test]
    fn empty_view_is_zeroed_out() {
        let v = InspectorView::empty();
        assert!(v.agents.is_empty());
        assert!(v.recent_events.is_empty());
        assert_eq!(v.spawned_total, 0);
        assert_eq!(v.running_count, 0);
        assert!(v.denials.is_empty());
    }

    // ---- Sec.3: DenialEntry / builder ------------------------------------

    #[test]
    fn builder_preserves_denial_insertion_order() {
        let view = InspectorBuilder::new()
            .with_denial(DenialEntry {
                role: "Watcher".into(),
                attempted_tool: "a".into(),
                attempted_class: "Sense".into(),
                source_chain: Vec::new(),
                timestamp_ms: 1,
            })
            .with_denial(DenialEntry {
                role: "Actor".into(),
                attempted_tool: "b".into(),
                attempted_class: "Act".into(),
                source_chain: vec![7, 8],
                timestamp_ms: 2,
            })
            .build();
        assert_eq!(view.denials.len(), 2);
        assert_eq!(view.denials[0].role, "Watcher");
        assert_eq!(view.denials[1].role, "Actor");
        assert_eq!(view.denials[1].source_chain, vec![7, 8]);
    }

    #[test]
    fn builder_caps_denials_dropping_oldest() {
        let mut b = InspectorBuilder::new();
        for i in 0..(MAX_RECENT_DENIALS + 5) as u64 {
            b = b.with_denial(DenialEntry {
                role: format!("r-{i}"),
                attempted_tool: "t".into(),
                attempted_class: "Sense".into(),
                source_chain: Vec::new(),
                timestamp_ms: i,
            });
        }
        let view = b.build();
        assert_eq!(view.denials.len(), MAX_RECENT_DENIALS);
        // Oldest dropped: first 5 (roles "r-0".."r-4") gone, "r-5" survives.
        assert_eq!(view.denials.first().unwrap().role, "r-5");
    }

    #[test]
    fn denial_entry_serializes_with_expected_fields() {
        let entry = DenialEntry {
            role: "Watcher".into(),
            attempted_tool: "run_command".into(),
            attempted_class: "Act".into(),
            source_chain: vec![123, 456],
            timestamp_ms: 999,
        };
        let v = serde_json::to_value(&entry).expect("denial serializes");
        assert_eq!(v["role"], "Watcher");
        assert_eq!(v["attempted_tool"], "run_command");
        assert_eq!(v["attempted_class"], "Act");
        assert_eq!(v["source_chain"][0], 123);
        assert_eq!(v["source_chain"][1], 456);
        assert_eq!(v["timestamp_ms"], 999);
    }

    #[test]
    fn view_serializes_round_trip() {
        // Snapshot must serialize cleanly — it travels over sockets and into
        // debug logs. We don't pin the wire shape (that's the renderer's
        // contract) but we do require it doesn't error.
        let r = agent_ref(1, AgentRole::Watcher, "a", 0);
        let view = InspectorBuilder::new()
            .with_agent(AgentRow::new(r, "Idle", None, None, 0, 0))
            .with_event(EventRow {
                id: 1,
                ts_ms: 100,
                source_label: "User".into(),
                kind: "user.input".into(),
                summary: "User: hi".into(),
            })
            .build();
        let s = serde_json::to_string(&view).expect("view serializes");
        assert!(s.contains("\"agents\""));
        assert!(s.contains("\"recent_events\""));
        assert!(s.contains("\"spawned_total\""));
        assert!(s.contains("\"running_count\""));
    }
}
