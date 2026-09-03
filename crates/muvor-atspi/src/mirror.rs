//! A live copy of the applications' accessibility trees (§4.2-iii, D15).
//!
//! **Why this exists.** Reading a GTK4 window costs 12.2 ms, of which 7.3 ms
//! is `Cache.GetItems` re-fetching the *entire application* before a single
//! node is filtered — and a cold GTK4 cache answers with a fraction of the
//! tree, so the cheap read is not merely slow but wrong (§4.2-ii trap 3).
//! Neither problem has a fix at read time; both disappear if the tree is
//! already in hand.
//!
//! So muvor holds one, warms it when a window is activated (D14 supplies the
//! moment for free) and keeps it current from `Cache` and state-change
//! events. The hotkey path then costs a map lookup plus one pipelined pass of
//! `GetExtents` — the only thing AT-SPI will not tell you in advance.
//!
//! **What can go wrong, and why that is acceptable.** A mirror is state, and
//! state goes stale: an event can be missed, and a node can change between
//! the last event and the keypress. That is exactly the risk §4.5 already
//! answers — every click is validated against the live tree before it is
//! sent, and a target that cannot be re-identified is not clicked. The mirror
//! makes hints fast; it is never the last word on what gets clicked.

use std::collections::HashMap;
use std::time::Instant;

use atspi::{CacheItem, Interface, Role, State, StateSet};

use crate::filter;

/// Depth cap for ancestry walks. Deep enough for a real GTK hierarchy —
/// Nautilus nests about fifteen levels — and shallow enough that a cyclic or
/// malformed tree terminates as a dropped node rather than a hang.
pub const DEPTH_LIMIT: usize = 32;

/// One node, as much of it as can be known without asking for extents.
#[derive(Debug, Clone)]
pub struct Node {
    pub parent: String,
    pub role: Role,
    pub name: String,
    pub states: StateSet,
    /// No `Component`, no extents, so no bounds and therefore no target.
    pub has_component: bool,
}

/// One application's tree, keyed by object path.
#[derive(Debug)]
pub struct AppTree {
    nodes: HashMap<String, Node>,
    /// When the tree was last built from scratch.
    pub warmed: Instant,
    /// How the warm-up was done, and therefore how much to trust it.
    pub source: WarmSource,
    /// Events applied since. A tree with a warm-up and no events is either a
    /// still application or a missed subscription; the count is what tells
    /// the two apart in `--dump`.
    pub updates: u64,
    /// When the last event was applied to this tree.
    ///
    /// The count alone cannot answer the question that matters — *are events
    /// still arriving* — because a tree that stopped receiving them an hour
    /// ago and one still being fed look identical from a total. This is the
    /// difference between a tree that is old and one that is stale (M5s-d).
    pub last_update: Option<Instant>,
    /// The walk stopped at its node budget and this tree is a prefix of the
    /// real one.
    ///
    /// Recorded rather than assumed harmless: a truncated walk of LibreOffice
    /// returned 52 of its 64 targets and looked exactly like a complete
    /// answer, which is the failure §4.2-ii-g is named after.
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarmSource {
    /// `Cache.GetItems` — one call, and trustworthy only when its parent
    /// chains actually reach the window. Verified, never assumed.
    Cache,
    /// A recursive walk, which is what forces a lazily-populated GTK4 cache
    /// to exist at all, and the only source that asks the objects directly.
    Walk,
    /// Not mirrored at all: this application answers `Collection` in
    /// milliseconds, so a mirror would be machinery in front of something
    /// already fast enough. Chromium and GTK3 land here.
    Live,
}

impl AppTree {
    pub fn new(source: WarmSource) -> Self {
        Self {
            nodes: HashMap::new(),
            warmed: Instant::now(),
            source,
            updates: 0,
            last_update: None,
            truncated: false,
        }
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn get(&self, path: &str) -> Option<&Node> {
        self.nodes.get(path)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &Node)> {
        self.nodes.iter().map(|(k, v)| (k.as_str(), v))
    }

    pub fn insert(&mut self, path: String, node: Node) {
        self.nodes.insert(path, node);
    }

    pub fn insert_item(&mut self, item: &CacheItem) {
        self.insert(
            item.object.path_as_str().to_owned(),
            Node {
                parent: item.parent.path_as_str().to_owned(),
                role: item.role,
                name: if item.name.is_empty() { item.short_name.clone() } else { item.name.clone() },
                states: item.states,
                has_component: item.ifaces.contains(Interface::Component),
            },
        );
    }

    pub fn remove(&mut self, path: &str) {
        self.nodes.remove(path);
    }

    /// Apply one `object:state-changed`. Unknown nodes are ignored rather
    /// than invented: a state change for something never seen means the
    /// mirror is missing a subtree, and guessing at it would be worse than
    /// leaving the gap visible.
    pub fn set_state(&mut self, path: &str, state: State, on: bool) -> bool {
        let Some(node) = self.nodes.get_mut(path) else { return false };
        if on {
            node.states.insert(state);
        } else {
            node.states.remove(state);
        }
        true
    }

    /// Does `path` sit under `ancestor`? Depth-limited, so a cycle ends the
    /// search instead of the process.
    pub fn descends_from(&self, path: &str, ancestor: &str) -> bool {
        let mut cur = path;
        for _ in 0..DEPTH_LIMIT {
            if cur == ancestor {
                return true;
            }
            match self.nodes.get(cur) {
                Some(node) => cur = &node.parent,
                None => return false,
            }
        }
        false
    }

    /// Everything under `frame` that passes the role and state halves of
    /// §4.4. Bounds are not here and cannot be — that is the pass this whole
    /// module exists to isolate.
    pub fn candidates(&self, frame: &str) -> Vec<(&str, &Node)> {
        self.nodes
            .iter()
            .filter(|(_, n)| n.has_component)
            .filter(|(_, n)| filter::role_is_actionable(n.role))
            .filter(|(_, n)| filter::states_ok(n.states).is_ok())
            .filter(|(path, _)| self.descends_from(path, frame))
            .map(|(path, node)| (path.as_str(), node))
            .collect()
    }
}

/// Every application muvor has looked at, keyed by D-Bus name.
#[derive(Debug, Default)]
pub struct Mirror {
    apps: HashMap<String, AppTree>,
    /// Every event the drain task has received.
    ///
    /// Not diagnostics for their own sake: "0 events applied" has two very
    /// different causes — a desktop where nothing relevant changed, and a
    /// subscription that silently never delivered — and without this counter
    /// they are indistinguishable.
    ///
    /// It counts what **matches our subscriptions**, not what crosses the
    /// bus: the daemon filters against our match rules before delivery.
    /// Measured 2026-08-19, and worth knowing — registering *any* `object:`
    /// event with the registry makes applications emit the whole family, so
    /// an idle desktop puts ~100 `object:text-changed` per ten seconds on
    /// the bus that muvor never has to read. Subscribing wider than the
    /// mirror needs would import all of it.
    pub seen: u64,
    /// When the last event arrived, on any application. `None` means none
    /// ever has, which is a different fault from a desktop nobody is using.
    pub last_event: Option<Instant>,
    /// Whether an application implements `org.a11y.atspi.Collection`, once
    /// asked (§4.2-ii-d).
    ///
    /// The answer is a property of the toolkit and cannot change while the
    /// application lives, and asking costs a round trip that **fails** on
    /// GTK4 — the whole of GNOME's future. Ask once per connection, keep it,
    /// and let every later scan go straight to the path that works.
    collection: HashMap<String, bool>,
    /// Which accessibility connection belongs to which process (§2.5, M5s-c).
    ///
    /// Tying the compositor's window to its tree costs one
    /// `GetConnectionUnixProcessID` **per application on the bus** until the
    /// pid matches — fourteen of them here, and 6.7 ms of a 17 ms hint. A
    /// connection's pid cannot change while the connection lives, so the
    /// answer is worth exactly as much the second time and costs nothing.
    ///
    /// Wrong only if a name is reused for a different process, which is why
    /// the lookup **verifies the memo before trusting it** — one round trip
    /// against fourteen.
    app_for_pid: HashMap<u32, String>,
    /// The connection whose window last answered a given title.
    ///
    /// The pid memo cannot help an application the compositor reports under
    /// a pid no accessibility connection owns, and that is not a corner
    /// case — it is Nautilus, every time (§4.2-ii-c). Such a window reaches
    /// the title fallback on every hint, and the fallback is the most
    /// expensive path there is: every window of every application, three
    /// round trips each. Remembering which connection answered turns the
    /// second and later lookups into one.
    ///
    /// Keyed by the lowercased title muvor searched for, which is what the
    /// compositor supplied — not by pid, because the pid is the thing that
    /// did not work.
    app_for_title: HashMap<String, String>,
    /// Processes known to own no accessibility connection at all.
    ///
    /// The positive memo saves the *scan*; this saves the **search** that
    /// runs before it. A pid that matches nothing still costs one
    /// `GetConnectionUnixProcessID` per application on the bus — fourteen
    /// here — and pays it on every hint, because there is no answer to
    /// remember. Remembering the *absence* is the other half, and without it
    /// the title memo barely showed: measured 2026-08-19, `frame` went
    /// 7.54 → 6.28 ms with the positive memo alone.
    ///
    /// Given up on rather than re-checked: the title path finds the window
    /// anyway, so the cost of a stale entry is that muvor keeps calling the
    /// window `Named` instead of `Compositor` — a weaker claim about
    /// identity, not a wrong one. Cleared whenever the title memo misses,
    /// which is when something about the window has changed.
    pid_without_app: std::collections::HashSet<u32>,
}

impl Mirror {
    pub fn app(&self, bus_name: &str) -> Option<&AppTree> {
        self.apps.get(bus_name)
    }

    /// What is known about this application's `Collection` support, or
    /// `None` if it has never been asked.
    pub fn collection_capable(&self, bus_name: &str) -> Option<bool> {
        self.collection.get(bus_name).copied()
    }

    pub fn note_collection(&mut self, bus_name: &str, capable: bool) {
        self.collection.insert(bus_name.to_owned(), capable);
    }

    /// The connection last seen to belong to `pid`, if any.
    pub fn app_for_pid(&self, pid: u32) -> Option<String> {
        self.app_for_pid.get(&pid).cloned()
    }

    pub fn note_app_for_pid(&mut self, pid: u32, bus_name: &str) {
        self.app_for_pid.insert(pid, bus_name.to_owned());
    }

    pub fn forget_app_for_pid(&mut self, pid: u32) {
        self.app_for_pid.remove(&pid);
    }

    /// Which connection answered this title last time, if one did.
    pub fn app_for_title(&self, title: &str) -> Option<String> {
        self.app_for_title.get(title).cloned()
    }

    pub fn note_app_for_title(&mut self, title: &str, bus_name: &str) {
        self.app_for_title.insert(title.to_owned(), bus_name.to_owned());
    }

    /// Forget a memo that did not pay off — the window closed, the title
    /// changed, the application restarted. Never sticky: a wrong memo must
    /// cost one slow lookup, not every lookup after it.
    pub fn forget_app_for_title(&mut self, title: &str) {
        self.app_for_title.remove(title);
    }

    /// Has this pid already been searched for, and found to own nothing?
    pub fn pid_owns_nothing(&self, pid: u32) -> bool {
        self.pid_without_app.contains(&pid)
    }

    pub fn note_pid_owns_nothing(&mut self, pid: u32) {
        self.pid_without_app.insert(pid);
    }

    pub fn forget_pid_owns_nothing(&mut self, pid: u32) {
        self.pid_without_app.remove(&pid);
    }

    pub fn app_mut(&mut self, bus_name: &str) -> Option<&mut AppTree> {
        self.apps.get_mut(bus_name)
    }

    pub fn put(&mut self, bus_name: String, tree: AppTree) {
        self.apps.insert(bus_name, tree);
    }

    /// An application went away. Dropping its whole tree is the only correct
    /// response: every path in it belonged to that connection.
    pub fn forget(&mut self, bus_name: &str) {
        self.apps.remove(bus_name);
    }

    pub fn apps(&self) -> impl Iterator<Item = (&str, &AppTree)> {
        self.apps.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// A snapshot for `--dump`, and the only window onto whether the feed is
    /// alive: an application warmed by an activation muvor was *not* asked
    /// about is proof that the event path works end to end.
    pub fn stats(&self) -> MirrorStats {
        let mut apps: Vec<AppStats> = self
            .apps
            .iter()
            .map(|(bus, t)| AppStats {
                bus_name: bus.clone(),
                nodes: t.len(),
                source: t.source,
                updates: t.updates,
                age: t.warmed.elapsed(),
                quiet: t.last_update.map(|t| t.elapsed()),
            })
            .collect();
        apps.sort_by_key(|a| a.age);
        MirrorStats { seen: self.seen, apps, quiet: self.last_event.map(|t| t.elapsed()) }
    }
}

#[derive(Debug, Clone)]
pub struct MirrorStats {
    /// Events delivered to muvor — see [`Mirror::seen`].
    pub seen: u64,
    pub apps: Vec<AppStats>,
    /// Time since *any* event arrived, on any application. `None` means none
    /// ever has — which is §4.7a's symptom, not a quiet desktop.
    pub quiet: Option<std::time::Duration>,
}

#[derive(Debug, Clone)]
pub struct AppStats {
    pub bus_name: String,
    pub nodes: usize,
    pub source: WarmSource,
    pub updates: u64,
    pub age: std::time::Duration,
    /// Time since the last event was applied. `None` means none ever was.
    pub quiet: Option<std::time::Duration>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(parent: &str, role: Role) -> Node {
        Node {
            parent: parent.to_owned(),
            role,
            name: String::new(),
            states: [State::Visible, State::Showing, State::Sensitive].into_iter().collect(),
            has_component: true,
        }
    }

    fn tree() -> AppTree {
        let mut t = AppTree::new(WarmSource::Cache);
        t.insert("/frame".into(), node("/root", Role::Frame));
        t.insert("/box".into(), node("/frame", Role::Panel));
        t.insert("/button".into(), node("/box", Role::Button));
        t.insert("/other-window".into(), node("/root", Role::Frame));
        t.insert("/other-button".into(), node("/other-window", Role::Button));
        t
    }

    #[test]
    fn a_nested_button_is_a_candidate_of_its_frame() {
        let t = tree();
        let found = t.candidates("/frame");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, "/button");
    }

    /// `GetItems` returns the whole application, so scoping to one window is
    /// this test and nothing else (§4.3).
    #[test]
    fn another_windows_button_is_not() {
        let t = tree();
        assert!(t.candidates("/frame").iter().all(|(p, _)| *p != "/other-button"));
        assert_eq!(t.candidates("/other-window").len(), 1);
    }

    #[test]
    fn a_cycle_terminates_at_the_depth_limit() {
        let mut t = AppTree::new(WarmSource::Cache);
        t.insert("/a".into(), node("/b", Role::Button));
        t.insert("/b".into(), node("/a", Role::Panel));
        assert!(!t.descends_from("/a", "/frame"));
        assert!(t.candidates("/frame").is_empty());
    }

    #[test]
    fn a_state_change_removes_a_target_without_a_refetch() {
        let mut t = tree();
        assert_eq!(t.candidates("/frame").len(), 1);
        assert!(t.set_state("/button", State::Sensitive, false));
        assert!(t.candidates("/frame").is_empty(), "a greyed-out button is not a target");
        assert!(t.set_state("/button", State::Sensitive, true));
        assert_eq!(t.candidates("/frame").len(), 1);
    }

    #[test]
    fn a_state_change_for_an_unknown_node_is_reported_not_invented() {
        let mut t = tree();
        assert!(!t.set_state("/never-seen", State::Showing, true));
        assert!(t.get("/never-seen").is_none());
    }

    #[test]
    fn a_removed_node_stops_being_a_target() {
        let mut t = tree();
        t.remove("/button");
        assert!(t.candidates("/frame").is_empty());
    }

    #[test]
    fn a_node_without_component_is_never_a_candidate() {
        let mut t = tree();
        let mut n = node("/box", Role::Button);
        n.has_component = false;
        t.insert("/no-bounds".into(), n);
        assert_eq!(t.candidates("/frame").len(), 1);
    }
}
