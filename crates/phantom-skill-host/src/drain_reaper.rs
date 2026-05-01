//! Global drain-reaper thread for [`SwapManager`].
//!
//! # Design
//!
//! Rather than spawning one background thread per `SwapManager` (which would
//! produce O(N) threads when ~10 swap targets are live), the reaper is a
//! single global thread that iterates a `Vec<Box<dyn DrainEntry>>` every
//! 250 ms.
//!
//! Because `SwapManagerInner<T>` is generic over `T`, the type is erased at
//! registration time via a `Box<dyn DrainEntry>` trait object that exposes
//! only the `poll` and `snapshot_state` methods.  This avoids monomorphising
//! the reaper loop for every concrete `T`.
//!
//! # Lifetime & shutdown
//!
//! The reaper is started lazily on the first call to [`register`] via
//! [`std::sync::OnceLock`].  It runs as a daemon thread and does not prevent
//! process exit.  Dead entries (where the `Weak` upgrade fails) are pruned
//! from the list automatically every poll cycle.

use std::sync::{Mutex, OnceLock, Weak};
use std::time::Duration;

use crate::swap_manager::{SwapManagerInner, SwapState};

/// Poll interval for the reaper thread.
const REAPER_POLL: Duration = Duration::from_millis(250);

// ---------------------------------------------------------------------------
// Poll result
// ---------------------------------------------------------------------------

#[derive(PartialEq, Eq)]
enum PollResult {
    /// The entry is idle or just became idle.
    Idle,
    /// Still draining a previous generation.
    Draining,
    /// The `Weak` pointer is dead — entry should be pruned.
    Dead,
}

// ---------------------------------------------------------------------------
// Type-erased drain entry
// ---------------------------------------------------------------------------

/// Object-safe interface for a type-erased `SwapManagerInner<T>`.
trait DrainEntry: Send + Sync {
    /// Poll once; returns status.
    fn poll(&self) -> PollResult;
    /// Returns `true` if the underlying `Weak` is still live.
    fn is_alive(&self) -> bool;
    /// Name of the swap target (for logging).
    fn name(&self) -> &str;
    /// Snapshot the current swap state, or `None` if the weak pointer is dead.
    fn snapshot_state(&self) -> Option<SwapState>;
}

/// Concrete entry for a specific `SwapManagerInner<T>`.
struct TypedEntry<T: ?Sized> {
    name: String,
    weak: Weak<Mutex<SwapManagerInner<T>>>,
}

impl<T: ?Sized + Send + Sync + 'static> DrainEntry for TypedEntry<T> {
    fn poll(&self) -> PollResult {
        let arc = match self.weak.upgrade() {
            Some(a) => a,
            None => return PollResult::Dead,
        };
        let mut guard = match arc.lock() {
            Ok(g) => g,
            Err(e) => {
                log::error!(
                    "skill-host/reaper: mutex poisoned for '{}': {e}",
                    self.name
                );
                // Treat poisoned mutex as dead — we cannot safely operate on it.
                return PollResult::Dead;
            }
        };

        let was_draining = guard.previous.is_some();
        let transitioned = guard.poll_drain();

        if was_draining && transitioned {
            // Went from Draining/Forced → Idle; decrement global counter.
            crate::swap_manager::dec_pending();
            log::info!(
                "skill-host/reaper: '{}' previous generation reaped",
                self.name
            );
            PollResult::Idle
        } else if was_draining {
            PollResult::Draining
        } else {
            PollResult::Idle
        }
    }

    fn is_alive(&self) -> bool {
        self.weak.strong_count() > 0
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn snapshot_state(&self) -> Option<SwapState> {
        let arc = self.weak.upgrade()?;
        let guard = arc.lock().ok()?;
        Some(SwapState {
            name: guard.name.clone(),
            status: guard.snapshot_status(),
        })
    }
}

// ---------------------------------------------------------------------------
// DrainReaper — the global singleton
// ---------------------------------------------------------------------------

/// Registry of all live `SwapManager` entries polled by the reaper thread.
pub(crate) struct DrainReaper {
    entries: Mutex<Vec<Box<dyn DrainEntry>>>,
}

impl DrainReaper {
    fn new() -> Self {
        Self {
            entries: Mutex::new(Vec::new()),
        }
    }

    fn register_entry(&self, entry: Box<dyn DrainEntry>) {
        self.entries.lock().unwrap().push(entry);
    }

    /// One poll cycle.  Prunes dead entries.
    fn poll_all(&self) {
        let mut entries = self.entries.lock().unwrap();
        entries.retain(|entry| {
            match entry.poll() {
                PollResult::Dead => {
                    log::debug!(
                        "skill-host/reaper: pruning dead entry '{}'",
                        entry.name()
                    );
                    false
                }
                PollResult::Draining | PollResult::Idle => true,
            }
        });
    }

    /// Drive one synchronous poll cycle — test use only.
    ///
    /// Equivalent to what the background thread does on each wakeup, but called
    /// on the calling thread so tests can advance reaper state deterministically
    /// without sleeping.
    pub(crate) fn tick_for_test(&self) {
        self.poll_all();
    }

    /// Snapshot all live entries for telemetry.
    fn snapshot_all(&self) -> Vec<SwapState> {
        let entries = self.entries.lock().unwrap();
        entries
            .iter()
            .filter(|e| e.is_alive())
            .filter_map(|e| e.snapshot_state())
            .collect()
    }
}

// SAFETY: `DrainReaper` contains only `Mutex`-wrapped data and `Box<dyn
// DrainEntry>` where `DrainEntry: Send + Sync`.
unsafe impl Send for DrainReaper {}
unsafe impl Sync for DrainReaper {}

// ---------------------------------------------------------------------------
// Global singleton + background thread
// ---------------------------------------------------------------------------

static REAPER: OnceLock<DrainReaper> = OnceLock::new();

fn global_reaper() -> &'static DrainReaper {
    REAPER.get_or_init(|| {
        let reaper = DrainReaper::new();

        // Spawn the shared drain-poll thread.  Daemon thread — does not block
        // process exit.
        std::thread::Builder::new()
            .name("skill-host/drain-reaper".into())
            .spawn(|| {
                // SAFETY: OnceLock guarantees `REAPER` is fully initialised
                // before the closure runs.  The thread's lifetime is the
                // process's lifetime, so the 'static reference is valid.
                let reaper: &'static DrainReaper =
                    REAPER.get().expect("reaper OnceLock not yet initialised");
                loop {
                    std::thread::sleep(REAPER_POLL);
                    reaper.poll_all();
                }
            })
            .expect("skill-host: failed to spawn drain-reaper thread");

        reaper
    })
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Register a new `SwapManager<T>` with the global drain reaper.
///
/// Called automatically by [`SwapManager::new`].  The reaper holds a `Weak`
/// pointer so it cannot prevent the manager from being dropped.
pub(crate) fn register<T: ?Sized + Send + Sync + 'static>(
    name: String,
    weak: Weak<Mutex<SwapManagerInner<T>>>,
) {
    let entry: Box<dyn DrainEntry> = Box::new(TypedEntry {
        name: name.clone(),
        weak,
    });
    global_reaper().register_entry(entry);
    log::debug!("skill-host/reaper: registered swap target '{name}'");
}

/// Tick the global drain-reaper once synchronously — **test use only**.
///
/// Exposed so integration tests in `tests/` can drive one reaper poll cycle
/// deterministically without sleeping.  Initialises the reaper singleton if it
/// has not yet been started (safe — identical to what `register` does).
///
/// Production callers should never need to call this; it is intended solely
/// for use in integration tests to avoid wall-clock `thread::sleep` delays.
pub fn tick_reaper_for_test() {
    global_reaper().tick_for_test();
}

/// Return the swap state of every live `SwapManager`.
///
/// Returns an empty `Vec` when `PHANTOM_HOT_MODULES` is unset — zero overhead
/// in non-hot-modules builds.
///
/// This function is always present (no `#[cfg]`) so that `phantom-app` can
/// call it unconditionally and simply observe an empty list when hot-modules is
/// disabled.
pub fn all_swap_states() -> Vec<SwapState> {
    // Fast path: no OnceLock initialised and feature is off.
    if std::env::var_os("PHANTOM_HOT_MODULES").is_none() {
        return Vec::new();
    }

    match REAPER.get() {
        Some(r) => r.snapshot_all(),
        None => Vec::new(),
    }
}
