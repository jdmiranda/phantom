//! Structured error types for the Phantom plugin system.
//!
//! Using [`thiserror`] is not a dependency here; instead we hand-implement
//! [`std::error::Error`] so we stay within the crate's existing `anyhow`-only
//! dependency footprint.

use std::fmt;

/// Top-level error type for plugin host operations.
///
/// [`PluginError`] is returned (wrapped in [`anyhow::Error`] via `?`) from
/// [`crate::wasm_host::WasmHost::load`] and related entry points.  Callers
/// that need to distinguish sandbox violations from ordinary load failures can
/// downcast with [`anyhow::Error::downcast_ref::<PluginError>()`].
#[derive(Debug)]
pub enum PluginError {
    /// A WASM module requested a host import that the sandbox does not provide.
    ///
    /// This includes all WASI syscalls (`wasi_snapshot_preview1`, `wasi_unstable`,
    /// etc.) as well as any other import namespace not explicitly whitelisted by
    /// the host.
    ///
    /// # Fields
    /// - `namespace` — the import module string (e.g. `"wasi_snapshot_preview1"`)
    /// - `name`      — the import field string  (e.g. `"fd_write"`)
    UnsupportedImport {
        namespace: String,
        name: String,
    },

    /// A WASM module could not be compiled or instantiated for reasons other
    /// than an unsatisfied import (bad magic bytes, invalid opcodes, etc.).
    LoadFailure(String),
}

impl fmt::Display for PluginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PluginError::UnsupportedImport { namespace, name } => write!(
                f,
                "sandbox violation: WASM module imports `{namespace}::{name}` \
                 which is not provided by the Phantom plugin host"
            ),
            PluginError::LoadFailure(msg) => {
                write!(f, "WASM plugin load failure: {msg}")
            }
        }
    }
}

impl std::error::Error for PluginError {}
