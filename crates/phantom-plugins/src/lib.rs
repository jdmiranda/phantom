//! Phantom plugin system.
//!
//! This crate provides the full plugin lifecycle:
//!
//! - **manifest** — plugin metadata, permissions, hooks, and commands
//! - **host** — WASM runtime interface for executing plugins
//! - **registry** — local plugin registry and loader
//! - **builtins** — official plugin manifests that ship with Phantom
//! - **marketplace** — discovery, installation, and management of plugins

pub mod manifest;
pub mod host;
pub mod registry;
pub mod builtins;
pub mod marketplace;

pub use builtins::{get_official, official_plugins};
pub use host::{HookContext, HookEvent, HookResponse, MockRuntime, PluginRuntime};
pub use manifest::{
    CommandDef, HookType, Permission, PluginManifest, StatusBarDef, StatusBarPosition,
};
pub use marketplace::{Marketplace, MarketplaceListing};
pub use registry::{LoadedPlugin, PluginInfo, PluginRegistry};
