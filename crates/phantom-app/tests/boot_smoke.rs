//! Boot smoke test — headless, no GPU required.
//!
//! Verifies that the non-GPU subsystems comprising the phantom-app boot path
//! can initialize without panicking and reach a valid initial state.
//!
//! Exercises (in order):
//!   1. `RuntimeConfig::under_dir` — temp-dir config construction.
//!   2. `AgentRuntime::new(config, seed_rules)` — substrate runtime init.
//!   3. `AgentRuntime::tick()` — one update tick with no pending events.
//!   4. `AppCoordinator::new(bus)` — event bus + coordinator construction.
//!   5. `spawn_brain(BrainConfig)` — brain thread spawn + channel health check.
//!   6. Graceful brain shutdown via `AiEvent::Shutdown`.
//!
//! Invariants asserted post-boot:
//!   - Runtime ticked without panic; spawn-rule registry is non-empty
//!     (default seed rules are installed at `AgentRuntime::new`).
//!   - No supervised children alive immediately after construction.
//!   - Coordinator starts empty (0 adapters, no focused adapter).
//!   - Brain event channel is open (send returns `Ok`).
//!
//! No GPU, no display server, no Claude API key required.
//! Run in CI with: `cargo test -p phantom-app --test boot_smoke`

use phantom_adapter::EventBus;
use phantom_app::{
    coordinator::AppCoordinator,
    runtime::{AgentRuntime, RuntimeConfig},
};
use phantom_brain::{
    brain::{BrainConfig, spawn_brain},
    events::AiEvent,
};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helper
// ---------------------------------------------------------------------------

/// Build a runtime rooted at a fresh temp directory so the event log and any
/// other on-disk state are isolated from the developer's home directory and
/// from parallel test runs.
fn make_runtime() -> (AgentRuntime, TempDir) {
    let dir = TempDir::new().expect("boot_smoke: failed to create temp dir");
    let cfg = RuntimeConfig::under_dir(dir.path());
    let rt = AgentRuntime::new(cfg, Vec::new())
        .expect("boot_smoke: AgentRuntime::new must succeed with a writable temp dir");
    (rt, dir)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Substrate runtime constructs without panicking and installs seed rules.
///
/// `AgentRuntime::new` must:
/// - Open the event log on the given path.
/// - Install the default seed spawn rules (Watcher-on-pane-open, Fixer-on-block,
///   Defender-on-denial — at least one rule must exist).
/// - Leave the supervisor with zero supervised children.
#[test]
fn runtime_new_succeeds_with_test_config() {
    let (rt, _dir) = make_runtime();

    // Seed rules are always installed; the registry must be non-empty.
    assert!(
        rt.rules().rule_count() > 0,
        "boot_smoke: default seed rules must be installed; rule_count was 0",
    );

    // No supervised children immediately after construction.
    assert_eq!(
        rt.supervisor_running_count(),
        0,
        "boot_smoke: supervisor must start with zero running children",
    );
}

/// A single `tick()` with no pending events must not panic.
///
/// An empty pending queue means: supervisor reap is a no-op, drain is empty,
/// and `last_actions` is empty after the tick.
#[test]
fn runtime_tick_with_no_events_does_not_panic() {
    let (mut rt, _dir) = make_runtime();

    // No events pushed — tick must complete without panic.
    rt.tick();

    // No events → no spawn actions.
    assert!(
        rt.last_actions().is_empty(),
        "boot_smoke: empty tick must produce no spawn actions; got {} actions",
        rt.last_actions().len(),
    );
}

/// `AppCoordinator` constructed from a fresh `EventBus` must report zero
/// adapters and no focused adapter — the coordinator invariant at boot.
#[test]
fn coordinator_starts_empty() {
    let bus = EventBus::new();
    let coord = AppCoordinator::new(bus);

    assert_eq!(
        coord.adapter_count(),
        0,
        "boot_smoke: fresh coordinator must have 0 adapters; got {}",
        coord.adapter_count(),
    );

    assert!(
        coord.focused().is_none(),
        "boot_smoke: fresh coordinator must have no focused adapter; got {:?}",
        coord.focused(),
    );

    assert!(
        coord.all_app_ids().is_empty(),
        "boot_smoke: fresh coordinator all_app_ids must be empty; got {:?}",
        coord.all_app_ids(),
    );
}

/// Brain spawns without panicking and its event channel is immediately open.
///
/// `spawn_brain` starts a background OS thread; the returned `BrainHandle`
/// must allow sending an event without error on the very first call. We
/// send `AiEvent::Shutdown` so the background thread exits cleanly and
/// doesn't outlive the test binary.
#[test]
fn brain_spawns_and_channel_is_open() {
    let handle = spawn_brain(BrainConfig {
        project_dir: ".".into(),
        enable_suggestions: false,
        enable_memory: false,
        quiet_threshold: 1.0, // suppress all actions so the thread stays quiet
        router: None,
        catalog: None,
    });

    // The channel must be open immediately after spawn.
    let send_result = handle.send_event(AiEvent::Shutdown);
    assert!(
        send_result.is_ok(),
        "boot_smoke: brain event channel must be open after spawn; got {send_result:?}",
    );
}

/// End-to-end boot invariants: runtime + coordinator + brain all initialized
/// together without panicking, and together meet the post-boot invariants
/// documented in the issue:
///
/// - Runtime rule count > 0 (seed rules installed).
/// - Runtime supervisor_running_count == 0.
/// - Coordinator adapter_count == 0 (at least valid for headless boot).
/// - Brain event channel open.
///
/// This is the canonical "boot smoke" referenced by GitHub issue #215.
#[test]
fn boot_smoke_full_invariants() {
    // ── Substrate runtime ────────────────────────────────────────────────
    let (mut rt, _dir) = make_runtime();

    // One tick with no events (mirrors App::update on the very first frame).
    rt.tick();

    assert!(
        rt.rules().rule_count() > 0,
        "boot_smoke: seed rules must be present post-tick",
    );
    assert_eq!(
        rt.supervisor_running_count(),
        0,
        "boot_smoke: no supervised children should exist at boot",
    );

    // ── App coordinator ──────────────────────────────────────────────────
    let bus = EventBus::new();
    let coord = AppCoordinator::new(bus);

    // At least one pane concept will exist in production (the initial
    // TerminalAdapter). Here we verify the coordinator is in a valid
    // initializable state — the TerminalAdapter construction requires a PTY
    // (OS resource) that we exclude from this headless test.
    assert_eq!(
        coord.adapter_count(),
        0,
        "boot_smoke: coordinator in headless mode must start with 0 adapters",
    );

    // ── AI brain ─────────────────────────────────────────────────────────
    let brain = spawn_brain(BrainConfig {
        project_dir: ".".into(),
        enable_suggestions: false,
        enable_memory: false,
        quiet_threshold: 1.0,
        router: None,
        catalog: None,
    });

    assert!(
        brain.send_event(AiEvent::Shutdown).is_ok(),
        "boot_smoke: brain must accept events immediately after spawn",
    );
}
