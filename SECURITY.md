# Security and release policy

nonstopvibin handles subscription credentials and is a local desktop application
that is being prepared for its first public release. Passing automated checks is
necessary but does not certify a release.

## Supported versions

Only the latest release receives security fixes. The public API and behavior may
change without notice during the 0.x release series.

## Supported deployment

The application is a loopback desktop service on macOS/Linux, or a headless
Linux service reached through SSH (optionally HTTPS Tailscale Serve; see the
[server guide](docs/server.md)), not an internet-facing or multi-user hosted proxy.
The agent gateway remains loopback-only in both modes. Profiles separate account
pools, not OS users. SQLite
secrets use AES-GCM with an owner-only vault.key stored beside the database; software
running as the same OS user can read both and decrypt them. The upstream core also
requires plaintext OAuth/config files in owner-only directories. See
[storage documentation](docs/architecture.md) for storage locations.

The Rust daemon enforces these boundaries. The gateway crate cannot name the
management client, so a profile key cannot reach management endpoints. The
listener checks Host, Origin and the session token. Only the window helper links
WebKit. It accepts bridge calls only from the main frame of a page on the daemon's
origin, denies other navigation and new windows, refuses every download but a file
the page makes itself (a `blob:` URL of the daemon's origin, as Activity's CSV
export), which it saves in the Downloads folder, and receives the session token on
its standard input. On Linux, where WebKitGTK checks such a download as a
navigation, the page may also open a `blob:` URL it made; the helper drops what that
page posts. The daemon validates the arguments of the four calls the helper forwards
to it again.

## Reporting

Report vulnerabilities through [GitHub private vulnerability reporting](https://github.com/samuelfarkas/nonstopvibin/security/advisories/new).
Do not put credentials, provider responses, or exploit details in public issues.
Do not include credentials or raw provider responses in a private report either.
No response-time commitment is currently offered.

## Required checks

Every pull request and every release runs them: the build, test, smoke and packaging
checks in [checks.yml](.github/workflows/checks.yml) and the dependency, tooling and
secret scans in [security.yml](.github/workflows/security.yml). To run them locally,
follow the checklist in [releases](docs/releases.md#first-release-020); if DeepSec is
installed, also run `bun run security:tooling`. Dependency audits fail
at high severity, and `cargo deny check` and `cargo audit` check the Rust
workspace against `deny.toml` and RustSec; inspect moderate findings too. Secret scans redact output.
The directory scan excludes generated local state through `.gitleaks-source.toml`;
the history scan uses `.gitleaks.toml` and has no directory exclusions. The only
fixture exception is the exact public RFC 6455 example WebSocket nonce, constrained
to its test file and header line. Never suppress a whole test/source directory to
hide a finding. Rotate exposed credentials; adding them to .gitignore is insufficient.

Make the desktop build, secret scanning, and dependency jobs required checks for
protected branches; [releases](docs/releases.md#enable-releases-and-updates) names
them. Review dependency and action updates before merging. Workflows
use read-only repository permissions and do not receive signing or model credentials
on pull requests. Raw scanner reports are not public artifacts.

## Deep review

Use `.deepsec/README.md` for setup and bounded investigation. Review changes to
profile routing, credential import/refresh, gateway forwarding, IPC, storage, and
packaging with the threat context in `.deepsec/data/nonstopvibin/INFO.md`.
Validate findings with synthetic reproducers and regression tests before fixing or
publishing them. Review the bundled CLIProxyAPI release separately: scanning this
Rust host does not audit the upstream Go implementation or binary.

## Core updates

Installed builds offer the CLIProxyAPI core that the reviewed pin on `main`
(`crates/core/core-release.json`) names. They download it only from the upstream
release URL for that version and install it only when the archive and the
extracted executable match the pin's SHA-256 for their platform and architecture.
Merging a pin therefore ships its core to installed users: review it as a release.
`NONSTOPVIBIN_CORE_FEED` changes only where the pin is read; the archive origin is
fixed. Installed cores are owner-only files in the data folder, checked against
their manifest at launch. Like the database, they are not protected from software
running as the same OS user.

## App updates

The Rust app updates itself from this repository's GitHub Releases
(`crates/shell/src/update`). It reads `releases.atom` for the newest version it may
install (a beta only from a beta, never a downgrade), then that release's
`SHA256SUMS`, `latest-mac.yml` or `latest-linux.yml`, and the zip or AppImage. It
accepts the update only when:

- `SHA256SUMS.minisig` verifies with the public key compiled in from
  `build/update-signing.pub`, and its trusted comment names this version, once the
  owner has committed that key ([release signing](docs/release-signing.md#sign-updates-for-the-rust-updater));
- the feed and the payload match their lines in `SHA256SUMS`, and the payload
  matches the feed's size and SHA-512, the check electron-updater made;
- on macOS, the unpacked bundle satisfies the running bundle's designated
  requirement (`codesign --verify --deep --strict`) and its core matches its manifest.

Requests use HTTPS with the bundled web PKI roots, follow redirects only to
`github.com`, `objects.githubusercontent.com` and
`release-assets.githubusercontent.com`, and read at most the feed's payload size.
The update is staged beside the app and swapped in with one atomic rename where the
file system supports it. Logs never contain response bodies.

**What the signature adds.** Electron builds trusted whatever the release served
over HTTPS: the SHA-512 sits in a feed from the same release, so anyone who can
upload or replace release assets (a leaked token with `contents: write`, a hijacked
GitHub session, a compromised workflow outside the `release` environment) could ship
an update, and an ad-hoc macOS requirement accepts any code with the bundle
identifier. With the key committed, that attacker also needs the minisign secret,
which only the `release` environment holds and only `v*` tags reach.

**What it does not cover.** Anything that runs in the release workflow can sign: a
malicious change merged and tagged, a compromised action in `release.yml`, or an
admin who changes the environment's rules. Signing proves the release workflow
published the files, not what source they came from; build provenance
(`gh attestation verify`) covers that. Builds from before the key was committed
verify nothing beyond HTTPS and the hashes, and their first update with a key is
trusted that way. A lost or leaked key needs a release that ships a new public key.
Software running as the same OS user can replace the app directly.

## Distribution checklist

- Include the application MIT license and preserve third-party licenses. Audit the
  final package's notices, including native binaries and assets.
- Build from a reviewed revision with clean, frozen dependency installs. Record its
  source revision, core version and hashes, target OS/architecture, and artifact hash.
  The reviewed core pins are the archive and binary hashes in
  `crates/core/core-release.json`; the packaged manifest is derived data.
- Inspect the actual archive: include only built app code, production dependencies,
  reviewed assets/licenses, and the verified core. Exclude `.deepsec`, `.agents`,
  runtime data, environment files, signing keys, local review material, and raw scan
  reports.
- macOS releases are ad-hoc signed and not notarized until a Developer ID is set up.
  An installed app accepts any update with its bundle identifier, so an update's
  authenticity rests on GitHub Releases and, in the Rust app, the release key
  ([App updates](#app-updates)); [macOS release signing](docs/release-signing.md)
  describes the signing. Until the first Developer ID release,
  say in the README and release notes that macOS builds are not notarized. Test
  installation, launch, quit, and reopen on a clean machine.
  Test owner-only storage permissions and credential persistence after reopening.
- Run end-to-end provider verification only with deliberately assigned test accounts.
  Keep live credentials and customer/work profiles out of reusable fixtures.
- Resolve high/critical findings and document remaining limitations. Dependency and
  regex scans alone cannot establish distribution safety.

Record the checks performed on each release.
Unverified platforms and providers remain unverified.
