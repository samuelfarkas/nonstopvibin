#!/bin/sh
# `cargo <command> [arguments]` over the whole workspace where the window helper builds (macOS,
# or Linux with WebKitGTK 4.1's development files), else over Cargo.toml's default-members.
set -eu
command=$1
shift
if [ "$(uname)" = Darwin ] || pkg-config --exists webkit2gtk-4.1 2>/dev/null; then
  set -- --workspace "$@"
fi
exec cargo "$command" "$@"
