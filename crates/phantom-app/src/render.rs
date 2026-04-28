//! GPU render pipeline: 3-pass rendering (scene → post-FX → overlay),
//! boot sequence, and terminal grid rendering with container chrome.

use anyhow::Result;
use wgpu::CommandEncoderDescriptor;

use phantom_renderer::grid::{GridCell, GridRenderData};
use phantom_ui::widgets::Widget;
use phantom_renderer::postfx::PostFxParams;
use phantom_renderer::quads::QuadInstance as QI;

use crate::app::{App, AppState};
use crate::pane::{pane_inner_rect, container_rect, scrollbar_track_rect, scrollbar_thumb_rect,
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
        crate::profile_scope!("render");
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

            // -- Settings panel overlay --
            if self.settings_panel.open {
                self.build_settings_overlay(screen_size, &mut chrome_quads, &mut chrome_glyphs);
            }

            // -- Context menu overlay --
            if self.context_menu.visible {
                self.build_context_menu_overlay(screen_size, &mut chrome_quads, &mut chrome_glyphs);
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

        crate::profile_frame!();

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
        _quads: &mut Vec<QI>,
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
        quads: &mut Vec<QI>,
        glyphs: &mut Vec<phantom_renderer::text::GlyphInstance>,
        chrome_quads: &mut Vec<QI>,
        chrome_glyphs: &mut Vec<phantom_renderer::text::GlyphInstance>,
    ) {
        // -- Fullscreen mode: render only the fullscreen adapter at full size --
        if let Some(fs_app_id) = self.fullscreen_pane {
            // Get render output from the fullscreen adapter.
            let coordinator_outputs = self.coordinator.render_all(&self.layout, self.cell_size);
            if let Some((_id, _rect, ro)) = coordinator_outputs.iter().find(|(id, _, _)| *id == fs_app_id) {
                if let Some(ref grid) = ro.grid {
                    let margin = 12.0;
                    let origin = (margin, margin);

                    self.pool_grid_cells.clear();
                    self.pool_grid_cells.extend(grid.cells.iter().map(|tc| GridCell { ch: tc.ch, fg: tc.fg, bg: tc.bg }));

                    let (mut bg_quads, mut glyph_instances) = GridRenderData::prepare(
                        &self.pool_grid_cells, grid.cols, grid.rows,
                        &mut self.text_renderer, &mut self.atlas, &self.gpu.queue,
                        origin, self.cell_size,
                    );
                    quads.append(&mut bg_quads);
                    glyphs.append(&mut glyph_instances);

                    if let Some(ref cursor) = grid.cursor {
                        if cursor.visible {
                            let cx = margin + cursor.col as f32 * self.cell_size.0;
                            let cy = margin + cursor.row as f32 * self.cell_size.1;
                            quads.push(QI {
                                pos: [cx, cy],
                                size: [self.cell_size.0, self.cell_size.1],
                                color: [self.theme.colors.cursor[0], self.theme.colors.cursor[1], self.theme.colors.cursor[2], 0.7],
                                border_radius: 0.0,
                            });
                        }
                    }

                    // Selection highlight overlay.
                    if let Some(ref sel) = ro.selection {
                        self.push_selection_quads(sel, margin, margin, grid.cols, quads);
                    }
                }
                self.render_overlay_text("ESC to exit fullscreen", screen_size[0] - 200.0, screen_size[1] - 30.0, [0.4, 0.6, 0.4, 0.5], chrome_glyphs);
            }
            return;
        }

        // -- Coordinator two-phase render: collect outputs from all registered adapters --
        let coordinator_outputs = self.coordinator.render_all(&self.layout, self.cell_size);
        let focused_app = self.coordinator.focused();
        let mut coord_titles: Vec<(String, f32, f32, [f32; 4])> = Vec::new();

        for (app_id, _rect, ro) in &coordinator_outputs {
            // Render quads (colored rectangles).
            for q in &ro.quads {
                quads.push(QI { pos: [q.x, q.y], size: [q.w, q.h], color: q.color, border_radius: 0.0 });
            }
            // Render text segments.
            for seg in &ro.text_segments {
                self.render_overlay_text(&seg.text, seg.x, seg.y, seg.color, glyphs);
            }

            // -- Container chrome for this adapter --
            // Skip chrome when only 1 tiled pane (no need for borders/title).
            let tiled_count = coordinator_outputs.len();
            let is_focused = focused_app == Some(*app_id);
            let pane_id = self.coordinator.pane_id_for(*app_id);
            let layout_rect = pane_id.and_then(|pid| self.layout.get_pane_rect(pid).ok());

            if tiled_count <= 1 {
                // Single pane — skip chrome, just render the grid below.
            } else if let Some(layout_rect) = layout_rect {
                let pane_rect = container_rect(layout_rect, self.cell_size);
                let inner_rect = pane_inner_rect(self.cell_size, pane_rect);
                let bg = self.theme.colors.background;

                // -- Scene pass chrome (gets CRT effects) --

                // Drop shadow.
                quads.push(QI {
                    pos: [pane_rect.x + 3.0, pane_rect.y + 3.0],
                    size: [pane_rect.width, pane_rect.height],
                    color: [0.0, 0.0, 0.0, 0.15],
                    border_radius: 6.0,
                });

                // Container background.
                let cont_bg = [
                    (bg[0] + 0.06).min(1.0),
                    (bg[1] + 0.10).min(1.0),
                    (bg[2] + 0.06).min(1.0),
                    1.0,
                ];
                quads.push(QI {
                    pos: [pane_rect.x, pane_rect.y],
                    size: [pane_rect.width, pane_rect.height],
                    color: cont_bg,
                    border_radius: 6.0,
                });

                // Title strip.
                let title_h = self.cell_size.1 * CONTAINER_TITLE_H_CELLS;
                let title_bg = if is_focused {
                    [bg[0] * 1.6 + 0.04, bg[1] * 2.0 + 0.06, bg[2] * 1.6 + 0.04, 1.0]
                } else {
                    [bg[0] * 1.3 + 0.02, bg[1] * 1.5 + 0.03, bg[2] * 1.3 + 0.02, 1.0]
                };
                quads.push(QI {
                    pos: [pane_rect.x, pane_rect.y],
                    size: [pane_rect.width, title_h],
                    color: title_bg,
                    border_radius: 6.0,
                });

                // -- Overlay pass chrome (crisp, post-CRT) --

                // Focus-aware border (1px lines).
                let border_color = if is_focused {
                    [0.2, 1.0, 0.5, 0.85]
                } else {
                    [0.15, 0.25, 0.18, 0.60]
                };
                let t = 1.0;
                // top
                chrome_quads.push(QI { pos: [pane_rect.x, pane_rect.y], size: [pane_rect.width, t], color: border_color, border_radius: 0.0 });
                // bottom
                chrome_quads.push(QI { pos: [pane_rect.x, pane_rect.y + pane_rect.height - t], size: [pane_rect.width, t], color: border_color, border_radius: 0.0 });
                // left
                chrome_quads.push(QI { pos: [pane_rect.x, pane_rect.y], size: [t, pane_rect.height], color: border_color, border_radius: 0.0 });
                // right
                chrome_quads.push(QI { pos: [pane_rect.x + pane_rect.width - t, pane_rect.y], size: [t, pane_rect.height], color: border_color, border_radius: 0.0 });

                // Title text.
                let dot_color = if is_focused {
                    [0.2, 1.0, 0.5, 1.0]
                } else {
                    [0.4, 0.5, 0.4, 0.7]
                };
                let title_x = pane_rect.x + self.cell_size.0 * CONTAINER_PAD_X_CELLS;
                let title_y = pane_rect.y + (title_h - self.cell_size.1) * 0.5;
                let app_type = self.coordinator.registry().get(*app_id)
                    .map(|e| e.app_type.as_str())
                    .unwrap_or("app");
                let grid_dims = ro.grid.as_ref()
                    .map(|g| format!("  {}x{}", g.cols, g.rows))
                    .unwrap_or_default();
                let title_text = format!("\u{25cf} {}{}", app_type, grid_dims);
                coord_titles.push((title_text, title_x, title_y, dot_color));

                // -- Scrollbar (overlay pass — crisp, post-CRT) --
                if let Some(ref scroll) = ro.scroll {
                    if scroll.history_size > 0 {
                        let track = scrollbar_track_rect(inner_rect);
                        // Track background.
                        chrome_quads.push(QI {
                            pos: [track.x, track.y],
                            size: [track.width, track.height],
                            color: [0.2, 0.2, 0.2, 0.3],
                            border_radius: 3.0,
                        });
                        // Thumb.
                        if let Some(thumb) = scrollbar_thumb_rect(track, scroll.display_offset, scroll.history_size, scroll.visible_rows) {
                            let thumb_color = if is_focused {
                                [0.2, 1.0, 0.5, 0.4]
                            } else {
                                [0.5, 0.5, 0.5, 0.5]
                            };
                            chrome_quads.push(QI {
                                pos: [thumb.x, thumb.y],
                                size: [thumb.width, thumb.height],
                                color: thumb_color,
                                border_radius: 3.0,
                            });
                        }
                    }
                }
            }

            // Render terminal grid data (the critical path for terminal adapters).
            if let Some(ref grid) = ro.grid {
                self.pool_grid_cells.clear();
                self.pool_grid_cells.extend(grid.cells.iter().map(|tc| GridCell { ch: tc.ch, fg: tc.fg, bg: tc.bg }));

                let origin = (grid.origin.0, grid.origin.1);
                let (mut bg_quads, mut glyph_instances) = GridRenderData::prepare(
                    &self.pool_grid_cells, grid.cols, grid.rows,
                    &mut self.text_renderer, &mut self.atlas, &self.gpu.queue,
                    origin, self.cell_size,
                );
                quads.append(&mut bg_quads);
                glyphs.append(&mut glyph_instances);

                // Render cursor.
                if let Some(ref cursor) = grid.cursor {
                    if cursor.visible {
                        let cx = grid.origin.0 + cursor.col as f32 * self.cell_size.0;
                        let cy = grid.origin.1 + cursor.row as f32 * self.cell_size.1;
                        quads.push(QI {
                            pos: [cx, cy],
                            size: [self.cell_size.0, self.cell_size.1],
                            color: self.theme.colors.cursor,
                            border_radius: 0.0,
                        });
                    }
                }

                // Selection highlight overlay.
                if let Some(ref sel) = ro.selection {
                    self.push_selection_quads(sel, grid.origin.0, grid.origin.1, grid.cols, quads);
                }
            }
        }

        // -- Coordinator adapter title text (overlay pass — crisp, readable) --
        for (label, x, y, color) in &coord_titles {
            self.render_overlay_text(label, *x, *y, *color, chrome_glyphs);
        }

        // -- Monitor panels: hstack (sysmon left, appmon right) --
        let monitor_height = self.render_monitor_hstack(screen_size, quads, glyphs);
        // -- Agent panels below monitors --
        let agent_height = self.render_agent_panels_offset(
            screen_size, quads, glyphs, monitor_height,
        );
        let _panels_height = monitor_height + agent_height;

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

    /// Push selection highlight quads for the given selection range.
    fn push_selection_quads(
        &self,
        sel: &phantom_adapter::adapter::SelectionRange,
        origin_x: f32,
        origin_y: f32,
        grid_cols: usize,
        quads: &mut Vec<QI>,
    ) {
        for row in sel.start_row..=sel.end_row {
            let start_col = if row == sel.start_row { sel.start_col } else { 0 };
            let end_col = if row == sel.end_row {
                sel.end_col
            } else {
                grid_cols.saturating_sub(1)
            };
            if end_col < start_col {
                continue;
            }

            let x = origin_x + start_col as f32 * self.cell_size.0;
            let y = origin_y + row as f32 * self.cell_size.1;
            let w = (end_col - start_col + 1) as f32 * self.cell_size.0;
            let h = self.cell_size.1;

            quads.push(QI {
                pos: [x, y],
                size: [w, h],
                color: [0.2, 0.5, 1.0, 0.3], // Blue highlight overlay
                border_radius: 0.0,
            });
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
