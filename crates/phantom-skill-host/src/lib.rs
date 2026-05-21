//! `phantom-skill-host` — runtime dylib loader for Phantom skill crates.
//!
//! # Design
//!
//! Hot-module reload in Rust requires crossing a dylib boundary safely.  The
//! standard Rust ABI is not stable across compilations, so the host and guest
//! must communicate through a **C ABI vtable** (`extern "C"` function
//! pointers).  This crate defines those vtables and provides two modes of use:
//!
//! **Static path** (default — `hot-modules` feature not enabled or release
//! build):
//! `SkillHost::build_static()` / `LlmHost::build_static()` wrap the
//! monomorphised implementations from `phantom-semantic` / `phantom-nlp`
//! directly.  Zero overhead.  Byte-identical behaviour to today's direct calls.
//!
//! **Dynamic path** (`hot-modules` feature + debug build +
//! `PHANTOM_HOT_MODULES=1` env var):
//! The loader opens the dylib, resolves the registration symbol, calls it to
//! obtain a vtable, and wraps that in a `DylibBacked*` adapter that also holds
//! the `Library` alive so no use-after-free can occur.  A background watcher
//! thread (see `watcher.rs`) posts reload events; call `SkillHost::poll_reload()`
//! from your update loop.
//!
//! # Skill targets
//!
//! | Crate              | Trait         | Factory       | Register symbol          |
//! |--------------------|---------------|---------------|--------------------------|
//! | `phantom-semantic` | `SemanticSkill` | `SkillHost` | `phantom_skill_register` |
//! | `phantom-nlp`      | `LlmSkill`    | `LlmHost`     | `phantom_llm_register`   |
//!
//! # FFI safety contract
//!
//! The vtable function pointers obey `extern "C"` calling convention.  All
//! string parameters cross the boundary as `(*const u8, usize)` pairs — no
//! Rust `&str` references.  The caller is responsible for freeing heap
//! allocations via the `free_buf` pointer in the same vtable.
//!
//! **The vtable pointer outlives the `Library`.** Each host type holds the
//! `Arc<Library>` inside its dylib-backed adapter so the library is never
//! unloaded while references to the vtable exist.  Phase 2 (#384) will add
//! refcount-drain quiescence before unloading the old library; for Phase 1 the
//! old library is intentionally leaked (~a few MB per reload) rather than risk
//! a use-after-free.
//!
//! # Forbid lints (mirroring epic rule: swappable crates must not call
//! `tokio::spawn`)
#![forbid(unsafe_op_in_unsafe_fn)]

pub(crate) mod drain_reaper;
pub mod ffi;
pub mod host;
pub mod llm_ffi;
pub mod llm_host;
pub mod llm_loader;
pub mod loader;
pub mod swap_manager;
pub mod watcher;

pub use drain_reaper::all_swap_states;
#[doc(hidden)]
pub use drain_reaper::tick_reaper_for_test;
pub use host::{SemanticSkill, SkillHost};
pub use llm_host::{LlmHost, LlmSkill, LlmSkillAdapter};
pub use swap_manager::{SwapManager, SwapState, SwapStatus, pending_swaps};
