//! `phantom-fleet` — the "app of apps" meta-orchestrator.
//!
//! One Phantom process can host N autonomous apps — typically builders, one
//! per target GitHub repo — all sharing the substrate's event bus, brain,
//! and substrate driver. This crate is the headless orchestrator that
//! consumes [`phantom_adapter`]'s `AppAdapter` abstraction to load a
//! [`FleetSpec`] from TOML and run every entry concurrently.
//!
//! # Mental model
//!
//! ```text
//! ┌─────────────── one Phantom process ───────────────┐
//! │                                                   │
//! │   FleetRunner                                     │
//! │      ├── LoopQueueRegistry        (Arc)           │
//! │      ├── SubstrateAgentDispatcher (shared)        │
//! │      ├── SubstrateDriver          (shared)        │
//! │      ├── BrainHandle              (shared)        │
//! │      │                                            │
//! │      └── N hosted AppAdapter tasks                │
//! │           ├── builder: jdmiranda/phantom          │
//! │           ├── builder: jdmiranda/badass-cli       │
//! │           └── loop:    /path/to/.phantom/loops    │
//! └───────────────────────────────────────────────────┘
//! ```
//!
//! # Quick start
//!
//! ```no_run
//! use phantom_fleet::{FleetSpec, run_fleet};
//!
//! # async fn main_inner() -> Result<(), phantom_fleet::FleetError> {
//! let spec = FleetSpec::load(std::path::Path::new("~/.phantom/fleet.toml"))?;
//! run_fleet(spec).await
//! # }
//! ```
//!
//! # phantom-builder integration
//!
//! [`AppKind::Builder`] entries are gated behind the optional `builder-apps`
//! Cargo feature. The fleet compiles cleanly without it; trying to *run* a
//! builder entry without the feature returns a clear
//! [`FleetError::Unsupported`] error pointing at the rebuild command.

pub mod app_kind;
pub mod error;
pub mod registry;
pub mod run;
pub mod spec;

// Re-export the public API at the crate root.
pub use app_kind::{AppKind, BuilderSpec, CustomAppSpec, LoopAppSpec};
pub use error::{FleetError, FleetResult};
pub use registry::{FleetAppHandle, FleetAppId, FleetAppSnapshot, FleetAppStatus, FleetRegistry};
pub use run::{
    AppFactoryResult, AppShutdown, BoxedLifecycle, CustomFactory, FleetContext, FleetRunner,
    builder_feature_enabled, run_fleet,
};
pub use spec::{FleetSpec, SharedFleetSettings};
