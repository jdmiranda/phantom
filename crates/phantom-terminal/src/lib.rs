pub mod alt_screen;
pub mod process;
pub mod terminal;
pub mod input;
pub mod output;
pub mod kitty;

/// Re-exported alacritty types needed by the adapter layer for selection.
pub mod selection {
    pub use alacritty_terminal::index::{Column, Line, Point, Side};
    pub use alacritty_terminal::selection::SelectionType;
}
