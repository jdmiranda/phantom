//! Connection registry — tracks live Phantom WebSocket connections.
//!
//! [`ConnectionRegistry`] is the single source of truth for which Phantom
//! instances are currently connected to the hub. It is wrapped in an
//! `Arc<RwLock<…>>` and shared across all request handlers.
//!
//! # Concurrency model
//!
//! Reads (listing, routing) vastly outnumber writes (register/unregister), so
//! a [`tokio::sync::RwLock`] is the right primitive. The write lock is held
//! only for the duration of a map mutation; it is never held across an `await`
//! of an I/O operation.
//!
//! # Disconnect safety
//!
//! When the WebSocket task calls [`ConnectionRegistry::unregister`] it receives
//! back the entire [`ConnState`]. The caller is then responsible for dropping
//! `ConnState.pending`, which closes every in-flight [`tokio::sync::oneshot`]
//! sender and causes waiting [`router::forward`] calls to return
//! [`crate::router::RouteError::Disconnected`].

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use ed25519_dalek::VerifyingKey;
use tokio::sync::{mpsc, oneshot, RwLock};

use crate::peer_key_store::PeerKeyStore;
use crate::router::{JsonRpcRequest, JsonRpcResponse};

// ---------------------------------------------------------------------------
// PhantomId
// ---------------------------------------------------------------------------

/// Opaque identifier for a connected Phantom instance.
///
/// The shape matches `phantom_agents::peer_routing::PeerId` —
/// a transparent `String` newtype — so string values are interchangeable
/// without a crate dependency on `phantom-agents`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct PhantomId(pub String);

impl PhantomId {
    /// Construct a `PhantomId` from any string-like value.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

impl std::fmt::Display for PhantomId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// ---------------------------------------------------------------------------
// HubId — per-call correlation handle
// ---------------------------------------------------------------------------

/// Hub-local request identifier.
///
/// Generated as a monotonic counter per connection; unique within the hub's
/// in-flight table. The hub rewrites the original Claude-side `req.id` to
/// this before forwarding, then rewrites back when the response arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HubId(pub u64);

// ---------------------------------------------------------------------------
// ConnState
// ---------------------------------------------------------------------------

/// Live state for a single Phantom WebSocket connection.
pub struct ConnState {
    /// Sender half of the outbound mpsc channel.
    ///
    /// The WebSocket write task drains this channel and serialises each
    /// `JsonRpcRequest` as a text frame. Capacity is bounded at
    /// [`OUTBOUND_CHANNEL_CAPACITY`] frames; a full channel is surfaced to the
    /// caller as [`crate::router::RouteError::Backpressure`].
    pub(crate) tx: mpsc::Sender<JsonRpcRequest>,

    /// In-flight request table: `hub_id → reply sender`.
    ///
    /// The router inserts an entry before forwarding; the inbound task removes
    /// it when the matching response arrives and completes the oneshot.
    /// Dropped en-masse when the connection is removed from the registry.
    ///
    /// `pub(crate)` — callers outside `phantom-hub` must not write to this
    /// map directly (issue #500).  Integration tests use
    /// [`ConnState::insert_pending_for_test`].
    pub(crate) pending: HashMap<HubId, oneshot::Sender<JsonRpcResponse>>,

    /// Hub-local nonce counter for this connection.
    pub(crate) next_hub_id: u64,

    /// Timestamp of the most recent inbound frame (used by `list_online` to
    /// filter stale entries).
    pub(crate) last_seen: Instant,

    /// Remote host string (IP or hostname) for diagnostics.
    pub(crate) host: String,

    /// Phantom client version string from the registration frame.
    pub(crate) version: String,
}

impl ConnState {
    /// Allocate a fresh hub-local request id.
    pub fn alloc_hub_id(&mut self) -> HubId {
        let id = HubId(self.next_hub_id);
        self.next_hub_id += 1;
        id
    }

    /// Public read-only accessor for the remote host string.
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Public read-only accessor for the last-seen timestamp.
    #[must_use]
    pub fn last_seen(&self) -> Instant {
        self.last_seen
    }

    /// Insert a oneshot sender into the in-flight table.
    ///
    /// This method exists so that integration tests (which compile as a
    /// separate crate and cannot see `pub(crate)` fields) can exercise the
    /// disconnect path without exposing the raw `HashMap` as `pub`.
    ///
    /// It MUST NOT be called from production code paths — use
    /// [`crate::router::forward`] instead, which inserts into `pending` as
    /// part of the request-correlation protocol.
    ///
    /// Gated behind the `"testing"` cargo feature so it is excluded from
    /// release builds unless explicitly opted-in.
    #[cfg(any(test, feature = "testing"))]
    pub fn insert_pending_for_test(
        &mut self,
        hub_id: HubId,
        sender: oneshot::Sender<JsonRpcResponse>,
    ) {
        self.pending.insert(hub_id, sender);
    }
}

/// Maximum queued outbound frames per Phantom connection.
pub const OUTBOUND_CHANNEL_CAPACITY: usize = 64;

/// How long a connection can be silent before it is treated as offline by
/// [`ConnectionRegistry::list_online`].
pub const STALE_THRESHOLD_SECS: u64 = 60;

// ---------------------------------------------------------------------------
// ConnectionRegistry
// ---------------------------------------------------------------------------

/// Shared registry of live Phantom connections.
///
/// Wrap in [`SharedRegistry`] for multi-task access.
///
/// # Public-key registry (issue #527)
///
/// In addition to the live-connection map, the registry owns a
/// [`PeerKeyStore`] that persists each Phantom's Ed25519 verifying key under
/// the user's config directory. Calls that mutate or query peer keys are
/// thin pass-throughs to the store — the registry does not duplicate the
/// in-memory map. Persistence means the public-key registry survives a hub
/// restart even though the live-connection map does not.
pub struct ConnectionRegistry {
    conns: HashMap<PhantomId, ConnState>,
    /// Persistent file-backed Ed25519 verifying-key store (issue #527).
    ///
    /// Replaces the previous in-memory `HashMap<PhantomId, VerifyingKey>` —
    /// see [`PeerKeyStore`] for the storage layout, atomic-write strategy,
    /// and per-process cache.
    peer_keys: PeerKeyStore,
}

impl ConnectionRegistry {
    /// Create an empty registry, opening the persistent peer-key store.
    ///
    /// # Errors
    /// Returns an error if the peer-key store cannot be opened (config
    /// directory unavailable, or an existing file is malformed). On a fresh
    /// install with no peer-keys file yet, [`PeerKeyStore::open`] returns an
    /// empty store and this constructor succeeds — first-boot behaviour is
    /// not an error.
    pub fn new() -> Result<Self> {
        Ok(Self {
            conns: HashMap::new(),
            peer_keys: PeerKeyStore::open()?,
        })
    }

    /// Create a registry from an explicit [`PeerKeyStore`].
    ///
    /// Useful for tests that want to point the registry at a tmp file via
    /// `PHANTOM_PEER_KEYS_FILE`, or for callers that need to share a single
    /// store across multiple registries.
    #[must_use]
    pub fn with_peer_key_store(peer_keys: PeerKeyStore) -> Self {
        Self {
            conns: HashMap::new(),
            peer_keys,
        }
    }

    /// Register a new Phantom connection.
    ///
    /// Returns `Err` if a connection with the same `id` is already registered.
    ///
    /// # Arguments
    /// * `id` — the Phantom's stable identity from the registration frame.
    /// * `tx` — the outbound channel sender; the caller spawns the writer task.
    /// * `host` — peer address string for diagnostics.
    /// * `version` — Phantom version string from the registration frame.
    pub fn register(
        &mut self,
        id: PhantomId,
        tx: mpsc::Sender<JsonRpcRequest>,
        host: String,
        version: String,
    ) -> Result<(), RegistryError> {
        if self.conns.contains_key(&id) {
            return Err(RegistryError::AlreadyRegistered(id));
        }
        self.conns.insert(
            id,
            ConnState {
                tx,
                pending: HashMap::new(),
                next_hub_id: 0,
                last_seen: Instant::now(),
                host,
                version,
            },
        );
        Ok(())
    }

    /// Remove a connection and return its `ConnState` so the caller can
    /// cancel in-flight requests by dropping it.
    ///
    /// Returns `None` if the id was not registered.
    pub fn unregister(&mut self, id: &PhantomId) -> Option<ConnState> {
        self.conns.remove(id)
    }

    /// Return a mutable reference to a live connection, or `None`.
    pub fn get_mut(&mut self, id: &PhantomId) -> Option<&mut ConnState> {
        self.conns.get_mut(id)
    }

    /// Snapshot of all Phantom IDs that have been seen within
    /// [`STALE_THRESHOLD_SECS`].
    #[must_use]
    pub fn list_online(&self) -> Vec<OnlinePhantom> {
        let threshold = std::time::Duration::from_secs(STALE_THRESHOLD_SECS);
        self.conns
            .iter()
            .filter(|(_, s)| s.last_seen.elapsed() < threshold)
            .map(|(id, s)| OnlinePhantom {
                id: id.clone(),
                host: s.host.clone(),
                version: s.version.clone(),
                last_seen_secs_ago: s.last_seen.elapsed().as_secs(),
            })
            .collect()
    }

    /// Number of currently registered connections (including potentially stale
    /// ones that have not yet been cleaned up).
    #[must_use]
    pub fn len(&self) -> usize {
        self.conns.len()
    }

    /// Returns `true` when the registry is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.conns.is_empty()
    }

    /// Update the `last_seen` timestamp for a connection.
    pub fn touch(&mut self, id: &PhantomId) {
        if let Some(state) = self.conns.get_mut(id) {
            state.last_seen = Instant::now();
        }
    }

    // -----------------------------------------------------------------------
    // Persistent peer-key store accessors (issue #527)
    //
    // These are thin pass-throughs to the underlying [`PeerKeyStore`] so
    // callers don't need to thread the store separately.  Every write goes
    // straight to disk via the store's atomic-write path; the per-process
    // cache lives inside the store itself.
    // -----------------------------------------------------------------------

    /// Look up the persisted Ed25519 verifying key for `id`, if any.
    ///
    /// Returns `Ok(None)` when no key has ever been registered for `id`.
    pub fn peer_key(&self, id: &PhantomId) -> Result<Option<VerifyingKey>> {
        self.peer_keys.get(id)
    }

    /// Insert (or overwrite) the persisted Ed25519 verifying key for `id`.
    ///
    /// Atomically writes the full peer-key map to disk before returning.
    pub fn insert_peer_key(&self, id: PhantomId, key: VerifyingKey) -> Result<()> {
        self.peer_keys.insert(id, key)
    }

    /// Remove the persisted Ed25519 verifying key for `id`.
    pub fn remove_peer_key(&self, id: &PhantomId) -> Result<()> {
        self.peer_keys.remove(id)
    }

    /// Borrow the underlying [`PeerKeyStore`] handle.
    ///
    /// Mostly useful for cloning the cheap `Arc`-backed handle into a
    /// background task.
    #[must_use]
    pub fn peer_key_store(&self) -> &PeerKeyStore {
        &self.peer_keys
    }
}

// ---------------------------------------------------------------------------
// OnlinePhantom — list_online snapshot entry
// ---------------------------------------------------------------------------

/// A lightweight snapshot of a connected Phantom instance.
#[derive(Debug, Clone)]
pub struct OnlinePhantom {
    pub id: PhantomId,
    pub host: String,
    pub version: String,
    pub last_seen_secs_ago: u64,
}

// ---------------------------------------------------------------------------
// RegistryError
// ---------------------------------------------------------------------------

/// Errors returned by registry operations.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("phantom {0} is already registered")]
    AlreadyRegistered(PhantomId),
}

// ---------------------------------------------------------------------------
// SharedRegistry
// ---------------------------------------------------------------------------

/// Thread-safe shared registry handle.
pub type SharedRegistry = Arc<RwLock<ConnectionRegistry>>;

/// Create a new empty [`SharedRegistry`] backed by the on-disk peer-key store.
///
/// # Errors
/// Returns an error if the peer-key store cannot be opened — see
/// [`ConnectionRegistry::new`] for the conditions under which that happens.
pub fn new_shared() -> Result<SharedRegistry> {
    Ok(Arc::new(RwLock::new(ConnectionRegistry::new()?)))
}

/// Create a new empty [`SharedRegistry`] backed by an explicit
/// [`PeerKeyStore`].
///
/// Used by tests that point the store at a tmp file via
/// `PHANTOM_PEER_KEYS_FILE`, and by integration tests that share one store
/// across multiple registries.
#[must_use]
pub fn new_shared_with_store(peer_keys: PeerKeyStore) -> SharedRegistry {
    Arc::new(RwLock::new(ConnectionRegistry::with_peer_key_store(
        peer_keys,
    )))
}

/// Create a [`SharedRegistry`] for tests, pointing the persistent peer-key
/// store at a unique tmp file so each test is isolated from every other.
///
/// The on-disk file is created lazily on the first peer-key write — most
/// tests never trigger it.  This helper is always compiled (rather than
/// gated behind the `testing` feature) because it has no production-mode
/// failure modes: each call uses a fresh tmp filename and cleans up after
/// itself.  Production code paths use [`new_shared`] instead.
#[must_use]
pub fn new_shared_for_tests() -> SharedRegistry {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    // Serialise env mutation across concurrent calls — the env var is set
    // just long enough for `PeerKeyStore::open` to capture the path, then
    // cleared so it does not leak into other code paths.  We share the
    // crate-level `peer_key_store::ENV_SERIAL` mutex so that unit tests in
    // `peer_key_store` and registry-side helpers never interleave their
    // env mutations.
    let _serial = crate::peer_key_store::ENV_SERIAL
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let path = std::env::temp_dir().join(format!(
        "phantom-hub-test-shared-{}-{}.json",
        std::process::id(),
        n
    ));

    // SAFETY: env mutation is serialised via the crate-level ENV_SERIAL.
    unsafe { std::env::set_var("PHANTOM_PEER_KEYS_FILE", &path) };
    let store = PeerKeyStore::open().expect("test peer-key store must open");
    unsafe { std::env::remove_var("PHANTOM_PEER_KEYS_FILE") };
    // Best-effort cleanup of any file created by the open.  Tests that need
    // persistent storage open their own store via `PHANTOM_PEER_KEYS_FILE`.
    let _ = std::fs::remove_file(&path);

    new_shared_with_store(store)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn make_tx() -> mpsc::Sender<JsonRpcRequest> {
        mpsc::channel(8).0
    }

    fn make_id(s: &str) -> PhantomId {
        PhantomId::new(s)
    }

    /// Per-test counter so each test gets a unique peer-keys tmp path.
    static REG_TEST_COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// Build a registry pointed at an isolated peer-keys tmp file so the
    /// test does not touch the user's real config dir.  The file is left
    /// behind on disk; tests that care about cleanup pass an explicit
    /// `CleanupGuard`.
    fn make_registry() -> ConnectionRegistry {
        // Serialise env mutation via the crate-level shared mutex.  Without
        // this, two concurrent `make_registry` calls (or one concurrent with
        // a `peer_key_store` unit test) could race on the env var and the
        // wrong path would be captured by `PeerKeyStore::open` (see PR #640
        // review).
        let _serial = crate::peer_key_store::ENV_SERIAL
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let n = REG_TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        let path: PathBuf =
            std::env::temp_dir().join(format!("phantom-hub-registry-test-{pid}-{n}.json"));
        // SAFETY: env mutation is serialised via the crate-level ENV_SERIAL.
        unsafe { std::env::set_var("PHANTOM_PEER_KEYS_FILE", &path) };
        let reg = ConnectionRegistry::new().expect("registry new must succeed in tests");
        // Best-effort cleanup of the env var so it doesn't leak into other
        // tests; the store has already captured the path.
        unsafe { std::env::remove_var("PHANTOM_PEER_KEYS_FILE") };
        let _ = std::fs::remove_file(&path);
        reg
    }

    #[test]
    fn register_inserts_and_len_reflects_it() {
        let mut reg = make_registry();
        reg.register(make_id("phantom-a"), make_tx(), "host-a".into(), "0.1.0".into())
            .unwrap();
        assert_eq!(reg.len(), 1);
        assert!(!reg.is_empty());
    }

    #[test]
    fn duplicate_registration_fails() {
        let mut reg = make_registry();
        reg.register(make_id("dup"), make_tx(), "h".into(), "v".into())
            .unwrap();
        let err = reg.register(make_id("dup"), make_tx(), "h".into(), "v".into());
        assert!(err.is_err());
    }

    #[test]
    fn unregister_returns_state_and_removes_entry() {
        let mut reg = make_registry();
        reg.register(make_id("gone"), make_tx(), "h".into(), "v".into())
            .unwrap();
        let state = reg.unregister(&make_id("gone"));
        assert!(state.is_some());
        assert_eq!(reg.len(), 0);
    }

    #[test]
    fn unregister_unknown_returns_none() {
        let mut reg = make_registry();
        assert!(reg.unregister(&make_id("ghost")).is_none());
    }

    #[test]
    fn list_online_returns_registered_peers() {
        let mut reg = make_registry();
        reg.register(make_id("alpha"), make_tx(), "host1".into(), "1.0.0".into())
            .unwrap();
        reg.register(make_id("beta"), make_tx(), "host2".into(), "1.0.1".into())
            .unwrap();

        let online = reg.list_online();
        assert_eq!(online.len(), 2);
        let ids: Vec<_> = online.iter().map(|o| o.id.0.as_str()).collect();
        assert!(ids.contains(&"alpha"));
        assert!(ids.contains(&"beta"));
    }

    #[test]
    fn alloc_hub_id_is_monotonic() {
        let mut reg = make_registry();
        reg.register(make_id("p"), make_tx(), "h".into(), "v".into())
            .unwrap();
        let state = reg.get_mut(&make_id("p")).unwrap();
        let id0 = state.alloc_hub_id();
        let id1 = state.alloc_hub_id();
        let id2 = state.alloc_hub_id();
        assert_eq!(id0.0, 0);
        assert_eq!(id1.0, 1);
        assert_eq!(id2.0, 2);
    }

    #[test]
    fn touch_updates_last_seen() {
        let mut reg = make_registry();
        reg.register(make_id("t"), make_tx(), "h".into(), "v".into())
            .unwrap();
        // Touching twice should not panic and the entry stays online.
        reg.touch(&make_id("t"));
        reg.touch(&make_id("t"));
        assert_eq!(reg.list_online().len(), 1);
    }

    /// Issue #527: peer-key writes must be visible through the registry's
    /// pass-through accessors.
    #[test]
    fn peer_key_round_trip_via_registry() {
        use ed25519_dalek::SigningKey;
        use rand::rngs::OsRng;

        let reg = make_registry();
        let id = make_id("keyholder");
        let key = SigningKey::generate(&mut OsRng).verifying_key();

        reg.insert_peer_key(id.clone(), key)
            .expect("insert_peer_key must succeed");
        let got = reg
            .peer_key(&id)
            .expect("peer_key must succeed")
            .expect("must be present");
        assert_eq!(got.as_bytes(), key.as_bytes());

        reg.remove_peer_key(&id).expect("remove_peer_key");
        assert!(
            reg.peer_key(&id)
                .expect("peer_key after remove")
                .is_none(),
            "remove_peer_key must evict the entry"
        );
    }
}
