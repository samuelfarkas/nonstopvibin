#!/bin/sh
# NonstopVibin headless server installer. Run it from the extracted release
# directory as the same non-root user that runs your coding agents.
set -eu

# Filled in by scripts/package-server.ts (renderInstaller in scripts/distribution.ts).
VERSION="@VERSION@"
ARCH="@ARCH@"
EXECUTABLE="@EXECUTABLE@"
REQUIRED="@REQUIRED@"
ENTRIES="@ENTRIES@"
MACHINES="@MACHINES@"
DEFAULT_PORT="@DEFAULT_PORT@"
DEFAULT_PREFIX="@DEFAULT_PREFIX@"
PREFIX="$HOME/$DEFAULT_PREFIX"
SERVICE=1
SERVICE_ARGS=""

say() { printf '%s\n' "$*"; }
warn() { printf 'WARNING: %s\n' "$*" >&2; }
die() {
  printf 'Error: %s\n' "$*" >&2
  exit 1
}
# Presence check only, not validation of provider TLS or the runtime's root set.
# Match Go's common Linux bundle/directory locations and explicit overrides.
has_ca_certificates() (
  set -f
  has_certificate() {
    [ -f "$1" ] && [ -r "$1" ] && [ -s "$1" ] &&
      grep -q -- '-----BEGIN CERTIFICATE-----' "$1"
  }
  if [ -n "${SSL_CERT_FILE:-}" ]; then
    has_certificate "$SSL_CERT_FILE" && return 0
  else
    for file in /etc/ssl/certs/ca-certificates.crt /etc/pki/tls/certs/ca-bundle.crt \
      /etc/ssl/ca-bundle.pem /etc/pki/tls/cacert.pem \
      /etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem /etc/ssl/cert.pem; do
      has_certificate "$file" && return 0
    done
  fi
  IFS=:
  for directory in ${SSL_CERT_DIR:-/etc/ssl/certs:/etc/pki/tls/certs}; do
    set +f
    for file in "$directory"/* "$directory"/.[!.]* "$directory"/..?*; do
      has_certificate "$file" && return 0
    done
    set -f
  done
  return 1
)

usage() {
  cat <<EOF
Install NonstopVibin $VERSION (linux-$ARCH) for the current user.

Usage: ./install.sh [--prefix DIR] [--no-service] [--port N] [--allow-host FQDN]

  --prefix DIR       Install to DIR/lib/nonstopvibin with a DIR/bin/$EXECUTABLE
                     link (default: \$HOME/$DEFAULT_PREFIX).
  --no-service       Do not install or restart the systemd user service.
  --port N           Service port (default $DEFAULT_PORT). Passed to
                     \`$EXECUTABLE service install\`.
  --allow-host FQDN  Exact HTTPS host name for the web UI, e.g. from
                     \`tailscale serve\`. Passed to \`$EXECUTABLE service install\`.

A first install creates and starts the user service. An upgrade restarts the
existing service and keeps its options, unless --port or --allow-host is given.
Targets: glibc-based Linux, x64 or arm64 (not Alpine/musl).
A systemd user session is needed only for the service; otherwise use --no-service.
EOF
}

set -f # SERVICE_ARGS is split on purpose; never glob it.
while [ $# -gt 0 ]; do
  case "$1" in
    --prefix)
      [ $# -ge 2 ] || die "--prefix needs a directory."
      PREFIX=$2
      shift 2
      ;;
    --prefix=*)
      PREFIX=${1#--prefix=}
      shift
      ;;
    --no-service)
      SERVICE=0
      shift
      ;;
    --port | --allow-host)
      [ $# -ge 2 ] || die "$1 needs a value."
      case "$2" in
        "" | -* | *[!A-Za-z0-9.-]*) die "Invalid value for $1: $2" ;;
      esac
      SERVICE_ARGS="$SERVICE_ARGS $1 $2"
      shift 2
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      usage >&2
      die "Unknown option: $1"
      ;;
  esac
done

[ "$(uname -s)" = Linux ] || die "This build is for Linux."
machine=$(uname -m)
for pair in $MACHINES; do
  case "$machine" in
    "${pair#*:}") machine=${pair%%:*} ;;
  esac
done
[ "$machine" = "$ARCH" ] ||
  die "This archive is for linux-$ARCH, but this machine is $machine. Download the linux-$machine archive."
[ "$(id -u)" -ne 0 ] ||
  warn "Installing as root. Install as the non-root user that runs your coding agents instead."

case "$PREFIX" in
  /*) ;;
  *) PREFIX="$(pwd)/$PREFIX" ;;
esac
SOURCE=$(cd "$(dirname "$0")" && pwd)
for entry in $REQUIRED; do
  [ -e "$SOURCE/$entry" ] || die "$SOURCE/$entry is missing. Extract the complete archive first."
done

if ! command -v curl >/dev/null 2>&1; then
  warn "curl is not installed. Agent credential helpers need it; install curl with your distribution's package manager."
fi
if ! has_ca_certificates; then
  warn "No readable CA certificate bundle was found in common Linux trust-store locations. Install/configure your distribution's CA certificates (see docs/server.md); this presence check does not validate provider TLS."
fi

LIB="$PREFIX/lib/nonstopvibin"
BIN="$PREFIX/bin"
UNIT=
if [ "$SERVICE" = 1 ]; then
  "$SOURCE/$EXECUTABLE" --version >/dev/null ||
    die "The new binary does not run on this system. Nothing was changed."
  # The CLI writes the unit, so it alone knows its path and how it quotes the executable.
  UNIT=$("$SOURCE/$EXECUTABLE" service unit "$LIB/$EXECUTABLE") || exit 1
fi
NEW="$PREFIX/lib/.nonstopvibin-new.$$"
OLD="$PREFIX/lib/.nonstopvibin-old.$$"
mkdir -p "$PREFIX/lib" "$BIN"
rm -rf "$NEW" "$OLD"
mkdir "$NEW"
trap 'rm -rf "$NEW"' EXIT
for entry in $ENTRIES; do
  [ ! -e "$SOURCE/$entry" ] || cp -R "$SOURCE/$entry" "$NEW/"
done
"$NEW/$EXECUTABLE" --version >/dev/null ||
  die "The new binary does not run on this system. Nothing was changed."

# Keep the previous installation until the new one runs from its final path.
upgrade=0
if [ -e "$LIB" ]; then
  upgrade=1
  mv "$LIB" "$OLD"
fi
mv "$NEW" "$LIB"
trap - EXIT
if ! "$LIB/$EXECUTABLE" --version >/dev/null; then
  rm -rf "$LIB"
  [ "$upgrade" = 0 ] || mv "$OLD" "$LIB"
  die "The installed binary failed to run; the previous installation was restored."
fi
rm -rf "$OLD"
ln -sfn "$LIB/$EXECUTABLE" "$BIN/$EXECUTABLE"
say "Installed $("$LIB/$EXECUTABLE" --version) to $LIB"

running=0
if [ "$SERVICE" = 1 ]; then
  if [ -n "${XDG_RUNTIME_DIR:-}" ] && systemctl --user show-environment >/dev/null 2>&1; then
    if [ -f "$UNIT" ] && [ -z "$SERVICE_ARGS" ]; then
      say "Restarting the existing service..."
      systemctl --user restart "${UNIT##*/}"
      tries=0
      until "$BIN/$EXECUTABLE" status >/dev/null 2>&1; do
        tries=$((tries + 1))
        [ "$tries" -lt 30 ] ||
          die "The service did not become ready. Check: journalctl --user -u ${UNIT##*/} -e"
        sleep 1
      done
    else
      # shellcheck disable=SC2086 # validated words, split on purpose
      "$BIN/$EXECUTABLE" service install $SERVICE_ARGS
    fi
    running=1
  else
    say ""
    say "No systemd user manager is reachable, so no service was installed."
    say "Log in over SSH directly as this user (not su/sudo), and check that"
    say "your distribution's systemd user session and D-Bus are available."
    say "Then run: $BIN/$EXECUTABLE service install"
    say "Without systemd, use --no-service and run serve under your own supervisor."
  fi
fi

say ""
if [ "$running" = 1 ]; then
  "$BIN/$EXECUTABLE" url
else
  say "Start it in the foreground with: $BIN/$EXECUTABLE serve"
  say "Then run \`$BIN/$EXECUTABLE url\` in another shell for the session link."
fi
case ":$PATH:" in
  *":$BIN:"*) ;;
  *)
    say ""
    say "$BIN is not on your PATH. Use $BIN/$EXECUTABLE, or add this to your shell profile:"
    say "  export PATH=\"$BIN:\$PATH\""
    ;;
esac
