# Releases and app updates

GitHub Actions builds releases; GitHub Releases hosts installers and update metadata.
No separate update server is required. Updates are enabled by
`updatesEnabled: true` in `package.json` from 0.2.0, the first tagged release. The
release repository is
[samuelfarkas/nonstopvibin](https://github.com/samuelfarkas/nonstopvibin).
Development builds and Linux deb installations do not auto-update.

## Enable releases and updates

Done in the repository:

- `updatesEnabled` is `true` in `package.json`, and `repository` in `Cargo.toml`
  names this repository, which `package.json` repeats (`scripts/verify-release.mjs`
  checks that they agree). `crates/shell/build.rs` compiles both into the app;
  changing them on GitHub cannot enable an already-installed build.
- The "Protect release tags" ruleset is active for `v*` tags. It blocks deleting
  and force-moving a release tag; it does not restrict who can create one.

Remaining owner steps:

1. Protect `main`. It has no branch protection, although this document and
   [SECURITY.md](../SECURITY.md) assume reviewed changes and required checks:
   require pull requests with review, and require the `build / desktop (macos-26)`,
   `build / desktop (ubuntu-24.04)`, `build / server`, `build / server (arm64)`,
   `build / checks (macos-26)`, `build / checks (ubuntu-24.04)`, `dependencies` and
   `secrets` checks from the shared [checks workflow](../.github/workflows/checks.yml).
2. Run an actual upgrade between two released versions on a Mac before advertising
   updates. The first chance is 0.2.0 to the next release.
3. Before the first Rust release, create the minisign key pair, commit its public
   key as `build/update-signing.pub`, and create the protected `release` environment,
   limited to `v*` tags, with the secret key
   ([release signing](release-signing.md#sign-updates-for-the-rust-updater)). The
   publishing job uses it; GitHub creates it without that limit on the first release
   otherwise. Builds made before the public key is committed cannot check the
   signature.

The workflow checks that the configured update repository equals the repository
publishing the release. It uses Actions' built-in `GITHUB_TOKEN` only in the publish
job; clients download public assets without credentials.

## Publish a version

Bump `package.json` to an unused version and update `CHANGELOG.md`, then commit and
merge the reviewed changes. From that revision, with `X.Y.Z` the new version:

```sh
git tag vX.Y.Z
git push origin vX.Y.Z
```

The tag must equal `v` plus the package version. Normal versions, including `0.x`,
are regular releases. `0.2.0-beta.1` / `v0.2.0-beta.1` creates a prerelease; only
installed beta builds opt into prerelease discovery. Other prerelease names are
not supported. The workflow applies `scripts/release-channel.ts`, and the app's
`AppVersion` (`crates/domain/src/version.rs`) follows the same rule.

Releases publish `latest-mac.yml` and `latest-linux.yml` even on GitHub
prereleases, so installed Electron builds can still update through
electron-updater. The Rust updater selects the release from `releases.atom` first,
then reads these files.

The [Release workflow](../.github/workflows/release.yml) runs the same checks as every
pull request ([checks.yml](../.github/workflows/checks.yml) and
[security.yml](../.github/workflows/security.yml)). macOS builds are ad-hoc signed
and not notarized; [macOS release signing](release-signing.md) explains how they
update.
After all checks pass, the final job verifies the assets `scripts/verify-release.mjs`
expects from `scripts/distribution.ts`, metadata versions, payload sizes and SHA-512 hashes, writes
SHA256SUMS and signs it as `SHA256SUMS.minisig` (`scripts/sign-release.ts`), creates
provenance attestations, uploads a draft release, then publishes it.
Only the final job can publish. A failed upload leaves a draft, not a partial public
update. Remove the failed draft before retrying the publish job; never overwrite
an already-published version. Fix a bad release with a higher version.

Preserve the generated DMG, macOS ZIP, AppImage, deb and channel YAML files. A DMG
alone cannot supply a macOS update. Releases have no blockmaps: an Electron build
that finds none downloads the whole payload, and the Rust updater always does.
The GitHub-generated source ZIP is not an update payload.

## First release (0.2.0)

`package.json` is at 0.2.0 with updates enabled, and `CHANGELOG.md` has a `[0.2.0]`
section. Work through these steps in order.

1. **Preconditions.** `main` is protected as described above, and the 0.2.0 changes
   are merged through a reviewed pull request with green checks. No `v0.2.0` tag or release exists yet
   (`git ls-remote --tags origin`, `gh release list`). The tag ruleset blocks
   deleting or moving a pushed tag, so push it only when everything else is ready.
2. **Create the update-signing key** and the `release` environment as described in
   [Enable releases and updates](#enable-releases-and-updates), if that is not done.
3. **Run the checks** on a clean checkout of the merged revision, in the order
   [checks.yml](../.github/workflows/checks.yml) runs them:

   ```sh
   bun run setup
   bun run quality
   bun run build:client
   bun run test        # On Linux: sh tests/headless.sh bun run test
   bun run test:shell  # Linux only
   bun run test:ui
   # On the target system: dist:mac on macOS, dist:linux on Linux.
   bun run dist:mac
   # On a GitHub Actions macOS runner only: update older builds to this one.
   bun run test:update:mac
   # On Linux, for the host architecture (CI builds x64 and arm64):
   bun run dist:server && bun run test:server
   bun run security:deps
   bun run security:install && bun run security:secrets && bun run security:history
   GITHUB_REPOSITORY=samuelfarkas/nonstopvibin bun scripts/verify-release.mjs v0.2.0
   ```

   The `[0.2.0]` date in `CHANGELOG.md` must equal the day you push the tag
   (`date +%F`). If it does not, correct it through a reviewed pull request first.

4. **Tag** the merged revision:

   ```sh
   git switch main
   git pull --ff-only
   git tag v0.2.0
   git push origin v0.2.0
   ```

5. **The Release workflow** then verifies the tag, version, `updatesEnabled` and
   update feed; runs the pull-request checks (security scans, the Rust and
   TypeScript checks and the desktop packages on both runners, and the server
   archives with their smoke test); updates older macOS builds to the new one; and
   finally verifies the assets and update metadata, writes and signs `SHA256SUMS`,
   attests provenance, and
   publishes the release with generated notes. If a job fails for a reason that needs
   no source change, such as a runner outage, re-run the failed jobs. If the source
   must change, release 0.2.1 instead; the tag cannot be moved.
6. **Confirm the result.** `gh release view v0.2.0` shows a published, non-prerelease
   release with every asset `scripts/verify-release.mjs` requires (the installers,
   both update feeds and both server archives) and `SHA256SUMS` (with
   `SHA256SUMS.minisig` once the update-signing key exists). In a folder
   with the downloaded assets, `shasum -a 256 -c SHA256SUMS` passes and
   `gh attestation verify <file> -R samuelfarkas/nonstopvibin` passes for an
   installer. On a clean Mac, drag the app from the DMG to Applications and open it
   with **Open Anyway** in **System Settings → Privacy & Security**.
   `codesign -d -r- /Applications/NonstopVibin.app` prints
   `designated => identifier "app.nonstopvibin.desktop"`. The app launches, quits,
   and reopens with its data. Settings shows 0.2.0, and **Check for updates…**
   reports that no updates are available.

## Updating the CLIProxyAPI core

CLIProxyAPI ships inside every installer as a separate native executable in
`core/` beside the daemon (`scripts/package-layout.ts` has each layout), with its
`manifest.json`. The daemon launches it as a child process for each
running profile;
[storage and profile boundaries](architecture.md) describes how a core is tied to
the app's lifetime.
`crates/core/core-release.json` is the reviewed pin: the core version, an optional
`minApp`, and the SHA-256 of each target's upstream archive and extracted
executable. Each build bundles the core of the pin it was built from. Installed
builds also read the pin on `main` and offer a newer core in **Settings → Proxy
core**, so a reviewed core reaches users when its pin merges, without an app
release. To publish one:

1. Review the chosen [upstream release](https://github.com/router-for-me/CLIProxyAPI/releases)
   and its changes to configuration, management APIs, authentication and routing.
2. Run `bun run core:pin <version>`, or `bun run core:pin` for the latest release.
   Like `core:install` it runs `bun run core:tool`, the dev-only
   [core tool](../crates/core/examples/core-tool.rs) over the daemon's own pin code.
   It requires all four targets in the release's `checksums.txt`, checks each
   downloaded archive against it, and writes the archive and executable hashes.
   Checksums published beside the downloads establish integrity, not independent
   assurance of the publisher.
3. If the core needs app changes, set `minApp` to the first NonstopVibin version
   (a release or a beta) that has them; older apps keep their core. `core:pin` keeps an existing `minApp`.
4. Quit the running app and run `bun run core:install`. This installs the pin
   into `.vendor/core`. For another packaging target, pass `--target`, for example
   `bun run core:install --target linux_x64`.
5. Run the checks of the first-release checklist above and exercise the
   synthetic-provider integration tests against the new core.
6. Try the pin on an installed app: start it with `NONSTOPVIBIN_CORE_FEED` set to
   the pull request's raw `core-release.json` URL and update from Settings.
7. Record the core upgrade in `CHANGELOG.md` and merge. Installed apps offer the
   core at their next check: at launch, and whenever Settings opens. The next app
   release bundles it.

The [Core pin workflow](../.github/workflows/core-pin.yml) does step 2 on its
schedule, and on demand from the Actions tab. When upstream's latest stable release is
newer than the pin and old enough (the workflow's `age` check), it opens a pull request titled
`build(deps): update CLIProxyAPI to <version>` with the rest of this list as a
checklist. Pull requests opened with the workflow's own token start no workflows,
so it starts the build and security checks on its branch. Closing the pull request
declines that version; the workflow does not open it again. The workflow never runs
the downloaded core: the pull request's checks do. It needs **Settings → Actions →
General → Allow GitHub Actions to create and approve pull requests**. Until that is
on, each run pushes its branch and then fails to open the pull request. GitHub
disables scheduled workflows in a public repository after 60 days without activity.

An installed app downloads only the pinned version's archive from upstream's
release URL, and installs it only when the archive and its executable match the
pin for its platform and architecture. It starts the new core on a throwaway
profile, keeps it in `cores/<version>` in the data folder, then restarts running
profiles on it; requests in progress are interrupted. Settings shows it as
updated from the bundled core, and **Use bundled** checks the bundled core,
restarts profiles on it, then deletes installed cores. At launch the newest intact
installed core above the bundled one runs, so an app update that bundles a newer
or equal core replaces it. Development runs never install or select cores: they share the data
folder with an installed app. On macOS an installed core keeps upstream's own
signature instead of the app's; that path is untested.

`core:install` and packaging accept only the bundled pin. Packaging rejects
mismatched versions, platforms, architectures and binary hashes, and ships the
staged release's own `LICENSE` as `licenses/CLIProxyAPI.txt`.

The 8.1.0 upgrade keeps the legacy YAML configuration and `/v0/management`
routes, which upstream retains alongside its v8 configuration and OAuth APIs.
Upstream 8.1.0 adds a GitHub Copilot provider, which NonstopVibin does not offer.
On its first start a core writes a generated `server-id` into its profile's config
file. The app rewrites that file at each start, so the ID changes then; nothing
reads it. Linux uses the standard GLIBC 2.17 release assets; dynamic plugins remain
disabled. Since 8.0.67 the core supplies its own Claude Code fingerprint without a
wrapper override.
App startup upgrades existing owned Pi connections; reload Pi to remove older
tool-change fallbacks. Core installation replaces the binary for the next process
start; restart NonstopVibin to replace any running profile cores.
macOS signing updates the packaged manifest hash after verifying the unsigned core.
Builds up to 0.2.0 receive a new core only with a newer app; deb installations and
builds older than 0.2.0 install that manually. Development checkouts rerun
`bun run core:install` after pulling a changed pin, then restart the app.

## Installed behavior

Enabled packaged macOS and AppImage builds check shortly after startup and then
periodically (`FIRST_CHECK` and `EVERY` in `crates/shell/src/update/mod.rs`).
**Check for updates…** is available in the app menu (the macOS menu bar, the Linux main window's menu bar) and the tray's menu; it is disabled on unsupported or unconfigured builds.

Downloads happen in the background. A native dialog offers **Restart to update**
or **Later**, defaulting to Later because restart interrupts connected agents.
After Later, use the same menu command to install the downloaded update. Ordinary
Quit does not opt into installation. Checks and downloads are serialized, and
background failures remain quiet. Restart closes the local service, swaps in the new version, and starts it. The
app verifies the release before it swaps anything: its signed `SHA256SUMS`,
the feed and the payload hashes, and on macOS the bundle's signature and core
([SECURITY.md](../SECURITY.md#app-updates)). Credentials, the database and installed core updates remain in
the data directory. The bundled core is replaced with the app bundle; newer
reviewed cores install from Settings (see
[Updating the CLIProxyAPI core](#updating-the-cliproxyapi-core)).

On macOS, the app updates only from a folder the user can change, such as
Applications. Run from the disk image, from App Translocation or from a folder the
user cannot change, it does not check, and **Check for updates…** explains how to
move it.

Existing installations without enabled update support need one manual install.
Linux deb users continue installing newer deb packages manually.

## Verification before the first public release

Run the checks of the first-release checklist above. Verify the packaged archive
with `bun scripts/verify-rust-package.ts <artifact>` and launch the packaged app,
which holds the updater in its daemon.

Before advertising updates, use synthetic profiles to test two macOS versions
installed from the DMG and two AppImage versions (the first pair is 0.2.0 and the
release after it):
update discovery, Later, explicit restart, shutdown of the core/gateway, restored
credentials/profile identity, and offline/corrupt-download recovery. Verify beta
discovery separately before advertising it. `bun run test:update:mac` proves the
Electron-to-Rust update and the Rust-to-Rust update in place on a CI runner, without Gatekeeper's first-launch check and with the runner's
pre-granted privacy permissions. `bun run test:update:linux` updates an AppImage on a
CI runner and checks that a tampered payload and a foreign signature are refused.
Installation and Linux desktop behavior remain unverified until tested on the
supported target systems.
