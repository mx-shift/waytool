#!/usr/bin/env bash
# Install + enable the waytool gnome-shell extension, plus the
# systemd user unit its Quick Settings toggle controls.
#
# Drops metadata.json + *.js into the per-user extension directory,
# then asks gnome-extensions to enable the UUID.  Installs
# waytool.service (pointing at this checkout's release binary)
# but leaves it off: the daemon only runs while switched on from the
# Quick Settings toggle.  gnome-shell on Wayland loads a new or changed
# extension only when it starts, so log out and back in afterwards.

set -eu

UUID="waytool@mxshift.com"
SRC="$(cd "$(dirname "$0")" && pwd)"
DEST="${HOME}/.local/share/gnome-shell/extensions/${UUID}"
BIN="$(cd "$SRC/.." && pwd)/target/release/waytool"
UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"

for f in metadata.json extension.js service.js; do
    if [[ ! -f "$SRC/$f" ]]; then
        echo "ERROR: install.sh expects to live next to metadata.json, extension.js and service.js" >&2
        exit 2
    fi
done

# This project used to be called wayland-agent: take out what an install
# under that name left, so the old extension and unit don't linger
# beside the new ones.
OLD_UUID="wayland-agent@mxshift.com"
if [[ -d "${HOME}/.local/share/gnome-shell/extensions/${OLD_UUID}" ]]; then
    gnome-extensions disable "$OLD_UUID" 2>/dev/null || true
    rm -rf "${HOME}/.local/share/gnome-shell/extensions/${OLD_UUID}"
    echo "Removed the old $OLD_UUID extension"
fi
if [[ -f "$UNIT_DIR/wayland-agent.service" ]]; then
    systemctl --user stop wayland-agent.service 2>/dev/null || true
    rm -f "$UNIT_DIR/wayland-agent.service"
    echo "Removed the old wayland-agent.service"
fi

mkdir -p "$DEST"
cp -f "$SRC/metadata.json" "$DEST/"
cp -f "$SRC/extension.js"  "$DEST/"
cp -f "$SRC/service.js"    "$DEST/"
echo "Installed extension files into $DEST"

if [[ ! -x "$BIN" ]]; then
    echo "WARNING: $BIN not built yet — run 'cargo build --release' before starting the daemon." >&2
fi
mkdir -p "$UNIT_DIR"
"$SRC/../systemd/make-unit.sh" "$BIN" > "$UNIT_DIR/waytool.service"
# Clear any login autostart left by an older install.sh.
rm -f "$UNIT_DIR/graphical-session.target.wants/wayland-agent.service"
if ! systemctl --user daemon-reload; then
    echo "WARNING: couldn't reach your systemd user manager; run 'systemctl --user daemon-reload' from your desktop session." >&2
fi
echo "Installed $UNIT_DIR/waytool.service (off until toggled on in Quick Settings)"

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
