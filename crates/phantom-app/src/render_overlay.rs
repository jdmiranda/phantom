//! Overlay rendering: command bar, debug shader HUD, AI suggestion overlay.
//!
//! All overlays render post-CRT directly onto the surface — crisp, clean,
//! no post-processing applied.

use phantom_renderer::quads::QuadInstance;

use crate::app::App;

impl App {
    pub(crate) fn build_command_overlay(
        &mut self,
        screen_size: [f32; 2],
        quads: &mut Vec<QuadInstance>,
        glyphs: &mut Vec<phantom_renderer::text::GlyphInstance>,
    ) {
        let bar_height = 28.0;
        let y = screen_size[1] - bar_height;
        let cmd_text = self.command_input.as_deref().unwrap_or("");
        let display = format!("> {cmd_text}_");

        // Dark background bar.
        quads.push(QuadInstance {
            pos: [0.0, y],
            size: [screen_size[0], bar_height],
            color: [0.02, 0.02, 0.04, 0.95],
            border_radius: 0.0,
        });

        // Command text.
        let color = [0.2, 1.0, 0.5, 1.0]; // bright green
        let cells: Vec<phantom_renderer::text::TerminalCell> = display
            .chars()
            .map(|ch| phantom_renderer::text::TerminalCell { ch, fg: color })
            .collect();

        if !cells.is_empty() {
            let cols = cells.len();
            let origin = (8.0, y + 4.0);
            let mut g = self.text_renderer.prepare_glyphs(
                &mut self.atlas,
                &self.gpu.queue,
                &cells,
                cols,
                origin,
            );
            glyphs.append(&mut g);
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
