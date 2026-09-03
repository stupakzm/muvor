#!/usr/bin/env bash
# errscan — collect every NEW error muvor produced into ERRORS.md.
#
#   bash tools/errscan.sh              scan since the last scan
#   bash tools/errscan.sh --all        rescan this whole boot (still deduped)
#   bash tools/errscan.sh --since "2 hours ago"
#   bash tools/errscan.sh --dry-run    print what would be added, write nothing
#
# WHY THIS EXISTS. muvor has no version control and no CI, and the two things
# it depends on die with every session. An error that scrolls past in a
# journal nobody reads is an error that gets rediscovered from scratch three
# sessions later. So: everything is written down once, with a fingerprint, and
# **a fingerprint that has been recorded is never recorded again** — which is
# what makes it safe to run this on a five-minute timer.
#
# THE CONTRACT WITH THE NEXT SESSION: ERRORS.md is read FIRST, before plan.md,
# and anything under "## Open" is fixed before new work starts. See CLAUDE.md.
#
# Sources, because muvor's errors land in three different places:
#   1. journalctl --user, units muvor-daemon.service and uictld.service
#   2. journalctl --user, gnome-shell — where the extension's console.log and
#      any JS ERROR from the shell half go (§5.4a)
#   3. ~/.local/state/muvor/*.log — the units' own stdout/stderr
#
# Deduplication is by NORMALISED text: digits become #, paths and timestamps
# are stripped, so "panicked at src/main.rs:1231" and the same panic at a
# different line collapse to one entry. That is deliberate — the same fault
# reported twice is noise, and a genuinely new fault reads differently.

set -uo pipefail

REPO="${MUVOR_REPO:-/media/stupakzm/X9 Pro/repos/muvor}"
STATE="${XDG_STATE_HOME:-$HOME/.local/state}/muvor"
SEEN="$STATE/errscan.seen"
STAMP="$STATE/errscan.stamp"
ERRORS="$REPO/ERRORS.md"

SINCE=""
DRY=0
ALL=0
while [ $# -gt 0 ]; do
    case "$1" in
        --all)     ALL=1 ;;
        --since)   SINCE="$2"; shift ;;
        --dry-run) DRY=1 ;;
        -h|--help) sed -n '2,12p' "$0"; exit 0 ;;
        *) echo "errscan: unknown argument $1" >&2; exit 2 ;;
    esac
    shift
done

mkdir -p "$STATE"
touch "$SEEN"

# If the repo is not mounted there is nowhere to write, and the watermark must
# NOT advance — the journal still holds everything and the next run with the
# drive present picks it all up. Silent and exit 0: this runs on a timer, and
# a timer that fails every five minutes because a USB disk is unplugged is
# itself the kind of noise this file exists to prevent.
if [ ! -d "$REPO" ]; then
    exit 0
fi

if [ "$ALL" = 1 ]; then
    # This boot, not all history. The first version used "@0" and dredged up
    # five JS errors from OTHER extensions dating back three months — noise
    # that reads exactly like a muvor fault until you check the timestamp.
    SINCE="$(date -Is -d "@$(( $(date +%s) - $(cut -d. -f1 /proc/uptime) ))")"
elif [ -z "$SINCE" ]; then
    SINCE="$(cat "$STAMP" 2>/dev/null || echo "-1 hour")"
fi
NOW="$(date -Is)"

# ---------------------------------------------------------------- patterns --
# What counts as an error. Kept as one editable list on purpose: the cost of
# a pattern that is too broad is a noisy ERRORS.md, and the cost of one that
# is too narrow is a fault nobody ever sees. Tune it here.
ERR_RE='panicked at|JS ERROR|Gjs-CRITICAL|GLib-CRITICAL|Traceback|Segmentation fault|core-dumped|[Ff]ailed|[Ee]rror|ERROR|[Cc]annot |No such|not answering|refused|is not running|unreachable|timed out|Timed out|Assertion|assertion'

# Designed outcomes that are NOT faults. A refused click is muvor working:
# §4.5 refuses rather than clicking the wrong thing, and recording that as an
# error would bury the real ones within a day.
OK_RE='REFUSED — nothing was clicked|A refused click is annoying|muvor: cancelled|would not click this|^ *rate  *REFUSED'

collect() {
    # journal: the two units
    journalctl --user --since "$SINCE" --no-pager -o short-iso \
        -u muvor-daemon.service -u uictld.service 2>/dev/null \
        | sed 's/^/journal /'
    # journal: the shell half — the extension logs through gnome-shell
    #
    # muvor's lines only, and this is the second attempt at that. A bare
    # "JS ERROR" filter catches every other extension in the session — five
    # of them here, from months ago, indistinguishable from a muvor fault at
    # a glance. A context window around every muvor mention was worse: the
    # routine "muvor: typed as" lines dragged in whatever mutter happened to
    # warn about next, which is how two unrelated assertions arrived.
    #
    # So: muvor's own lines, plus exactly one context case. GJS prints
    # "JS ERROR: TypeError ..." on one line and the frames naming
    # extension.js on the next few, so a frame belonging to muvor's extension
    # is the one thing that earns a look backwards.
    local shelljournal
    shelljournal=$(journalctl --user --since "$SINCE" --no-pager -o short-iso \
        -u org.gnome.Shell@wayland.service -u gnome-shell.service 2>/dev/null)
    printf '%s\n' "$shelljournal" | grep -i -E 'muvor' | sed 's/^/shell /'
    printf '%s\n' "$shelljournal" \
        | grep -B3 -F 'muvor@muvor.local/extension.js' \
        | grep -E 'JS ERROR|Gjs-CRITICAL' | sed 's/^/shell /'
    # the units' own log files, which survive a journal that is not persistent
    for f in "$STATE"/*.log; do
        [ -e "$f" ] || continue
        sed "s|^|file:$(basename "$f") |" "$f"
    done
}

# Fingerprint: strip what varies between two reports of the same fault.
normalise() {
    sed -E \
        -e 's/^[a-z:.]+ //' \
        -e 's/[0-9]{4}-[0-9]{2}-[0-9]{2}T?[0-9:.,+-]*//g' \
        -e 's/\b[0-9]+\b/#/g' \
        -e 's/0x[0-9a-f]+/#/g' \
        -e 's/[[:space:]]+/ /g' \
        -e 's/^ //; s/ $//' \
    | tr 'A-Z' 'a-z'
}

added=0
tmp="$(mktemp)"
trap 'rm -f "$tmp" "$tmp.md"' EXIT

while IFS= read -r line; do
    printf '%s\n' "$line" | grep -q -E "$OK_RE" && continue
    norm="$(printf '%s' "$line" | normalise)"
    [ -z "$norm" ] && continue
    fp="$(printf '%s' "$norm" | sha1sum | cut -c1-12)"
    grep -q "^$fp " "$SEEN" 2>/dev/null && continue
    grep -q "^$fp " "$tmp" 2>/dev/null && continue
    printf '%s %s\n' "$fp" "$NOW" >> "$tmp"
    {
        printf '\n### open · %s\n\n' "${NOW%%+*}"
        printf '```\n%s\n```\n\n' "$line"
        printf '_fingerprint `%s` — first seen %s. Fix it, then move this whole\n' "$fp" "${NOW%%+*}"
        printf 'block under `## Fixed` with one line saying what the cause was._\n'
    } >> "$tmp.md"
    added=$((added + 1))
done < <(collect | grep -E "$ERR_RE")

if [ "$added" = 0 ]; then
    [ "$DRY" = 1 ] || printf '%s\n' "$NOW" > "$STAMP"
    echo "errscan: no new errors (scanned since $SINCE)"
    exit 0
fi

if [ "$DRY" = 1 ]; then
    echo "errscan: $added new error(s) — dry run, nothing written:"
    cat "$tmp.md"
    rm -f "$tmp.md"
    exit 0
fi

[ -f "$ERRORS" ] || cat > "$ERRORS" <<'HEADER'
# ERRORS.md — read this before plan.md

Written by `tools/errscan.sh`, appended to under the Open heading below.
Everything still open is fixed before new work starts; see CLAUDE.md.

## Open

## Fixed
HEADER

# Insert under "## Open" so the newest fault is the first thing read.
python3 - "$ERRORS" "$tmp.md" <<'PY'
import re, sys
errors, block = sys.argv[1], sys.argv[2]
body = open(errors).read()
new = open(block).read()
# The heading, not the first mention of it. The first version of this used
# body.find("## Open") and inserted into the sentence in the header that
# *names* the section, which split the file in half. Match a line.
m = re.search(r"^## Open[ \t]*$", body, re.M)
if not m:
    body = body.rstrip() + "\n\n## Open\n"
    m = re.search(r"^## Open[ \t]*$", body, re.M)
cut = m.end()
open(errors, "w").write(body[:cut] + "\n" + new + body[cut:])
PY

cat "$tmp" >> "$SEEN"
rm -f "$tmp.md"
printf '%s\n' "$NOW" > "$STAMP"
echo "errscan: $added new error(s) written to $ERRORS"
