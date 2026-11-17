# Releases and app updates

GitHub Actions builds releases; GitHub Releases hosts installers and update metadata.
No separate update server is required. Updates are enabled by
`updatesEnabled: true` in `package.json` from 0.2.0, the first tagged release. The
release repository is
[samuelfarkas/nonstopvibin](https://github.com/samuelfarkas/nonstopvibin).
Development builds and Linux deb installations do not auto-update.

## Enable releases and updates

Done in the repository:

- `updatesEnabled` is `true` and the update feed in `package.json` names this
  repository. The flag is compiled into the app; changing it on GitHub cannot
  enable an already-installed build.
- The "Protect release tags" ruleset is active for `v*` tags. It blocks deleting
  and force-moving a release tag; it does not restrict who can create one.

Remaining owner steps:

1. Configure the five Apple signing/notarization secrets in
   [release signing](release-signing.md). None are configured yet, so a tag push
   today passes the version preflight and then fails at the macOS build job's
   signing step. Ordinary CI and local packaging still build unsigned installers.
2. Protect `main`. It has no branch protection, although this document and
   [SECURITY.md](../SECURITY.md) assume reviewed changes and required checks:
   require pull requests with review, and require the `build (macos-26)`,
   `build (ubuntu-24.04)`, `dependencies` and `secrets` checks. The macOS check was
   named `build (macos-latest)` before the runner label was pinned to `macos-26`.
3. Run an actual signed upgrade between two versions before advertising updates.
   The first chance is 0.2.0 to the next release.

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
not supported by this workflow.

The pinned builder uses `latest-mac.yml` and `latest-linux.yml` even on GitHub
prereleases; the updater selects the release first and falls back to these names.

CI runs quality, tests, desktop smoke tests and security checks, builds macOS arm64
and Linux x64 packages and the headless Linux server archives (x64 and arm64), and
signs/notarizes macOS. After all checks pass, the final job verifies metadata
versions, payload sizes and SHA-512 hashes, generates
SHA256SUMS and provenance attestations, uploads a draft release, then publishes it.
Only the final job can publish. A failed upload leaves a draft, not a partial public
update. Remove the failed draft before retrying the publish job; never overwrite
an already-published version. Fix a bad release with a higher version.

Preserve the generated DMG, macOS ZIP, AppImage, deb, blockmaps and channel YAML
files. A DMG alone cannot supply a macOS update. Do not upload builder debug files.
The GitHub-generated source ZIP is not an update payload.

## First release (0.2.0)

`package.json` is at 0.2.0 with updates enabled, and `CHANGELOG.md` has a `[0.2.0]`
section. Work through these steps in order.

1. **Preconditions.** The five signing secrets are configured, and a local signed
   build passes the checks in [release signing](release-signing.md). `main` is
   protected as described above, and the 0.2.0 changes are merged through a reviewed
   pull request with green checks. No `v0.2.0` tag or release exists yet
   (`git ls-remote --tags origin`, `gh release list`). The tag ruleset blocks
   deleting or moving a pushed tag, so push it only when everything else is ready.
2. **Leave the deferred hardening for a later release.** These are owner steps after
   0.2.0 is signed and published; do not enable them for 0.2.0:
   - Re-enable the `onlyLoadAppFromAsar` and `enableEmbeddedAsarIntegrityValidation`
     fuses. They stay off because unsigned builds failed to launch with them, so
     only a real signed build can show that they work.
   - Enable cookie encryption (the `enableCookieEncryption` fuse). It is listed for
     after the first signed release without a stated reason; proving it needs a
     signed macOS build and a packaged Linux build, because it changes how Electron
     protects its browser storage on each platform.
   - Then test packaging, launch, updates, credential access, and agent connections
     before that release.
3. **Run the checks** on a clean checkout of the merged revision:

   ```sh
   bun ci
   bun run core:install
   bun run quality
   bun test
   bun run build
   bun run test:desktop
   bun run test:ui
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
   update feed; runs the dependency, tooling and secret scans; builds, tests and
   smoke-tests on `macos-26` and `ubuntu-24.04`; signs, notarizes and verifies the
   macOS app; builds the AppImage, the deb and the two headless server archives; and
   finally verifies the update metadata, writes `SHA256SUMS`, attests provenance,
   and publishes the release with generated notes. If a job fails for a reason that needs no source change, such as
   a missing secret, fix it and re-run the failed jobs. If the source must change,
   release 0.2.1 instead; the tag cannot be moved.
6. **Confirm the result.** `gh release view v0.2.0` shows a published, non-prerelease
   release with the DMG, macOS ZIP, AppImage, deb, blockmaps, `latest-mac.yml`,
   `latest-linux.yml`, `nonstopvibin-server-0.2.0-linux-x64.tar.gz`,
   `nonstopvibin-server-0.2.0-linux-arm64.tar.gz` and `SHA256SUMS`. In a folder
   with the downloaded assets, `shasum -a 256 -c SHA256SUMS` passes and
   `gh attestation verify <file> -R samuelfarkas/nonstopvibin` passes for an
   installer. On a clean Mac, the installed app passes
   `spctl -a -vv -t execute /Applications/NonstopVibin.app` as
   `accepted, source=Notarized Developer ID`, launches, quits, and reopens with its
   data. Settings shows 0.2.0, and **Check for updates…** reports that no updates
   are available.

## Updating the CLIProxyAPI core

CLIProxyAPI ships inside every installer as a separate native executable at
`resources/core/cli-proxy-api` (inside `Contents/Resources` on macOS), alongside
`manifest.json`. Electron launches it as a child process for each running profile;
[storage and profile boundaries](architecture.md) describes how a core is tied to
the app's lifetime.
Users do not install the core separately. Settings shows the version pinned in
`src/server/core-release.json`.

Keep core upgrades part of an app release so the wrapper and core are tested and
distributed together. To upgrade:

1. Review the chosen [upstream release](https://github.com/router-for-me/CLIProxyAPI/releases)
   and its changes to configuration, management APIs, authentication and routing.
2. Update `src/server/core-release.json`: the version, archive SHA-256 checksums, and
   SHA-256 hashes of the extracted unsigned executables for all four listed targets.
   Verify downloaded archives against upstream checksums before extracting and
   hashing their executables. Checksums published beside the downloads establish
   integrity, not independent assurance of the publisher.
3. Quit the running app and run `bun run core:install`. This installs the reviewed
   pin into `.vendor/core`; it does not select the latest upstream release.
   For another packaging target, set `CORE_PLATFORM` and `CORE_ARCH`, for example
   `CORE_PLATFORM=linux CORE_ARCH=x64 bun run core:install`.
4. Run `bun run quality`, `bun test`, `bun run build`, and the release checks in
   [SECURITY.md](../SECURITY.md). Exercise the synthetic-provider integration tests
   against the new core and verify the packaged app on each shipping target.
5. Bump the app version, record the core upgrade in `CHANGELOG.md`, and publish
   through the normal release workflow above.

Packaging rejects mismatched versions, platforms, architectures and binary hashes.

The 8.0.67 upgrade keeps the legacy YAML configuration and `/v0/management`
routes, which upstream retains alongside its new v8 configuration and OAuth APIs.
Linux uses the standard GLIBC 2.17 release assets; dynamic plugins remain disabled.
The core now supplies its Claude Code 2.1.321 fingerprint without a wrapper override.
App startup upgrades existing owned Pi connections; reload Pi to remove older
tool-change fallbacks. Core installation replaces the binary for the next process
start; restart NonstopVibin to replace any running profile cores.
macOS signing updates the packaged manifest hash after verifying the unsigned core.
Installed users receive the new core by updating NonstopVibin; deb installations
and builds older than 0.2.0 install the newer app manually. Development checkouts rerun
`bun run core:install` after pulling a changed pin, then restart the app.

## Installed behavior

Enabled packaged macOS and AppImage builds check 30 seconds after startup and
every six hours. **Check for updates…** is available in the app menu and the tray's
right-click menu; it is disabled on unsupported or unconfigured builds.

Downloads happen in the background. A native dialog offers **Restart to update**
or **Later**, defaulting to Later because restart interrupts connected agents.
After Later, use the same menu command to install the downloaded update. Ordinary
Quit does not opt into installation. Checks and downloads are serialized, and
background failures remain quiet. Restart closes the local service before handing
off to the updater. Credentials and the database remain in the data directory;
the pinned core is replaced only as part of the app bundle.

Existing installations without enabled update support need one manual install.
Linux deb users continue installing newer deb packages manually.

## Verification before the first public release

Run `bun run quality`, affected tests, `bun run build`, `bun run security:deps`,
and the remaining release checks in `SECURITY.md`. Verify the packaged archive and
launch the actual Electron build; the desktop bundle includes the updater and
does not ship a separate node_modules tree.

Before advertising updates, use synthetic profiles to test two signed macOS versions
and two AppImage versions (the first pair is 0.2.0 and the release after it):
update discovery, Later, explicit restart, shutdown of the core/gateway, restored
credentials/profile identity, and offline/corrupt-download recovery. Verify beta
discovery separately before advertising it. Unsigned local builds cannot establish
macOS update correctness. Installation and Linux desktop behavior remain unverified
until tested on the supported target systems.
