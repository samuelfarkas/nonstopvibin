# macOS release signing

Releases are ad-hoc signed. They need no Apple Developer ID, no notarization and no
signing secrets. [Switch to a Developer ID](#switch-to-a-developer-id) describes the
change for later.

## How ad-hoc builds update

Without signing secrets, `scripts/package-mac.ts` signs every build ad-hoc. The
updater accepts a new app only when it satisfies the running app's designated
requirement. Squirrel.Mac, which installed Electron builds use, applies the same
rule. An ad-hoc signature's own requirement is its code hash, and no later build has
that hash. The package script therefore gives the app bundle this requirement:

```text
designated => identifier "app.nonstopvibin.desktop"
```

Any later build with that identifier and a valid signature can replace the app. The
updater checks the unpacked bundle against the running bundle's designated
requirement before it swaps it in. The requirement does not identify the publisher. The update's authenticity comes from HTTPS to GitHub Releases
and the SHA-512 of the zip in `latest-mac.yml`, as for the AppImage, and in the Rust
app from the signed `SHA256SUMS` once the release key is committed
([SECURITY.md](../SECURITY.md#app-updates)). Users can check a download with
`gh attestation verify <file> -R samuelfarkas/nonstopvibin`.

macOS also recognizes the app by this requirement in privacy permissions and keychain
access, so any ad-hoc code with that identifier counts as NonstopVibin there. The app
uses neither today.

Every executable (the daemon, the window helper, the core and the relaunch helper)
gets the hardened runtime and the empty `build/entitlements.mac.minimal.plist`: no JIT,
executable-memory or library-validation exceptions. WebKit's JIT runs in Apple's
WebContent process, and both Rust executables link only system libraries, which
library validation accepts. `scripts/verify-rust-package.ts` fails the package if an
executable links anything outside `/System/Library` or `/usr/lib`.

macOS blocks the first launch of a downloaded app that Apple has not notarized. Users
open **System Settings → Privacy & Security** and click **Open Anyway** once. Updates
are not quarantined, so the updated app opens without that step.

Do not re-sign an installed app. `codesign --force` replaces the requirement with the
new code hash, and the app can no longer update.

Pull request builds are signed the same way, so they can update and be updated.
On every pull request and release, `bun run test:update:mac` updates an Electron build
and then an older Rust build to the packaged app.

Signing rewrites the bundled core binary. `scripts/package-mac.ts` checks the
unsigned core against the reviewed pin, signs it, and then writes the signed binary's
hash into the packaged manifest. The desktop app checks the core against that
manifest at launch.

The bundle also holds a minimal `Squirrel.framework`. Squirrel.Mac in an Electron
build relaunches the updated app by running ShipIt from the new bundle, and
`scripts/squirrel-relaunch.c` is that ShipIt: it only opens the bundle.

## Switch to a Developer ID

Gatekeeper on macOS 15 and later blocks apps without a Developer ID signature, and only
**Open Anyway** gets past it. A signed and notarized app opens without that step.

### Set up the certificate

1. Open the [Apple Developer account](https://developer.apple.com/account).
2. Go to **Certificates**, click **+**, and choose **Developer ID Application**.
3. Create a certificate signing request in Keychain Access. Choose **Keychain Access → Certificate Assistant → Request a Certificate From a Certificate Authority**. Save it to disk.
4. Upload the request. Download the certificate and double-click it to install it.
5. In Keychain Access, select the certificate and its private key. Export both as a password-protected `.p12` file.
6. Copy the encoded certificate:

```sh
base64 -i cert.p12 | pbcopy
```

### Add release secrets

Find the Team ID under **Membership details** in the Apple Developer account.

Create an app-specific password at [account.apple.com](https://account.apple.com). Open **Sign-In and Security → App-Specific Passwords**.

Open the GitHub repository. Go to **Settings → Environments** and open the `release`
environment (see [Sign updates](#sign-updates-for-the-rust-updater)). Add these environment
secrets:

- `CSC_LINK`: the base64 text from the `.p12` file
- `CSC_KEY_PASSWORD`: the `.p12` export password
- `APPLE_ID`: the Apple account email
- `APPLE_APP_SPECIFIC_PASSWORD`: the app-specific password
- `APPLE_TEAM_ID`: the Apple Developer Team ID

Keep the `.p12` file and its password out of the repository. Rotate the certificate in the Apple portal if it leaks. Anyone with repository admin access can replace workflows and use the `.p12` stored in GitHub secrets to sign as you.

### Change the build

`scripts/package-mac.ts` signs with the Developer ID, notarizes and staples when all
five secrets are set, and `.github/workflows/checks.yml` passes them to its macOS
**Package** step only in releases. Pull requests do not receive them. The executables
already use the empty entitlements. The remaining steps are:

1. Make `tests/mac-update.mjs` expect a Developer ID signature instead of an ad-hoc
   one.
2. Remove the **Open Anyway** instructions from the release notes in
   `.github/workflows/release.yml`, the README's Install section, and step 6 of the
   first-release checklist in [releases](releases.md).

The first Developer ID release still updates ad-hoc installs, because it satisfies
their `identifier "app.nonstopvibin.desktop"` requirement. Later releases must satisfy
the Developer ID requirement.

### Test a signed release

Run the release build with all five values set:

```sh
CSC_LINK=... CSC_KEY_PASSWORD=... APPLE_ID=... APPLE_APP_SPECIFIC_PASSWORD=... APPLE_TEAM_ID=... bun run dist:mac
codesign --verify --deep --strict --verbose=2 release/mac-arm64/NonstopVibin.app
xcrun stapler validate release/mac-arm64/NonstopVibin.app
spctl -a -vv -t execute release/mac-arm64/NonstopVibin.app
```

The `spctl` result should say `accepted, source=Notarized Developer ID`. Notarization usually takes 2 to 15 minutes. The DMG is not signed or stapled. Gatekeeper checks the app inside it.

### After the first signed release

Test packaging, launch, updates, credential access, and agent connections before the
next release.

## Sign updates for the Rust updater

Release CI signs `SHA256SUMS` with [minisign](https://jedisct1.github.io/minisign/) into
`SHA256SUMS.minisig`, and the Rust updater accepts an update only when that signature and
the checksum verify. Until the key exists, releases publish unsigned with a warning.

1. Install minisign 0.12 and create the key pair with a password:

   ```sh
   minisign -G -p update-signing.pub -s update-signing.key
   ```

2. In **Settings → Environments**, create the `release` environment. Under **Deployment
   branches and tags**, choose **Selected branches and tags** and add the tag rule `v*`.
3. Add environment secrets: `MINISIGN_SECRET_KEY`, the contents of `update-signing.key`,
   and `MINISIGN_PASSWORD`, its password.
4. Commit `update-signing.pub` as `build/update-signing.pub`. From then on a release
   without the secret fails, and `scripts/sign-release.ts` checks each signature against
   that key.
5. Keep `update-signing.key` and its password in a password manager, off the repository.
   A lost or leaked key needs a new pair and an app release that ships the new public key
   before releases can switch to it.
