//! Find-in-terminal search coordination.
//!
//! Bridges the [`crate::app::App::search_bar`] widget with the
//! `update_search` adapter command, routing query changes and navigation
//! requests through the coordinator.
//!
//! Three entry points are called from `input.rs`:
//! - [`App::update_search_query`] — rebuild the index after the query changed.
//! - [`App::advance_search_match`] — step forward (+1) or back (-1) through
//!   the match list, scrolling the terminal to keep the hit in view.
//! - [`App::clear_search`] — clear the index and deactivate search mode.

use log::debug;

use crate::app::App;

impl App {
    /// Send the new `query` to the focused terminal adapter and update the
    /// search bar's match counter from the response.
    pub(crate) fn update_search_query(&mut self, query: &str) {
        if let Some(focused) = self.coordinator.focused() {
            let _ = self.coordinator.send_command(
                focused,
                "update_search",
                &serde_json::json!({ "query": query }),
            );

            // Read back the match count to update the widget display.
            if let Ok(info_json) = self.coordinator.send_command(
                focused,
                "search_info",
                &serde_json::json!({}),
            ) && let Ok(v) = serde_json::from_str::<serde_json::Value>(&info_json) {
                let total = v.get("total").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
                // Reset to match 1 on every query change.
                self.search_bar.set_match_info(total, 1);
                debug!("Search query {:?}: {} matches", query, total);
            }
        }
    }

    /// Step through search matches by `delta` (+1 = next, -1 = prev).
    ///
    /// Wraps around at the boundary so the user can keep pressing and cycle
    /// through all results. No-ops when there are no matches.
    pub(crate) fn advance_search_match(&mut self, delta: i32) {
        // We don't store a "current_match_index" separately — the search bar
        // tracks `current_match` (1-indexed). Derive the 0-indexed slot.
        let total = self.search_bar.match_count();
        if total == 0 {
            return;
        }

        let current_1 = self.search_bar.current_match().max(1);
        let next_0 = (current_1 as i32 - 1 + delta).rem_euclid(total as i32) as usize;
        let next_1 = next_0 + 1;

        self.search_bar.set_match_info(total, next_1);

        // Scroll the focused terminal to put the nth match in view.
        if let Some(focused) = self.coordinator.focused() {
            let _ = self.coordinator.send_command(
                focused,
                "scroll_to_search_match",
                &serde_json::json!({ "index": next_0 }),
            );
        }
    }

    /// Clear the search index and hide the search bar.
    pub(crate) fn clear_search(&mut self) {
        self.search_bar.set_match_info(0, 0);
        if let Some(focused) = self.coordinator.focused() {
            let _ = self.coordinator.send_command(
                focused,
                "update_search",
                &serde_json::json!({ "query": "" }),
            );
        }
        debug!("Search cleared");
    }

    /// Render the search bar overlay quads and text into the pre-allocated
    /// chrome buffers. Called from `render.rs` after the terminal cells are
    /// rendered.
    pub(crate) fn build_search_overlay(
        &mut self,
        quads: &mut Vec<phantom_renderer::quads::QuadInstance>,
        glyphs: &mut Vec<phantom_renderer::text::GlyphInstance>,
    ) {
        if !self.search_bar.visible {
            return;
        }

        // Find the pane rect for the focused adapter.
        let pane_rect = self
            .coordinator
            .focused()
            .and_then(|id| self.coordinator.pane_id_for(id))
            .and_then(|pane| self.layout.get_pane_rect(pane).ok());

        let pane_rect = match pane_rect {
            Some(r) => phantom_ui::layout::Rect {
                x: r.x,
                y: r.y,
                width: r.width,
                height: r.height,
            },
            None => return,
        };

        // Quads.
        quads.extend(self.search_bar.render_quads(&pane_rect));

        // Text segments → glyph instances.
        for seg in self.search_bar.render_text(&pane_rect) {
            self.render_overlay_text(&seg.text, seg.x, seg.y, seg.color, glyphs);
        }
    }
}
