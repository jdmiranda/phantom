//! Per-peer session state.

use std::time::Instant;

use tokio::sync::mpsc;
use uuid::Uuid;

use crate::envelope::PeerId;

/// A handle the router uses to forward messages to a connected peer.
///
/// Cloning is cheap — both ends share the same underlying channel.
#[derive(Debug, Clone)]
pub struct SessionHandle {
    /// Assigned peer identity.
    pub peer_id: PeerId,
    /// Session token issued during the handshake.
    pub token: Uuid,
    /// Sender side of the per-session outbound queue.
    pub tx: mpsc::Sender<String>,
}

/// Full session state owned by the per-connection task.
#[derive(Debug)]
pub struct Session {
    /// Assigned peer identity.
    pub peer_id: PeerId,
    /// Session token issued during the handshake.
    pub token: Uuid,
    /// Monotonic timestamp of the last heartbeat / message received.
    pub last_heartbeat: Instant,
    /// Receiver side of the per-session outbound queue.
    pub rx: mpsc::Receiver<String>,
    /// Sender side — kept here so the session can hand it to the router.
    pub tx: mpsc::Sender<String>,
}

impl Session {
    /// Create a new session for `peer_id`.
    #[must_use]
    pub fn new(peer_id: PeerId) -> Self {
        let token = Uuid::new_v4();
        let (tx, rx) = mpsc::channel::<String>(256);
        Self {
            peer_id,
            token,
            last_heartbeat: Instant::now(),
            rx,
            tx,
        }
    }

    /// Return a lightweight handle suitable for storage in the router.
    #[must_use]
    pub fn handle(&self) -> SessionHandle {
        SessionHandle {
            peer_id: self.peer_id.clone(),
            token: self.token,
            tx: self.tx.clone(),
        }
    }

    /// Update the last-heartbeat timestamp.
    pub fn touch(&mut self) {
        self.last_heartbeat = Instant::now();
    }
}
