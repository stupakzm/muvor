//! What counts as a target (plan.md §4.4).
//!
//! Pure, and deliberately so: every rule here is decided from a role, a
//! state set and two rectangles, so the whole of §4.4 is testable with no
//! bus, no display and no session — including against the counter-examples
//! this machine actually produces.

use atspi::{Role, State, StateSet};

use crate::target::WindowRect;

/// Roles that can be clicked. §4.4.
///
/// A closed list, not a "not a container" heuristic: an unknown role is not
/// a target, because a wrong click is the failure mode this whole document
/// is organised around, and a missing hint is merely an annoyance.
pub const ACTIONABLE: &[Role] = &[
    // `Button` is AT-SPI's `push button`; the role name in a --dump is
    // "push button" even though the Rust variant is not.
    Role::Button,
    Role::PushButtonMenu,
    Role::ToggleButton,
    Role::CheckBox,
    Role::RadioButton,
    Role::Link,
    Role::MenuItem,
    Role::CheckMenuItem,
    Role::RadioMenuItem,
    Role::PageTab,
    Role::Entry,
    Role::PasswordText,
    Role::ComboBox,
    Role::ListItem,
    Role::TreeItem,
    Role::Slider,
    Role::SpinButton,
];

/// Roles that may sit between a target and the point about to be clicked.
///
/// A closed list for the same reason `ACTIONABLE` is one, and the reasoning
/// is symmetric: an unknown role is not a *target*, and it is not
/// *decoration* either. §4.5 accepts a click whose point resolves to one of
/// these inside the claimed target — a row's own label, the box GTK4 wraps
/// it in — and refuses everything else, including `Role::Invalid` and a node
/// that will not say what it is.
///
/// **Measured on Nautilus, 2026-08-19:** a sidebar row resolves to
/// `label` (29) inside `panel` (39) — GTK4 names that panel "generic" —
/// inside `grouping` (99), and the row itself is never what
/// `GetAccessibleAtPoint` returns. The same
/// window returns `invalid` inside a dismissed popover's button, which must
/// keep being refused; that is why this is a list and not `!ACTIONABLE`.
pub const PASSIVE: &[Role] = &[
    Role::Label,
    Role::Static,
    Role::Text,
    Role::Image,
    Role::Icon,
    Role::Panel,
    Role::Filler,
    Role::Section,
    Role::Separator,
    Role::Grouping,
    Role::ScrollPane,
    Role::Viewport,
];

/// Role *names* that may sit between a target and the point, when the role
/// number itself is `Invalid`.
///
/// **AT-SPI has no number for ARIA's `presentation`**, so GTK4 sends role 0
/// — `Invalid` — and puts the word in `GetRoleName` instead. `Invalid` is
/// exactly what `PASSIVE` refuses to trust, so the two meanings of 0 arrive
/// indistinguishable unless the name is asked for.
///
/// **Measured on Nautilus, 2026-08-26**, walking `Parent` from every node
/// that blocked a click: all seven are role 0 named `presentation`, and
/// every one of their parents is the claimed widget itself —
/// `button 'Close'`, `button 'Back'`, `toggle button 'Search Everywhere'`,
/// and `button 'Unmount'`, which §4.5's notes had recorded as a
/// dismissed-popover ghost. It is the icon inside the button. Blocking it
/// refused **seven of Nautilus's header-bar and sidebar controls** with no
/// reason a user could act on.
///
/// A list, not "anything that names itself": a node that answers `Invalid`
/// with any other name, or will not answer at all, still blocks. And this is
/// only ever consulted *below* the claim — the walk that reaches it has the
/// claimed target as an ancestor, so a ghost with no relation to the claim
/// is refused before the name is asked for.
pub const PASSIVE_ROLE_NAMES: &[&str] = &["presentation"];

/// Below this, in either dimension, a rect is not something a human aims at.
/// GTK reports 1x1 and 0x0 rects for laid-out-but-unrendered widgets.
pub const MIN_SIDE: i32 = 4;

/// Two targets whose centres are within this many pixels click the same
/// thing, whatever the tree says they are.
///
/// **Measured on Nautilus, 2026-08-19:** `Main Menu` is exposed as a
/// `push button` *and* a `toggle button` on identical bounds; `View Options`
/// appears three times across two overlapping rectangles. Labelling each is
/// two hints that do the same thing, and one of them has to be wrong about
/// what it is.
pub const DUPLICATE_PX: i32 = 6;

/// A rect covering this much of the window is a container wearing an
/// actionable role, not a click target. §4.4. Kept high on purpose: the job
/// is to drop the window-sized `list item` that some views expose, not to
/// second-guess a genuinely large button.
pub const MAX_FRAME_FRACTION: f64 = 0.9;

/// Why a node was dropped. `--dump --rejects` prints these; without them,
/// "detection missed my button" is unanswerable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reject {
    Role,
    NotVisible,
    NotShowing,
    Insensitive,
    /// Zero, negative, or `i32::MIN` — the "no position" sentinels of §4.4.
    Degenerate,
    /// Smaller than `MIN_SIDE`.
    TooSmall,
    /// Outside the window it claims to live in.
    OffFrame,
    /// Covers `MAX_FRAME_FRACTION` or more of the window.
    Container,
    /// Another target already claims this pixel. Two hints on one point are
    /// two keystrokes for one click, and the second one is a lie.
    Duplicate,
    /// Extents could not be read at all — no `Component` interface, or the
    /// object vanished between being listed and being asked. Distinct from
    /// `Degenerate`, which is a rectangle that arrived and was nonsense.
    NoBounds,
}

impl Reject {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Role => "role",
            Self::NotVisible => "not-visible",
            Self::NotShowing => "not-showing",
            Self::Insensitive => "insensitive",
            Self::Degenerate => "degenerate",
            Self::TooSmall => "too-small",
            Self::OffFrame => "off-frame",
            Self::Container => "container",
            Self::Duplicate => "duplicate",
            Self::NoBounds => "no-bounds",
        }
    }
}

pub fn role_is_actionable(role: Role) -> bool {
    ACTIONABLE.contains(&role)
}

/// Do these two rectangles click the same thing?
pub fn same_spot(a: WindowRect, b: WindowRect) -> bool {
    let (ax, ay) = a.centre();
    let (bx, by) = b.centre();
    (ax - bx).abs() <= DUPLICATE_PX && (ay - by).abs() <= DUPLICATE_PX
}

/// Which of two targets on the same spot to keep.
///
/// The smaller rectangle wins: where a toolkit exposes a widget twice, the
/// tighter box is the more specific object, and a click at its centre is the
/// one more likely to land inside both. Ties break on role order and then on
/// path, so the choice is total and reproducible rather than dependent on
/// which arrived first.
pub fn prefer(a: (WindowRect, Role, &str), b: (WindowRect, Role, &str)) -> std::cmp::Ordering {
    let rank = |r: Role| ACTIONABLE.iter().position(|x| *x == r).unwrap_or(usize::MAX);
    a.0.area().cmp(&b.0.area()).then_with(|| rank(a.1).cmp(&rank(b.1))).then_with(|| a.2.cmp(b.2))
}

/// The state half of §4.4, on its own because `Collection.GetMatches` can
/// push exactly this into the server and the `Cache` path cannot — so both
/// paths must agree on what it means, and this is the definition they share.
pub fn states_ok(states: StateSet) -> Result<(), Reject> {
    // VISIBLE and SHOWING. Both. Either alone is a lie: VISIBLE means "not
    // hidden by its own widget flags", SHOWING means "an ancestor is
    // displaying it". A widget on an unselected notebook page passes the
    // first and fails the second.
    if !states.contains(State::Visible) {
        return Err(Reject::NotVisible);
    }
    if !states.contains(State::Showing) {
        return Err(Reject::NotShowing);
    }
    if !states.contains(State::Sensitive) {
        return Err(Reject::Insensitive);
    }
    Ok(())
}

/// The geometry half. `clip` is the window's own bounds; since both rects are
/// window-relative (D13) they are directly comparable, which is the one
/// convenience of not having screen coordinates yet.
///
/// `clip` is `None` when the toplevel's own rectangle cannot be trusted as a
/// boundary — see [`crate::target::Frame::clip`]. The tests that need it are
/// then skipped rather than guessed at: a target that cannot be checked
/// against a boundary is still a target, and dropping it would lose the only
/// actionable thing on the shell.
pub fn bounds_ok(r: WindowRect, clip: Option<WindowRect>) -> Result<(), Reject> {
    // Live counter-examples on this machine, both of which pass VISIBLE and
    // SHOWING: a gnome-terminal `page tab` at `-1,-1 -1x-1`, and a
    // `Profiles` toggle at `-2147483648,-2147483648` — i32::MIN used as a
    // "no position" sentinel. A filter that trusts the state bits ships both.
    if r.w <= 0 || r.h <= 0 {
        return Err(Reject::Degenerate);
    }
    if r.x == i32::MIN || r.y == i32::MIN {
        return Err(Reject::Degenerate);
    }
    if r.w < MIN_SIDE || r.h < MIN_SIDE {
        return Err(Reject::TooSmall);
    }
    let Some(frame) = clip else { return Ok(()) };
    // The centre is what gets clicked, so the centre is what has to be on
    // the window. A half-scrolled row whose top is above the viewport is a
    // real target; one scrolled entirely out of it is not.
    let (cx, cy) = r.centre();
    if cx < frame.x || cy < frame.y || cx >= frame.x + frame.w || cy >= frame.y + frame.h {
        return Err(Reject::OffFrame);
    }
    let frame_area = frame.area();
    if frame_area > 0 && (r.area() as f64) >= (frame_area as f64) * MAX_FRAME_FRACTION {
        return Err(Reject::Container);
    }
    Ok(())
}

/// All of §4.4 at once. `Ok(())` means "label it".
pub fn keep(
    role: Role,
    states: StateSet,
    r: WindowRect,
    clip: Option<WindowRect>,
) -> Result<(), Reject> {
    if !role_is_actionable(role) {
        return Err(Reject::Role);
    }
    states_ok(states)?;
    bounds_ok(r, clip)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: Option<WindowRect> = Some(WindowRect::new(0, 0, 1920, 1048));

    fn states(list: &[State]) -> StateSet {
        let mut s = StateSet::empty();
        for st in list {
            s.insert(*st);
        }
        s
    }

    fn good() -> StateSet {
        states(&[State::Visible, State::Showing, State::Sensitive])
    }

    #[test]
    fn a_plain_button_is_a_target() {
        assert_eq!(keep(Role::Button, good(), WindowRect::new(40, 20, 90, 34), FRAME), Ok(()));
    }

    /// gnome-terminal, measured 2026-08-18: a `page tab` reporting
    /// `-1,-1 -1x-1` while passing VISIBLE *and* SHOWING.
    #[test]
    fn negative_one_page_tab_is_rejected() {
        assert_eq!(
            keep(Role::PageTab, good(), WindowRect::new(-1, -1, -1, -1), FRAME),
            Err(Reject::Degenerate)
        );
    }

    /// gnome-terminal, measured 2026-08-18: a `Profiles` toggle at i32::MIN.
    #[test]
    fn int32_min_sentinel_is_rejected() {
        assert_eq!(
            keep(Role::ToggleButton, good(), WindowRect::new(i32::MIN, i32::MIN, 36, 46), FRAME),
            Err(Reject::Degenerate)
        );
    }

    /// The sentinel must not be reached through arithmetic either — `centre`
    /// on an i32::MIN rect is exactly where an overflow panic would live.
    #[test]
    fn sentinel_rect_does_not_overflow_on_the_way_to_rejection() {
        let r = WindowRect::new(i32::MIN, i32::MIN, i32::MAX, i32::MAX);
        assert!(bounds_ok(r, FRAME).is_err());
        assert_eq!(r.area(), i64::from(i32::MAX) * i64::from(i32::MAX));
    }

    #[test]
    fn each_state_bit_is_load_bearing() {
        let r = WindowRect::new(40, 20, 90, 34);
        for (missing, expect) in [
            (State::Visible, Reject::NotVisible),
            (State::Showing, Reject::NotShowing),
            (State::Sensitive, Reject::Insensitive),
        ] {
            let mut s = good();
            s.remove(missing);
            assert_eq!(keep(Role::Button, s, r, FRAME), Err(expect), "{missing:?}");
        }
    }

    #[test]
    fn a_container_the_size_of_the_window_is_not_a_target() {
        assert_eq!(
            keep(Role::ListItem, good(), WindowRect::new(0, 0, 1920, 1048), FRAME),
            Err(Reject::Container)
        );
    }

    #[test]
    fn a_row_scrolled_out_of_the_viewport_is_not_a_target() {
        assert_eq!(
            keep(Role::ListItem, good(), WindowRect::new(10, -400, 800, 40), FRAME),
            Err(Reject::OffFrame)
        );
    }

    #[test]
    fn a_half_scrolled_row_is_still_a_target() {
        assert_eq!(keep(Role::ListItem, good(), WindowRect::new(10, -10, 800, 40), FRAME), Ok(()));
    }

    /// gnome-shell, measured 2026-08-19: the Clutter stage reports its own
    /// bounds as `0,55 99x56`, so clipping to them throws away `Activities`
    /// — the single actionable thing the shell has. With no trustworthy
    /// clip, the same target survives.
    #[test]
    fn without_a_trustworthy_clip_the_boundary_tests_are_skipped() {
        let activities = WindowRect::new(0, 55, 99, 56);
        let stage = Some(WindowRect::new(0, 55, 99, 56));
        let elsewhere = WindowRect::new(900, 400, 120, 40);
        assert_eq!(keep(Role::ToggleButton, good(), elsewhere, stage), Err(Reject::OffFrame));
        assert_eq!(keep(Role::ToggleButton, good(), elsewhere, None), Ok(()));
        // ... and a degenerate rect is still degenerate without a clip.
        assert_eq!(keep(Role::ToggleButton, good(), WindowRect::new(-1, -1, -1, -1), None), Err(Reject::Degenerate));
        assert_eq!(keep(Role::ToggleButton, good(), activities, None), Ok(()));
    }

    #[test]
    fn two_targets_on_one_pixel_are_one_target() {
        let a = WindowRect::new(174, 0, 34, 34);
        let b = WindowRect::new(174, 0, 34, 34);
        let c = WindowRect::new(1028, 0, 24, 34);
        let d = WindowRect::new(1029, 0, 23, 34);
        let far = WindowRect::new(995, 0, 34, 34);
        assert!(same_spot(a, b), "identical bounds");
        assert!(same_spot(c, d), "a pixel apart is the same spot");
        assert!(!same_spot(c, far), "34 px apart is not");
    }

    #[test]
    fn the_tighter_box_wins_a_tie() {
        let big = (WindowRect::new(0, 0, 40, 40), Role::Button, "/a");
        let small = (WindowRect::new(2, 2, 36, 36), Role::ToggleButton, "/b");
        assert_eq!(prefer(small, big), std::cmp::Ordering::Less);
        // Same box, same role: the path decides, so the answer never depends
        // on iteration order.
        let x = (WindowRect::new(0, 0, 40, 40), Role::Button, "/a");
        let y = (WindowRect::new(0, 0, 40, 40), Role::Button, "/b");
        assert_eq!(prefer(x, y), std::cmp::Ordering::Less);
    }

    #[test]
    fn unknown_roles_are_not_guessed_at() {
        assert_eq!(keep(Role::Filler, good(), WindowRect::new(40, 20, 90, 34), FRAME), Err(Reject::Role));
        assert_eq!(keep(Role::Panel, good(), WindowRect::new(40, 20, 90, 34), FRAME), Err(Reject::Role));
    }
}
