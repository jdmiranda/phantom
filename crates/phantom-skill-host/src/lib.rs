//! `phantom-skill-host` — runtime dylib loader for Phantom skill crates.
//!
//! # Design
//!
//! Hot-module reload in Rust requires crossing a dylib boundary safely.  The
//! standard Rust ABI is not stable across compilations, so the host and guest
//! must communicate through a **C ABI vtable** (`extern "C"` function
//! pointers).  This crate defines that vtable (`SemanticSkillVtable`) and
//! provides two modes of use:
//!
//! **Static path** (default — `hot-modules` feature not enabled or release
//! build):
//! `SkillHost::build_static()` wraps the monomorphised `SemanticParser` from
//! `phantom-semantic` directly.  Zero overhead.  Byte-identical behaviour to
//! today's direct calls.
//!
//! **Dynamic path** (`hot-modules` feature + debug build +
//! `PHANTOM_HOT_MODULES=1` env var):
//! `SkillHost::load()` opens the dylib, resolves `phantom_skill_register`,
//! calls it to obtain a `SemanticSkillVtable`, and wraps that in a
//! `DylibBacked` adapter that also holds the `Library` alive so no use-after-
//! free can occur.  A background watcher thread (see `watcher.rs`) posts
//! reload events; call `SkillHost::poll_reload()` from your update loop.
//!
//! # FFI safety contract
//!
//! The vtable function pointers obey `extern "C"` calling convention.  All
//! string parameters cross the boundary as `(*const u8, usize)` pairs — no
//! Rust `&str` references.  Return values are heap-allocated `*mut u8` (the
//! serialised JSON of [`ParsedOutput`][phantom_semantic::ParsedOutput]) with
//! length written to an out-parameter.  The caller is responsible for freeing
//! the allocation via `phantom_skill_free` in the same vtable.
//!
//! **The vtable pointer outlives the `Library`.** `SkillHost` holds the
//! `Arc<Library>` inside `DylibBacked` so the library is never unloaded while
//! references to the vtable exist. Phase 2 (#384) will add refcount-drain
//! quiescence before unloading the old library; for Phase 1 the old library is
//! intentionally leaked (~a few MB per reload) rather than risk a use-after-
//! free.
//!
//! # Forbid lints (mirroring epic rule: swappable crates must not call
//! `tokio::spawn`)
#![forbid(unsafe_op_in_unsafe_fn)]

pub mod ffi;
pub mod host;
pub mod loader;
pub mod watcher;

pub use host::{SemanticSkill, SkillHost};
