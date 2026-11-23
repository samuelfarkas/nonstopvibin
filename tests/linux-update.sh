#!/bin/sh
# An installed AppImage updates itself in place: builds this checkout twice with
# scripts/package-linux.ts, as 0.0.1 and as package.json's version, both pointed at a test feed
# on 127.0.0.1 (NONSTOPVIBIN_TEST_UPDATE_FEED) and holding a throwaway release key in
# build/update-signing.pub. The newer release is checksummed and signed as release.yml does it.
# The old AppImage must refuse a tampered payload and a foreign signature, then take the real
# release: "Restart to update" in its dialog, the file replaced at the same path, and the new
# version serving the same data folder.
#
# Run it in a Linux container or VM with WebKitGTK 4.1 and GTK 3 development files, Xvfb,
# xdotool, musl-tools, python3, bun, cargo with the x86_64-unknown-linux-musl target, after
# `bun run build` and `bun run setup` in the checkout (`bun run test:update:linux`), for example:
#   docker run --rm -v ~/.local/rust:/rust -v "$PWD":/src:ro -v <bun>:/usr/local/bin/bun:ro \
#     -v <target dir>:/target -e CARGO_TARGET_DIR=/target <image> sh /src/tests/linux-update.sh
set -eu
src=$(cd "$(dirname "$0")/.." && pwd)
port=${NSV_UPDATE_TEST_PORT:-48731}
new=$(sed -n 's/^  "version": "\(.*\)",$/\1/p' "$src/package.json")
old=0.0.1
work=$(mktemp -d)
log=$work/app.log
apps=$work/Applications
image=$apps/NonstopVibin.AppImage
# The app gets its own HOME; the toolchain stays where it was installed.
export RUSTUP_HOME=${RUSTUP_HOME:-$HOME/.rustup} CARGO_HOME=${CARGO_HOME:-$HOME/.cargo}
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$work/target} DISPLAY=:99 HOME=$work/home
export APPIMAGE_EXTRACT_AND_RUN=1 NONSTOPVIBIN_DATA_DIR=$work/data NONSTOPVIBIN_PORT=0
mkdir -p "$work/src" "$work/feed" "$apps" "$HOME"
mkdir -m 700 "$NONSTOPVIBIN_DATA_DIR"

pids=""
app=""
cleanup() {
  status=$?
  set +e
  # shellcheck disable=SC2086
  [ -n "$pids" ] && kill $pids 2>/dev/null
  [ -n "$app" ] && /bin/kill -s KILL -- "-$app" 2>/dev/null
  if [ "$status" != 0 ] && [ -f "$log" ]; then
    echo "--- app log"
    cat "$log"
  fi
  rm -rf "$work"
  exit "$status"
}
trap cleanup EXIT
fail() {
  echo "FAIL: $*" >&2
  exit 1
}
# Polls `$@` every half second for up to $limit seconds.
wait_for() {
  what=$1
  limit=$2
  shift 2
  end=$(($(date +%s) + limit))
  until "$@"; do
    [ "$(date +%s)" -lt "$end" ] || fail "timed out waiting for $what"
    sleep 0.5
  done
}

Xvfb :99 -screen 0 1280x800x24 -nolisten tcp >/dev/null 2>&1 &
pids="$pids $!"

# A copy whose version and key this test may change; the checkout's node_modules serve it.
tar -C "$src" --exclude=./target --exclude=./node_modules --exclude=./.git \
  --exclude=./release --exclude=./build/update-signing.pub -cf - . | tar -C "$work/src" -xf -
ln -s "$src/node_modules" "$work/src/node_modules"
cd "$work/src"
minisign=$(bun -e 'import { releaseTool } from "./scripts/release-tools.ts"; console.log(await releaseTool("minisign"))')
"$minisign" -G -W -p build/update-signing.pub -s "$work/release.key" >/dev/null
"$minisign" -G -W -p "$work/foreign.pub" -s "$work/foreign.key" >/dev/null

# Builds the AppImage at version $1 into $2. Fat LTO adds minutes and proves nothing here.
package() {
  python3 - "$1" <<'EOF'
import re, sys
version = sys.argv[1]
def rewrite(path, pattern, replacement, count=0):
    text = open(path).read()
    text, n = re.subn(pattern, replacement, text, count=count, flags=re.M)
    assert n, (path, pattern)
    open(path, "w").write(text)
rewrite("package.json", r'^  "version": ".*",$', f'  "version": "{version}",', 1)
rewrite("Cargo.toml", r'^(\[workspace\.package\]\nversion = )".*"', rf'\1"{version}"')
rewrite("Cargo.lock", r'^(name = "nsv-[a-z]+"\nversion = )".*"', rf'\1"{version}"')
EOF
  NONSTOPVIBIN_TEST_UPDATE_FEED=http://127.0.0.1:$port/ CARGO_PROFILE_RELEASE_LTO=off \
    CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 bun scripts/package-linux.ts >"$work/package-$1.log" 2>&1 ||
    { tail -40 "$work/package-$1.log"; fail "packaging $1"; }
  mkdir -p "$2"
  mv release/*-"$1"-* release/latest-linux.yml "$2/"
}
package "$old" "$work/old"
package "$new" "$work/new"
payload=NonstopVibin-$new-linux-x86_64.AppImage
[ -f "$work/new/$payload" ] || fail "no $payload"
MINISIGN_SECRET_KEY=$(cat "$work/release.key") bun scripts/sign-release.ts "$work/new" >/dev/null

release=$work/feed/releases/download/v$new
cat >"$work/feed/releases.atom" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom"><entry>
<link rel="alternate" type="text/html" href="https://github.com/samuelfarkas/nonstopvibin/releases/tag/v$new"/>
</entry></feed>
EOF
(cd "$work/feed" && exec python3 -m http.server "$port" --bind 127.0.0.1 >/dev/null 2>&1) &
pids="$pids $!"
publish() {
  rm -rf "$release"
  mkdir -p "$release"
  cp "$work/new"/* "$release/"
}

cp "$work/old/NonstopVibin-$old-linux-x86_64.AppImage" "$image"
before=$(sha256sum <"$image")
# The AppImage runtime runs the daemon as its child: each launch gets a process group, and
# stop() ends all of it, the daemon gracefully.
launch() {
  : >"$log"
  setsid "$image" --background >>"$log" 2>&1 &
  app=$!
}
stop() {
  /bin/kill -s TERM -- "-$app" 2>/dev/null || true
  wait_for "the app to stop" 30 sh -c "! /bin/kill -0 -- -$app 2>/dev/null"
  wait "$app" 2>/dev/null || true
}
logged() { grep -q "$1" "$log"; }

# The first check runs 30 s after start; a refused release leaves the app as it was.
publish
printf 'x' | dd of="$release/$payload" bs=1 seek=4096 conv=notrunc 2>/dev/null
launch
wait_for "the tampered payload to be refused" 90 logged "the update does not match SHA256SUMS"
stop
[ "$(sha256sum <"$image")" = "$before" ] || fail "a tampered update replaced the app"
echo "ok: a payload that does not match the signed SHA256SUMS is refused"

publish
"$minisign" -S -s "$work/foreign.key" -m "$release/SHA256SUMS" -x "$release/SHA256SUMS.minisig" \
  -t "NonstopVibin $new SHA256SUMS" >/dev/null
launch
wait_for "the foreign signature to be refused" 90 logged "SHA256SUMS is not signed by the release key"
stop
[ "$(sha256sum <"$image")" = "$before" ] || fail "a foreign signature replaced the app"
echo "ok: SHA256SUMS signed by another key is refused"

publish
launch
dialog() { xdotool search --name '^NonstopVibin$' >"$work/window" 2>/dev/null; }
wait_for "the update dialog" 120 dialog
window=$(head -n 1 "$work/window")
# "Later" is the default; the button before it is "Restart to update".
xdotool windowfocus --sync "$window"
xdotool key shift+Tab space
wait "$app" || fail "the old app exited with $?"
echo "ok: $old accepted the update and quit with 0"
[ "$(sha256sum <"$image")" = "$(sha256sum <"$work/new/$payload")" ] || fail "the AppImage was not replaced"
[ -x "$image" ] || fail "the update is not executable"
# The authenticated probe `url` makes, read from the data folder: running the AppImage to ask
# would extract it again (APPIMAGE_EXTRACT_AND_RUN) while the relaunch extracts it, and the
# relaunch then fails with "Text file busy".
running() {
  python3 - "$NONSTOPVIBIN_DATA_DIR/server.json" "$new" "$work/pid" <<'EOF'
import json, sys, urllib.request
path, version, out = sys.argv[1:]
try:
    session = json.load(open(path))
    request = urllib.request.Request(
        f"http://127.0.0.1:{session['port']}/api/state",
        headers={"Authorization": f"Bearer {session['token']}"},
    )
    state = json.load(urllib.request.urlopen(request, timeout=2))
except (OSError, ValueError, KeyError):
    sys.exit(1)
if session["version"] != version or state["version"] != version:
    sys.exit(1)
open(out, "w").write(str(session["pid"]))
EOF
}
wait_for "$new to serve the data folder" 60 running
pid=$(cat "$work/pid")
pids="$pids $pid"
gone() { [ ! -e "$apps/.NonstopVibin.AppImage.update" ]; }
wait_for "the staging folder to be removed" 30 gone
echo "ok: $image is $new in place, relaunched as pid $pid, and its staging folder is gone"
logged "app update failed" && fail "the accepted update logged a failure"
kill "$pid"
wait_for "$new to quit" 30 sh -c "! kill -0 $pid 2>/dev/null"
echo "PASS: AppImage $old updated itself to $new"
