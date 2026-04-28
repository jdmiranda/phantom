//! Inter-agent chat tools.
//!
//! Three tools that let one running agent talk to another by label, read
//! what another agent has said, or broadcast to every agent of a given role.
//!
//! Distinct from [`crate::chat`], which is the LLM-backend abstraction
//! ([`crate::chat::ChatBackend`], [`crate::chat::ChatModel`]). This module is
//! purely about peer-to-peer agent messaging via the substrate's
//! [`AgentRegistry`] and [`EventLog`].
//!
//! ## Tools
//!
//! - [`send_to_agent`] — Sense+Reflect class. Looks up the recipient by label
//!   in the [`AgentRegistry`], delivers an [`InboxMessage::AgentSpeak`] to
//!   their inbox, and (best-effort) appends an `agent.speak` envelope to the
//!   shared [`EventLog`] so peers and the inspector can see who said what.
//! - [`read_from_agent`] — Sense class. Filters the event log for
//!   `agent.speak` envelopes whose `from.label == label && id > since_id` and
//!   returns the most recent N (capped at 20) as JSON.
//! - [`broadcast_to_role`] — Coordinate class. Resolves the role string to
//!   an [`AgentRole`], calls [`AgentRegistry::broadcast_role`], and returns
//!   the recipient count. Conversational and Composer roles are the only
//!   ones that get this — Watcher and Capturer cannot mass-broadcast.
//!
//! All three operate via the [`ChatToolContext`] which carries the calling
//! agent's [`AgentRef`], an `Arc<Mutex<AgentRegistry>>`, and (optionally)
//! an `Arc<Mutex<EventLog>>` so the test/legacy paths that don't have a log
//! still work — log emission is best-effort, never load-bearing.

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::inbox::{AgentRegistry, InboxMessage};
use crate::role::{AgentRef, AgentRole, CapabilityClass};
use crate::speech::{SpeakBody, SpeakEvent, SpeakTarget};
use phantom_memory::event_log::{EventEnvelope, EventLog, EventSource as LogEventSource};

// ---------------------------------------------------------------------------
// ChatTool catalog
// ---------------------------------------------------------------------------

/// The three inter-agent chat tool ids.
///
/// Mirrors [`crate::tools::ToolType`] and [`crate::composer_tools::ComposerTool`]:
/// each variant carries the [`CapabilityClass`] it requires, so the
/// role-aware tool dispatcher can default-deny calls from agents whose
/// manifest lacks that class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ChatTool {
    /// Send a body to one specific agent by label.
    SendToAgent,
    /// Read the most recent things some other agent has said.
    ReadFromAgent,
    /// Broadcast a body to every agent of a given role.
    BroadcastToRole,
}

impl ChatTool {
    /// Wire name used in tool definitions and JSON dispatch.
    #[must_use]
    pub fn api_name(&self) -> &'static str {
        match self {
            Self::SendToAgent => "send_to_agent",
            Self::ReadFromAgent => "read_from_agent",
            Self::BroadcastToRole => "broadcast_to_role",
        }
    }

    /// Parse from a wire name. Returns `None` for unknown ids.
    #[must_use]
    pub fn from_api_name(name: &str) -> Option<Self> {
        match name {
            "send_to_agent" => Some(Self::SendToAgent),
            "read_from_agent" => Some(Self::ReadFromAgent),
            "broadcast_to_role" => Some(Self::BroadcastToRole),
            _ => None,
        }
    }

    /// The capability class the calling role must declare to invoke this
    /// tool.
    ///
    /// `send_to_agent` and `read_from_agent` are tagged Sense — every role
    /// with peer-observation capability gets them. `broadcast_to_role` is
    /// Coordinate, restricted to Conversational and Composer so Watcher /
    /// Capturer / Actor cannot fan out unbounded peer traffic.
    #[must_use]
    pub fn class(&self) -> CapabilityClass {
        match self {
            Self::SendToAgent => CapabilityClass::Sense,
            Self::ReadFromAgent => CapabilityClass::Sense,
            Self::BroadcastToRole => CapabilityClass::Coordinate,
        }
    }
}

// ---------------------------------------------------------------------------
// ChatToolContext
// ---------------------------------------------------------------------------

/// Context handed to every [`ChatTool`] handler.
///
/// Carries the substrate references the handler needs to do its job:
///
/// - `self_ref` is the calling agent's [`AgentRef`], stamped on every
///   outgoing message and event-log entry so attribution is preserved.
/// - `registry` is the live agent directory used for label lookup and
///   broadcast.
/// - `event_log` is the shared append-only log; the `agent.speak` envelope
///   is appended best-effort so peers and the inspector can see traffic.
///   `None` means "skip log emission" — useful for tests that don't open a
///   log file and for the legacy CLI paths that haven't been wired yet.
#[derive(Clone)]
pub struct ChatToolContext {
    pub self_ref: AgentRef,
    pub registry: Arc<Mutex<AgentRegistry>>,
    pub event_log: Option<Arc<Mutex<EventLog>>>,
}

impl ChatToolContext {
    /// Construct a context.
    #[must_use]
    pub fn new(
        self_ref: AgentRef,
        registry: Arc<Mutex<AgentRegistry>>,
        event_log: Option<Arc<Mutex<EventLog>>>,
    ) -> Self {
        Self {
            self_ref,
            registry,
            event_log,
        }
    }
}

// ---------------------------------------------------------------------------
// Argument decoders
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct SendArgs {
    label: String,
    body: String,
}

#[derive(Debug, Deserialize)]
struct ReadArgs {
    label: String,
    #[serde(default)]
    since_event_id: u64,
}

#[derive(Debug, Deserialize)]
struct BroadcastArgs {
    role: String,
    body: String,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The kind name we use for `agent.speak` envelopes in the [`EventLog`].
///
/// Pinning this as a constant keeps producers ([`send_to_agent`]) and
/// consumers ([`read_from_agent`]) in lockstep. Tests assert on this exact
/// string so a typo in either direction trips the suite.
pub const AGENT_SPEAK_EVENT_KIND: &str = "agent.speak";

/// Maximum number of envelopes [`read_from_agent`] returns in one call,
/// regardless of what the caller asks for.
pub const READ_FROM_AGENT_CAP: usize = 20;

/// Encode a [`SpeakEvent`] as a JSON payload suitable for the [`EventLog`].
///
/// Shape:
/// ```json
/// {
///   "from": { "id": ..., "role": "Watcher", "label": "..." },
///   "to":   { "kind": "agent", "id": ... } | { "kind": "broadcast" } | ...,
///   "body": { "kind": "text", "text": "..." } | { "kind": "ping" } | ...,
///   "importance": 0.5,
///   "at_unix_ms": ...
/// }
/// ```
///
/// [`read_from_agent`] reads `payload.from.label` to filter, so the
/// `from.label` field must always be present and a string.
pub fn encode_speak_event(ev: &SpeakEvent) -> serde_json::Value {
    let body = match &ev.body {
        SpeakBody::Text(s) => serde_json::json!({"kind": "text", "text": s}),
        SpeakBody::StatusPing => serde_json::json!({"kind": "ping"}),
        SpeakBody::Notification { kind, ref_uri } => serde_json::json!({
            "kind": "notification",
            "name": kind,
            "ref_uri": ref_uri,
        }),
        SpeakBody::Stream { stream_id } => serde_json::json!({
            "kind": "stream",
            "stream_id": stream_id,
        }),
    };
    let to = match &ev.to {
        SpeakTarget::User => serde_json::json!({"kind": "user"}),
        SpeakTarget::Agent(id) => serde_json::json!({"kind": "agent", "id": id}),
        SpeakTarget::Broadcast => serde_json::json!({"kind": "broadcast"}),
    };
    serde_json::json!({
        "from": {
            "id": ev.from.id,
            "role": ev.from.role.label(),
            "label": ev.from.label,
        },
        "to": to,
        "body": body,
        "importance": ev.importance,
        "at_unix_ms": ev.at_unix_ms,
    })
}

/// Map a free-form role string (case-insensitive) to an [`AgentRole`].
fn parse_role(name: &str) -> Option<AgentRole> {
    match name.to_ascii_lowercase().as_str() {
        "conversational" | "chat" => Some(AgentRole::Conversational),
        "watcher" | "watch" => Some(AgentRole::Watcher),
        "capturer" | "capture" => Some(AgentRole::Capturer),
        "transcriber" => Some(AgentRole::Transcriber),
        "reflector" => Some(AgentRole::Reflector),
        "indexer" => Some(AgentRole::Indexer),
        "actor" => Some(AgentRole::Actor),
        "composer" => Some(AgentRole::Composer),
        "fixer" => Some(AgentRole::Fixer),
        _ => None,
    }
}

/// Best-effort append of a [`SpeakEvent`] to the shared event log.
///
/// Any failure (poisoned mutex, I/O error) is swallowed — chat traffic must
/// not stall on log persistence. Returns `Some(envelope.id)` on success so
/// callers (and tests) can correlate the produced id back to a follow-up
/// `read_from_agent`.
fn append_speak_to_log(
    log: &Option<Arc<Mutex<EventLog>>>,
    source_id: u64,
    ev: &SpeakEvent,
) -> Option<u64> {
    let log = log.as_ref()?;
    let mut g = log.lock().ok()?;
    let payload = encode_speak_event(ev);
    g.append(LogEventSource::Agent { id: source_id }, AGENT_SPEAK_EVENT_KIND, payload)
        .ok()
        .map(|env| env.id)
}

// ---------------------------------------------------------------------------
// send_to_agent
// ---------------------------------------------------------------------------

/// Send a text body to a specific agent by label.
///
/// Looks up the target via [`AgentRegistry::find_by_label`]. If the label
/// resolves to a registered agent, delivers an [`InboxMessage::AgentSpeak`]
/// to that agent's inbox and emits a [`SpeakEvent`] (importance 0.5 so the
/// inspector / UI may surface it) into the event log when one is configured.
///
/// Returns:
/// - `Ok("delivered to <label>")` on successful inbox delivery.
/// - `Err("agent label not found: <label>")` when the label is unknown.
/// - `Err("agent inbox closed: <label>")` when the recipient's channel was
///   dropped (agent crashed) or is full.
pub fn send_to_agent(
    args: &serde_json::Value,
    ctx: &ChatToolContext,
) -> Result<String, String> {
    let parsed: SendArgs = serde_json::from_value(args.clone())
        .map_err(|e| format!("invalid send_to_agent args: {e}"))?;

    let registry = ctx
        .registry
        .lock()
        .map_err(|_| "agent registry poisoned".to_string())?;

    let handle = registry
        .find_by_label(&parsed.label)
        .ok_or_else(|| format!("agent label not found: {}", parsed.label))?;

    let target_id = handle.agent_ref.id;
    let inbox = handle.inbox.clone();
    drop(registry);

    inbox
        .try_send(InboxMessage::AgentSpeak {
            from: ctx.self_ref.clone(),
            body: parsed.body.clone(),
        })
        .map_err(|_| format!("agent inbox closed: {}", parsed.label))?;

    let ev = SpeakEvent {
        from: ctx.self_ref.clone(),
        to: SpeakTarget::Agent(target_id),
        body: SpeakBody::Text(parsed.body),
        importance: 0.5,
        at_unix_ms: now_unix_ms(),
    };
    let _ = append_speak_to_log(&ctx.event_log, ctx.self_ref.id, &ev);

    Ok(format!("delivered to {}", parsed.label))
}

// ---------------------------------------------------------------------------
// read_from_agent
// ---------------------------------------------------------------------------

/// Read recent `agent.speak` envelopes attributed to `args.label`, capped at
/// [`READ_FROM_AGENT_CAP`].
///
/// Filtering predicates:
/// - envelope `kind == "agent.speak"`,
/// - payload `from.label == args.label`,
/// - envelope `id > args.since_event_id`.
///
/// Returns a JSON array (chronological order, oldest first) so the caller
/// can iterate and update its own `since_event_id` cursor against the last
/// element's `id` for the next poll.
///
/// Errors:
/// - `Err("event log not configured")` when the context wasn't given a log
///   (legacy / test path).
/// - `Err("event log poisoned")` if the underlying mutex was poisoned.
pub fn read_from_agent(
    args: &serde_json::Value,
    ctx: &ChatToolContext,
) -> Result<Vec<EventEnvelope>, String> {
    let parsed: ReadArgs = serde_json::from_value(args.clone())
        .map_err(|e| format!("invalid read_from_agent args: {e}"))?;

    let log = ctx
        .event_log
        .as_ref()
        .ok_or_else(|| "event log not configured".to_string())?;

    let g = log.lock().map_err(|_| "event log poisoned".to_string())?;
    // Pull a generous tail so callers reading after the fact don't miss
    // older traffic. The in-memory tail is bounded (~4096 envelopes) so
    // this is a constant-bounded scan.
    let tail = g.tail(4096);
    drop(g);

    let mut out: Vec<EventEnvelope> = tail
        .into_iter()
        .filter(|env| {
            if env.kind != AGENT_SPEAK_EVENT_KIND {
                return false;
            }
            if env.id <= parsed.since_event_id {
                return false;
            }
            let from_label = env
                .payload
                .get("from")
                .and_then(|f| f.get("label"))
                .and_then(|l| l.as_str());
            from_label == Some(parsed.label.as_str())
        })
        .collect();

    if out.len() > READ_FROM_AGENT_CAP {
        let cut = out.len() - READ_FROM_AGENT_CAP;
        out.drain(..cut);
    }

    Ok(out)
}

// ---------------------------------------------------------------------------
// broadcast_to_role
// ---------------------------------------------------------------------------

/// Broadcast a text body to every agent currently registered under a given
/// role.
///
/// Resolves the role string with [`parse_role`] and delegates to
/// [`AgentRegistry::broadcast_role`], which is best-effort: agents whose
/// inbox is full or closed are silently skipped, and the returned count is
/// the number of successful deliveries.
///
/// Returns:
/// - `Ok(usize)` with the number of agents that received the message.
/// - `Err("unknown role: <role>")` for an unrecognized role string.
pub fn broadcast_to_role(
    args: &serde_json::Value,
    ctx: &ChatToolContext,
) -> Result<usize, String> {
    let parsed: BroadcastArgs = serde_json::from_value(args.clone())
        .map_err(|e| format!("invalid broadcast_to_role args: {e}"))?;

    let role = parse_role(&parsed.role)
        .ok_or_else(|| format!("unknown role: {}", parsed.role))?;

    let registry = ctx
        .registry
        .lock()
        .map_err(|_| "agent registry poisoned".to_string())?;

    let delivered = registry.broadcast_role(
        role,
        InboxMessage::AgentSpeak {
            from: ctx.self_ref.clone(),
            body: parsed.body.clone(),
        },
    );
    drop(registry);

    let ev = SpeakEvent {
        from: ctx.self_ref.clone(),
        to: SpeakTarget::Broadcast,
        body: SpeakBody::Text(parsed.body),
        importance: 0.5,
        at_unix_ms: now_unix_ms(),
    };
    let _ = append_speak_to_log(&ctx.event_log, ctx.self_ref.id, &ev);

    Ok(delivered)
}

// ---------------------------------------------------------------------------
// Time helper
// ---------------------------------------------------------------------------

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inbox::{AgentHandle, AgentStatus};
    use crate::role::SpawnSource;
    use serde_json::json;
    use std::time::Duration;
    use tempfile::tempdir;
    use tokio::sync::{mpsc, watch};
    use tokio::time::timeout;

    /// Build a fake registered agent. Returns the handle (for register) plus
    /// the receiver half so tests can verify what was delivered.
    fn fake_agent(
        id: u64,
        role: AgentRole,
        label: &str,
    ) -> (AgentHandle, mpsc::Receiver<InboxMessage>) {
        let (tx, rx) = mpsc::channel(8);
        let (_status_tx, status_rx) = watch::channel(AgentStatus::Idle);
        let handle = AgentHandle {
            agent_ref: AgentRef::new(id, role, label, SpawnSource::Substrate),
            inbox: tx,
            status: status_rx,
        };
        (handle, rx)
    }

    /// Build a `ChatToolContext` whose calling agent is the supplied
    /// (id, role, label) and whose registry contains the supplied handles.
    /// Optionally opens a temp event log.
    fn build_ctx(
        self_id: u64,
        self_role: AgentRole,
        self_label: &str,
        handles: Vec<AgentHandle>,
        with_log: bool,
    ) -> (ChatToolContext, Option<tempfile::TempDir>) {
        let mut reg = AgentRegistry::new();
        for h in handles {
            reg.register(h);
        }
        let registry = Arc::new(Mutex::new(reg));
        let (log, dir) = if with_log {
            let d = tempdir().expect("tempdir");
            let path = d.path().join("events.jsonl");
            let log = EventLog::open(&path).expect("open");
            (Some(Arc::new(Mutex::new(log))), Some(d))
        } else {
            (None, None)
        };
        let self_ref = AgentRef::new(self_id, self_role, self_label, SpawnSource::User);
        let ctx = ChatToolContext::new(self_ref, registry, log);
        (ctx, dir)
    }

    // ---- ChatTool catalog tests --------------------------------------------

    #[test]
    fn chat_tool_api_name_round_trip() {
        for t in [
            ChatTool::SendToAgent,
            ChatTool::ReadFromAgent,
            ChatTool::BroadcastToRole,
        ] {
            let parsed = ChatTool::from_api_name(t.api_name());
            assert_eq!(parsed, Some(t));
        }
    }

    #[test]
    fn chat_tool_unknown_returns_none() {
        assert_eq!(ChatTool::from_api_name("not_a_tool"), None);
    }

    #[test]
    fn chat_tool_class_assignments() {
        assert_eq!(ChatTool::SendToAgent.class(), CapabilityClass::Sense);
        assert_eq!(ChatTool::ReadFromAgent.class(), CapabilityClass::Sense);
        assert_eq!(
            ChatTool::BroadcastToRole.class(),
            CapabilityClass::Coordinate
        );
    }

    // ---- send_to_agent ------------------------------------------------------

    #[tokio::test]
    async fn send_to_agent_delivers_to_target_inbox() {
        let (target_handle, mut target_rx) =
            fake_agent(2, AgentRole::Watcher, "target");
        let (ctx, _dir) = build_ctx(
            1,
            AgentRole::Conversational,
            "speaker",
            vec![target_handle],
            true,
        );

        let result = send_to_agent(
            &json!({"label": "target", "body": "hello target"}),
            &ctx,
        )
        .expect("send should succeed");
        assert_eq!(result, "delivered to target");

        let got = timeout(Duration::from_millis(100), target_rx.recv())
            .await
            .expect("receive timed out")
            .expect("channel closed");
        match got {
            InboxMessage::AgentSpeak { from, body } => {
                assert_eq!(from.id, 1);
                assert_eq!(from.label, "speaker");
                assert_eq!(body, "hello target");
            }
            other => panic!("wrong message: {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_to_agent_unknown_label_returns_err() {
        let (ctx, _dir) = build_ctx(
            1,
            AgentRole::Conversational,
            "speaker",
            Vec::new(),
            true,
        );

        let err = send_to_agent(
            &json!({"label": "nobody", "body": "hi"}),
            &ctx,
        )
        .expect_err("should fail for unknown label");
        assert!(err.contains("nobody"), "error message lost label: {err}");
        assert!(
            err.contains("not found"),
            "error message lost reason: {err}",
        );
    }

    #[tokio::test]
    async fn send_to_agent_appends_speak_envelope_to_log() {
        let (target_handle, _rx) =
            fake_agent(2, AgentRole::Watcher, "target");
        let (ctx, _dir) = build_ctx(
            1,
            AgentRole::Conversational,
            "speaker",
            vec![target_handle],
            true,
        );

        send_to_agent(
            &json!({"label": "target", "body": "logged body"}),
            &ctx,
        )
        .expect("send should succeed");

        // Pull the log tail and find the speak envelope.
        let log = ctx.event_log.clone().expect("log present");
        let g = log.lock().unwrap();
        let tail = g.tail(64);
        drop(g);
        let speak = tail
            .iter()
            .find(|e| e.kind == AGENT_SPEAK_EVENT_KIND)
            .expect("agent.speak envelope appended");
        assert_eq!(
            speak.payload["from"]["label"].as_str(),
            Some("speaker"),
        );
        assert_eq!(
            speak.payload["body"]["text"].as_str(),
            Some("logged body"),
        );
    }

    #[tokio::test]
    async fn send_to_agent_works_without_event_log() {
        // Legacy / test paths that don't have a log open must still deliver
        // inbox messages — log emission is best-effort.
        let (target_handle, mut target_rx) =
            fake_agent(2, AgentRole::Watcher, "target");
        let (ctx, _dir) = build_ctx(
            1,
            AgentRole::Conversational,
            "speaker",
            vec![target_handle],
            false,
        );

        send_to_agent(
            &json!({"label": "target", "body": "no-log"}),
            &ctx,
        )
        .expect("send should succeed without a log");

        let got = timeout(Duration::from_millis(100), target_rx.recv())
            .await
            .expect("receive timed out")
            .expect("channel closed");
        assert!(matches!(got, InboxMessage::AgentSpeak { .. }));
    }

    // ---- read_from_agent ----------------------------------------------------

    #[tokio::test]
    async fn read_from_agent_filters_by_label_and_since_id() {
        // Build two agents: A and B. Send from A → C (logged), then send
        // from B → C (logged). Reading "from agent A since id 0" must
        // return exactly the A envelope.
        let (a_handle, _a_rx) = fake_agent(1, AgentRole::Conversational, "A");
        let (b_handle, _b_rx) = fake_agent(2, AgentRole::Conversational, "B");
        let (c_handle, _c_rx) = fake_agent(3, AgentRole::Watcher, "C");

        // Use a single shared log + registry so the sends from A and B are
        // observable through the same context.
        let mut reg = AgentRegistry::new();
        reg.register(a_handle);
        reg.register(b_handle);
        reg.register(c_handle);
        let registry = Arc::new(Mutex::new(reg));
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("events.jsonl");
        let log = Arc::new(Mutex::new(EventLog::open(&path).expect("open")));

        let a_ref = AgentRef::new(1, AgentRole::Conversational, "A", SpawnSource::User);
        let a_ctx = ChatToolContext::new(a_ref, registry.clone(), Some(log.clone()));
        let b_ref = AgentRef::new(2, AgentRole::Conversational, "B", SpawnSource::User);
        let b_ctx = ChatToolContext::new(b_ref, registry.clone(), Some(log.clone()));

        send_to_agent(&json!({"label": "C", "body": "from A"}), &a_ctx)
            .expect("A send");
        send_to_agent(&json!({"label": "C", "body": "from B"}), &b_ctx)
            .expect("B send");

        // Read C's view: "what has agent A said?".
        let read_ctx = a_ctx.clone();
        let envelopes = read_from_agent(
            &json!({"label": "A", "since_event_id": 0u64}),
            &read_ctx,
        )
        .expect("read should succeed");
        assert_eq!(envelopes.len(), 1, "expected one envelope from A");
        assert_eq!(
            envelopes[0].payload["body"]["text"].as_str(),
            Some("from A"),
        );

        // Now use that envelope's id as `since_event_id` — should return
        // nothing because the only A envelope is at-or-below the cursor.
        let after_id = envelopes[0].id;
        let envelopes2 = read_from_agent(
            &json!({"label": "A", "since_event_id": after_id}),
            &read_ctx,
        )
        .expect("read should succeed");
        assert_eq!(
            envelopes2.len(),
            0,
            "expected zero envelopes after the cursor",
        );

        // And asking for B returns the B envelope, demonstrating the label
        // filter is doing real work.
        let envelopes_b = read_from_agent(
            &json!({"label": "B", "since_event_id": 0u64}),
            &read_ctx,
        )
        .expect("read should succeed");
        assert_eq!(envelopes_b.len(), 1);
        assert_eq!(
            envelopes_b[0].payload["body"]["text"].as_str(),
            Some("from B"),
        );
    }

    #[tokio::test]
    async fn read_from_agent_caps_at_twenty() {
        // Append 25 speak envelopes attributed to label "noisy"; read with
        // since_id 0 must return exactly READ_FROM_AGENT_CAP (20).
        let (noisy_handle, _rx) =
            fake_agent(1, AgentRole::Conversational, "noisy");
        let (ctx, _dir) = build_ctx(
            42,
            AgentRole::Conversational,
            "reader",
            vec![noisy_handle],
            true,
        );

        // Synthesize 25 envelopes via direct log appends (instead of routing
        // 25 inbox messages, which would back up the channel).
        let log = ctx.event_log.clone().unwrap();
        for i in 0..25 {
            let mut g = log.lock().unwrap();
            g.append(
                LogEventSource::Agent { id: 1 },
                AGENT_SPEAK_EVENT_KIND,
                json!({
                    "from": {"id": 1, "role": "Conversational", "label": "noisy"},
                    "to": {"kind": "agent", "id": 99},
                    "body": {"kind": "text", "text": format!("msg {i}")},
                    "importance": 0.5,
                    "at_unix_ms": 0,
                }),
            )
            .unwrap();
        }

        let envelopes = read_from_agent(
            &json!({"label": "noisy", "since_event_id": 0u64}),
            &ctx,
        )
        .expect("read should succeed");
        assert_eq!(
            envelopes.len(),
            READ_FROM_AGENT_CAP,
            "expected cap at {READ_FROM_AGENT_CAP}",
        );
        // The cap keeps the most recent N (drained from the front), so the
        // last returned envelope must carry "msg 24".
        assert_eq!(
            envelopes.last().unwrap().payload["body"]["text"].as_str(),
            Some("msg 24"),
        );
    }

    #[tokio::test]
    async fn read_from_agent_without_log_returns_err() {
        let (ctx, _dir) =
            build_ctx(1, AgentRole::Conversational, "reader", Vec::new(), false);
        let err = read_from_agent(
            &json!({"label": "anyone", "since_event_id": 0u64}),
            &ctx,
        )
        .expect_err("should fail without a log");
        assert!(
            err.contains("event log not configured"),
            "wrong error: {err}",
        );
    }

    // ---- broadcast_to_role --------------------------------------------------

    #[tokio::test]
    async fn broadcast_to_role_returns_recipient_count() {
        let (w1, mut w1_rx) = fake_agent(1, AgentRole::Watcher, "w1");
        let (w2, mut w2_rx) = fake_agent(2, AgentRole::Watcher, "w2");
        let (a1, mut a1_rx) = fake_agent(3, AgentRole::Actor, "a1");
        let (ctx, _dir) = build_ctx(
            10,
            AgentRole::Composer,
            "boss",
            vec![w1, w2, a1],
            true,
        );

        let count = broadcast_to_role(
            &json!({"role": "watcher", "body": "all watch out"}),
            &ctx,
        )
        .expect("broadcast should succeed");
        assert_eq!(count, 2, "should reach exactly the two watchers");

        for rx in [&mut w1_rx, &mut w2_rx] {
            let got = timeout(Duration::from_millis(100), rx.recv())
                .await
                .expect("watcher receive timed out")
                .expect("watcher channel closed");
            match got {
                InboxMessage::AgentSpeak { from, body } => {
                    assert_eq!(from.label, "boss");
                    assert_eq!(body, "all watch out");
                }
                other => panic!("wrong message: {other:?}"),
            }
        }
        assert!(
            a1_rx.try_recv().is_err(),
            "actor must not have received the watcher broadcast",
        );
    }

    #[tokio::test]
    async fn broadcast_to_role_unknown_role_returns_err() {
        let (ctx, _dir) =
            build_ctx(1, AgentRole::Composer, "boss", Vec::new(), true);
        let err = broadcast_to_role(
            &json!({"role": "warlock", "body": "x"}),
            &ctx,
        )
        .expect_err("should fail for unknown role");
        assert!(err.contains("warlock"), "error lost role: {err}");
    }

    #[tokio::test]
    async fn broadcast_to_role_with_no_matches_returns_zero() {
        let (a1, _rx) = fake_agent(1, AgentRole::Actor, "a1");
        let (ctx, _dir) =
            build_ctx(10, AgentRole::Composer, "boss", vec![a1], true);

        let count = broadcast_to_role(
            &json!({"role": "watcher", "body": "hello"}),
            &ctx,
        )
        .expect("broadcast succeeds even with no matches");
        assert_eq!(count, 0);
    }

    // ---- Encoder tests ------------------------------------------------------

    #[test]
    fn encode_speak_event_text_to_agent_shape() {
        // Round-trips important fields: from.label, to.kind, body.text.
        let from = AgentRef::new(7, AgentRole::Reflector, "ref", SpawnSource::User);
        let ev = SpeakEvent::text_to_agent(from.clone(), 99, "hi 99");
        let payload = encode_speak_event(&ev);
        assert_eq!(payload["from"]["label"].as_str(), Some("ref"));
        assert_eq!(payload["from"]["id"].as_u64(), Some(7));
        assert_eq!(payload["to"]["kind"].as_str(), Some("agent"));
        assert_eq!(payload["to"]["id"].as_u64(), Some(99));
        assert_eq!(payload["body"]["kind"].as_str(), Some("text"));
        assert_eq!(payload["body"]["text"].as_str(), Some("hi 99"));
    }
}
