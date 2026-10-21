#!/bin/sh
# End-to-end test on a real KDE Plasma session: the portal backend
# (approving its consent dialog through AT-SPI), libei typing with Caps
# Lock on, the KWin script's windows, click-in on the client area, a
# screenshot, and a monitor scale change that re-establishes the
# session.
#
# Run it inside a Plasma session (a VM is fine: KWin composites with
# llvmpipe there) with foot, wev, kscreen-doctor, python3-pyatspi and
# waytool on PATH, and the KWin script installed (or
# WAYTOOL_KWIN_SCRIPT set). It needs the session's environment:
#
#   export DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/$(id -u)/bus
#   export WAYLAND_DISPLAY=wayland-0 XDG_RUNTIME_DIR=/run/user/$(id -u)
#
# It changes the first monitor's scale and puts it back.

set -eu

export LANG=C.UTF-8
work=$(mktemp -d)
output=
trap 'kill $(jobs -p) 2>/dev/null || true; pkill -f "^waytool daemon" || true;
      [ -n "$output" ] && kscreen-doctor "output.$output.scale.1" > /dev/null 2>&1;
      rm -rf "$work"' EXIT

fail() {
    echo "FAIL: $*" >&2
    echo "--- daemon.log ---" >&2
    cat "$work/daemon.log" >&2 || true
    exit 1
}

# Press a button in the portal's dialog, once it shows.
approve() {
    for _ in $(seq 60); do
        python3 - <<'EOF' && return 0
import sys, pyatspi
def find(acc):
    if acc.getRoleName() in ("push button", "button") and acc.name == "Approve":
        return acc
    for i in range(acc.childCount):
        hit = find(acc.getChildAtIndex(i))
        if hit:
            return hit
for app in pyatspi.Registry.getDesktop(0):
    if app is not None and app.name == "xdg-desktop-portal-kde":
        hit = find(app)
        if hit:
            hit.queryAction().doAction(0)
            sys.exit(0)
sys.exit(1)
EOF
        sleep 0.5
    done
    fail "no consent dialog to approve"
}

ready() {
    for _ in $(seq 100); do
        waytool ping > /dev/null 2>&1 && return 0
        sleep 0.2
    done
    fail "the daemon didn't come up"
}

# Qt apps only appear on the accessibility bus while it's enabled.
busctl --user set-property org.a11y.Bus /org/a11y/bus org.a11y.Status IsEnabled b true
pkill -f "^waytool daemon" || true

pkill -x foot || true
pkill -x wev || true
foot --title=wa-test -e sh -c "stty -icanon -echo; cat > $work/typed.txt" > /dev/null 2>&1 &
stdbuf -oL wev > "$work/wev.log" 2>&1 &
sleep 2
waytool daemon > "$work/daemon.log" 2>&1 &
approve
ready
grep -q "windows from KWin" "$work/daemon.log" || fail "the daemon didn't pick KWin for windows"

echo "typing"
text='Hello, World! @#$%^&*() ~<>?|"'
waytool type-in wa-test "$text"
sleep 0.5
[ "$(cat "$work/typed.txt")" = "$text" ] || fail "typed [$(cat "$work/typed.txt")]"

echo "caps lock"
waytool key Caps_Lock
waytool type " aB"
waytool key Caps_Lock
sleep 0.5
case "$(cat "$work/typed.txt")" in
*" aB") ;;
*) fail "with Caps Lock on, typed [$(cat "$work/typed.txt")]" ;;
esac

echo "text off the layout"
if err=$(waytool type "😀" 2>&1); then
    # Plasma 6.8+ takes it through libei.
    sleep 0.5
    case "$(cat "$work/typed.txt")" in
    *"😀") ;;
    *) fail "typed [$(cat "$work/typed.txt")] for an emoji" ;;
    esac
else
    case "$err" in
    *"no way to type it as text"*) ;;
    *) fail "emoji: $err" ;;
    esac
fi

echo "screenshot"
size() {
    od -An -tu1 -j16 -N8 "$1" | awk '{ printf "%dx%d", $1*16777216+$2*65536+$3*256+$4, $5*16777216+$6*65536+$7*256+$8 }'
}
(cd "$work" && waytool screenshot shot.png > /dev/null)
for shot in "$work"/shot*.png; do break; done
expect=$(waytool monitors | awk '$1 == "width" { w = $2 } $1 == "height" { h = $2 } $1 == "scale" { s = $2 } END { printf "%dx%d", w * s + 0.5, h * s + 0.5 }')
[ "$(size "$shot")" = "$expect" ] || fail "screenshot is $(size "$shot"), monitor is $expect"

echo "windows"
waytool windows | grep -q "^app_id.wev$" || fail "windows doesn't list wev"
# From somewhere else, so the click comes with a motion event.
waytool move-global 1 1
: > "$work/wev.log"
waytool click-in --client wev 10 20 > /dev/null
sleep 0.5
grep -q 'x, y: 10.000000, 20.000000' "$work/wev.log" || fail "click-in --client wev 10 20 missed"

echo "screen lock"
session=$(loginctl list-sessions --no-legend | awk '$4 == "seat0" { print $1; exit }')
loginctl lock-session "$session"
sleep 2
if waytool key a 2> /dev/null; then
    loginctl unlock-session "$session"
    fail "input went through to the locked screen"
fi
(cd "$work" && waytool screenshot locked.png > /dev/null) || fail "no screenshot while locked"
loginctl unlock-session "$session"
sleep 2
waytool key Shift_L || fail "input refused after unlocking"

echo "scale change"
output=$(waytool monitors | awk '$1 == "name" { print $2; exit }')
kscreen-doctor "output.$output.scale.1.5" > /dev/null
approve
sleep 2
ready
grep -q "re-established" "$work/daemon.log" || fail "the session wasn't re-established"
# From somewhere else, so the click comes with a motion event.
waytool move-global 1 1
: > "$work/wev.log"
waytool click-in --client wev 10 20 > /dev/null
sleep 0.5
grep -q 'x, y: 10.000000, 20.000000' "$work/wev.log" || fail "after the scale change, click-in missed"

echo "PASS"
