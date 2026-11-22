# macOS release signing

Releases are ad-hoc signed. They need no Apple Developer ID, no notarization and no
signing secrets. [Switch to a Developer ID](#switch-to-a-developer-id) describes the
change for later.

## How ad-hoc builds update

`mac.identity` is `-` in `package.json`, so electron-builder signs every build ad-hoc
through `scripts/sign-mac.cjs`. Squirrel.Mac, which installs app updates on macOS,
accepts a new app only when it satisfies the running app's designated requirement.
An ad-hoc signature's own requirement is its code hash, and no later build has that
hash. The sign script therefore gives the app bundle this requirement:

```text
designated => identifier "app.nonstopvibin.desktop"
```

Any later build with that identifier and a valid signature can replace the app. The
requirement does not identify the publisher. The update's authenticity comes from
HTTPS to GitHub Releases and the SHA-512 of the zip in `latest-mac.yml`, as for the
AppImage. Users can check a download with
`gh attestation verify <file> -R samuelfarkas/nonstopvibin`.

macOS also recognizes the app by this requirement in privacy permissions and keychain
access, so any ad-hoc code with that identifier counts as NonstopVibin there. The app
uses neither today. Cookie encryption, which would add a keychain item, waits for a
Developer ID.

Ad-hoc code has no Team ID, so hardened Electron processes would fail library
validation. `build/entitlements.mac.plist` gives Electron's binaries
`com.apple.security.cs.disable-library-validation` with the JIT and executable-memory
entitlements. The core and the tray click helper keep the empty
`build/entitlements.mac.minimal.plist`.

macOS blocks the first launch of a downloaded app that Apple has not notarized. Users
open **System Settings → Privacy & Security** and click **Open Anyway** once. Updates
are not quarantined, so the updated app opens without that step.

Do not re-sign an installed app. `codesign --force` replaces the requirement with the
new code hash, and the app can no longer update.

`bun run dist:built`, which CI runs, sets `CSC_FOR_PULL_REQUEST`. electron-builder
otherwise skips signing in pull request builds, and those builds could not update.

`bun run test:update:mac` updates an older build to the packaged one on every pull
request and release.

Signing rewrites the bundled core binary. `scripts/sign-mac.cjs` refreshes the packaged
manifest hash during signing. This happens after `scripts/verify-package.cjs` compares
the unsigned copy with the reviewed input.

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

Open the GitHub repository. Go to **Settings → Secrets and variables → Actions**. Add these repository secrets:

- `CSC_LINK`: the base64 text from the `.p12` file
- `CSC_KEY_PASSWORD`: the `.p12` export password
- `APPLE_ID`: the Apple account email
- `APPLE_APP_SPECIFIC_PASSWORD`: the app-specific password
- `APPLE_TEAM_ID`: the Apple Developer Team ID

Keep the `.p12` file and its password out of the repository. Rotate the certificate in the Apple portal if it leaks. Anyone with repository admin access can replace workflows and use the `.p12` stored in GitHub secrets to sign as you.

### Change the build

1. In `package.json`, remove `"identity": "-"` and set `"notarize": true`. The sign
   script then keeps the requirement codesign derives from the certificate. Remove
   `com.apple.security.cs.disable-library-validation` from
   `build/entitlements.mac.plist`.
2. Pass the five secrets from `.github/workflows/release.yml` to `checks.yml`, and give
   them to its macOS `Package` step only in releases. Pull requests must not receive
   them.
3. Make `tests/mac-update.mjs` expect a Developer ID signature instead of an ad-hoc one.
4. Remove the **Open Anyway** instructions from the release notes in
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

Re-enable the `onlyLoadAppFromAsar` and `enableEmbeddedAsarIntegrityValidation` fuses. Enable cookie encryption. Test packaging, launch, updates, credential access, and agent connections before release.
