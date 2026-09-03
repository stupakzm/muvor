#!/usr/bin/env bash
# Does muvor's modal grab close an open menu? (§5.1e)
#
# On Wayland a menu is an xdg_popup holding a grab. When gnome-shell takes a
# modal grab, mutter is entitled to send xdg_popup.popup_done and the client
# then destroys the popup — which looks exactly like "the menu disappeared".
#
# The other suspect is the ALT in the accelerator: Firefox dismisses menus on
# a bare Alt press, and Alt reaches the client before `;` completes the combo.
#
# This probe separates them by PRESSING NOTHING. It photographs the screen,
# puts the overlay up over D-Bus, photographs it again, and takes it down.
# No key is ever sent, so Alt cannot be the cause of anything seen here.
#
#   bash tools/menu-probe.sh [seconds-to-get-ready]
#
# Read the two PNGs it names:
#   menu in BEFORE, gone in AFTER  -> the GRAB closes it. pushModal is the
#                                     cause and §5.1e's rewrite is needed.
#   menu in BOTH                   -> the grab is innocent; the Alt modifier
#                                     is the cause and the fix is the accel.
set -u
READY=${1:-8}
OUT=${OUT:-/tmp/muvor-menu-probe}
mkdir -p "$OUT"

echo
echo "OPEN A MENU NOW — LibreWolf's hamburger menu, or right-click the page."
echo "Leave it open and DO NOT TOUCH THE KEYBOARD until this says done."
for i in $(seq "$READY" -1 1); do printf '\r  %2ds ' "$i"; sleep 1; done
printf '\r      \n'

muvor capture --out "$OUT/before.png" >/dev/null 2>&1 || { echo "capture failed"; exit 1; }

# The overlay, put up WITHOUT a keystroke. --dry-run so nothing can be
# clicked; the deadman takes it down even if this script dies.
muvor hint --dry-run --explain > "$OUT/hint.log" 2>&1 &
sleep 3
muvor capture --out "$OUT/after.png" >/dev/null 2>&1
muvor shell hide >/dev/null 2>&1
wait 2>/dev/null

echo "done — no key was pressed at any point."
echo "  before  $OUT/before.png   (menu open, no grab)"
echo "  after   $OUT/after.png    (grab active)"
grep -E "^window|targets|to labels" "$OUT/hint.log" | head -5
