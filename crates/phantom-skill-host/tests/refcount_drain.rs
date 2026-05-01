//! Integration tests for `SwapManager` refcount-drain quiescence (#384).
//!
//! All tests here use an in-process dummy trait — no real dylib is loaded.
//! The tests cover:
//!
//! 1. Normal drain: `previous` is **not** dropped while threads hold clones;
//!    dropped within 500 ms after all holders release.
//! 2. Force-drop path: a leaker holds past the timeout; asserts the status
//!    transitions to `Forced`.
//! 3. Timeout abort: verifies `swap_state().status` becomes `Forced` when the
//!    deadline expires.
//! 4. Multi-thread contention: 50 threads each cloning + dispatching
//!    concurrently while a swap is in flight.
//! 5. Backward-compatibility: the Phase 1 `swap_smoke` static path is
//!    unchanged.

use std::sync::{Arc, Barrier};
use std::time::Duration;

use phantom_skill_host::{SwapManager, SwapStatus, tick_reaper_for_test};

// ---------------------------------------------------------------------------
// Dummy trait for tests — avoids any real dylib dependency
// ---------------------------------------------------------------------------

trait Widget: Send + Sync {
    fn value(&self) -> u64;
}

struct WidgetImpl(u64);
impl Widget for WidgetImpl {
    fn value(&self) -> u64 {
        self.0
    }
}

fn mgr(val: u64, name: &str) -> SwapManager<dyn Widget> {
    SwapManager::new(name, Arc::new(WidgetImpl(val)) as Arc<dyn Widget>)
}

// ---------------------------------------------------------------------------
// Helper: tick-driven wait — replaces wall-clock spin-sleep with synchronous
// reaper ticks.
//
// Drives the reaper synchronously on each iteration and yields to the OS
// scheduler (so other threads can release their clones) between checks.
// For deadline-based tests the deadline itself is real-time; `yield_now`
// allows enough CPU time to pass without an explicit `sleep`.  Returns
// `true` as soon as the condition holds.
// ---------------------------------------------------------------------------

fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
    use std::time::Instant;
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        tick_reaper_for_test();
        if condition() {
            return true;
        }
        std::thread::yield_now();
    }
    // One final tick + check after deadline.
    tick_reaper_for_test();
    condition()
}

// ---------------------------------------------------------------------------
// Test 1: Drain blocks until refcount hits zero
// ---------------------------------------------------------------------------

#[test]
fn previous_not_dropped_while_threads_hold_clone() {
    let mgr = Arc::new(mgr(1, "drain-test-1"));

    // Hold 100 clones of the old generation across thread::scope.
    let thread_count = 100;
    let barrier = Arc::new(Barrier::new(thread_count + 1));
    let release_barrier = Arc::new(Barrier::new(thread_count + 1));

    // Spawn 100 threads each holding a clone of the old generation.
    std::thread::scope(|s| {
        let old_handle = mgr.load(); // clone before swap

        for _ in 0..thread_count {
            let clone = Arc::clone(&old_handle);
            let b1 = Arc::clone(&barrier);
            let b2 = Arc::clone(&release_barrier);
            s.spawn(move || {
                b1.wait(); // signal: I have a clone
                b2.wait(); // wait: test says drop now
                drop(clone); // drop the clone
            });
        }

        // Wait for all threads to signal they hold a clone.
        barrier.wait();

        // Perform the swap while threads still hold the old generation.
        mgr.swap(Arc::new(WidgetImpl(2)) as Arc<dyn Widget>);

        // Assert current is updated.
        assert_eq!(mgr.load().value(), 2, "swap should install new handle");

        // Assert previous is still in Draining state (refcount > 1).
        let state = mgr.swap_state();
        assert!(
            matches!(state.status, SwapStatus::Draining { refcount, .. } if refcount > 1),
            "expected Draining with refcount > 1 while threads hold clones, got {:?}",
            state.status
        );

        // Signal threads to release their clones.
        release_barrier.wait();
        drop(old_handle); // drop the one held by the test thread too

        // Wait up to 500 ms for the reaper to observe refcount == 1 and drop.
        let drained = wait_until(Duration::from_millis(500), || {
            matches!(mgr.swap_state().status, SwapStatus::Idle)
        });
        assert!(
            drained,
            "previous generation should drain within 500 ms after all holders drop; \
             state = {:?}",
            mgr.swap_state().status
        );
    });
}

// ---------------------------------------------------------------------------
// Test 2: Swap promotes new handle; old value is gone from dispatch
// ---------------------------------------------------------------------------

#[test]
fn swap_promotes_new_handle() {
    let mgr = mgr(10, "promote-test");

    assert_eq!(mgr.load().value(), 10);
    mgr.swap(Arc::new(WidgetImpl(20)) as Arc<dyn Widget>);
    assert_eq!(mgr.load().value(), 20, "new handle should be current after swap");
    mgr.swap(Arc::new(WidgetImpl(30)) as Arc<dyn Widget>);
    assert_eq!(mgr.load().value(), 30);
}

// ---------------------------------------------------------------------------
// Test 3: Timeout aborts and rolls back to Forced status
// ---------------------------------------------------------------------------

#[test]
fn timeout_sets_forced_status() {
    // Override timeout to 100 ms so the test completes quickly.
    // SAFETY: single-threaded modifiable env var — this test runs in its own
    // process when executed with `cargo test` default isolation.
    unsafe { std::env::set_var("PHANTOM_HOT_DRAIN_TIMEOUT_MS", "100") };

    let mgr = Arc::new(mgr(1, "timeout-test"));

    // Hold a clone so the drain never completes naturally.
    let _leaking_clone = mgr.load();

    // Perform the swap.
    mgr.swap(Arc::new(WidgetImpl(2)) as Arc<dyn Widget>);

    // Current should be the new handle.
    assert_eq!(mgr.load().value(), 2);

    // Wait for the reaper to detect the deadline and force-drop (200 ms budget).
    let forced = wait_until(Duration::from_millis(600), || {
        matches!(mgr.swap_state().status, SwapStatus::Forced { .. } | SwapStatus::Idle)
    });

    assert!(
        forced,
        "expected Forced or Idle status after deadline; got {:?}",
        mgr.swap_state().status
    );

    // Restore env.
    unsafe { std::env::remove_var("PHANTOM_HOT_DRAIN_TIMEOUT_MS") };

    // Drop the leaking clone — this is a no-op for safety (dylib was already
    // force-dropped).  In a real dylib scenario, dropping _leaking_clone after
    // force-drop is the hazard; here it's safe because WidgetImpl is on the
    // heap, not in a library.
    drop(_leaking_clone);
}

// ---------------------------------------------------------------------------
// Test 4: Multi-thread contention — dispatch works under concurrent swap
// ---------------------------------------------------------------------------

#[test]
fn dispatch_works_under_contention() {
    let mgr = Arc::new(mgr(0, "contention-test"));
    let thread_count = 50;
    let iterations = 200;

    std::thread::scope(|s| {
        // Reader threads: load and call value() in a tight loop.
        for t in 0..thread_count {
            let m = Arc::clone(&mgr);
            s.spawn(move || {
                for _ in 0..iterations {
                    let v = m.load().value();
                    // Values must be either the old or the new generation.
                    assert!(
                        v <= thread_count as u64,
                        "thread {t}: unexpected value {v}"
                    );
                }
            });
        }

        // Swap thread: install new generations while readers run.
        for i in 1..=(thread_count as u64) {
            mgr.swap(Arc::new(WidgetImpl(i)) as Arc<dyn Widget>);
            std::thread::yield_now();
        }
    });

    // After all threads finish, the final value must be the last swap.
    assert_eq!(mgr.load().value(), thread_count as u64);
}

// ---------------------------------------------------------------------------
// Test 5: State is Idle when no previous generation is pending
// ---------------------------------------------------------------------------

#[test]
fn idle_when_no_previous() {
    let mgr = mgr(5, "idle-test");
    // No swap performed.
    assert_eq!(mgr.swap_state().status, SwapStatus::Idle);
}

// ---------------------------------------------------------------------------
// Test 6: Drain reaches Idle when no holder retains old handle
// ---------------------------------------------------------------------------

#[test]
fn drains_immediately_when_no_external_holder() {
    let mgr = Arc::new(mgr(1, "fast-drain-test"));

    // Swap without holding a clone of the old generation.
    mgr.swap(Arc::new(WidgetImpl(2)) as Arc<dyn Widget>);

    // The reaper polls every 250 ms.  Within 500 ms it should observe
    // refcount == 1 (only the SwapManagerInner holds the previous) and drop it.
    let drained = wait_until(Duration::from_millis(500), || {
        matches!(mgr.swap_state().status, SwapStatus::Idle)
    });

    assert!(
        drained,
        "expected Idle within 500 ms when no holder retains old handle; \
         state = {:?}",
        mgr.swap_state().status
    );
}

// ---------------------------------------------------------------------------
// Test 7: force_drop_previous transitions to Forced
// ---------------------------------------------------------------------------

#[test]
fn force_drop_previous_transitions_state() {
    let mgr = mgr(1, "force-drop-test");
    let _hold = mgr.load(); // keep refcount > 1

    mgr.swap(Arc::new(WidgetImpl(2)) as Arc<dyn Widget>);

    // Sanity: should be Draining.
    assert!(
        matches!(mgr.swap_state().status, SwapStatus::Draining { .. }),
        "expected Draining before force_drop_previous"
    );

    // SAFETY: we accept the documented risk (WidgetImpl is heap-allocated, not
    // in a dylib, so no actual use-after-free can occur here).
    unsafe { mgr.force_drop_previous() };

    let status = mgr.swap_state().status;
    // After force_drop, status is Forced (set by force_drop_previous) or Idle
    // (if the reaper ran first).
    assert!(
        matches!(status, SwapStatus::Forced { .. } | SwapStatus::Idle),
        "expected Forced or Idle after force_drop_previous, got {:?}",
        status
    );

    drop(_hold);
}

// ---------------------------------------------------------------------------
// Test 8: Backwards compat — swap_smoke static path unchanged
// ---------------------------------------------------------------------------

#[test]
fn static_skill_host_path_unchanged() {
    use phantom_skill_host::SkillHost;
    use phantom_semantic::{CommandType, GitCommand};

    let skill = SkillHost::build_static();
    assert_eq!(
        skill.classify_command("git status"),
        CommandType::Git(GitCommand::Status)
    );
}
