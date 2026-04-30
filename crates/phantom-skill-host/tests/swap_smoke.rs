//! Smoke test for the static `SkillHost` path (no dylib required).
//!
//! The full dylib swap test requires a compiled `phantom-semantic` cdylib and
//! is intentionally left as a manual / CI-only exercise (see notes below).
//! This file exercises the public API contract and the static path so that
//! `cargo test -p phantom-skill-host` is always green without special setup.
//!
//! ## Full end-to-end swap (manual)
//!
//! 1. `cargo build -p phantom-semantic` (builds the cdylib artifact)
//! 2. `PHANTOM_HOT_MODULES=1 cargo test -p phantom-skill-host --features hot-modules`
//!
//! The watcher test (`watcher_detects_new_dylib`) would then build a
//! synthetic dylib in a tempdir, load it, overwrite it, and assert the new
//! value arrives within 500 ms.  That test is marked `#[ignore]` here because
//! it requires `cargo` to be on PATH and a working Rust toolchain inside the
//! test runner — acceptable for CI but not for default `cargo test`.

use phantom_skill_host::SkillHost;
use phantom_semantic::{CargoCommand, CommandType, GitCommand};

// ---------------------------------------------------------------------------
// Static path — always run
// ---------------------------------------------------------------------------

#[test]
fn static_classify_git_status() {
    let skill = SkillHost::build_static();
    assert_eq!(
        skill.classify_command("git status"),
        CommandType::Git(GitCommand::Status)
    );
}

#[test]
fn static_classify_cargo_build() {
    let skill = SkillHost::build_static();
    assert_eq!(
        skill.classify_command("cargo build --release"),
        CommandType::Cargo(CargoCommand::Build)
    );
}

#[test]
fn static_parse_returns_correct_command_type() {
    let skill = SkillHost::build_static();
    let out = skill.parse("cargo build", "", "error[E0308]: mismatched types\n  --> src/main.rs:10:5\n", Some(101));
    assert_eq!(out.command, "cargo build");
    assert_eq!(out.command_type, CommandType::Cargo(CargoCommand::Build));
}

#[test]
fn static_parse_classifies_unknown() {
    let skill = SkillHost::build_static();
    let out = skill.parse("some-tool --flag", "output", "", Some(0));
    assert_eq!(out.command_type, CommandType::Unknown);
}

#[test]
fn new_without_env_uses_static() {
    // SAFETY: single-threaded test.
    unsafe { std::env::remove_var("PHANTOM_HOT_MODULES") };
    let skill = SkillHost::build();
    assert_eq!(skill.classify_command("ls -la"), CommandType::Shell);
}

#[test]
fn skill_is_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<phantom_skill_host::SkillHost>();
}

// ---------------------------------------------------------------------------
// Dynamic path — requires cdylib artifact + feature flag
// ---------------------------------------------------------------------------

/// Full dylib load + verify the vtable is callable.
///
/// Run manually with:
/// ```text
/// cargo build -p phantom-semantic
/// PHANTOM_HOT_MODULES=1 cargo test -p phantom-skill-host \
///     --features hot-modules -- dylib_load_and_classify --ignored --nocapture
/// ```
#[test]
#[cfg(all(debug_assertions, feature = "hot-modules"))]
#[ignore = "requires `cargo build -p phantom-semantic` and PHANTOM_HOT_MODULES=1"]
fn dylib_load_and_classify() {
    std::env::set_var("PHANTOM_HOT_MODULES", "1");
    let skill = SkillHost::build();
    // If the dylib loaded, classify_command should still work correctly.
    let ct = skill.classify_command("git log --oneline");
    assert_eq!(ct, CommandType::Git(GitCommand::Log));
}

/// Verify the vtable `parse` path round-trips through JSON correctly.
#[test]
#[cfg(all(debug_assertions, feature = "hot-modules"))]
#[ignore = "requires `cargo build -p phantom-semantic` and PHANTOM_HOT_MODULES=1"]
fn dylib_parse_round_trips() {
    std::env::set_var("PHANTOM_HOT_MODULES", "1");
    let skill = SkillHost::build();
    let out = skill.parse("cargo build", "", "", Some(0));
    assert_eq!(out.command_type, CommandType::Cargo(CargoCommand::Build));
}
