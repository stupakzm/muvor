#!/bin/sh
# Install the muvor shell extension for the current user.
#
# A newly installed extension can be enabled without restarting the session.
# A *changed* one cannot, on Wayland: GJS caches the module, so editing this
# extension and re-enabling it re-runs the old code. Log out and back in
# after every edit, or test in a nested shell.
set -eu

UUID=muvor@muvor.local
SRC=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
DEST="${XDG_DATA_HOME:-$HOME/.local/share}/gnome-shell/extensions/$UUID"

mkdir -p "$DEST/schemas"
cp "$SRC/metadata.json" "$SRC/extension.js" "$SRC/stylesheet.css" "$DEST/"
cp "$SRC/schemas/"*.gschema.xml "$DEST/schemas/"
glib-compile-schemas "$DEST/schemas"

echo "installed to $DEST"
echo "enable with:  gnome-extensions enable $UUID"
