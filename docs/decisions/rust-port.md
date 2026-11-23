# Decision record: one Rust binary replaces Electron and the TypeScript server

Status: accepted, November 2026. Consulted: Fable (claude-fable-5-2), the architecture
reviewer the owner named. Measurements: [evidence](rust-port-evidence.md).

## Context

nonstopvibin runs in the background all day. On main (4ea188a), Electron hosts the TypeScript
server in its main process, and the headless Linux server is the same code compiled with Bun.
With no window open, Electron alone keeps about 180 MiB PSS in 7 processes. The Bun server keeps
about 44 MiB and wakes 22–52 times a second for minutes after a burst of requests, because of
its garbage-collector timer. One CLIProxyAPI Go core per profile stays a subprocess in every
option below. Only the host changes.

## Decisions

### Processes

- **One daemon binary, `nonstopvibin`.** It serves the gateway, the management API and the
  React UI, runs the CLI, and hosts the tray in desktop mode. On Linux it links no GUI
  library, so the same binary runs on a headless server. With no command it is the desktop
  app; on Linux without `DISPLAY` or `WAYLAND_DISPLAY` it prints the headless help instead,
  as the TypeScript CLI did. `--background`, which Electron's autostart entries pass, always
  starts the desktop app, without a window.
- **The UI is a directory beside the daemon, not embedded.** A package puts the built
  interface in `client/` next to the real executable (`Contents/Resources/client` in the macOS
  bundle), as the server archive already did; a source checkout serves `dist/client`.
  _Deviation from Fable,_ who embedded it with a `build.rs`. One code path serves packages,
  development runs and the golden replay, `cargo build` and `cargo test` never need the
  JavaScript build, the server archive and `install.sh` keep their layout, and a UI-only
  change does not rebuild the daemon. The cost is a `stat` per request and a read per
  uncached file; the ETag stays per launch, so file times are never trusted.
- **One window helper, `nonstopvibin-window`.** It is the only code that links WebKit (WKWebView
  on macOS, WebKitGTK 4.1 on Linux). The daemon starts it when a window or a dialog opens, and
  it exits when its last window closes. Process exit is what guarantees that no WebKit process
  outlives the window. Both windows (main and quota popup) live in this one helper.
- **Plain `wry` and `tao`, not Tauri.** Tauri's bundler, updater, tray and plugins all belong to
  the long-lived process, which cannot link GTK. Inside the helper, Tauri adds only weight
  around the same `wry` window, and its Linux tray has no click event.
- **Tray:** `ksni` on Linux (D-Bus, no GTK), `tray-icon` with `muda` on macOS on the main
  thread's event loop.
- **macOS:** the daemon is a UI element (`LSUIElement`). The helper takes the regular
  activation policy while the main window exists, so the Dock icon shows only then.
  Electron showed it whenever the app ran. The quota popup closes on focus loss and Escape,
  and the Swift click monitor is deleted.
- **Linux:** the helper uses the system WebKitGTK. If it cannot start, the daemon opens the
  session link in the system browser. On NVIDIA systems the helper sets
  `WEBKIT_DISABLE_DMABUF_RENDERER=1` unless the user set it.

### The bridge to the UI

- The UI keeps `window.nonstopvibin` with all nine methods and no change. The helper injects
  the bridge as a WebKit user script, and calls arrive over `wry` IPC.
- The helper serves `bootstrap`, `copy`, `chooseProject`, `releaseWindow` and `showWindow`.
  It forwards `openExternal`, `getLoginItem`, `setLoginItem` and `revealData` to the daemon over
  its stdio pipe, and the daemon validates each argument again.
  _Deviation from Fable,_ which moved those four to `/api/desktop/*` routes at the cost of about a
  dozen UI lines. The owner asked to keep the UI as unchanged as possible, and the pipe already
  exists and is private to a process the daemon started.
- The session token reaches the helper on its stdin, never in argv, the environment or a URL.
- The helper accepts IPC only from the main frame of a page on the daemon origin, and parses
  each message as `{ id, method, arg }` with a length cap. Unknown methods fail to parse.
- Navigation to any other origin is denied. New windows are denied, and their URL goes to the
  same provider-host allowlist as `openExternal`. Downloads are denied. Devtools are off in
  release builds. The CSP stays byte-identical to main's.
- A second launch finds the running daemon through `server.json` and calls
  `POST /api/desktop/show` with its token.
- When the desktop app cannot start, or its quit cannot close cleanly, it shows main.ts's
  error box ("NonstopVibin could not start", "Could not shut down cleanly") with the reason and
  exits 1 once it is dismissed. The daemon starts the helper with a single `Alert` line in
  place of `Start`: no origin, token or webview. _Deviation from Fable,_ whose dialogs carry
  only fixed message kinds and no free text. The reason (a missing `vault.key`, a refused
  lock, a missing core) is what the user must act on, and stderr never reaches a user who
  started the app from a launcher or at login. `Reason` is the one free text on the pipe, and
  both ends parse it: control characters except line breaks and bidirectional controls are
  dropped, at most 1,000 characters are kept, and the helper shows it only as the dialog's
  plain text (GTK secondary text without markup, `NSAlert` informative text). If the helper
  cannot show it, for example without WebKitGTK, Linux posts the reason, markup-escaped, as a
  desktop notification over D-Bus; stderr keeps it in every case.

### Code layout

Thirteen crates under `crates/`, edition 2024, one `[workspace.lints]` table. "Depends on"
lists each crate's direct workspace dependencies:

| Crate      | Owns                                                                                                                                                                         | Depends on                                                     |
| ---------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------- |
| `domain`   | Ids, enums, wire types, the registry, secret newtypes, JavaScript semantics, time, random ids, the daemon's addresses, the helper protocol. No file, network or process I/O. | —                                                              |
| `files`    | The data directory and every path in it, its single-instance lock, owner and symlink checks, atomic writes.                                                                  | `domain`                                                       |
| `store`    | Vault, SQLite thread.                                                                                                                                                        | `domain`, `files`                                              |
| `net`      | The loopback HTTP/1 client and the one HTTPS GET client (rustls with ring).                                                                                                  | —                                                              |
| `core`     | Core binary pin and hash checks, config, spawn, watchdog, ledger, restart policy, pool, management client, the reviewed-pin fetch.                                           | `domain`, `files`, `net`, `store`                              |
| `accounts` | Accounts, quota, usage drain, the deadline scheduler, sign-in, credentials, core updates, pricing, model catalog.                                                            | `core`, `domain`, `files`, `net`, `store`                      |
| `agents`   | The four agent setup writers, native config files, the credential socket.                                                                                                    | `domain`, `files`                                              |
| `gateway`  | Key extraction, routing, header rewrite, HTTP, SSE and upgrade pass-through.                                                                                                 | `domain`                                                       |
| `server`   | Listener, Host, Origin and token checks, `/api` routes, the UI's files, startup and shutdown order.                                                                          | all above, `shell`                                             |
| `shell`    | Tray, helper supervision, login item, app updater, behind a `ShellHost` trait the server implements.                                                                         | `domain`, `files`, `net`; `core` on macOS                      |
| `window`   | The helper binary: `wry` window, bridge script, IPC, dialogs.                                                                                                                | `domain`                                                       |
| `cli`      | The daemon binary: arguments, `serve`, `url`, `status`, `service`, mode selection.                                                                                           | `agents`, `core`, `domain`, `files`, `net`, `server`, `shell`  |
| `testkit`  | Test support, a dev-dependency only: temp directories, port reservations, chunked bodies, fake cores, the accounts' world, a shell host.                                     | `accounts`, `core`, `domain`, `files`, `net`, `shell`, `store` |

- The gateway receives a request and a `Lease` (profile id, slug, client and core keys, upstream
  address, in-flight guard). A `Lease` holds no management key, and the gateway crate cannot
  name the management client, so "profile keys never grant management access" holds at compile
  time.
- Secrets are newtypes over `secrecy` with no `Serialize` and a redacted `Debug`. Client keys
  compare in constant time (`subtle`).
- Raw `hyper` 1 serves and calls HTTP. There is no `axum`, `reqwest`, `clap`, `regex`, YAML
  crate or WebSocket crate. The core config is written by a small emitter and proved by loading
  it into the real core in the integration test.
- Wire types derive `ts-rs` in tests, and a TypeScript type test asserts that each generated
  type and its hand-written twin in `src/shared` are mutually assignable. Provider, agent and
  vocabulary data stays in TypeScript, and a script exports it to
  `crates/domain/data/registry.json`, with a CI drift check.

### Runtime

- Tokio `current_thread`, measured at 0 idle wakeups. SQLite runs on one dedicated thread, and
  each synchronous TypeScript store section becomes one closure on that thread. At most four
  blocking threads, kept alive two seconds.
- One scheduler task sleeps until the earliest deadline (usage drains, account refresh, idle
  stop, restart backoff, retention). With no deadline it never wakes.
- `panic = "unwind"` with overflow checks. A request task's panic ends that request only. A panic
  in the pool, scheduler or store thread starts a graceful shutdown with a non-zero exit, and
  the panic hook logs the location, never the payload.
- Linux packages keep static musl's own allocator. In an interleaved A/B, mimalloc saved
  0.2–0.26 ms of host CPU per large request but did not change request latency, and it doubled
  PSS after bursts (rust-port-evidence.md). Bodies pass through as frames with hyper's buffers
  capped.

### Data

- No migration. File names, SQLite schema and PRAGMAs, JSON bodies, vault format
  (base64 of `nonce(12) ‖ tag(16) ‖ ciphertext`, AES-256-GCM), core ledger and lock files stay as
  they are. This PR adds no table and no column.
- Timestamps keep the exact `toISOString()` shape, because SQL compares them as text.
- Stricter than main: `vault.key` and the database must be ordinary files the user owns, never
  followed through a link (main followed links and checked no owner or file type).
- `tests/fixtures/ts-data-dir` is a data directory main wrote. The Rust test reads it against
  literals, then writes through every store operation. A CI job opens the result with the last
  TypeScript `Store` to prove downgrade.
- **Downgrade:** the updater never downgrades. A manual install of an older build opens the
  same directory with nothing lost. The new entries (`server.json` in desktop mode,
  `webview/`, update staging) are ignored by old code. Quit the app before installing an
  older build: old Electron builds take no lock.

### Updates and packaging

- Packaging is hand-rolled in `scripts/`, which already holds the signing, core validation and
  package verification. It produces the `.app` bundle, zip and dmg, and on Linux
  `appimagetool` and `dpkg-deb`.
- Layouts. macOS: `Contents/MacOS/NonstopVibin` (the daemon, still the bundle executable)
  and `nonstopvibin-window`, `Contents/Resources/core/` and `licenses/`. Linux, the AppImage
  root (`AppRun` links to the daemon) and the deb's `/opt/NonstopVibin/`, where the Electron
  executable was: `nonstopvibin`, `nonstopvibin-window`, `core/`, `licenses/`. Server archive:
  `nonstopvibin`, `core/`, `licenses/`, `install.sh`, `README.md`. The daemon finds `core/` in
  `../Resources` on macOS and beside its real path on Linux. Menus start it without
  arguments; existing Electron autostart entries pass `--background`.
- Releases keep publishing `latest-mac.yml` and `latest-linux.yml` with the same bundle
  identifier, designated requirement, artifact names and zip layout. Installed Electron builds
  therefore update onto the Rust build through electron-updater.
- The Rust updater (`crates/shell/src/update`) reads the same feed. It finds the newest release
  in `releases.atom`, as electron-updater did for betas, and keeps electron-updater's channel,
  timing, dialogs and SHA-512 check. On macOS it stages beside the bundle, verifies the code
  signature against the running bundle's requirement and the bundled core against its manifest,
  swaps the bundle with one atomic exchange (two renames with rollback where the file system has
  none), and starts the new daemon itself with the old one's environment. On Linux it swaps the
  AppImage in place the same way. Squirrel.Mac relaunched without the environment.
- Login item: `SMAppService.mainApp` on macOS, the registration Electron made. On Linux it is
  the same `~/.config/autostart` entry.
- Entitlements become empty: no JIT, executable memory or library-validation exceptions.
  WebKit's JIT runs in Apple's WebContent process, and both executables link only system
  libraries, which library validation accepts. `scripts/verify-rust-package.ts` fails the
  package if an executable links anything outside `/System/Library` or `/usr/lib`.
- The Linux daemon is static musl. A glibc build on ubuntu-24.04 needs `GLIBC_2.34`
  (`__libc_start_main`, the pthread symbols) and refuses to start on Rocky 8 (glibc 2.28) from
  the [server](../server.md) matrix; the musl build of the same tests passes there and on
  Alpine. The archive's floor is then the core's glibc 2.17. The window helper links the
  system WebKitGTK, stays glibc and needs 2.34, below every distribution with WebKitGTK 4.1.
  The allocator measurement still decides whether musl gets mimalloc.
- Squirrel.Mac, which installs the update in an Electron build, relaunches the app by running
  ShipIt from the new bundle: `Contents/Frameworks/Squirrel.framework/Resources/ShipIt
  ___launch___ <bundle>`. The bundle ships a minimal framework whose ShipIt
  (`scripts/squirrel-relaunch.c`) only opens the bundle. Without it, the CI update from an
  Electron build installed the Rust app and left it stopped (`posix_spawn: No such file or
  directory`).
- No blockmaps. An Electron build that finds none for the new version downloads the whole
  zip or AppImage, and the Rust updater downloads whole payloads.
- Release CI signs `SHA256SUMS` with minisign (`SHA256SUMS.minisig`). The Rust updater checks
  it against `build/update-signing.pub`; [release signing](../release-signing.md) has the
  owner's steps.

### Staging inside the one PR

1. Scaffold the workspace, lints, `deny.toml` and CI jobs.
2. Record goldens from main with a committed script (`tests/golden/record.ts`). It is the oracle
   and cannot be rebuilt once `src/server` is gone.
3. Port `domain`, `store`, `net`, `core`, `gateway`, `accounts`, `agents`, `server` and `cli`,
   each proved against its goldens and ported tests.
4. Switch 1: the server archive ships the Rust daemon. The Bun compile path goes.
5. Build `shell` and `window`, with desktop smoke and lifetime tests.
6. Switch 2: packaging builds the Rust app. Electron, `src/server` and `src/desktop` go.
7. Docs, the before and after table, and the standards review.

No commit ships Electron on a Rust daemon. Each product has exactly one implementation at
every commit.

#### Switching packaging

`scripts/package-{mac,linux,server}.ts`, `verify-rust-package.ts`, `release-tools.ts`,
`rust-licenses.ts`, `sign-release.ts` and `.github/workflows/rust.yml` exist beside the
Electron and Bun paths until the switch commits, which:

- Switch 1: delete `scripts/build-server.mjs`; `dist:server` runs `package-server.ts`;
  `renderInstaller` defaults to `rustServerArchive`; `tests/server-archive-smoke.mjs` takes
  its entries and exit code from `scripts/` instead of `src/server/host-defaults.ts`;
  docs/server.md's compatibility section describes the static daemon.
- Switch 2: delete package.json `build`, `start`, `package`, `dist:mac`, `dist:linux`,
  `dist:built` and the Electron dependencies; `scripts/build-desktop.mjs` (keep its
  `THIRD-PARTY.txt` generation, which `stageLicences` needs, in `bun run build`),
  `sign-mac.cjs`, `validate-core.cjs`, `verify-package.cjs` and
  `build/entitlements.mac.plist`. Move the build-side core code (`core-binary.ts`,
  `core-install.ts`) to `scripts/`, which `install-core.mjs`, `pin-core.mjs` and the
  packaging scripts import. `verify-release.mjs` reads `product.feed` and `desktopTargets`
  from `scripts/distribution.ts`; the identity test in `tests/rust-distribution.test.mjs`
  compares with literals instead of package.json; the Electron tests in
  `tests/distribution.test.mjs` go.
- Workflows: delete the `desktop` and `server` jobs from `checks.yml` and move rust.yml's
  `checks`, `desktop` and `server` jobs there, with artifacts named `nonstopvibin-<os>` and
  `nonstopvibin-server-<arch>`; `build.yml` drops its `rust` job. The required checks
  `build / desktop (macos-26)` and `build / desktop (ubuntu-24.04)` keep their names;
  `build / server` becomes `build / server (x64)` and `(arm64)`, and `build / checks (...)`
  is new. Releases then build with the `release` environment's signing secrets.
- `tests/mac-update.mjs` builds its old version with electron-builder from the checkout.
  After switch 2 it must install a published Electron release's zip instead.

## Behaviour that differs from main

Everything else is meant to behave as main 4ea188a does; the goldens and the ported tests check
it. These differences are deliberate, each with its reason in the commit or test that made it.

- **Gateway targets.** The gateway checks the raw request target instead of the path the WHATWG
  URL parser produced, and refuses with 404 (403 under a foreign profile prefix) what main
  normalised or what Bun's parser answered 400: raw non-ASCII bytes, `*`, `.` segments and `\`.
  Nothing is forwarded either way (22 of the 1,290 gateway goldens, listed in
  `crates/gateway/tests/gateway/golden.rs`). An empty POST reaches the core with no
  `Content-Length: 0`, which HTTP reads as no body.
- **Upgrades.** Only a WebSocket upgrade (`Upgrade: websocket` with `Connection: upgrade`) takes
  the upgrade path; another protocol gets 400. Main forwarded any upgrade as a bodyless GET.
- **Data files.** `vault.key` and the database must be ordinary files the user owns, never links;
  the key must be 32 bytes. Main checked less. Decryption requires canonical base64 and UTF-8,
  which every value main wrote is.
- **Durability.** Credential and agent-configuration writes are synced before their rename.
- **Error statuses and text.** A core the pool cannot reach answers 502, not 500; a sign-in link
  or pasted URL that is not a URL answers 502 or 400, not 500. Messages that came from Bun or
  Node (`ENOENT …`, `Invalid URL`) are now Rust's own; every `AppError` message is unchanged.
- **Small parsing edges.** Usage counters are whole numbers (a fractional counter loses its
  fraction); a date-time without an offset reads as UTC (V8 used local time); a hand-edited
  retention value such as `"3e1"` reads as keep everything; an empty `HOME` is used as is.
- **Agent configuration.** Merged Claude settings keep the user's key order; numbers above
  2^53 in the user's JSON stay exact (main rounded them).
- **Credential socket.** A client that stalls is disconnected instead of answered 408; an
  oversized request head gets 431.
- **Desktop.** The Dock icon shows only while the main window exists; the desktop app writes
  `server.json` and holds `server.lock.sqlite`; the quota popup closes on focus loss and Escape
  (the Swift click monitor is gone); on Linux a missing WebKitGTK opens the session link in the
  browser; WebKit's default context menu is suppressed; on macOS the helper moves the window
  for drag regions, which gives up edge tiling and double-click zoom on those regions. The
  interface's `localStorage` starts empty once, so it forgets the last selected profile and page.
- **Start and quit failures.** The could-not-start and failed-quit dialogs carry the real reason
  as the one free-text field on the helper pipe (capped, control and bidirectional characters
  stripped, shown as plain native text). On Linux without WebKitGTK a desktop notification
  carries it instead.
- **Updates.** The update keeps `NONSTOPVIBIN_DATA_DIR` and `NONSTOPVIBIN_PORT`; once the
  release key is committed, a minisign signature over `SHA256SUMS` is required too.

## What would change these decisions

| Result                                                                     | Then                                                                          |
| -------------------------------------------------------------------------- | ----------------------------------------------------------------------------- |
| The macOS helper fails to come to the front once in 20 tries on `macos-26` | Start it as a nested `.app` through LaunchServices, over a Unix socket        |
| Focus loss fails to close the popup once in 20                             | An `NSEvent` monitor inside the helper                                        |
| The helper's first paint is over 150 ms slower than in-process `wry`       | Keep the helper alive for 30 s after the last close                           |
| `ksni` keeps the daemon above 20 MiB or 0.1 wakeups/s                      | Profile `zbus`; move the tray into the helper only if it cannot be made quiet |
| A small API call's p99 during the 1 MB × 8 burst exceeds main's            | Two tokio worker threads                                                      |
