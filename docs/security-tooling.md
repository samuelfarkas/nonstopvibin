# Quality and security tooling

## Checks

- `bun run quality` runs strict TypeScript checks, Oxlint, Oxfmt, and the
  registry drift check.
- CI runs `cargo fmt --check`, `cargo clippy` with warnings as errors, and
  `cargo machete` on the Rust workspace. Each crate forbids `unsafe` code except
  the macOS shell's scoped `SMAppService` calls.
- `bun run security:deps` runs Bun's dependency audit at high severity.
- The security workflow runs `cargo deny check` (RustSec advisories, licenses,
  bans and sources, configured in `deny.toml`) and `cargo audit`.
- Gitleaks 8.30.1 scans the working tree and Git history. The installer verifies
  pinned official archive hashes before placing it under `.vendor/security`.
- DeepSec 2.3.10 has a separate locked Bun workspace under `.deepsec`. Its local
  pattern scan produces candidates, not confirmed vulnerabilities. Read
  [the workspace runbook](../.deepsec/README.md) before using AI processing.
- Dependabot proposes dependency and GitHub Actions updates for review. Bun version
  updates for both workspaces wait seven days after publication, matching
  `minimumReleaseAge` in `bunfig.toml`. The cooldown does not apply to Dependabot
  security updates, so Bun can reject a security update younger than seven days.

See [SECURITY.md](../SECURITY.md) for the required checks and release policy.

## Vendored skills

The repository keeps six reviewed skills in `.agents/skills`:

- `vercel-react-best-practices`, `vercel-composition-patterns`, and
  `web-design-guidelines` come from
  [vercel-labs/agent-skills](https://github.com/vercel-labs/agent-skills) at commit
  `063bee94c3f4df8453406c830b0a7df0f2860278`.
- `sharp-edges` and `property-based-testing` come from
  [trailofbits/skills](https://github.com/trailofbits/skills) at commit
  `d3323cefbcf645678b8dc481de204b02ad3d02dc`.
- `vite` comes from [antfu/skills](https://github.com/antfu/skills) at commit
  `a74f281a27dadc02397bc1a174b0f2c97531b6ae`.

The lockfile records each copy's source path and content hash. License texts and
attribution are in [docs/skill-licenses](skill-licenses/README.md).

## Update policy

A checkout already contains the reviewed skill copies. Setup must not fetch newer
instructions. Update a skill only after reviewing its upstream diff, references,
assets, executable files, compatibility, and license. Replace the committed copy
and update `skills-lock.json` together. Preserve the exact source revision in this
document and the attribution files.

The `web-design-guidelines` skill fetches a remote Vercel guideline when used.
Inspect that content before applying it. For framework and runtime details, use the
official sources linked from [stack guidance](stack-guidance.md).
