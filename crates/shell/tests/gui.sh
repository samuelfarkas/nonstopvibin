#!/bin/sh
# The shell's tests that need a display: tests/headless.sh, the built window helper and a fake
# xdg-open that records what the shell opens. Needs WebKitGTK 4.1's development files and what
# tests/headless.sh needs. Run from the repository root; arguments go to both cargo commands,
# after which come test-name filters: `tests/gui.sh --release -- measure_idle`.
set -eu
bin=$(mktemp -d)
printf '#!/bin/sh\nprintf "%%s\\n" "$*" >> "$NSV_OPENED"\n' > "$bin/xdg-open"
chmod +x "$bin/xdg-open"
export NSV_OPENED="$bin/opened.log" PATH="$bin:$PATH"
trap 'rm -rf "$bin"' EXIT
cargo_args=""
while [ $# -gt 0 ] && [ "$1" != "--" ]; do cargo_args="$cargo_args $1"; shift; done
[ $# -gt 0 ] && shift
# The idle measurement takes a minute and asserts nothing: it runs only when a filter names it.
[ $# -gt 0 ] || set -- --skip measure_idle
# shellcheck disable=SC2086
cargo build -p nsv-window $cargo_args
# shellcheck disable=SC2086
sh tests/headless.sh cargo test -p nsv-shell $cargo_args -- --include-ignored --test-threads=1 "$@"
