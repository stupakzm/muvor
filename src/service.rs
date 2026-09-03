//! The daemon, and the client that prefers it (§2.5, M5s-a).
//!
//! **M5s-a** established the pipe: a command can run here and print there,
//! because `hint` writes to a sink rather than to stdout.
//!
//! **M5s-b** puts state behind it. The daemon holds one [`Session`] —
//! the compositor's D-Bus proxy, the accessibility connection, the uictl
//! socket and §3.5's calibration — and every request after the first is
//! served from it. The visible consequence is the one worth having: the
//! calibration is measured once, so **the pointer stops moving before every
//! overlay**.
//!
//! The session is opened **at startup**, because the accessibility
//! subscription lives inside it and a subscription that starts on the first
//! hotkey press has already missed every activation worth warming for. If it
//! cannot be opened — uictld not up, extension still loading — the daemon
//! starts anyway and retries on each request, because a daemon that refuses
//! to start is a worse thing to debug than a slow hint.
//!
//! One client at a time, on purpose. A hint holds a modal keyboard grab —
//! two at once is not a thing to serialise, it is a thing that must not
//! happen, and an accept loop that finishes one connection before taking the
//! next says so in the shape of the code.

use std::io::Write;
use std::os::unix::net::{UnixListener, UnixStream};

use crate::ipc::{self, Request, Response};

type Fallible = Result<(), Box<dyn std::error::Error>>;

/// Run the daemon until it is killed.
pub fn serve() -> Fallible {
    let path = ipc::socket_path();

    // A socket file left by a daemon that died is indistinguishable from one
    // held by a daemon that is alive — until you knock on it. So knock: if
    // something answers, this is a second daemon and it should not start; if
    // nothing does, the file is litter.
    if path.exists() {
        if ping().is_some() {
            return Err(format!("a muvor daemon is already listening on {}", path.display()).into());
        }
        std::fs::remove_file(&path)?;
    }

    let listener = UnixListener::bind(&path)?;
    println!("muvor daemon listening on {}", path.display());

    // §4.7a layer 0, and it comes before the session on purpose: with the
    // announce flag false, applications emit no AT-SPI events at all, so a
    // session opened first would subscribe to a bus that has gone quiet and
    // warm nothing. Claimed here, restored on the way out (M5s-d).
    let announce = std::sync::Arc::new(match crate::announce::Announce::claim() {
        Ok(a) => {
            if a.owes_restore() {
                println!("a11y announce  was off — turned on, and will be put back on exit");
            } else {
                println!("a11y announce  already on — left alone, nothing owed");
            }
            a
        }
        Err(e) => {
            // Not fatal, for the same reason the session is not: a desktop
            // where another AT already set this works perfectly.
            println!("a11y announce  could not be read or set ({e})");
            println!("  if applications emit no events, this is the first thing to check (§4.7a)");
            return serve_on(listener, None);
        }
    });
    crate::announce::on_signal(std::sync::Arc::clone(&announce));

    serve_on(listener, Some(announce))
}

/// The other half of "system-wide": turn the hotkey into a hint.
///
/// Until extension v5 the hotkey called `Demo()` and drew M4's hardcoded
/// rectangles, so the real loop could only ever be entered by running
/// `muvor hint` in a terminal. The extension now emits a `Hotkey` signal and
/// decides nothing (§5.5); this is what listens.
///
/// **It connects to the daemon's own socket rather than touching the
/// session directly.** That looks indirect and is the point: the accept loop
/// already serialises one request at a time against one session, and a
/// second path into that session would need a lock and would be a second
/// path to test. Going in through the front door means the hotkey runs
/// *exactly* the code a terminal `muvor hint` runs — the same warm session,
/// the same tier logic, the same validation — and the cost is a unix socket
/// round trip measured in microseconds against a hint measured in
/// milliseconds.
///
/// A thread rather than a select loop because `wait_hotkey` blocks on the
/// bus and the accept loop blocks on the socket, and neither has any state
/// the other wants.
fn spawn_hotkey_listener() {
    std::thread::spawn(|| {
        // Its own connection: signals are broadcast, so this subscription
        // and the one `hint` uses for `Typed` coexist without either
        // stealing from the other.
        let shell = match crate::shell::Shell::connect() {
            Ok(s) => s,
            Err(e) => {
                eprintln!("muvor daemon: no hotkey listener ({e})");
                eprintln!("  the hotkey will draw nothing until the extension is reachable");
                return;
            }
        };
        loop {
            match shell.wait_hotkey() {
                Ok(()) => {
                    // Output goes nowhere: nobody is reading a terminal when
                    // the hotkey is what started this. Failures are logged
                    // because the journal is the only place they could show.
                    let mut sink = std::io::sink();
                    let request = Request::Hint {
                        after: 0,
                        dry_run: false,
                        explain: false,
                        measure: false,
                        deep: false,
                    };
                    if let Some(Err(e)) = crate::service::run_remote(&request, &mut sink) {
                        eprintln!("muvor daemon: hotkey hint failed: {e}");
                    }
                }
                Err(e) => {
                    // The bus went away, which is the session ending. Say so
                    // once and stop, rather than spinning on a dead socket.
                    eprintln!("muvor daemon: hotkey listener stopped: {e}");
                    return;
                }
            }
        }
    });
}

/// The accept loop, once the announce flag has been dealt with.
fn serve_on(
    listener: UnixListener,
    announce: Option<std::sync::Arc<crate::announce::Announce>>,
) -> Fallible {

    // **Opened now, not on the first request** — and that ordering is the
    // whole of M5s-c, not a preference.
    //
    // The accessibility subscription lives in the session. A session opened
    // lazily is a subscription that starts when somebody presses the hotkey,
    // by which point every `window:activate` that mattered has already
    // happened and the mirror is empty. Measured 2026-08-19: Nautilus was
    // opened, activated, and hinted, and detection still paid the 71.7 ms
    // walk, because the daemon had not been listening when the window came
    // up. The daemon exists to be already there.
    //
    // Failure is still not fatal — that part of the lazy design was right.
    // uictld may not be up yet, or the extension may be loading; the slot
    // stays empty and the next request tries again.
    let mut session: Option<crate::Session> = match crate::Session::open() {
        Ok(s) => {
            println!("session open — listening for window:activate, warming as windows come up");
            Some(s)
        }
        Err(e) => {
            println!("session not open yet ({e}) — retrying on the first request");
            println!("  until it opens, nothing is being warmed at activation");
            None
        }
    };

    spawn_hotkey_listener();

    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                if let Err(e) = handle(s, &mut session) {
                    // A client that hangs up mid-hint is ordinary, not an
                    // event: it is what Ctrl-C looks like from this side —
                    // and what `muvor dump | head` looks like too, which is
                    // the common one. This comment said so while the line
                    // below still printed it, and errscan duly recorded
                    // `Broken pipe (os error 32)` as a fault to be fixed
                    // before new work could start (2026-08-25). A watchdog
                    // that reports normal operation trains you to skim it.
                    if !is_hangup(&e) {
                        eprintln!("muvor daemon: {e}");
                    }
                }
            }
            Err(e) => eprintln!("muvor daemon: accept: {e}"),
        }
    }
    // Reached when the listener stops yielding, which is not how the daemon
    // usually ends — `on_signal` gets there first. Restoring in both places
    // is the point: whichever arrives, the flag goes back.
    if let Some(a) = &announce {
        a.restore();
    }
    Ok(())
}

/// Is this the client going away rather than something going wrong?
///
/// Three kinds, and all three are a reader that stopped reading: a pipe
/// closed on us (`head`), a peer that reset (Ctrl-C), and a connection that
/// ended between frames. `ipc::Error::Io` is the only variant that can carry
/// one — a protocol error is still worth printing.
fn is_hangup(e: &ipc::Error) -> bool {
    use std::io::ErrorKind::{BrokenPipe, ConnectionReset, UnexpectedEof};
    matches!(e, ipc::Error::Io(io) if matches!(
        io.kind(), BrokenPipe | ConnectionReset | UnexpectedEof))
}

fn handle(stream: UnixStream, session: &mut Option<crate::Session>) -> Result<(), ipc::Error> {
    let mut reader = stream.try_clone().map_err(ipc::Error::Io)?;
    let request = match ipc::read_request(&mut reader) {
        Ok(r) => r,
        // Connected and said nothing: that is `is_running`'s probe.
        Err(ipc::Error::Closed) => return Ok(()),
        Err(e) => return Err(e),
    };

    let mut out = stream;
    match request {
        Request::Ping => {
            ipc::send_response(&mut out, &Response::Pong { version: crate::VERSION.to_owned() })?;
            ipc::send_response(&mut out, &Response::Done(None))
        }
        Request::Status => {
            let mut lines = ipc::Lines::new(out);
            let outcome = match open_session(session) {
                Ok(s) => crate::status(s, &mut lines),
                Err(e) => Err(e),
            };
            lines.finish().map_err(ipc::Error::Io)?;
            let mut out = lines.into_inner();
            ipc::send_response(&mut out, &Response::Done(outcome.err().map(|e| e.to_string())))
        }
        Request::Hint { after, dry_run, explain, measure, deep } => {
            // Rebuilt rather than passed through, so the daemon runs the
            // command the client asked for and not a string it was handed.
            let mut args: Vec<String> = Vec::new();
            if after > 0 {
                args.push("--after".to_owned());
                args.push(after.to_string());
            }
            if dry_run {
                args.push("--dry-run".to_owned());
            }
            if explain {
                args.push("--explain".to_owned());
            }
            if measure {
                args.push("--measure".to_owned());
            }
            if deep {
                args.push("--deep".to_owned());
            }
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();

            let mut lines = ipc::Lines::new(out);
            let outcome = match open_session(session) {
                Ok(s) => crate::hint(&refs, s, &mut lines),
                // A session that will not open is reported as the command's
                // own failure, so the client sees what the one-shot would
                // have printed — and is *not* cached, so the next request
                // tries again once uictld or the shell is back.
                Err(e) => Err(e),
            };
            // A dependency that died takes the session with it (M5s-d). The
            // held connections are to uictld, gnome-shell and the a11y bus,
            // and any of the three can go away under a daemon that outlives
            // them: uictld is restarted, the extension is reloaded, the a11y
            // bus stops. Keeping a session whose connection is dead means
            // every later hint fails the same way with no route back; a
            // daemon that drops it reconnects on the next keypress and the
            // user never learns there was a problem.
            if let Err(e) = &outcome {
                if let Some(dep) = failed_dependency(&e.to_string()) {
                    let _ = writeln!(
                        lines,
                        "{dep} — dropping the held session; the next hint reconnects"
                    );
                    *session = None;
                }
            }
            lines.finish().map_err(ipc::Error::Io)?;
            let mut out = lines.into_inner();
            let done = Response::Done(outcome.err().map(|e| e.to_string()));
            ipc::send_response(&mut out, &done)
        }
    }
}

/// The session, opened on first need.
///
/// Held across requests — that is the entire point of M5s-b — but never
/// held *broken*: a failure to open leaves the slot empty so the next
/// request retries rather than inheriting a dead connection.
fn open_session(slot: &mut Option<crate::Session>) -> Result<&mut crate::Session, Box<dyn std::error::Error>> {
    if slot.is_none() {
        *slot = Some(crate::Session::open()?);
    }
    Ok(slot.as_mut().expect("just opened"))
}

/// Which dependency an error blames, when it blames one.
///
/// String matching, and that is worth defending rather than hiding: the
/// three failures arrive as three unrelated error types from three crates —
/// a `zbus` error, a `muvor_atspi::Error`, a `std::io::Error` from a Unix
/// socket — and the alternative is threading a taxonomy through every layer
/// to answer one question asked in one place. If a message changes, the
/// daemon reconnects one hint later than it might have; nothing breaks.
///
/// M5s-d's criterion is that a dead dependency is *named*, not that it is
/// classified.
fn failed_dependency(message: &str) -> Option<&'static str> {
    let m = message.to_ascii_lowercase();
    if m.contains("uictl") || m.contains("uinput") {
        Some("uictld is not answering")
    } else if m.contains("org.muvor.shell") || m.contains("serviceunknown") || m.contains("name is not activatable") {
        Some("the muvor extension is not there — was gnome-shell restarted, or the extension disabled?")
    } else if m.contains("at-spi") || m.contains("a11y") {
        Some("the accessibility bus is not answering")
    } else {
        None
    }
}

/// Ask the daemon what it is, or `None` if there is nobody there.
pub fn ping() -> Option<String> {
    let mut s = UnixStream::connect(ipc::socket_path()).ok()?;
    ipc::send_request(&mut s, &Request::Ping).ok()?;
    match ipc::read_response(&mut s.try_clone().ok()?) {
        Ok(Response::Pong { version }) => Some(version),
        _ => None,
    }
}

/// Run a request on the daemon, printing what it prints.
///
/// `None` means there is no daemon — **not** that the command failed. The
/// caller runs it in-process instead, which is what keeps `muvor hint`
/// working on a machine where nothing has been set up, and what keeps every
/// §5.2 measurement comparable across this change.
pub fn run_remote(request: &Request, out: &mut dyn Write) -> Option<Fallible> {
    let mut stream = UnixStream::connect(ipc::socket_path()).ok()?;
    if ipc::send_request(&mut stream, request).is_err() {
        return None;
    }

    let mut reader = match stream.try_clone() {
        Ok(r) => r,
        Err(_) => return None,
    };
    loop {
        match ipc::read_response(&mut reader) {
            Ok(Response::Line(l)) => {
                if writeln!(out, "{l}").is_err() {
                    return Some(Ok(()));
                }
            }
            Ok(Response::Done(None)) => return Some(Ok(())),
            Ok(Response::Done(Some(e))) => return Some(Err(e.into())),
            Ok(Response::Pong { .. }) => {}
            // The daemon died mid-command. Say so rather than reporting the
            // half-finished command as a success: an overlay may still be up,
            // and the extension's deadman is what ends it (§5.1).
            Err(ipc::Error::Closed) => {
                return Some(Err("the muvor daemon closed the connection mid-command".into()))
            }
            Err(e) => return Some(Err(Box::new(e))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three dependencies a daemon outlives, in the words they actually
    /// fail with. Taken from real messages, not invented for the test.
    #[test]
    fn each_dependency_is_named_from_its_own_error() {
        assert_eq!(
            failed_dependency("uictl socket: connection refused"),
            Some("uictld is not answering")
        );
        assert!(failed_dependency("org.muvor.Shell: ServiceUnknown").is_some());
        assert_eq!(
            failed_dependency("at-spi: 'targets' did not answer in 5s"),
            Some("the accessibility bus is not answering")
        );
    }

    /// An error that blames nothing in particular must not cost the session.
    /// Dropping connections on every failure would turn "no targets in this
    /// window" into a reconnect, and M5s-b exists to stop reconnecting.
    #[test]
    fn an_ordinary_failure_keeps_the_session() {
        assert_eq!(failed_dependency("no targets in this window"), None);
        assert_eq!(failed_dependency("the overlay reported 'zz'"), None);
    }
}
