//! App lifecycle state machine.
//!
//! Defines the valid states an app can be in and governs which transitions
//! are legal. See ARD-003 for the full state diagram.

use serde::{Deserialize, Serialize};

/// The lifecycle state of a registered app.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AppState {
    /// App is loading / initializing. Not yet ready for events.
    Initializing,
    /// App is running normally. Receives events, renders (if visual).
    Running,
    /// App is paused / backgrounded. Retains state but receives no updates.
    Suspended,
    /// App is shutting down gracefully. Finishing work, saving state.
    Exiting,
    /// App is dead. Will be garbage-collected by the registry.
    Dead,
}

impl AppState {
    /// Returns `true` if `self` is allowed to transition to `next`.
    ///
    /// Valid transitions (from the ARD):
    /// - Initializing -> Running
    /// - Running -> Suspended
    /// - Suspended -> Running
    /// - Running -> Exiting
    /// - Exiting -> Dead
    /// - ANY -> Dead  (crash / kill / timeout)
    pub fn can_transition_to(&self, next: AppState) -> bool {
        // Any state can transition to Dead (force-kill).
        if next == AppState::Dead {
            return true;
        }

        matches!(
            (self, next),
            (AppState::Initializing, AppState::Running)
                | (AppState::Running, AppState::Suspended)
                | (AppState::Suspended, AppState::Running)
                | (AppState::Running, AppState::Exiting)
                | (AppState::Exiting, AppState::Dead)
        )
    }

    /// An app is "active" if it is Running or Suspended.
    pub fn is_active(&self) -> bool {
        matches!(self, AppState::Running | AppState::Suspended)
    }

    /// Only Running apps receive input events.
    pub fn receives_input(&self) -> bool {
        matches!(self, AppState::Running)
    }

    /// Running or Suspended apps may publish to the event bus.
    pub fn can_publish(&self) -> bool {
        matches!(self, AppState::Running | AppState::Suspended)
    }
}
