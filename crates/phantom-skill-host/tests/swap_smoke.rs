//! Smoke tests for the `SkillHost` + dylib hot-reload path.
//!
//! # Structure
//!
//! * **Always-run tests** (no feature flags, no special env vars): exercise the
//!   static `SkillHost` path so `cargo test -p phantom-skill-host` is always
//!   green.
//!
//! * **Dylib integration tests** (`#[ignore]`, requires `hot-modules` feature +
//!   `PHANTOM_HOT_MODULES=1`): full end-to-end load → dispatch → swap → assert
//!   cycle.  Run explicitly in CI via:
//!   ```text
//!   cargo build -p phantom-semantic
//!   PHANTOM_HOT_MODULES=1 \
//!       cargo test -p phantom-skill-host --features hot-modules \
//!       --tests -- --ignored swap_smoke --nocapture --test-threads=1
//!   ```
//!
//! The dylib tests are tagged `#[ignore]` so they are **skipped** by the
//! default `cargo test` sweep, which does not pre-build the cdylib.  The
//! dedicated CI workflow (`.github/workflows/dylib-swap-smoke.yml`) builds the
//! dylib first, then runs the ignored tests explicitly — resolving issue #450.

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
    let out = skill.parse(
        "cargo build",
        "",
        "error[E0308]: mismatched types\n  --> src/main.rs:10:5\n",
        Some(101),
    );
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
    // SAFETY: single-threaded test; no other test in this file modifies
    // PHANTOM_HOT_MODULES concurrently (run with --test-threads=1 when
    // mixing static and dynamic tests in the same binary).
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
// Dynamic path — requires cdylib artifact + hot-modules feature
// ---------------------------------------------------------------------------
//
// All tests below:
//   * are guarded by `#[cfg(all(debug_assertions, feature = "hot-modules"))]`
//   * are tagged `#[ignore]` — skipped by the default `cargo test` sweep
//   * use a `swap_smoke_` name prefix so `-- --ignored swap_smoke` in CI
//     selects exactly these tests and nothing from other test files
//
// The CI workflow `.github/workflows/dylib-swap-smoke.yml` builds the
// phantom-semantic cdylib and then runs these tests explicitly via:
//   cargo test -p phantom-skill-host --features hot-modules \
//       --tests -- --ignored swap_smoke --nocapture --test-threads=1

/// Build `phantom-semantic` (cdylib) and return the path to the artifact.
///
/// - If `PHANTOM_HOT_MODULES_DIR` is already set and the dylib exists there,
///   the build step is skipped (lets CI pass a pre-built artifact).
/// - Otherwise walks up from `CARGO_MANIFEST_DIR` to find the workspace root
///   and runs `cargo build -p phantom-semantic`.
///
/// # Panics
///
/// Panics (with the captured cargo output) if the build fails.
#[cfg(all(debug_assertions, feature = "hot-modules"))]
fn build_semantic_dylib() -> std::path::PathBuf {
    use std::path::PathBuf;
    use std::process::Command;

    // Platform-specific artifact name — must stay in sync with loader.rs.
    #[cfg(target_os = "macos")]
    let dylib_name = "libphantom_semantic.dylib";
    #[cfg(target_os = "linux")]
    let dylib_name = "libphantom_semantic.so";
    #[cfg(target_os = "windows")]
    let dylib_name = "phantom_semantic.dll";
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    let dylib_name = "libphantom_semantic.so";

    // 1. Honour an explicit override — useful when the CI step pre-builds and
    //    exports the directory via PHANTOM_HOT_MODULES_DIR.
    if let Some(dir) = std::env::var_os("PHANTOM_HOT_MODULES_DIR") {
        let path = PathBuf::from(&dir).join(dylib_name);
        if path.exists() {
            eprintln!("[swap_smoke] reusing pre-built dylib: {path:?}");
            return path;
        }
    }

    // 2. Walk up from CARGO_MANIFEST_DIR to find the workspace root
    //    (the directory that contains Cargo.lock).
    let manifest_dir = PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR")
            .expect("CARGO_MANIFEST_DIR not set — are you running under `cargo test`?"),
    );

    let mut workspace_root = manifest_dir.as_path();
    loop {
        if workspace_root.join("Cargo.lock").exists() {
            break;
        }
        workspace_root = workspace_root
            .parent()
            .expect("reached filesystem root without finding Cargo.lock");
    }

    eprintln!(
        "[swap_smoke] workspace root: {workspace_root:?} — running `cargo build -p phantom-semantic`"
    );

    // 3. Build the cdylib.
    let output = Command::new("cargo")
        .args(["build", "-p", "phantom-semantic"])
        .current_dir(workspace_root)
        .output()
        .expect("failed to spawn `cargo build -p phantom-semantic`");

    if !output.status.success() {
        panic!(
            "`cargo build -p phantom-semantic` failed ({})\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    let path = workspace_root
        .join("target")
        .join("debug")
        .join(dylib_name);

    assert!(
        path.exists(),
        "cdylib not found at {path:?} after successful `cargo build -p phantom-semantic`"
    );

    eprintln!("[swap_smoke] built dylib: {path:?}");
    path
}

/// Full dylib load + vtable dispatch smoke test.
///
/// Builds `phantom-semantic` if needed, loads it via `SkillHost::build()`, and
/// verifies that `classify_command` returns correct variants through the C ABI
/// vtable (not the static monomorphised path).
///
/// Run explicitly with:
/// ```text
/// cargo build -p phantom-semantic
/// PHANTOM_HOT_MODULES=1 \
///     cargo test -p phantom-skill-host --features hot-modules \
///     --tests -- swap_smoke --ignored --nocapture --test-threads=1
/// ```
#[test]
#[cfg(all(debug_assertions, feature = "hot-modules"))]
#[ignore = "requires cdylib build and PHANTOM_HOT_MODULES=1; run via dylib-swap-smoke.yml"]
fn swap_smoke_load_and_classify() {
    let dylib_path = build_semantic_dylib();
    let dylib_dir = dylib_path.parent().expect("dylib has no parent dir");

    // SAFETY: test-process-scoped mutation; run with --test-threads=1.
    unsafe {
        std::env::set_var("PHANTOM_HOT_MODULES", "1");
        std::env::set_var("PHANTOM_HOT_MODULES_DIR", dylib_dir);
    }

    let skill = SkillHost::build();
    eprintln!("[swap_smoke] SkillHost::build() returned (hot path)");

    // The vtable classify_command returns a coarse `u8` discriminant which the
    // host maps to `CommandType::Git(GitCommand::Other(""))` etc. — not the
    // fine-grained sub-variant.  We verify the *top-level* discriminant here.
    let ct = skill.classify_command("git log --oneline");
    assert!(
        matches!(ct, CommandType::Git(_)),
        "expected Git(_) via dylib vtable; got {ct:?}"
    );

    let ct2 = skill.classify_command("cargo test");
    assert!(
        matches!(ct2, CommandType::Cargo(_)),
        "expected Cargo(_) via dylib vtable; got {ct2:?}"
    );

    let ct3 = skill.classify_command("ls -la");
    assert_eq!(ct3, CommandType::Shell, "expected Shell via dylib vtable; got {ct3:?}");

    eprintln!("[swap_smoke] dylib_load_and_classify PASSED");
}

/// Full dylib `parse` round-trip through JSON serialisation.
///
/// Loads the cdylib, calls `parse`, and verifies `ParsedOutput` survives the
/// `*mut u8 / free_buf` round-trip (the memory-ownership protocol across the
/// C ABI boundary).
#[test]
#[cfg(all(debug_assertions, feature = "hot-modules"))]
#[ignore = "requires cdylib build and PHANTOM_HOT_MODULES=1; run via dylib-swap-smoke.yml"]
fn swap_smoke_parse_round_trips() {
    let dylib_path = build_semantic_dylib();
    let dylib_dir = dylib_path.parent().expect("dylib has no parent dir");

    unsafe {
        std::env::set_var("PHANTOM_HOT_MODULES", "1");
        std::env::set_var("PHANTOM_HOT_MODULES_DIR", dylib_dir);
    }

    let skill = SkillHost::build();
    let out = skill.parse(
        "cargo build",
        "Compiling phantom-semantic v0.1.0\nFinished dev\n",
        "",
        Some(0),
    );

    assert_eq!(out.command, "cargo build", "command field mismatch");
    assert_eq!(
        out.command_type,
        CommandType::Cargo(CargoCommand::Build),
        "command_type mismatch after JSON round-trip through C ABI"
    );

    eprintln!("[swap_smoke] dylib_parse_round_trips PASSED");
}

/// Load → `SwapManager::swap` → verify new handle is live + old generation drains.
///
/// This is the core test that resolves #450: it exercises the full hot-swap
/// lifecycle end-to-end with a real dylib:
///
/// 1. Build + load the cdylib → wrap in `SwapManager`.
/// 2. Retain an "in-flight" clone of the first generation (simulates a long
///    dispatch call that started before the swap).
/// 3. Load the cdylib a second time (a fresh `Arc<dyn SemanticSkill>`) and
///    call `SwapManager::swap`.
/// 4. Assert the manager serves the new generation immediately post-swap.
/// 5. Assert `SwapStatus::Draining` while the in-flight clone is alive.
/// 6. Drop the in-flight clone; tick the reaper; assert `SwapStatus::Idle`.
/// 7. Dispatch through the post-swap manager and verify correctness.
#[test]
#[cfg(all(debug_assertions, feature = "hot-modules"))]
#[ignore = "requires cdylib build and PHANTOM_HOT_MODULES=1; run via dylib-swap-smoke.yml"]
fn swap_smoke_manager_lifecycle() {
    use phantom_skill_host::{SwapManager, SwapStatus, tick_reaper_for_test};
    use std::time::Duration;

    let dylib_path = build_semantic_dylib();
    let dylib_dir = dylib_path.parent().expect("dylib has no parent dir");

    unsafe {
        std::env::set_var("PHANTOM_HOT_MODULES", "1");
        std::env::set_var("PHANTOM_HOT_MODULES_DIR", dylib_dir);
    }

    // --- 1. Load initial (first) generation --------------------------------
    let first_skill = SkillHost::build();
    eprintln!("[swap_smoke] first generation loaded");

    // The vtable returns coarse discriminants — verify Git(_) not the sub-variant.
    assert!(
        matches!(first_skill.classify_command("git status"), CommandType::Git(_)),
        "first generation vtable must return Git(_) for 'git status'"
    );

    let mgr = SwapManager::new(
        "phantom-semantic-smoke",
        std::sync::Arc::clone(&first_skill),
    );

    // --- 2. Retain an in-flight clone (refcount > 1) -----------------------
    let in_flight = mgr.load();
    assert!(
        matches!(in_flight.classify_command("cargo build"), CommandType::Cargo(_)),
        "in-flight clone must dispatch Cargo(_) for 'cargo build'"
    );

    // --- 3. Load second generation + swap ----------------------------------
    // Re-loading the same dylib path produces a fresh Arc (new Library
    // instance, same code) — this simulates what the watcher does when the
    // dylib on disk changes.
    let second_skill = SkillHost::build();
    eprintln!("[swap_smoke] second generation loaded — calling mgr.swap()");
    mgr.swap(std::sync::Arc::clone(&second_skill));

    // --- 4. New generation must be live immediately ------------------------
    let current = mgr.load();
    assert!(
        matches!(current.classify_command("git log"), CommandType::Git(_)),
        "current handle must serve Git(_) from the new generation after swap"
    );
    eprintln!("[swap_smoke] swap committed");

    // --- 5. SwapStatus must be Draining while in_flight clone is alive ------
    let state = mgr.swap_state();
    assert!(
        matches!(state.status, SwapStatus::Draining { .. }),
        "expected Draining while in-flight clone is alive; got {:?}",
        state.status
    );
    eprintln!("[swap_smoke] SwapStatus::Draining confirmed");

    // --- 6. Drop all clones of the old generation; tick reaper; wait for Idle
    //
    // Arc holders of the OLD (first) generation after the swap:
    //   - `in_flight` (held above)
    //   - `first_skill` (the original Arc built before the manager was created)
    //   - `SwapManagerInner.previous` (refcount = 1 after both above are dropped)
    //
    // `current` and `second_skill` are Arcs to the NEW generation — dropping
    // them does not affect the drain wait.
    drop(in_flight);
    drop(first_skill); // must drop the original Arc so refcount of old gen → 1
    drop(current); // new-gen clone — not required for drain, but clean up

    eprintln!("[swap_smoke] in-flight clone dropped — ticking reaper");

    let deadline = std::time::Instant::now() + Duration::from_millis(500);
    loop {
        tick_reaper_for_test();
        if matches!(mgr.swap_state().status, SwapStatus::Idle) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "previous generation did not drain within 500 ms; \
             status = {:?}",
            mgr.swap_state().status
        );
        std::thread::yield_now();
    }
    eprintln!("[swap_smoke] SwapStatus::Idle — drain complete");

    // --- 7. Dispatch still works post-swap ---------------------------------
    let post_swap = mgr.load();
    assert!(
        matches!(post_swap.classify_command("cargo test"), CommandType::Cargo(_)),
        "post-swap dispatch must return Cargo(_) for 'cargo test'"
    );

    eprintln!("[swap_smoke] dylib_swap_manager_lifecycle PASSED");
}

/// `SkillHost::build()` must fall back to the static path when the dylib
/// cannot be found — no panic.
///
/// Covers the defensive path exercised when the CI build step is skipped or
/// the dylib is in an unexpected location.
#[test]
#[cfg(all(debug_assertions, feature = "hot-modules"))]
#[ignore = "requires hot-modules feature; run via dylib-swap-smoke.yml"]
fn swap_smoke_missing_dylib_falls_back_to_static() {
    unsafe {
        std::env::set_var("PHANTOM_HOT_MODULES", "1");
        // Point at a directory that does not exist.
        std::env::set_var(
            "PHANTOM_HOT_MODULES_DIR",
            "/tmp/phantom-skill-host-nonexistent-smoke-test",
        );
    }

    // Must not panic — loader logs a warning and returns the static skill.
    let skill = SkillHost::build();

    let ct = skill.classify_command("git status");
    assert_eq!(
        ct,
        CommandType::Git(GitCommand::Status),
        "fallback static skill must classify correctly after dylib load failure"
    );

    // Cleanup env for any tests that run after this one in the same process.
    unsafe {
        std::env::remove_var("PHANTOM_HOT_MODULES");
        std::env::remove_var("PHANTOM_HOT_MODULES_DIR");
    }

    eprintln!("[swap_smoke] dylib_missing_falls_back_to_static PASSED");
}
