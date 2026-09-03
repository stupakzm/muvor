//! Reading one window's tree (plan.md §4.2).
//!
//! Four ways in, and the choice is **probed per frame, never assumed**:
//!
//! ```text
//! mirror warm for this app?  -> map lookup                              ~0 ms
//! Collection present?        -> GetMatches(roles, VISIBLE+SHOWING+SENSITIVE)
//! otherwise                  -> Cache.GetItems + filter in-process
//! then                       -> pipelined GetExtents over survivors only
//! ```
//!
//! GTK4 does not implement `Collection` — Nautilus exposes only
//! `Accessible`, `Action` and `Component` — while GTK3, Clutter and Chromium
//! do. So the fast path is not universal, and a build that assumed either
//! one is broken on half the desktop.
//!
//! Nothing here returns bounds without asking for them: neither `GetItems`
//! nor `GetMatches` carries extents, and `GetExtents` is ~0.08 ms a call, so
//! extents are always a second pass — over survivors only, and pipelined
//! ([`crate::pipeline`]). That pass is the irreducible cost of a hint, which
//! is why the mirror exists to remove everything around it.

use std::time::{Duration, Instant};

use atspi::proxy::accessible::{AccessibleProxy, ObjectRefExt};
use atspi::proxy::cache::CacheProxy;
use atspi::zbus::names::UniqueName;
use atspi::zbus::zvariant::ObjectPath;
use atspi::{zbus, AccessibilityConnection, ObjectRef, ObjectRefOwned, Role};

use crate::bus::{call, extents, Shared};
use crate::filter::{self, Reject};
use crate::mirror::{AppTree, Node, WarmSource};
use crate::pipeline::join_all;
use crate::rule;
use crate::target::{Frame, Provenance, Target, WindowRect};
use crate::Error;

/// Guard against a pathological application, and **not** the primary
/// mechanism — §4.2 is the primary mechanism. A window that really has more
/// than 500 actionable things in it cannot be labelled with two keystrokes
/// anyway (M3), so truncating is honest as well as cheap.
pub const MAX_NODES: usize = 500;

/// How many *tree nodes* the walk will read before giving up.
///
/// Not [`MAX_NODES`], and the difference cost a wrong answer: that cap is
/// about how many things can be *labelled*, and 500 is generous there. A
/// **tree** is a different size entirely — LibreOffice Writer's is over two
/// thousand nodes and holds 64 actionable ones. Sharing the constant made
/// the walk stop at 500 nodes and silently return a subtree, which is how
/// Writer's whole right-hand sidebar went missing (§4.2-ii-g).
///
/// Truncation here is now reported rather than assumed harmless: an
/// incomplete tree that looks complete is the failure mode this whole
/// section is about.
pub const WALK_NODES: usize = 5000;

/// One node dropped, and why. `--dump --rejects` prints these; without them
/// "detection missed my button" cannot be answered.
#[derive(Debug, Clone)]
pub struct Rejected {
    pub role: Role,
    pub name: String,
    pub path: String,
    pub why: Reject,
}

/// What a warm-up produced.
#[derive(Debug, Clone, Copy)]
pub struct Warmth {
    pub source: WarmSource,
    pub nodes: usize,
    /// Nodes belonging to the window that triggered the warm-up, as opposed
    /// to the rest of the application.
    pub in_window: usize,
}

/// The result of reading one window, with the measurements §5.2 requires.
#[derive(Debug)]
pub struct Scan {
    pub provenance: Provenance,
    pub targets: Vec<Target>,
    pub rejects: Vec<Rejected>,
    /// Nodes the candidate step produced, before extents were fetched.
    pub candidates: usize,
    /// Nodes the candidate step *examined*. Equal to `candidates` on the
    /// `Collection` path, where the filtering happened inside the app.
    pub examined: usize,
    /// Choosing and fetching the candidate set.
    pub select: Duration,
    /// The pipelined extents pass over survivors.
    pub resolve: Duration,
    /// `MAX_NODES` was hit and the tail was dropped.
    pub truncated: bool,
    /// For the mirror path: how old the tree is, how many events have been
    /// applied to it since, and how many arrived at all. Together they are
    /// how a stale mirror — or a dead subscription — is spotted.
    pub mirror_age: Option<Duration>,
    pub mirror_updates: Option<u64>,
    pub mirror_seen: Option<u64>,
}

/// A node on its way to becoming a [`Target`]. `role`/`name` are `None` when
/// the path that produced it did not already know them — `Collection` hands
/// back bare object references; the cache, the walk and the mirror hand back
/// everything except bounds.
struct Candidate {
    obj: ObjectRefOwned,
    role: Option<Role>,
    name: Option<String>,
}

pub(crate) async fn scan(
    conn: &AccessibilityConnection,
    mirror: &Shared,
    frame: &Frame,
    force: Option<Provenance>,
    metadata: bool,
) -> Result<Scan, Error> {
    let t0 = Instant::now();
    let mut walk_cut = false;
    let mut mirror_age = None;
    let mut mirror_updates = None;
    let mut mirror_seen = None;

    let (provenance, mut candidates, examined) = match force {
        Some(Provenance::Mirror) | None => match from_mirror(mirror, frame) {
            Some((cands, examined, age, updates, seen)) => {
                mirror_age = Some(age);
                mirror_updates = Some(updates);
                mirror_seen = Some(seen);
                (Provenance::Mirror, cands, Some(examined))
            }
            // Cold. Fall through to reading it live, which is also what
            // happens for any window activated before muvor started.
            None if force == Some(Provenance::Mirror) => {
                return Err(Error::Bus(format!(
                    "no warm tree for {} — activate the window, or drop --via mirror",
                    frame.bus_name
                )))
            }
            None => {
                let (prov, cands, examined, cut) = probe_live(conn, mirror, frame).await?;
                walk_cut = cut;
                (prov, cands, examined)
            }
        },
        Some(Provenance::Collection) => {
            (Provenance::Collection, collection_path(conn, frame).await?, None)
        }
        Some(Provenance::Cache) => {
            let tree = cache_tree(conn, &frame.bus_name).await?;
            let n = tree.len();
            (Provenance::Cache, candidates_of(&tree, &frame.bus_name, &frame.path), Some(n))
        }
        Some(Provenance::Walk) => {
            let tree = walk_tree(conn, frame).await?;
            let n = tree.len();
            walk_cut = tree.truncated;
            (Provenance::Walk, candidates_of(&tree, &frame.bus_name, &frame.path), Some(n))
        }
    };
    let select = t0.elapsed();

    let truncated = walk_cut || candidates.len() > MAX_NODES;
    candidates.truncate(MAX_NODES);
    let examined = examined.unwrap_or(candidates.len());

    let t1 = Instant::now();
    let (targets, rejects) = resolve(conn, frame, provenance, candidates, metadata).await;
    Ok(Scan {
        provenance,
        candidates: targets.len() + rejects.len(),
        examined,
        targets,
        rejects,
        select,
        resolve: t1.elapsed(),
        truncated,
        mirror_age,
        mirror_updates,
        mirror_seen,
    })
}

/// The probe proper, for a window with no warm tree. Trying `GetMatches`
/// *is* the probe: an application that does not implement `Collection`
/// answers with an error in one round trip, which is cheaper than the
/// `Introspect` it would take to ask politely, and there is nothing else the
/// answer could mean.
///
/// Asked **once per application** and remembered (§4.2-ii-d): the answer is a
/// property of the toolkit, and on GTK4 — where it is always "no" — it is a
/// round trip that exists only to fail.
///
/// The fallback is the **walk, not the cache**. That is the survey's finding
/// and it costs 18–47 ms: `Cache.GetItems` answered a cold Nautilus with five
/// nodes that were all ghosts of a dismissed popover, a cold Calculator with
/// nothing at all, and Chromium with four live widgets belonging to a
/// different window (§4.2-ii-c … -e). A slow correct answer can be moved off
/// the hotkey path by the mirror; a fast wrong one cannot be moved anywhere.
async fn probe_live(
    conn: &AccessibilityConnection,
    mirror: &Shared,
    frame: &Frame,
) -> Result<(Provenance, Vec<Candidate>, Option<usize>, bool), Error> {
    let known = mirror.lock().expect("mirror mutex").collection_capable(&frame.bus_name);
    if known != Some(false) {
        match collection_path(conn, frame).await {
            Ok(c) => {
                mirror.lock().expect("mirror mutex").note_collection(&frame.bus_name, true);
                return Ok((Provenance::Collection, c, None, false));
            }
            Err(_) => {
                mirror.lock().expect("mirror mutex").note_collection(&frame.bus_name, false);
            }
        }
    }
    let tree = walk_tree(conn, frame).await?;
    let n = tree.len();
    let cut = tree.truncated;
    Ok((Provenance::Walk, candidates_of(&tree, &frame.bus_name, &frame.path), Some(n), cut))
}

/// A map lookup, which is the entire point of §4.2-iii.
fn from_mirror(
    mirror: &Shared,
    frame: &Frame,
) -> Option<(Vec<Candidate>, usize, Duration, u64, u64)> {
    let m = mirror.lock().expect("mirror mutex");
    let seen = m.seen;
    let tree = m.app(&frame.bus_name)?;
    if tree.is_empty() {
        return None;
    }
    let cands = candidates_of(tree, &frame.bus_name, &frame.path);

    // **An empty answer is not an answer.** The mirror is keyed by
    // application but warmed from the subtree of whichever *window* was
    // activated, so a second window of the same application finds a tree
    // full of nodes, none of which are its own — and 0 candidates from a
    // warm mirror is indistinguishable from a window with no tree at all.
    //
    // Measured 2026-08-19: opening a second Nautilus window and hinting the
    // first reported *"no targets in this window (235 examined)"* — D12's
    // free-mode message, for a window with 24 buttons in it. Falling through
    // to a live read costs the walk and is never wrong; answering costs
    // nothing and was wrong here.
    if cands.is_empty() {
        return None;
    }
    Some((cands, tree.len(), tree.warmed.elapsed(), tree.updates, seen))
}

/// Every node in a tree belongs to the connection that published it, and the
/// mirror is keyed by that name, so the bus name comes from the frame rather
/// than from the node.
fn candidates_of(tree: &AppTree, bus_name: &str, frame_path: &str) -> Vec<Candidate> {
    tree.candidates(frame_path)
        .into_iter()
        .filter_map(|(path, node)| {
            Some(Candidate {
                obj: object_ref(bus_name, path).ok()?,
                role: Some(node.role),
                name: Some(node.name.clone()),
            })
        })
        .collect()
}

/// `Collection.GetMatches` — the whole of §4.4's role and state test, pushed
/// into the application so that the nodes that fail it never cross the bus.
///
/// The rule is built by hand ([`crate::rule`]) rather than with the `atspi`
/// crate's `ObjectMatchRule`, which produces a rule this server answers with
/// silence. See that module for the three reasons why.
async fn collection_path(
    conn: &AccessibilityConnection,
    frame: &Frame,
) -> Result<Vec<Candidate>, Error> {
    let path = ObjectPath::try_from(frame.path.as_str())
        .map_err(|e| Error::Bus(format!("frame path: {e}")))?;
    let body = (
        rule::actionable(),
        rule::SORT_CANONICAL,
        // Capped in the request as well as after the fact: worth more as "do
        // not send me 10,000 objects" than as "drop the tail once they are
        // here".
        MAX_NODES as i32,
        // Must be true. §4.2-ii: with `traverse: false` this returns an empty
        // array, whatever the rule says.
        true,
    );
    let reply = call(
        "Collection.GetMatches",
        conn.connection().call_method(
            Some(frame.bus_name.as_str()),
            &path,
            Some("org.a11y.atspi.Collection"),
            "GetMatches",
            &body,
        ),
    )
    .await?;
    let matches: Vec<ObjectRefOwned> = reply
        .body()
        .deserialize()
        .map_err(|e| Error::Bus(format!("GetMatches reply: {e}")))?;

    Ok(matches
        .into_iter()
        .map(|obj| Candidate { obj, role: None, name: None })
        .collect())
}

/// `Cache.GetItems` — one call for the application's entire tree.
///
/// Complete on atk-backed toolkits and **lazily populated on GTK4**, where a
/// cold window answers with a fraction of itself (§4.2-ii trap 3). That is
/// what [`warm`] exists to notice.
pub(crate) async fn cache_tree(
    conn: &AccessibilityConnection,
    bus_name: &str,
) -> Result<AppTree, Error> {
    let proxy = CacheProxy::builder(conn.connection())
        .destination(bus_name.to_owned())
        .map_err(|e| Error::Bus(format!("cache destination: {e}")))?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .map_err(|e| Error::Bus(format!("cache proxy: {e}")))?;

    let items = call("Cache.GetItems", proxy.get_items()).await?;
    let mut tree = AppTree::new(WarmSource::Cache);
    for item in &items {
        tree.insert_item(item);
    }
    Ok(tree)
}

/// The naive recursive walk. Never chosen by the probe (§4.2) — it is the
/// reference the two fast paths are checked against, and it is also the only
/// way to force a lazily-populated GTK4 cache into existence.
pub(crate) async fn walk_tree(
    conn: &AccessibilityConnection,
    frame: &Frame,
) -> Result<AppTree, Error> {
    let dbus = conn.connection();
    let mut tree = AppTree::new(WarmSource::Walk);
    let mut level = vec![(object_ref(&frame.bus_name, &frame.path)?, String::new())];

    for _ in 0..crate::mirror::DEPTH_LIMIT {
        if level.is_empty() {
            break;
        }
        if tree.len() >= WALK_NODES {
            tree.truncated = true;
            break;
        }
        // One level at a time, every node in the level concurrently. A
        // node-at-a-time walk of Nautilus costs 153 ms; this is the same
        // number of round trips arranged so they overlap, which is the
        // fairest version of the thing being compared against.
        let described = join_all(level.iter().map(|(o, _)| describe_walked(dbus, o)).collect()).await;

        let mut next = Vec::new();
        for ((obj, parent), described) in level.into_iter().zip(described) {
            let Some((node, children)) = described else { continue };
            let path = obj.path_as_str().to_owned();
            let node = Node { parent: if parent.is_empty() { node.parent } else { parent }, ..node };
            tree.insert(path.clone(), node);
            next.extend(children.into_iter().map(|c| (c, path.clone())));
        }
        level = next;
    }
    Ok(tree)
}

type Walked = Option<(Node, Vec<ObjectRefOwned>)>;

async fn describe_walked(dbus: &zbus::Connection, obj: &ObjectRefOwned) -> Walked {
    let proxy = obj.as_accessible_proxy(dbus).await.ok()?;
    let role = call("walk role", proxy.get_role()).await.ok()?;
    let name = call("walk name", proxy.name()).await.unwrap_or_default();
    let states = call("walk states", proxy.get_state()).await.ok()?;
    let ifaces = call("walk interfaces", proxy.get_interfaces()).await.ok()?;
    let children = call("walk children", proxy.get_children()).await.unwrap_or_default();
    Some((
        Node {
            parent: String::new(),
            role,
            name,
            states,
            has_component: ifaces.contains(atspi::Interface::Component),
        },
        children,
    ))
}

/// Build or rebuild the mirror for one application (§4.2-iii).
///
/// `GetItems` first, because it is one call. Then the completeness question,
/// which has a measured answer rather than a guess: if the application does
/// not implement `Collection` it is GTK4, its cache is lazily populated, and
/// a cold window will have answered with a fraction of itself — so it is
/// walked, which both fills the mirror and forces the application's own
/// cache into existence for every later event.
pub(crate) async fn warm(
    conn: &AccessibilityConnection,
    mirror: &Shared,
    bus_name: &str,
    frame_path: &str,
) -> Result<Warmth, Error> {
    // Walked from the **application's root**, not from the window that was
    // activated (§2.5, M5s-c).
    //
    // The mirror is keyed by application, and a per-window warm-up fills it
    // with one window's nodes: measured 2026-08-19, opening a second Nautilus
    // window and hinting the first found a tree of 235 nodes containing none
    // of that window's 24 targets. An application's windows are cheap
    // relative to the round trip that got here, and half a mirror is worse
    // than none — it costs the walk anyway (the empty answer falls through)
    // and spends the activation warming the wrong thing.
    let root = Frame {
        focus: crate::target::FocusSource::Activate,
        pid_memo: false,
        title_memo: false,
        app: String::new(),
        title: String::new(),
        role: Role::Frame,
        bus_name: bus_name.to_owned(),
        path: "/org/a11y/atspi/accessible/root".to_owned(),
        bounds: WindowRect::new(0, 0, 0, 0),
    };
    let frame = root;

    // An application with a working `Collection` needs no mirror: it answers
    // the whole of §4.4 in one round trip, measured at 0.4 ms (Terminal) and
    // 1.1 ms (Chromium). Mirroring it would be machinery in front of
    // something already fast enough — and would cost a 164 ms walk per
    // activation to build.
    let capable = has_collection(conn, &frame).await;
    mirror.lock().expect("mirror mutex").note_collection(bus_name, capable);
    if capable {
        return Ok(Warmth { source: WarmSource::Live, nodes: 0, in_window: 0 });
    }

    // **The walk, and only the walk** (§4.2-ii-c … -e). `GetItems` is one
    // call and it was tried first here for exactly that reason, guarded by a
    // "did this produce candidates" test — and the survey found the guard
    // passing on garbage. A cold Nautilus produced five candidates and all
    // five were ghosts of a dismissed popover; Chromium produced fifteen, of
    // which four were live widgets in a *different toplevel of the same
    // application*, at coordinates that are real somewhere else.
    //
    // So the cache is not merged in either, not even to fill gaps the walk
    // did not reach: every node it offered that the walk did not was wrong in
    // one of those two ways. The walk asks the objects themselves, it has
    // been right in every window measured, and 18–47 ms is affordable here
    // because here is `window:activate` and nobody is waiting.
    let tree = walk_tree(conn, &frame).await?;
    let warmth = Warmth {
        source: tree.source,
        nodes: tree.len(),
        in_window: tree.candidates(frame_path).len(),
    };
    mirror.lock().expect("mirror mutex").put(bus_name.to_owned(), tree);
    Ok(warmth)
}

/// Spawnable form, for the event loop. A warm-up that fails is not worth
/// interrupting anything over: the next read falls back to the live paths,
/// which is where it would have been anyway.
pub(crate) async fn warm_app(
    conn: AccessibilityConnection,
    mirror: Shared,
    bus_name: String,
    frame_path: String,
) {
    let _ = warm(&conn, &mirror, &bus_name, &frame_path).await;
}

/// One round trip, asking for at most one match — the cheapest true answer
/// to "is this GTK4".
async fn has_collection(conn: &AccessibilityConnection, frame: &Frame) -> bool {
    let Ok(path) = ObjectPath::try_from(frame.path.as_str()) else { return false };
    let body = (rule::actionable(), rule::SORT_CANONICAL, 1i32, true);
    call(
        "Collection probe",
        conn.connection().call_method(
            Some(frame.bus_name.as_str()),
            &path,
            Some("org.a11y.atspi.Collection"),
            "GetMatches",
            &body,
        ),
    )
    .await
    .is_ok()
}

fn object_ref(bus_name: &str, path: &str) -> Result<ObjectRefOwned, Error> {
    let name = UniqueName::try_from(bus_name)
        .map_err(|e| Error::Bus(format!("bus name '{bus_name}': {e}")))?
        .to_owned();
    let path = ObjectPath::try_from(path)
        .map_err(|e| Error::Bus(format!("object path: {e}")))?
        .into_owned();
    Ok(ObjectRefOwned::new(ObjectRef::new(name, path)))
}

/// The second pass: everything the candidate step could not know, fetched
/// for survivors only and all at once.
async fn resolve(
    conn: &AccessibilityConnection,
    frame: &Frame,
    provenance: Provenance,
    candidates: Vec<Candidate>,
    metadata: bool,
) -> (Vec<Target>, Vec<Rejected>) {
    let dbus = conn.connection();
    let clip = frame.clip();
    let resolved =
        join_all(candidates.iter().map(|c| resolve_one(dbus, c, metadata)).collect()).await;

    let mut targets: Vec<Target> = Vec::new();
    let mut rejects = Vec::new();
    for (c, r) in candidates.into_iter().zip(resolved) {
        let path = c.obj.path_as_str().to_owned();
        let Some((role, name, bounds)) = r else {
            rejects.push(Rejected {
                role: c.role.unwrap_or(Role::Invalid),
                name: c.name.unwrap_or_default(),
                path,
                why: Reject::NoBounds,
            });
            continue;
        };
        match filter::bounds_ok(bounds, clip) {
            Ok(()) => targets.push(Target {
                role,
                name,
                bounds,
                provenance,
                bus_name: c.obj.name_as_str().unwrap_or_default().to_owned(),
                path,
            }),
            Err(why) => rejects.push(Rejected { role, name, path, why }),
        }
    }
    dedupe(&mut targets, &mut rejects);
    (targets, rejects)
}

/// Drop targets that click the same pixel as one already kept (§4.4).
///
/// O(n²) over a list capped at 500 and typically under 40, comparing two
/// integers — the shape that matters here is determinism, not asymptotics.
fn dedupe(targets: &mut Vec<Target>, rejects: &mut Vec<Rejected>) {
    let mut kept: Vec<Target> = Vec::with_capacity(targets.len());
    for t in targets.drain(..) {
        match kept.iter().position(|k| filter::same_spot(k.bounds, t.bounds)) {
            None => kept.push(t),
            Some(i) => {
                let incumbent = &kept[i];
                let challenger_wins = filter::prefer(
                    (t.bounds, t.role, &t.path),
                    (incumbent.bounds, incumbent.role, &incumbent.path),
                )
                .is_lt();
                let loser = if challenger_wins { std::mem::replace(&mut kept[i], t) } else { t };
                rejects.push(Rejected {
                    role: loser.role,
                    name: loser.name,
                    path: loser.path,
                    why: Reject::Duplicate,
                });
            }
        }
    }
    *targets = kept;
}

async fn resolve_one(
    dbus: &zbus::Connection,
    c: &Candidate,
    metadata: bool,
) -> Option<(Role, String, WindowRect)> {
    let proxy: AccessibleProxy<'_> = c.obj.as_accessible_proxy(dbus).await.ok()?;
    // Only what is missing, and only when it is wanted. Everything except
    // `Collection` already knows role and name; `Collection` would pay two
    // extra round trips per survivor to learn them — most of its cost on a
    // window with many targets, and worth nothing to a hint. M3 labels by
    // position (D5), so a hint needs bounds and nothing else; role and name
    // are for `--dump` and for validate-at-action (§4.5).
    let role = match c.role {
        Some(r) => r,
        None if !metadata => Role::Invalid,
        None => call("role", proxy.get_role()).await.ok()?,
    };
    let name = match &c.name {
        Some(n) => n.clone(),
        None if !metadata => String::new(),
        None => call("name", proxy.name()).await.unwrap_or_default(),
    };
    let bounds = extents(dbus, &proxy).await.ok()?;
    Some((role, name, bounds))
}
