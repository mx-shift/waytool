#!/usr/bin/env bash
# Install + enable the waytool gnome-shell extension.
#
# Drops metadata.json + extension.js into the per-user extension
# directory, then asks gnome-extensions to enable the UUID.
# gnome-shell on Wayland loads a new or changed extension only when it
# starts, so log out and back in afterwards.

set -eu

UUID="waytool@mxshift.com"
SRC="$(cd "$(dirname "$0")" && pwd)"
DEST="${HOME}/.local/share/gnome-shell/extensions/${UUID}"

for f in metadata.json extension.js; do
    if [[ ! -f "$SRC/$f" ]]; then
        echo "ERROR: install.sh expects to live next to metadata.json and extension.js" >&2
        exit 2
    fi
done

# This project used to be called wayland-agent: take out the extension
# an install under that name left, so it doesn't linger beside the new
# one.
OLD_UUID="wayland-agent@mxshift.com"
if [[ -d "${HOME}/.local/share/gnome-shell/extensions/${OLD_UUID}" ]]; then
    gnome-extensions disable "$OLD_UUID" 2>/dev/null || true
    rm -rf "${HOME}/.local/share/gnome-shell/extensions/${OLD_UUID}"
    echo "Removed the old $OLD_UUID extension"
fi

mkdir -p "$DEST"
cp -f "$SRC/metadata.json" "$DEST/"
cp -f "$SRC/extension.js"  "$DEST/"
echo "Installed extension files into $DEST"

if command -v gnome-extensions >/dev/null 2>&1; then
    if gnome-extensions enable "$UUID" 2>/dev/null; then
        echo "Enabled $UUID"
    else
        echo "WARNING: couldn't enable $UUID yet — after logging back in, run 'gnome-extensions enable $UUID'." >&2
    fi
else
    echo "WARNING: gnome-extensions CLI not found; after logging back in, enable the extension in the Extensions app." >&2
fi
if [[ "$(gsettings get org.gnome.shell disable-user-extensions 2>/dev/null)" == true ]]; then
    echo "WARNING: user extensions are switched off for the whole session, so $UUID won't run. Turn them on with the switch at the top of the Extensions app, or: gsettings set org.gnome.shell disable-user-extensions false" >&2
fi

cat >&2 <<EOF

NOTE: GNOME on Wayland will not pick up the new extension until
gnome-shell reloads.  Log out and back in (or reboot) — then verify
with:
    gnome-extensions show $UUID | grep State

and, with the daemon running, \`waytool windows\`.  The
extension answers only the waytool daemon, so calling it
directly (gdbus, busctl) is refused.
EOF
