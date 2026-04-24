use std::time::{Duration, Instant};

/// A pausable, scalable monotonic clock.
///
/// Used for session time (paused during console overlay), FX time
/// (scaled for slo-mo debugging), real time (never paused), etc.
pub struct Clock {
    elapsed: Duration,
    time_scale: f32,
    is_paused: bool,
    created_at: Instant,
}

impl Clock {
    /// Create a new clock that starts running immediately.
    pub fn new() -> Self {
        Self {
            elapsed: Duration::ZERO,
            time_scale: 1.0,
            is_paused: false,
            created_at: Instant::now(),
        }
    }

    /// Create a new clock that starts in a paused state.
    pub fn new_paused() -> Self {
        Self {
            elapsed: Duration::ZERO,
            time_scale: 1.0,
            is_paused: true,
            created_at: Instant::now(),
        }
    }

    /// Advance the clock by `real_dt * time_scale`. No-op if paused.
    pub fn tick(&mut self, real_dt: Duration) {
        if self.is_paused {
            return;
        }
        let abs_scale = self.time_scale.abs();
        if (abs_scale - 1.0).abs() < f32::EPSILON {
            self.elapsed += real_dt;
        } else {
            let scaled = real_dt.mul_f32(abs_scale);
            self.elapsed += scaled;
        }
    }

    /// Total elapsed scaled time since creation.
    pub fn elapsed(&self) -> Duration {
        self.elapsed
    }

    /// Elapsed as `f32` seconds (convenience for passing to `update(dt)`).
    pub fn elapsed_secs_f32(&self) -> f32 {
        self.elapsed.as_secs_f32()
    }

    /// Elapsed as `f64` seconds (higher precision).
    pub fn elapsed_secs_f64(&self) -> f64 {
        self.elapsed.as_secs_f64()
    }

    /// Pause the clock. Subsequent [`tick`](Self::tick) calls are no-ops.
    pub fn pause(&mut self) {
        self.is_paused = true;
    }

    /// Resume the clock after pausing.
    pub fn resume(&mut self) {
        self.is_paused = false;
    }

    /// Returns `true` if the clock is paused.
    pub fn is_paused(&self) -> bool {
        self.is_paused
    }

    /// Set the time scale factor. 1.0 = normal, 0.5 = half-speed, 2.0 = double.
    pub fn set_scale(&mut self, scale: f32) {
        self.time_scale = scale;
    }

    /// Current time scale.
    pub fn time_scale(&self) -> f32 {
        self.time_scale
    }

    /// Force-advance by exactly `step`, ignoring pause and scale.
    /// Used for frame-by-frame debugging.
    pub fn single_step(&mut self, step: Duration) {
        self.elapsed += step;
    }

    /// When this clock was created (wall-clock).
    pub fn created_at(&self) -> Instant {
        self.created_at
    }
}

impl Default for Clock {
    fn default() -> Self {
        Self::new()
    }
}

/// Clamps measured frame dt to prevent physics/animation explosion
/// after debugger pauses or OS suspends.
pub struct DtClamp {
    target_dt: Duration,
    max_dt: Duration,
}

impl DtClamp {
    /// Create a clamp with explicit target and max durations.
    pub fn new(target_dt: Duration, max_dt: Duration) -> Self {
        Self { target_dt, max_dt }
    }

    /// Standard 60 fps clamp (16.6 ms target, 100 ms max).
    pub fn default_60fps() -> Self {
        Self {
            target_dt: Duration::from_micros(16_667),
            max_dt: Duration::from_millis(100),
        }
    }

    /// If `measured` exceeds `max_dt`, return `target_dt` instead.
    pub fn apply(&self, measured: Duration) -> Duration {
        if measured > self.max_dt {
            self.target_dt
        } else {
            measured
        }
    }

    /// The nominal target frame duration.
    pub fn target_dt(&self) -> Duration {
        self.target_dt
    }

    /// The maximum tolerable frame duration before clamping kicks in.
    pub fn max_dt(&self) -> Duration {
        self.max_dt
    }
}

/// Per-subsystem tick rate governor.
///
/// Each adapter declares a target Hz. The coordinator checks
/// [`should_tick`](Self::should_tick) before calling `adapter.update()`.
pub struct Cadence {
    target_hz: f32,
    interval: Duration,
    accumulated: Duration,
}

impl Cadence {
    /// Create a cadence that fires at `target_hz` ticks per second.
    ///
    /// A `target_hz` of 0 (or negative) means the cadence never fires.
    pub fn new(target_hz: f32) -> Self {
        let interval = if target_hz > 0.0 {
            Duration::from_secs_f64(1.0 / f64::from(target_hz))
        } else {
            Duration::MAX
        };
        Self {
            target_hz,
            interval,
            accumulated: Duration::ZERO,
        }
    }

    /// Unlimited cadence — always ticks (for the renderer at frame rate).
    pub fn unlimited() -> Self {
        Self {
            target_hz: f32::INFINITY,
            interval: Duration::ZERO,
            accumulated: Duration::ZERO,
        }
    }

    /// Accumulate `dt` and return `true` if enough time has passed for a tick.
    pub fn should_tick(&mut self, dt: Duration) -> bool {
        if self.interval == Duration::ZERO {
            return true;
        }
        self.accumulated += dt;
        if self.accumulated >= self.interval {
            self.accumulated -= self.interval;
            true
        } else {
            false
        }
    }

    /// The declared target frequency in Hz.
    pub fn target_hz(&self) -> f32 {
        self.target_hz
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Assert two durations are within 1 microsecond (accounts for `mul_f32` rounding).
    fn assert_duration_approx(actual: Duration, expected: Duration) {
        let diff = if actual > expected {
            actual - expected
        } else {
            expected - actual
        };
        assert!(
            diff < Duration::from_micros(1),
            "durations differ by {diff:?}: actual={actual:?}, expected={expected:?}",
        );
    }

    #[test]
    fn clock_advances_monotonic() {
        let mut clock = Clock::new();
        clock.tick(Duration::from_millis(16));
        clock.tick(Duration::from_millis(16));
        assert_eq!(clock.elapsed(), Duration::from_millis(32));
    }

    #[test]
    fn clock_pause_freezes_time() {
        let mut clock = Clock::new();
        clock.tick(Duration::from_millis(10));
        clock.pause();
        clock.tick(Duration::from_millis(100));
        assert_eq!(clock.elapsed(), Duration::from_millis(10));
    }

    #[test]
    fn clock_resume_resumes() {
        let mut clock = Clock::new();
        clock.pause();
        clock.tick(Duration::from_millis(100));
        clock.resume();
        clock.tick(Duration::from_millis(50));
        assert_eq!(clock.elapsed(), Duration::from_millis(50));
    }

    #[test]
    fn clock_scale_slows_time() {
        let mut clock = Clock::new();
        clock.set_scale(0.5);
        clock.tick(Duration::from_millis(100));
        assert_duration_approx(clock.elapsed(), Duration::from_millis(50));
    }

    #[test]
    fn clock_scale_double_speed() {
        let mut clock = Clock::new();
        clock.set_scale(2.0);
        clock.tick(Duration::from_millis(100));
        assert_duration_approx(clock.elapsed(), Duration::from_millis(200));
    }

    #[test]
    fn clock_negative_scale_uses_abs() {
        let mut clock = Clock::new();
        clock.set_scale(-1.0);
        clock.tick(Duration::from_millis(100));
        // abs(-1.0) == 1.0, takes fast path — exact match
        assert_eq!(clock.elapsed(), Duration::from_millis(100));
    }

    #[test]
    fn clock_single_step_while_paused() {
        let mut clock = Clock::new_paused();
        assert!(clock.is_paused());
        clock.single_step(Duration::from_millis(16));
        assert_eq!(clock.elapsed(), Duration::from_millis(16));
    }

    #[test]
    fn clock_new_paused_starts_paused() {
        let clock = Clock::new_paused();
        assert!(clock.is_paused());
        assert_eq!(clock.elapsed(), Duration::ZERO);
    }

    #[test]
    fn dt_clamp_passes_normal_values() {
        let clamp = DtClamp::default_60fps();
        let normal = Duration::from_millis(16);
        assert_eq!(clamp.apply(normal), normal);
    }

    #[test]
    fn dt_clamp_clamps_breakpoint_lag() {
        let clamp = DtClamp::default_60fps();
        let huge = Duration::from_secs(5);
        assert_eq!(clamp.apply(huge), clamp.target_dt());
    }

    #[test]
    fn dt_clamp_boundary_at_max() {
        let clamp = DtClamp::default_60fps();
        let at_max = Duration::from_millis(100);
        assert_eq!(clamp.apply(at_max), at_max);
        let over = Duration::from_millis(101);
        assert_eq!(clamp.apply(over), clamp.target_dt());
    }

    #[test]
    fn cadence_fires_at_target_hz() {
        let mut cadence = Cadence::new(10.0);
        assert!(!cadence.should_tick(Duration::from_millis(50)));
        assert!(cadence.should_tick(Duration::from_millis(50)));
    }

    #[test]
    fn cadence_skips_when_too_soon() {
        let mut cadence = Cadence::new(1.0);
        assert!(!cadence.should_tick(Duration::from_millis(100)));
        assert!(!cadence.should_tick(Duration::from_millis(100)));
    }

    #[test]
    fn cadence_unlimited_always_ticks() {
        let mut cadence = Cadence::unlimited();
        assert!(cadence.should_tick(Duration::ZERO));
        assert!(cadence.should_tick(Duration::from_nanos(1)));
    }

    #[test]
    fn cadence_zero_hz_never_ticks() {
        let mut cadence = Cadence::new(0.0);
        assert!(!cadence.should_tick(Duration::from_secs(100)));
    }
}
