//! Movement mode — the pointer under the keyboard, with the buttons on the
//! other hand (M7, plan.md §12).
//!
//! **The mode a label drops you into.** A hint puts the pointer on a target
//! and clicks it; holding the label's second key puts the pointer on the
//! target and *stops there*, in this. From here `hjkl` corrects the position
//! a few pixels at a time, `s` makes that a sweep, and the left hand carries
//! every button: `f` click, `d` double, `g` triple, `a` middle, `r` right.
//!
//! **`f` is not "click, and separately, drag" — `f` IS the left button**
//! (§12.7). Its press sends the button down and its release sends it up, so
//! a tap is a click and a hold is a drag, and the selection a user makes by
//! holding `f` and sweeping with `l` needs nothing added to make it work.
//! It also retires one of the two questions M7 could not answer from the
//! wire: with the motion tick running underneath it there is no such thing
//! as a drag with no intermediate motion.
//!
//! **Motion is a 90 Hz tick and not key repeat** (§12.5, §12.17). Key repeat
//! does not *start* for ~500 ms, and a user who presses `l` and watches the
//! pointer sit still for half a second has been told the tool is broken. So a
//! press moves once immediately, and a key still held runs the tick. One
//! `MOVE_ABS` per tick however many keys are down — diagonals are the vector
//! sum and cost the same one request — which keeps this inside §3.4's budget
//! with 10/s to spare for the buttons.
//!
//! **90 and not 30, changed 2026-08-27**: the panel here runs at 144 Hz and a
//! pointer that only moves 30 times a second on it is visibly stepping rather
//! than gliding. 30 was never a feel decision — it was §3.4's 50/s ceiling,
//! and that ceiling was raised to 100/s in uictld for this. The *speeds* did
//! not change: every pixel number below was divided by three at the same
//! time, so the pointer covers the same ground per second and covers it in
//! three times as many places.
//!
//! **The slow speed does not accelerate, and that is the departure from
//! `free`** (§12.6). Free mode starts wherever the pointer happens to be and
//! has to cross a screen, so it accelerates. Movement mode starts *on the
//! target*, so its default job is a few pixels of correction — and a
//! correction speed that changes while you hold it is a correction you
//! cannot aim. `s` is where fast lives, and only `s` accelerates.
//!
//! Pure, like the rest of this crate: it is given events and a clock reading
//! and returns commands. It has never heard of uictl, and every test below
//! proves rather than measures.

use crate::detect::Rect;
use crate::free::Dir;
use core::f32::consts::FRAC_1_SQRT_2;
use std::time::Duration;

/// The buttons movement mode can send. Deliberately not `muvor-uictl`'s
/// `Button`: this crate has no dependencies and that is what makes §6's
/// ports a port. The binary maps these three onto the wire's codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Left,
    Middle,
    Right,
}

/// A key movement mode understands. Everything else is ignored, silently and
/// on purpose: under the grab the mode sees every key on the keyboard, and a
/// mode that reacted to keys it has no meaning for would be a mode that
/// cannot be typed near.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// `hjkl`. Held is the normal case, not the exception.
    Move(Dir),
    /// `s` — while held, move fast.
    Fast,
    /// `f` — the left button itself. Tap to click, hold to drag or select.
    LeftButton,
    /// `d`
    DoubleClick,
    /// `g`
    TripleClick,
    /// `a` — **the wheel itself**, not a button that happens to be under
    /// one. Tap it and it is a wheel click; hold it and turn with `hjkl` and
    /// it scrolls, which is what the wheel under a hand does.
    Wheel,
    /// `r`
    RightClick,
    /// `Tab`, right Alt, or Escape.
    Leave,
}

impl Key {
    /// From the keysym *name* the compositor reports, because that is what
    /// the extension forwards and a name survives a modifier or a dead key
    /// where a character does not (§4.5c).
    ///
    /// **Right Alt is two names.** On a plain US layout it is `Alt_R`; on any
    /// layout with a third level — which is most of Europe — the same
    /// physical key reports `ISO_Level3_Shift`. Binding only one of them
    /// would make "press right Alt to leave" work on one machine and do
    /// nothing on the next, which is the kind of fault that gets blamed on
    /// the grab.
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "h" => Self::Move(Dir::Left),
            "j" => Self::Move(Dir::Down),
            "k" => Self::Move(Dir::Up),
            "l" => Self::Move(Dir::Right),
            "s" => Self::Fast,
            "f" => Self::LeftButton,
            "d" => Self::DoubleClick,
            "g" => Self::TripleClick,
            "a" => Self::Wheel,
            "r" => Self::RightClick,
            "Tab" | "Escape" | "Alt_R" | "ISO_Level3_Shift" => Self::Leave,
            _ => return None,
        })
    }
}

/// What happened. `Tick` is the clock; the other two are the grab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ev {
    Down(Key),
    Up(Key),
    Tick,
}

/// What muvor should do about it. One `MoveTo` is one `MOVE_ABS` and one
/// rate-limit token; `Click` is one batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmd {
    MoveTo(i32, i32),
    /// The button goes down and stays down — a drag has started.
    Press(Button),
    Release(Button),
    /// A complete click, `times` of them. `times` is 2 or 3 for `d` and `g`,
    /// and how those are encoded on the wire is §12.8's open measurement,
    /// not this machine's business.
    Click {
        button: Button,
        times: u8,
    },
    /// Wheel notches: `v` positive is up, `h` positive is right — uictl's
    /// own convention, carried through rather than re-invented so nothing
    /// has to remember which end flipped the sign.
    Scroll {
        v: i32,
        h: i32,
    },
    /// The mode is over. Never emitted while a button is still down —
    /// see [`Motion::leave`].
    Leave,
}

/// The four numbers the feel is made of. Separated from the machine so they
/// can be tuned — eventually from M8's config — without touching a line of
/// logic.
#[derive(Debug, Clone, Copy)]
pub struct Params {
    /// How often the tick fires while a direction is held. 90 Hz: §3.4's
    /// budget is 100 requests/second sustained since §12.17 raised it, and
    /// this spends 90 of them. Written in microseconds because 90 Hz is
    /// 11.111 ms and milliseconds would silently round it to 90.9.
    pub tick: Duration,
    /// The slow step, in pixels, and it is constant. **1 px is the floor and
    /// the whole point**: at 90 Hz it is 90 px/s — about what 2 px at 30 Hz
    /// was — and it is also the finest correction the tool can express, which
    /// is what the slow speed is *for*. Crossing the screen is not.
    ///
    /// One consequence, accepted rather than engineered around: a slow
    /// diagonal cannot be scaled by 1/√2 any more, because the scaled step
    /// rounds to 0 and a step of 0 is a pointer that does not move. So a
    /// slow diagonal is √2 faster than a straight line instead of the other
    /// way round. Fixing that properly means a sub-pixel accumulator, which
    /// is a different design and not one 1 px of correction needs.
    pub slow_step: i32,
    /// The first step of a fast sweep. 2.667 px at 90 Hz is 240 px/s, which
    /// is what 8 px at 30 Hz was.
    pub fast_base: f32,
    /// What each fast tick multiplies the step by. 1.046 takes the base to
    /// the cap in about 0.4 s — 36 ticks rather than 30 Hz's 11.5, the same
    /// wall-clock curve — and that curve is the one that lands: full speed
    /// from the first tick overshoots on a tap-and-hold.
    pub fast_growth: f32,
    /// The fastest step. 13.333 px at 90 Hz is 1200 px/s — a screen in 1.6 s,
    /// unchanged from 40 px at 30 Hz.
    pub fast_cap: f32,
    /// Ticks between wheel notches while `a` is held. 12 at 90 Hz is 7.5
    /// notches a second, which is a hand turning a wheel to read — not the
    /// 90 a second the movement tick runs at, which would be unusable.
    pub scroll_slow_ticks: u32,
    /// Ticks between notches with `s` also held: every third tick, 30 a
    /// second, which is page-flipping speed. Still one request per notch and
    /// still well inside §3.4's budget.
    pub scroll_fast_ticks: u32,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            tick: Duration::from_micros(11_111),
            slow_step: 1,
            fast_base: 2.667,
            fast_growth: 1.046,
            fast_cap: 13.333,
            scroll_slow_ticks: 12,
            scroll_fast_ticks: 3,
        }
    }
}

/// The mode. Constructed at the point the label put the pointer on.
#[derive(Debug, Clone)]
pub struct Motion {
    pos: (i32, i32),
    bounds: Rect,
    params: Params,
    /// Up, Down, Left, Right — held state, indexed by [`Motion::slot`]. A set
    /// rather than a single direction because diagonals are free: they are
    /// the vector sum and they cost one `MOVE_ABS` like everything else.
    held: [bool; 4],
    fast: bool,
    /// `f` is down, so the left button is down and this is a drag.
    dragging: bool,
    /// The fast sweep's current step. Reset by anything that changes what is
    /// being asked for — a new direction, `s` going down or up, or every key
    /// coming off — because overshooting has to be correctable immediately.
    ramp: f32,
    /// `a` is down, so `hjkl` turn the wheel instead of moving the pointer.
    wheel: bool,
    /// Whether this press of `a` has actually scrolled anything. It decides
    /// what the *release* means: a wheel that was turned has done its job,
    /// and a wheel that was only pressed is a wheel click.
    wheel_used: bool,
    /// Ticks since the last notch, so scrolling can run slower than the
    /// movement tick without a second clock.
    since_notch: u32,
    /// Which keys are physically down, one bit each ([`Motion::bit`]).
    ///
    /// **A held key repeats, and a repeat is not a press.** Measured on the
    /// real desktop 2026-08-26: the overlay counted **436 presses against 45
    /// releases** in one session of movement mode, so about nine in ten of
    /// the presses muvor sees are the keyboard repeating a key nobody has
    /// let go of. `Move` and `LeftButton` each guarded themselves against
    /// that; `d`, `g` and `r` did not, and they *click on the press* — so a
    /// tap held a fraction too long fired a stream of double clicks, triple
    /// clicks or context menus, at a rate that also spends §3.4's whole
    /// budget in a second. `s` did not either, and reset the fast ramp on
    /// every repeat, which is a sweep that can never accelerate.
    ///
    /// The guard belongs here and not in the four handlers, because "a
    /// repeat is not a press" is a fact about keyboards rather than about
    /// any one key.
    keys_down: u16,
    over: bool,
}

impl Motion {
    pub fn new(pos: (i32, i32), bounds: Rect, params: Params) -> Self {
        let mut m = Self {
            pos,
            bounds,
            params,
            held: [false; 4],
            fast: false,
            dragging: false,
            ramp: params.fast_base,
            wheel: false,
            wheel_used: false,
            since_notch: 0,
            keys_down: 0,
            over: false,
        };
        // Clamped rather than trusted, for `free`'s reason: the compositor's
        // idea of where the pointer is can predate a monitor being unplugged.
        m.pos = clamp(bounds, pos);
        m
    }

    pub const fn pos(&self) -> (i32, i32) {
        self.pos
    }

    /// Whether a button is currently down. The daemon asks before it decides
    /// what a lost grab means.
    pub const fn dragging(&self) -> bool {
        self.dragging
    }

    pub const fn finished(&self) -> bool {
        self.over
    }

    /// Whether `a` is held, so `hjkl` are turning the wheel rather than
    /// moving the pointer.
    /// Whether this machine believes `k` is being held down.
    ///
    /// The driver's question, not the mode's: it is how a bare `KeyDown` for
    /// a key that is already down gets read as a lost `KeyUp` rather than
    /// silently dropped (§12.16, [`Motion::repress`]).
    pub const fn is_down(&self, k: Key) -> bool {
        self.keys_down & Self::bit(k) != 0
    }

    pub const fn scrolling(&self) -> bool {
        self.wheel
    }

    /// How long until the next tick is due, or `None` when nothing is held
    /// and the mode should sleep. **An idle tick must produce no command at
    /// all** — a mode that sent `MOVE_ABS` 30 times a second while the user
    /// thought about it would spend the whole budget on standing still.
    pub const fn wants_tick(&self) -> Option<Duration> {
        if self.over || !self.moving() {
            None
        } else {
            Some(self.params.tick)
        }
    }

    const fn moving(&self) -> bool {
        self.held[0] || self.held[1] || self.held[2] || self.held[3]
    }

    const fn slot(dir: Dir) -> usize {
        match dir {
            Dir::Up => 0,
            Dir::Down => 1,
            Dir::Left => 2,
            Dir::Right => 3,
        }
    }

    /// The direction being asked for, as a unit-ish vector. Opposite keys
    /// held at once cancel, which is what the arithmetic does anyway and is
    /// also the only sane answer.
    const fn vector(&self) -> (i32, i32) {
        let mut v = (0, 0);
        if self.held[0] {
            v.1 -= 1;
        }
        if self.held[1] {
            v.1 += 1;
        }
        if self.held[2] {
            v.0 -= 1;
        }
        if self.held[3] {
            v.0 += 1;
        }
        v
    }

    /// Drive the machine. `_now` is taken for the shape the driver wants and
    /// because a future gesture (a double-tap, a repeat threshold) will need
    /// it; nothing here reads a clock of its own, which is what keeps the
    /// tests provable.
    pub fn event(&mut self, ev: Ev) -> Vec<Cmd> {
        if self.over {
            return Vec::new();
        }
        match ev {
            Ev::Down(k) => {
                // The repeat guard, for every key rather than the two that
                // remembered to write their own.
                if self.keys_down & Self::bit(k) != 0 {
                    return Vec::new();
                }
                self.keys_down |= Self::bit(k);
                self.down(k)
            }
            Ev::Up(k) => {
                self.keys_down &= !Self::bit(k);
                self.up(k)
            }
            Ev::Tick => self.tick(),
        }
    }

    /// One bit per key. `Move` is four of them: the directions are held
    /// independently and a diagonal is two keys down at once.
    const fn bit(k: Key) -> u16 {
        match k {
            Key::Move(Dir::Up) => 1 << 0,
            Key::Move(Dir::Down) => 1 << 1,
            Key::Move(Dir::Left) => 1 << 2,
            Key::Move(Dir::Right) => 1 << 3,
            Key::Fast => 1 << 4,
            Key::LeftButton => 1 << 5,
            Key::DoubleClick => 1 << 6,
            Key::TripleClick => 1 << 7,
            Key::Wheel => 1 << 8,
            Key::RightClick => 1 << 9,
            Key::Leave => 1 << 10,
        }
    }

    fn down(&mut self, k: Key) -> Vec<Cmd> {
        match k {
            Key::Move(d) => {
                let slot = Self::slot(d);
                if self.held[slot] {
                    return Vec::new(); // autorepeat under the grab: already held
                }
                self.held[slot] = true;
                self.ramp = self.params.fast_base;
                if self.wheel {
                    // The wheel is held, so this key turns it. One notch
                    // now, and the tick repeats it — the same shape as a
                    // direction press, one level up.
                    self.since_notch = 0;
                    return self.notch();
                }
                // The press moves once, immediately, at the first step. This
                // is the tap: exact, and the same size whether or not the key
                // is about to be held.
                self.step_now()
            }
            Key::Fast => {
                self.fast = true;
                self.ramp = self.params.fast_base;
                Vec::new()
            }
            Key::LeftButton => {
                if self.dragging {
                    return Vec::new();
                }
                self.dragging = true;
                vec![Cmd::Press(Button::Left)]
            }
            Key::DoubleClick => vec![Cmd::Click { button: Button::Left, times: 2 }],
            Key::TripleClick => vec![Cmd::Click { button: Button::Left, times: 3 }],
            Key::Wheel => {
                // Nothing happens on the press, and that is the whole
                // design: what `a` meant is decided by its *release*. A
                // wheel that was turned has already done its work; a wheel
                // that was only pressed is a wheel click.
                self.wheel = true;
                self.wheel_used = false;
                self.since_notch = 0;
                Vec::new()
            }
            Key::RightClick => vec![Cmd::Click { button: Button::Right, times: 1 }],
            Key::Leave => self.leave(),
        }
    }

    fn up(&mut self, k: Key) -> Vec<Cmd> {
        match k {
            Key::Move(d) => {
                self.held[Self::slot(d)] = false;
                self.ramp = self.params.fast_base;
                Vec::new()
            }
            Key::Fast => {
                self.fast = false;
                self.ramp = self.params.fast_base;
                Vec::new()
            }
            Key::Wheel => {
                if !self.wheel {
                    return Vec::new();
                }
                self.wheel = false;
                if self.wheel_used {
                    // It was turned. Clicking now would mean every scroll
                    // ended with a middle click, which in a browser is a new
                    // tab or a paste.
                    return Vec::new();
                }
                vec![Cmd::Click { button: Button::Middle, times: 1 }]
            }
            Key::LeftButton => {
                if !self.dragging {
                    return Vec::new();
                }
                self.dragging = false;
                // A tap got here milliseconds after the press, which is a
                // click. A hold got here after a sweep, which is a drag. The
                // machine does not need to know which, and that is the point.
                vec![Cmd::Release(Button::Left)]
            }
            _ => Vec::new(),
        }
    }

    fn tick(&mut self) -> Vec<Cmd> {
        if !self.moving() {
            return Vec::new();
        }
        if self.wheel {
            let every = if self.fast {
                self.params.scroll_fast_ticks
            } else {
                self.params.scroll_slow_ticks
            };
            self.since_notch += 1;
            if self.since_notch < every.max(1) {
                return Vec::new();
            }
            self.since_notch = 0;
            return self.notch();
        }
        let cmds = self.step_now();
        if self.fast {
            self.ramp = (self.ramp * self.params.fast_growth).min(self.params.fast_cap);
        }
        cmds
    }

    /// One wheel notch in whatever direction is held.
    ///
    /// **The pointer does not move.** That is the difference between
    /// scrolling and moving, and it is also why scrolling costs no
    /// `MOVE_ABS`: the content comes to the pointer.
    ///
    /// Signs are uictl's (`+v` up, `+h` right) and the y axis flips on the
    /// way, because screen-down is positive and wheel-down is negative.
    fn notch(&mut self) -> Vec<Cmd> {
        let (vx, vy) = self.vector();
        if (vx, vy) == (0, 0) {
            return Vec::new();
        }
        self.wheel_used = true;
        vec![Cmd::Scroll { v: -vy, h: vx }]
    }

    /// One step in the direction currently held. Empty when the pointer is
    /// already against the edge it is being pushed at — an unchanged position
    /// is not worth a request, and at 90 Hz that matters three times over.
    fn step_now(&mut self) -> Vec<Cmd> {
        let (vx, vy) = self.vector();
        if (vx, vy) == (0, 0) {
            return Vec::new();
        }
        #[allow(clippy::cast_possible_truncation)]
        let mut step = if self.fast { self.ramp.round() as i32 } else { self.params.slow_step };
        if vx != 0 && vy != 0 {
            // A diagonal at full step on both axes would be √2 times faster
            // than a straight line, which reads as the pointer speeding up
            // when you add a key.
            #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
            let scaled = ((step as f32) * FRAC_1_SQRT_2).round() as i32;
            step = scaled.max(1);
        }
        let next = clamp(self.bounds, (self.pos.0 + vx * step, self.pos.1 + vy * step));
        if next == self.pos {
            return Vec::new();
        }
        self.pos = next;
        vec![Cmd::MoveTo(next.0, next.1)]
    }

    /// A fresh press of a key this machine already believes is held: its
    /// release was lost, and this is the repair.
    ///
    /// **Only ever call this on positive evidence.** The evidence is
    /// extension v10 and later reporting autorepeat as `KeyRepeat` rather
    /// than as `KeyDown` (§12.16): once the keyboard's own repeats are
    /// named, a second `KeyDown` with no `KeyUp` between cannot be one, and
    /// the only remaining explanation is a release that never arrived. From
    /// an extension that does not draw the distinction this is 30 clicks a
    /// second, which is why [`Motion::event`] still drops the duplicate and
    /// the driver version-gates the call.
    ///
    /// **Where the releases go.** Measured 2026-08-27: the overlay loses the
    /// clutter key focus every time muvor's own click changes which window
    /// is focused, and the keys that land in that window go to the
    /// application. v10 closes it to nothing in the shell; this is what
    /// makes a gap that opens anyway cost one key event instead of that key
    /// for the rest of the mode.
    ///
    /// Let go, then press again — with one exception. `a` releasing without
    /// having scrolled is a **middle click** (§12.10), and a paste or a new
    /// tab that nobody asked for is exactly what §4.5's doctrine spends its
    /// refusals avoiding. A repair is muvor admitting it lost track, so it
    /// pays the safe side of that: the lost release of `a` is worth one
    /// wheel click too few, never one too many.
    pub fn repress(&mut self, k: Key) -> Vec<Cmd> {
        if self.over || !self.is_down(k) {
            return self.event(Ev::Down(k));
        }
        if k == Key::Wheel {
            self.wheel_used = true;
        }
        self.keys_down &= !Self::bit(k);
        let mut out = self.up(k);
        self.keys_down |= Self::bit(k);
        out.extend(self.down(k));
        out
    }

    /// A key this machine believes is held has been found not to be — its
    /// release was lost and nothing is going to press it again, so there is
    /// no `repress` to make. Let go of it, and only that.
    ///
    /// **The witness is silence** (§12.16): a key that was repeating and
    /// stopped has come up. That is the only evidence a key nobody presses
    /// twice ever gets, and it is why this exists separately from
    /// [`Motion::event`] — an `Up` that muvor synthesised must not be able
    /// to do more than an `Up` that arrived.
    ///
    /// **`a` is the exception, for `repress`'s reason.** A wheel released
    /// without having scrolled is a middle click (§12.10), and a repair is
    /// muvor admitting it lost track — so it pays the safe side of that and
    /// sends one wheel click too few, never one too many. Without this the
    /// watchdog's own repair would paste the primary selection into whatever
    /// the pointer was over.
    pub fn lost(&mut self, k: Key) -> Vec<Cmd> {
        if self.over || !self.is_down(k) {
            return Vec::new();
        }
        if k == Key::Wheel {
            self.wheel_used = true;
        }
        self.keys_down &= !Self::bit(k);
        self.up(k)
    }

    /// End a drag without ending the mode, because something outside muvor
    /// already did.
    ///
    /// **uictl's `HOLD_MAX_SEC` is 30** (§3.4): a button held longer than
    /// that is released by the broker's own dead-man timer, correctly and
    /// without telling anyone. A drag that outlives it leaves this machine
    /// believing a button is down that is not — and the next `f` release
    /// would then send an *unpaired* release. So the driver watches the
    /// clock and calls this, which puts muvor back in agreement with
    /// reality and leaves the user in the mode they are still standing in.
    pub fn release_drag(&mut self) -> Vec<Cmd> {
        if !self.dragging || self.over {
            return Vec::new();
        }
        self.dragging = false;
        vec![Cmd::Release(Button::Left)]
    }

    /// Leave, releasing anything still held. **`Release` before `Leave`,
    /// unconditionally** (§12.9): a mode that can hold a mouse button can
    /// leave one down, and a stuck left button on a live desktop is
    /// selecting text and dragging files invisibly.
    pub fn leave(&mut self) -> Vec<Cmd> {
        if self.over {
            return Vec::new();
        }
        self.over = true;
        // A wheel still held when the mode ends is NOT a wheel click. Tab
        // means leave, and a middle click on the way out — a new tab, or a
        // primary-selection paste — is the last thing a user leaving a mode
        // wants. Only a deliberate release of `a` clicks.
        self.wheel = false;
        self.wheel_used = false;
        let mut out = Vec::new();
        if self.dragging {
            self.dragging = false;
            out.push(Cmd::Release(Button::Left));
        }
        out.push(Cmd::Leave);
        out
    }
}

/// The far edge is inside the screen, not one past it — `free` found this
/// with a clamp test after a truncated 1919.6 put the boundary one pixel
/// inside, which is exactly where an edge target lives.
fn clamp(b: Rect, (x, y): (i32, i32)) -> (i32, i32) {
    let max_x = b.x + if b.w > 0 { b.w - 1 } else { 0 };
    let max_y = b.y + if b.h > 0 { b.h - 1 } else { 0 };
    (x.clamp(b.x, max_x), y.clamp(b.y, max_y))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: Rect = Rect::new(0, 0, 1920, 1080);

    fn at(x: i32, y: i32) -> Motion {
        Motion::new((x, y), SCREEN, Params::default())
    }

    fn down(m: &mut Motion, k: Key) -> Vec<Cmd> {
        m.event(Ev::Down(k))
    }
    fn up(m: &mut Motion, k: Key) -> Vec<Cmd> {
        m.event(Ev::Up(k))
    }
    /// The step a fast sweep takes on its first tick — `fast_base` as the
    /// machine rounds it. Written once because five tests assert it and the
    /// literal `8` in them was really "8 px at 30 Hz" (§12.17).
    #[allow(clippy::cast_possible_truncation)]
    fn base_step() -> i32 {
        Params::default().fast_base.round() as i32
    }

    fn tick(m: &mut Motion) -> Vec<Cmd> {
        m.event(Ev::Tick)
    }

    #[test]
    fn the_keymap_is_the_one_in_the_table() {
        use Dir::{Down as D, Left as L, Right as R, Up as U};
        for (name, key) in [
            ("h", Key::Move(L)),
            ("j", Key::Move(D)),
            ("k", Key::Move(U)),
            ("l", Key::Move(R)),
            ("s", Key::Fast),
            ("f", Key::LeftButton),
            ("d", Key::DoubleClick),
            ("g", Key::TripleClick),
            ("a", Key::Wheel),
            ("r", Key::RightClick),
            ("Tab", Key::Leave),
            ("Escape", Key::Leave),
        ] {
            assert_eq!(Key::from_name(name), Some(key), "{name}");
        }
    }

    #[test]
    fn right_alt_is_two_names_and_both_leave() {
        // Alt_R on a US layout, ISO_Level3_Shift anywhere with a third
        // level. Binding one would work on one machine and not the next.
        assert_eq!(Key::from_name("Alt_R"), Some(Key::Leave));
        assert_eq!(Key::from_name("ISO_Level3_Shift"), Some(Key::Leave));
    }

    #[test]
    fn a_key_with_no_meaning_is_ignored_rather_than_refused() {
        for name in ["q", "F5", "Shift_L", "b", "1", ""] {
            assert_eq!(Key::from_name(name), None, "{name}");
        }
    }

    #[test]
    fn a_tap_moves_exactly_the_slow_step() {
        let step = Params::default().slow_step;
        let mut m = at(500, 500);
        assert_eq!(down(&mut m, Key::Move(Dir::Right)), vec![Cmd::MoveTo(500 + step, 500)]);
        assert_eq!(m.pos(), (500 + step, 500));
    }

    #[test]
    fn each_direction_goes_the_way_vim_says() {
        let n = Params::default().slow_step;
        for (dir, want) in [
            (Dir::Left, (500 - n, 500)),
            (Dir::Down, (500, 500 + n)),
            (Dir::Up, (500, 500 - n)),
            (Dir::Right, (500 + n, 500)),
        ] {
            let mut m = at(500, 500);
            down(&mut m, Key::Move(dir));
            assert_eq!(m.pos(), want, "{dir:?}");
        }
    }

    #[test]
    fn the_slow_speed_does_not_accelerate() {
        // The one place this departs from `free`, and the reason is aiming:
        // a correction speed that changes while you hold it cannot be aimed.
        let mut m = at(500, 500);
        down(&mut m, Key::Move(Dir::Right));
        let mut last = m.pos().0;
        for _ in 0..40 {
            tick(&mut m);
            assert_eq!(m.pos().0 - last, Params::default().slow_step);
            last = m.pos().0;
        }
    }

    #[test]
    fn fast_ramps_up_and_stops_at_the_cap() {
        let mut m = at(0, 500);
        down(&mut m, Key::Fast);
        down(&mut m, Key::Move(Dir::Right));
        let mut steps = Vec::new();
        let mut last = m.pos().0;
        // Enough ticks to reach the cap at any tick rate: 36 of them at
        // 90 Hz, 11.5 at 30 Hz, and the assertion below is what checks it.
        for _ in 0..80 {
            tick(&mut m);
            steps.push(m.pos().0 - last);
            last = m.pos().0;
        }
        #[allow(clippy::cast_possible_truncation)]
        let base = Params::default().fast_base.round() as i32;
        assert_eq!(steps[0], base, "the first fast tick is the base step, not the cap");
        assert!(steps.windows(2).take(30).all(|w| w[1] >= w[0]), "must grow: {steps:?}");
        #[allow(clippy::cast_possible_truncation)]
        let cap = Params::default().fast_cap as i32;
        assert!(steps.iter().all(|&s| s <= cap), "nothing may exceed the cap: {steps:?}");
        assert_eq!(*steps.last().unwrap(), cap, "and it must actually reach it");
    }

    #[test]
    fn fast_crosses_the_screen_in_about_one_and_a_half_seconds() {
        // The claim Params makes, checked rather than left in a doc comment
        // where it can rot.
        let mut m = at(0, 500);
        down(&mut m, Key::Fast);
        down(&mut m, Key::Move(Dir::Right));
        let mut ticks = 0;
        while m.pos().0 < 1919 && ticks < 1000 {
            tick(&mut m);
            ticks += 1;
        }
        let secs = ticks as f64 * Params::default().tick.as_secs_f64();
        assert!((1.0..=2.2).contains(&secs), "expected ~1.6 s, took {secs:.2} s");
    }

    #[test]
    fn letting_go_of_s_resets_the_ramp() {
        let mut m = at(0, 500);
        down(&mut m, Key::Fast);
        down(&mut m, Key::Move(Dir::Right));
        for _ in 0..20 {
            tick(&mut m);
        }
        up(&mut m, Key::Fast);
        let before = m.pos().0;
        tick(&mut m);
        assert_eq!(m.pos().0 - before, Params::default().slow_step, "slow again, immediately");
        down(&mut m, Key::Fast);
        let before = m.pos().0;
        tick(&mut m);
        assert_eq!(m.pos().0 - before, base_step(), "and fast starts over from the base");
    }

    #[test]
    fn a_new_direction_resets_the_ramp() {
        let mut m = at(900, 500);
        down(&mut m, Key::Fast);
        down(&mut m, Key::Move(Dir::Right));
        for _ in 0..20 {
            tick(&mut m);
        }
        up(&mut m, Key::Move(Dir::Right));
        let before = m.pos().0;
        down(&mut m, Key::Move(Dir::Left));
        assert_eq!(
            before - m.pos().0,
            base_step(),
            "turning round must be precise again at once"
        );
    }

    /// **The half of this that still holds at 90 Hz is the request count.**
    /// The 1/√2 scaling does not: `slow_step` is 1 px now (§12.17), the
    /// scaled step rounds to 0, and `max(1)` puts it back — so a slow
    /// diagonal moves 1 px on each axis and is √2 *faster* than a straight
    /// line rather than the same speed. That is stated here rather than
    /// asserted away, because the only real fix is a sub-pixel accumulator
    /// and 1 px of correction does not need one. The fast sweep, where the
    /// step is large enough to scale, is checked below.
    #[test]
    fn a_diagonal_is_one_move_and_the_fast_step_is_still_scaled() {
        let mut m = at(500, 500);
        down(&mut m, Key::Move(Dir::Right));
        down(&mut m, Key::Move(Dir::Down));
        let before = m.pos();
        let cmds = tick(&mut m);
        assert_eq!(cmds.len(), 1, "one MOVE_ABS however many keys are held: {cmds:?}");
        let (dx, dy) = (m.pos().0 - before.0, m.pos().1 - before.1);
        let slow = Params::default().slow_step;
        assert_eq!((dx, dy), (slow.max(1), slow.max(1)), "the floor is one pixel, on both axes");

        // Fast, where there are pixels to scale: the diagonal step must be
        // smaller than the straight-line one, or adding a key speeds the
        // pointer up. Measured over a *tick* and not over the presses —
        // every press moves once immediately, so the diagonal's two presses
        // would otherwise be counted as one step.
        let mut straight = at(500, 500);
        down(&mut straight, Key::Fast);
        down(&mut straight, Key::Move(Dir::Right));
        let from = straight.pos().0;
        tick(&mut straight);
        let one_axis = straight.pos().0 - from;

        let mut diag = at(500, 500);
        down(&mut diag, Key::Fast);
        down(&mut diag, Key::Move(Dir::Right));
        down(&mut diag, Key::Move(Dir::Down));
        let from = diag.pos().0;
        tick(&mut diag);
        let both_axes = diag.pos().0 - from;
        assert!(
            both_axes < one_axis,
            "a fast diagonal must be scaled: {both_axes} px per axis vs {one_axis} straight"
        );
    }

    #[test]
    fn opposite_directions_cancel_rather_than_arguing() {
        let mut m = at(500, 500);
        down(&mut m, Key::Move(Dir::Left));
        down(&mut m, Key::Move(Dir::Right));
        assert_eq!(tick(&mut m), vec![], "nothing is being asked for");
    }

    #[test]
    fn an_idle_tick_costs_nothing() {
        // §3.4: 90 requests a second spent on standing still would be
        // most of the budget gone before the user has decided anything.
        let mut m = at(500, 500);
        assert_eq!(m.wants_tick(), None);
        for _ in 0..100 {
            assert_eq!(tick(&mut m), vec![]);
        }
        assert_eq!(m.pos(), (500, 500));
        down(&mut m, Key::Move(Dir::Right));
        assert_eq!(m.wants_tick(), Some(Params::default().tick));
        up(&mut m, Key::Move(Dir::Right));
        assert_eq!(m.wants_tick(), None, "and it goes back to sleep");
    }

    /// **A held key repeats, and a repeat must do nothing.** Measured on the
    /// real desktop: 436 presses to 45 releases in one session (see
    /// `Motion::keys_down`). `d`, `g` and `r` click on the press, so without
    /// this the keyboard alone fires a stream of them.
    #[test]
    fn a_repeated_press_is_not_a_second_press() {
        for (key, first) in [
            (Key::DoubleClick, Cmd::Click { button: Button::Left, times: 2 }),
            (Key::TripleClick, Cmd::Click { button: Button::Left, times: 3 }),
            (Key::RightClick, Cmd::Click { button: Button::Right, times: 1 }),
        ] {
            let mut m = at(500, 500);
            assert_eq!(m.event(Ev::Down(key)), vec![first], "{key:?} on the press");
            for n in 0..30 {
                assert_eq!(
                    m.event(Ev::Down(key)),
                    vec![],
                    "{key:?} repeat {n} clicked again"
                );
            }
            // And the key still works once it has genuinely been let go of.
            m.event(Ev::Up(key));
            assert_eq!(m.event(Ev::Down(key)), vec![first], "{key:?} after a release");
        }
    }

    /// `s` reset the ramp on every press, so autorepeat pinned a fast sweep
    /// to its first step for as long as it was held — the sweep that exists
    /// to cross the screen could never leave 8 px a tick.
    #[test]
    fn a_repeated_s_does_not_pin_the_ramp() {
        let mut m = at(500, 500);
        down(&mut m, Key::Fast);
        down(&mut m, Key::Move(Dir::Right));
        for _ in 0..20 {
            tick(&mut m);
            m.event(Ev::Down(Key::Fast)); // the keyboard, repeating
            m.event(Ev::Down(Key::Move(Dir::Right)));
        }
        let (x, _) = m.pos();
        // **`round`, not `as i32`.** A pinned ramp emits `round(fast_base)`
        // every tick, so that is the number this has to beat. Truncation
        // makes the bar *lower* than a pinned sweep clears — at 90 Hz a base
        // of 2.667 truncates to 2 while the pinned step is 3, and the test
        // would have passed on exactly the fault it was written for.
        #[allow(clippy::cast_possible_truncation)]
        let pinned = 20 * Params::default().fast_base.round() as i32;
        assert!(
            x - 500 > pinned,
            "the sweep never accelerated: {} px in 20 ticks, pinned would be {pinned}",
            x - 500
        );
    }

    #[test]
    fn f_is_the_button_so_a_tap_is_a_click() {
        let mut m = at(500, 500);
        assert_eq!(down(&mut m, Key::LeftButton), vec![Cmd::Press(Button::Left)]);
        assert!(m.dragging());
        assert_eq!(up(&mut m, Key::LeftButton), vec![Cmd::Release(Button::Left)]);
        assert!(!m.dragging());
    }

    #[test]
    fn holding_f_and_moving_is_a_drag_with_real_intermediate_motion() {
        // The claim §12.7 makes: an application that follows the pointer
        // rather than the endpoints sees every step of this.
        let mut m = at(100, 100);
        down(&mut m, Key::LeftButton);
        down(&mut m, Key::Move(Dir::Right));
        let mut moves = 0;
        for _ in 0..10 {
            moves += tick(&mut m).iter().filter(|c| matches!(c, Cmd::MoveTo(..))).count();
        }
        assert_eq!(moves, 10, "every tick of a drag is a position the app can see");
        assert!(m.dragging(), "and the button was down for all of them");
        assert_eq!(up(&mut m, Key::LeftButton), vec![Cmd::Release(Button::Left)]);
    }

    #[test]
    fn s_works_during_a_drag_because_it_only_changes_the_step() {
        let mut m = at(100, 100);
        down(&mut m, Key::LeftButton);
        down(&mut m, Key::Fast);
        down(&mut m, Key::Move(Dir::Right));
        let before = m.pos().0;
        tick(&mut m);
        assert_eq!(m.pos().0 - before, base_step());
        assert!(m.dragging());
    }

    #[test]
    fn the_click_keys_send_the_button_and_the_count() {
        for (key, want) in [
            (Key::DoubleClick, Cmd::Click { button: Button::Left, times: 2 }),
            (Key::TripleClick, Cmd::Click { button: Button::Left, times: 3 }),
            (Key::RightClick, Cmd::Click { button: Button::Right, times: 1 }),
        ] {
            let mut m = at(500, 500);
            assert_eq!(down(&mut m, key), vec![want], "{key:?}");
        }
    }

    #[test]
    fn tapping_a_is_a_wheel_click() {
        // Press does nothing; the release is what decides. A wheel that was
        // never turned is a wheel click.
        let mut m = at(500, 500);
        assert_eq!(down(&mut m, Key::Wheel), vec![]);
        assert!(m.scrolling());
        assert_eq!(up(&mut m, Key::Wheel), vec![Cmd::Click { button: Button::Middle, times: 1 }]);
        assert!(!m.scrolling());
    }

    #[test]
    fn holding_a_turns_hjkl_into_the_wheel() {
        for (dir, want) in [
            (Dir::Up, Cmd::Scroll { v: 1, h: 0 }),
            (Dir::Down, Cmd::Scroll { v: -1, h: 0 }),
            (Dir::Right, Cmd::Scroll { v: 0, h: 1 }),
            (Dir::Left, Cmd::Scroll { v: 0, h: -1 }),
        ] {
            let mut m = at(500, 500);
            down(&mut m, Key::Wheel);
            assert_eq!(down(&mut m, Key::Move(dir)), vec![want], "{dir:?}");
            assert_eq!(m.pos(), (500, 500), "scrolling does not move the pointer");
        }
    }

    #[test]
    fn a_wheel_that_was_turned_does_not_click_when_it_is_let_go() {
        // The one that matters in a browser: a middle click after every
        // scroll is a new tab, or a primary-selection paste.
        let mut m = at(500, 500);
        down(&mut m, Key::Wheel);
        down(&mut m, Key::Move(Dir::Down));
        up(&mut m, Key::Move(Dir::Down));
        assert_eq!(up(&mut m, Key::Wheel), vec![], "it was turned, so it is not a click");
    }

    #[test]
    fn a_held_direction_repeats_notches_slower_than_it_moves() {
        let mut m = at(500, 500);
        down(&mut m, Key::Wheel);
        down(&mut m, Key::Move(Dir::Down));
        let p = Params::default();
        let mut notches = 0;
        for _ in 0..p.scroll_slow_ticks * 10 {
            notches += tick(&mut m).len();
        }
        assert_eq!(notches, 10, "expected the slow scroll rate");
        // And the rate itself, which is the claim `Params` makes: 7.5 notches
        // a second — a hand turning a wheel to read, not the 90 a second the
        // movement tick runs at. Checked here so the two numbers cannot
        // drift apart the next time the tick changes.
        let per_sec = 1.0 / (p.tick.as_secs_f64() * f64::from(p.scroll_slow_ticks));
        assert!((per_sec - 7.5).abs() < 0.3, "slow scroll is {per_sec:.2} notches/s");
        assert_eq!(m.pos(), (500, 500));
    }

    #[test]
    fn s_makes_the_wheel_spin() {
        let mut m = at(500, 500);
        down(&mut m, Key::Wheel);
        down(&mut m, Key::Fast);
        down(&mut m, Key::Move(Dir::Down));
        let p = Params::default();
        let mut notches = 0;
        for _ in 0..p.scroll_fast_ticks * 20 {
            notches += tick(&mut m).len();
        }
        assert_eq!(notches, 20, "expected the fast scroll rate");
        let per_sec = 1.0 / (p.tick.as_secs_f64() * f64::from(p.scroll_fast_ticks));
        assert!((per_sec - 30.0).abs() < 1.0, "fast scroll is {per_sec:.2} notches/s");
        assert!(p.scroll_fast_ticks < p.scroll_slow_ticks, "and it must be the faster one");
    }

    #[test]
    fn letting_go_of_a_hands_the_keys_back_to_the_pointer() {
        let mut m = at(500, 500);
        down(&mut m, Key::Wheel);
        down(&mut m, Key::Move(Dir::Right));
        tick(&mut m);
        assert_eq!(m.pos(), (500, 500));
        up(&mut m, Key::Wheel);
        // `l` is still held, and now it moves again.
        let before = m.pos().0;
        tick(&mut m);
        assert!(m.pos().0 > before, "the direction key goes back to moving the pointer");
    }

    #[test]
    fn pressing_a_mid_sweep_stops_the_pointer_and_starts_scrolling() {
        let mut m = at(500, 500);
        down(&mut m, Key::Move(Dir::Down));
        tick(&mut m);
        let stopped_at = m.pos();
        down(&mut m, Key::Wheel);
        for _ in 0..8 {
            tick(&mut m);
        }
        assert_eq!(m.pos(), stopped_at, "the pointer stays where the wheel took over");
    }

    #[test]
    fn leaving_with_the_wheel_held_does_not_click() {
        // Tab means leave. A middle click on the way out is a new tab or a
        // paste, which is the last thing a user leaving a mode wants.
        let mut m = at(500, 500);
        down(&mut m, Key::Wheel);
        assert_eq!(down(&mut m, Key::Leave), vec![Cmd::Leave]);
    }

    #[test]
    fn the_wheel_costs_no_move_abs_at_all() {
        // §3.4: one request per notch and nothing else, so 30 a second is
        // the ceiling even with `s` held.
        let mut m = at(500, 500);
        down(&mut m, Key::Wheel);
        down(&mut m, Key::Fast);
        down(&mut m, Key::Move(Dir::Up));
        let mut cmds = Vec::new();
        for _ in 0..30 {
            cmds.extend(tick(&mut m));
        }
        assert!(
            cmds.iter().all(|c| matches!(c, Cmd::Scroll { .. })),
            "a scroll must not move the pointer: {cmds:?}"
        );
    }

    #[test]
    fn leaving_mid_drag_releases_the_button_first() {
        // §12.9. A stuck left button is invisible and it is dragging files.
        let mut m = at(500, 500);
        down(&mut m, Key::LeftButton);
        assert_eq!(down(&mut m, Key::Leave), vec![Cmd::Release(Button::Left), Cmd::Leave]);
        assert!(m.finished());
        assert!(!m.dragging());
    }

    #[test]
    fn the_deadman_path_releases_it_too() {
        // `leave()` is the only exit, so the timer, a lost grab and a
        // stopped daemon all go through the same release.
        let mut m = at(500, 500);
        down(&mut m, Key::LeftButton);
        assert_eq!(m.leave(), vec![Cmd::Release(Button::Left), Cmd::Leave]);
    }

    #[test]
    fn a_drag_can_be_ended_from_outside_without_ending_the_mode() {
        // uictl's HOLD_MAX_SEC is 30 s and it releases without telling
        // anyone. Agreeing with it is the only way the next `f` is not an
        // unpaired release.
        let mut m = at(500, 500);
        down(&mut m, Key::LeftButton);
        assert_eq!(m.release_drag(), vec![Cmd::Release(Button::Left)]);
        assert!(!m.dragging());
        assert!(!m.finished(), "the mode is still the mode");
        assert_eq!(m.release_drag(), vec![], "and it does not release twice");
        // **The key is still down**, so what arrives next is autorepeat —
        // and it must not put the button back. Letting go early to agree
        // with uictl's dead-man is worth nothing if the keyboard undoes it
        // 30 ms later (§12.8a).
        assert_eq!(
            m.event(Ev::Down(Key::LeftButton)),
            vec![],
            "autorepeat re-pressed the button the dead-man had just released"
        );
        // The mode still works, and a genuine `f` — released first, as a
        // finger must — starts a new drag rather than sending a release
        // nothing pressed.
        up(&mut m, Key::LeftButton);
        assert_eq!(down(&mut m, Key::LeftButton), vec![Cmd::Press(Button::Left)]);
        assert_eq!(up(&mut m, Key::LeftButton), vec![Cmd::Release(Button::Left)]);
    }

    // ---- §12.16: a release that never arrived --------------------------
    //
    // Every one of these is the same event — a key going down that is
    // already down — and it reaches `repress` only when the shell has
    // promised that autorepeat arrives as autorepeat (extension v10,
    // `Shell::marks_repeats`). `event(Ev::Down(..))` keeps dropping it,
    // which is what an older extension still gets and what the four tests
    // above this block are about.

    #[test]
    fn a_second_press_of_a_key_already_down_repairs_the_lost_release() {
        let mut m = at(500, 500);
        assert_eq!(
            down(&mut m, Key::DoubleClick),
            vec![Cmd::Click { button: Button::Left, times: 2 }]
        );
        // No release: it was delivered to the application while muvor's own
        // click had the keyboard. The old rule made `d` dead for the rest of
        // the mode.
        assert_eq!(m.event(Ev::Down(Key::DoubleClick)), vec![], "the latch is still the default");
        assert_eq!(
            m.repress(Key::DoubleClick),
            vec![Cmd::Click { button: Button::Left, times: 2 }],
            "the repair is the click the user asked for"
        );
        // And the machine is back in step: one release now, not two.
        assert_eq!(up(&mut m, Key::DoubleClick), vec![]);
        assert_eq!(
            down(&mut m, Key::DoubleClick),
            vec![Cmd::Click { button: Button::Left, times: 2 }]
        );
    }

    #[test]
    fn repairing_f_lets_go_of_the_button_before_pressing_it_again() {
        let mut m = at(500, 500);
        assert_eq!(down(&mut m, Key::LeftButton), vec![Cmd::Press(Button::Left)]);
        assert!(m.dragging());
        // **Release first, unconditionally.** uictl keeps held buttons per
        // connection and the daemon keeps one connection for its life
        // (§12.9), so a press on top of a press is `ERR_KEY_ALREADY_HELD`
        // and every click after it is refused.
        assert_eq!(
            m.repress(Key::LeftButton),
            vec![Cmd::Release(Button::Left), Cmd::Press(Button::Left)]
        );
        assert!(m.dragging(), "and the new drag is a drag");
        assert_eq!(up(&mut m, Key::LeftButton), vec![Cmd::Release(Button::Left)]);
    }

    /// The watchdog's repair goes through `lost`, and `lost` owes `a` the
    /// same debt `repress` does: a wheel that muvor merely lost track of must
    /// not paste the primary selection into whatever is under the pointer.
    #[test]
    fn a_lost_wheel_key_does_not_middle_click_on_the_way_out() {
        let mut m = at(500, 500);
        down(&mut m, Key::Wheel);
        assert_eq!(m.lost(Key::Wheel), vec![], "not a click: muvor lost it, the user did not");
        assert!(!m.scrolling(), "and the wheel is no longer held");
        // The pointer keys go straight back to moving the pointer.
        let before = m.pos();
        down(&mut m, Key::Move(Dir::Right));
        assert_ne!(m.pos(), before);
    }

    #[test]
    fn a_lost_f_lets_go_of_the_left_button() {
        let mut m = at(500, 500);
        down(&mut m, Key::LeftButton);
        assert!(m.dragging());
        assert_eq!(m.lost(Key::LeftButton), vec![Cmd::Release(Button::Left)]);
        assert!(!m.dragging());
        assert!(!m.is_down(Key::LeftButton), "and the machine stops believing it is held");
    }

    #[test]
    fn losing_a_key_that_is_not_held_does_nothing() {
        let mut m = at(500, 500);
        assert_eq!(m.lost(Key::LeftButton), vec![]);
        assert_eq!(m.lost(Key::Wheel), vec![]);
        assert!(!m.scrolling(), "and it did not switch the wheel on to switch it off");
    }

    #[test]
    fn repairing_the_wheel_does_not_middle_click() {
        let mut m = at(500, 500);
        down(&mut m, Key::Wheel);
        // `a` released without having scrolled is a middle click (§12.10),
        // which is a paste or a new tab. A repair is muvor admitting it lost
        // track of the keyboard, and it pays the safe side of that: one
        // wheel click too few, never one too many.
        assert_eq!(m.repress(Key::Wheel), vec![], "a repair must not click");
        assert!(m.scrolling(), "and `a` is held again");
        // A deliberate release still clicks, because that one was witnessed.
        assert_eq!(
            up(&mut m, Key::Wheel),
            vec![Cmd::Click { button: Button::Middle, times: 1 }]
        );
    }

    #[test]
    fn a_repaired_direction_is_one_step_and_not_a_second_one() {
        let mut m = at(500, 500);
        let step = Params::default().slow_step;
        assert_eq!(down(&mut m, Key::Move(Dir::Right)), vec![Cmd::MoveTo(500 + step, 500)]);
        assert_eq!(m.repress(Key::Move(Dir::Right)), vec![Cmd::MoveTo(500 + 2 * step, 500)]);
        assert!(m.wants_tick().is_some(), "and it is still held, so it still sweeps");
        up(&mut m, Key::Move(Dir::Right));
        assert!(m.wants_tick().is_none());
    }

    #[test]
    fn repressing_a_key_that_is_not_down_is_an_ordinary_press() {
        let mut m = at(500, 500);
        assert_eq!(
            m.repress(Key::RightClick),
            vec![Cmd::Click { button: Button::Right, times: 1 }]
        );
    }

    #[test]
    fn nothing_is_repaired_after_the_mode_is_over() {
        let mut m = at(500, 500);
        down(&mut m, Key::LeftButton);
        m.leave();
        assert_eq!(m.repress(Key::LeftButton), vec![], "the mode is over");
    }

    #[test]
    fn the_machine_says_which_keys_it_believes_are_down() {
        let mut m = at(500, 500);
        assert!(!m.is_down(Key::Fast));
        down(&mut m, Key::Fast);
        assert!(m.is_down(Key::Fast), "the driver's only way to spot a lost release");
        up(&mut m, Key::Fast);
        assert!(!m.is_down(Key::Fast));
    }

    #[test]
    fn a_release_after_an_outside_release_sends_nothing() {
        let mut m = at(500, 500);
        down(&mut m, Key::LeftButton);
        m.release_drag();
        assert_eq!(up(&mut m, Key::LeftButton), vec![], "the button is already up");
    }

    #[test]
    fn leaving_without_a_drag_just_leaves() {
        let mut m = at(500, 500);
        assert_eq!(down(&mut m, Key::Leave), vec![Cmd::Leave]);
    }

    #[test]
    fn nothing_happens_after_the_mode_is_over() {
        let mut m = at(500, 500);
        m.leave();
        assert_eq!(down(&mut m, Key::Move(Dir::Right)), vec![]);
        assert_eq!(down(&mut m, Key::LeftButton), vec![]);
        assert_eq!(tick(&mut m), vec![]);
        assert_eq!(m.leave(), vec![], "and leaving twice does not release twice");
        assert_eq!(m.pos(), (500, 500));
    }

    #[test]
    fn autorepeat_under_the_grab_does_not_double_the_press() {
        // Under a modal grab a held key repeats, so the same Down arrives
        // again. It must not re-press the button or reset the ramp.
        let mut m = at(500, 500);
        assert_eq!(down(&mut m, Key::LeftButton), vec![Cmd::Press(Button::Left)]);
        assert_eq!(down(&mut m, Key::LeftButton), vec![], "still one button, still down");
        down(&mut m, Key::Fast);
        down(&mut m, Key::Move(Dir::Right));
        for _ in 0..10 {
            tick(&mut m);
        }
        let before = m.pos().0;
        assert_eq!(down(&mut m, Key::Move(Dir::Right)), vec![], "a repeat is not a new press");
        assert_eq!(m.pos().0, before, "and it does not move on its own");
    }

    #[test]
    fn a_release_of_a_key_that_was_never_pressed_is_harmless() {
        // The grab can be entered with a key already down, so the first
        // event a mode ever sees can be an Up.
        let mut m = at(500, 500);
        assert_eq!(up(&mut m, Key::Move(Dir::Left)), vec![]);
        assert_eq!(up(&mut m, Key::LeftButton), vec![]);
        assert_eq!(up(&mut m, Key::Fast), vec![]);
        assert_eq!(m.pos(), (500, 500));
    }

    #[test]
    fn the_pointer_cannot_leave_the_screen() {
        let mut m = at(10, 10);
        down(&mut m, Key::Fast);
        down(&mut m, Key::Move(Dir::Left));
        down(&mut m, Key::Move(Dir::Up));
        for _ in 0..200 {
            tick(&mut m);
        }
        assert_eq!(m.pos(), (0, 0));
        let mut m = at(1900, 1070);
        down(&mut m, Key::Fast);
        down(&mut m, Key::Move(Dir::Right));
        down(&mut m, Key::Move(Dir::Down));
        for _ in 0..200 {
            tick(&mut m);
        }
        assert_eq!(m.pos(), (1919, 1079), "the far edge is inside the screen");
    }

    #[test]
    fn a_pointer_against_the_edge_stops_spending_requests() {
        let mut m = at(1919, 500);
        down(&mut m, Key::Move(Dir::Right));
        for _ in 0..30 {
            assert_eq!(tick(&mut m), vec![], "an unchanged position is not worth a request");
        }
    }

    #[test]
    fn a_starting_position_outside_the_screen_is_clamped() {
        let m = Motion::new((5000, -20), SCREEN, Params::default());
        assert_eq!(m.pos(), (1919, 0));
    }
}
