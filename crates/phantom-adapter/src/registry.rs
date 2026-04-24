//! App registry — discovery, lifecycle management, and adapter storage.
//!
//! Apps self-register via `register()`, transition through lifecycle
//! states, and are garbage-collected when dead.

use crate::adapter::{AppAdapter, AppId};
use crate::lifecycle::AppState;

/// Metadata for a registered app.
pub struct RegisteredApp {
    pub id: AppId,
    pub app_type: String,
    pub state: AppState,
    pub visual: bool,
    pub accepts_input: bool,
    pub accepts_commands: bool,
    pub registered_at: std::time::Instant,
}

/// Central registry of all apps in the Phantom runtime.
///
/// Adapters are stored in a parallel `Vec<Option<Box<dyn AppAdapter>>>`
/// so that `RegisteredApp` stays `Sized` and easy to query in tests.
pub struct AppRegistry {
    entries: Vec<RegisteredApp>,
    adapters: Vec<Option<Box<dyn AppAdapter>>>,
    next_id: AppId,
}

impl AppRegistry {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            adapters: Vec::new(),
            next_id: 1,
        }
    }

    /// Register a new app. Assigns an ID, calls `on_init`, and sets the
    /// initial state to `Initializing`.
    pub fn register(&mut self, app: Box<dyn AppAdapter>) -> AppId {
        let id = self.next_id;
        self.next_id += 1;

        let entry = RegisteredApp {
            id,
            app_type: app.app_type().to_string(),
            state: AppState::Initializing,
            visual: app.is_visual(),
            accepts_input: app.accepts_input(),
            accepts_commands: app.accepts_commands(),
            registered_at: std::time::Instant::now(),
        };

        self.entries.push(entry);
        self.adapters.push(Some(app));
        id
    }

    /// Transition an app to `Running` (from `Initializing`).
    /// Returns `true` if the transition was valid.
    pub fn ready(&mut self, id: AppId) -> bool {
        self.transition(id, AppState::Running)
    }

    /// Suspend a running app.
    pub fn suspend(&mut self, id: AppId) -> bool {
        self.transition(id, AppState::Suspended)
    }

    /// Resume a suspended app.
    pub fn resume(&mut self, id: AppId) -> bool {
        self.transition(id, AppState::Running)
    }

    /// Request graceful exit for an app.
    pub fn request_exit(&mut self, id: AppId) -> bool {
        self.transition(id, AppState::Exiting)
    }

    /// Force-kill an app (any state -> Dead).
    pub fn kill(&mut self, id: AppId) {
        self.transition(id, AppState::Dead);
    }

    /// Garbage-collect dead apps. Returns the number of entries removed.
    pub fn gc(&mut self) -> usize {
        let before = self.entries.len();

        // Collect indices of dead entries (reverse order for safe removal).
        let dead_indices: Vec<usize> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.state == AppState::Dead)
            .map(|(i, _)| i)
            .rev()
            .collect();

        for i in dead_indices {
            self.entries.swap_remove(i);
            self.adapters.swap_remove(i);
        }

        before - self.entries.len()
    }

    /// Look up an app's metadata.
    pub fn get(&self, id: AppId) -> Option<&RegisteredApp> {
        self.entries.iter().find(|e| e.id == id)
    }

    /// Borrow the adapter for `id`.
    pub fn get_adapter(&self, id: AppId) -> Option<&dyn AppAdapter> {
        self.index_of(id)
            .and_then(|i| self.adapters[i].as_deref())
    }

    /// Mutably borrow the adapter for `id`.
    pub fn get_adapter_mut(&mut self, id: AppId) -> Option<&mut dyn AppAdapter> {
        let idx = self.index_of(id)?;
        match &mut self.adapters[idx] {
            Some(boxed) => Some(&mut **boxed),
            None => None,
        }
    }

    /// All app IDs in the given state.
    pub fn by_state(&self, state: AppState) -> Vec<AppId> {
        self.entries
            .iter()
            .filter(|e| e.state == state)
            .map(|e| e.id)
            .collect()
    }

    /// All running app IDs.
    pub fn all_running(&self) -> Vec<AppId> {
        self.by_state(AppState::Running)
    }

    /// All visual app IDs (any state except Dead).
    pub fn all_visual(&self) -> Vec<AppId> {
        self.entries
            .iter()
            .filter(|e| e.visual && e.state != AppState::Dead)
            .map(|e| e.id)
            .collect()
    }

    /// Total number of registered (non-GC'd) apps.
    pub fn count(&self) -> usize {
        self.entries.len()
    }

    // ----- internal helpers -----

    fn index_of(&self, id: AppId) -> Option<usize> {
        self.entries.iter().position(|e| e.id == id)
    }

    fn transition(&mut self, id: AppId, next: AppState) -> bool {
        let Some(idx) = self.index_of(id) else {
            return false;
        };

        let entry = &mut self.entries[idx];
        if !entry.state.can_transition_to(next) {
            return false;
        }

        entry.state = next;

        // Notify the adapter of the state change.
        if let Some(adapter) = &mut self.adapters[idx] {
            adapter.on_state_change(next);
        }

        true
    }
}

impl Default for AppRegistry {
    fn default() -> Self {
        Self::new()
    }
}
