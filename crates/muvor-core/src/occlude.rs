//! What is covered, and by what (plan.md §4.3).
//!
//! §4.3 scopes detection to the focused window, and says why in one
//! sentence: **"AT-SPI has no concept of occlusion."** Nautilus's `Search
//! Everywhere` and `Main Menu` toggles both pass `VISIBLE`, `SHOWING` and
//! `SENSITIVE`, with sane bounds, while the window is completely buried
//! under a maximized terminal. A badge drawn on one of those is a badge that
//! clicks whatever is on top — the wrong-click outcome §4.5 exists to
//! prevent, arriving by a route §4.5 cannot see, because the claim is built
//! and validated in the buried window's own tree and everything about it is
//! true except that nobody can see it.
//!
//! The same paragraph names the only thing that can answer it: *"only the
//! compositor's stacking order can [tell you a window is covered], which is
//! one more thing the extension supplies."* Extension v8 supplies it, and
//! this is the arithmetic that consumes it.
//!
//! **The rule is about the click point, not the rectangle.** A target half
//! behind another window is still clickable if the pixel muvor would aim at
//! is showing, and refusing it would drop most of a sidebar every time a
//! dialog overlapped its edge. A target whose *click point* is covered is
//! never offered, whatever fraction of it is visible.
//!
//! Everything here is integer arithmetic on rectangles, and is tested
//! without a compositor for the reason §8 gives.

use crate::Rect;

/// Is this point covered by any of `above`?
///
/// `above` is in stacking order or any order — covering is a set question.
/// Rectangles are half-open: a point on the right or bottom edge of a window
/// is *not* inside it, which is the same convention [`Rect::area`] implies
/// and the one the compositor's own frame rects follow.
pub fn is_covered(point: (i32, i32), above: &[Rect]) -> bool {
    let (px, py) = point;
    above.iter().any(|r| {
        px >= r.x && py >= r.y && px < r.x.saturating_add(r.w) && py < r.y.saturating_add(r.h)
    })
}

/// How much of `rect` no rectangle in `above` covers, as a fraction of its
/// area. `1.0` is entirely visible, `0.0` entirely buried.
///
/// Reported rather than enforced. It is what makes "muvor skipped that
/// window" legible in `--explain` instead of being a silent absence, and
/// §2.5's rule is that a measurement nobody can see gets rediscovered.
///
/// Computed by sampling the covering set per row of the rectangle rather
/// than by rectangle subtraction: `above` is a handful of windows, `rect` is
/// a window, and an exact area needs no more than the row spans.
pub fn visible_fraction(rect: Rect, above: &[Rect]) -> f64 {
    if rect.w <= 0 || rect.h <= 0 {
        return 0.0;
    }
    let total = rect.area() as f64;
    let mut shown: i64 = 0;
    for y in rect.y..rect.y.saturating_add(rect.h) {
        // The covered x-spans on this row, merged.
        let mut spans: Vec<(i32, i32)> = above
            .iter()
            .filter(|r| y >= r.y && y < r.y.saturating_add(r.h))
            .map(|r| {
                (
                    r.x.max(rect.x),
                    r.x.saturating_add(r.w).min(rect.x.saturating_add(rect.w)),
                )
            })
            .filter(|(a, b)| b > a)
            .collect();
        spans.sort_unstable();
        let mut covered: i64 = 0;
        let mut edge = i32::MIN;
        for (a, b) in spans {
            let a = a.max(edge);
            if b > a {
                covered += i64::from(b - a);
                edge = b;
            }
        }
        shown += i64::from(rect.w) - covered;
    }
    (shown as f64 / total).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_above_covers_nothing() {
        assert!(!is_covered((10, 10), &[]));
        assert_eq!(visible_fraction(Rect::new(0, 0, 10, 10), &[]), 1.0);
    }

    #[test]
    fn a_point_inside_a_window_above_is_covered() {
        let above = [Rect::new(100, 100, 200, 200)];
        assert!(is_covered((150, 150), &above));
        assert!(!is_covered((99, 150), &above));
        assert!(!is_covered((150, 99), &above));
    }

    #[test]
    fn the_far_edge_is_outside() {
        // Half-open, like the compositor's rects: a window at x=100 w=200
        // owns 100..299, and 300 belongs to whatever is next to it. Getting
        // this wrong drops a column of targets down the seam between two
        // tiled windows.
        let above = [Rect::new(100, 100, 200, 200)];
        assert!(is_covered((299, 299), &above));
        assert!(!is_covered((300, 200), &above));
        assert!(!is_covered((200, 300), &above));
    }

    #[test]
    fn a_partly_covered_rectangle_reports_its_fraction() {
        // A 100x100 window with a 100x50 bar across its bottom half.
        let r = Rect::new(0, 0, 100, 100);
        let above = [Rect::new(0, 50, 100, 50)];
        assert!((visible_fraction(r, &above) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn overlapping_covers_are_not_counted_twice() {
        // Two windows above, overlapping each other. Naive summation would
        // report more covered than exists and could go negative — which is
        // how a fully visible window comes out "0% visible" and is skipped.
        let r = Rect::new(0, 0, 100, 100);
        let above = [Rect::new(0, 0, 60, 100), Rect::new(40, 0, 60, 100)];
        assert_eq!(visible_fraction(r, &above), 0.0);

        let above = [Rect::new(0, 0, 60, 100), Rect::new(40, 0, 20, 100)];
        assert!((visible_fraction(r, &above) - 0.4).abs() < 1e-9);
    }

    #[test]
    fn a_cover_reaching_outside_the_rectangle_is_clipped() {
        let r = Rect::new(50, 50, 100, 100);
        let above = [Rect::new(0, 0, 1000, 100)];   // covers rows 50..99
        assert!((visible_fraction(r, &above) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn a_degenerate_rectangle_is_not_visible() {
        assert_eq!(visible_fraction(Rect::new(0, 0, 0, 10), &[]), 0.0);
    }

    #[test]
    fn saturating_arithmetic_survives_a_nonsense_rectangle() {
        // §4.4 lets a rejected node carry i32::MIN/MAX. Overflow here would
        // be a panic on the hot path, in the one function whose job is to
        // stop a wrong click.
        let above = [Rect::new(i32::MAX - 1, i32::MAX - 1, 10, 10)];
        assert!(!is_covered((0, 0), &above));
        let _ = visible_fraction(Rect::new(0, 0, 4, 4), &above);
    }
}
