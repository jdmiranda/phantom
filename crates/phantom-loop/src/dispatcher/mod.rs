//! Concrete [`crate::runner::AgentDispatcher`] implementations.
//!
//! The trait itself lives at [`crate::runner::dispatcher::AgentDispatcher`].
//! This module ships the **production** implementation for C3 plus the
//! **headless driver** that makes `phantom loop run` actually drain the
//! spawn queue without booting the full `phantom-app` event loop:
//!
//! - [`substrate::SubstrateAgentDispatcher`] — backed by
//!   [`phantom_agents::composer_tools::SpawnSubagentQueue`]. It pushes a
//!   [`phantom_agents::composer_tools::SpawnSubagentRequest`] when
//!   `dispatch` is called and resolves the oneshot when the substrate fires
//!   a matching [`phantom_protocol::Event::AgentTaskComplete`].
//! - [`driver::SubstrateDriver`] — the CLI-side counterpart to the
//!   `App::update` queue drain. Spawns a tokio task that periodically
//!   drains the same queue, runs each request through a pluggable
//!   [`driver::SubstrateBackend`] (real Claude API by default,
//!   [`driver::MockSubstrateBackend`] in tests), and emits
//!   `Event::AgentTaskComplete` onto a tokio mpsc bus the
//!   [`SubstrateCompletionRouter`] subscribes to.

pub mod driver;
pub mod substrate;

pub use driver::{
    ChatBackedSubstrateBackend, DEFAULT_MAX_ROUNDS, DEFAULT_TICK_INTERVAL, MockSubstrateBackend,
    SubstrateBackend, SubstrateDriver,
};
pub use substrate::{SubstrateAgentDispatcher, SubstrateCompletionRouter};
