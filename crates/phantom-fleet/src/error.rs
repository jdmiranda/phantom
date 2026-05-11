//! Fleet error type.
//!
//! All [`crate::run::run_fleet`] failures funnel through [`FleetError`]. The
//! variants intentionally stay coarse: the fleet does not need a different
//! caller-visible error shape per app kind — a friendly message is enough.

use thiserror::Error;

/// Errors produced by `phantom-fleet`.
#[derive(Debug, Error)]
pub enum FleetError {
    /// Failed to read the [`crate::FleetSpec`] TOML at the configured path.
    #[error("fleet config not readable at {path}: {source}")]
    ConfigRead {
        /// The path the loader tried to open.
        path: String,
        /// The underlying IO error.
        #[source]
        source: std::io::Error,
    },

    /// The TOML parsed but did not match the [`crate::FleetSpec`] shape.
    #[error("fleet config malformed: {0}")]
    ConfigParse(#[from] toml::de::Error),

    /// The fleet config validated structurally but the orchestrator could not
    /// honour it — e.g. an `AppKind::Builder` entry when `phantom-builder` is
    /// not compiled in.
    #[error("fleet config unsupported: {0}")]
    Unsupported(String),

    /// A bus or spawn task encountered an unrecoverable error.
    #[error("fleet runtime error: {0}")]
    Runtime(String),

    /// A passthrough for nested anyhow errors — used by the run-side helpers
    /// that already wrap their own failure modes in `anyhow::Error`.
    #[error("{0}")]
    Other(#[from] anyhow::Error),
}

/// Convenience result alias.
pub type FleetResult<T> = Result<T, FleetError>;
