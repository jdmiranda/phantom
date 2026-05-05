//! Widget system for Phantom's UI chrome.
//!
//! Widgets produce rendering primitives — quads and text segments — that the
//! renderer composites into the final frame. The layout engine provides each
//! widget with a [`Rect`] describing its available screen region; the widget
//! fills that region with visual content.
//!
//! Two concrete widgets ship with this module:
//! - [`TabBar`] — horizontal tab strip with active-tab highlighting.
//! - [`StatusBar`] — bottom bar showing cwd, git branch, and clock.

use crate::layout::Rect;
use crate::themes::UiColors;
use phantom_renderer::quads::QuadInstance;

// Sec.8 — top-of-screen banner that surfaces capability-denial patterns.
// Re-exported so the App can reference `phantom_ui::widgets::Notification
// Banner` alongside `StatusBar` / `TabBar`.
pub mod notification_banner;
pub use notification_banner::{BannerSeverity, NOTIFICATION_BANNER_HEIGHT, NotificationBanner};

// Issue #21 — bottom-of-pane status strip (left / center / right slots).
pub mod status_strip;
pub use status_strip::{STATUS_STRIP_HEIGHT, StatusStrip};

// Issue #22 — chat-style message block for agent panes.
pub mod message_block;
pub use message_block::{AVATAR_GAP, AVATAR_W, MessageBlock, MessageRole};

// Issue #26 — thin separator line (horizontal / vertical).
pub mod divider;
pub use divider::{Divider, Orientation};

// Issue #20 — generic bordered panel with optional title bar.
pub mod panel;
pub use panel::{Panel, TITLE_BAR_HEIGHT};

// Issue #16 — 8px vertical scrollbar with token-driven colors.
pub mod scrollbar;
pub use scrollbar::{SCROLLBAR_WIDTH, ScrollState, ScrollbarAction, Scrollbar, track_y_to_offset};

// Issue #25 — horizontal tab strip with badge and keyboard nav.
pub mod tab_strip;
// Note: `Tab` is not re-exported here to avoid a name conflict with the
// legacy private `Tab` struct used by `TabBar` in this module. Import it
// directly: `use phantom_ui::widgets::tab_strip::Tab`.
pub use tab_strip::{TAB_MIN_W, TabStrip};

// Issue #23 — single-line text input with prompt, cursor, and history.
pub mod input_bar;
pub use input_bar::{INPUT_BAR_HEIGHT, InputBar, InputKey};

// Issue #30 — animated focus-ring overlay keyed to an AppId.
pub mod focus_ring;
pub use focus_ring::{FADE_DURATION_MS, FocusRing};

// Context menu widget — floating popup with keyboard navigation.
pub mod context_menu;
pub use context_menu::{ContextMenu, ContextMenuItem};

// Issue #27 — full-screen keybind help overlay (F1 / ?).
pub mod keybind_help;
pub use keybind_help::KeybindHelp;

// Find-in-terminal search bar (Cmd+F).
pub mod search_bar;
pub use search_bar::{SEARCH_BAR_HEIGHT, SearchBar, SearchBarAction, SearchKey};

// Context menu widget — floating popup with keyboard navigation.
pub mod context_menu;
pub use context_menu::{ContextMenu, ContextMenuItem};

// -----------------------------------------------------------------------
// Color palette
// -----------------------------------------------------------------------

/// Dark gray-blue status bar background.
const STATUS_BAR_BG: [f32; 4] = [0.08, 0.08, 0.12, 1.0];
/// Muted green status bar foreground.
const STATUS_BAR_FG: [f32; 4] = [0.6, 0.7, 0.6, 1.0];

/// Fallback tab bar background used when no theme is applied.
const DEFAULT_TAB_BAR_BG: [f32; 4] = [0.06, 0.06, 0.09, 1.0];
/// Fallback active tab background used when no theme is applied.
const DEFAULT_ACTIVE_TAB_BG: [f32; 4] = [0.12, 0.14, 0.18, 1.0];
/// Fallback inactive tab text used when no theme is applied.
const DEFAULT_INACTIVE_TAB_FG: [f32; 4] = [0.4, 0.4, 0.5, 1.0];
/// Fallback active tab text used when no theme is applied.
const DEFAULT_ACTIVE_TAB_FG: [f32; 4] = [0.8, 0.9, 0.8, 1.0];

/// Horizontal padding inside each tab button, in pixels.
const TAB_PADDING_H: f32 = 16.0;
/// Minimum width of a single tab button, in pixels.
const TAB_MIN_WIDTH: f32 = 80.0;
/// Assumed monospace character width for text layout (pixels).
/// This is a rough estimate used for positioning text segments;
/// the actual glyph renderer handles precise metrics.
const CHAR_WIDTH: f32 = 8.0;

// -----------------------------------------------------------------------
// TextSegment
// -----------------------------------------------------------------------

/// A positioned run of text that the renderer should draw.
///
/// Widgets emit these to describe their textual content. The renderer is
/// responsible for shaping and rasterizing the string at the given position.
#[derive(Clone, Debug, PartialEq)]
pub struct TextSegment {
    /// The text content to render.
    pub text: String,
    /// X position in pixels (left edge of the first glyph).
    pub x: f32,
    /// Y position in pixels (baseline or top, per renderer convention).
    pub y: f32,
    /// RGBA color for the text.
    pub color: [f32; 4],
}

// -----------------------------------------------------------------------
// Widget trait
// -----------------------------------------------------------------------

/// A renderable UI component that occupies a rectangular region.
///
/// Implementations produce two kinds of rendering primitives:
/// - **Quads**: colored rectangles for backgrounds, highlights, borders.
/// - **Text segments**: positioned strings with color information.
///
/// The caller provides a [`Rect`] from the layout engine that describes the
/// pixel region the widget should fill.
pub trait Widget {
    /// Produce background and decorative quads for this widget.
    fn render_quads(&self, rect: &Rect) -> Vec<QuadInstance>;

    /// Produce text segments for this widget.
    fn render_text(&self, rect: &Rect) -> Vec<TextSegment>;
}

// -----------------------------------------------------------------------
// StatusBar
// -----------------------------------------------------------------------

// -----------------------------------------------------------------------
// ConnectionIndicator
// -----------------------------------------------------------------------

/// Connection state of the AI backend, shown in the status bar.
///
/// This is a UI-facing mirror of the brain's `ConnectionState` enum.
/// It lives in `phantom-ui` to avoid a cross-crate dependency on
/// `phantom-brain` from the UI layer; the app layer maps between the two.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionIndicator {
    /// All backends reachable — no indicator shown.
    Connected,
    /// A named provider is degraded — shown in amber.
    Degraded { provider: String },
    /// Network offline — shown in red.
    Offline,
}

impl ConnectionIndicator {
    /// Short label displayed in the status bar (empty when connected).
    #[must_use] 
    pub fn label(&self) -> String {
        match self {
            Self::Connected => String::new(),
            Self::Degraded { provider } => format!("~ {provider}"),
            Self::Offline => "OFFLINE".to_owned(),
        }
    }

    /// RGBA color for the indicator text.
    #[must_use] 
    pub fn color(&self) -> [f32; 4] {
        match self {
            Self::Connected => [0.6, 0.7, 0.6, 1.0],
            Self::Degraded { .. } => [0.9, 0.7, 0.2, 1.0],
            Self::Offline => [0.9, 0.3, 0.3, 1.0],
        }
    }
}

/// Bottom status bar showing working directory, git branch, and time.
///
/// The bar is divided into three regions:
/// - **Left**: current working directory, truncated with a leading `...` if
///   the path is too long for the available space.
/// - **Center**: git branch name prefixed with a branch icon ( ).
/// - **Right**: `[OFFLINE]`/`[P]` flag chips (when set), connection indicator
///   (when AI backend is degraded or offline), then wall clock in `HH:MM`.
#[derive(Clone, Debug)]
pub struct StatusBar {
    cwd: String,
    branch: String,
    time: String,
    activity: Option<String>,
    connection: Option<ConnectionIndicator>,
    offline_mode: bool,
    privacy_mode: bool,
}

impl StatusBar {
    /// Create a status bar with sensible defaults.
    #[must_use] 
    pub fn new() -> Self {
        Self {
            cwd: String::from("~"),
            branch: String::from("main"),
            time: String::from("00:00"),
            activity: None,
            connection: None,
            offline_mode: false,
            privacy_mode: false,
        }
    }

    /// Enable or disable the privacy mode lock indicator.
    pub fn set_privacy_mode(&mut self, enabled: bool) {
        self.privacy_mode = enabled;
    }

    /// Whether the privacy mode indicator is currently shown.
    #[must_use] 
    pub fn privacy_mode(&self) -> bool {
        self.privacy_mode
    }

    /// Update the displayed working directory.
    pub fn set_cwd(&mut self, path: &str) {
        self.cwd = path.to_owned();
    }

    /// Update the displayed git branch name.
    pub fn set_branch(&mut self, branch: &str) {
        self.branch = branch.to_owned();
    }

    /// Update the displayed clock string.
    pub fn set_time(&mut self, time: &str) {
        self.time = time.to_owned();
    }

    /// Set an activity message (e.g. session welcome-back).
    /// Cleared automatically after the first render.
    pub fn set_activity(&mut self, msg: &str) {
        self.activity = Some(msg.to_owned());
    }

    /// Take and clear the current activity message.
    pub fn take_activity(&mut self) -> Option<String> {
        self.activity.take()
    }

    /// Update the AI backend connection indicator.
    ///
    /// Pass `None` to clear the indicator (equivalent to `Connected`).
    pub fn set_connection(&mut self, indicator: Option<ConnectionIndicator>) {
        self.connection = indicator;
    }

    /// The current connection indicator, if any.
    #[must_use] 
    pub fn connection(&self) -> Option<&ConnectionIndicator> {
        self.connection.as_ref()
    }

    /// Set offline mode indicator.
    pub fn set_offline_mode(&mut self, enabled: bool) {
        self.offline_mode = enabled;
    }

    /// Truncate `text` so it fits within `max_chars`, prepending `...` if needed.
    fn truncate_path(text: &str, max_chars: usize) -> String {
        if max_chars < 4 {
            return String::new();
        }
        let chars: Vec<char> = text.chars().collect();
        if chars.len() <= max_chars {
            text.to_owned()
        } else {
            let keep = max_chars.saturating_sub(3);
            let start = chars.len() - keep;
            format!("...{}", chars[start..].iter().collect::<String>())
        }
    }
}

impl Default for StatusBar {
    fn default() -> Self {
        Self::new()
    }
}

impl Widget for StatusBar {
    fn render_quads(&self, rect: &Rect) -> Vec<QuadInstance> {
        vec![QuadInstance {
            pos: [rect.x, rect.y],
            size: [rect.width, rect.height],
            color: STATUS_BAR_BG,
            border_radius: 0.0,
        }]
    }

    fn render_text(&self, rect: &Rect) -> Vec<TextSegment> {
        let mut segments = Vec::with_capacity(5);
        // Padding scales with screen width so content survives CRT barrel
        // distortion at edges. ~1.5% of width handles curvature up to ~0.06.
        let padding = (rect.width * 0.015).max(8.0);
        let text_y = rect.y + (rect.height * 0.5) - 1.0;

        // -- Right: [OFFLINE]/[P] flag chips + time (anchored to right edge
        // with CRT margin). Chips are prepended so they render alongside the
        // clock as a single STATUS_BAR_FG segment.
        let mut right_text = String::new();
        if self.offline_mode {
            right_text.push_str("[OFFLINE] ");
        }
        if self.privacy_mode {
            right_text.push_str("[P] ");
        }
        right_text.push_str(&self.time);

        let right_width = right_text.len() as f32 * CHAR_WIDTH;
        let right_x = rect.x + rect.width - right_width - padding;

        segments.push(TextSegment {
            text: right_text,
            x: right_x,
            y: text_y,
            color: STATUS_BAR_FG,
        });

        // -- Right-of-center: connection indicator (shown when not Connected),
        // sitting just to the left of the time/chip group so the warning color
        // is visually grouped with the clock.
        if let Some(indicator) = &self.connection {
            let label = indicator.label();
            if !label.is_empty() {
                let indicator_width = label.len() as f32 * CHAR_WIDTH;
                let gap = 8.0;
                let indicator_x = right_x - indicator_width - gap;
                segments.push(TextSegment {
                    text: label,
                    x: indicator_x,
                    y: text_y,
                    color: indicator.color(),
                });
            }
        }

        // -- Center: branch with icon --
        let branch_text = format!("\u{E0A0} {}", self.branch);
        let branch_width = branch_text.len() as f32 * CHAR_WIDTH;
        let branch_x = rect.x + (rect.width - branch_width) * 0.5;

        segments.push(TextSegment {
            text: branch_text,
            x: branch_x,
            y: text_y,
            color: STATUS_BAR_FG,
        });

        // -- Left: cwd, truncated to fit --
        // Available space: from left padding to branch region (with some gap).
        let available_px = (branch_x - rect.x - padding * 2.0).max(0.0);
        let max_chars = (available_px / CHAR_WIDTH) as usize;
        let cwd_display = Self::truncate_path(&self.cwd, max_chars);

        if !cwd_display.is_empty() {
            segments.push(TextSegment {
                text: cwd_display,
                x: rect.x + padding,
                y: text_y,
                color: STATUS_BAR_FG,
            });
        }

        segments
    }
}

// -----------------------------------------------------------------------
// TabBar
// -----------------------------------------------------------------------

/// A single tab within the [`TabBar`].
#[derive(Clone, Debug)]
struct Tab {
    title: String,
}

/// Horizontal tab strip rendered at the top of the window.
///
/// Each tab is drawn as a rectangular button. The active tab receives a
/// lighter background and brighter text; inactive tabs are dimmed.
///
/// Colors default to a neutral dark palette. Call [`TabBar::apply_ui_colors`]
/// with a theme's [`UiColors`] to make the tab bar follow the active theme.
#[derive(Clone, Debug)]
pub struct TabBar {
    tabs: Vec<Tab>,
    active: usize,
    /// Background color of the full tab bar strip.
    bar_bg: [f32; 4],
    /// Background color applied to the active tab button.
    active_tab_bg: [f32; 4],
    /// Text color for inactive (non-selected) tab labels.
    inactive_tab_fg: [f32; 4],
    /// Text color for the active (selected) tab label.
    active_tab_fg: [f32; 4],
}

impl TabBar {
    /// Create an empty tab bar with no tabs.
    ///
    /// Colors default to a neutral dark palette. Call [`TabBar::apply_ui_colors`]
    /// after construction to apply the active theme.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tabs: Vec::new(),
            active: 0,
            bar_bg: DEFAULT_TAB_BAR_BG,
            active_tab_bg: DEFAULT_ACTIVE_TAB_BG,
            inactive_tab_fg: DEFAULT_INACTIVE_TAB_FG,
            active_tab_fg: DEFAULT_ACTIVE_TAB_FG,
        }
    }

    /// Apply theme colors from a [`UiColors`] token set.
    ///
    /// Call this whenever the active theme changes so the tab bar reflects the
    /// new palette. Replaces all four color fields derived from the theme.
    pub fn apply_ui_colors(&mut self, ui: &UiColors) {
        self.bar_bg = ui.tab_bar_bg;
        self.active_tab_bg = ui.tab_active_bg;
        self.inactive_tab_fg = ui.tab_bar_fg;
        self.active_tab_fg = ui.tab_active_fg;
    }

    /// Current bar background color (reflects active theme after
    /// [`apply_ui_colors`](Self::apply_ui_colors) is called).
    #[must_use]
    pub fn bar_bg(&self) -> [f32; 4] {
        self.bar_bg
    }

    /// Current active-tab background color.
    #[must_use]
    pub fn active_tab_bg(&self) -> [f32; 4] {
        self.active_tab_bg
    }

    /// Current inactive-tab foreground color.
    #[must_use]
    pub fn inactive_tab_fg(&self) -> [f32; 4] {
        self.inactive_tab_fg
    }

    /// Current active-tab foreground color.
    #[must_use]
    pub fn active_tab_fg(&self) -> [f32; 4] {
        self.active_tab_fg
    }

    /// Add a tab with the given title. Returns the index of the new tab.
    pub fn add_tab(&mut self, title: &str) -> usize {
        let idx = self.tabs.len();
        self.tabs.push(Tab {
            title: title.to_owned(),
        });
        // If this is the first tab, make it active.
        if self.tabs.len() == 1 {
            self.active = 0;
        }
        idx
    }

    /// Remove the tab at `index`.
    ///
    /// # Panics
    ///
    /// Panics if `index` is out of bounds.
    pub fn remove_tab(&mut self, index: usize) {
        self.tabs.remove(index);
        if self.tabs.is_empty() {
            self.active = 0;
        } else if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        }
    }

    /// Set the active tab by index.
    ///
    /// # Panics
    ///
    /// Panics if `index >= tab count`.
    pub fn set_active(&mut self, index: usize) {
        assert!(
            index < self.tabs.len(),
            "set_active index {index} out of bounds ({})",
            self.tabs.len()
        );
        self.active = index;
    }

    /// Return the index of the currently active tab.
    #[must_use] 
    pub fn active(&self) -> usize {
        self.active
    }

    /// Return the number of tabs.
    #[must_use] 
    pub fn tab_count(&self) -> usize {
        self.tabs.len()
    }

    /// Compute the pixel width of each tab given the total bar width.
    fn tab_width(&self, bar_width: f32) -> f32 {
        if self.tabs.is_empty() {
            return 0.0;
        }
        let natural = bar_width / self.tabs.len() as f32;
        natural.max(TAB_MIN_WIDTH)
    }
}

impl Default for TabBar {
    fn default() -> Self {
        Self::new()
    }
}

impl Widget for TabBar {
    fn render_quads(&self, rect: &Rect) -> Vec<QuadInstance> {
        // One background quad for the full bar, plus one per tab.
        let mut quads = Vec::with_capacity(1 + self.tabs.len());

        // Full bar background — reads from theme-derived field.
        quads.push(QuadInstance {
            pos: [rect.x, rect.y],
            size: [rect.width, rect.height],
            color: self.bar_bg,
            border_radius: 0.0,
        });

        if self.tabs.is_empty() {
            return quads;
        }

        let tab_w = self.tab_width(rect.width);

        for (i, _tab) in self.tabs.iter().enumerate() {
            let tab_x = rect.x + i as f32 * tab_w;
            let is_active = i == self.active;

            if is_active {
                quads.push(QuadInstance {
                    pos: [tab_x, rect.y],
                    size: [tab_w, rect.height],
                    color: self.active_tab_bg,
                    border_radius: 0.0,
                });
            }
        }

        quads
    }

    fn render_text(&self, rect: &Rect) -> Vec<TextSegment> {
        if self.tabs.is_empty() {
            return Vec::new();
        }

        let tab_w = self.tab_width(rect.width);
        let text_y = rect.y + (rect.height * 0.5) - 1.0;

        let mut segments = Vec::with_capacity(self.tabs.len());

        for (i, tab) in self.tabs.iter().enumerate() {
            let tab_x = rect.x + i as f32 * tab_w;
            let is_active = i == self.active;

            // Truncate title to fit within the tab button (minus padding).
            let max_chars = ((tab_w - TAB_PADDING_H * 2.0) / CHAR_WIDTH).max(0.0) as usize;
            let title: String = tab.title.chars().take(max_chars).collect();

            if title.is_empty() {
                continue;
            }

            // Center the title text within the tab button.
            let title_width = title.len() as f32 * CHAR_WIDTH;
            let text_x = tab_x + (tab_w - title_width) * 0.5;

            // Colors read from theme-derived fields rather than hardcoded constants.
            let color = if is_active {
                self.active_tab_fg
            } else {
                self.inactive_tab_fg
            };

            segments.push(TextSegment {
                text: title,
                x: text_x,
                y: text_y,
                color,
            });
        }

        segments
    }
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn bar_rect() -> Rect {
        Rect {
            x: 0.0,
            y: 0.0,
            width: 1920.0,
            height: 30.0,
        }
    }

    fn status_rect() -> Rect {
        Rect {
            x: 0.0,
            y: 1056.0,
            width: 1920.0,
            height: 24.0,
        }
    }

    // -- TabBar tests --

    #[test]
    fn tab_bar_empty_renders_background_only() {
        let bar = TabBar::new();
        let quads = bar.render_quads(&bar_rect());
        assert_eq!(
            quads.len(),
            1,
            "empty tab bar should have one background quad"
        );
        assert_eq!(quads[0].color, bar.bar_bg());

        let texts = bar.render_text(&bar_rect());
        assert!(texts.is_empty());
    }

    #[test]
    fn tab_bar_add_returns_sequential_indices() {
        let mut bar = TabBar::new();
        assert_eq!(bar.add_tab("Alpha"), 0);
        assert_eq!(bar.add_tab("Beta"), 1);
        assert_eq!(bar.add_tab("Gamma"), 2);
        assert_eq!(bar.tab_count(), 3);
    }

    #[test]
    fn tab_bar_active_defaults_to_first() {
        let mut bar = TabBar::new();
        bar.add_tab("First");
        bar.add_tab("Second");
        assert_eq!(bar.active(), 0);
    }

    #[test]
    fn tab_bar_set_active() {
        let mut bar = TabBar::new();
        bar.add_tab("A");
        bar.add_tab("B");
        bar.set_active(1);
        assert_eq!(bar.active(), 1);
    }

    #[test]
    #[should_panic(expected = "out of bounds")]
    fn tab_bar_set_active_out_of_bounds_panics() {
        let mut bar = TabBar::new();
        bar.add_tab("Only");
        bar.set_active(5);
    }

    #[test]
    fn tab_bar_remove_adjusts_active() {
        let mut bar = TabBar::new();
        bar.add_tab("A");
        bar.add_tab("B");
        bar.add_tab("C");
        bar.set_active(2);
        bar.remove_tab(2);
        assert_eq!(bar.active(), 1, "active should clamp to last tab");
    }

    #[test]
    fn tab_bar_remove_all() {
        let mut bar = TabBar::new();
        bar.add_tab("A");
        bar.remove_tab(0);
        assert_eq!(bar.tab_count(), 0);
        assert_eq!(bar.active(), 0);
    }

    #[test]
    fn tab_bar_renders_active_highlight() {
        let mut bar = TabBar::new();
        bar.add_tab("A");
        bar.add_tab("B");
        bar.set_active(1);

        let quads = bar.render_quads(&bar_rect());
        // 1 background + 1 active tab highlight
        assert_eq!(quads.len(), 2);
        assert_eq!(quads[1].color, bar.active_tab_bg());
    }

    #[test]
    fn tab_bar_text_colors_match_active_state() {
        let mut bar = TabBar::new();
        bar.add_tab("A");
        bar.add_tab("B");
        bar.set_active(0);

        let texts = bar.render_text(&bar_rect());
        assert_eq!(texts.len(), 2);
        assert_eq!(
            texts[0].color,
            bar.active_tab_fg(),
            "active tab should use active fg"
        );
        assert_eq!(
            texts[1].color,
            bar.inactive_tab_fg(),
            "inactive tab should use inactive fg"
        );
    }

    // -- TabBar theme-wiring tests --

    #[test]
    fn tab_bar_uses_theme_bg_color() {
        use crate::themes;
        let theme = themes::amber();
        let mut bar = TabBar::new();
        bar.apply_ui_colors(&theme.ui_colors);

        let quads = bar.render_quads(&bar_rect());
        assert_eq!(
            quads[0].color,
            theme.ui_colors.tab_bar_bg,
            "bar background must match theme.ui_colors.tab_bar_bg after apply_ui_colors"
        );
    }

    #[test]
    fn tab_bar_active_tab_color_matches_theme() {
        use crate::themes;
        let theme = themes::ice();
        let mut bar = TabBar::new();
        bar.add_tab("X");
        bar.add_tab("Y");
        bar.set_active(0);
        bar.apply_ui_colors(&theme.ui_colors);

        let quads = bar.render_quads(&bar_rect());
        // quads[0] = bar bg, quads[1] = active tab highlight
        assert_eq!(quads.len(), 2);
        assert_eq!(
            quads[1].color,
            theme.ui_colors.tab_active_bg,
            "active tab quad must match theme.ui_colors.tab_active_bg"
        );

        let texts = bar.render_text(&bar_rect());
        assert_eq!(texts.len(), 2);
        assert_eq!(
            texts[0].color,
            theme.ui_colors.tab_active_fg,
            "active tab text must match theme.ui_colors.tab_active_fg"
        );
        assert_eq!(
            texts[1].color,
            theme.ui_colors.tab_bar_fg,
            "inactive tab text must match theme.ui_colors.tab_bar_fg"
        );
    }

    #[test]
    fn theme_switch_updates_tab_bar_colors() {
        use crate::themes;
        let mut bar = TabBar::new();
        bar.add_tab("A");

        let phosphor = themes::phosphor();
        bar.apply_ui_colors(&phosphor.ui_colors);
        let phosphor_bg = bar.render_quads(&bar_rect())[0].color;

        let blood = themes::blood();
        bar.apply_ui_colors(&blood.ui_colors);
        let blood_bg = bar.render_quads(&bar_rect())[0].color;

        assert_ne!(
            phosphor_bg, blood_bg,
            "switching themes must change the tab bar background color"
        );
    }

    // -- StatusBar tests --

    #[test]
    fn status_bar_renders_background_quad() {
        let bar = StatusBar::new();
        let quads = bar.render_quads(&status_rect());
        assert_eq!(quads.len(), 1);
        assert_eq!(quads[0].color, STATUS_BAR_BG);
        assert_eq!(quads[0].size[0], 1920.0);
    }

    #[test]
    fn status_bar_renders_three_text_segments() {
        let mut bar = StatusBar::new();
        bar.set_cwd("/home/user/project");
        bar.set_branch("develop");
        bar.set_time("14:30");

        let texts = bar.render_text(&status_rect());
        // time + branch + cwd = 3
        assert_eq!(texts.len(), 3);

        // Verify time segment content (should be the right-most segment).
        let time_seg = texts
            .iter()
            .find(|s| s.text.contains("14:30"))
            .expect("should contain time");
        assert_eq!(time_seg.color, STATUS_BAR_FG);

        // Verify branch segment contains the branch icon and name.
        let branch_seg = texts
            .iter()
            .find(|s| s.text.contains("develop"))
            .expect("should contain branch");
        assert!(
            branch_seg.text.contains('\u{E0A0}'),
            "branch should have  icon"
        );

        // Verify cwd segment.
        let cwd_seg = texts
            .iter()
            .find(|s| s.text.contains("project"))
            .expect("should contain cwd");
        assert_eq!(cwd_seg.color, STATUS_BAR_FG);
    }

    #[test]
    fn status_bar_defaults() {
        let bar = StatusBar::new();
        let texts = bar.render_text(&status_rect());
        assert!(texts.iter().any(|s| s.text.contains("main")));
        assert!(texts.iter().any(|s| s.text.contains("00:00")));
    }

    #[test]
    fn status_bar_shows_offline_indicator() {
        let mut bar = StatusBar::new();
        bar.set_offline_mode(true);
        bar.set_time("14:30");

        let texts = bar.render_text(&status_rect());
        let right_seg = texts
            .iter()
            .find(|s| s.text.contains("14:30"))
            .expect("should contain time");
        assert!(right_seg.text.contains("[OFFLINE]"));
    }

    #[test]
    fn status_bar_shows_privacy_indicator() {
        let mut bar = StatusBar::new();
        bar.set_privacy_mode(true);
        bar.set_time("14:30");

        let texts = bar.render_text(&status_rect());
        let right_seg = texts
            .iter()
            .find(|s| s.text.contains("14:30"))
            .expect("should contain time");
        assert!(right_seg.text.contains("[P]"));
    }

    #[test]
    fn status_bar_shows_both_indicators() {
        let mut bar = StatusBar::new();
        bar.set_offline_mode(true);
        bar.set_privacy_mode(true);
        bar.set_time("14:30");

        let texts = bar.render_text(&status_rect());
        let right_seg = texts
            .iter()
            .find(|s| s.text.contains("14:30"))
            .expect("should contain time");
        assert!(right_seg.text.contains("[OFFLINE]"));
        assert!(right_seg.text.contains("[P]"));
    }

    #[test]
    fn status_bar_truncates_long_cwd() {
        let truncated = StatusBar::truncate_path("/very/long/path/to/some/deep/directory", 15);
        assert!(truncated.starts_with("..."));
        assert!(truncated.len() <= 15);
    }

    #[test]
    fn status_bar_short_cwd_not_truncated() {
        let result = StatusBar::truncate_path("~/code", 20);
        assert_eq!(result, "~/code");
    }

    #[test]
    fn status_bar_truncate_tiny_budget() {
        let result = StatusBar::truncate_path("/anything", 3);
        assert!(result.is_empty(), "should return empty for budget < 4");
    }

    // -- TextSegment tests --

    #[test]
    fn text_segment_clone_eq() {
        let a = TextSegment {
            text: "hello".into(),
            x: 10.0,
            y: 20.0,
            color: [1.0, 1.0, 1.0, 1.0],
        };
        let b = a.clone();
        assert_eq!(a, b);
    }

    // -- Widget trait object safety --

    #[test]
    fn widget_is_object_safe() {
        let bar = StatusBar::new();
        let widget: &dyn Widget = &bar;
        let quads = widget.render_quads(&status_rect());
        assert!(!quads.is_empty());
    }
}
