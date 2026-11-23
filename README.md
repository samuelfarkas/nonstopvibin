<p align="center">
  <img src="public/icon.png" width="88" height="88" alt="NonstopVibin icon">
</p>

# NonstopVibin

Connect **Claude Code, Codex, and pi** to your AI subscriptions and API accounts.
NonstopVibin is a local desktop proxy for **macOS and Linux**, powered by
[CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI).

**[Install](#install)** · [Setup](#setup) · [Build from source](#build-from-source) · [Provider support](docs/providers.md)

_Screenshots use sample accounts and simulated quotas. [Capture notes](docs/media/production.md)._

## Accounts, quotas, and agents in one place

- **See your remaining quota.** Check account limits and reset times in the app or
  menu bar. Use **Fill first** to prioritize accounts or **Round robin** to spread
  requests. New profiles use fill first with session affinity enabled.
- **Keep account pools separate.** Work, personal, and project profiles each have
  their own key and endpoint. Requests use only that profile's accounts and fail
  when none can serve them.
- **Connect the agents you already use.** Sign in to Claude or ChatGPT/Codex,
  connect OpenCode Go with a subscription key, add a compatible API, or import
  CLIProxyAPI account files. Then open **Connect agents** for setup and launch
  instructions.

![Subscription accounts, remaining quotas, and reset times in the Work profile](docs/media/subscriptions.png)

## See where requests go

Review request history, token counts, errors, and estimated API value. Filter the
request log or export it as CSV. Charts, tables, and exports use the latest 500
requests in the selected period; summary totals cover the full period.

![Seven days of simulated activity, token usage, and request history](docs/media/activity.png)

Quota reporting depends on the provider. Available Codex banked resets can be
reviewed and used with explicit confirmation. Cross-provider models are opt-in
for Claude Code and Codex and use experimental protocol translation.

**Prerelease:** live subscription inference, quota checks, and reset redemption
remain unverified. See [provider support](docs/providers.md) for the tested scope.

## Install

Download the latest release from
[GitHub Releases](https://github.com/samuelfarkas/nonstopvibin/releases/latest).

- **macOS, Apple Silicon:** open the DMG and drag NonstopVibin to Applications.
  Apple has not notarized the app, so macOS blocks its first launch. Open
  **System Settings → Privacy & Security** and click **Open Anyway**.
- **Linux, x86_64:** run the AppImage, or install the deb package.

The macOS app and the AppImage then update themselves. They download each new
release in the background and install it when you choose **Restart to update**.
Deb installations update when you install the newer package.

## Setup

Install your coding agent separately, then:

1. Create a profile and choose **Add subscription**.
2. Sign in, enter an API key, or import an account file into that profile.
3. Start the profile and open **Connect agents**.
4. Select Claude Code, Codex, or pi and follow the connection and launch
   instructions. Claude Code requires a project folder.

Keep NonstopVibin and the profile running while you work. Selecting another
profile in the app changes the view, not an agent's existing conversation.
See [agent setup](docs/agent-setup.md) for client versions, model selection,
reconnecting, and disconnecting.

![Codex connected to the isolated Work profile, with its launch command](docs/media/connect.png)

This capture shows configuration in the simulator's isolated agent home. It does
not demonstrate a live Codex conversation.

Closing the window leaves the proxy running in the menu bar. **Quit** stops it.
**Start at login** is optional in Settings.

## Run on a Linux server

Install the headless build on a glibc-based Linux server for trusted co-located
agents, or under a dedicated protected user behind a separate consumer-owned
private inference gate. Open its operator UI from your laptop through SSH:

```sh
bun run server:deploy myserver   # build, copy, install a systemd user service
bun run server:open myserver     # tunnel and open the UI
```

See [Run on a Linux server](docs/server.md) for manual installation, provider
sign-in, Tailscale, upgrades, and the [protected-upstream contract](docs/server.md#protected-upstream)
(with Homeplane as a planned consumer example, not a bundled gate).

## Build from source

This is a prerelease. Install Git, [Bun 1.4.2](https://bun.com/docs/installation),
and [rustup](https://rustup.rs). `rust-toolchain.toml` pins the Rust version, and
rustup installs it on the first `cargo` command. On macOS, also install Xcode
Command Line Tools (`xcode-select --install`). On Linux, install the WebKitGTK 4.1
and GTK 3 development files, for example `libwebkit2gtk-4.1-dev` and
`libgtk-3-dev` on Debian and Ubuntu. From a checkout of this repository:

```sh
bun run setup
bun run build
bun run start
```

Setup installs locked dependencies and downloads the pinned CLIProxyAPI core.
`bun run build` builds the React interface into `dist/client` and the Rust
binaries in release mode. `bun run start` runs the desktop app from the checkout.
To create an installer, run the matching command on the target system:

| Release target       | Command              | Output in `release/` |
| -------------------- | -------------------- | -------------------- |
| macOS, Apple Silicon | `bun run dist:mac`   | DMG and ZIP          |
| Linux, x86_64        | `bun run dist:linux` | AppImage and deb     |

The Linux packages link the daemon statically against musl, so `dist:linux` also
needs `musl-tools` and `rustup target add x86_64-unknown-linux-musl`.
Packaged apps do not require Bun. Starting with 0.2.0, packaged macOS and AppImage
builds check GitHub Releases for updates; deb installations update manually.
Installed builds, deb included, also update the proxy core from Settings once a
reviewed core pin reaches `main`. See [releases and app updates](docs/releases.md).
Installation and tray behavior in an interactive Linux desktop session still need
verification. See [release requirements](SECURITY.md) before distributing builds.

For development, use `bun run dev`. Development shares `~/.nonstopvibin` with the
desktop app by default, so quit the app first or use a separate data directory as
described in [Development](docs/development.md). See [Contributing](CONTRIBUTING.md)
for checks.

## Resource use

The app is one Rust daemon, `nonstopvibin`, plus one CLIProxyAPI core per running
profile. The daemon serves the agent gateway, the management API, and the
interface. A separate `nonstopvibin-window` process shows the window and exits
when you close it, so no web engine runs while the app sits in the menu bar.

These figures compare the daemon with the Bun server and the Electron app it
replaced, on the same synthetic workload with three profiles. They cover the host process only, not the cores.

| Measured on Linux x64                         | Bun server    | Electron, no window          | Rust daemon  |
| --------------------------------------------- | ------------- | ---------------------------- | ------------ |
| Startup to first accepted connection          | 48–101 ms     | 164–995 ms                   | 2.8–3.0 ms   |
| Memory (PSS) while idle                       | 43.5–43.7 MiB | 178.6–185.1 MiB              | 9.4–11.4 MiB |
| Wakeups per second, 2–3 minutes after a burst | 21.9–51.6     | 1.9–2.0                      | 0.0          |
| Memory (PSS) right after eight 1 MB requests  | 88–99 MiB     | 82–84 MiB, main process only | 10.0 MiB     |

The Rust column is the static musl build that the Linux packages ship, run as the
headless server. As the desktop app with its tray registered and no window open, the
daemon measured 10.6–10.8 MiB and no wakeups, and 12.8–13.8 MiB after the same burst.
Opening the window starts the window process in 277–282 ms; it uses about 350–410 MiB
while open and exits within about 34 ms of the last close. See
[the measurements](docs/decisions/rust-port-evidence.md) for the method, CPU per
request, and macOS gaps.

## Data and privacy

Data lives in `~/.nonstopvibin`, accessible through **Settings → Open data folder**.
Activity stores request metadata and token counts, not prompts or responses.
History is kept by default; optional retention deletes records older than 30, 90,
or 365 days. Usage can lose events during a crash; API value estimates are not
subscription charges.

The proxy listens on `127.0.0.1`. Profile keys cannot access management endpoints.
SQLite secrets use a local, owner-only `vault.key`; CLIProxyAPI also requires
owner-only plaintext credential files. Software running as your OS user can read
these credentials. Profiles separate account pools, not OS users. Adding the same
subscription to multiple profiles still shares its provider quota.

See [storage and reset instructions](docs/architecture.md) for details. Report
vulnerabilities through [Security](SECURITY.md), and keep credentials out of issues.

## License

[MIT](LICENSE), with [third-party notices](licenses/NOTICE.md) for bundled components.
Provider names and logos belong to their owners. NonstopVibin is not affiliated
with those providers.
