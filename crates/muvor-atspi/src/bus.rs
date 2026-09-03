//! The one D-Bus thread (D4).
//!
//! AT-SPI is D-Bus, D-Bus in Rust is async, and the hotkey → detect → draw →
//! inject path is plain blocking code that must not grow a runtime. So the
//! async lives here, on a thread of its own, behind a channel: `Detector` is
//! the sync side and never awaits anything.
//!
//! The runtime is `current_thread` deliberately. There is exactly one
//! connection and exactly one in-flight request; a work-stealing scheduler
//! would add threads to a workload that is entirely waiting on a socket.

use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use atspi::events::cache::{AddAccessibleEvent, RemoveAccessibleEvent};
use atspi::events::object::StateChangedEvent;
use atspi::events::window::{ActivateEvent, DeactivateEvent};
use atspi::events::{CacheEvents, Event, ObjectEvents, WindowEvents};
use atspi::proxy::accessible::{AccessibleProxy, ObjectRefExt};
use atspi::proxy::component::ComponentProxy;
use atspi::{zbus, AccessibilityConnection, CoordType, ObjectRefOwned, Role, State};
use futures_lite::StreamExt;

use crate::mirror::Mirror;
use crate::target::{FocusSource, Frame, WindowRect};
use crate::Error;

/// The focused window, shared between the draining task and the served
/// requests. A `std::sync::Mutex` on purpose: it is never held across an
/// await, and a `tokio` one would suggest otherwise.
type Focus = Arc<Mutex<Option<ObjectRefOwned>>>;

/// The tree mirror (§4.2-iii), shared the same way and for the same reason.
pub(crate) type Shared = Arc<Mutex<Mirror>>;

/// Per-call ceiling on any single D-Bus round trip.
///
/// zbus defaults to 25 seconds, which is the right answer for a tool that is
/// allowed to hang and the wrong one for a keypress. An application that has
/// stopped answering its accessibility bus — a hung tab, a stopped process —
/// must cost detection one budget, not one session.
const CALL_TIMEOUT: Duration = Duration::from_millis(250);

/// What the sync side can ask for.
pub(crate) enum Request {
    /// The window everything else is scoped to (§4.3).
    FocusedFrame,
    /// A window picked by name — `--window`. Testing only; see
    /// [`FocusSource::Named`].
    NamedFrame(String),
    /// The window the compositor says is focused, identified by the pid and
    /// title its `FocusedWindow` reported. The product path (§4.3a).
    CompositorFrame { title: String, pid: u32, size: (i32, i32) },
    /// Re-check one target immediately before it is clicked (§4.5).
    Verify { frame: Box<Frame>, claim: Box<crate::verify::Claim> },
    /// Warm the mirror for one window, off the hotkey path (§4.2-iii).
    Warm(Box<Frame>),
    /// What the mirror currently holds.
    MirrorStats,
    /// Actionable nodes and their parents, for diagnosing an empty result.
    MirrorNodes { bus_name: String, limit: usize },
    /// Everything clickable in one window (§4.2). `force` overrides the
    /// probe, which is how the paths get compared on one window.
    Targets {
        frame: Box<Frame>,
        force: Option<crate::target::Provenance>,
        /// Fetch role and name for nodes whose source did not supply them.
        /// The hotkey path does not need either; `--dump` prints both.
        metadata: bool,
    },
}

impl Request {
    /// A short name for error messages, so a stalled request says *which*
    /// one stalled. Without it, `Error::Stalled` would be the same sentence
    /// whether detection or verification stopped answering, and those are
    /// two very different faults to be told about.
    pub(crate) const fn name(&self) -> &'static str {
        match self {
            Self::FocusedFrame => "focused frame",
            Self::NamedFrame(_) => "named frame",
            Self::CompositorFrame { .. } => "compositor frame",
            Self::Verify { .. } => "validate-at-action",
            Self::Warm(_) => "warm mirror",
            Self::MirrorStats => "mirror stats",
            Self::MirrorNodes { .. } => "mirror nodes",
            Self::Targets { .. } => "targets",
        }
    }
}

/// What comes back, with what it cost. The timing is not decoration: §5.2
/// requires every milestone from M2 to be measured rather than assumed, and
/// the M2 exit criterion is a number in milliseconds.
pub(crate) struct Reply {
    pub(crate) payload: Result<Payload, Error>,
    pub(crate) elapsed: Duration,
}

pub(crate) enum Payload {
    Frame(Option<Frame>),
    Scan(Box<crate::probe::Scan>),
    Warmed(crate::probe::Warmth),
    Mirror(crate::mirror::MirrorStats),
    Nodes(Vec<(String, String, Role, String)>),
    Verified(Box<crate::verify::Verified>),
}

pub(crate) struct Job {
    pub(crate) request: Request,
    pub(crate) reply: mpsc::SyncSender<Reply>,
}

/// Requests arrive on a **tokio** channel rather than a `std` one, and that
/// is not a detail.
///
/// The obvious shape — block the thread on `rx.recv()` and `block_on` one
/// job at a time — leaves the runtime unscheduled between requests, which
/// means the draining task of [`drain_events`] does not run either. For a
/// hotkey tool, "between requests" is almost all of the time: the connection
/// would fill its broadcast channel while idle and stall before the keypress
/// that mattered. Receiving inside the runtime keeps the drain alive
/// whenever the thread is alive. An unbounded sender is what lets the sync
/// side post a job without a runtime of its own.
pub(crate) type Sender = tokio::sync::mpsc::UnboundedSender<Job>;

/// Spawn the thread and block until the connection is up or has failed.
///
/// Connecting eagerly is the point: "no a11y bus" must be reported at
/// startup by `muvor check`, not discovered at the first hotkey.
pub(crate) fn spawn() -> Result<(Sender, thread::JoinHandle<()>), Error> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), Error>>(1);

    let handle = thread::Builder::new()
        .name("muvor-atspi".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = ready_tx.send(Err(Error::Bus(format!("tokio runtime: {e}"))));
                    return;
                }
            };
            let conn = match rt.block_on(AccessibilityConnection::new()) {
                Ok(c) => c,
                Err(e) => {
                    let _ = ready_tx.send(Err(Error::Bus(format!("a11y bus: {e}"))));
                    return;
                }
            };
            // Subscribing here, before anyone can ask a question, is the
            // whole focus mechanism: activations that happen while muvor is
            // idle are what make the answer available instantly when the
            // hotkey finally arrives.
            if let Err(e) = rt.block_on(subscribe(&conn)) {
                let _ = ready_tx.send(Err(e));
                return;
            }

            // This task is not only how focus is tracked — it is what keeps
            // the connection alive. See `drain_events`.
            let focus: Focus = Arc::new(Mutex::new(None));
            let mirror: Shared = Arc::new(Mutex::new(Mirror::default()));
            rt.spawn(drain_events(
                conn.clone(),
                Arc::clone(&focus),
                Arc::clone(&mirror),
                rt.handle().clone(),
            ));

            if ready_tx.send(Ok(())).is_err() {
                return;
            }

            // One job at a time, by construction — but *inside* the runtime,
            // so the drain task keeps running between them and during them.
            rt.block_on(async {
                while let Some(job) = rx.recv().await {
                    let t0 = Instant::now();
                    let payload = serve(&conn, &focus, &mirror, job.request).await;
                    let _ = job.reply.send(Reply { payload, elapsed: t0.elapsed() });
                }
            });
        })
        .map_err(|e| Error::Bus(format!("cannot spawn D-Bus thread: {e}")))?;

    match ready_rx.recv() {
        Ok(Ok(())) => Ok((tx, handle)),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(Error::Disconnected),
    }
}

async fn subscribe(conn: &AccessibilityConnection) -> Result<(), Error> {
    conn.register_event::<ActivateEvent>()
        .await
        .map_err(|e| Error::Bus(format!("subscribe window:activate: {e}")))?;
    conn.register_event::<DeactivateEvent>()
        .await
        .map_err(|e| Error::Bus(format!("subscribe window:deactivate: {e}")))?;
    // The mirror's maintenance feed (§4.2-iii). `Cache` add/remove is how
    // the tree changes shape; `object:state-changed` is how a node stops
    // being a target without the tree changing at all — a button greying
    // out is exactly that, and it is the case §4.4 cares most about.
    conn.register_event::<AddAccessibleEvent>()
        .await
        .map_err(|e| Error::Bus(format!("subscribe cache:add: {e}")))?;
    conn.register_event::<RemoveAccessibleEvent>()
        .await
        .map_err(|e| Error::Bus(format!("subscribe cache:remove: {e}")))?;
    conn.register_event::<StateChangedEvent>()
        .await
        .map_err(|e| Error::Bus(format!("subscribe object:state-changed: {e}")))
}

/// Track focus, maintain the mirror, and — the part that is not optional —
/// **keep the connection drained**.
///
/// Measured 2026-08-19: an undrained event stream stalls zbus. Its incoming
/// messages go through a bounded broadcast channel, and once that fills, the
/// socket reader stops, which means *method replies stop arriving too*. It
/// presents as calls timing out against an application that answers the same
/// calls from `gdbus` in 4 ms — around a hundred calls in, and never before
/// then, which is as misleading a symptom as this project has produced.
///
/// An earlier version drained the stream once per request instead. It looked
/// tidier, cost nothing while idle, and quietly capped every operation at
/// about sixty round trips.
///
/// Nothing slow may be awaited *in here* for the same reason: warming a tree
/// takes up to 150 ms, so it is spawned as its own task rather than awaited,
/// and this loop returns to the stream immediately.
async fn drain_events(
    conn: AccessibilityConnection,
    focus: Focus,
    mirror: Shared,
    rt: tokio::runtime::Handle,
) {
    let mut events = std::pin::pin!(conn.event_stream());
    while let Some(ev) = events.next().await {
        {
            // Stamped on every event, not only the ones that change a tree:
            // the question `muvor status` has to answer is whether the
            // stream is *alive*, and a desktop emitting nothing but
            // text-changed is still emitting (M5s-d).
            let mut m = mirror.lock().expect("mirror mutex");
            m.seen += 1;
            m.last_event = Some(Instant::now());
        }
        match ev {
            Ok(Event::Window(WindowEvents::Activate(e))) => {
                // A window becoming focused is the moment to pay for its
                // tree: the user is looking at it and has not pressed the
                // hotkey yet. D14 gives this hook for free.
                let bus = e.item.name_as_str().unwrap_or_default().to_owned();
                let path = e.item.path_as_str().to_owned();
                *focus.lock().expect("focus mutex") = Some(e.item);
                rt.spawn(crate::probe::warm_app(
                    conn.clone(),
                    Arc::clone(&mirror),
                    bus,
                    path,
                ));
            }
            Ok(Event::Window(WindowEvents::Deactivate(e))) => {
                // Only when it is still the one we hold: activate(new) can
                // arrive before deactivate(old), and clearing unconditionally
                // would drop a focus that had already moved on.
                let mut held = focus.lock().expect("focus mutex");
                if held.as_ref().is_some_and(|f| f.path() == e.item.path()) {
                    *held = None;
                }
            }
            Ok(Event::Cache(CacheEvents::Add(e))) => {
                let bus = e.node_added.object.name_as_str().unwrap_or_default().to_owned();
                let mut m = mirror.lock().expect("mirror mutex");
                if let Some(tree) = m.app_mut(&bus) {
                    tree.insert_item(&e.node_added);
                    tree.updates += 1;
                    tree.last_update = Some(Instant::now());
                }
            }
            Ok(Event::Cache(CacheEvents::Remove(e))) => {
                let bus = e.node_removed.name_as_str().unwrap_or_default().to_owned();
                let mut m = mirror.lock().expect("mirror mutex");
                if let Some(tree) = m.app_mut(&bus) {
                    tree.remove(e.node_removed.path_as_str());
                    tree.updates += 1;
                    tree.last_update = Some(Instant::now());
                }
            }
            Ok(Event::Object(ObjectEvents::StateChanged(e))) => {
                let bus = e.item.name_as_str().unwrap_or_default().to_owned();
                let mut m = mirror.lock().expect("mirror mutex");
                if let Some(tree) = m.app_mut(&bus) {
                    if tree.set_state(e.item.path_as_str(), e.state, e.enabled) {
                        tree.updates += 1;
                        tree.last_update = Some(Instant::now());
                    }
                }
            }
            _ => {}
        }
    }
}

async fn serve(
    conn: &AccessibilityConnection,
    focus: &Focus,
    mirror: &Shared,
    req: Request,
) -> Result<Payload, Error> {
    match req {
        Request::FocusedFrame => {
            let held = focus.lock().expect("focus mutex").clone();
            focused_frame(conn, held.as_ref()).await.map(Payload::Frame)
        }
        Request::NamedFrame(q) => named_frame(conn, &q).await.map(Payload::Frame),
        Request::CompositorFrame { title, pid, size } => {
            compositor_frame(conn, mirror, &title, pid, size).await.map(Payload::Frame)
        }
        Request::Verify { frame, claim } => crate::verify::verify(conn, &frame, &claim)
            .await
            .map(|v| Payload::Verified(Box::new(v))),
        Request::MirrorNodes { bus_name, limit } => {
            let m = mirror.lock().expect("mirror mutex");
            let nodes = m.app(&bus_name).map_or_else(Vec::new, |t| {
                t.iter()
                    .filter(|(_, n)| crate::filter::role_is_actionable(n.role))
                    .take(limit)
                    .map(|(p, n)| (p.to_owned(), n.parent.clone(), n.role, n.name.clone()))
                    .collect()
            });
            Ok(Payload::Nodes(nodes))
        }
        Request::MirrorStats => {
            Ok(Payload::Mirror(mirror.lock().expect("mirror mutex").stats()))
        }
        Request::Warm(frame) => {
            crate::probe::warm(conn, mirror, &frame.bus_name, &frame.path).await.map(Payload::Warmed)
        }
        Request::Targets { frame, force, metadata } => {
            crate::probe::scan(conn, mirror, &frame, force, metadata)
                .await
                .map(|s| Payload::Scan(Box::new(s)))
        }
    }
}

pub(crate) async fn call<T>(what: &'static str, fut: impl Future<Output = zbus::Result<T>>) -> Result<T, Error> {
    match tokio::time::timeout(CALL_TIMEOUT, fut).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(Error::Bus(format!("{what}: {e}"))),
        Err(_) => Err(Error::Timeout(what)),
    }
}

/// Find the active window.
///
/// The cached activation is the fast path and costs two round trips. The
/// fallback enumerates applications, which §4.3 measures at **26.5 ms** for
/// 14 apps — far outside the 5 ms probe budget, and the reason it is
/// reported separately from probe time rather than folded into it.
///
/// Both are temporary: the extension already knows which window is focused,
/// and from M4 it hands that over for free along with the origin (§4.2a) and
/// the stacking order AT-SPI cannot see (§4.3).
async fn focused_frame(
    conn: &AccessibilityConnection,
    focus: Option<&ObjectRefOwned>,
) -> Result<Option<Frame>, Error> {
    if let Some(obj) = focus {
        // A window can close without deactivating first, so a cached
        // activation is a hypothesis, not a fact — resolving it is also how
        // it gets checked. A stale ref falls through to the scan.
        if let Ok(proxy) = obj.as_accessible_proxy(conn.connection()).await {
            if let Ok(frame) = describe(conn, obj, &proxy, FocusSource::Activate).await {
                return Ok(Some(frame));
            }
        }
    }
    scan(conn, None).await
}

/// `--window <substring>`: the first window whose application name or title
/// contains `query`, case-insensitively.
async fn named_frame(conn: &AccessibilityConnection, query: &str) -> Result<Option<Frame>, Error> {
    scan(conn, Some(&query.to_lowercase())).await
}

/// The window the compositor named (§4.3a) — the product path.
///
/// The extension hands over a pid, a title and an origin in one call. The pid
/// is what makes this reliable rather than merely convenient: it narrows the
/// registry to one application before any title is compared, so two windows
/// called "Untitled document" in two different applications can never be
/// confused, and a title that changed between the hotkey and the lookup still
/// lands in the right process.
///
/// The pid comes from the **a11y bus**, not the session bus: an application's
/// accessibility connection is a connection like any other, and
/// `GetConnectionUnixProcessID` on its unique name is the one question that
/// ties the two worlds together. One extra round trip per application
/// examined, and the walk stops at the first match.
///
/// Returns [`FocusSource::Compositor`] when the pid matched, and
/// [`FocusSource::Named`] when it did not and the title alone had to carry
/// it — the caller prints the difference, because a title-only match is a
/// guess and the user is entitled to know one was made.
/// How far the accessibility tree and the compositor may disagree about a
/// window's size and still be talking about the same window.
///
/// Client-side decorations and shadows put a few pixels between the two on
/// every toolkit measured; a mismatched *window* is out by hundreds.
const SIZE_TOLERANCE: i32 = 8;

async fn compositor_frame(
    conn: &AccessibilityConnection,
    mirror: &Shared,
    title: &str,
    pid: u32,
    size: (i32, i32),
) -> Result<Option<Frame>, Error> {
    let dbus = conn.connection();
    let fdo = zbus::fdo::DBusProxy::new(dbus)
        .await
        .map_err(|e| Error::Bus(format!("a11y bus proxy: {e}")))?;

    let root = conn
        .root_accessible_on_registry()
        .await
        .map_err(|e| Error::Bus(format!("registry root: {e}")))?;
    let apps = call("registry children", root.get_children()).await?;

    // Ask the connection this pid used last time first, and *verify* it
    // rather than trusting it (§2.5, M5s-c). One round trip against the
    // fourteen the loop below pays — and if the name has been reused or the
    // application is gone, the answer is simply no and the loop runs anyway.
    // A pid that owns no connection costs one round trip per application to
    // discover, and discovering it again on every hint is the single largest
    // avoidable cost in this function (M5s-d).
    let hopeless = pid != 0 && mirror.lock().expect("mirror mutex").pid_owns_nothing(pid);

    let remembered = if hopeless { None } else { mirror.lock().expect("mirror mutex").app_for_pid(pid) };
    let mut known: Option<String> = None;
    let ordered: Vec<&ObjectRefOwned> = match remembered {
        Some(ref name) if pid != 0 && app_pid(&fdo, name).await == Some(pid) => {
            // Verified here, so the loop must not ask again: one round trip
            // is the entire saving and paying it twice would halve it.
            known = Some(name.clone());
            apps.iter()
                .filter(|a| a.name_as_str() == Some(name.as_str()))
                .chain(apps.iter().filter(|a| a.name_as_str() != Some(name.as_str())))
                .collect()
        }
        Some(_) => {
            mirror.lock().expect("mirror mutex").forget_app_for_pid(pid);
            apps.iter().collect()
        }
        None => apps.iter().collect(),
    };

    // The pid is asked **before** anything else about an application, and
    // that ordering is worth 8 ms: measured 2026-08-19, asking each app for
    // its children and its name first cost 12.6 ms across this desktop's
    // applications, against 4.4 ms when the pid is the only question put to
    // the ones that do not match. Two round trips per application is not a
    // rounding error when there are fourteen of them and a 50 ms budget
    // (§5.2) to fit the whole path into.
    for app in if hopeless { Vec::new() } else { ordered } {
        let Some(bus_name) = app.name_as_str() else { continue };
        if known.as_deref() != Some(bus_name)
            && (pid == 0 || app_pid(&fdo, bus_name).await != Some(pid))
        {
            continue;
        }
        mirror.lock().expect("mirror mutex").note_app_for_pid(pid, bus_name);
        let Ok(proxy) = app.as_accessible_proxy(dbus).await else { continue };
        let Ok(windows) = call("app children", proxy.get_children()).await else { continue };
        let app_name = call("app name", proxy.name()).await.unwrap_or_default();

        // One window: the pid has already decided it. An application with a
        // single toplevel cannot be ambiguous whatever it calls itself.
        let chosen = if windows.len() == 1 {
            windows.first().map(|w| (w.clone(), FocusSource::Compositor))
        } else {
            pick(conn, &windows, title, size).await
        };
        let Some((win, source)) = chosen else { continue };
        let Ok(wproxy) = win.as_accessible_proxy(dbus).await else { continue };
        let mut frame = describe(conn, &win, &wproxy, source).await?;

        // **A pid match is evidence, not proof** — measured 2026-08-19.
        // Opening Nautilus, the compositor reported its window under
        // gnome-shell's pid (2287, not nautilus's 2998); the scan duly found
        // gnome-shell, took its only toplevel — the 99x56 "Main stage" — and
        // reported `matched by compositor`, which is muvor saying it is
        // certain. It was pointing at the wrong window, in the wrong
        // application, with full confidence.
        //
        // The geometry is the check the pid cannot supply: two windows that
        // are the same window agree about how big they are. Disagreement
        // beyond the tolerance means this is not it, so keep looking rather
        // than return it — and if nothing else matches, the title fallback
        // below answers `Named`, which at least says a guess was made.
        // §4.5's rule reaches back this far: a wrong click starts with a
        // wrong window.
        if size != (0, 0)
            && ((frame.bounds.w - size.0).abs() > SIZE_TOLERANCE
                || (frame.bounds.h - size.1).abs() > SIZE_TOLERANCE)
        {
            continue;
        }

        frame.pid_memo = known.as_deref() == Some(bus_name);
        if frame.app.is_empty() {
            frame.app = app_name;
        }
        return Ok(Some(frame));
    }

    // No application on the accessibility bus belongs to that process. That
    // is not necessarily an error — an app can be reached under a different
    // pid than the one holding the surface, and a window with no tree at all
    // (D12) reaches here too — so fall back to the title, and say so through
    // `FocusSource::Named`: a title-only match is a guess, and the caller
    // prints that it made one.
    //
    // **An empty title is not a fallback, it is a refusal.** `scan` matches
    // by substring, and every string contains the empty one — so passing it
    // through would return whichever window answered first and call it
    // focused. Measured: it named gnome-shell as the focused window of a
    // pid that had exited. Nothing is the right answer here.
    if !hopeless && pid != 0 {
        mirror.lock().expect("mirror mutex").note_pid_owns_nothing(pid);
    }

    if title.is_empty() {
        return Ok(None);
    }

    // The fallback is the most expensive path muvor has — every window of
    // every application, three round trips each — and for an application
    // whose pid never matches it is the path taken *every time*. So it gets
    // a memo of its own: which connection answered this title last, tried
    // alone before the walk (M5s-d).
    let key = title.to_lowercase();
    let remembered = mirror.lock().expect("mirror mutex").app_for_title(&key);
    if let Some(name) = remembered {
        match scan_app(conn, &name, &key).await {
            Ok(Some(mut frame)) => {
                frame.title_memo = true;
                return Ok(Some(frame));
            }
            // The window closed, the title changed, the application
            // restarted. Forget it and pay the walk once — a memo that is
            // wrong must cost one slow lookup, not every lookup after it.
            _ => {
                let mut m = mirror.lock().expect("mirror mutex");
                m.forget_app_for_title(&key);
                // Whatever changed may have changed the pid answer as well,
                // so let the next hint pay the full search once rather than
                // inherit a conclusion drawn about a window that is gone.
                m.forget_pid_owns_nothing(pid);
            }
        }
    }

    let found = scan(conn, Some(&key)).await?;
    if let Some(frame) = &found {
        mirror.lock().expect("mirror mutex").note_app_for_title(&key, &frame.bus_name);
    }
    Ok(found)
}

/// The title scan, restricted to one application.
///
/// The same test `scan` applies, against one connection instead of every
/// connection on the bus. Separate rather than a parameter on `scan` because
/// `scan` starts from the registry root and this starts from a name: the two
/// share their matching rule, not their traversal.
async fn scan_app(
    conn: &AccessibilityConnection,
    bus_name: &str,
    query: &str,
) -> Result<Option<Frame>, Error> {
    let dbus = conn.connection();
    let root = conn
        .root_accessible_on_registry()
        .await
        .map_err(|e| Error::Bus(format!("registry root: {e}")))?;
    let apps = call("registry children", root.get_children()).await?;
    let Some(app) = apps.iter().find(|a| a.name_as_str() == Some(bus_name)) else {
        return Ok(None);
    };
    let Ok(proxy) = app.as_accessible_proxy(dbus).await else { return Ok(None) };
    let Ok(windows) = call("app children", proxy.get_children()).await else { return Ok(None) };
    let app_name = call("app name", proxy.name()).await.unwrap_or_default();

    for win in windows {
        let Ok(wproxy) = win.as_accessible_proxy(dbus).await else { continue };
        let title = call("window name", wproxy.name()).await.unwrap_or_default();
        if !app_name.to_lowercase().contains(query) && !title.to_lowercase().contains(query) {
            continue;
        }
        let mut frame = describe(conn, &win, &wproxy, FocusSource::Named).await?;
        if frame.app.is_empty() {
            frame.app = app_name;
        }
        return Ok(Some(frame));
    }
    Ok(None)
}

/// Which of an application's windows the compositor meant.
///
/// **Titles are not identity and equality is the wrong test.** Measured
/// 2026-08-19: gnome-terminal's title is `◐ Next task` — the glyph is a
/// running-command spinner and it changes while the hotkey is in flight.
/// So the title is scored rather than matched, and the window *size* scores
/// alongside it: the compositor supplied a frame rectangle in the same call
/// as the title, and two windows of the same application are rarely the same
/// size to the pixel.
///
/// A window that wins on either signal is [`FocusSource::Compositor`]. When
/// nothing scores — several windows, no title agreement, no size agreement —
/// the first is returned as [`FocusSource::Named`], which is muvor saying
/// *this is a guess* in the only place the distinction can still be printed.
/// It is never a wrong click: §4.5 re-reads the target before injecting, and
/// a hint drawn on the wrong window fails that check.
async fn pick(
    conn: &AccessibilityConnection,
    windows: &[ObjectRefOwned],
    title: &str,
    size: (i32, i32),
) -> Option<(ObjectRefOwned, FocusSource)> {
    let dbus = conn.connection();
    let wanted = title.to_lowercase();
    let mut best: Option<(u32, ObjectRefOwned)> = None;

    for win in windows {
        let Ok(wproxy) = win.as_accessible_proxy(dbus).await else { continue };
        let name = call("window name", wproxy.name()).await.unwrap_or_default().to_lowercase();
        let mut score = 0;
        if !wanted.is_empty() && !name.is_empty() {
            if name == wanted {
                score += 4;
            } else if name.contains(&wanted) || wanted.contains(&name) {
                score += 2;
            }
        }
        if size.0 > 0 && size.1 > 0 {
            if let Ok(b) = extents(dbus, &wproxy).await {
                // 8 px of slack: server-side decorations and shadow borders
                // are the difference between what mutter measures and what
                // a toolkit reports, and neither is wrong.
                if (b.w - size.0).abs() <= 8 && (b.h - size.1).abs() <= 8 {
                    score += 3;
                }
            }
        }
        if score > best.as_ref().map_or(0, |(s, _)| *s) {
            best = Some((score, win.clone()));
        }
    }

    match best {
        Some((_, win)) => Some((win, FocusSource::Compositor)),
        None => windows.first().map(|w| (w.clone(), FocusSource::Named)),
    }
}

/// The pid behind an accessibility connection, or `None` if the bus will not
/// say. A failure here is not fatal — it degrades the match to title-only,
/// which is what the old `--window` path did all along.
async fn app_pid(fdo: &zbus::fdo::DBusProxy<'_>, bus_name: &str) -> Option<u32> {
    let name = zbus::names::BusName::try_from(bus_name.to_owned()).ok()?;
    // `fdo` has an error type of its own; the timeout wrapper takes zbus's,
    // and the conversion is the only reason this is not a one-liner.
    let ask = async {
        fdo.get_connection_unix_process_id(name).await.map_err(zbus::Error::from)
    };
    call("connection pid", ask).await.ok()
}

/// Walk every application's toplevels, looking either for `STATE_ACTIVE` or
/// for a name match.
async fn scan(
    conn: &AccessibilityConnection,
    query: Option<&str>,
) -> Result<Option<Frame>, Error> {
    let dbus = conn.connection();
    let root = conn
        .root_accessible_on_registry()
        .await
        .map_err(|e| Error::Bus(format!("registry root: {e}")))?;
    let apps = call("registry children", root.get_children()).await?;

    for app in apps {
        // One dead application must not take the desktop with it: a failing
        // or slow app is skipped, not propagated. This is also why the
        // per-call timeout exists.
        let Ok(proxy) = app.as_accessible_proxy(dbus).await else { continue };
        let Ok(windows) = call("app children", proxy.get_children()).await else { continue };
        let app_name = call("app name", proxy.name()).await.unwrap_or_default();

        for win in windows {
            let Ok(wproxy) = win.as_accessible_proxy(dbus).await else { continue };
            let title = call("window name", wproxy.name()).await.unwrap_or_default();
            let source = match query {
                // Named lookup: no state bit is consulted, deliberately. The
                // point of --window is to reach a window that focus tracking
                // cannot currently name.
                Some(q) => {
                    if !app_name.to_lowercase().contains(q) && !title.to_lowercase().contains(q) {
                        continue;
                    }
                    FocusSource::Named
                }
                None => {
                    let Ok(states) = call("window states", wproxy.get_state()).await else {
                        continue;
                    };
                    if !states.contains(State::Active) {
                        continue;
                    }
                    FocusSource::ActiveState
                }
            };
            let mut frame = describe(conn, &win, &wproxy, source).await?;
            if frame.app.is_empty() {
                frame.app = app_name;
            }
            frame.title = title;
            return Ok(Some(frame));
        }
    }
    Ok(None)
}

/// Fill in everything `--dump` prints about a window, given a handle to it.
async fn describe(
    conn: &AccessibilityConnection,
    obj: &ObjectRefOwned,
    proxy: &AccessibleProxy<'_>,
    focus: FocusSource,
) -> Result<Frame, Error> {
    let dbus = conn.connection();
    let title = call("window name", proxy.name()).await.unwrap_or_default();
    let app = match call("window application", proxy.get_application()).await {
        Ok(a) => match a.as_accessible_proxy(dbus).await {
            Ok(ap) => call("application name", ap.name()).await.unwrap_or_default(),
            Err(_) => String::new(),
        },
        Err(_) => String::new(),
    };
    let role = call("window role", proxy.get_role()).await.unwrap_or(Role::Invalid);
    let bounds = extents(dbus, proxy).await?;
    Ok(Frame {
        focus,
        // `describe` does not know how its caller found the application;
        // `compositor_frame` is the only path with a memo and it overwrites
        // these before returning.
        pid_memo: false,
        title_memo: false,
        app,
        title,
        role,
        bus_name: obj.name_as_str().unwrap_or_default().to_owned(),
        path: obj.path_as_str().to_owned(),
        bounds,
    })
}

/// `CoordType::Screen`, and that is a measurement rather than a preference.
///
/// The two agree on ordinary content and disagree on everything drawn in a
/// **popup**. Measured on gnome-terminal with its menu open: of 99 nodes in
/// the frame's tree, 73 report identical rectangles for `Screen` and
/// `Window`, and **all 26 that differ are inside the open menu**. A popup's
/// contents report `Window` relative to the popup *surface* and `Screen`
/// relative to the toplevel.
///
/// `Window` was therefore wrong by the whole width of the screen on menus
/// (§5.1f): `New Window` read as `25,69` while it was drawn at `1709,115`,
/// because muvor added the *toplevel's* origin (D13) to a *popup-relative*
/// number and put every badge and every click in the top-left corner.
/// `Screen` is toplevel-relative for both kinds of node, which is the space
/// D13's arithmetic already expects — so this changes nothing wherever the
/// two agreed, and corrects every place they did not.
pub(crate) async fn extents(
    dbus: &zbus::Connection,
    proxy: &AccessibleProxy<'_>,
) -> Result<WindowRect, Error> {
    Ok(extents_in(dbus, proxy).await?.0)
}

/// The same rectangle, and **which coordinate space actually answered**.
///
/// §4.5 hit-tests the click point with `GetAccessibleAtPoint`, and the point
/// and the rectangle have to be in the same space or the two checks are about
/// two different places. That was true by construction while this function
/// only ever asked for one type; the GTK4 fallback below made it a fact that
/// has to be carried, so it is returned rather than assumed. See `verify`.
pub(crate) async fn extents_in(
    dbus: &zbus::Connection,
    proxy: &AccessibleProxy<'_>,
) -> Result<(WindowRect, CoordType), Error> {
    let component = ComponentProxy::builder(dbus)
        .destination(proxy.inner().destination().to_owned())
        .map_err(|e| Error::Bus(format!("component destination: {e}")))?
        .path(proxy.inner().path().to_owned())
        .map_err(|e| Error::Bus(format!("component path: {e}")))?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .map_err(|e| Error::Bus(format!("component proxy: {e}")))?;
    let raw = call("extents", component.get_extents(CoordType::Screen)).await?;
    let screen = WindowRect::from(raw);
    // **GTK4 does not implement `Screen` position, and says so as `0,0`.**
    //
    // Measured on the real desktop 2026-08-25, gnome-text-editor, by asking
    // AT-SPI directly for both coordinate types on the same node:
    //
    //     button 'Open'        SCREEN (0,0,76,34)   WINDOW (-1,0,76,34)
    //     button 'New Tab'     SCREEN (0,0,34,34)   WINDOW (81,0,34,34)
    //     button 'Main Menu'   SCREEN (0,0,34,34)   WINDOW (1833,0,34,34)
    //
    // The *sizes* are right and the *positions* are all zero, so every target
    // in a GTK4 window collapses onto the window origin — which is not a
    // near-miss but a badge and a click in the top-left corner for the whole
    // window. §4.5 then refuses, because the point resolves to whatever is
    // actually there, and a refusal is the only reason this was survivable.
    //
    // The comment above is still right about *why* `Screen` was chosen; it
    // was measured on gnome-terminal, which is GTK3, and GTK3 implements
    // both. So the rule is narrowed rather than reversed: **fall back to
    // `Window` only when `Screen` reports a zero position and `Window` does
    // not.** A node genuinely at the origin has both at zero and is
    // unaffected; a popup's contents report a non-zero `Screen` and never
    // reach this branch at all, so §5.1f's fix stands untouched.
    //
    // The second round trip is paid only by nodes that answered zero, which
    // on GTK3 is the frame and little else.
    //
    // **And `GetAccessibleAtPoint(Screen)` is unimplemented in the same way**,
    // which is the half that was missed on 2026-08-25 and cost every GTK4
    // click for a day. Measured on the real desktop 2026-08-26, asking one
    // Nautilus frame for three points that are 1,000 px apart:
    //
    //     point (window)   Screen                    Window
    //     16,17            toggle button 'Search…'   -> 'Search Everywhere'
    //     191,17           toggle button 'Search…'   -> button 'Main Menu'
    //     1075,17          toggle button 'Search…'   -> button 'Close'
    //
    // `Screen` answers with the node at the window origin for **every** point,
    // so §4.5's check 2 could never resolve to the claim and **every target in
    // every GTK4 window was refused** — 27 of 27 in Nautilus. `Window`
    // hit-tests correctly, and returns the deepest descendant, which is what
    // the ancestry walk in `verify` already expects.
    //
    // So the coordinate type is returned with the rectangle: whichever space
    // answered here is the space the point must be asked in.
    if screen.x == 0 && screen.y == 0 {
        let raw = call("extents", component.get_extents(CoordType::Window)).await?;
        let window = WindowRect::from(raw);
        if window.x != 0 || window.y != 0 {
            return Ok((window, CoordType::Window));
        }
    }
    Ok((screen, CoordType::Screen))
}
