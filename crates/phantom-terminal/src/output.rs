//! Terminal grid state -> GPU cell buffer.
//!
//! Bridges the `alacritty_terminal` grid to the renderer by extracting the
//! visible region into a flat `Vec<RenderCell>` with RGBA float colors.

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
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
// Color conversion
// ---------------------------------------------------------------------------

/// Default foreground: opaque white.
const DEFAULT_FG: [f32; 4] = [1.0, 1.0, 1.0, 1.0];
/// Default background: fully transparent black.
const DEFAULT_BG: [f32; 4] = [0.0, 0.0, 0.0, 0.0];

/// Convert an alacritty `Color` to an RGBA float quad.
///
/// Named special colors (`Foreground`, `Background`, `Cursor`, and the `Dim*`
/// / `Bright*` variants that don't map to a standard index) fall back to the
/// default fg/bg since we don't have access to the user's color scheme here.
fn color_to_rgba(color: Color, table: &[[f32; 4]; 256], is_fg: bool) -> [f32; 4] {
    match color {
        Color::Spec(rgb) => [
            rgb.r as f32 / 255.0,
            rgb.g as f32 / 255.0,
            rgb.b as f32 / 255.0,
            1.0,
        ],
        Color::Indexed(idx) => table[idx as usize],
        Color::Named(named) => named_color_to_rgba(named, table, is_fg),
    }
}

/// Resolve a `NamedColor` to RGBA.
fn named_color_to_rgba(named: NamedColor, table: &[[f32; 4]; 256], is_fg: bool) -> [f32; 4] {
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

        // Semantic names — fall back to defaults.
        NamedColor::Foreground | NamedColor::BrightForeground => DEFAULT_FG,
        NamedColor::DimForeground => dim(DEFAULT_FG),
        NamedColor::Background => DEFAULT_BG,
        NamedColor::Cursor => {
            if is_fg { DEFAULT_BG } else { DEFAULT_FG }
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
pub fn extract_grid<T: EventListener>(
    term: &Term<T>,
) -> (Vec<RenderCell>, usize, usize, CursorState) {
    let grid = term.grid();
    let cols = grid.columns();
    let rows = grid.screen_lines();
    let table = ansi_color_table();

    let mut cells = Vec::with_capacity(cols * rows);

    for row_idx in 0..rows {
        let line = &grid[Line(row_idx as i32)];
        for col_idx in 0..cols {
            let cell = &line[Column(col_idx)];

            // Skip wide-char spacer cells — emit a space placeholder instead.
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER)
                || cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER)
            {
                cells.push(RenderCell {
                    ch: ' ',
                    fg: DEFAULT_FG,
                    bg: color_to_rgba(cell.bg, &table, false),
                    flags: CellFlags::empty(),
                });
                continue;
            }

            let mut fg = color_to_rgba(cell.fg, &table, true);
            let mut bg = color_to_rgba(cell.bg, &table, false);

            let alac_flags = cell.flags;
            let render_flags = CellFlags::from_alac(alac_flags);

            // Handle INVERSE: swap foreground and background.
            if alac_flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
                // Ensure the swapped background is opaque if it came from fg.
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

    #[test]
    fn color_spec_conversion() {
        let table = ansi_color_table();
        let rgb = alacritty_terminal::vte::ansi::Rgb {
            r: 128,
            g: 64,
            b: 255,
        };
        let rgba = color_to_rgba(Color::Spec(rgb), &table, true);
        assert!((rgba[0] - 128.0 / 255.0).abs() < 1e-6);
        assert!((rgba[1] - 64.0 / 255.0).abs() < 1e-6);
        assert!((rgba[2] - 255.0 / 255.0).abs() < 1e-6);
        assert_eq!(rgba[3], 1.0);
    }

    #[test]
    fn color_indexed_conversion() {
        let table = ansi_color_table();
        // Index 1 should be red.
        let rgba = color_to_rgba(Color::Indexed(1), &table, true);
        assert_eq!(rgba, table[1]);
    }

    #[test]
    fn color_named_foreground_background() {
        let table = ansi_color_table();
        let fg = color_to_rgba(Color::Named(NamedColor::Foreground), &table, true);
        assert_eq!(fg, DEFAULT_FG);
        let bg = color_to_rgba(Color::Named(NamedColor::Background), &table, false);
        assert_eq!(bg, DEFAULT_BG);
    }

    #[test]
    fn cube_color_index_21() {
        // Index 21 = 16 + 0*36 + 0*6 + 5 = pure blue in cube.
        let t = ansi_color_table();
        assert_eq!(t[21], [0.0, 0.0, 1.0, 1.0]);
    }
}
