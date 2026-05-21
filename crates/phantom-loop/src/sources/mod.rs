//! Concrete [`crate::runner::LoopSource`] implementations.
//!
//! C2 shipped two:
//!
//! - [`cron::CronSource`] — interval-driven; used by agentless poll loops
//!   like the PR-finder example from issue #650.
//! - [`queue::LoopMessageQueueSource`] — drains a named in-process queue
//!   from a [`crate::LoopQueueRegistry`]; used by Reviewer / Implementer
//!   loops downstream of the PR-finder.
//!
//! C3 adds two GitHub-backed sources:
//!
//! - [`gh_issues::GhIssueQueueSource`] — shells `gh issue list -R …`.
//! - [`gh_pr::GhPrReviewQueueSource`] — shells `gh pr list -R …`.
//!
//! Both dedupe in-memory and stream one fresh result per `next()` call.

pub mod cron;
pub mod gh_issues;
pub mod gh_pr;
pub mod queue;

pub use cron::CronSource;
pub use gh_issues::{GhBinaryRunner, GhCommandRunner, GhIssueQueueSource};
pub use gh_pr::GhPrReviewQueueSource;
pub use queue::LoopMessageQueueSource;
