#!/bin/sh
# End-to-end test of the Wayland backend on labwc, nested in headless
# sway: typing, a screenshot, and windows through
# wlr-foreign-toplevel-management, labwc having no IPC of its own —
# including focusing a window that doesn't have focus (type-in).
#
# Needs sway, labwc, foot, wev, dbus-run-session and waytool on
# PATH; run it like sway.sh, with labwc installed as well.

set -eu

export XDG_RUNTIME_DIR=/run/wa-test
export LANG=C.UTF-8
mkdir -m 700 "$XDG_RUNTIME_DIR"
work=$(mktemp -d)
trap 'kill $(jobs -p) 2>/dev/null || true; rm -rf "$work"' EXIT

fail() {
    echo "FAIL: $*" >&2
    for log in labwc daemon; do
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

WAYLAND_DISPLAY=wayland-1 WLR_RENDERER=pixman labwc > "$work/labwc.log" 2>&1 &
# labwc takes the first free display name, whichever that is.
nested=
for _ in $(seq 100); do
    for sock in "$XDG_RUNTIME_DIR"/wayland-?; do
        [ -S "$sock" ] && [ "${sock##*/}" != wayland-1 ] && nested=${sock##*/}
    done
    [ -n "$nested" ] && break
    sleep 0.1
done
[ -n "$nested" ] || fail "labwc didn't start"
export WAYLAND_DISPLAY="$nested"

foot -e sh -c "stty -icanon -echo; cat > $work/typed.txt" > "$work/foot.log" 2>&1 &
sleep 1
env -u SWAYSOCK dbus-run-session -- waytool daemon > "$work/daemon.log" 2>&1 &
for _ in $(seq 100); do
    waytool ping > /dev/null 2>&1 && break
    sleep 0.1
done
waytool ping > /dev/null || fail "the daemon didn't start"
grep -q "windows from wlr-foreign-toplevel" "$work/daemon.log" || fail "the daemon didn't pick wlr-foreign-toplevel"

echo "typing"
text='Hello, labwc! café € 😀 Привет <>'
waytool type "$text"
sleep 0.5
[ "$(cat "$work/typed.txt")" = "$text" ] || fail "typed [$(cat "$work/typed.txt")], expected [$text]"

echo "screenshot"
(cd "$work" && waytool screenshot shot.png > /dev/null)
[ -s "$work/shot.png" ] || fail "no screenshot"

echo "windows"
wev > /dev/null 2>&1 &
sleep 1
waytool windows | grep -q "^app_id.foot$" || fail "windows doesn't list foot"
waytool type-in foot " typed-in"
sleep 0.5
case "$(cat "$work/typed.txt")" in
*" typed-in") ;;
*) fail "type-in foot typed [$(cat "$work/typed.txt")]" ;;
esac

echo "PASS"
