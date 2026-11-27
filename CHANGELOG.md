# Changelog

## [Unreleased]

### Added

- **Claude Agent**: a profile's Claude subscription is served by your own signed-in
  Claude Code. Claude Code makes the requests to Anthropic itself, with its own
  identity and the entrypoint label `nonstopvibin`; the harness's tools reach it
  over a local MCP server. **Sign in → Claude Agent** shows the sign-in command for
  the profile and connects it. A profile holds one Claude Agent subscription. This
  is lower risk than a proxy that imitates Claude Code, not sanctioned use; see
  [Claude Agent](docs/providers.md#claude-agent).

- T3 Code can show a profile's subscription limits. Each profile serves a
  CLIProxyAPI-compatible usage hub at `http://127.0.0.1:<port>/p/<slug>` behind its
  own hub key. **Connect agents → Show limits in T3 Code** shows the URL and the key
  and replaces the key. The hub answers on the loopback address only, sends only
  the provider calls T3 Code makes for Codex and Grok with the app's own headers,
  and never starts a core. See [Show limits in T3 Code](README.md#show-limits-in-t3-code).

### Removed

- **The Claude sign-in.** The proxy core no longer signs in to Claude or sends other
  harnesses' requests dressed up as Claude Code. On its first launch this release
  deletes every profile's Claude sign-ins, their credential files and their
  accounts; connect Claude Agent instead. Claude quota readings, Claude usage
  resets, organization seats and reconnecting go with it, and the desktop app no
  longer opens Claude's pages. A release from before Claude Agent cannot read a
  store that holds a Claude Agent account, so going back to one is not supported.

## [0.3.1] - 2026-11-26

### Fixed

- Claude subscriptions now list the limit resets Claude offers them, such as Fable
  resets, instead of "No resets available". Claude answers these calls only for
  Claude Code, so the app now sends them with the Claude Code identity its proxy
  core already uses, as CLIProxyAPI's Management Center does. See
  [Claude usage resets](docs/providers.md#claude-usage-resets) for the account risk.
- Claude resets are listed one row per kind (Fable, full, 5-hour, weekly) with
  how many are left, as Claude Code offers them. Every usable kind can be
  redeemed, not only the grant Claude would use next, and a kind that cannot be
  used yet says why. When Claude offers a subscription no resets, the panel says
  why instead of "No resets available".

## [0.3.0] - 2026-11-26

The first tagged and published release. Its changes include everything listed
under 0.2.0.

### Added

- The CLIProxyAPI core updates from **Settings → Proxy core** without a new app
  release. Installed desktop builds and the packaged server offer a newer core once
  its reviewed pin is on `main`, install it only when its archive and executable
  match that pin, check that it starts, then restart running profiles on it.
  **Use bundled** goes back to the core the app shipped with. A dot on Settings
  marks an available update. `bun run core:pin` writes the pin for an upstream
  release, and a workflow opens a pull request for each new upstream release.
- Claude subscriptions show the usage resets Claude offers them in their details
  and can redeem one after confirmation, as Codex banked resets already could.

### Fixed

- A limit reset done on claude.ai, chatgpt.com or in the app no longer leaves the
  subscription cooling down in its proxy core until the old reset time: when a
  quota check shows room in every window again, the app clears the core's quota
  cooldown for that account. Quota is checked while the app's window is open.

### Changed

- NonstopVibin is now one Rust daemon, `nonstopvibin`, instead of Electron and the
  TypeScript server. It serves the agent gateway, the management API and the same
  React interface, runs the CLI and, on the desktop, the tray. A separate
  `nonstopvibin-window` process shows the window and the quota popup and exits
  when the last one closes, so no web engine runs while the app sits in the menu
  bar. On Linux x64 the daemon starts in 2–3 ms and idles at 9–11 MiB with no
  wakeups, against 179–185 MiB for Electron without a window and 44 MiB for the
  Bun server. Data, profiles, agent connections and the update feed are
  unchanged, and installed builds update onto the Rust app. See
  [the decision record](docs/decisions/rust-port.md) and
  [the measurements](docs/decisions/rust-port-evidence.md).
- App updates check a minisign signature over the release's `SHA256SUMS` once the
  release key is committed, beside the SHA-512 check, and on macOS the new
  bundle's code signature and core. An update now starts the new version with
  the old one's environment, so `NONSTOPVIBIN_DATA_DIR` and `NONSTOPVIBIN_PORT`
  survive it.
- The desktop app now writes `server.json` and holds `server.lock.sqlite` in the
  data folder, as the server does. A second launch uses them to show the running
  app's window. On Linux the window keeps its web storage in `webview/` there.
- On macOS the Dock icon shows only while the main window is open. The quota
  popup closes when it loses focus or on Escape.
- On Linux, if the window cannot start, for example without WebKitGTK 4.1, the
  app opens its session link in the system browser.
- The agent gateway checks the raw request target and refuses, with 404, targets that
  the old server normalised: raw non-ASCII bytes, `*`, `.` segments and `\`. Only a
  WebSocket upgrade is forwarded as one; other upgrade requests get 400.
- `vault.key` and the database must be ordinary files you own, never symbolic links, and
  the key must be 32 bytes; the app refuses to start otherwise. Credential and agent
  configuration writes reach the disk before they replace the old file.
- The server archive's `nonstopvibin` is a static musl executable. The bundled
  core still needs glibc.
- The reviewed core pin moved to `crates/core/core-release.json`. Installed
  TypeScript builds read the old path on `main`, so they offer no new core until
  they update to the Rust app.
- Building from source needs a Rust toolchain (rustup reads
  `rust-toolchain.toml`) and, on Linux, the WebKitGTK 4.1 and GTK 3 development
  files.
- The macOS app now installs its own updates. Releases are ad-hoc signed, and an
  installed app accepts any new version with its bundle identifier, so updates no
  longer fail Squirrel.Mac's signature check. Releases need no Apple signing secrets. The
  first launch of a downloaded app still needs **Open Anyway** in
  **System Settings → Privacy & Security**. Run from the disk image or a folder you
  cannot change, the app does not check for updates, and **Check for updates…**
  explains how to move it.
- Upgraded bundled CLIProxyAPI from 8.0.67 to 8.1.0 with reviewed archive and binary
  hashes for all four targets. It brings upstream's stream failover, Antigravity and
  Codex fixes. On its first start each profile core writes a generated `server-id`
  into that profile's config file; NonstopVibin does not use it.

### Removed

- `bun run preview`, the sample-data simulator, left with the TypeScript server.
- Releases no longer publish blockmaps. Installed Electron builds download the
  whole update instead of a difference.

## 0.2.0 - 2026-11-17

Never tagged or published; its changes first ship in 0.3.0.

### Added

- Headless Linux server: `bun run dist:server` builds a self-contained
  `nonstopvibin` archive for Debian 12+ (x64 and arm64) with `install.sh`, a
  systemd user service, and `serve`, `url`, `status` and `service` commands.
  `bun run server:deploy` and `bun run server:open` install it and open its UI
  through an SSH tunnel. Optional `--allow-host` admits one exact HTTPS host,
  such as Tailscale Serve, for the web UI only. See [the server guide](docs/server.md).
- In a browser, the sign-in dialog now opens its paste-callback field by default,
  and an expired session after a server restart shows the recovery screen.

### Fixed

- Profile proxy cores are now stopped when NonstopVibin crashes, is force-quit or is
  killed. Each core has a small watchdog that stops it as soon as the app process
  ends, and the next start stops any core that a crashed instance left running for
  the same data folder.
- A profile core that does not become ready within 15 seconds is now stopped instead
  of being left running.
- Cancelled agent requests no longer show up as internal errors. When a profile's
  proxy cannot be reached, agents now get "The profile proxy is unavailable"
  instead of an internal error.
- The connection error banner now clears by itself once the app reaches its local
  service again.
- The menu bar quota popup can always be closed, even while it shows an error or
  notice, and a popup that failed to load opens again on the next click.
- The NonstopVibin wordmark now uses a bundled Space Grotesk font, so it no longer
  depends on a locally installed copy and looks the same on every system.
- The core now refreshes its upstream subscription model catalog at startup and
  every three hours, so new subscription models can appear without a core update.
- pi keeps an explicitly launched NonstopVibin model instead of switching to the
  repository's saved choice, including in forked sessions such as subagents. Models
  newer than pi's bundled catalog use pi's refreshed catalog, so their thinking
  setting is no longer rejected.
- Upgraded bundled CLIProxyAPI from 7.2.151 to 8.0.67 with reviewed archive and binary
  hashes for all four targets. The core now presents the Claude Code 2.1.321 client
  baseline, so Claude subscriptions can use Claude Fable 5.2 and Haiku 5.5; 8.0.8
  requests for these models were rejected with "version 2.1.290 or newer is required".
  Pi native tool changes now retain their beta headers
  and matching tool aliases; use `/reload` after app startup updates Pi extensions.
- Errored profiles can now be restarted directly from the sidebar.
- API account and credential updates now roll back together when a write fails.
- Malformed sign-in callback URLs now receive a safe error response without stopping the app.
- Subscriptions whose provider rejected the saved sign-in now show **Reconnect needed**
  instead of **Cooling down**.
- Renaming, pausing or reprioritising a signed-in subscription of a stopped profile
  now asks you to start the profile instead of failing with an internal error.
- Settings changed while a profile is starting, and API-key subscriptions paused,
  renamed or removed meanwhile, now reach its proxy once it is ready instead of
  waiting for the next change or restart.
- A packaged app whose bundled proxy core is missing now refuses to start and asks
  you to reinstall, instead of opening and failing each time a profile starts.
- When the main window cannot be opened from the menu bar popup, the reason now
  appears in the app's error list instead of nothing happening.
- A profile with subscriptions whose proxy exited or failed to start, even on its
  first start, is now started again automatically, retrying a failing proxy less
  often up to every five minutes, so its quotas keep updating; Refresh starts it
  too, instead of reporting each quota check as failed. After 10 failed starts in a
  row the app stops retrying and the profile says so until you use Restart.

### Security

- CSV exports now neutralize whitespace-prefixed formulas, and release signing secrets are limited to the macOS signing steps.
- Core archives and extracted binaries now use reviewed source pins before packaging.
- Packaged resources now exclude unapproved license files and reject secret-like paths.
- Custom agent configuration roots now reject shared write access.
- Invalid quota responses no longer persist provider response excerpts in error text.
- Updated sharp, oxfmt (tinypool), source-map-js, and http-cache-semantics, plus the DeepSec workspace's MCP SDK and proxy-addr, to resolve high and critical audit advisories.
- Updated Electron to 44.9.1, Vite to 8.2.4, Zod to 4.6.6 and DeepSec to 2.3.10,
  with patched undici versions, to resolve high audit advisories.
- macOS signing gives the bundled proxy core and menu bar click helper the hardened
  runtime without the JIT and executable-memory entitlements that Electron needs.
- Packaged builds now also ignore `--inspect` debugging arguments and withhold extra
  `file://` privileges, in addition to the run-as-Node and `NODE_OPTIONS` overrides.
- Agent WebSocket connections no longer receive upstream cookies or CORS headers,
  matching ordinary HTTP responses.

### Changed

- Reconnecting a Claude subscription now finishes on its own after the browser
  sign-in; the organization review remains for new connections and mismatches.
  The desktop window comes forward when the browser sign-in returns.
- Installed agent connections now update automatically on app startup, including
  stopped profiles and every connected Claude project. Existing user settings are
  preserved; edited or removed integrations require manual repair.
- Desktop and development now share `~/.nonstopvibin` and an owner-only AES-GCM
  vault key. Linux no longer requires a secret service. Older data directories
  are not imported automatically; see [storage migration](docs/architecture.md).
- Added optional activity retention, full-period usage totals and estimated API
  value, plus manually confirmed Codex banked resets.
- Main and quota windows now unload when closed, preserving unfinished forms;
  the menu bar icon stays static and logo animations finish instead of looping.
- Startup failures are now also printed to the terminal.
- Packaged macOS and AppImage builds now check GitHub Releases for app updates 30
  seconds after startup and every six hours, and offer **Check for updates…** in the
  app and tray menus. Updates download in the background and install only after you
  choose **Restart to update**; Linux deb and development builds do not update
  themselves.
- pi remembers the last profile and model per Git repository; a worktree without
  its own choice inherits the main checkout's. Its model picker shows only the
  active profile's models.
- Settings shows the version of the bundled CLIProxyAPI core.
- Less work while idle: usage collection sleeps once traffic has settled instead of
  checking every 30 seconds, repeated collection failures back off to once a minute,
  account checks skip unchanged credential files, and request routing and the menu
  bar no longer read every profile from the database. The Activity page no longer
  recomputes its charts on every status refresh.
- Quitting now stops all profiles at the same time instead of one after another, so
  shutdown no longer takes longer with every profile, and one profile that fails to
  stop no longer keeps the others from finishing their usage collection.
- **Save name** in Settings stays disabled until the name changes.
- The menu bar popup now names quota states as the main window does (**Paused**,
  **Check failed**, **Stale**) and shows each quota window with the main window's
  meter and wording; an empty provider pool on the Subscriptions page is now red.
- This first published release exposes Claude Code, Codex, and pi as agents, and Claude, Codex, OpenCode Go, and custom API-key providers. Kimi, Antigravity, xAI, and the OpenCode agent stay in the code but are hidden from the app.
- Packaged Electron builds now disable run-as-Node and `NODE_OPTIONS` environment overrides.

### Removed

- The unused local management route `POST /api/profiles/:id/stop` and the unused
  `/api/state` fields `coreAvailable`, `storage`, and per-profile `usage` and
  `lastSyncedAt`.

## 0.1.1 - 2026-09-07

Release candidate only; it was never tagged or published. Its changes first ship in 0.2.0.

- Added separate profiles and account pools with round-robin and fill-first routing.
- Added native connections for Claude Code, Codex, and pi.
- Added quota windows and reset times in the main app and menu bar popup.
- Added activity history with observed token usage and CSV export.
- Added encrypted desktop storage through Electron safeStorage. CLIProxyAPI's
  required plaintext files remain restricted to the current OS user.
- Added macOS DMG and Linux AppImage/deb packaging. macOS builds are unsigned and
  not notarized.

[Unreleased]: https://github.com/samuelfarkas/nonstopvibin/compare/v0.3.1...HEAD
[0.3.1]: https://github.com/samuelfarkas/nonstopvibin/releases/tag/v0.3.1
[0.3.0]: https://github.com/samuelfarkas/nonstopvibin/releases/tag/v0.3.0
