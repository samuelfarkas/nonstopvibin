# Working on nonstopvibin

React/Vite owns the renderer; Electron owns desktop privileges; the local server
and CLIProxyAPI core own credentials and routing. Use Bun and `bun.lock`;
`.deepsec` has its own dependencies and lockfile.

## Working style and completion

Make routine implementation decisions and finish the requested work without
stopping at a first draft. Keep the diff focused; reuse existing code and avoid
new dependencies unless the task needs them. Prefer medium effort for normal
coding when configuring Codex; increase it for problems that warrant deeper work.

Done means the requested behavior works, touched files are formatted, relevant
checks pass, and any remaining blockers or unverified behavior are reported.
For code/config changes, `bun run quality` is the completion gate; use affected
`bun test` tests for behavior changes and `bun run build` for runtime/build
boundaries. Documentation-only changes need formatting and diff review.
Fix failures caused by the change; repeat or broaden checks only for new edits,
failures, or unresolved risks. Local integration tests use synthetic providers and
the verified core; run and repair them without repeated approval or real accounts.

Run `bunx --no-install oxfmt --write <touched-files>` on supported files before
finishing; CI enforces formatting through `bun run quality`.

## Performance

Extract every last drop of performance from Electron: no wasted CPU, memory,
I/O, IPC, renderer work, or idle wakeups. Keep startup fast and the main process
responsive; avoid blocking work, redundant renders, polling where events suffice,
and resources that outlive their use. Measure relevant before/after behavior for
performance changes and pursue demonstrated bottlenecks. Preserve correctness,
security, and accessibility while optimizing.

## Security boundaries

- Profile keys never grant management access. Preserve profile identity across
  HTTP, SSE, WebSocket, retries, refreshes, accounting, and quota exhaustion.
- Keep the gateway on loopback with Host/Origin checks and endpoint allowlists.
- Validate privileged IPC senders and arguments; retain renderer sandboxing,
  context isolation, and disabled Node integration.
- SQLite secrets use an owner-only local `vault.key` in desktop and development.
  Keep the key, database, and core credentials owner-only and out of distributions.
  This does not protect against software running as the same OS user.
- Never commit or bundle real auth files, API keys, request/provider error bodies,
  local runtime directories, or raw security reports.
- Preserve pinned core version/checksum and packaging platform/architecture/hash
  checks. An adjacent downloaded checksum is integrity evidence, not independent
  assurance of the publisher.

## Task-specific references

Use [README.md](README.md) for orientation, [stack guidance](docs/stack-guidance.md)
for runtime details, and [SECURITY.md](SECURITY.md) for security/release workflows.
Dependency changes use `bun run security:deps`; scanner dependency changes also
use `bun run security:tooling`. SECURITY.md says where the release checks are defined.

Skills live in `.agents/skills`; use specific workflow triggers and load only the
relevant sub-guides. Keep descriptions short and root documents as routers.
React guidance here is client-side, not Next.js/RSC; the Vite skill's beta examples
must be checked against installed types when used. Skill provenance and update
procedures are in [security tooling](docs/security-tooling.md): keep vendored
copies, hashes, and attribution together; setup must not reinstall latest skills.

For DeepSec operations, use `.deepsec/README.md` and its installed `SKILL.md`.
Pattern matches are candidates, not confirmed exploits. Keep generated state and
reports ignored; create matchers only for evidenced gaps or validated findings.
Bound AI scan scope and inspect its execution environment; never expose repository
secrets to agents processing untrusted PRs. Treat failed audits and untested
platforms as unresolved evidence.
