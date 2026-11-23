# Development

For client prerequisites, model catalogs, removal, and troubleshooting, see
[Quick agent setup](agent-setup.md).

NonstopVibin is a Rust workspace in `crates/` and a React interface in `src/`.
[The decision record](decisions/rust-port.md) explains the process model and what
each crate owns.

## Prerequisites

Install these on the OS and architecture you are targeting:

- Git and [Bun 1.4.2](https://bun.com/docs/installation). `package.json` pins Bun
  for CI.
- [rustup](https://rustup.rs). `rust-toolchain.toml` pins Rust 1.100.0 with
  clippy and rustfmt, and rustup installs it on the first `cargo` command.
- On macOS, Xcode Command Line Tools (`xcode-select --install`).
- On Linux, the WebKitGTK 4.1 and GTK 3 development files for the window
  helper. On Debian and Ubuntu, install `libwebkit2gtk-4.1-dev` and
  `libgtk-3-dev`. The Linux packages and the server archive also need
  `musl-tools` and the `x86_64-unknown-linux-musl` or
  `aarch64-unknown-linux-musl` Rust target.

## Run from source

```sh
bun run setup       # Locked dependencies + checksum-verified proxy core
bun run dev         # Daemon + Vite; open the session URL printed in the terminal
```

`bun run dev` runs `cargo run --bin nonstopvibin -- serve` with
`NONSTOPVIBIN_DEVELOPMENT=1` and Vite beside it. In this mode a build from the
checkout also accepts the Vite server's Host and Origin, and starts without a core
when `.vendor/core` is missing. Vite proxies `/api` to the daemon and updates the
interface as you edit. Restart `bun run dev` after Rust changes. Ctrl+C stops
both processes.

Development defaults to `~/.nonstopvibin`, the same data folder as the desktop app,
and the daemon refuses to start while another one holds that folder. Quit the
desktop app first, or run
`NONSTOPVIBIN_DATA_DIR=.test-runtime/dev NONSTOPVIBIN_PORT=4319 bun run dev` for
isolated development. Older `.runtime` data is not imported automatically.

`bun run start` builds the window helper and runs the desktop app from the
checkout, with its tray and window. To run the headless server from source, run
`bun run build:client` once, then `cargo run --bin nonstopvibin -- serve`.
`cargo run --bin nonstopvibin -- --help` lists the server commands.

A binary without `core/` beside its real path is a development run. It serves
this checkout's `dist/client` and starts `.vendor/core/cli-proxy-api`, both found
from the crate's path at compile time, and it never installs core updates. It
reads the interface from disk on each request, so after `bun run build:client` a
reload shows the new UI without a restart.

## Checks and builds

```sh
bun run quality       # TypeScript, Oxlint, Oxfmt and registry drift checks
bun run format        # Format TypeScript sources, tests, scripts and configuration
bun test              # TypeScript tests; add --watch to rerun while editing
cargo test -p nsv-domain --locked   # One crate's tests; --workspace for all
bun run test          # bun test, then cargo test for the whole workspace
bun run build         # The interface, then the Rust binaries in release mode
bun run build:client  # The interface alone, into dist/client
bun run test:ui       # UI checks in the real window helper (Xvfb on Linux)
bun run test:server   # Installs and runs the server archive (Linux, after dist:server)
bun run dist:mac      # DMG and ZIP, on macOS
bun run dist:linux    # AppImage and deb, on Linux
bun run dist:server   # Headless server archive for this host's architecture
```

CI also runs `cargo fmt --all --check`,
`cargo clippy --workspace --all-targets --locked -- -D warnings`, and `cargo machete`
for unused dependencies. Run them before pushing Rust changes.

Some tests need a display or a session bus. On Linux, run them under Xvfb and
`dbus-run-session`, as CI does:

```sh
xvfb-run -a dbus-run-session -- cargo test --workspace --locked
sh crates/shell/tests/gui.sh --locked
```

`crates/shell/tests/gui.sh` runs the shell's tests that drive a real window
helper. `bun run test:ui` runs `crates/window/tests/ui.sh`, which builds the
interface, the daemon and the helper, then checks focus, the logo, page layout,
the close guard and, on Linux, the error dialogs and signals. On macOS the
terminal needs the Accessibility permission.

`dist:server` runs `scripts/package-server.ts` after `bun run build:client`. It
builds the static musl daemon with cargo, stages the pinned Linux core in a
temporary directory (your own `.vendor/core` is untouched), and writes
`release/nonstopvibin-server-<version>-linux-<arch>.tar.gz`. Pass `x64` or
`arm64` to choose the architecture; each needs its musl target and a C compiler
for it, so CI builds each on a native runner. See
[Run on a Linux server](server.md).

The app update tests run on CI runners, not on a workstation:

- `bun run test:update:mac` updates an Electron build, made from a checkout of
  main 4ea188a in `NSV_ELECTRON_CHECKOUT`, to the packaged Rust app.
- `bun tests/mac-update-rust.mjs` updates an older Rust build in place.
- `tests/linux-update.sh` updates an installed AppImage in place. Run it in a
  Linux container or VM; its header lists what it needs.

`tests/downgrade/read-with-ts.ts` proves that the last TypeScript store opens a
data directory the Rust store wrote. CI runs it against a checkout of main 4ea188a, the last TypeScript release.

For memory comparisons, use a packaged build with the same enabled profiles and
distinguish idle, UI-open, and request-load measurements. Measure the daemon and
the window helper separately; summed RSS includes shared pages. The figures in
[the evidence record](decisions/rust-port-evidence.md) say how each was measured.

## Dependencies and linting

Use `bun ci` to restore the exact JavaScript dependency versions, `bun add <package>`
to add one, and `bun update` for intentional updates. Commit `bun.lock` with
`package.json`, and `Cargo.lock` with any `Cargo.toml` change. Rust dependencies
are declared once in the root `Cargo.toml` under `[workspace.dependencies]`, and
`cargo deny` checks them against `deny.toml`. CI reads the Bun version from
`packageManager` and uses a frozen install. No global Vite or TypeScript CLI is
needed.

Linting uses [Oxlint](https://oxc.rs/docs/guide/usage/linter), including the migrated
JavaScript/TypeScript rules and React Hooks checks, with zero warnings allowed.
[Oxfmt](https://oxc.rs/docs/guide/usage/formatter) preserves the existing 80-column
style without sorting imports or package keys. Configuration lives in
`.oxlintrc.json` and `.oxfmtrc.json`; generated files, local runtime data, vendored
skills, and the separate `.deepsec` workspace are excluded. VS Code and Cursor
recommend the official Oxc extension; install it to use the included format-on-save
settings. `bun run check` checks TypeScript alone; `bun run format:check` checks formatting.
Rust uses the workspace lints in `Cargo.toml`, `clippy.toml` and `rustfmt.toml`.

The [anti-slop](https://github.com/dmmulroy/anti-slop) Oxlint plugin (v0.1.2, commit
`e8c4880`, MIT) is vendored under `tools/oxlint/anti-slop/` and loaded through
`@oxlint/plugins`. All fifteen rules run as errors on TypeScript sources; the
vendored copy matches upstream except for one documented local change in
`rules/no-runtime-typeof.ts`: the `typeof x === "undefined"` exemption requires
an identifier operand. It remains excluded from linting and formatting.

## Wire types and the registry

The interface's wire types in `src/shared` have Rust twins in `nsv-domain`.
`cargo test -p nsv-domain` writes the `ts-rs` output to `tests/types/generated`,
and `bun run check` asserts that each pair is mutually assignable. Commit the
generated files. Provider, agent and vocabulary data stays in TypeScript. After
changing it, run `bun run registry:export`, which rewrites
`crates/domain/data/registry.json`; `bun run quality` fails while the two differ.

## Linux desktop

Linux does not require a desktop secret service. GNOME may require an AppIndicator
extension to show a tray icon; the View menu's quota command remains available. If
the window helper cannot start, for example without WebKitGTK, the daemon opens the
session link in the system browser. A desktop Linux session remains untested. CI
([checks.yml](../.github/workflows/checks.yml)) runs the workspace tests and the
window helper's UI checks under Xvfb and a session bus on Linux, and packages and
tests updates on Linux and macOS. Xvfb does not replace testing installation and
tray behavior in a desktop session.

## The proxy core

The core installer pins the version and all four supported archive checksums in
`crates/core/core-release.json`, then checks the downloaded release against those
pins. Packaging also checks the core platform, architecture, and binary hash,
preventing a Mac core from accidentally being included in a Linux build.
`bun run core:pin [version]` writes a pin from an upstream release; reinstall it
with `bun run core:install` and run the tests before packaging. Development runs
never install or select core updates: they share the data folder with the desktop
app, and an installed core would shadow the pin under test.
[Updating the CLIProxyAPI core](releases.md#updating-the-cliproxyapi-core)
describes how installed builds receive one.

## Contributor guidance

The project includes reviewed skills in `.agents/skills`; no global installation is
required. Read [AGENTS.md](../AGENTS.md) for task routing and
[stack guidance](stack-guidance.md) for Rust, SQLite, Bun, TypeScript,
React, and Vite. `bun run setup` uses the checked-in copies without downloading
latest skill content. Sources, exact reviewed revisions, and the update policy are
recorded in [security tooling](security-tooling.md). UI work follows the
[visual system](design/DESIGN.md); app and tray icons are described in
[icons](design/icons.md).

## Package checks

The packaging scripts (`scripts/package-mac.ts`, `package-linux.ts` and
`package-server.ts`) build the Rust binaries, copy `dist/client` to `client/`
beside the daemon (`Contents/Resources/client` in the macOS bundle), and stage the
pinned core and licenses. `scripts/verify-rust-package.ts` then checks each
artifact: the exact file layout, the pinned core's version and hash, each
executable's platform and architecture, a static Linux daemon, macOS executables
that link only system libraries, the licenses, and no private files. Run it on a
built `.tar.gz`, `.AppImage`, `.deb` or, on macOS, `.zip` with
`bun scripts/verify-rust-package.ts <artifact>`. Packaging creates local artifacts
only and publishes nothing. `bun run build:client` writes the interface's
dependency licenses to `dist/licenses/THIRD-PARTY.txt`, and
`scripts/rust-licenses.ts` writes the Rust crates' licenses to
`licenses/THIRD-PARTY-RUST.txt` in each package. Follow the
[macOS release signing guide](release-signing.md) for signing, then see
[SECURITY.md](../SECURITY.md) for release requirements.

Run `bun run security:install`, `bun run security:secrets`,
`bun run security:history`, and `bun run security:deps` before sharing source.
The optional scanner workspace has its own locked dependencies and
`bun run security:tooling` audit.

`bun run icons` regenerates the [app and tray assets](design/icons.md) from the
mirrorball generator.
`scripts/check-pi-profiles.mjs` is an optional compatibility check against an
installed pi package; pass that package's directory as its argument.
