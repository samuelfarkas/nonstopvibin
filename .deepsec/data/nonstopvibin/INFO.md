# nonstopvibin

## What this codebase does

A desktop app for macOS/Linux, and a headless Linux server, that separates AI
subscription accounts into profiles and exposes profile-specific
OpenAI/Anthropic-compatible endpoints to coding agents. One Rust daemon,
`nonstopvibin` (crates in `crates/`), serves the gateway, the management API and
the React 19/Vite UI on loopback, with `rusqlite` for storage and hyper for HTTP.
On the desktop it also runs the tray, and a separate `nonstopvibin-window` helper
is the only code that links WebKit. Each profile runs an unmodified pinned
CLIProxyAPI Go binary. Bun drives development, TypeScript tests and the packaging
scripts. There is one upstream core process, credential directory, and account
pool per profile. Cross-profile credential fallback is forbidden, even under
exhaustion.

## Auth shape

- `Checks::admit` in `crates/server/src/checks.rs` checks the Host (`valid_host`,
  `allowed_host`) and Origin before the `/api/` branch reaches management routes.
  Management uses a per-launch random session token distinct from profile API keys.
- `route` in `crates/gateway/src/route.rs` authenticates profile keys with
  `same_secret` (constant time), binds any `/p/slug` to that profile, and
  allowlists forwarded paths (`allowed`). WebSocket upgrades must retain the same
  auth boundary as HTTP and SSE. The gateway's `Lease` carries no management key,
  and the gateway crate cannot name the management client.
- `parse` in `crates/window/src/bridge.rs` accepts bridge calls only from the main
  frame of a page on the daemon's origin, with a length cap and a fixed method
  list. The helper forwards `openExternal`, `getLoginItem`, `setLoginItem` and
  `revealData` to the daemon over its stdio pipe, and the daemon validates them
  again. The session token reaches the helper on stdin only.
- `safe_external` in `crates/shell/src/external.rs` restricts browser launches to
  HTTPS provider hosts. OAuth flow state and profile ownership are enforced by the
  core pool and the sign-in code in `crates/accounts`.
- Desktop, development and the server use `Vault` (`crates/store/src/vault.rs`):
  AES-256-GCM with an owner-only `vault.key` beside SQLite in `~/.nonstopvibin` (or
  the explicit data override). No system keychain is required; access to both
  files permits decryption.

## Threat model

Highest impact is leaking subscription credentials or routing a work request
through another profile. Treat remote websites, page input to the window helper,
imported auth files, provider responses, and proxy client requests as untrusted.
Management access must not follow from possession of a profile key. A malicious
upstream release, app update or packaging mistake can execute native code on the
user's machine.

The gateway is intentionally loopback-only; this is not a multi-user hosted
service. Processes running as the same OS user can read owner-only files; profile
separation does not claim an OS sandbox between accounts. The upstream core needs
plaintext generated configuration and OAuth files in 0700/0600 storage.

## Project-specific patterns to flag

- Forwarding a new core route without `route` and `allowed` restrictions, or
  forwarding management paths or client credentials through `forwarded` in
  `crates/gateway/src/headers.rs`.
- A pool fallback, retry, refresh, pause/resume, or usage attribution path that
  uses another profile's auth directory, management key, or account pool.
- New bridge methods in `crates/window/src/bridge.rs`, navigation or new-window
  handlers that allow other origins, devtools in release builds, or a daemon
  handler for a forwarded call that skips its own argument checks.
- `ImportCatalog` accepting page-provided paths, following links, or letting
  token-bearing content reach error messages, logs, activity records, or exports.
- Bypasses of `install-core.mjs` checksums, the core pin checks in
  `crates/core/src/binary.rs`, or `scripts/package-layout.ts`; credentials,
  local runtimes, scanner reports, or keys entering packaged resources.
- Changes in `crates/shell/src/update` that install an update without its
  minisign signature, SHA-512 or, on macOS, the designated-requirement check.
- Changes in `crates/core/src/processes.rs` that signal a process without matching
  its recorded PID, start time, and full command line, or that reap cores outside
  this data folder.
- `unsafe` code outside the macOS shell's scoped `SMAppService` calls.

## Known false-positive context (verify rather than blanket-suppress)

- Tests use synthetic company/personal keys and local HTTP fixture servers;
  these exercise the real core without live provider credentials.
- `ProviderUrl` in `crates/accounts/src/api_accounts.rs` intentionally permits
  HTTP for loopback custom providers only. Authenticated users may configure HTTPS
  providers; inspect reachability before calling every user-selected URL a
  remotely exploitable server-side request.
- `Vault` deliberately stores the encryption key in an owner-only file in every
  mode; this does not protect against the same OS user.
- `tests/golden/record.ts` and `tests/fixtures/ts-data-dir` hold recordings and a
  data directory from the TypeScript build. They are test oracles, outside the
  packaged application.
- Quota and request accounting are best-effort observations, not a billing ledger;
  missing quota data must remain unknown instead of claiming replenishment.
