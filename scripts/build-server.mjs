// Builds release/nonstopvibin-server-<version>-linux-<arch>.tar.gz.
// Usage: bun scripts/build-server.mjs [x64|arm64 ...] (default: both).
// Run `bun run build` first for dist/client and dist/licenses.
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  chmod,
  copyFile,
  cp,
  mkdir,
  mkdtemp,
  readFile,
  readdir,
  rm,
  stat,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, relative, resolve } from "node:path";
import release from "./core-release.json" with { type: "json" };
import pkg from "../package.json" with { type: "json" };
import verifyPackage from "./verify-package.cjs";

const repo = resolve(import.meta.dirname, "..");
assert.equal(
  execFileSync("bun", ["--version"], { encoding: "utf8" }).trim(),
  pkg.packageManager.replace(/^bun@/, ""),
  "Build server archives with the pinned Bun runtime (licenses/Bun.txt)",
);
const arches = process.argv.slice(2).length
  ? process.argv.slice(2)
  : ["x64", "arm64"];
for (const arch of arches)
  if (!["x64", "arm64"].includes(arch))
    throw new Error(`Unsupported server architecture: ${arch}`);
for (const file of ["dist/client/index.html", "dist/licenses/THIRD-PARTY.txt"])
  await stat(join(repo, file)).catch(() => {
    throw new Error(`Missing ${file}; run bun run build first.`);
  });

const sha256 = async (path) =>
  createHash("sha256")
    .update(await readFile(path))
    .digest("hex");
async function files(root, folder = root) {
  const entries = await readdir(folder, { withFileTypes: true });
  const nested = await Promise.all(
    entries.map(async (entry) => {
      const path = join(folder, entry.name);
      return entry.isDirectory()
        ? [`${relative(root, path)}/`, ...(await files(root, path))]
        : [relative(root, path)];
    }),
  );
  return nested.flat();
}
// ELF64 little-endian, e_machine: x86-64 = 62, AArch64 = 183.
async function assertElf(path, arch) {
  const header = (await readFile(path)).subarray(0, 20);
  assert.equal(header.subarray(0, 4).toString("latin1"), "\x7fELF", path);
  assert.equal(header[4], 2, `${path} must be 64-bit`);
  assert.equal(
    header.readUInt16LE(18),
    arch === "x64" ? 62 : 183,
    `${path} is not a linux-${arch} executable`,
  );
}
// install-core.mjs writes .vendor/core and licenses/ relative to its cwd, so a
// private cwd keeps the developer's own core untouched.
function stageCore(directory, arch) {
  execFileSync("bun", [join(repo, "scripts/install-core.mjs")], {
    cwd: directory,
    env: { ...process.env, CORE_PLATFORM: "linux", CORE_ARCH: arch },
    stdio: "inherit",
  });
}
const bsdtar = execFileSync("tar", ["--version"], {
  encoding: "utf8",
}).includes("bsdtar");

await mkdir(join(repo, "release"), { recursive: true });
for (const arch of arches) {
  const name = `nonstopvibin-server-${pkg.version}-linux-${arch}`;
  const temporary = await mkdtemp(join(tmpdir(), "nv-server-"));
  try {
    const out = join(temporary, name);
    const coreStage = join(temporary, "core-stage");
    await mkdir(join(out, "core"), { recursive: true });
    await mkdir(join(out, "licenses"));
    await mkdir(coreStage);
    await stageCore(coreStage, arch);
    for (const file of ["cli-proxy-api", "manifest.json"])
      await copyFile(
        join(coreStage, ".vendor/core", file),
        join(out, "core", file),
      );
    execFileSync(
      "bun",
      [
        "build",
        "src/server/cli.ts",
        "--compile",
        `--target=bun-linux-${arch}`,
        "--minify",
        "--no-compile-autoload-dotenv",
        "--no-compile-autoload-bunfig",
        "--define",
        "NONSTOPVIBIN_PACKAGED=true",
        "--outfile",
        join(out, "nonstopvibin"),
      ],
      { cwd: repo, stdio: "inherit" },
    );
    await cp(join(repo, "dist/client"), join(out, "client"), {
      recursive: true,
    });
    for (const [from, to] of [
      ["LICENSE", "LICENSE"],
      ["licenses/NOTICE.md", "NOTICE.md"],
      ["licenses/Bun.txt", "Bun.txt"],
      ["dist/licenses/THIRD-PARTY.txt", "THIRD-PARTY.txt"],
    ])
      await copyFile(join(repo, from), join(out, "licenses", to));
    await copyFile(
      join(coreStage, "licenses/CLIProxyAPI.txt"),
      join(out, "licenses/CLIProxyAPI.txt"),
    );
    await writeFile(
      join(out, "install.sh"),
      (await readFile(join(repo, "scripts/server-install.sh"), "utf8"))
        .replace("@VERSION@", pkg.version)
        .replace("@ARCH@", arch),
    );
    await writeFile(
      join(out, "README.md"),
      `# NonstopVibin ${pkg.version} headless server (linux-${arch})

Install as the non-root user that runs your coding agents (Debian 12+):

    ./install.sh            # ~/.local, plus a systemd user service
    ./install.sh --help     # options

Then run \`nonstopvibin url\` for the session link and SSH tunnel command.
Full guide: ${pkg.homepage}/blob/main/docs/server.md

\`core/manifest.json\` records the bundled CLIProxyAPI ${release.version} checksums.
They are integrity evidence, not independent proof of the publisher.
Licenses are in \`licenses/\`.
`,
    );
    for (const path of ["nonstopvibin", "core/cli-proxy-api", "install.sh"])
      await chmod(join(out, path), 0o755);

    // Verify the staged tree before archiving it.
    await assertElf(join(out, "nonstopvibin"), arch);
    await assertElf(join(out, "core/cli-proxy-api"), arch);
    const target = `linux_${arch}`;
    const manifest = JSON.parse(
      await readFile(join(out, "core/manifest.json"), "utf8"),
    );
    assert.equal(manifest.version, release.version, "Core version pin");
    assert.equal(manifest.platform, "linux", "Core platform");
    assert.equal(manifest.arch, arch, "Core architecture");
    assert.equal(
      manifest.sha256,
      release.checksums[target],
      "Core archive pin",
    );
    assert.equal(manifest.binarySha256, release.binaries[target], "Core pin");
    assert.equal(
      await sha256(join(out, "core/cli-proxy-api")),
      release.binaries[target],
      "Core binary differs from the reviewed pin",
    );
    const entries = await files(out);
    const fixed = entries.filter((entry) => !entry.startsWith("client/"));
    assert.deepEqual(
      fixed.sort(),
      [
        "README.md",
        "core/",
        "core/cli-proxy-api",
        "core/manifest.json",
        "install.sh",
        "licenses/",
        "licenses/Bun.txt",
        "licenses/CLIProxyAPI.txt",
        "licenses/LICENSE",
        "licenses/NOTICE.md",
        "licenses/THIRD-PARTY.txt",
        "nonstopvibin",
      ],
      "Unexpected server archive contents",
    );
    assert.ok(entries.includes("client/index.html"), "Missing client");
    for (const entry of entries)
      assert.doesNotMatch(
        entry,
        verifyPackage.privateFilePattern,
        `Private or development file in server archive: ${entry}`,
      );

    const archive = join(repo, "release", `${name}.tar.gz`);
    await rm(archive, { force: true });
    execFileSync(
      "tar",
      [
        "-czf",
        archive,
        ...(bsdtar
          ? ["--uid", "0", "--gid", "0", "--no-xattrs", "--no-mac-metadata"]
          : ["--owner=0", "--group=0", "--numeric-owner"]),
        "-C",
        temporary,
        name,
      ],
      { env: { ...process.env, COPYFILE_DISABLE: "1" } },
    );
    const size = (await stat(archive)).size;
    console.log(
      `${relative(repo, archive)}  ${(size / 1048576).toFixed(1)} MiB  sha256 ${await sha256(archive)}`,
    );
  } finally {
    await rm(temporary, { recursive: true, force: true });
  }
}
