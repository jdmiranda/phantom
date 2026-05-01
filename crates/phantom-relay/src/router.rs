//! Peer registry and envelope router.
//!
//! The `Router` is the single shared mutable state of the relay.  It is
//! wrapped in an `Arc<Mutex<…>>` and cloned into each connection task.

use std::collections::HashMap;

use anyhow::{bail, Result};
use log::{debug, warn};

use crate::envelope::{ClientMessage, Envelope, PeerId, RelayMessage};
use crate::grant::{CapabilityClass, Grant, PeerGrantRegistry};
use crate::rate_limit::TokenBucket;
use crate::session::SessionHandle;

/// Environment variable that flips the unsigned-envelope policy from
/// "accept with WARN" (migration window) to "deny" (issue #525).
///
/// Accepted truthy values: `"1"` and `"true"` (case-insensitive). Anything
/// else — including an unset variable — preserves the migration-window
/// behavior so existing peers continue to work during rollout.
pub const ENV_REQUIRE_SIGNATURES: &str = "PHANTOM_RELAY_REQUIRE_SIGNATURES";

/// Returns `true` when [`ENV_REQUIRE_SIGNATURES`] is set to a truthy value.
fn deny_unsigned_from_env() -> bool {
    match std::env::var(ENV_REQUIRE_SIGNATURES) {
        Ok(v) => {
            let lower = v.to_ascii_lowercase();
            lower == "1" || lower == "true"
        }
        Err(_) => false,
    }
}

/// Shared state of the relay server.
pub struct Router {
    /// Live peer → session handle map.
    sessions: HashMap<PeerId, SessionHandle>,
    /// Per-peer token buckets.
    rate_buckets: HashMap<PeerId, TokenBucket>,
    /// Per-peer capability grant registry (default-deny).
    grants: PeerGrantRegistry,
    /// Default rate limit (messages / second).
    rate_limit: u32,
    /// Maximum simultaneously connected peers.
    max_peers: usize,
    /// When `true`, envelopes lacking an Ed25519 signature are rejected with
    /// [`RelayMessage::SignatureRequired`] instead of being accepted with a
    /// migration-window WARN. Toggled via [`ENV_REQUIRE_SIGNATURES`] at
    /// construction time, or explicitly via
    /// [`Router::with_deny_unsigned`] (issue #525).
    deny_unsigned: bool,
}

impl Router {
    /// Create a new router with the given operator limits.
    ///
    /// Reads [`ENV_REQUIRE_SIGNATURES`] to decide whether unsigned envelopes
    /// are denied. When the env var is unset (the default), the relay
    /// preserves the migration-window behavior of accepting unsigned
    /// envelopes with a WARN log.
    #[must_use]
    pub fn new(rate_limit: u32, max_peers: usize) -> Self {
        Self::with_deny_unsigned(rate_limit, max_peers, deny_unsigned_from_env())
    }

    /// Create a new router with an explicit `deny_unsigned` policy, bypassing
    /// the [`ENV_REQUIRE_SIGNATURES`] env-var lookup.
    ///
    /// Useful for tests and for callers that load the policy from their own
    /// configuration source rather than the process environment.
    #[must_use]
    pub fn with_deny_unsigned(rate_limit: u32, max_peers: usize, deny_unsigned: bool) -> Self {
        Self {
            sessions: HashMap::new(),
            rate_buckets: HashMap::new(),
            grants: PeerGrantRegistry::new(),
            rate_limit,
            max_peers,
            deny_unsigned,
        }
    }

    /// Returns `true` when this router is configured to reject unsigned
    /// envelopes.
    #[must_use]
    pub fn deny_unsigned(&self) -> bool {
        self.deny_unsigned
    }

    /// Grant a capability to `peer_id`.
    pub fn grant(&mut self, peer_id: &PeerId, grant: Grant) {
        self.grants.grant(peer_id, grant);
    }

    /// Revoke a specific capability for `peer_id`.
    pub fn revoke(&mut self, peer_id: &PeerId, class: CapabilityClass) {
        self.grants.revoke(peer_id, class);
    }

    /// Revoke all capability grants for `peer_id` (e.g. on disconnect).
    pub fn revoke_all(&mut self, peer_id: &PeerId) {
        self.grants.revoke_all(peer_id);
    }

    /// Access the underlying [`PeerGrantRegistry`] for inspection.
    #[must_use]
    pub fn grants(&self) -> &PeerGrantRegistry {
        &self.grants
    }

    /// Register a peer's session handle.
    ///
    /// Returns an error when `max_peers` is already reached or the peer is
    /// already registered.
    pub fn register(&mut self, handle: SessionHandle) -> Result<()> {
        if self.sessions.len() >= self.max_peers {
            bail!("max_peers ({}) reached", self.max_peers);
        }
        let id = handle.peer_id.clone();
        if self.sessions.contains_key(&id) {
            bail!("peer {} is already registered", id);
        }
        debug!("router: registered peer {}", id);
        self.sessions.insert(id.clone(), handle);
        self.rate_buckets
            .entry(id)
            .or_insert_with(|| TokenBucket::new(self.rate_limit));
        Ok(())
    }

    /// Remove a peer from the registry (on disconnect).
    pub fn unregister(&mut self, peer_id: &PeerId) {
        debug!("router: unregistered peer {}", peer_id);
        self.sessions.remove(peer_id);
        // Keep the rate bucket so a reconnecting peer inherits its state —
        // prevents burst abuse via rapid reconnects.
    }

    /// Route a [`ClientMessage`] from `sender`.
    ///
    /// For `Send` variants:
    ///   - Check the sender's rate bucket.
    ///   - Look up the destination session.
    ///   - Forward the serialized envelope via the channel.
    ///
    /// Returns the [`RelayMessage`] that should be sent back to the sender.
    pub fn route(&mut self, sender: &PeerId, msg: ClientMessage) -> RelayMessage {
        match msg {
            ClientMessage::Pong => {
                // Keepalive acknowledged; nothing to route.
                debug!("router: pong from {}", sender);
                RelayMessage::Ping // silence — caller ignores this for Pong
            }
            ClientMessage::Send(envelope) => self.forward(sender, envelope),
        }
    }

    fn forward(&mut self, sender: &PeerId, envelope: Envelope) -> RelayMessage {
        // --- unsigned-envelope policy (issue #525) ---
        //
        // During the migration window we accept envelopes with an empty `sig`
        // field and only emit a WARN log so existing un-upgraded peers keep
        // working. Operators can flip this to deny-all by setting the env
        // var `PHANTOM_RELAY_REQUIRE_SIGNATURES=1` (or `true`) before the
        // relay process starts, or by constructing the router with
        // [`Router::with_deny_unsigned`].
        //
        // When `deny_unsigned` is true, the unsigned envelope is rejected
        // with `RelayMessage::SignatureRequired` and never reaches rate-limit
        // accounting or destination lookup.
        //
        // The relay only checks for *presence* of a signature here. Full
        // cryptographic verification is the responsibility of the recipient
        // (see `signing::verify_envelope`); the relay does not hold per-peer
        // verifying keys.
        if envelope.sig.is_empty() {
            if self.deny_unsigned {
                warn!(
                    "router: unsigned envelope from peer {} rejected (deny_unsigned=true)",
                    sender
                );
                return RelayMessage::SignatureRequired {
                    peer_id: sender.clone(),
                };
            }
            warn!(
                "router: unsigned envelope from peer {} accepted under migration window \
                 (set {} to deny)",
                sender, ENV_REQUIRE_SIGNATURES
            );
        }

        // --- capability grant check (issue #415) ---
        //
        // Every peer must hold a `Relay` grant before any envelope is
        // forwarded. server.rs issues this grant on handshake success and
        // revokes it on disconnect. Unknown / unganted peers are denied here
        // with a `CapabilityDenied` reply, preventing unauthenticated message
        // injection without a rate-limit token burn.
        if let Err(reason) = self.grants.check(sender, CapabilityClass::Relay) {
            warn!(
                "router: capability denied for peer {} (Relay): {}",
                sender, reason
            );
            return RelayMessage::CapabilityDenied {
                peer_id: sender.clone(),
                reason: reason.to_string(),
            };
        }

        // --- rate limit check ---
        let bucket = self
            .rate_buckets
            .entry(sender.clone())
            .or_insert_with(|| TokenBucket::new(self.rate_limit));

        let (allowed, retry_ms) = bucket.check();
        if !allowed {
            warn!(
                "router: rate limit exceeded for peer {} (retry in {}ms)",
                sender, retry_ms
            );
            return RelayMessage::RateLimitExceeded {
                peer_id: sender.clone(),
                retry_after_ms: retry_ms,
            };
        }

        // --- destination lookup ---
        let nonce = envelope.nonce;
        let to = envelope.to.clone();

        let Some(dest) = self.sessions.get(&to) else {
            warn!("router: peer {} not found (requested by {})", to, sender);
            return RelayMessage::PeerNotFound { peer_id: to };
        };

        // Serialize and enqueue.  A send error means the destination task
        // has already exited; treat it as "not found".
        let Ok(json) = serde_json::to_string(&ClientMessage::Send(envelope)) else {
            return RelayMessage::Error {
                code: "serialization_error".into(),
                message: "failed to serialize envelope".into(),
            };
        };

        if dest.tx.try_send(json).is_err() {
            warn!(
                "router: failed to enqueue to {}; channel full or closed",
                to
            );
            return RelayMessage::PeerNotFound { peer_id: to };
        }

        debug!(
            "router: delivered envelope {} from {} to {}",
            nonce, sender, to
        );
        RelayMessage::Delivered { nonce }
    }

    /// Number of currently connected peers.
    #[must_use]
    pub fn peer_count(&self) -> usize {
        self.sessions.len()
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use crate::grant::Grant;
    use crate::session::Session;
    use uuid::Uuid;

    fn make_session(id: &str) -> (Session, SessionHandle) {
        let session = Session::new(PeerId(id.into()));
        let handle = session.handle();
        (session, handle)
    }

    /// Grant a permanent `Relay` capability to `peer_id` on `router`.
    fn grant_relay(router: &mut Router, peer_id: &str) {
        router.grant(&PeerId(peer_id.into()), Grant::permanent(CapabilityClass::Relay));
    }

    #[test]
    fn register_and_unregister() {
        let mut router = Router::new(100, 10);
        let (_, handle) = make_session("alice");
        router.register(handle).unwrap();
        assert_eq!(router.peer_count(), 1);
        router.unregister(&PeerId("alice".into()));
        assert_eq!(router.peer_count(), 0);
    }

    #[test]
    fn duplicate_registration_fails() {
        let mut router = Router::new(100, 10);
        let (_, handle1) = make_session("alice");
        let (_, handle2) = make_session("alice");
        router.register(handle1).unwrap();
        assert!(router.register(handle2).is_err());
    }

    #[test]
    fn max_peers_respected() {
        let mut router = Router::new(100, 2);
        let (_, h1) = make_session("a");
        let (_, h2) = make_session("b");
        let (_, h3) = make_session("c");
        router.register(h1).unwrap();
        router.register(h2).unwrap();
        assert!(router.register(h3).is_err());
    }

    #[tokio::test]
    async fn forward_delivers_to_destination() {
        let mut router = Router::new(100, 10);
        let (mut session_bob, handle_bob) = make_session("bob");
        let (_, handle_alice) = make_session("alice");
        router.register(handle_alice).unwrap();
        router.register(handle_bob).unwrap();
        // Both peers must hold a Relay grant (issued by server.rs on handshake
        // in production; issued manually here in tests).
        grant_relay(&mut router, "alice");
        grant_relay(&mut router, "bob");

        let envelope = Envelope {
            from: PeerId("alice".into()),
            to: PeerId("bob".into()),
            payload: serde_json::json!("hello"),
            sig: "deadbeef".into(),
            nonce: Uuid::new_v4(),
        };
        let nonce = envelope.nonce;

        let reply = router.route(&PeerId("alice".into()), ClientMessage::Send(envelope));
        assert!(matches!(reply, RelayMessage::Delivered { nonce: n } if n == nonce));

        // Bob's channel should have the message.
        let raw = session_bob.rx.try_recv().expect("message not delivered");
        assert!(raw.contains("hello"));
    }

    #[test]
    fn peer_not_found_reply() {
        let mut router = Router::new(100, 10);
        let (_, handle_alice) = make_session("alice");
        router.register(handle_alice).unwrap();
        grant_relay(&mut router, "alice");

        let envelope = Envelope {
            from: PeerId("alice".into()),
            to: PeerId("ghost".into()),
            payload: serde_json::json!(null),
            sig: "sig".into(),
            nonce: Uuid::new_v4(),
        };
        let reply = router.route(&PeerId("alice".into()), ClientMessage::Send(envelope));
        assert!(matches!(reply, RelayMessage::PeerNotFound { .. }));
    }

    #[test]
    fn rate_limit_trips_on_burst() {
        let mut router = Router::new(3, 10); // 3 msg/s
        let (_alice_session, ha) = make_session("alice");
        let (_bob_session, hb) = make_session("bob"); // keep rx alive so channel stays open
        router.register(ha).unwrap();
        router.register(hb).unwrap();
        grant_relay(&mut router, "alice");
        grant_relay(&mut router, "bob");

        let make_env = || Envelope {
            from: PeerId("alice".into()),
            to: PeerId("bob".into()),
            payload: serde_json::json!(null),
            sig: "sig".into(),
            nonce: Uuid::new_v4(),
        };

        // First 3 should succeed.
        for _ in 0..3 {
            let reply = router.route(&PeerId("alice".into()), ClientMessage::Send(make_env()));
            assert!(matches!(reply, RelayMessage::Delivered { .. }), "expected Delivered");
        }
        // 4th should be rejected.
        let reply = router.route(&PeerId("alice".into()), ClientMessage::Send(make_env()));
        assert!(
            matches!(reply, RelayMessage::RateLimitExceeded { .. }),
            "expected RateLimitExceeded, got {:?}",
            reply
        );
    }

    // ── Grant-check tests (issue #415) ────────────────────────────────────────

    /// A peer with no Relay grant must receive `CapabilityDenied`.
    #[test]
    fn unganted_peer_is_denied() {
        let mut router = Router::new(100, 10);
        let (_, ha) = make_session("alice");
        let (_, hb) = make_session("bob");
        router.register(ha).unwrap();
        router.register(hb).unwrap();
        // No grant issued for alice.

        let envelope = Envelope {
            from: PeerId("alice".into()),
            to: PeerId("bob".into()),
            payload: serde_json::json!(null),
            sig: "sig".into(),
            nonce: Uuid::new_v4(),
        };
        let reply = router.route(&PeerId("alice".into()), ClientMessage::Send(envelope));
        assert!(
            matches!(reply, RelayMessage::CapabilityDenied { .. }),
            "expected CapabilityDenied, got {reply:?}"
        );
    }

    /// After `revoke`, subsequent forwards are denied.
    #[test]
    fn revoke_denies_subsequent_forward() {
        let mut router = Router::new(100, 10);
        let (_, ha) = make_session("alice");
        let (_, hb) = make_session("bob");
        router.register(ha).unwrap();
        router.register(hb).unwrap();
        grant_relay(&mut router, "alice");

        router.revoke(&PeerId("alice".into()), CapabilityClass::Relay);

        let envelope = Envelope {
            from: PeerId("alice".into()),
            to: PeerId("bob".into()),
            payload: serde_json::json!(null),
            sig: "sig".into(),
            nonce: Uuid::new_v4(),
        };
        let reply = router.route(&PeerId("alice".into()), ClientMessage::Send(envelope));
        assert!(
            matches!(reply, RelayMessage::CapabilityDenied { .. }),
            "expected CapabilityDenied after revoke, got {reply:?}"
        );
    }

    /// An expired grant (expiry set in the past) must be denied.
    #[test]
    fn expired_grant_is_denied() {
        let mut router = Router::new(100, 10);
        let (_, ha) = make_session("alice");
        let (_, hb) = make_session("bob");
        router.register(ha).unwrap();
        router.register(hb).unwrap();

        let past = Instant::now() - Duration::from_secs(1);
        router.grant(
            &PeerId("alice".into()),
            Grant::with_expiry(CapabilityClass::Relay, past),
        );

        let envelope = Envelope {
            from: PeerId("alice".into()),
            to: PeerId("bob".into()),
            payload: serde_json::json!(null),
            sig: "sig".into(),
            nonce: Uuid::new_v4(),
        };
        let reply = router.route(&PeerId("alice".into()), ClientMessage::Send(envelope));
        assert!(
            matches!(reply, RelayMessage::CapabilityDenied { .. }),
            "expected CapabilityDenied for expired grant, got {reply:?}"
        );
    }

    /// A grant expiring in the future should be accepted.
    #[tokio::test]
    async fn future_expiry_grant_delivers() {
        let mut router = Router::new(100, 10);
        let (mut bob_session, hb) = make_session("bob");
        let (_, ha) = make_session("alice");
        router.register(ha).unwrap();
        router.register(hb).unwrap();

        let future = Instant::now() + Duration::from_secs(60);
        router.grant(
            &PeerId("alice".into()),
            Grant::with_expiry(CapabilityClass::Relay, future),
        );

        let envelope = Envelope {
            from: PeerId("alice".into()),
            to: PeerId("bob".into()),
            payload: serde_json::json!("future-grant"),
            sig: "sig".into(),
            nonce: Uuid::new_v4(),
        };
        let nonce = envelope.nonce;

        let reply = router.route(&PeerId("alice".into()), ClientMessage::Send(envelope));
        assert!(
            matches!(reply, RelayMessage::Delivered { nonce: n } if n == nonce),
            "expected Delivered with future expiry grant, got {reply:?}"
        );

        let raw = bob_session.rx.try_recv().expect("message not delivered");
        assert!(raw.contains("future-grant"));
    }

    // ── Unsigned-envelope policy tests (issue #525) ───────────────────────────

    /// Process-wide lock that serializes env-var manipulation across tests.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// RAII guard that sets `ENV_REQUIRE_SIGNATURES` and restores the prior
    /// value on drop. The `_lock` field keeps the test serial.
    struct EnvGuard {
        prev: Option<String>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl EnvGuard {
        fn set(value: &str) -> Self {
            let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let prev = std::env::var(ENV_REQUIRE_SIGNATURES).ok();
            // SAFETY: tests are serialized by `ENV_LOCK`.
            unsafe {
                std::env::set_var(ENV_REQUIRE_SIGNATURES, value);
            }
            Self { prev, _lock: lock }
        }

        fn unset() -> Self {
            let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let prev = std::env::var(ENV_REQUIRE_SIGNATURES).ok();
            // SAFETY: tests are serialized by `ENV_LOCK`.
            unsafe {
                std::env::remove_var(ENV_REQUIRE_SIGNATURES);
            }
            Self { prev, _lock: lock }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: tests are serialized by `ENV_LOCK`.
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var(ENV_REQUIRE_SIGNATURES, v),
                    None => std::env::remove_var(ENV_REQUIRE_SIGNATURES),
                }
            }
        }
    }

    fn unsigned_envelope(from: &str, to: &str) -> Envelope {
        Envelope {
            from: PeerId(from.into()),
            to: PeerId(to.into()),
            payload: serde_json::json!("hello"),
            sig: String::new(),
            nonce: Uuid::new_v4(),
        }
    }

    /// Default behavior (migration window): unsigned envelopes are accepted
    /// and delivered with only a WARN log. This guards against accidental
    /// behavior change for un-upgraded peers.
    #[tokio::test]
    async fn unsigned_envelope_accepted_when_deny_unsigned_false() {
        let mut router = Router::with_deny_unsigned(100, 10, false);
        let (mut bob_session, hb) = make_session("bob");
        let (_, ha) = make_session("alice");
        router.register(ha).unwrap();
        router.register(hb).unwrap();
        grant_relay(&mut router, "alice");
        grant_relay(&mut router, "bob");

        let envelope = unsigned_envelope("alice", "bob");
        let nonce = envelope.nonce;

        let reply = router.route(&PeerId("alice".into()), ClientMessage::Send(envelope));
        assert!(
            matches!(reply, RelayMessage::Delivered { nonce: n } if n == nonce),
            "expected Delivered for unsigned envelope under migration window, got {reply:?}"
        );

        let raw = bob_session.rx.try_recv().expect("message not delivered");
        assert!(raw.contains("hello"));
    }

    /// New deny path: when `deny_unsigned == true`, unsigned envelopes are
    /// rejected with `SignatureRequired` before any rate-limit accounting
    /// or destination lookup runs.
    #[test]
    fn unsigned_envelope_rejected_when_deny_unsigned_true() {
        let mut router = Router::with_deny_unsigned(100, 10, true);
        let (_, ha) = make_session("alice");
        let (_, hb) = make_session("bob");
        router.register(ha).unwrap();
        router.register(hb).unwrap();
        grant_relay(&mut router, "alice");
        grant_relay(&mut router, "bob");

        let envelope = unsigned_envelope("alice", "bob");
        let reply = router.route(&PeerId("alice".into()), ClientMessage::Send(envelope));
        assert!(
            matches!(reply, RelayMessage::SignatureRequired { ref peer_id } if peer_id == &PeerId("alice".into())),
            "expected SignatureRequired with deny_unsigned=true, got {reply:?}"
        );
    }

    /// `Router::new` must honor `PHANTOM_RELAY_REQUIRE_SIGNATURES=1` and
    /// flip `deny_unsigned` to true at construction time.
    #[test]
    fn env_var_phantom_relay_require_signatures_enables_deny() {
        // Truthy "1": deny_unsigned must be set.
        {
            let _g = EnvGuard::set("1");
            let r = Router::new(100, 10);
            assert!(
                r.deny_unsigned(),
                "PHANTOM_RELAY_REQUIRE_SIGNATURES=1 must enable deny_unsigned"
            );
        }
        // Truthy "true": case-insensitive accepted.
        {
            let _g = EnvGuard::set("TRUE");
            let r = Router::new(100, 10);
            assert!(
                r.deny_unsigned(),
                "PHANTOM_RELAY_REQUIRE_SIGNATURES=TRUE must enable deny_unsigned"
            );
        }
        // Unset: migration-window default preserved.
        {
            let _g = EnvGuard::unset();
            let r = Router::new(100, 10);
            assert!(
                !r.deny_unsigned(),
                "unset env var must preserve migration-window default"
            );
        }
        // Other values are not truthy.
        {
            let _g = EnvGuard::set("0");
            let r = Router::new(100, 10);
            assert!(
                !r.deny_unsigned(),
                "PHANTOM_RELAY_REQUIRE_SIGNATURES=0 must not enable deny_unsigned"
            );
        }
    }
}
