# Contributing

## Prerequisites

Install Bun 1.4.2 and Git. On macOS, install Xcode Command Line Tools
(`xcode-select --install`) to build the native menu bar helper.

## Setup

```sh
bun run setup
```

This installs the locked dependencies and the checksum-pinned CLIProxyAPI core.

Keep direct dependencies pinned to exact versions in both package manifests.
Bun saves exact versions by default; use `bun ci` for routine installs so transitive
versions and integrity hashes stay locked too. Upgrade deliberately with
`bun add --exact <package>@<version>` (add `--dev` for development dependencies),
review the manifest and lockfile diff, and run the dependency audit and checks.
For scanner updates, work inside `.deepsec` and run `bun run security:tooling`
from the app root. Dependabot updates are reviewable pull requests, not automatic
installs. Version pins do not establish that a package is safe.

## Development

```sh
bun run dev      # Local API and Vite development server
bun run preview  # Isolated simulator with synthetic accounts
```

CI runs every check on your pull request ([SECURITY.md](SECURITY.md#required-checks)
says where they are defined). Before opening one, run `bun run quality` and
`bun test`, and `bun run build` when changing runtime or build boundaries. Format
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
