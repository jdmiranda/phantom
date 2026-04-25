//! Terminal grid state -> GPU cell buffer.
//!
//! Bridges the `alacritty_terminal` grid to the renderer by extracting the
//! visible region into a flat `Vec<RenderCell>` with RGBA float colors.

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::{Color, CursorShape as AlacCursorShape, NamedColor};
use alacritty_terminal::Term;

// ---------------------------------------------------------------------------
// CellFlags — renderer-facing bitflags (decoupled from alacritty)
// ---------------------------------------------------------------------------

bitflags::bitflags! {
    /// Cell attribute flags consumed by the GPU renderer.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct CellFlags: u16 {
        const BOLD          = 1 << 0;
        const DIM           = 1 << 1;
        const ITALIC        = 1 << 2;
        const UNDERLINE     = 1 << 3;
        const BLINK         = 1 << 4;
        const INVERSE       = 1 << 5;
        const HIDDEN        = 1 << 6;
        const STRIKETHROUGH = 1 << 7;
        const WIDE_CHAR     = 1 << 8;
    }
}

impl CellFlags {
    /// Translate alacritty terminal `Flags` into renderer `CellFlags`.
    fn from_alac(f: Flags) -> Self {
        let mut out = Self::empty();
        if f.contains(Flags::BOLD) {
            out |= Self::BOLD;
        }
        if f.contains(Flags::DIM) {
            out |= Self::DIM;
        }
        if f.contains(Flags::ITALIC) {
            out |= Self::ITALIC;
        }
        if f.intersects(Flags::UNDERLINE | Flags::DOUBLE_UNDERLINE | Flags::UNDERCURL | Flags::DOTTED_UNDERLINE | Flags::DASHED_UNDERLINE) {
            out |= Self::UNDERLINE;
        }
        if f.contains(Flags::INVERSE) {
            out |= Self::INVERSE;
        }
        if f.contains(Flags::HIDDEN) {
            out |= Self::HIDDEN;
        }
        if f.contains(Flags::STRIKEOUT) {
            out |= Self::STRIKETHROUGH;
        }
        if f.contains(Flags::WIDE_CHAR) {
            out |= Self::WIDE_CHAR;
        }
        out
    }
}

// ---------------------------------------------------------------------------
// RenderCell
// ---------------------------------------------------------------------------

/// A single cell ready for GPU consumption.
#[derive(Debug, Clone, Copy)]
pub struct RenderCell {
    /// The character to render (' ' for empty / spacer cells).
    pub ch: char,
    /// Foreground color as linear RGBA.
    pub fg: [f32; 4],
    /// Background color as linear RGBA.
    pub bg: [f32; 4],
    /// Attribute flags (bold, italic, underline, etc.).
    pub flags: CellFlags,
}

// ---------------------------------------------------------------------------
// CursorState
// ---------------------------------------------------------------------------

/// Cursor shape as seen by the renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorShape {
    Block,
    Underline,
    Bar,
}

/// Cursor rendering state extracted from the terminal.
#[derive(Debug, Clone, Copy)]
pub struct CursorState {
    pub row: usize,
    pub col: usize,
    pub visible: bool,
    pub shape: CursorShape,
}

// ---------------------------------------------------------------------------
// Theme-aware color mapping
// ---------------------------------------------------------------------------

/// Theme color overrides passed into grid extraction.
///
/// This decouples `phantom-terminal` from the theme system — the caller
/// (phantom-app) populates this from `Theme::colors` and passes it in.
#[derive(Debug, Clone)]
pub struct TerminalThemeColors {
    /// Default foreground (used for `NamedColor::Foreground`).
    pub foreground: [f32; 4],
    /// Default background (used for `NamedColor::Background`).
    pub background: [f32; 4],
    /// Cursor color.
    pub cursor: [f32; 4],
    /// Override for the first 16 ANSI palette entries.
    /// If `Some`, these replace the xterm defaults for indices 0..16.
    pub ansi: Option<[[f32; 4]; 16]>,
}

impl Default for TerminalThemeColors {
    fn default() -> Self {
        Self {
            foreground: [1.0, 1.0, 1.0, 1.0],
            background: [0.0, 0.0, 0.0, 0.0],
            cursor: [1.0, 1.0, 1.0, 1.0],
            ansi: None,
        }
    }
}

/// Convert an alacritty `Color` to an RGBA float quad.
///
/// Theme colors are used for `Foreground`, `Background`, and `Cursor`
/// semantics. ANSI colors use the theme override palette when available.
fn color_to_rgba(color: Color, table: &[[f32; 4]; 256], theme: &TerminalThemeColors, is_fg: bool) -> [f32; 4] {
    match color {
        Color::Spec(rgb) => [
            rgb.r as f32 / 255.0,
            rgb.g as f32 / 255.0,
            rgb.b as f32 / 255.0,
            1.0,
        ],
        Color::Indexed(idx) => table[idx as usize],
        Color::Named(named) => named_color_to_rgba(named, table, theme, is_fg),
    }
}

/// Resolve a `NamedColor` to RGBA using theme colors.
fn named_color_to_rgba(named: NamedColor, table: &[[f32; 4]; 256], theme: &TerminalThemeColors, is_fg: bool) -> [f32; 4] {
    match named {
        // Standard 16 ANSI colors map directly to indices 0..16.
        NamedColor::Black => table[0],
        NamedColor::Red => table[1],
        NamedColor::Green => table[2],
        NamedColor::Yellow => table[3],
        NamedColor::Blue => table[4],
        NamedColor::Magenta => table[5],
        NamedColor::Cyan => table[6],
        NamedColor::White => table[7],
        NamedColor::BrightBlack => table[8],
        NamedColor::BrightRed => table[9],
        NamedColor::BrightGreen => table[10],
        NamedColor::BrightYellow => table[11],
        NamedColor::BrightBlue => table[12],
        NamedColor::BrightMagenta => table[13],
        NamedColor::BrightCyan => table[14],
        NamedColor::BrightWhite => table[15],

        // Dim variants — use the base color at ~67% brightness.
        NamedColor::DimBlack => dim(table[0]),
        NamedColor::DimRed => dim(table[1]),
        NamedColor::DimGreen => dim(table[2]),
        NamedColor::DimYellow => dim(table[3]),
        NamedColor::DimBlue => dim(table[4]),
        NamedColor::DimMagenta => dim(table[5]),
        NamedColor::DimCyan => dim(table[6]),
        NamedColor::DimWhite => dim(table[7]),

        // Semantic names — resolved from the THEME, not hardcoded.
        NamedColor::Foreground | NamedColor::BrightForeground => theme.foreground,
        NamedColor::DimForeground => dim(theme.foreground),
        NamedColor::Background => theme.background,
        NamedColor::Cursor => {
            if is_fg { theme.background } else { theme.cursor }
        }
    }
}

/// Dim a color by scaling RGB channels to ~67%.
#[inline]
const fn dim(c: [f32; 4]) -> [f32; 4] {
    [c[0] * 0.67, c[1] * 0.67, c[2] * 0.67, c[3]]
}

// ---------------------------------------------------------------------------
// Grid extraction
// ---------------------------------------------------------------------------

/// Extract the visible terminal grid into a flat cell buffer.
///
/// Returns `(cells, cols, rows, cursor_state)` where `cells` is row-major with
/// `cols * rows` entries.
///
/// When `theme` is `None`, falls back to xterm defaults (white on black).
pub fn extract_grid<T: EventListener>(
    term: &Term<T>,
) -> (Vec<RenderCell>, usize, usize, CursorState) {
    extract_grid_themed(term, &TerminalThemeColors::default())
}

/// Extract the visible terminal grid with theme-aware colors.
///
/// The `theme` parameter provides foreground, background, cursor, and
/// ANSI palette overrides. This is the primary path used by the GUI app.
pub fn extract_grid_themed<T: EventListener>(
    term: &Term<T>,
    theme: &TerminalThemeColors,
) -> (Vec<RenderCell>, usize, usize, CursorState) {
    let grid = term.grid();
    let cols = grid.columns();
    let rows = grid.screen_lines();
    let mut table = ansi_color_table();

    // Override ANSI 0..16 with theme palette if provided.
    if let Some(ref ansi) = theme.ansi {
        for (i, color) in ansi.iter().enumerate() {
            table[i] = *color;
        }
    }

    // Get selection range for highlight rendering.
    let selection_range = term.selection.as_ref().and_then(|s| s.to_range(term));

    let mut cells = Vec::with_capacity(cols * rows);

    for row_idx in 0..rows {
        let line = &grid[Line(row_idx as i32)];
        for col_idx in 0..cols {
            let cell = &line[Column(col_idx)];

            // Check if this cell is in the selection.
            let is_selected = selection_range
                .as_ref()
                .map_or(false, |range| {
                    range.contains(Point::new(Line(row_idx as i32), Column(col_idx)))
                });

            // Skip wide-char spacer cells — emit a space placeholder instead.
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER)
                || cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER)
            {
                let mut fg = theme.foreground;
                let mut bg = color_to_rgba(cell.bg, &table, theme, false);
                if is_selected {
                    std::mem::swap(&mut fg, &mut bg);
                    if bg[3] == 0.0 { bg[3] = 1.0; }
                }
                cells.push(RenderCell {
                    ch: ' ',
                    fg,
                    bg,
                    flags: CellFlags::empty(),
                });
                continue;
            }

            let mut fg = color_to_rgba(cell.fg, &table, theme, true);
            let mut bg = color_to_rgba(cell.bg, &table, theme, false);

            let alac_flags = cell.flags;
            let render_flags = CellFlags::from_alac(alac_flags);

            // Handle INVERSE: swap foreground and background.
            if alac_flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
                if bg[3] == 0.0 {
                    bg[3] = 1.0;
                }
            }

            // DIM: reduce foreground intensity.
            if alac_flags.contains(Flags::DIM) && !alac_flags.contains(Flags::INVERSE) {
                fg[0] *= 0.67;
                fg[1] *= 0.67;
                fg[2] *= 0.67;
            }

            // HIDDEN: make fg match bg.
            if alac_flags.contains(Flags::HIDDEN) {
                fg = bg;
            }

            // Selection highlight: swap fg/bg for selected cells.
            if is_selected {
                std::mem::swap(&mut fg, &mut bg);
                if bg[3] == 0.0 { bg[3] = 1.0; }
            }

            cells.push(RenderCell {
                ch: cell.c,
                fg,
                bg,
                flags: render_flags,
            });
        }
    }

    // Cursor state.
    let cursor = extract_cursor(term);

    (cells, cols, rows, cursor)
}

/// Extract cursor position and shape from the terminal.
fn extract_cursor<T: EventListener>(term: &Term<T>) -> CursorState {
    let content = term.renderable_content();
    let rc = content.cursor;

    let visible = rc.shape != AlacCursorShape::Hidden;
    let shape = match rc.shape {
        AlacCursorShape::Block | AlacCursorShape::HollowBlock => CursorShape::Block,
        AlacCursorShape::Underline => CursorShape::Underline,
        AlacCursorShape::Beam => CursorShape::Bar,
        AlacCursorShape::Hidden => CursorShape::Block, // shape is irrelevant when hidden
    };

    // The renderable cursor point uses Line(i32). Convert to usize row within
    // the visible viewport.
    let row = rc.point.line.0.max(0) as usize;
    let col = rc.point.column.0;

    CursorState {
        row,
        col,
        visible,
        shape,
    }
}

// ---------------------------------------------------------------------------
// 256-color xterm palette
// ---------------------------------------------------------------------------

/// Build the full xterm 256-color palette as `[f32; 4]` RGBA (all opaque).
///
/// - Indices 0..16: standard ANSI colors.
/// - Indices 16..232: 6x6x6 color cube.
/// - Indices 232..256: grayscale ramp.
pub fn ansi_color_table() -> [[f32; 4]; 256] {
    let mut t = [[0.0f32; 4]; 256];

    // -----------------------------------------------------------------------
    // 0..16 — Standard ANSI colors (matches xterm defaults)
    // -----------------------------------------------------------------------
    static ANSI16: [[u8; 3]; 16] = [
        [0x00, 0x00, 0x00], // 0  Black
        [0xCD, 0x00, 0x00], // 1  Red
        [0x00, 0xCD, 0x00], // 2  Green
        [0xCD, 0xCD, 0x00], // 3  Yellow
        [0x00, 0x00, 0xEE], // 4  Blue
        [0xCD, 0x00, 0xCD], // 5  Magenta
        [0x00, 0xCD, 0xCD], // 6  Cyan
        [0xE5, 0xE5, 0xE5], // 7  White
        [0x7F, 0x7F, 0x7F], // 8  Bright Black (Gray)
        [0xFF, 0x00, 0x00], // 9  Bright Red
        [0x00, 0xFF, 0x00], // 10 Bright Green
        [0xFF, 0xFF, 0x00], // 11 Bright Yellow
        [0x5C, 0x5C, 0xFF], // 12 Bright Blue
        [0xFF, 0x00, 0xFF], // 13 Bright Magenta
        [0x00, 0xFF, 0xFF], // 14 Bright Cyan
        [0xFF, 0xFF, 0xFF], // 15 Bright White
    ];

    for (i, rgb) in ANSI16.iter().enumerate() {
        t[i] = [
            rgb[0] as f32 / 255.0,
            rgb[1] as f32 / 255.0,
            rgb[2] as f32 / 255.0,
            1.0,
        ];
    }

    // -----------------------------------------------------------------------
    // 16..232 — 6x6x6 color cube
    // -----------------------------------------------------------------------
    // Each axis value in {0, 1, 2, 3, 4, 5} maps to {0x00, 0x5F, 0x87, 0xAF, 0xD7, 0xFF}.
    static CUBE_STEPS: [u8; 6] = [0x00, 0x5F, 0x87, 0xAF, 0xD7, 0xFF];

    for r in 0..6u8 {
        for g in 0..6u8 {
            for b in 0..6u8 {
                let idx = 16 + (r as usize) * 36 + (g as usize) * 6 + (b as usize);
                t[idx] = [
                    CUBE_STEPS[r as usize] as f32 / 255.0,
                    CUBE_STEPS[g as usize] as f32 / 255.0,
                    CUBE_STEPS[b as usize] as f32 / 255.0,
                    1.0,
                ];
            }
        }
    }

    // -----------------------------------------------------------------------
    // 232..256 — Grayscale ramp
    // -----------------------------------------------------------------------
    // 24 shades from 0x08 to 0xEE in steps of 10.
    for i in 0..24u8 {
        let v = (8 + 10 * i) as f32 / 255.0;
        t[232 + i as usize] = [v, v, v, 1.0];
    }

    t
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_table_has_correct_bounds() {
        let t = ansi_color_table();

        // All values should be in [0.0, 1.0].
        for (i, rgba) in t.iter().enumerate() {
            for (ch, &v) in ["r", "g", "b", "a"].iter().zip(rgba) {
                assert!(
                    (0.0..=1.0).contains(&v),
                    "index {i} channel {ch} out of range: {v}"
                );
            }
        }

        // Index 0 (black) should be [0, 0, 0, 1].
        assert_eq!(t[0], [0.0, 0.0, 0.0, 1.0]);

        // Index 15 (bright white) should be [1, 1, 1, 1].
        assert_eq!(t[15], [1.0, 1.0, 1.0, 1.0]);

        // Index 16 should be [0, 0, 0, 1] (cube origin).
        assert_eq!(t[16], [0.0, 0.0, 0.0, 1.0]);

        // Index 231 should be [1, 1, 1, 1] (cube corner).
        assert_eq!(t[231], [1.0, 1.0, 1.0, 1.0]);

        // Index 232 (first grayscale) = 8/255.
        let expected = 8.0 / 255.0;
        assert!((t[232][0] - expected).abs() < 1e-6);

        // Index 255 (last grayscale) = 238/255.
        let expected = 238.0 / 255.0;
        assert!((t[255][0] - expected).abs() < 1e-6);
    }

    #[test]
    fn cell_flags_from_alac_round_trips() {
        let alac = Flags::BOLD | Flags::ITALIC | Flags::UNDERLINE;
        let render = CellFlags::from_alac(alac);
        assert!(render.contains(CellFlags::BOLD));
        assert!(render.contains(CellFlags::ITALIC));
        assert!(render.contains(CellFlags::UNDERLINE));
        assert!(!render.contains(CellFlags::DIM));
        assert!(!render.contains(CellFlags::STRIKETHROUGH));
    }

    #[test]
    fn cell_flags_wide_char() {
        let alac = Flags::WIDE_CHAR;
        let render = CellFlags::from_alac(alac);
        assert!(render.contains(CellFlags::WIDE_CHAR));
    }

    #[test]
    fn cell_flags_all_underline_variants() {
        for flag in [
            Flags::UNDERLINE,
            Flags::DOUBLE_UNDERLINE,
            Flags::UNDERCURL,
            Flags::DOTTED_UNDERLINE,
            Flags::DASHED_UNDERLINE,
        ] {
            let render = CellFlags::from_alac(flag);
            assert!(
                render.contains(CellFlags::UNDERLINE),
                "alacritty flag {flag:?} should map to CellFlags::UNDERLINE"
            );
        }
    }

    fn default_theme() -> TerminalThemeColors {
        TerminalThemeColors::default()
    }

    /// Phosphor theme colors (green on dark) for regression testing.
    fn phosphor_theme() -> TerminalThemeColors {
        let green: [f32; 4] = [0.2, 1.0, 0.0, 1.0];
        let bg: [f32; 4] = [0.04, 0.055, 0.078, 1.0];
        let cursor: [f32; 4] = [0.2, 1.0, 0.0, 1.0];
        let mut ansi = [[0.0f32; 4]; 16];
        ansi[0] = bg;
        ansi[2] = green;
        ansi[7] = [0.69, 0.8, 0.63, 1.0];
        TerminalThemeColors {
            foreground: green,
            background: bg,
            cursor,
            ansi: Some(ansi),
        }
    }

    #[test]
    fn color_spec_conversion() {
        let table = ansi_color_table();
        let theme = default_theme();
        let rgb = alacritty_terminal::vte::ansi::Rgb {
            r: 128,
            g: 64,
            b: 255,
        };
        let rgba = color_to_rgba(Color::Spec(rgb), &table, &theme, true);
        assert!((rgba[0] - 128.0 / 255.0).abs() < 1e-6);
        assert!((rgba[1] - 64.0 / 255.0).abs() < 1e-6);
        assert!((rgba[2] - 255.0 / 255.0).abs() < 1e-6);
        assert_eq!(rgba[3], 1.0);
    }

    #[test]
    fn color_indexed_conversion() {
        let table = ansi_color_table();
        let theme = default_theme();
        // Index 1 should be red.
        let rgba = color_to_rgba(Color::Indexed(1), &table, &theme, true);
        assert_eq!(rgba, table[1]);
    }

    #[test]
    fn color_named_foreground_uses_default_when_no_theme() {
        let table = ansi_color_table();
        let theme = default_theme();
        let fg = color_to_rgba(Color::Named(NamedColor::Foreground), &table, &theme, true);
        assert_eq!(fg, [1.0, 1.0, 1.0, 1.0]);
    }

    #[test]
    fn cube_color_index_21() {
        // Index 21 = 16 + 0*36 + 0*6 + 5 = pure blue in cube.
        let t = ansi_color_table();
        assert_eq!(t[21], [0.0, 0.0, 1.0, 1.0]);
    }

    // ===================================================================
    // REGRESSION: theme color mapping (the bug that made Phantom gray)
    // ===================================================================

    #[test]
    fn foreground_uses_theme_color_not_white() {
        let table = ansi_color_table();
        let theme = phosphor_theme();
        let fg = color_to_rgba(Color::Named(NamedColor::Foreground), &table, &theme, true);
        // Must be phosphor green, NOT hardcoded white.
        assert_eq!(fg, theme.foreground, "NamedColor::Foreground must use theme foreground");
        assert_ne!(fg, [1.0, 1.0, 1.0, 1.0], "must NOT be hardcoded white");
    }

    #[test]
    fn background_uses_theme_color_not_transparent() {
        let table = ansi_color_table();
        let theme = phosphor_theme();
        let bg = color_to_rgba(Color::Named(NamedColor::Background), &table, &theme, false);
        // Must be theme background, NOT transparent black.
        assert_eq!(bg, theme.background, "NamedColor::Background must use theme background");
        assert_ne!(bg, [0.0, 0.0, 0.0, 0.0], "must NOT be transparent black");
    }

    #[test]
    fn cursor_uses_theme_color() {
        let table = ansi_color_table();
        let theme = phosphor_theme();
        let cursor_bg = color_to_rgba(Color::Named(NamedColor::Cursor), &table, &theme, false);
        assert_eq!(cursor_bg, theme.cursor, "NamedColor::Cursor (bg) must use theme cursor");
    }

    #[test]
    fn bright_foreground_uses_theme_color() {
        let table = ansi_color_table();
        let theme = phosphor_theme();
        let fg = color_to_rgba(Color::Named(NamedColor::BrightForeground), &table, &theme, true);
        assert_eq!(fg, theme.foreground, "BrightForeground must use theme foreground");
    }

    #[test]
    fn dim_foreground_uses_dimmed_theme_color() {
        let table = ansi_color_table();
        let theme = phosphor_theme();
        let fg = color_to_rgba(Color::Named(NamedColor::DimForeground), &table, &theme, true);
        let expected = dim(theme.foreground);
        assert_eq!(fg, expected, "DimForeground must be 67% of theme foreground");
    }

    #[test]
    fn ansi_palette_override_applied() {
        let mut table = ansi_color_table();
        let theme = phosphor_theme();
        // Apply override to table like extract_grid_themed does.
        if let Some(ref ansi) = theme.ansi {
            for (i, color) in ansi.iter().enumerate() {
                table[i] = *color;
            }
        }
        // NamedColor::Green (index 2) must use theme's green, not xterm green.
        let green = named_color_to_rgba(NamedColor::Green, &table, &theme, true);
        assert_eq!(green, theme.ansi.unwrap()[2], "ANSI green must use theme palette");
        assert_ne!(green, [0.0, 0xCD as f32 / 255.0, 0.0, 1.0], "must NOT be xterm default green");
    }

    #[test]
    fn ansi_palette_override_only_affects_0_to_15() {
        let mut table = ansi_color_table();
        let original_16 = table[16]; // first cube color
        let theme = phosphor_theme();
        if let Some(ref ansi) = theme.ansi {
            for (i, color) in ansi.iter().enumerate() {
                table[i] = *color;
            }
        }
        // Index 16+ must be unchanged.
        assert_eq!(table[16], original_16, "color cube (16+) must not be affected by ANSI override");
    }

    #[test]
    fn no_ansi_override_uses_xterm_defaults() {
        let table = ansi_color_table();
        let theme = TerminalThemeColors {
            foreground: [0.5, 0.5, 0.5, 1.0],
            background: [0.1, 0.1, 0.1, 1.0],
            cursor: [0.5, 0.5, 0.5, 1.0],
            ansi: None, // no override
        };
        // NamedColor::Red (index 1) should use xterm default.
        let red = named_color_to_rgba(NamedColor::Red, &table, &theme, true);
        assert_eq!(red, table[1], "without ANSI override, should use xterm defaults");
    }

    #[test]
    fn spec_color_unaffected_by_theme() {
        let table = ansi_color_table();
        let theme = phosphor_theme();
        let rgb = alacritty_terminal::vte::ansi::Rgb { r: 0xAA, g: 0xBB, b: 0xCC };
        let rgba = color_to_rgba(Color::Spec(rgb), &table, &theme, true);
        // Exact RGB values must be preserved regardless of theme.
        assert!((rgba[0] - 0xAA as f32 / 255.0).abs() < 1e-6);
        assert!((rgba[1] - 0xBB as f32 / 255.0).abs() < 1e-6);
        assert!((rgba[2] - 0xCC as f32 / 255.0).abs() < 1e-6);
    }
}
