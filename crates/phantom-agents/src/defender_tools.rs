//! Defender-only tool surface.
//!
//! The [`AgentRole::Defender`] is auto-spawned by the substrate when the
//! Layer-2 dispatch gate emits
//! [`crate::spawn_rules::EventKind::CapabilityDenied`] for some other agent
//! (Sec.4). Sec.5 — this module — gives the Defender exactly one offensive
//! capability: ask the offender, in their own inbox, why they tried the
//! denied tool.
//!
//! ## The challenge_agent tool
//!
//! [`challenge_agent`] is the single tool exposed here. It is a
//! [`CapabilityClass::Coordinate`] tool because it speaks to another agent —
//! the Defender's manifest gains `Coordinate` precisely so this one route
//! works. The handler:
//!
//! 1. Looks up the target by [`AgentId`] (note: tool args carry `u32` per
//!    spec but the registry keys on `u64`; we widen at the boundary).
//! 2. Wraps the caller-supplied `question` with the canonical Defender
//!    framing, including the source `denial_event_id`, so the offender's
//!    next turn sees the inbox message with full context.
//! 3. Delivers an [`InboxMessage::AgentSpeak`] tagged from the Defender via
//!    the existing inbox substrate. Re-using `AgentSpeak` keeps the
//!    offender's inbox loop unchanged — the message looks like any other
//!    peer speech, attributed to the Defender's [`AgentRef`].
//! 4. Best-effort appends an `agent.challenge` envelope to the
//!    [`EventLog`] so the Inspector / Sec.3 panes can render the challenge
//!    alongside the original denial. Log emission is non-load-bearing: a
//!    missing or poisoned log does not fail the call (mirrors `chat_tools`
//!    semantics).
//!
//! ## Gating
//!
//! The dispatcher in [`crate::dispatch`] intersects [`DefenderTool::class`]
//! against the calling agent's role manifest before this handler runs. A
//! non-Defender role lacking `Coordinate` (e.g. `Watcher`, `Capturer`) sees
//! the canonical `"capability denied: Coordinate not in <Role> manifest"`
//! body in its `tool_result` block. The handler itself does not re-check.

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::inbox::{AgentRegistry, InboxMessage};
use crate::role::{AgentId, AgentRef, CapabilityClass};
use phantom_memory::event_log::{EventLog, EventSource as LogEventSource};

// ---------------------------------------------------------------------------
// DefenderTool catalog
// ---------------------------------------------------------------------------

/// The Defender-only tool ids.
///
/// Mirrors [`crate::chat_tools::ChatTool`] and
/// [`crate::composer_tools::ComposerTool`]: each variant carries the
/// [`CapabilityClass`] it requires so the role-aware dispatcher can default-
/// deny calls from agents whose manifest lacks that class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DefenderTool {
    /// Confront a denied agent. Posts the Defender's question into the
    /// target agent's inbox tagged with the original denial event id.
    ChallengeAgent,
}

impl DefenderTool {
    /// Wire name used in tool definitions and JSON dispatch.
    #[must_use]
    pub fn api_name(&self) -> &'static str {
        match self {
            Self::ChallengeAgent => "challenge_agent",
        }
    }

    /// Parse from a wire name. Returns `None` for unknown ids.
    #[must_use]
    pub fn from_api_name(name: &str) -> Option<Self> {
        match name {
            "challenge_agent" => Some(Self::ChallengeAgent),
            _ => None,
        }
    }

    /// The capability class the calling role must declare to invoke this
    /// tool. `challenge_agent` is `Coordinate` because it speaks to another
    /// agent — the Defender role's manifest is widened in lockstep so this
    /// is the single route the Defender holds.
    #[must_use]
    pub fn class(&self) -> CapabilityClass {
        match self {
            Self::ChallengeAgent => CapabilityClass::Coordinate,
        }
    }
}

// ---------------------------------------------------------------------------
// DefenderToolContext
// ---------------------------------------------------------------------------

/// Context handed to every [`DefenderTool`] handler.
///
/// Mirrors [`crate::chat_tools::ChatToolContext`]:
///
/// - `self_ref` is the calling Defender's [`AgentRef`], stamped on every
///   delivered inbox message and event-log entry so attribution is
///   preserved end-to-end.
/// - `registry` is the live agent directory used for id lookup of the
///   denied target.
/// - `event_log` is the shared append-only log; the `agent.challenge`
///   envelope is appended best-effort so the Inspector can correlate the
///   challenge with the source denial. `None` skips log emission — useful
///   for tests and legacy paths that haven't opened a log file yet.
#[derive(Clone)]
pub struct DefenderToolContext {
    pub self_ref: AgentRef,
    pub registry: Arc<Mutex<AgentRegistry>>,
    pub event_log: Option<Arc<Mutex<EventLog>>>,
}

impl DefenderToolContext {
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
// Argument decoder
// ---------------------------------------------------------------------------

/// Tool args for `challenge_agent`. Spec pins `target_agent_id` as `u32`;
/// we widen to [`AgentId`] (`u64`) at the registry boundary.
#[derive(Debug, Deserialize)]
struct ChallengeArgs {
    target_agent_id: u32,
    denial_event_id: u64,
    question: String,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The kind name we use for `agent.challenge` envelopes in the [`EventLog`].
///
/// Pinned as a constant so the Inspector / Sec.3 denials tab and any future
/// consumer can match on the exact string. Tests assert on this value so a
/// typo trips the suite.
pub const AGENT_CHALLENGE_EVENT_KIND: &str = "agent.challenge";

/// Format the question with the canonical Defender framing.
///
/// Shape: `"[defender challenge re: denial #<id>] <question>"`
///
/// Pinning the prefix means the offender's run loop (and any downstream
/// log consumer) can recognize a Defender challenge by string-matching on
/// `"[defender challenge"`. Carrying the denial event id inline is the
/// single piece of context the offender needs to look up the original
/// denial in the event log.
fn format_challenge_body(denial_event_id: u64, question: &str) -> String {
    format!("[defender challenge re: denial #{denial_event_id}] {question}")
}

/// Best-effort append of a challenge envelope to the shared event log.
///
/// Any failure (no log configured, poisoned mutex, I/O error) is swallowed —
/// the challenge delivery itself must not stall on log persistence. Mirrors
/// [`crate::chat_tools::append_speak_to_log`]'s contract.
fn append_challenge_to_log(
    log: &Option<Arc<Mutex<EventLog>>>,
    defender: &AgentRef,
    target_id: AgentId,
    denial_event_id: u64,
    question: &str,
) -> Option<u64> {
    let log = log.as_ref()?;
    let mut g = log.lock().ok()?;
    let payload = serde_json::json!({
        "from": {
            "id": defender.id,
            "role": defender.role.label(),
            "label": defender.label,
        },
        "to": {
            "kind": "agent",
            "id": target_id,
        },
        "denial_event_id": denial_event_id,
        "question": question,
    });
    g.append(
        LogEventSource::Agent { id: defender.id },
        AGENT_CHALLENGE_EVENT_KIND,
        payload,
    )
    .ok()
    .map(|env| env.id)
}

// ---------------------------------------------------------------------------
// challenge_agent
// ---------------------------------------------------------------------------

/// Confront a denied agent with a Defender's question.
///
/// Looks up `target_agent_id` in the [`AgentRegistry`]. If present, wraps
/// `question` with the canonical challenge framing (carrying
/// `denial_event_id` for context) and delivers an
/// [`InboxMessage::AgentSpeak`] to the target's inbox tagged from the
/// caller's [`AgentRef`]. Best-effort appends an `agent.challenge` envelope
/// to the event log when one is configured.
///
/// Returns:
/// - `Ok("delivered challenge to agent <id>")` on successful inbox delivery.
/// - `Err("agent id not found: <id>")` when the registry has no such agent.
/// - `Err("agent inbox closed: <id>")` when the recipient's channel was
///   dropped (agent crashed) or is full at try-send time.
/// - `Err("agent registry poisoned")` if the registry mutex is poisoned.
/// - `Err("invalid challenge_agent args: <reason>")` for malformed JSON.
pub fn challenge_agent(
    args: &serde_json::Value,
    ctx: &DefenderToolContext,
) -> Result<String, String> {
    let parsed: ChallengeArgs = serde_json::from_value(args.clone())
        .map_err(|e| format!("invalid challenge_agent args: {e}"))?;

    let target_id: AgentId = AgentId::from(parsed.target_agent_id);

    let registry = ctx
        .registry
        .lock()
        .map_err(|_| "agent registry poisoned".to_string())?;

    let handle = registry
        .get(target_id)
        .ok_or_else(|| format!("agent id not found: {target_id}"))?;

    let inbox = handle.inbox.clone();
    drop(registry);

    let body = format_challenge_body(parsed.denial_event_id, &parsed.question);

    inbox
        .try_send(InboxMessage::AgentSpeak {
            from: ctx.self_ref.clone(),
            body: body.clone(),
        })
        .map_err(|_| format!("agent inbox closed: {target_id}"))?;

    let _ = append_challenge_to_log(
        &ctx.event_log,
        &ctx.self_ref,
        target_id,
        parsed.denial_event_id,
        &parsed.question,
    );

    Ok(format!("delivered challenge to agent {target_id}"))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatch::{DispatchContext, dispatch_tool};
    use crate::composer_tools::new_spawn_subagent_queue;
    use crate::inbox::{AgentHandle, AgentStatus};
    use crate::role::{AgentRole, SpawnSource};
    use serde_json::json;
    use std::path::Path;
    use std::time::Duration;
    use tokio::sync::{mpsc, watch};
    use tokio::time::timeout;

    /// Build a fake registered agent. Returns the handle (for register) plus
    /// the receiver half so tests can verify what was delivered.
    fn fake_agent(
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

    // ---- DefenderTool catalog ----------------------------------------------

    #[test]
    fn defender_tool_api_name_round_trip() {
        for t in [DefenderTool::ChallengeAgent] {
            let parsed = DefenderTool::from_api_name(t.api_name());
            assert_eq!(parsed, Some(t));
        }
    }

    #[test]
    fn defender_tool_unknown_returns_none() {
        assert_eq!(DefenderTool::from_api_name("not_a_tool"), None);
    }

    #[test]
    fn defender_tool_class_assignments() {
        // Coordinate — the Defender role must declare this in its manifest
        // for dispatch to succeed.
        assert_eq!(
            DefenderTool::ChallengeAgent.class(),
            CapabilityClass::Coordinate,
        );
    }

    // ---- challenge_agent direct (delivery happy-path) ----------------------

    /// The Defender posts its challenge into the target's inbox as an
    /// `AgentSpeak` tagged from the Defender, with the body wrapped in the
    /// canonical `[defender challenge re: denial #<id>] <question>` framing.
    #[tokio::test]
    async fn challenge_agent_delivers_message_to_target_inbox() {
        // Defender id=99, target Watcher id=42.
        let (target_handle, mut target_rx) =
            fake_agent(42, AgentRole::Watcher, "offender");

        let mut reg = AgentRegistry::new();
        reg.register(target_handle);
        let registry = Arc::new(Mutex::new(reg));

        let defender_ref =
            AgentRef::new(99, AgentRole::Defender, "defender-on-denial", SpawnSource::Substrate);
        let ctx = DefenderToolContext::new(defender_ref, registry, None);

        let result = challenge_agent(
            &json!({
                "target_agent_id": 42u32,
                "denial_event_id": 7u64,
                "question": "why did you attempt run_command?",
            }),
            &ctx,
        )
        .expect("challenge should succeed");
        assert!(
            result.contains("delivered challenge to agent 42"),
            "wrong success body: {result}",
        );

        // Target inbox must have received the AgentSpeak from the Defender.
        let got = timeout(Duration::from_millis(100), target_rx.recv())
            .await
            .expect("receive timed out")
            .expect("channel closed");
        match got {
            InboxMessage::AgentSpeak { from, body } => {
                assert_eq!(from.id, 99, "challenge must be tagged from the Defender");
                assert_eq!(from.role, AgentRole::Defender);
                assert!(
                    body.starts_with("[defender challenge re: denial #7]"),
                    "body must carry canonical framing + denial id, got: {body}",
                );
                assert!(
                    body.contains("why did you attempt run_command?"),
                    "body must carry the original question, got: {body}",
                );
            }
            other => panic!("wrong inbox message: {other:?}"),
        }
    }

    /// Unknown target id surfaces a structured `not found` error so the
    /// model's next `tool_result` block can self-correct without retrying
    /// blindly.
    #[tokio::test]
    async fn challenge_agent_unknown_target_returns_err() {
        let registry = Arc::new(Mutex::new(AgentRegistry::new()));
        let defender_ref =
            AgentRef::new(99, AgentRole::Defender, "defender", SpawnSource::Substrate);
        let ctx = DefenderToolContext::new(defender_ref, registry, None);

        let err = challenge_agent(
            &json!({
                "target_agent_id": 1234u32,
                "denial_event_id": 5u64,
                "question": "why?",
            }),
            &ctx,
        )
        .expect_err("should fail when no such agent registered");
        assert!(err.contains("1234"), "error message lost target id: {err}");
        assert!(err.contains("not found"), "error message lost reason: {err}");
    }

    // ---- challenge_agent capability gate (dispatch-level) ------------------

    /// Load-bearing: a denied agent (here a `Watcher`, which lacks
    /// `Coordinate`) cannot itself invoke `challenge_agent`. The dispatch
    /// gate must short-circuit with the canonical
    /// `"capability denied: Coordinate not in <Role> manifest"` body before
    /// the handler ever runs. Without this, an offender could "challenge"
    /// the Defender right back, bypassing the security model.
    #[test]
    fn challenge_agent_denied_for_role_lacking_coordinate() {
        let target_dir = std::env::temp_dir();
        // Put any agent in the registry — the gate fires before lookup.
        let (peer_handle, _peer_rx) = fake_agent(42, AgentRole::Watcher, "peer");
        let mut reg = AgentRegistry::new();
        reg.register(peer_handle);
        let registry = Arc::new(Mutex::new(reg));

        let watcher_ref =
            AgentRef::new(7, AgentRole::Watcher, "offender", SpawnSource::Substrate);
        let ctx = DispatchContext {
            self_ref: watcher_ref,
            role: AgentRole::Watcher,
            working_dir: Path::new(&target_dir),
            registry,
            event_log: None,
            pending_spawn: new_spawn_subagent_queue(),
            source_event_id: None,
        };

        let result = dispatch_tool(
            "challenge_agent",
            &json!({
                "target_agent_id": 42u32,
                "denial_event_id": 1u64,
                "question": "you can't ask me this",
            }),
            &ctx,
        );

        assert!(!result.success, "Watcher must not be allowed Coordinate tools");
        assert!(
            result.output.starts_with("capability denied:"),
            "expected canonical phrasing, got: {}",
            result.output,
        );
        assert!(
            result.output.contains("Coordinate"),
            "denial must name the missing class, got: {}",
            result.output,
        );
        assert!(
            result.output.contains("Watcher"),
            "denial must name the offending role, got: {}",
            result.output,
        );
    }

    // ---- challenge_agent log emission --------------------------------------

    /// When an event log is configured, the challenge is recorded as an
    /// `agent.challenge` envelope carrying `from` (the Defender), `to`
    /// (the offender id), `denial_event_id`, and the verbatim question.
    /// Sec.3 (Inspector denials tab) reads these envelopes to attach the
    /// challenge underneath the original denial in the source-chain tree.
    #[tokio::test]
    async fn challenge_agent_appends_envelope_to_log_when_configured() {
        use phantom_memory::event_log::EventLog;
        use tempfile::tempdir;

        let dir = tempdir().expect("tempdir");
        let log_path = dir.path().join("events.jsonl");
        let log = Arc::new(Mutex::new(EventLog::open(&log_path).expect("open log")));

        let (target_handle, _rx) = fake_agent(42, AgentRole::Watcher, "offender");
        let mut reg = AgentRegistry::new();
        reg.register(target_handle);
        let registry = Arc::new(Mutex::new(reg));

        let defender_ref =
            AgentRef::new(99, AgentRole::Defender, "defender", SpawnSource::Substrate);
        let ctx = DefenderToolContext::new(defender_ref, registry, Some(log.clone()));

        challenge_agent(
            &json!({
                "target_agent_id": 42u32,
                "denial_event_id": 13u64,
                "question": "why did you attempt write_file?",
            }),
            &ctx,
        )
        .expect("challenge should succeed");

        let g = log.lock().unwrap();
        let tail = g.tail(64);
        drop(g);
        let challenge = tail
            .iter()
            .find(|e| e.kind == AGENT_CHALLENGE_EVENT_KIND)
            .expect("agent.challenge envelope must be appended");
        assert_eq!(
            challenge.payload["from"]["label"].as_str(),
            Some("defender"),
        );
        assert_eq!(challenge.payload["to"]["id"].as_u64(), Some(42));
        assert_eq!(
            challenge.payload["denial_event_id"].as_u64(),
            Some(13),
        );
        assert_eq!(
            challenge.payload["question"].as_str(),
            Some("why did you attempt write_file?"),
        );
    }
}
