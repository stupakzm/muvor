//! Where a badge is drawn, and where the click lands (§5.1a).
//!
//! A label drawn on the target's centre hides the target: on a 36x46
//! header-bar button the badge covers almost all of it, which is exactly the
//! icon the user is trying to recognise. So the badge sits *beside* the
//! click point, near enough to read as its label and far enough to leave the
//! target visible.
//!
//! **Beside the click point, not beside the bounding box** — changed
//! 2026-08-20. The first rule hung the badge off the target's *edge*, which
//! makes the distance from the badge to the dot `w/2 - INSET`: 10 px on a
//! 36 px button, and **512 px on D11's 1917x997 centre target**, whose badge
//! was drawn at the top of the screen while its dot sat in the middle of it.
//! A label that far from what it labels is not a label. Capping the gap at a
//! constant fixes the large case and leaves every small one untouched.
//!
//! **The click, though, lands at the target's centre** — changed 2026-08-19,
//! after the cyan dot (§5.1c) put the old rule on screen for the first time
//! and it read as an error rather than as a design. The badge is off-centre
//! because it must be; the click has no such excuse, and the centre is the
//! point furthest from every edge.
//!
//! So the two numbers no longer share a rule: the badge follows the free
//! side, the click does not move at all. That is the *point* — a click that
//! does not depend on where the badge was pushed cannot drift when the badge
//! is pushed somewhere else.
//!
//! Pure arithmetic, so the tests prove it rather than measure it (§8).

/// The badge's drawn size in pixels.
///
/// **A contract with the shell half**, not a local constant: `extension.js`
/// centres its `St.Label` inside the rectangle it is handed, so muvor sends
/// the badge's own rectangle and the shell's arithmetic becomes the identity.
/// The numbers match `.muvor-label` in `stylesheet.css` — two monospace
/// characters at 13px, 4px of horizontal padding, a 2px border. If the style
/// changes, these change with it, and until they do the click sits beside a
/// badge of the wrong width.
pub const BADGE_W: i32 = 28;
pub const BADGE_H: i32 = 22;

/// How far the badge overlaps the target's edge, on a target small enough
/// for that to be the binding constraint.
///
/// Enough that the badge visibly belongs to the target rather than floating
/// beside it, and little enough that it hides none of the icon. Until
/// 2026-08-19 this was also where the click landed; the click is the centre
/// now, so this number is cosmetic and can be tuned by eye.
const INSET: i32 = 8;

/// The furthest the badge's near edge may sit from the click point.
///
/// This is the fix of 2026-08-20 and it is one line of arithmetic. The
/// original rule placed the badge's near edge at `x + INSET` — measured from
/// the *bounding box* — which is the same as saying the gap to the dot is
/// `w/2 - INSET`. That term is invisible on the targets it was designed
/// against and unbounded on everything else:
///
/// | target | `w/2 - INSET` |
/// |---|---|
/// | 34 px toolbar button | 9 px |
/// | 36 px header-bar button (M5's first click) | 10 px |
/// | ~200 px Nautilus sidebar row | ~92 px |
/// | 1917 px D11 centre target | **950 px** |
///
/// Capping it at 10 makes every target up to 36 px keep the placement it was
/// measured with — byte-identical, which the tests below assert — and stops
/// the badge walking away from its own dot on everything larger.
const GAP_MAX: i32 = 10;

/// Which side of the target the badge sits on.
///
/// Four, not two, because two is not enough: a row of toolbar buttons 34 px
/// apart cannot hold 28 px badges side by side, and the ones that could not
/// fit were drawn **on top of each other** — measured on Nautilus's header
/// bar, where `k` and `l` landed in the same place and neither could be
/// read. Above and below are where the room actually is on a packed row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// Badge to the left of the target — the first choice, because the eye
    /// reads left to right and the badge is read first.
    Left,
    /// Badge to the right of the target.
    Right,
    /// Badge above the target.
    Above,
    /// Badge below the target.
    Below,
}

impl Side {
    /// The order they are tried in. Left first, then right, then the
    /// verticals — a horizontal row is the crowded case, so the escape from
    /// it should be vertical.
    const PREFERENCE: [Self; 4] = [Self::Left, Self::Right, Self::Above, Self::Below];
}

/// A badge rectangle and the point to click, in the same coordinate space as
/// the bounds they were computed from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    /// Top-left of the badge, `BADGE_W` x `BADGE_H`.
    pub badge_x: i32,
    pub badge_y: i32,
    /// The point that will actually be injected: the target's centre,
    /// whichever side the badge ended up on.
    pub click_x: i32,
    pub click_y: i32,
    pub side: Side,
}

impl Placement {
    pub fn badge(&self) -> (i32, i32, i32, i32) {
        (self.badge_x, self.badge_y, BADGE_W, BADGE_H)
    }

    pub fn click(&self) -> (i32, i32) {
        (self.click_x, self.click_y)
    }
}

/// Place a badge beside a target's bounds.
///
/// `room_on_left` is the caller's answer to "is there screen to the left of
/// this target", which only the caller can know: these bounds are
/// window-relative (D13) and the screen edge is not.
pub fn place(x: i32, y: i32, w: i32, h: i32, room_on_left: bool) -> Placement {
    on_side(x, y, w, h, if room_on_left { Side::Left } else { Side::Right })
}

/// How far out from the click point a badge is placed.
///
/// Two of them, tried in order. `Hug` is where a badge belongs. `Edge` is
/// the pre-2026-08-20 bounding-box rule, kept as an outer ring rather than
/// deleted, because hugging on its own removes an escape that was load
/// bearing: **nested targets share a centre**, so all four hugged sides give
/// them the same four rectangles and the badges would be drawn on top of one
/// another — the fault §5.1c was written to remove. Falling back to the box
/// edge puts the crowded case back exactly where it used to be, which is a
/// placement that has already been measured.
///
/// **Hugging offers two clear sides, not four.** At `GAP_MAX` the four
/// badges of a shared centre are packed tightly enough that `Above` and
/// `Below` clip the corners of `Left` and `Right` — a vertical badge would
/// need `gap_y >= BADGE_H / 2` to clear them, and forcing that would move
/// every small target, which is the one thing this change must not do. So
/// the third co-centred target onward takes the outer ring. Only targets
/// nested inside one another pay it; a packed toolbar is untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ring {
    /// Beside the dot, at most `GAP_MAX` from it.
    Hug,
    /// Beside the bounding box, overlapping it by `INSET`.
    Edge,
}

impl Ring {
    /// Inner first. A badge only moves outward to escape a collision.
    const OUTWARD: [Self; 2] = [Self::Hug, Self::Edge];
}

/// The placement a target would get on a given side, hugging its click point.
pub fn on_side(x: i32, y: i32, w: i32, h: i32, side: Side) -> Placement {
    on_ring(x, y, w, h, side, Ring::Hug)
}

fn on_ring(x: i32, y: i32, w: i32, h: i32, side: Side, ring: Ring) -> Placement {
    // Never past the target's own middle: on a target narrower than twice
    // the inset, a badge overlapping "just inside the edge" would cover it.
    let inset_x = INSET.min((w / 2).max(1));
    let inset_y = INSET.min((h / 2).max(1));

    // The click, and it is the same point whichever side the badge takes.
    let (cx, cy) = (x + w / 2, y + h / 2);

    // The gap from the dot to the badge's near edge. `Edge` is the old rule
    // rearranged — `x + inset_x` is `cx - (w/2 - inset_x)` — and `Hug` is
    // that same expression with its unbounded term capped. The two are equal
    // on any target up to `2 * (INSET + GAP_MAX)` px, which is why every
    // small target keeps the placement it was measured with.
    let (mut gap_x, mut gap_y) = ((w / 2 - inset_x).max(0), (h / 2 - inset_y).max(0));
    if ring == Ring::Hug {
        gap_x = gap_x.min(GAP_MAX);
        gap_y = gap_y.min(GAP_MAX);
    }

    // The badge hangs off the click point, and is centred on the other axis.
    let (badge_x, badge_y) = match side {
        Side::Left => (cx - gap_x - BADGE_W, cy - BADGE_H / 2),
        Side::Right => (cx + gap_x, cy - BADGE_H / 2),
        Side::Above => (cx - BADGE_W / 2, cy - gap_y - BADGE_H),
        Side::Below => (cx - BADGE_W / 2, cy + gap_y),
    };
    Placement { badge_x, badge_y, click_x: cx, click_y: cy, side }
}

/// Do two badges overlap?
fn collides(a: &Placement, b: &Placement) -> bool {
    a.badge_x < b.badge_x + BADGE_W
        && b.badge_x < a.badge_x + BADGE_W
        && a.badge_y < b.badge_y + BADGE_H
        && b.badge_y < a.badge_y + BADGE_H
}

/// Place a whole set of badges so that **no two overlap**.
///
/// Targets are taken in the order given — which is label order, so the
/// result is deterministic for a given set — and each takes the first side
/// (§`Side::PREFERENCE`) whose badge is on screen and clear of everything
/// already placed. The four sides are tried **hugging the dot first, then at
/// the bounding-box edge** (§`Ring::OUTWARD`), so a badge only moves away
/// from what it labels in order to escape a collision. If all eight collide,
/// the least-bad one is used: a badge that overlaps is still better than a
/// target with no badge at all, and this is the case D11's centre targets
/// exist to make rare.
///
/// **The cost, stated plainly:** a badge's *position* now depends on its
/// neighbours, so adding a target can move a badge that was already there.
/// The **label** does not move — that is §5.3b's screen column, which depends
/// only on the target's own position — and the label is what the hand types.
/// D5 protects the keystrokes, not the pixels.
pub fn place_all(bounds: &[(i32, i32, i32, i32)], screen: (i32, i32)) -> Vec<Placement> {
    let mut placed: Vec<Placement> = Vec::with_capacity(bounds.len());
    for &(x, y, w, h) in bounds {
        let mut fallback: Option<Placement> = None;
        let mut chosen: Option<Placement> = None;
        'rings: for ring in Ring::OUTWARD {
            for side in Side::PREFERENCE {
                let p = on_ring(x, y, w, h, side, ring);
                let on_screen = p.badge_x >= 0
                    && p.badge_y >= 0
                    && p.badge_x + BADGE_W <= screen.0
                    && p.badge_y + BADGE_H <= screen.1;
                if !on_screen {
                    continue;
                }
                if fallback.is_none() {
                    fallback = Some(p);
                }
                if !placed.iter().any(|q| collides(&p, q)) {
                    chosen = Some(p);
                    break 'rings;
                }
            }
        }
        placed.push(
            chosen
                .or(fallback)
                .unwrap_or_else(|| on_side(x, y, w, h, Side::Left)),
        );
    }
    placed
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nautilus's header bar, where the badges were landing on top of each
    /// other: buttons 34 px wide with 34 px between them cannot hold 28 px
    /// badges side by side.
    #[test]
    fn a_packed_toolbar_never_draws_two_badges_in_one_place() {
        let row: Vec<(i32, i32, i32, i32)> =
            (0..6).map(|i| (900 + i * 35, 0, 34, 34)).collect();
        let placed = place_all(&row, (1920, 1080));

        assert_eq!(placed.len(), 6);
        for (i, a) in placed.iter().enumerate() {
            for (j, b) in placed.iter().enumerate() {
                assert!(i == j || !collides(a, b), "badge {i} overlaps badge {j}");
            }
        }
    }

    /// Every click must still land inside the target it labels, whichever
    /// side the badge was pushed to.
    #[test]
    fn moving_the_badge_never_moves_the_click_outside_its_target() {
        let row: Vec<(i32, i32, i32, i32)> =
            (0..8).map(|i| (100 + i * 36, 300, 34, 34)).collect();
        for (p, &(x, y, w, h)) in place_all(&row, (1920, 1080)).iter().zip(&row) {
            assert!(p.click_x >= x && p.click_x <= x + w, "{:?} escaped in x", p.side);
            assert!(p.click_y >= y && p.click_y <= y + h, "{:?} escaped in y", p.side);
        }
    }

    #[test]
    fn a_badge_is_never_drawn_off_the_screen() {
        // Targets in all four corners, where two of the four sides are
        // unavailable each time.
        let corners = [(0, 0, 30, 30), (1890, 0, 30, 30), (0, 1050, 30, 30), (1890, 1050, 30, 30)];
        for p in place_all(&corners, (1920, 1080)) {
            assert!(p.badge_x >= 0 && p.badge_y >= 0, "{p:?} is off the top-left");
            assert!(p.badge_x + BADGE_W <= 1920, "{p:?} is off the right");
            assert!(p.badge_y + BADGE_H <= 1080, "{p:?} is off the bottom");
        }
    }

    /// Same input, same output — the overlay must not shimmer between two
    /// equally good arrangements.
    #[test]
    fn placement_is_deterministic() {
        let set: Vec<(i32, i32, i32, i32)> =
            (0..12).map(|i| (200 + i * 30, 40 + (i % 3) * 20, 34, 34)).collect();
        assert_eq!(place_all(&set, (1920, 1080)), place_all(&set, (1920, 1080)));
    }

    /// gnome-terminal's New Tab button, the target M5's first real click hit.
    #[test]
    fn the_badge_clears_the_button_it_labels() {
        let p = place(6, 0, 36, 46, true);
        // The click is the centre: 6 + 36/2, 0 + 46/2.
        assert_eq!(p.click(), (24, 23));
        // And the badge is almost entirely outside the target: 8 px of
        // overlap out of 36, against the 36 the centred badge used to cover.
        let (bx, _, bw, _) = p.badge();
        assert_eq!(bx + bw - 6, 8, "only the inset overlaps the target");
    }

    /// The rule that replaced "the click is the badge's inner edge"
    /// (2026-08-19): the badge moves, the click does not.
    #[test]
    fn the_click_is_the_centre_whichever_side_the_badge_took() {
        let centre = (120, 120);

        let left = place(100, 100, 40, 40, true);
        assert_eq!(left.side, Side::Left);
        assert_eq!(left.click(), centre);

        let right = place(100, 100, 40, 40, false);
        assert_eq!(right.side, Side::Right);
        assert_eq!(right.click(), centre);

        for side in Side::PREFERENCE {
            assert_eq!(on_side(100, 100, 40, 40, side).click(), centre, "{side:?}");
        }

        // The badge did move, or the test above proves nothing.
        assert_ne!(left.badge_x, right.badge_x);
        // Same target, so the badge is vertically centred either way.
        assert_eq!(left.badge_y, right.badge_y);
    }

    #[test]
    fn a_narrow_target_is_still_clicked_inside_itself() {
        // Narrower than twice the inset, so the inset is clamped to w/2 and
        // the badge cannot swallow the target whole.
        let p = place(50, 50, 6, 20, true);
        assert!(p.click_x > 50 && p.click_x < 56, "{} is outside 50..56", p.click_x);
        assert_eq!(p.badge_x + BADGE_W, 53, "the badge stops at the middle");
        let q = place(50, 50, 6, 20, false);
        assert!(q.click_x > 50 && q.click_x < 56, "{} is outside 50..56", q.click_x);
        assert_eq!(q.badge_x, 53, "the badge starts at the middle");
    }

    /// Degenerate bounds are the toolkit's problem, not a panic here: §4.4
    /// filters them, and validate-at-action refuses what is left.
    #[test]
    fn a_zero_width_target_does_not_divide_by_zero() {
        let p = place(10, 10, 0, 0, true);
        assert_eq!(p.click(), (10, 10));
    }

    /// How far the dot is from the nearest edge of its own badge. Zero when
    /// the dot is inside the badge.
    fn gap_to_dot(p: &Placement) -> i32 {
        let dx = (p.badge_x - p.click_x).max(p.click_x - (p.badge_x + BADGE_W)).max(0);
        let dy = (p.badge_y - p.click_y).max(p.click_y - (p.badge_y + BADGE_H)).max(0);
        dx.max(dy)
    }

    /// The bug this rule was changed for, 2026-08-20, with the real numbers
    /// off the screen it was found on: D11's centre target on a maximized
    /// gnome-terminal is `1,81 1917x997` and its click is `959,579`.
    ///
    /// Under the bounding-box rule left and right were both off-screen, so
    /// the badge went *above* — to `945,67`, **512 px from its own dot**, up
    /// against the top of the display where it read as a label for the
    /// panel. It must now sit beside the dot.
    #[test]
    fn d11s_centre_target_is_labelled_beside_its_own_dot() {
        let placed = place_all(&[(1, 81, 1917, 997)], (1920, 1080));
        let p = placed[0];

        assert_eq!(p.click(), (959, 579), "the click did not move — it never does");
        assert!(gap_to_dot(&p) <= GAP_MAX, "{p:?} is {} px from its dot", gap_to_dot(&p));

        // And specifically: beside it, not above it. Hugging puts left back
        // on screen, so the badge takes the side the eye reads first.
        assert_eq!(p.side, Side::Left);
        assert_eq!((p.badge_x, p.badge_y), (921, 568));
    }

    /// The general form of the same claim, across four orders of magnitude
    /// of target size. One target at a time, so nothing is displaced by a
    /// collision — that escape is the next test's business.
    #[test]
    fn no_badge_is_ever_far_from_the_dot_it_labels() {
        for w in [6, 20, 34, 36, 120, 400, 892, 1917] {
            for h in [6, 22, 34, 46, 300, 997] {
                let placed = place_all(&[(4, 4, w, h)], (1920, 1080));
                let g = gap_to_dot(&placed[0]);
                assert!(g <= GAP_MAX, "{w}x{h}: badge sits {g} px from its dot ({:?})", placed[0]);
            }
        }
    }

    /// Nothing small moved. These are the exact coordinates the old
    /// bounding-box rule produced, kept as literals: the cap is chosen so
    /// that every target up to `2 * (INSET + GAP_MAX)` px is byte-identical,
    /// and this is what says so.
    #[test]
    fn small_targets_kept_the_placement_they_were_measured_with() {
        // gnome-terminal's New Tab button — M5's first real click.
        let p = place(6, 0, 36, 46, true);
        assert_eq!((p.badge_x, p.badge_y), (-14, 12));

        // Nautilus's header bar, the row that forced four sides (§5.1c).
        let q = place(900, 0, 34, 34, true);
        assert_eq!((q.badge_x, q.badge_y), (880, 6));
        assert_eq!(on_side(900, 0, 34, 34, Side::Above).badge_y, -14);
        assert_eq!(on_side(900, 0, 34, 34, Side::Below).badge_y, 26);

        // A 30 px corner target, badge pushed to the right.
        assert_eq!(place(0, 0, 30, 30, false).badge_x, 22);
    }

    /// The escape the `Edge` ring exists for. Five nested targets share a
    /// centre, so all four hugged sides are the same four rectangles for
    /// every one of them — without an outer ring the fifth badge would have
    /// nowhere left to go and would be drawn on top of another.
    #[test]
    fn targets_that_share_a_centre_still_get_badges_of_their_own() {
        let nested: Vec<(i32, i32, i32, i32)> =
            (1..=5).map(|k| (300 - k * 50, 300 - k * 50, k * 100, k * 100)).collect();
        let placed = place_all(&nested, (1920, 1080));

        for p in &placed {
            assert_eq!(p.click(), (300, 300), "every one of them clicks the same point");
        }
        for (i, a) in placed.iter().enumerate() {
            for (j, b) in placed.iter().enumerate() {
                assert!(i == j || !collides(a, b), "badge {i} overlaps badge {j}");
            }
        }
        // Two hug, and then the ring is exhausted — hugging packs the four
        // sides tightly enough that `Above` and `Below` clip the corners of
        // `Left` and `Right`, so a shared centre really does offer only two
        // clear positions. That is the cost of the change, and it is paid
        // only by targets that sit inside one another.
        assert_eq!(placed[0].side, Side::Left);
        assert_eq!(placed[1].side, Side::Right);
        for (i, p) in placed.iter().take(2).enumerate() {
            assert!(gap_to_dot(p) <= GAP_MAX, "badge {i} was pushed out unnecessarily");
        }
        for (i, p) in placed.iter().skip(2).enumerate() {
            assert!(gap_to_dot(p) > GAP_MAX, "badge {} did not need the outer ring", i + 2);
        }
    }

    #[test]
    fn the_badge_is_where_the_shell_would_centre_it() {
        // The shell places a label at `x + w/2 - BADGE_W/2`, so handing it the
        // badge's own rectangle must be the identity.
        let p = place(200, 80, 60, 30, true);
        let (bx, by, bw, bh) = p.badge();
        assert_eq!(bx + bw / 2 - BADGE_W / 2, bx);
        assert_eq!(by + bh / 2 - BADGE_H / 2, by);
    }
}
