# macOS release signing

## Set up the certificate

1. Open the [Apple Developer account](https://developer.apple.com/account).
2. Go to **Certificates**, click **+**, and choose **Developer ID Application**.
3. Create a certificate signing request in Keychain Access. Choose **Keychain Access → Certificate Assistant → Request a Certificate From a Certificate Authority**. Save it to disk.
4. Upload the request. Download the certificate and double-click it to install it.
5. In Keychain Access, select the certificate and its private key. Export both as a password-protected `.p12` file.
6. Copy the encoded certificate:

```sh
base64 -i cert.p12 | pbcopy
```

## Add release secrets

Find the Team ID under **Membership details** in the Apple Developer account.

Create an app-specific password at [account.apple.com](https://account.apple.com). Open **Sign-In and Security → App-Specific Passwords**.

Open the GitHub repository. Go to **Settings → Secrets and variables → Actions**. Add these repository secrets:

- `CSC_LINK`: the base64 text from the `.p12` file
- `CSC_KEY_PASSWORD`: the `.p12` export password
- `APPLE_ID`: the Apple account email
- `APPLE_APP_SPECIFIC_PASSWORD`: the app-specific password
- `APPLE_TEAM_ID`: the Apple Developer Team ID

## Test a signed release

Run the release build with all five values set:

```sh
CSC_LINK=... CSC_KEY_PASSWORD=... APPLE_ID=... APPLE_APP_SPECIFIC_PASSWORD=... APPLE_TEAM_ID=... bun run dist:mac
codesign --verify --deep --strict --verbose=2 release/mac-arm64/NonstopVibin.app
xcrun stapler validate release/mac-arm64/NonstopVibin.app
spctl -a -vv -t execute release/mac-arm64/NonstopVibin.app
```

The `spctl` result should say `accepted, source=Notarized Developer ID`. Notarization usually takes 2 to 15 minutes. The DMG is not signed or stapled. Gatekeeper checks the app inside it.

Signing rewrites the bundled core binary. `scripts/sign-mac.cjs` refreshes the packaged manifest hash during signing. This happens after `scripts/verify-package.cjs` compares the unsigned copy with the reviewed input.

Electron's own binaries get `build/entitlements.mac.plist` (JIT and unsigned executable memory). The core and the tray click helper are signed with hardened runtime and the empty `build/entitlements.mac.minimal.plist`, so they get no such exceptions. Check with `codesign -d --entitlements - <file>`.

Keep the `.p12` file and its password out of the repository. Rotate the certificate in the Apple portal if it leaks. Anyone with repository admin access can replace workflows and use the `.p12` stored in GitHub secrets to sign as you.

## Why this matters

Gatekeeper on macOS 15 and later blocks unsigned apps with no Control-click override.

## After the first signed release

Re-enable the `onlyLoadAppFromAsar` and `enableEmbeddedAsarIntegrityValidation` fuses. Enable cookie encryption. Test packaging, launch, updates, credential access, and agent connections before release.
