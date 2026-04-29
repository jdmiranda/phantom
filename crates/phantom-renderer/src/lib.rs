pub mod gpu;
pub mod atlas;
pub mod text;
pub mod grid;
pub mod quads;
pub mod postfx;
pub mod images;
pub mod screenshot;
pub mod debug_draw;
pub mod video;
pub mod shader_loader;

// === Phase 0.D clip-rect submodules (DO NOT DROP) ===
//
// These modules are deliberately isolated single-purpose files so that
// concurrent rewrites or auto-format passes against the larger
// `quads.rs` / `text.rs` cannot accidentally drop the `ClipRect` /
// `GlyphClipRect` types. They are kept private here and re-exported
// from `quads` / `text` to preserve the historical public paths
// `phantom_renderer::quads::ClipRect` and
// `phantom_renderer::text::GlyphClipRect`.
//
// If you remove either `mod clip;` or `mod glyph_clip;` below, the
// integration test at `crates/phantom-renderer/tests/clip_rect.rs`
// stops compiling. Don't.
mod clip;
mod glyph_clip;
