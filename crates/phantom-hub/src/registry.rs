//! Connection registry — tracks live Phantom WebSocket connections.
//!
//! # SCAFFOLD — issue #396 fills this in
//!
//! Phase 1: type definitions only. The registry holds no state and all
//! operations are unimplemented stubs. Issue #396 wires real insertion,
//! lookup, and eviction.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Opaque identifier for a connected Phantom instance.
///
/// Assigned at device registration time and persisted in the device-token
/// JWT. Issue #398 refines the identity model.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PhantomId(pub String);

impl std::fmt::Display for PhantomId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A live connection handle to a single Phantom instance.
///
/// SCAFFOLD: fields will be populated in issue #396 (WebSocket sink, metadata).
#[derive(Debug)]
pub struct PhantomHandle {
    pub id: PhantomId,
    // TODO(#396): add `ws_tx: tokio::sync::mpsc::Sender<String>` for outbound frames.
    // TODO(#396): add `connected_at: std::time::Instant`.
    // TODO(#396): add `labels: HashMap<String, String>` for fleet queries.
}

impl PhantomHandle {
    /// Create a new handle.
    ///
    /// SCAFFOLD: currently only stores the id. Real construction in issue #396.
    #[must_use]
    pub fn new(id: PhantomId) -> Self {
        Self { id }
    }
}

/// Shared registry of live Phantom connections.
///
/// SCAFFOLD: the inner map is present but no operations are implemented.
/// Issue #396 adds `register`, `unregister`, `get`, and `list`.
#[derive(Debug, Default)]
pub struct Registry {
    phantoms: HashMap<PhantomId, PhantomHandle>,
}

impl Registry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the number of currently registered Phantom instances.
    #[must_use]
    pub fn len(&self) -> usize {
        self.phantoms.len()
    }

    /// Return `true` when no Phantom instances are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.phantoms.is_empty()
    }
}

/// Convenience type alias for a thread-safe shared registry.
pub type SharedRegistry = Arc<Mutex<Registry>>;

/// Create a new empty [`SharedRegistry`].
#[must_use]
pub fn new_shared() -> SharedRegistry {
    Arc::new(Mutex::new(Registry::new()))
}
