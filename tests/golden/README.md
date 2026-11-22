# Golden files from the TypeScript server

These files record what the TypeScript server (`src/server` at main 4ea188a, with
the `tests/fixtures/ts-data-dir` fixture from 8abd008) does. The Rust port loads the
same JSON and must produce the same outputs. `record.ts` is the oracle. It cannot be
rebuilt once `src/server` is deleted, so it is committed with the files.

```
bun tests/golden/record.ts            # every area
bun tests/golden/record.ts gateway    # one area
```

Run it from the repository root with Bun 1.4.x. Two runs give byte-identical files.
Loopback ports 24318 (api) and 24319 (cli state stub) must be free, and nothing may
listen on 24399 (the cli's stale server.json). It never starts a CLIProxyAPI core
and never touches `~/.nonstopvibin` or port 4320. Scratch data goes to
`/tmp/nsv-golden` and is removed at the end.

## Format

Each `<area>.json` is an array with one case per line:

- `{ "name", "input", "output" }`, or
- `{ "name", "input", "error": { "message", "status" } }` for an `AppError`.
  A zod error records only `{ "status": 400 }`, the status the server answers.
  Any other `Error` records `{ "message", "status": 500, "thrown": <Error name> }`.
  That message comes from Bun or Node (for example `ENOENT: ...` or `Invalid URL`)
  and is informational. The Rust port should match the status, not the text.

Every clock the server reads is fixed at `2026-11-22T12:00:00.000Z`
(`setSystemTime`), so `checkedAt`, `fetchedAt` and relative resets use that instant.
The models.dev catalog is the fixed payload `modelsDev` in `lib.ts`, also recorded
as the `catalog payload` case in `usage.json`.

## Placeholders

Values the code generates are replaced in every string and object key:

| Placeholder                     | Replaces                                                                 |
| ------------------------------- | ------------------------------------------------------------------------ |
| `<data>`                        | the temporary data directory                                             |
| `<home>`, `<agent-home>`        | the temporary home or agent home (cli `dataDirectory`: the real `$HOME`) |
| `<root>`                        | an area's scratch root                                                   |
| `<repository>`                  | the repository root (cli `dataDirectory` of relative paths)              |
| `<socket-dir>`                  | `/tmp/nonstopvibin-<uid>-<24 hex>`, the credential socket directory      |
| `<profile>`, `<profile:<slug>>` | a profile UUID made by `Store.createProfile`                             |
| `<priced-profile>`              | the usage area's second profile UUID                                     |
| `<uuid>`                        | random UUIDs: `apiAccount` ids, `x-opencode-session`, reset request ids  |
| `<token>`                       | the Application's session token                                          |
| `<core-port>`                   | the gateway area's stub core port                                        |
| `<asset-version>`               | the random asset version that starts a static ETag                       |
| `<fetched-at>`                  | the models.dev fetch time (equal to the fixed clock)                     |
| `<now>`                         | a usage record timestamp the code takes from the wall clock              |
| `<user>`                        | the OS user name in SSH tunnel commands                                  |

The fixture's own profile UUIDs (api area) and the agents area's fixed profile
(`11111111-1111-4111-8111-111111111111`, slug `work`) are literal. JSON cannot hold
`±Infinity`, so `restart.json` writes them as the strings `"Infinity"` and
`"-Infinity"`.

## Areas

| File            | Cases | What                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
| --------------- | ----: | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `gateway.json`  |  1290 | Raw HTTP requests through the real `Gateway.proxy` (its routing, header and catalog functions are private) over a store of 4 profiles, one with a key rotated after the index was built, and a raw stub core. Input: `{ method, target, headers: [name, value][], body? }`. Output: the AppError, or `kind: "proxied"` with the request line the core received (for `forward`, `response set` and `catalog` cases also the forwarded headers and body, the status and the response headers the client got), or `kind: "catalog"`/`"http"`. Covers 10 prefixes × 49 paths × GET/POST, 21 query forms on 4 paths, absolute-form targets, 35 credential header sets × 5 targets, forwarded and stripped headers, the session header chain, response header rewriting and every curated catalog trigger. |
| `config.json`   |    30 | `ConfigWriter.write` for 15 profiles (every API provider and protocol, prefixes, trailing slashes, disabled accounts, both strategies and affinity, OpenCode Go protocol rules and headers, merged and separate compatible providers, YAML quoting): the `yaml` text, the `parsed` YAML and the `returned` object; an unknown profile. `applied()` against 13 reported configs. Slugs from `createProfile` for 15 names.                                                                                                                                                                                                                                                                                                                                                                             |
| `usage.json`    |    77 | `Usage.collect` on stubbed usage-queue pages, recording the rows the Store keeps (malformed, partial, wrapped, duplicate and breakdown edge cases). `normalizedTokens` and `estimateUsd` for 21 records; `UsagePrices.history` totals for 3 windows; `enrichModels` for 9 model and account sets; a malformed catalog.                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| `accounts.json` |   134 | `parseQuota` for every provider (31 payloads). `Accounts.sync` on a listing with credential files on disk (statuses, notes, priorities, seat identity, ChatGPT ids, skipped entries) and `credentialPath`. `Quotas.refresh` per provider: the stored quota and the exact `/api-call` requests (OpenCode Go: the fetch it sends). `CodexResets` credits and consume with their wire requests; reset schemas. `claudeIdentity`, `sameSeat`, `verifyClaudeIdentity` (stubbed fetch). `apiAccount` validation and discovery; `goModelProtocol`.                                                                                                                                                                                                                                                          |
| `agents.json`   |    81 | 32 `AgentSetup` scenarios (Claude Code with projects, repositories and worktrees, Codex, OpenCode, pi; user files absent, present, conflicting, edited or deleted; our own prior settings; port changes; a stopped profile; invalid inputs). Each step records its result or error and the files it changed (`path → { content, mode }`, `null` when removed). Also the credential socket's raw replies, the generated key helper run against it, `offeredModels` per agent, pi and OpenCode catalog entries, `agentSetupSchema`, pi's profile controls source and `unprintable`.                                                                                                                                                                                                                    |
| `restart.json`  |   137 | `restart-policy.ts`: 16 named sequences, 60 random event sequences and 60 sequences that follow the policy's own actions (seeded PRNG). Each output step is the state, the actions and `wakeAt`.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     |
| `api.json`      |   138 | The real `Application` (no core binary, `allowMissingCore`) on a copy of `tests/fixtures/ts-data-dir`, with modes and empty directories restored from its `expected.json`. The first case is what launch changed on disk (ledger reaped, agent connections rewritten for the moved directory, Claude left for the user). Then `GET /state`, `/usage` (7, 30, 3650, default and invalid days), `/key`, `/oauth-session`, `/agent-setup`, `/import-sources` (synthetic CLIProxyAPI files), and the error JSON for host, origin, token, routing, method (with `Allow`), malformed paths, profile ids, bodies, content types and an oversized body; static UI serving (cache headers, ETag and `If-None-Match`, CSP, traversal attempts) from a tiny client directory.                                   |
| `cli.json`      |   124 | `bun src/server/cli.ts` as a child process (temporary HOME and data directory, a stub `systemctl`): help, version, usage errors, `status`, `url` and `url --json` with no, stale, malformed, wrong-mode and running `server.json`, over SSH, `service install`, `unit` and `uninstall`. `renderUnit`, `updateUnit`, `tunnelCommand`, `urlText`, `validAllowHost`, `dataDirectory`, `releaseChannel`, the providers table and the desktop's external host allowlist.                                                                                                                                                                                                                                                                                                                                  |

## Not recorded, and why

- **Running profiles through the API.** No core runs, so `/agent-setup` for the
  fixture's OAuth profiles answers 409 and `/models` 503, and sign-in, refresh,
  import, account edits and resets appear only as refusals. A fake core would
  answer `/auth-files` empty and overwrite the fixture's account metadata.
  `agents.json` records `status()` for running profiles directly.
- **Successful mutating routes** (`POST /profiles`, `PATCH`, API-account creation,
  sign-in). They create random UUIDs and keys. The Store-level behaviour they wrap
  is in `config.json` and `accounts.json`.
- **WebSocket upgrades** (`Gateway.upgrade`) and streaming or SSE timing. Only the
  HTTP proxy verdicts and headers are recorded.
- **The scheduler** (`Usage` timers, `CorePool` start and stop, core updates and
  downloads). Only its pure policy is recorded, in `restart.json`.
- **`safeExternal`** lives in `src/desktop/main.ts`, which imports Electron. The
  allowlist comes from the real `providers.ts`, each URL's parse from the WHATWG
  parser, and the `allowed` field applies the rule as copied from that function.
- **`service install` success.** From a source checkout it refuses ("Install the
  service from the packaged release"). The unit it would write is covered by
  `renderUnit` and `updateUnit`.
- **`install.sh`** is a release script outside `src/server`.
- Bun- or Node-specific error texts (the `--bogus` parse error, `ENOENT`) are
  recorded as they are, but they are not behaviour to match.

## Gitleaks

Every secret is low-entropy and synthetic (`nv_` + 43 × one letter,
`synthetic key ...`, `core-key-N`). `gitleaks dir tests/golden` finds nothing with
`.gitleaks-source.toml` or `.gitleaks.toml`, without allowlist entries.

## Repository integration

`.oxfmtrc.json` ignores `tests/golden/*.json`, which keep one case per line and
would otherwise be expanded by oxfmt. `record.ts`, `lib.ts` and `areas/*.ts` pass
oxfmt, oxlint and `tsc`. `bun-clock.d.ts` declares Bun's `setSystemTime`, as
`tests/bun-test.d.ts` does for the hooks.
