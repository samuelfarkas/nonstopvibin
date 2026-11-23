#!/bin/sh
# The UI checks (crates/window/tests/ui) in the real window helper. Builds the interface, the
# fixture pages, the daemon and the helper, then runs the checks; needs `bun install`,
# `bun run core:install` (.vendor/core) and, on Linux, WebKitGTK 4.1's development files, Xvfb
# and dbus-daemon. On macOS the terminal running it needs the Accessibility permission (System
# Events plays the user). Run from the repository root; arguments go to cargo, after which
# come test-name filters: `crates/window/tests/ui.sh --locked -- focus`.
set -eu
cargo_args=""
while [ $# -gt 0 ] && [ "$1" != "--" ]; do cargo_args="$cargo_args $1"; shift; done
[ $# -gt 0 ] && shift
bunx --no-install vite build --logLevel warn
bun crates/window/tests/ui/fixtures/build.mjs
# shellcheck disable=SC2086
cargo build -p nsv-cli -p nsv-window $cargo_args
if [ "$(uname)" = Darwin ]; then
  # shellcheck disable=SC2086
  exec cargo test -p nsv-window --test ui $cargo_args -- --test-threads=1 "$@"
fi
# A screen as wide as the main window, as tests/gui.sh uses.
export DISPLAY=:98
Xvfb :98 -screen 0 1280x1024x24 -nolisten tcp >/dev/null 2>&1 &
xvfb=$!
trap 'kill $xvfb' EXIT
for _ in $(seq 50); do [ -e /tmp/.X11-unix/X98 ] && break; sleep 0.1; done
# shellcheck disable=SC2086
dbus-run-session -- cargo test -p nsv-window --test ui $cargo_args -- --test-threads=1 "$@"
