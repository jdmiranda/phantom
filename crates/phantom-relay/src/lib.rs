//! `phantom-relay` library target.
//!
//! Exposes the relay internals for integration testing and for future embedding
//! in the Phantom supervisor or test harness.
//!
//! The [`agent_route`] module adds the agent-addressing layer on top of the
//! peer-level broker: [`agent_route::AgentEnvelope`] and
//! [`agent_route::route_agent_envelope`] let callers route messages to a
//! specific agent by [`agent_route::AgentTarget`] (local or remote) without
//! knowing the peer topology directly.

pub mod agent_route;
pub mod envelope;
pub mod rate_limit;
pub mod router;
pub mod server;
pub mod session;
