# Working on nonstopvibin

React/Vite owns the interface. The Rust daemon `nonstopvibin` owns credentials,
routing, the gateway, the management API and the tray. The window helper
`nonstopvibin-window` is the only code that links WebKit. One CLIProxyAPI core per
running profile does the upstream work. Rust crates live in `crates/` with
`Cargo.lock`; the interface and build scripts use Bun and `bun.lock`; `.deepsec`
has its own dependencies and lockfile.

## Working style and completion

Make routine implementation decisions and finish the requested work without
stopping at a first draft. Keep the diff focused; reuse existing code and avoid
new dependencies unless the task needs them. Prefer medium effort for normal
coding when configuring Codex; increase it for problems that warrant deeper work.

Done means the requested behavior works, touched files are formatted, relevant
checks pass, and any remaining blockers or unverified behavior are reported.
For code/config changes, `bun run quality` is the completion gate; it includes
`cargo fmt --check`, clippy with `-D warnings` and the wire-type drift check. Rust
changes also need `cargo test -p <crate> --locked` for each affected crate
(`bun run test` runs every test). Use affected `bun test`
tests for TypeScript behavior changes and `bun run build` for runtime/build
boundaries. Documentation-only changes need formatting and diff review.
Fix failures caused by the change; repeat or broaden checks only for new edits,
failures, or unresolved risks. Local integration tests use synthetic providers and
the verified core; run and repair them without repeated approval or real accounts.

Run `bunx --no-install oxfmt --write <touched-files>` on supported files and
`cargo fmt` on Rust before finishing; CI enforces both.

Always squash-merge pull requests, with a title that follows the
[commit conventions](docs/commits.md). Never use a merge commit or a rebase merge.

## Performance

Extract every last drop of performance from the daemon and the window helper: no
wasted CPU, memory, I/O, IPC, rendering, or idle wakeups. The daemon idles at 0
wakeups per second; keep it there. Keep startup fast and the runtime thread
responsive; avoid blocking work, redundant renders, polling where events suffice,
and resources that outlive their use. The window helper must exit when its last
window closes. Measure relevant before/after behavior for
performance changes and pursue demonstrated bottlenecks. Preserve correctness,
security, and accessibility while optimizing.

## Security boundaries

- Profile keys never grant management access. Preserve profile identity across
  HTTP, SSE, WebSocket, retries, refreshes, accounting, and quota exhaustion.
- Keep the gateway on loopback with Host/Origin checks and endpoint allowlists.
- Validate bridge calls in the window helper (main frame, daemon origin, known
  method) and validate their arguments again in the daemon. Keep navigation to
  other origins, new windows and downloads denied.
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
[The Rust port decision record](docs/decisions/rust-port.md) explains the process
model and crate layout. React guidance here is client-side, not Next.js/RSC; the Vite skill's beta examples
must be checked against installed types when used. Skill provenance and update
procedures are in [security tooling](docs/security-tooling.md): keep vendored
copies, hashes, and attribution together; setup must not reinstall latest skills.

For DeepSec operations, use `.deepsec/README.md` and its installed `SKILL.md`.
Pattern matches are candidates, not confirmed exploits. Keep generated state and
reports ignored; create matchers only for evidenced gaps or validated findings.
Bound AI scan scope and inspect its execution environment; never expose repository
secrets to agents processing untrusted PRs. Treat failed audits and untested
platforms as unresolved evidence.
