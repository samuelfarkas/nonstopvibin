#!/bin/sh
# Runs a command the way the Linux GUI tests need it: on a 1280x800 Xvfb screen, which the window
# helper's placement tests assume, inside its own D-Bus session. Needs Xvfb, xauth and
# dbus-daemon: `sh tests/headless.sh cargo test -p nsv-window`.
exec xvfb-run -a -s "-screen 0 1280x800x24" dbus-run-session -- "$@"
