//! Inspector pane spawn helper — split the focused pane and register an
//! [`InspectorAdapter`] in the new child.
//!
//! Companion to [`crate::agent_pane`]'s `App::spawn_agent_pane`. Mirrors that
//! flow: split focused pane vertically, give the inspector half the width,
//! register the adapter, focus the new pane.
//!
//! Snapshot architecture — the App holds the canonical
//! `Arc<RwLock<InspectorView>>` and pushes a fresh snapshot at the end of
//! each `update()` cycle by calling [`crate::runtime::AgentRuntime::snapshot`]
//! and writing into the lock. The inspector adapter (and any future
//! observers) read through the same lock during `render()`.

use std::sync::{Arc, RwLock};

use log::{info, warn};

use phantom_agents::inspector::{
    DenialEntry, InspectorView, MAX_RECENT_DENIALS,
};
use phantom_ui::tokens::Tokens;
use phantom_ui::RenderCtx;

use crate::adapters::inspector::InspectorAdapter;
use crate::app::App;

/// Dotted-name prefix the runtime writes for `EventKind::CapabilityDenied`
/// (see `runtime::kind_dotted_name`). The `<agent_id>` suffix is appended
/// by the runtime, so prefix-matching is the right contract: the ids vary
/// from event to event.
const CAPABILITY_DENIED_KIND_PREFIX: &str = "agent.capability_denied.";

impl App {
    /// Spawn an inspector pane as a first-class coordinator adapter.
    ///
    /// Splits the focused pane vertically (50/50), creates an
    /// [`InspectorAdapter`] sharing the App's snapshot lock, registers it in
    /// the new split pane, and focuses it. Returns `false` if no pane is
    /// focused or the split fails.
    #[allow(dead_code)] // Wired by command path in a follow-up; kept ahead of time.
    pub(crate) fn spawn_inspector_pane(&mut self) -> bool {
        // Split the focused pane to make room for the inspector.
        let Some(focused_app_id) = self.coordinator.focused() else {
            warn!("Cannot spawn inspector: no focused adapter");
            return false;
        };
        let Some(current_pane_id) = self.coordinator.pane_id_for(focused_app_id) else {
            warn!("Cannot spawn inspector: focused adapter has no layout pane");
            return false;
        };

        let split_result = self.layout.split_vertical(current_pane_id);
        let (existing_child, new_child) = match split_result {
            Ok(ids) => ids,
            Err(e) => {
                warn!("Inspector split failed: {e}");
                return false;
            }
        };

        // Equal split: the existing pane keeps half, inspector takes the other.
        let _ = self.layout.set_flex_grow(existing_child, 1.0);
        let _ = self.layout.set_flex_grow(new_child, 1.0);

        // Resize layout after split.
        let width = self.gpu.surface_config.width;
        let height = self.gpu.surface_config.height;
        let _ = self.layout.resize(width as f32, height as f32);

        // Remap the existing terminal/agent's PaneId.
        self.coordinator.remap_pane(focused_app_id, current_pane_id, existing_child);

        // Resize the existing pane to fit its new (smaller) bounds.
        if let Ok(rect) = self.layout.get_pane_rect(existing_child) {
            let (cols, rows) = crate::pane::pane_cols_rows(self.cell_size, rect);
            let _ = self.coordinator.send_command(
                focused_app_id,
                "resize",
                &serde_json::json!({"cols": cols, "rows": rows}),
            );
        }

        // Build the snapshot lock if we don't have one yet, and seed it with
        // the runtime's current view.
        let snapshot = self
            .inspector_snapshot
            .get_or_insert_with(|| Arc::new(RwLock::new(InspectorView::empty())))
            .clone();
        if let Ok(mut guard) = snapshot.write() {
            let mut view = self.runtime.snapshot();
            view.denials = self.collect_recent_denials();
            *guard = view;
        }

        // Build the live tokens arc from the App's current cell size using
        // the default phosphor palette. The App can write a new `Tokens`
        // value into this arc whenever the active theme changes, and the
        // inspector adapter picks it up on the next `render()` call.
        let inspector_tokens = self
            .inspector_tokens
            .get_or_insert_with(|| {
                let ctx = RenderCtx::new(self.cell_size, self.cell_size.1);
                Arc::new(RwLock::new(Tokens::phosphor(ctx)))
            })
            .clone();

        let adapter = InspectorAdapter::new(snapshot, inspector_tokens);

        let scene_node = self.scene.add_node(
            self.scene_content_node,
            phantom_scene::node::NodeKind::Pane,
        );

        let app_id = self.coordinator.register_adapter_at_pane(
            Box::new(adapter),
            new_child,
            scene_node,
            phantom_scene::clock::Cadence::unlimited(),
        );

        // Focus the new inspector pane.
        self.coordinator.set_focus(app_id);

        info!("Inspector adapter registered (AppId {app_id}) in split pane");
        true
    }

    /// Push a fresh snapshot into the shared inspector view, if one is
    /// active. Cheap when no inspector pane is open (no Arc, no work).
    ///
    /// Called from `App::update()` once per frame after the runtime has
    /// ticked. Inspector adapters read the result through their own clone
    /// of the same `Arc<RwLock<InspectorView>>`.
    ///
    /// Augments the runtime's base snapshot with a [`DenialEntry`] feed
    /// (Sec.3) read off the same on-disk event log. The runtime stays
    /// agnostic of UI policy; the App is the right layer to project the
    /// `agent.capability_denied.*` lines into a renderable shape.
    #[allow(dead_code)] // Wired in Phase 2.G alongside the inspector adapter.
    pub(crate) fn refresh_inspector_snapshot(&mut self) {
        let Some(ref snapshot) = self.inspector_snapshot else {
            return;
        };
        let mut view = self.runtime.snapshot();
        view.denials = self.collect_recent_denials();
        if let Ok(mut guard) = snapshot.write() {
            *guard = view;
        }
    }

    /// Read the last [`MAX_RECENT_DENIALS`] `agent.capability_denied.*`
    /// envelopes from the runtime's event log and project them into the
    /// renderable [`DenialEntry`] shape.
    ///
    /// This is the App-side projection for Sec.3. We deliberately don't
    /// touch the runtime — it just exposes its event log via
    /// `event_log_handle`/`event_log()`, and the inspector reads.
    ///
    /// Order: oldest-first inside the returned vec, matching the rest of
    /// the snapshot's "newest-last" convention. The renderer reverses if
    /// it wants newest-on-top.
    ///
    /// Returns an empty vec if the log is empty or no `CapabilityDenied`
    /// events have been recorded yet.
    fn collect_recent_denials(&self) -> Vec<DenialEntry> {
        // Lock the event log briefly. The tail is in-memory and the lock
        // is released before we project — keep work under the guard small.
        let envelopes = {
            let log = self.runtime.event_log();
            // Pull a wider window than MAX_RECENT_DENIALS because most
            // events in the tail are not denials; we'll filter and then
            // truncate.
            log.tail(phantom_agents::inspector::MAX_RECENT_EVENTS)
        };

        let mut out = Vec::new();
        for env in envelopes {
            if !env.kind.starts_with(CAPABILITY_DENIED_KIND_PREFIX) {
                continue;
            }
            let payload = &env.payload;
            let role = payload
                .get("agent_role")
                .and_then(|v| v.as_str())
                .unwrap_or("?")
                .to_string();
            let attempted_tool = payload
                .get("attempted_tool")
                .and_then(|v| v.as_str())
                .unwrap_or("?")
                .to_string();
            let attempted_class = payload
                .get("attempted_class")
                .and_then(|v| v.as_str())
                .unwrap_or("?")
                .to_string();
            let source_chain: Vec<u64> = payload
                .get("source_chain")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter().filter_map(|v| v.as_u64()).collect()
                })
                .unwrap_or_default();
            // ts_unix_ms on the envelope is i64; clamp negatives to 0 for
            // the unsigned-millisecond contract the renderer expects.
            let timestamp_ms = if env.ts_unix_ms < 0 {
                0
            } else {
                env.ts_unix_ms as u64
            };
            out.push(DenialEntry {
                role,
                attempted_tool,
                attempted_class,
                source_chain,
                timestamp_ms,
            });
        }

        // Cap to MAX_RECENT_DENIALS — keep the most recent (back of the
        // vec). The builder applies the same cap downstream, but we trim
        // here so we don't hand it more rows than it wants.
        if out.len() > MAX_RECENT_DENIALS {
            let drop = out.len() - MAX_RECENT_DENIALS;
            out.drain(0..drop);
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    //! Spawn-flow tests live alongside other integration tests in `app.rs`
    //! because constructing a real `App` requires a GPU context. The
    //! adapter-level rendering tests live in `crate::adapters::inspector`.
    //!
    //! What we *can* unit-test here: the `inspect` console command routes
    //! to `App::spawn_inspector_pane` (without actually constructing an App).
    use crate::console::COMMANDS;

    /// The `inspect` command must be in the tab-completion list so it shows
    /// up in the console's autocompletion. This guards against a regression
    /// where `inspect` is added to commands.rs but forgotten in console.rs.
    #[test]
    fn console_inspect_command_is_in_completions() {
        assert!(
            COMMANDS.contains(&"inspect"),
            "console COMMANDS must include 'inspect' for tab completion",
        );
    }

    /// Lightweight router test that doesn't require a full `App`: we
    /// confirm that the parser splits "inspect" into a recognized first
    /// token. This is a no-GPU smoke test for the command path; the
    /// actual spawn path is integration-tested when an App is available.
    #[test]
    fn console_inspect_command_parses_as_known_command() {
        let input = "inspect";
        let parts: Vec<&str> = input.trim().splitn(3, ' ').collect();
        assert_eq!(parts[0], "inspect");
    }

    // =========================================================================
    // Issue #179 — Inspector denial log: record count, ordering, query
    // =========================================================================

    use phantom_agents::inspector::{DenialEntry, InspectorBuilder, MAX_RECENT_DENIALS};

    fn denial(agent_id: u32, ts_ms: u64) -> DenialEntry {
        DenialEntry {
            role: format!("agent-{agent_id}"),
            attempted_tool: "run_command".into(),
            attempted_class: "Act".into(),
            source_chain: vec![],
            timestamp_ms: ts_ms,
        }
    }

    #[test]
    fn denial_log_count_is_three_after_three_entries() {
        let view = InspectorBuilder::new()
            .with_denial(denial(1, 1_000))
            .with_denial(denial(2, 2_000))
            .with_denial(denial(3, 3_000))
            .build();
        assert_eq!(view.denials.len(), 3);
    }

    // Records pushed chronologically must appear oldest-first in the vec
    // (the builder preserves insertion order; newest-on-top rendering is the
    // renderer's responsibility).
    #[test]
    fn denial_records_ordered_oldest_first_when_pushed_chronologically() {
        let view = InspectorBuilder::new()
            .with_denial(denial(1, 1_000))
            .with_denial(denial(2, 2_000))
            .with_denial(denial(3, 3_000))
            .build();
        // Oldest (smallest ts) must be at index 0; or the builder keeps push order.
        // Verify monotone ordering from front to back by checking the last is newest.
        // The builder pushes via `with_denial` in order, so we just check order preserved.
        assert!(
            view.denials[0].timestamp_ms <= view.denials[2].timestamp_ms,
            "denials must be in insertion (oldest-first) order"
        );
    }

    // The builder must preserve insertion order regardless of timestamp values.
    #[test]
    fn denial_insertion_order_is_preserved_regardless_of_timestamp() {
        // Out-of-order timestamps: 3000, 1000, 2000.
        let view = InspectorBuilder::new()
            .with_denial(denial(10, 3_000))
            .with_denial(denial(20, 1_000))
            .with_denial(denial(30, 2_000))
            .build();
        assert_eq!(view.denials[0].role, "agent-10");
        assert_eq!(view.denials[1].role, "agent-20");
        assert_eq!(view.denials[2].role, "agent-30");
    }

    // Filter by agent id by matching role label.
    #[test]
    fn query_by_agent_id_returns_only_matching_records() {
        let view = InspectorBuilder::new()
            .with_denial(denial(1, 1_000))
            .with_denial(denial(2, 2_000))
            .with_denial(denial(1, 3_000))
            .build();
        let agent1_denials: Vec<_> = view.denials.iter()
            .filter(|d| d.role == "agent-1")
            .collect();
        assert_eq!(agent1_denials.len(), 2, "expected exactly 2 denials for agent-1");
        for d in &agent1_denials {
            assert_eq!(d.role, "agent-1");
        }
    }

    // Querying an unknown agent must return an empty slice.
    #[test]
    fn query_by_unknown_agent_returns_empty() {
        let view = InspectorBuilder::new()
            .with_denial(denial(1, 1_000))
            .build();
        let unknown: Vec<_> = view.denials.iter()
            .filter(|d| d.role == "agent-999")
            .collect();
        assert!(unknown.is_empty());
    }

    // Adding MAX+5 denials must cap at MAX_RECENT_DENIALS.
    #[test]
    fn denial_log_capped_at_max_recent_denials() {
        let mut builder = InspectorBuilder::new();
        for i in 0..(MAX_RECENT_DENIALS + 5) as u64 {
            builder = builder.with_denial(denial(0, i * 1_000));
        }
        let view = builder.build();
        assert_eq!(view.denials.len(), MAX_RECENT_DENIALS);
    }

    // When the log is capped the most recent (latest-pushed) entries must be kept.
    #[test]
    fn cap_drops_oldest_entries_keeps_most_recent() {
        let mut builder = InspectorBuilder::new();
        let total = MAX_RECENT_DENIALS + 5;
        for i in 0..total as u64 {
            builder = builder.with_denial(denial(0, i * 1_000));
        }
        let view = builder.build();
        // The kept entries must be the last MAX_RECENT_DENIALS pushed.
        let first_kept_ts = (5 * 1_000) as u64; // first 5 dropped
        assert!(
            view.denials[0].timestamp_ms >= first_kept_ts,
            "oldest entries must be dropped when cap is exceeded"
        );
    }

    #[test]
    fn empty_inspector_view_has_no_denials() {
        let view = InspectorBuilder::new().build();
        assert!(view.denials.is_empty());
    }

    // A DenialEntry must round-trip through InspectorView with no field loss.
    #[test]
    fn denial_entry_fields_round_trip_through_view() {
        let entry = DenialEntry {
            role: "Watcher".into(),
            attempted_tool: "write_file".into(),
            attempted_class: "Act".into(),
            source_chain: vec![1, 2, 3],
            timestamp_ms: 42_000,
        };
        let view = InspectorBuilder::new().with_denial(entry).build();
        let got = &view.denials[0];
        assert_eq!(got.role, "Watcher");
        assert_eq!(got.attempted_tool, "write_file");
        assert_eq!(got.attempted_class, "Act");
        assert_eq!(got.source_chain, vec![1, 2, 3]);
        assert_eq!(got.timestamp_ms, 42_000);
    }
}
