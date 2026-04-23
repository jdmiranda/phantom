//! Overlay rendering: command bar, debug shader HUD, AI suggestion overlay.
//!
//! All overlays render post-CRT directly onto the surface — crisp, clean,
//! no post-processing applied.

use phantom_renderer::quads::QuadInstance;

use crate::app::App;

impl App {
    /// Render the Quake-style drop-down console.
    ///
    /// Drops from the top of the screen, ~40% height. Shows scrollback
    /// history with a command input line at the bottom of the pane.
    pub(crate) fn build_console_overlay(
        &mut self,
        screen_size: [f32; 2],
        quads: &mut Vec<QuadInstance>,
        glyphs: &mut Vec<phantom_renderer::text::GlyphInstance>,
    ) {
        let line_height = self.cell_size.1;
        let cell_w = self.cell_size.0;
        let padding = 10.0;
        let slide = self.console.slide;

        // Console takes 40% of screen height, scaled by slide animation.
        let full_height = (screen_size[1] * 0.40).max(120.0);
        let console_height = full_height * slide;
        let console_width = screen_size[0];

        // Slide the entire console upward by offsetting Y.
        let y_offset = -full_height + console_height; // starts at -full_height, ends at 0

        // -- Background: fully opaque dark --
        quads.push(QuadInstance {
            pos: [0.0, y_offset],
            size: [console_width, full_height],
            color: [0.01, 0.01, 0.03, 1.0],
            border_radius: 0.0,
        });

        // -- Glowing bottom edge (cyan scanline, the Quake separator) --
        // Positioned at the visible bottom of the sliding console.
        quads.push(QuadInstance {
            pos: [0.0, y_offset + full_height - 2.0],
            size: [console_width, 2.0],
            color: [0.0, 0.85, 0.95, 0.9],
            border_radius: 0.0,
        });
        quads.push(QuadInstance {
            pos: [0.0, y_offset + full_height],
            size: [console_width, 6.0],
            color: [0.0, 0.4, 0.5, 0.25],
            border_radius: 0.0,
        });

        // Don't render text content until slide is far enough to be readable.
        if slide < 0.15 {
            return;
        }

        // -- Title bar --
        let title_h = line_height + 4.0;
        quads.push(QuadInstance {
            pos: [0.0, y_offset],
            size: [console_width, title_h],
            color: [0.02, 0.04, 0.06, 0.95],
            border_radius: 0.0,
        });

        let title = "PHANTOM CONSOLE";
        self.render_overlay_text(title, padding, y_offset + 2.0, [0.0, 0.8, 0.9, 0.8], glyphs);

        let hint = "[`] close  [Tab] complete  [PgUp/Dn] scroll";
        let hint_x = console_width - (hint.len() as f32 * cell_w) - padding;
        self.render_overlay_text(hint, hint_x, y_offset + 2.0, [0.3, 0.5, 0.5, 0.5], glyphs);

        // -- Input line (at bottom of console pane) --
        let input_y = y_offset + full_height - line_height - padding;
        let input_bar_y = input_y - 4.0;
        let input_bar_h = line_height + 8.0;

        quads.push(QuadInstance {
            pos: [0.0, input_bar_y],
            size: [console_width, input_bar_h],
            color: [0.02, 0.03, 0.05, 0.95],
            border_radius: 0.0,
        });
        quads.push(QuadInstance {
            pos: [0.0, input_bar_y],
            size: [console_width, 1.0],
            color: [0.1, 0.3, 0.2, 0.5],
            border_radius: 0.0,
        });

        let input_display = format!("> {}_", self.console.input);
        self.render_overlay_text(
            &input_display,
            padding,
            input_y,
            [0.2, 1.0, 0.5, 1.0],
            glyphs,
        );

        // -- Scrollback history --
        let history_top = y_offset + title_h + 4.0;
        let history_bottom = input_bar_y - 4.0;
        let visible_lines = ((history_bottom - history_top) / line_height).floor().max(0.0) as usize;

        if visible_lines == 0 {
            return;
        }

        let total = self.console.history.len();
        let scroll = self.console.scroll_offset;
        let end = total.saturating_sub(scroll);
        let start = end.saturating_sub(visible_lines);
        let max_chars = ((console_width - padding * 2.0) / cell_w) as usize;

        // Reuse pooled buffer to avoid per-frame allocation.
        let mut lines_buf = std::mem::take(&mut self.overlay_line_buf);
        lines_buf.clear();

        for line in &self.console.history[start..end] {
            let (text, color) = match line {
                crate::console::ConsoleLine::Command(cmd) => {
                    (format!("> {cmd}"), [0.2, 1.0, 0.5, 0.9])
                }
                crate::console::ConsoleLine::Output(msg) => {
                    (msg.clone(), [0.6, 0.75, 0.7, 0.85])
                }
                crate::console::ConsoleLine::Error(msg) => {
                    (msg.clone(), [1.0, 0.35, 0.25, 0.9])
                }
                crate::console::ConsoleLine::System(msg) => {
                    (msg.clone(), [0.0, 0.8, 0.9, 0.8])
                }
            };
            // Truncate in-place if needed.
            let truncated = if text.chars().count() > max_chars {
                text.chars().take(max_chars).collect()
            } else {
                text
            };
            lines_buf.push((truncated, color));
        }

        for (i, (text, color)) in lines_buf.iter().enumerate() {
            let y = history_top + (i as f32 * line_height);
            self.render_overlay_text(text, padding, y, *color, glyphs);
        }

        // Return the buffer to the pool.
        self.overlay_line_buf = lines_buf;

        // Scroll indicator when not at bottom.
        if scroll > 0 {
            let indicator = format!("-- {scroll} more --");
            let ix = (console_width - indicator.len() as f32 * cell_w) / 2.0;
            self.render_overlay_text(
                &indicator,
                ix,
                history_bottom - line_height,
                [0.5, 0.7, 0.5, 0.6],
                glyphs,
            );
        }
    }

    /// Build the AI suggestion overlay (post-CRT, crisp).
    pub(crate) fn build_suggestion_overlay(
        &mut self,
        screen_size: [f32; 2],
        quads: &mut Vec<QuadInstance>,
        glyphs: &mut Vec<phantom_renderer::text::GlyphInstance>,
    ) {
        let suggestion = match self.suggestion {
            Some(ref s) => s,
            None => return,
        };

        let line_height = 22.0;
        let padding = 12.0;
        let option_lines = suggestion.options.len();
        let total_lines = 1 + option_lines; // text line + option lines
        let box_height = (total_lines as f32) * line_height + padding * 2.0;
        let box_width = 500.0_f32.min(screen_size[0] - 32.0);
        let box_x = (screen_size[0] - box_width) / 2.0;
        let box_y = screen_size[1] - box_height - 60.0; // above status bar

        // Background panel.
        quads.push(QuadInstance {
            pos: [box_x, box_y],
            size: [box_width, box_height],
            color: [0.02, 0.04, 0.08, 0.92],
            border_radius: 4.0,
        });

        // Cyan border.
        for &(pos, size) in &[
            ([box_x, box_y], [box_width, 1.0]),
            ([box_x, box_y + box_height - 1.0], [box_width, 1.0]),
            ([box_x, box_y], [1.0, box_height]),
            ([box_x + box_width - 1.0, box_y], [1.0, box_height]),
        ] {
            quads.push(QuadInstance {
                pos,
                size,
                color: [0.0, 0.8, 0.9, 0.7],
                border_radius: 0.0,
            });
        }

        // Suggestion text.
        let prefix = "[PHANTOM]: ";
        let display = format!("{prefix}{}", suggestion.text);
        let text_color = [0.1, 0.9, 0.6, 1.0];
        let cells: Vec<phantom_renderer::text::TerminalCell> = display
            .chars()
            .map(|ch| phantom_renderer::text::TerminalCell { ch, fg: text_color })
            .collect();

        if !cells.is_empty() {
            let cols = cells.len();
            let origin = (box_x + padding, box_y + padding);
            let mut g = self.text_renderer.prepare_glyphs(
                &mut self.atlas,
                &self.gpu.queue,
                &cells,
                cols,
                origin,
            );
            glyphs.append(&mut g);
        }

        // Option labels.
        let opt_color = [0.6, 0.8, 0.4, 1.0];
        for (i, (key, label)) in suggestion.options.iter().enumerate() {
            let opt_text = format!("  [{key}] {label}");
            let opt_cells: Vec<phantom_renderer::text::TerminalCell> = opt_text
                .chars()
                .map(|ch| phantom_renderer::text::TerminalCell { ch, fg: opt_color })
                .collect();

            if !opt_cells.is_empty() {
                let cols = opt_cells.len();
                let origin = (box_x + padding, box_y + padding + (i as f32 + 1.0) * line_height);
                let mut g = self.text_renderer.prepare_glyphs(
                    &mut self.atlas,
                    &self.gpu.queue,
                    &opt_cells,
                    cols,
                    origin,
                );
                glyphs.append(&mut g);
            }
        }
    }

    /// Build the debug shader HUD overlay (post-CRT, crisp).
    pub(crate) fn build_debug_hud(
        &mut self,
        screen_size: [f32; 2],
        quads: &mut Vec<QuadInstance>,
        glyphs: &mut Vec<phantom_renderer::text::GlyphInstance>,
    ) {
        let sp = &self.theme.shader_params;
        let params: &[(&str, f32)] = &[
            ("scanlines", sp.scanline_intensity),
            ("bloom", sp.bloom_intensity),
            ("aberration", sp.chromatic_aberration),
            ("curvature", sp.curvature),
            ("vignette", sp.vignette_intensity),
            ("noise", sp.noise_intensity),
        ];

        let hud_width = 340.0;
        let line_height = 20.0;
        let hud_height = (params.len() as f32 + 3.0) * line_height;
        let hud_x = screen_size[0] - hud_width - 16.0;
        let hud_y = 16.0;

        // Background panel.
        quads.push(QuadInstance {
            pos: [hud_x, hud_y],
            size: [hud_width, hud_height],
            color: [0.02, 0.02, 0.04, 0.90],
            border_radius: 4.0,
        });

        // Border.
        let border_color = [0.15, 0.4, 0.2, 0.8];
        for &(pos, size) in &[
            ([hud_x, hud_y], [hud_width, 1.0]),
            ([hud_x, hud_y + hud_height - 1.0], [hud_width, 1.0]),
            ([hud_x, hud_y], [1.0, hud_height]),
            ([hud_x + hud_width - 1.0, hud_y], [1.0, hud_height]),
        ] {
            quads.push(QuadInstance {
                pos,
                size,
                color: border_color,
                border_radius: 0.0,
            });
        }

        let mut text_y = hud_y + 6.0;
        let text_x = hud_x + 12.0;

        // Title
        let title = "SHADER DEBUG";
        self.render_overlay_text(title, text_x, text_y, [0.55, 1.0, 0.72, 1.0], glyphs);
        text_y += line_height;

        // Separator
        let sep = "────────────────────────────────────";
        self.render_overlay_text(sep, text_x, text_y, [0.15, 0.4, 0.2, 0.5], glyphs);
        text_y += line_height;

        // Param lines
        for (i, &(name, value)) in params.iter().enumerate() {
            let selected = i == self.debug_hud_selected;
            let bar_len = 20;
            let filled = ((value * bar_len as f32).round() as usize).min(bar_len);
            let empty = bar_len - filled;
            let bar: String = "█".repeat(filled) + &"░".repeat(empty);
            let marker = if selected { "▶" } else { " " };
            let line = format!("{marker} {name:<14} {bar} {value:.2}");

            let color = if selected {
                [0.2, 1.0, 0.5, 1.0]
            } else {
                [0.5, 0.7, 0.5, 0.8]
            };
            self.render_overlay_text(&line, text_x, text_y, color, glyphs);
            text_y += line_height;
        }

        // Help line
        text_y += 4.0;
        let help = "[Tab] next  [↑↓] adjust  [Esc] close";
        self.render_overlay_text(help, text_x, text_y, [0.3, 0.5, 0.3, 0.6], glyphs);
    }

    /// Render the monitor hstack: sysmon (left) + appmon (right).
    /// Side by side when both visible, full-width when only one is.
    /// Returns total height consumed.
    pub(crate) fn render_monitor_hstack(
        &mut self,
        screen_size: [f32; 2],
        quads: &mut Vec<QuadInstance>,
        glyphs: &mut Vec<phantom_renderer::text::GlyphInstance>,
    ) -> f32 {
        // Reserve space for sysmon as soon as it's toggled on — even before
        // the first poll arrives — so the layout doesn't jump when data shows up.
        let show_sys = self.sysmon_visible;
        let show_app = self.appmon_visible;

        if !show_sys && !show_app {
            return 0.0;
        }

        let margin = 12.0;
        let gap = 8.0;
        let panel_y = 30.0;
        let full_width = screen_size[0] - margin * 2.0;

        let (sys_width, app_width, app_x) = match (show_sys, show_app) {
            (true, true) => {
                let half = (full_width - gap) / 2.0;
                (half, half, margin + half + gap)
            }
            (true, false) => (full_width, 0.0, 0.0),
            (false, true) => (0.0, full_width, margin),
            (false, false) => unreachable!(),
        };

        let mut max_h: f32 = 0.0;

        if show_sys {
            let lines = if let Some(ref stats) = self.sysmon.latest {
                crate::sysmon::build_monitor_lines(stats)
            } else {
                vec![("▮ Polling...".into(), [0.4, 0.7, 0.5, 0.5])]
            };
            let h = self.render_monitor_panel(
                "SYSTEM RESOURCES",
                &lines,
                margin, panel_y, sys_width,
                [0.15, 0.5, 0.3, 0.7],
                quads, glyphs,
            );
            max_h = max_h.max(h);
        }

        if show_app {
            let metrics = self.collect_metrics();
            let lines = crate::appmon::build_appmon_lines(&metrics);
            let h = self.render_monitor_panel(
                "APP DIAGNOSTICS",
                &lines,
                app_x, panel_y, app_width,
                [0.15, 0.3, 0.5, 0.7],
                quads, glyphs,
            );
            max_h = max_h.max(h);
        }

        max_h + 4.0
    }

    /// Render a generic monitor panel at a given position/width.
    /// Returns the panel height.
    fn render_monitor_panel(
        &mut self,
        title: &str,
        lines: &[(String, [f32; 4])],
        panel_x: f32,
        panel_y: f32,
        panel_width: f32,
        border_color: [f32; 4],
        quads: &mut Vec<QuadInstance>,
        glyphs: &mut Vec<phantom_renderer::text::GlyphInstance>,
    ) -> f32 {
        let line_height = self.cell_size.1;
        let padding = 8.0;
        let title_h = line_height + 6.0;
        let content_h = lines.len() as f32 * line_height;
        let panel_height = title_h + content_h + padding * 2.0;

        let bg = self.theme.colors.background;

        // Background.
        quads.push(QuadInstance {
            pos: [panel_x, panel_y],
            size: [panel_width, panel_height],
            color: [
                (bg[0] + 0.02).min(1.0),
                (bg[1] + 0.04).min(1.0),
                (bg[2] + 0.06).min(1.0),
                1.0,
            ],
            border_radius: 6.0,
        });

        // Border.
        let t = 1.0;
        quads.push(QuadInstance { pos: [panel_x, panel_y], size: [panel_width, t], color: border_color, border_radius: 0.0 });
        quads.push(QuadInstance { pos: [panel_x, panel_y + panel_height - t], size: [panel_width, t], color: border_color, border_radius: 0.0 });
        quads.push(QuadInstance { pos: [panel_x, panel_y], size: [t, panel_height], color: border_color, border_radius: 0.0 });
        quads.push(QuadInstance { pos: [panel_x + panel_width - t, panel_y], size: [t, panel_height], color: border_color, border_radius: 0.0 });

        // Title bar.
        quads.push(QuadInstance {
            pos: [panel_x, panel_y],
            size: [panel_width, title_h],
            color: [bg[0] * 1.3 + 0.02, bg[1] * 1.5 + 0.03, bg[2] * 1.5 + 0.04, 1.0],
            border_radius: 6.0,
        });

        // Title text.
        let title_display = format!("▮ {title}");
        self.render_overlay_text(
            &title_display,
            panel_x + padding,
            panel_y + 3.0,
            [0.3, 0.8, 0.5, 0.9],
            glyphs,
        );

        // Content lines.
        let text_y_start = panel_y + title_h + padding;
        for (i, (text, color)) in lines.iter().enumerate() {
            self.render_overlay_text(
                text,
                panel_x + padding,
                text_y_start + i as f32 * line_height,
                *color,
                glyphs,
            );
        }

        panel_height
    }

    /// Render agent panes as a stacked panel above the terminal (scene pass).
    ///
    /// `y_offset` shifts the panel down (e.g. when sysmon is above it).
    /// Returns the total height consumed by agent panels.
    pub(crate) fn render_agent_panels_offset(
        &mut self,
        screen_size: [f32; 2],
        quads: &mut Vec<QuadInstance>,
        glyphs: &mut Vec<phantom_renderer::text::GlyphInstance>,
        y_offset: f32,
    ) -> f32 {
        if self.agent_panes.is_empty() {
            return 0.0;
        }

        let line_height = self.cell_size.1;
        let cell_w = self.cell_size.0;
        let padding = 10.0;
        let margin = 12.0;
        let title_h = line_height + 8.0;
        let panel_width = screen_size[0] - margin * 2.0;
        let max_panel_h = (screen_size[1] * 0.35).min(400.0);
        let panel_x = margin;
        let panel_y = 30.0 + y_offset; // below tab bar + sysmon

        let max_visible = ((max_panel_h - title_h - padding * 2.0) / line_height)
            .floor().max(1.0) as usize;

        // Rebuild caches first (needs &mut).
        for p in &mut self.agent_panes {
            p.tail_lines(max_visible);
        }

        // Now collect render data from the updated caches.
        struct AgentRenderData {
            task: String,
            status: crate::agent_pane::AgentPaneStatus,
            display_lines: Vec<String>,
        }

        let agent_data: Vec<AgentRenderData> = self.agent_panes.iter().map(|p| {
            AgentRenderData {
                task: p.task.clone(),
                status: p.status,
                display_lines: p.cached_lines.clone(),
            }
        }).collect();

        let pane = &agent_data[0];
        let display_lines = &pane.display_lines;
        let content_h = display_lines.len() as f32 * line_height;
        let panel_height = (title_h + content_h + padding * 2.0).min(max_panel_h);

        let bg = self.theme.colors.background;

        // Panel background (slightly different tint from terminal container).
        quads.push(QuadInstance {
            pos: [panel_x, panel_y],
            size: [panel_width, panel_height],
            color: [
                (bg[0] + 0.03).min(1.0),
                (bg[1] + 0.06).min(1.0),
                (bg[2] + 0.08).min(1.0),
                1.0,
            ],
            border_radius: 6.0,
        });

        // Status-aware border.
        let border_color = match pane.status {
            crate::agent_pane::AgentPaneStatus::Working => [0.0, 0.8, 0.9, 0.8],
            crate::agent_pane::AgentPaneStatus::Done => [0.2, 1.0, 0.5, 0.8],
            crate::agent_pane::AgentPaneStatus::Failed => [1.0, 0.3, 0.2, 0.8],
        };
        let t = 1.0;
        quads.push(QuadInstance { pos: [panel_x, panel_y], size: [panel_width, t], color: border_color, border_radius: 0.0 });
        quads.push(QuadInstance { pos: [panel_x, panel_y + panel_height - t], size: [panel_width, t], color: border_color, border_radius: 0.0 });
        quads.push(QuadInstance { pos: [panel_x, panel_y], size: [t, panel_height], color: border_color, border_radius: 0.0 });
        quads.push(QuadInstance { pos: [panel_x + panel_width - t, panel_y], size: [t, panel_height], color: border_color, border_radius: 0.0 });

        // Title bar.
        let title_bg = [bg[0] * 1.4 + 0.02, bg[1] * 1.6 + 0.04, bg[2] * 1.8 + 0.06, 1.0];
        quads.push(QuadInstance {
            pos: [panel_x, panel_y],
            size: [panel_width, title_h],
            color: title_bg,
            border_radius: 6.0,
        });

        // Title text.
        let status_char = match pane.status {
            crate::agent_pane::AgentPaneStatus::Working => "●",
            crate::agent_pane::AgentPaneStatus::Done => "✓",
            crate::agent_pane::AgentPaneStatus::Failed => "✗",
        };
        let title_text = format!(
            "{status_char} AGENT  {}",
            if pane.task.chars().count() > 60 {
                format!("{}…", pane.task.chars().take(59).collect::<String>())
            } else {
                pane.task.clone()
            }
        );
        self.render_overlay_text(
            &title_text,
            panel_x + padding,
            panel_y + 4.0,
            border_color,
            glyphs,
        );

        // Output text.
        let text_color = [0.6, 0.85, 0.75, 0.9];
        let text_y_start = panel_y + title_h + padding;
        let max_chars = ((panel_width - padding * 2.0) / cell_w) as usize;
        for (i, line) in display_lines.iter().enumerate() {
            let truncated: String = line.chars().take(max_chars).collect();
            self.render_overlay_text(
                &truncated,
                panel_x + padding,
                text_y_start + i as f32 * line_height,
                text_color,
                glyphs,
            );
        }

        // Return total height consumed (panel + gap before terminal).
        panel_height + 4.0
    }

    /// Helper: render a text string directly into the overlay glyph buffer.
    pub(crate) fn render_overlay_text(
        &mut self,
        text: &str,
        x: f32,
        y: f32,
        color: [f32; 4],
        glyphs: &mut Vec<phantom_renderer::text::GlyphInstance>,
    ) {
        let cells: Vec<phantom_renderer::text::TerminalCell> = text
            .chars()
            .map(|ch| phantom_renderer::text::TerminalCell { ch, fg: color })
            .collect();
        if !cells.is_empty() {
            let cols = cells.len();
            let mut g = self.text_renderer.prepare_glyphs(
                &mut self.atlas,
                &self.gpu.queue,
                &cells,
                cols,
                (x, y),
            );
            glyphs.append(&mut g);
        }
    }
}
