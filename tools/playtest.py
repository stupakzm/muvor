#!/usr/bin/env python3
"""playtest — press real keys at a nested GNOME Shell, from a script.

WHY THIS EXISTS. `tools/nested-shell.sh` says, at the top, that the one thing
it cannot test is key events: a nested shell has no window focus unless its
own window is focused in the outer session (plan.md §5.4a, fact 4). That
sentence has cost two logouts and, on 2026-08-25, two rounds of asking a
human to press `Alt+;` and report what happened. Everything about the hint
state machine — the hotkey, label typing, Tab to deepen, tap versus hold,
Escape, whether the key focus comes back — was unverifiable without a person.

It is verifiable. **mutter injects keys into its own session over D-Bus**:
`org.gnome.Mutter.RemoteDesktop`, which is the interface every remote-desktop
and accessibility tool on GNOME already uses. Pointed at the *nested* shell's
private bus it drives that session and cannot touch the real one.

WHAT THIS IS NOT. It is not an input device and it never becomes one. No
`/dev/input`, no `/dev/uinput`, no uictl key policy — muvor's doctrine that
it never opens an input device is not bent here, because this is a test
driver and not muvor. uictl's default-deny key gate stays shut and no policy
file is needed (ERRORS.md, 2026-08-25).

WHAT IT COVERS, AND WHAT IT DOES NOT. It delivers keys, so it answers the
whole shell half and every decision muvor makes from one. It does **not**
verify where the pointer physically went: muvor's own output goes through
uictl to the host seat, not into the nested session. Read the daemon's
printed claim for that, and keep §12.8's real-desktop measurements for the
pointer itself.

    # one session, a script of timed events
    python3 tools/playtest.py tap:alt+semicolon wait:400 tap:h hold:s:900

    # the session dies with this process, which is mutter's rule and not a
    # bug: create, Start, inject, exit.

SCRIPT TOKENS
    wait:MS         do nothing for MS milliseconds
    tap:KEY         press and release
    down:KEY        press and leave down
    up:KEY          release
    hold:KEY:MS     press, wait MS, release — the hold gesture (§12.3)

KEY is a keysym name: a single character (`a`, `;`), a named key (`Tab`,
`Escape`, `Return`), or a combination joined with `+` (`alt+semicolon`).
Modifiers go down in order and come up in reverse, which is what a real
keyboard does and what mutter's keybinding code expects.
"""

import sys
import time

import gi

gi.require_version("Gio", "2.0")
from gi.repository import Gio, GLib  # noqa: E402

# X11 keysyms for everything that is not a printable ASCII character. For
# printable ASCII the keysym IS the codepoint, which is why there is no table
# of letters here.
NAMED = {
    "alt": 0xFFE9, "alt_l": 0xFFE9, "alt_r": 0xFFEA,
    "shift": 0xFFE1, "shift_l": 0xFFE1, "shift_r": 0xFFE2,
    "ctrl": 0xFFE3, "control": 0xFFE3, "control_l": 0xFFE3, "control_r": 0xFFE4,
    "super": 0xFFEB, "super_l": 0xFFEB, "super_r": 0xFFEC,
    "tab": 0xFF09, "escape": 0xFF1B, "esc": 0xFF1B, "return": 0xFF0D,
    "enter": 0xFF0D, "space": 0x0020, "backspace": 0xFF08,
    "up": 0xFF52, "down": 0xFF54, "left": 0xFF51, "right": 0xFF53,
    "f1": 0xFFBE, "f2": 0xFFBF, "f3": 0xFFC0, "f4": 0xFFC1, "f5": 0xFFC2,
    "f6": 0xFFC3, "f7": 0xFFC4, "f8": 0xFFC5, "f9": 0xFFC6, "f10": 0xFFC7,
    "f11": 0xFFC8, "f12": 0xFFC9,
    "semicolon": 0x003B, "comma": 0x002C, "period": 0x002E, "slash": 0x002F,
    "apostrophe": 0x0027, "minus": 0x002D, "equal": 0x003D,
}


def keysym(name):
    """One key name -> one X keysym."""
    low = name.lower()
    if low in NAMED:
        return NAMED[low]
    if len(name) == 1:
        return ord(name)
    raise SystemExit(f"playtest: no keysym for {name!r}")


def combo(spec):
    """`alt+semicolon` -> [0xffe9, 0x3b], in press order."""
    return [keysym(p) for p in spec.split("+")]


class Session:
    """One RemoteDesktop session, alive for as long as this object is.

    **The connection is the session's lifetime** and that is mutter's rule,
    not an accident: `CreateSession` ties the session to the calling D-Bus
    connection and destroys it the moment that connection drops. A `gdbus
    call` per keystroke therefore creates a session, tears it down, and
    injects nothing — which is exactly what it looks like when it does not
    work, so it is worth knowing before debugging it.
    """

    IFACE = "org.gnome.Mutter.RemoteDesktop"

    def __init__(self, address):
        self.conn = Gio.DBusConnection.new_for_address_sync(
            address,
            Gio.DBusConnectionFlags.AUTHENTICATION_CLIENT
            | Gio.DBusConnectionFlags.MESSAGE_BUS_CONNECTION,
            None,
            None,
        )
        self.path = self._call(
            "/org/gnome/Mutter/RemoteDesktop", self.IFACE, "CreateSession", None, "(o)"
        )[0]
        self._call(self.path, f"{self.IFACE}.Session", "Start", None, "()")

    def _call(self, path, iface, method, args, reply):
        return self.conn.call_sync(
            self.IFACE, path, iface, method, args, GLib.VariantType(reply), 0, -1, None
        ).unpack()

    def key(self, sym, down):
        self._call(
            self.path,
            f"{self.IFACE}.Session",
            "NotifyKeyboardKeysym",
            GLib.Variant("(ub)", (sym, down)),
            "()",
        )

    def stop(self):
        try:
            self._call(self.path, f"{self.IFACE}.Session", "Stop", None, "()")
        except GLib.Error:
            pass


def run(session, tokens, verbose=True):
    def say(msg):
        if verbose:
            print(f"  {msg}", flush=True)

    for token in tokens:
        parts = token.split(":")
        verb = parts[0]
        if verb == "wait":
            ms = int(parts[1])
            say(f"wait {ms} ms")
            time.sleep(ms / 1000)
        elif verb == "tap":
            syms = combo(parts[1])
            say(f"tap {parts[1]}")
            for s in syms:
                session.key(s, True)
            for s in reversed(syms):
                session.key(s, False)
        elif verb == "down":
            syms = combo(parts[1])
            say(f"down {parts[1]}")
            for s in syms:
                session.key(s, True)
        elif verb == "up":
            syms = combo(parts[1])
            say(f"up {parts[1]}")
            for s in reversed(syms):
                session.key(s, False)
        elif verb == "hold":
            syms, ms = combo(parts[1]), int(parts[2])
            say(f"hold {parts[1]} for {ms} ms")
            for s in syms:
                session.key(s, True)
            time.sleep(ms / 1000)
            for s in reversed(syms):
                session.key(s, False)
        else:
            raise SystemExit(f"playtest: unknown token {token!r}")


def await_show(address, timeout_s):
    """Block until the overlay is drawn, and return the labels it was given.

    **Why the labels are read off the bus instead of guessed.** They are
    assigned from `HOME_ROW` by column and `RANK_ROWS` by rank (§5.3c), which
    is deterministic but depends on what the detector found and where — so a
    harness that hardcodes `da` passes until the window changes and then
    fails for a reason that has nothing to do with what it was testing.
    `Show` carries them, so the honest thing is to watch it go past.

    This uses `BecomeMonitor`, the same mechanism as `dbus-monitor`. It has
    to be in-process: the overlay's deadman is 10 s (§5.1b), and reading the
    labels in one command and injecting in the next spends all of it.
    """
    conn = Gio.DBusConnection.new_for_address_sync(
        address,
        Gio.DBusConnectionFlags.AUTHENTICATION_CLIENT
        | Gio.DBusConnectionFlags.MESSAGE_BUS_CONNECTION,
        None, None)
    conn.call_sync(
        "org.freedesktop.DBus", "/org/freedesktop/DBus",
        "org.freedesktop.DBus.Monitoring", "BecomeMonitor",
        GLib.Variant("(asu)", ([
            "type='method_call',interface='org.muvor.Shell',member='Show'",
            "type='method_call',interface='org.muvor.Shell',member='ShowMarked'",
        ], 0)),
        GLib.VariantType("()"), 0, -1, None)

    found = []
    loop = GLib.MainLoop()

    def on_message(_conn, message, _incoming):
        if message.get_member() in ("Show", "ShowMarked") and not found:
            body = message.get_body()
            if body:
                for target in body.unpack()[0]:
                    found.append(target[-1])
            loop.quit()
        return message

    conn.add_filter(on_message)
    GLib.timeout_add_seconds(timeout_s, lambda: (loop.quit(), False)[1])
    loop.run()
    return found


def main(argv):
    bus_file = "/tmp/nested-addr"
    on_show = None
    timeout_s = 20
    tokens = []
    i = 0
    while i < len(argv):
        if argv[i] == "--bus":
            bus_file = argv[i + 1]
            i += 2
        elif argv[i] == "--on-show":
            on_show = argv[i + 1]
            i += 2
        elif argv[i] == "--timeout":
            timeout_s = int(argv[i + 1])
            i += 2
        else:
            tokens.append(argv[i])
            i += 1
    if not tokens:
        print(__doc__)
        return 0

    try:
        address = open(bus_file).read().strip()
    except OSError:
        raise SystemExit(
            f"playtest: no bus address at {bus_file} — is the nested shell up?\n"
            "           bash tools/nested-shell.sh"
        )

    if on_show is not None:
        print(f"playtest: waiting up to {timeout_s}s for the overlay", flush=True)
        labels = await_show(address, timeout_s)
        if not labels:
            raise SystemExit("playtest: no overlay was drawn — nothing to type")
        first = labels[0]
        print(f"playtest: {len(labels)} label(s): {' '.join(labels)}", flush=True)
        # `{c0}`/`{c1}` are the first label's characters, which is what a
        # scenario usually wants. `{L2}`, `{L2c0}`, `{L2c1}` reach any other
        # one by position — needed because §4.5 may refuse the first target,
        # and a harness that can only ever type one label cannot then get
        # past it.
        subs = {"label": first, "labels": " ".join(labels)}
        for n, ch in enumerate(first):
            subs[f"c{n}"] = ch
        for i, lab in enumerate(labels, start=1):
            subs[f"L{i}"] = lab
            for n, ch in enumerate(lab):
                subs[f"L{i}c{n}"] = ch
        tokens = on_show.format(**subs).split() + tokens

    session = Session(address)
    print(f"playtest: session {session.path} on {bus_file}", flush=True)
    try:
        run(session, tokens)
    finally:
        session.stop()
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
