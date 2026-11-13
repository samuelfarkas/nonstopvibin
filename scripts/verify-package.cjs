const assert = require("node:assert/strict");
const { readFileSync, readdirSync } = require("node:fs");
const { join, relative } = require("node:path");
const { createHash } = require("node:crypto");
const { listPackage, extractFile } = require("@electron/asar");
const { Arch } = require("electron-builder");

const privateFilePattern =
  /(?:^|\/)(?:\.env(?:\.[^/]*)?|\.runtime|\.test-runtime|\.deepsec|\.agents|\.git|\.codex|\.claude|\.playwright-cli|id_rsa|id_ed25519|auth\.json|credentials\.json|tokens\.json)(?:\/|$)|\.(?:pem|key|p12|pfx|sqlite3?(?:-(?:wal|shm))?|db(?:-(?:wal|shm))?|map)$/;

function resourcePaths(root, folder = root) {
  return readdirSync(folder, { withFileTypes: true }).flatMap((entry) => {
    const path = join(folder, entry.name);
    const name = relative(root, path);
    return entry.isDirectory() ? [name, ...resourcePaths(root, path)] : [name];
  });
}

module.exports = async function verifyPackage(context) {
  const resources =
    context.electronPlatformName === "darwin"
      ? join(
          context.appOutDir,
          `${context.packager.appInfo.productFilename}.app`,
          "Contents/Resources",
        )
      : join(context.appOutDir, "resources");
  const archive = join(resources, "app.asar");
  const entries = listPackage(archive);
  for (const entry of entries) {
    assert.match(
      entry,
      /^\/(dist(?:\/|$)|package\.json$)/,
      `Unexpected package entry: ${entry}`,
    );
    assert.doesNotMatch(
      entry,
      privateFilePattern,
      `Private or development file in package: ${entry}`,
    );
  }
  for (const entry of resourcePaths(resources))
    assert.doesNotMatch(
      entry,
      privateFilePattern,
      `Private or development file in package resources: ${entry}`,
    );
  for (const file of [
    "dist/desktop/main.cjs",
    "dist/desktop/preload.cjs",
    "dist/client/index.html",
    "dist/licenses/THIRD-PARTY.txt",
  ])
    assert.ok(
      extractFile(archive, file).length,
      `Missing built entry: ${file}`,
    );
  assert.deepEqual(
    entries.filter((entry) => entry.startsWith("/dist/desktop/")),
    [
      "/dist/desktop/main.cjs",
      "/dist/desktop/preload.cjs",
      ...(context.electronPlatformName === "darwin"
        ? ["/dist/desktop/tray-click-monitor"]
        : []),
    ],
    "Stale desktop build output",
  );
  const project = context.packager.projectDir;
  if (context.electronPlatformName === "darwin") {
    const monitor = readFileSync(
      join(project, "dist/desktop/tray-click-monitor"),
    );
    assert.equal(
      monitor.readUInt32LE(0),
      0xfeedfacf,
      "Click monitor must be a Mach-O executable",
    );
    assert.equal(
      monitor.readUInt32LE(4),
      context.arch === Arch.arm64 ? 0x0100000c : 0x01000007,
      "Click monitor architecture differs from the package target",
    );
    // LC_BUILD_VERSION stores the deployment target as major.minor.patch bytes.
    let minimum;
    for (let offset = 32, i = 0; i < monitor.readUInt32LE(16); i++) {
      assert.ok(
        offset + 8 <= monitor.length,
        "Invalid click monitor load command",
      );
      const size = monitor.readUInt32LE(offset + 4);
      assert.ok(
        size >= 8 && offset + size <= monitor.length,
        "Invalid click monitor command size",
      );
      if (monitor.readUInt32LE(offset) === 0x32) {
        assert.ok(size >= 24, "Invalid click monitor build version");
        assert.equal(
          monitor.readUInt32LE(offset + 8),
          1,
          "Click monitor must target macOS",
        );
        minimum = monitor.readUInt32LE(offset + 12);
      }
      offset += size;
    }
    assert.equal(minimum, 13 << 16, "Click monitor must target macOS 13.0");
    assert.deepEqual(
      readFileSync(
        join(resources, "app.asar.unpacked/dist/desktop/tray-click-monitor"),
      ),
      monitor,
      "Packaged click monitor differs from the built input",
    );
  }
  for (const file of ["cli-proxy-api", "manifest.json"])
    assert.deepEqual(
      readFileSync(join(resources, "core", file)),
      readFileSync(join(project, ".vendor/core", file)),
      "Packaged core differs from the verified input",
    );
  assert.deepEqual(readdirSync(join(resources, "core")).sort(), [
    "cli-proxy-api",
    "manifest.json",
  ]);
  const licenses = ["CLIProxyAPI.txt", "NOTICE.md"];
  assert.deepEqual(readdirSync(join(resources, "licenses")).sort(), licenses);
  for (const file of licenses)
    assert.deepEqual(
      readFileSync(join(resources, "licenses", file)),
      readFileSync(join(project, "licenses", file)),
      `Missing license: ${file}`,
    );
  assert.deepEqual(
    readFileSync(join(resources, "LICENSE")),
    readFileSync(join(project, "LICENSE")),
  );
  console.log(
    `Verified package contents (${entries.length} entries, ASAR SHA-256 ${createHash("sha256").update(readFileSync(archive)).digest("hex")}).`,
  );
};
module.exports.privateFilePattern = privateFilePattern;
