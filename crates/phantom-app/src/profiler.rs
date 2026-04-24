//! Hierarchical profiler integration.
//!
//! Provides `profile_scope!` and `profile_frame!` macros that are zero-cost
//! when the `phantom-profile` feature is disabled.
//!
//! When enabled, scopes are logged via `log::trace!` with timing info.
//! Future: replace with `tracy-client` for full flamegraph support.
//!
//! # Instrumentation points (WU-5 applies these)
//!
//! - `App::tick` -> `profile_scope!("tick")`
//! - `App::render` -> `profile_scope!("render")`
//! - render.scene, render.postfx, render.overlay (nested)
//! - `AppCoordinator::update_all` -> `profile_scope!("coord.update")`
//! - `TextRenderer::prepare_glyphs` -> `profile_scope!("text.prepare")`
//! - `BrainHandle::tick` -> `profile_scope!("brain.tick")`
//! - End of frame -> `profile_frame!()`

/// A profiling scope that measures wall-clock time from creation to drop.
#[cfg(feature = "phantom-profile")]
pub struct ProfileScope {
    name: &'static str,
    start: std::time::Instant,
}

#[cfg(feature = "phantom-profile")]
impl ProfileScope {
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            start: std::time::Instant::now(),
        }
    }
}

#[cfg(feature = "phantom-profile")]
impl Drop for ProfileScope {
    fn drop(&mut self) {
        let elapsed = self.start.elapsed();
        log::trace!(
            target: "phantom::profiler",
            "[PROFILE] {}: {:.3}ms",
            self.name,
            elapsed.as_secs_f64() * 1000.0,
        );
    }
}

/// Frame counter for profiling.
#[cfg(feature = "phantom-profile")]
static FRAME_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Mark the end of a frame (for frame-rate tracking).
#[cfg(feature = "phantom-profile")]
pub fn frame_mark() {
    let frame = FRAME_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if frame % 60 == 0 {
        log::trace!(target: "phantom::profiler", "[FRAME] frame {frame}");
    }
}

#[cfg(not(feature = "phantom-profile"))]
pub fn frame_mark() {}

/// Create a profiling scope. Measures wall-clock time until the scope exits.
/// Zero-cost when `phantom-profile` feature is disabled.
///
/// # Example
/// ```ignore
/// profile_scope!("render.scene");
/// // ... rendering code ...
/// // scope drops here, logs elapsed time
/// ```
#[macro_export]
macro_rules! profile_scope {
    ($name:literal) => {
        #[cfg(feature = "phantom-profile")]
        let _profile_scope = $crate::profiler::ProfileScope::new($name);
    };
}

/// Mark the end of a frame for profiling.
#[macro_export]
macro_rules! profile_frame {
    () => {
        $crate::profiler::frame_mark();
    };
}

#[cfg(test)]
mod tests {
    // These tests verify the macros compile in both configurations.

    #[test]
    fn profile_scope_compiles_without_feature() {
        // This test runs without the feature flag (default).
        // The macro should expand to nothing.
        profile_scope!("test.scope");
        // If we get here, the macro compiled correctly as a no-op.
    }

    #[test]
    fn profile_frame_compiles_without_feature() {
        profile_frame!();
    }

    #[test]
    fn frame_mark_is_callable() {
        super::frame_mark();
    }
}
