# Run on a Linux server

NonstopVibin can run without a desktop on a Linux server, either next to trusted
coding agents or as a protected upstream consumed by a separate private gate. The server build is one
`nonstopvibin` binary with the web UI and the pinned CLIProxyAPI core beside it.
It listens only on `127.0.0.1`; you open the UI from your laptop through an SSH
tunnel.

The two archives target **glibc-based Linux, x64 or arm64**, not just Debian.
Use them on common distributions such as Ubuntu/Debian, Fedora, Rocky/Alma/RHEL,
Arch and openSUSE, subject to the compatibility evidence below. Bun and Node are
not needed on the server. Alpine/musl is not supported by these archives;
compatibility shims are not a substitute for a tested musl build. A systemd user
session is required only for automatic service installation, not foreground use.
For **trusted co-located agents**, install it as the same non-root user that runs
those agents: native setup writes their settings (`~/.codex`, `~/.claude`, pi and
OpenCode folders) and serves their credential helpers over that user's Unix
socket. For **untrusted/VM workers**, instead use a dedicated protected non-root
service user with no coding agents running as that user; see
[Protected upstream](#protected-upstream) below. These are different trust models.

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
overrides, including hidden PEM files in certificate directories. It does not
validate certificate contents, provider TLS, or every
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

Earlier official archives were exercised in these isolated environments (before
the protected-upstream socket-read correction):

| Distribution                  | glibc | Architecture | Coverage                                         |
| ----------------------------- | ----- | ------------ | ------------------------------------------------ |
| Ubuntu 22.04.5                | 2.35  | native arm64 | SSH/PAM, systemd user service                    |
| Fedora 43                     | 2.42  | native arm64 | SSH/PAM, systemd user service                    |
| Arch rolling (20260927 image) | 2.44  | emulated x64 | container, foreground                            |
| Rocky 8.9                     | 2.28  | native arm64 | container, foreground                            |
| openSUSE Leap 16.0            | 2.40  | native arm64 | partial container: install, startup, UI, cleanup |

A separate **current-source** compiled protected-upstream smoke passed on existing
Debian 12 arm64/glibc 2.36: native Anthropic synthetic tool SSE/replay/images,
non-spending readiness, graceful FIN cleanup, startup ownership, restart/session
rotation and stopped complete synthetic-data backup/restore. It ran foreground
with isolated configuration and no service/linger policy changes. The older
matrix above remains historical lifecycle/ABI evidence, not current-gateway
acceptance on every listed distribution; neither check proves live inference.

All but the partial openSUSE check exercised authenticated state and UI HTTP 200, a real pinned
core request to a synthetic provider, restart, status, session-token rotation
and cleanup. openSUSE's package mirror was unavailable, so synthetic requests and
restart were not verified there. The SSH checks used OrbStack Linux machines
with a shared VM kernel; the foreground checks used Docker, not full VMs. Arch's
full VM could not start. Automatic lingering was denied on Ubuntu/Fedora;
corrected root/orb probes with administrator-enabled lingering verified
authenticated reachability while logged out, with no user-login sessions before
or after the probe and unchanged service PID/session-token identity. Earlier
probes reopened SSH before fetching state and did not prove this persistence.
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

## Protected upstream

This is a **general deployment contract**, not a remote-worker feature inside
NonstopVibin. The app already owns credentials, profile account pools, core
lifecycle, protocol forwarding, provider quota and profile usage. A consuming
system must provide its own private authenticated inference gate and worker
isolation. There is no bundled gate, attempt-token issuer, remote credential
broker, scheduler, task database or new agent launcher.

```text
worker with consumer-issued scoped credential
  -> consumer's private protected inference gate
  -> LOCAL http://127.0.0.1:4320/p/<profile-slug>/v1
  -> pinned CLIProxyAPI -> approved provider/account pool
```

### Deployment and authority

- Run the existing server build/service under a **dedicated protected non-root
  user**. Coding agents stay under other identities/in their VMs, never under
  this user. Use its own private `HOME` and `NONSTOPVIBIN_DATA_DIR`; do not inherit
  worker `CODEX_HOME`, `PI_CODING_AGENT_DIR`, `OPENCODE_CONFIG_DIR` or
  `XDG_CONFIG_HOME` overrides into the service. Native **Connect agents** remains
  supported for the trusted co-located mode, not for configuring remote guests.
- Both app and profile cores remain on `127.0.0.1`. The gate's protected-side
  connector calls the profile API **locally**, with the fixed approved endpoint
  and NonstopVibin profile key. Guests receive **only the consumer's credentials**,
  never NonstopVibin profile keys or management tokens. The gate must not accept
  caller-supplied upstream URLs, keys or profile identity.
- Never share the data directory, SQLite, `vault.key`, provider OAuth/API/config
  files, management session, Unix broker socket, host credential helper or host
  loopback connection settings with VMs, their images/mounts, browser bundles or
  model contexts. Same-user access can decrypt the vault; profiles are account
  pools, not an OS security boundary. Provision only authorized accounts and
  obey provider terms and usage limits; pooling does not create entitlement.
- `--allow-host exact.private.name` is solely for a private **HTTPS management
  UI**, still requiring the management session. It does not enable VM inference
  or trust Tailscale identity headers. Do not publish NonstopVibin/core endpoints
  through zrok, public ingress, Funnel or `0.0.0.0` listeners/forwards.
- Loopback is not guest egress isolation. The consumer must deny access to host
  management, broker/data, sibling tasks and direct-provider/auth bypass, and
  authorize every gate request. A private network alone is not authorization.

### Existing API and non-spending readiness

The headless CLI defaults to **4320** (desktop/development defaults to 4318).
Use the configured port and discover profile identity from authenticated state:
`id` is the UUID used in `/api/profiles/<id>/…`; `slug` is used in
`/p/<slug>/v1`. `endpoint` in state is authoritative. Do not infer an endpoint
from a display name or use a UUID where a slug is expected. The unprefixed `/v1`
route also selects exclusively by profile key, but a protected connector should
pin the explicit profile URL to catch mapping mistakes.

| Check                                                  | Authority and result                                                                                                                                                                                                                                                     |
| ------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `nonstopvibin status`                                  | Local protected user; exit **0** means authenticated `/api/state` liveness, **3** means stopped/unreachable/stale session, other errors remain errors. Human-readable output, not a profile-readiness assertion.                                                         |
| `nonstopvibin url --json`                              | Same local user; exit 0 with `running`, PID, port, app/core versions and session links. Exit 3 if not running. **Contains management authority**: never put it in worker/shared logs. `status --json` is not a JSON status API.                                          |
| `GET /api/state`                                       | Exact `Authorization: Bearer <management-session>`; JSON app state, including `profiles[]` (`id`, `slug`, `runtime`, `endpoint`, optional error), `coreAvailable`, core/app versions. A running app/file-present core does not mean every profile restored successfully. |
| `GET /api/profiles/<UUID>/models`                      | Management session; JSON **array** of model objects with `id` (optional `owned_by`). Missing UUID: 404; malformed UUID: 400; stopped profile: 503.                                                                                                                       |
| `GET /p/<slug>/v1/models`                              | Protected-side profile key; normally `{object:"list",data:[…]}`. With `anthropic-version`, the core uses Anthropic pagination fields (`data`, `first_id`, `last_id`, `has_more`). A stopped profile is 503; an empty running catalog can be **200 with `data:[]`**.      |
| `POST /api/profiles/<UUID>/agent-check` with JSON `{}` | Management session; checks profile-key authentication and a nonempty gateway catalog, **no inference**. Success 200 `{ok:true}`; stopped/catalog HTTP failure 502; empty catalog 400; missing profile 404. This does not install agent settings.                         |

The gate's serving identity needs only the approved **profile key**, not the
management session. Keep management/start/stop/key retrieval in protected
operator maintenance, not guest tools or a consumer dashboard. Inference accepts
`Authorization: Bearer <profile-key>` or `x-api-key: <profile-key>`; if both are
present they must match. Query `key`/`api_key` authentication is rejected.
Management accepts only its own exact Bearer session, never `x-api-key` or a
profile key. Management tokens cannot substitute for profile keys either.

Gateway requests require the exact loopback Host (including port) and **no Origin
header**. Unknown/missing keys: 401; valid key on another/unknown slug: 403;
unsupported proxy path: 404. App errors are JSON
`{error:{message,type:"nonstopvibin_error",code:<status>}}`. Host/browser-origin
rejection is 403. Infer only against the current approved model IDs in a running,
nonempty profile. Catalog availability is not proof of inference, authorization,
remaining quota or tool/vision compatibility: an exhausted pool can still have a
catalog. A synthetic unavailable model on the native Messages route returns
400 `invalid_request_error` without contacting a provider. Exhaustion fails
inside that profile, never borrows another pool. Core and provider errors have
their own protocol shapes/statuses, not app-error shape.

### Native Anthropic transport and limitations

Native routes are `POST /p/<slug>/v1/messages` and
`POST /p/<slug>/v1/messages/count_tokens`; JSON requests use actual catalog model
IDs. Streaming Messages use `text/event-stream`; nonstreaming uses JSON. The
app pipes bodies/streams to the real core without a second protocol adapter.
The core can normalize fields and headers; native Anthropic and cross-protocol
translation are **different acceptance paths**.

- The app forwards `anthropic-version`/`anthropic-beta`, strips client auth,
  cookie, Origin/Referer, `forwarded`, `x-forwarded-for`, `x-forwarded-host`,
  `x-forwarded-proto`, `x-goog-api-key` and `x-nonstopvibin-agent`, then uses the
  selected profile's internal core key. It
  adds/stabilizes `x-opencode-session` from caller session headers or a random ID.
  The core selects provider credentials; they never come from the worker.
- Pinned **8.0.8** synthetic native custom-origin tests show the caller's version
  preserved and a missing version defaulted to **2023-06-01**. Extension betas
  survive, body `betas` are lifted into the header, and no beta is added to a
  plain custom API request without one. This is not unconditional beta
  transparency: the fixture shows `effort-2025-11-24` filtered when
  `thinking.type` is `disabled`, while `message-threads-2026-08-12` survives.
  Other core model/feature gating and OAuth/explicit CLI fingerprints assemble
  their own beta/header profile. Existing tests also cover tool-change beta forwarding
  and OAuth-fingerprint tool references, with synthetic credentials only.
- Native synthetic tests preserve SSE ordering, two fragmented tool argument
  streams, IDs and tool-result replay content, and PNG source type/media
  type/decoded bytes. For an unconfirmed API caller the core inserts an
  ephemeral cache breakpoint on the latest user block. This is not proof of an
  actual Claude process, provider-issued signatures, compaction, or live OAuth.
- The app forwards core status/response headers and adds
  `x-nonstopvibin-profile: <slug>`, removing `set-cookie` and
  `access-control-allow-origin`. The core does not promise every provider header
  reaches the app. Synthetic provider 400 becomes Anthropic
  `{type:"error",error:{type:"invalid_request_error",message:…}}`; the core drops
  provider `request_id` from that body. Synthetic exhausted credentials return
  429 with `rate_limit_error`. The app's own 32-in-flight **per-profile** cap is
  429 with `Retry-After: 2`, not a per-task limit or global coding-session policy.
- After streaming headers are committed, an error cannot change HTTP 200.
  Explicit native provider error events survive without a `message_stop`.
  **Pinned native clean EOF can silently end an incomplete stream without an
  error event**. A consumer must require the protocol completion terminal,
  distinguish truncation/error/unknown outcomes, and avoid uncontrolled retries.
- `count_tokens` is **not uniformly authoritative**. Pinned source uses native
  upstream counting only at the first-party Anthropic origin with credentials;
  that live branch was not exercised. A custom origin uses the core's local
  O200kBase text estimate and sends no provider count request. Fixture evidence
  shows adding an image leaves that count unchanged: image bytes/tokens are
  omitted. Empty messages produce 400. Other provider routes can have different
  or unsupported counting behavior. Do not use these estimates as a hard
  multimodal/context or spending budget; reject unsupported admission cases or
  conservatively bound them in the consumer, never label omissions authoritative.
- After consuming the initial native frame, fixture checks verify **graceful
  FIN, explicit TCP reset, Fetch abort and ClientRequest destroy** release the
  local profile slot and close an idle provider transport within two seconds.
  The gateway resumes socket reads once after successful body-pipeline completion:
  Bun 1.4.2 can otherwise leave native reads paused and defer a peer FIN. Checks
  also preserve fragmented request bodies and successive profile identities on
  one keep-alive connection. This is tested local transport cleanup, not a
  universal native-client cancellation guarantee; the consuming gate/guest path
  still needs its own acceptance. Even closure does not prove remote computation
  or billing stopped. NonstopVibin has no task cancellation endpoint; stopping a
  shared profile is not task cancellation.

Reproduce the supported local slice with `bun test tests/native-anthropic.test.ts`
and existing `tests/cross-harness.test.ts`/`tests/integration.test.ts`. Only the
external provider boundary is synthetic; app and compiled pinned core are real.
These checks do not certify a consuming gate, real provider or full native-client
acceptance. A completed local transport check must not be treated as proof that
provider computation/billing or a consuming task's tools stopped.

### Unattended operation, backup and recovery

Use the existing `systemd --user` service and `status`/authenticated profile
checks above, not inference heartbeats. A user manager available at login is not
necessarily available after logout or at boot: administrators must approve and
verify lingering (`loginctl show-user <protected-user> -p Linger`) and the host's
boot/storage/network readiness. The earlier compatibility evidence covers logout
with enabled linger on specific disposable environments, **not reboot or
long-duration uptime**. No service/linger/host policy is changed by this guide.

A per-data-directory SQLite startup lock holds one writer from before store/core
initialization through shutdown, even if a contender uses a different port. Do
not delete the lock inode or launch a second instance over the same directory.
Startup restores previously enabled profiles; a profile restore failure is
reported in state and does not make that pool ready. Shutdown interrupts active
traffic and drains published accounting before killing cores; a crash can leave
unknown outcomes. Restart rotates the management session: recover the operator
URL with `nonstopvibin url`, rather than reusing an old tab's token. Profile keys
persist across profile/app restarts and a matching restore; stopping/disconnecting
an agent does **not** rotate/revoke its cached key. There is no documented public
profile-key rotation API; never treat restart as attempt-credential revocation.

Back up the **complete protected data directory with the app and all its cores
STOPPED**, including SQLite/any sidecars, matching `vault.key`, profile config/auth
files and connection metadata. Use protected encrypted backup storage, preserve
0700 directories/0600 secret files and service-user ownership, and keep it away
from worker snapshots/artifacts. Never copy only an active main database. Restore
with the service stopped into an owner-only directory, set the same data override
and verify ownership before startup, then check each restored profile/catalog.
An existing database without its matching key is rejected; a new key cannot
recover it. `server.json` from backup is not a live session and is authenticated
before discovery; the new process writes a new management token. An isolated
restore can auto-start enabled pools: revoke/fence consumer grants first and do
not independently refresh duplicate copies of one OAuth identity. Provider token
rotation can invalidate the other copy. Restore does not roll back external
provider spending, consumer grants or task state.

Profile usage is delayed reporting, **not a crash-safe budget ledger**. Collection
normally follows requests at about two seconds, settles delayed publication for
ten seconds and falls back to thirty seconds idle; crashes between provider
completion, queue collection and SQLite commit can lose events. Provider quota
reads are about two minutes apart and readings older than five minutes are stale.
Unknown prices/usage remain unknown, not free. The consumer owns atomic durable
call/token/concurrency reservations and conservative settlement across retries,
uncertain results and restarts; profile totals cannot enforce hard task budgets.

### Consumer example: Homeplane (planned gate)

The selected design is **native Claude in a Boxd task VM → planned private
protected Homeplane gate with current attempt credential → existing NonstopVibin
loopback profile endpoint → pinned core → approved provider pool**. The gate is
not implemented/proven by this repository. Homeplane/Boxd/Herdr/Canvas end-to-end
acceptance and live inference remain outstanding; selecting NonstopVibin does not
relax credential protection, provider authorization, terms or usage limits.

Homeplane owns issuance/revocation bound to task/attempt/generation (and bounded
protected reasoning roles), fixed profile + allowed-model mapping, endpoint and
request-size/deadline limits, task/daily budget admission, session/task attribution,
scoped cancel, guest-native settings and private ingress/guest-network isolation.
Its initial **at most two active coding sessions globally** is Homeplane policy,
not a NonstopVibin runtime flag. Use shared deliberate pools, not one profile per
attempt; a shared pool is not a shared task permission.

Homeplane must generate the guest's native Claude `ANTHROPIC_BASE_URL` pointing to
its **private gate**, approved model IDs/settings, and an `apiKeyHelper` returning
only the current Homeplane attempt credential. Do not copy the host helper/Unix
socket or `127.0.0.1:4320` settings into a VM. Preserve native hooks/permissions,
clear conflicting inherited provider/OAuth auth and deny direct/personal-login
fallback at the network and authorization boundaries. Native Claude lifecycle
stays with Herdr; NonstopVibin neither starts nor replaces it. Cancel revokes that
attempt and closes its own supported active transports/tools, not a profile shared
with another task. Gate restart, NonstopVibin restart and backup restore must not
revive old grants. None of these Homeplane obligations is provided merely by the
loopback listener or profile key.

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
  NonstopVibin as the same non-root user only for trusted co-located agents.
  Protected deployments use a dedicated non-root service user, not a worker user.
- Never expose port 4320 publicly (no `0.0.0.0` forwards, reverse proxies on
  public interfaces, or Tailscale Funnel). The session link grants full control.
- Profile keys never grant management access, and the agent gateway stays on
  loopback even with `--allow-host`.
