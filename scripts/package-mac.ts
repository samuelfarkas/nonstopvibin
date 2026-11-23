// Builds the macOS app from the Rust binaries for the host architecture:
// release/mac-<arch>/NonstopVibin.app, release/NonstopVibin-<version>-mac-<arch>.zip and .dmg,
// and latest-mac.yml for electron-updater. Run `bun run build:client` first.
//
// Signing follows docs/release-signing.md. Without signing secrets the app is ad-hoc signed
// with the requirement `identifier "app.nonstopvibin.desktop"`, which installed builds check
// updates against. With CSC_LINK, CSC_KEY_PASSWORD, APPLE_ID, APPLE_APP_SPECIFIC_PASSWORD and
// APPLE_TEAM_ID it signs with the Developer ID, notarizes and staples. Every executable gets the
// hardened runtime and the empty build/entitlements.mac.minimal.plist.
import { execFileSync } from "node:child_process";
import { randomBytes } from "node:crypto";
import {
  chmod,
  copyFile,
  mkdir,
  mkdtemp,
  readFile,
  rm,
  symlink,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  coreLayout,
  readCoreManifest,
  writeCoreManifest,
} from "./core-binary.ts";
import {
  architecture,
  desktopArtifact,
  product,
  rustTarget,
} from "./distribution.ts";
import {
  cargoBuild,
  releaseDir,
  repo,
  report,
  run,
  stageClient,
  stageCore,
  stageLicences,
  version,
  writeFeed,
} from "./package-common.ts";
import { sha256 } from "./pinned-download.ts";
import {
  bundle,
  squirrel,
  verifyArtifact,
  verifyMacBundle,
  verifyMacSignature,
} from "./package-layout.ts";

if (process.platform !== "darwin")
  throw new Error("Package the macOS app on macOS.");
const arch = process.arch;
const build = {
  target: rustTarget("macos", arch),
  packages: ["nsv-cli", "nsv-window"],
};
const binaries = cargoBuild(build, {
  MACOSX_DEPLOYMENT_TARGET: product.minimumMacOS,
});

/** An Info.plist with string values, then `raw` lines inside the dict. */
const plist = (
  strings: Record<string, string>,
  raw: string[] = [],
) => `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
${[
  ...Object.entries(strings).map(
    ([key, value]) => `\t<key>${key}</key>\n\t<string>${value}</string>`,
  ),
  ...raw,
].join("\n")}
</dict>
</plist>
`;
const appPlist = plist(
  {
    CFBundleDevelopmentRegion: "en",
    CFBundleDisplayName: product.name,
    CFBundleExecutable: product.name,
    CFBundleIconFile: "icon.icns",
    CFBundleIdentifier: product.id,
    CFBundleInfoDictionaryVersion: "6.0",
    CFBundleName: product.name,
    CFBundlePackageType: "APPL",
    CFBundleShortVersionString: version,
    CFBundleVersion: version,
    LSApplicationCategoryType: product.macCategory,
    LSMinimumSystemVersion: product.minimumMacOS,
    NSPrincipalClass: "NSApplication",
  },
  [
    "\t<key>LSUIElement</key>",
    "\t<true/>",
    "\t<key>NSHighResolutionCapable</key>",
    "\t<true/>",
    "\t<key>NSAppTransportSecurity</key>",
    "\t<dict>",
    "\t\t<key>NSAllowsLocalNetworking</key>",
    "\t\t<true/>",
    "\t</dict>",
  ],
);

/**
 * Squirrel.framework with only scripts/squirrel-relaunch.c as its ShipIt, and an empty
 * library as the framework binary, so codesign seals it like any framework.
 */
async function stageRelaunch(app: string): Promise<void> {
  const framework = join(app, squirrel.framework);
  const current = join(framework, "Versions/A");
  await mkdir(join(current, "Resources"), { recursive: true });
  const target = [
    "-target",
    `${architecture(arch).swift}-apple-macos${product.minimumMacOS}`,
  ];
  run("xcrun", [
    "clang",
    ...target,
    "-O2",
    "-o",
    join(app, squirrel.shipIt),
    join(repo, "scripts/squirrel-relaunch.c"),
  ]);
  run("xcrun", [
    "clang",
    ...target,
    "-dynamiclib",
    "-install_name",
    "@rpath/Squirrel.framework/Versions/A/Squirrel",
    "-o",
    join(current, "Squirrel"),
    "-x",
    "c",
    "/dev/null",
  ]);
  await writeFile(
    join(current, "Resources/Info.plist"),
    plist({
      CFBundleExecutable: "Squirrel",
      CFBundleIdentifier: `${product.id}.relaunch`,
      CFBundleInfoDictionaryVersion: "6.0",
      CFBundleName: "Squirrel",
      CFBundlePackageType: "FMWK",
      CFBundleShortVersionString: version,
      CFBundleVersion: version,
    }),
  );
  await symlink("A", join(framework, "Versions/Current"));
  await symlink("Versions/Current/Squirrel", join(framework, "Squirrel"));
  await symlink("Versions/Current/Resources", join(framework, "Resources"));
}

interface Identity {
  /** `-` for ad-hoc, else the certificate's SHA-1. */
  name: string;
  keychain?: string;
}
const minimal = join(repo, "build/entitlements.mac.minimal.plist");
function sign(path: string, identity: Identity, extra: string[] = []): void {
  run("codesign", [
    "--sign",
    identity.name,
    "--force",
    identity.name === "-" ? "--timestamp=none" : "--timestamp",
    "--options",
    "runtime",
    "--entitlements",
    minimal,
    ...(identity.keychain ? ["--keychain", identity.keychain] : []),
    ...extra,
    path,
  ]);
}

const secrets = [
  "CSC_LINK",
  "CSC_KEY_PASSWORD",
  "APPLE_ID",
  "APPLE_APP_SPECIFIC_PASSWORD",
  "APPLE_TEAM_ID",
] as const;
const env = (name: (typeof secrets)[number]) => process.env[name] ?? "";

/** Imports the Developer ID certificate into a keychain of its own. */
async function developerId(work: string): Promise<Identity> {
  const keychain = join(work, "signing.keychain-db");
  const password = randomBytes(24).toString("hex");
  const p12 = join(work, "certificate.p12");
  await writeFile(p12, Buffer.from(env("CSC_LINK"), "base64"), { mode: 0o600 });
  run("security", ["create-keychain", "-p", password, keychain]);
  run("security", ["set-keychain-settings", "-lut", "21600", keychain]);
  run("security", ["unlock-keychain", "-p", password, keychain]);
  run("security", [
    "import",
    p12,
    "-k",
    keychain,
    "-P",
    env("CSC_KEY_PASSWORD"),
    "-T",
    "/usr/bin/codesign",
  ]);
  await rm(p12);
  run(
    "security",
    [
      "set-key-partition-list",
      "-S",
      "apple-tool:,apple:",
      "-s",
      "-k",
      password,
      keychain,
    ],
    {
      stdio: ["ignore", "ignore", "inherit"],
    },
  );
  const searched = execFileSync("security", ["list-keychains", "-d", "user"], {
    encoding: "utf8",
  })
    .split("\n")
    .map((line) => line.trim().replace(/^"|"$/g, ""))
    .filter(Boolean);
  run("security", [
    "list-keychains",
    "-d",
    "user",
    "-s",
    keychain,
    ...searched,
  ]);
  const found = execFileSync(
    "security",
    ["find-identity", "-v", "-p", "codesigning", keychain],
    {
      encoding: "utf8",
    },
  ).match(/\b([0-9A-F]{40}) "Developer ID Application: [^"]+"/);
  if (!found)
    throw new Error("CSC_LINK holds no Developer ID Application identity.");
  return { name: found[1], keychain };
}

function notarize(app: string, work: string): void {
  const upload = join(work, "notarize.zip");
  run("ditto", ["-c", "-k", "--sequesterRsrc", "--keepParent", app, upload]);
  const result = JSON.parse(
    execFileSync(
      "xcrun",
      [
        "notarytool",
        "submit",
        upload,
        "--apple-id",
        env("APPLE_ID"),
        "--password",
        env("APPLE_APP_SPECIFIC_PASSWORD"),
        "--team-id",
        env("APPLE_TEAM_ID"),
        "--wait",
        "--timeout",
        "30m",
        "--output-format",
        "json",
      ],
      { encoding: "utf8" },
    ),
  );
  if (result.status !== "Accepted")
    throw new Error(
      `Notarization ${result.id}: ${result.status}. Run xcrun notarytool log ${result.id} for details.`,
    );
  run("xcrun", ["stapler", "staple", app]);
  run("xcrun", ["stapler", "validate", app]);
}

const given = secrets.filter((name) => process.env[name]);
if (given.length && given.length !== secrets.length)
  throw new Error(
    `Developer ID signing needs all of ${secrets.join(", ")}; only ${given.join(", ")} are set.`,
  );
const out = join(releaseDir, `mac-${arch}`);
const app = join(out, bundle);
await rm(out, { recursive: true, force: true });
const work = await mkdtemp(join(tmpdir(), "nv-mac-"));
let identity: Identity = { name: "-" };
try {
  const macOS = join(app, "Contents/MacOS");
  const resources = join(app, "Contents/Resources");
  const core = join(resources, coreLayout.packaged);
  await mkdir(macOS, { recursive: true });
  await mkdir(join(work, "core-stage"));
  await stageCore(join(work, "core-stage"), core, "darwin", arch);
  await copyFile(join(binaries, product.executable), join(macOS, product.name));
  await copyFile(join(binaries, product.helper), join(macOS, product.helper));
  await copyFile(join(repo, "build/icon.icns"), join(resources, "icon.icns"));
  await stageClient(join(resources, "client"));
  await stageLicences(join(resources, "licenses"), [build]);
  await writeFile(join(app, "Contents/Info.plist"), appPlist);
  await stageRelaunch(app);
  await writeFile(join(app, "Contents/PkgInfo"), "APPL????");
  for (const file of [
    join(macOS, product.name),
    join(macOS, product.helper),
    join(core, coreLayout.binary),
  ])
    await chmod(file, 0o755);
  verifyMacBundle(app, arch, false);

  if (given.length) identity = await developerId(work);
  // Inside out: the core and the helper, then the bundle, which seals them.
  sign(join(core, coreLayout.binary), identity);
  // Signing rewrote the core; the app verifies it against its manifest at runtime.
  const manifest = readCoreManifest(core);
  manifest.binarySha256 = sha256(await readFile(join(core, coreLayout.binary)));
  writeCoreManifest(core, manifest);
  sign(join(macOS, product.helper), identity);
  sign(join(app, squirrel.shipIt), identity);
  sign(join(app, squirrel.framework, "Versions/A"), identity);
  // An ad-hoc signature's own requirement is its hash, which no later build meets.
  sign(
    app,
    identity,
    identity.name === "-"
      ? [`-r=designated => identifier "${product.id}"`]
      : [],
  );
  verifyMacBundle(app, arch, true);
  verifyMacSignature(app);
  if (given.length) notarize(app, work);

  // Squirrel.Mac unpacks the zip with ditto and expects the bundle at its root.
  const zip = desktopArtifact(version, "mac", arch, "zip");
  const dmg = desktopArtifact(version, "mac", arch, "dmg");
  await rm(join(releaseDir, zip), { force: true });
  run("ditto", [
    "-c",
    "-k",
    "--sequesterRsrc",
    "--keepParent",
    app,
    join(releaseDir, zip),
  ]);
  // The DMG is not signed: Gatekeeper checks the app inside it (docs/release-signing.md).
  const volume = join(work, "dmg");
  await mkdir(volume);
  run("ditto", [app, join(volume, bundle)]);
  await symlink("/Applications", join(volume, "Applications"));
  run("hdiutil", [
    "create",
    "-volname",
    product.name,
    "-srcfolder",
    volume,
    "-fs",
    "HFS+",
    "-format",
    "UDZO",
    "-ov",
    join(releaseDir, dmg),
  ]);

  const feed = await writeFeed(releaseDir, "mac", [zip, dmg]);
  verifyArtifact(join(releaseDir, zip));
  for (const name of [zip, dmg]) await report(join(releaseDir, name));
  console.log(`Wrote ${feed.slice(repo.length + 1)}.`);
} finally {
  if (identity.keychain)
    run("security", ["delete-keychain", identity.keychain]);
  await rm(work, { recursive: true, force: true });
}
