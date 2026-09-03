#!/usr/bin/env bash
# M5s-d, first measurement: does the daemon's hint path get slower as its
# a11y connection accumulates events? (plan.md §2.5, §4.2-ii)
#
# The experiment holds everything constant except daemon age and event count:
#   - the same focused window throughout (nobody touches the desktop)
#   - the same provenance (no new window:activate, so the mirror does not
#     change state mid-series)
#   - the release binary, through the daemon, --measure so no human is needed
#
# Then one final sample with the daemon killed, as the in-process control.
#
# Usage: bash drain.sh [samples] [gap_seconds]

set -u
N=${1:-12}
GAP=${2:-10}
OUT=${3:-"$(dirname "$0")/daemon-vs-oneshot-$(date +%F-%H%M).tsv"}

field() { sed -n "s/^  $1 *\([0-9.]*\).*/\1/p" <<<"$2"; }

printf 'sample\tage_s\tevents\tfocus\tframe\tdetect\tdraw\tto_labels\tprovenance\tmode\n' > "$OUT"

start=$(date +%s)
for i in $(seq 1 "$N"); do
    ev=$(muvor status 2>/dev/null | sed -n 's/^  mirror  *\([0-9]*\) events seen/\1/p')
    s=$(timeout 30 muvor hint --measure --explain 2>&1)
    age=$(( $(date +%s) - start ))
    prov=$(sed -n 's/.*targets via \(.*\)/\1/p' <<<"$s" | head -1)
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\tdaemon\n' \
        "$i" "$age" "${ev:-?}" \
        "$(field focus "$s")" "$(field frame "$s")" "$(field detect "$s")" \
        "$(field draw "$s")" "$(sed -n 's/^  to labels *\([0-9.]*\).*/\1/p' <<<"$s")" \
        "${prov:-?}" >> "$OUT"
    tail -1 "$OUT"
    [ "$i" -lt "$N" ] && sleep "$GAP"
done

# The control: same window, same binary, no daemon.
pkill -x -f "muvor daemon" 2>/dev/null
sleep 1
for i in 1 2 3; do
    s=$(timeout 30 muvor hint --measure --explain 2>&1)
    prov=$(sed -n 's/.*targets via \(.*\)/\1/p' <<<"$s" | head -1)
    printf 'c%s\t0\t0\t%s\t%s\t%s\t%s\t%s\t%s\toneshot\n' \
        "$i" \
        "$(field focus "$s")" "$(field frame "$s")" "$(field detect "$s")" \
        "$(field draw "$s")" "$(sed -n 's/^  to labels *\([0-9.]*\).*/\1/p' <<<"$s")" \
        "${prov:-?}" >> "$OUT"
    tail -1 "$OUT"
    sleep 2
done

# Leave the session as we found it: a daemon must be running (§2.5).
nohup muvor daemon >/tmp/muvord.log 2>&1 &
sleep 2
echo
echo "daemon restarted; results in $OUT"
