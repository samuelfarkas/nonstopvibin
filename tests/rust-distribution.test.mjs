import test from "node:test";
import assert from "node:assert/strict";
import {
  chmod,
  cp,
  mkdir,
  mkdtemp,
  readFile,
  rm,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { load } from "js-yaml";
import pkg from "../package.json" with { type: "json" };
import {
  desktopArtifact,
  desktopTargets,
  product,
  rustLicenses,
} from "../scripts/distribution.ts";
import { writeFeed } from "../scripts/package-common.ts";
import {
  checkMacExecutable,
  verifyServerTree,
} from "../scripts/verify-rust-package.ts";

const project = resolve(import.meta.dirname, "..");

test("Rust packages keep the identity and file names installed Electron builds update from", () => {
  const { build } = pkg;
  assert.equal(product.id, build.appId);
  assert.equal(product.name, build.productName);
  // electron-builder's default Linux executable: the package name, lowercased.
  assert.equal(product.executable, pkg.name);
  assert.deepEqual(product.feed, {
    owner: build.publish[0].owner,
    repo: build.publish[0].repo,
  });
  assert.equal(product.macCategory, build.mac.category);
  assert.equal(product.linuxCategory, build.linux.category);
  assert.equal(product.maintainer, build.linux.maintainer);
  assert.deepEqual(desktopTargets, {
    mac: build.mac.target,
    linux: build.linux.target,
  });
  // The names electron-builder gave the 0.2.0 installers.
  assert.equal(
    desktopArtifact("0.2.0", "mac", "arm64", "zip"),
    "NonstopVibin-0.2.0-mac-arm64.zip",
  );
  assert.equal(
    desktopArtifact("0.2.0", "mac", "arm64", "dmg"),
    "NonstopVibin-0.2.0-mac-arm64.dmg",
  );
  assert.equal(
    desktopArtifact("0.2.0", "linux", "x64", "AppImage"),
    "NonstopVibin-0.2.0-linux-x86_64.AppImage",
  );
  assert.equal(
    desktopArtifact("0.2.0", "linux", "x64", "deb"),
    "NonstopVibin-0.2.0-linux-amd64.deb",
  );
});

test("update feeds have electron-builder's fields and electron-updater's YAML parser reads them", async () => {
  const directory = await mkdtemp(join(tmpdir(), "nv-feed-"));
  try {
    await writeFile(join(directory, "app.zip"), "zip");
    await writeFile(join(directory, "app.dmg"), "dmg!");
    const feed = load(
      await readFile(
        await writeFeed(
          directory,
          "mac",
          ["app.zip", "app.dmg"],
          "2026-11-23T10:00:00.000Z",
        ),
        "utf8",
      ),
    );
    const zip =
      "A5JY2soDJJhmgGhmPv6GEdDq+HBY/XClPIYl3ihyBDSzsovWx0Zygh1Ia3qqCnnyhsAwSsPI8qr1BFKbpXR0tA==";
    assert.deepEqual(feed, {
      version: pkg.version,
      files: [
        { url: "app.zip", sha512: zip, size: 3 },
        {
          url: "app.dmg",
          sha512:
            "/o7tYN2h9j86w7puod3JkGx2dgIxdoGWZ0v9keavtY4Wt57PqE9v9qBEuML5JSrxVBSj91APmauLMFQssTmHZA==",
          size: 4,
        },
      ],
      path: "app.zip",
      sha512: zip,
      // A string, not the Date js-yaml makes of an unquoted timestamp.
      releaseDate: "2026-11-23T10:00:00.000Z",
    });
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});

/** A minimal ELF64 header with one program header: PT_LOAD, or PT_INTERP when dynamic. */
function elf(machine, dynamic = false) {
  const bytes = Buffer.alloc(64 + 56);
  bytes.write("\x7fELF", 0, "latin1");
  bytes[4] = 2;
  bytes[5] = 1;
  bytes.writeUInt16LE(machine, 18);
  bytes.writeBigUInt64LE(64n, 32);
  bytes.writeUInt16LE(56, 54);
  bytes.writeUInt16LE(1, 56);
  bytes.writeUInt32LE(dynamic ? 3 : 1, 64);
  return bytes;
}

const installed = JSON.parse(
  await readFile(join(project, ".vendor/core/manifest.json"), "utf8"),
);
test(
  "the server archive check rejects a missing interface, a dynamic daemon, another architecture, an unreviewed core, changed licences and wrong modes",
  {
    skip:
      installed.platform !== "linux" && "needs a linux core from core:install",
  },
  async () => {
    const root = await mkdtemp(join(tmpdir(), "nv-rust-server-"));
    const { arch } = installed;
    const machine = { x64: 62, arm64: 183 }[arch];
    try {
      await cp(join(project, ".vendor/core"), join(root, "core"), {
        recursive: true,
        filter: (path) => !path.endsWith("LICENSE"),
      });
      await mkdir(join(root, "licenses"));
      const sources = {
        LICENSE: "LICENSE",
        "NOTICE.md": "licenses/NOTICE.md",
        "CLIProxyAPI.txt": "licenses/CLIProxyAPI.txt",
        "THIRD-PARTY.txt": "dist/licenses/THIRD-PARTY.txt",
      };
      for (const name of rustLicenses)
        if (sources[name])
          await cp(join(project, sources[name]), join(root, "licenses", name));
        else await writeFile(join(root, "licenses", name), "notices");
      await writeFile(join(root, "nonstopvibin"), elf(machine));
      await writeFile(join(root, "install.sh"), "#!/bin/sh\n");
      await writeFile(join(root, "README.md"), "readme");
      for (const file of ["nonstopvibin", "install.sh", "core/cli-proxy-api"])
        await chmod(join(root, file), 0o755);
      assert.throws(
        () => verifyServerTree(root, arch),
        /Missing client\/index.html/,
      );
      await mkdir(join(root, "client/assets"), { recursive: true });
      await writeFile(join(root, "client/assets/index-abc123.js"), "ui");
      assert.throws(
        () => verifyServerTree(root, arch),
        /Missing client\/index.html/,
      );
      await writeFile(join(root, "client/index.html"), "<!doctype html>");
      verifyServerTree(root, arch);

      await writeFile(join(root, "nonstopvibin"), elf(machine, true));
      assert.throws(() => verifyServerTree(root, arch), /statically linked/);
      await writeFile(join(root, "nonstopvibin"), elf(machine));
      assert.throws(
        () => verifyServerTree(root, arch === "x64" ? "arm64" : "x64"),
        /is not a linux-/,
      );

      await writeFile(join(root, "licenses/.env"), "TOKEN=1");
      assert.throws(
        () => verifyServerTree(root, arch),
        /Unexpected server archive contents/,
      );
      await rm(join(root, "licenses/.env"));

      const core = join(root, "core/cli-proxy-api");
      const bytes = await readFile(core);
      await writeFile(core, Buffer.concat([bytes, Buffer.from("x")]));
      assert.throws(() => verifyServerTree(root, arch), /reviewed release pin/);
      await writeFile(core, bytes);

      await writeFile(join(root, "licenses/NOTICE.md"), "changed");
      assert.throws(() => verifyServerTree(root, arch), /Licence differs/);
      await cp(
        join(project, "licenses/NOTICE.md"),
        join(root, "licenses/NOTICE.md"),
      );

      await chmod(join(root, "install.sh"), 0o644);
      assert.throws(() => verifyServerTree(root, arch), /must be 0755/);
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  },
);

/** A thin arm64 Mach-O executable with LC_BUILD_VERSION and one LC_LOAD_DYLIB per library. */
function machO({ cputype = 0x0100000c, minos = 13 << 16, dylibs }) {
  const build = Buffer.alloc(24);
  build.writeUInt32LE(0x32, 0);
  build.writeUInt32LE(24, 4);
  build.writeUInt32LE(1, 8);
  build.writeUInt32LE(minos, 12);
  const commands = dylibs.map((name) => {
    const size = Math.ceil((24 + name.length + 1) / 8) * 8;
    const command = Buffer.alloc(size);
    command.writeUInt32LE(0xc, 0);
    command.writeUInt32LE(size, 4);
    command.writeUInt32LE(24, 8);
    command.write(name, 24);
    return command;
  });
  const header = Buffer.alloc(32);
  header.writeUInt32LE(0xfeedfacf, 0);
  header.writeUInt32LE(cputype, 4);
  header.writeUInt32LE(2, 12);
  header.writeUInt32LE(1 + commands.length, 16);
  return Buffer.concat([header, build, ...commands]);
}

test("macOS executables target macOS 13 on the package's architecture and link only Apple's libraries", async () => {
  const root = await mkdtemp(join(tmpdir(), "nv-macho-"));
  const file = join(root, "NonstopVibin");
  const system = [
    "/usr/lib/libSystem.B.dylib",
    "/System/Library/Frameworks/WebKit.framework/Versions/A/WebKit",
  ];
  try {
    await writeFile(file, machO({ dylibs: system }));
    checkMacExecutable(file, "arm64");
    assert.throws(() => checkMacExecutable(file, "x64"), /darwin-x64/);
    await writeFile(
      file,
      machO({ dylibs: [...system, "@rpath/libwebkit.dylib"] }),
    );
    assert.throws(
      () => checkMacExecutable(file, "arm64"),
      /@rpath\/libwebkit\.dylib, which is not an Apple system library/,
    );
    await writeFile(file, machO({ minos: 11 << 16, dylibs: system }));
    assert.throws(() => checkMacExecutable(file, "arm64"), /macOS 13\.0/);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});
