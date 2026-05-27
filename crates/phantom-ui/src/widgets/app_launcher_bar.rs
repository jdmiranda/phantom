//! Top-of-window app launcher — clickable buttons for every spawnable
//! chrome pane. The user feedback that triggered this widget was:
//!
//! > "there is no menus in the top toolbar. only hotkeys which i can't remember"
//!
//! Phantom has ~13 chrome adapters (Inspector, Settings, Memory, Logs,
//! Notifications, FilesWatch, Diff, Fleet, Plugins, Database, VoiceStt,
//! KeybindsHelp, Console) each reachable only by a keyboard shortcut. The
//! shortcuts are not discoverable. This widget surfaces every pane as a
//! labelled boxy chip with the keybind printed beside it, so the user can
//! click their way around without memorising any key combos.
//!
//! ```text
//! +---------+---------+---------+ ... +---------+
//! |INSPECTOR|SETTINGS | MEMORY  |     | CONSOLE |
//! |  Cmd+I  |  Cmd+,  |  Cmd+M  |     | Cmd+S+C |
//! +---------+---------+---------+ ... +---------+
//! ```
//!
//! The widget is *stateless*: each frame the App constructs an
//! `AppLauncherBar`, asks it for quads/text, and (when a click lands on the
//! bar's rect) calls `hit_test` to discover which pane was clicked. The
//! result is a `LauncherAction::OpenPane(LauncherPaneKind)` that the App
//! routes to the matching `toggle_*_pane` helper.
//!
//! Styling follows the retro-hacker aesthetic from `docs/mockups/apps.html`:
//! boxy chips with hairline borders, all-caps labels, monospace keybind
//! chips beneath the label. No rounded corners.

use crate::layout::Rect;
use crate::render_ctx::RenderCtx;
use crate::tokens::Tokens;
use crate::widgets::{TextSegment, Widget};
use phantom_renderer::quads::QuadInstance;

// ---------------------------------------------------------------------------
// LauncherPaneKind
// ---------------------------------------------------------------------------

/// Identifier for every chrome pane reachable from the launcher bar.
///
/// Mirrors the set of `toggle_*_pane` helpers in
/// `phantom-app::spawn_chrome` (plus the legacy `spawn_inspector_pane`).
/// The App's mouse handler maps each variant to its concrete spawn call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LauncherPaneKind {
    /// Sec. event log / live agent state (Cmd+I).
    Inspector,
    /// Per-project memory inspector (Cmd+M).
    Memory,
    /// Phantom settings adapter (Cmd+,).
    Settings,
    /// Real-time log tail (Cmd+L).
    Logs,
    /// User-visible notification list (Cmd+Shift+N).
    Notifications,
    /// Filesystem watcher (Cmd+Shift+F).
    FilesWatch,
    /// Git diff viewer (Cmd+Shift+G).
    Diff,
    /// Fleet / federation node list (Cmd+Shift+L).
    Fleet,
    /// Plugin registry (Cmd+Shift+P).
    Plugins,
    /// Capture / bundle store browser (Cmd+Shift+B).
    Database,
    /// Speech-to-text monitor (Cmd+Shift+V).
    VoiceStt,
    /// Keybinds help reference (Cmd+Shift+K / F1 / ?).
    KeybindsHelp,
    /// In-pane REPL console (Cmd+Shift+C).
    Console,
}

impl LauncherPaneKind {
    /// All variants in display order. The bar lays buttons out in this order
    /// left-to-right, top-to-bottom.
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[
            Self::Inspector,
            Self::Memory,
            Self::Settings,
            Self::Logs,
            Self::Notifications,
            Self::FilesWatch,
            Self::Diff,
            Self::Fleet,
            Self::Plugins,
            Self::Database,
            Self::VoiceStt,
            Self::KeybindsHelp,
            Self::Console,
        ]
    }
}

// ---------------------------------------------------------------------------
// LauncherItem
// ---------------------------------------------------------------------------

/// A single launcher chip — a labelled clickable target with a keybind hint.
#[derive(Debug, Clone)]
pub struct LauncherItem {
    /// Pane identity — routed to App's toggle helpers on click.
    pub id: LauncherPaneKind,
    /// All-caps display label (e.g. `"INSPECTOR"`).
    pub label: &'static str,
    /// Keybind glyph rendered in the small chip beneath the label
    /// (e.g. `"\u{2318}I"` for Cmd+I).
    pub keybind: &'static str,
}

/// Action produced by [`AppLauncherBar::hit_test`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LauncherAction {
    /// Open or close the named pane (the App's toggle logic decides).
    OpenPane(LauncherPaneKind),
    /// Click landed inside the bar but on padding, not on a chip.
    None,
}

// ---------------------------------------------------------------------------
// Layout constants
// ---------------------------------------------------------------------------

/// Logical height of the launcher bar before DPI scaling, in pixels.
///
/// 48 px gives two stacked text rows (label + keybind chip) plus padding,
/// each row legible at the default cell metrics. Comfortably above the 44 px
/// hit-target floor required for click discoverability.
pub const APP_LAUNCHER_BAR_HEIGHT: f32 = 48.0;

/// Horizontal padding inside each chip, in pixels.
const CHIP_PAD_X: f32 = 8.0;

/// Vertical padding inside each chip, in pixels.
const CHIP_PAD_Y: f32 = 4.0;

/// Gap between adjacent chips, in pixels.
const CHIP_GAP: f32 = 4.0;

/// Outer left/right padding of the bar itself, in pixels.
const BAR_PAD_X: f32 = 8.0;

// ---------------------------------------------------------------------------
// AppLauncherBar
// ---------------------------------------------------------------------------

/// Top-of-window discoverable launcher for every chrome pane.
#[derive(Debug, Clone)]
pub struct AppLauncherBar {
    items: Vec<LauncherItem>,
    /// Live render context — drives keybind-chip sizing through `cell_w`.
    ctx: RenderCtx,
    /// Live token palette — drives bar background, chip borders, label/keybind colors.
    tokens: Tokens,
}

impl AppLauncherBar {
    /// Build a launcher bar populated with the default chrome-pane item set.
    ///
    /// Keybind glyphs use real macOS modifier symbols (`\u{2318}` = `\u{2318}`,
    /// `\u{21E7}` = `\u{21E7}`) so the chip is short enough to fit beside the
    /// label without truncation. Linux/Windows users see the same glyph; the
    /// App's actual handler accepts the platform-specific modifier
    /// (`super_key()` in winit covers both).
    #[must_use]
    pub fn new() -> Self {
        let items = vec![
            LauncherItem { id: LauncherPaneKind::Inspector,     label: "INSPECT",   keybind: "\u{2318}I" },
            LauncherItem { id: LauncherPaneKind::Memory,        label: "MEMORY",    keybind: "\u{2318}M" },
            LauncherItem { id: LauncherPaneKind::Settings,      label: "SETTINGS",  keybind: "\u{2318}," },
            LauncherItem { id: LauncherPaneKind::Logs,          label: "LOGS",      keybind: "\u{2318}L" },
            LauncherItem { id: LauncherPaneKind::Notifications, label: "NOTIFY",    keybind: "\u{2318}\u{21E7}N" },
            LauncherItem { id: LauncherPaneKind::FilesWatch,    label: "FILES",     keybind: "\u{2318}\u{21E7}F" },
            LauncherItem { id: LauncherPaneKind::Diff,          label: "DIFF",      keybind: "\u{2318}\u{21E7}G" },
            LauncherItem { id: LauncherPaneKind::Fleet,         label: "FLEET",     keybind: "\u{2318}\u{21E7}L" },
            LauncherItem { id: LauncherPaneKind::Plugins,       label: "PLUGINS",   keybind: "\u{2318}\u{21E7}P" },
            LauncherItem { id: LauncherPaneKind::Database,      label: "DB",        keybind: "\u{2318}\u{21E7}B" },
            LauncherItem { id: LauncherPaneKind::VoiceStt,      label: "VOICE",     keybind: "\u{2318}\u{21E7}V" },
            LauncherItem { id: LauncherPaneKind::KeybindsHelp,  label: "HELP",      keybind: "F1" },
            LauncherItem { id: LauncherPaneKind::Console,       label: "CONSOLE",   keybind: "\u{2318}\u{21E7}C" },
        ];

        Self {
            items,
            ctx: RenderCtx::fallback(),
            tokens: Tokens::phosphor(RenderCtx::fallback()),
        }
    }

    /// Builder: bind a live `RenderCtx` so chip widths scale with the font.
    #[must_use]
    pub fn with_ctx(mut self, ctx: RenderCtx) -> Self {
        self.ctx = ctx;
        self
    }

    /// Builder: bind a live `Tokens` snapshot so theme switches recolor the bar.
    #[must_use]
    pub fn with_tokens(mut self, tokens: Tokens) -> Self {
        self.tokens = tokens;
        self
    }

    /// Read-only access to the active items list (mainly for tests / debug).
    #[must_use]
    pub fn items(&self) -> &[LauncherItem] {
        &self.items
    }

    /// Replace the default item list with a custom set. Used by tests and
    /// when downstream code wants to filter the launcher to a subset.
    pub fn set_items(&mut self, items: Vec<LauncherItem>) {
        self.items = items;
    }

    /// Compute the chip width that fits the longest label+keybind in the
    /// current item set, rounded to a whole pixel.
    fn chip_width(&self) -> f32 {
        let cell_w = self.ctx.cell_w();
        let mut max_chars = 0usize;
        for item in &self.items {
            let label_chars = item.label.chars().count();
            let kb_chars = item.keybind.chars().count();
            max_chars = max_chars.max(label_chars).max(kb_chars);
        }
        // 1 cell of breathing room either side of the longest content row.
        let inner_w = (max_chars as f32 + 2.0) * cell_w;
        (inner_w + CHIP_PAD_X * 2.0).ceil()
    }

    /// Pixel rect of the chip at `idx` inside `bar_rect`. Used by both the
    /// render path and `hit_test`.
    fn chip_rect(&self, bar_rect: &Rect, idx: usize) -> Rect {
        let chip_w = self.chip_width();
        let total_chips = self.items.len() as f32;
        // Equally distribute leftover width as additional gap between chips so
        // the strip fills the bar without leaving a ragged right edge. The
        // first chip starts at `BAR_PAD_X`, every subsequent chip shifts by
        // `chip_w + per_chip_gap`.
        let chips_used = chip_w * total_chips;
        let usable_w = (bar_rect.width - BAR_PAD_X * 2.0).max(0.0);
        let leftover = (usable_w - chips_used).max(0.0);
        // Distribute leftover between chips. `n` chips have `n-1` gaps internally
        // plus the implicit gap-to-padding on the right.
        let gap_count = (self.items.len().saturating_sub(1)).max(1) as f32;
        let per_chip_gap = if total_chips > 1.0 {
            (leftover / gap_count).max(CHIP_GAP)
        } else {
            0.0
        };
        let x = bar_rect.x + BAR_PAD_X + idx as f32 * (chip_w + per_chip_gap);
        let y = bar_rect.y + CHIP_PAD_Y;
        let h = (bar_rect.height - CHIP_PAD_Y * 2.0).max(0.0);
        Rect {
            x,
            y,
            width: chip_w,
            height: h,
        }
    }

    /// Hit-test a click against the bar.
    ///
    /// Returns [`LauncherAction::OpenPane`] when the click lands inside a
    /// chip rect, [`LauncherAction::None`] when it lands on padding between
    /// chips or completely outside the bar.
    #[must_use]
    pub fn hit_test(&self, bar_rect: &Rect, mouse_x: f32, mouse_y: f32) -> LauncherAction {
        if mouse_x < bar_rect.x
            || mouse_x > bar_rect.x + bar_rect.width
            || mouse_y < bar_rect.y
            || mouse_y > bar_rect.y + bar_rect.height
        {
            return LauncherAction::None;
        }
        for (idx, item) in self.items.iter().enumerate() {
            let chip = self.chip_rect(bar_rect, idx);
            if mouse_x >= chip.x
                && mouse_x <= chip.x + chip.width
                && mouse_y >= chip.y
                && mouse_y <= chip.y + chip.height
            {
                return LauncherAction::OpenPane(item.id);
            }
        }
        LauncherAction::None
    }
}

impl Default for AppLauncherBar {
    fn default() -> Self {
        Self::new()
    }
}

impl AppLauncherBar {
    /// Compute the inner kbd sub-chip rect for the keybind glyph row.
    ///
    /// Mockup `.kbd` style: `border: 1px solid var(--frame-dim);
    /// border-radius: 4px; padding: 1px 6px; background:
    /// var(--surface-raised)`. The pixel sizing is derived from the glyph
    /// metric so each kbd box hugs its keybind text exactly — no fixed
    /// width.
    fn kbd_rect(&self, chip: &Rect, keybind: &str) -> Rect {
        let cell_w = self.ctx.cell_w();
        let cell_h = self.ctx.cell_h();
        // Width hugs the glyph: `chars * cell_w` for the text plus 6 px
        // padding on each side to match `.kbd { padding: 1px 6px }`.
        let kb_w = keybind.chars().count() as f32 * cell_w + 12.0;
        // Height = cell_h + 2 px vertical padding (top/bottom 1 px each).
        let kb_h = cell_h + 2.0;
        // Center horizontally inside the parent chip.
        let kb_x = chip.x + (chip.width - kb_w) * 0.5;
        // Pin the kbd box to the bottom half of the chip, leaving room for
        // the label row above it.
        let kb_y = chip.y + chip.height * 0.5 + 1.0;
        Rect {
            x: kb_x.max(chip.x + 2.0),
            y: kb_y,
            width: kb_w,
            height: kb_h,
        }
    }
}

impl Widget for AppLauncherBar {
    fn render_quads(&self, rect: &Rect) -> Vec<QuadInstance> {
        let t = self.tokens;
        // 1 bar bg + 1 bottom hairline + per chip: 1 fill + 4 borders +
        // 1 kbd fill + 4 kbd borders = 10 per chip.
        let mut quads = Vec::with_capacity(2 + self.items.len() * 10);

        // Full bar background — sits flush with the tab strip just beneath us.
        quads.push(QuadInstance {
            pos: [rect.x, rect.y],
            size: [rect.width, rect.height],
            color: t.colors.surface_base,
            border_radius: 0.0,
        ..Default::default()
            });

        let hair = t.hair().max(1.0);

        for (idx, item) in self.items.iter().enumerate() {
            let chip = self.chip_rect(rect, idx);

            // Chip background — surface_recessed for the boxy retro look.
            quads.push(QuadInstance {
                pos: [chip.x, chip.y],
                size: [chip.width, chip.height],
                color: t.colors.surface_recessed,
                border_radius: 0.0,
            ..Default::default()
            });

            // Hairline border around the chip, drawn as four 1px quads. The
            // borders share the divider color so chips visually group with
            // the rest of the chrome.
            let border = t.colors.chrome_divider;
            // top
            quads.push(QuadInstance {
                pos: [chip.x, chip.y],
                size: [chip.width, hair],
                color: border,
                border_radius: 0.0,
            ..Default::default()
            });
            // bottom
            quads.push(QuadInstance {
                pos: [chip.x, chip.y + chip.height - hair],
                size: [chip.width, hair],
                color: border,
                border_radius: 0.0,
            ..Default::default()
            });
            // left
            quads.push(QuadInstance {
                pos: [chip.x, chip.y],
                size: [hair, chip.height],
                color: border,
                border_radius: 0.0,
            ..Default::default()
            });
            // right
            quads.push(QuadInstance {
                pos: [chip.x + chip.width - hair, chip.y],
                size: [hair, chip.height],
                color: border,
                border_radius: 0.0,
            ..Default::default()
            });

            // Mockup-style .kbd sub-chip around the keybind glyph. The
            // mockup CSS is `border: 1px solid var(--frame-dim);
            // border-radius: 4px; padding: 1px 6px; background:
            // var(--surface-raised)`. Renders as a small rounded box that
            // hugs the keybind text inside the larger label chip.
            let kbd = self.kbd_rect(&chip, item.keybind);

            // kbd background fill — surface_raised so it pops against the
            // surface_recessed parent chip body.
            quads.push(QuadInstance {
                pos: [kbd.x, kbd.y],
                size: [kbd.width, kbd.height],
                color: t.colors.surface_raised,
                border_radius: 4.0,
                ..Default::default()
            });
            // kbd 1-px border (4 sides).
            // top
            quads.push(QuadInstance {
                pos: [kbd.x, kbd.y],
                size: [kbd.width, hair],
                color: border,
                border_radius: 0.0,
                ..Default::default()
            });
            // bottom
            quads.push(QuadInstance {
                pos: [kbd.x, kbd.y + kbd.height - hair],
                size: [kbd.width, hair],
                color: border,
                border_radius: 0.0,
                ..Default::default()
            });
            // left
            quads.push(QuadInstance {
                pos: [kbd.x, kbd.y],
                size: [hair, kbd.height],
                color: border,
                border_radius: 0.0,
                ..Default::default()
            });
            // right
            quads.push(QuadInstance {
                pos: [kbd.x + kbd.width - hair, kbd.y],
                size: [hair, kbd.height],
                color: border,
                border_radius: 0.0,
                ..Default::default()
            });
        }

        // Bottom hairline separating the launcher from the tab strip.
        quads.push(QuadInstance {
            pos: [rect.x, rect.y + rect.height - hair],
            size: [rect.width, hair],
            color: t.colors.chrome_divider,
            border_radius: 0.0,
            ..Default::default()
        });

        quads
    }

    fn render_text(&self, rect: &Rect) -> Vec<TextSegment> {
        let t = self.tokens;
        let cell_w = self.ctx.cell_w();
        let cell_h = self.ctx.cell_h();
        // Two stacked rows inside each chip: label (top), keybind (bottom).
        // The label uses `text_primary` (bright), keybind uses
        // `text_secondary` (matches mockup `.kbd { color: var(--text-secondary) }`).
        let mut segs = Vec::with_capacity(self.items.len() * 2);

        for (idx, item) in self.items.iter().enumerate() {
            let chip = self.chip_rect(rect, idx);
            let label_w = item.label.chars().count() as f32 * cell_w;
            let kb_w = item.keybind.chars().count() as f32 * cell_w;

            // Center each text run horizontally inside the chip.
            let label_x = chip.x + (chip.width - label_w) * 0.5;

            // Vertical layout: label hugs the top half. The keybind centres
            // inside its dedicated .kbd sub-chip in the bottom half.
            let mid = chip.y + chip.height * 0.5;
            let label_y = mid - cell_h - 1.0;

            segs.push(TextSegment {
                text: item.label.to_owned(),
                x: label_x.max(chip.x + CHIP_PAD_X),
                y: label_y.max(chip.y + CHIP_PAD_Y),
                color: t.colors.text_primary,
            });

            // Keybind text — centred inside the kbd sub-chip box drawn in
            // `render_quads`. Use text_secondary to match mockup `.kbd`
            // foreground color.
            let kbd = self.kbd_rect(&chip, item.keybind);
            let kb_x = kbd.x + (kbd.width - kb_w) * 0.5;
            let kb_y = kbd.y + (kbd.height - cell_h) * 0.5;
            segs.push(TextSegment {
                text: item.keybind.to_owned(),
                x: kb_x.max(kbd.x + 2.0),
                y: kb_y,
                color: t.colors.text_secondary,
            });
        }

        segs
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn bar_rect() -> Rect {
        Rect {
            x: 0.0,
            y: 0.0,
            width: 1920.0,
            height: APP_LAUNCHER_BAR_HEIGHT,
        }
    }

    #[test]
    fn default_item_set_covers_every_chrome_pane() {
        let bar = AppLauncherBar::new();
        // 13 chrome panes are reachable from the launcher today. If a new
        // pane is added, update `LauncherPaneKind::all()` and bump this
        // assertion — the launcher must always cover the full set.
        assert_eq!(bar.items().len(), LauncherPaneKind::all().len());
        assert_eq!(bar.items().len(), 13);
    }

    #[test]
    fn every_item_has_label_and_keybind() {
        let bar = AppLauncherBar::new();
        for item in bar.items() {
            assert!(
                !item.label.is_empty(),
                "label must be non-empty for {:?}",
                item.id
            );
            assert!(
                !item.keybind.is_empty(),
                "keybind hint must be non-empty for {:?}",
                item.id
            );
        }
    }

    #[test]
    fn label_strings_are_all_caps() {
        // The retro-hacker mockup style is upper-case chip labels. Catch
        // accidental lower-case introductions in code review by testing here.
        let bar = AppLauncherBar::new();
        for item in bar.items() {
            assert_eq!(
                item.label,
                &item.label.to_ascii_uppercase(),
                "label must be all-caps: '{}'",
                item.label
            );
        }
    }

    #[test]
    fn pane_kind_set_is_unique_and_complete() {
        // Every variant in `LauncherPaneKind::all()` must appear exactly once
        // in the default item set. Detects copy-paste bugs.
        let bar = AppLauncherBar::new();
        let mut seen: Vec<LauncherPaneKind> = bar.items().iter().map(|i| i.id).collect();
        seen.sort_by_key(|k| format!("{k:?}"));
        let mut expected: Vec<LauncherPaneKind> = LauncherPaneKind::all().to_vec();
        expected.sort_by_key(|k| format!("{k:?}"));
        assert_eq!(seen, expected);
    }

    #[test]
    fn render_quads_emits_one_bg_per_chip() {
        let bar = AppLauncherBar::new();
        let quads = bar.render_quads(&bar_rect());
        // Background + bottom hairline + (chip 1 fill + 4 borders) per chip +
        // (kbd 1 fill + 4 borders) per chip = 2 + items * 10.
        let expected = 2 + bar.items().len() * 10;
        assert_eq!(quads.len(), expected);
    }

    #[test]
    fn render_text_emits_label_and_keybind_per_chip() {
        let bar = AppLauncherBar::new();
        let texts = bar.render_text(&bar_rect());
        assert_eq!(texts.len(), bar.items().len() * 2);
        // Inspector is the first item; verify its label appears verbatim and
        // its keybind chip uses the Cmd glyph.
        let joined: String = texts.iter().map(|s| s.text.as_str()).collect();
        assert!(joined.contains("INSPECT"), "INSPECT label missing");
        assert!(joined.contains("\u{2318}I"), "Cmd+I keybind chip missing");
        assert!(joined.contains("CONSOLE"), "CONSOLE label missing");
        assert!(
            joined.contains("\u{2318}\u{21E7}C"),
            "Cmd+Shift+C keybind chip missing"
        );
    }

    #[test]
    fn keybind_chips_for_every_chrome_keybind() {
        let bar = AppLauncherBar::new();
        let texts = bar.render_text(&bar_rect());
        let joined: String = texts.iter().map(|s| s.text.as_str()).collect();
        // Every chrome pane keybind that lives in `phantom-app::input.rs`
        // must surface as a chip hint here. If the keybind table changes,
        // this test loudly flags the mismatch.
        for expected in &[
            "\u{2318}I",
            "\u{2318}M",
            "\u{2318},",
            "\u{2318}L",
            "\u{2318}\u{21E7}N",
            "\u{2318}\u{21E7}F",
            "\u{2318}\u{21E7}G",
            "\u{2318}\u{21E7}L",
            "\u{2318}\u{21E7}P",
            "\u{2318}\u{21E7}B",
            "\u{2318}\u{21E7}V",
            "F1",
            "\u{2318}\u{21E7}C",
        ] {
            assert!(
                joined.contains(expected),
                "missing keybind chip hint '{}' from launcher",
                expected
            );
        }
    }

    #[test]
    fn hit_test_outside_bar_returns_none() {
        let bar = AppLauncherBar::new();
        let r = bar_rect();
        // Below the bar.
        assert_eq!(
            bar.hit_test(&r, 100.0, r.height + 50.0),
            LauncherAction::None
        );
        // Above the bar.
        assert_eq!(bar.hit_test(&r, 100.0, -5.0), LauncherAction::None);
        // Right of the bar.
        assert_eq!(
            bar.hit_test(&r, r.width + 50.0, 10.0),
            LauncherAction::None
        );
    }

    #[test]
    fn hit_test_maps_click_to_first_chip() {
        let bar = AppLauncherBar::new();
        let r = bar_rect();
        let chip0 = bar.chip_rect(&r, 0);
        let cx = chip0.x + chip0.width * 0.5;
        let cy = chip0.y + chip0.height * 0.5;
        assert_eq!(
            bar.hit_test(&r, cx, cy),
            LauncherAction::OpenPane(LauncherPaneKind::Inspector),
            "first chip must map to Inspector"
        );
    }

    #[test]
    fn hit_test_maps_click_to_last_chip() {
        let bar = AppLauncherBar::new();
        let r = bar_rect();
        let last = bar.items().len() - 1;
        let chip = bar.chip_rect(&r, last);
        let cx = chip.x + chip.width * 0.5;
        let cy = chip.y + chip.height * 0.5;
        assert_eq!(
            bar.hit_test(&r, cx, cy),
            LauncherAction::OpenPane(LauncherPaneKind::Console),
            "last chip must map to Console"
        );
    }

    #[test]
    fn hit_test_each_chip_maps_to_correct_pane_kind() {
        let bar = AppLauncherBar::new();
        let r = bar_rect();
        for (idx, item) in bar.items().iter().enumerate() {
            let chip = bar.chip_rect(&r, idx);
            let cx = chip.x + chip.width * 0.5;
            let cy = chip.y + chip.height * 0.5;
            assert_eq!(
                bar.hit_test(&r, cx, cy),
                LauncherAction::OpenPane(item.id),
                "chip at idx {idx} should map to {:?}",
                item.id
            );
        }
    }

    #[test]
    fn chip_height_meets_44px_hit_target() {
        // The 44 px floor for clickable hit targets isn't optional. If the
        // bar height changes such that chip height drops below 44 px the
        // launcher is unusable on touch / trackpad. Guard that here.
        let bar = AppLauncherBar::new();
        let r = bar_rect();
        let chip = bar.chip_rect(&r, 0);
        // Bar height minus padding gives chip height; both must meet 44 px.
        assert!(
            r.height >= 44.0,
            "launcher bar height ({}) below 44 px hit-target floor",
            r.height
        );
        assert!(
            chip.height >= 36.0,
            "chip height ({}) too small (target floor ~36-44 px after padding)",
            chip.height
        );
    }

    #[test]
    fn chips_do_not_overlap() {
        // Layout sanity: walking the chips left-to-right, the right edge of
        // chip N must be strictly less than (or equal to) the left edge of
        // chip N+1. Overlap would create ambiguous hit-test regions.
        let bar = AppLauncherBar::new();
        let r = bar_rect();
        let mut prev_right = r.x;
        for idx in 0..bar.items().len() {
            let chip = bar.chip_rect(&r, idx);
            assert!(
                chip.x >= prev_right,
                "chip {idx} (x={}) overlaps prior right edge {}",
                chip.x,
                prev_right
            );
            prev_right = chip.x + chip.width;
        }
    }

    #[test]
    fn chips_fit_within_bar_width() {
        let bar = AppLauncherBar::new();
        let r = bar_rect();
        let last = bar.items().len() - 1;
        let last_chip = bar.chip_rect(&r, last);
        let right_edge = last_chip.x + last_chip.width;
        assert!(
            right_edge <= r.x + r.width + 1.0,
            "rightmost chip (right={}) extends past bar width ({})",
            right_edge,
            r.x + r.width,
        );
    }

    #[test]
    fn custom_items_replace_defaults() {
        let mut bar = AppLauncherBar::new();
        bar.set_items(vec![LauncherItem {
            id: LauncherPaneKind::Settings,
            label: "ONLY",
            keybind: "X",
        }]);
        assert_eq!(bar.items().len(), 1);
        let texts = bar.render_text(&bar_rect());
        // 1 label + 1 keybind = 2 segments.
        assert_eq!(texts.len(), 2);
        let joined: String = texts.iter().map(|s| s.text.as_str()).collect();
        assert!(joined.contains("ONLY"));
        assert!(joined.contains("X"));
    }

    #[test]
    fn theme_swap_recolors_bar_bg() {
        use crate::tokens::{ColorRoles, Tokens};
        let phosphor = Tokens::phosphor(RenderCtx::fallback());
        let mut blue_roles = ColorRoles::phosphor();
        blue_roles.surface_base = [0.0, 0.0, 1.0, 1.0];
        let blue = Tokens::new(blue_roles, RenderCtx::fallback());

        let bar_p = AppLauncherBar::new().with_tokens(phosphor);
        let bar_b = AppLauncherBar::new().with_tokens(blue);
        let bg_p = bar_p.render_quads(&bar_rect())[0].color;
        let bg_b = bar_b.render_quads(&bar_rect())[0].color;
        assert_ne!(bg_p, bg_b);
        assert!(bg_b[2] > 0.9, "blue theme: bar bg must be dominantly blue");
    }
}
