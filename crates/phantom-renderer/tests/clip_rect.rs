// Phase 0.D — clip_rect concept tests.
//
// These tests verify the CPU-side contract of the per-instance clip-rect
// primitives: that the newtypes round-trip cleanly through `bytemuck`,
// that the "no clip" sentinel behaves as expected, and that the parallel
// upload buffer for `QuadInstance` + `ClipRect` packs in lockstep without
// padding surprises that would mis-align the GPU vertex stream.
//
// Tests are CPU-only — no `wgpu::Device` is created. The shader-level
// `discard` behavior is exercised manually per the documented procedure
// in `manual_visual_validation_procedure` below; running it under
// `cargo test -- --ignored` runs the no-op test placeholder so the
// procedure stays discoverable.

use phantom_renderer::quads::{ClipRect, QuadInstance};
use phantom_renderer::text::GlyphClipRect;

// ---------------------------------------------------------------------------
// ClipRect: defaults, sentinels, round-trip
// ---------------------------------------------------------------------------

#[test]
fn clip_rect_default_is_no_clip_sentinel() {
    let zero = ClipRect::default();
    assert_eq!(zero.xywh, [0.0; 4]);
    assert!(zero.is_none(), "zero-init must be the 'no clip' sentinel");

    let none = ClipRect::NONE;
    assert_eq!(none.xywh, [0.0; 4]);
    assert!(none.is_none());
    assert_eq!(none, ClipRect::none());
}

#[test]
fn clip_rect_new_constructs_with_explicit_dims() {
    let clip = ClipRect::new(10.0, 20.0, 400.0, 300.0);
    assert_eq!(clip.xywh, [10.0, 20.0, 400.0, 300.0]);
    assert!(!clip.is_none(), "non-zero w/h must NOT be the sentinel");
}

#[test]
fn clip_rect_zero_width_or_height_is_none() {
    // Sentinel detection: w == 0 OR h == 0 must read as "no clip"
    // — this is the contract the fragment shader relies on.
    assert!(ClipRect::new(10.0, 20.0, 0.0, 100.0).is_none(),
        "zero width is the sentinel");
    assert!(ClipRect::new(10.0, 20.0, 100.0, 0.0).is_none(),
        "zero height is the sentinel");
    assert!(ClipRect::new(10.0, 20.0, -1.0, 100.0).is_none(),
        "negative width is the sentinel");
    assert!(ClipRect::new(10.0, 20.0, 100.0, -1.0).is_none(),
        "negative height is the sentinel");
}

#[test]
fn clip_rect_pod_size_is_16_bytes() {
    // Four f32s, no padding. This MUST match the shader's `vec4<f32>`
    // attribute stride or the GPU will read garbage.
    assert_eq!(std::mem::size_of::<ClipRect>(), 16);
    assert_eq!(std::mem::align_of::<ClipRect>(), 4);
}

#[test]
fn clip_rect_round_trips_through_bytemuck() {
    let original = ClipRect::new(100.0, 50.0, 400.0, 300.0);
    let bytes = bytemuck::bytes_of(&original);
    assert_eq!(bytes.len(), 16);

    // Re-cast back and confirm bit-exact equality.
    let recovered: &ClipRect = bytemuck::from_bytes(bytes);
    assert_eq!(recovered.xywh, original.xywh);
    assert_eq!(*recovered, original);
}

#[test]
fn clip_rect_slice_round_trips_through_bytemuck() {
    // The renderer uploads a contiguous slice of clip rects via
    // `bytemuck::cast_slice`; this exercises that exact path.
    let clips = vec![
        ClipRect::new(0.0, 0.0, 100.0, 100.0),
        ClipRect::NONE,
        ClipRect::new(200.0, 200.0, 50.0, 50.0),
    ];
    let bytes = bytemuck::cast_slice::<ClipRect, u8>(&clips);
    assert_eq!(bytes.len(), 3 * 16);

    let recovered: &[ClipRect] = bytemuck::cast_slice(bytes);
    assert_eq!(recovered.len(), 3);
    assert_eq!(recovered[0].xywh, [0.0, 0.0, 100.0, 100.0]);
    assert!(recovered[1].is_none());
    assert_eq!(recovered[2].xywh, [200.0, 200.0, 50.0, 50.0]);
}

// ---------------------------------------------------------------------------
// QuadInstance + ClipRect: parallel-buffer alignment invariants
// ---------------------------------------------------------------------------

#[test]
fn quad_and_clip_buffer_lengths_stay_in_lockstep() {
    // The parallel-buffer design relies on `quads.len() == clips.len()`.
    // The renderer enforces this with an `assert_eq!` in
    // `prepare_with_clips`; this test documents the CPU-side contract.
    let quads = vec![
        QuadInstance { pos: [0.0, 0.0], size: [10.0, 10.0],
                       color: [1.0; 4], border_radius: 0.0 },
        QuadInstance { pos: [20.0, 0.0], size: [10.0, 10.0],
                       color: [1.0; 4], border_radius: 0.0 },
    ];
    let clips = vec![ClipRect::NONE, ClipRect::new(0.0, 0.0, 100.0, 100.0)];
    assert_eq!(quads.len(), clips.len(),
        "parallel buffers must be the same length");

    // Both upload paths use bytemuck::cast_slice — confirm bytes align.
    let quad_bytes = bytemuck::cast_slice::<QuadInstance, u8>(&quads);
    let clip_bytes = bytemuck::cast_slice::<ClipRect, u8>(&clips);
    assert_eq!(quad_bytes.len(), 2 * std::mem::size_of::<QuadInstance>());
    assert_eq!(clip_bytes.len(), 2 * std::mem::size_of::<ClipRect>());
}

#[test]
fn quad_instance_size_is_unchanged_from_baseline() {
    // Defense in depth: if anyone adds a field to QuadInstance, this test
    // fires before the shader's vertex-attribute offsets are out of sync.
    // 2*f32 (pos) + 2*f32 (size) + 4*f32 (color) + 1*f32 (border) = 36 bytes.
    assert_eq!(std::mem::size_of::<QuadInstance>(), 36);
}

// ---------------------------------------------------------------------------
// GlyphClipRect: parallels the ClipRect contract for text rendering
// ---------------------------------------------------------------------------

#[test]
fn glyph_clip_rect_default_is_no_clip_sentinel() {
    let zero = GlyphClipRect::default();
    assert_eq!(zero.xywh, [0.0; 4]);
    assert!(zero.is_none());

    assert_eq!(GlyphClipRect::NONE, GlyphClipRect::none());
    assert!(GlyphClipRect::NONE.is_none());
}

#[test]
fn glyph_clip_rect_new_constructs_with_explicit_dims() {
    let clip = GlyphClipRect::new(50.0, 100.0, 800.0, 600.0);
    assert_eq!(clip.xywh, [50.0, 100.0, 800.0, 600.0]);
    assert!(!clip.is_none());
}

#[test]
fn glyph_clip_rect_zero_dims_are_sentinel() {
    assert!(GlyphClipRect::new(0.0, 0.0, 0.0, 100.0).is_none());
    assert!(GlyphClipRect::new(0.0, 0.0, 100.0, 0.0).is_none());
}

#[test]
fn glyph_clip_rect_round_trips_through_bytemuck() {
    let original = GlyphClipRect::new(10.0, 20.0, 300.0, 400.0);
    let bytes = bytemuck::bytes_of(&original);
    assert_eq!(bytes.len(), 16);

    let recovered: &GlyphClipRect = bytemuck::from_bytes(bytes);
    assert_eq!(*recovered, original);
}

#[test]
fn glyph_clip_rect_size_matches_quad_clip_rect() {
    // Both clip primitives use the same vec4<f32> shader layout.
    // If their sizes diverge, one of them has acquired hidden padding
    // and parallel uploads will skew.
    assert_eq!(
        std::mem::size_of::<GlyphClipRect>(),
        std::mem::size_of::<ClipRect>(),
    );
    assert_eq!(std::mem::size_of::<GlyphClipRect>(), 16);
}

#[test]
fn glyph_clip_rect_buffer_layout_matches_size() {
    let layout = GlyphClipRect::buffer_layout();
    assert_eq!(layout.array_stride as usize, std::mem::size_of::<GlyphClipRect>());
    assert_eq!(layout.step_mode, wgpu::VertexStepMode::Instance);
    assert_eq!(layout.attributes.len(), 1);
    // Shader location 4 is reserved across both quad and glyph pipelines
    // for the clip-rect attribute (see WGSL `@location(4) clip`).
    assert_eq!(layout.attributes[0].shader_location, 4);
    assert_eq!(layout.attributes[0].format, wgpu::VertexFormat::Float32x4);
}

// ---------------------------------------------------------------------------
// Manual visual validation procedure (defense-in-depth confirmation)
// ---------------------------------------------------------------------------

/// Manual procedure to confirm GPU-level clipping actually clips drawing.
///
/// **Why `#[ignore]`?** This is a procedure, not an automated test. It
/// requires running the full app and visually verifying nothing draws
/// outside the clip rect. Running `cargo test --ignored` exercises the
/// no-op body so the procedure stays discoverable in test output.
///
/// **Procedure**:
///
/// 1. In `crates/phantom-app/src/render.rs`, find a `quad_renderer.prepare(
///    device, queue, &quads, screen_size)` call for a single pane.
/// 2. Replace it with `quad_renderer.prepare_with_clips(...)`. Build a
///    `clips: Vec<ClipRect>` of the same length as `quads`, each entry
///    set to `ClipRect::new(pane_x, pane_y, pane_w, pane_h)`.
/// 3. **Deliberately make the clip rect smaller than the pane** — e.g.,
///    inset 50 pixels on every side. Run the app.
/// 4. Confirm: the pane content (background, cursor, glyphs) is visibly
///    cut off at the inset boundary, with hard edges at the clip boundary.
///    Nothing should draw outside the clip rect — not even debug overlays
///    that share the same `QuadRenderer`.
/// 5. Set `clip = ClipRect::NONE` (or `clip.xywh = [0,0,0,0]`) and re-run.
///    Confirm: rendering returns to normal — clipping is fully disabled.
///
/// **Expected failure modes if the wiring breaks**:
///   * Garbage clip values → black screen or nothing renders. Likely a
///     bytemuck stride mismatch or wrong shader_location.
///   * Clip applied even with NONE sentinel → the shader's
///     `clip_rect.z > 0.0 && clip_rect.w > 0.0` test was changed; the
///     sentinel definition in `ClipRect::is_none` and the shader test
///     must stay aligned.
///   * Clip clips the wrong region → coordinate-system drift. The clip
///     rect is in PIXEL coordinates with TOP-LEFT origin, matching
///     `QuadInstance::pos`. NDC-space clip rects will not work.
#[test]
#[ignore = "manual visual validation; see doc comment for procedure"]
fn manual_visual_validation_procedure() {
    // No-op body. Read the doc comment above and follow the procedure
    // when verifying clipping end-to-end.
}
