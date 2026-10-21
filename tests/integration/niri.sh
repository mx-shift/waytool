#!/bin/sh
# End-to-end test of the Wayland backend on niri, nested in headless
# sway (niri has no headless backend of its own): typing, screenshots
# (niri offers only wlr-screencopy), and the window provider through
# niri's IPC. Pointer positions aren't checked: nested niri's output is
# flipped (winit), and niri maps virtual-pointer positions through it.
#
# Needs sway, niri, foot, dbus-run-session and waytool on PATH;
# run it like sway.sh, with niri installed as well.

set -eu

export XDG_RUNTIME_DIR=/run/wa-test
export LANG=C.UTF-8
mkdir -m 700 "$XDG_RUNTIME_DIR"
work=$(mktemp -d)
trap 'kill $(jobs -p) 2>/dev/null || true; rm -rf "$work"' EXIT

fail() {
    echo "FAIL: $*" >&2
    for log in niri daemon; do
        echo "--- $log.log ---" >&2
        cat "$work/$log.log" >&2 || true
    done
    exit 1
}

wait_for() {
    for _ in $(seq 100); do
        [ -e "$1" ] && return 0
        sleep 0.1
    done
    fail "$1 didn't appear"
}

printf 'output HEADLESS-1 resolution 1920x1080\n' > "$work/sway.conf"
WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_LIBINPUT_NO_DEVICES=1 \
    sway -c "$work/sway.conf" > "$work/sway.log" 2>&1 &
wait_for "$XDG_RUNTIME_DIR/wayland-1"

mkdir -p "$HOME/.config/niri"
printf 'animations {\n    off\n}\n' > "$HOME/.config/niri/config.kdl"
WAYLAND_DISPLAY=wayland-1 LIBGL_ALWAYS_SOFTWARE=1 niri > "$work/niri.log" 2>&1 &
wait_for "$XDG_RUNTIME_DIR/wayland-2"
export WAYLAND_DISPLAY=wayland-2

foot -e sh -c "stty -icanon -echo; cat > $work/typed.txt" > "$work/foot.log" 2>&1 &
sleep 1
# No NIRI_SOCKET or SWAYSOCK: the daemon has to find niri's socket by
# the compositor it's connected to, past the outer sway's.
env -u NIRI_SOCKET -u SWAYSOCK dbus-run-session -- waytool daemon > "$work/daemon.log" 2>&1 &
for _ in $(seq 100); do
    waytool ping > /dev/null 2>&1 && break
    sleep 0.1
done
waytool ping > /dev/null || fail "the daemon didn't start"
grep -q "windows from niri" "$work/daemon.log" || fail "the daemon didn't pick niri's IPC"

echo "typing"
text='Hello, niri! café € 😀 Привет <>'
waytool type "$text"
sleep 0.5
[ "$(cat "$work/typed.txt")" = "$text" ] || fail "typed [$(cat "$work/typed.txt")], expected [$text]"

echo "screenshot"
(cd "$work" && waytool screenshot shot.png > /dev/null)
[ -s "$work/shot.png" ] || fail "no screenshot"

echo "windows"
waytool windows | grep -q "^app_id.foot$" || fail "windows doesn't list foot"
waytool type-in foot " typed-in"
sleep 0.5
case "$(cat "$work/typed.txt")" in
*" typed-in") ;;
*) fail "type-in foot typed [$(cat "$work/typed.txt")]" ;;
esac

echo "PASS"
