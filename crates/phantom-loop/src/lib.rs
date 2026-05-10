//! Loop overseer crate — types and spec parsing for repo-scoped autonomous loops.
//!
//! This is the C1 slice of issue [#650](https://github.com/jdmiranda/phantom/issues/650):
//! the type-system foundation for repo-pointable loops (PR-finder, Reviewer,
//! Implementer). Subsequent slices add:
//!
//! - **C2** — `LoopRunner` state machine, `LoopSource` trait implementations,
//!   `LoopQueueRegistry`, and `LoopEffect` execution.
//! - **C3** — `phantom loop` CLI subcommand.
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
//! Use [`load_spec`] (or [`parse_spec_str`] for in-memory specs) to obtain a
//! [`LoopSpec`] paired with its compiled [`ExitSchema`].

pub mod effect;
pub mod error;
pub mod exit;
pub mod id;
pub mod source;
pub mod spec;

pub use effect::{FieldMap, LoopEffect};
pub use error::LoopSpecError;
pub use exit::ExitSchema;
pub use id::LoopId;
pub use source::{GhPrPredicate, GhPrState, LoopSourceSpec};
pub use spec::{
    LoopAgentSpec, LoopPolicy, LoopQuarantinePolicy, LoopSpec, load_spec, parse_spec_str,
};
