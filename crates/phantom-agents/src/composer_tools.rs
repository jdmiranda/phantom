//! Composer collaboration toolkit.
//!
//! Four tools the [`AgentRole::Composer`] reaches for when planning and
//! delegating multi-agent work:
//!
//! - [`spawn_subagent`] — Coordinate-class. Asks the host App to spawn a
//!   typed sub-agent on the next frame. The actual spawn dispatches through
//!   the App-owned [`SpawnSubagentQueue`] because tool handlers do not own
//!   the runtime / coordinator. Returns a freshly minted agent id.
//! - [`wait_for_agent`] — Sense-class. Polls an [`EventLog`] for an
//!   `agent.complete.<id>` or `agent.failed.<id>` envelope, up to a deadline.
//! - [`request_critique`] — Compute-class. Posts a templated critique
//!   request into the target agent's [`InboxMessage::AgentSpeak`] inbox and
//!   waits up to 60 s for the corresponding completion envelope.
//! - [`event_log_query`] — Sense-class. Filters the event log's in-memory
//!   tail by agent_id / kind / since_id, returning up to `limit` envelopes.
//!
//! Every tool is gated by its [`CapabilityClass`] via [`ComposerTool::class`]
//! so the role-aware tool dispatcher can default-deny calls from agents
//! whose manifest lacks the class. That mirrors the existing pattern in
//! [`crate::tools`] but uses the substrate's stricter `CapabilityClass` axis
//! (Sense / Reflect / Compute / Act / Coordinate) instead of the per-tool
//! permission list used by [`crate::tools::ToolType`].
//!
//! ## Threading
//!
//! - [`SpawnSubagentQueue`] is `Arc<Mutex<VecDeque<…>>>`. Tool handlers push;
//!   the App drains once per frame in `update.rs`.
//! - [`wait_for_agent`] uses a polling loop with a 50 ms cadence so it works
//!   in both sync and async tool dispatchers without a tokio context.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::chat::ChatModel;
use crate::inbox::{AgentRegistry, InboxMessage};
use crate::role::{AgentId, AgentRef, AgentRole, SpawnSource};
use phantom_memory::event_log::{EventEnvelope, EventLog};

// ---------------------------------------------------------------------------
// Capability class re-export
// ---------------------------------------------------------------------------

pub use crate::role::CapabilityClass;

// ---------------------------------------------------------------------------
// Composer tool catalogue
// ---------------------------------------------------------------------------

/// The four tool ids the Composer agent invokes during a debate / delegation
/// turn. Every variant carries the [`CapabilityClass`] it requires; the
/// dispatcher intersects that with the calling agent's role manifest before
/// running the handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ComposerTool {
    SpawnSubagent,
    WaitForAgent,
    RequestCritique,
    EventLogQuery,
}

impl ComposerTool {
    /// API/wire name (matches the model-side schema below).
    #[must_use]
    pub fn api_name(&self) -> &'static str {
        match self {
            Self::SpawnSubagent => "spawn_subagent",
            Self::WaitForAgent => "wait_for_agent",
            Self::RequestCritique => "request_critique",
            Self::EventLogQuery => "event_log_query",
        }
    }

    /// The capability class the calling role must declare to invoke this
    /// tool. The dispatcher must default-deny when the role manifest
    /// (see [`AgentRole::manifest`]) does not list this class.
    #[must_use]
    pub fn class(&self) -> CapabilityClass {
        match self {
            Self::SpawnSubagent => CapabilityClass::Coordinate,
            Self::WaitForAgent => CapabilityClass::Sense,
            Self::RequestCritique => CapabilityClass::Compute,
            Self::EventLogQuery => CapabilityClass::Sense,
        }
    }

    /// Parse from a wire name. Returns `None` for unknown ids.
    #[must_use]
    pub fn from_api_name(name: &str) -> Option<Self> {
        match name {
            "spawn_subagent" => Some(Self::SpawnSubagent),
            "wait_for_agent" => Some(Self::WaitForAgent),
            "request_critique" => Some(Self::RequestCritique),
            "event_log_query" => Some(Self::EventLogQuery),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// spawn_subagent — Coordinate
// ---------------------------------------------------------------------------

/// One queued spawn request, waiting for the App's draining hook to honor it.
///
/// The Composer cannot spawn agents itself because tool handlers don't own
/// the App's coordinator / scene tree / supervisor. Instead a tool call
/// pushes one of these onto [`SpawnSubagentQueue`]; the next `update.rs`
/// frame drains the queue, calls `App::spawn_agent_pane_with_opts`, and the
/// spawned agent's id is published back so callers can address it.
#[derive(Debug, Clone)]
pub struct SpawnSubagentRequest {
    /// Pre-allocated id for the spawned agent. `wait_for_agent` and
    /// `send_to_agent` (queued in `chat_tools.rs`) use this id to address
    /// the new sub-agent before the App finishes registering it.
    pub assigned_id: AgentId,
    /// Role to spawn under. Static-at-spawn, no escalation.
    pub role: AgentRole,
    /// User-visible label, e.g. `"claude-planner"`. Labels need not be
    /// unique at the registry layer; the Composer is responsible for label
    /// discipline.
    pub label: String,
    /// Initial task description handed to the agent.
    pub task: String,
    /// Optional explicit chat model. `None` = default Claude path.
    pub chat_model: Option<ChatModel>,
    /// Composer that issued the spawn — recorded as `SpawnSource::Agent`.
    pub parent: AgentId,
}

/// Shared queue draining one [`SpawnSubagentRequest`] per pending tool call.
///
/// The App owns the canonical instance; clones are handed to the Composer
/// tool dispatcher at agent-spawn time so handlers can push without holding
/// a reference back to the App.
pub type SpawnSubagentQueue = Arc<Mutex<VecDeque<SpawnSubagentRequest>>>;

/// Construct an empty queue.
#[must_use]
pub fn new_spawn_subagent_queue() -> SpawnSubagentQueue {
    Arc::new(Mutex::new(VecDeque::new()))
}

/// Monotonic id allocator. Composer-side only — production code should
/// share the same allocator with the App so ids never collide.
static NEXT_SUBAGENT_ID: AtomicU64 = AtomicU64::new(10_000);

/// Allocate the next composer-side sub-agent id.
#[must_use]
pub fn allocate_subagent_id() -> AgentId {
    NEXT_SUBAGENT_ID.fetch_add(1, Ordering::SeqCst)
}

/// Decode the JSON args struct expected by `spawn_subagent`.
#[derive(Debug, Deserialize)]
struct SpawnArgs {
    role: String,
    label: String,
    task: String,
    #[serde(default)]
    model: Option<String>,
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

/// Map a free-form model string (case-insensitive) to a [`ChatModel`].
/// Returns `None` for unknown ids — the caller decides how strict to be.
fn parse_model(name: &str) -> Option<ChatModel> {
    match name.to_ascii_lowercase().as_str() {
        "claude" | "claude-default" => Some(ChatModel::default_claude()),
        "gpt-4o" | "openai" | "gpt4o" => Some(ChatModel::default_openai()),
        _ => None,
    }
}

/// Run the `spawn_subagent` tool. Returns the freshly assigned agent id.
///
/// The actual spawn happens out-of-band: the queue is drained by the App's
/// per-frame update hook, which calls `App::spawn_agent_pane_with_opts`.
/// Clients can use the returned id with `wait_for_agent` immediately.
pub fn spawn_subagent(
    args: &serde_json::Value,
    parent: AgentId,
    queue: &SpawnSubagentQueue,
) -> Result<AgentId, String> {
    let parsed: SpawnArgs = serde_json::from_value(args.clone())
        .map_err(|e| format!("invalid spawn_subagent args: {e}"))?;

    let role = parse_role(&parsed.role)
        .ok_or_else(|| format!("unknown role: {}", parsed.role))?;

    let chat_model = match parsed.model.as_deref() {
        None => None,
        Some("") => None,
        Some(name) => Some(
            parse_model(name).ok_or_else(|| format!("unknown chat model: {name}"))?,
        ),
    };

    let assigned_id = allocate_subagent_id();
    let req = SpawnSubagentRequest {
        assigned_id,
        role,
        label: parsed.label,
        task: parsed.task,
        chat_model,
        parent,
    };

    let mut q = queue.lock().map_err(|_| "spawn queue poisoned".to_string())?;
    q.push_back(req);
    Ok(assigned_id)
}

// ---------------------------------------------------------------------------
// wait_for_agent — Sense
// ---------------------------------------------------------------------------

/// Decode the JSON args struct expected by `wait_for_agent`.
#[derive(Debug, Deserialize)]
struct WaitArgs {
    agent_id: u64,
    max_seconds: u64,
}

/// Build the dotted-name kinds we accept as terminal states for `agent_id`.
fn completion_kinds_for(agent_id: u64) -> [String; 4] {
    [
        format!("agent.complete.{agent_id}"),
        format!("agent.failed.{agent_id}"),
        // Loose-form fallbacks so callers that don't know the dotted suffix
        // (e.g. the Composer prompt's instruction to emit `"agent.complete"`
        // verbatim) still match.
        "agent.complete".to_string(),
        "agent.failed".to_string(),
    ]
}

/// Run the `wait_for_agent` tool against an event log.
///
/// Polls the in-memory tail every 50 ms until either:
/// - an envelope whose `kind` matches one of [`completion_kinds_for`] AND
///   whose payload's `agent_id == args.agent_id` is found, OR
/// - the deadline expires (returns `Err("timeout waiting for agent {id}")`).
pub fn wait_for_agent(
    args: &serde_json::Value,
    log: &Mutex<EventLog>,
) -> Result<EventEnvelope, String> {
    let parsed: WaitArgs = serde_json::from_value(args.clone())
        .map_err(|e| format!("invalid wait_for_agent args: {e}"))?;

    let deadline = Instant::now() + Duration::from_secs(parsed.max_seconds);
    let kinds = completion_kinds_for(parsed.agent_id);
    let poll = Duration::from_millis(50);

    loop {
        // Single-pass scan of the in-memory tail.
        if let Ok(g) = log.lock() {
            // Pull a generous tail so we don't miss old envelopes when the
            // caller polls after the fact.
            let tail = g.tail(4096);
            for ev in tail.into_iter().rev() {
                if !kinds.iter().any(|k| ev.kind == *k) {
                    continue;
                }
                let matches_id = ev
                    .payload
                    .get("agent_id")
                    .and_then(|v| v.as_u64())
                    .map(|id| id == parsed.agent_id)
                    .unwrap_or(true); // kindless fallback when payload omits id
                if matches_id {
                    return Ok(ev);
                }
            }
        }

        if Instant::now() >= deadline {
            return Err(format!("timeout waiting for agent {}", parsed.agent_id));
        }
        std::thread::sleep(poll);
    }
}

// ---------------------------------------------------------------------------
// request_critique — Compute
// ---------------------------------------------------------------------------

/// Decode the JSON args struct expected by `request_critique`.
#[derive(Debug, Deserialize)]
struct CritiqueArgs {
    from_agent_label: String,
    of_message: String,
    context: String,
}

/// Build the verbatim critique-request body sent to the target. The exact
/// template is pinned by tests; do not paraphrase.
#[must_use]
pub fn build_critique_body(of_message: &str, context: &str) -> String {
    format!(
        "Please critique the following message in the context of {context}. \
         Reply with one of: AGREE, DISAGREE (with reason), PARTIAL (with caveat). \
         Message: {of_message}"
    )
}

/// Run the `request_critique` tool: send a templated critique request to
/// `args.from_agent_label`, then wait up to 60 s for that agent to publish
/// `agent.complete.<id>` into the event log.
pub fn request_critique(
    args: &serde_json::Value,
    composer: &AgentRef,
    registry: &AgentRegistry,
    log: &Mutex<EventLog>,
) -> Result<EventEnvelope, String> {
    let parsed: CritiqueArgs = serde_json::from_value(args.clone())
        .map_err(|e| format!("invalid request_critique args: {e}"))?;

    let target = registry
        .find_by_label(&parsed.from_agent_label)
        .ok_or_else(|| format!("unknown agent label: {}", parsed.from_agent_label))?;

    let body = build_critique_body(&parsed.of_message, &parsed.context);
    let target_id = target.agent_ref.id;

    target
        .inbox
        .try_send(InboxMessage::AgentSpeak {
            from: composer.clone(),
            body,
        })
        .map_err(|_| format!("agent {target_id} inbox closed or full"))?;

    // Wait up to 60s for the target to emit a completion envelope.
    let wait_args = serde_json::json!({
        "agent_id": target_id,
        "max_seconds": 60u64,
    });
    wait_for_agent(&wait_args, log)
}

// ---------------------------------------------------------------------------
// event_log_query — Sense
// ---------------------------------------------------------------------------

/// Decode the JSON args struct expected by `event_log_query`.
///
/// All fields optional; an empty filter returns the most recent `limit`
/// envelopes from the in-memory tail.
#[derive(Debug, Deserialize, Default)]
struct QueryArgs {
    #[serde(default)]
    agent_id: Option<u64>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    since_id: Option<u64>,
    #[serde(default)]
    limit: Option<usize>,
}

/// Default page size when `limit` is omitted.
const DEFAULT_QUERY_LIMIT: usize = 20;

/// Run the `event_log_query` tool against an event log.
pub fn event_log_query(
    args: &serde_json::Value,
    log: &Mutex<EventLog>,
) -> Result<Vec<EventEnvelope>, String> {
    let parsed: QueryArgs = serde_json::from_value(args.clone())
        .map_err(|e| format!("invalid event_log_query args: {e}"))?;

    let limit = parsed.limit.unwrap_or(DEFAULT_QUERY_LIMIT).max(1);

    let g = log.lock().map_err(|_| "event log poisoned".to_string())?;
    let tail = g.tail(4096);
    drop(g);

    let mut out: Vec<EventEnvelope> = tail
        .into_iter()
        .filter(|ev| {
            if let Some(min_id) = parsed.since_id
                && ev.id < min_id
            {
                return false;
            }
            if let Some(ref want_kind) = parsed.kind
                && &ev.kind != want_kind
            {
                return false;
            }
            if let Some(want_id) = parsed.agent_id {
                let payload_id = ev
                    .payload
                    .get("agent_id")
                    .and_then(|v| v.as_u64());
                if payload_id != Some(want_id) {
                    return false;
                }
            }
            true
        })
        .collect();

    if out.len() > limit {
        let cut = out.len() - limit;
        out.drain(..cut);
    }

    Ok(out)
}

// ---------------------------------------------------------------------------
// Helpers used by tests and downstream callers.
// ---------------------------------------------------------------------------

/// Build a Composer [`AgentRef`] suitable for use as the `from` field of
/// outgoing `AgentSpeak` messages.
#[must_use]
pub fn composer_ref(id: AgentId, label: impl Into<String>) -> AgentRef {
    AgentRef::new(id, AgentRole::Composer, label, SpawnSource::User)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inbox::{AgentHandle, AgentStatus};
    use crate::role::{AgentRef, SpawnSource};
    use phantom_memory::event_log::EventSource as LogEventSource;
    use serde_json::json;
    use tempfile::tempdir;
    use tokio::sync::{mpsc, watch};

    fn open_log() -> (Mutex<EventLog>, tempfile::TempDir) {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("events.jsonl");
        let log = EventLog::open(&path).expect("open");
        (Mutex::new(log), dir)
    }

    fn append(log: &Mutex<EventLog>, kind: &str, payload: serde_json::Value) {
        let mut g = log.lock().unwrap();
        g.append(LogEventSource::Substrate, kind, payload).unwrap();
    }

    fn fake_handle(
        id: AgentId,
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

    // ---- ComposerTool catalogue --------------------------------------------

    #[test]
    fn every_composer_tool_declares_a_capability_class() {
        for t in [
            ComposerTool::SpawnSubagent,
            ComposerTool::WaitForAgent,
            ComposerTool::RequestCritique,
            ComposerTool::EventLogQuery,
        ] {
            // Sanity: the role gates we documented above match what the
            // dispatcher will actually intersect against the role manifest.
            let class = t.class();
            assert!(
                matches!(
                    class,
                    CapabilityClass::Sense
                        | CapabilityClass::Reflect
                        | CapabilityClass::Compute
                        | CapabilityClass::Act
                        | CapabilityClass::Coordinate
                ),
                "{:?} has unexpected class {:?}",
                t,
                class,
            );
        }
        assert_eq!(ComposerTool::SpawnSubagent.class(), CapabilityClass::Coordinate);
        assert_eq!(ComposerTool::WaitForAgent.class(), CapabilityClass::Sense);
        assert_eq!(ComposerTool::RequestCritique.class(), CapabilityClass::Compute);
        assert_eq!(ComposerTool::EventLogQuery.class(), CapabilityClass::Sense);
    }

    #[test]
    fn composer_role_has_all_required_classes() {
        // The Composer manifest must declare every class the four tools
        // require (Sense, Compute, Coordinate). If a future refactor
        // narrows the manifest, this test fails loudly so the dispatcher's
        // default-deny gate catches it before runtime.
        let manifest = AgentRole::Composer.manifest();
        let needed = [
            CapabilityClass::Sense,
            CapabilityClass::Compute,
            CapabilityClass::Coordinate,
        ];
        for class in needed {
            assert!(
                manifest.classes.contains(&class),
                "Composer manifest missing {:?}; needed by {:?}",
                class,
                manifest.classes,
            );
        }
    }

    #[test]
    fn composer_tool_api_name_round_trip() {
        for t in [
            ComposerTool::SpawnSubagent,
            ComposerTool::WaitForAgent,
            ComposerTool::RequestCritique,
            ComposerTool::EventLogQuery,
        ] {
            let parsed = ComposerTool::from_api_name(t.api_name());
            assert_eq!(parsed, Some(t));
        }
        assert_eq!(ComposerTool::from_api_name("nope"), None);
    }

    // ---- spawn_subagent ----------------------------------------------------

    #[test]
    fn spawn_subagent_returns_new_id_via_pending_queue() {
        let queue = new_spawn_subagent_queue();
        let parent: AgentId = 1;
        let args = json!({
            "role": "watcher",
            "label": "test-watcher",
            "task": "watch the build",
        });

        let id = spawn_subagent(&args, parent, &queue).expect("ok");

        let q = queue.lock().unwrap();
        assert_eq!(q.len(), 1, "spawn must enqueue exactly one request");
        let req = &q[0];
        assert_eq!(req.assigned_id, id);
        assert_eq!(req.role, AgentRole::Watcher);
        assert_eq!(req.label, "test-watcher");
        assert_eq!(req.task, "watch the build");
        assert!(req.chat_model.is_none());
        assert_eq!(req.parent, parent);
    }

    #[test]
    fn spawn_subagent_with_explicit_model() {
        let queue = new_spawn_subagent_queue();
        let args = json!({
            "role": "composer",
            "label": "planner",
            "task": "delegate",
            "model": "gpt-4o",
        });
        spawn_subagent(&args, 1, &queue).expect("ok");
        let q = queue.lock().unwrap();
        assert!(q[0].chat_model.is_some());
    }

    #[test]
    fn spawn_subagent_rejects_unknown_role() {
        let queue = new_spawn_subagent_queue();
        let args = json!({
            "role": "wizard",
            "label": "x",
            "task": "y",
        });
        let err = spawn_subagent(&args, 1, &queue).expect_err("must fail");
        assert!(err.to_lowercase().contains("unknown role"));
    }

    // ---- wait_for_agent ----------------------------------------------------

    #[test]
    fn wait_for_agent_returns_completion_event() {
        let (log, _dir) = open_log();
        // Pre-seed a completion envelope. The kind uses the dotted suffix.
        append(
            &log,
            "agent.complete.42",
            json!({"agent_id": 42, "result": "ok"}),
        );

        let args = json!({"agent_id": 42, "max_seconds": 1u64});
        let env = wait_for_agent(&args, &log).expect("must find");
        assert!(env.kind.starts_with("agent.complete"));
        assert_eq!(
            env.payload.get("agent_id").and_then(|v| v.as_u64()),
            Some(42),
        );
    }

    #[test]
    fn wait_for_agent_times_out() {
        let (log, _dir) = open_log();
        // Empty log + zero-timeout deadline.
        let args = json!({"agent_id": 7, "max_seconds": 0u64});
        let err = wait_for_agent(&args, &log).expect_err("must time out");
        assert!(err.contains("timeout"));
        assert!(err.contains("7"));
    }

    #[test]
    fn wait_for_agent_filters_by_id() {
        let (log, _dir) = open_log();
        // A completion for someone else.
        append(
            &log,
            "agent.complete.99",
            json!({"agent_id": 99, "result": "ok"}),
        );
        let args = json!({"agent_id": 42, "max_seconds": 0u64});
        let err = wait_for_agent(&args, &log).expect_err("must time out");
        assert!(err.contains("timeout"));
    }

    // ---- request_critique --------------------------------------------------

    #[test]
    fn request_critique_sends_template_to_target() {
        let (log, _dir) = open_log();
        let mut reg = AgentRegistry::new();
        let (handle, mut rx) = fake_handle(7, AgentRole::Conversational, "alice");
        reg.register(handle);

        // Pre-seed the critique reply so wait_for_agent doesn't block.
        append(
            &log,
            "agent.complete.7",
            json!({"agent_id": 7, "verdict": "AGREE"}),
        );

        let composer = composer_ref(1, "comp");
        let args = json!({
            "from_agent_label": "alice",
            "of_message": "the sky is blue",
            "context": "weather forecast",
        });

        let env = request_critique(&args, &composer, &reg, &log).expect("ok");
        assert!(env.kind.contains("agent.complete"));

        // The recipient must have observed the templated AgentSpeak body.
        let msg = rx.try_recv().expect("recipient inbox must contain message");
        match msg {
            InboxMessage::AgentSpeak { from, body } => {
                assert_eq!(from.id, 1);
                assert!(
                    body.contains("Please critique the following message"),
                    "body must use the verbatim template; got: {body}",
                );
                assert!(body.contains("AGREE"));
                assert!(body.contains("DISAGREE"));
                assert!(body.contains("PARTIAL"));
                assert!(body.contains("the sky is blue"), "must embed of_message");
                assert!(body.contains("weather forecast"), "must embed context");
            }
            other => panic!("wrong inbox message: {other:?}"),
        }
    }

    #[test]
    fn request_critique_rejects_unknown_label() {
        let (log, _dir) = open_log();
        let reg = AgentRegistry::new();
        let composer = composer_ref(1, "comp");
        let args = json!({
            "from_agent_label": "ghost",
            "of_message": "x",
            "context": "y",
        });
        let err = request_critique(&args, &composer, &reg, &log).expect_err("must fail");
        assert!(err.to_lowercase().contains("unknown agent label"));
    }

    #[test]
    fn build_critique_body_contains_all_required_tokens() {
        let body = build_critique_body("the cake is done", "baking schedule");
        // The exact template wording is load-bearing for the Composer's
        // protocol — the recipient parses for AGREE/DISAGREE/PARTIAL.
        assert!(body.contains("Please critique the following message"));
        assert!(body.contains("AGREE"));
        assert!(body.contains("DISAGREE (with reason)"));
        assert!(body.contains("PARTIAL (with caveat)"));
        assert!(body.contains("the cake is done"));
        assert!(body.contains("baking schedule"));
    }

    // ---- event_log_query ---------------------------------------------------

    #[test]
    fn event_log_query_filters_by_kind_and_agent_id() {
        let (log, _dir) = open_log();
        // 5 mixed envelopes.
        append(&log, "tool.invoked", json!({"agent_id": 1}));
        append(&log, "agent.complete", json!({"agent_id": 1}));
        append(&log, "tool.invoked", json!({"agent_id": 2}));
        append(&log, "agent.complete", json!({"agent_id": 2}));
        append(&log, "agent.complete", json!({"agent_id": 1}));

        // Filter by kind only.
        let by_kind = event_log_query(
            &json!({"kind": "agent.complete"}),
            &log,
        )
        .unwrap();
        assert_eq!(by_kind.len(), 3);
        assert!(by_kind.iter().all(|e| e.kind == "agent.complete"));

        // Filter by agent_id only.
        let by_id = event_log_query(&json!({"agent_id": 1u64}), &log).unwrap();
        assert_eq!(by_id.len(), 3);
        assert!(by_id.iter().all(|e| e
            .payload
            .get("agent_id")
            .and_then(|v| v.as_u64())
            == Some(1)));

        // Compound filter.
        let both = event_log_query(
            &json!({"agent_id": 1u64, "kind": "agent.complete"}),
            &log,
        )
        .unwrap();
        assert_eq!(both.len(), 2);
    }

    #[test]
    fn event_log_query_respects_since_id_and_limit() {
        let (log, _dir) = open_log();
        for i in 0..6u64 {
            append(&log, "k", json!({"i": i}));
        }
        // since_id=3 should drop ids < 3 (we appended ids 1..=6).
        let after = event_log_query(&json!({"since_id": 3u64}), &log).unwrap();
        assert!(after.iter().all(|e| e.id >= 3));

        // limit=2 should cap output at 2 entries (the most recent ones).
        let limited = event_log_query(&json!({"limit": 2usize}), &log).unwrap();
        assert_eq!(limited.len(), 2);
    }

    #[test]
    fn event_log_query_empty_filter_returns_recent_tail() {
        let (log, _dir) = open_log();
        for i in 0..3u64 {
            append(&log, "k", json!({"i": i}));
        }
        let all = event_log_query(&json!({}), &log).unwrap();
        // Default limit is 20, we appended only 3.
        assert_eq!(all.len(), 3);
    }
}
