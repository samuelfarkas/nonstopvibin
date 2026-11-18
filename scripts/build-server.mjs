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
import pkg from "../package.json" with { type: "json" };
import {
  checkCoreDirectory,
  coreFiles,
  coreLayout,
  coreVersion,
} from "../src/server/core-binary.ts";
import { checkoutLayout, serverArchive } from "../src/server/host-defaults.ts";
import {
  architecture,
  privateFilePattern,
  renderInstaller,
  serverArchiveName,
  serverArchitectures,
} from "./distribution.ts";

const repo = resolve(import.meta.dirname, "..");
assert.equal(
  execFileSync("bun", ["--version"], { encoding: "utf8" }).trim(),
  pkg.packageManager.replace(/^bun@/, ""),
  "Build server archives with the pinned Bun runtime (licenses/Bun.txt)",
);
const arches = process.argv.slice(2).length
  ? process.argv.slice(2)
  : serverArchitectures;
for (const arch of arches)
  if (!serverArchitectures.includes(arch))
    throw new Error(`Unsupported server architecture: ${arch}`);
for (const file of [
  join(checkoutLayout.client, "index.html"),
  "dist/licenses/THIRD-PARTY.txt",
])
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
// ELF64 little-endian with the architecture's e_machine.
async function assertElf(path, arch) {
  const header = (await readFile(path)).subarray(0, 20);
  assert.equal(header.subarray(0, 4).toString("latin1"), "\x7fELF", path);
  assert.equal(header[4], 2, `${path} must be 64-bit`);
  assert.equal(
    header.readUInt16LE(18),
    architecture(arch).elf,
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
const licenses = [
  ["LICENSE", "LICENSE"],
  ["licenses/NOTICE.md", "NOTICE.md"],
  ["licenses/Bun.txt", "Bun.txt"],
  ["dist/licenses/THIRD-PARTY.txt", "THIRD-PARTY.txt"],
];
const bsdtar = execFileSync("tar", ["--version"], {
  encoding: "utf8",
}).includes("bsdtar");

await mkdir(join(repo, "release"), { recursive: true });
for (const arch of arches) {
  const name = serverArchiveName(pkg.version, arch);
  const temporary = await mkdtemp(join(tmpdir(), "nv-server-"));
  try {
    const out = join(temporary, name);
    const coreStage = join(temporary, "core-stage");
    const core = join(out, serverArchive.core);
    await mkdir(core, { recursive: true });
    await mkdir(join(out, serverArchive.licenses));
    await mkdir(coreStage);
    await stageCore(coreStage, arch);
    for (const file of coreFiles)
      await copyFile(
        join(coreStage, coreLayout.checkout, file),
        join(core, file),
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
        join(out, serverArchive.executable),
      ],
      { cwd: repo, stdio: "inherit" },
    );
    await cp(
      join(repo, checkoutLayout.client),
      join(out, serverArchive.client),
      { recursive: true },
    );
    for (const [from, to] of licenses)
      await copyFile(join(repo, from), join(out, serverArchive.licenses, to));
    await copyFile(
      join(coreStage, "licenses/CLIProxyAPI.txt"),
      join(out, serverArchive.licenses, "CLIProxyAPI.txt"),
    );
    await writeFile(
      join(out, serverArchive.installer),
      renderInstaller(
        await readFile(join(repo, "scripts/server-install.sh"), "utf8"),
        pkg.version,
        arch,
      ),
    );
    await writeFile(
      join(out, serverArchive.readme),
      `# NonstopVibin ${pkg.version} headless server (linux-${arch})

Run on glibc-based Linux (x64 or arm64; not Alpine/musl) as a non-root user.
For trusted co-located agents, use their user. For a protected upstream consumed
through a separate private inference gate, use a dedicated protected service
user with no coding agents; never share its credentials/helpers with guests.
A systemd user session is optional:

    ./install.sh            # ~/.local, plus a systemd user service
    ./install.sh --no-service  # foreground / existing supervisor
    ./install.sh --help     # options

Then run \`nonstopvibin url\` for the session link and SSH tunnel command.
Full guide: ${pkg.homepage}/blob/main/docs/server.md

\`${serverArchive.core}/${coreLayout.manifest}\` records the bundled CLIProxyAPI ${coreVersion} checksums.
They are integrity evidence, not independent proof of the publisher.
Licenses are in \`licenses/\`.
`,
    );
    const binary = join(core, coreLayout.binary);
    for (const path of [
      join(out, serverArchive.executable),
      binary,
      join(out, serverArchive.installer),
    ])
      await chmod(path, 0o755);

    await assertElf(join(out, serverArchive.executable), arch);
    await assertElf(binary, arch);
    checkCoreDirectory(core, "linux", arch);
    const entries = await files(out);
    const client = `${serverArchive.client}/`;
    const fixed = entries.filter((entry) => !entry.startsWith(client));
    // Every archive entry is staged; the directories hold exactly these files.
    const contents = {
      [serverArchive.core]: coreFiles,
      [serverArchive.licenses]: [
        ...licenses.map(([, to]) => to),
        "CLIProxyAPI.txt",
      ],
    };
    assert.deepEqual(
      fixed.sort(),
      Object.values(serverArchive)
        .filter((entry) => entry !== serverArchive.client)
        .flatMap((entry) =>
          contents[entry]
            ? [
                `${entry}/`,
                ...contents[entry].map((file) => `${entry}/${file}`),
              ]
            : [entry],
        )
        .sort(),
      "Unexpected server archive contents",
    );
    assert.ok(entries.includes(`${client}index.html`), "Missing client");
    for (const entry of entries)
      assert.doesNotMatch(
        entry,
        privateFilePattern,
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
