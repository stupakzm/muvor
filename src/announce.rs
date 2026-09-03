//! Layer 0: telling the desktop that an assistive technology is here (§4.7a).
//!
//! `org.a11y.Status.IsEnabled` is the switch that decides whether
//! applications emit AT-SPI events *at all*. With it false, GTK apps expose
//! a tree when asked but never announce a change to it, so the mirror is
//! never warmed, browsers look empty, and `muvor status` reads `0 events
//! seen` while everything else looks healthy. It was false on this machine
//! until 2026-08-19, and it is why Chromium appeared to have no buttons.
//!
//! It is **session state, not muvor's state**: a per-session D-Bus property
//! that other assistive technologies share. So the rule is the one any
//! well-behaved program follows with a global it did not own — set it if it
//! must, remember what it was, and put it back on the way out. A daemon is
//! stopped by a signal rather than by returning from `main`, which is why
//! [`Announce::on_signal`] exists and why `tokio`'s `signal` feature is a
//! dependency.
//!
//! Failing to set it is **not fatal**. A desktop where another AT already
//! turned it on works perfectly; so does one where muvor cannot reach the
//! property at all, until something needs an event. The daemon says what
//! happened and carries on — the same rule the session itself follows.

use std::sync::Mutex;

const DEST: &str = "org.a11y.Bus";
const PATH: &str = "/org/a11y/bus";
const IFACE: &str = "org.a11y.Status";
const PROP: &str = "IsEnabled";

type Fallible<T> = Result<T, Box<dyn std::error::Error>>;

/// muvor's claim on the announce flag, and its promise to give it back.
pub struct Announce {
    rt: tokio::runtime::Runtime,
    conn: zbus::Connection,
    /// `Some(previous)` while muvor is the one that changed it. `None` when
    /// there is nothing owed: it was already true, or it has been restored.
    ///
    /// A `Mutex` because restoring happens from whichever of two places gets
    /// there first — the signal thread or the end of `serve` — and doing it
    /// twice must be harmless.
    owed: Mutex<Option<bool>>,
}

impl Announce {
    /// Read the flag, set it if it is false, and remember what it was.
    pub fn claim() -> Fallible<Self> {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        let conn = rt.block_on(zbus::Connection::session())?;
        let me = Self { rt, conn, owed: Mutex::new(None) };
        let was = me.read()?;
        if !was {
            me.write(true)?;
            *me.owed.lock().expect("announce lock") = Some(was);
        }
        Ok(me)
    }

    /// True when muvor turned it on and still owes a restore.
    pub fn owes_restore(&self) -> bool {
        self.owed.lock().expect("announce lock").is_some()
    }

    /// Put it back if muvor changed it. Idempotent, and quiet on failure:
    /// this runs on the way out, where there is nobody left to tell.
    pub fn restore(&self) {
        let mut owed = self.owed.lock().expect("announce lock");
        if let Some(previous) = owed.take() {
            let _ = self.write(previous);
        }
    }

    fn proxy(&self) -> Fallible<zbus::Proxy<'_>> {
        Ok(self.rt.block_on(zbus::Proxy::new(&self.conn, DEST, PATH, IFACE))?)
    }

    fn read(&self) -> Fallible<bool> {
        Ok(self.rt.block_on(self.proxy()?.get_property::<bool>(PROP))?)
    }

    fn write(&self, value: bool) -> Fallible<()> {
        Ok(self.rt.block_on(self.proxy()?.set_property(PROP, value))?)
    }
}

/// Restore on the way out even when the way out is a signal.
///
/// Covers the normal end of `serve` too, so the flag is put back whether the
/// daemon is stopped, killed, or simply returns. `SIGKILL` cannot be caught
/// and will leave the flag set — that is a property of `SIGKILL`, and the
/// run sheet's step 0 is the backstop for it.
impl Drop for Announce {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Wait for `SIGTERM`/`SIGINT` on a thread of its own, restore, and exit.
///
/// A thread rather than a handler: the daemon's accept loop is blocking, so
/// there is nothing to poll a signal from, and this keeps the whole
/// arrangement inside safe code.
pub fn on_signal(announce: std::sync::Arc<Announce>) {
    std::thread::Builder::new()
        .name("muvor-signals".into())
        .spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else {
                return;
            };
            rt.block_on(async {
                use tokio::signal::unix::{signal, SignalKind};
                let (Ok(mut term), Ok(mut int)) =
                    (signal(SignalKind::terminate()), signal(SignalKind::interrupt()))
                else {
                    return;
                };
                // `futures_lite::race` rather than `tokio::select!`, which
                // would mean pulling in tokio's macros feature for one line.
                futures_lite::future::race(
                    Box::pin(async {
                        term.recv().await;
                    }),
                    Box::pin(async {
                        int.recv().await;
                    }),
                )
                .await;
            });
            if announce.owes_restore() {
                println!("muvor daemon: restoring org.a11y.Status.IsEnabled to false");
            }
            announce.restore();
            std::process::exit(0);
        })
        .ok();
}

/// Read the flag without claiming it.
///
/// Separate from [`Announce::claim`] on purpose: `muvor check` reports what
/// the desktop is doing and must not change it. A command that silently
/// turned a session-wide switch on while "checking" would be the sort of
/// thing this file exists to avoid doing to other people's settings.
pub fn is_enabled() -> Fallible<bool> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let conn = rt.block_on(zbus::Connection::session())?;
    let proxy = rt.block_on(zbus::Proxy::new(&conn, DEST, PATH, IFACE))?;
    Ok(rt.block_on(proxy.get_property::<bool>(PROP))?)
}
