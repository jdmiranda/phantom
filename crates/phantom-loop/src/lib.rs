//! Loop overseer crate — types, spec parsing, and runtime engine for
//! repo-scoped autonomous loops.
//!
//! This crate is delivered in three slices against issue
//! [#650](https://github.com/jdmiranda/phantom/issues/650):
//!
//! - **C1** (PR #658) — types + TOML parsing.
//! - **C2** (this PR) — `LoopRunner` state machine, `LoopSource` trait
//!   implementations, `LoopQueueRegistry`, and `LoopEffect` execution.
//! - **C3** — the `phantom loop` CLI subcommand and the real
//!   substrate-backed [`runner::AgentDispatcher`].
//!
//! # Spec format
//!
//! A loop is declared by a TOML file at `<repo>/.phantom/loops/<name>.toml`.
//! For example:
//!
//! ```toml
//! id = "reviewer"
//!
//! [agent]
//! role = "Actor"
//! allow_tools = ["read_file", "gh_pr_review", "gh_pr_merge"]
//! system_prompt = "Review the PR..."
//!
//! [agent.exit_schema]
//! type = "object"
//! required = ["pr_number", "decision"]
//!
//! [agent.exit_schema.properties.pr_number]
//! type = "integer"
//!
//! [agent.exit_schema.properties.decision]
//! enum = ["approved", "rejected", "needs_changes"]
//!
//! [source]
//! kind = "gh_pr"
//! repo = "jdmiranda/phantom"
//!
//! [source.predicate]
//! state = "open"
//! review_required = true
//! ```
//!
//! Use [`load_spec`] (or [`parse_spec_str`] for in-memory specs) to obtain
//! a [`LoopSpec`] paired with its compiled [`ExitSchema`].
//!
//! # Runtime
//!
//! The C2 runtime exposes:
//!
//! - [`runner::LoopRunner`] — async state machine that drives one spec.
//! - [`runner::LoopSource`] — pluggable input-side abstraction.
//! - [`runner::AgentDispatcher`] — pluggable agent-spawning abstraction.
//!   C2 ships only stub implementations; the real
//!   [`phantom_agents::composer_tools::new_spawn_subagent_queue`]-backed
//!   dispatcher lands in C3.
//! - [`queue::LoopQueueRegistry`] — in-process directory of named queues
//!   for cross-loop messaging.
//! - [`effect_runner::run_effects`] — the dispatcher for
//!   [`LoopEffect::EnqueueTo`], [`LoopEffect::LogToBus`], and
//!   [`LoopEffect::StopLoop`].
//!
//! And two concrete source implementations under [`sources`]:
//!
//! - [`sources::CronSource`] — interval-driven for agentless poll loops.
//! - [`sources::LoopMessageQueueSource`] — drains a named cross-loop queue.
//!
//! The GitHub-backed `GhIssueQueueSource` / `GhPrReviewQueueSource` are
//! deferred to C3, alongside the CLI wiring.

pub mod action_handler;
pub mod dispatcher;
pub mod effect;
pub mod effect_runner;
pub mod error;
pub mod exit;
pub mod id;
pub mod preflight;
pub mod queue;
pub mod registry;
pub mod runner;
pub mod source;
pub mod sources;
pub mod spec;

pub use action_handler::{LoopQueueActionHandler, NoopInner};
pub use dispatcher::{
    ChatBackedSubstrateBackend, DEFAULT_MAX_ROUNDS, DEFAULT_TICK_INTERVAL, MockSubstrateBackend,
    SubstrateAgentDispatcher, SubstrateBackend, SubstrateCompletionRouter, SubstrateDriver,
};
pub use effect::{FieldMap, LoopEffect};
pub use effect_runner::{EffectContext, EffectError, EffectOutcome, run_effects};
pub use error::LoopSpecError;
pub use exit::ExitSchema;
pub use id::LoopId;
pub use preflight::{PreflightError, RunLock, check_gh_auth, check_gh_binary, check_mcp_collisions};
pub use queue::{LoopMessage, LoopQueue, LoopQueueRegistry};
pub use registry::{LoopHandle, LoopRegistry, LoopRegistryError, LoopSnapshot, LoopStatus};
pub use runner::{
    AgentDispatcher, CorrelationId, DispatchError, DispatchHandle, LoopContext, LoopInput,
    LoopPullResult, LoopRunner, LoopSource, LoopSourceError, LoopState,
};
pub use source::{GhPrPredicate, GhPrState, LoopSourceSpec};
pub use sources::{
    CronSource, GhIssueQueueSource, GhPrReviewQueueSource, LoopMessageQueueSource,
};
pub use spec::{
    LoopAgentSpec, LoopPolicy, LoopQuarantinePolicy, LoopSpec, load_spec, parse_spec_str,
};
