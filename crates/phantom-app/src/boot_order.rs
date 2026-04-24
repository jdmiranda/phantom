//! Explicit subsystem initialization and teardown ordering.
//!
//! Documents the dependency DAG between Phantom's subsystems and provides
//! ordered start_up/shut_down sequences. Wired into `App::new()` during
//! integration (WU-5).
//!
//! # Tier model
//!
//! Subsystems are grouped into tiers. Each tier's subsystems can be
//! initialized in any order within the tier, but all of tier N must
//! complete before tier N+1 starts. Shutdown runs in reverse tier order.

/// A group of subsystems that share the same dependency depth.
///
/// All subsystems within a tier depend only on tiers with a lower `order`
/// value, so they can safely be initialized in parallel (or any order)
/// once their prerequisite tiers are ready.
pub struct SubsystemTier {
    /// Human-readable tier name (e.g. `"foundation"`).
    pub name: &'static str,
    /// Monotonically increasing tier index. Lower values boot first.
    pub order: u32,
    /// Subsystem identifiers belonging to this tier.
    pub subsystems: &'static [&'static str],
}

/// Canonical boot ordering for all Phantom subsystems.
///
/// Use [`TIERS`](Self::TIERS) to iterate leaf-to-root (startup) or
/// [`shutdown_order`](Self::shutdown_order) for root-to-leaf (teardown).
pub struct BootOrder;

impl BootOrder {
    /// Subsystem initialization tiers (leaf -> root).
    pub const TIERS: &'static [SubsystemTier] = &[
        SubsystemTier {
            name: "foundation",
            order: 0,
            subsystems: &["logging", "gpu", "clocks"],
        },
        SubsystemTier {
            name: "rendering",
            order: 1,
            subsystems: &[
                "atlas",
                "text_renderer",
                "grid_renderer",
                "quad_renderer",
                "postfx",
                "shader_pipeline",
            ],
        },
        SubsystemTier {
            name: "scene",
            order: 2,
            subsystems: &["scene_tree", "layout_engine"],
        },
        SubsystemTier {
            name: "app_framework",
            order: 3,
            subsystems: &["registry", "event_bus", "coordinator"],
        },
        SubsystemTier {
            name: "agents",
            order: 4,
            subsystems: &["supervisor", "brain", "mcp_listener"],
        },
        SubsystemTier {
            name: "ui",
            order: 5,
            subsystems: &["keybinds", "theme", "widgets", "boot_sequence"],
        },
    ];

    /// Returns tiers in reverse order (root -> leaf), suitable for shutdown.
    pub fn shutdown_order() -> impl Iterator<Item = &'static SubsystemTier> {
        Self::TIERS.iter().rev()
    }
}

/// Guard that logs subsystem shutdown in reverse tier order.
///
/// Create one during app startup and either call [`shut_down`](Self::shut_down)
/// explicitly or let `Drop` handle it. Repeated calls are harmless -- the
/// guard is idempotent.
pub struct ShutdownGuard {
    started: bool,
}

impl ShutdownGuard {
    /// Create a new guard, marking the app as "started".
    pub fn new() -> Self {
        Self { started: true }
    }

    /// Walk tiers in reverse order and log each one being shut down.
    /// Subsequent calls are no-ops.
    pub fn shut_down(&mut self) {
        if !self.started {
            return;
        }
        for tier in BootOrder::shutdown_order() {
            log::info!("Shutting down tier '{}': {:?}", tier.name, tier.subsystems);
        }
        self.started = false;
    }
}

impl Default for ShutdownGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ShutdownGuard {
    fn drop(&mut self) {
        self.shut_down();
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiers_are_ordered() {
        let tiers = BootOrder::TIERS;
        for window in tiers.windows(2) {
            assert!(
                window[0].order < window[1].order,
                "tier '{}' (order {}) should precede '{}' (order {})",
                window[0].name,
                window[0].order,
                window[1].name,
                window[1].order,
            );
        }
    }

    #[test]
    fn shutdown_order_is_reversed() {
        let forward: Vec<_> = BootOrder::TIERS.iter().map(|t| t.order).collect();
        let reverse: Vec<_> = BootOrder::shutdown_order().map(|t| t.order).collect();
        let mut expected = forward.clone();
        expected.reverse();
        assert_eq!(reverse, expected);
    }

    #[test]
    fn foundation_tier_is_first() {
        assert_eq!(BootOrder::TIERS[0].name, "foundation");
    }

    #[test]
    fn all_tiers_have_subsystems() {
        for tier in BootOrder::TIERS {
            assert!(
                !tier.subsystems.is_empty(),
                "tier '{}' has no subsystems",
                tier.name,
            );
        }
    }

    #[test]
    fn shutdown_guard_is_idempotent() {
        let mut guard = ShutdownGuard::new();
        guard.shut_down();
        guard.shut_down(); // must not panic
    }

    #[test]
    fn shutdown_guard_default_matches_new() {
        let a = ShutdownGuard::new();
        let b = ShutdownGuard::default();
        assert_eq!(a.started, b.started);
    }

    #[test]
    fn tier_count() {
        assert_eq!(BootOrder::TIERS.len(), 6);
    }

    #[test]
    fn no_duplicate_tier_orders() {
        let orders: Vec<u32> = BootOrder::TIERS.iter().map(|t| t.order).collect();
        for (i, a) in orders.iter().enumerate() {
            for b in &orders[i + 1..] {
                assert_ne!(a, b, "duplicate tier order {a}");
            }
        }
    }
}
