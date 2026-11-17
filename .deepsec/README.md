# DeepSec for nonstopvibin

This isolated workspace pins Vercel DeepSec 2.3.10. The source root is `..`; priority
areas and exclusions are in `data/nonstopvibin/config.json`, and the short manually
reviewed threat context is `data/nonstopvibin/INFO.md`.

## Local setup and inspection

From the application root:

```sh
bun install --cwd .deepsec --frozen-lockfile
bun run security:tooling
bun run security:scan
bun run --cwd .deepsec status
```

Pattern scan matches are candidates, not vulnerabilities. `report` says there are
no analyzed files until processing finishes. See `docs/security-tooling.md` in the
parent project for the tool list, limitations, and update policy.

## AI review when ready

Read the installed `node_modules/deepsec/SKILL.md` and relevant docs in
`node_modules/deepsec/dist/docs/`; these match the pinned release.

On this release, the upstream `init --plan` command probes Vercel CLI authentication even before selecting
a local model route, and an unauthenticated probe can start a device-login prompt.
Our probes were stopped without authenticating (including a timed CI-mode retry).
No plan shortcut is provided; the working local scan needs none of this. Do not treat plan completion as a prerequisite for reviewing local candidates.

Select model access during setup. A local logged-in Codex/Claude CLI can use
`--model-auth local`; Gateway/direct API access uses `.env.local` or environment
variables. Cloud Sandbox needs its own supported credential path. Keep model access
separate from the app's live subscription profiles. Do not paste keys into chat.

A limited manual investigation after choosing/authenticating a provider:

```sh
bun run --cwd .deepsec process --limit 5
bun run --cwd .deepsec revalidate
bun run security:report
bun run --cwd .deepsec export
```

`process` is AI work and may use paid model calls or local subscription quota.
Local agent execution can access files and a shell; source exclusions are scanner
filters, not a sandbox boundary. For untrusted code, use an isolated environment.
This setup does not create a Vercel project, upload the source, or run cloud workers.

## What belongs in Git

Commit config, generated-matchers.ts after review, package.json, bun.lock, this
runbook, AGENTS.md, and curated INFO.md / SETUP.md / config.json. The workspace
.gitignore denies generated project data by default and keeps findings, exports,
reports, traces, environment files, and .vercel state out of source control.
New matcher patterns should follow actual findings or demonstrated coverage gaps.

Read the upstream docs: <https://deepsec.sh/docs/getting-started> and
<https://deepsec.sh/docs/configuration>.
