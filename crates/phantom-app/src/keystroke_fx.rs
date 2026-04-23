//! Per-keystroke glitch effect.
//!
//! Reuses the boot sequence's "logo reveal" aesthetic: when a character is
//! typed, the cell at the cursor briefly cycles through random glitch
//! characters before locking in to the real character. The effect is purely
//! visual — terminal state is never modified.

use std::time::Instant;

use phantom_renderer::grid::GridCell;

/// Glitch characters — same block elements used in the boot sequence.
const GLITCH_CHARS: &[char] = &[
    '\u{2591}', '\u{2592}', '\u{2593}', '\u{2588}', '\u{2580}', '\u{2584}',
    '\u{258C}', '\u{2590}', '\u{2502}', '\u{2524}', '\u{2561}', '\u{2562}',
    '\u{2556}', '\u{2555}', '\u{2563}', '\u{2551}', '\u{2557}', '\u{255D}',
    '\u{255C}', '\u{255B}', '\u{2510}', '\u{2514}', '\u{2534}', '\u{252C}',
    '\u{251C}', '\u{2500}', '\u{253C}',
];

/// Duration of the glitch animation per cell, in seconds.
const GLITCH_DURATION_SECS: f32 = 0.12;

/// Fraction of the duration where glitch chars are shown (rest is lock-in).
const GLITCH_PHASE: f32 = 0.75;

/// A single cell that is currently glitching.
struct GlitchCell {
    col: usize,
    row: usize,
    started_at: Instant,
}

/// Tracks per-keystroke glitch animations.
pub struct KeystrokeFx {
    cells: Vec<GlitchCell>,
    frame_counter: u32,
}

impl KeystrokeFx {
    pub fn new() -> Self {
        Self {
            cells: Vec::with_capacity(16),
            frame_counter: 0,
        }
    }

    /// Record a keystroke at the given cursor position.
    pub fn trigger(&mut self, col: usize, row: usize) {
        if self.cells.len() >= 64 {
            return;
        }
        self.cells.push(GlitchCell {
            col,
            row,
            started_at: Instant::now(),
        });
    }

    /// Advance the frame counter and expire old animations. Call once per frame.
    pub fn tick(&mut self) {
        self.frame_counter = self.frame_counter.wrapping_add(1);
        self.cells
            .retain(|c| c.started_at.elapsed().as_secs_f32() < GLITCH_DURATION_SECS);
    }

    /// Apply glitch effect to a grid of cells in-place.
    ///
    /// For each active glitch cell whose age is still in the "glitch phase",
    /// replace the character with a cycling glitch character. Once past the
    /// glitch phase the real character shows through (lock-in).
    pub fn apply(&self, grid: &mut [GridCell], cols: usize) {
        let frame = self.frame_counter;
        for gc in &self.cells {
            let age = gc.started_at.elapsed().as_secs_f32();
            let progress = age / GLITCH_DURATION_SECS;

            if progress > GLITCH_PHASE {
                continue;
            }

            let idx = gc.row * cols + gc.col;
            let Some(cell) = grid.get_mut(idx) else { continue };

            if cell.ch == ' ' {
                continue;
            }

            let hash = noise_hash(gc.row, gc.col, frame);
            cell.ch = GLITCH_CHARS[hash % GLITCH_CHARS.len()];
        }
    }
}

/// Deterministic noise hash (same algorithm as boot sequence).
fn noise_hash(row: usize, col: usize, frame: u32) -> usize {
    let mut h = (row as u32).wrapping_mul(2654435761);
    h ^= (col as u32).wrapping_mul(2246822519);
    h ^= frame.wrapping_mul(3266489917);
    h = h.wrapping_mul(668265263);
    h ^= h >> 15;
    h as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trigger_and_tick_lifecycle() {
        let mut fx = KeystrokeFx::new();
        fx.trigger(5, 3);
        assert_eq!(fx.cells.len(), 1);

        fx.tick();
        assert_eq!(fx.cells.len(), 1);

        std::thread::sleep(std::time::Duration::from_millis(150));
        fx.tick();
        assert_eq!(fx.cells.len(), 0);
    }

    #[test]
    fn apply_modifies_grid_cell() {
        let mut fx = KeystrokeFx::new();
        fx.trigger(0, 0);
        fx.tick();

        let mut grid = vec![GridCell {
            ch: 'A',
            fg: [1.0; 4],
            bg: [0.0; 4],
        }];

        fx.apply(&mut grid, 1);
        assert_ne!(grid[0].ch, 'A');
        assert!(GLITCH_CHARS.contains(&grid[0].ch));
    }

    #[test]
    fn apply_skips_spaces() {
        let mut fx = KeystrokeFx::new();
        fx.trigger(0, 0);
        fx.tick();

        let mut grid = vec![GridCell {
            ch: ' ',
            fg: [1.0; 4],
            bg: [0.0; 4],
        }];

        fx.apply(&mut grid, 1);
        assert_eq!(grid[0].ch, ' ');
    }

    #[test]
    fn apply_out_of_bounds_is_safe() {
        let mut fx = KeystrokeFx::new();
        fx.trigger(99, 99);
        fx.tick();

        let mut grid = vec![GridCell {
            ch: 'X',
            fg: [1.0; 4],
            bg: [0.0; 4],
        }];

        fx.apply(&mut grid, 1);
        assert_eq!(grid[0].ch, 'X');
    }
}
