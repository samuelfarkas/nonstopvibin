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
  embedded React UI, runs the CLI, and hosts the tray in desktop mode. On Linux it links no GUI
  library, so the same binary runs on a headless server.
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

### Code layout

Eleven crates under `crates/`, edition 2024, one `[workspace.lints]` table:

| Crate      | Owns                                                                                                                     | Depends on        |
| ---------- | ------------------------------------------------------------------------------------------------------------------------ | ----------------- |
| `domain`   | Ids, enums, wire types, the provider and agent registry, secret newtypes, the helper protocol. No I/O.                   | —                 |
| `store`    | Data directory layout, owner and symlink checks, atomic writes, vault, SQLite thread, lock.                              | `domain`          |
| `net`      | The loopback HTTP/1 client and the one HTTPS GET client (rustls with ring).                                              | `domain`          |
| `core`     | Core binary pin and hash checks, config, spawn, watchdog, ledger, restart policy, pool, management client, core updates. | `store`, `net`    |
| `accounts` | Accounts, quota, usage drain, the deadline scheduler, sign-in, pricing, model catalog.                                   | `core`            |
| `agents`   | The four agent setup writers, native config files, the credential socket.                                                | `store`           |
| `gateway`  | Key extraction, routing, header rewrite, HTTP, SSE and upgrade pass-through.                                             | `domain`          |
| `server`   | Listener, Host, Origin and token checks, `/api` routes, embedded UI, shutdown order.                                     | all above         |
| `shell`    | Tray, helper supervision, login item, app updater.                                                                       | `server`, `net`   |
| `window`   | The helper binary: `wry` window, bridge script, IPC, dialogs.                                                            | `domain`          |
| `cli`      | The daemon binary: arguments, `serve`, `url`, `status`, `service`, mode selection.                                       | `server`, `shell` |

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
- The allocator is chosen by measurement after the 1 MB × 8 burst. The options are glibc
  default, glibc tuned, static musl and mimalloc. Bodies pass through as frames with hyper's
  buffers capped.

### Data

- No migration. File names, SQLite schema and PRAGMAs, JSON bodies, vault format
  (base64 of `nonce(12) ‖ tag(16) ‖ ciphertext`, AES-256-GCM), core ledger and lock files stay as
  they are. This PR adds no table and no column.
- Timestamps keep the exact `toISOString()` shape, because SQL compares them as text.
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
- The Rust updater reads the same feed. On macOS it stages beside the bundle, verifies the code
  signature against the requirement and the bundled core against its manifest, swaps the bundle
  by rename with rollback, and restarts. On Linux it replaces the AppImage in place.
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
  `sign-mac.cjs`, `validate-core.cjs`, `verify-package.cjs`, the `swift` field of `architectures` and
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

## What would change these decisions

| Result                                                                     | Then                                                                          |
| -------------------------------------------------------------------------- | ----------------------------------------------------------------------------- |
| The macOS helper fails to come to the front once in 20 tries on `macos-26` | Start it as a nested `.app` through LaunchServices, over a Unix socket        |
| Focus loss fails to close the popup once in 20                             | An `NSEvent` monitor inside the helper                                        |
| The helper's first paint is over 150 ms slower than in-process `wry`       | Keep the helper alive for 30 s after the last close                           |
| `ksni` keeps the daemon above 20 MiB or 0.1 wakeups/s                      | Profile `zbus`; move the tray into the helper only if it cannot be made quiet |
| A small API call's p99 during the 1 MB × 8 burst exceeds main's            | Two tokio worker threads                                                      |
