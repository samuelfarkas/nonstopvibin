// Checks a Rust desktop package or server archive before it ships: the exact file layout, the
// pinned core, each executable's platform and architecture, the static Linux daemon, macOS
// executables that link only system libraries, the licences, and no private files.
// The packaging scripts call these checks; scripts/verify-rust-package.ts runs them on built
// artifacts.
import assert from "node:assert/strict";
import { execFileSync, spawnSync } from "node:child_process";
import {
  existsSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  statSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { basename, join, relative, resolve } from "node:path";
import pkg from "../package.json" with { type: "json" };
import {
  architecture,
  privateFilePattern,
  product,
  rustLicenses,
  serverArchive,
} from "./distribution.ts";
import { coreFiles, coreLayout, coreTool } from "./package-common.ts";

const repo = resolve(import.meta.dirname, "..");

/** Every path under `root`, directories with a trailing `/`; symlinks are not followed. */
export function tree(root: string, folder = root): string[] {
  return readdirSync(folder, { withFileTypes: true }).flatMap((entry) => {
    const path = join(folder, entry.name);
    const name = relative(root, path);
    return entry.isDirectory() ? [`${name}/`, ...tree(root, path)] : [name];
  });
}

/** `files` plus each of their parent directories, sorted like a checked tree. */
function withDirectories(files: readonly string[]): string[] {
  const all = new Set(files);
  for (const file of files) {
    const parts = file.replace(/\/$/, "").split("/");
    for (let i = 1; i < parts.length; i++)
      all.add(`${parts.slice(0, i).join("/")}/`);
  }
  return [...all].sort();
}

function assertTree(root: string, expected: readonly string[], what: string) {
  const entries = tree(root).sort();
  assert.deepEqual(entries, withDirectories(expected), `Unexpected ${what}`);
  for (const entry of entries)
    assert.doesNotMatch(
      entry,
      privateFilePattern,
      `Private or development file in ${what}: ${entry}`,
    );
}

const PT_INTERP = 3;
/** An ELF64 little-endian file's machine and whether it asks for a dynamic loader. */
export function elf(path: string) {
  const bytes = readFileSync(path);
  assert.equal(
    bytes.subarray(0, 4).toString("latin1"),
    "\x7fELF",
    `${path} is not an ELF file`,
  );
  assert.equal(bytes[4], 2, `${path} must be 64-bit`);
  assert.equal(bytes[5], 1, `${path} must be little-endian`);
  const table = Number(bytes.readBigUInt64LE(32));
  const size = bytes.readUInt16LE(54);
  let dynamic = false;
  for (let i = 0; i < bytes.readUInt16LE(56); i++) {
    const offset = table + i * size;
    assert.ok(offset + 4 <= bytes.length, `${path}: invalid program header`);
    if (bytes.readUInt32LE(offset) === PT_INTERP) dynamic = true;
  }
  return { machine: bytes.readUInt16LE(18), dynamic };
}

const LC_BUILD_VERSION = 0x32;
const PLATFORM_MACOS = 1;
// LC_LOAD_DYLIB, LC_LOAD_WEAK_DYLIB, LC_REEXPORT_DYLIB, LC_LOAD_UPWARD_DYLIB.
const dylibCommands = new Set([0xc, 0x80000018, 0x8000001f, 0x80000023]);
/** A thin 64-bit Mach-O file's CPU type, LC_BUILD_VERSION and linked libraries. */
interface MachO {
  cputype: number;
  platform?: number;
  minos?: number;
  dylibs: string[];
}
export function machO(path: string): MachO {
  const bytes = readFileSync(path);
  assert.equal(
    bytes.readUInt32LE(0),
    0xfeedfacf,
    `${path} must be a 64-bit Mach-O file`,
  );
  const result: MachO = {
    cputype: bytes.readUInt32LE(4),
    dylibs: [],
  };
  for (let offset = 32, i = 0; i < bytes.readUInt32LE(16); i++) {
    assert.ok(offset + 8 <= bytes.length, `${path}: invalid load command`);
    const command = bytes.readUInt32LE(offset);
    const size = bytes.readUInt32LE(offset + 4);
    assert.ok(
      size >= 8 && offset + size <= bytes.length,
      `${path}: invalid load command size`,
    );
    if (command === LC_BUILD_VERSION) {
      assert.ok(size >= 24, `${path}: invalid build version`);
      result.platform = bytes.readUInt32LE(offset + 8);
      result.minos = bytes.readUInt32LE(offset + 12);
    }
    if (dylibCommands.has(command)) {
      const start = offset + bytes.readUInt32LE(offset + 8);
      const end = bytes.indexOf(0, start);
      result.dylibs.push(
        bytes.toString("utf8", start, end < 0 ? offset + size : end),
      );
    }
    offset += size;
  }
  return result;
}

/**
 * A macOS executable for `arch` and the minimum OS, linking only Apple's libraries. Library
 * validation then needs no entitlement: the bundle signs with build/entitlements.mac.minimal.plist
 * (empty). A third-party dylib fails here, which is when to revisit that decision.
 */
export function checkMacExecutable(path: string, arch: string): void {
  const binary = machO(path);
  assert.equal(
    binary.cputype,
    architecture(arch).machO,
    `${path} is not a darwin-${arch} executable`,
  );
  assert.equal(binary.platform, PLATFORM_MACOS, `${path} must target macOS`);
  const [major, minor] = product.minimumMacOS.split(".").map(Number);
  assert.equal(
    binary.minos,
    (major << 16) | (minor << 8),
    `${path} must target macOS ${product.minimumMacOS} (MACOSX_DEPLOYMENT_TARGET)`,
  );
  for (const dylib of binary.dylibs)
    assert.match(
      dylib,
      /^\/(?:System\/Library|usr\/lib)\//,
      `${path} links ${dylib}, which is not an Apple system library`,
    );
}

/** The Linux daemon: static (no loader), so no glibc floor of its own. */
function checkLinuxDaemon(path: string, arch: string): void {
  const binary = elf(path);
  assert.equal(
    binary.machine,
    architecture(arch).elf,
    `${path} is not a linux-${arch} executable`,
  );
  assert.ok(!binary.dynamic, `${path} must be statically linked (musl)`);
}

function checkLinuxExecutable(path: string, arch: string): void {
  assert.equal(
    elf(path).machine,
    architecture(arch).elf,
    `${path} is not a linux-${arch} executable`,
  );
}

function checkMode(path: string): void {
  assert.equal(statSync(path).mode & 0o777, 0o755, `${path} must be 0755`);
}

/**
 * Throws unless the core in `directory` is the reviewed pin for `platform`/`arch`. Signing
 * rewrites the core, so a `signed` one's binary is held to its manifest instead of the pin.
 */
function checkCoreDirectory(
  directory: string,
  platform: string,
  arch: string,
  signed = false,
): void {
  coreTool(
    "check",
    "--target",
    `${platform}_${arch}`,
    ...(signed ? ["--signed"] : []),
    directory,
  );
}

function checkLicences(directory: string): void {
  for (const [name, source] of Object.entries(rustLicenses)) {
    const bytes = readFileSync(join(directory, name));
    if (source)
      assert.deepEqual(
        bytes,
        readFileSync(join(repo, source)),
        `Licence differs from the reviewed input: ${name}`,
      );
    else assert.ok(bytes.length > 0, `Empty licence file: ${name}`);
  }
}

/** The built interface in `folder` under `root`: present, with its index.html. */
function clientEntries(root: string, folder: string): string[] {
  assert.ok(
    existsSync(join(root, folder, "index.html")),
    `Missing ${folder}/index.html: the package has no interface.`,
  );
  return tree(join(root, folder)).map((entry) => `${folder}/${entry}`);
}

const coreEntries = (folder: string) =>
  coreFiles.map((file) => `${folder}/${file}`);
const licenceEntries = (folder: string) =>
  Object.keys(rustLicenses).map((file) => `${folder}/${file}`);

/** The server archive's top folder. */
export function verifyServerTree(root: string, arch: string): void {
  const entries = serverArchive;
  assertTree(
    root,
    [
      entries.executable,
      entries.installer,
      entries.readme,
      ...coreEntries(entries.core),
      ...clientEntries(root, entries.client),
      ...licenceEntries(entries.licenses),
    ],
    "server archive contents",
  );
  const core = join(root, entries.core);
  checkLinuxDaemon(join(root, entries.executable), arch);
  checkLinuxExecutable(join(core, coreLayout.binary), arch);
  checkCoreDirectory(core, "linux", arch);
  for (const file of [
    entries.executable,
    entries.installer,
    join(entries.core, coreLayout.binary),
  ])
    checkMode(join(root, file));
  checkLicences(join(root, entries.licenses));
}

/** The Linux app folder (the AppImage root, /opt/NonstopVibin in the deb): what it must hold. */
const linuxApp = [
  product.executable,
  product.helper,
  ...coreEntries(coreLayout.packaged),
  ...licenceEntries("licenses"),
];
function checkLinuxApp(app: string, arch: string): void {
  checkLinuxDaemon(join(app, product.executable), arch);
  checkLinuxExecutable(join(app, product.helper), arch);
  const core = join(app, coreLayout.packaged);
  checkLinuxExecutable(join(core, coreLayout.binary), arch);
  checkCoreDirectory(core, "linux", arch);
  for (const file of [
    product.executable,
    product.helper,
    join(coreLayout.packaged, coreLayout.binary),
  ])
    checkMode(join(app, file));
  checkLicences(join(app, "licenses"));
}

export const desktopFile = `${product.executable}.desktop`;
export const iconFile = `${product.executable}.png`;
const appImageExtras = ["AppRun", ".DirIcon", desktopFile, iconFile];
export function verifyAppDir(root: string, arch: string): void {
  assertTree(
    root,
    [...linuxApp, ...clientEntries(root, "client"), ...appImageExtras],
    "AppImage contents",
  );
  checkLinuxApp(root, arch);
}

/** Where the Electron deb installed the app; existing autostart entries run this path. */
export const debApp = `opt/${product.name}`;
const debExtras = [
  `usr/share/applications/${desktopFile}`,
  `usr/share/icons/hicolor/512x512/apps/${iconFile}`,
];
/** The deb's files as `dpkg-deb -x` extracts them (without DEBIAN/). */
export function verifyDebTree(root: string, arch: string): void {
  assertTree(
    root,
    [
      ...linuxApp.map((file) => `${debApp}/${file}`),
      ...clientEntries(root, `${debApp}/client`),
      ...debExtras,
    ],
    "deb contents",
  );
  checkLinuxApp(join(root, debApp), arch);
}

export const bundle = `${product.name}.app`;
/** The relaunch shim installed Electron builds run after updating to this app (package-mac.ts). */
export const squirrel = {
  framework: "Contents/Frameworks/Squirrel.framework",
  shipIt: "Contents/Frameworks/Squirrel.framework/Versions/A/Resources/ShipIt",
} as const;
/** Where stapler puts the notarization ticket. */
const ticket = "Contents/CodeResources";
/** The .app bundle; `signed` after codesign, which adds the seal and rewrites the core. */
export function verifyMacBundle(
  app: string,
  arch: string,
  signed: boolean,
): void {
  const resources = "Contents/Resources";
  const stapled = signed && existsSync(join(app, ticket));
  assertTree(
    app,
    [
      "Contents/Info.plist",
      "Contents/PkgInfo",
      `Contents/MacOS/${product.name}`,
      `Contents/MacOS/${product.helper}`,
      `${resources}/icon.icns`,
      `${squirrel.framework}/Squirrel`,
      `${squirrel.framework}/Resources`,
      `${squirrel.framework}/Versions/Current`,
      `${squirrel.framework}/Versions/A/Squirrel`,
      `${squirrel.framework}/Versions/A/Resources/Info.plist`,
      squirrel.shipIt,
      ...(signed
        ? [`${squirrel.framework}/Versions/A/_CodeSignature/CodeResources`]
        : []),
      ...coreEntries(`${resources}/${coreLayout.packaged}`),
      ...clientEntries(app, `${resources}/client`),
      ...licenceEntries(`${resources}/licenses`),
      ...(signed ? ["Contents/_CodeSignature/CodeResources"] : []),
      ...(stapled ? [ticket] : []),
    ],
    "app bundle contents",
  );
  for (const executable of [
    `Contents/MacOS/${product.name}`,
    `Contents/MacOS/${product.helper}`,
    squirrel.shipIt,
    `${squirrel.framework}/Versions/A/Squirrel`,
  ])
    checkMacExecutable(join(app, executable), arch);
  const core = join(app, resources, coreLayout.packaged);
  assert.equal(
    machO(join(core, coreLayout.binary)).cputype,
    architecture(arch).machO,
    "The core is not a darwin executable for the target",
  );
  checkCoreDirectory(core, "darwin", arch, signed);
  const plist = readFileSync(join(app, "Contents/Info.plist"), "utf8");
  for (const [key, value] of [
    ["CFBundleIdentifier", product.id],
    ["CFBundleExecutable", product.name],
    ["CFBundleShortVersionString", pkg.version],
    ["LSMinimumSystemVersion", product.minimumMacOS],
  ])
    assert.ok(
      plist.includes(`<key>${key}</key>\n\t<string>${value}</string>`),
      `Info.plist ${key} must be ${value}`,
    );
  checkLicences(join(app, resources, "licenses"));
}

function codesign(...args: string[]): string {
  const result = spawnSync("codesign", args, { encoding: "utf8" });
  assert.equal(
    result.status,
    0,
    `codesign ${args.join(" ")}: ${result.stderr}`,
  );
  return `${result.stdout}${result.stderr}`;
}

/**
 * The signed bundle: valid, hardened, no entitlements, and the requirement installed builds
 * check updates against. Ad-hoc builds carry exactly `identifier "<bundle id>"`.
 */
export function verifyMacSignature(app: string): void {
  codesign("--verify", "--deep", "--strict", "--all-architectures", app);
  const adhoc = codesign("-dv", app).includes("Signature=adhoc");
  const requirement = codesign("-d", "-r-", app);
  const identifier = `identifier "${product.id}"`;
  if (adhoc)
    assert.match(
      requirement,
      new RegExp(`^designated => ${identifier}$`, "m"),
      "Ad-hoc builds must carry the bundle-identifier requirement",
    );
  else assert.ok(requirement.includes(identifier), requirement);
  if (existsSync(join(app, ticket))) {
    assert.ok(!adhoc, "Only a Developer ID build can be notarized");
    execFileSync("xcrun", ["stapler", "validate", app], { stdio: "inherit" });
  }
  for (const file of [
    `Contents/MacOS/${product.name}`,
    `Contents/MacOS/${product.helper}`,
    squirrel.shipIt,
    `Contents/Resources/${coreLayout.packaged}/${coreLayout.binary}`,
  ]) {
    const path = join(app, file);
    assert.match(
      codesign("-dv", "--verbose=2", path),
      /flags=0x[0-9a-f]+\([^)]*runtime[^)]*\)/,
      `${file} must use the hardened runtime`,
    );
    assert.doesNotMatch(
      codesign("-d", "--entitlements", "-", "--xml", path),
      /com\.apple\./,
      `${file} must have no entitlements`,
    );
  }
}

/** Unpacks a built artifact into a temporary folder and runs its checks. */
export function verifyArtifact(file: string): void {
  const name = basename(file);
  const work = mkdtempSync(join(tmpdir(), "nv-verify-"));
  const arch = (machine: string) =>
    ({ x64: "x64", x86_64: "x64", amd64: "x64", arm64: "arm64" })[machine] ??
    assert.fail(`No architecture in ${name}`);
  try {
    let match: RegExpMatchArray | null;
    if (
      (match = name.match(/^nonstopvibin-server-(.+)-linux-(\w+)\.tar\.gz$/))
    ) {
      assert.equal(match[1], pkg.version, `${name}: version`);
      execFileSync("tar", ["-xzf", resolve(file), "-C", work]);
      verifyServerTree(
        join(work, name.slice(0, -".tar.gz".length)),
        arch(match[2]),
      );
    } else if (
      (match = name.match(/^NonstopVibin-(.+)-linux-(\w+)\.AppImage$/))
    ) {
      assert.equal(match[1], pkg.version, `${name}: version`);
      execFileSync(resolve(file), ["--appimage-extract"], {
        cwd: work,
        stdio: "ignore",
      });
      verifyAppDir(join(work, "squashfs-root"), arch(match[2]));
    } else if ((match = name.match(/^NonstopVibin-(.+)-linux-(\w+)\.deb$/))) {
      const fields = execFileSync(
        "dpkg-deb",
        ["-f", file, "Package", "Version", "Architecture"],
        { encoding: "utf8" },
      );
      assert.equal(
        fields,
        `Package: ${product.executable}\nVersion: ${pkg.version}\nArchitecture: ${match[2]}\n`,
        `${name}: control fields`,
      );
      execFileSync("dpkg-deb", ["-x", file, work]);
      verifyDebTree(work, arch(match[2]));
    } else if ((match = name.match(/^NonstopVibin-(.+)-mac-(\w+)\.zip$/))) {
      assert.equal(match[1], pkg.version, `${name}: version`);
      // How Squirrel.Mac unpacks the update: the bundle must sit at the archive root.
      execFileSync("ditto", ["-x", "-k", file, work]);
      assert.deepEqual(readdirSync(work), [bundle], `${name}: archive root`);
      verifyMacBundle(join(work, bundle), arch(match[2]), true);
      verifyMacSignature(join(work, bundle));
    } else assert.fail(`Not a Rust release artifact: ${name}`);
    console.log(`Verified ${name}.`);
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
}
