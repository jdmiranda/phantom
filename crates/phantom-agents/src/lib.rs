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
pub mod audit;
pub mod lifecycle;
pub mod correlation;
pub mod policy;
pub mod handoff;
pub mod chat;
pub mod chat_tools;
pub mod cli;
pub mod composer_tools;
pub mod defender;
pub mod defender_tools;
pub mod dispatch;
pub mod dispatcher;
pub mod failure_store;
pub mod fixer;
pub mod inbox;
pub mod inspector;
pub mod manager;
pub mod mcp_tools;
pub mod peer_grants;
pub mod permissions;
pub mod plan;
pub mod quarantine;
pub mod render;
pub mod role;
pub mod router;
pub mod sandbox;
pub mod semantic_context;
pub mod speech;
pub mod spawn_rules;
pub mod suggest;
pub mod supervisor;
pub mod skill_registry;
pub mod system_prompt;
pub mod taint;
pub mod tools;

pub use agent::*;
pub use correlation::CorrelationId;
pub use dispatch::Disposition;
pub use policy::AgentPolicy;
pub use chat::{
    ChatBackend, ChatError, ChatModel, ChatRequest, ChatResponse, ClaudeBackend,
    OpenAiChatBackend, build_backend,
};
pub use failure_store::{FailureRecord, FailureStore};
pub use defender::defender_spawn_rule;
pub use manager::*;
pub use peer_grants::{PeerId, PeerGrants, PeerGrantRegistry};
pub use role::{
    AgentId as RoleAgentId, AgentRef, AgentRole, CapabilityClass, RoleManifest, SpawnSource,
};
pub use quarantine::{
    AgentRuntime, AutoQuarantinePolicy, QuarantineRegistry, QuarantineState,
    DEFAULT_QUARANTINE_THRESHOLD,
};
pub use lifecycle::{LifecycleEvent, LifecycleHook, LifecycleHooks};
pub use semantic_context::SemanticContext;
pub use skill_registry::SkillRegistry;
pub use system_prompt::SkillInjection;
pub use taint::TaintLevel;
pub use tools::*;
