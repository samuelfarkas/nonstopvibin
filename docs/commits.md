# Commit conventions

Use [Conventional Commits 1.0.0](https://www.conventionalcommits.org/en/v1.0.0/)
for new commits and squash-merge pull request titles:

```text
type(scope): imperative summary

Optional body explaining the problem, resulting behavior, and relevant limits.

Optional trailers, such as Fixes: #123
```

This fits the repository's React interface, Rust daemon and desktop shell, and
release tooling: the type explains intent and an optional scope identifies the
area. It keeps history searchable without adding a commit hook or dependency.

## Types and scopes

| Type       | Use for                                                   |
| ---------- | --------------------------------------------------------- |
| `feat`     | New user-visible behavior                                 |
| `fix`      | Correcting a defect, including a security defect          |
| `perf`     | Measured performance improvements                         |
| `refactor` | Restructuring without changing behavior                   |
| `docs`     | Documentation only                                        |
| `test`     | Tests only                                                |
| `build`    | Dependencies, bundling, or packaging                      |
| `ci`       | GitHub Actions and automation                             |
| `chore`    | Maintenance that does not fit another type                |
| `style`    | Formatting only, not visual UI changes                    |
| `revert`   | Reverting a prior change; identify its commit in the body |

Prefer existing areas such as `client`, `desktop`, `server`, `gateway`, `agents`,
`quota`, `activity`, or `release`. Omit the scope for a change across areas; do not
invent a scope for every file. Choose the type by the main outcome: tests and docs
that accompany a feature belong in its `feat` commit.

Use lowercase types/scopes, an imperative summary, and no final period. Aim for
72 characters or fewer in the subject. Explain why in the body when the subject
cannot carry the context. Record checks and limitations in the PR description;
include them in the body when committing directly without a PR.

```text
feat(quota): support confirmed Codex reset redemption
fix(server): refuse to replace a missing vault key
perf(desktop): suspend tray work while hidden
docs: document commit and release conventions
build(deps): update hyper
```

Use `!` before the colon for an incompatible change and a `BREAKING CHANGE:` footer
that explains the impact and migration. Apply this to supported configuration,
storage, endpoints, and agent integration contracts, including during `0.x`.
Do not mark an internal refactor as breaking.

## Prepare a commit

1. Inspect `git status --short`, `git diff`, and `git diff --cached`; include
   untracked files in the review. Stage explicit paths or use `git add -p` for
   unrelated changes in one file.
2. Keep one coherent outcome per commit, with its tests, assets, and necessary
   documentation. Each retained commit should build. Keep coupled changes together
   when splitting them would leave broken intermediate states.
3. Format touched supported files with
   `bunx --no-install oxfmt --write <files>`. Run `bun run quality`, affected
   `bun test` tests, and `bun run build` for runtime/build boundaries. Dependency
   changes also require `bun run security:deps`; scanner dependencies additionally
   require `bun run security:tooling`. Documentation-only commits need formatting
   and diff review. See [Contributing](../CONTRIBUTING.md) and
   [Security](../SECURITY.md) for the remaining checks.
4. Review `git diff --cached --check` and `git diff --cached` before committing.
   Exclude credentials, runtime data, raw security reports, and generated build
   output. Commit intended deletions and source assets along with their consumers.
5. Check `git status --short` and `git log -5 --oneline` afterward. Do not push,
   create a release tag, or publish as an incidental part of local cleanup.

## Clean up history

Always squash-merge pull requests, with a conforming PR title: `main` gets one
commit per pull request. Never use a merge commit or a rebase merge, however
clean the branch's series is.

On a branch, use interactive rebase to reword or combine private WIP/fixup
commits before sharing; logical commits there help review, and the squash
removes them from `main`.

Before rewriting, check branches, remotes, and tags, and save a recovery reference
or Git bundle outside the checkout. Rewrite only confirmed private history. A
missing remote alone does not prove that commits were never shared: check the
project's publication context too. Leave published commits and release tags
unchanged; use follow-up commits instead. Git's
[rebasing guidance](https://git-scm.com/book/en/v2/Git-Branching-Rebasing)
explains why rewriting shared ancestry disrupts collaborators.

## Releases

Commit prefixes do not currently generate versions or changelogs. Keep
`package.json` and `CHANGELOG.md` explicit, following [Releases](releases.md).
`feat` and `fix` describe feature and patch intent; a breaking footer records
compatibility impact. The release workflow validates the chosen version and tag.
GitHub's generated release-note categories come from PR labels in
[the release configuration](../.github/release.yml), not commit prefixes. Apply
those labels as well when using PRs.
