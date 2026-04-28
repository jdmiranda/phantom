//! Compositional layout primitive for reserving rectangles from a parent area.
//!
//! `LayoutBox` is a math-only wrapper around [`Rect`](crate::layout::Rect) that
//! supports reservation (carve a strip off an edge), padding, and ratio-based
//! splitting. Pane chrome uses it to reserve title strips, footers, and gutters
//! from the body without manual offset arithmetic.
//!
//! # Naming
//!
//! The struct is named `LayoutBox` rather than `Box` to avoid shadowing the
//! ubiquitous [`std::boxed::Box`] in user code. The trade-off is mild verbosity
//! for unambiguous imports — pane chrome calls `LayoutBox::new(rect)` without
//! any `use` collision worries.
//!
//! # Non-goals
//!
//! - No tree structure: each reservation is point-in-time and returns fresh
//!   `LayoutBox` values. The caller decides what to do with them.
//! - No caching, no rendering: this is pure rectangle math.
//!
//! # Clamping
//!
//! Reservations and padding clamp to the parent's dimensions. A `reserve_top(h)`
//! with `h > parent.height` returns the full parent as the top slice and a
//! zero-height remainder. `pad` that exceeds a dimension produces a zero-sized
//! axis rather than negative width/height.

use crate::layout::Rect;

/// A rectangle that supports compositional reservation, padding, and splitting.
///
/// `LayoutBox` is `Copy`, so reservation methods that consume `self` are cheap
/// and the original value remains usable in the caller until the operation is
/// performed. The intent is fluent, top-down composition:
///
/// ```ignore
/// let body = LayoutBox::new(pane_rect);
/// let (title, body) = body.reserve_top(20.0);
/// let (body, footer) = body.reserve_bottom(16.0);
/// let body = body.pad(4.0, 4.0, 4.0, 4.0);
/// ```
#[derive(Debug, Clone, Copy)]
pub struct LayoutBox {
    /// The underlying rectangle in pixel coordinates.
    pub rect: Rect,
}

impl LayoutBox {
    /// Wrap a [`Rect`] as a `LayoutBox`.
    pub fn new(rect: Rect) -> Self {
        Self { rect }
    }

    /// Carve `h` pixels off the top edge.
    ///
    /// Returns `(top_slice, remainder)`. The top slice has height `h` (or the
    /// full parent height if `h` exceeds it). The remainder occupies what's
    /// left below.
    pub fn reserve_top(self, h: f32) -> (LayoutBox, LayoutBox) {
        let h = h.max(0.0).min(self.rect.height);
        let top = Rect {
            x: self.rect.x,
            y: self.rect.y,
            width: self.rect.width,
            height: h,
        };
        let remainder = Rect {
            x: self.rect.x,
            y: self.rect.y + h,
            width: self.rect.width,
            height: self.rect.height - h,
        };
        (LayoutBox::new(top), LayoutBox::new(remainder))
    }

    /// Carve `h` pixels off the bottom edge.
    ///
    /// Returns `(remainder, bottom_slice)`. The bottom slice has height `h`
    /// (or the full parent height if `h` exceeds it). The remainder occupies
    /// what's left above.
    pub fn reserve_bottom(self, h: f32) -> (LayoutBox, LayoutBox) {
        let h = h.max(0.0).min(self.rect.height);
        let remainder = Rect {
            x: self.rect.x,
            y: self.rect.y,
            width: self.rect.width,
            height: self.rect.height - h,
        };
        let bottom = Rect {
            x: self.rect.x,
            y: self.rect.y + self.rect.height - h,
            width: self.rect.width,
            height: h,
        };
        (LayoutBox::new(remainder), LayoutBox::new(bottom))
    }

    /// Carve `w` pixels off the left edge.
    ///
    /// Returns `(left_slice, remainder)`.
    pub fn reserve_left(self, w: f32) -> (LayoutBox, LayoutBox) {
        let w = w.max(0.0).min(self.rect.width);
        let left = Rect {
            x: self.rect.x,
            y: self.rect.y,
            width: w,
            height: self.rect.height,
        };
        let remainder = Rect {
            x: self.rect.x + w,
            y: self.rect.y,
            width: self.rect.width - w,
            height: self.rect.height,
        };
        (LayoutBox::new(left), LayoutBox::new(remainder))
    }

    /// Carve `w` pixels off the right edge.
    ///
    /// Returns `(remainder, right_slice)`.
    pub fn reserve_right(self, w: f32) -> (LayoutBox, LayoutBox) {
        let w = w.max(0.0).min(self.rect.width);
        let remainder = Rect {
            x: self.rect.x,
            y: self.rect.y,
            width: self.rect.width - w,
            height: self.rect.height,
        };
        let right = Rect {
            x: self.rect.x + self.rect.width - w,
            y: self.rect.y,
            width: w,
            height: self.rect.height,
        };
        (LayoutBox::new(remainder), LayoutBox::new(right))
    }

    /// Inset on all four sides.
    ///
    /// Arguments are top, right, bottom, left (CSS order). If padding exceeds
    /// the box's width or height, the corresponding axis clamps to zero rather
    /// than going negative.
    pub fn pad(self, t: f32, r: f32, b: f32, l: f32) -> LayoutBox {
        let t = t.max(0.0);
        let r = r.max(0.0);
        let b = b.max(0.0);
        let l = l.max(0.0);

        let new_w = (self.rect.width - l - r).max(0.0);
        let new_h = (self.rect.height - t - b).max(0.0);

        LayoutBox::new(Rect {
            x: self.rect.x + l,
            y: self.rect.y + t,
            width: new_w,
            height: new_h,
        })
    }

    /// Split horizontally into N children whose widths are in the given ratios.
    ///
    /// Ratios should sum to 1.0; the implementation tolerates minor rounding
    /// drift by absorbing leftover width into the final child. Children are
    /// returned in left-to-right order with continuous x positions (no gaps).
    /// An empty `ratios` slice returns an empty `Vec`.
    pub fn split_h(self, ratios: &[f32]) -> Vec<LayoutBox> {
        if ratios.is_empty() {
            return Vec::new();
        }

        let total: f32 = ratios.iter().sum();
        if total <= 0.0 {
            return Vec::new();
        }

        let mut out = Vec::with_capacity(ratios.len());
        let mut x = self.rect.x;
        let mut consumed = 0.0_f32;

        let last = ratios.len() - 1;
        for i in 0..ratios.len() {
            let w = if i == last {
                // Absorb rounding into the final child so widths sum exactly.
                self.rect.width - consumed
            } else {
                self.rect.width * (ratios[i] / total)
            };
            out.push(LayoutBox::new(Rect {
                x,
                y: self.rect.y,
                width: w,
                height: self.rect.height,
            }));
            x += w;
            consumed += w;
        }

        out
    }

    /// Split vertically into N children whose heights are in the given ratios.
    ///
    /// See [`split_h`](Self::split_h) for ratio semantics. Children are
    /// returned in top-to-bottom order with continuous y positions.
    pub fn split_v(self, ratios: &[f32]) -> Vec<LayoutBox> {
        if ratios.is_empty() {
            return Vec::new();
        }

        let total: f32 = ratios.iter().sum();
        if total <= 0.0 {
            return Vec::new();
        }

        let mut out = Vec::with_capacity(ratios.len());
        let mut y = self.rect.y;
        let mut consumed = 0.0_f32;

        let last = ratios.len() - 1;
        for i in 0..ratios.len() {
            let h = if i == last {
                self.rect.height - consumed
            } else {
                self.rect.height * (ratios[i] / total)
            };
            out.push(LayoutBox::new(Rect {
                x: self.rect.x,
                y,
                width: self.rect.width,
                height: h,
            }));
            y += h;
            consumed += h;
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::LayoutBox;
    use crate::layout::Rect;

    const EPSILON: f32 = 0.5;

    fn approx_eq(a: f32, b: f32) -> bool {
        (a - b).abs() < EPSILON
    }

    fn parent() -> LayoutBox {
        LayoutBox::new(Rect {
            x: 10.0,
            y: 20.0,
            width: 800.0,
            height: 600.0,
        })
    }

    // ---- type traits ------------------------------------------------------

    #[test]
    fn layout_box_is_copy_clone_debug() {
        let p = parent();
        let copy = p; // requires Copy
        let cloned = p.clone(); // requires Clone
        let _ = format!("{:?}", p); // requires Debug
        // p is still usable because Copy, not consumed
        assert_eq!(p.rect.width, 800.0);
        assert_eq!(copy.rect.width, 800.0);
        assert_eq!(cloned.rect.width, 800.0);
    }

    // ---- reserve_top ------------------------------------------------------

    #[test]
    fn reserve_top_carves_strip_and_remainder() {
        let (top, rem) = parent().reserve_top(50.0);

        assert_eq!(top.rect.x, 10.0);
        assert_eq!(top.rect.y, 20.0);
        assert_eq!(top.rect.width, 800.0);
        assert_eq!(top.rect.height, 50.0);

        assert_eq!(rem.rect.x, 10.0);
        assert_eq!(rem.rect.y, 70.0);
        assert_eq!(rem.rect.width, 800.0);
        assert_eq!(rem.rect.height, 550.0);
    }

    #[test]
    fn reserve_top_clamps_when_exceeds_parent() {
        let (top, rem) = parent().reserve_top(9999.0);
        assert_eq!(top.rect.height, 600.0);
        assert_eq!(top.rect.y, 20.0);
        assert_eq!(rem.rect.height, 0.0);
        assert_eq!(rem.rect.y, 620.0);
    }

    #[test]
    fn reserve_top_zero_height_is_noop() {
        let (top, rem) = parent().reserve_top(0.0);
        assert_eq!(top.rect.height, 0.0);
        assert_eq!(rem.rect.height, 600.0);
        assert_eq!(rem.rect.y, 20.0);
    }

    // ---- reserve_bottom ---------------------------------------------------

    #[test]
    fn reserve_bottom_carves_strip_and_remainder() {
        let (rem, bot) = parent().reserve_bottom(40.0);

        assert_eq!(rem.rect.x, 10.0);
        assert_eq!(rem.rect.y, 20.0);
        assert_eq!(rem.rect.width, 800.0);
        assert_eq!(rem.rect.height, 560.0);

        assert_eq!(bot.rect.x, 10.0);
        assert_eq!(bot.rect.y, 580.0);
        assert_eq!(bot.rect.width, 800.0);
        assert_eq!(bot.rect.height, 40.0);
    }

    #[test]
    fn reserve_bottom_clamps_when_exceeds_parent() {
        let (rem, bot) = parent().reserve_bottom(9999.0);
        assert_eq!(bot.rect.height, 600.0);
        assert_eq!(bot.rect.y, 20.0);
        assert_eq!(rem.rect.height, 0.0);
    }

    // ---- reserve_left -----------------------------------------------------

    #[test]
    fn reserve_left_carves_strip_and_remainder() {
        let (left, rem) = parent().reserve_left(80.0);

        assert_eq!(left.rect.x, 10.0);
        assert_eq!(left.rect.y, 20.0);
        assert_eq!(left.rect.width, 80.0);
        assert_eq!(left.rect.height, 600.0);

        assert_eq!(rem.rect.x, 90.0);
        assert_eq!(rem.rect.y, 20.0);
        assert_eq!(rem.rect.width, 720.0);
        assert_eq!(rem.rect.height, 600.0);
    }

    #[test]
    fn reserve_left_clamps_when_exceeds_parent() {
        let (left, rem) = parent().reserve_left(9999.0);
        assert_eq!(left.rect.width, 800.0);
        assert_eq!(rem.rect.width, 0.0);
        assert_eq!(rem.rect.x, 810.0);
    }

    // ---- reserve_right ----------------------------------------------------

    #[test]
    fn reserve_right_carves_strip_and_remainder() {
        let (rem, right) = parent().reserve_right(60.0);

        assert_eq!(rem.rect.x, 10.0);
        assert_eq!(rem.rect.y, 20.0);
        assert_eq!(rem.rect.width, 740.0);
        assert_eq!(rem.rect.height, 600.0);

        assert_eq!(right.rect.x, 750.0);
        assert_eq!(right.rect.y, 20.0);
        assert_eq!(right.rect.width, 60.0);
        assert_eq!(right.rect.height, 600.0);
    }

    #[test]
    fn reserve_right_clamps_when_exceeds_parent() {
        let (rem, right) = parent().reserve_right(9999.0);
        assert_eq!(right.rect.width, 800.0);
        assert_eq!(rem.rect.width, 0.0);
    }

    // ---- pad --------------------------------------------------------------

    #[test]
    fn pad_insets_all_four_sides() {
        let p = parent().pad(5.0, 6.0, 7.0, 8.0);
        // top=5, right=6, bottom=7, left=8
        assert_eq!(p.rect.x, 18.0); // 10 + 8
        assert_eq!(p.rect.y, 25.0); // 20 + 5
        assert_eq!(p.rect.width, 786.0); // 800 - 8 - 6
        assert_eq!(p.rect.height, 588.0); // 600 - 5 - 7
    }

    #[test]
    fn pad_zero_is_noop() {
        let p = parent().pad(0.0, 0.0, 0.0, 0.0);
        assert_eq!(p.rect.x, 10.0);
        assert_eq!(p.rect.y, 20.0);
        assert_eq!(p.rect.width, 800.0);
        assert_eq!(p.rect.height, 600.0);
    }

    #[test]
    fn pad_clamps_to_zero_when_excessive() {
        // Horizontal overflow: l + r > width.
        let p = parent().pad(0.0, 500.0, 0.0, 500.0);
        assert_eq!(p.rect.width, 0.0);
        assert_eq!(p.rect.height, 600.0);

        // Vertical overflow: t + b > height.
        let p = parent().pad(400.0, 0.0, 400.0, 0.0);
        assert_eq!(p.rect.width, 800.0);
        assert_eq!(p.rect.height, 0.0);
    }

    // ---- split_h ----------------------------------------------------------

    #[test]
    fn split_h_equal_halves_are_continuous() {
        let parts = parent().split_h(&[0.5, 0.5]);
        assert_eq!(parts.len(), 2);

        // Widths sum to parent.width.
        let sum: f32 = parts.iter().map(|p| p.rect.width).sum();
        assert!(approx_eq(sum, 800.0), "widths sum: got {}", sum);

        // Equal widths.
        assert!(approx_eq(parts[0].rect.width, parts[1].rect.width));

        // Continuous: child[1].x == child[0].x + child[0].width.
        assert!(approx_eq(parts[1].rect.x, parts[0].rect.x + parts[0].rect.width));

        // Heights and y unchanged.
        for p in &parts {
            assert_eq!(p.rect.y, 20.0);
            assert_eq!(p.rect.height, 600.0);
        }
    }

    #[test]
    fn split_h_three_parts_sum_within_tolerance() {
        let parts = parent().split_h(&[0.3, 0.3, 0.4]);
        assert_eq!(parts.len(), 3);

        let sum: f32 = parts.iter().map(|p| p.rect.width).sum();
        assert!(approx_eq(sum, 800.0), "widths sum: got {}", sum);

        // Continuous x positions.
        assert!(approx_eq(parts[1].rect.x, parts[0].rect.x + parts[0].rect.width));
        assert!(approx_eq(parts[2].rect.x, parts[1].rect.x + parts[1].rect.width));

        // First two roughly equal.
        assert!(approx_eq(parts[0].rect.width, parts[1].rect.width));
    }

    #[test]
    fn split_h_single_ratio_returns_full_parent() {
        let parts = parent().split_h(&[1.0]);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].rect.x, 10.0);
        assert_eq!(parts[0].rect.y, 20.0);
        assert_eq!(parts[0].rect.width, 800.0);
        assert_eq!(parts[0].rect.height, 600.0);
    }

    #[test]
    fn split_h_empty_ratios_returns_empty_vec() {
        let parts = parent().split_h(&[]);
        assert!(parts.is_empty());
    }

    // ---- split_v ----------------------------------------------------------

    #[test]
    fn split_v_equal_halves_are_continuous() {
        let parts = parent().split_v(&[0.5, 0.5]);
        assert_eq!(parts.len(), 2);

        let sum: f32 = parts.iter().map(|p| p.rect.height).sum();
        assert!(approx_eq(sum, 600.0));
        assert!(approx_eq(parts[0].rect.height, parts[1].rect.height));
        assert!(approx_eq(parts[1].rect.y, parts[0].rect.y + parts[0].rect.height));

        for p in &parts {
            assert_eq!(p.rect.x, 10.0);
            assert_eq!(p.rect.width, 800.0);
        }
    }

    #[test]
    fn split_v_three_parts_sum_within_tolerance() {
        let parts = parent().split_v(&[0.25, 0.25, 0.5]);
        assert_eq!(parts.len(), 3);

        let sum: f32 = parts.iter().map(|p| p.rect.height).sum();
        assert!(approx_eq(sum, 600.0));
        assert!(approx_eq(parts[1].rect.y, parts[0].rect.y + parts[0].rect.height));
        assert!(approx_eq(parts[2].rect.y, parts[1].rect.y + parts[1].rect.height));
    }

    #[test]
    fn split_v_single_ratio_returns_full_parent() {
        let parts = parent().split_v(&[1.0]);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].rect.height, 600.0);
    }

    #[test]
    fn split_v_empty_ratios_returns_empty_vec() {
        let parts = parent().split_v(&[]);
        assert!(parts.is_empty());
    }

    // ---- composition ------------------------------------------------------

    #[test]
    fn composed_chrome_reserves_title_and_footer() {
        // Simulates pane chrome: title strip, footer, padded body.
        let body = parent();
        let (title, body) = body.reserve_top(20.0);
        let (body, footer) = body.reserve_bottom(16.0);
        let body = body.pad(4.0, 4.0, 4.0, 4.0);

        assert_eq!(title.rect.height, 20.0);
        assert_eq!(footer.rect.height, 16.0);
        // body should be parent - 20 (title) - 16 (footer), then minus 4*2 padding
        assert_eq!(body.rect.height, 600.0 - 20.0 - 16.0 - 8.0);
        assert_eq!(body.rect.width, 800.0 - 8.0);
        // body sits below title and above footer
        assert_eq!(body.rect.y, 20.0 + 20.0 + 4.0);
    }
}
