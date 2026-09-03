#!/usr/bin/env bash
# Photograph the overlay. The only way to see what muvor actually drew.
#
# The overlay holds a modal keyboard grab, so no screenshot key can reach it
# — but Capture() is a D-Bus call the shell answers WHILE the overlay is up.
# M6's capture is therefore the instrument that verifies M4/M5's overlay,
# which was the last thing in this project that needed somebody's eyes.
#
#   bash tools/overlay-shot.sh /tmp/shot.png            plain hint
#   bash tools/overlay-shot.sh /tmp/shot.png --deep     D18 tier 2 as well
#
# `muvor hint --measure` answers "how fast". This answers "what was drawn".
# Note the collision this has with §4.5b: a CV click hides the overlay before
# it validates, precisely so muvor does not photograph its own badges.
set -u
OUT="${1:?usage: overlay-shot.sh <out.png> [hint flags...]}"
shift
muvor hint --dry-run --explain "$@" > /tmp/overlay-shot.log 2>&1 &
sleep 4
muvor capture --out "$OUT" > /dev/null 2>&1
muvor shell hide > /dev/null 2>&1
wait 2>/dev/null
echo "wrote $OUT"
grep -E "deep |detect |to labels|window " /tmp/overlay-shot.log
