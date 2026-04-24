//! GPU render pipeline: 3-pass rendering (scene → post-FX → overlay),
//! boot sequence, and terminal grid rendering with container chrome.

use anyhow::Result;
use log::warn;
use wgpu::CommandEncoderDescriptor;

use phantom_renderer::grid::{GridCell, GridRenderData};
use phantom_renderer::postfx::PostFxParams;
use phantom_renderer::quads::QuadInstance;
use phantom_terminal::output::{self, CursorShape, TerminalThemeColors};
use phantom_ui::widgets::Widget;

use phantom_renderer::quads::QuadInstance as QI;

use crate::app::{App, AppState};
use crate::pane::{pane_inner_rect, container_rect,
    CONTAINER_PAD_X_CELLS, CONTAINER_TITLE_H_CELLS};

impl App {
    // Render
    // -----------------------------------------------------------------------

    /// Render one frame.
    ///
    /// This is the main render path, called every `RedrawRequested`. It:
    /// 1. Renders the scene (boot or terminal) into the PostFx offscreen texture.
    /// 2. Composites CRT effects onto the final surface texture.
    pub fn render(&mut self) -> Result<()> {
        let output = self.gpu.surface.get_current_texture()?;
        let surface_view = output.texture.create_view(&Default::default());

        let width = self.gpu.surface_config.width;
        let height = self.gpu.surface_config.height;
        let screen_size = [width as f32, height as f32];

        let mut encoder = self
            .gpu
            .device
            .create_command_encoder(&CommandEncoderDescriptor {
                label: Some("phantom-frame-encoder"),
            });

        // -----------------------------------------------------------------
        // Collect scene data (quads + glyphs) — must happen before
        // borrowing self.postfx.scene_view() to avoid borrow conflicts.
        // -----------------------------------------------------------------
        // Reuse pooled Vecs (clear + retain capacity instead of allocating).
        let mut all_quads = std::mem::take(&mut self.pool_quads);
        let mut all_glyphs = std::mem::take(&mut self.pool_glyphs);
        let mut chrome_quads = std::mem::take(&mut self.pool_chrome_quads);
        let mut chrome_glyphs = std::mem::take(&mut self.pool_chrome_glyphs);
        all_quads.clear();
        all_glyphs.clear();
        chrome_quads.clear();
        chrome_glyphs.clear();

        match self.state {
            AppState::Boot => {
                self.render_boot(
                    screen_size,
                    &mut all_quads,
                    &mut all_glyphs,
                );
            }
            AppState::Terminal => {
                self.render_terminal(
                    screen_size,
                    &mut all_quads,
                    &mut all_glyphs,
                    &mut chrome_quads,
                    &mut chrome_glyphs,
                );
            }
        }

        // Upload to GPU.
        self.quad_renderer
            .prepare(&self.gpu.device, &self.gpu.queue, &all_quads, screen_size);
        self.grid_renderer
            .prepare(&self.gpu.device, &self.gpu.queue, &all_glyphs, screen_size);

        // -----------------------------------------------------------------
        // Scene pass: render into the PostFx offscreen texture
        // -----------------------------------------------------------------
        {
            let bg = self.theme.colors.background;
            let scene_view = self.postfx.scene_view();

            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("scene-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: scene_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: bg[0] as f64,
                            g: bg[1] as f64,
                            b: bg[2] as f64,
                            a: bg[3] as f64,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });

            // Draw quads (backgrounds, cursor, UI chrome).
            self.quad_renderer.render(&mut pass);

            // Draw glyphs (text).
            self.grid_renderer
                .render(&mut pass, self.atlas.bind_group());

            // Draw video frame (goes through CRT post-processing).
            if self.video_renderer.has_frame() {
                // Center the video in the screen.
                let vw = self.video_playback.as_ref().map_or(0, |p| p.width) as f32;
                let vh = self.video_playback.as_ref().map_or(0, |p| p.height) as f32;
                let vx = (screen_size[0] - vw) / 2.0;
                let vy = (screen_size[1] - vh) / 2.0;
                self.video_renderer.prepare(
                    &self.gpu.device,
                    &self.gpu.queue,
                    screen_size,
                    [vx, vy],
                    [vw, vh],
                );
                self.video_renderer.render(&mut pass);
            }
        }

        // -----------------------------------------------------------------
        // Post-FX pass: composite CRT effects onto the surface
        // -----------------------------------------------------------------
        {
            let sp = &self.theme.shader_params;
            let elapsed = self.start_time.elapsed().as_secs_f32();

            // During boot, scale CRT effect intensities by the boot warmup ramp.
            let crt_scale = if self.state == AppState::Boot {
                self.boot.crt_intensity()
            } else {
                1.0
            };

            let params = PostFxParams::from_theme(
                sp.scanline_intensity * crt_scale,
                sp.bloom_intensity * crt_scale,
                sp.chromatic_aberration * crt_scale,
                sp.curvature * crt_scale,
                sp.vignette_intensity * crt_scale,
                sp.noise_intensity * crt_scale,
                sp.glow_color,
                elapsed,
                width,
                height,
            );

            self.postfx
                .render(&mut encoder, &surface_view, &self.gpu.queue, &params);
        }

        // -----------------------------------------------------------------
        // Pass 3: System overlay — rendered AFTER CRT, directly on surface.
        // No post-processing. Crisp, clean, always readable.
        // Includes: container chrome (tint, border, title) + command bar +
        // debug HUD + AI suggestion overlay.
        // -----------------------------------------------------------------
        {
            // Chrome vecs already contain container quads/glyphs from render_terminal.
            // Append command mode / debug HUD / suggestion overlays on top.

            // -- Quake console (renders during slide animation too) --
            if self.console.visible() {
                self.build_console_overlay(screen_size, &mut chrome_quads, &mut chrome_glyphs);
            }

            // -- Debug shader HUD --
            if self.debug_hud {
                self.build_debug_hud(screen_size, &mut chrome_quads, &mut chrome_glyphs);
            }

            // -- AI Suggestion overlay --
            if self.suggestion.is_some() {
                self.build_suggestion_overlay(screen_size, &mut chrome_quads, &mut chrome_glyphs);
            }


            if !chrome_quads.is_empty() || !chrome_glyphs.is_empty() {
                // Upload + render overlay in its own pass on the surface.
                self.quad_renderer.prepare(
                    &self.gpu.device,
                    &self.gpu.queue,
                    &chrome_quads,
                    screen_size,
                );
                self.grid_renderer.prepare(
                    &self.gpu.device,
                    &self.gpu.queue,
                    &chrome_glyphs,
                    screen_size,
                );

                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("system-overlay-pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &surface_view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Load, // preserve the CRT output
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                });

                self.quad_renderer.render(&mut pass);
                self.grid_renderer.render(&mut pass, self.atlas.bind_group());
            }
        }

        // Submit and present.
        self.gpu.queue.submit(std::iter::once(encoder.finish()));
        output.present();

        // Return pooled Vecs for reuse next frame (retains capacity).
        self.pool_quads = all_quads;
        self.pool_glyphs = all_glyphs;
        self.pool_chrome_quads = chrome_quads;
        self.pool_chrome_glyphs = chrome_glyphs;

        Ok(())
    }

    /// Build the command input overlay (post-CRT, crisp).

    // -----------------------------------------------------------------------
    // Boot rendering
    // -----------------------------------------------------------------------

    /// Build quads and glyphs for the boot sequence.
    fn render_boot(
        &mut self,
        _screen_size: [f32; 2],
        _quads: &mut Vec<QuadInstance>,
        glyphs: &mut Vec<phantom_renderer::text::GlyphInstance>,
    ) {
        let boot_lines = self.boot.visible_text();
        let opacity = self.boot.screen_opacity();

        // Convert boot text lines to grid cells and render them.
        // Each BootTextLine has a row, text, color, and chars_visible.
        // We render them as positioned text using the text renderer.
        for line in &boot_lines {
            if line.chars_visible == 0 {
                continue;
            }

            // Truncate text to chars_visible for the typewriter effect.
            let visible_text: String = line.text.chars().take(line.chars_visible).collect();

            // Convert to TerminalCells for the text renderer.
            let cells: Vec<phantom_renderer::text::TerminalCell> = visible_text
                .chars()
                .map(|ch| phantom_renderer::text::TerminalCell {
                    ch,
                    fg: [
                        line.color[0],
                        line.color[1],
                        line.color[2],
                        line.color[3] * opacity,
                    ],
                })
                .collect();

            if cells.is_empty() {
                continue;
            }

            let cols = cells.len();
            let origin = (
                self.cell_size.0 * 2.0, // left margin
                self.cell_size.1 * line.row as f32,
            );

            let mut line_glyphs = self.text_renderer.prepare_glyphs(
                &mut self.atlas,
                &self.gpu.queue,
                &cells,
                cols,
                origin,
            );

            glyphs.append(&mut line_glyphs);
        }
    }

    // -----------------------------------------------------------------------
    // Terminal rendering
    // -----------------------------------------------------------------------

    /// Build quads and glyphs for all terminal panes plus chrome.
    fn render_terminal(
        &mut self,
        screen_size: [f32; 2],
        quads: &mut Vec<QuadInstance>,
        glyphs: &mut Vec<phantom_renderer::text::GlyphInstance>,
        _chrome_quads: &mut Vec<QuadInstance>,
        _chrome_glyphs: &mut Vec<phantom_renderer::text::GlyphInstance>,
    ) {
        // -- Coordinator two-phase render: collect outputs from all registered adapters --
        // (Strangler fig: runs alongside the legacy pane loop below.)
        let coordinator_outputs = self.coordinator.render_all(&self.layout);
        for (_app_id, _rect, ro) in &coordinator_outputs {
            for q in &ro.quads {
                quads.push(QI {
                    pos: [q.x, q.y],
                    size: [q.w, q.h],
                    color: q.color,
                    border_radius: 0.0,
                });
            }
            for seg in &ro.text_segments {
                self.render_overlay_text(&seg.text, seg.x, seg.y, seg.color, glyphs);
            }
        }

        let _has_multiple = self.panes.len() > 1;
        let mut detached_labels: Vec<(usize, f32, f32, [f32; 4])> = Vec::with_capacity(self.panes.len());
        let mut container_titles: Vec<(usize, usize, f32, f32, [f32; 4])> = Vec::with_capacity(self.panes.len());

        // -- Monitor panels: hstack (sysmon left, appmon right) --
        let monitor_height = self.render_monitor_hstack(screen_size, quads, glyphs);
        // -- Agent panels below monitors --
        let agent_height = self.render_agent_panels_offset(
            screen_size, quads, glyphs, monitor_height,
        );
        let panels_height = monitor_height + agent_height;

        // Build theme-aware color mapping for terminal grid extraction.
        let theme_colors = TerminalThemeColors {
            foreground: self.theme.colors.foreground,
            background: self.theme.colors.background,
            cursor: self.theme.colors.cursor,
            ansi: Some(self.theme.colors.ansi),
        };

        for (pane_index, pane) in self.panes.iter().enumerate() {
            let is_focused = pane_index == self.focused_pane;

            // Skip panes whose scene node is marked invisible.
            if let Some(node) = self.scene.get(pane.scene_node) {
                if !node.visible {
                    continue;
                }
            }

            // -- Extract terminal grid with theme colors --
            let (render_cells, cols, rows, cursor) =
                output::extract_grid_themed(pane.terminal.term(), &theme_colors);

            // -- Get pane rectangle from layout engine --
            // When agent panes are active, shrink and shift the terminal down
            // to make room for the agent panel stacked above.
            let mut layout_rect = self.layout.get_pane_rect(pane.pane_id).unwrap_or_else(|e| {
                warn!("Layout missing for pane {:?} in render: {e}", pane.pane_id);
                phantom_ui::layout::Rect {
                    x: 0.0,
                    y: 30.0,
                    width: screen_size[0],
                    height: screen_size[1] - 54.0,
                }
            });
            if panels_height > 0.0 {
                layout_rect.y += panels_height;
                layout_rect.height -= panels_height;
                if layout_rect.height < self.cell_size.1 * 4.0 {
                    layout_rect.height = self.cell_size.1 * 4.0;
                }
            }

            // Inset by outer margin to create the "floating container" look.
            let pane_rect = container_rect(layout_rect, self.cell_size);

            // -- Inner rect: area inside container chrome where the grid draws --
            let inner_rect = pane_inner_rect(self.cell_size, pane_rect);
            let origin = (inner_rect.x, inner_rect.y);

            // -- App-container chrome ----
            // Non-detached panes only; detached panes have their own (cyan) chrome.
            //
            // Background + drop shadow → scene pass (gets CRT, draws UNDER text).
            // Border + title → overlay pass (stays crisp, draws OVER CRT).
            if !pane.is_detached {
                let bg = self.theme.colors.background;
                let cont_bg = [
                    (bg[0] + 0.06).min(1.0),
                    (bg[1] + 0.10).min(1.0),
                    (bg[2] + 0.06).min(1.0),
                    1.0,
                ];

                // Drop shadow (scene pass — gets CRT).
                quads.push(QuadInstance {
                    pos: [pane_rect.x + 3.0, pane_rect.y + 3.0],
                    size: [pane_rect.width, pane_rect.height],
                    color: [0.0, 0.0, 0.0, 0.35],
                    border_radius: 6.0,
                });

                // Container background (scene pass — gets CRT).
                quads.push(QuadInstance {
                    pos: [pane_rect.x, pane_rect.y],
                    size: [pane_rect.width, pane_rect.height],
                    color: cont_bg,
                    border_radius: 6.0,
                });

                // Title strip (scene pass — gets CRT).
                let title_h = self.cell_size.1 * CONTAINER_TITLE_H_CELLS;
                let title_bg = if is_focused {
                    [bg[0] * 1.6 + 0.04, bg[1] * 2.0 + 0.06, bg[2] * 1.6 + 0.04, 1.0]
                } else {
                    [bg[0] * 1.3 + 0.02, bg[1] * 1.5 + 0.03, bg[2] * 1.3 + 0.02, 1.0]
                };
                quads.push(QuadInstance {
                    pos: [pane_rect.x, pane_rect.y],
                    size: [pane_rect.width, title_h],
                    color: title_bg,
                    border_radius: 6.0,
                });

                // Focus-aware border (scene pass — curves with CRT alongside bg).
                let border_color = if is_focused {
                    [0.2, 1.0, 0.5, 0.85]
                } else {
                    [0.15, 0.25, 0.18, 0.60]
                };
                let t = 1.0;
                // top
                quads.push(QuadInstance { pos: [pane_rect.x, pane_rect.y], size: [pane_rect.width, t], color: border_color, border_radius: 0.0 });
                // bottom
                quads.push(QuadInstance { pos: [pane_rect.x, pane_rect.y + pane_rect.height - t], size: [pane_rect.width, t], color: border_color, border_radius: 0.0 });
                // left
                quads.push(QuadInstance { pos: [pane_rect.x, pane_rect.y], size: [t, pane_rect.height], color: border_color, border_radius: 0.0 });
                // right
                quads.push(QuadInstance { pos: [pane_rect.x + pane_rect.width - t, pane_rect.y], size: [t, pane_rect.height], color: border_color, border_radius: 0.0 });

                // Title text: "● shell · {cols}×{rows}"
                let dot_color = if is_focused {
                    [0.2, 1.0, 0.5, 1.0]
                } else {
                    [0.4, 0.5, 0.4, 0.7]
                };
                let title_x = pane_rect.x + self.cell_size.0 * CONTAINER_PAD_X_CELLS;
                let title_y = pane_rect.y + (title_h - self.cell_size.1) * 0.5;
                container_titles.push((cols, rows, title_x, title_y, dot_color));
            }

            // -- Convert RenderCells to GridCells --
            let mut grid_cells: Vec<GridCell> = render_cells
                .iter()
                .map(|rc| GridCell {
                    ch: rc.ch,
                    fg: rc.fg,
                    bg: rc.bg,
                })
                .collect();

            // -- Apply per-keystroke glitch effect --
            if is_focused {
                self.keystroke_fx.apply(&mut grid_cells, cols);
            }

            // -- Prepare background quads + glyph instances via GridRenderData --
            let (mut bg_quads, mut glyph_instances) = GridRenderData::prepare(
                &grid_cells,
                cols,
                rows,
                &mut self.text_renderer,
                &mut self.atlas,
                &self.gpu.queue,
                origin,
                self.cell_size,
            );

            quads.append(&mut bg_quads);
            glyphs.append(&mut glyph_instances);

            // -- Cursor quad (only for focused pane, or dim for unfocused) --
            if cursor.visible {
                let cursor_x = inner_rect.x + cursor.col as f32 * self.cell_size.0;
                let cursor_y = inner_rect.y + cursor.row as f32 * self.cell_size.1;
                let cursor_color = if is_focused {
                    self.theme.colors.cursor
                } else {
                    // Dim cursor for unfocused panes.
                    let c = self.theme.colors.cursor;
                    [c[0] * 0.3, c[1] * 0.3, c[2] * 0.3, c[3] * 0.4]
                };

                let cursor_quad = match cursor.shape {
                    CursorShape::Block => QuadInstance {
                        pos: [cursor_x, cursor_y],
                        size: [self.cell_size.0, self.cell_size.1],
                        color: [cursor_color[0], cursor_color[1], cursor_color[2], if is_focused { 0.7 } else { 0.3 }],
                        border_radius: 0.0,
                    },
                    CursorShape::Underline => QuadInstance {
                        pos: [cursor_x, cursor_y + self.cell_size.1 - 2.0],
                        size: [self.cell_size.0, 2.0],
                        color: cursor_color,
                        border_radius: 0.0,
                    },
                    CursorShape::Bar => QuadInstance {
                        pos: [cursor_x, cursor_y],
                        size: [2.0, self.cell_size.1],
                        color: cursor_color,
                        border_radius: 0.0,
                    },
                };
                quads.push(cursor_quad);
            }

            // -- Pane border --
            // Detached panes always get an animated cyan tether border + header.
            // Non-detached panes get the standard border when multiple panes exist.
            if pane.is_detached {
                let elapsed = self.start_time.elapsed().as_secs_f32();
                let pulse = (elapsed * 2.0).sin() * 0.15 + 0.85;
                let border_color = [0.0, pulse, pulse * 0.8, 0.9];
                let border_thickness = 2.0;

                // Top edge
                quads.push(QuadInstance {
                    pos: [pane_rect.x, pane_rect.y],
                    size: [pane_rect.width, border_thickness],
                    color: border_color,
                    border_radius: 0.0,
                });
                // Bottom edge
                quads.push(QuadInstance {
                    pos: [pane_rect.x, pane_rect.y + pane_rect.height - border_thickness],
                    size: [pane_rect.width, border_thickness],
                    color: border_color,
                    border_radius: 0.0,
                });
                // Left edge
                quads.push(QuadInstance {
                    pos: [pane_rect.x, pane_rect.y],
                    size: [border_thickness, pane_rect.height],
                    color: border_color,
                    border_radius: 0.0,
                });
                // Right edge
                quads.push(QuadInstance {
                    pos: [pane_rect.x + pane_rect.width - border_thickness, pane_rect.y],
                    size: [border_thickness, pane_rect.height],
                    color: border_color,
                    border_radius: 0.0,
                });

                // -- Detach header bar --
                let header_height = self.cell_size.1 + 4.0;
                // Dark semi-transparent header background.
                quads.push(QuadInstance {
                    pos: [pane_rect.x + border_thickness, pane_rect.y + border_thickness],
                    size: [pane_rect.width - border_thickness * 2.0, header_height],
                    color: [0.02, 0.06, 0.08, 0.85],
                    border_radius: 0.0,
                });

                // Tether indicator dot (animated).
                let dot_size = 6.0;
                let dot_x = pane_rect.x + border_thickness + 6.0;
                let dot_y = pane_rect.y + border_thickness + (header_height - dot_size) / 2.0;
                quads.push(QuadInstance {
                    pos: [dot_x, dot_y],
                    size: [dot_size, dot_size],
                    color: [0.0, pulse, pulse * 0.8, 1.0],
                    border_radius: 3.0,
                });

                // Process name label.
                let label_x = dot_x + dot_size + 4.0;
                let label_y = pane_rect.y + border_thickness + 2.0;
                let label_color = [0.0, pulse, pulse * 0.8, 1.0];

                detached_labels.push((pane_index, label_x, label_y, label_color));
            }
            // Note: non-detached pane borders are now drawn as part of the
            // app-container chrome at the top of the pane loop.
        }

        // -- Detached pane labels (rendered after the pane loop to avoid borrow issues) --
        for &(pi, x, y, color) in &detached_labels {
            use std::fmt::Write;
            let mut buf = std::mem::take(&mut self.title_buf);
            buf.clear();
            if let Some(pane) = self.panes.get(pi) {
                let _ = write!(buf, "  {} ", &pane.detached_label);
            }
            self.render_overlay_text(&buf, x, y, color, glyphs);
            self.title_buf = buf;
        }

        // -- App-container title text (scene pass — curves with CRT) --
        for &(cols, rows, x, y, color) in &container_titles {
            use std::fmt::Write;
            let mut buf = std::mem::take(&mut self.title_buf);
            buf.clear();
            let _ = write!(buf, "● shell  {}×{}", cols, rows);
            self.render_overlay_text(&buf, x, y, color, glyphs);
            self.title_buf = buf;
        }

        // -- Tab bar --
        if let Ok(tab_rect) = self.layout.get_tab_bar_rect() {
            let tab_quads = self.tab_bar.render_quads(&tab_rect);
            quads.extend(tab_quads);

            // Render tab bar text as glyphs.
            let tab_texts = self.tab_bar.render_text(&tab_rect);
            self.render_text_segments(&tab_texts, glyphs);
        }

        // -- Status bar --
        if let Ok(status_rect) = self.layout.get_status_bar_rect() {
            let status_quads = self.status_bar.render_quads(&status_rect);
            quads.extend(status_quads);

            // Render status bar text as glyphs.
            let status_texts = self.status_bar.render_text(&status_rect);
            self.render_text_segments(&status_texts, glyphs);

            // Command overlay moved to system overlay pass (post-CRT).
        }
    }

    /// Convert widget TextSegments into GlyphInstances via the text renderer.
    fn render_text_segments(
        &mut self,
        segments: &[phantom_ui::widgets::TextSegment],
        glyphs: &mut Vec<phantom_renderer::text::GlyphInstance>,
    ) {
        for seg in segments {
            self.text_cell_buf.clear();
            self.text_cell_buf.extend(seg.text.chars().map(|ch| {
                phantom_renderer::text::TerminalCell { ch, fg: seg.color }
            }));

            if self.text_cell_buf.is_empty() {
                continue;
            }

            let cols = self.text_cell_buf.len();
            let origin = (seg.x, seg.y);

            let mut seg_glyphs = self.text_renderer.prepare_glyphs(
                &mut self.atlas,
                &self.gpu.queue,
                &self.text_cell_buf,
                cols,
                origin,
            );

            glyphs.append(&mut seg_glyphs);
        }
    }


}
