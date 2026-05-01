//! Two-handle `SwapManager<T>` with refcount-drain quiescence.
//!
//! # Design
//!
//! Phase 1 (#382, #383) kept the old `Library` alive forever to avoid any
//! use-after-free hazard.  This module replaces that placeholder with a proper
//! two-generation scheme modelled on BEAM's module versioning:
//!
//! * **`current`** — the live generation.  All new callers receive an
//!   [`Arc`]-clone of this handle.  Stored in an `RwLock<Arc<T>>` — readers
//!   take the read lock (extremely cheap under no contention), clone the
//!   `Arc`, and release immediately.
//! * **`previous`** — the draining generation.  Held until every clone that
//!   was handed to callers before the swap has been dropped (i.e.,
//!   `Arc::strong_count == 1`, meaning only the `SwapManager` itself holds
//!   it).  After the count reaches 1 the entry is dropped, freeing the
//!   associated `libloading::Library` and unmapping the old code.
//!
//! If the refcount has not drained within [`DRAIN_TIMEOUT_DEFAULT`] a
//! force-drop occurs.  This may cause a segfault if any caller still holds a
//! pointer into the old library, but it is deliberately chosen over unbounded
//! memory growth.  The OS supervisor restarts the process.
//!
//! The drain poll loop runs in a single shared background thread called the
//! [`DrainReaper`] (see `drain_reaper.rs`), not one thread per manager.  Each
//! manager registers itself with the reaper via a [`Weak`] pointer so the
//! reaper holds no strong reference.
//!
//! # Why `RwLock<Arc<T>>` and not `ArcSwap`
//!
//! `arc-swap 1.x` implements `RefCnt` only for `T: Sized`, so
//! `ArcSwap<dyn SemanticSkill>` does not compile.  The workaround
//! (`ArcSwapAny<Arc<Arc<dyn T>>>`) adds an extra indirection that is less
//! readable than `RwLock<Arc<T>>` with identical performance characteristics
//! for a dev-mode hot-reload path.  `arc-swap` is still a dependency
//! (available for future use in non-trait-object contexts).
//!
//! # Concurrency invariants
//!
//! * `current` uses an `RwLock<Arc<T>>`: the write lock is held only during
//!   `swap()`, which is a rare operation.  Readers never block each other.
//! * `inner` is wrapped in a separate `Mutex` covering the drain-state fields.
//!   This lock is held only during `swap()` and during the reaper's 250 ms
//!   poll.  It is never held while dispatching through the trait.
//! * The reaper holds a [`Weak`] to each `SwapManagerInner`; if the manager
//!   is dropped the weak upgrade fails and the reaper removes the dead entry.
//! * `Arc::strong_count` is a relaxed read — safe here because we only need
//!   monotone progress ("has it reached 1 yet"), not a synchronised snapshot.

use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

/// Default deadline before a force-drop is attempted.
///
/// Overridden by the `PHANTOM_HOT_DRAIN_TIMEOUT_MS` environment variable.
pub const DRAIN_TIMEOUT_DEFAULT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Public status types
// ---------------------------------------------------------------------------

/// High-level status of a single swap target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwapStatus {
    /// No previous generation is pending.
    Idle,
    /// A previous generation is alive and draining.
    Draining {
        /// Milliseconds since the swap was requested.
        age_ms: u64,
        /// Current `Arc::strong_count` of the previous handle.
        ///
        /// When this reaches `1`, the previous generation is dropped.
        refcount: usize,
    },
    /// A force-drop was executed because `previous` did not drain in time.
    Forced {
        /// Milliseconds elapsed at the time of the force-drop.
        age_ms: u64,
    },
}

/// A snapshot of swap state for a single manager — used by the global
/// registry API consumed by #385 (Inspector telemetry).
#[derive(Debug, Clone)]
pub struct SwapState {
    /// Registered name of this swap target (e.g. `"phantom-semantic"`).
    pub name: String,
    /// Current status.
    pub status: SwapStatus,
}

// ---------------------------------------------------------------------------
// SwapManagerInner — the mutable drain state behind a Mutex
// ---------------------------------------------------------------------------

/// The drain-state fields that the reaper thread reads and mutates.
///
/// Wrapped in a `Mutex` inside [`SwapManager`]; the reaper reaches it via a
/// `Weak<Mutex<SwapManagerInner<T>>>`.
pub(crate) struct SwapManagerInner<T: ?Sized> {
    /// Registered name, e.g. `"phantom-semantic"`.
    pub(crate) name: String,
    /// The previous-generation trait object handle (if draining).
    pub(crate) previous: Option<Arc<T>>,
    /// When the swap was initiated (used to compute age_ms and detect timeout).
    pub(crate) swap_started: Option<Instant>,
    /// Deadline beyond which a force-drop is triggered.
    pub(crate) swap_deadline: Option<Instant>,
    /// Last recorded status (Draining or Forced).  `Idle` when no previous.
    pub(crate) last_status: SwapStatus,
}

impl<T: ?Sized> SwapManagerInner<T> {
    pub(crate) fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            previous: None,
            swap_started: None,
            swap_deadline: None,
            last_status: SwapStatus::Idle,
        }
    }

    /// Snapshot the current status for telemetry.
    pub(crate) fn snapshot_status(&self) -> SwapStatus {
        let prev = match &self.previous {
            None => return SwapStatus::Idle,
            Some(p) => p,
        };

        let age_ms = self
            .swap_started
            .map(|s| s.elapsed().as_millis() as u64)
            .unwrap_or(0);

        // Relaxed strong_count read — safe for monotone progress check.
        let refcount = Arc::strong_count(prev);

        if matches!(self.last_status, SwapStatus::Forced { .. }) {
            SwapStatus::Forced { age_ms }
        } else {
            SwapStatus::Draining { age_ms, refcount }
        }
    }

    /// Poll once.  Drops `previous` if refcount == 1, or force-drops after
    /// deadline.
    ///
    /// Returns `true` if the state transitioned to `Idle` or `Forced`.
    pub(crate) fn poll_drain(&mut self) -> bool {
        if self.previous.is_none() {
            return false;
        }

        let now = Instant::now();
        // Relaxed strong_count — acceptable for monotone progress.
        let refcount = Arc::strong_count(self.previous.as_ref().unwrap());

        if refcount == 1 {
            log::debug!(
                "skill-host/swap: '{}' previous generation drained — dropping",
                self.name
            );
            self.previous = None;
            self.swap_started = None;
            self.swap_deadline = None;
            self.last_status = SwapStatus::Idle;
            return true;
        }

        // Deadline exceeded → force-drop.
        if matches!(self.swap_deadline, Some(d) if now >= d) {
            let age_ms = self
                .swap_started
                .map(|s| s.elapsed().as_millis() as u64)
                .unwrap_or(0);
            log::error!(
                "skill-host/swap: '{}' force-dropping {} still-referenced handles \
                 after timeout — this may segfault if callers hold raw pointers \
                 into the old dylib",
                self.name,
                refcount.saturating_sub(1)
            );
            // SAFETY: deliberate force-drop.  See module-level doc.
            self.previous = None;
            self.swap_started = None;
            self.swap_deadline = None;
            self.last_status = SwapStatus::Forced { age_ms };
            return true;
        }

        // Still draining — update last_status for telemetry.
        self.last_status = self.snapshot_status();
        false
    }
}

// ---------------------------------------------------------------------------
// SwapManager<T>
// ---------------------------------------------------------------------------

/// Two-generation swap manager for a hot-reloadable `Arc<T>`.
///
/// `T` is typically `dyn SemanticSkill` or `dyn LlmSkill`.  Callers continue
/// to dispatch through the trait via [`SwapManager::load`] — the swapping
/// indirection is invisible to dispatch sites.
///
/// # Thread safety
///
/// `SwapManager<T>` is `Send + Sync` when `T: Send + Sync`.
///
/// # Example
///
/// ```rust,no_run
/// use std::sync::Arc;
/// use phantom_skill_host::{SwapManager, SkillHost, SemanticSkill};
///
/// let initial = SkillHost::build_static();
/// let mgr = SwapManager::new("phantom-semantic", initial);
///
/// // Dispatch — never blocks.
/// let skill = mgr.load();
/// let ct = skill.classify_command("git status");
///
/// // Install a new generation.
/// let next = SkillHost::build_static();
/// mgr.swap(next);
/// ```
pub struct SwapManager<T: ?Sized> {
    /// Live generation — callers clone from this.
    ///
    /// `RwLock` rather than `ArcSwap` because `arc-swap 1.x` requires
    /// `T: Sized` and cannot hold `dyn Trait`.  The read lock is nearly
    /// never contended (writers are rare hot-reload events).
    current: RwLock<Arc<T>>,
    /// Drain state, shared with the reaper thread via `Weak`.
    pub(crate) inner: Arc<Mutex<SwapManagerInner<T>>>,
}

impl<T: ?Sized + Send + Sync + 'static> SwapManager<T> {
    /// Construct a new `SwapManager` with `initial` as the first generation.
    ///
    /// Registers with the global [`DrainReaper`] immediately so the background
    /// thread begins watching this manager.
    ///
    /// `name` is used in log messages and the telemetry registry
    /// (e.g. `"phantom-semantic"`, `"phantom-nlp"`).
    pub fn new(name: impl Into<String>, initial: Arc<T>) -> Self {
        let name_str: String = name.into();
        let inner = Arc::new(Mutex::new(SwapManagerInner::new(name_str.clone())));

        let mgr = Self {
            current: RwLock::new(initial),
            inner: Arc::clone(&inner),
        };

        // Register with the shared global drain-reaper thread.
        crate::drain_reaper::register(name_str, Arc::downgrade(&inner));

        mgr
    }

    /// Load the current generation.
    ///
    /// Takes the read lock, clones the `Arc<T>`, and releases immediately.
    /// The strong-count increment is what the reaper observes to determine
    /// when the old generation is safe to drop.
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned (only possible if a
    /// previous writer panicked, which cannot happen in safe code here).
    pub fn load(&self) -> Arc<T> {
        Arc::clone(&self.current.read().unwrap())
    }

    /// Install `new_handle` as the current generation.
    ///
    /// The previous current becomes the draining generation.  A background
    /// thread (the global [`DrainReaper`]) drops it once all in-flight clones
    /// are released, or after the drain timeout (force-drop with error log).
    ///
    /// If a previous generation is still draining when `swap` is called, the
    /// old `previous` is replaced immediately (we keep at most two live
    /// generations).
    ///
    /// # Telemetry events
    ///
    /// Emits `log::info!` lines at "swap requested", "swap committed", and
    /// `log::error!` on force-drop timeout — consumed by #385.
    pub fn swap(&self, new_handle: Arc<T>) {
        let timeout = drain_timeout();
        let deadline = Instant::now() + timeout;
        let now = Instant::now();

        // Atomically install the new generation and retrieve the old one.
        let old_current = {
            let mut w = self.current.write().unwrap();
            // `std::mem::replace` returns the old Arc without an additional
            // strong_count bump — exactly what we want.
            std::mem::replace(&mut *w, new_handle)
        };

        let mut guard = self.inner.lock().unwrap();

        if let Some(prev) = &guard.previous {
            // Previous generation still draining — log a warning.
            let refcount = Arc::strong_count(prev);
            log::warn!(
                "skill-host/swap: '{}' swap requested while previous generation is \
                 still draining (refcount={}). Discarding oldest generation.",
                guard.name,
                refcount
            );
            // Unconditionally replace; this is the BEAM-style "oldest wins"
            // drop when more than two generations would be live.
        } else {
            // Only increment pending counter on the first swap (not on
            // successive re-swaps before the previous drains).
            crate::swap_manager::inc_pending();
        }

        log::info!(
            "skill-host/swap: '{}' swap requested — refcount of outgoing generation={}",
            guard.name,
            Arc::strong_count(&old_current)
        );

        guard.previous = Some(old_current);
        guard.swap_started = Some(now);
        guard.swap_deadline = Some(deadline);
        guard.last_status = SwapStatus::Draining {
            age_ms: 0,
            refcount: Arc::strong_count(guard.previous.as_ref().unwrap()),
        };

        log::info!(
            "skill-host/swap: '{}' swap committed — drain timeout {}ms",
            guard.name,
            timeout.as_millis()
        );
    }

    /// Return a snapshot of the current swap state.
    pub fn swap_state(&self) -> SwapState {
        let guard = self.inner.lock().unwrap();
        SwapState {
            name: guard.name.clone(),
            status: guard.snapshot_status(),
        }
    }

    /// Force-drop the previous generation regardless of refcount.
    ///
    /// # Safety
    ///
    /// If any caller still holds an `Arc` clone of the previous generation and
    /// calls through it after `force_drop_previous` returns, the behaviour is
    /// undefined (use-after-free of dylib code).  Only call this if you can
    /// guarantee all such holders have been notified and have dropped their
    /// clones, or if you accept the crash-and-supervisor-restart risk.
    pub unsafe fn force_drop_previous(&self) {
        let mut guard = self.inner.lock().unwrap();
        let name = guard.name.clone();
        if guard.previous.is_some() {
            let refcount = Arc::strong_count(guard.previous.as_ref().unwrap());
            let age_ms = guard
                .swap_started
                .map(|s| s.elapsed().as_millis() as u64)
                .unwrap_or(0);
            log::error!(
                "skill-host/swap: '{}' force_drop_previous called with refcount={} \
                 — possible segfault if holders call through the old handle",
                name,
                refcount
            );
            guard.previous = None;
            guard.swap_started = None;
            guard.swap_deadline = None;
            guard.last_status = SwapStatus::Forced { age_ms };
            dec_pending();
        }
    }
}

// SAFETY: `RwLock<Arc<T>>` and `Mutex<SwapManagerInner<T>>` are `Send + Sync`
// when `T: Send + Sync`.
unsafe impl<T: ?Sized + Send + Sync> Send for SwapManager<T> {}
unsafe impl<T: ?Sized + Send + Sync> Sync for SwapManager<T> {}

// ---------------------------------------------------------------------------
// Drain-timeout helper
// ---------------------------------------------------------------------------

/// Read drain timeout from the environment, with default fallback.
fn drain_timeout() -> Duration {
    std::env::var("PHANTOM_HOT_DRAIN_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(DRAIN_TIMEOUT_DEFAULT)
}

// ---------------------------------------------------------------------------
// Global pending-swaps counter — consumed by #385
// ---------------------------------------------------------------------------

static PENDING_SWAPS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Return the number of `SwapManager` instances currently holding a draining
/// previous generation.
///
/// This is a relaxed read — suitable for metrics/telemetry but not for
/// synchronisation.
pub fn pending_swaps() -> usize {
    PENDING_SWAPS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Increment the pending-swaps counter.  Called from [`SwapManager::swap`].
pub(crate) fn inc_pending() {
    PENDING_SWAPS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// Decrement the pending-swaps counter.  Called by the reaper after drain/force-drop.
pub(crate) fn dec_pending() {
    // Saturating sub prevents wrapping if callers are imbalanced.
    PENDING_SWAPS.fetch_update(
        std::sync::atomic::Ordering::Relaxed,
        std::sync::atomic::Ordering::Relaxed,
        |v| Some(v.saturating_sub(1)),
    ).ok();
}

// ---------------------------------------------------------------------------
// Tests (unit — no dylib required)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Minimal trait to drive `SwapManager` in tests without any real skill.
    trait Pinger: Send + Sync {
        fn ping(&self) -> u32;
    }

    struct PingImpl(u32);
    impl Pinger for PingImpl {
        fn ping(&self) -> u32 {
            self.0
        }
    }

    fn make_mgr(val: u32) -> SwapManager<dyn Pinger> {
        SwapManager::new(
            format!("test-pinger-{val}"),
            Arc::new(PingImpl(val)) as Arc<dyn Pinger>,
        )
    }

    #[test]
    fn load_returns_current() {
        let mgr = make_mgr(42);
        assert_eq!(mgr.load().ping(), 42);
    }

    #[test]
    fn swap_updates_current() {
        let mgr = make_mgr(1);
        mgr.swap(Arc::new(PingImpl(2)) as Arc<dyn Pinger>);
        assert_eq!(mgr.load().ping(), 2);
    }

    #[test]
    fn swap_state_is_idle_initially() {
        let mgr = make_mgr(100);
        assert_eq!(mgr.swap_state().status, SwapStatus::Idle);
    }

    #[test]
    fn swap_state_is_draining_after_swap_with_holder() {
        let mgr = make_mgr(1);
        // Hold a clone so refcount > 1 after the swap.
        let _hold = mgr.load();
        mgr.swap(Arc::new(PingImpl(2)) as Arc<dyn Pinger>);
        let state = mgr.swap_state();
        assert!(
            matches!(state.status, SwapStatus::Draining { .. }),
            "expected Draining, got {:?}",
            state.status
        );
    }

    #[test]
    fn dispatch_through_manager_works() {
        let mgr = Arc::new(make_mgr(10));
        assert_eq!(mgr.load().ping(), 10);
        mgr.swap(Arc::new(PingImpl(20)) as Arc<dyn Pinger>);
        assert_eq!(mgr.load().ping(), 20);
    }

    #[test]
    fn pending_swaps_increments_on_swap() {
        // Use a unique value to identify "our" swap.
        let before = pending_swaps();
        let mgr = make_mgr(999);
        let _hold = mgr.load(); // keep refcount > 1
        mgr.swap(Arc::new(PingImpl(998)) as Arc<dyn Pinger>);
        // Counter must have incremented by at least 1 immediately after swap.
        let after = pending_swaps();
        // The swap installs a Draining entry; pending counter must be strictly greater than before.
        assert!(
            after > before,
            "pending_swaps should be > before after swap; before={before}, after={after}"
        );
        drop(_hold);
        drop(mgr);
    }
}
