//! Concrete [`crate::runner::LoopSource`] implementations.
//!
//! C2 ships two:
//!
//! - [`cron::CronSource`] — interval-driven; used by agentless poll loops
//!   like the PR-finder example from issue #650.
//! - [`queue::LoopMessageQueueSource`] — drains a named in-process queue
//!   from a [`crate::LoopQueueRegistry`]; used by Reviewer / Implementer
//!   loops downstream of the PR-finder.
//!
//! The GitHub-backed sources (`gh issue list`, `gh pr list`) are
//! deliberately deferred to C3 because they depend on the runtime decision
//! around how `phantom loop run` shells out to the `gh` CLI.

pub mod cron;
pub mod queue;

pub use cron::CronSource;
pub use queue::LoopMessageQueueSource;
