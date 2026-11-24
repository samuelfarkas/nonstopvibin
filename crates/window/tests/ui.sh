#!/bin/sh
# The UI checks (crates/window/tests/integration/ui) in the real window helper, and on Linux the
# desktop app's error boxes and signals (integration/app.rs). Builds the interface, the
# fixture pages, the daemon and the helper, then runs the checks; needs `bun install`,
# `bun run core:install` (.vendor/core) and, on Linux, WebKitGTK 4.1's development files and what
# tests/headless.sh needs. On macOS the terminal running it needs the Accessibility permission (System
# Events plays the user). Run from the repository root; arguments go to cargo, after which
# come test-name filters: `crates/window/tests/ui.sh --locked -- focus`.
set -eu
cargo_args=""
while [ $# -gt 0 ] && [ "$1" != "--" ]; do cargo_args="$cargo_args $1"; shift; done
[ $# -gt 0 ] && shift
bunx --no-install vite build --logLevel warn
bun crates/window/tests/integration/ui/fixtures/build.mjs
# shellcheck disable=SC2086
cargo build -p nsv-cli -p nsv-window $cargo_args
if [ "$(uname)" = Darwin ]; then
  # shellcheck disable=SC2086
  exec cargo test -p nsv-window --test integration $cargo_args -- --include-ignored --test-threads=1 "$@"
fi
# Not the helper's tests, which `cargo test` runs, nor the measurement, which runs on demand.
# shellcheck disable=SC2086
exec sh tests/headless.sh cargo test -p nsv-window --test integration $cargo_args -- --include-ignored --test-threads=1 --skip helper:: --skip measure:: "$@"
