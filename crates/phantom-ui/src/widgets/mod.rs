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
use phantom_renderer::quads::QuadInstance;

// -----------------------------------------------------------------------
// Color palette
// -----------------------------------------------------------------------

/// Dark gray-blue status bar background.
const STATUS_BAR_BG: [f32; 4] = [0.08, 0.08, 0.12, 1.0];
/// Muted green status bar foreground.
const STATUS_BAR_FG: [f32; 4] = [0.6, 0.7, 0.6, 1.0];

/// Near-black tab bar background.
const TAB_BAR_BG: [f32; 4] = [0.06, 0.06, 0.09, 1.0];
/// Slightly lighter active tab background.
const ACTIVE_TAB_BG: [f32; 4] = [0.12, 0.14, 0.18, 1.0];
/// Dim inactive tab text.
const INACTIVE_TAB_FG: [f32; 4] = [0.4, 0.4, 0.5, 1.0];
/// Bright active tab text.
const ACTIVE_TAB_FG: [f32; 4] = [0.8, 0.9, 0.8, 1.0];

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

/// Bottom status bar showing working directory, git branch, and time.
///
/// The bar is divided into three regions:
/// - **Left**: current working directory, truncated with a leading `...` if
///   the path is too long for the available space.
/// - **Center**: git branch name prefixed with a branch icon ( ).
/// - **Right**: wall clock in `HH:MM` format.
#[derive(Clone, Debug)]
pub struct StatusBar {
    cwd: String,
    branch: String,
    time: String,
    activity: Option<String>,
}

impl StatusBar {
    /// Create a status bar with sensible defaults.
    pub fn new() -> Self {
        Self {
            cwd: String::from("~"),
            branch: String::from("main"),
            time: String::from("00:00"),
            activity: None,
        }
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
        let mut segments = Vec::with_capacity(3);
        let padding = 8.0_f32;
        let text_y = rect.y + (rect.height * 0.5) - 1.0;

        // -- Right: time (anchored to right edge) --
        let time_width = self.time.len() as f32 * CHAR_WIDTH;
        let time_x = rect.x + rect.width - time_width - padding;

        segments.push(TextSegment {
            text: self.time.clone(),
            x: time_x,
            y: text_y,
            color: STATUS_BAR_FG,
        });

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
#[derive(Clone, Debug)]
pub struct TabBar {
    tabs: Vec<Tab>,
    active: usize,
}

impl TabBar {
    /// Create an empty tab bar with no tabs.
    pub fn new() -> Self {
        Self {
            tabs: Vec::new(),
            active: 0,
        }
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
    pub fn active(&self) -> usize {
        self.active
    }

    /// Return the number of tabs.
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

        // Full bar background.
        quads.push(QuadInstance {
            pos: [rect.x, rect.y],
            size: [rect.width, rect.height],
            color: TAB_BAR_BG,
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
                    color: ACTIVE_TAB_BG,
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

            let color = if is_active {
                ACTIVE_TAB_FG
            } else {
                INACTIVE_TAB_FG
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
        assert_eq!(quads.len(), 1, "empty tab bar should have one background quad");
        assert_eq!(quads[0].color, TAB_BAR_BG);

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
        assert_eq!(quads[1].color, ACTIVE_TAB_BG);
    }

    #[test]
    fn tab_bar_text_colors_match_active_state() {
        let mut bar = TabBar::new();
        bar.add_tab("A");
        bar.add_tab("B");
        bar.set_active(0);

        let texts = bar.render_text(&bar_rect());
        assert_eq!(texts.len(), 2);
        assert_eq!(texts[0].color, ACTIVE_TAB_FG, "active tab should use active fg");
        assert_eq!(texts[1].color, INACTIVE_TAB_FG, "inactive tab should use inactive fg");
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

        // Verify time segment content.
        let time_seg = texts.iter().find(|s| s.text == "14:30").expect("should contain time");
        assert_eq!(time_seg.color, STATUS_BAR_FG);

        // Verify branch segment contains the branch icon and name.
        let branch_seg = texts
            .iter()
            .find(|s| s.text.contains("develop"))
            .expect("should contain branch");
        assert!(branch_seg.text.contains('\u{E0A0}'), "branch should have  icon");

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
        assert!(texts.iter().any(|s| s.text == "00:00"));
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
