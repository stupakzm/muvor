#!/usr/bin/env bash
# autostart-install — make muvor start itself at login, and keep a log.
#
#   bash tools/autostart-install.sh            install and enable
#   bash tools/autostart-install.sh --status    say what is installed and live
#   bash tools/autostart-install.sh --remove    undo all of it
#
# WHAT THIS REPLACES. Two things died with every session and had to be started
# by hand — `uictld`, which owns /dev/uinput, and `muvor daemon`, which holds
# the mirror, focus and calibration. The daemon in particular has to be up
# BEFORE the windows you want hinted are activated (§2.5, M5s-c): it warms
# from window:activate and cannot hear an activation that happened before it
# existed. Starting it by hand after the desktop is up is therefore not the
# same thing as starting it at login, and this is the difference.
#
# WHY systemd USER UNITS and not ~/.config/autostart/*.desktop:
#   - Restart=on-failure, so a crash is not the end of the session
#   - OnFailure= writes the crash straight into ERRORS.md
#   - stdout and stderr go to a file AND the journal with no shell tricks
#   - `systemctl --user status muvor-daemon` answers "is it running" honestly,
#     which `pgrep -f "muvor daemon"` never quite did
#
# WHY THE UNITS ARE COPIED AND NOT SYMLINKED. This repo lives on a removable
# drive. A unit file symlinked into ~/.config/systemd/user that points at an
# unmounted /media/... is a unit that fails at every login, silently, until
# somebody plugs the drive back in. The copies work with the drive absent;
# only `errscan` needs the repo, and it exits 0 when it is not there.

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
UNITS="$HOME/.config/systemd/user"
BIN="$HOME/.local/bin"
STATE="${XDG_STATE_HOME:-$HOME/.local/state}/muvor"
NAMES=(uictld.service muvor-daemon.service muvor-errscan.service muvor-errscan.timer)

case "${1:-install}" in
--status)
    printf '%-28s %s\n' "unit" "state"
    for n in "${NAMES[@]}"; do
        installed=no; [ -f "$UNITS/$n" ] && installed=yes
        printf '%-28s installed=%-4s enabled=%-9s active=%s\n' "$n" "$installed" \
            "$(systemctl --user is-enabled "$n" 2>/dev/null || echo -)" \
            "$(systemctl --user is-active  "$n" 2>/dev/null || echo -)"
    done
    echo
    echo "logs:    $STATE"
    ls -la "$STATE" 2>/dev/null | sed 's/^/  /' || echo "  (none yet)"
    exit 0
    ;;
--remove)
    systemctl --user disable --now "${NAMES[@]}" 2>/dev/null || true
    for n in "${NAMES[@]}"; do rm -f "$UNITS/$n"; done
    rm -f "$BIN/muvor-errscan"
    systemctl --user daemon-reload
    echo "removed. Logs and ERRORS.md are left alone."
    exit 0
    ;;
install|"") ;;
*) echo "unknown argument: $1" >&2; exit 2 ;;
esac

# --- the binaries the units name must exist, or the login fails quietly -----
[ -x "$HOME/.cargo/bin/muvor" ] || {
    echo "muvor is not installed. Run: cd '$REPO' && cargo install --path ." >&2
    exit 1
}
[ -x "$HOME/projects/uictl/uictld" ] || {
    echo "uictld is not at ~/projects/uictl/uictld — nothing can click." >&2
    exit 1
}

mkdir -p "$UNITS" "$BIN" "$STATE"
install -m 0644 "$REPO"/packaging/systemd/*.service "$REPO"/packaging/systemd/*.timer "$UNITS/"
# errscan runs from a copy for the removable-drive reason above. `start.sh`
# re-syncs this copy on every run, so editing the repo version is enough.
install -m 0755 "$REPO/tools/errscan.sh" "$BIN/muvor-errscan"

systemctl --user daemon-reload
systemctl --user enable --now uictld.service muvor-daemon.service muvor-errscan.timer

echo
bash "$0" --status
echo
echo "The units are WantedBy=graphical-session.target, so they come up at"
echo "login from now on. A Rust change still needs 'cargo install --path .'"
echo "AND 'systemctl --user restart muvor-daemon' — the running daemon holds"
echo "the old binary, which is a five-minute trap."
