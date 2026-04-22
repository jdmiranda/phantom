//! Phantom agent runtime.
//!
//! This crate defines the core agent lifecycle: spawning, tool execution,
//! conversation management, and pool orchestration. It is consumed by:
//!
//! - `phantom-app` for creating/rendering agent panes
//! - The Claude API integration for driving agent reasoning
//! - The error-to-agent pipeline for auto-spawning fix agents
//! - The agent CLI for user-initiated agent spawns

pub mod agent;
pub mod api;
pub mod cli;
pub mod manager;
pub mod permissions;
pub mod render;
pub mod suggest;
pub mod tools;

pub use agent::*;
pub use manager::*;
pub use tools::*;
