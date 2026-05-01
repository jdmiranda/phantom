//! `phantom` library surface.
//!
//! The phantom binary is the primary product of this crate.  This `lib.rs`
//! exposes a small library face whose sole purpose is to make
//! integration-test fixtures importable — the binary's `main.rs` modules
//! cannot otherwise be reached from `tests/`.
//!
//! Concretely: `auth_cli` is re-exported so `tests/auth_cli.rs` can
//! cross-validate the registration payload against `phantom-hub`'s
//! verifier without a runtime HTTP round-trip.
//!
//! No other code in the workspace depends on this lib target — the GUI
//! and headless surfaces are entry points, not library consumers.

pub mod auth_cli;
