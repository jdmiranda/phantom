//! `phantom-relay` library target.
//!
//! Exposes the relay internals for integration testing and for future embedding
//! in the Phantom supervisor or test harness.

pub mod envelope;
pub mod rate_limit;
pub mod router;
pub mod server;
pub mod session;
