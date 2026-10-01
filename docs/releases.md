# Releases and app updates

GitHub Actions builds releases; GitHub Releases hosts installers and update metadata.
No separate update server is required. Updates are currently disabled by
`updatesEnabled: false` in `package.json`. The release repository is
[samuelfarkas/nonstopvibin](https://github.com/samuelfarkas/nonstopvibin).
Development builds and Linux deb installations also do not auto-update.

## Enable releases and updates

1. Configure the five Apple signing/notarization secrets in
   [release signing](release-signing.md). Tagged releases fail without signing;
   ordinary CI and local packaging can still build unsigned installers.
2. Set `updatesEnabled` to `true`. This is compiled into the app; changing it on
   GitHub cannot enable an already-installed build.
3. Keep release tags protected and require reviewed changes on the default branch.
   Run an actual signed upgrade between two versions before distributing publicly.

The workflow checks that the configured update repository equals the repository
publishing the release. It uses Actions' built-in `GITHUB_TOKEN` only in the publish
job; clients download public assets without credentials.

## Publish a version

Bump `package.json` to an unused version and update `CHANGELOG.md`, then commit and
merge the reviewed changes. From that revision, for example:

```sh
git tag v0.1.2
git push origin v0.1.2
```

The tag must equal `v` plus the package version. Normal versions, including `0.x`,
are regular releases. `0.2.0-beta.1` / `v0.2.0-beta.1` creates a prerelease; only
installed beta builds opt into prerelease discovery. Other prerelease names are
not supported by this workflow.

The pinned builder uses `latest-mac.yml` and `latest-linux.yml` even on GitHub
prereleases; the updater selects the release first and falls back to these names.

CI runs quality, tests, desktop smoke tests and security checks, builds macOS arm64
and Linux x64 packages, and signs/notarizes macOS. After all checks pass, the final
job verifies metadata versions, payload sizes and SHA-512 hashes, generates
SHA256SUMS and provenance attestations, uploads a draft release, then publishes it.
Only the final job can publish. A failed upload leaves a draft, not a partial public
update. Remove the failed draft before retrying the publish job; never overwrite
an already-published version. Fix a bad release with a higher version.

Preserve the generated DMG, macOS ZIP, AppImage, deb, blockmaps and channel YAML
files. A DMG alone cannot supply a macOS update. Do not upload builder debug files.
The GitHub-generated source ZIP is not an update payload.

## Updating the CLIProxyAPI core

CLIProxyAPI ships inside every installer as a separate native executable at
`resources/core/cli-proxy-api` (inside `Contents/Resources` on macOS), alongside
`manifest.json`. Electron launches it as a child process for each running profile.
Users do not install the core separately. Settings shows the version pinned in
`scripts/core-release.json`.

Keep core upgrades part of an app release so the wrapper and core are tested and
distributed together. To upgrade:

1. Review the chosen [upstream release](https://github.com/router-for-me/CLIProxyAPI/releases)
   and its changes to configuration, management APIs, authentication and routing.
2. Update `scripts/core-release.json`: the version, archive SHA-256 checksums, and
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

The 8.0.8 upgrade keeps the legacy YAML configuration and `/v0/management`
routes, which upstream retains alongside its new v8 configuration and OAuth APIs.
Linux uses the standard GLIBC 2.17 release assets; dynamic plugins remain disabled.
The core now supplies its Claude Code 2.1.280 fingerprint without a wrapper override.
App startup upgrades existing owned Pi connections; reload Pi to remove older
tool-change fallbacks. Core installation replaces the binary for the next process
start; restart NonstopVibin to replace any running profile cores.
macOS signing updates the packaged manifest hash after verifying the unsigned core.
Installed users receive the new core by updating NonstopVibin; while app updates
are disabled, they install the newer app manually. Development checkouts rerun
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

Using synthetic profiles, test two signed macOS versions and two AppImage versions:
update discovery, Later, explicit restart, shutdown of the core/gateway, restored
credentials/profile identity, and offline/corrupt-download recovery. Verify beta
discovery separately before advertising it. Unsigned local builds cannot establish
macOS update correctness. Installation and Linux desktop behavior remain unverified
until tested on the supported target systems.
