/// Theme system for Phantom terminal.
///
/// Defines color palettes, CRT shader parameters, and UI chrome colors.
/// Each built-in theme is a curated, production-ready visual identity.

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Convert a hex color `#RRGGBB` to `[f32; 4]` with alpha = 1.0.
const fn hex(r: u8, g: u8, b: u8) -> [f32; 4] {
    [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0]
}

/// Same as [`hex`] but with explicit alpha.
const fn hexa(r: u8, g: u8, b: u8, a: f32) -> [f32; 4] {
    [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, a]
}

/// Extract RGB channels as `[f32; 3]` from an `[f32; 4]` color.
const fn rgb3(c: [f32; 4]) -> [f32; 3] {
    [c[0], c[1], c[2]]
}

// ---------------------------------------------------------------------------
// Core types
// ---------------------------------------------------------------------------

/// The 16 ANSI colors plus semantic terminal colors.
#[derive(Debug, Clone, PartialEq)]
pub struct TerminalColors {
    /// Default text color.
    pub foreground: [f32; 4],
    /// Terminal background.
    pub background: [f32; 4],
    /// Cursor block/beam color.
    pub cursor: [f32; 4],
    /// Selection highlight (typically semi-transparent).
    pub selection: [f32; 4],
    /// Standard 16 ANSI palette.
    /// Indices 0-7 are normal, 8-15 are bright variants.
    pub ansi: [[f32; 4]; 16],
}

/// Parameters fed into the CRT post-processing shader.
///
/// All intensity values are clamped to `0.0..=1.0` by convention.
#[derive(Debug, Clone, PartialEq)]
pub struct ShaderParams {
    /// Horizontal scanline darkening.
    pub scanline_intensity: f32,
    /// Additive bloom/glow around bright pixels.
    pub bloom_intensity: f32,
    /// RGB channel separation at screen edges.
    pub chromatic_aberration: f32,
    /// Barrel distortion amount (CRT screen curvature).
    pub curvature: f32,
    /// Corner/edge darkening.
    pub vignette_intensity: f32,
    /// Animated noise grain.
    pub noise_intensity: f32,
    /// Tint color applied to the bloom pass (phosphor color).
    pub glow_color: [f32; 3],
}

/// Colors for the UI chrome surrounding the terminal viewport.
#[derive(Debug, Clone, PartialEq)]
pub struct UiColors {
    pub status_bar_bg: [f32; 4],
    pub status_bar_fg: [f32; 4],
    pub tab_bar_bg: [f32; 4],
    pub tab_bar_fg: [f32; 4],
    pub tab_active_bg: [f32; 4],
    pub border: [f32; 4],
}

/// A complete visual theme for Phantom.
#[derive(Debug, Clone, PartialEq)]
pub struct Theme {
    pub name: String,
    pub colors: TerminalColors,
    pub shader_params: ShaderParams,
    pub ui_colors: UiColors,
}

impl Default for Theme {
    fn default() -> Self {
        phosphor()
    }
}

// ---------------------------------------------------------------------------
// Built-in themes
// ---------------------------------------------------------------------------

/// **Phosphor** — Classic green CRT phosphor.
///
/// Deep void background, bright green text, heavy scanlines, warm bloom.
/// The definitive Phantom look.
pub fn phosphor() -> Theme {
    // Greens
    let green = hex(0x33, 0xFF, 0x00);
    let dim_green = hex(0x1A, 0x99, 0x00);
    let bg = hex(0x0A, 0x0E, 0x14);

    Theme {
        name: "Phosphor".into(),
        colors: TerminalColors {
            foreground: green,
            background: bg,
            cursor: hex(0x33, 0xFF, 0x00),
            selection: hexa(0x33, 0xFF, 0x00, 0.25),
            ansi: [
                // Normal 0-7
                hex(0x0A, 0x0E, 0x14), // 0  black
                hex(0xCC, 0x33, 0x33), // 1  red
                hex(0x33, 0xFF, 0x00), // 2  green (phosphor)
                hex(0xCC, 0xCC, 0x33), // 3  yellow
                hex(0x33, 0x99, 0xCC), // 4  blue
                hex(0x99, 0x33, 0xCC), // 5  magenta
                hex(0x33, 0xCC, 0x99), // 6  cyan
                hex(0xB0, 0xCC, 0xA0), // 7  white (green-tinted)
                // Bright 8-15
                hex(0x1A, 0x24, 0x30), // 8  bright black
                hex(0xFF, 0x55, 0x55), // 9  bright red
                hex(0x66, 0xFF, 0x44), // 10 bright green
                hex(0xFF, 0xFF, 0x66), // 11 bright yellow
                hex(0x55, 0xBB, 0xFF), // 12 bright blue
                hex(0xCC, 0x66, 0xFF), // 13 bright magenta
                hex(0x55, 0xFF, 0xCC), // 14 bright cyan
                hex(0xD0, 0xEE, 0xC0), // 15 bright white
            ],
        },
        shader_params: ShaderParams {
            scanline_intensity: 0.08,
            bloom_intensity: 0.10,
            chromatic_aberration: 0.01,
            curvature: 0.0,
            vignette_intensity: 0.08,
            noise_intensity: 0.01,
            glow_color: rgb3(dim_green),
        },
        ui_colors: UiColors {
            status_bar_bg: hex(0x06, 0x08, 0x0C),
            status_bar_fg: dim_green,
            tab_bar_bg: hex(0x06, 0x08, 0x0C),
            tab_bar_fg: hex(0x22, 0x99, 0x00),
            tab_active_bg: hex(0x12, 0x1A, 0x0E),
            border: hex(0x1A, 0x33, 0x00),
        },
    }
}

/// **Amber** — Warm amber CRT.
///
/// The look of a Zenith or Wyse terminal from the 1980s.
/// Amber (#FFB000) on near-black, classic and warm.
pub fn amber() -> Theme {
    let amber = hex(0xFF, 0xB0, 0x00);
    let dim_amber = hex(0xB3, 0x7A, 0x00);
    let bg = hex(0x0C, 0x09, 0x04);

    Theme {
        name: "Amber".into(),
        colors: TerminalColors {
            foreground: amber,
            background: bg,
            cursor: hex(0xFF, 0xC8, 0x44),
            selection: hexa(0xFF, 0xB0, 0x00, 0.25),
            ansi: [
                // Normal 0-7
                hex(0x0C, 0x09, 0x04), // 0  black
                hex(0xCC, 0x44, 0x22), // 1  red
                hex(0xCC, 0x99, 0x00), // 2  green (amber-shifted)
                hex(0xFF, 0xB0, 0x00), // 3  yellow (amber)
                hex(0x88, 0x77, 0x44), // 4  blue (muted gold)
                hex(0xCC, 0x66, 0x33), // 5  magenta (burnt orange)
                hex(0xDD, 0x99, 0x44), // 6  cyan (warm gold)
                hex(0xDD, 0xBB, 0x88), // 7  white (cream)
                // Bright 8-15
                hex(0x22, 0x1A, 0x0C), // 8  bright black
                hex(0xFF, 0x66, 0x44), // 9  bright red
                hex(0xFF, 0xBB, 0x33), // 10 bright green
                hex(0xFF, 0xDD, 0x55), // 11 bright yellow
                hex(0xBB, 0xAA, 0x66), // 12 bright blue
                hex(0xFF, 0x88, 0x55), // 13 bright magenta
                hex(0xFF, 0xCC, 0x66), // 14 bright cyan
                hex(0xFF, 0xDD, 0xAA), // 15 bright white
            ],
        },
        shader_params: ShaderParams {
            scanline_intensity: 0.20,
            bloom_intensity: 0.25,
            chromatic_aberration: 0.04,
            curvature: 0.07,
            vignette_intensity: 0.22,
            noise_intensity: 0.03,
            glow_color: rgb3(dim_amber),
        },
        ui_colors: UiColors {
            status_bar_bg: hex(0x08, 0x06, 0x02),
            status_bar_fg: dim_amber,
            tab_bar_bg: hex(0x08, 0x06, 0x02),
            tab_bar_fg: hex(0x99, 0x6E, 0x00),
            tab_active_bg: hex(0x1A, 0x14, 0x06),
            border: hex(0x33, 0x24, 0x00),
        },
    }
}

/// **Ice** — Cool blue / TRON aesthetic.
///
/// Deep dark background, bright cyan text, neon blue glow.
/// Minimal scanlines, clean digital look with subtle bloom.
pub fn ice() -> Theme {
    let cyan = hex(0x00, 0xD4, 0xFF);
    let dim_cyan = hex(0x00, 0x88, 0xAA);
    let bg = hex(0x04, 0x08, 0x12);

    Theme {
        name: "Ice".into(),
        colors: TerminalColors {
            foreground: cyan,
            background: bg,
            cursor: hex(0x44, 0xDD, 0xFF),
            selection: hexa(0x00, 0xD4, 0xFF, 0.22),
            ansi: [
                // Normal 0-7
                hex(0x04, 0x08, 0x12), // 0  black
                hex(0xFF, 0x33, 0x66), // 1  red (neon pink-red)
                hex(0x00, 0xCC, 0x88), // 2  green (teal)
                hex(0xCC, 0xDD, 0x44), // 3  yellow (lime)
                hex(0x00, 0x88, 0xFF), // 4  blue (neon blue)
                hex(0xBB, 0x44, 0xFF), // 5  magenta (electric purple)
                hex(0x00, 0xD4, 0xFF), // 6  cyan (ice)
                hex(0xBB, 0xCC, 0xDD), // 7  white (blue-tinted)
                // Bright 8-15
                hex(0x10, 0x1C, 0x2C), // 8  bright black
                hex(0xFF, 0x66, 0x88), // 9  bright red
                hex(0x44, 0xFF, 0xAA), // 10 bright green
                hex(0xEE, 0xFF, 0x77), // 11 bright yellow
                hex(0x44, 0xAA, 0xFF), // 12 bright blue
                hex(0xDD, 0x77, 0xFF), // 13 bright magenta
                hex(0x44, 0xEE, 0xFF), // 14 bright cyan
                hex(0xDD, 0xEE, 0xFF), // 15 bright white
            ],
        },
        shader_params: ShaderParams {
            scanline_intensity: 0.20,
            bloom_intensity: 0.45,
            chromatic_aberration: 0.08,
            curvature: 0.08,
            vignette_intensity: 0.30,
            noise_intensity: 0.03,
            glow_color: rgb3(dim_cyan),
        },
        ui_colors: UiColors {
            status_bar_bg: hex(0x02, 0x05, 0x0C),
            status_bar_fg: dim_cyan,
            tab_bar_bg: hex(0x02, 0x05, 0x0C),
            tab_bar_fg: hex(0x00, 0x77, 0x99),
            tab_active_bg: hex(0x08, 0x14, 0x22),
            border: hex(0x00, 0x2A, 0x44),
        },
    }
}

/// **Blood** — Red / Cyberpunk.
///
/// Very dark background, searing red text, high contrast.
/// Maximum menace. Heavy vignette, moderate scanlines.
pub fn blood() -> Theme {
    let red = hex(0xFF, 0x00, 0x33);
    let dim_red = hex(0xAA, 0x00, 0x22);
    let bg = hex(0x0A, 0x04, 0x06);

    Theme {
        name: "Blood".into(),
        colors: TerminalColors {
            foreground: red,
            background: bg,
            cursor: hex(0xFF, 0x44, 0x55),
            selection: hexa(0xFF, 0x00, 0x33, 0.25),
            ansi: [
                // Normal 0-7
                hex(0x0A, 0x04, 0x06), // 0  black
                hex(0xFF, 0x00, 0x33), // 1  red (blood)
                hex(0x88, 0x44, 0x33), // 2  green (dried blood)
                hex(0xDD, 0x66, 0x33), // 3  yellow (ember)
                hex(0x66, 0x22, 0x44), // 4  blue (bruise)
                hex(0xCC, 0x22, 0x66), // 5  magenta (crimson)
                hex(0xBB, 0x44, 0x44), // 6  cyan (rust)
                hex(0xCC, 0x99, 0x99), // 7  white (pale)
                // Bright 8-15
                hex(0x1A, 0x0C, 0x10), // 8  bright black
                hex(0xFF, 0x44, 0x55), // 9  bright red
                hex(0xBB, 0x66, 0x55), // 10 bright green
                hex(0xFF, 0x88, 0x55), // 11 bright yellow
                hex(0x99, 0x44, 0x66), // 12 bright blue
                hex(0xFF, 0x44, 0x88), // 13 bright magenta
                hex(0xDD, 0x66, 0x66), // 14 bright cyan
                hex(0xEE, 0xBB, 0xBB), // 15 bright white
            ],
        },
        shader_params: ShaderParams {
            scanline_intensity: 0.15,
            bloom_intensity: 0.20,
            chromatic_aberration: 0.05,
            curvature: 0.05,
            vignette_intensity: 0.25,
            noise_intensity: 0.03,
            glow_color: rgb3(dim_red),
        },
        ui_colors: UiColors {
            status_bar_bg: hex(0x06, 0x02, 0x04),
            status_bar_fg: dim_red,
            tab_bar_bg: hex(0x06, 0x02, 0x04),
            tab_bar_fg: hex(0x88, 0x00, 0x1A),
            tab_active_bg: hex(0x18, 0x06, 0x0C),
            border: hex(0x44, 0x00, 0x11),
        },
    }
}

/// **Vapor** — Vaporwave / Retrowave.
///
/// Purple-pink palette, dual accent colors (#FF71CE pink, #01CDFE cyan).
/// Moderate CRT effects, dreamy bloom. Maximum A E S T H E T I C.
pub fn vapor() -> Theme {
    let pink = hex(0xFF, 0x71, 0xCE);
    let cyan = hex(0x01, 0xCD, 0xFE);
    let dim_pink = hex(0xAA, 0x44, 0x88);
    let bg = hex(0x0C, 0x04, 0x14);

    let _ = cyan; // used in ansi palette below

    Theme {
        name: "Vapor".into(),
        colors: TerminalColors {
            foreground: pink,
            background: bg,
            cursor: hex(0x01, 0xCD, 0xFE),
            selection: hexa(0xFF, 0x71, 0xCE, 0.22),
            ansi: [
                // Normal 0-7
                hex(0x0C, 0x04, 0x14), // 0  black
                hex(0xFF, 0x33, 0x88), // 1  red (hot pink)
                hex(0x01, 0xCD, 0xFE), // 2  green (miami cyan)
                hex(0xFF, 0xF0, 0x68), // 3  yellow (sun)
                hex(0x77, 0x44, 0xDD), // 4  blue (synthwave purple)
                hex(0xFF, 0x71, 0xCE), // 5  magenta (vapor pink)
                hex(0x05, 0xFC, 0xC1), // 6  cyan (neon mint)
                hex(0xE0, 0xCC, 0xEE), // 7  white (lavender)
                // Bright 8-15
                hex(0x1C, 0x10, 0x2C), // 8  bright black
                hex(0xFF, 0x66, 0xAA), // 9  bright red
                hex(0x44, 0xDD, 0xFF), // 10 bright green
                hex(0xFF, 0xFF, 0x99), // 11 bright yellow
                hex(0x99, 0x66, 0xFF), // 12 bright blue
                hex(0xFF, 0x99, 0xDD), // 13 bright magenta
                hex(0x66, 0xFF, 0xDD), // 14 bright cyan
                hex(0xF0, 0xE0, 0xFF), // 15 bright white
            ],
        },
        shader_params: ShaderParams {
            scanline_intensity: 0.12,
            bloom_intensity: 0.22,
            chromatic_aberration: 0.03,
            curvature: 0.04,
            vignette_intensity: 0.18,
            noise_intensity: 0.02,
            glow_color: rgb3(dim_pink),
        },
        ui_colors: UiColors {
            status_bar_bg: hex(0x08, 0x02, 0x0E),
            status_bar_fg: dim_pink,
            tab_bar_bg: hex(0x08, 0x02, 0x0E),
            tab_bar_fg: hex(0x88, 0x44, 0x99),
            tab_active_bg: hex(0x18, 0x08, 0x28),
            border: hex(0x33, 0x11, 0x44),
        },
    }
}

/// **Pip-Boy** — Vault-Tec terminal from the wasteland.
///
/// The Fallout Pip-Boy 3000 aesthetic: bright green (#20C20E) on near-black,
/// heavy scanlines, aggressive bloom, slight curvature. Chunky, utilitarian,
/// radiation-proof. War never changes, but your terminal can.
pub fn pipboy() -> Theme {
    let green = hex(0x20, 0xC2, 0x0E);
    let dim_green = hex(0x14, 0x7A, 0x0A);
    let dark_green = hex(0x0A, 0x3D, 0x05);
    let bg = hex(0x05, 0x0C, 0x04);

    Theme {
        name: "Pip-Boy".into(),
        colors: TerminalColors {
            foreground: green,
            background: bg,
            cursor: hex(0x30, 0xFF, 0x15),
            selection: hexa(0x20, 0xC2, 0x0E, 0.30),
            ansi: [
                // Normal 0-7 — monochrome green spectrum
                hex(0x05, 0x0C, 0x04), // 0  black (bg)
                hex(0x40, 0xA0, 0x10), // 1  red (olive green)
                hex(0x20, 0xC2, 0x0E), // 2  green (pip-boy green)
                hex(0x60, 0xD0, 0x20), // 3  yellow (lime)
                hex(0x10, 0x80, 0x30), // 4  blue (forest)
                hex(0x30, 0x90, 0x20), // 5  magenta (mid green)
                hex(0x18, 0xAA, 0x40), // 6  cyan (emerald)
                hex(0x70, 0xD0, 0x60), // 7  white (pale green)
                // Bright 8-15
                hex(0x0A, 0x1A, 0x08), // 8  bright black
                hex(0x50, 0xC0, 0x20), // 9  bright red
                hex(0x30, 0xFF, 0x15), // 10 bright green (max glow)
                hex(0x80, 0xFF, 0x30), // 11 bright yellow
                hex(0x20, 0xA0, 0x50), // 12 bright blue
                hex(0x40, 0xC0, 0x30), // 13 bright magenta
                hex(0x28, 0xDD, 0x50), // 14 bright cyan
                hex(0x90, 0xFF, 0x70), // 15 bright white (max)
            ],
        },
        shader_params: ShaderParams {
            scanline_intensity: 0.25, // heavy scanlines — CRT authenticity
            bloom_intensity: 0.20,    // strong phosphor glow
            chromatic_aberration: 0.02,
            curvature: 0.06,          // slight barrel distortion
            vignette_intensity: 0.22, // dark corners like a real CRT
            noise_intensity: 0.04,    // wasteland static
            glow_color: rgb3(dim_green),
        },
        ui_colors: UiColors {
            status_bar_bg: hex(0x03, 0x08, 0x02),
            status_bar_fg: dim_green,
            tab_bar_bg: hex(0x03, 0x08, 0x02),
            tab_bar_fg: dark_green,
            tab_active_bg: hex(0x0A, 0x1A, 0x06),
            border: hex(0x14, 0x40, 0x0A),
        },
    }
}

/// Look up a built-in theme by name (case-insensitive).
///
/// Returns `None` if the name doesn't match any built-in theme.
pub fn builtin_by_name(name: &str) -> Option<Theme> {
    match name.to_ascii_lowercase().as_str() {
        "phosphor" => Some(phosphor()),
        "amber" => Some(amber()),
        "ice" => Some(ice()),
        "blood" => Some(blood()),
        "vapor" | "vaporwave" => Some(vapor()),
        "pipboy" | "pip-boy" => Some(pipboy()),
        _ => None,
    }
}

/// Names of all built-in themes, in presentation order.
pub const BUILTIN_NAMES: &[&str] = &["Phosphor", "Amber", "Ice", "Blood", "Vapor", "Pip-Boy"];

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_color_range(color: &[f32; 4], label: &str) {
        for (i, &c) in color.iter().enumerate() {
            assert!(
                (0.0..=1.0).contains(&c),
                "{label} channel {i} out of range: {c}"
            );
        }
    }

    fn assert_shader_range(params: &ShaderParams, theme_name: &str) {
        let fields = [
            ("scanline_intensity", params.scanline_intensity),
            ("bloom_intensity", params.bloom_intensity),
            ("chromatic_aberration", params.chromatic_aberration),
            ("curvature", params.curvature),
            ("vignette_intensity", params.vignette_intensity),
            ("noise_intensity", params.noise_intensity),
        ];
        for (name, val) in fields {
            assert!(
                (0.0..=1.0).contains(&val),
                "{theme_name}.shader_params.{name} out of range: {val}"
            );
        }
        for (i, &c) in params.glow_color.iter().enumerate() {
            assert!(
                (0.0..=1.0).contains(&c),
                "{theme_name}.shader_params.glow_color[{i}] out of range: {c}"
            );
        }
    }

    fn validate_theme(theme: &Theme) {
        let n = &theme.name;

        assert_color_range(&theme.colors.foreground, &format!("{n}.foreground"));
        assert_color_range(&theme.colors.background, &format!("{n}.background"));
        assert_color_range(&theme.colors.cursor, &format!("{n}.cursor"));
        assert_color_range(&theme.colors.selection, &format!("{n}.selection"));

        for (i, color) in theme.colors.ansi.iter().enumerate() {
            assert_color_range(color, &format!("{n}.ansi[{i}]"));
        }

        assert_shader_range(&theme.shader_params, n);

        assert_color_range(
            &theme.ui_colors.status_bar_bg,
            &format!("{n}.status_bar_bg"),
        );
        assert_color_range(
            &theme.ui_colors.status_bar_fg,
            &format!("{n}.status_bar_fg"),
        );
        assert_color_range(&theme.ui_colors.tab_bar_bg, &format!("{n}.tab_bar_bg"));
        assert_color_range(&theme.ui_colors.tab_bar_fg, &format!("{n}.tab_bar_fg"));
        assert_color_range(
            &theme.ui_colors.tab_active_bg,
            &format!("{n}.tab_active_bg"),
        );
        assert_color_range(&theme.ui_colors.border, &format!("{n}.border"));
    }

    #[test]
    fn all_builtins_valid() {
        validate_theme(&phosphor());
        validate_theme(&amber());
        validate_theme(&ice());
        validate_theme(&blood());
        validate_theme(&vapor());
    }

    #[test]
    fn default_is_phosphor() {
        assert_eq!(Theme::default().name, "Phosphor");
    }

    #[test]
    fn builtin_lookup() {
        assert_eq!(builtin_by_name("Phosphor").unwrap().name, "Phosphor");
        assert_eq!(builtin_by_name("AMBER").unwrap().name, "Amber");
        assert_eq!(builtin_by_name("vaporwave").unwrap().name, "Vapor");
        assert!(builtin_by_name("nonexistent").is_none());
    }

    #[test]
    fn phosphor_bg_is_void() {
        let t = phosphor();
        // #0a0e14
        let bg = t.colors.background;
        assert!((bg[0] - 0.0392).abs() < 0.01);
        assert!((bg[1] - 0.0549).abs() < 0.01);
        assert!((bg[2] - 0.0784).abs() < 0.01);
    }

    #[test]
    fn all_themes_have_16_ansi_colors() {
        // Compile-time guarantee via fixed-size array, but belt and suspenders.
        assert_eq!(phosphor().colors.ansi.len(), 16);
        assert_eq!(amber().colors.ansi.len(), 16);
        assert_eq!(ice().colors.ansi.len(), 16);
        assert_eq!(blood().colors.ansi.len(), 16);
        assert_eq!(vapor().colors.ansi.len(), 16);
    }

    // ── QA #169: Theme switching — --theme flag changes visual appearance ──────

    /// All four CLI-exposed themes parse successfully via `builtin_by_name`.
    #[test]
    fn qa_169_all_cli_themes_parse_successfully() {
        let cli_themes = ["amber", "ice", "blood", "vapor"];
        for name in cli_themes {
            let result = builtin_by_name(name);
            assert!(
                result.is_some(),
                "theme '{name}' must be recognised by builtin_by_name",
            );
        }
    }

    /// An unknown theme name must return `None` (not panic).
    #[test]
    fn qa_169_unknown_theme_returns_none() {
        assert!(
            builtin_by_name("nonexistent").is_none(),
            "unknown theme must return None",
        );
        assert!(
            builtin_by_name("").is_none(),
            "empty string must return None"
        );
        assert!(
            builtin_by_name("NEON").is_none(),
            "unregistered name must return None"
        );
    }

    /// Foreground colors must differ between themes.
    /// The whole point of a theme flag is to change the visual appearance;
    /// if foregrounds were identical there would be no visual difference.
    #[test]
    fn qa_169_themes_have_distinct_foreground_colors() {
        let amber_fg = amber().colors.foreground;
        let ice_fg = ice().colors.foreground;
        let blood_fg = blood().colors.foreground;
        let vapor_fg = vapor().colors.foreground;

        // Each pair must differ on at least one channel.
        let pairs = [
            ("amber", amber_fg, "ice", ice_fg),
            ("amber", amber_fg, "blood", blood_fg),
            ("amber", amber_fg, "vapor", vapor_fg),
            ("ice", ice_fg, "blood", blood_fg),
            ("ice", ice_fg, "vapor", vapor_fg),
            ("blood", blood_fg, "vapor", vapor_fg),
        ];

        for (na, ca, nb, cb) in pairs {
            let same = ca.iter().zip(cb.iter()).all(|(a, b)| (a - b).abs() < 0.01);
            assert!(
                !same,
                "theme '{na}' and '{nb}' have identical foreground colors — themes must differ",
            );
        }
    }

    /// Background colors must also differ between themes.
    #[test]
    fn qa_169_themes_have_distinct_background_colors() {
        let amber_bg = amber().colors.background;
        let ice_bg = ice().colors.background;
        let blood_bg = blood().colors.background;
        let vapor_bg = vapor().colors.background;

        let pairs = [
            ("amber", amber_bg, "ice", ice_bg),
            ("amber", amber_bg, "blood", blood_bg),
            ("ice", ice_bg, "vapor", vapor_bg),
        ];

        for (na, ca, nb, cb) in pairs {
            let same = ca.iter().zip(cb.iter()).all(|(a, b)| (a - b).abs() < 0.01);
            assert!(
                !same,
                "theme '{na}' and '{nb}' must have different background colors",
            );
        }
    }

    /// `builtin_by_name` must be case-insensitive for all four CLI themes.
    #[test]
    fn qa_169_theme_lookup_is_case_insensitive() {
        assert_eq!(builtin_by_name("Amber").unwrap().name, "Amber");
        assert_eq!(builtin_by_name("AMBER").unwrap().name, "Amber");
        assert_eq!(builtin_by_name("Ice").unwrap().name, "Ice");
        assert_eq!(builtin_by_name("ICE").unwrap().name, "Ice");
        assert_eq!(builtin_by_name("Blood").unwrap().name, "Blood");
        assert_eq!(builtin_by_name("BLOOD").unwrap().name, "Blood");
        assert_eq!(builtin_by_name("Vapor").unwrap().name, "Vapor");
        assert_eq!(builtin_by_name("VAPOR").unwrap().name, "Vapor");
    }

    /// Each theme's shader params must differ, proving visual appearance changes.
    #[test]
    fn qa_169_themes_have_distinct_shader_params() {
        let amber_sp = amber().shader_params;
        let ice_sp = ice().shader_params;
        let blood_sp = blood().shader_params;
        let vapor_sp = vapor().shader_params;

        // Bloom intensity alone differs enough to prove distinct appearance.
        let blooms = [
            ("amber", amber_sp.bloom_intensity),
            ("ice", ice_sp.bloom_intensity),
            ("blood", blood_sp.bloom_intensity),
            ("vapor", vapor_sp.bloom_intensity),
        ];

        // Not all bloom values can be equal.
        let first = blooms[0].1;
        let all_same = blooms.iter().all(|(_, b)| (b - first).abs() < 0.001);
        assert!(
            !all_same,
            "all themes have identical bloom_intensity — shader params must differ between themes",
        );
    }
}
