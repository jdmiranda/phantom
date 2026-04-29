//! Protocol primitives for the adapter contract.
//!
//! Defines [`AdapterId`] (a monotonically-assigned opaque u64 identity) and
//! [`AdapterEvent`] (the coordinator's observable event stream).
//!
//! # Design
//!
//! `AdapterId` is a newtype over `u64` rather than the existing `AppId` (`u32`)
//! because the protocol contract must be stable across the workspace. The
//! registry still assigns `AppId` for backward-compatible storage; the
//! coordinator casts on the boundary.
//!
//! # Examples
//!
//! ```
//! use phantom_adapter::protocol::{AdapterId, AdapterEvent};
//!
//! let id = AdapterId::new(1);
//! assert_eq!(id.get(), 1);
//!
//! let ev = AdapterEvent::Spawned { id };
//! assert!(matches!(ev, AdapterEvent::Spawned { .. }));
//! ```

use std::collections::VecDeque;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

// ---------------------------------------------------------------------------
// AdapterId
// ---------------------------------------------------------------------------

/// Opaque, monotonically-increasing adapter identity.
///
/// Assigned by the coordinator's [`AdapterIdGen`] at registration time.
/// Identifiers are never re-used within a process lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct AdapterId(u64);

impl AdapterId {
    /// Construct an `AdapterId` from a raw `u64`.
    ///
    /// Prefer [`AdapterIdGen::next`] in production code; this constructor
    /// is provided for testing and deserialization.
    #[inline]
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Return the underlying `u64`.
    #[inline]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Returns `true` if this is the sentinel value (`0`).
    ///
    /// `AdapterId(0)` is the reserved sentinel returned by
    /// [`AppCore::adapter_id`]'s default implementation before the coordinator
    /// calls [`Lifecycled::set_adapter_id`]. Callers that need to distinguish
    /// "not yet assigned" from a real id should check this predicate rather
    /// than comparing against a raw `0`.
    ///
    /// # Note
    ///
    /// Real ids start at `1` (see [`AdapterIdGen::new`]). `0` is permanently
    /// reserved as the sentinel and is never emitted by the generator.
    #[inline]
    pub const fn is_sentinel(self) -> bool {
        self.0 == 0
    }
}

impl fmt::Display for AdapterId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "adapter#{}", self.0)
    }
}

// ---------------------------------------------------------------------------
// AdapterIdGen — monotonic counter
// ---------------------------------------------------------------------------

/// Thread-safe monotonic generator for [`AdapterId`] values.
///
/// # Examples
///
/// ```
/// use phantom_adapter::protocol::AdapterIdGen;
///
/// let id_gen = AdapterIdGen::default();
/// let a = id_gen.next();
/// let b = id_gen.next();
/// assert_ne!(a, b);
/// assert!(b.get() > a.get());
/// ```
pub struct AdapterIdGen {
    counter: AtomicU64,
}

impl AdapterIdGen {
    /// Create a new generator starting at 1.
    pub fn new() -> Self {
        Self {
            counter: AtomicU64::new(1),
        }
    }

    /// Allocate the next [`AdapterId`]. Strictly increasing; never wraps.
    ///
    /// # Panics
    ///
    /// Panics if the counter overflows `u64::MAX`. Not realistic in practice.
    pub fn next(&self) -> AdapterId {
        let raw = self.counter.fetch_add(1, Ordering::AcqRel);
        // Guard against overflow (theoretical).
        assert!(raw < u64::MAX, "AdapterId overflow");
        AdapterId::new(raw)
    }
}

impl Default for AdapterIdGen {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// AdapterEvent
// ---------------------------------------------------------------------------

/// Events emitted by the coordinator as adapters change state.
///
/// Observers consume this stream to react to adapter lifecycle changes
/// without coupling to the coordinator's internals.
///
/// # Variants
///
/// - [`AdapterEvent::Spawned`] — a new adapter has been registered and is
///   ready to participate in the frame loop.
/// - [`AdapterEvent::Closed`] — the adapter has been removed (e.g. PTY exited
///   or the user closed the pane).
/// - [`AdapterEvent::Focused`] — the given adapter has received keyboard
///   focus. Only one adapter is focused at a time.
/// - [`AdapterEvent::ContentChanged`] — the adapter's rendered content has
///   changed since the previous frame (dirty bit).
///
/// # Non-exhaustive
///
/// The enum is `#[non_exhaustive]` so downstream observers must include a
/// wildcard arm. New variants can be added without a breaking change.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdapterEvent {
    /// A new adapter has been registered with the given [`AdapterId`].
    Spawned {
        /// The identifier assigned to the newly-registered adapter.
        id: AdapterId,
    },

    /// The adapter with the given [`AdapterId`] has been closed / removed.
    Closed {
        /// The identifier of the adapter that was removed.
        id: AdapterId,
    },

    /// Keyboard focus moved to the given adapter.
    Focused {
        /// The adapter that now holds focus.
        id: AdapterId,
    },

    /// The adapter's rendered content has changed since the previous frame.
    ContentChanged {
        /// The adapter whose content changed.
        id: AdapterId,
    },
}

impl AdapterEvent {
    /// Return the [`AdapterId`] that this event concerns.
    #[inline]
    pub fn adapter_id(&self) -> AdapterId {
        match self {
            AdapterEvent::Spawned { id }
            | AdapterEvent::Closed { id }
            | AdapterEvent::Focused { id }
            | AdapterEvent::ContentChanged { id } => *id,
        }
    }
}

// ---------------------------------------------------------------------------
// EventStream — lightweight observable buffer
// ---------------------------------------------------------------------------

/// A simple, in-process, single-producer multi-consumer event buffer.
///
/// The coordinator pushes [`AdapterEvent`]s here; observers call
/// [`EventStream::drain`] to consume them in FIFO order. Observers are
/// responsible for draining promptly; the buffer is bounded at
/// [`MAX_STREAM_CAPACITY`] entries.
///
/// # Thread safety
///
/// `EventStream` is not `Send + Sync`. Use a `Mutex<EventStream>` or an async
/// channel if cross-thread delivery is needed. The current use-site (the
/// coordinator's frame loop) is single-threaded.
const MAX_STREAM_CAPACITY: usize = 1024;

/// Buffered, observable adapter event stream.
///
/// # Examples
///
/// ```
/// use phantom_adapter::protocol::{AdapterId, AdapterEvent, EventStream};
///
/// let mut stream = EventStream::new();
/// let id = AdapterId::new(7);
/// stream.push(AdapterEvent::Spawned { id });
///
/// let events = stream.drain();
/// assert_eq!(events.len(), 1);
/// assert!(matches!(events[0], AdapterEvent::Spawned { .. }));
/// ```
pub struct EventStream {
    buf: VecDeque<AdapterEvent>,
}

impl EventStream {
    /// Create an empty stream.
    pub fn new() -> Self {
        Self {
            buf: VecDeque::with_capacity(32),
        }
    }

    /// Push an event into the stream. Drops the oldest event if the buffer is
    /// at capacity.
    pub fn push(&mut self, event: AdapterEvent) {
        if self.buf.len() >= MAX_STREAM_CAPACITY {
            self.buf.pop_front();
        }
        self.buf.push_back(event);
    }

    /// Drain all queued events in FIFO order. The internal buffer is cleared.
    pub fn drain(&mut self) -> Vec<AdapterEvent> {
        std::mem::take(&mut self.buf).into_iter().collect()
    }

    /// Number of buffered events.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Whether the stream has no buffered events.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

impl Default for EventStream {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests (written first — TDD)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // =======================================================================
    // AdapterId tests
    // =======================================================================

    #[test]
    fn adapter_id_new_stores_raw_value() {
        let id = AdapterId::new(42);
        assert_eq!(id.get(), 42);
    }

    #[test]
    fn adapter_id_equality_by_value() {
        assert_eq!(AdapterId::new(1), AdapterId::new(1));
        assert_ne!(AdapterId::new(1), AdapterId::new(2));
    }

    #[test]
    fn adapter_id_ordering() {
        assert!(AdapterId::new(1) < AdapterId::new(2));
        assert!(AdapterId::new(100) > AdapterId::new(99));
    }

    #[test]
    fn adapter_id_copy_semantics() {
        let a = AdapterId::new(7);
        let b = a; // Copy, not move
        assert_eq!(a, b);
    }

    #[test]
    fn adapter_id_display_format() {
        let id = AdapterId::new(3);
        assert_eq!(format!("{id}"), "adapter#3");
    }

    #[test]
    fn adapter_id_debug_format() {
        let id = AdapterId::new(5);
        let dbg = format!("{id:?}");
        assert!(dbg.contains("5"), "Debug output must contain the raw id");
    }

    #[test]
    fn adapter_id_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<AdapterId>();
    }

    // =======================================================================
    // AdapterIdGen tests
    // =======================================================================

    #[test]
    fn id_gen_starts_at_one() {
        let id_gen = AdapterIdGen::new();
        let first = id_gen.next();
        assert_eq!(first.get(), 1, "generator must start at 1");
    }

    #[test]
    fn id_gen_is_strictly_monotonic() {
        let id_gen = AdapterIdGen::new();
        let ids: Vec<AdapterId> = (0..100).map(|_| id_gen.next()).collect();
        for window in ids.windows(2) {
            assert!(
                window[0] < window[1],
                "ids must be strictly increasing: {:?} then {:?}",
                window[0],
                window[1],
            );
        }
    }

    #[test]
    fn id_gen_produces_unique_ids() {
        let id_gen = AdapterIdGen::new();
        let ids: Vec<AdapterId> = (0..1000).map(|_| id_gen.next()).collect();
        let deduped: std::collections::HashSet<u64> = ids.iter().map(|x| x.get()).collect();
        assert_eq!(deduped.len(), 1000, "all generated ids must be unique");
    }

    #[test]
    fn id_gen_default_is_equivalent_to_new() {
        let a = AdapterIdGen::default();
        let b = AdapterIdGen::new();
        assert_eq!(a.next().get(), b.next().get());
    }

    /// Verify that concurrent calls to `AdapterIdGen::next()` across multiple
    /// threads never produce duplicate ids.
    ///
    /// Spawns `N_THREADS` threads, each calling `next()` `IDS_PER_THREAD`
    /// times, then asserts the union of all collected ids contains no
    /// duplicates.
    #[test]
    fn id_gen_concurrent_uniqueness() {
        use std::sync::Arc;

        const N_THREADS: usize = 8;
        const IDS_PER_THREAD: usize = 1_000;

        let id_gen = Arc::new(AdapterIdGen::new());
        let handles: Vec<_> = (0..N_THREADS)
            .map(|_| {
                let id_gen = Arc::clone(&id_gen);
                std::thread::spawn(move || {
                    (0..IDS_PER_THREAD)
                        .map(|_| id_gen.next().get())
                        .collect::<Vec<u64>>()
                })
            })
            .collect();

        let all_ids: std::collections::HashSet<u64> = handles
            .into_iter()
            .flat_map(|h: std::thread::JoinHandle<Vec<u64>>| h.join().expect("thread panicked"))
            .collect();

        assert_eq!(
            all_ids.len(),
            N_THREADS * IDS_PER_THREAD,
            "concurrent AdapterIdGen::next() produced duplicate ids",
        );
    }

    // =======================================================================
    // AdapterEvent tests
    // =======================================================================

    #[test]
    fn adapter_event_spawned_variant() {
        let id = AdapterId::new(1);
        let ev = AdapterEvent::Spawned { id };
        assert!(matches!(ev, AdapterEvent::Spawned { .. }));
        assert_eq!(ev.adapter_id(), id);
    }

    #[test]
    fn adapter_event_closed_variant() {
        let id = AdapterId::new(2);
        let ev = AdapterEvent::Closed { id };
        assert!(matches!(ev, AdapterEvent::Closed { .. }));
        assert_eq!(ev.adapter_id(), id);
    }

    #[test]
    fn adapter_event_focused_variant() {
        let id = AdapterId::new(3);
        let ev = AdapterEvent::Focused { id };
        assert!(matches!(ev, AdapterEvent::Focused { .. }));
        assert_eq!(ev.adapter_id(), id);
    }

    #[test]
    fn adapter_event_content_changed_variant() {
        let id = AdapterId::new(4);
        let ev = AdapterEvent::ContentChanged { id };
        assert!(matches!(ev, AdapterEvent::ContentChanged { .. }));
        assert_eq!(ev.adapter_id(), id);
    }

    #[test]
    fn adapter_event_adapter_id_consistent_across_all_variants() {
        let id = AdapterId::new(99);
        for ev in [
            AdapterEvent::Spawned { id },
            AdapterEvent::Closed { id },
            AdapterEvent::Focused { id },
            AdapterEvent::ContentChanged { id },
        ] {
            assert_eq!(
                ev.adapter_id(),
                id,
                "adapter_id() must return the same id for variant {:?}",
                ev,
            );
        }
    }

    #[test]
    fn adapter_event_clone_and_equality() {
        let id = AdapterId::new(10);
        let a = AdapterEvent::Spawned { id };
        let b = a.clone();
        assert_eq!(a, b);
    }

    #[test]
    fn adapter_event_debug_format() {
        let id = AdapterId::new(7);
        let ev = AdapterEvent::Focused { id };
        let dbg = format!("{ev:?}");
        assert!(dbg.contains("Focused"), "Debug must include variant name");
        assert!(dbg.contains("7"), "Debug must include raw id");
    }

    // Wildcard arm required by `#[non_exhaustive]` in external code;
    // the following test exercises it from within the same crate (where
    // `#[non_exhaustive]` is not enforced) but documents the pattern.
    #[test]
    fn adapter_event_non_exhaustive_match_all_variants() {
        let events = vec![
            AdapterEvent::Spawned {
                id: AdapterId::new(1),
            },
            AdapterEvent::Closed {
                id: AdapterId::new(2),
            },
            AdapterEvent::Focused {
                id: AdapterId::new(3),
            },
            AdapterEvent::ContentChanged {
                id: AdapterId::new(4),
            },
        ];
        for ev in events {
            // Every variant is handled — proof of coverage.
            let _ = match ev {
                AdapterEvent::Spawned { id } => id,
                AdapterEvent::Closed { id } => id,
                AdapterEvent::Focused { id } => id,
                AdapterEvent::ContentChanged { id } => id,
                // Wildcard required externally (non_exhaustive prevents
                // exhaustive matching outside defining crate). This arm
                // silences the warning inside the crate.
                #[allow(unreachable_patterns)]
                _ => unreachable!(),
            };
        }
    }

    // =======================================================================
    // EventStream tests
    // =======================================================================

    #[test]
    fn event_stream_starts_empty() {
        let stream = EventStream::new();
        assert!(stream.is_empty());
        assert_eq!(stream.len(), 0);
    }

    #[test]
    fn event_stream_push_and_drain() {
        let mut stream = EventStream::new();
        let id = AdapterId::new(1);
        stream.push(AdapterEvent::Spawned { id });
        assert_eq!(stream.len(), 1);

        let events = stream.drain();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], AdapterEvent::Spawned { .. }));
        // Buffer must be cleared after drain.
        assert!(stream.is_empty());
    }

    #[test]
    fn event_stream_drain_preserves_fifo_order() {
        let mut stream = EventStream::new();
        let a = AdapterId::new(1);
        let b = AdapterId::new(2);
        stream.push(AdapterEvent::Spawned { id: a });
        stream.push(AdapterEvent::Closed { id: b });

        let events = stream.drain();
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], AdapterEvent::Spawned { id } if id == a));
        assert!(matches!(events[1], AdapterEvent::Closed { id } if id == b));
    }

    #[test]
    fn event_stream_drain_returns_empty_when_no_events() {
        let mut stream = EventStream::new();
        let events = stream.drain();
        assert!(events.is_empty());
    }

    #[test]
    fn event_stream_caps_at_max_capacity() {
        let mut stream = EventStream::new();
        let id = AdapterId::new(1);
        // Push more than MAX_STREAM_CAPACITY events.
        for _ in 0..MAX_STREAM_CAPACITY + 50 {
            stream.push(AdapterEvent::Spawned { id });
        }
        assert!(
            stream.len() <= MAX_STREAM_CAPACITY,
            "stream must not exceed MAX_STREAM_CAPACITY; got {}",
            stream.len(),
        );
    }

    #[test]
    fn event_stream_oldest_dropped_at_capacity() {
        let mut stream = EventStream::new();
        // Fill to capacity with Spawned events tagged with sequential ids.
        for i in 0..MAX_STREAM_CAPACITY as u64 {
            stream.push(AdapterEvent::Spawned {
                id: AdapterId::new(i),
            });
        }
        // Push one more Closed event — this should evict the oldest Spawned.
        let newest_id = AdapterId::new(9999);
        stream.push(AdapterEvent::Closed { id: newest_id });

        let events = stream.drain();
        // The newest event must be present.
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AdapterEvent::Closed { id } if *id == newest_id)),
            "newest event must survive capacity eviction",
        );
        // The very first Spawned (id=0) must have been evicted.
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AdapterEvent::Spawned { id } if id.get() == 0)),
            "oldest event (id=0) must have been evicted at capacity",
        );
    }

    #[test]
    fn event_stream_default_is_empty() {
        let stream = EventStream::default();
        assert!(stream.is_empty());
    }

    // =======================================================================
    // Integration: coordinator emits; observer consumes
    // =======================================================================

    /// Simulates the coordinator registering an adapter, focusing it, and
    /// then closing it — an observer drains the stream and verifies all three
    /// events arrive in order.
    #[test]
    fn coordinator_lifecycle_event_sequence() {
        let mut stream = EventStream::new();
        let id_gen = AdapterIdGen::new();

        let id = id_gen.next();

        // Coordinator: register
        stream.push(AdapterEvent::Spawned { id });
        // Coordinator: user focuses the pane
        stream.push(AdapterEvent::Focused { id });
        // Coordinator: content updated
        stream.push(AdapterEvent::ContentChanged { id });
        // Coordinator: pane closed
        stream.push(AdapterEvent::Closed { id });

        // Observer: drain
        let events = stream.drain();
        assert_eq!(events.len(), 4);
        assert!(matches!(events[0], AdapterEvent::Spawned { .. }));
        assert!(matches!(events[1], AdapterEvent::Focused { .. }));
        assert!(matches!(events[2], AdapterEvent::ContentChanged { .. }));
        assert!(matches!(events[3], AdapterEvent::Closed { .. }));
        // All events refer to the same adapter.
        for ev in &events {
            assert_eq!(ev.adapter_id(), id);
        }
    }

    /// Multiple adapters: stream carries events for distinct ids.
    #[test]
    fn event_stream_multiple_adapter_ids() {
        let mut stream = EventStream::new();
        let id_gen = AdapterIdGen::new();

        let a = id_gen.next();
        let b = id_gen.next();

        stream.push(AdapterEvent::Spawned { id: a });
        stream.push(AdapterEvent::Spawned { id: b });
        stream.push(AdapterEvent::Focused { id: a });
        stream.push(AdapterEvent::Closed { id: b });

        let events = stream.drain();
        assert_eq!(events.len(), 4);

        // a is spawned then focused; b is spawned then closed.
        let a_events: Vec<_> = events.iter().filter(|e| e.adapter_id() == a).collect();
        let b_events: Vec<_> = events.iter().filter(|e| e.adapter_id() == b).collect();

        assert_eq!(a_events.len(), 2);
        assert_eq!(b_events.len(), 2);
        assert!(matches!(a_events[0], AdapterEvent::Spawned { .. }));
        assert!(matches!(a_events[1], AdapterEvent::Focused { .. }));
        assert!(matches!(b_events[0], AdapterEvent::Spawned { .. }));
        assert!(matches!(b_events[1], AdapterEvent::Closed { .. }));
    }
}
