# Contributing

## Prerequisites

Install Bun 1.4.2, Git, and [rustup](https://rustup.rs); `rust-toolchain.toml`
pins the Rust version. On macOS, install Xcode Command Line Tools
(`xcode-select --install`). On Linux, install the WebKitGTK 4.1 and GTK 3
development files. [Development](docs/development.md#prerequisites) lists the
packages.

## Setup

```sh
bun run setup
```

This installs the locked dependencies and the checksum-pinned CLIProxyAPI core.

Keep direct JavaScript dependencies pinned to exact versions in both package
manifests. Declare Rust dependencies once in the root `Cargo.toml` and commit
`Cargo.lock`.
Bun saves exact versions by default; use `bun ci` for routine installs so transitive
versions and integrity hashes stay locked too. Upgrade deliberately with
`bun add --exact <package>@<version>` (add `--dev` for development dependencies),
review the manifest and lockfile diff, and run the dependency audit and checks.
For scanner updates, work inside `.deepsec` and run `bun run security:tooling`
from the app root. Dependabot updates are reviewable pull requests, not automatic
installs. Version pins do not establish that a package is safe.

## Development

```sh
bun run dev      # Rust daemon and Vite development server
bun run start    # The desktop app, with its tray and window
```

CI runs every check on your pull request ([SECURITY.md](SECURITY.md#required-checks)
says where they are defined). Before opening one, run `bun run quality` (it
includes `cargo fmt`, clippy and the drift checks) and `bun run test`, and
`bun run build` when changing runtime or build boundaries. Format
touched files with `bunx --no-install oxfmt --write <files>` before the final checks.
Documentation-only changes need formatting and a diff review.

Use [the commit conventions](docs/commits.md) for commit messages, scopes, and
history cleanup.

Keep pull requests small and focused. Explain what changed, why it changed, and
which checks you ran. Include screenshots for visible UI changes.

Never put real credentials, API keys, tokens, request bodies, or provider error
responses in issues, tests, fixtures, or commits. Report vulnerabilities through
the private channel in [SECURITY.md](SECURITY.md), not a public issue.

[AGENTS.md](AGENTS.md) documents the architecture invariants and repository-specific
checks that all changes must preserve.
