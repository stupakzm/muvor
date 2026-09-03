#!/usr/bin/env bash
# playtest — a whole GNOME session muvor can be driven in, with real keys.
#
# WHY. `tools/nested-shell.sh` says at the top that the one thing it cannot
# test is key events, and that sentence has cost two logouts and several
# rounds of asking a human to press `Alt+;` and report back. It is no longer
# true. mutter injects keys into its own session over
# `org.gnome.Mutter.RemoteDesktop`; pointed at a nested shell's private bus
# it drives that session and cannot reach the real one.
#
#   bash tools/playtest.sh up            nested shell + a11y + apps + shim
#   bash tools/playtest.sh env           print the env to run muvor with
#   bash tools/playtest.sh tap           v8-1: hint, tap a label
#   bash tools/playtest.sh hold          v8-2: hint, hold the label's 2nd key
#   bash tools/playtest.sh race          §12.14: release the held key, and reach
#                                        for an action key, the way a hand does
#   bash tools/playtest.sh free          check 3: free mode, hjkl, real motion
#   bash tools/playtest.sh repeat        §12.16: a held action key is ONE click
#   bash tools/playtest.sh keys TOKENS   anything else (see playtest.py)
#   bash tools/playtest.sh down          stop everything
#
# THE FOUR PARTS
#   1. a nested gnome-shell on a private D-Bus, with muvor's extension loaded
#   2. an a11y bus of its own, so the tree muvor walks is the nested one
#   3. gnome-text-editor and gnome-calculator inside it, to have targets
#   4. `tools/fake-uictld.py` — uictl's wire protocol, injecting into the
#      nested session instead of `/dev/uinput`, so muvor's pointer NEVER
#      touches the host seat. §4.5's "a wrong click is unrecoverable" is
#      exactly what a harness must not risk on a live machine. It also
#      models the real broker's **rate limit** (50/s, burst 50, the
#      `interactive` class) and its **held-button arbitration**, per
#      connection, validating a batch from it exactly as uictld does. Until
#      2026-08-26 it answered OK however fast it was asked and tracked no
#      state at all, and so certified a movement mode that died on the desk
#      at the first refusal and a lost release that refused every click for
#      hours. `MUVOR_FAKE_RATE=5/5` dials the limiter to the untrusted floor
#      to exercise the backoff on purpose.
#
# WHAT IT PROVES, AND WHAT IT DOES NOT (measured 2026-08-25)
#   PROVES: the hotkey, label typing, Tab, tap versus hold (`Typed` versus
#   `TypedHold`), Escape and the key-focus restore, free/movement mode's keys,
#   §4.5's verdict on every target, and every byte muvor sends over uictl —
#   logged and assertable.
#   DOES NOT, second: **a second press of a key that is already down.**
#   mutter folds an injected press of a key its own state says is down into
#   the autorepeat stream, so §12.16's repair path cannot be reached from
#   here — measured 2026-08-27 with a `key-press-event` probe: `down:d
#   wait:700 down:d` produced one `flags=0` press and seventeen `flags=4`
#   repeats, and no second press at all. `Motion::repress` is proved by its
#   unit tests; what this harness proves is the half above it, that a repeat
#   is never mistaken for one (`playtest.sh repeat`).
#   DOES NOT: **where the pointer actually lands.** `tools/fake-uictld.py`
#   positions it with `NotifyPointerMotionRelative`, because absolute motion
#   wants a ScreenCast stream — and mutter puts relative motion through
#   pointer acceleration, so the pixel is not the one muvor asked for.
#   Measured 2026-08-26: `muvor calibrate` inside the sandbox solves an origin
#   of -320,-214 for a screen that begins at 0,0, and a click that §4.5
#   cleared on `Main Menu` was injected, landed elsewhere, and opened nothing.
#   Everything up to the injection is real; the landing pixel needs the real
#   desktop (§12.8).
#   THE OLD NOTE HERE said the a11y geometry inside the sandbox comes back at
#   0,0 and that §4.5 therefore refuses everything. That was muvor's own GTK4
#   coordinate fault, fixed 2026-08-26 — bounds are right in here now and 32
#   of 32 targets validate, which is why the harness can answer §4.5 at all.
set -uo pipefail

HERE="$(cd "$(dirname "$0")/.." && pwd)"
RT=/tmp/muvor-playtest          # short: an AF_UNIX path has ~108 bytes
BUSFILE=/tmp/nested-addr
SIZE=${PLAYTEST_SIZE:-1280x720}

usage() { sed -n '2,50p' "$0"; }

nested_env() {
    echo "export DBUS_SESSION_BUS_ADDRESS='$(cat $BUSFILE)'"
    echo "export XDG_RUNTIME_DIR='$RT'"
}

case "${1:-}" in
up)
    rm -f "$BUSFILE"
    rm -rf "$RT"; mkdir -p "$RT/at-spi"; chmod 700 "$RT"

    # **The sandbox gets its own XDG_RUNTIME_DIR from the very first process,
    # and that is what keeps the real session safe.** The nested D-Bus is a
    # plain `dbus-daemon --session`, so it activates `org.a11y.Bus` from the
    # ordinary service file — and the activated launcher inherits *its*
    # environment. Set the runtime dir only on the launcher and you lose the
    # race: on 2026-08-25 the activated one ran with the host's, took over
    # `/run/user/1000/at-spi/`, and deleted the real session's socket. Every
    # app already running lost its accessibility tree permanently, because
    # atk-bridge resolves the bus once at startup and never reconnects.
    #
    # `WAYLAND_DISPLAY` is an absolute path for the same reason: the nested
    # shell still has to reach the *host* compositor to have somewhere to
    # draw, and with the runtime dir moved it can no longer find it by name.
    export XDG_RUNTIME_DIR="$RT"
    export WAYLAND_DISPLAY="${HOST_WAYLAND:-/run/user/1000/wayland-0}"

    # NOT `|| exit`: nested-shell.sh ends with `grep -c "JS ERROR"`, which
    # exits 1 precisely when there are none. Judge it by what it produced.
    bash "$HERE/tools/nested-shell.sh" >/tmp/playtest-nested.log 2>&1
    if [ ! -s "$BUSFILE" ] || ! grep -q "TypedHold" /tmp/playtest-nested.log; then
        echo "playtest: nested shell failed — see /tmp/playtest-nested.log"; exit 1
    fi
    echo "JS errors      $(grep -c 'JS ERROR' /tmp/nested-shell.log)"
    ADDR="$(cat $BUSFILE)"
    echo "nested shell   up, v$(grep -o "'[0-9]*'" /tmp/playtest-nested.log | head -1 | tr -d "'") on $ADDR"

    # Its own a11y bus. Without this the detector walks the HOST's tree while
    # the compositor half answers about the nested one, which is the worst of
    # both and looks like a muvor bug.
    #
    # **`XDG_RUNTIME_DIR` MUST be the sandbox, and this is not a tidiness
    # point.** at-spi-bus-launcher puts its socket in `$XDG_RUNTIME_DIR/at-spi/`
    # and *takes over that directory*. Run with the host's runtime dir on
    # 2026-08-25 it deleted `/run/user/1000/at-spi/bus` — the real session's
    # accessibility socket — and every app already running lost its tree for
    # good, because atk-bridge resolves the bus once at startup and does not
    # reconnect. It cost the user a logout. The sandbox gets its own.
    DBUS_SESSION_BUS_ADDRESS="$ADDR" nohup /usr/libexec/at-spi-bus-launcher \
        --launch-immediately >/tmp/playtest-a11y.log 2>&1 &
    echo $! > "$RT/a11y.pid"
    sleep 3
    DBUS_SESSION_BUS_ADDRESS="$ADDR" gsettings set \
        org.gnome.desktop.interface toolkit-accessibility true 2>/dev/null
    A11Y="$(DBUS_SESSION_BUS_ADDRESS="$ADDR" gdbus call --session \
        --dest org.a11y.Bus --object-path /org/a11y/bus --method org.a11y.Bus.GetAddress \
        2>/dev/null | sed "s/.*unix:path=\([^,]*\).*/\1/")"
    echo "a11y bus       $A11Y"
    case "$A11Y" in
      "$RT"/*) ;;
      *) echo "playtest: REFUSING — the a11y bus is at $A11Y, outside $RT."
         echo "          That is the host's, and using it would damage the real session."
         exit 1 ;;
    esac
    [ -n "$A11Y" ] && ln -sf "$A11Y" "$RT/at-spi/bus"

    # Ask the shell what socket it took rather than assuming. With the
    # sandbox's own runtime dir there is no host compositor to collide with,
    # so it takes `wayland-0` — not the `wayland-1` it gets when it shares
    # `/run/user/1000`. Hardcoding either one breaks the other setup.
    NESTED_WL="$(grep -o "Wayland display name '[^']*'" /tmp/nested-shell.log \
        | tail -1 | sed "s/.*'\(.*\)'/\1/")"
    NESTED_WL="${NESTED_WL:-wayland-0}"
    echo "nested wayland $NESTED_WL"

    for app in gnome-text-editor gnome-calculator; do
        DBUS_SESSION_BUS_ADDRESS="$ADDR" WAYLAND_DISPLAY="$NESTED_WL" \
            GDK_BACKEND=wayland GTK_A11Y=atspi \
            nohup "$app" >"/tmp/playtest-$app.log" 2>&1 &
    done
    sleep 6
    echo "apps           gnome-text-editor, gnome-calculator"

    nohup python3 "$HERE/tools/fake-uictld.py" --runtime "$RT" --bus "$BUSFILE" \
        --size "$SIZE" >/tmp/playtest-uictld.log 2>&1 &
    sleep 2
    head -1 /tmp/playtest-uictld.log
    echo
    echo "drive it with:"
    nested_env | sed 's/^/  /'
    ;;

env) nested_env ;;

down)
    pkill -f fake-uictld.py
    pkill -f "gnome-shell --nested"
    # BY PID, never `pkill -f at-spi-bus-launcher` — that name matches the
    # HOST's launcher too, and killing it takes the real session's
    # accessibility with it.
    [ -f "$RT/a11y.pid" ] && kill "$(cat "$RT/a11y.pid")" 2>/dev/null
    bash "$HERE/tools/nested-shell.sh" stop >/dev/null 2>&1
    rm -rf "$RT"
    echo "playtest: down"
    ;;

tap|hold|race|free|repeat|keys)
    [ -f "$BUSFILE" ] || { echo "playtest: not up — bash tools/playtest.sh up"; exit 1; }
    eval "$(nested_env)"
    : > "$RT/uictl.log"
    case "$1" in
      tap)  SCEN=(--on-show 'tap:{c0} tap:{c1}' wait:1500) ; CMD=(muvor hint) ;;
      # 220 ms is 40 ms past HOLD_MS, which is what a hand does. It used to
      # be 700, and **that is why this scenario certified §12.14's fault**:
      # the release landed 520 ms after the gesture resolved, by which time
      # muvor had finished a 9 ms validate and opened its key stream. A
      # harness whose timings are all generous cannot find a race.
      hold) SCEN=(--on-show 'tap:{c0} hold:{c1}:220' wait:400 hold:l:600 wait:200 tap:Tab)
            CMD=(muvor hint) ;;
      # §12.14, both halves in one run. Expect `1 click(s)` and a pointer
      # that moved: the entering key comes back to life once its release is
      # seen (tap `{c1}`), and the first action key after the gesture is not
      # eaten. Before the fix this printed `0 click(s)`.
      # Every token is inside --on-show because only that string gets
      # `{c1}` substituted; the trailing ones are passed through verbatim.
      race) SCEN=(--on-show 'tap:{c0} hold:{c1}:220 wait:120 tap:{c1} wait:300
                             hold:l:400 wait:200 tap:Tab' wait:100)
            CMD=(muvor hint) ;;
      free) SCEN=(wait:2000 hold:l:700 wait:300 hold:j:700 wait:300 tap:Escape)
            CMD=(muvor free) ;;
      # §12.16. `d` held for 700 ms is ONE double click, because extension
      # v10 reports autorepeat as `KeyRepeat` and movement mode is
      # tick-driven. Expect `1 click(s)` and **two** BATCHes.
      #
      # This is the scenario that catches a repeat filter which has silently
      # stopped filtering — the shape the fault took on 2026-08-27 was one
      # wrong word in an enum member name, `EventFlags.REPEATED` where GJS
      # calls it `FLAG_REPEATED`, and `x & undefined` is 0 in JavaScript. No
      # error anywhere; the same run printed 9 clicks and 7 false repairs
      # where two clicks were asked for. A held key is the one input every
      # other scenario here avoids.
      repeat) SCEN=(--on-show 'tap:{c0} hold:{c1}:220 wait:400
                               hold:d:700 wait:300 tap:Tab' wait:100)
            CMD=(muvor hint) ;;
      keys) shift; SCEN=("$@") ; CMD=(true) ;;
    esac
    ( timeout 45 python3 "$HERE/tools/playtest.py" "${SCEN[@]}" >/tmp/playtest-drv.log 2>&1 & )
    sleep 1
    timeout 40 "${CMD[@]}" 2>&1 | tail -20
    echo "--- driver ---";      cat /tmp/playtest-drv.log
    echo "--- uictl saw ---";   cat "$RT/uictl.log"
    ;;

*) usage ;;
esac
