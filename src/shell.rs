//! D-Bus to the extension (plan.md §5.5).
//!
//! Six calls and two signals, and the extension holds no product logic —
//! so this module is the entire coupling between muvor and GNOME. A port
//! (§6) replaces the other side of it and nothing else.
//!
//! Blocking, like everything on the main thread (D4): the calls here happen
//! between a keypress and a frame, and an async main loop would buy nothing
//! but a colour on every function in the path.

use std::time::Duration;

const BUS_NAME: &str = "org.muvor.Shell";
const OBJECT_PATH: &str = "/org/muvor/Shell";
const IFACE: &str = "org.muvor.Shell";

/// A call must not outlive the frame it belongs to. The shell answers in
/// well under a millisecond when it is healthy; if it does not, muvor has to
/// carry on without it rather than hang the keyboard.
const CALL_TIMEOUT: Duration = Duration::from_millis(500);

/// Capture is the one call that cannot meet [`CALL_TIMEOUT`], and not because
/// anything is unhealthy: the PNG encode alone is 38-151 ms at 1920x1080
/// (§5.4b), on a thread muvor does not control. This is a bound on a stall,
/// not a budget — the budget is the reason capture is off the hot path.
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug)]
pub enum Error {
    /// The extension is not running, or not installed.
    NotRunning(String),
    Bus(String),
    Timeout,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotRunning(m) => write!(
                f,
                "the muvor shell extension is not answering on {BUS_NAME}: {m}\n  \
                 hint: gnome-extensions enable muvor@muvor.local, and on Wayland a \
                 changed extension needs the session restarted"
            ),
            Self::Bus(m) => write!(f, "shell: {m}"),
            Self::Timeout => write!(f, "shell: timed out"),
        }
    }
}

impl std::error::Error for Error {}

/// One screen-space rectangle and the label drawn on it.
///
/// Screen space, not window-relative: the extension does no arithmetic it
/// can avoid, so muvor adds the origin from [`Shell::focused_window`] before
/// sending (D13).
pub struct Hint {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    /// Where the click will land, in the same screen space.
    ///
    /// Sent separately from the rectangle because the rectangle stopped
    /// describing it: the badge sits beside its target (§5.1a) and dodges
    /// other badges (§5.1c), so its position is no longer a claim about
    /// where the pointer goes. The extension draws this as a cyan dot.
    pub click_x: i32,
    pub click_y: i32,
    pub label: String,
}

/// What the compositor knows and a Wayland client never does.
#[derive(Debug, Clone)]
pub struct Window {
    pub title: String,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    /// The process behind the window. Identity, not decoration: it is how
    /// this window is found again in the accessibility tree, where the only
    /// other handle is a title that is neither unique nor stable.
    pub pid: u32,
    /// `WM_CLASS` — for the user's benefit in diagnostics, and for the
    /// per-application rules §1 leaves to a later version.
    pub wm_class: String,
    /// The **buffer** rect: the window *including* its client-side shadow
    /// (§5.1h, extension v7 and later).
    ///
    /// `x, y, w, h` above is the frame rect, which is the window without the
    /// shadow — and the accessibility frame is measured against the buffer.
    /// Adding a window-relative a11y bound to the frame origin therefore
    /// lands ~26 px out on any window that draws a shadow, and §4.5 cannot
    /// catch it because the claim is built and validated in a11y space and
    /// the error arrives afterwards. **Use [`Window::origin`], never `x, y`,
    /// for the correction.**
    ///
    /// Zero from an older extension, which is why `origin` falls back.
    pub buffer: (i32, i32, i32, i32),
    /// The compositor says this window is minimized (v8, [`Shell::windows`]).
    ///
    /// Always `false` from `FocusedWindow`, which only ever names a window
    /// that is not.
    pub minimized: bool,
    /// The compositor says this window would be showing on its workspace —
    /// which is not the same as "visible", because it says nothing about
    /// what is stacked on top. Occlusion is [`crate::geom`]'s job and the
    /// stacking order is what it needs.
    pub showing: bool,
}

impl Window {
    pub fn is_none(&self) -> bool {
        self.w == 0 && self.h == 0
    }

    /// The corner a window-relative accessibility bound must be added to
    /// (§5.1h).
    ///
    /// The buffer origin when the extension reports one, the frame origin
    /// otherwise — a v6 shell answers with zeros and the old, slightly wrong
    /// arithmetic is still better than an origin at `0,0`.
    pub const fn origin(&self) -> (i32, i32) {
        if self.buffer.2 > 0 && self.buffer.3 > 0 {
            (self.buffer.0, self.buffer.1)
        } else {
            (self.x, self.y)
        }
    }

    /// The size the accessibility frame should agree with — the buffer's,
    /// for the same reason.
    pub const fn a11y_size(&self) -> (i32, i32) {
        if self.buffer.2 > 0 && self.buffer.3 > 0 {
            (self.buffer.2, self.buffer.3)
        } else {
            (self.w, self.h)
        }
    }
}

/// One captured frame, as the compositor encoded it (§5.4b, D16).
///
/// PNG rather than pixels because GJS cannot carry pixels: see
/// [`Shell::capture`]. The bytes are decoded on muvor's side, where a decoder
/// is a dependency rather than a marshalling accident.
#[derive(Debug, Clone)]
pub struct Frame {
    /// PNG bytes, as encoded inside the compositor.
    pub png: Vec<u8>,
    /// The stage rectangle that was captured, in the screen coordinates
    /// every other rectangle on this boundary already uses.
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    /// Image pixels per logical pixel. `1.0` here, and not assumed to be:
    /// GNOME 48 offers fractional scales, so the image can be a different
    /// size from the rectangle it covers, and a target found at an image
    /// pixel has to be divided back down before it can be clicked.
    pub scale: f64,
}

/// What ended a hint session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// A complete label was typed.
    Typed(String),
    /// Escape, or a grab that could not be taken.
    Cancelled,
    /// Nothing arrived before the deadline.
    TimedOut,
    /// Tab: the user has looked at the overlay and wants D18's tier 2 —
    /// the camera — added to it. Extension v5 and later.
    Deepen,
    /// A key going down, by name, while the shell holds the keyboard for
    /// muvor — free mode (§4.5c) or movement mode (§12). Extension v5 and
    /// later; v7 renamed the signal and added the release.
    KeyDown(String),
    /// A key coming up. **v7 and later, and movement mode does not work
    /// without it**: `s` is a speed and `f` is the left mouse button, both
    /// held, so a mode that only heard presses could start a drag it could
    /// never end (§12.4).
    KeyUp(String),
    /// A complete label whose last key was *held* — movement mode (§12.3).
    /// The shell already holds the keyboard when this arrives.
    TypedHold(String),
    /// The keyboard repeating a key that is still down. **Extension v10 and
    /// later**, and the whole point of it is what a [`Self::KeyDown`] means
    /// once this exists: a key going down that muvor already believes is
    /// held can no longer be autorepeat, so it is that key's lost
    /// [`Self::KeyUp`] (§12.16).
    ///
    /// v9 and earlier sent a repeat as an ordinary `KeyDown`, so both halves
    /// had to defend against it by latching — 436 presses to 45 releases
    /// measured on the desk — and the latch is what made one lost release
    /// kill an action key for the rest of the mode.
    KeyRepeat(String),
}

/// One signal from the extension, or `None` for one muvor does not care
/// about.
///
/// Free-standing because there are two callers now and they must not drift:
/// [`Shell::wait`], which is the hint path, and [`Keys::next`], which is
/// movement mode's 90 Hz loop.
fn decode_signal(message: &zbus::Message) -> Result<Option<Outcome>, Error> {
    let header = message.header();
    Ok(match header.member().map(zbus::names::MemberName::as_str) {
        Some("Typed") => {
            let (label,): (String,) = message
                .body()
                .deserialize()
                .map_err(|e| Error::Bus(format!("Typed payload: {e}")))?;
            Some(Outcome::Typed(label))
        }
        Some("TypedHold") => {
            let (label,): (String,) = message
                .body()
                .deserialize()
                .map_err(|e| Error::Bus(format!("TypedHold payload: {e}")))?;
            Some(Outcome::TypedHold(label))
        }
        Some("Cancelled") => Some(Outcome::Cancelled),
        Some("Deepen") => Some(Outcome::Deepen),
        Some("KeyUp") => {
            let (name,): (String,) = message
                .body()
                .deserialize()
                .map_err(|e| Error::Bus(format!("KeyUp payload: {e}")))?;
            Some(Outcome::KeyUp(name))
        }
        // v7 calls it KeyDown; v6 called it Key. Both mean a key went down.
        Some("KeyDown" | "Key") => {
            let (name,): (String,) = message
                .body()
                .deserialize()
                .map_err(|e| Error::Bus(format!("KeyDown payload: {e}")))?;
            Some(Outcome::KeyDown(name))
        }
        // v10. A key going down that the *keyboard* sent, not a hand.
        Some("KeyRepeat") => {
            let (name,): (String,) = message
                .body()
                .deserialize()
                .map_err(|e| Error::Bus(format!("KeyRepeat payload: {e}")))?;
            Some(Outcome::KeyRepeat(name))
        }
        _ => None,
    })
}

/// A signal stream held open across many reads.
///
/// **`Shell::wait` cannot be movement mode's loop**, and the reason is not
/// style: it builds a `MatchRule` and a `MessageStream` per call, which is an
/// `AddMatch` round trip on the way in and a `RemoveMatch` on the way out.
/// At one call per hint that is invisible; at movement mode's 90 Hz it is
/// sixty extra round trips a second underneath a loop whose entire job is to
/// be smooth.
pub struct Keys<'a> {
    shell: &'a Shell,
    /// `Option` only so that [`Drop`] can take it. It is `Some` for the
    /// whole of the value's useful life.
    stream: Option<zbus::MessageStream>,
}

/// **The stream must be dropped inside the runtime, and this is why.**
///
/// `zbus::MessageStream` registered a match rule with the bus, so its `Drop`
/// deregisters one — and with the `tokio` feature that is
/// `Connection::queue_remove_match` -> `Executor::spawn` -> `tokio::task::spawn`,
/// which **panics** when no runtime is entered on the calling thread. Every
/// other stream in this file is built and dropped inside one `block_on`, so
/// none of them can reach it; `Keys` is the only one that outlives its
/// `block_on`, and scope exit therefore ran that spawn on a bare thread.
///
/// It killed the daemon rather than the hint — the panic unwound out of
/// `movement_mode`, systemd restarted the unit, and what the user saw was a
/// hotkey that needed five presses and an `hjkl` that typed into the app
/// because the overlay had gone down with the process.
///
/// `rt.enter()` is the whole fix: it puts the runtime in the thread's context
/// so the spawn lands on the worker thread, which is running (see
/// `Shell::connect` — the runtime is multi-thread for exactly this reason)
/// and so actually delivers the `RemoveMatch`.
impl Drop for Keys<'_> {
    fn drop(&mut self) {
        if let Some(stream) = self.stream.take() {
            let _guard = self.shell.rt.enter();
            drop(stream);
        }
    }
}

impl Keys<'_> {
    /// The next key, or `TimedOut` — **which is the tick**. Movement mode
    /// asks for the tick interval it wants and treats the timeout as the
    /// clock, so there is one loop and not a loop plus a timer thread.
    pub fn next(&mut self, timeout: Duration) -> Result<Outcome, Error> {
        let Some(stream) = self.stream.as_mut() else {
            return Err(Error::Bus("the key stream is closed".to_owned()));
        };
        self.shell.rt.block_on(async {
            let deadline = tokio::time::Instant::now() + timeout;
            loop {
                let next =
                    tokio::time::timeout_at(deadline, futures_lite::StreamExt::next(stream));
                // **Only an elapsed deadline is a tick.** This used to accept
                // an ended stream and a broken message as one too, and the
                // difference is not cosmetic: a stream that has ended yields
                // `None` *immediately and for ever*, so movement mode would
                // read it as a clock running at the speed of the CPU —
                // spending its whole §3.4 budget in milliseconds, and then
                // dying on the refusal that followed. A keyboard that has
                // gone away is the end of the mode, and it has to say so.
                let Ok(message) = next.await else { return Ok(Outcome::TimedOut) };
                let Some(message) = message else {
                    return Err(Error::Bus("the key stream ended".to_owned()));
                };
                // One undecodable message is not the end of the keyboard.
                let Ok(message) = message else { continue };
                if let Some(outcome) = decode_signal(&message)? {
                    return Ok(outcome);
                }
            }
        })
    }
}

pub struct Shell {
    rt: tokio::runtime::Runtime,
    conn: zbus::Connection,
    /// The loaded extension's version, asked once.
    ///
    /// It cannot change while this process lives: a changed extension needs
    /// the session restarted (§5.4a), and that takes muvor with it. Asking
    /// per call was a D-Bus round trip on the hot path for an answer that is
    /// a constant.
    version: std::sync::OnceLock<u32>,
}

impl Shell {
    pub fn connect() -> Result<Self, Error> {
        // **Multi-thread, for one worker.** A current-thread runtime only
        // runs while somebody is inside `block_on`, and this connection has
        // a signal match rule on it (see `wait`). In a one-shot that is
        // harmless — the process is inside `block_on` for most of its life.
        // In the daemon it is the whole cost: between hints nothing drives
        // the runtime, zbus's reader task does not run, incoming traffic
        // queues, and the next call has to push the executor through the
        // backlog before its own reply is seen.
        //
        // `bus.rs` had already found this and fixed it the other way, by
        // receiving jobs *inside* the runtime; its comment describes exactly
        // this stall. A worker thread is the same fix in less code, because
        // it keeps the reader running whether or not anyone is calling.
        //
        // Measured 2026-08-19 on gnome-terminal, daemon, `hint --measure`:
        // 19.81 ms mean before, and see §2.5 for after.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .map_err(|e| Error::Bus(format!("runtime: {e}")))?;
        let conn = rt
            .block_on(zbus::Connection::session())
            .map_err(|e| Error::Bus(format!("session bus: {e}")))?;
        let shell = Self { rt, conn, version: std::sync::OnceLock::new() };
        // Fail here, once, with a fixable message — rather than at the first
        // hotkey with a D-Bus error the user has to interpret.
        shell.focused_window()?;
        Ok(shell)
    }

    pub fn focused_window(&self) -> Result<Window, Error> {
        let reply = self.call("FocusedWindow", &())?;
        // v7 added the buffer rect (§5.1h). Deserialised as the long form
        // first and the short form second rather than switched on `Version`:
        // §5.4h's lesson is that a version gate is a condition that keeps
        // compiling while its meaning changes, and this one can simply try.
        type V7 = (String, i32, i32, i32, i32, i32, String, i32, i32, i32, i32);
        type V6 = (String, i32, i32, i32, i32, i32, String);
        if let Ok((w7,)) = reply.body().deserialize::<(V7,)>() {
            let (title, x, y, w, h, pid, wm_class, bx, by, bw, bh) = w7;
            return Ok(Window {
                title,
                x,
                y,
                w,
                h,
                pid: pid.max(0) as u32,
                wm_class,
                buffer: (bx, by, bw, bh),
                minimized: false,
                showing: true,
            });
        }
        let ((title, x, y, w, h, pid, wm_class),): (V6,) = reply
            .body()
            .deserialize()
            .map_err(|e| Error::Bus(format!("FocusedWindow reply: {e}")))?;
        Ok(Window {
            title,
            x,
            y,
            w,
            h,
            pid: pid.max(0) as u32,
            wm_class,
            buffer: (0, 0, 0, 0),
            minimized: false,
            showing: true,
        })
    }

    /// Every window on the active workspace, **bottom to top in stacking
    /// order** — extension v8.
    ///
    /// §4.3 scopes detection to the focused window because *"AT-SPI has no
    /// concept of occlusion"*: a buried Nautilus toggle still passes
    /// `VISIBLE`, `SHOWING` and `SENSITIVE` with sane bounds, and labelling
    /// it produces a badge that clicks whatever is on top. The same
    /// paragraph names what would fix it — *"only the compositor's stacking
    /// order can [tell you a window is covered], which is one more thing the
    /// extension supplies"* — and this is that call.
    ///
    /// **An older extension has no `Windows`**, and the caller is expected
    /// to fall back to [`Shell::focused_window`] rather than fail: hinting
    /// one window is what muvor did for eleven months and is not an error.
    pub fn windows(&self) -> Result<Vec<Window>, Error> {
        let reply = self.call("Windows", &())?;
        type V8 = (String, i32, i32, i32, i32, i32, String, i32, i32, i32, i32, bool, bool);
        let (rows,): (Vec<V8>,) = reply
            .body()
            .deserialize()
            .map_err(|e| Error::Bus(format!("Windows reply: {e}")))?;
        Ok(rows
            .into_iter()
            .map(|(title, x, y, w, h, pid, wm_class, bx, by, bw, bh, minimized, showing)| Window {
                title,
                x,
                y,
                w,
                h,
                pid: pid.max(0) as u32,
                wm_class,
                buffer: (bx, by, bw, bh),
                minimized,
                showing,
            })
            .collect())
    }

    /// Where the pointer is, in stage coordinates — §3.5's readback channel.
    ///
    /// The only one on this platform: AT-SPI emits no mouse events under
    /// mutter, so without the extension muvor cannot see where it just put
    /// the pointer, and calibration is unverifiable. That is the whole
    /// reason §3.5 said the maths could be written at M1 and only closed
    /// at M4.
    pub fn pointer(&self) -> Result<(i32, i32), Error> {
        let reply = self.call("Pointer", &())?;
        let ((x, y),): ((i32, i32),) = reply
            .body()
            .deserialize()
            .map_err(|e| Error::Bus(format!("Pointer reply: {e}")))?;
        Ok((x, y))
    }

    /// Draw the overlay, marking the click points where the extension can.
    ///
    /// `ShowMarked` arrived with extension version 3. An extension already
    /// loaded into a running session cannot grow a method (§5.4a), so the
    /// version is asked and the old `Show` used when it is older — muvor
    /// keeps working across the restart that the new one needs, in both
    /// directions.
    pub fn show(&self, hints: &[Hint]) -> Result<(), Error> {
        if self.version_at_least(3) {
            let targets: Vec<(i32, i32, i32, i32, i32, i32, &str)> = hints
                .iter()
                .map(|h| (h.x, h.y, h.w, h.h, h.click_x, h.click_y, h.label.as_str()))
                .collect();
            return self.call("ShowMarked", &(targets,)).map(drop);
        }
        let targets: Vec<(i32, i32, i32, i32, &str)> =
            hints.iter().map(|h| (h.x, h.y, h.w, h.h, h.label.as_str())).collect();
        self.call("Show", &(targets,)).map(drop)
    }

    /// The loaded extension's `version`, or 0 for one too old to have one.
    ///
    /// `0.1.0` is what the property answers before `metadata.json` carried a
    /// version at all, and it is not a number — treating it as 0 is what
    /// makes "older than 3" true for it.
    ///
    /// **This was broken from the day it was written, and silently** (found
    /// 2026-08-20). The member was spelled
    /// `"org.freedesktop.DBus.Properties.Get"` while the interface argument
    /// still said `org.muvor.Shell`; a D-Bus member name may not contain a
    /// dot, so zbus refused the call before it left the process, and the
    /// `let Ok(..) else { return false }` turned that into "the extension is
    /// old". Every version gate therefore answered **no**: `show` used the
    /// pre-version-3 `Show` on every hint, and §5.1c's cyan dot — the only
    /// on-screen readback of where the click actually lands — was never
    /// drawn by this path.
    ///
    /// The lesson is the swallow, not the typo. A malformed call is a bug in
    /// muvor and a missing service is a fact about the desktop, and `Err`
    /// meant both here.
    pub fn version(&self) -> u32 {
        *self.version.get_or_init(|| {
            let Ok(reply) = self.call_on(
                "org.freedesktop.DBus.Properties",
                "Get",
                &(IFACE, "Version"),
                CALL_TIMEOUT,
            ) else {
                return 0;
            };
            let body = reply.body();
            let Ok(value) = body.deserialize::<zbus::zvariant::Value<'_>>() else {
                return 0;
            };
            String::try_from(value).ok().and_then(|s| s.parse::<u32>().ok()).unwrap_or(0)
        })
    }

    fn version_at_least(&self, want: u32) -> bool {
        self.version() >= want
    }

    /// Whether the loaded extension tells autorepeat apart from a press
    /// ([`Outcome::KeyRepeat`], v10 and later).
    ///
    /// **A gate rather than an assumption, and it is load-bearing.** With it
    /// true, a `KeyDown` for a key muvor already believes is held is that
    /// key's lost `KeyUp` and the right response is to let go and press
    /// again ([`crate::motion_repair`], §12.16). With it false the very same
    /// event is the keyboard repeating thirty times a second, and treating
    /// *that* as a re-press is `d` firing thirty double clicks a second.
    /// The two readings differ only in what the other half promises, which
    /// is exactly what a version is for (§5.4a) — and an extension changed
    /// on disk is not the extension that is running until a logout, so the
    /// stale-session case is the normal one here, not the exotic one.
    pub fn marks_repeats(&self) -> bool {
        self.version_at_least(10)
    }

    /// The whole stage, as PNG (§5.4b, D16).
    ///
    /// **Not on the hot path, and the extension explains why at length:** the
    /// three raw-pixel routes out of the compositor are all unusable from
    /// GJS, two of them silently, so the only correct route encodes PNG at
    /// 38-151 ms. This is the transport the region detector is built and
    /// tuned against, and the one that generates §5.4's labelled dataset.
    /// A native shim replaces it if the detector earns the hot path; this
    /// signature does not change when it does.
    ///
    /// Requires extension version 4. An older one cannot grow the method
    /// until the session restarts (§5.4a), so say that rather than let the
    /// user read a D-Bus error about an unknown method.
    pub fn capture(&self) -> Result<Frame, Error> {
        if !self.version_at_least(4) {
            return Err(Error::NotRunning(
                "the loaded extension is older than version 4 and has no Capture; \
                 reinstall it and restart the session"
                    .to_owned(),
            ));
        }
        let reply = self.call_for("Capture", &(), CAPTURE_TIMEOUT)?;
        let (png, (x, y, w, h), scale): (Vec<u8>, (i32, i32, i32, i32), f64) = reply
            .body()
            .deserialize()
            .map_err(|e| Error::Bus(format!("Capture reply: {e}")))?;
        Ok(Frame { png, x, y, w, h, scale })
    }

    /// Take the keyboard for free mode (§4.5c). Extension v5.
    ///
    /// A grab of its own rather than a mode of the label overlay, so the
    /// two most safety-relevant paths in the shell never share a branch.
    /// Keys arrive as [`Outcome::Key`] until Escape or the deadman.
    /// Take the keyboard and forward every key by name until Escape or the
    /// deadman.
    ///
    /// **v7 renamed this from `FreeMode`**, and the rename is the point:
    /// "free mode" is a *product* concept and the shell is not allowed to
    /// have one (§5.5, D5). What the shell does is hold keys; whether that
    /// is free mode or movement mode is muvor's business. v6 and earlier are
    /// still asked the old way, so a stale session degrades to free mode
    /// rather than to an error nobody can read.
    pub fn grab_keys(&self) -> Result<(), Error> {
        if self.version_at_least(7) {
            return self.call("GrabKeys", &()).map(drop);
        }
        if !self.version_at_least(5) {
            return Err(Error::NotRunning(
                "the loaded extension is older than version 5 and cannot forward keys; \
                 reinstall it and restart the session"
                    .to_owned(),
            ));
        }
        self.call("FreeMode", &()).map(drop)
    }

    /// What the shell's key observer has actually seen (§12.3): whether
    /// presses and releases reach it at all, and how many.
    ///
    /// This is the one question the design could not answer without a
    /// logout — whether holding a label's second key can be told from
    /// tapping it — reduced to one call. `false, false` means the hold
    /// gesture is unavailable and every label is a click, which is v6's
    /// behaviour and not a fault. v7 and later.
    pub fn probe(&self) -> Result<(bool, bool, u32, u32), Error> {
        let reply = self.call("Probe", &())?;
        let ((p, r, np, nr),): ((bool, bool, u32, u32),) = reply
            .body()
            .deserialize()
            .map_err(|e| Error::Bus(format!("Probe reply: {e}")))?;
        Ok((p, r, np, nr))
    }

    /// Open a signal stream and keep it open (see [`Keys`]).
    pub fn keys(&self) -> Result<Keys<'_>, Error> {
        let stream = self.rt.block_on(async {
            let rule = zbus::MatchRule::builder()
                .msg_type(zbus::message::Type::Signal)
                .interface(IFACE)
                .map_err(|e| Error::Bus(format!("match rule: {e}")))?
                .build();
            // **256, not 64.** The queue is what makes one stream a fix and
            // not a smaller race (§12.14): D-Bus holds what arrives while
            // muvor is inside §4.5's validate, §4.5b's 3 s capture or a
            // `MOVE_ABS`, and a *full* queue drops the oldest message
            // silently — which is a `KeyUp`, and a lost `KeyUp` is the fault
            // §12.16 exists for. At the 30/s a held key repeats at, 64 is
            // two seconds and the capture path alone can outlast it.
            zbus::MessageStream::for_match_rule(rule, &self.conn, Some(256))
                .await
                .map_err(|e| Error::Bus(format!("signal stream: {e}")))
        })?;
        Ok(Keys { shell: self, stream: Some(stream) })
    }

    pub fn hide(&self) -> Result<(), Error> {
        self.call("Hide", &()).map(drop)
    }

    /// M4's hardcoded rectangles, drawn by the shell itself.
    pub fn demo(&self) -> Result<(), Error> {
        self.call("Demo", &()).map(drop)
    }

    /// Block until the overlay reports what happened.
    pub fn wait(&self, timeout: Duration) -> Result<Outcome, Error> {
        self.rt.block_on(async {
            let rule = zbus::MatchRule::builder()
                .msg_type(zbus::message::Type::Signal)
                .interface(IFACE)
                .map_err(|e| Error::Bus(format!("match rule: {e}")))?
                .build();
            let mut stream = zbus::MessageStream::for_match_rule(rule, &self.conn, Some(4))
                .await
                .map_err(|e| Error::Bus(format!("signal stream: {e}")))?;

            let deadline = tokio::time::Instant::now() + timeout;
            loop {
                let next = tokio::time::timeout_at(deadline, futures_lite::StreamExt::next(&mut stream));
                let Ok(message) = next.await else { return Ok(Outcome::TimedOut) };
                let Some(Ok(message)) = message else { return Ok(Outcome::TimedOut) };
                if let Some(outcome) = decode_signal(&message)? {
                    return Ok(outcome);
                }
            }
        })
    }

    /// Block until the hotkey is pressed, however long that takes.
    ///
    /// The daemon's other half. `Main.wm.addKeybinding` fires inside the
    /// compositor and the extension turns it into a signal (v5) rather than
    /// acting on it — §5.5's rule that the shell answers calls and emits
    /// signals but never decides anything. This is the subscription that
    /// makes the hotkey mean something on a desktop with no terminal open.
    ///
    /// **No timeout.** Every other wait in muvor is bounded because
    /// something is holding the keyboard; nothing is holding anything here,
    /// and a daemon that stopped listening after ten seconds would be a
    /// daemon that only worked if you were quick.
    pub fn wait_hotkey(&self) -> Result<(), Error> {
        self.rt.block_on(async {
            let rule = zbus::MatchRule::builder()
                .msg_type(zbus::message::Type::Signal)
                .interface(IFACE)
                .map_err(|e| Error::Bus(format!("match rule: {e}")))?
                .member("Hotkey")
                .map_err(|e| Error::Bus(format!("match rule: {e}")))?
                .build();
            let mut stream = zbus::MessageStream::for_match_rule(rule, &self.conn, Some(4))
                .await
                .map_err(|e| Error::Bus(format!("signal stream: {e}")))?;
            match futures_lite::StreamExt::next(&mut stream).await {
                Some(Ok(_)) => Ok(()),
                // The stream ending means the bus went away, which is the
                // session ending. Reported rather than retried: the caller
                // decides whether a daemon outlives its session.
                _ => Err(Error::Bus("the session bus closed".to_owned())),
            }
        })
    }

    /// Draw again over a grab that is already held, without disturbing it.
    ///
    /// D18's tier 2 arriving late: the user pressed Tab, muvor went and
    /// asked the camera, and the overlay has to grow. `ShowMarked` already
    /// replaces the target set in place, so this is `show` with a name that
    /// says why it is being called twice.
    pub fn redraw(&self, hints: &[Hint]) -> Result<(), Error> {
        self.show(hints)
    }

    fn call<B>(&self, method: &str, body: &B) -> Result<zbus::Message, Error>
    where
        B: zbus::export::serde::ser::Serialize + zbus::zvariant::DynamicType,
    {
        self.call_for(method, body, CALL_TIMEOUT)
    }

    fn call_for<B>(
        &self,
        method: &str,
        body: &B,
        timeout: Duration,
    ) -> Result<zbus::Message, Error>
    where
        B: zbus::export::serde::ser::Serialize + zbus::zvariant::DynamicType,
    {
        self.call_on(IFACE, method, body, timeout)
    }

    /// The same call, on an interface that is not muvor's own — which in
    /// practice means `org.freedesktop.DBus.Properties`. It exists because
    /// the alternative was spelling the interface into the member name, and
    /// that is exactly the bug [`Shell::version`] describes.
    fn call_on<B>(
        &self,
        iface: &str,
        method: &str,
        body: &B,
        timeout: Duration,
    ) -> Result<zbus::Message, Error>
    where
        B: zbus::export::serde::ser::Serialize + zbus::zvariant::DynamicType,
    {
        self.rt.block_on(async {
            let call = self.conn.call_method(Some(BUS_NAME), OBJECT_PATH, Some(iface), method, body);
            match tokio::time::timeout(timeout, call).await {
                Ok(Ok(reply)) => Ok(reply),
                Ok(Err(zbus::Error::MethodError(name, detail, _)))
                    if name.as_str().ends_with("ServiceUnknown")
                        || name.as_str().ends_with("NameHasNoOwner") =>
                {
                    Err(Error::NotRunning(detail.unwrap_or_default()))
                }
                Ok(Err(e)) => Err(Error::Bus(format!("{method}: {e}"))),
                Err(_) => Err(Error::Timeout),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{Outcome, decode_signal};

    fn signal(member: &str, body: &str) -> zbus::message::Message {
        zbus::message::Message::signal(super::OBJECT_PATH, super::IFACE, member)
            .expect("builder")
            .build(&(body,))
            .expect("message")
    }

    /// **A repeat and a press are two different signals**, and every fix in
    /// §12.16 rests on muvor being able to tell them apart. A `KeyDown` for
    /// a key already held is a lost release; the very same event read as
    /// autorepeat is thirty double clicks a second.
    #[test]
    fn a_repeat_decodes_as_a_repeat_and_not_as_a_press() {
        assert_eq!(
            decode_signal(&signal("KeyRepeat", "d")).expect("decodes"),
            Some(Outcome::KeyRepeat("d".to_owned()))
        );
        assert_eq!(
            decode_signal(&signal("KeyDown", "d")).expect("decodes"),
            Some(Outcome::KeyDown("d".to_owned()))
        );
        assert_eq!(
            decode_signal(&signal("KeyUp", "d")).expect("decodes"),
            Some(Outcome::KeyUp("d".to_owned()))
        );
    }

    /// v6 called a press `Key`, and an extension from before v10 sends a
    /// repeat under that same name. Nothing here may invent a `KeyRepeat`
    /// that the shell did not send — the version gate is what stands between
    /// a stale session and `d` clicking at 30 Hz.
    #[test]
    fn an_old_extensions_press_is_still_only_a_press() {
        assert_eq!(
            decode_signal(&signal("Key", "d")).expect("decodes"),
            Some(Outcome::KeyDown("d".to_owned()))
        );
        assert_eq!(decode_signal(&signal("Nonsense", "d")).expect("decodes"), None);
    }

    /// The panic that killed the daemon on 2026-08-25, reproduced without
    /// the extension, the overlay or a keystroke.
    ///
    /// A `MessageStream` built for a match rule deregisters that rule in its
    /// `Drop`, and under the `tokio` feature that is `tokio::task::spawn` —
    /// which panics on a thread with no runtime entered. `Keys` is the one
    /// stream in this file that outlives its `block_on`, so its scope exit
    /// ran the spawn bare. The assertion is not that the drop *works*; it is
    /// that it does not take the process down.
    ///
    /// Needs a session bus and nothing else. Skipped where there is none, so
    /// that a headless `cargo test` stays green.
    #[test]
    fn a_match_rule_stream_survives_being_dropped_outside_block_on() {
        let Ok(rt) = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
        else {
            return;
        };
        let Ok(conn) = rt.block_on(zbus::Connection::session()) else {
            eprintln!("no session bus — skipped");
            return;
        };
        let rule = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface(super::IFACE)
            .expect("interface")
            .build();
        let stream = rt
            .block_on(zbus::MessageStream::for_match_rule(rule, &conn, Some(4)))
            .expect("stream");

        // The line that used to abort the daemon. `Keys::drop` wraps exactly
        // this, and without the guard the spawn inside `queue_remove_match`
        // panics with "there is no reactor running".
        let guard = rt.enter();
        drop(stream);
        drop(guard);
    }

    /// **A signal emitted while no stream is open is gone**, and a stream
    /// that *is* open holds what arrives while its reader is busy elsewhere.
    ///
    /// That sentence is the whole of the fault of 2026-08-26. `Shell::wait`
    /// builds a `MessageStream` per call and drops it on the way out, so
    /// between two calls muvor is deaf — and the extension starts forwarding
    /// keys in the same call that emits `TypedHold`, while muvor is still in
    /// §4.5's validation, §4.5b's capture and the `MOVE_ABS`. The held key's
    /// own release landed in that window, `swallow` was never cleared, and
    /// that action key was dead for the whole of movement mode; so did the
    /// first `f` or `d` a quick hand pressed, which is why it "worked on the
    /// second try".
    ///
    /// The second half is what makes the fix a fix rather than a race made
    /// smaller: the 200 ms sleep here stands for all of that work, and the
    /// signal sent before it is still there afterwards. D-Bus queues it.
    ///
    /// Needs a session bus and nothing else.
    #[test]
    fn a_signal_sent_while_no_stream_is_open_is_lost() {
        use futures_lite::StreamExt as _;
        use std::time::Duration;

        let Ok(rt) = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
        else {
            return;
        };
        let Ok(tx) = rt.block_on(zbus::Connection::session()) else {
            eprintln!("no session bus — skipped");
            return;
        };
        let rx = rt.block_on(zbus::Connection::session()).expect("second connection");

        let emit = |name: &str| {
            let msg = zbus::message::Message::signal(super::OBJECT_PATH, super::IFACE, "KeyUp")
                .expect("builder")
                .build(&(name,))
                .expect("message");
            rt.block_on(tx.send(&msg)).expect("send");
        };
        let rule = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface(super::IFACE)
            .expect("interface")
            .member("KeyUp")
            .expect("member")
            .build();
        let open = || {
            rt.block_on(zbus::MessageStream::for_match_rule(rule.clone(), &rx, Some(16)))
                .expect("stream")
        };

        // The gap. Nothing is listening, so this key never happened.
        emit("lost");
        let mut stream = open();
        emit("kept");
        // Everything `movement_mode` used to do before it opened its stream.
        std::thread::sleep(Duration::from_millis(200));

        let got = rt
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(2), stream.next()).await
            })
            .expect("a signal within 2s")
            .expect("a message")
            .expect("a valid message");
        let (name,): (String,) = got.body().deserialize().expect("payload");
        assert_eq!(
            name, "kept",
            "the stream delivered the key sent before it was opened — the test is not testing \
             what it claims to"
        );

        let guard = rt.enter();
        drop(stream);
        drop(guard);
    }
}
