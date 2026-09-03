#!/usr/bin/env bash
# start — the first thing to run in a session. Everything the old
# `~/curcom.txt 0` did, in the repo where it belongs.
#
#   bash tools/start.sh
#
# ~/curcom.txt WAS the muvor runbook and is NOT any more — it was replaced on
# 2026-08-24 with a DaVinci Resolve script. The old text is at
# ~/curcom.muvor.bak.txt. Do not run `bash ~/curcom.txt 0` expecting muvor.
#
# This is idempotent and starts nothing that is already running.

set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
STATE="${XDG_STATE_HOME:-$HOME/.local/state}/muvor"
EXT="$HOME/.local/share/gnome-shell/extensions/muvor@muvor.local"

say() { printf '%-14s %s\n' "$1" "$2"; }

# Measure the release binary only: ~/.cargo/bin/muvor via `cargo install
# --path .`. target/debug costs 3-7x more and every number taken there is
# fiction (plan.md M5, "the trap").
command -v muvor >/dev/null || {
    echo "muvor is not on PATH — run: cd '$REPO' && cargo install --path ."
    exit 1
}

gnome-extensions info muvor@muvor.local >/dev/null 2>&1 || {
    echo "The extension is not loaded in THIS session. Nothing below can work."
    echo "Check: gsettings get org.gnome.shell disable-user-extensions"
    exit 1
}

# ---- the two things that used to die with every session -------------------
# They are systemd user units now (tools/autostart-install.sh) and come up at
# login. This block is the backstop, and it is also what makes the script work
# on a session where the units were never installed.
if [ -f "$HOME/.config/systemd/user/muvor-daemon.service" ]; then
    systemctl --user start uictld.service muvor-daemon.service 2>/dev/null
    say "autostart" "systemd user units — $(systemctl --user is-active uictld.service) / $(systemctl --user is-active muvor-daemon.service)"
    # Keep the installed errscan copy in step with the repo (see
    # autostart-install.sh on why it is a copy).
    if ! cmp -s "$REPO/tools/errscan.sh" "$HOME/.local/bin/muvor-errscan"; then
        install -m 0755 "$REPO/tools/errscan.sh" "$HOME/.local/bin/muvor-errscan"
        say "errscan" "installed copy was stale — re-synced from the repo"
    fi
    # The units are COPIES, so a fix in packaging/ that was never re-installed
    # keeps firing at every login — and errscan cannot report it, because a
    # fingerprint recorded as fixed is never recorded again. That is exactly
    # how a `Documentation=` line with a `%20` in it survived from 2026-08-24
    # to 2026-08-25. systemd will say so on demand; ask it.
    UNITWARN=$(systemd-analyze --user verify \
        "$HOME/.config/systemd/user/muvor-daemon.service" \
        "$HOME/.config/systemd/user/uictld.service" 2>&1)
    if [ -n "$UNITWARN" ]; then
        say "units" "systemd COMPLAINS — re-run tools/autostart-install.sh:"
        printf '%s\n' "$UNITWARN" | sed 's/^/               /'
    fi
    for n in muvor-daemon.service uictld.service; do
        if ! cmp -s "$REPO/packaging/systemd/$n" "$HOME/.config/systemd/user/$n"; then
            say "units" "$n on disk differs from packaging/ — run tools/autostart-install.sh"
        fi
    done
else
    say "autostart" "NOT INSTALLED — run: bash tools/autostart-install.sh"
    mkdir -p "$STATE"
    pgrep -x uictld >/dev/null || { nohup "$HOME/projects/uictl/uictld" >>"$STATE/uictld.log" 2>&1 & sleep 1; }
    pgrep -f -x "muvor daemon" >/dev/null || { nohup muvor daemon >>"$STATE/muvord.log" 2>&1 & sleep 2; }
fi

# ---- a11y announce, a backstop since M5s-d --------------------------------
# With this false, applications emit NO AT-SPI events at all, the mirror is
# never warmed, and browsers expose no tree (§4.7a). The daemon claims it at
# startup and restores it on exit; this matters when the daemon is not up.
A11Y=$(gdbus call --session -d org.a11y.Bus -o /org/a11y/bus \
        -m org.freedesktop.DBus.Properties.Get org.a11y.Status IsEnabled 2>/dev/null)
case "$A11Y" in
    *true*) say "a11y announce" "enabled — apps will emit events" ;;
    *)
        say "a11y announce" "DISABLED — turning it on, or nothing below is warmed"
        gdbus call --session -d org.a11y.Bus -o /org/a11y/bus \
            -m org.freedesktop.DBus.Properties.Set org.a11y.Status IsEnabled "<true>" >/dev/null
        ;;
esac

muvor check >/dev/null 2>&1 || {
    echo
    echo "uictl is not answering. Look at $STATE/uictld.log and"
    echo "  systemctl --user status uictld.service"
}

# ---- the hotkey nobody else may hold --------------------------------------
# A duplicate accelerator grab fails SILENTLY (§5.2a), so ask gsettings rather
# than pressing keys and guessing.
ACCEL=$(dconf read /org/gnome/shell/extensions/muvor/hint 2>/dev/null)
ACCEL=${ACCEL:-"['<Alt>semicolon']"}
say "hotkey" "$ACCEL"
BARE=$(printf '%s' "$ACCEL" | sed "s/.*'\(.*\)'.*/\1/")
OTHERS=$(gsettings list-recursively 2>/dev/null | grep -F "'$BARE'" | grep -v 'shell.extensions.muvor' || true)
if [ -n "$OTHERS" ]; then
    say "" "CONTESTED — a duplicate grab is silent (§5.2a):"
    printf '%s\n' "$OTHERS" | sed 's/^/               /'
else
    say "" "uncontested"
fi

# ---- running vs on disk: the check that keeps costing a test cycle --------
LIVE=$(gdbus call --session -d org.muvor.Shell -o /org/muvor/Shell \
        -m org.freedesktop.DBus.Properties.Get org.muvor.Shell Version 2>/dev/null \
        | sed "s/.*'\(.*\)'.*/\1/")
DISK=$(python3 -c "import json;print(json.load(open('$EXT/metadata.json'))['version'])" 2>/dev/null)
say "shell half" "running ${LIVE:-unreachable}, on disk ${DISK:-?}"
if [ -n "$LIVE" ] && [ -n "$DISK" ] && [ "$LIVE" != "$DISK" ]; then
    cat <<BANNER

  ############################################################
  #  STOP. The extension on disk is NOT the one running.     #
  #  GNOME 48 scans extensions once at startup and never     #
  #  rescans, and Wayland has no Alt+F2 r. LOG OUT AND BACK  #
  #  IN before testing anything in the JS half, or you are   #
  #  testing the old code and will not be told.              #
  ############################################################

BANNER
fi

echo
echo "muvor status:"
muvor status 2>&1 | sed 's/^/  /'

# ---- errors, which are read before any new work ---------------------------
echo
bash "$REPO/tools/errscan.sh" 2>&1 | sed 's/^/  /'
OPEN=$(awk '/^## Open/{f=1;next} /^## /{f=0} f && /^### open/{n++} END{print n+0}' "$REPO/ERRORS.md" 2>/dev/null)
if [ "${OPEN:-0}" -gt 0 ]; then
    echo
    echo "  $OPEN OPEN ERROR(S) in ERRORS.md. Read them and fix them before new work."
fi
