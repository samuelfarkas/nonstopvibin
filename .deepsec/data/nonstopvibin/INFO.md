# nonstopvibin

## What this codebase does

An Electron desktop app for macOS/Linux that separates AI subscription accounts
into profiles and exposes profile-specific OpenAI/Anthropic-compatible endpoints
to coding agents. React 19/Vite renders a local UI; a Node-compatible HTTP server uses
node:sqlite, Zod, YAML, and an unmodified pinned CLIProxyAPI Go binary.
Bun drives development/tests/build tooling; Electron provides the desktop runtime.
There is one upstream core process, credential directory, and account pool per
profile. Cross-profile credential fallback is forbidden, even under exhaustion.

## Auth shape

- `Application.validHost` and the `/api/` branch in `Application.handle` protect
  management routes;
  management uses a per-launch random token distinct from profile API keys.
- `routeRequest` authenticates profile keys with `sameSecret`, binds any `/p/slug`
  to that profile, and allowlists forwarded paths. WebSocket upgrades must retain
  the same auth boundary as HTTP and SSE.
- `assertSender` checks the IPC sender frame, app origin, and known window IDs;
  the preload exposes a narrow bridge rather than arbitrary IPC or filesystem APIs.
- `safeExternal` restricts browser launches to HTTPS provider hosts. OAuth flow
  state and profile ownership are enforced by `CorePool`.
- Desktop and development use `fileKeyCodec`: AES-GCM with an owner-only
  `vault.key` beside SQLite in `~/.nonstopvibin` (or the explicit data override).
  No system keychain is required; access to both files permits decryption.

## Threat model

Highest impact is leaking subscription credentials or routing a work request
through another profile. Treat remote websites, renderer input, imported auth
files, provider responses, and proxy client requests as untrusted. Management
access must not follow from possession of a profile key. A malicious upstream
release or packaging mistake can execute native code on the user's machine.

The gateway is intentionally loopback-only; this is not a multi-user hosted
service. Processes running as the same OS user can read owner-only files; profile
separation does not claim an OS sandbox between accounts. The upstream core needs
plaintext generated configuration and OAuth files in 0700/0600 storage.

## Project-specific patterns to flag

- Forwarding a new core route without `routeRequest` restrictions, or forwarding
  management paths / client credentials through `forwardedHeaders` logic in gateway.ts.
- A `CorePool` fallback, retry, refresh, pause/resume, or usage attribution path
  that uses another profile's auth directory, management key, or account pool.
- New `ipcMain.handle` handlers missing `assertSender`, broad context-bridge
  methods, insecure BrowserWindow preferences, or arbitrary `shell.openExternal`.
- `ImportCatalog` accepting renderer-provided paths, following links, or letting
  token-bearing content reach error messages, logs, activity records, or exports.
- Bypasses of `install-core.mjs` checksums or `validate-core.cjs` target/hash checks;
  credentials, local runtimes, scanner reports, or keys entering packaged resources.
- Changes in `core-processes.ts` that signal a process without matching its recorded
  PID, start time, and full command line, or that reap cores outside this data folder.

## Known false-positive context (verify rather than blanket-suppress)

- Tests use synthetic company/personal keys and local HTTP fixture servers;
  these exercise the real core without live provider credentials.
- `providerURL` intentionally permits HTTP for loopback custom providers only.
  Authenticated users may configure HTTPS providers; inspect reachability before
  calling every user-selected URL a remotely exploitable server-side request.
- `fileKeyCodec` deliberately stores the encryption key in an owner-only file
  in both desktop and development; this does not protect against the same OS user.
- `scripts/preview.ts` builds explicitly simulated local accounts for visual QA.
  It must remain a development script, outside the packaged application.
- Quota and request accounting are best-effort observations, not a billing ledger;
  missing quota data must remain unknown instead of claiming replenishment.
