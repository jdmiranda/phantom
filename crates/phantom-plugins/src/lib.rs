//! Phantom plugin system.
//!
//! This crate provides the full plugin lifecycle:
//!
//! - **manifest** — plugin metadata, permissions, hooks, and commands
//! - **host** — WASM runtime interface for executing plugins
//! - **wasm_host** — real wasmtime-backed WASM host ([`WasmHost`] / [`WasmRuntime`])
//! - **registry** — local plugin registry and loader
//! - **builtins** — official plugin manifests that ship with Phantom
//! - **marketplace** — discovery, installation, and management of plugins

pub mod manifest;
pub mod host;
pub mod wasm_host;
pub mod registry;
pub mod builtins;
pub mod marketplace;
pub mod script;

pub use builtins::{get_official, official_plugins};
pub use host::{HookContext, HookEvent, HookResponse, MockRuntime, PluginRuntime};
pub use manifest::{
    CommandDef, HookType, Permission, PluginManifest, StatusBarDef, StatusBarPosition,
};
pub use marketplace::{Marketplace, MarketplaceListing};
pub use registry::{LoadedPlugin, PluginInfo, PluginRegistry};
pub use script::ScriptRuntime;
pub use wasm_host::{SandboxViolation, WasmHost, WasmRuntime};
