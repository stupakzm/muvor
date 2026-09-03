//! Validate at the point of action (plan.md §4.5).
//!
//! Everything else in this crate answers "where are the buttons". This file
//! answers the only question that is asked after the user has committed:
//! **is the thing under this coordinate still the thing the label promised?**
//!
//! It exists because every other part of the path is a snapshot. Targets were
//! read before the overlay was drawn; the user then took some hundreds of
//! milliseconds to type, and in that time a dialog can open, a list can
//! scroll, a button can grey out, a window can close. A hint drawn over a
//! stale tree is a cosmetic bug. A *click* on a stale tree is the one
//! unacceptable outcome, and this is the last place it can be stopped.
//!
//! Two independent checks, because they fail differently:
//!
//! 1. **Re-read the node** — it still exists, still has the role and name it
//!    was labelled with, is still sensitive and showing, and still covers
//!    the coordinate. Catches the target changing under a stable geometry.
//! 2. **Descend `GetAccessibleAtPoint` from the frame** — the coordinate
//!    resolves to that node, or to something inside it. Catches something
//!    *else* having moved on top of it, which the first check cannot see:
//!    a node knows its own extents, never what is stacked above them.
//!
//! On anything but agreement: **abort, do not click.** A refused click is
//! annoying; a wrong click is unrecoverable.

use atspi::proxy::accessible::AccessibleProxy;
use atspi::proxy::component::ComponentProxy;
use atspi::{zbus, AccessibilityConnection, CoordType, Role, State};

use crate::bus::call;
use crate::target::{Frame, Target, WindowRect};
use crate::Error;

/// How deep the point-descent may go before it is treated as a loop.
///
/// Real trees are 5–12 deep at a button. This is a guard against an
/// application that answers `GetAccessibleAtPoint` with something that is not
/// a descendant — not a tuning parameter.
const MAX_DEPTH: usize = 24;

/// What the label promised, taken from the [`Target`] the user chose.
///
/// A borrowed snapshot rather than the `Target` itself: verification compares
/// *what was drawn* against *what is there now*, so the promise has to be the
/// old copy by construction.
#[derive(Debug, Clone)]
pub struct Claim {
    pub bus_name: String,
    pub path: String,
    pub role: Role,
    pub name: String,
    pub bounds: WindowRect,
    /// The window-relative point about to be clicked. Usually the centre of
    /// `bounds`, but passed explicitly — the thing that must be validated is
    /// the coordinate that will actually be injected, not one recomputed
    /// here from numbers that may already have moved.
    pub x: i32,
    pub y: i32,
}

impl Claim {
    /// The centre of the target — the right claim when nothing else has an
    /// opinion, e.g. `dump` printing what a click *would* do.
    pub fn of(target: &Target) -> Self {
        let (x, y) = target.bounds.centre();
        Self::at(target, x, y)
    }

    /// A claim on an explicit point, which is what `muvor hint` uses: the
    /// point is chosen by placement (§5.1a) and must arrive here intact
    /// rather than be recomputed from bounds.
    pub fn at(target: &Target, x: i32, y: i32) -> Self {
        Self {
            bus_name: target.bus_name.clone(),
            path: target.path.clone(),
            role: target.role,
            name: target.name.clone(),
            bounds: target.bounds,
            x,
            y,
        }
    }
}

/// Why a click was refused, or that it was not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Both checks agree. This is the only value that may be clicked.
    Ok,
    /// The node stopped answering: destroyed, or its application is gone.
    Gone(String),
    /// Still there, but not what was promised.
    Changed(String),
    /// The coordinate resolves to something else — something is on top.
    Covered(Step),
    /// The coordinate resolves to passive decoration *inside* the target: a
    /// label, an icon, a box drawn as part of it. Clickable, and the reason
    /// check 2 says "or to something inside it" (§4.5).
    ///
    /// Separate from `Ok` so `--explain` can say which of the two happened;
    /// both may be clicked.
    Inside(Step),
}

impl Verdict {
    pub const fn is_ok(&self) -> bool {
        matches!(self, Self::Ok | Self::Inside(_))
    }

    /// One line, phrased for a user who is about to be told nothing happened.
    pub fn why(&self) -> String {
        match self {
            Self::Ok => "the target is unchanged".into(),
            Self::Gone(m) => format!("the target is gone ({m})"),
            Self::Changed(m) => format!("the target changed: {m}"),
            Self::Covered(step) => format!("the point now resolves to {step}"),
            Self::Inside(step) => format!("the point resolves to {step}, inside the target"),
        }
    }
}

/// One node the point-descent passed through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// `None` when the node was reached but would not describe itself.
    /// Kept apart from `Role::Invalid`, which is an application saying its
    /// role *is* invalid — a different fact, and one that would otherwise
    /// be indistinguishable from a call that failed.
    pub role: Option<Role>,
    pub name: String,
    pub path: String,
}

impl std::fmt::Display for Step {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.role {
            Some(r) => write!(f, "{r} '{}'  {}", self.name, self.path),
            None => write!(f, "(no answer)  {}", self.path),
        }
    }
}

/// The verdict and the evidence behind it.
#[derive(Debug, Clone)]
pub struct Verified {
    pub verdict: Verdict,
    /// What the point-descent walked through, frame first and deepest last.
    /// Printed by `muvor hint --explain`: when a click is refused, this is
    /// the difference between "muvor is broken" and "a menu was open".
    pub chain: Vec<Step>,
    /// The bounds the node reports *now*.
    pub bounds: Option<WindowRect>,
}

pub(crate) async fn verify(
    conn: &AccessibilityConnection,
    frame: &Frame,
    claim: &Claim,
) -> Result<Verified, Error> {
    let dbus = conn.connection();

    // --- check 1: the node is still what it was ------------------------
    let Ok(proxy) = accessible(dbus, &claim.bus_name, &claim.path).await else {
        return Ok(Verified {
            verdict: Verdict::Gone("no route to the object".into()),
            chain: Vec::new(),
            bounds: None,
        });
    };

    // A destroyed object answers with an error rather than a value, which is
    // why every one of these is a `Gone` and not a propagated `Err`: the
    // application being unable to answer *is* the finding.
    let role = match call("verify role", proxy.get_role()).await {
        Ok(r) => r,
        Err(e) => return Ok(gone(e)),
    };
    let name = match call("verify name", proxy.name()).await {
        Ok(n) => n,
        Err(e) => return Ok(gone(e)),
    };
    let states = match call("verify states", proxy.get_state()).await {
        Ok(s) => s,
        Err(e) => return Ok(gone(e)),
    };
    // The space that answered comes back with the rectangle, because check 2
    // below has to ask its point in the same one. A GTK4 application answers
    // `Screen` with a zero position and hit-tests it with the node at the
    // origin whatever point it is given (`bus::extents_in`), so a `Window`
    // rectangle checked against a `Screen` point refused every click it was
    // ever given.
    let (bounds, space) = match crate::bus::extents_in(dbus, &proxy).await {
        Ok(b) => b,
        Err(e) => return Ok(gone(e)),
    };

    let changed = |m: String| Verified {
        verdict: Verdict::Changed(m),
        chain: Vec::new(),
        bounds: Some(bounds),
    };
    if role != claim.role {
        return Ok(changed(format!("role was {}, is {role}", claim.role)));
    }
    if name != claim.name {
        return Ok(changed(format!("name was '{}', is '{name}'", claim.name)));
    }
    // §4.4's three states, re-asked. A button that greyed out while the
    // overlay was up is the single most likely thing to change, and it is
    // invisible to a geometry check — the rectangle does not move.
    for (state, what) in [
        (State::Showing, "no longer showing"),
        (State::Visible, "no longer visible"),
        (State::Sensitive, "no longer sensitive"),
    ] {
        if !states.contains(state) {
            return Ok(changed(what.into()));
        }
    }
    // Bounds are compared by *containment of the click point*, not by
    // equality. A hover or focus ring can legitimately shift a rectangle by
    // a pixel; what must not have changed is whether the coordinate muvor is
    // about to inject is still inside the thing it was chosen for.
    if !contains(&bounds, claim.x, claim.y) {
        return Ok(changed(format!(
            "bounds were {}, are {bounds} — {},{} is now outside",
            claim.bounds, claim.x, claim.y
        )));
    }

    // --- check 2: the coordinate resolves to it ------------------------
    let mut chain = descend(conn, &frame.bus_name, &frame.path, claim.x, claim.y, space).await?;
    let mut hit = chain.iter().any(|step| step.path == claim.path);

    // The frame will not hit-test a popup drawn above it (§5.1f), so a menu
    // item is refused by a check that never looked at the menu: gnome-terminal
    // answers with the widget underneath and LibreWolf with the page behind.
    // Asking the surface itself is the same question asked of the thing that
    // will actually receive the click.
    //
    // Only reached when the frame's descent missed, so every click that was
    // going to happen anyway pays nothing for it.
    if !hit {
        for root in retry_roots(conn, frame, claim).await {
            let from_root =
                descend(conn, &claim.bus_name, &root, claim.x, claim.y, space).await?;
            if from_root.is_empty() {
                continue;
            }
            // The answer of whichever root could resolve the point is the
            // evidence that matters now, and it is what `--explain` prints
            // whether it cleared the click or refused it.
            chain = from_root;
            if chain.iter().any(|step| step.path == claim.path) {
                hit = true;
                break;
            }
        }
    }
    let verdict = if hit {
        Verdict::Ok
    } else {
        match chain.last() {
            // Not in the chain is not the same as not inside. GTK4 answers
            // `GetAccessibleAtPoint` with the *deepest* descendant in one
            // hop, so the chain arrives one element long and never mentions
            // the ancestor that was claimed — measured on Nautilus's
            // sidebar, where every row resolves straight to its own label
            // and 16 of 27 targets were refused for it. The ancestry is the
            // question the chain cannot answer.
            Some(step) => match ancestry(conn, claim, step).await {
                Inside::Passive => Verdict::Inside(step.clone()),
                Inside::Blocked(control) => Verdict::Covered(control),
                Inside::Elsewhere => Verdict::Covered(step.clone()),
            },
            // The frame itself did not answer, or answered null at a point
            // inside its own window. Neither is a state a click belongs in.
            None => Verdict::Gone("the point resolves to nothing".into()),
        }
    };
    Ok(Verified { verdict, chain, bounds: Some(bounds) })
}

fn gone(e: Error) -> Verified {
    Verified { verdict: Verdict::Gone(e.to_string()), chain: Vec::new(), bounds: None }
}

fn contains(r: &WindowRect, x: i32, y: i32) -> bool {
    x >= r.x && y >= r.y && x < r.x.saturating_add(r.w) && y < r.y.saturating_add(r.h)
}

/// Walk `GetAccessibleAtPoint` down from the frame.
///
/// AT-SPI's `GetAccessibleAtPoint` returns the **direct child** at a point,
/// not the deepest descendant, so reaching a button means calling it once per
/// level. Each call is ~0.1–0.3 ms and a tree is a handful of levels deep,
/// which is what makes §4.5's "one call, ~0.5 ms" true in spirit if not in
/// arithmetic.
///
/// What the ancestry says about a point that resolved to something other
/// than the claim.
enum Inside {
    /// The claim is an ancestor, and everything between it and the point is
    /// decoration. The click lands inside the thing it was chosen for.
    Passive,
    /// The claim is an ancestor, but something that is not decoration sits
    /// between it and the point: a control in its own right, a node with an
    /// `invalid` role, or one that will not say what it is. Clicking would
    /// press *that*, or something dead — Nautilus produces both, an
    /// `Unmount` button inside a sidebar row and an `invalid` inside a
    /// dismissed popover, so neither is hypothetical.
    Blocked(Step),
    /// The claim is not an ancestor at all. Something else is there.
    Elsewhere,
}

/// Walk `Parent` up from the point's deepest answer, looking for the claim.
///
/// Only ever reached when the claim was *not* in the descent chain, which is
/// the refusal path — so its round trips cost nothing on a click that is
/// going to happen anyway, and the trees are shallow (§`MAX_DEPTH`).
///
/// What may sit in between is §4.4's `PASSIVE` list, imported rather than
/// restated, and it is an allow-list on purpose. "Not actionable" is not the
/// same as "decoration": the first version of this walk used it and accepted
/// a click onto a node whose role was `invalid` — a dismissed popover's
/// ghost, which is the exact thing §4.5 was built to refuse. An unknown role
/// blocks, and the cost of that is a refusal, which is the cheap failure.
/// Whether one step of the walk is decoration the click may pass through.
///
/// The role number decides it, except for the one value that means two
/// things: **GTK4 sends role 0 for ARIA's `presentation`**, which AT-SPI has
/// no number for, and 0 is also what a node says when it is a ghost or will
/// not answer. So an `Invalid` is asked its *name* — one extra round trip,
/// on the refusal path only, for the difference between an icon inside a
/// button and something dead (`filter::PASSIVE_ROLE_NAMES`).
async fn passes(dbus: &zbus::Connection, bus: &str, step: &Step) -> bool {
    match step.role {
        Some(r) if crate::filter::PASSIVE.contains(&r) => true,
        Some(Role::Invalid) => {
            let Ok(proxy) = accessible(dbus, bus, &step.path).await else { return false };
            match call("ancestry role name", proxy.get_role_name()).await {
                Ok(n) => crate::filter::PASSIVE_ROLE_NAMES.contains(&n.as_str()),
                Err(_) => false,
            }
        }
        _ => false,
    }
}

/// The same control, exposed twice — `filter::DUPLICATE_PX` from the other end.
///
/// **Measured on Nautilus, 2026-08-26.** GTK4 wraps a menu button in a
/// `button` and a `toggle button` of the same name whose centres are the
/// *same pixel*: `Main Menu` (174,0 34x34 both), `Current Folder Menu`
/// (914,0 35x34 outside, 913,0 36x34 inside) and `View Options`. §4.4 keeps
/// one — `filter::prefer` takes the smaller rectangle, and the outer one is
/// smaller by a pixel of focus ring — and `GetAccessibleAtPoint` returns the
/// other. Both are the same widget and clicking either does the same thing,
/// which is exactly what `DUPLICATE_PX` was measured to mean.
///
/// The name must match and must not be empty: two anonymous boxes on one
/// spot prove nothing, and this is the one place §4.5 accepts a node it did
/// not claim. Bounds are re-read rather than trusted, in the space that
/// answered for them (`bus::extents_in`), because the claim's own rectangle
/// was re-read the same way in check 1.
async fn twin(dbus: &zbus::Connection, bus: &str, step: &Step, claim: &Claim) -> bool {
    if claim.name.is_empty() || step.name != claim.name {
        return false;
    }
    let Ok(proxy) = accessible(dbus, bus, &step.path).await else { return false };
    match crate::bus::extents(dbus, &proxy).await {
        Ok(b) => crate::filter::same_spot(b, claim.bounds),
        Err(_) => false,
    }
}

async fn ancestry(conn: &AccessibilityConnection, claim: &Claim, hit: &Step) -> Inside {
    let dbus = conn.connection();
    let mut bus = claim.bus_name.clone();
    let mut step = hit.clone();

    for _ in 0..MAX_DEPTH {
        // The claim is checked before the role test, because the claim is
        // itself usually actionable — a `list item` is on §4.4's list.
        if step.path == claim.path {
            return Inside::Passive;
        }
        if !passes(dbus, &bus, &step).await {
            // §4.4's duplicate rule, arriving from the other side. Detection
            // drops one of two nodes that click the same spot; the hit-test
            // then answers with the one it dropped, and check 2 sees a
            // control it did not claim sitting on the claim's own pixel.
            if twin(dbus, &bus, &step, claim).await {
                return Inside::Passive;
            }
            return Inside::Blocked(step);
        }

        let Ok(proxy) = accessible(dbus, &bus, &step.path).await else {
            return Inside::Elsewhere;
        };
        let Ok(parent) = call("ancestry parent", proxy.parent()).await else {
            return Inside::Elsewhere;
        };
        let path = parent.path_as_str().to_owned();
        // The root answers with itself or with null; either way the walk is
        // over and the claim was not on it.
        if path.ends_with("/null") || path == step.path {
            return Inside::Elsewhere;
        }
        let next_bus = parent.name_as_str().unwrap_or(&bus).to_owned();
        let (role, name) = match accessible(dbus, &next_bus, &path).await {
            Ok(p) => match call("ancestry role", p.get_role()).await {
                Ok(r) => (Some(r), call("ancestry name", p.name()).await.unwrap_or_default()),
                Err(_) => (None, String::new()),
            },
            Err(_) => (None, String::new()),
        };
        bus = next_bus;
        step = Step { role, name, path };
    }
    Inside::Elsewhere
}

/// The roots to retry the point-descent from, highest first.
///
/// The claim's strict ancestors below the frame. Reached only when the
/// frame's own hit-test missed the claim, which is the popup case: a surface
/// stacked above the window is not part of the widget tree the frame
/// hit-tests, and **both toolkits measured refuse it for different reasons**.
/// gnome-terminal draws its menu on its own surface, so the frame answers
/// with the widget underneath; LibreWolf keeps the menu in the toplevel's
/// coordinates and the frame answers with the page behind it. Neither is
/// reachable by descending from the frame, and neither is wrong to click.
///
/// Highest first, on purpose: the highest ancestor that can still resolve
/// the point is the **strongest** remaining version of check 2, because
/// everything it contains is something it could have answered with instead.
/// Descending from the claim's own parent would nearly always "succeed" and
/// would be worth nearly nothing.
///
/// The claim itself is not a root: a descent from it can only reach its own
/// children, so it can never contain the claim and can never clear it.
async fn retry_roots(conn: &AccessibilityConnection, frame: &Frame, claim: &Claim) -> Vec<String> {
    let dbus = conn.connection();
    let mut roots = Vec::new();
    let mut bus = claim.bus_name.clone();
    let mut path = claim.path.clone();

    for _ in 0..MAX_DEPTH {
        let Ok(proxy) = accessible(dbus, &bus, &path).await else { break };
        let Ok(parent) = call("retry parent", proxy.parent()).await else { break };
        let next = parent.path_as_str().to_owned();
        // The frame is where this started and it already answered.
        if next.ends_with("/null") || next == path || next == frame.path {
            break;
        }
        bus = parent.name_as_str().unwrap_or(&bus).to_owned();
        path = next.clone();
        roots.push(next);
    }
    roots.reverse();
    roots
}

/// **`space` is whichever coordinate type answered for the claim's own
/// extents**, and passing it rather than choosing one here is the whole of
/// §4.5's second check being asked about the same place as its first.
/// `Screen` is what a GTK3 window and a popup answer in, and it is the space
/// D13's arithmetic expects; a GTK4 window implements only `Window` and
/// hit-tests `Screen` with the node at its origin for every point on it
/// (`bus::extents_in` has the measurement).
///
/// The root is explicit because the frame is not always the right one to
/// ask. A popup is a **sibling branch its own frame will not hit-test**:
/// with gnome-terminal's menu open, the frame answers the menu's coordinate
/// with `filler`, the widget underneath, and LibreWolf answers with the page
/// behind the menu. Both are the honest answer to "what of *mine* is there",
/// and both are the wrong question once the thing clicked is on a surface
/// stacked above.
async fn descend(
    conn: &AccessibilityConnection,
    bus_name: &str,
    root: &str,
    x: i32,
    y: i32,
    space: CoordType,
) -> Result<Vec<Step>, Error> {
    let dbus = conn.connection();
    let mut chain = Vec::new();
    let mut bus = bus_name.to_owned();
    let mut path = root.to_owned();

    for _ in 0..MAX_DEPTH {
        let Ok(component) = component(dbus, &bus, &path).await else { break };
        let Ok(next) = call("at-point", component.get_accessible_at_point(x, y, space)).await
        else {
            break;
        };
        let next_path = next.path_as_str().to_owned();
        // A null ObjectRef — path `/org/a11y/atspi/null` — is how an
        // application says "nothing of mine is there". So is being handed
        // back the node just asked, which some toolkits do instead.
        if next_path.ends_with("/null") || next_path == path {
            break;
        }
        let next_bus = next.name_as_str().unwrap_or(&bus).to_owned();
        let (role, name) = match accessible(dbus, &next_bus, &next_path).await {
            Ok(p) => match call("at-point role", p.get_role()).await {
                Ok(r) => (Some(r), call("at-point name", p.name()).await.unwrap_or_default()),
                // Reached, but not answering: a node handed back by
                // `GetAccessibleAtPoint` that will not say what it is.
                // Measured in Nautilus, where a dismissed popover leaves
                // exactly this behind — which is why it is a distinct
                // outcome rather than a default value.
                Err(_) => (None, String::new()),
            },
            Err(_) => (None, String::new()),
        };
        chain.push(Step { role, name, path: next_path.clone() });
        bus = next_bus;
        path = next_path;
    }
    Ok(chain)
}

pub(crate) async fn accessible<'a>(
    dbus: &zbus::Connection,
    bus_name: &str,
    path: &str,
) -> Result<AccessibleProxy<'a>, Error> {
    AccessibleProxy::builder(dbus)
        .destination(bus_name.to_owned())
        .map_err(|e| Error::Bus(format!("destination {bus_name}: {e}")))?
        .path(path.to_owned())
        .map_err(|e| Error::Bus(format!("path {path}: {e}")))?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .map_err(|e| Error::Bus(format!("accessible proxy: {e}")))
}

async fn component<'a>(
    dbus: &zbus::Connection,
    bus_name: &str,
    path: &str,
) -> Result<ComponentProxy<'a>, Error> {
    ComponentProxy::builder(dbus)
        .destination(bus_name.to_owned())
        .map_err(|e| Error::Bus(format!("destination {bus_name}: {e}")))?
        .path(path.to_owned())
        .map_err(|e| Error::Bus(format!("path {path}: {e}")))?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .map_err(|e| Error::Bus(format!("component proxy: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, w: i32, h: i32) -> WindowRect {
        WindowRect::new(x, y, w, h)
    }

    #[test]
    fn containment_is_half_open() {
        let r = rect(10, 10, 100, 20);
        assert!(contains(&r, 10, 10));
        assert!(contains(&r, 109, 29));
        // The far edge belongs to the next pixel, not this rectangle: a
        // centre is always interior, and an inclusive test would accept a
        // coordinate one pixel outside the widget.
        assert!(!contains(&r, 110, 20));
        assert!(!contains(&r, 9, 15));
    }

    #[test]
    fn a_degenerate_rect_contains_nothing() {
        // §4.4 lets `i32::MIN` through as a bounds value; the click check
        // must reject it rather than overflow on it.
        assert!(!contains(&rect(0, 0, 0, 0), 0, 0));
        assert!(!contains(&rect(i32::MIN, i32::MIN, 1, 1), 0, 0));
        assert!(!contains(&rect(0, 0, i32::MAX, i32::MAX), -1, -1));
    }

    #[test]
    fn verdict_is_only_ok_when_it_says_so() {
        assert!(Verdict::Ok.is_ok());
        assert!(!Verdict::Gone("x".into()).is_ok());
        assert!(!Verdict::Changed("x".into()).is_ok());
        assert!(!Verdict::Covered(Step {
            role: Some(Role::Button),
            name: "Cancel".into(),
            path: "/p".into()
        })
        .is_ok());
        // Decoration inside the target is clickable — check 2 has always
        // said "or to something inside it"; until 2026-08-19 it could not
        // see it, because GTK4's one-hop answer leaves no ancestors in the
        // chain to recognise.
        assert!(Verdict::Inside(Step {
            role: Some(Role::Label),
            name: "Videos".into(),
            path: "/p".into()
        })
        .is_ok());
    }

    /// The rule the `Inside` verdict rests on, and it is an allow-list.
    ///
    /// The first version asked "is it actionable?" and accepted everything
    /// else — which accepted `Role::Invalid`, the dismissed-popover ghost
    /// that §4.5 exists to refuse. Nautilus produces every case in this test
    /// in one window.
    #[test]
    fn only_decoration_may_sit_between_a_claim_and_its_click_point() {
        // Measured: a sidebar row resolves to `label` inside `panel`.
        for passive in [Role::Label, Role::Panel, Role::Image, Role::Static] {
            assert!(
                crate::filter::PASSIVE.contains(&passive),
                "{passive} is decoration; blocking it refuses a click that would work"
            );
        }
        // A control in its own right: the click would press this instead.
        for control in [Role::Button, Role::ToggleButton, Role::Link, Role::CheckBox] {
            assert!(
                !crate::filter::PASSIVE.contains(&control),
                "{control} earns its own hint and must block a click aimed past it"
            );
        }
        // Neither decoration nor a control: a node saying it is nothing.
        assert!(
            !crate::filter::PASSIVE.contains(&Role::Invalid),
            "an invalid role is the ghost popover, and must never be clicked through"
        );
    }

    /// A node that will not describe itself blocks, the same as one that
    /// describes itself as nothing. `Step::role` is `None` for exactly that.
    #[test]
    fn a_node_that_answers_nothing_is_not_decoration() {
        let silent: Option<Role> = None;
        assert!(!silent.is_some_and(|r| crate::filter::PASSIVE.contains(&r)));
    }
}
