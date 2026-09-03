#!/usr/bin/env python3
"""fake-uictld — uictl's wire protocol, wired to a nested shell instead of a device.

WHY THIS EXISTS. `tools/playtest.py` can press keys at a nested GNOME Shell,
which makes the shell half testable at last. But muvor answers a key by
moving the *pointer*, and it does that through uictl — which writes to
`/dev/uinput`, on the **host seat**. In a nested session that is the wrong
screen: muvor would calibrate against a 1280x720 nested display and then
drive the real pointer with the numbers, and a click at those coordinates
lands somewhere on the user's actual desktop. "A wrong click is
unrecoverable" (§4.5) is exactly the thing a test harness must not risk.

So this speaks uictl's protocol (`~/projects/uictl/WIRE.md`, laid out
machine-readably in `proto.json`) and injects through
`org.gnome.Mutter.RemoteDesktop` on the **nested** bus. Every pointer motion,
button and scroll muvor decides on lands inside the nested session, is
recorded here, and can be asserted against. Nothing touches the host.

**This is not a uictl implementation and must never be mistaken for one.**
It implements the frames muvor sends and nothing else. It does no policy, no
confirmation, no rate limiting, no audit. It refuses every key opcode, which
is not a simplification — muvor injects no keys (ERRORS.md, 2026-08-25), so
a shim that accepted them would be able to pass a test the real broker would
fail.

    python3 tools/fake-uictld.py --runtime DIR --bus /tmp/nested-addr \
        --size 1280x720 --log DIR/uictl.log

Every request is appended to the log, one per line, as
`OPCODE field=value ... -> what it did`, which is the assertion surface.
"""

import argparse
import os
import socket
import struct
import sys
import threading
import time

import gi

gi.require_version("Gio", "2.0")
from gi.repository import Gio, GLib  # noqa: E402

HDR = struct.Struct("<HHIII")  # version, opcode, source_tag, seq, payload_len

PING, MOVE_ABS, HELLO, KEY_TAP, KEY_SEQ, KEY_DOWN, KEY_UP = 1, 2, 3, 4, 5, 6, 7
BUTTON, MOVE_REL, SCROLL, BATCH = 11, 12, 13, 14

OK, ERR_PAYLOAD_INVALID, ERR_DENIED_BY_POLICY = 0, 3, 4
ERR_RATE_LIMITED = 11
ERR_KEY_ALREADY_HELD = 12
ERR_KEY_NOT_HELD = 14

# **Held-button arbitration, modelled — because the real broker refuses a
# press of a button it already holds, and muvor's connection is long-lived.**
# uictld keeps `held_bits` per connection and seeds a BATCH's validation from
# them (uictld.c, `memcpy(would_hold, c->held_bits, ...)`), so one lost
# release poisons **every later click for the life of the connection** —
# which is exactly what reached a user on 2026-08-26 with the sandbox
# reporting clicks fine, because this shim tracked no state at all.

# **The real broker's rate limit, modelled — because a shim without one lets
# muvor pass a test the desk would fail.** uictld gives a registered
# `interactive` client 50 requests/second sustained with a bucket of 50
# (src/uictld.c, rate_classes), refills continuously, and answers
# ERR_RATE_LIMITED on the frame it drops rather than queueing it. Movement
# mode ticks at 30 Hz against that budget (§12.5), so anything that makes it
# ask faster shows up here as a refusal — which is exactly the failure that
# reached a user on 2026-08-26 with this harness reporting everything fine.
# `rate_charged` per opcode is from uictl's proto.json.
# `MUVOR_FAKE_RATE=5/5` dials it down to the untrusted floor, which is how
# the backoff path is exercised without waiting for muvor to misbehave.
RATE_PER_SEC, RATE_BURST = (
    [int(v) for v in os.environ.get("MUVOR_FAKE_RATE", "50/50").split("/")] + [50])[:2]
RATE_UNIT = 1000

NAMES = {
    PING: "PING", MOVE_ABS: "MOVE_ABS", HELLO: "HELLO", KEY_TAP: "KEY_TAP",
    KEY_SEQ: "KEY_SEQUENCE", KEY_DOWN: "KEY_DOWN", KEY_UP: "KEY_UP",
    BUTTON: "BUTTON", MOVE_REL: "MOVE_REL", SCROLL: "SCROLL", BATCH: "BATCH",
}

ABS_MAX = 32767
# PING, MOVE_ABS, HELLO, BUTTON, MOVE_REL, SCROLL, BATCH — the set muvor uses,
# and the ONLY correct feature test a client has (WIRE.md §3.4).
BITMAP = (1 << PING) | (1 << MOVE_ABS) | (1 << HELLO) | (1 << BUTTON) \
    | (1 << MOVE_REL) | (1 << SCROLL) | (1 << BATCH)
CAPS = 0x000F


class Pointer:
    """The nested session's pointer, driven over RemoteDesktop.

    **Absolute motion without a screencast stream.**
    `NotifyPointerMotionAbsolute` wants a stream name, which means a
    ScreenCast session, which is a lot of machinery for a test. Relative
    motion needs none — and mutter clamps the pointer to the screen, so a
    large enough negative delta parks it at 0,0 exactly. Home, then step to
    the target. Two calls, no stream, and the position is deterministic.
    """

    IFACE = "org.gnome.Mutter.RemoteDesktop"

    def __init__(self, address, width, height):
        self.w, self.h = width, height
        self.conn = Gio.DBusConnection.new_for_address_sync(
            address,
            Gio.DBusConnectionFlags.AUTHENTICATION_CLIENT
            | Gio.DBusConnectionFlags.MESSAGE_BUS_CONNECTION,
            None, None)
        self.path = self._call("/org/gnome/Mutter/RemoteDesktop", self.IFACE,
                               "CreateSession", None, "(o)")[0]
        self._call(self.path, f"{self.IFACE}.Session", "Start", None, "()")
        self.x = self.y = 0
        self._home()

    def _call(self, path, iface, method, args, reply):
        return self.conn.call_sync(self.IFACE, path, iface, method, args,
                                   GLib.VariantType(reply), 0, -1, None).unpack()

    def _rel(self, dx, dy):
        self._call(self.path, f"{self.IFACE}.Session", "NotifyPointerMotionRelative",
                   GLib.Variant("(dd)", (float(dx), float(dy))), "()")

    def _home(self):
        self._rel(-4 * self.w, -4 * self.h)
        self.x = self.y = 0

    def to_px(self, dev_x, dev_y):
        """Device units -> nested pixels. The inverse of what muvor calibrates."""
        return (round(dev_x * (self.w - 1) / ABS_MAX),
                round(dev_y * (self.h - 1) / ABS_MAX))

    def move_abs(self, dev_x, dev_y):
        px, py = self.to_px(dev_x, dev_y)
        self._home()
        if px or py:
            self._rel(px, py)
        self.x, self.y = px, py
        return px, py

    def move_rel(self, dx, dy):
        self._rel(dx, dy)
        self.x, self.y = self.x + dx, self.y + dy
        return self.x, self.y

    def button(self, code, down):
        self._call(self.path, f"{self.IFACE}.Session", "NotifyPointerButton",
                   GLib.Variant("(ib)", (code, bool(down))), "()")

    def scroll(self, v, h):
        for axis, steps in ((0, v), (1, h)):
            if steps:
                self._call(self.path, f"{self.IFACE}.Session",
                           "NotifyPointerAxisDiscrete",
                           GLib.Variant("(ui)", (axis, -steps)), "()")


class Server:
    def __init__(self, pointer, log_path):
        self.p = pointer
        self.log_path = log_path
        self.lock = threading.Lock()
        self.milli = RATE_BURST * RATE_UNIT
        self.last_refill = time.monotonic()
        self.refused = 0
        self.held = set()   # per connection, reset in `handle`, like uictld's

    def log(self, line):
        with self.lock:
            with open(self.log_path, "a") as f:
                f.write(line + "\n")
        print(line, flush=True)

    def handle(self, conn):
        shook = False
        buf = b""
        while True:
            try:
                chunk = conn.recv(65536)
            except OSError:
                break
            if not chunk:
                break
            buf += chunk
            while len(buf) >= HDR.size:
                ver, op, tag, seq, plen = HDR.unpack_from(buf)
                if len(buf) < HDR.size + plen:
                    break
                payload = buf[HDR.size:HDR.size + plen]
                buf = buf[HDR.size + plen:]
                result, data = self.dispatch(op, payload, shook)
                if op == HELLO and result == OK:
                    shook = True
                out = HDR.pack(ver, op, tag, seq, 2 + len(data))
                conn.sendall(out + struct.pack("<H", result) + data)
        # uictld synthesizes a release burst when a client goes away, and
        # keeps it "small and predictable" (proto.h). Modelling it matters:
        # without it a stuck button looks permanent here and merely
        # session-long there.
        for code in sorted(self.held):
            self.p.button(code, 0)
            self.log(f"BUTTON code={code} down=0   (released: client went away)")
        self.held.clear()
        conn.close()

    def rate_allow(self, op, payload):
        """One token per charged request, refilled by the clock. A BUTTON
        release is not charged — uictl never refuses letting go of one."""
        charged = {MOVE_ABS, MOVE_REL, SCROLL, BATCH, KEY_TAP, KEY_SEQ, KEY_DOWN}
        if op == BUTTON:
            if len(payload) >= 4 and payload[2] == 0:
                return True
        elif op not in charged:
            return True
        now = time.monotonic()
        self.milli = min(RATE_BURST * RATE_UNIT,
                         self.milli + int((now - self.last_refill) * RATE_PER_SEC * RATE_UNIT))
        self.last_refill = now
        if self.milli < RATE_UNIT:
            return False
        self.milli -= RATE_UNIT
        return True

    def dispatch(self, op, payload, shook):
        name = NAMES.get(op, f"OP{op}")
        if op == PING:
            return OK, b""
        if shook and not self.rate_allow(op, payload):
            self.refused += 1
            if self.refused <= 3 or self.refused % 25 == 0:
                self.log(f"{name} -> ERR_RATE_LIMITED (#{self.refused}) — "
                         f"over {RATE_PER_SEC}/s, burst {RATE_BURST}")
            return ERR_RATE_LIMITED, b""
        if op == HELLO:
            if len(payload) < 36:
                return ERR_PAYLOAD_INVALID, b""
            client = payload[4:36].split(b"\0")[0].decode("ascii", "replace")
            self.log(f"HELLO client={client} -> proto 1, caps 0x{CAPS:04x}, abs_max {ABS_MAX}")
            return OK, struct.pack("<HHIQII", 1, CAPS, ABS_MAX, BITMAP, 0x000300, 0) + struct.pack("<II", 0, 0)
        if not shook:
            return ERR_DENIED_BY_POLICY, b""
        if op in (KEY_TAP, KEY_SEQ, KEY_DOWN, KEY_UP):
            # Default-deny, exactly as the real broker does with no policy
            # file. muvor injects no keys; a shim that allowed them could
            # pass a test uictld would refuse.
            self.log(f"{name} -> ERR_DENIED_BY_POLICY (no key policy, by design)")
            return ERR_DENIED_BY_POLICY, b""
        if op == MOVE_ABS:
            x, y = struct.unpack("<ii", payload)
            px, py = self.p.move_abs(x, y)
            self.log(f"MOVE_ABS x={x} y={y} -> {px},{py} px")
            return OK, b""
        if op == MOVE_REL:
            dx, dy = struct.unpack("<ii", payload)
            px, py = self.p.move_rel(dx, dy)
            self.log(f"MOVE_REL dx={dx} dy={dy} -> {px},{py} px")
            return OK, b""
        if op == BUTTON:
            code, down, _ = struct.unpack("<HBB", payload)
            if down and code in self.held:
                self.log(f"BUTTON code={code} down=1 -> ERR_KEY_ALREADY_HELD "
                         f"(this connection holds {sorted(self.held)})")
                return ERR_KEY_ALREADY_HELD, b""
            if not down and code not in self.held:
                self.log(f"BUTTON code={code} down=0 -> ERR_KEY_NOT_HELD")
                return ERR_KEY_NOT_HELD, b""
            self.held.add(code) if down else self.held.discard(code)
            self.p.button(code, down)
            self.log(f"BUTTON code={code} down={down}"
                     + (f"   held={sorted(self.held)}" if self.held else ""))
            return OK, b""
        if op == SCROLL:
            v, h = struct.unpack("<ii", payload)
            self.p.scroll(v, h)
            self.log(f"SCROLL v={v} h={h}")
            return OK, b""
        if op == BATCH:
            count, _ = struct.unpack_from("<HH", payload)
            # **Validated before anything is applied, and seeded from what
            # this connection already holds** — uictld does exactly this, and
            # it is why a button left down refuses every click that follows.
            would = set(self.held)
            for i in range(count):
                sub, _r, a, b = struct.unpack_from("<HHii", payload, 4 + 12 * i)
                if sub != BUTTON:
                    continue
                if b == 1:
                    if a in would:
                        self.log(f"BATCH item {i} code={a} already held "
                                 f"{sorted(would)} -> ERR_KEY_ALREADY_HELD")
                        return ERR_KEY_ALREADY_HELD, b""
                    would.add(a)
                else:
                    if a not in would:
                        self.log(f"BATCH item {i} code={a} -> ERR_KEY_NOT_HELD")
                        return ERR_KEY_NOT_HELD, b""
                    would.discard(a)
            parts = []
            for i in range(count):
                sub, _r, a, b = struct.unpack_from("<HHii", payload, 4 + 12 * i)
                if sub == MOVE_ABS:
                    px, py = self.p.move_abs(a, b)
                    parts.append(f"MOVE_ABS({a},{b})->{px},{py}px")
                elif sub == MOVE_REL:
                    px, py = self.p.move_rel(a, b)
                    parts.append(f"MOVE_REL({a},{b})->{px},{py}px")
                elif sub == BUTTON:
                    self.held.add(a) if b else self.held.discard(a)
                    self.p.button(a, b)
                    parts.append(f"BUTTON({a},{'down' if b else 'up'})")
                elif sub == SCROLL:
                    self.p.scroll(a, b)
                    parts.append(f"SCROLL({a},{b})")
                else:
                    parts.append(f"OP{sub}(refused)")
            self.log(f"BATCH count={count} [{' '.join(parts)}]")
            return OK, b""
        return ERR_PAYLOAD_INVALID, b""


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--runtime", required=True, help="the XDG_RUNTIME_DIR to listen in")
    ap.add_argument("--bus", default="/tmp/nested-addr", help="file holding the nested bus address")
    ap.add_argument("--size", default="1280x720", help="the nested screen, WxH")
    ap.add_argument("--log", default=None)
    a = ap.parse_args()

    w, h = (int(v) for v in a.size.split("x"))
    address = open(a.bus).read().strip()
    path = os.path.join(a.runtime, "uictld.sock")
    log_path = a.log or os.path.join(a.runtime, "uictl.log")

    pointer = Pointer(address, w, h)
    server = Server(pointer, log_path)

    if os.path.exists(path):
        os.unlink(path)
    os.makedirs(a.runtime, exist_ok=True)
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.bind(path)
    os.chmod(path, 0o700)
    s.listen(8)
    print(f"fake-uictld: {path}  ->  nested {w}x{h}  (log {log_path})", flush=True)

    while True:
        conn, _ = s.accept()
        threading.Thread(target=server.handle, args=(conn,), daemon=True).start()


if __name__ == "__main__":
    sys.exit(main())
