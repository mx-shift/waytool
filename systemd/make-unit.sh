#!/bin/sh
# Print the waytool systemd user unit, with the daemon binary's
# absolute path filled in for @BIN@:
#
#     systemd/make-unit.sh /usr/bin/waytool > waytool.service
#
# Used by gnome-extension/install.sh and the packaging.

set -eu

if [ $# -ne 1 ]; then
    echo "usage: $0 /absolute/path/to/waytool" >&2
    exit 2
fi
bin=$1
case $bin in
    /*) ;;
    *) echo "$0: the binary path must be absolute: $bin" >&2; exit 2 ;;
esac
# The path goes inside double quotes in ExecStart=, where systemd would
# treat these specially.
case $bin in
    *'"'* | *\\* | *'%'* | *'$'*)
        echo "$0: can't put a path containing \", \\, % or \$ in a unit: $bin" >&2
        exit 2 ;;
esac

while IFS= read -r line || [ -n "$line" ]; do
    case $line in
        *@BIN@*) printf '%s\n' "${line%%@BIN@*}$bin${line#*@BIN@}" ;;
        *) printf '%s\n' "$line" ;;
    esac
done < "$(dirname "$0")/waytool.service.in"
