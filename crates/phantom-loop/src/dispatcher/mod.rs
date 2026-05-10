//! Concrete [`crate::runner::AgentDispatcher`] implementations.
//!
//! The trait itself lives at [`crate::runner::dispatcher::AgentDispatcher`].
//! This module ships the **production** implementation for C3:
//!
//! - [`substrate::SubstrateAgentDispatcher`] — backed by
//!   [`phantom_agents::composer_tools::SpawnSubagentQueue`]. It pushes a
//!   [`phantom_agents::composer_tools::SpawnSubagentRequest`] when
//!   `dispatch` is called and resolves the oneshot when the substrate fires
//!   a matching [`phantom_protocol::Event::AgentTaskComplete`].

pub mod substrate;

pub use substrate::{SubstrateAgentDispatcher, SubstrateCompletionRouter};
