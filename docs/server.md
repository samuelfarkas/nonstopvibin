# Run on a Linux server

NonstopVibin can run without a desktop on a Linux server where your coding
agents run, for example a VPS you reach over SSH. The server build is one
`nonstopvibin` binary with the web UI and the pinned CLIProxyAPI core beside it.
It listens only on `127.0.0.1`; you open the UI from your laptop through an SSH
tunnel.

The two archives target **glibc-based Linux, x64 or arm64**, not just Debian.
Use them on common distributions such as Ubuntu/Debian, Fedora, Rocky/Alma/RHEL,
Arch and openSUSE, subject to the compatibility evidence below. Bun and Node are
not needed on the server. Alpine/musl is not supported by these archives;
compatibility shims are not a substitute for a tested musl build. A systemd user
session is required only for automatic service installation, not foreground use.
Install it as
the same non-root user that runs your agents, because it writes their settings
(`~/.codex`, `~/.claude`, pi and OpenCode folders) and serves their credential
helpers over a Unix socket owned by that user.

The server needs `curl` (the agents' credential helpers use it) and system CA
certificates (the core verifies providers with them):

Use your distribution's packages, for example:

```sh
# Debian / Ubuntu
sudo apt install curl ca-certificates
# Fedora / Rocky / Alma / RHEL
sudo dnf install curl ca-certificates
# Arch (x86_64) / Arch Linux ARM (separate project)
sudo pacman -Syu curl ca-certificates
# openSUSE
sudo zypper install curl ca-certificates ca-certificates-mozilla
```

For `systemctl --user`, log in directly as that user through SSH/PAM, not `su` or
`sudo`. The systemd user session and D-Bus must be installed and reachable.
Minimal Debian/Ubuntu installs may also need `sudo apt install dbus-user-session`;
other distributions package these components differently. Without a user manager,
use `install.sh --no-service`, then `nonstopvibin serve` in a terminal or under
your existing supervisor. No OpenRC/runit unit is provided.

The installer checks readable PEM certificate presence in common Debian/Arch,
RPM and openSUSE locations, plus `SSL_CERT_FILE`/colon-separated `SSL_CERT_DIR`
overrides. It does not validate certificate contents, provider TLS, or every
runtime's root selection. Keep the distribution's CA trust package current;
configure custom roots for the runtime that needs them rather than disabling TLS
verification.

## Compatibility evidence

Both current executables are dynamically linked ELF64 binaries, not static
Linux-universal executables. They need the matching glibc loader and system
libraries: the app uses libc, libpthread, libdl and libm; the pinned Go core also
uses libresolv. The current app's largest required GLIBC symbol version is 2.17
on both targets; the core's is 2.17 on arm64 and 2.3.2 on x64. These measured symbol
versions are **not** a complete minimum supported OS/kernel/CPU guarantee.
Old glibc releases, unusual loaders, CPU configurations and stripped-down images
need their own runtime checks. The loaders are `/lib64/ld-linux-x86-64.so.2` and
`/lib/ld-linux-aarch64.so.1`. These archives use Bun 1.4.2's normal Linux targets
(the x64 archive is not its alternate baseline-CPU target); the pinned core was
built with Go 1.26.4. Do not infer an older-CPU or kernel floor from ELF ABI notes.
Arch Linux ARM is distinct from official x86_64 Arch; results for one do not
certify the other.

The official archives were exercised in these isolated environments:

| Distribution                  | glibc | Architecture | Coverage                                         |
| ----------------------------- | ----- | ------------ | ------------------------------------------------ |
| Ubuntu 22.04.5                | 2.35  | native arm64 | SSH/PAM, systemd user service                    |
| Fedora 43                     | 2.42  | native arm64 | SSH/PAM, systemd user service                    |
| Arch rolling (20260927 image) | 2.44  | emulated x64 | container, foreground                            |
| Rocky 8.9                     | 2.28  | native arm64 | container, foreground                            |
| openSUSE Leap 16.0            | 2.40  | native arm64 | partial container: install, startup, UI, cleanup |

All but the partial openSUSE check exercised authenticated state and UI HTTP 200, a real pinned
core request to a synthetic provider, restart, status, session-token rotation
and cleanup. openSUSE's package mirror was unavailable, so synthetic requests and
restart were not verified there. The SSH checks used OrbStack Linux machines
with a shared VM kernel; the foreground checks used Docker, not full VMs. Arch's
full VM could not start. Automatic lingering was denied on Ubuntu/Fedora;
administrator-enabled lingering kept their services reachable after SSH logout.
Boot persistence was not tested. Native x64 hardware,
Arch Linux ARM, real-provider TLS/OAuth and arbitrary older distributions remain
unverified. A newer distribution working does not prove every older one works.

## Install

From a checkout of this repository on your laptop, with SSH access to the server
(`myserver` is any destination your `ssh` accepts, including `~/.ssh/config`
aliases):

```sh
bun run setup
bun run server:deploy myserver
```

This checks the server's architecture, builds the matching archive
(`bun run dist:server <arch>`), copies it over `scp`, and runs its `install.sh`.
Extra arguments go to `install.sh`, for example
`bun run server:deploy myserver --allow-host myserver.tailnet.ts.net`.

To install by hand, copy `nonstopvibin-server-<version>-linux-<arch>.tar.gz`
from a release (or `release/` after `bun run dist:server`) to the server, then:

```sh
tar -xzf nonstopvibin-server-<version>-linux-<arch>.tar.gz
./nonstopvibin-server-<version>-linux-<arch>/install.sh
```

`install.sh` copies the files to `~/.local/lib/nonstopvibin`, links
`~/.local/bin/nonstopvibin`, installs and starts a systemd user service, and
prints the session link. Options: `--prefix DIR`, `--no-service`, `--port N`
(default 4320), `--allow-host FQDN`. It warns when `curl` or CA certificates are
missing, and tells you when `~/.local/bin` is not on your `PATH`. The checksums in
the archive's `core/manifest.json` show that the core matches the reviewed pin;
they are integrity evidence, not independent proof of the publisher.

The service keeps running after you log out only with _lingering_ enabled.
`service install` enables it when allowed; otherwise it prints the command for
an administrator: `sudo loginctl enable-linger <user>`.

## Open the UI

From the repository checkout on your laptop:

```sh
bun run server:open myserver
```

This reads the session from the server (`nonstopvibin url --json` over SSH),
starts `ssh -N -L 127.0.0.1:4320:127.0.0.1:4320 myserver`, waits for the tunnel,
and opens your browser. Ctrl+C closes the tunnel. `--no-browser` only prints the
link.

Without the repository, run `nonstopvibin url` on the server. It prints the
tunnel command to run on your laptop and the link to open:

```text
ssh -N -o ExitOnForwardFailure=yes -L 127.0.0.1:4320:127.0.0.1:4320 you@203.0.113.10
http://127.0.0.1:4320/#session=…
```

Use the same port number on both ends of the tunnel, and open `127.0.0.1`, not
`localhost`: the server checks the browser's Host and Origin. If the desktop app
or another program already uses the port on your laptop, stop it, or reinstall
the server with another `--port`.

The session link is valid until the server restarts. A restart (including an
upgrade) shows a recovery screen in open tabs; run `nonstopvibin url` again and
paste the new link.

## Sign in to subscriptions

Provider sign-in returns your browser to a `http://localhost:…` address on your
laptop, which the remote server cannot see. When the page fails to load, copy its
full address from the address bar and paste it into the sign-in dialog, which
offers a field for it in the browser. Do it before the dialog's expiry time; the
code works once.

For automatic completion, forward the provider callback ports as well:

```sh
bun run server:open myserver --sign-in
# or add to the manual tunnel:
#   -L 127.0.0.1:1455:127.0.0.1:1455 -L 127.0.0.1:54545:127.0.0.1:54545
```

Port 1455 is Codex and 54545 is Claude. If an agent on the server is signing in
to its own account at the same time, the port is busy there and NonstopVibin
returns "Sign-in port N is in use" (HTTP 409); finish or cancel the other sign-in
first. API keys and account imports need no extra ports.

## Tailscale (optional)

With [Tailscale Serve](https://tailscale.com/kb/1312/serve), devices on your
tailnet can open the UI over HTTPS without an SSH tunnel:

```sh
nonstopvibin service install --allow-host myserver.tailnet.ts.net
sudo tailscale serve --bg 4320
nonstopvibin url
```

`--allow-host` takes one exact host name (no wildcard, port, or IP address). The
server then accepts that Host for the web UI and only the `https://` origin for
management. The agent API and WebSockets still answer only on loopback. The
session token is still required; Tailscale identity headers are not trusted.
Running `tailscale serve` as your user requires `sudo tailscale set
--operator=$USER`. Never use `tailscale funnel` for this port.

## Manage the service

```sh
nonstopvibin status                       # exit 0 running, 3 stopped
nonstopvibin url                          # session link and tunnel command
systemctl --user restart nonstopvibin     # issues a new session link
systemctl --user status nonstopvibin
journalctl --user -u nonstopvibin -f
```

Foreground/service logs never contain the session token unless attached to a
terminal. `url` prints it intentionally: do not save that output to shared logs.
`nonstopvibin service install` accepts `--port` and `--allow-host`; on an existing
unit it changes only supplied options and environment variables, preserving the
executable path, other arguments and previously configured directories. It copies
`NONSTOPVIBIN_DATA_DIR`, `CODEX_HOME`, `OPENCODE_CONFIG_DIR`, `XDG_CONFIG_HOME`
and `PI_CODING_AGENT_DIR` from your shell when they are set, so agent settings
land where your agents read them. Changing the port requires reconnecting agents
in **Connect agents**. Without systemd, run `nonstopvibin serve` in a terminal
multiplexer instead.

## Upgrade

Run `bun run server:deploy myserver` again, or extract a newer archive and run
its `install.sh`. The previous installation is kept until the new binary runs.
An existing service keeps its options and is restarted; pass `--port` or
`--allow-host` to change them. Use the original `--prefix`; the installer refuses
to silently upgrade a different executable from the one in the unit. `url` and
`status` discover a running user service's custom data directory when your shell
has no `NONSTOPVIBIN_DATA_DIR`. For any stopped instance, set that variable
again to inspect its custom directory.

## Uninstall

1. In the UI, disconnect each agent in **Connect agents**, so their settings no
   longer point at NonstopVibin.
2. Run `nonstopvibin service uninstall`.
3. Remove `~/.local/lib/nonstopvibin` and `~/.local/bin/nonstopvibin`.

Your data stays in `~/.nonstopvibin`; delete it to remove accounts and history.

## Security notes

- Everything running as your user is inside the trust boundary, as on the
  desktop: it can read `~/.nonstopvibin` (including `vault.key` and the session
  token in the owner-only `server.json`) and reach the loopback port. Run
  NonstopVibin as the same non-root user as your agents, not as root.
- Never expose port 4320 publicly (no `0.0.0.0` forwards, reverse proxies on
  public interfaces, or Tailscale Funnel). The session link grants full control.
- Profile keys never grant management access, and the agent gateway stays on
  loopback even with `--allow-host`.
