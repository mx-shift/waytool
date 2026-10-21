#!/bin/sh
# End-to-end test of the Wayland backend on headless sway: two outputs
# (one at scale 1.5), the daemon, and checks that typing, screenshots
# and the pointer land where they should.
#
# Needs sway, foot, wev, dbus-run-session and waytool on PATH, and
# a throwaway environment (it starts its own compositor). CI runs it in
# a Fedora container; locally:
#
#   podman run --rm --cap-add SYS_NICE -v "$PWD":/src:ro \
#       -v "$PWD/target/release/waytool":/usr/local/bin/waytool:ro \
#       registry.fedoraproject.org/fedora:44 sh -c \
#       'dnf install -y sway foot wev dbus-daemon >/dev/null && /src/tests/integration/sway.sh'
#
# (sway carries a file capability, which a container may only exec with
# SYS_NICE allowed.)

set -eu

export XDG_RUNTIME_DIR=/run/wa-test
export LANG=C.UTF-8
mkdir -m 700 "$XDG_RUNTIME_DIR"
work=$(mktemp -d)
trap 'kill $(jobs -p) 2>/dev/null || true; rm -rf "$work"' EXIT

fail() {
    echo "FAIL: $*" >&2
    for log in sway daemon; do
        echo "--- $log.log ---" >&2
        cat "$work/$log.log" >&2 || true
    done
    exit 1
}

cat > "$work/sway.conf" <<EOF
output HEADLESS-1 resolution 1920x1080 position 0 0 scale 1.5
output HEADLESS-2 resolution 1280x720 position 1280 0
default_border none
EOF
WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_LIBINPUT_NO_DEVICES=1 WLR_HEADLESS_OUTPUTS=2 \
    sway -c "$work/sway.conf" > "$work/sway.log" 2>&1 &
for _ in $(seq 100); do
    [ -S "$XDG_RUNTIME_DIR/wayland-1" ] && break
    sleep 0.1
done
export WAYLAND_DISPLAY=wayland-1
SWAYSOCK=$(ls "$XDG_RUNTIME_DIR"/sway-ipc.*.sock) || fail "sway didn't start"
export SWAYSOCK

dbus-run-session -- waytool daemon > "$work/daemon.log" 2>&1 &
for _ in $(seq 100); do
    waytool ping > /dev/null 2>&1 && break
    sleep 0.1
done
waytool ping > /dev/null || fail "the daemon didn't start"

# The scaled output's stream index, from `monitors`.
scaled=$(waytool monitors | awk '/^--- monitor/ { i = $2 } $1 == "name" && $2 == "HEADLESS-1" { print i }')
scaled=$(echo "$scaled" | tr -dc 0-9)
[ -n "$scaled" ] || fail "monitors doesn't list HEADLESS-1"

echo "typing"
swaymsg focus output HEADLESS-1 > /dev/null
foot -e sh -c "stty -icanon -echo; cat > $work/typed.txt" > "$work/foot.log" 2>&1 &
sleep 1
text='Hello, World! @#$%^&*() ~<>?|" café ñ € 😀👍 Привет'
waytool type "$text"
sleep 0.5
[ "$(cat "$work/typed.txt")" = "$text" ] || fail "typed [$(cat "$work/typed.txt")], expected [$text]"

echo "screenshots"
(cd "$work" && waytool screenshot shot.png > /dev/null)
size() {
    # A PNG's width and height are big-endian at bytes 16-23.
    od -An -tu1 -j16 -N8 "$1" | awk '{ printf "%dx%d", $1*16777216+$2*65536+$3*256+$4, $5*16777216+$6*65536+$7*256+$8 }'
}
sizes=$(for f in "$work"/shot-*.png; do size "$f"; echo; done | sort | tr '\n' ' ')
[ "$sizes" = "1280x720 1920x1080 " ] || fail "screenshot sizes are $sizes"

echo "pointer"
stdbuf -oL wev > "$work/wev.log" 2>&1 &
sleep 1
swaymsg '[app_id=wev] fullscreen enable' > /dev/null
sleep 0.5
waytool move --stream "$scaled" 1910 1070
waytool click --button right
waytool move-global 100.5 200.25
sleep 0.5
grep -q 'x, y: 1273.33' "$work/wev.log" || fail "pixel (1910, 1070) at scale 1.5 didn't reach (1273.33, 713.33)"
grep -q 'button: 273 (right), state: 0' "$work/wev.log" || fail "no right click"
grep -q 'x, y: 100.500000, 200.250000' "$work/wev.log" || fail "move-global didn't reach (100.5, 200.25)"

echo "windows"
waytool windows | grep -q "^app_id.foot$" || fail "windows doesn't list foot"
waytool type-in foot " typed-in"
sleep 0.5
case "$(cat "$work/typed.txt")" in
*" typed-in") ;;
*) fail "type-in foot typed [$(cat "$work/typed.txt")]" ;;
esac
: > "$work/wev.log"
waytool click-in wev 10 20
sleep 0.5
grep -q 'x, y: 10.000000, 20.000000' "$work/wev.log" || fail "click-in wev 10 20 didn't land at (10, 20)"
grep -q 'button: 272 (left), state: 1' "$work/wev.log" || fail "click-in didn't click"

echo "rotated output"
swaymsg output HEADLESS-2 transform 90 > /dev/null
sleep 0.5
rm -f "$work"/shot-*.png
(cd "$work" && waytool screenshot shot.png > /dev/null)
sizes=$(for f in "$work"/shot-*.png; do size "$f"; echo; done | sort | tr '\n' ' ')
[ "$sizes" = "1920x1080 720x1280 " ] || fail "with HEADLESS-2 turned, screenshot sizes are $sizes"

echo "PASS"
