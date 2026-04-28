//! Agent inbox, handle, and registry primitives.
//!
//! Each spawned agent has an [`AgentHandle`] holding:
//! - its [`AgentRef`] (identity/role/label/spawn metadata),
//! - an `mpsc::Sender<InboxMessage>` the substrate uses to deliver work,
//! - a `watch::Receiver<AgentStatus>` exposing live lifecycle state.
//!
//! [`AgentRegistry`] owns the `HashMap<AgentId, AgentHandle>` and provides
//! addressing primitives: register/unregister, point-to-point [`route`], and
//! role-scoped broadcast. Routing is fail-loud — addressing a dead or unknown
//! agent returns [`RouteError`] instead of silently dropping.
//!
//! [`route`]: AgentRegistry::route

use std::collections::HashMap;

use crate::role::{AgentId, AgentRef, AgentRole};

// ---------------------------------------------------------------------------
// Messages & status
// ---------------------------------------------------------------------------

/// Anything that can land in an agent's inbox. The agent's run loop selects
/// over this and any role-specific subscriptions.
#[derive(Debug)]
pub enum InboxMessage {
    /// User typed something targeted at this agent.
    UserSpeak(String),
    /// Another agent emitted speech addressed (or broadcast) to this one.
    AgentSpeak { from: AgentRef, body: String },
    /// Cooperative shutdown request. Agent should drain, persist, then exit.
    Stop,
    /// Live config update. Schema is role-defined and validated per-agent.
    Reconfigure(serde_json::Value),
}

/// Coarse lifecycle phase reported by an agent on its `watch` channel. The
/// substrate uses this to badge the UI and decide whether to send work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStatus {
    /// Constructed but not yet entered its run loop.
    Spawning,
    /// In the run loop, no active work, accepting messages.
    Idle,
    /// Processing a message or running a tool.
    Working,
    /// Currently streaming speech tokens to the UI.
    EmittingSpeech,
    /// Exited cleanly via `Stop` or natural completion.
    Stopped,
    /// Terminated abnormally; substrate may respawn under policy.
    Failed,
}

// ---------------------------------------------------------------------------
// Handle
// ---------------------------------------------------------------------------

/// Substrate-side handle to a live agent. Cloning the inbox sender is fine;
/// the watch receiver supports being subscribed to by multiple observers.
pub struct AgentHandle {
    pub agent_ref: AgentRef,
    pub inbox: tokio::sync::mpsc::Sender<InboxMessage>,
    pub status: tokio::sync::watch::Receiver<AgentStatus>,
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// In-memory directory of all live agents, keyed by [`AgentId`]. Single-owner;
/// the substrate orchestrator holds it and gates access. Not `Sync`-friendly
/// by design — share via an `Arc<Mutex<_>>` at the orchestrator boundary.
#[derive(Default)]
pub struct AgentRegistry {
    by_id: HashMap<AgentId, AgentHandle>,
}

impl AgentRegistry {
    /// Construct an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            by_id: HashMap::new(),
        }
    }

    /// Insert (or replace) the handle keyed by its `agent_ref.id`.
    pub fn register(&mut self, handle: AgentHandle) {
        self.by_id.insert(handle.agent_ref.id, handle);
    }

    /// Drop the handle for `id`, if any. Closing inbox/status channels is the
    /// caller's prior responsibility.
    pub fn unregister(&mut self, id: AgentId) {
        self.by_id.remove(&id);
    }

    /// Look up a handle by id.
    #[must_use]
    pub fn get(&self, id: AgentId) -> Option<&AgentHandle> {
        self.by_id.get(&id)
    }

    /// Send `msg` to the agent identified by `target`. Returns [`RouteError`]
    /// if the agent is unknown or its inbox has been closed (receiver dropped).
    ///
    /// Uses the non-blocking `try_send` so the substrate cannot wedge on a
    /// stalled agent. A full inbox is reported as [`RouteError::InboxClosed`]
    /// (we treat back-pressure as a liveness failure for routing purposes).
    pub fn route(&self, target: AgentId, msg: InboxMessage) -> Result<(), RouteError> {
        let Some(handle) = self.by_id.get(&target) else {
            return Err(RouteError::NoSuchAgent(target));
        };
        handle
            .inbox
            .try_send(msg)
            .map_err(|_| RouteError::InboxClosed)
    }

    /// Deliver `msg` to every agent whose role matches `role`, returning the
    /// number of agents that successfully received it. Failed deliveries
    /// (closed inboxes, full channels) are silently skipped — broadcast is
    /// best-effort by design.
    pub fn broadcast_role(&self, role: AgentRole, msg: InboxMessage) -> usize {
        let mut delivered = 0usize;
        // We only have one `msg`. Build a fresh `Reconfigure`/etc per send by
        // cloning the body where we can; otherwise swap to a custom clone.
        // Since `InboxMessage` doesn't impl `Clone` (Reconfigure carries a
        // Value that we don't want to require Clone here), we synthesize the
        // per-recipient copy via match.
        for handle in self.by_id.values().filter(|h| h.agent_ref.role == role) {
            let copy = clone_msg(&msg);
            if handle.inbox.try_send(copy).is_ok() {
                delivered += 1;
            }
        }
        delivered
    }

    /// All live agent refs, in insertion-order-ish (HashMap iteration).
    pub fn list(&self) -> Vec<&AgentRef> {
        self.by_id.values().map(|h| &h.agent_ref).collect()
    }

    /// First handle whose label matches `label` exactly. Returns `None` if no
    /// agent has that label. Labels aren't guaranteed unique by the registry;
    /// the spawner is responsible for label discipline.
    pub fn find_by_label(&self, label: &str) -> Option<&AgentHandle> {
        self.by_id.values().find(|h| h.agent_ref.label == label)
    }
}

/// Manual per-recipient clone of an [`InboxMessage`]. Used by [`broadcast_role`]
/// because `serde_json::Value` is `Clone` but `mpsc::Sender::try_send` consumes
/// the message — we need a fresh value per recipient.
///
/// [`broadcast_role`]: AgentRegistry::broadcast_role
fn clone_msg(msg: &InboxMessage) -> InboxMessage {
    match msg {
        InboxMessage::UserSpeak(s) => InboxMessage::UserSpeak(s.clone()),
        InboxMessage::AgentSpeak { from, body } => InboxMessage::AgentSpeak {
            from: from.clone(),
            body: body.clone(),
        },
        InboxMessage::Stop => InboxMessage::Stop,
        InboxMessage::Reconfigure(v) => InboxMessage::Reconfigure(v.clone()),
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Failure modes for [`AgentRegistry::route`].
#[derive(Debug, thiserror::Error)]
pub enum RouteError {
    #[error("no agent with id {0}")]
    NoSuchAgent(AgentId),
    #[error("agent inbox closed")]
    InboxClosed,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::role::SpawnSource;
    use std::time::Duration;
    use tokio::sync::{mpsc, watch};
    use tokio::time::timeout;

    /// Build a fake agent: returns the handle (to register) plus the receiver
    /// half so tests can verify what was delivered.
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

    /// register/get/unregister round trip works.
    #[tokio::test]
    async fn register_get_unregister_roundtrip() {
        let mut reg = AgentRegistry::new();
        let (handle, _rx) = fake_agent(1, AgentRole::Watcher, "w1");

        reg.register(handle);
        assert!(reg.get(1).is_some(), "should find registered agent");
        assert_eq!(reg.list().len(), 1);

        reg.unregister(1);
        assert!(reg.get(1).is_none(), "should be gone after unregister");
        assert_eq!(reg.list().len(), 0);
    }

    /// route() delivers the exact message to the addressed agent's inbox.
    #[tokio::test]
    async fn route_delivers_to_specific_inbox() {
        let mut reg = AgentRegistry::new();
        let (h1, mut rx1) = fake_agent(1, AgentRole::Watcher, "w1");
        let (h2, mut rx2) = fake_agent(2, AgentRole::Watcher, "w2");
        reg.register(h1);
        reg.register(h2);

        reg.route(2, InboxMessage::UserSpeak("hi 2".into()))
            .expect("route should succeed");

        // Addressed inbox receives.
        let got = timeout(Duration::from_millis(100), rx2.recv())
            .await
            .expect("receive timed out")
            .expect("channel closed");
        match got {
            InboxMessage::UserSpeak(s) => assert_eq!(s, "hi 2"),
            other => panic!("wrong message: {other:?}"),
        }

        // Non-addressed inbox does NOT receive (try_recv returns Empty).
        assert!(rx1.try_recv().is_err(), "agent 1 should not have received");
    }

    /// route() to an unknown id returns NoSuchAgent.
    #[tokio::test]
    async fn route_to_absent_id_returns_no_such_agent() {
        let reg = AgentRegistry::new();
        let err = reg
            .route(999, InboxMessage::Stop)
            .expect_err("should fail");
        match err {
            RouteError::NoSuchAgent(id) => assert_eq!(id, 999),
            other => panic!("wrong error: {other:?}"),
        }
    }

    /// route() to a closed inbox (receiver dropped) returns InboxClosed.
    #[tokio::test]
    async fn route_to_closed_inbox_returns_inbox_closed() {
        let mut reg = AgentRegistry::new();
        let (handle, rx) = fake_agent(1, AgentRole::Watcher, "w1");
        reg.register(handle);
        drop(rx); // simulate agent crash

        let err = reg
            .route(1, InboxMessage::Stop)
            .expect_err("should fail");
        assert!(matches!(err, RouteError::InboxClosed), "got {err:?}");
    }

    /// broadcast_role hits exactly the matching agents and returns the count.
    #[tokio::test]
    async fn broadcast_role_hits_only_matching_agents() {
        let mut reg = AgentRegistry::new();
        let (w1, mut w1_rx) = fake_agent(1, AgentRole::Watcher, "w1");
        let (w2, mut w2_rx) = fake_agent(2, AgentRole::Watcher, "w2");
        let (a1, mut a1_rx) = fake_agent(3, AgentRole::Actor, "a1");
        reg.register(w1);
        reg.register(w2);
        reg.register(a1);

        let n = reg.broadcast_role(AgentRole::Watcher, InboxMessage::Stop);
        assert_eq!(n, 2, "should reach both watchers");

        // Both watchers got Stop.
        for rx in [&mut w1_rx, &mut w2_rx] {
            let got = timeout(Duration::from_millis(100), rx.recv())
                .await
                .expect("watcher receive timed out")
                .expect("watcher channel closed");
            assert!(matches!(got, InboxMessage::Stop));
        }
        // Actor did not.
        assert!(a1_rx.try_recv().is_err(), "actor should not be hit");
    }

    /// broadcast_role to a role with no agents returns 0 and is a no-op.
    #[tokio::test]
    async fn broadcast_role_with_no_matches_returns_zero() {
        let mut reg = AgentRegistry::new();
        let (h, _rx) = fake_agent(1, AgentRole::Watcher, "w");
        reg.register(h);

        let n = reg.broadcast_role(AgentRole::Composer, InboxMessage::Stop);
        assert_eq!(n, 0);
    }

    /// find_by_label returns Some for an existing label and None for unknown.
    #[tokio::test]
    async fn find_by_label_some_and_none() {
        let mut reg = AgentRegistry::new();
        let (h, _rx) = fake_agent(7, AgentRole::Reflector, "contradiction-finder");
        reg.register(h);

        let found = reg.find_by_label("contradiction-finder");
        assert!(found.is_some());
        assert_eq!(found.unwrap().agent_ref.id, 7);

        assert!(reg.find_by_label("nope").is_none());
    }
}
