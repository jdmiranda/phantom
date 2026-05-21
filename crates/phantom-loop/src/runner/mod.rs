//! Runtime engine for one [`crate::LoopSpec`].
//!
//! The runner pulls work from a [`source::LoopSource`], dispatches the
//! per-iteration agent through an [`dispatcher::AgentDispatcher`], validates
//! the result against the spec's [`crate::ExitSchema`], and fires any
//! [`crate::LoopEffect`]s.
//!
//! Public surface:
//!
//! - [`fsm::LoopRunner`] — the state machine and its driver.
//! - [`fsm::LoopState`] — the state enum.
//! - [`source::LoopSource`] / [`source::LoopPullResult`] / [`source::LoopInput`] —
//!   the source-side trait and types.
//! - [`dispatcher::AgentDispatcher`] / [`dispatcher::DispatchHandle`] /
//!   [`dispatcher::DispatchError`] — the agent-spawn abstraction. The real
//!   substrate-backed impl lands in C3.

pub mod dispatcher;
pub mod fsm;
pub mod source;

pub use dispatcher::{AgentDispatcher, DispatchError, DispatchHandle};
pub use fsm::{LoopRunner, LoopState};
pub use source::{
    CorrelationId, LoopContext, LoopInput, LoopPullResult, LoopSource, LoopSourceError,
};
