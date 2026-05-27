//! GPU render pipeline: 3-pass rendering (scene → post-FX → overlay),
//! boot sequence, and terminal grid rendering with container chrome.

use anyhow::Result;
use wgpu::CommandEncoderDescriptor;

use phantom_renderer::grid::{GridCell, GridRenderData};
use phantom_renderer::postfx::{PostFxParams, PostFxThemeParams};
use phantom_renderer::quads::QuadInstance as QI;
use phantom_ui::widgets::Widget;

use phantom_adapter::adapter::CursorShape;

use crate::app::{App, AppState};
use crate::pane::{
    CONTAINER_PAD_X_CELLS, CONTAINER_TITLE_H_CELLS, container_rect, pane_inner_rect,
    scrollbar_thumb_rect, scrollbar_track_rect,
};

/// Return the (width, height) of the cursor quad for the given shape.
///
/// - `Block`: full cell — the safe default that matches legacy behaviour.
/// - `Bar`: a 2-pixel-wide vertical bar on the left edge of the cell.
/// - `Underline`: a 2-pixel-tall horizontal bar at the bottom of the cell.
///
/// The caller is responsible for adjusting the Y origin for `Underline` so the
/// bar sits flush with the cell bottom rather than the cell top.
fn cursor_shape_size(shape: CursorShape, cell_size: (f32, f32)) -> (f32, f32) {
    match shape {
        CursorShape::Block => (cell_size.0, cell_size.1),
        CursorShape::Bar => (2.0_f32.max(cell_size.0 * 0.1), cell_size.1),
        CursorShape::Underline => (cell_size.0, 2.0_f32.max(cell_size.1 * 0.1)),
    }
}

impl App {
    // Render
    // -----------------------------------------------------------------------

    /// Render one frame.
    ///
    /// This is the main render path, called every `RedrawRequested`. It:
    /// 1. Renders the scene (boot or terminal) into the PostFx offscreen texture.
    /// 2. Composites CRT effects onto the final surface texture.
    ///
    /// Returns `Ok(())` immediately (skipping all GPU work) when neither the
    /// scene graph has dirty nodes nor `force_redraw` is set.  This prevents
    /// redundant buffer uploads on frames where nothing changed.
    pub fn render(&mut self) -> Result<()> {
        crate::profile_scope!("render");

        // When `--screenshot` is queued, force a redraw so the early return
        // below doesn't starve the screenshot trigger of a fresh frame.
        if self.screenshot_after_frame.is_some() {
            self.force_redraw = true;
        }

        // Skip the entire GPU pipeline when the scene is clean and no
        // external event has requested a forced repaint.
        if !self.scene.has_dirty_nodes() && !self.force_redraw {
            return Ok(());
        }

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
        // Color-emoji instances (Rgba8UnormSrgb atlas, no FG tint — fixes #356).
        let mut all_color_glyphs = std::mem::take(&mut self.pool_color_glyphs);
        let mut chrome_quads = std::mem::take(&mut self.pool_chrome_quads);
        let mut chrome_glyphs = std::mem::take(&mut self.pool_chrome_glyphs);
        all_quads.clear();
        all_glyphs.clear();
        all_color_glyphs.clear();
        chrome_quads.clear();
        chrome_glyphs.clear();

        match self.state {
            AppState::Boot => {
                self.render_boot(screen_size, &mut all_quads, &mut all_glyphs, &mut all_color_glyphs);
            }
            AppState::Terminal => {
                self.render_terminal(
                    screen_size,
                    &mut all_quads,
                    &mut all_glyphs,
                    &mut all_color_glyphs,
                    &mut chrome_quads,
                    &mut chrome_glyphs,
                );
            }
        }

        // Upload to GPU — monochrome pipeline.
        self.quad_renderer
            .prepare(&self.gpu.device, &self.gpu.queue, &all_quads, screen_size);
        self.grid_renderer
            .prepare(&self.gpu.device, &self.gpu.queue, &all_glyphs, screen_size);
        // Upload color-emoji instances to the RGBA pipeline (fixes #356).
        self.color_grid_renderer
            .prepare(&self.gpu.device, &self.gpu.queue, &all_color_glyphs, screen_size);

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

            // Draw monochrome glyphs (text, Nerd Font icons).
            self.grid_renderer
                .render(&mut pass, self.atlas.bind_group());

            // Draw full-color emoji glyphs (no FG tint — fixes #356).
            self.color_grid_renderer
                .render(&mut pass, self.color_atlas.bind_group());

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
            // When the user has disabled CRT via Settings, force the scale
            // to zero so every shader intensity collapses to "no effect".
            let crt_scale = if !self.crt_enabled {
                0.0
            } else if self.state == AppState::Boot {
                self.boot.crt_intensity()
            } else {
                1.0
            };

            let params = PostFxParams::from_theme(&PostFxThemeParams {
                scanline_intensity: sp.scanline_intensity * crt_scale,
                bloom_intensity: sp.bloom_intensity * crt_scale,
                chromatic_aberration: sp.chromatic_aberration * crt_scale,
                curvature: sp.curvature * crt_scale,
                vignette_intensity: sp.vignette_intensity * crt_scale,
                noise_intensity: sp.noise_intensity * crt_scale,
                glow_color: sp.glow_color,
                time: elapsed,
                width,
                height,
            });

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

            // -- Keybind help overlay (F1 / ?) — rendered above all other overlays --
            if self.keybind_help.visible() {
                self.build_keybind_help_overlay(screen_size, &mut chrome_quads, &mut chrome_glyphs);
            }

            // -- Find-in-terminal search bar (Cmd+F) --
            if self.search_bar.visible {
                self.build_search_overlay(&mut chrome_quads, &mut chrome_glyphs);
            }

            // -- Alt-screen bezier tether (issue #323) --
            if !self.alt_screen_secondaries.is_empty() {
                self.build_alt_screen_tether(&mut chrome_quads);
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
                self.grid_renderer
                    .render(&mut pass, self.atlas.bind_group());
            }
        }

        // Submit and present.
        self.gpu.queue.submit(std::iter::once(encoder.finish()));
        output.present();

        // Clear scene-graph dirty flags so static frames are skipped next time,
        // and reset the force-redraw latch set by external events.
        self.scene.clear_all_dirty();
        self.force_redraw = false;

        // Return pooled Vecs for reuse next frame (retains capacity).
        self.pool_quads = all_quads;
        self.pool_glyphs = all_glyphs;
        self.pool_color_glyphs = all_color_glyphs;
        self.pool_chrome_quads = chrome_quads;
        self.pool_chrome_glyphs = chrome_glyphs;

        // -- One-shot screenshot trigger --
        // Skip the first 4 frames so adapter chrome, AppHeads, gradients,
        // and the live-dot pulse have time to settle before we grab the
        // surface texture. After capture, set `quit_requested = true` so
        // the winit event loop tears down cleanly.
        self.frames_since_startup = self.frames_since_startup.saturating_add(1);
        if self.frames_since_startup >= 4
            && let Some(path) = self.screenshot_after_frame.take()
        {
            if let Err(e) = self.capture_one_shot_screenshot(&path) {
                log::error!("one-shot screenshot failed: {e}");
                eprintln!("phantom: --screenshot capture failed: {e}");
            } else {
                eprintln!("phantom: --screenshot saved to {}", path.display());
            }
            self.quit_requested = true;
        }

        crate::profile_frame!();

        Ok(())
    }

    /// Capture the current scene texture (post-FX target) and save as PNG.
    ///
    /// Used by the `--screenshot <PATH>` one-shot mode to grab the rendered
    /// frame from a headless invocation. Routes through the same
    /// `capture_frame` / `save_screenshot` helpers as the runtime
    /// `screenshot` console command.
    fn capture_one_shot_screenshot(&mut self, path: &std::path::Path) -> anyhow::Result<()> {
        use std::time::{SystemTime, UNIX_EPOCH};
        use phantom_renderer::screenshot::{ScreenshotMetadata, capture_frame, save_screenshot};

        let texture = self.postfx.scene_texture();
        let width = texture.width();
        let height = texture.height();
        let pixels = capture_frame(&self.gpu.device, &self.gpu.queue, texture, width, height)
            .map_err(|e| anyhow::anyhow!("capture_frame failed: {e}"))?;

        // Swap BGRA→RGBA on Metal/D3D12.
        let pixels_rgba = match self.gpu.format {
            wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb => {
                let mut out = pixels;
                for px in out.chunks_exact_mut(4) {
                    px.swap(0, 2);
                }
                out
            }
            _ => pixels,
        };

        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let metadata = ScreenshotMetadata {
            timestamp,
            width,
            height,
            theme: self.theme.name.clone(),
            pane_count: self.coordinator.adapter_count(),
            project: self.context.as_ref().map(|c| c.name.clone()),
            branch: self
                .context
                .as_ref()
                .and_then(|c| c.git.as_ref().map(|g| g.branch.clone())),
        };

        // Ensure parent directory exists.
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }

        save_screenshot(&pixels_rgba, width, height, &metadata, path)
            .map_err(|e| anyhow::anyhow!("save_screenshot failed: {e}"))?;

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Boot rendering
    // -----------------------------------------------------------------------

    /// Build quads and glyphs for the boot sequence.
    fn render_boot(
        &mut self,
        _screen_size: [f32; 2],
        _quads: &mut Vec<QI>,
        glyphs: &mut Vec<phantom_renderer::text::GlyphInstance>,
        color_glyphs: &mut Vec<phantom_renderer::text::GlyphInstance>,
    ) {
        let boot_lines = self.boot.visible_text();
        let opacity = self.boot.screen_opacity();

        // Convert boot text lines to grid cells and render them.
        for line in &boot_lines {
            if line.chars_visible == 0 {
                continue;
            }

            let visible_text: String = line.text.chars().take(line.chars_visible).collect();

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
                    bold: false,
                    italic: false,
                })
                .collect();

            if cells.is_empty() {
                continue;
            }

            let cols = cells.len();
            let origin = (
                self.cell_size.0 * 2.0,
                self.cell_size.1 * line.row as f32,
            );

            let batch = self.text_renderer.prepare_glyphs(
                &mut self.atlas,
                &mut self.color_atlas,
                &self.gpu.queue,
                &cells,
                cols,
                origin,
            );

            // Per-character phosphor halo (item 1, path A). Emitted before the
            // sharp glyphs so the halo instances render behind in instance
            // order — gives every glyph a soft phosphor rim without modifying
            // the text shader.
            phantom_renderer::text::append_glow_halos(&batch.mono, glyphs, None, None);
            glyphs.extend(batch.mono);
            color_glyphs.extend(batch.color);
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
        color_glyphs: &mut Vec<phantom_renderer::text::GlyphInstance>,
        chrome_quads: &mut Vec<QI>,
        chrome_glyphs: &mut Vec<phantom_renderer::text::GlyphInstance>,
    ) {
        // -- Fullscreen mode: render only the fullscreen adapter at full size --
        if let Some(fs_app_id) = self.fullscreen_pane {
            // Get render output from the fullscreen adapter.
            let coordinator_outputs = self.coordinator.render_all(&self.layout, self.cell_size);
            if let Some((_id, _rect, ro)) = coordinator_outputs
                .iter()
                .find(|(id, _, _)| *id == fs_app_id)
            {
                if let Some(ref grid) = ro.grid {
                    let margin = 12.0;
                    let origin = (margin, margin);

                    self.pool_grid_cells.clear();
                    self.pool_grid_cells
                        .extend(grid.cells.iter().map(|tc| GridCell {
                            ch: tc.ch,
                            fg: tc.fg,
                            bg: tc.bg,
                            bold: tc.bold,
                            italic: tc.italic,
                        }));

                    let (mut bg_quads, batch) = GridRenderData::prepare(
                        &self.pool_grid_cells,
                        grid.cols,
                        grid.rows,
                        &mut self.text_renderer,
                        &mut self.atlas,
                        &mut self.color_atlas,
                        &self.gpu.queue,
                        origin,
                        self.cell_size,
                    );
                    quads.append(&mut bg_quads);
                    phantom_renderer::text::append_glow_halos(&batch.mono, glyphs, None, None);
                    glyphs.extend(batch.mono);
                    color_glyphs.extend(batch.color);

                    if let Some(ref cursor) = grid.cursor {
                        let blink_visible = !cursor.blinking || self.cursor_blink.is_visible();
                        if cursor.visible && blink_visible {
                            let cx = margin + cursor.col as f32 * self.cell_size.0;
                            let cy = margin + cursor.row as f32 * self.cell_size.1;
                            let (cw, ch) = cursor_shape_size(cursor.shape, self.cell_size);
                            let cy = if matches!(cursor.shape, CursorShape::Underline) {
                                cy + self.cell_size.1 - ch
                            } else {
                                cy
                            };
                            quads.push(QI {
                                pos: [cx, cy],
                                size: [cw, ch],
                                color: [
                                    self.theme.colors.cursor[0],
                                    self.theme.colors.cursor[1],
                                    self.theme.colors.cursor[2],
                                    0.7,
                                ],
                                border_radius: 0.0,
                            });
                        }
                    }

                    // Selection is rendered via per-cell inversion in extract_grid_themed.
                }
                self.render_overlay_text(
                    "ESC to exit fullscreen",
                    screen_size[0] - 200.0,
                    screen_size[1] - 30.0,
                    [0.4, 0.6, 0.4, 0.5],
                    chrome_glyphs,
                );
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
                quads.push(QI {
                    pos: [q.x, q.y],
                    size: [q.w, q.h],
                    color: q.color,
                    border_radius: 0.0,
                });
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

            // Container chrome (border + shadow + body bg) is ALWAYS rendered,
            // not gated on multi-pane mode. The mockup's `.app { box-shadow:
            // 0 8px 24px rgba(0,0,0,0.5); border-radius: 10px; border: 1px
            // solid var(--frame-dim) }` is per-pane chrome that every adapter
            // sits inside, single-pane or tiled.
            //
            // Title strip rendering is still suppressed in single-pane mode
            // because the AppHead widget (rendered by each adapter) supplies
            // the per-app header — drawing both would double-stack chrome.
            if let Some(layout_rect) = layout_rect {
                let pane_rect = container_rect(layout_rect, self.cell_size);
                let inner_rect = pane_inner_rect(self.cell_size, pane_rect);
                let bg = self.theme.colors.background;

                // Per the mockup `.app { border-radius: 10px }` the container
                // corners use a 10 px radius (was 6 px — too tight, read
                // "blocky" instead of "soft chrome").
                const PANE_RADIUS: f32 = 10.0;

                // -- Scene pass chrome (gets CRT effects) --

                // Drop shadow — ALWAYS painted, every pane, every layout. The
                // mockup `.app { box-shadow: 0 8px 24px rgba(0,0,0,0.5) }`
                // is unconditional. Offset y=8, blur extends ~24px; we fake
                // the blur by stacking two soft shadow quads at growing
                // offsets / decreasing alphas — flat fill alone reads as a
                // hard drop, multi-layer reads as a soft halo.
                quads.push(QI {
                    pos: [pane_rect.x + 6.0, pane_rect.y + 10.0],
                    size: [pane_rect.width, pane_rect.height],
                    color: [0.0, 0.0, 0.0, 0.35],
                    border_radius: PANE_RADIUS + 4.0,
                });
                quads.push(QI {
                    pos: [pane_rect.x + 3.0, pane_rect.y + 5.0],
                    size: [pane_rect.width, pane_rect.height],
                    color: [0.0, 0.0, 0.0, 0.25],
                    border_radius: PANE_RADIUS + 2.0,
                });

                // Container background — mockup `.app-body { background:
                // linear-gradient(180deg, var(--surface-recessed), var(--bg)) }`
                // is approximated by stacking two quads: a recessed-tinted
                // top, fading to the base bg at the bottom. We render the
                // base bg first, then a faded top half-quad on top.
                let cont_bg = [
                    (bg[0] + 0.03).min(1.0),
                    (bg[1] + 0.05).min(1.0),
                    (bg[2] + 0.03).min(1.0),
                    1.0,
                ];
                quads.push(QI {
                    pos: [pane_rect.x, pane_rect.y],
                    size: [pane_rect.width, pane_rect.height],
                    color: cont_bg,
                    border_radius: PANE_RADIUS,
                });
                // Gradient top half — slightly brighter (raised feel) at
                // ~35 % alpha, fading visually as we render only the top
                // half of the pane. This is the cheap stacked-quad gradient
                // path; a proper linear interp would be a shader variant.
                let raised_tint = [
                    (bg[0] + 0.08).min(1.0),
                    (bg[1] + 0.12).min(1.0),
                    (bg[2] + 0.08).min(1.0),
                    0.35,
                ];
                quads.push(QI {
                    pos: [pane_rect.x, pane_rect.y],
                    size: [pane_rect.width, pane_rect.height * 0.5],
                    color: raised_tint,
                    border_radius: PANE_RADIUS,
                });

                // Title strip — only drawn in multi-pane mode. In single-pane
                // mode adapters use their own AppHead.
                if tiled_count > 1 {
                    let title_h = self.cell_size.1 * CONTAINER_TITLE_H_CELLS;
                    let title_bg = if is_focused {
                        [
                            bg[0] * 1.6 + 0.04,
                            bg[1] * 2.0 + 0.06,
                            bg[2] * 1.6 + 0.04,
                            1.0,
                        ]
                    } else {
                        [
                            bg[0] * 1.3 + 0.02,
                            bg[1] * 1.5 + 0.03,
                            bg[2] * 1.3 + 0.02,
                            1.0,
                        ]
                    };
                    quads.push(QI {
                        pos: [pane_rect.x, pane_rect.y],
                        size: [pane_rect.width, title_h],
                        color: title_bg,
                        border_radius: PANE_RADIUS,
                    });

                    // Title text.
                    let dot_color = if is_focused {
                        [0.2, 1.0, 0.5, 1.0]
                    } else {
                        [0.4, 0.5, 0.4, 0.7]
                    };
                    let title_x = pane_rect.x + self.cell_size.0 * CONTAINER_PAD_X_CELLS;
                    let title_y = pane_rect.y + (title_h - self.cell_size.1) * 0.5;
                    let app_type = self
                        .coordinator
                        .registry()
                        .get(*app_id)
                        .map(|e| e.app_type.as_str())
                        .unwrap_or("app");
                    let grid_dims = ro
                        .grid
                        .as_ref()
                        .map(|g| format!("  {}x{}", g.cols, g.rows))
                        .unwrap_or_default();
                    let title_text = format!("\u{25cf} {}{}", app_type, grid_dims);
                    coord_titles.push((title_text, title_x, title_y, dot_color));
                }

                // -- Overlay pass chrome (crisp, post-CRT) --

                // Focus-aware border (1px lines). Mockup `.app { border: 1px
                // solid var(--frame-dim) }`; `.app.focused { border-color:
                // var(--frame-active); box-shadow: var(--glow) }`. We use
                // the theme tokens so theme switches recolor the border.
                let border_color = if is_focused {
                    [0.2, 1.0, 0.5, 0.85]
                } else {
                    [0.15, 0.25, 0.18, 0.60]
                };
                let t = 1.0;
                // top
                chrome_quads.push(QI {
                    pos: [pane_rect.x, pane_rect.y],
                    size: [pane_rect.width, t],
                    color: border_color,
                    border_radius: 0.0,
                });
                // bottom
                chrome_quads.push(QI {
                    pos: [pane_rect.x, pane_rect.y + pane_rect.height - t],
                    size: [pane_rect.width, t],
                    color: border_color,
                    border_radius: 0.0,
                });
                // left
                chrome_quads.push(QI {
                    pos: [pane_rect.x, pane_rect.y],
                    size: [t, pane_rect.height],
                    color: border_color,
                    border_radius: 0.0,
                });
                // right
                chrome_quads.push(QI {
                    pos: [pane_rect.x + pane_rect.width - t, pane_rect.y],
                    size: [t, pane_rect.height],
                    color: border_color,
                    border_radius: 0.0,
                });

                // -- Scrollbar (overlay pass — crisp, post-CRT) --
                if let Some(ref scroll) = ro.scroll
                    && scroll.history_size > 0 {
                        let track = scrollbar_track_rect(inner_rect);
                        // Track background.
                        chrome_quads.push(QI {
                            pos: [track.x, track.y],
                            size: [track.width, track.height],
                            color: [0.2, 0.2, 0.2, 0.3],
                            border_radius: 3.0,
                        });
                        // Thumb.
                        if let Some(thumb) = scrollbar_thumb_rect(
                            track,
                            scroll.display_offset,
                            scroll.history_size,
                            scroll.visible_rows,
                        ) {
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

            // Render terminal grid data (the critical path for terminal adapters).
            if let Some(ref grid) = ro.grid {
                self.pool_grid_cells.clear();
                self.pool_grid_cells
                    .extend(grid.cells.iter().map(|tc| GridCell {
                        ch: tc.ch,
                        fg: tc.fg,
                        bg: tc.bg,
                        bold: tc.bold,
                        italic: tc.italic,
                    }));

                let origin = (grid.origin.0, grid.origin.1);
                let (mut bg_quads, batch) = GridRenderData::prepare(
                    &self.pool_grid_cells,
                    grid.cols,
                    grid.rows,
                    &mut self.text_renderer,
                    &mut self.atlas,
                    &mut self.color_atlas,
                    &self.gpu.queue,
                    origin,
                    self.cell_size,
                );
                quads.append(&mut bg_quads);
                phantom_renderer::text::append_glow_halos(&batch.mono, glyphs, None, None);
                glyphs.extend(batch.mono);
                color_glyphs.extend(batch.color);

                // Render cursor.
                // The cursor is drawn only when:
                //   (a) the terminal reports it as visible (DECTCEM on), AND
                //   (b) the clock-driven blink timer is in the "on" half-cycle
                //       (or the terminal did not request blinking at all).
                // This decouples blink timing from repaint frequency so that
                // spinner-heavy TUIs (gemini, htop, lazygit) that repaint the
                // prompt row on every frame no longer cause the cursor quad to
                // strobe through rapid cell churn.
                if let Some(ref cursor) = grid.cursor {
                    let blink_visible = !cursor.blinking || self.cursor_blink.is_visible();
                    if cursor.visible && blink_visible {
                        let cx = grid.origin.0 + cursor.col as f32 * self.cell_size.0;
                        let cy = grid.origin.1 + cursor.row as f32 * self.cell_size.1;
                        let (cw, ch) = cursor_shape_size(cursor.shape, self.cell_size);
                        let cy = if matches!(cursor.shape, CursorShape::Underline) {
                            cy + self.cell_size.1 - ch
                        } else {
                            cy
                        };
                        quads.push(QI {
                            pos: [cx, cy],
                            size: [cw, ch],
                            color: self.theme.colors.cursor,
                            border_radius: 0.0,
                        });
                    }
                }

                // Selection is rendered via per-cell inversion in extract_grid_themed.
            }
        }

        // -- Coordinator adapter title text (overlay pass — crisp, readable) --
        for (label, x, y, color) in &coord_titles {
            self.render_overlay_text(label, *x, *y, *color, chrome_glyphs);
        }

        // -- Monitor panels: hstack (sysmon left, appmon right) --
        self.render_monitor_hstack(screen_size, quads, glyphs);

        // -- Top theme strip (matches mockup `.controls` row) --
        // Sits above the tab bar. Sync runtime state into the widget once per
        // frame so theme switches / CRT toggles redraw without a rebuild.
        self.theme_strip
            .set_active(&self.theme.name);
        self.theme_strip
            .set_crt(self.theme.shader_params.scanline_intensity > 0.001);
        if let Ok(theme_rect) = self.layout.get_theme_strip_rect() {
            // Build a live tokens snapshot so the swatch ring and CRT chrome
            // colors stay in step with the active theme.
            let tokens = phantom_ui::tokens::Tokens::new(
                self.theme.token_color_roles(),
                phantom_ui::RenderCtx::new(self.cell_size, 1.0),
            );
            let strip = self.theme_strip.clone().with_tokens(tokens);
            let strip_quads = strip.render_quads(&theme_rect);
            quads.extend(strip_quads);
            let strip_texts = strip.render_text(&theme_rect);
            self.render_text_segments(&strip_texts, glyphs);
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

        // -- Sec.8 notification banner (top-of-screen, post-CRT overlay) --
        // The `NotificationCenter` (in `update.rs`) decides whether a banner
        // is active; we just translate `current_banner()` → widget output
        // and emit it onto the chrome (overlay) buffers so the message is
        // crisp / unaffected by CRT post-FX. Hidden state is a no-op: the
        // widget's `render_quads` / `render_text` return empty `Vec`s.
        self.build_notification_banner_overlay(screen_size, chrome_quads, chrome_glyphs);
    }

    /// Build the top-of-screen notification banner onto the overlay buffers.
    ///
    /// Reads the highest-severity active banner from
    /// `App::notifications` and feeds it into a transient
    /// `NotificationBanner` widget. Pinned to the top of the window across
    /// the full width with a fixed height
    /// (`NOTIFICATION_BANNER_HEIGHT`); hidden when there is no active
    /// banner.
    fn build_notification_banner_overlay(
        &mut self,
        screen_size: [f32; 2],
        chrome_quads: &mut Vec<QI>,
        chrome_glyphs: &mut Vec<phantom_renderer::text::GlyphInstance>,
    ) {
        use phantom_ui::widgets::{BannerSeverity, NOTIFICATION_BANNER_HEIGHT, NotificationBanner};

        let Some(banner_data) = self.notifications.current_banner() else {
            return;
        };

        // Map the app-side `Severity` enum onto the widget's
        // `BannerSeverity` (decoupled so phantom-ui doesn't depend on
        // phantom-app's notification module).
        let severity = match banner_data.severity {
            crate::notifications::Severity::Info => BannerSeverity::Info,
            crate::notifications::Severity::Warn => BannerSeverity::Warn,
            crate::notifications::Severity::Danger => BannerSeverity::Danger,
        };
        let message = banner_data.message.clone();

        // Stateless per frame: instantiate, fill, render, drop.
        let mut banner = NotificationBanner::new();
        banner.set_render_ctx(phantom_ui::RenderCtx::new(self.cell_size, 1.0));
        banner.set_message(message, severity);

        let banner_rect = phantom_ui::layout::Rect {
            x: 0.0,
            y: 0.0,
            width: screen_size[0],
            height: NOTIFICATION_BANNER_HEIGHT,
        };

        use phantom_ui::widgets::Widget;
        let banner_quads = banner.render_quads(&banner_rect);
        for q in banner_quads {
            chrome_quads.push(q);
        }
        let banner_texts = banner.render_text(&banner_rect);
        self.render_text_segments(&banner_texts, chrome_glyphs);
    }

    /// Convert widget TextSegments into GlyphInstances via the text renderer.
    fn render_text_segments(
        &mut self,
        segments: &[phantom_ui::widgets::TextSegment],
        glyphs: &mut Vec<phantom_renderer::text::GlyphInstance>,
    ) {
        for seg in segments {
            self.text_cell_buf.clear();
            self.text_cell_buf.extend(
                seg.text
                    .chars()
                    .map(|ch| phantom_renderer::text::TerminalCell { ch, fg: seg.color, bold: false, italic: false }),
            );

            if self.text_cell_buf.is_empty() {
                continue;
            }

            let cols = self.text_cell_buf.len();
            let origin = (seg.x, seg.y);

            let batch = self.text_renderer.prepare_glyphs(
                &mut self.atlas,
                &mut self.color_atlas,
                &self.gpu.queue,
                &self.text_cell_buf,
                cols,
                origin,
            );

            // Widget text segments are always monochrome; color batch is empty.
            // Glow halos are emitted ahead of the sharp glyphs so the halo
            // instances render behind the crisp text (item 1, path A — see
            // `phantom_renderer::text::append_glow_halos`).
            phantom_renderer::text::append_glow_halos(&batch.mono, glyphs, None, None);
            glyphs.extend(batch.mono);
        }
    }
}

// ---------------------------------------------------------------------------
// Headless unit tests — no GPU / winit required
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::cursor_shape_size;
    use phantom_adapter::adapter::CursorShape;

    // -----------------------------------------------------------------------
    // cursor_shape_size — Block
    // -----------------------------------------------------------------------

    #[test]
    fn block_returns_full_cell() {
        let (w, h) = cursor_shape_size(CursorShape::Block, (12.0, 24.0));
        assert_eq!(w, 12.0);
        assert_eq!(h, 24.0);
    }

    #[test]
    fn block_zero_size_cell_returns_zero() {
        let (w, h) = cursor_shape_size(CursorShape::Block, (0.0, 0.0));
        assert_eq!(w, 0.0);
        assert_eq!(h, 0.0);
    }

    #[test]
    fn block_large_cell_returns_full_size() {
        let (w, h) = cursor_shape_size(CursorShape::Block, (1000.0, 2000.0));
        assert_eq!(w, 1000.0);
        assert_eq!(h, 2000.0);
    }

    // -----------------------------------------------------------------------
    // cursor_shape_size — Bar (2-px-min vertical bar)
    // -----------------------------------------------------------------------

    #[test]
    fn bar_normal_cell_is_10pct_width_and_full_height() {
        // 12 * 0.1 = 1.2 → clamped to 2.0 minimum
        let (w, h) = cursor_shape_size(CursorShape::Bar, (12.0, 24.0));
        assert_eq!(w, 2.0, "bar width should clamp to 2 px minimum");
        assert_eq!(h, 24.0, "bar height should equal full cell height");
    }

    #[test]
    fn bar_wide_cell_uses_10pct_of_width() {
        // 100 * 0.1 = 10.0 → exceeds 2 px floor
        let (w, h) = cursor_shape_size(CursorShape::Bar, (100.0, 200.0));
        assert_eq!(w, 10.0);
        assert_eq!(h, 200.0);
    }

    #[test]
    fn bar_zero_width_cell_clamps_to_2px() {
        let (w, h) = cursor_shape_size(CursorShape::Bar, (0.0, 20.0));
        assert_eq!(w, 2.0, "zero-width cell must clamp bar width to 2 px");
        assert_eq!(h, 20.0);
    }

    #[test]
    fn bar_very_large_cell_uses_10pct() {
        let (w, h) = cursor_shape_size(CursorShape::Bar, (10_000.0, 5_000.0));
        assert_eq!(w, 1_000.0);
        assert_eq!(h, 5_000.0);
    }

    #[test]
    fn bar_exactly_20px_wide_uses_10pct() {
        // 20 * 0.1 = 2.0 — sits exactly at the minimum boundary
        let (w, h) = cursor_shape_size(CursorShape::Bar, (20.0, 30.0));
        assert_eq!(w, 2.0);
        assert_eq!(h, 30.0);
    }

    #[test]
    fn bar_just_below_20px_wide_clamps_to_2px() {
        // 19 * 0.1 = 1.9 → clamped to 2 px
        let (w, h) = cursor_shape_size(CursorShape::Bar, (19.0, 30.0));
        assert_eq!(w, 2.0);
        assert_eq!(h, 30.0);
    }

    // -----------------------------------------------------------------------
    // cursor_shape_size — Underline (2-px-min horizontal bar)
    // -----------------------------------------------------------------------

    #[test]
    fn underline_normal_cell_is_full_width_and_10pct_height() {
        // 24 * 0.1 = 2.4 → exceeds 2 px floor
        let (w, h) = cursor_shape_size(CursorShape::Underline, (12.0, 24.0));
        assert_eq!(w, 12.0, "underline width should equal full cell width");
        assert!(h >= 2.0, "underline height must be at least 2 px");
        let expected_h = 2.0_f32.max(24.0 * 0.1);
        assert!((h - expected_h).abs() < f32::EPSILON);
    }

    #[test]
    fn underline_short_cell_clamps_to_2px_height() {
        // 10 * 0.1 = 1.0 → clamped to 2 px
        let (w, h) = cursor_shape_size(CursorShape::Underline, (15.0, 10.0));
        assert_eq!(w, 15.0);
        assert_eq!(h, 2.0, "short cell must clamp underline height to 2 px");
    }

    #[test]
    fn underline_zero_height_cell_clamps_to_2px() {
        let (w, h) = cursor_shape_size(CursorShape::Underline, (20.0, 0.0));
        assert_eq!(w, 20.0);
        assert_eq!(h, 2.0, "zero-height cell must clamp underline height to 2 px");
    }

    #[test]
    fn underline_very_large_cell_uses_10pct_height() {
        let (w, h) = cursor_shape_size(CursorShape::Underline, (5_000.0, 10_000.0));
        assert_eq!(w, 5_000.0);
        assert_eq!(h, 1_000.0);
    }

    #[test]
    fn underline_exactly_20px_tall_uses_10pct() {
        // 20 * 0.1 = 2.0 — sits exactly at the minimum boundary
        let (w, h) = cursor_shape_size(CursorShape::Underline, (30.0, 20.0));
        assert_eq!(w, 30.0);
        assert_eq!(h, 2.0);
    }

    // -----------------------------------------------------------------------
    // cursor_shape_size — typical font metrics (regression anchors)
    // -----------------------------------------------------------------------

    /// Typical HiDPI cell at 2× scale: ~9 × 20 logical pixels.
    #[test]
    fn regression_hidpi_cell_block() {
        let (w, h) = cursor_shape_size(CursorShape::Block, (9.0, 20.0));
        assert_eq!((w, h), (9.0, 20.0));
    }

    #[test]
    fn regression_hidpi_cell_bar() {
        // 9 * 0.1 = 0.9 → clamped to 2.0
        let (w, h) = cursor_shape_size(CursorShape::Bar, (9.0, 20.0));
        assert_eq!(w, 2.0);
        assert_eq!(h, 20.0);
    }

    #[test]
    fn regression_hidpi_cell_underline() {
        // 20 * 0.1 = 2.0 — at floor
        let (w, h) = cursor_shape_size(CursorShape::Underline, (9.0, 20.0));
        assert_eq!(w, 9.0);
        assert_eq!(h, 2.0);
    }
}
