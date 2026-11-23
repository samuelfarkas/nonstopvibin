# TS data-directory fixture (main 4ea188a)

Goal: a data directory written by the TypeScript server, for the Rust port's
migration test to open and check. Everything here was produced by
`bun tests/fixtures/ts-data-dir/generate.ts` (run from the repository root of a
4ea188a checkout, Bun 1.4.4). The generator imports `src/server`, which this branch
deleted: it is kept as the fixture's source, excluded from `tsc` and oxlint, and runs
only from a checkout of commit 8abd008, which added it. Rerunning it rewrites `data/`, `agent-home/` and
`expected.json`; only UUIDs, AES-GCM nonces, the reset request id and the ledger
owner pid change between runs.

## Layout

```
ts-data-dir/
  generate.ts      the generator (runs from 8abd008; not type-checked here)
  expected.json    what a reader must find (see below)
  data/            the data directory (NONSTOPVIBIN_DATA_DIR)
  agent-home/      the user's home with agent connections installed
```

The generator builds under `/tmp/nsv-ts-fixture/{data,agent-home}` and copies
with `cp -a`. Absolute paths inside files (config.yaml `auth-dir`, helper paths in
agent configs, ledger command lines) therefore say `/tmp/nsv-ts-fixture/...`.
`expected.json.originalPaths` records them. A migration test that copies the
fixture elsewhere sees a moved data directory, which is a real user case.

## Artefacts and the TS code that wrote them

| Path                                             | Written by                                                                                             | Mode                                                |
| ------------------------------------------------ | ------------------------------------------------------------------------------------------------------ | --------------------------------------------------- |
| `data/`                                          | `ownerOnlyDirectory` (data-directory.ts)                                                               | 0700                                                |
| `data/vault.key`                                 | generator, the way `fileKeyCodec` writes it (`wx`, 0600), bytes 0x01..0x20                             | 0600                                                |
| `data/nonstopvibin.sqlite`                       | `Store` (store.ts); closed, so no `-wal`/`-shm`                                                        | 0600                                                |
| `data/profiles/<id>/` and `auth/`                | `ConfigWriter.writeFile` (config.ts)                                                                   | 0700                                                |
| `data/profiles/<id>/config.yaml`                 | `ConfigWriter.writeFile` → `YAML.stringify(coreConfiguration(...))`                                    | 0600                                                |
| `data/profiles/<id>/auth/<provider>-<uuid>.json` | generator, as `Credentials.importCredential`'s reconnect path writes (`JSON.stringify(payload)`, 0600) | 0600                                                |
| `data/core-processes/<pid>.json`                 | `recordCore` (core-processes.ts) with an injected `identify`                                           | dir 0700, file 0600                                 |
| `data/agent-connections/<slug>/...`              | `AgentSetup.install` (agent-setup.ts)                                                                  | dir 0700; manifests/settings 0600; key helpers 0700 |
| `agent-home/...`                                 | `AgentSetup.install` plus two user files the generator wrote first                                     | see expected.json `files`                           |

`expected.json.files` lists every directory and file with its exact mode. Git
keeps none of them (only the executable bit), so the Rust test must chmod from
that list, or assert modes only where it creates files itself. Git also drops
empty directories: `profiles/<personal>/auth/` and `profiles/<★ ★ ★>/auth/` are
empty and will not survive a commit.

Profiles have no OAuth sign-in, so no core ran. `Accounts.sync` read a stubbed
`/auth-files` listing and `Usage.collect` read stubbed `/usage-queue` pages,
so account metadata and usage rows are exactly what those functions store. Quotas
come from `parseQuota(provider, payload, now)` with a fixed `now`, plus the literal
`unavailable` and `error` shapes `Quotas.refresh` stores.

## Contents

- Profiles (creation order = rowid order): `Équipe Café: R&D (été)` → slug
  `equipe-cafe-r-d-ete` (round-robin, affinity off, enabled); `Personal` →
  `personal` (fill-first, affinity on, disabled); `Personal` → `personal-2`
  (round-robin, affinity on, enabled); `★ ★ ★` → `profile` (empty stem falls back).
  `createdAt` is fixed per profile.
- Secrets: `<profileId>:client` = `nv_` + 43 × a/b/c/d; `<profileId>:core` =
  `core-` + 32 × letter; `<profileId>:management` = `management-` + 32 × letter;
  `<accountId>:api` = `synthetic api key one`..`six`.
- API accounts: team has a custom account with one model per protocol (it lands in
  all three config sections), an Anthropic preset, and a disabled OpenAI preset
  (absent from config.yaml); personal has Gemini; personal-2 has two OpenCode Go
  accounts whose OpenAI-protocol entries merge into one `openai-compatibility`
  provider with two `api-key-entries`. Base URLs are stored without a trailing slash.
- OAuth: team has Claude with org (Ready, priority 2, note), Claude personal
  without org (disabled → Paused), Codex (status error + invalid grant →
  Reconnect needed); personal-2 has Claude with upper-case UUIDs on disk (metadata
  lowercases them) and `unavailable` → Cooling down. The listing also had an
  `account_type: api_key` entry, which sync drops.
- Usage: 8 rows for team, 3 for personal-2, 1 legacy row for personal, 0 for ★.
  `pricingTokens` is an object, `null` (no breakdown, or parts that do not add up),
  or absent (legacy row). `upstreamModel` is absent when the queue entry had only
  an alias. One id is `<profileId>:<sha256 hex of the entry JSON>` (no request_id).
  Two rows share a timestamp. One timestamp has nanoseconds
  (`2026-11-20T10:00:00.123456789Z`): timestamps are stored verbatim.
- Settings: `usage-retention-days` = `"365"`; `accounts:<profileId>` for the two
  profiles with OAuth; `codex-reset:["<chatgpt account id>","credit-synthetic-1"]`
  = a random UUID.
- Ledger: pids 4242 (team) and 4243 (personal-2), owner = the generator's pid. The
  owner is dead, so a reaper must treat both as stale.
- Agents (gateway port 4318): team has Claude Code in two projects
  (`projects/webapp`, which had user permissions first, and `projects/api server`
  with otherProviders), Codex, OpenCode and pi; personal-2 has Codex with
  otherProviders. Claude connects per project only; the others are global.

Usage timestamps are 2026-11-15..22. Retention 365 days prunes them after
2027-11-15 if a test prunes against the wall clock.

## Formats a Rust reader must match

- SQLite schema: in `expected.json.tables` (verbatim `CREATE` SQL). WAL mode,
  `foreign_keys=ON`. Every `body`/`value` is compact `JSON.stringify` (no spaces,
  key order as below). Optional fields are omitted, never `null`, except
  `pricingTokens: null` and `QuotaWindow.remainingPercent`/`resetsAt` `null`.
  - Profile: `id, slug, name, strategy, sessionAffinity, enabled, createdAt`.
  - ApiAccount: `id, name, provider, baseUrl, prefix, models[{id, protocol}], disabled`.
  - Account metadata (`settings.accounts:<id>`, a JSON array): `id, name,
    accountUuid?, organizationUuid?, organizationName?, provider, kind, email?,
    disabled, status, priority, authIndex`. The OAuth account id is the credential
    file name.
  - Quota: `status, windows[{label, remainingPercent, resetsAt, scoped?}],
    checkedAt, error?, plan?, bankedResets?`.
  - UsageRecord (from the queue): `id, profileId, timestamp, provider, model,
    upstreamModel?, pricingTokens, account, inputTokens, outputTokens,
    cachedTokens, reasoningTokens, cacheWriteTokens, totalTokens, latencyMs,
    failed, statusCode, stream`. The legacy row has no `upstreamModel` or
    `pricingTokens`. PricingTokens: `input, cached, cacheWrite, output, reasoning`.
    The `usage.timestamp` column duplicates the body's.
  - Timestamps the app makes are `Date.toISOString()`: `YYYY-MM-DDTHH:MM:SS.mmmZ`.
- Secrets: `encrypted` = standard base64 (with `+/` and `=` padding) of
  `nonce(12) || tag(16) || ciphertext`, AES-256-GCM, key = the 32 raw bytes of
  `vault.key`, no AAD, plaintext UTF-8.
- Client key: `nv_` + 43 chars of `[a-zA-Z0-9_-]` (base64url of 32 bytes in real
  installs). The key helper checks length 46.
- config.yaml: `yaml` package defaults: 2-space indent, block style, `- ` items
  indented under their key, empty map as `{}`. A headers object shared by one
  account's entries across protocols is written once as an anchor (`&a1 {}`) and
  then as aliases (`*a1`), so the reader must resolve YAML aliases. Anthropic
  entries drop a trailing `/v1` from `base-url`. The `openai-compatibility` name
  is `<provider>-<sha256 hex of JSON.stringify([baseUrl, prefix, models])>`.
- Credential files: one line of compact JSON, no trailing newline. Claude:
  `type, email, access_token, refresh_token, id_token?, expired, last_refresh,
  account_uuid, organization_uuid?, organization_name?, disabled, note?,
  priority?`. Codex adds `id_token` and `account_id`. These are synthetic: the
  real core also writes these files itself, possibly in another layout.
- Ledger entry: compact JSON `{"pid", "identity", "owner": {"pid", "identity"}}`.
  An identity is `ps -o lstart= -o command=`: 24 characters of C-locale start time,
  a space, then the command line ending in `--config <data>/profiles/<uuid>/config.yaml`.
- Agent manifests (`<slug>/<agent>.json`, Claude `<slug>/claude-<first 24 hex of
  sha256(project path)>.json`): `JSON.stringify(manifest, null, 2) + "\n"`, fields
  `version, input{agent, projectDirectory?, otherProviders?}, models, port,
  content, retained?, helperContent?`. `content` is the agent config text written.
  Claude's `.settings.json` sits beside its manifest.
- Key helper `<slug>/<agent>-key-<port>` (0700, `#!/bin/sh`): curls
  `http://localhost/profiles/<profileId>` over the Unix socket
  `/tmp/nonstopvibin-<uid>-<first 24 hex of sha256(resolved data dir)>/agents.sock`,
  checks the key shape and port, and prints the key. The uid in the fixture is the
  generating user's (979). The helper never holds a key.
- Agent configs: Claude `<project>/.claude/settings.local.json` merges
  `apiKeyHelper` (shell-quoted helper path) and `env` into the user's JSON; Codex
  writes `~/.codex/nonstopvibin-<slug>.config.toml` and leaves the user's
  `config.toml` untouched; OpenCode writes
  `~/.config/opencode/plugins/nonstopvibin-<profileId>.js`; pi writes
  `~/.pi/agent/extensions/nonstopvibin-<slug>.js`. Each result's `files`, `command`
  and `provider` are in `expected.json.agentConnections`.

## expected.json keys

`originalPaths, vaultKeyHex, gatewayPort, corePorts, profiles, secrets
(id → plaintext), apiAccounts, accountMetadata, accountsList (Accounts.list,
with quota), quotas, usage (per profile: count and every record with its rowid
sequence), settings (raw rows), retentionDays, resetRequest, tables,
credentialFiles, coreLedger, agentConnections, credentialSocket, files (path,
type, mode), gitIgnored`.

## Repository integration the lead must add

- `.gitignore` ignores two fixture files (`git check-ignore`):
  `tests/fixtures/ts-data-dir/data/nonstopvibin.sqlite` and
  `tests/fixtures/ts-data-dir/data/vault.key`. Negate both, scoped to this
  directory. Nothing else is ignored, and there is no `auth.json`.
- `bun run quality` would fail on the fixture: oxfmt reformats 14 JSON files under
  `data/` and oxlint flags the generated pi extension under `agent-home/`. Add
  `tests/fixtures/ts-data-dir/data/` and `tests/fixtures/ts-data-dir/agent-home/`
  to `ignorePatterns` in `.oxfmtrc.json` and `.oxlintrc.json`. `generate.ts` and
  `expected.json` pass both (the generator runs oxfmt on expected.json).

## Gitleaks (v8.30.1, vendored binary)

- `gitleaks dir tests/fixtures/ts-data-dir` with `.gitleaks.toml` and with
  `.gitleaks-source.toml`: no leaks.
- `security:history` equivalent: committed the fixture into a throwaway repo and
  ran `gitleaks git . --config .gitleaks.toml --log-opts=--all`: no leaks.
  `gitleaks dir . --config .gitleaks-source.toml` there: no leaks.
- Positive control: a planted random secret in a copy of config.yaml was reported.
- Gitleaks skips `nonstopvibin.sqlite` as binary in both modes. A text dump of its
  rows does trip `generic-api-key` six times, on the AES-GCM ciphertexts of the
  `<uuid>:api` secrets. Ciphertext is random by construction, so a low-entropy
  form does not exist. This matters only if someone commits a SQL dump.
- No allowlist entries were added.
