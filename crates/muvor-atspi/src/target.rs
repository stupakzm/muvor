//! What detection produces, and where it came from.

use atspi::Role;

/// A rectangle **in window-relative pixels** (D13, plan.md §4.2a).
///
/// There is no screen-coordinate rectangle in this crate, and that is not an
/// omission. On Wayland a client is never told its own position, so both of
/// AT-SPI's coordinate types answer in the toplevel's space and adding the
/// window origin stays the compositor's job — which means the extension's
/// (M4). Naming the type `WindowRect` is the cheapest way to stop a
/// window-relative number being passed to something that wants a screen one.
///
/// **The two coordinate types are not interchangeable, which this comment
/// used to claim they were.** They agree on ordinary content and disagree
/// inside a popup, where `Window` is relative to the popup surface and
/// `Screen` is relative to the toplevel. muvor reads `Screen` for that
/// reason (§5.1f, and `bus::extents` carries the measurement); what arrives
/// here is toplevel-relative either way, which is what this type means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl WindowRect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }

    /// `i64` because a degenerate rect can carry `i32::MIN` (§4.4) and this
    /// must not be the thing that panics on it.
    pub fn area(&self) -> i64 {
        i64::from(self.w) * i64::from(self.h)
    }

    /// The point muvor would click. D11's "centre target" is this same rule
    /// applied to a whole window, so it lives here rather than in the caller.
    pub fn centre(&self) -> (i32, i32) {
        (self.x + self.w / 2, self.y + self.h / 2)
    }
}

impl From<(i32, i32, i32, i32)> for WindowRect {
    fn from((x, y, w, h): (i32, i32, i32, i32)) -> Self {
        Self::new(x, y, w, h)
    }
}

impl std::fmt::Display for WindowRect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{},{} {}x{}", self.x, self.y, self.w, self.h)
    }
}

/// Which mechanism produced a target.
///
/// Every `Target` carries this (§4.4). It is what makes `--dump` debuggable,
/// what tells validate-at-action (§4.5) how much it can trust a node, and
/// what turns "does detection work in app X" from an opinion into a
/// measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    /// The live tree mirror (§4.2-iii) — a map lookup, no round trip. The
    /// normal path once the window has been activated with muvor running.
    Mirror,
    /// `Collection.GetMatches` — the fast path. GTK3 and Clutter. 0.2–2.7 ms.
    Collection,
    /// `Cache.GetItems` + in-process filter. GTK4 has no `Collection`. 1.3–4.3 ms.
    Cache,
    /// Recursive `GetChildren` walk. **Reference implementation only** (§4.2):
    /// it exists to test the other two against, and is never a shipping path.
    Walk,
}

impl Provenance {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Mirror => "mirror",
            Self::Collection => "collection",
            Self::Cache => "cache",
            Self::Walk => "walk",
        }
    }
}

/// How the focused window was identified.
///
/// **Measured 2026-08-19, and it is why this enum exists:** on GNOME 48
/// Wayland *no* GTK frame carries `STATE_ACTIVE`. Nautilus, Terminal and the
/// shell all report `VISIBLE|SHOWING|SENSITIVE` and nothing that says "this
/// is the one you are typing into". The textbook way to find the focused
/// window does not work here.
///
/// `window:activate` **does** fire, unlike the mouse events of §3.5 — so
/// focus is tracked as a running fact rather than asked for as a state bit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusSource {
    /// A cached `window:activate`. The normal path.
    Activate,
    /// `STATE_ACTIVE` on the frame. Never seen on GNOME 48 Wayland; kept
    /// because it is correct on X11 and on toolkits that do set it, and it
    /// is the only thing that works before the first activation is seen.
    ActiveState,
    /// Named on the command line with `--window`. A testing affordance, not
    /// a product path: it is how `--dump` can be pointed at Nautilus,
    /// Terminal and Chromium in turn without alt-tabbing between them.
    Named,
    /// The compositor said so — the extension's `FocusedWindow`, matched
    /// back onto the accessibility tree by pid and title (M4/M5).
    ///
    /// **This is the product path**, and the reason the other three are now
    /// fallbacks: mutter always knows which window has focus, immediately
    /// and without ambiguity, where AT-SPI on this desktop knows it only if
    /// it happened to be watching when focus moved. A one-shot `muvor hint`
    /// has watched nothing, which is exactly the case `Activate` cannot
    /// serve.
    Compositor,
}

impl FocusSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Activate => "window:activate",
            Self::ActiveState => "STATE_ACTIVE",
            Self::Named => "--window",
            Self::Compositor => "compositor",
        }
    }
}

/// The application window detection is scoped to (§4.3).
#[derive(Debug, Clone)]
pub struct Frame {
    /// How this window was decided to be the focused one.
    pub focus: FocusSource,
    /// Whether the pid → connection memo answered, or the bus had to be
    /// walked application by application (§2.5, M5s-c).
    ///
    /// Reported because the `frame` timing swings between ~1 ms and ~6 ms
    /// and the two causes look identical from outside: a memo that missed,
    /// and an application with several toplevels that had to be picked
    /// between. A measurement that cannot say which is a measurement that
    /// has to be repeated, and repeating this one costs a person a keystroke.
    pub pid_memo: bool,
    /// Whether the *title* memo answered instead.
    ///
    /// A separate fact from `pid_memo` because it is a separate path with a
    /// separate cost. Some applications never match by pid at all — the
    /// compositor reports Nautilus's window under a pid no accessibility
    /// connection owns (§4.2-ii-c) — so they fall to the title fallback on
    /// **every** hint, and the fallback walks every window of every
    /// application. Measured 2026-08-19: that is 8–19 ms of a 15 ms target,
    /// and it never warms up, which is exactly what a missing memo looks
    /// like from outside.
    pub title_memo: bool,
    /// The application's accessible name — "Files", "Terminal".
    pub app: String,
    /// The window's own accessible name, i.e. usually its title.
    pub title: String,
    /// `Frame` for an ordinary window; the shell's toplevel is a `Window`.
    pub role: Role,
    /// D-Bus destination of the owning application.
    pub bus_name: String,
    /// Object path of the frame within that application.
    pub path: String,
    /// The frame's own bounds. Window-relative, so `x`/`y` are near-zero and
    /// carry no information; `w`/`h` are the window size, and those are real
    /// — for an actual window. See [`Self::clip`].
    pub bounds: WindowRect,
}

impl Frame {
    /// The rectangle targets must fall inside, or `None` when this toplevel
    /// has no trustworthy one.
    ///
    /// **Measured 2026-08-19:** gnome-shell's Clutter stage is role `window`
    /// (not `frame`) and reports its bounds as `0,55 99x56` — the size of the
    /// `Activities` button, not of the screen it covers. Clipping to that
    /// rejects `Activities` itself as `off-frame`, which is to say it throws
    /// away the only actionable thing the shell has.
    ///
    /// So the clip is trusted only for a real application `frame`. Everything
    /// else keeps the degenerate-rectangle tests of §4.4 and skips the two
    /// that need a boundary. The shell is an explicit mode, not the normal
    /// path (§4.3), and a real screen rectangle arrives with M4 anyway.
    pub fn clip(&self) -> Option<WindowRect> {
        (self.role == Role::Frame && self.bounds.w > 0 && self.bounds.h > 0).then_some(self.bounds)
    }
}

/// One clickable thing.
#[derive(Debug, Clone)]
pub struct Target {
    pub role: Role,
    /// May be empty. M3 labels by position, not by name, so an unnamed target
    /// is still perfectly labellable — the name is for `--dump` and for §4.5.
    pub name: String,
    pub bounds: WindowRect,
    pub provenance: Provenance,
    /// Identity for re-validation before the click (§4.5).
    pub bus_name: String,
    pub path: String,
}
