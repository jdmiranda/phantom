//! Test fixtures shared between unit tests, integration tests, and any
//! downstream crate that needs to drive the dispatch boundary.
//!
//! The module is gated by either `#[cfg(test)]` (so internal unit tests pick
//! it up automatically) or the `test-support` Cargo feature (so integration
//! tests and downstream crates can opt in).  Production binaries never pull
//! this code in — the optional `tempfile` dep is only enabled by the
//! `test-support` feature.
//!
//! See issue #645: the dispatch boundary
//! ([`crate::dispatch::DispatchContext`], [`crate::chat_tools::ChatToolContext`],
//! [`crate::defender_tools::DefenderToolContext`]) now requires a non-`Option`
//! [`phantom_memory::event_log::EventLog`] handle. Every test site that used
//! to pass `event_log: None` must instead build a real (in-memory-ish) log.
//! [`log_fixture::fresh_log`] is the one-line canonical builder.

pub mod log_fixture;

pub use log_fixture::{LogFixture, fresh_log};
