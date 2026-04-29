//! Heartbeat keepalive — pings the relay every 30 seconds, triggers reconnect
//! with exponential back-off when a pong is not received in time.
//!
//! # Design
//! The [`Heartbeat`] struct is a state machine driven by the caller.  The
//! caller is responsible for actually sending ping messages and supplying
//! pong acknowledgements; `Heartbeat` only tracks timing and back-off state.
//!
//! This separation keeps the heartbeat logic testable without a real network.

use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// How often to send a ping when the connection is healthy.
pub const PING_INTERVAL: Duration = Duration::from_secs(30);

/// How long to wait for a pong before declaring the connection dead.
pub const PONG_TIMEOUT: Duration = Duration::from_secs(10);

/// Maximum back-off delay between reconnect attempts.
const MAX_BACKOFF: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------
// HeartbeatState
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeartbeatState {
    /// Connection is healthy; no ping outstanding.
    Idle,
    /// A ping has been sent and we are waiting for a pong.
    AwaitingPong { sent_at: Instant },
    /// The connection is dead; waiting for the back-off delay to expire.
    Reconnecting { attempt: u32, retry_at: Instant },
}

// ---------------------------------------------------------------------------
// Heartbeat
// ---------------------------------------------------------------------------

/// Tracks heartbeat state for a single relay connection.
pub struct Heartbeat {
    state: HeartbeatState,
    last_ping_at: Option<Instant>,
}

impl Heartbeat {
    /// Create a new heartbeat in the idle state.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: HeartbeatState::Idle,
            last_ping_at: None,
        }
    }

    /// Poll the heartbeat state machine.
    ///
    /// Returns a [`HeartbeatAction`] that tells the caller what to do next.
    /// Call this in a tight event loop (e.g. `tokio::select!`).
    pub fn poll(&mut self) -> HeartbeatAction {
        let now = Instant::now();
        match &self.state {
            HeartbeatState::Idle => {
                let should_ping = self
                    .last_ping_at
                    .map(|t| now.duration_since(t) >= PING_INTERVAL)
                    .unwrap_or(true); // First tick: ping immediately.

                if should_ping {
                    self.state = HeartbeatState::AwaitingPong { sent_at: now };
                    self.last_ping_at = Some(now);
                    HeartbeatAction::SendPing
                } else {
                    let next_ping = self.last_ping_at.unwrap() + PING_INTERVAL;
                    HeartbeatAction::WaitUntil(next_ping)
                }
            }

            HeartbeatState::AwaitingPong { sent_at } => {
                let elapsed = now.duration_since(*sent_at);
                if elapsed >= PONG_TIMEOUT {
                    // No pong received in time — connection is dead.
                    let retry_at = now + backoff_delay(1);
                    self.state = HeartbeatState::Reconnecting {
                        attempt: 1,
                        retry_at,
                    };
                    HeartbeatAction::Reconnect
                } else {
                    let deadline = *sent_at + PONG_TIMEOUT;
                    HeartbeatAction::WaitUntil(deadline)
                }
            }

            HeartbeatState::Reconnecting { retry_at, .. } => {
                if now >= *retry_at {
                    HeartbeatAction::Reconnect
                } else {
                    HeartbeatAction::WaitUntil(*retry_at)
                }
            }
        }
    }

    /// Called when a pong (or any valid relay message) is received.
    ///
    /// Resets the state to `Idle`.
    pub fn on_pong(&mut self) {
        self.state = HeartbeatState::Idle;
    }

    /// Called when a reconnect attempt has been made.
    ///
    /// Advances the back-off counter so the next failure waits longer.
    pub fn on_reconnect_attempt(&mut self) {
        if let HeartbeatState::Reconnecting { attempt, .. } = &self.state {
            let next_attempt = attempt + 1;
            let retry_at = Instant::now() + backoff_delay(next_attempt);
            self.state = HeartbeatState::Reconnecting {
                attempt: next_attempt,
                retry_at,
            };
        }
    }

    /// Called when a reconnect succeeds.  Resets to `Idle`.
    pub fn on_reconnect_success(&mut self) {
        self.state = HeartbeatState::Idle;
        self.last_ping_at = None;
    }

    /// Current state (useful for diagnostics/logging).
    #[must_use]
    pub fn state(&self) -> &HeartbeatState {
        &self.state
    }
}

impl Default for Heartbeat {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// HeartbeatAction
// ---------------------------------------------------------------------------

/// Instruction returned by [`Heartbeat::poll`].
#[derive(Debug)]
pub enum HeartbeatAction {
    /// Caller should send a ping message to the relay now.
    SendPing,
    /// Caller should attempt to reconnect to the relay now.
    Reconnect,
    /// Nothing to do yet; wake up at this instant and call `poll` again.
    WaitUntil(Instant),
}

// ---------------------------------------------------------------------------
// Back-off
// ---------------------------------------------------------------------------

/// Exponential back-off: `min(2^attempt, MAX_BACKOFF)`.
fn backoff_delay(attempt: u32) -> Duration {
    let secs = 2u64.saturating_pow(attempt).min(MAX_BACKOFF.as_secs());
    Duration::from_secs(secs)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_poll_requests_ping() {
        let mut hb = Heartbeat::new();
        assert!(
            matches!(hb.poll(), HeartbeatAction::SendPing),
            "first poll should request a ping"
        );
    }

    #[test]
    fn after_ping_waits_for_pong() {
        let mut hb = Heartbeat::new();
        let _ = hb.poll(); // Transitions to AwaitingPong.
        assert!(matches!(hb.state(), HeartbeatState::AwaitingPong { .. }));
    }

    #[test]
    fn pong_resets_to_idle() {
        let mut hb = Heartbeat::new();
        let _ = hb.poll(); // SendPing → AwaitingPong
        hb.on_pong();
        assert_eq!(*hb.state(), HeartbeatState::Idle);
    }

    #[test]
    fn timeout_triggers_reconnect() {
        let mut hb = Heartbeat::new();
        // Force the AwaitingPong state with a past timestamp.
        hb.state = HeartbeatState::AwaitingPong {
            sent_at: Instant::now() - PONG_TIMEOUT - Duration::from_millis(1),
        };
        assert!(matches!(hb.poll(), HeartbeatAction::Reconnect));
        assert!(matches!(
            hb.state(),
            HeartbeatState::Reconnecting { .. }
        ));
    }

    #[test]
    fn reconnect_success_resets() {
        let mut hb = Heartbeat::new();
        hb.state = HeartbeatState::Reconnecting {
            attempt: 3,
            retry_at: Instant::now() - Duration::from_secs(1),
        };
        hb.on_reconnect_success();
        assert_eq!(*hb.state(), HeartbeatState::Idle);
    }

    #[test]
    fn backoff_grows_exponentially() {
        let d1 = backoff_delay(1);
        let d2 = backoff_delay(2);
        let d3 = backoff_delay(3);
        assert!(d2 > d1);
        assert!(d3 > d2);
    }

    #[test]
    fn backoff_caps_at_max() {
        let big = backoff_delay(100);
        assert_eq!(big, MAX_BACKOFF);
    }
}
