//! Free mode — driving the pointer by hand when there is nothing to hint
//! (D12, D3, §1).
//!
//! **D3 is "hints only — no grid, ever", and this is how that cost is paid.**
//! A hints-only tool's failure mode is "nothing happens", which a user cannot
//! tell from "broken". D12's answer is not a lettered grid — that is what
//! warpd and mouseless already do — it is a pointer the keyboard can drive
//! directly. `hjkl`, and the pointer goes where you push it.
//!
//! **One move per keypress, not a tick loop, and the rate limiter is why.**
//! §3.4 warned that "a 60 Hz nudge loop must batch or it trips the limiter in
//! under a second" — `interactive` was 50 requests/second sustained. A loop
//! would have to batch, buffer and then reconcile against a pointer that
//! moved underneath it. Moving once per key *event* sidesteps all of it:
//! GNOME's key repeat tops out around 30/s, comfortably inside the budget,
//! and one `MOVE_ABS` is one token. The constraint made the simpler design
//! the correct one.
//!
//! **The ceiling has since moved to 100/s and this has not** (§12.17). It is
//! still the right shape here: free mode has no tick and needs none, because
//! it starts wherever the pointer is and accelerates. Movement mode is the
//! one that ticks, and 90 Hz is what the raised ceiling bought.
//!
//! **Reach comes from acceleration, not frequency.** A fixed step cannot be
//! both precise and fast: 8 px takes 240 presses to cross this screen and
//! 200 px cannot land on a checkbox. So consecutive presses in the same
//! direction grow the step geometrically and anything else resets it —
//! a tap is a tap, a held key is a sweep, and the user does not choose a
//! mode to get either.
//!
//! Pure, like the rest of this crate. It is given a position and a
//! direction and returns a position; it has never heard of uictl.

use crate::detect::Rect;
use std::time::Duration;

/// `hjkl` — vim's four, and shared with movement mode (D19, §12.2).
///
/// **This was `ijkl` until 2026-08-24**, on a real argument: the right
/// hand's home position with the index on `j`, so the hand that just typed a
/// label does not move. The user asked for `hjkl` and vim is the reason, and
/// what is not defensible is *two* direction maps in one product — so free
/// mode moved with movement mode and `ijkl` no longer works. The cost is
/// that the four directions sit one key to the left of where §4.5c put them,
/// paid once; the benefit is that anyone who has used vim, less or a tiling
/// window manager already knows this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Up,
    Down,
    Left,
    Right,
}

impl Dir {
    /// The keys, so that the extension, the CLI and the config all spell
    /// them the same way.
    pub const fn from_key(c: char) -> Option<Self> {
        match c {
            'k' => Some(Self::Up),
            'j' => Some(Self::Down),
            'h' => Some(Self::Left),
            'l' => Some(Self::Right),
            _ => None,
        }
    }

    const fn delta(self) -> (i32, i32) {
        match self {
            Self::Up => (0, -1),
            Self::Down => (0, 1),
            Self::Left => (-1, 0),
            Self::Right => (1, 0),
        }
    }
}

/// How free mode accelerates.
#[derive(Debug, Clone, Copy)]
pub struct Params {
    /// The first step of a run, in pixels. Small enough to land on a
    /// checkbox without overshooting.
    pub base: i32,
    /// What each consecutive press in the same direction multiplies the
    /// step by.
    pub growth: f32,
    /// The largest single step. Bounded so a long run cannot leave the
    /// screen in one press and lose the user entirely.
    pub cap: i32,
    /// A gap longer than this ends the run and resets to `base`. This is
    /// what makes a deliberate tap always precise, however fast the
    /// previous sweep was going.
    pub reset_after: Duration,
}

impl Default for Params {
    /// Chosen against a 1920×1080 screen: 8 px lands on anything clickable,
    /// and 8 → 12 → 18 → 27 → 40 → 60 → 90 → 135 → 202 → 256 crosses the
    /// full width in about fourteen presses, which is under half a second
    /// at GNOME's repeat rate.
    fn default() -> Self {
        Self { base: 8, growth: 1.5, cap: 256, reset_after: Duration::from_millis(300) }
    }
}

/// The pointer, and how fast it is currently going.
#[derive(Debug, Clone)]
pub struct Free {
    pos: (i32, i32),
    bounds: Rect,
    params: Params,
    run: Option<(Dir, i32)>,
}

impl Free {
    /// Start at `pos`, confined to `bounds`. The starting position is
    /// clamped rather than trusted: the compositor's idea of where the
    /// pointer is can predate a monitor being unplugged.
    pub fn new(pos: (i32, i32), bounds: Rect, params: Params) -> Self {
        let mut f = Self { pos, bounds, params, run: None };
        f.pos = f.clamp(pos);
        f
    }

    pub const fn pos(&self) -> (i32, i32) {
        self.pos
    }

    /// The step the *next* press in `dir` would take. Exposed because it is
    /// the only interesting internal state and a user who cannot see it
    /// cannot learn the acceleration.
    pub fn next_step(&self, dir: Dir, since_last: Duration) -> i32 {
        match self.run {
            Some((d, step)) if d == dir && since_last <= self.params.reset_after => {
                #[allow(clippy::cast_possible_truncation)]
                let grown = (step as f32 * self.params.growth).round() as i32;
                grown.min(self.params.cap)
            }
            _ => self.params.base,
        }
    }

    /// Move, and return where the pointer now is.
    ///
    /// `since_last` is the gap since the previous nudge — passed in rather
    /// than read from a clock, so this stays pure and its tests stay
    /// provable rather than timing-dependent.
    pub fn nudge(&mut self, dir: Dir, since_last: Duration) -> (i32, i32) {
        let step = self.next_step(dir, since_last);
        let (dx, dy) = dir.delta();
        self.pos = self.clamp((self.pos.0 + dx * step, self.pos.1 + dy * step));
        self.run = Some((dir, step));
        self.pos
    }

    fn clamp(&self, (x, y): (i32, i32)) -> (i32, i32) {
        (
            x.clamp(self.bounds.x, self.bounds.x + (self.bounds.w - 1).max(0)),
            y.clamp(self.bounds.y, self.bounds.y + (self.bounds.h - 1).max(0)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: Rect = Rect::new(0, 0, 1920, 1080);
    const QUICK: Duration = Duration::from_millis(20);
    const SLOW: Duration = Duration::from_millis(1000);

    fn at(x: i32, y: i32) -> Free {
        Free::new((x, y), SCREEN, Params::default())
    }

    #[test]
    fn hjkl_are_the_four_directions_and_nothing_else_is() {
        assert_eq!(Dir::from_key('k'), Some(Dir::Up));
        assert_eq!(Dir::from_key('j'), Some(Dir::Down));
        assert_eq!(Dir::from_key('h'), Some(Dir::Left));
        assert_eq!(Dir::from_key('l'), Some(Dir::Right));
        // `i` was Up until D19 and must now be nothing, or a user who
        // learned the old map gets silence instead of a correction.
        for c in ['i', 'a', ' ', 'K', '\n'] {
            assert_eq!(Dir::from_key(c), None, "{c:?} is not a direction");
        }
    }

    #[test]
    fn one_press_moves_by_the_base_step() {
        let mut f = at(500, 500);
        assert_eq!(f.nudge(Dir::Right, QUICK), (508, 500));
        assert_eq!(f.pos(), (508, 500));
    }

    #[test]
    fn each_direction_goes_the_way_it_says() {
        assert_eq!(at(500, 500).nudge(Dir::Up, QUICK), (500, 492));
        assert_eq!(at(500, 500).nudge(Dir::Down, QUICK), (500, 508));
        assert_eq!(at(500, 500).nudge(Dir::Left, QUICK), (492, 500));
        assert_eq!(at(500, 500).nudge(Dir::Right, QUICK), (508, 500));
    }

    #[test]
    fn consecutive_presses_accelerate() {
        let mut f = at(0, 500);
        let xs: Vec<i32> = (0..5).map(|_| f.nudge(Dir::Right, QUICK).0).collect();
        // 8, then 12, 18, 27, 41 — each gap larger than the last.
        let steps: Vec<i32> = xs.windows(2).map(|w| w[1] - w[0]).collect();
        assert_eq!(xs[0], 8, "the first press is always the base step");
        assert!(
            steps.windows(2).all(|w| w[1] > w[0]),
            "steps should grow monotonically, got {steps:?}"
        );
    }

    #[test]
    fn a_change_of_direction_resets_the_run() {
        let mut f = at(900, 500);
        for _ in 0..6 {
            f.nudge(Dir::Right, QUICK);
        }
        let before = f.pos();
        // Turning around must be precise again immediately, or overshooting
        // becomes impossible to correct — which is the whole failure of a
        // fixed-large step.
        let after = f.nudge(Dir::Left, QUICK);
        assert_eq!(before.0 - after.0, Params::default().base);
    }

    #[test]
    fn a_pause_resets_the_run() {
        let mut f = at(900, 500);
        for _ in 0..6 {
            f.nudge(Dir::Right, QUICK);
        }
        let before = f.pos();
        let after = f.nudge(Dir::Right, SLOW);
        assert_eq!(
            after.0 - before.0,
            Params::default().base,
            "a deliberate tap after a pause must be precise however fast the sweep was"
        );
    }

    #[test]
    fn the_step_is_capped() {
        let mut f = at(0, 500);
        let mut last = 0;
        for _ in 0..40 {
            let x = f.nudge(Dir::Right, QUICK).0;
            assert!(x - last <= Params::default().cap, "step {} exceeded the cap", x - last);
            last = x;
        }
    }

    #[test]
    fn the_pointer_cannot_leave_the_screen() {
        let mut f = at(10, 10);
        for _ in 0..60 {
            f.nudge(Dir::Left, QUICK);
            f.nudge(Dir::Up, QUICK);
        }
        assert_eq!(f.pos(), (0, 0));
        let mut f = at(1900, 1070);
        for _ in 0..60 {
            f.nudge(Dir::Right, QUICK);
            f.nudge(Dir::Down, QUICK);
        }
        assert_eq!(f.pos(), (1919, 1079), "the far edge is inside the screen, not one past it");
    }

    #[test]
    fn a_starting_position_outside_the_screen_is_clamped() {
        // The compositor can report a pointer on a monitor that has been
        // unplugged since. Trusting it would inject a move off-screen.
        let f = Free::new((5000, -20), SCREEN, Params::default());
        assert_eq!(f.pos(), (1919, 0));
    }

    #[test]
    fn crossing_the_screen_takes_about_fourteen_presses() {
        // The claim `Params::default` makes, checked rather than asserted in
        // a doc comment where it could rot.
        let mut f = at(0, 500);
        let mut n = 0;
        while f.pos().0 < 1919 && n < 100 {
            f.nudge(Dir::Right, QUICK);
            n += 1;
        }
        assert!((10..=18).contains(&n), "expected roughly fourteen presses, took {n}");
    }

    #[test]
    fn next_step_agrees_with_what_nudge_actually_does() {
        let mut f = at(500, 500);
        for _ in 0..8 {
            let predicted = f.next_step(Dir::Right, QUICK);
            let before = f.pos().0;
            let after = f.nudge(Dir::Right, QUICK).0;
            assert_eq!(after - before, predicted, "next_step must not lie about the next step");
        }
    }
}
