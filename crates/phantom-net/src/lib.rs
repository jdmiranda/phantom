//! `phantom-net` — identity bootstrap, relay handshake, and opaque message
//! envelope for Phantom federation (Phase 9).
//!
//! # Overview
//!
//! Each Phantom instance has a stable [`identity::Identity`] backed by an
//! OS keyring (macOS Keychain / libsecret / wincred).  It connects to a
//! relay server over WebSocket, exchanges a handshake, and then sends and
//! receives signed [`envelope::Envelope`]s to peer instances.
//!
//! The [`client::RelayClient`] drives a heartbeat keepalive (see
//! [`heartbeat`]) that automatically reconnects with exponential back-off
//! when the relay is unreachable.
//!
//! # Upgrade path
//! The transport layer (`transport.rs`) wraps `tokio-tungstenite`.  Once a
//! relay server supports QUIC, the transport can be swapped for `quinn`
//! without changing the public API.
//!
//! # Modules
//! - [`identity`] — Ed25519 keypair + [`identity::PeerId`] + keyring persistence
//! - [`envelope`] — signed, nonce-stamped message envelope
//! - [`transport`] — WebSocket framing (QUIC upgrade path noted)
//! - [`client`]    — [`client::RelayClient`]: connect / send / recv / heartbeat
//! - [`heartbeat`] — ping/pong state machine with exponential back-off

pub mod client;
pub mod envelope;
pub mod heartbeat;
pub mod identity;
pub mod transport;

pub use client::RelayClient;
pub use envelope::Envelope;
pub use identity::{Identity, PeerId};

#[cfg(test)]
mod tests;
