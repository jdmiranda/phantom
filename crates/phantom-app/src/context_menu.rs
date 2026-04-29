//! Right-click context menu overlay.
//!
//! Provides a simple popup menu with hit-testing and hover tracking.
//! The menu is rendered in the overlay pass (post-CRT, crisp).

/// A right-click context menu overlay.
pub struct ContextMenu {
    /// Screen position of the top-left corner.
    pub x: f32,
    pub y: f32,
    /// Menu items.
    pub items: Vec<MenuItem>,
    /// Currently hovered item index (None = no hover).
    pub hovered: Option<usize>,
    /// Whether the menu is visible.
    pub visible: bool,
}

pub struct MenuItem {
    pub label: String,
    pub action: MenuAction,
    #[allow(dead_code)]
    pub enabled: bool,
}

#[derive(Clone)]
#[allow(dead_code)]
pub enum MenuAction {
    Copy,
    Paste,
    SelectAll,
    SplitHorizontal,
    SplitVertical,
    Close,
    Fullscreen,
}

const ITEM_HEIGHT: f32 = 28.0;
const PADDING: f32 = 8.0;
const MENU_WIDTH: f32 = 180.0;

impl ContextMenu {
    pub fn new() -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            items: Vec::new(),
            hovered: None,
            visible: false,
        }
    }

    pub fn show(&mut self, x: f32, y: f32, items: Vec<MenuItem>) {
        self.x = x;
        self.y = y;
        self.items = items;
        self.hovered = None;
        self.visible = true;
    }

    pub fn hide(&mut self) {
        self.visible = false;
        self.items.clear();
        self.hovered = None;
    }

    /// Returns the `MenuItem` index at the given pixel position, or `None`.
    pub fn hit_test(&self, mx: f32, my: f32) -> Option<usize> {
        if !self.visible {
            return None;
        }

        if mx < self.x || mx > self.x + MENU_WIDTH {
            return None;
        }

        let rel_y = my - self.y - PADDING;
        if rel_y < 0.0 {
            return None;
        }

        let idx = (rel_y / ITEM_HEIGHT) as usize;
        if idx < self.items.len() {
            Some(idx)
        } else {
            None
        }
    }

    pub fn update_hover(&mut self, mx: f32, my: f32) {
        self.hovered = self.hit_test(mx, my);
    }

    /// Total height of the menu for rendering.
    pub fn height(&self) -> f32 {
        PADDING * 2.0 + self.items.len() as f32 * ITEM_HEIGHT
    }

    /// Menu width constant exposed for rendering.
    pub fn width(&self) -> f32 {
        MENU_WIDTH
    }

    /// Item height constant exposed for rendering.
    pub fn item_height(&self) -> f32 {
        ITEM_HEIGHT
    }

    /// Padding constant exposed for rendering.
    pub fn padding(&self) -> f32 {
        PADDING
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_menu_is_hidden() {
        let menu = ContextMenu::new();
        assert!(!menu.visible);
        assert!(menu.items.is_empty());
        assert!(menu.hovered.is_none());
    }

    #[test]
    fn show_makes_visible() {
        let mut menu = ContextMenu::new();
        let items = vec![MenuItem {
            label: "Copy".into(),
            action: MenuAction::Copy,
            enabled: true,
        }];
        menu.show(100.0, 200.0, items);
        assert!(menu.visible);
        assert_eq!(menu.x, 100.0);
        assert_eq!(menu.y, 200.0);
        assert_eq!(menu.items.len(), 1);
    }

    #[test]
    fn hide_clears_state() {
        let mut menu = ContextMenu::new();
        let items = vec![MenuItem {
            label: "Copy".into(),
            action: MenuAction::Copy,
            enabled: true,
        }];
        menu.show(100.0, 200.0, items);
        menu.hide();
        assert!(!menu.visible);
        assert!(menu.items.is_empty());
    }

    #[test]
    fn hit_test_when_hidden() {
        let menu = ContextMenu::new();
        assert_eq!(menu.hit_test(100.0, 200.0), None);
    }

    #[test]
    fn hit_test_inside_first_item() {
        let mut menu = ContextMenu::new();
        let items = vec![
            MenuItem {
                label: "Copy".into(),
                action: MenuAction::Copy,
                enabled: true,
            },
            MenuItem {
                label: "Paste".into(),
                action: MenuAction::Paste,
                enabled: true,
            },
        ];
        menu.show(100.0, 200.0, items);

        // Click at y = 200 + 8 (padding) + 5 = 213 -> first item
        assert_eq!(menu.hit_test(150.0, 213.0), Some(0));
    }

    #[test]
    fn hit_test_inside_second_item() {
        let mut menu = ContextMenu::new();
        let items = vec![
            MenuItem {
                label: "Copy".into(),
                action: MenuAction::Copy,
                enabled: true,
            },
            MenuItem {
                label: "Paste".into(),
                action: MenuAction::Paste,
                enabled: true,
            },
        ];
        menu.show(100.0, 200.0, items);

        // Click at y = 200 + 8 + 28 + 5 = 241 -> second item
        assert_eq!(menu.hit_test(150.0, 241.0), Some(1));
    }

    #[test]
    fn hit_test_outside_x() {
        let mut menu = ContextMenu::new();
        let items = vec![MenuItem {
            label: "Copy".into(),
            action: MenuAction::Copy,
            enabled: true,
        }];
        menu.show(100.0, 200.0, items);

        // Click outside x range (left of menu)
        assert_eq!(menu.hit_test(50.0, 213.0), None);
        // Click outside x range (right of menu)
        assert_eq!(menu.hit_test(300.0, 213.0), None);
    }

    #[test]
    fn hit_test_below_items() {
        let mut menu = ContextMenu::new();
        let items = vec![MenuItem {
            label: "Copy".into(),
            action: MenuAction::Copy,
            enabled: true,
        }];
        menu.show(100.0, 200.0, items);

        // Click below items: y = 200 + 8 + 28 + 10 = 246
        assert_eq!(menu.hit_test(150.0, 246.0), None);
    }

    #[test]
    fn update_hover_tracks_position() {
        let mut menu = ContextMenu::new();
        let items = vec![
            MenuItem {
                label: "Copy".into(),
                action: MenuAction::Copy,
                enabled: true,
            },
            MenuItem {
                label: "Paste".into(),
                action: MenuAction::Paste,
                enabled: true,
            },
        ];
        menu.show(100.0, 200.0, items);

        menu.update_hover(150.0, 213.0);
        assert_eq!(menu.hovered, Some(0));

        menu.update_hover(150.0, 241.0);
        assert_eq!(menu.hovered, Some(1));

        menu.update_hover(50.0, 213.0);
        assert_eq!(menu.hovered, None);
    }
}
