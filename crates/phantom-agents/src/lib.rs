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
pub mod chat;
pub mod chat_tools;
pub mod cli;
pub mod composer_tools;
pub mod correlation;
pub mod dag_explorer;
pub mod defender;
pub mod defender_tools;
pub mod dispatch;
pub mod dispatcher;
pub mod failure_store;
pub mod fixer;
pub mod handoff;
pub mod inbox;
pub mod inspector;
pub mod lifecycle;
pub mod manager;
pub mod mcp_tools;
pub mod peer_grants;
pub mod peer_routing;
pub mod permissions;
pub mod plan;
pub mod policy;
pub mod quarantine;
pub mod render;
pub mod role;
pub mod router;
pub mod sandbox;
pub mod self_extension_tools;
pub mod semantic_context;
pub mod skill_registry;
pub mod spawn_rules;
pub mod speech;
pub mod suggest;
pub mod supervisor;
pub mod system_prompt;
pub mod taint;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
pub mod tools;

pub use agent::*;
pub use chat::{
    ChatBackend, ChatError, ChatModel, ChatRequest, ChatResponse, ClaudeBackend,
    OpenAiChatBackend, PrivacyGuard, build_backend, build_backend_with_privacy,
};
pub use correlation::CorrelationId;
pub use defender::defender_spawn_rule;
pub use dispatch::Disposition;
pub use failure_store::{FailureRecord, FailureStore};
pub use lifecycle::{LifecycleEvent, LifecycleHook, LifecycleHooks};
pub use manager::*;
pub use peer_grants::{PeerGrants, PeerGrantRegistry};
pub use peer_routing::{
    AgentRouter, AnyAgentRef, PeerId, RemoteAgentInfo, RemoteInboxMessage, RemoteMessageContent,
    RemoteRouteError, decode_inbound,
};
pub use policy::AgentPolicy;
pub use quarantine::{
    AgentRuntime, AutoQuarantinePolicy, DEFAULT_QUARANTINE_THRESHOLD, QuarantineRegistry,
    QuarantineState,
};
pub use role::{
    AgentId as RoleAgentId, AgentRef, AgentRole, CapabilityClass, RoleManifest, SpawnSource,
};
pub use semantic_context::SemanticContext;
pub use skill_registry::SkillRegistry;
pub use system_prompt::SkillInjection;
pub use taint::TaintLevel;
pub use tools::*;
