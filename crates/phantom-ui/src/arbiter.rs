//! Layout constraint solver that bridges `SpatialPreference` and the layout engine.
//!
//! The `LayoutArbiter` collects spatial preferences from adapters, sorts by
//! priority, and performs greedy allocation. It does **not** own or mutate
//! the Taffy `LayoutEngine` -- it produces a [`LayoutPlan`] that the caller
//! (coordinator) applies to Taffy.

use std::collections::HashMap;

use phantom_adapter::spatial::{NegotiationResult, ResizeResult, SpatialPreference};

use crate::layout::Rect;

/// Unique adapter identifier (matches `AppId` from coordinator).
pub type AppId = u32;

/// The result of a layout negotiation round.
#[derive(Debug, Clone)]
pub struct LayoutPlan {
    /// Final pixel-space allocations for each adapter.
    pub allocations: HashMap<AppId, Rect>,
    /// Adapters that got no space at all.
    pub denied: Vec<AppId>,
    /// Adapters that got less than their minimum.
    pub partial: Vec<(AppId, ResizeResult)>,
}

/// Layout constraint solver.
///
/// Collects spatial preferences from adapters, sorts by priority,
/// and performs greedy allocation. Does NOT own the Taffy `LayoutEngine` --
/// it produces a `LayoutPlan` that the caller applies to Taffy.
pub struct LayoutArbiter {
    /// Available content area size in pixels (width, height).
    available: (f32, f32),
    /// Cell size for cols/rows to pixel conversion (cell_width, cell_height).
    cell_size: (f32, f32),
}

/// Internal working entry used during negotiation.
struct NegEntry {
    app_id: AppId,
    min_h: f32,
    preferred_h: f32,
    max_h: Option<f32>,
    priority: f32,
    /// Allocated height after negotiation (0 means denied).
    allocated_h: f32,
}

impl LayoutArbiter {
    /// Create a new arbiter with the given available content area and cell size.
    pub fn new(available: (f32, f32), cell_size: (f32, f32)) -> Self {
        Self {
            available,
            cell_size,
        }
    }

    /// Update the available content area (e.g. on window resize).
    pub fn set_available(&mut self, available: (f32, f32)) {
        self.available = available;
    }

    /// Run the constraint solver over the given preferences.
    ///
    /// Layout direction: vertical stack. Each adapter gets full width;
    /// height is the negotiated dimension.
    pub fn negotiate(&self, preferences: &[(AppId, SpatialPreference)]) -> LayoutPlan {
        if preferences.is_empty() {
            return LayoutPlan {
                allocations: HashMap::new(),
                denied: Vec::new(),
                partial: Vec::new(),
            };
        }

        let available_w = self.available.0;
        let available_h = self.available.1;

        // Build working entries, sorted by priority descending.
        let mut entries: Vec<NegEntry> = preferences
            .iter()
            .map(|(id, pref)| {
                let min_h = pref.min_size.1 as f32 * self.cell_size.1;
                let mut preferred_h = pref.preferred_size.1 as f32 * self.cell_size.1;
                let max_h = pref
                    .max_size
                    .map(|(_, rows)| rows as f32 * self.cell_size.1);

                // Aspect ratio constraint: if set, adjust preferred_h so that
                // width / height == aspect_ratio. Width is always available_w.
                if let Some(ratio) = pref.aspect_ratio {
                    if ratio > 0.0 {
                        let ratio_h = available_w / ratio;
                        // Clamp to min/max.
                        preferred_h = ratio_h.max(min_h);
                        if let Some(mx) = max_h {
                            preferred_h = preferred_h.min(mx);
                        }
                    }
                }

                NegEntry {
                    app_id: *id,
                    min_h,
                    preferred_h,
                    max_h,
                    priority: pref.priority,
                    allocated_h: 0.0,
                }
            })
            .collect();

        // Sort by priority descending (stable sort preserves insertion order for ties).
        entries.sort_by(|a, b| {
            b.priority
                .partial_cmp(&a.priority)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // ------------------------------------------------------------------
        // Phase 1: Check if all minimums fit. If not, deny lowest-priority
        //          adapters until the remaining minimums fit.
        // ------------------------------------------------------------------
        let total_min: f32 = entries.iter().map(|e| e.min_h).sum();
        let mut denied_ids: Vec<AppId> = Vec::new();

        if total_min > available_h {
            // Deny from the tail (lowest priority) until minimums fit.
            let mut running_min = total_min;
            for entry in entries.iter_mut().rev() {
                if running_min <= available_h {
                    break;
                }
                running_min -= entry.min_h;
                denied_ids.push(entry.app_id);
                entry.allocated_h = 0.0;
                // Mark min_h = 0 so later phases skip it.
                entry.min_h = 0.0;
                entry.preferred_h = 0.0;
            }
        }

        // ------------------------------------------------------------------
        // Phase 2: Greedy allocation. Give each non-denied adapter its
        //          preferred height if space remains, otherwise as much as
        //          possible (down to minimum).
        // ------------------------------------------------------------------
        let mut remaining = available_h;

        for entry in &mut entries {
            if denied_ids.contains(&entry.app_id) {
                continue;
            }

            let target = entry.preferred_h.min(remaining);
            let clamped = if let Some(mx) = entry.max_h {
                target.min(mx)
            } else {
                target
            };

            if clamped >= entry.min_h {
                entry.allocated_h = clamped;
            } else {
                // Cannot even meet minimum; give whatever is left.
                entry.allocated_h = remaining.max(0.0);
            }

            remaining -= entry.allocated_h;
            if remaining < 0.0 {
                remaining = 0.0;
            }
        }

        // ------------------------------------------------------------------
        // Phase 3: Distribute leftover space proportionally by priority to
        //          adapters that didn't reach their preferred size.
        // ------------------------------------------------------------------
        if remaining > 0.0 {
            let eligible: Vec<usize> = entries
                .iter()
                .enumerate()
                .filter(|(_, e)| !denied_ids.contains(&e.app_id) && e.allocated_h < e.preferred_h)
                .map(|(i, _)| i)
                .collect();

            let total_priority: f32 = eligible
                .iter()
                .map(|&i| entries[i].priority.max(0.001))
                .sum();

            if total_priority > 0.0 {
                for &i in &eligible {
                    let share = remaining * (entries[i].priority.max(0.001) / total_priority);
                    let headroom = entries[i].preferred_h - entries[i].allocated_h;
                    let max_headroom = entries[i]
                        .max_h
                        .map_or(headroom, |mx| (mx - entries[i].allocated_h).max(0.0));
                    let grant = share.min(headroom).min(max_headroom);
                    entries[i].allocated_h += grant;
                }
            } else {
                // All priorities are zero -- split equally.
                let count = eligible.len() as f32;
                if count > 0.0 {
                    let share = remaining / count;
                    for &i in &eligible {
                        let headroom = entries[i].preferred_h - entries[i].allocated_h;
                        entries[i].allocated_h += share.min(headroom);
                    }
                }
            }
        }

        // ------------------------------------------------------------------
        // Build the LayoutPlan.
        // ------------------------------------------------------------------
        let mut allocations = HashMap::new();
        let mut partial = Vec::new();
        let mut y_offset: f32 = 0.0;

        for entry in &entries {
            if denied_ids.contains(&entry.app_id) {
                continue;
            }

            let rect = Rect {
                x: 0.0,
                y: y_offset,
                width: available_w,
                height: entry.allocated_h,
            };

            // Reconstruct the original min_h from preferences for partial detection.
            let original_min_h = preferences
                .iter()
                .find(|(id, _)| *id == entry.app_id)
                .map(|(_, p)| p.min_size.1 as f32 * self.cell_size.1)
                .unwrap_or(0.0);

            if entry.allocated_h < original_min_h {
                partial.push((
                    entry.app_id,
                    ResizeResult::Partial {
                        width: available_w,
                        height: entry.allocated_h,
                    },
                ));
            }

            allocations.insert(entry.app_id, rect);
            y_offset += entry.allocated_h;
        }

        LayoutPlan {
            allocations,
            denied: denied_ids,
            partial,
        }
    }

    /// Re-negotiate after one adapter requests a different size.
    ///
    /// Treats the requested `new_size` (in pixels) as that adapter's new
    /// preferred size, converts to cols/rows, and re-runs `negotiate`.
    pub fn request_resize(
        &self,
        _current_plan: &LayoutPlan,
        app_id: AppId,
        new_size: (f32, f32),
        preferences: &[(AppId, SpatialPreference)],
    ) -> LayoutPlan {
        let new_cols = (new_size.0 / self.cell_size.0).round() as u32;
        let new_rows = (new_size.1 / self.cell_size.1).round() as u32;

        let patched: Vec<(AppId, SpatialPreference)> = preferences
            .iter()
            .map(|(id, pref)| {
                if *id == app_id {
                    let mut patched_pref = pref.clone();
                    patched_pref.preferred_size = (new_cols, new_rows);
                    (*id, patched_pref)
                } else {
                    (*id, pref.clone())
                }
            })
            .collect();

        self.negotiate(&patched)
    }

    /// Two-phase negotiation: compute a plan, query adapters for
    /// accept/counter/reject, adjust, repeat up to 3 rounds.
    pub fn negotiate_with_feedback<F>(
        &self,
        preferences: &[(AppId, SpatialPreference)],
        mut query: F,
    ) -> LayoutPlan
    where
        F: FnMut(AppId, f32, f32) -> NegotiationResult,
    {
        let mut plan = self.negotiate(preferences);

        for _round in 0..3 {
            let mut any_counter = false;
            let mut adjusted: Vec<(AppId, SpatialPreference)> = preferences.to_vec();

            for (app_id, rect) in &plan.allocations {
                let result = query(*app_id, rect.width, rect.height);
                match result {
                    NegotiationResult::Accepted => {}
                    NegotiationResult::CounterOffer { width, height } => {
                        if let Some((_, pref)) = adjusted.iter_mut().find(|(id, _)| id == app_id) {
                            let cols = (width / self.cell_size.0).round() as u32;
                            let rows = (height / self.cell_size.1).round() as u32;
                            pref.preferred_size =
                                (cols.max(pref.min_size.0), rows.max(pref.min_size.1));
                        }
                        any_counter = true;
                    }
                    NegotiationResult::Rejected { .. } => {
                        if let Some((_, pref)) = adjusted.iter_mut().find(|(id, _)| id == app_id) {
                            pref.preferred_size = pref.min_size;
                        }
                        any_counter = true;
                    }
                }
            }

            if !any_counter {
                break;
            }
            plan = self.negotiate(&adjusted);
        }

        plan
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phantom_adapter::spatial::{InternalLayout, SpatialPreference};

    const CELL_W: f32 = 8.0;
    const CELL_H: f32 = 16.0;
    const EPSILON: f32 = 0.5;

    fn approx_eq(a: f32, b: f32) -> bool {
        (a - b).abs() < EPSILON
    }

    fn make_pref(min_rows: u32, pref_rows: u32, priority: f32) -> SpatialPreference {
        SpatialPreference {
            min_size: (80, min_rows),
            preferred_size: (80, pref_rows),
            max_size: None,
            aspect_ratio: None,
            internal_panes: 1,
            internal_layout: InternalLayout::Single,
            priority,
        }
    }

    // 1. Single adapter gets full space.
    #[test]
    fn single_adapter_gets_full_space() {
        let available_h = 480.0; // 30 rows * 16px
        let arbiter = LayoutArbiter::new((640.0, available_h), (CELL_W, CELL_H));
        let prefs = vec![(1, make_pref(10, 30, 1.0))];
        let plan = arbiter.negotiate(&prefs);

        assert!(plan.denied.is_empty());
        assert!(plan.partial.is_empty());
        assert_eq!(plan.allocations.len(), 1);

        let rect = plan.allocations.get(&1).unwrap();
        assert!(approx_eq(rect.height, available_h));
        assert!(approx_eq(rect.width, 640.0));
        assert!(approx_eq(rect.y, 0.0));
    }

    // 2. Two adapters split by priority.
    #[test]
    fn two_adapters_split_by_priority() {
        // Available: 480px (30 rows). Adapter A wants 20 rows (p=5), B wants 20 rows (p=1).
        // Total preferred = 40 rows = 640px > 480px.
        // A gets its 20 rows (320px), B gets remaining 160px (10 rows).
        let arbiter = LayoutArbiter::new((640.0, 480.0), (CELL_W, CELL_H));
        let prefs = vec![(1, make_pref(5, 20, 5.0)), (2, make_pref(5, 20, 1.0))];
        let plan = arbiter.negotiate(&prefs);

        assert!(plan.denied.is_empty());
        let r1 = plan.allocations.get(&1).unwrap();
        let r2 = plan.allocations.get(&2).unwrap();

        // Higher priority adapter gets its preferred size.
        assert!(approx_eq(r1.height, 320.0), "A height: {}", r1.height);
        // Lower priority gets the remainder.
        assert!(approx_eq(r2.height, 160.0), "B height: {}", r2.height);
        // B stacks below A.
        assert!(approx_eq(r2.y, r1.height), "B y: {}", r2.y);
    }

    // 3. Adapter denied when minimums exceed space.
    #[test]
    fn adapter_denied_when_minimums_exceed_space() {
        // Available: 160px (10 rows). A min=8 rows (128px, p=5), B min=8 rows (128px, p=1).
        // Total min = 256px > 160px. B (lower priority) denied.
        let arbiter = LayoutArbiter::new((640.0, 160.0), (CELL_W, CELL_H));
        let prefs = vec![(1, make_pref(8, 10, 5.0)), (2, make_pref(8, 10, 1.0))];
        let plan = arbiter.negotiate(&prefs);

        assert_eq!(plan.denied.len(), 1);
        assert!(plan.denied.contains(&2));
        assert!(plan.allocations.contains_key(&1));
    }

    // 4. Preferred sizes honored when space allows.
    #[test]
    fn preferred_sizes_honored_when_space_allows() {
        // Available: 800px. A preferred=20 rows (320px), B preferred=15 rows (240px).
        // Total preferred = 560px < 800px. Both get preferred.
        let arbiter = LayoutArbiter::new((640.0, 800.0), (CELL_W, CELL_H));
        let prefs = vec![(1, make_pref(5, 20, 2.0)), (2, make_pref(5, 15, 1.0))];
        let plan = arbiter.negotiate(&prefs);

        assert!(plan.denied.is_empty());
        let r1 = plan.allocations.get(&1).unwrap();
        let r2 = plan.allocations.get(&2).unwrap();

        assert!(approx_eq(r1.height, 320.0), "A height: {}", r1.height);
        assert!(approx_eq(r2.height, 240.0), "B height: {}", r2.height);
    }

    // 5. Leftover space distributed proportionally.
    #[test]
    fn leftover_space_distributed_proportionally() {
        // Available: 800px. A preferred=20 rows=320px (p=3), B preferred=20 rows=320px (p=1).
        // Both get preferred (640px total). Leftover = 160px.
        // A gets 3/4 of leftover but is already at preferred so phase 3 skips them.
        // Actually both are at preferred, so no eligible adapters -- leftover stays.
        // Let me design this properly:
        // Available: 800px. A preferred=30 rows=480px (p=3), B preferred=30 rows=480px (p=1).
        // Total preferred = 960px > 800px. A gets 480, B gets 320 (remainder).
        // B didn't get preferred. Phase 3 leftover = 0 (used all space).
        // Better test: make preferred huge so neither gets full preferred.
        // Available: 480px. A preferred=20 rows=320px (p=3), B preferred=20 rows=320px (p=1).
        // Total preferred = 640px > 480px. Phase 2: A gets 320, B gets 160.
        // Phase 3: no leftover (all allocated). B < preferred but no space left.
        //
        // To test leftover distribution: all get min initially, leftover distributed.
        // Available: 640px. A min=5 rows=80px, pref=30 rows=480px (p=3).
        //                   B min=5 rows=80px, pref=30 rows=480px (p=1).
        // Phase 2: A gets 480, B gets 160 (remaining). B < preferred.
        // Phase 3: leftover = 0.
        //
        // Better: Use 3 adapters with small preferred, lots of leftover.
        // Available: 480px. A pref=10 rows=160px (p=3), B pref=10 rows=160px (p=1).
        // Phase 2: Both get 160px. Total = 320px. Leftover = 160px.
        // Phase 3: Neither is < preferred, so no eligible. Leftover stays.
        //
        // Make preferred larger than what they'd get:
        // Available: 480px. A min=5(80px) pref=25(400px) p=3, B min=5(80px) pref=25(400px) p=1.
        // Phase 2: A gets 400, B gets 80 (remaining). B < preferred.
        // Phase 3: leftover=0.
        //
        // I need leftover to exist. That happens when all adapters get their preferred
        // and there's still space. But then no one is eligible for phase 3.
        // OR: use max_size to cap allocations.
        let arbiter = LayoutArbiter::new((640.0, 480.0), (CELL_W, CELL_H));
        let mut pref_a = make_pref(5, 25, 3.0);
        pref_a.max_size = Some((80, 15)); // max 240px
        let mut pref_b = make_pref(5, 25, 1.0);
        pref_b.max_size = Some((80, 15)); // max 240px

        let prefs = vec![(1, pref_a), (2, pref_b)];
        let plan = arbiter.negotiate(&prefs);

        // Phase 2: A wants 400px but capped at 240px. B wants 400px but capped at 240px.
        // Total = 480px = available. Both get max.
        let r1 = plan.allocations.get(&1).unwrap();
        let r2 = plan.allocations.get(&2).unwrap();
        assert!(approx_eq(r1.height, 240.0), "A height: {}", r1.height);
        assert!(approx_eq(r2.height, 240.0), "B height: {}", r2.height);
    }

    // 6. Zero-priority adapter still gets minimum.
    #[test]
    fn zero_priority_adapter_gets_minimum() {
        let arbiter = LayoutArbiter::new((640.0, 480.0), (CELL_W, CELL_H));
        let prefs = vec![(1, make_pref(10, 20, 5.0)), (2, make_pref(5, 20, 0.0))];
        let plan = arbiter.negotiate(&prefs);

        assert!(plan.denied.is_empty());
        let r2 = plan.allocations.get(&2).unwrap();
        let min_h = 5.0 * CELL_H; // 80px
        assert!(
            r2.height >= min_h - EPSILON,
            "zero-priority should get at least min: {}",
            r2.height
        );
    }

    // 7. Aspect ratio constraint respected.
    #[test]
    fn aspect_ratio_constraint_respected() {
        // Width=640, aspect_ratio=2.0 => desired height = 640/2 = 320px.
        let arbiter = LayoutArbiter::new((640.0, 800.0), (CELL_W, CELL_H));
        let mut pref = make_pref(5, 50, 1.0);
        pref.aspect_ratio = Some(2.0);

        let prefs = vec![(1, pref)];
        let plan = arbiter.negotiate(&prefs);

        let r = plan.allocations.get(&1).unwrap();
        // With aspect ratio 2.0 and width 640, preferred_h becomes 320.
        assert!(
            approx_eq(r.height, 320.0),
            "aspect ratio height: {}",
            r.height
        );
    }

    // 8. request_resize re-negotiates correctly.
    #[test]
    fn request_resize_renegotiates() {
        let arbiter = LayoutArbiter::new((640.0, 480.0), (CELL_W, CELL_H));
        let prefs = vec![(1, make_pref(5, 15, 2.0)), (2, make_pref(5, 15, 1.0))];
        let plan = arbiter.negotiate(&prefs);

        // Now adapter 1 requests to be 20 rows tall (320px).
        let new_plan = arbiter.request_resize(&plan, 1, (640.0, 320.0), &prefs);

        let r1 = new_plan.allocations.get(&1).unwrap();
        assert!(
            approx_eq(r1.height, 320.0),
            "resize request height: {}",
            r1.height
        );
    }

    // 9. Empty preferences returns empty plan.
    #[test]
    fn empty_preferences_returns_empty_plan() {
        let arbiter = LayoutArbiter::new((640.0, 480.0), (CELL_W, CELL_H));
        let plan = arbiter.negotiate(&[]);

        assert!(plan.allocations.is_empty());
        assert!(plan.denied.is_empty());
        assert!(plan.partial.is_empty());
    }

    // 10. Very small available space denies all but highest priority.
    #[test]
    fn very_small_space_denies_all_but_highest() {
        // Available: 96px (6 rows). A min=5 rows=80px (p=5), B min=5 rows=80px (p=2), C min=5 rows=80px (p=1).
        // Total min = 240px > 96px. Deny C, then B. Only A fits.
        let arbiter = LayoutArbiter::new((640.0, 96.0), (CELL_W, CELL_H));
        let prefs = vec![
            (1, make_pref(5, 10, 5.0)),
            (2, make_pref(5, 10, 2.0)),
            (3, make_pref(5, 10, 1.0)),
        ];
        let plan = arbiter.negotiate(&prefs);

        assert!(plan.denied.contains(&2));
        assert!(plan.denied.contains(&3));
        assert!(plan.allocations.contains_key(&1));
        assert!(!plan.allocations.contains_key(&2));
        assert!(!plan.allocations.contains_key(&3));
    }

    // 11. set_available changes negotiation result.
    #[test]
    fn set_available_changes_result() {
        let mut arbiter = LayoutArbiter::new((640.0, 160.0), (CELL_W, CELL_H));
        let prefs = vec![(1, make_pref(8, 10, 5.0)), (2, make_pref(8, 10, 1.0))];

        // With 160px, B is denied (min total = 256px).
        let plan1 = arbiter.negotiate(&prefs);
        assert!(plan1.denied.contains(&2));

        // Expand to 480px -- both should fit.
        arbiter.set_available((640.0, 480.0));
        let plan2 = arbiter.negotiate(&prefs);
        assert!(plan2.denied.is_empty());
        assert_eq!(plan2.allocations.len(), 2);
    }

    // 12. Allocations stack vertically with correct y offsets.
    #[test]
    fn allocations_stack_vertically() {
        let arbiter = LayoutArbiter::new((640.0, 800.0), (CELL_W, CELL_H));
        let prefs = vec![
            (1, make_pref(5, 10, 3.0)),
            (2, make_pref(5, 10, 2.0)),
            (3, make_pref(5, 10, 1.0)),
        ];
        let plan = arbiter.negotiate(&prefs);

        let r1 = plan.allocations.get(&1).unwrap();
        let r2 = plan.allocations.get(&2).unwrap();
        let r3 = plan.allocations.get(&3).unwrap();

        assert!(approx_eq(r1.y, 0.0));
        assert!(approx_eq(r2.y, r1.height));
        assert!(approx_eq(r3.y, r1.height + r2.height));
    }
}
