pub mod arbiter;
pub mod cursor;
pub mod keybinds;
pub mod layout;
pub mod render_ctx;
pub mod selection;
pub mod themes;
pub mod tokens;
pub mod widgets;

// Re-export the `RenderCtx` primitive at the crate root so existing
// `crate::RenderCtx` references in `tokens.rs` (and any future widget
// modules that take a render context) keep working without an extra
// `use` line. This matches how `Rect` is implicitly available via
// `crate::layout::Rect`.
pub use render_ctx::RenderCtx;

// Re-export the cursor blink primitive at the crate root for ergonomic access:
// `phantom_ui::CursorBlink` alongside `phantom_ui::RenderCtx`.
pub use cursor::{CursorBlink, DEFAULT_PERIOD_MS};
