//! muvor — keyboard-driven pointer control.
//!
//! At M1 this is the uictl half only: enough to prove the wire layer against
//! a live daemon and to report why the setup is not ready when it is not.

mod announce;
mod frame;
mod geom;
mod ipc;
mod service;
mod shell;

use muvor_atspi::{Claim, Provenance};
use muvor_core::Rect;
use muvor_core::{place, Alphabet, Labels, Placement, Point, Progress, BADGE_W};
use muvor_uictl::{Button, Client, Error};

const USAGE: &str = "\
muvor 0.1.0

  muvor hint                  the loop: focused window -> labels -> keystroke
                              -> validate -> click. Two keystrokes, one click.
  muvor hint --after <secs>   wait first, so the window you focus next is the
                              one hinted — not the terminal you typed this in
  muvor hint --dry-run        do everything except inject
  muvor hint --explain        print what validate-at-action walked through
  muvor hint --measure        print the timings and take the overlay straight
                              back down — no keystroke, no ten-second grab.
                              Implies --dry-run. For sampling the hint path.
  muvor calibrate             measure the pixel -> device mapping (§3.5)

  muvor daemon                run the service: one process, kept state (§2.5)
                              `muvor hint` uses it when it is up, and runs
                              in-process when it is not

  muvor check                 report whether the setup is ready, and what to fix
  muvor poke <x> <y>          move the pointer to a pixel (see --screen)
  muvor poke --device <x> <y> move the pointer to raw device units, 0..32767
  muvor click <x> <y>         move and left-click, as one BATCH (one token)
  muvor click --slow <x> <y>  ... as three frames 40 ms apart instead: tells a
                              target that ignores the click apart from one
                              that ignores three events sharing a timestamp
  muvor click --times 2 <x> <y>
                              a real double click: two batched clicks 80 ms
                              apart, which is what a hand does. --times 3 is
                              a triple click
  muvor click --here          click where the pointer already is, calibrated.
                              With --times, this is how a double click is
                              confirmed against a real window (§12.8)
  muvor shell windows         the stacking order — every window on the active
                              workspace, bottom first, and how much of each one
                              nothing covers (§4.3, extension v8)
  muvor shell window          ask the extension where the focused window is
  muvor shell demo            draw hints on a 3x3 grid inside the focused
                              window — Rust computes them from the origin
  muvor shell demo --shell    let the extension draw its own grid instead
  muvor shell hide            take the overlay down
  muvor shell wait [secs]     listen for what the overlay reports (default 60)

  muvor capture               capture the whole screen and report what came
                              back — pixels stay in memory unless --out
  muvor capture --out <path>  ... and write the PNG there, for building and
                              tuning the region detector offline (§5.4b)
  muvor capture --repeat <n>  capture n times and print the spread

  muvor dump                  list what detection sees in the focused window
  muvor dump --window <name>  ... in the first window matching <name> instead
  muvor dump --pid <pid>      ... in the window the compositor would name:
                              match by pid, then by --window as the title
  muvor dump --via <path>     force mirror | collection | cache | walk (probe)
  muvor dump --warm           build the tree mirror first, then read from it
  muvor dump --tree           print the mirror's actionable nodes and parents
  muvor dump --type <keys>    resolve keystrokes against the labels, and run
                              validate-at-action (§4.5) on whatever they hit
  muvor dump --hold <secs>    wait between warming and reading — shows whether
                              the mirror is actually being kept current
  muvor dump --rejects        also print every node dropped, and why
  muvor dump --no-meta        skip role/name lookups — what the hotkey path costs

  muvor sample --out <dir>    §5.4's dataset generator: one captured frame plus
                              the AT-SPI bounds for that same frame, in screen
                              coordinates, as a labelled sample. Detection runs
                              either side of the capture and the sample records
                              whether they agreed. A window with no tree is kept
                              rather than refused — that is D16's case
  muvor sample --note <text>  ... and record why this sample was taken

  muvor detect                what to click where the tree says nothing: the
                              largest rectangle AT-SPI cannot explain, and the
                              discrete items inside it (§5.4e, D11)
  muvor detect --out <path>   ... and draw the answer on the frame, which is
                              the only way to check a detector without
                              believing it
  muvor detect --click <n>    click the nth item, through §4.5b's validation:
                              re-capture, re-detect, and refuse unless the
                              item is still there and still looks the same
  muvor detect --dry-run      ... everything but the injection
  muvor detect --after <secs> wait first, to focus another window

  muvor free                  D12's fallback: drive the pointer by hand with
                              hjkl, accelerating while you hold a direction,
                              space to click, q to leave. Reads stdin a line
                              at a time until the extension can forward keys
  muvor free --keys <seq>     ... replay a sequence instead, for scripting
  muvor free --dry-run        ... and inject nothing

  muvor motion                M7's movement mode (§12): the pointer under the
                              keyboard. hjkl moves, s is fast, f is the left
                              button (tap = click, hold = drag), a is the
                              wheel (tap = wheel click, hold + hjkl =
                              scroll), d/g double and triple, r right,
                              Tab or right Alt leaves
  muvor motion --keys <seq>   replay a sequence of events. `l+` presses, `l-`
                              releases, a bare `l` taps, `.` is one 90 Hz
                              tick and `.90` is ninety of them — so
                              `f+ l+ .90 l- f-` is a one-second drag right
  muvor motion --at <x>,<y>   start there instead of where the pointer is
  muvor motion --dry-run      ... and inject nothing

Options:
  --screen <W>x<H>            pixel space to convert against (default 1920x1080)

Note: --screen is the *assumed* mapping, and `poke` and `click` still use it
because they are diagnostics that must work before anything else does.
`muvor hint` does not: it calibrates against the compositor (§3.5, and
`muvor calibrate` shows the working), because what mutter maps 0..32767 onto
is policy rather than contract, and an assumption there aims the pointer at
the wrong pixel on any layout but this one.
";

/// Where a command's output goes.
///
/// Stdout when a person ran it; the client's socket when the daemon is
/// running it on their behalf (§2.5). The point of threading this through
/// rather than capturing stdout is that **the daemon runs the same code**,
/// so "the daemon prints what the one-shot prints" is a property of the
/// transport and not a pair of code paths kept in step by hand.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

type Out<'a> = &'a mut dyn std::io::Write;

/// `println!`, but to wherever this command's output is going.
///
/// A write that fails means the client hung up mid-command; that ends the
/// command, which is what a person pressing Ctrl-C expects.
macro_rules! say {
    ($out:expr) => {
        writeln!($out).map_err(|e| -> Box<dyn std::error::Error> { Box::new(e) })?
    };
    ($out:expr, $($arg:tt)*) => {
        writeln!($out, $($arg)*).map_err(|e| -> Box<dyn std::error::Error> { Box::new(e) })?
    };
}

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    match run(&refs) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("muvor: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run(args: &[&str]) -> Result<(), Box<dyn std::error::Error>> {
    let mut screen = (1920i64, 1080i64);
    let mut rest: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--screen" {
            let v = args.get(i + 1).ok_or("--screen needs <W>x<H>")?;
            let (w, h) = v.split_once('x').ok_or("--screen needs <W>x<H>")?;
            screen = (w.parse()?, h.parse()?);
            i += 2;
        } else {
            rest.push(args[i]);
            i += 1;
        }
    }

    match rest.first().copied() {
        Some("check") => check(),
        Some("hint") => {
            // Prefer the daemon; fall back to running it here. §2.5: the
            // fallback is not politeness, it is what keeps the numbers
            // comparable and the CLI testable without a session.
            let request = parse_hint(&rest[1..])?;
            let mut stdout = std::io::stdout();
            match service::run_remote(&request, &mut stdout) {
                Some(result) => result,
                None => {
                    let mut session = Session::open()?;
                    hint(&rest[1..], &mut session, &mut stdout)
                }
            }
        }
        Some("daemon") => service::serve(),
        Some("status") => {
            let mut stdout = std::io::stdout();
            match service::run_remote(&ipc::Request::Status, &mut stdout) {
                Some(result) => result,
                None => Err("no muvor daemon is running — start one with `muvor daemon`".into()),
            }
        }
        Some("calibrate") => calibrate_cmd(),
        Some("poke") => poke(&rest[1..], screen),
        Some("click") => click(&rest[1..], screen),
        Some("dump") => dump(&rest[1..]),
        Some("shell") => shell_cmd(&rest[1..]),
        Some("capture") => capture_cmd(&rest[1..]),
        Some("sample") => sample_cmd(&rest[1..]),
        Some("detect") => detect_cmd(&rest[1..]),
        Some("free") => free_cmd(&rest[1..]),
        Some("motion") => motion_cmd(&rest[1..]),
        _ => {
            print!("{USAGE}");
            Ok(())
        }
    }
}

/// Startup checks. Every failure names its fix — the failure modes here all
/// present as "muvor is broken" and none of them are bugs.
fn check() -> Result<(), Box<dyn std::error::Error>> {
    // The daemon first, and unconditionally. It is optional by design
    // (§2.5) — its absence is a slower hint, never a broken one — but the
    // point of `check` is to name *everything* that is wrong, and a report
    // that stops at the first failure hides the rest of the answer.
    let muvor_sock = ipc::socket_path();
    println!("muvor daemon   {}", muvor_sock.display());
    match service::ping() {
        Some(v) => println!("  running      v{v} — `muvor hint` runs there"),
        None => println!("  not running  hint runs in-process, paying startup every time"),
    }

    let sock = muvor_uictl::default_socket_path();
    println!("uictl socket   {}", sock.display());
    if !sock.exists() {
        println!("  MISSING      start uictld; nothing below can be checked");
        return Err("uictl daemon is not running".into());
    }

    let mut c = Client::connect()?;
    let h = *c.hello();
    println!("  connected     proto v{}", h.proto_selected);
    println!("  abs_range_max {}  (device units; muvor converts, §5A.1)", h.abs_range_max);
    println!("  device_caps   0x{:04x}", h.device_caps);
    println!("  opcodes       MOVE_ABS={} BUTTON={} BATCH={} SCROLL={}",
        h.supports(muvor_uictl::Op::MoveAbs),
        h.supports(muvor_uictl::Op::Button),
        h.supports(muvor_uictl::Op::Batch),
        h.supports(muvor_uictl::Op::Scroll));

    // The rate class is not reported by HELLO, so infer it the only way a
    // client can: spend more than the untrusted budget and see if it holds.
    // 5/s untrusted vs 50/s interactive presents as "works for one second,
    // then dies" — worth catching here rather than mid-gesture.
    //
    // NOT with PING. §3.7 makes PING and HELLO free of rate-limit charge, so
    // that a throttled client can still ask why it is throttled — which makes
    // a PING-based probe a test that cannot fail. Use MOVE_REL in cancelling
    // pairs instead: each one is charged, and the pointer ends where it began.
    let mut sent = 0;
    let t0 = std::time::Instant::now();
    let verdict = loop {
        if sent >= 12 { break Ok(()); }
        let dx = if sent % 2 == 0 { 1 } else { -1 };
        match c.move_rel(dx, 0) {
            Ok(()) => sent += 1,
            Err(e) => break Err(e),
        }
        if t0.elapsed() > std::time::Duration::from_millis(900) { break Ok(()); }
    };
    if sent % 2 != 0 {
        c.move_rel(-1, 0).ok(); // leave the pointer exactly where it was found
    }
    match verdict {
        Ok(()) => println!("  rate          {sent} requests in {:?}, no refusal", t0.elapsed()),
        Err(Error::Refused { result, .. }) => {
            println!("  rate          REFUSED after {sent} requests (result {})", result.0);
            println!("                muvor is likely 'untrusted' at 5/s. Add to");
            println!("                ~/.config/uictl/clients:");
            let exe = exe_path();
            if exe.contains(' ') {
                // uictld parses the registry with strtok_r(line, " \t\r") and
                // has no quoting, so a path with a space silently invalidates
                // the whole line. Printing it as a fix would be printing a
                // fix that cannot work.
                println!("                  muvor interactive");
                println!("                (omit exe=: the registry has no quoting and");
                println!("                 this binary's path contains a space:");
                println!("                 {exe})");
            } else {
                println!("                  muvor interactive exe={exe}");
            }
        }
        Err(e) => return Err(e.into()),
    }

    // `bus`, or `bus_0`, or `bus_1` — at-spi-bus-launcher names the socket
    // after the display it was started for, and which name you get depends
    // on whether `DISPLAY` was set when it ran. Checking only `bus` reported
    // MISSING on a perfectly healthy session (2026-08-25) and sent a whole
    // session looking for a fault that was not there. The detector never
    // used this path anyway — it resolves the bus through `org.a11y.Bus` —
    // so this line is a report, and a report that can be wrong is worse than
    // no report.
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_owned());
    let a11y_dir = std::path::Path::new(&dir).join("at-spi");
    let found = std::fs::read_dir(&a11y_dir).ok().and_then(|entries| {
        entries.flatten().map(|e| e.file_name()).find(|n| {
            n.to_str().is_some_and(|n| n == "bus" || n.starts_with("bus_"))
        })
    });
    match &found {
        Some(name) => {
            println!("a11y bus       {}", a11y_dir.join(name).display());
            println!("  present      (plan.md §4.1: the toolkit-accessibility flag is not the gate)");
        }
        None => {
            println!("a11y bus       {}/bus*", a11y_dir.display());
            println!("  MISSING      detection will find nothing");
        }
    }

    // §4.7a layer 0. Read only — `check` reports, it does not change session
    // state; the daemon is what claims this, and puts it back.
    println!("a11y announce  {}", match crate::announce::is_enabled() {
        Ok(true) => "on           applications will emit events".to_owned(),
        Ok(false) => {
            "OFF          applications emit NO events at all (§4.7a).\n               `muvor daemon` turns it on at startup and restores it on exit"
                .to_owned()
        }
        Err(e) => format!("unreadable   ({e})"),
    });

    // §12.3, and the line that was wrong about it until 2026-08-25.
    //
    // v7 asked "has the shell ever seen a key press and a key release?" and
    // said `available` on 450 down / 2 up — while every hold on the desk was
    // clicking. The two questions are not the same. What v7 counted was
    // events on `global.stage`'s `captured-event`, and mutter's own
    // `src/core/events.c` consumes a grabbed accelerator inside a
    // `clutter_event_add_filter()` callback, above every stage signal — so
    // the label key's press and release were the two events that could never
    // be counted, whatever the total said.
    //
    // v8 does not overhear anything: it holds the key focus, so the numbers
    // below are events the overlay itself *handled*. Zero is then a real
    // fault rather than an unobservable one, and the version is what says
    // which of the two a number means.
    let version = shell::Shell::connect().map(|sh| sh.version()).unwrap_or(0);
    let v8 = version >= 8;
    match shell::Shell::connect().and_then(|sh| sh.probe()) {
        Ok((_, _, np, nr)) if v8 => {
            println!("hold gesture   {}", if np > 0 && nr > 0 {
                format!("available    the overlay handles press and release ({np} down, {nr} up)")
            } else if np > 0 {
                format!("SUSPECT      {np} presses handled, no releases — hold a label key and")
                    + "\n               re-run. If it stays at zero the key focus is not held"
            } else {
                "untested     no key has reached the overlay yet. Press the hotkey, type a\n                                label, and run this again — v8 cannot fail silently the way v7 did"
                    .to_owned()
            });
        }
        Ok((_, _, np, nr)) => {
            println!("hold gesture   UNAVAILABLE  extension v{version} watches the stage, and mutter");
            println!("               consumes a grabbed key before any stage signal exists");
            println!("               (src/core/events.c). Every hold is a click; ' is the entry.");
            println!("               {np} down / {nr} up is what it overheard, not what it needs.");
            println!("               Extension v8 fixes it and needs a logout.");
        }
        Err(_) => println!("hold gesture   unknown      no extension answering, or older than v7"),
    }

    Ok(())
}

fn exe_path() -> String {
    std::fs::read_link("/proc/self/exe")
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "/abs/path/to/muvor".into())
}

/// Nine hints on a 3x3 grid inside a window, in **screen** coordinates.
/// Hardcoded positions, real geometry: if the overlay lands anywhere but on
/// that window, the origin arithmetic is wrong and M5 would have blamed
/// detection for it.
fn grid(w: &shell::Window) -> Vec<shell::Hint> {
    let mut hints = Vec::with_capacity(9);
    let alphabet = "asdfghjkl;";
    for row in 0..3 {
        for col in 0..3 {
            let i = row * 3 + col;
            let label: String = [
                alphabet.as_bytes()[i / 10] as char,
                alphabet.as_bytes()[i % 10] as char,
            ]
            .iter()
            .collect();
            let (x, y) = (
                w.x + w.w * (col as i32 + 1) / 4 - 40,
                w.y + w.h * (row as i32 + 1) / 4 - 16,
            );
            // The demo grid clicks nothing, so its dot marks the cell's own
            // centre — which is still the honest answer to "where would this
            // one go", and makes the dot visible in the one command that
            // needs no accessibility tree at all.
            hints.push(shell::Hint {
                x,
                y,
                w: 80,
                h: 32,
                click_x: x + 40,
                click_y: y + 16,
                label,
            });
        }
    }
    hints
}

/// The extension boundary (§5.5). At M4 the shell draws hardcoded
/// rectangles and reports what was typed; M5 replaces the rectangles with
/// detection's output and the report with a click.
fn shell_cmd(args: &[&str]) -> Result<(), Box<dyn std::error::Error>> {
    let shell = shell::Shell::connect()?;
    match args.first().copied() {
        // The stacking order, printed. §4.3 could not see it for eleven
        // months and the first thing it needed after v8 supplied it was a
        // way to look at it without pressing the hotkey — a badge that is
        // missing and a window that is covered look identical from the desk.
        Some("windows") => {
            let all = shell.windows()?;
            if all.is_empty() {
                println!("no windows on the active workspace");
                return Ok(());
            }

            println!("bottom of the stack first — what is later is on top of what is earlier");
            println!();
            for (i, w) in all.iter().enumerate() {
                // Everything later in the list is on top of this one, which
                // is what "bottom to top" means and the only reason the
                // order is worth transporting.
                let over: Vec<Rect> = all[i + 1..]
                    .iter()
                    .filter(|o| !o.minimized && o.showing && o.w > 0 && o.h > 0)
                    .map(|o| Rect::new(o.x, o.y, o.w, o.h))
                    .collect();
                let vis = muvor_core::visible_fraction(Rect::new(w.x, w.y, w.w, w.h), &over);
                println!(
                    "{:>2}. {:<24} {:>5},{:<5} {:>4}x{:<4}  pid {:<7} {}",
                    i + 1,
                    w.wm_class,
                    w.x,
                    w.y,
                    w.w,
                    w.h,
                    w.pid,
                    if w.minimized {
                        "minimized".to_owned()
                    } else if !w.showing {
                        "not showing".to_owned()
                    } else {
                        format!("{:.0}% visible, {} on top", vis * 100.0, over.len())
                    }
                );
                println!("    {}", w.title);
            }
            Ok(())
        }
        Some("window") => {
            let w = shell.focused_window()?;
            if w.is_none() {
                println!("no focused window");
            } else {
                println!("focused         {}", w.title);
                println!("frame rect      {},{} {}x{}   (screen coordinates)", w.x, w.y, w.w, w.h);
                println!();
                println!("This is the origin D13 says AT-SPI cannot know: a target's");
                println!("window-relative bounds plus {},{} is where it really is.", w.x, w.y);
            }
            Ok(())
        }
        Some("demo") => {
            if args.get(1).copied() == Some("--shell") {
                shell.demo()?;
            } else {
                // Rust-side hints exercise what the shell's own Demo() cannot:
                // the a(iiiis) marshalling, and the arithmetic that turns a
                // window origin into screen coordinates — which is the whole
                // reason M4 exists before M5 (D13).
                let w = shell.focused_window()?;
                if w.is_none() {
                    return Err("no focused window to draw in".into());
                }
                let hints = grid(&w);
                println!("window          {} at {},{} {}x{}", w.title, w.x, w.y, w.w, w.h);
                shell.show(&hints)?;
            }
            println!("overlay up — type a label, or Escape.");
            match shell.wait(std::time::Duration::from_secs(30))? {
                shell::Outcome::Typed(label) => println!("typed           {label}"),
                shell::Outcome::Cancelled => println!("cancelled"),
                shell::Outcome::TimedOut => {
                    shell.hide().ok();
                    println!("nothing typed in 30s — overlay taken down");
                }
                // `shell demo` is M4's probe and predates both; reported so
                // the probe never lies about what the extension sent.
                other => println!("unexpected outcome from the overlay: {other:?}"),
            }
            Ok(())
        }
        Some("hide") => {
            shell.hide()?;
            Ok(())
        }
        Some("wait") => {
            let secs: u64 = args.get(1).map_or(Ok(60), |s| s.parse())?;
            println!("listening {secs}s for the overlay — press the hotkey and type a label");
            match shell.wait(std::time::Duration::from_secs(secs))? {
                shell::Outcome::Typed(label) => println!("typed           {label}"),
                shell::Outcome::Cancelled => println!("cancelled"),
                shell::Outcome::TimedOut => println!("nothing arrived"),
                shell::Outcome::Deepen => println!("deepen — Tab, asking for tier 2 (D18)"),
                shell::Outcome::KeyDown(k) => println!("key down        {k}"),
                shell::Outcome::KeyRepeat(k) => println!("key repeat      {k}  — the keyboard, not a hand (v10)"),
                shell::Outcome::KeyUp(k) => println!("key up          {k}"),
                shell::Outcome::TypedHold(l) => println!("typed (held)    {l}  — movement mode, §12"),
            }
            Ok(())
        }
        _ => Err("muvor shell <windows|window|demo|hide|wait>".into()),
    }
}

/// Full-screen capture (§5.4b, D16) — the rung that needs nothing from the
/// application.
///
/// D16 supersedes D10's "never full-screen" for one measured reason: an
/// application that exposes no tree exposes no rectangle either, so there is
/// nothing to scope a capture *to*. LibreWolf is the whole of it, and
/// Nautilus's file view is the same problem inside a window that otherwise
/// works.
///
/// **Pixels stay in memory unless `--out` is given.** D10's other two halves
/// survive D16 intact: capture is in-compositor, and it is transient. Writing
/// a frame to disk is a deliberate, named act for building the detector —
/// never something the hint path does.
fn capture_cmd(args: &[&str]) -> Result<(), Box<dyn std::error::Error>> {
    let mut out: Option<&str> = None;
    let mut repeat: u32 = 1;
    let mut i = 0;
    while i < args.len() {
        match args[i] {
            "--out" => {
                out = Some(args.get(i + 1).ok_or("capture: --out needs a path")?);
                i += 2;
            }
            "--repeat" => {
                repeat = args.get(i + 1).ok_or("capture: --repeat needs a count")?.parse()?;
                i += 2;
            }
            other => return Err(format!("capture: unexpected argument {other}").into()),
        }
    }

    let shell = shell::Shell::connect()?;
    let mut times = Vec::with_capacity(repeat as usize);
    let mut last = None;
    for _ in 0..repeat {
        let t0 = std::time::Instant::now();
        let frame = shell.capture()?;
        times.push(t0.elapsed());
        last = Some(frame);
    }
    let frame = last.ok_or("capture: --repeat 0 captured nothing")?;

    println!("area            {},{} {}x{}   (screen coordinates)", frame.x, frame.y, frame.w, frame.h);
    println!("scale           {}", frame.scale);
    println!("png             {} bytes", frame.png.len());
    let raw = i64::from(frame.w) * i64::from(frame.h) * 4;
    println!("raw would be    {raw} bytes BGRA at scale 1");

    if repeat == 1 {
        println!("round trip      {:?}", times[0]);
    } else {
        let min = times.iter().min().expect("repeat >= 1");
        let max = times.iter().max().expect("repeat >= 1");
        let mean = times.iter().sum::<std::time::Duration>() / repeat;
        println!("round trip      min {min:?}  mean {mean:?}  max {max:?}   ({repeat} captures)");
    }

    match out {
        Some(path) => {
            std::fs::write(path, &frame.png)?;
            println!("wrote           {path}");
        }
        None => println!("pixels          not written — pass --out <path> to keep them"),
    }
    Ok(())
}


/// §5.4's dataset generator — a captured frame plus the AT-SPI bounds for
/// that same frame, written as one labelled sample (M6, D11).
///
/// This is the increment §5.4 calls "the part that actually transfers": the
/// detector is tuned against real precision and recall instead of against
/// somebody's eye, per-application coverage becomes a number, and if the ML
/// route is ever taken this is where the training data comes from.
///
/// **Both halves already existed** as `muvor capture --out` and `muvor dump`.
/// What this adds is the join, and the join is the part with the correctness
/// problem: a frame and a tree read at different moments describe different
/// user interfaces, and nothing downstream can tell that from a detector
/// that is wrong. So detection runs **twice, on either side of the capture**,
/// and the sample records whether the two agreed. A sample that drifted is
/// still written — it is evidence about how fast that application changes —
/// but it says so, and a training set can filter on it.
///
/// **Screen coordinates throughout** (D13). The bounds AT-SPI reports are
/// window-relative on Wayland, and a label that does not index the pixels it
/// labels is not ground truth. The window origin the compositor supplies is
/// added here, exactly as `hint` adds it before injecting.
///
/// **A window with no accessibility tree is a valid sample, not an error** —
/// it is D16's whole motivating case (LibreWolf exposes nothing, §4.7-i).
/// `hint` fails there because it has nothing to click; the dataset wants
/// that frame most of all, with zero targets and the reason recorded. This
/// is the one place muvor treats "no tree" as data.
///
/// D10 and D16 both survive: pixels reach the disk only because `--out`
/// asked for them, and this command exists to be pointed at a directory.
fn sample_cmd(args: &[&str]) -> Result<(), Box<dyn std::error::Error>> {
    let mut dir = None;
    let mut note = None;
    let mut i = 0;
    while i < args.len() {
        match args[i] {
            "--out" => {
                dir = Some(*args.get(i + 1).ok_or("sample: --out needs a directory")?);
                i += 2;
            }
            "--note" => {
                note = Some(*args.get(i + 1).ok_or("sample: --note needs some text")?);
                i += 2;
            }
            other => return Err(format!("sample: unexpected argument {other}").into()),
        }
    }
    // Required rather than defaulted: this command's entire output is pixels
    // on disk, and D10 says pixels reach the disk only when asked for. A
    // default path would be muvor deciding that for the user.
    let dir = dir.ok_or(
        "sample: --out <dir> is required — this command writes pixels to disk and \
         D10 says that happens only when asked for",
    )?;
    std::fs::create_dir_all(dir)?;

    let shell = shell::Shell::connect()?;
    let det = muvor_atspi::Detector::connect()?;

    let win = shell.focused_window()?;
    if win.is_none() {
        return Err("no focused window — the compositor reports nothing focused".into());
    }

    // The a11y side of the same window. `None` is not a failure here (see
    // the doc comment): it is D16's case, and the frame is worth keeping.
    let found = det.frame_for_window(&win.title, win.pid, (win.w, win.h))?;
    let frame = found.value;

    // Detection before, capture, detection after. The gap between the two
    // scans is the window in which the interface could have changed under
    // the camera, and comparing them is the only way to know that it did
    // not — `stable` below is what makes a sample trustworthy rather than
    // merely recent.
    let before = match &frame {
        Some(f) => Some(det.targets(f, None, true)?.value),
        None => None,
    };
    let shot = shell.capture()?;
    let after = match &frame {
        Some(f) => Some(det.targets(f, None, true)?.value),
        None => None,
    };
    let win_after = shell.focused_window()?;

    // Window-relative to screen (D13), the same arithmetic `hint` does before
    // it injects: the compositor's origin, minus the a11y frame's own, which
    // is near-zero on every toolkit measured and is subtracted rather than
    // assumed because "near" is not "is".
    let (fx, fy) = frame.as_ref().map_or((0, 0), |f| (f.bounds.x, f.bounds.y));
    // `win.origin()`, not `win.x, win.y` — §5.1h. The frame rect is the
    // window without its client-side shadow and the a11y frame is measured
    // against the *buffer*, which includes it, so on any window that draws a
    // shadow these are different corners by ~26 px.
    let (wx, wy) = win.origin();
    let to_screen = |x: i32, y: i32| (wx + x - fx, wy + y - fy);

    let same_window = !win_after.is_none()
        && win_after.pid == win.pid
        && win_after.title == win.title
        && (win_after.x, win_after.y, win_after.w, win_after.h) == (win.x, win.y, win.w, win.h);
    // Compared **by identity, not by position**. The first version of this
    // zipped the two lists, and Nautilus reported unstable on every sample
    // with the same 24 targets on both sides: the `Walk` path (GTK4 has no
    // `Collection`, §4.2-ii-e) does not promise an order, so a positional
    // comparison was measuring the traversal rather than the interface. The
    // object path is the identity §4.5 already trusts for re-validation.
    let drift = match (&before, &after) {
        (Some(a), Some(b)) => drift_between(a, b),
        (None, None) => Vec::new(),
        // One side saw a tree and the other did not, which is a real change.
        _ => vec!["the accessibility tree appeared or vanished".to_owned()],
    };
    let same_targets = drift.is_empty();
    let stable = same_window && same_targets;

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| format!("sample: the clock is before 1970: {e}"))?
        .as_millis();
    let png_name = format!("sample-{stamp}.png");
    let png_path = std::path::Path::new(dir).join(&png_name);
    let json_path = std::path::Path::new(dir).join(format!("sample-{stamp}.json"));

    let mut j = String::with_capacity(4096);
    j.push_str("{\n  \"schema\": \"muvor.sample/1\",\n");
    j.push_str(&format!("  \"captured_unix_ms\": {stamp},\n"));
    j.push_str(&format!("  \"image\": {},\n", quote(&png_name)));
    if let Some(n) = note {
        j.push_str(&format!("  \"note\": {},\n", quote(n)));
    }
    j.push_str(&format!(
        "  \"screen\": {{ \"x\": {}, \"y\": {}, \"w\": {}, \"h\": {}, \"scale\": {} }},\n",
        shot.x, shot.y, shot.w, shot.h, shot.scale
    ));
    j.push_str("  \"window\": {\n");
    j.push_str(&format!("    \"pid\": {},\n", win.pid));
    j.push_str(&format!("    \"wm_class\": {},\n", quote(&win.wm_class)));
    j.push_str(&format!("    \"title\": {},\n", quote(&win.title)));
    j.push_str(&format!(
        "    \"compositor\": {{ \"x\": {}, \"y\": {}, \"w\": {}, \"h\": {} }},\n",
        win.x, win.y, win.w, win.h
    ));
    match &frame {
        Some(f) => {
            j.push_str(&format!("    \"app\": {},\n", quote(&f.app)));
            j.push_str(&format!("    \"a11y_title\": {},\n", quote(&f.title)));
            j.push_str(&format!("    \"a11y_role\": {},\n", quote(&f.role.to_string())));
            j.push_str(&format!("    \"focus_source\": {},\n", quote(f.focus.as_str())));
            j.push_str(&format!(
                "    \"a11y_frame\": {{ \"x\": {}, \"y\": {}, \"w\": {}, \"h\": {} }},\n",
                f.bounds.x, f.bounds.y, f.bounds.w, f.bounds.h
            ));
            // §4.5's own check, recorded rather than enforced: when the two
            // sizes disagree the origin correction above is off by the
            // difference, and every target rectangle in this sample is
            // shifted by it. A consumer that does not know that would tune
            // a detector against a systematic offset.
            j.push_str(&format!(
                "    \"size_agrees\": {}\n",
                (f.bounds.w - win.w).abs() <= 8 && (f.bounds.h - win.h).abs() <= 8
            ));
        }
        // D16's case, and the reason this command does not just call `hint`.
        None => j.push_str("    \"a11y\": null\n"),
    }
    j.push_str("  },\n");

    let scan = after.as_ref().or(before.as_ref());
    match scan {
        Some(s) => {
            j.push_str(&format!(
                "  \"detection\": {{ \"provenance\": {}, \"examined\": {}, \"candidates\": {}, \
                 \"truncated\": {} }},\n",
                quote(s.provenance.as_str()),
                s.examined,
                s.candidates,
                s.truncated
            ));
            j.push_str("  \"targets\": [\n");
            for (n, t) in s.targets.iter().enumerate() {
                let (x, y) = to_screen(t.bounds.x, t.bounds.y);
                let (wcx, wcy) = t.bounds.centre();
                let (cx, cy) = to_screen(wcx, wcy);
                j.push_str(&format!(
                    "    {{ \"role\": {}, \"name\": {}, \"x\": {x}, \"y\": {y}, \"w\": {}, \
                     \"h\": {}, \"cx\": {cx}, \"cy\": {cy}, \"provenance\": {} }}{}\n",
                    quote(&t.role.to_string()),
                    quote(&t.name),
                    t.bounds.w,
                    t.bounds.h,
                    quote(t.provenance.as_str()),
                    if n + 1 == s.targets.len() { "" } else { "," }
                ));
            }
            j.push_str("  ],\n");
            // The negatives, and they are not filler: §4.4's ghosts are the
            // nodes a detector must *not* find, and a dataset of positives
            // alone cannot measure a false positive.
            j.push_str("  \"rejects\": [\n");
            for (n, r) in s.rejects.iter().enumerate() {
                j.push_str(&format!(
                    "    {{ \"role\": {}, \"name\": {}, \"why\": {} }}{}\n",
                    quote(&r.role.to_string()),
                    quote(&r.name),
                    quote(r.why.as_str()),
                    if n + 1 == s.rejects.len() { "" } else { "," }
                ));
            }
            j.push_str("  ],\n");
        }
        None => {
            j.push_str("  \"detection\": null,\n  \"targets\": [],\n  \"rejects\": [],\n");
        }
    }
    j.push_str(&format!(
        "  \"stable\": {stable},\n  \"same_window\": {same_window},\n  \"same_targets\": {same_targets},\n"
    ));
    // *What* drifted, not just that something did. A consumer filtering on
    // `stable` needs the boolean; anybody asking why this application will
    // not hold still needs the list, and it is the same list either way.
    j.push_str("  \"drift\": [\n");
    for (n, d) in drift.iter().enumerate() {
        j.push_str(&format!("    {}{}\n", quote(d), if n + 1 == drift.len() { "" } else { "," }));
    }
    j.push_str("  ]\n}\n");

    // The PNG last: the JSON names it, so a reader that finds the image finds
    // a description of it too, whichever order a crash interrupts.
    std::fs::write(&json_path, j)?;
    std::fs::write(&png_path, &shot.png)?;

    println!("window          {} — {}  [{}]", frame.as_ref().map_or("(no a11y tree)", |f| f.app.as_str()), win.title, win.wm_class);
    println!("frame           {},{} {}x{}   scale {}", shot.x, shot.y, shot.w, shot.h, shot.scale);
    match scan {
        Some(s) => println!(
            "targets         {} labelled, {} rejected   (via {})",
            s.targets.len(),
            s.rejects.len(),
            s.provenance.as_str()
        ),
        None => {
            println!("targets         none — this window exposes no accessibility tree at all");
            println!("                That is D16's case and it is why this sample is worth");
            println!("                keeping: it is a frame the tree cannot explain.");
        }
    }
    if stable {
        println!("stable          yes — detection agreed either side of the capture");
    } else {
        println!("stable          NO — the interface changed under the camera");
        if !same_window {
            println!("                the focused window is not the one that was captured");
        }
        for d in &drift {
            println!("                {d}");
        }
        println!("                Kept, and marked: filter on \"stable\" before training.");
    }
    println!("wrote           {}", png_path.display());
    println!("                {}", json_path.display());
    Ok(())
}

/// What changed between two scans of the same window, keyed by the object
/// path — the identity §4.5 re-validates against, and the only handle that
/// survives a traversal reordering.
///
/// Returns an empty list when the two scans describe the same interface. The
/// strings are for a human reading a sample that refused to hold still, so
/// they name the target rather than dumping both structures.
fn drift_between(a: &muvor_atspi::Scan, b: &muvor_atspi::Scan) -> Vec<String> {
    use std::collections::BTreeMap;
    let index = |s: &muvor_atspi::Scan| -> BTreeMap<String, (String, String, muvor_atspi::WindowRect)> {
        s.targets
            .iter()
            .map(|t| (t.path.clone(), (t.role.to_string(), t.name.clone(), t.bounds)))
            .collect()
    };
    let (x, y) = (index(a), index(b));
    let mut out = Vec::new();
    for (path, (role, name, bounds)) in &x {
        match y.get(path) {
            None => out.push(format!("gone after the capture: {role} '{name}'")),
            Some((r2, n2, b2)) => {
                if r2 != role || n2 != name {
                    out.push(format!("became {r2} '{n2}': was {role} '{name}'"));
                } else if b2 != bounds {
                    out.push(format!("moved: {role} '{name}' {bounds} -> {b2}"));
                }
            }
        }
    }
    for (path, (role, name, _)) in &y {
        if !x.contains_key(path) {
            out.push(format!("appeared during the capture: {role} '{name}'"));
        }
    }
    out
}

/// M6's second half: what to click where the accessibility tree says nothing
/// (§5.4, §5.4e, D11, D16).
///
/// The whole pipeline in one command, and deliberately separate from `hint`
/// until it has earned a place there: capture the screen, ask AT-SPI what it
/// can explain about the focused window, take the largest rectangle it
/// cannot, and look for discrete items inside it.
///
/// **What it prints is the shape of the answer, not just the answer.**
/// `grid` and `single` are the two cases §5.4e measured, and which one a
/// window produces is the interesting fact about it — Nautilus's file view
/// is a grid of twenty, gnome-terminal's text area is one region. A user
/// reading this is being told whether muvor found *things* or found *a
/// place*.
///
/// Not wired into `hint` yet. Capture costs ~83 ms (D17) against a 15 ms
/// target, so putting this on the hotkey path would break §5.2's budget for
/// every window, including the ones AT-SPI already explains perfectly.
/// What that costs is a decision, and it is not this increment's.
fn detect_cmd(args: &[&str]) -> Result<(), Box<dyn std::error::Error>> {
    let mut out = None;
    let mut click = None;
    let mut dry_run = false;
    let mut after = 0u64;
    let mut i = 0;
    while i < args.len() {
        match args[i] {
            "--out" => {
                out = Some(*args.get(i + 1).ok_or("detect: --out needs a path")?);
                i += 2;
            }
            "--click" => {
                click = Some(args.get(i + 1).ok_or("detect: --click needs an item number")?
                    .parse::<usize>()?);
                i += 2;
            }
            "--dry-run" => {
                dry_run = true;
                i += 1;
            }
            "--after" => {
                after = args.get(i + 1).ok_or("detect: --after needs seconds")?.parse()?;
                i += 2;
            }
            other => return Err(format!("detect: unexpected argument {other}").into()),
        }
    }
    if after > 0 {
        println!("waiting {after}s — focus the window you want detected");
        std::thread::sleep(std::time::Duration::from_secs(after));
    }

    let shell = shell::Shell::connect()?;
    let det = muvor_atspi::Detector::connect()?;
    let win = shell.focused_window()?;
    if win.is_none() {
        return Err("no focused window — the compositor reports nothing focused".into());
    }

    // The tree first and the pixels second, because the tree is what decides
    // *where* to look and it costs a thousandth of what the capture does.
    let found = det.frame_for_window(&win.title, win.pid, (win.w, win.h))?;
    let (mut occupied, provenance) = match &found.value {
        Some(f) => {
            let scan = det.targets(f, None, true)?.value;
            let (wx, wy) = win.origin();   // §5.1h, not win.x/win.y
            let to_screen = |x: i32, y: i32| (wx + x - f.bounds.x, wy + y - f.bounds.y);
            let rects = scan
                .targets
                .iter()
                .map(|t| {
                    let (x, y) = to_screen(t.bounds.x, t.bounds.y);
                    Rect::new(x, y, t.bounds.w, t.bounds.h)
                })
                .collect::<Vec<_>>();
            (rects, scan.provenance.as_str())
        }
        // D16: no tree means nothing is explained, so the whole window is
        // the region to look in. This is the case the detector exists for.
        None => (Vec::new(), "none"),
    };

    let t_cap = std::time::Instant::now();
    let shot = shell.capture()?;
    let t_cap = t_cap.elapsed();
    let t_dec = std::time::Instant::now();
    let luma = frame::decode(&shot.png, (shot.x, shot.y))?;
    let t_dec = t_dec.elapsed();

    // Scoped to the window rather than the screen: everything outside it
    // belongs to some other application, and offering targets there would
    // be muvor answering a question nobody asked. The window is clipped to
    // the captured area so a window hanging off the edge cannot index
    // outside the image.
    let area = clip(Rect::new(win.x, win.y, win.w, win.h), luma.bounds());
    // Anything the tree explained is not for the detector to find again.
    occupied.retain(|r| overlaps(*r, area));

    let t_scope = std::time::Instant::now();
    let block = muvor_core::opaque_block(area, &occupied);
    let t_scope = t_scope.elapsed();

    println!("window          {} — {}  [{}]", found.value.as_ref().map_or("(no a11y tree)", |f| f.app.as_str()), win.title, win.wm_class);
    println!("  frame         {},{} {}x{}", area.x, area.y, area.w, area.h);
    println!("  explained     {} targets via {provenance}", occupied.len());

    let Some(block) = block else {
        println!("  opaque        none — the tree explains every pixel of this window");
        println!();
        println!("Nothing for the detector to do here, which is the good case:");
        println!("§4.4's filter and §4.5's validation already cover this window.");
        return Ok(());
    };
    let cover = 100.0 * block.area() as f64 / area.area().max(1) as f64;
    println!("  opaque block  {},{} {}x{}   {cover:.0}% of the window", block.x, block.y, block.w, block.h);

    // The detector's own parameters are pixel distances, and the capture can
    // be a different size from the rectangle it covers on a fractional
    // scale (§5.4b). Scale them rather than silently measuring gaps in the
    // wrong unit.
    let params = muvor_core::Params::for_scale(shot.scale);
    let t_det = std::time::Instant::now();
    let (items, kind) = muvor_core::targets_in(&luma.pixels, luma.w, block, luma.origin, params);
    let t_det = t_det.elapsed();

    println!();
    match kind {
        muvor_core::Found::Grid(n) => {
            println!("found           {n} discrete items — a grid");
            println!("                D11 would have offered one target here, and one target");
            println!("                is what this window is not. §5.4e has the measurement.");
        }
        muvor_core::Found::Single => {
            println!("found           no discrete structure — one centre target (D11)");
            println!("                This is the case D11 was written for and is right about.");
        }
    }
    for (n, r) in items.iter().enumerate().take(24) {
        let (cx, cy) = r.centre();
        println!("  {:>3}  {},{} {}x{}   click {cx},{cy}", n + 1, r.x, r.y, r.w, r.h);
    }
    if items.len() > 24 {
        println!("  ...  and {} more", items.len() - 24);
    }

    println!();
    println!("timings (ms)");
    println!("  capture       {:>7.2}   D17: full-res PNG, off the hot path", ms(t_cap));
    println!("  decode        {:>7.2}   PNG -> luma, {}x{}", ms(t_dec), luma.w, luma.h);
    println!("  scope         {:>7.2}   largest rectangle the tree cannot explain", ms(t_scope));
    println!("  detect        {:>7.2}   sobel, projections, cells", ms(t_det));
    println!("  ------------------------");
    println!("  without capture {:>5.2}   what this would cost on a frame already in hand", ms(t_dec) + ms(t_scope) + ms(t_det));

    if let Some(n) = click {
        cv_click(n, &items, &luma, &shell, &det, &win, block, params, dry_run)?;
    }

    if let Some(path) = out {
        // The frame with the answer drawn on it. This is the only way to
        // check a detector without believing it, and §5.4d built the habit.
        let mut rgb = vec![0u8; luma.w * luma.h * 3];
        for (px, &l) in rgb.chunks_exact_mut(3).zip(&luma.pixels) {
            px.fill(l);
        }
        let stroke = |rgb: &mut Vec<u8>, r: Rect, c: [u8; 3]| {
            let (x0, y0) = ((r.x - luma.origin.0).max(0) as usize, (r.y - luma.origin.1).max(0) as usize);
            let (x1, y1) = ((x0 + r.w.max(0) as usize).min(luma.w), (y0 + r.h.max(0) as usize).min(luma.h));
            for x in x0..x1 {
                for y in [y0, y1.saturating_sub(1)] {
                    if y < luma.h { rgb[(y * luma.w + x) * 3..][..3].copy_from_slice(&c); }
                }
            }
            for y in y0..y1 {
                for x in [x0, x1.saturating_sub(1)] {
                    if x < luma.w { rgb[(y * luma.w + x) * 3..][..3].copy_from_slice(&c); }
                }
            }
        };
        stroke(&mut rgb, block, [255, 255, 0]);
        for r in &items {
            stroke(&mut rgb, *r, [0, 255, 255]);
        }
        let file = std::fs::File::create(path)?;
        let mut enc = png::Encoder::new(std::io::BufWriter::new(file), luma.w as u32, luma.h as u32);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header()?.write_image_data(&rgb)?;
        println!();
        println!("wrote           {path}   (yellow: the opaque block, cyan: what was found)");
    }
    Ok(())
}

/// D12's fallback, and D3's cost being paid: drive the pointer by hand when
/// there is nothing to hint.
///
/// **The keyboard this wants is the one the extension is holding.** Real free
/// mode runs under the modal grab, where `hjkl` reach muvor as key events and
/// nothing reaches the application. The extension does not forward keys yet —
/// that is version 5 and a logout (§5.4a) — so this command drives the same
/// state machine from the terminal instead: `--keys` replays a sequence, and
/// with no `--keys` it reads lines from stdin, a line at a time.
///
/// Line-buffered rather than raw, and that is a real limitation rather than
/// an oversight: putting the terminal in raw mode is a `termios` call, and
/// this workspace forbids `unsafe`. Type `jjjl` and press Enter. The state
/// machine underneath is the one the grab will drive, unchanged, so what is
/// being tested here is everything except the key delivery.
///
/// muvor does not read `/dev/input` here or anywhere (Doctrine): these are
/// this process's own stdin, handed over by the terminal that owns it.
fn free_cmd(args: &[&str]) -> Result<(), Box<dyn std::error::Error>> {
    let mut keys = None;
    let mut dry_run = false;
    let mut i = 0;
    while i < args.len() {
        match args[i] {
            "--keys" => {
                keys = Some(*args.get(i + 1).ok_or("free: --keys needs a sequence")?);
                i += 2;
            }
            "--dry-run" => {
                dry_run = true;
                i += 1;
            }
            other => return Err(format!("free: unexpected argument {other}").into()),
        }
    }

    let shell = shell::Shell::connect()?;
    let mut client = Client::connect()?;
    let mapping = calibrate(&shell, &mut client)?;
    let (span_w, span_h) = mapping.span();
    let (ox, oy) = mapping.origin();
    // Rounded, not truncated. The span is a *measurement* (§3.5's fit), so
    // it arrives as 1919.6 rather than 1920, and truncating it puts the
    // right-hand clamp one pixel inside the screen — which is exactly where
    // an edge target lives.
    #[allow(clippy::cast_possible_truncation)]
    let bounds = Rect::new(
        ox.round() as i32,
        oy.round() as i32,
        span_w.round() as i32,
        span_h.round() as i32,
    );

    // Where the pointer actually is, from the compositor — §3.5's readback,
    // and the reason free mode can start anywhere rather than warping to the
    // middle of the screen first.
    let start = shell.pointer()?;
    let mut free = muvor_core::Free::new(start, bounds, muvor_core::free::Params::default());
    println!("free mode       pointer at {},{}  screen {}x{} at {ox:.0},{oy:.0}",
        start.0, start.1, bounds.w, bounds.h);
    println!("  hjkl          left / down / up / right (vim), accelerating while you hold a direction");
    println!("  space         left click");
    println!("  q             leave");
    if dry_run {
        println!("  --dry-run     nothing is injected");
    }

    let mut last = std::time::Instant::now();
    let mut moves = 0u32;
    let apply = |free: &muvor_core::Free,
                     client: &mut Client|
     -> Result<(), Box<dyn std::error::Error>> {
        let (x, y) = free.pos();
        let (dx, dy) = mapping.to_device(x, y);
        if !dry_run {
            client.move_abs(dx, dy)?;
        }
        Ok(())
    };

    let mut feed = |line: &str, client: &mut Client, free: &mut muvor_core::Free, moves: &mut u32|
     -> Result<bool, Box<dyn std::error::Error>> {
        for c in line.chars() {
            if let Some(dir) = muvor_core::Dir::from_key(c) {
                let gap = last.elapsed();
                let step = free.next_step(dir, gap);
                let (x, y) = free.nudge(dir, gap);
                last = std::time::Instant::now();
                *moves += 1;
                apply(free, client)?;
                println!("  {c}  {step:>4} px -> {x},{y}");
            } else {
                match c {
                    ' ' => {
                        let (x, y) = free.pos();
                        let (dx, dy) = mapping.to_device(x, y);
                        if dry_run {
                            println!("  _  click at {x},{y} — --dry-run, not injected");
                        } else {
                            client.click_at(dx, dy, Button::Left)?;
                            println!("  _  clicked {x},{y}");
                        }
                    }
                    'q' => return Ok(false),
                    // Anything else is ignored rather than refused: under
                    // the grab this will be a stray keypress, and refusing
                    // one by leaving the mode would be the worst possible
                    // response to a typo.
                    _ => {}
                }
            }
        }
        Ok(true)
    };

    if let Some(seq) = keys {
        feed(seq, &mut client, &mut free, &mut moves)?;
    } else if let Ok(mut stream) = shell.keys().and_then(|s| shell.grab_keys().map(|()| s)) {
        // Extension v5: the grab is ours and every key comes as a signal.
        // This is what free mode is *for* — the application under the
        // pointer sees nothing, which is the whole difference between
        // driving the pointer and typing at whatever has focus.
        //
        // **One stream, opened before the grab.** `Shell::wait` opens and
        // closes one per call, so every key pressed while free mode was
        // injecting the last move was dropped on the floor — invisible at
        // one move per press, and the same fault that made movement mode's
        // action keys need a second try (see `movement_mode`).
        println!("  (grabbed — the keyboard is muvor's until Escape)");
        loop {
            match stream.next(std::time::Duration::from_secs(60))? {
                // A release is nothing to free mode: it moves once per press
                // (§4.5c). Movement mode is the one that needs both (§12.4).
                shell::Outcome::KeyUp(_) => {}
                shell::Outcome::TypedHold(_) => {}
                // **Free mode wants the repeats**, which is the opposite of
                // movement mode and the reason v10 marks them rather than
                // dropping them. Free mode has no tick: it moves exactly
                // once per press, so a held `l` sweeps only because the
                // keyboard keeps sending it (§4.5c). Movement mode is
                // tick-driven and a repeat is worth nothing to it but proof
                // the key is still down (§12.16).
                shell::Outcome::KeyRepeat(name) | shell::Outcome::KeyDown(name) => {
                    // Names, not characters (§4.5c): `space` is a word here.
                    let c = match name.as_str() {
                        "space" => ' ',
                        "q" | "Q" => 'q',
                        other => other.chars().next().unwrap_or('\0'),
                    };
                    if !feed(&c.to_string(), &mut client, &mut free, &mut moves)? {
                        shell.hide().ok();
                        break;
                    }
                }
                shell::Outcome::Cancelled => {
                    println!("  escape");
                    break;
                }
                shell::Outcome::TimedOut => {
                    shell.hide().ok();
                    println!("  nothing pressed for 60s — the grab was let go");
                    break;
                }
                other => println!("  ignored {other:?}"),
            }
        }
    } else {
        // Pre-v5, or no extension at all. The same state machine, driven a
        // line at a time from a terminal that still has the keyboard.
        println!("  (no v5 extension — reading stdin instead, a line at a time)");
        use std::io::BufRead;
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            if !feed(&line?, &mut client, &mut free, &mut moves)? {
                break;
            }
        }
    }

    let (x, y) = free.pos();
    println!();
    println!("left free mode  pointer at {x},{y} after {moves} move{}",
        if moves == 1 { "" } else { "s" });
    Ok(())
}

/// One window muvor is willing to hint, and what is on top of it.
///
/// §4.3 hinted the focused window and nothing else, for a correctness reason
/// rather than a speed one: the accessibility tree cannot tell you a window
/// is covered, so a badge on a buried node clicks whatever is above it.
/// Extension v8 supplies the compositor's stacking order, which §4.3 named
/// as the only thing that could answer it, and a `Pane` is one window with
/// that answer attached.
struct Pane {
    win: shell::Window,
    frame: muvor_atspi::Frame,
    scan: muvor_atspi::Scan,
    /// The frame rects of every window stacked **above** this one, in screen
    /// coordinates. The frame rect and not the buffer rect: a client-side
    /// shadow is translucent and hides nothing (§5.1h).
    above: Vec<Rect>,
    /// This window's own frame rect, clipped to the display.
    area: Rect,
    /// What fraction of `area` nothing covers. Reported, not enforced — the
    /// per-target rule is the click point (`muvor_core::is_covered`).
    visible: f64,
    /// What the a11y frame lookup and the tree read cost for this window
    /// alone.
    ///
    /// Per pane rather than one total, because §5.2's budget is now spent
    /// across N windows and "20 ms" says nothing about whether that is four
    /// cheap panes or one pathological one. §2.5's rule: a measurement
    /// nobody can see gets rediscovered.
    t_frame: std::time::Duration,
    t_detect: std::time::Duration,
}

/// Every window worth hinting, bottom of the stack to top.
///
/// Falls back to the focused window alone when the extension is older than
/// v8 and has no `Windows` — hinting one window is what muvor did for
/// eleven months and is not an error.
///
/// **Skipped, and each for its own reason:** a minimized window and one the
/// compositor says is not showing have no pixels to aim at; a window with a
/// degenerate rect cannot be clipped against; a window whose click points
/// are all covered contributes nothing but time; and a window with no
/// accessibility tree is D12's case, which the camera answers at tier 2
/// rather than the tree at tier 1.
fn panes(
    shell: &shell::Shell,
    det: &muvor_atspi::Detector,
    screen: Rect,
    out: Out<'_>,
) -> Result<Vec<Pane>, Box<dyn std::error::Error>> {
    let all = match shell.windows() {
        Ok(w) if !w.is_empty() => w,
        _ => {
            let win = shell.focused_window()?;
            if win.is_none() {
                return Ok(Vec::new());
            }
            vec![win]
        }
    };

    // Bottom to top, so the windows above one already-seen window are the
    // ones that come after it. Built in one pass backwards instead: for each
    // window, everything later in the list is on top of it.
    let mut panes = Vec::new();
    for (i, win) in all.iter().enumerate() {
        if win.minimized || !win.showing || win.w <= 0 || win.h <= 0 {
            continue;
        }
        let above: Vec<Rect> = all[i + 1..]
            .iter()
            .filter(|w| !w.minimized && w.showing && w.w > 0 && w.h > 0)
            .map(|w| Rect::new(w.x, w.y, w.w, w.h))
            .collect();
        let area = clip(Rect::new(win.x, win.y, win.w, win.h), screen);
        if area.w <= 0 || area.h <= 0 {
            continue;
        }
        let visible = muvor_core::visible_fraction(area, &above);
        // Entirely buried. Not a judgement about usefulness — there is
        // nothing on screen to aim at, so every target it could offer would
        // fail the click-point rule anyway, one bus walk later.
        if visible <= 0.0 {
            continue;
        }
        let found = det.frame_for_window(&win.title, win.pid, (win.w, win.h))?;
        let t_frame = found.elapsed;
        let Some(frame) = found.value else {
            say!(out, "  window         {} '{}' — no accessibility tree (D12), camera only",
                win.wm_class, win.title);
            continue;
        };
        let scanned = det.targets(&frame, None, true)?;
        let t_detect = scanned.elapsed;
        let scan = scanned.value;
        panes.push(Pane {
            win: win.clone(), frame, scan, above, area, visible, t_frame, t_detect,
        });
    }
    Ok(panes)
}

/// Everything between "the tree has answered" and "the overlay is drawn".
///
/// Extracted so it can run **twice**: once for tier 1, and again when the
/// user presses Tab and asks for tier 2 over an overlay that is already up
/// (D18). A second copy of this, differing only in whether the camera ran,
/// would be two places to get the origin arithmetic wrong.
struct Assembly {
    aims: Vec<Aim>,
    cv: Vec<muvor_core::CvClaim>,
    /// Which pane each camera claim was found in, parallel to `cv`.
    ///
    /// §4.5b re-captures and re-detects to validate, and it has to do that
    /// against the same window's picture the claim came from. A claim is a
    /// rectangle and its pixels and carries no window with it, so the pane
    /// travels beside it.
    cv_pane: Vec<usize>,
    labels: Labels,
    placements: Vec<Placement>,
    hints: Vec<shell::Hint>,
    deep: bool,
    t_deep: std::time::Duration,
    t_label: std::time::Duration,
}

#[allow(clippy::too_many_arguments)]
fn assemble(
    deep: bool,
    panes: &[Pane],
    shell: &shell::Shell,
    span: (f64, f64),
    origin_x: f64,
    out: Out<'_>,
) -> Result<Assembly, Box<dyn std::error::Error>> {
    let (span_w, span_h) = span;
    let tree_total: usize = panes.iter().map(|p| p.scan.targets.len()).sum();

    // --- tier 1: the tree, and what is on top of it --------------------
    //
    // Screen-space rectangles for every pane's targets, in pane order, with
    // the ones nobody can see dropped. The rule is the **click point**, not
    // the rectangle (`muvor_core::occlude`): a target half behind a dialog
    // is still clickable if the pixel muvor aims at is showing, and refusing
    // those would drop a column of a sidebar every time a dialog overlapped
    // its edge. A target whose click point is covered is never offered —
    // that is the badge that would click whatever is on top, which is the
    // wrong-click outcome §4.5 exists to prevent and cannot itself catch,
    // because everything about the buried claim is true except that nobody
    // can see it.
    let mut aims: Vec<Aim> = Vec::new();
    let mut screen_rects: Vec<(i32, i32, i32, i32)> = Vec::new();
    let mut hidden = 0usize;
    for (pi, pane) in panes.iter().enumerate() {
        let (wx, wy) = pane.win.origin();
        for (ti, t) in pane.scan.targets.iter().enumerate() {
            let r = Rect::new(
                wx + t.bounds.x - pane.frame.bounds.x,
                wy + t.bounds.y - pane.frame.bounds.y,
                t.bounds.w,
                t.bounds.h,
            );
            if muvor_core::is_covered(r.centre(), &pane.above) {
                hidden += 1;
                continue;
            }
            aims.push(Aim::Tree(pi, ti));
            screen_rects.push((r.x, r.y, r.w, r.h));
        }
    }

    // --- D18: tier 2, the camera ---------------------------------------
    //
    // Tier 1 above is unchanged and costs what it always did. Tier 2 runs
    // when the tree found nothing — D12's case, where showing an empty
    // overlay is indistinguishable from being broken — or when the user
    // asked for it. It is never automatic merely because a window has a
    // large opaque area: Nautilus is 76% opaque with a perfectly good
    // toolbar, and making it slow by default buys nobody anything.
    //
    // **Per pane since v8.** One capture, and then the same question asked
    // once per visible window, because `grid`'s projection is a
    // within-one-surface argument: run over a rectangle spanning two windows
    // it finds the seam between them, not the items in either.
    let go_deep = deep || screen_rects.is_empty();
    let mut cv: Vec<muvor_core::CvClaim> = Vec::new();
    let mut cv_pane: Vec<usize> = Vec::new();
    let mut t_deep = std::time::Duration::ZERO;
    if go_deep {
        let t = std::time::Instant::now();
        let shot = shell.capture()?;
        let luma = frame::decode(&shot.png, (shot.x, shot.y))?;
        let params = muvor_core::Params::for_scale(shot.scale);
        for (pi, pane) in panes.iter().enumerate() {
            let area = clip(pane.area, luma.bounds());
            if area.w <= 0 || area.h <= 0 {
                continue;
            }
            // Two kinds of thing the camera must not offer again, and they
            // are the same kind of thing: a rectangle somebody else has
            // already explained. The tree explains its own targets; the
            // windows on top explain their own pixels, and a CV item found
            // under one of them would be an item detected in the wrong
            // window's picture.
            let (wx, wy) = pane.win.origin();
            let mut occupied: Vec<Rect> = pane
                .scan
                .targets
                .iter()
                .map(|t| {
                    Rect::new(
                        wx + t.bounds.x - pane.frame.bounds.x,
                        wy + t.bounds.y - pane.frame.bounds.y,
                        t.bounds.w,
                        t.bounds.h,
                    )
                })
                .filter(|r| overlaps(*r, area))
                .collect();
            occupied.extend(pane.above.iter().copied().filter(|r| overlaps(*r, area)));

            let Some(block) = muvor_core::opaque_block(area, &occupied) else {
                say!(out, "  deep           {} — the tree and the stack explain every pixel",
                    pane.win.wm_class);
                continue;
            };
            let (items, found) =
                muvor_core::targets_in(&luma.pixels, luma.w, block, luma.origin, params);
            for &r in &items {
                cv.push(muvor_core::CvClaim {
                    rect: r,
                    click: r.centre(),
                    print: muvor_core::Fingerprint::of(&luma.pixels, luma.w, r, luma.origin),
                });
                cv_pane.push(pi);
            }
            say!(out, "  deep           {},{} {}x{} opaque in {} -> {}",
                block.x, block.y, block.w, block.h, pane.win.wm_class,
                match found {
                    muvor_core::Found::Grid(n) => format!("{n} items, a grid (§5.4f)"),
                    muvor_core::Found::Single => "one centre target (D11)".to_owned(),
                });
        }
        t_deep = t.elapsed();
    }
    for (i, c) in cv.iter().enumerate() {
        aims.push(Aim::Cv(i));
        screen_rects.push((c.rect.x, c.rect.y, c.rect.w, c.rect.h));
    }

    if screen_rects.is_empty() {
        let examined: usize = panes.iter().map(|p| p.scan.examined).sum();
        let rejected: usize = panes.iter().map(|p| p.scan.rejects.len()).sum();
        return Err(format!(
            "nothing to aim at. {tree_total} tree target{} across {} visible window{}, \
             {hidden} of them behind something ({examined} examined, {rejected} rejected), \
             and the camera found nothing either.\n  This is the bottom of the ladder, and \
             D12's answer is `muvor free` — hjkl, and the pointer goes where you push it.",
            if tree_total == 1 { "" } else { "s" },
            panes.len(),
            if panes.len() == 1 { "" } else { "s" },
        )
        .into());
    }
    if hidden > 0 {
        say!(out, "  occluded       {hidden} tree target{} dropped — click point covered (§4.3)",
            if hidden == 1 { "" } else { "s" });
    }

    let t_label = std::time::Instant::now();
    // Labelled by *screen* column (§5.3b), not by position within the window:
    // the first key means "how far right on the display", so the rightmost
    // thing in view is `;`-something whether it belongs to a maximized window
    // or a narrow one parked on the right. A window-relative column would
    // rename every target the moment the window moved — and with more than
    // one window on the overlay it is the only rule that can name them all
    // without two windows claiming the same label.
    let points: Vec<Point> = screen_rects
        .iter()
        .map(|&(x, y, w, h)| Point::new(x + w / 2, y + h / 2))
        .collect();
    let ox = origin_x;
    let labels =
        Labels::in_columns(Alphabet::default(), Alphabet::ranks(), &points, ox as i32, span_w as i32);
    let t_label = t_label.elapsed();

    // §5.1a: the badge sits beside the target rather than on top of it, and
    // the click lands where the two meet. One rule, computed once, used for
    // both the drawing and the injection — so what the user aims at and what
    // muvor clicks cannot drift apart.
    //
    // Whether there is room on the left is a screen-space question and these
    // bounds are window-relative (D13), so it is answered here and passed in.
    // Placed in **label order**, and as a set: a badge takes the first of
    // left/right/above/below that is on screen and clear of every badge
    // already placed (§5.1c). Screen coordinates throughout, because "on
    // screen" and "clear of" are both screen-space questions.
    let ordered: Vec<(i32, i32, i32, i32)> = labels.iter().map(|(_, i)| screen_rects[i]).collect();
    let screen = (span_w as i32, span_h as i32);
    let by_label = muvor_core::place_all(&ordered, screen);

    // Back to target order, so `placements[target_index]` still means what
    // the rest of this function thinks it means.
    let mut placements: Vec<Placement> = vec![by_label[0]; aims.len()];
    for ((_, target), p) in labels.iter().zip(&by_label) {
        placements[target] = *p;
    }

    let hints: Vec<shell::Hint> = labels
        .iter()
        .map(|(label, i)| {
            // Already screen-space: `place_all` was given screen bounds.
            let (x, y, w, h) = placements[i].badge();
            let (cx, cy) = placements[i].click();
            shell::Hint { x, y, w, h, click_x: cx, click_y: cy, label: label.to_owned() }
        })
        .collect();


    Ok(Assembly { aims, cv, cv_pane, labels, placements, hints, deep: go_deep, t_deep, t_label })
}

/// What a badge points at (D18).
///
/// Two kinds of target now share one overlay, one label alphabet and one
/// placement pass — and part at the last possible moment, where they are
/// validated. A tree target has a name and an object path; a CV target has
/// a rectangle and its pixels. Everything before the click can treat them
/// the same, and nothing after it can.
#[derive(Debug, Clone, Copy)]
enum Aim {
    /// A pane, and an index into that pane's AT-SPI scan. Validated by §4.5.
    ///
    /// The pane index is not decoration: validation re-reads the target
    /// through *its own window's* frame, and with more than one window on
    /// the overlay a bare target index would verify the right node against
    /// the wrong tree.
    Tree(usize, usize),
    /// An index into the camera's claims. Validated by §4.5b.
    Cv(usize),
}

/// §4.5b, factored out because both `hint` and `detect --click` need it and
/// a second copy of a safety check is a second thing to get wrong.
///
/// Re-captures, re-scopes against a *fresh* AT-SPI read, re-detects, and
/// compares. The re-read matters: if the tree has grown a target where the
/// camera used to see nothing, the opaque block shrinks and the item may
/// legitimately no longer be the detector's to offer.
///
/// **The window is named by the caller, not looked up.** It used to ask the
/// compositor what was focused, which was the same window by construction
/// while muvor only ever hinted one. It is not any more: a CV target can
/// belong to a window the user is not typing into, and re-deriving the area
/// from the focused window would scope the second capture to a different
/// picture and refuse every such click — which looks exactly like §4.5b
/// working, and is the most expensive kind of wrong.
fn revalidate_cv(
    shell: &shell::Shell,
    det: &muvor_atspi::Detector,
    claim: &muvor_core::CvClaim,
    win: &shell::Window,
    above: &[Rect],
) -> Result<muvor_core::CvVerdict, Box<dyn std::error::Error>> {
    let shot = shell.capture()?;
    let luma = frame::decode(&shot.png, (shot.x, shot.y))?;
    let area = clip(Rect::new(win.x, win.y, win.w, win.h), luma.bounds());
    let mut occupied = Vec::new();
    if let Some(f) = det.frame_for_window(&win.title, win.pid, (win.w, win.h))?.value {
        let (wx, wy) = win.origin();
        occupied = det
            .targets(&f, None, true)?
            .value
            .targets
            .iter()
            .map(|t| {
                Rect::new(wx + t.bounds.x - f.bounds.x, wy + t.bounds.y - f.bounds.y, t.bounds.w, t.bounds.h)
            })
            .filter(|r| overlaps(*r, area))
            .collect();
    }
    // The stack, for the same reason `assemble` passes it: a window that
    // moved on top of this one since the overlay went up now explains those
    // pixels, and an item still "found" underneath it is one nobody can see.
    occupied.extend(above.iter().copied().filter(|r| overlaps(*r, area)));
    let Some(block) = muvor_core::opaque_block(area, &occupied) else {
        // The tree now explains the whole window, so there is no region for
        // this claim to live in. Refuse: something changed enough that the
        // detector would not offer this target again.
        return Ok(muvor_core::CvVerdict::Gone);
    };
    let params = muvor_core::Params::for_scale(shot.scale);
    let (fresh, _) = muvor_core::targets_in(&luma.pixels, luma.w, block, luma.origin, params);
    Ok(muvor_core::revalidate(claim, &fresh, &luma.pixels, luma.w, luma.origin))
}

/// Click a CV target, through §4.5b's validation.
///
/// M6's exit criterion: a target *found and clicked* in a region the
/// accessibility tree does not explain. The finding was §5.4f; this is the
/// clicking, and the whole of the difficulty is in the two lines that refuse.
///
/// **The claim is made against the frame the item was found in, and checked
/// against a frame captured now.** That second capture is the entire point:
/// §4.5 re-resolves through the bus because the bus is how it found the
/// target, and this re-captures because the camera is how this one did.
#[allow(clippy::too_many_arguments)]
fn cv_click(
    n: usize,
    items: &[Rect],
    luma: &frame::Luma,
    shell: &shell::Shell,
    det: &muvor_atspi::Detector,
    win: &shell::Window,
    block: Rect,
    params: muvor_core::Params,
    dry_run: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let item = *items
        .get(n.wrapping_sub(1))
        .ok_or_else(|| format!("detect: --click {n} but only {} items were found", items.len()))?;
    let claim = muvor_core::CvClaim {
        rect: item,
        click: item.centre(),
        print: muvor_core::Fingerprint::of(&luma.pixels, luma.w, item, luma.origin),
    };
    println!();
    println!("claim           item {n} at {},{} {}x{}", item.x, item.y, item.w, item.h);

    // §4.5b: re-observe through the same channel that found it. The frame
    // must be taken with nothing of muvor's own drawn over the target —
    // there is no overlay up in this command, but `hint` will have to hide
    // one first (§5.1c's photography trick, working against us).
    let t = std::time::Instant::now();
    let shot = shell.capture()?;
    let fresh_luma = frame::decode(&shot.png, (shot.x, shot.y))?;
    let win_now = shell.focused_window()?;
    let area = clip(Rect::new(win_now.x, win_now.y, win_now.w, win_now.h), fresh_luma.bounds());
    let mut occupied = Vec::new();
    if let Some(f) = det.frame_for_window(&win_now.title, win_now.pid, (win_now.w, win_now.h))?.value
    {
        let scan = det.targets(&f, None, true)?.value;
        occupied = scan
            .targets
            .iter()
            .map(|t| {
                Rect::new(
                    win_now.x + t.bounds.x - f.bounds.x,
                    win_now.y + t.bounds.y - f.bounds.y,
                    t.bounds.w,
                    t.bounds.h,
                )
            })
            .filter(|r| overlaps(*r, area))
            .collect();
    }
    let fresh_block = muvor_core::opaque_block(area, &occupied).unwrap_or(block);
    let (fresh, _) =
        muvor_core::targets_in(&fresh_luma.pixels, fresh_luma.w, fresh_block, fresh_luma.origin, params);
    let verdict =
        muvor_core::revalidate(&claim, &fresh, &fresh_luma.pixels, fresh_luma.w, fresh_luma.origin);
    println!("validate        {:>6.2} ms — {}", ms(t.elapsed()), verdict.why());

    // The window itself moving is not something revalidate can see: it
    // compares rectangles in screen space, and if the whole window shifted
    // the item moved with it and matches nothing. Named separately because
    // "the view moved" and "you focused something else" want different
    // fixes from the user.
    if win_now.pid != win.pid || (win_now.x, win_now.y) != (win.x, win.y) {
        println!("                NOTE the window is not where it was, or is not the same window");
    }
    if !verdict.is_ok() {
        println!("REFUSED — nothing was clicked.");
        println!("A refused click is annoying; a wrong click is unrecoverable (§4.5, §4.5b).");
        return Ok(());
    }

    let (cx, cy) = claim.click;
    let mut client = Client::connect()?;
    let mapping = calibrate(shell, &mut client)?;
    let (dx, dy) = mapping.to_device(cx, cy);
    println!("click           {cx},{cy} screen -> {dx},{dy} device");
    if dry_run {
        println!("--dry-run — not injected");
        return Ok(());
    }
    let t_click = std::time::Instant::now();
    client.click_at(dx, dy, Button::Left)?;
    println!("injected        {:>6.2} ms  BATCH move+press+release", ms(t_click.elapsed()));
    Ok(())
}

/// `r` clipped into `bounds`.
fn clip(r: Rect, bounds: Rect) -> Rect {
    let x = r.x.max(bounds.x);
    let y = r.y.max(bounds.y);
    let x1 = r.x.saturating_add(r.w).min(bounds.x.saturating_add(bounds.w));
    let y1 = r.y.saturating_add(r.h).min(bounds.y.saturating_add(bounds.h));
    Rect::new(x, y, (x1 - x).max(0), (y1 - y).max(0))
}

fn overlaps(a: Rect, b: Rect) -> bool {
    a.x < b.x.saturating_add(b.w)
        && b.x < a.x.saturating_add(a.w)
        && a.y < b.y.saturating_add(b.h)
        && b.y < a.y.saturating_add(a.h)
}


/// Minimal JSON string escaping. Hand-rolled because this workspace carries
/// no serialization dependency and one flat object does not justify adding
/// one — window titles are the only untrusted text that reaches it, and a
/// title containing a quote or a newline is ordinary rather than exotic.
fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// M2's instrument. Bounds are printed **window-relative** and labelled as
/// such: AT-SPI reports the same numbers for SCREEN and WINDOW on Wayland
/// (D13, plan.md §4.2a), so a dump that omitted the caveat would be printing
/// coordinates that are quietly wrong everywhere but a maximized window at
/// the origin. The window origin arrives with the extension, at M4.
fn dump(args: &[&str]) -> Result<(), Box<dyn std::error::Error>> {
    let mut window = None;
    let mut pid = None;
    let mut via = None;
    let mut rejects = false;
    let mut metadata = true;
    let mut warm = false;
    let mut hold = 0u64;
    let mut tree = false;
    let mut typed = None;
    let mut i = 0;
    while i < args.len() {
        match args[i] {
            "--window" => {
                window = Some(*args.get(i + 1).ok_or("--window needs a name")?);
                i += 2;
            }
            "--pid" => {
                pid = Some(args.get(i + 1).ok_or("--pid needs a number")?.parse::<u32>()?);
                i += 2;
            }
            "--via" => {
                via = Some(match *args.get(i + 1).ok_or("--via needs a path")? {
                    "mirror" => Provenance::Mirror,
                    "collection" => Provenance::Collection,
                    "cache" => Provenance::Cache,
                    "walk" => Provenance::Walk,
                    other => {
                        return Err(format!(
                            "--via {other}: expected mirror|collection|cache|walk"
                        )
                        .into())
                    }
                });
                i += 2;
            }
            "--rejects" => {
                rejects = true;
                i += 1;
            }
            "--no-meta" => {
                metadata = false;
                i += 1;
            }
            "--warm" => {
                warm = true;
                i += 1;
            }
            "--tree" => {
                tree = true;
                warm = true;
                i += 1;
            }
            "--type" => {
                typed = Some(*args.get(i + 1).ok_or("--type needs keys")?);
                i += 2;
            }
            "--hold" => {
                hold = args.get(i + 1).ok_or("--hold needs seconds")?.parse()?;
                warm = true;
                i += 2;
            }
            other => return Err(format!("dump: unexpected argument {other}").into()),
        }
    }

    let det = muvor_atspi::Detector::connect()?;
    // `--pid` is the product lookup (§4.3a) with the compositor stubbed out
    // by hand: it is how the pid-then-title match can be exercised — and
    // measured — without a session restart to load the extension.
    let found = match (pid, window) {
        (Some(p), q) => det.frame_for_window(q.unwrap_or_default(), p, (0, 0))?,
        (None, Some(q)) => det.frame_named(q)?,
        (None, None) => det.focused_frame()?,
    };

    print!("window discovery {:.2} ms", found.ms());
    let Some(frame) = found.value else {
        println!();
        if window.is_some() {
            println!("no window matched — try a substring of the app name or the title");
        } else {
            println!("no focused window.");
            println!();
            println!("Measured on GNOME 48 Wayland: no GTK frame sets STATE_ACTIVE, so focus");
            println!("is tracked from window:activate events — and a one-shot CLI subscribes");
            println!("microseconds before it asks, so it has seen none. This is not a bug in");
            println!("detection and it disappears at M4, when the extension names the focused");
            println!("window outright. Until then:  muvor dump --window <name>");
        }
        return Ok(());
    };
    println!("   (via {})", frame.focus.as_str());
    println!("window           {} — {}  [{}]", frame.app, frame.title, frame.role);
    println!("                 {} on {}", frame.path, frame.bus_name);
    println!("bounds           {}", frame.bounds);

    if warm {
        // What a long-running muvor pays on window:activate, and never at
        // the keypress (§4.2-iii). A one-shot CLI has to ask for it.
        let w = det.warm(&frame)?;
        println!(
            "warm             {:>6.2} ms   {} nodes via {:?}, {} in this window",
            w.ms(),
            w.value.nodes,
            w.value.source,
            w.value.in_window
        );
    }

    if hold > 0 {
        // The detector thread stays alive and keeps draining while this
        // process does nothing at all — which is the state a hotkey tool is
        // in for all but a few milliseconds a day.
        println!("hold             {hold}s ...");
        std::thread::sleep(std::time::Duration::from_secs(hold));
    }

    if tree {
        println!();
        println!("mirror nodes for {} (frame is {})", frame.bus_name, frame.path);
        for (path, parent, role, name) in det.mirror_nodes(&frame.bus_name, 20)? {
            println!("  {path:<44} parent {parent:<44} {role} {name}");
        }
    }

    let scan = det.targets(&frame, via, metadata)?;
    let s = &scan.value;
    if warm {
        let m = det.mirror()?;
        println!();
        println!("mirror           {} events delivered", m.value.seen);
        for a in &m.value.apps {
            println!(
                "  {:<10} {:>5} nodes via {:<5?} {:>4} updates  warmed {:.1}s ago",
                a.bus_name,
                a.nodes,
                a.source,
                a.updates,
                a.age.as_secs_f64()
            );
        }
    }
    println!();
    println!(
        "probe            {:<10}  {} examined -> {} candidates{}",
        s.provenance.as_str(),
        s.examined,
        s.candidates,
        if s.truncated { format!(" (capped at {})", muvor_atspi::probe::MAX_NODES) } else { String::new() }
    );
    if let (Some(age), Some(updates)) = (s.mirror_age, s.mirror_updates) {
        println!(
            "  mirror         {:>6.0} ms old, {} events seen, {updates} applied",
            ms(age),
            s.mirror_seen.unwrap_or(0)
        );
    }
    println!("  select         {:>6.2} ms", ms(s.select));
    println!("  resolve        {:>6.2} ms   {} over {} survivors",
        ms(s.resolve),
        if metadata { "role+name+extents" } else { "extents only" },
        s.candidates);
    // The M2 exit criterion is about detection, so it is select+resolve and
    // not the window hunt above it: §4.3's enumeration is what M4 deletes.
    let total = ms(s.select) + ms(s.resolve);
    println!(
        "  total          {:>6.2} ms   {} the 5 ms budget (§7)",
        total,
        if total <= 5.0 { "inside" } else { "OVER" }
    );
    println!();
    println!("{} targets   (bounds are WINDOW-RELATIVE, D13 — screen coords land at M4)", s.targets.len());

    // D5: labels are assigned by screen column (§5.3b), and this list is
    // printed in that same order — one ordering in the product, not one for
    // the user and another for the log.
    //
    // dump has no window origin (the compositor supplies it at M4), so the
    // columns are cut across the *window* here where `muvor hint` cuts them
    // across the screen. Identical for a maximized window, which is what
    // dump is usually pointed at, and off by the window's own x otherwise.
    let points: Vec<Point> = s
        .targets
        .iter()
        .map(|t| {
            let (x, y) = t.bounds.centre();
            Point::new(x, y)
        })
        .collect();
    let labels = Labels::in_columns(Alphabet::default(), Alphabet::ranks(), &points, 0, frame.bounds.w);
    for (label, i) in labels.iter() {
        let t = &s.targets[i];
        println!("  {label}  {:<16} {:<20} {}", t.role.to_string(), t.bounds.to_string(), t.name);
    }

    if let Some(keys) = typed {
        println!();
        let mut session = labels.typing();
        for c in keys.chars() {
            match session.press(c) {
                Progress::Pending { matches } => {
                    println!("  '{c}'  -> {matches} labels still match ({}…)", session.typed());
                }
                Progress::Hit { target } => {
                    let t = &s.targets[target];
                    // The same placement rule `muvor hint` injects with
                    // (§5.1a), so what dump validates is what hint would
                    // click. dump has no window origin — the compositor
                    // supplies that at M4 — so "room on the left" is judged
                    // window-relative, which is the screen's answer too for
                    // the maximized windows dump is usually pointed at.
                    let spot = place(t.bounds.x, t.bounds.y, t.bounds.w, t.bounds.h, t.bounds.x >= BADGE_W);
                    let (cx, cy) = spot.click();
                    println!("  '{c}'  -> HIT {} '{}'", t.role, t.name);
                    println!("         would click {cx},{cy} window-relative, plus the window");
                    println!("         origin the compositor supplies (D13)");
                    println!("         badge {:?} of it, click at its centre (§5.1a)", spot.side);
                    // The §4.5 check, run here for the same reason `--dump`
                    // exists at all: it is the only way to exercise it
                    // against a real application without a hotkey, a grab
                    // and a human. What it prints is exactly what decides
                    // whether `muvor hint` injects.
                    let claim = Claim::at(t, cx, cy);
                    let v = det.verify(&frame, &claim)?;
                    println!("         validate {:.2} ms — {}", v.ms(), v.value.verdict.why());
                    for (depth, step) in v.value.chain.iter().enumerate() {
                        println!("         {:width$}{step}", "", width = depth * 2);
                    }
                    if !v.value.verdict.is_ok() {
                        println!("         REFUSED — muvor hint would not click this");
                    }
                }
                Progress::Miss => println!("  '{c}'  -> miss, nothing typed"),
            }
        }
    }

    if rejects {
        println!();
        println!("{} rejected", s.rejects.len());
        for r in &s.rejects {
            println!("  {:<16} {:<20} {}", r.why.as_str(), r.role.to_string(), r.name);
        }
    } else if !s.rejects.is_empty() {
        println!();
        println!("{} rejected  (--rejects to see why)", s.rejects.len());
    }
    Ok(())
}

fn ms(d: std::time::Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn parse_xy(args: &[&str]) -> Result<(i64, i64), Box<dyn std::error::Error>> {
    let x = args.first().ok_or("need <x> <y>")?.parse()?;
    let y = args.get(1).ok_or("need <x> <y>")?.parse()?;
    Ok((x, y))
}

fn poke(args: &[&str], screen: (i64, i64)) -> Result<(), Box<dyn std::error::Error>> {
    let raw = args.first() == Some(&"--device");
    let args = if raw { &args[1..] } else { args };
    let (x, y) = parse_xy(args)?;
    let mut c = Client::connect()?;
    let max = c.abs_range_max();
    let (dx, dy) = if raw {
        (x as i32, y as i32)
    } else {
        // The *assumed* mapping (§5A.1), not a calibrated one: `poke` has to
        // work before the extension does, which is exactly when nothing can
        // read the pointer back. `muvor hint` calibrates instead.
        geom::assumed(screen.0 as i32, screen.1 as i32, max).to_device(x as i32, y as i32)
    };
    c.move_abs(dx, dy)?;
    println!("MOVE_ABS {dx} {dy}  (max {max}{})",
        if raw { String::new() } else { format!(", from {x},{y} px of {}x{}", screen.0, screen.1) });
    Ok(())
}

/// The gap between the clicks of a double or triple click (§12.8, settled
/// 2026-08-24).
///
/// **A `BATCH` is one `SYN_REPORT`** — uictl's `proto.h` says so in as many
/// words — so every event in it carries one timestamp. A double click sent
/// as one batch is four button events at a single instant, which is nothing
/// any mouse has ever produced, and a toolkit decides "double" by the
/// *interval* between two clicks. So a multiple click is N separate frames
/// with a real gap, and it costs N tokens instead of one.
///
/// **That is affordable and it was checked rather than assumed**: §3.4's
/// budget is 100 requests/second sustained with a **burst of 100** (§12.17),
/// so the two or three extra tokens of a deliberate click are not a cost at
/// all.
///
/// **80 ms**, against GNOME's `double-click` setting, which is 400 ms by
/// default and whose slider bottoms out at 100 ms — so this is inside the
/// threshold at every setting a user can choose, and it is also about what a
/// human hand actually does. The single click inside each frame stays
/// batched, because that form is proven: it is what M5 and §5.1f clicked
/// with.
const MULTI_CLICK_GAP: std::time::Duration = std::time::Duration::from_millis(80);

fn click(args: &[&str], screen: (i64, i64)) -> Result<(), Box<dyn std::error::Error>> {
    // `--slow` is a diagnostic, not a mode: three frames with a gap between
    // them instead of one BATCH, to answer whether a target that ignores a
    // batched click is refusing the *click* or refusing three events that
    // share one timestamp (§5.1g).
    let slow = args.contains(&"--slow");
    let one_frame = args.contains(&"--one-frame");
    // `--times` is how §12.8 gets measured against a real window: one
    // command, one window, and the app itself says whether it saw a double
    // click by opening or by selecting a word.
    let mut times: u8 = 1;
    let mut rest: Vec<&str> = Vec::new();
    let mut i = 0;
    let plain: Vec<&str> =
        args.iter().copied().filter(|a| *a != "--slow" && *a != "--one-frame").collect();
    while i < plain.len() {
        if plain[i] == "--times" {
            times = plain.get(i + 1).ok_or("click: --times needs a count")?.parse()?;
            if times == 0 || times > 3 {
                return Err("click: --times is 1, 2 or 3".into());
            }
            i += 2;
        } else {
            rest.push(plain[i]);
            i += 1;
        }
    }
    // `--here` clicks where the pointer already is, which is the whole
    // ceremony of §12.8's confirmation: park the mouse over a word, run one
    // command, and the application says what it saw. It calibrates rather
    // than assuming (§3.5), because a click that lands 20 px off proves
    // nothing about click *counting*.
    let here = rest.contains(&"--here");
    let rest: Vec<&str> = rest.into_iter().filter(|a| *a != "--here").collect();
    let mut c = Client::connect()?;
    let max = c.abs_range_max();
    let (x, y, dx, dy) = if here {
        let shell = shell::Shell::connect()?;
        let mapping = calibrate(&shell, &mut c)?;
        let (px, py) = shell.pointer()?;
        let (dx, dy) = mapping.to_device(px, py);
        (i64::from(px), i64::from(py), dx, dy)
    } else {
        let (x, y) = parse_xy(&rest)?;
        let (dx, dy) =
            geom::assumed(screen.0 as i32, screen.1 as i32, max).to_device(x as i32, y as i32);
        (x, y, dx, dy)
    };
    if slow {
        let gap = std::time::Duration::from_millis(40);
        c.move_abs(dx, dy)?;
        std::thread::sleep(gap);
        c.button(Button::Left, true)?;
        std::thread::sleep(gap);
        c.button(Button::Left, false)?;
        println!("SPLIT move, press, release at {x},{y} px ({dx},{dy}) — 40 ms apart, three tokens");
        return Ok(());
    }
    // `--one-frame` is the encoding §12.8 replaced, kept as a diagnostic for
    // the same reason `--slow` is kept: when an application ignores a
    // gesture, the question is always whether it is refusing the click or
    // refusing the *timing*, and the only way to tell is to send both.
    if one_frame {
        let code = Button::Left;
        let mut items = vec![muvor_uictl::BatchItem::MoveAbs { x: dx, y: dy }];
        for _ in 0..times {
            items.push(muvor_uictl::BatchItem::Button { code, down: true });
            items.push(muvor_uictl::BatchItem::Button { code, down: false });
        }
        c.batch(&items)?;
        println!(
            "ONE FRAME: {times} click(s) at {x},{y} px ({dx},{dy}) in a single BATCH —\n\
             one SYN_REPORT, one timestamp, one token. This is what a multiple\n\
             click looked like before §12.8."
        );
        return Ok(());
    }
    if times > 1 {
        for n in 0..times {
            if n > 0 {
                std::thread::sleep(MULTI_CLICK_GAP);
            }
            c.click_at(dx, dy, Button::Left)?;
        }
        println!(
            "{times} BATCH clicks at {x},{y} px ({dx},{dy}) — {} ms apart, {times} tokens.\n\
             What the window did with them is the answer: a double click opens the\n\
             item or selects the word, a triple selects the line (§12.8).",
            MULTI_CLICK_GAP.as_millis()
        );
        return Ok(());
    }
    c.click_at(dx, dy, Button::Left)?;
    println!("BATCH move+press+release at {x},{y} px ({dx},{dy}) — one rate-limit token");
    Ok(())
}

/// Measure the pixel → device mapping (§3.5).
///
/// Three injections and three readbacks, ~5 ms, once at startup. The pointer
/// is put back where it was found, because a tool that moves the pointer to
/// answer a question the user did not ask is a tool that gets turned off.
fn calibrate(
    shell: &shell::Shell,
    client: &mut Client,
) -> Result<geom::Mapping, Box<dyn std::error::Error>> {
    let max = client.abs_range_max();
    let home = shell.pointer()?;
    let low = sample(shell, client, geom::LOW, geom::LOW)?;
    let high = sample(shell, client, geom::HIGH, geom::HIGH)?;
    let mapping = geom::Mapping::solve(
        (geom::LOW, geom::LOW),
        low,
        (geom::HIGH, geom::HIGH),
        high,
        max,
    )?;
    let (hx, hy) = mapping.to_device(home.0, home.1);
    client.move_abs(hx, hy)?;
    Ok(mapping)
}

/// Inject one device coordinate and read back where the pointer landed.
///
/// Polling rather than sleeping a fixed interval: the compositor processes
/// the event when it processes it, and a sleep long enough to be safe is
/// three times longer than the wait usually is. The loop gives up quietly —
/// an unchanged reading is not an error here, it is a *sample*, and
/// [`geom::Mapping::solve`] is what refuses a fit made of two of them.
fn sample(
    shell: &shell::Shell,
    client: &mut Client,
    x: i32,
    y: i32,
) -> Result<(i32, i32), Box<dyn std::error::Error>> {
    let before = shell.pointer()?;
    client.move_abs(x, y)?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(100);
    loop {
        let now = shell.pointer()?;
        if now != before || std::time::Instant::now() > deadline {
            return Ok(now);
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

fn calibrate_cmd() -> Result<(), Box<dyn std::error::Error>> {
    let shell = shell::Shell::connect()?;
    let mut client = Client::connect()?;
    let t0 = std::time::Instant::now();
    let m = calibrate(&shell, &mut client)?;
    let elapsed = t0.elapsed();

    let (w, h) = m.span();
    let (ox, oy) = m.origin();
    println!("solved in {:.1} ms", elapsed.as_secs_f64() * 1000.0);
    println!("  x   stage = {:.6} * dev + {:.1}", m.x.m, m.x.c);
    println!("  y   stage = {:.6} * dev + {:.1}", m.y.m, m.y.c);
    println!();
    println!("0..{} covers  {:.0}x{:.0} px at {:.0},{:.0}", m.max, w, h, ox, oy);
    println!("             {:.2} device units per px horizontally", f64::from(m.max) / w);
    println!();
    println!("§3.5: if that rectangle is the union of your logical monitors, the");
    println!("device spans the desktop. If it is one output, the device is bound to");
    println!("that output and cross-output moves need one of the three escape");
    println!("hatches. muvor does not have to know which — it aims with the fit.");
    Ok(())
}

/// The loop (M5): focused window → labels → keystroke → validate → click.
///
/// Written as a command before it is a daemon, deliberately: every step is
/// visible and timed, so when the click lands somewhere unexpected the
/// question "which step was wrong" is already answered. The daemon is this
/// function with the hotkey signal in front of it and the connections held
/// open — which is also why the timings below separate startup from the
/// path: only the path is what §5.2 budgets.
/// Parse `hint`'s flags.
///
/// Split out so the **client and the daemon share one parser** (§2.5): the
/// client turns flags into a request, the daemon runs the same command from
/// it, and an argument cannot mean two things at the two ends.
pub fn parse_hint(args: &[&str]) -> Result<ipc::Request, Box<dyn std::error::Error>> {
    let mut after = 0u64;
    let mut dry_run = false;
    let mut explain = false;
    let mut measure = false;
    let mut deep = false;
    let mut i = 0;
    while i < args.len() {
        match args[i] {
            "--after" => {
                after = args.get(i + 1).ok_or("--after needs seconds")?.parse()?;
                i += 2;
            }
            "--dry-run" => {
                dry_run = true;
                i += 1;
            }
            "--explain" => {
                explain = true;
                i += 1;
            }
            "--measure" => {
                measure = true;
                dry_run = true;
                i += 1;
            }
            // D18: ask for the camera on a window the tree already explains.
            // Without it, tier 2 runs only when tier 1 found nothing at all.
            "--deep" => {
                deep = true;
                i += 1;
            }
            other => {
                return Err(format!(
                    "hint: unexpected argument {other}\n\
                     usage: muvor hint [--after SECONDS] [--dry-run] [--explain] [--measure] [--deep]"
                )
                .into())
            }
        }
    }
    Ok(ipc::Request::Hint { after, dry_run, explain, measure, deep })
}

/// Everything a hint needs that is not the hint (§2.5, M5s-b).
///
/// The one-shot builds one and throws it away; the daemon builds one and
/// keeps it. That is the whole of the difference between the two, and it is
/// why `hint` takes a session rather than making one — a function that opens
/// its own connections can only ever be a one-shot.
/// How far past the calibrated rectangle a window may reach before the fit
/// is presumed stale.
///
/// Not zero, and the reason is worth keeping: the fit is a pair of floats, so
/// a 32767-unit axis lands on 1079.997 rather than 1080, while a maximized
/// window's bottom edge is exactly 1080. An exact comparison therefore calls
/// **every** maximized window an outgrown desktop and re-measures on every
/// hint — which is precisely the pointer movement M5s-b exists to remove.
/// A real layout change moves an edge by hundreds of pixels, so this can be
/// generous without ever missing one. It matches the 8 px the a11y-versus-
/// compositor size check already allows (§4.3a).
const FIT_MARGIN: f64 = 8.0;

/// Has the desktop outgrown this fit?
///
/// The honest test available without asking the compositor a new question: a
/// window that extends past the calibrated rectangle proves the rectangle is
/// no longer the desktop — a monitor was plugged in, or the layout changed
/// under a process that outlives display arrangements.
fn outgrew(m: &geom::Mapping, win: &shell::Window) -> bool {
    let (w, h) = m.span();
    f64::from(win.x + win.w) > w + FIT_MARGIN || f64::from(win.y + win.h) > h + FIT_MARGIN
}

pub struct Session {
    shell: shell::Shell,
    det: muvor_atspi::Detector,
    client: Client,
    /// §3.5's fit. `None` until something needs it, then kept.
    ///
    /// **Measuring it moves the pointer three times.** In a one-shot that
    /// happened before every overlay and the user saw it; here it happens
    /// once and the pointer is still from then on.
    mapping: Option<geom::Mapping>,
    /// Hints served. `0` means the next one pays for the calibration.
    hints: u64,
}

impl Session {
    pub fn open() -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self {
            shell: shell::Shell::connect()?,
            det: muvor_atspi::Detector::connect()?,
            client: Client::connect()?,
            mapping: None,
            hints: 0,
        })
    }

    /// The pixel → device fit, measured on first use and re-measured only if
    /// the desktop has outgrown it.
    ///
    /// The staleness test is the honest one available without asking the
    /// compositor a new question: a window that extends past the calibrated
    /// rectangle proves the rectangle is no longer the desktop — a monitor
    /// was plugged in, or the layout changed under us. §3.5 says the fit is
    /// measured and never assumed; this keeps that true for a process that
    /// outlives a display arrangement.
    fn mapping_for(
        &mut self,
        win: &shell::Window,
    ) -> Result<(geom::Mapping, bool), Box<dyn std::error::Error>> {
        let stale = self.mapping.as_ref().is_some_and(|m| outgrew(m, win));
        if stale {
            self.mapping = None;
        }
        match self.mapping {
            Some(m) => Ok((m, false)),
            None => {
                let m = calibrate(&self.shell, &mut self.client)?;
                self.mapping = Some(m);
                Ok((m, true))
            }
        }
    }
}

/// What the daemon is holding (§2.5, M5s-c).
pub fn status(session: &mut Session, out: Out<'_>) -> Result<(), Box<dyn std::error::Error>> {
    say!(out, "hints served     {}", session.hints);
    say!(
        out,
        "calibration      {}",
        match session.mapping {
            Some(m) => {
                let (w, h) = m.span();
                format!("{w:.0}x{h:.0} px, measured once this session")
            }
            None => "not measured yet — the next hint pays for it".to_owned(),
        }
    );

    let stats = session.det.mirror()?;

    // Age is not the question. A tree warmed an hour ago and still being fed
    // events is fine; one warmed a minute ago whose stream has died is not,
    // and until M5s-d the two printed the same.
    //
    // **What this cannot do is tell a dead subscription from a still
    // desktop**, and the first version of this line claimed it could — it
    // printed "the stream has stopped" after thirty seconds of nobody
    // touching the machine, which was simply false. muvor subscribes
    // narrowly (window:activate, cache add/remove, state-changed), so an
    // idle desktop really does emit nothing. So the report is the fact —
    // *how long since the last event* — and the interpretation is left to
    // the reader, who knows whether they have been using the computer.
    const QUIET_AFTER: std::time::Duration = std::time::Duration::from_secs(30);

    say!(out, "mirror           {} events seen", stats.value.seen);
    match stats.value.quiet {
        None if stats.value.seen == 0 => {
            // Two causes, and this cannot tell them apart from inside: a
            // desktop nobody has touched since the daemon started emits
            // nothing either. `muvor check` reads the announce flag, which
            // is the half that is worth ruling out first.
            say!(out, "  NO EVENTS      nothing has arrived yet. If windows have been used since");
            say!(out, "                 this daemon started, §4.7a's announce flag is the cause —");
            say!(out, "                 `muvor check` reads it. On an idle desktop this is normal");
        }
        Some(q) if q > QUIET_AFTER => {
            say!(out, "  QUIET          no events for {:.0}s. If you have been using the desktop,", q.as_secs_f64());
            say!(out, "                 the subscription has died and every tree below is a");
            say!(out, "                 snapshot; if you have not, this is what idle looks like");
        }
        Some(q) => say!(out, "  live           last event {:.1}s ago", q.as_secs_f64()),
        None => {}
    }
    if stats.value.apps.is_empty() {
        say!(out, "  EMPTY          nothing has been warmed — no window:activate has arrived,");
        say!(out, "                 or every app that activated answers Collection (§4.2-iii)");
    }
    for a in &stats.value.apps {
        say!(
            out,
            "  {:<10} {:<7} {:>5} nodes  {:>4} updates  warmed {:.1}s ago{}",
            a.bus_name,
            format!("{:?}", a.source),
            a.nodes,
            a.updates,
            a.age.as_secs_f64(),
            match a.quiet {
                None => "  — never updated".to_owned(),
                Some(q) if q > QUIET_AFTER => format!("  — quiet {:.0}s", q.as_secs_f64()),
                Some(q) => format!("  — updated {:.1}s ago", q.as_secs_f64()),
            }
        );
    }
    Ok(())
}

fn hint(
    args: &[&str],
    session: &mut Session,
    out: Out<'_>,
) -> Result<(), Box<dyn std::error::Error>> {
    let ipc::Request::Hint { after, dry_run, explain, measure, deep } = parse_hint(args)? else {
        unreachable!("parse_hint returns a Hint")
    };

    if after > 0 {
        say!(out, "waiting {after}s — focus the window you want hinted");
        std::thread::sleep(std::time::Duration::from_secs(after));
    }

    // --- the path §5.2 budgets -----------------------------------------
    let t_start = std::time::Instant::now();
    let win = session.shell.focused_window()?;
    let t_focus = t_start.elapsed();
    if win.is_none() {
        return Err("no focused window — the compositor reports nothing focused".into());
    }

    // Calibration sits *after* the clock starts and *inside* the reported
    // time, because on a cold session it is genuinely part of what the user
    // waited for. On every hint after the first it costs nothing and the
    // pointer does not move — which is the difference M5s-b exists to make,
    // and hiding it above the clock would hide exactly that.
    session.hints += 1;
    let (mapping, measured) = session.mapping_for(&win)?;
    let (span_w, span_h) = mapping.span();
    say!(
        out,
        "calibrated       0..{} covers {span_w:.0}x{span_h:.0} px   {}",
        mapping.max,
        if measured {
            "measured now — the pointer moved".to_owned()
        } else {
            // Whether the daemon is actually being reused is otherwise
            // invisible, and it is the one thing M5s-b changes.
            format!("kept — hint {} on this session, pointer still", session.hints)
        }
    );

    // --- every window that can be seen, not just the focused one ------
    //
    // §4.3 hinted the focused window alone and gave a correctness reason for
    // it: nothing in the accessibility tree can tell you a window is
    // covered, so labelling a buried node produces a badge that clicks
    // whatever is on top. It also named what would lift the restriction —
    // the compositor's stacking order — and extension v8 supplies it. The
    // rule "if I can see it, it is hinted" is now the rule muvor follows.
    let t_frame = std::time::Instant::now();
    let screen = Rect::new(0, 0, span_w as i32, span_h as i32);
    let panes = panes(&session.shell, &session.det, screen, &mut *out)?;
    let t_frame = t_frame.elapsed();
    if panes.is_empty() {
        return Err(format!(
            "no window on this workspace has an accessibility tree.\n  The focused one is \
             {} '{}', pid {} — this is D12's case: some windows expose nothing (a game, a \
             canvas,\n  an Electron app started without --force-renderer-accessibility). Try \
             `muvor hint --deep`,\n  which asks the camera instead (D18), or `muvor free` to \
             drive the pointer by hand.",
            win.wm_class, win.title, win.pid
        )
        .into());
    }

    for pane in &panes {
        say!(out,
            "window           {} — {}  pid {}  [{}]{}",
            pane.frame.app, pane.frame.title, pane.win.pid, pane.win.wm_class,
            if pane.win.title == win.title { "  (focused)" } else { "" }
        );
        say!(out, "  compositor     {},{} {}x{}", pane.win.x, pane.win.y, pane.win.w, pane.win.h);
        say!(out, "  a11y frame     {}   matched by {}{}",
            pane.frame.bounds,
            pane.frame.focus.as_str(),
            if pane.frame.pid_memo {
                ", pid memo hit"
            } else if pane.frame.title_memo {
                ", title memo hit"
            } else {
                ", bus walked"
            }
        );
        if pane.visible < 1.0 {
            // The number that explains a short overlay. Without it, "muvor
            // did not label that" and "muvor labelled it and you cannot see
            // the badge" look identical from the desk (§2.5).
            say!(out, "  visible        {:.0}% — {} window{} on top",
                pane.visible * 100.0, pane.above.len(),
                if pane.above.len() == 1 { "" } else { "s" });
        }
        if pane.frame.focus == muvor_atspi::FocusSource::Named {
            say!(out, "  NOTE           the pid matched no application on the a11y bus, so this");
            say!(out, "                 window was found by title alone — a guess, not identity.");
        }
        let (aw, ah) = pane.win.a11y_size();
        if (pane.frame.bounds.w - aw).abs() > 8 || (pane.frame.bounds.h - ah).abs() > 8 {
            // Against the BUFFER size since v7 (§5.1h). This used to fire on
            // every window with a client-side shadow — the a11y frame is the
            // buffer and it was being compared to the frame rect, which is
            // the buffer minus the shadow. A warning that fires on the
            // normal case is a warning nobody reads, and underneath it was a
            // real 26 px error in the origin.
            say!(out,
                "  MISMATCH       a11y says {}x{}, the compositor's buffer says {aw}x{ah} — the",
                pane.frame.bounds.w, pane.frame.bounds.h
            );
            say!(out, "                 origin correction may be off by the difference");
        }
    }

    // The wall clock above covers both, so the two below are what it was
    // spent ON — they sum to less than `t_windows`, and the difference is the
    // `Windows` round trip itself.
    let t_a11y: std::time::Duration = panes.iter().map(|p| p.t_frame).sum();
    let t_detect: std::time::Duration = panes.iter().map(|p| p.t_detect).sum();
    let s_targets: usize = panes.iter().map(|p| p.scan.targets.len()).sum();
    let provenance = panes
        .first()
        .map(|p| p.scan.provenance.as_str())
        .unwrap_or("none");

    let mut asm = assemble(deep, &panes, &session.shell, (span_w, span_h), mapping.origin().0, &mut *out)?;
    let (mut aims, mut cv) = (std::mem::take(&mut asm.aims), std::mem::take(&mut asm.cv));
    let mut cv_pane = std::mem::take(&mut asm.cv_pane);
    let (mut labels, mut placements) = (asm.labels.clone(), std::mem::take(&mut asm.placements));
    let mut hints = std::mem::take(&mut asm.hints);
    let (go_deep, t_deep, t_label) = (asm.deep, asm.t_deep, asm.t_label);
    let t_draw = std::time::Instant::now();
    session.shell.show(&hints)?;
    let t_draw = t_draw.elapsed();
    let to_labels = t_start.elapsed();

    // **The keyboard is one stream from here to the end of movement mode.**
    //
    // `Shell::wait` builds a `MessageStream` per call and drops it on the
    // way out, and a stream that is not open does not receive: every signal
    // the extension emits between two calls is gone. That was invisible
    // while the only signal was `Typed` — nothing is emitted between one
    // wait and the label — and it broke the hold gesture, because a hold
    // hands the keyboard over *in the same call that emits `TypedHold`*
    // (extension `_resolve`), so `KeyDown`/`KeyUp` start arriving while
    // muvor is still in §4.5's validation, §4.5b's capture and the
    // `MOVE_ABS`. All of it fell on the floor. Opening the stream once, here,
    // and carrying it through the Tab loop and into `movement_mode` means
    // the D-Bus queue holds those keys until the loop is ready to read them.
    //
    // It is opened *after* `to_labels` is taken so §5.2's number keeps
    // measuring what §5.2 budgets, and before anything a human could react
    // to — one `AddMatch` round trip against an overlay that still has to
    // paint.
    let mut keys = session.shell.keys()?;

    say!(out);
    say!(out, "timings (ms)     — §5.2 budgets 50, targets 15");
    say!(out, "  focus          {:>6.2}   FocusedWindow", ms(t_focus));
    say!(out, "  windows        {:>6.2}   Windows + a11y + detect, {} visible pane{}",
        ms(t_frame), panes.len(), if panes.len() == 1 { "" } else { "s" });
    say!(out, "    frame        {:>6.2}   compositor -> a11y, summed over the panes", ms(t_a11y));
    say!(out, "    detect       {:>6.2}   {} tree target{} via {}", ms(t_detect), s_targets,
        if s_targets == 1 { "" } else { "s" }, provenance);
    say!(out, 
        "  label          {:>6.2}   {} deep, {} columns x {} ranks",
        ms(t_label),
        labels.label_len(),
        labels.alphabet().len(),
        labels.ranks().len()
    );
    say!(out, "  draw           {:>6.2}   Show", ms(t_draw));
    if go_deep {
        // Printed separately and *inside* the total, because it is the one
        // stage a user can choose not to pay for (D18) and hiding it in the
        // total would make that choice invisible.
        say!(out, "  deep           {:>6.2}   capture + decode + detect, {} CV target{}", ms(t_deep), cv.len(),
            if cv.len() == 1 { "" } else { "s" });
    }
    say!(out, "  ------------------------");
    say!(out, 
        "  to labels      {:>6.2}   {}",
        ms(to_labels),
        if ms(to_labels) <= 15.0 {
            "inside the target"
        } else if ms(to_labels) <= 50.0 {
            "inside the budget, over the target"
        } else {
            "OVER BUDGET"
        }
    );
    // Everything §5.2 budgets has now been printed, and none of it needed a
    // human. `--measure` stops here so the path can be sampled repeatedly
    // without costing somebody ten seconds of their keyboard each time —
    // §2.5's "instrument rather than repeat", made cheap enough to obey.
    if measure {
        session.shell.hide().ok();
        say!(out);
        say!(out, "--measure — overlay down, nothing typed, nothing clicked");
        return Ok(());
    }

    say!(out);
    say!(out, "type a label, or Escape. (The overlay takes itself down after 10s.)");

    let typed_at = std::time::Instant::now();
    // D18's tier 2, asked for at the right moment: the user has looked at
    // the overlay, not found what they wanted, and pressed Tab. muvor goes
    // to the camera and draws again over the grab it never dropped.
    //
    // A loop rather than a recursion because only one thing can repeat —
    // deepening — and it can only happen once: `go_deep` is already true
    // the second time round, so a second Tab redraws the same set rather
    // than paying for another capture.
    let (label, held) = loop {
        match keys.next(std::time::Duration::from_secs(30))? {
        shell::Outcome::Deepen => {
            if go_deep {
                say!(out, "Tab — already deep, nothing more to add");
                continue;
            }
            let t = std::time::Instant::now();
            let mut again =
                assemble(true, &panes, &session.shell, (span_w, span_h), mapping.origin().0, &mut *out)?;
            aims = std::mem::take(&mut again.aims);
            cv = std::mem::take(&mut again.cv);
            cv_pane = std::mem::take(&mut again.cv_pane);
            labels = again.labels.clone();
            placements = std::mem::take(&mut again.placements);
            hints = std::mem::take(&mut again.hints);
            session.shell.redraw(&hints)?;
            say!(out, "Tab — tier 2 in {:.2} ms, {} targets now", ms(t.elapsed()), hints.len());
            continue;
        }
        // Keys arriving at the label overlay. The extension only sends
        // these under its own key grab, so this is the two grabs having got
        // out of step — reported rather than acted on.
        shell::Outcome::KeyDown(k) | shell::Outcome::KeyUp(k) | shell::Outcome::KeyRepeat(k) => {
            say!(out, "unexpected key '{k}' from a key grab that is not this one");
            continue;
        }
        shell::Outcome::Typed(l) => break (l, false),
        // §12.3: the label's last key was held, or the ' prefix armed it.
        // The shell is already holding the keyboard — it handed it over in
        // the same call rather than dropping it, because a gap where nobody
        // holds a key that is still down leaks its release into the
        // application. Everything below is unchanged: §4.5 clears the
        // *target*, and a target cleared for a click is cleared for a
        // pointer that is about to sit on it.
        shell::Outcome::TypedHold(l) => break (l, true),
        shell::Outcome::Cancelled => {
            say!(out, "cancelled — nothing clicked");
            return Ok(());
        }
        shell::Outcome::TimedOut => {
            session.shell.hide().ok();
            say!(out, "nothing typed — overlay taken down, nothing clicked");
            return Ok(());
        }
    }
    };
    let human = typed_at.elapsed();

    let Some(index) = labels.target_of(&label) else {
        // The shell only completes labels it was given, so this is muvor and
        // the overlay disagreeing about the label set — a bug, not a miss.
        return Err(format!("the overlay reported '{label}', which is not a label muvor drew").into());
    };
    let spot = placements[index];
    say!(out);
    say!(out, "typed            {label}  after {:.0} ms", ms(human));

    // Two kinds of target and therefore two validations — §4.5 through the
    // bus, §4.5b through the camera. The rule is the same in both: re-observe
    // through the channel that found it, and refuse on any disagreement.
    let (sx, sy) = match aims[index] {
        Aim::Tree(pi, i) => {
            // The pane this target came from, and therefore the frame it is
            // validated against. With one window on the overlay these were
            // the same thing; with the stack on it, verifying a Nautilus
            // icon against the terminal's tree would refuse every click and
            // look exactly like §4.5 working.
            let pane = &panes[pi];
            let target = &pane.scan.targets[i];
            let (wx, wy) = pane.win.origin();
            // `place_all` worked in screen space, and a `Claim` is
            // window-relative (D13) — the same 32 px that made M5's click
            // land, in reverse.
            let claim = Claim::at(
                target,
                spot.click_x - wx + pane.frame.bounds.x,
                spot.click_y - wy + pane.frame.bounds.y,
            );
            say!(out, "target           {} '{}'  at {}  in {}", target.role, target.name,
                target.bounds, pane.win.wm_class);
            if explain {
                say!(out,
                    "  badge          {:?} of it — the click is the target's centre (§5.1a)",
                    spot.side
                );
            }

            // --- §4.5: the last place a wrong click can be stopped -------
            let checked = session.det.verify(&pane.frame, &claim)?;
            let v = &checked.value;
            say!(out, "validate         {:>6.2} ms  {}", checked.ms(), v.verdict.why());
            if explain {
                for (depth, step) in v.chain.iter().enumerate() {
                    say!(out, "  {:width$}{step}", "", width = depth * 2 + 2);
                }
            }
            if !v.verdict.is_ok() {
                say!(out);
                say!(out, "REFUSED — nothing was clicked.");
                say!(out, "A refused click is annoying; a wrong click is unrecoverable (§4.5).");
                return Ok(());
            }
            // Window-relative to screen (D13), against this pane's own
              // origin. The a11y frame's origin is subtracted rather than
              // assumed to be zero: it is near-zero on every toolkit
              // measured, and "near" is exactly the assumption that puts a
              // click two pixels outside a button on the one that is not.
            let (sx, sy) = (
                wx + claim.x - pane.frame.bounds.x,
                wy + claim.y - pane.frame.bounds.y,
            );
            say!(out,
                "click            {},{} window-relative -> {sx},{sy} screen",
                claim.x, claim.y
            );
            (sx, sy)
        }
        Aim::Cv(i) => {
            let claim = &cv[i];
            say!(out, "target           found by the camera, not the tree — at {},{} {}x{}",
                claim.rect.x, claim.rect.y, claim.rect.w, claim.rect.h);
            if explain {
                say!(out, "  badge          {:?} of it (§5.1a)", spot.side);
                say!(out, "  no name        a CV target has a rectangle and pixels, and no name;");
                say!(out, "                 that is the identity §4.5b has to work with");
            }

            // --- §4.5b: the same rule, through the camera ----------------
            //
            // The overlay must come down first. `Capture()` photographs the
            // stage as composited, badges included (§5.1c) — the trick that
            // verified the cyan dot works against us here, and a frame with
            // muvor's own overlay in it would fingerprint as changed.
            session.shell.hide().ok();
            let t = std::time::Instant::now();
            let pane = &panes[cv_pane[i]];
            let verdict =
                revalidate_cv(&session.shell, &session.det, claim, &pane.win, &pane.above)?;
            say!(out, "validate         {:>6.2} ms  {}", ms(t.elapsed()), verdict.why());
            if !verdict.is_ok() {
                say!(out);
                say!(out, "REFUSED — nothing was clicked.");
                say!(out, "A refused click is annoying; a wrong click is unrecoverable (§4.5b).");
                return Ok(());
            }
            // **The capture cost us the keyboard, and this takes it back.**
            //
            // `hide()` above put the overlay down so the frame would not
            // fingerprint muvor's own badges (§5.1c) — and `Hide` is
            // `_teardown`, which gives the key focus back *unconditionally*,
            // because a keyboard typing into a dead actor is the one failure
            // v8 must never leave behind. On a hold that unconditional
            // release throws away the focus the overlay just handed over,
            // and `movement_mode` then listens to a stream nothing can ever
            // feed: every `hjkl` goes to the application instead. The tree
            // branch above never hides, so it never had the fault — which is
            // why this only ever showed up after Tab.
            //
            // `GrabKeys` is `_enterKeyGrab` cold, which is the case it was
            // written for, so this costs no logout. It goes *after* the
            // refusal check on purpose: a refused hold must not leave the
            // keyboard taken.
            if held {
                session.shell.grab_keys()?;
            }
            let (sx, sy) = claim.click;
            say!(out, "click            {sx},{sy} screen");
            (sx, sy)
        }
    };
    let (dx, dy) = mapping.to_device(sx, sy);
    say!(out, "                 -> {dx},{dy} device");
    if dry_run {
        say!(out, "--dry-run — not injected");
        if held {
            say!(out, "                 (movement mode would have started here)");
        }
        return Ok(());
    }
    if held {
        // The pointer goes there and stops. Nothing is clicked — that is the
        // whole difference, and it is why the gesture is worth having: the
        // prefix grammar this replaced committed to a button before the user
        // could see where the label had actually landed (§12).
        session.client.move_abs(dx, dy)?;
        say!(out);
        // The label's last key is **still down** — that is what a hold is —
        // and the shell handed the keyboard over without dropping it, so its
        // release lands inside the mode. Every letter of §5.3c's alphabet is
        // also one of §12's keys, so that release is an action: measured
        // 2026-08-26 in the playtest harness, a hold on label `da` entered
        // movement mode and the release of `a` **middle-clicked the target**,
        // which is a paste or a new tab, not a no-op. The mode is told which
        // key it was entered with and swallows exactly one release of it.
        let entered_with = label.chars().last().and_then(|c| {
            let mut buf = [0u8; 4];
            muvor_core::motion::Key::from_name(c.encode_utf8(&mut buf))
        });
        return movement_mode(
            &session.shell,
            &mut session.client,
            &mut keys,
            &mapping,
            (sx, sy),
            entered_with,
            out,
        );
    }
    let t_click = std::time::Instant::now();
    session.client.click_at(dx, dy, Button::Left)?;
    say!(out, "injected         {:>6.2} ms  BATCH move+press+release, one token", ms(t_click.elapsed()));
    Ok(())
}

/// M7's movement mode, driven from the terminal (§12, and §12.11 step 4).
///
/// **The keyboard this wants is the one the extension is holding**, and the
/// extension cannot give it yet: movement mode needs key *release* — `s` and
/// `f` are held — and the hint overlay's accelerators report activation only
/// (§12.3). That is extension v7 and a logout. Until then this drives the
/// same state machine from a replayed sequence, which is the same bargain
/// `muvor free` made and for the same reason: everything except key delivery
/// is exercised here, so the shell half stays a small delta rather than a
/// leap of faith.
///
/// The sequence syntax exists because the machine is press/release and a
/// terminal cannot hold a key down. `l+` is a press, `l-` a release, a bare
/// `l` is a tap (press then release), `.` is one tick and `.N` is N of them.
/// Whitespace separates tokens.
fn motion_cmd(args: &[&str]) -> Result<(), Box<dyn std::error::Error>> {
    use muvor_core::motion::{Button as MButton, Cmd, Ev, Key, Motion, Params};

    let mut keys = None;
    let mut dry_run = false;
    let mut at: Option<(i32, i32)> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i] {
            "--keys" => {
                keys = Some(*args.get(i + 1).ok_or("motion: --keys needs a sequence")?);
                i += 2;
            }
            "--dry-run" => {
                dry_run = true;
                i += 1;
            }
            "--at" => {
                let v = args.get(i + 1).ok_or("motion: --at needs <x>,<y>")?;
                let (x, y) = v.split_once(',').ok_or("motion: --at needs <x>,<y>")?;
                at = Some((x.trim().parse()?, y.trim().parse()?));
                i += 2;
            }
            other => return Err(format!("motion: unexpected argument {other}").into()),
        }
    }

    let shell = shell::Shell::connect()?;
    let mut client = Client::connect()?;
    let mapping = calibrate(&shell, &mut client)?;
    let (span_w, span_h) = mapping.span();
    let (ox, oy) = mapping.origin();
    // Rounded, not truncated — §4.5c's clamp bug: the span is a measurement
    // and arrives as 1919.6, and truncating puts the right-hand clamp one
    // pixel inside the screen, which is exactly where an edge target lives.
    #[allow(clippy::cast_possible_truncation)]
    let bounds = Rect::new(
        ox.round() as i32,
        oy.round() as i32,
        span_w.round() as i32,
        span_h.round() as i32,
    );

    let start = match at {
        Some(p) => p,
        None => shell.pointer()?,
    };
    let params = Params::default();
    let mut motion = Motion::new(start, bounds, params);

    // `--at` means *put it there*, not "pretend it is there". Under the real
    // grab the label has already moved the pointer, and a replay that only
    // moved its own idea of the position would send scroll notches to
    // whatever window the pointer happened to be sitting over — which is how
    // the first scroll test went to the wrong window.
    if at.is_some() && !dry_run {
        let (dx, dy) = mapping.to_device(start.0, start.1);
        client.move_abs(dx, dy)?;
    }

    println!("movement mode   pointer at {},{}  screen {}x{} at {ox:.0},{oy:.0}",
        start.0, start.1, bounds.w, bounds.h);
    #[allow(clippy::cast_precision_loss)]
    let hz = 1.0 / params.tick.as_secs_f32();
    println!("  hjkl          left / down / up / right — {} px a tick, {hz:.0} ticks a second",
        params.slow_step);
    println!("  s             held: fast, ramping to {:.1} px a tick ({:.0} px/s)",
        params.fast_cap, params.fast_cap * hz);
    println!("  f             the left button itself — tap to click, hold to drag or select");
    println!("  a             the wheel itself — tap to wheel-click, hold and turn with hjkl");
    println!("  d g r         double, triple, right");
    println!("  Tab / RAlt    leave");
    if dry_run {
        println!("  --dry-run     nothing is injected");
    }

    // uictl's Button, which muvor-core deliberately does not know about: the
    // pure crate has no dependencies and that is what makes §6's ports ports.
    let wire_button = |b: MButton| match b {
        MButton::Left => Button::Left,
        MButton::Middle => Button::Middle,
        MButton::Right => Button::Right,
    };

    // uictl releases a button held longer than HOLD_MAX_SEC = 30 (§3.4), so
    // muvor lets go first and says so. Left at 30 it would be a race with
    // the broker over who owns the button.
    const HOLD_SAFE: std::time::Duration = std::time::Duration::from_secs(25);
    let mut drag_since: Option<std::time::Instant> = None;

    let mut moves = 0u32;
    // `pos` is passed rather than read from the machine, because the
    // machine is being mutated by the caller in the same expression.
    let apply = |cmds: Vec<Cmd>, pos: (i32, i32), client: &mut Client, moves: &mut u32|
     -> Result<bool, Box<dyn std::error::Error>> {
        for cmd in cmds {
            match cmd {
                Cmd::MoveTo(x, y) => {
                    let (dx, dy) = mapping.to_device(x, y);
                    if !dry_run {
                        client.move_abs(dx, dy)?;
                    }
                    *moves += 1;
                }
                Cmd::Press(b) => {
                    println!("  press   {b:?} at {},{}", pos.0, pos.1);
                    if !dry_run {
                        // Move first, then hold the button down: a drag that
                        // begins where the pointer was two commands ago is
                        // the classic off-by-one-gesture bug.
                        let (dx, dy) = mapping.to_device(pos.0, pos.1);
                        client.move_abs(dx, dy)?;
                        client.press(wire_button(b))?;
                    }
                }
                Cmd::Release(b) => {
                    println!("  release {b:?} at {},{}", pos.0, pos.1);
                    if !dry_run {
                        client.release(wire_button(b))?;
                    }
                }
                Cmd::Click { button, times } => {
                    let (x, y) = pos;
                    println!("  click   {button:?} x{times} at {x},{y}");
                    if !dry_run {
                        let (dx, dy) = mapping.to_device(x, y);
                        let code = wire_button(button);
                        // Not one batch (§12.8, settled 2026-08-24): a batch
                        // is one SYN_REPORT, so all of it shares a timestamp,
                        // and a toolkit decides "double" by the interval
                        // between two clicks. N frames, MULTI_CLICK_GAP
                        // apart, is what a hand does — so `d` opens a file
                        // and selects a word, and `g` selects a line,
                        // because the application's own handler is what
                        // does that and it only ever needed to be told the
                        // truth about the count.
                        for n in 0..times {
                            if n > 0 {
                                std::thread::sleep(MULTI_CLICK_GAP);
                            }
                            client.click_at(dx, dy, code)?;
                        }
                    }
                }
                Cmd::Scroll { v, h } => {
                    println!("  scroll  v{v:+} h{h:+} at {},{}", pos.0, pos.1);
                    if !dry_run {
                        client.scroll(v, h)?;
                    }
                }
                Cmd::Leave => {
                    println!("  leave");
                    return Ok(false);
                }
            }
        }
        Ok(true)
    };

    let Some(seq) = keys else {
        // No sequence: this is the real thing. The extension takes the
        // keyboard and forwards every key by name, and the same state
        // machine runs against it — which is the whole point of having built
        // it behind `--keys` first.
        if dry_run {
            return Err("motion: --dry-run needs --keys; under the grab there is nothing to \n\
                        replay and the pointer is the output"
                .into());
        }
        let mut session = Session::open()?;
        let (dx, dy) = mapping.to_device(start.0, start.1);
        session.client.move_abs(dx, dy)?;
        // Before the grab, not after: `GrabKeys` returns once the overlay
        // holds the key focus, and from that instant every key is a signal.
        // A stream opened afterwards would miss whatever was pressed in
        // between — which on this path is the first key of a user who was
        // already reaching for `l`.
        let mut keys = session.shell.keys()?;
        session.shell.grab_keys()?;
        println!("  (grabbed — the keyboard is muvor's until Tab, right Alt or Escape)");
        let mut stdout = std::io::stdout();
        // Nothing was held to get here: `muvor motion` is entered from a
        // command line, not from a label.
        return movement_mode(
            &session.shell,
            &mut session.client,
            &mut keys,
            &mapping,
            start,
            None,
            &mut stdout,
        );
    };

    for token in seq.split_whitespace() {
        let cmds = if let Some(n) = token.strip_prefix('.') {
            // `.` is one tick, `.90` is ninety — a second of held key at
            // 90 Hz. Ticks are what a held key produces, and they are the
            // only part of this a terminal cannot generate by itself.
            let n: u32 = if n.is_empty() { 1 } else { n.parse()? };
            let mut out = Vec::new();
            for _ in 0..n {
                out.extend(motion.event(Ev::Tick));
            }
            out
        } else {
            let (name, ev): (&str, fn(Key) -> Ev) = if let Some(k) = token.strip_suffix('+') {
                (k, Ev::Down)
            } else if let Some(k) = token.strip_suffix('-') {
                (k, Ev::Up)
            } else {
                // A bare token is a tap: press and release, which is what a
                // key that is not held actually is.
                let key = Key::from_name(token)
                    .ok_or_else(|| format!("motion: {token} is not a key movement mode knows"))?;
                let mut out = motion.event(Ev::Down(key));
                out.extend(motion.event(Ev::Up(key)));
                let pos = motion.pos();
                if !apply(out, pos, &mut client, &mut moves)? {
                    break;
                }
                continue;
            };
            let key = Key::from_name(name)
                .ok_or_else(|| format!("motion: {name} is not a key movement mode knows"))?;
            motion.event(ev(key))
        };
        // Outside the closure on purpose: the guard below reads this, and a
        // closure that captured it mutably would own it for the whole loop.
        for cmd in &cmds {
            match cmd {
                Cmd::Press(_) => drag_since = Some(std::time::Instant::now()),
                Cmd::Release(_) => drag_since = None,
                _ => {}
            }
        }
        let pos = motion.pos();
        if !apply(cmds, pos, &mut client, &mut moves)? {
            break;
        }
        if drag_since.is_some_and(|t| t.elapsed() > HOLD_SAFE) {
            drag_since = None;
            println!("  drag held {}s — releasing before uictl's dead-man does (§3.4)",
                HOLD_SAFE.as_secs());
            let cmds = motion.release_drag();
            let pos = motion.pos();
            apply(cmds, pos, &mut client, &mut moves)?;
        }
    }

    // The mode is over however the sequence ended, and leaving is the only
    // path that can release a held button (§12.9). A sequence that stops
    // mid-drag must not leave the left button down on a live desktop.
    let tail = motion.leave();
    let pos = motion.pos();
    apply(tail, pos, &mut client, &mut moves)?;

    let (x, y) = motion.pos();
    println!("\n  {moves} move(s), pointer at {x},{y}");
    Ok(())
}

/// Movement mode under the real grab (§12).
///
/// **Entered from a label whose last key was held**, with the pointer already
/// on the target and nothing clicked. From here `hjkl` corrects the position,
/// `s` sweeps, and the left hand carries every button.
///
/// **One loop, and the tick is the wait's timeout.** The machine says how
/// long it wants to sleep — 33 ms while a direction is held, half a minute
/// while it is not — and a timeout *is* the tick. That is why there is no
/// timer thread and no channel: the two things that can happen, a key and a
/// tick, arrive through one call.
///
/// **An idle mode costs nothing.** `wants_tick` returns `None` with no
/// direction held, so a user thinking about where to go spends no requests
/// at all — which matters, because §3.4's budget is 50 a second and the
/// moving case already spends 30 of them.
/// Was this uictl saying "too fast", rather than something the mode should
/// die of? §3.4's limiter answers `ERR_RATE_LIMITED` on the frame it drops,
/// and a dropped pointer frame is a stutter, not a fault.
fn is_uictl_refusal(e: &(dyn std::error::Error + 'static)) -> bool {
    matches!(
        e.downcast_ref::<muvor_uictl::Error>(),
        Some(muvor_uictl::Error::Refused { .. })
    )
}

/// `entered_with` is the key that is still down because the user is holding
/// it — the second key of a label (§12.13). Its release belongs to the
/// gesture that started the mode, not to the mode, and is swallowed once.
///
/// **`keys` is opened by the caller and is never opened here**, and that is
/// a fix rather than a preference. The extension switches to `keys` mode in
/// the same call that emits `TypedHold`, so it starts forwarding the moment
/// the label completes — while muvor is still validating the target,
/// possibly photographing the screen for §4.5b, and moving the pointer. A
/// `MessageStream` only receives what arrives while it is open, so a stream
/// built *here* misses everything in that window: the held key's own
/// release, and any `f d g a r` pressed in the first tenth of a second.
///
/// The release is the load-bearing one. `swallow` is cleared by that key's
/// release **and by nothing else** (see below), so losing it makes that one
/// action key dead for the whole of the mode — which from the desk is
/// "`f` does nothing, and `hjkl` are fine".
/// When each *repeating* key last said it was still down.
///
/// **The keyboard is the only witness to a key nobody presses twice.** A tap
/// whose release goes missing repairs itself the next time the key is
/// pressed ([`muvor_core::motion::Motion::repress`]); a key that is *held* —
/// a sweep, `f` halfway through a drag — is never pressed again, so a lost
/// release would leave it down for the rest of the mode with nothing to
/// contradict it. What contradicts it is silence: extension v10 reports
/// autorepeat as [`shell::Outcome::KeyRepeat`], so a key that was repeating
/// and stops has come up.
///
/// Only keys that have actually repeated are watched, and the deadline is
/// measured from their own cadence rather than assumed — the repeat interval
/// is the user's setting and an accessibility profile can make it very slow.
/// A key held for less than the repeat *delay* has no heartbeat and is not
/// watched at all, which is correct: its release is still owed and still
/// expected.
///
/// **A keyboard repeats one key: the last one pressed.** That is the fact the
/// first cut of this got wrong (§12.17), and getting it wrong is what made
/// `a`-and-`j` — the wheel — stop working. Hold `a` past the repeat delay and
/// it repeats; press `j` and the keyboard hands the repeat to `j` and `a` goes
/// quiet *while it is still held down*. This watched every key that had ever
/// repeated, so a quarter of a second later it declared `a` up, the wheel
/// switched off, and `j` went back to moving the pointer. `s`-then-`l` lost
/// its sweep the same way and `f`-then-`l` dropped its drag mid-selection.
///
/// So there is one slot, not a list, and it holds whichever key the keyboard
/// is repeating now. Silence from *that* key is evidence. Silence from any
/// other key is not evidence of anything, because a key that is not the
/// repeat owner has nothing to say either way — which is why a press of a
/// new key empties the slot rather than adding to it.
///
/// **What that costs**, stated plainly: a lost release of `f` while `l` is
/// being swept is no longer caught here. It cannot be — the keyboard emits
/// nothing about `f` for as long as `l` owns the repeat. It falls back to
/// `HOLD_SAFE`, the 25-second release before uictl's dead-man (§12.9), and
/// to the unconditional release on the way out of the mode. One key held
/// alone — a drag with no direction, a sweep, a wheel — is still covered,
/// and that is the case §12.16 was written for.
#[derive(Default)]
struct Heartbeats {
    /// The key the keyboard is repeating, when it last said so, and the
    /// interval between its last two repeats once there have been two.
    owner: Option<(muvor_core::motion::Key, std::time::Instant, Option<std::time::Duration>)>,
}

impl Heartbeats {
    /// Five intervals of silence is a key that came up. Measured 2026-08-27:
    /// a held `l` repeated every 40 ms, so this is 200 ms in the ordinary
    /// case — long enough that a missed repeat is not a released key.
    const MULTIPLE: u32 = 5;
    /// Never sooner than this, whatever the cadence says.
    const FLOOR: std::time::Duration = std::time::Duration::from_millis(250);
    /// And never later. Also the deadline before a *second* repeat has
    /// arrived and there is a cadence to measure at all.
    const CEILING: std::time::Duration = std::time::Duration::from_secs(3);

    /// Whether anything is being watched at all.
    ///
    /// **The loop sleeps on `Motion`'s clock, and `Motion` has no clock while
    /// only `f` is down.** A drag with no direction held wants no tick
    /// (`Motion::wants_tick`), so the loop would block on the 30-second idle
    /// deadline and the watchdog below would not run until it expired —
    /// thirty seconds of a left button held down on a live desktop, which is
    /// the exact outcome §12.9 exists to prevent. While anything is watched
    /// the loop polls on [`Self::FLOOR`] instead.
    fn is_watching(&self) -> bool {
        self.owner.is_some()
    }

    /// The keyboard repeated `k` — so `k` is what it is repeating, and a
    /// second repeat of the same key is what measures the cadence.
    fn beat(&mut self, k: muvor_core::motion::Key) {
        let now = std::time::Instant::now();
        match &mut self.owner {
            Some((owner, at, gap)) if *owner == k => {
                *gap = Some(now.duration_since(*at));
                *at = now;
            }
            slot => *slot = Some((k, now, None)),
        }
    }

    /// A hand pressed `k`. **Whatever was repeating stops**, because the
    /// keyboard repeats the key pressed last and `k` is now that key — and
    /// `k` itself has not repeated yet, so there is nothing to watch until
    /// it does. Emptying the slot is the whole fix: it is what stops the
    /// held `a` under a fresh `j` from being read as released.
    fn pressed(&mut self, _k: muvor_core::motion::Key) {
        self.owner = None;
    }

    /// A hand released `k`. Only the repeat owner's release says anything:
    /// some other key coming up leaves the owner repeating exactly as it
    /// was.
    fn released(&mut self, k: muvor_core::motion::Key) {
        if self.owner.is_some_and(|(owner, ..)| owner == k) {
            self.owner = None;
        }
    }

    /// The repeating key, if it has gone quiet — cleared on the way out so
    /// it is reported exactly once.
    fn silent(&mut self) -> Option<muvor_core::motion::Key> {
        let (k, at, gap) = self.owner?;
        let deadline = gap
            .and_then(|g| g.checked_mul(Self::MULTIPLE))
            .unwrap_or(Self::CEILING)
            .clamp(Self::FLOOR, Self::CEILING);
        if std::time::Instant::now().duration_since(at) > deadline {
            self.owner = None;
            Some(k)
        } else {
            None
        }
    }
}

fn movement_mode(
    shell: &shell::Shell,
    client: &mut Client,
    keys: &mut shell::Keys<'_>,
    mapping: &geom::Mapping,
    start: (i32, i32),
    entered_with: Option<muvor_core::motion::Key>,
    out: &mut dyn std::io::Write,
) -> Result<(), Box<dyn std::error::Error>> {
    use muvor_core::motion::{Button as MButton, Cmd, Ev, Key, Motion, Params};

    let (span_w, span_h) = mapping.span();
    let (ox, oy) = mapping.origin();
    #[allow(clippy::cast_possible_truncation)]
    let bounds = Rect::new(
        ox.round() as i32,
        oy.round() as i32,
        span_w.round() as i32,
        span_h.round() as i32,
    );
    let params = Params::default();
    let mut motion = Motion::new(start, bounds, params);

    #[allow(clippy::cast_precision_loss)]
    let hz = 1.0 / params.tick.as_secs_f32();
    say!(out, "movement mode    at {},{} — {hz:.0} Hz; hjkl move, s fast, f click/drag, a wheel (hold + hjkl), d g r, Tab leaves",
        start.0, start.1);

    let wire = |b: MButton| match b {
        MButton::Left => Button::Left,
        MButton::Middle => Button::Middle,
        MButton::Right => Button::Right,
    };

    // The mode outlives no button (§12.9) and no dead-man (§12.8a).
    const HOLD_SAFE: std::time::Duration = std::time::Duration::from_secs(25);
    const IDLE: std::time::Duration = std::time::Duration::from_secs(30);
    let mut drag_since: Option<std::time::Instant> = None;
    let mut tally = Tally::default();
    // What uictl is believed to be holding — see `apply_motion`. `Motion`'s
    // own `dragging` cannot answer this, because a refused release clears
    // the machine's flag and not the broker's.
    let mut held: Option<Button> = None;
    // Cleared by that key's release, and by nothing else. **Measured
    // 2026-08-26**: a hold on label `da` arrives in the mode as
    // `Down(Wheel) Down(Wheel) Up(Wheel)` — the key is still down, so it
    // repeats — and clearing the guard on a press let the release through as
    // a wheel tap, which is a middle click, which is a paste. The cost of
    // the strict rule is that a release lost between the two halves would
    // make that one key dead for the rest of the mode; the cost of the loose
    // one is a click nobody asked for.
    let mut swallow = entered_with;
    let mut failure: Option<Box<dyn std::error::Error>> = None;
    let mut first_refusal: Option<String> = None;

    // **Whether a duplicate `KeyDown` can be read at all** — extension v10
    // and later name autorepeat as itself, and without that promise the two
    // readings of one event are "the release was lost" and "the keyboard is
    // repeating at its own rate" (see `Shell::marks_repeats`).
    let repeats_marked = shell.marks_repeats();
    let mut beats = Heartbeats::default();
    let mut repaired = 0u32;

    // **The cadence is measured from the last tick, not from the last
    // return.** Asking for `params.tick` after each frame has been injected
    // makes the period `tick + work`, so the 90 Hz §12.5 costed against
    // §3.4's budget was really 16 Hz in the sandbox — visibly steppy, and
    // slower the more the pointer is doing. Deadlines also cannot drift into
    // a burst: falling more than one period behind resets the clock instead
    // of firing the backlog, because the backlog would be spent against the
    // rate limiter all at once.
    let mut next_tick = std::time::Instant::now() + params.tick;
    let mut refused = 0u32;

    // **How fast the tick actually ran, measured rather than costed.** §12.5
    // asked for 30 Hz and got 16 for a fortnight, because the period was
    // really `tick + work` and nothing in the mode said so; §12.17 asks for
    // 90, which is three times less room for the same mistake. Only the time
    // the mode spent *wanting* a tick is counted — a user thinking about
    // where to go is not a dropped frame.
    let mut ticking_for = std::time::Duration::ZERO;
    let mut since = std::time::Instant::now();

    'mode: loop {
        let now = std::time::Instant::now();
        if motion.wants_tick().is_some() {
            ticking_for += now.saturating_duration_since(since);
        }
        since = now;
        let timeout = if motion.wants_tick().is_some() {
            next_tick.saturating_duration_since(now)
        } else {
            next_tick = now + params.tick;
            // A held key with nothing moving — `f` mid-drag — still has to
            // be checked on (`Heartbeats::is_watching`).
            if beats.is_watching() { Heartbeats::FLOOR } else { IDLE }
        };
        let ev = match match keys.next(timeout) {
            Ok(o) => o,
            // **Not `?`.** Returning from here would step over the release
            // below with the left button still down (§12.9), and the daemon
            // holds one uictl connection for its whole life — so the button
            // would stay down and every click after it would be refused.
            Err(e) => {
                failure = Some(e.into());
                break 'mode;
            }
        } {
            shell::Outcome::KeyDown(name) => Key::from_name(&name).map(Ev::Down),
            shell::Outcome::KeyUp(name) => Key::from_name(&name).map(Ev::Up),
            // **The keyboard saying a key is still down.** Movement mode is
            // tick-driven (§12.5), so a repeat moves nothing and clicks
            // nothing — it is worth exactly one thing, which is proof of
            // life for `Heartbeats` below. Free mode is the opposite and
            // consumes them as presses, because it has no tick.
            shell::Outcome::KeyRepeat(name) => {
                if let Some(k) = Key::from_name(&name) {
                    beats.beat(k);
                }
                continue;
            }
            // Escape, or the shell's deadman. The grab is already gone.
            shell::Outcome::Cancelled => {
                say!(out, "                 left (escape or the deadman)");
                break 'mode;
            }
            // The timeout IS the tick. With nothing held this is the idle
            // deadline instead, and the mode has simply been abandoned.
            shell::Outcome::TimedOut => {
                // **A watched key going quiet is not an abandoned mode**, so
                // the poll above must not be read as the idle deadline. An
                // idle tick produces no command at all (`Motion::wants_tick`),
                // so running the watchdog costs nothing but the wakeup, and
                // `Heartbeats` empties as soon as it reports — after which
                // the mode is back on the 30-second deadline and can still
                // be abandoned.
                if motion.wants_tick().is_none() && !beats.is_watching() {
                    say!(out, "                 left (idle)");
                    break 'mode;
                }
                Some(Ev::Tick)
            }
            _ => None,
        };
        let Some(ev) = ev else { continue };
        match ev {
            Ev::Up(k) if swallow == Some(k) => {
                swallow = None;
                beats.released(k);
                continue;
            }
            // A press of the entering key while its release is still owed.
            // What that *is* depends on what the shell promises: from v9 and
            // earlier it is the keyboard repeating a key nobody has let go
            // of and there is nothing to do; from v10 a repeat arrives as a
            // repeat, so this is a second press by hand and the release it
            // is owed has been lost. Stop owing it and treat the press as
            // the press it is.
            Ev::Down(k) if swallow == Some(k) => {
                if !repeats_marked {
                    continue;
                }
                swallow = None;
            }
            _ => {}
        }

        if ev == Ev::Tick {
            let now = std::time::Instant::now();
            next_tick += params.tick;
            if next_tick < now {
                next_tick = now + params.tick;
            }
        }

        // **A key going down that is already down is a release that never
        // arrived** — but only where a repeat would have said so itself
        // (§12.16). muvor's own click changes which window has focus, the
        // overlay loses the clutter key focus with it, and the keys that
        // land in that gap are delivered to the application: measured
        // 2026-08-27 at 16.0 ms per gap, which is a whole release. Before
        // this, that one lost `KeyUp` left the key latched down for the
        // rest of the mode and the action it names simply stopped existing
        // — "it works, but not every time", from the desk.
        let cmds = match ev {
            Ev::Down(k) if repeats_marked && motion.is_down(k) => {
                repaired += 1;
                beats.pressed(k);
                motion.repress(k)
            }
            Ev::Down(k) => {
                beats.pressed(k);
                motion.event(ev)
            }
            Ev::Up(k) => {
                beats.released(k);
                motion.event(ev)
            }
            // **A key that was repeating and went quiet came up.** The
            // repair above needs a second press to fire, and the one key
            // nobody presses twice is the one they are still holding — a
            // sweep, or `f` halfway through a drag. Its release is the one
            // that must not be waited for: §12.9's whole argument is that a
            // left button left down is a desktop silently selecting text and
            // dragging files. Heartbeats make that answerable without a
            // timer of its own, and cost nothing while nothing is held.
            Ev::Tick => {
                let mut out = Vec::new();
                if let Some(k) = beats.silent() {
                    eprintln!(
                        "muvor: movement mode: {k:?} stopped repeating without a release —                          letting go of it (§12.16)"
                    );
                    repaired += 1;
                    // **`lost`, not `event(Up)`.** A release muvor invented
                    // must not be able to do more than one that arrived, and
                    // the one thing an `Up` of `a` can do is middle-click
                    // (§12.10) — a new tab, or a primary-selection paste,
                    // from a watchdog admitting it lost track.
                    out.extend(motion.lost(k));
                }
                out.extend(motion.event(ev));
                out
            }
        };
        for cmd in &cmds {
            match cmd {
                Cmd::Press(_) => drag_since = Some(std::time::Instant::now()),
                Cmd::Release(_) => drag_since = None,
                _ => {}
            }
        }
        let leaving = cmds.iter().any(|c| matches!(c, Cmd::Leave));
        // **A refused frame is not the end of the mode.** uictl refuses when
        // muvor asks faster than §3.4's budget, and the response to that is
        // to ask more slowly — not to drop the keyboard, which is what `?`
        // did: the pointer stopped, every key after it did nothing, and on
        // the hotkey path the error went to a sink. Back off by a tick so
        // the bucket refills, and say so where a hotkey user can find it.
        if let Err(e) =
            apply_motion(&cmds, &mut motion, client, mapping, wire, &mut tally, &mut held)
        {
            if !is_uictl_refusal(e.as_ref()) {
                failure = Some(e);
                break 'mode;
            }
            refused += 1;
            next_tick = std::time::Instant::now() + params.tick * 2;
            if refused == 1 {
                // The reason is kept rather than guessed at: the first
                // version of this line blamed §3.4's rate limiter for every
                // refusal, and the one that actually reached a user was
                // `ERR_KEY_ALREADY_HELD` — a different fault with a
                // different fix (§12.9).
                eprintln!("muvor: movement mode: {e}");
                first_refusal = Some(e.to_string());
                say!(out, "                 uictl refused a frame — backing off one tick");
            }
        }
        if leaving {
            say!(out, "                 left (Tab or right Alt)");
            break 'mode;
        }

        // uictl releases a held button after HOLD_MAX_SEC = 30 (§3.4) and
        // says nothing. Letting go first is the only way the next `f` is not
        // an unpaired release.
        if drag_since.is_some_and(|t| t.elapsed() > HOLD_SAFE) {
            drag_since = None;
            let cmds = motion.release_drag();
            // **Not `?`, for the reason the loop above already learned.**
            // This was the last `return` in the function that stepped over
            // the §12.9 release below, and it is the one that fires with a
            // button *definitely* down. A refusal here is the rate limiter
            // saying "later", and later is what the exit release is for.
            if let Err(e) =
                apply_motion(&cmds, &mut motion, client, mapping, wire, &mut tally, &mut held)
            {
                say!(out, "                 drag held 25s — the release was refused: {e}");
                if !is_uictl_refusal(e.as_ref()) {
                    failure = Some(e);
                    break 'mode;
                }
            } else {
                say!(out, "                 drag held 25s — released before uictl's dead-man");
            }
        }
    }

    // **§12.9, and this time unconditionally.** Every way out of that loop
    // ends here — Tab, Escape, the deadman, the idle timeout, a dead keyboard
    // and a broker that stopped answering — because a mode that can hold the
    // left button can leave it down, and a stuck left button is a desktop
    // silently selecting text and dragging files. `leave` is idempotent, so
    // the paths that already released pay nothing. Best effort: if this fails
    // there is nothing further to try, and the failure that brought us here
    // is the one worth reporting.
    let cmds = motion.leave();
    if !cmds.is_empty() {
        let _ = apply_motion(&cmds, &mut motion, client, mapping, wire, &mut tally, &mut held);
    }
    // **And again, against what uictl thinks rather than what `Motion`
    // thinks.** `leave` releases only what the machine still calls a drag,
    // and a release the rate limiter refused already cleared that flag — so
    // this is the case the "unconditional" release above could not see. It
    // retries, because the reason a release gets refused is a bucket that is
    // empty *now*, and one tick later it is not. `release` is a no-op when
    // the button is genuinely up, so the loop costs nothing in the normal
    // case: `held` is `None` and it does not run at all.
    if let Some(b) = held {
        let mut freed = false;
        for _ in 0..4 {
            if client.release(b).is_ok() {
                freed = true;
                break;
            }
            std::thread::sleep(params.tick);
        }
        if !freed {
            // Nothing further to try from here, and silence would be the
            // worst outcome: every click for the rest of the daemon's life
            // is about to be refused, and `Client::press`'s repair is the
            // only thing standing between the user and that.
            eprintln!(
                "muvor: movement mode could not release the {b:?} button — uictl refused four \
                 times. The next click will repair it (Client::unstick)."
            );
        }
    }

    shell.hide().ok();
    let (x, y) = motion.pos();
    say!(out, "                 {} move(s), {} click(s), {} notch(es), pointer at {x},{y}",
        tally.moves, tally.clicks, tally.scrolls);
    // Enough of a sweep to divide by: a two-frame nudge says nothing about a
    // cadence and would report wild numbers for no reason.
    let asked = 1.0 / params.tick.as_secs_f64();
    if tally.moves >= 10 && ticking_for > std::time::Duration::from_millis(200) {
        let got = f64::from(tally.moves) / ticking_for.as_secs_f64();
        say!(out, "                 tick ran at {got:.0} Hz over {:.1}s of held keys (asked {asked:.0})",
            ticking_for.as_secs_f64());
        // The hotkey path writes `out` to a sink, and a tick running at half
        // the rate it was costed at is exactly the fault §12.5 could not see
        // for a fortnight. 80% is the bar: below it the pointer is stepping.
        if got < asked * 0.8 {
            eprintln!(
                "muvor: movement mode ran at {got:.0} Hz, not {asked:.0} — the loop is spending \
                 longer than one tick per frame, or uictl is refusing (§12.5, §12.17). \
                 {} move(s) in {:.1}s of held keys.",
                tally.moves,
                ticking_for.as_secs_f64()
            );
        }
    }
    // **A repair is a lost key event, and a lost key event is a fault.** The
    // mode carries on and the user sees nothing, which is the whole point of
    // §12.16 — and is exactly why it has to be said somewhere. Three
    // faults in a row (§12.12, §12.14, §12.15) presented as "it doesn't
    // always work" and left `ERRORS.md` empty, because nothing failed. This
    // is the line that stops the fourth one being invisible.
    if repaired > 0 {
        eprintln!(
            "muvor: movement mode repaired {repaired} lost key release(s) — the overlay was              not holding the keyboard for part of the mode (§12.16). {} move(s), {} click(s).",
            tally.moves, tally.clicks
        );
        say!(out, "                 {repaired} lost key release(s) repaired (§12.16)");
    }
    // The hotkey path writes `out` to `io::sink()`, so this is the only line
    // that can reach a person who never opened a terminal — which is why it
    // is here and why it fires only when something was actually refused. A
    // watchdog that reports normal operation trains you to skim it.
    if refused > 0 {
        eprintln!(
            "muvor: movement mode ended after {refused} refused frame(s) — {} move(s), \
             {} click(s), {} notch(es). First was: {}",
            tally.moves,
            tally.clicks,
            tally.scrolls,
            first_refusal.as_deref().unwrap_or("(not recorded)")
        );
    }
    if let Some(e) = failure {
        return Err(e);
    }
    Ok(())
}

/// What movement mode did, for the line it prints on the way out. Three
/// counters in a struct rather than three `&mut u32` arguments, which is
/// what clippy's argument limit was pointing at and it was right.
#[derive(Default)]
struct Tally {
    moves: u32,
    clicks: u32,
    scrolls: u32,
}

/// Send what the machine asked for.
///
/// Split out because movement mode's loop calls it from five places and one
/// of them is the path that must always release a held button.
/// `held` is **uictl's** idea of what is down, not `Motion`'s, and the two
/// are not the same thing. `Motion` is pure: it decides a release has
/// happened the instant it emits the command. uictl can then refuse that
/// command — the rate limiter does not care what the frame was for — and
/// from that moment the machine believes the button is up while the broker
/// believes it is down. Because the daemon keeps **one uictl connection for
/// its whole life** (§12.9), that divergence is permanent: every click after
/// it is `ERR_KEY_ALREADY_HELD`, in every window, which from the desk is
/// `f d g a r` having stopped existing while `hjkl` still works. `Motion`
/// has nothing left to release, so the unconditional release on the way out
/// released nothing.
///
/// So the driver keeps its own flag, and it is deliberately pessimistic in
/// both directions: set **before** the press is sent, cleared only **after**
/// uictl has accepted the release. Releasing a button that is already up is
/// a no-op ([`Client::release`] treats `ERR_KEY_NOT_HELD` as success);
/// leaving one down is a desktop silently dragging files.
fn apply_motion(
    cmds: &[muvor_core::motion::Cmd],
    motion: &mut muvor_core::motion::Motion,
    client: &mut Client,
    mapping: &geom::Mapping,
    wire: impl Fn(muvor_core::motion::Button) -> Button,
    tally: &mut Tally,
    held: &mut Option<Button>,
) -> Result<(), Box<dyn std::error::Error>> {
    use muvor_core::motion::Cmd;
    for cmd in cmds {
        match *cmd {
            Cmd::MoveTo(x, y) => {
                let (dx, dy) = mapping.to_device(x, y);
                client.move_abs(dx, dy)?;
                tally.moves += 1;
            }
            Cmd::Press(b) => {
                let (x, y) = motion.pos();
                let (dx, dy) = mapping.to_device(x, y);
                // Move, then hold: a drag that starts where the pointer was
                // two commands ago is the off-by-one-gesture bug.
                client.move_abs(dx, dy)?;
                // Recorded before the wire, not after: a press that is sent
                // and whose reply is lost is still a press.
                *held = Some(wire(b));
                // `press`, not `button(.., true)`: it repairs a hold uictl
                // still thinks muvor owns, which one lost release is enough
                // to create and which nothing else clears while the daemon
                // lives.
                client.press(wire(b))?;
            }
            // `release`, so "it was not held" counts as let go of — see
            // `Client::release`. A mode must always be able to stop holding.
            // Cleared only once uictl has said yes: `?` here means the
            // button is still down as far as the broker is concerned, and
            // the exit path has to know that.
            Cmd::Release(b) => {
                client.release(wire(b))?;
                *held = None;
            }
            Cmd::Click { button, times } => {
                let (x, y) = motion.pos();
                let (dx, dy) = mapping.to_device(x, y);
                let code = wire(button);
                // N frames, not one batch (§12.8). Measured: one batch is
                // not a slow double click, it is not a double click at all.
                for n in 0..times {
                    if n > 0 {
                        std::thread::sleep(MULTI_CLICK_GAP);
                    }
                    client.click_at(dx, dy, code)?;
                }
                tally.clicks += 1;
            }
            Cmd::Scroll { v, h } => {
                // Its own opcode, one token, and no MOVE_ABS: the pointer
                // does not move when the wheel turns (§12.10).
                client.scroll(v, h)?;
                tally.scrolls += 1;
            }
            Cmd::Leave => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ---- §12.16: the key nobody presses twice ------------------------
    ///
    /// `Motion::repress` repairs a lost release the next time that key is
    /// pressed, which covers `d`, `g`, `r` and every tap. It cannot cover a
    /// key that is *held* — a sweep, `f` in the middle of a drag — because
    /// there is no second press coming. `Heartbeats` is the other half:
    /// extension v10 reports autorepeat as itself, so a key that was
    /// repeating and goes quiet has come up.
    mod heartbeats {
        use super::Heartbeats;
        use muvor_core::free::Dir;
        use muvor_core::motion::Key;
        use std::time::{Duration, Instant};

        /// The cadence is measured, not assumed, so the test states it
        /// rather than sleeping for it.
        fn beating(k: Key, gap: Duration, ago: Duration) -> Heartbeats {
            let now = Instant::now();
            Heartbeats { owner: Some((k, now - ago, Some(gap))) }
        }

        #[test]
        fn a_key_that_never_repeated_is_never_watched() {
            let mut h = Heartbeats::default();
            h.released(Key::LeftButton);
            assert!(h.silent().is_none(), "its release is still owed and still expected");
            assert!(!h.is_watching(), "and the mode goes back to sleeping on the idle deadline");
        }

        /// `f` held with no direction is a drag, and a drag wants no tick.
        /// The loop would sleep on the 30-second idle deadline and the
        /// watchdog would not run until it expired — thirty seconds of a
        /// left button down on a live desktop (§12.9).
        #[test]
        fn a_drag_with_nothing_moving_is_still_polled() {
            let mut h = Heartbeats::default();
            h.beat(Key::LeftButton);
            assert!(h.is_watching());
            // And once it has been reported the mode can be abandoned again.
            let mut h = beating(Key::LeftButton, Duration::from_millis(40),
                                Duration::from_millis(400));
            assert_eq!(h.silent(), Some(Key::LeftButton));
            assert!(!h.is_watching());
        }

        #[test]
        fn a_key_still_repeating_is_still_down() {
            let mut h = beating(Key::Move(Dir::Right), Duration::from_millis(40),
                                Duration::from_millis(40));
            assert!(h.silent().is_none());
        }

        #[test]
        fn a_key_that_stopped_repeating_came_up() {
            let mut h = beating(Key::LeftButton, Duration::from_millis(40),
                                Duration::from_millis(400));
            assert_eq!(h.silent(), Some(Key::LeftButton), "and a drag does not outlive the mode");
            assert!(h.silent().is_none(), "reported once, not on every tick");
        }

        /// The repeat interval is the user's setting and an accessibility
        /// profile can make it very slow. A deadline of "five of its own
        /// intervals" cannot misfire on a keyboard it has never measured;
        /// a fixed number of milliseconds would release a held button once
        /// a second on that machine.
        #[test]
        fn the_deadline_is_the_keyboards_own_cadence() {
            let slow = Duration::from_millis(500);
            let mut h = beating(Key::Move(Dir::Up), slow, Duration::from_millis(900));
            assert!(h.silent().is_none(), "under two intervals of a slow keyboard");
            let mut h = beating(Key::Move(Dir::Up), slow, Duration::from_millis(2600));
            assert_eq!(h.silent(), Some(Key::Move(Dir::Up)));
        }

        /// Before a second repeat there is no cadence, so the ceiling
        /// applies — late is the only safe direction to be wrong in when
        /// the thing being guessed at is "let go of the left button".
        #[test]
        fn an_unmeasured_cadence_waits_the_ceiling_out() {
            let now = Instant::now();
            let mut h = Heartbeats { owner: Some((Key::Fast, now - Duration::from_secs(1), None)) };
            assert!(h.silent().is_none());
            let mut h = Heartbeats { owner: Some((Key::Fast, now - Duration::from_secs(4), None)) };
            assert_eq!(h.silent(), Some(Key::Fast));
        }

        #[test]
        fn a_hand_releasing_the_key_starts_it_over() {
            let mut h = beating(Key::LeftButton, Duration::from_millis(40),
                                Duration::from_millis(400));
            h.released(Key::LeftButton);
            assert!(h.silent().is_none(), "the witnessed event is the one that counts");
        }

        /// ---- §12.17: a keyboard repeats the key pressed last -----------
        ///
        /// This is the one that makes `a`-and-`j` work. Hold `a` until it
        /// repeats, then press `j`: the keyboard hands the repeat to `j` and
        /// `a` goes quiet *while it is still held*. Reading that silence as
        /// a release switched the wheel off a quarter of a second into every
        /// scroll, and `j` went back to moving the pointer.
        #[test]
        fn a_new_press_takes_the_repeat_and_the_old_key_is_not_declared_up() {
            let mut h = beating(Key::Wheel, Duration::from_millis(40),
                                Duration::from_millis(40));
            h.pressed(Key::Move(Dir::Down));
            assert!(!h.is_watching(), "nothing is repeating yet, so nothing is being judged");
            // Even long past `a`'s deadline: it is not the repeat owner, so
            // its silence is not evidence.
            std::thread::sleep(Duration::from_millis(1));
            assert!(h.silent().is_none(), "`a` is still held and nothing says otherwise");
        }

        /// And the key that took the repeat is watched in its place, so the
        /// cover §12.16 bought is not simply given back.
        #[test]
        fn the_key_that_took_the_repeat_is_the_one_watched() {
            let mut h = Heartbeats::default();
            h.beat(Key::Wheel);
            h.pressed(Key::Move(Dir::Down));
            h.beat(Key::Move(Dir::Down));
            assert_eq!(h.owner.expect("watched").0, Key::Move(Dir::Down));
            let mut h = beating(Key::Move(Dir::Down), Duration::from_millis(40),
                                Duration::from_millis(400));
            assert_eq!(h.silent(), Some(Key::Move(Dir::Down)));
        }

        /// One slot, because a keyboard repeats one key. The old shape kept
        /// a list and judged every entry in it against its own last beat,
        /// which is what produced the false releases.
        #[test]
        fn only_one_key_is_ever_watched() {
            let mut h = Heartbeats::default();
            h.beat(Key::Move(Dir::Left));
            h.beat(Key::Fast);
            assert_eq!(h.owner.expect("watched").0, Key::Fast, "the newest repeat owns the slot");
            assert!(h.owner.expect("watched").2.is_none(), "a different key has no cadence yet");
            h.beat(Key::Fast);
            assert!(
                h.owner.expect("watched").2.is_some(),
                "a second repeat of the same key measures the cadence"
            );
        }

        /// Releasing something else must not blind the watchdog to the key
        /// that is actually repeating.
        #[test]
        fn releasing_another_key_leaves_the_repeat_owner_watched() {
            let mut h = beating(Key::Move(Dir::Right), Duration::from_millis(40),
                                Duration::from_millis(400));
            h.released(Key::Wheel);
            assert_eq!(h.silent(), Some(Key::Move(Dir::Right)));
        }
    }

    fn win(x: i32, y: i32, w: i32, h: i32) -> shell::Window {
        shell::Window {
            title: String::new(),
            x,
            y,
            w,
            h,
            pid: 0,
            wm_class: String::new(),
            // A test window has no shadow, so the buffer is the frame.
            buffer: (x, y, w, h),
            minimized: false,
            showing: true,
        }
    }

    /// The regression this constant exists for: on 2026-08-19 an exact
    /// comparison re-measured the fit before every single hint, because a
    /// maximized window ends at 1080 and the fit ends at 1079.997.
    #[test]
    fn a_maximized_window_does_not_look_like_a_new_monitor() {
        let m = geom::assumed(1920, 1080, 32767);
        assert!(!outgrew(&m, &win(0, 32, 1920, 1048)), "flush with the bottom edge");
        assert!(!outgrew(&m, &win(0, 0, 1920, 1080)), "flush with every edge");
    }

    #[test]
    fn a_window_on_a_monitor_that_was_not_there_does() {
        let m = geom::assumed(1920, 1080, 32767);
        assert!(outgrew(&m, &win(1920, 0, 1920, 1080)), "a second screen to the right");
        assert!(outgrew(&m, &win(0, 1080, 1920, 1080)), "a second screen below");
    }

    /// A window a few pixels over is a rounding artefact or a shadow, not a
    /// display change — and re-measuring moves the pointer.
    #[test]
    fn a_few_pixels_over_is_not_a_display_change() {
        let m = geom::assumed(1920, 1080, 32767);
        assert!(!outgrew(&m, &win(0, 0, 1924, 1080)));
        assert!(outgrew(&m, &win(0, 0, 1940, 1080)), "but a real overhang is");
    }
}
