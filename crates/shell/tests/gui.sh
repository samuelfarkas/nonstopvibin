#!/bin/sh
# The shell's tests that need a display: Xvfb, a session bus (dbus-run-session), the built
# window helper and a fake xdg-open that records what the shell opens. Needs WebKitGTK 4.1's
# development files, Xvfb and dbus-daemon. Run from the repository root; arguments go to both
# cargo commands, after which come test-name filters: `tests/gui.sh --release -- measure_idle`.
set -eu
bin=$(mktemp -d)
printf '#!/bin/sh\nprintf "%%s\\n" "$*" >> "$NSV_OPENED"\n' > "$bin/xdg-open"
chmod +x "$bin/xdg-open"
export NSV_OPENED="$bin/opened.log" PATH="$bin:$PATH" DISPLAY=:99
Xvfb :99 -screen 0 1280x800x24 -nolisten tcp >/dev/null 2>&1 &
xvfb=$!
trap 'kill $xvfb; rm -rf "$bin"' EXIT
for _ in $(seq 50); do [ -e /tmp/.X11-unix/X99 ] && break; sleep 0.1; done
cargo_args=""
while [ $# -gt 0 ] && [ "$1" != "--" ]; do cargo_args="$cargo_args $1"; shift; done
[ $# -gt 0 ] && shift
# shellcheck disable=SC2086
cargo build -p nsv-window $cargo_args
# shellcheck disable=SC2086
dbus-run-session -- cargo test -p nsv-shell $cargo_args -- --include-ignored --test-threads=1 "$@"
