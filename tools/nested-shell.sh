#!/usr/bin/env bash
# §5.4a's development loop, working. Run this instead of logging out.
#
# A newly installed extension is invisible until the session restarts: GNOME
# 48 scans the extensions directory at startup and never rescans, and on
# Wayland there is no Alt+F2 r. That makes every extension edit cost a
# logout — unless it is loaded into a nested shell on a private bus, which
# is what this does. The real session is never touched.
#
# It found the v5 work good before a logout was spent on it (§5.4h), and it
# is the reason §5.4b could settle the pixel-readback question without one.
#
#   bash tools/nested-shell.sh          install, launch, introspect
#   NESTED=$(cat /tmp/nested-addr)      then drive it:
#   DBUS_SESSION_BUS_ADDRESS="$NESTED" gdbus call --session \
#     --dest org.muvor.Shell --object-path /org/muvor/Shell \
#     --method org.muvor.Shell.<Method>
#   bash tools/nested-shell.sh stop     clean up
#
# WHAT IT CANNOT TEST BY ITSELF: key events. A nested shell has no window
# focus unless its own window is focused in the outer session (§5.4a, fact 4),
# so the hotkey, Tab-to-deepen and free mode's keys are unreachable from here.
# Every D-Bus method and every signal muvor emits *itself* is testable.
#
# **`tools/playtest.sh` lifts that restriction** (2026-08-25). mutter injects
# keys into its own session over org.gnome.Mutter.RemoteDesktop, so pointed
# at THIS shell's private bus it presses real keys at it — the hotkey, labels,
# Tab, tap versus hold, Escape. Use that when the question involves a key;
# use this when it does not, because this is much faster to start.
set -u
HERE="$(cd "$(dirname "$0")/.." && pwd)"
DST="$HOME/.local/share/gnome-shell/extensions/muvor@muvor.local"

if [ "${1:-start}" = "stop" ]; then
    pkill -f "gnome-shell --nested"
    [ -f /tmp/nested-pid ] && kill "$(cat /tmp/nested-pid)" 2>/dev/null
    rm -f /tmp/nested-addr /tmp/nested-pid
    echo "stopped"
    exit 0
fi

cp "$HERE/extension/extension.js" "$HERE/extension/metadata.json" "$DST/" || exit 1
echo "installed v$(python3 -c "import json;print(json.load(open('$DST/metadata.json'))['version'])")"

# A nested shell reads dconf directly and is never told when it changes
# (§5.4a, fact 3), so the keys are set BEFORE it starts, on its own bus.
eval "$(dbus-daemon --session --fork --print-address=1 --print-pid=1 | {
  read -r A; read -r P; echo "export ADDR='$A'; export PID=$P"; })"
echo "$ADDR" > /tmp/nested-addr
echo "$PID"  > /tmp/nested-pid
echo "bus $ADDR (pid $PID)"

DBUS_SESSION_BUS_ADDRESS="$ADDR" gsettings set org.gnome.shell disable-user-extensions false
DBUS_SESSION_BUS_ADDRESS="$ADDR" gsettings set org.gnome.shell enabled-extensions "['muvor@muvor.local']"
DBUS_SESSION_BUS_ADDRESS="$ADDR" MUTTER_DEBUG_DUMMY_MODE_SPECS=1280x720 \
    nohup gnome-shell --nested --wayland > /tmp/nested-shell.log 2>&1 &
echo "shell pid $!  (log: /tmp/nested-shell.log)"
sleep 9

echo "--- Version ---"
DBUS_SESSION_BUS_ADDRESS="$ADDR" gdbus call --session --dest org.muvor.Shell \
  --object-path /org/muvor/Shell --method org.freedesktop.DBus.Properties.Get \
  org.muvor.Shell Version
echo "--- interface ---"
DBUS_SESSION_BUS_ADDRESS="$ADDR" gdbus introspect --session --dest org.muvor.Shell \
  --object-path /org/muvor/Shell 2>/dev/null | grep -E "^      [A-Z]"
echo "--- JS errors ---"
grep -c "JS ERROR" /tmp/nested-shell.log
