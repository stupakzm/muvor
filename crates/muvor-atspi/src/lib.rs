//! Detection: read the accessibility tree of the focused window into targets.
//!
//! This is muvor's answer to "where are the buttons". It never draws, never
//! clicks and never talks to uictl — it produces [`Target`]s and the
//! provenance of each, and stops there.
//!
//! Two things about it are unusual and both are forced by the platform:
//!
//! 1. **Every rectangle here is window-relative** ([`WindowRect`], D13 /
//!    plan.md §4.2a). AT-SPI on Wayland reports identical numbers for
//!    `SCREEN` and `WINDOW` because a Wayland client is never told its own
//!    position. Screen coordinates do not exist until the extension supplies
//!    the window origin (M4).
//! 2. **All D-Bus work happens on one dedicated thread** (D4). [`Detector`]
//!    is a blocking handle; the hotkey path stays plain synchronous code.
//!
//! `plan.md` §4 is normative. Section numbers in comments refer to it.

pub mod filter;
pub mod mirror;
pub mod probe;
pub mod rule;
pub mod target;
pub mod verify;

mod bus;
mod pipeline;

use std::sync::mpsc;
use std::time::Duration;

pub use atspi::Role;
pub use filter::Reject;
pub use mirror::{AppStats, MirrorStats, WarmSource};
pub use probe::{Rejected, Scan, Warmth};
pub use target::{FocusSource, Frame, Provenance, Target, WindowRect};
pub use verify::{Claim, Step, Verdict, Verified};

/// How long a request to the detection thread may go unanswered.
///
/// Generously above anything measured — the slowest real answer on this
/// machine is Nautilus's cold walk at ~90 ms (§4.2-ii-i) — because this is
/// not a latency budget. It is the line between "slow" and "never", and
/// crossing it produces a sentence rather than a frozen desktop.
pub const REPLY_DEADLINE: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub enum Error {
    /// The accessibility bus said no, or said nothing intelligible.
    Bus(String),
    /// A single D-Bus call exceeded its budget — an app that stopped
    /// answering, not a protocol error.
    Timeout(&'static str),
    /// The D-Bus thread is gone.
    Disconnected,
    /// The D-Bus thread is *alive* and did not answer inside
    /// [`REPLY_DEADLINE`].
    ///
    /// A distinct fault from [`Self::Disconnected`], and the one M5s-d was
    /// written for: a thread that has died drops its sender and the wait
    /// ends by itself, but a thread stuck awaiting a reply that will never
    /// come leaves the caller blocked forever. §4.2-ii is where that stall
    /// was first measured; a daemon does thousands of round trips where the
    /// one-shot did tens, so "forever" is a real outcome rather than a
    /// theoretical one.
    Stalled(&'static str),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bus(m) => write!(f, "at-spi: {m}"),
            Self::Timeout(what) => write!(f, "at-spi: '{what}' timed out; the application is not answering"),
            Self::Disconnected => write!(f, "at-spi: detection thread is gone"),
            Self::Stalled(what) => write!(
                f,
                "at-spi: '{what}' did not answer in {}s — the accessibility bus or the \
                 application behind it has stopped responding. muvor is not stuck; it \
                 gave up on purpose (§4.2-ii)",
                REPLY_DEADLINE.as_secs()
            ),
        }
    }
}

impl std::error::Error for Error {}

/// A measurement attached to whatever produced it. §5.2 says every milestone
/// from M2 onward is measured, so nothing in this crate returns a bare value.
#[derive(Debug)]
pub struct Timed<T> {
    pub value: T,
    pub elapsed: Duration,
}

impl<T> Timed<T> {
    pub fn ms(&self) -> f64 {
        self.elapsed.as_secs_f64() * 1000.0
    }
}

/// The blocking handle to detection.
///
/// Construction connects to the accessibility bus and fails if it is not
/// there, so "a11y bus missing" is a startup diagnostic rather than a
/// mysteriously empty hint set.
pub struct Detector {
    tx: bus::Sender,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Detector {
    pub fn connect() -> Result<Self, Error> {
        let (tx, thread) = bus::spawn()?;
        Ok(Self { tx, thread: Some(thread) })
    }

    /// The active window.
    ///
    /// `None` has three causes worth telling apart, and only the first is a
    /// real absence: nothing is focused; the focused surface has no
    /// accessibility tree (which is what D12's free-mode fallback exists
    /// for); or no `window:activate` has been seen *yet* and this desktop
    /// does not set `STATE_ACTIVE` — measured true of GNOME 48 Wayland. A
    /// long-lived process crosses the third case on the first window switch;
    /// a one-shot CLI never does, which is what [`Self::frame_named`] is for
    /// until M4 makes the question moot.
    pub fn focused_frame(&self) -> Result<Timed<Option<Frame>>, Error> {
        match self.request(bus::Request::FocusedFrame)? {
            (bus::Payload::Frame(f), elapsed) => Ok(Timed { value: f, elapsed }),
            _ => Err(Error::Bus("wrong reply for FocusedFrame".into())),
        }
    }

    /// The first window whose application name or title contains `query`,
    /// case-insensitively. A testing affordance — see [`FocusSource::Named`].
    pub fn frame_named(&self, query: &str) -> Result<Timed<Option<Frame>>, Error> {
        match self.request(bus::Request::NamedFrame(query.to_owned()))? {
            (bus::Payload::Frame(f), elapsed) => Ok(Timed { value: f, elapsed }),
            _ => Err(Error::Bus("wrong reply for NamedFrame".into())),
        }
    }

    /// The window the compositor says is focused (§4.3a) — the product
    /// path, and the one M5 uses.
    ///
    /// `title` and `pid` come from the extension's `FocusedWindow`, in the
    /// same call that supplied the origin. This replaces both fallbacks:
    /// [`Self::focused_frame`] cannot work in a one-shot process on GNOME 48
    /// Wayland (no `STATE_ACTIVE`, and no activation seen yet — D14) and
    /// [`Self::frame_named`] was only ever a testing affordance.
    ///
    /// The returned frame's [`FocusSource`] says which half of the identity
    /// carried it: `Compositor` when the pid matched an application on the
    /// accessibility bus, `Named` when only the title did.
    /// `size` is the compositor's frame rectangle, and it is a second
    /// discriminator rather than a nicety: titles change between the hotkey
    /// and the lookup — gnome-terminal's carries a spinner glyph — and two
    /// windows of one application are rarely the same size to the pixel.
    /// Pass `(0, 0)` when it is not known.
    pub fn frame_for_window(
        &self,
        title: &str,
        pid: u32,
        size: (i32, i32),
    ) -> Result<Timed<Option<Frame>>, Error> {
        let request =
            bus::Request::CompositorFrame { title: title.to_owned(), pid, size };
        match self.request(request)? {
            (bus::Payload::Frame(f), elapsed) => Ok(Timed { value: f, elapsed }),
            _ => Err(Error::Bus("wrong reply for CompositorFrame".into())),
        }
    }

    /// Re-check a target immediately before clicking it (§4.5).
    ///
    /// The one call that stands between a stale tree and a wrong click.
    /// Anything but [`Verdict::Ok`] means **do not inject** — see
    /// [`verify`] for what is checked and why it is two checks.
    pub fn verify(&self, frame: &Frame, claim: &Claim) -> Result<Timed<Verified>, Error> {
        let request = bus::Request::Verify {
            frame: Box::new(frame.clone()),
            claim: Box::new(claim.clone()),
        };
        match self.request(request)? {
            (bus::Payload::Verified(v), elapsed) => Ok(Timed { value: *v, elapsed }),
            _ => Err(Error::Bus("wrong reply for Verify".into())),
        }
    }

    /// Build the mirror for `frame`'s application (§4.2-iii), off the
    /// hotkey path.
    ///
    /// A long-running muvor does this by itself on every `window:activate`.
    /// This is for a one-shot process, which subscribes microseconds before
    /// it asks and so has never seen an activation — the same asymmetry
    /// `--window` exists for.
    pub fn warm(&self, frame: &Frame) -> Result<Timed<Warmth>, Error> {
        match self.request(bus::Request::Warm(Box::new(frame.clone())))? {
            (bus::Payload::Warmed(w), elapsed) => Ok(Timed { value: w, elapsed }),
            _ => Err(Error::Bus("wrong reply for Warm".into())),
        }
    }

    /// What the mirror holds right now, across every application.
    pub fn mirror(&self) -> Result<Timed<MirrorStats>, Error> {
        match self.request(bus::Request::MirrorStats)? {
            (bus::Payload::Mirror(m), elapsed) => Ok(Timed { value: m, elapsed }),
            _ => Err(Error::Bus("wrong reply for MirrorStats".into())),
        }
    }

    /// Actionable nodes the mirror holds for one application, with their
    /// parents — the diagnostic for "the mirror has nodes but finds none in
    /// my window", which is always an ancestry question.
    pub fn mirror_nodes(
        &self,
        bus_name: &str,
        limit: usize,
    ) -> Result<Vec<(String, String, Role, String)>, Error> {
        let request = bus::Request::MirrorNodes { bus_name: bus_name.to_owned(), limit };
        match self.request(request)? {
            (bus::Payload::Nodes(n), _) => Ok(n),
            _ => Err(Error::Bus("wrong reply for MirrorNodes".into())),
        }
    }

    /// Everything clickable in `frame` (§4.2).
    ///
    /// `force` overrides the per-frame probe and is how the three paths are
    /// compared on one window; `None` is the product path. `metadata` asks
    /// for role and name where the source did not supply them — two extra
    /// round trips per node on the `Collection` path, and worth nothing to a
    /// hint, which is placed by position alone.
    pub fn targets(
        &self,
        frame: &Frame,
        force: Option<Provenance>,
        metadata: bool,
    ) -> Result<Timed<Scan>, Error> {
        let request =
            bus::Request::Targets { frame: Box::new(frame.clone()), force, metadata };
        match self.request(request)? {
            (bus::Payload::Scan(s), elapsed) => Ok(Timed { value: *s, elapsed }),
            _ => Err(Error::Bus("wrong reply for Targets".into())),
        }
    }

    fn request(&self, request: bus::Request) -> Result<(bus::Payload, Duration), Error> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        let name = request.name();
        self.tx
            .send(bus::Job { request, reply: reply_tx })
            .map_err(|_| Error::Disconnected)?;
        // Bounded, because the unbounded version is a hang. A thread that
        // has *died* drops its sender and `recv` returns immediately; a
        // thread stuck inside a D-Bus call does neither, and that is the
        // case worth naming rather than waiting out (M5s-d).
        let reply = reply_rx.recv_timeout(REPLY_DEADLINE).map_err(|e| match e {
            mpsc::RecvTimeoutError::Timeout => Error::Stalled(name),
            mpsc::RecvTimeoutError::Disconnected => Error::Disconnected,
        })?;
        reply.payload.map(|p| (p, reply.elapsed))
    }
}

impl Drop for Detector {
    fn drop(&mut self) {
        // Dropping the sender ends the thread's recv loop; joining it means a
        // dropped Detector leaves no connection behind on the a11y bus.
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let dead = std::mem::replace(&mut self.tx, tx);
        drop(dead);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
